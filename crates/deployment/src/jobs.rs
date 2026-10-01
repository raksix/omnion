//! The job write side (REQ-024, slice 2): create a deploy, advance its steps, append to its log.
//!
//! Slice 1 shipped the decisions — the step plan, the cancel boundary, the status fold — and the
//! read routes. What was missing is the thing that *runs* a job, and the temptation there is to
//! let the route handler drive it: start a step, run "the deploy", write the result. Three
//! properties have to hold anyway, so they are written here where each can be tested without a
//! running service:
//!
//! * **One job per environment, enforced by the database.** The partial unique index from the
//!   `0211` migration is the real guard; this module's [`active_job`] is what turns the index's
//!   refusal into a `409` with the id of the job that is in the way. A check-then-insert is a
//!   race, and the loser of that race would overwrite a live deploy's row.
//! * **The log is append-only.** [`append_log`] concatenates and never rewrites, because the log
//!   is the only record of what a run did after the operator closed the browser. A step that
//!   rewrites its own output loses the lines that explained its failure.
//! * **A step is started before it runs and finished after.** `start_step` refuses a step that is
//!   not `pending` and refuses to run ahead of the previous one, so a caller cannot mark `verify`
//!   done before `deploy` started — which is the shape of a green history row for a deploy that
//!   never came up.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::StoreError;
use crate::job::{Job, JobKind, JobStatus, Step, StepStatus, plan_steps};

// The log cursor moved to [`crate::log_cursor`] so its edge-case tests run without a database.
// Re-exported here because a caller that already imports this module for the job write side
// should not have to learn a second path for the log it is reading.
pub use crate::log_cursor::{cursor_for, log_since};

/// The environment a job targets, as the routes receive it.
///
/// `Clone` but **not** `Copy`: it owns the environment name, and a `Copy` derive on a struct with
/// a `String` is refused by the compiler — a derive that looks harmless and stops the build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// `production`, `staging` or `sandbox`.
    pub environment: String,
    /// Is this production? Production additionally requires the version to be typed.
    pub production: bool,
}

impl Target {
    /// A target for an environment name.
    ///
    /// The production flag is **derived from the name** rather than passed separately: a caller
    /// that could say "production, not really" would turn the typed-confirmation rule off on the
    /// one environment where it is the only thing between an operator and a bad deploy.
    #[must_use]
    pub fn new(environment: impl Into<String>) -> Self {
        let environment = environment.into();
        let production = environment == "production";
        Self {
            environment,
            production,
        }
    }
}

/// What the caller is asking for.
#[derive(Debug, Clone)]
pub struct NewJob {
    /// Which environment.
    pub target: Target,
    /// Which kind of job.
    pub kind: JobKind,
    /// The version it came from. `None` for a restart.
    pub from_version: Option<String>,
    /// The version it goes to. `None` for a restart.
    pub to_version: Option<String>,
    /// Who started it.
    pub actor: Option<Uuid>,
    /// The reason — mandatory for a rollback, and the table says so too.
    pub reason: Option<String>,
    /// The backup id, when the caller already took one.
    pub backup_id: Option<Uuid>,
    /// The workload a **restart** targets (REQ-024 slice 4). `None` for every other kind.
    ///
    /// Carried on the struct rather than in a separate function because the two are the same
    /// row: the `0214` constraint `deployments_restart_names_a_workload` refuses a restart with
    /// no workload *and* a deploy carrying one, so "which column do I set" is decided by the
    /// kind and cannot be forgotten at a call site. A deploy's versions and a restart's workload
    /// name are the same question — "what is this row about" — asked in two vocabularies.
    pub workload: Option<String>,
}

/// The job the create call produced, with its steps.
#[derive(Debug, Clone)]
pub struct CreatedJob {
    /// The row's id.
    pub id: Uuid,
    /// The step rows, in run order, all `pending`.
    pub steps: Vec<Step>,
}

/// Why a deploy could not be started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartRefusal {
    /// Another job already holds this environment. Carries its id so the wizard can say *which*
    /// deploy is in the way instead of "a deploy is already running".
    Busy(Uuid),
    /// The row could not be written.
    Failed(String),
}

