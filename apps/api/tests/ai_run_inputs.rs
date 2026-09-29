//! Integration tests for what a run was told to read (REQ-099, slice 2, second half).
//!
//! The Run sheet lets a person hand a run a handful of workspace files, and that choice used to
//! live in an audit row as `"files": 3`. Three is not a record. These walks are about the four
//! things a reference has to survive:
//!
//! 1. **It outlives the file.** A run that named `q3.csv`, and a person who then deleted it, is a
//!    run whose input list now reads "q3.csv — missing". That is the sentence that explains the
//!    failure, so the reference survives the delete (`file_id` is `set null`, asserted here by
//!    re-reading after the delete rather than by reading the constraint's text).
//! 2. **It may name a file that does not exist yet.** "Write the summary to `summary.md`" is an
//!    instruction, not a malformed request, so an unresolvable path is stored with a `NULL`
//!    file id instead of refused.
//! 3. **A re-submission replaces, it does not append.** The sheet can be pressed twice (a 409, a
//!    retry, a double-click), and two lists on one run is a trace that changes depending on which
//!    attempt the reader happened to see.
//! 4. **The database refuses a traversal on this table too.** The route validates, but a rule
//!    that exists only in a handler is a rule a migration, a script or a future writer sails
//!    past — and this is a path that ends up in a prompt.
//!
//! The harness is the workspace suite's, reused rather than copied: one throwaway database per
//! walk, dropped explicitly and awaited.

use omnion_ai_hub::run_store::{NewAgent, NewRun, create_agent, create_run};
use omnion_ai_hub::workspace::{self, NewAgentFile, MAX_RUN_INPUTS};
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::Db;
use sqlx::PgPool;
use uuid::Uuid;

// -------------------------------------------------------------------------------------------
// The harness
// -------------------------------------------------------------------------------------------

struct Inputs {
    pool: PgPool,
    organization_id: Uuid,
    agent_id: Uuid,
    database: String,
    maintenance: Option<Db>,
}

impl Inputs {
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

        let database = format!("omnion_ainp_{}", Uuid::new_v4().simple());
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
        let agent = create_agent(
            db.pool(),
            &NewAgent::with_defaults(organization_id, "summarizer", "Summarizer"),
        )
        .await
        .expect("the fixture agent must be created");

        Some(Self {
            pool: db.pool().clone(),
            organization_id,
            agent_id: agent.id,
            database,
            maintenance: Some(maintenance),
        })
    }

    /// A run row, which a reference cannot exist without.
    async fn run(&self, goal: &str) -> Uuid {
        let mut new = NewRun::default_for(self.organization_id, self.agent_id);
        new.goal = goal.to_owned();
        create_run(&self.pool, &new)
            .await
            .expect("the fixture run must be created")
            .id
    }

    /// A workspace file, so a reference has something to resolve against.
    async fn file(&self, path: &str) -> Uuid {
        let checksum = workspace::checksum_of(path.as_bytes());
        workspace::put_file(
            &self.pool,
            &NewAgentFile {
                agent_id: self.agent_id,
                run_id: None,
                path: path.to_owned(),
                size_bytes: 128,
                content_type: "text/csv".to_owned(),
                storage_key: workspace::storage_key(self.agent_id, &checksum),
                checksum,
                created_by: None,
            },
        )
        .await
        .expect("the fixture file must be stored")
        .id
    }

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
// A reference records a path and, when there is one, the file behind it
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_named_input_resolves_to_the_file_it_points_at() {
    let Some(store) = Inputs::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let run_id = store.run("Summarise the quarter.").await;
    let file_id = store.file("q3.csv").await;
    let inputs = workspace::set_run_inputs(
        &store.pool,
        run_id,
        store.agent_id,
        None,
        &["q3.csv".to_owned()],
    )
    .await
    .expect("the references must be written");

    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].path, "q3.csv");
    assert_eq!(inputs[0].file_id, Some(file_id), "the path must resolve to its file");
    store.dispose().await;
}

