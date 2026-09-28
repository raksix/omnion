//! The engine: what one tick of the runner does, and what the sweeper repairs.
//!
//! The engine is a loop over the durable store, not a call stack: a tick starts the schedules
//! that came due, then claims and advances at most `batch` steps. A wait step is parked (a
//! write) and picked up again when its deadline passes; a failed step is re-queued one backoff
//! later until its attempts run out. Nothing sleeps, so an API restart loses no work
//! (docs/09-N8N-TEARDOWN.md §13 lesson 1).

use serde_json::json;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::actions;
use crate::approval;
use crate::branch;
use crate::definition::{MAX_ATTEMPTS, wait_seconds_from};
use crate::error::{Result, WorkflowError};
use crate::guard::{GuardStep, NoRunGuard, RunGuard};
use crate::handler::{ActionContext, ActionHandler, NoActionHandler};
use crate::model::{ExecutionStatus, OnError, StepKind, TriggerKind, Workflow, WorkflowExecution};
use crate::store::{self, ClaimedStep};

/// Knobs of the runner, filled from `OMNION_WORKFLOW_*` (see `omnion_core::config`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunnerConfig {
    /// Delay between two ticks.
    pub tick: Duration,
    /// Delay between two sweeps.
    pub sweep: Duration,
    /// Steps one tick is allowed to advance.
    pub batch: usize,
    /// Rows one sweep may touch per repair.
    pub sweep_batch: i64,
    /// Scheduled workflows one tick may start.
    pub scheduler_batch: i64,
    /// First retry backoff; doubles with every attempt.
    pub retry_base: Duration,
    /// Ceiling of the retry backoff.
    pub retry_max: Duration,
    /// How long a claimed step may stay open before the sweeper treats its runner as gone.
    pub lease: Duration,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            // Sub-second wakeups are the design goal (docs/09 §13 lesson 19); the sweep is the
            // safety net, not the mechanism.
            tick: Duration::seconds(1),
            sweep: Duration::seconds(30),
            batch: 16,
            sweep_batch: 100,
            scheduler_batch: 8,
            retry_base: Duration::seconds(5),
            retry_max: Duration::seconds(300),
            // Longer than any built-in action; a step the engine itself runs takes milliseconds.
            lease: Duration::seconds(300),
        }
    }
}

/// What one tick did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TickReport {
    /// Runs the scheduler started.
    pub started: usize,
    /// Schedules that could not be started (logged, never fatal for the tick).
    pub start_failures: usize,
    /// Steps the engine advanced.
    pub steps_run: usize,
    /// Wait steps that parked.
    pub waits_parked: usize,
    /// Failing steps that were re-queued for another attempt.
    pub retries: usize,
    /// Runs that finished successfully during this tick.
    pub completed: usize,
    /// Runs that ran out of attempts during this tick.
    pub failed: usize,
    /// Runs cancelled by a caller during this tick.
    pub cancelled: usize,
}

impl TickReport {
    /// `true` when the tick had nothing to do.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.started == 0 && self.steps_run == 0
    }
}

/// What one sweep repaired.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepReport {
    /// Parked waits that were resolved because their deadline passed.
    pub waits_resolved: usize,
    /// Steps whose runner disappeared and that were put back on the queue.
    pub steps_reclaimed: usize,
    /// Runs that were left open without any step to run and were settled.
    pub executions_settled: usize,
    /// Gates that expired without a decision and ended their run.
    pub approvals_expired: usize,
}

/// What running one claimed step did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct StepOutcome {
    waited: bool,
    retried: bool,
    settled: Option<ExecutionStatus>,
}

/// Run one tick: due schedules, then due steps.
///
/// The process installs no host action handler here: a definition that names a host action
/// fails its step with that reason. Use [`tick_with`] when the process can run them.
pub async fn tick(pool: &PgPool, config: &RunnerConfig) -> Result<TickReport> {
    tick_with(pool, config, &NoActionHandler, &NoRunGuard).await
}

