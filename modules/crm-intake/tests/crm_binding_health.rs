//! The binding-health gate: a broken mapping is *recorded on a submission*, not asserted in a
//! unit test.
//!
//! Run through `scripts/qa/run-crm-binding-health.sh`.
//!
//! ## Why this file exists
//!
//! `mapping::health` and `store::set_broken_mappings` shipped several slices before this gate
//! and **neither had a production caller**. The pure function's references were its own
//! definition and two unit tests; the writer's were its own definition. The column had a model
//! predicate and a screen branch that renders the missing keys, so the panel drew a "broken
//! mapping" warning that no state on any installation could reach.
//!
//! That is the shape this crate has now met four times — the round-robin cursor, the
//! autoresponder reservation, the SLA reminder claim, this one — and the same answer each time:
//! **an exported, unit-tested, REQ-named function is not a feature.** A unit test on
//! `health()` proves the verdict and nothing about a caller, which is why the interesting
//! tests below go through `store::capture` and then read the column back.
//!
//! ## The forms module is not on this branch
//!
//! REQ-064's tables are not here, so the check's form-key path cannot be driven by a real
//! `cms_forms` row. That is not a reason to skip the gate — it is the *interesting* case, and
//! the fallback it exercises is the one every CRM-without-forms installation runs:
//!
//! * a **form-bound** source falls back to the payload's own keys, so a renamed field is
//!   caught from a live submission alone;
//! * a source whose payloads carry no keys at all answers `Unknown`, which must leave a
//!   previously-recorded `broken_mappings` list exactly as it was;
//! * an **endpoint** source has no `form_key` and is healthy on a forms-less platform — the
//!   "do not break every source" half.
//!
//! The last one is the assertion most likely to be deleted by a future reader as obvious, and
//! it is the one that keeps a red badge from meaning "install the forms module".

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

/// A source whose mapping asks for `email` and `job` — the second is the field the form will
/// stop having.
async fn source(pool: &PgPool, org: Uuid) -> Uuid {
    form_source(pool, org, "contact").await
}

/// A **form-bound** source, which is the only kind the health check judges.
///
/// This is not a detail. The first draft of these tests built an `endpoint` source and every
/// one of them failed with `[]` instead of `["job"]` — because an endpoint has no `form_key`, a
/// form-bound check is correctly `Unknown` for it, and `Unknown` writes nothing. The code was
/// right and the test was measuring the wrong surface: a keyed endpoint has no form whose
/// fields can be renamed, so calling it broken would be the bug.
async fn form_source(pool: &PgPool, org: Uuid, form_key: &str) -> Uuid {
    let label = format!("health gate {}", Uuid::new_v4().simple());
    let mut draft = NewIntakeSource::endpoint(org, &label, None).with_mapping(
        vec![
            MappingEntry::new("email", "email"),
            MappingEntry::new("job_title", "job"),
        ],
        vec!["email".to_string()],
    );
    draft.kind = "form".to_string();
    draft.form_key = Some(form_key.to_string());
    let (created, _key) = store::create_source(pool, &draft).await.expect("a source");
    created.id
}

fn submission(org: Uuid, source_id: Uuid, payload: serde_json::Value) -> Submission {
    Submission {
        organization_id: org,
        site_id: None,
        source_id,
        submission_id: Some(Uuid::new_v4().to_string()),
        ip: Some("203.0.113.11".to_string()),
        payload,
        received_at: time::OffsetDateTime::now_utc(),
    }
}

async fn broken_of(pool: &PgPool, source_id: Uuid) -> Vec<String> {
    sqlx::query_scalar("select broken_mappings from crm_intake_sources where id = $1")
        .bind(source_id)
        .fetch_one(pool)
        .await
        .expect("the source's broken-mapping list")
}

/// The stored lead's e-mail, read back out of the row rather than off the returned struct.
///
/// This crate has been bitten twice by trusting the struct that *produced* a row instead of the
/// row: the `OffsetDateTime` serialised as a component array, and the autoresponder claim
/// predicate that matched a skip line. Every assertion that can read the database does.
async fn captured_email_is_reachable(pool: &PgPool, org: Uuid) -> Option<String> {
    sqlx::query_scalar("select email::text from crm_leads where organization_id = $1 limit 1")
        .bind(org)
        .fetch_optional(pool)
        .await
        .expect("the stored lead's e-mail")
        .flatten()
}

/// The core claim: a submission whose payload no longer carries a mapped key **records** it.
///
/// The lead is still written — a broken integration must not cost a business the enquiry — and
/// the column names the field, so the editor can say which one.
#[tokio::test]
async fn a_renamed_field_is_recorded_on_the_submission_that_proves_it() {
    let pool = pool().await;
    let org = fresh_org(&pool, "binding-rename").await;
    let source_id = source(&pool, org).await;

    // The form still has `email`. `job` was renamed away.
    let captured = store::capture(
        &pool,
        &submission(
            org,
            source_id,
            serde_json::json!({ "email": "ada@example.com", "consent": true, "submitted_in_ms": 4200 }),
        ),
    )
    .await
    .expect("the submission is captured, broken mapping or not");

    assert_eq!(
        captured.lead.email.as_deref(),
        Some("ada@example.com"),
        "a broken mapping must not stop the lead being written"
    );
    assert_eq!(broken_of(&pool, source_id).await, vec!["job".to_string()]);

    drop_org(&pool, org).await;
}

