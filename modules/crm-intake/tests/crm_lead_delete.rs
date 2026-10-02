//! REQ-117, slice 19b — the second defect: a lead cannot be deleted.
//!
//! Run through `scripts/qa/run-crm-lead-delete.sh`.
//!
//! ## The defect, and why it is not this slice's fault
//!
//! Found by `crm_address_ceiling.rs::a_deleted_lead_stops_counting_against_the_visitor`, which
//! deletes a lead on purpose: a counter that is incremented on the way in and never
//! decremented is a rate limit that eventually refuses a source nobody is using, so the
//! deletion is the behaviour under test. It raised
//!
//! ```text
//! 23514 new row for relation "crm_lead_submissions" violates check constraint
//!       "crm_lead_submissions_completion"
//! ```
//!
//! **Nothing to do with the address.** The two constraints on that table are
//!
//! ```sql
//! constraint crm_lead_submissions_completion
//!     check ((lead_id is null) = (completed_at is null))
//! constraint crm_lead_submissions_lead_id_fkey
//!     foreign key (lead_id) references crm_leads (id) on delete set null
//! ```
//!
//! and they contradict each other for exactly one case. `capture` finishes a claim by writing
//! **both** `lead_id` and `completed_at`, so a completed claim is a row with neither null. Now
//! delete the lead it points at: the `on delete set null` fires, `lead_id` becomes NULL,
//! `completed_at` stays set — and `(lead_id is null) = (completed_at is null)` is
//! `true = false`, so the check refuses the row PostgreSQL is trying to create. The FK's
//! action is not a bad intention; the check is asserting an invariant the FK is built to
//! break.
//!
//! This is a **deleting a lead raises a 500**, on the one route an operator reaches for when a
//! visitor asked to be removed, and it is the route REQ-117 acceptance 15 promises. It sat
//! here because every gate that ever deleted a lead deleted one whose submission had **no**
//! claim row — a keyed endpoint with no `x-idempotency-key` never writes one, and the gates
//! that do write claims assert on the lead rather than removing it.
//!
//! ## The fix is in the migration, not the store
//!
//! The obvious repair — complete the claim in `delete_lead` before deleting — is a second
//! statement in a function whose contract is "delete one row", and it would leave the same
//! contradiction for a *rejected* claim (a claim that was taken and completed for a verdict
//! row has no lead at all, so there is nothing to null). The check is the thing that is wrong:
//! "a completed claim names a lead" is not an invariant this table can keep, because the lead
//! it names is deletable. Migration `0193` narrows the check to the direction that *is*
//! enforceable: an open claim may carry a completion instant only if it also names a lead.

use omnion_module_crm_intake::store::{self, Submission};
use omnion_module_crm_intake::{IntakeSource, MappingEntry, NewIntakeSource};
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

async fn endpoint(pool: &PgPool, org: Uuid) -> IntakeSource {
    let name = format!("lead delete {}", Uuid::new_v4().simple());
    let draft = NewIntakeSource::endpoint(org, &name, None)
        .with_mapping(vec![MappingEntry::new("email", "email").required()], vec![]);
    let (created, _key) = store::create_source(pool, &draft).await.expect("a source");
    created
}

fn submission(source: &IntakeSource, index: i64) -> Submission {
    Submission {
        organization_id: source.organization_id,
        site_id: source.site_id,
        source_id: source.id,
        // **A submission id, so a claim row is written.** This is the whole difference
        // between reproducing the defect and reproducing its neighbour: without one, `capture`
        // takes no claim and the delete succeeds, which is why every earlier gate passed.
        submission_id: Some(format!("claim-{}", Uuid::new_v4())),
        ip: Some("198.51.100.21".to_string()),
        payload: serde_json::json!({
            "email": format!("deleter-{index}-{}@example.test", source.id),
            "message": "please remove me",
        }),
        received_at: time::OffsetDateTime::now_utc(),
    }
}

