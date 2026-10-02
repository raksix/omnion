//! Session endpoints: `POST /api/v1/auth/login` and `POST /api/v1/auth/logout`.
//!
//! The sign-in itself lives in `crates/identity` ([`omnion_identity::signin`]) so the API layer
//! only maps its outcome onto HTTP: a locked account, a refused address, a disabled account and
//! a wrong password each answer their own status, and an account that holds a confirmed factor
//! answers with a challenge instead of a cookie — the session starts when
//! `POST /api/v1/auth/mfa/verify` proves the second factor.

use axum::Json;
use axum::extract::State;
use axum::http::header::{HeaderValue, SET_COOKIE, USER_AGENT};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use omnion_events::{NewEvent, bus};
use omnion_identity::devices;
use omnion_identity::security;
use omnion_identity::sessions::{self, NewSession};
use omnion_identity::signin::{self, SignInOutcome};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::client_ip::ClientAddress;
use crate::cookies;
use crate::dto::UserBody;
use crate::error::ApiError;
use crate::state::AppState;

/// Request body of `POST /api/v1/auth/login`.
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    /// Email address of the account.
    pub email: String,
    /// Plaintext password.
    pub password: String,
}

/// Response body of `POST /api/v1/auth/login`.
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    /// The signed-in account.
    pub user: UserBody,
    /// The device this sign-in was attributed to.
    pub device: DeviceBody,
    /// The session lifetime in seconds (the cookie's `Max-Age`).
    pub expires_in: i64,
}

/// The device a sign-in came from, as the panel shows it.
#[derive(Debug, Serialize)]
pub struct DeviceBody {
    /// Device id.
    pub id: uuid::Uuid,
    /// Human label.
    pub label: String,
    /// Operating system family.
    pub platform: String,
    /// Browser family.
    pub browser: String,
    /// Whether the device is trusted right now.
    pub trusted: bool,
}

/// Response of a sign-in that still needs its second factor.
#[derive(Debug, Serialize)]
pub struct MfaChallengeResponse {
    /// Always `true` — the password matched and a factor is required.
    pub mfa_required: bool,
    /// The challenge token to send to `POST /api/v1/auth/mfa/verify` with a code.
    pub challenge: String,
    /// How long the challenge stays valid, in minutes.
    pub expires_in_minutes: i64,
}

/// Sign in with email and password; the response carries the session cookie.
///
/// When the account holds a confirmed second factor, the answer is a challenge — no cookie —
/// and the caller has to complete it with a code.
pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    client: ClientAddress,
    Json(body): Json<LoginRequest>,
) -> Result<Response, ApiError> {
    if body.email.trim().is_empty() || body.password.is_empty() {
        return Err(ApiError::bad_request(
            "invalid_request",
            "email and password are required",
        ));
    }

    let user_agent = headers
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let ip_address = client.as_text();

    let outcome = signin::sign_in(
        state.db().pool(),
        &body.email,
        &body.password,
        ip_address.as_deref(),
        user_agent.as_deref(),
    )
    .await?;

    let user = match outcome {
        SignInOutcome::Authenticated {
            user: _,
            challenge: Some(challenge),
        } => {
            return Ok(Json(MfaChallengeResponse {
                mfa_required: true,
                challenge,
                expires_in_minutes: signin::CHALLENGE_TTL_MINUTES,
            })
            .into_response());
        }
        SignInOutcome::Authenticated {
            user,
            challenge: None,
        } => user,
        SignInOutcome::InvalidCredentials => {
            return Err(ApiError::unauthorized(
                "invalid_credentials",
                "email or password is incorrect",
            ));
        }
        SignInOutcome::AccountDisabled { .. } => {
            return Err(ApiError::forbidden(
                "account_disabled",
                "this account is not active",
            ));
        }
        SignInOutcome::AccountLocked {
            until,
            newly_locked,
            user_id,
            organization_id,
            attempts,
        } => {
            // A lockout is the account's own state, so the answer says so and carries when it
            // ends — the panel shows it instead of "wrong password" forever.
            //
            // The `security.lockout.triggered` event is emitted here, and **only** when this
            // attempt is the one that applied the lock (`newly_locked`). `crates/identity` has
            // no bus handle on purpose — the event bus depends on nothing, and an identity
            // crate that reached for it would put a delivery fan-out on the sign-in path of
            // every deployment. The API layer owns a bus already and this is the only caller of
            // the password path, so the emitter sits here rather than in the crate that
            // applies the lock.
            //
            // A failed emission is a log line, never a `500`: the lock is applied and the caller
            // is already being refused, so failing the request would report a sign-in as broken
            // when the platform did precisely what it was configured to do.
            if newly_locked {
                if let Err(error) = bus::emit(
                    state.db().pool(),
                    NewEvent::new("security.lockout.triggered")
                        .organization(organization_id)
                        .payload(json!({
                            "user_id": user_id,
                            "attempts": attempts,
                            "lockout_minutes": lockout_minutes_from(until),
                        })),
                )
                .await
                {
                    tracing::warn!(
                        error = %error,
                        "the account was locked but the event was not emitted"
                    );
                }
            }

            return Err(ApiError::forbidden(
                "account_locked",
                "too many failed attempts — this account is locked for a while",
            )
            .with_details(json!({
                "retry_after": until.format(&Rfc3339).unwrap_or_default(),
            })));
        }
        SignInOutcome::IpBlocked { reason, rule } => {
            return Err(ApiError::forbidden(
                "address_blocked",
                "sign-ins from this address are refused by the security policy",
            )
            .with_details(json!({ "reason": reason, "rule": rule })));
        }
    };

    start_session(
        &state,
        &user,
        user_agent,
        ip_address,
        vec!["password".to_owned()],
    )
    .await
}

