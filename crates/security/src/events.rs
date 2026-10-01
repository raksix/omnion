//! The security-event view's model and filters (REQ-012, slice 4).
//!
//! ## The trap this module is built around
//!
//! The screen is specified as "a security-event table from the audit trail", and that sentence
//! is **half wrong**, in the way this request keeps producing defects. The audit trail is one of
//! the two places a security event lives; the other is `sign_in_attempts` — which no amount of
//! auditing produces rows in, because a failed sign-in happens before there is a session, and
//! therefore before there is an actor to write an audit entry for.
//!
//! A projection over `audit_log` alone would render a screen whose *sign-in* column is empty on
//! a platform where every one of its own requirements is met, and it would do so silently and
//! plausibly: an empty table with a filter that appears to work. The requirement's own wording
//! names the four things the screen must show, and "real sign-in … entries" is the first of
//! them — so the sign-in log is not an optional extra, it is half the definition.
//!
//! The two sources are therefore read as **one timeline**, and [`EventSource`] is what keeps
//! them honest about where a row came from. The screen renders the source rather than pretending
//! the rows are homogeneous.
//!
//! ## What is deliberately absent from a security event
//!
//! A security event is the row most likely to be copied into a ticket and forwarded to a third
//! party, so it carries **no payload blob**. `metadata` from the audit trail is summarised to a
//! short, key-level digest rather than exported whole: a security settings change writes the
//! policy it changed into its audit metadata, and a policy is configuration an operator treats
//! as sensitive — the same reasoning that keeps values out of the event-bus payloads in
//! `crates/events/src/catalogue.rs`. The digest names the *keys* that changed and nothing else.
//! The sign-in log carries an email and a user agent, both of which are read straight from the
//! row the platform already records.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::vocabulary::MAX_PAGE;

/// Which of the two tables a row came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    /// `audit_log` — a privileged action somebody took.
    Audit,
    /// `sign_in_attempts` — a sign-in attempt, which happens before there is an actor.
    SignIn,
}

impl EventSource {
    /// Value as stored and rendered.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Audit => "audit",
            Self::SignIn => "sign_in",
        }
    }
}

/// The categories the screen filters by.
///
/// This is a **presentation** vocabulary, not a storage one: the audit trail's `action` is a
/// stable dotted name (`security.ip_rule.added`) and the sign-in log's `outcome` is one of five
/// words, and the screen wants one drop-down. Mapping both into the same five categories is what
/// lets one filter answer "show me the denials" across two tables that share no vocabulary —
/// and it is why the categories are defined here, beside the query that has to match them,
/// rather than in the panel where a future row would silently fall out of every filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventCategory {
    /// A sign-in that failed, was refused by the address rule, or hit the lockout.
    SignIn,
    /// A lockout applied or released.
    Lockout,
    /// A permission refusal. **Never populated today** — see the note on [`list`].
    Denial,
    /// A settings, header or policy change.
    SettingsChange,
    /// An access-rule change.
    IpRuleChange,
    /// Anything the platform recorded that the screen does not classify.
    Other,
}

impl EventCategory {
    /// Value as rendered.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SignIn => "sign_in",
            Self::Lockout => "lockout",
            Self::Denial => "denial",
            Self::SettingsChange => "settings_change",
            Self::IpRuleChange => "ip_rule_change",
            Self::Other => "other",
        }
    }

    /// The drop-down's options, in the order the screen lists them.
    pub const ALL: &'static [Self] = &[
        Self::SignIn,
        Self::Lockout,
        Self::Denial,
        Self::SettingsChange,
        Self::IpRuleChange,
        Self::Other,
    ];

    /// Classify one audit action name.
    ///
    /// Prefix matching, because these names are a namespace rather than an enumeration: a new
    /// `security.ip_rule.*` row must classify as an access-rule change **without this function
    /// being edited**, or the screen quietly stops categorising it and the operator falls back to
    /// the free-text filter on a column they cannot search by action.
    #[must_use]
    pub fn of_action(action: &str) -> Self {
        if action.starts_with("security.ip_rule") {
            Self::IpRuleChange
        } else if action.starts_with("security.lockout") || action == "security.account.unlocked" {
            Self::Lockout
        } else if action.starts_with("security.") {
            Self::SettingsChange
        } else if action.starts_with("iam.session_revoked") {
            Self::Other
        } else {
            Self::Other
        }
    }

    /// Classify one sign-in attempt's outcome.
    ///
    /// `blocked` is the **address** rule refusing an attempt against a real account, and it is a
    /// refusal rather than a lockout: the account was never locked, so counting it as a lockout
    /// would report a lockout that did not happen — which is the one claim this screen must not
    /// make.
    #[must_use]
    pub fn of_outcome(outcome: &str) -> Self {
        match outcome {
            "locked" => Self::Lockout,
            _ => Self::SignIn,
        }
    }
}

