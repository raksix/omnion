//! The backup retention worker (REQ-013, slice 3).
//!
//! `prune_candidates` shipped with slice 1 and **nothing called it**. The retention screen
//! could list what the sweep would do and the walkthrough could assert its exemptions, and
//! the bytes on the destination would still accumulate for ever. This module is the caller —
//! and it is deliberately the *only* caller, so the exemptions live in exactly one statement
//! and the removal lives in exactly one function ([`omnion_backup::remove_run_artifacts`]).
//!
//! # Four decisions, each of which the obvious version gets wrong
//!
//! * **The destination root is read once per tick, not once per run.** A settings row read
//!   inside the loop is N chances to observe a half-applied save: the sweep would remove some
//!   runs from the old root and the rest from the new one, and the operator would find a
//!   clean list over a destination that is full.
//! * **A destination that cannot be read is not an error that repeats every six hours.** The
//!   tick logs the refusal and returns, because the alternative is a `warn` line every tick
//!   for a condition an operator fixes by editing one settings field. The distinction is
//!   "the database is unreachable" (real, transient, worth a warning) against "the root is
//!   empty" (real, permanent, and named in the message).
//! * **A partial removal still deletes the row.** The delete handler made that call a tick
//!   ago and the sweep must make the same one, or the two halves of the same feature would
//!   disagree about what "deleted" means: the route would keep a row whose files are stuck,
//!   the sweep would keep every expired row the moment one file was stuck, and the retention
//!   window would stop being a window.
//! * **Nothing here emits a `backup.deleted` event.** The route emits it with a real actor.
//!   A worker event with `created_by = null` is an event every subscriber has to learn to
//!   ignore, and the audit trail is more useful than a webhook that always arrives from
//!   nobody.

use std::time::Duration as StdDuration;

use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::state::AppState;

/// Start the backup retention worker; the returned handle ends with the process.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let poll_ms = state.config().retention.backup_sweep_poll_ms.max(60_000);
    let max_tenants = state.config().retention.backup_sweep_max_tenants;

    tracing::info!(poll_ms, max_tenants, "the backup retention sweep started");

    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(StdDuration::from_millis(poll_ms));
        // A slow tick must not become a burst of catch-up ticks: the rows are still there, so
        // the next tick sweeps the same set and the log shows two passes rather than one
        // enormous one. This matters more here than for the media sweeper — every catch-up
        // pass is another set of unattended deletes.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick fires immediately; boot has its own work to do and the queue is
        // almost always empty at boot anyway.
        ticks.tick().await;

        loop {
            ticks.tick().await;
            if let Err(error) = tick(&state, max_tenants).await {
                tracing::warn!(error = %error, "the backup retention tick failed");
            }
        }
    })
}

/// One sweep over every tenant that has a backup, plus the platform's own.
pub async fn tick(state: &AppState, max_tenants: i64) -> omnion_backup::Result<omnion_backup::SweepReport> {
    let pool = state.db().pool();

    // Once per tick, before the loop — see the module comment. A missing settings row is not
    // an error: `load_settings` returns the shipped default (a local root under
    // `/var/lib/omnion/backups`), and on a platform that has never configured a destination
    // that root is the one a backup would have been written to.
    let settings = omnion_backup::load_settings(pool).await?;
    let root = settings.local_root.clone();

    let report =
        omnion_backup::sweep_all(pool, &root, OffsetDateTime::now_utc(), max_tenants).await?;

    if !report.is_idle() {
        tracing::info!(
            walked = report.walked,
            candidates = report.candidates,
            removed = report.removed,
            partial = report.partial,
            failed = report.failed,
            "the backup retention sweep removed runs"
        );
    }
    // The stranded list is logged whenever it is non-empty and not merely as part of the line
    // above: "1 run pruned" over a destination that still holds a full media library is
    // exactly the sentence the delete route stopped saying a tick ago, in a place with no
    // operator watching.
    for stranded in &report.stranded {
        tracing::warn!(
            backup_id = %stranded.backup_id,
            path = %stranded.path,
            reason = %stranded.reason,
            "a swept backup's artifacts are still on the destination"
        );
    }
    if report.is_idle() {
        tracing::debug!(walked = report.walked, "the backup retention sweep found nothing to remove");
    }

    Ok(report)
}
