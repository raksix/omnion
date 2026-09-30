//! The store of record for retry policies and the attempt ledger (REQ-127, slice 3).
//!
//! [`crate::retry`] is the pure state machine: given a policy, an attempt number and a failure it
//! says what to do next, with no I/O and no clock of its own. This module is the other half —
//! where that decision is *written*, and the three properties the request asks for are properties
//! of this file rather than of the caller:
//!
//! * **Policies are editable without a deploy.** [`upsert_policy`] is keyed on
//!   `(subsystem, provider_override)`, which is exactly the `unique nulls not distinct` constraint
//!   the migration carries, so a provider override and a subsystem default are one row each and
//!   neither can be silently duplicated.
//! * **A restart resumes instead of replaying or losing an attempt.** [`record_outcome`] writes
//!   `next_attempt_at` on the job row itself. A worker that dies between attempts comes back,
//!   reads the ledger, and continues; nothing is held in a queue's head position.
//! * **Exhaustion produces exactly one dead letter.** The `retry_outcomes_dead_letter` index is
//!   partial on `dead_letter`, and [`record_outcome`] is the only writer that sets it, so
//!   "one dead letter with the full timeline" is a query over one table
//!   ([`load_timeline`], [`load_dead_letters`]) rather than a second store that could disagree.
//!
//! ## Why the timeline is read from the ledger and not reassembled from a job table
//!
//! The tempting design keeps a `job` row and rewrites its `attempts` column. The migration says
//! no, and for a reason worth repeating: the attempts are a *history*, and a history written in
//! place is a history that can only hold the last one. Every attempt is its own row, append-only,
//! so the timeline is the truth and the dead letter is a flag on the row that ended the sequence.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{ReliabilityError, Result};
use crate::retry::Policy;
use crate::vocabulary::MAX_PAGE;

/// `numeric(4,2)` in the migration decodes into a `rust_decimal`-free crate as a string or f64
/// depending on the driver, so the column is read as `f64` here after an explicit cast in the
/// `select` — see [`PolicyRow`].
#[derive(Debug, sqlx::FromRow)]
struct PolicyRow {
    id: Uuid,
    subsystem: String,
    provider_override: Option<String>,
    max_attempts: i32,
    base_delay_ms: i32,
    factor: f64,
    jitter: String,
    max_elapse_ms: i64,
    retry_on: serde_json::Value,
    enabled: bool,
}

impl From<PolicyRow> for Policy {
    fn from(row: PolicyRow) -> Self {
        let retry_on = row
            .retry_on
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_bool().unwrap_or(false)))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            subsystem: row.subsystem,
            provider_override: row.provider_override,
            max_attempts: row.max_attempts,
            // `base_delay_ms` is `int` in the migration and the domain field is `i64`: the same
            // INT4/INT8 bind mismatch `store.rs` documents, and the same fix.
            base_delay_ms: i64::from(row.base_delay_ms),
            factor: row.factor,
            jitter: row.jitter,
            max_elapse_ms: row.max_elapse_ms,
            retry_on,
            enabled: row.enabled,
        }
    }
}

#[derive(Debug, sqlx::FromRow)]
struct OutcomeRow {
    id: i64,
    subsystem: String,
    subject_kind: String,
    subject_id: Option<String>,
    attempt: i32,
    scheduled_at: Option<OffsetDateTime>,
    executed_at: Option<OffsetDateTime>,
    outcome: String,
    error_class: Option<String>,
    next_delay_ms: Option<i32>,
    dead_letter: bool,
    next_attempt_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
}

/// One row of the attempt ledger, as the panel's timeline and dead-letter list read it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AttemptRecord {
    pub id: i64,
    pub subsystem: String,
    pub subject_kind: String,
    pub subject_id: Option<String>,
    pub attempt: i32,
    pub scheduled_at: Option<OffsetDateTime>,
    pub executed_at: Option<OffsetDateTime>,
    pub outcome: String,
    pub error_class: Option<String>,
    pub next_delay_ms: Option<i64>,
    pub dead_letter: bool,
    pub next_attempt_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
}

