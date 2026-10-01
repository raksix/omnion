//! The first-touch gate: the *mapped* address decides whose first touch is kept.
//!
//! Run through `scripts/qa/run-crm-attribution.sh`.
//!
//! ## The defect this file exists for
//!
//! REQ-117 acceptance 6: "An accepted submission with UTM parameters stores first-touch and
//! last-touch attribution, the referrer host and the landing path; **a second submission from
//! the same visitor keeps the original first touch**."
//!
//! `Attribution::merge_first_touch` implements the second sentence correctly and had a unit
//! test for it since slice 1. The defect was one level up, in `store::merge_attribution`,
//! which decides *whose* first touch to merge:
//!
//! ```text
//! let Some(key) = submission.payload.get("email")            // the RAW payload's key
//!                  .and_then(Value::as_str)
//!                  .and_then(|v| dedupe::normalize_email(Some(v)))
//! else { return Ok(later.clone()) };
//!
//! select … from crm_leads where … and lower(email) = $3
//! ```
//!
//! Two names for one value, one level apart. The lead row is written from **`mapped`**
//! (`insert_lead` binds `mapped.get("email")`), and the lookup keyed on **`payload["email"]`**.
//! Every source whose form calls the field anything other than `email` — `e_mail`,
//! `contact_email`, `eposta`, `your_email` — found nothing, so `merge_attribution` returned
//! the later visit untouched and the second submission **overwrote the campaign that first
//! brought the visitor in**.
//!
//! It survived because every fixture in this crate maps `email` from a key called `email`, so
//! the two names coincided and the wrong lookup answered the right question. The unit test
//! could not see it either: it drove `merge_first_touch` directly with hand-built
//! `Attribution` values and never went near the lookup that decides whose row to read.
//!
//! ## The shape worth recording
//!
//! This is the **fifth** time this module has shipped a function with the right answer, a
//! unit test over it, a column, a screen that renders it — and no caller that could ever
//! produce the state it describes. Here the *unit-tested* half was correct and the *uncalled*
//! half was the defect, which is the mirror image of the binding-health and round-robin cases
//! and worth naming: a test on the pure function is not a test of the impure one that
//! decides when to apply it. **A test that cannot fail is not a test** — and the cheapest way
//! to write one is to name a source key that differs from the target.
//!
//! Every test below goes through `store::capture` and reads the **stored row** back, because
//! a defect about which row is consulted is invisible to anything that only looks at the
//! struct that did the writing.

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

async fn source_with(pool: &PgPool, org: Uuid, mapping: Vec<MappingEntry>) -> Uuid {
    let label = format!("attribution {}", Uuid::new_v4().simple());
    let draft = NewIntakeSource::endpoint(org, &label, None).with_mapping(mapping, vec![]);
    let (created, _key) = store::create_source(pool, &draft).await.expect("a source");
    created.id
}

fn submission(org: Uuid, source_id: Uuid, payload: serde_json::Value) -> Submission {
    Submission {
        organization_id: org,
        site_id: None,
        source_id,
        submission_id: Some(Uuid::new_v4().to_string()),
        ip: Some("203.0.113.44".to_string()),
        payload,
        received_at: time::OffsetDateTime::now_utc(),
    }
}

/// The attribution columns **as the database holds them**, per lead, oldest first.
///
/// The struct is not evidence: this crate has been bitten by trusting it twice (`OffsetDateTime`
/// serialised as a component array, a skip line read as a sent one). The defect was *which row
/// the merge consulted*, so the only place it is visible is the rows themselves.
async fn stored_touches(pool: &PgPool, org: Uuid) -> Vec<(Option<String>, Option<String>, Option<String>)> {
    sqlx::query_as(
        "select utm_campaign, referrer_host, landing_path from crm_leads
         where organization_id = $1 order by received_at asc, id asc",
    )
    .bind(org)
    .fetch_all(pool)
    .await
    .expect("the stored attribution rows")
}

