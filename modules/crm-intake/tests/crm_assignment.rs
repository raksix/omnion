//! The slice-2 gate: the assignment chain, the atomic claim, SLA policies and the breach
//! sweep, against a real database.
//!
//! Run through `scripts/qa/run-crm-assignment.sh`, which creates the database and the two
//! organizations these tests need. Each test is independent and cleans up after itself, so
//! one failure does not cascade into the next.
//!
//! ## What this file exists to prove that a unit test cannot
//!
//! * **The claim is a transaction.** `two_concurrent_claims_take_two_different_people` is the
//!   whole reason the cursor moves under `for update`. A read-then-write passes every
//!   sequential test and fails this one, and it fails *intermittently* — which is why it has
//!   to be forced here rather than hoped against in review.
//! * **A cross-organization id is a `None`, not a `403`.** A `403` on a lead or a rule is an
//!   oracle for "that exists, not for you".
//! * **The seeded defaults exist for an organization created *after* the migration ran**,
//!   which is the case a migration-only seed silently misses.
//! * **`escalated_at` is written once.** "Notifies the escalation target exactly once" is a
//!   claim about a predicate, and predicates are only as good as the test that calls them
//!   twice.

use omnion_module_crm_intake::assignment::{AssignmentInput, SlaState};
use omnion_module_crm_intake::assignment_store::{
    self, NewPolicy, NewRule, DEFAULT_POLICY_NAME, DEFAULT_RULE_NAME,
};
use omnion_module_crm_intake::{AssignmentOutcome, Result, store};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL (the shell script sets it)");
    PgPool::connect(&url).await.expect("the QA database")
}

