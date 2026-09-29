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
pub mod credential_store;
pub mod credentials;
pub mod cron;
pub mod definition;
pub mod engine;
pub mod error;
pub mod graph;
pub mod graph_store;
pub mod handler;
pub mod model;
pub mod node_package;
pub mod oauth;
pub mod oauth_client;
pub mod oauth_refresh;
pub mod oauth_store;
pub mod registry;
pub mod store;

pub use definition::{StepDefinition, Trigger, WorkflowDefinition};
pub use engine::{RunnerConfig, SweepReport, TickReport};
pub use error::{Result, WorkflowError};
pub use graph::{
    Compiled, Connection, Graph, GraphNode, Issue, Position, StickyNote, compile,
    compile_or_refuse, validate,
};
pub use graph_store::{SaveOutcome, StoredGraph};
pub use handler::{ActionContext, ActionFuture, ActionHandler, NoActionHandler};
pub use model::{
    ExecutionStatus, NewWorkflow, StepKind, StepStatus, TriggerKind, Workflow, WorkflowExecution,
    WorkflowStep,
};
pub use registry::{
    Capability, CredentialDefinition, CredentialField, CredentialKind, FieldType, LintFinding,
    NodeCategory, NodeDefinition, OAuthConfig, ParamHint, ParamSpec, Port, PortKind, Sandbox,
    credential_type_keys, credential_types, find_credential_type, find_node, lint, node_keys,
    nodes,
};
// The credential entity of REQ-087 slice 2. Re-exported because the API layer names these
// types in its bodies, and a caller reaching two modules for one entity is how the two halves
// of a feature end up disagreeing about what a credential is.
pub use credential_store::{
    CredentialUpdate, DeleteOutcome, NewNodePackage, NodePackage, PACKAGE_COLUMNS,
};
pub use credentials::{
    Credential, CredentialUsage, CredentialUsageReport, Health, ListQuery as CredentialListQuery,
    NewCredential, SCOPES as CREDENTIAL_SCOPES, SHARINGS as CREDENTIAL_SHARINGS, SecretPayload,
    Settings, TestOutcome,
};
