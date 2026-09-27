//! The durable step store: every read and write the engine and the API perform.
//!
//! The store is deliberately explicit SQL rather than an ORM: the engine's correctness lives in
//! a handful of statements — claim a due step, park a wait, settle a run — and each of them is
//! written so that two instances racing over the same rows still hand every step to exactly one
//! runner (`for update skip locked`).

use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::definition::StepDefinition;
use crate::error::{Result, WorkflowError};
use crate::model::{
    EXECUTION_COLUMNS, ExecutionStatus, NewWorkflow, STEP_COLUMNS, StepKind, StepStatus,
    TriggerKind, WORKFLOW_COLUMNS, Workflow, WorkflowExecution, WorkflowStep,
};

/// Columns of `workflows` for one `select`, in [`Workflow`] order.
fn workflow_columns() -> &'static str {
    WORKFLOW_COLUMNS
}

/// Insert a workflow definition.
pub async fn insert_workflow(pool: &PgPool, new: NewWorkflow) -> Result<Workflow> {
    let sql = format!(
        "insert into workflows (organization_id, site_id, name, description, enabled, \
         trigger_kind, schedule, trigger_event, conditions, on_error, next_run_at, steps, \
         created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) returning {}",
        workflow_columns()
    );

    let workflow: Workflow = sqlx::query_as(&sql)
        .bind(new.organization_id)
        .bind(new.site_id)
        .bind(new.name)
        .bind(new.description)
        .bind(new.enabled)
        .bind(new.trigger.as_str())
        .bind(new.schedule)
        .bind(new.trigger_event)
        .bind(new.conditions)
        .bind(new.on_error.as_str())
        .bind(new.next_run_at)
        .bind(new.steps)
        .bind(new.created_by)
        .fetch_one(pool)
        .await?;

    Ok(workflow)
}

/// List workflows, newest first; `organization_id` and `site_id` filter when given.
pub async fn list_workflows(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    site_id: Option<Uuid>,
) -> Result<Vec<Workflow>> {
    let sql = format!(
        "select {} from workflows \
         where ($1::uuid is null or organization_id = $1) \
           and ($2::uuid is null or site_id = $2) \
         order by created_at desc, id",
        workflow_columns()
    );

    let workflows: Vec<Workflow> = sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(site_id)
        .fetch_all(pool)
        .await?;

    Ok(workflows)
}

/// Load one workflow.
pub async fn find_workflow(pool: &PgPool, id: Uuid) -> Result<Option<Workflow>> {
    let sql = format!("select {} from workflows where id = $1", workflow_columns());
    let workflow: Option<Workflow> = sqlx::query_as(&sql).bind(id).fetch_optional(pool).await?;
    Ok(workflow)
}

/// The definition columns a change may rewrite.
#[derive(Debug, Clone)]
pub struct WorkflowUpdate {
    /// New display name.
    pub name: String,
    /// New description.
    pub description: String,
    /// New site scope.
    pub site_id: Option<Uuid>,
    /// Whether the schedule/event trigger stays armed.
    pub enabled: bool,
    /// New trigger kind.
    pub trigger: TriggerKind,
    /// New cron expression (schedules only).
    pub schedule: Option<String>,
    /// New event name (event triggers only).
    pub trigger_event: Option<String>,
    /// New conditions (event triggers only).
    pub conditions: serde_json::Value,
    /// New next due time (schedules only).
    pub next_run_at: Option<OffsetDateTime>,
    /// The rule's own error policy.
    pub on_error: crate::model::OnError,
    /// New step definitions as stored JSON.
    pub steps: serde_json::Value,
}

