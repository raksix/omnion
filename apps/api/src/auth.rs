//! Session-backed authentication extractor.
//!
//! Handlers ask for a [`CurrentSession`] and Axum resolves the cookie for them; a request
//! without a valid session is rejected with `401` before the handler body runs. Route guards
//! (`crate::guards`) resolve the session first and hand it over through the request
//! extensions, so a guarded handler needs no extra lookup.

use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
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

impl CurrentSession {
    /// Resolve the session cookie of a request.
    ///
    /// `401 unauthenticated` when no cookie is present, `401 invalid_session` when the token is
    /// unknown, expired or revoked — the two cases are distinguishable on purpose, so a client
    /// can tell "sign in" from "your session ended".
    pub async fn resolve(state: &AppState, headers: &HeaderMap) -> Result<Self, ApiError> {
        let Some(token) = cookies::session_token(headers) else {
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

impl FromRequestParts<AppState> for CurrentSession {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // A guard already resolved this request's session; reuse it instead of querying again.
        if let Some(resolved) = parts.extensions.get::<Self>() {
            return Ok(resolved.clone());
        }

        Self::resolve(state, &parts.headers).await
    }
}

/// The caller of a route that accepts both a session and a machine key.
///
/// The guard (`crate::guards::require_or_machine`) has already authenticated the request and put
/// one of the two principals into the extensions; this extractor only reads it back, so a handler
/// never re-authenticates and a machine request never pretends to be a person. The session is
/// boxed for the same reason as in `Caller`: it is the large variant, and this value is built
/// once per request.
#[derive(Debug, Clone)]
pub enum ApiCaller {
    /// A signed-in session.
    Session(Box<CurrentSession>),
    /// A service account that presented a key.
    Machine(crate::guards::MachinePrincipal),
}

impl ApiCaller {
    /// The organization the caller works in (`None` = platform level).
    #[must_use]
    pub fn organization_id(&self) -> Option<uuid::Uuid> {
        match self {
            Self::Session(session) => session.user.organization_id,
            Self::Machine(machine) => Some(machine.organization_id),
        }
    }

    /// Who to audit the action as (`None` for a machine: it has no account row).
    #[must_use]
    pub fn actor_user_id(&self) -> Option<uuid::Uuid> {
        match self {
            Self::Session(session) => Some(session.user.id),
            Self::Machine(_) => None,
        }
    }
}

impl FromRequestParts<AppState> for ApiCaller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if let Some(session) = parts.extensions.get::<CurrentSession>() {
            return Ok(Self::Session(Box::new(session.clone())));
        }
        if let Some(machine) = parts.extensions.get::<crate::guards::MachinePrincipal>() {
            return Ok(Self::Machine(machine.clone()));
        }

        Err(ApiError::unauthorized(
            "unauthenticated",
            "sign in to continue",
        ))
    }
}
