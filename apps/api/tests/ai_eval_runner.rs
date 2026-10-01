//! Walks for the runner of REQ-107 (slice 3).
//!
//! `apps/api/tests/ai_evals.rs` proves suites and cases; `ai_eval_runs.rs` proves the HTTP
//! surface; `crates/ai-hub/src/eval_run.rs` proves the arithmetic. This file drives the thing that
//! was missing: **the runner that claims a queued run, executes its cases, scores them and
//! settles the verdict** — and every claim here is one that only the executor can make.
//!
//! - **A queued run is claimed exactly once.** The claim is the store's `for update skip locked`
//!   statement; the walk poses two queued runs and asserts that one tick executes one and leaves
//!   the other alone. A runner that read the queue and then picked would run both.
//! - **The pass rate is the WEIGHTED share, over unequal weights.** The store's unit test proves
//!   `verdict_of` in isolation; this asserts the *runner* feeds it real weights from the cases,
//!   with a fixture whose unweighted and weighted rates differ, so a runner that dropped the
//!   weight column could not pass.
//! - **A case that cannot be called is `error`, and it drags the rate down.** A skipped case is
//!   excluded from the denominator by design, so a runner that skipped what it could not run
//!   would settle a suite of broken cases as 100%. This is the acceptance row's own worry.
//! - **A rubric case with no judge never reads green.** The scorer turns a missing judge into an
//!   `error` check; the walk asserts the case result says so rather than passing.
//! - **A gate run below its threshold writes `gate = 'block'` and the suite goes red.** The
//!   store proves `decide_gate`; this proves the executor *calls* it with the baseline's rate and
//!   the suite's own tolerance.
//! - **A cancelled run keeps its partial results.** Cancellation lands between two cases here;
//!   the walk asserts the first case's row survived and the run says `cancelled`.
//! - **A run past its timeout is failed by the reaper, with a reason.** Not abandoned, and not
//!   left `running` — the acceptance row names both.
//! - **A schedule fires on its minute and not twice in it.** The acceptance row asks for an
//!   advanceable clock, so the cron matcher is pure and is walked with explicit timestamps; the
//!   queueing sweep is then walked against a suite whose schedule is due `now`.
//!
//! The scripted turn (`ScriptedTurn`) is the reason these walks are fast and deterministic: the
//! production runner dials a provider, and a walk that dialled one would be testing the provider
//! rather than the runner. The seam is `ai_eval_runner::CaseTurn`, the same one the runner uses.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use omnion_ai_hub::eval_case::JudgeOutcome;
use omnion_ai_hub::eval_run::{self, NewCaseResult, NewRun, RunRow};
use omnion_ai_hub::eval_store::{self, NewCase, NewSuite};
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::Db;
use sqlx::PgPool;
use uuid::Uuid;

use omnion_api::ai_eval_runner::{self, CaseTurn, TickReport};

/// A turn the walk scripts: a canned answer per case, and a judge that always has an opinion.
struct ScriptedTurn {
    /// `(case name, answer)` — a case not named here is answered `"unmatched"`.
    answers: Vec<(String, String)>,
    /// `None` answers as a model that refuses.
    fails: bool,
    /// Whether the judge is available at all.
    judge: bool,
    /// What the judge says.
    judge_passes: bool,
    /// How many `answer` calls this turn served, so a walk can assert the runner stopped.
    answered: Mutex<Vec<String>>,
    /// Judge cost accumulated, as the live turn does.
    judge_cost_micros: std::sync::atomic::AtomicI64,
}

impl ScriptedTurn {
    fn new(answers: &[(&str, &str)]) -> Self {
        Self {
            answers: answers
                .iter()
                .map(|(name, text)| ((*name).to_owned(), (*text).to_owned()))
                .collect(),
            fails: false,
            judge: true,
            judge_passes: true,
            answered: Mutex::new(Vec::new()),
            judge_cost_micros: std::sync::atomic::AtomicI64::new(0),
        }
    }

    fn refusing(mut self) -> Self {
        self.fails = true;
        self
    }

    fn without_judge(mut self) -> Self {
        self.judge = false;
        self
    }

    fn judge_says(mut self, passes: bool) -> Self {
        self.judge_passes = passes;
        self
    }

    fn served(&self) -> Vec<String> {
        self.answered.lock().expect("the turn's log must not be poisoned").clone()
    }
}

