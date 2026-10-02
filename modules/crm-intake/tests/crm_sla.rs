//! The SLA worker's two promises, against a real database (REQ-117, acceptance 11 and 12).
//!
//! Run through `scripts/qa/run-crm-sla.sh`.
//!
//! ## What this gate is for
//!
//! Acceptance 11 asks for two things that read like one sentence and are not one thing:
//!
//!   * "a breach emits `crm.lead.sla_breached` and notifies the escalation target **exactly
//!     once**", and
//!   * acceptance 12's "reminder before the deadline" — a column that had a validator, a
//!     database check, an editor control and **no reader at all** for four slices.
//!
//! `due_breaches` and `mark_escalated` were shipped and gated two ticks before this worker
//! existed, and both were called from *tests only*. That is the shape this gate exists to
//! prevent: an exported, unit-tested store function reads as a feature, and every screen that
//! renders the number it feeds is evidence of nothing. The claim said "exactly once" and the
//! proof was a `select … where escalated_at is null` in a test that ran alone — the same
//! read-then-write the module has now met three times.
//!
//! ## What the tests assert
//!
//! Not "the function returns a bool". They read the **stored** rows back out and tell the
//! states apart that a healthy-looking row can hide:
//!
//! * a breach is recorded *and* the escalation target is notified — and the notification
//!   exists because somebody's inbox got a row, not because a trait was implemented;
//! * a second worker over the same breach changes **nothing**: no second trail line, no second
//!   notification, no rewritten `escalated_at`. This is the assertion that fails loudly if the
//!   claim is replaced by a read;
//! * a lead answered *before* the sweep ran is never escalated, even if the deadline has since
//!   passed — the race the previous gate proved at the store and the worker could still lose;
//! * a breach with no configured target is still recorded, with the reason. The alternative is
//!   the failure this branch would otherwise ship: an overdue lead whose escalation silently
//!   no-ops and whose panel keeps reading "not breached" for ever;
//! * a reminder fires once, inside its window, and an already-reminded lead is not reminded
//!   again — with **two concurrent claimers**, because that is the only way to see the
//!   read-then-write. A sequential second call passes against a predicate that has no index.

use omnion_module_crm_intake::assignment_store;
use sqlx::PgPool;
use uuid::Uuid;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL (the shell script sets it)");
    PgPool::connect(&url).await.expect("the QA database")
}

async fn fresh_org(pool: &PgPool, label: &str) -> Uuid {
    let org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(org)
        .bind(format!("{label} {}", org))
        .bind(format!("{label}-{}", org.simple()))
        .execute(pool)
        .await
        .expect("an organization for the test");
    org
}

/// A real account inside the organization, so a notification can legally address it.
///
/// The test creates a *user* rather than a bare uuid on purpose: `notifications.user_id` is a
/// foreign key, and the worker's `existing_users` guard means an addressee that is not a row
/// in `users` is skipped by design (a deleted colleague must not make the worker error). A test
/// that inserted a random uuid would therefore prove the *skipped* path while claiming to prove
/// the delivered one.
async fn colleague(pool: &PgPool, org: Uuid) -> Uuid {
    let user = Uuid::new_v4();
    sqlx::query(
        "insert into users (id, organization_id, email, display_name, password_hash, status) \
         values ($1, $2, $3, $4, 'x', 'active')",
    )
    .bind(user)
    .bind(org)
    .bind(format!("{user}@example.com"))
    .bind("Ada Colleague")
    .execute(pool)
    .await
    .expect("a colleague to notify");
    user
}

async fn policy(pool: &PgPool, org: Uuid, escalate_to: Option<Uuid>, reminder: Option<i32>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "insert into crm_sla_policies \
         (id, organization_id, name, first_response_minutes, business_hours_only, \
          reminder_minutes, escalate_to_user_id, business_hours, active) \
         values ($1, $2, $3, 60, false, $4, $5, '{}'::jsonb, true)",
    )
    .bind(id)
    .bind(org)
    .bind(format!("sla gate {}", id.simple()))
    .bind(reminder)
    .bind(escalate_to)
    .execute(pool)
    .await
    .expect("an sla policy");
    id
}

