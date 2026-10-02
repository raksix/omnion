//! A throwaway database per walk, for every suite under `tests/` that needs one.
//!
//! # The defect this closes
//!
//! Most suites in this directory opened whatever `OMNION_DATABASE_URL` named and ran their
//! walks in it. On a writer's box that is the **shared QA database** — the same rows every
//! other stack on the box points at, in every other writer's worktree. Two consequences, and
//! neither of them names the behaviour under test:
//!
//! * Two walks inserting the same fixture key collide on a unique index, so the failure is a
//!   duplicate-key error about a name the test invented rather than anything about the code.
//! * `seed::ensure` binds the built-in Owner role to the **earliest user in the database**.
//!   On a shared database that is whichever walk inserted first, so a walk that leans on Owner
//!   without granting it measures the schedule instead of the guard — and it passes on the
//!   writer whose walk landed first and fails on every other one.
//!
//! A third failure is quieter: a run whose credentials were wrong connected nowhere, took the
//! skip branch and reported `13 passed` with nothing executed. Cargo **captures** `eprintln!`,
//! so the summary alone cannot tell a skip from a pass. [`announce_skip`] counts skips, so the
//! suite can be red about it — see [`assert_nothing_skipped`].
//!
//! # Why a module and not a copy per suite
//!
//! Twelve suites carried the same fifteen lines, and `cms_themes` had already fixed the bug
//! on its own — with the measurements that made the fix obvious written down beside it. Twelve
//! copies of that fix is twelve places for the next one to miss, and the copies would drift:
//! some would keep sweeping, some would not, and a reader could not tell which. So the fixed
//! version lives here once and each suite keeps only its own prefix.
//!
//! # The cleanup that actually holds
//!
//! [`IsolatedDb::dispose`] is not a `Drop` guard, and that is deliberate and measured: a panic
//! inside `#[tokio::test]` unwinds the runtime **task**, not the future the macro awaits, so a
//! guard in that future never runs. Twelve databases accumulated over six failing runs while
//! the guard was believed to be "fixed". The next run is the only process guaranteed to exist
//! on the failing path, so [`IsolatedDb::open`] sweeps whatever a previous run left behind
//! before it takes a new name — and that holds whatever the last run did.
//!
//! # Usage
//!
//! ```ignore
//! let Some(isolated) = IsolatedDb::open(&config.database.url, 4, "themes").await? else {
//!     return Ok(()); // the skip was announced; the gate below turns it red
//! };
//! let state = /* router over isolated.db */;
//! // … the walk …
//! isolated.dispose().await;
//! ```

use omnion_core::config::DatabaseConfig;
use omnion_core::error::Result as CoreResult;
use omnion_core::Db;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::atomic::{AtomicUsize, Ordering};
use uuid::Uuid;

/// Walks in this binary that announced they did not run.
///
/// Per **binary**, which is per file here: cargo compiles every `tests/*.rs` into its own
/// binary, so a suite's walks share one counter and no other suite can bump it. That is what
/// makes the gate below a statement about *this file* rather than about the box.
static SKIPPED: AtomicUsize = AtomicUsize::new(0);

/// Say a walk did not run, in a way cargo cannot hide.
///
/// The reason goes to **stdout** rather than stderr on purpose: cargo captures a passing
/// test's output, and the skip branch returning `Ok(())` is a passing test.
pub fn announce_skip(reason: &str) {
    SKIPPED.fetch_add(1, Ordering::SeqCst);
    println!("WALK SKIPPED: {reason}");
}

/// How many walks in this binary announced that they did not run.
pub fn skipped_count() -> usize {
    SKIPPED.load(Ordering::SeqCst)
}

/// The assertion behind [`register_honesty_gate`].
///
/// A file of walks that skips everything and reports `ok` is the worst outcome a test suite
/// can produce, because it is indistinguishable from success in every artifact a human or a CI
/// job reads. Panic only when the count is non-zero — the run is otherwise untouched.
pub fn assert_nothing_skipped() {
    let skipped = skipped_count();
    assert_eq!(
        skipped, 0,
        "{skipped} walk(s) in this file were SKIPPED, not passed. Their output above says why \
         (a database that cannot be reached, or one that cannot be created). A skipped walk \
         reports `ok`, which is how a suite that measured nothing becomes a green line."
    );
}

/// A database of this walk's own, plus the maintenance connection that removes it.
///
/// `maintenance` stays open for the walk's lifetime on purpose: a database cannot be dropped
/// while a session is attached to it, so the pool that creates it is the pool that has to
/// still be connected when it is dropped.
pub struct IsolatedDb {
    /// The handle every query in the walk goes through, migrations already applied.
    pub db: Db,
    /// A pool on the **parent** database — the one the environment named.
    maintenance: PgPool,
    /// The name this walk's database was given.
    pub name: String,
}

