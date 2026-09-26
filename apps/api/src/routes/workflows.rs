//! `/api/v1/workflows` — the automation surface (docs/requests/REQ-003, phase P09).
//!
//! A workflow is a definition: a trigger (manual, or a cron schedule in UTC) plus an ordered
//! list of steps. Running one materialises the steps as rows (`workflow_steps`) and the
//! background runner (`crate::workflow_runner`) advances them — this module is the panel side:
//! create, read, edit, remove, start, watch and cancel.
//!
//! Two rules the handlers enforce on top of the permission guard (`crate::guards`):
//!
//! * tenancy — a workflow belongs to one organization, and an account with a primary
//!   organization only ever touches its own (`crate::scope`);
//! * definition rules live in the engine crate, not here: the body is validated by
//!   [`omnion_workflows::WorkflowDefinition`], so an unusable cron or an unknown action is
//!   refused with the engine's own stable code (`invalid_cron`, `invalid_step_action`, …).
//!
//! Every definition change and every cancellation is audited; the lifecycle of a run is
//! audited by the engine itself (`workflow.execution.started|completed|failed|cancelled`).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_identity::Site;
use omnion_identity::sites;
use omnion_workflows::definition::{StepDefinition, Trigger, WorkflowDefinition};
use omnion_workflows::model::NewWorkflow;
use omnion_workflows::store::{self, WorkflowUpdate};
use omnion_workflows::{
    ExecutionStatus, TriggerKind, Workflow, WorkflowError, WorkflowExecution, WorkflowStep, engine,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::scope::{ensure_same_organization, resolve_organization};
use crate::state::AppState;

/// Runs a list returns when the caller does not ask for a size.
const EXECUTION_PAGE_DEFAULT: i64 = 20;

/// Most runs a list returns.
const EXECUTION_PAGE_MAX: i64 = 100;

// ---------------------------------------------------------------------------------------------
// Response bodies
// ---------------------------------------------------------------------------------------------

/// One workflow definition.
#[derive(Debug, Serialize)]
pub struct WorkflowBody {
    /// Workflow id.
    pub id: Uuid,
    /// Organization that owns it.
    pub organization_id: Uuid,
    /// Site it is scoped to, when it is.
    pub site_id: Option<Uuid>,
    /// Display name.
    pub name: String,
    /// Free-form description.
    pub description: String,
    /// Whether the schedule is armed (`false` = manual runs only).
    pub enabled: bool,
    /// Trigger kind.
    pub trigger: &'static str,
    /// Cron expression of a schedule.
    pub schedule: Option<String>,
    /// Event name of an event trigger.
    pub trigger_event: Option<String>,
    /// Conditions an event trigger's payload must satisfy.
    pub conditions: serde_json::Value,
    /// When the trigger last started a run.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_triggered_at: Option<OffsetDateTime>,
    /// How many runs the trigger has started.
    pub trigger_count: i32,
    /// Next due time of a schedule.
    #[serde(with = "time::serde::rfc3339::option")]
    pub next_run_at: Option<OffsetDateTime>,
    /// Steps, in order.
    pub steps: serde_json::Value,
    /// How many steps the definition carries.
    pub step_count: usize,
    /// Creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last definition change.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl WorkflowBody {
    /// Describe one stored definition.
    fn build(workflow: &Workflow) -> Self {
        let step_count = workflow.steps.as_array().map(Vec::len).unwrap_or_default();

        Self {
            id: workflow.id,
            organization_id: workflow.organization_id,
            site_id: workflow.site_id,
            name: workflow.name.clone(),
            description: workflow.description.clone(),
            enabled: workflow.enabled,
            trigger: workflow.trigger().as_str(),
            schedule: workflow.schedule.clone(),
            trigger_event: workflow.trigger_event.clone(),
            conditions: workflow.conditions.clone(),
            last_triggered_at: workflow.last_triggered_at,
            trigger_count: workflow.trigger_count,
            next_run_at: workflow.next_run_at,
            steps: workflow.steps.clone(),
            step_count,
            created_at: workflow.created_at,
            updated_at: workflow.updated_at,
        }
    }
}

/// The list payload.
#[derive(Debug, Serialize)]
pub struct WorkflowListResponse {
    /// Matching workflows, newest first.
    pub workflows: Vec<WorkflowBody>,
}

/// One run of a workflow, without its steps.
#[derive(Debug, Serialize)]
pub struct ExecutionSummary {
    /// Execution id.
    pub id: Uuid,
    /// Workflow that ran.
    pub workflow_id: Uuid,
    /// `running`, `completed`, `failed` or `cancelled`.
    pub status: String,
    /// `manual` or `schedule`.
    pub trigger: String,
    /// Account that started a manual run.
    pub triggered_by: Option<Uuid>,
    /// When the run started.
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    /// When it reached a terminal state.
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    /// Error of the failing step, when the run failed.
    pub error: Option<String>,
}

impl ExecutionSummary {
    /// Describe one stored run.
    fn build(execution: &WorkflowExecution) -> Self {
        Self {
            id: execution.id,
            workflow_id: execution.workflow_id,
            status: execution.status.clone(),
            trigger: execution.trigger_kind.clone(),
            triggered_by: execution.triggered_by,
            started_at: execution.started_at,
            finished_at: execution.finished_at,
            error: execution.error.clone(),
        }
    }
}

/// One step of a run.
#[derive(Debug, Serialize)]
pub struct StepBody {
    /// 1-based position.
    pub step_no: i32,
    /// Step name.
    pub name: String,
    /// `task` or `wait`.
    pub kind: String,
    /// Built-in action of a task step.
    pub action: Option<String>,
    /// `pending`, `running`, `waiting`, `succeeded`, `failed` or `cancelled`.
    pub status: String,
    /// Attempts made so far.
    pub attempts: i32,
    /// Attempts allowed in total.
    pub max_attempts: i32,
    /// When the step may run next (retry backoff, wait deadline).
    #[serde(with = "time::serde::rfc3339")]
    pub available_at: OffsetDateTime,
    /// When the current attempt started.
    #[serde(with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    /// When the step reached a terminal state.
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    /// Result of a successful step.
    pub output: Option<serde_json::Value>,
    /// Message of the last failure.
    pub error: Option<String>,
}

impl StepBody {
    /// Describe one stored step.
    fn build(step: &WorkflowStep) -> Self {
        Self {
            step_no: step.step_no,
            name: step.name.clone(),
            kind: step.kind.clone(),
            action: step.action.clone(),
            status: step.status.clone(),
            attempts: step.attempts,
            max_attempts: step.max_attempts,
            available_at: step.available_at,
            started_at: step.started_at,
            finished_at: step.finished_at,
            output: step.output.clone(),
            error: step.error.clone(),
        }
    }
}

/// One run with the state of each of its steps.
#[derive(Debug, Serialize)]
pub struct ExecutionDetail {
    /// The run itself.
    #[serde(flatten)]
    pub execution: ExecutionSummary,
    /// Steps, in order.
    pub steps: Vec<StepBody>,
}

impl ExecutionDetail {
    /// Describe one run plus its steps.
    fn build(execution: &WorkflowExecution, steps: &[WorkflowStep]) -> Self {
        Self {
            execution: ExecutionSummary::build(execution),
            steps: steps.iter().map(StepBody::build).collect(),
        }
    }
}

/// The list payload of runs.
#[derive(Debug, Serialize)]
pub struct ExecutionListResponse {
    /// Workflow the runs belong to.
    pub workflow_id: Uuid,
    /// Runs, newest first.
    pub executions: Vec<ExecutionSummary>,
}

// ---------------------------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------------------------

/// Filters of the workflow list.
#[derive(Debug, Deserialize)]
pub struct WorkflowListQuery {
    /// Organization to list (platform accounts only; organization accounts always see their own).
    pub organization_id: Option<Uuid>,
    /// Site to filter on.
    pub site_id: Option<Uuid>,
}

/// A whole definition: what `POST` creates and `PUT` replaces.
#[derive(Debug, Deserialize)]
pub struct WorkflowInput {
    /// Display name.
    pub name: String,
    /// Free-form description.
    #[serde(default)]
    pub description: String,
    /// Organization that will own the workflow (required for platform accounts).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// Site scope.
    #[serde(default)]
    pub site_id: Option<Uuid>,
    /// Whether a schedule is armed.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// How the workflow starts.
    pub trigger: Trigger,
    /// Steps, in order.
    pub steps: Vec<StepDefinition>,
}

/// Serde default for [`WorkflowInput::enabled`]: a new workflow is armed.
fn enabled_by_default() -> bool {
    true
}

/// How many runs a list returns.
#[derive(Debug, Deserialize)]
pub struct ExecutionListQuery {
    /// Page size (`1`–`100`).
    pub limit: Option<i64>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/workflows` — the definitions of one organization.
pub async fn list_workflows(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<WorkflowListQuery>,
) -> Result<Json<WorkflowListResponse>, ApiError> {
    let organization_id = match current.user.organization_id {
        Some(own) => {
            ensure_same_organization(&current, query.organization_id)?;
            Some(own)
        }
        None => query.organization_id,
    };

    let workflows =
        store::list_workflows(state.db().pool(), organization_id, query.site_id).await?;

    Ok(Json(WorkflowListResponse {
        workflows: workflows.iter().map(WorkflowBody::build).collect(),
    }))
}

/// `POST /api/v1/workflows` — create a definition.
pub async fn create_workflow(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(input): Json<WorkflowInput>,
) -> Result<(StatusCode, Json<WorkflowBody>), ApiError> {
    let organization_id = resolve_organization(&current, input.organization_id)?;
    if let Some(site_id) = input.site_id {
        site_in_scope(&state, &current, site_id).await?;
    }

    let name = input.name.trim().to_owned();
    if name.is_empty() {
        return Err(ApiError::bad_request(
            "invalid_request",
            "a workflow needs a name",
        ));
    }

    let definition = WorkflowDefinition::new(input.trigger, input.steps)?;
    let next_run_at = definition.trigger.validate(OffsetDateTime::now_utc())?;

    let workflow = store::insert_workflow(
        state.db().pool(),
        NewWorkflow {
            organization_id,
            site_id: input.site_id,
            name: name.clone(),
            description: input.description.trim().to_owned(),
            enabled: input.enabled,
            trigger: definition.trigger.kind,
            schedule: definition.trigger.cron.clone(),
            trigger_event: definition.trigger.event.clone(),
            conditions: definition.conditions_json()?,
            next_run_at,
            steps: definition.steps_json()?,
            created_by: Some(current.user.id),
        },
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "workflow.created")
            .organization(organization_id)
            .target("workflow", workflow.id.to_string())
            .metadata(json!({
                "name": name,
                "trigger": workflow.trigger_kind,
                "schedule": workflow.schedule,
                "steps": workflow.steps.as_array().map(Vec::len).unwrap_or_default(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(WorkflowBody::build(&workflow))))
}

/// `GET /api/v1/workflows/{id}` — one definition.
pub async fn get_workflow(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(workflow_id): Path<Uuid>,
) -> Result<Json<WorkflowBody>, ApiError> {
    let workflow = workflow_in_scope(&state, &current, workflow_id).await?;
    Ok(Json(WorkflowBody::build(&workflow)))
}

/// `PUT /api/v1/workflows/{id}` — replace a definition.
///
/// A workflow is replaced, not patched: a step list is edited as a whole, and replacing it
/// keeps the stored definition exactly what the panel shows. Runs that already started keep
/// the steps they materialised, so editing never rewrites history.
pub async fn update_workflow(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(workflow_id): Path<Uuid>,
    Json(input): Json<WorkflowInput>,
) -> Result<Json<WorkflowBody>, ApiError> {
    let existing = workflow_in_scope(&state, &current, workflow_id).await?;

    if let Some(site_id) = input.site_id {
        site_in_scope(&state, &current, site_id).await?;
    }

    let name = input.name.trim().to_owned();
    if name.is_empty() {
        return Err(ApiError::bad_request(
            "invalid_request",
            "a workflow needs a name",
        ));
    }

    let definition = WorkflowDefinition::new(input.trigger, input.steps)?;
    let next_run_at = definition.trigger.validate(OffsetDateTime::now_utc())?;

    let updated = store::update_workflow(
        state.db().pool(),
        existing.id,
        WorkflowUpdate {
            name: name.clone(),
            description: input.description.trim().to_owned(),
            site_id: input.site_id,
            enabled: input.enabled,
            trigger: definition.trigger.kind,
            schedule: definition.trigger.cron.clone(),
            trigger_event: definition.trigger.event.clone(),
            conditions: definition.conditions_json()?,
            next_run_at,
            steps: definition.steps_json()?,
        },
    )
    .await?
    .ok_or_else(workflow_not_found)?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "workflow.updated")
            .organization(updated.organization_id)
            .target("workflow", updated.id.to_string())
            .metadata(json!({
                "name": name,
                "trigger": updated.trigger_kind,
                "schedule": updated.schedule,
                "enabled": updated.enabled,
                "steps": updated.steps.as_array().map(Vec::len).unwrap_or_default(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(Json(WorkflowBody::build(&updated)))
}

/// `DELETE /api/v1/workflows/{id}` — remove a definition and its run history.
pub async fn delete_workflow(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(workflow_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let workflow = workflow_in_scope(&state, &current, workflow_id).await?;

    if !store::delete_workflow(state.db().pool(), workflow.id).await? {
        return Err(workflow_not_found());
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "workflow.deleted")
            .organization(workflow.organization_id)
            .target("workflow", workflow.id.to_string())
            .metadata(json!({ "name": workflow.name }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/workflows/{id}/run` — start a run now.
///
/// The run is durable from this instant: the execution and its steps are rows before the
/// response leaves, and the background runner advances them.
pub async fn run_workflow(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(workflow_id): Path<Uuid>,
) -> Result<(StatusCode, Json<ExecutionDetail>), ApiError> {
    let workflow = workflow_in_scope(&state, &current, workflow_id).await?;

    let execution = engine::start_run(
        state.db().pool(),
        &workflow,
        TriggerKind::Manual,
        Some(current.user.id),
    )
    .await?;

    let steps = store::list_steps(state.db().pool(), execution.id).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(ExecutionDetail::build(&execution, &steps)),
    ))
}

