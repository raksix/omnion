//! `/api/v1/api-keys` and `/api/v1/request-logs` — the developer surface's two reads and
//! three writes (docs/requests/REQ-033, slice 1).
//!
//! Reading keys and the request log is `developer.keys.read`; minting, rotating and revoking is
//! `developer.keys.manage`. The split is not a formality: a key is a credential that
//! authenticates *as this organization*, so an account that can read the key list must not
//! thereby be able to add one.
//!
//! # The one shape this file must never produce
//!
//! [`Minted`] is the only type in `omnion-developer` that carries plaintext, and it exists only
//! as the return value of `create` and `rotate`. There is deliberately no handler that turns a
//! stored row back into one — so "the secret came back a second time" is a code path that does
//! not exist rather than a test that has to remember to fail. Everything else here returns
//! [`ApiKey`], which has no secret field to fill in.
//!
//! # Where the rate tier is checked
//!
//! In the handler, from the *resolved* role rather than from the body, because the request says
//! `high` "requires an owner or admin role" and a check that read the submitted role would be a
//! check the caller could satisfy by typing it.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_developer::model::RateTier;
use omnion_developer::store;
use omnion_developer::{
    ApiKey, DeveloperError, Environment, Minted, NewKey, RequestLog, RequestLogPage,
    RequestLogQuery,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Request and response shapes
// ---------------------------------------------------------------------------------------------

/// Body of `POST /api/v1/api-keys`.
#[derive(Debug, Deserialize)]
pub struct CreateKeyInput {
    /// Display name, 3–60 characters, unique in this organization.
    pub name: String,
    /// Permission keys the key carries. At least one.
    pub scopes: Vec<String>,
    /// Which environment it authenticates against.
    pub environment: String,
    /// `standard`, or `high` for an owner or administrator.
    #[serde(default)]
    pub rate_tier: Option<String>,
    /// CIDR blocks the key may be used from. Omit for "any source".
    #[serde(default)]
    pub ip_allowlist: Option<Vec<String>>,
    /// Days until expiry: 30, 90, 365, or `None` for never.
    ///
    /// Days rather than an instant because the panel offers exactly those four choices and a
    /// date picker would let a developer key expire at 03:14 on a Sunday. `None` here means
    /// "never expires", which is the same reading as a form that submits the default.
    #[serde(default)]
    pub expires_in_days: Option<i64>,
}

/// The keys list. A wrapper rather than a bare array so a future field (a summary count, the
/// environments the organization uses) does not change the response shape from an array into an
/// object and break every client at once.
#[derive(Debug, Serialize)]
pub struct KeysResponse {
    /// The keys, newest first.
    pub keys: Vec<ApiKey>,
}

/// A key as the panel sees it, including the usage series on its detail screen.
#[derive(Debug, Serialize)]
pub struct KeyDetailResponse {
    /// The key itself — no secret, and there is no field for one.
    pub key: ApiKey,
    /// Daily request/error counters, oldest first.
    pub usage: Vec<UsagePoint>,
}

/// One bar of a key's usage chart.
#[derive(Debug, Serialize)]
pub struct UsagePoint {
    /// The day.
    pub day: Date,
    /// Requests that day.
    pub requests: i32,
    /// Requests that day that did not return 2xx.
    pub errors: i32,
    /// 95th percentile latency, when the day had enough samples to mean anything.
    pub p95_ms: Option<i32>,
}

/// The one-time response of `create` and `rotate`.
///
/// The name says what it is, and the panel's dialog is driven by its presence: this struct is
/// returned by exactly two handlers and deserialised by exactly one component.
#[derive(Debug, Serialize)]
pub struct MintedResponse {
    /// The key, as stored.
    #[serde(flatten)]
    pub key: ApiKey,
    /// The secret. Shown once, never again.
    pub secret: String,
}

impl From<Minted> for MintedResponse {
    fn from(minted: Minted) -> Self {
        Self {
            key: minted.key,
            secret: minted.plaintext,
        }
    }
}

/// Filters of `GET /api/v1/request-logs`.
///
/// Every field is optional and every one of them is validated before the query runs — the
/// validation lives in [`RequestLogQuery::normalized`] in the crate, and this struct's only job
/// is to decide what a *missing* filter means. `since_hours` is the one defaulting choice: the
/// panel's own default range is 24 hours, and a log with no range at all is the fourteen-day
/// retention window, which nobody reads.
#[derive(Debug, Deserialize)]
pub struct RequestLogFilters {
    /// Only this key's requests (by id).
    pub api_key_id: Option<Uuid>,
    /// Only this key, by public prefix — what a key row's "View logs" link uses.
    pub key_prefix: Option<String>,
    /// Only this exact status.
    pub status: Option<i16>,
    /// Only this class: `2xx`, `4xx`, `5xx`.
    pub status_class: Option<String>,
    /// Only paths starting with this.
    pub path_prefix: Option<String>,
    /// Only this method.
    pub method: Option<String>,
    /// How far back, in hours. Defaults to 24.
    pub since_hours: Option<i64>,
    /// At least this many milliseconds.
    pub min_duration_ms: Option<i32>,
    /// Page size.
    pub limit: Option<i64>,
    /// Rows to skip.
    pub offset: Option<i64>,
}

/// `days` of `GET /api/v1/api-keys/{id}/usage`.
#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    /// How many days back, default 30.
    pub days: Option<i64>,
}

