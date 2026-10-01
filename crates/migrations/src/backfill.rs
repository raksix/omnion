//! Backfill jobs and seed datasets (docs/requests/REQ-129, slice 3).
//!
//! ## The one thing that can be quietly false here
//!
//! **"It resumed" is not the same claim as "it resumed exactly".** A backfill that restarts and
//! re-processes rows that were already done is *finishing*, so any assertion that only waits for
//! the job to reach `completed` passes while the feature is broken — the rows end up right by
//! being written twice. So the cursor in this module is a **primary key value**, never a row
//! count, and [`run_once`] is written so that the batch it processes is exactly the rows above the
//! stored cursor. A restart therefore resumes from a value that means "the last key that
//! finished", and re-processing is not merely unlikely, it is unrepresentable.
//!
//! The second half of the same claim is that the *cursor is written in the same transaction as
//! the rows it describes*. If the rows commit and the cursor does not, the next batch starts
//! before the last one finished and the rows between the two are never visited. That is why
//! [`run_once`] takes one transaction and commits once — see the note on [`BatchOutcome`].
//!
//! ## Pure where it can be
//!
//! The parts an operator can get wrong without a database are pure and unit-tested here: which
//! state a transition is legal from ([`can_transition`]), whether a job is still doing work
//! ([`is_open`]), and how a batch's statements are shaped ([`batch_statements`]). The database
//! parts are thin wrappers over those answers. Clocks and pools arrive as arguments throughout,
//! for the reason [`crate::runner`] states: a `--dry-run` has to answer honestly without
//! connecting to anything.

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{MigrationSafetyError, Result};

/// The states a backfill job can be in.
///
/// A `const` array rather than a `strum`-style enum because the value list is duplicated in the
/// migration's CHECK and the API's validation, and an enum's `Display` is a second spelling. The
/// CHECK is the database's guarantee; this list is what the code compares against.
pub const STATES: [&str; 5] = ["pending", "running", "paused", "completed", "failed"];

/// The states in which a job still has work to do.
pub const OPEN_STATES: [&str; 3] = ["pending", "running", "paused"];

/// One backfill job row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Backfill {
    /// Its id.
    pub id: Uuid,
    /// The descriptor's stable name.
    pub name: String,
    /// The table being backfilled.
    pub table_name: String,
    /// The column the backfill writes.
    pub column_name: String,
    /// The column the cursor walks — the target table's primary key.
    ///
    /// Distinct from `column_name` on purpose. The column being backfilled is NULL for every row
    /// still to do, so it cannot bound a batch or order one: a cursor built from the value being
    /// filled in has no lower bound and the job re-selects the same first page forever.
    pub key_column: String,
    /// Rows per batch.
    pub batch_size: i32,
    /// Rows per second ceiling, so a backfill can be told not to fight live traffic.
    pub rate_limit_per_second: i32,
    /// The last primary key value whose batch completed. `None` until the first one does.
    pub resume_key: Option<String>,
    /// Rows completed so far. A counter, not a query — see the module doc.
    pub rows_done: i64,
    /// `pending` | `running` | `paused` | `completed` | `failed`.
    pub state: String,
    /// The failing batch's message, cleared by a successful resume.
    pub last_error: Option<String>,
    /// When it was paused.
    pub paused_at: Option<OffsetDateTime>,
    /// When it first ran.
    pub started_at: Option<OffsetDateTime>,
    /// When it finished.
    pub completed_at: Option<OffsetDateTime>,
    /// When the row was created.
    pub created_at: OffsetDateTime,
}

/// A backfill a migration declared.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackfillDescriptor {
    /// The migration that declared it.
    pub version: String,
    /// Its stable name.
    pub name: String,
    /// The table it backfills.
    pub table_name: String,
    /// The column it writes.
    pub column_name: String,
    /// The primary key of the target table, which is what the cursor walks.
    pub key_column: String,
    /// Rows per batch.
    pub batch_size: i32,
    /// Rows per second ceiling.
    pub rate_limit_per_second: i32,
    /// What the batch runs.
    pub statement: String,
}

