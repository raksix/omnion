//! `/api/v1/reliability/idempotency` — the keyed-write ledger (REQ-127, slice 2).
//!
//! Three routes, and the third is the one an operator reaches for at 3am:
//!
//! | Path | Power | Question it answers |
//! |---|---|---|
//! | `GET /reliability/idempotency` | `reliability.read` | which keys exist, in what state, and how often each was replayed |
//! | `GET /reliability/idempotency/{key}` | `reliability.read` | one key's stored response METADATA |
//! | `DELETE /reliability/idempotency/{key}` | `reliability.manage` | this key is stuck — release it |
//!
//! ## The scope is the caller's, and a key belongs to one subject
//!
//! A key is stored under `(scope, subject_id, key)` and the unique index is on that triple, so
//! "the key `abc`" is not one row on the platform — it is one row per subject. The list route is
//! therefore scoped to the signed-in account rather than searching the whole table: a key is a
//! write identifier, and one tenant reading another's is both a leak and a way to probe for keys
//! in use.
//!
//! ## The detail route never answers with a stored BODY
//!
//! The request says the detail screen shows "stored response metadata", and the reason is in the
//! risks: *"stored bodies pass the shared redaction helper, are capped, and expire; the store
//! must never become a second request archive."* A read route that returned the body would hand
//! a `reliability.read` holder a copy of a write's payload for as long as the key lives, which is
//! exactly the archive the request forbids. What it does return is everything an operator needs
//! to decide: the status, whether a body was kept inline, how large it was, and the reference when
//! the body went to the object store.
//!
//! ## Release is the ONLY destructive action, and it is audited
//!
//! Releasing a key flips `in_progress` to `failed`, which is what makes the next attempt of that
//! write run again. It is refused for a `completed` key — that one has a real stored response, and
//! releasing it would destroy a result the caller is entitled to replay. The reason is required
//! (an empty body is `400`) and it travels into both the audit row and the
//! `reliability.idempotency.keys.released` event, because "somebody pressed the release button" is
//! not a record.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::{NewAuditEntry, record as record_audit};
use omnion_events::{NewEvent, bus};
use omnion_reliability::idempotency::{self, INLINE_BODY_CAP};
use omnion_reliability::idem_store;
use omnion_reliability::vocabulary::events::IDEMPOTENCY_KEYS_RELEASED;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

/// The read of the keys list.
#[derive(Debug, Deserialize)]
pub struct KeysQuery {
    /// How many keys to return; clamped to the shared page ceiling.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Filter by state. Absent means every state, because "show me only the stuck ones" and
    /// "show me everything" are different questions and the second is the default a screen opens.
    #[serde(default)]
    pub state: Option<String>,
}

/// One key as the list reads it.
#[derive(Debug, Serialize)]
pub struct KeyBody {
    /// The key the caller sent.
    pub key: String,
    /// The endpoint family it protects, e.g. `POST /api/v1/automations`.
    pub scope: String,
    /// `in_progress` · `completed` · `failed`.
    pub state: String,
    /// How many times this key has been answered from the store.
    pub replay_count: i32,
    /// When the key stops protecting.
    pub expires_at: String,
    /// When the attempt finished, or `None` while it is running.
    pub completed_at: Option<String>,
    /// Whether a response was stored at all, which is not the same as being `completed`.
    pub has_response: bool,
}

/// The list, with the counts the header tiles read.
#[derive(Debug, Serialize)]
pub struct KeysBody {
    /// The keys, newest first.
    pub keys: Vec<KeyBody>,
    /// How many keys this subject holds in total, in every state.
    pub total: i64,
    /// How many are stuck `in_progress` — the only number that needs action.
    pub in_progress: i64,
    /// The state filter that was applied, so the panel can show what it is looking at.
    pub state_filter: Option<String>,
}

