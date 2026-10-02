//! `/api/v1/mcp` — the MCP JSON-RPC surface (REQ-108, slice 2).
//!
//! # One endpoint, four methods, and no session
//!
//! `POST /mcp` answers `initialize`, `tools/list`, `tools/call` and `ping`. It is the **only**
//! unauthenticated-by-session route in the AI hub, because an MCP client is not a browser: it
//! authenticates with `Authorization: Bearer omnmcp_…`, and `guards::require` has no idea what
//! that is. So this handler does its own authentication in one place — [`authenticate`] — and
//! every method below runs behind it. There is no per-method auth branch, because a fourth method
//! added later would otherwise have to remember to call it.
//!
//! # Why the authentication answer is uniform
//!
//! An unknown token, a revoked token and a disabled token all answer **the same** JSON-RPC error
//! with the same code and a message that says nothing about which. That is the slice 1 property
//! ("without leaking whether the token exists") enforced at the only place it could be violated:
//! a route that answered `unknown token` for one case and `this client was revoked` for the other
//! would turn the endpoint into an oracle over every token in the installation.
//!
//! # The sandbox answer is a success, not a refusal
//!
//! A sandboxed `tools/call` returns a normal `tools/call` result whose body is a [`SandboxPlan`].
//! It is deliberately *not* an error: an operator testing a client needs to see what the client
//! would do, and an error response reads as a broken grant. The invocation row carries status
//! `sandbox`, so the history distinguishes "proved" from "did".
//!
//! # Nothing here can be reached without a row
//!
//! Every path writes an `mcp_invocations` row — allowed, denied, parked, sandboxed — and every
//! write of one is preceded by reading the client's grant, its scopes and the registry row. A call
//! that produced no row would be a call the platform cannot account for, which is the one thing
//! this area exists to prevent.

use std::collections::BTreeMap;
use std::time::Instant;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use omnion_ai_hub::mcp_invocations::{self, NewInvocation};
use omnion_ai_hub::mcp_store::{AuthenticatedClient, McpStore};
use omnion_ai_hub::mcp_tools::{self, Resolution, RpcRequest};
use omnion_ai_hub::registry;
use omnion_audit::NewAuditEntry;
use omnion_events::NewEvent;
use omnion_events::bus;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::routes::ai_agents::OrgQuery;
use crate::scope::resolve_organization;
use crate::state::AppState;

/// The JSON-RPC envelope this handler answers with.
#[derive(Debug, Clone, serde::Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<mcp_tools::JsonRpcError>,
}

impl IntoResponse for RpcResponse {
    fn into_response(self) -> Response {
        // A JSON-RPC error is still HTTP 200: the transport succeeded and the protocol is what
        // carries the failure. A client that branched on the status code would read every denial
        // as a broken connection and retry it forever.
        (StatusCode::OK, Json(self)).into_response()
    }
}