/// Rewrite a workflow definition; `None` when the row is gone.
pub async fn update_workflow(
    pool: &PgPool,
    id: Uuid,
    update: WorkflowUpdate,
) -> Result<Option<Workflow>> {
    let sql = format!(
        "update workflows set name = $2, description = $3, site_id = $4, enabled = $5, \
         trigger_kind = $6, schedule = $7, next_run_at = $8, steps = $9, trigger_event = $10, \
         conditions = $11, on_error = $12, updated_at = now() \
         where id = $1 returning {}",
        workflow_columns()
    );

    let workflow: Option<Workflow> = sqlx::query_as(&sql)
        .bind(id)
        .bind(update.name)
        .bind(update.description)
        .bind(update.site_id)
        .bind(update.enabled)
        .bind(update.trigger.as_str())
        .bind(update.schedule)
        .bind(update.next_run_at)
        .bind(update.steps)
        .bind(update.trigger_event)
        .bind(update.conditions)
        .bind(update.on_error.as_str())
        .fetch_optional(pool)
        .await?;

    Ok(workflow)
}

/// Remove a workflow and (through the schema) its executions.
pub async fn delete_workflow(pool: &PgPool, id: Uuid) -> Result<bool> {
    let removed = sqlx::query("delete from workflows where id = $1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();

    Ok(removed > 0)
}

/// Claim the scheduled workflows that are due, advancing each one's next due time.
///
/// The claim takes its row locks before it computes anything, so two runners sweeping at the
/// same second start each due workflow exactly once. A workflow whose stored cron no longer
/// parses is disabled instead of being retried every second.
pub async fn claim_due_schedules(pool: &PgPool, limit: i64) -> Result<Vec<Workflow>> {
    let mut transaction = pool.begin().await?;

    let sql = format!(
        "select {} from workflows \
         where enabled and trigger_kind = 'schedule' and next_run_at is not null \
           and next_run_at <= now() \
         order by next_run_at asc, id limit $1 for update skip locked",
        workflow_columns()
    );

    let due: Vec<Workflow> = sqlx::query_as(&sql)
        .bind(limit)
        .fetch_all(&mut *transaction)
        .await?;

    let now = OffsetDateTime::now_utc();
    let mut claimed = Vec::with_capacity(due.len());

    for workflow in due {
        let expression = workflow.schedule.clone().unwrap_or_default();
        match crate::cron::CronSchedule::parse(&expression) {
            Ok(schedule) => {
                let next = schedule.next_after(now)?;
                sqlx::query("update workflows set next_run_at = $2 where id = $1")
                    .bind(workflow.id)
                    .bind(next)
                    .execute(&mut *transaction)
                    .await?;
                claimed.push(workflow);
            }
            Err(err) => {
                tracing::warn!(
                    workflow_id = %workflow.id,
                    expression,
                    error = %err,
                    "disabling a schedule whose cron expression no longer parses"
                );
                sqlx::query(
                    "update workflows set enabled = false, updated_at = now() where id = $1",
                )
                .bind(workflow.id)
                .execute(&mut *transaction)
                .await?;
            }
        }
    }

    transaction.commit().await?;
    Ok(claimed)
}

/// Start a run: one execution row plus one row per step, written together.
///
/// Materialising the steps up front is what makes the run durable — from this moment the work
/// is a set of rows, and the runner only ever moves them forward.
pub async fn create_execution(
    pool: &PgPool,
    workflow: &Workflow,
    trigger: TriggerKind,
    triggered_by: Option<Uuid>,
    steps: &[StepDefinition],
) -> Result<(WorkflowExecution, Vec<WorkflowStep>)> {
    let mut transaction = pool.begin().await?;
    let created = create_execution_in(
        &mut transaction,
        workflow,
        trigger,
        triggered_by,
        steps,
        None,
    )
    .await?;
    transaction.commit().await?;
    Ok(created)
}

/// The same write, inside a transaction the caller owns.
///
/// The automation layer needs it: a match starts a run and advances the event cursor in ONE
/// transaction, so a crash can never leave a cursor that skipped an event whose run never
/// existed (and a replay can never start the same run twice). It also passes the event
/// payload, because a branch step reads `event.<field>` from the run and the run detail
/// shows the payload beside the trace.
pub async fn create_execution_in(
    connection: &mut sqlx::PgConnection,
    workflow: &Workflow,
    trigger: TriggerKind,
    triggered_by: Option<Uuid>,
    steps: &[StepDefinition],
    event_payload: Option<serde_json::Value>,
) -> Result<(WorkflowExecution, Vec<WorkflowStep>)> {
    let execution_sql = format!(
        "insert into workflow_executions (workflow_id, organization_id, status, trigger_kind, \
         triggered_by, event_payload) values ($1, $2, 'running', $3, $4, $5) returning {}",
        EXECUTION_COLUMNS
    );

    let execution: WorkflowExecution = sqlx::query_as(&execution_sql)
        .bind(workflow.id)
        .bind(workflow.organization_id)
        .bind(trigger.as_str())
        .bind(triggered_by)
        .bind(event_payload)
        .fetch_one(&mut *connection)
        .await?;

    let step_sql = format!(
        "insert into workflow_steps (execution_id, step_no, name, kind, action, params, on_error, \
         timeout_ms, status, attempts, max_attempts) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, 'pending', 0, $9) \
         returning {}",
        STEP_COLUMNS
    );

    let mut rows = Vec::with_capacity(steps.len());
    for (index, step) in steps.iter().enumerate() {
        // A control step's `action` is derived, not authored: the definition check already
        // refused a branch that names an action and a stop that does not, and the panel
        // writes `{"kind": "branch", "params": {…}}` with no `action` field at all. Storing
        // `None` for one of those would trip `workflow_steps_action_shape`, which is the
        // database doing exactly its job.
        let action = match step.kind {
            StepKind::Branch => Some(crate::branch::BRANCH_ACTION.to_owned()),
            StepKind::Task => step.action.clone(),
            StepKind::Wait | StepKind::Stop => None,
        };

        let row: WorkflowStep = sqlx::query_as(&step_sql)
            .bind(execution.id)
            .bind(index as i32 + 1)
            .bind(step.name.trim())
            .bind(step.kind.as_str())
            .bind(action)
            .bind(step.params.clone())
            .bind(step.on_error.as_str())
            .bind(step.timeout_ms)
            .bind(step.max_attempts)
            .fetch_one(&mut *connection)
            .await?;
        rows.push(row);
    }

    Ok((execution, rows))
}

/// The armed, event-triggered workflows of one tenant that listen for one event.
///
/// A rule with no site applies to the whole organization; a rule bound to a site only fires for
/// that site's events. An event without an organization matches nothing — the same rule the bus
/// applies to webhook fan-out: an event that belongs to nobody cannot reach a tenant.
///
/// Takes any executor, because the matcher reads this inside the transaction that also creates
/// the runs and advances its cursor.
pub async fn list_event_rules<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    organization_id: Uuid,
    site_id: Option<Uuid>,
    event_name: &str,
) -> Result<Vec<Workflow>> {
    let sql = format!(
        "select {} from workflows \
         where trigger_kind = 'event' and enabled and trigger_event = $1 \
           and organization_id = $2 \
           and (site_id is null or site_id = $3) \
         order by created_at asc, id",
        workflow_columns()
    );

    let workflows: Vec<Workflow> = sqlx::query_as(&sql)
        .bind(event_name)
        .bind(organization_id)
        .bind(site_id)
        .fetch_all(executor)
        .await?;

    Ok(workflows)
}

