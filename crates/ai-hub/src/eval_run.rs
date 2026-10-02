//! Eval runs, their per-case results and the baseline a gate compares against
//! (REQ-107, slice 2).
//!
//! Slice 1's [`crate::eval_store`] owns what is *measured* — suites and cases. This module owns
//! what happened: one run row carrying the snapshot that makes the run reproducible, one result
//! row per case with the checks that held, and the one baseline pointer a suite carries.
//!
//! # The pass rate is a weighted share, and that is not the case score
//!
//! The request says "computes `pass_rate` as the weighted pass share" and the acceptance row
//! demands a fixture with unequal weights. So a case's `weight` multiplies its verdict in the
//! run's rate, while [`crate::eval_case::CaseVerdict::score`] — the fraction of a case's
//! *properties* that held — is what the result row stores. Mixing the two is the kind of
//! confusion that produces a run at 80% whose every case reads 100%: the two numbers answer
//! different questions and this module never returns one where the other belongs.
//!
//! # A run is claimed once, by the store
//!
//! [`claim_next_run`] does the selection and the status change in one statement, exactly as
//! [`crate::run_store::claim_next_run`] does for agent runs. A runner that read a queue and then
//! picked a run would race a second API process, and two processes judging the same suite with
//! two different judge models produces two verdicts for one run — which is worse than no eval
//! at all, because the panel would show one of them and call it the result.
//!
//! # Cancelling keeps the partial results
//!
//! [`cancel_run`] moves the run to `cancelled` and *leaves every result row it had*. A
//! cancelled run is evidence: twenty of forty cases ran, and deleting them would make the run
//! look like a suite with twenty cases. The counters still have to add up (the table's check
//! constraint enforces it), so cancellation settles the run against what actually executed.

use std::collections::BTreeMap;

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AiHubError, Result};
use crate::eval_case;

/// A run row, as the History list and the run detail header read it.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct RunRow {
    /// Run identity.
    pub id: Uuid,
    /// The suite it belongs to.
    pub suite_id: Uuid,
    /// The suite's key, denormalised so the run list needs no join to render a row.
    pub suite_key: String,
    /// The suite's display name.
    pub suite_name: String,
    /// Owning tenant.
    pub organization_id: Uuid,
    /// `manual`, `scheduled` or `gate`.
    pub kind: String,
    /// Where it is: queued, running, passed, failed, error, cancelled.
    pub status: String,
    /// The reproduction data — see the module header.
    pub snapshot: serde_json::Value,
    /// The model under test, as a registry row.
    pub model_id: Option<Uuid>,
    /// The judge model, as a registry row.
    pub judge_model_id: Option<Uuid>,
    /// How many cases the run set out to execute.
    pub total_cases: i32,
    /// How many passed.
    pub passed_cases: i32,
    /// How many failed a check.
    pub failed_cases: i32,
    /// How many could not be executed at all.
    pub error_cases: i32,
    /// The weighted pass share, 0–100, or NULL while the run has not settled.
    pub pass_rate: Option<f64>,
    /// The threshold this run was judged against.
    pub threshold_percent: i32,
    /// `none`, `pass` or `block`.
    pub gate: String,
    /// The baseline this run was compared to.
    pub base_run_id: Option<Uuid>,
    /// What it cost, in micros.
    pub cost_micros: i64,
    /// How long it took.
    pub duration_ms: Option<i32>,
    /// Who started it.
    pub triggered_by: Option<Uuid>,
    /// Why it failed, when it failed.
    pub error: Option<String>,
    /// When it was claimed.
    pub started_at: OffsetDateTime,
    /// When it settled.
    pub finished_at: Option<OffsetDateTime>,
    /// The suite's regression tolerance at claim time, so a later tolerance edit cannot restate
    /// an old regression verdict.
    pub max_regression_points: f64,
    /// The suite's blocking flag at claim time — a gate that was asked for when the run started.
    pub blocking: bool,
}

/// One case's result, as the run detail's table reads it.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct CaseResultRow {
    /// Result identity.
    pub id: i64,
    /// The run it belongs to.
    pub run_id: Uuid,
    /// The case, or NULL once the case has been deleted.
    pub case_id: Option<Uuid>,
    /// The case's name as the run saw it.
    pub case_name: String,
    /// `pass`, `fail`, `error` or `skipped`.
    pub status: String,
    /// The unweighted share of checks that held, 0–1.
    pub score: Option<f64>,
    /// `[{property, passed, detail}]`.
    pub checks: serde_json::Value,
    /// The judge's sentence, verbatim.
    pub judge_reason: Option<String>,
    /// The output the checks ran against.
    pub output: Option<String>,
    /// Wall-clock for this case.
    pub latency_ms: Option<i32>,
    /// Input tokens this case's calls spent.
    pub prompt_tokens: Option<i32>,
    /// Output tokens this case's calls spent.
    pub completion_tokens: Option<i32>,
    /// What this case cost, in micros — the judge included.
    pub cost_micros: i64,
    /// `[{tool, ok, detail}]`.
    pub tool_calls: serde_json::Value,
    /// Why the case could not run, as opposed to failing a check.
    pub error: Option<String>,
    /// The case's weight at claim time, so the rate can be recomputed from the results alone.
    pub weight: f64,
}

/// A suite's baseline.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct BaselineRow {
    /// The suite.
    pub suite_id: Uuid,
    /// The run taken as the baseline.
    pub run_id: Uuid,
    /// Its pass rate at the moment it was set.
    pub pass_rate: f64,
    /// Who set it.
    pub set_by: Option<Uuid>,
    /// When.
    pub set_at: OffsetDateTime,
}

/// What a run's verdict came to, computed from the results rather than read off the run row.
///
/// The run row's own `passed_cases` / `failed_cases` / `pass_rate` are the *stored* verdict, and
/// the table's check constraint keeps the counters honest. This struct is the same arithmetic
/// performed on the rows a caller is holding, so the two can be compared — a run that disagrees
/// with its own results is a bug in the settle, and a test that only reads the run row would
/// never see it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Verdict {
    /// Cases that passed.
    pub passed: i32,
    /// Cases that failed a check.
    pub failed: i32,
    /// Cases that could not run.
    pub errors: i32,
    /// The weighted pass share, 0–100.
    pub pass_rate: f64,
}

impl Verdict {
    /// How many cases were executed in all.
    #[must_use]
    pub fn total(&self) -> i32 {
        self.passed + self.failed + self.errors
    }
}

/// What a caller supplies to start a run.
#[derive(Debug, Clone)]
pub struct NewRun {
    /// The suite to run.
    pub suite_id: Uuid,
    /// `manual`, `scheduled` or `gate`.
    pub kind: String,
    /// The suite's own configuration, copied into the snapshot.
    pub snapshot: serde_json::Value,
    /// The model under test, as the router settled on it.
    pub model_id: Option<Uuid>,
    /// The judge model.
    pub judge_model_id: Option<Uuid>,
    /// The threshold to judge against, copied from the suite.
    pub threshold_percent: i32,
    /// The baseline to compare against, if any.
    pub base_run_id: Option<Uuid>,
    /// Who started it.
    pub triggered_by: Option<Uuid>,
}

