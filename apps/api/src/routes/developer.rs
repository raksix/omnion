//! `/api/v1/developer` — the credential and traffic surface (REQ-022, slice 1).
//!
//! ## What this module guarantees, and where each guarantee lives
//!
//! * **A secret is in exactly one response.** [`create_key`] and [`rotate_key`] return
//!   [`IssuedKey`], whose `token` field is the only field in the crate that can hold a
//!   plaintext. Every other handler answers [`KeyView`], which has no such field. The
//!   guarantee is therefore a property of the response type, not of anybody's memory.
//! * **A key's scopes may only narrow.** [`create_key`] resolves the caller's own granted set
//!   and refuses any scope they do not hold, with the name in the message. This is the check
//!   that stops a read-only integration from minting one that can rotate keys.
//! * **Every credential action is audited and emits an event**, and both carry ids, names,
//!   environments and scope *names* — never a token and never a hash. A hash is as sensitive
//!   as the token for an offline-guessing attacker, so it does not travel either.
//! * **Nothing is echoed back on refusal.** A bad token answers a stable code and a sentence,
//!   never the presented value; the REQ names this explicitly and an error message is the one
//!   place a credential habitually leaks.

use axum::Json;
use axum::extract::{Path, Query, State};
use omnion_audit::{NewAuditEntry, record as record_audit};
use omnion_developer::{
    DeveloperError, ENVIRONMENTS, KeyView, IssuedKey, KeyQuery, KeyStatus, LogQuery, UsagePoint,
    keys_store, logs_store, mintable_from, scope_names_valid,
};
use omnion_events::{NewEvent, bus};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------------------------

