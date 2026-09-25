//! `GET /api/v1/me` — the account behind the current session.

use axum::Json;
use axum::extract::State;
use omnion_identity::sessions;
use serde::Serialize;

use crate::auth::CurrentSession;
use crate::dto::UserBody;
use crate::error::ApiError;
use crate::state::AppState;

/// Response body of `GET /api/v1/me`.
#[derive(Debug, Serialize)]
pub struct MeResponse {
    /// The signed-in account.
    pub user: UserBody,
}

/// Return the signed-in account.
pub async fn me(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<MeResponse>, ApiError> {
    // Session activity is bookkeeping: never fail an authenticated request over it.
    if let Err(err) = sessions::touch_session(state.db().pool(), current.session.id).await {
        tracing::warn!(error = %err, session_id = %current.session.id, "could not record session activity");
    }

    Ok(Json(MeResponse {
        user: UserBody::from(&current.user),
    }))
}