/// The event-triggered workflows of a scope, for the automations surface.
pub async fn list_event_workflows(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    site_id: Option<Uuid>,
) -> Result<Vec<Workflow>> {
    let sql = format!(
        "select {} from workflows \
         where trigger_kind = 'event' \
           and ($1::uuid is null or organization_id = $1) \
           and ($2::uuid is null or site_id = $2) \
         order by created_at desc, id",
        workflow_columns()
    );

    let workflows: Vec<Workflow> = sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(site_id)
        .fetch_all(pool)
        .await?;

    Ok(workflows)
}

/// Record that a rule fired: one more run, and when.
///
/// Called by the matcher inside its own transaction, so the counter moves exactly with the run
/// it counts.
pub async fn note_match(connection: &mut sqlx::PgConnection, workflow_id: Uuid) -> Result<()> {
    sqlx::query(
        "update workflows set last_triggered_at = now(), trigger_count = trigger_count + 1 \
         where id = $1",
    )
    .bind(workflow_id)
    .execute(&mut *connection)
    .await?;

    Ok(())
}

/// Load one run.
pub async fn find_execution(pool: &PgPool, id: Uuid) -> Result<Option<WorkflowExecution>> {
    let sql = format!(
        "select {} from workflow_executions where id = $1",
        EXECUTION_COLUMNS
    );
    let execution: Option<WorkflowExecution> =
        sqlx::query_as(&sql).bind(id).fetch_optional(pool).await?;
    Ok(execution)
}