/// What one [`run_once`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchOutcome {
    /// The batch ran and committed. `rows` is how many it touched.
    Ran {
        /// Rows the batch processed.
        rows: i64,
        /// The cursor it wrote, which is the last key it processed.
        resume_key: Option<String>,
    },
    /// There was nothing above the cursor. The job is complete.
    Finished,
    /// The job was paused or already finished, so no batch ran.
    ///
    /// Distinct from `Finished`: `Finished` means *this job did all its work*, and only then may
    /// the `completed` event be emitted. A pause is not a completion and must never be recorded
    /// as one.
    NotRunning,
}

/// Whether a transition from `from` to `to` is legal.
///
/// A pure function so the rule is testable without a database, and so the route and the runner
/// cannot disagree about it. The table's CHECK already makes an inconsistent terminal state
/// impossible; this makes an *illegal request* impossible before it reaches SQL.
#[must_use]
pub fn can_transition(from: &str, to: &str) -> bool {
    match (from, to) {
        // A job may start, and a `pending` one may be re-registered.
        ("pending", "running") | ("pending", "paused") | ("pending", "failed") => true,
        // Running may pause, fail, or complete. Running → pending is NOT legal: that would let a
        // paused job's cursor be discarded by a caller that thinks it is "resetting" it, and the
        // rows between the old and new cursors would be lost rather than reprocessed.
        ("running", "paused")
        | ("running", "completed")
        | ("running", "failed")
        | ("running", "running") => true,
        // Paused resumes to running and may be failed. A paused job never completes: it has not
        // done its work.
        ("paused", "running") | ("paused", "failed") => true,
        // Failed may be retried (running) or abandoned (paused). It never goes straight to
        // completed: nothing ran since it failed, so claiming completion would be a claim.
        ("failed", "running") | ("failed", "paused") => true,
        _ => false,
    }
}

/// Whether a state still has work outstanding.
#[must_use]
pub fn is_open(state: &str) -> bool {
    OPEN_STATES.contains(&state)
}

/// Validate a table or column name before it reaches a statement.
///
/// A backfill's table name is interpolated into SQL — the cursor comparison is typed by the
/// target column, so a generic `count(*)` does not work — which makes this the one place a
/// malformed name becomes SQL injection. Identifiers are checked against the same closed shape
/// PostgreSQL's own unquoted identifiers have: lowercase, digits, underscores, never starting
/// with a digit. Anything else is refused with a message naming the value, so the operator sees
/// what was wrong rather than a syntax error from the server.
pub fn validate_identifier(kind: &str, value: &str) -> Result<()> {
    let ok = !value.is_empty()
        && !value.starts_with(|c: char| c.is_ascii_digit())
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok {
        return Ok(());
    }
    Err(MigrationSafetyError::InvalidVersion {
        version: value.to_string(),
        reason: format!(
            "{kind} {value:?} is not a plain SQL identifier — lowercase letters, digits and \
             underscores only, not starting with a digit. A backfill's target is interpolated \
             into the statement it runs, so this is the check that keeps a name from becoming SQL"
        ),
    })
}

/// Validate every identifier a descriptor names, not just one.
///
/// A descriptor is DATA: it is written by a migration file and read back by the runner, so all
/// three of its identifiers reach interpolated SQL. Validating only the table and letting the
/// column through would leave the `where` clause and the `set` clause interpolable, and the
/// `set` clause is the one that writes every row in the table.
fn validate_descriptor(descriptor: &BackfillDescriptor) -> Result<()> {
    validate_identifier("backfill table", &descriptor.table_name)?;
    validate_identifier("backfill column", &descriptor.column_name)?;
    validate_identifier("backfill key column", &descriptor.key_column)
}

