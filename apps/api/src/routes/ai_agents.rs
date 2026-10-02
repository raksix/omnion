//! `/api/v1/ai/agents` and `/api/v1/ai/runs` — the agent runtime's HTTP surface (REQ-099, slice 1).
//!
//! The route is thin on purpose. The loop, the stop conditions and the durable record all live in
//! `omnion_ai_hub`; this module decides who may ask for what, translates a body into the store's
//! shapes, and turns the run's event stream into frames a browser can read. Nothing here decides
//! *when* a run stops — a second implementation of that would be a second set of rules to keep in
//! agreement with the first.
//!
//! ## Why starting a run streams steps, and re-attaching does not
//!
//! `POST /ai/agents/{id}/runs` answers `text/event-stream` and the frames *are* the run's
//! lifecycle: a `run` frame with the run id, then the loop's own events, then `done`. The client
//! therefore sees the same sequence the store writes, and the screen hands off to the run detail
//! with a run id that already exists — no polling loop, no second request to find out what
//! happened.
//!
//! `GET /ai/runs/{id}/events` is the re-attach, and it does **not** stream from the runner. The
//! runner's sink is dropped on purpose (see `ai_agent_runner`), so there is nothing to subscribe
//! to; the endpoint reads the step rows and sends them, then polls until the run reaches a
//! terminal state. That is a replay rather than a live tail, and calling it a replay is the whole
//! point: the spec's box says "replay matches SSE", and a stream that only exists while somebody
//! is watching cannot be compared with the row that was written while nobody was.
//!
//! ## The two refusals that are not 400s
//!
//! * **`runner_disabled` (503).** `OMNION_AI_RUNNER=false` means the installation has no worker.
//!   Queuing the run anyway would produce a row that stays `queued` forever and a screen that says
//!   "running" — so the start endpoint refuses, and the message names the environment variable.
//! * **`run_in_progress` (409).** The migration's partial unique index allows one active run per
//!   agent, which is what stops a double-clicked Run button from spending twice. The conflict is
//!   answered with the *existing* run's id, so the client can attach to it instead of guessing.

use std::convert::Infallible;
use std::time::Duration as StdDuration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use omnion_ai_hub::agent::{AgentEvent, MAX_GOAL_CHARS, StopReason};
use omnion_ai_hub::run_store::{
    self, Agent, NewAgent, NewRun, Run, Step, call_arguments, list_agents, list_runs, list_steps,
    validate_goal,
};
use omnion_ai_hub::tools::AllowList;
use omnion_ai_hub::workspace::{self as agent_workspace, RunInput};
use omnion_audit::NewAuditEntry;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::scope::resolve_organization;
use crate::state::AppState;

/// How many events may queue in front of a client before the stream waits for it.
const STREAM_BUFFER: usize = 64;

/// How often a re-attaching stream re-reads the run's steps while it is still going.
const REATTACH_POLL_MS: u64 = 400;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// The organization selector every agent and run route accepts.
///
/// Only a platform-level account (primary organization `null`) needs it: an organization account
/// always acts inside its own, and passing a different id is a `403 cross_organization` rather
/// than a silent re-scope.
#[derive(Debug, Default, Deserialize)]
pub struct OrgQuery {
    /// The organization to act for.
    pub organization_id: Option<Uuid>,
}

/// `GET /ai/runs` — the history list's own filters.
#[derive(Debug, Default, Deserialize)]
pub struct RunQuery {
    /// Only this agent's runs.
    pub agent_id: Option<Uuid>,
    /// Only runs in this state.
    pub status: Option<String>,
    /// Only runs that ended this way.
    pub stop_reason: Option<String>,
    /// Row cap.
    pub limit: Option<i64>,
    /// The organization, for a platform account.
    pub organization_id: Option<Uuid>,
}

/// `POST /ai/agents` — the create form.
#[derive(Debug, Deserialize)]
pub struct NewAgentBody {
    /// API-visible key; immutable afterwards.
    pub key: String,
    /// Display name.
    pub name: String,
    /// What the agent is for.
    #[serde(default)]
    pub description: String,
    /// The prompt that frames every run.
    #[serde(default)]
    pub system_prompt: String,
    /// Pinned model.
    pub model_id: Option<Uuid>,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Step ceiling.
    pub max_steps: Option<i32>,
    /// Wall-clock ceiling in seconds.
    pub deadline_seconds: Option<i32>,
    /// Token ceiling.
    pub token_budget: Option<i64>,
    /// Tool allow-list, in order.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Tools that park the run for a person.
    #[serde(default)]
    pub approvals: Vec<String>,
    /// Memory scope.
    pub memory_scope: Option<String>,
    /// Whether the agent can be started.
    pub enabled: Option<bool>,
}

/// `PATCH /ai/agents/{id}` — the edit form. Every field is optional; `None` means "unchanged".
#[derive(Debug, Default, Deserialize)]
pub struct AgentPatch {
    /// Display name.
    pub name: Option<String>,
    /// What the agent is for.
    pub description: Option<String>,
    /// The prompt.
    pub system_prompt: Option<String>,
    /// Pinned model.
    pub model_id: Option<Option<Uuid>>,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Step ceiling.
    pub max_steps: Option<i32>,
    /// Wall-clock ceiling.
    pub deadline_seconds: Option<i32>,
    /// Token ceiling.
    pub token_budget: Option<i64>,
    /// Tool allow-list.
    pub tools: Option<Vec<String>>,
    /// Approval list.
    pub approvals: Option<Vec<String>>,
    /// Memory scope.
    pub memory_scope: Option<String>,
    /// Enabled flag.
    pub enabled: Option<bool>,
}

