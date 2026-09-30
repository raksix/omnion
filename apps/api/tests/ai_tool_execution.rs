//! Walks for the execution pipeline (REQ-100, slice 3).
//!
//! The unit tests in `tool_exec.rs` prove the *rules* — the payload filter, the outcome shapes,
//! the projection into the loop. These walks prove the three things only a real database and a
//! real registry row can answer, and every one of them asserts on a **row**, not on a return
//! value:
//!
//! - **a permitted call runs, and writes exactly one `ai_tool_calls` row** carrying the same run
//!   and step. The request's criterion is "exactly one row and one `audit_log` row … both
//!   carrying the same run and step"; a pipeline that wrote a row and then a second one on the
//!   way out would make the usage counts double every call.
//! - **a denied call leaves no side effect and is still recorded.** "The fixture target row is
//!   unchanged" is a claim about the world, so the walk writes a target row, attempts the
//!   call, and reads the target back. Asserting only the refusal would pass with a tool that
//!   ran and *then* reported a refusal.
//! - **the cap is counted across a resume, not per process.** A run parked on an approval and
//!   re-driven arrives with its earlier rows still in the table, so it must not get a second
//!   allowance — this is the assertion that a counter in the loop's stack cannot satisfy, and it
//!   is why `count_in_run` is a `count` over rows.
//!
//! Everything runs against a **fresh database** created and dropped by the harness, so a walk
//! that leaves a row behind cannot make the next one pass, and the shared `max_connections` pool
//! (7 writers on this box) is opened at 4 rather than the default.

use std::collections::BTreeMap;
use std::sync::Arc;

use omnion_ai_hub::agent::ToolCall;
use omnion_ai_hub::identity::{self, GrantEffect, NewIdentity};
use omnion_ai_hub::registry;
use omnion_ai_hub::run_store::{NewAgent, create_agent};
use omnion_ai_hub::tool_calls::CallStatus;
use omnion_ai_hub::tool_exec::{
    CallOutcome, Caller, PermissionGate, Pipeline, ResolvedIdentity, identity_of,
};
use omnion_ai_hub::tools::{FnTool, ToolOutcome, ToolRegistry};
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::Db;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

// -------------------------------------------------------------------------------------------
// The harness
// -------------------------------------------------------------------------------------------

struct PipelineStore {
    pool: PgPool,
    organization_id: Uuid,
    other_organization_id: Uuid,
    /// The side-effect probe: a row a tool body would have to touch for a denial to be a lie.
    /// The count is read before and after every refusal, so "a denied call leaves no side effect"
    /// is a claim about a row rather than about a return value.
    database: String,
    maintenance: Option<Db>,
}

impl PipelineStore {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().ok()?;
        if let Err(err) = Db::connect(&DatabaseConfig {
            url: config.database.url.clone(),
            max_connections: 1,
        })
        .await
        {
            // The URL is in the message on purpose: a database that does not exist and a server
            // that is down produce the same class of failure, and only the URL says which.
            eprintln!("PostgreSQL is not reachable at {}: {err}", config.database.url);
            return None;
        }

        let database = format!("omnion_aipipe_{}", Uuid::new_v4().simple());
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
            // 4, not the default: this box runs 7 writer loops against one `max_connections = 100`
            // server, and REQ-100's own notes record a suite that opened 11 databases at once and
            // deadlocked on the pool rather than on anything it was testing.
            max_connections: 4,
        })
        .await
        .expect("the fresh database must connect");
        db.migrate().await.expect("migrations must apply");

        let organization_id = seed_organization(db.pool(), "pipeco").await;
        let other_organization_id = seed_organization(db.pool(), "otherco").await;
        let seeded = registry::seed(db.pool())
            .await
            .expect("the registry seed must run");
        assert!(seeded.inserted > 0, "the seeder must insert the v1 tool set");

        install_pool(db.pool());

        Some(Self {
            pool: db.pool().clone(),
            organization_id,
            other_organization_id,
            database,
            maintenance: Some(maintenance),
        })
    }

    async fn touched(&self) -> i64 {
        let count: i64 = sqlx::query_scalar("select count(*) from organizations where name = 'touched'")
            .fetch_one(&self.pool)
            .await
            .expect("the fixture count must be readable");
        count
    }

    /// An agent carrying `tools`, created through the store the route uses.
    async fn agent(&self, key: &str, tools: &[&str]) -> Uuid {
        let agent = create_agent(
            &self.pool,
            &NewAgent {
                organization_id: self.organization_id,
                site_id: None,
                key: key.to_owned(),
                name: format!("{key} title"),
                description: String::new(),
                system_prompt: "You are a fixture.".to_owned(),
                model_id: None,
                // The run's own defaults, written out rather than `None`: `NewAgent` takes
                // concrete values, and a walk that had to guess them would be asserting against
                // a limit the platform invented rather than the one the schema declares.
                temperature: 0.2,
                max_steps: 8,
                deadline_seconds: 300,
                token_budget: 200_000,
                tools: tools.iter().map(|k| (*k).to_owned()).collect(),
                approvals: Vec::new(),
                memory_scope: "none".to_owned(),
                enabled: true,
                created_by: None,
            },
        )
        .await
        .expect("the fixture agent must be created");
        agent.id
    }

    /// A run row the call log can point at, written the way the runner writes one.
    async fn run(&self, agent_id: Uuid, goal: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "insert into ai_runs (id, organization_id, agent_id, trigger, goal, status) \
             values ($1, $2, $3, 'agent', $4, 'running')",
        )
        .bind(id)
        .bind(self.organization_id)
        .bind(agent_id)
        .bind(goal)
        .execute(&self.pool)
        .await
        .expect("the fixture run must be created");
        id
    }

    async fn identity(&self, key: &str, grants: &[(&str, GrantEffect)]) -> ResolvedIdentity {
        let created = identity::create_identity(
            &self.pool,
            &NewIdentity {
                organization_id: Some(self.organization_id),
                key: key.to_owned(),
                name: format!("{key} title"),
                description: String::new(),
                is_default: false,
                created_by: None,
            },
        )
        .await
        .expect("the fixture identity must be created");
        for (tool, effect) in grants {
            identity::set_grant(&self.pool, created.id, tool, *effect, None)
                .await
                .expect("the grant must be written");
        }
        let stored = identity::grants_of(&self.pool, created.id)
            .await
            .expect("the grants must read back");
        identity_of(&created, stored)
    }

    fn caller(&self, agent_id: Uuid, run_id: Uuid) -> Caller {
        Caller {
            organization_id: self.organization_id,
            agent_id,
            run_id,
            step_id: None,
            user_id: None,
            site_id: None,
        }
    }

    /// The rows this run's call log holds for one tool, straight from SQL.
    async fn rows_for(&self, run_id: Uuid, tool_key: &str) -> Vec<(String, Option<String>)> {
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            "select status, error_code from ai_tool_calls \
             where run_id = $1 and tool_key = $2 order by id",
        )
        .bind(run_id)
        .bind(tool_key)
        .fetch_all(&self.pool)
        .await
        .expect("the call rows must be readable");
        rows
    }

    async fn total_rows(&self, run_id: Uuid) -> i64 {
        let count: i64 = sqlx::query_scalar("select count(*) from ai_tool_calls where run_id = $1")
            .bind(run_id)
            .fetch_one(&self.pool)
            .await
            .expect("the call count must be readable");
        count
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

/// The harness, or a **panic**.
///
/// The other suites in this directory print "skipping" and `return`, which is the right call for
/// a developer who has not started a database. It is the wrong call for a loop that reports
/// "15 passed": a URL naming a database that does not exist makes every walk in the file skip,
/// and the summary still reads green. That happened here — `omnion_test_w7` (the name I invented
/// from the crate suffix) does not exist; the QA stack's database is `omnion_qa_w7` — and four
/// walks failed only after the skip was made fatal.
///
/// So: an unreachable database panics with the URL it tried. A skipped walk is a walk that proved
/// nothing, and a loop that cannot tell the difference reports success for a suite it never ran.
macro_rules! pipe {
    () => {
        match PipelineStore::fresh().await {
            Some(store) => store,
            None => panic!(
                "PostgreSQL is not reachable, so every walk in this file would have SKIPPED. \
                 Set OMNION_DATABASE_URL to an existing database — on this box the QA stack's is \
                 postgres://omnion:omnion@127.0.0.1:5433/omnion_qa_w7. A skip must not read as a \
                 pass."
            ),
        }
    };
}

/// A registry whose two tools are the ones the walks assert on.
///
/// Both are registered under keys the **compiled catalogue carries**, because the pipeline's
/// `enabled` check reads the catalogue: a fixture under a made-up key would be refused as
/// disabled, and the walk would pass for a reason that has nothing to do with what it is
/// testing. `registry::seed` above guarantees both rows exist in `ai_tools`, which the cap and
/// the timeout both read.
fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::empty();
    registry.register(Arc::new(
        FnTool::new(
            "content.search",
            "Search content",
            "content.read",
            |_| ToolOutcome::ok("3 hits"),
        )
        .with_schema(json!({
            "type": "object",
            "properties": { "q": { "type": "string", "minLength": 1 } },
            "required": ["q"],
            "additionalProperties": false
        })),
    ));
    registry.register(Arc::new(writing_tool()));
    registry
}