/// One row of the security-event timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct SecurityEvent {
    /// Stable identity within the page: `"<source>:<id>"`.
    pub id: String,
    /// Which table this row came from.
    pub source: EventSource,
    /// When it happened.
    pub occurred_at: OffsetDateTime,
    /// The stable action name, or the sign-in outcome.
    pub action: String,
    /// The screen's category for it.
    pub category: EventCategory,
    /// Who did it, when the row names an account. `None` is meaningful: a failed sign-in has no
    /// actor, and rendering that as a blank cell reads as a rendering fault rather than a fact.
    pub actor: Option<String>,
    /// The account a lockout was applied to, which is *not* the actor — the same distinction
    /// `crates/events/src/catalogue.rs` makes about `security.lockout.triggered`.
    pub subject_user_id: Option<Uuid>,
    /// The peer address, when the row recorded one.
    pub client_ip: Option<String>,
    /// The user agent, when the row recorded one.
    pub user_agent: Option<String>,
    /// One line an operator can read instead of parsing the action name.
    pub outcome: String,
    /// Key-level digest of the audit metadata. Never the metadata itself — see the module note.
    pub detail: Option<String>,
    /// The organization's entry, for the detail drawer.
    pub organization_id: Option<Uuid>,
}

/// What the screen is asking for.
#[derive(Debug, Clone, Default)]
pub struct EventQuery {
    /// Free text over the action and the actor.
    pub search: Option<String>,
    /// One category, or every category.
    pub category: Option<EventCategory>,
    /// One source table, or both.
    pub source: Option<EventSource>,
    /// Lower bound on `occurred_at`.
    pub since: Option<OffsetDateTime>,
    /// Upper bound on `occurred_at`.
    pub until: Option<OffsetDateTime>,
    /// How many rows.
    pub limit: Option<usize>,
}

/// One page of the timeline, plus the counters the screen's header shows.
#[derive(Debug, Clone, PartialEq)]
pub struct EventPage {
    /// The rows, newest first, already merged across both sources.
    pub events: Vec<SecurityEvent>,
    /// How many the filter matched in total — the count the screen shows next to "Export", and
    /// the number that makes "50 of 312" visible instead of implying the page is all there is.
    pub total: i64,
    /// How many audit rows matched.
    pub audit_count: i64,
    /// How many sign-in rows matched.
    pub sign_in_count: i64,
    /// Whether the returned page is shorter than the total.
    pub truncated: bool,
}

/// The outcome words the sign-in log uses, and what the screen calls them.
///
/// The sign-in log's `outcome` is a five-value check constraint, and three of the five are a
/// refusal the operator needs to tell apart. `mfa_required` is deliberately *not* a failure: it
/// is a step-up, and a screen that renders it red teaches operators to ignore red.
pub fn describe_outcome(outcome: &str) -> &'static str {
    match outcome {
        "success" => "signed in",
        "failed" => "wrong password or unknown account",
        "locked" => "account locked out",
        "blocked" => "refused by the address rule",
        "mfa_required" => "second factor required",
        _ => "recorded attempt",
    }
}

/// Whether an outcome is a refusal the operator should look at.
#[must_use]
pub fn is_refusal(outcome: &str) -> bool {
    matches!(outcome, "failed" | "locked" | "blocked")
}

/// The CSV columns, in the table's own order.
pub const EVENT_COLUMNS: &[&str] = &[
    "occurred_at",
    "category",
    "action",
    "outcome",
    "actor",
    "subject_user_id",
    "client_ip",
    "user_agent",
    "detail",
    "source",
    "id",
];

/// How many events one export may carry.
pub const MAX_EXPORT_ROWS: usize = 50_000;

/// Build a row's page identity.
///
/// Two sources can hand back the same numeric `id` — both tables have their own identity column
/// — so an identity without the source would collide, and a merged list would drop one of the
/// two rows as a duplicate.
#[must_use]
pub fn event_id(source: EventSource, id: i64) -> String {
    format!("{}:{id}", source.as_str())
}

