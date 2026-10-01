//! Running one API call as the signed-in caller (REQ-033, slice 2).
//!
//! # What the Explorer is, and what it must not become
//!
//! The request file's risk note is the sharpest sentence in the whole specification: *"The
//! Explorer can resemble a privileged proxy: it must run with the caller's session and
//! permissions only."* A design that made an outbound HTTP request to `localhost` with a
//! service credential, or that dispatched into the handlers while skipping the guards, would
//! look identical in the panel and be a different platform entirely: the first is a superuser
//! wearing a developer's clothes, the second is a hole in every permission on the platform.
//!
//! So this module dispatches **in process, through the same `Router`, with the caller's own
//! session cookie attached**. Every layer therefore runs for real — the permission guard, CSRF,
//! the rate limiter, the module guard — and the answer the Explorer shows is the answer the
//! caller would have got typing the same request into their own terminal. There is no path here
//! that reaches a handler without passing the layers, which is the property that makes the
//! Explorer's `403` meaningful rather than decorative.
//!
//! # What the pre-check in front of the guard is for
//!
//! [`prepare`] resolves the operation's declared permission against the caller *before*
//! dispatch, and refuses there. That is not a second authorisation: the guard runs immediately
//! afterwards and is the authority. It is there so the refusal arrives with the operation's
//! permission name in it — a form that says "you cannot publish pages" teaches, where a bare
//! `403 permission_denied` from four layers down does not.
//!
//! # The loop hazard, and why the dispatch cannot recurse
//!
//! The Explorer is itself a route. A call to `POST /api/v1/dev/explorer/requests` sent from the
//! Explorer would dispatch into a router that dispatches into a router, and a caller who found
//! the button twice would be able to spend one budget on many. So the Explorer is on
//! [`REFUSABLE`]'s counterpart list: [`assert_dispatchable`] refuses any path under
//! `/api/v1/dev/`. One static rule, checked in one place, and the Explorer cannot reach itself
//! whatever the document says about it.

use std::net::IpAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, header};
use omnion_developer::openapi::{self, Operation};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// Prefix the Explorer refuses to dispatch to.
///
/// The whole `/api/v1/dev/` namespace, not just the two Explorer routes: a module may add
/// another debugging surface there later, and a list of two paths is a list that has to be
/// remembered to grow.
pub const SELF_PREFIX: &str = "/api/v1/dev/";

/// Largest request body the Explorer will forward, in bytes.
///
/// The platform's own cap is 2 MiB (`DefaultBodyLimit`); this is an order below it, because a
/// body pasted into a form is a debugging artefact and a debugging artefact that can ship a
/// megabyte at a time is a way to fill the request log.
pub const MAX_BODY_BYTES: usize = 128 * 1024;

/// The path prefix every operation shares.
const API_PREFIX: &str = "/api/v1";

/// How many Explorer calls one caller may make per window.
///
/// Deliberately its own number rather than a reuse of the platform's `authenticated_api`
/// budget: the Explorer is the one surface where a *legitimate* user can generate an unbounded
/// number of requests in a minute by clicking `Send` repeatedly, and it is also the one surface
/// an attacker with a stolen session wants. The request file asks for "it is rate-limited per
/// user"; this is that limit, enforced by the same counter machinery the middleware uses so
/// there is no second counting implementation to get subtly wrong.
pub const RUNS_PER_WINDOW: i32 = 60;

/// The window [`RUNS_PER_WINDOW`] is measured over, in seconds.
pub const RUN_WINDOW_SECONDS: i64 = 60;

/// Body of `POST /api/v1/dev/explorer/requests`.
///
/// `path` is the **template** (`/api/v1/pages/{id}`) plus the values the form filled in — see
/// [`Resolved`]. The form does not have to send a path that exists: the Explorer's whole job is
/// to send paths that do not exist yet, and the router answers `404` exactly as it would for
/// anybody else.
#[derive(Debug, Deserialize)]
pub struct RunRequest {
    /// The HTTP method, uppercase.
    pub method: String,
    /// The path template, e.g. `/api/v1/pages/{id}`.
    pub path: String,
    /// Filled path parameters, in the template's order when the form left them positional.
    #[serde(default)]
    pub path_params: Vec<String>,
    /// Query pairs, as `name=value` strings.
    #[serde(default)]
    pub query: Vec<String>,
    /// Raw request body, exactly as typed.
    #[serde(default)]
    pub body: Option<String>,
}

