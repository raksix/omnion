//! The background AI health probe runner.
//!
//! `main.rs` spawns this task when the runner is enabled (`OMNION_AI_HEALTH_RUNNER`, default
//! on). Each tick samples **every enabled provider** and prunes the history that fell out of the
//! retention window (docs/requests/REQ-097, slice 3).
//!
//! Two decisions are kept here rather than left to the tick:
//!
//! * **The tick calls the same `probe_now` the "Probe now" button calls.** A second copy of the
//!   probe would drift from the first, and the drift would show up as a Health tab whose button
//!   and whose sparkline disagree about what "probed" means. The sample is a real measurement
//!   with its real cost either way.
//! * **One provider's failure does not end the tick.** A dead endpoint is the normal case the
//!   runner exists for, so each provider is probed independently and a failure is recorded as
//!   that provider's own health — not as a failed tick that skips the rest of the chain.
//!
//! Nothing here decides what a status *is*: the verdict is computed by
//! [`omnion_ai_hub::health_status`] over the samples, and this module only collects them.

use std::time::Duration as StdDuration;

use omnion_ai_hub::connection_test::StepStatus;
use omnion_ai_hub::health::HealthStatus;
use omnion_ai_hub::health_store::{self, NewSample};
use omnion_ai_hub::{list_models, test_provider};
use omnion_audit::{NewAuditEntry, record};
use sqlx::PgPool;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use crate::state::AppState;

/// What one tick did, as the caller and the log line describe it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Providers that answered.
    pub ok: usize,
    /// Providers that did not.
    pub failed: usize,
    /// Health transitions announced this tick.
    pub transitions: usize,
    /// Rows the pruner dropped.
    pub pruned: u64,
}

impl TickReport {
    /// `true` when the tick had nothing to do — no provider is connected yet.
    #[must_use]
    pub fn is_idle(self) -> bool {
        self.ok == 0 && self.failed == 0 && self.pruned == 0
    }
}

/// The provider rows the runner samples, in the order it samples them.
///
/// Read by the walkthrough to assert that a provider the operator switched off is not dialled —
/// an inference from the disabled flag that the runner honours.
pub async fn scheduled_providers(pool: &PgPool) -> Result<Vec<Uuid>, omnion_ai_hub::AiHubError> {
    health_store::enabled_providers(pool).await
}

/// What one probe saw: whether the endpoint answered, and the transition it caused.
///
/// Both halves travel together because the tick needs each for a different reason — the outcome
/// is the tick's own count, the transition is the event. Returning only the transition (which is
/// `Some` for a *failed* provider going `unknown → degraded`) made a dead endpoint count as a
/// success, and a runner that reports two healthy providers while one is refusing is a runner
/// nobody can act on.
type Outcome = (bool, Option<(HealthStatus, HealthStatus)>);

/// Sample one provider and record what it saw.
///
/// This is the whole probe: the same five-step connection test the operator sees when they press
/// the button, turned into one sample. The `latency_ms` is the report's own total, because a
/// sample that claimed 0 ms would make every p95 on the Usage and Health tabs a lie.
async fn sample_one(
    pool: &PgPool,
    provider: &omnion_ai_hub::Provider,
) -> Result<Outcome, omnion_ai_hub::AiHubError> {
    let known: Vec<String> = list_models(pool, Some(provider.id))
        .await?
        .into_iter()
        .map(|model| model.model_key)
        .collect();
    let report = test_provider(provider, &known).await;

    let failing = report
        .steps
        .iter()
        .find(|step| matches!(step.status, StepStatus::Failed));

    let sample = NewSample {
        provider_id: provider.id,
        ok: report.ok,
        latency_ms: report.total_ms.clamp(0, i64::from(i32::MAX)) as i32,
        http_status: None,
        error: failing
            .and_then(|step| step.error.clone())
            .or_else(|| (!report.ok).then(|| report.summary.clone())),
    };

    // The provider in the row is the provider in the URL parameter the button passes — same
    // function, so the tick and the click cannot write a sample onto two different verdicts.
    let transition = health_store::probe_now(pool, provider.id, sample).await?;
    Ok((report.ok, transition))
}

