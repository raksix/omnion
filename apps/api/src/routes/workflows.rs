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
    /// The graph node the run was started at, when it was started with *Run from here*.
    ///
    /// `None` for a whole run. It is on the summary rather than only the detail because
    /// the run *list* is where an operator asks "which of these did I start halfway
    /// down?" — a list of runs that cannot tell a full run from a partial one shows the
    /// same two rows and the reader has to open each one to find out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_from_node: Option<String>,
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
            started_from_node: execution.started_from_node.clone(),
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
    /// `task`, `wait`, `branch` or `stop`.
    pub kind: String,
    /// Built-in action of a task step.
    pub action: Option<String>,
    /// The step's inputs, as stored JSON (REQ-004 slice 3, criterion 2).
    ///
    /// This is the half of "clicking the node opens that step's inputs and output" that
    /// lives on the server, and it is the half that cannot be derived in the client: the
    /// node on the canvas carries the *authored* parameters, which is what the inspector
    /// edits, while this is what the engine was actually handed — a run from a node, a
    /// retry, or an edit that was never saved all leave the two different, and the one an
    /// operator debugging a run needs is this one.
    ///
    /// Sent whenever the step has them, so "no inputs" reads as a missing key rather than
    /// as a `null` a client has to distinguish from an empty object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    /// `pending`, `running`, `waiting`, `succeeded`, `failed` or `cancelled`.
    pub status: String,
    /// The graph node this step came from, when the rule was started from a graph.
    ///
    /// This is what paints the status pill on the builder canvas, and it is emitted
    /// whenever the column is set rather than only for graph rules: a step whose node is
    /// unknown must read as "no node" on the client, and a `null` that had to be
    /// distinguished from an absent key is a distinction a client will eventually get
    /// wrong. Rules defined before the builder carry `None` here and are the reason this
    /// field is optional rather than defaulted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// Why a `skipped` step did not run, in the run's own words (REQ-004 slice 3).
    ///
    /// The trace's whole claim is that it says *why*, and a reason is only a reason if it
    /// reaches the reader unedited — the server writes it, so the server sends it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    /// Attempts made so far.
    pub attempts: i32,
    /// Attempts allowed in total.
    pub max_attempts: i32,
    /// What this step's own failure does; `inherit` takes the rule's policy.
    pub on_error: String,
    /// How long this step may block before it is failed with the limit named.
    pub timeout_ms: i32,
    /// `true` when the run deliberately outlived this step's failure — the row stays
    /// `failed` and the run is not a failure because of it.
    pub ignored: bool,
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
            params: Some(step.params.clone()),
            status: step.status.clone(),
            node_id: step.node_id.clone(),
            skip_reason: step.skip_reason.clone(),
            attempts: step.attempts,
            max_attempts: step.max_attempts,
            on_error: step.on_error.clone(),
            timeout_ms: step.timeout_ms,
            ignored: step.ignored,
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
    /// The event payload this run started from — what its branch steps read and what the
    /// panel shows in the trace's sidebar. `None` for a manual run and a schedule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_payload: Option<serde_json::Value>,
    /// Which controls this run offers: a settled run can be retried from a step, a running
    /// one can be cancelled, and a cancelled one is not retried (that was a decision).
    pub can_retry: bool,
    pub can_cancel: bool,
}

