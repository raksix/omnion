//! The background webhook delivery runner.
//!
//! `main.rs` spawns this task when the event runner is enabled (`OMNION_EVENTS_RUNNER`, default
//! on). It ticks the delivery queue on the configured cadence: claim the due deliveries, POST
//! each one signed, record the outcome and schedule the retry backoff (docs/BUILD-BACKLOG.md
//! P12). Nothing here decides what work exists — the queue does; a tick that cannot reach the
//! database is logged and retried on the next one, because the work it left behind is durable
//! rows, not an in-memory queue.

use std::time::Duration as StdDuration;

use omnion_core::config::EventsConfig;
use omnion_events::engine::{self, RunnerConfig};
use omnion_events::sender;
use time::Duration;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::state::AppState;

/// Translate the process configuration into the engine's own knobs.
#[must_use]
pub fn runner_config(config: &EventsConfig) -> RunnerConfig {
    RunnerConfig {
        batch: config.batch as i64,
        lease_seconds: config.lease_seconds,
        request_timeout: StdDuration::from_millis(config.request_timeout_ms),
        retry_base: Duration::milliseconds(config.retry_base_ms as i64),
        retry_max: Duration::milliseconds(config.retry_max_ms as i64),
    }
}

/// Start the runner; the returned handle is kept by the binary (and ends with the process).
///
/// `None` means the runner could not start at all — an HTTP client that cannot be built would
/// otherwise turn every claimed delivery into a failure, so the platform refuses to drain the
/// queue with it and says so.
#[must_use]
pub fn spawn(state: AppState) -> Option<JoinHandle<()>> {
    let process = state.config().events.clone();
    let config = runner_config(&process);
    let poll_ms = process.poll_ms.max(50);

    let client = match sender::client(config.request_timeout) {
        Ok(client) => client,
        Err(error) => {
            tracing::error!(
                error = %error,
                "the webhook delivery runner could not build its HTTP client"
            );
            return None;
        }
    };

    tracing::info!(
        poll_ms,
        batch = config.batch,
        lease_seconds = config.lease_seconds,
        request_timeout_ms = config.request_timeout.as_millis() as i64,
        "webhook delivery runner started"
    );

    Some(tokio::spawn(async move {
        let mut ticks = tokio::time::interval(StdDuration::from_millis(poll_ms));
        // A slow tick must not turn into a burst of catch-up ticks: the next tick is enough,
        // because every due delivery is still in the store.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick fires immediately; boot has its own work to do, so wait for one.
        ticks.tick().await;

        loop {
            ticks.tick().await;
            match engine::run_due(state.db().pool(), &client, &config).await {
                Ok(report) if !report.is_idle() => {
                    tracing::info!(?report, "webhook delivery tick");
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "webhook delivery tick failed");
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runner_config_follows_the_process_config() {
        let process = EventsConfig {
            runner_enabled: true,
            poll_ms: 250,
            batch: 7,
            lease_seconds: 30,
            request_timeout_ms: 1_500,
            retry_base_ms: 100,
            retry_max_ms: 900,
        };

        let config = runner_config(&process);
        assert_eq!(config.batch, 7);
        assert_eq!(config.lease_seconds, 30);
        assert_eq!(config.request_timeout, StdDuration::from_millis(1_500));
        assert_eq!(config.retry_base, Duration::milliseconds(100));
        assert_eq!(config.retry_max, Duration::milliseconds(900));
    }

    #[test]
    fn the_development_defaults_match_the_engine() {
        let config = runner_config(&EventsConfig::default());
        assert_eq!(config.batch, RunnerConfig::default().batch);
        assert_eq!(config.lease_seconds, RunnerConfig::default().lease_seconds);
        assert_eq!(
            config.request_timeout,
            RunnerConfig::default().request_timeout
        );
        assert_eq!(config.retry_base, RunnerConfig::default().retry_base);
        assert_eq!(config.retry_max, RunnerConfig::default().retry_max);
    }
}
