//! `/api/v1/ai/evals/*` — eval suites and their cases (REQ-107, slice 1).
//!
//! | Endpoint | Power | What it does |
//! |---|---|---|
//! | `GET /ai/evals/suites` | `ai.evals.read` | Every suite with its case counts, its tag coverage and the empty flag |
//! | `POST /ai/evals/suites` | `ai.evals.manage` | Create a suite; every rule is checked here and named |
//! | `GET /ai/evals/suites/{key}` | `ai.evals.read` | One suite, its cases and the judges it may use |
//! | `PATCH /ai/evals/suites/{key}` | `ai.evals.manage` | Change a suite; validated on the merged row |
//! | `DELETE /ai/evals/suites/{key}` | `ai.evals.manage` | Remove a suite; the key must be confirmed |
//! | `GET /ai/evals/suites/{key}/cases` | `ai.evals.read` | Its cases |
//! | `POST /ai/evals/suites/{key}/cases` | `ai.evals.manage` | Add a case |
//! | `PATCH /ai/evals/cases/{id}` | `ai.evals.manage` | Change a case |
//! | `DELETE /ai/evals/cases/{id}` | `ai.evals.manage` | Remove a case |
//! | `POST /ai/evals/suites/{key}/import` | `ai.evals.manage` | Import cases from CSV, reporting the bad lines by number |
//!
//! **Runs are not here yet.** `POST /suites/{key}/run`, the run list and the diff are slice 2
//! and slice 3, and they are the routes that decide the *outcome* of a suite rather than its
//! contents. The `Run now` button is therefore rendered from [`SuiteRow::rubric_case_count`]'s
//! sibling — the suite's readiness — and the panel says a run is not wired up yet rather than
//! offering a button that would have to lie. See the note on [`suite_readiness`].
//!
//! # The suite's rules are refused by the store, and this file only names the field
//!
//! Every rule in [`eval_store::validate_suite`] and [`eval_store::validate_case`] runs inside
//! the store, so the panel and any other caller get the same answer. The route's job is to turn
//! the refusal into a field the form can mark: [`field_for`] reads the message the store wrote
//! and finds the field it is about, so an invalid `regex` lands on the regex box and not on the
//! form as a whole.
//!
//! # The delete asks for the key, because a suite is the thing a gate is named after
//!
//! `DELETE /suites/{key}` requires `confirm` to equal the key. A suite is referenced by a
//! promotion pipeline, a baseline and a schedule; a confirm dialog that said "are you sure?"
//! would let a mistyped id remove the ruler a release depends on. The rule is a query parameter
//! rather than a body so the failure mode is a `422` naming what is missing.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_ai_hub::error::AiHubError;
use omnion_ai_hub::eval_case;
use omnion_ai_hub::eval_store::{
    self, CaseChanges, CaseRow, NewCase, NewSuite, SuiteChanges, SuiteRow, MAX_CASE_INPUT_CHARS,
    SCHEDULE_PRESETS,
};
use omnion_ai_hub::run_store;
use omnion_ai_hub::store;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::routes::ai_agents::OrgQuery;
use crate::scope::resolve_organization;
use crate::state::AppState;

/// A picker option: a row id plus the label a human reads.
#[derive(Debug, Clone, Serialize)]
pub struct Option_ {
    /// The row id, or the raw key for copilots.
    pub id: String,
    /// What the screen shows.
    pub label: String,
    /// A second line of context — a model key, a description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

// -------------------------------------------------------------------------------------------
// Suites
// -------------------------------------------------------------------------------------------

/// `GET /ai/evals/suites` query.
#[derive(Debug, Default, Deserialize)]
pub struct SuiteQuery {
    /// `agent`, `copilot`, `task` or `model`.
    pub target: Option<String>,
    /// Only enabled, or only disabled, suites.
    pub enabled: Option<bool>,
    /// Only blocking, or only advisory, suites.
    pub blocking: Option<bool>,
    /// Free text over key, name and description.
    pub q: Option<String>,
    /// Case with this tag.
    pub tag: Option<String>,
}

/// One suite on the list row, flattened so the screen never has to reach into a nested object.
#[derive(Debug, Clone, Serialize)]
pub struct SuiteSummary {
    /// The row itself; the panel reads the counts off it.
    #[serde(flatten)]
    pub suite: SuiteRow,
    /// `ready`, `needs_judge`, `empty` or `disabled` — what a single badge has to say.
    pub readiness: &'static str,
    /// Why the badge reads the way it does, in words the panel can show.
    pub readiness_note: String,
}

/// `GET /ai/evals/suites` — every suite, with the stat tiles.
#[derive(Debug, Clone, Serialize)]
pub struct SuiteList {
    /// The rows after filtering.
    pub suites: Vec<SuiteSummary>,
    /// How many suites the tenant owns in total, so a filtered list can say "3 of 11".
    pub total: usize,
    /// Suites that are the promotion gate.
    pub blocking_count: usize,
    /// Suites carrying a cron schedule.
    pub scheduled_count: usize,
    /// Cases across all suites, enabled or not — the coverage stat tile.
    pub case_count: i64,
    /// Tag → case count, so the screen can show what the suites actually cover.
    pub coverage: Vec<CoverageRow>,
    /// `true` when the tenant has no suite at all, so the panel shows its empty state rather
    /// than an empty table it has to guess the meaning of.
    pub is_empty: bool,
}

/// One tag and how many cases carry it.
#[derive(Debug, Clone, Serialize)]
pub struct CoverageRow {
    /// The tag.
    pub tag: String,
    /// How many cases name it.
    pub cases: i64,
}

/// `GET /ai/evals/suites` — the list the panel opens on.
pub async fn list_suites(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Query(query): Query<SuiteQuery>,
) -> Result<Json<SuiteList>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let pool = state.db().pool();
    let all = eval_store::list_suites(pool, organization).await?;
    let coverage = eval_store::tag_coverage(pool, organization).await?;