/// Run one tick with the process's host action handler and its run guard.
///
/// The guard is a *second* `&dyn` rather than a second method on the handler because the
/// two answer different questions and a process legitimately has only one of them: a
/// content worker that runs synthetic steps needs no loop guard, and a process that runs
/// automations needs both. See [`crate::guard`] for where the call sits and why.
pub async fn tick_with(
    pool: &PgPool,
    config: &RunnerConfig,
    handler: &dyn ActionHandler,
    guard: &dyn RunGuard,
) -> Result<TickReport> {
    let mut report = TickReport::default();

    for workflow in store::claim_due_schedules(pool, config.scheduler_batch).await? {
        match start_run(pool, &workflow, TriggerKind::Schedule, None).await {
            Ok(execution) => {
                report.started += 1;
                tracing::info!(
                    workflow_id = %workflow.id,
                    execution_id = %execution.id,
                    "a scheduled workflow started"
                );
            }
            Err(err) => {
                // One broken definition must not stop the other schedules — it is logged and
                // counted, and the definition's next chance is its next due time.
                report.start_failures += 1;
                tracing::warn!(workflow_id = %workflow.id, error = %err, "a schedule could not start");
            }
        }
    }

    for _ in 0..config.batch {
        let Some(claimed) = store::claim_due_step(pool).await? else {
            break;
        };
        report.steps_run += 1;

        let outcome = advance_step(pool, config, handler, guard, &claimed).await?;
        if outcome.waited {
            report.waits_parked += 1;
        }
        if outcome.retried {
            report.retries += 1;
        }
        match outcome.settled {
            Some(ExecutionStatus::Completed) => report.completed += 1,
            Some(ExecutionStatus::Failed) => report.failed += 1,
            Some(ExecutionStatus::Cancelled) => report.cancelled += 1,
            // A run parked on a person is not settled by a step — the gate's own write
            // moves it back to `running` when somebody decides. Counting it here would
            // report a settled run for a run that is only waiting.
            Some(ExecutionStatus::Running) | Some(ExecutionStatus::AwaitingApproval) | None => {}
        }
    }

    Ok(report)
}

/// Run one sweep: resolve the waits whose deadline passed, take back the steps of a runner that
/// disappeared, and close the runs that were left open without a step to run.
pub async fn sweep(pool: &PgPool, config: &RunnerConfig) -> Result<SweepReport> {
    // A due wait is finished here, in one write: the run moves on without a second claim, and
    // a wait that already resumed (or was cancelled) is untouched.
    let mut touched: Vec<Uuid> = store::resolve_due_waits(pool, config.sweep_batch).await?;
    let waits_resolved = touched.len();

    // A gate nobody decided stops being a question after its deadline. Expiring it here is
    // what stops a run from sitting in `awaiting_approval` forever with nobody watching:
    // the gate is decided as a rejection (a *nobody answered* is a no), the run is reopened
    // so the engine's own resume claim can see that decision, and the audit trail says why
    // it ended.
    let expired = expire_stale_approvals(pool, config).await?;
    let approvals_expired = expired.len();
    touched.extend(expired.iter().copied());

    let reclaimed = reclaim_stale_steps(pool, config).await?;
    touched.extend(reclaimed.iter().copied());

    // Whatever this sweep closed (a resumed wait may have been the last step of its run).
    for execution_id in &touched {
        settle_if_open(pool, *execution_id).await?;
    }

    let recovered = store::reconcile_executions(pool, config.sweep_batch).await?;
    for execution_id in &recovered {
        audit_settlement(pool, *execution_id).await?;
    }

    if waits_resolved > 0 || approvals_expired > 0 || !reclaimed.is_empty() || !recovered.is_empty()
    {
        tracing::info!(
            waits_resolved,
            approvals_expired,
            steps_reclaimed = reclaimed.len(),
            executions_settled = recovered.len(),
            "workflow sweep"
        );
    }

    Ok(SweepReport {
        waits_resolved,
        steps_reclaimed: reclaimed.len(),
        executions_settled: recovered.len(),
        approvals_expired,
    })
}

