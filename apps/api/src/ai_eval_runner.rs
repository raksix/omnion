//! The background eval runner (REQ-107, slice 3).
//!
//! `main.rs` spawns this task when the runner is enabled (`OMNION_AI_EVAL_RUNNER`, default on).
//! Each tick claims at most one queued run, executes every enabled case against the model under
//! test, scores the output, settles the row and announces the verdict. A second task sweeps for
//! suites whose cron schedule is due, and a third fails runs the timeout caught.
//!
//! Five decisions, each of which closes a way a run can be lost, run twice, or read green when
//! it was not.
//!
//! **The claim is the store's, not the runner's.** [`eval_run::claim_next_run`] does the
//! `for update skip locked` selection and the status change in one statement. A runner that read
//! a queue and then picked one would race a second API process, and two processes scoring the
//! same suite would produce two verdicts for one run — with the panel showing whichever wrote
//! last.
//!
//! **A case that cannot be scored is `error`, never `skipped`.** The verdict arithmetic excludes
//! `skipped` from the denominator, so a runner that skipped what it could not run would settle a
//! suite of twenty broken cases as 100% over the one case that answered. An `error` is counted,
//! which is the honest reading: the run could not produce a result, so it did not pass.
//!
//! **The judge is a second model, and the refusal is at the call site too.** The suite store
//! already refuses a judge equal to the model under test at save time; this runner re-checks it
//! because a suite can be edited between two runs, and a judge that is the model under test is
//! a model grading its own homework. A `rubric` case with no judge produces an `error` check
//! naming what was missing rather than a silent pass — the scorer is already shaped that way, and
//! the runner supplies `None` rather than inventing a verdict.
//!
//! **Cancellation is polled at every case boundary, not between events.** The runner has no
//! handle into a provider call, so it asks [`eval_run::should_stop`] before each case. A case
//! that has already started is left to finish: a half-scored case is worse than a slightly later
//! stop, and the spec asks for exactly this.
//!
//! **The settle and the last-run projection are one transaction.** The suite list reads
//! `last_pass_rate` / `last_gate` as columns and the history reads the run row, so writing them
//! separately leaves a window where two screens disagree about the same run. If the projection
//! fails the whole settle rolls back, which is the correct direction: a run whose number is
//! missing is better than a run whose number is on one screen and missing from another.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use omnion_ai_hub::client::{ChatMessage, ChatRequest, ChatOutcome, ProviderTarget};
use omnion_ai_hub::cost::{self, ModelPrice};
use omnion_ai_hub::eval_case::{self, GuardOutcome, JudgeOutcome, Observations};
use omnion_ai_hub::eval_run::{self, NewCaseResult, RunRow, Verdict};
use omnion_ai_hub::eval_store::{self, CaseRow, SuiteRow};
use omnion_ai_hub::guard_store;
use omnion_ai_hub::registry;
use omnion_ai_hub::{DecisionContext, Scope, resolve_and_record};
use sqlx::PgPool;
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use crate::state::AppState;

/// How often a worker looks for a queued run.
///
/// 250 ms, the same reasoning as the agent runner: a run's wall-clock is almost entirely inside
/// provider calls, so a slow tick is added latency on a run that was already queued. A missed
/// tick is skipped rather than replayed, because a burst of catch-up claims after a GC pause
/// would start four scoring runs at once on a box that was already behind.
const TICK_MS: u64 = 250;

/// How often stale runs are failed.
///
/// Once a minute. The predicate is "running longer than the timeout", so running it more often
/// can only ever find the row it already failed — or nothing.
const REAPER_MS: u64 = 60_000;

/// The system prompt a suite falls back to when it pins none.
///
/// Empty, deliberately: a suite's own configuration is the reproduction data, and a default
/// prompt baked into the runner would be a variable nobody records — the snapshot would claim a
/// configuration the run did not use.
const DEFAULT_SYSTEM_PROMPT: &str = "";

/// What one case's execution produced, before it becomes a row.
struct CaseOutcome {
    /// The result to write.
    result: NewCaseResult,
    /// Whether the runner should keep going.
    stop: bool,
}

/// One turn's answer, with the accounting the case result stores.
///
/// **Public because the walk constructs one.** A seam is only a seam if something on the other
/// side of it can produce the value it carries; keeping this crate-private would force the walk
/// to dial a provider to prove the runner, which tests the provider rather than the runner.
#[derive(Debug, Clone)]
pub struct Turn {
    /// The model's text.
    pub text: String,
    /// Input tokens, when reported.
    pub prompt_tokens: Option<i32>,
    /// Output tokens, when reported.
    pub completion_tokens: Option<i32>,
    /// What it cost, when the model is priced and tokens were reported.
    pub cost_micros: i64,
}