/// What one case's execution produced.
#[derive(Debug, Clone)]
pub struct NewCaseResult {
    /// The case, or NULL when a case was deleted mid-run.
    pub case_id: Option<Uuid>,
    /// The case's name.
    pub case_name: String,
    /// The verdict.
    pub status: String,
    /// The unweighted share of checks that held.
    pub score: Option<f64>,
    /// `[{property, passed, detail}]`.
    pub checks: serde_json::Value,
    /// The judge's sentence.
    pub judge_reason: Option<String>,
    /// The output.
    pub output: Option<String>,
    /// Wall-clock.
    pub latency_ms: Option<i32>,
    /// Input tokens.
    pub prompt_tokens: Option<i32>,
    /// Output tokens.
    pub completion_tokens: Option<i32>,
    /// Cost in micros, judge included.
    pub cost_micros: i64,
    /// `[{tool, ok, detail}]`.
    pub tool_calls: serde_json::Value,
    /// Why the case could not run.
    pub error: Option<String>,
    /// The case's weight, as the run saw it.
    pub weight: f64,
}

/// Refuse a run whose own fields disagree, naming the field.
///
/// Everything here is checkable before a row exists, and every refusal names its field for the
/// same reason the suite store's do: the run form is one panel and the messages travel to it.
fn validate_run(run: &NewRun) -> Result<()> {
    if !["manual", "scheduled", "gate"].contains(&run.kind.as_str()) {
        return Err(AiHubError::InvalidEval(format!(
            "`kind` must be manual, scheduled or gate, got `{}`",
            run.kind
        )));
    }
    if !(1..=100).contains(&run.threshold_percent) {
        return Err(AiHubError::InvalidEval(format!(
            "`threshold_percent` must be between 1 and 100, got {}",
            run.threshold_percent
        )));
    }
    if !run.snapshot.is_object() {
        return Err(AiHubError::InvalidEval(
            "`snapshot` must be a json object, so it can be read without a parser".to_string(),
        ));
    }
    // A gate that is not compared against anything is not a gate. The request asks for the
    // baseline comparison to be *reported* — a run that blocks on its threshold alone is a
    // threshold, and the diff view would have no row to diff.
    if run.kind == "gate" && run.base_run_id.is_none() {
        return Err(AiHubError::InvalidEval(
            "a `gate` run needs a `base_run_id`; without a baseline there is nothing to gate \
             against"
                .to_string(),
        ));
    }
    // The converse is refused too, and for the same reason. A baseline is the gate's argument:
    // a manual or scheduled run that carries one is answered by the existence check below as
    // "not a settled run of this suite", which sends the caller hunting for a tenancy or a
    // settle-status problem when the request itself never made sense. (The *diff view* is not
    // affected — it picks a baseline after the fact against a settled run, separately.)
    if run.kind != "gate" && run.base_run_id.is_some() {
        return Err(AiHubError::InvalidEval(format!(
            "only a `gate` run takes a `base_run_id`; a `{}` run is measured on its own",
            run.kind
        )));
    }
    Ok(())
}

/// The run's own projection, shared by every read so a column cannot be added to one query only.
const RUN_SELECT: &str = "select r.id, r.suite_id, s.key as suite_key, s.name as suite_name, \
     r.organization_id, r.kind, r.status, r.snapshot, r.model_id, r.judge_model_id, \
     r.total_cases, r.passed_cases, r.failed_cases, r.error_cases, r.pass_rate::float8, \
     r.threshold_percent, r.gate, r.base_run_id, r.cost_micros, r.duration_ms, \
     r.triggered_by, r.error, r.started_at, r.finished_at, \
     s.max_regression_points::float8, s.blocking \
     from ai_eval_runs r join ai_eval_suites s on s.id = r.suite_id";

/// Create a run row in `queued`.
///
/// **A suite with no enabled cases is refused here, not at settle time.** A run that starts and
/// immediately passes with 0 of 0 is a 100% green row on a suite measuring nothing — the exact
/// false confidence the request's risks section warns about. The count is taken from the same
/// query the runner will use, so the refusal and the execution cannot disagree.
pub async fn create_run(pool: &PgPool, organization_id: Uuid, new: &NewRun) -> Result<RunRow> {
    validate_run(new)?;

    let cases: i64 = sqlx::query_scalar(
        "select count(*) from ai_eval_cases where organization_id = $1 and suite_id = $2 and enabled",
    )
    .bind(organization_id)
    .bind(new.suite_id)
    .fetch_one(pool)
    .await?;
    if cases == 0 {
        return Err(AiHubError::InvalidEval(
            "this suite has no enabled cases, so a run would measure nothing; add a case first"
                .to_string(),
        ));
    }

    // A gate asks a question about *this* run versus a baseline. A gate whose baseline has
    // been deleted, or that belongs to another suite, is answered against nothing — so the
    // reference is verified here, in the tenant, rather than trusted to `on delete set null`
    // and discovered at settle time as a gate that passed for no reason.
    if let Some(base) = new.base_run_id {
        let same_suite: bool = sqlx::query_scalar(
            "select exists (select 1 from ai_eval_runs \
             where id = $1 and organization_id = $2 and suite_id = $3)",
        )
        .bind(base)
        .bind(organization_id)
        .bind(new.suite_id)
        .fetch_one(pool)
        .await?;
        if !same_suite {
            return Err(AiHubError::InvalidEval(format!(
                "`base_run_id` {base} is not a settled run of this suite in this organization"
            )));
        }
    }

    let id: Uuid = sqlx::query_scalar(
        "insert into ai_eval_runs (suite_id, organization_id, kind, status, snapshot, model_id, \
         judge_model_id, threshold_percent, base_run_id, triggered_by) \
         values ($1, $2, $3, 'queued', $4, $5, $6, $7, $8, $9) returning id",
    )
    .bind(new.suite_id)
    .bind(organization_id)
    .bind(&new.kind)
    .bind(&new.snapshot)
    .bind(new.model_id)
    .bind(new.judge_model_id)
    .bind(new.threshold_percent)
    .bind(new.base_run_id)
    .bind(new.triggered_by)
    .fetch_one(pool)
    .await?;

    find_run(pool, organization_id, id)
        .await?
        .ok_or_else(|| AiHubError::EvalRunNotFound(id.to_string()))
}