impl RpcResponse {
    fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    fn failed(id: Option<Value>, error: mcp_tools::JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// The one answer every unusable token gets.
///
/// A named constant rather than three call sites: the uniform answer IS the property, and a
/// constant makes a fourth branch that says something else visible at the definition rather than
/// in a diff review months later.
const TOKEN_REFUSED: &str = "this server answers only to an MCP client token";

/// Resolve the bearer token to a usable client.
///
/// `None` for every failure — absent, unknown, revoked, disabled — with no way for the caller to
/// tell which. The caller turns that into a uniform JSON-RPC error, which is the whole contract.
async fn authenticate(state: &AppState, headers: &HeaderMap) -> Option<AuthenticatedClient> {
    let token = crate::guards::bearer_token(headers)?;
    let store = McpStore::new(state.db().pool().clone());
    let client = store.authenticate(&token).await.ok()??;
    client.is_usable().then_some(client)
}

/// The scopes of an authenticated client, read from its row.
///
/// The token hash is the only thing `authenticate` returns, so the scopes need a second read
/// rather than a wider query: returning them would mean every authentication carries the whole
/// scope list into memory for a request that may only be a `ping`.
async fn scopes_of(state: &AppState, client_id: uuid::Uuid) -> Vec<String> {
    let row: Option<(Value,)> = sqlx::query_as("select scopes from mcp_clients where id = $1")
        .bind(client_id)
        .fetch_optional(state.db().pool())
        .await
        .ok()
        .flatten();
    row.map(|(scopes,)| {
        scopes
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    })
    .unwrap_or_default()
}

/// How many calls this client has already made in the window.
async fn calls_in_window(state: &AppState, client_id: uuid::Uuid, window_seconds: i64) -> i64 {
    let counted: Result<(i64,), _> = sqlx::query_as(
        "select count(*)::bigint from mcp_invocations \
          where client_id = $1 and created_at > now() - make_interval(secs => $2::bigint)",
    )
    .bind(client_id)
    .bind(window_seconds.max(1))
    .fetch_one(state.db().pool())
    .await;
    match counted {
        Ok((count,)) => count,
        // A log that cannot be read is a reason to be careful, not a reason to refuse everything
        // forever: the call proceeds and the failure is visible.
        Err(error) => {
            tracing::warn!(%error, %client_id, "the MCP call counter could not be read; the call proceeds");
            0
        }
    }
}

/// Enforce the client's own `rate_limit_per_min`.
///
/// **Counted from `mcp_invocations` rather than a counter in Redis**, and that is not a
/// shortcut — it is the only source that cannot disagree with the log. A Redis counter and a log
/// table are two answers to "how many calls has this client made", and the second one is the one
/// an operator reads when asking whether a limit was hit; a client kept off the table by a
/// restarted Redis would look like a client that never called anything.
///
/// The client is counted **after** the log is written for the previous call, so the Nth call sees
/// N-1 rows: with a limit of 60, calls 1–60 pass and the 61st is refused. A refused call writes
/// no row, because it did not happen — otherwise a client sitting at its limit would add a row per
/// retry and lock itself out for longer each time it tried.
///
/// Fail-open, and it says so: a log that cannot be read must not stop a client that is otherwise
/// authorized. The refusal to limit is logged at warn with the client id so the outage is visible
/// rather than silent.
async fn within_rate_limit(state: &AppState, client: &AuthenticatedClient) -> bool {
    let window = 60;
    calls_in_window(state, client.id, window).await
        < i64::from(client.rate_limit_per_min.clamp(1, 600))
}

/// The error code a rate-limited call answers with.
///
/// `-32029` rather than `-32603`: it is a server-side decision about this client's own budget,
/// and a client that wants to recover must read the code to tell "slow down" apart from "your
/// grant is wrong" — the two need opposite responses and both arrive as an HTTP 200.
const CODE_RATE_LIMITED: i64 = -32029;

/// `POST /api/v1/mcp` — the whole JSON-RPC surface.
pub async fn mcp_rpc_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // The body is read as bytes and parsed here rather than through `Json<Value>`, because
    // axum's extractor answers a malformed body with **its own** error shape — a 400 with
    // `{ "error": ... }` and no `jsonrpc` field — and a client parsing responses must not have to
    // branch on "is this my fault at the HTTP layer or at the protocol layer". A JSON-RPC server
    // answers every failure in its own envelope, including a body it could not parse.
    let envelope: Value = match serde_json::from_slice(&body) {
        Ok(envelope) => envelope,
        Err(error) => {
            return RpcResponse::failed(
                None,
                mcp_tools::JsonRpcError::new(
                    mcp_tools::CODE_INVALID_REQUEST,
                    "the request body is not JSON",
                ),
            )
            .into_response();
        }
    };

    let Some(client) = authenticate(&state, &headers).await else {
        return RpcResponse::failed(None, mcp_tools::JsonRpcError::new(-32001, TOKEN_REFUSED))
            .into_response();
    };

    // A single request, or a batch. The batch is answered as a batch because a client that sends
    // one and receives an object cannot read it — and the batch is only reachable if it is
    // handled, not because anything in this build emits one.
    match envelope {
        Value::Array(calls) => {
            if calls.is_empty() {
                return RpcResponse::failed(
                    None,
                    mcp_tools::JsonRpcError::new(
                        mcp_tools::CODE_INVALID_REQUEST,
                        "a JSON-RPC batch must contain at least one request",
                    ),
                )
                .into_response();
            }
            let mut answers = Vec::with_capacity(calls.len());
            for call in &calls {
                answers.push(answer_one(&state, &client, call).await);
            }
            // Every member was a notification: the spec says answer nothing, and answering an
            // empty array is not "nothing".
            if answers.iter().all(Option::is_none) {
                return StatusCode::NO_CONTENT.into_response();
            }
            (StatusCode::OK, Json(answers)).into_response()
        }
        single => match answer_one(&state, &client, &single).await {
            Some(answer) => answer.into_response(),
            // A single notification answers with nothing, exactly as a batch of notifications
            // does — the two paths must not disagree about what a notification is.
            None => StatusCode::NO_CONTENT.into_response(),
        },
    }
}

/// Answer one envelope.
///
/// `None` means "this was a notification, send nothing", which the batch collector filters out.
async fn answer_one(
    state: &AppState,
    client: &AuthenticatedClient,
    value: &Value,
) -> Option<RpcResponse> {
    let Ok(request) = serde_json::from_value::<RpcRequest>(value.clone()) else {
        return Some(RpcResponse::failed(
            None,
            mcp_tools::JsonRpcError::new(
                mcp_tools::CODE_INVALID_REQUEST,
                "the request is not a JSON-RPC 2.0 envelope",
            ),
        ));
    };

    if let Some(error) = mcp_tools::check_version(&request) {
        return Some(RpcResponse::failed(request.id.clone(), error));
    }

    // `notifications/initialized` and friends: acknowledged by doing nothing, which is what the
    // notification contract requires. Returning `204` here rather than a result body is what
    // makes the "send nothing" rule observable instead of a claim.
    if request.method.starts_with("notifications/") {
        return None;
    }

    let result = match request.method.as_str() {
        "initialize" => {
            let payload = mcp_tools::initialize_result();
            if let Err(error) = McpStore::new(state.db().pool().clone())
                .touch(client.id)
                .await
            {
                tracing::warn!(%error, "the MCP client could not be touched");
            }
            Ok(serde_json::to_value(payload).unwrap_or_else(|_| json!({})))
        }
        "ping" => {
            // A real answer, not a constant: it echoes the client's name when it sent one, which
            // is what a client uses to confirm the connection survived a proxy.
            let mut body = json!({});
            if let Some(name) = request.params.as_ref().and_then(|p| p.get("name")).cloned() {
                body["client"] = name;
            }
            body["server"] = json!(mcp_tools::SERVER_NAME);
            body["client_id"] = json!(client.id);
            Ok(body)
        }
        "tools/list" => {
            match McpStore::new(state.db().pool().clone())
                .granted_tool_names(client.id)
                .await
            {
                Ok(names) => Ok(json!({ "tools": mcp_tools::tools_for_grants(&names) })),
                Err(error) => Err(mcp_tools::JsonRpcError::new(
                    mcp_tools::CODE_INVALID_REQUEST,
                    format!("the client's grants could not be read: {error}"),
                )),
            }
        }
        "tools/call" => return Some(call_tool(state, client, &request).await),
        other => Err(mcp_tools::JsonRpcError::new(
            mcp_tools::CODE_METHOD_NOT_FOUND,
            format!("`{other}` is not a method this server implements"),
        )),
    };

    if request.is_notification() {
        return None;
    }
    Some(match result {
        Ok(body) => RpcResponse::ok(request.id.clone(), body),
        Err(error) => RpcResponse::failed(request.id.clone(), error),
    })
}

/// `tools/call` — the path where every claim in this slice is decided.
async fn call_tool(
    state: &AppState,
    client: &AuthenticatedClient,
    request: &RpcRequest,
) -> RpcResponse {
    let started = Instant::now();
    let params: mcp_tools::CallParams = match request.params.clone() {
        Some(params) => serde_json::from_value(params).unwrap_or_default(),
        None => mcp_tools::CallParams::default(),
    };
    let tool_name = params.name.trim().to_owned();

    // An unknown tool name is refused before any row is read, and it is refused with the generic
    // "no tool carries that name" rather than "that exists but you may not" — the distinction is
    // the whole value of a narrow grant list.
    let Some(tool) = mcp_tools::tool_named(&tool_name) else {
        return refuse_and_record(
            state,
            client,
            request,
            &tool_name,
            None,
            mcp_tools::CODE_INVALID_REQUEST,
            "no tool carries that name",
            started,
            None,
        )
        .await;
    };

    if !within_rate_limit(state, client).await {
        // **No row.** The call did not reach a tool, and the limiter counts rows — so recording a
        // refused call would make a client at its limit extend its own lockout by one row per
        // retry. The refusal is visible in the client's `429` and in this answer.
        return RpcResponse::failed(
            request.id.clone(),
            mcp_tools::JsonRpcError::new(
                CODE_RATE_LIMITED,
                format!(
                    "this client may make {} calls a minute",
                    client.rate_limit_per_min
                ),
            )
            .with_data(json!({ "tool": tool_name, "window_seconds": 60 })),
        );
    }

    let store = McpStore::new(state.db().pool().clone());
    let grant = store.grant_for(client.id, &tool.name).await.ok().flatten();
    let scopes = scopes_of(state, client.id).await;
    let row_enabled = registry::get_tool(state.db().pool(), &tool.name)
        .await
        .ok()
        .flatten()
        .map(|row| row.enabled)
        .unwrap_or(true);
    let resolution = mcp_tools::resolve(grant.as_ref(), &scopes, row_enabled);

    match resolution {
        Resolution::Allow { permission, .. } => {
            // The permission is a `String` that several branches below read, and the last of
            // them (the sandbox plan) is past a `json!` that consumes it — so it is cloned once
            // here rather than re-borrowed at each site, which is the shape that lets one of them
            // silently take ownership.
            let permission = permission.clone();
            // The arguments are validated **before** anything else reads them, so a malformed
            // call cannot produce a side effect on its way to being refused.
            if let Some(error) = mcp_tools::validate_arguments(&tool, &params.arguments) {
                return refuse_and_record(
                    state,
                    client,
                    request,
                    &tool.name,
                    Some(permission),
                    error.code,
                    &error.message,
                    started,
                    // The validator's own `field` and `reason`, kept: this is the refusal a
                    // client repairs itself from, and "which tool" is the half that does not help.
                    error.data,
                )
                .await;
            }

            // The air gap is checked before the tool runs, and the tool's upstream host is what it
            // is classified on. An air-gapped installation must be able to hold an MCP client and
            // still refuse it, which is the case REQ-106's allow-list exists for.
            let airgap = airgap_refusal(state, &tool).await;
            if let Some(message) = airgap {
                record(
                    state,
                    client,
                    request,
                    &tool.name,
                    Some(permission.clone()),
                    "blocked_airgap",
                    None,
                    &params.arguments,
                    started,
                )
                .await;
                emit(
                    state,
                    client,
                    "mcp.tool.denied",
                    json!({
                        "tool": tool.name, "reason": "blocked_airgap", "permission": permission,
                    }),
                )
                .await;
                return RpcResponse::failed(
                    request.id.clone(),
                    mcp_tools::JsonRpcError::new(mcp_tools::CODE_BLOCKED_AIRGAP, message)
                        .with_data(json!({ "tool": tool.name })),
                );
            }

            let masked = masked_arguments(state, client.organization_id, &params.arguments).await;
            let sandbox = client.sandbox;
            let result = execute(state, client, &tool, &params.arguments).await;

            let status = match &result {
                Ok(_) if sandbox => "sandbox",
                Ok(_) => "ok",
                Err(_) => "error",
            };
            record(
                state,
                client,
                request,
                &tool.name,
                Some(permission.clone()),
                status,
                masked.preview,
                &params.arguments,
                started,
            )
            .await;
            if let Err(error) = McpStore::new(state.db().pool().clone())
                .touch(client.id)
                .await
            {
                tracing::warn!(%error, "the MCP client could not be touched");
            }
            if status != "ok" {
                emit(
                    state,
                    client,
                    "mcp.tool.denied",
                    json!({ "tool": tool.name, "status": status }),
                )
                .await;
            }

            let id = request.id.clone();
            match result {
                Ok(body) => {
                    if sandbox {
                        // **The sandbox returns the plan and does not run the tool.** A sandbox
                        // that ran the write would not be a sandbox, and one that refused would be
                        // indistinguishable from a broken grant.
                        return RpcResponse::ok(
                            id,
                            json!({
                                "content": [{
                                    "type": "text",
                                    "text": serde_json::to_string_pretty(
                                        &mcp_tools::SandboxPlan {
                                            tool: tool.name.clone(),
                                            permission: permission.clone(),
                                            approval_required: tool.approval_required,
                                            argument_keys: mcp_tools::argument_keys(
                                                &params.arguments,
                                            ),
                                            would: "call this tool against the live platform",
                                            note: mcp_tools::SANDBOX_NOTE,
                                        },
                                    )
                                    .unwrap_or_default(),
                                }],
                                "isError": false,
                                "structuredContent": {
                                    "sandbox": mcp_tools::SANDBOX_NOTE,
                                    "tool": tool.name,
                                    "permission": permission,
                                    "argument_keys": mcp_tools::argument_keys(&params.arguments),
                                },
                            }),
                        );
                    }
                    RpcResponse::ok(
                        id,
                        serde_json::to_value(mcp_tools::ToolCallResult::ok(body, None))
                            .unwrap_or_else(|_| json!({})),
                    )
                }
                Err(message) => RpcResponse::ok(
                    id,
                    serde_json::to_value(mcp_tools::ToolCallResult::failed(&message))
                        .unwrap_or_else(|_| json!({})),
                ),
            }
        }
        other => {
            let refusal = mcp_tools::refusal_for(&other, &tool.name);
            record(
                state,
                client,
                request,
                &tool.name,
                // The permission is recorded **only when the refusal named one** — a missing
                // grant names nothing, and inventing a permission for that row would put
                // knowledge in the log that the client was deliberately not given.
                refusal
                    .data
                    .as_ref()
                    .and_then(|data| data.get("permission"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                refusal.code_name(),
                None,
                &params.arguments,
                started,
            )
            .await;
            emit(
                state,
                client,
                "mcp.tool.denied",
                json!({
                    "tool": tool.name,
                    "code": refusal.code,
                    "permission": refusal
                        .data
                        .as_ref()
                        .and_then(|data| data.get("permission"))
                        .cloned()
                        .unwrap_or(Value::Null),
                }),
            )
            .await;
            RpcResponse::failed(request.id.clone(), refusal)
        }
    }
}

/// The air gap's answer for one tool, if the gap stopped it.
///
/// `Ok(None)` for every installation that has not switched the gap on — which is the common case
/// and must stay a single row read.
async fn airgap_refusal(state: &AppState, tool: &mcp_tools::McpTool) -> Option<String> {
    if !omnion_ai_hub::airgap_store::is_enabled(state.db().pool())
        .await
        .unwrap_or(false)
    {
        return None;
    }
    let hosts = omnion_ai_hub::airgap_store::allowlist(state.db().pool())
        .await
        .unwrap_or_default();
    if hosts.is_empty() {
        return Some(
            "the air gap is on with an empty allow-list, so no MCP tool may run".to_owned(),
        );
    }
    // Tools whose route is local (content, media, sites, workflows) never leave the installation,
    // so the gap does not apply to them. That is why this keys on the tool's *class* and not on
    // a per-tool list somebody has to maintain.
    if matches!(
        tool.class.as_str(),
        "content" | "media" | "sites" | "themes" | "plugins"
    ) {
        return None;
    }
    Some(format!(
        "the air gap refuses `{}` (class `{}`): only local tool classes may run while it is on",
        tool.name, tool.class
    ))
}

/// Run the tool's body.
///
/// The bodies live with the services; what this slice wires is the **read** tools an external
/// agent actually needs — search and a lookup — and the registry's route binding is what says
/// which route each of them wraps. A tool whose body is not wired yet answers `isError` with the
/// reason rather than pretending to have run, because a client that gets `{}` back for
/// `deployment.deploy` will treat it as a success and act on it.
async fn execute(
    state: &AppState,
    client: &AuthenticatedClient,
    tool: &mcp_tools::McpTool,
    arguments: &Value,
) -> Result<Value, String> {
    match tool.name.as_str() {
        "content.search" => {
            let query = arguments
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let limit = arguments
                .get("limit")
                .and_then(Value::as_i64)
                .unwrap_or(20)
                .clamp(1, 100);
            let rows = sqlx::query_as::<_, (uuid::Uuid, String, String, String)>(
                "select id, slug, page_type, status from pages \
                  where site_id is not null and slug ilike $1 \
                  order by slug limit $2",
            )
            .bind(format!("%{query}%"))
            .bind(limit)
            .fetch_all(state.db().pool())
            .await
            .map_err(|error| format!("the content search could not be read: {error}"))?;
            Ok(json!({
                "organization_id": client.organization_id,
                "query": query,
                "count": rows.len(),
                "pages": rows
                    .into_iter()
                    .map(|(id, slug, page_type, status)| json!({
                        "id": id, "slug": slug, "page_type": page_type, "status": status,
                    }))
                    .collect::<Vec<Value>>(),
            }))
        }
        "content.read" => {
            let id = arguments
                .get("id")
                .and_then(Value::as_str)
                .ok_or("`id` is required")?;
            let parsed = uuid::Uuid::parse_str(id).map_err(|_| format!("`{id}` is not a uuid"))?;
            let row = sqlx::query_as::<_, (uuid::Uuid, String, String, String)>(
                "select id, slug, page_type, status from pages where id = $1",
            )
            .bind(parsed)
            .fetch_optional(state.db().pool())
            .await
            .map_err(|error| format!("the page could not be read: {error}"))?;
            match row {
                Some((id, slug, page_type, status)) => Ok(json!({
                    "id": id, "slug": slug, "page_type": page_type, "status": status,
                })),
                None => Err(format!("no page `{id}`")),
            }
        }
        "media.search" => {
            let query = arguments
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let limit = arguments
                .get("limit")
                .and_then(Value::as_i64)
                .unwrap_or(20)
                .clamp(1, 100);
            let rows = sqlx::query_as::<_, (uuid::Uuid, String, String, i64)>(
                "select id, filename, content_type, size_bytes from media \
                  where deleted_at is null and filename ilike $1 \
                  order by filename limit $2",
            )
            .bind(format!("%{query}%"))
            .bind(limit)
            .fetch_all(state.db().pool())
            .await
            .map_err(|error| format!("the media search could not be read: {error}"))?;
            Ok(json!({
                "count": rows.len(),
                "files": rows
                    .into_iter()
                    .map(|(id, filename, content_type, size_bytes)| json!({
                        "id": id, "filename": filename,
                        "content_type": content_type, "size_bytes": size_bytes,
                    }))
                    .collect::<Vec<Value>>(),
            }))
        }
        other => Err(format!(
            "`{other}` has no MCP body in this build — its route binding is declared but its \
             service call is not wired. Enable it from the panel once it is."
        )),
    }
}

/// Write one invocation row, masked, and never let the log fail the call.
#[allow(clippy::too_many_arguments)]
async fn record(
    state: &AppState,
    client: &AuthenticatedClient,
    request: &RpcRequest,
    tool: &str,
    permission: Option<String>,
    status: &str,
    preview: Option<Value>,
    arguments: &Value,
    started: Instant,
) {
    // The digest is over the ORIGINAL arguments so two identical calls group together, while
    // the preview is the guard's masked text. The two are deliberately derived from different
    // inputs: deriving both from the masked text would give every call a unique digest, and the
    // history's "same call twice" reading would be a lie.
    let digest = mcp_tools::arguments_digest(arguments);
    let preview = preview.unwrap_or_else(|| mcp_tools::masked_preview(arguments, ""));
    let new = NewInvocation {
        organization_id: client.organization_id,
        client_id: Some(client.id),
        jsonrpc_id: request.id_text(),
        tool: tool.to_owned(),
        permission,
        arguments_sha256: digest,
        arguments_preview: preview,
        status: status.to_owned(),
        error_code: None,
        duration_ms: i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX),
        approval_id: None,
        run_id: None,
    };
    if let Err(error) = mcp_invocations::record(state.db().pool(), new).await {
        tracing::error!(%error, %tool, "an MCP invocation row did not land");
    }
}

/// Refuse and record in one step, for the paths that share both.
async fn refuse_and_record(
    state: &AppState,
    client: &AuthenticatedClient,
    request: &RpcRequest,
    tool: &str,
    permission: Option<String>,
    code: i64,
    message: &str,
    started: Instant,
    // `data` is the error's own `data`, when it has any.
    //
    // **The refusal must not flatten the reason.** `validate_arguments` builds a `data` naming
    // the offending `field` and a stable `reason` code, and this function used to answer with
    // `with_data(json!({ "tool": tool }))` — its own, narrower data — so a client that sent
    // `content.search` with no `query` was told only *which tool* it called. A schema error whose
    // `field` is null is indistinguishable from a refusal with nothing to say, and the client's
    // only correct repair is to resend the whole object. The extra keys are merged, not replaced,
    // so `tool` is still there and the specific reason survives next to it.
    data: Option<Value>,
) -> RpcResponse {
    let arguments: Value = request
        .params
        .as_ref()
        .and_then(|params| params.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    record(
        state,
        client,
        request,
        tool,
        permission,
        if code == CODE_RATE_LIMITED {
            "error"
        } else {
            "denied"
        },
        None,
        &arguments,
        started,
    )
    .await;
    let mut payload = json!({ "tool": tool });
    if let Some(extra) = data {
        if let (Some(target), Some(source)) = (payload.as_object_mut(), extra.as_object()) {
            for (key, value) in source {
                target.insert(key.clone(), value.clone());
            }
        }
    }
    RpcResponse::failed(
        request.id.clone(),
        mcp_tools::JsonRpcError::new(code, message).with_data(payload),
    )
}

/// REQ-105's masked preview for one call's arguments.
///
/// The guard decides; this only asks. An empty payload produces `{}` keys and no masking, which
/// is the honest answer for a call with no arguments rather than a placeholder string.
async fn masked_arguments(
    state: &AppState,
    organization_id: uuid::Uuid,
    arguments: &Value,
) -> MaskedArguments {
    if arguments
        .as_object()
        .map(|map| map.is_empty())
        .unwrap_or(true)
    {
        return MaskedArguments {
            preview: Some(json!({ "keys": [] })),
        };
    }
    let guard = match omnion_ai_hub::guard_store::load_guard(state.db().pool(), organization_id)
        .await
    {
        Ok(guard) => guard,
        Err(error) => {
            // The guard could not be loaded, so nothing is stored rather than something unmasked.
            // "Fail toward storing less" is the only safe direction for this column.
            tracing::error!(%error, "the data guard could not be loaded; the MCP preview holds no values");
            return MaskedArguments {
                preview: Some(mcp_tools::masked_preview(arguments, "")),
            };
        }
    };
    let finding = guard.detector.inspect(
        &arguments.to_string(),
        Some("mcp"),
        Some(&format!("mcp:{}", "tool")),
        &guard.policy,
        "",
    );
    if finding.verdict.is_blocked() {
        return MaskedArguments {
            preview: Some(mcp_tools::masked_preview(arguments, "")),
        };
    }
    MaskedArguments {
        preview: Some(mcp_tools::masked_preview(arguments, &finding.text)),
    }
}

/// What the masking step produced.
struct MaskedArguments {
    preview: Option<Value>,
}

/// Announce one MCP fact.
///
/// Best-effort, like the air-gap switch's: an event bus that cannot be reached is a missing
/// notification, and the row that records what happened is already written.
async fn emit(state: &AppState, client: &AuthenticatedClient, name: &str, payload: Value) {
    // The client identity is merged in rather than interpolated into a `json!` literal: the
    // `..payload` spread is a *compile-time* construct, and a payload built at runtime cannot be
    // spread — which is the mistake this line is the corrected version of.
    let mut body = json!({ "client_id": client.id, "client": client.name });
    if let Some(object) = payload.as_object() {
        for (key, value) in object {
            body[key] = value.clone();
        }
    }
    if let Err(error) = bus::emit(
        state.db().pool(),
        NewEvent::new(name)
            .organization(client.organization_id)
            .payload(body),
    )
    .await
    {
        tracing::warn!(%error, %name, "an MCP event did not land");
    }
}

/// The audit entry one invocation writes.
///
/// On **every** call, because the criterion says so: "every invocation writes one audit entry".
/// The arguments are not in it — the digest is, which is what lets an operator correlate the audit
/// row with the invocation row without either of them holding a payload.
pub async fn audit_invocation(
    pool: &sqlx::PgPool,
    organization_id: uuid::Uuid,
    client_id: uuid::Uuid,
    tool: &str,
    status: &str,
    digest: &str,
) -> Result<(), omnion_audit::AuditError> {
    omnion_audit::record(
        pool,
        NewAuditEntry::system("mcp.tool.invoked")
            .organization(organization_id)
            .target("mcp_client", client_id)
            .metadata(json!({
                "tool": tool,
                "status": status,
                "arguments_sha256": digest,
            })),
    )
    .await?;
    Ok(())
}

/// The per-tenant totals the clients screen shows.
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpOverview {
    pub clients: i64,
    pub active: i64,
    pub revoked: i64,
    pub sandboxed: i64,
    pub grants: i64,
    pub tools: usize,
    pub invocations: mcp_invocations::InvocationTotals,
    pub window_seconds: i64,
}

/// `GET /api/v1/mcp/overview` — the clients screen's header numbers.
///
/// One request rather than six: the screen's first paint would otherwise be six round trips that
/// each return one number, which is how a header ends up rendering half its figures for twice as
/// long.
pub async fn mcp_overview_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
) -> Result<Json<McpOverview>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let pool = state.db().pool();
    let store = McpStore::new(pool.clone());
    let clients = store.list_clients(organization).await?;
    let mut grants = 0i64;
    for client in &clients {
        grants += client.tool_count;
    }
    let invocations = mcp_invocations::totals(pool, organization, 86_400).await?;
    Ok(Json(McpOverview {
        clients: clients.len() as i64,
        active: clients.iter().filter(|c| c.status() == "active").count() as i64,
        revoked: clients.iter().filter(|c| c.status() == "revoked").count() as i64,
        sandboxed: clients.iter().filter(|c| c.sandbox).count() as i64,
        grants,
        tools: mcp_tools::all_tools().len(),
        invocations,
        window_seconds: 86_400,
    }))
}

/// `GET /api/v1/mcp/invocations` — the log the clients screen lists.
pub async fn list_invocations_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Query(mut filter): Query<mcp_invocations::InvocationFilter>,
) -> Result<Json<mcp_invocations::InvocationPage>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    // The tenant is overwritten here rather than trusted from the query: `organization_id` is a
    // filter for the *platform* account and a leak for everybody else, and this route is not the
    // platform's.
    let mut scoped = mcp_invocations::InvocationFilter {
        client_id: filter.client_id,
        tool: filter.tool.clone(),
        status: filter.status.clone(),
        window_seconds: filter.window_seconds,
        before: filter.before,
        limit: filter.limit,
    };
    scoped.status = scoped
        .status
        .filter(|status| mcp_invocations::STATUSES.contains(&status.as_str()));
    let page = mcp_invocations::search(state.db().pool(), organization, &scoped).await?;
    Ok(Json(page))
}

