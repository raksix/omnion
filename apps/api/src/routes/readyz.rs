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
///
/// ## The drain check comes FIRST, and it is not the same question
///
/// A load balancer asks this every couple of seconds. During a shutdown the honest answer to
/// *that* question — "are your dependencies reachable?" — is still yes, the database is right
/// there. What the orchestrator needs to know is a different fact: **stop sending me traffic**,
/// which is about this process's intent rather than about Postgres.
///
/// So the flag is read before anything is pinged, and a `503` is returned without a round trip
/// at all. Order matters for a second reason: pinging during a drain is work the process is
/// trying to stop doing, and a probe that has to wait on a database to learn it should stop
/// accepting traffic delays the very thing the drain is waiting for.
///
/// `/healthz` deliberately does NOT consult this flag — a liveness probe that fails during a
/// drain restarts a process that is behaving perfectly. The asymmetry is the contract
/// (`omnion_telemetry::lifecycle`).
pub async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    let lifecycle = omnion_telemetry::lifecycle::global();
    if lifecycle.is_draining() {
        let build = state.build();
        let mut checks = BTreeMap::new();
        checks.insert(
            "process",
            Check {
                status: "draining",
                error: None,
            },
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ReadyResponse {
                ok: false,
                service: build.service,
                version: build.version,
                checks,
            }),
        );
    }

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
