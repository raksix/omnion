//! REQ-117, slice 19 — the per-address hourly ceiling, and the column that makes it countable.
//!
//! Run through `scripts/qa/run-crm-address-ceiling.sh`.
//!
//! ## What this file is for
//!
//! `Submission.ip` has carried the doc comment "the submitter's IP, for the per-IP rate limit
//! and the audit trail" since the capture struct shipped. The audit half was real. The
//! per-IP rate limit **did not exist**: `submissions_this_hour` is the only ceiling in the
//! capture path and it counts *by source*, so one address could spend a whole source's hourly
//! budget in seconds, and the single dial an operator had moved the flood and their real
//! traffic together.
//!
//! ## What a gate on this has to measure
//!
//! `capture`, not the counting function — the same rule `run-crm-capture-routing.sh` and
//! `run-crm-assignment.sh` were both written to state. Nine assertions about three correct
//! functions with no production caller were green for twenty-four ticks, and the reason is
//! structural: a gate that begins at the function proves the function. Here the thing under
//! test is *whether a submission is refused*, so every test drives real captures and then
//! reads the **stored rows** back.
//!
//! ## The negative controls carry the weight
//!
//! "Another address is unaffected" is what distinguishes a per-address ceiling from the
//! per-source one that already existed — without it, every assertion here would still pass if
//! the change had tightened the source's own budget. "A submission with no address is never
//! throttled" is what keeps an events-bus delivery from being throttle-able by anybody who
//! can reach the source's endpoint from a host with no parseable address.

use omnion_module_crm_intake::error::CrmIntakeError;
use omnion_module_crm_intake::store::{self, Submission};
use omnion_module_crm_intake::{IntakeSource, MappingEntry, NewIntakeSource, vocabulary};
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

/// A source that maps and requires an e-mail, so every submission below is contactable.
///
/// Without `required()` on the *line* a missing e-mail reads as "not missing" and the verdict
/// is the wrong one for the right reason — see `crm_verdict_rows.rs`, which documents the
/// distinction between the line's flag and the source's `required_targets` list.
fn contact_mapping() -> Vec<MappingEntry> {
    vec![MappingEntry::new("email", "email").required()]
}

async fn endpoint(pool: &PgPool, org: Uuid, label: &str) -> IntakeSource {
    let name = format!("{label} {}", Uuid::new_v4().simple());
    let draft = NewIntakeSource::endpoint(org, &name, None).with_mapping(contact_mapping(), vec![]);
    let (created, _key) = store::create_source(pool, &draft).await.expect("a source");
    created
}

/// A submission for `source` from `address`, with an e-mail that differs per `index`.
///
/// The distinct addresses are load-bearing: with `link` as the policy, a repeated e-mail is
/// judged a *duplicate*, and this gate is about counting deliveries — a fixture whose keys
/// collided would be measuring the dedupe policy instead of the ceiling.
fn submission(source: &IntakeSource, address: &str, index: i64) -> Submission {
    Submission {
        organization_id: source.organization_id,
        site_id: source.site_id,
        source_id: source.id,
        submission_id: Some(format!("{}-{index}", Uuid::new_v4())),
        ip: if address.is_empty() {
            None
        } else {
            Some(address.to_string())
        },
        payload: serde_json::json!({
            "email": format!("visitor-{index}-{}@example.test", source.id),
            "message": format!("enquiry {index}"),
        }),
        received_at: time::OffsetDateTime::now_utc(),
    }
}

/// The host half of an `inet` rendered value, with or without its mask.
fn host_of(value: &str) -> &str {
    value.split('/').next().unwrap_or(value)
}

/// The address the row holds, read as the database holds it.
async fn stored_address(pool: &PgPool, lead: Uuid) -> Option<String> {
    sqlx::query_scalar("select submitter_ip::text from crm_leads where id = $1")
        .bind(lead)
        .fetch_one(pool)
        .await
        .expect("the row is readable")
}

// ---------------------------------------------------------------------------------------------
// The ceiling
// ---------------------------------------------------------------------------------------------

/// **The core line.** One address is refused at the eleventh submission in an hour.
///
/// The loop has to *store* rows rather than merely attempt submissions: a refusal written
/// without a row would not be counted, and the eleventh would sail past — which is exactly
/// the shape of the defect this file exists for.
#[tokio::test]
async fn one_address_is_refused_at_the_eleventh_submission_in_an_hour() {
    let pool = pool().await;
    let org = fresh_org(&pool, "address-ceiling").await;
    let source = endpoint(&pool, org, "ceiling").await;
    let address = "198.51.100.7";

    for index in 0..vocabulary::MAX_SUBMISSIONS_PER_ADDRESS_PER_HOUR {
        store::capture(&pool, &submission(&source, address, index))
            .await
            .unwrap_or_else(|error| panic!("submission {index} should be accepted: {error}"));
    }

    let refused = store::capture(&pool, &submission(&source, address, 99)).await;
    assert!(
        matches!(refused, Err(CrmIntakeError::RateLimited)),
        "the eleventh submission from one address must be refused, got {refused:?}"
    );

    drop_org(&pool, org).await;
}

