//! The verdict-row gate: a submission with nothing to contact is **recorded**, not lost.
//!
//! Run through `scripts/qa/run-crm-verdict-rows.sh`.
//!
//! ## The defect this file exists for
//!
//! REQ-117 acceptance 5: "A submission missing both e-mail and phone is refused with a readable
//! reason, and no partial lead row is written." The store's half shipped in slice 1 and reads
//! correctly:
//!
//! ```text
//! // 3. Nothing to contact: a rejected row, never a partial lead.
//! if !mapped.missing_required.is_empty() || !contactable(email, phone) {
//!     let lead = insert_lead(..., LeadWrite { status: "rejected", rejection_reason: Some(reason), ... })
//! ```
//!
//! **On this branch that insert raised `23514` and the row was never written.** `crm_leads`
//! carried
//!
//! ```sql
//! constraint crm_leads_contactable_check
//!     check (coalesce(email,'') <> '' or coalesce(phone,'') <> '')
//! ```
//!
//! and the row being inserted is by construction a row with neither. So the one path the REQ
//! promises a record for was the one path the database refused, and it surfaced to the visitor
//! as a 500 instead of as a readable reason.
//!
//! It survived twenty ticks because **nothing drove it**: the `rejected` branch needs a mapping
//! that produces no e-mail and no phone, and every CRM gate's fixture maps `email`. Two sibling
//! files recorded the absence in the words of a workaround rather than a test —
//! `crm_autoresponder.rs` says the case is "unreachable through this fixture on purpose", and
//! `crm_binding_health.rs` says the health check's `Unknown` state "is not reachable through
//! capture on this branch". **Both sentences are now false, and both were a test that was
//! written to fit the defect.** Migration `0159` is the fix; these tests are the evidence that
//! it landed and that it did not loosen anything else.
//!
//! ## What a gate on this feature has to measure
//!
//! Not `insert_lead` — it is `capture`, because the whole defect lived in what the *database*
//! said about a row the store hands it. Every test below goes through `store::capture` and reads
//! the row back out, because a unit test on the store would have been green for twenty ticks.
//!
//! ## The three states, and why three
//!
//! A submission can produce no address because the **mapping** has no e-mail target, because a
//! **required target** was not filled, or because the visitor sent neither. All three are
//! rejections and all three are rows. The fourth state — a lead that is `new` with no address —
//! stays a refusal, and `scripts/qa/run-crm-intake.sh` asserts it at the SQL level in both
//! directions: an open contactless row is refused, a `rejected` one is written.

use omnion_module_crm_intake::store::{self, Submission};
use omnion_module_crm_intake::{MappingEntry, NewIntakeSource};
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
        "delete from crm_lead_submissions where source_id in (select id from crm_intake_sources where organization_id = $1)",
        "delete from crm_leads where organization_id = $1",
        "delete from crm_intake_sources where organization_id = $1",
        "delete from organizations where id = $1",
    ] {
        sqlx::query(sql)
            .bind(org)
            .execute(pool)
            .await
            .expect("the test's own rows");
    }
}

/// A source whose mapping asks for `email` and requires it.
///
/// **`.required()` is on the line, not on the source's `required_targets` list.** The list is
/// validated at *save* time — it refuses a mapping that could never fill the target — while
/// `mapping::apply` decides what is "missing" from the line's own flag. The first version of
/// this test set only the list, and the submission came back with "neither e-mail nor phone
/// was submitted": the right status for the wrong reason, on a test loose enough to pass on it.
/// The reason string is the whole content of a verdict row, so the test has to name which
/// branch it is on.
fn required_contact_mapping() -> Vec<MappingEntry> {
    vec![MappingEntry::new("email", "email").required()]
}

async fn source_with(pool: &PgPool, org: Uuid, mapping: Vec<MappingEntry>, required: Vec<String>) -> Uuid {
    let label = format!("verdict rows {}", Uuid::new_v4().simple());
    let draft = NewIntakeSource::endpoint(org, &label, None).with_mapping(mapping, required);
    let (created, _key) = store::create_source(pool, &draft).await.expect("a source");
    created.id
}