/// `POST /ai/agents/{id}/runs` — the Run sheet.
#[derive(Debug, Deserialize)]
pub struct StartRunBody {
    /// What the run must do.
    pub goal: String,
    /// Workspace files the run is told to read. Recorded on the run, not resolved here: reading
    /// them is the tool's job and a file that is deleted between the sheet and the run must fail
    /// as a tool error rather than as a 400 on a run that was legitimately started.
    #[serde(default)]
    pub files: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Response bodies
// ---------------------------------------------------------------------------------------------

/// One agent as the panel sees it. The tool keys are the agent's own list, not the registry's.
#[derive(Debug, Serialize)]
pub struct AgentView {
    /// Row id.
    pub id: Uuid,
    /// Tenant.
    pub organization_id: Uuid,
    /// Optional site binding.
    pub site_id: Option<Uuid>,
    /// API-visible key.
    pub key: String,
    /// Display name.
    pub name: String,
    /// What it is for.
    pub description: String,
    /// The prompt.
    pub system_prompt: String,
    /// Pinned model.
    pub model_id: Option<Uuid>,
    /// Sampling temperature.
    pub temperature: f64,
    /// Step ceiling.
    pub max_steps: i32,
    /// Wall-clock ceiling.
    pub deadline_seconds: i32,
    /// Token ceiling.
    pub token_budget: i64,
    /// The 30-day roll-up, so the table's cell is one request rather than one per row.
    pub telemetry: Option<AgentTelemetryView>,
    /// Tool allow-list.
    pub tools: Vec<String>,
    /// How many of those park the run.
    pub approvals_count: usize,
    /// Approval list.
    pub approvals: Vec<String>,
    /// Memory scope.
    pub memory_scope: String,
    /// Whether it can be started.
    pub enabled: bool,
    /// When it was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When it was last changed.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl AgentView {
    /// Describe one row.
    ///
    /// `telemetry` is `None` by default because the *detail* screen has no use for a 30-day
    /// roll-up, and a builder that always computed one would make every read of an agent cost
    /// a second query. The list route fills it in from the bulk roll-up it already fetched —
    /// one query for the whole table rather than one per row, which on a 200-agent tenant is
    /// the difference between an instant screen and an eleven-second one.
    fn build(agent: &Agent) -> Self {
        Self {
            telemetry: None,
            id: agent.id,
            organization_id: agent.organization_id,
            site_id: agent.site_id,
            key: agent.key.clone(),
            name: agent.name.clone(),
            description: agent.description.clone(),
            system_prompt: agent.system_prompt.clone(),
            model_id: agent.model_id,
            temperature: agent.temperature,
            max_steps: agent.max_steps,
            deadline_seconds: agent.deadline_seconds,
            token_budget: agent.token_budget,
            tools: agent.tools.clone(),
            approvals_count: agent.approvals.len(),
            approvals: agent.approvals.clone(),
            memory_scope: agent.memory_scope.clone(),
            enabled: agent.enabled,
            created_at: agent.created_at,
            updated_at: agent.updated_at,
        }
    }
}

/// One run as the list renders it.
#[derive(Debug, Serialize)]
pub struct RunView {
    /// Row id.
    pub id: Uuid,
    /// The agent, or `null` once the agent is deleted.
    pub agent_id: Option<Uuid>,
    /// Who started it.
    pub user_id: Option<Uuid>,
    /// What started it.
    pub trigger: String,
    /// What it was asked to do.
    pub goal: String,
    /// Where it is.
    pub status: String,
    /// Why it ended, once it has.
    pub stop_reason: Option<String>,
    /// The model it settled on.
    pub model_id: Option<Uuid>,
    /// How many steps have started.
    pub current_step: i32,
    /// How many times it was resumed.
    pub resume_count: i32,
    /// Prompt tokens charged.
    pub prompt_tokens: i32,
    /// Completion tokens charged.
    pub completion_tokens: i32,
    /// Cost in millionths.
    pub cost_micros: i64,
    /// When it began.
    #[serde(with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    /// When it ended.
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    /// The failure text, when there is one.
    pub error: Option<String>,
}

impl RunView {
    /// Describe one row.
    fn build(run: &Run) -> Self {
        Self {
            id: run.id,
            agent_id: run.agent_id,
            user_id: run.user_id,
            trigger: run.trigger.clone(),
            goal: run.goal.clone(),
            status: run.status.clone(),
            stop_reason: run.stop_reason.clone(),
            model_id: run.model_id,
            current_step: run.current_step,
            resume_count: run.resume_count,
            prompt_tokens: run.prompt_tokens,
            completion_tokens: run.completion_tokens,
            cost_micros: run.cost_micros,
            started_at: run.started_at,
            finished_at: run.finished_at,
            error: run.error.clone(),
        }
    }
}

/// One run's recomputed telemetry, as the run detail's header renders it.
#[derive(Debug, Serialize)]
pub struct RunTelemetryView {
    /// Steps that finished.
    pub completed_steps: i64,
    /// Steps that failed.
    pub failed_steps: i64,
    /// Tool calls attempted.
    pub tool_calls: i64,
    /// Prompt tokens, summed from the completed steps.
    pub prompt_tokens: i32,
    /// Completion tokens, summed from the completed steps.
    pub completion_tokens: i32,
    /// Cost in millionths, summed from the completed steps.
    pub cost_micros: i64,
    /// Wall-clock milliseconds from the first step to the last.
    pub duration_ms: i64,
    /// Total tokens, so the panel does not add the two halves in eight places.
    pub total_tokens: i64,
}

impl RunTelemetryView {
    /// Describe one run's recomputed numbers.
    fn build(telemetry: &omnion_ai_hub::telemetry::RunTelemetry) -> Self {
        Self {
            completed_steps: telemetry.completed_steps,
            failed_steps: telemetry.failed_steps,
            tool_calls: telemetry.tool_calls,
            prompt_tokens: telemetry.prompt_tokens,
            completion_tokens: telemetry.completion_tokens,
            cost_micros: telemetry.cost_micros,
            duration_ms: telemetry.duration_ms,
            total_tokens: i64::from(telemetry.prompt_tokens) + i64::from(telemetry.completion_tokens),
        }
    }
}

/// One agent's 30-day roll-up, as the agents table's cell renders it.
#[derive(Debug, Serialize)]
pub struct AgentTelemetryView {
    /// Finished runs in the window.
    pub runs: i64,
    /// Of those, the ones that produced an answer.
    pub completed: i64,
    /// Of those, the ones a person stopped.
    pub cancelled: i64,
    /// Of those, the ones that failed.
    pub failed: i64,
    /// Total tokens in the window.
    pub total_tokens: i64,
    /// Cost in millionths in the window.
    pub cost_micros: i64,
    /// Steps in the window.
    pub steps: i64,
    /// Completed as a percentage of finished runs, or `None` when nothing has finished.
    ///
    /// `None` is the honest answer for a brand-new agent and `0%` is a claim about a division
    /// that never happened, so the panel renders an em dash rather than a number here.
    pub success_rate: Option<f64>,
    /// When the window starts, so the column header is not the only statement of it.
    pub since: time::OffsetDateTime,
}

impl AgentTelemetryView {
    /// Describe one agent's roll-up.
    fn build(telemetry: &omnion_ai_hub::telemetry::AgentTelemetry, since: time::OffsetDateTime) -> Self {
        Self {
            runs: telemetry.runs,
            completed: telemetry.completed,
            cancelled: telemetry.cancelled,
            failed: telemetry.failed,
            total_tokens: telemetry.total_tokens(),
            cost_micros: telemetry.cost_micros,
            steps: telemetry.steps,
            success_rate: telemetry.success_rate(),
            since,
        }
    }
}

/// One step, as the trace accordion renders it.
///
/// `arguments` is the **redacted** form the store wrote, not a re-read of the tool call: a
/// transcript is read by people who are not the agent's author, and the redaction happened before
/// the row existed.
#[derive(Debug, Serialize)]
pub struct StepView {
    /// 1-based position.
    pub step_no: i32,
    /// What kind of step it is.
    pub kind: String,
    /// The tool, for the tool kinds.
    pub tool: Option<String>,
    /// The call arguments, redacted.
    pub arguments: Option<serde_json::Value>,
    /// The result summary.
    pub result: Option<serde_json::Value>,
    /// Where it got to.
    pub status: String,
    /// Prompt tokens this step charged.
    pub prompt_tokens: i32,
    /// Completion tokens this step charged.
    pub completion_tokens: i32,
    /// Cost this step was billed at.
    pub cost_micros: i64,
    /// How long it took.
    pub duration_ms: Option<i32>,
    /// The failure text, when it failed.
    pub error: Option<String>,
}

impl StepView {
    /// Describe one row.
    fn build(step: &Step) -> Self {
        Self {
            step_no: step.step_no,
            kind: step.kind.clone(),
            tool: step.tool.clone(),
            arguments: step.arguments.clone(),
            result: step.result.clone(),
            status: step.status.clone(),
            prompt_tokens: step.prompt_tokens,
            completion_tokens: step.completion_tokens,
            cost_micros: step.cost_micros,
            duration_ms: step.duration_ms,
            error: step.error.clone(),
        }
    }
}

/// One workspace reference as the run detail renders it.
#[derive(Debug, Serialize)]
pub struct RunInputView {
    /// Row id.
    pub id: Uuid,
    /// The path, exactly as the sheet named it.
    pub path: String,
    /// Whether a file is behind it right now.
    ///
    /// `false` is the state that explains a failed run: the sheet named `q3.csv`, and somebody
    /// deleted it before the runner claimed the run. The row survives the delete on purpose —
    /// see migration `0154` — so the trace can say *which* input went missing instead of
    /// silently showing a run that had none.
    pub resolved: bool,
    /// The file's size when it resolves, in bytes.
    pub size_bytes: u64,
}

/// One run with its steps — the detail screen's whole payload.
#[derive(Debug, Serialize)]
pub struct RunDetail {
    /// The run.
    #[serde(flatten)]
    pub run: RunView,
    /// What the run actually did, recomputed from its step rows.
    ///
    /// A separate block rather than more columns on the run view, because the run's columns are
    /// the *stored* totals and this is the *recomputed* one — and the acceptance criterion is
    /// that they agree. Shipping them side by side turns that criterion from a test into
    /// something an operator can check by eye on the screen, and a drift shows up as two
    /// different numbers in the same header instead of as a wrong number.
    pub telemetry: RunTelemetryView,
    /// The trace.
    pub steps: Vec<StepView>,
    /// What the run was told to read.
    pub inputs: Vec<RunInputView>,
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// Refuse a name the migration's check would reject, with the limit in the message.
fn validate_name(name: &str) -> Result<(), ApiError> {
    let length = name.trim().chars().count();
    if length == 0 {
        return Err(ApiError::bad_request("agent.invalid", "the name is empty"));
    }
    if length > 80 {
        return Err(ApiError::bad_request(
            "agent.invalid",
            format!("the name is {length} characters; the limit is 80"),
        ));
    }
    Ok(())
}

/// Refuse a key that is not `^[a-z][a-z0-9_-]{0,63}$`, with the rule in the message.
///
/// Checked here rather than only by the constraint because the key appears in URLs and in
/// workflow node configuration: a key with a space in it is a key somebody has to quote forever,
/// and a 500 carrying a constraint name is not a message an operator can act on.
fn validate_key(key: &str) -> Result<(), ApiError> {
    let valid = !key.is_empty()
        && key.len() <= 64
        && key
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase())
        && key
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-');
    if valid {
        return Ok(());
    }
    Err(ApiError::bad_request(
        "agent.invalid_key",
        "a key starts with a lowercase letter and then uses lowercase letters, digits, `_` or `-`",
    ))
}

/// Refuse a limit the runtime would clamp, rather than silently clamping it.
///
/// The runtime *does* clamp — a row written by a seed script cannot make it loop forever. But a
/// form that says 200 and a run that stops at 50 is a form that lied, so the write path refuses
/// and the panel shows the message under the field.
fn validate_limits(
    max_steps: i32,
    deadline_seconds: i32,
    token_budget: i64,
) -> Result<(), ApiError> {
    use omnion_ai_hub::agent::{
        MAX_DEADLINE_SECONDS, MAX_STEPS_CEILING, MAX_TOKEN_BUDGET, MIN_DEADLINE_SECONDS,
        MIN_TOKEN_BUDGET,
    };
    if !(1..=MAX_STEPS_CEILING as i32).contains(&max_steps) {
        return Err(ApiError::bad_request(
            "agent.invalid_max_steps",
            format!("max steps must be between 1 and {MAX_STEPS_CEILING}"),
        ));
    }
    if !(MIN_DEADLINE_SECONDS as i32..=MAX_DEADLINE_SECONDS as i32).contains(&deadline_seconds) {
        return Err(ApiError::bad_request(
            "agent.invalid_deadline",
            format!(
                "the deadline must be between {MIN_DEADLINE_SECONDS} and {MAX_DEADLINE_SECONDS} seconds"
            ),
        ));
    }
    if !(MIN_TOKEN_BUDGET..=MAX_TOKEN_BUDGET).contains(&token_budget) {
        return Err(ApiError::bad_request(
            "agent.invalid_token_budget",
            format!("the token budget must be between {MIN_TOKEN_BUDGET} and {MAX_TOKEN_BUDGET}"),
        ));
    }
    Ok(())
}

/// The memory scopes the column accepts.
fn validate_memory_scope(scope: &str) -> Result<(), ApiError> {
    if matches!(scope, "none" | "organization" | "site" | "user") {
        return Ok(());
    }
    Err(ApiError::bad_request(
        "agent.invalid_memory_scope",
        "the memory scope must be none, organization, site or user",
    ))
}

// ---------------------------------------------------------------------------------------------
// Handlers · agents
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/ai/agents` — the agents table.
pub async fn list_agents_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
) -> Result<Json<Vec<AgentView>>, ApiError> {
    // The organization an agent, a run and a step belong to. An organization account always gets
    // its own; a platform-level account (primary organization `null`) has to name one in the
    // query, and is refused when it does not. This is `scope::resolve_organization`, the same
    // helper the tenancy surface uses — an agent route that grew its own copy of the rule would
    // be a second answer to "may this caller touch this tenant".
    let organization = resolve_organization(&current, scope_query.organization_id)?;

    let agents = list_agents(state.db().pool(), organization).await?;
    // One roll-up query for the whole table. The `left join` inside it keeps an agent with no
    // runs in the result with zeroes, so a brand-new agent renders an em dash rather than
    // disappearing from its own table.
    let since = omnion_ai_hub::telemetry::window_start(omnion_ai_hub::telemetry::ROLLUP_DAYS);
    let rollup =
        omnion_ai_hub::telemetry::agents_telemetry(state.db().pool(), organization, since).await?;
    Ok(Json(
        agents
            .iter()
            .map(|agent| {
                let mut view = AgentView::build(agent);
                view.telemetry = rollup.get(&agent.id).map(|row| AgentTelemetryView::build(row, since));
                view
            })
            .collect(),
    ))
}

/// `GET /api/v1/ai/agents/{id}/telemetry` — one agent's roll-up, on its own.
///
/// The list already carries the number, so this route exists for the *detail* screen and for
/// anything that wants the roll-up without the whole agent list — a workflow node asking "is
/// this agent worth calling", or an operator who has just changed the step cap and wants the
/// rate to move without waiting for a page refresh. It is `ai.agents.read` and answers 404 for
/// another tenant's agent, like every other agent route.
pub async fn agent_telemetry_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(id): Path<Uuid>,
) -> Result<Json<AgentTelemetryView>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    // The agent has to exist *and* belong to this tenant. A roll-up for an agent that is not
    // there is a 404 rather than a set of zeroes, because zeroes for a deleted agent read as
    // "it has never run" on a screen that also lists deleted agents' history.
    run_store::get_agent(state.db().pool(), organization, id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "agent.not_found", "no such agent"))?;
    let since = omnion_ai_hub::telemetry::window_start(omnion_ai_hub::telemetry::ROLLUP_DAYS);
    let telemetry =
        omnion_ai_hub::telemetry::agent_telemetry(state.db().pool(), organization, id).await?;
    Ok(Json(AgentTelemetryView::build(&telemetry, since)))
}

/// `GET /api/v1/ai/agents/tool-usage` — how often each tool was called in the window.
///
/// Tenant-wide rather than per agent, because the question it answers is "what is this
/// installation actually spending its tool calls on", and answering it per agent would make
/// the caller fan out over the agent list to get one table.
///
/// The path is a sibling of `/ai/agents`, not of `/ai/agents/{id}`, and it is registered before
/// the capture so `tool-usage` is never read as an agent id. It was briefly registered on
/// `/ai/telemetry/tools`, which is REQ-107 slice 4's roll-up: two `get` handlers on one path is
/// an overlapping method route, and axum rejects that when the router is **constructed**, so the
/// defect was invisible to `cargo build` and to any test of either handler — only a test that
/// builds the router (or boots the API) could see it.
pub async fn tool_usage_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
) -> Result<Json<ToolUsageView>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let since = omnion_ai_hub::telemetry::window_start(omnion_ai_hub::telemetry::ROLLUP_DAYS);
    let usage =
        omnion_ai_hub::telemetry::tool_usage(state.db().pool(), organization, since).await?;
    Ok(Json(ToolUsageView { since, tools: usage }))
}

/// The tool-usage table, as the API hands it over.
#[derive(Debug, Serialize)]
pub struct ToolUsageView {
    /// When the window starts, so the client does not have to guess its own.
    pub since: time::OffsetDateTime,
    /// Call counts by tool key, in a stable order.
    pub tools: std::collections::BTreeMap<String, i64>,
}

/// `POST /api/v1/ai/agents` — create one.
pub async fn create_agent_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    address: ClientAddress,
    Json(body): Json<NewAgentBody>,
) -> Result<Json<AgentView>, ApiError> {
    // The organization an agent, a run and a step belong to. An organization account always gets
    // its own; a platform-level account (primary organization `null`) has to name one in the
    // query, and is refused when it does not. This is `scope::resolve_organization`, the same
    // helper the tenancy surface uses — an agent route that grew its own copy of the rule would
    // be a second answer to "may this caller touch this tenant".
    let organization = resolve_organization(&current, scope_query.organization_id)?;

    validate_key(body.key.trim())?;
    validate_name(&body.name)?;
    let temperature = body.temperature.unwrap_or(0.20);
    if !(0.0..=1.0).contains(&temperature) {
        return Err(ApiError::bad_request(
            "agent.invalid_temperature",
            "the temperature must be between 0.00 and 1.00",
        ));
    }
    let max_steps = body.max_steps.unwrap_or(8);
    let deadline_seconds = body.deadline_seconds.unwrap_or(300);
    let token_budget = body.token_budget.unwrap_or(200_000);
    validate_limits(max_steps, deadline_seconds, token_budget)?;
    let memory_scope = body.memory_scope.unwrap_or_else(|| "none".to_owned());
    validate_memory_scope(&memory_scope)?;
    if body.system_prompt.chars().count() > 8_000 {
        return Err(ApiError::bad_request(
            "agent.invalid_system_prompt",
            "the system prompt is longer than 8000 characters",
        ));
    }
    if body.description.chars().count() > 400 {
        return Err(ApiError::bad_request(
            "agent.invalid_description",
            "the description is longer than 400 characters",
        ));
    }
    // An approval-gated tool that is not in the allow-list is a row that promises a gate that
    // cannot fire: the loop only reaches the approval branch for a tool it was allowed. Refused
    // here, with both lists named, because a form that quietly drops a half-configured gate is
    // how a destructive tool ends up running unattended.
    let allow = AllowList::new(body.tools.clone(), body.approvals.clone());
    if let Some(stray) = body.approvals.iter().find(|key| !allow.allows(key)) {
        return Err(ApiError::bad_request(
            "agent.approval_not_allowed",
            format!("{stray} needs approval but is not in the tool list"),
        ));
    }

    let mut new = NewAgent::with_defaults(
        organization,
        body.key.trim().to_owned(),
        body.name.trim().to_owned(),
    );
    new.description = body.description;
    new.system_prompt = body.system_prompt;
    new.model_id = body.model_id;
    new.temperature = temperature;
    new.max_steps = max_steps;
    new.deadline_seconds = deadline_seconds;
    new.token_budget = token_budget;
    new.tools = body.tools;
    new.approvals = body.approvals;
    new.memory_scope = memory_scope;
    new.enabled = body.enabled.unwrap_or(true);
    new.created_by = Some(current.user.id);

    let agent = run_store::create_agent(state.db().pool(), &new).await?;
    let entry = NewAuditEntry::by_user(current.user.id, "ai.agent.created")
        .organization(organization)
        .target("ai_agent", agent.id)
        .metadata(json!({ "key": agent.key, "tools": agent.tools.len() }))
        .ip_address(address.as_text());
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(Json(AgentView::build(&agent)))
}

/// `GET /api/v1/ai/agents/{id}` — one agent.
///
/// A row in another organization is a 404, not a 403: "forbidden" confirms that the id exists,
/// and sequential ids make that confirmation free.
pub async fn get_agent_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(id): Path<Uuid>,
) -> Result<Json<AgentView>, ApiError> {
    // The organization an agent, a run and a step belong to. An organization account always gets
    // its own; a platform-level account (primary organization `null`) has to name one in the
    // query, and is refused when it does not. This is `scope::resolve_organization`, the same
    // helper the tenancy surface uses — an agent route that grew its own copy of the rule would
    // be a second answer to "may this caller touch this tenant".
    let organization = resolve_organization(&current, scope_query.organization_id)?;

    let agent = run_store::get_agent(state.db().pool(), organization, id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "agent.not_found", "no such agent"))?;
    Ok(Json(AgentView::build(&agent)))
}

/// `PATCH /api/v1/ai/agents/{id}` — change one.
///
/// `model_id` is a double option on purpose: `Some(None)` is "stop pinning this agent", and
/// `None` is "leave the pin alone". A single `Option<Uuid>` cannot express the difference, and the
/// difference is the difference between a pinned agent and a routed one.
pub async fn patch_agent_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    address: ClientAddress,
    Path(id): Path<Uuid>,
    Json(body): Json<AgentPatch>,
) -> Result<Json<AgentView>, ApiError> {
    // The organization an agent, a run and a step belong to. An organization account always gets
    // its own; a platform-level account (primary organization `null`) has to name one in the
    // query, and is refused when it does not. This is `scope::resolve_organization`, the same
    // helper the tenancy surface uses — an agent route that grew its own copy of the rule would
    // be a second answer to "may this caller touch this tenant".
    let organization = resolve_organization(&current, scope_query.organization_id)?;

    let existing = run_store::get_agent(state.db().pool(), organization, id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "agent.not_found", "no such agent"))?;

    let previous_name = existing.name.clone();
    let name = body.name.unwrap_or_else(|| previous_name.clone());
    validate_name(&name)?;
    if let Some(description) = &body.description {
        if description.chars().count() > 400 {
            return Err(ApiError::bad_request(
                "agent.invalid_description",
                "the description is longer than 400 characters",
            ));
        }
    }
    if let Some(prompt) = &body.system_prompt {
        if prompt.chars().count() > 8_000 {
            return Err(ApiError::bad_request(
                "agent.invalid_system_prompt",
                "the system prompt is longer than 8000 characters",
            ));
        }
    }
    let temperature = body.temperature.unwrap_or(existing.temperature);
    if !(0.0..=1.0).contains(&temperature) {
        return Err(ApiError::bad_request(
            "agent.invalid_temperature",
            "the temperature must be between 0.00 and 1.00",
        ));
    }
    let max_steps = body.max_steps.unwrap_or(existing.max_steps);
    let deadline_seconds = body.deadline_seconds.unwrap_or(existing.deadline_seconds);
    let token_budget = body.token_budget.unwrap_or(existing.token_budget);
    validate_limits(max_steps, deadline_seconds, token_budget)?;
    let memory_scope = body.memory_scope.unwrap_or(existing.memory_scope.clone());
    validate_memory_scope(&memory_scope)?;
    let tools = body.tools.clone().unwrap_or(existing.tools.clone());
    let approvals = body.approvals.clone().unwrap_or(existing.approvals.clone());
    let allow = AllowList::new(tools.clone(), approvals.clone());
    if let Some(stray) = approvals.iter().find(|key| !allow.allows(key)) {
        return Err(ApiError::bad_request(
            "agent.approval_not_allowed",
            format!("{stray} needs approval but is not in the tool list"),
        ));
    }

    let mut changed: Vec<&str> = Vec::new();
    if name != previous_name {
        changed.push("name");
    }
    if body.description.is_some() {
        changed.push("description");
    }
    if body.system_prompt.is_some() {
        changed.push("system_prompt");
    }
    if body.model_id.is_some() {
        changed.push("model_id");
    }
    if temperature != existing.temperature {
        changed.push("temperature");
    }
    if max_steps != existing.max_steps {
        changed.push("max_steps");
    }
    if deadline_seconds != existing.deadline_seconds {
        changed.push("deadline_seconds");
    }
    if token_budget != existing.token_budget {
        changed.push("token_budget");
    }
    if body.tools.is_some() {
        changed.push("tools");
    }
    if body.approvals.is_some() {
        changed.push("approvals");
    }
    if memory_scope != existing.memory_scope {
        changed.push("memory_scope");
    }
    if let Some(enabled) = body.enabled {
        if enabled != existing.enabled {
            changed.push("enabled");
        }
    }

    let agent = update_agent(
        state.db().pool(),
        organization,
        &existing,
        AgentPatch {
            name: Some(name),
            description: body.description,
            system_prompt: body.system_prompt,
            model_id: body.model_id,
            temperature: Some(temperature),
            max_steps: Some(max_steps),
            deadline_seconds: Some(deadline_seconds),
            token_budget: Some(token_budget),
            tools: Some(tools),
            approvals: Some(approvals),
            memory_scope: Some(memory_scope),
            enabled: body.enabled,
        },
    )
    .await?;

    if !changed.is_empty() {
        let entry = NewAuditEntry::by_user(current.user.id, "ai.agent.updated")
            .organization(organization)
            .target("ai_agent", id)
            .metadata(json!({ "changed": changed }))
            .ip_address(address.as_text());
        omnion_audit::record(state.db().pool(), entry).await?;
    }
    Ok(Json(AgentView::build(&agent)))
}

/// Write the merged agent back.
///
/// Kept as a private helper rather than a `PATCH` body straight into SQL so the merge logic has
/// one name: the read-modify-write above is the interesting half, and the statement is not.
async fn update_agent(
    pool: &sqlx::PgPool,
    organization_id: Uuid,
    existing: &Agent,
    patch: AgentPatch,
) -> Result<Agent, ApiError> {
    let name = patch.name.unwrap_or_else(|| existing.name.clone());
    let sql = format!(
        "update ai_agents set name = $3, description = $4, system_prompt = $5, model_id = $6, \
         temperature = $7, max_steps = $8, deadline_seconds = $9, token_budget = $10, \
         tools = $11::jsonb, approvals = $12::jsonb, memory_scope = $13, enabled = $14, \
         updated_at = now() where id = $1 and organization_id = $2 returning {}",
        run_store::AGENT_COLUMNS
    );
    sqlx::query_as::<_, Agent>(&sql)
        .bind(existing.id)
        .bind(organization_id)
        .bind(name)
        .bind(patch.description.unwrap_or_else(|| existing.description.clone()))
        .bind(patch.system_prompt.unwrap_or_else(|| existing.system_prompt.clone()))
        .bind(patch.model_id.unwrap_or(existing.model_id))
        .bind(patch.temperature.unwrap_or(existing.temperature))
        .bind(patch.max_steps.unwrap_or(existing.max_steps))
        .bind(patch.deadline_seconds.unwrap_or(existing.deadline_seconds))
        .bind(patch.token_budget.unwrap_or(existing.token_budget))
        .bind(json!(patch.tools.unwrap_or_else(|| existing.tools.clone())))
        .bind(json!(patch.approvals.unwrap_or_else(|| existing.approvals.clone())))
        .bind(patch.memory_scope.unwrap_or_else(|| existing.memory_scope.clone()))
        .bind(patch.enabled.unwrap_or(existing.enabled))
        .fetch_one(pool)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "agent.store_failed",
                format!("the agent row could not be written: {error}"),
            )
        })
}

/// `DELETE /api/v1/ai/agents/{id}` — remove one.
///
/// The agent's runs survive with a null `agent_id`, because they are the only record that the
/// agent ever spent money.
pub async fn delete_agent_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    address: ClientAddress,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    // The organization an agent, a run and a step belong to. An organization account always gets
    // its own; a platform-level account (primary organization `null`) has to name one in the
    // query, and is refused when it does not. This is `scope::resolve_organization`, the same
    // helper the tenancy surface uses — an agent route that grew its own copy of the rule would
    // be a second answer to "may this caller touch this tenant".
    let organization = resolve_organization(&current, scope_query.organization_id)?;

    let removed = run_store::delete_agent(state.db().pool(), organization, id).await?;
    if !removed {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "agent.not_found",
            "no such agent",
        ));
    }
    let entry = NewAuditEntry::by_user(current.user.id, "ai.agent.removed")
        .organization(organization)
        .target("ai_agent", id)
        .ip_address(address.as_text());
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Handlers · runs
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/ai/runs` — the history list.
pub async fn list_runs_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<RunQuery>,
) -> Result<Json<Vec<RunView>>, ApiError> {
    let organization = resolve_organization(&current, query.organization_id)?;
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let mut runs = list_runs(
        state.db().pool(),
        organization,
        query.agent_id,
        limit,
    )
    .await?;
    // The status and stop-reason filters are applied here rather than in SQL because
    // `list_runs` is shared with the runner's own reads, and a second query shape for the same
    // screen is a second shape to keep correct. The list is already capped by the query.
    if let Some(status) = &query.status {
        runs.retain(|run| &run.status == status);
    }
    if let Some(reason) = &query.stop_reason {
        runs.retain(|run| run.stop_reason.as_deref() == Some(reason.as_str()));
    }
    Ok(Json(runs.iter().map(RunView::build).collect()))
}