/// The number is the dial it claims to be, not a limit of one.
///
/// Without this, a "fix" that shipped `MAX = 1` would pass the test above and refuse every
/// second enquiry on a real form.
#[tokio::test]
async fn the_per_address_ceiling_is_tighter_than_but_not_the_sources_own() {
    let pool = pool().await;
    let org = fresh_org(&pool, "address-ceiling-value").await;
    let source = endpoint(&pool, org, "ceiling value").await;
    assert!(
        i64::from(source.rate_limit_per_hour) > vocabulary::MAX_SUBMISSIONS_PER_ADDRESS_PER_HOUR,
        "the per-address ceiling must be the tighter of the two, or it is not the dial it is"
    );
    for index in 0..2 {
        store::capture(&pool, &submission(&source, "198.51.100.8", index))
            .await
            .unwrap_or_else(|error| panic!("submission {index} should be accepted: {error}"));
    }
    drop_org(&pool, org).await;
}

/// **The assertion that names the defect.** A second address shares no budget.
///
/// With only the per-source ceiling this fails: the flood would spend the source's budget and
/// a visitor who had done nothing would be refused for it.
#[tokio::test]
async fn another_address_is_not_counted_against_the_first() {
    let pool = pool().await;
    let org = fresh_org(&pool, "address-neighbour").await;
    let source = endpoint(&pool, org, "neighbour").await;

    for index in 0..vocabulary::MAX_SUBMISSIONS_PER_ADDRESS_PER_HOUR {
        store::capture(&pool, &submission(&source, "198.51.100.9", index))
            .await
            .unwrap_or_else(|error| panic!("submission {index} should be accepted: {error}"));
    }
    store::capture(&pool, &submission(&source, "198.51.100.10", 0))
        .await
        .expect("a different address shares no budget with the first one");
    store::capture(&pool, &submission(&source, "198.51.100.10", 1))
        .await
        .expect("and may send two of its own");

    drop_org(&pool, org).await;
}

/// The count is per *(source, address)*, not per address.
///
/// One visitor posting to two forms is one visitor; a per-address limit keyed on the address
/// alone would refuse their second enquiry because of the first, which is a different and much
/// worse bug than the one being fixed.
#[tokio::test]
async fn a_second_source_has_its_own_budget_for_the_same_visitor() {
    let pool = pool().await;
    let org = fresh_org(&pool, "address-two-sources").await;
    let first = endpoint(&pool, org, "first").await;
    let second = endpoint(&pool, org, "second").await;
    let address = "198.51.100.11";

    for index in 0..vocabulary::MAX_SUBMISSIONS_PER_ADDRESS_PER_HOUR {
        store::capture(&pool, &submission(&first, address, index))
            .await
            .unwrap_or_else(|error| panic!("submission {index} should be accepted: {error}"));
    }
    store::capture(&pool, &submission(&second, address, 0))
        .await
        .expect("a second source has its own budget, even for the same visitor");

    drop_org(&pool, org).await;
}

/// A submission with no address is never throttled here.
///
/// The events-bus path has no HTTP request behind it. Counting "unknown" as its own bucket
/// would make the platform's own delivery pipeline throttle-able by anybody who can reach the
/// source's endpoint from a host whose address does not parse — a self-inflicted outage.
#[tokio::test]
async fn a_submission_with_no_address_is_never_throttled() {
    let pool = pool().await;
    let org = fresh_org(&pool, "address-absent").await;
    let source = endpoint(&pool, org, "absent").await;

    for index in 0..(vocabulary::MAX_SUBMISSIONS_PER_ADDRESS_PER_HOUR * 2) {
        store::capture(&pool, &submission(&source, "", index))
            .await
            .unwrap_or_else(|error| panic!("a submission with no address must be kept: {error}"));
    }
    drop_org(&pool, org).await;
}

/// A malformed address is *no* address, and not a bucket everybody malformed shares.
///
/// Grouping every unparseable value together would let any caller throttle every other
/// malformed caller, so each is treated as absent — and as a bucket nobody else counts in.
#[tokio::test]
async fn an_unparseable_address_is_no_address_rather_than_a_shared_bucket() {
    let pool = pool().await;
    let org = fresh_org(&pool, "address-malformed").await;
    let source = endpoint(&pool, org, "malformed").await;

    for index in 0..(vocabulary::MAX_SUBMISSIONS_PER_ADDRESS_PER_HOUR * 2) {
        let mut submission = submission(&source, "", index);
        submission.ip = Some("not-an-address".to_string());
        store::capture(&pool, &submission)
            .await
            .unwrap_or_else(|error| panic!("an unparseable address must be kept: {error}"));
    }
    drop_org(&pool, org).await;
}