/// Run one tick: probe every enabled provider, then prune.
///
/// Exposed (and unit-tested) separately from [`spawn`] so the tick itself can be exercised
/// against a real database without starting a timer.
pub async fn tick(pool: &PgPool, retention_days: i64) -> TickReport {
    let mut report = TickReport::default();

    match health_store::enabled_providers(pool).await {
        Ok(providers) => {
            for provider_id in providers {
                // The chain the operator ranked is read fresh here, because a tick must dial
                // what is *currently* enabled — a provider switched off between listing the ids
                // and reading the row is a provider the operator asked us not to call.
                let provider = match omnion_ai_hub::find_provider(pool, provider_id).await {
                    Ok(Some(provider)) if provider.enabled => provider,
                    // Removed or switched off in the gap: there is nothing left to sample.
                    Ok(_) => continue,
                    Err(error) => {
                        report.failed += 1;
                        tracing::warn!(error = %error, %provider_id, "the health probe could not read a provider");
                        continue;
                    }
                };
                match sample_one(pool, &provider).await {
                    Ok((answered, Some((from, to)))) => {
                        report.transitions += 1;
                        if answered {
                            report.ok += 1;
                        } else {
                            // A failure is still a transition worth announcing — going to
                            // `degraded` is exactly what an automation listens for.
                            report.failed += 1;
                        }
                        // The event is what an automation on "a provider went down" subscribes
                        // to. A tick and a button press must be indistinguishable to it, so both
                        // write the same action with the same payload shape.
                        let entry = NewAuditEntry::system("ai.provider.health_changed")
                            .target("ai_provider", provider.id.to_string())
                            .metadata(serde_json::json!({
                                "name": provider.name,
                                "from": from.as_str(),
                                "to": to.as_str(),
                                "source": "runner",
                            }));
                        if let Err(error) = record(pool, entry).await {
                            tracing::warn!(
                                error = %error,
                                provider = %provider.name,
                                "a health transition could not be recorded"
                            );
                        }
                    }
                    Ok((answered, None)) => {
                        if answered {
                            report.ok += 1;
                        } else {
                            report.failed += 1;
                        }
                    }
                    Err(error) => {
                        // A provider that cannot even be recorded is still a failure of *that*
                        // provider, never a reason to skip the ones after it.
                        report.failed += 1;
                        tracing::warn!(
                            error = %error,
                            provider = %provider.name,
                            "the health probe could not record a sample"
                        );
                    }
                }
            }
        }
        Err(error) => {
            tracing::warn!(error = %error, "the health probe could not list the providers");
        }
    }

    // Pruning in the same tick that wrote the samples: a provider nobody looks at still stops
    // costing rows, and the retention is longer than any window the UI offers so a 30-day Usage
    // range is always answerable from what is on disk.
    match health_store::prune(pool, retention_days).await {
        Ok(pruned) => report.pruned = pruned,
        Err(error) => tracing::warn!(error = %error, "the health probe could not prune"),
    }

    report
}

/// Start the runner; the returned handle is kept by the binary (and ends with the process).
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    // The floor keeps a misconfigured poll from spinning the runner: the sample is a real
    // network call to a real vendor, and a 1 ms cadence is a denial of service with an API key.
    let poll_ms = state.config().ai_hub.poll_ms.max(1_000);
    let retention_days = i64::try_from(state.config().ai_hub.retention_days).unwrap_or(30);

    tracing::info!(poll_ms, retention_days, "AI health probe runner started");

    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(StdDuration::from_millis(poll_ms));
        // A slow tick must not become a burst of catch-up ticks: the samples it would have taken
        // are already stale, and a provider cannot be brought back by probing it ten times at
        // once.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick fires immediately; boot has its own work to do.
        ticks.tick().await;

        loop {
            ticks.tick().await;
            let report = tick(state.db().pool(), retention_days).await;
            if !report.is_idle() {
                tracing::debug!(?report, "AI health probe tick");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tick_with_nothing_connected_is_idle() {
        // The fresh-install case: a runner with no provider must not log a line every minute.
        assert!(TickReport::default().is_idle());
        assert!(
            !TickReport {
                ok: 1,
                ..TickReport::default()
            }
            .is_idle()
        );
        assert!(
            !TickReport {
                pruned: 3,
                ..TickReport::default()
            }
            .is_idle()
        );
        // A provider that failed is real work: it is the tick's whole reason to exist.
        assert!(
            !TickReport {
                failed: 1,
                ..TickReport::default()
            }
            .is_idle()
        );
    }
}
