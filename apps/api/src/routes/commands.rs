//! `/api/v1/commands` and `/api/v1/command-center/*` — the palette's own surface
//! (docs/requests/REQ-032, slice 1; action commands and their audit, slice 3).
//!
//! The command palette is the panel's front door: one box that opens screens, offers the
//! commands of the features the caller may actually run, and remembers what this account did
//! last. This module is the HTTP shape around it. Four promises it keeps:
//!
//! * **The projection is permission-filtered server-side.** The registry (`omnion-search`'s
//!   [`omnion_search::commands`]) is compiled in and projected through the caller's effective
//!   permissions, so a command the caller cannot run is never sent — not hidden by the panel,
//!   absent from the answer.
//! * **Recents are the caller's own.** Reads, writes and the clear all bind `user_id` from the
//!   session; one account can never read, empty or pollute another's history.
//! * **A recent that can no longer run is not shown.** A stored command is resolved against the
//!   registry and the caller's permissions on read, so a role change cannot leave a dead row in
//!   the palette.
//! * **An action command runs through its owning service, and says so afterwards.** `POST
//!   /commands/{id}/run` re-checks the command's own permission, refuses a command that asks
//!   for confirmation unless the caller confirms, executes the act through the same code the
//!   owning screen uses, and leaves one `command.run` audit entry (actor, command, target,
//!   outcome) plus a count in `command_usage_daily`. The palette's feedback line is derived from
//!   the owning service's own answer, never invented here.

use axum::Json;
use axum::extract::{Path, Query as QueryParams, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_permissions::effective_permissions;
use omnion_search::commands::{self, CommandSpec};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::guards::scope_of;
use crate::state::AppState;

/// Longest query text the palette may remember (mirrors the column's own check).
const MAX_QUERY_CHARS: usize = 200;
/// Newest recents the palette reads back.
const RECENT_LIMIT: i64 = 20;
/// Recents kept per account; everything older is trimmed on write.
const RECENT_KEEP: i64 = 50;

// ---------------------------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------------------------

/// One command, as the panel receives it.
///
/// The id is the stable handle; `kind` says whether running it opens a screen (`navigate`) or
/// acts through a service (`action`); `confirm` asks the panel to show the question first; `route`
/// is where a navigation command lands, or the screen that reads an action's record back; `icon`
/// is a name from the panel's own icon set, never markup.
#[derive(Debug, Serialize)]
pub struct CommandBody {
    pub id: &'static str,
    pub title: &'static str,
    pub group: &'static str,
    pub hint: &'static str,
    pub icon: &'static str,
    pub kind: &'static str,
    pub confirm: bool,
    pub route: &'static str,
    pub keywords: &'static [&'static str],
    pub aliases: &'static [&'static str],
    pub permission: Option<&'static str>,
}

impl From<&'static CommandSpec> for CommandBody {
    fn from(spec: &'static CommandSpec) -> Self {
        Self {
            id: spec.id,
            title: spec.title,
            group: spec.group,
            hint: spec.hint,
            icon: spec.icon,
            kind: spec.kind().as_str(),
            confirm: spec.confirm,
            route: spec.route,
            keywords: spec.keywords,
            aliases: spec.aliases,
            permission: spec.permission,
        }
    }
}

/// Answer of `GET /api/v1/commands`.
#[derive(Debug, Serialize)]
pub struct CommandsResponse {
    pub commands: Vec<CommandBody>,
}

/// Answer of `GET /api/v1/command-center/context`.
#[derive(Debug, Serialize)]
pub struct ContextResponse {
    /// The route the suggestions were resolved for.
    pub route: String,
    pub commands: Vec<CommandBody>,
}

/// One row of the caller's palette history.
///
/// A command row carries the command's current title and route (resolved from the registry), so
/// the palette renders a recent without a second lookup; a query row carries the typed text and,
/// when the search knew it, how many results it answered with.
#[derive(Debug, Serialize)]
pub struct RecentItemBody {
    pub kind: &'static str,
    pub query: Option<String>,
    pub command_id: Option<String>,
    pub title: Option<String>,
    pub route: Option<String>,
    pub result_count: Option<i32>,
    pub created_at: OffsetDateTime,
}

/// Answer of `GET /api/v1/command-center/recent`.
#[derive(Debug, Serialize)]
pub struct RecentResponse {
    pub items: Vec<RecentItemBody>,
}

