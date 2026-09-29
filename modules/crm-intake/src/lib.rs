//! Omnion CRM intake — the front door of the business modules (docs/requests/REQ-117).
//!
//! A lead does not come from the CRM. It comes from a form on somebody's website, a quote
//! request, a phone number somebody typed into a landing page — a *surface* the CRM knows
//! nothing about. This crate is the seam: it binds a capture surface to a pipeline, maps the
//! submission's keys onto lead columns, judges whether the person is new, and files the row
//! with everything needed to explain the verdict later.
//!
//! It is a module, not a core crate (docs/04-ARCHITECTURE.md): it knows what a lead *is*, and
//! it knows nothing about HTTP, about the panel or about the other business modules. The
//! routes in `apps/api` are a thin layer over [`store`], and the notification the assignment
//! triggers is slice 2.
//!
//! ## The four rules the rest of the crate is built around
//!
//! 1. **A rejected submission is a row, not a hole.** Spam and invalid submissions are stored
//!    with their reason and their score ([`model::SpamVerdict`]). The alternative — dropping
//!    them — makes a form that started rejecting everything look exactly like a form nobody
//!    submitted, which is the failure an operator cannot diagnose.
//! 2. **The public endpoint never says more than it must.** A capture call answers `202` with
//!    a reference whether the submission was accepted, filed as a duplicate or filed as spam
//!    ([`model::CaptureOutcome`]). A response that distinguished those teaches an attacker
//!    which addresses are already in the CRM and which guesses worked.
//! 3. **Every verdict carries its reason.** The matched key and the score
//!    ([`dedupe::Match`]), the transform that cleaned a value ([`mapping`]), the heuristic that
//!    judged a submission ([`model::SpamVerdict`]) — because the first time an operator is
//!    wrong about one of them they stop trusting the whole inbox.
//! 4. **Closed lists, checked against the migration.** Statuses, decisions, kinds, policies,
//!    targets and transforms are compile-time lists ([`vocabulary`], [`mapping`]) and the
//!    SQL that has to agree with them carries a test that reads the migration file. A value
//!    the crate accepts and the database refuses reads as "nothing happened", every time.

#![forbid(unsafe_code)]

pub mod assignment;
pub mod assignment_store;
pub mod autoresponder;
pub mod autoresponder_store;
pub mod convert;
pub mod convert_store;
pub mod dedupe;
pub mod error;
pub mod keys;
pub mod mapping;
pub mod model;
pub mod store;
pub mod vocabulary;

pub use assignment::{
    AssignmentInput, AssignmentOutcome, AssignmentRule, BusinessHours, SlaPolicy, SlaState, due_at,
    next_position, renumber, simulate, validate_policy, validate_rule,
};
pub use assignment_store::{
    Breach, NewPolicy, NewRule, claim_assignment, create_policy, create_rule, delete_policy,
    delete_rule, due_breaches, escalation_target, find_policy, find_rule, list_policies,
    list_rules, mark_escalated, policy_for_source, reorder_rules, stamp_assignment, update_policy,
    update_rule,
};
pub use convert::{
    Availability, Conversion, STEPS, Step, StepState, deal_title, initial_amount, step_plan,
};
pub use dedupe::{Candidate, DedupePolicy, Match, MatchKey, Verdict, evaluate as dedupe_evaluate};
pub use error::{CrmIntakeError, Result};
pub use keys::{hash_key, hint_for, issue_key, verify_key};
pub use mapping::{MappedValues, MappingEntry, apply, health, validate_required_targets};
pub use model::{
    Attribution, CaptureOutcome, IntakeSource, Lead, LeadEvent, LeadMetrics, LeadOwner,
    NewIntakeSource, SpamVerdict, contactable,
};
pub use vocabulary::{
    ASSIGNMENT_TARGETS, DECISIONS, DEDUPE_POLICIES, MAX_BULK_IDS, MAX_PAGE, MAX_PAYLOAD_BYTES,
    SOURCE_KINDS, STATUSES, is_decision, is_dedupe_policy, is_open, is_round_robin_target,
    is_source_kind, is_status,
};
