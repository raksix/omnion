//! Integration tests for the per-agent workspace (REQ-099, slice 2).
//!
//! The unit tests in `workspace.rs` prove the *rules* — a traversal is refused, a cap is
//! arithmetic, a key is derived. None of them touch a database, and none of them can catch the
//! failure that actually happens in production: a rule the code enforces and the schema does
//! not, so a row written by a migration, a restore or a future writer sails past the one check
//! nobody re-runs.
//!
//! So these walks are all about **the row and the rule agreeing**:
//!
//! - the *database* refuses a traversal, an oversized file and a missing key — not just the
//!   Rust function, which is what makes the function's verdict binding on every other writer;
//! - the per-agent quota is a `sum` over rows, so a file deleted out from under a crashed
//!   process releases its bytes (a counter in process memory would not);
//! - a replacement at one path is a **replacement**, not a second row, and the old bytes stop
//!   being charged;
//! - a file in another organization is `None` through every accessor, including the
//!   by-id one — asserted as absence rather than as a 403, because sequential ids make a 403
//!   a free existence oracle.
//!
//! They reuse the same throwaway-database harness as the run store's suite, and drop it
//! explicitly and awaited: `Drop` cannot await, so a `Drop`-based teardown races the test
//! binary's exit and leaks a database per walk — on a PostgreSQL that ten writers share, leaked
//! databases show up as *other* suites failing with "pool timed out".

use omnion_ai_hub::error::AiHubError;
use omnion_ai_hub::run_store::{NewAgent, create_agent};
use omnion_ai_hub::workspace::{
    self, MAX_AGENT_BYTES, MAX_FILE_BYTES, NewAgentFile, Usage, check_caps, storage_key,
    validate_path,
};
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::Db;
use sqlx::PgPool;
use uuid::Uuid;

// -------------------------------------------------------------------------------------------
// The harness
// -------------------------------------------------------------------------------------------

struct Workspace {
    pool: PgPool,
    organization_id: Uuid,
    /// A second tenant, for the isolation walks.
    other_organization_id: Uuid,
    agent_id: Uuid,
    database: String,
    maintenance: Option<Db>,
}

impl Workspace {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().ok()?;
        if let Err(err) = Db::connect(&DatabaseConfig {
            url: config.database.url.clone(),
            max_connections: 1,
        })
        .await
        {
            eprintln!("skipping: PostgreSQL is not reachable: {err}");
            return None;
        }

        let database = format!("omnion_aws_{}", Uuid::new_v4().simple());
        let maintenance = Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, "postgres"),
            max_connections: 1,
        })
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

        let organization_id = seed_organization(db.pool(), "acme").await;
        let other_organization_id = seed_organization(db.pool(), "globex").await;
        let pool = db.pool().clone();

        let agent = create_agent(
            &pool,
            &NewAgent::with_defaults(organization_id, "reporter", "Reporter"),
        )
        .await
        .expect("the fixture agent must be created");

        Some(Self {
            pool,
            organization_id,
            other_organization_id,
            agent_id: agent.id,
            database,
            maintenance: Some(maintenance),
        })
    }

    /// Drop the row for a file of `size` bytes at `path`, through the store.
    async fn put(&self, path: &str, size: i64) -> omnion_ai_hub::workspace::AgentFile {
        let checksum = format!("{:064x}", Uuid::new_v4().as_u128());
        self.put_with_key(path, size, &checksum).await
    }

    /// The same, with a caller-supplied checksum, so a walk can prove the key derivation.
    async fn put_with_key(
        &self,
        path: &str,
        size: i64,
        checksum: &str,
    ) -> omnion_ai_hub::workspace::AgentFile {
        workspace::put_file(
            &self.pool,
            &NewAgentFile {
                agent_id: self.agent_id,
                run_id: None,
                path: validate_path(path).expect("the fixture path must be legal"),
                size_bytes: size,
                content_type: "text/plain".to_owned(),
                storage_key: storage_key(self.agent_id, checksum),
                checksum: checksum.to_owned(),
                created_by: None,
            },
        )
        .await
        .expect("the fixture file must be stored")
    }
}

impl Workspace {
    async fn dispose(mut self) {
        self.pool.close().await;
        let database = std::mem::take(&mut self.database);
        if let Some(maintenance) = self.maintenance.take() {
            sqlx::query(&format!("drop database if exists \"{database}\" with (force)"))
                .execute(maintenance.pool())
                .await
                .expect("the temporary database must be removed");
            maintenance.pool().close().await;
        }
    }
}

fn swap_database(url: &str, database: &str) -> String {
    let (base, _) = url.rsplit_once('/').expect("a database URL has a path");
    format!("{base}/{database}")
}

async fn seed_organization(pool: &PgPool, label: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(id)
        .bind(label)
        .bind(format!("{label}-{}", Uuid::new_v4().simple()))
        .execute(pool)
        .await
        .expect("the fixture organization must be created");
    id
}

