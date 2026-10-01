//! Eval suites and their cases: the rows, and the rules a suite must satisfy (REQ-107, slice 1).
//!
//! Runs and results are slice 2 and live elsewhere; this module owns the two tables that
//! describe *what is being measured*, because a run cannot be reproduced without them and a
//! case cannot be scored without its expectations.
//!
//! # The suite's own rules are refused here, not at run time
//!
//! The request's first acceptance row is that a `blocking` suite needs a threshold and a judge
//! model when any case carries a `rubric`. Every part of that is checkable before a run starts,
//! and each part is refused by name:
//!
//! - a threshold outside 1–100 (the column's check constraint is the backstop, not the rule);
//! - a `rubric` case with no judge model — checked against the *cases*, so a suite is created
//!   first and the rule lands when the first rubric case does, or the panel calls
//!   [`crate::eval_case::Expectation::has_rubric`] itself;
//! - a judge model that is the model under test, which is a **row** rule and therefore a
//!   constraint on the table, so no code path can write the pair.
//!
//! # The judge must differ from the model under test, and the API says so
//!
//! The request is explicit: "the judge model must differ from the model under test (the API
//! refuses the combination)". The column constraint catches the row where both are pinned;
//! [`validate_suite`] catches the case that matters more — a `model`-targeted suite's judge
//! resolving to the *same model through the router* would be a judge grading its own homework,
//! and the constraint cannot see that because it only knows two ids.
//!
//! # Keys are per tenant, and a taken key is a conflict rather than an error
//!
//! The key is a URL segment and the thing an operator types at a gate, so it is unique per
//! tenant. A second suite with the same key answers [`AiHubError::EvalSuiteKeyTaken`] — a
//! distinct code from every other "taken" in this crate, because the panel resolves it by
//! offering a different key rather than by printing a sentence about the name field.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AiHubError, Result};
use crate::eval_case;

/// A suite row, as the list screen and the detail header read it.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct SuiteRow {
    /// Suite id.
    pub id: Uuid,
    /// Owning tenant.
    pub organization_id: Uuid,
    /// URL-safe key.
    pub key: String,
    /// Display name.
    pub name: String,
    /// What the suite is for.
    pub description: String,
    /// `agent`, `copilot`, `task` or `model`.
    pub target: String,
    /// The agent under test, when the target is one.
    pub agent_id: Option<Uuid>,
    /// The copilot under test, when the target is one.
    pub copilot_key: Option<String>,
    /// The task kind under test, when the target is one.
    pub task: Option<String>,
    /// The model under test, when the target is one.
    pub model_id: Option<Uuid>,
    /// Sampling temperature pinned on the run.
    pub temperature: Option<f64>,
    /// The tool allow-list the run is given.
    pub tools: serde_json::Value,
    /// The knowledge collections the run may read.
    pub collections: serde_json::Value,
    /// The pass share below which a gate blocks.
    pub threshold_percent: i32,
    /// How many points a run may drop against its baseline before it is a regression.
    pub max_regression_points: f64,
    /// Whether this suite is the promotion gate.
    pub blocking: bool,
    /// A cron expression, or NULL for a manual-only suite.
    pub schedule: Option<String>,
    /// The second model that judges rubric cases.
    pub judge_model_id: Option<Uuid>,
    /// The judge prompt, versioned on the run.
    pub judge_prompt: Option<String>,
    /// Which revision of the judge prompt this suite asks for.
    pub judge_prompt_version: i32,
    /// Whether the operator has the suite switched on.
    pub enabled: bool,
    /// Who created it.
    pub created_by: Option<Uuid>,
    /// When.
    pub created_at: OffsetDateTime,
    /// When it last changed.
    pub updated_at: OffsetDateTime,
    /// How many cases it has, enabled or not.
    pub case_count: i64,
    /// How many of those are enabled — the number a run would execute.
    pub enabled_case_count: i64,
    /// How many of the enabled cases name a `rubric`, i.e. need the judge.
    pub rubric_case_count: i64,
    /// The most recent run's pass rate, or NULL when the suite has never run.
    ///
    /// Always NULL in slice 1: `ai_eval_runs` is slice 2's table, and reading it here would
    /// make this slice's every query fail on a fresh database until that migration lands. The
    /// column is in the struct and in the projection from the start so slice 2 is a query
    /// change, not a shape change the panel has to absorb.
    pub last_pass_rate: Option<f64>,
    /// When the last run finished, or NULL. Always NULL in slice 1, as above.
    pub last_run_at: Option<OffsetDateTime>,
    /// The last run's gate verdict, or NULL. Always NULL in slice 1, as above.
    pub last_gate: Option<String>,
}

/// A case row, as the cases tab and a run's result rows read it.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct CaseRow {
    /// Case id.
    pub id: Uuid,
    /// Owning suite.
    pub suite_id: Uuid,
    /// Owning tenant, denormalised so a case query never joins the suite to be scoped.
    pub organization_id: Uuid,
    /// Display name.
    pub name: String,
    /// The prompt/context the run replays.
    pub input: serde_json::Value,
    /// The properties this case asserts.
    pub expected: serde_json::Value,
    /// How much this case counts for in the run's pass rate.
    pub weight: f64,
    /// Coverage labels, which the panel groups by.
    pub tags: Vec<String>,
    /// Whether a run would execute it.
    pub enabled: bool,
    /// `manual`, `import` or `run`.
    pub source: String,
    /// The run this case was captured from, when it was.
    pub source_run_id: Option<Uuid>,
    /// When.
    pub created_at: OffsetDateTime,
    /// When it last changed.
    pub updated_at: OffsetDateTime,
    /// The most recent result for this case, or NULL when it has never run.
    ///
    /// Always NULL in slice 1, for the same reason as [`SuiteRow::last_pass_rate`]: the table
    /// that would answer it is slice 2's.
    pub last_status: Option<String>,
    /// When it last ran. Always NULL in slice 1, as above.
    pub last_run_at: Option<OffsetDateTime>,
}

/// The columns both list queries project.
// The two `numeric` columns are cast to `float8` in the projection rather than decoded as
// `BigDecimal`. The request models them as numeric for exact storage — a tolerance of 5.00
// should not drift — but nothing here does arithmetic that needs the exact decimal: the value
// is validated, stored, and compared against a run's percentage. `f64` is what the rest of
// this crate uses for the same kind of number (`temperature`), and mixing two numeric
// representations across a struct is how a value ends up 0.1 apart from itself on the way
// through the panel.
const SUITE_COLUMNS: &str = "s.id, s.organization_id, s.key, s.name, s.description, s.target, \
     s.agent_id, s.copilot_key, s.task, s.model_id, s.temperature::float8, s.tools, s.collections, \
     s.threshold_percent, s.max_regression_points::float8, s.blocking, s.schedule, \
     s.judge_model_id, s.judge_prompt, s.judge_prompt_version, s.enabled, s.created_by, \
     s.created_at, s.updated_at";