/// Runs of one workflow, newest first.
pub async fn list_executions(
    pool: &PgPool,
    workflow_id: Uuid,
    limit: i64,
) -> Result<Vec<WorkflowExecution>> {
    let sql = format!(
        "select {} from workflow_executions where workflow_id = $1 \
         order by started_at desc, id desc limit $2",
        EXECUTION_COLUMNS
    );

    let executions: Vec<WorkflowExecution> = sqlx::query_as(&sql)
        .bind(workflow_id)
        .bind(limit)
        .fetch_all(pool)
        .await?;

    Ok(executions)
}

/// Steps of one run, in order.
pub async fn list_steps(pool: &PgPool, execution_id: Uuid) -> Result<Vec<WorkflowStep>> {
    let sql = format!(
        "select {} from workflow_steps where execution_id = $1 order by step_no asc",
        STEP_COLUMNS
    );

    let steps: Vec<WorkflowStep> = sqlx::query_as(&sql)
        .bind(execution_id)
        .fetch_all(pool)
        .await?;

    Ok(steps)
}

/// A step the runner just claimed (its attempt counter already increased).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ClaimedStep {
    /// Step id.
    pub id: Uuid,
    /// Run the step belongs to.
    pub execution_id: Uuid,
    /// Position in the run.
    pub step_no: i32,
    /// Step name.
    pub name: String,
    /// `task`, `wait`, `branch` or `stop`.
    pub kind: String,
    /// Built-in action of a task step.
    pub action: Option<String>,
    /// Step parameters.
    pub params: serde_json::Value,
    /// What this step's own failure does (`inherit` takes the rule's policy).
    pub on_error: String,
    /// How long the step may block before it is failed with the limit named.
    pub timeout_ms: i32,
    /// Attempt number of the claim (1 for the first).
    pub attempts: i32,
    /// Attempts allowed in total.
    pub max_attempts: i32,
}

/// Claim the next step that is due.
///
/// One statement, one row: the run's oldest open step that is due and whose earlier steps have
/// all finished. The row lock makes the claim exclusive even when several API instances run the
/// engine at the same time.
pub async fn claim_due_step(pool: &PgPool) -> Result<Option<ClaimedStep>> {
    let claimed: Option<ClaimedStep> = sqlx::query_as(
        "with due as ( \
             select s.id from workflow_steps s \
             join workflow_executions e on e.id = s.execution_id \
             where e.status = 'running' \
               and s.status in ('pending', 'waiting') \
               and s.available_at <= now() \
               and not exists ( \
                   select 1 from workflow_steps earlier \
                   where earlier.execution_id = s.execution_id \
                     and earlier.step_no < s.step_no \
                     and earlier.status in ('pending', 'running', 'waiting') \
               ) \
             order by e.started_at asc, e.id, s.step_no asc \
             limit 1 for update of s skip locked \
         ) \
         update workflow_steps s \
         set status = 'running', attempts = s.attempts + 1, started_at = now() \
         from due where s.id = due.id \
         returning s.id, s.execution_id, s.step_no, s.name, s.kind, s.action, s.params, \
                   s.on_error, s.timeout_ms, s.attempts, s.max_attempts",
    )
    .fetch_optional(pool)
    .await?;

    Ok(claimed)
}

/// Park a wait step until `available_at`; the run stays open meanwhile.
pub async fn set_step_waiting(
    pool: &PgPool,
    step_id: Uuid,
    available_at: OffsetDateTime,
) -> Result<()> {
    sqlx::query(
        "update workflow_steps set status = 'waiting', started_at = null, available_at = $2 \
         where id = $1 and status = 'running'",
    )
    .bind(step_id)
    .bind(available_at)
    .execute(pool)
    .await?;

    Ok(())
}