/// `GET /api/v1/ai/runs/{id}` — one run with its steps.
pub async fn get_run_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(id): Path<Uuid>,
) -> Result<Json<RunDetail>, ApiError> {
    // The organization an agent, a run and a step belong to. An organization account always gets
    // its own; a platform-level account (primary organization `null`) has to name one in the
    // query, and is refused when it does not. This is `scope::resolve_organization`, the same
    // helper the tenancy surface uses — an agent route that grew its own copy of the rule would
    // be a second answer to "may this caller touch this tenant".
    let organization = resolve_organization(&current, scope_query.organization_id)?;

    let run = run_store::get_run(state.db().pool(), organization, id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "run.not_found", "no such run"))?;
    let steps = list_steps(state.db().pool(), run.id).await?;
    let inputs = agent_workspace::list_run_inputs(state.db().pool(), run.id).await?;
    // The agent may be gone (a run outlives its definition on purpose), and a reference is still
    // worth rendering: "your input went missing" is the explanation a person needs, so the
    // detail reads the run's references rather than the agent's files.
    let files = match run.agent_id {
        Some(agent_id) => agent_workspace::list_files(state.db().pool(), agent_id).await?,
        None => Vec::new(),
    };
    let present: Vec<(Uuid, u64)> = files
        .iter()
        .map(|file| (file.id, file.bytes()))
        .collect();
    let resolution = agent_workspace::resolve(&inputs, &present);
    // Recomputed from the step rows rather than read off the run's own columns, so the panel's
    // numbers and the store's columns are two independent reads of the same fact and a drift
    // between them is visible instead of being asserted (REQ-099 slice 4).
    let telemetry = omnion_ai_hub::telemetry::run_telemetry(state.db().pool(), run.id).await?;
    Ok(Json(RunDetail {
        run: RunView::build(&run),
        telemetry: RunTelemetryView::build(&telemetry),
        steps: steps.iter().map(StepView::build).collect(),
        // In the order the sheet named them, not "resolved first": the goal quotes them in that
        // order and a trace that silently reorders them is a trace the reader has to re-derive.
        inputs: inputs
            .iter()
            .map(|input| {
                let size = present
                    .iter()
                    .find(|(id, _)| Some(*id) == input.file_id)
                    .map_or(0, |(_, bytes)| *bytes);
                RunInputView {
                    id: input.id,
                    path: input.path.clone(),
                    resolved: !resolution.missing.iter().any(|m| m.id == input.id),
                    size_bytes: size,
                }
            })
            .collect(),
    }))
}

