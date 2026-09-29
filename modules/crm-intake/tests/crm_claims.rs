//! The delivery-claim gate: one submission, one lead — under *concurrency*.
//!
//! Run through `scripts/qa/run-crm-claims.sh`.
//!
//! ## Why this file exists
//!
//! "One submission, one lead" was the last unticked acceptance box in REQ-117, and it was
//! unticked because every way of failing it is invisible. A second lead does not raise, does
//! not log, and appears on no screen as anything but two rows that both look right. The code
//! that was supposed to prevent it was a `select … where exists` **followed by an unguarded
//! insert** — a read-then-write with no lock and nothing to collide with.
//!
//! The defect is therefore invisible to every sequential test, which is exactly the note in
//! `run-crm-assignment.sh`'s header about the round-robin cursor: *"a read-then-write cursor
//! passes every sequential test and fails here, intermittently."* That gate exists because the
//! same shape shipped once already, in a different column, on a different request. This file is
//! the second gate for the same shape.
//!
//! ## What the tests actually assert
//!
//! Not "calling capture twice returns the same lead" — the old code passed that. They assert
//! the property the migration makes true of the *data*: of N simultaneous deliveries of one
//! submission, **exactly one lead row exists**, and every delivery that got an answer got the
//! same lead. A sequential double-call proves the read; only a concurrent one proves the key.

use std::sync::Arc;

use omnion_module_crm_intake::claims::{self, Claimed};
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

/// A keyed endpoint whose mapping produces a contactable lead from `email`.
///
/// The name carries the id because `crm_intake_sources` is unique on
/// `(organization_id, name)` — and a test that needs two sources in one organization then has
/// to make that name different, which is a real constraint of the product rather than a
/// nuisance of the fixture.
async fn source(pool: &PgPool, org: Uuid) -> Uuid {
    let label = format!("claims gate {}", Uuid::new_v4().simple());
    let draft = NewIntakeSource::endpoint(org, &label, None)
        .with_mapping(
            vec![
                MappingEntry::new("email", "email"),
                MappingEntry::new("first_name", "name"),
            ],
            vec!["email".to_string()],
        );
    let (created, _key) = store::create_source(pool, &draft).await.expect("a source");
    created.id
}

fn payload() -> serde_json::Value {
    serde_json::json!({
        "email": "visitor@example.com",
        "name": "Ada",
        "consent": true,
        "submitted_in_ms": 4200,
    })
}

fn submission(org: Uuid, source_id: Uuid, id: Option<&str>) -> Submission {
    Submission {
        organization_id: org,
        site_id: None,
        source_id,
        submission_id: id.map(str::to_string),
        ip: Some("203.0.113.7".to_string()),
        payload: payload(),
        received_at: time::OffsetDateTime::now_utc(),
    }
}

async fn lead_count(pool: &PgPool, org: Uuid) -> i64 {
    sqlx::query_scalar("select count(*) from crm_leads where organization_id = $1")
        .bind(org)
        .fetch_one(pool)
        .await
        .expect("the lead count")
}

/// The defect, reproduced: many simultaneous deliveries of one submission id must leave one row.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn eight_simultaneous_deliveries_of_one_submission_write_one_lead() {
    let pool = pool().await;
    let org = fresh_org(&pool, "claims-concurrent").await;
    let source_id = source(&pool, org).await;
    let pool = Arc::new(pool);

    // Every delivery starts before any of them has finished, which is the whole point: the old
    // read-then-write read "not found" in all eight and inserted in all eight. `barrier` makes
    // the simultaneity a fact rather than a hope about the scheduler.
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let pool = Arc::clone(&pool);
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            store::capture(&pool, &submission(org, source_id, Some("delivery-abc")))
                .await
                .map(|captured| captured.lead.id)
        }));
    }

    let mut answered = Vec::new();
    for handle in handles {
        match handle.await.expect("the delivery task") {
            Ok(lead_id) => answered.push(lead_id),
            // A refused redelivery is a legitimate answer (the winner had not finished yet) and
            // is counted separately below — what must never happen is a *second lead row*.
            Err(_) => {}
        }
    }

    assert_eq!(
        lead_count(&pool, org).await,
        1,
        "eight deliveries of one submission id must produce exactly one lead"
    );

    // And every delivery that *did* get a lead got the same one — a second id answering would
    // be a duplicate even though the count happened to be one.
    let distinct: std::collections::BTreeSet<Uuid> = answered.iter().copied().collect();
    assert!(
        distinct.len() <= 1,
        "deliveries answered with different leads: {distinct:?}"
    );

    drop_org(&pool, org).await;
}