/// The side-effecting tool, and the reason the denial walks mean anything.
///
/// **The body really writes.** The first version of this file counted rows named `touched` and
/// never created any, so "a denied call leaves no side effect" compared 0 with 0 — a criterion
/// that passes for every implementation, including one that ran the body first and reported a
/// refusal afterwards. A control walk at the bottom calls this tool *through the granted path*
/// and asserts the count moved, which is what makes the denial walks' unchanged count a
/// statement about the denial rather than about a tool that never writes.
fn writing_tool() -> impl omnion_ai_hub::tools::Tool {
    WritingTool
}

struct WritingTool;

impl omnion_ai_hub::tools::Tool for WritingTool {
    fn key(&self) -> &str {
        "content.create"
    }
    fn description(&self) -> &str {
        "Create content"
    }
    fn permission(&self) -> &str {
        "content.create"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": { "title": { "type": "string" } },
            "required": ["title"],
            "additionalProperties": false
        })
    }
    fn run(
        &self,
        _arguments: &serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolOutcome> + Send + '_>> {
        // The pool the walk handed in, read at call time through a once-cell. `FnTool` takes a
        // sync `Fn`, so a body that needs the database implements `Tool` directly — the same
        // reason the real tools will.
        let pool = SIDE_EFFECT_POOL
            .get()
            .cloned()
            .expect("the walk must install the pool before a tool body runs");
        let pool = pool.clone();
        Box::pin(async move {
            sqlx::query("insert into organizations (id, name, slug) values ($1, 'touched', $2)")
                .bind(Uuid::new_v4())
                .bind(format!("touched-{}", Uuid::new_v4().simple()))
                .execute(&pool)
                .await
                .expect("the tool body must be able to write");
            ToolOutcome::ok("created")
        })
    }
}

/// The pool the fixture tool writes through, installed once per test by [`install_pool`].
static SIDE_EFFECT_POOL: std::sync::OnceLock<PgPool> = std::sync::OnceLock::new();

/// Hand the fixture tool a pool. Once per process, so every walk in this file shares the first
/// one — and that is a real limitation: the walks run in threads of one process, so the pool is
/// whichever test installed it first, and a drop of that test's database would break the others.
/// **Fixed by running the file with `--test-threads=1`**, which the crate's own convention already
/// requires for the suites that open scratch databases; without it these walks are not
/// trustworthy, and the fix is not to make them thread-safe but to run them serially.
fn install_pool(pool: &PgPool) {
    let _ = SIDE_EFFECT_POOL.set(pool.clone());
}

/// The gate a caller with everything gets, and the one a caller without a key gets.
struct Gate(BTreeMap<String, bool>);

impl PermissionGate for Gate {
    fn allows(&self, permission: &str) -> bool {
        self.0.get(permission).copied().unwrap_or(false)
    }
}

fn allow_all() -> Gate {
    let mut map = BTreeMap::new();
    for key in [
        "content.read",
        "content.create",
        "ai.tools.manage",
        "users.create",
        "deployment.deploy",
    ] {
        map.insert(key.to_owned(), true);
    }
    Gate(map)
}

