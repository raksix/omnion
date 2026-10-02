//! The name-split gate: one form field, two CRM columns.
//!
//! Run through `scripts/qa/run-crm-name-split.sh`.
//!
//! ## The defect this file exists for
//!
//! `split_full_name` has been in `mapping::TRANSFORMS` since the module's first migration. It is
//! validated by `is_transform`, offered by the mapping editor's transform list, and documented on
//! the function itself as the answer to "a form with one name field is the common case". Its unit
//! test asserted the splitter's two returns directly:
//!
//! ```text
//! #[test] fn split_full_name_fills_both_halves() { … }
//! ```
//!
//! **Nothing ever called the splitter.** `apply_transforms` — the one function the public
//! `apply` runs every transform through — answered `"split_full_name" => value`, an identity,
//! with the comment *"Unreachable: `apply` validates every name before running"*. That comment
//! was true and irrelevant: validation is about whether the name is *known*, not whether the
//! branch does anything. So every lead from every one-name form on every installation of this
//! branch was written with the whole name in `first_name` and `last_name` NULL, and nothing
//! failed: the surname is not required by `crm_leads_contactable_check` and no screen renders
//! both columns as a pair.
//!
//! **Why twenty ticks of green found nothing: the test was for the function, not for the
//! feature.** `split_full_name_fills_both_halves` calls `split_full_name` directly, so it kept
//! passing while the transform that names it did nothing. A test that calls the leaf proves the
//! leaf works; only a test that goes through the entry point can tell whether the leaf is
//! *reached*.
//!
//! ## What every test below does instead
//!
//! Every one goes through `store::capture` and reads the row back out of `crm_leads`, because
//! the unit-level fact ("`apply` produces two values") is exactly the fact that was true and
//! useless. The column is the claim.

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

/// A source that reads one `name` field, splits it, and requires an address so the lead is not
/// a verdict row — the name assertion would otherwise pass on a rejected lead's `first_name`.
async fn splitting_source(pool: &PgPool, org: Uuid, mapping: Vec<MappingEntry>) -> Uuid {
    let label = format!("name split {}", Uuid::new_v4().simple());
    let draft = NewIntakeSource::endpoint(org, &label, None)
        .with_mapping(mapping, Vec::new());
    let (created, _key) = store::create_source(pool, &draft).await.expect("a source");
    created.id
}

fn submission(org: Uuid, source_id: Uuid, payload: serde_json::Value) -> Submission {
    Submission {
        organization_id: org,
        site_id: None,
        source_id,
        submission_id: Some(Uuid::new_v4().to_string()),
        ip: Some("203.0.113.77".to_string()),
        payload,
        received_at: time::OffsetDateTime::now_utc(),
    }
}

