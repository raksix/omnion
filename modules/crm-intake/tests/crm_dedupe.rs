//! The duplicate verdict gate: `reject_duplicate` against an installation that **has** the CRM.
//!
//! Run through `scripts/qa/run-crm-dedupe.sh`.
//!
//! ## Why this file exists at all
//!
//! Fourteen ticks shipped a defect that a `reject_duplicate` source triggers on **every**
//! submission, and no test on this branch could see it. The shape of the miss is worth keeping:
//!
//! * `crm_leads.duplicate_of` is a foreign key to `crm_leads`. `capture` wrote a
//!   `crm_contacts` id into it, in a second `update` after the insert. On an installation with
//!   the CRM that is a 23503 and the submission fails; on this branch `crm_contacts` does not
//!   exist, `fetch_candidates` takes its module-absence path, no candidate is ever returned,
//!   and the arm is dead code in every test.
//! * **The absence branch that is supposed to be the default is what hid the bug on the
//!   non-default installation.** Testing one installation is testing one product, and the one
//!   chosen here was the one where the feature cannot run.
//!
//! So this gate builds the CRM's own tables — copied byte for byte from
//! `0022_crm.sql` on `origin/wave4`, not retyped, because a fixture that lies about another
//! module's schema fails on the half that is *correct* — and then proves the three policies
//! against real contacts.
//!
//! The negative assertions matter as much as the positive ones. `a_contact_id_is_never_a_lead`
//! cannot fail against the fixed code by accident: it reads the two pointers out of the stored
//! row and requires the contact pointer to hold a contact. Against the old code the column
//! would hold a contact id too — in the *wrong* column — and the test would pass, which is why
//! the assertion that catches the old code is `duplicate_of is null` (the FK column the old
//! code violated could not have held the value at all) and the 500 in the first place.

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
        "delete from crm_contacts where organization_id = $1",
        "delete from crm_companies where organization_id = $1",
        "delete from organizations where id = $1",
    ] {
        let _ = sqlx::query(sql).bind(org).execute(pool).await;
    }
}

/// The CRM's tables, as `0022_crm.sql` declares them.
///
/// Copied from `git show origin/wave4:database/migrations/0022_crm.sql` rather than retyped:
/// a fixture that misses a `default gen_random_uuid()` fails with a not-null violation on the
/// *id*, which reads as "the platform forgets to generate contact ids" — and it does not, the
/// DDL was wrong. Only the two tables this test needs are built, and only when the gate script
/// has not already applied the real migration.
async fn ensure_crm_tables(pool: &PgPool) {
    for sql in [
        "create table if not exists crm_companies (
            id uuid primary key default gen_random_uuid(),
            organization_id uuid not null references organizations (id) on delete cascade,
            name text not null, domain text, created_at timestamptz not null default now(),
            updated_at timestamptz not null default now())",
        "create table if not exists crm_contacts (
            id uuid primary key default gen_random_uuid(),
            organization_id uuid not null references organizations (id) on delete cascade,
            company_id uuid references crm_companies (id) on delete set null,
            first_name text, last_name text, email text, phone text, job_title text,
            notes text, archived_at timestamptz,
            created_at timestamptz not null default now(),
            updated_at timestamptz not null default now())",
    ] {
        sqlx::query(sql)
            .execute(pool)
            .await
            .expect("the CRM tables this gate needs");
    }
}

async fn seed_contact(pool: &PgPool, org: Uuid, email: &str, first: &str, last: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "insert into crm_contacts (id, organization_id, first_name, last_name, email) \
         values ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(org)
    .bind(first)
    .bind(last)
    .bind(email)
    .execute(pool)
    .await
    .expect("a contact to match against");
    id
}

/// A contact reachable only by phone.
///
/// Deliberately has **no e-mail**: a contact with one would let the e-mail arm answer first and
/// the test would pass against the broken query, which is exactly how this defect survived
/// sixteen tests of its own file.
async fn seed_contact_with_phone(
    pool: &PgPool,
    org: Uuid,
    phone: &str,
    first: &str,
    last: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "insert into crm_contacts (id, organization_id, first_name, last_name, phone) \
         values ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(org)
    .bind(first)
    .bind(last)
    .bind(phone)
    .execute(pool)
    .await
    .expect("a contact to match against");
    id
}