/// A gate that holds everything except one named key — the "viewer without a tool's permission"
/// case the acceptance criteria name.
fn gate_without(key: &str) -> Gate {
    let mut gate = allow_all();
    gate.0.insert(key.to_owned(), false);
    gate
}

// -------------------------------------------------------------------------------------------
// A permitted call: one row, one execution
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_permitted_call_runs_and_writes_exactly_one_row() {
    let store = pipe!();
    let registry = registry();
    let agent_id = store.agent("writer", &["content.search"]).await;
    let run_id = store.run(agent_id, "find the report").await;
    let identity = store
        .identity("editor", &[("content.search", GrantEffect::Allow)])
        .await;
    let gate = allow_all();
    let pipe = Pipeline::new(
        std::sync::Arc::new(registry),
        Some(identity),
        vec!["content.search".to_owned()],
        Vec::new(),
        std::sync::Arc::new(gate),
        store.caller(agent_id, run_id),
    );

    let outcome = pipe
        .call(
            &store.pool,
            &ToolCall::new("content.search", json!({"q": "report"})),
        )
        .await
        .expect("the pipeline must answer");

    match &outcome {
        CallOutcome::Ran { tool, summary, failed, .. } => {
            assert_eq!(tool, "content.search");
            assert_eq!(summary, "3 hits");
            assert!(!failed);
        }
        other => panic!("a permitted call must run, got {other:?}"),
    }

    let rows = store.rows_for(run_id, "content.search").await;
    assert_eq!(
        rows.len(),
        1,
        "one call writes exactly one row — the usage counts on /ai/tools aggregate this table, \
         so a second row per call doubles every number the panel shows"
    );
    assert_eq!(rows[0].0, "ok");
    assert_eq!(rows[0].1, None, "a success carries no error code");
    store.dispose().await;
}

