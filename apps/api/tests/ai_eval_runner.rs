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

use omnion_api::ai_eval_runner::{self, CaseTurn};

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

    /// Make a model the installation default, the way an operator marks one.
    ///
    /// **This is the counter-case the pinned-model walk needs.** A suite that pins its own model
    /// must be graded by that model even when a different one is the installation default — so
    /// the walk has to *have* a real default that is not the pin. Without this the runner's bug
    /// (resolving `default_model` instead of the snapshot) and a correct runner would return the
    /// same string, and the assertion would be green in both worlds.
    async fn make_default(&self, model: Uuid) {
        sqlx::query("update ai_models set is_default = (id = $1)")
            .bind(model)
            .execute(&self.pool)
            .await
            .expect("the default flag must be settable");
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

    /// The `provider/model` string a `model_keyed(key)` row is registered under.
    ///
    /// **Read from the row, not composed from the fixture's inputs.** `model_keyed` generates a
    /// random provider name (`evalrun3-provider-<uuid>`), so a walk that typed the string out
    /// would compare the runner's answer against a key that names no provider — and the
    /// assertion would fail for the wrong reason, or (worse) pass because the fallback path
    /// happened to agree with a typo.
    async fn key_of(&self, model: Uuid) -> String {
        sqlx::query_scalar(
            "select p.name || '/' || m.model_key from ai_models m \
             join ai_providers p on p.id = m.provider_id where m.id = $1",
        )
        .bind(model)
        .fetch_one(&self.pool)
        .await
        .expect("the model must be registered")
    }

    /// A model row served by a real socket, under the OpenAI-compatible protocol.
    ///
    /// Separate from `model_keyed` because the base URL is the whole point here: `model_keyed`
    /// registers `https://models.invalid/v1`, which is deliberately undialable so a walk cannot
    /// accidentally reach a vendor. This one points at a stub the walk itself started.
    async fn model_at(&self, base_url: String, key: &str) -> Uuid {
        let provider = Uuid::new_v4();
        sqlx::query(
            "insert into ai_providers (id, name, kind, base_url, enabled, protocol) \
             values ($1, $2, 'cloud', $3, true, 'openai_compatible')",
        )
        .bind(provider)
        .bind(format!("evalrun3-provider-{provider}"))
        .bind(base_url)
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

    /// The provider serving a model — the scope a usage row is read by.
    ///
    /// `ai_provider_usage` carries no organization column, so a walk cannot scope its reads by
    /// tenant the way every other assertion in this file does. It scopes by provider instead, and
    /// this is the read that makes that sound: the fixture's provider names carry a fresh uuid,
    /// so the scope is this walk's rows and no one else's.
    async fn provider_of(&self, model: Uuid) -> Uuid {
        sqlx::query_scalar("select provider_id from ai_models where id = $1")
            .bind(model)
            .fetch_one(&self.pool)
            .await
            .expect("the model must be registered")
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
        self.queue_with(
            organization_id,
            suite_id,
            model,
            serde_json::json!({ "model": "p/walk-model", "temperature": 0.0 }),
        )
        .await
    }

    /// Queue a run with an **explicit** snapshot, for a walk about what the runner reads.
    ///
    /// `queue` carries a placeholder `p/walk-model` whose prefix names no registered provider,
    /// so it cannot accidentally satisfy a reader looking for a real key. A walk that is about
    /// the snapshot's model must therefore write the real one — and `provider_name_of` is what
    /// the fixture generates for `model_keyed("walk-model")`, so the string cannot drift away
    /// from the row the fixture actually created.
    async fn queue_with(
        &self,
        organization_id: Uuid,
        suite_id: Uuid,
        model: Uuid,
        snapshot: serde_json::Value,
    ) -> RunRow {
        eval_run::create_run(
            &self.pool,
            organization_id,
            &NewRun {
                suite_id,
                kind: "manual".to_string(),
                snapshot,
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

    /// Point one case's `input.prompt` at a real question.
    ///
    /// Written as an UPDATE rather than folded into `suite_with` because a case's prompt is the
    /// **question**, and a fixture that had to supply it per case at creation time would put the
    /// provider's answer-selection in the same call as the expectation — two variables in one
    /// line, which is how a walk ends up asserting the stub rather than the runner.
    ///
    /// `case_messages` pushes the prompt unconditionally, so an empty one is not a blank question:
    /// `validate_request` refuses it and the case settles `error` before a provider is reached.
    async fn input_prompt(&self, organization_id: Uuid, suite_id: Uuid, name: &str, prompt: &str) {
        sqlx::query(
            "update ai_eval_cases set input = jsonb_build_object('prompt', $4::text) \
             where suite_id = $1 and organization_id = $2 and name = $3",
        )
        .bind(suite_id)
        .bind(organization_id)
        .bind(name)
        .bind(prompt)
        .execute(&self.pool)
        .await
        .expect("the case prompt must be settable");
    }

    /// The suite's own `last_*` columns, read the way the suite list reads them.
    async fn suite_state(&self, suite_id: Uuid) -> (Option<f64>, Option<String>) {
        let row: (Option<f64>, Option<String>) = sqlx::query_as(
            "select last_pass_rate::float8, last_gate from ai_eval_suites where id = $1",
        )
        .bind(suite_id)
        .fetch_one(&self.pool)
        .await
        .expect("the suite must be readable");
        row
    }

    /// Every `ai.eval.*` payload the bus recorded for one tenant, under one event name.
    ///
    /// **Read the ROW, never the return value.** The acceptance rows are about the events a
    /// subscriber receives, and `announce()` returns nothing: a runner that skipped the emit
    /// and one that emitted a different name, a different tenant or a payload missing the
    /// regressed case names would both leave `tick()` returning the same `TickReport`. The
    /// tenant predicate is here for the same reason the emit reads its tenant off the run —
    /// an event published under another organization's id is the failure this catches.
    async fn eval_events(&self, organization_id: Uuid, name: &str) -> Vec<serde_json::Value> {
        sqlx::query_scalar(
            "select payload from events where organization_id = $1 and name = $2 order by created_at, id",
        )
        .bind(organization_id)
        .bind(name)
        .fetch_all(&self.pool)
        .await
        .expect("the events must read")
    }

    /// The set of `ai.eval.*` event names this tenant has, so a walk can prove an event was
    /// NOT emitted as well as proving one was.
    async fn eval_event_names(&self, organization_id: Uuid) -> Vec<String> {
        sqlx::query_scalar(
            "select distinct name from events where organization_id = $1 and name like 'ai.eval.%' \
             order by name",
        )
        .bind(organization_id)
        .fetch_all(&self.pool)
        .await
        .expect("the events must read")
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

/// The most recently created run id for a suite — the gate's own row, not the baseline's.
///
/// `order by created_at desc, id desc` matches the runner's own "latest" ordering elsewhere; a
/// tie on `created_at` inside one test transaction is broken by the id rather than left to the
/// planner, because "whichever row the index returned first" is not a run identity.
async fn latest_run(fx: &Runner, suite: Uuid) -> Uuid {
    sqlx::query_scalar(
        "select id from ai_eval_runs where suite_id = $1 order by created_at desc, id desc limit 1",
    )
    .bind(suite)
    .fetch_one(&fx.pool)
    .await
    .expect("the suite must have a run")
}

/// **A blocked gate is ANNOUNCED, and the announcement names the run it is about.**
///
/// The row assertions above are about `ai_eval_runs` and `ai_eval_suites`; this row is about
/// `events`, which is what a webhook, a workflow trigger or an e-mail rule subscribes to. The
/// runner calls `announce()` after settling and nothing read it back, so a runner that stopped
/// emitting the promotion signal — or emitted `gate.passed` for a blocked run — would have left
/// every other walk green.
///
/// Three claims, and the middle one is the one that would be easy to fake:
/// - exactly one `ai.eval.gate.blocked` row exists for this tenant;
/// - its payload names the gate run, the suite and the rate the run actually settled with;
/// - `gate.passed` was NOT emitted for it, since a subscriber acting on both would promote the
///   very release the gate refused.
#[tokio::test]
async fn a_blocked_gate_is_announced_with_the_run_it_is_about() {
    let fx = runner!();
    let org = fx.organization().await;
    let model = fx.model().await;
    let suite = fx
        .suite_with(org, "announced", model, &[("only", exact("right"), 1.0)], 90, 5.0, None)
        .await;

    // A passing run first, so the gate has a baseline and the pre-gate events are not the
    // empty set — an assertion against `[]` proves nothing about which name was written.
    fx.queue(org, suite, model).await;
    ai_eval_runner::tick(&fx.pool, 1, &ScriptedTurn::new(&[("only", "right")]))
        .await
        .expect("the baseline tick must not fail");

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
            base_run_id: Some(first_run(&fx, suite).await),
            triggered_by: None,
        },
    )
    .await
    .expect("the gate run must be created");
    ai_eval_runner::tick(&fx.pool, 1, &ScriptedTurn::new(&[("only", "wrong")]))
        .await
        .expect("the gate tick must not fail");

    let blocked = fx.eval_events(org, "ai.eval.gate.blocked").await;
    assert_eq!(
        blocked.len(),
        1,
        "exactly one gate must be announced, got {:?}",
        fx.eval_event_names(org).await
    );

    // The payload is compared against the SETTLED row, not against the fixture's own inputs:
    // the rate in the payload is the claim a subscriber renders, and a runner that computed it
    // independently would be free to disagree with the row it is announcing.
    let settled = eval_run::find_run(&fx.pool, org, gate_run.id)
        .await
        .expect("the read must succeed")
        .expect("the gate run must still be there");
    let payload = &blocked[0];
    assert_eq!(payload["run_id"], serde_json::json!(gate_run.id.to_string()));
    assert_eq!(payload["suite_id"], serde_json::json!(suite.to_string()));
    assert_eq!(payload["gate"], "block");
    assert_eq!(payload["kind"], "gate");
    assert_eq!(payload["pass_rate"], serde_json::json!(settled.pass_rate));
    // `Verdict::total()` is passed + failed + errors, and the settled row is where the third of
    // those lives. A payload that omitted the errored cases would report a smaller suite than the
    // one that ran — which reads as a clean run rather than a lossy one.
    assert_eq!(
        payload["total_cases"],
        serde_json::json!(settled.passed_cases + settled.failed_cases + settled.error_cases)
    );

    // The counter-case inside the same run: a blocked gate must not also announce a pass.
    let passed = fx.eval_events(org, "ai.eval.gate.passed").await;
    assert!(
        passed.iter().all(|row| row["run_id"] != serde_json::json!(gate_run.id.to_string())),
        "the blocked run must not also be announced as a passed gate"
    );
    // Every run announces its completion, so the blocked one stays visible to a subscriber that
    // listens only for `run.completed`.
    let completed = fx.eval_events(org, "ai.eval.run.completed").await;
    assert_eq!(completed.len(), 2, "both runs announce a completion");

    fx.dispose().await;
}

/// **A regression is announced WITH THE NAMES OF THE CASES THAT REGRESSED.**
///
/// This is the acceptance row's "with their names", and it is the row most likely to be
/// satisfied by the wrong thing: a payload listing every case in the suite is *structurally* the
/// same claim as one listing the regressed ones, and a subscriber reading it to decide which evals
/// to re-run cannot tell the difference without diffing the whole suite itself.
///
/// So the fixture is built so the two lists differ: three cases, the baseline passes all of them
/// and the gate run regresses exactly one. A payload naming all three is wrong even though every
/// name in it is a real case.
///
/// The expected list is computed from the stored `ai_eval_case_results`, not from the turn's
/// script: the rows are what the diff view reads, so if the rows and the announcement ever
/// disagree, the announcement should be judged against the rows and the disagreement is the
/// finding.
#[tokio::test]
async fn a_regression_is_announced_with_the_names_of_the_regressed_cases_only() {
    let fx = runner!();
    let org = fx.organization().await;
    let model = fx.model().await;
    let suite = fx
        .suite_with(
            org,
            "regressing",
            model,
            &[
                ("alpha", exact("right"), 1.0),
                ("beta", exact("right"), 1.0),
                ("gamma", exact("right"), 1.0),
            ],
            50,
            5.0,
            None,
        )
        .await;

    // The baseline: all three cases pass.
    fx.queue(org, suite, model).await;
    ai_eval_runner::tick(
        &fx.pool,
        1,
        &ScriptedTurn::new(&[("alpha", "right"), ("beta", "right"), ("gamma", "right")]),
    )
    .await
    .expect("the baseline tick must not fail");
    let baseline = first_run(&fx, suite).await;

    // The gate run: only `beta` regresses. The threshold is low enough that the rate alone
    // (66% >= 50%) still holds the gate, so this run can only be announced as a regression
    // because of the drop against the baseline — which is the behaviour the row is about, not
    // the threshold check the other walk already covers.
    eval_run::create_run(
        &fx.pool,
        org,
        &NewRun {
            suite_id: suite,
            kind: "gate".to_string(),
            snapshot: serde_json::json!({ "model": "p/walk-model" }),
            model_id: Some(model),
            judge_model_id: None,
            threshold_percent: 50,
            base_run_id: Some(baseline),
            triggered_by: None,
        },
    )
    .await
    .expect("the gate run must be created");
    ai_eval_runner::tick(
        &fx.pool,
        1,
        &ScriptedTurn::new(&[("alpha", "right"), ("beta", "wrong"), ("gamma", "right")]),
    )
    .await
    .expect("the gate tick must not fail");

    // **The expectation comes from `diff_runs`, the function the announcement is derived from.**
    // The first version of this row hand-wrote the SQL: `base.status = 'passed' and
    // head.status <> 'passed'`, in a schema whose status vocabulary is `pass`/`fail`/`error`/
    // `skipped`. It matched nothing, so the walk asserted `[] == ["beta"]` and failed — the
    // fixture, not the announcement, was wrong. It is worth naming why that is the dangerous
    // shape: a reader who fixed it by widening to `status <> 'pass'` would have passed, and
    // would have proved nothing about which cases regressed. Deriving the expectation from the
    // store's own comparator keeps the row honest about *what regressed* instead of about *a
    // query I wrote twice*.
    let head_rows = eval_run::list_case_results(&fx.pool, org, latest_run(&fx, suite).await)
        .await
        .expect("the head run's results must be readable");
    let base_rows = eval_run::list_case_results(&fx.pool, org, baseline)
        .await
        .expect("the baseline's results must be readable");
    let regressed: Vec<String> = eval_run::diff_runs(&base_rows, &head_rows)
        .rows
        .into_iter()
        .filter(|row| row.movement == "regressed")
        .map(|row| row.case_name)
        .collect();
    assert_eq!(
        regressed,
        vec!["beta".to_string()],
        "the fixture must regress exactly one case"
    );

    let announced = fx.eval_events(org, "ai.eval.regression.detected").await;
    assert_eq!(
        announced.len(),
        1,
        "exactly one regression must be announced, got {:?}",
        fx.eval_event_names(org).await
    );

    let names: Vec<String> = announced[0]["regressed_cases"]
        .as_array()
        .expect("regressed_cases must be a list")
        .iter()
        .map(|name| name.as_str().expect("each name must be a string").to_owned())
        .collect();
    assert_eq!(
        names, regressed,
        "the announcement must name the regressed cases the rows show, not every case in the suite"
    );
    assert!(
        !names.contains(&"alpha".to_string()) && !names.contains(&"gamma".to_string()),
        "cases that still pass must not be announced as regressed: a subscriber re-running the \
         named cases would burn budget on cases that were never broken"
    );

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

/// **A suite that pins a model is graded against THAT model, not the installation default.**
///
/// This is the "written but never read" walk for REQ-107. `start_run` resolves the model under
/// test through `resolve_under_test`, writes its `provider/model` string into the run's snapshot
/// (`snapshot["model"]`) and keeps its row id in `run.model_id` — and every reader of the
/// snapshot reads `prompt` (the system prompt) and `temperature`. **Nothing read `model`.**
///
/// `build_turn` then called the router with `requested: None`, and `lookup`'s `None` arm is
/// `default_model(pool)` — the installation's default. So a suite pinning `walk-judge` for its
/// regression checks would silently be graded by whatever model happened to be default, and the
/// run would report a pass rate for a model it never asked. The test failed for that reason:
///
/// ```text
/// the run graded walk-judge, the model the suite pins
///   left: walk-judge (what `start_run` recorded in the snapshot)
///  right: walk-default (what the router returned instead)
/// ```
///
/// The counter-case is the second half of the assertion and the reason this is a *defect* and not
/// a design choice: the walk pins a second model as the installation default, and asserts the
/// runner still grades the suite's own. A runner that resolved the default correctly, and a
/// runner that ignored the pin entirely, would be indistinguishable here — so the default has to
/// be a real, different, dialable model or the assertion measures nothing.
#[tokio::test]
async fn a_pinned_model_is_the_model_graded_not_the_installation_default() {
    let fx = runner!();
    let org = fx.organization().await;
    let pinned = fx.model().await;
    // A second model, made the installation's default. `resolve` with `requested: None` returns
    // exactly this one, so if the runner asks the router without naming the pin this is what it
    // gets — and the two assertions below tell the two worlds apart.
    let fallback = fx.model().await;
    fx.make_default(fallback).await;

    let suite = fx
        .suite_with(org, "pinned", pinned, &[("only", exact("right"), 1.0)], 90, 5.0, None)
        .await;
    // The run carries the pin in the snapshot, exactly as `start_run` writes it.
    let pinned_key = fx.key_of(pinned).await;
    fx.queue_with(
        org,
        suite,
        pinned,
        serde_json::json!({ "model": pinned_key, "temperature": 0.0, "prompt": "" }),
    )
    .await;

    // The turn is scripted, so this walk is about *which model the runner resolved*, not about
    // the answer. `LiveTurn` is private, so the fact under test is read the way production reads
    // it: through `build_turn`, which is what the runner's own tick calls. A test that asserted
    // on a value it passed in would measure the test.
    let queued = eval_run::claim_next_run(&fx.pool)
        .await
        .expect("the queue must read")
        .expect("the run must be claimable");
    let graded = ai_eval_runner::resolve_model_under_test(&fx.pool, &queued).await;
    assert_eq!(
        graded, pinned_key,
        "the run graded {graded}, the model the suite pins"
    );

    fx.dispose().await;
}

/// A tick with no free slot claims nothing — the guard that keeps a saturated process
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


// -------------------------------------------------------------------------------------------
// The live seam: a provider that answers on a real socket
// -------------------------------------------------------------------------------------------

/// A provider on `127.0.0.1` that answers every chat completion with `reply`.
///
/// The walks above script their turn, which is right for testing the *runner* and exactly wrong
/// for testing anything the runner does on the way to a provider: usage rows, prices, cost
/// attribution. `ScriptedTurn` writes none of that, so the `eval:judge` billing path was
/// implemented, reached by `spawn`, and provable only here.
///
/// The body is OpenAI's non-streaming answer shape because `adapter_for`'s default arm is
/// `openai_compatible` — which is also why `model_at` registers the provider with that protocol
/// rather than leaving it to the fallback: a test that relies on an unknown protocol resolving to
/// the default is testing the fallback, not the protocol it names.
async fn stub_provider(reply: &'static str) -> (String, tokio::task::JoinHandle<()>) {
    use axum::routing::post as route_post;
    use axum::{Json, Router};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the stub must bind a port");
    let address = listener.local_addr().expect("the stub has an address");
    let app = Router::new().route(
        "/chat/completions",
        route_post(move || {
            let reply = reply.to_owned();
            async move {
                Json(serde_json::json!({
                    "choices": [{
                        "message": { "role": "assistant", "content": reply },
                        "finish_reason": "stop"
                    }],
                    // Real token counts, not zeros: the cost row is derived from them, so a stub
                    // reporting zero tokens would produce a priced row of zero and the walk would
                    // pass while measuring nothing.
                    "usage": { "prompt_tokens": 1200, "completion_tokens": 300, "total_tokens": 1500 }
                }))
            }
        }),
    );
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), task)
}


/// **A rubric run records its judge, grades under it, and bills the grading to `eval:judge`.**
///
/// This is the one acceptance row no scripted walk could ever close. Every other walk in this file
/// drives `ScriptedTurn`, which writes **no** `ai_usage` rows by design — the seam exists to test
/// the runner, not the provider. The only writer of `eval:judge` is `LiveTurn::judge`: private,
/// built by `build_turn`, reached in production only through `spawn`'s tick loop. So the row was
/// fully implemented and completely unproven, and a walk on the scripted seam would have asserted
/// against a seam that writes nothing and passed for the wrong reason.
///
/// The walk therefore goes through the **live** seam against two stub providers on `127.0.0.1`: one
/// for the model under test, one for the judge. The judge stub answers `VERDICT: fail`, so the run
/// visibly settles on what the grading decided.
///
/// Six claims, each of which has a distinct wrong answer that a looser row would accept:
/// - the case result carries the judge's own sentence verbatim;
/// - the run's snapshot names the judge's model **and** the prompt version it was judged with, so
///   the run detail's reproduction claim is about the judge that actually ran;
/// - exactly one usage row carries `task = 'eval'` and exactly one carries `'eval:judge'`;
/// - the judge's row names the **judge's** provider and model key — grading billed to the graded
///   model is the mistake this whole split exists to prevent, and it is invisible in the row
///   count;
/// - the judge's row carries a **priced** total, and the answer's row carries a different one,
///   because the two are billed at two different models' prices;
/// - a run whose snapshot names no judge records **no** `eval:judge` row at all, which is what
///   keeps the number meaningful: a judge-less suite contributing judge spend would mean the cost
///   screen is counting something that never happened.
#[tokio::test]
async fn a_rubric_runs_judge_and_bill_the_grading_to_eval_judge() {
    let fx = runner!();
    let org = fx.organization().await;

    let (model_base, model_stub) = stub_provider("the model under test answers 'a vague answer'")
        .await;
    let (judge_base, judge_stub) = stub_provider("VERDICT: fail\nIt does not meet the rubric.")
        .await;

    // The stub servers must live until every call has been made, and the walk reads their
    // counters after `execute` returns, so both handles are kept to the end of the scope.
    let model = fx.model_at(model_base, "graded-model").await;
    let judge = fx.model_at(judge_base, "judge-model").await;

    // Prices, so the billing is arithmetic rather than a row of zeroes. A `cost_total_micros` of
    // 0 and a `cost_total_micros` of null are different claims, and this walk needs the numbers to
    // be able to tell them apart.
    sqlx::query(
        "update ai_models set input_cost_micros_per_mtok = 1000, \
         output_cost_micros_per_mtok = 2000 where id = any($1)",
    )
    .bind(vec![model, judge])
    .execute(&fx.pool)
    .await
    .expect("the prices must be settable");

    let model_key = fx.key_of(model).await;
    let judge_key = fx.key_of(judge).await;
    let suite = fx
        .suite_with(
            org,
            "judged-live",
            model,
            &[("rubric-case", serde_json::json!({ "rubric": "is clear and complete" }), 1.0)],
            90,
            5.0,
            Some(judge),
        )
        .await;
    fx.queue_with(
        org,
        suite,
        model,
        serde_json::json!({
            "model": model_key,
            "temperature": 0.0,
            "prompt": "",
            "judge_model": judge_key,
            "judge_prompt": "grade strictly",
            "judge_prompt_version": 3,
        }),
    )
    .await;

    // The production seam, built by the runner's own factory rather than assembled by the walk.
    let queued = eval_run::claim_next_run(&fx.pool)
        .await
        .expect("the queue must read")
        .expect("the run must be claimable");
    // `live_turn_for` is the runner's own factory, so this line is the claim that the pinned
    // model **reaches the resolve**. It is where the pin bug was caught: `build_turn` had the
    // pin in `DecisionContext.requested` but passed `None` for the argument `decide` reads, so
    // the runner resolved the installation default and this returned `None`. No scripted walk
    // could have seen it, because the scripted seam never resolves anything.
    let turn = ai_eval_runner::live_turn_for(&fx.pool, &queued)
        .await
        .expect("the run's pinned model and judge must both resolve to dialable targets");
    let verdict = ai_eval_runner::execute(&fx.pool, &queued, &turn)
        .await
        .expect("the live run must not fail");
    let results = eval_run::list_case_results(&fx.pool, org, queued.id)
        .await
        .expect("the results must be readable");
    assert_eq!(results.len(), 1);
    assert_eq!(
        verdict.failed, 1,
        "the judge failed the case, so the run failed it (status {:?}, error {:?}, checks {:?})",
        results[0].status, results[0].error, results[0].checks
    );
    // The `error` is in the message because a case that could not be called also settles with
    // no failures, and "0 failed" alone sends the reader looking for a scoring bug instead of at
    // the reason the row carries.
    assert_eq!(
        results[0].status, "fail",
        "the judge said fail, so the case fails (error: {:?})",
        results[0].error
    );
    assert!(
        results[0]
            .judge_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("VERDICT: fail")),
        "the judge's own sentence is stored verbatim, got {:?}",
        results[0].judge_reason
    );

    // The snapshot is what a reproduction reads, so it must name the judge that ran and the
    // prompt version it judged with.
    let settled = eval_run::find_run(&fx.pool, org, queued.id)
        .await
        .expect("the read must succeed")
        .expect("the run must still be there");
    assert_eq!(settled.snapshot["judge_model"], serde_json::json!(judge_key));
    assert_eq!(settled.snapshot["judge_prompt_version"], serde_json::json!(3));

    // The billing. Read the rows, do not trust the return value: `record_usage` is best-effort
    // (a failed write is a `tracing::warn!`, never a run failure), so the row count is the only
    // evidence the row landed.
    // Scoped by the two fixture providers rather than by tenant: `ai_provider_usage` has no
    // organization column (it links to `ai_route_decisions` by `decision_id`), and the fixture's
    // provider names carry a fresh uuid, so this scope cannot pick up another test's rows. The
    // alternative — filtering on nothing — would be a walk that passes or fails with whatever
    // else is in the table.
    let usage = sqlx::query_as::<_, (String, Option<String>, Option<i64>)>(
        "select task, model_key, cost_total_micros from ai_provider_usage \
         where provider_id = any($1) order by task",
    )
    .bind(vec![fx.provider_of(model).await, fx.provider_of(judge).await])
    .fetch_all(&fx.pool)
    .await
    .expect("the usage rows must be readable");

    assert_eq!(
        usage.len(),
        2,
        "one answer call and one judge call, got {usage:?}"
    );
    let tasks: Vec<&str> = usage.iter().map(|(task, _, _)| task.as_str()).collect();
    assert!(
        tasks.contains(&"eval") && tasks.contains(&"eval:judge"),
        "the grading must be billed separately from the answer, got {tasks:?}"
    );

    let judge_row = usage
        .iter()
        .find(|(task, _, _)| task == "eval:judge")
        .expect("a judge row must exist");
    assert_eq!(
        judge_row.1.as_deref(),
        Some(judge_key.as_str()),
        "grading is billed to the judge that did the grading, not to the model being graded"
    );
    assert!(
        judge_row.2.is_some_and(|micros| micros > 0),
        "the judge row must carry a priced total, got {:?} - a null here is the `cost: None` \
         defect this walk was written to catch",
        judge_row.2
    );

    // The two rows are priced at two different models' prices, so equal totals would mean the
    // price was taken from one model and applied to both.
    let answer_row = usage
        .iter()
        .find(|(task, _, _)| task == "eval")
        .expect("an answer row must exist");
    assert_eq!(answer_row.1.as_deref(), Some(model_key.as_str()));
    assert!(answer_row.2.is_some_and(|micros| micros > 0));

    model_stub.abort();
    judge_stub.abort();
    fx.dispose().await;
}

/// **A run with no judge records no judge spend.** The control for the row above, and the reason
/// its count is worth asserting: if a judge-less suite produced an `eval:judge` row, then the
/// number on `/ai/costs` would be counting calls that never happened.
#[tokio::test]
async fn a_run_with_no_judge_records_no_judge_spend() {
    let fx = runner!();
    let org = fx.organization().await;
    let (model_base, model_stub) = stub_provider("a plain answer").await;
    let model = fx.model_at(model_base, "solo-model").await;
    sqlx::query(
        "update ai_models set input_cost_micros_per_mtok = 1000, \
         output_cost_micros_per_mtok = 2000 where id = $1",
    )
    .bind(model)
    .execute(&fx.pool)
    .await
    .expect("the prices must be settable");

    let suite = fx
        .suite_with(org, "no-judge", model, &[("only", exact("a plain answer"), 1.0)], 90, 5.0, None)
        .await;
    fx.queue_with(
        org,
        suite,
        model,
        serde_json::json!({ "model": fx.key_of(model).await, "temperature": 0.0, "prompt": "" }),
    )
    .await;

    let queued = eval_run::claim_next_run(&fx.pool)
        .await
        .expect("the queue must read")
        .expect("the run must be claimable");
    let turn = ai_eval_runner::live_turn_for(&fx.pool, &queued)
        .await
        .expect("the run's model must resolve");
    ai_eval_runner::execute(&fx.pool, &queued, &turn)
        .await
        .expect("the live run must not fail");

    let tasks: Vec<String> = sqlx::query_scalar(
        "select task from ai_provider_usage where provider_id = $1 order by task",
    )
    .bind(fx.provider_of(model).await)
    .fetch_all(&fx.pool)
    .await
    .expect("the usage rows must be readable");
    assert_eq!(
        tasks,
        vec!["eval".to_string()],
        "a suite with no judge spends exactly one row, and it is the answer's"
    );

    model_stub.abort();
    fx.dispose().await;
}

/// **A stub that answers each case's own prompt.** The two answers below are the fixture's, and
/// they are keyed on the *question*, not on the case row.
///
/// The first version used the single-reply `stub_provider` above, on the reasoning that "the same
/// bytes for every case" isolates the expectation. It isolates it in the wrong direction: with one
/// fixed reply both cases get the leaky answer, so the clean case fails too and the walk asserts
/// `failed == 2` — it measures the stub, not the guard. The honest shape is the one a real
/// provider has: the model reads the case's prompt and answers *that* question, so the run's two
/// rows differ because the model behaved differently, which is exactly what an eval measures.
///
/// The prompt is matched on its content, so the answer follows the case that was asked rather than
/// its position in the queue — a runner that executed the cases in a different order would get the
/// same rows.
async fn stub_provider_answering_two() -> (String, tokio::task::JoinHandle<()>) {
    use axum::routing::post as route_post;
    use axum::{Json, Router};

    const LEAKY: &str = "Reach me at ada@example.com or +90 532 111 22 33.";
    const CLEAN: &str = "The order shipped on Tuesday.";

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the stub must bind a port");
    let address = listener.local_addr().expect("the stub has an address");
    let app = Router::new().route(
        "/chat/completions",
        route_post(move |body: String| async move {
            // The whole request body, not the parsed messages: the claim is which question the
            // model was asked, and a walk that parsed and re-serialized it would be asserting on
            // its own serialization. A stub that answered by *order* would pass a runner that
            // scored every case against the previous case's output.
            let reply = if body.contains("contact") { LEAKY } else { CLEAN };
            Json(serde_json::json!({
                "choices": [{
                    "message": { "role": "assistant", "content": reply },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 1200, "completion_tokens": 300, "total_tokens": 1500 }
            }))
        }),
    );
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), task)
}

/// **`no_pii` is decided by the installation's guard, at the run level, and it is the guard that
/// decides — not a regex this file wrote.**
///
/// The acceptance row's wording is "a value REQ-105's detector would mask". That phrase is the
/// whole test: an eval suite that grew its own e-mail/phone pattern would drift from the guard the
/// product actually applies to outbound text, so a suite could pass while production masked the
/// same output. The runner already loads `guard_store::load_guard` once per run — this walks that
/// seam with the **seeded** platform rules, which is the only rule set a real installation has.
///
/// The two cases are the shape of the property: a leaky answer fails it, a clean one passes it,
/// and the clean case names a second property too — a runner that short-circuited on the first
/// satisfied property, or that never ran the guard at all and answered `pass` by default, would
/// read the leaky row green.
#[tokio::test]
async fn a_no_pii_case_fails_on_what_the_data_guard_would_mask_and_passes_a_clean_one() {
    let fx = runner!();
    let org = fx.organization().await;

    // The live seam again, so the output that reaches `score_case` is a provider's answer and the
    // guard's verdict is computed over exactly the bytes the panel would show.
    let (model_base, model_stub) = stub_provider_answering_two().await;
    let model = fx.model_at(model_base, "pii-model").await;
    let model_key = fx.key_of(model).await;

    // Assert the fixture is a guard fixture at all: on a database whose seed lost the platform
    // rules every row below would read "clean" and the walk would pass while measuring nothing.
    let guard = omnion_ai_hub::guard_store::load_guard(&fx.pool, org)
        .await
        .expect("the guard must load");
    let findings = omnion_ai_hub::eval_case::pii_findings(&guard, "write to ada@example.com");
    assert!(
        !findings.clean,
        "the seeded guard must mask an e-mail address, or this walk measures nothing: {findings:?}"
    );
    assert!(
        omnion_ai_hub::eval_case::pii_findings(&guard, "The order shipped on Tuesday.").clean,
        "and the control output must be clean"
    );

    let suite = fx
        .suite_with(
            org,
            "pii",
            model,
            &[
                // The word `contact` is what the stub keys on to pick the leaky answer, so the two
                // cases are asked two genuinely different questions rather than being told the same
                // one and then expecting two different verdicts from identical bytes.
                ("masked", serde_json::json!({ "no_pii": true, "contains": ["ada"] }), 1.0),
                ("clean", serde_json::json!({ "no_pii": true, "contains": ["Tuesday"] }), 1.0),
            ],
            90,
            5.0,
            None,
        )
        .await;
    // The prompts carry the questions the stub reads. `NewCase` writes `input` verbatim, so this is
    // the whole wire the model sees.
    for (name, prompt) in [
        ("masked", "give me the contact address for the account"),
        ("clean", "when did the order ship?"),
    ] {
        fx.input_prompt(org, suite, name, prompt).await;
    }
    fx.queue_with(
        org,
        suite,
        model,
        serde_json::json!({ "model": model_key, "temperature": 0.0, "prompt": "" }),
    )
    .await;

    let queued = eval_run::claim_next_run(&fx.pool)
        .await
        .expect("the queue must read")
        .expect("the run must be claimable");
    let turn = ai_eval_runner::live_turn_for(&fx.pool, &queued)
        .await
        .expect("the run's pinned model must resolve to a dialable target");
    let verdict = ai_eval_runner::execute(&fx.pool, &queued, &turn)
        .await
        .expect("the live run must not fail");

    assert_eq!(verdict.failed, 1, "exactly the masked case fails: {verdict:?}");
    let results = eval_run::list_case_results(&fx.pool, org, queued.id)
        .await
        .expect("the results must be readable");
    let by_name = |name: &str| {
        results
            .iter()
            .find(|row| row.case_name == name)
            .unwrap_or_else(|| panic!("{name} must have a result: {results:?}"))
    };

    // The masked case fails on the guard, and the detail names the label — an operator reading an
    // expanded row has to be able to tell *which* control fired without opening the guard screen.
    let masked = by_name("masked");
    assert_eq!(masked.status, "fail", "checks: {:?}", masked.checks);
    let masked_checks: Vec<serde_json::Value> = serde_json::from_value(masked.checks.clone())
        .unwrap_or_else(|_| panic!("checks must be a list: {:?}", masked.checks));
    let pii = masked_checks
        .iter()
        .find(|check| check["property"] == "no_pii")
        .unwrap_or_else(|| panic!("the no_pii check must be recorded: {masked_checks:?}"));
    assert_eq!(pii["passed"], serde_json::json!(false));
    let detail = pii["detail"].as_str().unwrap_or_default().to_owned();
    assert!(
        detail.contains("email"),
        "the detail must name the label the guard fired on, got {detail:?}"
    );
    // The output itself is stored unmasked: this is the panel's own copy of an answer that leaked
    // an address, and an eval store that masked it would hide the very finding the case exists to
    // surface. Asserted so a later "let's be careful with eval output" change cannot quietly do it.
    assert!(
        masked.output.as_deref().unwrap_or_default().contains("@example.com"),
        "the output row keeps what the model said, so the finding is readable"
    );

    // The clean case passes on BOTH of its properties. A runner that decided `no_pii` from the
    // case's own regex rather than the guard would also pass this — which is exactly why the
    // masked row above carries the real label in its detail.
    let clean = by_name("clean");
    assert_eq!(clean.status, "pass", "checks: {:?}", clean.checks);
    let clean_checks: Vec<serde_json::Value> = serde_json::from_value(clean.checks.clone())
        .unwrap_or_else(|_| panic!("checks must be a list: {:?}", clean.checks));
    assert!(
        clean_checks.iter().any(|c| c["property"] == "no_pii" && c["passed"] == serde_json::json!(true)),
        "the clean case records a passing no_pii: {clean_checks:?}"
    );

    model_stub.abort();
    fx.dispose().await;
}

/// **A case whose `no_pii` was never evaluated is `error`, not `pass`.**
///
/// The control for the walk above, and the reason the row above needed a live seam at all. The
/// runner hands `score_case` an `Option<&LoadedGuard>`; a runner that passed `None` — because the
/// load failed, or because it never tried — still produces two plausible rows, except that the
/// `no_pii` check is recorded `unevaluated` and the case settles `error`. A property that could
/// not be measured must never read as a satisfied bar in a promotion gate.
///
/// It is walked here rather than in `eval_case.rs` because the unit module can only be handed a
/// `GuardOutcome` by its own caller; proving that the *runner* supplies one needs the runner.
#[tokio::test]
async fn a_no_pii_case_the_guard_never_ran_is_an_error_and_not_a_pass() {
    let fx = runner!();
    let org = fx.organization().await;

    // Break the guard the way production does: the rule budget refuses to compile. `load_guard`
    // is what the runner swallows into `None`, so this is the honest way to reach that arm — and
    // it is why the failure has to settle as `error`, not silently as a pass.
    sqlx::query("update ai_guard_rules set enabled = false")
        .execute(&fx.pool)
        .await
        .expect("the guard rules must be disableable");
    // Zero rules is not a budget refusal, so the load still succeeds with an empty detector and
    // the row below would read clean. Force the arm under test instead: a rule whose pattern the
    // compiler rejects makes `Detector::new` fail and `load_guard` return Err.
    sqlx::query(
        "update ai_guard_rules set enabled = true, pattern = '(?<invalid' \
         where key = 'email.builtin'",
    )
    .execute(&fx.pool)
    .await
    .expect("the platform email rule must be reachable");
    assert!(
        omnion_ai_hub::guard_store::load_guard(&fx.pool, org).await.is_err(),
        "an uncompilable rule set must fail the load, or the walk is not reaching the None arm"
    );

    let model = fx.model().await;
    let suite = fx
        .suite_with(
            org,
            "no-guard",
            model,
            &[("pii", serde_json::json!({ "no_pii": true }), 1.0)],
            90,
            5.0,
            None,
        )
        .await;
    fx.queue(org, suite, model).await;

    let turn = ScriptedTurn::new(&[("pii", "write to ada@example.com")]);
    let report = ai_eval_runner::tick(&fx.pool, 1, &turn)
        .await
        .expect("the tick must not fail");
    assert_eq!(report.claimed, 1, "the run must still be claimed and scored");

    let results = eval_run::list_case_results(&fx.pool, org, first_run(&fx, suite).await)
        .await
        .expect("the results must be readable");
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].status, "error",
        "a case whose guard never ran must settle error, not pass: {:?}",
        results[0].checks
    );

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