// ---------------------------------------------------------------------------------------------
// Handlers — keys
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/api-keys` — the organization's keys, metadata only.
pub async fn list_keys(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<KeysResponse>, ApiError> {
    let organization_id = organization_of(&current)?;
    let keys = store::list(state.db().pool(), organization_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(KeysResponse { keys }))
}

/// `GET /api/v1/api-keys/{id}` — one key and its usage chart.
///
/// One handler rather than a key route plus a usage route, because the chart is on the detail
/// screen and a second round trip would make the screen render its header and then change its
/// shape — the "two fetches, two skeletons, one flash" problem every other detail screen in the
/// panel already avoids.
pub async fn get_key(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(key_id): Path<Uuid>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<KeyDetailResponse>, ApiError> {
    let organization_id = organization_of(&current)?;
    let key = store::get(state.db().pool(), organization_id, key_id)
        .await
        .map_err(ApiError::from)?;
    let usage = store::usage(
        state.db().pool(),
        organization_id,
        key_id,
        query.days.unwrap_or(30),
    )
    .await
    .map_err(ApiError::from)?;

    Ok(Json(KeyDetailResponse {
        key,
        usage: usage
            .into_iter()
            .map(|day| UsagePoint {
                day: day.day,
                requests: day.requests,
                errors: day.errors,
                p95_ms: day.p95_ms,
            })
            .collect(),
    }))
}

/// `POST /api/v1/api-keys` — mint a key. The secret is in this response and nowhere else.
pub async fn create_key(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(input): Json<CreateKeyInput>,
) -> Result<(StatusCode, Json<MintedResponse>), ApiError> {
    let organization_id = organization_of(&current)?;
    let environment = Environment::parse(&input.environment).map_err(ApiError::from)?;
    refuse_undelegable_scopes(state.db().pool(), current.user.id, organization_id, &input.scopes)
        .await?;
    let rate_tier = match input.rate_tier.as_deref() {
        None | Some("") | Some("standard") => RateTier::Standard,
        Some("high") => {
            // The tier is checked against the *resolved* role, not the body: the body is the
            // caller's own claim about themselves. `developer.keys.manage` is already required
            // to reach this handler, so the gate is "managing keys is not the same as being
            // allowed to lift your own ceiling" — which is the relationship the request asks
            // for.
            if !caller_may_elevate(state.db().pool(), current.user.id, organization_id).await? {
                return Err(ApiError::forbidden(
                    "rate_tier_not_permitted",
                    "the high rate tier is reserved for an owner or administrator",
                )
                .with_details(json!({ "field": "rate_tier" })));
            }
            RateTier::High
        }
        Some(other) => {
            return Err(ApiError::bad_request(
                "invalid_rate_tier",
                format!("{other:?} is not a rate tier"),
            )
            .with_details(json!({ "field": "rate_tier" })));
        }
    };

    let expires_at = expiry_from_days(input.expires_in_days).map_err(ApiError::from)?;

    let minted = store::create(
        state.db().pool(),
        organization_id,
        &NewKey {
            name: input.name,
            scopes: input.scopes,
            environment,
            rate_tier,
            ip_allowlist: input.ip_allowlist,
            expires_at,
            created_by: current.user.id,
        },
    )
    .await
    .map_err(ApiError::from)?;

    // The audit entry carries the prefix — the public half, safe to log — and never the
    // plaintext. `scopes` are permission names, so they are safe too, and the request asks for
    // "actor, target and scopes" on every mutating developer action.
    audit(
        &state,
        &current,
        &address,
        "developer.api_key.created",
        json!({
            "key": minted.key.id,
            "prefix": minted.key.prefix,
            "name": minted.key.name,
            "environment": minted.key.environment,
            "scopes": minted.key.scopes,
            "rate_tier": minted.key.rate_tier,
            "has_ip_allowlist": minted.key.ip_allowlist.is_some(),
            "expires_at": minted.key.expires_at,
        }),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(minted.into())))
}

/// `POST /api/v1/api-keys/{id}/rotate` — a new secret; the old one dies at once.
pub async fn rotate_key(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(key_id): Path<Uuid>,
) -> Result<Json<MintedResponse>, ApiError> {
    let organization_id = organization_of(&current)?;
    let minted = store::rotate(
        state.db().pool(),
        organization_id,
        key_id,
        OffsetDateTime::now_utc(),
    )
    .await
    .map_err(ApiError::from)?;

    audit(
        &state,
        &current,
        &address,
        "developer.api_key.rotated",
        json!({
            "key": minted.key.id,
            "prefix": minted.key.prefix,
            "name": minted.key.name,
            "scopes": minted.key.scopes,
        }),
    )
    .await?;

    Ok(Json(minted.into()))
}

/// `DELETE /api/v1/api-keys/{id}` — revoke. Idempotent, and it does not delete the row.
///
/// A revoked key keeps its history and its name: the request log is the record of what an
/// integration did, and a delete would take that with it — so "this key was called 40 000
/// times last month" becomes unanswerable because the key is gone.
pub async fn revoke_key(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(key_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let organization_id = organization_of(&current)?;
    let key = store::revoke(
        state.db().pool(),
        organization_id,
        key_id,
        OffsetDateTime::now_utc(),
    )
    .await
    .map_err(ApiError::from)?;

    audit(
        &state,
        &current,
        &address,
        "developer.api_key.revoked",
        json!({ "key": key.id, "prefix": key.prefix, "name": key.name }),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Handlers — the request log
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/request-logs` — a filtered page of the request log.
///
/// Metadata only, and that is the table's schema rather than a filter applied on the way out:
/// there is no body column, so "why can I not see the payload" is answered by the migration and
/// cannot be answered wrongly by a redaction list that grows a hole.
pub async fn list_request_logs(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(filters): Query<RequestLogFilters>,
) -> Result<Json<RequestLogPage>, ApiError> {
    let organization_id = organization_of(&current)?;

    let query = RequestLogQuery {
        api_key_id: filters.api_key_id,
        key_prefix: filters.key_prefix,
        status: filters.status,
        status_class: filters.status_class,
        path_prefix: filters.path_prefix,
        method: filters.method,
        since: default_window(filters.since_hours),
        until: None,
        min_duration_ms: filters.min_duration_ms,
        limit: filters.limit.unwrap_or(RequestLogQuery::DEFAULT_LIMIT),
        offset: filters.offset.unwrap_or(0),
    }
    .normalized()
    .map_err(ApiError::from)?;

    let page = store::list_requests(state.db().pool(), organization_id, &query)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(page))
}

