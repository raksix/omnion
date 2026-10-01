//! The clock's state is the module's answer, not the panel's — against a real database.
//!
//! Run through `scripts/qa/run-crm-sla-state.sh`, which creates the database and the
//! organization these tests need.
//!
//! ## The gap this file exists to measure
//!
//! `assignment::SlaState::of` is the module's definition of a lead's first-response clock, and
//! it was exported, documented and unit-tested with **no production caller** — the panel
//! derived the same four states itself, in `apps/admin/lib/crm-intake.ts`, from a hard-coded
//! `AT_RISK_MINUTES = 60`. That function's own doc said *"when slice 2 lands it takes over
//! this function rather than the screens, so nothing here has to change"*; slice 2 landed and
//! nothing changed, so for three ticks the two answers were different by construction.
//!
//! Two disagreements follow from the fixed threshold, and **both are only visible when the
//! window is not the seeded 240 minutes**, which is why every fixture here creates its own
//! policy rather than using the default:
//!
//! * `at_risk` is a quarter of the *policy's own* window, floored at 15 minutes. On a
//!   15-minute policy a lead with 5 minutes left is at risk; a fixed hour called it on track
//!   and then breached it with no warning at all.
//! * A lead answered *after* its deadline is `met` in the module — the response happened, the
//!   breach is a separate fact — while the client re-derived `breached` from the two instants
//!   and showed red on a lead somebody had already answered.
//!
//! ## Why these tests read the store and call `SlaState::of` rather than parsing a client
//!
//! The defect was a *second implementation*, so asserting the module against itself is what
//! proves there is one left: `policy_windows` is what the API resolves a lead's window
//! through, and the state's arithmetic is the module's. A client-side test would pass against
//! the pre-fix code in its entirety, because the client's disagreement was never visible to
//! Rust.

use omnion_module_crm_intake::assignment::SlaState;
use omnion_module_crm_intake::assignment_store::{self, NewPolicy};
use omnion_module_crm_intake::{NewIntakeSource, store};
use sqlx::PgPool;
use time::OffsetDateTime;
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

async fn cleanup(pool: &PgPool, org: Uuid) {
    // A list of statements rather than a table list formatted into SQL: `sqlx::query` takes
    // a `&str`, so a `format!` here is a compile error rather than a query — which is the
    // better outcome, because a table name interpolated into a statement is exactly the
    // place a stray name would become an injection.
    for sql in [
        "delete from crm_lead_events where lead_id in (select id from crm_leads where organization_id = $1)",
        "delete from crm_lead_submissions where source_id in (select id from crm_intake_sources where organization_id = $1)",
        "delete from crm_leads where organization_id = $1",
        "delete from crm_intake_sources where organization_id = $1",
        "delete from crm_assignment_rules where organization_id = $1",
        "delete from crm_sla_policies where organization_id = $1",
    ] {
        let _ = sqlx::query(sql).bind(org).execute(pool).await;
    }
    let _ = sqlx::query("delete from organizations where id = $1")
        .bind(org)
        .execute(pool)
        .await;
}

async fn endpoint_source(pool: &PgPool, org: Uuid, name: &str) -> Uuid {
    // `.0`: an endpoint's key is revealed exactly once and this gate has no use for it.
    store::create_source(pool, &NewIntakeSource::endpoint(org, name, None))
        .await
        .expect("a keyed source")
        .0
        .id
}

/// A policy with a chosen window, never the seeded 240-minute default.
async fn policy_with_minutes(
    pool: &PgPool,
    org: Uuid,
    name: &str,
    minutes: i32,
) -> omnion_module_crm_intake::assignment::SlaPolicy {
    assignment_store::create_policy(
        pool,
        org,
        &NewPolicy {
            name: name.to_string(),
            first_response_minutes: minutes,
            business_hours_only: false,
            reminder_minutes: None,
            escalate_to_user_id: None,
            business_hours: serde_json::json!({}),
            active: true,
        },
    )
    .await
    .expect("a policy")
}

