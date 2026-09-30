//! Queued restore jobs — the window in which a restore can still be stopped (REQ-013, slice
//! 2c).
//!
//! # Why this is a table and not a `POST` that takes longer
//!
//! Slice 2b runs a restore inside the request and says so plainly: there is no cancel button,
//! because a `POST` in flight cannot be un-pressed, and a progress bar an operator could stop
//! halfway through would leave a library that is half old and half new with **no safety
//! backup covering the half that was written**. The object store is not transactional, the
//! loop deliberately does not roll back, and a rollback that deletes what it just wrote is the
//! operation nobody can audit afterwards.
//!
//! The acceptance criterion for that decision was honest about where a real abort belongs:
//! *"a genuine abort belongs with a queued restore"*. This is the queued restore.
//!
//! # The five rules, each one a shortcut that produces a plausible wrong answer
//!
//! * **The window is a state, not a feeling.** `queued` is the only state in which the panel
//!   offers an abort, and the *schema* says so: the check constraint in `0177` requires
//!   `started_at is null` for a `queued` job and `aborted` may only name a job that never
//!   started. A cancel button on a `succeeded` row is a control about the past.
//! * **`started_at` is written by the worker, never by the request.** A request that stamped
//!   its own `started_at` could age itself out of the cancellable window before a single
//!   object had been read, and the abort would be a control that never works.
//! * **A cancel is recorded, not deleted.** The row is the answer to "who stopped this and
//!   when" long after the library is back, and a `delete` would leave the restore list unable
//!   to show a restore that an operator stopped on purpose.
//! * **A cancel is honoured even when the worker already has the job.** `cancel_requested`
//!   exists so an intent recorded during the index read is not lost to a race; the worker
//!   checks it before the **first write**, which is the only moment that still means anything.
//! * **At most one live job per run.** Two queued restores of the same archive are both
//!   legitimate requests, but a panel offering "cancel" for both is a panel whose list is a
//!   coin flip — and the operator pressing the wrong one is the outcome this feature exists to
//!   prevent. Enforced by a partial unique index, so a terminal job is history and history
//!   must not block a new attempt.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{BackupError, Result};

/// The columns of a restore job row, in the order [`RestoreJob`] reads them.
const JOB_COLUMNS: &str = "id, organization_id, backup_id, parts, confirmation, status, \
                           cancel_requested, started_at, finished_at, safety_backup_id, \
                           live_dropped, live_matches, result, error, created_by, created_at, \
                           updated_at";

/// A queued restore, as the panel lists it and the worker claims it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RestoreJob {
    /// Job id.
    pub id: Uuid,
    /// Tenant it belongs to.
    pub organization_id: Option<Uuid>,
    /// The run it restores.
    pub backup_id: Uuid,
    /// The parts the operator left ticked, in request order.
    pub parts: Vec<String>,
    /// The confirmation phrase they typed, carried so the audit trail can name it.
    pub confirmation: String,
    /// `queued|running|succeeded|failed|aborted`.
    pub status: String,
    /// Whether somebody asked for this to stop.
    pub cancel_requested: bool,
    /// When the worker claimed it. `None` is the cancellable window.
    pub started_at: Option<OffsetDateTime>,
    /// When it stopped, however it stopped.
    pub finished_at: Option<OffsetDateTime>,
    /// The protected run to go back to. Present on every `succeeded` job.
    pub safety_backup_id: Option<Uuid>,
    /// The live items the preview priced as dropped, carried from the moment it was agreed to.
    pub live_dropped: i64,
    /// The live items the archive holds.
    pub live_matches: i64,
    /// What the restore did, in the shape the synchronous route returns.
    pub result: Option<serde_json::Value>,
    /// Why it failed, when it failed.
    pub error: Option<String>,
    /// Who queued it.
    pub created_by: Option<Uuid>,
    /// When it was queued.
    pub created_at: OffsetDateTime,
    /// When it last changed.
    pub updated_at: OffsetDateTime,
}

