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
pub mod approval;
pub mod branch;
pub mod cron;
pub mod definition;
pub mod engine;
pub mod error;
pub mod graph;
pub mod graph_store;
pub mod guard;
pub mod handler;
pub mod model;
pub mod plugin_nodes;

#[cfg(test)]
#[path = "plugin_nodes_tests.rs"]
mod plugin_nodes_tests;
pub mod run_from;
pub mod retry_node;
pub mod store;
pub mod test_listener;

pub use approval::{APPROVAL_PERMISSION, ApprovalParams, Decision as ApprovalDecision};
pub use definition::{StepDefinition, Trigger, WorkflowDefinition};
pub use engine::{RunnerConfig, SweepReport, TickReport};
pub use error::{Result, WorkflowError};
pub use graph::{
    Edge, Finding, Graph, Node, NodeType, Port, Severity, find_cycle, find_node_type, project,
    reachable_from, validate,
};
pub use graph_store::{GraphDefinition, GraphUpdate, find_graph, replace_graph};
pub use guard::{GuardStep, GuardVerdict, NoRunGuard, RunGuard};
pub use handler::{ActionContext, ActionFuture, ActionHandler, NoActionHandler};
pub use test_listener::{
    LISTENER_TTL, ListenerStatus, MAX_PAYLOAD_BYTES, TestListener, hash_token, listener_is_live,
    mint_token, status_of,
};
pub use model::{
    DEFAULT_STEP_TIMEOUT_MS, ExecutionStatus, MAX_STEP_TIMEOUT_MS, NewWorkflow, OnError, StepKind,
    StepStatus, TriggerKind, Workflow, WorkflowExecution, WorkflowStep,
};
