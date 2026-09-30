//! Omnion AI — the workflow builder's half.
//!
//! The request this module answers is one sentence: *"If an invoice is 7 days overdue, email
//! the customer; if 14 days, create a task for the sales owner"* (docs/requests/REQ-046). The
//! answer is a draft: a definition the workflow API already accepts, held in a lifecycle a
//! person decides on, and never a rule that fires before somebody approved it.
//!
//! Three rules shape everything here, and each of them is a decision that could have gone the
//! other way:
//!
//! * **A generated definition is an ordinary definition.** It is `{ trigger, steps }` and it
//!   is validated by `omnion_workflows` itself — the same validator `POST /workflows` uses.
//!   A second shape the engine cannot run would be a parallel execution path wearing a
//!   generator's name.
//! * **One repair round-trip, then the draft fails.** The action registry is closed, so an
//!   answer that names an action the platform does not have is not fixable by widening the
//!   registry — it is fixable by telling the model which rule it broke, exactly once.
//! * **Approval materialises a *disabled* workflow.** A human read it and a test run exercised
//!   it; arming a schedule is a separate press.

#![forbid(unsafe_code)]

pub mod definition;
pub mod error;
pub mod generate;
pub mod model;
pub mod store;

pub use error::{AiWorkflowError, Result};
pub use generate::{
    Attempt, Fold, GenerationOutcome, GenerationPlan, GenerationRequest, MAX_ATTEMPTS,
    MAX_OUTPUT_TOKENS, MAX_REPAIR_ROUNDS, fold, generate, plan_for, repair_prompt, request_for,
    system_prompt, user_prompt,
};
pub use model::{
    AiWorkflowDraft, DraftFilter, DraftPage, DraftTokens, MAX_PROMPT_LEN, MAX_TITLE_LEN,
    NEW_DRAFT_STATUS, NewDraft, OPEN_STATUSES, STATUSES, draft_status_is_known,
};
pub use store::{
    apply_answer, apply_decision, apply_failure, apply_revision, attach_workflow, delete_draft,
    find_draft, find_draft_in, insert_draft, list_authors, list_drafts, replace_definition,
};