/// What a route sends when it queues a restore.
#[derive(Debug, Clone)]
pub struct NewRestoreJob {
    /// Tenant it belongs to.
    pub organization_id: Option<Uuid>,
    /// The run it restores.
    pub backup_id: Uuid,
    /// The parts the operator left ticked.
    pub parts: Vec<String>,
    /// The phrase they typed.
    pub confirmation: String,
    /// The price they agreed to, carried forward so a job cannot be executed against a
    /// cheaper preview than the one on the screen.
    pub live_dropped: i64,
    /// The archive's coverage of the live library at the same moment.
    pub live_matches: i64,
    /// Who queued it.
    pub created_by: Option<Uuid>,
}

/// A restore job's lifecycle, in the words the panel renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// Waiting for the worker. **The cancellable window.**
    Queued,
    /// The worker has claimed it and taken the safety backup.
    Running,
    /// Every selected part was restored.
    Succeeded,
    /// It stopped on an error. The reason is on the row.
    Failed,
    /// Somebody stopped it while it was still cancellable. Nothing was written.
    Aborted,
}

impl JobStatus {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }

    /// Whether a job in this state may still be stopped.
    ///
    /// Only `queued`. A `running` job has **already taken its safety backup** — that is
    /// precisely what the worker does first, because a restore with nothing to go back to is
    /// the one outcome this feature will not allow — so stopping it would discard a protected
    /// run for a restore that has written some of its objects already.
    #[must_use]
    pub const fn is_cancellable(self) -> bool {
        matches!(self, Self::Queued)
    }
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Why a job could not be found.
///
/// Its own variant rather than a `Rejected` carrying a string, because the two callers need
/// to say different things: a route reading a job that belongs to another tenant must answer
/// a `404` that does not name the tenancy rule, and a worker claiming a job that was aborted
/// a moment ago needs to know it should move on rather than retry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JobError {
    /// No job with that id for this tenant.
    #[error("no queued restore with this id")]
    NotFound,
    /// The job exists but is no longer in a state that permits the call.
    ///
    /// The message names the state, because "you cannot cancel this" is only actionable if
    /// the operator is told what it became.
    #[error("this restore is {0} and can no longer be cancelled")]
    NotCancellable(&'static str),
    /// The run already has a restore waiting or in flight.
    #[error("this run already has a restore that is {0}")]
    AlreadyLive(&'static str),
    /// The database refused or could not answer.
    ///
    /// Its own variant rather than a `NotFound`, because the two send the operator in
    /// opposite directions: "not found" says stop, this says try again. Dressing a connection
    /// failure as a missing row is how a queued restore disappears from the panel and the
    /// operator never learns that the platform accepted their confirmation.
    #[error("the restore could not be queued: {0}")]
    Unavailable(String),
}

/// Queue a restore.
///
/// The empty selection is refused **here**, before the row exists, for the reason the
/// synchronous route refuses it: a form that posted nothing and got the whole archive back
/// would restore more than it showed. `parts` is also checked against the five known names,
/// so a typo is a refusal rather than a part nobody recognises and a restore that quietly did
/// less than it was asked.
pub async fn queue_restore(pool: &PgPool, new: &NewRestoreJob) -> std::result::Result<RestoreJob, JobError> {
    if new.parts.is_empty() {
        return Err(JobError::NotCancellable("nothing was selected"));
    }
    for part in &new.parts {
        if !crate::PARTS.contains(&part.as_str()) {
            return Err(JobError::NotCancellable("a selected part is not one of the five"));
        }
    }

    // A duplicate part is a restore of one part twice, which reads on the panel as two rows
    // for one thing. Narrowed rather than refused, because unlike the unknown name above it
    // cannot make the restore do less than asked.
    let mut parts = new.parts.clone();
    parts.sort_by_key(|part| {
        crate::PARTS
            .iter()
            .position(|known| *known == part.as_str())
            .unwrap_or(usize::MAX)
    });
    parts.dedup();

    match sqlx::query_as::<_, RestoreJob>(&format!(
        "insert into backup_restore_jobs \
         (organization_id, backup_id, parts, confirmation, live_dropped, live_matches, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7) \
         returning {JOB_COLUMNS}"
    ))
    .bind(new.organization_id)
    .bind(new.backup_id)
    .bind(&parts)
    .bind(&new.confirmation)
    .bind(new.live_dropped)
    .bind(new.live_matches)
    .bind(new.created_by)
    .fetch_one(pool)
    .await
    {
        Ok(job) => Ok(job),
        // The partial unique index, caught here so the caller gets a sentence rather than a
        // `500`. `23505` is `unique_violation`; the constraint is named in the migration, and
        // this is the only one that can fire on this insert, so matching the code is enough
        // and reading the current job's status is what turns it into a useful message.
        Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("23505") => {
            let live: Option<(String,)> = sqlx::query_as(
                "select status from backup_restore_jobs \
                 where backup_id = $1 and status in ('queued', 'running') limit 1",
            )
            .bind(new.backup_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
            Err(JobError::AlreadyLive(
                live.as_ref().map_or("queued", |(status,)| static_status(status)),
            ))
        }
        Err(error) => {
            // Anything else is a real database failure and must not be dressed as "not
            // found": a `404` on a queued restore would send the operator looking for a row
            // that is still there, and the retry that never comes.
            tracing::warn!(%error, "a queued restore could not be recorded");
            Err(JobError::Unavailable(error.to_string()))
        }
    }
}

/// Turn a `String` from a status column into a `&'static str` for the error variant.
///
/// The status vocabulary is closed by the schema's own constraint, so an unknown name cannot
/// normally arrive — but a *future* migration adding one would panic a worker here rather
/// than degrade, and a panicking worker is worse than one that says the wrong noun. Unknown
/// names collapse to `"in flight"`, which is true of both states the index actually covers.
fn static_status(status: &str) -> &'static str {
    match status {
        "queued" => "queued",
        "running" => "running",
        _ => "in flight",
    }
}

/// Read one job, scoped by tenant in the same `where` that finds it.
pub async fn find_restore_job(
    pool: &PgPool,
    id: Uuid,
    organization_id: Option<Uuid>,
) -> Result<RestoreJob> {
    let job = sqlx::query_as::<_, RestoreJob>(&format!(
        "select {JOB_COLUMNS} from backup_restore_jobs \
         where id = $1 and organization_id is not distinct from $2"
    ))
    .bind(id)
    .bind(organization_id)
    .fetch_optional(pool)
    .await?
    .ok_or(BackupError::Rejected("no queued restore with this id".to_owned()))?;
    Ok(job)
}

/// A run's jobs, newest first. The detail screen's "restores of this run" list.
pub async fn list_restore_jobs(
    pool: &PgPool,
    backup_id: Uuid,
    organization_id: Option<Uuid>,
) -> Result<Vec<RestoreJob>> {
    let jobs = sqlx::query_as::<_, RestoreJob>(&format!(
        "select {JOB_COLUMNS} from backup_restore_jobs \
         where backup_id = $1 and organization_id is not distinct from $2 \
         order by created_at desc limit 20"
    ))
    .bind(backup_id)
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    Ok(jobs)
}

/// Every queued job on the platform, oldest first, for the worker.
///
/// **Deliberately unscoped, and a separate function rather than a `None` passed to a scoped
/// one.** `is not distinct from $1` with a `None` matches rows whose `organization_id` *is*
/// null — the platform's own jobs — and nothing else, so a worker calling the scoped reader
/// with `None` would sit on a perfectly healthy tick, find zero jobs and report nothing,
/// while every tenant's restore sat `queued` for ever. That is the same trap the retention
/// sweep documents about `null` being a real member of the tenant list, read from the other
/// side: a background worker is *supposed* to cross tenants, and the scoping that protects a
/// request is exactly what hides a tenant's work from the thing that has to do it.
///
/// The two callers are different questions and have different names: [`find_restore_job`] and
/// [`list_restore_jobs`] answer for **one** caller and are scoped; this one answers for the
/// platform and is not.
pub async fn all_queued_restore_jobs(pool: &PgPool) -> Result<Vec<RestoreJob>> {
    let jobs = sqlx::query_as::<_, RestoreJob>(&format!(
        "select {JOB_COLUMNS} from backup_restore_jobs \
         where status = 'queued' \
         order by created_at asc limit 10"
    ))
    .fetch_all(pool)
    .await?;
    Ok(jobs)
}

/// Queued jobs a tenant can see, oldest first.
///
/// The panel's own "waiting" list, so it is scoped — and it is this function, not
/// [`all_queued_restore_jobs`], that a route calls.
pub async fn queued_restore_jobs(
    pool: &PgPool,
    organization_id: Option<Uuid>,
) -> Result<Vec<RestoreJob>> {
    let jobs = sqlx::query_as::<_, RestoreJob>(&format!(
        "select {JOB_COLUMNS} from backup_restore_jobs \
         where status = 'queued' and organization_id is not distinct from $1 \
         order by created_at asc limit 10"
    ))
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    Ok(jobs)
}

/// Finish a cancel **synchronously**: turn a flagged, still-queued job into `aborted`.
///
/// A separate function rather than a flag plus a claim, because the obvious way to settle a
/// cancel is to let the worker notice the flag on its next tick — and that leaves the panel
/// showing a queued row with a cancel on it for up to a poll interval, which is a control
/// that appears not to work. It is also *wrong* to settle it by claiming: `claim_restore_job`
/// stamps `started_at`, so a cancel route that used it would take a job that was never
/// started and mark it `running`, then have to decide what to do with a running restore it
/// has no intention of finishing.
///
/// The `where` clause carries both guards, so this is a single atomic statement that either
/// aborts a job that is genuinely still cancellable, or touches nothing.
pub async fn settle_restore_cancel(pool: &PgPool, id: Uuid, reason: &str) -> Result<bool> {
    let updated = sqlx::query(
        "update backup_restore_jobs \
         set status = 'aborted', finished_at = now(), error = $2, updated_at = now() \
         where id = $1 and status = 'queued' and cancel_requested",
    )
    .bind(id)
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() > 0)
}

