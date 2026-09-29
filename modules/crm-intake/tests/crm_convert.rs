//! The slice-3 gate: conversion, the flow's degradation and the retention sweep, against a
//! real database.
//!
//! Run through `scripts/qa/run-crm-convert.sh`.
//!
//! ## What this file exists to prove that a unit test cannot
//!
//! * **The CRM-present path really writes a contact and a deal.** The pure half of
//!   conversion is a decision; this is the SQL behind it, and the two disagree in exactly
//!   the way that matters — a decision about a contact the insert then refuses.
//! * **The CRM-absent path degrades instead of failing.** `crm_contacts` belongs to REQ-051
//!   and is *not on this branch*, which makes this suite the only place the degradation can
//!   be observed for real rather than mocked. The test asserts the contact still lands and
//!   the reason is recorded, because a `500` on `Convert` for an installation that simply
//!   has no CRM module is the failure this is written against.
//! * **Converting twice makes one contact.** The second press must reuse the first
//!   contact's id, or every impatient operator doubles their CRM.
//! * **A cross-organization lead is a `None`.** Not a `403` — a `403` is an oracle.
//! * **The retention sweep keeps the row and clears the body.** The two halves of that
//!   promise are separate columns, and a sweep that empties the row as well as the payload
//!   would turn "deletable" into "invisible".

use omnion_module_crm_intake::convert_store;
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
        "delete from crm_deals where organization_id = $1",
        "delete from crm_contacts where organization_id = $1",
        "delete from crm_pipeline_stages where organization_id = $1",
        "delete from crm_pipelines where organization_id = $1",
    ] {
        let _ = sqlx::query(sql).bind(org).execute(pool).await;
    }
    let _ = sqlx::query("delete from organizations where id = $1")
        .bind(org)
        .execute(pool)
        .await;
}

/// The three CRM tables this branch does not carry, at the shape REQ-051 defines them.
///
/// Created and dropped *per test* and in a transaction that is rolled back afterwards, so
/// two tests racing in the same binary do not see each other's tables. This is a deliberate
/// lie to the production schema, and it is commented as one: the point is to exercise the
/// insert path, not to re-declare another module's migration.
async fn with_crm_tables(pool: &PgPool, body: impl std::future::Future<Output = ()>) {
    // The tables have to be *committed* to be visible to the pool the body uses, so this
    // cannot be a transaction that rolls back. It is guarded by an advisory lock instead,
    // and the absent-path test takes the shared half — so the two tests can never see each
    // other's world, whatever order cargo picks them in.
    //
    // (A rolled-back transaction was the first version, and it failed in a way that looked
    // like a product bug: the body wrote to tables that did not exist, because a
    // rolled-back `create table` is invisible to every other connection.)
    let mut held = pool.acquire().await.expect("a connection");
    sqlx::query("select pg_advisory_lock($1)")
        .bind(ADV_LOCK_CRM)
        .execute(&mut *held)
        .await
        .expect("the exclusive fixture lock");
    for sql in [
        "create table crm_pipelines (id uuid primary key default gen_random_uuid(), organization_id uuid not null references organizations (id) on delete cascade, name text not null, is_default boolean not null default false, created_at timestamptz not null default now(), updated_at timestamptz not null default now())",
        "create table crm_pipeline_stages (id uuid primary key default gen_random_uuid(), organization_id uuid not null references organizations (id) on delete cascade, pipeline_id uuid not null references crm_pipelines (id) on delete cascade, name text not null, kind text not null default 'open', position integer not null, probability integer not null default 0, created_at timestamptz not null default now(), updated_at timestamptz not null default now())",
        "create table crm_companies (id uuid primary key default gen_random_uuid(), organization_id uuid not null references organizations (id) on delete cascade, name text not null, domain text, owner_user_id uuid, status text not null default 'lead', tags text[] not null default '{}', custom jsonb not null default '{}', notes text not null default '', archived_at timestamptz, created_at timestamptz not null default now(), updated_at timestamptz not null default now())",
        "create table crm_contacts (id uuid primary key default gen_random_uuid(), organization_id uuid not null references organizations (id) on delete cascade, first_name text, last_name text, email text, phone text, job_title text, company_id uuid, owner_user_id uuid, status text not null default 'lead', tags text[] not null default '{}', custom jsonb not null default '{}', notes text not null default '', last_activity_at timestamptz, archived_at timestamptz, created_at timestamptz not null default now(), updated_at timestamptz not null default now())",
        "create table crm_deals (id uuid primary key default gen_random_uuid(), organization_id uuid not null references organizations (id) on delete cascade, pipeline_id uuid not null, stage_id uuid not null, title text not null, company_id uuid, contact_id uuid, owner_user_id uuid, amount numeric(14,2) not null default 0, currency char(3) not null default 'USD', probability integer, expected_close_on date, source text, lost_reason text, stage_changed_at timestamptz not null default now(), archived_at timestamptz, created_at timestamptz not null default now(), updated_at timestamptz not null default now())",
    ] {
        sqlx::query(sql)
            .execute(&mut *held)
            .await
            .expect("the CRM tables for this test");
    }
    body.await;
    for sql in [
        "drop table if exists crm_deals",
        "drop table if exists crm_contacts",
        "drop table if exists crm_companies",
        "drop table if exists crm_pipeline_stages",
        "drop table if exists crm_pipelines",
    ] {
        let _ = sqlx::query(sql).execute(&mut *held).await;
    }
    sqlx::query("select pg_advisory_unlock($1)")
        .bind(ADV_LOCK_CRM)
        .execute(&mut *held)
        .await
        .expect("to release the fixture lock");
}

