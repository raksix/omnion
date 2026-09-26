//! The background automation matcher.
//!
//! `main.rs` spawns this task when the matcher is enabled (`OMNION_AUTOMATION_RUNNER`, default
//! on). Each tick reads the events recorded since the last one, evaluates the armed rules of
//! their tenants against them, and starts one run per match — a run the workflow engine then
//! advances like any other (the same runner that drives P09's manual and scheduled workflows).
//!
//! Nothing here remembers anything: the cursor, the rules and the runs are rows. A tick that
//! cannot reach the database is logged and the next one picks the work up, because an event that
//! has not been evaluated is still in the bus with its id above the cursor.

use std::time::Duration as StdDuration;

use omnion_automation::matcher;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::state::AppState;

/// Start the matcher; the returned handle is kept by the binary (and ends with the process).
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let config = state.config().automation.clone();
    // The floor keeps a misconfigured poll from spinning the runner.
    let poll_ms = config.poll_ms.max(100);
    let batch = i64::try_from(config.batch).unwrap_or(matcher::DEFAULT_BATCH);

    tracing::info!(poll_ms, batch, "automation matcher started");

    tokio::spawn(async move {
        // Seed a never-advanced cursor to the end of the bus before the first drain: a fresh
        // installation watches forward. Doing it here — before the loop, with the bus already
        // readable — is also what closes the boot window: an event recorded between the seed and
        // the first tick sits above the cursor, so the tick evaluates it.
        match matcher::seed_cursor(state.db().pool()).await {
            Ok(Some(cursor)) => {
                tracing::info!(cursor, "automation cursor seeded to the end of the bus");
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(error = %error, "the automation cursor could not be seeded");
            }
        }

        let mut ticks = tokio::time::interval(StdDuration::from_millis(poll_ms));
        // A slow tick must not turn into a burst of catch-up ticks: the events are still in the
        // bus, so the next tick evaluates them.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick fires immediately; boot has its own work to do.
        ticks.tick().await;

        loop {
            ticks.tick().await;
            match matcher::drain(state.db().pool(), batch).await {
                Ok(report) if !report.is_idle() => {
                    tracing::debug!(?report, "automation tick");
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "automation tick failed");
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_matcher_polls_fast_enough_to_feel_immediate() {
        use omnion_core::config::AutomationConfig;

        let defaults = AutomationConfig::default();
        assert!(defaults.poll_ms <= 5_000, "a match should not wait long");
        assert!(defaults.batch >= 1);
        assert_eq!(
            matcher::DEFAULT_BATCH,
            omnion_core::config::DEFAULT_AUTOMATION_BATCH as i64,
            "the crate default and the process default agree"
        );
    }
}