impl ExecutionDetail {
    /// Describe one run plus its steps.
    fn build(execution: &WorkflowExecution, steps: &[WorkflowStep]) -> Self {
        let status = execution.status();
        let terminal = status.is_some_and(omnion_workflows::ExecutionStatus::is_terminal);

        Self {
            execution: ExecutionSummary::build(execution),
            steps: steps.iter().map(StepBody::build).collect(),
            event_payload: execution.event_payload.clone(),
            // A cancelled run is a person's decision, so "retry" must not offer to undo it
            // silently — the panel says why instead.
            can_retry: status == Some(omnion_workflows::ExecutionStatus::Failed),
            can_cancel: terminal.then_some(false).unwrap_or(true),
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
            on_error: omnion_workflows::OnError::Stop,
            organization_id,
            site_id: input.site_id,
            name: name.clone(),
            description: input.description.trim().to_owned(),
            enabled: input.enabled,
            trigger: definition.trigger.kind,
            schedule: definition.trigger.cron.clone(),
            trigger_event: definition.trigger.event.clone(),
            conditions: definition.conditions_json()?,
            // The two rule bounds are not this surface's vocabulary: a workflow a person
            // starts by hand is not a rule that fires on its own, so nothing is sent and
            // the column defaults stand.
            rate_limit_per_hour: None,
            concurrency: None,
            // The manual/scheduled surface has no run-as picker: those workflows are run by
            // the people who manage them, so they follow their author. The field is
            // automation's (`crates/automation::authority`), and a second way to set it here
            // would be a second answer to "who does this run as".
            run_as_user_id: None,
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
            on_error: omnion_workflows::OnError::Stop,
            name: name.clone(),
            description: input.description.trim().to_owned(),
            site_id: input.site_id,
            enabled: input.enabled,
            trigger: definition.trigger.kind,
            schedule: definition.trigger.cron.clone(),
            trigger_event: definition.trigger.event.clone(),
            conditions: definition.conditions_json()?,
            // As on create: `None` leaves whatever a rule handed to this workflow already
            // has alone, rather than resetting a bound nobody on this surface can see.
            rate_limit_per_hour: None,
            concurrency: None,
            // Carried through rather than cleared: a workflow that was an event rule and is
            // being converted back must not silently lose the account it was handed.
            run_as_user_id: existing.run_as_user_id,
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

    // The steps are attributed to the graph's nodes here, not only on the *Run from here*
    // path. A run started the ordinary way is the **common** case, and a run whose steps
    // carry no node id has no status layer at all: the canvas paints nothing, a click opens
    // nothing, and *Retry this node* answers "took no part in this run" on every card. The
    // walk that proved it was a walk for *retry*, and the defect it found was not in retry.
    //
    // A rule whose graph does not project is left unattributed rather than half-attributed:
    // a partially painted canvas reads as "those nodes were skipped", which is a claim
    // about work the engine did. Such a rule's validation problem is already on its
    // overview, so the run still starts — it just cannot be read by node.
    if let Some(definition) = omnion_workflows::graph_store::find_graph(state.db().pool(), workflow.id)
        .await?
    {
        // The attribution is best-effort *for the decoration only*. It is not allowed to
        // fail the run it describes: the work has already started, and reporting an error
        // here would tell an operator their run did not happen when it did.
        if let Err(err) = omnion_workflows::graph_store::attribute_steps_to_graph(
            state.db().pool(),
            execution.id,
            &definition.graph,
        )
        .await
        {
            tracing::warn!(
                workflow_id = %workflow.id,
                execution_id = %execution.id,
                error = %err,
                "a run started without per-node attribution; the canvas will show no status \
                 for this run"
            );
        }
    }

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

/// `POST /api/v1/workflows/{id}/run-from-node` — *Run from here* on a canvas node.
///
/// The criterion it exists for (REQ-004 slice 3): "Run from here" on a mid-graph node
/// starts a run whose **first step is that node**, the earlier nodes stay `skipped`, and
/// **the trace says why**.
///
/// Three decisions live in the crate rather than here, and each of them is one this
/// handler could have got wrong in a way the response would not have shown:
///
/// * **the plan** ([`omnion_workflows::run_from::plan_from_node`]) is made from the rule's
///   own graph, so the node id is resolved against the same revision the steps come from;
/// * **the prefix is inserted as `skipped`**, never as pending-then-updated, so a crash
///   cannot leave a run the engine is about to execute from the top;
/// * **an empty plan is refused** rather than answered with a run that settles
///   `completed` having done nothing.
///
/// The run is `manual` even when the rule is armed for a schedule or an event: pressing
/// *Run from here* is a person asking for this run, and the audit row has to say so.
pub async fn run_workflow_from_node(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(workflow_id): Path<Uuid>,
    Json(input): Json<RunFromNodeInput>,
) -> Result<(StatusCode, Json<RunFromNodeResponse>), ApiError> {
    let workflow = workflow_in_scope(&state, &current, workflow_id).await?;

    let node_id = input.node_id.trim();
    if node_id.is_empty() {
        return Err(ApiError::bad_request(
            "node_id_required",
            "say which node the run starts at — the builder sends the node that was clicked",
        ));
    }

    let definition = omnion_workflows::graph_store::find_graph(state.db().pool(), workflow.id)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "workflow_not_found",
                "the workflow no longer exists — it may have been deleted while the builder \
                 was open",
            )
        })?;

