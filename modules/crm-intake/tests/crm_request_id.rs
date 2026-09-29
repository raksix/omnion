//! The request id on a trail line (REQ-117, acceptance 17 — the `request id` half).
//!
//! Run through `scripts/qa/run-crm-request-id.sh`.
//!
//! ## What this gate is actually for
//!
//! Acceptance 17 asks for an audit entry with "actor, before/after **and request id**". The first
//! two shipped long ago; this is the third, and the reason it was left until now is worth
//! recording: the API had no per-request id on these routes, and the REQ explicitly refused to
//! invent a header-shaped column that is never populated — "a column that reads as recorded and is
//! not" is worse than an absent one.
//!
//! **That is the shape of the failure this gate exists to prevent.** A trail line written without
//! an id is *not* an error, is *not* logged, and is indistinguishable from a healthy row on every
//! screen. So the acceptance line could have been ticked with the column in place and nothing
//! behind it — and every test that only asserted "the column exists" would have stayed green.
//!
//! ## What the tests assert
//!
//! Not "the detail object has a `request_id` key". They assert the value **in the stored row**, in
//! three states that have to be told apart:
//!
//! * an exchange that carried an id → the row carries that exact id;
//! * no exchange (a worker sweep, a direct store call) → the row says `null` rather than omitting
//!   the key, because an absent key cannot be told apart from a line written before the field
//!   existed;
//! * two concurrent exchanges → each row carries **its own** id, which is the cross-talk case that
//!   a shared (thread-local or global) cell would get wrong and a test on one task cannot reach.

use omnion_module_crm_intake::request_id::{self, scope, scope_with};
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

async fn source(pool: &PgPool, org: Uuid) -> Uuid {
    let label = format!("request-id gate {}", Uuid::new_v4().simple());
    let draft = NewIntakeSource::endpoint(org, &label, None).with_mapping(
        vec![
            MappingEntry::new("email", "email"),
            MappingEntry::new("first_name", "name"),
        ],
        vec!["email".to_string()],
    );
    let (created, _key) = store::create_source(pool, &draft).await.expect("a source");
    created.id
}

/// `visitor` has to be a parameter: the cross-talk test captures two leads at once, and an
/// assertion that keyed both rows by the same e-mail could not tell "each row carries its own
/// id" from "both rows carry the same id" — which is precisely the failure it exists to catch.
fn submission_for(org: Uuid, source_id: Uuid, visitor: &str) -> Submission {
    Submission {
        organization_id: org,
        site_id: None,
        source_id,
        submission_id: None,
        ip: Some("203.0.113.7".to_string()),
        payload: serde_json::json!({
            "email": format!("{visitor}@example.com"),
            "name": "Ada",
            "consent": true,
            "submitted_in_ms": 4200,
        }),
        received_at: time::OffsetDateTime::now_utc(),
    }
}

fn submission(org: Uuid, source_id: Uuid) -> Submission {
    submission_for(org, source_id, "visitor")
}

/// What the stored line says about the request id, read back out of the database.
///
/// Two facts, not one, because they are different claims and the gate has to make both:
///
/// * `present` — the `request_id` **key exists**. A missing key cannot be told apart from a line
///   written before the field existed, which is exactly the ambiguity that lets "the column is
///   there" pass for "the column is filled".
/// * `value` — what it holds, via `->>` so a JSON `null` reads as SQL `None` rather than as the
///   four-character string "null".
async fn stored_request_id(
    pool: &PgPool,
    org: Uuid,
    kind: &str,
) -> Option<(bool, Option<String>)> {
    sqlx::query_as::<_, (bool, Option<String>)>(
        "select e.detail ? 'request_id', e.detail->>'request_id' from crm_lead_events e \
         join crm_leads l on l.id = e.lead_id \
         where l.organization_id = $1 and e.kind = $2 \
         order by e.id desc limit 1",
    )
    .bind(org)
    .bind(kind)
    .fetch_optional(pool)
    .await
    .expect("the trail line")
}

/// The lead's own id, so a test can read one specific row's trail.
async fn lead_id(pool: &PgPool, org: Uuid) -> Uuid {
    sqlx::query_scalar("select id from crm_leads where organization_id = $1 limit 1")
        .bind(org)
        .fetch_one(pool)
        .await
        .expect("the captured lead")
}