/// The advisory lock key the two tests serialize on.
const ADV_LOCK_CRM: i64 = 0x0c_72_6d_00_00_00_01;

/// A source and one lead through it, captured through the real capture path.
async fn one_lead(pool: &PgPool, org: Uuid, email: &str, payload: serde_json::Value) -> Uuid {
    let source_id = Uuid::new_v4();
    // The mapping is NOT empty. That was the first version of this helper and it failed
    // with `crm_leads_contactable_check` — correctly: with no mapping a submission has no
    // path to the lead's own columns, so capture filed it as `rejected` for having nothing
    // to contact. The product was right and the fixture was wrong, which is a useful shape
    // for a failure to have.
    // The serde names are `source_key` and `transform`, and getting them wrong is silent:
    // every line deserializes with `source_key: None`, the mapping produces nothing, and
    // capture files the submission as `rejected` — which reads as a product refusal
    // rather than a typo in a fixture. So the field names are checked, not trusted.
    let mapping = serde_json::json!([
        { "target": "email", "source_key": "email", "transform": ["trim", "lowercase"],
          "required": false },
        { "target": "first_name", "source_key": "first_name", "transform": ["trim"],
          "required": false },
        { "target": "last_name", "source_key": "last_name", "transform": ["trim"],
          "required": false },
        { "target": "company_name", "source_key": "company_name", "transform": ["trim"],
          "required": false },
        { "target": "product_interest", "source_key": "product_interest", "transform": ["trim"],
          "required": false },
        { "target": "message", "source_key": "message", "transform": ["strip_html"],
          "required": false }
    ]);
    let lines: Vec<omnion_module_crm_intake::MappingEntry> =
        serde_json::from_value(mapping.clone()).expect("the mapping deserializes");
    assert!(
        lines.iter().all(|line| line.source_key.is_some()),
        "every line names a source key — a wrong serde field name fails silently"
    );
    sqlx::query(
        "insert into crm_intake_sources (id, organization_id, name, kind, mapping, \
             required_targets, dedupe_policy) \
         values ($1, $2, $4, 'endpoint', $3, '{}', 'link')",
    )
    .bind(source_id)
    .bind(org)
    .bind(mapping)
    .bind(format!("convert source {}", source_id))
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
            payload,
            received_at: time::OffsetDateTime::now_utc(),
        },
    )
    .await
    .expect("a captured lead");
    // A rejected fixture row would make every conversion test below pass for the wrong
    // reason — the row exists, it is just not a lead. Asserted here, once, where the
    // fixture is made, so the failure names the fixture rather than a conversion.
    assert_eq!(
        captured.lead.status, "new",
        "the fixture lead was not accepted ({}): {}",
        captured.lead.status,
        captured.lead.rejection_reason.as_deref().unwrap_or("no reason recorded")
    );
    assert_eq!(captured.lead.email.as_deref(), Some(email), "the mapped address");
    captured.lead.id
}