/// Put the steps of a stopped runner back on the queue.
///
/// A claim is a lease, not a lock: when an instance stops mid-step, the row stays `running`
/// with the attempt already counted. The sweeper gives the work back — the next attempt for a
/// task step, another park for a wait that never parked, and a clean failure for a step whose
/// last attempt was lost, so the run always reaches a terminal state.
async fn reclaim_stale_steps(pool: &PgPool, config: &RunnerConfig) -> Result<Vec<Uuid>> {
    let lease_seconds = config.lease.whole_seconds().max(1);
    let stale = store::stale_steps(pool, lease_seconds, config.sweep_batch).await?;
    let mut touched = Vec::with_capacity(stale.len());

    for step in stale {
        tracing::warn!(
            step_id = %step.id,
            step = %step.name,
            attempts = step.attempts,
            "reclaiming a step whose runner stopped"
        );

        match step.kind() {
            Some(StepKind::Wait) => {
                if step.attempts > 1 {
                    // The wait had already parked once: this claim was its resume.
                    store::complete_step(
                        pool,
                        step.id,
                        &json!({ "waited": true, "resumed": true }),
                    )
                    .await?;
                } else if let Ok(seconds) = wait_seconds_from(&step.params) {
                    store::set_step_waiting(
                        pool,
                        step.id,
                        store::now() + Duration::seconds(seconds),
                    )
                    .await?;
                } else {
                    store::fail_step(
                        pool,
                        step.id,
                        "the wait step could not be resumed after a restart",
                    )
                    .await?;
                }
            }
            Some(StepKind::Task) => {
                if step.attempts < step.max_attempts.min(MAX_ATTEMPTS) {
                    store::retry_step(
                        pool,
                        step.id,
                        "the previous attempt was lost when its runner stopped",
                        store::now(),
                    )
                    .await?;
                } else {
                    store::fail_step(
                        pool,
                        step.id,
                        "the last attempt was lost when its runner stopped",
                    )
                    .await?;
                }
            }
            Some(StepKind::Branch) | Some(StepKind::Stop) => {
                // A control step the dead runner claimed never wrote anything, so it goes
                // straight back to pending: a comparison is not "attempt 2 of 2", and a
                // decision is not re-made.
                store::requeue_step(pool, step.id).await?;
            }
            Some(StepKind::Approval) => {
                // A gate whose runner died between the claim and the write has no approval
                // row yet, so the step simply parks again on its first claim. A gate whose
                // row *is* there is already waiting for a person and the run is parked with
                // it — requeueing the step would let the engine claim a run a decision has
                // not been asked about, so it goes back to `waiting` and the run's own
                // status (which the claim query reads) keeps the two in step.
                match step.approval_id {
                    None => store::requeue_step(pool, step.id).await?,
                    Some(approval_id) => {
                        let decided = approval::decision_of(pool, approval_id).await?;
                        match decided {
                            Some(_) => store::requeue_step(pool, step.id).await?,
                            None => {
                                // Park until the gate's own deadline: the run status may have
                                // been lost to the crash, and the deadline is the same one
                                // the sweeper expires on, so this cannot wait forever.
                                let seconds = step
                                    .params
                                    .get("expires_in_hours")
                                    .and_then(serde_json::Value::as_i64)
                                    .unwrap_or(i64::from(approval::DEFAULT_TTL_HOURS));
                                store::set_step_waiting(
                                    pool,
                                    step.id,
                                    store::now() + Duration::hours(seconds),
                                )
                                .await?;
                            }
                        }
                    }
                }
            }
            None => {
                store::fail_step(pool, step.id, "the step carries an unknown kind").await?;
            }
        }

        touched.push(step.execution_id);
    }

    Ok(touched)
}

/// Settle one run when nothing is left to do, and audit it.
async fn settle_if_open(pool: &PgPool, execution_id: Uuid) -> Result<()> {
    if store::settle_execution(pool, execution_id).await?.is_some() {
        audit_settlement(pool, execution_id).await?;
    }
    Ok(())
}

/// Write the audit row of a run that already reached a terminal state.
async fn audit_settlement(pool: &PgPool, execution_id: Uuid) -> Result<()> {
    if let Some(execution) = store::find_execution(pool, execution_id).await? {
        if let Some(status) = execution.status() {
            if status.is_terminal() {
                record_settlement(pool, &execution, status).await?;
            }
        }
    }
    Ok(())
}

/// Start a run of one workflow and audit it.
pub async fn start_run(
    pool: &PgPool,
    workflow: &Workflow,
    trigger: TriggerKind,
    triggered_by: Option<Uuid>,
) -> Result<WorkflowExecution> {
    let definitions = workflow.definitions()?;
    start_run_with(pool, workflow, trigger, triggered_by, &definitions).await
}

/// Start a run of one workflow from steps the caller already holds, and audit it.
///
/// An event trigger uses this: the automation layer resolves the definition's bindings against
/// the event's payload first, so the run's steps carry the values of *that* event (a retry of a
/// step then repeats exactly what the first attempt did).
pub async fn start_run_with(
    pool: &PgPool,
    workflow: &Workflow,
    trigger: TriggerKind,
    triggered_by: Option<Uuid>,
    steps: &[crate::definition::StepDefinition],
) -> Result<WorkflowExecution> {
    let (execution, _steps) =
        store::create_execution(pool, workflow, trigger, triggered_by, steps).await?;

    record_start(pool, workflow, &execution, trigger, steps.len()).await?;
    Ok(execution)
}