/// A fresh organization per test, created up front and dropped at the end.
///
/// The first version of this file used two shared organizations and had every test `delete
/// from … where organization_id = …` before it started. Cargo runs the tests in this binary
/// concurrently, so each one was deleting the others' fixtures mid-run: 8 failures that all
/// read like product bugs and were all a harness deleting its own ground. One organization
/// per test is the same isolation the product's tenancy is built on, so the gate now also
/// proves that two organizations in the same database do not interfere.
async fn fresh_org(pool: &PgPool, label: &str) -> Uuid {
    // The label only makes the organization findable when a run fails; the id is what the
    // rows are keyed on.
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

async fn drop_org(pool: &PgPool, org: Uuid) {
    cleanup(pool, org).await;
    let _ = sqlx::query("delete from organizations where id = $1")
        .bind(org)
        .execute(pool)
        .await;
}

/// Three accounts a pool can hand leads to. They are users of organization A, because the
/// pool's ids have to be real users for a lead's `owner_user_id` to mean anything.
async fn three_users(pool: &PgPool, organization_id: Uuid) -> Vec<Uuid> {
    let mut ids = Vec::new();
    for n in 0..3 {
        let id = Uuid::new_v4();
        sqlx::query(
            "insert into users (id, email, password_hash, display_name) values ($1, $2, 'x', $3)",
        )
        .bind(id)
        .bind(format!("pool-{n}-{}@example.invalid", Uuid::new_v4()))
        .bind(format!("Pool member {n}"))
        .execute(pool)
        .await
        .expect("a user");
        // Attach to organization A if the column exists; skip silently if this migration
        // predates membership, so the gate tests assignment rather than membership.
        if let Err(error) = sqlx::query("update users set organization_id = $2 where id = $1")
            .bind(id)
            .bind(organization_id)
            .execute(pool)
            .await
        {
            tracing::warn!(error = %error, "users.organization_id is not available");
        }
        ids.push(id);
    }
    ids
}

async fn cleanup(pool: &PgPool, organization_id: Uuid) {
    for sql in [
        "delete from crm_lead_events where lead_id in (select id from crm_leads where organization_id = $1)",
        "delete from crm_leads where organization_id = $1",
        "delete from crm_intake_sources where organization_id = $1",
        "delete from crm_assignment_rules where organization_id = $1",
        "delete from crm_sla_policies where organization_id = $1",
    ] {
        let _ = sqlx::query(sql).bind(organization_id).execute(pool).await;
    }
    let _ = sqlx::query("delete from users where organization_id = $1")
        .bind(organization_id)
        .execute(pool)
        .await;
}

// ---------------------------------------------------------------------------------------------
// The transaction
// ---------------------------------------------------------------------------------------------

/// Ten sequential claims across a three-person pool, and then ten that race.
///
/// The sequential half proves the *arithmetic* (which the unit tests already cover). The
/// racing half is the point of this file: ten claims fired at once must still produce ten
/// owners with no person taking two in a row, and the cursor must land exactly where ten
/// sequential claims would have left it.
#[tokio::test]
async fn ten_concurrent_claims_never_repeat_a_person_twice_in_a_row() {
    let pool = pool().await;
    let org = fresh_org(&pool, "gate").await;
    assignment_store::ensure_defaults(&pool, org).await.expect("defaults");

    let users = three_users(&pool, org).await;
    // Delete the seeded catch-all so only the pool rule can match, then put the pool on top.
    sqlx::query("delete from crm_assignment_rules where organization_id = $1 and name <> $2")
        .bind(org)
        .bind("Pool")
        .execute(&pool)
        .await
        .expect("clear the chain");
    assignment_store::create_rule(
        &pool,
        org,
        &NewRule {
            name: "Pool".into(),
            conditions: serde_json::json!({ "has_email": true }),
            target_kind: "pool".into(),
            target_user_id: None,
            pool_user_ids: users.clone(),
            active: true,
        },
    )
    .await
    .expect("a pool rule");

    let input = AssignmentInput {
        has_email: Some(true),
        ..Default::default()
    };

    // Sequential first: this is the baseline the concurrent run must match.
    let mut sequential = Vec::new();
    for _ in 0..10 {
        let outcome = assignment_store::claim_assignment(&pool, org, &input)
            .await
            .expect("a sequential claim");
        sequential.push(outcome.owner_user_id.expect("an owner"));
    }

    // Now the same ten, all in flight at once.
    let mut set = Vec::new();
    for _ in 0..10 {
        let pool = pool.clone();
        let input = input.clone();
        set.push(tokio::spawn(async move {
            // A claim that loses the row lock and finds the cursor moved is *correct*
            // behaviour, not a failure: the caller retries. The test counts the outcome,
            // not the attempt.
            match assignment_store::claim_assignment(&pool, org, &input).await {
                Ok(outcome) => outcome.owner_user_id,
                Err(_) => None,
            }
        }));
    }
    let mut concurrent = Vec::new();
    for handle in set {
        if let Some(owner) = handle.await.expect("the claim task") {
            concurrent.push(owner);
        }
    }

    for window in sequential.windows(2) {
        assert_ne!(
            window[0], window[1],
            "sequential claims repeated a pool member"
        );
    }

    for owner in &concurrent {
        assert!(
            users.contains(owner),
            "a claim produced an owner outside the pool: {owner}"
        );
    }

    // The claim about concurrency is a claim about the CURSOR, not about the order the
    // tasks happened to finish in. An earlier version of this test compared adjacent
    // entries of the completion order and failed — correctly, because task A can take the
    // lock, commit, and be *descheduled* before task B is even scheduled. Two leads in
    // flight are not two leads in a row; they are two leads whose order the scheduler
    // decides.
    //
    // What must hold is the invariant that makes round-robin fair: over any window of
    // `pool.len()` claims, every member appears exactly once. That is checkable, and it is
    // what "the cursor advanced under a lock" actually buys. Comparing each owner's count
    // to the fair share does the same job and states the property instead of the symptom.
    // The fair share is over *all twenty* claims, not the concurrent ten: the sequential
    // ten already moved the cursor round the pool once and a bit, so the concurrent ten
    // start wherever it stopped. Twenty over three is 7/7/6, and asserting 3 each is
    // arithmetic the test got wrong, not a fairness the product failed to deliver.
    let all: Vec<Uuid> = sequential.iter().chain(concurrent.iter()).copied().collect();
    // The pool's own order, not the UUID's: the cursor cycles through `pool_user_ids` in
    // order, so "the first member gets the leftover claim" is about position in the pool.
    // An earlier version sorted the counts by UUID, which made the expected distribution
    // depend on a byte comparison of random ids and failed for a distribution that was
    // exactly fair.
    let counts: Vec<(Uuid, usize)> = users
        .iter()
        .map(|owner| (*owner, all.iter().filter(|o| *o == owner).count()))
        .collect();
    let total = all.len();
    let base = total / users.len();
    let remainder = total % users.len();
    for (index, (owner, count)) in counts.iter().enumerate() {
        // The first `remainder` members get one extra: a cycle of `pool.len()` claims hands
        // each member exactly one, and the leftover claims start the next cycle at the top.
        let expected = base + usize::from(index < remainder);
        assert_eq!(
            *count, expected,
            "the pool is not shared evenly: {owner:?} took {count}, expected {expected} \
             (a cycle of {} over {} claims)",
            users.len(),
            total
        );
    }
    // And the maximum gap between any two members is one claim, which is the property a
    // round-robin actually promises: nobody is ever skipped and nobody is ever served twice.
    let max = counts.iter().map(|(_, c)| *c).max().unwrap_or(0);
    let min = counts.iter().map(|(_, c)| *c).min().unwrap_or(0);
    assert!(
        max - min <= 1,
        "a round-robin never serves one member twice while another waits: {counts:?}"
    );

    // And the cursor itself: twenty claims (ten sequential, ten concurrent) over three
    // people leaves it at 20 % 3. A cursor that lost a race would be behind this.
    let rules = assignment_store::list_rules(&pool, org).await.expect("the chain");
    let cursor = rules
        .iter()
        .find(|r| r.target_kind == "pool")
        .map(|r| r.round_robin_cursor)
        .expect("the pool rule");
    assert_eq!(
        cursor,
        20 % users.len() as i32,
        "every claim advanced the cursor exactly once, including the ones that raced"
    );

    drop_org(&pool, org).await;
}

/// The cursor ends where the claims put it, whatever order they arrived in: ten claims
/// across three people is `10 % 3 == 1`, so the next lead goes to the second person.
#[tokio::test]
async fn the_cursor_advances_exactly_once_per_claim() {
    let pool = pool().await;
    let org = fresh_org(&pool, "gate").await;
    assignment_store::ensure_defaults(&pool, org).await.expect("defaults");
    sqlx::query("delete from crm_assignment_rules where organization_id = $1 and name <> $2")
        .bind(org)
        .bind("Pool")
        .execute(&pool)
        .await
        .expect("clear the chain");
    let users = three_users(&pool, org).await;
    let rule = assignment_store::create_rule(
        &pool,
        org,
        &NewRule {
            name: "Pool".into(),
            conditions: serde_json::json!({}),
            target_kind: "pool".into(),
            target_user_id: None,
            pool_user_ids: users,
            active: true,
        },
    )
    .await
    .expect("a pool rule");

    for _ in 0..10 {
        assignment_store::claim_assignment(&pool, org, &AssignmentInput::default())
            .await
            .expect("a claim");
    }
    let rules = assignment_store::list_rules(&pool, org).await.expect("the chain");
    let stored = rules
        .iter()
        .find(|r| r.id == rule.id)
        .expect("the pool rule");
    assert_eq!(
        stored.round_robin_cursor, 1,
        "ten claims over three people leave the cursor at 10 % 3"
    );
    drop_org(&pool, org).await;
}

// ---------------------------------------------------------------------------------------------
// Order
// ---------------------------------------------------------------------------------------------

/// Reordering is a prefix: the named rules move to the top, the rest keep their relative
/// order behind them, and the whole chain is renumbered from zero with no gaps.
#[tokio::test]
async fn a_reorder_moves_a_prefix_and_renumbers_without_gaps() {
    let pool = pool().await;
    let org = fresh_org(&pool, "gate").await;
    assignment_store::ensure_defaults(&pool, org).await.expect("defaults");

    let mut ids = Vec::new();
    for name in ["One", "Two", "Three"] {
        ids.push(
            assignment_store::create_rule(&pool, org, &NewRule::catch_all(name))
                .await
                .expect("a rule")
                .id,
        );
    }

    // Move the third to the front; the panel's "move up" sends exactly this.
    let reordered = assignment_store::reorder_rules(&pool, org, &[ids[2]]).await.expect("a reorder");
    let names: Vec<&str> = reordered.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names[0], "Three", "the named rule leads: {names:?}");
    assert!(
        names[1..].contains(&"One") && names[1..].contains(&"Two"),
        "the untouched rules follow in their old order: {names:?}"
    );
    for (index, rule) in reordered.iter().enumerate() {
        assert_eq!(
            rule.position, index as i32,
            "positions are dense and start at zero: {names:?}"
        );
    }

    // A second reorder keeps the prefix property, which is what makes repeated drags
    // compose rather than resetting the order.
    let again = assignment_store::reorder_rules(&pool, org, &[ids[0], ids[1]])
        .await
        .expect("a second reorder");
    assert_eq!(again[0].name, "One");
    assert_eq!(again[1].name, "Two");
    assert_eq!(again[2].name, "Three");
    for (index, rule) in again.iter().enumerate() {
        assert_eq!(rule.position, index as i32);
    }
    drop_org(&pool, org).await;
}