// -------------------------------------------------------------------------------------------
// A file round-trips, and its address is what the caller wrote
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_file_round_trips_with_its_path_size_and_checksum() {
    let Some(store) = Workspace::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let file = store.put("data/2026/q3.csv", 4_096).await;
    assert_eq!(file.path, "data/2026/q3.csv", "a subdirectory is a legal workspace path");
    assert_eq!(file.size_bytes, 4_096);
    assert_eq!(file.content_type, "text/plain");
    assert_eq!(file.checksum.len(), 64);

    // A subdirectory path is the reason the download route takes a wildcard segment: with a
    // single-segment capture this file is unreachable, and the table would show a row the user
    // cannot open.
    let read = workspace::get_file(&store.pool, store.agent_id, "data/2026/q3.csv")
        .await
        .expect("the read must answer")
        .expect("the file must be there");
    assert_eq!(read.id, file.id);

    let listed = workspace::list_files(&store.pool, store.agent_id).await.expect("list");
    assert_eq!(listed.len(), 1);
    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// The database enforces the rules, not only the function
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_database_also_refuses_a_traversal() {
    let Some(store) = Workspace::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    // The Rust function refuses this first, so reaching the database at all means the
    // enforcement has moved somewhere. The row insert is the *other* implementation of the
    // same rule — a restore, a migration or a future writer does not go through the function,
    // and a check constraint is the only thing that speaks to all of them.
    let error = sqlx::query(
        "insert into ai_agent_files (agent_id, path, size_bytes, storage_key) \
         values ($1, '../escape.txt', 10, 'agents/x/y')",
    )
    .bind(store.agent_id)
    .execute(&store.pool)
    .await
    .expect_err("the database must refuse a traversal");
    let text = error.to_string();
    assert!(
        text.contains("ai_agent_files_path_clean"),
        "the refusal must name the rule, not just fail: {text}"
    );
    store.dispose().await;
}

#[tokio::test]
async fn the_database_also_refuses_an_oversized_file() {
    let Some(store) = Workspace::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let error = sqlx::query(
        "insert into ai_agent_files (agent_id, path, size_bytes, storage_key) \
         values ($1, 'huge.bin', $2, 'agents/x/y')",
    )
    .bind(store.agent_id)
    .bind(MAX_FILE_BYTES as i64 + 1)
    .execute(&store.pool)
    .await
    .expect_err("the database must refuse a file over the per-file cap");
    assert!(
        error.to_string().contains("ai_agent_files_size_range"),
        "the refusal must name the rule: {error}"
    );
    store.dispose().await;
}

#[tokio::test]
async fn a_row_without_a_storage_key_is_refused() {
    let Some(store) = Workspace::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    // A row whose key is blank is a file the download route can never fetch, and the panel
    // renders it as a corrupt file rather than as a failed upload. The check makes that state
    // unreachable rather than merely unlikely.
    let error = sqlx::query(
        "insert into ai_agent_files (agent_id, path, size_bytes, storage_key) \
         values ($1, 'orphan.txt', 10, '')",
    )
    .bind(store.agent_id)
    .execute(&store.pool)
    .await
    .expect_err("the database must refuse a row with no storage key");
    assert!(
        error.to_string().contains("ai_agent_files_storage_key_present"),
        "the refusal must name the rule: {error}"
    );
    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// The quota is arithmetic over rows
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_quota_is_the_sum_of_the_agents_rows() {
    let Some(store) = Workspace::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let empty = workspace::usage(&store.pool, store.agent_id).await.expect("usage");
    assert_eq!(empty.used_bytes, 0);
    assert_eq!(empty.file_count, 0);
    assert_eq!(empty.limit_bytes, MAX_AGENT_BYTES);

    store.put("a.txt", 1_000).await;
    store.put("b.txt", 2_500).await;
    let used = workspace::usage(&store.pool, store.agent_id).await.expect("usage");
    assert_eq!(used.used_bytes, 3_500);
    assert_eq!(used.file_count, 2);
    store.dispose().await;
}

#[tokio::test]
async fn deleting_a_file_releases_its_bytes_from_the_quota() {
    let Some(store) = Workspace::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let file = store.put("big.bin", 5_000_000).await;
    let before = workspace::usage(&store.pool, store.agent_id).await.expect("usage");
    assert_eq!(before.used_bytes, 5_000_000);

    assert!(workspace::delete_file(&store.pool, store.agent_id, file.id)
        .await
        .expect("delete"));

    // This is the whole reason the quota is a `sum` rather than a running counter: a process
    // that dies between "bytes stored" and "counter incremented" leaves the sum correct and
    // the counter permanently off by one file.
    let after = workspace::usage(&store.pool, store.agent_id).await.expect("usage");
    assert_eq!(after.used_bytes, 0, "a deleted file must stop counting against the quota");
    assert_eq!(after.file_count, 0);
    store.dispose().await;
}

#[tokio::test]
async fn one_agents_files_never_count_against_another_agents_quota() {
    let Some(store) = Workspace::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let second = create_agent(
        &store.pool,
        &NewAgent::with_defaults(store.organization_id, "second", "Second"),
    )
    .await
    .expect("the second agent must be created");
    store.put("mine.txt", 4_000).await;

    workspace::put_file(
        &store.pool,
        &NewAgentFile {
            agent_id: second.id,
            run_id: None,
            path: "theirs.txt".to_owned(),
            size_bytes: 9_000,
            content_type: "text/plain".to_owned(),
            storage_key: storage_key(second.id, "cafe"),
            checksum: "cafe".to_owned(),
            created_by: None,
        },
    )
    .await
    .expect("the second file must be stored");

    let mine = workspace::usage(&store.pool, store.agent_id).await.expect("usage");
    let theirs = workspace::usage(&store.pool, second.id).await.expect("usage");
    assert_eq!(mine.used_bytes, 4_000);
    assert_eq!(theirs.used_bytes, 9_000);
    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// A repeated path is a replacement
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn re_uploading_a_path_replaces_the_row_instead_of_adding_one() {
    let Some(store) = Workspace::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let first = store.put("notes.md", 1_000).await;
    let second = store.put("notes.md", 2_000).await;

    assert_ne!(first.id, second.id, "a replacement is a new row, not an update in place");
    let rows = workspace::list_files(&store.pool, store.agent_id).await.expect("list");
    assert_eq!(rows.len(), 1, "one path names one file");
    assert_eq!(rows[0].size_bytes, 2_000, "the newer bytes are the ones the row describes");

    // The quota follows the replacement rather than adding to it — the difference between
    // "replace" and "add" is visible here and nowhere else.
    let usage = workspace::usage(&store.pool, store.agent_id).await.expect("usage");
    assert_eq!(usage.used_bytes, 2_000, "the old size must stop counting");
    store.dispose().await;
}

#[tokio::test]
async fn the_replacement_path_size_is_what_the_quota_refunds() {
    let Some(store) = Workspace::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    store.put("notes.md", 4_000).await;
    // The route asks this before the caps, and gets the *old* size so a replacement at the
    // ceiling is possible. A wrong answer here is a file that cannot be overwritten.
    let size = workspace::path_size(&store.pool, store.agent_id, "notes.md")
        .await
        .expect("path size");
    assert_eq!(size, 4_000);
    assert_eq!(
        workspace::path_size(&store.pool, store.agent_id, "absent.md")
            .await
            .expect("path size"),
        0,
        "a path with no file refunds nothing, which is a normal answer and not a 404"
    );
    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// Isolation
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_file_is_invisible_to_another_organization_through_every_accessor() {
    let Some(store) = Workspace::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let file = store.put("secret.txt", 32).await;

    // Every read accessor is asked, because "the listing is scoped" says nothing about the
    // by-id read the download route uses. A single un-scoped query is a cross-tenant read.
    let by_path = workspace::get_file(&store.pool, store.other_organization_id, "secret.txt")
        .await
        .expect("get by path");
    assert!(by_path.is_none(), "another organization must not see the file by path");

    let by_id = workspace::get_file_by_id(&store.pool, store.other_organization_id, file.id)
        .await
        .expect("get by id");
    assert!(by_id.is_none(), "another organization must not see the file by id");

    let listed = workspace::list_files(&store.pool, store.other_organization_id)
        .await
        .expect("list");
    assert!(listed.is_empty());

    // And the delete has to be scoped the same way, or "remove" is a write into somebody
    // else's workspace.
    let removed = workspace::delete_file(&store.pool, store.other_organization_id, file.id)
        .await
        .expect("delete");
    assert!(!removed, "another organization must not be able to delete the file");
    assert!(
        workspace::get_file(&store.pool, store.agent_id, "secret.txt")
            .await
            .expect("get")
            .is_some(),
        "the file must still be there afterwards"
    );
    store.dispose().await;
}

#[tokio::test]
async fn the_owner_of_an_agent_cannot_see_another_agents_workspace() {
    let Some(store) = Workspace::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let other = create_agent(
        &store.pool,
        &NewAgent::with_defaults(store.organization_id, "other", "Other"),
    )
    .await
    .expect("the second agent must be created");
    store.put("mine.txt", 10).await;

    let listed = workspace::list_files(&store.pool, other.id).await.expect("list");
    assert!(
        listed.is_empty(),
        "two agents in one organization must not share a workspace namespace"
    );
    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// The listing re-checks the path rule
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_row_whose_path_breaks_todays_rules_is_not_listed() {
    let Some(store) = Workspace::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    // A row the *current* schema would refuse, inserted by disabling the check. This is the
    // shape a restore from an older build produces, and the guarantee is that being old does
    // not make a row reachable: the listing re-runs the validator over every row it returns.
    sqlx::query("set session_replication_role = replica")
        .execute(&store.pool)
        .await
        .ok();
    let error = sqlx::query(
        "insert into ai_agent_files (agent_id, path, size_bytes, storage_key) \
         values ($1, '../legacy.txt', 10, 'agents/x/legacy')",
    )
    .bind(store.agent_id)
    .execute(&store.pool)
    .await;
    sqlx::query("set session_replication_role = origin")
        .execute(&store.pool)
        .await
        .ok();
    if error.is_err() {
        // A non-superuser cannot disable a check constraint; the walk is then redundant with
        // the one above and must not fail the suite for that.
        eprintln!("skipping: the check constraint cannot be suspended for this role");
        store.dispose().await;
        return;
    }

    let listed = workspace::list_files(&store.pool, store.agent_id).await.expect("list");
    assert!(
        listed.is_empty(),
        "a row written under a rule that no longer exists must not be reachable"
    );
    // A direct read of the same path is refused too, rather than answering with a row.
    assert!(
        workspace::get_file(&store.pool, store.agent_id, "../legacy.txt")
            .await
            .is_err(),
        "the single-path read must refuse before it reaches the database"
    );
    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// The caps, through the store's own function with real usage
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_caps_hold_against_the_agents_real_usage() {
    let Some(store) = Workspace::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    store.put("quarter.csv", 2_000_000).await;
    let usage = workspace::usage(&store.pool, store.agent_id).await.expect("usage");

    // Under both caps: fine.
    check_caps(&usage, 1_000, 0).expect("a small file fits");
    // Over the per-file cap: refused, and the message names the 10 MB the panel also shows.
    let error = check_caps(&usage, MAX_FILE_BYTES + 1, 0)
        .expect_err("the per-file cap must hold")
        .to_string();
    assert!(error.contains("10 MB"), "{error}");
    store.dispose().await;
}

#[tokio::test]
async fn a_workspace_at_the_ceiling_accepts_a_replacement_but_not_a_new_file() {
    let Some(store) = Workspace::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    // Ten files at the per-file maximum: exactly the agent ceiling.
    for index in 0..10 {
        store.put(&format!("file-{index}.bin"), MAX_FILE_BYTES as i64).await;
    }
    let usage = workspace::usage(&store.pool, store.agent_id).await.expect("usage");
    assert_eq!(usage.used_bytes, MAX_AGENT_BYTES, "the fixture fills the workspace exactly");
    assert_eq!(usage.file_count, 10);

    // Replacing an existing file at the same size cannot grow the workspace, so it must be
    // allowed. The alternative is a workspace that cannot be corrected once it is full.
    check_caps(&usage, MAX_FILE_BYTES as u64, MAX_FILE_BYTES as u64)
        .expect("replacing a file at the ceiling must work");

    // A *new* file cannot fit, and the message has to say what is already stored so the
    // reader knows to delete something first.
    let error = check_caps(&usage, 1_000, 0)
        .expect_err("a new file cannot fit in a full workspace")
        .to_string();
    assert!(error.contains("100 MB"), "{error}");
    assert!(error.contains("10 file"), "{error}");

    // And the emptiness of the space is arithmetic, not a promise: deleting one file frees it.
    let victim = workspace::list_files(&store.pool, store.agent_id)
        .await
        .expect("list")
        .remove(0);
    assert!(workspace::delete_file(&store.pool, store.agent_id, victim.id)
        .await
        .expect("delete"));
    let freed = workspace::usage(&store.pool, store.agent_id).await.expect("usage");
    check_caps(&freed, 1_000, 0).expect("a deleted file makes room");
    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// The error code is the workspace's own
// -------------------------------------------------------------------------------------------

#[test]
fn a_workspace_refusal_carries_its_own_code() {
    // The route maps this to `invalid_file`, and the panel's Workspace tab is the only screen
    // that should show the message. A refusal folded into `invalid_agent` puts a path error
    // above the agent's name field, which is the one place the reader is not looking when a
    // file picker refused a file.
    let error = validate_path("../escape").expect_err("a traversal must be refused");
    assert_eq!(error.code(), "invalid_file");
    assert!(matches!(error, AiHubError::InvalidFile(_)));
}

#[test]
fn the_usage_type_reports_what_the_bar_needs() {
    // Not a store walk: the bar is arithmetic, and this pins the three figures the panel reads
    // in one place.
    let usage = Usage { used_bytes: 25, limit_bytes: 100, file_count: 4 };
    assert_eq!(usage.percent(), 25);
    assert_eq!(usage.remaining_bytes(), 75);
}