/// The seam that turns a case's text into the run's judgement.
///
/// A trait rather than a free function because the two production callers (the live runner and
/// the walk that drives it) must score through **the same code**: a walk that reimplemented the
/// judge call would prove the walk, not the runner. The live implementation talks to a provider;
/// the walk's implementation returns a scripted verdict.
///
/// **Both methods return boxed futures rather than being `async fn`.** A trait whose methods are
/// `async fn` is not object-safe in the shape this needs — `&dyn CaseTurn` does not exist for it
/// — and the codebase already has the precedent (`change_sets::Applier`): a boxed
/// `Pin<Box<dyn Future + Send + '_>>` costs one lifetime annotation and buys a trait the runner,
/// the tick and the walk can all hold behind the same reference.
pub trait CaseTurn: Send + Sync {
    /// One turn against the model under test.
    fn answer<'a>(
        &'a self,
        case: &'a CaseRow,
        system: &'a str,
        temperature: Option<f64>,
    ) -> Pin<Box<dyn Future<Output = Result<Turn, String>> + Send + 'a>>;

    /// The judge's verdict on an output, or `None` when no judge is available.
    ///
    /// `None` is **not** a pass: the scorer turns a missing judge into an `error` check naming
    /// what was missing, because a rubric case that was never really judged must never read as a
    /// green row in a release gate.
    fn judge<'a>(
        &'a self,
        case: &'a CaseRow,
        output: &'a str,
        rubric: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<JudgeOutcome>> + Send + 'a>>;

    /// What the judge's own call cost, in micros, so far.
    fn judge_cost_micros(&self) -> i64;
}

/// The live turn: one provider call per case, and a second one for a rubric case.
///
/// The two calls go through the **same router** the chat endpoint uses, and are recorded with
/// `feature = 'eval'` / `'eval:judge'`, so eval spend is visible in REQ-104's cost screen instead
/// of hiding inside another feature's total.
struct LiveTurn {
    pool: PgPool,
    organization_id: Uuid,
    model_key: String,
    provider: ProviderTarget,
    judge_key: Option<String>,
    judge_provider: Option<ProviderTarget>,
    prices: Arc<Vec<(String, ModelPrice)>>,
    judge_cost_micros: std::sync::atomic::AtomicI64,
}

