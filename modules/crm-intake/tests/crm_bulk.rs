//! The bulk hand-over's gate: `store::bulk_assign_owner` against a real database.
//!
//! Run through `scripts/qa/run-crm-assign.sh` (it runs every `crm_*` test binary).
//!
//! ## What this file exists to prove that a unit test cannot
//!
//! * **Partial success is the normal case.** Twenty leads, one filed as spam, one belonging to
//!   another tenant: nineteen move and two do not. If the batch were a single transaction the
//!   legitimate hand-overs would roll back and the operator would be told "failed" about work
//!   that had already happened — and would then press again, doubling the trail.
//! * **Every refusal carries its own reason.** "19 of 20" without saying *which* one and why
//!   leaves an operator unable to tell a verdict from a typo, and those need different button
//!   presses.
//! * **The cap is enforced where the vocabulary is.** `MAX_BULK_IDS` existed for two ticks with
//!   nothing reading it; a handler-only check would make the constant a comment and a second
//!   caller would get a two-thousand-lead batch.
//!
//! The summary sentence is the one pure claim here, and it is last because it is the only one
//! that does not need the box to be honest — everything above it is a *row* claim.
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

/// A source and one lead through it, captured through the real capture path.
async fn one_lead(pool: &PgPool, org: Uuid, email: &str) -> Uuid {
    let source_id = Uuid::new_v4();
    let mapping = serde_json::json!([
        { "target": "email", "source_key": "email", "transform": ["trim", "lowercase"],
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
    .bind(format!("bulk source {}", source_id))
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
            payload: serde_json::json!({ "email": email }),
            received_at: time::OffsetDateTime::now_utc(),
        },
    )
    .await
    .expect("capture");
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

#[tokio::test]
async fn a_batch_where_one_row_is_a_verdict_moves_the_rest_and_names_the_one() {
    let pool = pool().await;
    let org = fresh_org(&pool, "bulk-partial").await;
    let person = one_user(&pool, org, "On Call").await;

    let first = one_lead(&pool, org, "bulk-one@example.test").await;
    let second = one_lead(&pool, org, "bulk-two@example.test").await;
    let spam = one_lead(&pool, org, "bulk-spam@example.test").await;
    sqlx::query("update crm_leads set status = 'spam' where id = $1")
        .bind(spam)
        .execute(&pool)
        .await
        .expect("file one as spam");

    let report = store::bulk_assign_owner(
        &pool,
        org,
        &[first, spam, second],
        Some(person),
        "on call this week",
        None,
    )
    .await
    .expect("the batch");

    assert_eq!(report.applied(), 2, "the two real leads moved");
    assert_eq!(report.refused(), 1, "the verdict did not");

    let refused = report
        .results
        .iter()
        .find(|row| row.id == spam)
        .expect("the spam row is in the report, named");
    assert!(!refused.done);
    let why = refused.reason.clone().unwrap_or_default();
    assert!(
        why.contains("spam"),
        "the refusal says which verdict, because the caller has to know which button to press \
         next. Got: {why}"
    );

    // The report must not be able to lie: read the rows back.
    let spam_row = store::find_lead(&pool, org, spam)
        .await
        .expect("read")
        .expect("the lead");
    assert_eq!(
        spam_row.owner_user_id, None,
        "a discarded enquiry is not somebody's work"
    );
    for lead in [first, second] {
        let moved = store::find_lead(&pool, org, lead)
            .await
            .expect("read")
            .expect("the lead");
        assert_eq!(
            moved.owner_user_id,
            Some(person),
            "the real hand-over landed"
        );
    }

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_batch_names_a_foreign_lead_instead_of_rolling_the_others_back() {
    let pool = pool().await;
    let org = fresh_org(&pool, "bulk-cross").await;
    let theirs = fresh_org(&pool, "bulk-cross-other").await;
    let person = one_user(&pool, org, "Mine").await;

    let mine = one_lead(&pool, org, "bulk-cross-mine@example.test").await;
    let not_mine = one_lead(&pool, theirs, "bulk-cross-theirs@example.test").await;

    let report = store::bulk_assign_owner(
        &pool,
        org,
        &[mine, not_mine],
        Some(person),
        "cross-tenant probe",
        None,
    )
    .await
    .expect("the batch");

    assert_eq!(
        report.applied(),
        1,
        "the lead that exists in this organization moved"
    );
    let refused = report
        .results
        .iter()
        .find(|row| row.id == not_mine)
        .expect("the foreign lead is named, not silently dropped");
    assert!(!refused.done);
    let why = refused.reason.clone().unwrap_or_default();
    assert!(
        why.contains("organization"),
        "the refusal is about tenancy, not a bare failure. Got: {why}"
    );

    let untouched = store::find_lead(&pool, theirs, not_mine)
        .await
        .expect("read")
        .expect("the lead");
    assert_eq!(
        untouched.owner_user_id, None,
        "another tenant's lead is not writable through this tenant's batch"
    );

    drop_org(&pool, org).await;
    drop_org(&pool, theirs).await;
}

#[tokio::test]
async fn a_batch_that_names_nothing_or_too_much_is_refused_before_anything_is_written() {
    let pool = pool().await;
    let org = fresh_org(&pool, "bulk-bounds").await;
    let person = one_user(&pool, org, "Bounds").await;
    let lead = one_lead(&pool, org, "bulk-bounds@example.test").await;

    let empty = store::bulk_assign_owner(&pool, org, &[], Some(person), "nothing", None).await;
    assert!(
        empty.is_err(),
        "an empty bulk action must not answer success — it did nothing and has to say so"
    );

    let too_many: Vec<Uuid> = (0..omnion_module_crm_intake::vocabulary::MAX_BULK_IDS + 1)
        .map(|_| Uuid::new_v4())
        .collect();
    let over =
        store::bulk_assign_owner(&pool, org, &too_many, Some(person), "too much", None).await;
    assert!(
        over.is_err(),
        "the cap is the store's and not only the handler's: a value one layer refuses and the \
         other accepts reads as nothing-happened from whichever caller forgot the check"
    );

    let untouched = store::find_lead(&pool, org, lead)
        .await
        .expect("read")
        .expect("the lead");
    assert_eq!(
        untouched.owner_user_id, None,
        "a refused batch wrote nothing"
    );

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_batch_back_to_the_queue_is_a_hand_over_and_writes_one_trail_line_per_lead() {
    let pool = pool().await;
    let org = fresh_org(&pool, "bulk-queue").await;
    let person = one_user(&pool, org, "Holder").await;

    let first = one_lead(&pool, org, "bulk-q1@example.test").await;
    let second = one_lead(&pool, org, "bulk-q2@example.test").await;
    for lead in [first, second] {
        store::assign_owner(&pool, org, lead, Some(person), "first home", None)
            .await
            .expect("assign")
            .expect("the lead");
    }

    let report =
        store::bulk_assign_owner(&pool, org, &[first, second], None, "queue is empty", None)
            .await
            .expect("the batch");
    assert_eq!(
        report.applied(),
        2,
        "putting them back is an assignment, not a cleared field"
    );

    for lead in [first, second] {
        assert_eq!(
            store::find_lead(&pool, org, lead)
                .await
                .expect("read")
                .expect("the lead")
                .owner_user_id,
            None,
            "the queue really is empty for this one"
        );
        // Capture writes an `assigned` line of its own when it claims the lead for a rule, so
        // counting every `assigned`/`reassigned` line sees three where the batch wrote two. The
        // batch's own lines are the ones carrying the reason it passed, which is the same
        // discriminator the hand-over tests use — count the rows this feature writes, not the
        // lead's whole history.
        let lines = store::list_events(&pool, org, lead).await.expect("events");
        let hands = lines
            .iter()
            .filter(|line| {
                (line.kind == "assigned" || line.kind == "reassigned")
                    && line
                        .detail
                        .get("reason")
                        .and_then(|value| value.as_str())
                        .is_some_and(|reason| !reason.is_empty())
            })
            .count();
        assert_eq!(
            hands, 2,
            "each lead keeps both of its hands — the trail is the only history"
        );
    }

    drop_org(&pool, org).await;
}

#[test]
fn the_summary_is_one_sentence_that_counts_the_refusals() {
    use omnion_module_crm_intake::store::{BulkAssignOutcome, BulkAssignReport};

    let mut clean = BulkAssignReport::default();
    for _ in 0..2 {
        clean.results.push(BulkAssignOutcome {
            id: Uuid::nil(),
            done: true,
            reason: None,
        });
    }
    assert!(clean.summary().contains("2"), "{}", clean.summary());
    assert_eq!(clean.refused(), 0);

    let mut mixed = BulkAssignReport::default();
    for _ in 0..3 {
        mixed.results.push(BulkAssignOutcome {
            id: Uuid::nil(),
            done: true,
            reason: None,
        });
    }
    for reason in [
        "this lead is spam",
        "this lead is spam",
        "no such lead in this organization",
    ] {
        mixed.results.push(BulkAssignOutcome {
            id: Uuid::nil(),
            done: false,
            reason: Some(reason.to_string()),
        });
    }
    let sentence = mixed.summary();
    assert!(sentence.starts_with("3 of 6"), "{sentence}");
    // The two identical refusals are counted once, not printed twice: the panel renders one
    // line per *reason*, and a list of twenty identical lines is a list nobody reads.
    assert!(sentence.contains("2 x this lead is spam"), "{sentence}");
    assert!(
        sentence.contains("1 x no such lead in this organization"),
        "{sentence}"
    );
}