/// Mark a step succeeded, with its output.
pub async fn complete_step(pool: &PgPool, step_id: Uuid, output: &serde_json::Value) -> Result<()> {
    sqlx::query(
        "update workflow_steps set status = 'succeeded', finished_at = now(), output = $2, \
         error = null where id = $1 and status = 'running'",
    )
    .bind(step_id)
    .bind(output)
    .execute(pool)
    .await?;

    Ok(())
}

/// Put a failed step back in the queue, one backoff later.
pub async fn retry_step(
    pool: &PgPool,
    step_id: Uuid,
    message: &str,
    available_at: OffsetDateTime,
) -> Result<()> {
    sqlx::query(
        "update workflow_steps set status = 'pending', error = $2, available_at = $3, \
         started_at = null where id = $1 and status = 'running'",
    )
    .bind(step_id)
    .bind(message)
    .bind(available_at)
    .execute(pool)
    .await?;

    Ok(())
}

/// Mark a step permanently failed.
pub async fn fail_step(pool: &PgPool, step_id: Uuid, message: &str) -> Result<()> {
    sqlx::query(
        "update workflow_steps set status = 'failed', finished_at = now(), error = $2 \
         where id = $1 and status = 'running'",
    )
    .bind(step_id)
    .bind(message)
    .execute(pool)
    .await?;

    Ok(())
}

/// Mark a claimed step cancelled because its run was cancelled while the step ran.
pub async fn cancel_step(pool: &PgPool, step_id: Uuid) -> Result<()> {
    sqlx::query(
        "update workflow_steps set status = 'cancelled', finished_at = now(), \
         error = 'the run was cancelled' \
         where id = $1 and status in ('pending', 'running', 'waiting')",
    )
    .bind(step_id)
    .execute(pool)
    .await?;

    Ok(())
}

/// Mark a step failed, and mark that the run deliberately outlived it.
///
/// The row stays `failed` — the trace must not pretend a step succeeded — and `ignored` is
/// what tells [`settle_execution`] that this failure does not make the run a failure. A run
/// whose only failure was outlived settles as completed, and its summary still names what
/// happened, because the trace keeps the row and the error.
pub async fn fail_step_ignored(pool: &PgPool, step_id: Uuid, message: &str) -> Result<()> {
    sqlx::query(
        "update workflow_steps set status = 'failed', finished_at = now(), error = $2, \
         ignored = true where id = $1 and status = 'running'",
    )
    .bind(step_id)
    .bind(message)
    .execute(pool)
    .await?;

    Ok(())
}

/// Close every step after `step_id` as cancelled, because a branch or a stop ended the run.
///
/// `cancelled` rather than `succeeded` is the honest state: those steps never ran, and the
/// trace must say so. Their cancellation does not fail the run either — the run's own
/// terminal state is written separately by the caller, and a run that stopped on purpose is
/// a completed run.
pub async fn end_run_after_branch(pool: &PgPool, execution_id: Uuid, step_id: Uuid) -> Result<u64> {
    let closed = sqlx::query(
        "update workflow_steps set status = 'cancelled', finished_at = now(), \
             error = 'the run ended before this step' \
         where execution_id = $1 and id <> $2 \
           and status in ('pending', 'waiting', 'running')",
    )
    .bind(execution_id)
    .bind(step_id)
    .execute(pool)
    .await?
    .rows_affected();

    Ok(closed)
}

/// Write a run's terminal state directly, for the two cases the engine decides itself.
///
/// Only the two *voluntary* endings use this: a branch that decided, and a stop step. A
/// failure still goes through [`settle_execution`], because only that one derives the status
/// from the rows.
pub async fn settle_execution_as(pool: &PgPool, execution_id: Uuid, status: &str) -> Result<bool> {
    let settled = sqlx::query(
        "update workflow_executions set status = $2, finished_at = now() \
         where id = $1 and status = 'running'",
    )
    .bind(execution_id)
    .bind(status)
    .execute(pool)
    .await?
    .rows_affected();

    Ok(settled > 0)
}

