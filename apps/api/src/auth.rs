//! Session-backed authentication extractor.
//!
//! Handlers ask for a [`CurrentSession`] and Axum resolves the cookie for them; a request
//! without a valid session is rejected with `401` before the handler body runs.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use omnion_identity::sessions::{self, AuthenticatedSession};

use crate::cookies;
use crate::error::ApiError;
use crate::state::AppState;

/// The authenticated caller of a request.
#[derive(Debug, Clone)]
pub struct CurrentSession {
    /// Account the session belongs to.
    pub user: omnion_identity::User,
    /// Session row (identity of this sign-in).
    pub session: sessions::Session,
    /// Raw session token, for endpoints that end the session (logout).
    pub token: String,
}

impl FromRequestParts<AppState> for CurrentSession {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some(token) = cookies::session_token(&parts.headers) else {
            return Err(ApiError::unauthorized(
                "unauthenticated",
                "sign in to continue",
            ));
        };

        let resolved: Option<AuthenticatedSession> =
            sessions::resolve_session(state.db().pool(), &token).await?;

        let Some(resolved) = resolved else {
            return Err(ApiError::unauthorized(
                "invalid_session",
                "this session is no longer valid — sign in again",
            ));
        };

        Ok(Self {
            user: resolved.user,
            session: resolved.session,
            token,
        })
    }
}
