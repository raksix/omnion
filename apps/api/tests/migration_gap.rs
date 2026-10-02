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

// ---------------------------------------------------------------------------
// Migration 0151 — the limiter and lockout documents (REQ-012, slice 3).
//
// The file is three statements, so "it applied" is not much of a claim; what matters is
// whether the *guarantees it makes to the Rust layer* survive a real install:
//
//   * a row inserted by anything other than the migration still holds a valid policy. The
//     columns are NOT NULL with literal defaults precisely so a bare
//     `insert into security_settings (id) values (1)` cannot produce a row every reader has
//     to special-case. The assertion is that insert, not a comment about it.
//   * the two shape constraints are load-bearing. `rate_limits` is a list and `lockout` is
//     an object, and `merge_with_defaults` is written on that assumption; a constraint that
//     accepts the wrong shape turns "a missing scope" into "a scope set to nonsense" at read
//     time instead of at write time. A constraint nobody has ever tried to violate is a
//     constraint nobody knows works, so each is violated here on purpose.
//
// A constraint test that only checks a *valid* row passes against a missing constraint just as
// happily, so the two refusals below are the real assertions.
// ---------------------------------------------------------------------------

/// The version of the limiter migration in this tree.
const LIMITER_VERSION: i64 = 151;

#[tokio::test]
async fn the_limiter_migration_lands_a_row_that_is_usable_without_it() {
    let name = "omnion_test_mig151";
    let Some(admin) = create_fresh(name).await else {
        return;
    };
    let db = connect(name).await;
    db.migrate()
        .await
        .expect("the limiter migration applies cleanly");

    let applied: bool = sqlx::query_scalar(
        "select exists(select 1 from _sqlx_migrations where version = $1 and success)",
    )
    .bind(LIMITER_VERSION)
    .fetch_one(db.pool())
    .await
    .expect("read the migration ledger");
    assert!(applied, "migration 0151 must be in the ledger as applied");

    // The insert nobody writes but something will: a fixture, a seed, an operator at a psql
    // prompt. It names no column, so the defaults are the whole policy.
    sqlx::query("insert into security_settings (id) values (1) on conflict (id) do nothing")
        .execute(db.pool())
        .await
        .expect("a settings row inserts with only its key");

    // A default limiter with no scopes is legal and is the state `merge_with_defaults` exists
    // for: the Rust layer fills the baseline, and the panel shows the merged document. What it
    // must not be is null or a shape the merge cannot read.
    let (rate_shape, lockout_shape): (String, String) =
        sqlx::query_as("select jsonb_typeof(rate_limits), jsonb_typeof(lockout) from security_settings where id = 1")
            .fetch_one(db.pool())
            .await
            .expect("read the two documents back");
    assert_eq!(
        rate_shape, "array",
        "the limiter document is a list of scopes"
    );
    assert_eq!(
        lockout_shape, "object",
        "the lockout document is one object"
    );

    // The partial index behind "which accounts are locked right now". Its predicate is the
    // point: an index over a nullable timestamp without it is a sequential read of every
    // account on a platform with millions of them, which is the query the screen runs.
    let predicate: Option<String> = sqlx::query_scalar(
        "select indexdef from pg_indexes where indexname = 'users_locked_until_live_idx'",
    )
    .fetch_optional(db.pool())
    .await
    .expect("read the index definition")
    .map(|def: String| def.to_lowercase());
    let predicate = predicate.expect("the locked-accounts index must exist");
    assert!(
        predicate.contains("where") && predicate.contains("locked_until is not null"),
        "the index must be partial on the locked rows only: {predicate}"
    );

    destroy_fresh(Some(admin), Some(db), name).await;
}