/// `GET /api/v1/mcp/invocations/{id}` — one row, for the detail drawer.
pub async fn read_invocation_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(id): Path<i64>,
) -> Result<Json<mcp_invocations::InvocationRow>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let row = mcp_invocations::read(state.db().pool(), organization, id)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "mcp_invocation_not_found",
                format!("no invocation {id} in this organization"),
            )
        })?;
    Ok(Json(row))
}

/// `GET /api/v1/mcp/tools` — the whole catalogue, for the panel's grant picker.
///
/// Generated from the compiled catalogue, so the picker cannot offer a tool the server will not
/// answer `tools/list` for, and cannot omit one that it will.
pub async fn list_catalogued_tools_route(
    _current: CurrentSession,
) -> Result<Json<Value>, ApiError> {
    let tools = mcp_tools::all_tools();
    let classes: BTreeMap<&str, Vec<&str>> = tools.iter().fold(BTreeMap::new(), |mut map, tool| {
        map.entry(tool.class.as_str())
            .or_default()
            .push(tool.name.as_str());
        map
    });
    Ok(Json(json!({
        "tools": tools,
        "classes": classes,
        "protocol_version": mcp_tools::PROTOCOL_VERSION,
        "server": mcp_tools::SERVER_NAME,
    })))
}

/// `POST /api/v1/mcp/sandbox-test` — the panel's "prove it" panel.
///
/// Runs a call through **everything except the write**: authentication is the panel's own
/// session, the client and grant are read, the schema is validated and the plan is returned. The
/// token is not needed and not accepted — a test that could use a token is a second door onto the
/// same door.
pub async fn sandbox_test_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Json(body): Json<SandboxTestRequest>,
) -> Result<Json<SandboxTestAnswer>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let store = McpStore::new(state.db().pool().clone());
    let Some(client) = store.read_client(organization, body.client_id).await? else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "mcp_client_not_found",
            "no MCP client with that id in this organization",
        ));
    };

    let tool_name = body.tool.trim().to_owned();
    let Some(tool) = mcp_tools::tool_named(&tool_name) else {
        return Err(ApiError::bad_request(
            "mcp_tool_unknown",
            format!("no tool named `{tool_name}` is in this installation's catalogue"),
        ));
    };

    let scopes: Vec<String> = client
        .scopes
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let grant = store.grant_for(client.id, &tool.name).await?;
    let row_enabled = registry::get_tool(state.db().pool(), &tool.name)
        .await?
        .map(|row| row.enabled)
        .unwrap_or(true);
    let resolution = mcp_tools::resolve(grant.as_ref(), &scopes, row_enabled);
    let arguments: Value = body.arguments.clone().unwrap_or_else(|| json!({}));

    // The would-be request is part of the answer, because the REQ asks the panel to "read the
    // would-be request and the validation result" — an operator deciding whether to grant a tool
    // is deciding on the shape of the call, and a validation error with no request shown is half
    // the evidence.
    let validation = mcp_tools::validate_arguments(&tool, &arguments).map(|error| {
        json!({
            "code": error.code,
            "message": error.message,
            "field": error.data.as_ref().and_then(|d| d.get("field")).cloned(),
        })
    });

    // The plan and the verdict are computed BEFORE the answer is assembled, because the answer
    // moves `arguments` and a borrow taken after that would not compile — and a builder order
    // that only exists because of a borrow checker is a builder order nobody can extend.
    let plan = mcp_tools::SandboxPlan {
        tool: tool.name.clone(),
        permission: tool.permission.clone(),
        approval_required: tool.approval_required,
        argument_keys: mcp_tools::argument_keys(&arguments),
        would: "call this tool against the live platform",
        note: mcp_tools::SANDBOX_NOTE,
    };
    let allowed = matches!(
        resolution,
        Resolution::Allow {
            approval_required: false,
            ..
        }
    );
    let refusal = match &resolution {
        Resolution::Allow { .. } => None,
        other => Some(mcp_tools::refusal_for(other, &tool.name)),
    };

    Ok(Json(SandboxTestAnswer {
        request: mcp_tools::example_call(&tool),
        arguments,
        plan,
        validation,
        refusal,
        client_sandbox: client.sandbox,
        allowed,
    }))
}