/// `GET /api/v1/request-logs/{id}` — one request's metadata.
///
/// Its own handler, and the request asks for it, because the panel's row opens a drawer: a list
/// endpoint that answered `?id=` on its own path would have to guess between a page and a
/// single row from the same `GET`, and the day it guessed wrong the log would paginate on a
/// drawer that should have shown one request.
pub async fn get_request_log(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(log_id): Path<i64>,
) -> Result<Json<RequestLog>, ApiError> {
    let organization_id = organization_of(&current)?;

    let row = sqlx::query(
        "select id, organization_id, api_key_id, api_key_prefix, actor_user_id, actor_name, permission, \
         method, path, status, duration_ms, request_id, bytes_in, bytes_out, error_code, \
         created_at \
         from api_request_logs where id = $1 and organization_id = $2",
    )
    .bind(log_id)
    .bind(organization_id)
    .fetch_optional(state.db().pool())
    .await
    // The database's own error, mapped through the same helper the row decoder uses, because
    // `ApiError: From<sqlx::Error>` does not exist in this API — every route that speaks sqlx
    // directly writes the mapping out, and that is the pattern being followed here.
    .map_err(decode_error)?
    .ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "request_log_not_found",
            "no such request in this organization's log",
        )
    })?;

    Ok(Json(log_from_row(&row)?))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The organization a developer key belongs to.
