//! HTTP route table for the Omnion API.
//!
//! API routes are versioned under `/api/v1` from the first release; operational endpoints
//! (`/healthz`, `/readyz`) stay unversioned so probes never depend on the API version.

pub mod auth;
pub mod health;
pub mod me;
pub mod readyz;

use axum::Router;
use axum::routing::{get, post};

use crate::state::AppState;

/// Build the application router around the shared [`AppState`].
pub fn router(state: AppState) -> Router {
    let v1 = Router::new()
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/me", get(me::me));

    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(readyz::readyz))
        .nest("/api/v1", v1)
        .with_state(state)
}