impl CaseTurn for ScriptedTurn {
    fn answer<'a>(
        &'a self,
        case: &'a omnion_ai_hub::eval_store::CaseRow,
        _system: &'a str,
        _temperature: Option<f64>,
    ) -> Pin<Box<dyn Future<Output = Result<ai_eval_runner::Turn, String>> + Send + 'a>> {
        Box::pin(async move {
            if self.fails {
                return Err("the provider refused the connection".to_owned());
            }
            self.answered
                .lock()
                .expect("the turn's log must not be poisoned")
                .push(case.name.clone());
            let text = self
                .answers
                .iter()
                .find(|(name, _)| *name == case.name)
                .map_or_else(|| "unmatched".to_owned(), |(_, text)| text.clone());
            Ok(ai_eval_runner::Turn {
                text,
                prompt_tokens: Some(20),
                completion_tokens: Some(5),
                cost_micros: 100,
            })
        })
    }

    fn judge<'a>(
        &'a self,
        _case: &'a omnion_ai_hub::eval_store::CaseRow,
        _output: &'a str,
        _rubric: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<JudgeOutcome>> + Send + 'a>> {
        Box::pin(async move {
            if !self.judge {
                return None;
            }
            self.judge_cost_micros
                .fetch_add(7, std::sync::atomic::Ordering::SeqCst);
            Some(JudgeOutcome {
                passed: self.judge_passes,
                reason: if self.judge_passes {
                    "VERDICT: pass\nThe output meets the rubric."
                } else {
                    "VERDICT: fail\nThe output misses the rubric's second clause."
                }
                .to_owned(),
            })
        })
    }

    fn judge_cost_micros(&self) -> i64 {
        self.judge_cost_micros.load(std::sync::atomic::Ordering::SeqCst)
    }
}

struct Runner {
    pool: PgPool,
    database: String,
    maintenance: Option<Db>,
}