    let needle = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_lowercase);
    let tag = query
        .tag
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_lowercase);

    let mut suites = Vec::new();
    for suite in &all {
        if let Some(target) = &query.target
            && target != "all"
            && suite.target != *target
        {
            continue;
        }
        if let Some(enabled) = query.enabled
            && suite.enabled != enabled
        {
            continue;
        }
        if let Some(blocking) = query.blocking
            && suite.blocking != blocking
        {
            continue;
        }
        if let Some(needle) = &needle
            && !suite.key.to_lowercase().contains(needle)
            && !suite.name.to_lowercase().contains(needle)
            && !suite.description.to_lowercase().contains(needle)
        {
            continue;
        }
        // The tag filter is the one that cannot be answered from a suite row: tags live on the
        // cases, so this is a per-suite question. It runs only when a tag was actually asked
        // for, which is why the common path never pays for it.
        if let Some(tag) = &tag {
            let cases = eval_store::list_cases(pool, organization, suite.id).await?;
            if !cases
                .iter()
                .any(|case| case.tags.iter().any(|value| value.to_lowercase() == *tag))
            {
                continue;
            }
        }
        let (readiness, readiness_note) = suite_readiness(suite);
        suites.push(SuiteSummary { suite: suite.clone(), readiness, readiness_note });
    }

    Ok(Json(SuiteList {
        total: all.len(),
        blocking_count: all.iter().filter(|suite| suite.blocking).count(),
        scheduled_count: all.iter().filter(|suite| suite.schedule.is_some()).count(),
        case_count: all.iter().map(|suite| suite.case_count).sum(),
        is_empty: all.is_empty(),
        coverage: coverage
            .into_iter()
            .map(|(tag, cases)| CoverageRow { tag, cases })
            .collect(),
        suites,
    }))
}

/// What one badge has to be able to say about a suite, in one word.
///
/// The request's own risk note is the reason this exists: "a suite of ten easy cases passes
/// everything". A green row next to a suite with no cases is worse than an empty table, so the
/// four states are distinguished and the reason travels with the word.
fn suite_readiness(suite: &SuiteRow) -> (&'static str, String) {
    if !suite.enabled {
        return ("disabled", "Switched off — a run will skip it.".to_string());
    }
    if suite.enabled_case_count == 0 {
        return (
            "empty",
            if suite.case_count == 0 {
                "No cases yet — a run would measure nothing.".to_string()
            } else {
                format!(
                    "All {} cases are disabled — a run would measure nothing.",
                    suite.case_count
                )
            },
        );
    }
    if suite.rubric_case_count > 0 && suite.judge_model_id.is_none() {
        return (
            "needs_judge",
            format!(
                "{} case{} ask for a rubric and no judge model is set — those cases will error.",
                suite.rubric_case_count,
                if suite.rubric_case_count == 1 { "" } else { "s" }
            ),
        );
    }
    (
        "ready",
        format!("{} enabled case{} ready to run.", suite.enabled_case_count,
            if suite.enabled_case_count == 1 { "" } else { "s" }),
    )
}

/// `POST /ai/evals/suites` body.
///
/// Every field has a default except the ones a suite cannot be without, so a form that posts
/// only what the operator filled in still creates a valid row rather than a `422` about a field
/// it never showed.
#[derive(Debug, Default, Deserialize)]
pub struct CreateSuiteBody {
    /// URL-safe key, unique per tenant.
    pub key: String,
    /// Display name.
    pub name: String,
    /// What the suite is for.
    #[serde(default)]
    pub description: String,
    /// `agent`, `copilot`, `task` or `model`.
    pub target: String,
    /// The agent under test.
    #[serde(default)]
    pub agent_id: Option<Uuid>,
    /// The copilot under test.
    #[serde(default)]
    pub copilot_key: Option<String>,
    /// The task kind under test.
    #[serde(default)]
    pub task: Option<String>,
    /// The model under test.
    #[serde(default)]
    pub model_id: Option<Uuid>,
    /// Sampling temperature, 0–2.
    #[serde(default)]
    pub temperature: Option<f64>,
    /// The tool allow-list.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// The knowledge collections the run may read.
    #[serde(default)]
    pub collections: Option<Vec<String>>,
    /// The pass share below which a gate blocks, 1–100.
    #[serde(default = "default_threshold")]
    pub threshold_percent: i32,
    /// How many points a run may drop against its baseline, 0–50.
    #[serde(default = "default_tolerance")]
    pub max_regression_points: f64,
    /// Whether this suite is the promotion gate.
    #[serde(default)]
    pub blocking: bool,
    /// A preset name or a five-field cron expression.
    #[serde(default)]
    pub schedule: Option<String>,
    /// The second model that judges rubric cases.
    #[serde(default)]
    pub judge_model_id: Option<Uuid>,
    /// The judge prompt.
    #[serde(default)]
    pub judge_prompt: Option<String>,
    /// Whether the suite starts enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_threshold() -> i32 {
    90
}

fn default_tolerance() -> f64 {
    5.0
}

fn default_true() -> bool {
    true
}