/// **The defect.** Deleting a lead whose submission wrote a claim raises `23514`.
#[tokio::test]
async fn a_lead_with_a_submission_claim_can_be_deleted() {
    let pool = pool().await;
    let org = fresh_org(&pool, "lead-delete-claimed").await;
    let source = endpoint(&pool, org).await;

    let captured = store::capture(&pool, &submission(&source, 0))
        .await
        .expect("the submission is accepted");

    // The claim is what makes this the interesting case, so it is asserted rather than assumed.
    let completed: Option<time::OffsetDateTime> = sqlx::query_scalar(
        "select completed_at from crm_lead_submissions \
         where source_id = $1 and lead_id = $2",
    )
    .bind(source.id)
    .bind(captured.lead.id)
    .fetch_optional(&pool)
    .await
    .expect("the claim is readable");
    assert!(
        completed.is_some(),
        "capture must have completed this claim, or the test is not exercising the case it names"
    );

    assert!(
        store::delete_lead(&pool, org, captured.lead.id)
            .await
            .expect("a visitor asked to be removed; the delete must answer"),
        "deleting the lead must report that it deleted it"
    );
    assert_eq!(
        store::find_lead(&pool, org, captured.lead.id)
            .await
            .expect("the read answers"),
        None,
        "and the lead must be gone"
    );

    drop_org(&pool, org).await;
}

/// The claim goes with the lead — no durable trace of a removed visitor.
///
/// The alternative repair (narrow the completion check so a claim may name no lead) was
/// written first and is wrong: it leaves the claim row behind, so a visitor who asked to be
/// removed still has a row in the platform keyed by their submission id. The claim is
/// "one submission, one lead" bookkeeping, and with the lead gone it has nothing left to
/// deduplicate against.
#[tokio::test]
async fn the_claim_is_deleted_with_the_lead() {
    let pool = pool().await;
    let org = fresh_org(&pool, "lead-delete-reclaim").await;
    let source = endpoint(&pool, org).await;
    let submission = submission(&source, 0);

    let captured = store::capture(&pool, &submission)
        .await
        .expect("the submission is accepted");
    store::delete_lead(&pool, org, captured.lead.id)
        .await
        .expect("the delete answers");

    let remaining: Option<(Option<Uuid>, Option<time::OffsetDateTime>)> = sqlx::query_as(
        "select lead_id, completed_at from crm_lead_submissions \
         where source_id = $1 and submission_id = $2",
    )
    .bind(source.id)
    .bind(submission.submission_id.as_deref().expect("a submission id"))
    .fetch_optional(&pool)
    .await
    .expect("the read answers");
    assert_eq!(
        remaining, None,
        "the claim must be gone with the lead, not re-opened: a removed visitor should leave \
         no row keyed by their submission"
    );

    drop_org(&pool, org).await;
}

/// A redelivery of a deleted lead's submission captures again — and that is correct.
///
/// **This is what pins the "cascade" decision over the "narrow the check" one.** Both make
/// the delete succeed and both let a redelivery capture, so neither is distinguished by the
/// previous test alone; the difference is the row that survives, and this test says the row
/// must not. A claim left behind would also make the *next* delivery of the same submission
/// answer "already captured" for ever if the check were ever re-tightened — so the two
/// decisions are not independent, which is why the test is here and not in a comment.
#[tokio::test]
async fn a_redelivery_after_the_deletion_captures_again() {
    let pool = pool().await;
    let org = fresh_org(&pool, "lead-delete-redeliver").await;
    let source = endpoint(&pool, org).await;
    let submission = submission(&source, 0);

    let first = store::capture(&pool, &submission)
        .await
        .expect("the submission is accepted");
    store::delete_lead(&pool, org, first.lead.id)
        .await
        .expect("the delete answers");

    let again = store::capture(&pool, &submission)
        .await
        .expect("a redelivery after the operator removed the result must be captured again");
    assert_ne!(
        again.lead.id, first.lead.id,
        "the redelivery is a new lead, not the deleted one"
    );
    assert!(
        store::delete_lead(&pool, org, again.lead.id)
            .await
            .expect("and the new lead must be deletable too — the fix is not one-shot"),
        "the second delete must succeed, or the first was only ever a fluke"
    );

    drop_org(&pool, org).await;
}

/// A lead whose submission wrote no claim is unaffected — the control for the two above.
///
/// Without this, a fix that simply completed every claim on delete would pass both tests
/// above and break the "one submission, one lead" guarantee for a redelivery.
#[tokio::test]
async fn a_lead_without_a_claim_is_deleted_unchanged() {
    let pool = pool().await;
    let org = fresh_org(&pool, "lead-delete-plain").await;
    let source = endpoint(&pool, org).await;

    let mut submission = submission(&source, 0);
    // No id ⇒ no claim: the keyed endpoint's default when a browser resends a post.
    submission.submission_id = None;
    let captured = store::capture(&pool, &submission)
        .await
        .expect("the submission is accepted");
    assert!(
        store::delete_lead(&pool, org, captured.lead.id)
            .await
            .expect("the delete answers"),
        "a lead nobody can replay must still be deletable"
    );

    drop_org(&pool, org).await;
}