#[tokio::test]
async fn a_path_with_no_file_is_stored_and_reports_itself_unresolved() {
    // "Write the summary to summary.md" is an instruction. Refusing it would make an agent that
    // keeps its output somewhere unusable for the only kind of output it has.
    let Some(store) = Inputs::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let run_id = store.run("Write the summary out.").await;
    let inputs = workspace::set_run_inputs(
        &store.pool,
        run_id,
        store.agent_id,
        None,
        &["summary.md".to_owned()],
    )
    .await
    .expect("an output path must be accepted");

    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].file_id, None);

    let listed = workspace::list_run_inputs(&store.pool, run_id)
        .await
        .expect("the list must answer");
    let resolution = workspace::resolve(&listed, &[]);
    assert!(!resolution.is_complete());
    let message = resolution.message().expect("a missing input must say which one");
    assert!(message.contains("summary.md"), "{message}");
    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// The reference outlives the file — this is the whole point of the table
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn deleting_the_file_leaves_the_reference_behind_named_as_missing() {
    let Some(store) = Inputs::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let run_id = store.run("Summarise the quarter.").await;
    let file_id = store.file("q3.csv").await;
    workspace::set_run_inputs(
        &store.pool,
        run_id,
        store.agent_id,
        None,
        &["q3.csv".to_owned()],
    )
    .await
    .expect("the reference must be written");

    workspace::delete_file(&store.pool, store.agent_id, file_id)
        .await
        .expect("the delete must answer");
    assert!(
        workspace::get_file_by_id(&store.pool, store.agent_id, file_id)
            .await
            .expect("the read must answer")
            .is_none(),
        "the file is gone, or the delete was a no-op"
    );

    let listed = workspace::list_run_inputs(&store.pool, run_id)
        .await
        .expect("the list must answer");
    assert_eq!(listed.len(), 1, "the reference must outlive the file");
    assert_eq!(listed[0].path, "q3.csv", "and it must still name the path");
    assert_eq!(
        listed[0].file_id, None,
        "the dangling link is cleared rather than left to fail a foreign key"
    );
    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// A re-submission replaces; a duplicate collapses
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn writing_twice_replaces_the_list_and_collapses_a_duplicate() {
    let Some(store) = Inputs::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let run_id = store.run("First attempt.").await;
    store.file("a.csv").await;
    store.file("b.csv").await;
    workspace::set_run_inputs(
        &store.pool,
        run_id,
        store.agent_id,
        None,
        &["a.csv".to_owned()],
    )
    .await
    .expect("the first list must be written");

    // The same path twice is a stale picker, not a request for two copies of the file.
    let second = workspace::set_run_inputs(
        &store.pool,
        run_id,
        store.agent_id,
        None,
        &["b.csv".to_owned(), "b.csv".to_owned()],
    )
    .await
    .expect("the second list must be written");
    assert_eq!(second.len(), 1, "a duplicate path is one reference");
    assert_eq!(second[0].path, "b.csv");
    assert!(
        !second.iter().any(|input| input.path == "a.csv"),
        "the previous list must be gone: two lists on one run is a trace that changes depending \
         on which attempt the reader happened to see"
    );
    store.dispose().await;
}

#[tokio::test]
async fn more_than_the_cap_is_refused_and_writes_nothing() {
    let Some(store) = Inputs::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let run_id = store.run("Too many.").await;
    let too_many: Vec<String> = (0..=MAX_RUN_INPUTS)
        .map(|index| format!("file-{index}.csv"))
        .collect();
    let error = workspace::set_run_inputs(
        &store.pool,
        run_id,
        store.agent_id,
        None,
        &too_many,
    )
    .await
    .expect_err("eleven inputs is a refusal, not a truncation");
    assert!(
        error.to_string().contains(&MAX_RUN_INPUTS.to_string()),
        "the message must name the ceiling: {error}"
    );

    // The refusal happens before the delete, so the run keeps whatever it had.
    workspace::set_run_inputs(
        &store.pool,
        run_id,
        store.agent_id,
        None,
        &["kept.csv".to_owned()],
    )
    .await
    .expect("the legal list must be written");
    let refused = workspace::set_run_inputs(&store.pool, run_id, store.agent_id, None, &too_many)
        .await
        .expect_err("still a refusal");
    assert!(refused.to_string().contains("at most"));

    let listed = workspace::list_run_inputs(&store.pool, run_id)
        .await
        .expect("the list must answer");
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].path, "kept.csv",
        "a refused write must not clear the list that was already there"
    );
    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// The database refuses a traversal here too
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_database_also_refuses_a_traversal_in_a_reference() {
    let Some(store) = Inputs::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let run_id = store.run("Traversal attempt.").await;
    for (label, path) in [
        ("parent", "../escape.csv"),
        ("absolute", "/etc/passwd"),
        ("windows drive", "C:\\notes.md"),
    ] {
        let written = sqlx::query(
            "insert into ai_run_inputs (run_id, agent_id, path) values ($1, $2, $3)",
        )
        .bind(run_id)
        .bind(store.agent_id)
        .bind(path)
        .execute(&store.pool)
        .await;
        assert!(
            written.is_err(),
            "the database must refuse a {label} path in a reference; it accepted {path:?}"
        );
    }
    assert!(
        workspace::list_run_inputs(&store.pool, run_id)
            .await
            .expect("the list must answer")
            .is_empty(),
        "a refused insert writes nothing"
    );
    store.dispose().await;
}

#[tokio::test]
async fn a_reference_is_scoped_to_its_own_run() {
    // `list_run_inputs` reads by run id alone, because a run id is already tenant-bound. This is
    // the walk that would notice if that ever stopped being true: a second run's inputs must not
    // leak into the first, and the only way to get a second run is a **second agent** — the
    // partial unique index `ai_runs_one_active_per_agent_uidx` refuses two queued runs for one
    // agent, which is the guarantee the Run sheet's 409 is built on.
    let Some(store) = Inputs::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let mine = store.run("Mine.").await;
    store.file("shared.csv").await;
    workspace::set_run_inputs(
        &store.pool,
        mine,
        store.agent_id,
        None,
        &["shared.csv".to_owned()],
    )
    .await
    .expect("the reference must be written");

    let other_agent = create_agent(
        &store.pool,
        &NewAgent::with_defaults(store.organization_id, "other", "Other"),
    )
    .await
    .expect("the second agent must be created");
    let mut other_run = NewRun::default_for(store.organization_id, other_agent.id);
    other_run.goal = "Somebody else's.".to_owned();
    let other = create_run(&store.pool, &other_run)
        .await
        .expect("the second run must be created")
        .id;
    workspace::set_run_inputs(
        &store.pool,
        other,
        other_agent.id,
        None,
        &["theirs.csv".to_owned()],
    )
    .await
    .expect("the second reference must be written");

    let mine_read = workspace::list_run_inputs(&store.pool, mine)
        .await
        .expect("the list must answer");
    assert_eq!(mine_read.len(), 1);
    assert_eq!(
        mine_read[0].path, "shared.csv",
        "another run's reference must not appear in this one"
    );
    let other_read = workspace::list_run_inputs(&store.pool, other)
        .await
        .expect("the list must answer");
    assert_eq!(other_read.len(), 1);
    assert_eq!(other_read[0].path, "theirs.csv");
    store.dispose().await;
}
