//! The queued-restore worker (REQ-013, slice 2c).
//!
//! Slice 2b runs a restore inside the `POST` and has no cancel button, on purpose: a `POST`
//! in flight cannot be un-pressed, and a progress bar an operator could stop halfway through
//! would leave a library that is half old and half new with **no safety backup covering the
//! half that was written**. The acceptance criterion for that decision said where a real
//! abort belongs — "a genuine abort belongs with a queued restore" — and this is it.
//!
//! # The order of operations, and why the cancel is read where it is
//!
//! ```text
//! claim  ──►  read the plan  ──►  [CANCEL CHECK]  ──►  safety backup  ──►  objects
//!             (seconds)                                       (minutes)
//! ```
//!
//! **The cancel is checked twice, and neither check is decorative.** Once immediately after
//! the claim, which catches a cancel that arrived while the job sat in the queue, and once
//! after the plan has been rebuilt and *before* the safety backup is taken. The second is the
//! load-bearing one: on a large library the safety backup is minutes of copying, so a cancel
//! pressed during that window has to be seen — and it is still honoured, because the abort
//! window is defined as **before the first write**, and the safety backup writes to the
//! *backup destination*, not to the live library. Restoring a safety run is not restoring.
//!
//! Once the first object lands, there is no cancel. The job runs to the end and reports
//! exactly how many objects landed, which is the same rule the synchronous route follows and
//! the same reason it has no rollback.
//!
//! # Three things this worker refuses to do
//!
//! * **It does not re-derive the price.** The live counts the operator agreed to are carried
//!   on the job, not recomputed. A queued restore re-priced at execution time would restore
//!   against *today's* library while the operator agreed to *yesterday's* number.
//! * **It does not take a second safety backup of a restore that already has one.** A job
//!   cancelled after its claim produced none, so a re-queue starts clean; a job that ran took
//!   one, and the row says which.
//! * **It does not run two jobs for the same run at once.** The claim is a conditional
//!   `update ... where status = 'queued'`, so a second worker — or a second tick — finds
//!   nothing and moves on. A restore that ran twice would take two safety backups and write
//!   every object twice, and the report would name one of them.

use std::time::Duration as StdDuration;

use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use crate::state::AppState;

/// The shortest interval the worker will poll at. A second is far too eager for an
/// operation measured in minutes, and the floor exists so a misconfigured `0` becomes a slow
/// spin rather than a divide-by-zero.
const MIN_POLL: u64 = 2_000;

/// How long a job may sit in the queue before the worker stops caring about it.
///
/// A queued restore is an operator sitting in front of a screen, so the window is short. But
/// a restore that is *never* picked up is worse than a slow one, and a worker that keeps
/// re-reading a poisoned job for ever is a tick that stops serving every other job behind it
/// — so a job older than this is failed with a reason rather than retried silently.
const QUEUE_MAX_AGE_MINUTES: i64 = 60;

/// Start the restore worker; the returned handle ends with the process.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let requested = state.config().retention.backup_schedule_poll_ms;
    let poll_ms = requested.max(MIN_POLL);
    if poll_ms != requested {
        tracing::warn!(
            requested_ms = requested,
            poll_ms,
            "OMNION_BACKUP_SCHEDULE_POLL_MS is below the floor and was raised for the restore \
             worker too"
        );
    }

    tracing::info!(poll_ms, "the queued restore worker started");

    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(StdDuration::from_millis(poll_ms));
        // A slow tick must not become a burst. A catch-up tick would find the same queued job
        // again — the first one is the one still running — and the second would claim nothing
        // and log a warning that says the platform is wedged when it is only busy.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        ticks.tick().await;

        loop {
            ticks.tick().await;
            if let Err(error) = tick(&state).await {
                tracing::warn!(error = %error, "the queued restore tick failed");
            }
        }
    })
}

