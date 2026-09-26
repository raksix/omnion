//! The background analytics rollup worker.
//!
//! `main.rs` spawns this task when the worker is enabled (`OMNION_ANALYTICS_RUNNER`, default
//! on). Each tick rebuilds the recent hourly and daily buckets from the raw rows — a pageview
//! recorded a second ago shows up in a report after the next tick — and prunes hourly buckets
//! that fell out of the 48-hour window (docs/requests/REQ-007).
//!
//! Nothing here remembers anything: a bucket run is idempotent (it deletes its own rows and
//! writes what it computes), so a tick that cannot reach the database is logged and the next
//! one recomputes the same buckets from the same rows. The knobs are `OMNION_ANALYTICS_POLL_MS`.

use std::time::Duration as StdDuration;

use omnion_module_analytics::rollup;
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::state::AppState;

/// Start the rollup worker; the returned handle is kept by the binary (and ends with the
/// process).
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let poll_ms = state.config().analytics.poll_ms.max(1_000);

    tracing::info!(poll_ms, "analytics rollup worker started");

    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(StdDuration::from_millis(poll_ms));
        // A slow tick must not turn into a burst of catch-up ticks: the raw rows are still
        // there, so the next tick rolls the same buckets up again.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick fires immediately; boot has its own work to do.
        ticks.tick().await;

        loop {
            ticks.tick().await;
            match rollup::tick(state.db().pool(), OffsetDateTime::now_utc()).await {
                Ok(report) if !report.is_idle() => {
                    tracing::debug!(?report, "analytics rollup tick");
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "analytics rollup tick failed");
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use omnion_core::config::AnalyticsConfig;

    #[test]
    fn the_worker_polls_fast_enough_for_a_fresh_pageview_to_show_up() {
        let defaults = AnalyticsConfig::default();
        assert!(
            defaults.poll_ms <= 30_000,
            "a rollup should not lag a minute behind"
        );
        assert!(
            defaults.collect_per_minute >= 60,
            "a page needs room for its own beacon"
        );
    }
}
