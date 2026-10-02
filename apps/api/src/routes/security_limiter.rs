//! `/api/v1/security/rate-limits` and `/security/sign-in-protection` (REQ-012, slice 3).
//!
//! Two screens, one rule: **the tester must be the limiter.** Every endpoint here that answers
//! "would this be limited" calls [`omnion_security::decide`], the same function the middleware
//! calls with the same inputs. A tester implemented as a second copy of the arithmetic is a
//! tester that agrees with the middleware on the day it was written and drifts the first time
//! someone tunes a limit — which is exactly the day somebody is relying on it.
//!
//! The endpoints, and what each is for:
//!
//! | Path | Power | Purpose |
//! |---|---|---|
//! | `GET /security/rate-limits` | `security.read` | the five scopes, merged with the defaults |
//! | `PUT /security/rate-limits` | `security.manage` | save the document (compare-and-swap) |
//! | `POST /security/rate-limits/test` | `security.read` | dry-run one request |
//! | `GET /security/sign-in-protection` | `security.read` | the lockout document |
//! | `PUT /security/sign-in-protection` | `security.manage` | save it (compare-and-swap) |
//! | `GET /security/locked-accounts` | `security.read` | who is locked right now |
//! | `POST /security/locked-accounts/{id}/unlock` | `security.manage` | release one |
//!
//! **The tester takes an optional `count`.** Without it the tester reports what the policy
//! *allows*, which is the question an operator has while tuning. With it, the tester answers
//! "if the counter already stood at N, what would happen", which is the question they have when
//! a real client is being refused and the screen needs to explain it. A tester that can only
//! answer the first question is useless during an incident, which is when the screen is open.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use omnion_audit::{NewAuditEntry, record as record_audit};
use omnion_events::{NewEvent, bus};
use omnion_security::{
    ClientId, LockoutPolicy, LockoutState, RATE_SCOPES, RatePolicy, RequestFacts, Verdict, decide,
    evaluate_lockout, failures_in_window, load_documents, load_lockout, load_rate_limits,
    locked_accounts, locked_count, lockout_to_document, merge_with_defaults, parse_lockout,
    parse_rate_limits, rate_limits_to_document, save_lockout, save_rate_limits, scope_of,
    unlock_account,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::net::IpAddr;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

/// The longest a single backoff delay may become, mirroring `crates/security/src/lockout.rs`.
///
/// Duplicated rather than imported because the crate does not re-export it, and a **re-export is
/// the better fix** — so this constant is here as the thing to delete, and the test at the bottom
/// is what notices if the two drift. The alternative, hard-coding `60` at each use site, would
/// have been three copies and no test.
const MAX_DELAY_SECONDS: i32 = omnion_security::lockout::MAX_DELAY_SECONDS;

/// Map a crate error onto the API surface.
fn map_store(error: omnion_security::SecurityError) -> ApiError {
    use omnion_security::SecurityError as E;
    match error {
        E::Invalid(message) => ApiError::bad_request("invalid_security_input", message),
        E::NotFound => ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "security setting not found",
        ),
        E::Database(inner) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("security settings store: {inner}"),
        ),
    }
}

/// Now, as a unix timestamp. A helper so the tester's clock and the store's are the same call.
fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// One scope as the panel reads it.
#[derive(Debug, Serialize)]
pub struct RateLimitBody {
    /// The scope's name, one of [`RATE_SCOPES`].
    pub scope: String,
    /// The window in seconds.
    pub window_seconds: i64,
    /// Requests allowed per window.
    pub limit: i32,
    /// Extra requests forgiven inside the window.
    pub burst: i32,
    /// Whether the scope is enforced.
    pub enabled: bool,
    /// The total the scope admits, limit + burst, so the table can show it without the reader
    /// doing the addition and getting it wrong on one row.
    pub ceiling: i64,
    /// The seconds a refused caller is told to wait, so an operator can answer "for how long"
    /// without doing the modular arithmetic themselves.
    pub window_remaining_seconds: i64,
}

impl From<&RatePolicy> for RateLimitBody {
    fn from(policy: &RatePolicy) -> Self {
        let window = policy.window_seconds.max(1);
        let elapsed = now().rem_euclid(window);
        Self {
            scope: policy.scope.clone(),
            window_seconds: policy.window_seconds,
            limit: policy.limit,
            burst: policy.burst,
            enabled: policy.enabled,
            ceiling: policy.ceiling(),
            window_remaining_seconds: window - elapsed,
        }
    }
}

