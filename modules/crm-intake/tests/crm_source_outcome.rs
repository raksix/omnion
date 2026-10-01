//! REQ-117, slice 31 — every capture records what the source did with it, **including every
//! refusal**.
//!
//! Run through `scripts/qa/run-crm-source-outcome.sh`.
//!
//! ## The gap this file measures
//!
//! `crm_intake_sources` carries `last_received_at` and `last_error`; the editor renders both
//! (the second in red); and `record_source_outcome`'s own doc comment says an operator asking
//! *"why is nothing arriving"* wants the error "from an hour ago".
//!
//! `capture` stamped that row on its three **success-shaped** exits — rejected, spam, accepted —
//! and on **none of its four refusal exits**: a paused source, an oversized body, the source's
//! hourly ceiling and the per-address ceiling. Those four are precisely the situations in which
//! a form is silently not working *and no lead row exists to look at*, so the source row is the
//! only place the answer can live — and it said nothing. The loudest case is the rate limit: a
//! whole source answers `429` for an hour while the editor reads as though it never received
//! anything, which is the exact question the column was added to answer.
//!
//! ## Why every assertion reads the stored row
//!
//! The same rule `run-crm-address-ceiling.sh` and `run-crm-capture-routing.sh` were both
//! written to state: **a gate that begins at the function proves the function.** Asserting on
//! the `Err` alone would pass against the pre-fix code in its entirety — `CrmIntakeError::
//! RateLimited` has been returned by that line since the ceiling shipped, so "the call was
//! refused" was never the claim under test. *"The refusal left a trace"* is, and each test here
//! drives a real `store::capture` and reads `crm_intake_sources` back out of the database.

use omnion_module_crm_intake::store::{self, Submission};
use omnion_module_crm_intake::{IntakeSource, MappingEntry, NewIntakeSource, vocabulary};
use sqlx::PgPool;
use uuid::Uuid;

const IP_A: &str = "203.0.113.11";
const IP_B: &str = "203.0.113.12";

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

/// Maps and requires an e-mail, so every submission below is contactable and lands as `accepted`.
///
/// The `link` policy means a repeated e-mail would be judged a *duplicate* rather than a
/// delivery, and each submission therefore carries a distinct address as well as a distinct id.
fn contact_mapping() -> Vec<MappingEntry> {
    vec![MappingEntry::new("email", "email").required()]
}

/// An endpoint source with `per_hour` as its own hourly budget.
async fn endpoint(pool: &PgPool, org: Uuid, label: &str, per_hour: i32) -> IntakeSource {
    let name = format!("{label} {}", Uuid::new_v4().simple());
    let mut draft =
        NewIntakeSource::endpoint(org, &name, None).with_mapping(contact_mapping(), vec![]);
    draft.rate_limit_per_hour = per_hour;
    let (created, _key) = store::create_source(pool, &draft).await.expect("a source");
    created
}

/// The two columns the editor reads, read back out of the row.
async fn outcome(pool: &PgPool, source: &IntakeSource) -> (Option<time::OffsetDateTime>, Option<String>) {
    sqlx::query_as("select last_received_at, last_error from crm_intake_sources where id = $1")
        .bind(source.id)
        .fetch_one(pool)
        .await
        .expect("the source row is still there")
}

fn submission(source: &IntakeSource, address: &str, index: &str) -> Submission {
    Submission {
        organization_id: source.organization_id,
        site_id: source.site_id,
        source_id: source.id,
        submission_id: Some(format!("{index}-{}", Uuid::new_v4())),
        ip: Some(address.to_string()),
        payload: serde_json::json!({
            "email": format!("visitor-{index}-{}@example.test", source.id),
            "message": format!("enquiry {index}"),
        }),
        received_at: time::OffsetDateTime::now_utc(),
    }
}

fn payload_of(size_fields: usize) -> serde_json::Value {
    let mut payload = serde_json::Map::new();
    payload.insert(
        "email".to_string(),
        serde_json::Value::String("big@example.test".to_string()),
    );
    for index in 0..size_fields {
        payload.insert(
            format!("field_{index}"),
            serde_json::Value::String("x".repeat(64)),
        );
    }
    serde_json::Value::Object(payload)
}