impl RunRequest {
    /// The method, normalised — a form that sends `get` is not a different verb.
    fn method(&self) -> Method {
        Method::from_bytes(self.method.trim().to_ascii_uppercase().as_bytes())
            .unwrap_or(Method::GET)
    }
}

/// A request the Explorer is about to send, with its placeholders filled in.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The method, uppercase.
    pub method: Method,
    /// The absolute path, with every path parameter substituted.
    pub path: String,
    /// The query string, without the leading `?`. Empty when there is none.
    pub query: String,
    /// The body, as text.
    pub body: Option<String>,
}

/// The answer to one Explorer call.
///
/// A *result* and not an error, deliberately: the request file says a failed call "renders as a
/// normal result (status, body, latency), not a page error". A `403` is an answer the developer
/// asked for, and a screen that replaces itself with an error box for a `404` teaches nothing
/// about the platform they are trying to learn.
#[derive(Debug, Serialize)]
pub struct RunResult {
    /// The status the real request produced.
    pub status: u16,
    /// The body, pretty-printed when it is JSON and raw otherwise.
    pub body: String,
    /// Whether the body was JSON — the panel pretty-prints only when it can.
    pub body_is_json: bool,
    /// Wall-clock milliseconds, measured around the whole dispatch.
    pub duration_ms: u64,
    /// The exact path that was sent, so the snippet and the request log agree.
    pub sent_path: String,
    /// The request id, when the handler stamped one.
    pub request_id: Option<String>,
    /// `true` when the answer was produced by a layer rather than a handler.
    ///
    /// A `403` from the permission guard and a `403` from CSRF are both refusals, and telling
    /// them apart in the panel is the difference between "your role cannot do this" and "this
    /// request is not a browser request".
    pub refused: bool,
}

/// Why an Explorer call never left the handler, as its own error type.
///
/// A distinct type from [`ApiError`] because these are refusals of the *form*, not of the
/// request: a path outside `/api/v1` is something the form can produce and the router should
/// never see, and it is a `400` with a message the form can put next to the field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The path is not under the versioned prefix.
    NotApiPath,
    /// The path leaves the versioned prefix: `..` is the one that matters, and it is a path
    /// traversal rather than a bad path.
    PathEscape,
    /// The path is under `/api/v1/dev/`, which the Explorer may not call.
    Recursive,
    /// The body is larger than [`MAX_BODY_BYTES`].
    BodyTooLarge,
    /// The caller's budget for this window is spent.
    RateLimited,
    /// The caller's session is not signed in, so there is nothing to run as.
    NotSignedIn,
    /// The operation is documented and the caller does not hold its permission.
    Permission(&'static str),
}

impl Refusal {
    /// The `ApiError` this becomes, with a message a person can act on.
    #[must_use]
    pub fn into_error(self) -> ApiError {
        match self {
            Self::NotApiPath => ApiError::bad_request(
                "explorer_path_not_api",
                "the Explorer only calls this platform's own API — start the path with /api/v1",
            ),
            Self::PathEscape => {
                ApiError::bad_request("explorer_path_escape", "the path may not leave /api/v1")
            }
            Self::Recursive => {
                ApiError::bad_request("explorer_recursive", "the Explorer may not call itself")
            }
            Self::BodyTooLarge => ApiError::bad_request(
                "explorer_body_too_large",
                format!("the request body is larger than the {MAX_BODY_BYTES} byte limit"),
            ),
            Self::RateLimited => ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "explorer_rate_limited",
                format!("the Explorer allows {RUNS_PER_WINDOW} calls per {RUN_WINDOW_SECONDS}s"),
            )
            .with_retry_after(RUN_WINDOW_SECONDS),
            Self::NotSignedIn => {
                ApiError::unauthorized("explorer_no_session", "sign in before sending a call")
            }
            Self::Permission(permission) => ApiError::forbidden(
                "permission_denied",
                format!("this call needs the {permission} permission"),
            )
            .with_details(json!({ "permission": permission })),
        }
    }
}

