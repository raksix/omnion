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
    assert!(
        !ar_store::was_sent(
            &ar_store::existing_claim(&pool, lead.id)
                .await
                .expect("reading the claim")
                .expect("a claim exists")
        ),
        "an immediate claim has taken the slot, not delivered a message (migration 0202)"
    );

    // While the claim stands, the lead is answered. This is the guarantee that makes
    // releasing rather than merely logging the right repair: nothing re-answers in the window.
    let second = ar_store::prepare(&pool, &lead, &source, now)
        .await
        .expect("prepare");
    assert!(!second.sent(), "a claimed lead is not sent again");

    // The refusal. **This release is the assertion the test is named for, and the earlier
    // version of this file never made it** — it asserted only that the second attempt was
    // refused, which is true whether or not a release is possible, so a build that could
    // never recover passed it. The delay here is 0, i.e. the IMMEDIATE path: the old writer
    // stored that claim with `sent = true`, and both `release_claim` and `mark_sent` select on
    // `sent <> 'true'`, so the release matched no row and the lead was silenced for ever by
    // the mechanism whose whole job is to guarantee it is answered.
    assert!(
        ar_store::release_claim(&pool, lead.id, "retry@example.com")
            .await
            .expect("releasing a refused immediate send"),
        "a refused immediate send must be able to take its claim back"
    );

    // And the lead is answerable again, from the durable row rather than from the struct.
    assert!(
        ar_store::existing_claim(&pool, lead.id)
            .await
            .expect("reading the claim")
            .is_none(),
        "a released claim leaves no row to occupy the one slot a lead has"
    );
    let third = ar_store::prepare(&pool, &lead, &source, now)
        .await
        .expect("prepare");
    assert!(
        third.sent(),
        "after a release the next attempt owns the send again"
    );

    drop_org(&pool, org).await;
}