/// Turn a store refusal into the field the form should mark.
///
/// The store writes its messages with the field name in backticks — `` `threshold_percent` must
/// be between 1 and 100 `` — precisely so this function can find it, and the property messages
/// do the same (`a \`regex\` that does not compile`). A client that could not map a refusal onto
/// a field would print it above the form, which for a case with ten properties is the one place
/// an operator will not look.
fn field_for(message: &str) -> Option<&'static str> {
    const MAP: &[(&str, &str)] = &[
        ("threshold_percent", "threshold_percent"),
        ("max_regression_points", "max_regression_points"),
        ("judge_model_id", "judge_model_id"),
        ("judge_prompt_version", "judge_prompt"),
        ("judge_prompt", "judge_prompt"),
        ("agent_id", "target"),
        ("copilot_key", "target"),
        ("source_run_id", "source"),
        ("source", "source"),
        ("model_id", "target"),
        ("temperature", "temperature"),
        ("collections", "collections"),
        ("tools", "tools"),
        ("schedule", "schedule"),
        ("task", "target"),
        ("weight", "weight"),
        ("input", "input"),
        ("tags", "tags"),
        ("name", "name"),
    ];
    if let Some((_, field)) = MAP.iter().find(|(needle, _)| message.contains(&format!("`{needle}`"))) {
        return Some(field);
    }
    // Two store messages name the field without backticks — "a case's expected properties must
    // be a JSON object" and "a case must name at least one expected property" — so the map above
    // cannot see either. One substring covers both, and it is deliberately the *whole phrase*
    // rather than the word "expected": that word also appears in "expected `regex` to be a
    // string", which names a single property and must mark the property box, not the group.
    if message.contains("expected propert") {
        return Some("expected");
    }
    // The ten assertable properties come second, and the case editor is the reason they have
    // to be here at all: the scorer writes "expected `regex` to be …" and "expected `rubric` must
    // not be blank", naming the *property* in backticks. A mapper that only knew the suite's
    // columns would drop every one of those refusals onto the form, ten fields at a time — and
    // the case editor marks one input per error, so a refusal with no field is a refusal with no
    // visible target. `eval_case::PROPERTIES` stays the single source: a property added to the
    // scorer without a checkbox on screen is a broken checkbox, not an unmapped message.
    eval_case::PROPERTIES
        .iter()
        .find(|property| message.contains(&format!("`{property}`")))
        .copied()
}

/// `POST /ai/evals/suites` — create a suite.
pub async fn create_suite(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Json(body): Json<CreateSuiteBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let name = body.name.trim().to_owned();
    if name.is_empty() {
        return Err(ApiError::bad_request(
            "invalid_eval",
            "`name` must be between 1 and 200 characters, got 0",
        ));
    }

    let suite = NewSuite {
        key: body.key.trim().to_owned(),
        name,
        description: body.description.trim().to_owned(),
        target: body.target.trim().to_lowercase(),
        agent_id: body.agent_id,
        copilot_key: body
            .copilot_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        task: body
            .task
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        model_id: body.model_id,
        temperature: body.temperature,
        tools: string_list(body.tools.unwrap_or_default()),
        collections: string_list(body.collections.unwrap_or_default()),
        threshold_percent: body.threshold_percent,
        max_regression_points: body.max_regression_points,
        blocking: body.blocking,
        schedule: body
            .schedule
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        judge_model_id: body.judge_model_id,
        judge_prompt: body
            .judge_prompt
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        enabled: body.enabled,
        created_by: Some(current.user.id),
    };

    let created = eval_store::create_suite(state.db().pool(), organization, &suite)
        .await
        .map_err(|error| annotate(error))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "suite": created })),
    ))
}

/// Turn a list into a clean jsonb array, dropping blanks.
///
/// `None` is an empty array rather than a JSON `null` at the call sites, because the columns
/// are `jsonb not null default '[]'`: a `null` would be cast by Postgres and read back as `[]`
/// anyway, but the response would say `null` in the same breath, and a client rendering
/// `tools.length` would have to handle a shape the database never holds.
fn string_list(values: Vec<String>) -> Value {
    Value::Array(
        values
            .into_iter()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .map(Value::String)
            .collect(),
    )
}

/// Add the field name to a refusal so the form can mark one input.
///
/// The field travels in `details.field` rather than being folded into the message, because the
/// message is what the operator reads and the field is what the form acts on: a form that had
/// to parse the sentence to know which input to mark would be guessing, and the store's wording
/// is allowed to improve without a client change.
fn annotate(error: AiHubError) -> ApiError {
    let message = error.to_string();
    let api = ApiError::from(error);
    match field_for(&message) {
        Some(field) => api.with_details(serde_json::json!({ "field": field })),
        None => api,
    }
}

/// `GET /ai/evals/suites/{key}` — one suite with its cases and the judges it may use.
#[derive(Debug, Clone, Serialize)]
pub struct SuiteDetail {
    /// The suite.
    #[serde(flatten)]
    pub suite: SuiteRow,
    /// Its cases.
    pub cases: Vec<CaseRow>,
    /// `ready`, `needs_judge`, `empty` or `disabled`.
    pub readiness: &'static str,
    /// Why, in words.
    pub readiness_note: String,
    /// What the run would be measured against — the models this tenant may pin.
    pub model_options: Vec<Option_>,
    /// The agents a suite may target.
    pub agent_options: Vec<Option_>,
    /// The schedule presets the config tab offers.
    pub schedule_presets: Vec<SchedulePreset>,
    /// The properties a case may assert, with the editor hint for each.
    pub properties: Vec<PropertyInfo>,
}

/// One cron preset and what it expands to.
#[derive(Debug, Clone, Serialize)]
pub struct SchedulePreset {
    /// The preset name, or `custom`.
    pub id: String,
    /// What the screen calls it.
    pub label: String,
    /// The cron expression, or `null` for a manual-only suite and for `custom`.
    pub cron: Option<&'static str>,
}