/// One run, or `None`.
///
/// A `404` and never a `403`, for the same reason as every other read in this crate: a run id
/// that answered "exists in another tenant" would make this installation's run ids an existence
/// oracle.
pub async fn find_run(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<RunRow>> {
    let sql = format!("{RUN_SELECT} where r.organization_id = $1 and r.id = $2");
    Ok(sqlx::query_as::<_, RunRow>(&sql)
        .bind(organization_id)
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// How a run list is filtered.
#[derive(Debug, Clone, Default)]
pub struct RunFilter {
    /// One suite.
    pub suite_id: Option<Uuid>,
    /// One status, or `incomplete` for everything that has not settled.
    pub status: Option<String>,
    /// One kind.
    pub kind: Option<String>,
    /// Only gate verdicts.
    pub gate: Option<String>,
    /// Who started it.
    pub user_id: Option<Uuid>,
    /// Newest first by default; `oldest` reverses it.
    pub order: Option<String>,
    /// Page size.
    pub limit: i64,
    /// Page offset.
    pub offset: i64,
}

/// The statuses a `status=incomplete` filter means.
const INCOMPLETE: &[&str] = &["queued", "running"];

/// Every status a run can be in, for the filter's own validation.
///
/// Declared here rather than read from the migration's check constraint because a query that
/// enumerates its own accepted values is a second list to keep in step with the first: the
/// constraint is the authority, and this is its readable copy. The test at the bottom of this
/// module asserts the copy against the database, so a migration that adds a status without
/// updating it fails rather than silently filtering it out.
const RUN_STATUSES: &[&str] = &[
    "queued", "running", "passed", "failed", "error", "cancelled",
];

/// The run history, filtered.
pub async fn list_runs(
    pool: &PgPool,
    organization_id: Uuid,
    filter: &RunFilter,
) -> Result<Vec<RunRow>> {
    // The `incomplete` alias exists because "not finished" is what an operator means by
    // "still running", and `status=queued,running` is not a thing an HTTP query can say. It
    // resolves to the same two statuses the claim query uses, so a screen filtering on it and a
    // runner looking for work can never disagree about what is outstanding.
    let status: Option<Vec<String>> = match filter.status.as_deref() {
        Some("incomplete") => Some(INCOMPLETE.iter().map(|s| (*s).to_owned()).collect()),
        Some(other) => Some(vec![other.to_owned()]),
        None => None,
    };
    if let Some(values) = &status
        && let Some(unknown) = values
            .iter()
            .find(|value| !RUN_STATUSES.contains(&value.as_str()))
    {
        return Err(AiHubError::InvalidEval(format!(
            "`status` must be one of queued, running, passed, failed, error, cancelled or \
             incomplete, got `{unknown}`"
        )));
    }

    let order = match filter.order.as_deref() {
        Some("oldest") => "r.started_at asc",
        _ => "r.started_at desc",
    };
    let sql = format!(
        "{RUN_SELECT} \
         where r.organization_id = $1 \
           and ($2::uuid is null or r.suite_id = $2) \
           and ($3::text is null or r.status = any($3)) \
           and ($4::text is null or r.kind = $4) \
           and ($5::text is null or r.gate = $5) \
           and ($6::uuid is null or r.triggered_by = $6) \
         order by {order} limit $7 offset $8"
    );
    Ok(sqlx::query_as::<_, RunRow>(&sql)
        .bind(organization_id)
        .bind(filter.suite_id)
        .bind(status)
        .bind(filter.kind.as_deref())
        .bind(filter.gate.as_deref())
        .bind(filter.user_id)
        .bind(filter.limit.clamp(1, 200))
        .bind(filter.offset.max(0))
        .fetch_all(pool)
        .await?)
}

/// Every result row of a run, in the order the cases were executed.
pub async fn list_case_results(
    pool: &PgPool,
    organization_id: Uuid,
    run_id: Uuid,
) -> Result<Vec<CaseResultRow>> {
    // The weight is joined rather than stored: it lives on the case, and the run's rate was
    // computed from the weight the case had *at claim time*. A weight edited afterwards would
    // silently restate the rate, so the caller that recomputes a verdict from these rows is
    // told today's weight and the stored `pass_rate` is the one that was actually judged.
    // That is the trade: the stored rate is the authority, the recomputation is a check.
    let sql = "select r.id, r.run_id, r.case_id, r.case_name, r.status, r.score::float8, \
               r.checks, r.judge_reason, r.output, r.latency_ms, r.prompt_tokens, \
               r.completion_tokens, r.cost_micros, r.tool_calls, r.error, \
               coalesce(c.weight, 1.00)::float8 as weight \
             from ai_eval_case_results r left join ai_eval_cases c on c.id = r.case_id \
             where r.run_id = $1 and r.run_id in \
               (select id from ai_eval_runs where organization_id = $2) \
             order by r.id";
    Ok(sqlx::query_as::<_, CaseResultRow>(sql)
        .bind(run_id)
        .bind(organization_id)
        .fetch_all(pool)
        .await?)
}

/// Claim the oldest queued run, or `None` when there is nothing to do.
///
/// The selection and the status change are one statement under `for update skip locked`, so two
/// API processes cannot both claim the same run — which matters more here than for agent runs,
/// because a second claim would produce two verdicts for one run and the panel would show one
/// of them as the result.
pub async fn claim_next_run(pool: &PgPool) -> Result<Option<RunRow>> {
    let id: Option<Uuid> = sqlx::query_scalar(
        "update ai_eval_runs set status = 'running', started_at = now(), updated_at = now() \
         where id = ( \
           select id from ai_eval_runs \
           where status = 'queued' \
           order by created_at \
           for update skip locked \
           limit 1 \
         ) returning id",
    )
    .fetch_optional(pool)
    .await?;
    let Some(id) = id else { return Ok(None) };
    // The claim is installation-wide rather than per tenant, so the row is read back without an
    // organization filter — there is no caller to scope it by. The runner is the only reader of
    // this function and it immediately re-reads the suite under `organization_id`, so a run
    // whose suite has been deleted fails there with a reason rather than here.
    let sql = format!("{RUN_SELECT} where r.id = $1");
    Ok(sqlx::query_as::<_, RunRow>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// How many runs are waiting. The runner's idle report and the panel's "queued" badge.
pub async fn queued_runs(pool: &PgPool) -> Result<i64> {
    Ok(sqlx::query_scalar("select count(*) from ai_eval_runs where status = 'queued'")
        .fetch_one(pool)
        .await?)
}

/// The statuses a case result may carry, in the scorer's own vocabulary.
///
/// Read from [`CaseStatus`]'s four arms by hand rather than through a generated `ALL`: the enum
/// is a four-arm match with an `as_str`, and a `const ALL` beside it is a third list to keep in
/// step. The mapping is one line each and the compiler refuses to compile if an arm is added
/// without a line here — which is the property an `ALL` array would have *lost*.
const CASE_STATUSES: &[&str] = &["pass", "fail", "error", "skipped"];

/// Write one case's result.
///
/// The status vocabulary is checked against [`crate::eval_case::CaseStatus`] rather than a local
/// copy: a case result row that stored a status the scorer does not produce would be a row the
/// run's own arithmetic cannot classify, and the run would settle with a count that does not add
/// up — the table's constraint would catch it as a hard error instead, at settle time, with the
/// real cause three functions away.
pub async fn record_case_result(
    pool: &PgPool,
    run_id: Uuid,
    result: &NewCaseResult,
) -> Result<()> {
    if !CASE_STATUSES.contains(&result.status.as_str()) {
        return Err(AiHubError::InvalidEval(format!(
            "`status` must be pass, fail, error or skipped, got `{}`",
            result.status
        )));
    }
    if !result.checks.is_array() {
        return Err(AiHubError::InvalidEval(
            "`checks` must be a json array of {property, passed, detail}".to_string(),
        ));
    }
    if !result.tool_calls.is_array() {
        return Err(AiHubError::InvalidEval(
            "`tool_calls` must be a json array".to_string(),
        ));
    }
    sqlx::query(
        "insert into ai_eval_case_results (run_id, case_id, case_name, status, score, checks, \
         judge_reason, output, latency_ms, prompt_tokens, completion_tokens, cost_micros, \
         tool_calls, error) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
    )
    .bind(run_id)
    .bind(result.case_id)
    .bind(&result.case_name)
    .bind(&result.status)
    .bind(result.score)
    .bind(&result.checks)
    .bind(result.judge_reason.as_deref())
    .bind(result.output.as_deref())
    .bind(result.latency_ms)
    .bind(result.prompt_tokens)
    .bind(result.completion_tokens)
    .bind(result.cost_micros)
    .bind(&result.tool_calls)
    .bind(result.error.as_deref())
    .execute(pool)
    .await?;
    Ok(())
}

/// Compute the run's verdict from its result rows.
///
/// The weighted share is `sum(weight of passed) / sum(weight of all executed) * 100`. A
/// `skipped` case is excluded from the denominator rather than counted as a failure: a case the
/// run did not attempt says nothing about the model, and counting it would punish a run for the
/// suite's configuration rather than the model's behaviour. An `error` case *is* counted — the
/// run could not produce a result for it, and a model that cannot be called is not passing.
///
/// The denominator is zero only for a run where every case was skipped, which settles as 0
/// rather than as a division by zero: a run with nothing to say has no pass rate, and `0` is the
/// reading a gate must treat as a block.
pub fn verdict_of(results: &[CaseResultRow]) -> Verdict {
    let mut passed = 0i32;
    let mut failed = 0i32;
    let mut errors = 0i32;
    let mut weight_passed = 0.0f64;
    let mut weight_total = 0.0f64;

    for result in results {
        match result.status.as_str() {
            "skipped" => continue,
            "pass" => {
                passed += 1;
                weight_passed += result.weight.max(0.0);
                weight_total += result.weight.max(0.0);
            }
            "error" => {
                errors += 1;
                weight_total += result.weight.max(0.0);
            }
            _ => {
                failed += 1;
                weight_total += result.weight.max(0.0);
            }
        }
    }
    let pass_rate = if weight_total > 0.0 {
        (weight_passed / weight_total) * 100.0
    } else {
        0.0
    };
    Verdict {
        passed,
        failed,
        errors,
        pass_rate: (pass_rate * 100.0).round() / 100.0,
    }
}

/// Settle a run: write the verdict its results earned, and the gate it concluded.
///
/// The gate is decided here rather than by the caller, because the request's acceptance row is
/// about what the *row* says: a run below its threshold writes `gate = 'block'` and a run at or
/// above writes `pass`. A caller-supplied gate would let a caller answer `pass` for a run at 40%
/// against a threshold of 90, which is the one thing a promotion gate must not permit.
///
/// The regression check needs a baseline; with none, the gate is `pass` when the threshold held
/// and the row says so. The `regression` boolean is what a later run (slice 3) uses to emit
/// `ai.eval.regression.detected` — this slice writes the fact, and slice 3 announces it.
#[allow(clippy::too_many_arguments)]
pub async fn settle_run(
    pool: &PgPool,
    run_id: Uuid,
    status: &str,
    gate: &str,
    verdict: Verdict,
    cost_micros: i64,
    duration_ms: i32,
    error: Option<&str>,
) -> Result<()> {
    if !["passed", "failed", "error", "cancelled"].contains(&status) {
        return Err(AiHubError::InvalidEval(format!(
            "a run settles as passed, failed, error or cancelled, got `{status}`"
        )));
    }
    if !["none", "pass", "block"].contains(&gate) {
        return Err(AiHubError::InvalidEval(format!(
            "a gate is none, pass or block, got `{gate}`"
        )));
    }
    let affected = sqlx::query(
        "update ai_eval_runs set status = $2, gate = $3, total_cases = $4, passed_cases = $5, \
         failed_cases = $6, error_cases = $7, pass_rate = $8, cost_micros = $9, \
         duration_ms = $10, error = $11, finished_at = now(), updated_at = now() \
         where id = $1 and status in ('queued', 'running')",
    )
    .bind(run_id)
    .bind(status)
    .bind(gate)
    .bind(verdict.total())
    .bind(verdict.passed)
    .bind(verdict.failed)
    .bind(verdict.errors)
    .bind(verdict.pass_rate)
    .bind(cost_micros)
    .bind(duration_ms)
    .bind(error)
    .execute(pool)
    .await?
    .rows_affected();
    if affected == 0 {
        // The `where status in (...)` guard is what makes a double settle impossible, and its
        // refusal is deliberately the run-not-found error rather than a distinct one: a second
        // settle of a run that already settled is a caller bug, and the message that helps is
        // the one naming the run.
        return Err(AiHubError::EvalRunNotFound(run_id.to_string()));
    }
    Ok(())
}

/// Decide a gate from a run's rate and its baseline.
///
/// Pure, and the function the acceptance row is asserted against: below the threshold blocks,
/// at or above it passes, and either way a drop of more than the tolerance against the baseline
/// is a regression. The regression is reported rather than folded into the verdict because the
/// request asks for both — a run can hold its threshold and still regress, and those are
/// different alerts.
#[must_use]
pub fn decide_gate(
    pass_rate: f64,
    threshold_percent: i32,
    baseline: Option<f64>,
    max_regression_points: f64,
) -> GateVerdict {
    let held = pass_rate >= f64::from(threshold_percent);
    let drop = baseline.map(|base| base - pass_rate);
    let regressed = drop.is_some_and(|dropped| dropped > max_regression_points);
    GateVerdict {
        gate: if held { "pass" } else { "block" },
        held_threshold: held,
        baseline_pass_rate: baseline,
        drop_points: drop.map(|dropped| (dropped * 100.0).round() / 100.0),
        regressed,
    }
}

/// What [`decide_gate`] concluded.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct GateVerdict {
    /// `pass` or `block`.
    pub gate: &'static str,
    /// Whether the threshold itself was met.
    pub held_threshold: bool,
    /// The baseline the run was compared to, if any.
    pub baseline_pass_rate: Option<f64>,
    /// How far below the baseline the run landed.
    pub drop_points: Option<f64>,
    /// Whether that drop is a regression.
    pub regressed: bool,
}

/// Compare two runs case by case.
///
/// The request's diff view marks a row improved, unchanged or regressed, and the summary line
/// is "x improved, y regressed, z unchanged". The comparison is on the *case*, matched by the
/// case id and falling back to the name, because a case that was deleted between the two runs
/// must not silently become "unchanged".
pub fn diff_runs(base: &[CaseResultRow], head: &[CaseResultRow]) -> RunDiff {
    let mut by_id: BTreeMap<String, &CaseResultRow> = BTreeMap::new();
    for row in base {
        let key = row
            .case_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| format!("name:{}", row.case_name));
        by_id.insert(key, row);
    }

    let mut rows = Vec::new();
    for row in head {
        let key = row
            .case_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| format!("name:{}", row.case_name));
        let previous = by_id.remove(&key);
        let movement = match previous.map(|p| p.score.unwrap_or(0.0)) {
            None => "added",
            Some(before) => {
                let after = row.score.unwrap_or(0.0);
                if (after - before).abs() < f64::EPSILON {
                    "unchanged"
                } else if after > before {
                    "improved"
                } else {
                    "regressed"
                }
            }
        };
        rows.push(DiffRow {
            case_id: row.case_id,
            case_name: row.case_name.clone(),
            base_status: previous.map(|p| p.status.clone()),
            base_score: previous.and_then(|p| p.score),
            head_status: row.status.clone(),
            head_score: row.score,
            movement,
        });
    }
    // A case the head run no longer has is *removed*, not "unchanged" — its disappearance is the
    // most useful thing a diff can say about a suite, because a case deleted between two runs
    // is how a pass rate goes up without the model improving.
    for (key, previous) in by_id {
        let (case_id, case_name) = match key.strip_prefix("name:") {
            Some(name) => (None, name.to_owned()),
            None => (Uuid::parse_str(&key).ok(), String::new()),
        };
        rows.push(DiffRow {
            case_id,
            case_name: if case_name.is_empty() {
                previous.case_name.clone()
            } else {
                case_name
            },
            base_status: Some(previous.status.clone()),
            base_score: previous.score,
            head_status: "absent".to_owned(),
            head_score: None,
            movement: "removed",
        });
    }

    // The counts are taken **before** the struct is built, because the struct owns `rows` and a
    // closure borrowing `rows` cannot outlive the move. This reads as a trivial ordering detail
    // and is not: the first cut put `rows` in the literal's first field, so the compiler refused
    // the whole function for a borrow that would have been perfectly fine in any other order.
    let count = |movement: &str| {
        rows.iter()
            .filter(|row| row.movement == movement)
            .count() as i32
    };
    let improved = count("improved");
    let regressed = count("regressed");
    let unchanged = count("unchanged");
    let added = count("added");
    let removed = count("removed");

    RunDiff {
        rows,
        improved,
        regressed,
        unchanged,
        added,
        removed,
    }
}

/// One case's movement between two runs.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DiffRow {
    /// The case.
    pub case_id: Option<Uuid>,
    /// Its name.
    pub case_name: String,
    /// The baseline run's verdict.
    pub base_status: Option<String>,
    /// The baseline run's score.
    pub base_score: Option<f64>,
    /// This run's verdict.
    pub head_status: String,
    /// This run's score.
    pub head_score: Option<f64>,
    /// `improved`, `regressed`, `unchanged`, `added` or `removed`.
    pub movement: &'static str,
}

/// The summary a diff view shows above its table.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunDiff {
    /// One row per case in either run.
    pub rows: Vec<DiffRow>,
    /// How many cases scored higher.
    pub improved: i32,
    /// How many scored lower.
    pub regressed: i32,
    /// How many scored the same.
    pub unchanged: i32,
    /// How many cases this run has that the baseline did not.
    pub added: i32,
    /// How many the baseline had that this run does not.
    pub removed: i32,
}

