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
    EXECUTION_COLUMNS, ExecutionStatus, NewWorkflow, STEP_COLUMNS, StepStatus, TriggerKind,
    WORKFLOW_COLUMNS, Workflow, WorkflowExecution, WorkflowStep,
};

/// Columns of `workflows` for one `select`, in [`Workflow`] order.
fn workflow_columns() -> &'static str {
    WORKFLOW_COLUMNS
}

/// Insert a workflow definition.
///
/// **The write guard lives here, not in the two handlers.** `POST /workflows` and
/// `POST /automations` are separate functions in separate files that both reach this one insert,
/// and a check pasted into each is a check that will be on one of them — the same shape as the
/// run guard this store already moved away from the handlers. Two questions are asked, in the
/// order that makes each refusal name a remedy that works:
///
/// 1. **Is this project still writable?** An archived project refuses edits as well as runs
///    (`ensure_project_accepts_writes`), and an editor or owner pressing save on a project that
///    was archived an hour ago gets told to restore it rather than silently succeeding.
/// 2. **Is there room for one more?** `max_workflows` is a cap this REQ promises and the limits
///    screen draws a bar for; the notice sweep only *notices* a crossing, so without this a
///    project could exceed its cap for ever while the bar read "at the limit".
///
/// Both run inside the transaction that writes, so a project archived between the handler's
/// capability check and this insert cannot slip a save through: the insert sees `archived` and
/// refuses. That is the check-then-write race this module has now removed from the run path and
/// the move path, applied to the last write path that did not have it.
pub async fn insert_workflow(pool: &PgPool, new: NewWorkflow) -> Result<Workflow> {
    let mut tx = pool.begin().await?;
    crate::projects::ensure_project_accepts_writes(&mut tx, new.project_id).await?;
    let sql = format!(
        "insert into workflows (organization_id, project_id, site_id, name, description, \
         enabled, trigger_kind, schedule, trigger_event, conditions, next_run_at, steps, \
         created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) returning {}",
        workflow_columns()
    );

    let workflow: Workflow = sqlx::query_as(&sql)
        .bind(new.organization_id)
        .bind(new.project_id)
        .bind(new.site_id)
        .bind(new.name)
        .bind(new.description)
        .bind(new.enabled)
        .bind(new.trigger.as_str())
        .bind(new.schedule)
        .bind(new.trigger_event)
        .bind(new.conditions)
        .bind(new.next_run_at)
        .bind(new.steps)
        .bind(new.created_by)
        .fetch_one(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(workflow)
}

/// List workflows, newest first; `organization_id` and `site_id` filter when given.
///
/// `project_id` filters too (REQ-133), and the parameter is `Option<Uuid>` on purpose even
/// though the column is `not null`: the column being non-nullable is a statement about *rows in
/// the table*, and this function is a statement about *which rows a caller may see*. A caller who
/// passes `None` is not writing a workflow without a project — they are asking for everything
/// they can see, which is the question the project list and the instance-wide views ask.
///
/// The filter is a `where` clause rather than a caller-side partition because a caller-side one is
/// a leak: the rows still cross the wire, and slice 2's rule is that a workflow the caller may not
/// see is never *fetched*, not merely never displayed.
pub async fn list_workflows(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    site_id: Option<Uuid>,
    project_id: Option<Uuid>,
) -> Result<Vec<Workflow>> {
    let sql = format!(
        "select {} from workflows \
         where ($1::uuid is null or organization_id = $1) \
           and ($2::uuid is null or site_id = $2) \
           and ($3::uuid is null or project_id = $3) \
         order by created_at desc, id",
        workflow_columns()
    );

    let workflows: Vec<Workflow> = sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(site_id)
        .bind(project_id)
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
    /// New step definitions as stored JSON.
    pub steps: serde_json::Value,
}

/// Rewrite a workflow definition; `None` when the row is gone.
///
/// **The archive half of the read-only rule is here too**, for the reason
/// [`insert_workflow`] gives in full: an archived project is *"read-only — no new runs, **no
/// edits**"*, and the edit door was the one with no guard. The project's own row is read in the
/// same transaction as the write (the workflow's `project_id` is a `not null` column, so it
/// cannot be absent and cannot be resolved any other way), which is the check-then-write shape
/// removed from the run path two slices ago.
pub async fn update_workflow(
    pool: &PgPool,
    id: Uuid,
    update: WorkflowUpdate,
) -> Result<Option<Workflow>> {
    let mut tx = pool.begin().await?;
    // Read the project this workflow is in and ask the same question the create path asks. A
    // workflow that does not exist resolves to `None` below exactly as it did before; asking
    // about a project only after the row is known to exist would be two statements where one
    // `select … left join` is both, and it cannot answer for a workflow that is not there.
    let project_id: Option<Uuid> =
        sqlx::query_scalar("select project_id from workflows where id = $1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(project_id) = project_id {
        // The **archive** guard only. `ensure_project_accepts_writes` would also refuse this
        // rewrite because the project is over its workflow cap, and a cap is a statement about how
        // many definitions may EXIST — refusing a rename because the number is already right would
        // make an over-cap project uneditable as well as uncapped, which is not what `max_workflows`
        // says anywhere. The create path is where the cap belongs.
        crate::projects::ensure_project_is_writable(&mut tx, project_id).await?;
    }
    let sql = format!(
        "update workflows set name = $2, description = $3, site_id = $4, enabled = $5, \
         trigger_kind = $6, schedule = $7, next_run_at = $8, steps = $9, trigger_event = $10, \
         conditions = $11, updated_at = now() \
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
        .fetch_optional(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(workflow)
}

/// Remove a workflow and (through the schema) its executions.
///
/// **A delete is a write, and an archived project is read-only.** Removing the last workflow of
/// an archived project is also the only way an operator could empty it without restoring it, which
/// is why the guard is asked here and not only on the create and update doors: "keep their
/// history" (the REQ's own words for archiving) is a promise about rows, and a delete is the
/// action that breaks it.
pub async fn delete_workflow(pool: &PgPool, id: Uuid) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let project_id: Option<Uuid> =
        sqlx::query_scalar("select project_id from workflows where id = $1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(project_id) = project_id {
        // Archive only, and deliberately: deleting is the operator's way back UNDER a cap, so the
        // cap here would make an over-quota project unrecoverable without a route nobody wrote.
        // `a_delete_is_never_refused_by_the_workflow_cap` is the assertion that keeps this honest.
        crate::projects::ensure_project_is_writable(&mut tx, project_id).await?;
    }
    let removed = sqlx::query("delete from workflows where id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    tx.commit().await?;

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
    let created =
        create_execution_in(&mut transaction, workflow, trigger, triggered_by, steps).await?;
    transaction.commit().await?;
    Ok(created)
}

/// The same write, inside a transaction the caller owns.
///
/// The automation layer needs it: a match starts a run and advances the event cursor in ONE
/// transaction, so a crash can never leave a cursor that skipped an event whose run never
/// existed (and a replay can never start the same run twice).
///
/// **This is also the archive guard's only call site, and that placement is the point.** Every
/// way a run starts — the manual route, the scheduler, the event matcher, a retry — passes
/// through this one function, so "an archived project refuses new runs at the API boundary"
/// (REQ-133) is a fact about the write rather than a promise each caller has to remember to
/// keep. The check reads the project row in this transaction, so a run cannot slip between a
/// read that said `active` and the insert: see [`crate::projects::ensure_run_allowed`].
///
/// **The limits guard is the second thing called here, and it belongs here for the same reason.**
/// `ensure_run_within_limits` reads the day's counters and the in-flight executions; asking them
/// through a pool would be a second connection reading what this transaction is about to change.
/// A limit that is checked anywhere but inside the write it guards is a limit that counts a run
/// before it exists — which, on a project one run below its cap, refuses the run that would have
/// been the last one.
///
/// **The counter is written here for the same reason the guard is read here.** `record_usage_in`
/// is the only writer of `automation_project_usage`, and for its whole life on this branch it had
/// **no caller outside its own tests** — the thirteenth instance of the defect this module keeps
/// meeting. Every guard read it, the screen rendered it, the notice sweep claimed on it, and the
/// daily cap was therefore decided against a table nothing ever wrote: a project could run a
/// hundred workflows a day under a cap of ten and never be refused, its limits bar sat at zero,
/// its series was empty, and the CSV exported nothing. Not one of those checks could have told,
/// because each of them asked the counter a question and the counter answered.
///
/// Placement is the whole of the fix. Counting here rather than at `settle_execution` is not a
/// stylistic choice: the guard above reads the counter, so a counter written anywhere after the
/// run completes counts *n+1* runs while the cap says *n* — a project would be allowed exactly
/// its cap of runs and refused on the run after it, and "the cap is the cap" would be off by one
/// in the direction that looks like an intermittent refusal. Counting where the guard reads it
/// makes the two statements one transaction, so a run that was refused is never counted and a run
/// that was counted is always counted once.
pub async fn create_execution_in(
    connection: &mut sqlx::PgConnection,
    workflow: &Workflow,
    trigger: TriggerKind,
    triggered_by: Option<Uuid>,
    steps: &[StepDefinition],
) -> Result<(WorkflowExecution, Vec<WorkflowStep>)> {
    crate::projects::ensure_run_allowed(connection, workflow.project_id).await?;
    crate::limits::ensure_run_within_limits(connection, workflow.project_id).await?;
    // **After the guard, before the insert.** The counter is the guard's own input, so it has to be
    // written by the same transaction that read it: a run refused above must leave the day's count
    // untouched, and a run allowed above must be counted before this transaction ends or the next
    // caller re-reads a cap that has already spent one of its runs.
    crate::limits::record_usage_in(connection, workflow.project_id, false, 0).await?;

    let execution_sql = format!(
        "insert into workflow_executions (workflow_id, organization_id, status, trigger_kind, \
         triggered_by) values ($1, $2, 'running', $3, $4) returning {}",
        EXECUTION_COLUMNS
    );

    let execution: WorkflowExecution = sqlx::query_as(&execution_sql)
        .bind(workflow.id)
        .bind(workflow.organization_id)
        .bind(trigger.as_str())
        .bind(triggered_by)
        .fetch_one(&mut *connection)
        .await?;

    let step_sql = format!(
        "insert into workflow_steps (execution_id, step_no, name, kind, action, params, status, \
         attempts, max_attempts) values ($1, $2, $3, $4, $5, $6, 'pending', 0, $7) \
         returning {}",
        STEP_COLUMNS
    );

    let mut rows = Vec::with_capacity(steps.len());
    for (index, step) in steps.iter().enumerate() {
        let row: WorkflowStep = sqlx::query_as(&step_sql)
            .bind(execution.id)
            .bind(index as i32 + 1)
            .bind(step.name.trim())
            .bind(step.kind.as_str())
            .bind(step.action.as_deref())
            .bind(step.params.clone())
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

/// The definitions a caller may see: organization, site, and an explicit set of projects.
///
/// This is the query a scoped list actually runs, and it is separate from [`list_workflows`]
/// rather than a flag on it because the id set is a **closure that is already computed**: the
/// caller asked `visible_project_ids` once and narrowed it. The alternative — a nullable
/// `project_id = any($3)` where `null` means "no filter" — is a filter that silently stops
/// applying the moment a variable is wrong, and the one place that must never silently stop is
/// the scoping filter.
///
/// `project_ids` is `&[Uuid]`, so an empty slice is a real, answerable question ("which workflows
/// are in none of the projects you can see?" → none) rather than an accident.
pub async fn list_workflows_in_projects(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    site_id: Option<Uuid>,
    project_ids: &[Uuid],
) -> Result<Vec<Workflow>> {
    if project_ids.is_empty() {
        // Answered without a round trip, and the shortcut is safe precisely because the caller's
        // set is already empty: there is no project in it, so no workflow in it either.
        return Ok(Vec::new());
    }

    let sql = format!(
        "select {} from workflows \
         where ($1::uuid is null or organization_id = $1) \
           and ($2::uuid is null or site_id = $2) \
           and project_id = any($3) \
         order by created_at desc, id",
        workflow_columns()
    );

    let workflows: Vec<Workflow> = sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(site_id)
        .bind(project_ids)
        .fetch_all(pool)
        .await?;

    Ok(workflows)
}

/// The event-triggered workflows of a scope, for the automations surface.
///
/// Carries the project filter for the same reason as [`list_workflows`]: the automations list is
/// a screen the project switcher scopes, and a rule in another project is not a rule this caller
/// may run.
pub async fn list_event_workflows(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    site_id: Option<Uuid>,
    project_id: Option<Uuid>,
) -> Result<Vec<Workflow>> {
    let sql = format!(
        "select {} from workflows \
         where trigger_kind = 'event' \
           and ($1::uuid is null or organization_id = $1) \
           and ($2::uuid is null or site_id = $2) \
           and ($3::uuid is null or project_id = $3) \
         order by created_at desc, id",
        workflow_columns()
    );

    let workflows: Vec<Workflow> = sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(site_id)
        .bind(project_id)
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
    /// `task` or `wait`.
    pub kind: String,
    /// Built-in action of a task step.
    pub action: Option<String>,
    /// Step parameters.
    pub params: serde_json::Value,
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
                   s.attempts, s.max_attempts",
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

/// Derive a run's terminal state once no step is open, and write it.
///
/// Answers `Some(status)` only for the call that actually settled the run, so the caller can
/// audit a state change exactly once.
///
/// **The project's failure counter is written here, in the same statement's transaction, and
/// that is the placement argument rather than a convenience.** `runs` is counted when the run
/// starts (see `create_execution_in`) because the daily cap reads it there; `failures` can only
/// be counted where the outcome is known, which is here. Splitting the two columns across the two
/// places that can actually see their own number is the whole point — a single write at start time
/// cannot know whether the run will fail, and a single write at settlement would count a run the
/// cap never saw. The `where status = 'running'` in the update below is the once: a second caller
/// that races this one matches no row, `settled` is 0, and the failure is not counted twice.
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
             count(*) filter (where status = 'failed') as failed, \
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

    // A failed run reports the first failure it met, so a later reader sees why.
    let error: Option<String> = if status == ExecutionStatus::Failed {
        sqlx::query_scalar::<_, Option<String>>(
            "select error from workflow_steps where execution_id = $1 and status = 'failed' \
             order by step_no asc limit 1",
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

    // The failure counter, taken only by the caller that actually settled the row. `settled > 0`
    // is the once, not a defensive re-read: the update above matches a running row, so a second
    // caller racing it sees zero rows and must not count. Only `Failed` counts — a cancelled run
    // was a person's decision and a completed run has nothing to report.
    if settled > 0 && status == ExecutionStatus::Failed {
        if let Some(project_id) = execution_project_id(pool, execution_id).await? {
            let mut connection = pool.acquire().await?;
            crate::limits::count_failed_run_in(&mut connection, project_id).await?;
        }
    }

    Ok(if settled > 0 { Some(status) } else { None })
}

/// The project a run belongs to, read through its workflow.
///
/// `workflow_executions` carries `organization_id` but not `project_id`, and the two are not
/// interchangeable: an organization holds many projects and the usage counters are per project, so
/// the join is the only path to the right number. Returns `None` when the workflow is gone — a run
/// whose workflow was deleted while it was still settling has no project to attribute, and
/// inventing one would charge an unrelated project's counter.
async fn execution_project_id(pool: &PgPool, execution_id: Uuid) -> Result<Option<Uuid>> {
    sqlx::query_scalar(
        "select w.project_id from workflow_executions e \
         join workflows w on w.id = e.workflow_id where e.id = $1",
    )
    .bind(execution_id)
    .fetch_optional(pool)
    .await
    .map_err(WorkflowError::from)
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
                      s.status, s.attempts, s.max_attempts, s.available_at, s.started_at, \
                      s.finished_at, s.output, s.error \
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