/// `GET /api/v1/ai/runs/{id}/steps` — the trace alone, for the polling fallback.
pub async fn get_run_steps(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<StepView>>, ApiError> {
    // The organization an agent, a run and a step belong to. An organization account always gets
    // its own; a platform-level account (primary organization `null`) has to name one in the
    // query, and is refused when it does not. This is `scope::resolve_organization`, the same
    // helper the tenancy surface uses — an agent route that grew its own copy of the rule would
    // be a second answer to "may this caller touch this tenant".
    let organization = resolve_organization(&current, scope_query.organization_id)?;

    let run = run_store::get_run(state.db().pool(), organization, id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "run.not_found", "no such run"))?;
    let steps = list_steps(state.db().pool(), run.id).await?;
    Ok(Json(steps.iter().map(StepView::build).collect()))
}

/// `POST /api/v1/ai/agents/{id}/runs` — start a run, streamed.
///
/// Everything decidable before the first byte is decided here with a normal HTTP status: an agent
/// that is disabled, a goal that is empty, a runner that is switched off, or a run already active
/// for this agent. After that the run exists and the stream carries the loop's events.
pub async fn start_run(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    address: ClientAddress,
    Path(agent_id): Path<Uuid>,
    Json(body): Json<StartRunBody>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    if !state.config().ai_hub.agent_runner_enabled {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "runner_disabled",
            "this installation has no agent runner (OMNION_AI_RUNNER=false), so a run would \
             never start. Enable the runner or start it from a workflow node on an installation \
             that has one.",
        ));
    }

    let agent = run_store::get_agent(state.db().pool(), organization, agent_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "agent.not_found", "no such agent"))?;
    if !agent.enabled {
        return Err(ApiError::bad_request(
            "agent.disabled",
            "this agent is disabled; enable it before running it",
        ));
    }
    validate_goal(&body.goal)?;
    if body.goal.chars().count() > MAX_GOAL_CHARS {
        return Err(ApiError::bad_request(
            "run.invalid_goal",
            format!("the goal is longer than {MAX_GOAL_CHARS} characters"),
        ));
    }

    // One active run per agent. The partial unique index is the guarantee; this read exists to
    // answer with the *existing* run's id, so the client attaches to the run that is already
    // going rather than pressing Run again.
    if let Some(active) = active_run(state.db().pool(), agent.id).await? {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "run_in_progress",
            format!(
                "this agent already has a run in progress ({}). Cancel it or wait for it before \
                 starting another.",
                active.id
            ),
        )
        .with_details(json!({ "run_id": active.id, "status": active.status })));
    }

    let mut new = NewRun::default_for(organization, agent.id);
    new.user_id = Some(current.user.id);
    new.trigger = "agent".to_owned();
    new.goal = body.goal.trim().to_owned();
    new.model_id = agent.model_id;
    new.token_budget = agent.token_budget;
    new.deadline_at = Some(OffsetDateTime::now_utc() + time::Duration::seconds(agent.deadline_seconds as i64));

    let run = match run_store::create_run(state.db().pool(), &new).await {
        Ok(run) => run,
        // The unique index fired between the read above and this insert — another request, or
        // another browser tab, got there first. Same answer as the read: the id, so the client
        // attaches instead of retrying into another conflict.
        Err(error) if is_active_conflict(&error) => {
            let Some(active) = active_run(state.db().pool(), agent.id).await? else {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "run_in_progress",
                    "this agent already has a run in progress",
                ));
            };
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "run_in_progress",
                "this agent already has a run in progress",
            )
            .with_details(json!({ "run_id": active.id, "status": active.status })));
        }
        Err(error) => return Err(error.into()),
    };

    // The run exists; its named inputs go on the row now rather than being resolved by a tool
    // later. Two reasons, both about the failure an operator has to explain: the trace can say
    // *which* paths the sheet named, and a reference that has no file behind it reads as missing
    // instead of silently disappearing from the record.
    let inputs = agent_workspace::set_run_inputs(
        state.db().pool(),
        run.id,
        agent.id,
        Some(current.user.id),
        &body.files,
    )
    .await?;

    let entry = NewAuditEntry::by_user(current.user.id, "ai.run.started")
        .organization(organization)
        .target("ai_run", run.id)
        .metadata(json!({
            "agent": agent.key,
            "model_id": agent.model_id,
            "goal": run.goal,
            "files": inputs.len(),
        }))
        .ip_address(address.as_text());
    if let Err(error) = omnion_audit::record(state.db().pool(), entry).await {
        tracing::warn!(%error, "the agent run audit row could not be written");
    }

    let (frames, receiver) = mpsc::channel::<Event>(STREAM_BUFFER);
    let pool = state.db().pool().clone();
    let runner = state.config().ai_hub.agent_runner_enabled;

    // The loop runs **here**, on the request's own task, rather than waiting for the background
    // runner to claim the row. That is the difference between "press Run and watch" and "press Run
    // and watch, if the runner happens to be idle": the stream is the reason the endpoint exists,
    // and a client that has to poll a queue it just wrote to is a client that has to be told the
    // runner might be asleep.
    //
    // The row is already `queued`, so a process that dies mid-stream leaves a durable run the
    // background runner will pick up — the two paths are one queue, not two implementations.
    tokio::spawn(async move {
        if !runner {
            return;
        }
        // Claim our own row. The claim is the same statement the background runner uses, so a run
        // is executed by exactly one of them.
        let Some(claimed) = run_store::claim_run(&pool, run.id).await.ok().flatten() else {
            let _ = frames
                .send(Event::default().event("error").data(
                    json!({ "code": "run_not_claimed", "message": "the run was claimed elsewhere" })
                        .to_string(),
                ))
                .await;
            return;
        };

        let _ = frames
            .send(
                Event::default()
                    .event("run")
                    .data(
                        json!({
                            "run_id": run.id,
                            "agent_id": run.agent_id,
                            "status": claimed.status,
                        })
                        .to_string(),
                    ),
            )
            .await;

        // The loop's own events go onto this channel; the runner's `Persist` writes the same
        // events to the step rows. The live view and the reloaded trace are therefore the same
        // sequence by construction rather than by a promise that two code paths agree.
        let (loop_events, mut watched) = tokio::sync::mpsc::channel::<AgentEvent>(128);
        // The relay owns its own sender; the `done` frame below is sent on a clone so the client
        // gets the terminal frame even though the relay has already taken the original.
        let relay_frames = frames.clone();
        let relay = tokio::spawn(async move {
            while let Some(event) = watched.recv().await {
                let name = event_name(&event);
                let payload = serde_json::to_string(&event).unwrap_or_default();
                if relay_frames
                    .send(Event::default().event(name).data(payload))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        let row = crate::ai_agent_runner::execute_with_sink(&pool, &claimed, Some(loop_events)).await;
        let _ = relay.await;
        let _ = frames
            .send(
                Event::default()
                    .event("done")
                    .data(
                        json!({
                            "run_id": run.id,
                            "status": row.status,
                            "stop_reason": row.stop_reason,
                        })
                        .to_string(),
                    ),
            )
            .await;
    });

    let stream = ReceiverStream::new(receiver).map(Ok);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// The SSE event name one loop event is published under.
///
/// Named after the event, not after a transport word: a client that has to map seven transport
/// verbs onto seven lifecycle events is a client that will eventually map one wrong, and a trace
/// that says `message` where the code says `text` is an afternoon nobody enjoys.
fn event_name(event: &omnion_ai_hub::agent::AgentEvent) -> &'static str {
    use omnion_ai_hub::agent::AgentEvent;
    match event {
        AgentEvent::StepStarted { .. } => "step_started",
        AgentEvent::Text { .. } => "text",
        AgentEvent::ToolCall { .. } => "tool_call",
        AgentEvent::ToolResult { .. } => "tool_result",
        AgentEvent::Usage { .. } => "usage",
        AgentEvent::AwaitingApproval { .. } => "awaiting_approval",
        AgentEvent::Done { .. } => "loop_done",
        AgentEvent::Error { .. } => "error",
        // A guardrail is not an error: the run may be perfectly healthy and a rule may still
        // have fired. Publishing it as `error` would make the trace's error badge appear on a
        // run that succeeded, and an operator would go looking for a break that is not there.
        AgentEvent::Guardrail { .. } => "guardrail",
    }
}

/// The run this agent is already running, if any.
async fn active_run(pool: &sqlx::PgPool, agent_id: Uuid) -> Result<Option<Run>, ApiError> {
    let runs = sqlx::query_as::<_, Run>(
        "select id, organization_id, site_id, agent_id, user_id, trigger, goal, status, \
         stop_reason, model_id, current_step, resume_count, cancel_requested_at, deadline_at, \
         token_budget, prompt_tokens, completion_tokens, cost_micros, output_repairs, heartbeat_at, started_at, \
         finished_at, error from ai_runs where agent_id = $1 \
         and status in ('queued','running','awaiting_approval') order by started_at asc nulls first limit 1",
    )
    .bind(agent_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "run.store_failed",
            format!("the active runs could not be read: {error}"),
        )
    })?;
    Ok(runs)
}