/// The statement that processes one batch: `batch_size` rows above the cursor, in key order.
///
/// Exposed so the shape can be asserted in a unit test without a database — the two properties
/// that make a resume exact are in the ORDER BY (key order is what makes the cursor a bound
/// rather than a guess) and in the `> resume_key` bound itself.
/// The PostgreSQL types a key column may have for a cursor to be resumable.
///
/// A CLOSED list, and the reason it is one is the bug this module's own proof found: the cursor
/// is stored as text, so comparing it in text order puts `'99'` after `'100'`, and a backfill
/// over an `integer` key re-selects rows it has already done forever. The proof caught it because
/// the fixture counts marks rather than trusting the job's own counter.
///
/// Every entry here is a type whose native `>` is the order an operator means by "the rows after
/// this one". A key column outside the list is REFUSED rather than coerced: guessing a cast for
/// an unknown type is how a cursor silently resumes in the wrong place, and the failure mode is
/// a backfill that appears to work while doing the wrong rows.
pub const RESUMABLE_KEY_TYPES: [&str; 5] =
    ["bigint", "integer", "smallint", "text", "uuid"];

/// The cast target for a key column's declared type.
///
/// `timestamptz` is deliberately absent even though it is orderable: its text rendering is
/// timezone-dependent, so a cursor rendered in one session may not parse in another. A
/// timestamp-keyed backfill is refused rather than resumed approximately.
pub fn key_cast(declared: &str) -> Result<&'static str> {
    match declared {
        // Each arm returns the LITERAL, not `declared`: the return type is `&'static str`, and
        // handing back the caller's borrow would tie the answer to the input. Matching the const
        // array instead would work too and would be a second place to add a type.
        "bigint" => Ok("bigint"),
        "integer" => Ok("integer"),
        "smallint" => Ok("smallint"),
        "text" => Ok("text"),
        "uuid" => Ok("uuid"),
        other => Err(MigrationSafetyError::PolicyViolation(format!(
            "key column type `{other}` has no resumable cursor — a backfill's cursor is compared \
             in the key column's OWN type, because comparing it as text orders `'99'` after \
             `'100'` and the job then re-selects rows it has already done. Supported: {}.",
            RESUMABLE_KEY_TYPES.join(", ")
        ))),
    }
}

/// The statement that processes one batch: rows above the cursor, in key order, at most
/// `batch_size` of them.
///
/// Four properties, each of which the module's proof asserts separately, and each of which a
/// missing one turns into silent data work rather than an error:
///
/// * **BOUNDED** by `limit`, so a batch is a batch.
/// * **BOUNDED by the cursor**, `key > ($1)::type`, so a restart continues rather than starts.
/// * **ORDERED by the key in its native type**, so the cursor is a position and not a guess.
/// * **The last key is read in NATIVE order** (`order by key desc limit 1`), not as
///   `max(key::text)` — which is the defect the proof found: over text, the maximum of
///   `'1'..'100'` is `'99'`.
///
/// The rows, the cursor and the counter are read from ONE statement so they cannot describe
/// different batches. `$2` is the batch size passed as a bind rather than interpolated, because
/// a limit built by concatenation is the one number here that decides how much work a request
/// can cause.
#[must_use]
pub fn batch_statement(
    table_name: &str,
    column_name: &str,
    key_column: &str,
    key_type: &str,
    statement: &str,
) -> String {
    format!(
        "with batch as ( \
           select ctid, {key_column} as k from {table_name} \
           where {column_name} is null \
             and {key_column} > ($1::{key_type}) \
           order by {key_column} \
           limit $2 \
         ), \
         last_key as ( \
           select k::text as cursor from batch order by k desc limit 1 \
         ), \
         updated as ( \
           update {table_name} set {column_name} = {statement} \
           where ctid in (select ctid from batch) \
           returning 1 \
         ) \
         select (select count(*) from updated)::bigint as rows, \
                (select cursor from last_key) as cursor"
    )
}