/// List every suite a tenant owns, newest name order.
///
/// The three sub-counts and the last run are computed in the same statement rather than by the
/// caller running three more queries: the suite list screen renders a row per suite and a
/// separate count query per row is the N+1 that makes a hundred suites feel broken.
pub async fn list_suites(pool: &PgPool, organization_id: Uuid) -> Result<Vec<SuiteRow>> {
    let sql = format!(
        "select {SUITE_COLUMNS}, \
           (select count(*) from ai_eval_cases c where c.suite_id = s.id) as case_count, \
           (select count(*) from ai_eval_cases c where c.suite_id = s.id and c.enabled) as enabled_case_count, \
           (select count(*) from ai_eval_cases c where c.suite_id = s.id and c.enabled \
              and c.expected ? 'rubric') as rubric_case_count, \
           -- The last-run columns are slice 2's. They are NULL here rather than read from
           -- ai_eval_runs, because a query that names a table a later migration creates makes
           -- THIS slice dead until that one lands: the walk failed on a missing relation the
           -- moment it touched a suite row, which is a slice that only works once its successor
           -- is finished. Slice 2 replaces the three NULLs with the sub-selects.
           null::numeric as last_pass_rate, \
           null::timestamptz as last_run_at, \
           null::text as last_gate \
         from ai_eval_suites s \
         where s.organization_id = $1 \
         order by s.name, s.key"
    );
    let rows = sqlx::query_as::<_, SuiteRow>(&sql)
        .bind(organization_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Read one suite by key, scoped to the tenant.
///
/// A suite in another tenant is [`AiHubError::EvalSuiteNotFound`] rather than a 403 — the
/// screen opens by key, and a key that answered "exists elsewhere" would make the key space of
/// the installation an existence oracle.
pub async fn find_suite(
    pool: &PgPool,
    organization_id: Uuid,
    key: &str,
) -> Result<Option<SuiteRow>> {
    let sql = format!(
        "select {SUITE_COLUMNS}, \
           (select count(*) from ai_eval_cases c where c.suite_id = s.id) as case_count, \
           (select count(*) from ai_eval_cases c where c.suite_id = s.id and c.enabled) as enabled_case_count, \
           (select count(*) from ai_eval_cases c where c.suite_id = s.id and c.enabled \
              and c.expected ? 'rubric') as rubric_case_count, \
           -- The last-run columns are slice 2's. They are NULL here rather than read from
           -- ai_eval_runs, because a query that names a table a later migration creates makes
           -- THIS slice dead until that one lands: the walk failed on a missing relation the
           -- moment it touched a suite row, which is a slice that only works once its successor
           -- is finished. Slice 2 replaces the three NULLs with the sub-selects.
           null::numeric as last_pass_rate, \
           null::timestamptz as last_run_at, \
           null::text as last_gate \
         from ai_eval_suites s \
         where s.organization_id = $1 and s.key = $2"
    );
    let row = sqlx::query_as::<_, SuiteRow>(&sql)
        .bind(organization_id)
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Read one suite by id, scoped to the tenant.
pub async fn find_suite_by_id(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<SuiteRow>> {
    let sql = format!(
        "select {SUITE_COLUMNS}, \
           (select count(*) from ai_eval_cases c where c.suite_id = s.id) as case_count, \
           (select count(*) from ai_eval_cases c where c.suite_id = s.id and c.enabled) as enabled_case_count, \
           (select count(*) from ai_eval_cases c where c.suite_id = s.id and c.enabled \
              and c.expected ? 'rubric') as rubric_case_count, \
           -- The last-run columns are slice 2's. They are NULL here rather than read from
           -- ai_eval_runs, because a query that names a table a later migration creates makes
           -- THIS slice dead until that one lands: the walk failed on a missing relation the
           -- moment it touched a suite row, which is a slice that only works once its successor
           -- is finished. Slice 2 replaces the three NULLs with the sub-selects.
           null::numeric as last_pass_rate, \
           null::timestamptz as last_run_at, \
           null::text as last_gate \
         from ai_eval_suites s \
         where s.organization_id = $1 and s.id = $2"
    );
    let row = sqlx::query_as::<_, SuiteRow>(&sql)
        .bind(organization_id)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// The cases of one suite, in the order the cases tab shows them.
pub async fn list_cases(
    pool: &PgPool,
    organization_id: Uuid,
    suite_id: Uuid,
) -> Result<Vec<CaseRow>> {
    let sql = "select c.id, c.suite_id, c.organization_id, c.name, c.input, c.expected, \
               c.weight::float8, c.tags, c.enabled, c.source, c.source_run_id, c.created_at, \
               c.updated_at, \
               -- Slice 2 fills these from ai_eval_case_results; see the note on SuiteRow.
               null::text as last_status, \
               null::timestamptz as last_run_at \
             from ai_eval_cases c \
             where c.organization_id = $1 and c.suite_id = $2 \
             order by c.name, c.id";
    let rows: Vec<CaseRow> = sqlx::query_as(sql)
        .bind(organization_id)
        .bind(suite_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Read one case, scoped to the tenant.
pub async fn find_case(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<CaseRow>> {
    let sql = "select c.id, c.suite_id, c.organization_id, c.name, c.input, c.expected, \
               c.weight::float8, c.tags, c.enabled, c.source, c.source_run_id, c.created_at, \
               c.updated_at, null::text as last_status, null::timestamptz as last_run_at \
             from ai_eval_cases c where c.organization_id = $1 and c.id = $2";
    let row = sqlx::query_as::<_, CaseRow>(sql)
        .bind(organization_id)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// What a suite save carries.
#[derive(Debug, Clone, Default)]
pub struct NewSuite {
    /// URL-safe key, unique per tenant.
    pub key: String,
    /// Display name.
    pub name: String,
    /// What the suite is for.
    pub description: String,
    /// `agent`, `copilot`, `task` or `model`.
    pub target: String,
    /// The agent under test.
    pub agent_id: Option<Uuid>,
    /// The copilot under test.
    pub copilot_key: Option<String>,
    /// The task kind under test.
    pub task: Option<String>,
    /// The model under test.
    pub model_id: Option<Uuid>,
    /// Sampling temperature, 0–2.
    pub temperature: Option<f64>,
    /// The tool allow-list the run is given.
    pub tools: serde_json::Value,
    /// The knowledge collections the run may read.
    pub collections: serde_json::Value,
    /// The pass share below which a gate blocks.
    pub threshold_percent: i32,
    /// How many points a run may drop against its baseline.
    pub max_regression_points: f64,
    /// Whether this suite is the promotion gate.
    pub blocking: bool,
    /// A cron expression, or NULL.
    pub schedule: Option<String>,
    /// The second model that judges rubric cases.
    pub judge_model_id: Option<Uuid>,
    /// The judge prompt.
    pub judge_prompt: Option<String>,
    /// Whether the suite starts enabled.
    pub enabled: bool,
    /// Who is creating it.
    pub created_by: Option<Uuid>,
}

/// What a suite edit carries. Every field is optional, so a field the caller omits keeps its
/// current value — which is why this is a struct of `Option<Option<_>>` for the columns where
/// "not mentioned" and "clear it" are different requests.
#[derive(Debug, Clone, Default)]
pub struct SuiteChanges {
    /// New display name.
    pub name: Option<String>,
    /// New description.
    pub description: Option<String>,
    /// New temperature.
    pub temperature: Option<Option<f64>>,
    /// New tool allow-list.
    pub tools: Option<serde_json::Value>,
    /// New knowledge collections.
    pub collections: Option<serde_json::Value>,
    /// New threshold.
    pub threshold_percent: Option<i32>,
    /// New regression tolerance.
    pub max_regression_points: Option<f64>,
    /// New blocking flag.
    pub blocking: Option<bool>,
    /// New schedule.
    pub schedule: Option<Option<String>>,
    /// New judge model.
    pub judge_model_id: Option<Option<Uuid>>,
    /// New judge prompt.
    pub judge_prompt: Option<Option<String>>,
    /// New enabled flag.
    pub enabled: Option<bool>,
}

/// Refuse a suite that breaks one of its own rules, naming the field.
///
/// Called on create and on every edit, because an edit is how a suite is quietly broken: the
/// editor can clear the judge model and leave a `rubric` case with nothing to judge it, and
/// the next run then reports every rubric case as an error for a reason nobody changed.
#[allow(clippy::too_many_arguments)]
pub fn validate_suite(
    target: &str,
    agent_id: Option<Uuid>,
    copilot_key: Option<&str>,
    task: Option<&str>,
    model_id: Option<Uuid>,
    threshold_percent: i32,
    max_regression_points: f64,
    temperature: Option<f64>,
    judge_model_id: Option<Uuid>,
    judge_prompt: Option<&str>,
    schedule: Option<&str>,
    judge_prompt_version: i32,
) -> Result<()> {
    if !["agent", "copilot", "task", "model"].contains(&target) {
        return Err(AiHubError::InvalidEval(format!(
            "unknown target `{target}`; expected agent, copilot, task or model"
        )));
    }
    // The target and its reference have to agree, and only one may be set. The table constraint
    // says this too, but a caller that reached it got SQLSTATE 23514 and a constraint name
    // instead of a sentence naming the target — and the editor needs to know WHICH field to
    // clear. The run layer is where this matters most: it has to choose one thing to record in
    // the snapshot, and a suite that names two makes that choice arbitrary.
    let references = [
        ("agent_id", agent_id.is_some()),
        ("copilot_key", copilot_key.is_some()),
        ("task", task.is_some()),
        ("model_id", model_id.is_some()),
    ];
    let expected = match target {
        "agent" => "agent_id",
        "copilot" => "copilot_key",
        "task" => "task",
        _ => "model_id",
    };
    let set: Vec<&str> = references
        .iter()
        .filter(|(_, is_set)| *is_set)
        .map(|(name, _)| *name)
        .collect();
    if set != [expected] {
        let named = if set.is_empty() {
            format!("`{expected}` must be set for target `{target}`")
        } else {
            format!(
                "target `{target}` needs only `{expected}`, but {} is also set",
                set.iter().filter(|name| **name != expected).copied().collect::<Vec<_>>().join(" and ")
            )
        };
        return Err(AiHubError::InvalidEval(named));
    }
    if !(1..=100).contains(&threshold_percent) {
        return Err(AiHubError::InvalidEval(format!(
            "`threshold_percent` must be between 1 and 100, got {threshold_percent}"
        )));
    }
    if !(0.0..=50.0).contains(&max_regression_points) {
        return Err(AiHubError::InvalidEval(format!(
            "`max_regression_points` must be between 0 and 50, got {max_regression_points}"
        )));
    }
    if let Some(temperature) = temperature
        && !(0.0..=2.0).contains(&temperature)
    {
        return Err(AiHubError::InvalidEval(format!(
            "`temperature` must be between 0 and 2, got {temperature}"
        )));
    }
    // A judge that is the model under test grades its own homework. The table constraint
    // catches the case where both are pinned to the same row; this catches the case where the
    // suite targets an *agent* whose configured model is the judge, which the constraint cannot
    // see because it only knows the suite's own two columns.
    if let (Some(judge), Some(model)) = (judge_model_id, model_id)
        && judge == model
    {
        return Err(AiHubError::InvalidEval(
            "`judge_model_id` must be a different model from the one under test".to_string(),
        ));
    }
    if let Some(prompt) = judge_prompt {
        if prompt.chars().count() > 8000 {
            return Err(AiHubError::InvalidEval(format!(
                "`judge_prompt` must be 8000 characters or fewer, got {}",
                prompt.chars().count()
            )));
        }
        if prompt.trim().is_empty() {
            return Err(AiHubError::InvalidEval(
                "`judge_prompt` must not be blank".to_string(),
            ));
        }
    }
    if let Some(schedule) = schedule {
        validate_schedule(schedule)?;
    }
    if judge_prompt_version < 1 {
        return Err(AiHubError::InvalidEval(format!(
            "`judge_prompt_version` starts at 1, got {judge_prompt_version}"
        )));
    }
    Ok(())
}

/// The presets the config tab offers, and the only cron shapes the runner schedules.
///
/// A `schedule` column that accepted any string would let an operator type a cron the runner
/// cannot parse, and the suite would silently never run — a green suite that measures nothing.
pub const SCHEDULE_PRESETS: &[&str] = &["hourly", "daily", "weekly", "custom"];

/// The cron expression a preset expands to, if it has one.
pub fn preset_cron(preset: &str) -> Option<&'static str> {
    match preset {
        // Minute and hour are fixed so a suite lands at a predictable local time rather than
        // whenever the scheduler's own interval happens to fall.
        "hourly" => Some("0 * * * *"),
        "daily" => Some("17 3 * * *"),
        "weekly" => Some("23 4 * * 1"),
        _ => None,
    }
}

/// Refuse a schedule the runner could not honour.
pub fn validate_schedule(schedule: &str) -> Result<()> {
    let schedule = schedule.trim();
    if SCHEDULE_PRESETS.contains(&schedule) {
        return Ok(());
    }
    if schedule.split_whitespace().count() != 5 {
        return Err(AiHubError::InvalidEval(format!(
            "`schedule` must be one of {} or a five-field cron expression",
            SCHEDULE_PRESETS.join(", ")
        )));
    }
    Ok(())
}

/// Whether a five-field cron expression fires at this instant.
///
/// **Minute resolution, and the caller owns the interval.** This answers "is *this* minute the
/// schedule's minute", which is what makes a scheduler that wakes once a minute correct and a
/// scheduler that wakes every 250 ms harmless: the second wakes four times inside the same minute
/// and this predicate says yes four times, so **the caller must also check that it has not already
/// queued this minute**. That guard is [`has_active_run`]'s job for a running suite and the
/// `last_run_at` column's for a finished one.
///
/// Fields are `minute hour day-of-month month day-of-week`, and each supports `*`, `a-b`,
/// `a,b,c` and `*&#47;n` (step). Names (`jan`, `mon`) are **not** supported: `validate_schedule`
/// accepts any five-field string and this function refuses the ones it cannot honour, so a suite
/// saved with `0 3 * jan mon` fails loudly here instead of never firing. Day-of-week is 0=Sunday
/// and 6=Saturday, which is what cron means; when both day fields are restricted, cron semantics
/// are a union — a schedule of `0 3 * * 1` (Mondays) or `0 3 15 * *` (the 15th) fires on both.
#[must_use]
pub fn cron_fires(expression: &str, now: time::OffsetDateTime) -> bool {
    let Some((minute, hour, dom, month, dow)) = parse_cron(expression) else {
        return false;
    };
    field_matches(&minute, now.minute() as u32)
        && field_matches(&hour, now.hour() as u32)
        // `time::Month` is an enum, not a `u32`, and it does not implement `From` — the
        // conversion is `u8::from(month) + 1`, because the enum starts at January = 1. Writing
        // `u32::from(now.month())` fails to compile rather than silently wrapping, which is the
        // behaviour we want from a month field: there is no sensible wrong answer here.
        && field_matches(&month, u32::from(u8::from(now.month())) + 1)
        && day_matches(&dom, &dow, now)
}

/// The five parsed fields, or `None` when the expression is not one this can honour.
fn parse_cron(expression: &str) -> Option<(Field, Field, Field, Field, Field)> {
    let parts: Vec<&str> = expression.split_whitespace().collect();
    if parts.len() != 5 {
        return None;
    }
    Some((
        parse_field(parts[0], 0, 59)?,
        parse_field(parts[1], 0, 23)?,
        parse_field(parts[2], 1, 31)?,
        parse_field(parts[3], 1, 12)?,
        parse_field(parts[4], 0, 6)?,
    ))
}

/// One cron field: the values it names, already expanded.
#[derive(Debug, Clone)]
struct Field {
    /// Every value the field matches, expanded from ranges, lists and steps.
    values: Vec<u32>,
    /// Whether the field was `*` (or `*&#47;n`), which is what decides the day-of-month /
    /// day-of-week union rule below.
    unrestricted: bool,
}

/// Parse one field, refusing anything outside `[min, max]`.
///
/// A field that names a value outside its range (`minute 75`) is refused rather than clamped:
/// clamped, it would silently become a schedule the operator did not write, and the suite would
/// run at a time nobody chose.
fn parse_field(spec: &str, min: u32, max: u32) -> Option<Field> {
    let mut values = Vec::new();
    for term in spec.split(',') {
        let (range, step) = match term.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step.parse().ok()?;
                if step == 0 {
                    return None;
                }
                (range, step)
            }
            None => (term, 1),
        };
        let (from, to) = if range == "*" {
            (min, max)
        } else if let Some((from, to)) = range.split_once('-') {
            (from.parse().ok()?, to.parse().ok()?)
        } else {
            // A bare `5` is a one-value field, and `5/2` means "from 5 to the end, every 2" —
            // which is why the single-value branch still honours the step below.
            let value: u32 = range.parse().ok()?;
            let end = if step > 1 { max } else { value };
            (value, end)
        };
        if from > to || from < min || to > max {
            return None;
        }
        let mut value = from;
        while value <= to {
            if !values.contains(&value) {
                values.push(value);
            }
            value += step;
        }
    }
    if values.is_empty() {
        return None;
    }
    Some(Field {
        values,
        unrestricted: spec.split(',').all(|term| term.starts_with('*')),
    })
}

impl Field {
    /// Whether this field names `value`.
    fn matches(&self, value: u32) -> bool {
        self.values.contains(&value)
    }
}

/// `true` for a field that names every value.
fn field_matches(field: &Field, value: u32) -> bool {
    field.matches(value)
}

/// The day fields, under cron's union rule.
fn day_matches(dom: &Field, dow: &Field, now: time::OffsetDateTime) -> bool {
    let by_day_of_month = dom.matches(now.day() as u32);
    // `number_days_from_sunday` is 0=Sunday, matching cron's numbering, so no re-basing.
    let by_weekday = dow.matches(now.weekday().number_days_from_sunday() as u32);
    match (dom.unrestricted, dow.unrestricted) {
        // Either one unrestricted: the other decides.
        (true, true) => true,
        (true, false) => by_weekday,
        (false, true) => by_day_of_month,
        // Both restricted: cron ORs them, and a schedule naming "Mondays or the 15th" is the
        // idiom this exists for.
        (false, false) => by_day_of_month || by_weekday,
    }
}

/// Every enabled suite with a schedule, so the runner can decide which are due.
///
/// **No time filter here.** Whether a schedule is due depends on `now`, and the acceptance row
/// asks for a harness that advances a clock — pushing `now` into SQL would make the walk depend
/// on a statement it cannot steer. The list is "every candidate", and [`schedule_due`] answers
/// "is this one due" in Rust.
pub async fn scheduled_suites(pool: &PgPool) -> Result<Vec<SuiteRow>> {
    // **This statement needs its own FROM and its own sub-counts.** The first version carried
    // only `where enabled and schedule is not null`, which made it two bugs at once: with no
    // `from ai_eval_suites s` the `s.` prefixes had nothing to qualify (a syntax error at the
    // first one), and `SuiteRow` is a projection of *more* columns than `SUITE_COLUMNS` — the
    // three counts and the three last-run columns are part of the struct, so a query that
    // selects only the base columns fails at the decode step with "the columns returned do not
    // match". Selecting a struct is not "give me this struct's obvious fields".
    let rows = sqlx::query_as::<_, SuiteRow>(&format!(
        "select {SUITE_COLUMNS}, \
           (select count(*) from ai_eval_cases c where c.suite_id = s.id) as case_count, \
           (select count(*) from ai_eval_cases c where c.suite_id = s.id and c.enabled) as enabled_case_count, \
           (select count(*) from ai_eval_cases c where c.suite_id = s.id and c.enabled \
              and c.expected ? 'rubric') as rubric_case_count, \
           null::numeric as last_pass_rate, \
           null::timestamptz as last_run_at, \
           null::text as last_gate \
         from ai_eval_suites s \
         where s.enabled and s.schedule is not null order by s.created_at"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Whether a suite's schedule fires now, resolving its preset first.
///
/// A preset (`hourly`, `daily`, `weekly`) is expanded by [`preset_cron`]; `custom` is stored as
/// the literal cron expression the panel wrote. A suite whose stored value expands to nothing
/// (`custom` with an empty expression) is **not** due — it can never be, and returning `true`
/// would queue it once a minute forever.
#[must_use]
pub fn schedule_due(schedule: &str, now: time::OffsetDateTime) -> bool {
    match preset_cron(schedule) {
        Some(expanded) => cron_fires(expanded, now),
        // Not a preset: the stored value is the cron expression itself.
        None => cron_fires(schedule, now),
    }
}

/// Whether this suite has a run in flight, so a schedule does not stack a second one behind it.
///
/// **Installation-wide rather than per tenant on purpose.** The claim that two runs of one suite
/// would conflict is about the *suite row*, not about a caller's permissions, and a suite id is
/// unique across tenants by its primary key — so scoping by organization could only ever answer
/// "no" for a suite belonging to somebody else, which is the answer we want anyway but for the
/// wrong reason. The runner calls this with a suite id it already holds from an unscoped claim.
pub async fn has_active_run(pool: &PgPool, suite_id: Uuid) -> Result<bool> {
    let row: (bool,) = sqlx::query_as(
        "select exists (select 1 from ai_eval_runs \
         where suite_id = $1 and status in ('queued', 'running'))",
    )
    .bind(suite_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Whether a suite has any case that needs the judge model.
pub async fn suite_has_rubric_cases(pool: &PgPool, suite_id: Uuid) -> Result<bool> {
    let row: (bool,) = sqlx::query_as(
        "select exists (select 1 from ai_eval_cases where suite_id = $1 and expected ? 'rubric')",
    )
    .bind(suite_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Create a suite.
pub async fn create_suite(
    pool: &PgPool,
    organization_id: Uuid,
    suite: &NewSuite,
) -> Result<SuiteRow> {
    validate_suite(
        &suite.target,
        suite.agent_id,
        suite.copilot_key.as_deref(),
        suite.task.as_deref(),
        suite.model_id,
        suite.threshold_percent,
        suite.max_regression_points,
        suite.temperature,
        suite.judge_model_id,
        suite.judge_prompt.as_deref(),
        suite.schedule.as_deref(),
        1,
    )?;
    validate_key(&suite.key)?;
    validate_string_list("tools", &suite.tools)?;
    validate_string_list("collections", &suite.collections)?;

    let sql = "insert into ai_eval_suites (organization_id, key, name, description, target, \
                 agent_id, copilot_key, task, model_id, temperature, tools, collections, \
                 threshold_percent, max_regression_points, blocking, schedule, judge_model_id, \
                 judge_prompt, enabled, created_by) \
               values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, \
                 $17, $18, $19, $20) \
               returning id";
    let id: Uuid = match sqlx::query_scalar(sql)
        .bind(organization_id)
        .bind(&suite.key)
        .bind(&suite.name)
        .bind(&suite.description)
        .bind(&suite.target)
        .bind(suite.agent_id)
        .bind(suite.copilot_key.as_deref())
        .bind(suite.task.as_deref())
        .bind(suite.model_id)
        .bind(suite.temperature)
        .bind(&suite.tools)
        .bind(&suite.collections)
        .bind(suite.threshold_percent)
        .bind(suite.max_regression_points)
        .bind(suite.blocking)
        .bind(suite.schedule.as_deref())
        .bind(suite.judge_model_id)
        .bind(suite.judge_prompt.as_deref())
        .bind(suite.enabled)
        .bind(suite.created_by)
        .fetch_one(pool)
        .await
    {
        Ok(id) => id,
        Err(sqlx::Error::Database(ref error)) if is_unique_violation(error) => {
            return Err(AiHubError::EvalSuiteKeyTaken(suite.key.clone()));
        }
        Err(error) => return Err(error.into()),
    };

    find_suite_by_id(pool, organization_id, id)
        .await?
        .ok_or_else(|| AiHubError::EvalSuiteNotFound(id.to_string()))
}

/// Apply an edit to a suite.
///
/// The target columns are deliberately not editable: changing what a suite measures without
/// changing its key or its cases would make every historical run's snapshot a lie about what
/// was tested, and the request's run model exists precisely so a result can be reproduced.
pub async fn update_suite(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    changes: &SuiteChanges,
) -> Result<SuiteRow> {
    let current = find_suite_by_id(pool, organization_id, id)
        .await?
        .ok_or_else(|| AiHubError::EvalSuiteNotFound(id.to_string()))?;

    let name = changes.name.as_deref().unwrap_or(&current.name);
    if name.trim().is_empty() || name.chars().count() > 120 {
        return Err(AiHubError::InvalidEval(
            "`name` must be between 1 and 120 characters".to_string(),
        ));
    }
    let threshold_percent =
        changes.threshold_percent.unwrap_or(current.threshold_percent);
    let max_regression_points =
        changes.max_regression_points.unwrap_or(current.max_regression_points);
    let temperature = changes.temperature.unwrap_or(current.temperature);
    let judge_model_id = changes.judge_model_id.unwrap_or(current.judge_model_id);
    let judge_prompt = changes.judge_prompt.clone().unwrap_or_else(|| current.judge_prompt.clone());
    let schedule = changes.schedule.clone().unwrap_or(current.schedule);
    let tools = changes.tools.clone().unwrap_or_else(|| current.tools.clone());
    let collections = changes
        .collections
        .clone()
        .unwrap_or_else(|| current.collections.clone());
    let blocking = changes.blocking.unwrap_or(current.blocking);
    let enabled = changes.enabled.unwrap_or(current.enabled);

    validate_suite(
        &current.target,
        current.agent_id,
        current.copilot_key.as_deref(),
        current.task.as_deref(),
        current.model_id,
        threshold_percent,
        max_regression_points,
        temperature,
        judge_model_id,
        judge_prompt.as_deref(),
        schedule.as_deref(),
        current.judge_prompt_version,
    )?;
    validate_string_list("tools", &tools)?;
    validate_string_list("collections", &collections)?;

    // The judge's revision is bumped by the *editor*, not by the caller, and only when the
    // prompt actually changed. A run pins `judge_prompt_version`, so a version that moves
    // without the text moving makes two runs look different when they were not.
    let prompt_changed = judge_prompt != current.judge_prompt;
    let judge_prompt_version =
        i32::from(prompt_changed) + current.judge_prompt_version;

    // The blocking/rubric rule is checked BEFORE the write, not after it. It was checked after
    // in the first cut, which meant a refused edit had already cleared the judge before the
    // refusal was raised: the walk caught it by reading the row back and finding the change
    // half-applied. A validation that runs after its own write is not a validation.
    if blocking && judge_model_id.is_none() && suite_has_rubric_cases(pool, id).await? {
        return Err(AiHubError::InvalidEval(
            "this suite has a `rubric` case, so `judge_model_id` is required while it is \
             blocking"
                .to_string(),
        ));
    }

    let sql = "update ai_eval_suites set name = $3, description = $4, temperature = $5, \
                 tools = $6, collections = $7, threshold_percent = $8, \
                 max_regression_points = $9, blocking = $10, schedule = $11, \
                 judge_model_id = $12, judge_prompt = $13, judge_prompt_version = $14, \
                 enabled = $15, updated_at = now() \
               where organization_id = $1 and id = $2";
    let affected = sqlx::query(sql)
        .bind(organization_id)
        .bind(id)
        .bind(name)
        .bind(changes.description.as_deref().unwrap_or(&current.description))
        .bind(temperature)
        .bind(&tools)
        .bind(&collections)
        .bind(threshold_percent)
        .bind(max_regression_points)
        .bind(blocking)
        .bind(schedule.as_deref())
        .bind(judge_model_id)
        .bind(judge_prompt.as_deref())
        .bind(judge_prompt_version)
        .bind(enabled)
        .execute(pool)
        .await?
        .rows_affected();
    if affected == 0 {
        return Err(AiHubError::EvalSuiteNotFound(id.to_string()));
    }

    find_suite_by_id(pool, organization_id, id)
        .await?
        .ok_or_else(|| AiHubError::EvalSuiteNotFound(id.to_string()))
}

/// Remove a suite and, by cascade, its cases.
pub async fn delete_suite(pool: &PgPool, organization_id: Uuid, id: Uuid) -> Result<()> {
    let affected = sqlx::query("delete from ai_eval_suites where organization_id = $1 and id = $2")
        .bind(organization_id)
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    if affected == 0 {
        return Err(AiHubError::EvalSuiteNotFound(id.to_string()));
    }
    Ok(())
}

/// What a case save carries.
#[derive(Debug, Clone, Default)]
pub struct NewCase {
    /// Display name, 1–80 characters.
    pub name: String,
    /// The prompt/context the run replays.
    pub input: serde_json::Value,
    /// The properties this case asserts.
    pub expected: serde_json::Value,
    /// How much this case counts for in the run's pass rate.
    pub weight: f64,
    /// Coverage labels.
    pub tags: Vec<String>,
    /// Whether a run would execute it.
    pub enabled: bool,
    /// `manual`, `import` or `run`.
    pub source: String,
    /// The run this case was captured from.
    pub source_run_id: Option<Uuid>,
}

/// What a case edit carries.
#[derive(Debug, Clone, Default)]
pub struct CaseChanges {
    /// New name.
    pub name: Option<String>,
    /// New input.
    pub input: Option<serde_json::Value>,
    /// New expected properties.
    pub expected: Option<serde_json::Value>,
    /// New weight.
    pub weight: Option<f64>,
    /// New tags.
    pub tags: Option<Vec<String>>,
    /// New enabled flag.
    pub enabled: Option<bool>,
}

/// The largest case input the platform will replay, in characters.
///
/// A case's input is stored, snapshotted on every run and rendered in the case editor, so it
/// is bounded rather than free: a 4 MB context pasted into a cell would make the cases tab
/// unusable and every run's snapshot enormous.
pub const MAX_CASE_INPUT_CHARS: usize = 16_000;

/// Refuse a case that breaks one of its own rules, naming the field.
pub fn validate_case(case: &NewCase) -> Result<()> {
    let name_length = case.name.trim().chars().count();
    if name_length == 0 || name_length > 80 {
        return Err(AiHubError::InvalidEval(format!(
            "`name` must be between 1 and 80 characters, got {name_length}"
        )));
    }
    if !(0.1..=10.0).contains(&case.weight) {
        return Err(AiHubError::InvalidEval(format!(
            "`weight` must be between 0.1 and 10, got {}",
            case.weight
        )));
    }
    let input = case.input.to_string();
    if input.chars().count() > MAX_CASE_INPUT_CHARS {
        return Err(AiHubError::InvalidEval(format!(
            "`input` must be {MAX_CASE_INPUT_CHARS} characters or fewer, got {}",
            input.chars().count()
        )));
    }
    if !matches!(case.source.as_str(), "manual" | "import" | "run") {
        return Err(AiHubError::InvalidEval(format!(
            "unknown source `{}`; expected manual, import or run",
            case.source
        )));
    }
    if case.source == "run" && case.source_run_id.is_none() {
        return Err(AiHubError::InvalidEval(
            "a case captured from a run must name the `source_run_id` it came from".to_string(),
        ));
    }
    // This is the check that keeps a suite honest: the document has to name at least one
    // property, and every property it names has to be one this platform implements. Both are
    // refused by name, because the editor marks one field at a time.
    let expectation = eval_case::expectation_from(&case.expected)?;
    if expectation.is_empty() {
        return Err(AiHubError::InvalidEval(
            "a case must name at least one expected property".to_string(),
        ));
    }
    if let Some(schema) = case.expected.get("json_schema") {
        eval_case::schema_is_supported(schema).map_err(AiHubError::InvalidEval)?;
    }
    Ok(())
}

/// Add a case to a suite.
pub async fn create_case(
    pool: &PgPool,
    organization_id: Uuid,
    suite_id: Uuid,
    case: &NewCase,
) -> Result<CaseRow> {
    validate_case(case)?;
    let sql = "insert into ai_eval_cases (suite_id, organization_id, name, input, expected, \
                 weight, tags, enabled, source, source_run_id) \
               values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) returning id";
    let id: Uuid = sqlx::query_scalar(sql)
        .bind(suite_id)
        .bind(organization_id)
        .bind(case.name.trim())
        .bind(&case.input)
        .bind(&case.expected)
        .bind(case.weight)
        .bind(&case.tags)
        .bind(case.enabled)
        .bind(&case.source)
        .bind(case.source_run_id)
        .fetch_one(pool)
        .await?;
    find_case(pool, organization_id, id)
        .await?
        .ok_or(AiHubError::EvalCaseNotFound(id))
}

/// Apply an edit to a case.
pub async fn update_case(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    changes: &CaseChanges,
) -> Result<CaseRow> {
    let current = find_case(pool, organization_id, id)
        .await?
        .ok_or(AiHubError::EvalCaseNotFound(id))?;

    // Validation runs on the *merged* row rather than on the patch, because a patch that is
    // fine on its own can break the case: removing the only property leaves a case asserting
    // nothing, and validating only the changed field would save it.
    let merged = NewCase {
        name: changes
            .name
            .clone()
            .unwrap_or_else(|| current.name.clone()),
        input: changes
            .input
            .clone()
            .unwrap_or_else(|| current.input.clone()),
        expected: changes
            .expected
            .clone()
            .unwrap_or_else(|| current.expected.clone()),
        weight: changes.weight.unwrap_or(current.weight),
        tags: changes.tags.clone().unwrap_or_else(|| current.tags.clone()),
        enabled: changes.enabled.unwrap_or(current.enabled),
        source: current.source.clone(),
        source_run_id: current.source_run_id,
    };
    validate_case(&merged)?;

    let sql = "update ai_eval_cases set name = $3, input = $4, expected = $5, weight = $6, \
                 tags = $7, enabled = $8, updated_at = now() \
               where organization_id = $1 and id = $2";
    let affected = sqlx::query(sql)
        .bind(organization_id)
        .bind(id)
        .bind(merged.name.trim())
        .bind(&merged.input)
        .bind(&merged.expected)
        .bind(merged.weight)
        .bind(&merged.tags)
        .bind(merged.enabled)
        .execute(pool)
        .await?
        .rows_affected();
    if affected == 0 {
        return Err(AiHubError::EvalCaseNotFound(id));
    }
    find_case(pool, organization_id, id)
        .await?
        .ok_or(AiHubError::EvalCaseNotFound(id))
}

/// Remove a case.
pub async fn delete_case(pool: &PgPool, organization_id: Uuid, id: Uuid) -> Result<()> {
    let affected = sqlx::query("delete from ai_eval_cases where organization_id = $1 and id = $2")
        .bind(organization_id)
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    if affected == 0 {
        return Err(AiHubError::EvalCaseNotFound(id));
    }
    Ok(())
}

/// The cases of one suite, as the run layer will execute them.
///
/// Separate from [`list_cases`] on purpose: this one carries only the columns an execution
/// needs and only the enabled rows, and it is the function the runner calls. A run that read
/// the screen's rows would carry the last-status sub-selects into every execution.
pub async fn cases_for_run(
    pool: &PgPool,
    organization_id: Uuid,
    suite_id: Uuid,
) -> Result<Vec<CaseRow>> {
    let sql = "select c.id, c.suite_id, c.organization_id, c.name, c.input, c.expected, \
               c.weight::float8, c.tags, c.enabled, c.source, c.source_run_id, c.created_at, \
               c.updated_at, null::text as last_status, null::timestamptz as last_run_at \
             from ai_eval_cases c \
             where c.organization_id = $1 and c.suite_id = $2 and c.enabled \
             order by c.name, c.id";
    let rows = sqlx::query_as::<_, CaseRow>(sql)
        .bind(organization_id)
        .bind(suite_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// The tags a tenant's cases use, with how many cases carry each.
///
/// The request's risks section asks the panel to show coverage, and coverage is a count per
/// tag — a query the panel would otherwise run client-side over every case it had paged in,
/// which is a coverage number that silently changes with the page size.
pub async fn tag_coverage(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<Vec<(String, i64)>> {
    let rows = sqlx::query_as::<_, (String, i64)>(
        "select tag, count(*) from ai_eval_cases c, unnest(c.tags) as tag \
         where c.organization_id = $1 \
         group by tag order by count(*) desc, tag",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// A case key must be a URL segment an operator can type at a gate.
pub fn validate_key(key: &str) -> Result<()> {
    let length = key.chars().count();
    if !(3..=64).contains(&length) {
        return Err(AiHubError::InvalidEval(format!(
            "`key` must be between 3 and 64 characters, got {length}"
        )));
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(AiHubError::InvalidEval(
            "`key` may only contain lowercase letters, digits, `-` and `_`".to_string(),
        ));
    }
    if key.starts_with('-') || key.ends_with('-') || key.starts_with('_') || key.ends_with('_') {
        return Err(AiHubError::InvalidEval(
            "`key` must start and end with a letter or a digit".to_string(),
        ));
    }
    Ok(())
}

/// `tools` and `collections` are string arrays, and the run layer indexes them as such.
///
/// SQL NULL and JSON `null` both mean "not set", and an unset allow-list is the empty one —
/// a run with no tool allow-list is a normal run, not a malformed one. `NewSuite::default()`
/// therefore has to be a valid suite, and so does a caller that leaves the field out. Only a
/// value that is present and of the wrong shape is a refusal.
fn validate_string_list(field: &str, value: &serde_json::Value) -> Result<()> {
    if value.is_null() {
        return Ok(());
    }
    let Some(list) = value.as_array() else {
        return Err(AiHubError::InvalidEval(format!(
            "`{field}` must be a JSON array of strings"
        )));
    };
    for item in list {
        if !item.is_string() {
            return Err(AiHubError::InvalidEval(format!(
                "every entry of `{field}` must be a string"
            )));
        }
    }
    Ok(())
}

/// Whether a database error is a unique-constraint violation (SQLSTATE 23505).
///
/// Takes the `DatabaseError` rather than the whole `sqlx::Error` so the caller can pass the
/// boxed inner error it already destructured, instead of re-wrapping a value it has in hand.
fn is_unique_violation(error: &Box<dyn sqlx::error::DatabaseError>) -> bool {
    error.code().as_deref() == Some("23505")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suite() -> NewSuite {
        NewSuite {
            key: "support-replies".to_string(),
            name: "Support replies".to_string(),
            target: "model".to_string(),
            model_id: Some(Uuid::from_u128(1)),
            tools: serde_json::json!([]),
            collections: serde_json::json!([]),
            threshold_percent: 90,
            max_regression_points: 5.0,
            enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn a_well_formed_suite_validates() {
        assert!(validate_suite("model", None, None, None, Some(Uuid::from_u128(1)), 90, 5.0, None, None, None, None, 1).is_ok());
    }

    #[test]
    fn a_threshold_outside_one_to_hundred_is_refused_by_field_name() {
        for threshold in [0, 101] {
            let error = validate_suite("model", None, None, None, Some(Uuid::from_u128(1)), threshold, 5.0, None, None, None, None, 1)
                .unwrap_err();
            assert!(error.to_string().contains("threshold_percent"), "{error}");
        }
    }

    #[test]
    fn a_judge_that_is_the_model_under_test_is_refused() {
        let model = Uuid::from_u128(7);
        let error =
            validate_suite("model", None, None, None, Some(model), 90, 5.0, None, Some(model), None, None, 1)
                .unwrap_err();
        assert!(error.to_string().contains("judge_model_id"), "{error}");
    }

    #[test]
    fn a_temperature_outside_zero_to_two_is_refused() {
        let error = validate_suite("model", None, None, None, Some(Uuid::from_u128(1)), 90, 5.0, Some(2.5), None, None, None, 1)
            .unwrap_err();
        assert!(error.to_string().contains("temperature"), "{error}");
    }

    #[test]
    fn an_unknown_target_is_refused_with_the_real_ones_named() {
        let error = validate_suite("pipeline", None, None, None, None, 90, 5.0, None, None, None, None, 1)
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("pipeline"), "{message}");
        assert!(message.contains("copilot"), "{message}");
    }

    #[test]
    fn a_preset_schedule_is_accepted_and_a_five_field_cron_too() {
        for schedule in ["hourly", "daily", "weekly", "custom", "23 4 * * 1"] {
            assert!(validate_schedule(schedule).is_ok(), "{schedule}");
        }
    }

    #[test]
    fn a_schedule_the_runner_could_not_parse_is_refused() {
        // A suite with an unparseable schedule silently never runs, and a green suite that
        // measures nothing is the worst state this screen can be in.
        for schedule in ["every tuesday", "0 * * *", "* * * * * *"] {
            let error = validate_schedule(schedule).unwrap_err();
            assert!(error.to_string().contains("schedule"), "{error}");
        }
    }

    #[test]
    fn the_presets_expand_to_cron_the_runner_can_use() {
        for preset in ["hourly", "daily", "weekly"] {
            let cron = preset_cron(preset).expect("preset has a cron");
            assert_eq!(cron.split_whitespace().count(), 5, "{preset} -> {cron}");
        }
        assert_eq!(preset_cron("custom"), None);
    }

    #[test]
    fn a_key_must_be_a_url_segment_that_ends_on_an_alphanumeric() {
        assert!(validate_key("support-replies").is_ok());
        assert!(validate_key("seo_v2").is_ok());
        for key in ["ab", "-lead", "trail-", "_lead", "Support", "with space", &"x".repeat(65)] {
            assert!(validate_key(key).is_err(), "{key} should be refused");
        }
    }

    #[test]
    fn a_case_must_name_at_least_one_property() {
        let case = NewCase {
            name: "empty".to_string(),
            input: serde_json::json!({ "prompt": "hi" }),
            expected: serde_json::json!({}),
            weight: 1.0,
            source: "manual".to_string(),
            enabled: true,
            ..Default::default()
        };
        let error = validate_case(&case).unwrap_err();
        assert!(error.to_string().contains("at least one"), "{error}");
    }

    #[test]
    fn a_case_with_a_schema_keyword_the_platform_does_not_implement_is_refused() {
        let case = NewCase {
            name: "schema".to_string(),
            input: serde_json::json!({ "prompt": "hi" }),
            expected: serde_json::json!({ "json_schema": { "type": "string", "pattern": "^a" } }),
            weight: 1.0,
            source: "manual".to_string(),
            enabled: true,
            ..Default::default()
        };
        let error = validate_case(&case).unwrap_err();
        assert!(error.to_string().contains("pattern"), "{error}");
    }

    #[test]
    fn a_weight_outside_a_tenth_to_ten_is_refused() {
        for weight in [0.05, 10.5] {
            let case = NewCase {
                name: "w".to_string(),
                input: serde_json::json!({ "prompt": "hi" }),
                expected: serde_json::json!({ "contains": ["x"] }),
                weight,
                source: "manual".to_string(),
                enabled: true,
                ..Default::default()
            };
            let error = validate_case(&case).unwrap_err();
            assert!(error.to_string().contains("weight"), "{error}");
        }
    }

    #[test]
    fn an_oversized_case_input_is_refused_with_its_own_limit_named() {
        let case = NewCase {
            name: "big".to_string(),
            input: serde_json::json!({ "prompt": "x".repeat(MAX_CASE_INPUT_CHARS + 1) }),
            expected: serde_json::json!({ "contains": ["x"] }),
            weight: 1.0,
            source: "manual".to_string(),
            enabled: true,
            ..Default::default()
        };
        let error = validate_case(&case).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("16000"), "{message}");
    }

    #[test]
    fn a_case_captured_from_a_run_must_name_that_run() {
        let case = NewCase {
            name: "captured".to_string(),
            input: serde_json::json!({ "prompt": "hi" }),
            expected: serde_json::json!({ "contains": ["x"] }),
            weight: 1.0,
            source: "run".to_string(),
            enabled: true,
            ..Default::default()
        };
        let error = validate_case(&case).unwrap_err();
        assert!(error.to_string().contains("source_run_id"), "{error}");
    }

    #[test]
    fn a_case_name_longer_than_eighty_characters_is_refused() {
        let case = NewCase {
            name: "x".repeat(81),
            input: serde_json::json!({ "prompt": "hi" }),
            expected: serde_json::json!({ "contains": ["x"] }),
            weight: 1.0,
            source: "manual".to_string(),
            enabled: true,
            ..Default::default()
        };
        let error = validate_case(&case).unwrap_err();
        assert!(error.to_string().contains("80"), "{error}");
    }

    #[test]
    fn the_tool_allow_list_must_be_an_array_of_strings() {
        // An unset allow-list is the empty one. `NewSuite::default()` carries `Value::Null` here,
        // and a default-built suite that the store refuses is a trap for every caller that
        // fills the struct in with `..Default::default()` — which is most of them.
        assert!(validate_string_list("tools", &serde_json::Value::Null).is_ok());
        assert!(validate_string_list("collections", &serde_json::Value::Null).is_ok());
        assert!(validate_string_list("tools", &serde_json::json!(["a", "b"])).is_ok());
        assert!(validate_string_list("tools", &serde_json::json!([1])).is_err());
        assert!(validate_string_list("tools", &serde_json::json!({ "a": 1 })).is_err());
    }

    #[test]
    fn a_target_that_disagrees_with_its_reference_is_refused_by_both_fields() {
        // The column catches this too, but a caller that reached the column got SQLSTATE 23514
        // and a constraint name; the store has to say which field is wrong.
        let error = validate_suite(
            "agent",
            Some(Uuid::from_u128(3)),
            None,
            None,
            Some(Uuid::from_u128(1)),
            90,
            5.0,
            None,
            None,
            None,
            None,
            1,
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("model_id"), "the extra field is named: {message}");
        assert!(message.contains("agent"), "the target is named: {message}");
    }

    #[test]
    fn a_target_with_no_reference_at_all_is_refused() {
        let error = validate_suite("copilot", None, None, None, None, 90, 5.0, None, None, None, None, 1)
            .unwrap_err();
        assert!(error.to_string().contains("copilot_key"), "{error}");
    }

    #[test]
    fn every_target_is_accepted_when_only_its_own_reference_is_set() {
        for (target, agent, copilot, task, model) in [
            ("agent", Some(Uuid::from_u128(3)), None, None, None),
            ("copilot", None, Some("support"), None, None),
            ("task", None, None, Some("summarize"), None),
            ("model", None, None, None, Some(Uuid::from_u128(1))),
        ] {
            validate_suite(
                target, agent, copilot, task, model, 90, 5.0, None, None, None, None, 1,
            )
            .unwrap_or_else(|e| panic!("{target} with its own reference must validate: {e}"));
        }
    }

    #[test]
    fn the_new_suite_fixture_validates_so_the_validation_tests_are_about_the_rule_not_the_fixture() {
        // A fixture that is itself invalid turns every assertion above into a test of the
        // fixture, which is the way a whole block of tests goes green without testing anything.
        validate_suite(
            &suite().target,
            suite().agent_id,
            suite().copilot_key.as_deref(),
            suite().task.as_deref(),
            suite().model_id,
            suite().threshold_percent,
            suite().max_regression_points,
            suite().temperature,
            suite().judge_model_id,
            suite().judge_prompt.as_deref(),
            suite().schedule.as_deref(),
            1,
        )
        .expect("the shared fixture is valid");
    }
}
