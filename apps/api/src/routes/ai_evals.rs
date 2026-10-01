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
use omnion_ai_hub::eval_run;
use omnion_ai_hub::eval_store::{
    self, CaseChanges, CaseRow, MAX_CASE_INPUT_CHARS, NewCase, NewSuite, SCHEDULE_PRESETS,
    SuiteChanges, SuiteRow,
};
use omnion_ai_hub::run_store;
use omnion_ai_hub::store;
use omnion_ai_hub::tool_stats::{self, ToolAggregate};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{Date, Duration, OffsetDateTime};
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
    /// The write keys this viewer is missing, so `Run now` can be disabled **with the reason
    /// attached** instead of being present and answering 403.
    ///
    /// Served beside the rows for the reason the approvals inbox does the same thing: a
    /// greyed-out button with no text is a dead button, which the definition of done forbids
    /// outright, while a button that says "you are missing `ai.evals.run`" is a control the
    /// operator can act on — by asking for the key, which is the actual next step for them.
    /// Recomputed per request from the caller's effective permissions rather than from the
    /// role's name, because two people with the same role can differ.
    pub viewer_missing: std::collections::BTreeSet<String>,
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
        suites.push(SuiteSummary {
            suite: suite.clone(),
            readiness,
            readiness_note,
        });
    }

    let viewer_missing = viewer_run_keys(pool, &current).await?;

    Ok(Json(SuiteList {
        viewer_missing,
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
                if suite.rubric_case_count == 1 {
                    ""
                } else {
                    "s"
                }
            ),
        );
    }
    (
        "ready",
        format!(
            "{} enabled case{} ready to run.",
            suite.enabled_case_count,
            if suite.enabled_case_count == 1 {
                ""
            } else {
                "s"
            }
        ),
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
    if let Some((_, field)) = MAP
        .iter()
        .find(|(needle, _)| message.contains(&format!("`{needle}`")))
    {
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

/// The write keys this viewer is missing, for `Run now` and `Make baseline`.
///
/// Recomputed per request from the caller's **effective** permissions rather than from the
/// role's name: two people with the same role can differ, and a panel that guessed from the
/// role would either hide a button the caller may use or offer one they cannot. The keys are
/// the same three the write routes are guarded by, read from the mount site rather than written
/// out again here — a second copy of the list is a list that will drift from the guards.
async fn viewer_run_keys(
    pool: &sqlx::PgPool,
    current: &CurrentSession,
) -> Result<std::collections::BTreeSet<String>, ApiError> {
    const KEYS: [&str; 3] = ["ai.evals.run", "ai.evals.manage", "ai.evals.read"];
    let effective = omnion_permissions::effective_permissions(
        pool,
        current.user.id,
        crate::guards::scope_of(&current.user),
    )
    .await?;
    Ok(KEYS
        .iter()
        .filter(|key| !effective.allows(**key))
        .map(|key| (*key).to_string())
        .collect())
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

/// Read a stored jsonb array of strings back into a list.
///
/// The inverse of [`string_list`], and separate from it on purpose: one takes the strings a form
/// posted, the other takes what the column holds. A malformed or non-array value reads as an
/// empty list rather than an error, because the column's own check constraint guarantees the
/// shape and a snapshot is the wrong place to fail a run over a tool list.
fn read_string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .filter(|item| !item.trim().is_empty())
                .collect()
        })
        .unwrap_or_default()
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
        (
            "exact",
            "Output equals this string",
            "expected.exact",
            false,
        ),
        (
            "contains",
            "Output contains this text",
            "expected.contains",
            false,
        ),
        (
            "regex",
            "Output matches this pattern",
            "expected.regex",
            false,
        ),
        (
            "json_schema",
            "Output parses and matches this schema",
            "expected.json_schema",
            false,
        ),
        (
            "citations_required",
            "Output cites its sources",
            "expected.citations_required",
            false,
        ),
        (
            "no_pii",
            "Output masks nothing the data guard would mask",
            "expected.no_pii",
            false,
        ),
        (
            "max_steps",
            "Run uses at most this many steps",
            "expected.max_steps",
            false,
        ),
        (
            "max_cost_micros",
            "Run costs at most this much",
            "expected.max_cost_micros",
            false,
        ),
        (
            "max_latency_ms",
            "Run answers within this many milliseconds",
            "expected.max_latency_ms",
            false,
        ),
        (
            "rubric",
            "A second model judges this free text",
            "expected.rubric",
            true,
        ),
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
            label: model
                .display_name
                .clone()
                .unwrap_or_else(|| model.model_key.clone()),
            detail: Some(model.model_key.clone()),
        })
        .collect())
}

