//! Session endpoints: `POST /api/v1/auth/login` and `POST /api/v1/auth/logout`.

use axum::Json;
use axum::extract::State;
use axum::http::header::{HeaderValue, SET_COOKIE, USER_AGENT};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use omnion_identity::authentication::{self, AuthOutcome};
use omnion_identity::sessions::{self, SESSION_TTL_SECONDS};
use serde::{Deserialize, Serialize};

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
}

/// Sign in with email and password; the response carries the session cookie.
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

    let outcome =
        authentication::authenticate(state.db().pool(), &body.email, &body.password).await?;
    let user = match outcome {
        AuthOutcome::Authenticated(user) => user,
        AuthOutcome::InvalidCredentials => {
            return Err(ApiError::unauthorized(
                "invalid_credentials",
                "email or password is incorrect",
            ));
        }
        AuthOutcome::AccountDisabled { .. } => {
            return Err(ApiError::forbidden(
                "account_disabled",
                "this account is not active",
            ));
        }
    };

    let user_agent = headers
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let ip_address = client.as_text();

    let (session, token) = sessions::create_session(
        state.db().pool(),
        user.id,
        user_agent.as_deref(),
        ip_address.as_deref(),
    )
    .await?;

    tracing::info!(user_id = %user.id, session_id = %session.id, "session created");

    let secure = !state.config().env.is_development();
    let cookie = cookies::session_cookie(&token, SESSION_TTL_SECONDS, secure);

    let mut response = Json(LoginResponse {
        user: UserBody::from(&user),
    })
    .into_response();
    response.headers_mut().insert(
        SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("session cookie is valid header text"),
    );
    Ok(response)
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
    response.headers_mut().insert(
        SET_COOKIE,
        HeaderValue::from_str(&cookies::cleared_session_cookie(secure))
            .expect("cleared cookie is valid header text"),
    );
    Ok(response)
}