// ---------------------------------------------------------------------------------------------
// Request shapes
// ---------------------------------------------------------------------------------------------

/// Query string of `GET /api/v1/command-center/context`.
#[derive(Debug, Deserialize)]
pub struct ContextParams {
    /// The panel route the caller is on (`/pages?focus=…`); the panel sends its own pathname.
    pub route: Option<String>,
}

/// Body of `POST /api/v1/command-center/recent`.
#[derive(Debug, Deserialize)]
pub struct RecordBody {
    /// `query` or `command`.
    pub kind: String,
    /// The typed text, for `kind = "query"`.
    pub query: Option<String>,
    /// The registry id, for `kind = "command"`.
    pub command_id: Option<String>,
    /// How many results the search answered with, when the palette knew.
    pub result_count: Option<i64>,
}

/// Body of `POST /api/v1/commands/{id}/run`.
///
/// `confirm` is the caller's own yes: a command that asks before it runs is refused with
/// `confirmation_required` until the request carries it, so "did you mean it" is a rule of the
/// API rather than a decoration of one dialog.
#[derive(Debug, Deserialize)]
pub struct RunBody {
    #[serde(default)]
    pub confirm: bool,
}

/// Answer of a successful run: the owning service's own result, plus the one line the palette
/// shows above it.
#[derive(Debug, Serialize)]
pub struct RunResponse {
    /// The registry id that ran.
    pub command: &'static str,
    /// Always `action`; a navigation command is refused.
    pub kind: &'static str,
    /// `ok` — failures answer as errors instead (with their own audit entry).
    pub outcome: &'static str,
    /// A human line derived from the owning service's answer.
    pub message: String,
    /// The owning feature's response shape, unchanged.
    pub result: serde_json::Value,
}