impl std::fmt::Display for StartRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartRefusal::Busy(_) => write!(f, "another job already holds this environment"),
            StartRefusal::Failed(reason) => write!(f, "the job could not be created: {reason}"),
        }
    }
}

/// Is this database error the one-active-per-environment index refusing?
///
/// Matched on the **constraint name** rather than the message, because the message is
/// PostgreSQL's prose and the constraint is our name. A `23505` on a different unique index is
/// a different bug and must not read as "busy".
fn is_busy(error: &sqlx::Error) -> bool {
    let sqlx::Error::Database(ref db) = *error else {
        return false;
    };
    db.constraint() == Some("deployments_one_active_per_environment_idx")
}

/// The active job for an environment, if it has one.
pub async fn active_job(pool: &PgPool, environment: &str) -> Result<Option<Uuid>, StoreError> {
    let id: Option<Uuid> = sqlx::query_scalar(
        "select id from deployments where environment = $1 \
         and status in ('preflight', 'running', 'verifying') limit 1",
    )
    .bind(environment)
    .fetch_optional(pool)
    .await?;
    Ok(id)
}

/// Create a job and its step rows, or refuse.
///
/// The insert and the step rows go in **one transaction**: a job with no steps renders as a
/// wizard timeline with nothing in it, and the operator has no way to tell that apart from a job
/// that finished instantly. The step list comes from [`plan_steps`], so a caller cannot ship a
/// deploy whose timeline is missing the `verify` row the wizard promises.
pub async fn create_job(pool: &PgPool, new_job: &NewJob) -> Result<CreatedJob, StartRefusal> {
    let steps = plan_steps(new_job.kind);
    let id = Uuid::new_v4();

    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(error) => return Err(StartRefusal::Failed(error.to_string())),
    };

    let inserted = sqlx::query(
        "insert into deployments (id, environment, kind, from_version, to_version, status, \
         started_by, reason, backup_id, workload) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(id)
    .bind(&new_job.target.environment)
    .bind(new_job.kind.as_str())
    .bind(&new_job.from_version)
    .bind(&new_job.to_version)
    .bind(JobStatus::Preflight.as_str())
    .bind(new_job.actor)
    .bind(&new_job.reason)
    .bind(new_job.backup_id)
    .bind(&new_job.workload)
    .execute(&mut *tx)
    .await;

    if let Err(error) = inserted {
        // The unique index is the guard, so its refusal is the `409` — read the id back rather
        // than reporting a bare "busy", and roll the transaction back before doing anything else.
        let refusal = if is_busy(&error) {
            let busy = sqlx::query_scalar::<_, Uuid>(
                "select id from deployments where environment = $1 \
                 and status in ('preflight', 'running', 'verifying') limit 1",
            )
            .bind(&new_job.target.environment)
            .fetch_optional(&mut *tx)
            .await
            .ok()
            .flatten();
            match busy {
                Some(other) => StartRefusal::Busy(other),
                // The index fired but the row cannot be read back — a concurrent finish. Report
                // the refusal without an id rather than inventing one.
                None => StartRefusal::Failed("another job holds this environment".to_string()),
            }
        } else {
            StartRefusal::Failed(error.to_string())
        };
        let _ = tx.rollback().await;
        return Err(refusal);
    }

    for (position, name) in steps.iter().enumerate() {
        if let Err(error) = sqlx::query(
            "insert into deployment_steps (deployment_id, position, name, status) \
             values ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(position as i32)
        .bind(*name)
        .bind(StepStatus::Pending.as_str())
        .execute(&mut *tx)
        .await
        {
            let reason = error.to_string();
            let _ = tx.rollback().await;
            return Err(StartRefusal::Failed(reason));
        }
    }

    if let Err(error) = tx.commit().await {
        return Err(StartRefusal::Failed(error.to_string()));
    }

    Ok(CreatedJob {
        id,
        steps: steps
            .iter()
            .enumerate()
            .map(|(position, name)| Step {
                position: position as u32,
                name: (*name).to_string(),
                status: StepStatus::Pending,
                output: String::new(),
                started_at: None,
                finished_at: None,
            })
            .collect(),
    })
}

/// Why a step could not be started or finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepRefusal {
    /// The job does not exist.
    NoJob,
    /// No step by that name in this job's plan — including a name another kind's plan uses, so a
    /// rollback cannot be "advanced" into a migration it does not have.
    NoSuchStep,
    /// The step is not `pending` any more.
    NotPending,
    /// An earlier step has not finished, so this one would run out of order.
    OutOfOrder,
    /// The job has already finished.
    JobFinished,
    /// The database refused the statement.
    ///
    /// Its own variant, and the reason it is not folded into `NoJob`: "the job does not exist"
    /// and "the database is unreachable" are different operator problems, and a caller that
    /// answered a `500` with a `404` would send someone to check an id that exists.
    Storage(String),
}