#[tokio::test]
async fn a_capture_inside_an_exchange_stores_that_exchanges_id() {
    let pool = pool().await;
    let org = fresh_org(&pool, "request-id-scoped").await;
    let source_id = source(&pool, org).await;

    scope(Some("req-capture-0001".to_string()), async {
        store::capture(&pool, &submission(org, source_id))
            .await
            .expect("the submission is captured");
    })
    .await;

    let (present, value) = stored_request_id(&pool, org, "received")
        .await
        .expect("the arrival line exists");
    assert!(present, "the key must be in the stored detail, not merely in the schema");
    assert_eq!(
        value.as_deref(),
        Some("req-capture-0001"),
        "the stored row must carry the exchange's id, not the key it happens to have"
    );

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_capture_with_no_exchange_records_null_rather_than_omitting_the_key() {
    let pool = pool().await;
    let org = fresh_org(&pool, "request-id-absent").await;
    let source_id = source(&pool, org).await;

    // The worker sweep case: no exchange exists, and inventing one would write a value that
    // correlates with nothing — which is the failure the whole field exists to avoid.
    store::capture(&pool, &submission(org, source_id))
        .await
        .expect("the submission is captured");

    let (present, value) = stored_request_id(&pool, org, "received")
        .await
        .expect("the arrival line exists");
    assert!(
        present,
        "the key must be present with a null value, not absent: an absent key is \
         indistinguishable from a line written before the field existed"
    );
    assert_eq!(
        value, None,
        "no exchange in, nothing out — an invented id correlates with nothing"
    );
    drop_org(&pool, org).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_exchanges_do_not_write_each_others_id() {
    let pool = pool().await;
    let org = fresh_org(&pool, "request-id-cross-talk").await;
    let source_id = source(&pool, org).await;

    // Two leads, two exchanges, interleaved. A shared cell would put one id on both rows and
    // neither row would show anything wrong — so the assertion is per *lead*, not per org.
    let first = tokio::spawn({
        let pool = pool.clone();
        async move {
            scope(Some("req-exchange-one".to_string()), async {
                store::capture(&pool, &submission_for(org, source_id, "one"))
                    .await
                    .expect("the first submission");
            })
            .await
        }
    });
    let second = tokio::spawn({
        let pool = pool.clone();
        async move {
            scope_with(
                Some("req-exchange-two".to_string()),
                store::capture(&pool, &submission_for(org, source_id, "two")),
            )
            .await
            .expect("the second submission");
        }
    });
    first.await.expect("the first task");
    second.await.expect("the second task");

    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "select l.email, e.detail->>'request_id' from crm_lead_events e \
         join crm_leads l on l.id = e.lead_id \
         where l.organization_id = $1 and e.kind = 'received'",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .expect("both arrival lines");

    assert_eq!(rows.len(), 2, "both submissions were captured: {rows:?}");
    // Keyed on the *lead's* address rather than on the id, so this asserts each row carries its
    // own exchange's id — the failure a per-organization check would read as a pass.
    for (email, id) in &rows {
        let expected = match email.as_str() {
            "one@example.com" => "req-exchange-one",
            "two@example.com" => "req-exchange-two",
            other => panic!("unexpected captured address {other}"),
        };
        assert_eq!(
            id.as_deref(),
            Some(expected),
            "a lead must carry its OWN exchange's id: {email} was captured inside {expected}"
        );
    }
    // And the negative control, stated rather than implied: if both rows carried the same id the
    // loop above would still pass only by accident of the fixture, so the two ids must be
    // genuinely different values in the result.
    let mut ids: Vec<&str> = rows.iter().filter_map(|(_, id)| id.as_deref()).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec!["req-exchange-one", "req-exchange-two"], "{ids:?}");

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn the_id_survives_a_hand_over_into_the_assignment_trail_line() {
    let pool = pool().await;
    let org = fresh_org(&pool, "request-id-assign").await;
    let source_id = source(&pool, org).await;

    store::capture(&pool, &submission(org, source_id))
        .await
        .expect("the submission is captured");
    let lead = lead_id(&pool, org).await;

    // Assignment is the trail line whose transaction the module documents as atomic with the
    // write, so it is the line a missing id would be most damaging on: the row changed hands and
    // the audit says nothing about which request did it.
    scope(Some("req-assign-0002".to_string()), async {
        store::assign_owner(&pool, org, lead, None, "back to the queue", None)
            .await
            .expect("the assignment");
    })
    .await;

    let (present, value) = stored_request_id(&pool, org, "assigned")
        .await
        .expect("the assignment line exists");
    assert!(present, "the assignment line carries the key too");
    assert_eq!(
        value.as_deref(),
        Some("req-assign-0002"),
        "the hand-over line is the one whose audit answer matters most: the lead changed \
         hands and the trail has to say which request did it"
    );

    drop_org(&pool, org).await;
}
