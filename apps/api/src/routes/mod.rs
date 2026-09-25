//! HTTP route table for the Omnion API.

pub mod health;

use axum::{Router, routing::get};

use crate::state::AppState;

/// Build the application router.
///
/// API routes are versioned under `/api/v1` from the first release; operational
/// endpoints such as `/healthz` stay unversioned so probes never depend on the API version.
pub fn router() -> Router {
    Router::new()
        .route("/healthz", get(health::healthz))
        .with_state(AppState::default())
}