impl From<OutcomeRow> for AttemptRecord {
    fn from(row: OutcomeRow) -> Self {
        Self {
            id: row.id,
            subsystem: row.subsystem,
            subject_kind: row.subject_kind,
            subject_id: row.subject_id,
            attempt: row.attempt,
            scheduled_at: row.scheduled_at,
            executed_at: row.executed_at,
            outcome: row.outcome,
            error_class: row.error_class,
            next_delay_ms: row.next_delay_ms.map(i64::from),
            dead_letter: row.dead_letter,
            next_attempt_at: row.next_attempt_at,
            created_at: row.created_at,
        }
    }
}

/// Read every policy, enabled or not.
///
/// The panel needs the disabled ones ("why is this subsystem not retrying?") and the request path
/// needs the enabled ones; two functions rather than a flag, for the reason `store.rs` gives.
pub async fn load_policies(pool: &PgPool) -> Result<Vec<Policy>> {
    let rows = sqlx::query_as::<_, PolicyRow>(
        "select id, subsystem, provider_override, max_attempts, base_delay_ms, \
                factor::float8 as factor, jitter, max_elapse_ms, retry_on, enabled \
           from retry_policies \
          order by subsystem, provider_override nulls first",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Policy::from).collect())
}

/// The policies a subsystem actually runs: its own row, plus every provider override.
///
/// Both are returned, not merged: the override is a *different document* an operator wrote, and
/// folding it into the base would make it un-editable and un-deletable from the screen that lists
/// it. [`resolve_policy`] is where the choice is made, once, for every caller.
pub async fn load_enabled_policies(pool: &PgPool) -> Result<Vec<Policy>> {
    Ok(load_policies(pool)
        .await?
        .into_iter()
        .filter(|p| p.enabled)
        .collect())
}

/// The one policy in force for a subsystem, optionally with a provider's override applied.
///
/// The lookup is ordered, not merged: an override for the named provider wins outright, otherwise
/// the subsystem's own row wins, otherwise nothing is returned and the caller decides what an
/// absent policy means. `default_policy_for` is the in-process fallback, used by the scheduler
/// before a subsystem has ever been edited.
pub async fn resolve_policy(
    pool: &PgPool,
    subsystem: &str,
    provider: Option<&str>,
) -> Result<Option<Policy>> {
    let row = sqlx::query_as::<_, PolicyRow>(
        "select id, subsystem, provider_override, max_attempts, base_delay_ms, \
                factor::float8 as factor, jitter, max_elapse_ms, retry_on, enabled \
           from retry_policies \
          where subsystem = $1 and provider_override is not distinct from $2 \
          limit 1",
    )
    .bind(subsystem)
    .bind(provider)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Policy::from))
}

/// Write a policy, replacing any row already keyed on the same `(subsystem, provider_override)`.
///
/// The `on conflict ... do update` names the constraint's *columns*, never its generated name:
/// `nulls not distinct` indexes cannot be inferred by `on conflict do update` without them, and
/// letting the driver guess produces `there is no unique or exclusion constraint matching` at
/// runtime rather than at build time.
pub async fn upsert_policy(pool: &PgPool, policy: &Policy) -> Result<Policy> {
    policy.validate()?;
    let row = sqlx::query_as::<_, PolicyRow>(
        "insert into retry_policies \
             (subsystem, provider_override, max_attempts, base_delay_ms, factor, jitter, \
              max_elapse_ms, retry_on, enabled, updated_at) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, now()) \
         on conflict (subsystem, provider_override) do update set \
             max_attempts = excluded.max_attempts, \
             base_delay_ms = excluded.base_delay_ms, \
             factor = excluded.factor, \
             jitter = excluded.jitter, \
             max_elapse_ms = excluded.max_elapse_ms, \
             retry_on = excluded.retry_on, \
             enabled = excluded.enabled, \
             updated_at = now() \
         returning id, subsystem, provider_override, max_attempts, base_delay_ms, \
                   factor::float8 as factor, jitter, max_elapse_ms, retry_on, enabled",
    )
    .bind(&policy.subsystem)
    .bind(&policy.provider_override)
    .bind(policy.max_attempts)
    .bind(policy.base_delay_ms as i32)
    .bind(policy.factor)
    .bind(&policy.jitter)
    .bind(policy.max_elapse_ms)
    .bind(serde_json::to_value(&policy.retry_on).unwrap_or_else(|_| serde_json::json!({})))
    .bind(policy.enabled)
    .fetch_one(pool)
    .await?;
    Ok(Policy::from(row))
}