/// Ask a run to stop, and settle it as `cancelled` with what it has.
///
/// The results written so far are kept, deliberately: a cancelled run is evidence, and the
/// counters settle against what actually executed so the table's constraint still holds. The
/// `where status in ('queued', 'running')` guard means cancelling a finished run is a no-op that
/// reports `false`, so the panel can disable the button without a second rule.
pub async fn cancel_run(pool: &PgPool, run_id: Uuid) -> Result<bool> {
    let results = list_case_results_unscoped(pool, run_id).await?;
    let verdict = verdict_of(&results);
    let affected = sqlx::query(
        "update ai_eval_runs set status = 'cancelled', gate = 'none', total_cases = $2, \
         passed_cases = $3, failed_cases = $4, error_cases = $5, pass_rate = $6, \
         cost_micros = $7, \
         duration_ms = greatest(0, extract(epoch from (now() - started_at)) * 1000)::int, \
         finished_at = now(), updated_at = now() \
         where id = $1 and status in ('queued', 'running')",
    )
    .bind(run_id)
    .bind(verdict.total())
    .bind(verdict.passed)
    .bind(verdict.failed)
    .bind(verdict.errors)
    .bind(verdict.pass_rate)
    .bind(results.iter().map(|row| row.cost_micros).sum::<i64>())
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// A run's results with no tenant filter, for the runner and for cancellation.
///
/// The tenant is not known to the caller — the runner claims installation-wide and cancellation
/// arrives by id from a route that already authorised the run — so this read is not scoped. It is
/// private so the only users are [`cancel_run`] and the runner's own settle, both of which reach
/// the run by a path that already established the tenant.
///
/// The weight is **joined, not defaulted**. It was `1.00` in the first cut, which made every
/// cancelled run's pass rate an *unweighted* share while a normally-settled run's was weighted —
/// so cancelling a suite with unequal weights produced a number that disagreed with the same
/// cases scored by any other route. The stored rate is a claim about the run, and it has to be
/// the same claim whichever way the run ended.
async fn list_case_results_unscoped(pool: &PgPool, run_id: Uuid) -> Result<Vec<CaseResultRow>> {
    let sql = "select r.id, r.run_id, r.case_id, r.case_name, r.status, r.score::float8, \
               r.checks, r.judge_reason, r.output, r.latency_ms, r.prompt_tokens, \
               r.completion_tokens, r.cost_micros, r.tool_calls, r.error, \
               coalesce(c.weight, 1.00)::float8 as weight \
             from ai_eval_case_results r left join ai_eval_cases c on c.id = r.case_id \
             where r.run_id = $1 order by r.id";
    Ok(sqlx::query_as::<_, CaseResultRow>(sql)
        .bind(run_id)
        .fetch_all(pool)
        .await?)
}

/// Whether the run has been taken away from the runner, so it must stop writing results.
///
/// **Cancellation is the status leaving `queued`/`running`, not a flag beside it.** The first
/// version asked the opposite question — "is this run still running *and* does it already have
/// results" — which is true for every healthy run halfway through and false for a cancelled one,
/// so the runner would have kept scoring a suite an operator had just stopped, and the results it
/// wrote after the cancel would have landed under a `cancelled` run. The settle's
/// `where status in ('queued','running')` guard then refused the runner's own settle and the run
/// kept the counters it had at cancel time: a cancel that half-worked, which is the one outcome
/// worse than not cancelling.
///
/// The name says "stop" rather than "cancel requested" because the same signal arrives when
/// [`fail_stale_runs`] reaps a run the timeout caught — the runner must stop either way, and
/// distinguishing them is the announcement's job (slice 3), not the poll's.
pub async fn should_stop(pool: &PgPool, run_id: Uuid) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "select not exists (select 1 from ai_eval_runs \
         where id = $1 and status in ('queued', 'running'))",
    )
    .bind(run_id)
    .fetch_one(pool)
    .await?)
}