/// One pass: run every queued job that is not cancelled, oldest first.
///
/// Returns how many restores it finished, so a caller — or a test — can assert the effect
/// rather than the absence of an error. A tick that logs nothing and returns `()` is a tick
/// whose only observable output is the absence of a crash.
pub async fn tick(state: &AppState) -> std::result::Result<usize, omnion_backup::BackupError> {
    let pool = state.db().pool();

    // A job nothing will ever claim is worse than a slow one, so the tick ages the queue out
    // first. Before the claim, not after: a row that has been spinning for an hour is a lie
    // the panel is telling an operator right now, and the operator is the one who has to be
    // able to trust this screen.
    let stale = omnion_backup::restore_jobs::fail_stale_restore_jobs(pool, QUEUE_MAX_AGE_MINUTES)
        .await?;
    if stale > 0 {
        tracing::warn!(
            stale,
            minutes = QUEUE_MAX_AGE_MINUTES,
            "queued restores nobody claimed were stopped rather than left waiting for ever"
        );
    }

    // **Unscoped on purpose.** A background worker is supposed to cross tenants; the scoping
    // that protects a request is exactly what would hide a tenant's restore from the thing
    // that has to perform it, and `is not distinct from null` matches only the platform's own
    // rows — so the scoped reader called with `None` would report a healthy tick while every
    // tenant's restore sat `queued` for ever.
    let jobs = omnion_backup::restore_jobs::all_queued_restore_jobs(pool).await?;

    let mut finished = 0;
    for job in jobs {
        if let Err(error) = run_one(state, job.id).await {
            // The job is already marked `failed` with its reason by `run_one`; this is the
            // tick-level log, and it deliberately carries the id so an operator chasing a
            // stuck restore in the panel can find the line that explains it.
            tracing::warn!(job = %job.id, %error, "a queued restore did not finish");
        }
        finished += 1;
    }
    Ok(finished)
}