/// Decide the gates whose deadline passed without a decision, and hand their runs back to
/// the engine so the resume claim can see the decision.
///
/// Expiry is a *rejection* rather than a third state on purpose: a gate nobody answered is
/// a no, and a no is a state the engine already knows how to end a run on. A third state
/// would need a third ending, and the run-history UI would need a third colour, for what is
/// the same fact — "it did not go ahead".
///
/// The write is guarded on `decision is null`, so a person pressing *Reject* in the same
/// millisecond wins exactly one of the two, and the loser sees the row already decided
/// rather than overwriting a human's decision with a clock's.
async fn expire_stale_approvals(pool: &PgPool, config: &RunnerConfig) -> Result<Vec<Uuid>> {
    let expired = approval::expired_approvals(pool, config.sweep_batch).await?;
    let mut ended = Vec::with_capacity(expired.len());

    for approval_id in expired {
        if !approval::expire_approval(pool, approval_id).await? {
            // Somebody decided it between the read and the write.
            continue;
        }

        // The row knows its run and its step, so the run is reopened and the step re-queued
        // for the engine's second claim — the same path an approval takes, which is the
        // point: an expired gate and a rejected one reach the run's end by the same write.
        let gate: Option<(Uuid, Uuid, Uuid)> = sqlx::query_as(
            "select execution_id, step_id, organization_id from workflow_approvals where id = $1",
        )
        .bind(approval_id)
        .fetch_optional(pool)
        .await?;
        let Some((execution_id, step_id, organization_id)) = gate else {
            continue;
        };

        approval::resume_after_decision(pool, execution_id, step_id).await?;

        let entry = omnion_audit::NewAuditEntry::system("workflow.approval.expired")
            .organization(organization_id)
            .target("workflow_approval", approval_id.to_string())
            .metadata(json!({
                "execution_id": execution_id,
                "step_id": step_id,
                "reason": "the approval expired before anybody decided it",
            }));
        if let Err(err) = omnion_audit::record(pool, entry).await {
            // A gap in the trail is worth a log line, never a failed sweep: the *decision*
            // already happened, and undoing it because the audit row could not be written
            // would be a worse answer than a missing one.
            tracing::warn!(approval_id = %approval_id, error = %err, "an expired approval's audit row could not be written");
        }

        tracing::info!(
            approval_id = %approval_id,
            execution_id = %execution_id,
            "an approval expired and its run was handed back to the engine"
        );
        ended.push(execution_id);
    }

    Ok(ended)
}

/// Write the audit row of a gate that has just been opened.
///
/// The **token is deliberately absent** from the metadata: an audit row is read by far more
/// people than a decision credential is meant for, and the same rule the inbound hook token
/// follows. What the row carries instead is everything an operator needs to find the gate —
/// the run, the step, the permission that may open it, and when it stops waiting.
#[allow(clippy::too_many_arguments)]
async fn record_approval_requested(
    pool: &PgPool,
    execution: &WorkflowExecution,
    step_no: i32,
    issued: &approval::IssuedToken,
    params: &approval::ApprovalParams,
) -> Result<()> {
    let entry = omnion_audit::NewAuditEntry::system(approval::REQUESTED_EVENT)
        .organization(execution.organization_id)
        .target("workflow_execution", execution.id.to_string())
        .metadata(json!({
            "workflow_id": execution.workflow_id,
            "step_no": step_no,
            "permission": params.permission,
            "message": params.message,
            "expires_at": approval::approval_deadline(params, store::now()),
            "token_hash": issued.hash,
        }));

    omnion_audit::record(pool, entry).await?;
    Ok(())
}

/// Write the audit row of a run that has just been created.
///
/// Public because a run can be materialised inside a caller's own transaction (the automation
/// layer starts a run in the same transaction that advances its event cursor). The audit is the
/// second write after that commit — a run whose audit row is missing is a gap in the trail, not
/// a run that did not happen.
pub async fn record_start(
    pool: &PgPool,
    workflow: &Workflow,
    execution: &WorkflowExecution,
    trigger: TriggerKind,
    steps: usize,
) -> Result<()> {
    let entry = omnion_audit::NewAuditEntry::system("workflow.execution.started")
        .organization(workflow.organization_id)
        .target("workflow_execution", execution.id.to_string())
        .metadata(json!({
            "workflow_id": workflow.id,
            "trigger": trigger.as_str(),
            "steps": steps,
            "triggered_by": execution.triggered_by,
        }));

    omnion_audit::record(pool, entry).await?;
    Ok(())
}