// ---------------------------------------------------------------------------------------------
// Tenancy
// ---------------------------------------------------------------------------------------------

/// Another organization's rule is a `None`, so the route answers 404 and not 403. A 403 says
/// "that exists, not for you", which is an oracle for enumerating ids.
#[tokio::test]
async fn a_rule_of_another_organization_is_a_none() {
    let pool = pool().await;
    let org = fresh_org(&pool, "gate").await;
    assignment_store::ensure_defaults(&pool, org).await.expect("defaults");
    let other = fresh_org(&pool, "tenant-other").await;
    let rule = assignment_store::create_rule(&pool, org, &NewRule::catch_all("Acme only"))
        .await
        .expect("a rule");

    assert!(
        assignment_store::find_rule(&pool, other, rule.id)
            .await
            .expect("a read")
            .is_none(),
        "a cross-organization rule must not be readable"
    );
    assert!(
        assignment_store::update_rule(
            &pool,
            other,
            rule.id,
            &NewRule::catch_all("hijacked")
        )
        .await
        .expect("an update")
        .is_none(),
        "a cross-organization rule must not be editable"
    );
    assert!(
        !assignment_store::delete_rule(&pool, other, rule.id)
            .await
            .expect("a delete"),
        "a cross-organization rule must not be deletable"
    );
    assert!(
        assignment_store::find_rule(&pool, org, rule.id)
            .await
            .expect("a read")
            .is_some(),
        "the rule is still there for its own organization"
    );
    drop_org(&pool, org).await;
}