fn submission(org: Uuid, source_id: Uuid, payload: serde_json::Value) -> Submission {
    Submission {
        organization_id: org,
        site_id: None,
        source_id,
        submission_id: Some(Uuid::new_v4().to_string()),
        ip: Some("203.0.113.44".to_string()),
        payload,
        received_at: time::OffsetDateTime::now_utc(),
    }
}

/// The row as the database holds it, not the struct that wrote it.
///
/// This crate has been bitten repeatedly by trusting the struct: `OffsetDateTime` serialised
/// as a component array, and a skip line read as a sent one. The defect this file exists for was
/// *a constraint the struct knows nothing about*, so the only place it can be seen is the row.
async fn stored_rejection(pool: &PgPool, org: Uuid) -> (String, Option<String>, Option<String>) {
    let row: (String, Option<String>, Option<String>) = sqlx::query_as(
        "select status, coalesce(email, ''), coalesce(rejection_reason, '')
         from crm_leads where organization_id = $1 order by received_at desc, id desc limit 1",
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .expect("a stored lead for the organization");
    (row.0, row.1, row.2)
}

/// **The core line.** A submission whose mapping yields no e-mail and no phone is recorded as a
/// `rejected` row with a reason an operator can read.
///
/// Under the old constraint this raised `23514` from `insert_lead` and `capture` returned an
/// error, so the row below did not exist and the assertion could not even be written — the test
/// was written as a workaround comment instead. Note what is *not* asserted: nothing here
/// expects the capture to fail, and nothing tolerates a partial lead.
#[tokio::test]
async fn a_submission_with_no_contactable_value_is_recorded_as_rejected() {
    let pool = pool().await;
    let org = fresh_org(&pool, "verdict-no-address").await;
    // A mapping that has no e-mail and no phone target at all: this is the smallest submission
    // that reaches the branch, and the one a plain endpoint source produces.
    let source_id = source_with(&pool, org, vec![MappingEntry::new("message", "note")], vec![])
        .await;

    let captured = store::capture(
        &pool,
        &submission(
            org,
            source_id,
            serde_json::json!({ "note": "I would like a quote", "submitted_in_ms": 4200 }),
        ),
    )
    .await
    .expect("a refused submission is still a captured submission, not an error");

    assert_eq!(
        captured.lead.status, "rejected",
        "no contactable value is a verdict, and the row says so"
    );
    assert_eq!(
        captured.lead.decision.as_deref(),
        Some("rejected"),
        "the dedupe verdict is the same verdict: this row was never a candidate"
    );
    let (status, email, reason) = stored_rejection(&pool, org).await;
    assert_eq!(status, "rejected");
    assert_eq!(
        email.as_deref(),
        Some(""),
        "the stored row really has no address — this is the row the constraint used to refuse"
    );
    assert_eq!(
        reason.as_deref(),
        Some("neither e-mail nor phone was submitted"),
        "the reason is the sentence the operator reads, and it is stored rather than returned"
    );
    // The message the visitor's form should render. Named here because the REQ's promise is a
    // *readable reason* and a stored `either` is not one.
    assert_eq!(
        captured.lead.rejection_reason.as_deref(),
        Some("neither e-mail nor phone was submitted")
    );

    drop_org(&pool, org).await;
}

/// A required target that was not filled is a **different** reason from "no address at all",
/// and it names the field. Both land in the same status, so the reason string is the only
/// thing telling an operator whether the form is broken or the visitor was in a hurry.
#[tokio::test]
async fn an_unfilled_required_target_names_the_field_in_the_stored_reason() {
    let pool = pool().await;
    let org = fresh_org(&pool, "verdict-missing-required").await;
    let source_id = source_with(&pool, org, required_contact_mapping(), vec!["email".to_string()]).await;

    // No `email` key at all, so the required target is missing rather than blank.
    let captured = store::capture(
        &pool,
        &submission(
            org,
            source_id,
            serde_json::json!({ "submitted_in_ms": 4200 }),
        ),
    )
    .await
    .expect("a submission missing a required target is recorded");

    assert_eq!(captured.lead.status, "rejected");
    let reason = captured
        .lead
        .rejection_reason
        .as_deref()
        .expect("the reason names the field");
    assert_eq!(
        reason, "required field(s) not filled: email",
        "the field is named rather than counted, so an operator can fix the mapping or the form"
    );

    drop_org(&pool, org).await;
}

/// A rejected row is **work for nobody**: it must not sit in the inbox's unassigned counter,
/// and it must not be assignable. The SLA index already excludes it, and `is_open` reads the
/// same vocabulary — this asserts both, because a verdict that counts as unassigned work is a
/// number an operator cannot act on.
#[tokio::test]
async fn a_rejected_row_is_not_open_work() {
    let pool = pool().await;
    let org = fresh_org(&pool, "verdict-not-open").await;
    let source_id = source_with(&pool, org, vec![MappingEntry::new("message", "note")], vec![]).await;

    store::capture(
        &pool,
        &submission(org, source_id, serde_json::json!({ "note": "hello" })),
    )
    .await
    .expect("the rejected row");

    let page = store::list_leads(
        &pool,
        org,
        &store::LeadQuery {
            source_id: None,
            status: Vec::new(),
            owner: Some("unassigned".to_string()),
            acting_user_id: None,
            search: None,
            product_interest: None,
            since: None,
            until: None,
            limit: 25,
            before: None,
        },
    )
    .await
    .expect("the inbox read");

    assert_eq!(
        page.metrics.unassigned, 0,
        "a submission nobody can answer is not work waiting for an owner"
    );
    assert!(
        page.metrics.discarded >= 1,
        "and it IS counted as discarded, so the counter an operator reads says it arrived"
    );

    drop_org(&pool, org).await;
}

/// **The negative half, and the reason this gate can fail.** The check was narrowed; if the
/// narrowing had been written as "drop the constraint", the open-lead refusal would go with it
/// and every other lead in the platform would become unanswerable. This is the assertion that
/// names which side of the line each status is on.
#[tokio::test]
async fn an_open_lead_with_no_address_is_still_refused() {
    let pool = pool().await;
    let org = fresh_org(&pool, "verdict-open-still-refused").await;

    let error = sqlx::query(
        "insert into crm_leads (organization_id, status, first_name)
         values ($1, 'new', 'Nobody')",
    )
    .bind(org)
    .execute(&pool)
    .await
    .expect_err("an OPEN lead with neither e-mail nor phone is still refused at the database");

    let text = error.to_string();
    assert!(
        text.contains("crm_leads_contactable_check"),
        "the refusal names the constraint, not some other rule: {text}"
    );

    // And the same insert under a verdict status is accepted — the other half of the line, so
    // that a future edit cannot pass by inverting which side is refused.
    sqlx::query(
        "insert into crm_leads (organization_id, status, first_name, rejection_reason)
         values ($1, 'rejected', 'Nobody', 'neither e-mail nor phone was submitted')",
    )
    .bind(org)
    .execute(&pool)
    .await
    .expect("a verdict row with no address is the shape REQ-117 requires");

    drop_org(&pool, org).await;
}

/// `spam` and `duplicate` are the other two verdicts, and they are in the narrowing for the same
/// reason: a spam submission can carry no address (a honeypot hit has no reason to fill the
/// form), and a duplicate is a lead somebody may still link from the duplicate queue.
#[tokio::test]
async fn the_other_two_verdict_statuses_are_allowed_without_an_address() {
    let pool = pool().await;
    let org = fresh_org(&pool, "verdict-other-two").await;

    for (status, reason) in [
        ("spam", "spam heuristics: honeypot"),
        ("duplicate", "already matched an existing contact on e-mail"),
    ] {
        sqlx::query(
            "insert into crm_leads (organization_id, status, first_name, rejection_reason)
             values ($1, $2, 'Nobody', $3)",
        )
        .bind(org)
        .bind(status)
        .bind(reason)
        .execute(&pool)
        .await
        .unwrap_or_else(|error| panic!("{status} with no address must be a row, not a 23514: {error}"));
    }

    let counted: i64 =
        sqlx::query_scalar("select count(*) from crm_leads where organization_id = $1 and email is null")
            .bind(org)
            .fetch_one(&pool)
            .await
            .expect("the count");
    assert_eq!(counted, 2);

    drop_org(&pool, org).await;
}

/// A rejected row that is later **given** an address becomes an ordinary lead again, and the
/// edit path is what does it. This is the half the `patch_lead` guard exists to protect, and it
/// is also the reason a rejected row is worth keeping: the visitor comes back, the operator
/// pastes the address, and the row becomes the lead it should have been.
#[tokio::test]
async fn a_rejected_row_can_be_given_an_address_by_an_edit() {
    let pool = pool().await;
    let org = fresh_org(&pool, "verdict-then-fixed").await;
    let source_id = source_with(&pool, org, vec![MappingEntry::new("message", "note")], vec![]).await;

    let captured = store::capture(
        &pool,
        &submission(org, source_id, serde_json::json!({ "note": "hello" })),
    )
    .await
    .expect("the rejected row");

    let updated = store::patch_lead(
        &pool,
        org,
        captured.lead.id,
        &store::LeadPatch {
            status: Some("new".to_string()),
            email: Some("ada@example.com".to_string()),
            ..Default::default()
        },
    )
    .await
    .expect("the edit is allowed: it supplies the address the row was missing")
    .expect("the lead is there");

    assert_eq!(updated.status, "new");
    assert_eq!(updated.email.as_deref(), Some("ada@example.com"));

    // The trail records the move, so an operator can see the row was a refusal first. A row
    // that changed status silently reads as one that was never rejected.
    let events = store::list_events(&pool, org, captured.lead.id)
        .await
        .expect("the trail");
    assert!(
        events.iter().any(|event| event.kind == "received"),
        "the arrival is on the trail"
    );
    assert!(
        events.iter().any(|event| event.kind == "edited" || event.kind == "status_changed"),
        "and so is the change an operator made to it, not just the arrival"
    );

    drop_org(&pool, org).await;
}

/// The capture must not raise just because the row has no address — and the claim must be
/// finished, or a redelivery of the same submission answers "already being captured" for ever.
///
/// The claim half is the part a test that only checked the row would miss: the rejected branch
/// returns early, and an early return that skips `finish_claim` is the wedge this crate has
/// already paid for once.
#[tokio::test]
async fn a_rejected_submission_finishes_its_claim_so_a_resend_is_not_wedged() {
    let pool = pool().await;
    let org = fresh_org(&pool, "verdict-claim-finished").await;
    let source_id = source_with(&pool, org, vec![MappingEntry::new("message", "note")], vec![]).await;

    let first = submission(
        org,
        source_id,
        serde_json::json!({ "note": "one" }),
    );
    let submission_id = first.submission_id.clone().expect("the test sends a key");

    let captured = store::capture(&pool, &first).await.expect("the first attempt");
    assert_eq!(captured.lead.status, "rejected");

    // The claim is complete, so the open claim is 0 and a redelivery finds the row.
    let open: i64 = sqlx::query_scalar(
        "select count(*) from crm_lead_submissions
         where source_id = $1 and submission_id = $2 and lead_id is null",
    )
    .bind(source_id)
    .bind(&submission_id)
    .fetch_one(&pool)
    .await
    .expect("the claim rows");
    assert_eq!(
        open, 0,
        "the claim carries the lead id; an open claim here is a wedge on the next resend"
    );

    let second = store::capture(&pool, &first).await.expect("the redelivery answers, not errors");
    assert_eq!(
        second.lead.id, captured.lead.id,
        "one submission, one lead — the rejected row is that lead"
    );

    drop_org(&pool, org).await;
}
