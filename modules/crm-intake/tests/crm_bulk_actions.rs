//! The bulk bar's other verbs and its export, against a real database.
//!
//! Run through `scripts/qa/run-crm-bulk-actions.sh`.
//!
//! ## What this file exists to prove that a unit test cannot
//!
//! * **Partial success is still the normal case.** `run_action` is per row for the hand-over's
//!   reason; the new question is whether the *refusals* name the right thing. A lead already
//!   filed as spam and a rejection are different cases: the spam verb is what the operator
//!   asked for, the reject verb is not.
//! * **`record_response` is idempotent on the instant**, so a re-run is free — but the *trail*
//!   is not idempotent, and the number of lines is what an operator's "when did we answer
//!   this" actually rests on.
//! * **The export's rows are the filter's rows.** The cursor loop is the only place in this
//!   crate where a bounded read is the wrong shape, and a loop that advances wrongly either
//!   drops rows or repeats them — the one thing a CSV with a duplicated lead cannot be repaired
//!   for.
//!
//! ## The fixture rule this file follows
//!
//! **Read the consequence, not the call.** `fail_step`'s silent no-op hid a flaky gate on this
//! branch for three ticks, and the lesson generalises: every assertion here re-reads the row
//! (`find_lead`, `list_events`) instead of trusting that a function which returned `Ok` did
//! something.
use omnion_module_crm_intake::bulk::{self, BulkAction};
use omnion_module_crm_intake::export;
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
    .bind(format!("bulk-action source {}", source_id))
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
        captured.lead.status, "new",
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

async fn status_of(pool: &PgPool, org: Uuid, id: Uuid) -> String {
    store::find_lead(pool, org, id)
        .await
        .expect("read")
        .expect("the lead is still there")
        .status
}