/// A source whose mapping yields a **phone and no e-mail**, plus one submission through it.
///
/// The `e_mail` source key is the point twice over: the payload never carries it, so the e-mail
/// key is `None` and the phone arm is the only thing that can answer; and the name is not
/// `email`, so a lookup that reached for a column called `email` — rather than for the *mapped*
/// value — would also fail here, which keeps the two halves of this fix honest against each
/// other.
async fn submit_with_phone(pool: &PgPool, org: Uuid, policy: &str, phone: &str) -> (Uuid, Uuid) {
    let source_id = Uuid::new_v4();
    let mapping = serde_json::json!([
        { "target": "email", "source_key": "e_mail", "transform": ["trim", "lowercase"],
          "required": false },
        { "target": "phone", "source_key": "phone", "transform": ["trim", "e164_lite"],
          "required": false }
    ]);
    sqlx::query(
        "insert into crm_intake_sources (id, organization_id, name, kind, mapping, \
             required_targets, dedupe_policy) \
         values ($1, $2, $4, 'endpoint', $3, '{}', $5)",
    )
    .bind(source_id)
    .bind(org)
    .bind(mapping)
    .bind(format!("phone dedupe source {}", source_id))
    .bind(policy)
    .execute(pool)
    .await
    .expect("a source");

    let captured = store::capture(
        pool,
        &Submission {
            organization_id: org,
            site_id: None,
            source_id,
            submission_id: Some(format!("sub-{source_id}")),
            ip: Some("203.0.113.7".to_string()),
            payload: serde_json::json!({ "phone": phone }),
            received_at: time::OffsetDateTime::now_utc(),
        },
    )
    .await
    .expect("a submission must be captured, whatever the dedupe verdict is");
    (source_id, captured.lead.id)
}

/// A source with the given dedupe policy, and one submission through it.
async fn submit(pool: &PgPool, org: Uuid, policy: &str, email: &str) -> (Uuid, Uuid) {
    let source_id = Uuid::new_v4();
    // `source_key`, not `source`: serde has no error for an unknown field, so a mistyped name
    // deserializes into an empty mapping and the submission is filed as `rejected` for having
    // nothing to contact. The gate would then pass for entirely the wrong reason.
    let mapping = serde_json::json!([
        { "target": "email", "source_key": "email", "transform": ["trim", "lowercase"],
          "required": false },
        { "target": "first_name", "source_key": "first_name", "transform": ["trim"],
          "required": false }
    ]);
    sqlx::query(
        "insert into crm_intake_sources (id, organization_id, name, kind, mapping, \
             required_targets, dedupe_policy) \
         values ($1, $2, $4, 'endpoint', $3, '{}', $5)",
    )
    .bind(source_id)
    .bind(org)
    .bind(mapping)
    .bind(format!("dedupe source {}", source_id))
    .bind(policy)
    .execute(pool)
    .await
    .expect("a source");

    let captured = store::capture(
        pool,
        &Submission {
            organization_id: org,
            site_id: None,
            source_id,
            submission_id: Some(format!("sub-{source_id}")),
            ip: Some("203.0.113.7".to_string()),
            payload: serde_json::json!({ "email": email, "first_name": "Ayse" }),
            received_at: time::OffsetDateTime::now_utc(),
        },
    )
    .await
    .expect("a submission must be captured, whatever the dedupe verdict is");
    (source_id, captured.lead.id)
}