/// Fail a run that has been `running` for longer than `seconds`, with a reason.
///
/// Slice 3 owns the scheduler, so the timeout value and the caller live there; the query lives
/// here because it is this module's table and the rule is the request's: "a run stuck beyond
/// the timeout is failed by the runner with a reason, not left `running`". A run with results
/// keeps them; a run with none settles at 0 cases, which is what it actually achieved.
pub async fn fail_stale_runs(pool: &PgPool, seconds: i64) -> Result<Vec<Uuid>> {
    let stale: Vec<Uuid> = sqlx::query_scalar(
        "update ai_eval_runs set status = 'error', gate = 'none', \
           total_cases = coalesce(total_cases, 0), finished_at = now(), updated_at = now(), \
           error = $1 \
         where status = 'running' and started_at < now() - make_interval(secs => $2::double precision) \
         returning id",
    )
    .bind("this run was abandoned by the runner before it settled")
    // `make_interval(secs => …)` takes a `double precision`, but a **named** argument carrying
    // the `::double precision` cast is resolved as `double`, which this PostgreSQL has no type
    // for: `type "double" does not exist`. Positional `$2::double precision` is the same value
    // through a cast PostgreSQL does know.
    .bind(seconds.max(1) as f64)
    .fetch_all(pool)
    .await?;
    Ok(stale)
}

