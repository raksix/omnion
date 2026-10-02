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
use crate::definition::{MAX_ATTEMPTS, wait_seconds_from};
use crate::error::{Result, WorkflowError};
use crate::handler::{ActionContext, ActionHandler, NoActionHandler};
use crate::model::{ExecutionStatus, StepKind, TriggerKind, Workflow, WorkflowExecution};
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
    tick_with(pool, config, &NoActionHandler).await
}

/// Run one tick with the process's host action handler.
pub async fn tick_with(
    pool: &PgPool,
    config: &RunnerConfig,
    handler: &dyn ActionHandler,
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

        let outcome = advance_step(pool, config, handler, &claimed).await?;
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
            Some(ExecutionStatus::Running) | None => {}
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

    if waits_resolved > 0 || !reclaimed.is_empty() || !recovered.is_empty() {
        tracing::info!(
            waits_resolved,
            steps_reclaimed = reclaimed.len(),
            executions_settled = recovered.len(),
            "workflow sweep"
        );
    }

    Ok(SweepReport {
        waits_resolved,
        steps_reclaimed: reclaimed.len(),
        executions_settled: recovered.len(),
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
                handler.execute(&action, &claimed.params, &context).await
            } else {
                actions::run(&action, &claimed.params, claimed.attempts)
            };

            match outcome {
                Ok(output) => {
                    store::complete_step(pool, claimed.id, &output).await?;
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

                    store::fail_step(pool, claimed.id, &message).await?;
                    tracing::warn!(
                        step_id = %claimed.id,
                        step = %claimed.name,
                        attempts = claimed.attempts,
                        "a step ran out of attempts"
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
        ExecutionStatus::Running => return Ok(()),
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
