//! The storefront settings store, against a database that has applied `0169`.
//!
//! Run through `scripts/qa/run-storefront-settings.sh`.
//!
//! ## Why this is a database test and not more unit tests
//!
//! The branch already has the lesson twice this quarter, in two different requests. A
//! `check` constraint and a Rust constant are written twice, and **no unit test can tell when
//! they disagree** — the crate validates, the database refuses, and the two happen in different
//! processes in a customer's browser session. `modules/ecommerce`'s
//! `tests_agree_with_the_migration` reads the migration *file*, which catches a list that was
//! never written into SQL; this file drives the *store* against a database that really applied
//! the migration, which catches the other direction: a column the store writes that the table
//! does not have, or a row the store silently refuses to create.
//!
//! Three of the assertions below exist for defects that are **invisible in the panel**:
//!
//! * `load` answering defaults for a *foreign* site rather than a 404 is a cross-organization
//!   read that looks like a working shop.
//! * `save` returning `false` for a foreign site is the write-side twin, and a handler that
//!   ignores that boolean answers `200` with the values it was sent — a settings screen that
//!   shows saved values that were not saved.
//! * `load_all` including an unconfigured site is the `left join`; with an `inner join` a
//!   freshly created shop is the one shop a deployment sweep skips, and nothing about the
//!   *other* shops looks wrong.

use omnion_module_ecommerce::StorefrontSettings;
use omnion_module_ecommerce::store;
use sqlx::PgPool;
use uuid::Uuid;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL (the shell script sets it)");
    PgPool::connect(&url).await.expect("the QA database")
}

/// A fresh organization with one site, and the ids needed to clean it up.
async fn fresh_org(pool: &PgPool) -> (Uuid, Uuid) {
    let org = Uuid::new_v4();
    // `slug` is NOT NULL on organizations (0001) — the first run of this gate failed on all
    // six tests for exactly this reason, which is a fair reminder that a fixture is code too.
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(org)
        .bind(format!("storefront {org}"))
        .bind(format!("storefront-{}", org.simple()))
        .execute(pool)
        .await
        .expect("organization");
    let site = Uuid::new_v4();
    sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
        .bind(site)
        .bind(org)
        .bind("shop")
        .bind("The shop")
        .execute(pool)
        .await
        .expect("site");
    (org, site)
}

/// A real `users` row, because `storefront_settings.updated_by` is a foreign key.
///
/// The second run of this gate failed here with 23503: a `Uuid::new_v4()` editor that is not a
/// user. That is the constraint working, and it is also the reason the stamp cannot be a free
/// text column — an audit stamp that is not a user is an audit entry nobody can follow back.
async fn fresh_user(pool: &PgPool, org: Uuid) -> Uuid {
    let user = Uuid::new_v4();
    sqlx::query(
        "insert into users (id, organization_id, email, password_hash, display_name) \
         values ($1, $2, $3, 'x', $4)",
    )
    .bind(user)
    .bind(org)
    .bind(format!("shopper-{user}@example.test"))
    .bind("Shopper")
    .execute(pool)
    .await
    .expect("user");
    user
}

/// A second organization, for the cross-organization assertions.
async fn other_org() -> (PgPool, Uuid, Uuid) {
    let pool = pool().await;
    let (org, site) = fresh_org(&pool).await;
    (pool, org, site)
}

#[tokio::test]
async fn an_unconfigured_site_serves_defaults_and_says_so() {
    let pool = pool().await;
    let (org, site) = fresh_org(&pool).await;

    let loaded = store::load(&pool, org, site).await.expect("read");
    assert!(!loaded.row_exists, "a site nobody configured has no row");
    assert_eq!(loaded.settings, StorefrontSettings::defaults(site));
    // And the values are the ones a shop would actually serve, not zeroes: a page size of 0
    // renders an empty catalogue with a working "next" link.
    assert_eq!(loaded.settings.page_size, 24);
    assert!(loaded.settings.guest_checkout);

    // The read path must not have *created* the row: creating it here would turn "nobody
    // configured this" into "somebody configured this" and the panel would claim the operator
    // chose defaults.
    assert!(!store::row_exists(&pool, org, site).await.expect("exists"));
}

#[tokio::test]
async fn a_saved_setting_reads_back_exactly() {
    let pool = pool().await;
    let (org, site) = fresh_org(&pool).await;
    let editor = fresh_user(&pool, org).await;

    let mut settings = StorefrontSettings::defaults(site);
    // Three of the acceptance line's promises at once: guest checkout off, exclusive tax
    // wording, and a raised quantity cap.
    settings.guest_checkout = false;
    settings.tax_display = "exclusive".to_string();
    settings.per_order_item_max = 7;
    settings.page_size = 48;
    settings.abandonment_hours = 2;
    settings.currency = "TRY".to_string();
    settings.normalize().expect("valid");

    let written = store::save(&pool, org, &settings, Some(editor))
        .await
        .expect("save");
    assert!(written, "the first write creates the row");

    let loaded = store::load(&pool, org, site).await.expect("read");
    assert!(loaded.row_exists);
    assert_eq!(
        loaded.settings, settings,
        "the store wrote what it was given"
    );
    assert_eq!(loaded.settings.quantity_cap(), 7);
    assert!(!loaded.settings.shows_tax_inclusive());

    // A second write is an update, not a second row: `save` is called on every form submit.
    settings.page_size = 12;
    settings.normalize().expect("valid");
    assert!(
        store::save(&pool, org, &settings, Some(editor))
            .await
            .expect("save again")
    );
    let reloaded = store::load(&pool, org, site).await.expect("read");
    assert_eq!(reloaded.settings.page_size, 12);
    assert_eq!(
        reloaded.settings.per_order_item_max, 7,
        "an update must not reset the fields this call did not mention — it sends all of them, \
         so a field silently reverting is the write path dropping a column"
    );
}