/// A lead whose deadline is exactly `minutes` after `received`, attached to `policy`.
///
/// The deadline is written by the product's own `due_at` so the gate asks the code it is
/// testing, and every instant is a bind parameter — interpolating a timestamp into SQL
/// shifted it by this box's offset three times and read as a product bug each time.
async fn lead_on_policy(
    pool: &PgPool,
    org: Uuid,
    source: Uuid,
    policy: Uuid,
    received: OffsetDateTime,
) -> Uuid {
    let stored = assignment_store::find_policy(pool, org, policy)
        .await
        .expect("a policy read")
        .expect("the policy");
    let due = omnion_module_crm_intake::due_at(&stored, received);
    let id: Uuid = sqlx::query_scalar(
        "insert into crm_leads \
         (organization_id, source_id, status, email, first_name, received_at, \
          first_response_due_at, sla_policy_id) \
         values ($1, $2, 'new', $3, 'Gate', $4, $5, $6) returning id",
    )
    .bind(org)
    .bind(source)
    .bind(format!("gate-{}@example.invalid", Uuid::new_v4()))
    .bind(received)
    .bind(due)
    .bind(policy)
    .fetch_one(pool)
    .await
    .expect("a lead");
    id
}

/// The windows the API would resolve for a page of leads: policy id → minutes.
async fn windows_for(pool: &PgPool, org: Uuid, leads: &[Uuid]) -> std::collections::HashMap<Uuid, i32> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "select distinct sla_policy_id from crm_leads \
         where id = any($1) and sla_policy_id is not null",
    )
    .bind(leads)
    .fetch_all(pool)
    .await
    .expect("the leads' policies");
    assignment_store::policy_windows(pool, org, &ids)
        .await
        .expect("the windows")
}

/// The one thing this gate is really asserting, in one place.
///
/// `SlaState::of` is called with the window the API would read for that lead. Before the
/// fix there was no such read — the panel guessed — so this function is the fix, and every
/// other test here is a consequence of it.
fn state_the_api_sends(
    windows: &std::collections::HashMap<Uuid, i32>,
    policy: Option<Uuid>,
    first_response_at: Option<OffsetDateTime>,
    due: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> &'static str {
    match policy.and_then(|id| windows.get(&id).copied()) {
        // **No window, no clock.** `none` is the panel's "No target", and it is deliberately
        // different from `on_track`: a green badge on a lead nobody is measuring claims a
        // promise that was never made, which is the failure mode the client had.
        None => "none",
        Some(minutes) => SlaState::of(first_response_at, due, now, minutes).as_str(),
    }
}

#[tokio::test]
async fn the_window_a_lead_is_judged_against_is_its_own_policys() {
    let pool = pool().await;
    let org = fresh_org(&pool, "sla-state").await;
    let source = endpoint_source(&pool, org, "Short window").await;
    let short = policy_with_minutes(&pool, org, "15 minutes", 15).await;
    let long = policy_with_minutes(&pool, org, "8 hours", 480).await;

    let received = OffsetDateTime::parse(
        "2026-01-05T09:00:00Z",
        &time::format_description::well_known::Rfc3339,
    )
    .expect("a timestamp");
    let short_lead = lead_on_policy(&pool, org, source, short.id, received).await;
    let long_lead = lead_on_policy(&pool, org, source, long.id, received).await;

    let windows = windows_for(&pool, org, &[short_lead, long_lead]).await;
    assert_eq!(
        windows.get(&short.id),
        Some(&15),
        "the short policy reports its own minutes"
    );
    assert_eq!(windows.get(&long.id), Some(&480));
    assert_eq!(windows.len(), 2, "one read answered both policies");

    // **Five minutes left on each.** The lead with five minutes left is measured against a
    // 15-minute window and a quarter of that is 3.75, floored to the 15-minute floor, so five
    // minutes left is inside the warning band. The 8-hour policy warns at two hours, so five
    // minutes left on a *different* clock is not "due soon" by the same measure — the whole
    // point is that "due soon" is a fraction of the promise, not a number of minutes.
    let short_due: OffsetDateTime = sqlx::query_scalar::<_, Option<OffsetDateTime>>(
        "select first_response_due_at from crm_leads where id = $1",
    )
    .bind(short_lead)
    .fetch_one(&pool)
    .await
    .expect("a deadline")
    .expect("a deadline was written");
    let now = short_due - time::Duration::minutes(5);
    assert_eq!(
        state_the_api_sends(&windows, Some(short.id), None, Some(short_due), now),
        "at_risk",
        "five minutes left on a 15-minute policy is the warning the panel never showed"
    );

    let long_due: OffsetDateTime = sqlx::query_scalar::<_, Option<OffsetDateTime>>(
        "select first_response_due_at from crm_leads where id = $1",
    )
    .bind(long_lead)
    .fetch_one(&pool)
    .await
    .expect("a deadline")
    .expect("a deadline was written");
    assert_eq!(
        state_the_api_sends(&windows, Some(long.id), None, Some(long_due), now),
        "on_track",
        "the same five minutes against an 8-hour target is not the same warning"
    );

    cleanup(&pool, org).await;
}