fn at(org: Uuid, source_id: Uuid, email_key: &str, campaign: &str, referrer: &str) -> Submission {
    submission(
        org,
        source_id,
        serde_json::json!({
            email_key: "ada@example.com",
            "utm_campaign": campaign,
            "utm_source": "newsletter",
            "referrer": referrer,
            "landing_path": "/pricing",
            "submitted_in_ms": 4200,
        }),
    )
}

/// **The core line, and the one the old code fails.** A source whose form calls the e-mail
/// field `e_mail` — the single most ordinary thing a form does, and the one every locale and
/// every template produces — keeps its **first** campaign on the second submission.
///
/// Under the old lookup this fails with the *second* campaign stored on both rows, because
/// `payload.get("email")` was `None`, `merge_attribution` returned the later visit unchanged,
/// and `insert_lead` wrote it verbatim. The assertion names the campaign, so the failure text
/// says which half broke rather than just "not equal".
#[tokio::test]
async fn the_first_touch_survives_when_the_form_field_is_not_called_email() {
    let pool = pool().await;
    let org = fresh_org(&pool, "attr-mapped-key").await;
    let source_id = source_with(
        &pool,
        org,
        vec![MappingEntry::new("email", "e_mail").with_transforms(&["lowercase"])],
    )
    .await;

    store::capture(&pool, &at(org, source_id, "e_mail", "spring-sale", "https://news.example/"))
        .await
        .expect("the first visit");
    store::capture(&pool, &at(org, source_id, "e_mail", "autumn-sale", "https://search.example/"))
        .await
        .expect("the second visit, from a different place");

    let rows = stored_touches(&pool, org).await;
    assert_eq!(rows.len(), 2, "two submissions, two leads");
    assert_eq!(
        rows[0].0.as_deref(),
        Some("spring-sale"),
        "the row that arrived first keeps its own campaign — nothing to merge yet"
    );
    assert_eq!(
        rows[1].0.as_deref(),
        Some("spring-sale"),
        "THE DEFECT: the second visit overwrote the first touch. The lookup keyed on \
         payload[\"email\"] while the row is written from the MAPPED value, so a form field \
         named anything but `email` never found its own first row"
    );
    // The referrer and the landing path are the *last* visit by design — that is the split
    // the panel's two panels read, and asserting only the campaign would let a "fix" that
    // froze every column pass.
    assert_eq!(
        rows[1].1.as_deref(),
        Some("https://search.example/"),
        "the referrer follows the most recent visit, which is the question an operator asks"
    );
    assert_eq!(rows[1].2.as_deref(), Some("/pricing"), "and so does the landing path");

    drop_org(&pool, org).await;
}

/// The defect is **not** specific to a renamed key. A source that maps e-mail from `email` and
/// the **phone** first — the fallback in `dedupe_key` — lost the first touch too, because the
/// old lookup had no phone arm at all. This pins the second half of the same fix.
#[tokio::test]
async fn a_visitor_identified_by_phone_keeps_their_first_touch() {
    let pool = pool().await;
    let org = fresh_org(&pool, "attr-phone-key").await;
    // The e-mail target maps from a key the payload does not carry, so the key is the phone.
    let source_id = source_with(
        &pool,
        org,
        vec![
            MappingEntry::new("email", "never_sent"),
            MappingEntry::new("phone", "phone").with_transforms(&["e164_lite"]),
        ],
    )
    .await;

    let visit = |campaign: &str, referrer: &str| {
        submission(
            org,
            source_id,
            serde_json::json!({
                "phone": "+90 555 111 22 33",
                "utm_campaign": campaign,
                "referrer": referrer,
                "submitted_in_ms": 4200,
            }),
        )
    };

    store::capture(&pool, &visit("spring-sale", "https://news.example/"))
        .await
        .expect("the first visit");
    store::capture(&pool, &visit("autumn-sale", "https://search.example/"))
        .await
        .expect("the second visit");

    let rows = stored_touches(&pool, org).await;
    assert_eq!(rows.len(), 2, "two submissions, two leads");
    assert_eq!(
        rows[1].0.as_deref(),
        Some("spring-sale"),
        "a phone-identified visitor keeps the campaign that first brought them in; the old \
         lookup only ever looked at an e-mail column"
    );

    drop_org(&pool, org).await;
}