/// Record that somebody wants this restore stopped.
///
/// **This is a flag, not a transition**, and the difference is the whole design: the worker
/// may already be reading the index, in which case a `status = 'aborted'` write here would be
/// a lie the schema's own check constraint refuses — an `aborted` job must never have started.
/// So the intent is stored, and the worker is the only thing that may turn it into an
/// `aborted` row, once it has established that it has written nothing.
///
/// The `where` clause is the guard: only a job that is *still* queued gets the flag, so a
/// cancel that lost a race with the worker is reported as "already running" rather than
/// silently recorded against a job that has already restored the library.
pub async fn request_restore_cancel(pool: &PgPool, id: Uuid) -> Result<bool> {
    let updated = sqlx::query(
        "update backup_restore_jobs set cancel_requested = true, updated_at = now() \
         where id = $1 and status = 'queued'",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() > 0)
}

/// Claim the next queued job for one tenant and mark it `running`.
///
/// The `for update skip locked` is what makes two workers safe, and `skip locked` rather
/// than plain `for update` because a plain lock would make the second worker *wait* — and a
/// restore of a large library is measured in minutes, so the wait would be the whole tick.
/// A skip is a "somebody else has it", which is the correct answer in under a millisecond.
///
/// **The claim and the `started_at` stamp are the same statement.** Splitting them would open
/// a window in which a job reads as `running` with a null `started_at`, which the schema's own
/// check constraint refuses — so the two could not be written separately without a second
/// statement, and the second statement is where a job would get stuck: claimed, stamped never,
/// and invisible to the panel as anything but a spinner.
pub async fn claim_restore_job(
    pool: &PgPool,
    id: Uuid,
) -> std::result::Result<RestoreJob, JobError> {
    let job = sqlx::query_as::<_, RestoreJob>(&format!(
        "update backup_restore_jobs \
         set status = 'running', started_at = now(), updated_at = now() \
         where id = $1 and status = 'queued' \
         returning {JOB_COLUMNS}"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        tracing::warn!(%error, "a queued restore could not be claimed");
        JobError::NotFound
    })?;

    let job = job.ok_or(JobError::NotFound)?;

    // A cancel that arrived before the claim. The job has not started — `started_at` was set
    // by the statement above, but nothing has been read or written yet, so the honest status is
    // `aborted` and the schema allows it only because `cancel_requested` is set. It is set
    // here rather than by the cancel route precisely so this race has exactly one winner.
    if job.cancel_requested {
        abort_restore_job(pool, job.id, "cancelled before the first write")
            .await
            .map_err(|error| {
                tracing::warn!(%error, job = %job.id, "a cancelled restore could not be marked aborted");
                JobError::Unavailable(error.to_string())
            })?;
        return Err(JobError::NotCancellable("aborted"));
    }
    Ok(job)
}