    let plan = omnion_workflows::run_from::plan_from_node(&definition.graph, node_id)
        .map_err(ApiError::from)?;

    let steps = omnion_workflows::run_from::runnable_steps(&plan);
    let (execution, rows) = store::create_execution_from_node(
        state.db().pool(),
        &workflow,
        TriggerKind::Manual,
        Some(current.user.id),
        &steps,
        &plan,
    )
    .await?;

    // The graph a run started with is pinned on the run, so a trace resolves its node ids
    // even after the definition moves on — and here it matters twice over, because the
    // very act of "starting from a node" is a fact about a *version* of the graph.
    if let Some(version) = Some(definition.graph_version) {
        omnion_workflows::graph_store::pin_execution_graph(
            state.db().pool(),
            execution.id,
            Some(version),
        )
        .await?;
    }

    // Stamp each row with the node it came from, so the canvas can paint this run's status
    // per node — including the skipped prefix, which is the half a trace reads first.
    for row in &rows {
        let node = if plan
            .skipped
            .iter()
            .any(|skipped| skipped.step_no == row.step_no)
        {
            plan.skipped
                .iter()
                .find(|skipped| skipped.step_no == row.step_no)
                .map(|skipped| skipped.node_id.clone())
        } else {
            plan.runs
                .iter()
                .find(|entry| entry.step_no == row.step_no)
                .map(|entry| entry.node_id.clone())
        };
        omnion_workflows::graph_store::set_step_node(
            state.db().pool(),
            row.id,
            node.as_deref(),
            None,
        )
        .await?;
    }

    let stored = store::list_steps(state.db().pool(), execution.id).await?;

    let entry = NewAuditEntry::by_user(current.user.id, "workflow.run.from_node")
        .organization(workflow.organization_id)
        .target("workflow", workflow.id.to_string())
        .metadata(json!({
            "execution_id": execution.id,
            "node_id": plan.node_id,
            "skipped_steps": plan.skipped.len(),
            "run_steps": plan.runs.len(),
            "reason": plan.reason,
        }))
        .ip_address(address.as_text());
    omnion_audit::record(state.db().pool(), entry).await?;

    Ok((
        StatusCode::ACCEPTED,
        Json(RunFromNodeResponse {
            execution: ExecutionDetail::build(&execution, &stored),
            started_from_node: plan.node_id,
            skipped: plan.skipped.iter().map(RunFromSkipped::build).collect(),
            reason: plan.reason,
        }),
    ))
}

/// The body of a *Run from here* request.
#[derive(Debug, Deserialize)]
pub struct RunFromNodeInput {
    /// The node the run starts at, as the canvas draws it.
    pub node_id: String,
}

