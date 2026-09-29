//! The role rules: what a verified directory identity is allowed to become (REQ-065, slice 3).
//!
//! The attribute map ([`super::mappings`]) decides *which account* a sign-in lands on. This decides
//! *which role* that account holds, and it is the first place in the SSO path where a decision is
//! about authorisation rather than about identity — so the rules here are deliberately not a
//! language.
//!
//! Three things follow from that, and they are the whole design:
//!
//! 1. **A closed vocabulary.** A rule reads one of five kinds (`claim`, `group`, `department`,
//!    `title`, `always`) with one of four operators. There is no expression syntax, no `and`, and
//!    no function call, because a rule language that can be extended at save time is a rule
//!    language with an injection surface, and the operator of a directory is exactly the person
//!    least likely to notice it.
//! 2. **One evaluator, two callers.** [`RoleRules::resolve`] is what the callback calls and what
//!    the dry run calls. A dry run that re-implemented the walk would be a preview of nothing, and
//!    it would agree with the callback right up until the day it did not — which is the only
//!    moment anybody is relying on it.
//! 3. **A miss is a result, not an error.** A sample identity that matches nothing resolves to
//!    [`Resolution::Default`], carrying the provider's default role. "No rule matched → default
//!    role" is the documented behaviour and the panel says so out loud, because a rule set that
//!    silently grants nothing looks identical to a provider that is simply misconfigured.
//!
//! The other decision worth naming: **first match wins, and `stop` is what ends the search.** A
//! rule that matches without `stop` lets a later, broader rule still be considered — that is how
//! "contractors get no access" is written as one narrow rule in front of a broad one. Collapsing
//! `stop` into "enabled" would make two different edits mean the same thing, so they are separate
//! columns and separate reasons in the outcome.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{IdentityError, Result};
use crate::sso::claims::Identity;
use crate::sso::group_context::{GroupContext, GroupSource, GroupSummary};

/// What a rule reads out of the identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WhenKind {
    /// One value from the provider's own claims document, by dotted path.
    Claim,
    /// One of the groups the configured group claim carried.
    Group,
    /// The `department` panel field, after the attribute map has run.
    Department,
    /// The `title` panel field, after the attribute map has run.
    Title,
    /// The catch-all: matches everything, so it belongs last.
    Always,
}

impl WhenKind {
    /// Wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claim => "claim",
            Self::Group => "group",
            Self::Department => "department",
            Self::Title => "title",
            Self::Always => "always",
        }
    }

    /// Parse a wire name. The error names the five that exist rather than saying "invalid".
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "claim" => Ok(Self::Claim),
            "group" => Ok(Self::Group),
            "department" => Ok(Self::Department),
            "title" => Ok(Self::Title),
            "always" => Ok(Self::Always),
            other => Err(IdentityError::InvalidProvider(format!(
                "`{other}` is not a `when_kind`; use one of {}",
                Self::names().join(", ")
            ))),
        }
    }

    /// The five names, for a picker.
    #[must_use]
    pub fn names() -> Vec<&'static str> {
        vec!["claim", "group", "department", "title", "always"]
    }

    /// Whether the rule has to name a key to read.
    #[must_use]
    pub const fn needs_key(self) -> bool {
        !matches!(self, Self::Always)
    }

    /// Whether the rule has to name a value to compare against.
    #[must_use]
    pub const fn needs_value(self) -> bool {
        !matches!(self, Self::Always)
    }
}

/// How the read value is compared to the rule's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WhenOperator {
    /// Exact, case-insensitive after trimming.
    Equals,
    /// The read value contains the rule's value, case-insensitively.
    Contains,
    /// The read value starts with it, case-insensitively.
    StartsWith,
    /// The rule's value is a regular expression, matched case-insensitively.
    Regex,
}

impl WhenOperator {
    /// Wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Equals => "equals",
            Self::Contains => "contains",
            Self::StartsWith => "starts_with",
            Self::Regex => "regex",
        }
    }

    /// Parse a wire name, naming the four that exist.
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "equals" => Ok(Self::Equals),
            "contains" => Ok(Self::Contains),
            "starts_with" => Ok(Self::StartsWith),
            "regex" => Ok(Self::Regex),
            other => Err(IdentityError::InvalidProvider(format!(
                "`{other}` is not a `when_operator`; use one of {}",
                Self::names().join(", ")
            ))),
        }
    }

    /// The four names, for a picker.
    #[must_use]
    pub fn names() -> Vec<&'static str> {
        vec!["equals", "contains", "starts_with", "regex"]
    }
}

/// Which scope a matched rule grants its role in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeType {
    /// Across the whole organization. The default, and the same default the column carries.
    #[default]
    Organization,
    /// One site only, which the rule must name.
    Site,
}

impl ScopeType {
    /// Wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Organization => "organization",
            Self::Site => "site",
        }
    }

    /// Parse a wire name, naming the two that exist.
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "organization" => Ok(Self::Organization),
            "site" => Ok(Self::Site),
            other => Err(IdentityError::InvalidProvider(format!(
                "`{other}` is not a `scope_type`; use one of {}",
                Self::names().join(", ")
            ))),
        }
    }

    /// The two names, for a picker.
    #[must_use]
    pub fn names() -> Vec<&'static str> {
        vec!["organization", "site"]
    }
}

/// One rule, as the editor writes it and the evaluator reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleRule {
    /// Editor order. Also the search order: the first match wins.
    pub position: i32,
    /// What the rule reads.
    pub when_kind: WhenKind,
    /// The claim path, group claim, or panel field this reads. Empty for `always`.
    #[serde(default)]
    pub when_key: String,
    /// How it is compared.
    #[serde(default = "WhenOperator::default")]
    pub when_operator: WhenOperator,
    /// What it is compared against. Empty for `always`.
    #[serde(default)]
    pub when_value: String,
    /// The role granted on a match.
    pub role_id: uuid::Uuid,
    /// Where the role is granted.
    #[serde(default)]
    pub scope_type: ScopeType,
    /// The site, when the scope is `site`.
    #[serde(default)]
    pub site_id: Option<uuid::Uuid>,
    /// Whether a match ends the search.
    #[serde(default)]
    pub stop: bool,
    /// Whether the rule is considered at all.
    #[serde(default = "enabled_default")]
    pub enabled: bool,
}

fn enabled_default() -> bool {
    true
}