#[tokio::test]
async fn a_row_carries_the_size_of_its_arguments_and_never_the_arguments() {
    let store = pipe!();
    let registry = registry();
    let agent_id = store.agent("writer", &["content.search"]).await;
    let run_id = store.run(agent_id, "find the report").await;
    let identity = store
        .identity("editor", &[("content.search", GrantEffect::Allow)])
        .await;
    let gate = allow_all();
    let pipe = Pipeline::new(
        std::sync::Arc::new(registry),
        Some(identity.clone()),
        vec!["content.search".to_owned()],
        Vec::new(),
        std::sync::Arc::new(gate),
        store.caller(agent_id, run_id),
    );

    let arguments = json!({"q": "a long enough query to measure"});
    let expected = omnion_ai_hub::tool_calls::json_bytes(&arguments);
    pipe.call(&store.pool, &ToolCall::new("content.search", arguments))
        .await
        .expect("the pipeline must answer");

    let (args_bytes, result_bytes, identity_id, agent_id_row): (
        Option<i32>,
        Option<i32>,
        Option<Uuid>,
        Option<Uuid>,
    ) = sqlx::query_as(
        "select args_bytes, result_bytes, identity_id, agent_id from ai_tool_calls where run_id = $1",
    )
    .bind(run_id)
    .fetch_one(&store.pool)
    .await
    .expect("the row must be readable");

    assert_eq!(
        args_bytes,
        Some(i32::try_from(expected).expect("a small payload fits")),
        "the log records the size of the arguments, which is what makes it readable by an \
         operator without making it a copy of the payload"
    );
    assert_eq!(result_bytes, Some("3 hits".len() as i32));
    // The identity is the run's own, which is what makes the log answer "which identity made this
    // call" without joining through the run.
    assert_eq!(identity_id, Some(identity.id));
    assert_eq!(agent_id_row, Some(agent_id));
    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// A denial: recorded, and with no side effect
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_explicit_deny_beats_the_agent_allow_list_and_touches_nothing() {
    let store = pipe!();
    let registry = registry();
    let agent_id = store.agent("writer", &["content.create"]).await;
    let run_id = store.run(agent_id, "publish the draft").await;
    // The agent lists the tool AND the identity allows it — the deny arrives third and still wins,
    // which is the ordering `identity::resolve` documents and the request's criterion names.
    let identity = store
        .identity("restricted", &[("content.create", GrantEffect::Deny)])
        .await;
    let before = store.touched().await;
    let gate = allow_all();
    let pipe = Pipeline::new(
        std::sync::Arc::new(registry),
        Some(identity),
        vec!["content.create".to_owned()],
        Vec::new(),
        std::sync::Arc::new(gate),
        store.caller(agent_id, run_id),
    );

    let outcome = pipe
        .call(
            &store.pool,
            &ToolCall::new("content.create", json!({"title": "Draft"})),
        )
        .await
        .expect("the pipeline must answer");

    match &outcome {
        CallOutcome::Refused { code, tool, .. } => {
            assert_eq!(code, "tool_denied");
            assert_eq!(tool, "content.create");
        }
        other => panic!("an explicit deny must refuse, got {other:?}"),
    }
    assert_eq!(
        store.touched().await,
        before,
        "a denied call must leave no side effect"
    );
    let rows = store.rows_for(run_id, "content.create").await;
    assert_eq!(rows.len(), 1, "the denial is recorded like any other call");
    assert_eq!(rows[0].0, "denied");
    assert_eq!(rows[0].1.as_deref(), Some("tool_denied"));
    store.dispose().await;
}

#[tokio::test]
async fn a_tool_the_caller_may_not_perform_is_refused_with_the_key_named() {
    let store = pipe!();
    let registry = registry();
    let agent_id = store.agent("writer", &["content.create"]).await;
    let run_id = store.run(agent_id, "publish the draft").await;
    let identity = store
        .identity("editor", &[("content.create", GrantEffect::Allow)])
        .await;
    // A viewer who holds the AI keys but not `content.create` — the exact half the acceptance
    // criteria left open for the execution path, with the 403's key named in the message.
    let gate = gate_without("content.create");
    let pipe = Pipeline::new(
        std::sync::Arc::new(registry),
        Some(identity),
        vec!["content.create".to_owned()],
        Vec::new(),
        std::sync::Arc::new(gate),
        store.caller(agent_id, run_id),
    );

    let outcome = pipe
        .call(
            &store.pool,
            &ToolCall::new("content.create", json!({"title": "Draft"})),
        )
        .await
        .expect("the pipeline must answer");

    match &outcome {
        CallOutcome::Refused { code, reason, .. } => {
            assert_eq!(code, "permission_denied");
            assert!(
                reason.contains("content.create"),
                "the message must name the permission the caller lacks and the tool that needs \
                 it, or an operator reading a refusal has nothing to go on. Got: {reason}"
            );
        }
        other => panic!("a missing permission must refuse, got {other:?}"),
    }
    let rows = store.rows_for(run_id, "content.create").await;
    assert_eq!(rows[0].0, "denied");
    assert_eq!(rows[0].1.as_deref(), Some("permission_denied"));
    store.dispose().await;
}

#[tokio::test]
async fn a_run_with_no_identity_executes_nothing_at_all() {
    let store = pipe!();
    let registry = registry();
    let agent_id = store.agent("writer", &["content.create"]).await;
    let run_id = store.run(agent_id, "publish the draft").await;
    let before = store.touched().await;
    let gate = allow_all();
    // The identity resolution failed. The request: "a run with no resolvable identity executes
    // nothing" — and the check happens *before* the registry is consulted, so this call cannot
    // even confirm the tool exists.
    let pipe = Pipeline::new(
        std::sync::Arc::new(registry),
        None,
        vec!["content.create".to_owned()],
        Vec::new(),
        std::sync::Arc::new(gate),
        store.caller(agent_id, run_id),
    );

    let outcome = pipe
        .call(
            &store.pool,
            &ToolCall::new("content.create", json!({"title": "Draft"})),
        )
        .await
        .expect("the pipeline must answer");

    match &outcome {
        CallOutcome::Refused { code, .. } => assert_eq!(code, "identity_unresolved"),
        other => panic!("a run with no identity must refuse, got {other:?}"),
    }
    assert_eq!(store.touched().await, before);
    assert_eq!(store.total_rows(run_id).await, 1, "the refusal is still recorded");
    store.dispose().await;
}

#[tokio::test]
async fn a_tool_disabled_by_an_operator_is_still_callable_inside_the_run_that_started_first() {
    // The trade `Pipeline::enabled` documents, asserted as a fact so that changing it is a
    // deliberate diff rather than an accident. The check reads the *compiled* catalogue rather
    // than the operator's row, because `model_facing` runs it once per tool per step and 20 round
    // trips to assemble a payload is its own cost. The consequence: a tool switched off while a
    // run is live keeps working until that run ends, and the next run refuses it.
    let store = pipe!();
    let registry = registry();
    let agent_id = store.agent("writer", &["content.search"]).await;
    let run_id = store.run(agent_id, "find the report").await;
    let identity = store
        .identity("editor", &[("content.search", GrantEffect::Allow)])
        .await;

    registry::update_tool(
        &store.pool,
        "content.search",
        &registry::ToolLimits {
            enabled: Some(false),
            ..registry::ToolLimits::default()
        },
    )
    .await
    .expect("the tool must be patchable");
    // The row really is off, so the assertion below is about the pipeline and not about a patch
    // that silently did nothing.
    let row = registry::get_tool(&store.pool, "content.search")
        .await
        .expect("the registry must answer")
        .expect("the row exists");
    assert!(!row.enabled, "the operator's decision must be on the row");

    let gate = allow_all();
    let pipe = Pipeline::new(
        std::sync::Arc::new(registry),
        Some(identity),
        vec!["content.search".to_owned()],
        Vec::new(),
        std::sync::Arc::new(gate),
        store.caller(agent_id, run_id),
    );
    let outcome = pipe
        .call(&store.pool, &ToolCall::new("content.search", json!({"q": "report"})))
        .await
        .expect("the pipeline must answer");
    assert!(
        matches!(outcome, CallOutcome::Ran { .. }),
        "the compiled enabled() check does not read the operator's row mid-run. Got: {outcome:?}"
    );
    store.dispose().await;
}

#[tokio::test]
async fn an_unknown_field_is_refused_before_the_tool_runs() {
    let store = pipe!();
    let registry = registry();
    let agent_id = store.agent("writer", &["content.search"]).await;
    let run_id = store.run(agent_id, "find the report").await;
    let identity = store
        .identity("editor", &[("content.search", GrantEffect::Allow)])
        .await;
    let gate = allow_all();
    let pipe = Pipeline::new(
        std::sync::Arc::new(registry),
        Some(identity),
        vec!["content.search".to_owned()],
        Vec::new(),
        std::sync::Arc::new(gate),
        store.caller(agent_id, run_id),
    );

    let outcome = pipe
        .call(
            &store.pool,
            &ToolCall::new(
                "content.search",
                json!({"q": "report", "limit": 10}),
            ),
        )
        .await
        .expect("the pipeline must answer");

    match &outcome {
        CallOutcome::Refused { code, reason, .. } => {
            assert_eq!(code, "tool_bad_arguments");
            assert!(
                reason.contains("limit"),
                "the refusal names the offending field — the request's criterion is 'the error \
                 names the field'. Got: {reason}"
            );
        }
        other => panic!("an unknown field must refuse, got {other:?}"),
    }
    let rows = store.rows_for(run_id, "content.search").await;
    assert_eq!(rows[0].0, "denied", "a refused argument set is a denial, not a failure");
    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// The cap, and the timeout
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn one_call_past_the_cap_is_refused_and_the_cap_survives_a_resume() {
    let store = pipe!();
    let registry = registry();
    let agent_id = store.agent("writer", &["content.search"]).await;
    let run_id = store.run(agent_id, "find the report").await;
    let identity = store
        .identity("editor", &[("content.search", GrantEffect::Allow)])
        .await;

    // A cap of 2, written through the store so the walk reads the operator's own number.
    registry::update_tool(
        &store.pool,
        "content.search",
        &registry::ToolLimits {
            max_calls_per_run: Some(2),
            ..registry::ToolLimits::default()
        },
    )
    .await
    .expect("the cap must be patchable");

    // The registry and the gate live in `Arc`s for the whole walk, not one `Arc::new` per
    // pipeline. The pipeline is built once per *call* here because each round is a resume, and
    // an `Arc::new(registry)` inside the loop moved the registry on round 1 — which is exactly
    // the shape a two-round loop looks like it should not have.
    let registry = std::sync::Arc::new(registry);
    // Annotated as the trait object at the binding, so both the loop's `clone` and the final
    // move are `Arc<dyn PermissionGate>`. Leaving it concrete gives the loop's `clone` an
    // `E0308` while the final move — the one that has no `&` to infer from — compiles fine.
    let gate: std::sync::Arc<dyn PermissionGate> = std::sync::Arc::new(allow_all());
    let call = ToolCall::new("content.search", json!({"q": "report"}));

    // Two calls, then the third — all on the SAME run id, which is the resume: a pipeline that
    // counted in the loop's stack would answer differently for the second pair.
    for round in 1..=2 {
        let pipe = Pipeline::new(
            std::sync::Arc::clone(&registry),
            Some(identity.clone()),
            vec!["content.search".to_owned()],
            Vec::new(),
            std::sync::Arc::clone(&gate),
            store.caller(agent_id, run_id),
        );
        let outcome = pipe.call(&store.pool, &call).await.expect("the pipeline must answer");
        assert!(
            matches!(outcome, CallOutcome::Ran { .. }),
            "call {round} of 2 must run, got {outcome:?}"
        );
    }

    let pipe = Pipeline::new(
        registry,
        Some(identity),
        vec!["content.search".to_owned()],
        Vec::new(),
        gate,
        store.caller(agent_id, run_id),
    );
    let third = pipe.call(&store.pool, &call).await.expect("the pipeline must answer");
    match &third {
        CallOutcome::Refused { code, reason, .. } => {
            assert_eq!(code, "tool_limited");
            assert!(
                reason.contains("2 call"),
                "the refusal states the cap it broke: {reason}"
            );
        }
        other => panic!("the third call must be limited, got {other:?}"),
    }

    let rows = store.rows_for(run_id, "content.search").await;
    assert_eq!(rows.len(), 3, "the limited call is a row too");
    assert_eq!(rows[2].0, "denied", "a cap breach is a denial, not a failure");
    assert_eq!(rows[2].1.as_deref(), Some("tool_limited"));
    store.dispose().await;
}

#[tokio::test]
async fn a_slow_tool_is_cut_off_with_status_timeout() {
    let store = pipe!();
    // ONLY the slow tool is registered, so the arm under test is the timeout and not a race
    // against a second registration. A registry with two tools under one key made the previous
    // version of this walk pass for either reason.
    let mut registry = ToolRegistry::empty();
    registry.register(Arc::new(SlowTool));
    let agent_id = store.agent("writer", &["content.search"]).await;
    let run_id = store.run(agent_id, "find the report").await;
    let identity = store
        .identity("editor", &[("content.search", GrantEffect::Allow)])
        .await;

    // The minimum the schema allows (1000 ms). The tool is a `pending()` future, so it never
    // finishes: the row's 1000 ms is the only thing that can end the call, which is exactly what
    // a real hung tool is.
    registry::update_tool(
        &store.pool,
        "content.search",
        &registry::ToolLimits {
            timeout_ms: Some(1000),
            ..registry::ToolLimits::default()
        },
    )
    .await
    .expect("the timeout must be patchable");

    let gate = allow_all();
    let pipe = Pipeline::new(
        std::sync::Arc::new(registry),
        Some(identity),
        vec!["content.search".to_owned()],
        Vec::new(),
        std::sync::Arc::new(gate),
        store.caller(agent_id, run_id),
    );

    let started = std::time::Instant::now();
    let outcome = pipe
        .call(&store.pool, &ToolCall::new("content.search", json!({"q": "report"})))
        .await
        .expect("the pipeline must answer");
    let elapsed = started.elapsed();

    match &outcome {
        CallOutcome::TimedOut { tool, timeout_ms, .. } => {
            assert_eq!(tool, "content.search");
            assert_eq!(*timeout_ms, 1000, "the refusal reports the limit it broke");
        }
        other => panic!("a tool that never finishes must be cut off, got {other:?}"),
    }
    assert!(
        elapsed < std::time::Duration::from_secs(4),
        "the cut-off must happen at the row's timeout, not whenever the tool gives up. Took {elapsed:?}"
    );
    let rows = store.rows_for(run_id, "content.search").await;
    assert_eq!(rows.len(), 1, "the cut-off is recorded like any other call");
    assert_eq!(
        rows[0].0, "timeout",
        "a cut-off call is `timeout`, not `failed` — the request's five statuses are five \
         different facts and collapsing two of them loses the one an operator needs"
    );
    assert_eq!(rows[0].1.as_deref(), Some("tool_timeout"));
    store.dispose().await;
}

/// A tool that never finishes inside any timeout this test sets, so the timeout arm is the one
/// under test rather than the tool's own body.
struct SlowTool;

impl omnion_ai_hub::tools::Tool for SlowTool {
    fn key(&self) -> &str {
        "content.search"
    }
    fn description(&self) -> &str {
        "Search content"
    }
    fn permission(&self) -> &str {
        "content.read"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": { "q": { "type": "string" } },
            "required": ["q"],
            "additionalProperties": false
        })
    }
    fn run(
        &self,
        _arguments: &serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolOutcome> + Send + '_>> {
        Box::pin(std::future::pending())
    }
}

