//! Omnion audit trail.
//!
//! Append-only records of privileged actions (docs/07-IAM.md §13, §19). The same chain later
//! carries AI-agent and service-account actions, which is why `actor_type` is part of the
//! row (docs/06-AI-HUB.md); the identity crate never writes here directly — privileged work
//! goes through this module.
//!
//! The audit trail is not a log sink: a caller that cannot record an audit row must not report
//! the action as successful.

#![forbid(unsafe_code)]

pub mod entries;
pub mod error;

pub use entries::{ActorType, AuditEntry, NewAuditEntry, recent, record};
pub use error::{AuditError, Result};