/// The claim is the mechanism, and it is the *key* — not a lock, not a retry loop.
#[tokio::test]
async fn the_second_delivery_loses_the_claim_and_is_told_who_won() {
    let pool = pool().await;
    let org = fresh_org(&pool, "claims-loser").await;
    let source_id = source(&pool, org).await;

    let first = claims::take(&pool, source_id, "shared-key")
        .await
        .expect("the first claim");
    assert!(
        matches!(first, Claimed::Owned { .. }),
        "the first delivery owns the claim"
    );

    let second = claims::take(&pool, source_id, "shared-key")
        .await
        .expect("the second claim");
    assert_eq!(
        second,
        Claimed::Taken { lead_id: None },
        "the second delivery is told the claim is held, and that nothing has been written yet"
    );

    drop_org(&pool, org).await;
}

/// A claim is scoped to its source: two sources may legitimately hold the same id, because the
/// id is the *form's* and a site can have two forms with the same key.
#[tokio::test]
async fn a_claim_is_scoped_to_its_source() {
    let pool = pool().await;
    let org = fresh_org(&pool, "claims-scoped").await;
    let first_source = source(&pool, org).await;
    let second_source = source(&pool, org).await;

    assert!(matches!(
        claims::take(&pool, first_source, "same-key").await.expect("first"),
        Claimed::Owned { .. }
    ));
    assert!(
        matches!(
            claims::take(&pool, second_source, "same-key").await.expect("second"),
            Claimed::Owned { .. }
        ),
        "a second source holds the same submission id without colliding"
    );

    drop_org(&pool, org).await;
}

/// The trap a bare unique index would have sprung: a capture that dies leaves its claim open,
/// and the next delivery must be able to recover it rather than find the form wedged for ever.
#[tokio::test]
async fn a_claim_left_open_by_a_dead_capture_is_taken_over_not_obeyed() {
    let pool = pool().await;
    let org = fresh_org(&pool, "claims-stale").await;
    let source_id = source(&pool, org).await;

    claims::take(&pool, source_id, "crashed-key")
        .await
        .expect("a claim nobody will complete");

    // Too young to steal: a live capture must not have its submission taken away.
    assert_eq!(
        claims::take_over_stale(&pool, source_id, "crashed-key", time::OffsetDateTime::now_utc())
            .await
            .expect("a young sweep"),
        None,
        "a claim taken a moment ago is a capture in flight, not a crash"
    );

    // **Age the claim itself.** Computing an old instant and then sweeping with `now_utc` would
    // test nothing: the sweep's cutoff is derived from the `now` it is *given*, so passing the
    // present time makes every claim look fresh however long ago it was taken. The claim has to
    // be backdated in the database, which is the only place the age is real.
    sqlx::query(
        "update crm_lead_submissions set claimed_at = $2 \
         where source_id = $1 and submission_id = 'crashed-key'",
    )
    .bind(source_id)
    .bind(time::OffsetDateTime::now_utc() - claims::CLAIM_STALE_AFTER * 3)
    .execute(&pool)
    .await
    .expect("backdating the abandoned claim");

    // **The redelivery heals its own predecessor** — no sweeper, no schedule, no cleanup job to
    // forget to install. The claim exists, so a bare unique index would have answered "taken"
    // here for ever; the delivery is the only party that knows somebody is actually retrying.
    let captured = store::capture(&pool, &submission(org, source_id, Some("crashed-key")))
        .await
        .expect("the redelivery captures after inheriting a dead claim");
    assert_eq!(lead_count(&pool, org).await, 1, "recovery, then one lead");

    // And the same submission is now closed: a third attempt finds the lead the recovery wrote,
    // and no amount of retrying adds a row.
    let again = store::capture(&pool, &submission(org, source_id, Some("crashed-key")))
        .await
        .expect("the second redelivery finds the claim");
    assert_eq!(again.lead.id, captured.lead.id, "one submission, one lead");
    assert_eq!(lead_count(&pool, org).await, 1);

    // A second recoverer gets nothing, because the inherited claim is fresh again.
    assert_eq!(
        claims::take_over_stale(&pool, source_id, "crashed-key", time::OffsetDateTime::now_utc())
            .await
            .expect("a sweep after the recovery"),
        None,
        "once inherited, the claim is fresh and a second recoverer gets nothing"
    );

    drop_org(&pool, org).await;
}

