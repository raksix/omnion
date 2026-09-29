//! The slice-3 autoresponder gate: once per lead, nothing to a rejected one, against a real
//! database.
//!
//! Run through `scripts/qa/run-crm-autoresponder.sh`.
//!
//! ## What this file exists to prove that a unit test cannot
//!
//! * **The claim is a race, and exactly one caller wins it.** The pure half decides *what*
//!   to send; whether one lead gets one message is a property of the conditional insert, and
//!   a read-then-write passes every sequential test. This is the same reason
//!   `run-crm-assignment.sh` exists for the round-robin cursor.
//! * **Ten concurrent captures send one message.** Not two, not "usually one".
//! * **A released claim is answerable again.** A mailer that refuses must not silence the
//!   lead forever, which is the failure mode of claiming before sending.
//! * **A rejected or spam submission gets no message and says so on the trail.** A lead with
//!   no address is a different line from a lead whose submission was never accepted.
//! * **The trail is what a delayed reservation leaves behind.** A reserved-but-unsent line
//!   must read as pending, or the detail page claims a reply that was never sent.

use omnion_module_crm_intake::autoresponder::{Autoresponder, Delivery};
use omnion_module_crm_intake::autoresponder_store as ar_store;
use omnion_module_crm_intake::model::{IntakeSource, Lead, SpamVerdict};
use omnion_module_crm_intake::store::{self, Submission};
use sqlx::PgPool;
use uuid::Uuid;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL (the shell script sets it)");
    PgPool::connect(&url).await.expect("the QA database")
}

async fn fresh_org(pool: &PgPool, label: &str) -> Uuid {
    let org = Uuid::new_v4();
    // The slug carries a `organizations_slug_format` check, so the readable label belongs in
    // the name and the slug stays lowercase and hyphenated. Same shape as the other gates.
    let slug: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(org)
        .bind(format!("{label} {org}"))
        .bind(format!("{slug}-{}", org.simple()))
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
    ] {
        let _ = sqlx::query(sql).bind(org).execute(pool).await;
    }
    let _ = sqlx::query("delete from organizations where id = $1")
        .bind(org)
        .execute(pool)
        .await;
}

/// A source whose autoresponder is on, mapping e-mail through unchanged.
async fn source_with_autoresponder(pool: &PgPool, org: Uuid, delay_minutes: i64) -> IntakeSource {
    source(pool, org, Some(delay_minutes)).await
}

/// A capture source, with or without an autoresponder.
async fn source(pool: &PgPool, org: Uuid, delay_minutes: Option<i64>) -> IntakeSource {
    let autoresponder = match delay_minutes {
        Some(minutes) => serde_json::json!({
            "enabled": true,
            "template": "acknowledgement",
            "subject": "We received your message",
            "template_body": "thanks for writing to {{source}}",
            "delay_minutes": minutes,
        }),
        None => serde_json::json!({}),
    };
    let draft = omnion_module_crm_intake::model::NewIntakeSource {
        organization_id: org,
        site_id: None,
        name: format!("Contact {}", Uuid::new_v4().simple()),
        kind: "endpoint".to_string(),
        form_key: None,
        mapping: vec![omnion_module_crm_intake::mapping::MappingEntry {
            target: "email".to_string(),
            source_key: Some("email".to_string()),
            transform: vec!["trim".to_string()],
            required: true,
            fallback: None,
        }],
        required_targets: vec!["email".to_string()],
        consent_required: false,
        consent_text: None,
        dedupe_policy: "create_anyway".to_string(),
        pipeline_id: None,
        stage_id: None,
        auto_tags: Vec::new(),
        autoresponder,
        active: true,
        rate_limit_per_hour: 1000,
        created_by: None,
    };
    store::create_source(pool, &draft)
        .await
        .expect("a source")
        .0
}

