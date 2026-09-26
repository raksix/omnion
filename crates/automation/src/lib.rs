//! Omnion automation: the layer that turns a recorded event into a running workflow.
//!
//! The automation engine of docs/requests/REQ-003 — **trigger → condition → action** — built on
//! top of the durable step engine (P09) rather than beside it:
//!
//! ```text
//! platform records an event ──▶ the matcher reads the bus (once, in order)
//!                                   │
//!                     an armed rule listens for that event name
//!                                   │  conditions hold against the payload?
//!                                   ▼
//!                     a run starts: the rule's actions become step rows
//!                                   │
//!                     the P09 runner advances them (retries, waits, audits)
//! ```
//!
//! What this crate adds to the engine:
//!
//! * [`model`] — a rule as the panel sees it: one event, its conditions and its actions;
//! * [`condition`] — the closed comparison set a payload is tested against (no expression
//!   language, no dynamic code — docs/09-N8N-TEARDOWN.md §13 lesson 14);
//! * [`binding`] — `{{event.field}}` placeholders in action parameters, resolved **when the run
//!   is materialised**, so a stored step carries the values of the event that started it;
//! * [`matcher`] — reading the bus from a durable cursor and starting one run per match;
//! * [`actions`] — the host actions the engine hands over: `send_email` (SMTP) and
//!   `comment_revision` (a note on a content revision);
//! * [`mail`] — the small SMTP client behind the email action.
//!
//! The crate never writes `workflows`/`workflow_steps` rows by hand: it goes through
//! `omnion_workflows::store`, which keeps the engine's invariants in one place.

#![forbid(unsafe_code)]

pub mod actions;
pub mod binding;
pub mod condition;
pub mod error;
pub mod mail;
pub mod matcher;
pub mod model;

pub use actions::AutomationActions;
pub use binding::{resolve_params, validate_bindings};
pub use condition::{Condition, ConditionOperator};
pub use error::{AutomationError, Result};
pub use mail::{Email, MailError, MailSettings};
pub use matcher::{MatchReport, drain, event_cursor};
pub use model::{AutomationRule, NewRule, build_definition};