impl std::fmt::Display for StepRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            StepRefusal::NoJob => "the job does not exist",
            StepRefusal::NoSuchStep => "this job has no step by that name",
            StepRefusal::NotPending => "this step has already started",
            StepRefusal::OutOfOrder => "an earlier step has not finished",
            StepRefusal::JobFinished => "the job has already finished",
            StepRefusal::Storage(reason) => return write!(f, "the job store failed: {reason}"),
        };
        f.write_str(text)
    }
}

/// Turn a database failure into [`StepRefusal::Storage`], with the driver's message.
///
/// Kept as a function because `sqlx::Error`'s `Display` is the only useful thing an operator
/// can be shown, and inlining `.map_err(|e| StepRefusal::Storage(e.to_string()))` at twelve call
/// sites is twelve chances to write `NoJob` instead by accident.
fn storage(error: sqlx::Error) -> StepRefusal {
    StepRefusal::Storage(error.to_string())
}

/// Mark a step as running.
///
/// Refused when the step is not `pending`, when an earlier step is unfinished, or when the job
/// itself has finished — the three ways a timeline can otherwise claim progress that did not
/// happen.
pub async fn start_step(pool: &PgPool, deployment_id: Uuid, name: &str) -> Result<(), StepRefusal> {
    let (status, position) = load_step(pool, deployment_id, name)
        .await?
        .ok_or(StepRefusal::NoSuchStep)?;
    if JobStatus::parse(&status).is_some_and(JobStatus::is_finished) {
        return Err(StepRefusal::JobFinished);
    }
    if status != StepStatus::Pending.as_str() {
        return Err(StepRefusal::NotPending);
    }

    let unfinished: i64 = sqlx::query_scalar(
        "select count(*) from deployment_steps \
         where deployment_id = $1 and position < $2 and status not in ('done', 'failed', 'skipped')",
    )
    .bind(deployment_id)
    .bind(position)
    .fetch_one(pool)
    .await
    .map_err(storage)?;
    if unfinished > 0 {
        return Err(StepRefusal::OutOfOrder);
    }

    sqlx::query(
        "update deployment_steps set status = $1, started_at = now() \
         where deployment_id = $2 and position = $3",
    )
    .bind(StepStatus::Running.as_str())
    .bind(deployment_id)
    .bind(position)
    .execute(pool)
    .await
    .map_err(storage)?;
    Ok(())
}

/// Load one step's stored status and position.
async fn load_step(
    pool: &PgPool,
    deployment_id: Uuid,
    name: &str,
) -> Result<Option<(String, i32)>, StepRefusal> {
    let row: Option<(String, i32)> = sqlx::query_as(
        "select s.status, s.position from deployment_steps s \
         where s.deployment_id = $1 and s.name = $2",
    )
    .bind(deployment_id)
    .bind(name)
    .fetch_optional(pool)
    .await
    .map_err(storage)?;
    Ok(row)
}

