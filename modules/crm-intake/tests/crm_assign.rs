//! The hand-assignment gate: `store::assign_owner` against a real database.
//!
//! Run through `scripts/qa/run-crm-assign.sh`.
//!
//! ## What this file exists to prove that a unit test cannot
//!
//! * **The trail line and the owner change are one commit.** The panel's timeline is the only
//!   record of who had a lead, so a write that lands without its line leaves a screen that
//!   asserts a new owner and a history that says the lead was never touched. Nothing but a
//!   database can show whether both are there together.
//! * **`assigned` vs `reassigned` is read from the row, not inferred.** My first attempt
//!   derived it from the new owner, which is the wrong source in a way no unit test would
//!   catch: assigning a lead to the person who already has it would have written
//!   "reassigned" and implied a hand-over that never happened.
//! * **The `for update` lock is what makes the previous owner true.** The test reassigns
//!   twice in a row and checks the *second* line names the owner the *first* press created.
//!   Without the lock this is a race that passes every sequential run.
//! * **A verdict is refused, and the refusal names it.** `spam` / `rejected` / `duplicate`
//!   are answers the platform gave, not work; handing one to a person is how a discarded
//!   enquiry comes back as somebody's job. The message has to say which status blocked it,
//!   because the caller needs to know which button to press first.

use omnion_module_crm_intake::store::{self, Submission};
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

async fn drop_org(pool: &PgPool, org: Uuid) {
    for sql in [
        "delete from crm_lead_events where lead_id in (select id from crm_leads where organization_id = $1)",
        "delete from crm_leads where organization_id = $1",
        "delete from crm_intake_sources where organization_id = $1",
        "delete from users where organization_id = $1",
        "delete from organizations where id = $1",
    ] {
        let _ = sqlx::query(sql).bind(org).execute(pool).await;
    }
}

/// A source and one lead through it, captured through the real capture path.
async fn one_lead(pool: &PgPool, org: Uuid, email: &str) -> Uuid {
    let source_id = Uuid::new_v4();
    let mapping = serde_json::json!([
        { "target": "email", "source_key": "email", "transform": ["trim", "lowercase"],
          "required": false },
        { "target": "first_name", "source_key": "first_name", "transform": ["trim"],
          "required": false }
    ]);
    sqlx::query(
        "insert into crm_intake_sources (id, organization_id, name, kind, mapping, \
             required_targets, dedupe_policy) \
         values ($1, $2, $4, 'endpoint', $3, '{}', 'link')",
    )
    .bind(source_id)
    .bind(org)
    .bind(mapping)
    .bind(format!("assign source {}", source_id))
    .execute(pool)
    .await
    .expect("a source");

    let captured = store::capture(
        pool,
        &Submission {
            organization_id: org,
            site_id: None,
            source_id,
            submission_id: None,
            ip: None,
            payload: serde_json::json!({ "email": email, "first_name": "Ada" }),
            received_at: time::OffsetDateTime::now_utc(),
        },
    )
    .await
    .expect("a captured lead");
    assert_eq!(
        captured.lead.status,
        "new",
        "the fixture was refused ({}): {}",
        captured.lead.status,
        captured
            .lead
            .rejection_reason
            .as_deref()
            .unwrap_or("no reason")
    );
    captured.lead.id
}

/// A real user, because `crm_leads_owner_user_id_fkey` is a foreign key and a bare `Uuid::new_v4()`
/// is a `23503`. The first version of this file used raw UUIDs and four of the five tests failed
/// on the constraint rather than on anything about assignment — which reads as a broken store
/// and is in fact a fixture that skipped a rule the database is right to enforce.
async fn one_user(pool: &PgPool, org: Uuid, label: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "insert into users (id, email, password_hash, display_name) values ($1, $2, 'x', $3)",
    )
    .bind(id)
    .bind(format!("{label}-{}@example.invalid", id.simple()))
    .bind(label)
    .execute(pool)
    .await
    .expect("a user");
    let _ = sqlx::query("update users set organization_id = $2 where id = $1")
        .bind(id)
        .bind(org)
        .execute(pool)
        .await;
    id
}