/// The agents a suite may target.
async fn agent_options(pool: &sqlx::PgPool, organization: Uuid) -> Result<Vec<Option_>, ApiError> {
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
            value
                .map(|inner| inner.trim().to_owned())
                .filter(|inner| !inner.is_empty())
        }),
        judge_model_id: body.judge_model_id,
        judge_prompt: body.judge_prompt.map(|value| {
            value
                .map(|inner| inner.trim().to_owned())
                .filter(|inner| !inner.is_empty())
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
        enabled_weight: cases
            .iter()
            .filter(|case| case.enabled)
            .map(|case| case.weight)
            .sum(),
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
        source: if body.source == "import" {
            "manual".to_string()
        } else {
            body.source
        },
        source_run_id: body.source_run_id,
    };
    let created = eval_store::create_case(pool, organization, suite.id, &case)
        .await
        .map_err(annotate)?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "case": created })),
    ))
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
                problems.push(ImportProblem {
                    line: line_number,
                    message,
                });
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
    let name = get(&["name", "case"])
        .unwrap_or_default()
        .trim()
        .to_string();
    if name.is_empty() {
        return Err("`name` is empty".to_string());
    }
    if name.chars().count() > 80 {
        return Err(format!(
            "`name` is longer than 80 characters ({} chars)",
            name.chars().count()
        ));
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

    Ok(CsvRow {
        name,
        input,
        properties,
        weight,
        tags,
        enabled,
    })
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

// -------------------------------------------------------------------------------------------
// Runs (REQ-107 slice 2)
// -------------------------------------------------------------------------------------------

/// `GET /ai/evals/runs` query.
///
/// Every filter is optional and every one is validated by the store, which answers a refusal
/// naming the field — so a hand-typed `status=complete` is a `422` saying what the six are
/// rather than an empty table that reads as "this installation has never run an eval".
#[derive(Debug, Default, Deserialize)]
pub struct RunListQuery {
    /// One suite's runs.
    pub suite: Option<String>,
    /// queued / running / passed / failed / error / cancelled, or `incomplete` for the first two.
    pub status: Option<String>,
    /// manual / scheduled / gate.
    pub kind: Option<String>,
    /// none / pass / block.
    pub gate: Option<String>,
    /// Who started it.
    pub user: Option<Uuid>,
    /// `oldest` reverses the default.
    pub order: Option<String>,
    /// Page size.
    pub limit: Option<i64>,
    /// Page offset.
    pub offset: Option<i64>,
    /// The stat tiles' window, in days.
    pub days: Option<i64>,
}

/// One run on the list row.
#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    /// The run.
    pub run: eval_run::RunRow,
    /// What the status means for the operator, in one word.
    pub state: String,
    /// The same fact as a sentence, for a screen that wants the reason and not the colour.
    pub state_note: String,
}

/// `GET /ai/evals/runs` — the run history with its stat tiles.
pub async fn list_runs(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Query(query): Query<RunListQuery>,
) -> Result<Json<RunList>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let pool = state.db().pool();

    // A suite is named by its key, which is what a person types and what a link carries. An
    // unknown key is a 404 naming it rather than an empty list: "no runs" and "no such suite"
    // are different facts and the panel renders them differently.
    let suite_id = match query.suite.as_deref() {
        Some(key) => Some(
            eval_store::find_suite(pool, organization, key)
                .await?
                .ok_or_else(|| AiHubError::EvalSuiteNotFound(key.to_string()))?
                .id,
        ),
        None => None,
    };

    let filter = eval_run::RunFilter {
        suite_id,
        status: query.status.clone(),
        kind: query.kind.clone(),
        gate: query.gate.clone(),
        user_id: query.user,
        order: query.order.clone(),
        limit: query.limit.unwrap_or(50),
        offset: query.offset.unwrap_or(0),
    };
    let runs = eval_run::list_runs(pool, organization, &filter).await?;
    let stats = eval_run::run_stats(pool, organization, query.days.unwrap_or(7)).await?;

    Ok(Json(RunList {
        total: runs.len(),
        is_empty: runs.is_empty(),
        stats,
        runs: runs
            .iter()
            .map(|run| {
                let (state, state_note) = run_state(run);
                RunSummary {
                    run: run.clone(),
                    state,
                    state_note,
                }
            })
            .collect(),
    }))
}