/// Put a claimed step back in the queue without counting an attempt.
///
/// For a step whose claim was lost to a stopped runner and that has no attempts to spend —
/// a wait that never parked, a branch or a stop that never decided. The attempt counter is
/// left alone, which is what makes the lost claim invisible to the step's own budget.
pub async fn requeue_step(pool: &PgPool, step_id: Uuid) -> Result<()> {
    sqlx::query(
        "update workflow_steps set status = 'pending', started_at = null, \
         available_at = now() where id = $1 and status = 'running'",
    )
    .bind(step_id)
    .execute(pool)
    .await?;

    Ok(())
}

/// Re-open a run from one of its steps: the step and everything after it go back on the
/// queue, and the steps that already succeeded are left untouched.
///
/// This is **Retry** and **Resume from here** on the run detail, and they are the same
/// write on purpose. Re-running only the failed step would let a run whose middle failed
/// march on to completion, so a retry deliberately re-runs the whole tail — which is
/// exactly what an operator means by "try that again" on a run detail.
///
/// Three invariants the write keeps:
///
/// * a `cancelled` step is re-opened but a **cancelled run is not** — cancellation was a
///   person's decision, and "retry" must not silently undo it;
/// * the attempt counter of a re-opened step is reset, because the operator's click is a
///   new attempt budget, not the last one of the old one;
/// * `ignored` is cleared, so a step that once failed and was deliberately outlived does
///   not stay exempt when it is run again.
///
/// It answers how many steps it re-opened, so the caller can log it and the panel can say
/// "steps 3–5 are queued again" rather than "something happened".
pub async fn retry_step_from(pool: &PgPool, execution_id: Uuid, step_no: i32) -> Result<u64> {
    let requeued = sqlx::query(
        "update workflow_steps set status = 'pending', error = null, ignored = false, \
             attempts = 0, available_at = now(), started_at = null, finished_at = null, \
             output = null \
         where execution_id = $1 and step_no >= $2 \
           and status in ('failed', 'cancelled', 'pending', 'waiting')",
    )
    .bind(execution_id)
    .bind(step_no)
    .execute(pool)
    .await?
    .rows_affected();

    sqlx::query(
        "update workflow_executions set status = 'running', finished_at = null, error = null \
         where id = $1 and status = 'failed'",
    )
    .bind(execution_id)
    .execute(pool)
    .await?;

    Ok(requeued)
}

/// One step of a run, as the retry/resume endpoints need to address it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StepPosition {
    /// The step's 1-based position in its run.
    pub step_no: i32,
    /// The step's current status.
    pub status: String,
    /// The step's `on_error` policy, for the log line.
    pub on_error: String,
}

/// Read the one step a retry/resume request addresses, or `None` when it does not exist.
pub async fn find_step(
    pool: &PgPool,
    execution_id: Uuid,
    step_no: i32,
) -> Result<Option<StepPosition>> {
    let step: Option<StepPosition> = sqlx::query_as(
        "select step_no, status, on_error from workflow_steps \
         where execution_id = $1 and step_no = $2",
    )
    .bind(execution_id)
    .bind(step_no)
    .fetch_optional(pool)
    .await?;

    Ok(step)
}

/// Derive a run's terminal state once no step is open, and write it.
///
/// Answers `Some(status)` only for the call that actually settled the run, so the caller can
/// audit a state change exactly once.
pub async fn settle_execution(
    pool: &PgPool,
    execution_id: Uuid,
) -> Result<Option<ExecutionStatus>> {
    #[derive(sqlx::FromRow)]
    struct Counts {
        open: i64,
        failed: i64,
        cancelled: i64,
    }

    let counts: Counts = sqlx::query_as(
        "select \
             count(*) filter (where status in ('pending', 'running', 'waiting')) as open, \
             count(*) filter (where status = 'failed' and not ignored) as failed, \
             count(*) filter (where status = 'cancelled') as cancelled \
         from workflow_steps where execution_id = $1",
    )
    .bind(execution_id)
    .fetch_one(pool)
    .await?;

    if counts.open > 0 {
        return Ok(None);
    }

    let status = if counts.failed > 0 {
        ExecutionStatus::Failed
    } else if counts.cancelled > 0 {
        ExecutionStatus::Cancelled
    } else {
        ExecutionStatus::Completed
    };

    // A failed run reports the first failure it met, so a later reader sees why. A failure
    // the run deliberately outlived is not one: `ignored` is the flag that says the author
    // chose to continue past it, so the run is not a failure because of that row.
    let error: Option<String> = if status == ExecutionStatus::Failed {
        sqlx::query_scalar::<_, Option<String>>(
            "select error from workflow_steps where execution_id = $1 and status = 'failed' \
             and not ignored order by step_no asc limit 1",
        )
        .bind(execution_id)
        .fetch_one(pool)
        .await?
    } else {
        None
    };

    let settled = sqlx::query(
        "update workflow_executions set status = $2, finished_at = now(), error = $3 \
         where id = $1 and status = 'running'",
    )
    .bind(execution_id)
    .bind(status.as_str())
    .bind(error)
    .execute(pool)
    .await?
    .rows_affected();

    Ok(if settled > 0 { Some(status) } else { None })
}

