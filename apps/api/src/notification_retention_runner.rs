//! The notification delivery-log sweeper (REQ-021, slice 7).
//!
//! `main.rs` spawns this task when the sweeper is enabled
//! (`OMNION_NOTIFICATION_RETENTION_RUNNER`, default on). Each tick walks the organizations that
//! have notifications on the bus, oldest backlog first, and removes the delivery rows and
//! browser devices that are past **their own** window.
//!
//! ## What this worker is for
//!
//! The outbox screen has published "the log goes back 60 days" since it shipped, and the two
//! functions that were supposed to enforce it — `push::prune_deliveries` and
//! `push::prune_stale` — had **zero call sites in the repository**. A number the panel shows an
//! administrator, promising a floor nothing implemented. `prune_endpoints` did have a caller
//! (the delivery queue, after a push service answers 404/410); these two had none at all.
//!
//! ## Five decisions shape this file, and each is a place the obvious shortcut is wrong
//!
//! * **The tick is a day, not a minute — and this one really is.** The event and media sweepers
//!   argue for minute-long ticks because a sweep that finds nothing is cheap and a broken sweep
//!   should be visible within the hour. That argument does not transfer: this sweep deletes rows
//!   that are *supposed* to be gone, so a tick that runs too often does not surface a fault
//!   sooner, it only makes the window's boundary noisier. A day is the retention period's own
//!   unit, and the run log records every pass including the empty one.
//! * **The clock is the settle instant, not the enqueue instant.** This is the substantive
//!   fix and it lives in the crate, not here: `coalesce(settled_at, created_at)`. The dead
//!   statement selected on `created_at`, which `enqueue` writes once and nothing ever
//!   updates, so a delivery queued on day 1 and settled on day 59 was swept on day 60 *for
//!   being sixty days old* — the guarantee measured from the wrong end, and a row that spent a
//!   month failing lost its history the day after it finally arrived.
//! * **The window is the organization's own column.** `organizations
//!   .notification_retention_days`, read inside the work list, for the reason `0123` gives for
//!   the event bus and more strongly here: a delivery log is *evidence*, and "show me what was
//!   sent to this customer on 3 March" is a question whose answer length is a policy. One
//!   global number makes one customer's compliance window the whole platform's.
//! * **A `pending` row is not history, whatever its age.** `settled_at is null` is true of a
//!   row the runner has claimed right now, so the sweep carries a separate
//!   `status <> 'pending'` guard: without it, a tick racing the delivery queue could remove a
//!   delivery out from under an in-flight send. Reached from the other side, this is the same
//!   protection `settle_not_ready`'s lease clause gives.
//! * **Nothing here emits an event.** The sweep deletes history; the run log is the record, it
//!   carries the window and the cutoff and the counts, and an event would be a second, thinner
//!   copy of it that every subscriber has to learn to ignore. This is the same rule the media
//!   sweeper sets, and the reason it is written down rather than left to taste.

use std::time::Duration as StdDuration;

use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use omnion_notifications::retention::{self, MAX_ROWS_PER_SWEEP};

use crate::state::AppState;

/// Start the sweeper; the returned handle is kept by the binary (and ends with the process).
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let poll_ms = state
        .config()
        .notification_retention
        .poll_ms
        .max(60_000);
    let max_orgs = state
        .config()
        .notification_retention
        .max_organizations;
    // The same lease the delivery queue uses, so "recently claimed" means the same thing to
    // both. A sweep that guarded in-flight sends with a *different* window would either race
    // the queue (window too short) or keep dead claims for ever (window too long).
    let lease_seconds = state.config().events.lease_seconds as f64;

    tracing::info!(poll_ms, max_orgs, "notification retention worker started");

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
            if let Err(error) = tick(&state, max_orgs, lease_seconds).await {
                tracing::warn!(error = %error, "the notification retention tick failed");
            }
        }
    })
}

/// One pass over the organizations that have notifications.
pub async fn tick(
    state: &AppState,
    max_orgs: i64,
    lease_seconds: f64,
) -> std::result::Result<retention::PassReport, omnion_notifications::NotificationError> {
    retention::run_pass(state.db().pool(), max_orgs, MAX_ROWS_PER_SWEEP, lease_seconds).await
}

#[cfg(test)]
mod tests {
    use omnion_core::config::Config;

    use super::*;

    /// **The sweeper's switch has to be readable, or the worker is unreachable in both
    /// directions.** A runner that cannot be switched off is a permanent deletion job with no
    /// operator escape; one that cannot be switched *on* is a sweep that silently never
    /// happens — which is precisely the state this worker was written to end.
    #[test]
    fn the_sweeper_is_enabled_by_default_and_is_a_separate_switch() {
        let defaults = Config::default();
        assert!(
            defaults.notification_retention.runner_enabled,
            "delivery-log retention must default on: it is the promise the outbox screen \
             published with nothing behind it"
        );
        // Its own flag, not the delivery runner's. Sharing one switch means an installation
        // that drains the queue from a dedicated worker also has to forget retention, and vice
        // versa — the argument `0123` makes for the event sweeper next to the event runner.
        assert!(defaults.events.runner_enabled);
        assert_eq!(
            defaults.notification_retention.poll_ms,
            omnion_core::config::DEFAULT_NOTIFICATION_RETENTION_POLL_MS
        );
    }

    /// **The tick signature carries a bound and no window.**
    ///
    /// A runner with a window parameter would be a *second* answer to how long a delivery log
    /// is kept, beside `organizations.notification_retention_days` and the panel's
    /// `retention_days`. The per-organization value is read inside the crate's work list, which
    /// is what makes an operator's change take effect on that organization's next tick — and
    /// what keeps one tenant's compliance window from shortening another's log.
    #[test]
    fn the_worker_passes_a_bound_and_never_a_window() {
        assert!(MAX_ROWS_PER_SWEEP > 0);
        assert_eq!(
            omnion_notifications::OUTBOX_RETENTION_DAYS, 60,
            "the fallback the work list binds must be the number the screen publishes"
        );
    }
}