/// Advance one claimed step by one attempt.
async fn advance_step(
    pool: &PgPool,
    config: &RunnerConfig,
    handler: &dyn ActionHandler,
    guard: &dyn RunGuard,
    claimed: &ClaimedStep,
) -> Result<StepOutcome> {
    let kind = StepKind::parse(&claimed.kind).ok_or_else(|| {
        WorkflowError::invalid(
            "workflow_store_error",
            format!(
                "step {} has the unknown kind {:?}",
                claimed.id, claimed.kind
            ),
        )
    })?;

    match kind {
        StepKind::Wait => {
            // A wait step is claimed twice: the first claim parks it, the second (once its
            // deadline passed) resumes it. The claim count is the whole state, so the sweep
            // that re-queues a due wait cannot turn a parked step into a parked-again one.
            if claimed.attempts > 1 {
                store::complete_step(
                    pool,
                    claimed.id,
                    &json!({ "waited": true, "resumed": true }),
                )
                .await?;
                return Ok(StepOutcome {
                    settled: settle_after_step(pool, claimed).await?,
                    ..StepOutcome::default()
                });
            }

            let seconds = wait_seconds_from(&claimed.params)?;
            store::set_step_waiting(pool, claimed.id, store::now() + Duration::seconds(seconds))
                .await?;
            tracing::debug!(
                step_id = %claimed.id,
                seconds,
                "a wait step parked the run"
            );
            // The run may have been cancelled while the wait was being parked.
            settle_after_step(pool, claimed).await?;
            Ok(StepOutcome {
                waited: true,
                ..StepOutcome::default()
            })
        }
        StepKind::Branch => {
            // A branch is a write like a wait: it succeeds and the run goes on, or it
            // succeeds and the run *ends* — either way the step itself succeeded, because a
            // branch that stopped the run is the branch working, not the branch failing.
            let scope = branch_scope(pool, claimed.execution_id).await?;
            let outcome = branch::evaluate(&claimed.params, &scope);

            match outcome {
                Ok(true) => {
                    let field = claimed
                        .params
                        .get("field")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    store::complete_step(
                        pool,
                        claimed.id,
                        &json!({ "branch": { "field": field, "holds": true } }),
                    )
                    .await?;
                    tracing::debug!(step_id = %claimed.id, field, "a branch let the run go on");
                    Ok(StepOutcome {
                        settled: settle_after_step(pool, claimed).await?,
                        ..StepOutcome::default()
                    })
                }
                Ok(false) => {
                    // The comparison did not hold: the run ends *here*, and every step after
                    // it is closed as cancelled so the trace shows they were never reached.
                    let field = claimed
                        .params
                        .get("field")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    store::complete_step(
                        pool,
                        claimed.id,
                        &json!({ "branch": { "field": field, "holds": false } }),
                    )
                    .await?;
                    store::end_run_after_branch(pool, claimed.execution_id, claimed.id).await?;
                    store::settle_execution_as(pool, claimed.execution_id, "completed").await?;
                    tracing::info!(
                        step_id = %claimed.id,
                        field,
                        "a branch ended the run before the steps after it"
                    );
                    Ok(StepOutcome {
                        settled: Some(ExecutionStatus::Completed),
                        ..StepOutcome::default()
                    })
                }
                // A field nothing produced: a broken definition, not a branch that decided.
                Err(message) => {
                    store::fail_step(pool, claimed.id, &message).await?;
                    let settled = settle_after_step(pool, claimed).await?;
                    Ok(StepOutcome {
                        settled,
                        ..StepOutcome::default()
                    })
                }
            }
        }
        StepKind::Stop => {
            // A stop is a decision, not a failure: the run completes, the steps after it are
            // closed, and the trace says why. That is the difference from a step that ran out
            // of attempts, which is what an operator has to go and fix.
            let reason = claimed
                .params
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("the run was stopped here")
                .to_owned();

            store::complete_step(
                pool,
                claimed.id,
                &json!({ "stopped": true, "reason": reason }),
            )
            .await?;
            store::end_run_after_branch(pool, claimed.execution_id, claimed.id).await?;
            store::settle_execution_as(pool, claimed.execution_id, "completed").await?;
            tracing::info!(
                step_id = %claimed.id,
                step = %claimed.name,
                reason,
                "a stop step ended the run"
            );
            Ok(StepOutcome {
                settled: Some(ExecutionStatus::Completed),
                ..StepOutcome::default()
            })
        }
        StepKind::Approval => {
            // A gate is claimed twice, exactly like a wait: the first claim writes the
            // approval row and parks the run, the second is the *resume* the decision hands
            // back. A parked run is not claimable, so the second claim can only happen after
            // somebody's decision has reopened it — which is the whole protection, and it is
            // the reason `is_claimable` is a separate question from "is the run open".
            if claimed.attempts > 1 {
                // The gate is resumed by a decision, not by the clock — so the first thing
                // the resume checks is whether the run was let go *at all*. A claim that
                // reaches here with the gate still undecided is not a race to win: it parks
                // itself again, which is the one behaviour that keeps a half-resumed run
                // from marching past a gate nobody opened.
                let decided = match claimed.approval_id {
                    Some(approval_id) => approval::decision_of(pool, approval_id).await?,
                    None => None,
                };

                let Some(decision) = decided else {
                    // Park it again, until the gate's own deadline. `set_step_waiting`
                    // keeps the attempt count, so the resume claim is still there when the
                    // decision lands, and it does not spin: `available_at` is the deadline,
                    // not now.
                    let deadline = store::now()
                        + Duration::hours(i64::from(
                            approval::params_from(&claimed.params)?.expires_in_hours,
                        ));
                    store::set_step_waiting(pool, claimed.id, deadline).await?;
                    tracing::warn!(
                        step_id = %claimed.id,
                        "a gate was claimed for a resume with no decision behind it; parked again"
                    );
                    return Ok(StepOutcome {
                        waited: true,
                        ..StepOutcome::default()
                    });
                };

                match decision {
                    approval::Decision::Approved => {
                        // Approved: the gate *succeeds* and the run carries on. The output
                        // names who decided, because "who let this through" is the question
                        // an audit asks of every gated effect.
                        let decided_by: Option<Uuid> = sqlx::query_scalar(
                            "select decided_by from workflow_approvals where id = $1",
                        )
                        .bind(claimed.approval_id)
                        .fetch_one(pool)
                        .await?;
                        store::complete_step(
                            pool,
                            claimed.id,
                            &json!({
                                "approved": true,
                                "approval_id": claimed.approval_id,
                                "decided_by": decided_by,
                            }),
                        )
                        .await?;
                        tracing::info!(step_id = %claimed.id, "an approval resumed the run");
                        return Ok(StepOutcome {
                            settled: settle_after_step(pool, claimed).await?,
                            ..StepOutcome::default()
                        });
                    }
                    approval::Decision::Rejected => {
                        // Rejected: the run ends *here* and the steps after it are closed,
                        // exactly as a branch closes them. A person decided this, so the
                        // step is cancelled rather than failed and the run settles as
                        // cancelled: nothing in the rule went wrong, it was told not to.
                        let note = claimed
                            .params
                            .get("message")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("an approver rejected this step")
                            .to_owned();
                        approval::end_at_rejection(
                            pool,
                            claimed.execution_id,
                            claimed.id,
                            &format!("rejected by an approver: {note}"),
                        )
                        .await?;
                        tracing::info!(
                            step_id = %claimed.id,
                            "an approval was rejected and the run ended there"
                        );
                        return Ok(StepOutcome {
                            settled: Some(ExecutionStatus::Cancelled),
                            ..StepOutcome::default()
                        });
                    }
                }
            }

            // The first claim: write the gate, park the step, park the run. Three writes in
            // one transaction (see `approval::open`) so a gate row, a parked step and a
            // parked run either all exist or none do.
            let Some(execution) = store::find_execution(pool, claimed.execution_id).await? else {
                return Ok(StepOutcome::default());
            };
            let params = approval::params_from(&claimed.params)?;
            let issued = approval::open(
                pool,
                &execution,
                claimed.id,
                claimed.step_no,
                &params,
                store::now(),
            )
            .await?;

            record_approval_requested(pool, &execution, claimed.step_no, &issued, &params).await?;

            Ok(StepOutcome {
                waited: true,
                ..StepOutcome::default()
            })
        }
        StepKind::Task => {
            let action = claimed.action.clone().unwrap_or_default();
            let outcome = if actions::is_host_action(&action) {
                // The action touches the world: the process's handler runs it. The context is
                // the run's own rows, so an action that writes (a comment) lands on the right
                // tenant without the definition carrying it.
                let execution = store::find_execution(pool, claimed.execution_id).await?;
                let context = ActionContext {
                    pool,
                    organization_id: execution
                        .as_ref()
                        .map(|row| row.organization_id)
                        .unwrap_or_default(),
                    site_id: claim_site(pool, claimed.execution_id).await?,
                    execution_id: claimed.execution_id,
                    step_id: claimed.id,
                    step_no: claimed.step_no,
                    attempt: claimed.attempts,
                };
                // A step's `timeout_ms` is a *budget on this attempt*, not a deadline the
                // engine schedules around: the runner does not sleep, so the only way to
                // honour it is to stop waiting for the future and record the limit. The
                // attempt's side effects are the action's own idempotency contract's problem,
                // which is why every outbound call carries the run id as its key.
                let budget = std::time::Duration::from_millis(u64::from(
                    claimed.timeout_ms.clamp(1, 120_000) as u32,
                ));
                match tokio::time::timeout(
                    budget,
                    handler.execute(&action, &claimed.params, &context),
                )
                .await
                {
                    Ok(outcome) => outcome,
                    Err(_) => Err(format!(
                        "the step did not answer within {} ms; the action was abandoned",
                        claimed.timeout_ms
                    )),
                }
            } else {
                actions::run(&action, &claimed.params, claimed.attempts)
            };

            match outcome {
                Ok(output) => {
                    store::complete_step(pool, claimed.id, &output).await?;

                    // The one place a run guard is consulted: the step has succeeded and
                    // the run is not settled yet. A verdict of "stop" ends the run *here*,
                    // the same write the `stop` step and a failed step do, so a guard that
                    // fires cannot leave a claimable step behind it.
                    let verdict = crate::guard::check_run(
                        guard,
                        GuardStep {
                            execution_id: claimed.execution_id,
                            step_no: claimed.step_no,
                            kind: claimed.kind.as_str(),
                            action: claimed.action.as_deref(),
                            params: &claimed.params,
                        },
                    )
                    .await;

                    if verdict.stop {
                        let reason = verdict.reason.unwrap_or_else(|| {
                            "a run guard stopped this run after the step".to_owned()
                        });
                        store::fail_step(pool, claimed.id, &reason).await?;
                        store::end_run_after_branch(pool, claimed.execution_id, claimed.id).await?;
                        tracing::warn!(
                            step_id = %claimed.id,
                            step = %claimed.name,
                            step_no = claimed.step_no,
                            "a run guard stopped the run after the step succeeded"
                        );
                        return Ok(StepOutcome {
                            settled: settle_after_step(pool, claimed).await?,
                            ..StepOutcome::default()
                        });
                    }

                    Ok(StepOutcome {
                        settled: settle_after_step(pool, claimed).await?,
                        ..StepOutcome::default()
                    })
                }
                Err(message) => {
                    let attempts_allowed = claimed.max_attempts.min(MAX_ATTEMPTS);
                    if claimed.attempts < attempts_allowed {
                        let delay = store::retry_delay(
                            claimed.attempts,
                            config.retry_base,
                            config.retry_max,
                        );
                        store::retry_step(pool, claimed.id, &message, store::now() + delay).await?;
                        tracing::info!(
                            step_id = %claimed.id,
                            attempt = claimed.attempts,
                            of = attempts_allowed,
                            delay_ms = delay.whole_milliseconds() as i64,
                            "a failing step was re-queued"
                        );
                        return Ok(StepOutcome {
                            retried: true,
                            ..StepOutcome::default()
                        });
                    }

                    // Out of attempts. The per-step policy decides whether the run outlives
                    // it, and this is where the request's "routes to a failure branch" lives
                    // in v0 shape: `continue` records the failure and lets the rest run, and
                    // `stop` (or the rule's own policy) ends the run as a failure.
                    let policy =
                        effective_on_error(&claimed.on_error, pool, claimed.execution_id).await?;
                    if policy == OnError::Continue {
                        store::fail_step_ignored(pool, claimed.id, &message).await?;
                        tracing::warn!(
                            step_id = %claimed.id,
                            step = %claimed.name,
                            error = %message,
                            "a step failed and the run was told to continue past it"
                        );
                        return Ok(StepOutcome {
                            settled: settle_after_step(pool, claimed).await?,
                            ..StepOutcome::default()
                        });
                    }

                    store::fail_step(pool, claimed.id, &message).await?;
                    // A run that stops on a failure stops *there*: the steps after it are
                    // closed as cancelled, exactly as a branch closes them. Without this the
                    // run is not "stopped" at all — the next tick claims step N+1 and the
                    // failure only shows up in the summary, which is the one thing a `stop`
                    // policy is supposed to prevent.
                    store::end_run_after_branch(pool, claimed.execution_id, claimed.id).await?;
                    tracing::warn!(
                        step_id = %claimed.id,
                        step = %claimed.name,
                        attempts = claimed.attempts,
                        "a step ran out of attempts and the run stopped there"
                    );
                    Ok(StepOutcome {
                        settled: settle_after_step(pool, claimed).await?,
                        ..StepOutcome::default()
                    })
                }
            }
        }
    }
}