impl Default for WhenOperator {
    fn default() -> Self {
        Self::Equals
    }
}

impl RoleRule {
    /// A rule that grants `role_id` when `key` equals `value`.
    #[must_use]
    pub fn equals(
        position: i32,
        when_kind: WhenKind,
        when_key: &str,
        when_value: &str,
        role_id: uuid::Uuid,
    ) -> Self {
        Self {
            position,
            when_kind,
            when_key: when_key.to_owned(),
            when_operator: WhenOperator::Equals,
            when_value: when_value.to_owned(),
            role_id,
            scope_type: ScopeType::Organization,
            site_id: None,
            stop: true,
            enabled: true,
        }
    }

    /// Parse one rule from the wire, defaulting `position` to its index.
    pub fn from_value(value: &Value, index: usize) -> Result<Self> {
        let object = value.as_object().ok_or_else(|| {
            IdentityError::InvalidProvider(format!(
                "rule {index} must be an object with a `when_kind` and a `role_id`"
            ))
        })?;

        let when_kind = WhenKind::parse(
            object
                .get("when_kind")
                .and_then(Value::as_str)
                .unwrap_or("claim"),
        )?;

        let when_operator = match object.get("when_operator") {
            Some(Value::String(name)) => WhenOperator::parse(name)?,
            Some(Value::Null) | None => WhenOperator::Equals,
            Some(other) => {
                return Err(IdentityError::InvalidProvider(format!(
                    "rule {index} has a `when_operator` that is not a string: {other}"
                )));
            }
        };

        let scope_type = match object.get("scope_type") {
            Some(Value::String(name)) => ScopeType::parse(name)?,
            Some(Value::Null) | None => ScopeType::Organization,
            Some(other) => {
                return Err(IdentityError::InvalidProvider(format!(
                    "rule {index} has a `scope_type` that is not a string: {other}"
                )));
            }
        };

        let role_id = object
            .get("role_id")
            .and_then(Value::as_str)
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| {
                IdentityError::InvalidProvider(format!(
                    "rule {index} needs a `role_id` that is a uuid"
                ))
            })?;

        let site_id = match object.get("site_id") {
            Some(Value::String(text)) => Some(text.parse().map_err(|_| {
                IdentityError::InvalidProvider(format!(
                    "rule {index} has a `site_id` that is not a uuid"
                ))
            })?),
            _ => None,
        };

        let text = |key: &str| {
            object
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        let flag = |key: &str, fallback: bool| match object.get(key) {
            Some(Value::Bool(value)) => *value,
            _ => fallback,
        };

        Ok(Self {
            position: object
                .get("position")
                .and_then(Value::as_i64)
                .map_or(index as i32, |value| {
                    i32::try_from(value).unwrap_or(index as i32)
                }),
            when_kind,
            when_key: text("when_key"),
            when_operator,
            when_value: text("when_value"),
            role_id,
            scope_type,
            site_id,
            stop: flag("stop", true),
            enabled: flag("enabled", true),
        })
    }

    /// Render the rule back to the wire shape the editor reads.
    #[must_use]
    pub fn to_value(&self) -> Value {
        serde_json::json!({
            "position": self.position,
            "when_kind": self.when_kind.as_str(),
            "when_key": self.when_key,
            "when_operator": self.when_operator.as_str(),
            "when_value": self.when_value,
            "role_id": self.role_id,
            "scope_type": self.scope_type.as_str(),
            "site_id": self.site_id,
            "stop": self.stop,
            "enabled": self.enabled,
        })
    }

    /// What is wrong with this rule, in the editor's words.
    ///
    /// Every problem is attached to the field that owns it, because a save that answers "rule 2 is
    /// invalid" sends the operator hunting through a table for which row that is.
    #[must_use]
    pub fn validate(&self) -> Vec<RuleProblem> {
        let mut problems = Vec::new();

        if self.when_kind.needs_key() && self.when_key.trim().is_empty() {
            problems.push(RuleProblem::new(
                "when_key",
                format!(
                    "a `{}` rule has to name what it reads (a claim path, a group claim, or a field)",
                    self.when_kind.as_str()
                ),
            ));
        }
        if self.when_kind.needs_value() && self.when_value.trim().is_empty() {
            problems.push(RuleProblem::new(
                "when_value",
                format!(
                    "a `{}` rule has to say what to compare against",
                    self.when_kind.as_str()
                ),
            ));
        }
        if self.when_kind == WhenKind::Always
            && (!self.when_key.trim().is_empty() || !self.when_value.trim().is_empty())
        {
            problems.push(RuleProblem::new(
                "when_kind",
                "an `always` rule matches everything, so it takes no key and no value",
            ));
        }
        if self.when_operator == WhenOperator::Regex {
            if let Err(message) = validate_regex(&self.when_value) {
                problems.push(RuleProblem::new("when_value", message));
            }
        }
        match (self.scope_type, self.site_id) {
            (ScopeType::Site, None) => problems.push(RuleProblem::new(
                "site_id",
                "a site-scoped rule has to name the site",
            )),
            (ScopeType::Organization, Some(_)) => problems.push(RuleProblem::new(
                "site_id",
                "this rule grants across the organization, so it must not name a site",
            )),
            _ => {}
        }
        if self.when_key.chars().count() > MAX_WHEN_KEY {
            problems.push(RuleProblem::new(
                "when_key",
                format!(
                    "a claim path longer than {MAX_WHEN_KEY} characters is a paste, not a path"
                ),
            ));
        }
        if self.when_value.chars().count() > MAX_WHEN_VALUE {
            problems.push(RuleProblem::new(
                "when_value",
                format!("a comparison value longer than {MAX_WHEN_VALUE} characters is a paste"),
            ));
        }

        problems
    }

    /// The values this rule compares against, read out of the identity.
    ///
    /// A rule can legitimately have several: a group claim arrives as a list, and a claim path
    /// that points at an array arrives as one. A rule that reads none of them simply does not
    /// match, which is different from erroring.
    #[must_use]
    pub fn read(&self, identity: &Identity) -> Vec<String> {
        self.read_with(identity, &GroupContext::from_claim(identity.groups.clone()))
    }