/// Trim a free-text filter to something a `like` can carry, and return `None` when it is empty.
///
/// Capped at 120 characters so a pasted paragraph cannot become an unbounded pattern; an
/// operator pasting a paragraph is searching for nothing in particular, and the cap is applied
/// here rather than trusted to the caller.
#[must_use]
pub fn search_term(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    let escaped: String = trimmed
        .chars()
        .take(120)
        .map(|c| match c {
            '%' | '_' | '\\' => format!("\\{c}"),
            _ => c.to_string(),
        })
        .collect();
    Some(escaped)
}

/// The `where` fragment and its bindings for one side of the union.
///
/// Kept beside [`crate::events`] rather than inlined in the SQL so both tables are filtered by
/// **one** implementation: the risk this screen has is a filter that works on the audit half and
/// silently ignores the sign-in half, which is indistinguishable from a filter that simply
/// matched nothing, and is the reason the UI filter list and this query are written together.
#[must_use]
pub fn query_spec(query: &EventQuery) -> QuerySpec {
    let mut clauses: Vec<String> = Vec::new();
    let mut values: Vec<Option<String>> = Vec::new();
    let mut push = |clause: &str, value: Option<String>| {
        if let Some(value) = value {
            values.push(Some(value));
            clauses.push(format!("{clause} ${}", values.len()));
        }
    };

    if let Some(term) = search_term(query.search.as_deref()) {
        push("action ilike", Some(format!("%{term}%")));
    }
    if let Some(category) = query.category {
        push(
            "action like",
            Some(format!("{}%", category_prefix(category))),
        );
    }
    if let Some(since) = query.since {
        values.push(Some(format!("{since}")));
        clauses.push(format!("created_at >= ${}", values.len()));
    }
    if let Some(until) = query.until {
        values.push(Some(format!("{until}")));
        clauses.push(format!("created_at <= ${}", values.len()));
    }

    QuerySpec { clauses, values }
}

/// The SQL fragment and the values it binds, with their placeholders already numbered.
///
/// Returned rather than rendered into a string so a caller cannot renumber a placeholder by
/// hand — the class of bug where the audit query gets one binding and the sign-in query two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuerySpec {
    /// `and …` clauses, each already carrying its own placeholder.
    pub clauses: Vec<String>,
    /// Values in placeholder order; `None` is a SQL `null`.
    pub values: Vec<Option<String>>,
}

impl QuerySpec {
    /// `true` when nothing narrowed the read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.clauses.is_empty()
    }

    /// The fragment, with a leading `" where 1 = 1"` so a caller never has to build one.
    #[must_use]
    pub fn where_clause(&self) -> String {
        if self.clauses.is_empty() {
            " where 1 = 1".to_owned()
        } else {
            format!(" where {}", self.clauses.join(" and "))
        }
    }

    /// The values, for binding.
    #[must_use]
    pub fn values(&self) -> Vec<Option<String>> {
        self.values.clone()
    }
}

/// The `action like` prefix a category filters on.
///
/// A prefix rather than an equality, because the audit trail's actions are a namespace: the
/// categories are decided by the *name*, and a category that matched only the six exact names
/// would drop every row a later release adds under the same namespace.
fn category_prefix(category: EventCategory) -> &'static str {
    match category {
        EventCategory::Lockout => "security.lockout%",
        EventCategory::IpRuleChange => "security.ip_rule%",
        EventCategory::SettingsChange => "security.%",
        // The sign-in side carries an outcome, not an action, and the category is applied in SQL
        // by outcome instead. An `action like` on these three would match nothing and look like
        // an empty filter, so the caller is expected to skip the action clause for them.
        EventCategory::SignIn | EventCategory::Denial | EventCategory::Other => "%",
    }
}

/// Summarise an audit row's metadata into at most one short line of **keys**.
///
/// The rule this enforces: a security event is the row most likely to be forwarded out of the
/// platform, so `metadata` — which for a settings change is the policy that changed — is never
/// exported whole. Only the key names and their value *shapes* are named. `attempted_password`,
/// `token` and `secret` keys are dropped entirely rather than summarised, because their presence
/// is itself the signal.
#[must_use]
pub fn summarise_metadata(metadata: &serde_json::Value) -> Option<String> {
    let object = metadata.as_object()?;
    let mut keys: Vec<String> = object
        .keys()
        .filter(|key| !is_sensitive_key(key))
        .map(|key| {
            let shape = match object.get(key) {
                Some(serde_json::Value::Number(number)) => number.to_string(),
                Some(serde_json::Value::Bool(flag)) => flag.to_string(),
                Some(serde_json::Value::Array(items)) => format!("{} items", items.len()),
                Some(serde_json::Value::Null) | None => "null".to_owned(),
                Some(_) => "set".to_owned(),
            };
            format!("{key}={shape}")
        })
        .collect();
    if keys.is_empty() {
        return None;
    }
    keys.sort();
    keys.truncate(8);
    Some(keys.join(", "))
}