/// One executed action: the palette's line and the owning service's own shape.
struct ActionResult {
    message: String,
    result: serde_json::Value,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// The commands the caller may run, in registry order.
pub async fn list_commands(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<CommandsResponse>, ApiError> {
    let permissions = caller_permissions(&state, &current).await?;
    let commands = commands::visible(&|key| permissions.allows(key))
        .into_iter()
        .map(CommandBody::from)
        .collect();
    Ok(Json(CommandsResponse { commands }))
}

/// The commands worth suggesting on the route the caller is on.
pub async fn context(
    State(state): State<AppState>,
    current: CurrentSession,
    QueryParams(params): QueryParams<ContextParams>,
) -> Result<Json<ContextResponse>, ApiError> {
    let route = params.route.unwrap_or_default().trim().to_owned();
    if route.is_empty() || !route.starts_with('/') {
        return Err(ApiError::bad_request(
            "route_required",
            "the \"route\" query parameter must carry the panel path the caller is on",
        ));
    }

    let permissions = caller_permissions(&state, &current).await?;
    let commands = commands::suggest(&route, &|key| permissions.allows(key))
        .into_iter()
        .map(CommandBody::from)
        .collect();
    Ok(Json(ContextResponse { route, commands }))
}

/// The caller's own recent queries and commands, newest first.
pub async fn recent(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<RecentResponse>, ApiError> {
    let permissions = caller_permissions(&state, &current).await?;
    let rows = sqlx::query(
        "select kind, query, command_id, result_count, created_at from command_recents \
         where user_id = $1 order by created_at desc, id desc limit $2",
    )
    .bind(current.user.id)
    .bind(RECENT_LIMIT)
    .fetch_all(state.db().pool())
    .await
    .map_err(store)?;

    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        let kind: String = row.get("kind");
        let command_id: Option<String> = row.get("command_id");
        let result_count: Option<i32> = row.get("result_count");
        let created_at: OffsetDateTime = row.get("created_at");

        if kind == "command" {
            // Resolved live: a command the caller may no longer run is dropped rather than shown
            // as a row that would be refused.
            let Some(id) = command_id.as_deref() else {
                continue;
            };
            let Some(spec) = commands::command(id) else {
                continue;
            };
            if !spec.permission.is_none_or(|key| permissions.allows(key)) {
                continue;
            }
            items.push(RecentItemBody {
                kind: "command",
                query: None,
                command_id: Some(spec.id.to_owned()),
                title: Some(spec.title.to_owned()),
                route: Some(spec.route.to_owned()),
                result_count: None,
                created_at,
            });
        } else {
            items.push(RecentItemBody {
                kind: "query",
                query: row.get("query"),
                command_id: None,
                title: None,
                route: None,
                result_count,
                created_at,
            });
        }
    }

    Ok(Json(RecentResponse { items }))
}

/// Remember one search or command for the caller; repeating one moves it up instead of adding a
/// copy.
pub async fn record(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<RecordBody>,
) -> Result<StatusCode, ApiError> {
    let pool = state.db().pool();
    let kind = body.kind.trim().to_lowercase();

    match kind.as_str() {
        "query" => {
            let query = recent_query(body.query)?;
            let result_count = body.result_count.unwrap_or(0).max(0);

            sqlx::query(
                "insert into command_recents (user_id, organization_id, kind, query, result_count) \
                 values ($1, $2, 'query', $3, $4) \
                 on conflict (user_id, kind, query_key, command_key) do update set \
                 created_at = now(), result_count = excluded.result_count",
            )
            .bind(current.user.id)
            .bind(organization_id(&current))
            .bind(query)
            .bind(result_count as i32)
            .execute(pool)
            .await
            .map_err(store)?;
        }
        "command" => {
            let command_id = body.command_id.unwrap_or_default();
            let command_id = command_id.trim();
            let Some(spec) = commands::command(command_id) else {
                return Err(ApiError::bad_request(
                    "unknown_command",
                    format!("\"{command_id}\" is not a command of this platform"),
                ));
            };
            let permissions = caller_permissions(&state, &current).await?;
            if !spec.permission.is_none_or(|key| permissions.allows(key)) {
                return Err(ApiError::forbidden(
                    "command_not_allowed",
                    "this command needs a permission the caller does not hold",
                ));
            }

            sqlx::query(
                "insert into command_recents (user_id, organization_id, kind, command_id) \
                 values ($1, $2, 'command', $3) \
                 on conflict (user_id, kind, query_key, command_key) do update set \
                 created_at = now()",
            )
            .bind(current.user.id)
            .bind(organization_id(&current))
            .bind(spec.id)
            .execute(pool)
            .await
            .map_err(store)?;
        }
        _ => {
            return Err(ApiError::bad_request(
                "unknown_kind",
                "the recent must be a \"query\" or a \"command\"",
            ));
        }
    }

    // Keep the newest [`RECENT_KEEP`] rows and let go of the rest; the history is a convenience
    // and a convenience that grows without bound is a liability.
    sqlx::query(
        "delete from command_recents where user_id = $1 and id not in ( \
           select id from command_recents where user_id = $1 \
           order by created_at desc, id desc limit $2)",
    )
    .bind(current.user.id)
    .bind(RECENT_KEEP)
    .execute(pool)
    .await
    .map_err(store)?;

    Ok(StatusCode::NO_CONTENT)
}

/// Run one action command through its owning service.
///
/// The refusals are deliberate and each has its own code: an id nobody knows (`unknown_command`,
/// 404), a navigation command (`not_runnable`, 400 — it opens a screen, and a caller that wants
/// the screen says so by opening it), a command whose key the caller does not hold
/// (`command_not_allowed`, 403 — re-checked here even though the projection already filters, so
/// the endpoint is safe on its own), and a command that asks first without the caller's yes
/// (`confirmation_required`, 400).
///
/// An execution that starts is audited and counted whatever its outcome: the `command.run` entry
/// names actor, command and target, and `command_usage_daily` counts the run — a failed run is
/// still a run somebody asked for.
pub async fn run(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(id): Path<String>,
    Json(body): Json<RunBody>,
) -> Result<Json<RunResponse>, ApiError> {
    let id = id.trim();
    let Some(spec) = commands::runnable(id) else {
        // Two different refusals hide behind one lookup: an id nobody knows, and an id that is a
        // screen rather than a job.
        if commands::command(id).is_some() {
            return Err(ApiError::bad_request(
                "not_runnable",
                format!("\"{id}\" opens a screen; it is not a command that runs"),
            ));
        }
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown_command",
            format!("\"{id}\" is not a command of this platform"),
        ));
    };

    let permissions = caller_permissions(&state, &current).await?;
    if !spec.permission.is_none_or(|key| permissions.allows(key)) {
        return Err(ApiError::forbidden(
            "command_not_allowed",
            format!("\"{id}\" needs a permission the caller does not hold"),
        ));
    }
    if spec.confirm && !body.confirm {
        return Err(ApiError::bad_request(
            "confirmation_required",
            format!("\"{id}\" changes data that cannot be put back; confirm it to run it"),
        ));
    }