/// Fill the placeholders and check the shape of a call, before anything is sent.
///
/// Pure: no database, no router, no session. That is what makes it testable without a stack and
/// what keeps the security rules — no escapes, no recursion, bounded body — in one readable
/// function rather than spread across a handler.
pub fn prepare(request: &RunRequest) -> Result<Resolved, Refusal> {
    let template = request.path.trim();
    if !template.starts_with(API_PREFIX) {
        return Err(Refusal::NotApiPath);
    }
    // `%2e%2e`, `..`, and a template that resolves above the prefix all reach the same place, and
    // the check is on the *substituted* path rather than the template because a path parameter
    // is attacker-controlled text: `page_id = "../../admin"` must be caught here, and checking
    // the template would not see it.
    let resolved = substitute(template, &request.path_params);
    if resolved.contains("..") {
        return Err(Refusal::PathEscape);
    }
    if !resolved.starts_with(API_PREFIX) {
        return Err(Refusal::PathEscape);
    }
    if resolved.starts_with(SELF_PREFIX) {
        return Err(Refusal::Recursive);
    }
    if let Some(body) = &request.body {
        if body.len() > MAX_BODY_BYTES {
            return Err(Refusal::BodyTooLarge);
        }
    }

    let query = build_query(&request.query);
    Ok(Resolved {
        method: request.method(),
        path: resolved,
        query,
        body: request.body.clone(),
    })
}

/// Replace `{name}` placeholders with the supplied values, in order.
///
/// Positional rather than by name, because the form knows the order of the parameters it drew
/// from the schema and a name-keyed map would need the template parsed a second time. A
/// placeholder with no value is left as written: the router answers `400` for an unresolved
/// `{id}` (it will not match a route), which is a better error than sending a literal `{id}` to
/// a handler that would treat it as an id.
fn substitute(template: &str, values: &[String]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    let mut next = 0;
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}').map(|index| index + open) else {
            break;
        };
        out.push_str(&rest[..open]);
        match values.get(next) {
            Some(value) => {
                out.push_str(value);
                next += 1;
            }
            // No value: emit the placeholder verbatim so the route does not match and the caller
            // sees a `404` naming the missing segment rather than a value they did not type.
            None => out.push_str(&rest[open..=close]),
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Join query pairs, dropping any that are not `name=value`.
///
/// A pair with no `=` is dropped rather than sent as a bare key: the platform's own query
/// parsers answer `400` on a malformed value, and a form that produced one is a form bug, not a
/// finding about the API.
///
/// **Both halves are trimmed.** A form input is a string a person typed, and `" offset=5 "` is
/// what a person produces by pasting — a trailing space in a query value is a `400` from every
/// parser downstream, and a `400` that traces back to invisible whitespace in a field the
/// person cannot see is a support ticket rather than a bug report.
fn build_query(pairs: &[String]) -> String {
    pairs
        .iter()
        .filter_map(|pair| pair.split_once('='))
        .map(|(name, value)| format!("{}={}", name.trim(), value.trim()))
        .filter(|pair| !pair.is_empty())
        .collect::<Vec<_>>()
        .join("&")
}

/// Whether the caller holds `permission`.
///
/// A thin wrapper over the IAM resolution so the handler reads as intent and the scope it
/// authorises in is decided in exactly one place — the same [`crate::guards::scope_of`] the real
/// guard uses, because an Explorer that resolved permissions in a *different* scope than the
/// guard would be a second opinion on the same question.
pub async fn holds(
    state: &AppState,
    user: &omnion_identity::User,
    permission: &str,
) -> Result<bool, ApiError> {
    let scope = crate::guards::scope_of(user);
    let decision = omnion_permissions::authorize(state.db().pool(), user.id, scope, permission)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "permission_resolution_failed",
                error.to_string(),
            )
        })?;
    Ok(decision.is_allowed())
}

/// The operation the call is addressed to, if the document describes one.
pub fn operation_of(resolved: &Resolved) -> Option<&'static Operation> {
    openapi::find(resolved.method.as_str(), &resolved.path)
}

/// The operations this caller may see in the Explorer.
///
/// Read-only, and the Explorer has to *know* the caller's permission set to draw this: a browser
/// that hides a button the API would refuse is a convenience; one that shows it and then
/// explains the `403` is a reference. Both are acceptable, and this implementation does the
/// second — the operation list is filtered, and a refused send names the permission.
pub async fn operations_for(
    state: &AppState,
    user: &omnion_identity::User,
) -> Result<Vec<Value>, ApiError> {
    let mut visible = Vec::new();
    for operation in openapi::OPERATIONS.iter() {
        // `None` is "no permission", which is visible to anybody who reached the page at all.
        let permitted = match operation.permission {
            None => true,
            Some(permission) => holds(state, user, permission).await?,
        };
        if permitted {
            visible.push(operation_value(operation));
        }
    }
    Ok(visible)
}

/// One operation as the panel's browser consumes it.
#[must_use]
pub fn operation_value(operation: &Operation) -> Value {
    json!({
        "id": openapi::operation_id(operation),
        "method": operation.method,
        "path": operation.path,
        "tag": operation.tag,
        "summary": operation.summary,
        "permission": operation.permission,
        "parameters": operation
            .parameters
            .iter()
            .map(|parameter| serde_json::to_value(parameter).unwrap_or_else(|_| json!({})))
            .collect::<Vec<_>>(),
        "body": operation.body.as_ref().map(openapi::Schema::to_value),
    })
}

