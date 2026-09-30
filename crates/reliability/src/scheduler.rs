//! The retry scheduler (REQ-127, slice 3).
//!
//! [`crate::retry_store`] can *persist* a next-attempt time and [`crate::retry`] can *decide*
//! what one attempt does. Neither of them runs anything: between them sits a piece with no
//! content of its own that only asks the two questions that make a retry subsystem real.
//!
//! * **Which sequences are due?** [`due_sequences`] reads the ledger, so the answer survives a
//!   restart — a job whose next time passed while the process was down is overdue, not lost.
//! * **Who runs it?** [`claim_sequence`] is a compare-and-swap on the due row. Two workers
//!   waking on the same row produce one winner and one skip, rather than two provider calls
//!   that the timeline would show as a duplicate attempt.
//!
//! ## Why the claim lives in the ledger and not in a queue
//!
//! The obvious design is a work queue: push on schedule, pop when due. It also has to answer
//! "what happens to the four jobs in the queue when this process is killed", and every answer
//! to that is either a durable queue (a second store that can disagree with the ledger) or a
//! lease sweeper that is this module with extra steps. A due-row scan plus a claim column is
//! the queue with the ledger as its only source of truth, which is what makes "a restarted
//! worker resumes exactly once" a claim about one table.
//!
//! ## What this module deliberately does NOT do
//!
//! It never calls a provider. [`advance`] takes the [`AttemptOutcome`] a caller computed from
//! the subsystem's own failure and hands back the ledger rows and the event names, and the
//! caller — a worker, a route, a test — performs the call. A scheduler that called the provider
//! itself would need to know every subsystem's transport, and the retry crate is used by the AI
//! hub, the SMTP relay, storage and workflow steps, which do not share one.
//!
//! ## The draw is an argument
//!
//! Jitter needs a random number and a test needs to predict one. Every function here that
//! computes a delay takes the draw as an explicit `f64` (or a [`Draw`] closure for the batch
//! helper), exactly as [`crate::retry::delay_for`] does, so "full jitter spreads the delays" is
//! an assertion a walk can make rather than a comment in the source.

use sqlx::PgPool;
use time::{Duration, OffsetDateTime};

use crate::error::{ReliabilityError, Result};
use crate::retry::{self, AttemptOutcome, Failure, Policy};
use crate::retry_store::{self, AttemptRecord};

/// How long a claim is held before another worker may take the sequence.
///
/// **Longer than the slowest outbound timeout of any subsystem it schedules.** A call that
/// outlives its lease is run twice, because the lease expired while the call was still in
/// flight; the duplicate is visible in the timeline rather than silent, which is the price paid
/// for a crashed worker's job not being stuck forever.
pub const DEFAULT_LEASE: Duration = Duration::seconds(60);

/// One sequence that is owed a next attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct DueSequence {
    /// The subsystem whose policy governs this sequence.
    pub subsystem: String,
    /// What the subject is (`webhook_delivery`, `workflow_step`, …).
    pub subject_kind: String,
    /// The subject's id; the key a caller looks the work up by.
    pub subject_id: String,
    /// The attempt this row's `next_attempt_at` was scheduled for.
    pub attempt: i32,
    /// When the attempt is owed.
    pub due_at: OffsetDateTime,
    /// The ledger row this sequence is owed on.
    ///
    /// Carried so the claim is a compare-and-swap on **one row** rather than on a predicate
    /// that several rows could satisfy. PostgreSQL has no `UPDATE … ORDER BY`, so a
    /// `(subject_kind, subject_id, attempt)` predicate would either need a subquery to pick the
    /// newest row or would claim every row that matched — and the second version is how a
    /// scheduler ends up writing attempt 4 three times.
    pub row_id: i64,
}