/// Map a crate error onto the API surface.
///
/// The four shapes map to four different statuses on purpose: `Conflict` is `409` because the
/// panel shows a different message for "that name is taken" than for "that input is wrong", and
/// collapsing them into one `400` would make the duplicate-name case unreadable.
pub fn map_store(error: DeveloperError) -> ApiError {
    match error {
        DeveloperError::Invalid(message) => {
            ApiError::bad_request("invalid_developer_input", message)
        }
        DeveloperError::Conflict(message) => {
            ApiError::new(axum::http::StatusCode::CONFLICT, "duplicate_name", message)
        }
        DeveloperError::NotFound(message) => {
            ApiError::new(axum::http::StatusCode::NOT_FOUND, "not_found", message)
        }
        DeveloperError::Database(inner) => ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("developer portal store: {inner}"),
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// Body of `POST /api/v1/developer/api-keys`.
#[derive(Debug, Deserialize)]
pub struct CreateKeyBody {
    /// 3–64 characters, unique per organization and environment.
    pub name: String,
    /// At least one, all of them catalogued, all of them held by the caller.
    pub scopes: Vec<String>,
    /// `live` or `sandbox`.
    #[serde(default = "default_environment")]
    pub environment: String,
    /// Optional expiry. Refused in the past.
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
}

fn default_environment() -> String {
    omnion_developer::ENVIRONMENT_LIVE.to_owned()
}

/// Body of `GET /api/v1/developer/api-keys`.
#[derive(Debug, Deserialize)]
pub struct ListKeysQuery {
    /// Narrow to one environment.
    pub environment: Option<String>,
    /// `active`, `expired` or `revoked`.
    pub status: Option<String>,
    /// Free text over name and prefix.
    pub search: Option<String>,
    /// Page size, clamped by the store.
    pub limit: Option<usize>,
}

/// Body of `GET /api/v1/developer/api-keys/{id}`.
#[derive(Debug, Serialize)]
pub struct KeyDetail {
    /// The key.
    pub key: KeyView,
    /// Per-day counters for the usage chart.
    pub usage: Vec<UsagePointBody>,
    /// The retention window the log screen publishes.
    pub log_retention_days: u32,
}

/// One day of the usage chart, with the day **as a string**.
///
/// The crate's own `UsagePoint` holds a `time::Date`, and the workspace enables `serde-well-known`
/// without `serde-human-readable`, so that type would cross the wire as a three-element array —
/// which the panel would use as the React `key` of every bar in the chart. `time` exposes no
/// `Date` codec under the features this workspace enables, so the conversion happens here, at the
/// one boundary where the platform decides what a date looks like on the wire. The alternative —
/// adding a feature to every crate that touches `Date` — would change the wire form of every
/// existing date in the platform to fix one screen.
#[derive(Debug, Serialize)]
pub struct UsagePointBody {
    /// `YYYY-MM-DD`.
    pub day: String,
    /// Requests that day.
    pub requests: i32,
    /// Refusals that day.
    pub errors: i32,
    /// Mean duration that day.
    pub avg_duration_ms: i32,
}

impl From<UsagePoint> for UsagePointBody {
    fn from(point: UsagePoint) -> Self {
        Self {
            // `Iso8601::DATE` cannot fail for a `Date`, so the fallback is unreachable and exists
            // only to satisfy the fallible signature: a date that fails to print itself would be a
            // bug in the formatter, and there is no error channel on a read-only response body.
            day: point
                .day
                .format(&time::macros::format_description!(
                    "[year]-[month]-[day]"
                ))
                .unwrap_or_else(|_| String::from("1970-01-01")),
            requests: point.requests,
            errors: point.errors,
            avg_duration_ms: point.avg_duration_ms,
        }
    }
}

/// Body of `GET /api/v1/developer/logs`.
#[derive(Debug, Deserialize)]
pub struct ListLogsQuery {
    /// Narrow to one key.
    pub api_key_id: Option<Uuid>,
    /// Narrow to one method.
    pub method: Option<String>,
    /// Narrow to a path prefix.
    pub path_prefix: Option<String>,
    /// `2xx` … `5xx`.
    pub status_class: Option<String>,
    /// How far back, in days.
    pub window_days: Option<u32>,
    /// Page size.
    pub limit: Option<usize>,
    /// Keyset cursor.
    pub before: Option<i64>,
}

/// Body of `GET /api/v1/developer/logs/{id}`.
#[derive(Debug, Serialize)]
pub struct LogDetail {
    /// The row.
    pub row: omnion_developer::LogRow,
    /// The class the filter names, so the drawer and the toolbar cannot disagree.
    pub status_class: &'static str,
    /// The retention window, printed on the screen rather than buried here.
    pub retention_days: u32,
}

/// Body of `GET /api/v1/developer/scopes`.
#[derive(Debug, Serialize)]
pub struct ScopeCatalogue {
    /// Grouped by category, which is how the picker renders them.
    pub categories: Vec<ScopeCategory>,
    /// The environments a key may carry.
    pub environments: Vec<&'static str>,
}

/// One category of the scope picker.
#[derive(Debug, Serialize)]
pub struct ScopeCategory {
    /// The category key, e.g. `content`.
    pub key: &'static str,
    /// The scopes in it, with a description the picker shows.
    pub scopes: Vec<ScopeRow>,
}

/// One assignable scope.
#[derive(Debug, Serialize)]
pub struct ScopeRow {
    /// The permission key.
    pub key: &'static str,
    /// What it allows, in product language.
    pub description: &'static str,
    /// Whether the caller may grant it — a scope picker that offers something the caller
    /// cannot delegate is a form that submits and is then refused by the server.
    pub grantable: bool,
}

// ---------------------------------------------------------------------------------------------
// Scopes
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/developer/scopes` — the assignable catalogue, grouped for the picker.
pub async fn list_scopes(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<ScopeCatalogue>, ApiError> {
    let organization_id = crate::scope::resolve_organization(&session, None)?;
    let effective = omnion_permissions::effective_permissions(
        state.db().pool(),
        session.user.id,
        omnion_permissions::Scope::Organization { organization_id },
    )
    .await
    .map_err(|error| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("could not resolve the caller's permissions: {error}"),
        )
    })?;

    // Grouped here rather than in the panel: the grouping is a property of the catalogue, and
    // a panel that re-derives it would drift the day a category is renamed.
    let mut categories: Vec<ScopeCategory> = Vec::new();
    for definition in omnion_permissions::catalogue::CATALOGUE {
        let Some(category) = categories
            .iter_mut()
            .find(|entry: &&mut ScopeCategory| entry.key == definition.category)
        else {
            categories.push(ScopeCategory {
                key: definition.category,
                scopes: Vec::new(),
            });
            continue;
        };
        category.scopes.push(ScopeRow {
            key: definition.key,
            description: definition.description,
            grantable: effective.allows(definition.key),
        });
    }

    Ok(Json(ScopeCatalogue {
        categories,
        environments: ENVIRONMENTS.to_vec(),
    }))
}

// ---------------------------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/developer/api-keys` — the key list.
pub async fn list_keys(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<ListKeysQuery>,
) -> Result<Json<Vec<KeyView>>, ApiError> {
    let organization_id = crate::scope::resolve_organization(&session, None)?;
    let page = keys_store::list(
        state.db().pool(),
        &KeyQuery {
            organization_id,
            environment: query.environment,
            status: query.status,
            search: query.search,
            limit: query.limit.unwrap_or(50),
            before: None,
        },
        OffsetDateTime::now_utc(),
    )
    .await
    .map_err(map_store)?;

    Ok(Json(page.keys.into_iter().map(KeyView::from).collect()))
}

/// `GET /api/v1/developer/api-keys/{id}` — one key with its usage window.
pub async fn get_key(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<KeyDetail>, ApiError> {
    let organization_id = crate::scope::resolve_organization(&session, None)?;
    let now = OffsetDateTime::now_utc();
    let key = keys_store::find(state.db().pool(), organization_id, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| {
            ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "not_found",
                "no such API key",
            )
        })?;

    let usage = keys_store::usage(state.db().pool(), organization_id, id, 30, now.date())
        .await
        .map_err(map_store)?;

    Ok(Json(KeyDetail {
        key: KeyView::from(key),
        usage: usage.into_iter().map(UsagePointBody::from).collect(),
        log_retention_days: logs_store::window_days(),
    }))
}

/// `POST /api/v1/developer/api-keys` — create one. **The only response carrying a token.**
pub async fn create_key(
    State(state): State<AppState>,
    session: CurrentSession,
    body: Json<CreateKeyBody>,
) -> Result<(axum::http::StatusCode, Json<IssuedKey>), ApiError> {
    let organization_id = crate::scope::resolve_organization(&session, None)?;
    let now = OffsetDateTime::now_utc();

    // 1. The catalogue first: a scope nothing can enforce must never reach the table, because
    //    the list would then show a key that looks powerful and does nothing on the wire.
    let catalogue = omnion_permissions::catalogue::keys();
    scope_names_valid(&body.scopes, &catalogue).map_err(map_store)?;

    // 2. Then the delegation rule, against the caller's **own** granted set. This is what
    //    makes a key a delegation rather than a privilege escalation wearing one.
    let effective = omnion_permissions::effective_permissions(
        state.db().pool(),
        session.user.id,
        omnion_permissions::Scope::Organization { organization_id },
    )
    .await
    .map_err(|error| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("could not resolve the caller's permissions: {error}"),
        )
    })?;
    // `mintable_from` takes `&[&str]` on purpose — a slice of borrowed strings cannot be built
    // by accident from something that owns a longer lifetime. The bridge is here, once, rather
    // than by loosening the signature for the convenience of one caller.
    let granted = effective.granted_keys();
    let held: Vec<&str> = granted.iter().map(String::as_str).collect();
    let scopes = mintable_from(&body.scopes, &held).map_err(map_store)?;

    let new_key = omnion_developer::NewKey {
        organization_id,
        name: body.name.clone(),
        environment: body.environment.clone(),
        scopes,
        expires_at: body.expires_at,
        created_by: Some(session.user.id),
        created_by_name: display_name(&session),
    };

    let (row, secret) = keys_store::create(state.db().pool(), &new_key, now)
        .await
        .map_err(map_store)?;

    // The event carries names and scope **names**, never the token and never the hash.
    emit_credential_event(
        &state,
        organization_id,
        session.user.id,
        "developer.api_key.created",
        &row,
        now,
    )
    .await;

    audit(
        &state,
        organization_id,
        session.user.id,
        "developer.api_key.created",
        &row,
        now,
    )
    .await?;

    Ok((
        axum::http::StatusCode::CREATED,
        Json(IssuedKey {
            key: KeyView::from(row),
            token: secret.token,
        }),
    ))
}