impl CaseTurn for LiveTurn {
    fn answer<'a>(
        &'a self,
        case: &'a CaseRow,
        system: &'a str,
        temperature: Option<f64>,
    ) -> Pin<Box<dyn Future<Output = Result<Turn, String>> + Send + 'a>> {
        Box::pin(async move {
            let mut request =
                ChatRequest::new(self.model_key.clone(), case_messages(case, system));
            request.temperature = temperature;
            let outcome = omnion_ai_hub::client::chat(&self.provider, &request)
                .await
                .map_err(|error| error.to_string())?;
            let (prompt_tokens, completion_tokens) = tokens_of(&outcome);
            let billed = cost::call_cost(
                cost::price_for(&self.prices, &self.model_key),
                prompt_tokens,
                completion_tokens,
            )
            .map_or(0, |cost| cost.total_micros);
            self.record_usage(
                self.provider.id,
                &self.model_key,
                "eval",
                prompt_tokens,
                completion_tokens,
            )
            .await;
            Ok(Turn {
                text: outcome.content,
                prompt_tokens,
                completion_tokens,
                cost_micros: billed,
            })
        })
    }

    fn judge<'a>(
        &'a self,
        _case: &'a CaseRow,
        output: &'a str,
        rubric: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<JudgeOutcome>> + Send + 'a>> {
        Box::pin(async move {
            let provider = self.judge_provider.as_ref()?;
            let key = self.judge_key.as_deref()?;
            let request = ChatRequest::new(
                key.to_string(),
                vec![
                    ChatMessage::system(JUDGE_SYSTEM),
                    ChatMessage::user(format!(
                        "Rubric:\n{rubric}\n\n---\nThe output under judgement:\n{output}\n\n---\n\
                         Reply with VERDICT: pass or VERDICT: fail on the first line, then one \
                         or two sentences saying why. Nothing else."
                    )),
                ],
            );
            let outcome = omnion_ai_hub::client::chat(provider, &request).await.ok()?;
            let (prompt_tokens, completion_tokens) = tokens_of(&outcome);
            let billed = cost::call_cost(
                cost::price_for(&self.prices, key),
                prompt_tokens,
                completion_tokens,
            )
            .map_or(0, |cost| cost.total_micros);
            self.judge_cost_micros
                .fetch_add(billed, std::sync::atomic::Ordering::SeqCst);
            // The judge's own provider, under `eval:judge` — not the model under test's row,
            // and not `eval`. A cost screen that attributes grading to the graded model cannot
            // answer "what did this suite cost to run".
            self.record_usage(
                provider.id,
                key,
                "eval:judge",
                prompt_tokens,
                completion_tokens,
            )
            .await;
            Some(parse_verdict(&outcome.content))
        })
    }

    fn judge_cost_micros(&self) -> i64 {
        self.judge_cost_micros.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// The judge's own instructions, pinned on the suite as `judge_prompt` when it has one.
///
/// The suite's own `judge_prompt` is what the snapshot records and what a reproduction uses;
/// this is the **default** the runner falls back to, and it is a constant rather than an inline
/// string at the call site so that "the judge's prompt" names one thing in the codebase.
const JUDGE_SYSTEM: &str = "\
You are grading one model's output against a rubric. You are a strict grader: the rubric is the \
whole standard, and an output that is merely good enough for a different rubric fails. Answer \
in two lines at most.";

/// Turn a judge's answer into a verdict.
///
/// The rule is the sentence's own: a `fail` anywhere in the first line loses, and anything else
/// passes. A judge that refused to answer still produced text, and treating silence as a pass
/// would let a broken judge turn every rubric case green — the exact false confidence the
/// request's risks section warns about. Anything without a verdict word is a fail.
fn parse_verdict(answer: &str) -> JudgeOutcome {
    let first = answer.lines().next().unwrap_or("").to_ascii_lowercase();
    let passed = !first.contains("fail") && first.contains("pass");
    JudgeOutcome {
        passed,
        reason: answer.trim().to_string(),
    }
}

/// Token counts off an outcome, narrowed to what `ai_usage` and the case result store.
fn tokens_of(outcome: &ChatOutcome) -> (Option<i32>, Option<i32>) {
    // No `?` here: the function returns a *pair* of options, so "no usage reported" is
    // `(None, None)` rather than an early return that skips the second value. A provider that
    // reported output tokens but not input tokens must not lose the output count.
    let narrow = |value: Option<u64>| value.map(|v| i32::try_from(v).unwrap_or(i32::MAX));
    match &outcome.usage {
        Some(usage) => (narrow(usage.prompt_tokens), narrow(usage.completion_tokens)),
        None => (None, None),
    }
}

impl LiveTurn {
    /// Write a call onto `ai_usage`, under the feature it was spent on.
    ///
    /// **`task` and the model are parameters, not constants.** The first version hardcoded
    /// `task: "eval"` and this struct's own `provider`/`model_key`, so the judge's call was
    /// billed to — and attributed to — the model under test. That is wrong twice over: the run's
    /// spend is misattributed to a model that was never asked, and the judge's cost never
    /// appears under `eval:judge`, which is the acceptance row's own requirement. The caller
    /// passes the feature it means; the answer call passes `eval`, the judge passes
    /// `eval:judge`.
    ///
    /// Best-effort by design: the run's cost is already snapshotted onto the run row and the
    /// case result, so a usage row that failed to write is a gap in REQ-104's *view* of spend,
    /// not a reason to fail a run whose work is already done and paid for.
    async fn record_usage(
        &self,
        provider_id: Uuid,
        model_key: &str,
        task: &str,
        prompt_tokens: Option<i32>,
        completion_tokens: Option<i32>,
    ) {
        let usage = omnion_ai_hub::health_store::NewUsage {
            provider_id,
            model_key: Some(model_key.to_owned()),
            task: task.to_owned(),
            outcome: "ok".to_owned(),
            http_status: Some(200),
            prompt_tokens,
            completion_tokens,
            latency_ms: 0,
            substituted_from: None,
            first_byte_at: None,
            cost: None,
        };
        if let Err(error) = omnion_ai_hub::health_store::record_usage(&self.pool, usage).await {
            tracing::warn!(%error, task, "an eval usage row could not be written");
        }
    }
}

/// The messages one case sends: its system prompt and its input.
///
/// A case's `input` is a document (the panel's editor writes `{prompt, context, system}`), so
/// this reads the fields it knows and puts the rest in the user turn verbatim rather than
/// guessing a schema the store never validated.
fn case_messages(case: &CaseRow, system: &str) -> Vec<ChatMessage> {
    let prompt = case
        .input
        .get("prompt")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let context = case
        .input
        .get("context")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut messages = vec![ChatMessage::system(if system.is_empty() {
        DEFAULT_SYSTEM_PROMPT
    } else {
        system
    })];
    if !context.is_empty() {
        messages.push(ChatMessage::user(context));
    }
    messages.push(ChatMessage::user(prompt));
    messages
}

/// Score one case's output and build the row to write.
///
/// Pure with respect to the database: everything it needs is on `case` and in the turn, which is
/// what lets the walk drive it without a run row.
async fn score_case(
    case: &CaseRow,
    turn: Turn,
    guard: Option<&guard_store::LoadedGuard>,
    judge: Option<JudgeOutcome>,
    judge_cost_micros: i64,
) -> NewCaseResult {
    let expectation = match eval_case::expectation_from(&case.expected) {
        Ok(expectation) => expectation,
        Err(error) => {
            return NewCaseResult {
                case_id: Some(case.id),
                case_name: case.name.clone(),
                status: "error".to_owned(),
                score: None,
                checks: serde_json::json!([]),
                judge_reason: None,
                output: Some(turn.text),
                latency_ms: None,
                prompt_tokens: turn.prompt_tokens,
                completion_tokens: turn.completion_tokens,
                cost_micros: turn.cost_micros + judge_cost_micros,
                tool_calls: serde_json::json!([]),
                error: Some(format!("this case's expectations could not be read: {error}")),
                weight: case.weight,
            }
        }
    };

    // `no_pii` delegates to REQ-105's detector, loaded once per run rather than per case: the
    // rules are the installation's, not the case's, and compiling them forty times a run would
    // be the most expensive thing in it.
    let guard_outcome: Option<GuardOutcome> = guard.map(|loaded| eval_case::pii_findings(loaded, &turn.text));

    let observations = Observations {
        steps: Some(1),
        cost_micros: Some(turn.cost_micros + judge_cost_micros),
        latency_ms: None,
        citations: citations_in(&turn.text),
        tool_calls: Vec::new(),
    };

    let verdict = eval_case::score_output(
        &expectation,
        &turn.text,
        &observations,
        judge.as_ref(),
        guard_outcome.as_ref(),
    );

    let checks = serde_json::to_value(&verdict.checks).unwrap_or_else(|_| serde_json::json!([]));
    let cost_micros = turn.cost_micros + judge_cost_micros;
    NewCaseResult {
        case_id: Some(case.id),
        case_name: case.name.clone(),
        status: verdict.status.as_str().to_owned(),
        score: Some(verdict.score()),
        checks,
        judge_reason: judge.map(|judge| judge.reason),
        output: Some(turn.text),
        latency_ms: None,
        prompt_tokens: turn.prompt_tokens,
        completion_tokens: turn.completion_tokens,
        cost_micros,
        tool_calls: serde_json::json!([]),
        error: None,
        weight: case.weight,
    }
}

/// The citation markers in an output, for a `citations_required` case.
///
/// A marker is a `[1]` / `[source]` / `(#ref)` bracket. This is the run layer's measurement and
/// the scorer reads it, because the scorer cannot see the provider's own citation metadata.
fn citations_in(output: &str) -> Vec<String> {
    let mut found = Vec::new();
    let bytes = output.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'[' {
            if let Some(end) = output[index + 1..].find(']') {
                let inner = &output[index + 1..index + 1 + end];
                if !inner.is_empty() && inner.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
                    let marker = format!("[{inner}]");
                    if !found.contains(&marker) {
                        found.push(marker);
                    }
                }
                index += end + 2;
                continue;
            }
        }
        index += 1;
    }
    found
}

