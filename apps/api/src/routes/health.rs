//! Liveness endpoint.

use axum::{Json, extract::State};
use serde::Serialize;

use crate::state::AppState;

/// Payload returned by `GET /healthz`.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    /// Always `true` when the process is serving requests.
    pub ok: bool,
    /// Service identifier.
    pub service: &'static str,
    /// Running version.
    pub version: &'static str,
}

/// Liveness probe: reports that the process is up and serving requests.
pub async fn healthz(State(state): State<AppState>) -> Json<HealthResponse> {
    let build = state.build();
    Json(HealthResponse {
        ok: true,
        service: build.service,
        version: build.version,
    })
}
