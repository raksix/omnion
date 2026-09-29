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
use std::sync::OnceLock;

/// Take exclusive ownership of the **whole-database** evaluator, for a walk that needs it.
///
/// ## Why a walk needs this
///
/// The alert evaluator is not scoped. `evaluate` walks *every* enabled rule in
/// `obs_alert_rules`, applies the current window to each, and reports what each one did. There is
/// no rule id parameter, and there cannot be one without changing what an alert rule means: the
/// point of the sweep is that it sees the whole set, so a second operator's rule is evaluated
/// alongside yours.
///
/// That is correct for production and hostile to a test binary. `observability_alerts.rs` holds
/// eight walks, libtest runs them in parallel, and every one of them creates a rule and then calls
/// the sweep. So each walk's assertions were computed over the union of all eight rule sets:
/// `evaluated: 9` where the walk expected `1`, `silenced: 2` where it expected `1`, and a rule with
/// no samples at all opening an event because a *sibling's* rule was breaching.
///
/// The failure is nastier than a flaky count, because the number is not random — it grows with the
/// number of tests, so a suite that passes on one machine and fails on another depending on the
/// thread count is a suite that reports product defects that do not exist.
///
/// ## Why a mutex over one walk per binary
///
/// Cargo already runs test *binaries* one at a time, so the process boundary is not available as
/// isolation: the eight walks here are eight tests in one binary, and they were never going to be
/// separated by a build flag without giving up the parallelism the other files rely on. A process
/// mutex held for the duration of a walk is the smallest unit that actually contains the sweep.
///
/// The guard is a `tokio::sync::Mutex` in a `OnceLock`, so a walk holds it across `.await` points
/// and the runtime can still use every thread it has. It is poisoned rather than recovered: a walk
/// that panicked mid-sweep leaves the database in a state the next walk cannot reason about, and
/// silently continuing would turn a loud failure into a wrong count.
///
/// Acquire it with [`exclusive_evaluator`], and drop it when the walk ends.
static EVALUATOR_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

/// The guard type returned by [`exclusive_evaluator`].
pub type EvaluatorGuard = tokio::sync::MutexGuard<'static, ()>;

/// Wait for exclusive use of the database-wide evaluator. See [`EVALUATOR_LOCK`] for why.
pub async fn exclusive_evaluator() -> EvaluatorGuard {
    EVALUATOR_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Clear every alert row, so a walk starts from a known-empty evaluator.
///
/// Separate from the lock on purpose: the lock says "nobody else is sweeping", this says "there is
/// nothing in the table". A walk that only takes the lock still sees whatever a *previous* walk in
/// the same binary left behind, and a leftover rule is exactly the thing that made the counts wrong.
/// So this is called with the lock held, by every walk that sweeps.
///
/// The process-wide **metric** registry is cleared in the same breath, and for the same reason
/// with one extra step. `evaluate_pass` reads `global()` — an alert that evaluated a private
/// registry would never fire on real traffic — so a rule over `omnion_queue_depth` is being
/// evaluated against whatever the last walk recorded. A walk whose premise is "nothing recorded
/// yet, so no data never breaches" cannot set that premise while a sibling's `queue_depth 7.0` is
/// still inside the window, and rolling the clock forward does not help: the sample is still there.
///
/// The settings row is deliberately left alone: `obs_log_settings` is a single shared row that
/// walks read and write, and truncating it would turn one walk's teardown into another's failure.
pub async fn clear_alert_state(pool: &sqlx::PgPool) {
    sqlx::query("delete from obs_alert_events")
        .execute(pool)
        .await
        .expect("the alert events are deletable");
    sqlx::query("delete from obs_silences")
        .execute(pool)
        .await
        .expect("the silences are deletable");
    sqlx::query("delete from obs_alert_rules")
        .execute(pool)
        .await
        .expect("the alert rules are deletable");
    omnion_telemetry::metrics::clear_global_samples();
}

/// The database a connection string names, for a failure message.
///
/// Deliberately not a URL parser: a walk's database is the last path segment in every form this
/// repository uses, and a wrong answer here costs a tick of guessing, so anything unparsable
/// returns the raw string rather than `None`. **The password is never printed** — the panics this
/// feeds are read aloud in CI logs, and a connection string is the one place a secret is most
/// likely to be copied into a ticket.
fn database_name(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest)?;
    let last = after_scheme.rsplit('/').next()?;
    let name = last.split('?').next().unwrap_or(last);
    (!name.is_empty() && name.contains(|c: char| c.is_ascii_alphanumeric() || c == '_'))
        .then(|| name.to_owned())
}

