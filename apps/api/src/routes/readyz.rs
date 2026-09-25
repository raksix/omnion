//! Readiness endpoint.
//!
//! `/healthz` answers "is the process alive?"; `/readyz` answers "can it serve traffic?" and
//! is what orchestrators (and load balancers) should watch (docs/02-ARCHITECTURE.md).

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use omnion_core::CoreError;
use serde::Serialize;

use crate::state::AppState;

/// Payload returned by `GET /readyz`.
#[derive(Debug, Serialize)]
pub struct ReadyResponse {
    /// `true` only when every dependency answered.
    pub ok: bool,
    /// Service identifier.
    pub service: &'static str,
    /// Running version.
    pub version: &'static str,
    /// Per-dependency results.
    pub checks: BTreeMap<&'static str, Check>,
}

/// One dependency check inside the readiness payload.
#[derive(Debug, Serialize)]
pub struct Check {
    /// `ok` or `unavailable`.
    pub status: &'static str,
    /// Failure detail. Only included in development deployments, so production readiness
    /// output cannot leak connection details.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Check {
    fn from_result(result: Result<(), CoreError>, expose_details: bool) -> Self {
        match result {
            Ok(()) => Self {
                status: "ok",
                error: None,
            },
            Err(err) => {
                let error = if expose_details {
                    Some(err.to_string())
                } else {
                    None
                };
                Self {
                    status: "unavailable",
                    error,
                }
            }
        }
    }
}

/// Readiness probe: `200` when every dependency answers, `503` otherwise.
pub async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    let (database, redis) = tokio::join!(state.db().ping(), state.redis().ping());
    let expose_details = state.config().env.is_development();

    let mut checks = BTreeMap::new();
    checks.insert("database", Check::from_result(database, expose_details));
    checks.insert("redis", Check::from_result(redis, expose_details));

    let ok = checks.values().all(|check| check.status == "ok");
    let build = state.build();
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        status,
        Json(ReadyResponse {
            ok,
            service: build.service,
            version: build.version,
            checks,
        }),
    )
}