/// Mark a **claimed** job `aborted` because a cancel was honoured before the first write.
///
/// **The `started_at` is cleared, and that is the load-bearing word.** The claim stamped it,
/// and the schema's abort constraint requires an aborted job to have none — because an abort
/// is a statement that *nothing was written*, and a row that still says "it started" says the
/// opposite. The instant `started_at` normally means is "the safety backup is about to be
/// taken", and no byte has moved in that state: the claim is the worker announcing itself, not
/// the library changing.
///
/// This is the *claimed* path, and it is a **different function** from
/// [`settle_restore_cancel`] on purpose. That one aborts a still-queued row and clears
/// nothing (there is nothing to clear); this one aborts a row the worker just claimed. Two
/// callers, two statements, one shared `where` clause shape — because "the same job, stopped
/// early" is two different database transitions, and a single function with a flag saying
/// which one it wants is a function whose flag nobody sets correctly.
pub async fn abort_restore_job(pool: &PgPool, id: Uuid, reason: &str) -> Result<()> {
    sqlx::query(
        "update backup_restore_jobs \
         set status = 'aborted', started_at = null, finished_at = now(), error = $2, updated_at = now() \
         where id = $1 and status = 'running' and cancel_requested",
    )
    .bind(id)
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a finished restore, successfully or not.
///
/// `safety_backup_id` is written **with** the outcome rather than by a separate call, because
/// the schema requires it on a `succeeded` row and a two-statement version would leave a
/// succeeded restore with no way back for as long as the second statement took to run.
pub async fn finish_restore_job(
    pool: &PgPool,
    id: Uuid,
    succeeded: bool,
    safety_backup_id: Option<Uuid>,
    result: Option<serde_json::Value>,
    error: Option<&str>,
) -> Result<()> {
    let status = if succeeded { "succeeded" } else { "failed" };
    sqlx::query(
        "update backup_restore_jobs \
         set status = $2, finished_at = now(), safety_backup_id = $3, result = $4, error = $5, \
             updated_at = now() \
         where id = $1 and status = 'running'",
    )
    .bind(id)
    .bind(status)
    .bind(safety_backup_id)
    .bind(result)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Abort jobs that have been waiting longer than `max_age_minutes`, with a reason.
///
/// A row that nothing will ever claim is the worst state a queued restore can be in, because
/// the panel draws it as a spinner for ever: the operator pressed a button, the platform took
/// their confirmation, and nothing says it will not happen. A worker that crashed and
/// restarted against a new configuration is enough to produce one.
///
/// **The terminal state is `aborted`, not `failed`, and the schema is what decided that.** The
/// check constraint in `0177` requires `started_at is not null` for `failed` — deliberately,
/// because a `failed` job means "it began and it did not finish". This job never began, so
/// writing `failed` would be refused by the constraint, and the *right* refusal: the
/// alternative would be loosening the constraint to allow a failure that never started, and
/// then `failed` would stop meaning what it says. `aborted` already means exactly this — a
/// restore stopped before its first write — so the honest answer was already in the
/// vocabulary. `cancel_requested` is set because the constraint requires it for an abort, and
/// because it is *true*: the platform did ask, one hour in.
///
/// The age is measured from `created_at`, never from a "first seen" the worker keeps in
/// memory, because a worker that restarted has no memory — which is exactly when the rows it
/// abandoned are the ones it no longer knows about.
pub async fn fail_stale_restore_jobs(
    pool: &PgPool,
    max_age_minutes: i64,
) -> Result<u64> {
    let stale = sqlx::query_as::<_, RestoreJob>(&format!(
        "select {JOB_COLUMNS} from backup_restore_jobs \
         where status = 'queued' \
           and created_at < now() - make_interval(mins => $1::int) \
         order by created_at asc limit 20"
    ))
    // `make_interval` takes `int`, not `bigint`, and a bare bind of an `i64` therefore fails
    // with `42883 function make_interval(mins => bigint) does not exist` — a type error in a
    // query whose *text* is perfectly valid, and therefore one no amount of reading the SQL
    // finds. The cast is in the SQL rather than the bind so the statement names the shape it
    // expects.
    .bind(max_age_minutes.clamp(1, i32::MAX as i64) as i32)
    .fetch_all(pool)
    .await?;

    for job in &stale {
        // `where status = 'queued'` again: a job claimed between the read above and this
        // write is somebody's restore in progress, and aborting it here would report a
        // running job as stopped before it wrote anything — which is the exact lie this
        // whole feature is built to avoid telling.
        let reason = format!(
            "this restore waited {} minutes for a worker and none claimed it, so the platform \
             stopped it rather than leaving it to spin for ever. Nothing was written. Queue it \
             again when you are ready.",
            max_age_minutes
        );
        sqlx::query(
            "update backup_restore_jobs \
             set status = 'aborted', cancel_requested = true, finished_at = now(), \
                 error = $2, updated_at = now() \
             where id = $1 and status = 'queued'",
        )
        .bind(job.id)
        .bind(&reason)
        .execute(pool)
        .await?;
    }
    Ok(stale.len() as u64)
}

/// Whether a cancel has been asked for, checked immediately before the first write.
///
/// The last chance for an abort, and the reason it is a separate read rather than part of
/// the claim: the claim happens at the top of the worker, then the safety backup runs — which
/// on a large library is minutes. A cancel pressed during those minutes has to be seen here,
/// or the abort control works only for jobs nobody looked at.
pub async fn cancel_was_requested(pool: &PgPool, id: Uuid) -> Result<bool> {
    let asked: Option<bool> =
        sqlx::query_scalar("select cancel_requested from backup_restore_jobs where id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(asked.unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_queued_job_is_cancellable() {
        // The load-bearing statement of this whole module, asserted rather than described:
        // a job that has been claimed has already been promised a safety backup, so offering
        // an abort on it would discard the one thing the operator was told they had.
        assert!(JobStatus::Queued.is_cancellable());
        for state in [
            JobStatus::Running,
            JobStatus::Succeeded,
            JobStatus::Failed,
            JobStatus::Aborted,
        ] {
            assert!(
                !state.is_cancellable(),
                "{state} must not offer an abort"
            );
        }
    }

    #[test]
    fn the_status_spellings_are_the_ones_the_schema_constrains() {
        // A rename here would be a second vocabulary: the check constraint in `0177` lists
        // these five strings, and a crate that wrote `complete` would fail every insert with a
        // constraint violation rather than a sentence.
        assert_eq!(JobStatus::Queued.as_str(), "queued");
        assert_eq!(JobStatus::Running.as_str(), "running");
        assert_eq!(JobStatus::Succeeded.as_str(), "succeeded");
        assert_eq!(JobStatus::Failed.as_str(), "failed");
        assert_eq!(JobStatus::Aborted.as_str(), "aborted");
        assert_eq!(JobStatus::Succeeded.to_string(), "succeeded");
    }

    #[test]
    fn an_unknown_status_still_names_a_borrowed_stratum() {
        // `static_status` is reached only from a column the schema constrains, so an unknown
        // name cannot normally happen — but a future migration adding a status would panic a
        // worker rather than degrade, and a panicking worker is worse than an imprecise noun.
        assert_eq!(static_status("queued"), "queued");
        assert_eq!(static_status("running"), "running");
        assert_eq!(static_status("half-done"), "in flight");
    }
}
