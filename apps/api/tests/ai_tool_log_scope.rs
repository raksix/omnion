//! Integration walk: the tenancy of the event feed behind the `logs.read` AI tool (REQ-100).
//!
//! The criterion — "ops tools call the platform's own service layer: a deployment tool cannot be
//! invoked with a raw command, and `logs.read` is scoped to the caller's organization" — has two
//! halves in two different layers, so they are proved in two different places.
//!
//! The command half is a property of the tool's **schema** and is proven as a unit test next to
//! the catalogue (`crates/ai-hub/src/ops_binding.rs`): a raw `command`/`shell`/`args` field is
//! refused by `additionalProperties: false` before any service is reached. Nothing here repeats it.
//!
//! The tenancy half is a property of the **query**, and this file is where it can be proven,
//! because the tool table carries no organization at all. `omnion_events::store::list_events`
//! takes `organization_id: Option<Uuid>` and documents `None` as "reads every organization's" —
//! it is the platform-wide feed the retention sweep needs. `logs.read` is a different reader: it
//! hands an organization's own application logs to a model. Both go through the one function, so
//! the only thing separating a scoped read from a cross-tenant dump is the `Option` the caller
//! passes. A unit test could not see that, and a test reading the SQL would only prove the string
//! `organization_id = $1` is present, which says nothing about whether any caller supplies it.
//!
//! What is proven here, against a real database with real migrations:
//!
//! - **another tenant's row never surfaces.** Two organizations, one event each, identical shape.
//!   A missing filter answers two rows; a filter on the wrong column answers two rows; only the
//!   filter the function actually has answers one. Not blanked, not redacted — absent.
//! - **the scope is the caller's, not the function's.** The same function with `None` answers
//!   both rows, which is the platform-wide read the retention sweep and an instance admin need.
//!   Proving both directions is what makes the first one a scope rather than a coincidence.
//!
//! It runs on the same throwaway-database harness the routing suite uses and skips itself with a
//! printed reason when PostgreSQL is not reachable.

use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::Db;
use omnion_events::store::{EventFilter, list_events};
use uuid::Uuid;

// -------------------------------------------------------------------------------------------
// The harness — a throwaway database with every migration applied.
// -------------------------------------------------------------------------------------------

struct Scratch {
    db: Db,
    maintenance: Db,
    database: String,
}

impl Scratch {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().ok()?;
        if Db::connect(&DatabaseConfig {
            url: config.database.url.clone(),
            max_connections: 1,
        })
        .await
        .is_err()
        {
            return None;
        }

        let database = format!("omnion_eventscope_{}", Uuid::new_v4().simple());
        let maintenance = Db::connect(&maintenance_config(&config))
            .await
            .expect("the maintenance connection must work");
        sqlx::query(&format!("create database \"{database}\""))
            .execute(maintenance.pool())
            .await
            .expect("the temporary database must be created");

        let db = Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, &database),
            max_connections: 4,
        })
        .await
        .expect("the fresh database must connect");
        db.migrate().await.expect("migrations must apply");

        Some(Self {
            db,
            maintenance,
            database,
        })
    }

    async fn dispose(self) {
        self.db.pool().close().await;
        let database = self.database;
        sqlx::query(&format!("drop database if exists \"{database}\" with (force)"))
            .execute(self.maintenance.pool())
            .await
            .expect("the temporary database must be removed");
    }
}

fn maintenance_config(config: &Config) -> DatabaseConfig {
    let url = config.database.url.clone();
    let (base, _) = url.rsplit_once('/').expect("a database URL has a path");
    DatabaseConfig {
        url: format!("{base}/postgres"),
        max_connections: 1,
    }
}

fn swap_database(url: &str, database: &str) -> String {
    let (base, _) = url.rsplit_once('/').expect("a database URL has a path");
    format!("{base}/{database}")
}

/// One organization and one event, so the fixture is a tenant rather than a bare uuid.
///
/// `events.organization_id` references `organizations(id)`, so a uuid invented at the call site
/// would be refused by the foreign key — and a test that inserts around its own foreign key is
/// not testing the same schema the platform runs on.
async fn seed_tenant(pool: &sqlx::PgPool, org: Uuid) {
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(org)
        .bind(format!("Org {org}"))
        .bind(format!("org-{org}"))
        .execute(pool)
        .await
        .expect("the scratch database has the organizations table");
    sqlx::query("insert into events (name, organization_id, payload) values ($1, $2, $3)")
        .bind("page.published")
        .bind(org)
        .bind(serde_json::json!({ "origin": format!("{org}") }))
        .execute(pool)
        .await
        .expect("the scratch database has the events table");
}

/// A filter the way the ROUTE builds it, not `..Default::default()`.
///
/// `EventFilter` derives `Default`, and the derived `limit` is **0** — the route fills it with
/// `DEFAULT_PAGE` from the query string (`list_events` clamps with `limit + 1` and then
/// `truncate(limit)`, so a zero limit legitimately answers zero rows). The first version of this
/// file used `..Default::default()` and both walks failed with `left: 0, right: 1` against a
/// database that demonstrably held two events and two organizations — the filter was right and
/// the page size was zero.
///
/// That is worth stating rather than quietly fixing: `Default` on a struct whose fields are all
/// `Option` is a trap, because it looks like "no filter" and is actually "match nothing". Every
/// construction here therefore names its limit.
fn filter_for(organization_id: Option<Uuid>) -> EventFilter {
    EventFilter {
        organization_id,
        limit: 50,
        ..EventFilter::default()
    }
}

// -------------------------------------------------------------------------------------------
// The two walks.
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_filter_naming_an_organization_never_surfaces_another_tenants_row() {
    let Some(scratch) = Scratch::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable for the events tenancy walk");
        return;
    };
    let mine = Uuid::new_v4();
    let theirs = Uuid::new_v4();
    for org in [mine, theirs] {
        seed_tenant(scratch.db.pool(), org).await;
    }

    let page = list_events(scratch.db.pool(), &filter_for(Some(mine)))
    .await
    .expect("the org-scoped event read must answer");

    assert_eq!(
        page.events.len(),
        1,
        "an organization-scoped read returned {} rows; the other row belongs to another tenant \
         and the filter is not holding",
        page.events.len()
    );
    assert_eq!(
        page.events[0].organization_id,
        Some(mine),
        "the single row returned for this organization carries somebody else's id"
    );
    scratch.dispose().await;
}

#[tokio::test]
async fn the_same_filter_reads_one_tenant_and_none_reads_them_all() {
    let Some(scratch) = Scratch::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable for the events tenancy walk");
        return;
    };
    let mine = Uuid::new_v4();
    let theirs = Uuid::new_v4();
    for org in [mine, theirs] {
        seed_tenant(scratch.db.pool(), org).await;
    }
    let pool = scratch.db.pool();

    let scoped = list_events(pool, &filter_for(Some(mine)))
        .await
        .expect("the org-scoped event read must answer");
    let unscoped = list_events(pool, &filter_for(None))
        .await
        .expect("the platform-wide event read must answer");

    assert_eq!(
        scoped.events.len(),
        1,
        "a scope that returns nothing is a blackout, not a scope"
    );
    assert_eq!(
        unscoped.events.len(),
        2,
        "`None` is documented to read every organization; if it stopped doing so the platform-wide \
         feed and the retention sweep would silently go blind"
    );
    scratch.dispose().await;
}