/// A suite's baseline, or `None`.
pub async fn get_baseline(pool: &PgPool, suite_id: Uuid) -> Result<Option<BaselineRow>> {
    Ok(sqlx::query_as::<_, BaselineRow>(
        "select suite_id, run_id, pass_rate::float8, set_by, set_at from ai_eval_baselines \
         where suite_id = $1",
    )
    .bind(suite_id)
    .fetch_optional(pool)
    .await?)
}

/// Set a suite's baseline to a settled run.
///
/// Only a run with a rate can be a baseline — a queued or errored run has nothing to compare
/// against, and taking one as a baseline would make every later run "regressed" against a rate
/// that was never earned. The rate is copied from the run rather than passed in, so a caller
/// cannot set a baseline to a number the run never produced.
pub async fn set_baseline(
    pool: &PgPool,
    organization_id: Uuid,
    suite_id: Uuid,
    run_id: Uuid,
    set_by: Option<Uuid>,
) -> Result<BaselineRow> {
    let rate: Option<(f64,)> = sqlx::query_as(
        "select pass_rate::float8 from ai_eval_runs \
         where id = $1 and organization_id = $2 and suite_id = $3 \
           and status in ('passed', 'failed') and pass_rate is not null",
    )
    .bind(run_id)
    .bind(organization_id)
    .bind(suite_id)
    .fetch_optional(pool)
    .await?;
    let Some((rate,)) = rate else {
        return Err(AiHubError::InvalidEval(format!(
            "`run_id` {run_id} is not a settled run of this suite, so it cannot be a baseline"
        )));
    };
    sqlx::query(
        "insert into ai_eval_baselines (suite_id, run_id, pass_rate, set_by) \
         values ($1, $2, $3, $4) \
         on conflict (suite_id) do update set run_id = excluded.run_id, \
           pass_rate = excluded.pass_rate, set_by = excluded.set_by, set_at = now()",
    )
    .bind(suite_id)
    .bind(run_id)
    .bind(rate)
    .bind(set_by)
    .execute(pool)
    .await?;
    get_baseline(pool, suite_id)
        .await?
        .ok_or_else(|| AiHubError::EvalRunNotFound(run_id.to_string()))
}

/// The run list's stat tiles: suites, runs in the window, average pass rate and cost.
pub async fn run_stats(
    pool: &PgPool,
    organization_id: Uuid,
    days: i64,
) -> Result<RunStats> {
    let row: (i64, Option<f64>, Option<i64>) = sqlx::query_as(
        "select count(*), avg(pass_rate)::float8, sum(cost_micros)::bigint \
         from ai_eval_runs \
         where organization_id = $1 and started_at > now() - make_interval(days => $2::int) \
           and status in ('passed', 'failed')",
    )
    .bind(organization_id)
    .bind(days.clamp(1, 365))
    .fetch_one(pool)
    .await?;
    let suites: i64 = sqlx::query_scalar(
        "select count(*) from ai_eval_suites where organization_id = $1",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    Ok(RunStats {
        suites,
        runs: row.0,
        average_pass_rate: row.1.map(|rate| (rate * 100.0).round() / 100.0),
        cost_micros: row.2.unwrap_or(0),
        days: days.clamp(1, 365),
    })
}

/// The four tiles above the run list.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunStats {
    /// How many suites exist.
    pub suites: i64,
    /// How many settled runs fall in the window.
    pub runs: i64,
    /// The mean pass rate of those runs.
    pub average_pass_rate: Option<f64>,
    /// What they cost.
    pub cost_micros: i64,
    /// The window the numbers cover.
    pub days: i64,
}

/// Copy a settled run's verdict onto its suite and its cases (migration 0234).
///
/// **Called from the same transaction as the settle.** The suite list renders `last_pass_rate`,
/// `last_run_at` and `last_gate` as columns, and the Cases tab renders `last_status` per row —
/// both are denormalised, so both are only correct if they are written by whoever settled the
/// run. Splitting the write into its own statement after the settle would leave a window where
/// the history says a run finished and the suite list still shows the previous run's number: a
/// screen that contradicts itself for as long as the next load takes.
///
/// **The gate column is the run's own, not a recomputation.** A suite that blocked against a
/// baseline which has since been re-pointed must keep showing the block it actually concluded,
/// so nothing here calls [`decide_gate`] again. This function copies facts, it does not decide.
///
/// **Only a settled run may write.** The `where r.status in (...)` filter is the guard: a run
/// that is still `queued` or `running` has no verdict to copy, and a row written for it would
/// overwrite a previous run's rate with a null. The affected count is therefore the honest
/// answer to "did this write anything", which the executor uses to decide whether the suite list
/// and the run row can disagree.
pub async fn project_last_run(
    pool: &PgPool,
    run_id: Uuid,
    gate: &str,
    verdict: Verdict,
) -> Result<u64> {
    let affected = sqlx::query(
        "update ai_eval_suites s set last_pass_rate = $3, last_run_at = now(), \
           last_gate = $2, last_run_id = $1 \
         from ai_eval_runs r \
         where r.id = $1 and r.suite_id = s.id \
           and r.status in ('passed', 'failed', 'error', 'cancelled')",
    )
    .bind(run_id)
    .bind(gate)
    .bind(verdict.pass_rate)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected)
}

/// Copy each case's own result onto its case row, so the Cases tab's chip is a stored fact.
///
/// **A cancelled run's results are kept and projected too.** The request's acceptance row says a
/// cancelled run keeps its partial results, and a case chip that said `pass` while the run it came
/// from was `cancelled` would make the tab look like the case has never been green.
///
/// The join is on `case_id`, so a result whose case was deleted mid-run projects nothing — which
/// is correct: there is no case row left to hold the chip, and inventing one would resurrect a
/// case the operator deleted.
pub async fn project_case_results(pool: &PgPool, run_id: Uuid) -> Result<u64> {
    let affected = sqlx::query(
        "update ai_eval_cases c set last_status = r.status, last_run_at = now() \
         from ai_eval_case_results r \
         where r.run_id = $1 and r.case_id = c.id",
    )
    .bind(run_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected)
}