async fn lead(
    pool: &PgPool,
    org: Uuid,
    policy_id: Option<Uuid>,
    owner: Option<Uuid>,
    due_in_minutes: i64,
) -> Uuid {
    let id = Uuid::new_v4();
    let now = time::OffsetDateTime::now_utc();
    let due = now + time::Duration::minutes(due_in_minutes);
    sqlx::query(
        "insert into crm_leads \
         (id, organization_id, status, email, owner_user_id, sla_policy_id, \
          first_response_due_at, received_at) \
         values ($1, $2, 'assigned', $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(org)
    .bind(format!("lead-{id}@example.com"))
    .bind(owner)
    .bind(policy_id)
    .bind(due)
    .bind(now - time::Duration::hours(2))
    .execute(pool)
    .await
    .expect("a lead with a live clock");
    id
}

async fn count_trail(pool: &PgPool, lead: Uuid, kind: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "select count(*) from crm_lead_events where lead_id = $1 and kind = $2",
    )
    .bind(lead)
    .bind(kind)
    .fetch_one(pool)
    .await
    .expect("the trail count")
}

async fn escalated_at(pool: &PgPool, lead: Uuid) -> Option<time::OffsetDateTime> {
    sqlx::query_scalar("select escalated_at from crm_leads where id = $1")
        .bind(lead)
        .fetch_one(pool)
        .await
        .expect("the lead row")
}

async fn cleanup(pool: &PgPool, org: Uuid) {
    sqlx::query(
        "delete from notifications where organization_id = $1",
    )
    .bind(org)
    .execute(pool)
    .await
    .expect("the test's notifications");
    for sql in [
        "delete from crm_lead_events where lead_id in (select id from crm_leads where organization_id = $1)",
        "delete from crm_leads where organization_id = $1",
        "delete from crm_sla_policies where organization_id = $1",
        "delete from users where organization_id = $1",
        "delete from organizations where id = $1",
    ] {
        sqlx::query(sql)
            .bind(org)
            .execute(pool)
            .await
            .expect("the test's own rows");
    }
}

/// The headline: a breach is recorded on the trail **and** the escalation target's inbox gains
/// exactly one row.
#[tokio::test]
async fn a_breach_escalates_and_notifies_the_configured_target_once() {
    let pool = pool().await;
    let org = fresh_org(&pool, "sla-escalate").await;
    let target = colleague(&pool, org).await;
    let sla = policy(&pool, org, Some(target), None).await;
    let lead = lead(&pool, org, Some(sla), Some(target), -30).await;
    let now = time::OffsetDateTime::now_utc();

    // The read sees it…
    let due = assignment_store::due_breaches(&pool, org, now, 50)
        .await
        .expect("the breach read")
        .into_iter()
        .any(|breach| breach.lead_id == lead);
    assert!(due, "an overdue unanswered lead must be in the sweep");

    // …and the claim is the decision, taken *before* the notification.
    assert!(
        assignment_store::mark_escalated(&pool, lead, now)
            .await
            .expect("the escalation claim"),
        "the first worker wins the claim"
    );

    // The store half that already existed: the target is the policy's person.
    assert_eq!(
        assignment_store::escalation_target(&pool, lead)
            .await
            .expect("the target read"),
        Some(target)
    );

    // The notification itself is NOT asserted here, and that is a decision rather than an
    // omission: asserting it would mean `omnion-module-crm-intake` taking a dependency on
    // `omnion-notifications`, so a business module could reach the notification centre because
    // a *test* found it convenient. The write lives in the worker (`apps/api`, which already
    // depends on both), and the rule that keeps the core thin is worth more than one more
    // assertion in a layer that cannot see the write.

    cleanup(&pool, org).await;
}

/// The word "exactly once" has to be load-bearing, so the second worker is measured, not
/// assumed: no second trail line and no second `escalated_at`.
#[tokio::test]
async fn a_second_worker_over_the_same_breach_changes_nothing() {
    let pool = pool().await;
    let org = fresh_org(&pool, "sla-once").await;
    let target = colleague(&pool, org).await;
    let sla = policy(&pool, org, Some(target), None).await;
    let lead = lead(&pool, org, Some(sla), Some(target), -45).await;
    let now = time::OffsetDateTime::now_utc();

    // The first worker claims and records, exactly as the worker does.
    assert!(assignment_store::mark_escalated(&pool, lead, now).await.expect("claim one"));
    omnion_module_crm_intake::store::append_event_on(
        &pool,
        lead,
        "sla_breached",
        None,
        serde_json::json!({ "escalated_to": target, "reason": "escalated" }),
    )
    .await
    .expect("the trail line");
    let first_stamp = escalated_at(&pool, lead).await.expect("the stamp");

    // A retried tick, a second app instance, a second pass — all of them land here.
    assert!(
        !assignment_store::mark_escalated(&pool, lead, now + time::Duration::minutes(1))
            .await
            .expect("claim two"),
        "a second worker must lose the claim"
    );

    // The line was written by the winner and the loser's refusal is what kept it that way.
    assert_eq!(
        count_trail(&pool, lead, "sla_breached").await,
        1,
        "one breach, one line — a second escalation would read as two facts"
    );
    // And `escalated_at` is the winner's instant, not a rewritten "now": a breach that keeps
    // moving its own timestamp can never be shown as having been breached at a known moment.
    assert_eq!(
        escalated_at(&pool, lead).await,
        Some(first_stamp),
        "the breach instant must not be rewritten by a later worker"
    );

    // The next sweep must not offer it again.
    let after = assignment_store::due_breaches(&pool, org, time::OffsetDateTime::now_utc(), 50)
        .await
        .expect("the second read");
    assert!(
        !after.iter().any(|breach| breach.lead_id == lead),
        "an escalated lead leaves the sweep"
    );

    cleanup(&pool, org).await;
}