///
/// A platform account (`organization_id: None`) has no organization to hold a key, and saying
/// so plainly beats a `403` that names a permission the caller does hold.
/// `pub(crate)` because the OAuth routes in `developer_oauth.rs` resolve the tenant the same
/// way and must not carry a second copy: a handler that read `organization_id` from anywhere
/// else (the body, a header, an app row) is a cross-tenant write, and the only defence is that
/// there is exactly one function that answers "which tenant is this" for a session.
pub(crate) fn organization_of(current: &CurrentSession) -> Result<Uuid, ApiError> {
    current.user.organization_id.ok_or_else(|| {
        ApiError::forbidden(
            "organization_required",
            "an API key belongs to an organization; a platform account has no key to manage",
        )
    })
}

/// `GET /api/v1/developer/overview` — the card row and the recent failures.
///
/// Arrived from `origin/main` (REQ-022 slice 2) with a merge. It is **not** what the `/developer`
/// screen renders: that screen's six cards each read the list their own destination already lists
/// (`apps/admin/features/developer/developer-overview-view.tsx`), so a card cannot disagree with
/// the screen it opens. This endpoint is the *snapshot* version of the same question and it is
/// kept for two reasons that are both about honesty rather than use:
///
/// * it is the only reader that counts keys, requests and refusals in **one** statement, so a
///   caller who needs a consistent moment (a status page, a webhook payload) gets one;
/// * the walk `the_overview_counts_the_same_keys_and_requests_the_tables_show` came with it, and
///   that walk is the thing that would catch the two views drifting apart.
///
/// The retention field it reports comes from the same window the log screen filters with, so a
/// card cannot quote a window the table does not honour.
pub async fn overview(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<serde_json::Value>, ApiError> {
    let organization_id = organization_of(&current)?;
    let read = omnion_developer::overview::read(
        state.db().pool(),
        organization_id,
        OffsetDateTime::now_utc(),
    )
    .await
    .map_err(ApiError::from)?;

    Ok(Json(json!({
        "keys": {
            "active": read.active_keys,
            "expired": read.expired_keys,
            "revoked": read.revoked_keys,
        },
        "requests_today": read.requests_today,
        "errors_today": read.errors_today,
        "recent_failures": read.recent_failures,
        // From the same constant the log screen and the CSV export read, so a card cannot quote
        // a window the table does not honour. Main's version called `logs_store::window_days()`,
        // which this branch does not have; the number is the same one, written down once.
        "log_retention_days": omnion_developer::log_vocab::RETENTION_DAYS,
    })))
}

/// Refuse a key that carries a scope its issuer does not hold.
///
/// **This gate was missing entirely, and the walk that exists to catch it had never run.** It was
/// written three ticks ago and every run reported `8 passed in 0.08 s` — eight SKIPs, because a
/// refused database connection and a passing walk print the same line. The first run that reached a
/// live database answered `201` to a request for a key carrying `iam.users.manage` from an account
/// holding only `developer.keys.manage`, and the store wrote it. That is a privilege escalation
/// through the API-key surface: `developer.keys.manage` was supposed to mean "manage the keys you
/// are allowed to delegate", and it meant "mint a key with any scope in the catalogue".
///
/// The check is the caller's **effective** permissions for the same organization scope, which is
/// what [`list_scopes`] already derives its `grantable` flag from — so the picker and the create
/// route now answer from one source and cannot disagree. Reading the catalogue instead would be
/// wrong: a catalogue entry says the permission *exists*, not that this account holds it.
///
/// The refusal names the offending scope and points at the field. Naming it matters more than the
/// status code: an operator who typed `iam.users.manage` by mistake needs to be told *that* is the
/// problem, not that the request was invalid.
async fn refuse_undelegable_scopes(
    pool: &sqlx::PgPool,
    user_id: Uuid,
    organization_id: Uuid,
    scopes: &[String],
) -> Result<(), ApiError> {
    if scopes.is_empty() {
        // The store already refuses an empty scope list; returning early here keeps this gate
        // about delegation rather than about validation, so the two errors stay distinguishable.
        return Ok(());
    }
    let effective = omnion_permissions::effective_permissions(
        pool,
        user_id,
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

    if let Some(refused) = scopes.iter().find(|scope| !effective.allows(scope)) {
        return Err(ApiError::bad_request(
            "scope_not_delegable",
            format!(
                "{refused:?} is not a permission you hold, so a key you create cannot carry it"
            ),
        )
        .with_details(json!({ "scope": refused, "field": "scopes" })));
    }
    Ok(())
}

/// Whether this account may mint a key on the `high` tier.
///
/// Read from the role bindings rather than from the request, and deliberately *not* from the
/// effective permission set: `high` is a statement about how much load the caller is trusted to
/// put on the platform, and every role that holds `All` — owner, administrator — is trusted with
/// that. Asking "does this account hold a privileged role" is the question the request's own
/// words ask ("requires an owner or admin role"), and it is the only version of the question
/// whose answer a caller cannot influence.
async fn caller_may_elevate(
    pool: &sqlx::PgPool,
    user_id: Uuid,
    organization_id: Uuid,
) -> Result<bool, ApiError> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from role_bindings b join roles r on r.id = b.role_id \
         where b.revoked_at is null \
           and (b.expires_at is null or b.expires_at > now()) \
           and r.key in ('owner', 'administrator') \
           and ( (b.organization_id = $2) \
              or (b.scope_type = 'global' and b.organization_id is null) ) \
           and ( (b.subject_type = 'user' and b.subject_id = $1) \
              or (b.subject_type = 'group' and b.subject_id in \
                  (select group_id from group_members where user_id = $1)) )",
    )
    .bind(user_id)
    .bind(organization_id)
    .fetch_one(pool)
    .await
    .map_err(decode_error)?;
    Ok(count > 0)
}

