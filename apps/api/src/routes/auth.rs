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
use omnion_identity::devices;
use omnion_identity::security;
use omnion_identity::sessions::{self, NewSession};
use omnion_identity::signin::{self, SignInOutcome};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::format_description::well_known::Rfc3339;

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
        SignInOutcome::AccountLocked { until } => {
            // A lockout is the account's own state, so the answer says so and carries when it
            // ends — the panel shows it instead of "wrong password" forever.
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

    let mut response = Json(LoginResponse {
        user: UserBody::from(user),
        device: DeviceBody {
            id: device.id,
            label: device.label,
            platform: device.platform,
            browser: device.browser,
            trusted: device
                .trusted_until
                .is_some_and(|until| until > time::OffsetDateTime::now_utc()),
        },
        expires_in: sessions::SESSION_TTL_SECONDS,
    })
    .into_response();
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