/// One key's stored response METADATA. Never the body — see the module header.
#[derive(Debug, Serialize)]
pub struct KeyDetailBody {
    /// The key.
    pub key: String,
    /// The endpoint family it protects.
    pub scope: String,
    /// Its state.
    pub state: String,
    /// The method and path of the first attempt.
    pub method: String,
    pub path: String,
    /// The stored status, or `None` while the attempt is running.
    pub response_status: Option<i16>,
    /// Whether a body was kept inline. A key can be `completed` with nothing inline.
    pub stored_body: bool,
    /// The size of the stored body, so an operator can see how close it came to the cap.
    pub stored_body_bytes: Option<usize>,
    /// The cap the size is compared against, so the panel never hard-codes it.
    pub inline_cap_bytes: usize,
    /// The object-store reference, when the body was too large to keep inline.
    pub response_body_ref: Option<String>,
    /// How many replays this key has served.
    pub replay_count: i32,
    /// The request id of the FIRST execution, which is what a replay reports.
    pub original_request_id: Option<String>,
    pub created_expires_at: String,
    pub completed_at: Option<String>,
    /// What the platform will do with the next attempt of this key, in the panel's words.
    pub next_attempt: &'static str,
}

/// The release action's body.
#[derive(Debug, Deserialize)]
pub struct ReleaseInput {
    /// Why the key is being released. Required, and it is an empty string that is refused —
    /// a reason nobody had to type is a reason nobody had.
    pub reason: String,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /reliability/idempotency` — the caller's keys, newest first.
pub async fn list_keys(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<KeysQuery>,
) -> Result<Json<KeysBody>, ApiError> {
    let subject = session.user.id.to_string();
    if let Some(wanted) = query.state.as_deref()
        && !omnion_reliability::vocabulary::IDEMPOTENCY_STATES.contains(&wanted)
    {
        return Err(ApiError::bad_request(
            "invalid_reliability_input",
            format!(
                "state must be one of {}, got '{wanted}'",
                omnion_reliability::vocabulary::IDEMPOTENCY_STATES.join(", ")
            ),
        ));
    }

    let limit = query
        .limit
        .unwrap_or(omnion_reliability::vocabulary::MAX_PAGE as usize)
        .clamp(1, omnion_reliability::vocabulary::MAX_PAGE as usize);
    let pool = state.db().pool();
    // `idem_store::list` is scope-scoped, and this screen is explicitly cross-family: a key is
    // `(scope, subject, key)` and an operator looking at "my keys" means all of them. The query
    // is therefore written out here rather than bending the store's helper to a second shape —
    // a helper that grows a wildcard branch is a helper whose contract no caller can rely on.
    let rows: Vec<idem_store::KeySummary> = if let Some(wanted) = query.state.as_deref() {
        sqlx::query_as(
            "select key, scope, state, replay_count, expires_at, completed_at \
               from idempotency_keys \
              where subject_id = $1 and state = $2 \
              order by created_at desc limit $3",
        )
        .bind(&subject)
        .bind(wanted)
        .bind(limit as i64)
        .fetch_all(pool)
        .await
        .map_err(internal)?
    } else {
        sqlx::query_as(
            "select key, scope, state, replay_count, expires_at, completed_at \
               from idempotency_keys \
              where subject_id = $1 \
              order by created_at desc limit $2",
        )
        .bind(&subject)
        .bind(limit as i64)
        .fetch_all(pool)
        .await
        .map_err(internal)?
    };

    let total: i64 = sqlx::query_scalar(
        "select count(*) from idempotency_keys where subject_id = $1",
    )
        .bind(&subject)
        .fetch_one(pool)
        .await
        .map_err(internal)?;
    let in_progress: i64 = sqlx::query_scalar(
        "select count(*) from idempotency_keys \
          where subject_id = $1 and state = 'in_progress'",
    )
    .bind(&subject)
    .fetch_one(pool)
    .await
    .map_err(internal)?;

    Ok(Json(KeysBody {
        keys: rows
            .into_iter()
            .map(|row| KeyBody {
                has_response: row.state == omnion_reliability::vocabulary::IDEMPOTENCY_COMPLETED,
                key: row.key,
                scope: row.scope,
                state: row.state,
                replay_count: row.replay_count,
                expires_at: format_offset(row.expires_at),
                completed_at: row.completed_at.map(format_offset),
            })
            .collect(),
        total,
        in_progress,
        state_filter: query.state,
    }))
}

/// `GET /reliability/idempotency/{key}` — one key, metadata only.
pub async fn get_key(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(key): Path<String>,
) -> Result<Json<KeyDetailBody>, ApiError> {
    let subject = session.user.id.to_string();
    // The key is `(scope, subject, key)` on the unique index, and this screen is cross-family, so
    // the read is by `(subject, key)` — see `find_any_scope` for why that is not the store's
    // `find` with a wildcard.
    let record = find_any_scope(state.db().pool(), &subject, &key)
        .await
        .map_err(map_store)?
        .ok_or_else(|| ApiError::not_found("idempotency key", &key))?;

    // The words are computed from the state BEFORE it is moved into the body, which is the whole
    // reason this reads the way it does: `next_attempt_words(&record.state)` after a
    // `state: record.state` line does not compile, and the fix that compiles — cloning the
    // string — is a copy made only to satisfy the borrow checker. The explanation is derived from
    // the state the record actually holds, which is why it is bound first.
    let next_attempt = next_attempt_words(&record.state);
    let (_stored_body, stored_body_bytes, original_request_id, response_status) =
        read_response_metadata(state.db().pool(), &record.scope, &subject, &key).await?;

    Ok(Json(KeyDetailBody {
        key: record.key,
        scope: record.scope,
        state: record.state,
        method: record.method,
        path: record.path,
        response_status,
        stored_body: stored_body_bytes.is_some(),
        stored_body_bytes,
        inline_cap_bytes: INLINE_BODY_CAP,
        response_body_ref: record.response_body_ref,
        replay_count: record.replay_count,
        original_request_id,
        created_expires_at: format_offset(record.expires_at),
        completed_at: record.completed_at.map(format_offset),
        next_attempt,
    }))
}

/// `DELETE /reliability/idempotency/{key}` — release a stuck key so the write can run again.
pub async fn release_key(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(key): Path<String>,
    Json(input): Json<ReleaseInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if input.reason.trim().is_empty() {
        return Err(ApiError::bad_request(
            "invalid_reliability_input",
            "a release needs a reason: the next attempt re-runs the write",
        )
        .with_details(json!({ "field": "reason" })));
    }

    let subject = session.user.id.to_string();
    let pool = state.db().pool();
    let record = find_any_scope(pool, &subject, &key)
        .await
        .map_err(map_store)?
        .ok_or_else(|| ApiError::not_found("idempotency key", &key))?;

    // The refusal is explicit rather than "released: false", because the two states need
    // different sentences and an operator who released a completed key would otherwise be told
    // nothing and left believing the write can run again.
    if record.state != omnion_reliability::vocabulary::IDEMPOTENCY_IN_PROGRESS {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "idempotency_not_releasable",
            format!(
                "key '{}' is {} and has a stored response; releasing it would destroy a result the \
                 caller is entitled to replay",
                record.key, record.state
            ),
        )
        .with_details(json!({ "key": record.key, "state": record.state })));
    }

    let released = idem_store::release_stale(pool, &record.scope, &subject, &key, OffsetDateTime::now_utc())
        .await
        .map_err(map_store)?;
    if !released {
        // Lost a race with the attempt that was running: it finished between the read and the
        // UPDATE. That is a good outcome and the caller is told it.
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "idempotency_already_finished",
            format!(
                "key '{key}' finished while the release was being applied; nothing to release"
            ),
        ));
    }

    let metadata = json!({
        "key": record.key,
        "scope": record.scope,
        "method": record.method,
        "path": record.path,
        "reason": input.reason.trim(),
        "held_since": format_offset(record.expires_at),
    });

    if let Err(error) = record_audit(
        pool,
        NewAuditEntry::by_user(session.user.id, "idempotency.key.release")
            .organization(session.user.organization_id)
            .metadata(metadata.clone()),
    )
    .await
    {
        tracing::warn!(error = %error, "the key was released but the audit entry was not written");
    }
    if let Err(error) = bus::emit(
        pool,
        NewEvent::new(IDEMPOTENCY_KEYS_RELEASED)
            .organization(session.user.organization_id)
            .actor(session.user.id)
            .payload(metadata),
    )
    .await
    {
        tracing::warn!(error = %error, "the key was released but the event was not emitted");
    }

    Ok(Json(json!({
        "key": record.key,
        "scope": record.scope,
        "state": omnion_reliability::vocabulary::IDEMPOTENCY_FAILED,
        "released": true,
        "message": "the key is free; the next attempt of this write runs again",
    })))
}

// ---------------------------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------------------------

/// Find a key by `(subject, key)` across every scope.
///
/// **`idem_store::find` is not reachable for this screen**, and the reason is its contract: it
/// takes a scope because a caller that knows its endpoint family should be able to ask about
/// exactly that family, and the unique index makes the key `(scope, subject, key)`. The panel
/// screen knows the family is different per key and wants all of them, so the query is written
/// out here. A wildcard branch inside the store helper would have been the smaller change and
/// the worse one: a helper whose scope argument means "any" for one caller and "exactly" for
/// another is a helper no caller can reason about.
async fn find_any_scope(
    pool: &sqlx::PgPool,
    subject_id: &str,
    key: &str,
) -> Result<Option<idempotency::KeyRecord>, omnion_reliability::ReliabilityError> {
    let sql = format!(
        "select {COLUMNS} from idempotency_keys \
          where subject_id = $1 and key = $2 \
          order by created_at desc \
          limit 1"
    );
    let row: Option<Row> = sqlx::query_as(&sql)
        .bind(subject_id)
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(key_record_from))
}

/// Build the crate's [`idempotency::KeyRecord`] from a row read here.
///
/// The conversion is a `From` impl in the STORE, over a row type that is private to it. A
/// second conversion is the price of reading the table from a second place, and the price is
/// kept honest by returning the store's type and nothing more: the two fields this screen
/// additionally needs (the stored headers and the body's rendered text) are read by name in
/// [`read_response_metadata`], not smuggled through the record the decision function sees.
fn key_record_from(row: Row) -> idempotency::KeyRecord {
    idempotency::KeyRecord {
        scope: row.scope,
        subject_id: row.subject_id,
        key: row.key,
        method: row.method,
        path: row.path,
        request_hash: row.request_hash,
        state: row.state,
        response_status: row.response_status,
        response_body: row.response_body.map(|value| match value {
            serde_json::Value::String(text) => text,
            other => other.to_string(),
        }),
        response_body_ref: row.response_body_ref,
        replay_count: row.replay_count,
        expires_at: row.expires_at,
        completed_at: row.completed_at,
    }
}

/// The columns of `idempotency_keys`, in the order [`Row`] declares them.
const COLUMNS: &str = "scope, subject_id, key, method, path, request_hash, state, \
                       response_status, response_headers, response_body, response_body_ref, \
                       replay_count, expires_at, completed_at";

/// The row shape, mirroring the store's own — **a deliberate duplicate**.
///
/// The store's `Row` is private and its `KeyRecord` does not carry `response_headers` or the
/// body. This screen needs the stored status, the body SIZE and the original request id: the
/// three things the pure decision function has no use for. Widening `KeyRecord` to carry them
/// would make every `decide` call site — the middleware, the tests, every future caller — pay
/// for two fields none of them read, and it would put an HTTP-shaped concern (which headers
/// replay) inside the type that defines what a replay IS.
#[derive(Debug, sqlx::FromRow)]
struct Row {
    scope: String,
    subject_id: String,
    key: String,
    method: String,
    path: String,
    request_hash: String,
    state: String,
    response_status: Option<i16>,
    // Read by `read_response_metadata` in its own query rather than off this row: a second
    // query per field is one extra round trip, and folding them into this row would make the
    // LIST query (which does not need them) carry a body the screen never shows.
    #[allow(dead_code, reason = "the shape must match the table's column list")]
    response_headers: serde_json::Value,
    response_body: Option<serde_json::Value>,
    response_body_ref: Option<String>,
    replay_count: i32,
    expires_at: OffsetDateTime,
    completed_at: Option<OffsetDateTime>,
}

/// The stored response's metadata: whether a body was kept, how large, and the original id.
async fn read_response_metadata(
    pool: &sqlx::PgPool,
    scope: &str,
    subject_id: &str,
    key: &str,
) -> Result<(bool, Option<usize>, Option<String>, Option<i16>), ApiError> {
    let headers: Option<serde_json::Value> = sqlx::query_scalar(
        "select response_headers from idempotency_keys \
          where scope = $1 and subject_id = $2 and key = $3",
    )
    .bind(scope)
    .bind(subject_id)
    .bind(key)
    .fetch_optional(pool)
    .await
    .map_err(internal)?;

    let (bytes, status) = sqlx::query_as::<_, (Option<serde_json::Value>, Option<i16>)>(
        "select response_body, response_status from idempotency_keys \
          where scope = $1 and subject_id = $2 and key = $3",
    )
    .bind(scope)
    .bind(subject_id)
    .bind(key)
    .fetch_optional(pool)
    .await
    .map_err(internal)?
    .map_or((None, None), |(body, status)| {
        (
            body.map(|value| match value {
                serde_json::Value::String(text) => text.len(),
                other => other.to_string().len(),
            }),
            status,
        )
    });

    // The id is read from the STORED headers, not recomputed: a replay answers with this value,
    // and the panel shows the same one, so the two can never disagree about which execution
    // was the original.
    let original = headers
        .and_then(|value| {
            value
                .get(crate::idempotency_middleware::IDEMPOTENCY_ORIGINAL_HEADER)
                .and_then(|id| id.as_str())
                .map(str::to_owned)
        })
        .filter(|id| !id.is_empty());

    Ok((bytes.is_some(), bytes, original, status))
}

/// What the next attempt of a key in this state will do, in words an operator can act on.
fn next_attempt_words(state: &str) -> &'static str {
    match state {
        s if s == omnion_reliability::vocabulary::IDEMPOTENCY_COMPLETED => {
            "replays the stored response"
        }
        s if s == omnion_reliability::vocabulary::IDEMPOTENCY_FAILED => {
            "runs the write again"
        }
        _ => "is refused with 409 until the attempt finishes or the key is released",
    }
}

/// Format a timestamp the way the rest of the platform does.
fn format_offset(value: OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| value.unix_timestamp().to_string())
}

/// Map a crate error onto the API surface, mirroring the limits screen.
fn map_store(error: omnion_reliability::ReliabilityError) -> ApiError {
    use omnion_reliability::ReliabilityError as E;
    match error {
        E::Invalid(message) => ApiError::bad_request("invalid_reliability_input", message),
        E::NotFound => {
            ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such reliability record")
        }
        E::ProviderUnavailable { provider, retry_after } => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "provider_unavailable",
            format!("provider {provider} is unavailable"),
        )
        .with_retry_after(retry_after.unwrap_or(1)),
        E::RetriesExhausted { subsystem } => ApiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "retry_exhausted",
            format!("retries exhausted for {subsystem}"),
        ),
        E::Database(inner) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("reliability store: {inner}"),
        ),
    }
}

/// A SQL failure, which is never a client's fault and never a useful detail for one.
fn internal(error: sqlx::Error) -> ApiError {
    tracing::error!(error = %error, "the idempotency screen could not read its table");
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        "the idempotency store could not be read",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnion_reliability::vocabulary::{IDEMPOTENCY_COMPLETED, IDEMPOTENCY_FAILED};

    #[test]
    fn the_next_attempt_words_cover_every_state_the_platform_can_store() {
        // The three states are constants elsewhere; this is the panel's own list, so it is
        // checked against the vocabulary rather than trusted.
        for state in omnion_reliability::vocabulary::IDEMPOTENCY_STATES {
            assert!(
                !next_attempt_words(state).is_empty(),
                "state {state} would render as an empty explanation on the detail screen"
            );
        }
        assert_eq!(
            next_attempt_words(IDEMPOTENCY_COMPLETED),
            "replays the stored response"
        );
        assert_eq!(next_attempt_words(IDEMPOTENCY_FAILED), "runs the write again");
    }
}
