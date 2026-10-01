//! The store for the data guard's rows (REQ-105, slice 1).
//!
//! [`crate::guard_data`] is pure: labels, patterns, validators, the action lattice, the mask
//! map. It can decide, and it can be proven to decide the same way twice, with no database in
//! the picture. This module is the other half — the rows a decision reads and the rows it
//! leaves behind — and it holds three properties the request is explicit about.
//!
//! # 1. A tenant's rules and the platform's rules are one rule set
//!
//! [`load_detector`] reads the built-in rows (`organization_id is null`) **and** the tenant's,
//! compiles them, and hands back one [`Detector`]. A detector that only saw tenant rows would
//! find nothing on a fresh installation, and the operator who has not yet written a rule would
//! see a guard screen full of active rules and an events table that never fills — the exact
//! combination that reads as a broken feature rather than an unused one.
//!
//! # 2. Nothing here ever writes a matched value
//!
//! [`record_event`] takes hashes, counts and a bounded error code. There is no parameter on
//! any function in this module that accepts the text a rule matched, so "the payload never
//! appears in `ai_guard_events`" is a property of the signature and not of a reviewer's
//! attention. The request's acceptance criterion is a test that greps the stored row for the
//! original value; this is the shape that makes such a test impossible to fail by accident.
//!
//! # 3. A built-in rule is never mutated, and the `is null` half is visible to the tenant
//!
//! [`update_rule`] refuses a built-in row with [`AiHubError::InvalidGuardRule`] and its
//! message says to copy it. That is the request's "immutable, editable only by copying to a
//! tenant rule", and it is enforced in the store rather than in the route: a rule that only the
//! HTTP layer protects is one `sqlx::query` away from being changed by the next writer.
//!
//! # Why the detector is loaded per call instead of cached
//!
//! Loading is one query over at most [`crate::guard_data::MAX_ENABLED_RULES`] rows and
//! compiling at most that many expressions — sub-millisecond, and it happens *before* a network
//! call that costs hundreds of milliseconds. A cache would add a staleness window in which an
//! operator's `block` is not yet in force, and a guard that lags its own policy is worse than
//! one that costs a query.

use std::collections::BTreeMap;

use regex::Regex;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AiHubError, Result};
use crate::guard_data::{
    Action, Detector, Exemption, GuardError, Label, MaskStyle, Policy, Rule, RuleKind, Validator,
};

/// Columns read back from `ai_guard_rules`.
const RULE_COLUMNS: &str = "id, organization_id, key, label, custom_label, kind, pattern, \
     validator, action, severity, priority, providers, features, enabled, sample, created_by, \
     created_at, updated_at";

/// A guard rule row, as the rules screen and the detector both read it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RuleRow {
    /// Row id.
    pub id: Uuid,
    /// Owning organization; `None` for a platform rule.
    pub organization_id: Option<Uuid>,
    /// Storage key, unique per organization.
    pub key: String,
    /// The label this rule reports under (`custom` for a tenant label).
    pub label: String,
    /// The tenant's own label name, when `label` is `custom`.
    pub custom_label: Option<String>,
    /// `builtin` or `custom`.
    pub kind: String,
    /// The uncompiled expression, as stored.
    pub pattern: String,
    /// The validator's wire name.
    pub validator: String,
    /// The action's wire name.
    pub action: String,
    /// 1–5.
    pub severity: i16,
    /// 1–999; lower runs first.
    pub priority: i32,
    /// Provider scope; empty means everywhere.
    pub providers: serde_json::Value,
    /// Feature scope; empty means everywhere.
    pub features: serde_json::Value,
    /// The operator switch.
    pub enabled: bool,
    /// A sample string the rule form previews.
    pub sample: Option<String>,
    /// Who wrote it.
    pub created_by: Option<Uuid>,
    /// When it was written.
    pub created_at: OffsetDateTime,
    /// When it last changed.
    pub updated_at: OffsetDateTime,
}

impl RuleRow {
    /// The compiled rule this row describes.
    ///
    /// # Errors
    ///
    /// Refuses a row whose `label`, `kind`, `validator` or `action` is not one this build
    /// knows. A row can only get that way by being written outside the store (the check
    /// constraints cover most of it, but not the *vocabulary*: `label = 'email'` is accepted by
    /// the schema and unknown to the enum), and returning an error here is what stops an
    /// unknown label from being silently dropped from a rule set — a dropped rule is a
    /// protection that is off and looks on.
    pub fn compile(&self) -> Result<Rule> {
        let label = match (self.label.as_str(), self.custom_label.as_deref()) {
            // `custom` is the only label whose *name* is a tenant's, and the name itself is not
            // needed here: `Rule::label_wire` reads `custom_label` off the rule, so a label that
            // exists but whose name is empty is caught by the check constraint, not by this
            // arm. The binding is kept as a name rather than `_` so the arm still reads as the
            // (label, name) pair it is matching.
            ("custom", Some(_name)) => Label::Custom,
            (name, None) => Label::from_wire(name).ok_or_else(|| {
                AiHubError::InvalidGuardRule(format!(
                    "rule `{}` reports the label `{name}`, which this build does not know",
                    self.key
                ))
            })?,
            (name, Some(_)) => {
                return Err(AiHubError::InvalidGuardRule(format!(
                    "rule `{}` reports the label `{name}` and also carries a custom label",
                    self.key
                )));
            }
        };
        let kind = match self.kind.as_str() {
            "builtin" => RuleKind::Builtin,
            "custom" => RuleKind::Custom,
            other => {
                return Err(AiHubError::InvalidGuardRule(format!(
                    "rule `{}` has the unknown kind `{other}`",
                    self.key
                )));
            }
        };
        let validator = Validator::from_wire(&self.validator).ok_or_else(|| {
            AiHubError::InvalidGuardRule(format!(
                "rule `{}` has the unknown validator `{}`",
                self.key, self.validator
            ))
        })?;
        let action = Action::from_wire(&self.action).ok_or_else(|| {
            AiHubError::InvalidGuardRule(format!(
                "rule `{}` has the unknown action `{}`",
                self.key, self.action
            ))
        })?;
        let pattern = Regex::new(&self.pattern).map_err(|error| {
            AiHubError::InvalidGuardRule(format!(
                "rule `{}` has a pattern that does not compile: {error}",
                self.key
            ))
        })?;

        Ok(Rule {
            key: self.key.clone(),
            label,
            custom_label: self.custom_label.clone().unwrap_or_default(),
            kind,
            pattern,
            validator,
            action,
            priority: self.priority,
            providers: string_list(&self.providers),
            features: string_list(&self.features),
            enabled: self.enabled,
        })
    }
}

