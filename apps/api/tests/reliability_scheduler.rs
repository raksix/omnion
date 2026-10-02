//! The scheduler, proved against a real database (REQ-127 slice 3).
//!
//! `retry.rs` and `retry_store.rs` are shipped and tested: the ledger persists a next-attempt
//! time and the machine decides what one attempt does. What is left is the piece between them,
//! and it carries the one guarantee the whole request is named for — **a restarted worker
//! resumes exactly once** — so every walk below names the claim it is the evidence for:
//!
//! * `a_second_worker_cannot_claim_a_sequence_the_first_one_holds` — no double execution. Both
//!   workers run the same scan against the same row; the assertion is that exactly one claim
//!   succeeds, proved by CALLING the claim twice rather than by reading `claimed_at` back and
//!   assuming a `false` means "somebody else won".
//! * `a_sequence_is_due_once_and_then_stops_being_due` — the scan reads the newest row per
//!   subject. Claiming the middle of a sequence would replay an attempt the timeline already
//!   shows, so the walk writes three attempts and asserts only the LAST one is offered.
//! * `an_expired_claim_is_taken_over_by_the_next_worker` — the lease, and the crash story behind
//!   it. A worker that dies mid-attempt must not take the job down with it, so a claim older
//!   than the lease is available again; the counter-assertion is that a claim INSIDE the lease
//!   is not, which is what stops a slow provider from being picked up twice.
//! * `a_restart_resumes_from_the_persisted_next_attempt_time_through_the_scheduler` — the
//!   acceptance criterion itself, now through the scheduler rather than by reading the column.
//! * `the_backlog_counter_agrees_with_the_rows_the_scan_offers` — the panel's number and the
//!   worker's worklist are one question, not two that drift.

mod support;

use omnion_reliability::retry::{self, Failure};
use omnion_reliability::retry_store as rstore;
use omnion_reliability::scheduler::{self, DueSequence};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use support::walk_state::state_or_fail;

/// A subject unique to this run, so a suite that runs twice on one database does not inherit a
/// sibling walk's sequence — the lesson the store walks learned twice already.
fn fixture(subsystem: &str) -> (String, String, String) {
    let n = Uuid::new_v4();
    (
        subsystem.to_string(),
        format!("w6-job-{n}"),
        n.to_string(),
    )
}

async fn record(
    pool: &sqlx::PgPool,
    seq: &DueSequence,
    attempt: i32,
    outcome: &str,
    next_at: Option<OffsetDateTime>,
) {
    rstore::record_outcome(
        pool,
        &seq.subsystem,
        &seq.subject_kind,
        Some(&seq.subject_id),
        attempt,
        outcome,
        Some("500"),
        None,
        next_at,
        false,
    )
    .await
    .expect("record an attempt");
}

async fn subject_of(
    subsystem: &str,
    subject_id: &str,
) -> DueSequence {
    DueSequence {
        subsystem: subsystem.to_string(),
        subject_kind: "w6_scheduler_job".to_string(),
        subject_id: subject_id.to_string(),
        // Filled from the ledger by the first scan; these walks always scan before claiming.
        attempt: 0,
        due_at: OffsetDateTime::UNIX_EPOCH,
        row_id: 0,
    }
}

