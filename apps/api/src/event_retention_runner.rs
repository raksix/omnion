//! The event-retention worker (REQ-016, slice 3).
//!
//! `main.rs` spawns this task when retention is enabled (`OMNION_EVENT_RETENTION_RUNNER`,
//! default on). Each tick walks the organizations that have events on the bus, oldest first,
//! and removes the rows that are past **their own** window.
//!
//! Five decisions shape this file, and each is a place the obvious shortcut is wrong:
//!
//! * **The window is per organization, and it is read inside the delete.** The obvious design
//!   is one global `OMNION_EVENT_RETENTION_DAYS`, swept by one statement over the whole table.
//!   That is wrong twice over: it makes a compliance window an installation-wide setting, and
//!   one `delete` over the entire backlog takes a lock proportional to every row on the bus
//!   while each of them cascades its deliveries. Here the work list is the organizations, and
//!   each is swept in its own transaction — so a large tenant is worked down over several
//!   ticks instead of locking out the delivery runner for the length of its own history.
//! * **A pending delivery pins its event, and that is the whole point of the predicate.** The
//!   obvious sweep — "delete old events, let `on delete cascade` take the deliveries" —
//!   silently deletes facts a receiver is still owed. A `pending` row means the runner has not
//!   delivered it yet: `next_attempt_at` is in the future, a runner that claimed it died, or
//!   the endpoint is off and the row has not been settled. The store's `sweep_events` refuses
//!   those events; the sweeper only owns *which organizations and in what order*.
//! * **A run that deletes nothing is still written to the log.** "The last sweep was last week
//!   and it found nothing" is the sentence an operator needs on the day they ask why an event
//!   from March is still in the feed — and a log that only records activity cannot answer it
//!   on the day nothing happened.
//! * **One organization that cannot be swept does not abandon the rest.** A sweep is a delete
//!   batch, and a batch that stops at the first error is a batch that never reaches the tail.
//!   The failure is logged with the organization named, and the tick continues.
//! * **A tick that finds nothing logs at debug.** A worker that warns on every empty sweep is
//!   a worker whose real warnings stop being read — the same rule `retention_runner` sets for
//!   the media sweeper.

use std::time::Duration as StdDuration;

use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use omnion_events::store;

use crate::state::AppState;

/// Start the sweeper; the returned handle is kept by the binary (and ends with the process).
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let poll_ms = state.config().events.retention_poll_ms.max(60_000);
    let max_orgs = state.config().events.retention_max_orgs;

    tracing::info!(poll_ms, max_orgs, "event retention worker started");

    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(StdDuration::from_millis(poll_ms));
        // A slow tick must not turn into a burst of catch-up ticks: the rows are still there,
        // so the next tick sweeps the same set again and the run log shows two passes rather
        // than one enormous one.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick fires immediately; boot has its own work to do.
        ticks.tick().await;

        loop {
            ticks.tick().await;
            if let Err(error) = tick(&state, max_orgs).await {
                tracing::warn!(error = %error, "the event retention tick failed");
            }
        }
    })
}

/// One pass over the organizations that have events on the bus.
pub async fn tick(
    state: &AppState,
    max_orgs: i64,
) -> std::result::Result<TickReport, omnion_events::EventsError> {
    let pool = state.db().pool();

    let queue = store::organizations_with_events(pool, max_orgs).await?;
    let walked = queue.len();

    let mut events_deleted = 0_i64;
    let mut deliveries_deleted = 0_i64;
    let mut failed = 0_usize;

    for (organization_id, window_days) in queue {
        // One organization that refuses its sweep does not abandon the rest: the run log for
        // the ones that worked is the point of the tick, and a single misbehaving tenant is
        // not a reason to stop pruning every other tenant's history.
        match store::sweep_events(pool, organization_id, window_days).await {
            Ok(report) => {
                events_deleted += report.events_deleted;
                deliveries_deleted += report.deliveries_deleted;
                if report.events_deleted > 0 || report.deliveries_deleted > 0 {
                    tracing::info!(
                        organization_id = ?organization_id,
                        window_days = report.window_days,
                        events_deleted = report.events_deleted,
                        deliveries_deleted = report.deliveries_deleted,
                        "the event retention sweep removed history"
                    );
                }
            }
            Err(error) => {
                failed += 1;
                tracing::warn!(
                    organization_id = ?organization_id,
                    code = error.code(),
                    "an organization's history could not be swept: {error}"
                );
            }
        }
    }

    // The empty tick is the common one — most organizations have nothing past their window on
    // any given week — so it is logged at debug, where it is available and does not push a
    // real warning out of the reader's attention.
    if events_deleted == 0 && deliveries_deleted == 0 && failed == 0 {
        tracing::debug!(walked, "the event retention sweep found nothing to remove");
    }

    Ok(TickReport {
        walked,
        events_deleted,
        deliveries_deleted,
        failed,
        at: OffsetDateTime::now_utc(),
    })
}

/// What one tick did, as the numbers a log line and a test can both read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickReport {
    /// Organizations the tick walked.
    pub walked: usize,
    /// Events removed in total.
    pub events_deleted: i64,
    /// Delivery rows removed with them.
    pub deliveries_deleted: i64,
    /// Organizations whose sweep failed.
    pub failed: usize,
    /// When the tick finished.
    pub at: OffsetDateTime,
}

impl TickReport {
    /// `true` when the tick removed nothing and nothing failed — the shape that must not warn.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.events_deleted == 0 && self.deliveries_deleted == 0 && self.failed == 0
    }
}