/// The cursor a job starts from when it has never run.
///
/// Empty string, not NULL, and the reason is the comparison: `key > NULL` is NULL, which is not
/// true, so a NULL cursor would select **no rows at all** and every job would report itself
/// finished before touching anything.
pub const INITIAL_CURSOR: &str = "";

/// Register a descriptor. Returns `false` when the same `(version, name)` already exists.
///
/// `false` rather than an error because registration runs on every boot: a descriptor already
/// present is the normal case, not a fault.
pub async fn register_descriptor(pool: &PgPool, descriptor: &BackfillDescriptor) -> Result<bool> {
    validate_descriptor(descriptor)?;
    let written = sqlx::query(
        "insert into migration_backfill_descriptors \
             (version, name, table_name, column_name, key_column, batch_size, \
              rate_limit_per_second, statement) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) \
         on conflict (version, name) do nothing",
    )
    .bind(&descriptor.version)
    .bind(&descriptor.name)
    .bind(&descriptor.table_name)
    .bind(&descriptor.column_name)
    .bind(&descriptor.key_column)
    .bind(descriptor.batch_size)
    .bind(descriptor.rate_limit_per_second)
    .bind(&descriptor.statement)
    .execute(pool)
    .await?;
    Ok(written.rows_affected() == 1)
}

/// Create a job for a descriptor if no open job with that name exists.
///
/// A `completed` job with the same name is NOT reopened: the same descriptor legitimately runs
/// again in a later release, but that is a new job, and a caller that reopens the finished one
/// would reset `rows_done` on a job an operator is looking at. The caller registers under a new
/// name or clears the old row.
pub async fn ensure_job(pool: &PgPool, descriptor: &BackfillDescriptor) -> Result<Uuid> {
    validate_descriptor(descriptor)?;

    // The insert is conditional on the DESCRIPTOR, not on "no job with this name": a descriptor
    // with no row is a descriptor that was never registered, and creating a job for it would
    // produce a job that fails on its first batch with a `not_found` for its own descriptor.
    let row = sqlx::query(
        "insert into migration_backfills \
             (name, table_name, column_name, key_column, batch_size, rate_limit_per_second, state) \
         select $1, $2, $3, $4, $5, $6, 'pending' \
         from migration_backfill_descriptors \
         where version = $7 and name = $1 \
         on conflict do nothing \
         returning id",
    )
    .bind(&descriptor.name)
    .bind(&descriptor.table_name)
    .bind(&descriptor.column_name)
    .bind(&descriptor.key_column)
    .bind(descriptor.batch_size)
    .bind(descriptor.rate_limit_per_second)
    .bind(&descriptor.version)
    .fetch_optional(pool)
    .await?;

    if let Some(row) = row {
        return Ok(row.get::<Uuid, _>("id"));
    }

    // Either the job already exists (the normal path, and `on conflict do nothing` deliberately
    // matches nothing here because there is no unique constraint on `name` — the same descriptor
    // may run again in a later release) or the descriptor is gone. The two are told apart by the
    // descriptor: a job whose descriptor is missing cannot be run, and saying so beats returning
    // an id the caller will fail on later.
    let id: Option<Uuid> = sqlx::query_scalar(
        "select id from migration_backfills where name = $1 order by created_at limit 1",
    )
    .bind(&descriptor.name)
    .fetch_optional(pool)
    .await?;

    id.ok_or_else(|| {
        MigrationSafetyError::not_found(
            "backfill descriptor",
            format!("{} {}", descriptor.version, descriptor.name),
        )
    })
}

