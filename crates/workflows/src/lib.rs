//! Omnion workflows.
//!
//! The durable step engine behind the automation surface (docs/requests/REQ-003, phase P09):
//! a workflow is a definition plus an ordered list of steps, a run materialises those steps as
//! rows, and a background runner advances them one at a time — retrying a failed step with a
//! backoff, parking a wait step until its deadline, and settling the run when the last step is
//! done.
//!
//! The design follows the n8n teardown (docs/09-N8N-TEARDOWN.md §13): durable steps from day
//! one (lesson 1), an explicit cancellation flag (2), retries as step policy with a cap of five
//! (4), a wait sweeper as a safety net rather than the mechanism (9, 19) — and no dynamic code
//! in the core process (14): the actions this version runs are a closed, built-in set.
//!
//! ```text
//! definition ──▶ workflows row ──▶ workflow_executions row ──▶ workflow_steps rows
//!                     (schedule)          (one run)                (pending → running → …)
//! ```

#![forbid(unsafe_code)]

pub mod actions;
pub mod cron;
pub mod definition;
pub mod engine;
pub mod error;
pub mod handler;
pub mod model;
pub mod store;

pub use definition::{StepDefinition, Trigger, WorkflowDefinition};
pub use engine::{RunnerConfig, SweepReport, TickReport};
pub use error::{Result, WorkflowError};
pub use handler::{ActionContext, ActionFuture, ActionHandler, NoActionHandler};
pub use model::{
    ExecutionStatus, NewWorkflow, StepKind, StepStatus, TriggerKind, Workflow, WorkflowExecution,
    WorkflowStep,
};