/// The object a branch reads: the run's event payload and every finished step's output.
///
/// Built per branch, not cached, because a run has at most 50 steps and a branch that reads
/// a stale output is worse than one that reads a fresh query.
async fn branch_scope(pool: &PgPool, execution_id: Uuid) -> Result<serde_json::Value> {
    // `coalesce` because sqlx decodes a *column* into `Value`, and a SQL NULL is not JSON
    // null: without it a run with no payload — a manual run, a schedule — fails the branch
    // with a decode error instead of evaluating the comparison.
    let event: Option<serde_json::Value> = sqlx::query_scalar(
        "select coalesce(event_payload, 'null'::jsonb) from workflow_executions where id = $1",
    )
    .bind(execution_id)
    .fetch_optional(pool)
    .await?;

    let rows: Vec<(i32, Option<serde_json::Value>)> = sqlx::query_as(
        "select step_no, output from workflow_steps \
         where execution_id = $1 and output is not null order by step_no",
    )
    .bind(execution_id)
    .fetch_all(pool)
    .await?;

    let mut steps = serde_json::Map::new();
    for (step_no, output) in rows {
        steps.insert(
            step_no.to_string(),
            output.unwrap_or(serde_json::Value::Null),
        );
    }

    Ok(json!({
        "event": event.unwrap_or(serde_json::Value::Null),
        "steps": serde_json::Value::Object(steps),
    }))
}

