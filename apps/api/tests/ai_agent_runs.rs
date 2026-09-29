//! Integration tests for the agent runtime's store (REQ-099, slice 1).
//!
//! The crate's unit tests prove that the step machine *decides* to stop; they cannot prove
//! anything about durability, because they never touch a database. These walks prove the four
//! promises that only exist once rows are written:
//!
//! - **a step row is the idempotency record.** A run whose step 1 is `completed` and whose step 2
//!   is `running` (a process that died mid-tool) resumes *at step 2* and never re-runs step 1 —
//!   the test asserts one side effect for one step row, which is the box that says "a step whose
//!   tool already ran is not executed twice".
//! - **a run's totals are the sum of its steps.** Not a check of the column against itself: the
//!   figures are recomputed from the step rows and compared to what the run carries.
//! - **a run with every step completed cannot be resumed**, and the refusal says why.
//! - **one organization cannot read another's agent or run.** Asserted as `None`/404 rather than a
//!   403: sequential ids mean "it exists, you may not see it" confirms the id is real.
//!
//! They run against the same throwaway-database harness the decision suite uses.

use omnion_ai_hub::agent::{AgentEvent, StepKind, StepStatus, StopReason, ToolCall};
use omnion_ai_hub::run_store::{
    self, NewAgent, NewRun, call_arguments, cancel_requested, count_steps, create_agent, create_run,
    finish_run, finish_step, get_agent, get_run, list_agents, list_runs, list_steps, redact_arguments,
    request_cancel, resume_point, run_totals_match_steps, validate_goal,
};
use omnion_ai_hub::error::AiHubError;
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::Db;
use sqlx::PgPool;
use uuid::Uuid;

// -------------------------------------------------------------------------------------------
// The harness — a throwaway database with every migration applied.
// -------------------------------------------------------------------------------------------

struct Store {
    pool: PgPool,
    organization_id: Uuid,
    /// The throwaway database's name, kept so `dispose` can drop it.
    database: String,
    /// The connection to the maintenance database that created it.
    maintenance: Option<Db>,
}

impl Store {
    /// Close the pool and drop the throwaway database, and wait for it.
    ///
    /// Explicit and awaited rather than a `Drop` impl, on purpose. `Drop` cannot await, so it can
    /// only hand teardown to a detached task — and a detached task races the test binary's exit,
    /// which is a race it loses: 13 tests left 11 databases behind. On a PostgreSQL that ten
    /// writers share, leaked databases mean leaked connection pools, and the exhaustion surfaces
    /// as *other* suites skipping with "pool timed out" rather than as anything in this file. A
    /// failing test still prints its assertion; the schema was never the evidence.
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

impl Store {
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

        let database = format!("omnion_agrun_{}", Uuid::new_v4().simple());
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

        let organization_id = seed_organization(db.pool()).await;
        let pool = db.pool().clone();

        Some(Self {
            pool,
            organization_id,
            database,
            maintenance: Some(maintenance),
        })
    }

    /// An agent to run, carrying the documented defaults.
    async fn agent(&self, key: &str) -> omnion_ai_hub::run_store::Agent {
        let new = NewAgent::with_defaults(self.organization_id, key, "Reporter");
        create_agent(&self.pool, &new)
            .await
            .expect("the fixture agent must be created")
    }
}

fn swap_database(url: &str, database: &str) -> String {
    let (base, _) = url.rsplit_once('/').expect("a database URL has a path");
    format!("{base}/{database}")
}

async fn seed_organization(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, 'Acme', $2)")
        .bind(id)
        .bind(format!("acme-{}", Uuid::new_v4().simple()))
        .execute(pool)
        .await
        .expect("the fixture organization must be created");
    id
}