/// One assertable property and the field the case editor reveals for it.
#[derive(Debug, Clone, Serialize)]
pub struct PropertyInfo {
    /// The property key, exactly as it appears in `expected`.
    pub key: &'static str,
    /// What it asserts, in one line.
    pub label: &'static str,
    /// The editor field it reveals, so the form is not hard-coded against this list.
    pub field: &'static str,
    /// Whether a second model has to judge it.
    pub needs_judge: bool,
}

/// The properties the case editor offers, with the field each one reveals.
///
/// This is the server's list, sent to the screen, so the form cannot offer a property the
/// scorer does not implement. An earlier screen would have hard-coded this list and a property
/// added to `eval_case::PROPERTIES` would have become authorable-but-unscorable: the case would
/// save, the run would report every check as passing, and the suite would be a green screen
/// measuring nothing. `eval_case::PROPERTIES` remains the single source; this table only says
/// which editor field belongs to which key.
fn property_infos() -> Vec<PropertyInfo> {
    const TABLE: &[(&str, &str, &str, bool)] = &[
        ("exact", "Output equals this string", "expected.exact", false),
        ("contains", "Output contains this text", "expected.contains", false),
        ("regex", "Output matches this pattern", "expected.regex", false),
        ("json_schema", "Output parses and matches this schema", "expected.json_schema", false),
        ("citations_required", "Output cites its sources", "expected.citations_required", false),
        ("no_pii", "Output masks nothing the data guard would mask", "expected.no_pii", false),
        ("max_steps", "Run uses at most this many steps", "expected.max_steps", false),
        ("max_cost_micros", "Run costs at most this much", "expected.max_cost_micros", false),
        ("max_latency_ms", "Run answers within this many milliseconds", "expected.max_latency_ms", false),
        ("rubric", "A second model judges this free text", "expected.rubric", true),
    ];
    TABLE
        .iter()
        .filter(|(key, ..)| eval_case::PROPERTIES.contains(key))
        // The filter is what keeps the two lists honest: a property added to `eval_case` without
        // a row here is *absent* from the editor (visible in the walkthrough as a missing
        // checkbox), rather than present with no field and a checkbox that silently does nothing.
        .map(|(key, label, field, needs_judge)| PropertyInfo {
            key,
            label,
            field,
            needs_judge: *needs_judge,
        })
        .collect()
}

pub async fn read_suite(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(key): Path<String>,
) -> Result<Json<SuiteDetail>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let pool = state.db().pool();
    let suite = eval_store::find_suite(pool, organization, &key)
        .await?
        .ok_or_else(|| AiHubError::EvalSuiteNotFound(key.clone()))?;
    let cases = eval_store::list_cases(pool, organization, suite.id).await?;
    let (readiness, readiness_note) = suite_readiness(&suite);

    Ok(Json(SuiteDetail {
        suite: suite.clone(),
        cases,
        readiness,
        readiness_note,
        model_options: model_options(pool).await?,
        agent_options: agent_options(pool, organization).await?,
        schedule_presets: SCHEDULE_PRESETS
            .iter()
            .map(|id| SchedulePreset {
                id: (*id).to_string(),
                label: (*id).replace('_', " "),
                cron: eval_store::preset_cron(id),
            })
            .collect(),
        properties: property_infos(),
    }))
}

/// The models a suite may pin, with the model key beside the display name.
///
/// Both the model under test and the judge come from this one list, which is what lets the
/// panel enforce the request's rule — the judge select *hides* the model under test — without
/// owning a second copy of the registry.
async fn model_options(pool: &sqlx::PgPool) -> Result<Vec<Option_>, ApiError> {
    let rows = store::list_models(pool, None).await?;
    Ok(rows
        .into_iter()
        .map(|model| Option_ {
            id: model.id.to_string(),
            // The column is nullable and the panel falls back to the wire key when it is
            // blank, so an option is `None` rather than an empty label.
            label: model.display_name.clone().unwrap_or_else(|| model.model_key.clone()),
            detail: Some(model.model_key.clone()),
        })
        .collect())
}

/// The agents a suite may target.
async fn agent_options(
    pool: &sqlx::PgPool,
    organization: Uuid,
) -> Result<Vec<Option_>, ApiError> {
    let rows = run_store::list_agents(pool, organization).await?;
    Ok(rows
        .into_iter()
        .map(|agent| Option_ {
            id: agent.id.to_string(),
            label: agent.name.clone(),
            detail: Some(agent.key.clone()),
        })
        .collect())
}

/// `PATCH /ai/evals/suites/{key}` body. Every field is optional.
#[derive(Debug, Default, Deserialize)]
pub struct EditSuiteBody {
    /// New display name.
    pub name: Option<String>,
    /// New description.
    pub description: Option<String>,
    /// New temperature; `null` clears it.
    pub temperature: Option<Option<f64>>,
    /// New tool allow-list.
    pub tools: Option<Vec<String>>,
    /// New knowledge collections.
    pub collections: Option<Vec<String>>,
    /// New threshold.
    pub threshold_percent: Option<i32>,
    /// New tolerance.
    pub max_regression_points: Option<f64>,
    /// New blocking flag.
    pub blocking: Option<bool>,
    /// New schedule; `null` clears it.
    pub schedule: Option<Option<String>>,
    /// New judge model; `null` clears it.
    pub judge_model_id: Option<Option<Uuid>>,
    /// New judge prompt; `null` clears it.
    pub judge_prompt: Option<Option<String>>,
    /// New enabled flag.
    pub enabled: Option<bool>,
}