// ---------------------------------------------------------------------------------------------
// The column the ceiling counts
// ---------------------------------------------------------------------------------------------

/// The stored row carries the address the request had.
///
/// The ceiling counts *stored rows*, so the address has to be on the row: it cannot be
/// recovered once the request is gone, and the trail line's `detail->>'ip'` is jsonb a
/// `where` clause cannot index.
#[tokio::test]
async fn the_stored_row_carries_the_address() {
    let pool = pool().await;
    let org = fresh_org(&pool, "address-column").await;
    let source = endpoint(&pool, org, "column").await;

    let captured = store::capture(&pool, &submission(&source, "198.51.100.12", 0))
        .await
        .expect("the submission is accepted");
    // `inet` renders a host address in CIDR form — "198.51.100.12/32" — which
    // `IpAddr::from_str` rejects. So the assertions compare the *host*, taken by `split`,
    // rather than pinning the text: a test that asserted the exact string would be pinning a
    // PostgreSQL formatting decision that has nothing to do with the ceiling, and the first
    // version of this did exactly that and failed on a correct value.
    assert_eq!(
        host_of(
            captured
                .lead
                .submitter_ip
                .as_deref()
                .expect("the returned lead must carry the address the insert wrote")
        ),
        "198.51.100.12"
    );
    assert_eq!(
        host_of(
            stored_address(&pool, captured.lead.id)
                .await
                .expect("the row must be readable and must carry the address too")
                .as_str()
        ),
        "198.51.100.12",
        "and the row must say the same thing the returned lead did"
    );
    drop_org(&pool, org).await;
}

#[tokio::test]
async fn the_column_is_null_when_no_address_was_carried() {
    let pool = pool().await;
    let org = fresh_org(&pool, "address-column-null").await;
    let source = endpoint(&pool, org, "column null").await;

    let captured = store::capture(&pool, &submission(&source, "", 0))
        .await
        .expect("the submission is accepted");
    assert_eq!(captured.lead.submitter_ip, None);
    assert_eq!(
        stored_address(&pool, captured.lead.id).await,
        None,
        "no address is null, not an empty string"
    );
    drop_org(&pool, org).await;
}

/// The counter and the writer must be asking the same question.
#[tokio::test]
async fn the_counter_reads_the_rows_the_insert_writes() {
    let pool = pool().await;
    let org = fresh_org(&pool, "address-counter").await;
    let source = endpoint(&pool, org, "counter").await;
    let address = "198.51.100.13";

    for index in 0..3 {
        store::capture(&pool, &submission(&source, address, index))
            .await
            .expect("the submission is accepted");
    }
    assert_eq!(
        store::submissions_from_address_this_hour(&pool, source.id, Some(address))
            .await
            .expect("the count answers"),
        3
    );
    assert_eq!(
        store::submissions_from_address_this_hour(&pool, source.id, Some("198.51.100.99"))
            .await
            .expect("the count answers"),
        0,
        "an address that has sent nothing counts zero, not its nearest neighbour"
    );
    assert_eq!(
        store::submissions_from_address_this_hour(&pool, source.id, None)
            .await
            .expect("the count answers"),
        0,
        "no address is no bucket, not the busiest one"
    );
    drop_org(&pool, org).await;
}

/// A deleted row stops counting.
///
/// The reason this counts rows rather than a counter column: a counter incremented on the
/// way in and never decremented eventually refuses a source nobody is using, and the operator
/// has nothing to look at that would explain it.
#[tokio::test]
async fn a_deleted_lead_stops_counting_against_the_visitor() {
    let pool = pool().await;
    let org = fresh_org(&pool, "address-deleted").await;
    let source = endpoint(&pool, org, "deleted").await;
    let address = "198.51.100.14";

    let captured = store::capture(&pool, &submission(&source, address, 0))
        .await
        .expect("the submission is accepted");
    assert_eq!(
        store::submissions_from_address_this_hour(&pool, source.id, Some(address))
            .await
            .expect("the count answers"),
        1
    );
    assert!(
        store::delete_lead(&pool, org, captured.lead.id)
            .await
            .expect("the delete answers"),
        "the test's own lead must be deletable"
    );
    assert_eq!(
        store::submissions_from_address_this_hour(&pool, source.id, Some(address))
            .await
            .expect("the count answers"),
        0,
        "a deleted lead is not evidence against the visitor"
    );
    drop_org(&pool, org).await;
}