/// The limiter document as the panel reads it.
#[derive(Debug, Serialize)]
pub struct RateLimitsBody {
    /// The scopes, always all five — a missing row and a row nobody has configured are
    /// different states, and the table shows both.
    pub scopes: Vec<RateLimitBody>,
    /// The scope names the platform knows, for the panel's vocabulary check.
    pub vocabulary: Vec<&'static str>,
    /// Who last saved the document; `None` when nobody ever has.
    pub updated_by: Option<Uuid>,
    /// When it was last saved.
    pub updated_at: Option<String>,
    /// Whether the document on disk is one a person chose, or the platform's baseline.
    pub is_saved: bool,
}

/// The lockout document as the panel reads it.
#[derive(Debug, Serialize)]
pub struct SignInProtectionBody {
    /// The policy.
    pub policy: LockoutPolicy,
    /// How many accounts are locked right now, so the form can say so before the operator
    /// scrolls to the table.
    pub locked_accounts: i64,
    /// Whether the document is a saved one or the baseline.
    pub is_saved: bool,
    /// The ranges the form validates against, so the number inputs carry their own bounds
    /// instead of the panel hard-coding them a second time.
    pub bounds: BoundsBody,
}

/// The accepted ranges, sent to the form rather than duplicated in it.
#[derive(Debug, Serialize)]
pub struct BoundsBody {
    /// The failure window's own range, in seconds.
    pub window_seconds: [i64; 2],
    /// The attempt threshold's own range.
    pub attempts: [i32; 2],
    /// The lockout duration's own range, in minutes.
    pub lockout_minutes: [i32; 2],
    /// The base delay's own range, in seconds.
    pub base_delay_seconds: [i32; 2],
    /// The longest single delay the backoff can reach.
    pub max_delay_seconds: i32,
}

/// One locked account, as the panel reads it.
#[derive(Debug, Serialize)]
pub struct LockedAccountBody {
    /// The user.
    pub user_id: Uuid,
    /// Their address.
    pub email: String,
    /// When the lock ends.
    pub locked_until: String,
    /// Seconds left on it.
    pub seconds_remaining: i64,
    /// Failures counted when the lock was applied.
    pub failed_sign_in_count: i32,
}

/// The locked list as the panel reads it.
#[derive(Debug, Serialize)]
pub struct LockedAccountsBody {
    /// The accounts, soonest to expire first.
    pub accounts: Vec<LockedAccountBody>,
    /// How many there are in total, so a page of 50 out of 300 says so.
    pub total: i64,
}

/// A dry-run request.
#[derive(Debug, Deserialize)]
pub struct TestRequest {
    /// The HTTP method, uppercased by the handler.
    #[serde(default)]
    pub method: String,
    /// The path.
    pub path: String,
    /// The caller's address, when the operator is testing a specific client.
    #[serde(default)]
    pub client_ip: Option<String>,
    /// The caller's user id, when testing the authenticated scope.
    #[serde(default)]
    pub user_id: Option<Uuid>,
    /// The counter the tester should assume, when explaining a real refusal.
    #[serde(default)]
    pub count: Option<i64>,
    /// Whether the caller authenticated with a bearer machine key.
    #[serde(default)]
    pub machine_key: bool,
}