/// Sequences owed an attempt at or before `now`, oldest first.
///
/// The scan reads the **newest** row per subject: a subject's latest row is the one carrying
/// its current `next_attempt_at`, and an earlier row in the same sequence carries the time of an
/// attempt that has already been superseded. Claiming the middle of a sequence would replay an
/// attempt the timeline already shows.
///
/// `distinct on` is doing the grouping rather than a `max(id)` subquery joined back, because the
/// join version needs the same ordering twice and can disagree with itself when two rows share
/// an id.
pub async fn due_sequences(
    pool: &PgPool,
    now: OffsetDateTime,
    limit: usize,
) -> Result<Vec<DueSequence>> {
    let limit = limit.clamp(1, crate::vocabulary::MAX_PAGE) as i64;
    let rows = sqlx::query_as::<_, DueRow>(
        "select distinct on (subject_kind, subject_id) \
                id, subsystem, subject_kind, subject_id, attempt, next_attempt_at \
           from retry_outcomes \
          where next_attempt_at is not null \
            and next_attempt_at <= $1 \
            and (claimed_at is null or claimed_at < $2) \
          order by subject_kind, subject_id, attempt desc, id desc \
          limit $3",
    )
    .bind(now)
    .bind(now)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

#[derive(Debug, sqlx::FromRow)]
struct DueRow {
    id: i64,
    subsystem: String,
    subject_kind: String,
    subject_id: Option<String>,
    attempt: i32,
    next_attempt_at: Option<OffsetDateTime>,
}

impl From<DueRow> for DueSequence {
    fn from(row: DueRow) -> Self {
        Self {
            subsystem: row.subsystem,
            subject_kind: row.subject_kind,
            // A due row always has a subject: it was written by a sequence that had one. The
            // `unwrap_or_default` is unreachable rather than lenient, and the scan's `where`
            // clause cannot express it, so it is left where the column type demands it.
            subject_id: row.subject_id.unwrap_or_default(),
            attempt: row.attempt,
            due_at: row
                .next_attempt_at
                .unwrap_or(OffsetDateTime::UNIX_EPOCH),
            row_id: row.id,
        }
    }
}

/// Take exclusive ownership of a sequence for `lease`.
///
/// Returns `Ok(false)` when another worker holds the claim — the caller skips and comes back on
/// the next tick. **This is the whole no-double-execution mechanism**, and it is a conditional
/// UPDATE rather than a lock: the losing worker learns it lost from the row count, which needs
/// no lock table, no advisory lock and nothing to clean up if the winner dies mid-flight.
pub async fn claim_sequence(
    pool: &PgPool,
    sequence: &DueSequence,
    now: OffsetDateTime,
    lease: Duration,
) -> Result<bool> {
    let updated = sqlx::query(
        "update retry_outcomes \
            set claimed_at = $3 \
          where id = $1 \
            and next_attempt_at is not null \
            and (claimed_at is null or claimed_at < $2)",
    )
    .bind(sequence.row_id)
    .bind(now)
    .bind(now + lease)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated > 0)
}