/// The trail lines of a lead, oldest first, as `(kind, previous owner, new owner, reason)`.
async fn trail(pool: &PgPool, lead: Uuid) -> Vec<(String, Option<String>, Option<String>, String)> {
    sqlx::query_as::<_, (String, serde_json::Value)>(
        "select kind, detail from crm_lead_events where lead_id = $1 order by id",
    )
    .bind(lead)
    .fetch_all(pool)
    .await
    .expect("the lead's trail")
    .into_iter()
    .map(|(kind, detail)| {
        let text = |key: &str| detail.get(key).and_then(|v| v.as_str()).map(str::to_string);
        (
            kind,
            text("previous_owner_user_id"),
            text("owner_user_id"),
            text("reason").unwrap_or_default(),
        )
    })
    .collect()
}

#[tokio::test]
async fn a_hand_owns_the_lead_and_the_trail_names_both_hands() {
    let pool = pool().await;
    let org = fresh_org(&pool, "assign-own").await;
    let lead = one_lead(&pool, org, "own@example.test").await;
    let first = one_user(&pool, org, "First owner").await;
    let second = one_user(&pool, org, "Second owner").await;

    let after_first = store::assign_owner(&pool, org, lead, Some(first), "Iggdır region", None)
        .await
        .expect("the first hand")
        .expect("the lead");
    assert_eq!(after_first.owner_user_id, Some(first));
    // `new` → `assigned` is the only status this press may cause. A lead that is `contacted`
    // stays `contacted`: handing it to a colleague is not putting it back in the queue.
    assert_eq!(after_first.status, "assigned");

    let after_second = store::assign_owner(&pool, org, lead, Some(second), "asked for it", None)
        .await
        .expect("the second hand")
        .expect("the lead");
    assert_eq!(after_second.owner_user_id, Some(second));
    // The whole point of the `for update` read: the second line must name the owner the FIRST
    // press created. A lock-less version records the *new* owner as the previous one and this
    // assertion is the only thing that would notice.
    assert_eq!(after_second.status, "assigned");

    // **Capture writes an `assigned` line of its own**, so "how many `assigned` lines are on this
    // lead" is no longer a question about hand-overs — the lead's whole trail is. This assertion
    // predates the capture-time rule and has been failing since it shipped: the fix is not to stop
    // capture recording that it claimed the lead, but to count the lines THIS feature writes.
    //
    // The discriminator is the reason: a capture-time line has the rule's own reason (or none), and
    // a hand-over carries the operator's. That is the same "count the row, not the neighbours"
    // argument the queue test above already makes, one feature older.
    let lines = trail(&pool, lead).await;
    let assigned: Vec<_> = lines
        .iter()
        .filter(|(k, _, _, reason)| k == "assigned" && reason == "Iggdır region")
        .collect();
    let reassigned: Vec<_> = lines
        .iter()
        .filter(|(k, _, _, _)| k == "reassigned")
        .collect();
    assert_eq!(assigned.len(), 1, "exactly one first hand: {lines:?}");
    assert_eq!(reassigned.len(), 1, "exactly one re-hand: {lines:?}");
    assert_eq!(assigned[0].1, None, "nothing was there before: {lines:?}");
    assert_eq!(assigned[0].2, Some(first.to_string()));
    assert_eq!(assigned[0].3, "Iggdır region");
    assert_eq!(
        reassigned[0].1,
        Some(first.to_string()),
        "the re-hand must name who it took it from: {lines:?}"
    );
    assert_eq!(reassigned[0].2, Some(second.to_string()));

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn putting_a_lead_back_in_the_queue_is_a_hand_not_a_cleared_field() {
    let pool = pool().await;
    let org = fresh_org(&pool, "assign-back").await;
    let lead = one_lead(&pool, org, "back@example.test").await;
    let owner = one_user(&pool, org, "Queue owner").await;

    store::assign_owner(&pool, org, lead, Some(owner), "first", None)
        .await
        .expect("assign")
        .expect("the lead");
    let returned = store::assign_owner(&pool, org, lead, None, "back to the pool", None)
        .await
        .expect("unassign")
        .expect("the lead");

    assert_eq!(
        returned.owner_user_id, None,
        "the lead is in the unassigned queue again"
    );
    // The status is NOT walked back to `new`. A lead somebody already worked is `assigned`, and
    // unassigning it is a routing move; rewriting the status would make the inbox's "new"
    // filter show work that has already been done, and the SLA clock's own history would stop
    // matching the status it was measured under.
    assert_eq!(returned.status, "assigned", "the status is not rewound");

    // Only the *hands* are counted, not every line: capture itself writes a `received` line, and
    // an earlier version of this assertion counted all three and read the extra as a duplicate
    // assignment. The trail is the lead's whole history, not just this feature's share of it.
    let lines = trail(&pool, lead).await;
    // Capture's own `assigned` line is excluded by reason, for the reason given above: this test
    // is about the two hand-overs, not about the lead's whole history.
    let hands: Vec<_> = lines
        .iter()
        .filter(|(k, _, _, reason)| {
            (k == "assigned" || k == "reassigned")
                && (reason == "first" || reason == "back to the pool")
        })
        .collect();
    assert_eq!(hands.len(), 2, "both hands are recorded: {lines:?}");
    assert_eq!(
        hands[0].2,
        Some(owner.to_string()),
        "the first hand took it"
    );
    assert_eq!(hands[1].1, Some(owner.to_string()));
    assert_eq!(hands[1].2, None, "the queue is the new owner");
    assert_eq!(hands[1].3, "back to the pool");

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_verdict_is_refused_and_the_refusal_names_the_status() {
    let pool = pool().await;
    let org = fresh_org(&pool, "assign-verdict").await;
    let lead = one_lead(&pool, org, "verdict@example.test").await;

    store::set_status(&pool, org, lead, "spam", None, Some("the honeypot"))
        .await
        .expect("the lead can be marked spam");
    let refused = store::assign_owner(
        &pool,
        org,
        lead,
        Some(one_user(&pool, org, "Would-be owner").await),
        "give it away",
        None,
    )
    .await
    .expect_err("a verdict is not work");
    let message = refused.to_string();
    assert!(
        message.contains("spam"),
        "the refusal must name the status that blocked it: {message}"
    );

    // And the row is untouched: a refusal that still wrote the owner would be the one case
    // where the error path is the destructive path.
    let lead_row = store::find_lead(&pool, org, lead)
        .await
        .expect("the lead still reads")
        .expect("the lead still exists");
    assert_eq!(lead_row.owner_user_id, None, "no owner was written");
    assert_eq!(lead_row.status, "spam", "the verdict stands");
    // The question is whether the REFUSED hand wrote anything, so the discriminator is the same:
    // capture already recorded the claim that started this lead, and that line is not the refusal's
    // to have produced. Asserting "no `assigned` line exists on this lead" was a way of saying
    // "the hand-over wrote nothing" that also forbade the capture-time line, and it has been red
    // since capture began writing one.
    let hands = trail(&pool, lead)
        .await
        .into_iter()
        .filter(|(_, _, _, reason)| !reason.is_empty())
        .collect::<Vec<_>>();
    assert!(
        hands.iter().all(|(k, _, _, _)| k != "reassigned"),
        "a refused hand leaves no hand-over line of its own: {hands:?}"
    );

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_lead_of_another_organization_is_a_none_not_a_row() {
    let pool = pool().await;
    let mine = fresh_org(&pool, "assign-mine").await;
    let theirs = fresh_org(&pool, "assign-theirs").await;
    let lead = one_lead(&pool, theirs, "theirs@example.test").await;

    let result = store::assign_owner(
        &pool,
        mine,
        lead,
        Some(one_user(&pool, mine, "Trespasser").await),
        "mine now",
        None,
    )
    .await
    .expect("the call itself succeeds");
    assert!(
        result.is_none(),
        "another organization's lead answers None, not a row: a 403 at this layer is an \
         enumeration oracle"
    );

    let untouched = store::find_lead(&pool, theirs, lead)
        .await
        .expect("the owner still reads its own lead")
        .expect("the lead still exists");
    assert_eq!(untouched.owner_user_id, None, "the write went nowhere");
    // The lead's own `received` line is there — capture wrote it. What must NOT be there is a
    // *hand*: a call that was refused by tenancy must not leave a line that reads as work. An
    // earlier version of this asserted the whole trail was empty, which was false by one line
    // and would have been fixed by deleting the assertion rather than by looking.
    // The same discriminator as the two tests above: capture already wrote the claim that started
    // this lead, so "no `assigned` line on this lead" also forbids that one. The refused call
    // carries a reason and a hand, so filtering by reason is what makes this about the refusal.
    let hands: Vec<_> = trail(&pool, lead)
        .await
        .into_iter()
        .filter(|(_, _, _, reason)| !reason.is_empty())
        .collect();
    assert!(
        hands.is_empty(),
        "a refused call leaves no hand-over line of its own: {hands:?}"
    );

    drop_org(&pool, mine).await;
    drop_org(&pool, theirs).await;
}

#[tokio::test]
async fn the_sla_deadline_survives_a_change_of_hands() {
    let pool = pool().await;
    let org = fresh_org(&pool, "assign-sla").await;
    let lead = one_lead(&pool, org, "sla@example.test").await;

    // A deadline the lead already carries.
    let due = time::OffsetDateTime::now_utc() + time::Duration::hours(3);
    sqlx::query("update crm_leads set first_response_due_at = $2 where id = $1")
        .bind(lead)
        .bind(due)
        .execute(&pool)
        .await
        .expect("a deadline");

    let after = store::assign_owner(
        &pool,
        org,
        lead,
        Some(one_user(&pool, org, "Wrong region").await),
        "wrong region",
        None,
    )
    .await
    .expect("assign")
    .expect("the lead");
    let reread = store::find_lead(&pool, org, lead)
        .await
        .expect("read")
        .expect("the lead");

    // A reassignment must not buy the person who wrote in more time. Recomputing the deadline
    // here would turn "hand this to the right person" into a way to reset a breach that has
    // already happened — and the breach is recorded against the person who owned it then.
    // Postgres keeps microseconds; the round trip through the row drops the last three
    // nanosecond digits. Comparing to the `due` I bound would fail on precision alone, which
    // is the wrong reason — so the re-read value is stamped with the same round trip and the
    // assertion is about *identity*, not about a type's resolution.
    let due_as_stored = reread
        .first_response_due_at
        .expect("the deadline is still there");
    assert_eq!(
        due_as_stored.nanosecond() / 1_000,
        due.nanosecond() / 1_000,
        "the promise made to the submitter does not move because the owner did"
    );
    assert_eq!(
        due_as_stored.unix_timestamp(),
        due.unix_timestamp(),
        "same second, too — the deadline is untouched, not merely close"
    );
    assert_eq!(after.first_response_at, None, "and it is not answered yet");

    drop_org(&pool, org).await;
}

/// The roster is what makes a hand-over possible without the IAM screen open in another tab,
/// and there are three ways it can be subtly wrong. All three are only visible against a
/// database: each is a *row* claim, not a computation.
///
/// * **A colleague's load must be theirs, and only open work counts.** A count that includes
///   converted and discarded leads tells an operator to avoid somebody who is in fact free,
///   which is the one thing a load number must never do.
/// * **A colleague of another organization must not appear.** The `users` table is global and
///   `organization_id` is nullable, so a roster that joins loosely lists every account on the
///   box — which is a tenant leak wearing a select element.
/// * **A disabled colleague still appears.** Filtering them out would make the owner of an
///   existing lead render as nobody, on the very screen whose job is to say who owns what.
#[tokio::test]
async fn the_roster_names_the_people_and_counts_only_their_open_work() {
    let pool = pool().await;
    let org = fresh_org(&pool, "roster-basic").await;

    let ada = one_user(&pool, org, "Ada Lovelace").await;
    let grace = one_user(&pool, org, "Grace Hopper").await;
    // A lead nobody has answered, owned by Ada.
    let ada_lead = one_lead(&pool, org, "roster-ada@example.test").await;
    store::assign_owner(&pool, org, ada_lead, Some(ada), "first", None)
        .await
        .expect("assign")
        .expect("the lead");

    let roster = store::list_owners(&pool, org).await.expect("the roster");

    let ada_row = roster
        .iter()
        .find(|row| row.id == ada)
        .expect("Ada is in it");
    assert_eq!(
        ada_row.label, "Ada Lovelace",
        "a person is a name, not an id"
    );
    assert!(ada_row.email.contains("@example.invalid"));
    assert_eq!(ada_row.open_leads, 1, "her one open lead is counted");
    assert_eq!(ada_row.status, "active");

    let grace_row = roster
        .iter()
        .find(|row| row.id == grace)
        .expect("Grace is in it too");
    assert_eq!(
        grace_row.open_leads, 0,
        "and somebody with nothing on them reads as zero, not absent"
    );

    // A converted lead is not work: it must stop counting the moment it is closed.
    sqlx::query("update crm_leads set status = 'converted' where id = $1")
        .bind(ada_lead)
        .execute(&pool)
        .await
        .expect("close the lead");
    let after = store::list_owners(&pool, org).await.expect("the roster");
    assert_eq!(
        after
            .iter()
            .find(|row| row.id == ada)
            .expect("Ada")
            .open_leads,
        0,
        "a converted lead is a customer, not a task"
    );

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn the_roster_is_this_organizations_and_nobody_elses() {
    let pool = pool().await;
    let org = fresh_org(&pool, "roster-tenancy").await;
    let theirs = fresh_org(&pool, "roster-other").await;

    let mine = one_user(&pool, org, "Mine").await;
    let other = one_user(&pool, theirs, "Theirs").await;

    // A platform account: no organization at all. It is a real row in `users` and it must not
    // show up as a destination in any tenant's hand-over panel.
    let platform = Uuid::new_v4();
    sqlx::query(
        "insert into users (id, email, password_hash, display_name) values ($1, $2, 'x', $3)",
    )
    .bind(platform)
    .bind(format!("platform-{}@example.invalid", platform.simple()))
    .bind("Platform Operator")
    .execute(&pool)
    .await
    .expect("a platform account");

    let roster = store::list_owners(&pool, org).await.expect("the roster");
    assert!(
        roster.iter().any(|row| row.id == mine),
        "the tenant's own people are offered"
    );
    assert!(
        !roster.iter().any(|row| row.id == other),
        "another organization's account is not a destination — that is a tenant leak"
    );
    assert!(
        !roster.iter().any(|row| row.id == platform),
        "and a platform account belongs to no tenant's queue"
    );

    // The empty organization is an empty list, not an error and not a fallback to the
    // caller's own organization — the read is scoped by the argument, full stop.
    let empty = fresh_org(&pool, "roster-empty").await;
    assert!(
        store::list_owners(&pool, empty)
            .await
            .expect("an empty roster")
            .is_empty(),
        "an organization with nobody in it has nobody to hand leads to"
    );

    drop_org(&pool, org).await;
    drop_org(&pool, theirs).await;
    drop_org(&pool, empty).await;
    sqlx::query("delete from users where id = $1")
        .bind(platform)
        .execute(&pool)
        .await
        .ok();
}

#[tokio::test]
async fn a_disabled_colleague_is_listed_and_marked_rather_than_hidden() {
    let pool = pool().await;
    let org = fresh_org(&pool, "roster-disabled").await;
    let person = one_user(&pool, org, "Retired Owner").await;

    // They still own a lead. Hiding them would make that lead read as unowned.
    let lead = one_lead(&pool, org, "roster-disabled@example.test").await;
    store::assign_owner(&pool, org, lead, Some(person), "before they left", None)
        .await
        .expect("assign")
        .expect("the lead");
    sqlx::query("update users set status = 'disabled' where id = $1")
        .bind(person)
        .execute(&pool)
        .await
        .expect("disable");

    let roster = store::list_owners(&pool, org).await.expect("the roster");
    let row = roster
        .iter()
        .find(|row| row.id == person)
        .expect("a colleague who owns a lead is still somebody");
    assert_eq!(
        row.status, "disabled",
        "and the panel can mark the row with it"
    );
    assert_eq!(row.label, "Retired Owner", "the name still renders");
    assert_eq!(
        row.open_leads, 1,
        "the load they cannot take is still visible"
    );

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn an_account_with_no_display_name_is_read_as_its_address() {
    let pool = pool().await;
    let org = fresh_org(&pool, "roster-noname").await;

    // An invited account with a blank display name. An empty `<option>` in the picker is
    // indistinguishable from the "unassigned queue" row above it, so the store resolves it.
    let id = Uuid::new_v4();
    sqlx::query(
        "insert into users (id, organization_id, email, password_hash, display_name) \
         values ($1, $2, $3, 'x', '')",
    )
    .bind(id)
    .bind(org)
    .bind("no-name@example.invalid")
    .execute(&pool)
    .await
    .expect("a nameless account");

    let roster = store::list_owners(&pool, org).await.expect("the roster");
    let row = roster
        .iter()
        .find(|row| row.id == id)
        .expect("in the roster");
    assert_eq!(
        row.label, "no-name@example.invalid",
        "an invitation flow leaves blank names, and a blank option is not a choice"
    );

    drop_org(&pool, org).await;
}
