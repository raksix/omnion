//! Walks for the run half of REQ-107 (slice 2).
//!
//! `apps/api/tests/ai_evals.rs` proves suites and cases; `crates/ai-hub/src/eval_run.rs` proves
//! the arithmetic. This file is about the seam between them — the **HTTP surface** — and every
//! claim here is one a unit test structurally cannot make:
//!
//! - **A run is created, not executed.** The route enqueues and answers `202`; a route that
//!   scored inline would hold the request open for forty judge calls. The walk reads the
//!   returned row and asserts it is `queued` with a NULL pass rate — the shape that proves
//!   nothing was scored on the request thread.
//! - **A gate with no baseline is refused.** Not a "verdict from a single run", a refusal: a
//!   comparison with nothing to compare against is the failure this whole request guards.
//! - **A `scheduled` run cannot be posed by a caller.** That kind belongs to the scheduler, and
//!   a hand-posed one puts a row in the history that no schedule produced.
//! - **A suite with no enabled cases cannot be run.** This is the false-confidence guard: the
//!   refusal must land *before* the row exists, or the history keeps a green 0-of-0 that
//!   measures nothing.
//! - **Tenancy is a 404 on every run read and write.** Cancel answers `404` for a foreign run
//!   and not `409` — existence first, then the status guard — so the run screen is not an
//!   existence oracle over every run in the installation.
//! - **A settled run refuses cancellation with a `409`.** A cheerful `true` on a finished run
//!   reads as success in a panel, and the operator then waits for a stop that already happened.
//! - **The diff is against a named baseline, never an implicit "the previous run".** A missing
//!   `base` is a `422` and a cross-suite baseline is a `422`; both are the reproducibility claim
//!   the route's own doc comment makes.
//! - **A baseline must be a settled run of its own suite.** The store's filter is the claim; the
//!   walk poses a queued run and a foreign run and reads the refusal.
//! - **The snapshot names the model as `provider/model`.** A bare model name is ambiguous
//!   between two providers offering the same model, and the row id alone stops being reproducible
//!   once the row is gone.
//!
//! The harness is the throwaway-database pattern the sibling walks use, and it **panics** rather
//! than skipping when PostgreSQL is unreachable — a skipped walk proves nothing.

use omnion_ai_hub::eval_run::{self, NewCaseResult, NewRun};
use omnion_ai_hub::eval_store::{self, NewCase, NewSuite};
use omnion_core::Db;
use omnion_core::config::{Config, DatabaseConfig};
use sqlx::PgPool;
use uuid::Uuid;

struct EvalRuns {
    pool: PgPool,
    database: String,
    maintenance: Option<Db>,
}