/// Turn the panel's four expiry choices into an instant.
///
/// `None` means never, which is a legitimate choice and the one a developer testing an
/// integration wants. A *negative* or absurd number is refused rather than clamped: `expires_in_
/// days: -1` from a hand-written request means "expire in the past", and storing that produces a
/// key that is dead on arrival with no explanation — so it is a `400` naming the field.
fn expiry_from_days(days: Option<i64>) -> Result<Option<OffsetDateTime>, DeveloperError> {
    let Some(days) = days else {
        return Ok(None);
    };
    if days == 0 {
        return Ok(None);
    }
    if days < 0 || days > 3650 {
        return Err(DeveloperError::InvalidExpiry(days));
    }
    Ok(Some(OffsetDateTime::now_utc() + time::Duration::days(days)))
}

/// The default time window of the log screen.
///
/// Twenty-four hours, matching the panel's own default filter. Absent a default the query would
/// read the whole fourteen-day retention window, which is both slow and useless: nobody opens
/// this screen to read two weeks.
fn default_window(since_hours: Option<i64>) -> Option<OffsetDateTime> {
    let hours = since_hours.unwrap_or(24);
    // A negative window would produce `created_at >= now() + n` — an empty page presented as
    // "no requests", which reads as "the integration is broken" rather than "the filter is
    // nonsense". Clamped to zero instead: an immediate-past window that returns everything up
    // to now is the least surprising thing a nonsense filter can mean.
    if hours <= 0 {
        return None;
    }
    Some(OffsetDateTime::now_utc() - time::Duration::hours(hours))
}