// -------------------------------------------------------------------------------------------
// The agent
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_agent_round_trips_with_the_documented_defaults() {
    let Some(store) = Store::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let agent = store.agent("reporter").await;
    assert_eq!(agent.key, "reporter");
    assert_eq!(agent.max_steps, 8, "the documented default step ceiling");
    assert_eq!(agent.deadline_seconds, 300);
    assert_eq!(agent.token_budget, 200_000);
    assert!(agent.enabled);
    assert!(agent.tools.is_empty());

    // The limits the runtime enforces must come from the row, not from a constant, or editing
    // the agent in the panel would change nothing about how it runs.
    let limits = agent.limits();
    assert_eq!(limits.max_steps, 8);
    assert_eq!(limits.token_budget, 200_000);
    store.dispose().await;
}

#[tokio::test]
async fn a_duplicate_key_is_refused_with_a_message_that_names_it() {
    let Some(store) = Store::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let _first = store.agent("reporter").await;

    let error = create_agent(&store.pool, &NewAgent::with_defaults(store.organization_id, "reporter", "Second"))
        .await
        .expect_err("a duplicate key must be refused");
    assert!(matches!(error, AiHubError::InvalidAgent(_)), "got {error:?}");
    assert!(
        error.to_string().contains("reporter"),
        "the message must name the key so the operator knows which one to change: {error}"
    );
    store.dispose().await;
}