/// Mark a step finished, and fold the job's own status forward.
///
/// A failed step marks the job `failed` **and skips every later step in the same statement pair**,
/// so a job cannot sit in `running` forever waiting on a `verify` that will never start. The
/// status is computed from the step rows, never carried by the caller: a caller that says
/// "succeeded" about a run whose fourth step failed is exactly the green history entry the spec
/// forbids.
pub async fn finish_step(
    pool: &PgPool,
    deployment_id: Uuid,
    name: &str,
    outcome: StepStatus,
    error: Option<&str>,
) -> Result<JobStatus, StepRefusal> {
    if !matches!(outcome, StepStatus::Done | StepStatus::Failed) {
        return Err(StepRefusal::NotPending);
    }
    let (_, position) = load_step(pool, deployment_id, name)
        .await?
        .ok_or(StepRefusal::NoSuchStep)?;

    let mut tx = pool.begin().await.map_err(storage)?;

    sqlx::query(
        "update deployment_steps set status = $1, finished_at = now() \
         where deployment_id = $2 and position = $3",
    )
    .bind(outcome.as_str())
    .bind(deployment_id)
    .bind(position)
    .execute(&mut *tx)
    .await
    .map_err(storage)?;

    if outcome == StepStatus::Failed {
        sqlx::query(
            "update deployment_steps set status = $1 \
             where deployment_id = $2 and position > $3 and status = $4",
        )
        .bind(StepStatus::Skipped.as_str())
        .bind(deployment_id)
        .bind(position)
        .bind(StepStatus::Pending.as_str())
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
    }

    let failed: i64 = sqlx::query_scalar(
        "select count(*) from deployment_steps where deployment_id = $1 and status = $2",
    )
    .bind(deployment_id)
    .bind(StepStatus::Failed.as_str())
    .fetch_one(&mut *tx)
    .await
    .map_err(storage)?;
    let running: i64 = sqlx::query_scalar(
        "select count(*) from deployment_steps where deployment_id = $1 and status = $2",
    )
    .bind(deployment_id)
    .bind(StepStatus::Running.as_str())
    .fetch_one(&mut *tx)
    .await
    .map_err(storage)?;
    let pending: i64 = sqlx::query_scalar(
        "select count(*) from deployment_steps where deployment_id = $1 and status = $2",
    )
    .bind(deployment_id)
    .bind(StepStatus::Pending.as_str())
    .fetch_one(&mut *tx)
    .await
    .map_err(storage)?;

    let job_status = if failed > 0 {
        JobStatus::Failed
    } else if running > 0 {
        JobStatus::Running
    } else if pending > 0 {
        JobStatus::Preflight
    } else {
        JobStatus::Verifying
    };

    // Only a finished job gets its stamp, and the table refuses a `succeeded` row without one —
    // so the duration is computed here, from the row's own `started_at`, not from the runner's
    // clock, which would make a job that was queued for ten minutes claim a ten-minute duration.
    if job_status.is_finished() {
        sqlx::query(
            "update deployments set status = $1, error = coalesce($2, error), \
             finished_at = now(), duration_ms = greatest(0, (extract(epoch from (now() - started_at)) * 1000)::int) \
             where id = $3",
        )
        .bind(job_status.as_str())
        .bind(error)
        .bind(deployment_id)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
    } else {
        sqlx::query(
            "update deployments set status = $1, error = coalesce($2, error) where id = $3",
        )
        .bind(job_status.as_str())
        .bind(error)
        .bind(deployment_id)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
    }

    tx.commit().await.map_err(storage)?;
    Ok(job_status)
}

/// Append to a step's log. Never rewrites.
///
/// The concatenation is done in SQL rather than read-modify-write in Rust on purpose: two writers
/// appending to the same step would race, and the loser's line would vanish — which is the one
/// failure mode a log cannot have.
pub async fn append_log(
    pool: &PgPool,
    deployment_id: Uuid,
    name: &str,
    line: &str,
) -> Result<(), StepRefusal> {
    let result = sqlx::query(
        "update deployment_steps set output = output || $1 \
         where deployment_id = $2 and name = $3",
    )
    .bind(line)
    .bind(deployment_id)
    .bind(name)
    .execute(pool)
    .await
    .map_err(storage)?;
    if result.rows_affected() == 0 {
        return Err(StepRefusal::NoSuchStep);
    }
    Ok(())
}