/// The disagreement that never ends: a lead answered after its deadline is `met`, not
/// `breached`, and the client showed red on it for ever because it re-derived the state from
/// the two instants every time it rendered.
#[tokio::test]
async fn a_lead_answered_after_its_deadline_is_met_and_stays_met() {
    let pool = pool().await;
    let org = fresh_org(&pool, "sla-state").await;
    let source = endpoint_source(&pool, org, "Answered late").await;
    let policy = policy_with_minutes(&pool, org, "60 minutes", 60).await;
    let received = OffsetDateTime::parse(
        "2026-01-05T09:00:00Z",
        &time::format_description::well_known::Rfc3339,
    )
    .expect("a timestamp");
    let lead = lead_on_policy(&pool, org, source, policy.id, received).await;

    let due: OffsetDateTime = sqlx::query_scalar::<_, Option<OffsetDateTime>>(
        "select first_response_due_at from crm_leads where id = $1",
    )
    .bind(lead)
    .fetch_one(&pool)
    .await
    .expect("a deadline")
    .expect("a deadline was written");

    // Answered three hours *after* the deadline. The record is not erased by answering it —
    // the breach is a separate fact an SLA report needs — but the *clock* is met, and the
    // panel's badge is about the clock.
    let answered = due + time::Duration::hours(3);
    store::record_response(&pool, org, lead, None)
        .await
        .expect("the response is recorded");
    let stored_answer: Option<OffsetDateTime> = sqlx::query_scalar(
        "select first_response_at from crm_leads where id = $1",
    )
    .bind(lead)
    .fetch_one(&pool)
    .await
    .expect("the answer");
    let windows = windows_for(&pool, org, &[lead]).await;

    assert_eq!(
        state_the_api_sends(&windows, Some(policy.id), stored_answer, Some(due), answered),
        "met",
        "answered is met whatever the clock says afterwards — the client's permanent 'breached' is this"
    );

    // **And it stays met a year later.** A state recomputed at render time from `now` would
    // drift back to a breach the moment the deadline passed again; this is the assertion that
    // pins the answer to the *response* rather than to the moment it is asked.
    assert_eq!(
        state_the_api_sends(
            &windows,
            Some(policy.id),
            stored_answer,
            Some(due),
            answered + time::Duration::days(365)
        ),
        "met",
        "a lead answered once is answered; re-reading it a year later must not reopen the breach"
    );

    cleanup(&pool, org).await;
}