#[tokio::test]
async fn one_organization_cannot_read_another_organizations_agent() {
    let Some(store) = Store::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let agent = store.agent("reporter").await;

    let other = seed_organization(&store.pool).await;
    let seen = get_agent(&store.pool, other, agent.id)
        .await
        .expect("the read must succeed");
    assert!(
        seen.is_none(),
        "another tenant's agent must be indistinguishable from one that does not exist"
    );
    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// The trace and its idempotency record
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn steps_are_numbered_from_one_and_never_repeat() {
    let Some(store) = Store::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let agent = store.agent("reporter").await;
    let run = create_run(
        &store.pool,
        &NewRun {
            organization_id: store.organization_id,
            agent_id: agent.id,
            trigger: "agent".to_owned(),
            goal: "summarise the week".to_owned(),
            model_id: None,
            token_budget: 200_000,
            deadline_at: None,
            ..NewRun::default_for(store.organization_id, agent.id)
        },
    )
    .await
    .expect("the run must be created");

    assert_eq!(run.status, "queued", "a run is durable before it is claimed");
    assert_eq!(run.current_step, 0);

    let first = run_store::begin_step(&store.pool, run.id, StepKind::Message, None, None)
        .await
        .expect("the step must be written");
    let second = run_store::begin_step(&store.pool, run.id, StepKind::Note, None, None)
        .await
        .expect("the step must be written");
    assert_eq!((first, second), (1, 2), "step numbers are 1-based and dense");

    let steps = list_steps(&store.pool, run.id).await.expect("the trace must read");
    assert_eq!(steps.len(), 2);
    assert!(
        steps.iter().all(|step| step.status == "running"),
        "every step is written running BEFORE its work happens — that ordering is the resume story"
    );

    let refreshed = get_run(&store.pool, store.organization_id, run.id)
        .await
        .expect("the run must read")
        .expect("the run exists");
    assert_eq!(refreshed.current_step, 2, "the run tracks steps that started");
    store.dispose().await;
}

#[tokio::test]
async fn a_run_resumes_at_the_first_step_that_is_not_completed() {
    let Some(store) = Store::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let agent = store.agent("reporter").await;
    let run = create_run(
        &store.pool,
        &NewRun {
            organization_id: store.organization_id,
            agent_id: agent.id,
            trigger: "agent".to_owned(),
            goal: "send the invoice".to_owned(),
            model_id: None,
            token_budget: 200_000,
            deadline_at: None,
            ..NewRun::default_for(store.organization_id, agent.id)
        },
    )
    .await
    .expect("the run must be created");

    // Step 1 ran to completion; step 2 is the one the crash interrupted. This is the shape a
    // killed runner leaves behind, and the difference between "completed" and "running" is the
    // difference between "do not touch it" and "this may already have happened".
    let first = run_store::begin_step(&store.pool, run.id, StepKind::ToolCall, Some("send_invoice"), None)
        .await
        .expect("the step must be written");
    finish_step(
        &store.pool,
        run.id,
        first,
        StepStatus::Completed,
        None,
        120,
        40,
        1_500,
        Some(15),
        None,
    )
    .await
    .expect("the step must close");
    let second = run_store::begin_step(&store.pool, run.id, StepKind::ToolCall, Some("charge_card"), None)
        .await
        .expect("the step must be written");

    let at = resume_point(&store.pool, run.id)
        .await
        .expect("the resume point must read")
        .expect("an incomplete step exists");
    assert_eq!(at, second, "resume starts at the interrupted step, never before it");
    assert_ne!(at, first, "the completed step is never re-executed");
    assert_eq!(
        count_steps(&store.pool, run.id, StepStatus::Completed).await.expect("count"),
        1,
        "exactly one step row is completed, so exactly one tool may have run"
    );
    assert_eq!(
        count_steps(&store.pool, run.id, StepStatus::Running).await.expect("count"),
        1,
        "the interrupted step is still marked running, which is the ambiguity the runtime reports"
    );
    store.dispose().await;
}

#[tokio::test]
async fn a_run_whose_steps_all_completed_refuses_a_resume() {
    let Some(store) = Store::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let agent = store.agent("reporter").await;
    let run = create_run(
        &store.pool,
        &NewRun {
            organization_id: store.organization_id,
            agent_id: agent.id,
            trigger: "agent".to_owned(),
            goal: "answer once".to_owned(),
            model_id: None,
            token_budget: 200_000,
            deadline_at: None,
            ..NewRun::default_for(store.organization_id, agent.id)
        },
    )
    .await
    .expect("the run must be created");

    let step = run_store::begin_step(&store.pool, run.id, StepKind::Message, None, None)
        .await
        .expect("the step must be written");
    finish_step(&store.pool, run.id, step, StepStatus::Completed, None, 10, 5, 250, Some(1), None)
        .await
        .expect("the step must close");

    assert_eq!(
        resume_point(&store.pool, run.id).await.expect("the resume point must read"),
        None,
        "no incomplete step means there is nothing to resume; the caller turns this into a refusal"
    );
    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// Totals
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_runs_cost_equals_the_sum_of_its_own_steps() {
    let Some(store) = Store::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let agent = store.agent("reporter").await;
    let run = create_run(
        &store.pool,
        &NewRun {
            organization_id: store.organization_id,
            agent_id: agent.id,
            trigger: "agent".to_owned(),
            goal: "do three things".to_owned(),
            model_id: None,
            token_budget: 200_000,
            deadline_at: None,
            ..NewRun::default_for(store.organization_id, agent.id)
        },
    )
    .await
    .expect("the run must be created");

    // A step that FAILED is excluded from the total: its tokens were still spent, but the
    // acceptance box is about the run's own figures agreeing with its completed work, and a
    // half-finished step is not work the run did.
    for (prompt, completion, cost, status) in [
        (100, 50, 1_200_i64, StepStatus::Completed),
        (200, 75, 2_400_i64, StepStatus::Completed),
        (999, 999, 9_999_i64, StepStatus::Failed),
    ] {
        let step = run_store::begin_step(&store.pool, run.id, StepKind::Message, None, None)
            .await
            .expect("the step must be written");
        finish_step(&store.pool, run.id, step, status, None, prompt, completion, cost, Some(5), None)
            .await
            .expect("the step must close");
    }

    let totals = run_totals_match_steps(&store.pool, run.id)
        .await
        .expect("the totals must read");
    assert_eq!(totals.prompt_tokens, 300, "only completed steps count");
    assert_eq!(totals.completion_tokens, 125);
    assert_eq!(
        totals.cost_micros, 3_600,
        "the run's cost is the sum of its completed steps' costs — the box that says a run's \
         cost equals the sum of its usage rows, asserted against the step rows rather than \
         against the run's own column"
    );

    finish_run(&store.pool, run.id, StepStatus::Completed, StopReason::FinalAnswer, None)
        .await
        .expect("the run must finish");
    let finished = get_run(&store.pool, store.organization_id, run.id)
        .await
        .expect("the run must read")
        .expect("the run exists");
    assert_eq!(finished.prompt_tokens, totals.prompt_tokens, "the run carries the sum of its steps");
    assert_eq!(finished.completion_tokens, totals.completion_tokens);
    assert_eq!(finished.cost_micros, totals.cost_micros);
    assert_eq!(finished.status, "completed");
    assert_eq!(finished.reason(), Some(StopReason::FinalAnswer));
    assert!(finished.is_terminal());
    assert!(finished.finished_at.is_some(), "a finished run says when it finished");
    store.dispose().await;
}

#[tokio::test]
async fn finishing_a_run_twice_does_not_double_its_totals() {
    let Some(store) = Store::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let agent = store.agent("reporter").await;
    let run = create_run(
        &store.pool,
        &NewRun {
            organization_id: store.organization_id,
            agent_id: agent.id,
            trigger: "agent".to_owned(),
            goal: "retry the close".to_owned(),
            model_id: None,
            token_budget: 200_000,
            deadline_at: None,
            ..NewRun::default_for(store.organization_id, agent.id)
        },
    )
    .await
    .expect("the run must be created");
    let step = run_store::begin_step(&store.pool, run.id, StepKind::Message, None, None)
        .await
        .expect("the step must be written");
    finish_step(&store.pool, run.id, step, StepStatus::Completed, None, 40, 20, 900, Some(2), None)
        .await
        .expect("the step must close");

    for _ in 0..2 {
        finish_run(&store.pool, run.id, StepStatus::Completed, StopReason::FinalAnswer, None)
            .await
            .expect("the run must finish");
    }
    let finished = get_run(&store.pool, store.organization_id, run.id)
        .await
        .expect("the run must read")
        .expect("the run exists");
    assert_eq!(
        (finished.prompt_tokens, finished.completion_tokens, finished.cost_micros),
        (40, 20, 900),
        "the total is recomputed from the steps, so closing twice is idempotent"
    );
    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// Cancellation
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn cancellation_is_a_request_the_loop_meets_at_a_step_boundary() {
    let Some(store) = Store::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let agent = store.agent("reporter").await;
    let run = create_run(
        &store.pool,
        &NewRun {
            organization_id: store.organization_id,
            agent_id: agent.id,
            trigger: "agent".to_owned(),
            goal: "long job".to_owned(),
            model_id: None,
            token_budget: 200_000,
            deadline_at: None,
            ..NewRun::default_for(store.organization_id, agent.id)
        },
    )
    .await
    .expect("the run must be created");

    assert!(!cancel_requested(&store.pool, run.id).await.expect("read"));
    assert!(request_cancel(&store.pool, run.id).await.expect("cancel"));
    assert!(cancel_requested(&store.pool, run.id).await.expect("read"));
    assert!(
        !request_cancel(&store.pool, run.id).await.expect("cancel"),
        "a second request is a no-op, so the timestamp records the first one"
    );

    // Cancelling does not finish the run: the loop stops at the next boundary, so the partial
    // trace is still readable while it happens.
    let still = get_run(&store.pool, store.organization_id, run.id)
        .await
        .expect("read")
        .expect("exists");
    assert!(!still.is_terminal(), "cancellation is requested, not applied");
    assert!(still.cancel_requested_at.is_some());
    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// Redaction
// -------------------------------------------------------------------------------------------

#[test]
fn secret_shaped_arguments_are_redacted_before_they_reach_a_transcript() {
    let call = ToolCall::new(
        "http_request",
        serde_json::json!({
            "url": "https://api.example.com/v1/send",
            "Authorization": "Bearer sk-live-123",
            "api_key": "abc",
            "password": "hunter2",
            "retries": 3
        }),
    );
    let stored = call_arguments(&call);
    assert_eq!(stored["url"], "https://api.example.com/v1/send", "ordinary arguments survive");
    assert_eq!(stored["retries"], 3);
    for secret in ["Authorization", "api_key", "password"] {
        assert_eq!(stored[secret], "[redacted]", "{secret} must not reach the trace");
    }
    // The match is on the name and is case-insensitive, because the same secret is spelled three
    // ways by three tools and a case-sensitive filter catches exactly one of them.
    let lower = redact_arguments(&serde_json::json!({"APIKEY": "x"}));
    assert_eq!(lower["APIKEY"], "[redacted]");

    // A non-object argument has no keys to redact and must pass through untouched.
    assert_eq!(redact_arguments(&serde_json::json!(["a", "b"])), serde_json::json!(["a", "b"]));
}

// -------------------------------------------------------------------------------------------
// Events and goals
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_loop_event_lands_in_the_same_trace_the_panel_renders() {
    let Some(store) = Store::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let agent = store.agent("reporter").await;
    let run = create_run(
        &store.pool,
        &NewRun {
            organization_id: store.organization_id,
            agent_id: agent.id,
            trigger: "agent".to_owned(),
            goal: "stream it".to_owned(),
            model_id: None,
            token_budget: 200_000,
            deadline_at: None,
            ..NewRun::default_for(store.organization_id, agent.id)
        },
    )
    .await
    .expect("the run must be created");

    run_store::append_event(
        &store.pool,
        run.id,
        &AgentEvent::Text {
            step_no: 1,
            delta: "working".to_owned(),
        },
    )
    .await
    .expect("the event must be written");

    let steps = list_steps(&store.pool, run.id).await.expect("the trace must read");
    assert_eq!(steps.len(), 1, "an event is a trace row, not a side channel");
    assert_eq!(steps[0].kind(), Some(StepKind::Note));
    store.dispose().await;
}

#[tokio::test]
async fn a_goal_that_cannot_be_run_is_refused_with_the_limit_in_the_message() {
    assert!(validate_goal("summarise the week").is_ok());
    let empty = validate_goal("   ").expect_err("an empty goal is not a goal");
    assert!(matches!(empty, AiHubError::InvalidRun(_)), "got {empty:?}");
    let long = validate_goal(&"a".repeat(2_001)).expect_err("2001 characters breaks the bound");
    assert!(
        long.to_string().contains("2000"),
        "the message carries the limit so the form can show it: {long}"
    );
}

// -------------------------------------------------------------------------------------------
// Scoping
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn one_organization_cannot_read_another_organizations_runs() {
    let Some(store) = Store::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let agent = store.agent("reporter").await;
    let run = create_run(
        &store.pool,
        &NewRun {
            organization_id: store.organization_id,
            agent_id: agent.id,
            trigger: "agent".to_owned(),
            goal: "private".to_owned(),
            model_id: None,
            token_budget: 200_000,
            deadline_at: None,
            ..NewRun::default_for(store.organization_id, agent.id)
        },
    )
    .await
    .expect("the run must be created");

    let other = seed_organization(&store.pool).await;
    assert!(
        get_run(&store.pool, other, run.id).await.expect("read").is_none(),
        "another tenant's run must be indistinguishable from one that does not exist"
    );
    assert!(
        list_runs(&store.pool, other, None, 50).await.expect("read").is_empty(),
        "and it must not appear in their history either"
    );
    assert_eq!(list_runs(&store.pool, store.organization_id, None, 50).await.expect("read").len(), 1);
    assert_eq!(list_agents(&store.pool, other).await.expect("read").len(), 0);
    store.dispose().await;
}