/// Whether an error is the one-active-run index firing.
fn is_active_conflict(error: &omnion_ai_hub::AiHubError) -> bool {
    matches!(error, omnion_ai_hub::AiHubError::Database(inner) if inner.to_string().contains("ai_runs_one_active_per_agent_uidx"))
}

/// `GET /api/v1/ai/runs/{id}/events` — re-attach to a live run.
///
/// A replay, not a subscription: the runner's sink is dropped (there is nobody to publish to on
/// a request that is not running), so the frames come from the step rows. That is what makes the
/// spec's "replay matches SSE" box provable — the same rows produce both.
pub async fn run_events(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(id): Path<Uuid>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    // The organization an agent, a run and a step belong to. An organization account always gets
    // its own; a platform-level account (primary organization `null`) has to name one in the
    // query, and is refused when it does not. This is `scope::resolve_organization`, the same
    // helper the tenancy surface uses — an agent route that grew its own copy of the rule would
    // be a second answer to "may this caller touch this tenant".
    let organization = resolve_organization(&current, scope_query.organization_id)?;

    let run = run_store::get_run(state.db().pool(), organization, id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "run.not_found", "no such run"))?;

    let (frames, receiver) = mpsc::channel::<Event>(STREAM_BUFFER);
    let pool = state.db().pool().clone();
    let organization_id = organization;

    tokio::spawn(async move {
        let mut sent = 0_usize;
        loop {
            let steps = list_steps(&pool, id).await.unwrap_or_default();
            for step in steps.iter().skip(sent) {
                let frame = Event::default()
                    .event("step")
                    .data(serde_json::to_string(&StepView::build(step)).unwrap_or_default());
                if frames.send(frame).await.is_err() {
                    return;
                }
            }
            sent = steps.len();

            let Ok(Some(run)) = run_store::get_run(&pool, organization_id, id).await else {
                let _ = frames
                    .send(
                        Event::default()
                            .event("error")
                            .data(json!({ "code": "run.not_found" }).to_string()),
                    )
                    .await;
                return;
            };
            if run.is_terminal() || run.status == "awaiting_approval" {
                let _ = frames
                    .send(
                        Event::default()
                            .event("done")
                            .data(
                                json!({
                                    "run_id": run.id,
                                    "status": run.status,
                                    "stop_reason": run.stop_reason,
                                })
                                .to_string(),
                            ),
                    )
                    .await;
                return;
            }
            tokio::time::sleep(StdDuration::from_millis(REATTACH_POLL_MS)).await;
        }
    });

    let stream = ReceiverStream::new(receiver).map(Ok);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// `POST /api/v1/ai/runs/{id}/cancel` — ask a run to stop.