/// How many minutes a lock has left, rounded **up**, never negative.
///
/// The alternative — the difference between two instants, floor-divided — reports a 90-second
/// lock as `1` and a 30-second lock as `0`, so a subscriber reading the payload sees a lockout
/// that "lasts nothing". Rounding up is the honest direction for a countdown: it never claims
/// less time than the account is actually refused, and it never goes negative, which a plain
/// difference can when the lock expires between the two reads.
fn lockout_minutes_from(until: OffsetDateTime) -> i64 {
    lockout_minutes_between(OffsetDateTime::now_utc(), until)
}

/// [`lockout_minutes_from`] against an explicit "now", so the arithmetic can be tested at its
/// boundaries.
///
/// The split is not ceremony. A test that builds `now + 60 seconds` and lets the function read
/// its own clock is a test that computes 59.999 seconds and expects the answer for 60 — it fails
/// or passes by how much of that second the scheduler gave away, which is the same class of
/// defect as asserting a walk's own counter instead of the row it produced. The production
/// caller passes the real clock; only the tests pass a fixed one.
fn lockout_minutes_between(now: OffsetDateTime, until: OffsetDateTime) -> i64 {
    let seconds = until - now;
    if seconds <= time::Duration::ZERO {
        return 0;
    }
    // `Duration::whole_minutes` is still unstable, so the division is spelled out rather than
    // read off the duration — and rounding UP is the whole point, so this is not the
    // truncating `/`.
    (seconds.whole_seconds() + 59) / 60
}

/// End the current session. Idempotent: a request without a session is still a success.
pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if let Some(token) = cookies::session_token(&headers) {
        let revoked = sessions::revoke_session(state.db().pool(), &token).await?;
        tracing::info!(revoked, "session revoked");
    }

    let secure = !state.config().env.is_development();
    let mut response = StatusCode::NO_CONTENT.into_response();
    // Both cookies go, not just the session one: the CSRF token is derived from the session id,
    // so leaving it behind would leave a value the next sign-in overwrites and nothing in
    // between can use.
    for cookie in cookies::signout_cookies(secure) {
        response.headers_mut().append(
            SET_COOKIE,
            HeaderValue::from_str(&cookie).expect("cleared cookie is valid header text"),
        );
    }
    Ok(response)
}

/// Start a session for an account and answer with the cookie.
///
/// This is the shared tail of every sign-in path: the password alone, and the passkey that
/// finishes one (`auth/webauthn/authenticate/complete`). The session it creates is a full one —
/// device, policy lifetimes, auth methods — so a passkey sign-in is indistinguishable from a
/// password sign-in except by the methods it records.
pub(crate) async fn start_session(
    state: &AppState,
    user: &omnion_identity::User,
    user_agent: Option<String>,
    ip_address: Option<String>,
    auth_methods: Vec<String>,
) -> Result<Response, ApiError> {
    start_session_with_body(
        state,
        user,
        user_agent,
        ip_address,
        auth_methods,
        LoginResponse {
            user: UserBody::from(user),
            device: DeviceBody {
                id: Uuid::nil(),
                label: String::new(),
                platform: String::new(),
                browser: String::new(),
                trusted: false,
            },
            expires_in: sessions::SESSION_TTL_SECONDS,
        },
    )
    .await
}