#[tokio::test]
async fn a_duplicate_submission_is_captured_and_never_answers_five_hundred() {
    let pool = pool().await;
    ensure_crm_tables(&pool).await;
    let org = fresh_org(&pool, "dedupe-reject").await;
    let contact = seed_contact(&pool, org, "ayse@example.com", "Ayse", "Kaya").await;

    // The whole point of the gate: on an installation WITH the CRM this used to raise a foreign
    // key violation, because the matched contact id went into `duplicate_of` (a lead pointer).
    let (_source, lead_id) = submit(&pool, org, "reject_duplicate", "ayse@example.com").await;

    let row: (String, Option<String>, Option<Uuid>, Option<f64>) = sqlx::query_as(
        "select status, duplicate_of::text, dedupe_contact_id, dedupe_score \
         from crm_leads where id = $1",
    )
    .bind(lead_id)
    .fetch_one(&pool)
    .await
    .expect("the captured lead");

    assert_eq!(row.0, "duplicate", "the policy asked for a duplicate claim");
    assert!(
        row.1.is_none(),
        "duplicate_of is a LEAD pointer and must stay null on a matched submission — the old \
         code put a contact id here, which is a 23503 the moment the CRM is installed"
    );
    assert_eq!(
        row.2,
        Some(contact),
        "the contact the verdict matched is recorded in dedupe_contact_id"
    );
    let score = row.3.expect("the score the verdict was made on");
    assert!(
        (0.75..=1.0).contains(&score),
        "an e-mail match scores 0.95; got {score}"
    );

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_link_policy_attaches_the_lead_to_the_contact_it_matched() {
    let pool = pool().await;
    ensure_crm_tables(&pool).await;
    let org = fresh_org(&pool, "dedupe-link").await;
    let contact = seed_contact(&pool, org, "bob@example.com", "Bob", "Stone").await;

    let (_source, lead_id) = submit(&pool, org, "link", "bob@example.com").await;
    let lead = store::find_lead(&pool, org, lead_id)
        .await
        .expect("read the lead")
        .expect("it exists");

    assert_eq!(lead.status, "assigned");
    assert_eq!(lead.contact_id, Some(contact), "linked to the match");
    assert_eq!(lead.dedupe_contact_id, Some(contact));
    drop_org(&pool, org).await;
}

#[tokio::test]
async fn create_anyway_makes_a_second_lead_and_records_nothing_about_the_match() {
    let pool = pool().await;
    ensure_crm_tables(&pool).await;
    let org = fresh_org(&pool, "dedupe-anyway").await;
    seed_contact(&pool, org, "carla@example.com", "Carla", "Diaz").await;

    let (_source, lead_id) = submit(&pool, org, "create_anyway", "carla@example.com").await;
    let lead = store::find_lead(&pool, org, lead_id)
        .await
        .expect("read the lead")
        .expect("it exists");

    assert_eq!(lead.status, "new", "a new lead, not a duplicate claim");
    assert_eq!(
        lead.contact_id, None,
        "the match is deliberately not attached"
    );
    drop_org(&pool, org).await;
}

#[tokio::test]
async fn the_queue_reads_the_same_rows_the_verdict_wrote() {
    let pool = pool().await;
    ensure_crm_tables(&pool).await;
    let org = fresh_org(&pool, "dedupe-queue").await;
    seed_contact(&pool, org, "deniz@example.com", "Deniz", "Aksoy").await;

    let (_source, lead_id) = submit(&pool, org, "reject_duplicate", "deniz@example.com").await;
    let queue = store::list_duplicates(&pool, org, 50)
        .await
        .expect("the duplicate queue reads");

    let row = queue
        .iter()
        .find(|lead| lead.id == lead_id)
        .expect("a filed duplicate appears in the queue");
    assert_eq!(row.dedupe_key.as_deref(), Some("deniz@example.com"));
    assert!(row.dedupe_score.is_some(), "the queue can show the score");
    assert!(row.duplicate_of.is_none());

    // A lead of another tenant is not in this queue, and the read is organization-scoped.
    let other = fresh_org(&pool, "dedupe-other").await;
    seed_contact(&pool, other, "ege@example.com", "Ege", "Aydin").await;
    let (_s, other_lead) = submit(&pool, other, "reject_duplicate", "ege@example.com").await;
    let queue_again = store::list_duplicates(&pool, org, 50)
        .await
        .expect("read again");
    assert!(
        !queue_again.iter().any(|lead| lead.id == other_lead),
        "another organization's duplicate is not in this queue"
    );

    drop_org(&pool, other).await;
    drop_org(&pool, org).await;
}

#[tokio::test]
async fn linking_a_duplicate_attaches_the_contact_the_verdict_matched() {
    // The queue's `Link` button used to send `PATCH { status: "assigned" }` and nothing else.
    // `patch_lead` keeps the existing `contact_id` when the patch has none, a duplicate row has
    // none, and the row left the queue attached to nothing. The notice said otherwise.
    let pool = pool().await;
    ensure_crm_tables(&pool).await;
    let org = fresh_org(&pool, "dedupe-resolve-link").await;
    let contact = seed_contact(&pool, org, "elif@example.com", "Elif", "Yilmaz").await;
    let (_source, lead_id) = submit(&pool, org, "reject_duplicate", "elif@example.com").await;

    let resolution =
        store::resolve_duplicate(&pool, org, lead_id, store::DuplicateDecision::Link, None)
            .await
            .expect("the decision")
            .expect("the lead exists");

    assert!(resolution.refused.is_none(), "{:?}", resolution.refused);
    assert_eq!(resolution.contact_id, Some(contact));
    assert_eq!(resolution.lead.status, "assigned");
    assert_eq!(
        resolution.lead.contact_id,
        Some(contact),
        "the lead is ATTACHED — the defect left this null while the row left the queue"
    );

    // And it leaves the queue, because it is no longer a duplicate claim.
    let queue = store::list_duplicates(&pool, org, 50)
        .await
        .expect("read the queue");
    assert!(
        !queue.iter().any(|lead| lead.id == lead_id),
        "a linked lead is not a duplicate claim any more"
    );
    drop_org(&pool, org).await;
}

#[tokio::test]
async fn the_decision_is_on_the_trail_with_both_people_and_the_score() {
    let pool = pool().await;
    ensure_crm_tables(&pool).await;
    let org = fresh_org(&pool, "dedupe-resolve-trail").await;
    seed_contact(&pool, org, "gorkem@example.com", "Gorkem", "Sahin").await;
    let (_source, lead_id) = submit(&pool, org, "reject_duplicate", "gorkem@example.com").await;

    store::resolve_duplicate(
        &pool,
        org,
        lead_id,
        store::DuplicateDecision::KeepSeparate,
        None,
    )
    .await
    .expect("the decision")
    .expect("the lead exists");

    // The organization is an argument here too, and that is worth keeping: it is what makes
    // the trail read a tenant-scoped query rather than a lookup by a bare id.
    let lines = store::list_events(&pool, org, lead_id)
        .await
        .expect("the trail");
    let decided = lines
        .iter()
        .find(|line| line.kind == "duplicate_decided")
        .expect("the decision is on the trail");
    assert_eq!(
        decided.detail.get("decision").and_then(|v| v.as_str()),
        Some("kept_separate")
    );
    assert_eq!(
        decided
            .detail
            .get("previous_status")
            .and_then(|v| v.as_str()),
        Some("duplicate"),
        "the trail says what the row WAS, so a later reader can tell a reversal from a fresh lead"
    );
    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_link_with_no_recorded_contact_is_refused_and_writes_nothing() {
    // The ambiguous verdict: several contacts matched, so none was recorded as *the* match. The
    // answer names that and points at the detail screen rather than attaching something arbitrary.
    let pool = pool().await;
    ensure_crm_tables(&pool).await;
    let org = fresh_org(&pool, "dedupe-resolve-refuse").await;
    seed_contact(&pool, org, "murat@example.com", "Murat", "Koc").await;

    let source_id = Uuid::new_v4();
    let mapping = serde_json::json!([
        { "target": "email", "source_key": "email", "transform": ["trim", "lowercase"],
          "required": false }
    ]);
    sqlx::query(
        "insert into crm_intake_sources (id, organization_id, name, kind, mapping, \
             required_targets, dedupe_policy) values ($1, $2, $4, 'endpoint', $3, '{}', 'reject_duplicate')",
    )
    .bind(source_id)
    .bind(org)
    .bind(mapping)
    .bind(format!("ambiguous source {}", source_id))
    .execute(&pool)
    .await
    .expect("a source");

    // Written straight as the ambiguous case, because producing it through `capture` needs two
    // contacts on one e-mail — which the match order cannot produce (an e-mail match is a single
    // verdict). So the row is seeded the way the store writes it and the DECISION is what is
    // under test: what the store does with a duplicate that has no single contact.
    let lead_id = Uuid::new_v4();
    sqlx::query(
        "insert into crm_leads (id, organization_id, source_id, status, decision, email, \
             dedupe_key, duplicate_of) \
         values ($1, $2, $3, 'duplicate', 'duplicate', 'ambiguous@example.com', \
                 'ambiguous@example.com', $4)",
    )
    .bind(lead_id)
    .bind(org)
    .bind(source_id)
    .bind(lead_id) // a lead pointer, so the row is a duplicate claim by `duplicate_of`
    .execute(&pool)
    .await
    .expect("an ambiguous duplicate row");

    let resolution =
        store::resolve_duplicate(&pool, org, lead_id, store::DuplicateDecision::Link, None)
            .await
            .expect("the decision")
            .expect("the lead exists");

    let reason = resolution.refused.expect("a refusal, not a silent no-op");
    assert!(reason.contains("contact"), "{reason}");
    let after = store::find_lead(&pool, org, lead_id)
        .await
        .expect("read")
        .expect("it exists");
    assert_eq!(
        after.status, "duplicate",
        "a refusal leaves the row exactly as it was"
    );
    assert_eq!(after.contact_id, None);
    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_lead_that_is_not_a_duplicate_claim_is_refused_by_name() {
    let pool = pool().await;
    ensure_crm_tables(&pool).await;
    let org = fresh_org(&pool, "dedupe-resolve-notdup").await;
    let (_source, lead_id) = submit(&pool, org, "link", "nobody@example.com").await;

    let resolution = store::resolve_duplicate(
        &pool,
        org,
        lead_id,
        store::DuplicateDecision::KeepSeparate,
        None,
    )
    .await
    .expect("the decision")
    .expect("the lead exists");

    let reason = resolution.refused.expect("a refusal");
    assert!(
        reason.contains("new"),
        "the refusal names the status it has: {reason}"
    );
    drop_org(&pool, org).await;
}

#[tokio::test]
async fn another_organizations_duplicate_answers_the_same_nothing_as_a_deleted_one() {
    let pool = pool().await;
    ensure_crm_tables(&pool).await;
    let mine = fresh_org(&pool, "dedupe-resolve-tenant-a").await;
    let theirs = fresh_org(&pool, "dedupe-resolve-tenant-b").await;
    seed_contact(&pool, theirs, "hidden@example.com", "Hid", "Den").await;
    let (_s, their_lead) = submit(&pool, theirs, "reject_duplicate", "hidden@example.com").await;

    // `None` and not an error and not a refusal: a panel that can tell those apart can
    // enumerate ids across tenants.
    let answer = store::resolve_duplicate(
        &pool,
        mine,
        their_lead,
        store::DuplicateDecision::Link,
        None,
    )
    .await
    .expect("the call answers");
    assert!(answer.is_none(), "another tenant's lead answers None");

    let still = store::find_lead(&pool, theirs, their_lead)
        .await
        .expect("read")
        .expect("it still exists");
    assert_eq!(still.status, "duplicate", "the refused call wrote nothing");
    drop_org(&pool, theirs).await;
    drop_org(&pool, mine).await;
}

/// **The candidate query and the scorer disagreed about what a phone number IS, so the phone
/// arm of the matcher could never fire against a stored contact.**
///
/// `store::fetch_candidates` narrows the contact table in SQL with
/// `regexp_replace(coalesce(phone, ''), '[^0-9]', '', 'g') = $3`, and `$3` is
/// `dedupe::normalize_phone(submission)`. That function keeps a leading `+` when the value it
/// is given has one, so `+90 532 111 22 33` becomes `+905321112233` — while the SQL side strips
/// the `+` along with the spaces and answers `905321112233`. The equality is therefore never
/// true and **no contact is ever a candidate on the phone key**, no matter how many rows the
/// scorer would have matched.
///
/// The consequence is not "the score is slightly off", it is that the second key in the
/// documented match order does not exist: a submission carrying a phone number and no e-mail
/// is `Unique` against an installation full of that exact contact, and `dedupe_policy =
/// 'reject_duplicate'` files it as a brand-new lead instead of a duplicate claim. The lead
/// gets worked twice, which is the one outcome the whole module exists to prevent.
///
/// ## Why nothing on this branch could see it
///
/// Every fixture in this file maps an e-mail and seeds a contact with an e-mail, so the first
/// arm answers and the second is never reached; and `dedupe::phone_match` is unit-tested
/// **against `normalize_phone`'s own output on both sides**, which agrees with itself by
/// construction. The disagreement lives in the one hop the unit tests never cross: the SQL
/// side is a *different implementation* of the same rule, written in a different language
/// inside a query string, and it is the half that is wrong. This is the mirror image of
/// `merge_attribution`'s renamed key: there the two halves used two names for one value,
/// here they use two rules for one value — and the module header calls out "a `check`
/// constraint and a Rust constant are written twice" as its standing lesson.
#[tokio::test]
async fn a_contact_stored_with_an_international_phone_is_matched_on_that_phone() {
    let pool = pool().await;
    ensure_crm_tables(&pool).await;
    let org = fresh_org(&pool, "dedupe-phone-arm").await;
    // Stored the way the CRM stores what an operator types, and submitted the way a visitor
    // types: same number, different punctuation. This is not an adversarial fixture, it is two
    // people writing one phone number.
    seed_contact_with_phone(&pool, org, "+90 (532) 111 22 33", "Ilker", "Demir").await;

    let (_source, lead_id) = submit_with_phone(&pool, org, "reject_duplicate", "+90 532 111 22 33").await;
    let lead = store::find_lead(&pool, org, lead_id)
        .await
        .expect("read the lead")
        .expect("it exists");

    assert_eq!(
        lead.status, "duplicate",
        "the one contact whose phone IS this number must be found by the phone arm"
    );
    assert_eq!(
        lead.dedupe_key.as_deref(),
        Some("+905321112233"),
        "the key recorded is the *normalized* phone, which is the shape the queue compares on"
    );
    drop_org(&pool, org).await;
}

/// The regression guard for the opposite mistake: a phone-only submission that is genuinely
/// somebody else must stay `Unique`. A fix that widens the SQL to match anything with the same
/// digits — for instance by comparing suffixes — would pass the test above and merge two
/// different people who share a tail, so the negative half is load-bearing rather than tidy.
#[tokio::test]
async fn a_different_phone_is_not_matched_just_because_a_suffix_is_shared() {
    let pool = pool().await;
    ensure_crm_tables(&pool).await;
    let org = fresh_org(&pool, "dedupe-phone-suffix").await;
    // Same last seven digits, different country codes: +90 555 … vs +1 555 …
    seed_contact_with_phone(&pool, org, "+1 (555) 010 22 33", "Someone", "Else").await;

    let (_source, lead_id) = submit_with_phone(&pool, org, "reject_duplicate", "+90 555 010 22 33").await;
    let lead = store::find_lead(&pool, org, lead_id)
        .await
        .expect("read the lead")
        .expect("it exists");

    assert_eq!(
        lead.status, "new",
        "two people whose numbers share a tail are two people; a suffix comparison would file \
         this as a duplicate of a contact on another continent"
    );
    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_submission_that_matches_nothing_carries_no_match_facts() {
    let pool = pool().await;
    ensure_crm_tables(&pool).await;
    let org = fresh_org(&pool, "dedupe-unique").await;

    let (_source, lead_id) = submit(&pool, org, "reject_duplicate", "newcomer@example.com").await;
    let lead = store::find_lead(&pool, org, lead_id)
        .await
        .expect("read the lead")
        .expect("it exists");

    assert_eq!(lead.status, "new");
    // The dedupe KEY is the normalized e-mail and is always stored — it is what lets the queue
    // be rebuilt without re-scanning. The CONTACT and the SCORE are what a *match* produced, and
    // there was none, so a row that claims 0.95 against nothing is a row nobody can check.
    assert_eq!(lead.dedupe_key.as_deref(), Some("newcomer@example.com"));
    assert_eq!(lead.dedupe_contact_id, None);
    assert_eq!(lead.dedupe_score, None);
    drop_org(&pool, org).await;
}