/// Reads a jsonb array column of strings, treating anything else as empty.
///
/// A scope that silently matched nothing would **disable** a rule rather than widen it, and
/// the screen would still show an active rule that never fires — so a malformed scope falls
/// back to the wide reading. That is the safe direction for a control: over-scoped, never
/// under-scoped.
fn string_list(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The compiled rule set plus the policy it will be consulted with.
#[derive(Debug, Clone)]
pub struct LoadedGuard {
    /// The compiled rules, platform and tenant together.
    pub detector: Detector,
    /// The tenant's policy, with the defaults a missing row implies.
    pub policy: Policy,
}

/// A tenant's guard policy row, as the policy screen reads and writes it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PolicyRow {
    /// The tenant this is.
    pub organization_id: Uuid,
    /// Serialized `label -> action` map.
    pub label_defaults: serde_json::Value,
    /// `numbered` or `deterministic`.
    pub mask_style: String,
    /// Whether a user may weaken a label for their own calls.
    pub allow_user_override: bool,
    /// Who last changed it.
    pub updated_by: Option<Uuid>,
    /// When.
    pub updated_at: OffsetDateTime,
}

impl PolicyRow {
    /// The policy this row describes, with the live exemptions attached.
    ///
    /// A missing `mask_style` (a row written by an older build) is `numbered` rather than an
    /// error, and an unknown label default falls back to `allow` — the same reading
    /// [`Policy::action_for`] already has.
    #[must_use]
    pub fn as_policy(&self, exemptions: Vec<Exemption>) -> Policy {
        let mut label_defaults: BTreeMap<String, Action> = BTreeMap::new();
        if let Some(map) = self.label_defaults.as_object() {
            for (label, action) in map {
                if let Some(action) = action.as_str().and_then(Action::from_wire) {
                    label_defaults.insert(label.clone(), action);
                }
            }
        }
        Policy {
            label_defaults,
            mask_style: MaskStyle::from_wire(&self.mask_style).unwrap_or_default(),
            allow_user_override: self.allow_user_override,
            exemptions,
        }
    }
}

/// An exemption row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExemptionRow {
    /// Row id.
    pub id: Uuid,
    /// Owning tenant.
    pub organization_id: Uuid,
    /// The label this narrows.
    pub label: String,
    /// Provider scope; empty means every provider.
    pub providers: serde_json::Value,
    /// Feature scope; empty means every feature.
    pub features: serde_json::Value,
    /// Why, in the operator's words. Never empty (the schema refuses it).
    pub reason: String,
    /// Who created it.
    pub created_by: Option<Uuid>,
    /// When.
    pub created_at: OffsetDateTime,
    /// When it lapses; `None` means it does not.
    pub expires_at: Option<OffsetDateTime>,
}

impl ExemptionRow {
    /// The policy-side shape of this row.
    #[must_use]
    pub fn as_exemption(&self) -> Exemption {
        Exemption {
            label: self.label.clone(),
            providers: string_list(&self.providers),
            features: string_list(&self.features),
            reason: self.reason.clone(),
            expires_at: self.expires_at,
        }
    }

    /// Whether this exemption is still in force.
    #[must_use]
    pub fn is_live(&self, now: OffsetDateTime) -> bool {
        self.expires_at.is_none_or(|at| at > now)
    }
}

/// One event row: the audit of a decision, with no payload text in it.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct EventRow {
    /// Row id.
    pub id: i64,
    /// The tenant it happened in.
    pub organization_id: Uuid,
    /// The site, when the call was site-scoped.
    pub site_id: Option<Uuid>,
    /// Who made the call.
    pub user_id: Option<Uuid>,
    /// The request this was one inspection of.
    pub request_id: Uuid,
    /// The agent run, when the call came from one.
    pub run_id: Option<Uuid>,
    /// The provider that would have answered.
    pub provider_id: Option<Uuid>,
    /// The feature key.
    pub feature: Option<String>,
    /// `allowed`, `flagged`, `masked`, `blocked` or `remapped`.
    pub action: String,
    /// Which rules fired.
    pub rule_keys: Vec<String>,
    /// Per-label counts, serialized.
    pub label_counts: serde_json::Value,
    /// Total matches.
    pub match_count: i32,
    /// The `blocked` switch; the same fact as `action`, and constrained to agree with it.
    pub blocked: bool,
    /// Salted hashes of the matched values, never the values.
    pub value_hashes: Vec<String>,
    /// `ai_guard_blocked` on a refused row.
    pub error_code: Option<String>,
    /// When.
    pub created_at: OffsetDateTime,
}

/// The filters the events screen and its export both accept.
#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    /// The tenant. Always set by the caller — never taken from a query string.
    pub organization_id: Uuid,
    /// The site scope, when the read is site-scoped.
    pub site_id: Option<Uuid>,
    /// One action.
    pub action: Option<String>,
    /// One label, served by containment on `label_counts`.
    pub label: Option<String>,
    /// One feature.
    pub feature: Option<String>,
    /// One user.
    pub user_id: Option<Uuid>,
    /// Only refused rows.
    pub blocked_only: bool,
    /// Start of the window, inclusive.
    pub from: Option<OffsetDateTime>,
    /// End of the window, exclusive.
    pub to: Option<OffsetDateTime>,
    /// Page size.
    pub limit: i64,
    /// Rows to skip.
    pub offset: i64,
}