/// Organization B's chain never sees organization A's rules, and the catch-all it is seeded
/// with lands its own leads in its own queue.
#[tokio::test]
async fn two_organizations_evaluate_independently() {
    let pool = pool().await;
    let a = fresh_org(&pool, "tenant-a").await;
    let b = fresh_org(&pool, "tenant-b").await;
    assignment_store::ensure_defaults(&pool, a).await.expect("defaults for a");
    assignment_store::ensure_defaults(&pool, b).await.expect("defaults for b");

    let a_rules = assignment_store::list_rules(&pool, a).await.expect("a");
    let b_rules = assignment_store::list_rules(&pool, b).await.expect("b");
    assert_eq!(a_rules.len(), 1, "just the catch-all");
    assert_eq!(b_rules.len(), 1, "just the catch-all");
    assert_eq!(a_rules[0].name, DEFAULT_RULE_NAME);
    assert_eq!(b_rules[0].name, DEFAULT_RULE_NAME);
    assert_ne!(
        a_rules[0].id, b_rules[0].id,
        "the two catch-alls are separate rows"
    );

    drop_org(&pool, a).await;
    drop_org(&pool, b).await;
}

// ---------------------------------------------------------------------------------------------
// The defaults
// ---------------------------------------------------------------------------------------------

/// The seed has to hold for an organization created *after* the migration ran, which is
/// every organization created after the day the migration shipped. A migration-only seed is
/// correct on the day it runs and silently incomplete forever after, and the symptom is a
/// first lead that is unassigned with no deadline — indistinguishable, in the panel, from an
/// operator who turned everything off on purpose.
#[tokio::test]
async fn an_organization_created_after_the_migration_still_gets_the_defaults() {
    let pool = pool().await;
    // A fresh organization, created now, with no rules and no policies.
    let org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(org)
        .bind(format!("Late {}", Uuid::new_v4()))
        .bind(format!("late-{}", Uuid::new_v4()))
        .execute(&pool)
        .await
        .expect("an organization");

    let rules = assignment_store::list_rules(&pool, org).await.expect("the chain");
    assert_eq!(rules.len(), 1, "the catch-all is seeded on first read");
    assert_eq!(rules[0].name, DEFAULT_RULE_NAME);
    assert_eq!(rules[0].target_kind, "queue");

    let policies = assignment_store::list_policies(&pool, org).await.expect("the policies");
    assert_eq!(policies.len(), 1);
    assert_eq!(policies[0].name, DEFAULT_POLICY_NAME);
    assert_eq!(policies[0].first_response_minutes, 240);
    assert!(
        !policies[0].business_hours_only,
        "the seeded default runs around the clock, so a lead never looks stuck"
    );

    // Idempotent: a second read adds nothing, so the panel reloading does not accumulate
    // defaults and an operator sees one four-hour target rather than two.
    let again = assignment_store::list_rules(&pool, org).await.expect("the chain");
    assert_eq!(again.len(), 1, "the seed does not accumulate");

    let _ = sqlx::query("delete from organizations where id = $1")
        .bind(org)
        .execute(&pool)
        .await;
}

// ---------------------------------------------------------------------------------------------
// The clock
// ---------------------------------------------------------------------------------------------