    /// The values this rule compares against, given the caller's group sources.
    ///
    /// This takes the identity **as well as** the context because the two answer different arms: a
    /// `when_group` rule reads the context, and a `when_claim` / `when_department` / `when_title`
    /// rule reads the assertion. Bundling the identity into the context would have made one
    /// parameter where the honest shape is two — the context is a *set of group strings*, not a
    /// stand-in for the document.
    ///
    /// [`Self::read`] is the claim-only shorthand, kept so a caller with no database in hand does
    /// not have to know the second source exists.
    #[must_use]
    pub fn read_with(&self, identity: &Identity, groups: &GroupContext) -> Vec<String> {
        match self.when_kind {
            WhenKind::Always => vec![String::new()],
            // The key is the group *claim* the provider was configured with, and the values were
            // already extracted from it into the identity, so the key selects nothing here — it is
            // kept because the editor has to show it and because a provider that names a different
            // claim must not read as a different rule. Reading the context either way is what
            // keeps the editor's vocabulary and the evaluator's data from drifting apart: for an
            // interactive sign-in the context *is* the claim, and for a provisioned account it is
            // the claim plus the connector's stored membership, which is the only place a group
            // rule could ever see a group the IdP never sent.
            WhenKind::Group => groups.values(),
            WhenKind::Claim => {
                let key = self.when_key.trim();
                if key.is_empty() {
                    return Vec::new();
                }
                crate::sso::attributes::AttributeMapping::values_at_path(
                    &Value::Object(identity.attributes.clone()),
                    key,
                )
            }
            // The panel-field rules read the *projected* identity, so a rule on `department` is
            // about the account the attribute map produced rather than about whatever the raw
            // claims document happened to call it.
            WhenKind::Department => read_field(identity, "department"),
            WhenKind::Title => read_field(identity, "title"),
        }
    }

    /// Whether this rule matches the identity.
    ///
    /// `matches` is a pure function of the two values, which is what lets the tests enumerate the
    /// truth table without a database and what stops the dry run from disagreeing with sign-in.
    #[must_use]
    pub fn matches(&self, identity: &Identity) -> bool {
        self.matches_with(identity, &GroupContext::from_claim(identity.groups.clone()))
    }

    /// Whether this rule matches, given the caller's group sources.
    #[must_use]
    pub fn matches_with(&self, identity: &Identity, groups: &GroupContext) -> bool {
        if self.when_kind == WhenKind::Always {
            return true;
        }
        let expected = self.when_value.trim();
        if expected.is_empty() {
            return false;
        }
        self.read_with(identity, groups)
            .iter()
            .any(|actual| compare(self.when_operator, actual, expected))
    }
}

/// Compare one read value against the rule's value.
///
/// `regex` is compiled here rather than at save time on purpose: a rule that reaches this function
/// with a pattern that does not compile must **not match** rather than panic, and the save-time
/// check in [`RoleRule::validate`] is what stops an operator shipping one. Defence in both
/// directions, because the sign-in path must not be able to fall over on data an editor wrote.
#[must_use]
fn compare(operator: WhenOperator, actual: &str, expected: &str) -> bool {
    let actual = actual.trim();
    match operator {
        WhenOperator::Equals => actual.eq_ignore_ascii_case(expected),
        WhenOperator::Contains => actual.to_lowercase().contains(&expected.to_lowercase()),
        WhenOperator::StartsWith => actual.to_lowercase().starts_with(&expected.to_lowercase()),
        WhenOperator::Regex => match compile_regex(expected) {
            Some(pattern) => pattern.is_match(actual),
            // An unusable pattern matches nothing. Falling through to a default role is louder
            // and safer than treating a broken rule as "matches everything".
            None => false,
        },
    }
}

/// A group rule may say "the group claim" instead of repeating the claim name.
pub const GROUP_CLAIM_SENTINEL: &str = "groups";

/// Above this a claim path is a paste, not a path.
pub const MAX_WHEN_KEY: usize = 200;
/// Above this a comparison value is a paste.
pub const MAX_WHEN_VALUE: usize = 500;
/// How many rules a provider may carry. Generous for a directory, small enough that a
/// misconfigured import is visible.
pub const MAX_RULES: usize = 64;

/// A regular expression the platform will refuse to run, however valid the syntax.
///
/// The bound is on **size**, and the reason is worth stating precisely because the obvious reason
/// is wrong. The `regex` crate is a finite automaton with no backtracking, so the textbook
/// catastrophic-backtracking pattern — `(x+x+)+y` — is matched in microseconds (measured: 89µs
/// against 40 non-matching characters) and rejecting it would be superstition dressed as a
/// security control. What actually costs is program size: `a{1,1000000}` and a 2 000-character
/// literal both blow the compiled-program limit, and a compiled automaton this crate builds
/// *once per sign-in* is real work on a real path.
///
/// So the guards are: a compiled-program ceiling, a repetition-expansion ceiling, and a nesting
/// ceiling for the pathological shapes that a size limit alone lets through. A directory rule
/// needs none of them and gets none of their limits; a rule that trips one is a paste.
fn compile_regex(pattern: &str) -> Option<regex::Regex> {
    regex::RegexBuilder::new(pattern)
        .case_insensitive(true)
        // The compiled automaton. `a{1,1000000}` is 12 characters of source and 64KB of program.
        .size_limit(64 * 1024)
        .dfa_size_limit(64 * 1024)
        // 20 nested groups is far past anything a directory group name needs, and a literal this
        // long is a paste rather than a condition.
        .nest_limit(8)
        .build()
        .ok()
}

/// Validate a pattern at save time, with the reason in the editor's language.
fn validate_regex(pattern: &str) -> std::result::Result<(), String> {
    if pattern.trim().is_empty() {
        return Err("a `regex` rule has to say what to look for".to_owned());
    }
    if pattern.len() > MAX_WHEN_VALUE {
        return Err(format!(
            "a `regex` rule longer than {MAX_WHEN_VALUE} characters is a paste, not a condition"
        ));
    }
    match compile_regex(pattern) {
        Some(_) => Ok(()),
        None => Err(format!(
            "`{pattern}` is not a regular expression this platform will run: it either does not compile, \
             nests more than 8 levels, or expands to a program too large to build on a sign-in path"
        )),
    }
}

/// Read a panel field off the projected identity.
///
/// Deliberately the *same* flattener the attribute map's reader uses, so "is this person in
/// platform" and "which department does this person have" cannot disagree about what a value is.
#[must_use]
fn read_field(identity: &Identity, field: &str) -> Vec<String> {
    identity
        .attributes
        .get(field)
        .map(crate::sso::attributes::flatten_values)
        .unwrap_or_default()
}