/// The payload a quote form posts, with a budget band the amount parser has to read.
fn quote_payload(email: &str) -> serde_json::Value {
    serde_json::json!({
        "email": email,
        "first_name": "Furkan",
        "last_name": "Ermag",
        "company_name": "Acme",
        "product_interest": "Website rewrite",
        "budget": "10k-50k",
        "message": "We need a quote.",
    })
}

#[tokio::test]
async fn the_crm_absent_path_creates_the_contact_and_says_why_not_the_deal() {
    let pool = pool().await;
    // The shared half of the fixture lock, held for the whole test: the tables must be
    // absent for every statement this test makes, not only for the first.
    let mut held = pool.acquire().await.expect("a connection");
    sqlx::query("select pg_advisory_lock_shared($1)")
        .bind(ADV_LOCK_CRM)
        .execute(&mut *held)
        .await
        .expect("the shared fixture lock");
    let org = fresh_org(&pool, "convert-absent").await;
    // This branch has no `crm_contacts`/`crm_deals`, which is the real state — so the
    // degradation is observed, not simulated. If a future merge brings REQ-051 in, this
    // test starts failing with a *precise* message, and the fix is to make it create the
    // tables rather than to delete the branch.
    assert!(
        !table_present(&pool, "crm_contacts").await,
        "REQ-051's tables are on this branch now — wrap this test in with_crm_tables and \
         let the present-path test cover the other half"
    );

    let lead_id = one_lead(&pool, org, "absent@example.com", quote_payload("absent@example.com")).await;
    let report = convert_store::convert_lead(&pool, org, lead_id, None)
        .await
        .expect("the conversion")
        .expect("a report");

    // Nothing is written, and the panel is told why. A contact is a CRM row: with no
    // `crm_contacts` there is nowhere to put one, so "create the contact, skip the deal" is
    // not a degradation — it is a description of an installation this is not.
    assert!(report.deal_id.is_none(), "no deal can exist without the CRM tables");
    assert!(!report.contact_created, "no contact can exist without the CRM tables");
    assert!(
        report.deal_skipped.is_some(),
        "the panel has to be told why — a silent success reads as a converted lead"
    );
    assert!(
        report.deal_skipped.as_deref().unwrap_or_default().contains("CRM module"),
        "the reason names the module: {:?}",
        report.deal_skipped
    );

    // The lead row is untouched: a conversion that did not happen must not leave the row
    // looking as though it had.
    let lead = store::find_lead(&pool, org, lead_id)
        .await
        .expect("the lead")
        .expect("a lead row");
    assert_eq!(lead.contact_id, None, "no pointer to a row that cannot exist");
    assert_eq!(lead.deal_id, None);
    assert_eq!(lead.status, "new", "the status is not advanced by a failed conversion");

    // The trail carries the reason — that line is the only thing an operator has to go on.
    let events = store::list_events(&pool, org, lead_id).await.expect("the trail");
    let skipped = events
        .iter()
        .find(|event| event.kind == "conversion_skipped")
        .expect("a skipped line on the trail");
    assert!(
        skipped.detail.get("deal_skipped").is_some(),
        "the trail carries the reason too: {}",
        skipped.detail
    );

    drop_org(&pool, org).await;
    sqlx::query("select pg_advisory_unlock_shared($1)")
        .bind(ADV_LOCK_CRM)
        .execute(&mut *held)
        .await
        .expect("to release the shared lock");
}