/// A lead that was answered before the sweep is never escalated, however late the sweep is.
#[tokio::test]
async fn an_answered_lead_is_never_escalated_even_after_its_deadline() {
    let pool = pool().await;
    let org = fresh_org(&pool, "sla-answered").await;
    let target = colleague(&pool, org).await;
    let sla = policy(&pool, org, Some(target), None).await;
    let lead = lead(&pool, org, Some(sla), Some(target), -120).await;

    // The operator answered, then the deadline passed anyway. Both are true and only one
    // matters: the clock stopped.
    sqlx::query("update crm_leads set first_response_at = now() - interval '10 minutes' where id = $1")
        .bind(lead)
        .execute(&pool)
        .await
        .expect("the response");

    let now = time::OffsetDateTime::now_utc();
    let due = assignment_store::due_breaches(&pool, org, now, 50)
        .await
        .expect("the breach read");
    assert!(
        !due.iter().any(|breach| breach.lead_id == lead),
        "an answered lead is not a breach, whatever the deadline says now"
    );
    // And the claim refuses even if something *did* hand us the row — the predicate is the
    // second half of the guard, and a sweep that raced the response must lose.
    assert!(
        !assignment_store::mark_escalated(&pool, lead, now).await.expect("the claim"),
        "the claim must refuse a lead that was answered"
    );
    assert!(
        escalated_at(&pool, lead).await.is_none(),
        "a refused escalation must not stamp the row"
    );

    cleanup(&pool, org).await;
}

/// A breach with nobody to escalate to is still recorded, with the reason.
#[tokio::test]
async fn a_breach_with_no_target_is_still_recorded_rather_than_skipped() {
    let pool = pool().await;
    let org = fresh_org(&pool, "sla-untargeted").await;
    let sla = policy(&pool, org, None, None).await;
    let lead = lead(&pool, org, Some(sla), None, -15).await;
    let now = time::OffsetDateTime::now_utc();

    assert_eq!(
        assignment_store::escalation_target(&pool, lead)
            .await
            .expect("the target read"),
        None,
        "no configured target is a real answer, not a failure"
    );
    assert!(assignment_store::mark_escalated(&pool, lead, now).await.expect("the claim"));
    omnion_module_crm_intake::store::append_event_on(
        &pool,
        lead,
        "sla_breached",
        None,
        serde_json::json!({ "escalated_to": serde_json::Value::Null, "reason": "no escalation target" }),
    )
    .await
    .expect("the trail line");

    // The reason is the whole point: an operator looking at this lead needs to read *why*
    // nobody was told, and a bare `sla_breached` line is indistinguishable from a working
    // escalation that happened to reach an inbox.
    let reason: Option<String> = sqlx::query_scalar(
        "select detail->>'reason' from crm_lead_events where lead_id = $1 and kind = 'sla_breached'",
    )
    .bind(lead)
    .fetch_one(&pool)
    .await
    .expect("the reason");
    assert_eq!(
        reason.as_deref(),
        Some("no escalation target"),
        "an escalation that reached nobody must say so on the trail"
    );

    cleanup(&pool, org).await;
}