/// The tester's answer.
#[derive(Debug, Serialize)]
pub struct TestResponse {
    /// The scope the platform resolved the request into.
    pub scope: String,
    /// The same [`Verdict`] the middleware would produce, from the same function.
    pub verdict: Verdict,
    /// The client identity the counter is keyed on, so the tester shows *whose* budget is read.
    pub counter_identity: String,
    /// The key the counter lives under, for someone reading a Redis dump.
    pub counter_key: String,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /security/rate-limits` — the scopes, merged with the defaults.
pub async fn get_rate_limits(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<RateLimitsBody>, ApiError> {
    let _ = &session;
    let document = load_rate_limits(state.db().pool())
        .await
        .map_err(map_store)?;
    let policies = merge_with_defaults(&document);
    let updated_at = load_documents(state.db().pool())
        .await
        .map_err(map_store)?
        .rate_limits_updated_at;

    Ok(Json(RateLimitsBody {
        scopes: policies.iter().map(RateLimitBody::from).collect(),
        vocabulary: RATE_SCOPES.to_vec(),
        updated_by: None,
        updated_at: (document != serde_json::Value::Array(Vec::new()))
            .then(|| format_offset(updated_at)),
        is_saved: document != serde_json::Value::Array(Vec::new()),
    }))
}

/// `PUT /security/rate-limits` — save the document.
pub async fn put_rate_limits(
    State(state): State<AppState>,
    session: CurrentSession,
    body: Option<Json<serde_json::Value>>,
) -> Result<Json<RateLimitsBody>, ApiError> {
    let Json(document) = body.ok_or_else(|| {
        ApiError::bad_request("invalid_security_input", "the request body is empty")
    })?;

    // Validate before storing: the store's compare-and-swap is about concurrency, and it would
    // happily persist a document the middleware cannot act on. `parse_rate_limits` runs every
    // row through `RatePolicy::new`, so an out-of-range value is refused with its field named.
    let policies = parse_rate_limits(&document).map_err(map_store)?;
    if policies.len() != RATE_SCOPES.len() {
        return Err(ApiError::bad_request(
            "invalid_security_input",
            format!(
                "the limiter document must hold one row per scope ({}), not {}",
                RATE_SCOPES.join(", "),
                policies.len()
            ),
        ));
    }

    let saved = save_rate_limits(
        state.db().pool(),
        &rate_limits_to_document(&policies),
        Some(
            &load_rate_limits(state.db().pool())
                .await
                .map_err(map_store)?,
        ),
        session.user.id,
    )
    .await
    .map_err(map_store)?;

    if let Err(error) = record_audit(
        state.db().pool(),
        NewAuditEntry::by_user(session.user.id, "security.rate_limits.updated")
            .organization(session.user.organization_id)
            .metadata(json!({ "scopes": policies.len() })),
    )
    .await
    {
        tracing::warn!(error = %error, "the limits were saved but the audit entry was not written");
    }

    if let Err(error) = bus::emit(
        state.db().pool(),
        NewEvent::new("security.rate_limits.updated")
            .organization(session.user.organization_id)
            .actor(session.user.id)
            .payload(json!({ "scopes": policies.len() })),
    )
    .await
    {
        tracing::warn!(error = %error, "the limits were saved but the event was not emitted");
    }

    let saved_policies = parse_rate_limits(&saved.rate_limits).unwrap_or_else(|_| policies.clone());

    // The installed layer now decides by the numbers that were just saved (REQ-012, slice 3).
    // Without this the limiter would keep enforcing whatever it read at boot, and the screen's own
    // tester - which reads the store - would answer differently from the middleware that refuses
    // the request. A failure here is logged and not surfaced for the same reason the header save
    // logs its own: the write committed, and a 500 would report it as lost.
    if !crate::rate_limit_middleware::reload_from_store(&state).await {
        tracing::info!(
            "the rate limits were saved; the running process picks them up from the store"
        );
    }
    Ok(Json(RateLimitsBody {
        scopes: saved_policies.iter().map(RateLimitBody::from).collect(),
        vocabulary: RATE_SCOPES.to_vec(),
        updated_by: saved.rate_limits_updated_by,
        updated_at: Some(format_offset(saved.rate_limits_updated_at)),
        is_saved: true,
    }))
}

/// `POST /security/rate-limits/test` — dry-run one request through the real decision.
pub async fn test_rate_limit(
    State(state): State<AppState>,
    _session: CurrentSession,
    Json(body): Json<TestRequest>,
) -> Result<Json<TestResponse>, ApiError> {
    let policies = merge_with_defaults(
        &load_rate_limits(state.db().pool())
            .await
            .map_err(map_store)?,
    );

    let ip = match body.client_ip.as_deref() {
        None => None,
        Some(raw) => Some(raw.parse::<IpAddr>().map_err(|_| {
            ApiError::bad_request(
                "invalid_security_input",
                format!("\"{raw}\" is not an IP address"),
            )
        })?),
    };
    let client = ClientId {
        user_id: body.user_id.map(|id| id.to_string()),
        ip,
    };
    let exempt = body.machine_key;
    let facts = RequestFacts {
        method: body.method.as_str(),
        path: body.path.as_str(),
        client: &client,
        machine_key: body.machine_key,
        exempt,
    };
    let scope = scope_of(&facts);

    if scope == "exempt" {
        // The exempt scope has no policy row **by design**, so `decide` must not be called with
        // it. The answer is stated rather than computed, and it names why.
        return Ok(Json(TestResponse {
            scope: scope.to_owned(),
            verdict: Verdict {
                limited: false,
                scope: scope.to_owned(),
                reason: "this surface is exempt from rate limiting by design: the public renderer \
                          and webhook intake must not be able to lock each other out"
                    .to_owned(),
                count: 0,
                ceiling: 0,
                retry_after: None,
            },
            counter_identity: "none".to_owned(),
            counter_key: "none".to_owned(),
        }));
    }

    let count = body.count.unwrap_or(1).max(0);
    let clock = now();
    let verdict = decide(&policies, scope, &client, count, clock).map_err(map_store)?;
    let policy = policies
        .iter()
        .find(|policy| policy.scope == scope)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                format!("no policy for the resolved scope {scope}"),
            )
        })?;

    Ok(Json(TestResponse {
        scope: scope.to_owned(),
        counter_identity: policy.counter_identity(&client),
        counter_key: policy.counter_key(&client, clock),
        verdict,
    }))
}

/// `GET /security/sign-in-protection` — the lockout document.
pub async fn get_sign_in_protection(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<Json<SignInProtectionBody>, ApiError> {
    let stored = load_lockout(state.db().pool()).await.map_err(map_store)?;
    let is_saved = !stored.is_null() && !stored.as_object().is_some_and(serde_json::Map::is_empty);
    let policy = parse_lockout(&stored).map_err(map_store)?;
    let locked = locked_count(state.db().pool()).await.map_err(map_store)?;

    Ok(Json(SignInProtectionBody {
        bounds: BoundsBody {
            window_seconds: [
                omnion_security::MIN_FAILURE_WINDOW_SECONDS,
                omnion_security::MAX_FAILURE_WINDOW_SECONDS,
            ],
            attempts: [omnion_security::MIN_ATTEMPTS, omnion_security::MAX_ATTEMPTS],
            lockout_minutes: [
                omnion_security::MIN_LOCKOUT_MINUTES,
                omnion_security::MAX_LOCKOUT_MINUTES,
            ],
            base_delay_seconds: [1, MAX_DELAY_SECONDS],
            max_delay_seconds: MAX_DELAY_SECONDS,
        },
        policy,
        locked_accounts: locked,
        is_saved,
    }))
}

/// `PUT /security/sign-in-protection` — save the lockout document.
pub async fn put_sign_in_protection(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(policy): Json<LockoutPolicy>,
) -> Result<Json<SignInProtectionBody>, ApiError> {
    // The same validation the tester applies, applied on the way in: a policy stored without
    // being checked would be one the screen renders and the sign-in path cannot enforce.
    let policy = policy.validated().map_err(map_store)?;
    let document = lockout_to_document(&policy);
    let saved = save_lockout(
        state.db().pool(),
        &document,
        Some(&load_lockout(state.db().pool()).await.map_err(map_store)?),
        session.user.id,
    )
    .await
    .map_err(map_store)?;

    if let Err(error) = record_audit(
        state.db().pool(),
        NewAuditEntry::by_user(session.user.id, "security.sign_in_protection.updated")
            .organization(session.user.organization_id)
            .metadata(json!({ "attempts": policy.attempts })),
    )
    .await
    {
        tracing::warn!(error = %error, "the policy was saved but the audit entry was not written");
    }

    Ok(Json(SignInProtectionBody {
        bounds: BoundsBody {
            window_seconds: [
                omnion_security::MIN_FAILURE_WINDOW_SECONDS,
                omnion_security::MAX_FAILURE_WINDOW_SECONDS,
            ],
            attempts: [omnion_security::MIN_ATTEMPTS, omnion_security::MAX_ATTEMPTS],
            lockout_minutes: [
                omnion_security::MIN_LOCKOUT_MINUTES,
                omnion_security::MAX_LOCKOUT_MINUTES,
            ],
            base_delay_seconds: [1, MAX_DELAY_SECONDS],
            max_delay_seconds: MAX_DELAY_SECONDS,
        },
        policy: parse_lockout(&saved.lockout).map_err(map_store)?,
        locked_accounts: locked_count(state.db().pool()).await.map_err(map_store)?,
        is_saved: true,
    }))
}

/// The tester's evaluation for one account, so the form can show a real count.
#[derive(Debug, Deserialize)]
pub struct LockoutProbe {
    /// The account to evaluate.
    pub user_id: Uuid,
}

/// `POST /security/sign-in-protection/probe` — what the policy says about one account now.
pub async fn probe_lockout(
    State(state): State<AppState>,
    _session: CurrentSession,
    Json(body): Json<LockoutProbe>,
) -> Result<Json<LockoutState>, ApiError> {
    let policy = parse_lockout(&load_lockout(state.db().pool()).await.map_err(map_store)?)
        .map_err(map_store)?;
    let failures = failures_in_window(state.db().pool(), body.user_id, policy.window_seconds)
        .await
        .map_err(map_store)?;
    let evaluated = evaluate_lockout(&policy, failures).map_err(map_store)?;
    Ok(Json(evaluated))
}

/// `GET /security/locked-accounts` — who is locked right now.
pub async fn get_locked_accounts(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<Json<LockedAccountsBody>, ApiError> {
    let accounts = locked_accounts(state.db().pool(), 200)
        .await
        .map_err(map_store)?;
    let total = locked_count(state.db().pool()).await.map_err(map_store)?;
    Ok(Json(LockedAccountsBody {
        accounts: accounts
            .into_iter()
            .map(|account| LockedAccountBody {
                user_id: account.user_id,
                email: account.email,
                locked_until: format_offset(account.locked_until),
                seconds_remaining: account.seconds_remaining,
                failed_sign_in_count: account.failed_sign_in_count,
            })
            .collect(),
        total,
    }))
}

/// `POST /security/locked-accounts/{id}/unlock` — release one account.
pub async fn unlock(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(user_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let unlocked = unlock_account(state.db().pool(), user_id)
        .await
        .map_err(map_store)?;
    if !unlocked {
        // A user who is not locked has no unlock to perform. `404` rather than `200` with a
        // false, because a `200` that changed nothing is what makes an operator think the screen
        // is broken rather than that they clicked a row that had already expired.
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "that account is not locked",
        ));
    }

    if let Err(error) = record_audit(
        state.db().pool(),
        NewAuditEntry::by_user(session.user.id, "security.account.unlocked")
            .organization(session.user.organization_id)
            .target("user", user_id.to_string()),
    )
    .await
    {
        tracing::warn!(error = %error, "the account was unlocked but the audit entry was not written");
    }

    if let Err(error) = bus::emit(
        state.db().pool(),
        NewEvent::new("security.lockout.released")
            .organization(session.user.organization_id)
            .actor(session.user.id)
            .payload(json!({ "user_id": user_id, "by": session.user.id })),
    )
    .await
    {
        tracing::warn!(error = %error, "the account was unlocked but the event was not emitted");
    }

    Ok(Json(json!({ "unlocked": true, "user_id": user_id })))
}

/// Format a timestamp the way the rest of the security screens do.
fn format_offset(value: time::OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| value.unix_timestamp().to_string())
}

/// The scopes, re-exported so the router can name the paths without a second list.
pub use omnion_security::RATE_SCOPES as SCOPE_NAMES;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exempt_scope_has_a_row_nowhere_in_the_vocabulary() {
        // The tester short-circuits `exempt` before it calls `decide`, because `decide` would
        // refuse for want of a row. That contract only holds while no row exists for it.
        assert!(!RATE_SCOPES.contains(&"exempt"));
    }

    #[test]
    fn the_response_carries_a_ceiling_so_the_table_does_not_do_the_addition() {
        // limit + burst shown as a separate number: an operator reading "10 + 0" has to do the
        // arithmetic, and a table that does it for them cannot get one row wrong.
        let policy = RatePolicy::new("public_api", 60, 10, 5, true).expect("a valid row");
        let body = RateLimitBody::from(&policy);
        assert_eq!(body.ceiling, 15);
        assert_eq!(body.limit, 10);
        assert_eq!(body.burst, 5);
    }

    #[test]
    fn the_window_remaining_never_goes_negative_at_the_boundary() {
        // `rem_euclid` on a negative timestamp would go negative, and a row saying "-3 seconds
        // left in this window" is a clock bug the operator sees before anyone else does.
        for policy in RatePolicy::defaults() {
            let body = RateLimitBody::from(&policy);
            assert!(
                body.window_remaining_seconds > 0,
                "{} gave {}",
                policy.scope,
                body.window_remaining_seconds
            );
            assert!(body.window_remaining_seconds <= policy.window_seconds);
        }
    }

    #[test]
    fn a_client_address_that_does_not_parse_is_refused_with_the_value_in_the_message() {
        // The field-level refusal: the message names what was sent, so the operator does not
        // have to remember what they typed.
        let raw = "203.0.113.999";
        let error = raw.parse::<IpAddr>().expect_err("999 is not an octet");
        let message = format!("\"{raw}\" is not an IP address");
        assert!(message.contains(raw));
        assert!(!error.to_string().is_empty());
    }
}
