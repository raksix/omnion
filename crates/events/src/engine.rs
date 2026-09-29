//! The delivery runner: claim due deliveries, send them, record what happened.
//!
//! The runner is stateless — the queue is the state. One tick settles the deliveries of
//! endpoints that were switched off while the queue waited, claims the due rows, sends them one
//! by one, and writes each outcome back: delivered, retried with backoff, or failed when the
//! attempts run out. A tick that cannot reach the database is logged and the next one picks the
//! work up, because everything it needs is a durable row (`apps/api/src/event_runner.rs` drives
//! this loop in the API process).

use std::time::{Duration as StdDuration, Instant as StdInstant};

use sqlx::PgPool;
use time::Duration;

use crate::error::Result;
use crate::model::DeliveryJob;
use crate::sender::DeliveryOutcome;
use crate::{sender, store};

/// How much of a failure travels into the delivery row.
const MAX_ERROR_CHARS: usize = 500;

/// How long one delivery's HTTP round trip took, in whole milliseconds.
///
/// A receiver that never answers is bounded by the request timeout (10s by default), so this
/// cannot grow without limit; it is still clamped, because the delivery row's own check demands
/// a non-negative integer and a saturated conversion would be a value the column refuses.
fn elapsed_millis(started: StdInstant) -> Option<i32> {
    let millis = started
        .elapsed()
        .as_millis()
        .min(i32::MAX as u128);
    i32::try_from(millis).ok()
}

/// Knobs of one delivery tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerConfig {
    /// Deliveries one tick may claim.
    pub batch: i64,
    /// How long a claim is exclusive; an older claim was abandoned by a stopped process.
    pub lease_seconds: u64,
    /// How long one delivery's HTTP round trip may take.
    pub request_timeout: StdDuration,
    /// First retry backoff.
    pub retry_base: Duration,
    /// Ceiling of the retry backoff.
    pub retry_max: Duration,
}

impl Default for RunnerConfig {
    /// Development-friendly defaults: a small batch, a two-minute lease, ten seconds per
    /// receiver, and a 15s → 15min backoff ladder.
    fn default() -> Self {
        Self {
            batch: 20,
            lease_seconds: 120,
            request_timeout: StdDuration::from_secs(10),
            retry_base: Duration::seconds(15),
            retry_max: Duration::minutes(15),
        }
    }
}

/// What one tick did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunReport {
    /// Queued deliveries settled because their endpoint was switched off.
    pub cancelled: u64,
    /// Deliveries claimed by this tick.
    pub claimed: usize,
    /// Deliveries a receiver accepted.
    pub delivered: usize,
    /// Deliveries that failed and were scheduled for another attempt.
    pub retried: usize,
    /// Deliveries that ran out of attempts.
    pub failed: usize,
}

impl RunReport {
    /// `true` when the tick had nothing to do.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.cancelled == 0 && self.claimed == 0
    }
}

/// Drain the due deliveries once.
pub async fn run_due(
    pool: &PgPool,
    client: &reqwest::Client,
    config: &RunnerConfig,
) -> Result<RunReport> {
    let mut report = RunReport::default();
    let lease = config.lease_seconds as f64;

    report.cancelled = store::cancel_pending_for_disabled(pool, lease).await?;

    let jobs = store::claim_due(pool, config.batch, lease).await?;
    report.claimed = jobs.len();

    for job in jobs {
        deliver_one(pool, client, config, job, &mut report).await?;
    }

    Ok(report)
}

