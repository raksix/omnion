//! The background search indexer.
//!
//! `main.rs` spawns this task when the indexer is enabled (`OMNION_SEARCH_RUNNER`, default on).
//! Each tick reads the events recorded since the last one and applies them to the search index —
//! a page published a second ago answers a search without anyone clicking "reindex". The knobs
//! are `OMNION_SEARCH_POLL_MS` and `OMNION_SEARCH_BATCH`.
//!
//! Nothing here remembers anything: the cursor, the documents and the bus are rows. A tick that
//! cannot reach the database is logged and the next one picks the work up, because an event that
//! has not been applied is still on the bus with its id above the cursor (docs/requests/REQ-002).

use std::time::Duration as StdDuration;

use omnion_search::indexer;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::state::AppState;

/// Start the indexer; the returned handle is kept by the binary (and ends with the process).
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let config = state.config().search.clone();
    // The floor keeps a misconfigured poll from spinning the runner.
    let poll_ms = config.poll_ms.max(100);
    let batch = i64::try_from(config.batch).unwrap_or(200);

    tracing::info!(poll_ms, batch, "search indexer started");

    tokio::spawn(async move {
        // A never-advanced cursor is pointed at the end of the bus before the first drain, so a
        // fresh installation watches forward and an existing one keeps its position.
        match indexer::seed_cursor(state.db().pool()).await {
            Ok(Some(cursor)) => {
                tracing::info!(cursor, "search cursor seeded to the end of the bus");
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(error = %error, "the search cursor could not be seeded");
            }
        }

        let mut ticks = tokio::time::interval(StdDuration::from_millis(poll_ms));
        // A slow tick must not turn into a burst of catch-up ticks: the events are still on the
        // bus, so the next tick applies them.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick fires immediately; boot has its own work to do.
        ticks.tick().await;

        loop {
            ticks.tick().await;
            match indexer::drain(state.db().pool(), batch).await {
                Ok(report) if !report.is_idle() => {
                    tracing::debug!(?report, "search indexer tick");
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "search indexer tick failed");
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use omnion_core::config::SearchConfig;

    #[test]
    fn the_indexer_polls_fast_enough_to_feel_immediate() {
        let defaults = SearchConfig::default();
        assert!(defaults.poll_ms <= 5_000, "a publish should not wait long");
        assert!(defaults.batch >= 1);
    }
}
