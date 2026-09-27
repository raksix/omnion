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
    /// The platform recorded the event this workflow listens for (see [`crate::definition`]).
    Event,
}

impl TriggerKind {
    /// Canonical lowercase name stored in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Schedule => "schedule",
            Self::Event => "event",
        }
    }

    /// Parse a stored value.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "manual" => Some(Self::Manual),
            "schedule" => Some(Self::Schedule),
            "event" => Some(Self::Event),
            _ => None,
        }
    }

    /// `true` when the trigger is driven by the platform rather than by a person.
    #[must_use]
    pub const fn is_automatic(self) -> bool {
        matches!(self, Self::Schedule | Self::Event)
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
    /// Ends the run when a comparison over the resolved inputs does not hold.
    ///
    /// The engine evaluates it; nothing outside sees it. It is a *step* rather than a
    /// property of a step because a branch is a position in the run: the same field read
    /// before step 3 and after step 9 can answer differently, and the panel draws the
    /// difference on the trace.
    Branch,
    /// Ends the run on purpose, with a reason an operator reads in the trace.
    Stop,
}

impl StepKind {
    /// Canonical lowercase name stored in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Wait => "wait",
            Self::Branch => "branch",
            Self::Stop => "stop",
        }
    }

    /// Parse a stored value.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "task" => Some(Self::Task),
            "wait" => Some(Self::Wait),
            "branch" => Some(Self::Branch),
            "stop" => Some(Self::Stop),
            _ => None,
        }
    }

    /// `true` when a step of this kind never runs an action.
    #[must_use]
    pub const fn is_control(self) -> bool {
        matches!(self, Self::Wait | Self::Branch | Self::Stop)
    }
}

/// What a step's own failure does (REQ-003 slice 2).
///
/// This is the per-step half of the rule's error policy: `inherit` defers to the rule's
/// own setting, so a step an author never touched behaves the way every step behaved
/// before the policy existed — a failure ends the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnError {
    /// Take the rule's own policy.
    Inherit,
    /// End the run.
    Stop,
    /// Record the failure and let the run continue to the next step.
    Continue,
}

impl OnError {
    /// Canonical lowercase name stored in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::Stop => "stop",
            Self::Continue => "continue",
        }
    }

    /// Parse a stored value.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "inherit" => Some(Self::Inherit),
            "stop" => Some(Self::Stop),
            "continue" => Some(Self::Continue),
            _ => None,
        }
    }
}

/// Longest a step may block before the engine fails it with the limit named.
pub const MAX_STEP_TIMEOUT_MS: i32 = 120_000;

/// Default step timeout, and the floor (a step's timeout is `> 0`).
pub const DEFAULT_STEP_TIMEOUT_MS: i32 = 30_000;

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
    /// Event name when the trigger is an event.
    pub trigger_event: Option<String>,
    /// Conditions an event trigger's payload must satisfy, as stored JSON: an `all` / `any`
    /// group tree, or the flat array every rule written before migration 0020 carries.
    pub conditions: serde_json::Value,
    /// SHA-256 of an inbound-webhook trigger's token, when the rule has one.
    ///
    /// The token itself is never stored: it is shown once when it is minted, and only this
    /// hash can match a call. The automation layer owns it (see `omnion-automation::hooks`).
    pub hook_token_hash: Option<String>,
    /// The rule's own error policy, which a step inherits when it does not set one.
    pub on_error: String,
    /// The key that signs an outbound `http_request` from this rule.
    ///
    /// Never returned by the API, never audited, never rendered — the same rule the inbound
    /// token follows, and for the same reason: an audit row is read by more people than the
    /// secret is meant for.
    pub hook_secret: Option<String>,
    /// Next time the scheduler should start this workflow.
    pub next_run_at: Option<OffsetDateTime>,
    /// Ordered step definitions, as stored JSON.
    pub steps: serde_json::Value,
    /// When the trigger last started a run of this workflow (schedules and events).
    pub last_triggered_at: Option<OffsetDateTime>,
    /// How many runs the trigger has started.
    pub trigger_count: i32,
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
     trigger_kind, schedule, trigger_event, conditions, hook_token_hash, on_error, hook_secret, \
     next_run_at, steps, last_triggered_at, trigger_count, created_by, created_at, updated_at";

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
    /// Whether the schedule/event trigger is armed.
    pub enabled: bool,
    /// Trigger kind.
    pub trigger: TriggerKind,
    /// Cron expression when scheduled.
    pub schedule: Option<String>,
    /// Event name when the trigger is an event.
    pub trigger_event: Option<String>,
    /// Conditions of an event trigger, as stored JSON array.
    pub conditions: serde_json::Value,
    /// The rule's error policy; a step that inherits takes this.
    pub on_error: OnError,
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
    /// The event payload the run started from, when it started from an event.
    ///
    /// A branch step reads `event.<field>` out of it and the run detail shows it beside the
    /// trace. A manual run and a schedule carry `None` — there is no event behind them.
    pub event_payload: Option<serde_json::Value>,
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
     triggered_by, started_at, finished_at, error, event_payload";

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
    /// `task`, `wait`, `branch` or `stop`.
    pub kind: String,
    /// Built-in action of a task step.
    pub action: Option<String>,
    /// Step parameters, as stored JSON.
    pub params: serde_json::Value,
    /// What this step's own failure does (`inherit` takes the rule's policy).
    pub on_error: String,
    /// How long the step may block before it is failed with the limit named.
    pub timeout_ms: i32,
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
    /// `true` when the run deliberately outlived this step's failure.
    pub ignored: bool,
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

    /// The parsed error policy.
    #[must_use]
    pub fn on_error(&self) -> Option<OnError> {
        OnError::parse(&self.on_error)
    }
}

/// Columns of `workflow_steps`, in the order [`WorkflowStep`] expects.
pub const STEP_COLUMNS: &str = "id, execution_id, step_no, name, kind, action, params, on_error, \
     timeout_ms, status, attempts, max_attempts, available_at, started_at, finished_at, output, \
     error, ignored";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_trigger_kinds_round_trip() {
        for kind in [
            TriggerKind::Manual,
            TriggerKind::Schedule,
            TriggerKind::Event,
        ] {
            assert_eq!(TriggerKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(TriggerKind::parse("cron"), None);
        assert!(TriggerKind::Event.is_automatic());
        assert!(TriggerKind::Schedule.is_automatic());
        assert!(!TriggerKind::Manual.is_automatic());
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