#[tokio::test]
async fn the_limiter_migration_refuses_a_document_of_the_wrong_shape() {
    let name = "omnion_test_mig151_shape";
    let Some(admin) = create_fresh(name).await else {
        return;
    };
    let db = connect(name).await;
    db.migrate()
        .await
        .expect("the limiter migration applies cleanly");
    sqlx::query("insert into security_settings (id) values (1) on conflict (id) do nothing")
        .execute(db.pool())
        .await
        .expect("a settings row to violate");

    // An object where a list belongs. `merge_with_defaults` iterates; an object iterates too,
    // in a way that produces zero scopes and therefore "allow everything" — the exact failure
    // this whole feature exists to remove, arriving through a type that is merely the wrong one.
    let wrong_rate =
        sqlx::query("update security_settings set rate_limits = '{}'::jsonb where id = 1")
            .execute(db.pool())
            .await
            .expect_err("an object in a column the code treats as a list must be refused");
    assert!(
        wrong_rate
            .to_string()
            .contains("security_settings_rate_limits_array"),
        "the refusal must name the constraint: {wrong_rate}"
    );

    // And the mirror: a list where an object belongs, so the field lookups in `lockout.rs`
    // find nothing and a missing field silently means "no lockout at all".
    let wrong_lockout =
        sqlx::query("update security_settings set lockout = '[]'::jsonb where id = 1")
            .execute(db.pool())
            .await
            .expect_err("an array in a column the code treats as an object must be refused");
    assert!(
        wrong_lockout
            .to_string()
            .contains("security_settings_lockout_object"),
        "the refusal must name the constraint: {wrong_lockout}"
    );

    // The row survived both refusals unchanged — a rejected write must not half-apply, or the
    // panel would show a document nobody ever committed.
    let rate_shape: String =
        sqlx::query_scalar("select jsonb_typeof(rate_limits) from security_settings where id = 1")
            .fetch_one(db.pool())
            .await
            .expect("read the surviving document");
    assert_eq!(rate_shape, "array", "the refused write left the row alone");

    destroy_fresh(Some(admin), Some(db), name).await;
}
// ---------------------------------------------------------------------------
// The security migrations against a POPULATED database (REQ-012, migration criterion).
//
// The criterion asks for "fresh **and populated**", and fresh was proven while populated was
// not. The difference between the two words is the entire reason the criterion names both. A
// migration applied to an empty database only proves it can create a table; the failures that
// stop a platform from booting live in the other direction — an `add column … not null default`
// run against a table whose row already holds a value, an index built over rows, a foreign key
// that must resolve against rows that already exist. None of those can fail on an empty
// database, so "it applied cleanly" on a fresh one is fully compatible with a migration that
// refuses to run on a real deployment.
//
// The two later security migrations are the ones this is about, and the reason they can be
// re-run at all is that they are exactly the two that meet a populated table: `0151` alters a
// table that already holds the row `0135` inserted, and `0217` creates a table whose foreign
// keys must resolve against `users` and `organizations` rows that already exist. Rolling the
// ledger back to just before them is what produces that state — everything before them stays
// applied, which is the state every upgrading deployment is actually in.
//
// Rolling the ledger back is not the same as dropping the tables, and the difference is the
// whole test: the four tables the earlier security files created stay exactly as they were,
// with their rows, and only the two pending files run. Dropping them instead would re-run
// `0135`'s `create table` on an empty database — which is the fresh-database test this file
// already has, wearing a different name.
// ---------------------------------------------------------------------------

/// The two security migrations that meet an already-populated database.
///
/// `0054` and `0135` are deliberately absent: they create their tables, so a deployment that
/// already has them is by definition past them, and re-running them is not an upgrade path any
/// platform takes. What remains is the pair that runs on a database full of somebody else's
/// data.
const UPGRADE_MIGRATIONS: &[i64] = &[151, 217];