impl IsolatedDb {
    /// Open a throwaway database with every migration applied.
    ///
    /// `prefix` names the walk family for the sweep below, so two suites never sweep each
    /// other's leftovers and a reader can tell whose leaked what.
    ///
    /// Returns `None` — after announcing the skip — when the parent database is not
    /// reachable or the new database cannot be created or migrated. A walk in that state has
    /// measured nothing, so it must announce rather than quietly pass; the gate is the caller's
    /// to register.
    pub async fn open(
        url: &str,
        max_connections: u32,
        prefix: &str,
    ) -> CoreResult<Option<Self>> {
        let maintenance = match PgPoolOptions::new()
            .max_connections(2)
            .connect(url)
            .await
        {
            Ok(pool) => pool,
            Err(error) => {
                announce_skip(&format!("PostgreSQL is not reachable ({error})"));
                return Ok(None);
            }
        };

        // Drop anything a PREVIOUS run left behind, before taking a new name.
        //
        // The next run is the only process guaranteed to exist on the failing path, so the
        // sweep belongs here and not in the dispose. The LIKE pattern is built into a Rust
        // **raw** string: it needs backslashes to escape its own wildcard characters, and
        // escaping those once for Rust and once for SQL inside a normal literal is how a
        // query silently matches nothing — which is exactly what the first version of this
        // line did. Correct in psql, empty from the suite.
        //
        // `not exists (select 1 from pg_stat_activity …)` is the other half: a database with
        // a live session cannot be dropped, and on a box where several suites run at once
        // "left behind" and "in use right now" are different questions.
        let pattern = format!(r"omnion\_{}\_%", prefix.replace('_', r"\_"));
        let stale: Vec<String> = sqlx::query_scalar(
            r#"
            select datname
              from pg_database
             where datname like $1
               and datname <> current_database()
               and not exists (
                   select 1 from pg_stat_activity a where a.datname = pg_database.datname
               )
            "#,
        )
        .bind(&pattern)
        .fetch_all(&maintenance)
        .await
        .unwrap_or_default();
        for database in stale {
            // A failure here is not this walk's failure: another writer may still hold it, and
            // a sweep that refused to start would be worse than a sweep that skips one.
            if let Err(error) =
                sqlx::query(&format!(r#"drop database if exists "{database}" with (force)"#))
                    .execute(&maintenance)
                    .await
            {
                println!("WALK NOTE: could not drop the stale database {database} ({error})");
            }
        }

        let name = format!("omnion_{prefix}_{}", &Uuid::new_v4().simple().to_string()[..12]);
        if let Err(error) = sqlx::query(&format!(r#"create database "{name}""#))
            .execute(&maintenance)
            .await
        {
            announce_skip(&format!("a throwaway database could not be created ({error})"));
            return Ok(None);
        }

        let db = Db::connect(&DatabaseConfig {
            url: swap_database(url, &name),
            max_connections,
        })
        .await?;
        if let Err(error) = db.migrate().await {
            // The database exists but carries no schema, and it is not in `stale` under any
            // name the next sweep would guess if the create were renamed. Drop it here so a
            // migration failure does not leave a half-built database behind.
            let _ = sqlx::query(&format!(r#"drop database if exists "{name}" with (force)"#))
                .execute(&maintenance)
                .await;
            announce_skip(&format!("the throwaway database could not be migrated ({error})"));
            return Ok(None);
        }

        Ok(Some(Self {
            db,
            maintenance,
            name,
        }))
    }

    /// Drop the throwaway database. Call it when the walk RETURNS, not on the panicking path
    /// — see the module note for why nothing else is possible here.
    pub async fn dispose(&mut self) {
        let name = self.name.clone();
        self.db.pool().close().await;
        let _ = sqlx::query(&format!(r#"drop database if exists "{name}" with (force)"#))
            .execute(&self.maintenance)
            .await;
        self.maintenance.close().await;
    }

}

/// Point a connection string at a different database, keeping host, port and credentials.
///
/// The query string is carried over because a deployment that names its options through it —
/// a statement timeout, an application name — must not silently lose them when the same
/// credentials are pointed at another database.
pub fn swap_database(url: &str, database: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let prefix = base
        .rsplit_once('/')
        .expect("the URL must contain a database path")
        .0;
    match query {
        Some(query) => format!("{prefix}/{database}?{query}"),
        None => format!("{prefix}/{database}"),
    }
}
