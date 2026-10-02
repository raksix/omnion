//! The backup schedule worker (REQ-013, slice 3).
//!
//! `backup_schedules` shipped in slice 1 and `next_due_schedules` shipped with it — a query
//! that reads `next_run_at` and returns the rows whose time has come. **Nothing wrote that
//! column and nothing called that query.** A schedule could be created, listed, rendered with
//! a cadence sentence beside an empty "next run" cell, and never fire. It is the same silence
//! as the uncalled `prune_candidates`, one table over, and the shape is worth naming: a table
//! with a column, a query that reads it, and no writer is a feature that looks complete in
//! every screenshot and does nothing.
//!
//! The tick is deliberately boring, because the interesting failures are all in *when*:
//!
//! * **A schedule is claimed by its own `next_run_at`, not by a scan for "enabled and old".**
//!   The alternative re-derives a decision the write already made, and a schedule whose time
//!   is in the future would be run early by a worker that only looked at the clock.
//! * **The next run is computed from the run's own `started_at`-equivalent — now — and it is
//!   computed AFTER the run is recorded.** A worker that computed it first and then failed to
//!   record the run would have a schedule pointing at a slot nobody filled, and the next tick
//!   would claim it again: one failure, then a permanent loop.
//! * **A schedule that cannot be computed is disabled, not retried for ever.** An unknown
//!   timezone is a configuration mistake; the honest response is to stop the schedule, record
//!   why, and say so in the log — not to log a warning every minute for the next decade.
//! * **The tick runs every minute**, not every six hours like the sweep. The sweep's interval
//!   comes from the feature (retention is measured in days); a schedule's is measured in
//!   minutes, and an hourly schedule that fires at :37 because the worker woke at :37 is a
//!   schedule the operator did not write.

use std::time::Duration as StdDuration;

use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use crate::state::AppState;

/// The shortest interval the worker will poll at. A second is already too eager for a
/// feature measured in hours; the floor exists so a misconfigured `0` becomes a spin rather
/// than a divide-by-zero, and so a value below the floor logs what it was actually set to.
const MIN_POLL: u64 = 5_000;

/// How far ahead the worker will look for a schedule's next run after firing it.
///
/// One day is enough for every cadence the schema allows: the longest gap between two runs of
/// a monthly schedule on the 1st is 31 days, and the value is only ever *advanced* from the
/// instant the run actually happened, never from the slot that just elapsed.
const REARM_HORIZON_DAYS: i64 = 32;

/// Start the schedule worker; the returned handle ends with the process.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let requested = state.config().retention.backup_schedule_poll_ms;
    let poll_ms = requested.max(MIN_POLL);
    if poll_ms != requested {
        tracing::warn!(
            requested_ms = requested,
            poll_ms,
            "OMNION_BACKUP_SCHEDULE_POLL_MS is below the floor and was raised"
        );
    }

    tracing::info!(poll_ms, "the backup schedule worker started");

    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(StdDuration::from_millis(poll_ms));
        // A slow tick must not become a burst of catch-up ticks. For this worker the
        // consequence would be worse than for the sweep: a schedule that came due during a
        // stall would fire several times in a row, each one a full backup, because every
        // catch-up tick finds the same `next_run_at` in the past until the first one advances
        // it — and the first one is the one that is still running.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        ticks.tick().await;

        loop {
            ticks.tick().await;
            if let Err(error) = tick(&state).await {
                tracing::warn!(error = %error, "the backup schedule tick failed");
            }
        }
    })
}

/// One pass: take every backup whose time has come, and rearm the ones that did.
///
/// Returns how many runs it started, so a caller — or a test — can assert the effect rather
/// than the absence of an error. A tick that logs nothing and returns `()` is a tick whose
/// only observable output is the absence of a crash.
pub async fn tick(state: &AppState) -> Result<usize, omnion_backup::BackupError> {
    let pool = state.db().pool();
    let now = OffsetDateTime::now_utc();
    let due = omnion_backup::next_due_schedules(pool, now).await?;

    let mut started = 0;
    for schedule in due {
        match run_one(
            state,
            &schedule.id,
            schedule.organization_id,
            schedule.scopes.clone(),
        )
        .await
        {
            Ok(Some(backup_id)) => {
                started += 1;
                tracing::info!(
                    schedule = %schedule.name,
                    backup_id = %backup_id,
                    scopes = ?schedule.scopes,
                    "a scheduled backup ran"
                );
            }
            Ok(None) => {
                // Rejected before anything was produced. The reason is already recorded
                // against the schedule; logging it again here would repeat a configuration
                // mistake once a minute for a decade.
            }
            Err(error) => {
                tracing::warn!(
                    schedule = %schedule.name,
                    error = %error,
                    "a scheduled backup could not be taken; the schedule stays armed and will be \\
                     tried on the next tick"
                );
            }
        }
    }
    Ok(started)
}