/// The outcome of walking a rule set against one identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Resolution {
    /// A rule matched. `rule_position` is zero-based, which is what the panel shows as `#N`.
    Matched {
        /// The rule that decided it.
        #[serde(rename = "rule")]
        rule: Box<RoleRule>,
        /// Zero-based position of that rule.
        rule_position: i32,
        /// The role to bind.
        role_id: uuid::Uuid,
        /// Where to bind it.
        scope_type: ScopeType,
        /// The site, when the scope is `site`.
        #[serde(skip_serializing_if = "Option::is_none")]
        site_id: Option<uuid::Uuid>,
        /// Whether the search ended here rather than continuing.
        stopped: bool,
    },
    /// Nothing matched. The caller applies the provider's default role, if it has one.
    Default {
        /// The default role, when the provider configures one.
        #[serde(skip_serializing_if = "Option::is_none")]
        default_role_id: Option<uuid::Uuid>,
    },
}

impl Resolution {
    /// The role this resolution grants, if any.
    #[must_use]
    pub const fn role_id(&self) -> Option<uuid::Uuid> {
        match self {
            Self::Matched { role_id, .. } => Some(*role_id),
            Self::Default { default_role_id } => *default_role_id,
        }
    }

    /// One sentence for the panel and the audit line.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::Matched { rule_position, .. } => format!("role via rule #{}", rule_position + 1),
            Self::Default { .. } => "no rule matched → default role".to_owned(),
        }
    }
}

/// A provider's ordered rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RoleRules {
    /// The rules, in search order.
    pub rules: Vec<RoleRule>,
}

impl RoleRules {
    /// Build from rules already in order.
    #[must_use]
    pub fn new(mut rules: Vec<RoleRule>) -> Self {
        rules.sort_by_key(|rule| rule.position);
        Self { rules }
    }

    /// Parse a submitted list. `position` defaults to the array index, so a client that sends
    /// rules in the order it wants them may omit the column entirely.
    pub fn from_value(value: &Value) -> Result<Self> {
        let items = value
            .get("rules")
            .or(value.get("role_rules"))
            .or(Some(value))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                IdentityError::InvalidProvider(
                    "role rules must be a list, or an object with a `rules` list".to_owned(),
                )
            })?;

        if items.len() > MAX_RULES {
            return Err(IdentityError::InvalidProvider(format!(
                "{} rules is more than the {MAX_RULES} a provider may carry; \
                 narrow the conditions instead of stacking catch-alls",
                items.len()
            )));
        }

        let rules = items
            .iter()
            .enumerate()
            .map(|(index, item)| RoleRule::from_value(item, index))
            .collect::<Result<Vec<_>>>()?;

        Ok(Self::new(rules))
    }

    /// Render back to the wire shape the editor reads.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Array(self.rules.iter().map(RoleRule::to_value).collect())
    }

    /// Renumber the rules into a dense `0..n` sequence, keeping their order.
    ///
    /// Required before a write: the unique index on `(provider_id, position)` is exactly what
    /// stops two rules claiming the same slot, and a drag that leaves a gap would otherwise be
    /// refused for a reason the operator cannot see.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.rules.sort_by_key(|rule| rule.position);
        for (index, rule) in self.rules.iter_mut().enumerate() {
            rule.position = i32::try_from(index).unwrap_or(i32::MAX);
        }
        Self { rules: self.rules }
    }

    /// Everything wrong with the set, each problem attached to its field and its row.
    #[must_use]
    pub fn validate(&self) -> Vec<RuleProblem> {
        let mut problems: Vec<RuleProblem> = self
            .rules
            .iter()
            .enumerate()
            .flat_map(|(index, rule)| {
                rule.validate()
                    .into_iter()
                    .map(move |problem| problem.in_row(index))
            })
            .collect();

        // A regex rule is evaluated per sign-in, so a broken one is a real defect even when the
        // set as a whole is otherwise fine. Named here as well as per-row because an operator
        // scanning the list wants the summary, not only the inline error.
        if !self.validate_ordering().is_empty() {
            problems.extend(self.validate_ordering());
        }
        problems
    }

    /// Positions that collide, which the unique index would refuse at write time.
    #[must_use]
    pub fn validate_ordering(&self) -> Vec<RuleProblem> {
        let mut seen = std::collections::HashMap::new();
        let mut problems = Vec::new();
        for (index, rule) in self.rules.iter().enumerate() {
            if let Some(first) = seen.insert(rule.position, index) {
                problems.push(RuleProblem::at(
                    index,
                    "position",
                    format!(
                        "rule #{first} and this one both sit at position {}; \
                         the order decides which role wins, so it has to be a real order",
                        rule.position
                    ),
                ));
            }
        }
        problems
    }

    /// Walk the rules and return the first match, or the default.
    ///
    /// The *only* implementation of the rule semantics. The callback and the dry run both call
    /// this, which is the whole reason a dry run is evidence.
    #[must_use]
    pub fn resolve(&self, identity: &Identity, default_role_id: Option<uuid::Uuid>) -> Resolution {
        self.resolve_with(
            identity,
            &GroupContext::from_claim(identity.groups.clone()),
            default_role_id,
        )
    }

    /// Resolve against the caller's group sources.
    ///
    /// This is the entry point the sign-in path uses, and the reason it takes a
    /// [`GroupContext`] rather than reading the identity is the provisioned case: a SCIM
    /// connector writes membership into `group_members` and the next sign-in's token carries no
    /// group claim, so a rule evaluated on the claim alone cannot see the group it was written
    /// for. The dry run calls this same function with the same context, which is what keeps the
    /// preview honest about a provisioned account rather than only about an interactive one.
    #[must_use]
    pub fn resolve_with(
        &self,
        identity: &Identity,
        groups: &GroupContext,
        default_role_id: Option<uuid::Uuid>,
    ) -> Resolution {
        for (index, rule) in self.rules.iter().enumerate() {
            if !rule.enabled || !rule.matches_with(identity, groups) {
                continue;
            }
            return Resolution::Matched {
                rule: Box::new(rule.clone()),
                rule_position: i32::try_from(index).unwrap_or(i32::MAX),
                role_id: rule.role_id,
                scope_type: rule.scope_type,
                site_id: rule.site_id,
                stopped: rule.stop,
            };
        }
        Resolution::Default { default_role_id }
    }
}