/// Send one claimed delivery and write back what happened.
async fn deliver_one(
    pool: &PgPool,
    client: &reqwest::Client,
    config: &RunnerConfig,
    job: DeliveryJob,
    report: &mut RunReport,
) -> Result<()> {
    // Timed around the send, not around the write: the number an operator wants is how long
    // their *receiver* took, and folding the database round trip into it would make a fast
    // receiver on a busy database look slow.
    let started = StdInstant::now();
    let outcome = sender::deliver(client, &job).await;
    let duration_ms = elapsed_millis(started);

    match outcome {
        DeliveryOutcome::Delivered { status } => {
            store::mark_delivered(
                pool,
                job.delivery_id,
                Some(i32::from(status)),
                duration_ms,
            )
            .await?;
            report.delivered += 1;

            tracing::info!(
                delivery_id = %job.delivery_id,
                endpoint = %job.endpoint_name,
                event = %job.event_name,
                status,
                attempt = job.attempts,
                "webhook delivered"
            );
        }
        DeliveryOutcome::Failed { status, message } => {
            let message = sender::trim(&message, MAX_ERROR_CHARS);

            if job.attempts < job.max_attempts {
                let delay = store::retry_delay(job.attempts, config.retry_base, config.retry_max);
                store::mark_retry(
                    pool,
                    job.delivery_id,
                    status.map(i32::from),
                    &message,
                    store::now() + delay,
                    duration_ms,
                )
                .await?;
                report.retried += 1;

                tracing::info!(
                    delivery_id = %job.delivery_id,
                    endpoint = %job.endpoint_name,
                    event = %job.event_name,
                    attempt = job.attempts,
                    of = job.max_attempts,
                    delay_ms = delay.whole_milliseconds() as i64,
                    error = %message,
                    "webhook delivery failed; another attempt is queued"
                );
            } else {
                store::mark_failed(
                    pool,
                    job.delivery_id,
                    status.map(i32::from),
                    &message,
                    duration_ms,
                )
                .await?;
                report.failed += 1;

                // Giving up is the one delivery outcome an operator cannot see anywhere else:
                // the delivery row shows `failed`, the endpoint stays enabled, and nothing says
                // out loud that a fact the organization subscribed to never arrived. An event
                // subscribed to nothing else (there is no delivery of this event that can carry
                // it \u2014 the endpoint that failed is the one that would have received it) is
                // the honest way to surface it, and it reaches any *other* endpoint of the
                // organization watching its own bus.
                //
                // The failure is recorded after the delivery is marked failed rather than in the
                // same transaction: this one must not be able to take down the runner, or a
                // receiver that stays broken stops the whole queue from draining.
                if let Err(error) = crate::bus::emit(
                    pool,
                    crate::NewEvent::new("webhook.delivery.failed")
                        .organization(job.organization_id)
                        .site(job.site_id)
                        .actor(job.actor_user_id)
                        .payload(serde_json::json!({
                            "delivery_id": job.delivery_id,
                            "endpoint_id": job.endpoint_id,
                            "endpoint_name": job.endpoint_name,
                            "event_name": job.event_name,
                            "attempts": job.attempts,
                            "status": status.map(i32::from),
                            "reason": message,
                        })),
                )
                .await
                {
                    tracing::error!(
                        delivery_id = %job.delivery_id,
                        error = %error,
                        "the delivery-failed event could not be recorded"
                    );
                }

                tracing::warn!(
                    delivery_id = %job.delivery_id,
                    endpoint = %job.endpoint_name,
                    event = %job.event_name,
                    attempts = job.attempts,
                    status = status.unwrap_or(0),
                    error = %message,
                    "webhook delivery ran out of attempts"
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_idle_tick_says_so() {
        assert!(RunReport::default().is_idle());
        assert!(
            !RunReport {
                claimed: 1,
                ..RunReport::default()
            }
            .is_idle()
        );
        assert!(
            !RunReport {
                cancelled: 2,
                ..RunReport::default()
            }
            .is_idle()
        );
    }

    #[test]
    fn a_measured_round_trip_is_never_negative() {
        // A monotonic clock cannot go backwards, so the only way this is exercised is the
        // clamp: a value the column's own check would refuse must not reach the column.
        assert!(elapsed_millis(StdInstant::now()).is_some_and(|ms| ms >= 0));
    }

    #[test]
    fn the_defaults_are_the_documented_ladder() {
        let config = RunnerConfig::default();
        assert_eq!(config.batch, 20);
        assert_eq!(config.lease_seconds, 120);
        assert_eq!(config.request_timeout, StdDuration::from_secs(10));
        assert_eq!(config.retry_base, Duration::seconds(15));
        assert_eq!(config.retry_max, Duration::minutes(15));
    }
}