/// Spend one of the caller's Explorer runs, or refuse.
///
/// The counter is Redis and the key is the **user**, not the address: the limit exists to bound
/// what one person can do, and a person behind a rotating address is the same person. It is a
/// separate key prefix from the platform's own scopes, so spending Explorer budget can never
/// exhaust an API budget (and the reverse), which matters because the Explorer is a *tool* — a
/// developer debugging a rate limit should not have their debugging itself rate-limited by the
/// limit they are debugging.
///
/// A Redis that does not answer **allows** the call and says so in the log, for the same reason
/// `rate_limit_middleware` does: a limiter that takes the platform down with its own dependency
/// is worse than no limiter, and the log line is what makes the absence visible.
pub async fn spend_run(state: &AppState, user_id: Uuid) -> Result<(), Refusal> {
    let key = format!("omnion:explorer:runs:{user_id}");
    let Ok(mut connection) = state.redis().connection().await else {
        tracing::warn!(
            user = %user_id,
            "the Explorer counter was unreachable; the call was allowed and NOT counted"
        );
        return Ok(());
    };
    let window = RUN_WINDOW_SECONDS;
    let counted: Option<i32> = redis::cmd("INCR")
        .arg(&key)
        .query_async(&mut connection)
        .await
        .ok();
    let Some(count) = counted else {
        tracing::warn!(user = %user_id, "the Explorer counter could not be read; NOT counted");
        return Ok(());
    };
    if count == 1 {
        // Only the first caller in a window sets the expiry, so a busy window does not keep
        // extending itself and a caller's budget silently never resets.
        let _: Result<(), _> = redis::cmd("EXPIRE")
            .arg(&key)
            .arg(window)
            .query_async(&mut connection)
            .await;
    }
    if count > RUNS_PER_WINDOW {
        return Err(Refusal::RateLimited);
    }
    Ok(())
}

/// Send the call and return the answer.
///
/// `router` is the **application's own** router, passed in rather than rebuilt here: rebuilding
/// it would install a second header layer, a second limiter and a second IP access list into
/// process-wide `OnceLock`s, and the second one would be silently ignored (or, worse, the first
/// install would win and the Explorer would run with the outer layer's policy). The router
/// travels in the state of the process that is already running it.
///
/// # Why the session cookie is copied rather than the session resolved
///
/// The dispatch has to be a *request*, not a direct handler call, or none of the layers run.
/// Attaching the caller's own `omnion_session` cookie is what makes the guard, CSRF, the module
/// guard and the limiter all see an ordinary signed-in browser request. The cookie is never
/// logged and never leaves the process; it is put on an in-process request object and dropped
/// with it.
///
/// # The CSRF token is derived, not forwarded
///
/// A mutation has to pass the CSRF layer like any other, and the layer's token is a derivation
/// of the *session id* under the deployment's secret — so it is recomputed here from the same
/// two inputs the panel's own client reads. Forwarding the outer request's token would have
/// worked too and is the simpler code; it is wrong, because the derivation is per session and a
/// stale cookie in a second tab is exactly the case the layer exists for. When no secret is
/// configured the layer refuses every cookie-authenticated write anyway, so the header is simply
/// omitted rather than guessed.
pub async fn dispatch(
    router: &Router,
    state: &AppState,
    session: &crate::auth::CurrentSession,
    resolved: &Resolved,
    peer: Option<IpAddr>,
) -> Result<RunResult, ApiError> {
    let mut builder =
        Request::builder()
            .method(resolved.method.clone())
            .uri(if resolved.query.is_empty() {
                resolved.path.clone()
            } else {
                format!("{}?{}", resolved.path, resolved.query)
            });
    // The caller's own session, so every guard resolves it exactly as it resolves a browser.
    if let Ok(cookie) = HeaderValue::from_str(&format!(
        "{}={}",
        crate::cookies::SESSION_COOKIE,
        session.token
    )) {
        builder = builder.header(header::COOKIE, cookie);
    }
    if let Some(secret) = state.config().csrf.as_bytes() {
        let token = omnion_security::derive_csrf_token(secret, &session.session.id.to_string());
        if let Ok(value) = HeaderValue::from_str(&token) {
            builder = builder.header(omnion_security::CSRF_HEADER, value);
        }
    }
    if let Some(body) = &resolved.body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    // The peer address the outer request carried, so a route that judges the source address
    // judges the same address the caller's own request did rather than "no address at all" —
    // which, with the IP access list in force, is a `403 ip_unknown` that would look like a
    // permission problem.
    if let Some(peer) = peer {
        if let Ok(value) = HeaderValue::from_str(&peer.to_string()) {
            builder = builder.header("x-forwarded-for", value);
        }
    }
    let request = builder
        .body(match &resolved.body {
            Some(body) => Body::from(body.clone()),
            None => Body::empty(),
        })
        .map_err(|error| ApiError::bad_request("explorer_request_invalid", error.to_string()))?;

    let started = std::time::Instant::now();
    let response = router.clone().oneshot(request).await.map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "explorer_dispatch_failed",
            error.to_string(),
        )
    })?;
    let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), MAX_BODY_BYTES * 4)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "explorer_body_unreadable",
                error.to_string(),
            )
        })?;

    let raw = String::from_utf8_lossy(&bytes).to_string();
    let parsed: Option<Value> = serde_json::from_str(&raw).ok();
    let (body, is_json) = match &parsed {
        Some(value) => (
            serde_json::to_string_pretty(value).unwrap_or_else(|_| raw.clone()),
            true,
        ),
        None => (raw, false),
    };

    Ok(RunResult {
        status: status.as_u16(),
        body,
        body_is_json: is_json,
        duration_ms,
        sent_path: if resolved.query.is_empty() {
            resolved.path.clone()
        } else {
            format!("{}?{}", resolved.path, resolved.query)
        },
        request_id: header_value(&headers, "x-omnion-request-id"),
        refused: !matches!(
            status,
            StatusCode::OK | StatusCode::CREATED | StatusCode::NO_CONTENT
        ) && status.is_client_error(),
    })
}