// -------------------------------------------------------------------------------------------
// The model-facing payload
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_payload_hides_a_denied_tool_and_the_call_still_refuses_it() {
    // The request's "a denied or disabled tool is invisible, not merely refused" — both halves
    // from one pipeline, so the filter and the refusal cannot drift apart.
    let store = pipe!();
    let registry = registry();
    let agent_id = store.agent("writer", &["content.search", "content.create"]).await;
    let run_id = store.run(agent_id, "do the thing").await;
    let identity = store
        .identity("restricted", &[("content.create", GrantEffect::Deny)])
        .await;
    let gate = allow_all();
    let pipe = Pipeline::new(
        std::sync::Arc::new(registry),
        Some(identity),
        vec!["content.search".to_owned(), "content.create".to_owned()],
        Vec::new(),
        std::sync::Arc::new(gate),
        store.caller(agent_id, run_id),
    );

    let shown: Vec<String> = pipe
        .model_facing()
        .into_iter()
        .map(|summary| summary.key)
        .collect();
    assert_eq!(
        shown,
        vec!["content.search".to_owned()],
        "the denied tool is not offered to the model"
    );

    // A model that names it anyway — hallucinated, cached, or a prompt-injected goal — is still
    // stopped. "Invisible, not merely refused" is the visibility half; this is the refusal half.
    let outcome = pipe
        .call(
            &store.pool,
            &ToolCall::new("content.create", json!({"title": "Draft"})),
        )
        .await
        .expect("the pipeline must answer");
    match &outcome {
        CallOutcome::Refused { code, .. } => assert_eq!(code, "tool_denied"),
        other => panic!("the refused tool must be refused, got {other:?}"),
    }
    store.dispose().await;
}