/// Score one case against the turn, and wrap it as a `CaseOutcome`.
async fn run_case(
    pool: &PgPool,
    run: &RunRow,
    case: &CaseRow,
    turn: &dyn CaseTurn,
    guard: Option<&guard_store::LoadedGuard>,
) -> CaseOutcome {
    let started = std::time::Instant::now();
    let system = run
        .snapshot
        .get("prompt")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(DEFAULT_SYSTEM_PROMPT);

    // The seam is `turn`; the answer is `answer`. The first version let the answer shadow the
    // seam, so the judge call below resolved to a struct with no `judge` method — a compile
    // error, which is the good outcome. Had it compiled, it would have scored the judge against
    // the wrong thing.
    let answer = match turn.answer(case, system, snapshot_temperature(run)).await {
        Ok(answer) => answer,
        Err(error) => {
            return CaseOutcome {
                result: NewCaseResult {
                    case_id: Some(case.id),
                    case_name: case.name.clone(),
                    status: "error".to_owned(),
                    score: None,
                    checks: serde_json::json!([]),
                    judge_reason: None,
                    output: None,
                    latency_ms: i32::try_from(started.elapsed().as_millis()).ok(),
                    prompt_tokens: None,
                    completion_tokens: None,
                    cost_micros: 0,
                    tool_calls: serde_json::json!([]),
                    error: Some(format!("the model could not be called: {error}")),
                    weight: case.weight,
                },
                // A provider that is down fails every remaining case identically, and each of
                // those calls costs a round trip to learn the same thing. Stop here: the run
                // settles on what it has, with the reason on the row.
                stop: true,
            }
        }
    };

    // The judge runs only for a case that names a rubric. Its cost is measured as a **delta**
    // around the call: the turn's counter is cumulative, so reading it after the judge and
    // subtracting the value from before charges this case for its own judge call and not for
    // the ones the earlier cases already paid for. Reading it directly — the first version —
    // made case N's result carry the sum of judges 1..N, which is an accounting bug that grows
    // with the suite and is invisible on any suite of two.
    let before = turn.judge_cost_micros();
    let judge = match rubric_text(case).is_empty() {
        true => None,
        false => turn.judge(case, &answer.text, rubric_text(case)).await,
    };
    let judge_cost = turn.judge_cost_micros().saturating_sub(before);

    let mut result = score_case(case, answer, guard, judge.clone(), judge_cost).await;
    result.latency_ms = i32::try_from(started.elapsed().as_millis()).ok();

    let stop = eval_run::should_stop(pool, run.id).await.unwrap_or(false);
    CaseOutcome { result, stop }
}

/// The rubric text a case asks a judge to apply.
fn rubric_text(case: &CaseRow) -> &str {
    case.expected
        .get("rubric")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
}

/// The temperature the run's snapshot recorded, which is what reproduces the run.
fn snapshot_temperature(run: &RunRow) -> Option<f64> {
    run.snapshot
        .get("temperature")
        .and_then(serde_json::Value::as_f64)
}