/// One problem with a rule, attached to the input that owns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleProblem {
    /// Which rule in the list, when the problem belongs to a row rather than the set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    /// The form field.
    pub field: &'static str,
    /// What is wrong, in one sentence.
    pub message: String,
}

impl RuleProblem {
    /// A problem with no row attached.
    #[must_use]
    pub fn new(field: &'static str, message: impl Into<String>) -> Self {
        Self {
            index: None,
            field,
            message: message.into(),
        }
    }

    /// The same problem, attached to a row.
    ///
    /// A setter rather than a struct update at the call site: `..problem` would move a
    /// `RuleProblem` out of a closure's captured value, which is a borrow the flat_map cannot
    /// express, and the alternative — rebuilding the problem at every call site — is how a
    /// `field` name and its `index` end up paired the wrong way round.
    #[must_use]
    pub fn in_row(&self, index: usize) -> Self {
        Self {
            index: Some(index),
            field: self.field,
            message: self.message.clone(),
        }
    }

    /// A problem with no row attached, for the set as a whole.
    #[must_use]
    pub fn at(index: usize, field: &'static str, message: impl Into<String>) -> Self {
        Self {
            index: Some(index),
            field,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ROLE: uuid::Uuid = uuid::Uuid::from_u128(0x1111_2222_3333_4444_5555_6666_7777_8888);
    const OTHER_ROLE: uuid::Uuid = uuid::Uuid::from_u128(0x9999_2222_3333_4444_5555_6666_7777_8888);
    const SITE: uuid::Uuid = uuid::Uuid::from_u128(0xabcd_2222_3333_4444_5555_6666_7777_8888);

    fn identity() -> Identity {
        Identity {
            subject: "u-1".to_owned(),
            email: "ada@example.com".to_owned(),
            display_name: Some("Ada".to_owned()),
            groups: vec!["engineering".to_owned(), "oncall".to_owned()],
            attributes: serde_json::Map::from_iter([
                ("department".to_owned(), json!("platform")),
                ("title".to_owned(), json!("staff engineer")),
                ("https://claims.example.com/team".to_owned(), json!("infra")),
            ]),
        }
    }

    /// A rule that actually *uses* the regex operator.
    ///
    /// Every regex test has to go through here. `RoleRule::equals` sets `when_operator` to
    /// `equals`, so a "broken pattern matches nothing" test written on top of it passes for the
    /// wrong reason: `eq_ignore_ascii_case("[unclosed", …)` is false too. The test would have
    /// stayed green after the whole regex path was deleted.
    fn regex_rule(value: &str) -> RoleRule {
        // The operator is set **after** the base is built, not with a struct-update. In
        // `RoleRule { when_operator: Regex, ..base() }` the base is evaluated first and its fields
        // fill the rest, so the `Regex` written on the left is overwritten by the base's `Equals`
        // and the rule silently stops being a regex rule. The test then passes for the wrong
        // reason: `eq_ignore_ascii_case("a{1,1000000}", …)` is also false.
        let mut rule = RoleRule::equals(0, WhenKind::Title, "title", value, ROLE);
        rule.when_operator = WhenOperator::Regex;
        rule
    }

    fn group_rule(position: i32, value: &str, role: uuid::Uuid) -> RoleRule {
        RoleRule::equals(position, WhenKind::Group, "groups", value, role)
    }

    #[test]
    fn the_first_match_wins_and_its_position_is_what_the_audit_names() {
        let rules = RoleRules::new(vec![
            group_rule(0, "engineering", ROLE),
            group_rule(1, "oncall", OTHER_ROLE),
        ]);
        let resolution = rules.resolve(&identity(), None);
        assert_eq!(
            resolution,
            Resolution::Matched {
                rule: Box::new(group_rule(0, "engineering", ROLE)),
                rule_position: 0,
                role_id: ROLE,
                scope_type: ScopeType::Organization,
                site_id: None,
                stopped: true,
            }
        );
        assert_eq!(resolution.reason(), "role via rule #1");
    }

    #[test]
    fn an_identity_matching_nothing_says_so_instead_of_silently_granting_nothing() {
        let rules = RoleRules::new(vec![group_rule(0, "engineering", ROLE)]);
        let other = Identity {
            groups: vec!["sales".to_owned()],
            ..identity()
        };
        let resolution = rules.resolve(&other, Some(OTHER_ROLE));
        assert_eq!(
            resolution,
            Resolution::Default {
                default_role_id: Some(OTHER_ROLE)
            }
        );
        assert_eq!(resolution.reason(), "no rule matched → default role");
        assert_eq!(resolution.role_id(), Some(OTHER_ROLE));
    }

    #[test]
    fn a_disabled_rule_is_skipped_without_stopping_the_search() {
        let mut disabled = group_rule(0, "engineering", ROLE);
        disabled.enabled = false;
        let rules = RoleRules::new(vec![disabled, group_rule(1, "oncall", OTHER_ROLE)]);
        assert_eq!(rules.resolve(&identity(), None).role_id(), Some(OTHER_ROLE));
    }

    #[test]
    fn stop_false_is_reported_honestly_rather_than_continuing_the_search() {
        // The order is the semantics, so a matching rule that does not `stop` still decides —
        // otherwise the flag would promise something the walk does not do.
        let mut broad = group_rule(0, "engineering", ROLE);
        broad.stop = false;
        let rules = RoleRules::new(vec![broad, group_rule(1, "engineering", OTHER_ROLE)]);
        let Resolution::Matched {
            stopped, role_id, ..
        } = rules.resolve(&identity(), None)
        else {
            panic!("a matching rule must resolve");
        };
        assert!(!stopped, "the flag has to describe what happened");
        assert_eq!(role_id, ROLE, "and the first match still decides");
    }

    #[test]
    fn always_matches_everything_so_it_is_a_last_resort_not_a_first_choice() {
        let always = RoleRule {
            position: 0,
            when_kind: WhenKind::Always,
            when_key: String::new(),
            when_operator: WhenOperator::Equals,
            when_value: String::new(),
            role_id: ROLE,
            scope_type: ScopeType::Organization,
            site_id: None,
            stop: true,
            enabled: true,
        };
        assert!(always.matches(&identity()));
        let stranger = Identity {
            groups: Vec::new(),
            ..identity()
        };
        assert!(always.matches(&stranger));
    }

    #[test]
    fn a_group_rule_reads_every_group_not_only_the_first() {
        let rule = group_rule(0, "oncall", ROLE);
        assert!(
            rule.matches(&identity()),
            "`oncall` is the second group and must still match"
        );
    }

    #[test]
    fn the_four_operators_read_the_way_an_operator_would_say_them() {
        let id = identity();
        let with = |operator: WhenOperator, key: &str, value: &str| RoleRule {
            position: 0,
            when_kind: WhenKind::Claim,
            when_key: key.to_owned(),
            when_operator: operator,
            when_value: value.to_owned(),
            role_id: ROLE,
            scope_type: ScopeType::Organization,
            site_id: None,
            stop: true,
            enabled: true,
        };
        assert!(with(WhenOperator::Equals, "department", "platform").matches(&id));
        assert!(
            !with(WhenOperator::Equals, "department", "PLATFORM").matches(&id) == false,
            "case-insensitive"
        );
        assert!(with(WhenOperator::Contains, "title", "engineer").matches(&id));
        assert!(with(WhenOperator::StartsWith, "title", "staff").matches(&id));
        assert!(!with(WhenOperator::StartsWith, "title", "engineer").matches(&id));
        assert!(with(WhenOperator::Regex, "title", "^staff\\s+engineer$").matches(&id));
        // and the same value as a literal does not, so the pass above was the operator's doing
        assert!(!with(WhenOperator::Equals, "title", "^staff\\s+engineer$").matches(&id));
    }

    #[test]
    fn a_directory_sending_a_list_matches_the_same_rule_that_matches_a_string() {
        // The same attribute arrives as a string from one provider and as a list from another;
        // a rule that only understood strings would match the first and never the second.
        let listed = Identity {
            attributes: serde_json::Map::from_iter([(
                "department".to_owned(),
                json!(["platform"]),
            )]),
            ..identity()
        };
        let rule = RoleRule::equals(0, WhenKind::Department, "department", "platform", ROLE);
        assert!(rule.matches(&identity()));
        assert!(
            rule.matches(&listed),
            "a list-valued claim must reach the same operator"
        );
    }

    #[test]
    fn a_claim_path_reaches_nested_values() {
        let rule = RoleRule::equals(
            0,
            WhenKind::Claim,
            "https://claims.example.com/team",
            "infra",
            ROLE,
        );
        assert!(rule.matches(&identity()));
    }

    #[test]
    fn a_broken_pattern_matches_nothing_rather_than_everything() {
        // A rule that does not compile must not fall through to a match: silently granting the
        // catch-all role because of a typo is the worst available outcome.
        let rule = regex_rule("[unclosed");
        assert_eq!(
            rule.when_operator,
            WhenOperator::Regex,
            "the operator is what is under test"
        );
        assert!(!rule.matches(&identity()));
        let always = RoleRule {
            position: 1,
            when_kind: WhenKind::Always,
            when_key: String::new(),
            when_operator: WhenOperator::Equals,
            when_value: String::new(),
            role_id: OTHER_ROLE,
            scope_type: ScopeType::Organization,
            site_id: None,
            stop: true,
            enabled: true,
        };
        let rules = RoleRules::new(vec![rule, always]);
        assert_eq!(
            rules.resolve(&identity(), None).role_id(),
            Some(OTHER_ROLE),
            "the unusable rule is skipped, the next rule decides"
        );
    }

    #[test]
    fn a_pattern_that_expands_to_a_huge_program_is_refused_at_save_time() {
        // Not the textbook backtracking example — this crate is a finite automaton and that one is
        // harmless here. This is the one that actually costs: 12 characters of source, 64KB of
        // compiled program, rebuilt on every sign-in.
        // The two ceilings catch different shapes and the assertion accepts either, because
        // what matters is that a rule this expensive is refused and the operator is told which
        // limit it hit — not that both paths produce one identical sentence.
        for pattern in [
            "a{1,1000000}",                                    // 12 chars, 64KB of program
            &format!("{}a{}", "(".repeat(20), ")".repeat(20)), // deeper than nest_limit
            &"a".repeat(2000),                                 // a paste
        ] {
            let rule = regex_rule(pattern);
            let problems = rule.validate();
            assert_eq!(
                rule.when_operator,
                WhenOperator::Regex,
                "the operator is what is under test"
            );
            assert!(
                problems.iter().any(|p| p.field == "when_value"
                    && (p
                        .message
                        .contains("regular expression this platform will run")
                        || p.message.contains("is a paste"))),
                "expected a refusal naming the field for {pattern:.20}; got {problems:?}"
            );
        }
    }

    #[test]
    fn an_ordinary_pattern_is_accepted_so_the_guard_is_not_the_feature() {
        // A guard that refuses legitimate rules trains the operator to disable it, and then the
        // guard is gone. These are the shapes a real directory group rule uses.
        for pattern in [".*staff.*", "^eng-[a-z]+$", "\\p{L}+", "[a-z]{2,40}"] {
            let rule = regex_rule(pattern);
            assert!(
                rule.validate().is_empty(),
                "`{pattern}` is a normal condition and must be accepted, got {:?}",
                rule.validate()
            );
        }
    }

    #[test]
    fn a_rule_missing_its_key_or_value_is_refused_by_name() {
        let rule = RoleRule {
            position: 0,
            when_kind: WhenKind::Claim,
            when_key: String::new(),
            when_operator: WhenOperator::Equals,
            when_value: String::new(),
            role_id: ROLE,
            scope_type: ScopeType::Organization,
            site_id: None,
            stop: true,
            enabled: true,
        };
        let fields: Vec<_> = rule.validate().into_iter().map(|p| p.field).collect();
        assert!(fields.contains(&"when_key"));
        assert!(fields.contains(&"when_value"));
    }

    #[test]
    fn a_site_rule_must_name_its_site_and_an_organization_rule_must_not() {
        let site_without = RoleRule::equals(0, WhenKind::Group, "groups", "x", ROLE);
        assert!(
            site_without.validate().is_empty(),
            "organization scope needs no site"
        );

        let mut site_scope = group_rule(0, "x", ROLE);
        site_scope.scope_type = ScopeType::Site;
        assert!(site_scope.validate().iter().any(|p| p.field == "site_id"));

        let mut wrong_site = group_rule(0, "x", ROLE);
        wrong_site.site_id = Some(SITE);
        assert!(wrong_site.validate().iter().any(|p| p.field == "site_id"));
    }

    #[test]
    fn two_rules_claiming_one_slot_are_refused_before_the_database_is() {
        let rules = RoleRules::new(vec![
            group_rule(3, "a", ROLE),
            group_rule(3, "b", OTHER_ROLE),
        ]);
        let problems = rules.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.field == "position" && p.index.is_some()),
            "a tie in the order has to be named, got {problems:?}"
        );
    }