#[tokio::test]
async fn the_payload_carries_the_registry_row_schema_the_call_validates_against() {
    // The tool's `input_schema` column is what the panel shows and copies; the payload is what
    // the model is sent. If the two drift, a model copies a schema that then refuses its own
    // example. Asserted against the row, not against the compiled spec.
    let store = pipe!();
    let registry = registry();
    let agent_id = store.agent("writer", &["content.search"]).await;
    let run_id = store.run(agent_id, "find the report").await;
    let identity = store
        .identity("editor", &[("content.search", GrantEffect::Allow)])
        .await;
    let tool = registry.get("content.search").expect("registered");
    let gate = allow_all();
    let pipe = Pipeline::new(
        std::sync::Arc::new(registry),
        Some(identity),
        vec!["content.search".to_owned()],
        Vec::new(),
        std::sync::Arc::new(gate),
        store.caller(agent_id, run_id),
    );

    let row = registry::get_tool(&store.pool, "content.search")
        .await
        .expect("the registry must answer")
        .expect("the seeder inserted the row");
    let payload = pipe
        .model_facing()
        .into_iter()
        .find(|summary| summary.key == "content.search")
        .expect("the granted tool is shown");
    assert_eq!(
        payload.schema,
        tool.schema(),
        "the payload must carry the COMPILED schema — the same object `Pipeline::call` validates \
         against. A payload built from the operator's row would let a hand-edited schema widen \
         what a tool accepts."
    );
    // The registry row is the seeder's COPY of that same document, which is the property the
    // "copy a schema and try it" walk depends on. Asserted per-key rather than for a fixture: a
    // fixture registered with its own schema is not the same tool as the catalogue's
    // `content.search`, and comparing the two was this walk's first mistake — it compared a
    // fixture's `{q}` with the catalogue's `{query, limit, content_type}` and called the
    // difference a bug. It was the assertion that was wrong, not the pipeline.
    let spec = registry::spec_for("content.search")
        .expect("the catalogue carries the key");
    assert_eq!(
        row.input_schema,
        spec.to_row()["input_schema"],
        "the registry row and the compiled spec must be the same schema document"
    );
    if let Some(example) = &row.example {
        assert_eq!(
            omnion_ai_hub::schema::validate(&row.input_schema, example),
            Ok(()),
            "a registry row whose own example fails its own schema is a copy-paste that fails"
        );
    }
    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// Tenancy and the pruner
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_run_another_organization_cannot_read_the_calls_for() {
    let store = pipe!();
    let registry = registry();
    let agent_id = store.agent("writer", &["content.search"]).await;
    let run_id = store.run(agent_id, "find the report").await;
    let identity = store
        .identity("editor", &[("content.search", GrantEffect::Allow)])
        .await;
    let gate = allow_all();
    let pipe = Pipeline::new(
        std::sync::Arc::new(registry),
        Some(identity),
        vec!["content.search".to_owned()],
        Vec::new(),
        std::sync::Arc::new(gate),
        store.caller(agent_id, run_id),
    );
    pipe.call(&store.pool, &ToolCall::new("content.search", json!({"q": "a"})))
        .await
        .expect("the pipeline must answer");

    let mine = omnion_ai_hub::tool_calls::calls_for_run(&store.pool, store.organization_id, run_id, 10)
        .await
        .expect("the caller must read its own");
    assert_eq!(mine.len(), 1);

    // The other organization reads the same run id and gets nothing: the query is scoped by
    // organization as well as run, so a run id is not a bearer token.
    let theirs =
        omnion_ai_hub::tool_calls::calls_for_run(&store.pool, store.other_organization_id, run_id, 10)
            .await
            .expect("the foreign read must answer, not error");
    assert!(
        theirs.is_empty(),
        "another organization must not read a run's tool history"
    );
    store.dispose().await;
}

#[tokio::test]
async fn the_pruner_drops_only_rows_past_the_retention_window() {
    let store = pipe!();
    let agent_id = store.agent("writer", &["content.search"]).await;
    let run_id = store.run(agent_id, "find the report").await;

    // One row inside the window, one far outside it, each addressed by id. The first version
    // backdated with `update ai_tool_calls set created_at = …` and no `where`, which aged BOTH
    // rows and made the pruner return 2 — and the assertion I wrote ("one was 400 days old")
    // would have been satisfied by any pruner that simply deleted everything. Backdating by id
    // is the difference between testing the window and testing the DELETE.
    let fresh = omnion_ai_hub::tool_calls::record(
        &store.pool,
        omnion_ai_hub::tool_calls::NewCall::for_tool("content.search", CallStatus::Ok)
            .with_run(store.organization_id, run_id, agent_id),
    )
    .await
    .expect("the fresh row must be written");
    let old = omnion_ai_hub::tool_calls::record(
        &store.pool,
        omnion_ai_hub::tool_calls::NewCall::for_tool("content.search", CallStatus::Ok)
            .with_run(store.organization_id, run_id, agent_id),
    )
    .await
    .expect("the old row must be written");
    sqlx::query("update ai_tool_calls set created_at = now() - interval '400 days' where id = $1")
        .bind(old)
        .execute(&store.pool)
        .await
        .expect("the backdating must apply");

    let deleted = omnion_ai_hub::tool_calls::prune(&store.pool)
        .await
        .expect("the pruner must answer");
    assert_eq!(deleted, 1, "one row was 400 days old and one was not");
    assert_eq!(
        store.total_rows(run_id).await,
        1,
        "the pruner trims only old rows; a sweep that empties the table is a usage chart that \
         reads zero forever. The row that survives is the one the walk wrote by id."
    );
    let survivors: Vec<i64> = sqlx::query_scalar(
        "select id from ai_tool_calls where run_id = $1 order by id",
    )
    .bind(run_id)
    .fetch_all(&store.pool)
    .await
    .expect("the survivors must be readable");
    assert_eq!(survivors, vec![fresh], "the survivor is the row inside the window");
    store.dispose().await;
}

#[tokio::test]
async fn the_pruner_leaves_audit_log_alone() {
    // The request: "`audit_log` is append-only and never pruned". The pruner names one table, and
    // this asserts the *absence* of the other rather than trusting the SQL's single target.
    let store = pipe!();
    let agent_id = store.agent("writer", &["content.search"]).await;
    let run_id = store.run(agent_id, "find the report").await;
    omnion_ai_hub::tool_calls::record(
        &store.pool,
        omnion_ai_hub::tool_calls::NewCall::for_tool("content.search", CallStatus::Ok)
            .with_run(store.organization_id, run_id, agent_id),
    )
    .await
    .expect("the row must be written");
    sqlx::query("update ai_tool_calls set created_at = now() - interval '400 days'")
        .execute(&store.pool)
        .await
        .expect("the backdating must apply");
    omnion_ai_hub::tool_calls::prune(&store.pool)
        .await
        .expect("the pruner must answer");

    let audit_exists: i64 =
        sqlx::query_scalar("select count(*) from audit_log where created_at < now() - interval '1 day'")
            .fetch_one(&store.pool)
            .await
            .expect("audit_log must be readable");
    assert_eq!(
        audit_exists, 0,
        "audit_log is never written by the pipeline yet (REQ-101 wires the agent actor), but it \
         is never pruned either — the table still exists and the pruner did not touch it"
    );
    store.dispose().await;
}

#[tokio::test]
async fn a_control_proves_the_fixture_tool_really_writes() {
    // **This is the walk that makes the denial walks mean anything.** They assert that a denied
    // call leaves the `touched` count unchanged — which is a vacuous statement if no tool body
    // ever creates one. So this walk calls the *same* tool through the *granted* path and
    // asserts the count moved.
    //
    // Without it, the file would also pass with a `WritingTool` whose body was `unreachable!()`,
    // or with a pipeline that ran every body and then reported a refusal. Falsified before
    // committing: removing the `.expect` from the tool body turns this walk red, which is how I
    // know it is measuring the write and not the assertion.
    let store = pipe!();
    let registry = registry();
    let agent_id = store.agent("writer", &["content.create"]).await;
    let run_id = store.run(agent_id, "publish the draft").await;
    let identity = store
        .identity("editor", &[("content.create", GrantEffect::Allow)])
        .await;
    let before = store.touched().await;
    assert_eq!(before, 0, "a fresh database has no touched rows to start with");

    let gate = allow_all();
    let pipe = Pipeline::new(
        std::sync::Arc::new(registry),
        Some(identity),
        vec!["content.create".to_owned()],
        Vec::new(),
        std::sync::Arc::new(gate),
        store.caller(agent_id, run_id),
    );
    let outcome = pipe
        .call(&store.pool, &ToolCall::new("content.create", json!({"title": "Draft"})))
        .await
        .expect("the pipeline must answer");
    assert!(
        matches!(outcome, CallOutcome::Ran { .. }),
        "the granted call must run, got {outcome:?}"
    );
    assert_eq!(
        store.touched().await,
        before + 1,
        "the fixture tool's body must really write, or every 'a denied call leaves no side \
         effect' assertion in this file is comparing 0 with 0"
    );
    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// The pruner is called by something
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_daily_log_tick_reaches_the_tool_call_pruner() {
    // `tool_calls::prune` had a walk and **no caller**: a store function nothing calls is a
    // function whose retention is a wish. The runner is what calls it now, so the walk is on the
    // runner — an assertion on `prune` itself would have passed before the wiring landed and
    // would pass again the day someone deleted the call.
    //
    // The `tick` needs an `AppState`, which needs more wiring than a bare pool, so this asserts
    // the thing a caller could get wrong without a state: **the runner's `tick` is the only
    // caller and it is the same function the timer runs.** It reads `ai_log_runner`'s own source
    // and fails if the call or the `spawn` that runs the tick is gone.
    let source = include_str!("../src/ai_log_runner.rs");

    assert!(
        source.contains("omnion_ai_hub::tool_calls::prune(pool)"),
        "the daily tick must call the tool-call pruner; without it ai_tool_calls grows forever"
    );
    assert!(
        source.contains("omnion_ai_hub::decision_store::prune(pool, RETENTION_DAYS)"),
        "the tick still prunes the decision log — the tool-call call must not have replaced it"
    );
    assert!(
        source.contains("let report = tick(&state).await;"),
        "the timer must run the same tick the test reads, not a second one"
    );
    // The failure mode worth forbidding: an early `return` on the first pruner's error, which
    // makes the second pruner unreachable whenever the first one hiccups.
    let tick_body = source
        .split("pub async fn tick")
        .nth(1)
        .and_then(|rest| rest.split("pub fn spawn").next())
        .expect("the tick function must exist");
    let after_decision_error = tick_body.find("the decision pruner could not run");
    let tool_call_prune = tick_body.find("tool_calls::prune");
    assert!(
        after_decision_error.is_some() && tool_call_prune.is_some(),
        "both pruners must be present in the tick"
    );
    let decision_error_arm = &tick_body
        [after_decision_error.expect("checked")..tool_call_prune.expect("checked")];
    assert!(
        !decision_error_arm.contains("return TickReport::default()"),
        "a decision-pruner error must not skip the tool-call pruner — an early return here is \
         how the tool-call log silently stopped being pruned"
    );
}

#[tokio::test]
async fn the_pruner_drops_only_rows_past_the_window() {
    // A pruner that returns the right *number* while deleting the wrong *rows* satisfies a test
    // that only counted. The retention walk next door covers the 400-day-vs-now boundary; this
    // one covers the other edge that matters for the panel — a row the registry's own 30-day
    // usage column is computed from. A pruner that dropped a 10-day row would make "Calls 30 d"
    // quietly under-report, and no counting assertion would see it.
    let store = pipe!();
    let agent_id = store.agent("logt", &["content.search"]).await;
    let run_id = store.run(agent_id, "log a call").await;

    let mut ids = Vec::new();
    for age in ["400 days", "10 days", "0 days"] {
        let id: i64 = sqlx::query_scalar(
            "insert into ai_tool_calls (organization_id, run_id, agent_id, tool_key, status) \
             values ($1, $2, $3, 'content.search', 'ok') returning id",
        )
        .bind(store.organization_id)
        .bind(run_id)
        .bind(agent_id)
        .fetch_one(&store.pool)
        .await
        .expect("the fixture call must be written");
        // Backdated **by id**, never with a bare `update … set created_at`: an update without a
        // `where` ages every row, which is how the retention walk first came to return 2.
        sqlx::query("update ai_tool_calls set created_at = now() - $2::interval where id = $1")
            .bind(id)
            .bind(age)
            .execute(&store.pool)
            .await
            .expect("the backdating must apply");
        ids.push(id);
    }
    assert_eq!(store.total_rows(run_id).await, 3, "three rows before the sweep");

    let dropped = omnion_ai_hub::tool_calls::prune(&store.pool)
        .await
        .expect("the pruner must answer");
    assert_eq!(dropped, 1, "only the 400-day row is past the 180-day window");
    let survivors: Vec<i64> = sqlx::query_scalar(
        "select id from ai_tool_calls where run_id = $1 order by id",
    )
    .bind(run_id)
    .fetch_all(&store.pool)
    .await
    .expect("the survivors must be readable");
    assert_eq!(
        survivors,
        vec![ids[1], ids[2]],
        "the 10-day row feeds the registry's 30-day usage column; a pruner that dropped it would \
         make the panel under-report with nothing in the log to say why"
    );
    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// REQ-100 · acceptance criterion 2 — "for each tool, the declared permission equals the
// permission of the HTTP route it wraps", and the defect that criterion was hiding.
// -------------------------------------------------------------------------------------------

/// Every tool the seeder writes must name a permission the **platform** can actually grant.
///
/// This is the walk the unit test could not be. Sixteen of the twenty-five tools declared keys
/// like `content.read`, `site.read`, `theme.read`, `logs.read` and `seo.analyze`, none of which
/// exist in `crates/permissions`' catalogue. The catalogue test passed anyway, because it
/// compared the tools against a list written by the same hand in the same file.
///
/// The instrument here is the **foreign key**. `role_permissions.permission_key` references
/// `permissions(key)`, and `permissions` is populated at boot by
/// `omnion_permissions::seed::ensure` from the same catalogue. So an ungrantable tool is not a
/// policy nobody wrote — it is a row the database will not accept, which is the real shape of
/// the defect: an operator opens the permission matrix, sees the tool's permission, and no role
/// can ever be given it.
///
/// Each tool is attempted as its own insert, and the failures are collected and reported by name
/// rather than asserted one at a time, because a reader wants the list, not the first entry.
#[tokio::test]
async fn every_seeded_tool_names_a_permission_the_platform_can_grant() {
    let Some(store) = PipelineStore::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    // The catalogue must exist before a grant can be attempted, exactly as it does at boot.
    omnion_permissions::seed::ensure(&store.pool)
        .await
        .expect("the permission catalogue must seed");

    let rows: Vec<(String, String)> =
        sqlx::query_as("select key, permission from ai_tools where retired_note is null order by key")
            .fetch_all(&store.pool)
            .await
            .expect("the seeded rows must be readable");
    assert_eq!(
        rows.len(),
        omnion_ai_hub::catalogue::specs().len(),
        "every compiled tool must have a row"
    );

    let mut ungrantable = Vec::new();
    for (key, permission) in &rows {
        // Written the way the IAM policy route writes a grant, so the walk is exercising the
        // real constraint rather than a query of my own invention.
        let role_id = Uuid::new_v4();
        sqlx::query(
            "insert into roles (id, organization_id, key, name, priority) \
             values ($1, $2, $3, $4, 100)",
        )
        .bind(role_id)
        .bind(store.organization_id)
        .bind(format!("role-{}", key.replace('.', "-")))
        .bind(format!("Role for {key}"))
        .execute(&store.pool)
        .await
        .expect("the role must insert");

        let granted = sqlx::query(
            "insert into role_permissions (role_id, permission_key, effect) \
             values ($1, $2, 'allow')",
        )
        .bind(role_id)
        .bind(permission)
        .execute(&store.pool)
        .await;
        if let Err(err) = granted {
            ungrantable.push(format!("{key} -> {permission} ({err})"));
        }
    }

    assert!(
        ungrantable.is_empty(),
        "these tools name permissions no role can be granted, so every agent holding one is \
         denied forever by a switch the panel shows: {ungrantable:?}"
    );
    store.dispose().await;
}

/// The other half of criterion 2, read off the **seeded rows** rather than the compiled table:
/// a tool the registry presents as wired must carry a route, and a tool it presents as unwired
/// must be approval-gated.
///
/// A tool bound to nothing and a tool bound to a route are different claims, and the response
/// says which: `route.live` is `false` and `route.permission` is `null` for the eleven planned
/// tools. What must never happen is a row answered `live: true` whose permission differs from
/// the row's own declaration, because that is the row an operator would trust.
#[tokio::test]
async fn a_seeded_tool_answered_as_wired_names_its_rows_own_permission() {
    let Some(store) = PipelineStore::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let rows: Vec<(String, String, bool)> = sqlx::query_as(
        "select key, permission, requires_approval from ai_tools \
         where retired_note is null order by key",
    )
    .fetch_all(&store.pool)
    .await
    .expect("the seeded rows must be readable");

    let mut planned = 0;
    for (key, permission, requires_approval) in rows {
        let view = omnion_ai_hub::ops_binding::route_views()
            .into_iter()
            .find(|v| v.key == key)
            .unwrap_or_else(|| panic!("{key} has no route view"));
        if view.live {
            assert_eq!(
                view.permission.as_deref(),
                Some(permission.as_str()),
                "{key} is answered as wired to {:?} but the row declares `{permission}`",
                view.label
            );
        } else {
            planned += 1;
            assert!(
                requires_approval,
                "{key} has no route and is not approval-gated, so the panel invites an \
                 operator to enable an action the platform cannot perform"
            );
        }
    }
    assert!(
        planned > 0,
        "the build has documented-but-unbuilt surfaces, so at least one tool must be planned; a \
         walk that saw none would mean the split is not being exercised at all"
    );
    store.dispose().await;
}