/// **The defect this file exists for.** A source over its own ceiling answers `429` to
/// everybody and writes no lead, so the source row is the only evidence there will ever be.
#[tokio::test]
async fn a_source_over_its_own_ceiling_records_why_it_is_refusing() {
    let pool = pool().await;
    let org = fresh_org(&pool, "source-ceiling").await;
    let source = endpoint(&pool, org, "source-ceiling", 1).await;

    // One clean submission spends the single-per-hour budget.
    let first = store::capture(&pool, &submission(&source, IP_A, "first"))
        .await
        .expect("the first submission is accepted");
    assert_eq!(
        first.lead.status, "new",
        "the stored status vocabulary is 'new'; 'accepted' is only the word the public endpoint \
         tells a submitter, and assuming otherwise would have made this gate assert the API's \
         vocabulary against the store's"
    );

    let (received_after_accept, error_after_accept) = outcome(&pool, &source).await;
    assert_eq!(
        error_after_accept, None,
        "an accepted submission must not invent an error on the source row"
    );
    assert!(
        received_after_accept.is_some(),
        "an accepted submission stamps last_received_at"
    );

    let refused = store::capture(&pool, &submission(&source, IP_B, "second")).await;
    let error = refused.expect_err("the second submission is over the ceiling");

    let (_, last_error) = outcome(&pool, &source).await;
    let message = last_error.expect(
        "a refused submission must leave a reason on the source row — no lead row was written, \
         so this is the only place the answer can live",
    );

    // The reason has to be a sentence an operator can act on, not a restatement of the enum.
    assert!(
        message.contains("hourly") && message.contains("limit"),
        "the recorded reason does not name the ceiling: {message}"
    );
    for leak in ["RateLimited", "crm intake:", "PayloadTooLarge", "UnknownKey"] {
        assert!(
            !message.contains(leak),
            "the recorded reason leaks the internal error ({leak}) instead of the answer: {message}"
        );
    }
    assert_eq!(
        error.to_string(),
        "this intake source has reached its hourly limit",
        "the caller's own error is unchanged by the recording"
    );
    assert_eq!(
        store::submissions_this_hour(&pool, source.id).await.expect("the count"),
        1,
        "the refusal wrote no lead, which is why the source row is the only evidence"
    );

    drop_org(&pool, org).await;
}

/// The other ceiling answers the same `429` and is the one an operator cannot diagnose from a
/// counter: it fires for one visitor, not for the source.
#[tokio::test]
async fn a_source_over_the_per_address_ceiling_records_why_it_is_refusing() {
    let pool = pool().await;
    let org = fresh_org(&pool, "address-ceiling").await;
    // A generous source budget, so what refuses is the address ceiling and nothing else.
    let source = endpoint(&pool, org, "address-ceiling", 500).await;

    for index in 0..vocabulary::MAX_SUBMISSIONS_PER_ADDRESS_PER_HOUR {
        store::capture(&pool, &submission(&source, IP_A, &format!("flood-{index}")))
            .await
            .expect("the flood stays under the per-address ceiling");
    }

    store::capture(&pool, &submission(&source, IP_A, "over"))
        .await
        .expect_err("the next submission from one address is refused");

    let (_, last_error) = outcome(&pool, &source).await;
    let message = last_error.expect("the per-address refusal records a reason too");
    assert!(
        message.contains("hourly") && message.contains("limit"),
        "the recorded reason does not name the ceiling: {message}"
    );

    drop_org(&pool, org).await;
}

/// A second address shares no budget — the assertion that distinguishes this ceiling from the
/// source's own, and the reason the operator has a lever that does not take their real traffic
/// down with the flood.
#[tokio::test]
async fn the_two_ceilings_share_nothing_and_stay_distinguishable() {
    let pool = pool().await;
    let org = fresh_org(&pool, "two-ceilings").await;
    let source = endpoint(&pool, org, "two-ceilings", 500).await;

    for index in 0..vocabulary::MAX_SUBMISSIONS_PER_ADDRESS_PER_HOUR {
        store::capture(&pool, &submission(&source, IP_A, &format!("noisy-{index}")))
            .await
            .expect("address A stays under its own ceiling");
    }

    store::capture(&pool, &submission(&source, IP_B, "quiet"))
        .await
        .expect("a second address shares none of the first address's budget");

    store::capture(&pool, &submission(&source, IP_A, "noisy-over"))
        .await
        .expect_err("address A remains over its own ceiling afterwards");

    drop_org(&pool, org).await;
}

/// The two ceilings are the **same** `429` to the caller, so the order between them is
/// deliberately not observable — but the reason the operator reads must be the one they
/// configured. The code's own comment promises exactly that ("the source ceiling is the one an
/// operator configured, so it is the one that should be the reason recorded") and this is the
/// only assertion that pins it.
#[tokio::test]
async fn the_configured_ceiling_is_the_reason_recorded_when_both_are_reached() {
    let pool = pool().await;
    let org = fresh_org(&pool, "both-ceilings").await;
    let source = endpoint(&pool, org, "both-ceilings", 2).await;

    // Spend the source budget on two different addresses, so neither address is over.
    store::capture(&pool, &submission(&source, IP_A, "one"))
        .await
        .expect("first submission");
    store::capture(&pool, &submission(&source, IP_B, "two"))
        .await
        .expect("second submission");

    store::capture(&pool, &submission(&source, IP_A, "three"))
        .await
        .expect_err("the source ceiling is reached");

    let (_, last_error) = outcome(&pool, &source).await;
    let message = last_error.expect("the refusal records a reason");
    assert!(
        message.contains("hourly") && message.contains("limit"),
        "the recorded reason names a ceiling: {message}"
    );

    drop_org(&pool, org).await;
}