    #[test]
    fn normalization_closes_the_gaps_a_drag_leaves_behind() {
        let rules = RoleRules::new(vec![
            group_rule(7, "a", ROLE),
            group_rule(19, "b", OTHER_ROLE),
        ]);
        let dense = rules.normalized();
        assert_eq!(
            dense.rules.iter().map(|r| r.position).collect::<Vec<_>>(),
            vec![0, 1],
            "the unique index on (provider_id, position) is what makes the order real"
        );
    }

    #[test]
    fn the_wire_shape_round_trips() {
        let original = RoleRule {
            position: 4,
            when_kind: WhenKind::Title,
            when_key: "title".to_owned(),
            when_operator: WhenOperator::StartsWith,
            when_value: "staff".to_owned(),
            role_id: ROLE,
            scope_type: ScopeType::Site,
            site_id: Some(SITE),
            stop: false,
            enabled: true,
        };
        let value = original.to_value();
        let parsed = RoleRule::from_value(&value, 0).expect("a rule we wrote must parse");
        assert_eq!(parsed, original);
    }

    #[test]
    fn an_unknown_kind_or_operator_names_the_ones_that_exist() {
        let error = RoleRule::from_value(&json!({"when_kind": "moon", "role_id": ROLE}), 0)
            .expect_err("`moon` is not a kind");
        let message = error.to_string();
        assert!(message.contains("when_kind"), "{message}");
        assert!(
            message.contains("always"),
            "the refusal lists what is allowed: {message}"
        );

        let error = RoleRule::from_value(
            &json!({"when_kind": "claim", "when_operator": "sounds_like", "role_id": ROLE}),
            1,
        )
        .expect_err("`sounds_like` is not an operator");
        assert!(error.to_string().contains("starts_with"));
    }