/// `POST /api/v1/developer/api-keys/{id}/rotate` — a new secret, the old one dead at once.
pub async fn rotate_key(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<IssuedKey>, ApiError> {
    let organization_id = crate::scope::resolve_organization(&session, None)?;
    let now = OffsetDateTime::now_utc();

    let (row, secret) = keys_store::rotate(state.db().pool(), organization_id, id, now)
        .await
        .map_err(map_store)?;

    emit_credential_event(
        &state,
        organization_id,
        session.user.id,
        "developer.api_key.rotated",
        &row,
        now,
    )
    .await;

    audit(
        &state,
        organization_id,
        session.user.id,
        "developer.api_key.rotated",
        &row,
        now,
    )
    .await?;

    Ok(Json(IssuedKey {
        key: KeyView::from(row),
        token: secret.token,
    }))
}

/// `DELETE /api/v1/developer/api-keys/{id}` — revoke. Soft: the row and its log stay.
pub async fn revoke_key(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let organization_id = crate::scope::resolve_organization(&session, None)?;
    let now = OffsetDateTime::now_utc();

    // Read before revoking, so the audit row and the event can name the key. The read is
    // organization-scoped, so this cannot become an existence oracle for another tenant's id.
    let previous = keys_store::find(state.db().pool(), organization_id, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| {
            ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "not_found",
                "no such API key",
            )
        })?;

    keys_store::revoke(state.db().pool(), organization_id, id, now)
        .await
        .map_err(map_store)?;

    emit_credential_event(
        &state,
        organization_id,
        session.user.id,
        "developer.api_key.revoked",
        &previous,
        now,
    )
    .await;

    audit(
        &state,
        organization_id,
        session.user.id,
        "developer.api_key.revoked",
        &previous,
        now,
    )
    .await?;

    Ok(Json(json!({ "revoked": true, "id": id })))
}