/// A redelivery after the winner finished answers the winner's lead, not a new one.
#[tokio::test]
async fn a_completed_claim_answers_the_lead_its_winner_wrote() {
    let pool = pool().await;
    let org = fresh_org(&pool, "claims-complete").await;
    let source_id = source(&pool, org).await;

    let first = store::capture(&pool, &submission(org, source_id, Some("once")))
        .await
        .expect("the first delivery");
    let second = store::capture(&pool, &submission(org, source_id, Some("once")))
        .await
        .expect("the redelivery");

    assert_eq!(second.lead.id, first.lead.id);
    assert_eq!(lead_count(&pool, org).await, 1);
    assert_eq!(
        claims::lead_of(&pool, source_id, "once").await.expect("the claim"),
        Some(first.lead.id),
        "the claim names the lead, so a redelivery needs no guesswork"
    );

    drop_org(&pool, org).await;
}

/// A submission with no key still captures — the claim is an opt-in for callers that have an
/// identity, not a new requirement on the browser form post that has none.
#[tokio::test]
async fn a_submission_without_an_id_still_captures() {
    let pool = pool().await;
    let org = fresh_org(&pool, "claims-nokey").await;
    let source_id = source(&pool, org).await;

    store::capture(&pool, &submission(org, source_id, None))
        .await
        .expect("an unkeyed capture");
    store::capture(&pool, &submission(org, source_id, None))
        .await
        .expect("a second unkeyed capture");

    assert_eq!(
        lead_count(&pool, org).await,
        2,
        "two unkeyed posts are two submissions, and pretending otherwise would lose a lead"
    );
    assert!(
        sqlx::query_scalar::<_, i64>(
            "select count(*) from crm_lead_submissions where source_id = $1"
        )
        .bind(source_id)
        .fetch_one(&pool)
        .await
        .expect("the claim count")
        == 0,
        "an unkeyed submission writes no claim to collide with"
    );

    drop_org(&pool, org).await;
}

/// The claim table's own completion invariant, at the database: a row that is half-written is
/// the one shape that must be impossible, because "completed" and "has a lead" have to mean
/// the same thing to every reader.
#[tokio::test]
async fn the_database_refuses_a_claim_that_is_half_written() {
    let pool = pool().await;
    let org = fresh_org(&pool, "claims-check").await;
    let source_id = source(&pool, org).await;
    let lead = store::capture(&pool, &submission(org, source_id, None))
        .await
        .expect("a lead to point a claim at")
        .lead;

    // lead_id without completed_at.
    let half = sqlx::query(
        "insert into crm_lead_submissions (source_id, submission_id, lead_id) \
         values ($1, 'half-a', $2)",
    )
    .bind(source_id)
    .bind(lead.id)
    .execute(&pool)
    .await;
    assert!(half.is_err(), "a claim with a lead but no completion is refused");

    // completed_at without lead_id.
    let half = sqlx::query(
        "insert into crm_lead_submissions (source_id, submission_id, completed_at) \
         values ($1, 'half-b', now())",
    )
    .bind(source_id)
    .execute(&pool)
    .await;
    assert!(half.is_err(), "a claim that is complete but names no lead is refused");

    // A blank id, which would otherwise be a key every blank post collides on.
    let blank = sqlx::query(
        "insert into crm_lead_submissions (source_id, submission_id) values ($1, '')",
    )
    .bind(source_id)
    .execute(&pool)
    .await;
    assert!(blank.is_err(), "a blank submission id is not an identity");

    drop_org(&pool, org).await;
}