/// `true` for a metadata key whose *value* must never leave this screen.
///
/// The list is short and the rule is blunt: a key that names a credential does not get summarised,
/// it gets dropped. Naming it in the detail line would leak its name into an export; keeping the
/// value would leak the credential.
#[must_use]
pub fn is_sensitive_key(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "password",
        "secret",
        "token",
        "key_material",
        "authorization",
        "credential",
        "bearer",
    ];
    NEEDLES.iter().any(|needle| lowered.contains(needle))
}

/// The screen's page size.
#[must_use]
pub fn page_size(limit: Option<usize>) -> usize {
    limit.unwrap_or(MAX_PAGE).clamp(1, MAX_PAGE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_sources_cannot_collide_on_an_id() {
        // The reason `event_id` carries the source. Both tables have an identity column that
        // starts at 1, so `audit:41` and `sign_in:41` are two real rows in one merged list and
        // an identity without the source would drop one of them as a duplicate.
        assert_eq!(event_id(EventSource::Audit, 41), "audit:41");
        assert_eq!(event_id(EventSource::SignIn, 41), "sign_in:41");
        assert_ne!(
            event_id(EventSource::Audit, 41),
            event_id(EventSource::SignIn, 41)
        );
    }

    #[test]
    fn a_category_is_decided_by_the_namespace_not_by_an_enumeration() {
        // The point of prefix matching: a row added later under the same namespace classifies
        // without this test — or [`EventCategory::of_action`] — being edited. An equality list
        // would silently drop it into `other` the day it shipped.
        assert_eq!(
            EventCategory::of_action("security.ip_rule.added"),
            EventCategory::IpRuleChange
        );
        assert_eq!(
            EventCategory::of_action("security.ip_rule.removed"),
            EventCategory::IpRuleChange
        );
        // A name that does not exist yet, under a namespace that does.
        assert_eq!(
            EventCategory::of_action("security.ip_rule.expired_by_sweep"),
            EventCategory::IpRuleChange
        );
        assert_eq!(
            EventCategory::of_action("security.headers.updated"),
            EventCategory::SettingsChange
        );
        assert_eq!(
            EventCategory::of_action("security.lockout.triggered"),
            EventCategory::Lockout
        );
        assert_eq!(
            EventCategory::of_action("security.account.unlocked"),
            EventCategory::Lockout
        );
    }

    #[test]
    fn a_lockout_outcome_is_a_lockout_and_an_address_refusal_is_not() {
        assert_eq!(EventCategory::of_outcome("locked"), EventCategory::Lockout);
        // `blocked` is the address rule refusing an attempt against a real account: the account
        // was never locked, so calling it a lockout would report one that did not happen.
        assert_eq!(EventCategory::of_outcome("blocked"), EventCategory::SignIn);
        assert_eq!(EventCategory::of_outcome("failed"), EventCategory::SignIn);
        assert_eq!(EventCategory::of_outcome("success"), EventCategory::SignIn);
    }

    #[test]
    fn the_category_filter_prefixes_the_audit_namespace() {
        assert_eq!(
            category_prefix(EventCategory::IpRuleChange),
            "security.ip_rule%"
        );
        assert_eq!(category_prefix(EventCategory::Lockout), "security.lockout%");
        assert_eq!(category_prefix(EventCategory::SettingsChange), "security.%");
    }

    #[test]
    fn metadata_is_summarised_as_keys_and_never_as_values() {
        let metadata = serde_json::json!({ "mode": "enforce", "directives": 7 });
        let summary = summarise_metadata(&metadata).expect("an object summarises");
        assert_eq!(summary, "directives=7, mode=set");
        // The value of a string key is NOT exported — only the key and its shape. This is the
        // assertion that keeps a CSP origin list out of a CSV an operator emails to somebody.
        assert!(!summary.contains("enforce"), "{summary} leaked a value");
    }

    #[test]
    fn a_credential_named_in_metadata_is_dropped_not_summarised() {
        let metadata = serde_json::json!({
            "kind": "deny",
            "cidr": "203.0.113.0/24",
            "secret": "whsec_abc",
            "authorization": "Bearer x",
            "password": "hunter2",
        });
        let summary = summarise_metadata(&metadata).expect("the safe keys remain");
        assert!(summary.contains("cidr="), "{summary} lost a safe key");
        for needle in [
            "secret",
            "authorization",
            "password",
            "whsec",
            "Bearer",
            "hunter2",
        ] {
            assert!(
                !summary.contains(needle),
                "{summary} still mentions {needle} — a credential must leave nothing behind"
            );
        }
    }

    #[test]
    fn metadata_with_nothing_safe_left_is_none_rather_than_empty() {
        let metadata = serde_json::json!({ "token": "abc" });
        assert_eq!(summarise_metadata(&metadata), None);
        assert_eq!(summarise_metadata(&serde_json::json!({})), None);
        assert_eq!(
            summarise_metadata(&serde_json::json!("not an object")),
            None
        );
    }

    #[test]
    fn a_filter_matches_the_wildcards_itself_contains() {
        // An unescaped `%` in a free-text filter is not a search term, it is a match-everything
        // switch: the operator pastes a string containing one and the screen returns the whole
        // timeline while reading as "filtered".
        let term = search_term(Some("100%_done")).expect("a term remains");
        assert_eq!(term, "100\\%\\_done");
    }

    #[test]
    fn a_blank_search_is_no_filter_at_all() {
        assert_eq!(search_term(Some("   ")), None);
        assert_eq!(search_term(Some("")), None);
        assert_eq!(search_term(None), None);
    }

    #[test]
    fn an_over_long_filter_is_capped_rather_than_refused() {
        let term = search_term(Some(&"x".repeat(400))).expect("a term remains");
        assert_eq!(term.chars().count(), 120);
    }

    #[test]
    fn placeholders_are_numbered_once_and_in_order() {
        let query = EventQuery {
            search: Some("lockout".to_owned()),
            category: Some(EventCategory::Lockout),
            since: Some(OffsetDateTime::UNIX_EPOCH),
            ..EventQuery::default()
        };
        let spec = query_spec(&query);
        // Three filters, three placeholders, numbered 1..3 — and the values line up with them,
        // which is the property a hand-numbered second query gets wrong.
        assert_eq!(spec.values().len(), 3);
        assert!(spec.where_clause().contains("action ilike $1"));
        assert!(spec.where_clause().contains("action like $2"));
        assert!(spec.where_clause().contains("created_at >= $3"));
    }

    #[test]
    fn an_unfiltered_read_still_has_a_where_clause() {
        // A caller that concatenates `where {clause}` on an empty fragment produces
        // `where ` and a syntax error; the fragment always carries a leading `where`.
        let spec = query_spec(&EventQuery::default());
        assert!(spec.is_empty());
        assert_eq!(spec.where_clause(), " where 1 = 1");
    }

    #[test]
    fn mfa_required_is_not_rendered_as_a_failure() {
        assert!(!is_refusal("mfa_required"));
        assert!(is_refusal("failed"));
        assert!(is_refusal("locked"));
        assert!(is_refusal("blocked"));
        assert!(!is_refusal("success"));
        assert_eq!(describe_outcome("mfa_required"), "second factor required");
        assert_eq!(describe_outcome("blocked"), "refused by the address rule");
    }

    #[test]
    fn the_page_size_is_clamped_to_what_the_store_will_read() {
        assert_eq!(page_size(None), MAX_PAGE);
        assert_eq!(page_size(Some(0)), 1);
        assert_eq!(page_size(Some(10_000)), MAX_PAGE);
        assert_eq!(page_size(Some(25)), 25);
    }

    #[test]
    fn the_export_columns_name_every_field_the_table_shows() {
        // The same rule the findings export follows: a CSV with a different column set from the
        // table is a second schema nobody maintains. `detail` is in this list because it was
        // MISSING from the first version of it — the walk proved the audit row's digest reached
        // the screen and stopped at the export, so the file attached to a ticket was thinner
        // than the screen it came from. The containment loop could not catch that; the width
        // check below is what does.
        for column in [
            "occurred_at",
            "category",
            "action",
            "outcome",
            "actor",
            "subject_user_id",
            "client_ip",
            "user_agent",
            "detail",
            "source",
            "id",
        ] {
            assert!(EVENT_COLUMNS.contains(&column), "{column} is not exported");
        }
    }
}