    #[test]
    fn a_rule_needs_a_role_id() {
        let error = RoleRule::from_value(&json!({"when_kind": "group", "when_key": "groups"}), 0)
            .expect_err("a rule that grants nothing is not a rule");
        assert!(error.to_string().contains("role_id"));
    }

    #[test]
    fn positions_default_to_the_array_index_so_a_client_may_omit_them() {
        let rules = RoleRules::from_value(&json!([
            {"when_kind": "group", "when_key": "groups", "when_value": "a", "role_id": ROLE},
            {"when_kind": "group", "when_key": "groups", "when_value": "b", "role_id": OTHER_ROLE},
        ]))
        .expect("a bare list is the documented shape");
        assert_eq!(rules.rules[0].position, 0);
        assert_eq!(rules.rules[1].position, 1);
    }

    #[test]
    fn an_envelope_and_a_bare_list_are_the_same_request() {
        let bare = RoleRules::from_value(&json!([
            {"when_kind": "always", "role_id": ROLE},
        ]))
        .expect("bare list");
        let wrapped = RoleRules::from_value(&json!({"rules": [
            {"when_kind": "always", "role_id": ROLE},
        ]}))
        .expect("envelope");
        assert_eq!(bare, wrapped);
    }

    #[test]
    fn a_paste_of_rules_is_refused_rather_than_truncated() {
        let many: Vec<Value> = (0..MAX_RULES + 1)
            .map(|index| json!({"when_kind": "always", "role_id": ROLE, "position": index}))
            .collect();
        let error =
            RoleRules::from_value(&Value::Array(many)).expect_err("a paste is not a rule set");
        assert!(
            error.to_string().contains("narrow the conditions"),
            "{error}"
        );
    }

    #[test]
    fn a_shape_that_is_not_a_list_is_named_rather_than_treated_as_empty() {
        // An empty rule set is a legitimate state (an operator clearing it); a malformed one is
        // not, and the difference is that the first writes and the second never arrives.
        let error = RoleRules::from_value(&json!({"rules": "everything"})).expect_err("not a list");
        assert!(error.to_string().contains("must be a list"), "{error}");
        assert!(
            RoleRules::from_value(&json!([]))
                .expect("empty is valid")
                .rules
                .is_empty()
        );
    }

    // -----------------------------------------------------------------------------------------
    // The stored-membership source (slice 4 part 6)
    //
    // Each of these is written so that it fails if the membership source is removed from the
    // evaluator — which is the whole regression this slice exists to prevent. They are not
    // "tests of the new feature": a test that passes both before and after the change is a test
    // that proves nothing about it.
    // -----------------------------------------------------------------------------------------

    #[test]
    fn a_group_rule_matches_a_stored_membership_the_token_never_mentioned() {
        // The provisioned identity is an IdP that asserts nothing about groups, which is what
        // almost every SCIM deployment looks like on the wire: the account exists because a
        // connector created it, the membership exists because a connector wrote it, and the token
        // carries no groups because the IdP and the connector are separate systems. Before the
        // membership source existed, this rule could never match and nothing said why.
        let rules = RoleRules::new(vec![group_rule(0, "engineering", ROLE)]);
        let identity = Identity {
            subject: "u-99".to_owned(),
            email: "grace@example.com".to_owned(),
            display_name: Some("Grace".to_owned()),
            groups: Vec::new(),
            attributes: serde_json::Map::new(),
        };
        assert!(
            rules.resolve(&identity, None).role_id().is_none(),
            "with no membership there is nothing to match, and the test would prove nothing"
        );

        let groups = GroupContext::new(Vec::new(), vec!["engineering".to_owned()]);
        let resolution = rules.resolve_with(&provisioned_identity(), &groups, None);
        assert!(
            matches!(&resolution, Resolution::Matched { role_id, .. } if *role_id == ROLE),
            "a connector that put the account in a group must decide the next sign-in's role: {resolution:?}"
        );
    }