/// What a run's status means, in one word and one sentence.
///
/// The four that matter are separated because a run list that renders `running` and `queued`
/// both as "in progress" cannot answer the question an operator opens the screen with, which is
/// "is it stuck?". `queued` means nothing has claimed it — a runner that is not running, or a
/// suite whose schedule has not fired — and `running` means a runner has it and is scoring.
fn run_state(run: &eval_run::RunRow) -> (String, String) {
    match run.status.as_str() {
        "queued" => (
            "queued".to_string(),
            "Waiting for a runner — nothing has claimed it yet.".to_string(),
        ),
        "running" => (
            "running".to_string(),
            format!(
                "Scoring — {} of {} case results so far.",
                { run.passed_cases + run.failed_cases + run.error_cases },
                run.total_cases
            ),
        ),
        "passed" => (
            "passed".to_string(),
            format!(
                "{:.1}% against a threshold of {}%.",
                run.pass_rate.unwrap_or(0.0),
                run.threshold_percent
            ),
        ),
        "failed" => (
            "failed".to_string(),
            format!(
                "{:.1}% against a threshold of {}% — {} case{} failed.",
                run.pass_rate.unwrap_or(0.0),
                run.threshold_percent,
                run.failed_cases,
                if run.failed_cases == 1 { "" } else { "s" }
            ),
        ),
        "error" => (
            "error".to_string(),
            run.error
                .clone()
                .unwrap_or_else(|| "The run ended without producing a verdict.".to_string()),
        ),
        "cancelled" => (
            "cancelled".to_string(),
            format!("Stopped by an operator — {} partial result kept.", {
                run.passed_cases + run.failed_cases + run.error_cases
            }),
        ),
        // A status the vocabulary does not know, rendered as itself rather than as `running`:
        // a newer writer's value shown as "in progress" would be a lie about a run that may well
        // have finished. This is also why the word is a `String` and not a `&'static str` — the
        // first version returned the borrowed status and would not compile, which is the type
        // system saying the arm cannot be a constant.
        other => (other.to_string(), format!("Unrecognised status `{other}`.")),
    }
}

/// `GET /ai/evals/runs` — the list plus the tiles above it.
#[derive(Debug, Serialize)]
pub struct RunList {
    /// How many rows came back.
    pub total: usize,
    /// Whether there is nothing at all, so the screen can show its empty state.
    pub is_empty: bool,
    /// The tiles: suites, runs in the window, average pass rate, cost.
    pub stats: eval_run::RunStats,
    /// The rows.
    pub runs: Vec<RunSummary>,
}

/// `POST /ai/evals/suites/{key}/run` body.
#[derive(Debug, Default, Deserialize)]
pub struct StartRunBody {
    /// `manual` or `gate`. A caller cannot ask for `scheduled`: that kind belongs to the
    /// scheduler, and a hand-posed scheduled run would put a row in the history that no
    /// schedule produced.
    pub kind: Option<String>,
    /// The run to compare against, required for a `gate`.
    pub base_run_id: Option<Uuid>,
}

