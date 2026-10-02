//! The content-usage flush worker (REQ-019, slice 3).
//!
//! The read path accumulates in Redis and this worker carries the window into
//! `api_token_usage_daily`. It is a separate module rather than a tick inside
//! `content_meter` because the two have different lifetimes: the meter is called by every request
//! and must not hold anything, while this is called once a minute and may take as long as the
//! database takes.
//!
//! **Nothing here is remembered between ticks.** A flush is read-window → write → clear, and each
//! of those three steps is idempotent with respect to the other two: the write *adds* (so a replay
//! is harmless), the clear happens only after the write succeeded (so a failure loses nothing), and
//! the read is a `SCAN` over a namespace nothing else writes (so a partial scan is just a smaller
//! window this tick). A worker that needed to know what it had already flushed would be a worker
//! whose restart loses or double-counts a day, and this feature runs on every installation for
//! years.
//!
//! **The prune rides along rather than being its own worker.** Usage retention is measured in days
//! and there is exactly one thing to do about a row that is too old, so a separate timer would be
//! a second chance to forget it. It runs once per hour rather than per tick, which is the whole
//! reason the tick counts: pruning every minute on a table that only gains a row per day per
//! endpoint is a `DELETE` scan a minute to remove nothing.

use std::time::Duration as StdDuration;

use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::content_meter;
use crate::state::AppState;

/// How often the window is carried into the table.
///
/// A minute, so the "pending" number the usage tab adds on top of the durable rows stays small
/// enough that a person watching the chart does not see a number jump. Longer would be cheaper
/// and would make the live window the dominant half of every answer, which is the opposite of what
/// the tab is for.
pub const FLUSH_INTERVAL: StdDuration = StdDuration::from_secs(60);

/// How many days of usage are kept.
pub const RETENTION_DAYS: i32 = 90;

/// Ticks between prunes. Sixty minutes at the default interval.
const PRUNE_EVERY_TICKS: u32 = 60;

/// Start the worker; the returned handle is kept by the binary.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tracing::info!(
        interval = FLUSH_INTERVAL.as_secs(),
        retention_days = RETENTION_DAYS,
        "content usage flush worker started"
    );

    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(FLUSH_INTERVAL);
        // A slow flush must not become a burst of catch-up flushes: each one re-reads the same
        // window and *adds* it again, so N catch-up ticks after an outage would multiply the
        // counts by N.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick fires immediately; boot has its own work to do, and flushing an empty
        // namespace at boot is a scan for nothing.
        ticks.tick().await;

        let mut tick: u32 = 0;
        loop {
            ticks.tick().await;
            tick = tick.wrapping_add(1);

            let report = content_meter::flush(&state).await;
            if !report.is_idle() {
                tracing::info!(
                    rows = report.rows,
                    requests = report.requests,
                    throttled = report.throttled,
                    "content usage flushed"
                );
            }

            if tick % PRUNE_EVERY_TICKS == 0 {
                prune(&state).await;
            }
        }
    })
}

/// Drop usage days past the retention window.
async fn prune(state: &AppState) {
    match omnion_content::api_token_usage::prune(state.db().pool(), RETENTION_DAYS).await {
        Ok(0) => {}
        Ok(deleted) => tracing::info!(deleted, "old content usage rows pruned"),
        Err(error) => tracing::warn!(error = %error, "the content usage prune failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flush_interval_and_the_prune_period_agree_about_an_hour() {
        // The prune's "every N ticks" is only an hour if the tick is what this constant says. If
        // someone shortens the interval to make the usage tab fresher — a plausible edit — the
        // prune would silently run every 6 seconds, which is a `DELETE` scan a minute turned into
        // ten, on a table that only gains rows daily. So the two constants are checked against
        // each other rather than trusted separately.
        let ticks_per_hour = 3_600 / FLUSH_INTERVAL.as_secs();
        assert_eq!(
            ticks_per_hour, u64::from(PRUNE_EVERY_TICKS),
            "PRUNE_EVERY_TICKS must stay one hour at the current FLUSH_INTERVAL"
        );
    }

    #[test]
    fn the_retention_is_longer_than_the_window_the_tab_shows() {
        // A day the tab can ask about and a day the table has to be able to answer for are
        // different numbers, and the second must exceed the first or the API clamps a legitimate
        // question away. The tab's own default is 14 days and its ceiling is 365, so retention
        // has to sit between them — the 365 bound is the one that matters, since that is a
        // question the API accepts and must then be able to answer.
        assert!(RETENTION_DAYS > 14, "the tab's default window must be inside retention");
        assert!(
            RETENTION_DAYS < 365,
            "retention cannot answer a 365-day question the API already accepts"
        );
    }
}