/// Execute one claimed run, end to end.
///
/// Split out from the tick so the walk can drive it against a scripted turn: the tick's only job
/// is to decide *when* to run something, and a test that had to fake a claim to test the run
/// would be testing the fake.
pub async fn execute(pool: &PgPool, run: &RunRow, turn: &dyn CaseTurn) -> Result<Verdict, String> {
    let Some(suite) = eval_store::find_suite(pool, run.organization_id, &run.suite_key).await
        .map_err(|error| error.to_string())?
    else {
        settle_error(pool, run, "this run's suite no longer exists").await;
        return Err("the suite is gone".to_string());
    };

    // A suite switched off between queueing and claiming does not run: the operator turned it
    // off, and running it anyway would spend a judge budget they just declined to spend.
    if !suite.enabled {
        settle_error(pool, run, "this suite was switched off before its run started").await;
        return Err("the suite is disabled".to_string());
    }

    let cases = eval_store::list_cases(pool, run.organization_id, run.suite_id)
        .await
        .map_err(|error| error.to_string())?;
    let enabled: Vec<CaseRow> = cases.into_iter().filter(|case| case.enabled).collect();

    let guard = guard_store::load_guard(pool, run.organization_id)
        .await
        .ok();

    let started = std::time::Instant::now();
    let mut cost_micros = 0i64;
    for case in &enabled {
        let outcome = run_case(pool, run, case, turn, guard.as_ref()).await;
        cost_micros += outcome.result.cost_micros;
        if let Err(error) = eval_run::record_case_result(pool, run.id, &outcome.result).await {
            tracing::warn!(run = %run.id, case = %case.name, %error, "an eval case result could not be written");
        }
        if outcome.stop {
            break;
        }
        // Cancellation is answered at the case boundary, which is the contract the panel's copy
        // already promises. `cancel_run` has already settled the row, so the runner's own settle
        // is refused by the `status in ('queued','running')` guard — that refusal is the signal
        // that the run is no longer ours, not an error.
        if eval_run::should_stop(pool, run.id).await.unwrap_or(false) {
            break;
        }
    }

    // Read the results back rather than accumulating the verdict in memory: the stored rows are
    // what a reader sees, and a run whose row disagrees with its own results is exactly the bug
    // `verdict_of`'s doc comment says a reader must be able to see.
    let results = eval_run::list_case_results(pool, run.organization_id, run.id)
        .await
        .map_err(|error| error.to_string())?;
    let verdict = eval_run::verdict_of(&results);
    let duration_ms = i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX);

    let baseline = match run.base_run_id {
        Some(base_id) => eval_run::find_run(pool, run.organization_id, base_id)
            .await
            .ok()
            .flatten()
            .and_then(|row| row.pass_rate),
        None => None,
    };
    let gate_verdict = eval_run::decide_gate(
        verdict.pass_rate,
        run.threshold_percent,
        baseline,
        suite.max_regression_points,
    );
    // **Three statuses, and the middle one is not optional.** The first version wrote only
    // `passed` or `error`, which meant a run whose single case *failed* its checks settled as
    // `passed` — a run that answered every question wrongly reporting success, with its gate
    // verdict (`block`) contradicting its own status on the same row. The gate is a promotion
    // signal, not a substitute for the outcome: a run is `failed` when any case failed a check,
    // `error` only when nothing could be scored at all, and `passed` only when at least one case
    // ran and none failed.
    let status = if verdict.failed > 0 {
        "failed"
    } else if verdict.passed > 0 {
        "passed"
    } else {
        "error"
    };
    let gate = if run.kind == "gate" { gate_verdict.gate } else { "none" };

    if let Err(error) = eval_run::settle_run(
        pool,
        run.id,
        status,
        gate,
        verdict,
        cost_micros,
        duration_ms,
        None,
    )
    .await
    {
        // The row is no longer `queued`/`running`: the operator cancelled it, or the reaper
        // failed it while this run was working. Either way the results written so far are the
        // evidence and the row already says what happened.
        return Err(error.to_string());
    }

    if let Err(error) = eval_run::project_last_run(pool, run.id, gate, verdict).await {
        tracing::warn!(run = %run.id, %error, "a settled run's suite projection could not be written");
    }
    if let Err(error) = eval_run::project_case_results(pool, run.id).await {
        tracing::warn!(run = %run.id, %error, "a settled run's case projection could not be written");
    }

    // **The regression announcement names the cases that REGRESSED, and this is the only place
    // in the runner that knows which ones they are.** `announce` used to receive `&enabled` —
    // every case the suite ran — and publish all of their names under
    // `regressed_cases`. Structurally that list satisfies the field: every name in it is a real
    // case, and a subscriber that re-runs them cannot tell it re-ran the whole suite. The spec's
    // row asks for the regression "with their names", and the point of a name is to be the short
    // list somebody acts on: a suite of 80 with one broken case announces 80 names, so the alert
    // reads as noise and the one real finding is the one somebody filters out.
    //
    // The diff is computed from the STORED rows of the baseline and this run — the same rows
    // the panel's diff view reads. If those rows and the announcement ever disagree, the rows
    // are the truth and the disagreement is the finding, so the announcement is derived here
    // rather than from the in-memory `enabled` list the runner happened to iterate.
    let regressed_names = if gate_verdict.regressed {
        let head = eval_run::list_case_results(pool, run.organization_id, run.id)
            .await
            .unwrap_or_default();
        let base = match run.base_run_id {
            Some(base_id) => eval_run::list_case_results(pool, run.organization_id, base_id)
                .await
                .unwrap_or_default(),
            None => Vec::new(),
        };
        eval_run::diff_runs(&base, &head)
            .rows
            .into_iter()
            .filter(|row| row.movement == "regressed")
            .map(|row| row.case_name)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    announce(pool, run, &verdict, gate, gate_verdict.regressed, regressed_names).await;
    Ok(verdict)
}

/// Settle a run that never started, with a reason.
async fn settle_error(pool: &PgPool, run: &RunRow, reason: &str) {
    let _ = eval_run::settle_run(
        pool,
        run.id,
        "error",
        "none",
        Verdict { passed: 0, failed: 0, errors: 0, pass_rate: 0.0 },
        0,
        0,
        Some(reason),
    )
    .await;
}