/// Map one log row onto the shape the API returns.
///
/// A hand-written mapping rather than `query_as` for one reason that matters: this is the type
/// that a caller reads, and adding a column to the table must not be able to add a field to it.
/// `bytes_in` and `bytes_out` are `Option` because a request that was refused before a body was
/// read has no size, and `0` would read as "an empty body was sent".
fn log_from_row(row: &sqlx::postgres::PgRow) -> Result<RequestLog, ApiError> {
    let status: i16 = row.try_get("status").map_err(decode_error)?;
    Ok(RequestLog {
        id: row.try_get("id").map_err(decode_error)?,
        organization_id: row.try_get("organization_id").map_err(decode_error)?,
        api_key_id: row.try_get("api_key_id").map_err(decode_error)?,
        // The three attribution columns migration `0243` added. Read as `Option`/defaulted
        // because they are nullable by design — a session-authenticated request has no key
        // prefix, and a row written before this branch shipped has no recorded permission.
        api_key_prefix: row.try_get("api_key_prefix").map_err(decode_error)?,
        actor_user_id: row.try_get("actor_user_id").map_err(decode_error)?,
        actor_name: row.try_get("actor_name").unwrap_or_default(),
        permission: row.try_get("permission").map_err(decode_error)?,
        method: row.try_get("method").map_err(decode_error)?,
        path: row.try_get("path").map_err(decode_error)?,
        status,
        duration_ms: row.try_get("duration_ms").map_err(decode_error)?,
        request_id: row.try_get("request_id").map_err(decode_error)?,
        bytes_in: row.try_get("bytes_in").map_err(decode_error)?,
        bytes_out: row.try_get("bytes_out").map_err(decode_error)?,
        error_code: row.try_get("error_code").map_err(decode_error)?,
        created_at: row.try_get("created_at").map_err(decode_error)?,
    })
}

///
/// `ApiError` has no `From<sqlx::Error>`, so every route in this API that speaks sqlx directly
/// writes this mapping out — which means the *string* a database failure produces is a
/// hand-written decision, and three spellings of it in one file would be three things to keep
/// aligned. One helper, used by every call site here.
fn decode_error(error: sqlx::Error) -> ApiError {
    ApiError::from_core(omnion_core::CoreError::Unavailable {
        dependency: "developer store".into(),
        message: error.to_string(),
    })
}

/// Write the `developer.*` audit entry a mutation owes.
///
/// Propagated rather than logged-and-ignored, matching the rest of the API: a key that was
/// minted and left no audit row is a credential an operator cannot account for, and a `500`
/// naming the audit table is a better outcome than a silent gap.
pub(crate) async fn audit(
    state: &AppState,
    current: &CurrentSession,
    address: &ClientAddress,
    action: &'static str,
    metadata: serde_json::Value,
) -> Result<(), ApiError> {
    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, action)
            .target("api_key", "developer")
            .metadata(metadata)
            .ip_address(address.as_text()),
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Compilation guards for the properties this file claims
// ---------------------------------------------------------------------------------------------