/// One header's value as text.
fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let name = HeaderName::from_bytes(name.as_bytes()).ok()?;
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// The three snippets the Explorer offers, for a resolved call.
///
/// **The token is a placeholder, always.** A snippet that carried the caller's real session
/// would be a credential written to a clipboard, a screenshot and a bug report — and the request
/// file says so twice, in the visual check and in the risks. The placeholder is shaped like a
/// real key so the snippet is copy-pasteable *after* the reader has done the obvious thing:
/// it names the environment variable they must fill in.
pub fn snippets(resolved: &Resolved) -> Vec<Snippet> {
    let url = format!("{{API_BASE}}{}", resolved.path);
    let target = if resolved.query.is_empty() {
        url.clone()
    } else {
        format!("{url}?{}", resolved.query)
    };
    let body = resolved.body.clone().unwrap_or_default();

    let mut curl = format!(
        "curl -X {} \\\n  -H 'Authorization: Bearer $OMNION_API_KEY' \\\n  -H 'Content-Type: application/json' \\\n  '{target}'",
        resolved.method
    );
    if !body.is_empty() {
        curl.push_str(&format!(" \\\n  -d '{}'", body.replace('\'', "'\\''")));
    }

    let ts_method = match resolved.method {
        axum::http::Method::GET => "req.get",
        axum::http::Method::POST => "req.post",
        axum::http::Method::PUT => "req.put",
        axum::http::Method::PATCH => "req.patch",
        axum::http::Method::DELETE => "req.delete",
        _ => "req.request",
    };
    let typescript = format!(
        "const response = await fetch(\"{target}\", {{\n  method: \"{}\",\n  headers: {{\n    Authorization: `Bearer ${{OMNION_API_KEY}}`,\n    \"Content-Type\": \"application/json\",\n  }},{}\n}});\n\nconsole.log(response.status, await response.json());",
        resolved.method,
        if body.is_empty() {
            String::new()
        } else {
            format!("\n  body: JSON.stringify({body}),")
        }
    );

    let python = format!(
        "import os, requests\n\nresponse = requests.request(\n    \"{}\",\n    \"{target}\",\n    headers={{\n        \"Authorization\": \"Bearer \" + os.environ[\"OMNION_API_KEY\"],\n        \"Content-Type\": \"application/json\",\n    }},{}\n)\n\nprint(response.status_code, response.json())",
        resolved.method,
        if body.is_empty() {
            String::new()
        } else {
            format!("\n    json={body},")
        }
    );

    vec![
        Snippet {
            language: "curl".to_owned(),
            code: curl,
        },
        Snippet {
            language: "typescript".to_owned(),
            code: typescript,
        },
        Snippet {
            language: "python".to_owned(),
            code: python,
        },
    ]
}