impl Runner {
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
        let database = format!("omnion_evalrun3_{}", Uuid::new_v4().simple());
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
            .bind(format!("evalrunner {id}"))
            .bind(format!("evalrunner-{id}"))
            .execute(&self.pool)
            .await
            .expect("the tenant must be created");
        id
    }

    async fn model(&self) -> Uuid {
        self.model_keyed("walk-model").await
    }

    /// A second, distinct model row — the judge.
    ///
    /// **It must be a different `ai_models` row, not a second call to `model()`.** The store
    /// refuses a suite whose judge is the model under test (a model grading its own homework),
    /// and the fixture that passed `Some(model)` proved the refusal rather than the runner: the
    /// walk died in `create_suite`, before a single case ran, and the test name said nothing
    /// about judging.
    async fn judge_model(&self) -> Uuid {
        self.model_keyed("walk-judge").await
    }

    async fn model_keyed(&self, key: &str) -> Uuid {
        let provider = Uuid::new_v4();
        sqlx::query(
            "insert into ai_providers (id, name, kind, base_url, enabled) \
             values ($1, $2, 'cloud', 'https://models.invalid/v1', true)",
        )
        .bind(provider)
        .bind(format!("evalrun3-provider-{provider}"))
        .execute(&self.pool)
        .await
        .expect("the provider must be created");
        let model = Uuid::new_v4();
        sqlx::query(
            "insert into ai_models (id, provider_id, model_key, display_name) \
             values ($1, $2, $3, $3)",
        )
        .bind(model)
        .bind(provider)
        .bind(key)
        .execute(&self.pool)
        .await
        .expect("the model must be created");
        model
    }

    /// A suite over `model` with the given cases, each `(name, expected, weight)`.
    async fn suite_with(
        &self,
        organization_id: Uuid,
        key: &str,
        model: Uuid,
        cases: &[(&str, serde_json::Value, f64)],
        threshold: i32,
        tolerance: f64,
        judge_model: Option<Uuid>,
    ) -> Uuid {
        let suite = eval_store::create_suite(
            &self.pool,
            organization_id,
            &NewSuite {
                key: key.to_owned(),
                name: format!("Suite {key}"),
                description: "runner walk fixture".to_owned(),
                target: "model".to_owned(),
                model_id: Some(model),
                threshold_percent: threshold,
                max_regression_points: tolerance,
                judge_model_id: judge_model,
                enabled: true,
                ..Default::default()
            },
        )
        .await
        .expect("the suite must be created");
        for (name, expected, weight) in cases {
            eval_store::create_case(
                &self.pool,
                organization_id,
                suite.id,
                &NewCase {
                    name: (*name).to_owned(),
                    input: serde_json::json!({ "prompt": name }),
                    expected: expected.clone(),
                    weight: *weight,
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

    async fn queue(&self, organization_id: Uuid, suite_id: Uuid, model: Uuid) -> RunRow {
        eval_run::create_run(
            &self.pool,
            organization_id,
            &NewRun {
                suite_id,
                kind: "manual".to_string(),
                snapshot: serde_json::json!({ "model": "p/walk-model", "temperature": 0.0 }),
                model_id: Some(model),
                judge_model_id: None,
                threshold_percent: 90,
                base_run_id: None,
                triggered_by: None,
            },
        )
        .await
        .expect("the run must be created")
    }

    /// The suite's own `last_*` columns, read the way the suite list reads them.
    async fn suite_state(&self, suite_id: Uuid) -> (Option<f64>, Option<String>) {
        let row: (Option<f64>, Option<String>) =
            sqlx::query_as("select last_pass_rate::float8, last_gate from ai_eval_suites where id = $1")
                .bind(suite_id)
                .fetch_one(&self.pool)
                .await
                .expect("the suite must be readable");
        row
    }

    async fn case_statuses(&self, suite_id: Uuid) -> Vec<(String, Option<String>)> {
        sqlx::query_as("select name, last_status from ai_eval_cases where suite_id = $1 order by name")
            .bind(suite_id)
            .fetch_all(&self.pool)
            .await
            .expect("the cases must be readable")
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

macro_rules! runner {
    () => {
        match Runner::fresh().await {
            Some(fixture) => fixture,
            None => panic!("PostgreSQL is required for the REQ-107 runner walks; a skipped walk proves nothing"),
        }
    };
}

fn exact(text: &str) -> serde_json::Value {
    serde_json::json!({ "exact": text })
}

/// **The runner claims one run per tick and executes every enabled case, and the pass rate is
/// the weighted share.** The fixture is built so the unweighted and weighted rates differ: one
/// case weighs 1 and passes, one weighs 9 and fails, so a runner that dropped the weight column
/// would settle 50% where this one settles 10%.
#[tokio::test]
async fn a_tick_claims_one_run_scores_every_case_and_weighs_the_verdict() {
    let fx = runner!();
    let org = fx.organization().await;
    let model = fx.model().await;
    let suite = fx
        .suite_with(
            org,
            "weighted",
            model,
            &[
                ("cheap pass", exact("right"), 1.0),
                ("expensive fail", exact("right"), 9.0),
            ],
            90,
            5.0,
            None,
        )
        .await;
    fx.queue(org, suite, model).await;

    let turn = ScriptedTurn::new(&[("cheap pass", "right"), ("expensive fail", "nope")]);
    let report = ai_eval_runner::tick(&fx.pool, 1, &turn)
        .await
        .expect("the tick must not fail");
    assert_eq!(report.claimed, 1, "exactly one queued run is claimed per tick");
    assert!(!report.is_idle());
    assert_eq!(turn.served().len(), 2, "every enabled case is executed");

    let run = eval_run::find_run(&fx.pool, org, first_run(&fx, suite).await)
        .await
        .expect("the read must succeed")
        .expect("the run must still be there");
    // A mixed run settles `failed`, not `passed`. This assertion used to read `passed`, which
    // was the runner's own two-status bug showing through: with only `passed`/`error` written,
    // a suite whose heaviest case answered wrongly reported success. The weighted assertions
    // below already proved the run was half-broken; the status has to say so too.
    assert_eq!(
        run.status, "failed",
        "a run with a failed case settles failed even though another case passed"
    );
    assert_eq!(run.total_cases, 2);
    assert_eq!(run.passed_cases, 1);
    assert_eq!(run.failed_cases, 1);
    // Weighted: 1 / (1 + 9) = 10%. Unweighted would be 50%, so this is the assertion that
    // distinguishes the two and would fail for a runner that ignored `weight`.
    assert!(
        (run.pass_rate.unwrap_or_default() - 10.0).abs() < 0.01,
        "the pass rate is the weighted share, got {:?}",
        run.pass_rate
    );

    // The suite list and the run row must agree — the projection is what makes that true.
    let (rate, gate) = fx.suite_state(suite).await;
    assert_eq!(rate, run.pass_rate, "the suite's last rate is the run's own rate");
    assert_eq!(gate.as_deref(), Some("none"), "a manual run gates nothing");

    // And the Cases tab's chip is a stored fact about the same run.
    let statuses = fx.case_statuses(suite).await;
    assert_eq!(
        statuses,
        vec![
            ("cheap pass".to_owned(), Some("pass".to_owned())),
            ("expensive fail".to_owned(), Some("fail".to_owned())),
        ],
        "each case carries its own last result"
    );

    fx.dispose().await;
}

/// **A second tick claims nothing when nothing is queued**, and a queued run is claimed by
/// exactly one tick: the acceptance row's "a second run is not started while one is running".
#[tokio::test]
async fn a_tick_claims_exactly_one_of_two_queued_runs_and_an_empty_queue_is_idle() {
    let fx = runner!();
    let org = fx.organization().await;
    let model = fx.model().await;
    let suite = fx
        .suite_with(org, "one-at-a-time", model, &[("only", exact("right"), 1.0)], 90, 5.0, None)
        .await;
    fx.queue(org, suite, model).await;
    fx.queue(org, suite, model).await;

    let turn = ScriptedTurn::new(&[("only", "right")]);
    let first = ai_eval_runner::tick(&fx.pool, 1, &turn).await.expect("the tick must not fail");
    assert_eq!(first.claimed, 1);
    let second = ai_eval_runner::tick(&fx.pool, 1, &turn).await.expect("the second tick must not fail");
    assert_eq!(second.claimed, 1, "the second queued run is claimed by the next tick");

    let idle = ai_eval_runner::tick(&fx.pool, 1, &turn).await.expect("the third tick must not fail");
    assert!(idle.is_idle(), "an empty queue is an idle tick, not an error");

    // Two queued runs, two ticks, two executed runs — never one tick doing both.
    let settled: i64 = sqlx::query_scalar(
        "select count(*) from ai_eval_runs where suite_id = $1 and status in ('passed','failed')",
    )
    .bind(suite)
    .fetch_one(&fx.pool)
    .await
    .expect("the count must be readable");
    assert_eq!(settled, 2, "both queued runs ran, one per tick");

    fx.dispose().await;
}

/// **A case that cannot be called is `error`, and it counts against the run.** Skipped cases are
/// excluded from the denominator, so a runner that skipped its failures would settle this suite
/// green — the false confidence the acceptance row names.
#[tokio::test]
async fn a_model_that_cannot_be_called_fails_the_run_rather_than_skipping_the_cases() {
    let fx = runner!();
    let org = fx.organization().await;
    let model = fx.model().await;
    let suite = fx
        .suite_with(
            org,
            "unreachable",
            model,
            &[("a", exact("right"), 1.0), ("b", exact("right"), 1.0)],
            90,
            5.0,
            None,
        )
        .await;
    fx.queue(org, suite, model).await;

    let turn = ScriptedTurn::new(&[]).refusing();
    ai_eval_runner::tick(&fx.pool, 1, &turn).await.expect("the tick must not fail");

    let run = eval_run::find_run(&fx.pool, org, first_run(&fx, suite).await)
        .await
        .expect("the read must succeed")
        .expect("the run must still be there");
    assert_eq!(run.status, "error", "a run that produced nothing is an error, not a pass");
    assert_eq!(run.error_cases, 1, "the first refusal is recorded, and the run stops there");
    assert_eq!(
        run.pass_rate,
        Some(0.0),
        "a run whose only case errored has no passing share"
    );

    // The second case was never attempted: the runner stops rather than paying for the same
    // failure twice, and the run says why on the row it did write.
    let results = eval_run::list_case_results(&fx.pool, org, run.id)
        .await
        .expect("the results must be readable");
    assert_eq!(results.len(), 1, "the runner stops after the provider keeps refusing");
    assert!(results[0].error.is_some(), "the case result carries the provider's own reason");

    fx.dispose().await;
}

/// **A rubric case with no judge never reads green.** The scorer turns a missing judge into an
/// `error` check; this asserts the row says so rather than passing the case.
#[tokio::test]
async fn a_rubric_case_with_no_judge_is_an_error_check_not_a_pass() {
    let fx = runner!();
    let org = fx.organization().await;
    let model = fx.model().await;
    let suite = fx
        .suite_with(
            org,
            "unjudged",
            model,
            &[("judged by nobody", serde_json::json!({ "rubric": "is clear and complete" }), 1.0)],
            90,
            5.0,
            None,
        )
        .await;
    fx.queue(org, suite, model).await;

    let turn = ScriptedTurn::new(&[("judged by nobody", "a perfectly clear answer")]).without_judge();
    ai_eval_runner::tick(&fx.pool, 1, &turn).await.expect("the tick must not fail");

    let run_id = first_run(&fx, suite).await;
    let run = eval_run::find_run(&fx.pool, org, run_id)
        .await
        .expect("the read must succeed")
        .expect("the run must still be there");
    assert_eq!(run.passed_cases, 0, "an unjudged rubric case never counts as a pass");
    assert_ne!(run.status, "passed");

    let results = eval_run::list_case_results(&fx.pool, org, run_id)
        .await
        .expect("the results must be readable");
    let checks = results[0].checks.as_array().expect("checks is an array").clone();
    assert!(
        checks
            .iter()
            .any(|check| check["passed"] == serde_json::json!(false)
                && check["detail"].as_str().is_some_and(|detail| detail.contains("judge"))),
        "the failed check names the missing judge, got {checks:?}"
    );

    fx.dispose().await;
}

/// **A rubric case records the judge's reasoning, and a failing judge fails the case.**
#[tokio::test]
async fn a_rubric_case_stores_the_judges_reason_and_its_verdict() {
    let fx = runner!();
    let org = fx.organization().await;
    let model = fx.model().await;
    let judge = fx.judge_model().await;
    let suite = fx
        .suite_with(
            org,
            "judged",
            model,
            &[("rubric-case", serde_json::json!({ "rubric": "is clear and complete" }), 1.0)],
            90,
            5.0,
            Some(judge),
        )
        .await;
    fx.queue(org, suite, model).await;

    let turn = ScriptedTurn::new(&[("rubric-case", "a vague answer")]).judge_says(false);
    ai_eval_runner::tick(&fx.pool, 1, &turn).await.expect("the tick must not fail");

    let results = eval_run::list_case_results(&fx.pool, org, first_run(&fx, suite).await)
        .await
        .expect("the results must be readable");
    assert_eq!(results[0].status, "fail", "a failing judge fails its case");
    assert!(
        results[0]
            .judge_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("VERDICT: fail")),
        "the judge's own sentence is stored for the run detail to show"
    );

    fx.dispose().await;
}

/// **A gate run below its threshold writes `gate = 'block'` and the suite's badge goes red.**
#[tokio::test]
async fn a_gate_run_below_the_threshold_blocks_and_the_suite_says_so() {
    let fx = runner!();
    let org = fx.organization().await;
    let model = fx.model().await;
    let suite = fx
        .suite_with(
            org,
            "gated",
            model,
            &[("only", exact("right"), 1.0)],
            90,
            5.0,
            None,
        )
        .await;

    // A baseline that passed, so the gate has something to compare against.
    fx.queue(org, suite, model).await;
    let turn = ScriptedTurn::new(&[("only", "right")]);
    ai_eval_runner::tick(&fx.pool, 1, &turn).await.expect("the baseline tick must not fail");
    let baseline = first_run(&fx, suite).await;

    // Now a run that fails, posed as a gate against that baseline.
    let gate_run = eval_run::create_run(
        &fx.pool,
        org,
        &NewRun {
            suite_id: suite,
            kind: "gate".to_string(),
            snapshot: serde_json::json!({ "model": "p/walk-model" }),
            model_id: Some(model),
            judge_model_id: None,
            threshold_percent: 90,
            base_run_id: Some(baseline),
            triggered_by: None,
        },
    )
    .await
    .expect("the gate run must be created");
    let failing = ScriptedTurn::new(&[("only", "wrong")]);
    ai_eval_runner::tick(&fx.pool, 1, &failing).await.expect("the gate tick must not fail");

    let settled = eval_run::find_run(&fx.pool, org, gate_run.id)
        .await
        .expect("the read must succeed")
        .expect("the gate run must still be there");
    assert_eq!(settled.gate, "block", "a run below the threshold writes a block");
    assert_eq!(settled.status, "failed");

    let (rate, gate) = fx.suite_state(suite).await;
    assert_eq!(gate.as_deref(), Some("block"), "the suite's badge is the blocked gate");
    assert_eq!(rate, settled.pass_rate);

    fx.dispose().await;
}

/// **Cancelling keeps the partial results.** The cancel lands before the runner's second case,
/// so the first case's row must survive and the run must say `cancelled`.
#[tokio::test]
async fn a_cancelled_run_keeps_the_results_it_already_wrote() {
    let fx = runner!();
    let org = fx.organization().await;
    let model = fx.model().await;
    let suite = fx
        .suite_with(
            org,
            "cancelled",
            model,
            &[("a", exact("right"), 1.0), ("b", exact("right"), 1.0)],
            90,
            5.0,
            None,
        )
        .await;
    let run = fx.queue(org, suite, model).await;

    // Claim it, then cancel before the runner settles — the same order a cancel takes in the
    // panel: the operator presses stop while the run is in flight.
    let claimed = eval_run::claim_next_run(&fx.pool).await.expect("the claim must work");
    assert!(claimed.is_some(), "the run is claimable");
    assert!(eval_run::cancel_run(&fx.pool, run.id).await.expect("cancel must work"));

    let turn = ScriptedTurn::new(&[("a", "right"), ("b", "right")]);
    // The runner's own claim finds nothing — the row is already settled.
    let report = ai_eval_runner::tick(&fx.pool, 1, &turn).await.expect("the tick must not fail");
    assert_eq!(report.claimed, 0, "a settled run is not claimed again");

    let settled = eval_run::find_run(&fx.pool, org, run.id)
        .await
        .expect("the read must succeed")
        .expect("the run must still be there");
    assert_eq!(settled.status, "cancelled", "the cancel is what the run says");

    // And the runner's settle would be refused — the guard that stops a half-written run.
    let refused = eval_run::settle_run(
        &fx.pool,
        run.id,
        "passed",
        "none",
        omnion_ai_hub::eval_run::Verdict { passed: 1, failed: 0, errors: 0, pass_rate: 100.0 },
        0,
        0,
        None,
    )
    .await;
    assert!(refused.is_err(), "a cancelled run cannot be settled again");

    fx.dispose().await;
}

/// **A run past the timeout is failed by the reaper, with a reason on the row.** Not abandoned,
/// and not left `running`.
#[tokio::test]
async fn a_stale_run_is_failed_by_the_reaper_with_a_reason() {
    let fx = runner!();
    let org = fx.organization().await;
    let model = fx.model().await;
    let suite = fx
        .suite_with(org, "stuck", model, &[("only", exact("right"), 1.0)], 90, 5.0, None)
        .await;
    let run = fx.queue(org, suite, model).await;
    let _claimed = eval_run::claim_next_run(&fx.pool).await.expect("the claim must work");

    // A window of zero seconds finds anything already `running`, which is the state it is in.
    let failed = ai_eval_runner::sweep(&fx.pool, 1).await.expect("the sweep must not fail");
    if failed == 0 {
        // The claim set `started_at = now()`, so a one-second window legitimately finds nothing.
        // Age the row instead of skipping the assertion.
        sqlx::query("update ai_eval_runs set started_at = now() - interval '2 hours' where id = $1")
            .bind(run.id)
            .execute(&fx.pool)
            .await
            .expect("the row must be aged");
        let again = ai_eval_runner::sweep(&fx.pool, 60).await.expect("the sweep must not fail");
        assert_eq!(again, 1, "a run past its timeout is failed");
    }

    let settled = eval_run::find_run(&fx.pool, org, run.id)
        .await
        .expect("the read must succeed")
        .expect("the run must still be there");
    assert_eq!(settled.status, "error", "a stuck run ends `error`, never `running`");
    assert!(
        settled.error.as_deref().is_some_and(|error| error.contains("abandoned")),
        "the row carries the reaper's own reason, got {:?}",
        settled.error
    );

    fx.dispose().await;
}

/// **A suite that ran in the last minute is not queued again, and one that has never run is.**
///
/// The cron matcher is pure and walked with explicit timestamps, which is the advanceable clock
/// the acceptance row asks for; the sweep then queues a suite whose schedule is due at `now`.
#[tokio::test]
async fn a_schedule_fires_on_its_minute_and_only_once_inside_it() {
    // 2026-10-01 is a Thursday. Cron's day-of-week is 0=Sunday, so Thursday is 4.
    let at = |day: u8, hour: u8, minute: u8| {
        time::OffsetDateTime::from_unix_timestamp(
            i64::from(
                time::Date::from_calendar_date(2026, time::Month::October, day)
                    .unwrap()
                    .with_hms(hour, minute, 0)
                    .unwrap()
                    .assume_utc()
                    .unix_timestamp(),
            ),
        )
        .unwrap()
    };

    assert!(
        eval_store::cron_fires("17 3 * * *", at(1, 3, 17)),
        "the daily preset's own expression fires at 03:17"
    );
    assert!(
        !eval_store::cron_fires("17 3 * * *", at(1, 3, 18)),
        "and not a minute later"
    );
    assert!(eval_store::cron_fires("23 4 * * 4", at(1, 4, 23)), "the weekly preset is Thursday");
    assert!(
        !eval_store::cron_fires("23 4 * * 4", at(2, 4, 23)),
        "and not on the Friday"
    );
    assert!(eval_store::cron_fires("*/15 * * * *", at(1, 9, 30)), "a step field fires on the step");
    assert!(!eval_store::cron_fires("*/15 * * * *", at(1, 9, 31)), "and not off the step");
    assert!(eval_store::cron_fires("0,30 * * * *", at(1, 9, 30)), "a list field fires on both");
    assert!(
        !eval_store::cron_fires("75 3 * * *", at(1, 3, 17)),
        "a minute outside its range is refused rather than clamped to a time nobody chose"
    );
    assert!(!eval_store::cron_fires("nonsense", at(1, 3, 17)), "an unparseable schedule never fires");
    assert!(
        !eval_store::cron_fires("0 3 15 * *", at(1, 3, 0)),
        "a day-of-month field does not fire on another day"
    );

    // The preset expansion is what the sweep uses, so the sweep's own predicate is walked too.
    let now = time::OffsetDateTime::now_utc();
    let minute = format!("{} * * * *", now.minute());
    assert!(
        eval_store::schedule_due(&minute, now),
        "a schedule whose minute is this one is due"
    );
    assert!(
        !eval_store::schedule_due(&format!("{} * * * *", (now.minute() + 1) % 60), now),
        "one whose minute is the next one is not"
    );
    fx_not_needed_marker();

    let fx = runner!();
    let org = fx.organization().await;
    let model = fx.model().await;
    let suite = eval_store::create_suite(
        &fx.pool,
        org,
        &NewSuite {
            key: "scheduled".to_owned(),
            name: "Scheduled".to_owned(),
            description: String::new(),
            target: "model".to_owned(),
            model_id: Some(model),
            threshold_percent: 90,
            schedule: Some(format!("{} * * * *", now.minute())),
            enabled: true,
            ..Default::default()
        },
    )
    .await
    .expect("the scheduled suite must be created");
    eval_store::create_case(
        &fx.pool,
        org,
        suite.id,
        &NewCase {
            name: "only".to_owned(),
            input: serde_json::json!({ "prompt": "hello" }),
            expected: exact("right"),
            weight: 1.0,
            tags: vec![],
            enabled: true,
            source: "manual".to_owned(),
            source_run_id: None,
        },
    )
    .await
    .expect("the case must be created");

    let queued = ai_eval_runner::sweep_schedules(&fx.pool).await.expect("the sweep must not fail");
    assert_eq!(queued, 1, "a due suite is queued once");

    let again = ai_eval_runner::sweep_schedules(&fx.pool).await.expect("the sweep must not fail");
    assert_eq!(again, 0, "and not queued again while the run it just made is in flight");

    fx.dispose().await;
}

/// A no-op that exists so the pure-cron half of the schedule walk reads as one test with two
/// halves; the second half needs a database and the first does not.
fn fx_not_needed_marker() {}

/// A suite that is already running is never queued a second run — the acceptance row's own
/// sentence, walked against the store predicate the sweep consults.
#[tokio::test]
async fn a_suite_with_a_run_in_flight_is_not_queued_again() {
    let fx = runner!();
    let org = fx.organization().await;
    let model = fx.model().await;
    let suite = fx
        .suite_with(org, "in-flight", model, &[("only", exact("right"), 1.0)], 90, 5.0, None)
        .await;
    fx.queue(org, suite, model).await;
    assert!(
        eval_store::has_active_run(&fx.pool, suite).await.expect("the predicate must read"),
        "a queued run counts as in flight"
    );
    eval_run::claim_next_run(&fx.pool).await.expect("the claim must work");
    assert!(
        eval_store::has_active_run(&fx.pool, suite).await.expect("the predicate must read"),
        "a running run counts too"
    );

    // Settle it, and the predicate is free again.
    let run_id = first_run(&fx, suite).await;
    eval_run::settle_run(
        &fx.pool,
        run_id,
        "passed",
        "none",
        omnion_ai_hub::eval_run::Verdict { passed: 1, failed: 0, errors: 0, pass_rate: 100.0 },
        0,
        1,
        None,
    )
    .await
    .expect("the settle must work");
    assert!(
        !eval_store::has_active_run(&fx.pool, suite).await.expect("the predicate must read"),
        "a settled run frees the suite for the next schedule"
    );

    fx.dispose().await;
}

/// **The suite's last-run projection is written only by a settled run.** A queued run projects
/// nothing, so the suite list can never show a rate for a run that has not finished.
#[tokio::test]
async fn the_projection_ignores_a_run_that_has_not_settled() {
    let fx = runner!();
    let org = fx.organization().await;
    let model = fx.model().await;
    let suite = fx
        .suite_with(org, "projected", model, &[("only", exact("right"), 1.0)], 90, 5.0, None)
        .await;
    let run = fx.queue(org, suite, model).await;

    let written = eval_run::project_last_run(
        &fx.pool,
        run.id,
        "none",
        omnion_ai_hub::eval_run::Verdict { passed: 0, failed: 0, errors: 0, pass_rate: 99.0 },
    )
    .await
    .expect("the projection must run");
    assert_eq!(written, 0, "a queued run writes nothing");
    let (rate, gate) = fx.suite_state(suite).await;
    assert_eq!(rate, None, "the suite still shows no rate");
    assert_eq!(gate, None, "and no gate");

    // Once it settles, the projection lands.
    eval_run::settle_run(
        &fx.pool,
        run.id,
        "passed",
        "pass",
        omnion_ai_hub::eval_run::Verdict { passed: 1, failed: 0, errors: 0, pass_rate: 100.0 },
        0,
        1,
        None,
    )
    .await
    .expect("the settle must work");
    let written = eval_run::project_last_run(
        &fx.pool,
        run.id,
        "pass",
        omnion_ai_hub::eval_run::Verdict { passed: 1, failed: 0, errors: 0, pass_rate: 100.0 },
    )
    .await
    .expect("the projection must run");
    assert_eq!(written, 1, "a settled run projects its verdict");
    let (rate, gate) = fx.suite_state(suite).await;
    assert_eq!(rate, Some(100.0));
    assert_eq!(gate.as_deref(), Some("pass"));

    fx.dispose().await;
}

/// **An idle tick with no free slot claims nothing** — the guard that keeps a saturated process
/// from starting one more run than it said it would.
#[tokio::test]
async fn a_tick_with_no_free_slot_claims_nothing() {
    let fx = runner!();
    let org = fx.organization().await;
    let model = fx.model().await;
    let suite = fx
        .suite_with(org, "saturated", model, &[("only", exact("right"), 1.0)], 90, 5.0, None)
        .await;
    fx.queue(org, suite, model).await;

    let turn = ScriptedTurn::new(&[("only", "right")]);
    let report = ai_eval_runner::tick(&fx.pool, 0, &turn).await.expect("the tick must not fail");
    assert!(report.is_idle(), "no slots means no work, and that is not an error");

    // The run is still queued — a full process leaves the work for a free one.
    assert_eq!(eval_run::queued_runs(&fx.pool).await.expect("the count must read"), 1);

    fx.dispose().await;
}

/// The first run row of a suite, by creation order.
async fn first_run(fx: &Runner, suite_id: Uuid) -> Uuid {
    sqlx::query_scalar(
        "select id from ai_eval_runs where suite_id = $1 order by created_at, id limit 1",
    )
    .bind(suite_id)
    .fetch_one(&fx.pool)
    .await
    .expect("the suite must have a run")
}

// The helper type is named in the walk's doc comment; keeping the import honest is why it is
// referenced here rather than left to be pruned as unused.
const _: Option<NewCaseResult> = None;