///
/// The request is *recorded*, not acted on: the loop notices at the next step boundary, and a tool
/// that has already started is left to finish. A half-applied side effect is worse than a
/// slightly later stop.
pub async fn cancel_run(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    address: ClientAddress,
    Path(id): Path<Uuid>,
) -> Result<Json<RunView>, ApiError> {
    // The organization an agent, a run and a step belong to. An organization account always gets
    // its own; a platform-level account (primary organization `null`) has to name one in the
    // query, and is refused when it does not. This is `scope::resolve_organization`, the same
    // helper the tenancy surface uses — an agent route that grew its own copy of the rule would
    // be a second answer to "may this caller touch this tenant".
    let organization = resolve_organization(&current, scope_query.organization_id)?;

    let run = run_store::get_run(state.db().pool(), organization, id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "run.not_found", "no such run"))?;
    if run.is_terminal() {
        return Err(ApiError::bad_request(
            "run.not_running",
            format!("this run already ended ({}).", run.status),
        ));
    }
    let asked = run_store::request_cancel(state.db().pool(), id).await?;
    if !asked {
        return Err(ApiError::bad_request(
            "run.not_running",
            "this run is not running; there is nothing to cancel",
        ));
    }
    let entry = NewAuditEntry::by_user(current.user.id, "ai.run.cancelled")
        .organization(organization)
        .target("ai_run", id)
        .metadata(json!({ "step": run.current_step }))
        .ip_address(address.as_text());
    omnion_audit::record(state.db().pool(), entry).await?;
    let refreshed = run_store::get_run(state.db().pool(), organization, id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "run.not_found", "no such run"))?;
    Ok(Json(RunView::build(&refreshed)))
}