/// The error policy that applies to a step: its own, or the rule's when it inherits.
///
/// The rule's policy is the workflow's own setting; the automation layer has not yet added
/// a per-rule one (slice 4), so `inherit` means what it meant before the policy existed —
/// stop. Reading it here rather than at write time means changing the rule changes the runs
/// that have not reached the step yet.
async fn effective_on_error(stored: &str, pool: &PgPool, execution_id: Uuid) -> Result<OnError> {
    let own = OnError::parse(stored).ok_or_else(|| {
        WorkflowError::invalid(
            "workflow_store_error",
            format!("step carries the unknown error policy {stored:?}"),
        )
    })?;

    if own != OnError::Inherit {
        return Ok(own);
    }

    let rule_policy: Option<String> = sqlx::query_scalar(
        "select w.on_error from workflows w \
         join workflow_executions e on e.workflow_id = w.id where e.id = $1",
    )
    .bind(execution_id)
    .fetch_optional(pool)
    .await?;

    Ok(rule_policy
        .as_deref()
        .and_then(OnError::parse)
        .unwrap_or(OnError::Stop))
}

/// The site of the workflow behind an execution, for the action context.
async fn claim_site(pool: &PgPool, execution_id: Uuid) -> Result<Option<Uuid>> {
    let site: Option<Option<Uuid>> = sqlx::query_scalar(
        "select w.site_id from workflow_executions e \
         join workflows w on w.id = e.workflow_id where e.id = $1",
    )
    .bind(execution_id)
    .fetch_optional(pool)
    .await?;

    Ok(site.flatten())
}

