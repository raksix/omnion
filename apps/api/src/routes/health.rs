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
    /// Whether a drain is in progress.
    ///
    /// Reported but **not** acted on: this is a liveness probe, and a liveness probe that fails
    /// during a drain restarts a process that is shutting down exactly as it was asked to. The
    /// field exists so an operator debugging a slow stop can see it without reading logs.
    pub draining: bool,
    /// Requests still running when the drain began.
    pub in_flight: u64,
}

/// Liveness probe: reports that the process is up and serving requests.
///
/// `/livez` is the same handler under a second path, for platforms that expect that name. Two
/// paths, one implementation: an alias that drifts from the endpoint it aliases is a probe that
/// checks something the deployment does not.
pub async fn healthz(State(state): State<AppState>) -> Json<HealthResponse> {
    let build = state.build();
    let lifecycle = omnion_telemetry::lifecycle::global();
    Json(HealthResponse {
        ok: true,
        service: build.service,
        version: build.version,
        draining: lifecycle.is_draining(),
        in_flight: lifecycle.in_flight(),
    })
}