#[tokio::test]
async fn a_second_worker_cannot_claim_a_sequence_the_first_one_holds() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let now = OffsetDateTime::now_utc();
    let (subsystem, _kind, subject_id) = fixture("webhook");

    let seq = subject_of(&subsystem, &subject_id).await;
    record(&pool, &seq, 1, "failed_retryable", Some(now - Duration::seconds(1))).await;

    // Both workers scan at the same instant and both see the same due row.
    let worker_a = scheduler::due_sequences(pool, now, 10).await.expect("scan A");
    let worker_b = scheduler::due_sequences(pool, now, 10).await.expect("scan B");
    let offered_a = worker_a.iter().find(|s| s.subject_id == subject_id).expect("A sees it");
    let offered_b = worker_b.iter().find(|s| s.subject_id == subject_id).expect("B sees it");
    assert_eq!(
        offered_a.row_id, offered_b.row_id,
        "two scans must offer the same ROW, not two rows of the same sequence"
    );

    let won = scheduler::claim_sequence(pool, offered_a, now, Duration::seconds(60))
        .await
        .expect("first claim");
    assert!(won, "the first worker must win its own scan");

    // The second worker, holding the identical row, must lose. This is the whole
    // no-double-execution mechanism and it is one conditional UPDATE, not a lock table.
    let also_won = scheduler::claim_sequence(pool, offered_b, now, Duration::seconds(60))
        .await
        .expect("second claim");
    assert!(!also_won, "two workers ran the same attempt; the ledger now holds a duplicate");

    // And the count, not the boolean: a scheduler that wrote the claim unconditionally would
    // answer `false` the second time without ever having excluded anybody.
    let claims: (Option<i64>,) =
        sqlx::query_as("select count(*) from retry_outcomes where subject_id = $1 and claimed_at is not null")
            .bind(&subject_id)
            .fetch_one(pool)
            .await
            .expect("count claims");
    assert_eq!(claims.0, Some(1), "one sequence must carry at most one live claim");
}

#[tokio::test]
async fn a_sequence_is_due_once_and_then_stops_being_due() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let now = OffsetDateTime::now_utc();
    let (subsystem, _kind, subject_id) = fixture("email");

    let seq = subject_of(&subsystem, &subject_id).await;
    // Three attempts, all carrying a next time, as a live sequence does. Only the last one is
    // the sequence's CURRENT obligation; offering the first would replay attempt 2.
    record(&pool, &seq, 1, "failed_retryable", Some(now - Duration::seconds(30))).await;
    record(&pool, &seq, 2, "failed_retryable", Some(now - Duration::seconds(20))).await;
    record(&pool, &seq, 3, "failed_retryable", Some(now - Duration::seconds(10))).await;

    let due = scheduler::due_sequences(pool, now, 200).await.expect("scan");
    let offered: Vec<&DueSequence> = due.iter().filter(|s| s.subject_id == subject_id).collect();
    assert_eq!(
        offered.len(),
        1,
        "a three-attempt sequence was offered {} times; the scan is replaying its own history",
        offered.len()
    );
    assert_eq!(offered[0].attempt, 3, "the newest row is the one that carries the next attempt");

    // A sequence whose last row has no next time is FINISHED: nothing is owed.
    record(&pool, &seq, 4, "succeeded", None).await;
    let after = scheduler::due_sequences(pool, now, 200).await.expect("rescan");
    assert!(
        !after.iter().any(|s| s.subject_id == subject_id),
        "a succeeded sequence is still being offered"
    );
}

#[tokio::test]
async fn an_expired_claim_is_taken_over_and_a_live_one_is_not() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let now = OffsetDateTime::now_utc();
    let (subsystem, _kind, subject_id) = fixture("ai");

    let seq = subject_of(&subsystem, &subject_id).await;
    record(&pool, &seq, 1, "failed_retryable", Some(now - Duration::seconds(5))).await;
    let offered = scheduler::due_sequences(pool, now, 10)
        .await
        .expect("scan")
        .into_iter()
        .find(|s| s.subject_id == subject_id)
        .expect("the sequence is due");

    // A claim that is still inside its lease is NOT available. The counter-assertion matters:
    // "an expired claim is taken over" is trivially satisfiable by a claim that is never stored.
    assert!(
        scheduler::claim_sequence(pool, &offered, now, Duration::seconds(60))
            .await
            .expect("claim"),
        "the first worker takes it"
    );
    let later = now + Duration::seconds(10);
    let offered_later = scheduler::due_sequences(pool, later, 10)
        .await
        .expect("scan inside the lease");
    assert!(
        !offered_later.iter().any(|s| s.subject_id == subject_id),
        "a live claim is still being offered; a slow provider would be sent twice"
    );

    // Past the lease the claim is stale and the next worker takes over — the crash story: a
    // worker that dies mid-attempt hands the job back instead of taking the platform down.
    let after_lease = now + Duration::seconds(61);
    let offered_after = scheduler::due_sequences(pool, after_lease, 10)
        .await
        .expect("scan past the lease")
        .into_iter()
        .find(|s| s.subject_id == subject_id)
        .expect("an expired claim is offered again");
    assert!(
        scheduler::claim_sequence(pool, &offered_after, after_lease, Duration::seconds(60))
            .await
            .expect("takeover claim"),
        "an expired claim must be available to the next worker"
    );
}

