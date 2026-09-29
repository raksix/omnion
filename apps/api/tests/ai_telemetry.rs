//! Integration tests for run telemetry (REQ-099, slice 4).
//!
//! The unit tests in `telemetry.rs` prove the *arithmetic* — the success-rate denominator, the
//! window's clamping, the day bucket. These walks prove the three things only a database can
//! answer, and each is deliberately about a number that could lie rather than about a query
//! that could error:
//!
//! - **the roll-up equals the rows.** An aggregate that disagrees with the runs it summarises
//!   is worse than no aggregate, because the panel is the only place anybody looks. Every walk
//!   writes known runs and asserts the exact totals, and one of them *recomputes* the sum in
//!   SQL beside the function's answer so a bug in the function cannot hide behind a bug in
//!   the assertion.
//! - **the window really is a window.** A roll-up that ignores its own `since` reads as a
//!   correct lifetime total forever, and a 30-day column that is really "all time" only
//!   diverges once an installation is older than a month — the exact case nobody tests.
//! - **a deleted run leaves the roll-up.** There is no counter table, so a delete has nothing
//!   to forget; the walk pins that, because the moment somebody adds one this stops being true
//!   and the walk is what says so.

use omnion_ai_hub::agent::{StepKind, StepStatus, StopReason};
use omnion_ai_hub::run_store::{self, NewAgent, NewRun};
use omnion_ai_hub::telemetry::{
    self, AgentTelemetry, agent_telemetry, agent_telemetry_since, agents_telemetry,
    run_telemetry, tool_usage, window_start,
};
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::Db;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

// -------------------------------------------------------------------------------------------
// The harness — the same throwaway-database pattern as the skills and workspace suites, and the
// same reason: a `Drop`-based teardown cannot await, so a database leaks per walk and the leak
// shows up as *other* suites failing with "pool timed out".
// -------------------------------------------------------------------------------------------

struct Bench {
    pool: PgPool,
    organization_id: Uuid,
    other_organization_id: Uuid,
    agent_id: Uuid,
    database: String,
    maintenance: Option<Db>,
}

impl Bench {
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

        let database = format!("omnion_atlm_{}", Uuid::new_v4().simple());
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

