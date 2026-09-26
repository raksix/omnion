//! The background workflow runner.
//!
//! `main.rs` spawns this task when the engine is enabled (`OMNION_WORKFLOW_RUNNER`, default
//! on). It ticks the engine on the configured cadence — a fast tick that advances due steps
//! (sub-second wakeups, docs/09-N8N-TEARDOWN.md §13 lesson 19) and a slower sweep that
//! re-queues overdue waits and settles runs an earlier process left open.
//!
//! Nothing here decides what work exists: the store does. A tick that cannot reach the
//! database is logged and retried on the next one, because the work it left behind is durable
//! rows, not an in-memory queue.

use std::time::Duration as StdDuration;

use omnion_core::config::WorkflowConfig;
use omnion_workflows::engine::{self, RunnerConfig};
use time::Duration;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::state::AppState;

/// Translate the process configuration into the engine's own knobs.
#[must_use]
pub fn runner_config(config: &WorkflowConfig) -> RunnerConfig {
    RunnerConfig {
        tick: Duration::milliseconds(config.tick_ms as i64),
        sweep: Duration::seconds(config.sweep_seconds as i64),
        batch: config.batch,
        scheduler_batch: config.scheduler_batch as i64,
        retry_base: Duration::milliseconds(config.retry_base_ms as i64),
        retry_max: Duration::milliseconds(config.retry_max_ms as i64),
        // The sweep's batch is the engine's own default: it is a repair path, not a tuning knob.
        ..RunnerConfig::default()
    }
}

/// Start the runner; the returned handle is kept by the binary (and ends with the process).
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let config = runner_config(&state.config().workflows);

    // `tokio` works in `std` durations; the engine works in `time` durations. The floor keeps
    // a misconfigured tick from spinning the runner.
    let tick_ms = (config.tick.whole_milliseconds() as i64).max(50) as u64;
    let sweep_ms = (config.sweep.whole_milliseconds() as i64).max(tick_ms as i64) as u64;

    tracing::info!(
        tick_ms,
        sweep_ms,
        batch = config.batch,
        scheduler_batch = config.scheduler_batch,
        "workflow runner started"
    );

    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(StdDuration::from_millis(tick_ms));
        let mut sweeps = tokio::time::interval(StdDuration::from_millis(sweep_ms));
        // A slow tick must not turn into a burst of catch-up ticks: the next tick is enough,
        // because every due step is still in the store.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        sweeps.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Both intervals fire immediately; boot has its own work to do, so wait for the two
        // first ticks instead of starting with one.
        ticks.tick().await;
        sweeps.tick().await;

        loop {
            tokio::select! {
                _ = ticks.tick() => {
                    match engine::tick(state.db().pool(), &config).await {
                        Ok(report) if !report.is_idle() => {
                            tracing::debug!(?report, "workflow tick");
                        }
                        Ok(_) => {}
                        Err(error) => {
                            tracing::warn!(error = %error, "workflow tick failed");
                        }
                    }
                }
                _ = sweeps.tick() => {
                    match engine::sweep(state.db().pool(), &config).await {
                        Ok(report)
                            if report.waits_resolved > 0
                                || report.steps_reclaimed > 0
                                || report.executions_settled > 0 =>
                        {
                            tracing::info!(?report, "workflow sweep");
                        }
                        Ok(_) => {}
                        Err(error) => {
                            tracing::warn!(error = %error, "workflow sweep failed");
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runner_config_follows_the_process_config() {
        let process = WorkflowConfig {
            runner_enabled: true,
            tick_ms: 250,
            sweep_seconds: 5,
            batch: 4,
            scheduler_batch: 3,
            retry_base_ms: 100,
            retry_max_ms: 900,
        };

        let config = runner_config(&process);
        assert_eq!(config.tick, Duration::milliseconds(250));
        assert_eq!(config.sweep, Duration::seconds(5));
        assert_eq!(config.batch, 4);
        assert_eq!(config.scheduler_batch, 3);
        assert_eq!(config.retry_base, Duration::milliseconds(100));
        assert_eq!(config.retry_max, Duration::milliseconds(900));
        assert_eq!(
            config.sweep_batch,
            RunnerConfig::default().sweep_batch,
            "the sweep batch stays the engine default"
        );
    }

    #[test]
    fn the_development_defaults_match_the_engine() {
        let config = runner_config(&WorkflowConfig::default());
        assert_eq!(config.tick, RunnerConfig::default().tick);
        assert_eq!(config.sweep, RunnerConfig::default().sweep);
        assert_eq!(config.batch, RunnerConfig::default().batch);
    }
}