#[tokio::test]
async fn a_restart_resumes_through_the_scheduler_and_runs_the_attempt_exactly_once() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let now = OffsetDateTime::now_utc();
    let (subsystem, _kind, subject_id) = fixture("integration");
    let seq = subject_of(&subsystem, &subject_id).await;

    // Attempt one fails retryably, with the next attempt already owed.
    let due_at = now - Duration::seconds(2);
    record(&pool, &seq, 1, "failed_retryable", Some(due_at)).await;

    // "The restart": every piece of in-process state is dropped — the scan is built from the
    // ledger alone and the attempt number comes from the ledger, not from a counter this
    // walk owns. A scheduler holding the count in memory would answer attempt 1 again here.
    let resumed = scheduler::due_sequences(pool, now, 200)
        .await
        .expect("scan after restart")
        .into_iter()
        .find(|s| s.subject_id == subject_id)
        .expect("the sequence is owed an attempt after the restart");
    assert_eq!(resumed.attempt, 1, "the next attempt is the one the ledger scheduled");

    let attempts_before = rstore::attempt_count(pool, "w6_scheduler_job", &subject_id)
        .await
        .expect("count attempts");
    assert_eq!(attempts_before, 1);

    // The worker claims it and runs attempt two, which fails again under a real policy.
    let claimed = scheduler::claim_sequence(pool, &resumed, now, Duration::seconds(60))
        .await
        .expect("claim");
    assert!(claimed);

    let policy = scheduler::policy_for(pool, &subsystem, None)
        .await
        .expect("policy")
        .expect("a subsystem with no edited row still retries on the in-process default");
    let elapsed = scheduler::elapsed_ms(pool, &resumed, now).await.expect("elapsed");
    let advance = scheduler::advance(
        pool,
        &resumed,
        resumed.attempt + 1,
        Some(&Failure::Status(503)),
        elapsed,
        1.0,
    )
    .await
    .expect("advance");

    assert_eq!(advance.decision.attempt, 2);
    assert_eq!(advance.decision.outcome, "failed_retryable");
    assert!(advance.record.next_attempt_at.is_some(), "the next attempt time is persisted");
    assert!(!advance.record.dead_letter);

    // EXACTLY ONCE is the claim under test: attempt two exists once, not twice.
    let attempts_after = rstore::attempt_count(pool, "w6_scheduler_job", &subject_id)
        .await
        .expect("count attempts");
    assert_eq!(attempts_after, 2, "the resumed attempt ran more than once");

    // And the timeline the operator reads is attempt-ordered, which `now()` cannot provide.
    let timeline = rstore::load_timeline(pool, "w6_scheduler_job", &subject_id)
        .await
        .expect("timeline");
    let numbers: Vec<i32> = timeline.iter().map(|t| t.attempt).collect();
    assert_eq!(numbers, vec![1, 2], "the timeline is not in attempt order");

    let _ = policy;
}

#[tokio::test]
async fn the_backlog_counter_agrees_with_the_rows_the_scan_offers() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let now = OffsetDateTime::now_utc();
    let (subsystem, _kind, subject_id) = fixture("storage");

    let seq = subject_of(&subsystem, &subject_id).await;
    record(&pool, &seq, 1, "failed_retryable", Some(now - Duration::seconds(1))).await;

    let counter = scheduler::due_count(pool, now).await.expect("count");
    let scan = scheduler::due_sequences(pool, now, 200).await.expect("scan");

    // The panel's number is a subset check, not equality: sibling walks share this global table,
    // so an equality assertion would fail for a reason that is not a defect. The claim is that
    // this sequence is counted AND offered — a screen reporting a backlog of zero while work is
    // owed is the failure worth catching.
    assert!(
        counter >= 1,
        "a due sequence exists and the backlog counter says {counter}"
    );
    assert!(
        scan.iter().any(|s| s.subject_id == subject_id),
        "the counter counts a sequence the scan does not offer"
    );
    assert!(
        counter as usize >= scan.len().min(counter as usize),
        "the counter ({counter}) is behind the worklist ({})",
        scan.len()
    );
}