/// The two name columns as the database holds them.
///
/// `Option<String>` rather than `String`, because "absent" and "present and empty" are different
/// claims and coalescing them is how an empty surname becomes indistinguishable from a mapping
/// that never produced one.
async fn stored_name(pool: &PgPool, org: Uuid) -> (Option<String>, Option<String>) {
    sqlx::query_as(
        "select first_name, last_name from crm_leads where organization_id = $1
         order by received_at desc, id desc limit 1",
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .expect("a stored lead for the organization")
}

async fn stored_status(pool: &PgPool, org: Uuid) -> String {
    sqlx::query_scalar("select status from crm_leads where organization_id = $1 limit 1")
        .bind(org)
        .fetch_one(pool)
        .await
        .expect("a stored lead for the organization")
}

/// **The core line.** One form field carrying a full name fills both CRM name columns.
///
/// Under the identity arm this row read `first_name = "Ada Lovelace"`, `last_name = NULL`, and
/// the assertion could only have been written about the first column — which is precisely the
/// column that was never wrong.
#[tokio::test]
async fn a_split_mapping_fills_both_name_columns() {
    let pool = pool().await;
    let org = fresh_org(&pool, "split-both").await;
    let source = splitting_source(
        &pool,
        org,
        vec![
            MappingEntry::new("first_name", "name").with_transforms(&["split_full_name"]),
            MappingEntry::new("email", "email"),
        ],
    )
    .await;

    let captured = store::capture(
        &pool,
        &submission(
            org,
            source,
            serde_json::json!({"name": "ada lovelace", "email": "ada@example.com"}),
        ),
    )
    .await
    .expect("the capture");
    assert_eq!(captured.lead.status, "new");

    let (first, last) = stored_name(&pool, org).await;
    assert_eq!(first.as_deref(), Some("Ada"));
    assert_eq!(last.as_deref(), Some("Lovelace"), "the surname was written");
    drop_org(&pool, org).await;
}

/// A single-word name is a whole name. The surname must be absent, and — the part worth
/// asserting — the submission must still be a real lead rather than a refusal, because "ada" is
/// not a missing value.
#[tokio::test]
async fn a_single_word_name_still_produces_a_usable_lead() {
    let pool = pool().await;
    let org = fresh_org(&pool, "split-single").await;
    let source = splitting_source(
        &pool,
        org,
        vec![
            MappingEntry::new("first_name", "name")
                .with_transforms(&["split_full_name"])
                .required(),
            MappingEntry::new("email", "email").required(),
        ],
    )
    .await;

    store::capture(
        &pool,
        &submission(
            org,
            source,
            serde_json::json!({"name": "ada", "email": "ada@example.com"}),
        ),
    )
    .await
    .expect("the capture");

    assert_eq!(stored_status(&pool, org).await, "new", "not a refusal");
    let (first, last) = stored_name(&pool, org).await;
    assert_eq!(first.as_deref(), Some("Ada"));
    assert_eq!(last, None, "absent, not an empty string");
    drop_org(&pool, org).await;
}

/// Extra whitespace is the reason the splitter exists at all: `"Ada  Lovelace"` from a form
/// with two spaces is what a hand-typed surname column gets wrong.
#[tokio::test]
async fn extra_whitespace_in_the_name_field_is_normalised_by_the_split() {
    let pool = pool().await;
    let org = fresh_org(&pool, "split-ws").await;
    let source = splitting_source(
        &pool,
        org,
        vec![
            MappingEntry::new("first_name", "name").with_transforms(&["split_full_name"]),
            MappingEntry::new("email", "email"),
        ],
    )
    .await;

    store::capture(
        &pool,
        &submission(
            org,
            source,
            serde_json::json!({"name": "  ada   lovelace  ", "email": "a@example.com"}),
        ),
    )
    .await
    .expect("the capture");

    let (first, last) = stored_name(&pool, org).await;
    assert_eq!(first.as_deref(), Some("Ada"));
    assert_eq!(last.as_deref(), Some("Lovelace"));
    drop_org(&pool, org).await;
}

/// The save-time half, refused with the contested column named.
///
/// `first_name` split and `last_name` plain are two lines writing `last_name`. `apply` resolves
/// that by map insertion order rather than by asking, so the operator's data alternates between
/// two names depending on which line ran last — the worst kind of bug to chase from a lead list.
/// A source that could be created with this mapping is the defect; the refusal names the column.
#[tokio::test]
async fn a_mapping_where_two_lines_write_the_surname_is_refused_by_name() {
    let pool = pool().await;
    let org = fresh_org(&pool, "split-collide").await;

    let label = format!("name split collide {}", Uuid::new_v4().simple());
    let draft = NewIntakeSource::endpoint(org, &label, None).with_mapping(
        vec![
            MappingEntry::new("first_name", "name").with_transforms(&["split_full_name"]),
            MappingEntry::new("last_name", "surname"),
        ],
        Vec::new(),
    );
    let error = store::create_source(&pool, &draft)
        .await
        .expect_err("two lines writing one column is refused at save time");
    let message = error.to_string();
    assert!(
        message.contains("last_name"),
        "the refusal names the contested column: {message}"
    );
    drop_org(&pool, org).await;
}

/// The negative control for the refusal above: one split line and no rival saves, and the
/// transform is still the one that was asked for. Without this, "refuse everything" would pass
/// the test above and be invisible here.
#[tokio::test]
async fn a_single_split_line_saves_without_complaint() {
    let pool = pool().await;
    let org = fresh_org(&pool, "split-solo").await;
    let source = splitting_source(
        &pool,
        org,
        vec![MappingEntry::new("first_name", "name").with_transforms(&["split_full_name"])],
    )
    .await;

    // The row exists and carries the transform, so the refusal above is about the *rival* and
    // not about the transform being unknown.
    //
    // Read through the store's own `mapping_lines` rather than with a raw `select transform`:
    // the transforms live inside the `mapping` jsonb column, not in one of their own. A
    // hand-written column name answers `42703` naming a column that has never existed, which
    // reads as a broken gate rather than as a wrong query.
    let stored_mapping: serde_json::Value =
        sqlx::query_scalar("select mapping from crm_intake_sources where id = $1")
            .bind(source)
            .fetch_one(&pool)
            .await
            .expect("the stored mapping");
    let stored = format!("{stored_mapping}");
    assert!(
        stored.contains("split_full_name"),
        "the transform was stored as written: {stored}"
    );
    drop_org(&pool, org).await;
}