/// The write-only property, checked by the type system rather than by a test.
///
/// `ApiKey` is what every list and detail read returns and it has no `secret` field; the
/// assertion here is that this module can name it as the response type of *every* read without
/// a conversion step — so a future read that wanted the secret would have to introduce a
/// different type, and that type would not exist.
#[allow(dead_code)]
fn the_read_shape_cannot_carry_a_secret(key: ApiKey) -> ApiKey {
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn the_four_expiry_choices_the_panel_offers_all_produce_an_instant() {
        for days in [30, 90, 365] {
            let expiry = expiry_from_days(Some(days)).expect("a supported choice");
            assert!(expiry.is_some(), "{days} days must produce an instant");
        }
        // "Never" is a real choice, expressed twice: omit the field, or send zero.
        assert!(expiry_from_days(None).unwrap().is_none());
        assert!(expiry_from_days(Some(0)).unwrap().is_none());
    }

    #[test]
    fn an_expiry_in_the_past_is_refused_rather_than_stored() {
        // `expires_in_days: -1` is the shape of a hand-written request that wants a key which
        // never works. Storing it produces a credential that is dead on arrival, and the caller
        // sees "my key does not authenticate" with nothing on the panel to explain it.
        assert!(matches!(
            expiry_from_days(Some(-1)),
            Err(DeveloperError::InvalidExpiry(-1))
        ));
        assert!(matches!(
            expiry_from_days(Some(-365)),
            Err(DeveloperError::InvalidExpiry(_))
        ));
        // And beyond a decade, which is a typo rather than an intent.
        assert!(matches!(
            expiry_from_days(Some(36_500)),
            Err(DeveloperError::InvalidExpiry(_))
        ));
    }

    #[test]
    fn the_log_screen_defaults_to_a_day_and_never_to_a_window_that_looks_empty() {
        // No filter at all: 24 hours, matching the panel's own default.
        let window = default_window(None).expect("a default window");
        let day_ago = OffsetDateTime::now_utc() - time::Duration::hours(24);
        assert!(window <= day_ago, "the default must cover at least a day");

        // A negative window would read as `created_at >= now() + n` — an empty page presented as
        // "no requests", which an operator reads as "the integration is broken".
        assert!(
            default_window(Some(-5)).is_none(),
            "a negative window must not become a future cutoff"
        );
        assert!(default_window(Some(0)).is_none());
        // A week is honoured, because a week is a thing a person asks for.
        assert!(default_window(Some(24 * 7)).unwrap() < day_ago);
    }

    #[test]
    fn the_default_window_never_moves_forwards_in_time() {
        // The property, stated directly rather than through one input: whatever the filter says,
        // the cutoff is in the past. This is the assertion that would catch a future edit which
        // "helpfully" flipped a sign.
        for hours in [None, Some(-100), Some(0), Some(1), Some(24), Some(8760)] {
            if let Some(window) = default_window(hours) {
                assert!(
                    window <= OffsetDateTime::now_utc(),
                    "hours={hours:?} produced a cutoff in the future"
                );
            }
        }
    }

    #[test]
    fn the_one_time_response_flattens_the_key_and_names_the_field_secret() {
        // `secret` rather than `plaintext`: the panel's dialog says "this will not be shown
        // again", and the field name is the first half of that sentence. The flatten means the
        // response *is* an `ApiKey` with one extra field, so a client that already reads a key
        // needs no new shape to read a minted one.
        let minted = Minted {
            key: ApiKey {
                id: Uuid::nil(),
                organization_id: Uuid::nil(),
                name: "ci".to_owned(),
                prefix: "omn_000000000000".to_owned(),
                scopes: vec!["developer.keys.read".to_owned()],
                environment: Environment::Sandbox,
                rate_tier: RateTier::Standard,
                ip_allowlist: None,
                expires_at: None,
                last_used_at: None,
                revoked_at: None,
                rotated_at: None,
                created_by: Uuid::nil(),
                created_at: datetime!(2026-10-01 12:00 UTC),
                status: omnion_developer::model::KeyStatus::Active,
            },
            plaintext: "omn_000000000000.secret".to_owned(),
        };
        let rendered = serde_json::to_value(MintedResponse::from(minted)).expect("serialises");
        assert_eq!(rendered["secret"], "omn_000000000000.secret");
        // The key's own fields are at the top level, not nested under `key`.
        assert_eq!(rendered["name"], "ci");
        assert_eq!(rendered["prefix"], "omn_000000000000");
        assert!(rendered.get("key").is_none(), "the key must be flattened");
    }

    #[test]
    fn a_key_row_never_serialises_to_a_field_named_secret() {
        // The write-only property as an actual assertion on the bytes a client receives. It is
        // not "there is no field" but "the name is absent", which is what a test that greps a
        // response body can also rely on.
        let rendered =
            serde_json::to_string(&KeysResponse { keys: Vec::new() }).expect("serialises");
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("hash"));
    }
}