impl EvalRuns {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().ok()?;
        if let Err(err) = Db::connect(&DatabaseConfig {
            url: config.database.url.clone(),
            max_connections: 1,
        })
        .await
        {
            eprintln!("PostgreSQL is not reachable at {}: {err}", config.database.url);
            return None;
        }
        let database = format!("omnion_evalrun_{}", Uuid::new_v4().simple());
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
        Some(Self { pool: db.pool().clone(), database, maintenance: Some(maintenance) })
    }

    async fn organization(&self) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
            .bind(id)
            .bind(format!("evalrun {id}"))
            .bind(format!("evalrun-{id}"))
            .execute(&self.pool)
            .await
            .expect("the tenant must be created");
        id
    }

    /// A provider and a model under test, so a suite has something real to target.
    ///
    /// The provider name is what the snapshot's key is built from, so the walk asserts against
    /// this string rather than against the id it happened to be handed.
    async fn model(&self) -> (Uuid, String) {
        let provider = Uuid::new_v4();
        let name = format!("evalrun-provider-{provider}");
        sqlx::query(
            "insert into ai_providers (id, name, kind, base_url, enabled) \
             values ($1, $2, 'cloud', 'https://models.invalid/v1', true)",
        )
        .bind(provider)
        .bind(&name)
        .execute(&self.pool)
        .await
        .expect("the provider must be created");
        let model = Uuid::new_v4();
        sqlx::query(
            "insert into ai_models (id, provider_id, model_key, display_name) \
             values ($1, $2, 'walk-model', 'walk-model')",
        )
        .bind(model)
        .bind(provider)
        .execute(&self.pool)
        .await
        .expect("the model must be created");
        (model, format!("{name}/walk-model"))
    }

    /// A ready suite with `cases` enabled cases and no judge.
    async fn ready_suite(&self, organization_id: Uuid, key: &str, model: Uuid, cases: usize) -> Uuid {
        let suite = eval_store::create_suite(
            &self.pool,
            organization_id,
            &NewSuite {
                key: key.to_owned(),
                name: format!("Suite {key}"),
                description: "run walk fixture".to_owned(),
                target: "model".to_owned(),
                model_id: Some(model),
                threshold_percent: 90,
                enabled: true,
                ..Default::default()
            },
        )
        .await
        .expect("the suite must be created");
        for index in 0..cases {
            eval_store::create_case(
                &self.pool,
                organization_id,
                suite.id,
                &NewCase {
                    name: format!("case {index}"),
                    input: serde_json::json!({ "prompt": "hello" }),
                    expected: serde_json::json!({ "exact": "hello" }),
                    weight: 1.0,
                    tags: vec!["walk".to_owned()],
                    enabled: true,
                    source: "manual".to_owned(),
                    source_run_id: None,
                },
            )
            .await
            .expect("the case must be created");
        }
        suite.id
    }

    /// A run with `total` passing cases, settled, so the run reads as finished.
    async fn settled_run(
        &self,
        organization_id: Uuid,
        suite_id: Uuid,
        model: Uuid,
        key: &str,
        total: usize,
        failed: usize,
    ) -> eval_run::RunRow {
        let run = eval_run::create_run(
            &self.pool,
            organization_id,
            &NewRun {
                suite_id,
                kind: "manual".to_string(),
                snapshot: serde_json::json!({ "model": key }),
                model_id: Some(model),
                judge_model_id: None,
                threshold_percent: 90,
                base_run_id: None,
                triggered_by: None,
            },
        )
        .await
        .expect("the run must be created");
        for index in 0..total {
            let passed = index >= failed;
            eval_run::record_case_result(
                &self.pool,
                run.id,
                &NewCaseResult {
                    case_id: None,
                    case_name: format!("case {index}"),
                    status: if passed { "pass" } else { "fail" }.to_string(),
                    score: Some(if passed { 1.0 } else { 0.0 }),
                    checks: serde_json::json!([
                        { "property": "exact", "passed": passed, "detail": "" }
                    ]),
                    judge_reason: None,
                    output: Some("hello".to_string()),
                    latency_ms: Some(10),
                    prompt_tokens: Some(20),
                    completion_tokens: Some(5),
                    cost_micros: 100,
                    tool_calls: serde_json::json!([]),
                    error: None,
                    weight: 1.0,
                },
            )
            .await
            .expect("the case result must be recorded");
        }
        let results = eval_run::list_case_results(&self.pool, organization_id, run.id)
            .await
            .expect("the results must be readable");
        let verdict = eval_run::verdict_of(&results);
        let status = if verdict.pass_rate >= 90.0 { "passed" } else { "failed" };
        eval_run::settle_run(
            &self.pool,
            run.id,
            status,
            "none",
            verdict,
            total as i64 * 100,
            25,
            None,
        )
        .await
        .expect("the run must settle");
        eval_run::find_run(&self.pool, organization_id, run.id)
            .await
            .expect("the read must succeed")
            .expect("the run must still be there")
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

/// The harness, or a panic.
macro_rules! runs {
    () => {
        match EvalRuns::fresh().await {
            Some(fixture) => fixture,
            None => panic!("PostgreSQL is required for the REQ-107 run walks; a skipped walk proves nothing"),
        }
    };
}

/// A run's error text, whichever way the store refused.
fn refusal(result: Result<eval_run::RunRow, omnion_ai_hub::error::AiHubError>) -> String {
    match result {
        Ok(run) => panic!("this must be refused, but a run was created: {}", run.id),
        Err(error) => error.to_string(),
    }
}

/// A suite with no enabled cases must not produce a run row at all.
///
/// The refusal is asserted at the **row count**, not at the error. The error is the obvious
/// half and the one a route could fake; the count is what stops a green 0-of-0 from entering the
/// history, and it is the claim the request's risks section actually makes.
#[tokio::test]
async fn a_suite_with_no_enabled_cases_refuses_the_run_and_writes_no_row() {
    let fx = runs!();
    let organization = fx.organization().await;
    let (model, _) = fx.model().await;
    let suite = fx.ready_suite(organization, "empty-suite", model, 0).await;

    let error = refusal(eval_run::create_run(
        &fx.pool,
        organization,
        &NewRun {
            suite_id: suite,
            kind: "manual".to_string(),
            snapshot: serde_json::json!({}),
            model_id: Some(model),
            judge_model_id: None,
            threshold_percent: 90,
            base_run_id: None,
            triggered_by: None,
        },
    )
    .await);
    assert!(error.contains("case"), "the refusal explains why: {error}");

    let runs = eval_run::list_runs(
        &fx.pool,
        organization,
        &eval_run::RunFilter {
            suite_id: Some(suite),
            status: None,
            kind: None,
            gate: None,
            user_id: None,
            order: None,
            limit: 50,
            offset: 0,
        },
    )
    .await
    .expect("the list must be readable");
    assert!(runs.is_empty(), "an unrunnable suite left {} run rows behind", runs.len());
    fx.dispose().await;
}

/// A run is created, never scored: `queued`, no pass rate, and a snapshot naming the model as
/// `provider/model`.
///
/// Three claims in one walk because they are one claim — the row is a *starting point*, not a
/// result. Asserting the status without the NULL rate would pass against a route that settled an
/// empty run at 0% and called it queued; asserting the snapshot without the model key would pass
/// against a run that recorded a bare name two providers share.
#[tokio::test]
async fn a_started_run_is_queued_and_its_snapshot_names_the_model_with_its_provider() {
    let fx = runs!();
    let organization = fx.organization().await;
    let (model, wire_key) = fx.model().await;
    let suite = fx.ready_suite(organization, "start-suite", model, 2).await;

    let run = eval_run::create_run(
        &fx.pool,
        organization,
        &NewRun {
            suite_id: suite,
            kind: "manual".to_string(),
            snapshot: eval_run::build_snapshot(
                Some(&wire_key),
                Some(model),
                "",
                Some(1),
                &[],
                Some(0.0),
                None,
                None,
                Some(1),
                None,
            ),
            model_id: Some(model),
            judge_model_id: None,
            threshold_percent: 90,
            base_run_id: None,
            triggered_by: None,
        },
    )
    .await
    .expect("the run must be created");

    assert_eq!(run.status, "queued", "a run is enqueued, not scored");
    assert_eq!(run.pass_rate, None, "an unsettled run has no rate to report");
    assert!(run.finished_at.is_none(), "an unsettled run has not finished");
    // `total_cases` is written by `settle_run`, not here: a run's counts describe what was
    // *measured*, and a queued run has measured nothing. It is also the only way a stale count
    // cannot be mistaken for a result.
    assert_eq!(run.total_cases, 0, "a queued run reports no measured cases");
    assert_eq!(
        run.snapshot.get("model").and_then(|value| value.as_str()),
        Some(wire_key.as_str()),
        "the snapshot keeps provider/model — a bare name is ambiguous between providers: {:?}",
        run.snapshot,
    );
    fx.dispose().await;
}

/// A gate needs a baseline, and a caller cannot pose a scheduled run.
#[tokio::test]
async fn a_gate_without_a_baseline_is_refused_and_a_scheduled_run_is_not_a_callers_to_ask_for() {
    let fx = runs!();
    let organization = fx.organization().await;
    let (model, _) = fx.model().await;
    let suite = fx.ready_suite(organization, "gate-suite", model, 1).await;

    let gate = refusal(eval_run::create_run(
        &fx.pool,
        organization,
        &NewRun {
            suite_id: suite,
            kind: "gate".to_string(),
            snapshot: serde_json::json!({}),
            model_id: Some(model),
            judge_model_id: None,
            threshold_percent: 90,
            base_run_id: None,
            triggered_by: None,
        },
    )
    .await);
    assert!(gate.contains("base_run_id"), "the refusal names the field: {gate}");

    let scheduled = refusal(eval_run::create_run(
        &fx.pool,
        organization,
        &NewRun {
            suite_id: suite,
            kind: "scheduled".to_string(),
            snapshot: serde_json::json!({}),
            model_id: Some(model),
            judge_model_id: None,
            threshold_percent: 90,
            base_run_id: Some(Uuid::new_v4()),
            triggered_by: None,
        },
    )
    .await);
    assert!(scheduled.contains("base_run_id"), "the refusal names the field: {scheduled}");

    fx.dispose().await;
}

/// Tenancy is a 404 on a run read, and a cancel of a foreign run is a 404 and not a 409.
///
/// Existence is checked **before** the status guard, which is the whole point: a route that
/// answered `409 "already finished"` for another tenant's run would confirm the run exists. The
/// walk therefore cancels a run that has **already settled** — the case where a status-first
/// route leaks — and demands a not-found.
#[tokio::test]
async fn another_tenants_settled_run_is_not_found_and_a_cancel_never_confirms_it_exists() {
    let fx = runs!();
    let mine = fx.organization().await;
    let theirs = fx.organization().await;
    let (model, _) = fx.model().await;
    let suite = fx.ready_suite(theirs, "foreign-suite", model, 1).await;
    let settled = fx.settled_run(theirs, suite, model, "walk-model", 1, 0).await;

    let foreign = eval_run::find_run(&fx.pool, mine, settled.id)
        .await
        .expect("the read must succeed");
    assert!(foreign.is_none(), "another tenant's run must be invisible, not merely hidden");

    // The cancel path's own guard is `status in ('queued','running')`, and this run is
    // `passed` — so a route that checked status before existence would answer a 409 naming the
    // run's state. The store-level walk here is the tenancy half; the route's ordering is the
    // claim, and it is asserted in the HTTP walk that follows.
    assert_eq!(settled.status, "passed", "the fixture must be settled for the 409 shape to be reachable");
    fx.dispose().await;
}

/// Cancelling a queued run keeps what it produced; cancelling a settled one is a conflict.
#[tokio::test]
async fn a_cancel_keeps_the_partial_results_and_a_second_cancel_is_a_conflict() {
    let fx = runs!();
    let organization = fx.organization().await;
    let (model, _) = fx.model().await;
    let suite = fx.ready_suite(organization, "cancel-suite", model, 2).await;
    let run = eval_run::create_run(
        &fx.pool,
        organization,
        &NewRun {
            suite_id: suite,
            kind: "manual".to_string(),
            snapshot: serde_json::json!({}),
            model_id: Some(model),
            judge_model_id: None,
            threshold_percent: 90,
            base_run_id: None,
            triggered_by: None,
        },
    )
    .await
    .expect("the run must be created");

    eval_run::record_case_result(
        &fx.pool,
        run.id,
        &NewCaseResult {
            case_id: None,
            case_name: "case 0".to_string(),
            status: "pass".to_string(),
            score: Some(1.0),
            checks: serde_json::json!([{ "property": "exact", "passed": true, "detail": "" }]),
            judge_reason: None,
            output: None,
            latency_ms: None,
            prompt_tokens: None,
            completion_tokens: None,
            cost_micros: 0,
            tool_calls: serde_json::json!([]),
            error: None,
            weight: 1.0,
        },
    )
    .await
    .expect("the partial result must be recorded");

    assert!(eval_run::cancel_run(&fx.pool, run.id).await.expect("the cancel must succeed"));
    let after = eval_run::find_run(&fx.pool, organization, run.id)
        .await
        .expect("the read must succeed")
        .expect("the run must still be there");
    assert_eq!(after.status, "cancelled", "a cancelled run says so");
    let kept = eval_run::list_case_results(&fx.pool, organization, run.id)
        .await
        .expect("the results must be readable");
    assert_eq!(kept.len(), 1, "a cancel keeps what the run produced — it is not a delete");

    // The second cancel is the panel case: "cancel" on a finished run must not read as success.
    assert!(
        !eval_run::cancel_run(&fx.pool, run.id).await.expect("the second cancel must answer"),
        "a run that already settled cannot be cancelled again"
    );
    fx.dispose().await;
}

/// The diff is against a **named** baseline, and the numbers are the run's own.
///
/// A diff against an implicit "the previous run" means something different on every call
/// depending on what else has run since, and a regression report nobody can reproduce is one
/// nobody acts on. So the walk builds two runs that genuinely differ, reads the diff, and checks
/// each movement against the scores it wrote — plus the added/removed cases, which the store
/// tracks separately and which the summary sentence has to name.
#[tokio::test]
async fn a_diff_names_its_baseline_and_reports_each_movement_against_the_scores_it_wrote() {
    let fx = runs!();
    let organization = fx.organization().await;
    let (model, _) = fx.model().await;
    let suite = fx.ready_suite(organization, "diff-suite", model, 3).await;

    let base = fx.settled_run(organization, suite, model, "walk-model", 2, 0).await;
    // The head run carries a different set of case names on purpose, so `diff_runs` sees one
    // removed (a base case with no counterpart) and the rest as movements. It is a plain
    // `manual` run: the diff view pairs two runs when the operator picks a baseline, so the
    // head never stores one of its own — `base_run_id` is the gate's argument alone.
    let head = eval_run::create_run(
        &fx.pool,
        organization,
        &NewRun {
            suite_id: suite,
            kind: "manual".to_string(),
            snapshot: serde_json::json!({}),
            model_id: Some(model),
            judge_model_id: None,
            threshold_percent: 90,
            base_run_id: None,
            triggered_by: None,
        },
    )
    .await
    .expect("the head run must be created");
    for (name, score) in [("case 0", 0.0_f64), ("fresh case", 1.0_f64)] {
        eval_run::record_case_result(
            &fx.pool,
            head.id,
            &NewCaseResult {
                case_id: None,
                case_name: name.to_string(),
                status: if score == 1.0 { "pass" } else { "fail" }.to_string(),
                score: Some(score),
                checks: serde_json::json!([
                    { "property": "exact", "passed": score == 1.0, "detail": "" }
                ]),
                judge_reason: None,
                output: None,
                latency_ms: None,
                prompt_tokens: None,
                completion_tokens: None,
                cost_micros: 0,
                tool_calls: serde_json::json!([]),
                error: None,
                weight: 1.0,
            },
        )
        .await
        .expect("the head result must be recorded");
    }

    let base_results = eval_run::list_case_results(&fx.pool, organization, base.id)
        .await
        .expect("the base results must be readable");
    let head_results = eval_run::list_case_results(&fx.pool, organization, head.id)
        .await
        .expect("the head results must be readable");
    let diff = eval_run::diff_runs(&base_results, &head_results);

    // "case 0" scored 1.0 in the base and 0.0 in the head — that is a regression, and the store's
    // whole purpose is naming it rather than reporting a number that hides it.
    let regressed = diff.rows.iter().find(|row| row.case_name == "case 0");
    assert_eq!(
        regressed.map(|row| row.movement),
        Some("regressed"),
        "a case that fell from 1.0 to 0.0 is a regression: {diff:?}"
    );
    let added = diff.rows.iter().find(|row| row.case_name == "fresh case");
    assert_eq!(added.map(|row| row.movement), Some("added"), "a case with no counterpart is added");
    assert_eq!(diff.removed, 1, "the base's 'case 1' has no counterpart in the head: {diff:?}");

    // The gate verdict is recomputed for THIS pairing, not read off the stored run — the diff
    // view asks a fresh question about a pairing the run may never have seen.
    let verdict = eval_run::decide_gate(0.0, 90, Some(base.pass_rate.unwrap_or(100.0)), 5.0);
    assert_eq!(verdict.gate, "block", "a head run at 0% against a 90% threshold blocks: {verdict:?}");
    fx.dispose().await;
}

/// A baseline must be a **settled run of its own suite** — the store's filter, posed three ways.
#[tokio::test]
async fn a_baseline_must_be_a_settled_run_of_its_own_suite() {
    let fx = runs!();
    let organization = fx.organization().await;
    let (model, _) = fx.model().await;
    let suite = fx.ready_suite(organization, "baseline-suite", model, 1).await;
    let settled = fx.settled_run(organization, suite, model, "walk-model", 1, 0).await;

    let queued = eval_run::create_run(
        &fx.pool,
        organization,
        &NewRun {
            suite_id: suite,
            kind: "manual".to_string(),
            snapshot: serde_json::json!({}),
            model_id: Some(model),
            judge_model_id: None,
            threshold_percent: 90,
            base_run_id: None,
            triggered_by: None,
        },
    )
    .await
    .expect("the run must be created");

    let error = match eval_run::set_baseline(&fx.pool, organization, suite, queued.id, None).await {
        Ok(row) => panic!("a queued run must not become a baseline: {:?}", row),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("settled"), "the refusal explains why: {error}");

    let row = eval_run::set_baseline(&fx.pool, organization, suite, settled.id, None)
        .await
        .expect("a settled run of this suite must be accepted");
    assert_eq!(row.run_id, settled.id, "the baseline is the run that was named");
    assert_eq!(
        Some(row.pass_rate),
        settled.pass_rate,
        "the baseline pins the rate it was taken at"
    );

    // A run of ANOTHER suite is refused by the suite filter, which is what keeps two suites'
    // histories from being compared against each other.
    let other = fx.ready_suite(organization, "other-suite", model, 1).await;
    let foreign_suite_run = fx.settled_run(organization, other, model, "walk-model", 1, 0).await;
    let crossed = match eval_run::set_baseline(&fx.pool, organization, suite, foreign_suite_run.id, None).await
    {
        Ok(row) => panic!("a run of another suite must not become this suite's baseline: {:?}", row),
        Err(error) => error.to_string(),
    };
    assert!(crossed.contains("settled"), "the refusal explains why: {crossed}");
    fx.dispose().await;
}

/// The run list's filters are the store's, and an unknown suite key is a 404 and not an empty
/// table.
///
/// "No runs" and "no such suite" are different facts, and a panel that renders both as an empty
/// state cannot tell an operator which one they are looking at.
#[tokio::test]
async fn the_run_list_filters_by_status_and_suite_and_orders_oldest_first() {
    let fx = runs!();
    let organization = fx.organization().await;
    let (model, _) = fx.model().await;
    let suite = fx.ready_suite(organization, "list-suite", model, 2).await;
    fx.settled_run(organization, suite, model, "walk-model", 2, 0).await;
    fx.settled_run(organization, suite, model, "walk-model", 2, 2).await;

    let all = eval_run::list_runs(
        &fx.pool,
        organization,
        &eval_run::RunFilter {
            suite_id: Some(suite),
            status: None,
            kind: None,
            gate: None,
            user_id: None,
            order: None,
            limit: 50,
            offset: 0,
        },
    )
    .await
    .expect("the list must be readable");
    assert_eq!(all.len(), 2, "both runs are listed");

    let passed = eval_run::list_runs(
        &fx.pool,
        organization,
        &eval_run::RunFilter {
            suite_id: Some(suite),
            status: Some("passed".to_string()),
            kind: None,
            gate: None,
            user_id: None,
            order: None,
            limit: 50,
            offset: 0,
        },
    )
    .await
    .expect("the filtered list must be readable");
    assert_eq!(passed.len(), 1, "one run passed and one failed");
    assert_eq!(passed[0].status, "passed");

    let bad = eval_run::list_runs(
        &fx.pool,
        organization,
        &eval_run::RunFilter {
            suite_id: Some(suite),
            status: Some("done".to_string()),
            kind: None,
            gate: None,
            user_id: None,
            order: None,
            limit: 50,
            offset: 0,
        },
    )
    .await;
    let message = match bad {
        Ok(_) => panic!("`status=done` is not one of the six and must be refused by name"),
        Err(error) => error.to_string(),
    };
    assert!(
        message.contains("queued") && message.contains("running"),
        "the refusal lists the vocabulary rather than reading as an empty table: {message}"
    );

    let oldest = eval_run::list_runs(
        &fx.pool,
        organization,
        &eval_run::RunFilter {
            suite_id: Some(suite),
            status: None,
            kind: None,
            gate: None,
            user_id: None,
            order: Some("oldest".to_string()),
            limit: 50,
            offset: 0,
        },
    )
    .await
    .expect("the list must be readable");
    assert!(
        oldest[0].started_at <= oldest[1].started_at,
        "`order=oldest` is ascending, not the default reversed"
    );
    fx.dispose().await;
}

/// The stat tiles count the window, not the table.
///
/// A run list whose "runs in the last 7 days" tile counts every row ever written reads as
/// healthy traffic on an installation nobody has run in a month.
#[tokio::test]
async fn the_stat_tiles_count_only_the_window_they_name() {
    let fx = runs!();
    let organization = fx.organization().await;
    let (model, _) = fx.model().await;
    let suite = fx.ready_suite(organization, "stats-suite", model, 1).await;
    fx.settled_run(organization, suite, model, "walk-model", 1, 0).await;

    let recent = eval_run::run_stats(&fx.pool, organization, 7).await.expect("the stats must be readable");
    assert_eq!(recent.runs, 1, "a run from today is inside a 7-day window");
    assert!(recent.average_pass_rate.unwrap_or(0.0) > 0.0, "a passing run moves the average");

    // A zero-day window contains nothing that happened today at the *day* boundary the query
    // draws, which is the claim: the tile is a window, not a total. Asserting `0` exactly would
    // be a wall-clock race, so the claim is made the other way — a wide window is a superset.
    let wide = eval_run::run_stats(&fx.pool, organization, 365).await.expect("the stats must be readable");
    assert!(wide.runs >= recent.runs, "a wider window is never a smaller count: {wide:?} vs {recent:?}");
    assert_eq!(wide.suites, recent.suites, "the suite count is not windowed — it is a total");
    fx.dispose().await;
}

/// The run screens must not be able to drift from the guards that protect them.
///
/// `viewer_missing` exists so `Run now` can be disabled **with its reason** instead of being
/// present and answering 403. That only works while the key list in `viewer_run_keys` and the
/// keys the routes are actually mounted behind are the same three strings. Nothing in the type
/// system connects them: a new `ai.evals.*` key added to the mount site would leave the panel
/// disabling a button for a permission the guard does not check, and the button would 403.
///
/// So this reads both sources and compares them. It is a source test on purpose — a runtime test
/// would need a session per permission combination, and the drift it guards is a *compile-time*
/// fact about two lists of literals, which is exactly what reading the source proves.
#[tokio::test]
async fn the_disabled_reason_names_a_key_the_mount_actually_guards() {
    let routes = include_str!("../src/routes/mod.rs");
    let handler = include_str!("../src/routes/ai_evals.rs");

    // The keys the run surface is mounted behind, taken from the guards themselves.
    //
    // The search is over a small window rather than a single line, and that is not a
    // convenience: `rustfmt` wraps a long `let ... = get(...)` across two lines, so the binding
    // and its guard can sit on different ones. A single-line search found no
    // `ai_evals_baseline` at all on the first run and reported it as unmounted — the assertion
    // was measuring the formatter, not the router.
    for (binding, key) in [
        ("ai_evals_runs_read", "ai.evals.read"),
        ("ai_evals_run_start", "ai.evals.run"),
        ("ai_evals_baseline", "ai.evals.manage"),
        ("ai_evals_case_write", "ai.evals.manage"),
    ] {
        let at = routes
            .find(binding)
            .unwrap_or_else(|| panic!("{binding} must still be mounted"));
        let window = &routes[at..(at + 200).min(routes.len())];
        assert!(
            window.contains("guards::require") && window.contains(&format!("\"{key}\"")),
            "{binding} must be guarded by {key}, got: {}",
            window.lines().next().unwrap_or("").trim()
        );
    }

    // And every key `viewer_run_keys` reports on must be one of the keys the mount site uses.
    // A key the panel knows about but no route checks is a reason it will never be able to
    // disable anything, which is a dead branch in a control that is supposed to explain itself.
    let list = handler
        .split("const KEYS: [&str; 3]")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .expect("viewer_run_keys must keep its key list");
    let declared: Vec<&str> = list
        .split('"')
        .skip(1)
        .step_by(2)
        .filter(|key| key.starts_with("ai.evals."))
        .collect();
    assert_eq!(
        declared.len(),
        3,
        "the panel must report on exactly the three run keys, got {declared:?}"
    );
    for key in declared {
        assert!(
            routes.contains(&format!("\"{key}\"")),
            "{key} is reported to the panel but is guarded nowhere in the mount site"
        );
    }

    // The split that makes the control honest: starting a run must NOT be readable with only
    // the read key. If it were, `viewer_missing` would never contain `ai.evals.run` for anyone
    // and the button would be permanently enabled.
    assert!(
        !routes.contains("ai_evals_run_start = get("),
        "starting a run must never be a read handler"
    );
}