/// The log for one job, flattened across its steps with a header per step.
///
/// Returned as one string because that is what the log pane renders and what an operator
/// downloads; the per-step split is still available from [`list_steps`](crate::store::list_steps)
/// for the history expansion.
pub async fn job_log(pool: &PgPool, deployment_id: Uuid) -> Result<String, StepRefusal> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "select name, output from deployment_steps where deployment_id = $1 order by position",
    )
    .bind(deployment_id)
    .fetch_all(pool)
    .await
    .map_err(storage)?;
    if rows.is_empty() {
        return Err(StepRefusal::NoJob);
    }
    let mut out = String::new();
    for (name, output) in rows {
        if output.is_empty() {
            continue;
        }
        out.push_str(&format!("── {name} ──\n"));
        out.push_str(&output);
        if !output.ends_with('\n') {
            out.push('\n');
        }
    }
    Ok(out)
}

/// Read a job back, with its steps, as the API returns it.
pub async fn load_job(pool: &PgPool, id: Uuid) -> Result<Job, StoreError> {
    let row = crate::store::load_deployment(pool, id).await?;
    let steps = crate::store::list_steps(pool, id).await?;
    let kind = JobKind::parse(&row.kind).ok_or(StoreError::NotFound)?;
    let status = JobStatus::parse(&row.status).ok_or(StoreError::NotFound)?;
    Ok(Job {
        id: row.id,
        environment: row.environment,
        kind,
        status,
        from_version: row.from_version,
        to_version: row.to_version,
        started_by: row.started_by,
        reason: row.reason,
        started_at: row.started_at,
        finished_at: row.finished_at,
        duration_ms: row.duration_ms.map(i64::from),
        error: row.error,
        steps: steps
            .into_iter()
            .map(|s| Step {
                position: s.position.unsigned_abs(),
                name: s.name,
                // An unrecognised stored status becomes `Unknown`, not `Pending`.
                status: StepStatus::parse(&s.status).unwrap_or(StepStatus::Unknown),
                output: s.output,
                started_at: s.started_at,
                finished_at: s.finished_at,
            })
            .collect(),
    })
}

/// Stamp a finished job's error, for the result banner and the history row.
///
/// Separate from [`finish_step`] because a deploy that *starts* fine and fails its health
/// verification has no failed step to carry the message.
pub async fn mark_failed(pool: &PgPool, id: Uuid, reason: &str) -> Result<(), StoreError> {
    sqlx::query(
        "update deployments set status = $1, error = $2, finished_at = now(), \
         duration_ms = greatest(0, (extract(epoch from (now() - started_at)) * 1000)::int) \
         where id = $3",
    )
    .bind(JobStatus::Failed.as_str())
    .bind(reason)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a job verified and finished, for the end of a clean run.
pub async fn mark_succeeded(pool: &PgPool, id: Uuid) -> Result<(), StoreError> {
    sqlx::query(
        "update deployments set status = $1, finished_at = now(), \
         duration_ms = greatest(0, (extract(epoch from (now() - started_at)) * 1000)::int) \
         where id = $2",
    )
    .bind(JobStatus::Succeeded.as_str())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The job's elapsed time, for the wizard's clock.
///
/// Read from the row rather than kept by the caller, so a panel that reloaded mid-run shows the
/// real elapsed time instead of restarting its own timer at zero.
pub async fn elapsed_ms(pool: &PgPool, id: Uuid) -> Result<Option<i64>, StoreError> {
    let value: Option<i32> = sqlx::query_scalar(
        "select case when finished_at is null \
         then null \
         else greatest(0, (extract(epoch from (finished_at - started_at)) * 1000)::int) end \
         from deployments where id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(value.map(i64::from))
}

/// When a job row was started, for the history screen's sort when a filter is active.
pub async fn started_at(pool: &PgPool, id: Uuid) -> Result<Option<OffsetDateTime>, StoreError> {
    Ok(
        sqlx::query_scalar("select started_at from deployments where id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_is_derived_from_the_name() {
        // The typed-confirmation rule rides on this flag, so a caller must not be able to
        // declare production while naming a different environment.
        assert!(Target::new("production").production);
        assert!(!Target::new("staging").production);
        assert!(!Target::new("sandbox").production);
    }

    #[test]
    fn refusals_carry_their_reason() {
        assert!(StepRefusal::OutOfOrder.to_string().contains("earlier step"));
        assert!(StepRefusal::NoSuchStep.to_string().contains("no step"));
        assert!(
            StartRefusal::Busy(Uuid::nil())
                .to_string()
                .contains("already holds")
        );
    }
}