#[tokio::test]
async fn mark_responded_stops_every_clock_in_one_press() {
    let pool = pool().await;
    let org = fresh_org(&pool, "bulk-respond").await;
    let first = one_lead(&pool, org, "bulk-respond-1@example.test").await;
    let second = one_lead(&pool, org, "bulk-respond-2@example.test").await;

    let report = bulk::run_action(
        &pool,
        org,
        &[first, second],
        BulkAction::Respond,
        "",
        None,
    )
    .await
    .expect("the batch");
    assert_eq!(report.applied(), 2, "both leads answered: {}", report.summary());

    // The consequence, not the return value.
    for id in [first, second] {
        let lead = store::find_lead(&pool, org, id)
            .await
            .expect("read")
            .expect("the lead");
        assert!(
            lead.first_response_at.is_some(),
            "the SLA clock stopped — `Ok` from the store is not the measurement"
        );
        assert_eq!(
            lead.status, "contacted",
            "and the status follows the clock, which is what the inbox's column reads"
        );
    }

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn responding_twice_keeps_the_first_instant_and_the_trail_names_it_once() {
    let pool = pool().await;
    let org = fresh_org(&pool, "bulk-respond-twice").await;
    let lead = one_lead(&pool, org, "bulk-respond-twice@example.test").await;

    bulk::run_action(&pool, org, &[lead], BulkAction::Respond, "", None)
        .await
        .expect("the first press");
    let first_at = store::find_lead(&pool, org, lead)
        .await
        .expect("read")
        .expect("the lead")
        .first_response_at;

    bulk::run_action(&pool, org, &[lead], BulkAction::Respond, "", None)
        .await
        .expect("the second press");
    let second_at = store::find_lead(&pool, org, lead)
        .await
        .expect("read")
        .expect("the lead")
        .first_response_at;

    assert_eq!(
        first_at, second_at,
        "a double click must not rewrite the measurement a whole SLA report rests on"
    );

    // **The trail is not idempotent, and the gate says so rather than pretending it is.** Two
    // presses legitimately write two `responded` lines — that is what happened — so the
    // assertion is that both carry the SAME instant. A panel rendering "answered at" from the
    // newest line and an SLA report reading from the column would then agree, and that is the
    // property worth proving.
    let lines = store::list_events(&pool, org, lead).await.expect("events");
    let stamps: Vec<String> = lines
        .iter()
        .filter(|line| line.kind == "responded")
        .map(|line| {
            line.detail
                .get("first_response_at")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert_eq!(
        stamps.len(),
        2,
        "two presses are two facts, and the trail is the only place either is recorded"
    );
    assert!(
        stamps[0] == stamps[1] && !stamps[0].is_empty(),
        "and both lines name the SAME instant, so the newest line and the column agree: \
         {stamps:?}"
    );

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn one_lead_being_a_verdict_does_not_stop_the_rest() {
    let pool = pool().await;
    let org = fresh_org(&pool, "bulk-spam").await;
    let fresh = one_lead(&pool, org, "bulk-spam-fresh@example.test").await;
    let doomed = one_lead(&pool, org, "bulk-spam-doomed@example.test").await;
    sqlx::query("update crm_leads set status = 'converted' where id = $1")
        .bind(doomed)
        .execute(&pool)
        .await
        .expect("close one");

    let report = bulk::run_action(&pool, org, &[fresh, doomed], BulkAction::Spam, "", None)
        .await
        .expect("the batch");
    assert_eq!(
        report.applied(),
        2,
        "filing as spam is the operator's decision and a closed lead does not veto it: {}",
        report.summary()
    );
    for id in [fresh, doomed] {
        assert_eq!(
            status_of(&pool, org, id).await,
            "spam",
            "the row reads spam, not just the report"
        );
    }

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_rejection_without_a_reason_is_refused_and_writes_nothing() {
    let pool = pool().await;
    let org = fresh_org(&pool, "bulk-reject-reason").await;
    let lead = one_lead(&pool, org, "bulk-reject-noreason@example.test").await;

    let refused = bulk::run_action(&pool, org, &[lead], BulkAction::Reject, "   ", None).await;
    assert!(
        refused.is_err(),
        "REQ-117 says 'reject with reason'; a blank one is the case the sentence excludes"
    );
    assert_eq!(
        status_of(&pool, org, lead).await,
        "new",
        "and the refusal wrote nothing — a lead nobody can explain is one an operator undoes"
    );

    // Whitespace is not a reason, and the message has to say which half failed.
    let error = refused.expect_err("the refusal").to_string();
    assert!(
        error.contains("why"),
        "the operator has to know it is the reason, not the selection: {error}"
    );

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_foreign_lead_is_named_rather_than_written() {
    let pool = pool().await;
    let org = fresh_org(&pool, "bulk-act-cross").await;
    let theirs = fresh_org(&pool, "bulk-act-cross-other").await;
    let mine = one_lead(&pool, org, "bulk-act-mine@example.test").await;
    let not_mine = one_lead(&pool, theirs, "bulk-act-theirs@example.test").await;

    let report = bulk::run_action(
        &pool,
        org,
        &[mine, not_mine],
        BulkAction::Respond,
        "",
        None,
    )
    .await
    .expect("the batch");

    assert_eq!(report.applied(), 1, "only the lead that is mine moved");
    let refusal = report
        .results
        .iter()
        .find(|row| row.id == not_mine)
        .expect("the foreign lead is named");
    assert!(!refusal.done);
    assert!(
        refusal
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("organization"),
        "the refusal is about tenancy: {:?}",
        refusal.reason
    );
    let untouched = store::find_lead(&pool, theirs, not_mine)
        .await
        .expect("read")
        .expect("their lead");
    assert_eq!(
        untouched.first_response_at, None,
        "another tenant's SLA clock did not stop through this tenant's press"
    );

    drop_org(&pool, org).await;
    drop_org(&pool, theirs).await;
}

#[tokio::test]
async fn the_bounds_are_the_stores_before_any_row_is_touched() {
    let pool = pool().await;
    let org = fresh_org(&pool, "bulk-act-bounds").await;
    let lead = one_lead(&pool, org, "bulk-act-bounds@example.test").await;

    assert!(
        bulk::run_action(&pool, org, &[], BulkAction::Respond, "", None)
            .await
            .is_err(),
        "nothing selected must not answer success"
    );
    let too_many: Vec<Uuid> = (0..omnion_module_crm_intake::MAX_BULK_IDS + 1)
        .map(|_| Uuid::new_v4())
        .collect();
    assert!(
        bulk::run_action(&pool, org, &too_many, BulkAction::Respond, "", None)
            .await
            .is_err(),
        "the cap is the store's, not only the handler's"
    );
    assert_eq!(
        status_of(&pool, org, lead).await,
        "new",
        "and neither refusal touched the one lead in the organization"
    );

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn the_export_carries_the_filter_and_never_the_payload() {
    let pool = pool().await;
    let org = fresh_org(&pool, "bulk-export").await;
    let _mine = one_lead(&pool, org, "bulk-export-mine@example.test").await;
    let _other = one_lead(&pool, org, "bulk-export-other@example.test").await;
    let theirs = fresh_org(&pool, "bulk-export-other-org").await;
    let _theirs = one_lead(&pool, theirs, "bulk-export-theirs@example.test").await;

    let query = store::LeadQuery::inbox();
    let rows = store::list_leads(&pool, org, &query)
        .await
        .expect("the filter")
        .leads;
    assert_eq!(rows.len(), 2, "two of this organization's leads");

    let document = export::render(&rows).expect("render");
    let lines: Vec<&str> = document.lines().collect();
    assert_eq!(
        lines.len(),
        3,
        "header plus two rows: {}",
        lines.len()
    );
    assert_eq!(lines[0], export::COLUMNS.join(","));
    assert!(
        document.contains("bulk-export-mine@example.test"),
        "the row that matched is in the file"
    );
    assert!(
        !document.contains("bulk-export-theirs@example.test"),
        "**another tenant's lead is not in this tenant's export** — the export reads through \
         the same filtered read, so this asserts the filter rather than the renderer"
    );
    assert!(
        !document.contains("\"payload\"") && !document.contains("bulk-export-mine@example.test\",\"{"),
        "and the raw submission never leaves in a spreadsheet"
    );

    drop_org(&pool, org).await;
    drop_org(&pool, theirs).await;
}

#[tokio::test]
async fn a_visitor_supplied_cell_cannot_arrive_as_a_formula() {
    let pool = pool().await;
    let org = fresh_org(&pool, "bulk-injection").await;
    let source_id = Uuid::new_v4();
    let mapping = serde_json::json!([
        { "target": "email", "source_key": "email", "required": false },
        { "target": "first_name", "source_key": "name", "required": false }
    ]);
    sqlx::query(
        "insert into crm_intake_sources (id, organization_id, name, kind, mapping, \
             required_targets, dedupe_policy) \
         values ($1, $2, $4, 'endpoint', $3, '{}', 'link')",
    )
    .bind(source_id)
    .bind(org)
    .bind(mapping)
    .bind(format!("injection source {}", source_id))
    .execute(&pool)
    .await
    .expect("a source");

    // **The cell is a value the visitor chose**, which is the whole point: `first_name` is a
    // free-text field on a public form, so this is not a hypothetical attacker.
    let hostile = "=1+1";
    store::capture(
        &pool,
        &Submission {
            organization_id: org,
            site_id: None,
            source_id,
            submission_id: None,
            ip: None,
            payload: serde_json::json!({ "email": "injection@example.test", "name": hostile }),
            received_at: time::OffsetDateTime::now_utc(),
        },
    )
    .await
    .expect("capture");

    let rows = store::list_leads(&pool, org, &store::LeadQuery::inbox())
        .await
        .expect("read")
        .leads;
    let document = export::render(&rows).expect("render");
    assert!(
        document.contains("\"\t=1+1\"") || document.contains("\t=1+1"),
        "the formula arrives tab-prefixed, as a literal: {document}"
    );
    assert!(
        !document.lines().any(|line| line.starts_with("=1+1")),
        "and never as the first characters of a cell: {document}"
    );

    drop_org(&pool, org).await;
}