/// The same session, with a body the caller supplies.
///
/// A path that creates an account as part of something else — accepting an invitation
/// (REQ-005) is the one today — needs the session cookie *and* its own answer, not the login
/// body. The cookie, the device bookkeeping and the policy lifetimes are identical either way.
pub(crate) async fn start_session_with_body<T: Serialize>(
    state: &AppState,
    user: &omnion_identity::User,
    user_agent: Option<String>,
    ip_address: Option<String>,
    auth_methods: Vec<String>,
    body: T,
) -> Result<Response, ApiError> {
    let policy = signin::session_policy_for(state.db().pool(), user.organization_id).await?;
    let trust_days = match user.organization_id {
        Some(organization_id) => {
            security::ensure_policy(state.db().pool(), organization_id)
                .await?
                .device_trust_days
        }
        None => 30,
    };

    // Every sign-in registers the device it came from, so the panel can show what a session ran
    // on and a trust window can widen or narrow a "new device" notice.
    let device = devices::upsert(
        state.db().pool(),
        user.id,
        user_agent.as_deref(),
        trust_days,
    )
    .await?;

    let (session, token) = sessions::create_session_with_policy(
        state.db().pool(),
        user.id,
        NewSession {
            user_agent,
            ip_address,
            device_id: Some(device.id),
            auth_methods,
        },
        policy,
    )
    .await?;

    tracing::info!(user_id = %user.id, session_id = %session.id, device_id = %device.id, "session created");

    let secure = !state.config().env.is_development();
    let cookie = cookies::session_cookie(&token, sessions::SESSION_TTL_SECONDS, secure);
    // The token the CSRF layer will ask for. Issued here, in the same response as the session,
    // because a browser that has a session but no token cannot change anything — and a 403 on
    // every save is a far worse first sign-in than a header nobody reads.
    let csrf = cookies::csrf_cookie_for(&session.id, state.config().csrf.as_bytes(), secure);

    let mut response = Json(body).into_response();
    response.headers_mut().insert(
        SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("session cookie is valid header text"),
    );
    if let Some(csrf) = csrf {
        response.headers_mut().append(
            SET_COOKIE,
            HeaderValue::from_str(&csrf).expect("CSRF cookie is valid header text"),
        );
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed instant, so every boundary below is exact rather than a race.
    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("a valid instant")
    }

    #[test]
    fn a_lock_always_reports_at_least_the_minute_it_still_refuses() {
        // The countdown rounds UP, and these are the cases a plain floor-division gets wrong: a
        // 90-second lock is two minutes of refusal, and a 30-second lock is still one whole
        // minute the caller cannot sign in for. A subscriber reading `0` there concludes the
        // account was never really locked.
        //
        // The 60/61 pair is the whole reason the arithmetic exists — 60 seconds is 1 minute and
        // 61 is 2, so a truncating division and a rounding one disagree exactly here, and that
        // is the only place they do.
        assert_eq!(
            lockout_minutes_between(now(), now() + time::Duration::seconds(90)),
            2
        );
        assert_eq!(
            lockout_minutes_between(now(), now() + time::Duration::seconds(30)),
            1
        );
        assert_eq!(
            lockout_minutes_between(now(), now() + time::Duration::seconds(60)),
            1
        );
        assert_eq!(
            lockout_minutes_between(now(), now() + time::Duration::seconds(61)),
            2
        );
        assert_eq!(
            lockout_minutes_between(now(), now() + time::Duration::seconds(1)),
            1
        );
    }

    #[test]
    fn an_expired_lock_reports_zero_and_never_a_negative_count() {
        // The lock can expire between the row read and the emission — `until` is read before
        // the write, and the write is not in the same transaction as the clock. A plain
        // difference of two instants is negative there, and a negative `lockout_minutes` in a
        // payload is a number no receiver can render.
        assert_eq!(
            lockout_minutes_between(now(), now() - time::Duration::hours(1)),
            0
        );
        assert_eq!(lockout_minutes_between(now(), now()), 0);
        assert_eq!(
            lockout_minutes_between(now(), now() - time::Duration::seconds(1)),
            0
        );
    }

    #[test]
    fn a_long_lock_is_reported_in_whole_minutes_not_seconds() {
        // The payload field is named `lockout_minutes`, so the unit is part of the contract: a
        // subscriber that assumed seconds would turn a 15-minute lock into 900,000.
        assert_eq!(
            lockout_minutes_between(now(), now() + time::Duration::minutes(15)),
            15
        );
        assert_eq!(
            lockout_minutes_between(now(), now() + time::Duration::minutes(90)),
            90
        );
    }
}