pub async fn update_suite(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(key): Path<String>,
    Json(body): Json<EditSuiteBody>,
) -> Result<Json<Value>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let pool = state.db().pool();
    let suite = eval_store::find_suite(pool, organization, &key)
        .await?
        .ok_or_else(|| AiHubError::EvalSuiteNotFound(key.clone()))?;

    let changes = SuiteChanges {
        name: body.name.map(|value| value.trim().to_owned()),
        description: body.description.map(|value| value.trim().to_owned()),
        temperature: body.temperature,
        tools: body.tools.map(string_list),
        collections: body.collections.map(string_list),
        threshold_percent: body.threshold_percent,
        max_regression_points: body.max_regression_points,
        blocking: body.blocking,
        schedule: body.schedule.map(|value| {
            value.map(|inner| inner.trim().to_owned()).filter(|inner| !inner.is_empty())
        }),
        judge_model_id: body.judge_model_id,
        judge_prompt: body.judge_prompt.map(|value| {
            value.map(|inner| inner.trim().to_owned()).filter(|inner| !inner.is_empty())
        }),
        enabled: body.enabled,
    };

    // The suite's *identity* columns — key, target and the four reference columns — are not in
    // the patch on purpose. Changing what a suite measures under the same key would make every
    // past run incomparable with the next one, and the request asks for runs to be reproducible.
    let updated = eval_store::update_suite(pool, organization, suite.id, &changes)
        .await
        .map_err(annotate)?;
    Ok(Json(serde_json::json!({ "suite": updated })))
}

/// `DELETE /ai/evals/suites/{key}?confirm=<key>`.
#[derive(Debug, Default, Deserialize)]
pub struct DeleteQuery {
    /// The key, typed back.
    pub confirm: Option<String>,
}

pub async fn delete_suite(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Query(query): Query<DeleteQuery>,
    Path(key): Path<String>,
) -> Result<StatusCode, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    if query.confirm.as_deref() != Some(key.as_str()) {
        return Err(ApiError::bad_request(
            "eval_confirm_required",
            format!("type the suite key `{key}` to confirm the delete"),
        ));
    }
    let pool = state.db().pool();
    let suite = eval_store::find_suite(pool, organization, &key)
        .await?
        .ok_or_else(|| AiHubError::EvalSuiteNotFound(key.clone()))?;
    eval_store::delete_suite(pool, organization, suite.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// -------------------------------------------------------------------------------------------
// Cases
// -------------------------------------------------------------------------------------------

/// `GET /ai/evals/suites/{key}/cases` — the cases tab's rows.
#[derive(Debug, Clone, Serialize)]
pub struct CaseList {
    /// The rows.
    pub cases: Vec<CaseRow>,
    /// Enabled cases weighted total — the denominator of a run's pass rate.
    pub enabled_weight: f64,
    /// How many name a rubric, i.e. need a judge.
    pub rubric_count: i64,
}

pub async fn list_cases(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(key): Path<String>,
) -> Result<Json<CaseList>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let pool = state.db().pool();
    let suite = eval_store::find_suite(pool, organization, &key)
        .await?
        .ok_or_else(|| AiHubError::EvalSuiteNotFound(key.clone()))?;
    let cases = eval_store::list_cases(pool, organization, suite.id).await?;
    Ok(Json(CaseList {
        enabled_weight: cases.iter().filter(|case| case.enabled).map(|case| case.weight).sum(),
        rubric_count: cases
            .iter()
            .filter(|case| case.enabled && case.expected.get("rubric").is_some())
            .count() as i64,
        cases,
    }))
}

/// `POST /ai/evals/suites/{key}/cases` body.
#[derive(Debug, Default, Deserialize)]
pub struct CreateCaseBody {
    /// Display name, 1–80.
    pub name: String,
    /// The prompt/context. A string becomes `{"prompt": …}` so the simple editor and the JSON
    /// editor post the same shape.
    pub input: Value,
    /// The properties this case asserts.
    pub expected: Value,
    /// How much it counts for, 0.1–10.
    #[serde(default = "default_weight")]
    pub weight: f64,
    /// Coverage labels.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Whether a run would execute it.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// `manual` or `run`. `import` is the importer's own claim, not a caller's.
    #[serde(default = "default_source")]
    pub source: String,
    /// The run a captured case came from.
    #[serde(default)]
    pub source_run_id: Option<Uuid>,
}

fn default_weight() -> f64 {
    1.0
}

fn default_source() -> String {
    "manual".to_string()
}

/// `POST /ai/evals/suites/{key}/cases` — add a case.
pub async fn create_case(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(key): Path<String>,
    Json(body): Json<CreateCaseBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let pool = state.db().pool();
    let suite = eval_store::find_suite(pool, organization, &key)
        .await?
        .ok_or_else(|| AiHubError::EvalSuiteNotFound(key.clone()))?;

    // The request's combination rule, enforced where it can be enforced: a suite that is the
    // promotion gate and holds a rubric case must have a judge. On *create* the suite has no
    // cases yet, so the rule lands on the case that triggers it — and it is refused by name,
    // because "no judge" is the sentence an operator needs above the case form.
    if suite.blocking && suite.judge_model_id.is_none() {
        let expected = eval_case::expectation_from(&body.expected).unwrap_or_default();
        if expected.has_rubric() {
            return Err(ApiError::bad_request(
                "invalid_eval",
                "this suite is blocking and has no `judge_model_id`; a `rubric` case needs a \
                 second model to judge it",
            )
            .with_details(serde_json::json!({ "field": "judge_model_id" })));
        }
    }

    let case = NewCase {
        name: body.name,
        input: normalize_input(body.input),
        expected: body.expected,
        weight: body.weight,
        tags: body.tags,
        enabled: body.enabled,
        source: if body.source == "import" { "manual".to_string() } else { body.source },
        source_run_id: body.source_run_id,
    };
    let created = eval_store::create_case(pool, organization, suite.id, &case)
        .await
        .map_err(annotate)?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "case": created }))))
}

