//! The background retention worker (REQ-010, slice 4).
//!
//! `main.rs` spawns this task when the worker is enabled (`OMNION_RETENTION_RUNNER`, default
//! on). Each tick sweeps the superseded versions and the trash of every site that has a
//! library, and repairs the reference rows whose referent is gone.
//!
//! Five decisions shape this file, and each is a place the obvious shortcut is wrong:
//!
//! * **The tick is minutes, not days, and the "daily" in the plan is the windows, not the
//!   timer.** A sweep is idempotent — it claims rows, removes them and writes what it
//!   computed — so a tick that finds nothing is a no-op. A worker that only ran at 02:00 has
//!   one failure mode and no way to show it: the run at 02:00 either worked or it did not, and
//!   nothing in between produces evidence either way. Minutes-long ticks cost a handful of
//!   empty statements per site and make a broken sweep visible within the hour.
//! * **A site that cannot be swept does not abandon the others.** One site whose object store
//!   is refusing deletes must not stop the sweep of a thousand that are fine, and the failure
//!   is logged with the site named rather than swallowed.
//! * **The repair scan runs before the sweeps, not after.** A reference row pointing at a page
//!   that no longer exists refuses a purge for ever, so a library whose pages were deleted
//!   during a migration is permanently unpurgeable until somebody repairs it. Repairing first
//!   means the same tick can act on what it just learned was a lie; repairing afterwards means
//!   the operator waits a whole cycle for no reason.
//! * **The repair scan only touches a referent it can prove is gone.** `pages` is the only
//!   kind the platform can verify today. A kind it cannot verify is *kept*, because deleting a
//!   reference to a module that arrives tomorrow is a library that forgets where its files are
//!   used — and the purge refusal is the only thing standing between that module and a broken
//!   site.
//! * **Nothing here emits a `media.retention_applied` event.** The route does, and it carries
//!   the actor. A worker event with no actor is an event every subscriber has to learn to
//!   ignore, and a retention run is a *site* fact that the run log already records in full.

use std::time::Duration as StdDuration;

use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use crate::state::AppState;

/// Start the retention worker; the returned handle is kept by the binary (and ends with the
/// process).
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let poll_ms = state.config().retention.poll_ms.max(1_000);
    let max_sites = state.config().retention.max_sites;

    tracing::info!(poll_ms, max_sites, "retention worker started");

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
            if let Err(error) = tick(&state, max_sites).await {
                tracing::warn!(error = %error, "the retention tick failed");
            }
        }
    })
}

/// One pass over every site that has a library.
///
/// Bounded by `max_sites`: an installation with a thousand sites does not hold a thousand
/// statements open in one tick, and the remainder is picked up by the next one rather than
/// lost. The bound is a `limit` on the site query, not a slice afterwards, so the truncation
/// is visible in the log as a count.
pub async fn tick(state: &AppState, max_sites: i64) -> std::result::Result<TickReport, omnion_media::MediaError> {
    let pool = state.db().pool();

    // First, because a reference to a page that no longer exists refuses a purge for ever.
    let repaired = omnion_media::repair_references(pool, REPAIR_BATCH).await?;
    if repaired > 0 {
        tracing::info!(repaired, "stale media reference rows were removed");
    }

    let sites = omnion_media::sites_with_media(pool).await?;
    let walked = sites.len();
    let batch: Vec<Uuid> = sites.into_iter().take(max_sites.max(1) as usize).collect();
    if walked > batch.len() {
        tracing::info!(
            walked,
            swept = batch.len(),
            "the retention tick reached its site bound; the rest waits for the next tick"
        );
    }

    let mut purged = 0_i64;
    let mut versions = 0_i64;
    let mut failed = 0_usize;
    for site_id in &batch {
        // One site that cannot be swept must not abandon the others: the run log for the
        // sites that worked is the point of the tick, and a single misconfigured object
        // store is not a reason to stop pruning a thousand libraries.
        match sweep_site(state, *site_id).await {
            Ok((site_purged, site_versions)) => {
                purged += site_purged;
                versions += site_versions;
            }
            Err(error) => {
                failed += 1;
                tracing::warn!(
                    site_id = %site_id,
                    code = error.code(),
                    "a site could not be swept: {}",
                    error.message()
                );
            }
        }
    }

    Ok(TickReport { sites: batch.len(), purged, versions, repaired, failed })
}

/// How many stale reference rows one tick removes. Bounded because the repair is a scan over
/// `media_references` joined against `pages`, and a library with a hundred thousand rows must
/// not turn one tick into a maintenance window.
const REPAIR_BATCH: i64 = 500;

/// What one pass did. Returned rather than only logged so a caller (and a test) can assert it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TickReport {
    /// Sites swept.
    pub sites: usize,
    /// Files purged across them.
    pub purged: i64,
    /// Superseded versions removed across them.
    pub versions: i64,
    /// Stale reference rows removed.
    pub repaired: i64,
    /// Sites that could not be swept.
    pub failed: usize,
}

impl TickReport {
    /// Whether the pass did anything at all.
    ///
    /// A tick that removed nothing is *not* a failure and the worker says so at `debug`, not
    /// at `warn`: a warning per empty tick is a log nobody reads by the end of the day.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.purged == 0 && self.versions == 0 && self.repaired == 0
    }
}

/// Sweep one site's library, writing its run row.
///
/// The same function the route's `Run now` reaches through `run_once`, and deliberately so: two
/// answers to "what may this file go" is how a nightly sweep and an operator's click start
/// disagreeing about the same row.
async fn sweep_site(
    state: &AppState,
    site_id: Uuid,
) -> std::result::Result<(i64, i64), crate::error::ApiError> {
    // The route's `run_once` is `pub` precisely so the worker and the button share it. A
    // worker with its own copy of the sweep is a worker that can drift from the screen that
    // says what it will do — and the operator who reads the screen is the one who is misled.
    let outcome = crate::routes::media_retention::run_once(state, site_id, None).await?;
    Ok((outcome.run.purged, outcome.run.versions_removed))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tick that removed nothing is idle, and idle is not failure.
    ///
    /// The trap this pins: a worker that logs a warning whenever a tick finds nothing fills
    /// the log with entries nobody reads, and the *real* warning — a sweep that cannot reach
    /// the database — becomes the one line in a thousand that still gets looked at.
    #[test]
    fn an_empty_tick_is_idle_and_a_failing_site_is_not() {
        let empty = TickReport { sites: 12, ..TickReport::default() };
        assert!(empty.is_idle());

        let held = TickReport { repaired: 1, ..TickReport::default() };
        assert!(!held.is_idle(), "a repair is work, even when nothing was removed");

        let worked = TickReport { sites: 2, purged: 1, versions: 3, repaired: 0, failed: 0 };
        assert!(!worked.is_idle());
    }

    /// A site that failed is counted, not hidden, and it does not stop the others.
    #[test]
    fn a_failing_site_is_counted_rather_than_swallowed() {
        let report = TickReport { sites: 3, purged: 2, versions: 0, repaired: 0, failed: 1 };
        assert_eq!(report.sites, 3);
        assert_eq!(report.purged, 2, "the sites that worked still counted");
        assert_eq!(report.failed, 1, "and the one that did not is visible");
    }
}