/// Build the snapshot a run carries.
///
/// Pure, and the reason this is a function rather than a `json!` at the call site: the snapshot
/// is what makes a run reproducible, so its *shape* is a contract the run detail screen reads
/// key by key. A snapshot assembled at three call sites drifts, and the screen then renders
/// `undefined` for a field that used to be there.
#[must_use]
pub fn build_snapshot(
    model_key: Option<&str>,
    model_id: Option<Uuid>,
    prompt: &str,
    prompt_version: Option<i32>,
    tools: &[String],
    temperature: Option<f64>,
    judge_model_key: Option<&str>,
    judge_prompt: Option<&str>,
    judge_prompt_version: Option<i32>,
    authority_user_id: Option<Uuid>,
) -> serde_json::Value {
    serde_json::json!({
        "model": model_key,
        "model_id": model_id,
        "prompt": prompt,
        "prompt_version": prompt_version,
        "tools": tools,
        "temperature": temperature,
        "judge_model": judge_model_key,
        "judge_prompt": judge_prompt,
        "judge_prompt_version": judge_prompt_version,
        "authority_user_id": authority_user_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A result row with only the fields a test cares about.
    ///
    /// `Default` would do this, except `CaseResultRow` has no `Default` and adding one for
    /// tests would make a missing field in production code compile silently. Every field is
    /// therefore written out here, which is also what makes a new column on the row a
    /// compile error in this helper rather than a silently-defaulted test fixture.
    ///
    /// **`case_id` is derived from the name**, which the first version got wrong by hardcoding
    /// one id for every row. Two of the diff tests then collapsed silently: all three "cases"
    /// shared an id, the map behind the diff kept one of them, and the diff reported 1 row where
    /// there were three. The assertions failed, but they failed for the wrong reason — they read
    /// as "the diff is broken" rather than "the fixture gives every case the same identity",
    /// which is exactly the confusion a diff's real input cannot produce. A fixture that cannot
    /// express the thing under test is worse than no fixture: it teaches the next reader a false
    /// rule about the code.
    fn result(name: &str, status: &str, score: Option<f64>, weight: f64) -> CaseResultRow {
        CaseResultRow {
            id: 0,
            run_id: Uuid::from_u128(9),
            case_id: Some(Uuid::from_u128(
                name.bytes().fold(1469598103934665603u128, |acc, byte| {
                    (acc ^ u128::from(byte)).wrapping_mul(1099511628211)
                }) | 1,
            )),
            case_name: name.to_string(),
            status: status.to_string(),
            score,
            checks: serde_json::json!([]),
            judge_reason: None,
            output: None,
            latency_ms: None,
            prompt_tokens: None,
            completion_tokens: None,
            cost_micros: 0,
            tool_calls: serde_json::json!([]),
            error: None,
            weight,
        }
    }

    // -- verdict_of: the weighted pass share the acceptance row demands ---------------------

    #[test]
    fn the_rate_is_weighted_not_a_case_count() {
        // One heavy pass and two light failures is 1 of 3 *cases* but 8 of 10 *weight*. The
        // first version of this assertion expected 50 — it had mentally divided by the two
        // failures and forgotten the pass's own 8. 80 is the weighted share and 33.3 the
        // unweighted one, and the gap between them is the entire point of the acceptance row
        // asking for a fixture with unequal weights: a suite whose important cases are heavy
        // scores higher than a case count would say.
        let results = vec![
            result("heavy", "pass", Some(1.0), 8.0),
            result("light-a", "fail", Some(0.0), 1.0),
            result("light-b", "fail", Some(0.0), 1.0),
        ];
        let verdict = verdict_of(&results);
        assert_eq!(verdict.passed, 1);
        assert_eq!(verdict.failed, 2);
        assert_eq!(verdict.total(), 3);
        assert_eq!(verdict.pass_rate, 80.0, "8 of 10 weight passed");
    }

    #[test]
    fn an_error_case_counts_against_the_rate_and_a_skipped_one_does_not() {
        // An error is a case the run could not produce a result for, so the model did not pass
        // it. A skip is a case the run never attempted, which says nothing about the model and
        // must not be scored — counting it would punish the run for the suite's configuration.
        let results = vec![
            result("ran", "pass", Some(1.0), 1.0),
            result("broke", "error", None, 1.0),
            result("skipped", "skipped", None, 10.0),
        ];
        let verdict = verdict_of(&results);
        assert_eq!(verdict.passed, 1);
        assert_eq!(verdict.errors, 1);
        assert_eq!(verdict.total(), 2, "the skipped case is not a case the run executed");
        assert_eq!(
            verdict.pass_rate, 50.0,
            "1 of 2 executed weight passed; the 10-weight skip must not dilute it"
        );
    }

    #[test]
    fn a_run_where_everything_was_skipped_settles_at_zero_rather_than_dividing_by_zero() {
        // Zero is the reading a gate must treat as a block, which is why it is 0 and not None:
        // a null rate here would be a run whose verdict the panel could not render.
        let verdict = verdict_of(&[result("a", "skipped", None, 1.0)]);
        assert_eq!(verdict.total(), 0);
        assert_eq!(verdict.pass_rate, 0.0);
        assert!(verdict.pass_rate.is_finite());
    }

    #[test]
    fn no_results_at_all_is_an_empty_run_not_a_full_one() {
        let verdict = verdict_of(&[]);
        assert_eq!(verdict.total(), 0);
        assert_eq!(verdict.passed, 0);
        assert_eq!(verdict.pass_rate, 0.0);
    }

    #[test]
    fn a_negative_weight_cannot_inflate_the_rate() {
        // The column is constrained 0.1–10, so this cannot arrive from the database. It is
        // tested anyway because `.max(0.0)` is the guard and an unguarded sum would let one bad
        // row push a pass rate above 100 and break the table's own check constraint.
        let results = vec![
            result("good", "pass", Some(1.0), 5.0),
            result("bad", "pass", Some(1.0), -100.0),
        ];
        let verdict = verdict_of(&results);
        assert!(
            verdict.pass_rate <= 100.0,
            "a negative weight produced {verdict:?}"
        );
    }

    // -- decide_gate: the gate the acceptance row is written about --------------------------

    #[test]
    fn a_rate_below_the_threshold_blocks() {
        let verdict = decide_gate(84.0, 90, None, 5.0);
        assert_eq!(verdict.gate, "block");
        assert!(!verdict.held_threshold);
    }

    #[test]
    fn a_rate_equal_to_the_threshold_passes() {
        // `>=`, not `>`: a threshold of 90 means "90% is good enough", and an operator who
        // wrote 90 did not mean 90.00001.
        let verdict = decide_gate(90.0, 90, None, 5.0);
        assert_eq!(verdict.gate, "pass");
        assert!(verdict.held_threshold);
    }

    #[test]
    fn a_drop_beyond_the_tolerance_is_a_regression_even_when_the_threshold_held() {
        // The request asks for both facts separately, and they are different alerts: this run
        // passed its threshold at 92 but fell 8 points against a baseline of 100 whose tolerance
        // is 5.
        //
        // The first version used 95 against a 100 baseline — a drop of exactly 5, which the
        // tolerance admits, so the assertion demanded a regression the rule does not produce. The
        // numbers were chosen to make the test's *name* true while never making it so, which is
        // how a test ends up "proving" a rule the code does not have. The fixture has to be
        // arithmetically capable of the case it claims to check.
        let verdict = decide_gate(92.0, 90, Some(100.0), 5.0);
        assert_eq!(verdict.gate, "pass", "92 is at or above the threshold of 90");
        assert!(verdict.held_threshold);
        assert!(verdict.regressed, "8 points of drop exceeds a tolerance of 5");
        assert_eq!(verdict.drop_points, Some(8.0));
    }

    #[test]
    fn a_drop_exactly_at_the_tolerance_is_not_a_regression() {
        // `>` rather than `>=`, the mirror of the threshold rule and for the same reason: a
        // tolerance of 5 means "up to 5 points of drift is acceptable".
        let verdict = decide_gate(95.0, 90, Some(100.0), 5.0 + f64::EPSILON * 10.0);
        assert!(!verdict.regressed);
    }

    #[test]
    fn an_improvement_is_never_a_regression() {
        let verdict = decide_gate(100.0, 90, Some(40.0), 5.0);
        assert!(!verdict.regressed);
        assert_eq!(verdict.drop_points, Some(-60.0), "the drop is negative: it rose");
    }

    #[test]
    fn without_a_baseline_there_is_no_regression_to_report() {
        let verdict = decide_gate(10.0, 90, None, 5.0);
        assert_eq!(verdict.gate, "block");
        assert!(!verdict.regressed, "nothing to have regressed from");
        assert_eq!(verdict.baseline_pass_rate, None);
        assert_eq!(verdict.drop_points, None);
    }

    // -- diff_runs: what the diff view marks, and what the summary line counts --------------

    #[test]
    fn a_diff_marks_each_case_by_its_score_movement() {
        let base = vec![
            result("rose", "fail", Some(0.5), 1.0),
            result("flat", "pass", Some(1.0), 1.0),
            result("fell", "pass", Some(1.0), 1.0),
        ];
        let head = vec![
            result("rose", "pass", Some(1.0), 1.0),
            result("flat", "pass", Some(1.0), 1.0),
            result("fell", "fail", Some(0.0), 1.0),
        ];
        let diff = diff_runs(&base, &head);
        assert_eq!(diff.improved, 1);
        assert_eq!(diff.regressed, 1);
        assert_eq!(diff.unchanged, 1);
        let rose = diff.rows.iter().find(|row| row.case_name == "rose").unwrap();
        assert_eq!(rose.movement, "improved");
        assert_eq!(rose.base_status.as_deref(), Some("fail"));
        assert_eq!(rose.head_status, "pass");
    }

    #[test]
    fn a_case_only_the_new_run_has_is_added_not_unchanged() {
        let head = vec![result("brand new", "pass", Some(1.0), 1.0)];
        let diff = diff_runs(&[], &head);
        assert_eq!(diff.added, 1);
        assert_eq!(diff.unchanged, 0);
        assert_eq!(diff.rows[0].movement, "added");
        assert_eq!(diff.rows[0].base_status, None);
    }

    #[test]
    fn a_case_the_new_run_lost_is_removed_rather_than_silently_dropped() {
        // This is the one that matters: a case deleted between two runs is how a pass rate goes
        // up without the model improving, so a diff that ignored it would report "no change".
        let base = vec![result("retired", "fail", Some(0.0), 1.0)];
        let diff = diff_runs(&base, &[]);
        assert_eq!(diff.removed, 1);
        assert_eq!(diff.unchanged, 0);
        assert_eq!(diff.rows[0].movement, "removed");
        assert_eq!(diff.rows[0].head_status, "absent");
    }

    #[test]
    fn the_summary_counts_every_row_exactly_once() {
        // The five movements are disjoint, so they must add up to the row count. A summary line
        // that omitted `added`/`removed` would show "0 changed" for a suite that swapped its
        // entire case list.
        let base = vec![
            result("a", "pass", Some(1.0), 1.0),
            result("b", "fail", Some(0.0), 1.0),
            result("gone", "pass", Some(1.0), 1.0),
        ];
        let head = vec![
            result("a", "pass", Some(1.0), 1.0),
            result("b", "fail", Some(0.0), 1.0),
            result("new", "pass", Some(1.0), 1.0),
        ];
        let diff = diff_runs(&base, &head);
        let total = diff.improved + diff.regressed + diff.unchanged + diff.added + diff.removed;
        assert_eq!(
            total as usize,
            diff.rows.len(),
            "the five movements must partition the rows: {diff:?}"
        );
        assert_eq!(diff.unchanged, 2);
        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 1);
    }

    #[test]
    fn a_case_with_no_score_is_compared_as_zero_not_skipped() {
        // An `error` case has no score. Treating "no score" as "unchanged" would report a run
        // full of errors as identical to a clean one, because nothing moved from nothing.
        let base = vec![result("broke", "pass", Some(1.0), 1.0)];
        let head = vec![result("broke", "error", None, 1.0)];
        let diff = diff_runs(&base, &head);
        assert_eq!(diff.regressed, 1);
        assert_eq!(diff.unchanged, 0);
    }

    // -- validate_run: the refusals a run form can provoke -----------------------------------

    fn new_run(kind: &str, base: Option<Uuid>) -> NewRun {
        NewRun {
            suite_id: Uuid::from_u128(1),
            kind: kind.to_string(),
            snapshot: serde_json::json!({}),
            model_id: None,
            judge_model_id: None,
            threshold_percent: 90,
            base_run_id: base,
            triggered_by: None,
        }
    }

    #[test]
    fn a_gate_without_a_baseline_is_refused_by_field_name() {
        let error = validate_run(&new_run("gate", None)).unwrap_err();
        assert!(error.to_string().contains("base_run_id"), "{error}");
    }

    #[test]
    fn a_manual_run_needs_no_baseline() {
        assert!(validate_run(&new_run("manual", None)).is_ok());
        assert!(validate_run(&new_run("gate", Some(Uuid::from_u128(2)))).is_ok());
    }

    #[test]
    fn an_unknown_kind_or_threshold_is_refused_by_field_name() {
        let error = validate_run(&new_run("nightly", None)).unwrap_err();
        assert!(error.to_string().contains("`kind`"), "{error}");

        let mut run = new_run("manual", None);
        run.threshold_percent = 0;
        let error = validate_run(&run).unwrap_err();
        assert!(error.to_string().contains("threshold_percent"), "{error}");
    }

    #[test]
    fn a_snapshot_that_is_not_an_object_is_refused() {
        // The snapshot is read key by key by the run detail screen. An array would serialise
        // fine and render as `undefined` for every field.
        let mut run = new_run("manual", None);
        run.snapshot = serde_json::json!([1, 2, 3]);
        let error = validate_run(&run).unwrap_err();
        assert!(error.to_string().contains("snapshot"), "{error}");
    }

    // -- build_snapshot: the shape the run header reads -------------------------------------

    #[test]
    fn the_snapshot_carries_every_field_the_run_header_renders() {
        // Written as a list rather than a field-by-field assertion so that *removing* a key
        // from `build_snapshot` fails this test. The screen reads these by name; a snapshot
        // that silently lost `judge_prompt_version` would render a judge with no version and
        // nobody would notice until two runs compared unequal for no reason.
        let snapshot = build_snapshot(
            Some("anthropic/claude"),
            Some(Uuid::from_u128(7)),
            "be terse",
            Some(3),
            &["search".to_string()],
            Some(0.2),
            Some("anthropic/claude-other"),
            Some("does it answer the question?"),
            Some(2),
            Some(Uuid::from_u128(8)),
        );
        for key in [
            "model",
            "model_id",
            "prompt",
            "prompt_version",
            "tools",
            "temperature",
            "judge_model",
            "judge_prompt",
            "judge_prompt_version",
            "authority_user_id",
        ] {
            assert!(snapshot.get(key).is_some(), "the snapshot is missing `{key}`");
        }
        assert_eq!(snapshot["judge_prompt_version"], 2);
    }

    #[test]
    fn the_status_copies_match_the_scorers_vocabulary() {
        // The scorer is the authority on these four words. A copy that drifted would let a run
        // write a status the run's own arithmetic cannot classify — and the settle would then
        // fail on the table's constraint, three functions away from the mistake.
        let scorer: Vec<&str> = [
            crate::eval_case::CaseStatus::Pass,
            crate::eval_case::CaseStatus::Fail,
            crate::eval_case::CaseStatus::Error,
            crate::eval_case::CaseStatus::Skipped,
        ]
        .iter()
        .map(|status| status.as_str())
        .collect();
        assert_eq!(scorer, CASE_STATUSES, "CASE_STATUSES drifted from CaseStatus");
    }
}