/// The defect the test above's fixture was hiding, and it is the **fourth** spelling of the
/// phone rule rather than a new one.
///
/// `store::merge_attribution`'s lookup is
///
/// ```text
/// dedupe_key(mapped)                                       -> "+905****2233"  (normalize_phone KEEPS the +)
/// … and (lower(email) = $3 or lower(coalesce(phone, '')) = $3)
/// … where the phone column holds                           -> "+90 555 111 22 33" (mapped, VERBATIM)
/// ```
///
/// `normalize_phone` strips formatting but keeps the country prefix, while the **stored column is
/// the mapped value with whatever the source's mapping did to it** — and `insert_lead` binds
/// `mapped.get("phone")` verbatim. So the two sides only agree when the source happens to map
/// the field *through* the `e164_lite` transform, and a source that does not has a phone arm
/// that can never match: the key is `+905****2233`, the column is `+90 555 111 22 33`, and
/// `lower(coalesce(phone,'')) = $3` is false for ever.
///
/// **This is the same class as the tick-67 defect one function away, and the sibling fix is why
/// it is a re-audit rather than a first sighting.** Last tick `fetch_candidates` was given
/// `dedupe::PHONE_DIGITS_SQL` so its stored side keeps the plus; `merge_attribution` is the very
/// next query over the same column, it was not in that fix, and it still hand-writes its own
/// normalization. **When one half of a pair gets the shared rule, the other half is not thereby
/// correct — it is only the one that was looked at.**
///
/// It was invisible for a second reason worth naming: `a_visitor_identified_by_phone_keeps_their_first_touch`
/// maps the phone **through `e164_lite`**, so the stored column already reads `+905****2233` and the
/// comparison is trivially true. **A fixture that applies the transform makes the disagreement
/// disappear rather than fixing it** — the transform is optional on the operator's form, and a
/// source without it is the ordinary case, not an edge case.
#[tokio::test]
async fn a_phone_kept_verbatim_by_the_mapping_keeps_the_first_touch() {
    let pool = pool().await;
    let org = fresh_org(&pool, "attr-phone-raw").await;
    // **No `with_transforms`** — that is the difference from the test above, and it is the whole
    // defect. This is the shape an operator gets from the editor by default.
    let source_id = source_with(
        &pool,
        org,
        vec![
            MappingEntry::new("email", "never_sent"),
            MappingEntry::new("phone", "phone"),
        ],
    )
    .await;

    let visit = |campaign: &str, referrer: &str| {
        submission(
            org,
            source_id,
            serde_json::json!({
                "phone": "+90 555 111 22 33",
                "utm_campaign": campaign,
                "referrer": referrer,
                "submitted_in_ms": 4200,
            }),
        )
    };

    store::capture(&pool, &visit("spring-sale", "https://news.example/"))
        .await
        .expect("the first visit");
    store::capture(&pool, &visit("autumn-sale", "https://search.example/"))
        .await
        .expect("the second visit");

    // The stored value is the operator's own spelling — read it back rather than assuming, since
    // a build that normalized on the way in would make this fixture measure something else.
    let stored_phone: Option<String> =
        sqlx::query_scalar("select phone from crm_leads where organization_id = $1 limit 1")
            .bind(org)
            .fetch_one(&pool)
            .await
            .expect("the stored phone column");
    assert_eq!(
        stored_phone.as_deref(),
        Some("+90 555 111 22 33"),
        "the column holds the mapped value verbatim; this fixture only measures the defect \
         while that is true, which is what the missing transform guarantees"
    );

    let rows = stored_touches(&pool, org).await;
    assert_eq!(rows.len(), 2, "two submissions, two leads");
    assert_eq!(
        rows[1].0.as_deref(),
        Some("spring-sale"),
        "a visitor whose phone is stored as typed still keeps the campaign that first brought \
         them in: normalize_phone keeps the plus and strips the spaces, so the LOOKUP has to \
         normalize the stored column the same way rather than lower-casing it"
    );

    drop_org(&pool, org).await;
}

