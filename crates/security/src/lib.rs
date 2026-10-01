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
//! posture and findings; the header policy and the CSRF token are [`headers`] and [`csrf`]
//! (slice 2), the limiter and the lockout slice 3 and the IP rules slice 4 — each its own
//! module, because each answers a different question an operator will ask separately.

#![forbid(unsafe_code)]

pub mod csrf;
pub mod csv;
pub mod enforce;
pub mod error;
pub mod events;
pub mod events_csv;
pub mod events_store;
pub mod header_store;
pub mod headers;
pub mod ip_rules;
pub mod ip_store;
pub mod limiter;
pub mod limiter_redis;
pub mod limiter_store;
pub mod lockout;
pub mod model;
pub mod posture;
pub mod secrets;
pub mod secrets_store;
pub mod store;
pub mod vocabulary;

pub use csrf::{CSRF_COOKIE, CSRF_HEADER, derive_token as derive_csrf_token, tokens_match};
pub use csv::{COLUMNS as EXPORT_COLUMNS, MAX_EXPORT_ROWS, render as render_findings_csv};
pub use enforce::{EnforcedLockout, resolve as resolve_lockout};
pub use error::{Result, SecurityError};
pub use events::{
    EVENT_COLUMNS as SECURITY_EVENT_COLUMNS, EventCategory, EventPage, EventQuery, EventSource,
    MAX_EXPORT_ROWS as MAX_EVENT_EXPORT_ROWS, SecurityEvent,
};
pub use events::{
    describe_outcome, event_id, is_refusal, is_sensitive_key, page_size as event_page_size,
    search_term as event_search_term, summarise_metadata as summarise_event_metadata,
};
pub use events_csv::render as render_events_csv;
pub use events_store::{export_rows as export_security_events, list as list_security_events};
pub use header_store::{
    HeaderChange, StoredHeaders, header_history, history_count, load_headers, save_headers,
};
pub use headers::{
    CSP_DIRECTIVES, CspDirective, CspMode, HeaderLine, HeaderPolicy, HstsPolicy, PostureFacts,
    REFERRER_POLICIES, csp_header_name, is_effective_hsts,
};
pub use ip_rules::{
    IpRule, KINDS as IP_RULE_KINDS, MAX_NOTE as MAX_IP_RULE_NOTE, RuleKind, Verdict as IpVerdict,
    evaluate as evaluate_ip_rules, network_text, parse_cidr,
};
pub use ip_store::remove as remove_ip_rule;
pub use ip_store::{add as add_ip_rule, counts as ip_rule_counts, list as list_ip_rules};
pub use ip_store::{find_by_id as find_ip_rule, list_kind as list_ip_rules_of_kind};
pub use limiter::{
    ClientId, MAX_BURST, MAX_LIMIT, MAX_WINDOW_SECONDS, RatePolicy, RequestFacts, Verdict, decide,
    merge_with_defaults, parse_document as parse_rate_limits, scope_of, scope_options,
    to_document as rate_limits_to_document,
};
pub use limiter_redis::{Counted, enforce, forget, peek, retention_for};
pub use limiter_store::{
    StoredDocuments, failures_in_window, load_documents, load_lockout, load_rate_limits,
    locked_accounts, locked_count, save_lockout, save_rate_limits, unlock_account,
};
pub use lockout::{
    LockedAccount, LockoutPolicy, LockoutState, MAX_ATTEMPTS, MAX_LOCKOUT_MINUTES,
    MAX_WINDOW_SECONDS as MAX_FAILURE_WINDOW_SECONDS, MIN_ATTEMPTS, MIN_LOCKOUT_MINUTES,
    MIN_WINDOW_SECONDS as MIN_FAILURE_WINDOW_SECONDS, document as lockout_to_document,
    evaluate as evaluate_lockout, parse_document as parse_lockout,
};
pub use model::{
    BuiltFinding, CheckResult, Finding, FindingPage, FindingQuery, NewCheckResult, NewFinding,
    SeverityCount, StatusChange, fingerprint_of, is_slug,
};
pub use posture::{
    CHECKS, Check, CheckOutcome, Environment, Probe, evaluate_all, find, keys, to_overview,
    unevaluated_state,
};
pub use secrets::{
    INVENTORY_COLUMNS as SECRET_INVENTORY_COLUMNS, RotationEvidence, SecretInventory, SecretRef,
    SecretSource, SecretState, environment_names, environment_row,
};
pub use secrets_store::{
    NEVER_SELECTED_COLUMNS as SECRET_NEVER_SELECTED, inventory as secret_inventory,
};
pub use store::{
    BulkReport, bulk_set_status, count_findings, export_findings, find_finding, last_run_at,
    latest_results, list_findings, open_counts_by_severity, record_run, set_status,
    stale_dependency_count, upsert_finding,
};
pub use vocabulary::{
    FINDING_STATUSES, MAX_BULK_IDS, MAX_PAGE, RATE_SCOPES, SEVERITIES, SOURCES,
    STATE_WHEN_UNEVALUATED, STATES, is_finding_status, is_severity, is_source, is_state,
    severity_rank,
};