/// A delivered message is never released and never completed twice.
///
/// The negative control for the test above, and the assertion that keeps the fix from
/// over-correcting. `delivered_at` is what separates the two states, so a row that carries it
/// must be invisible to **both** `release_claim` and `mark_sent`: releasing it would let the
/// lead be answered twice, and completing it twice would write two delivery instants onto one
/// message. Under the old `sent <> 'true'` predicate both of these were vacuous for an
/// immediate send — they had nothing to select — so "the gate is not too eager" was true for
/// the wrong reason.
#[tokio::test]
async fn a_delivered_claim_is_neither_released_nor_completed_again() {
    let pool = pool().await;
    let org = fresh_org(&pool, "Autoresponder delivered").await;
    let source = source_with_autoresponder(&pool, org, 0).await;
    let lead = accepted_lead(&pool, org, &source, "delivered@example.com").await;
    let now = time::OffsetDateTime::now_utc();

    ar_store::prepare(&pool, &lead, &source, now)
        .await
        .expect("the slot is claimed");
    assert!(
        ar_store::mark_sent(&pool, lead.id, now).await.expect("the send completed"),
        "the mailer returned, so the completion wins the row"
    );

    let stored = ar_store::existing_claim(&pool, lead.id)
        .await
        .expect("reading the claim")
        .expect("a claim exists");
    assert!(ar_store::was_sent(&stored), "a delivered claim reads as sent");
    assert!(
        stored["delivered_at"].as_str().is_some(),
        "the delivery instant is recorded, which is the fact an immediate send used to lose"
    );

    // Both refusals, from the durable row's point of view.
    assert!(
        !ar_store::mark_sent(&pool, lead.id, now + time::Duration::minutes(1))
            .await
            .expect("the second completion"),
        "a message already delivered is not delivered twice"
    );
    assert!(
        !ar_store::release_claim(&pool, lead.id, "delivered@example.com")
            .await
            .expect("releasing a delivered send"),
        "a delivered message must not be released back into the queue"
    );
    let after = ar_store::existing_claim(&pool, lead.id)
        .await
        .expect("reading the claim")
        .expect("the claim is still standing");
    assert_eq!(
        after["delivered_at"], stored["delivered_at"],
        "neither attempt may rewrite the recorded delivery instant"
    );

    // And the lead is still answered, which is the promise the two refusals exist for.
    assert!(
        !ar_store::prepare(&pool, &lead, &source, now)
            .await
            .expect("prepare")
            .sent(),
        "a delivered lead is answered once and stays answered"
    );

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

    // A delayed autoresponder *reserves* at capture time, and the reservation is the whole
    // mechanism: the worker completes it when the delay elapses, it never claims one. The
    // earlier version of this test asserted the opposite ("a delayed message must not claim
    // at capture time") and it was right about what the code did and wrong about what the
    // code should do — a control that reserves nothing is a control whose reply never goes
    // out, on exactly the sources that asked for it to wait.
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
    let reservation = ar_store::existing_claim(&pool, other.id)
        .await
        .expect("reading the claim")
        .expect("a delayed message reserves its slot at capture time");
    // `sent` is a JSON boolean, not the string "false" — `was_sent` reads it with
    // `as_bool`, and asserting the string form passes for the wrong reason on one shape and
    // fails on another.
    assert_eq!(
        reservation["sent"],
        serde_json::json!(false),
        "a reservation reads as pending, never as sent"
    );
    assert!(
        !ar_store::was_sent(&reservation),
        "a reserved message has not been sent"
    );
    let due_at = reservation["due_at"]
        .as_str()
        .expect("`due_at` is stored as a formatted string, not a serde component array");
    assert!(
        !due_at.is_empty(),
        "a reservation names the instant it becomes due, or the worker cannot find it"
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
    // nothing") and the one that can carry an address. The `rejected` (non-spam) case is
    // **not** driven here on purpose, and not because of a limitation: this file's subject is
    // the autoresponder's *verdicts*, and a rejected row with no address is proven end to end —
    // written, stored, claimed, and repairable — by `tests/crm_verdict_rows.rs`. Repeating it
    // here would run the same fixture in a second database and prove the same line twice.
    //
    // (Until migration `0159` this note was a workaround: `crm_leads_contactable_check` refused
    // a row with neither e-mail nor phone at the database, so the rejected case genuinely could
    // not be written here. The sentence is kept because it is the reason the test is shaped
    // this way, and because "unreachable on purpose" was exactly how a real defect hid for
    // twenty ticks.)
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
    // The skip is a note, not a claim: `existing_claim` deliberately ignores rows without a
    // `sent` key, so the reason has to be read from the row that carries it.
    let detail: serde_json::Value = sqlx::query_scalar(
        "select detail from crm_lead_events \
         where lead_id = $1 and kind = $2 and detail ? 'reason' \
         order by id desc limit 1",
    )
    .bind(captured.lead.id)
    .bind(ar_store::SENT_KIND)
    .fetch_one(&pool)
    .await
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

/// Rewrite a source's autoresponder delay, through the real column.
///
/// The gate has to move a reservation's due instant into the past without waiting for a real
/// minute to pass, and it must do it through the same JSON the editor writes — a test that
/// rewrote the column with a different shape would prove the sweep works against a row no
/// operator can create.
async fn set_delay(pool: &PgPool, source: &IntakeSource, delay_minutes: i64) {
    let column = sqlx::query_scalar::<_, serde_json::Value>(
        "select autoresponder from crm_intake_sources where id = $1",
    )
    .bind(source.id)
    .fetch_one(pool)
    .await
    .expect("reading the source's autoresponder column");
    let mut column = column;
    column["delay_minutes"] = serde_json::json!(delay_minutes);
    sqlx::query("update crm_intake_sources set autoresponder = $2 where id = $1")
        .bind(source.id)
        .bind(&column)
        .execute(pool)
        .await
        .expect("the delay is written back");
}

#[tokio::test]
async fn a_delayed_autoresponder_is_sent_when_its_time_comes() {
    // The defect this test exists for. A send delay is a *promise about when*, and the code
    // that made it a promise was a reservation nothing ever completed: `prepare` skipped the
    // claim for a delayed message, so the sweep had no row to find and the reply never went
    // out. Every unit test was green — the pure half produced the right verdict, the delayed
    // `Message` carried the right `due_at` — and the feature was dead on the one path where
    // deadness shows.
    let pool = pool().await;
    let org = fresh_org(&pool, "Autoresponder delay").await;
    let source = source_with_autoresponder(&pool, org, 45).await;
    let lead = accepted_lead(&pool, org, &source, "patient@example.com").await;
    let now = time::OffsetDateTime::now_utc();

    let outcome = ar_store::prepare(&pool, &lead, &source, now)
        .await
        .expect("prepare");
    assert!(matches!(outcome.verdict, Delivery::Ready(ref m) if m.delayed));
    assert_eq!(outcome.verdict.reason(), "sent");

    // Before the delay, the sweep finds nothing. This is the half that says "not early",
    // and it is the half a "just send it" implementation passes by accident.
    let early = ar_store::due_reservations(&pool, now + time::Duration::minutes(44), 50)
        .await
        .expect("the sweep runs");
    assert!(
        !early.iter().any(|r| r.lead.id == lead.id),
        "a message 44 minutes into a 45-minute delay is not due"
    );

    // After it, exactly one due row — for this lead.
    let due = ar_store::due_reservations(&pool, now + time::Duration::minutes(46), 50)
        .await
        .expect("the sweep runs");
    let mine: Vec<_> = due.iter().filter(|r| r.lead.id == lead.id).collect();
    assert_eq!(
        mine.len(),
        1,
        "a due delay is offered to the mailer exactly once"
    );
    assert_eq!(mine[0].message.to, "patient@example.com");
    assert!(
        !mine[0].message.body.trim().is_empty(),
        "the re-rendered message must carry a body, or the mailer sends an empty letter"
    );
    assert_eq!(mine[0].source.id, source.id);

    // The completion is what turns the reservation into a send, and it is one-shot: the
    // second worker's pass must not find the same row again.
    assert!(
        ar_store::mark_sent(&pool, lead.id, now + time::Duration::minutes(46))
            .await
            .expect("marking the reservation sent"),
        "the first completion wins"
    );
    assert!(
        !ar_store::mark_sent(&pool, lead.id, now + time::Duration::minutes(47))
            .await
            .expect("the second completion is not an error, it is a loss"),
        "a second worker must not also mark it sent"
    );
    let after = ar_store::due_reservations(&pool, now + time::Duration::minutes(60), 50)
        .await
        .expect("the sweep runs");
    assert!(
        !after.iter().any(|r| r.lead.id == lead.id),
        "a completed reservation is never offered twice"
    );

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_reservation_whose_source_was_deleted_is_released_rather_than_retried_forever() {
    // The THIRD arm that can drop out of `due_reservations`, and the only one whose `continue`
    // is justified — a reservation whose lead or source is gone is *not an error*, and the
    // comment above it says so and gives the reason: "a sweep that refused to move on would
    // retry the same dead row on every tick for ever."
    //
    // The reasoning is exactly right, and the code does the opposite. Both of these arms
    // `continue` WITHOUT releasing, so the claim row stays `sent: false` with a `due_at` in
    // the past — which is the sweep's own WHERE clause. Two things follow, and both are
    // permanent:
    //
    //   1. `crm_lead_autoresponder_due_idx` keeps a permanently-due row that every pass
    //      re-reads, and every pass writes a `tracing::debug!` line naming it. That is a line
    //      per minute, forever, per orphaned reservation.
    //   2. Worse, it is a *starvation* bug and not only a noise bug. `due_reservations` is
    //      `order by due_at asc limit 50`, and the dead rows are the OLDEST — they have been
    //      due the longest. So orphans sort to the front of every batch and consume its
    //      budget: a table with 50 orphaned reservations ahead of a live one means the live
    //      one is never offered, on every tick, for ever.
    //
    // `crm_leads.source_id` is `on delete set null` (0055), so the reachable shape is not
    // exotic: `DELETE /crm/intake/sources/{id}` — a button the panel has — nulls it, and the
    // reservation outlives the source. Deleting a source is the ordinary way an operator
    // retires a form, and it silently stops the autoresponder worker for that lead.
    let pool = pool().await;
    let org = fresh_org(&pool, "Autoresponder source deleted").await;
    let source = source_with_autoresponder(&pool, org, 30).await;
    let lead = accepted_lead(&pool, org, &source, "orphaned@example.com").await;
    let now = time::OffsetDateTime::now_utc();

    ar_store::prepare(&pool, &lead, &source, now)
        .await
        .expect("the slot is reserved");

    // The panel's own delete button: the lead keeps its row and loses its source.
    assert!(
        store::delete_source(&pool, org, source.id).await.expect("the delete runs"),
        "the source is deleted the way the panel deletes one"
    );
    let source_id: Option<Uuid> = sqlx::query_scalar("select source_id from crm_leads where id = $1")
        .bind(lead.id)
        .fetch_one(&pool)
        .await
        .expect("reading the lead");
    assert_eq!(
        source_id, None,
        "the fixture must reproduce the shape this test is about: a lead whose source is gone"
    );

    // Two passes, one minute apart — the worker's own cadence.
    for minute in [31_i64, 32] {
        let due = ar_store::due_reservations(
            &pool,
            now + time::Duration::minutes(minute),
            50,
        )
        .await
        .expect("the sweep runs");
        assert!(
            !due.iter().any(|r| r.lead.id == lead.id),
            "there is no source left to answer it (pass at +{minute})"
        );
    }

    // THE ASSERTION: the orphan is *released*. A dead row is exactly the case the comment
    // names — a note would be noise, but leaving the row is the retry-forever the comment
    // refuses, and it is a live reservation occupying a slot in the batch ahead of real work.
    assert!(
        ar_store::existing_claim(&pool, lead.id)
            .await
            .expect("reading the claim")
            .is_none(),
        "a reservation whose source was deleted must be released, not left permanently due"
    );

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn orphaned_reservations_do_not_starve_the_batch_the_worker_takes() {
    // The reason the release above matters beyond tidiness, measured rather than argued: the
    // sweep is `order by due_at asc, id asc limit N`, so a permanently-due orphan is always
    // among the *oldest* and therefore always in front of the live work. This test fills the
    // batch with orphans, leaves exactly one real reservation behind them, and asserts the
    // live one is offered by the **second** pass.
    //
    // **The second pass is the assertion, and getting this wrong is what the first version of
    // this test did twice.** It reserved the LIVE lead first, so it sorted ahead of the
    // orphans on both keys and passed against unreleased code. Fixed that, it then asserted
    // recovery inside ONE pass — which is a promise the fix does not make and cannot: the
    // `limit` is applied by Postgres *before* Rust sees a row, so a batch of orphans is spent
    // on orphans on the tick that releases them, whatever the loop does. What a release buys is
    // that the *next* tick is clean. That is the whole value and it is the only thing
    // asserted here.
    //
    // So: pass 1 spends its budget on the orphans (and ends them), pass 2 offers the live
    // lead. Against unreleased code both passes return the same five orphans and the live
    // lead is never offered at all — which is the defect, measured.
    const BATCH: i64 = 5;
    let pool = pool().await;
    let org = fresh_org(&pool, "Autoresponder orphan starve").await;
    let source = source_with_autoresponder(&pool, org, 30).await;
    let now = time::OffsetDateTime::now_utc();

    // The orphans: reserved, then their sources deleted out from under them. Each is due at
    // `now + 30`, and each is therefore permanently due unless something releases it.
    for index in 0..BATCH {
        let orphan_source = source_with_autoresponder(&pool, org, 30).await;
        let orphan = accepted_lead(
            &pool,
            org,
            &orphan_source,
            &format!("orphan-{index}@example.com"),
        )
        .await;
        ar_store::prepare(&pool, &orphan, &orphan_source, now)
            .await
            .expect("the orphan slot is reserved");
        assert!(
            store::delete_source(&pool, org, orphan_source.id)
                .await
                .expect("the orphan's source is deleted"),
            "an orphan is a reservation whose source is gone"
        );
    }

    // The live lead arrives AFTER the orphans were already due, so `order by due_at asc` puts
    // every one of them ahead of it and nothing else in the batch may reach it.
    let live = accepted_lead(&pool, org, &source, "live@example.com").await;
    ar_store::prepare(&pool, &live, &source, now + time::Duration::minutes(1))
        .await
        .expect("the live slot is reserved");

    // The sweep is a minute past both instants — the worker's own cadence.
    let sweep = || now + time::Duration::minutes(32);

    // Pass one. Postgres spends the whole `limit` on the five orphans — they are the oldest
    // rows in the table — and the loop then drops every one of them, because there is no lead
    // and no source left to answer. So the batch returns **nothing**: five rows read, zero
    // sendable, and the live lead sitting behind all of them never offered. That zero is the
    // measurement. The SQL took the batch before the loop ran, which is why recovery is
    // asserted on the *next* pass rather than this one.
    let first = ar_store::due_reservations(&pool, sweep(), BATCH)
        .await
        .expect("the first sweep runs");
    assert!(
        first.is_empty(),
        "the first sweep reads a full batch of orphans and returns none of them: it carried {} \
         sendable rows, and the live lead was not among them",
        first.len()
    );

    // Pass two — one worker tick later, the ordinary cadence. The orphans are gone from the
    // table, so the batch is available to the lead that is actually waiting. Against
    // unreleased code this second pass returns the same five orphans and the live lead is
    // never offered at all.
    let second = ar_store::due_reservations(&pool, sweep(), BATCH)
        .await
        .expect("the second sweep runs");
    assert!(
        second.iter().any(|r| r.lead.id == live.id),
        "a released orphan stops occupying the batch, so the live lead is offered by the next \
         pass: the second sweep carried {} rows and none was the live lead",
        second.len()
    );

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_source_switched_off_inside_its_delay_is_not_answered() {
    // An operator who turns the autoresponder off has asked for the silence to be real. The
    // message is re-rendered at send time rather than stored on the claim precisely so that
    // this holds: a reservation made an hour ago does not keep a switched-off source talking.
    let pool = pool().await;
    let org = fresh_org(&pool, "Autoresponder off mid-delay").await;
    let source = source_with_autoresponder(&pool, org, 30).await;
    let lead = accepted_lead(&pool, org, &source, "cancelled@example.com").await;
    let now = time::OffsetDateTime::now_utc();

    ar_store::prepare(&pool, &lead, &source, now)
        .await
        .expect("the slot is reserved");

    sqlx::query("update crm_intake_sources set autoresponder = $2 where id = $1")
        .bind(source.id)
        .bind(serde_json::json!({ "enabled": false }))
        .execute(&pool)
        .await
        .expect("the source is switched off");

    let due = ar_store::due_reservations(&pool, now + time::Duration::minutes(31), 50)
        .await
        .expect("the sweep runs");
    assert!(
        !due.iter().any(|r| r.lead.id == lead.id),
        "a switched-off source must not answer its reservation"
    );

    // And the trail says why, so the lead's timeline does not show a pending reply for ever.
    // The reason lives on the *skip* line, which is a different row from the claim — the
    // claim is the reservation, the skip is the note explaining it was abandoned. Reading
    // them through one function is exactly the conflation this file's readers now avoid.
    let note: serde_json::Value = sqlx::query_scalar(
        "select detail from crm_lead_events \
         where lead_id = $1 and kind = $2 and detail ? 'reason' \
         order by id desc limit 1",
    )
    .bind(lead.id)
    .bind(ar_store::SENT_KIND)
    .fetch_one(&pool)
    .await
    .expect("the skip line was written");
    // `source_disabled`, not `not_configured`: the two are different facts and the trail is
    // the only place an operator can tell them apart. "not_configured" is what the pure half
    // says when asked at capture time; "source_disabled" says the operator changed their mind
    // in the hour between the reservation and the send, which is a different repair.
    assert_eq!(note["reason"], "source_disabled");

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_lead_that_became_spam_inside_its_delay_is_not_answered() {
    // The reservation was made when the submission looked fine. The platform later decides it
    // is spam, and mailing a spammer an acknowledgement is how a form ends up on a blocklist.
    let pool = pool().await;
    let org = fresh_org(&pool, "Autoresponder late spam").await;
    let source = source_with_autoresponder(&pool, org, 30).await;
    let lead = accepted_lead(&pool, org, &source, "late-spam@example.com").await;
    let now = time::OffsetDateTime::now_utc();

    ar_store::prepare(&pool, &lead, &source, now)
        .await
        .expect("the slot is reserved");

    sqlx::query("update crm_leads set status = 'spam' where id = $1")
        .bind(lead.id)
        .execute(&pool)
        .await
        .expect("the lead is marked as spam");
    let lead = store::find_lead(&pool, org, lead.id)
        .await
        .expect("reading the lead back")
        .expect("the lead is still there");

    let due = ar_store::due_reservations(&pool, now + time::Duration::minutes(31), 50)
        .await
        .expect("the sweep runs");
    assert!(
        !due.iter().any(|r| r.lead.id == lead.id),
        "a lead that turned to spam must not be answered"
    );
    let note: serde_json::Value = sqlx::query_scalar(
        "select detail from crm_lead_events \
         where lead_id = $1 and kind = $2 and detail ? 'reason' \
         order by id desc limit 1",
    )
    .bind(lead.id)
    .bind(ar_store::SENT_KIND)
    .fetch_one(&pool)
    .await
    .expect("the skip line was written");
    assert_eq!(note["reason"], "not_accepted");

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_delay_shortened_to_zero_is_due_at_once() {
    // The operator edits the source from "answer in an hour" to "answer now" and expects the
    // waiting visitor to be answered, not to keep waiting out the old hour.
    let pool = pool().await;
    let org = fresh_org(&pool, "Autoresponder shortened").await;
    let source = source_with_autoresponder(&pool, org, 120).await;
    let lead = accepted_lead(&pool, org, &source, "impatient@example.com").await;
    let now = time::OffsetDateTime::now_utc();

    ar_store::prepare(&pool, &lead, &source, now)
        .await
        .expect("the slot is reserved for two hours");
    assert!(ar_store::due_reservations(&pool, now + time::Duration::minutes(119), 50)
        .await
        .expect("the sweep runs")
        .iter()
        .all(|r| r.lead.id != lead.id));

    set_delay(&pool, &source, 0).await;
    let due = ar_store::due_reservations(&pool, now + time::Duration::minutes(121), 50)
        .await
        .expect("the sweep runs");
    let mine: Vec<_> = due.iter().filter(|r| r.lead.id == lead.id).collect();
    assert_eq!(
        mine.len(),
        1,
        "a shortened delay is honoured on the reservation that is already waiting"
    );
    assert!(
        !mine[0].message.delayed,
        "the re-rendered message follows the source's *current* delay"
    );

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_reservation_the_sweep_declines_is_released_rather_than_left_forever_due() {
    // The sweep has TWO ways to decline a due reservation — a source switched off inside the
    // delay, and a lead that turned to spam — and both were answered with a *note* and neither
    // with a *release*. The claim row therefore stayed exactly as the sweep found it: `sent:
    // false`, a `due_at` in the past, which is the sweep's own WHERE clause.
    //
    // So the same reservation is offered again on the next tick, declined again on the same
    // grounds, and noted again — once a minute, forever. The trail this REQ's detail screen
    // reads grows without bound from a lead nobody is going to answer, and
    // `crm_lead_autoresponder_due_idx` keeps a permanently-due row that every pass re-reads.
    let pool = pool().await;
    let org = fresh_org(&pool, "Autoresponder declined").await;
    let source = source_with_autoresponder(&pool, org, 30).await;
    let lead = accepted_lead(&pool, org, &source, "declined@example.com").await;
    let now = time::OffsetDateTime::now_utc();

    ar_store::prepare(&pool, &lead, &source, now)
        .await
        .expect("the slot is reserved");

    // Declined for the second reason: the lead turns to spam inside the delay.
    sqlx::query("update crm_leads set status = 'spam' where id = $1")
        .bind(lead.id)
        .execute(&pool)
        .await
        .expect("the lead is marked as spam");

    // Two passes, one minute apart — the worker's own cadence.
    for minute in [31_i64, 32] {
        let due = ar_store::due_reservations(
            &pool,
            now + time::Duration::minutes(minute),
            50,
        )
        .await
        .expect("the sweep runs");
        assert!(
            !due.iter().any(|r| r.lead.id == lead.id),
            "a lead that turned to spam is not answered (pass at +{minute})"
        );
    }

    // THE ASSERTION: a declined reservation is *released*. A note is not a release — the
    // claim row is what occupies the one slot a lead has, and what the sweep keeps re-reading.
    assert!(
        ar_store::existing_claim(&pool, lead.id)
            .await
            .expect("reading the claim")
            .is_none(),
        "a reservation the sweep declined must be released, not left pending for ever"
    );

    // And therefore the decline is recorded ONCE. Two passes, one fact.
    let notes: i64 = sqlx::query_scalar(
        "select count(*) from crm_lead_events \
         where lead_id = $1 and kind = $2 and detail->>'reason' = 'not_accepted'",
    )
    .bind(lead.id)
    .bind(ar_store::SENT_KIND)
    .fetch_one(&pool)
    .await
    .expect("the skip line was written");
    assert_eq!(
        notes, 1,
        "one decline is one fact; a reservation left pending re-records it every tick"
    );

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_reservation_of_a_source_switched_off_inside_its_delay_is_released() {
    // The same defect on the other decline path. This one `continue`s *before* the message is
    // rendered, so it never reaches the arm that mentions a release at all — and the test that
    // already exists (`a_source_switched_off_inside_its_delay_is_not_answered`) asserts the
    // lead is not answered, which is true whether or not the claim was taken back.
    let pool = pool().await;
    let org = fresh_org(&pool, "Autoresponder off, released").await;
    let source = source_with_autoresponder(&pool, org, 30).await;
    let lead = accepted_lead(&pool, org, &source, "switched-off@example.com").await;
    let now = time::OffsetDateTime::now_utc();

    ar_store::prepare(&pool, &lead, &source, now)
        .await
        .expect("the slot is reserved");

    sqlx::query("update crm_intake_sources set autoresponder = $2 where id = $1")
        .bind(source.id)
        .bind(serde_json::json!({ "enabled": false }))
        .execute(&pool)
        .await
        .expect("the source is switched off");

    let due = ar_store::due_reservations(&pool, now + time::Duration::minutes(31), 50)
        .await
        .expect("the sweep runs");
    assert!(!due.iter().any(|r| r.lead.id == lead.id));

    assert!(
        ar_store::existing_claim(&pool, lead.id)
            .await
            .expect("reading the claim")
            .is_none(),
        "a source that will never answer must not leave its reservation pending for ever"
    );

    drop_org(&pool, org).await;
}