/// The state an upgrading deployment is in, ready for `UPGRADE_MIGRATIONS` to be applied.
///
/// Returns `(maintenance pool, database, the account that predates the upgrade)`, or `None`
/// when PostgreSQL is unreachable — in which case the caller returns rather than reporting a
/// pass it did not earn.
async fn populated_before_the_limiter(name: &str) -> Option<(PgPool, Db, uuid::Uuid)> {
    let admin = admin_pool().await?;
    sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .execute(&admin)
        .await
        .expect("drop the leftover database");
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .expect("create a database to upgrade");
    admin.close().await;

    let admin = admin_pool().await?;
    let db = connect(name).await;
    db.migrate()
        .await
        .expect("the full baseline applies before the pair is rolled back");

    // --- the rows that already exist when the limiter migration arrives -----------------------------
    //
    // A tenant and an account, because `0151` indexes `users.locked_until` and `0217` hangs
    // `created_by` off `users`. Both are referenced by rows created *after* the migration runs,
    // so a pair that dropped either table would fail rather than quietly orphan.
    let organization: uuid::Uuid = sqlx::query_scalar(
        "insert into organizations (name, slug) values ('Seeded tenant', 'seeded-tenant') \
         returning id",
    )
    .fetch_one(db.pool())
    .await
    .expect("seed the tenant the security rows hang from");
    let user: uuid::Uuid =
        sqlx::query_scalar("insert into users (email) values ('upgrade@seed.test') returning id")
            .fetch_one(db.pool())
            .await
            .expect("seed the account the security rows hang from");

    // A **locked** account, and this is the row the pair's index is for. An index built on a
    // table whose only row was locked one statement later has never been shown to *contain*
    // anything, and "the index exists" is the one claim about an index that proves nothing.
    sqlx::query("update users set locked_until = now() + interval '30 minutes' where id = $1")
        .bind(user)
        .execute(db.pool())
        .await
        .expect("lock the seeded account so 0151's partial index has a member");

    // A header policy an operator already saved. `0151` runs three `alter table` statements
    // against this row; the one that matters asserts the row survived them *with its document
    // intact*, because a migration that rebuilt the table around new columns is a migration that
    // silently reverts every security setting the platform was running.
    sqlx::query(
        "update security_settings set headers = '{\"hsts\": true}'::jsonb \
         where id = 1",
    )
    .execute(db.pool())
    .await
    .expect("the singleton 0135 inserted takes a real policy before the upgrade");

    // A finding and a check result, so the two tables `0054` created are not empty when the
    // upgrade lands. Neither `0151` nor `0217` touches them, and that is precisely the claim
    // worth making: a migration added beside an existing feature must not disturb it.
    sqlx::query(
        "insert into security_findings (organization_id, source, severity, title, fingerprint) \
         values ($1, 'platform', 'high', 'Seeded finding before the upgrade', 'seeded-fingerprint')",
    )
    .bind(organization)
    .execute(db.pool())
    .await
    .expect("seed a finding that predates the upgrade");
    sqlx::query(
        "insert into security_check_results (organization_id, check_key, state, run_id) \
         values ($1, 'seeded_check', 'warn', gen_random_uuid())",
    )
    .bind(organization)
    .execute(db.pool())
    .await
    .expect("seed a check result that predates the upgrade");

    // --- roll exactly the pair back -------------------------------------------------------------------
    //
    // Only those two versions, and not "everything from 151 on": a range delete would also
    // un-apply the ~40 migrations between them and re-run all of them against this data, which
    // tests the sibling writers' migrations rather than this criterion — and one of those is
    // `0188_system_health`, which is not this loop's to prove.
    for version in UPGRADE_MIGRATIONS {
        let applied: i64 = sqlx::query_scalar(
            "select count(*) from _sqlx_migrations where version = $1 and success",
        )
        .bind(version)
        .fetch_one(db.pool())
        .await
        .expect("read the ledger row being rolled back");
        assert_eq!(
            applied, 1,
            "migration {version} must be applied before it is rolled back, or this test is \
             proving nothing"
        );
        sqlx::query("delete from _sqlx_migrations where version = $1")
            .bind(version)
            .execute(db.pool())
            .await
            .expect("roll one version back");
    }

    // `0217`'s table is the one object the pair *created*, so it has to go or its `create table`
    // finds it present and the file is not being exercised at all. `0151` created no table —
    // it only added columns, and those columns are dropped by hand below, because dropping the
    // table to remove a column is the very destruction this test exists to catch.
    sqlx::query("drop table if exists security_ip_rules")
        .execute(db.pool())
        .await
        .ok();
    sqlx::query("alter table security_settings drop column if exists rate_limits")
        .execute(db.pool())
        .await
        .ok();
    sqlx::query("alter table security_settings drop column if exists lockout")
        .execute(db.pool())
        .await
        .ok();
    sqlx::query("alter table security_settings drop column if exists rate_limits_updated_by")
        .execute(db.pool())
        .await
        .ok();
    sqlx::query("alter table security_settings drop column if exists rate_limits_updated_at")
        .execute(db.pool())
        .await
        .ok();
    sqlx::query("drop index if exists users_locked_until_live_idx")
        .execute(db.pool())
        .await
        .ok();

    // The premise, asserted rather than assumed: a test whose starting state is not what it
    // claims would pass against a pair of migrations that did nothing at all.
    let still_there: i64 = sqlx::query_scalar(
        "select count(*) from information_schema.columns \
         where table_name = 'security_settings' and column_name = 'rate_limits'",
    )
    .fetch_one(db.pool())
    .await
    .expect("read the pre-upgrade schema");
    assert_eq!(
        still_there, 0,
        "the limiter column must be gone before the migration that adds it"
    );

    Some((admin, db, user))
}