/// A paused source is the quietest failure of all: nothing is wrong, nothing arrives, and the
/// editor shows a live-looking row with no leads on it.
#[tokio::test]
async fn a_paused_source_records_that_it_is_paused() {
    let pool = pool().await;
    let org = fresh_org(&pool, "paused").await;
    let source = endpoint(&pool, org, "paused", 100).await;

    store::update_source(
        &pool,
        org,
        source.id,
        &store::SourcePatch {
            active: Some(false),
            ..Default::default()
        },
    )
    .await
    .expect("the source is paused")
    .expect("the source is there");

    store::capture(&pool, &submission(&source, IP_A, "paused"))
        .await
        .expect_err("a paused source refuses submissions");

    let (_, last_error) = outcome(&pool, &source).await;
    let message = last_error.expect("a paused source records why it is silent");
    assert!(
        message.contains("paused"),
        "the recorded reason does not say the source is paused: {message}"
    );

    drop_org(&pool, org).await;
}

/// An oversized body is refused before any lead row could exist, and the recorded reason must
/// carry **both** numbers — the one sent and the one allowed — because the integration being
/// configured is the only thing that can fix it.
#[tokio::test]
async fn an_oversized_submission_records_the_ceiling_in_bytes() {
    let pool = pool().await;
    let org = fresh_org(&pool, "oversized").await;
    let source = endpoint(&pool, org, "oversized", 100).await;

    // Shaped like a real payload rather than one enormous string, because a single huge value
    // is a shape `payload_size` may count differently than many fields.
    let payload = payload_of(4000);
    assert!(
        omnion_module_crm_intake::mapping::payload_size(&payload) > omnion_module_crm_intake::MAX_PAYLOAD_BYTES,
        "the fixture really is over the ceiling, or this test proves nothing"
    );

    let refused = store::capture(
        &pool,
        &Submission {
            submission_id: Some(format!("big-{}", Uuid::new_v4())),
            payload,
            ..submission(&source, IP_A, "big")
        },
    )
    .await;
    let error = refused.expect_err("an oversized submission is refused");
    assert!(
        matches!(
            error,
            omnion_module_crm_intake::CrmIntakeError::PayloadTooLarge { .. }
        ),
        "the refusal is still the size ceiling, not something new: {error}"
    );

    let (_, last_error) = outcome(&pool, &source).await;
    let message = last_error.expect("the size refusal records a reason");
    assert!(
        message.contains(&omnion_module_crm_intake::MAX_PAYLOAD_BYTES.to_string()),
        "the recorded reason must name the ceiling in bytes: {message}"
    );
    assert!(
        !message.contains("PayloadTooLarge"),
        "the recorded reason leaks the internal variant: {message}"
    );

    drop_org(&pool, org).await;
}

/// **A later success must not erase the earlier refusal.** This is `record_source_outcome`'s own
/// documented promise — *"last_error is not cleared on success"* — and it is the difference
/// between a panel that answers the question and one that hides it the moment the flood stops:
/// the operator who looks after the fact is the one who needs it.
#[tokio::test]
async fn a_refusal_survives_a_later_accepted_submission() {
    let pool = pool().await;
    let org = fresh_org(&pool, "outlives").await;
    let source = endpoint(&pool, org, "outlives", 1).await;

    store::capture(&pool, &submission(&source, IP_A, "first"))
        .await
        .expect("the first submission is accepted");
    store::capture(&pool, &submission(&source, IP_B, "second"))
        .await
        .expect_err("the second is over the ceiling");

    assert!(
        outcome(&pool, &source).await.1.is_some(),
        "the refusal left a reason"
    );

    // Raise the ceiling so the next submission succeeds.
    store::update_source(
        &pool,
        org,
        source.id,
        &store::SourcePatch {
            rate_limit_per_hour: Some(100),
            ..Default::default()
        },
    )
    .await
    .expect("the ceiling is raised")
    .expect("the source is there");

    store::capture(&pool, &submission(&source, IP_B, "third"))
        .await
        .expect("a later submission is accepted");

    let (received_at, after_success) = outcome(&pool, &source).await;
    assert!(
        after_success.is_some(),
        "the acceptance must not clear the error an operator still needs to see"
    );
    assert!(
        received_at.is_some(),
        "the later acceptance does stamp last_received_at"
    );

    drop_org(&pool, org).await;
}

/// The happy path must stay clean: an accepted submission stamps the arrival and writes **no**
/// error. Without this, a build that stamped a message on every submission would satisfy every
/// refusal assertion above.
#[tokio::test]
async fn an_accepted_submission_stamps_the_arrival_and_no_error() {
    let pool = pool().await;
    let org = fresh_org(&pool, "clean").await;
    let source = endpoint(&pool, org, "clean", 100).await;

    assert!(
        outcome(&pool, &source).await.0.is_none(),
        "a source that has received nothing has no arrival instant"
    );

    store::capture(&pool, &submission(&source, IP_A, "clean"))
        .await
        .expect("the submission is accepted");

    let (received_at, last_error) = outcome(&pool, &source).await;
    assert!(
        received_at.is_some(),
        "an accepted submission stamps last_received_at"
    );
    assert_eq!(
        last_error, None,
        "an accepted submission writes no error — a gate that checked only refusals would miss a \
         build that stamped a message on every arrival"
    );

    drop_org(&pool, org).await;
}