/// `POST /ai/evals/suites/{key}/run` — start a run.
///
/// **The run is created, not executed.** This route enqueues; the runner (slice 3) claims and
/// scores. A route that scored inline would block an HTTP request for the length of a suite —
/// forty judge calls is minutes — and the request's own runner pattern says the row is created
/// and claimed. So the answer is `202` with the queued row, and the panel polls.
///
/// The model is resolved *here*, before the row is written, and the resolution is what the
/// snapshot records. A run that resolved its model at claim time would record a snapshot
/// describing whatever the router said when the runner got to it, which is not what the run was
/// started against.
pub async fn start_run(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(key): Path<String>,
    Json(body): Json<StartRunBody>,
) -> Result<(StatusCode, Json<RunSummary>), ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let pool = state.db().pool();
    let suite = eval_store::find_suite(pool, organization, &key)
        .await?
        .ok_or_else(|| AiHubError::EvalSuiteNotFound(key.clone()))?;

    if !suite.enabled {
        return Err(ApiError::bad_request(
            "invalid_eval",
            "this suite is switched off; enable it before running it",
        ));
    }

    let kind = body.kind.as_deref().unwrap_or("manual");
    if kind == "scheduled" {
        return Err(ApiError::bad_request(
            "invalid_eval",
            "`kind` may be manual or gate here; a scheduled run is started by the scheduler",
        ));
    }

    // The suite's own readiness, refused before a row exists. A run queued for a suite with no
    // enabled cases would sit in the history as a green 0-of-0 that measures nothing — the exact
    // false confidence the readiness badge exists to prevent, and it would be recorded rather
    // than displayed.
    let (readiness, note) = suite_readiness(&suite);
    if readiness != "ready" {
        return Err(ApiError::bad_request("invalid_eval", note));
    }

    let baseline = match body.base_run_id {
        Some(id) => Some(id),
        // No explicit baseline: the suite's own, when it has one. A gate that silently compared
        // against nothing would report a verdict from a single run and call it a comparison.
        None => eval_run::get_baseline(pool, suite.id)
            .await?
            .map(|row| row.run_id),
    };
    if kind == "gate" && baseline.is_none() {
        return Err(ApiError::bad_request(
            "invalid_eval",
            "this suite has no baseline run yet — run it once, or pass `base_run_id`",
        ));
    }

    // The model under test: a `model`-targeted suite carries its own pin; an `agent` or
    // `copilot` target resolves through the router, exactly as the agent runtime does, so an
    // eval and a production turn land in the same decision log.
    let (model_key, model_id, prompt) = resolve_under_test(pool, organization, &suite).await?;

    // The stored `tools` is jsonb; the snapshot takes a slice of strings. The other
    // `string_list` in this file builds a jsonb array from a list, so this reads the column
    // rather than reusing it — two functions with the same name and opposite directions is a
    // trap for the next reader, so this one says what it does.
    let tools = read_string_list(&suite.tools);
    let snapshot = eval_run::build_snapshot(
        model_key.as_deref(),
        model_id,
        &prompt,
        suite.judge_prompt_version.into(),
        &tools,
        suite.temperature,
        judge_model_key(pool, suite.judge_model_id)
            .await?
            .as_deref(),
        suite.judge_prompt.as_deref(),
        suite.judge_prompt_version.into(),
        current.user.id.into(),
    );

    let run = eval_run::create_run(
        pool,
        organization,
        &eval_run::NewRun {
            suite_id: suite.id,
            kind: kind.to_string(),
            snapshot,
            model_id,
            judge_model_id: suite.judge_model_id,
            threshold_percent: suite.threshold_percent,
            base_run_id: baseline,
            triggered_by: Some(current.user.id),
        },
    )
    .await?;

    let (state, state_note) = run_state(&run);
    Ok((
        StatusCode::ACCEPTED,
        Json(RunSummary {
            run,
            state,
            state_note,
        }),
    ))
}

/// The model under test for a suite, as `(key, row id, system prompt)`.
///
/// An agent-targeting suite reads the agent's own pin, and a `task`/`copilot` target goes to the
/// router — the same walk the agent runtime makes, so a suite and the traffic it measures resolve
/// the same way. A suite that resolved differently would be evaluating a model nobody runs.
async fn resolve_under_test(
    pool: &sqlx::PgPool,
    organization: Uuid,
    suite: &SuiteRow,
) -> Result<(Option<String>, Option<Uuid>, String), ApiError> {
    match suite.target.as_str() {
        "model" => {
            let Some(id) = suite.model_id else {
                return Err(ApiError::bad_request(
                    "invalid_eval",
                    "this suite pins no model, so there is nothing to run",
                ));
            };
            // The registry row is read for its `provider/model` string, which the snapshot keeps
            // because the row id alone is not reproducible once the row is gone.
            let resolved = model_key_of(pool, id).await?;
            Ok((resolved, Some(id), String::new()))
        }
        "agent" => {
            let Some(agent_id) = suite.agent_id else {
                return Err(ApiError::bad_request(
                    "invalid_eval",
                    "this suite targets an agent but names none",
                ));
            };
            let Some(agent) = run_store::get_agent(pool, organization, agent_id).await? else {
                return Err(ApiError::bad_request(
                    "invalid_eval",
                    "this suite targets an agent that is not in this organization",
                ));
            };
            let key = match agent.model_id {
                Some(id) => model_key_of(pool, id).await?,
                None => None,
            };
            Ok((key, agent.model_id, agent.system_prompt))
        }
        // A copilot or a task kind is resolved by the router at run time, and the snapshot says
        // so by carrying no model. Recording the *current* default as though it were pinned
        // would make a run look reproducible when the next one may pick a different model.
        _ => Ok((None, None, String::new())),
    }
}