/// A bare string input becomes `{"prompt": …}`.
///
/// The case editor has a plain-text mode and a JSON mode (the request asks for both), and the
/// two must not produce two different row shapes — a suite that mixes them would have cases
/// whose input is a string and cases whose input is an object, and the runner would have to
/// branch on the type of every input it replays.
fn normalize_input(input: Value) -> Value {
    match input {
        Value::String(text) => serde_json::json!({ "prompt": text }),
        Value::Null => serde_json::json!({ "prompt": "" }),
        other => other,
    }
}

/// `PATCH /ai/evals/cases/{id}` body.
#[derive(Debug, Default, Deserialize)]
pub struct EditCaseBody {
    /// New name.
    pub name: Option<String>,
    /// New input.
    pub input: Option<Value>,
    /// New properties.
    pub expected: Option<Value>,
    /// New weight.
    pub weight: Option<f64>,
    /// New tags.
    pub tags: Option<Vec<String>>,
    /// New enabled flag.
    pub enabled: Option<bool>,
}

pub async fn update_case(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(id): Path<Uuid>,
    Json(body): Json<EditCaseBody>,
) -> Result<Json<Value>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let changes = CaseChanges {
        name: body.name,
        input: body.input.map(normalize_input),
        expected: body.expected,
        weight: body.weight,
        tags: body.tags,
        enabled: body.enabled,
    };
    let updated = eval_store::update_case(state.db().pool(), organization, id, &changes)
        .await
        .map_err(annotate)?;
    Ok(Json(serde_json::json!({ "case": updated })))
}

pub async fn delete_case(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    eval_store::delete_case(state.db().pool(), organization, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// -------------------------------------------------------------------------------------------
// CSV import
// -------------------------------------------------------------------------------------------

/// `POST /ai/evals/suites/{key}/import` body.
#[derive(Debug, Default, Deserialize)]
pub struct ImportBody {
    /// The CSV text. Header row required, in any column order.
    pub csv: String,
}

/// One row that could not be imported, with the line number the editor can jump to.
#[derive(Debug, Clone, Serialize)]
pub struct ImportProblem {
    /// The 1-based line in the pasted text, header included.
    pub line: usize,
    /// What was wrong with it.
    pub message: String,
}

/// `POST /ai/evals/suites/{key}/import` — the import's own report.
#[derive(Debug, Clone, Serialize)]
pub struct ImportReport {
    /// The cases that were written.
    pub imported: Vec<CaseRow>,
    /// The rows that were refused, by line.
    pub problems: Vec<ImportProblem>,
    /// `true` when nothing was refused, so the screen can say so in words.
    pub clean: bool,
}

/// Largest CSV the platform will read, in characters.
///
/// An import is a paste, and a paste has no size. The cap is the same order as the case input
/// cap it feeds: a file that would produce cases past [`MAX_CASE_INPUT_CHARS`] each is refused
/// row by row anyway, so the ceiling here only has to stop the parser itself being the thing
/// that runs out of memory.
const MAX_IMPORT_CHARS: usize = 2_000_000;

/// The columns the importer reads. Every one is optional except `name` and `expected`, so a
/// two-column export from a spreadsheet imports, and a six-column one from a real eval tool does
/// too.
#[derive(Debug)]
struct CsvRow {
    name: String,
    input: String,
    properties: Vec<(String, String)>,
    weight: f64,
    tags: Vec<String>,
    enabled: bool,
}

/// `POST /ai/evals/suites/{key}/import` — import cases from CSV, bad lines and all.
///
/// # The import is partial by design, and says which rows it refused
///
/// The request's QA plan asks for exactly this: a malformed row "must be reported by line".
/// A transactional import that refused the whole file on one bad row would be easier to
/// implement and useless in practice — a hundred-case export with two typos would demand the
/// whole file be fixed and re-pasted, and the operator could not tell which two without
/// counting lines by hand. So each row is validated on its own, the good ones are written, and
/// the report carries the refusals with their line numbers.
///
/// # Header required, and the columns are matched by name
///
/// A CSV with no header is a guess: the importer would have to assume column order, and a
/// file exported by two different tools has two different orders. Requiring the header turns
/// that guess into a message that says what to add.
pub async fn import_cases(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(key): Path<String>,
    Json(body): Json<ImportBody>,
) -> Result<Json<ImportReport>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let pool = state.db().pool();
    let suite = eval_store::find_suite(pool, organization, &key)
        .await?
        .ok_or_else(|| AiHubError::EvalSuiteNotFound(key.clone()))?;

    let text = body.csv.as_str();
    if text.trim().is_empty() {
        return Err(ApiError::bad_request(
            "invalid_eval",
            "the import needs CSV text with a header row",
        ));
    }
    if text.chars().count() > MAX_IMPORT_CHARS {
        return Err(ApiError::bad_request(
            "invalid_eval",
            format!(
                "the import is {MAX_IMPORT_CHARS} characters or fewer, got {}",
                text.chars().count()
            ),
        ));
    }

    let mut lines = text.lines().enumerate();
    let Some((_, header)) = lines.next() else {
        return Err(ApiError::bad_request("invalid_eval", "the import is empty"));
    };
    let columns: Vec<String> = split_csv(header)
        .into_iter()
        .map(|name| name.trim().to_lowercase())
        .collect();
    if !columns.iter().any(|name| name == "name" || name == "case") {
        return Err(ApiError::bad_request(
            "invalid_eval",
            "the header needs a `name` column (or `case`); found \
             `name`, `input`, `expected`, `weight`, `tags`, `enabled`",
        ));
    }

    let mut imported = Vec::new();
    let mut problems = Vec::new();
    // Line 1 is the header, so the first data row is line 2 in the editor's numbering. Reporting
    // an off-by-one here would send an operator to the row above the one that failed.
    for (index, line) in lines {
        let line_number = index + 1;
        if line.trim().is_empty() {
            continue;
        }
        let cells = split_csv(line);
        let row = match read_csv_row(&columns, &cells) {
            Ok(row) => row,
            Err(message) => {
                problems.push(ImportProblem { line: line_number, message });
                continue;
            }
        };
        let case = NewCase {
            name: row.name,
            input: normalize_input(Value::String(row.input)),
            expected: expected_from_properties(&row.properties),
            weight: row.weight,
            tags: row.tags,
            enabled: row.enabled,
            source: "import".to_string(),
            source_run_id: None,
        };
        match eval_store::create_case(pool, organization, suite.id, &case).await {
            Ok(saved) => imported.push(saved),
            Err(error) => problems.push(ImportProblem {
                line: line_number,
                message: error.to_string(),
            }),
        }
    }
    Ok(Json(ImportReport {
        clean: problems.is_empty(),
        imported,
        problems,
    }))
}

/// One CSV line, split into cells, honouring quoted cells and doubled quotes.
///
/// Not a dependency: a CSV is a loop over characters, and a crate would be a bigger change than
/// the twenty lines below. What matters is that a quoted cell may contain a comma, a newline is
/// never treated as a separator inside quotes, and `""` is one quote — the three things a
/// hand-rolled splitter gets wrong, and the three that appear in every export that has a comma
/// in a cell.
fn split_csv(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(char) = chars.next() {
        match char {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                cell.push('"');
            }
            '"' => quoted = !quoted,
            ',' if !quoted => {
                cells.push(cell.trim().to_owned());
                cell = String::new();
            }
            other => cell.push(other),
        }
    }
    cells.push(cell.trim().to_owned());
    cells
}