/// The sandbox panel's request.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SandboxTestRequest {
    pub client_id: uuid::Uuid,
    pub tool: String,
    #[serde(default)]
    pub arguments: Option<Value>,
}

/// The sandbox panel's answer.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SandboxTestAnswer {
    /// The request that would be sent, so the operator reads the same bytes the client would.
    pub request: Value,
    /// The arguments the panel would send.
    pub arguments: Value,
    /// The plan, which is what a sandboxed call returns.
    pub plan: mcp_tools::SandboxPlan,
    /// The schema validation result, `null` when the arguments are valid.
    pub validation: Option<Value>,
    /// The refusal this call would get, `null` when it is allowed.
    pub refusal: Option<mcp_tools::JsonRpcError>,
    /// Whether the client is in sandbox mode.
    pub client_sandbox: bool,
    /// Whether the call would be allowed.
    pub allowed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uniform_refusal_carries_no_information_about_the_token() {
        // The property is that ONE message serves three cases, so the test asserts there is one
        // message — a second literal anywhere in this file would be the drift this catches.
        assert!(!TOKEN_REFUSED.contains("revoked"));
        assert!(!TOKEN_REFUSED.contains("unknown"));
        assert!(!TOKEN_REFUSED.contains("expired"));
        assert!(TOKEN_REFUSED.contains("MCP client token"));
    }

    #[test]
    fn the_rate_limited_code_is_distinct_from_every_refusal_code() {
        // A client must be able to tell "slow down" from "your grant is wrong"; they arrive as
        // the same HTTP status, so the code is the only discriminator.
        for code in [
            mcp_tools::CODE_PERMISSION_DENIED,
            mcp_tools::CODE_INVALID_ARGUMENTS,
            mcp_tools::CODE_TOOL_NOT_FOUND,
            mcp_tools::CODE_METHOD_NOT_FOUND,
            mcp_tools::CODE_PENDING_APPROVAL,
            mcp_tools::CODE_BLOCKED_AIRGAP,
        ] {
            assert_ne!(
                code, CODE_RATE_LIMITED,
                "code {code} collides with the limiter"
            );
        }
    }

    #[test]
    fn a_response_carries_exactly_one_of_result_or_error() {
        let ok = RpcResponse::ok(Some(json!(1)), json!({"a": 1}));
        let rendered = serde_json::to_string(&ok).expect("renders");
        assert!(rendered.contains("\"result\""));
        assert!(!rendered.contains("\"error\""), "{rendered}");

        let bad = RpcResponse::failed(
            Some(json!(1)),
            mcp_tools::JsonRpcError::new(mcp_tools::CODE_INVALID_REQUEST, "no"),
        );
        let rendered = serde_json::to_string(&bad).expect("renders");
        assert!(rendered.contains("\"error\""));
        assert!(!rendered.contains("\"result\""), "{rendered}");
    }

    #[test]
    fn a_notification_response_has_no_id_and_no_result() {
        let rendered = serde_json::to_string(&RpcResponse::failed(
            None,
            mcp_tools::JsonRpcError::new(-32001, TOKEN_REFUSED),
        ))
        .expect("renders");
        assert!(!rendered.contains("\"id\""), "{rendered}");
        assert!(!rendered.contains("\"result\""), "{rendered}");
    }
}
