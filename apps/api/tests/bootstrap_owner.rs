//! Integration test for the bootstrap Owner binding (docs/07-IAM.md §20).
//!
//! WHY THIS SUITE EXISTS
//! A fresh installation is seeded once at boot, in this order: `seed_iam` runs
//! `permissions::seed::ensure` — catalogue, base roles, Owner invariant — and only
//! THEN `bootstrap_admin` creates the very first account from `OMNION_ADMIN_EMAIL`.
//! The `ensure` call evaluated the invariant against a database with no accounts, so
//! it bound nobody, and the account the bootstrap created on the next line was left
//! with no role at all. Nothing about that shape looks broken from the outside: the
//! sign-in succeeds, the panel renders, and every permission-guarded route answers
//! `403 permission_denied`. The onboarding screen does not catch it either, because
//! `status()` derives its `steps.owner` from "are there accounts", not from "does
//! somebody hold the Owner role" — so the wizard reports the account as the owner
//! while it holds nothing.
//!
//! The fix re-asserts the invariant after the bootstrap. This suite pins the shape
//! that made it necessary, so a future reordering of the boot sequence cannot
//! reintroduce it silently: the ordering is the thing under test, not the query.
//!
//! Every test opens its own throwaway database, so the suite proves the acceptance
//! criterion against a real PostgreSQL and never touches the development database.
//! Without the compose stack it skips itself with a printed reason, like the other
//! integration suites.

use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::Db;
use uuid::Uuid;

/// The first account's password. `bootstrap_first_admin` runs the real strength
/// validation, so it has to be a genuinely acceptable password.
const PASSWORD: &str = "correct horse battery staple";

/// Open a throwaway database with every migration applied, run `body` against it,
/// then drop the database even when the body panicked.
async fn with_fresh_database<F, Fut>(label: &str, body: F)
where
    F: FnOnce(Db) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let Some((db, maintenance, database)) = fresh_database().await else {
        return;
    };

    body(db.clone()).await;

    db.pool().close().await;
    if let Err(err) = sqlx::query(&format!(
        "drop database if exists \"{database}\" with (force)"
    ))
    .execute(maintenance.pool())
    .await
    {
        eprintln!("WARN: could not drop {label} database: {err}");
    }
}

/// `(throwaway database, maintenance connection, its name)`, or `None` when the
/// compose PostgreSQL is not running.
async fn fresh_database() -> Option<(Db, Db, String)> {
    let config = Config::from_env().expect("environment must be valid");
    if !postgres_is_reachable(&config).await {
        return None;
    }

    let database = format!("omnion_owner_{}", Uuid::new_v4().simple());
    let maintenance = Db::connect(&maintenance_config(&config))
        .await
        .expect("the maintenance connection must work");
    sqlx::query(&format!("create database \"{database}\""))
        .execute(maintenance.pool())
        .await
        .expect("the temporary database must be created");

    let db = Db::connect(&DatabaseConfig {
        url: swap_database(&config.database.url, &database),
        max_connections: 2,
    })
    .await
    .expect("the fresh database must connect");
    db.migrate().await.expect("migrations must apply");

    Some((db, maintenance, database))
}

/// The order `main` seeds in: the invariant first, the first account second.
#[tokio::test]
async fn an_account_bootstrapped_after_the_seed_ends_up_holding_the_owner_role() {
    with_fresh_database("bootstrap_owner", |db| async move {
        // Step 1 — the boot sequence's IAM seed, on a database with no accounts yet.
        let first_pass = omnion_permissions::seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");
        assert_eq!(
            first_pass.owner_bound, None,
            "nothing can be bound before any account exists"
        );

        // Step 2 — `bootstrap_admin`, which runs on the very next line of `main`.
        let outcome = omnion_identity::users::bootstrap_first_admin(
            db.pool(),
            &format!("root-{}@omnion.test", Uuid::new_v4().simple()),
            PASSWORD,
        )
        .await
        .expect("the bootstrap must create the first account");
        let user_id = match outcome {
            omnion_identity::users::BootstrapOutcome::Created { user_id, .. } => user_id,
            omnion_identity::users::BootstrapOutcome::SkippedExistingUsers => {
                panic!("the first account must be created on an empty database")
            }
        };

        // Before the fix this is where the installation stopped: the account exists and
        // holds no role, so every permission-guarded route answers 403.
        assert_eq!(
            live_owner_count(&db).await,
            0,
            "the seed ran before the account existed, so no live Owner binding exists yet"
        );

        // Step 3 — the invariant, re-asserted after the bootstrap.
        let bound = omnion_permissions::seed::ensure_owner_binding(db.pool())
            .await
            .expect("the re-asserted invariant must run");
        assert_eq!(
            bound,
            Some(user_id),
            "the account the bootstrap created is the one that must receive the role"
        );

        assert_eq!(
            live_owner_count(&db).await,
            1,
            "exactly one live Owner binding exists after the re-assertion"
        );
    })
    .await;
}