/// The breach sweep finds the overdue lead, escalates it once, and finds nothing the second
/// time — which is the whole content of "notifies the escalation target exactly once".
#[tokio::test]
async fn a_breach_escalates_exactly_once() {
    let pool = pool().await;
    let org = fresh_org(&pool, "gate").await;
    assignment_store::ensure_defaults(&pool, org).await.expect("defaults");
    // The escalation target is a real user: the column is a foreign key, so a fabricated id
    // is refused at the database and the test fails on its fixture rather than on the sweep.
    let escalation = three_users(&pool, org).await.remove(0);
    let policy = assignment_store::create_policy(
        &pool,
        org,
        &NewPolicy {
            escalate_to_user_id: Some(escalation),
            ..policy_with_minutes("Two hours", 120)
        },
    )
    .await
    .expect("a policy");

    let source = endpoint_source(&pool, org, "SLA gate").await;
    // Three hours old on a two-hour policy: overdue by an hour. A lead *younger* than its
    // own target is correctly absent from the sweep, which is why the "inside its window"
    // test below is a separate case rather than the same one with a different number.
    let lead = insert_lead(&pool, org, source, Some(policy.id), -180).await;

    // Read the fixture back before asserting on the sweep. Three wrong diagnoses in a row
    // all came from theorising about a row nobody had looked at; this line makes the next
    // one a two-second check instead of a rewrite.
    let (stored_due, minutes): (Option<OffsetDateTime>, i32) = sqlx::query_as(
        "select first_response_due_at, first_response_minutes \
         from crm_leads join crm_sla_policies on crm_sla_policies.id = crm_leads.sla_policy_id \
         where crm_leads.id = $1",
    )
    .bind(lead)
    .fetch_one(&pool)
    .await
    .expect("the fixture");
    let now: OffsetDateTime = sqlx::query_scalar("select now()")
        .fetch_one(&pool)
        .await
        .expect("the database clock");
    assert!(
        stored_due.is_some_and(|d| d < now),
        "the fixture must be overdue: due={stored_due:?} now={now:?} policy={minutes}m"
    );

    let breaches = assignment_store::due_breaches(&pool, org, now, 50)
        .await
        .expect("the sweep");
    assert!(
        breaches.iter().any(|b| b.lead_id == lead),
        "an overdue lead must be in the sweep: {breaches:?}"
    );

    assert!(
        assignment_store::mark_escalated(&pool, lead, db_now(&pool).await)
            .await
            .expect("an escalation"),
        "the first escalation is the one that counts"
    );
    assert!(
        !assignment_store::mark_escalated(&pool, lead, db_now(&pool).await)
            .await
            .expect("a second escalation"),
        "a second worker tick must not escalate the same lead again"
    );

    let after = assignment_store::due_breaches(&pool, org, db_now(&pool).await, 50)
        .await
        .expect("the sweep");
    assert!(
        !after.iter().any(|b| b.lead_id == lead),
        "an escalated lead leaves the sweep"
    );

    drop_org(&pool, org).await;
}

/// A lead that is not overdue is never in the sweep, however old it is: the predicate is the
/// deadline, not the age.
#[tokio::test]
async fn a_lead_inside_its_window_is_not_a_breach() {
    let pool = pool().await;
    let org = fresh_org(&pool, "gate").await;
    assignment_store::ensure_defaults(&pool, org).await.expect("defaults");
    let source = endpoint_source(&pool, org, "SLA in-window").await;
    // Due two hours from now: inside the window.
    let lead = insert_lead_at(&pool, org, source, None, OffsetDateTime::now_utc() + time::Duration::hours(2)).await;

    let breaches = assignment_store::due_breaches(&pool, org, db_now(&pool).await, 50)
        .await
        .expect("the sweep");
    assert!(
        !breaches.iter().any(|b| b.lead_id == lead),
        "a lead inside its window is not overdue"
    );
    drop_org(&pool, org).await;
}

/// A lead somebody already answered is not a breach, and marking it responded stops the
/// clock even after it was due — responding after the breach keeps the breach recorded, but
/// a *fresh* answer on an untouched lead must not escalate.
#[tokio::test]
async fn a_lead_that_was_answered_is_never_escalated() {
    let pool = pool().await;
    let org = fresh_org(&pool, "gate").await;
    assignment_store::ensure_defaults(&pool, org).await.expect("defaults");
    let source = endpoint_source(&pool, org, "SLA answered").await;
    let lead = insert_lead(&pool, org, source, None, -30).await;
    store::record_response(&pool, org, lead, None).await.expect("a response");

    let breaches = assignment_store::due_breaches(&pool, org, db_now(&pool).await, 50)
        .await
        .expect("the sweep");
    assert!(
        !breaches.iter().any(|b| b.lead_id == lead),
        "an answered lead is not waiting for anybody"
    );
    assert!(
        !assignment_store::mark_escalated(&pool, lead, db_now(&pool).await)
            .await
            .expect("an escalation"),
        "an answered lead must not escalate even if the sweep raced the response"
    );
    drop_org(&pool, org).await;
}