/// `POST /api/v1/workflow-executions/{id}/retry-node` — *Retry this node* on a canvas node.
///
/// The criterion (REQ-004 slice 3): **"Retry this node" re-runs only that node without
/// duplicating earlier side effects** — proven with the mail sink, because the earlier
/// side effect of a workflow is very often an e-mail and no assertion on a status column
/// can see it.
///
/// Three things are decided in the crate rather than here, and each is one this handler
/// could get wrong invisibly:
///
/// * **which step** ([`omnion_workflows::retry_node::plan_retry_node`]) — a branching node
///   is two rows, and retrying whichever the query returned first would report "nothing to
///   retry" on a node the canvas is painting red;
/// * **one row** ([`store::retry_single_step`]) — this is not a narrowed
///   `retry_step_from`. The tail re-run re-opens everything after the step, and reusing it
///   here would re-send the earlier e-mail, which is the one outcome the criterion names;
/// * **the attempt counter is left alone.** The operator re-ran one node; they did not
///   grant it a new budget, and nothing on the screen would show an unbounded one.
///
/// The response carries the run's refreshed steps so the canvas repaints from the engine's
/// own rows rather than from an optimistic guess — the same reason the run-from response
/// re-reads rather than echoing the plan.
pub async fn retry_workflow_node(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(execution_id): Path<Uuid>,
    Json(input): Json<RetryNodeInput>,
) -> Result<Json<RetryNodeResponse>, ApiError> {
    let execution = omnion_workflows::store::find_execution(state.db().pool(), execution_id)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "execution_not_found",
                "no such run — it may have been cleared while the builder was open",
            )
        })?;
    ensure_same_organization(&current, Some(execution.organization_id))?;

    let node_id = input.node_id.trim();
    if node_id.is_empty() {
        return Err(ApiError::bad_request(
            "node_id_required",
            "say which node to retry — the builder sends the node that was clicked",
        ));
    }

    // The plan reads the *stored* rows, so the answer is about the run that exists rather
    // than about the graph this screen happens to be holding.
    let rows = omnion_workflows::store::list_steps(state.db().pool(), execution_id).await?;
    let readable: Vec<omnion_workflows::retry_node::RetryableStep> = rows
        .iter()
        .map(|row| omnion_workflows::retry_node::RetryableStep {
            step_no: row.step_no,
            status: row.status.clone(),
            node_id: row.node_id.clone(),
            name: row.name.clone(),
        })
        .collect();

    let plan = omnion_workflows::retry_node::plan_retry_node(
        omnion_workflows::retry_node::RunState {
            status: execution.status.to_string(),
        },
        node_id,
        &readable,
    )
    .map_err(|refusal| {
        // A refusal is a 409 rather than a 400 for the two that are about the run's
        // current state: "it is still going" and "it was cancelled" are facts that will
        // change on their own, and a client caching a 400 as a permanent rejection of the
        // node is caching the wrong thing.
        let status = match refusal {
            omnion_workflows::retry_node::RetryRefusal::RunStillRunning
            | omnion_workflows::retry_node::RetryRefusal::RunCancelled => {
                StatusCode::CONFLICT
            }
            _ => StatusCode::BAD_REQUEST,
        };
        ApiError::new(status, refusal.code(), refusal.message(node_id))
    })?;

    let requeued = omnion_workflows::store::retry_single_step(
        state.db().pool(),
        execution_id,
        plan.step_no,
    )
    .await?;

    if requeued != 1 {
        // The plan and the write disagreeing means the run changed between the read and
        // the write — a second operator's retry, or a run that settled on its own. Saying
        // so is the honest answer; re-queueing until the count matches would be a write
        // loop fighting a concurrent change.
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "retry_raced",
            format!(
                "step {} was already re-queued by someone else — reload the run to see where \
                 it is now",
                plan.step_no
            ),
        ));
    }

    omnion_audit::record(
        state.db().pool(),
        omnion_audit::NewAuditEntry::by_user(current.user.id, "workflow.retry.node")
            .organization(execution.organization_id)
            .target("workflow_execution", execution_id.to_string())
            .metadata(json!({
                "node_id": plan.node_id,
                "step_no": plan.step_no,
                "requeued": requeued,
                "reason": plan.reason,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    let refreshed = omnion_workflows::store::list_steps(state.db().pool(), execution_id).await?;
    let after = omnion_workflows::store::find_execution(state.db().pool(), execution_id)
        .await?
        .unwrap_or(execution);

    Ok(Json(RetryNodeResponse {
        execution: ExecutionDetail::build(&after, &refreshed),
        node_id: plan.node_id.unwrap_or_else(|| node_id.to_owned()),
        step_no: plan.step_no,
        requeued,
        reason: plan.reason,
    }))
}

/// The body of a *Retry this node* request.
#[derive(Debug, Deserialize)]
pub struct RetryNodeInput {
    /// The node to retry, as the canvas draws it.
    pub node_id: String,
}

/// What a *Retry this node* press produced.
#[derive(Debug, Serialize)]
pub struct RetryNodeResponse {
    /// The run afterwards, with every step as the engine has it now.
    #[serde(flatten)]
    pub execution: ExecutionDetail,
    /// The node that was retried.
    pub node_id: String,
    /// The one step that went back on the queue.
    pub step_no: i32,
    /// How many rows the write re-opened. Always 1, and asserted on the walk — a value of 2
    /// is a tail retry wearing a node retry's name, and it re-sends earlier e-mails.
    pub requeued: u64,
    /// The sentence the panel shows.
    pub reason: String,
}


/// What a *Run from here* press produced.
#[derive(Debug, Serialize)]
pub struct RunFromNodeResponse {
    /// The run, with every step including the skipped prefix.
    #[serde(flatten)]
    pub execution: ExecutionDetail,
    /// The node the run started at.
    pub started_from_node: String,
    /// The steps it passed over, each with the sentence the trace shows.
    pub skipped: Vec<RunFromSkipped>,
    /// The same sentence, once, for the run header.
    pub reason: String,
}

/// One step a run-from-here passed over.
#[derive(Debug, Serialize)]
pub struct RunFromSkipped {
    /// Its 1-based position in the run, which it keeps.
    pub step_no: i32,
    /// Its name.
    pub name: String,
    /// The node it came from.
    pub node_id: String,
    /// Why it did not run.
    pub reason: String,
}

impl RunFromSkipped {
    fn build(skipped: &omnion_workflows::run_from::SkippedStep) -> Self {
        Self {
            step_no: skipped.step_no,
            name: skipped.name.clone(),
            node_id: skipped.node_id.clone(),
            reason: skipped.reason.clone(),
        }
    }
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
pub async fn workflow_in_scope(
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
            run_as_user_id: None,
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
            hook_token_hash: None,
            on_error: "stop".to_owned(),
            hook_secret: None,
            rate_limit_per_hour: 60,
            concurrency: "queue".to_owned(),
            last_error: None,
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
            approval_id: None,
            id: Uuid::nil(),
            workflow_id: Uuid::nil(),
            organization_id: Uuid::nil(),
            status: "completed".to_owned(),
            trigger_kind: "manual".to_owned(),
            triggered_by: None,
            started_at: OffsetDateTime::UNIX_EPOCH,
            finished_at: Some(OffsetDateTime::UNIX_EPOCH),
            error: None,
            event_payload: Some(serde_json::json!({ "status": "published" })),
            started_from_node: None,
        };
        let steps = vec![WorkflowStep {
            skip_reason: None,
            node_id: None,
            branch: None,
            approval_id: None,
            id: Uuid::nil(),
            execution_id: Uuid::nil(),
            step_no: 1,
            name: "prepare".to_owned(),
            kind: "task".to_owned(),
            action: Some("noop".to_owned()),
            params: serde_json::json!({}),
            on_error: "inherit".to_owned(),
            timeout_ms: 30_000,
            status: "succeeded".to_owned(),
            attempts: 1,
            max_attempts: 1,
            available_at: OffsetDateTime::UNIX_EPOCH,
            started_at: Some(OffsetDateTime::UNIX_EPOCH),
            finished_at: Some(OffsetDateTime::UNIX_EPOCH),
            output: Some(serde_json::json!({ "action": "noop" })),
            error: None,
            ignored: false,
        }];

        let detail = ExecutionDetail::build(&execution, &steps);
        let rendered = serde_json::to_value(&detail).expect("the detail serialises");
        assert_eq!(rendered["status"], "completed");
        assert_eq!(rendered["steps"][0]["name"], "prepare");
        assert_eq!(rendered["steps"][0]["attempts"], 1);
    }

    /// One stored step, for the tests below.
    fn step_row(step_no: i32, status: &str, params: serde_json::Value) -> WorkflowStep {
        WorkflowStep {
            skip_reason: None,
            node_id: Some(format!("node-{step_no}")),
            branch: None,
            approval_id: None,
            id: Uuid::nil(),
            execution_id: Uuid::nil(),
            step_no,
            name: format!("step-{step_no}"),
            kind: "task".to_owned(),
            action: Some("noop".to_owned()),
            params,
            on_error: "inherit".to_owned(),
            timeout_ms: 30_000,
            status: status.to_owned(),
            attempts: 1,
            max_attempts: 1,
            available_at: OffsetDateTime::UNIX_EPOCH,
            started_at: Some(OffsetDateTime::UNIX_EPOCH),
            finished_at: Some(OffsetDateTime::UNIX_EPOCH),
            output: Some(serde_json::json!({ "value": step_no })),
            error: None,
            ignored: false,
        }
    }

    #[test]
    fn a_step_sends_its_inputs_and_its_output() {
        // REQ-004 slice 3, criterion 2: "clicking the node opens that step's inputs and
        // output". The client half renders a panel; this test is the half that has to be
        // true for the panel to have anything to render.
        //
        // The assertion is on the SERIALISED body, not on the struct. A field that is set on
        // `StepBody` and dropped by a `skip_serializing_if` that always fires is a field the
        // client never sees, and a struct-level assertion would call that a pass — the same
        // shape of gap as the `node_id` one this criterion already paid for once.
        let body = StepBody::build(&step_row(
            2,
            "succeeded",
            serde_json::json!({ "url": "https://example.test/hook", "retries": 3 }),
        ));
        let rendered = serde_json::to_value(&body).expect("a step serialises");

        assert_eq!(rendered["params"]["url"], "https://example.test/hook");
        assert_eq!(rendered["params"]["retries"], 3);
        assert_eq!(rendered["output"]["value"], 2);
    }

    #[test]
    fn an_empty_input_object_is_still_sent() {
        // The distinction the client's `describePayload` turns on: a step whose inputs are
        // `{}` RAN and took nothing, while a step whose inputs are missing was never told
        // anything. Omitting an empty object would collapse those two into one, and the
        // panel would claim the second about the first.
        let body = StepBody::build(&step_row(1, "succeeded", serde_json::json!({})));
        let rendered = serde_json::to_value(&body).expect("a step serialises");
        assert!(
            rendered.get("params").is_some(),
            "an empty input object is still an input object, not an absent key"
        );
        assert!(rendered["params"].is_object());
    }

    #[test]
    fn a_step_that_produced_nothing_sends_a_null_output_rather_than_a_fake_one() {
        // The other half: a `pending` step has no output and says so. Inventing an empty
        // object here would let the panel report "the step ran and returned nothing" for a
        // step that has not run at all.
        let mut step = step_row(1, "pending", serde_json::json!({}));
        step.output = None;
        step.finished_at = None;
        let rendered = serde_json::to_value(StepBody::build(&step)).expect("a step serialises");
        assert!(rendered["output"].is_null());
        // `error` is sent unconditionally, so a step with no failure reads as a null rather
        // than as an absent key. The client must not have to tell those apart, and the
        // trace panel renders `error` only when it is a string.
        assert!(rendered["error"].is_null());
    }
}