#[tokio::test]
async fn a_foreign_site_reads_as_defaults_and_writes_nothing() {
    let (pool, _mine_org, _mine_site) = other_org().await;
    let (_other_pool, other_org_id, other_site) = other_org().await;

    // The victim's settings, so a leak would be visible as values rather than as a 404.
    let mut theirs = StorefrontSettings::defaults(other_site);
    theirs.page_size = 96;
    theirs.currency = "GBP".to_string();
    theirs.normalize().expect("valid");
    store::save(&pool, other_org_id, &theirs, None)
        .await
        .expect("seed the other organization's storefront");

    // A read of another organization's site is the defaults, never their configuration: a 200
    // carrying somebody else's currency is a cross-organization read.
    let leaked = store::load(&pool, _mine_org, other_site)
        .await
        .expect("read");
    assert_eq!(leaked.settings, StorefrontSettings::defaults(other_site));
    assert_ne!(
        leaked.settings.currency, "GBP",
        "another shop's currency leaked"
    );

    // The write side is the one that matters: it must write ZERO rows, and must say so.
    let mut attempt = StorefrontSettings::defaults(other_site);
    attempt.page_size = 8;
    attempt.normalize().expect("valid");
    let written = store::save(&pool, _mine_org, &attempt, None)
        .await
        .expect("save into another organization");
    assert!(
        !written,
        "save must report zero rows for a foreign site — a handler that ignores this answers \
         200 with values that were never stored"
    );

    // And the victim's row is untouched.
    let after = store::load(&pool, other_org_id, other_site)
        .await
        .expect("read");
    assert_eq!(
        after.settings.page_size, 96,
        "a foreign write changed the row"
    );
}

#[tokio::test]
async fn load_all_includes_a_site_nobody_configured() {
    let pool = pool().await;
    let (org, site) = fresh_org(&pool).await;

    // No save at all. `load_all` is what "every shop this organization has" means, and an
    // inner join would quietly return nothing here — the freshly created shop is exactly the
    // one a deployment sweep must not skip.
    let all = store::load_all(&pool, org).await.expect("list");
    let found = all
        .iter()
        .find(|loaded| loaded.settings.site_id == site)
        .expect("an unconfigured site is still a site");
    assert!(!found.row_exists);
    assert_eq!(found.settings, StorefrontSettings::defaults(site));

    // Configure it, and the same query now reports it as configured rather than dropping it.
    let mut settings = StorefrontSettings::defaults(site);
    settings.page_size = 6;
    settings.normalize().expect("valid");
    store::save(&pool, org, &settings, None)
        .await
        .expect("save");

    let all = store::load_all(&pool, org).await.expect("list");
    let found = all
        .iter()
        .find(|loaded| loaded.settings.site_id == site)
        .expect("a configured site is still listed");
    assert!(found.row_exists);
    assert_eq!(found.settings.page_size, 6);
}

#[tokio::test]
async fn the_database_refuses_a_value_the_crate_refuses() {
    let pool = pool().await;
    let (org, site) = fresh_org(&pool).await;
    let settings = StorefrontSettings::defaults(site);
    store::save(&pool, org, &settings, None)
        .await
        .expect("seed");

    // Write past the crate: a page size the crate would refuse, bypassing `normalize`. The
    // check constraint is the backstop, and this is the only assertion that can see it — the
    // crate's own tests never reach the database.
    let error = sqlx::query("update storefront_settings set page_size = 5000 where site_id = $1")
        .bind(site)
        .execute(&pool)
        .await
        .expect_err("the check constraint refuses a page size of 5000");
    let text = error.to_string();
    assert!(
        text.contains("storefront_settings_page_size_check"),
        "the refusal should name the constraint so a support ticket is diagnosable: {text}"
    );

    // Zero abandonment hours: the failure this whole slice is built to make impossible.
    let error =
        sqlx::query("update storefront_settings set abandonment_hours = 0 where site_id = $1")
            .bind(site)
            .execute(&pool)
            .await
            .expect_err("a cart must not be abandonable at birth");
    assert!(
        error
            .to_string()
            .contains("storefront_settings_abandonment_hours_check"),
        "{error}"
    );
}

#[tokio::test]
async fn a_site_created_after_the_migration_has_no_row_and_the_read_says_so() {
    let pool = pool().await;
    let (org, site) = fresh_org(&pool).await;

    // The migration's `insert … select id from sites` ran before this row existed, so there is
    // nothing to find. Asserted explicitly because the opposite belief — "the migration seeds
    // every site" — is exactly what a reader of the migration file concludes, and it is what
    // makes somebody add a *second* backfill when the real gap is that `load` has to cope.
    let seeded: Option<(i32,)> =
        sqlx::query_as("select page_size from storefront_settings where site_id = $1")
            .bind(site)
            .fetch_optional(&pool)
            .await
            .expect("read");
    assert!(
        seeded.is_none(),
        "a post-migration site has no settings row"
    );

    // What the read serves is the defaults, and the defaults are the *migration's* column
    // defaults — the two are written twice on purpose and the crate's own vocabulary test is
    // what keeps the lists honest. These three are the numbers a shop actually sees.
    let loaded = store::load(&pool, org, site).await.expect("read");
    assert_eq!(loaded.settings.page_size, 24);
    assert_eq!(loaded.settings.abandonment_hours, 24);
    assert!(loaded.settings.guest_checkout);
}