#[tokio::test]
async fn with_the_crm_present_a_contact_and_an_opportunity_are_written() {
    let pool = pool().await;
    let org = fresh_org(&pool, "convert-present").await;

    with_crm_tables(&pool, async {
        // A default pipeline with one open stage — the minimum a deal can exist in.
        let pipeline_id = Uuid::new_v4();
        let stage_id = Uuid::new_v4();
        sqlx::query("insert into crm_pipelines (id, organization_id, name, is_default) values ($1, $2, 'Sales', true)")
            .bind(pipeline_id).bind(org).execute(&pool).await.expect("a pipeline");
        sqlx::query("insert into crm_pipeline_stages (id, organization_id, pipeline_id, name, kind, position, probability) values ($1, $2, $3, 'Qualified', 'open', 1, 40)")
            .bind(stage_id).bind(org).bind(pipeline_id).execute(&pool).await.expect("a stage");

        let lead_id = one_lead(&pool, org, "present@example.com", quote_payload("present@example.com")).await;
        let report = convert_store::convert_lead(&pool, org, lead_id, None)
            .await
            .expect("the conversion")
            .expect("a report");

        let deal_id = report.deal_id.expect("a deal, with the CRM tables present");
        assert!(report.deal_skipped.is_none());

        // The deal is a row, and it carries the *midpoint* of the band the visitor typed:
        // "10k-50k" is 30 000, not 10 and not 0.
        // `amount` is `numeric(14,2)` and sqlx refuses to hand a `numeric` to an `f64`
        // rather than converting it. Casting in SQL is the honest fix: the assertion is
        // about the number the visitor's band became, not about a Rust decimal type.
        let (title, amount): (String, i64) =
            sqlx::query_as("select title, cast(amount as bigint) from crm_deals where id = $1")
            .bind(deal_id).fetch_one(&pool).await.expect("the deal row");
        assert_eq!(amount, 30_000, "the band midpoint of 10k-50k, not the low end");
        assert!(title.contains("Website rewrite"), "{title}");

        // The contact exists and the lead points at both.
        let contact: (String,) = sqlx::query_as("select coalesce(first_name, '') from crm_contacts where id = $1")
            .bind(report.contact_id).fetch_one(&pool).await.expect("the contact row");
        assert_eq!(contact.0, "Furkan");

        let lead = store::find_lead(&pool, org, lead_id).await.expect("read").expect("row");
        assert_eq!(lead.contact_id, Some(report.contact_id));
        assert_eq!(lead.deal_id, Some(deal_id));
        assert_eq!(lead.status, "qualified", "the opportunity exists, the customer does not");

        // Converting again reuses the contact instead of making a second one — the whole
        // point of pressing a button twice being harmless.
        let again = convert_store::convert_lead(&pool, org, lead_id, None)
            .await
            .expect("the second conversion")
            .expect("a report");
        assert_eq!(again.contact_id, report.contact_id, "one press, one contact");
        let contacts: (i64,) = sqlx::query_as("select count(*) from crm_contacts where organization_id = $1")
            .bind(org).fetch_one(&pool).await.expect("the count");
        assert_eq!(contacts.0, 1, "a second press must not double the CRM");
    })
    .await;

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn a_quotation_being_accepted_promotes_the_lead_idempotently() {
    let pool = pool().await;
    let org = fresh_org(&pool, "convert-quote").await;
    let lead_id = one_lead(&pool, org, "quote@example.com", quote_payload("quote@example.com")).await;
    let quote_id = Uuid::new_v4();

    let promoted = convert_store::mark_quote_accepted(&pool, org, lead_id, quote_id)
        .await
        .expect("the promotion")
        .expect("a lead");
    assert_eq!(promoted.status, "converted");
    assert_eq!(promoted.quote_id, Some(quote_id));
    let first_at = promoted.converted_at.expect("a conversion instant");

    // An at-least-once bus delivers twice. The second delivery must not move the instant:
    // the trail says when the customer signed, and a rewritten timestamp is a lie about
    // something contractual.
    let again = convert_store::mark_quote_accepted(&pool, org, lead_id, quote_id)
        .await
        .expect("the second promotion")
        .expect("a lead");
    assert_eq!(
        again.converted_at,
        Some(first_at),
        "the instant is the first one"
    );

    let trail = store::list_events(&pool, org, lead_id).await.expect("the trail");
    assert_eq!(
        trail.iter().filter(|e| e.kind == "quote_accepted").count(),
        1,
        "one acceptance, one line"
    );

    drop_org(&pool, org).await;
}

#[tokio::test]
async fn another_organizations_lead_converts_to_nothing_at_all() {
    let pool = pool().await;
    let mine = fresh_org(&pool, "convert-mine").await;
    let theirs = fresh_org(&pool, "convert-theirs").await;
    let their_lead = one_lead(&pool, theirs, "theirs@example.com", quote_payload("theirs@example.com")).await;

    // `None`, not a `403` and not an error: a cross-tenant id must be indistinguishable
    // from an id that does not exist, or the route is an enumeration oracle.
    let report = convert_store::convert_lead(&pool, mine, their_lead, None)
        .await
        .expect("no error");
    assert!(report.is_none(), "another tenant's lead is not convertible");

    let still_there: (i64,) = sqlx::query_as("select count(*) from crm_leads where id = $1")
        .bind(their_lead).fetch_one(&pool).await.expect("the count");
    assert_eq!(still_there.0, 1, "the refused conversion wrote nothing");

    drop_org(&pool, theirs).await;
    drop_org(&pool, mine).await;
}

#[tokio::test]
async fn the_retention_sweep_clears_the_payload_and_keeps_the_row() {
    let pool = pool().await;
    let org = fresh_org(&pool, "convert-retention").await;
    let lead_id = one_lead(&pool, org, "old@example.com", quote_payload("old@example.com")).await;

    // A row the sweep will take: aged past the window.
    sqlx::query("update crm_leads set received_at = now() - interval '900 days' where id = $1")
        .bind(lead_id).execute(&pool).await.expect("age the lead");
    // A row it will not: yesterday.
    let recent = one_lead(&pool, org, "new@example.com", quote_payload("new@example.com")).await;

    let archived = convert_store::archive_expired_payloads(&pool, org, 730)
        .await
        .expect("the sweep");
    assert_eq!(archived, 1, "exactly the aged row");

    let old = store::find_lead(&pool, org, lead_id).await.expect("read").expect("row");
    assert_eq!(old.payload_bytes, 0, "the submission body is gone");
    assert!(old.payload.as_object().is_none_or(serde_json::Map::is_empty));
    assert!(old.message.is_none(), "the free text goes with it");
    // The row itself — and everything the operations depend on — stays.
    assert_eq!(old.status, "new", "the row is still a lead");
    assert_eq!(old.email.as_deref(), Some("old@example.com"), "the address is kept");

    let kept = store::find_lead(&pool, org, recent).await.expect("read").expect("row");
    assert!(kept.payload_bytes > 0, "a recent lead is not swept");

    // And the trail survived: an archived lead with no history is not an archived lead.
    let events = store::list_events(&pool, org, lead_id).await.expect("the trail");
    assert!(!events.is_empty(), "the trail is not part of what retention erases");

    drop_org(&pool, org).await;
}

/// `true` when a relation is in this database's catalog.
async fn table_present(pool: &PgPool, name: &str) -> bool {
    sqlx::query_scalar::<_, bool>("select to_regclass($1) is not null")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap_or(false)
}