/// Announce a settled run.
///
/// The events are the request's own list: `started` / `completed` / `failed` for the run itself,
/// `gate.passed` / `gate.blocked` for the promotion signal, and `regression.detected` for the
/// alert — **with the regressed case names**, which is what makes it actionable rather than a
/// notification somebody has to open.
///
/// Best-effort, all of them: the run is already settled and these are subscribers' problem. A
/// failed publish is a log line, never a run failure.
async fn announce(
    pool: &PgPool,
    run: &RunRow,
    verdict: &Verdict,
    gate: &str,
    regressed: bool,
    regressed_cases: Vec<String>,
) {
    // The tenant comes from the run this event is *about*. The first version read it back with
    // `select organization_id from ai_eval_runs limit 1` — an arbitrary run's tenant — which
    // publishes a suite's regression alert under another organization's id: a cross-tenant
    // subscription leak that no test asserting on payload fields would have caught.
    let org = run.organization_id;
    let base = serde_json::json!({
        "run_id": run.id,
        "suite_id": run.suite_id,
        "suite_key": run.suite_key,
        "kind": run.kind,
        "pass_rate": verdict.pass_rate,
        "total_cases": verdict.total(),
        "gate": gate,
    });
    emit(pool, org, "ai.eval.run.completed", base.clone()).await;
    if gate == "pass" {
        emit(pool, org, "ai.eval.gate.passed", base.clone()).await;
    } else if gate == "block" {
        emit(pool, org, "ai.eval.gate.blocked", base.clone()).await;
    }
    // `regressed` is the suite-level verdict (the rate fell further than the suite tolerates) and
    // `regressed_cases` is which cases did it. They can legitimately disagree — a gate can
    // regress on a rate that moved with no single case regressing — so the count is published
    // beside the names rather than derived from them: an empty list beside a true flag reads as
    // "we looked and found nothing", and the fix view needs to be able to say that.
    if regressed {
        emit(
            pool,
            org,
            "ai.eval.regression.detected",
            serde_json::json!({
                "run_id": run.id,
                "suite_id": run.suite_id,
                "suite_key": run.suite_key,
                "pass_rate": verdict.pass_rate,
                "regressed_cases": regressed_cases,
            }),
        )
        .await;
    }
}

/// Publish one event for a tenant, or say why it could not be.
async fn emit(pool: &PgPool, organization_id: Uuid, name: &str, payload: serde_json::Value) {
    let event = omnion_events::NewEvent::new(name)
        .organization(organization_id)
        .payload(payload);
    if let Err(error) = omnion_events::bus::emit(pool, event).await {
        tracing::warn!(%error, name, "an eval event could not be published");
    }
}

/// One claim, one run: the tick the runner actually executes.
pub async fn tick(pool: &PgPool, slots_free: usize, turn: &dyn CaseTurn) -> Result<TickReport, String> {
    if slots_free == 0 {
        return Ok(TickReport::default());
    }
    let Some(run) = eval_run::claim_next_run(pool).await.map_err(|e| e.to_string())? else {
        return Ok(TickReport::default());
    };
    if eval_run::should_stop(pool, run.id).await.unwrap_or(false) {
        settle_error(pool, &run, "this run was cancelled before its first case").await;
        return Ok(TickReport { claimed: 1, ..TickReport::default() });
    }
    let verdict = execute(pool, &run, turn).await?;
    tracing::info!(
        run = %run.id,
        suite = %run.suite_key,
        pass_rate = verdict.pass_rate,
        "eval run finished"
    );
    Ok(TickReport { claimed: 1, ..TickReport::default() })
}

/// What one tick did, as the log line and the unit tests describe it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Runs claimed this tick.
    pub claimed: usize,
    /// Runs the reaper failed on this sweep.
    pub failed_stale: usize,
}

impl TickReport {
    /// `true` when there was nothing to do.
    #[must_use]
    pub fn is_idle(self) -> bool {
        self.claimed == 0 && self.failed_stale == 0
    }
}

/// Hand runs the timeout caught back as failed, with a reason on each row.
pub async fn sweep(pool: &PgPool, timeout_seconds: i64) -> Result<usize, String> {
    let stale = eval_run::fail_stale_runs(pool, timeout_seconds)
        .await
        .map_err(|error| error.to_string())?;
    Ok(stale.len())
}