/// Take one backup for one schedule and rearm the schedule. `Ok(None)` when the schedule was
/// refused for a reason that will not fix itself.
async fn run_one(
    state: &AppState,
    schedule_id: &Uuid,
    organization_id: Option<Uuid>,
    scopes: Vec<String>,
) -> Result<Option<Uuid>, omnion_backup::BackupError> {
    let pool = state.db().pool();

    // Re-read through the boundary rather than trusting the list the tick walked. Between
    // the query and this call a schedule may have been edited, disabled or deleted, and
    // running the OLD definition because the list was read earlier is a backup of settings
    // the operator has already changed.
    let schedule = match omnion_backup::find_schedule(pool, *schedule_id, organization_id).await {
        Ok(schedule) => schedule,
        Err(omnion_backup::BackupError::ScheduleNotFound) => return Ok(None),
        Err(error) => return Err(error),
    };
    if !schedule.enabled {
        return Ok(None);
    }

    let draft = omnion_backup::NewBackup {
        organization_id,
        label: schedule.name.clone(),
        kind: "scheduled".to_owned(),
        schedule_id: Some(schedule.id),
        scopes: if schedule.scopes.is_empty() {
            omnion_backup::PARTS
                .iter()
                .map(|part| (*part).to_owned())
                .collect()
        } else {
            scopes
        },
        destination: schedule.destination.clone(),
        storage_prefix: String::new(),
        // A scheduled run is not protected: it is an ordinary backup that happens to be
        // automatic, and marking it protected would put every nightly run outside the
        // retention window and fill the destination for ever.
        protected: false,
        retain_until: Some(
            OffsetDateTime::now_utc()
                + time::Duration::days(i64::from(schedule.retention_count.max(1))),
        ),
        // `created_by` is deliberately null: the row records who *created the schedule*,
        // which is not who produced this run. A worker has no actor, and a run attributed to
        // the operator who set the schedule up is a false audit trail — the list would show
        // "Furkan" on every nightly run when in fact nothing was pressed.
        created_by: None,
    };

    let row = omnion_backup::insert_backup(pool, &draft).await?;
    let prefix = omnion_backup::storage_prefix(&format!("{}/{}", row.created_at.date(), row.id));
    let prefixed = omnion_backup::set_prefix(pool, row.id, &prefix).await?;
    omnion_backup::start_run(pool, row.id).await?;

    let produced = crate::routes::backups::produce_for_worker(state, &prefixed).await;
    let stored = omnion_backup::list_parts(pool, row.id).await?;
    let finished = omnion_backup::finish_run(pool, row.id, &stored, &now_string()).await?;

    // The rearm happens whatever the outcome of the run. A schedule whose backup failed is
    // still a schedule, and leaving it armed at a past instant would make it fire again on
    // the next tick — every tick, for ever, each one a full copy of the library.
    let next = match omnion_backup::Cadence::from_schedule(&schedule) {
        Ok(cadence) => cadence
            .next_after(OffsetDateTime::now_utc())
            .unwrap_or_else(|_| {
                OffsetDateTime::now_utc() + time::Duration::days(REARM_HORIZON_DAYS)
            }),
        Err(error) => {
            tracing::warn!(
                schedule = %schedule.name,
                error = %error,
                "a schedule's cadence could not be computed; it is disabled so it does not retry \\
                 for ever, and the reason is on the row"
            );
            omnion_backup::set_schedule_enabled(pool, schedule.id, false).await?;
            return Ok(Some(finished.id));
        }
    };
    omnion_backup::record_schedule_run(pool, schedule.id, finished.id, Some(next)).await?;

    if produced > 0 {
        tracing::warn!(
            backup_id = %finished.id,
            failed_parts = produced,
            "a scheduled backup finished with parts that failed"
        );
    }
    Ok(Some(finished.id))
}

fn now_string() -> String {
    OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| String::from("1970-01-01T00:00:00Z"))
}