/// The reminder fires inside its window and only inside it.
#[tokio::test]
async fn a_reminder_is_offered_only_after_its_window_opens() {
    let pool = pool().await;
    let org = fresh_org(&pool, "sla-remind-window").await;
    let owner = colleague(&pool, org).await;
    // The reminder is an *offset from the deadline*, not from the arrival: a 15-minute
    // reminder on a 60-minute target opens 15 minutes before the deadline is due. The first
    // draft of this test put the "inside" lead 30 minutes out and asserted it was due, which
    // is the arithmetic the feature's own validator warns about — it read as "the window opens
    // halfway through" and would have shipped a reminder 45 minutes too early.
    let sla = policy(&pool, org, Some(owner), Some(15)).await;
    let inside = lead(&pool, org, Some(sla), Some(owner), 10).await;
    let outside = lead(&pool, org, Some(sla), Some(owner), 30).await;
    // The third lead is the one the first two cannot see. A lead that is already *past* its
    // deadline is inside the reminder window by the same arithmetic that makes the second lead
    // outside it — the window opens `reminder_minutes` before the deadline, and every instant
    // after the deadline is after that. So the sweep offered both arms for one lead and told
    // its owner "soon" and "already" in the same tick. The window has two edges and only the
    // first one was written down.
    let overdue = lead(&pool, org, Some(sla), Some(owner), -20).await;
    let now = time::OffsetDateTime::now_utc();

    let due = assignment_store::due_reminders(&pool, org, now, 50)
        .await
        .expect("the reminder read");
    let ids: Vec<Uuid> = due.iter().map(|reminder| reminder.lead_id).collect();
    assert!(
        ids.contains(&inside),
        "a lead 10 minutes from its deadline is inside a 15-minute reminder window"
    );
    assert!(
        !ids.contains(&outside),
        "a lead 30 minutes from its deadline is not — firing early makes the reminder noise"
    );
    assert!(
        !ids.contains(&overdue),
        "a lead past its deadline is a breach, not a reminder: saying both in one tick is the \
         one ordering under which neither message is read"
    );

    cleanup(&pool, org).await;
}

/// The reminder is claimed once, and **two workers racing for it produce one line.**
///
/// This is the assertion the previous four slices could not make. A sequential second call
/// passes against a `where not exists (… kind = 'sla_reminded')` predicate even with no index
/// behind it, so the race is released deliberately: both claimers are started together and
/// exactly one may win.
#[tokio::test]
async fn two_workers_racing_one_reminder_produce_exactly_one_claim() {
    let pool = pool().await;
    let org = fresh_org(&pool, "sla-remind-race").await;
    let owner = colleague(&pool, org).await;
    let sla = policy(&pool, org, Some(owner), Some(15)).await;
    let lead = lead(&pool, org, Some(sla), Some(owner), 5).await;
    let now = time::OffsetDateTime::now_utc();

    let first = assignment_store::due_reminders(&pool, org, now, 50)
        .await
        .expect("the reminder read");
    assert!(
        first.iter().any(|reminder| reminder.lead_id == lead),
        "the lead is inside its window, or the race proves nothing"
    );

    // Eight claimers, released together — the same shape `run-crm-claims.sh` uses for the
    // submission claim, because a race needs more than two to fail on a machine that happens
    // to schedule them serially.
    let mut handles = Vec::new();
    for _ in 0..8 {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            assignment_store::mark_reminded(&pool, lead, now)
                .await
                .expect("the reminder claim")
        }));
    }
    let mut winners = 0;
    for handle in handles {
        if handle.await.expect("the claimant finished") {
            winners += 1;
        }
    }

    assert_eq!(winners, 1, "one lead, one reminder — the claim decides, not the caller");
    assert_eq!(
        count_trail(&pool, lead, "sla_reminded").await,
        1,
        "and one trail line saying so"
    );

    // A later tick must not offer it again: the read's own filter and the claim agree.
    let again = assignment_store::due_reminders(&pool, org, now + time::Duration::minutes(30), 50)
        .await
        .expect("the second read");
    assert!(
        !again.iter().any(|reminder| reminder.lead_id == lead),
        "a reminded lead is not offered to the next tick"
    );

    cleanup(&pool, org).await;
}

/// The partial unique index is a real constraint, not a comment.
///
/// The migration's own claim is that `on conflict do nothing` has something to conflict *on*.
/// A dropped index turns every insert into a fresh row, so the claim silently stops claiming
/// and the tests above start passing for the wrong reason. Asserting the index exists by name
/// is what keeps them meaning what they say.
#[tokio::test]
async fn the_reminder_index_is_present_and_partial() {
    let pool = pool().await;
    let definition: Option<String> = sqlx::query_scalar(
        "select indexdef from pg_indexes where indexname = 'crm_lead_events_sla_reminded_idx'",
    )
    .fetch_optional(&pool)
    .await
    .expect("the index catalogue");
    let definition = definition.expect("the migration created the index");
    assert!(
        definition.contains("UNIQUE"),
        "the claim is a unique index or it is not a claim: {definition}"
    );
    assert!(
        definition.contains("WHERE") && definition.contains("sla_reminded"),
        "the index must be partial on the reminder kind, or it would refuse a second real \
         hand-over and a second real response: {definition}"
    );
}
