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
//! * [`groups`] — those comparisons arranged into `all` / `any` trees (REQ-003 slice 1), with a
//!   flat list still reading as one `all` group so every earlier rule keeps firing;
//! * [`catalogue`] — the closed vocabulary the panel writes rules in: the event library with
//!   its payload fields, the operators and their bounds, the action set and the trigger kinds;
//! * [`hooks`] — the inbound-webhook trigger: an unguessable per-rule URL whose only credential
//!   is a token stored as a hash;
//! * [`testing`] — a dry run against a hand-written payload (`would_send`, no side effects) and
//!   the one-shot listener that captures the next real event a rule matches.
//! * [`binding`] — `{{event.field}}` placeholders in action parameters, resolved **when the run
//!   is materialised**, so a stored step carries the values of the event that started it;
//! * [`matcher`] — reading the bus from a durable cursor and starting one run per match;
//! * [`authority`] — whose permissions a host action runs with, resolved *at run time*, and
//!   the `automation.rule.permission_revoked` refusal a rule stops on when that authority is
//!   gone (REQ-003 slice 3);
//! * [`actions`] — the host actions the engine hands over: `send_email` (SMTP),
//!   `comment_revision` (a note on a content revision), `http_request` (a signed outbound
//!   call), `publish_page` and `run_workflow` (REQ-003 slice 2);
//! * [`outbound`] — the three actions that leave the process, and the two bounds that keep
//!   them bounded: the outbound host allow-list and the HMAC signature every call carries;
//! * [`limits`] — the two bounds that make a rule safe to leave *armed*: the rolling-hour
//!   rate window and the concurrency policy, both decided in the same transaction that
//!   starts the run (REQ-003 slice 4);
//! * [`loopguard`] — the endless-loop guard: the same step kind twice in a row with
//!   identical resolved parameters stops the run and explains itself in the trace;
//! * [`mail`] — the small SMTP client behind the email action.
//!
//! The crate never writes `workflows`/`workflow_steps` rows by hand: it goes through
//! `omnion_workflows::store`, which keeps the engine's invariants in one place.

#![forbid(unsafe_code)]

pub mod actions;
pub mod authority;
pub mod binding;
pub mod catalogue;
pub mod condition;
pub mod error;
pub mod groups;
pub mod hooks;
pub mod limits;
pub mod loopguard;
pub mod mail;
pub mod matcher;
pub mod model;
pub mod outbound;
pub mod templates;
pub mod testing;
pub mod versions;

pub use actions::AutomationActions;
pub use authority::Authority;
pub use binding::{resolve_params, validate_bindings};
pub use condition::{Condition, ConditionOperator};
pub use error::{AutomationError, Result};
pub use limits::{Admit, Policy, admit};
pub use loopguard::LoopGuard;
pub use mail::{Email, MailError, MailSettings};
pub use matcher::{MatchReport, drain, event_cursor};
pub use model::{AutomationRule, NewRule, build_definition};
pub use outbound::{HttpSettings, Response as HttpResponse};
// The gallery is a list of definitions a user *installs*, so the crate exports it as a
// lookup rather than as a store: installing is an ordinary create the route performs.
pub use templates::{NewTemplateRule, Template, all as list_templates, find as find_template};
pub use versions::{Change, Version, list as list_versions, record as record_version};