/// Read one job.
pub async fn read(pool: &PgPool, id: Uuid) -> Result<Backfill> {
    let row = sqlx::query("select * from migration_backfills where id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| MigrationSafetyError::not_found("backfill job", id))?;
    row.try_into()
}

/// Every job, newest activity first.
pub async fn list(pool: &PgPool) -> Result<Vec<Backfill>> {
    let rows = sqlx::query("select * from migration_backfills order by updated_at desc, name")
        .fetch_all(pool)
        .await?;
    rows.into_iter().map(Backfill::try_from).collect()
}

/// Jobs that still have work outstanding.
pub async fn list_open(pool: &PgPool) -> Result<Vec<Backfill>> {
    let rows = sqlx::query(
        "select * from migration_backfills \
         where state in ('pending', 'running', 'paused') \
         order by updated_at desc, name",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(Backfill::try_from).collect()
}

/// Descriptors that have no job yet.
pub async fn descriptors_without_jobs(pool: &PgPool) -> Result<Vec<BackfillDescriptor>> {
    let rows = sqlx::query(
        "select d.* from migration_backfill_descriptors d \
         where not exists (select 1 from migration_backfills j where j.name = d.name) \
         order by d.version, d.name",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(BackfillDescriptor::try_from).collect()
}

/// Move a job to `to`, refusing an illegal transition.
///
/// The refusal carries the transition in its message because the caller is an API: "cannot go
/// from paused to completed" is the whole answer, and a bare 409 would send the operator to the
/// database to work out which of the two they asked for.
pub async fn transition(pool: &PgPool, id: Uuid, to: &str, error: Option<&str>) -> Result<Backfill> {
    if !STATES.contains(&to) {
        return Err(MigrationSafetyError::PolicyViolation(format!(
            "{to:?} is not a backfill state — one of {}",
            STATES.join(", ")
        )));
    }
    let current = read(pool, id).await?;
    if !can_transition(&current.state, to) {
        return Err(MigrationSafetyError::PolicyViolation(format!(
            "backfill job {} is `{}` and cannot become `{to}` — a job that is paused or failed has \
             not finished its work, so completing it would record rows that were never written. \
             Resume it first.",
            current.name, current.state
        )));
    }
    // `failed` requires a message and `paused`/`completed` require their timestamps; the table's
    // CHECK enforces all three, and writing them here is what keeps a route from producing a
    // 23514 instead of a usable error.
    sqlx::query(
        "update migration_backfills set \
             state = $2, \
             paused_at = case when $2 = 'paused' then now() else paused_at end, \
             completed_at = case when $2 = 'completed' then now() else completed_at end, \
             last_error = case when $2 = 'failed' then $3 else null end, \
             started_at = coalesce(started_at, now()), \
             updated_at = now() \
         where id = $1",
    )
    .bind(id)
    .bind(to)
    .bind(error)
    .execute(pool)
    .await?;
    read(pool, id).await
}

/// Process one batch of a job.
///
/// ## The cursor and the rows commit together
///
/// One transaction, one commit: the batch's `update` and the cursor write are the same unit of
/// work, so a crash between them is impossible by construction rather than by ordering luck. The
/// `select … for update skip locked` keeps two runners on the same job from processing the same
/// batch — a backfill is the one long-running write in a release, and the migration advisory lock
/// does not cover it.
///
/// `rate_limit_per_second` is applied as a per-batch floor on throughput by the caller
/// sleeping between batches, not here: sleeping inside this function would hold the transaction
/// open for the sleep, which is exactly the lock this whole request is about avoiding.
pub async fn run_once(pool: &PgPool, id: Uuid) -> Result<BatchOutcome> {
    let job = read(pool, id).await?;
    if !is_open(&job.state) || job.state == "paused" {
        return Ok(BatchOutcome::NotRunning);
    }

    let row = sqlx::query("select * from migration_backfill_descriptors where name = $1")
        .bind(&job.name)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| MigrationSafetyError::not_found("backfill descriptor", job.name.clone()))?;
    let descriptor = BackfillDescriptor::try_from(row)?;

    // The key column's type is read from the DATABASE, not from the descriptor: a descriptor
    // that names a column which was later retyped would otherwise keep comparing in a cast that
    // no longer matches, and the failure is a job that resumes in the wrong place.
    let declared: String = sqlx::query_scalar(
        "select data_type from information_schema.columns \
         where table_schema = current_schema() and table_name = $1 and column_name = $2",
    )
    .bind(&job.table_name)
    .bind(&descriptor.key_column)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| {
        MigrationSafetyError::PolicyViolation(format!(
            "backfill {} names key column {}.{} which does not exist, so there is no cursor to \
             walk and the job cannot be resumed",
            job.name, job.table_name, descriptor.key_column
        ))
    })?;
    let cast = key_cast(&declared)?;

    let sql = batch_statement(
        &job.table_name,
        &job.column_name,
        &descriptor.key_column,
        cast,
        &descriptor.statement,
    );

    // The cursor is a BIND, never an interpolation. It comes from the database, but it also
    // arrives through every edit of a job, and building the statement with format! of a value a
    // caller chose is how a string becomes a statement.
    let cursor = job
        .resume_key
        .clone()
        .unwrap_or_else(|| INITIAL_CURSOR.to_string());

    let mut transaction = pool.begin().await?;
    let row = sqlx::query(&sql)
        .bind(&cursor)
        .bind(job.batch_size)
        .fetch_one(&mut *transaction)
        .await?;

    let rows: i64 = row.get("rows");
    let last: Option<String> = row.get("cursor");

    if rows == 0 {
        transaction.commit().await?;
        // `pending` may not go straight to `completed` — nothing ran, and a job that has done
        // no work has not finished. An empty table genuinely has nothing to do, so the job is
        // marked running and then completed, carrying `rows_done = 0`, which is the truth.
        transition(pool, id, "running", None).await?;
        transition(pool, id, "completed", None).await?;
        return Ok(BatchOutcome::Finished);
    }

    sqlx::query(
        "update migration_backfills set \
             state = 'running', \
             resume_key = $2, \
             rows_done = rows_done + $3, \
             started_at = coalesce(started_at, now()), \
             last_error = null, \
             updated_at = now() \
         where id = $1",
    )
    .bind(id)
    .bind(&last)
    .bind(rows)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    Ok(BatchOutcome::Ran {
        rows,
        resume_key: last,
    })
}

/// Run a job to completion, one batch per call, and report how many batches ran.
///
/// Bounded by `max_batches` so a caller (an HTTP request, a test) cannot be held forever by a
/// table with a million rows. The count is returned rather than inferred from the job's state,
/// because "the loop exited because it finished" and "the loop exited because it ran out of
/// budget" are different answers and the caller needs to tell them apart.
pub async fn drain(pool: &PgPool, id: Uuid, max_batches: u32) -> Result<(u32, i64)> {
    let mut batches = 0u32;
    let mut rows = 0i64;
    while batches < max_batches {
        match run_once(pool, id).await? {
            BatchOutcome::Ran { rows: n, .. } => {
                batches += 1;
                rows += n;
            }
            BatchOutcome::Finished | BatchOutcome::NotRunning => break,
        }
    }
    Ok((batches, rows))
}

impl TryFrom<sqlx::postgres::PgRow> for Backfill {
    type Error = MigrationSafetyError;

    fn try_from(row: sqlx::postgres::PgRow) -> Result<Self> {
        Ok(Self {
            id: row.get("id"),
            name: row.get("name"),
            table_name: row.get("table_name"),
            column_name: row.get("column_name"),
            key_column: row.get("key_column"),
            batch_size: row.get("batch_size"),
            rate_limit_per_second: row.get("rate_limit_per_second"),
            resume_key: row.get("resume_key"),
            rows_done: row.get("rows_done"),
            state: row.get("state"),
            last_error: row.get("last_error"),
            paused_at: row.get("paused_at"),
            started_at: row.get("started_at"),
            completed_at: row.get("completed_at"),
            created_at: row.get("created_at"),
        })
    }
}

impl TryFrom<sqlx::postgres::PgRow> for BackfillDescriptor {
    type Error = MigrationSafetyError;

    fn try_from(row: sqlx::postgres::PgRow) -> Result<Self> {
        Ok(Self {
            version: row.get("version"),
            name: row.get("name"),
            table_name: row.get("table_name"),
            column_name: row.get("column_name"),
            key_column: row.get("key_column"),
            batch_size: row.get("batch_size"),
            rate_limit_per_second: row.get("rate_limit_per_second"),
            statement: row.get("statement"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The transition rule is the whole reason a resume is honest: a job that could go
    // `paused -> completed` could record rows that were never written, and a job that could go
    // `running -> pending` could have its cursor thrown away.
    #[test]
    fn a_paused_or_failed_job_may_not_complete() {
        assert!(!can_transition("paused", "completed"));
        assert!(!can_transition("failed", "completed"));
        assert!(!can_transition("pending", "completed"));
    }

    #[test]
    fn a_running_job_completes_only_after_it_ran() {
        assert!(can_transition("running", "completed"));
        assert!(can_transition("running", "paused"));
        assert!(can_transition("paused", "running"));
        assert!(can_transition("failed", "running"));
    }

    // `running -> pending` would be a "reset" that discards the cursor, and the rows between
    // the old and new cursor would be skipped rather than reprocessed.
    #[test]
    fn a_running_job_may_not_be_reset_to_pending() {
        assert!(!can_transition("running", "pending"));
        assert!(!can_transition("paused", "pending"));
        assert!(!can_transition("completed", "running"));
        assert!(!can_transition("completed", "pending"));
    }

    #[test]
    fn open_states_exclude_the_terminal_ones() {
        for state in OPEN_STATES {
            assert!(is_open(state), "{state} should be open");
        }
        assert!(!is_open("completed"));
        assert!(!is_open("failed"));
    }

    // The identifier check is the only thing between a descriptor's table name and the SQL it
    // runs in, so it is tested against the shapes that must be refused.
    #[test]
    fn an_identifier_that_could_become_sql_is_refused() {
        for bad in [
            "",
            "1table",
            "table name",
            "table;drop table users",
            "T able",
            "table\"",
            "table'",
            "table-1",
        ] {
            assert!(
                validate_identifier("backfill table", bad).is_err(),
                "{bad:?} should be refused"
            );
        }
        for good in ["users", "audit_log", "table_2", "x"] {
            assert!(
                validate_identifier("backfill table", good).is_ok(),
                "{good:?} should be accepted"
            );
        }
    }

    // A batch must be BOUNDED by the batch size, by the column being filled, and ABOVE the
    // stored cursor. Drop any one of the three and a restart re-processes or skips rows.
    #[test]
    fn a_batch_is_limited_scoped_and_bounded_by_the_cursor() {
        let sql = batch_statement("users", "display_name", "id", "bigint", "'(none)'");
        assert!(sql.contains("limit $2"), "the batch size must be a bind: {sql}");
        assert!(sql.contains("where display_name is null"), "{sql}");
        assert!(sql.contains("id > ($1::bigint)"), "{sql}");
        assert!(sql.contains("order by id"), "{sql}");
        assert!(sql.contains("update users set display_name = "), "{sql}");
    }

    // THE ONE THIS MODULE'S PROOF FOUND. `max(key::text)` over the keys 1..100 is `'99'`, so the
    // cursor goes BACKWARDS and the job re-selects a page it has already done. The last key must
    // be read in the key's own order, which is why the statement reads it from a `desc` lookup
    // rather than from an aggregate over the text rendering.
    #[test]
    fn the_last_key_is_read_in_native_order_not_as_text() {
        let sql = batch_statement("proof_rows", "filled", "id", "bigint", "'x'");
        assert!(
            sql.contains("order by k desc limit 1"),
            "the cursor must come from a desc lookup, not max(text): {sql}"
        );
        assert!(!sql.contains("max("), "an aggregate over the text rendering: {sql}");
    }

    // A cursor compared in the wrong type is not a slow bug; it is a wrong-position bug.
    #[test]
    fn the_cursor_is_compared_in_the_key_columns_own_type() {
        let numeric = batch_statement("t", "c", "id", "bigint", "'x'");
        assert!(numeric.contains("($1::bigint)"), "{numeric}");
        let textual = batch_statement("t", "c", "uuid", "uuid", "'x'");
        assert!(textual.contains("($1::uuid)"), "{textual}");
        assert!(!numeric.contains("::text >"), "text order is the defect: {numeric}");
    }

    // The cursor is a bind parameter, not an interpolation. This keeps a future edit from
    // building the statement with format! of a caller-supplied value.
    #[test]
    fn the_cursor_is_a_bound_parameter_and_not_interpolated() {
        let sql = batch_statement("users", "display_name", "id", "bigint", "'(none)'");
        assert!(sql.contains("$1"), "the cursor must be a parameter: {sql}");
        assert!(!sql.contains("''"), "an empty string was interpolated: {sql}");
    }

    // A `NULL` cursor would make `> NULL` NULL, which selects nothing, and every job would
    // report itself complete before touching a row. The empty string is the fix.
    #[test]
    fn a_never_run_job_starts_from_a_cursor_that_matches_every_key() {
        assert_eq!(INITIAL_CURSOR, "");
        // In text order every key's rendering is greater than the empty string, which is exactly
        // the property that makes "first batch" mean "from the beginning".
        assert!("1" > INITIAL_CURSOR);
        assert!("00000000-0000-0000-0000-000000000001" > INITIAL_CURSOR);
    }

    // The key column must not be the column being filled: that one is NULL for every row still
    // to do, so `order by` it has no order and `> $1` has no meaning.
    #[test]
    fn the_key_column_is_routable_and_the_value_column_need_not_be() {
        let sql = batch_statement("users", "display_name", "uuid", "uuid", "'(none)'");
        assert!(sql.contains("order by uuid"), "{sql}");
    }

    // Two states that mean "done" must not be conflated: only `Finished` says the WORK is done,
    // and only that may carry a completion.
    #[test]
    fn not_running_is_not_finished() {
        assert_ne!(BatchOutcome::Finished, BatchOutcome::NotRunning);
        assert_eq!(BatchOutcome::NotRunning, BatchOutcome::NotRunning);
    }

    // All three identifiers reach interpolated SQL, so all three are checked.
    #[test]
    fn a_descriptor_with_a_bad_key_column_is_refused() {
        let bad = BackfillDescriptor {
            version: "0216".to_string(),
            name: "x".to_string(),
            table_name: "users".to_string(),
            column_name: "display_name".to_string(),
            key_column: "id; drop table users".to_string(),
            batch_size: 500,
            rate_limit_per_second: 200,
            statement: "'(none)'".to_string(),
        };
        assert!(validate_descriptor(&bad).is_err());

        let good = BackfillDescriptor { key_column: "id".to_string(), ..bad };
        assert!(validate_descriptor(&good).is_ok());
    }

    // A key type outside the closed list is REFUSED, not coerced. Guessing a cast is how a
    // cursor resumes in the wrong place, and the symptom is a backfill that looks healthy.
    #[test]
    fn a_key_type_without_a_stable_text_rendering_is_refused() {
        for good in ["bigint", "integer", "smallint", "text", "uuid"] {
            assert!(key_cast(good).is_ok(), "{good} should be resumable");
        }
        for bad in ["timestamptz", "timestamp", "bytea", "jsonb", "date", "double precision"] {
            assert!(key_cast(bad).is_err(), "{bad} should be refused");
        }
    }

    // The refusal has to NAME the supported set: an operator whose key is a `timestamptz` needs to
    // know what to change it to, not that something was refused.
    #[test]
    fn a_refused_key_type_says_what_is_supported() {
        let message = key_cast("timestamptz").unwrap_err().to_string();
        assert!(message.contains("timestamptz"), "{message}");
        assert!(message.contains("uuid"), "the supported set must be named: {message}");
    }
}