/// One page of the event log.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EventPage {
    /// The rows, newest first.
    pub rows: Vec<EventRow>,
    /// How many rows the filters match in total.
    pub total: i64,
    /// The offset this page starts at.
    pub offset: i64,
    /// The limit actually applied.
    pub limit: i64,
}

// -------------------------------------------------------------------------------------------
// Reading
// -------------------------------------------------------------------------------------------

/// Every rule a call at this tenant consults: the platform's and the tenant's, in priority order.
///
/// Built-in rows first within a priority band, so a tenant's rule at the same priority is the
/// one the operator was looking at last — and because [`crate::guard_data::Detector`] breaks a
/// tie by the strictest action rather than by position, the ordering here is a *display*
/// property, not a semantic one. Making it explicit stops the day somebody reads the list and
/// concludes the order decides the outcome.
pub async fn list_rules(pool: &PgPool, organization_id: Uuid) -> Result<Vec<RuleRow>> {
    let sql = format!(
        "select {RULE_COLUMNS} from ai_guard_rules \
         where organization_id is null or organization_id = $1 \
         order by priority, key, id"
    );
    let rows: Vec<RuleRow> = sqlx::query_as(&sql)
        .bind(organization_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// One rule by id, but only inside `organization_id`.
///
/// A platform rule is visible to every tenant and is addressed by the same call, so the
/// predicate is `(organization_id = $2 or organization_id is null)` rather than a bare match.
/// A tenant row from another tenant is `None` — the `where` is the tenancy boundary, and it is
/// written here once so no caller can forget it.
pub async fn find_rule(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<RuleRow>> {
    let sql = format!(
        "select {RULE_COLUMNS} from ai_guard_rules \
         where id = $1 and (organization_id = $2 or organization_id is null)"
    );
    let row: Option<RuleRow> = sqlx::query_as(&sql)
        .bind(id)
        .bind(organization_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// The tenant's policy row, or the defaults when it has none.
///
/// `None` is a real answer, not a failure: a fresh tenant has no policy row, and `Policy::default`
/// (everything `allow`) is what the request wants for an installation that has never opened the
/// guard screen.
pub async fn find_policy(pool: &PgPool, organization_id: Uuid) -> Result<Option<PolicyRow>> {
    let sql = "select organization_id, label_defaults, mask_style, allow_user_override, \
               updated_by, updated_at from ai_guard_policy where organization_id = $1";
    let row: Option<PolicyRow> = sqlx::query_as(sql)
        .bind(organization_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Every exemption for a tenant, live or lapsed.
///
/// Both halves, because the screen's job is to show the ones that lapsed *and* the count of
/// the ones that did not — a list that silently drops the lapsed rows cannot show the operator
/// what an expiry did. The filter for "still in force" belongs to the policy, not the read.
pub async fn list_exemptions(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<Vec<ExemptionRow>> {
    let sql = "select id, organization_id, label, providers, features, reason, created_by, \
               created_at, expires_at from ai_guard_exemptions \
               where organization_id = $1 order by created_at desc, id";
    let rows: Vec<ExemptionRow> = sqlx::query_as(sql)
        .bind(organization_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// One exemption by id, inside `organization_id`.
pub async fn find_exemption(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<ExemptionRow>> {
    let sql = "select id, organization_id, label, providers, features, reason, created_by, \
               created_at, expires_at from ai_guard_exemptions \
               where id = $1 and organization_id = $2";
    let row: Option<ExemptionRow> = sqlx::query_as(sql)
        .bind(id)
        .bind(organization_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// The live exemptions, which is what a policy consults.
pub async fn live_exemptions(
    pool: &PgPool,
    organization_id: Uuid,
    now: OffsetDateTime,
) -> Result<Vec<Exemption>> {
    let rows = list_exemptions(pool, organization_id).await?;
    Ok(rows
        .iter()
        .filter(|row| row.is_live(now))
        .map(ExemptionRow::as_exemption)
        .collect())
}

/// Load the tenant's detector and policy in one step.
///
/// This is the call every provider request makes. It is one function rather than three because
/// the three reads have to agree: a detector built from a rule set and a policy built from a
/// *different* moment's exemptions would be a decision no operator ever made.
///
/// # Errors
///
/// [`AiHubError::GuardConfiguration`] when the enabled rule set is over
/// [`crate::guard_data::MAX_ENABLED_RULES`]. The request is explicit that this "refuses to start
/// in the guard (the API answers a configuration error) rather than slowing every call", so the
/// error is a configuration error and not a per-request `400`.
pub async fn load_guard(pool: &PgPool, organization_id: Uuid) -> Result<LoadedGuard> {
    let now = OffsetDateTime::now_utc();
    let rows = list_rules(pool, organization_id).await?;
    let mut rules = Vec::with_capacity(rows.len());
    for row in &rows {
        rules.push(row.compile()?);
    }
    let detector = Detector::new(rules).map_err(|error| match error {
        GuardError::RuleBudget { .. } => AiHubError::GuardConfiguration(error.to_string()),
    })?;

    let exemptions = live_exemptions(pool, organization_id, now).await?;
    let policy = match find_policy(pool, organization_id).await? {
        Some(row) => row.as_policy(exemptions),
        None => Policy {
            exemptions,
            ..Policy::default()
        },
    };

    Ok(LoadedGuard { detector, policy })
}

// -------------------------------------------------------------------------------------------
// Writing rules
// -------------------------------------------------------------------------------------------

/// A rule an operator wants to store.
#[derive(Debug, Clone)]
pub struct NewRule {
    /// Storage key, unique per organization.
    pub key: String,
    /// The label: a built-in wire name, or `custom`.
    pub label: String,
    /// The tenant's label name when `label` is `custom`.
    pub custom_label: Option<String>,
    /// The uncompiled expression.
    pub pattern: String,
    /// The validator's wire name.
    pub validator: String,
    /// The action's wire name.
    pub action: String,
    /// 1–5.
    pub severity: i16,
    /// 1–999.
    pub priority: i32,
    /// Provider scope; empty means everywhere.
    pub providers: Vec<String>,
    /// Feature scope; empty means everywhere.
    pub features: Vec<String>,
    /// The operator switch.
    pub enabled: bool,
    /// A sample string for the form's preview.
    pub sample: Option<String>,
}

/// A partial change to a rule. `None` leaves a field alone.
#[derive(Debug, Clone, Default)]
pub struct RuleChanges {
    /// New pattern.
    pub pattern: Option<String>,
    /// New validator.
    pub validator: Option<String>,
    /// New action.
    pub action: Option<String>,
    /// New severity.
    pub severity: Option<i16>,
    /// New priority.
    pub priority: Option<i32>,
    /// New provider scope.
    pub providers: Option<Vec<String>>,
    /// New feature scope.
    pub features: Option<Vec<String>>,
    /// New sample.
    pub sample: Option<Option<String>>,
    /// New enabled switch.
    pub enabled: Option<bool>,
}

/// Validates a key against the request's `[a-z0-9_.-]{2,60}`.
///
/// The key is part of the *event* an operator reads ("the rule `customer_code.builtin`
/// refused this"), so it has to be spellable by a human reading a screen and safe to interpolate
/// into a log line and a CSS selector. Lowercase and dots only: an uppercase or space key is
/// technically fine and practically a support ticket.
fn validate_key(key: &str) -> Result<String> {
    let key = key.trim();
    let ok = (2..=60).contains(&key.len())
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-'));
    if !ok {
        return Err(AiHubError::InvalidGuardRule(
            "key must be 2 to 60 characters of a-z, 0-9, dot, underscore or dash".to_owned(),
        ));
    }
    Ok(key.to_owned())
}

/// Validates the label / custom-label pair the request's form enforces.
fn validate_label(label: &str, custom_label: Option<&str>) -> Result<()> {
    if label == "custom" {
        let Some(name) = custom_label.map(str::trim).filter(|n| !n.is_empty()) else {
            return Err(AiHubError::InvalidGuardRule(
                "label `custom` needs a custom_label between 1 and 60 characters".to_owned(),
            ));
        };
        if name.chars().count() > 60 {
            return Err(AiHubError::InvalidGuardRule(
                "custom_label must be 60 characters or fewer".to_owned(),
            ));
        }
        return Ok(());
    }
    if Label::from_wire(label).is_none() {
        return Err(AiHubError::InvalidGuardRule(format!(
            "`{label}` is not a label this build knows; use one of {} or `custom`",
            Label::all()
                .iter()
                .map(|l| l.as_wire())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if custom_label.is_some_and(|c| !c.trim().is_empty()) {
        return Err(AiHubError::InvalidGuardRule(format!(
            "label `{label}` is built in, so it cannot also carry a custom_label"
        )));
    }
    Ok(())
}

/// Compiles a pattern, turning a regex error into a field error.
///
/// The request's criterion is that "an invalid expression is a field error, never a saved rule",
/// and the only place that can be enforced is before the write — a `check` constraint cannot
/// call a Rust regex, and a rule that failed to compile at request time would turn every
/// provider call into a panic.
fn compile_pattern(key: &str, pattern: &str) -> Result<Regex> {
    if pattern.trim().is_empty() {
        return Err(AiHubError::InvalidGuardRule(format!(
            "pattern cannot be empty (rule `{key}`)"
        )));
    }
    Regex::new(pattern).map_err(|error| {
        AiHubError::InvalidGuardRule(format!(
            "pattern for rule `{key}` is not a valid regular expression ({error})"
        ))
    })
}

fn validate_validator(name: &str) -> Result<()> {
    if Validator::from_wire(name).is_none() {
        return Err(AiHubError::InvalidGuardRule(format!(
            "`{name}` is not a validator; use one of {}",
            Validator::all()
                .iter()
                .map(|v| v.as_wire())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(())
}

fn validate_action(name: &str) -> Result<()> {
    if Action::from_wire(name).is_none() {
        return Err(AiHubError::InvalidGuardRule(format!(
            "`{name}` is not an action; use one of {}",
            Action::all()
                .iter()
                .map(|a| a.as_wire())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(())
}

fn validate_priority(priority: i32) -> Result<()> {
    if !(1..=999).contains(&priority) {
        return Err(AiHubError::InvalidGuardRule(
            "priority must be between 1 and 999".to_owned(),
        ));
    }
    Ok(())
}

fn validate_severity(severity: i16) -> Result<()> {
    if !(1..=5).contains(&severity) {
        return Err(AiHubError::InvalidGuardRule(
            "severity must be between 1 and 5".to_owned(),
        ));
    }
    Ok(())
}

/// Create a tenant rule.
///
/// # Errors
///
/// [`AiHubError::InvalidGuardRule`] naming the offending field, or
/// [`AiHubError::GuardRuleConflict`] when the folded unique index already holds the key.
pub async fn create_rule(
    pool: &PgPool,
    organization_id: Uuid,
    created_by: Option<Uuid>,
    new: NewRule,
) -> Result<RuleRow> {
    let key = validate_key(&new.key)?;
    validate_label(&new.label, new.custom_label.as_deref())?;
    let _compiled = compile_pattern(&key, &new.pattern)?;
    validate_validator(&new.validator)?;
    validate_action(&new.action)?;
    validate_priority(new.priority)?;
    validate_severity(new.severity)?;

    let sql = format!(
        "insert into ai_guard_rules (organization_id, key, label, custom_label, kind, pattern, \
         validator, action, severity, priority, providers, features, enabled, sample, created_by) \
         values ($1, $2, $3, $4, 'custom', $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) \
         returning {RULE_COLUMNS}"
    );
    let row: Result<RuleRow> = sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(&key)
        .bind(&new.label)
        .bind(new.custom_label.as_deref().map(str::trim))
        .bind(&new.pattern)
        .bind(&new.validator)
        .bind(&new.action)
        .bind(new.severity)
        .bind(new.priority)
        .bind(serde_json::to_value(&new.providers).unwrap_or(serde_json::json!([])))
        .bind(serde_json::to_value(&new.features).unwrap_or(serde_json::json!([])))
        .bind(new.enabled)
        .bind(new.sample.as_deref())
        .bind(created_by)
        .fetch_one(pool)
        .await
        .map_err(|error| key_conflict(error, &key));
    row
}

/// Change a rule.
///
/// Every candidate value is validated **before** the update is issued, and the row is re-read
/// with `organization_id = $1` in the `where`: a caller that passes another tenant's id gets
/// [`AiHubError::GuardRuleNotFound`] rather than a row it may read but not have written.
///
/// A platform rule is refused with a message that says to copy it — see the module header.
///
/// # Errors
///
/// [`AiHubError::InvalidGuardRule`], [`AiHubError::GuardRuleNotFound`], or
/// [`AiHubError::GuardConfiguration`] when the change would push the tenant over the rule budget.
pub async fn update_rule(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    changes: RuleChanges,
) -> Result<RuleRow> {
    let existing = find_rule(pool, organization_id, id)
        .await?
        .ok_or(AiHubError::GuardRuleNotFound(id))?;
    if existing.organization_id.is_none() {
        return Err(AiHubError::InvalidGuardRule(format!(
            "`{}` is a platform rule and cannot be changed. Copy it to a rule of your own first.",
            existing.key
        )));
    }

    if let Some(pattern) = changes.pattern.as_deref() {
        let _ = compile_pattern(&existing.key, pattern)?;
    }
    if let Some(validator) = changes.validator.as_deref() {
        validate_validator(validator)?;
    }
    if let Some(action) = changes.action.as_deref() {
        validate_action(action)?;
    }
    if let Some(priority) = changes.priority {
        validate_priority(priority)?;
    }
    if let Some(severity) = changes.severity {
        validate_severity(severity)?;
    }

    // The budget is checked against the tenant's *post-change* enabled count, because the
    // refusal has to be about the configuration the operator is asking for. Enabling the
    // 51st rule is refused here, at the write, not at the next request.
    if changes.enabled == Some(true) && !existing.enabled {
        let enabled_now = enabled_rule_count(pool, organization_id).await?;
        if enabled_now + 1 > crate::guard_data::MAX_ENABLED_RULES as i64 {
            return Err(AiHubError::GuardConfiguration(format!(
                "enabling this rule would make {} enabled rules and the ceiling is {}",
                enabled_now + 1,
                crate::guard_data::MAX_ENABLED_RULES
            )));
        }
    }

    // `coalesce($, …)` rather than eleven `coalesce`s in SQL: the changed fields are the ones
    // that are `Some`, and a caller that sends `enabled: false` must be able to switch a rule
    // *off* — which a truthiness test on the option would get wrong, because `false` and `none`
    // are the same thing to every `if let Some(x)` that maps it onto a boolean.
    let sql = format!(
        "update ai_guard_rules set \
           pattern = coalesce($3, pattern), \
           validator = coalesce($4, validator), \
           action = coalesce($5, action), \
           severity = coalesce($6, severity), \
           priority = coalesce($7, priority), \
           providers = coalesce($8, providers), \
           features = coalesce($9, features), \
           enabled = coalesce($10, enabled), \
           sample = case when $11 then $12 else sample end, \
           updated_at = now() \
         where id = $1 and organization_id = $2 \
         returning {RULE_COLUMNS}"
    );
    let row: RuleRow = sqlx::query_as(&sql)
        .bind(id)
        .bind(organization_id)
        .bind(changes.pattern.as_deref())
        .bind(changes.validator.as_deref())
        .bind(changes.action.as_deref())
        .bind(changes.severity)
        .bind(changes.priority)
        .bind(changes.providers.as_ref().map(|p| {
            serde_json::to_value(p).unwrap_or(serde_json::json!([]))
        }))
        .bind(changes.features.as_ref().map(|f| {
            serde_json::to_value(f).unwrap_or(serde_json::json!([]))
        }))
        .bind(changes.enabled)
        // The two-boundary `sample`: `$11` says "the caller was explicit", `$12` says "about what".
        // A `sample` of `None` is a real edit — clearing the preview — and a single nullable bind
        // cannot tell it from "leave it alone". `changes.sample` is `Option<Option<String>>`,
        // so the inner value is borrowed directly; `as_deref()` on the outer option would need
        // `Option<String>: Deref`, which is not a bound anything provides.
        .bind(changes.sample.is_some())
        .bind(changes.sample.as_ref().and_then(|s| s.as_deref()))
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// How many enabled rules a tenant would have, counting the platform's.
///
/// Only the enabled ones, because the budget is about the cost of the *running* set: a
/// disabled rule is not compiled and not evaluated, and refusing an organization for rules it
/// has switched off would be refusing it for a protection it is not using.
async fn enabled_rule_count(pool: &PgPool, organization_id: Uuid) -> Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "select count(*) from ai_guard_rules \
         where enabled and (organization_id is null or organization_id = $1)",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Remove a tenant rule.
///
/// The predicate is `organization_id = $2`, so a platform rule and another tenant's rule are
/// both [`AiHubError::GuardRuleNotFound`] — the second for the tenancy reason in the module
/// header, the first because a platform rule is immutable in both directions.
pub async fn delete_rule(pool: &PgPool, organization_id: Uuid, id: Uuid) -> Result<()> {
    let deleted = sqlx::query("delete from ai_guard_rules where id = $1 and organization_id = $2")
        .bind(id)
        .bind(organization_id)
        .execute(pool)
        .await?;
    if deleted.rows_affected() == 0 {
        // Read it back to tell a platform rule from a missing one, so the message can say
        // "platform rules cannot be deleted" rather than "not found" for a row that is
        // sitting right there in the list.
        if let Some(row) = find_rule(pool, organization_id, id).await?
            && row.organization_id.is_none()
        {
            return Err(AiHubError::InvalidGuardRule(format!(
                "`{}` is a platform rule and cannot be deleted",
                row.key
            )));
        }
        return Err(AiHubError::GuardRuleNotFound(id));
    }
    Ok(())
}

/// Turns a unique-violation on `(organization, key)` into a conflict the panel can resolve.
fn key_conflict(error: sqlx::Error, key: &str) -> AiHubError {
    if let sqlx::Error::Database(db) = &error
        && db.code().as_deref() == Some("23505")
    {
        return AiHubError::GuardRuleConflict(format!(
            "a rule with the key `{key}` already exists in this organization. Choose another key."
        ));
    }
    AiHubError::Database(error)
}

// -------------------------------------------------------------------------------------------
// Writing the policy
// -------------------------------------------------------------------------------------------

/// A policy edit.
#[derive(Debug, Clone, Default)]
pub struct PolicyChanges {
    /// The whole `label -> action` map, replacing what is there.
    ///
    /// Whole-map rather than per-label edits on purpose: the policy panel's control *is* the
    /// map, and a partial patch would have to decide what "absent" means for a label the
    /// operator is removing a setting from — which is the same question as "what does an
    /// unknown label default to", and the answer must be one function.
    pub label_defaults: Option<BTreeMap<String, Action>>,
    /// `numbered` or `deterministic`.
    pub mask_style: Option<MaskStyle>,
    /// Whether a user may weaken a label for their own calls.
    pub allow_user_override: Option<bool>,
}

/// Write the tenant's policy, creating the row on first save.
///
/// # Errors
///
/// [`AiHubError::InvalidGuardRule`] naming a label or an action this build does not know. The
/// validation happens here rather than in the route so the *seed* and the *tester* read the
/// same map: a policy that can be written into a shape the detector cannot read is a policy
/// screen that shows a setting which does nothing.
pub async fn save_policy(
    pool: &PgPool,
    organization_id: Uuid,
    updated_by: Option<Uuid>,
    changes: PolicyChanges,
) -> Result<PolicyRow> {
    for (label, action) in changes
        .label_defaults
        .as_ref()
        .into_iter()
        .flat_map(|map| map.iter())
    {
        if label == "custom" {
            return Err(AiHubError::InvalidGuardRule(
                "`custom` is not a policy label: a tenant label is set on its rule, not here"
                    .to_owned(),
            ));
        }
        if Label::from_wire(label).is_none() {
            return Err(AiHubError::InvalidGuardRule(format!(
                "`{label}` is not a label this build knows"
            )));
        }
        let _ = action;
    }

    let style = changes
        .mask_style
        .unwrap_or_default()
        .as_wire()
        .to_owned();
    let defaults = serde_json::to_value(changes.label_defaults.unwrap_or_default())
        .unwrap_or_else(|_| serde_json::json!({}));

    // One statement rather than read-then-branch: there is no third writer of this row, and a
    // `select` first would be a second answer to "does this tenant have a policy" that could
    // disagree with the insert's.
    let sql = "insert into ai_guard_policy (organization_id, label_defaults, mask_style, \
               allow_user_override, updated_by, updated_at) \
               values ($1, $2, $3, coalesce($4, false), $5, now()) \
               on conflict (organization_id) do update set \
                 label_defaults = excluded.label_defaults, \
                 mask_style = excluded.mask_style, \
                 allow_user_override = coalesce($4, ai_guard_policy.allow_user_override), \
                 updated_by = excluded.updated_by, \
                 updated_at = now() \
               returning organization_id, label_defaults, mask_style, allow_user_override, \
                 updated_by, updated_at";
    let row: PolicyRow = sqlx::query_as(sql)
        .bind(organization_id)
        .bind(defaults)
        .bind(&style)
        .bind(changes.allow_user_override)
        .bind(updated_by)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

// -------------------------------------------------------------------------------------------
// Writing exemptions
// -------------------------------------------------------------------------------------------

/// An exemption an operator wants to store.
#[derive(Debug, Clone)]
pub struct NewExemption {
    /// The label to narrow. Never `custom` without a name — an exemption names a label.
    pub label: String,
    /// Provider scope; empty means every provider.
    pub providers: Vec<String>,
    /// Feature scope; empty means every feature.
    pub features: Vec<String>,
    /// Why, in the operator's words. Required.
    pub reason: String,
    /// When it lapses; `None` means it does not.
    pub expires_at: Option<OffsetDateTime>,
}

/// Create an exemption.
///
/// # Errors
///
/// [`AiHubError::InvalidGuardExemption`] for a blank reason (the request's "every exemption
/// needs a reason"), an unknown label, or an expiry in the past.
pub async fn create_exemption(
    pool: &PgPool,
    organization_id: Uuid,
    created_by: Option<Uuid>,
    new: NewExemption,
) -> Result<ExemptionRow> {
    if Label::from_wire(new.label.trim()).is_none() {
        return Err(AiHubError::InvalidGuardExemption(format!(
            "`{}` is not a label this build knows",
            new.label.trim()
        )));
    }
    if new.reason.trim().is_empty() {
        return Err(AiHubError::InvalidGuardExemption(
            "reason is required: an exemption is the answer to \"why was this allowed\"".to_owned(),
        ));
    }
    if new.reason.chars().count() > 500 {
        return Err(AiHubError::InvalidGuardExemption(
            "reason must be 500 characters or fewer".to_owned(),
        ));
    }
    // The database refuses a past expiry too; catching it here turns a constraint violation
    // into a message that names the field, which is the difference between a form that
    // explains itself and one that reports a 500.
    if let Some(at) = new.expires_at
        && at <= OffsetDateTime::now_utc()
    {
        return Err(AiHubError::InvalidGuardExemption(
            "expires_at must be in the future".to_owned(),
        ));
    }

    let sql = "insert into ai_guard_exemptions (organization_id, label, providers, features, \
               reason, created_by, expires_at) \
               values ($1, $2, $3, $4, $5, $6, $7) \
               returning id, organization_id, label, providers, features, reason, created_by, \
                 created_at, expires_at";
    let row: ExemptionRow = sqlx::query_as(sql)
        .bind(organization_id)
        .bind(new.label.trim())
        .bind(serde_json::to_value(&new.providers).unwrap_or(serde_json::json!([])))
        .bind(serde_json::to_value(&new.features).unwrap_or(serde_json::json!([])))
        .bind(new.reason.trim())
        .bind(created_by)
        .bind(new.expires_at)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// Remove an exemption.
pub async fn delete_exemption(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<()> {
    let deleted =
        sqlx::query("delete from ai_guard_exemptions where id = $1 and organization_id = $2")
            .bind(id)
            .bind(organization_id)
            .execute(pool)
            .await?;
    if deleted.rows_affected() == 0 {
        return Err(AiHubError::InvalidGuardExemption(format!(
            "no exemption `{id}` in this organization"
        )));
    }
    Ok(())
}

/// The exemptions that lapsed since `since`, for the `ai.guard.exemption.expired` announcement.
///
/// A **read**, not a mutation: the row stays, because "this was allowed once, until the 12th"
/// is exactly the paper trail the request asks an exemption to leave, and a sweep that deleted
/// its own evidence would make the events screen unable to answer "was this ever exempt?".
/// Liveness is read from `expires_at`, so a lapsed row needs no housekeeping at all — which is
/// also why the announcement can be emitted by a caller that runs often, harmlessly, forever.
pub async fn lapsed_exemptions(
    pool: &PgPool,
    organization_id: Uuid,
    since: OffsetDateTime,
) -> Result<Vec<ExemptionRow>> {
    let sql = "select id, organization_id, label, providers, features, reason, created_by, \
               created_at, expires_at from ai_guard_exemptions \
               where organization_id = $1 and expires_at is not null and expires_at > $2 \
               order by expires_at, id";
    let rows: Vec<ExemptionRow> = sqlx::query_as(sql)
        .bind(organization_id)
        .bind(since)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

// -------------------------------------------------------------------------------------------
// Writing events
// -------------------------------------------------------------------------------------------

/// One inspection's worth of audit, to be stored.
///
/// Note what this type cannot express: there is no field for the text a rule matched, and no
/// field for the masked text. The request's criterion — "the payload never appears in
/// `ai_guard_events`" — is met here by the *type*, so the next writer who wants to log the
/// payload has to add a field, which is a diff somebody will read.
#[derive(Debug, Clone)]
pub struct NewEvent {
    /// The tenant it happened in.
    pub organization_id: Uuid,
    /// The site, when the call was site-scoped.
    pub site_id: Option<Uuid>,
    /// Who made the call.
    pub user_id: Option<Uuid>,
    /// The request this was one inspection of.
    pub request_id: Uuid,
    /// The agent run, when the call came from one.
    pub run_id: Option<Uuid>,
    /// The provider that would have answered.
    pub provider_id: Option<Uuid>,
    /// The feature key.
    pub feature: Option<String>,
    /// The verdict's wire name.
    pub action: String,
    /// Which rules fired.
    pub rule_keys: Vec<String>,
    /// Per-label counts.
    pub label_counts: BTreeMap<String, usize>,
    /// Total matches.
    pub match_count: i32,
    /// Salted hashes, never values.
    pub value_hashes: Vec<String>,
    /// `ai_guard_blocked` on a refused row.
    pub error_code: Option<String>,
}

/// Store one event.
///
/// # Errors
///
/// [`AiHubError::Database`] — the only failure this has, and deliberately: an event that could
/// not be written is an *audit* problem, not a request problem, and the request's own priority
/// is that the guard never silently changes the verdict it reported. The caller decides whether
/// to log a failure here (the chat route warns and continues), because refusing a user's chat
/// because the audit row did not fit would be a worse outcome than the gap it closes.
pub async fn record_event(pool: &PgPool, new: NewEvent) -> Result<i64> {
    let blocked = new.action == "blocked";
    let row: (i64,) = sqlx::query_as(
        "insert into ai_guard_events (organization_id, site_id, user_id, request_id, run_id, \
           provider_id, feature, action, rule_keys, label_counts, match_count, blocked, \
           value_hashes, error_code) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) \
         returning id",
    )
    .bind(new.organization_id)
    .bind(new.site_id)
    .bind(new.user_id)
    .bind(new.request_id)
    .bind(new.run_id)
    .bind(new.provider_id)
    .bind(new.feature.as_deref())
    .bind(&new.action)
    .bind(&new.rule_keys)
    .bind(serde_json::to_value(&new.label_counts).unwrap_or_else(|_| serde_json::json!({})))
    .bind(new.match_count)
    .bind(blocked)
    .bind(&new.value_hashes)
    .bind(new.error_code.as_deref())
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// One page of the event log, newest first.
pub async fn list_events(
    pool: &PgPool,
    filter: &EventFilter,
) -> Result<EventPage> {
    let where_sql = "organization_id = $1 \
        and ($2::uuid is null or site_id = $2) \
        and ($3::text is null or action = $3) \
        and ($4::text is null or label_counts ? $4) \
        and ($5::text is null or feature = $5) \
        and ($6::uuid is null or user_id = $6) \
        and (not $7 or blocked) \
        and ($8::timestamptz is null or created_at >= $8) \
        and ($9::timestamptz is null or created_at < $9)";
    let limit = filter.limit.clamp(1, 500);
    let offset = filter.offset.max(0);

    let rows_sql = format!(
        "select id, organization_id, site_id, user_id, request_id, run_id, provider_id, \
           feature, action, rule_keys, label_counts, match_count, blocked, value_hashes, \
           error_code, created_at from ai_guard_events where {where_sql} \
         order by created_at desc, id desc limit $10 offset $11"
    );
    let rows: Vec<EventRow> = sqlx::query_as(&rows_sql)
        .bind(filter.organization_id)
        .bind(filter.site_id)
        .bind(filter.action.as_deref())
        .bind(filter.label.as_deref())
        .bind(filter.feature.as_deref())
        .bind(filter.user_id)
        .bind(filter.blocked_only)
        .bind(filter.from)
        .bind(filter.to)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;

    let count_sql = format!("select count(*) from ai_guard_events where {where_sql}");
    let total: (i64,) = sqlx::query_as(&count_sql)
        .bind(filter.organization_id)
        .bind(filter.site_id)
        .bind(filter.action.as_deref())
        .bind(filter.label.as_deref())
        .bind(filter.feature.as_deref())
        .bind(filter.user_id)
        .bind(filter.blocked_only)
        .bind(filter.from)
        .bind(filter.to)
        .fetch_one(pool)
        .await?;

    Ok(EventPage {
        rows,
        total: total.0,
        offset,
        limit,
    })
}

/// One event, inside `organization_id`.
pub async fn read_event(
    pool: &PgPool,
    organization_id: Uuid,
    id: i64,
) -> Result<Option<EventRow>> {
    let sql = "select id, organization_id, site_id, user_id, request_id, run_id, provider_id, \
               feature, action, rule_keys, label_counts, match_count, blocked, value_hashes, \
               error_code, created_at from ai_guard_events where id = $1 and organization_id = $2";
    let row: Option<EventRow> = sqlx::query_as(sql)
        .bind(id)
        .bind(organization_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Per-label counts over a window, which is what the policy panel's stat cards aggregate.
///
/// Counted in SQL rather than by paging through [`list_events`]: the panel shows "matches 30d"
/// per label, and paging a table to add up numbers is how a dashboard ends up showing the last
/// page's numbers.
pub async fn label_stats(
    pool: &PgPool,
    organization_id: Uuid,
    since: OffsetDateTime,
) -> Result<BTreeMap<String, i64>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "select key, sum(value)::bigint from ai_guard_events, \
           jsonb_each_text(label_counts) as entry(key, value) \
         where organization_id = $1 and created_at >= $2 \
         group by key order by key",
    )
    .bind(organization_id)
    .bind(since)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// The action totals over a window, for the panel's masked/blocked/flagged cards.
pub async fn action_stats(
    pool: &PgPool,
    organization_id: Uuid,
    since: OffsetDateTime,
) -> Result<BTreeMap<String, i64>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "select action, count(*) from ai_guard_events \
         where organization_id = $1 and created_at >= $2 group by action order by action",
    )
    .bind(organization_id)
    .bind(since)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Delete events older than the retention window.
///
/// Shares the log retention window with the rest of the AI tables, per the request's slice 3
/// ("purge alongside the log retention window"). A guard event is a record that a value left
/// the platform in masked form, which is exactly the kind of row that has to age out — and
/// the reason the *payload* is not here is what makes that deletion cheap to reason about.
pub async fn prune_events(pool: &PgPool, days: i64) -> Result<u64> {
    let removed = sqlx::query("delete from ai_guard_events where created_at < now() - make_interval(days => $1)")
        .bind(days.max(0) as i32)
        .execute(pool)
        .await?;
    Ok(removed.rows_affected())
}

// -------------------------------------------------------------------------------------------
// The dry-run tester
// -------------------------------------------------------------------------------------------

/// A stored tester fixture.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct TestFixture {
    /// Row id.
    pub id: Uuid,
    /// The tenant it belongs to.
    pub organization_id: Uuid,
    /// The fixture's name.
    pub name: String,
    /// The payload to inspect.
    pub payload: String,
    /// Provider/feature context.
    pub context: serde_json::Value,
    /// What the operator expects to find.
    pub expected: serde_json::Value,
    /// When it was last run.
    pub last_run_at: Option<OffsetDateTime>,
    /// What the last run found.
    pub last_result: Option<serde_json::Value>,
    /// Who wrote it.
    pub created_by: Option<Uuid>,
    /// When.
    pub created_at: OffsetDateTime,
}

/// The fixtures for a tenant, in name order.
pub async fn list_fixtures(pool: &PgPool, organization_id: Uuid) -> Result<Vec<TestFixture>> {
    let sql = "select id, organization_id, name, payload, context, expected, last_run_at, \
               last_result, created_by, created_at from ai_guard_tests \
               where organization_id = $1 order by lower(name), id";
    let rows: Vec<TestFixture> = sqlx::query_as(sql)
        .bind(organization_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// One fixture by id, inside `organization_id`.
pub async fn find_fixture(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<TestFixture>> {
    let sql = "select id, organization_id, name, payload, context, expected, last_run_at, \
               last_result, created_by, created_at from ai_guard_tests \
               where id = $1 and organization_id = $2";
    let row: Option<TestFixture> = sqlx::query_as(sql)
        .bind(id)
        .bind(organization_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Record what a dry run found against a fixture.
pub async fn save_fixture_result(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    result: &serde_json::Value,
) -> Result<TestFixture> {
    let sql = "update ai_guard_tests set last_run_at = now(), last_result = $3 \
               where id = $1 and organization_id = $2 \
               returning id, organization_id, name, payload, context, expected, last_run_at, \
                 last_result, created_by, created_at";
    let row: TestFixture = sqlx::query_as(sql)
        .bind(id)
        .bind(organization_id)
        .bind(result)
        .fetch_optional(pool)
        .await?
        .ok_or(AiHubError::InvalidGuardRule(format!(
            "no tester fixture `{id}` in this organization"
        )))?;
    Ok(row)
}

/// Store a fixture.
pub async fn create_fixture(
    pool: &PgPool,
    organization_id: Uuid,
    created_by: Option<Uuid>,
    name: &str,
    payload: &str,
    context: &serde_json::Value,
    expected: &serde_json::Value,
) -> Result<TestFixture> {
    if name.trim().is_empty() {
        return Err(AiHubError::InvalidGuardRule(
            "name is required for a tester fixture".to_owned(),
        ));
    }
    if payload.is_empty() {
        return Err(AiHubError::InvalidGuardRule(
            "payload is required for a tester fixture".to_owned(),
        ));
    }
    let sql = "insert into ai_guard_tests (organization_id, name, payload, context, expected, \
               created_by) values ($1, $2, $3, $4, $5, $6) \
               returning id, organization_id, name, payload, context, expected, last_run_at, \
                 last_result, created_by, created_at";
    let row: TestFixture = sqlx::query_as(sql)
        .bind(organization_id)
        .bind(name.trim())
        .bind(payload)
        .bind(context)
        .bind(expected)
        .bind(created_by)
        .fetch_one(pool)
        .await?;
    Ok(row)
}