/// Cancel a running execution: the flag is the row's status, and every open step is closed.
pub async fn cancel_execution(
    pool: &PgPool,
    execution_id: Uuid,
) -> Result<Option<WorkflowExecution>> {
    let mut transaction = pool.begin().await?;

    let update_sql = format!(
        "update workflow_executions set status = 'cancelled', finished_at = now() \
         where id = $1 and status = 'running' returning {}",
        EXECUTION_COLUMNS
    );

    let cancelled: Option<WorkflowExecution> = sqlx::query_as(&update_sql)
        .bind(execution_id)
        .fetch_optional(&mut *transaction)
        .await?;

    if cancelled.is_some() {
        let step_sql = format!(
            "update workflow_steps set status = 'cancelled', finished_at = now(), \
             error = 'the run was cancelled' \
             where execution_id = $1 and status in ('pending', 'waiting') returning {}",
            STEP_COLUMNS
        );
        let _: Vec<WorkflowStep> = sqlx::query_as(&step_sql)
            .bind(execution_id)
            .fetch_all(&mut *transaction)
            .await?;
    }

    transaction.commit().await?;
    Ok(cancelled)
}

/// Finish the parked waits whose deadline has passed. Returns the runs they belong to.
///
/// The runner also picks a due wait up on its own tick; this sweep is the batch pass that
/// resolves overdue waits even if the runner is busy, and it closes the wait itself so a wake-up
/// is never counted as a second attempt. The sub-select locks the rows it takes
/// (`skip locked`), so a sweep and a tick that race over one wait cannot both act on it.
pub async fn resolve_due_waits(pool: &PgPool, limit: i64) -> Result<Vec<Uuid>> {
    let executions: Vec<Uuid> = sqlx::query_scalar(
        "with due as ( \
             select s.id from workflow_steps s \
             join workflow_executions e on e.id = s.execution_id \
             where s.status = 'waiting' and s.available_at <= now() and e.status = 'running' \
             order by s.available_at asc limit $1 for update of s skip locked \
         ) \
         update workflow_steps s \
         set status = 'succeeded', finished_at = now(), \
             output = '{\"waited\": true, \"resumed\": true}'::jsonb \
         from due where s.id = due.id \
         returning s.execution_id",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(executions)
}

/// Steps a runner claimed and never finished — the process that held them stopped mid-step.
///
/// `lease_seconds` is how long a claim may stay open before it counts as abandoned.
pub async fn stale_steps(
    pool: &PgPool,
    lease_seconds: i64,
    limit: i64,
) -> Result<Vec<WorkflowStep>> {
    let sql = "select s.id, s.execution_id, s.step_no, s.name, s.kind, s.action, s.params, \
                      s.on_error, s.timeout_ms, s.status, s.attempts, s.max_attempts, \
                      s.available_at, s.started_at, s.finished_at, s.output, s.error, s.ignored \
               from workflow_steps s \
               join workflow_executions e on e.id = s.execution_id \
               where s.status = 'running' and e.status = 'running' \
                 and s.started_at is not null \
                 and s.started_at < now() - ($1 * interval '1 second') \
               order by s.started_at asc limit $2";

    let stale: Vec<WorkflowStep> = sqlx::query_as(sql)
        .bind(lease_seconds)
        .bind(limit)
        .fetch_all(pool)
        .await?;

    Ok(stale)
}

/// Settle runs that were left open without any step still to run.
///
/// An instance can stop between "the last step succeeded" and "the run is completed"; the next
/// sweep closes those runs instead of leaving them running forever.
pub async fn reconcile_executions(pool: &PgPool, limit: i64) -> Result<Vec<Uuid>> {
    let stale: Vec<Uuid> = sqlx::query_scalar(
        "select e.id from workflow_executions e \
         where e.status = 'running' \
           and not exists ( \
               select 1 from workflow_steps s \
               where s.execution_id = e.id and s.status in ('pending', 'running', 'waiting') \
           ) \
         order by e.started_at asc limit $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let mut settled = Vec::with_capacity(stale.len());
    for execution_id in stale {
        if settle_execution(pool, execution_id).await?.is_some() {
            settled.push(execution_id);
        }
    }

    Ok(settled)
}

/// Backoff before the next attempt: exponential from `base`, capped at `max`.
///
/// Attempt 1 waits one base, attempt 2 two bases, attempt 3 four … never more than `max`. A
/// cap below the base is raised to the base, so the first wait is always the caller's base.
#[must_use]
pub fn retry_delay(attempt: i32, base: Duration, max: Duration) -> Duration {
    let shift = u32::try_from(attempt.clamp(1, 16) - 1).unwrap_or(0);
    let factor = 1_i64.checked_shl(shift).unwrap_or(i64::MAX);
    let base_ms = base.whole_milliseconds().max(1) as i64;
    let cap_ms = (max.whole_milliseconds() as i64).max(base_ms);
    Duration::milliseconds(base_ms.saturating_mul(factor).min(cap_ms))
}

/// Start of a `status in ('pending','waiting')` claim: the value the store compares against.
#[must_use]
pub fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

/// Type-check helper used by the API when it addresses a stored status.
pub fn parse_execution_status(raw: &str) -> Result<ExecutionStatus> {
    ExecutionStatus::parse(raw).ok_or_else(|| {
        WorkflowError::invalid("workflow_store_error", format!("unknown status {raw:?}"))
    })
}

/// Type-check helper used by the API when it addresses a stored step status.
pub fn parse_step_status(raw: &str) -> Result<StepStatus> {
    StepStatus::parse(raw).ok_or_else(|| {
        WorkflowError::invalid("workflow_store_error", format!("unknown status {raw:?}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_doubles_and_then_stops_at_the_cap() {
        let base = Duration::milliseconds(500);
        let max = Duration::seconds(10);

        assert_eq!(retry_delay(1, base, max), Duration::milliseconds(500));
        assert_eq!(retry_delay(2, base, max), Duration::seconds(1));
        assert_eq!(retry_delay(3, base, max), Duration::seconds(2));
        assert_eq!(retry_delay(4, base, max), Duration::seconds(4));
        assert_eq!(retry_delay(5, base, max), Duration::seconds(8));
        assert_eq!(retry_delay(6, base, max), Duration::seconds(10), "capped");
        assert_eq!(retry_delay(99, base, max), Duration::seconds(10), "capped");
    }

    #[test]
    fn the_cap_never_collapses_the_delay_to_zero() {
        // A cap below the base is raised to the base: the first wait is always the base.
        let delay = retry_delay(1, Duration::milliseconds(5), Duration::milliseconds(0));
        assert_eq!(delay, Duration::milliseconds(5));
        let delay = retry_delay(4, Duration::milliseconds(5), Duration::milliseconds(0));
        assert_eq!(delay, Duration::milliseconds(5), "capped at the base");
    }

    #[test]
    fn stored_statuses_are_validated_before_they_are_trusted() {
        assert_eq!(
            parse_execution_status("running").expect("known"),
            ExecutionStatus::Running
        );
        assert!(parse_execution_status("paused").is_err());
        assert_eq!(
            parse_step_status("waiting").expect("known"),
            StepStatus::Waiting
        );
        assert!(parse_step_status("sleeping").is_err());
    }
}