/// A CSRF secret for the walks, defaulted rather than required.
///
/// The API's CSRF layer REFUSES cookie-authenticated writes when `OMNION_CSRF_SECRET` is unset,
/// which is the right production behaviour — a platform that silently drops CSRF protection is
/// worse than one that visibly refuses writes. The consequence for a walk is that sign-in mints
/// no `omnion_csrf` cookie at all, so every suite that signs in and then POSTs dies on a
/// `403 csrf_unavailable` or on a missing cookie, and neither failure names its real cause.
///
/// Setting it here rather than in `scripts/qa/run.sh` and `scripts/qa/run-workspace-tests.sh`
/// alone is the point: those two wrappers are the only place it used to be set, so a bare
/// `cargo test -p omnion-api --test observability_logs` — the command a developer runs to check
/// one suite — failed for a reason that has nothing to do with the suite. The default is a
/// disposable value for a disposable database and is not a credential for anything; an operator
/// that exports the variable still gets theirs.
///
/// # Safety
///
/// `set_var` is `unsafe` in edition 2024 and this is a process-wide environment variable that
/// every test in the binary reads. It is set to a constant, so a race can only ever write the
/// same value the reader expects. The walk files already use this exact guard for
/// `OMNION_DATABASE_URL`.
pub fn ensure_csrf_secret() {
    if std::env::var("OMNION_CSRF_SECRET").is_ok_and(|value| !value.trim().is_empty()) {
        return;
    }
    // SAFETY: see the note above — the value is a constant, so ordering between tests cannot
    // produce a different answer than the one a reader expects.
    unsafe {
        std::env::set_var(
            "OMNION_CSRF_SECRET",
            "qa-walk-csrf-secret-not-a-credential",
        );
    }
}

/// Install a rate-limit document a whole test binary can spend.
///
/// Called from the same place as the CSRF default, and for the same reason: the router installs
/// whatever limiter is already in the process-wide `OnceLock`, so the FIRST walk to build a
/// router decides the budget for every other walk in that binary. With the shipped defaults the
/// sign-in scope allows ten requests per five minutes, eleven walks exhaust it, and the
/// eleventh fails on a `429` inside a test that never mentions rate limiting.
pub fn ensure_test_rate_limits(state: &omnion_api::state::AppState) {
    let _ = omnion_api::rate_limit_middleware::install(
        omnion_api::rate_limit_middleware::RateLimiter::new(
            state,
            omnion_security::RatePolicy::for_tests(),
        ),
    );
}

/// Build the state for a walk, or fail.
///
/// Panics on anything that is not "the environment is absent", and — unlike the six copies this
/// replaces — it never reports a skipped walk as a passed test.
pub async fn state_or_fail() -> AppState {
    ensure_csrf_secret();
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
        // Name the database. This message cost two ticks to diagnose: "migration 19 was
        // previously applied but has been modified" is a **symptom of pointing at somebody
        // else's database**, because a shared one records a DIFFERENT migration 19 (wave 2's
        // `0019_cms_blocks` against this writer's `0019_secret_hierarchy`) and the checksum can
        // never agree. Neither the file nor the commit named the target, so the reading was
        // "someone edited an applied migration" and the fix was "rebuild the database", which
        // is not it at all. One line of context turns a guess into a lookup.
        let target = database_name(&config.database.url).unwrap_or_else(|| "<unparsable>".into());
        panic!(
            "the migrations did not apply to `{target}`: {error}\n\
             This is a repository defect, not a missing environment: the schema in \
             database/migrations/ is part of what these walks test.\n\
             Two causes, and which one this is depends on the database named above:\n\
             - an applied migration was edited afterwards, so this database has to be rebuilt \
               (DROP and CREATE) because sqlx records a checksum per version;\n\
             - this is NOT the database the migrations in this tree describe. Every writer has \
               their own (`omnion_w6_dev` here), and a shared one carries a different migration \
               at the same number, which the checksum can never match. Check OMNION_DATABASE_URL \
               before you rebuild anything."
        );
    }

    seed::ensure(db.pool())
        .await
        .expect("the permission catalogue seeds");
    let redis = RedisClient::new(&config.redis.url).expect("a redis url");
    let storage = Storage::from_config(&StorageConfig::default())
        .expect("the default storage configuration is valid");
    let state = AppState::new(
        BuildInfo::new("omnion-api", env!("CARGO_PKG_VERSION")),
        config,
        db,
        redis,
        storage,
    );
    // Installed AFTER the state exists and BEFORE any walk builds a router, because the router
    // installs whatever limiter is already in the `OnceLock` — so this call is the difference
    // between a binary whose walks share a spendable budget and one where the eleventh walk
    // fails on a `429` it never asked for.
    ensure_test_rate_limits(&state);
    state
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