/// One model's `provider/model` string, the key the snapshot and the price table both use.
async fn model_key_of(pool: &sqlx::PgPool, model_id: Uuid) -> Result<Option<String>, ApiError> {
    // `api.models.model_key` is the bare model name; the wire key the router and the price
    // table both use is `provider/model`, joined here so the snapshot records what a call would
    // actually be sent. Reading only `model_key` would make every snapshot ambiguous between
    // two providers offering a model of the same name.
    let row: Option<(String, String)> = sqlx::query_as(
        "select p.name || '/' || m.model_key from ai_models m \
         join ai_providers p on p.id = m.provider_id where m.id = $1",
    )
    .bind(model_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "eval.store_failed",
            format!("the model registry could not be read for this run: {error}"),
        )
    })?;
    Ok(row.map(|(key, _)| key))
}

/// The judge model's key, for the snapshot.
async fn judge_model_key(
    pool: &sqlx::PgPool,
    judge_model_id: Option<Uuid>,
) -> Result<Option<String>, ApiError> {
    match judge_model_id {
        Some(id) => model_key_of(pool, id).await,
        None => Ok(None),
    }
}

/// `GET /ai/evals/runs/{id}` — one run with its results and the suites it can be diffed against.
pub async fn read_run(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(id): Path<Uuid>,
) -> Result<Json<RunDetail>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let pool = state.db().pool();
    let run = eval_run::find_run(pool, organization, id)
        .await?
        .ok_or_else(|| AiHubError::EvalRunNotFound(id.to_string()))?;
    let results = eval_run::list_case_results(pool, organization, id).await?;
    let baseline = eval_run::get_baseline(pool, run.suite_id).await?;

    // The diff picker needs something to pick from, and only settled runs of the same suite can
    // be a baseline — a queued or errored run has no rate to compare against. Offering every
    // run would put a dead choice in the list.
    let candidates: Vec<RunOption> = eval_run::list_runs(
        pool,
        organization,
        &eval_run::RunFilter {
            suite_id: Some(run.suite_id),
            status: None,
            kind: None,
            gate: None,
            user_id: None,
            order: Some("oldest".to_string()),
            limit: 50,
            offset: 0,
        },
    )
    .await?
    .into_iter()
    .filter(|row| {
        row.id != run.id
            && row.pass_rate.is_some()
            && !matches!(row.status.as_str(), "queued" | "running")
    })
    .map(|row| RunOption {
        id: row.id,
        started_at: row.started_at,
        pass_rate: row.pass_rate,
        label: format!(
            "{} — {:.1}%",
            row.started_at.date().to_string(),
            row.pass_rate.unwrap_or(0.0)
        ),
    })
    .collect();

    let (state, state_note) = run_state(&run);
    // `is_empty` is read before the struct takes `results`, for the same reason the diff
    // summary is built before its struct literal: the value moves, and the borrow after the
    // move is a compile error rather than a runtime surprise.
    let is_empty = results.is_empty();
    Ok(Json(RunDetail {
        run,
        state,
        state_note,
        results,
        baseline,
        diff_candidates: candidates,
        is_empty,
    }))
}

/// `GET /ai/evals/runs/{id}` — the run detail payload.
#[derive(Debug, Serialize)]
pub struct RunDetail {
    /// The run, with its snapshot.
    pub run: eval_run::RunRow,
    /// The status in one word.
    pub state: String,
    /// The status as a sentence.
    pub state_note: String,
    /// One row per case executed.
    pub results: Vec<eval_run::CaseResultRow>,
    /// The suite's baseline, if it has one.
    pub baseline: Option<eval_run::BaselineRow>,
    /// Settled runs of the same suite the diff picker offers.
    pub diff_candidates: Vec<RunOption>,
    /// Whether the run produced no rows at all — a queued run, or one that errored first.
    pub is_empty: bool,
}

/// One entry in the diff picker.
#[derive(Debug, Clone, Serialize)]
pub struct RunOption {
    /// The run id.
    pub id: Uuid,
    /// When it started.
    pub started_at: OffsetDateTime,
    /// Its pass rate, for the label.
    pub pass_rate: Option<f64>,
    /// The text the picker shows.
    pub label: String,
}