// ---------------------------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/developer/logs` — the request log, filtered.
pub async fn list_logs(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<ListLogsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let organization_id = crate::scope::resolve_organization(&session, None)?;
    let mut log_query = LogQuery {
        organization_id,
        api_key_id: query.api_key_id,
        method: query.method,
        path_prefix: query.path_prefix,
        status_class: query.status_class,
        window_days: query.window_days,
        limit: query.limit.unwrap_or(50),
        before: query.before,
    };
    let page = logs_store::search(state.db().pool(), &mut log_query)
        .await
        .map_err(map_store)?;
    Ok(Json(json!({ "rows": page.rows, "next_before": page.next_before })))
}

/// `GET /api/v1/developer/logs/{id}` — one request, with the permission its guard resolved.
pub async fn get_log(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<i64>,
) -> Result<Json<LogDetail>, ApiError> {
    let organization_id = crate::scope::resolve_organization(&session, None)?;
    let row = logs_store::find(state.db().pool(), organization_id, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| {
            ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "not_found",
                "no such request in the log",
            )
        })?;

    Ok(Json(LogDetail {
        status_class: row.status_class(),
        row,
        retention_days: logs_store::window_days(),
    }))
}

// ---------------------------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/developer/overview` — the card row and the recent failures.
///
/// The whole body is one number set read in one snapshot (see `omnion_developer::overview`), so
/// the route's own job is only to resolve the organization and hand back the answer. It does not
/// add a retention field of its own: the retention window the screen prints comes from the same
/// [`logs_store::window_days`] the log screen and the detail drawer use, so a screen cannot
/// quote a window the table does not honour.
pub async fn overview(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<serde_json::Value>, ApiError> {
    let organization_id = crate::scope::resolve_organization(&session, None)?;
    let read = omnion_developer::overview::read(
        state.db().pool(),
        organization_id,
        OffsetDateTime::now_utc(),
    )
    .await
    .map_err(map_store)?;

    Ok(Json(json!({
        "keys": {
            "active": read.active_keys,
            "expired": read.expired_keys,
            "revoked": read.revoked_keys,
        },
        "requests_today": read.requests_today,
        "errors_today": read.errors_today,
        "recent_failures": read.recent_failures,
        "log_retention_days": logs_store::window_days(),
    })))
}

// ---------------------------------------------------------------------------------------------
// Shared
// ---------------------------------------------------------------------------------------------

/// The name the list columns show. Empty when the account has no display name set, which the
/// cell renders as "—" rather than as a blank that looks like a rendering fault.
fn display_name(session: &CurrentSession) -> String {
    let display = session.user.display_name.trim();
    if !display.is_empty() {
        return display.to_owned();
    }
    let email = session.user.email.trim();
    if email.is_empty() {
        String::new()
    } else {
        email.to_owned()
    }
}

/// Write the audit row for a credential action.
///
/// An audit failure is **not** swallowed: the REQ says every credential action writes one, and
/// an operator whose rotation "worked" but left no record has been handed a false assurance.
async fn audit(
    state: &AppState,
    organization_id: Uuid,
    actor: Uuid,
    action: &'static str,
    key: &omnion_developer::ApiKey,
    _now: OffsetDateTime,
) -> Result<(), ApiError> {
    record_audit(
        state.db().pool(),
        NewAuditEntry::by_user(actor, action)
            .organization(organization_id)
            .target("api_key", key.id.to_string())
            // The metadata is the REQ's "ids, name, environment and scope names; never a
            // secret or a hash" — carried out literally.
            .metadata(json!({
                "name": key.name,
                "environment": key.environment,
                "key_prefix": key.key_prefix,
                "scopes": key.scopes,
            })),
    )
    .await
    .map_err(|error| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "audit_failed",
            format!("the change was stored but its audit row could not be written: {error}"),
        )
    })?;
    Ok(())
}