    let ip = address.as_text();
    let executed = execute(&state, &current, ip.clone(), spec).await;

    // The trail comes first: an executed action is not reported as successful without one.
    match &executed {
        Ok(action) => {
            record_run(&state, &current, ip, spec, "ok", action.result.clone()).await?;
        }
        Err(error) => {
            record_run(
                &state,
                &current,
                ip,
                spec,
                "failed",
                json!({ "code": error.code() }),
            )
            .await?;
        }
    }
    record_usage(&state, &current, spec.id).await?;

    let action = executed?;
    Ok(Json(RunResponse {
        command: spec.id,
        kind: spec.kind().as_str(),
        outcome: "ok",
        message: action.message,
        result: action.result,
    }))
}

/// Body of `POST /api/v1/command-center/resolve`.
///
/// One phrase, as the operator typed it. The box sends it after a short typing pause; the answer
/// is an interpretation, never an action.
#[derive(Debug, Deserialize)]
pub struct ResolveBody {
    /// The words typed into the palette.
    pub q: Option<String>,
}

/// One filter the interpretation carried, as the card prints it.
#[derive(Debug, Serialize)]
pub struct FilterBody {
    /// Stable key (`assignee`).
    pub key: &'static str,
    /// Label the card writes (`assignee`).
    pub label: &'static str,
    /// What was filtered on.
    pub value: String,
}

/// The interpreted intent, in the pieces the panel renders.
#[derive(Debug, Serialize)]
pub struct IntentBody {
    /// `search`, `command` or `unclear`.
    pub kind: &'static str,
    /// The domain word the reader used, if any.
    pub entity: Option<String>,
    /// The provider the domain maps to, when the index answers for it.
    pub provider: Option<&'static str>,
    /// The registry id a command interpretation named.
    pub command_id: Option<&'static str>,
    /// The words a search would carry.
    pub query: String,
    /// Filters the phrase carried.
    pub filters: Vec<FilterBody>,
    /// `newest`, `oldest` or `title`.
    pub sort: Option<&'static str>,
    /// How many results the phrase asked for.
    pub limit: Option<u32>,
    /// Confidence the reading carries, `0.0`–`0.95`.
    pub confidence: f32,
}

/// One thing the words could have meant instead.
#[derive(Debug, Serialize)]
pub struct AlternativeBody {
    /// The line the card prints.
    pub label: String,
    /// `search` or `command` — never an action.
    pub kind: &'static str,
    /// Where activating it goes.
    pub route: String,
    /// Provider a search alternative narrows to.
    pub provider: Option<&'static str>,
    /// Registry id a command alternative names.
    pub command_id: Option<&'static str>,
    /// How well the words matched.
    pub confidence: f32,
}

/// Answer of `POST /api/v1/command-center/resolve`.
///
/// The panel renders exactly this and nothing it invented: the preview line, the two ways to act
/// (`route` for a runnable reading, `command_id` when the platform's own run endpoint executes it)
/// and the alternatives. `source` and `degraded` say where the reading came from, so a local
/// reading is never dressed up as a model's.
#[derive(Debug, Serialize)]
pub struct ResolveResponse {
    /// The phrase as it was read (trimmed and capped).
    pub query: String,
    /// `local` (the platform's grammar) or `model` (an AI Hub model read it).
    pub source: &'static str,
    /// `true` when a model was configured but could not answer in time.
    pub degraded: bool,
    /// A caveat in plain words, when the reading needs one.
    pub note: Option<String>,
    /// The intent in plain words ("Tickets · assignee: Mehmet · last 10 · newest first").
    pub preview_text: String,
    /// `true` when the reading points somewhere the caller may really open.
    pub runnable: bool,
    /// Where a runnable reading lands, when it opens a screen.
    pub route: Option<String>,
    /// Where `Edit as search` goes: the same words, the same filters, runnable or not. A reading
    /// the caller may not run still has an editable shape, and the operator can fix it by hand.
    pub search_route: Option<String>,
    /// Confidence of the reading.
    pub confidence: f32,
    /// What was understood.
    pub intent: IntentBody,
    /// What else the words could have meant.
    pub alternatives: Vec<AlternativeBody>,
    /// `provider/model` that read the phrase, when a model did.
    pub model: Option<String>,
}