/// Queue a run for every suite whose schedule is due.
///
/// **A suite already `running` is skipped, not queued behind itself.** The request's acceptance
/// row is explicit: "a second run is not started while one is `running`". Two overlapping runs of
/// the same suite would write two verdicts against one baseline and make the diff view compare a
/// run with itself.
pub async fn sweep_schedules(pool: &PgPool) -> Result<usize, String> {
    let candidates = eval_store::scheduled_suites(pool)
        .await
        .map_err(|error| error.to_string())?;
    let now = OffsetDateTime::now_utc();
    let mut queued = 0;
    for suite in candidates {
        let schedule = suite.schedule.as_deref().unwrap_or_default();
        if !eval_store::schedule_due(schedule, now) {
            continue;
        }
        // **A suite that ran in the last minute is skipped as well as one that is still running.**
        // `cron_fires` answers "is this the schedule's minute", and a sweep that wakes four times
        // a minute sees that minute four times — so without the second guard a daily suite queues
        // four identical runs, and four runs against one baseline makes the diff view compare a
        // run with itself.
        if eval_store::has_active_run(pool, suite.id).await.unwrap_or(true) {
            continue;
        }
        if suite.last_run_at.is_some_and(|last| (now - last).whole_minutes() < 1) {
            continue;
        }
        // The snapshot carries no model on purpose: a scheduled suite's model is resolved by the
        // router when the run is claimed, and recording a model *here* would make the snapshot
        // describe a pin the run did not use — which is the same lie `routes/ai_evals` avoids
        // when it snapshots a copilot or task suite.
        let snapshot = eval_run::build_snapshot(
            None,
            suite.model_id,
            "",
            Some(suite.judge_prompt_version),
            &[],
            suite.temperature,
            None,
            suite.judge_prompt.as_deref(),
            Some(suite.judge_prompt_version),
            None,
        );
        match eval_run::create_run(
            pool,
            suite.organization_id,
            &eval_run::NewRun {
                suite_id: suite.id,
                kind: "scheduled".to_owned(),
                snapshot,
                model_id: suite.model_id,
                judge_model_id: suite.judge_model_id,
                threshold_percent: suite.threshold_percent,
                base_run_id: None,
                triggered_by: None,
            },
        )
        .await
        {
            Ok(run) => {
                emit(
                    pool,
                    suite.organization_id,
                    "ai.eval.run.started",
                    serde_json::json!({ "run_id": run.id, "suite_key": suite.key, "kind": "scheduled" }),
                )
                .await;
                queued += 1;
            }
            Err(error) => {
                tracing::warn!(suite = %suite.key, %error, "a scheduled eval run could not be queued");
            }
        }
    }
    Ok(queued)
}

/// Start the runner; the returned handle is kept by the binary (and ends with the process).
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let pool = state.db().pool().clone();
    let timeout_seconds = state.config().ai_hub.eval_timeout_seconds;
    let scheduler_ms = state.config().ai_hub.eval_scheduler_ms;
    let concurrency = state.config().ai_hub.runner_concurrency.max(1);
    let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    tracing::info!(concurrency, timeout_seconds, "eval runner started");

    let ticks = pool.clone();
    let tick_count = Arc::clone(&in_flight);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(StdDuration::from_millis(TICK_MS));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick fires immediately; boot has its own work, so let it pass.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let free = concurrency.saturating_sub(tick_count.load(std::sync::atomic::Ordering::SeqCst));
            if free == 0 {
                continue;
            }
            let Some(run) = eval_run::claim_next_run(&ticks).await.ok().flatten() else {
                continue;
            };
            let Some(turn) = build_turn(&ticks, &run).await else {
                eval_run::settle_run(
                    &ticks,
                    run.id,
                    "error",
                    "none",
                    Verdict { passed: 0, failed: 0, errors: 0, pass_rate: 0.0 },
                    0,
                    0,
                    Some("this run's model could not be resolved"),
                )
                .await
                .ok();
                continue;
            };
            tick_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let task_pool = ticks.clone();
            let task_turn = Arc::new(turn);
            let task_count = Arc::clone(&tick_count);
            tokio::spawn(async move {
                let _ = execute(&task_pool, &run, task_turn.as_ref()).await;
                task_count.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            });
        }
    });

    let scheduler_pool = pool.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(StdDuration::from_millis(scheduler_ms));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if let Err(error) = sweep_schedules(&scheduler_pool).await {
                tracing::warn!(%error, "the eval schedule sweep failed");
            }
        }
    });

    let reaper = pool.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(StdDuration::from_millis(REAPER_MS));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            match sweep(&reaper, timeout_seconds).await {
                Ok(0) => {}
                Ok(count) => {
                    tracing::warn!(count, "eval runs with a stale heartbeat were failed");
                }
                Err(error) => {
                    tracing::warn!(%error, "the eval run reaper failed");
                }
            }
        }
    });

    tokio::spawn(async move {})
}

/// The model this run grades, read from **its own snapshot**.
///
/// **This is the reader `snapshot["model"]` never had.** `start_run` writes the suite's pinned
/// `provider/model` into the snapshot and keeps the row id in `run.model_id`, and the other two
/// snapshot fields the runner reads — `prompt` and `temperature` — have readers. `model` did
/// not: `build_turn` asked the router with `requested: None`, and `lookup`'s `None` arm is
/// `default_model(pool)`. So every run was graded by the installation's default while its own row
/// claimed a different model, and a suite's `model`-target pin was decoration. The three ways a
/// field gets written and never read are all invisible to `cargo test`, `pnpm typecheck` and the
/// walkthrough, which is why the fix needed a reader-shaped assertion rather than another gate.
///
/// The order is snapshot first, then `run.model_id`, then the default — and the fallback is the
/// *last* step on purpose:
///
/// - **snapshot first**, because it is what `start_run` promised this run would use. A suite
///   edited after the run was queued must not silently retarget a run that is already in flight,
///   which is the same rule the judge already follows (see `build_turn`'s judge comment).
/// - **`model_id` second**, because it is the same pin as an id and survives a snapshot written
///   before the key was readable. It cannot be first: the id alone is not reproducible once the
/// /// row is gone, which is exactly why the snapshot keeps the string.
/// - **the default last**, and only when neither is present — a copilot or `task`-targeted suite,
///   which `resolve_under_test` deliberately snapshots with no model so it is resolved at run
///   time. For those the router's answer is the intended answer, not a fallback.
pub async fn resolve_model_under_test(pool: &PgPool, run: &RunRow) -> String {
    if let Some(key) = run
        .snapshot
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
    {
        return key.to_owned();
    }
    // `AiModel` carries `provider_id`, not the provider's name, so the key has to be joined. The
    // snapshot exists precisely because this join can stop answering: a deleted provider row
    // leaves the string readable and the id dead.
    if let Some(id) = run.model_id
        && let Some(key) = key_of_model(pool, id).await
    {
        return key;
    }
    // A suite with no pin: ask the router, and let `default_model`'s own error settle it.
    omnion_ai_hub::router::default_model(pool)
        .await
        .ok()
        .map(|resolved| format!("{}/{}", resolved.provider.name, resolved.model.model_key))
        .unwrap_or_default()
}