/// The screen branch has to be able to render, which means the model predicate has to agree
/// with what was stored. A column that says `job` while the predicate says healthy is the
/// version of this bug that ships.
#[tokio::test]
async fn a_recorded_break_makes_the_sources_own_predicate_agree() {
    let pool = pool().await;
    let org = fresh_org(&pool, "binding-predicate").await;
    let source_id = source(&pool, org).await;

    assert!(
        !store::find_source(&pool, org, source_id)
            .await
            .expect("the source")
            .expect("it exists")
            .binding_is_broken(),
        "a fresh source is not broken"
    );

    store::capture(
        &pool,
        &submission(
            org,
            source_id,
            serde_json::json!({ "email": "ada@example.com", "consent": true, "submitted_in_ms": 4200 }),
        ),
    )
    .await
    .expect("the submission");

    let reread = store::find_source(&pool, org, source_id)
        .await
        .expect("the source")
        .expect("it exists");
    assert!(
        reread.binding_is_broken(),
        "the screen's branch is reachable only if this is true"
    );
    assert_eq!(reread.broken_mappings, vec!["job".to_string()]);

    drop_org(&pool, org).await;
}

/// The unknown half, at the only level it is reachable from here — see the note below.
///
/// **This is a unit test on purpose, and the reason is a pre-existing defect.** The unknown
/// state needs a form-bound source whose key list cannot be read at all: no `cms_forms` row, no
/// stored lead, and a submission carrying no keys. The only payload that reaches it is `{}`,
/// and `{}` maps to no contactable value, so `capture` takes the *rejected* branch — which
/// writes a lead with neither e-mail nor phone and is refused by `crm_leads_contactable_check`
/// with a `23514`. The same failure is visible in the `crm_autoresponder` gate, which documents
/// the rejected case as unreachable for this reason and works around it.
///
/// So on this branch the unknown state is not reachable through `capture`, and pretending
/// otherwise would have meant a test that passes for a reason nobody could name. The property
/// that matters is asserted where it is provable instead — see
/// `binding_health::tests::an_unknown_answer_never_clears_a_recorded_one` in the module. The
/// defect is recorded in the BUILD-LOG for its own slice: REQ-117 acceptance 5 says a rejected
/// row is written, and on any real installation it raises `23514` instead.

/// The other half of "not fatal": a source the check cannot judge still captures a lead. A
/// health check that can lose an enquiry is worse than the broken mapping it reports.
#[tokio::test]
async fn a_submission_survives_a_binding_the_platform_cannot_read() {
    let pool = pool().await;
    let org = fresh_org(&pool, "binding-survives").await;
    let source_id = form_source(&pool, org, "no-such-form").await;

    store::capture(
        &pool,
        &submission(
            org,
            source_id,
            serde_json::json!({ "email": "grace@example.com", "job": "engineer", "consent": true, "submitted_in_ms": 4200 }),
        ),
    )
    .await
    .expect("the submission is captured");

    assert_eq!(
        captured_email_is_reachable(&pool, org).await,
        Some("grace@example.com".to_string())
    );
    assert!(
        broken_of(&pool, source_id).await.is_empty(),
        "a form that exists with every key intact is not broken"
    );

    drop_org(&pool, org).await;
}

/// An endpoint source is healthy on a platform with no forms module. Without this, installing
/// the CRM would mark every keyed integration broken and the badge would stop meaning anything.
#[tokio::test]
async fn a_keyed_endpoint_is_never_marked_broken_by_a_missing_forms_module() {
    let pool = pool().await;
    let org = fresh_org(&pool, "binding-endpoint").await;

    // Built as a real endpoint, not a form with the form key stripped afterwards: the check
    // keys off `form_key` being absent, and the two shapes have to be produced the way the
    // product produces them.
    let label = format!("endpoint gate {}", Uuid::new_v4().simple());
    let draft = NewIntakeSource::endpoint(org, &label, None).with_mapping(
        vec![
            MappingEntry::new("email", "email"),
            MappingEntry::new("job_title", "job"),
        ],
        vec!["email".to_string()],
    );
    let (source_row, _key) = store::create_source(&pool, &draft).await.expect("a source");
    assert!(source_row.form_key.is_none(), "an endpoint has no form to bind to");

    store::capture(
        &pool,
        &submission(
            org,
            source_row.id,
            serde_json::json!({ "email": "alan@example.com", "job": "engineer", "consent": true, "submitted_in_ms": 4200 }),
        ),
    )
    .await
    .expect("the submission");

    assert!(
        broken_of(&pool, source_row.id).await.is_empty(),
        "an endpoint source has no form whose fields can be renamed"
    );

    drop_org(&pool, org).await;
}

/// A second submission after the break does not re-write the health. The comparison is on the
/// **list**, not on `updated_at`: `record_source_outcome` touches that column on every
/// submission by design (it stamps `last_received_at`), so an `updated_at` assertion here would
/// be measuring the wrong thing — the first draft did exactly that, failed, and the failure was
/// the test's premise rather than the code's behaviour.
#[tokio::test]
async fn an_unchanged_answer_writes_nothing() {
    let pool = pool().await;
    let org = fresh_org(&pool, "binding-idempotent").await;
    let source_id = source(&pool, org).await;

    let payload =
        || serde_json::json!({ "email": "ada@example.com", "consent": true, "submitted_in_ms": 4200 });
    store::capture(&pool, &submission(org, source_id, payload()))
        .await
        .expect("the first submission records the break");
    assert_eq!(broken_of(&pool, source_id).await, vec!["job".to_string()]);

    // A second submission with the identical health: same answer, so the health write is
    // skipped. The list is the observable either way, and the *stability* of the answer is what
    // the assertion is really about — a check that re-derived a different verdict per
    // submission would flap the badge on and off in the source list.
    store::capture(&pool, &submission(org, source_id, payload()))
        .await
        .expect("the second submission");
    assert_eq!(
        broken_of(&pool, source_id).await,
        vec!["job".to_string()],
        "the same health must not change the recorded answer"
    );

    drop_org(&pool, org).await;
}