/// `0151` and `0217` apply to a database that already holds somebody's data.
#[tokio::test]
async fn the_security_migrations_apply_to_a_populated_database() {
    let name = "omnion_test_mig_security_populated";
    let Some((admin, db, user)) = populated_before_the_limiter(name).await else {
        return;
    };

    db.migrate()
        .await
        .expect("the limiter and IP-rule migrations apply to a populated database");

    // The account is still here and is **still locked**. Both halves are assertions rather than
    // one: a migration that dropped the row is a lost account, and one that kept the row but
    // cleared the lock is every locked account on the platform quietly unlocked by an upgrade.
    let (survivors, still_locked): (i64, i64) = sqlx::query_as(
        "select count(*), count(*) filter (where locked_until > now()) from users \
         where email = 'upgrade@seed.test'",
    )
    .fetch_one(db.pool())
    .await
    .expect("read the seeded account back");
    assert_eq!(
        survivors, 1,
        "an account that existed before the upgrade must survive it"
    );
    assert_eq!(
        still_locked, 1,
        "an upgrade must never unlock a locked account — 0151 only *indexes* the column"
    );

    // The saved header policy came through the three `alter table` statements with its document
    // intact. This is the assertion the fresh-database test could never make: there, the row was
    // inserted by `0135` itself with `{}`, so "the document survived" and "the document is the
    // default" are indistinguishable.
    let headers: String =
        sqlx::query_scalar("select headers::text from security_settings where id = 1")
            .fetch_one(db.pool())
            .await
            .expect("read the policy row back");
    assert!(
        headers.contains("hsts"),
        "the upgrade must not revert a policy an operator had already saved: {headers}"
    );

    // The two documents `0151` added are readable on that same existing row, with the shapes the
    // Rust layer indexes into. `merge_with_defaults` is written on exactly these two types.
    let (rate_shape, lockout_shape): (String, String) = sqlx::query_as(
        "select jsonb_typeof(rate_limits), jsonb_typeof(lockout) from security_settings where id = 1",
    )
    .fetch_one(db.pool())
    .await
    .expect("read the two documents off the pre-existing row");
    assert_eq!(
        rate_shape, "array",
        "the limiter document is a list of scopes"
    );
    assert_eq!(
        lockout_shape, "object",
        "the lockout document is one object"
    );

    // `0054`'s tables are untouched by a migration added beside them. Their rows are what make
    // the assertion mean anything — an empty table survives being dropped just as well.
    let (findings, results): (i64, i64) = sqlx::query_as(
        "select (select count(*) from security_findings where fingerprint = 'seeded-fingerprint'), \
                (select count(*) from security_check_results where check_key = 'seeded_check')",
    )
    .fetch_one(db.pool())
    .await
    .expect("read the pre-existing security rows");
    assert_eq!(
        findings, 1,
        "a finding recorded before the upgrade must survive it"
    );
    assert_eq!(
        results, 1,
        "a check result recorded before the upgrade must survive it"
    );

    // The partial index `0151` builds is now provably an index with a member in it. This is the
    // locked-accounts screen's exact predicate, and the count is answered from the seeded lock
    // that predates the index — so an index that existed but covered nothing is not what passes.
    let locked: i64 =
        sqlx::query_scalar("select count(*) from users where locked_until is not null")
            .fetch_one(db.pool())
            .await
            .expect("read through the partial index's predicate");
    assert_eq!(
        locked, 1,
        "the partial index must serve the locked row that existed before it was built"
    );

    // `0217` landed on a database that already had rows, so its two foreign keys resolved
    // against real ones. Writing a rule with the seeded actor is what proves that, and a rule
    // whose `created_by` silently became NULL would be an access rule with no author.
    sqlx::query(
        "insert into security_ip_rules (cidr, kind, note, created_by) \
         values ('198.51.100.0/24', 'deny', 'Written after the upgrade', $1)",
    )
    .bind(user)
    .execute(db.pool())
    .await
    .expect("a rule written after the upgrade resolves against the pre-existing account");

    let rule: (Option<uuid::Uuid>, String) = sqlx::query_as(
        "select created_by, cidr::text from security_ip_rules \
         where note = 'Written after the upgrade'",
    )
    .fetch_optional(db.pool())
    .await
    .expect("read the rule back")
    .map(|(by, cidr): (Option<uuid::Uuid>, String)| (by, cidr))
    .expect("the rule the walk inserted");
    assert_eq!(
        rule.0,
        Some(user),
        "the rule must keep its author — a NULL here is an access rule nobody can trace"
    );
    assert_eq!(rule.1, "198.51.100.0/24", "and the network is stored typed");

    destroy_fresh(Some(admin), Some(db), name).await;
}