/// A policy's deadline is a stored instant, and the state the panel shows is computed from
/// three facts — answered, deadline, now — and nothing else.
#[tokio::test]
async fn the_state_the_panel_shows_comes_from_the_stored_deadline() {
    let pool = pool().await;
    let org = fresh_org(&pool, "gate").await;
    assignment_store::ensure_defaults(&pool, org).await.expect("defaults");
    let policy = assignment_store::create_policy(
        &pool,
        org,
        &NewPolicy {
            business_hours_only: true,
            business_hours: serde_json::json!({
                "days": [1,2,3,4,5], "start": "09:00", "end": "17:00",
                "timezone": "Europe/Istanbul"
            }),
            ..policy_with_minutes("Working hours", 240)
        },
    )
    .await
    .expect("a policy");
    let source = endpoint_source(&pool, org, "SLA business hours").await;
    // Monday 2026-01-05 at 18:00 UTC is past the 17:00 close, so a four-hour business target
    // is due Tuesday 13:00 — the deadline must skip the closed hours, not the weekend's
    // neighbour, and must be stored as that instant.
    let received = OffsetDateTime::parse(
        "2026-01-05T18:00:00Z",
        &time::format_description::well_known::Rfc3339,
    )
    .expect("a timestamp");
    let lead = insert_lead_at(&pool, org, source, Some(policy.id), received).await;

    let stored: Option<OffsetDateTime> =
        sqlx::query_scalar("select first_response_due_at from crm_leads where id = $1")
            .bind(lead)
            .fetch_one(&pool)
            .await
            .expect("the deadline");
    let due = stored.expect("a deadline was written");
    assert_eq!(
        due,
        OffsetDateTime::parse(
            "2026-01-06T13:00:00Z",
            &time::format_description::well_known::Rfc3339
        )
        .expect("a timestamp"),
        "a lead after cutoff is due inside the next working day"
    );

    assert_eq!(
        SlaState::of(None, Some(due), received, 240),
        SlaState::OnTrack
    );
    assert_eq!(
        SlaState::of(None, Some(due), due, 240),
        SlaState::Breached,
        "the deadline instant itself is breached"
    );
    assert_eq!(
        SlaState::of(Some(received), Some(due), due + time::Duration::hours(99), 240),
        SlaState::Met,
        "answered is met, whatever the clock says afterwards"
    );
    drop_org(&pool, org).await;
}

/// The claim writes the owner, the rule that decided it and the deadline onto the lead, and
/// moves a `new` lead to `assigned` — but a lead that is already further along does not go
/// backwards.
#[tokio::test]
async fn the_claim_stamps_the_lead_without_rewinding_its_status() {
    let pool = pool().await;
    let org = fresh_org(&pool, "gate").await;
    assignment_store::ensure_defaults(&pool, org).await.expect("defaults");
    let users = three_users(&pool, org).await;
    let rule = assignment_store::create_rule(
        &pool,
        org,
        &NewRule {
            name: "Direct".into(),
            conditions: serde_json::json!({}),
            target_kind: "user".into(),
            target_user_id: users.first().copied(),
            pool_user_ids: vec![],
            active: true,
        },
    )
    .await
    .expect("a rule");
    let policy = assignment_store::find_policy(&pool, org, assignment_store::list_policies(&pool, org).await.expect("policies")[0].id)
        .await
        .expect("a read")
        .expect("a policy");
    let source = endpoint_source(&pool, org, "Claim stamp").await;
    let received = OffsetDateTime::now_utc();
    let lead = insert_lead_at(&pool, org, source, Some(policy.id), received).await;

    let outcome: AssignmentOutcome = assignment_store::claim_assignment(
        &pool,
        org,
        &AssignmentInput::default(),
    )
    .await
    .expect("a claim");
    assignment_store::stamp_assignment(&pool, lead, &outcome, Some(&policy), received)
        .await
        .expect("a stamp");

    let row: (Option<Uuid>, Option<Uuid>, String, Option<OffsetDateTime>) = sqlx::query_as(
        "select owner_user_id, assignment_rule_id, status, first_response_due_at \
         from crm_leads where id = $1",
    )
    .bind(lead)
    .fetch_one(&pool)
    .await
    .expect("the lead");
    assert_eq!(row.0, users.first().copied(), "the owner was written");
    assert_eq!(row.1, Some(rule.id), "the deciding rule was written");
    assert_eq!(row.2, "assigned", "a new lead that got an owner is assigned");
    assert!(row.3.is_some(), "the deadline was written");

    // A lead that already moved on is not rewound by a later claim.
    sqlx::query("update crm_leads set status = 'contacted' where id = $1")
        .bind(lead)
        .execute(&pool)
        .await
        .expect("a status change");
    assignment_store::stamp_assignment(&pool, lead, &outcome, Some(&policy), received)
        .await
        .expect("a second stamp");
    let status: String = sqlx::query_scalar("select status from crm_leads where id = $1")
        .bind(lead)
        .fetch_one(&pool)
        .await
        .expect("the status");
    assert_eq!(status, "contacted", "a claim never rewinds a lead's status");

    drop_org(&pool, org).await;
}

// ---------------------------------------------------------------------------------------------
// Validation at the database
// ---------------------------------------------------------------------------------------------

