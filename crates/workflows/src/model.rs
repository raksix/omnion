//! The durable model of the engine: definitions, executions and steps.
//!
//! `workflows` holds a definition, `workflow_executions` one run of it and `workflow_steps`
//! the materialised steps of that run — the durable step store of docs/09-N8N-TEARDOWN.md §13
//! lesson 1 ("durable steps from day one"): a run is a row per step, not a call stack, so it
//! survives a restart and every retry/wait is a write, not a sleep.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// How a run was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    /// A person pressed "run" (or the API did it for them).
    Manual,
    /// The engine's scheduler reached the workflow's next due time.
    Schedule,
}

impl TriggerKind {
    /// Canonical lowercase name stored in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Schedule => "schedule",
        }
    }

    /// Parse a stored value.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "manual" => Some(Self::Manual),
            "schedule" => Some(Self::Schedule),
            _ => None,
        }
    }
}

/// What a step does when its turn comes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// Runs a built-in action (see [`crate::actions`]).
    Task,
    /// Pauses the run for a fixed number of seconds; the engine resumes it later.
    Wait,
}

impl StepKind {
    /// Canonical lowercase name stored in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Wait => "wait",
        }
    }

    /// Parse a stored value.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "task" => Some(Self::Task),
            "wait" => Some(Self::Wait),
            _ => None,
        }
    }
}

/// Lifecycle of one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionStatus {
    /// Steps are still being worked through.
    Running,
    /// Every step succeeded.
    Completed,
    /// A step ran out of attempts.
    Failed,
    /// A caller cancelled the run; no further step starts.
    Cancelled,
}

impl ExecutionStatus {
    /// Canonical lowercase name stored in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parse a stored value.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// `true` when no further work can happen.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// Lifecycle of one step of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    /// Waiting for its turn (or for a retry's backoff to pass).
    Pending,
    /// Claimed by the engine right now.
    Running,
    /// A wait step that is parked until `available_at`.
    Waiting,
    /// Finished successfully.
    Succeeded,
    /// Ran out of attempts.
    Failed,
    /// The run was cancelled before (or while) this step ran.
    Cancelled,
}

impl StepStatus {
    /// Canonical lowercase name stored in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parse a stored value.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "waiting" => Some(Self::Waiting),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// `true` while the step still needs the engine's attention.
    #[must_use]
    pub const fn is_open(self) -> bool {
        matches!(self, Self::Pending | Self::Running | Self::Waiting)
    }
}

/// A stored workflow definition.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Workflow {
    /// Workflow id.
    pub id: Uuid,
    /// Organization that owns the workflow.
    pub organization_id: Uuid,
    /// Site the workflow belongs to, when it is site-scoped.
    pub site_id: Option<Uuid>,
    /// Display name.
    pub name: String,
    /// Free-form description.
    pub description: String,
    /// A disabled workflow never starts from its schedule (manual runs stay allowed).
    pub enabled: bool,
    /// `manual` or `schedule`.
    pub trigger_kind: String,
    /// Cron expression when the trigger is a schedule.
    pub schedule: Option<String>,
    /// Next time the scheduler should start this workflow.
    pub next_run_at: Option<OffsetDateTime>,
    /// Ordered step definitions, as stored JSON.
    pub steps: serde_json::Value,
    /// Account that created the workflow.
    pub created_by: Option<Uuid>,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last definition change.
    pub updated_at: OffsetDateTime,
}

impl Workflow {
    /// The parsed trigger kind (falls back to `manual` for a value the schema rejects anyway).
    #[must_use]
    pub fn trigger(&self) -> TriggerKind {
        TriggerKind::parse(&self.trigger_kind).unwrap_or(TriggerKind::Manual)
    }

    /// The step definitions this workflow declares.
    pub fn definitions(&self) -> crate::Result<Vec<crate::definition::StepDefinition>> {
        serde_json::from_value(self.steps.clone()).map_err(|err| {
            crate::error::WorkflowError::invalid(
                "workflow_definition_unreadable",
                format!("the stored steps are not readable: {err}"),
            )
        })
    }
}

/// Columns of `workflows`, in the order [`Workflow`] expects.
pub const WORKFLOW_COLUMNS: &str = "id, organization_id, site_id, name, description, enabled, \
     trigger_kind, schedule, next_run_at, steps, created_by, created_at, updated_at";

/// A definition row to be written.
#[derive(Debug, Clone)]
pub struct NewWorkflow {
    /// Organization that owns the workflow.
    pub organization_id: Uuid,
    /// Optional site scope.
    pub site_id: Option<Uuid>,
    /// Display name.
    pub name: String,
    /// Free-form description.
    pub description: String,
    /// Whether the schedule is armed.
    pub enabled: bool,
    /// Trigger kind.
    pub trigger: TriggerKind,
    /// Cron expression when scheduled.
    pub schedule: Option<String>,
    /// First due time when scheduled.
    pub next_run_at: Option<OffsetDateTime>,
    /// Step definitions as stored JSON.
    pub steps: serde_json::Value,
    /// Creating account.
    pub created_by: Option<Uuid>,
}