/// Read one data row against the header's columns.
fn read_csv_row(columns: &[String], cells: &[String]) -> std::result::Result<CsvRow, String> {
    let get = |wanted: &[&str]| -> Option<String> {
        columns
            .iter()
            .position(|name| wanted.contains(&name.as_str()))
            .and_then(|index| cells.get(index).cloned())
    };
    let name = get(&["name", "case"]).unwrap_or_default().trim().to_string();
    if name.is_empty() {
        return Err("`name` is empty".to_string());
    }
    if name.chars().count() > 80 {
        return Err(format!("`name` is longer than 80 characters ({} chars)", name.chars().count()));
    }

    let input = get(&["input", "prompt", "context"]).unwrap_or_default();
    if input.chars().count() > MAX_CASE_INPUT_CHARS {
        return Err(format!(
            "`input` is longer than {MAX_CASE_INPUT_CHARS} characters ({} chars)",
            input.chars().count()
        ));
    }

    // Two ways to name the properties, because the two real exports disagree about them: a
    // `properties` column holding a JSON document, and one column per property (`exact`,
    // `contains`, `regex`, …). A file carrying both is read as JSON when the JSON parses and
    // column-wise otherwise, rather than merged — merging would let a stale column silently
    // re-assert a property the JSON replaced.
    let mut properties: Vec<(String, String)> = Vec::new();
    if let Some(raw) = get(&["expected", "properties", "expectations"]) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            match serde_json::from_str::<Value>(trimmed) {
                Ok(Value::Object(map)) => {
                    for (key, value) in map {
                        properties.push((key, value_to_property(&value)));
                    }
                }
                Ok(_) => return Err("`expected` must be a JSON object".to_string()),
                Err(error) => {
                    return Err(format!("`expected` is not valid JSON: {error}"));
                }
            }
        }
    }
    for property in eval_case::PROPERTIES {
        if let Some(raw) = get(&[property]) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                properties.push(((*property).to_string(), trimmed.to_string()));
            }
        }
    }

    let weight = match get(&["weight"]) {
        Some(raw) if !raw.trim().is_empty() => match raw.trim().parse::<f64>() {
            Ok(value) => value,
            Err(_) => return Err(format!("`weight` is not a number: {}", raw.trim())),
        },
        _ => 1.0,
    };
    if !(0.1..=10.0).contains(&weight) {
        return Err(format!("`weight` must be between 0.1 and 10, got {weight}"));
    }

    let tags = get(&["tags", "tag"])
        .map(|raw| {
            raw.split([',', ';', '|'])
                .map(str::trim)
                .filter(|tag| !tag.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let enabled = match get(&["enabled", "active"]) {
        Some(raw) => matches!(
            raw.trim().to_lowercase().as_str(),
            "" | "1" | "true" | "yes" | "y" | "on" | "enabled"
        ),
        None => true,
    };

    Ok(CsvRow { name, input, properties, weight, tags, enabled })
}

/// Turn a JSON property value into the string the store stores.
///
/// A property is stored as text (`expected.exact` is a string, `expected.max_steps` is "3"), so
/// a spreadsheet export that wrote `{"max_steps": 3}` has to arrive as `"3"`. The one exception
/// is `json_schema`, which is a document rather than a scalar: it is kept as JSON text so the
/// store's own `schema_is_supported` check reads the structure rather than a quoted copy of it.
fn value_to_property(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(_) | Value::Array(_) => value.to_string(),
        other => other.to_string(),
    }
}

/// The `expected` document for a parsed row.
fn expected_from_properties(properties: &[(String, String)]) -> Value {
    let mut map = serde_json::Map::new();
    for (key, value) in properties {
        // Numeric properties are stored as numbers-as-text by the store's own reader, so the
        // value is left as the string the CSV carried: a weight of "3" and a weight of "3.0"
        // are both parsed downstream by `expectation_from`, and guessing here would refuse a
        // row the scorer would have accepted.
        map.insert(key.clone(), Value::String(value.clone()));
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quoted_cell_keeps_its_comma_and_its_quotes() {
        let cells = split_csv("\"a, b\",\"say \"\"hi\"\"\",plain");
        assert_eq!(cells, vec!["a, b", "say \"hi\"", "plain"]);
    }

    #[test]
    fn the_header_matches_columns_by_name_not_by_order() {
        let columns: Vec<String> =
            split_csv("Weight,Name,EXACT").iter().map(|name| name.trim().to_lowercase()).collect();
        let row = read_csv_row(&columns, &["2.5".to_string(), "case one".to_string(), "hi".into()])
            .expect("the row must parse");
        assert_eq!(row.name, "case one");
        assert_eq!(row.weight, 2.5);
        assert_eq!(row.properties, vec![("exact".to_string(), "hi".to_string())]);
    }

    #[test]
    fn a_row_without_a_property_is_refused_by_name() {
        let columns: Vec<String> = vec!["name".into(), "exact".into()];
        let error = read_csv_row(&columns, &["".to_string(), "hi".into()])
            .expect_err("an empty name must be refused");
        assert!(error.contains("name"), "the refusal names the column: {error}");
    }

    #[test]
    fn a_weight_that_is_not_a_number_is_refused_before_the_store_hears_about_it() {
        let columns: Vec<String> = vec!["name".into(), "weight".into()];
        let error = read_csv_row(&columns, &["case".into(), "heavy".into()])
            .expect_err("a non-numeric weight must be refused");
        assert!(error.contains("weight"), "{error}");
    }

    #[test]
    fn the_field_a_refusal_names_is_the_field_the_form_marks() {
        assert_eq!(field_for("`threshold_percent` must be between 1 and 100, got 0"), Some("threshold_percent"));
        assert_eq!(field_for("`judge_model_id` must be a different model"), Some("judge_model_id"));
        assert_eq!(field_for("`weight` must be between 0.1 and 10, got 20"), Some("weight"));
        assert_eq!(field_for("the database is on fire"), None);
    }

    /// Every message the store can produce has to name something the form can mark.
    ///
    /// The walk is exhaustive over the *real* refusal strings rather than the three I happened
    /// to think of, and it is exhaustive over `PROPERTIES` as well. A message that names no
    /// field is the failure mode worth guarding: the case editor marks one input per error, so
    /// an unmapped refusal is a red form and no red box, which an operator reads as "the form
    /// is broken" rather than "this regex does not compile".
    #[test]
    fn every_refusal_the_store_can_write_names_a_field_the_form_has() {
        const STORE_MESSAGES: &[&str] = &[
            "a case's expected properties must be a JSON object",
            "expected `exact` to be a string",
            "expected `regex` entry is invalid: unclosed group",
            "expected `json_schema` to be a JSON schema object",
            "expected `max_steps` to be a step count",
            "expected `rubric` must not be blank",
            "expected `rubric` must be 8000 characters or fewer",
            "`exact` is not a property a case can check; expected one of exact, contains",
            "`name` must be between 1 and 80 characters, got 0",
            "`weight` must be between 0.1 and 10, got 20",
            "`input` must be 16000 characters or fewer, got 40000",
            "`threshold_percent` must be between 1 and 100, got 0",
            "`max_regression_points` must be between 0 and 50, got 90",
            "`temperature` must be between 0 and 2, got 7",
            "`judge_prompt` must not be blank",
            "`schedule` must be one of hourly, daily, weekly, custom or a five-field cron expression",
            "`judge_model_id` must be a different model from the one under test",
            "`model_id` must be set for target `model`",
            "a case must name at least one expected property",
        ];
        for message in STORE_MESSAGES {
            assert!(
                field_for(message).is_some(),
                "the form cannot mark a refusal: {message:?}"
            );
        }
    }

    #[test]
    fn a_property_refusal_names_the_property_and_not_the_form() {
        // The two shapes the scorer actually writes, taken from `eval_case`'s own messages.
        assert_eq!(field_for("expected `regex` entry is invalid: bad"), Some("regex"));
        assert_eq!(field_for("expected `rubric` must not be blank"), Some("rubric"));
    }

    #[test]
    fn every_property_the_scorer_implements_has_an_editor_field() {
        let offered: Vec<&str> = property_infos().iter().map(|info| info.key).collect();
        for property in eval_case::PROPERTIES {
            assert!(
                offered.contains(property),
                "`{property}` is scorable but the case editor cannot ask for it"
            );
        }
        assert_eq!(offered.len(), eval_case::PROPERTIES.len(), "no dead checkboxes");
    }

    #[test]
    fn a_bare_string_input_and_a_json_input_store_the_same_way() {
        assert_eq!(normalize_input(Value::String("hi".into())), serde_json::json!({"prompt": "hi"}));
        assert_eq!(normalize_input(Value::Null), serde_json::json!({"prompt": ""}));
        assert_eq!(normalize_input(serde_json::json!({"prompt": "hi"})), serde_json::json!({"prompt": "hi"}));
    }
}
