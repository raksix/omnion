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
    /// Parks the run until a person with `workflows.approve` decides (REQ-003 slice 3).
    ///
    /// It is a kind rather than a host action on purpose: the gate has no effect of its own
    /// — it *suspends* — so the engine owns it, exactly as it owns a wait. An author parks
    /// the run by naming the permission that may let it go on; the engine then writes the
    /// approval row, emits the event and stops claiming steps until the decision lands.
    Approval,
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
            Self::Approval => "approval",
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
            "approval" => Some(Self::Approval),
            _ => None,
        }
    }

    /// `true` when a step of this kind never runs an action.
    #[must_use]
    pub const fn is_control(self) -> bool {
        matches!(
            self,
            Self::Wait | Self::Branch | Self::Stop | Self::Approval
        )
    }

    /// `true` when a step of this kind *parks* rather than finishing: it is claimed once to
    /// park and once to be let go, so the claim count is the state.
    #[must_use]
    pub const fn parks(self) -> bool {
        matches!(self, Self::Wait | Self::Approval)
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
    /// Parked on a person: a `wait_for_approval` step wrote its row and the run is waiting
    /// for a decision (REQ-003 slice 3).
    ///
    /// It is neither `Running` nor terminal, and that is the whole point. `Running` would
    /// lie — the engine must not claim a step while a person is thinking, and every listing
    /// that counts "in progress" would count a parked run as busy. Terminal would be worse:
    /// a decision reopens the run, and a state that can be reopened is not a final one.
    AwaitingApproval,
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
            Self::AwaitingApproval => "awaiting_approval",
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
            "awaiting_approval" => Some(Self::AwaitingApproval),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// `true` when no further work can happen.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// `true` while the engine may claim a step of this run.
    ///
    /// The run is *open* for a person and *claimable* for the engine are different questions:
    /// an approval parks the first without occupying the second, which is why the two are
    /// two methods and not one.
    #[must_use]
    pub const fn is_claimable(self) -> bool {
        matches!(self, Self::Running)
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
    /// The step did not run, because the run was started further down the graph
    /// (*Run from here*, REQ-004 slice 3).
    ///
    /// A sixth state rather than a flag on `succeeded` or `pending`, and the reason is
    /// visible in the three queries that consume step status:
    ///
    /// * `claim_due_step` reads `('pending', 'waiting')` — a skipped step is never claimed;
    /// * `settle_execution` counts open work as `('pending','running','waiting')` and
    ///   failures as `= 'failed'`, so a skipped step is closed and is not a failure, which
    ///   is what lets a run whose prefix was skipped still settle `completed`;
    /// * `retry_step_from` re-opens `('failed','cancelled','pending','waiting')` — a skipped
    ///   prefix stays skipped when a run is retried, because re-running from a node must
    ///   not silently re-run what the author asked to skip.
    ///
    /// So the new state needs no engine branch, and the invariant is the interesting part:
    /// the *same* three queries that already existed were shaped so that one more terminal
    /// state would be free. A state that had to be threaded through them would have been
    /// the tell that the schema was not ready for it.
    Skipped,
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
            Self::Skipped => "skipped",
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
            "skipped" => Some(Self::Skipped),
            _ => None,
        }
    }

    /// `true` while the step still needs the engine's attention.
    #[must_use]
    pub const fn is_open(self) -> bool {
        matches!(self, Self::Pending | Self::Running | Self::Waiting)
    }

    /// `true` when this step will never run again, whatever the engine does next.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Skipped
        )
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
    /// Whose authority the rule's host actions run with (REQ-003 slice 3).
    ///
    /// `None` means the author, read from `created_by` at run time — not a copied id, so
    /// the rule follows its author. A rule whose author has been deleted resolves to nobody
    /// and every host action stops with `automation.rule.permission_revoked`, which is the
    /// only honest answer: a deleted account's authority is not a permission anybody holds.
    ///
    /// The column is set explicitly when an operator hands a rule to a service account, which
    /// is the case the copy could not express: a shared "content publisher" identity that
    /// keeps working after its human leaves.
    pub run_as_user_id: Option<Uuid>,
    /// Runs this workflow may start in a rolling hour (REQ-003 slice 4).
    ///
    /// The column is the workflow engine's because the engine owns the table; the
    /// *meaning* belongs to the automation layer (`omnion_automation::limits`), which is
    /// the only thing that consults it. A scheduled workflow a human started by hand is
    /// not rate-limited by it — the bound is a property of a rule that fires on its own.
    pub rate_limit_per_hour: i32,
    /// What a second trigger does while a run of this workflow is still going:
    /// `queue` (it waits) or `skip` (the trigger is dropped). Read by
    /// `omnion_automation::limits`, like the rate limit above.
    pub concurrency: String,
    /// The last message one of this workflow's bounds produced when it refused a run.
    ///
    /// `None` is the ordinary state — a workflow that has never been refused. It is
    /// cleared the moment a run is admitted again, so the line answers "the last thing
    /// that went wrong", not "something once went wrong".
    pub last_error: Option<String>,
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
     run_as_user_id, rate_limit_per_hour, concurrency, last_error, next_run_at, steps, \
     last_triggered_at, trigger_count, created_by, created_at, updated_at";

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
    /// Whose authority the rule's host actions run with. `None` means the author.
    pub run_as_user_id: Option<Uuid>,
    /// Runs this workflow may start in a rolling hour; `None` takes the column default.
    ///
    /// `None` here rather than a required number so a caller outside the automation layer
    /// — a person creating a scheduled workflow through the workflows surface — cannot
    /// have to learn a bound that only rules on a trigger care about.
    pub rate_limit_per_hour: Option<i32>,
    /// What a second trigger does while a run is going; `None` takes the column default.
    pub concurrency: Option<String>,
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
    /// The approval that gates the step this run is parked on, when it is parked (REQ-003
    /// slice 3).
    ///
    /// A copy of the step's `approval_id` rather than a lookup: the panel's pending panel and
    /// a run's own summary both read it, and a join back from the run would make the panel
    /// query the steps table to answer "is this run waiting on somebody?" — the question a
    /// list of runs asks about every row it draws.
    #[sqlx(default)]
    pub approval_id: Option<Uuid>,
    /// The event payload the run started from, when it started from an event.
    ///
    /// A branch step reads `event.<field>` out of it and the run detail shows it beside the
    /// trace. A manual run and a schedule carry `None` — there is no event behind them.
    /// This is `COALESCE`-shaped rather than `Option`-shaped on purpose: the column is
    /// nullable in SQL, so reading it as `Option<Value>` would need the row to carry SQL
    /// NULL as *JSON* null, and every `select` would have to say `coalesce(event_payload,
    /// 'null'::jsonb)`. A missing payload and a JSON `null` payload mean the same thing to
    /// a caller, and the read must not be able to fail on one of them.
    #[sqlx(default)]
    pub event_payload: Option<serde_json::Value>,
    /// The graph node this run was started at, when it was started mid-graph
    /// (*Run from here*, REQ-004 slice 3).
    ///
    /// `None` for every ordinary run — a manual run, a schedule and an event all start at
    /// the trigger, and saying "the trigger" on each of those rows would be a field that
    /// always holds the same answer. Its absence is the signal, and the trace renders
    /// "started at the trigger" for it.
    #[sqlx(default)]
    pub started_from_node: Option<String>,
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
     triggered_by, started_at, finished_at, error, approval_id, \
     coalesce(event_payload, 'null'::jsonb) as event_payload, started_from_node";

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
    /// The approval row gating this step, when it is an approval step (REQ-003 slice 3).
    ///
    /// `None` for every other kind, and for an approval step that has not parked yet: the row
    /// is written by the claim that parks it, so a step the engine has not reached has
    /// nothing to point at.
    #[sqlx(default)]
    pub approval_id: Option<Uuid>,
    /// Why this step did not run, when it did not (REQ-004 slice 3).
    ///
    /// Set only on a `skipped` step, and the database refuses a skip without one: the
    /// criterion asks for a trace that says *why*, and a reason stored in a code that a
    /// reader has to know is not a reason.
    #[sqlx(default)]
    pub skip_reason: Option<String>,
    /// The graph node this step came from, when the rule was started from a graph.
    ///
    /// `None` for a rule whose definition predates the builder: attributing such a step to
    /// whichever node happens to sit at the same index would be a guess, and a wrong one.
    #[sqlx(default)]
    pub node_id: Option<String>,
    /// The output port that carried into this step (`success`, `true`, `case_1`, …).
    #[sqlx(default)]
    pub branch: Option<String>,
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
     error, ignored, approval_id, skip_reason, node_id, branch";

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
        for kind in [
            StepKind::Task,
            StepKind::Wait,
            StepKind::Branch,
            StepKind::Stop,
            StepKind::Approval,
        ] {
            assert_eq!(StepKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(StepKind::parse("loop"), None);
    }

    #[test]
    fn an_approval_step_is_a_control_step_that_parks() {
        // Both halves matter and they are different: `is_control` says the engine owns the
        // step (no action is named), `parks` says the claim count *is* the state (claim once
        // to park, once to be let go). A gate that was a task step would be handed to the
        // host handler with nothing to run; a gate that did not park would be claimed
        // forever.
        assert!(StepKind::Approval.is_control());
        assert!(StepKind::Approval.parks());
        assert!(StepKind::Wait.parks());
        assert!(!StepKind::Task.is_control());
        assert!(!StepKind::Task.parks());
        assert!(
            !StepKind::Branch.parks(),
            "a branch decides, it does not park"
        );
        assert!(!StepKind::Stop.parks());
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
    fn a_parked_run_is_open_but_not_claimable() {
        // The distinction the engine depends on: a run waiting on a person must not be
        // settled (a decision reopens it) and must not be claimed (nothing may progress
        // until somebody decides). Collapsing either half into `Running` or into a terminal
        // state is the bug this test exists to prevent.
        let parked = ExecutionStatus::AwaitingApproval;
        assert!(!parked.is_terminal(), "a decision reopens it");
        assert!(
            !parked.is_claimable(),
            "no step may progress behind a person's back"
        );
        assert_eq!(ExecutionStatus::parse("awaiting_approval"), Some(parked));
        assert_eq!(parked.as_str(), "awaiting_approval");

        assert!(ExecutionStatus::Running.is_claimable());
        for terminal in [
            ExecutionStatus::Completed,
            ExecutionStatus::Failed,
            ExecutionStatus::Cancelled,
        ] {
            assert!(terminal.is_terminal(), "{terminal:?}");
            assert!(
                !terminal.is_claimable(),
                "a finished run is not claimable either"
            );
        }
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