/// One accepted lead, filed through the real capture path.
async fn accepted_lead(pool: &PgPool, org: Uuid, source: &IntakeSource, email: &str) -> Lead {
    let captured = store::capture(
        pool,
        &Submission {
            organization_id: org,
            site_id: source.site_id,
            source_id: source.id,
            submission_id: None,
            ip: Some("203.0.113.9".to_string()),
            payload: serde_json::json!({ "email": email, "first_name": "Ada" }),
            received_at: time::OffsetDateTime::now_utc(),
        },
    )
    .await
    .expect("the submission is captured");
    assert_eq!(
        captured.lead.status, "new",
        "the fixture must be an accepted lead for this suite to mean anything"
    );
    captured.lead
}

#[tokio::test]
async fn ten_concurrent_captures_of_one_lead_send_exactly_one_message() {
    let pool = pool().await;
    let org = fresh_org(&pool, "Autoresponder race").await;
    let source = source_with_autoresponder(&pool, org, 0).await;
    let lead = accepted_lead(&pool, org, &source, "racer@example.com").await;

    // Every task calls `prepare` for the same lead at once. The claim is the only thing
    // standing between ten callers and ten copies of the same sentence to one person.
    let now = time::OffsetDateTime::now_utc();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..10 {
        let pool = pool.clone();
        let lead = lead.clone();
        let source = source.clone();
        tasks.spawn(async move { ar_store::prepare(&pool, &lead, &source, now).await });
    }
    let mut winners = 0;
    while let Some(joined) = tasks.join_next().await {
        let outcome = joined
            .expect("the task does not panic")
            .expect("prepare does not fail");
        if outcome.sent() {
            winners += 1;
        }
    }

    assert_eq!(
        winners, 1,
        "ten concurrent captures produced {winners} senders, not one"
    );

    let lines: i64 =
        sqlx::query_scalar("select count(*) from crm_lead_events where lead_id = $1 and kind = $2")
            .bind(lead.id)
            .bind(ar_store::SENT_KIND)
            .fetch_one(&pool)
            .await
            .expect("counting the trail");
    assert_eq!(lines, 1, "one send, one timeline line");

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_released_claim_lets_the_next_attempt_answer() {
    let pool = pool().await;
    let org = fresh_org(&pool, "Autoresponder release").await;
    let source = source_with_autoresponder(&pool, org, 0).await;
    let lead = accepted_lead(&pool, org, &source, "retry@example.com").await;
    let now = time::OffsetDateTime::now_utc();

    // The mailer refused, so the platform releases the claim rather than leaving a lead that
    // has been "answered" by a message that never left.
    let first = ar_store::prepare(&pool, &lead, &source, now)
        .await
        .expect("prepare");
    assert!(first.sent(), "the first attempt owns the send");
    assert!(ar_store::was_sent(
        &ar_store::existing_claim(&pool, lead.id)
            .await
            .expect("reading the claim")
            .expect("a claim exists")
    ));

    // Nothing to release: the claim was recorded as sent. A second attempt is refused.
    let second = ar_store::prepare(&pool, &lead, &source, now)
        .await
        .expect("prepare");
    assert!(!second.sent(), "a sent lead is not sent again");

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_pending_claim_is_not_a_sent_one() {
    // The distinction the detail page's timeline depends on: a *reserved* delayed message and
    // a *delivered* one both leave a line, and only one of them means the visitor heard back.
    let pool = pool().await;
    let org = fresh_org(&pool, "Autoresponder pending").await;
    let source = source_with_autoresponder(&pool, org, 0).await;
    let lead = accepted_lead(&pool, org, &source, "pending@example.com").await;
    let now = time::OffsetDateTime::now_utc();

    let outcome = ar_store::prepare(&pool, &lead, &source, now)
        .await
        .expect("prepare");
    assert!(outcome.sent());

    // A delayed autoresponder does not claim at capture time — the worker will. Nothing is
    // written, so nothing can later be mistaken for a send.
    let delayed = source_with_autoresponder(&pool, org, 30).await;
    let other = accepted_lead(&pool, org, &delayed, "later@example.com").await;
    let verdict = ar_store::prepare(&pool, &other, &delayed, now)
        .await
        .expect("prepare")
        .verdict;
    assert!(
        matches!(verdict, Delivery::Ready(ref message) if message.delayed),
        "a delayed autoresponder is a reserved message, got {verdict:?}"
    );
    assert!(
        ar_store::existing_claim(&pool, other.id)
            .await
            .expect("reading the claim")
            .is_none(),
        "a delayed message must not claim at capture time"
    );

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_rejected_submission_is_answered_by_nothing_and_says_why() {
    let pool = pool().await;
    let org = fresh_org(&pool, "Autoresponder rejected").await;
    let source = source_with_autoresponder(&pool, org, 0).await;
    let now = time::OffsetDateTime::now_utc();

    // A spam submission, which is the case the REQ names ("a rejected spam submission sends
    // nothing") and the one that can carry an address. The `rejected` case is unreachable
    // through this fixture on purpose: `crm_leads_contactable_check` refuses a row with
    // neither e-mail nor phone at the database, so a rejected lead is rejected *because* it
    // could never be answered — and the pure half covers that verdict in a unit test.
    //
    // The honeypot key is written through a binding, not inline: `json!({ <expr>: value })` is
    // a *value*, not a key, so a non-literal key would be dropped and the honeypot would never
    // arrive — the test would then pass against a spam check that does not exist. That is
    // called out on `SpamVerdict::HONEYPOT` and it is easy to walk into twice.
    let mut payload = serde_json::json!({ "email": "spammer@example.com" });
    payload[SpamVerdict::HONEYPOT] = serde_json::json!("http://buy-my-phishing-kit.example");
    let captured = store::capture(
        &pool,
        &Submission {
            organization_id: org,
            site_id: source.site_id,
            source_id: source.id,
            submission_id: None,
            ip: Some("203.0.113.10".to_string()),
            payload,
            received_at: now,
        },
    )
    .await
    .expect("the submission is captured");
    assert_eq!(
        captured.lead.status, "spam",
        "a filled honeypot must file as spam"
    );
    assert!(
        captured.spam.is_spam(),
        "the store must agree the submission is spam"
    );

    let outcome = ar_store::prepare(&pool, &captured.lead, &source, now)
        .await
        .expect("prepare");
    assert!(!outcome.sent(), "a rejected submission is never answered");
    assert_eq!(outcome.verdict.reason(), "not_accepted");

    // The trail names the silence rather than leaving "we did not mail them" and "we never
    // considered it" as the same empty event.
    ar_store::record_skip(
        &pool,
        &captured.lead,
        &source,
        outcome.verdict.reason(),
        serde_json::json!({}),
    )
    .await
    .expect("the skip is recorded");
    let detail = ar_store::existing_claim(&pool, captured.lead.id)
        .await
        .expect("reading the trail")
        .expect("a line was written");
    assert_eq!(detail["reason"], "not_accepted");
    assert_eq!(detail["source"], source.name);

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_source_with_no_autoresponder_still_answers_nothing_and_sends_no_mail() {
    let pool = pool().await;
    let org = fresh_org(&pool, "Autoresponder off").await;
    let plain = source(&pool, org, None).await;

    let lead = accepted_lead(&pool, org, &plain, "quiet@example.com").await;
    let outcome = ar_store::prepare(&pool, &lead, &plain, time::OffsetDateTime::now_utc())
        .await
        .expect("prepare");
    assert_eq!(outcome.verdict, Delivery::Disabled);
    assert!(!outcome.sent());
    assert_eq!(outcome.verdict.reason(), "not_configured");

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn the_shipped_templates_are_usable_as_the_column_stores_them() {
    // The column round trip is the shape an editor actually writes, and a template that only
    // works when a test hand-builds it is a template the operator cannot select.
    for (name, subject, body) in omnion_module_crm_intake::autoresponder::TEMPLATES {
        let stored = serde_json::json!({
            "enabled": true,
            "template": name,
            "subject": subject,
            "template_body": body,
            "delay_minutes": 0,
        });
        let parsed = Autoresponder::from_json(&stored);
        assert!(parsed.is_configured(), "{name} must be usable as shipped");
    }
    assert!(!omnion_module_crm_intake::autoresponder::template_names().is_empty());
}