/// `GET /api/v1/workflows/{id}/executions` — the run history of one workflow.
pub async fn list_executions(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(workflow_id): Path<Uuid>,
    Query(query): Query<ExecutionListQuery>,
) -> Result<Json<ExecutionListResponse>, ApiError> {
    let workflow = workflow_in_scope(&state, &current, workflow_id).await?;
    let limit = query
        .limit
        .unwrap_or(EXECUTION_PAGE_DEFAULT)
        .clamp(1, EXECUTION_PAGE_MAX);

    let executions = store::list_executions(state.db().pool(), workflow.id, limit).await?;

    Ok(Json(ExecutionListResponse {
        workflow_id: workflow.id,
        executions: executions.iter().map(ExecutionSummary::build).collect(),
    }))
}

/// `GET /api/v1/workflow-executions/{id}` — one run with the state of every step.
pub async fn get_execution(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(execution_id): Path<Uuid>,
) -> Result<Json<ExecutionDetail>, ApiError> {
    let execution = execution_in_scope(&state, &current, execution_id).await?;
    let steps = store::list_steps(state.db().pool(), execution.id).await?;
    Ok(Json(ExecutionDetail::build(&execution, &steps)))
}

/// `POST /api/v1/workflow-executions/{id}/cancel` — stop a running execution.
pub async fn cancel_execution(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(execution_id): Path<Uuid>,
) -> Result<Json<ExecutionDetail>, ApiError> {
    let execution = execution_in_scope(&state, &current, execution_id).await?;

    if execution.status() != Some(ExecutionStatus::Running) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "execution_not_running",
            "this run is not running any more",
        ));
    }

    let cancelled = store::cancel_execution(state.db().pool(), execution.id)
        .await?
        .ok_or_else(execution_not_found)?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "workflow.execution.cancelled")
            .organization(cancelled.organization_id)
            .target("workflow_execution", cancelled.id.to_string())
            .metadata(json!({
                "workflow_id": cancelled.workflow_id,
                "trigger": cancelled.trigger_kind,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    let steps = store::list_steps(state.db().pool(), cancelled.id).await?;
    Ok(Json(ExecutionDetail::build(&cancelled, &steps)))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Write an audit row; a privileged action is not reported as successful without one.
async fn record(state: &AppState, entry: NewAuditEntry) -> Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}

/// Load a site or answer `404 site_not_found`.
async fn site_of(state: &AppState, site_id: Uuid) -> Result<Site, ApiError> {
    sites::find_site(state.db().pool(), site_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "site_not_found", "no such site"))
}

/// Load a site and refuse it when it lives outside the caller's organization.
async fn site_in_scope(
    state: &AppState,
    current: &CurrentSession,
    site_id: Uuid,
) -> Result<Site, ApiError> {
    let site = site_of(state, site_id).await?;
    ensure_same_organization(current, Some(site.organization_id))?;
    Ok(site)
}

/// Load a workflow and refuse it when its organization is out of the caller's scope.
async fn workflow_in_scope(
    state: &AppState,
    current: &CurrentSession,
    workflow_id: Uuid,
) -> Result<Workflow, ApiError> {
    let workflow = store::find_workflow(state.db().pool(), workflow_id)
        .await?
        .ok_or_else(workflow_not_found)?;
    ensure_same_organization(current, Some(workflow.organization_id))?;
    Ok(workflow)
}

/// Load a run and refuse it when its organization is out of the caller's scope.
async fn execution_in_scope(
    state: &AppState,
    current: &CurrentSession,
    execution_id: Uuid,
) -> Result<WorkflowExecution, ApiError> {
    let execution = store::find_execution(state.db().pool(), execution_id)
        .await?
        .ok_or_else(execution_not_found)?;
    ensure_same_organization(current, Some(execution.organization_id))?;
    Ok(execution)
}

fn workflow_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "workflow_not_found",
        "no such workflow",
    )
}