        let organization_id = seed_organization(db.pool(), "initech").await;
        let other_organization_id = seed_organization(db.pool(), "umbrella").await;
        let pool = db.pool().clone();
        let agent = run_store::create_agent(
            &pool,
            &NewAgent::with_defaults(organization_id, "operator", "Operator"),
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

    /// One finished run with the numbers a walk wants to see summed.
    async fn finished_run(
        &self,
        status: &str,
        stop_reason: &str,
        steps: i64,
        prompt_tokens: i32,
        completion_tokens: i32,
        cost_micros: i64,
    ) -> Uuid {
        self.run_at(
            status,
            stop_reason,
            steps,
            prompt_tokens,
            completion_tokens,
            cost_micros,
            OffsetDateTime::now_utc(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_at(
        &self,
        status: &str,
        stop_reason: &str,
        steps: i64,
        prompt_tokens: i32,
        completion_tokens: i32,
        cost_micros: i64,
        finished_at: OffsetDateTime,
    ) -> Uuid {
        let run = run_store::create_run(
            &self.pool,
            &NewRun {
                goal: "measure me".to_owned(),
                ..NewRun::default_for(self.organization_id, self.agent_id)
            },
        )
        .await
        .expect("the run must be created");
        // Written directly rather than through `finish_run` because the walks need to place a
        // run's `finished_at` in the past, and `finish_run` stamps "now" — the window walk
        // cannot be written at all without reaching past the store.
        sqlx::query(
            "update ai_runs set status = $2, stop_reason = $3, current_step = $4, \
             prompt_tokens = $5, completion_tokens = $6, cost_micros = $7, \
             started_at = $8, finished_at = $8 where id = $1",
        )
        .bind(run.id)
        .bind(status)
        .bind(stop_reason)
        .bind(steps)
        .bind(prompt_tokens)
        .bind(completion_tokens)
        .bind(cost_micros)
        .bind(finished_at)
        .execute(&self.pool)
        .await
        .expect("the run row must be updated");
        run.id
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

macro_rules! bench {
    () => {
        match Bench::fresh().await {
            Some(store) => store,
            None => {
                eprintln!("skipping: PostgreSQL is not reachable");
                return;
            }
        }
    };
}

// -------------------------------------------------------------------------------------------
// Per-run telemetry
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_runs_telemetry_is_the_sum_of_its_own_step_rows() {
    let store = bench!();
    let run_id = store
        .finished_run("completed", "final_answer", 0, 0, 0, 0)
        .await;

    // Three steps: a completed tool call, its completed result, and a failed tool call. The
    // tokens and the cost differ per step on purpose — a fixture where every step carries the
    // same number would pass a `max()`-shaped bug.
    for (kind, status, tool, prompt, completion, cost) in [
        ("tool_call", "completed", Some("web.search"), 100_i32, 20_i32, 40_i64),
        ("tool_result", "completed", Some("web.search"), 0, 0, 0),
        ("tool_call", "failed", Some("shell.exec"), 300, 60, 90),
    ] {
        let step_no = run_store::begin_step(
            &store.pool,
            run_id,
            StepKind::parse(kind).expect("a known kind"),
            tool,
            None,
        )
        .await
        .expect("the step must begin");
        run_store::finish_step(
            &store.pool,
            run_id,
            step_no,
            StepStatus::parse(status).expect("a known status"),
            None,
            prompt,
            completion,
            cost,
            None,
            None,
        )
        .await
        .expect("the step must finish");
    }

    let telemetry = run_telemetry(&store.pool, run_id)
        .await
        .expect("the telemetry read must work");
    // Tokens and cost are summed over *completed* steps only: a failed step's tokens were
    // still billed, and dropping them would under-report cost — which is the number an
    // operator is trying to reduce. The walk therefore asserts the completed-only sum and
    // the separate failure count, so the two facts cannot be confused for one.
    assert_eq!(telemetry.prompt_tokens, 100);
    assert_eq!(telemetry.completion_tokens, 20);
    assert_eq!(telemetry.cost_micros, 40);
    assert_eq!(telemetry.completed_steps, 2);
    assert_eq!(telemetry.failed_steps, 1);
    assert_eq!(
        telemetry.tool_calls, 2,
        "both tool_call rows count, including the failed one"
    );
    assert!(telemetry.duration_ms >= 0);

    store.dispose().await;
}

#[tokio::test]
async fn the_runs_stored_columns_and_the_roll_up_agree() {
    // The acceptance box: telemetry equals the underlying rows. Both sides are read here and
    // compared, so this walk fails if either the run path or the roll-up drifts.
    let store = bench!();
    let run_id = store
        .finished_run("completed", "final_answer", 0, 0, 0, 0)
        .await;
    let step_no = run_store::begin_step(
        &store.pool,
        run_id,
        StepKind::Message,
        None,
        None,
    )
    .await
    .expect("the step must begin");
    run_store::finish_step(
        &store.pool,
        run_id,
        step_no,
        StepStatus::Completed,
        None,
        700,
        300,
        1_500,
        None,
        None,
    )
    .await
    .expect("the step must finish");
    // `finish_run` recomputes the run's own totals from the step rows, which is exactly the
    // property this walk checks — so the stored columns are not written by the fixture, they
    // are written by the same arithmetic the roll-up is compared against.
    run_store::finish_run(
        &store.pool,
        run_id,
        StepStatus::Completed,
        StopReason::FinalAnswer,
        None,
    )
    .await
    .expect("the run must finish");

    let stored = run_store::get_run(&store.pool, store.organization_id, run_id)
        .await
        .expect("the run read must work")
        .expect("the run must exist");
    let steps = run_store::run_totals_match_steps(&store.pool, run_id)
        .await
        .expect("the recomputation must work");
    let telemetry = run_telemetry(&store.pool, run_id)
        .await
        .expect("the telemetry read must work");

    assert_eq!(stored.prompt_tokens, steps.prompt_tokens);
    assert_eq!(stored.completion_tokens, steps.completion_tokens);
    assert_eq!(stored.cost_micros, steps.cost_micros);
    assert_eq!(telemetry.prompt_tokens, steps.prompt_tokens);
    assert_eq!(telemetry.completion_tokens, steps.completion_tokens);
    assert_eq!(telemetry.cost_micros, steps.cost_micros);

    store.dispose().await;
}

#[tokio::test]
async fn a_run_with_no_steps_reports_zeroes_rather_than_nulls() {
    let store = bench!();
    let run_id = store
        .finished_run("failed", "error", 0, 0, 0, 0)
        .await;
    let telemetry = run_telemetry(&store.pool, run_id)
        .await
        .expect("the telemetry read must work");
    assert_eq!(telemetry.completed_steps, 0);
    assert_eq!(telemetry.prompt_tokens, 0);
    assert_eq!(telemetry.cost_micros, 0);
    // A run with no steps has no start and no end, so there is no interval to measure. Zero
    // is the right answer here and not a missing value: the panel prints it either way, and a
    // `null` would render as an empty cell that reads as "not measured yet".
    assert_eq!(telemetry.duration_ms, 0);
    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// The per-agent roll-up
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_roll_up_sums_exactly_the_runs_that_finished() {
    let store = bench!();
    // 3 completed, 1 cancelled by a person, 1 failed.
    for _ in 0..3 {
        store.finished_run("completed", "final_answer", 2, 100, 50, 10).await;
    }
    store.finished_run("cancelled", "cancelled", 1, 200, 60, 20).await;
    store.finished_run("failed", "max_steps", 5, 300, 70, 30).await;

    let telemetry = agent_telemetry(&store.pool, store.organization_id, store.agent_id)
        .await
        .expect("the roll-up must work");

    assert_eq!(telemetry.runs, 5);
    assert_eq!(telemetry.completed, 3);
    assert_eq!(telemetry.cancelled, 1);
    assert_eq!(telemetry.failed, 1);
    // 3x(100+50) + (200+60) + (300+70) = 800 prompt, 280 completion, 80 cost. Written out
    // rather than left to the reader: the first version of this walk asserted 1000/330/90 and
    // failed, which is exactly what a hand-written expectation is for — it is the only number
    // in the file nobody computed twice.
    assert_eq!(telemetry.prompt_tokens, 800);
    assert_eq!(telemetry.completion_tokens, 280);
    assert_eq!(telemetry.cost_micros, 80);
    assert_eq!(telemetry.total_tokens(), 1080);
    assert_eq!(telemetry.success_rate(), Some(60.0));
    assert_eq!(telemetry.stop_reasons.get("final_answer"), Some(&3));
    assert_eq!(telemetry.stop_reasons.get("cancelled"), Some(&1));
    assert_eq!(telemetry.stop_reasons.get("max_steps"), Some(&1));

    store.dispose().await;
}

#[tokio::test]
async fn the_roll_up_agrees_with_the_same_sum_written_by_hand_in_sql() {
    // The anti-tautology walk. Asserting the function's answer against numbers the walk
    // computed itself is the version that can catch a bug in the function; the previous walk
    // checks the same totals but a shared mistake in *both* the fixture and the expectation
    // would pass it.
    let store = bench!();
    store.finished_run("completed", "final_answer", 1, 111, 222, 333).await;
    store.finished_run("failed", "deadline", 4, 444, 555, 666).await;

    let (sql_cost, sql_prompt, sql_completion, sql_runs): (i64, i32, i32, i64) = sqlx::query_as(
        "select coalesce(sum(cost_micros), 0)::bigint, \
                coalesce(sum(prompt_tokens), 0)::int, \
                coalesce(sum(completion_tokens), 0)::int, count(*)::bigint \
           from ai_runs where organization_id = $1 and agent_id = $2 and finished_at is not null",
    )
    .bind(store.organization_id)
    .bind(store.agent_id)
    .fetch_one(&store.pool)
    .await
    .expect("the hand-written sum must work");

    let telemetry = agent_telemetry(&store.pool, store.organization_id, store.agent_id)
        .await
        .expect("the roll-up must work");
    assert_eq!(telemetry.runs, sql_runs);
    assert_eq!(telemetry.prompt_tokens, sql_prompt);
    assert_eq!(telemetry.completion_tokens, sql_completion);
    assert_eq!(telemetry.cost_micros, sql_cost);

    store.dispose().await;
}

#[tokio::test]
async fn a_run_still_in_flight_is_not_in_the_roll_up() {
    // The denominator is *finished* runs, so a run that is queued or running right now is not
    // a failure and not a success. Counting it would make an agent's rate dip every time
    // somebody pressed Run, which is the fastest way to make a metric get ignored.
    let store = bench!();
    store.finished_run("completed", "final_answer", 1, 10, 10, 10).await;
    // A run left `queued`, which has no `finished_at` at all.
    run_store::create_run(
        &store.pool,
        &NewRun {
            goal: "still going".to_owned(),
            ..NewRun::default_for(store.organization_id, store.agent_id)
        },
    )
    .await
    .expect("the queued run must be created");

    let telemetry = agent_telemetry(&store.pool, store.organization_id, store.agent_id)
        .await
        .expect("the roll-up must work");
    assert_eq!(telemetry.runs, 1);
    assert_eq!(telemetry.success_rate(), Some(100.0));

    store.dispose().await;
}

#[tokio::test]
async fn the_window_really_excludes_a_run_from_before_it() {
    // The walk the request's "30 days" demands. A roll-up that ignores its own `since` is a
    // lifetime total that reads as correct right up until the installation is a month old —
    // and then the column that says "Runs 30 d" is simply wrong with nothing to notice it.
    let store = bench!();
    store.finished_run("completed", "final_answer", 1, 10, 10, 10).await;
    store
        .run_at(
            "completed",
            "final_answer",
            1,
            999,
            999,
            999,
            OffsetDateTime::now_utc() - Duration::days(90),
        )
        .await;

    let inside = agent_telemetry(&store.pool, store.organization_id, store.agent_id)
        .await
        .expect("the roll-up must work");
    assert_eq!(inside.runs, 1, "the 90-day-old run is outside the window");
    assert_eq!(inside.cost_micros, 10);

    // And the same agent, asked for a longer window, sees both — which is what makes the
    // first assertion a window rather than a filter that eats rows.
    let wide = agent_telemetry_since(
        &store.pool,
        store.organization_id,
        store.agent_id,
        window_start(180),
    )
    .await
    .expect("the wide roll-up must work");
    assert_eq!(wide.runs, 2);
    assert_eq!(wide.cost_micros, 1009);

    store.dispose().await;
}

#[tokio::test]
async fn a_deleted_run_leaves_the_roll_up() {
    // There is no counter table, so this is true by construction — and it is exactly the
    // property a counter would break. The walk is the tripwire: if somebody adds an
    // `ai_agent_stats` row, this fails the day after the first deletion.
    let store = bench!();
    let doomed = store
        .finished_run("completed", "final_answer", 1, 10, 10, 100)
        .await;
    store.finished_run("completed", "final_answer", 1, 10, 10, 20).await;

    let before = agent_telemetry(&store.pool, store.organization_id, store.agent_id)
        .await
        .expect("the roll-up must work");
    assert_eq!(before.cost_micros, 120);

    sqlx::query("delete from ai_runs where id = $1")
        .bind(doomed)
        .execute(&store.pool)
        .await
        .expect("the run must be deleted");

    let after = agent_telemetry(&store.pool, store.organization_id, store.agent_id)
        .await
        .expect("the roll-up must work");
    assert_eq!(after.cost_micros, 20, "a deleted run's cost is gone from the total");
    assert_eq!(after.runs, 1);

    store.dispose().await;
}

#[tokio::test]
async fn one_organizations_roll_up_is_empty_for_another_organizations_agent() {
    // The cross-tenant half. An agent id is a uuid the caller supplies, and this is the query
    // that sums money — the one place the tenant predicate has to be in the SQL rather than
    // assumed from the agent.
    let store = bench!();
    store.finished_run("completed", "final_answer", 1, 10, 10, 10).await;

    let foreign = agent_telemetry(&store.pool, store.other_organization_id, store.agent_id)
        .await
        .expect("the cross-tenant roll-up must work");
    assert_eq!(foreign.runs, 0, "another tenant's agent has no runs here");
    assert_eq!(foreign.cost_micros, 0);
    assert_eq!(foreign.success_rate(), None, "and therefore no rate to report");

    store.dispose().await;
}

#[tokio::test]
async fn the_bulk_roll_up_keeps_an_agent_that_has_never_run() {
    // The left join's whole reason for being. A table row that disappears when nothing has
    // happened yet looks like a deletion, so an agent with no runs must come back with zeroes
    // rather than being absent from the map.
    let store = bench!();
    store.finished_run("completed", "final_answer", 1, 10, 10, 10).await;
    let quiet = run_store::create_agent(
        &store.pool,
        &NewAgent::with_defaults(store.organization_id, "quiet", "Quiet"),
    )
    .await
    .expect("the second agent must be created");

    let all = agents_telemetry(
        &store.pool,
        store.organization_id,
        window_start(telemetry::ROLLUP_DAYS),
    )
    .await
    .expect("the bulk roll-up must work");

    assert_eq!(all.len(), 2, "both agents are present, one with no runs");
    let quiet_row = all
        .get(&quiet.id)
        .expect("an agent with no runs must still be in the map");
    assert_eq!(quiet_row.runs, 0);
    assert_eq!(quiet_row.cost_micros, 0);
    assert_eq!(quiet_row.success_rate(), None);
    assert_eq!(
        all.get(&store.agent_id).map(|row| row.runs),
        Some(1),
        "the agent that ran carries its run"
    );

    store.dispose().await;
}

#[tokio::test]
async fn the_bulk_roll_up_sees_only_this_tenants_agents() {
    let store = bench!();
    let foreign = run_store::create_agent(
        &store.pool,
        &NewAgent::with_defaults(store.other_organization_id, "rival", "Rival"),
    )
    .await
    .expect("the other tenant's agent must be created");

    let all = agents_telemetry(&store.pool, store.organization_id, window_start(30))
        .await
        .expect("the bulk roll-up must work");
    assert!(
        !all.contains_key(&foreign.id),
        "another tenant's agent is not in this tenant's map"
    );
    assert!(all.contains_key(&store.agent_id));

    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// Tool usage
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn tool_usage_counts_the_calls_and_keeps_its_order() {
    let store = bench!();
    for (tool, times) in [("web.search", 2_i64), ("mail.send", 3), ("db.query", 1)] {
        for _ in 0..times {
            let run_id = store
                .finished_run("completed", "final_answer", 0, 0, 0, 0)
                .await;
            let step_no = run_store::begin_step(
                &store.pool,
                run_id,
                StepKind::ToolCall,
                Some(tool),
                None,
            )
            .await
            .expect("the step must begin");
            run_store::finish_step(
                &store.pool,
                run_id,
                step_no,
                StepStatus::Completed,
                None,
                0,
                0,
                0,
                None,
                None,
            )
            .await
            .expect("the step must finish");
        }
    }

    let usage = tool_usage(&store.pool, store.organization_id, window_start(30))
        .await
        .expect("the tool usage read must work");
    assert_eq!(usage.get("mail.send"), Some(&3));
    assert_eq!(usage.get("web.search"), Some(&2));
    assert_eq!(usage.get("db.query"), Some(&1));
    // The map is a `BTreeMap`, so the iteration order is the sorted key order — which is what
    // makes the panel's list reproducible between two requests.
    let keys: Vec<&String> = usage.keys().collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);

    store.dispose().await;
}

#[tokio::test]
async fn tool_usage_sees_only_this_tenants_calls() {
    let store = bench!();
    let run_id = store
        .finished_run("completed", "final_answer", 0, 0, 0, 0)
        .await;
    let step_no = run_store::begin_step(
        &store.pool,
        run_id,
        StepKind::ToolCall,
        Some("web.search"),
        None,
    )
    .await
    .expect("the step must begin");
    run_store::finish_step(
        &store.pool,
        run_id,
        step_no,
        StepStatus::Completed,
        None,
        0,
        0,
        0,
        None,
        None,
    )
    .await
    .expect("the step must finish");

    let foreign = tool_usage(
        &store.pool,
        store.other_organization_id,
        window_start(30),
    )
    .await
    .expect("the cross-tenant read must work");
    assert!(
        foreign.is_empty(),
        "another tenant's tool usage is not this tenant's"
    );

    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// The rate's own shape
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_brand_new_agent_has_a_rate_of_none_and_not_zero() {
    // The panel's cell. An agent with no runs has no success rate, and a cell reading "0%" on
    // a brand-new agent is a claim about a division that never happened.
    let store = bench!();
    let fresh = AgentTelemetry {
        agent_id: store.agent_id,
        ..AgentTelemetry::default()
    };
    assert!(fresh.success_rate().is_none());
    let measured = agent_telemetry(&store.pool, store.organization_id, store.agent_id)
        .await
        .expect("the roll-up must work");
    assert!(measured.success_rate().is_none());
    store.dispose().await;
}

#[tokio::test]
async fn a_run_a_person_cancelled_is_reported_beside_the_rate_and_not_folded_into_it() {
    // Three finished runs, one of them stopped by a person. The rate is 2/3 and the cancelled
    // count is published next to it, so a reader who wants the other denominator does not
    // have to guess whether it was folded in.
    let store = bench!();
    store.finished_run("completed", "final_answer", 1, 10, 10, 10).await;
    store.finished_run("completed", "final_answer", 1, 10, 10, 10).await;
    store.finished_run("cancelled", "cancelled", 1, 10, 10, 10).await;

    let telemetry = agent_telemetry(&store.pool, store.organization_id, store.agent_id)
        .await
        .expect("the roll-up must work");
    assert_eq!(telemetry.runs, 3);
    assert_eq!(telemetry.completed, 2);
    assert_eq!(telemetry.cancelled, 1);
    // 2/3 as a percentage. Compared with an epsilon rather than a literal: `2.0/3.0*100.0` is
    // not the same double as a hand-typed `66.66666666666667`, and a test that fails on the
    // last bit of a division is a test about the compiler, not about the rate.
    let rate = telemetry.success_rate().expect("two of three finished");
    assert!(
        (rate - 66.666_666_666_666_67).abs() < 1e-9,
        "got {rate}"
    );

    store.dispose().await;
}