/// `GET /ai/evals/runs/{id}/diff?base={id}` — this run against a baseline, case by case.
///
/// The baseline is a **query parameter, never an implicit "the previous run".** An implicit
/// baseline makes the diff mean something different on every call depending on what else has run
/// since, and a regression report nobody can reproduce is a regression report nobody acts on.
pub async fn diff_run(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(id): Path<Uuid>,
    Query(query): Query<DiffQuery>,
) -> Result<Json<RunDiffView>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let pool = state.db().pool();
    let head = eval_run::find_run(pool, organization, id)
        .await?
        .ok_or_else(|| AiHubError::EvalRunNotFound(id.to_string()))?;

    let Some(base) = query.base else {
        return Err(ApiError::bad_request(
            "invalid_eval",
            "`base` is required: a diff against an unnamed previous run is not reproducible",
        ));
    };
    let base_run = eval_run::find_run(pool, organization, base)
        .await?
        .ok_or_else(|| AiHubError::EvalRunNotFound(base.to_string()))?;
    if base_run.suite_id != head.suite_id {
        return Err(ApiError::bad_request(
            "invalid_eval",
            "the baseline run belongs to a different suite, so its cases are not comparable",
        ));
    }

    let base_results = eval_run::list_case_results(pool, organization, base).await?;
    let head_results = eval_run::list_case_results(pool, organization, id).await?;
    let diff = eval_run::diff_runs(&base_results, &head_results);

    // The gate verdict for *this* run against the baseline, recomputed rather than read: the
    // stored `gate` is what the run concluded when it settled, and the diff view is asking a
    // fresh question about a pairing the run may never have seen.
    let verdict = eval_run::decide_gate(
        head.pass_rate.unwrap_or(0.0),
        head.threshold_percent,
        base_run.pass_rate,
        head.max_regression_points,
    );

    let summary = format!(
        "{} improved, {} regressed, {} unchanged{}.",
        diff.improved,
        diff.regressed,
        diff.unchanged,
        added_and_removed(&diff)
    );
    Ok(Json(RunDiffView {
        run: head,
        base: base_run,
        diff,
        gate: verdict,
        summary,
    }))
}

/// `GET /ai/evals/runs/{id}/diff` query.
#[derive(Debug, Default, Deserialize)]
pub struct DiffQuery {
    /// The run to compare against.
    pub base: Option<Uuid>,
}

/// The tail of the diff summary sentence, so added/removed cases are not silently invisible.
fn added_and_removed(diff: &eval_run::RunDiff) -> String {
    match (diff.added, diff.removed) {
        (0, 0) => String::new(),
        (added, 0) => format!(", {added} new"),
        (0, removed) => format!(", {removed} dropped"),
        (added, removed) => format!(", {added} new, {removed} dropped"),
    }
}

/// `GET /ai/evals/runs/{id}/diff` — the comparison and its summary line.
#[derive(Debug, Serialize)]
pub struct RunDiffView {
    /// The run being read.
    pub run: eval_run::RunRow,
    /// The baseline it is compared to.
    pub base: eval_run::RunRow,
    /// One row per case in either run.
    pub diff: eval_run::RunDiff,
    /// What this pairing concludes about the threshold and the tolerance.
    pub gate: eval_run::GateVerdict,
    /// The sentence above the table.
    pub summary: String,
}

/// `POST /ai/evals/runs/{id}/cancel` — stop a run, keeping what it produced.
///
/// A run that has already settled answers `409` rather than a cheerful `true`, because "cancel"
/// on a finished run reads as success in a panel and the operator then waits for a stop that
/// already happened. The store's guard is what decides, and the route only translates the answer.
pub async fn cancel_run(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(id): Path<Uuid>,
) -> Result<Json<RunSummary>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let pool = state.db().pool();
    // Existence first, so a foreign tenant's run is a 404 and not a 409.
    let before = eval_run::find_run(pool, organization, id)
        .await?
        .ok_or_else(|| AiHubError::EvalRunNotFound(id.to_string()))?;
    if !matches!(before.status.as_str(), "queued" | "running") {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "eval_run_not_cancellable",
            format!("this run already finished as `{}`", before.status),
        ));
    }
    if !eval_run::cancel_run(pool, id).await? {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "eval_run_not_cancellable",
            "this run finished while the cancel was in flight",
        ));
    }
    let run = eval_run::find_run(pool, organization, id)
        .await?
        .ok_or_else(|| AiHubError::EvalRunNotFound(id.to_string()))?;
    let (state, state_note) = run_state(&run);
    Ok(Json(RunSummary {
        run,
        state,
        state_note,
    }))
}