/// `POST /api/v1/ai/runs/{id}/resume` — requeue an interrupted or parked run.
///
/// Three refusals, all with a different reason, because "resume did nothing" is the worst of
/// them: a run whose steps all completed is finished, a run that is still going does not need
/// resuming, and a step left `running` by a crashed worker is *ambiguous* — the tool may already
/// have executed, so the runtime reports it for a person rather than running it twice.
pub async fn resume_run(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    address: ClientAddress,
    Path(id): Path<Uuid>,
) -> Result<Json<RunView>, ApiError> {
    // The organization an agent, a run and a step belong to. An organization account always gets
    // its own; a platform-level account (primary organization `null`) has to name one in the
    // query, and is refused when it does not. This is `scope::resolve_organization`, the same
    // helper the tenancy surface uses — an agent route that grew its own copy of the rule would
    // be a second answer to "may this caller touch this tenant".
    let organization = resolve_organization(&current, scope_query.organization_id)?;

    if !state.config().ai_hub.agent_runner_enabled {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "runner_disabled",
            "this installation has no agent runner (OMNION_AI_RUNNER=false), so a resumed run \
             would never start",
        ));
    }
    let run = run_store::get_run(state.db().pool(), organization, id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "run.not_found", "no such run"))?;

    if run.status == "queued" || run.status == "running" {
        return Err(ApiError::bad_request(
            "run.still_running",
            "this run is already going; cancel it first if you want to start it again",
        ));
    }
    match run_store::resume_point(state.db().pool(), id).await? {
        None => {
            return Err(ApiError::bad_request(
                "run.complete",
                "every step of this run completed; there is nothing to resume",
            ));
        }
        Some(step_no) => {
            // The ambiguous case: a step that started and never finished. The run is not silently
            // retried, because the tool may have done its work.
            let ambiguous: bool = sqlx::query_scalar::<_, bool>(
                "select exists (select 1 from ai_run_steps where run_id = $1 and step_no = $2 \
                 and status = 'running')",
            )
            .bind(id)
            .bind(step_no)
            .fetch_one(state.db().pool())
            .await
            .unwrap_or(true);
            if ambiguous {
                return Err(ApiError::bad_request(
                    "run.ambiguous_step",
                    format!(
                        "step {step_no} of this run started and never finished, so its tool may \
                         already have run. Inspect the trace, then start a new run."
                    ),
                ));
            }
        }
    }

    let requeued = run_store::requeue_run(state.db().pool(), id).await?;
    if !requeued {
        return Err(ApiError::bad_request(
            "run.not_resumable",
            format!("a run that is {} cannot be resumed", run.status),
        ));
    }
    let entry = NewAuditEntry::by_user(current.user.id, "ai.run.resumed")
        .organization(organization)
        .target("ai_run", id)
        .metadata(json!({ "resume_count": run.resume_count + 1 }))
        .ip_address(address.as_text());
    omnion_audit::record(state.db().pool(), entry).await?;
    let refreshed = run_store::get_run(state.db().pool(), organization, id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "run.not_found", "no such run"))?;
    Ok(Json(RunView::build(&refreshed)))
}