/// The regression guard for the shape the old code got *right*: a mapping that happens to name
/// the field `email` must keep working, and a campaign-less second visit must **not** erase
/// the first one.
///
/// The second half is the one worth having. `merge_first_touch` keeps the existing value and
/// falls back to the incoming one, so "the second submission carried no UTM" must not blank
/// the row — the failure mode of a naive `if let Some(..) = later { overwrite }` rewrite.
#[tokio::test]
async fn a_second_visit_with_no_campaign_does_not_erase_the_first() {
    let pool = pool().await;
    let org = fresh_org(&pool, "attr-no-erase").await;
    let source_id = source_with(&pool, org, vec![MappingEntry::new("email", "email")]).await;

    store::capture(&pool, &at(org, source_id, "email", "spring-sale", "https://news.example/"))
        .await
        .expect("the first visit");
    store::capture(
        &pool,
        &submission(
            org,
            source_id,
            serde_json::json!({
                "email": "ada@example.com",
                "referrer": "https://direct.example/",
                "submitted_in_ms": 4200,
            }),
        ),
    )
    .await
    .expect("the second visit, with no UTM at all");

    let rows = stored_touches(&pool, org).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[1].0.as_deref(),
        Some("spring-sale"),
        "a visit with no campaign is not a reason to forget the one that had it"
    );
    assert_eq!(
        rows[1].1.as_deref(),
        Some("https://direct.example/"),
        "while the referrer still follows the latest visit"
    );

    drop_org(&pool, org).await;
}

/// The merge is **per source**, and it is **per organization**. A different visitor must never
/// inherit a first touch, and neither must a different tenant's identically-addressed visitor —
/// the second is a leak, and it is the reason the query's `organization_id` is a bound
/// parameter rather than a constant the tests happen to satisfy.
#[tokio::test]
async fn a_first_touch_is_never_inherited_across_visitors_or_tenants() {
    let pool = pool().await;
    let org = fresh_org(&pool, "attr-isolation").await;
    let other = fresh_org(&pool, "attr-other-tenant").await;
    let source_id = source_with(&pool, org, vec![MappingEntry::new("email", "email")]).await;

    // A visitor who already has a first touch here.
    store::capture(&pool, &at(org, source_id, "email", "spring-sale", "https://news.example/"))
        .await
        .expect("the first visit");

    // A second visitor, same source, same organization: their own campaign, unmerged.
    store::capture(
        &pool,
        &submission(
            org,
            source_id,
            serde_json::json!({
                "email": "grace@example.com",
                "utm_campaign": "referral-partner",
                "submitted_in_ms": 4200,
            }),
        ),
    )
    .await
    .expect("a different visitor");

    // The same address in another tenant: a leak if the organization filter ever goes.
    let other_source = source_with(&pool, other, vec![MappingEntry::new("email", "email")]).await;
    store::capture(
        &pool,
        &at(other, other_source, "email", "private-campaign", "https://other.example/"),
    )
    .await
    .expect("the same address in another tenant");

    let mine = stored_touches(&pool, org).await;
    let theirs = stored_touches(&pool, other).await;
    assert_eq!(mine.len(), 2);
    assert_eq!(
        mine[1].0.as_deref(),
        Some("referral-partner"),
        "a different visitor keeps their own campaign: first touch is a property of a person"
    );
    assert_eq!(
        theirs[0].0.as_deref(),
        Some("private-campaign"),
        "and another organization is a different table of people entirely"
    );

    drop_org(&pool, org).await;
    drop_org(&pool, other).await;
}