/// `POST /ai/evals/suites/{key}/baseline` body.
#[derive(Debug, Default, Deserialize)]
pub struct BaselineBody {
    /// The settled run to take as the baseline.
    pub run_id: Uuid,
}

/// `POST /ai/evals/suites/{key}/baseline` — set the suite's baseline.
///
/// Setting a baseline is not a read-only act and is not treated as one: it is what every later
/// regression is measured against, so it takes `ai.evals.manage` and the run must be settled.
pub async fn set_baseline(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(key): Path<String>,
    Json(body): Json<BaselineBody>,
) -> Result<Json<eval_run::BaselineRow>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let pool = state.db().pool();
    let suite = eval_store::find_suite(pool, organization, &key)
        .await?
        .ok_or_else(|| AiHubError::EvalSuiteNotFound(key.clone()))?;
    Ok(Json(
        eval_run::set_baseline(
            pool,
            organization,
            suite.id,
            body.run_id,
            Some(current.user.id),
        )
        .await?,
    ))
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
        let columns: Vec<String> = split_csv("Weight,Name,EXACT")
            .iter()
            .map(|name| name.trim().to_lowercase())
            .collect();
        let row = read_csv_row(
            &columns,
            &["2.5".to_string(), "case one".to_string(), "hi".into()],
        )
        .expect("the row must parse");
        assert_eq!(row.name, "case one");
        assert_eq!(row.weight, 2.5);
        assert_eq!(
            row.properties,
            vec![("exact".to_string(), "hi".to_string())]
        );
    }

    #[test]
    fn a_row_without_a_property_is_refused_by_name() {
        let columns: Vec<String> = vec!["name".into(), "exact".into()];
        let error = read_csv_row(&columns, &["".to_string(), "hi".into()])
            .expect_err("an empty name must be refused");
        assert!(
            error.contains("name"),
            "the refusal names the column: {error}"
        );
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
        assert_eq!(
            field_for("`threshold_percent` must be between 1 and 100, got 0"),
            Some("threshold_percent")
        );
        assert_eq!(
            field_for("`judge_model_id` must be a different model"),
            Some("judge_model_id")
        );
        assert_eq!(
            field_for("`weight` must be between 0.1 and 10, got 20"),
            Some("weight")
        );
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
        assert_eq!(
            field_for("expected `regex` entry is invalid: bad"),
            Some("regex")
        );
        assert_eq!(
            field_for("expected `rubric` must not be blank"),
            Some("rubric")
        );
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
        assert_eq!(
            offered.len(),
            eval_case::PROPERTIES.len(),
            "no dead checkboxes"
        );
    }

    #[test]
    fn a_bare_string_input_and_a_json_input_store_the_same_way() {
        assert_eq!(
            normalize_input(Value::String("hi".into())),
            serde_json::json!({"prompt": "hi"})
        );
        assert_eq!(
            normalize_input(Value::Null),
            serde_json::json!({"prompt": ""})
        );
        assert_eq!(
            normalize_input(serde_json::json!({"prompt": "hi"})),
            serde_json::json!({"prompt": "hi"})
        );
    }
}

// -------------------------------------------------------------------------------------------
// Tool telemetry (REQ-107 slice 4)
// -------------------------------------------------------------------------------------------

/// `GET /ai/telemetry/tools` query.
#[derive(Debug, Default, Deserialize)]
pub struct TelemetryQuery {
    /// Inclusive first day. Defaults to 30 days back.
    pub from: Option<Date>,
    /// Inclusive last day. Defaults to today.
    pub to: Option<Date>,
    /// One tool's row.
    pub tool: Option<String>,
    /// Restrict to the tools that failed at least once.
    pub failing: Option<bool>,
}

/// One tool's row on the telemetry screen.
#[derive(Debug, Clone, Serialize)]
pub struct ToolTelemetryRow {
    /// Everything the store aggregated for the window.
    #[serde(flatten)]
    pub aggregate: ToolAggregate,
    /// Share of calls that succeeded, or `None` when the tool was never called.
    pub success_percent: Option<f64>,
    /// Share of calls the platform refused, or `None` when the tool was never called.
    pub denial_percent: Option<f64>,
}