/// The run's agent, as the panel's link column renders it.
pub async fn run_agent_link(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(id): Path<Uuid>,
) -> Result<Json<Option<AgentView>>, ApiError> {
    // The organization an agent, a run and a step belong to. An organization account always gets
    // its own; a platform-level account (primary organization `null`) has to name one in the
    // query, and is refused when it does not. This is `scope::resolve_organization`, the same
    // helper the tenancy surface uses — an agent route that grew its own copy of the rule would
    // be a second answer to "may this caller touch this tenant".
    let organization = resolve_organization(&current, scope_query.organization_id)?;

    let run = run_store::get_run(state.db().pool(), organization, id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "run.not_found", "no such run"))?;
    let Some(agent_id) = run.agent_id else {
        return Ok(Json(None));
    };
    let agent = run_store::get_agent(state.db().pool(), organization, agent_id).await?;
    Ok(Json(agent.as_ref().map(AgentView::build)))
}

/// The tool's redacted arguments, as the trace's "what was called" row reads them.
///
/// Exposed because the trace stores the *event* payload on a `note` step and the screen's
/// expanded row wants the arguments without re-implementing the redaction on the client — a
/// client that redacted for itself would disagree with the store about which keys are secret.
#[must_use]
pub fn preview_arguments(call: &omnion_ai_hub::agent::ToolCall) -> serde_json::Value {
    call_arguments(call)
}

/// The stop reasons the list's filter offers.
#[must_use]
pub fn stop_reasons() -> Vec<StopReason> {
    vec![
        StopReason::FinalAnswer,
        StopReason::MaxSteps,
        StopReason::Deadline,
        StopReason::TokenBudget,
        StopReason::Cancelled,
        StopReason::LoopDetected,
        StopReason::Error,
        // The output-schema failure (REQ-099 slice 4). In the filter and not folded into
        // `error`, because an operator chasing "the answer did not match the shape" through
        // a filter labelled "error" is looking at the wrong thing: nothing failed, the rule
        // did its job.
        StopReason::OutputSchema,
    ]
}

/// Where the run's own limits live, for a caller that has the agent but not the run.
#[must_use]
pub fn agent_limits(agent: &Agent) -> omnion_ai_hub::agent::RunLimits {
    agent.limits()
}

/// The allow-list a run enforces, for a caller that needs it without the store.
#[must_use]
pub fn allow_list(agent: &Agent) -> AllowList {
    AllowList::new(agent.tools.clone(), agent.approvals.clone())
}

/// The step kinds the trace renders as an icon rather than text.
#[must_use]
pub fn step_kinds() -> Vec<&'static str> {
    vec!["message", "tool_call", "tool_result", "approval", "note", "error"]
}