/// A stored run of a workflow.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct WorkflowExecution {
    /// Execution id.
    pub id: Uuid,
    /// Workflow that was run.
    pub workflow_id: Uuid,
    /// Organization of the workflow (kept on the run so audits need no join).
    pub organization_id: Uuid,
    /// `running`, `completed`, `failed` or `cancelled`.
    pub status: String,
    /// `manual` or `schedule`.
    pub trigger_kind: String,
    /// Account that started a manual run.
    pub triggered_by: Option<Uuid>,
    /// When the run started.
    pub started_at: OffsetDateTime,
    /// When the run reached a terminal state.
    pub finished_at: Option<OffsetDateTime>,
    /// Error of the failing step, when the run failed.
    pub error: Option<String>,
}

impl WorkflowExecution {
    /// The parsed status.
    #[must_use]
    pub fn status(&self) -> Option<ExecutionStatus> {
        ExecutionStatus::parse(&self.status)
    }

    /// `true` when the run can no longer change.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.status().is_some_and(ExecutionStatus::is_terminal)
    }
}

/// Columns of `workflow_executions`, in the order [`WorkflowExecution`] expects.
pub const EXECUTION_COLUMNS: &str = "id, workflow_id, organization_id, status, trigger_kind, \
     triggered_by, started_at, finished_at, error";

/// One materialised step of a run.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct WorkflowStep {
    /// Step id.
    pub id: Uuid,
    /// Run the step belongs to.
    pub execution_id: Uuid,
    /// 1-based position in the run.
    pub step_no: i32,
    /// Step name from the definition.
    pub name: String,
    /// `task` or `wait`.
    pub kind: String,
    /// Built-in action of a task step.
    pub action: Option<String>,
    /// Step parameters, as stored JSON.
    pub params: serde_json::Value,
    /// `pending`, `running`, `waiting`, `succeeded`, `failed` or `cancelled`.
    pub status: String,
    /// Attempts made so far (the first run counts as one).
    pub attempts: i32,
    /// Attempts allowed in total (1–5).
    pub max_attempts: i32,
    /// When the step may run; carries the retry backoff and the wait deadline.
    pub available_at: OffsetDateTime,
    /// When the current attempt started.
    pub started_at: Option<OffsetDateTime>,
    /// When the step reached a terminal state.
    pub finished_at: Option<OffsetDateTime>,
    /// Result of a successful step.
    pub output: Option<serde_json::Value>,
    /// Message of the last failure.
    pub error: Option<String>,
}

impl WorkflowStep {
    /// The parsed status.
    #[must_use]
    pub fn status(&self) -> Option<StepStatus> {
        StepStatus::parse(&self.status)
    }

    /// The parsed kind.
    #[must_use]
    pub fn kind(&self) -> Option<StepKind> {
        StepKind::parse(&self.kind)
    }
}

/// Columns of `workflow_steps`, in the order [`WorkflowStep`] expects.
pub const STEP_COLUMNS: &str = "id, execution_id, step_no, name, kind, action, params, status, \
     attempts, max_attempts, available_at, started_at, finished_at, output, error";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_trigger_kinds_round_trip() {
        for kind in [TriggerKind::Manual, TriggerKind::Schedule] {
            assert_eq!(TriggerKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(TriggerKind::parse("cron"), None);
    }

    #[test]
    fn the_step_kinds_round_trip() {
        for kind in [StepKind::Task, StepKind::Wait] {
            assert_eq!(StepKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(StepKind::parse("loop"), None);
    }

    #[test]
    fn only_the_three_terminal_run_states_are_terminal() {
        assert!(!ExecutionStatus::Running.is_terminal());
        for status in [
            ExecutionStatus::Completed,
            ExecutionStatus::Failed,
            ExecutionStatus::Cancelled,
        ] {
            assert!(status.is_terminal(), "{status:?}");
            assert_eq!(ExecutionStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(ExecutionStatus::parse("paused"), None);
    }

    #[test]
    fn open_step_states_are_the_ones_the_engine_must_pick_up() {
        for status in [
            StepStatus::Pending,
            StepStatus::Running,
            StepStatus::Waiting,
        ] {
            assert!(status.is_open(), "{status:?}");
        }
        for status in [
            StepStatus::Succeeded,
            StepStatus::Failed,
            StepStatus::Cancelled,
        ] {
            assert!(!status.is_open(), "{status:?}");
            assert_eq!(StepStatus::parse(status.as_str()), Some(status));
        }
    }
}
