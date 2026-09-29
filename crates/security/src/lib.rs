//! Omnion security centre — posture, findings and what the platform can honestly claim about
//! itself (docs/requests/REQ-012).
//!
//! A security screen has one job that no other screen has: **not to reassure the operator when
//! it has nothing to go on.** Every other list in this panel can fall back to a plausible
//! default; this one cannot, because a green row that was never checked is the single most
//! expensive thing the product can render. The rule is written into the design rather than left
//! to each check's judgement:
//!
//! * A check is a pure function of an [`posture::Environment`]. It cannot look anything up, so
//!   it cannot accidentally conclude anything from an empty result set.
//! * A probe that could not read answers [`posture::Probe::Unknown`], and the evaluator turns
//!   that into `unknown` — never `pass`. Absence of evidence is a state, not a pass.
//! * The overview renders **every** check in the registry, including one that has never run,
//!   so a missing row can never read as "nothing to report here".
//!
//! It is infrastructure, like `omnion-audit` and `omnion-events`: this crate knows what a
//! finding *is*, not what a specific vulnerability in a specific dependency means. Slice 1 is
//! posture and findings; the header policy, the limiter, the lockout and the IP rules are
//! slices 2–4 and each is its own module.

#![forbid(unsafe_code)]

pub mod error;
pub mod model;
pub mod posture;
pub mod store;
pub mod vocabulary;

pub use error::{Result, SecurityError};
pub use model::{
    BuiltFinding, CheckResult, Finding, FindingPage, FindingQuery, NewCheckResult, NewFinding,
    SeverityCount, StatusChange, fingerprint_of, is_slug,
};
pub use posture::{
    CHECKS, Check, CheckOutcome, Environment, Probe, evaluate_all, find, keys, to_overview,
    unevaluated_state,
};
pub use store::{
    BulkReport, bulk_set_status, count_findings, find_finding, last_run_at, latest_results,
    list_findings, open_counts_by_severity, record_run, set_status, stale_dependency_count,
    upsert_finding,
};
pub use vocabulary::{
    FINDING_STATUSES, MAX_BULK_IDS, MAX_PAGE, SEVERITIES, SOURCES, STATES,
    STATE_WHEN_UNEVALUATED, is_finding_status, is_severity, is_source, is_state, severity_rank,
};