fn execution_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "workflow_execution_not_found",
        "no such workflow run",
    )
}

/// Body validity is checked by the engine crate; this only re-labels an unreadable store row.
#[allow(dead_code)]
fn store_error(error: WorkflowError) -> ApiError {
    ApiError::bad_request(error.code(), error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workflow_row() -> Workflow {
        Workflow {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            site_id: None,
            name: "Nightly digest".to_owned(),
            description: "Publishes the nightly digest".to_owned(),
            enabled: true,
            trigger_kind: "schedule".to_owned(),
            schedule: Some("0 3 * * *".to_owned()),
            trigger_event: None,
            conditions: serde_json::json!([]),
            next_run_at: Some(OffsetDateTime::UNIX_EPOCH),
            steps: serde_json::json!([
                { "name": "prepare", "kind": "task", "action": "noop", "params": {}, "max_attempts": 1 }
            ]),
            last_triggered_at: None,
            trigger_count: 0,
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn the_body_describes_the_definition() {
        let body = WorkflowBody::build(&workflow_row());
        assert_eq!(body.trigger, "schedule");
        assert_eq!(body.step_count, 1);
        assert_eq!(body.schedule.as_deref(), Some("0 3 * * *"));

        let rendered = serde_json::to_value(&body).expect("the body serialises");
        assert_eq!(rendered["trigger"], "schedule");
        assert!(rendered["next_run_at"].as_str().is_some());
        assert!(rendered["created_at"].as_str().is_some());
        assert_eq!(rendered["steps"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn input_defaults_arm_the_schedule() {
        let input: WorkflowInput = serde_json::from_value(serde_json::json!({
            "name": "Nightly digest",
            "trigger": { "kind": "manual" },
            "steps": [{ "name": "prepare", "kind": "task", "action": "noop" }]
        }))
        .expect("a minimal definition parses");

        assert!(input.enabled, "a new workflow is armed");
        assert!(input.description.is_empty());
        assert_eq!(input.steps.len(), 1);
        assert_eq!(input.steps[0].max_attempts, 1);
        assert!(input.steps[0].params.is_object());
    }

    #[test]
    fn an_unknown_trigger_is_refused_by_deserialisation() {
        let parsed = serde_json::from_value::<WorkflowInput>(serde_json::json!({
            "name": "Nightly digest",
            "trigger": { "kind": "webhook" },
            "steps": [{ "name": "prepare", "kind": "task", "action": "noop" }]
        }));
        assert!(parsed.is_err(), "only manual and schedule exist in v0");
    }

    #[test]
    fn the_run_page_is_bounded() {
        let query: ExecutionListQuery =
            serde_json::from_value(serde_json::json!({ "limit": 500 })).expect("a limit parses");
        assert_eq!(query.limit, Some(500));
        assert_eq!(
            query
                .limit
                .unwrap_or(EXECUTION_PAGE_DEFAULT)
                .clamp(1, EXECUTION_PAGE_MAX),
            EXECUTION_PAGE_MAX
        );
    }

    #[test]
    fn the_detail_body_flattens_the_run_and_lists_its_steps() {
        let execution = WorkflowExecution {
            id: Uuid::nil(),
            workflow_id: Uuid::nil(),
            organization_id: Uuid::nil(),
            status: "completed".to_owned(),
            trigger_kind: "manual".to_owned(),
            triggered_by: None,
            started_at: OffsetDateTime::UNIX_EPOCH,
            finished_at: Some(OffsetDateTime::UNIX_EPOCH),
            error: None,
        };
        let steps = vec![WorkflowStep {
            id: Uuid::nil(),
            execution_id: Uuid::nil(),
            step_no: 1,
            name: "prepare".to_owned(),
            kind: "task".to_owned(),
            action: Some("noop".to_owned()),
            params: serde_json::json!({}),
            status: "succeeded".to_owned(),
            attempts: 1,
            max_attempts: 1,
            available_at: OffsetDateTime::UNIX_EPOCH,
            started_at: Some(OffsetDateTime::UNIX_EPOCH),
            finished_at: Some(OffsetDateTime::UNIX_EPOCH),
            output: Some(serde_json::json!({ "action": "noop" })),
            error: None,
        }];

        let detail = ExecutionDetail::build(&execution, &steps);
        let rendered = serde_json::to_value(&detail).expect("the detail serialises");
        assert_eq!(rendered["status"], "completed");
        assert_eq!(rendered["steps"][0]["name"], "prepare");
        assert_eq!(rendered["steps"][0]["attempts"], 1);
    }
}