/// Run one job to a terminal state. Every exit path writes the row.
async fn run_one(state: &AppState, job_id: Uuid) -> std::result::Result<(), omnion_backup::BackupError> {
    let pool = state.db().pool();

    let job = match omnion_backup::restore_jobs::claim_restore_job(pool, job_id).await {
        Ok(job) => job,
        // Already claimed, already terminal, or already aborted by the claim itself — all of
        // which are somebody else's race, not a failure. A tick that logged each one would
        // report a platform fault every time an operator cancelled a restore.
        Err(omnion_backup::restore_jobs::JobError::NotFound)
        | Err(omnion_backup::restore_jobs::JobError::NotCancellable(_)) => return Ok(()),
        Err(omnion_backup::restore_jobs::JobError::AlreadyLive(state_name)) => {
            tracing::warn!(job = %job_id, state_name, "a queued restore is {state_name}; skipped");
            return Ok(());
        }
        Err(omnion_backup::restore_jobs::JobError::Unavailable(reason)) => {
            return fail(pool, job_id, &reason).await;
        }
    };

    // --- 1. The window, closed ---------------------------------------------------------------
    // Read once more here rather than trusting the claim's check. The claim happened at the
    // top of the tick and everything between here and the safety backup is a chance for a
    // cancel to land; a worker that checked only at claim time has an abort control that
    // works for jobs nobody was watching.
    if omnion_backup::restore_jobs::cancel_was_requested(pool, job.id).await? {
        omnion_backup::restore_jobs::abort_restore_job(
            pool,
            job.id,
            "cancelled before the first write — nothing was changed",
        )
        .await?;
        tracing::info!(job = %job.id, "a queued restore was cancelled before it wrote anything");
        return Ok(());
    }

    // --- 2. The plan, rebuilt from the run, not trusted from the queue ------------------------
    let row = omnion_backup::find_backup(pool, job.backup_id, job.organization_id).await?;
    let settings = omnion_backup::load_settings(pool).await?;

    // The job carries the selection the operator made **and** the phrase they typed. The
    // phrase is re-checked against the run's own id here, not at queue time, because the
    // confirm phrase is a hash of the run id and a job that outlived a re-created run would
    // otherwise restore on a phrase that never belonged to it.
    let expected = omnion_backup::confirm_phrase(&row.id.to_string());
    if job.confirmation != expected {
        let reason = format!(
            "this run's confirmation phrase is {expected} and the queued restore carried a \
             different one, so it was not run. Queue it again from the preview."
        );
        fail(pool, job.id, &reason).await?;
        return Ok(());
    }

    // The preview, rebuilt, for the same reason the synchronous route rebuilds it: the
    // operator may have been sitting on the confirmation for minutes, and a plan built from
    // the queue would restore a part that was truncated in the meantime.
    let priced = crate::routes::restore_jobs::rebuild_preview(&state, &row, &settings).await;
    let preview = &priced.preview;
    if priced.live_comparison_failed {
        // The same refusal the synchronous route makes. A queued restore is not a *safer*
        // restore: it runs unattended, so a plan it cannot price is worse, not better, and
        // the only difference between the two paths would be a different rule.
        fail(pool, job.id, crate::routes::restore_jobs::UNPRICED).await?;
        return Ok(());
    }
    if !preview.restorable {
        let reason =
            "this run stopped being restorable while the restore was queued — an artifact \
             could not be re-read. Nothing was written."
                .to_owned();
        fail(pool, job.id, &reason).await?;
        return Ok(());
    }

    // --- 3. The safety backup, and only then the first write -----------------------------------
    let safety = match crate::routes::backups::take_safety_backup(
        state,
        &row,
        job.created_by.unwrap_or(row.created_by.unwrap_or_default()),
        job.organization_id,
    )
    .await
    {
        Ok(safety) => safety,
        Err(error) => {
            let reason = format!("the safety backup could not be taken: {error}");
            fail(pool, job.id, &reason).await?;
            return Ok(());
        }
    };
    if !safety.complete {
        let reason = format!(
            "the safety backup could not be completed ({}), so nothing was restored",
            safety.summary
        );
        fail(pool, job.id, &reason).await?;
        return Ok(());
    }

    // --- 4. The objects -----------------------------------------------------------------------
    // **There is no cancel check here, and that is the design.** The first object is the
    // moment the platform begins changing, and a cancel after it could not undo what has
    // already landed. The safety backup is what the operator goes back to instead, which is
    // why it is taken before this line and not after.
    let media = if preview.parts.iter().any(|part| part.part == "media" && part.available) {
        match crate::routes::backups::restore_media_part(&state, &row, &settings, job.organization_id)
            .await
        {
            Ok(media) => media,
            Err(error) => {
                let reason = format!("the media part could not be restored: {error}");
                fail(pool, job.id, &reason).await?;
                return Ok(());
            }
        }
    } else {
        omnion_backup::MediaRestoreReport::default()
    };

    let restored: Vec<String> = preview
        .parts
        .iter()
        .filter(|part| part.available && part.part != "media")
        .map(|part| part.part.clone())
        .collect();

    let summary = if media.objects_restored > 0 || media.objects_failed > 0 {
        media.summary()
    } else if restored.is_empty() {
        "nothing was written — this run offered no restorable part".to_owned()
    } else {
        format!("{} recorded from this run; no live bytes were written", restored.join(", "))
    };

    let outcome = serde_json::json!({
        "backup_id": row.id,
        "parts": preview.parts.iter().map(|part| part.part.clone()).collect::<Vec<_>>(),
        "restored": restored,
        "media": &media,
        "safety_backup_id": safety.backup_id,
        "live_dropped": job.live_dropped,
        "live_matches": job.live_matches,
        "summary": summary,
    });

    omnion_backup::restore_jobs::finish_restore_job(
        pool,
        job.id,
        true,
        Some(safety.backup_id),
        Some(outcome.clone()),
        None,
    )
    .await?;

    crate::routes::restore_jobs::record_restore_outcome(
        pool,
        job.organization_id,
        job.created_by,
        Some(row.id),
        "backup.restored",
        &outcome,
    )
    .await;

    tracing::info!(
        job = %job.id,
        backup_id = %row.id,
        objects_restored = media.objects_restored,
        objects_failed = media.objects_failed,
        "a queued restore finished"
    );
    Ok(())
}

/// Mark a job `failed` with the reason the panel shows, and record why.
///
/// The audit entry is written here and not only on success: a restore that was authorised,
/// queued, and then stopped is *exactly* the event an operator needs to be able to find, and
/// an audit trail that only records the restores that worked is a trail that is missing
/// precisely the entries worth reading.
async fn fail(
    pool: &sqlx::PgPool,
    job_id: Uuid,
    reason: &str,
) -> std::result::Result<(), omnion_backup::BackupError> {
    omnion_backup::restore_jobs::finish_restore_job(
        pool,
        job_id,
        false,
        None,
        None,
        Some(reason),
    )
    .await?;

    crate::routes::restore_jobs::record_restore_job_refused(pool, job_id, reason).await;
    Ok(())
}