/// Delete a policy, or return `None` when it was not there.
///
/// Deleting the subsystem default is allowed: an absent policy is not a misconfiguration, it is
/// the platform saying "use the in-process default", and the screen says which one that is.
pub async fn delete_policy(
    pool: &PgPool,
    subsystem: &str,
    provider_override: Option<&str>,
) -> Result<bool> {
    let result = sqlx::query(
        "delete from retry_policies where subsystem = $1 and provider_override is not distinct from $2",
    )
    .bind(subsystem)
    .bind(provider_override)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Append one attempt to the ledger and return it.
///
/// `next_attempt_at` is written **here, on the row**, in the same statement as the outcome — the
/// persistence contract the request asks for. Writing the outcome and the next time separately
/// would leave a window in which a crash between them loses the retry entirely, which is the one
/// failure a retry subsystem exists to prevent.
#[allow(clippy::too_many_arguments)]
pub async fn record_outcome(
    pool: &PgPool,
    subsystem: &str,
    subject_kind: &str,
    subject_id: Option<&str>,
    attempt: i32,
    outcome: &str,
    error_class: Option<&str>,
    next_delay_ms: Option<i64>,
    next_attempt_at: Option<OffsetDateTime>,
    dead_letter: bool,
) -> Result<AttemptRecord> {
    if attempt < 1 {
        return Err(ReliabilityError::invalid("attempt must be 1 or greater"));
    }
    let row = sqlx::query_as::<_, OutcomeRow>(
        "insert into retry_outcomes \
             (subsystem, subject_kind, subject_id, attempt, executed_at, outcome, error_class, \
              next_delay_ms, next_attempt_at, dead_letter) \
         values ($1, $2, $3, $4, now(), $5, $6, $7, $8, $9) \
         returning id, subsystem, subject_kind, subject_id, attempt, scheduled_at, executed_at, \
                   outcome, error_class, next_delay_ms, dead_letter, next_attempt_at, created_at",
    )
    .bind(subsystem)
    .bind(subject_kind)
    .bind(subject_id)
    .bind(attempt)
    .bind(outcome)
    .bind(error_class)
    .bind(next_delay_ms.map(|d| i32::try_from(d).unwrap_or(i32::MAX)))
    .bind(next_attempt_at)
    .bind(dead_letter)
    .fetch_one(pool)
    .await?;
    Ok(AttemptRecord::from(row))
}

/// The full attempt history of one subject, oldest first.
///
/// Ordered by `attempt` and not by `created_at`: `now()` is transaction-stable in PostgreSQL, so
/// every attempt of a single-transaction replay carries the *same* timestamp, and a timeline
/// ordered by it is a timeline whose order the reader cannot reproduce.
pub async fn load_timeline(
    pool: &PgPool,
    subject_kind: &str,
    subject_id: &str,
) -> Result<Vec<AttemptRecord>> {
    let rows = sqlx::query_as::<_, OutcomeRow>(
        "select id, subsystem, subject_kind, subject_id, attempt, scheduled_at, executed_at, \
                outcome, error_class, next_delay_ms, dead_letter, next_attempt_at, created_at \
           from retry_outcomes \
          where subject_kind = $1 and subject_id = $2 \
          order by attempt, id",
    )
    .bind(subject_kind)
    .bind(subject_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(AttemptRecord::from).collect())
}

/// The dead-letter list, newest first.
pub async fn load_dead_letters(pool: &PgPool, limit: usize) -> Result<Vec<AttemptRecord>> {
    let limit = limit.clamp(1, MAX_PAGE) as i64;
    let rows = sqlx::query_as::<_, OutcomeRow>(
        "select id, subsystem, subject_kind, subject_id, attempt, scheduled_at, executed_at, \
                outcome, error_class, next_delay_ms, dead_letter, next_attempt_at, created_at \
           from retry_outcomes \
          where dead_letter \
          order by created_at desc, id desc \
          limit $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(AttemptRecord::from).collect())
}

/// The most recent attempts, for the screen's ledger tab.
pub async fn load_recent(pool: &PgPool, limit: usize) -> Result<Vec<AttemptRecord>> {
    let limit = limit.clamp(1, MAX_PAGE) as i64;
    let rows = sqlx::query_as::<_, OutcomeRow>(
        "select id, subsystem, subject_kind, subject_id, attempt, scheduled_at, executed_at, \
                outcome, error_class, next_delay_ms, dead_letter, next_attempt_at, created_at \
           from retry_outcomes \
          order by created_at desc, id desc \
          limit $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(AttemptRecord::from).collect())
}

/// The next attempt time for a subject, or `None` when the sequence is finished.
///
/// **This is the restart contract.** A worker that comes back calls exactly this and learns whether
/// there is work still owed; it does not consult a queue, because a queue position is not durable
/// and a job whose next time passed while the process was down is overdue, not lost.
pub async fn next_attempt_time(
    pool: &PgPool,
    subject_kind: &str,
    subject_id: &str,
) -> Result<Option<OffsetDateTime>> {
    let row: Option<(Option<OffsetDateTime>,)> = sqlx::query_as(
        "select next_attempt_at from retry_outcomes \
          where subject_kind = $1 and subject_id = $2 and next_attempt_at is not null \
          order by attempt desc, id desc \
          limit 1",
    )
    .bind(subject_kind)
    .bind(subject_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0).flatten())
}

/// How many attempts a subject has recorded — the number `next_attempt` needs on resume.
///
/// Read from the ledger rather than kept in memory precisely so the answer survives a restart: a
/// scheduler that counts attempts in a field it owns loses the count with the process.
pub async fn attempt_count(pool: &PgPool, subject_kind: &str, subject_id: &str) -> Result<i32> {
    let row: (Option<i32>,) = sqlx::query_as(
        "select max(attempt) from retry_outcomes where subject_kind = $1 and subject_id = $2",
    )
    .bind(subject_kind)
    .bind(subject_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0.unwrap_or(0))
}

/// Re-open a dead letter: the `retry now` action.
///
/// It writes a **new attempt row** rather than clearing the flag on the old one, because the
/// timeline is the evidence an operator reads afterwards; a `retry now` that erased the failure
/// it was answering would leave a sequence with no failure in it and one extra success.
pub async fn retry_now(pool: &PgPool, record_id: i64) -> Result<Option<AttemptRecord>> {
    let existing = sqlx::query_as::<_, OutcomeRow>(
        "select id, subsystem, subject_kind, subject_id, attempt, scheduled_at, executed_at, \
                outcome, error_class, next_delay_ms, dead_letter, next_attempt_at, created_at \
           from retry_outcomes where id = $1",
    )
    .bind(record_id)
    .fetch_optional(pool)
    .await?;
    let Some(existing) = existing else {
        return Ok(None);
    };
    let next_attempt = existing.attempt + 1;
    let next = record_outcome(
        pool,
        &existing.subsystem,
        &existing.subject_kind,
        existing.subject_id.as_deref(),
        next_attempt,
        "succeeded",
        None,
        None,
        None,
        false,
    )
    .await?;
    Ok(Some(next))
}

/// Drop ledger rows older than `days`, so the store does not become a second request archive.
///
/// Dead letters are exempt: they are the evidence an operator keeps, and a retention policy that
/// quietly deleted them would delete the only record that a delivery ever failed.
pub async fn prune_outcomes(pool: &PgPool, days: i64) -> Result<u64> {
    let result = sqlx::query(
        "delete from retry_outcomes \
          where created_at < now() - make_interval(days => $1) and not dead_letter",
    )
    .bind(days as f64)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}