/// The invariant is idempotent: a boot that re-runs it must not pile up bindings or
/// move the role onto a second account.
#[tokio::test]
async fn re_asserting_the_invariant_twice_does_not_double_bind_or_move_the_role() {
    with_fresh_database("owner_idempotence", |db| async move {
        // `seed_iam` runs `ensure` (catalogue + base roles) before it re-asserts the
        // invariant, so the roles exist by the time `ensure_owner_binding` is called.
        omnion_permissions::seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        omnion_identity::users::bootstrap_first_admin(
            db.pool(),
            &format!("first-{}@omnion.test", Uuid::new_v4().simple()),
            PASSWORD,
        )
        .await
        .expect("the first account must be created");

        let first = omnion_permissions::seed::ensure_owner_binding(db.pool())
            .await
            .expect("the first re-assertion must run");
        assert!(first.is_some(), "the first call binds the owner");

        let second = omnion_permissions::seed::ensure_owner_binding(db.pool())
            .await
            .expect("the second re-assertion must run");
        assert_eq!(
            second, None,
            "the invariant already holds, so the second call binds nobody"
        );
        assert_eq!(
            live_owner_count(&db).await,
            1,
            "a boot that re-runs the seed must not create a second Owner binding"
        );

        // A later account must not steal the role: the earliest account keeps it.
        omnion_identity::users::create_user(
            db.pool(),
            omnion_identity::users::NewUser {
                email: format!("second-{}@omnion.test", Uuid::new_v4().simple()),
                password: PASSWORD.to_owned(),
                display_name: "Second".to_owned(),
                organization_id: None,
            },
        )
        .await
        .expect("the second account must be created");

        assert_eq!(
            omnion_permissions::seed::ensure_owner_binding(db.pool())
                .await
                .expect("the invariant must still run"),
            None,
            "an installation that already has an Owner does not bind a second one"
        );
        assert_eq!(live_owner_count(&db).await, 1);
    })
    .await;
}

/// The binding is what makes the panel usable, so the acceptance criterion is not
/// "a row exists" but "a permission-guarded route answers for this account".
#[tokio::test]
async fn the_bootstrapped_owner_holds_a_real_permission_not_just_a_row() {
    with_fresh_database("owner_permission", |db| async move {
        // The boot order again: roles first, the invariant last.
        omnion_permissions::seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        omnion_identity::users::bootstrap_first_admin(
            db.pool(),
            &format!("perm-{}@omnion.test", Uuid::new_v4().simple()),
            PASSWORD,
        )
        .await
        .expect("the first account must be created");

        omnion_permissions::seed::ensure_owner_binding(db.pool())
            .await
            .expect("the invariant must run");

        let granted = granted_permission_count(&db).await;
        assert!(
            granted > 0,
            "the Owner role must carry a permission set, or the row is cosmetic and every \
             guarded route still answers 403"
        );

        // `observability.read` is the permission REQ-126's screens hang off. Asserting
        // it specifically ties this suite to the request that surfaced the defect.
        let owner_role_id: Uuid = sqlx::query_scalar(
            "select id from roles where key = 'owner' and organization_id is null limit 1",
        )
        .fetch_one(db.pool())
        .await
        .expect("the base roles must exist");

        let holds_observability_read: bool = sqlx::query_scalar(
            "select exists ( \
                 select 1 from role_permissions rp \
                 join permissions p on p.key = rp.permission_key \
                 where rp.role_id = $1 and p.key = 'observability.read')",
        )
        .bind(owner_role_id)
        .fetch_one(db.pool())
        .await
        .expect("the permission check must run");

        assert!(
            holds_observability_read,
            "the Owner role must hold `observability.read` — that is the permission the \
             observability screens are guarded by"
        );
    })
    .await;
}

/// Live Owner bindings: `owner`, platform-wide, not revoked, not expired.
async fn live_owner_count(db: &Db) -> i64 {
    sqlx::query_scalar(
        "select count(*) from role_bindings b \
         join roles r on r.id = b.role_id \
         where r.key = 'owner' and r.organization_id is null \
           and b.revoked_at is null and (b.expires_at is null or b.expires_at > now())",
    )
    .fetch_one(db.pool())
    .await
    .expect("the binding count must be readable")
}

/// How many permissions the Owner role actually allows.
///
/// `role_permissions.effect` is `allow` or `deny`, so counting rows would let a
/// role that grants nothing but denials pass this assertion.
async fn granted_permission_count(db: &Db) -> i64 {
    sqlx::query_scalar(
        "select count(*) from role_permissions rp \
         join roles r on r.id = rp.role_id \
         where r.key = 'owner' and r.organization_id is null and rp.effect = 'allow'",
    )
    .fetch_one(db.pool())
    .await
    .expect("the permission count must be readable")
}

/// Connect to the compose PostgreSQL; `false` means the stack is not running.
async fn postgres_is_reachable(config: &Config) -> bool {
    match Db::connect(&DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 1,
    })
    .await
    {
        Ok(db) => {
            db.pool().close().await;
            true
        }
        Err(err) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({err}) — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
            );
            false
        }
    }
}

/// Maintenance connection (`postgres` database) used to create and drop throwaway databases.
fn maintenance_config(config: &Config) -> DatabaseConfig {
    DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 1,
    }
}

/// Replace the database name in a PostgreSQL connection string.
fn swap_database(url: &str, database: &str) -> String {
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