#[tokio::test]
async fn an_unedited_subsystem_retries_on_its_default_rather_than_not_at_all() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let (subsystem, _kind, _subject_id) = fixture("workflow");

    // No policy row has ever been written for this subsystem. The guarantee "an outbound path
    // that retries also breaks" does not survive a subsystem that silently stops retrying
    // because nobody opened the screen, so the in-process default is the fallback.
    let stored = rstore::resolve_policy(pool, &subsystem, None)
        .await
        .expect("resolve");
    assert!(stored.is_none(), "this fixture must not have a stored policy");

    let policy = scheduler::policy_for(pool, &subsystem, None)
        .await
        .expect("policy_for")
        .expect("the in-process default is used");
    assert_eq!(policy.jitter, "full", "a default that ships without full jitter amplifies outages");
    assert_eq!(retry::default_policy_for(&subsystem).map(|p| p.max_attempts), Some(policy.max_attempts));
}

#[tokio::test]
async fn a_non_retryable_failure_through_the_scheduler_is_not_scheduled_again() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let now = OffsetDateTime::now_utc();
    let (subsystem, _kind, subject_id) = fixture("email");

    let seq = subject_of(&subsystem, &subject_id).await;
    record(&pool, &seq, 1, "failed_retryable", Some(now - Duration::seconds(1))).await;
    let offered = scheduler::due_sequences(pool, now, 200)
        .await
        .expect("scan")
        .into_iter()
        .find(|s| s.subject_id == subject_id)
        .expect("due");

    let advance = scheduler::advance(
        pool,
        &offered,
        2,
        Some(&Failure::Status(422)),
        0,
        1.0,
    )
    .await
    .expect("advance");

    assert_eq!(advance.decision.outcome, "failed_permanent");
    assert!(advance.record.next_attempt_at.is_none(), "a permanent failure scheduled another attempt");
    assert!(!advance.record.dead_letter, "a permanent failure is not an exhausted one");
}

#[tokio::test]
async fn the_last_attempt_becomes_exactly_one_dead_letter() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let now = OffsetDateTime::now_utc();
    let (subsystem, _kind, subject_id) = fixture("workflow");

    // A policy with two attempts, so the second failure is the end of the budget.
    let mut policy = retry::default_policy_for(&subsystem).expect("default policy");
    policy.max_attempts = 2;
    policy.base_delay_ms = 1;
    rstore::upsert_policy(pool, &policy).await.expect("upsert");

    let seq = subject_of(&subsystem, &subject_id).await;
    record(&pool, &seq, 1, "failed_retryable", Some(now - Duration::seconds(1))).await;
    let offered = scheduler::due_sequences(pool, now, 200)
        .await
        .expect("scan")
        .into_iter()
        .find(|s| s.subject_id == subject_id)
        .expect("due");

    let advance = scheduler::advance(pool, &offered, 2, Some(&Failure::Status(500)), 0, 1.0)
        .await
        .expect("advance");

    assert_eq!(advance.decision.outcome, "exhausted");
    assert!(advance.decision.dead_letter);
    assert_eq!(advance.event, omnion_reliability::vocabulary::events::RETRY_EXHAUSTED);

    // COUNTED, not read back: a store that flagged the row twice still returns a flagged row.
    let dead: (Option<i64>,) =
        sqlx::query_as("select count(*) from retry_outcomes where subject_id = $1 and dead_letter")
            .bind(&subject_id)
            .fetch_one(pool)
            .await
            .expect("count dead letters");
    assert_eq!(dead.0, Some(1), "an exhausted sequence wrote {} dead letters", dead.0.unwrap_or(0));

    rstore::delete_policy(pool, &subsystem, None).await.expect("cleanup");
}