/// The tool-telemetry screen's payload.
#[derive(Debug, Clone, Serialize)]
pub struct ToolTelemetry {
    /// The window the numbers cover, echoed so the screen can label the range it is showing.
    pub from: Date,
    pub to: Date,
    /// The days in the window, so "3 of 30 days" is computable without a second query.
    pub days: i64,
    /// Whether there is nothing at all, so the screen can show its empty state.
    pub is_empty: bool,
    /// The headline numbers.
    pub totals: TelemetryTotals,
    /// The rows, busiest first.
    pub tools: Vec<ToolTelemetryRow>,
    /// The most expensive failing tool per day — the table under the scatter.
    pub costliest_failing: Vec<CostliestFailing>,
}

/// The window's headline numbers.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TelemetryTotals {
    pub calls: i64,
    pub successes: i64,
    pub failures: i64,
    pub denials: i64,
    pub cost_micros: i64,
}

/// One day's costliest failing tool.
#[derive(Debug, Clone, Serialize)]
pub struct CostliestFailing {
    pub day: Date,
    pub tool: String,
    pub failures: i32,
    pub cost_micros: i64,
}

/// `GET /ai/telemetry/tools` — per-tool success, denial and latency stats for a window.
///
/// **The window is clamped, and the clamp is the honest part.** An operator asking for a year of
/// telemetry on a roll-up that is written daily is asking for at most 366 rows per tool, so a
/// wide range is cheap — but a range of `1970` is not a window, it is a scan of everything the
/// table holds. Both ends are therefore clamped to `[today - 366, today]` and the *clamped*
/// window is echoed back in the response, so a screen that asked for too much shows the range it
/// actually got rather than silently reporting less than it displayed.
///
/// An inverted range (`from > to`) is **swapped rather than refused**: the store answers an empty
/// window, and "no telemetry" is a worse reading of a mistyped picker than "the range you meant".
pub async fn tool_telemetry(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Query(query): Query<TelemetryQuery>,
) -> Result<Json<ToolTelemetry>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let pool = state.db().pool();

    let today = OffsetDateTime::now_utc().date();
    // A year is the widest window the daily roll-up can answer honestly: the table keeps one row
    // per `(day, organization, tool)`, so beyond a year the window is adding noise, not history.
    let earliest = today - Duration::days(365);
    let mut to = query.to.unwrap_or(today).clamp(earliest, today);
    let mut from = query
        .from
        .unwrap_or(to - Duration::days(29))
        .clamp(earliest, to);

    // `from` was clamped against the *unclamped* `to`; a picker that sent both out of order needs
    // the swap after the clamp, or `from` can still sit above `to`.
    if from > to {
        std::mem::swap(&mut from, &mut to);
    }

    let window = tool_stats::Window { from, to };
    let mut tools = tool_stats::tools_in_window(pool, organization, window).await?;

    if let Some(only) = query
        .tool
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        tools.retain(|row| row.tool == only);
    }
    if query.failing == Some(true) {
        tools.retain(|row| row.failures > 0);
    }

    let totals = TelemetryTotals {
        calls: tools.iter().map(|row| row.calls).sum(),
        successes: tools.iter().map(|row| row.successes).sum(),
        failures: tools.iter().map(|row| row.failures).sum(),
        denials: tools.iter().map(|row| row.denials).sum(),
        cost_micros: tools.iter().map(|row| row.cost_micros).sum(),
    };
    let is_empty = tools.is_empty();

    let rows = tools
        .into_iter()
        .map(|aggregate| {
            let success_percent = aggregate.success_percent();
            let denial_percent = aggregate.denial_percent();
            ToolTelemetryRow {
                aggregate,
                success_percent,
                denial_percent,
            }
        })
        .collect();

    let costliest_failing = tool_stats::costliest_failing_per_day(pool, organization, window)
        .await?
        .into_iter()
        .map(|(day, tool, failures, cost_micros)| CostliestFailing {
            day,
            tool,
            failures,
            cost_micros,
        })
        .collect();

    Ok(Json(ToolTelemetry {
        from,
        to,
        // Inclusive of both ends, so a one-day window is 1 and never 0.
        days: (to - from).whole_days() + 1,
        is_empty,
        totals,
        tools: rows,
        costliest_failing,
    }))
}