impl From<crate::intent_resolver::Resolution> for ResolveResponse {
    fn from(resolution: crate::intent_resolver::Resolution) -> Self {
        let intent = resolution.intent;
        let search_route = if intent.entity.is_some() || !intent.query.trim().is_empty() {
            Some(intent.search_url())
        } else {
            None
        };
        Self {
            query: intent.query.clone(),
            source: resolution.source,
            degraded: resolution.degraded,
            note: resolution.note,
            preview_text: intent.preview(),
            runnable: resolution.runnable,
            route: resolution.route,
            search_route,
            confidence: intent.confidence,
            intent: IntentBody {
                kind: intent.kind.as_str(),
                entity: intent.entity.clone(),
                provider: intent.provider,
                command_id: intent.command_id,
                query: intent.query.clone(),
                filters: intent
                    .filters
                    .iter()
                    .map(|filter| FilterBody {
                        key: filter.key,
                        label: filter.label,
                        value: filter.value.clone(),
                    })
                    .collect(),
                sort: intent.sort,
                limit: intent.limit,
                confidence: intent.confidence,
            },
            alternatives: resolution
                .alternatives
                .into_iter()
                .map(|alternative| AlternativeBody {
                    label: alternative.label,
                    kind: alternative.kind,
                    route: alternative.route,
                    provider: alternative.provider,
                    command_id: alternative.command_id,
                    confidence: alternative.confidence,
                })
                .collect(),
            model: resolution.model,
        }
    }
}

/// One interpreted intent. Nothing here runs: the panel shows what was understood, and the
/// operator's `Run` goes through the same permission-checked path a manual command does.
pub async fn resolve(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<ResolveBody>,
) -> Result<Json<ResolveResponse>, ApiError> {
    let raw = body.q.unwrap_or_default();
    let query = raw.trim();
    if query.is_empty() {
        return Err(ApiError::bad_request(
            "query_required",
            "the resolve endpoint needs the words that were typed",
        ));
    }
    if query.chars().count() > omnion_search::intent::MAX_QUERY_CHARS {
        return Err(ApiError::bad_request(
            "query_too_long",
            format!(
                "the phrase is capped at {} characters",
                omnion_search::intent::MAX_QUERY_CHARS
            ),
        ));
    }

    let permissions = caller_permissions(&state, &current).await?;
    let resolution = crate::intent_resolver::resolve(&state, &current, query, &permissions).await?;
    Ok(Json(ResolveResponse::from(resolution)))
}

/// Forget everything the caller did in the palette.
pub async fn clear(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<StatusCode, ApiError> {
    clear_recents(state.db().pool(), current.user.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// The action runner
// ---------------------------------------------------------------------------------------------

/// Execute one action command through the code its owning screen also uses.
///
/// The mapping is deliberately explicit — a command registered as an action without a service
/// behind it answers `action_not_implemented` rather than pretending to work, and the suite walks
/// the registry to keep the two in step.
async fn execute(
    state: &AppState,
    current: &CurrentSession,
    ip_address: Option<String>,
    spec: &'static CommandSpec,
) -> Result<ActionResult, ApiError> {
    match spec.id {
        "act.reindex-search" => {
            let reports =
                crate::routes::search::perform_reindex(state, current.user.id, ip_address, None)
                    .await?;

            let indexed: u64 = reports.iter().map(|report| report.indexed).sum();
            let pruned: u64 = reports.iter().map(|report| report.pruned).sum();
            let duration_ms: u64 = reports.iter().map(|report| report.duration_ms).sum();
            let mut message = if reports.len() == 1 {
                format!(
                    "Rebuilt \"{}\" · {} documents · {} ms",
                    reports[0].provider, indexed, duration_ms
                )
            } else {
                format!(
                    "Rebuilt {} providers · {} documents · {} ms",
                    reports.len(),
                    indexed,
                    duration_ms
                )
            };
            if pruned > 0 {
                message.push_str(&format!(" · {pruned} pruned"));
            }

            Ok(ActionResult {
                message,
                result: json!(reports),
            })
        }
        "act.clear-recents" => {
            let cleared = clear_recents(state.db().pool(), current.user.id).await?;
            Ok(ActionResult {
                message: if cleared == 1 {
                    "Cleared the palette history — 1 entry forgotten.".to_owned()
                } else {
                    format!("Cleared the palette history — {cleared} entries forgotten.")
                },
                result: json!({ "cleared": cleared }),
            })
        }
        other => Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "action_not_implemented",
            format!("the command \"{other}\" is registered as an action but no service runs it"),
        )),
    }
}

/// Write the `command.run` audit entry: actor, command (the target), outcome and the owning
/// service's own aggregate result — never a row of content.
async fn record_run(
    state: &AppState,
    current: &CurrentSession,
    ip_address: Option<String>,
    spec: &'static CommandSpec,
    outcome: &'static str,
    result: serde_json::Value,
) -> Result<(), ApiError> {
    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "command.run")
            .organization(organization_id(current))
            .target("command", spec.id)
            .metadata(json!({ "outcome": outcome, "result": result }))
            .ip_address(ip_address),
    )
    .await?;
    Ok(())
}