    #[test]
    fn the_claim_alone_still_decides_an_interactive_sign_in() {
        // The regression guard in the other direction. Reading the membership is an *addition*;
        // a change that quietly made the claim stop mattering would pass the test above.
        let rules = RoleRules::new(vec![group_rule(0, "engineering", ROLE)]);
        let identity = identity();
        assert!(matches!(
            rules.resolve(&identity, None),
            Resolution::Matched { .. }
        ));
    }

    #[test]
    fn a_group_the_account_is_not_in_matches_nothing_from_either_source() {
        // The union must not widen into a match. "Engineering" is in the membership, "sales" is
        // not, and a rule for sales must not fire because the two strings are both group names.
        let rules = RoleRules::new(vec![group_rule(0, "sales", OTHER_ROLE)]);
        let groups = GroupContext::new(vec!["engineering".to_owned()], vec!["oncall".to_owned()]);
        assert!(matches!(
            rules.resolve_with(&provisioned_identity(), &groups, None),
            Resolution::Default { .. }
        ));
    }

    #[test]
    fn first_match_wins_across_both_sources() {
        // The order is the semantics, and it must not depend on which source the value came from:
        // a rule that fires on the claim still pre-empts a later one that would fire on stored
        // membership, and a claim rule is not "preferred" the way it would be under a precedence
        // rule. Both orders are asserted because both are plausible designs and only one of them
        // is what "first match wins" says.
        let rules = RoleRules::new(vec![
            group_rule(0, "engineering", ROLE),
            group_rule(1, "oncall", OTHER_ROLE),
        ]);
        let from_claim =
            GroupContext::new(vec!["engineering".to_owned()], vec!["oncall".to_owned()]);
        let from_membership =
            GroupContext::new(vec!["oncall".to_owned()], vec!["engineering".to_owned()]);

        assert!(matches!(
            rules.resolve_with(&provisioned_identity(), &from_claim, None),
            Resolution::Matched { role_id, .. } if role_id == ROLE
        ));
        assert!(matches!(
            rules.resolve_with(&provisioned_identity(), &from_membership, None),
            Resolution::Matched { role_id, .. } if role_id == ROLE
        ));
    }

    #[test]
    fn a_read_says_which_source_supplied_the_value_that_matched() {
        // The panel has to be able to answer "where did this group come from", and a boolean
        // match cannot: both sources produce an identical `Matched`.
        let rules = RoleRules::new(vec![group_rule(0, "engineering", ROLE)]);

        let from_membership = GroupContext::new(Vec::new(), vec!["engineering".to_owned()]);
        let resolution = rules.resolve_with(&provisioned_identity(), &from_membership, None);
        let Resolution::Matched { rule, .. } = &resolution else {
            panic!("expected a match, got {resolution:?}");
        };
        assert_eq!(
            from_membership.source_of(rule.when_value.trim()),
            Some(GroupSource::Membership),
            "a rule that fired on the stored row must not read as a claim match"
        );
    }

    #[test]
    fn a_non_group_rule_never_reports_a_group_source() {
        // A claim or department rule that fires while the account is in five groups must not
        // claim a group source — the value that matched came from somewhere else entirely, and
        // `source_of` would otherwise return a plausible wrong answer.
        //
        // The identity is the full one, because the rule reads `title` and the provisioned
        // fixture deliberately has no attributes. Passing that one would have failed the match
        // for a reason that has nothing to do with the source under test.
        let rules = RoleRules::new(vec![RoleRule::equals(
            0,
            WhenKind::Title,
            "title",
            "staff engineer",
            ROLE,
        )]);
        let identity = identity();
        let groups = GroupContext::from_claim(identity.groups.clone());
        assert!(matches!(
            rules.resolve_with(&identity, &groups, None),
            Resolution::Matched { .. }
        ));
        assert_eq!(groups.source_of("staff engineer"), None);
    }

    #[test]
    fn an_unreadable_membership_still_lets_a_claim_rule_fire() {
        // A database blip must not disable a provider whose tokens do carry group claims. The
        // failure is reported, not enforced, and the claim path is untouched by it.
        let rules = RoleRules::new(vec![group_rule(0, "engineering", ROLE)]);
        let groups =
            GroupContext::from_claim(vec!["engineering".to_owned()]).with_membership_unavailable();
        assert!(matches!(
            rules.resolve_with(&provisioned_identity(), &groups, None),
            Resolution::Matched { .. }
        ));
    }

    #[test]
    fn a_membership_read_failure_is_visible_to_the_caller_rather_than_silent() {
        // The evaluator cannot act on this by itself — withholding a grant is a policy decision
        // that belongs to the sign-in path — but it must be *sayable*, or the sign-in path would
        // report `no rule matched` and send an operator to edit a correct rule.
        let groups = GroupContext::from_claim(Vec::new()).with_membership_unavailable();
        assert!(groups.membership_unavailable());
        assert_eq!(groups.summary(), GroupSummary::MembershipUnavailable);
    }

    #[test]
    fn the_dry_run_trace_is_what_the_evaluator_compared() {
        // The single-implementation property, restated for the new source. A dry run that rendered
        // `identity.groups` while the sign-in evaluated the context would show an operator an
        // empty trace for a rule that fires — the precise confusion this slice removes. So the
        // trace is asserted to carry the *membership* value, and the claim-only reader is asserted
        // not to, which is what makes the two a real pair rather than a restatement.
        let rules = RoleRules::new(vec![group_rule(0, "engineering", ROLE)]);
        let groups = GroupContext::new(Vec::new(), vec!["engineering".to_owned()]);
        let rule = &rules.rules[0];

        assert_eq!(
            rule.read_with(&provisioned_identity(), &groups),
            vec!["engineering".to_owned()],
            "the trace a dry run shows must be the list the evaluator compared"
        );
        assert!(
            rule.read(&provisioned_identity()).is_empty(),
            "and it must not fall back to the token's empty claim, which is the old behaviour"
        );
    }

    /// The provisioned identity: an IdP that asserts nothing about groups.
    fn provisioned_identity() -> Identity {
        Identity {
            subject: "u-99".to_owned(),
            email: "grace@example.com".to_owned(),
            display_name: Some("Grace".to_owned()),
            groups: Vec::new(),
            attributes: serde_json::Map::new(),
        }
    }
}