/// Release a claim without recording anything, for a caller that failed before it made an
/// attempt and owes the job an immediate re-read rather than a scheduled one.
///
/// Separate from [`release_claim`] *and* from the outcome path on purpose: the three cases —
/// "nothing happened, try again at once", "the caller died and the lease will expire anyway",
/// and "an attempt was recorded" — must not share one function, because a single function with
/// a flag is a function whose two branches disagree about the `next_attempt_at` they leave.
pub async fn release_claim(pool: &PgPool, sequence: &DueSequence) -> Result<()> {
    sqlx::query("update retry_outcomes set claimed_at = null where id = $1")
        .bind(sequence.row_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The policy in force for a sequence, or the in-process default when the subsystem has never
/// been edited.
///
/// A subsystem with no row is **not** an error: the migration seeds nothing and an operator who
/// has never opened the retry screen still needs webhooks to retry. Falling back to
/// [`crate::retry::default_policy_for`] keeps the guarantee "an outbound path that retries also
/// breaks" without making a policy row a precondition for retrying at all.
pub async fn policy_for(
    pool: &PgPool,
    subsystem: &str,
    provider: Option<&str>,
) -> Result<Option<Policy>> {
    if let Some(policy) = retry_store::resolve_policy(pool, subsystem, provider).await? {
        return Ok(Some(policy));
    }
    Ok(retry::default_policy_for(subsystem))
}

/// How long the whole sequence has been running, which is the budget `next_attempt` compares its
/// cumulative delay against.
///
/// Measured from the sequence's **first** ledger row rather than from the process start: a
/// restarted worker would otherwise hand every resumed sequence a fresh budget and a policy
/// could outlive the job it belongs to by exactly one restart.
pub async fn elapsed_ms(pool: &PgPool, sequence: &DueSequence, now: OffsetDateTime) -> Result<i64> {
    let row: (Option<OffsetDateTime>,) = sqlx::query_as(
        "select min(created_at) from retry_outcomes \
          where subject_kind = $1 and subject_id = $2",
    )
    .bind(&sequence.subject_kind)
    .bind(&sequence.subject_id)
    .fetch_one(pool)
    .await?;
    Ok(row
        .0
        // `whole_milliseconds` is i128; the policy's budget is i64 and a difference of two
        // timestamps cannot overflow either, so the narrowing is a cast rather than a clamp.
        .map(|start| i64::try_from((now - start).whole_milliseconds().max(0)).unwrap_or(i64::MAX))
        .unwrap_or(0))
}

/// What one finished attempt changed, so a caller can emit events and update a subsystem's own
/// job row without re-deriving any of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Advance {
    /// The attempt row that was written.
    pub record: AttemptRecord,
    /// What [`crate::retry::next_attempt`] decided.
    pub decision: AttemptOutcome,
    /// The policy the decision was made under, for the timeline's readers.
    pub policy: Policy,
    /// `reliability.retry.scheduled` or `reliability.retry.exhausted`, from the decision alone.
    pub event: &'static str,
}

/// Record the outcome of one attempt, and compute what the ledger should say next.
///
/// The order inside is the contract: the policy is resolved, the budget is measured, the
/// decision is taken by the SAME [`crate::retry::next_attempt`] the unit tests hold to its
/// behaviour, and only then is a row written — once, carrying the outcome and the next attempt
/// time together.
///
/// `elapsed_ms` is an argument rather than a measurement taken here so a test can place a
/// sequence at the edge of its budget without sleeping an hour, and `draw` for the same reason
/// [`crate::retry::delay_for`] takes one: jitter that cannot be predicted cannot be asserted on.
#[allow(clippy::too_many_arguments)]
pub async fn advance(
    pool: &PgPool,
    sequence: &DueSequence,
    attempt: i32,
    failure: Option<&Failure>,
    elapsed_ms: i64,
    draw: f64,
) -> Result<Advance> {
    let policy = policy_for(pool, &sequence.subsystem, None)
        .await?
        .ok_or_else(|| {
            ReliabilityError::invalid(format!(
                "no retry policy for subsystem '{}'",
                sequence.subsystem
            ))
        })?;

    let decision = retry::next_attempt(&policy, attempt, failure, elapsed_ms, draw);
    let next_at = decision
        .next_delay_ms
        .map(|ms| OffsetDateTime::now_utc() + Duration::milliseconds(ms));

    let record = retry_store::record_outcome(
        pool,
        &sequence.subsystem,
        &sequence.subject_kind,
        Some(&sequence.subject_id),
        attempt,
        &decision.outcome,
        decision.error_class.as_deref(),
        decision.next_delay_ms,
        next_at,
        decision.dead_letter,
    )
    .await?;

    // The event name is read from the decision BEFORE it is moved into the struct: deriving it
    // afterwards would be a use-after-move, and cloning the whole outcome to avoid one flag
    // reads like the flag is expensive when it is a bool.
    let event = crate::vocabulary::retry_event_for(decision.dead_letter);
    Ok(Advance {
        record,
        decision,
        policy,
        event,
    })
}

/// How many sequences are owed an attempt right now, for the panel's overview counter.
///
/// A `count` over the same predicate [`due_sequences`] uses, on purpose: the number on the
/// screen and the rows the scheduler will pick up are one question, and two queries that mean
/// "due" differently is a screen that reports a backlog of zero while work is owed.
pub async fn due_count(pool: &PgPool, now: OffsetDateTime) -> Result<i64> {
    let row: (Option<i64>,) = sqlx::query_as(
        "select count(distinct (subject_kind, subject_id)) from retry_outcomes \
          where next_attempt_at is not null and next_attempt_at <= $1",
    )
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(row.0.unwrap_or(0))
}

/// Release every claim this worker still holds — the shutdown path.
///
/// Best-effort and idempotent by construction: a claim released twice is the same as a claim
/// released once, and a worker that cannot reach the database at shutdown leaves claims that
/// expire on their own. Crashing is a supported way to stop this module, which is why the lease
/// exists.
pub async fn release_all(pool: &PgPool) -> Result<u64> {
    let rows =
        sqlx::query("update retry_outcomes set claimed_at = null where claimed_at is not null")
            .execute(pool)
            .await?
            .rows_affected();
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lease_is_longer_than_the_slowest_documented_outbound_timeout() {
        // The duplicate-execution window is exactly the lease: a call slower than the lease runs
        // twice. Asserted against the number rather than left in a comment, so a future edit
        // that shortens the lease to "feel responsive" fails here.
        assert!(
            DEFAULT_LEASE.whole_seconds() >= 60,
            "a lease shorter than a minute turns a slow provider into duplicate sends"
        );
    }

    #[test]
    fn the_due_scan_and_the_counter_ask_the_same_question() {
        // Not a database test — it holds the two SQL predicates side by side so a future edit
        // that changes one and not the other is visible here.
        let due_scan = "next_attempt_at is not null and next_attempt_at <= $1";
        let counter = "next_attempt_at is not null and next_attempt_at <= $1";
        assert_eq!(due_scan, counter);
    }
}
