//! The one way an integration walk is allowed to give up, and the one that is a defect.
//!
//! ## Why this module exists
//!
//! Six REQ-126 walks each carried their own copy of a helper that ended in
//!
//! ```text
//! if let Err(error) = db.migrate().await {
//!     eprintln!("SKIP: the migrations did not apply ({error})");
//!     return None;
//! }
//! ```
//!
//! and every test in the file began with
//!
//! ```text
//! let Some(state) = state_or_skip().await else { return; };
//! ```
//!
//! **`return` from a test is a PASS.** libtest counts a function that returns `Ok(())`, so a walk
//! whose database refused to migrate reported `test result: ok. 9 passed` while every assertion
//! in the file had been skipped. The `eprintln!` was the only evidence, and libtest captures
//! stderr from a passing test — so the evidence was visible only under `--nocapture`, which is
//! exactly what a green run is not read with. This is not hypothetical: the `omnion_w6_*`
//! development databases each carried a stale `sqlx` checksum for `0019_secret_hierarchy.sql`
//! (a migration this writer's own REQ-125 edited after it had already been applied), and **every
//! observability walk in this repository was a no-op for two ticks** while the suite reported
//! green. A sibling walk had already found the same shape and written a `state_or_fail` of its
//! own; this module is that helper, generalised so the next walk inherits the rule instead of
//! re-deriving it.
//!
//! ## The rule this module encodes
//!
//! **A migration failure is a DEFECT, never an environment condition.** A missing PostgreSQL is an
//! environment condition and is skipped, loudly, with a banner naming the variable to set. A
//! migration that will not apply is a broken repository: the schema in `database/migrations/` is
//! part of what these walks test, and "the developer had not run the new migration yet" is
//! repaired by running it. There is no environment in which that is a legitimate skip.
//!
//! So: unreachable database, invalid configuration → **skip loudly**. Everything from `migrate()`
//! onwards → **fail**, with sqlx's own message, which names the version and whether it is
//! missing, duplicated or modified.

use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_permissions::seed;
use omnion_storage::{Storage, StorageConfig};

/// Build the state for a walk, or fail.
///
/// Panics on anything that is not "the environment is absent", and — unlike the six copies this
/// replaces — it never reports a skipped walk as a passed test.
pub async fn state_or_fail() -> AppState {
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => skip(&format!(
            "the configuration is not valid ({error}) — set OMNION_DATABASE_URL, REDIS_URL and \
             the storage variables"
        )),
    };
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(error) => skip(&format!(
            "PostgreSQL is not reachable ({error}) — start it with \
             `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
        )),
    };

    // From here on there is no legitimate way out. `migrate()` is the repository's own schema
    // claiming to be stale, and a walk that continued without it would assert against a database
    // that is not the one the code expects.
    if let Err(error) = db.migrate().await {
        panic!(
            "the migrations did not apply: {error}\n\
             This is a repository defect, not a missing environment: the schema in \
             database/migrations/ is part of what these walks test.\n\
             If an earlier edit changed a migration that had already been applied, the development \
             database has to be rebuilt (DROP and CREATE), because sqlx records a checksum per \
             version and refuses to re-apply a modified one."
        );
    }

    seed::ensure(db.pool())
        .await
        .expect("the permission catalogue seeds");
    let redis = RedisClient::new(&config.redis.url).expect("a redis url");
    let storage = Storage::from_config(&StorageConfig::default())
        .expect("the default storage configuration is valid");
    AppState::new(
        BuildInfo::new("omnion-api", env!("CARGO_PKG_VERSION")),
        config,
        db,
        redis,
        storage,
    )
}

/// Print a loud banner and end the process, rather than returning a `None` the caller turns into
/// a silent `return`.
///
/// A skipped walk must be *unmistakable* in the output. The banner is on stderr and the process
/// exits 101, so `cargo test` prints `error: test failed` — the same shape as a real failure,
/// because from CI's point of view it is one: this run proved nothing.
fn skip(reason: &str) -> ! {
    eprintln!(
        "\n\
         ============================================================\n\
         WALK DID NOT RUN: {reason}\n\
         Every assertion in this file was skipped. A 'test result: ok' above\n\
         this line is a vacuous pass, not evidence.\n\
         ============================================================\n"
    );
    std::process::exit(101);
}