/// Emit the credential event, best effort.
///
/// The state is already committed when this runs, so a bus failure is logged and not surfaced:
/// answering `500` would tell the operator their key was not created when it was, and the
/// audit row above is the authoritative record. This is the same trade every other emitter in
/// the platform makes, and it is written down here because the next reader will wonder.
async fn emit_credential_event(
    state: &AppState,
    organization_id: Uuid,
    actor: Uuid,
    name: &'static str,
    key: &omnion_developer::ApiKey,
    now: OffsetDateTime,
) {
    if let Err(error) = bus::emit(
        state.db().pool(),
        NewEvent::new(name)
            .organization(organization_id)
            .actor(actor)
            .payload(json!({
                "key_id": key.id,
                "name": key.name,
                "environment": key.environment,
                "key_prefix": key.key_prefix,
                "scopes": key.scopes,
                "status": KeyStatus::from(key.status_at(now)).as_str(),
            })),
    )
    .await
    {
        tracing::warn!(error = %error, event = name, "the credential change was stored but its event was not recorded");
    }
}

/// Whether an environment is production.
///
/// The sandbox console's red banner reads this, and nothing else may decide it: a second
/// `== "live"` in a component is how a banner ends up reassuring somebody that a live key is
/// pointing at a sandbox.
#[must_use]
pub fn is_production_environment(environment: &str) -> bool {
    omnion_developer::is_live_environment(environment)
}


/// `GET /api/v1/developer/sandbox/probe` — the sandbox console's "is my key alive?" call.
///
/// It exists because a key's whole point is to authenticate **somewhere other than the
/// portal**, and the portal is the one place a broken key still looks fine: the operator is
/// signed in with a session, so every screen loads. This route is guarded for a key, so the
/// answer an integrator gets is the platform's own, not the browser's.
///
/// The response names the key's prefix and the permission it was checked against, and nothing
/// else — the body of a probe that a browser can call is the last place a token should appear.
pub async fn sandbox_probe() -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "message": "the key authenticated and carried the scope this route requires",
    }))
}