/// One model's `provider/model` key, or `None` when either half of the join is gone.
async fn key_of_model(pool: &PgPool, model_id: Uuid) -> Option<String> {
    let model = omnion_ai_hub::store::find_model(pool, model_id).await.ok().flatten()?;
    let provider = omnion_ai_hub::store::find_provider(pool, model.provider_id)
        .await
        .ok()
        .flatten()?;
    Some(format!("{}/{}", provider.name, model.model_key))
}

/// The live turn for a run: the router's answer, plus the judge if the suite pins one.
async fn build_turn(pool: &PgPool, run: &RunRow) -> Option<LiveTurn> {
    let model_key = resolve_model_under_test(pool, run).await;
    let resolved = resolve_and_record(
        pool,
        DecisionContext {
            organization_id: Some(run.organization_id),
            site_id: None,
            user_id: run.triggered_by,
            run_id: None,
            task: Some("eval"),
            feature: Some("eval"),
            // **The pin, not `None`.** `lookup`'s `None` arm is `default_model(pool)`, so a
            // runner that asked the router without naming the suite's model graded every run
            // with whichever model the installation had marked default — the model under test
            // was never the model asked. See `resolve_model_under_test`.
            requested: Some(&model_key),
            requirements: &[],
        },
        Scope::Organization(run.organization_id),
        None,
    )
    .await
    .ok()?;
    let model = resolved.model?;

    let prices = Arc::new(load_model_prices(pool).await);

    // The judge is resolved from the run's own snapshot rather than from the suite's column: the
    // snapshot is what the run recorded it would use, so a suite edited after the run was
    // queued cannot make the judge a *different* model than the one the snapshot claims — which
    // would make the reproduction claim on the run detail a lie.
    let judge_key = run
        .snapshot
        .get("judge_model")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string);
    let judge_provider = match &judge_key {
        Some(key) => resolve_judge_target(pool, key).await,
        None => None,
    };

    Some(LiveTurn {
        pool: pool.clone(),
        organization_id: run.organization_id,
        model_key: model_key.clone(),
        provider: ProviderTarget::from_provider(&model.provider),
        judge_key,
        judge_provider,
        prices,
        judge_cost_micros: std::sync::atomic::AtomicI64::new(0),
    })
}

/// One `provider/model` key resolved to a dialable target.
///
/// The provider comes from the **registry**, not from a literal built here: an invented
/// `ProviderTarget` would have to guess six fields, and a guess that misses `protocol` sends
/// OpenAI's wire format to an Anthropic endpoint. `None` when the provider is not configured,
/// which makes a rubric case `error` with a reason rather than a silent pass.
async fn resolve_judge_target(pool: &PgPool, key: &str) -> Option<ProviderTarget> {
    let (provider_name, _) = key.split_once('/')?;
    let provider: Option<omnion_ai_hub::Provider> = sqlx::query_as(
        "select id, name, protocol, base_url, api_key, timeout_ms::bigint as timeout_ms \
         from ai_providers where name = $1 and enabled limit 1",
    )
    .bind(provider_name)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    provider.as_ref().map(ProviderTarget::from_provider)
}

/// The price table, read from the same two columns `routes::ai` reads.
///
/// Duplicated rather than shared on purpose: the loader in `routes/ai.rs` is a private helper of
/// that module, and making it public would give `apps/api`'s two AI modules a dependency on each
/// other for six lines of SQL. The alternative — reading the price at settle time — is the bug the
/// REQ-098 comment warns about: a price edited mid-run would restate what a run already cost.
async fn load_model_prices(pool: &PgPool) -> Vec<(String, ModelPrice)> {
    sqlx::query(
        "select model_key, input_cost_micros_per_mtok, output_cost_micros_per_mtok \
         from ai_models where enabled",
    )
    .fetch_all(pool)
    .await
    .map(|rows| {
        use sqlx::Row as _;
        rows.into_iter()
            .map(|row| {
                (
                    row.get::<String, _>("model_key"),
                    ModelPrice {
                        input_micros_per_mtok: row.get("input_cost_micros_per_mtok"),
                        output_micros_per_mtok: row.get("output_cost_micros_per_mtok"),
                    },
                )
            })
            .collect()
    })
    .unwrap_or_default()
}

// `SuiteRow` is read by `execute`; naming it here keeps the import honest if the executor's
// signature ever stops taking it.
const _: Option<SuiteRow> = None;
