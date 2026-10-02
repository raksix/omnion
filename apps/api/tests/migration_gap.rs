//! What a gap in the migration ledger actually does.
//!
//! `main`'s ledger jumps `0018 → 0021`: three sibling writers each claimed `0019`
//! independently — `0019_cms_blocks` (wave2), `0019_organization_memberships` (wave5),
//! `0019_secret_hierarchy` (wave6) — and none of those files reached `main`. So `0019`,
//! `0020` and `0022`…`0024` do not exist in this tree.
//!
//! Two build logs have since written that this makes `migrate()` fail on **any fresh
//! database** and blocks `scripts/qa/run.sh` at step 1, and one of them deferred a browser
//! pass on that basis. That is a claim about sqlx's `Migrator::run`, so it is tested here
//! rather than believed. The source settles it: `validate_applied_migrations` iterates the
//! *applied* rows and rejects one whose version is not in the embedded set. A fresh database
//! has no applied rows, so the loop has nothing to reject and the gap is inert.
//!
//! The gap is not nothing, though, and this file keeps the half that does bite pinned: a
//! database restored from a branch that had a `0019` carries an applied row this tree cannot
//! see, and that is a real refusal. A restore across branches is the dangerous operation
//! here, not a clean install.
//!
//! Each test creates and drops its own database, so a run never touches the development
//! database — a test that quietly used it would pass for the wrong reason. The server URL
//! comes from the environment the rest of the suites use, so no credential is written down
//! here: the defaults live in `crates/core/src/config.rs`.

use omnion_core::config::Config;
use omnion_core::db::Db;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use std::str::FromStr;
use std::time::Duration;

/// The version this tree does *not* ship, and the one a restore would carry.
const ABSENT_VERSION: i64 = 19;

/// The configured server, without the database name: `postgres://…@host:port`.
fn server_url() -> String {
    let url = Config::from_env()
        .expect("environment must be valid")
        .database
        .url;
    url.rfind('/')
        .map(|cut| url[..cut].to_owned())
        .unwrap_or(url)
}

/// A connection to the server's maintenance database, for create and drop.
async fn admin_pool() -> Option<PgPool> {
    let url = format!("{}/postgres", server_url());
    let options = match PgConnectOptions::from_str(&url) {
        Ok(options) => options,
        Err(err) => {
            eprintln!("SKIP: the configured database URL does not parse ({err})");
            return None;
        }
    };
    match PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options)
        .await
    {
        Ok(pool) => Some(pool),
        Err(err) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({err}) — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
            );
            None
        }
    }
}

/// Drop a database if it is there, then create it empty. `None` when the server is absent.
async fn create_fresh(name: &str) -> Option<PgPool> {
    let admin = admin_pool().await?;
    sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .execute(&admin)
        .await
        .expect("drop the leftover database");
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .expect("create a database to migrate");
    Some(admin)
}

/// Drop a database nobody is connected to. `Db`'s pool closes first, because PostgreSQL
/// refuses to drop a database that still has a session.
async fn destroy_fresh(admin: Option<PgPool>, db: Option<Db>, name: &str) {
    if let Some(db) = db {
        db.pool().close().await;
    }
    if let Some(admin) = admin {
        sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
            .execute(&admin)
            .await
            .expect("drop the test database");
        admin.close().await;
    }
}

/// Connect to a freshly created database by name.
async fn connect(name: &str) -> Db {
    let mut config = Config::from_env().expect("environment must be valid");
    config.database.url = format!("{}/{name}", server_url());
    Db::connect(&config.database)
        .await
        .expect("connect to the new database")
}

/// Migrate a database nobody has touched, and read the ledger back.
#[tokio::test]
async fn fresh_database_migrates_despite_a_gap() {
    let name = "omnion_test_gap_fresh";
    let Some(admin) = create_fresh(name).await else {
        return;
    };
    let db = connect(name).await;

    db.migrate()
        .await
        .expect("a fresh database must migrate; the 0018->0021 gap is inert here");

    let status = db
        .migration_status()
        .await
        .expect("migration bookkeeping is readable");
    assert!(
        status.is_up_to_date(),
        "a clean install applies everything it embeds: pending={:?}",
        status.pending
    );
    assert_eq!(
        status.applied.len(),
        status.total(),
        "the ledger holds one row per embedded migration"
    );
    let unique: std::collections::HashSet<&i64> = status.applied.iter().collect();
    assert_eq!(
        status.applied.len(),
        unique.len(),
        "no version is applied twice"
    );

    // The gap is a fact about the repository, so it is asserted rather than left implicit: a
    // future tick that merges a sibling's 0019 has to revisit this test on purpose.
    assert!(
        !status.applied.contains(&ABSENT_VERSION),
        "main still does not ship 0019; if a branch's 0019 was merged, revisit this test and \
         re-check the restore path"
    );

    // Idempotent, which is what the API does on every single boot.
    db.migrate().await.expect("migrate is idempotent");

    destroy_fresh(Some(admin), Some(db), name).await;
}

/// The half of the gap that *does* bite: a database whose ledger already holds a version this
/// tree cannot see must refuse, not carry on against a schema it cannot vouch for.
///
/// The row is written by hand rather than by importing a sibling's file, so the test states
/// the rule instead of the accident that produced it.
#[tokio::test]
async fn restored_ledger_must_be_contiguous() {
    let name = "omnion_test_gap_restored";
    let Some(admin) = create_fresh(name).await else {
        return;
    };
    let db = connect(name).await;
    db.migrate().await.expect("clean baseline");

    // A restore from a branch that had a 0019: applied here, absent from this tree.
    sqlx::query(
        "insert into _sqlx_migrations \
         (version, description, installed_on, success, checksum, execution_time) \
         values ($1, $2, now(), true, '\\x00'::bytea, 0)",
    )
    .bind(ABSENT_VERSION)
    .bind("from_another_branch")
    .execute(db.pool())
    .await
    .expect("seed the applied-but-absent row");

    let err = db
        .migrate()
        .await
        .expect_err("an applied migration this tree cannot see must stop the runner");
    let text = err.to_string();
    assert!(
        text.contains(&ABSENT_VERSION.to_string()),
        "the refusal must name the version it cannot see, not just fail: {text}"
    );

    destroy_fresh(Some(admin), Some(db), name).await;
}