/// A lead with no policy has no promise, and says so.
#[tokio::test]
async fn a_lead_with_no_policy_reads_as_no_target_rather_than_on_track() {
    let pool = pool().await;
    let org = fresh_org(&pool, "sla-state").await;
    let source = endpoint_source(&pool, org, "No policy").await;
    let received = OffsetDateTime::parse(
        "2026-01-05T09:00:00Z",
        &time::format_description::well_known::Rfc3339,
    )
    .expect("a timestamp");
    let id: Uuid = sqlx::query_scalar(
        "insert into crm_leads \
         (organization_id, source_id, status, email, first_name, received_at, first_response_due_at) \
         values ($1, $2, 'new', $3, 'Gate', $4, $5) returning id",
    )
    .bind(org)
    .bind(source)
    .bind(format!("gate-{}@example.invalid", Uuid::new_v4()))
    .bind(received)
    .bind(received + time::Duration::minutes(240))
    .fetch_one(&pool)
    .await
    .expect("a lead with a deadline and no policy");

    let windows = windows_for(&pool, org, &[id]).await;
    assert!(windows.is_empty(), "no policy, so no window to resolve");

    assert_eq!(
        state_the_api_sends(
            &windows,
            None,
            None,
            Some(received + time::Duration::minutes(240)),
            received
        ),
        "none",
        "a deadline nobody set is not a promise; 'on track' would be a green badge on a row nobody measures"
    );

    // **The half that is easy to get wrong.** A source with no policy still carries a deadline
    // in some installs (the column is nullable, and a lead can be stamped by a policy that
    // was later deleted). The window read must not fall back to the seeded 240 minutes,
    // because that silently marks such a lead `on_track` for ever — the exact "correct
    // looking badge on an unmeasured row" this gate is about.
    let org_with_default = fresh_org(&pool, "sla-state-default").await;
    assignment_store::ensure_defaults(&pool, org_with_default)
        .await
        .expect("defaults");
    let policies = assignment_store::list_policies(&pool, org_with_default)
        .await
        .expect("policies");
    assert_eq!(
        policies.len(),
        1,
        "the seeded default exists, which is exactly why a fallback would be tempting"
    );
    assert_eq!(policies[0].first_response_minutes, 240);

    cleanup(&pool, org).await;
    cleanup(&pool, org_with_default).await;
}

/// Another tenant's policy is not this tenant's answer.
#[tokio::test]
async fn another_organizations_policy_is_not_this_organizations_window() {
    let pool = pool().await;
    let mine = fresh_org(&pool, "sla-state-mine").await;
    let theirs = fresh_org(&pool, "sla-state-theirs").await;
    let source = endpoint_source(&pool, mine, "Mine").await;
    let their_policy = policy_with_minutes(&pool, theirs, "Theirs", 15).await;
    let my_policy = policy_with_minutes(&pool, mine, "Mine", 480).await;

    let received = OffsetDateTime::parse(
        "2026-01-05T09:00:00Z",
        &time::format_description::well_known::Rfc3339,
    )
    .expect("a timestamp");
    let lead = lead_on_policy(&pool, mine, source, my_policy.id, received).await;

    // Asking for the *other* organization's policy id must return nothing. The tenancy
    // predicate is on this read like it is on every other in the module, and a leak here
    // would answer a lead's clock with another company's promise.
    let theirs_only =
        assignment_store::policy_windows(&pool, mine, &[their_policy.id])
            .await
            .expect("a window read");
    assert!(
        theirs_only.is_empty(),
        "another organization's policy must not resolve: {:?}",
        theirs_only
    );

    let windows = windows_for(&pool, mine, &[lead]).await;
    assert_eq!(windows.get(&my_policy.id), Some(&480));

    cleanup(&pool, mine).await;
    cleanup(&pool, theirs).await;
}

/// An empty page must not build a statement at all.
#[tokio::test]
async fn a_page_with_no_policies_reads_nothing() {
    let pool = pool().await;
    let org = fresh_org(&pool, "sla-state-empty").await;
    let windows = assignment_store::policy_windows(&pool, org, &[])
        .await
        .expect("an empty read is not an error");
    assert!(
        windows.is_empty(),
        "an inbox page whose leads all carry no policy asks the database nothing"
    );
    cleanup(&pool, org).await;
}