/// Close a run when the step that just finished was its last one.
///
/// Also handles the race the cancellation flag exists for: a run cancelled while its step was
/// running settles as cancelled, and the claimed step is closed with it.
async fn settle_after_step(
    pool: &PgPool,
    claimed: &ClaimedStep,
) -> Result<Option<ExecutionStatus>> {
    let Some(execution) = store::find_execution(pool, claimed.execution_id).await? else {
        return Ok(None);
    };

    match execution.status() {
        None => Err(WorkflowError::invalid(
            "workflow_store_error",
            format!(
                "execution {} has the unknown status {:?}",
                execution.id, execution.status
            ),
        )),
        Some(ExecutionStatus::Running) => {
            let settled = store::settle_execution(pool, claimed.execution_id).await?;
            if let Some(status) = settled {
                record_settlement(pool, &execution, status).await?;
                return Ok(Some(status));
            }
            Ok(None)
        }
        // Cancelled (or otherwise terminal) while the step ran: close the step with the run.
        Some(_terminal) => {
            store::cancel_step(pool, claimed.id).await?;
            Ok(None)
        }
    }
}

/// Write the audit row of a run that just reached a terminal state.
async fn record_settlement(
    pool: &PgPool,
    execution: &WorkflowExecution,
    status: ExecutionStatus,
) -> Result<()> {
    let action = match status {
        ExecutionStatus::Completed => "workflow.execution.completed",
        ExecutionStatus::Failed => "workflow.execution.failed",
        ExecutionStatus::Cancelled => "workflow.execution.cancelled",
        // Not settled: a running run has not finished, and a run parked on a person is the
        // definition of not finished. Neither is a settlement to audit.
        ExecutionStatus::Running | ExecutionStatus::AwaitingApproval => return Ok(()),
    };

    // The settled row carries the failure message the steps left behind.
    let settled = store::find_execution(pool, execution.id).await?;
    let error = settled.as_ref().and_then(|row| row.error.clone());

    let entry = omnion_audit::NewAuditEntry::system(action)
        .organization(execution.organization_id)
        .target("workflow_execution", execution.id.to_string())
        .metadata(json!({
            "workflow_id": execution.workflow_id,
            "trigger": execution.trigger_kind,
            "status": status.as_str(),
            "error": error,
        }));

    omnion_audit::record(pool, entry).await?;
    Ok(())
}

/// The instant a parked wait becomes due, for callers that need to predict it.
#[must_use]
pub fn wait_deadline(seconds: i64) -> OffsetDateTime {
    store::now() + Duration::seconds(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_runner_ticks_fast_and_sweeps_on_a_cadence() {
        let config = RunnerConfig::default();
        assert_eq!(config.tick, Duration::seconds(1));
        assert_eq!(config.sweep, Duration::seconds(30));
        assert!(config.batch > 0 && config.sweep_batch > 0);
        assert!(
            config.retry_base < config.retry_max,
            "the first backoff is shorter than the ceiling"
        );
    }

    #[test]
    fn an_idle_tick_reports_nothing() {
        assert!(TickReport::default().is_idle());
        let busy = TickReport {
            steps_run: 1,
            ..TickReport::default()
        };
        assert!(!busy.is_idle());
    }

    #[test]
    fn a_wait_deadline_is_in_the_future() {
        let deadline = wait_deadline(5);
        assert!(deadline > store::now() - Duration::seconds(1));
    }
}