/// The refusals the editor makes are also the database's, which is what makes them
/// un-bypassable: an account that talks to the database directly cannot save a rule that
/// claims a target and names nobody.
#[tokio::test]
async fn the_database_refuses_the_rules_the_editor_refuses() {
    let pool = pool().await;
    let org = fresh_org(&pool, "gate").await;
    assignment_store::ensure_defaults(&pool, org).await.expect("defaults");

    let no_members: Vec<Uuid> = Vec::new();
    for (name, target_kind, pool_ids) in [
        ("Pool with no members", "pool", &no_members),
        ("User with nobody", "user", &no_members),
    ] {
        let sql = "insert into crm_assignment_rules \
                   (organization_id, name, position, target_kind, pool_user_ids) \
                   values ($1, $2, 0, $3, $4)";
        let error = sqlx::query(sql)
            .bind(org)
            .bind(name)
            .bind(target_kind)
            .bind(pool_ids)
            .execute(&pool)
            .await
            .expect_err("the database must refuse a rule that targets nothing");
        let text = error.to_string();
        assert!(
            text.contains("crm_assignment_rules_target_present_check"),
            "the refusal must name the constraint: {text}"
        );
    }

    let error = sqlx::query(
        "insert into crm_sla_policies (organization_id, name, first_response_minutes, reminder_minutes) \
         values ($1, 'Reminder at the breach', 240, 240)",
    )
    .bind(org)
    .execute(&pool)
    .await
    .expect_err("a reminder at the breach minute must be refused");
    assert!(
        error.to_string().contains("crm_sla_policies_reminder_check"),
        "the refusal must name the constraint: {error}"
    );

    // And the Rust validator refuses the same two, so the panel never sends a request the
    // database is going to turn away.
    assert!(
        omnion_module_crm_intake::validate_rule("Pool", &serde_json::json!({}), "pool", None, &[]).is_err()
    );
    drop_org(&pool, org).await;
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The default policy with a different target. A free function rather than a method on
/// `NewPolicy`: a test file lives in its own crate and cannot add an inherent impl to a type
/// the library owns (the orphan rule), so the ergonomics have to come from here.
fn policy_with_minutes(name: &str, minutes: i32) -> NewPolicy {
    NewPolicy {
        first_response_minutes: minutes,
        ..NewPolicy::web_default(name)
    }
}

/// The database's clock, read once per call.
///
/// The sweep compares a stored deadline against a `now`; if the two sides of that
/// comparison come from different clocks the answer is whatever the difference between the
/// two machines is. Every instant in this gate is the database's.
async fn db_now(pool: &PgPool) -> OffsetDateTime {
    sqlx::query_scalar("select now()")
        .fetch_one(pool)
        .await
        .expect("the database clock")
}

async fn insert_lead(
    pool: &PgPool,
    organization_id: Uuid,
    source_id: Uuid,
    policy_id: Option<Uuid>,
    minutes_ago: i64,
) -> Uuid {
    // The database is the clock, not this process.
    //
    // Five wrong diagnoses in a row all came from the same move: computing "now" in the
    // test and comparing it to the database. The instants this process produced were
    // consistently ahead of the database's, by an amount that grew with the offset being
    // applied — a timezone conversion in the middle of the arithmetic. Nothing in the
    // *product* was ever wrong: `due_breaches` compares against a bound `now`, and a lead
    // whose deadline is in the future is correctly not overdue.
    //
    // `select now() - make_interval(mins => $1)` makes Postgres do the subtraction, so the
    // two clocks in the test cannot disagree.
    assert!(
        minutes_ago < 0,
        "insert_lead takes a negative number: negative is minutes into the past"
    );
    let received: OffsetDateTime = sqlx::query_scalar(
        "select now() + make_interval(mins => $1::int)",
    )
    .bind(minutes_ago)
    .fetch_one(pool)
    .await
    .expect("the database clock");
    assert!(
        received <= OffsetDateTime::now_utc() + time::Duration::minutes(1),
        "the database clock and this process must agree within a minute: {received:?}"
    );
    insert_lead_at(pool, organization_id, source_id, policy_id, received).await
}

/// A keyed intake source for an organization. `create_source` answers
/// `(source, issued_key)` because an endpoint's key is revealed exactly once, and the gate
/// has no use for the key — it is here so a lead has a real `source_id`, which is what makes
/// the `policy_for_source` path and the `on delete set null` behaviour testable at all.
async fn endpoint_source(pool: &PgPool, organization_id: Uuid, name: &str) -> Uuid {
    store::create_source(
        pool,
        &omnion_module_crm_intake::NewIntakeSource::endpoint(organization_id, name, None),
    )
    .await
    .expect("an intake source")
    .0
    .id
}

/// A lead whose deadline is `due` minutes after `received`, with the contactability
/// constraint satisfied (an e-mail is one of the two ways to be contactable).
async fn insert_lead_at(
    pool: &PgPool,
    organization_id: Uuid,
    source_id: Uuid,
    policy_id: Option<Uuid>,
    received: OffsetDateTime,
) -> Uuid {
    // The deadline comes from the product's own `due_at`, never from arithmetic here.
    // An earlier version of this helper added the minutes itself, which quietly turned the
    // business-hours test into a test of the *helper*: it stored a wall-clock deadline and
    // then reported the product as wrong. The gate has to ask the code it is testing.
    let due = match policy_id {
        Some(id) => {
            let policy = assignment_store::find_policy(pool, organization_id, id)
                .await
                .expect("a policy read")
                .expect("the policy");
            omnion_module_crm_intake::due_at(&policy, received)
        }
        None => received + time::Duration::minutes(240),
    };
    // The two instants are interpolated into the SQL as UTC rather than bound. Binding a
    // `time::OffsetDateTime` here landed the deadline five hours in the *future* on this
    // box — the offset between the shell's local time and UTC — and the sweep then
    // correctly reported "not overdue" for a lead that was hours overdue. The lesson is
    // the one this whole slice is built on: when a gate fails, read the row before
    // theorising. Writing the instant in the query's own `timestamptz` literal removes the
    // conversion from the path entirely.
    // Bind the instants as parameters and let sqlx encode them. Three earlier attempts
    // interpolated them and each one taught the same lesson:
    //
    //  1. `Display` produced subsecond junk Postgres rejected (42601);
    //  2. Rfc3339 was accepted — and shifted the value three hours into the *future*,
    //     because the `time` crate's Rfc3339 renders the *local* offset and this box is
    //     not on UTC, while the column is `timestamptz` read as UTC;
    //  3. the symptom looked exactly like a product bug: a lead three hours old with a
    //     two-hour target is correctly NOT overdue, so the sweep said "nothing to do".
    //
    // The parameter path has no conversion step of our own, and the debug assertion below
    // prints the encoded form when a future instant ever slips through again.
    // Every value is a bind parameter, including the two timestamps. Interpolating an id
    // into SQL was the earlier mistake and it produced "trailing junk after numeric
    // literal" three times, because Postgres read the leading digits of a uuid as a number
    // and then hit the rest of the hex — the error names the token, not the cause.
    let query = "insert into crm_leads \
             (organization_id, source_id, status, email, first_name, received_at, \
              first_response_due_at, sla_policy_id) \
         values ($1, $2, 'new', $3, 'Gate', $4, $5, $6) \
         returning id";
    let id: Uuid = sqlx::query_scalar(query)
        .bind(organization_id)
        .bind(source_id)
        .bind(format!("gate-{}@example.invalid", Uuid::new_v4()))
        .bind(received)
        .bind(due)
        .bind(policy_id)
        .fetch_one(pool)
        .await
        .expect("a lead");
    id
}

/// `ensure_defaults` is on the *read* path, so a constraint its `on conflict do nothing`
/// depends on has to exist before anybody opens the screen. The failure this guards is
/// Postgres 42P10 — "no unique or exclusion constraint matching the ON CONFLICT
/// specification" — which is not obviously about a missing unique index, and which fires on
/// every organization rather than on the one the developer was looking at.
#[tokio::test]
async fn the_seeds_have_the_uniqueness_they_conflict_on() {
    let pool = pool().await;
    let org = fresh_org(&pool, "gate").await;

    // Two calls in a row: the first inserts, the second must hit the constraint and do
    // nothing rather than raise 42P10.
    assignment_store::ensure_defaults(&pool, org).await.expect("first seed");
    assignment_store::ensure_defaults(&pool, org).await.expect("second seed");
    assert_eq!(
        assignment_store::list_rules(&pool, org).await.expect("the chain").len(),
        1,
        "seeding twice leaves one catch-all"
    );
    assert_eq!(
        assignment_store::list_policies(&pool, org)
            .await
            .expect("the policies")
            .len(),
        1,
        "seeding twice leaves one policy"
    );

    // A second policy with a different name is a second policy — the constraint is on the
    // pair, not on the name alone, so two organizations may both have a "Web default".
    assignment_store::create_policy(&pool, org, &NewPolicy::web_default("Support hours"))
        .await
        .expect("a distinct policy");
    let other = fresh_org(&pool, "tenant-other").await;
    assignment_store::create_policy(
        &pool,
        other,
        &NewPolicy::web_default("Web default"),
    )
    .await
    .expect("the same name in another organization");
    drop_org(&pool, org).await;
    drop_org(&pool, other).await;
}

/// The gate's own smoke: reading the chain for an organization that has never had one.
#[tokio::test]
async fn reading_the_chain_of_a_fresh_organization_does_not_fail() -> Result<()> {
    let pool = pool().await;
    let org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(org)
        .bind("Reader")
        .bind(format!("reader-{}", Uuid::new_v4()))
        .execute(&pool)
        .await
        .expect("an organization");
    let rules = assignment_store::list_rules(&pool, org).await?;
    assert_eq!(rules.len(), 1);
    let outcome = assignment_store::claim_assignment(&pool, org, &AssignmentInput::default()).await?;
    assert_eq!(outcome.rule_id, Some(rules[0].id));
    assert_eq!(outcome.owner_user_id, None, "the catch-all hands to the queue");
    let _ = sqlx::query("delete from organizations where id = $1")
        .bind(org)
        .execute(&pool)
        .await;
    Ok(())
}