/// Count the run for today: adoption is a number, and the table stores no query text.
async fn record_usage(
    state: &AppState,
    current: &CurrentSession,
    command_id: &str,
) -> Result<(), ApiError> {
    sqlx::query(
        "insert into command_usage_daily (user_id, organization_id, command_id, day, runs) \
         values ($1, $2, $3, current_date, 1) \
         on conflict (user_id, command_id, day) do update \
         set runs = command_usage_daily.runs + 1",
    )
    .bind(current.user.id)
    .bind(organization_id(current))
    .bind(command_id)
    .execute(state.db().pool())
    .await
    .map_err(store)?;
    Ok(())
}

/// Forget every row the caller remembers; the owning service of both the palette's Clear control
/// and the `act.clear-recents` command, so the two cannot drift.
async fn clear_recents(pool: &sqlx::PgPool, user_id: Uuid) -> Result<u64, ApiError> {
    let result = sqlx::query("delete from command_recents where user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .map_err(store)?;
    Ok(result.rows_affected())
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The text of a `query` recent: trimmed, refused when empty and refused when it would not fit
/// the column — the caller gets a `400` with a reason instead of a `500` from a database check.
fn recent_query(raw: Option<String>) -> Result<String, ApiError> {
    let query = raw.unwrap_or_default();
    let query = query.trim();
    if query.is_empty() {
        return Err(ApiError::bad_request(
            "query_required",
            "a query recent needs the text that was typed",
        ));
    }
    if query.chars().count() > MAX_QUERY_CHARS {
        return Err(ApiError::bad_request(
            "query_too_long",
            format!("the remembered query is capped at {MAX_QUERY_CHARS} characters"),
        ));
    }
    Ok(query.to_owned())
}

/// The caller's effective permissions, resolved once per request.
async fn caller_permissions(
    state: &AppState,
    current: &CurrentSession,
) -> Result<omnion_permissions::EffectivePermissions, ApiError> {
    Ok(effective_permissions(state.db().pool(), current.user.id, scope_of(&current.user)).await?)
}

/// Organization a personal row is filed under: the caller's own, or the platform level (`null`),
/// exactly as the audit log files the same accounts.
fn organization_id(current: &CurrentSession) -> Option<Uuid> {
    current.user.organization_id
}

/// Database failures become a `500`/`503` with the store's own code — the same mapping the rest
/// of the API uses for `sqlx` errors.
fn store(error: sqlx::Error) -> ApiError {
    omnion_search::SearchError::Store(error).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_body_carries_everything_the_palette_renders() {
        let spec = commands::command("nav.create-page").expect("registered");
        let body = CommandBody::from(spec);
        assert_eq!(body.id, "nav.create-page");
        assert_eq!(body.route, "/pages?new=1");
        assert_eq!(body.permission, Some("content.pages.create"));
        assert_eq!(body.group, "Content");
    }

    #[test]
    fn a_query_recent_is_trimmed_and_refused_when_it_is_unusable() {
        assert_eq!(
            recent_query(Some("  release notes  ".to_owned())).expect("a usable query"),
            "release notes"
        );

        let empty = recent_query(Some("   ".to_owned())).expect_err("whitespace is not a query");
        assert_eq!(empty.code(), "query_required");
        assert_eq!(
            recent_query(None).expect_err("a missing query").code(),
            "query_required"
        );

        let long = recent_query(Some("x".repeat(MAX_QUERY_CHARS + 1)))
            .expect_err("one character past the cap is refused");
        assert_eq!(long.code(), "query_too_long");

        // The cap is counted in characters, not bytes: a query of multi-byte letters that fits
        // the column must not be refused.
        let accented =
            recent_query(Some("é".repeat(MAX_QUERY_CHARS))).expect("the cap is inclusive");
        assert_eq!(accented.chars().count(), MAX_QUERY_CHARS);
    }
}