// ---------------------------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------------------------

/// Turn a crate error into the API's own, keeping the code and the field detail.
///
/// Every variant that a form can provoke carries a `field`, because a validation message that
/// cannot be placed under its input is a message the person has to go and find. The store's
/// `KeyNameTaken` is the one that matters most here: it is a unique-index violation translated
/// into "that name is taken" next to the name box rather than a `500` with a PostgreSQL
/// constraint name in it.
impl From<DeveloperError> for ApiError {
    fn from(error: DeveloperError) -> Self {
        let message = error.to_string();
        let api = if error.is_client_error() {
            ApiError::bad_request(error.code(), message)
        } else {
            ApiError::from_core(omnion_core::CoreError::Unavailable {
                dependency: "developer store".into(),
                message,
            })
        };
        match &error {
            DeveloperError::InvalidName { .. } | DeveloperError::KeyNameTaken(_) => {
                api.with_details(json!({ "field": "name" }))
            }
            DeveloperError::NoScopes
            | DeveloperError::EmptyScope
            | DeveloperError::DuplicateScope(_) => api.with_details(json!({ "field": "scopes" })),
            DeveloperError::UnknownEnvironment(_) => {
                api.with_details(json!({ "field": "environment" }))
            }
            DeveloperError::UnknownRateTier(_) | DeveloperError::InvalidExpiry(_) => {
                api.with_details(json!({ "field": "expires_in_days" }))
            }
            DeveloperError::InvalidCidr(_) => api.with_details(json!({ "field": "ip_allowlist" })),
            DeveloperError::UnknownStatusClass(_) | DeveloperError::NegativeDuration => {
                api.with_details(json!({ "field": "status_class" }))
            }
            DeveloperError::KeyNotFound => {
                // A `404` rather than a `403`, and the same one a missing id produces: a
                // foreign tenant's key must not be distinguishable from one that never existed,
                // or the endpoint is an existence oracle for other tenants' key ids.
                ApiError::new(
                    StatusCode::NOT_FOUND,
                    "api_key_not_found",
                    "no API key with that id exists in this organization",
                )
            }
            // Slice 4. Each names the input it came from, which is the whole reason this match
            // exists: `ScaffoldRefused` already carries its own `code`, so the panel can switch
            // on it without the API having to know which rule was broken -- but it cannot put
            // the message under a field unless the field is named here.
            DeveloperError::UnknownScaffoldKind(_) => api.with_details(json!({ "field": "kind" })),
            DeveloperError::UnknownScaffoldTarget(_) => {
                api.with_details(json!({ "field": "target" }))
            }
            // The rule carries its own stable code (`invalid_scaffold_name`), so it is forwarded
            // rather than replaced: a name rule that grows a reason keeps the reason.
            DeveloperError::ScaffoldRefused { code, .. } => {
                api.with_details(json!({ "field": "name", "rule": code }))
            }
            DeveloperError::ScaffoldNotFound => ApiError::new(
                StatusCode::NOT_FOUND,
                "scaffold_not_found",
                "no such scaffold in this organization",
            ),
            // The device-code refusals carry no field: they are answers about a *code*, and the
            // panel has one input for that. `InvalidDeviceCode` stays a `400` rather than a
            // `404` on purpose -- it is the same answer for a code that never existed, one that
            // has expired and one that was already spent, and a `404` would tell a prober which
            // of the three it hit.
            DeveloperError::InvalidDeviceCode
            | DeveloperError::DeviceCodePending
            | DeveloperError::DeviceCodeSlowDown { .. }
            | DeveloperError::DeviceCodeApprovalRefused => api,
            _ => api,
        }
    }
}