/// One copyable form of a call.
#[derive(Debug, Serialize)]
pub struct Snippet {
    /// `curl`, `typescript` or `python`.
    pub language: String,
    /// The code, with `$OMNION_API_KEY` as the credential placeholder.
    pub code: String,
}

/// The router, handed in from `main.rs`.
///
/// The application router is not reachable from [`AppState`] because it does not exist yet when
/// state is built — so the Explorer holds it in a process-wide cell, installed once at boot. A
/// `OnceLock` rather than a `RwLock` because the router is immutable after boot and there is
/// nothing to swap.
static ROUTER: std::sync::OnceLock<Arc<Router>> = std::sync::OnceLock::new();

/// Install the running router for the Explorer to dispatch through.
pub fn install_router(router: Router) {
    let _ = ROUTER.set(Arc::new(router));
}

/// The installed router, if the process installed one.
#[must_use]
pub fn router() -> Option<Arc<Router>> {
    ROUTER.get().cloned()
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/dev/openapi.json` — the document, as served.
///
/// The raw [`omnion_developer::openapi::document`] and not the panel's *filtered* view, because
/// the point of the endpoint is that a developer can point a code generator at it. The
/// permission filter is a panel affordance (see [`list_operations`]), not a security control:
/// hiding a path from a document is not a way to protect it, and pretending otherwise would be
/// the kind of "hidden feature" the platform's own rules forbid.
pub async fn openapi_document() -> Result<axum::Json<serde_json::Value>, ApiError> {
    Ok(axum::Json(omnion_developer::openapi::document()))
}

/// `GET /api/v1/dev/operations` — the operations this caller may see, for the panel's browser.
///
/// A separate endpoint rather than a query flag on the document, because the two have different
/// contracts: the document is a *standard* (OpenAPI 3.1, `paths`, no custom keys at the top) and
/// an extension to it would break every generator pointed at it, while this is the platform's own
/// shape for its own UI. A client that wants the reference asks for the reference.
pub async fn list_operations(
    State(state): State<AppState>,
    session: crate::auth::CurrentSession,
) -> Result<axum::Json<ExplorerOperations>, ApiError> {
    let operations = operations_for(&state, &session.user).await?;
    // Whether the caller may *send* at all, resolved here rather than left to the panel.
    //
    // The panel cannot work it out: a role's permissions are not in the operations list, and a
    // client that guessed would guess wrong in the one direction that matters — a Send button
    // that is enabled and always `403` is worse than one that is honestly disabled, because it
    // teaches that the Explorer is broken rather than that the role is read-only. And the
    // reverse mistake is the one this replaces: comparing the *selected operation's*
    // permission against the run key answers a different question entirely, and answers it
    // "no" for every operation including the ones an owner can send.
    let can_send = holds(&state, &session.user, "developer.explorer.run").await?;
    Ok(axum::Json(ExplorerOperations {
        operations,
        can_send,
    }))
}

/// The body of `GET /api/v1/dev/operations`.
#[derive(Debug, Serialize)]
pub struct ExplorerOperations {
    /// The operations the caller holds the permission for, in the document's own order.
    pub operations: Vec<Value>,
    /// Whether the caller may send a call at all.
    ///
    /// Sent as a first-class field rather than derived by the panel from the operation list,
    /// because "may I read the reference" and "may I act as the person at this screen" are two
    /// different permissions and only this process can answer the second one.
    pub can_send: bool,
}

/// `POST /api/v1/dev/explorer/requests` — run one call as the signed-in caller.
///
/// The order of the checks is the whole handler, and each step exists for a different failure:
///
/// 1. **Budget** ([`spend_run`]) first, so a caller who has spent theirs is refused before the
///    work of resolving anything — and before the shape check, because the rate limiter is
///    there for the volume, not for the content.
/// 2. **Shape** ([`prepare`]) second: no path outside `/api/v1`, no escape, no recursion, bounded
///    body. All of these are refusals of the *form*, and they are `400`s with a message the
///    panel can put next to the field.
/// 3. **Permission** third, from the document, so the refusal names the permission. The real
///    guard runs inside the dispatch immediately after and is the authority — this is the
///    explanatory layer, not a second authorisation.
/// 4. **Dispatch** last, through the real router with the caller's own session.
pub async fn run_request(
    State(state): State<AppState>,
    session: crate::auth::CurrentSession,
    address: crate::client_ip::ClientAddress,
    axum::Json(input): axum::Json<RunRequest>,
) -> Result<axum::Json<ExplorerRun>, ApiError> {
    spend_run(&state, session.user.id)
        .await
        .map_err(Refusal::into_error)?;

    let resolved = prepare(&input).map_err(Refusal::into_error)?;

    // The document's own permission, resolved before dispatch so a refusal explains itself. An
    // *undocumented* path is not refused here: a developer is allowed to try a path the
    // reference does not carry, and the router's `404` is the honest answer for it.
    if let Some(operation) = operation_of(&resolved) {
        if let Some(permission) = operation.permission {
            if !holds(&state, &session.user, permission).await? {
                return Err(Refusal::Permission(permission).into_error());
            }
        }
    }

    let router = router().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "explorer_not_installed",
            "the API Explorer is not available in this process",
        )
    })?;

    let result = dispatch(&router, &state, &session, &resolved, address.0).await?;
    Ok(axum::Json(ExplorerRun {
        result,
        snippets: snippets(&resolved),
    }))
}

/// The body of a successful `POST /api/v1/dev/explorer/requests`.
///
/// The snippets ride the *answer* rather than being a second call: the form may have changed a
/// path parameter since the call was sent, and a snippet for a different call than the one whose
/// status is on screen is a snippet nobody can trust.
#[derive(Debug, Serialize)]
pub struct ExplorerRun {
    /// What the real request answered.
    pub result: RunResult,
    /// curl, TypeScript and Python, with `$OMNION_API_KEY` as the credential.
    pub snippets: Vec<Snippet>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(method: &str, path: &str) -> RunRequest {
        RunRequest {
            method: method.to_owned(),
            path: path.to_owned(),
            path_params: Vec::new(),
            query: Vec::new(),
            body: None,
        }
    }

    #[test]
    fn a_path_outside_the_api_is_refused_before_anything_is_sent() {
        assert_eq!(
            prepare(&run("GET", "/admin")).unwrap_err(),
            Refusal::NotApiPath
        );
        assert_eq!(prepare(&run("GET", "")).unwrap_err(), Refusal::NotApiPath);
        assert_eq!(
            prepare(&run("GET", "https://example.com/api/v1/pages")).unwrap_err(),
            Refusal::NotApiPath
        );
    }

    #[test]
    fn a_path_parameter_cannot_climb_out_of_the_prefix() {
        // The template is clean; the *value* is the attack. Checking only the template is how a
        // path traversal gets into a router that is otherwise perfectly guarded.
        let mut request = run("GET", "/api/v1/pages/{id}");
        request.path_params = vec!["../../admin".to_owned()];
        assert_eq!(prepare(&request).unwrap_err(), Refusal::PathEscape);
    }

    #[test]
    fn the_explorer_cannot_call_itself() {
        // Both its own routes and anything a future module puts beside them.
        assert_eq!(
            prepare(&run("POST", "/api/v1/dev/explorer/requests")).unwrap_err(),
            Refusal::Recursive
        );
        assert_eq!(
            prepare(&run("GET", "/api/v1/dev/openapi.json")).unwrap_err(),
            Refusal::Recursive
        );
        assert_eq!(
            prepare(&run("GET", "/api/v1/dev/anything-else")).unwrap_err(),
            Refusal::Recursive
        );
    }

    #[test]
    fn a_path_parameter_is_substituted_in_order() {
        let mut request = run("POST", "/api/v1/pages/{site_id}/blocks/{id}");
        request.path_params = vec!["site-1".to_owned(), "block-9".to_owned()];
        let resolved = prepare(&request).expect("a full template resolves");
        assert_eq!(resolved.path, "/api/v1/pages/site-1/blocks/block-9");
    }

    #[test]
    fn an_unfilled_parameter_stays_visible_in_the_path() {
        // Left as `{id}` the route does not match and the caller gets a `404` naming the missing
        // segment. Filled with an empty string it would 404 the same way but with a *valid-looking*
        // path, and the form would say "the id is wrong" for a field the developer never filled.
        let resolved = prepare(&run("GET", "/api/v1/pages/{id}")).expect("the template is allowed");
        assert_eq!(resolved.path, "/api/v1/pages/{id}");
    }

    #[test]
    fn a_body_over_the_cap_is_refused_rather_than_truncated() {
        let mut request = run("POST", "/api/v1/pages");
        request.body = Some("x".repeat(MAX_BODY_BYTES + 1));
        assert_eq!(prepare(&request).unwrap_err(), Refusal::BodyTooLarge);

        let mut at_cap = run("POST", "/api/v1/pages");
        at_cap.body = Some("x".repeat(MAX_BODY_BYTES));
        assert!(prepare(&at_cap).is_ok(), "exactly at the cap is allowed");
    }

    #[test]
    fn a_query_pair_without_a_value_is_dropped() {
        let mut request = run("GET", "/api/v1/pages");
        request.query = vec![
            "limit=10".to_owned(),
            "broken".to_owned(),
            "  offset =5 ".to_owned(),
        ];
        let resolved = prepare(&request).expect("a query is allowed");
        assert_eq!(resolved.query, "limit=10&offset=5");
    }

    #[test]
    fn the_method_is_normalised_rather_than_rejected() {
        // A form that sends `get` is a typo, not an attack, and `Method::from_bytes` is what
        // decides — the form's own select offers only the five the document knows.
        let resolved = prepare(&run("  get  ", "/api/v1/pages")).expect("a read is allowed");
        assert_eq!(resolved.method, Method::GET);
    }

    #[test]
    fn the_operations_response_carries_whether_the_caller_may_send() {
        // The contract the panel is built on: `can_send` is a *server* answer, not something
        // the client derives. A client that derived it from the operation list got it wrong in
        // the direction that matters -- a Send button that is enabled and always 403, which
        // reads as "the Explorer is broken" rather than "this role is read-only".
        //
        // This cannot exercise the real resolution without a database, so what it holds is the
        // shape: the field exists, it is a bool, and it is a sibling of the list rather than a
        // field on each operation. A per-operation flag would invite the client to sum them,
        // which is the derivation that was wrong.
        let read = ExplorerOperations {
            operations: Vec::new(),
            can_send: true,
        };
        let body = serde_json::to_value(&read).expect("the response serialises");
        assert_eq!(body["can_send"], serde_json::json!(true));
        // A sibling of the list, never a field on each operation: a per-operation flag would
        // invite the client to derive the answer by summing them, which is the derivation that
        // was wrong in the first place.
        assert!(
            body.get("operations").is_some(),
            "the list stays a field of its own"
        );
        let refused = ExplorerOperations {
            operations: Vec::new(),
            can_send: false,
        };
        assert_eq!(
            serde_json::to_value(&refused).expect("serialises")["can_send"],
            serde_json::json!(false),
            "a read-only role is told so, rather than being left to discover it"
        );
    }

    #[test]
    fn a_recursive_refusal_and_a_permission_refusal_carry_different_codes() {
        // The panel routes on the code: one is "fix your form", the other is "ask for a role".
        let recursive = Refusal::Recursive.into_error();
        let permission = Refusal::Permission("content.pages.publish").into_error();
        assert_ne!(recursive.code(), permission.code());
        assert_eq!(permission.status(), StatusCode::FORBIDDEN);
        assert_eq!(recursive.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn every_snippet_uses_the_environment_placeholder_and_never_a_token() {
        let mut request = run("POST", "/api/v1/pages");
        request.body = Some("{\"title\":\"Test\"}".to_owned());
        request.query = vec!["site_id=abc".to_owned()];
        let resolved = prepare(&request).expect("a write is allowed");
        let snippets = snippets(&resolved);

        assert_eq!(snippets.len(), 3);
        for snippet in &snippets {
            assert!(
                snippet.code.contains("OMNION_API_KEY"),
                "the {} snippet must read the key from the environment",
                snippet.language
            );
            assert!(
                !snippet.code.contains("omn_"),
                "the {} snippet looks like it carries a real key",
                snippet.language
            );
            assert!(
                !snippet.code.contains("Bearer ey"),
                "the {} snippet carries a base64 credential",
                snippet.language
            );
        }
    }

    #[test]
    fn a_snippet_keeps_the_query_and_the_body_the_form_built() {
        let mut request = run("GET", "/api/v1/pages");
        request.query = vec!["limit=5".to_owned()];
        let resolved = prepare(&request).expect("a read is allowed");
        let curl = &snippets(&resolved)[0].code;
        assert!(
            curl.contains("{API_BASE}/api/v1/pages?limit=5"),
            "the curl is: {curl}"
        );
    }

    #[test]
    fn the_permission_refusal_names_the_permission_and_not_the_roles() {
        // The request file: "a 403 names the missing permission, never the caller's roles".
        let error = Refusal::Permission("developer.explorer.run").into_error();
        let rendered = serde_json::json!({
            "error": { "code": error.code(), "message": error.message() }
        });
        assert!(
            rendered["error"]["message"]
                .as_str()
                .expect("a message")
                .contains("developer.explorer.run")
        );
        assert!(
            !rendered["error"]["message"]
                .as_str()
                .expect("a message")
                .contains("owner")
        );
    }
}
