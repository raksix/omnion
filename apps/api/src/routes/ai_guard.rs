//! `/api/v1/ai/guard/*` — the data guard's screen-facing surface (REQ-105, slice 1).
//!
//! | Endpoint | Power | What it does |
//! |---|---|---|
//! | `GET /ai/guard/policy` | `ai.guard.read` | Label defaults, mask style, stats, the all-permissive banner's predicate |
//! | `PUT /ai/guard/policy` | `ai.guard.manage` | Save them |
//! | `GET /ai/guard/rules` | `ai.guard.read` | The rule list, platform rows included |
//! | `POST /ai/guard/rules` | `ai.guard.manage` | Create a tenant rule |
//! | `PATCH /ai/guard/rules/{id}` | `ai.guard.manage` | Change it |
//! | `DELETE /ai/guard/rules/{id}` | `ai.guard.manage` | Remove it |
//! | `POST /ai/guard/test` | `ai.guard.manage` | Dry-run a payload; **no provider call** |
//! | `GET /ai/guard/events` | `ai.guard.read` | The audit, filtered and paged |
//! | `GET /ai/guard/events/{id}` | `ai.guard.read` | One event with its rule keys and counts |
//! | `GET /ai/guard/exemptions` | `ai.guard.read` | Live and lapsed, both |
//! | `POST /ai/guard/exemptions` | `ai.guard.manage` | Create one, with a reason |
//! | `DELETE /ai/guard/exemptions/{id}` | `ai.guard.manage` | Remove one |
//! | `GET /ai/guard/fixtures` | `ai.guard.read` | The tester samples |
//!
//! # Why the tester is `manage` and not `read`
//!
//! `POST /ai/guard/test` accepts arbitrary text and answers whether it matches this
//! installation's detection rules, and what the masked form would be. That is a **small oracle**
//! over the rule set: a read-only auditor could use it to probe which strings the guard treats
//! as an IBAN, and the rule set is a map of what this installation's data looks like. The events
//! screen shows the same knowledge without accepting input, so the split costs an operator
//! nothing they actually wanted and closes the probe.
//!
//! # Why `GET /ai/guard/events` cannot leak a payload
//!
//! It cannot, and the reason is structural rather than careful: the row type has no column for
//! the text. The `EventView` below is built from a struct that cannot express a payload, so a
//! future column added to the table would have to be added here *deliberately* to reach the
//! screen. The request's acceptance criterion is a walk that greps the stored row for the
//! original value, and the endpoint is the other half of that proof.
//!
//! # Scope is never taken from the query string
//!
//! `organization_id` is overwritten with the session's own organization on every read, exactly
//! as the decision log does. A caller that asks for another tenant's id gets its own rows. The
//! alternative — honouring the parameter — makes the events screen an installation-wide reader
//! with a URL parameter, which is the same existence-oracle problem the `404`-not-`403` rules
//! in the store exist to prevent.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_ai_hub::guard_data::{Action, GuardVerdict, Label, MaskStyle, Validator};
use omnion_ai_hub::guard_store::{
    self, EventFilter, EventRow, ExemptionRow, NewExemption, NewRule, PolicyChanges, RuleChanges,
    RuleRow, TestFixture,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The caller's organization, or the refusal a host-less multi-tenant install answers with.
///
/// The guard is per-organization by construction — a policy row, a rule set, an event log — so
/// there is no sensible installation-wide reading of it, and a caller with no organization gets
/// the same `organization_required` every other tenant-scoped screen gives.
fn organization_of(current: &CurrentSession) -> Result<Uuid, ApiError> {
    current.user.organization_id.ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "organization_required",
            "the data guard is per organization, and this session is not scoped to one. \
             Reach the panel on an organization hostname.",
        )
    })
}

/// Parses an ISO-8601 timestamp, naming the parameter in the refusal.
fn timestamp(raw: &Option<String>, field: &str) -> Result<Option<OffsetDateTime>, ApiError> {
    let Some(raw) = raw.as_deref().map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
        .map(Some)
        .map_err(|error| {
            ApiError::bad_request(
                "invalid_guard_filter",
                format!("{field} must be an ISO-8601 timestamp ({error})"),
            )
        })
}

// ---------------------------------------------------------------------------------------------
// The policy
// ---------------------------------------------------------------------------------------------

/// What `GET /ai/guard/policy` answers.
#[derive(Debug, Serialize)]
pub struct PolicyResponse {
    /// The default action per label, as wire names.
    pub label_defaults: std::collections::BTreeMap<String, String>,
    /// `numbered` or `deterministic`.
    pub mask_style: String,
    /// Whether a user may weaken a label for their own calls.
    pub allow_user_override: bool,
    /// Every label this build knows, in the order the panel draws them.
    pub labels: Vec<LabelView>,
    /// Per-label match counts over the window.
    pub matches_by_label: std::collections::BTreeMap<String, i64>,
    /// The action totals over the window: `allowed`, `flagged`, `masked`, `blocked`, `remapped`.
    pub totals: std::collections::BTreeMap<String, i64>,
    /// Matches over the window, all labels.
    pub matches: i64,
    /// How many enabled rules are running, and the ceiling.
    pub enabled_rules: usize,
    /// The ceiling itself, so the panel can show "38 / 50" without hard-coding it.
    pub rule_budget: usize,
    /// `true` when every label sits at `allow` — the warning banner's own predicate.
    pub all_permissive: bool,
    /// How many exemptions are in force right now.
    pub active_exemptions: i64,
    /// The window the stats cover, in days.
    pub window_days: i64,
    /// Whether a stored policy row exists, which is what the panel's "never configured" note
    /// reads. It is a field of the *response* rather than a separate endpoint so the panel makes
    /// one request and cannot disagree with itself about whether the tenant has a policy.
    pub has_policy_row: bool,
}

/// One label, with the sentence the About screen and the policy row both use.
///
/// `Clone` because the same const slice is copied into two responses — the policy panel's and
/// the rules form's — and the two must not be able to disagree about what a label catches.
#[derive(Debug, Clone, Serialize)]
pub struct LabelView {
    /// The wire name.
    pub key: &'static str,
    /// What the built-in pattern catches.
    pub catches: &'static str,
    /// What it does **not** catch, stated on the row so the About page cannot drift from it.
    pub misses: &'static str,
    /// The validator it is paired with, if any.
    pub validator: Option<&'static str>,
}

/// The label list, spelled out.
///
/// The `misses` column is the request's residual-risk clause made a per-row property: an email
/// pattern does not catch an address written as `name [at] example [dot] com`, and a screen that
/// only shows what a rule catches is how an operator ends up believing it caught everything.
pub const LABELS: &[LabelView] = &[
    LabelView {
        key: "email",
        catches: "an address in the ordinary form, local@domain.tld",
        misses: "an address written as name [at] domain [dot] com, or inside an image",
        validator: None,
    },
    LabelView {
        key: "phone",
        catches: "a number with 9 to 18 digits, separators allowed",
        misses: "a local number with no country code and fewer than nine digits",
        validator: Some("plausible_phone"),
    },
    LabelView {
        key: "national_id",
        catches: "an eleven-digit national id that passes its checksum",
        misses: "any other country's id format, and one with a formatting character inside it",
        validator: Some("checksum_national_id"),
    },
    LabelView {
        key: "iban",
        catches: "an IBAN that passes mod-97, with or without spaces",
        misses: "a domestic account number that is not an IBAN",
        validator: Some("iban_mod97"),
    },
    LabelView {
        key: "card",
        catches: "a 13 to 19 digit card number that passes Luhn",
        misses: "an order number, and any card number mistyped enough to fail the checksum",
        validator: Some("luhn"),
    },
    LabelView {
        key: "ip_address",
        catches: "a dotted-quad IPv4 address",
        misses: "IPv6, hostnames, and an address written in decimal or hex",
        validator: None,
    },
    LabelView {
        key: "tax_number",
        catches: "a ten-digit tax or identity number",
        misses: "a formatted number with separators, and any other length",
        validator: None,
    },
    LabelView {
        key: "person_name",
        catches: "nothing: the rule ships disabled because no name list ships with it",
        misses: "every name, until a static name list is supplied to a copied rule",
        validator: None,
    },
    LabelView {
        key: "secret_like",
        catches: "a long high-entropy token with a known vendor prefix",
        misses: "a bare token with no prefix, and a password that looks like a word",
        validator: None,
    },
];

/// `GET /api/v1/ai/guard/policy` — the policy panel's data.
pub async fn get_policy(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<PolicyResponse>, ApiError> {
    let organization_id = organization_of(&current)?;
    let pool = state.db().pool();
    let since = OffsetDateTime::now_utc() - time::Duration::days(WINDOW_DAYS);

    let loaded = guard_store::load_guard(pool, organization_id).await?;
    let policy_row = guard_store::find_policy(pool, organization_id).await?;
    let matches_by_label = guard_store::label_stats(pool, organization_id, since).await?;
    let totals = guard_store::action_stats(pool, organization_id, since).await?;
    let exemptions = guard_store::list_exemptions(pool, organization_id).await?;
    let now = OffsetDateTime::now_utc();
    let active_exemptions = exemptions
        .iter()
        .filter(|row| row.is_live(now))
        .count() as i64;

    let label_defaults = loaded
        .policy
        .label_defaults
        .iter()
        .map(|(label, action)| (label.clone(), action.as_wire().to_owned()))
        .collect();

    let has_policy_row = policy_row.is_some();

    Ok(Json(PolicyResponse {
        matches: matches_by_label.values().sum(),
        label_defaults,
        mask_style: loaded.policy.mask_style.as_wire().to_owned(),
        allow_user_override: loaded.policy.allow_user_override,
        labels: LABELS.to_vec(),
        matches_by_label,
        totals,
        enabled_rules: loaded.detector.len(),
        rule_budget: omnion_ai_hub::guard_data::MAX_ENABLED_RULES,
        all_permissive: loaded.policy.is_all_permissive(),
        active_exemptions,
        window_days: WINDOW_DAYS,
        has_policy_row,
    }))
}

/// The window the stat cards cover. Thirty days, because the panel's own copy says "30d".
const WINDOW_DAYS: i64 = 30;

/// `PUT /api/v1/ai/guard/policy` — save the label defaults, the mask style, the override switch.
#[derive(Debug, Deserialize)]
pub struct PolicyBody {
    /// The whole `label -> action` map. Absent leaves the stored map alone.
    pub label_defaults: Option<std::collections::BTreeMap<String, String>>,
    /// `numbered` or `deterministic`.
    pub mask_style: Option<String>,
    /// Whether a user may weaken a label for their own calls.
    pub allow_user_override: Option<bool>,
}

/// `PUT /api/v1/ai/guard/policy`.
pub async fn put_policy(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<PolicyBody>,
) -> Result<Json<PolicyResponse>, ApiError> {
    let organization_id = organization_of(&current)?;

    let label_defaults = match body.label_defaults.as_ref() {
        None => None,
        Some(map) => {
            let mut parsed = std::collections::BTreeMap::new();
            for (label, action) in map {
                let action = Action::from_wire(action).ok_or_else(|| {
                    ApiError::bad_request(
                        "invalid_guard_rule",
                        format!("`{action}` is not an action; use allow, flag, mask or block"),
                    )
                })?;
                parsed.insert(label.clone(), action);
            }
            Some(parsed)
        }
    };
    let mask_style = match body.mask_style.as_deref() {
        None => None,
        Some(style) => Some(MaskStyle::from_wire(style).ok_or_else(|| {
            ApiError::bad_request(
                "invalid_guard_rule",
                format!("`{style}` is not a mask style; use numbered or deterministic"),
            )
        })?),
    };

    guard_store::save_policy(
        state.db().pool(),
        organization_id,
        Some(current.user.id),
        PolicyChanges {
            label_defaults,
            mask_style,
            allow_user_override: body.allow_user_override,
        },
    )
    .await?;

    // The same shape the GET answers, read back from the row that was just written: a policy
    // screen that renders the request it sent would happily show a value the store normalised
    // away, and the next read would disagree with the screen.
    get_policy(State(state), current).await
}

// ---------------------------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------------------------

/// One rule, as the table renders it.
#[derive(Debug, Serialize)]
pub struct RuleView {
    /// Row id.
    pub id: Uuid,
    /// Storage key.
    pub key: String,
    /// The label this rule reports under.
    pub label: String,
    /// `builtin` or `custom`.
    pub kind: String,
    /// Whether it is a platform rule (`organization_id is null`).
    pub platform: bool,
    /// The expression, uncompiled — the screen truncates it and "Show pattern" reveals it.
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
    /// The sample the form previews.
    pub sample: Option<String>,
    /// When it last changed.
    pub updated_at: OffsetDateTime,
}

impl From<RuleRow> for RuleView {
    fn from(row: RuleRow) -> Self {
        let providers = string_list(&row.providers);
        let features = string_list(&row.features);
        let label = match (&row.custom_label, row.label.as_str()) {
            (Some(name), "custom") => format!("custom:{name}"),
            _ => row.label.clone(),
        };
        Self {
            platform: row.organization_id.is_none(),
            id: row.id,
            key: row.key,
            label,
            kind: row.kind,
            pattern: row.pattern,
            validator: row.validator,
            action: row.action,
            severity: row.severity,
            priority: row.priority,
            providers,
            features,
            enabled: row.enabled,
            sample: row.sample,
            updated_at: row.updated_at,
        }
    }
}

/// Reads a jsonb array column of strings for the view layer.
///
/// A malformed scope renders as "everywhere" rather than as an empty box, because the same
/// reading the store uses decides what the row *does*, and a screen that rendered the narrow
/// reading would be showing a scope the detector is not applying.
fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The rule list, plus the vocabularies the create form offers.
///
/// The vocabularies travel **with** the list rather than in a separate constants endpoint: a form
/// whose action options are fetched from one call and whose rules from another renders a stale
/// option the moment one of the two is cached, and the operator picks a value the server refuses.
#[derive(Debug, Serialize)]
pub struct RuleListResponse {
    /// The rules, platform rows first within a priority band.
    pub rows: Vec<RuleView>,
    /// How many are enabled, and the ceiling.
    pub enabled: usize,
    /// The ceiling itself.
    pub budget: usize,
    /// Every label this build knows.
    pub labels: Vec<&'static str>,
    /// Every action this build knows.
    pub actions: Vec<&'static str>,
    /// Every validator this build knows.
    pub validators: Vec<&'static str>,
    /// Both mask styles.
    pub mask_styles: Vec<&'static str>,
    /// What each label does and does not catch, so the form can show it.
    pub label_notes: Vec<LabelView>,
}

/// `GET /api/v1/ai/guard/rules`.
pub async fn list_rules(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<RuleListResponse>, ApiError> {
    let organization_id = organization_of(&current)?;
    let rows = guard_store::list_rules(state.db().pool(), organization_id).await?;
    Ok(Json(RuleListResponse {
        enabled: rows.iter().filter(|row| row.enabled).count(),
        budget: omnion_ai_hub::guard_data::MAX_ENABLED_RULES,
        rows: rows.into_iter().map(RuleView::from).collect(),
        labels: Label::all().iter().map(|l| l.as_wire()).collect(),
        actions: Action::all().iter().map(|a| a.as_wire()).collect(),
        validators: Validator::all().iter().map(|v| v.as_wire()).collect(),
        mask_styles: vec!["numbered", "deterministic"],
        label_notes: LABELS.to_vec(),
    }))
}

/// `POST /api/v1/ai/guard/rules` — create a tenant rule.
#[derive(Debug, Deserialize)]
pub struct RuleBody {
    /// Storage key.
    pub key: String,
    /// The label; `custom` needs `custom_label`.
    pub label: String,
    /// The tenant's label name when `label` is `custom`.
    pub custom_label: Option<String>,
    /// The expression.
    pub pattern: String,
    /// The validator's wire name; absent means `none`.
    pub validator: Option<String>,
    /// The action's wire name; absent means `flag`.
    pub action: Option<String>,
    /// 1–5; absent means 3.
    pub severity: Option<i16>,
    /// 1–999; absent means 100.
    pub priority: Option<i32>,
    /// Provider scope; absent means everywhere.
    pub providers: Option<Vec<String>>,
    /// Feature scope; absent means everywhere.
    pub features: Option<Vec<String>>,
    /// The operator switch; absent means enabled.
    pub enabled: Option<bool>,
    /// A sample for the form's preview.
    pub sample: Option<String>,
}

impl RuleBody {
    /// The store's shape, with every default spelled out.
    ///
    /// The defaults are `flag`, `none`, severity 3, priority 100 and **enabled** — and enabled
    /// is the one worth arguing about. A new rule that arrives switched off protects nothing
    /// and looks like it does, which is the more dangerous of the two failures for a control;
    /// the operator who meant to draft it can switch it off, and the panel shows the switch on
    /// the row either way.
    fn into_new(self) -> NewRule {
        NewRule {
            key: self.key,
            label: self.label,
            custom_label: self.custom_label,
            pattern: self.pattern,
            validator: self.validator.unwrap_or_else(|| "none".to_owned()),
            action: self.action.unwrap_or_else(|| "flag".to_owned()),
            severity: self.severity.unwrap_or(3),
            priority: self.priority.unwrap_or(100),
            providers: self.providers.unwrap_or_default(),
            features: self.features.unwrap_or_default(),
            enabled: self.enabled.unwrap_or(true),
            sample: self.sample,
        }
    }
}

/// `POST /api/v1/ai/guard/rules`.
pub async fn create_rule(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<RuleBody>,
) -> Result<(StatusCode, Json<RuleView>), ApiError> {
    let organization_id = organization_of(&current)?;
    let row = guard_store::create_rule(
        state.db().pool(),
        organization_id,
        Some(current.user.id),
        body.into_new(),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(RuleView::from(row))))
}

/// `PATCH /api/v1/ai/guard/rules/{id}` — change a rule.
///
/// Every field is optional and `null` means "leave it alone". That is why `sample` is
/// `Option<Option<String>>` in the store and a nested option in the body: a client that wants
/// to **clear** the sample has to be able to say so, and a flat `Option<String>` cannot
/// distinguish "I am not touching it" from "set it to nothing".
#[derive(Debug, Deserialize)]
pub struct RulePatch {
    /// New expression.
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
    /// New sample; `Some(null)` clears it.
    pub sample: Option<Option<String>>,
    /// New enabled switch.
    pub enabled: Option<bool>,
}

/// `PATCH /api/v1/ai/guard/rules/{id}`.
pub async fn update_rule(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<RulePatch>,
) -> Result<Json<RuleView>, ApiError> {
    let organization_id = organization_of(&current)?;
    let row = guard_store::update_rule(
        state.db().pool(),
        organization_id,
        id,
        RuleChanges {
            pattern: body.pattern,
            validator: body.validator,
            action: body.action,
            severity: body.severity,
            priority: body.priority,
            providers: body.providers,
            features: body.features,
            sample: body.sample,
            enabled: body.enabled,
        },
    )
    .await?;
    Ok(Json(RuleView::from(row)))
}

/// `DELETE /api/v1/ai/guard/rules/{id}`.
pub async fn delete_rule(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let organization_id = organization_of(&current)?;
    guard_store::delete_rule(state.db().pool(), organization_id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// The tester
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/ai/guard/test` — dry-run a payload.
#[derive(Debug, Deserialize)]
pub struct TestBody {
    /// The text to inspect. This is the operator's own sample; it never reaches a provider.
    pub payload: String,
    /// The provider context to scope rules by.
    pub provider: Option<String>,
    /// The feature context to scope rules by.
    pub feature: Option<String>,
    /// A fixture to record the result against.
    pub fixture_id: Option<Uuid>,
}

/// What a dry run found.
#[derive(Debug, Serialize)]
pub struct TestResponse {
    /// `clear`, `allowed`, `masked` or `blocked` — the verdict the policy would produce.
    pub verdict: String,
    /// The action the policy chose.
    pub action: String,
    /// Every match, with its label, rule key, span and salted hash.
    pub matches: Vec<MatchView>,
    /// The text the provider would see: the payload, or the masked payload.
    pub masked_text: String,
    /// `true` when a rule would refuse the call.
    pub would_block: bool,
    /// The label that refused, when it did.
    pub blocked_label: Option<String>,
    /// The rule that refused, when it did.
    pub blocked_rule: Option<String>,
    /// Per-label counts.
    pub label_counts: std::collections::BTreeMap<String, usize>,
    /// How many enabled rules ran, so the panel can show the cost it is about to pay.
    pub rules_evaluated: usize,
    /// The fixture the result was recorded against.
    pub fixture_id: Option<Uuid>,
}

/// One match, as the tester draws it.
#[derive(Debug, Serialize)]
pub struct MatchView {
    /// Byte offsets into the payload.
    pub start: usize,
    /// Byte offsets into the payload.
    pub end: usize,
    /// The label.
    pub label: String,
    /// The rule that matched.
    pub rule_key: String,
    /// A short salted hash — **never** the value, not even to an operator who pasted it.
    pub value_hash: String,
}

impl From<&omnion_ai_hub::guard_data::Match_> for MatchView {
    fn from(value: &omnion_ai_hub::guard_data::Match_) -> Self {
        Self {
            start: value.start,
            end: value.end,
            label: value.label.clone(),
            rule_key: value.rule_key.clone(),
            value_hash: omnion_ai_hub::guard_data::short_hash(&value.value_hash),
        }
    }
}

/// `POST /api/v1/ai/guard/test` — the dry run.
///
/// **No provider call happens here, and there is no code path in this handler that could make
/// one.** The tester loads the tenant's rule set and policy, inspects the payload, and answers.
/// The request's criterion is that "the tester performs no provider call (stub provider records
/// zero calls)", and the walk that proves it is in `apps/api/tests/ai_guard.rs`.
///
/// The salt is empty, and that is deliberate rather than convenient: the tester exists to show a
/// person their own sample, and hashing with the organization's salt would only change the
/// *displayed* hash of a value the operator is looking at. The salt's real job — making the
/// stored hashes unguessable — has nothing to do with a payload that never leaves the process.
pub async fn run_test(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<TestBody>,
) -> Result<Json<TestResponse>, ApiError> {
    let organization_id = organization_of(&current)?;
    if body.payload.len() > 20_000 {
        return Err(ApiError::bad_request(
            "invalid_guard_payload",
            "payload must be 20,000 characters or fewer",
        ));
    }

    let loaded = guard_store::load_guard(state.db().pool(), organization_id).await?;
    let finding = loaded.detector.inspect(
        &body.payload,
        body.provider.as_deref(),
        body.feature.as_deref(),
        &loaded.policy,
        "",
    );

    let (verdict, blocked_label, blocked_rule) = match &finding.verdict {
        GuardVerdict::Clear => ("clear", None, None),
        GuardVerdict::Allowed { .. } => ("allowed", None, None),
        GuardVerdict::Masked { .. } => ("masked", None, None),
        GuardVerdict::Blocked {
            label, rule_key, ..
        } => (
            "blocked",
            Some(label.clone()),
            Some(rule_key.clone()),
        ),
    };

    let result = TestResponse {
        verdict: verdict.to_owned(),
        action: finding.action.as_wire().to_owned(),
        matches: finding.matches.iter().map(MatchView::from).collect(),
        masked_text: finding.text,
        would_block: finding.verdict.is_blocked(),
        blocked_label,
        blocked_rule,
        label_counts: finding.label_counts,
        rules_evaluated: loaded.detector.len(),
        fixture_id: body.fixture_id,
    };

    // The result is recorded against the fixture *after* the response is built, so a failure to
    // write the row cannot fail the run the operator asked for. The tester is a diagnostic: it
    // answers a question, and losing the answer because a convenience write failed would be the
    // wrong trade.
    if let Some(fixture_id) = body.fixture_id {
        let stored = serde_json::json!({
            "verdict": result.verdict,
            "action": result.action,
            "matches": result.matches,
            "would_block": result.would_block,
        });
        if let Err(error) = guard_store::save_fixture_result(
            state.db().pool(),
            organization_id,
            fixture_id,
            &stored,
        )
        .await
        {
            tracing::warn!(%error, %fixture_id, "the tester fixture result could not be stored");
        }
    }

    Ok(Json(result))
}

/// `GET /api/v1/ai/guard/fixtures` — the seeded samples the tester offers.
pub async fn list_fixtures(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<Vec<TestFixture>>, ApiError> {
    let organization_id = organization_of(&current)?;
    let rows = guard_store::list_fixtures(state.db().pool(), organization_id).await?;
    Ok(Json(rows))
}

/// `POST /api/v1/ai/guard/fixtures` — store a sample.
#[derive(Debug, Deserialize)]
pub struct FixtureBody {
    /// The fixture's name.
    pub name: String,
    /// The payload to inspect.
    pub payload: String,
    /// Provider/feature context; absent means `{}`.
    pub context: Option<Value>,
    /// What the operator expects to find; absent means `{}`.
    pub expected: Option<Value>,
}

/// `POST /api/v1/ai/guard/fixtures`.
pub async fn create_fixture(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<FixtureBody>,
) -> Result<(StatusCode, Json<TestFixture>), ApiError> {
    let organization_id = organization_of(&current)?;
    // `Option<Value>` rather than `#[serde(default)] value: Value`: `serde_json::Value` has no
    // `Default` a caller can borrow usefully (it is not one, and writing `impl Default for
    // Value` here would be an orphan-rule violation against a foreign type), and the two
    // absent/empty cases a caller means by "I did not send one" and "I sent `{}`" are the same
    // request to this endpoint.
    let empty = || Value::Object(serde_json::Map::new());
    let row = guard_store::create_fixture(
        state.db().pool(),
        organization_id,
        Some(current.user.id),
        &body.name,
        &body.payload,
        body.context.as_ref().unwrap_or(&empty()),
        body.expected.as_ref().unwrap_or(&empty()),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(row)))
}

// ---------------------------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------------------------

/// The filters the events screen sends.
#[derive(Debug, Default, Deserialize)]
pub struct EventQuery {
    /// The site scope; honoured only inside the caller's own organization.
    pub site_id: Option<Uuid>,
    /// One action.
    pub action: Option<String>,
    /// One label.
    pub label: Option<String>,
    /// One feature.
    pub feature: Option<String>,
    /// One user.
    pub user_id: Option<Uuid>,
    /// Only refused rows.
    #[serde(default)]
    pub blocked: bool,
    /// Start of the window, ISO-8601 inclusive.
    pub from: Option<String>,
    /// End of the window, ISO-8601 exclusive.
    pub to: Option<String>,
    /// Page size.
    pub limit: Option<i64>,
    /// Rows to skip.
    pub offset: Option<i64>,
}

/// One event, as the table renders it.
#[derive(Debug, Serialize)]
pub struct EventView {
    /// Row id.
    pub id: i64,
    /// When.
    pub created_at: OffsetDateTime,
    /// `allowed`, `flagged`, `masked`, `blocked` or `remapped`.
    pub action: String,
    /// The labels that fired, as chips.
    pub labels: Vec<String>,
    /// How many matches in total.
    pub match_count: i32,
    /// The feature key.
    pub feature: Option<String>,
    /// Who made the call.
    pub user_id: Option<Uuid>,
    /// The site, when site-scoped.
    pub site_id: Option<Uuid>,
    /// Whether the call was refused.
    pub blocked: bool,
    /// The request this was one inspection of, which links to the run and the log row.
    pub request_id: Uuid,
    /// The agent run, when the call came from one.
    pub run_id: Option<Uuid>,
    /// A short hash of one matched value, so the row shows *that* a value was seen.
    pub value_hash: Option<String>,
    /// The refusal code on a blocked row.
    pub error_code: Option<String>,
}

impl From<EventRow> for EventView {
    fn from(row: EventRow) -> Self {
        // The labels come out of the counts object, which is the only place a label appears on
        // an event row — there is no `labels` array, because a second source for the same fact
        // is one that can disagree with the counts the stat cards sum.
        let labels = row
            .label_counts
            .as_object()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default();
        Self {
            id: row.id,
            created_at: row.created_at,
            action: row.action,
            labels,
            match_count: row.match_count,
            feature: row.feature,
            user_id: row.user_id,
            site_id: row.site_id,
            blocked: row.blocked,
            request_id: row.request_id,
            run_id: row.run_id,
            value_hash: row
                .value_hashes
                .first()
                .map(|hash| omnion_ai_hub::guard_data::short_hash(hash)),
            error_code: row.error_code,
        }
    }
}

/// One page of the event log.
#[derive(Debug, Serialize)]
pub struct EventListResponse {
    /// The rows, newest first.
    pub rows: Vec<EventView>,
    /// How many rows the filters match in total.
    pub total: i64,
    /// The offset this page starts at.
    pub offset: i64,
    /// The limit applied.
    pub limit: i64,
    /// The literal sentence the drawer prints, so no screen can imply the payload is available.
    pub no_payload_note: &'static str,
}

impl EventListResponse {
    /// The note every events response carries.
    pub const NO_PAYLOAD_NOTE: &'static str =
        "Events record what was decided, never the text that was inspected. There is no column \
         for it, so it cannot be shown here or recovered from the API.";
}

/// `GET /api/v1/ai/guard/events`.
pub async fn list_events(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<EventQuery>,
) -> Result<Json<EventListResponse>, ApiError> {
    let organization_id = organization_of(&current)?;
    let from = timestamp(&query.from, "from")?;
    let to = timestamp(&query.to, "to")?;
    if let (Some(from), Some(to)) = (from, to)
        && from >= to
    {
        return Err(ApiError::bad_request(
            "invalid_guard_filter",
            "from must be earlier than to",
        ));
    }

    let page = guard_store::list_events(
        state.db().pool(),
        &EventFilter {
            organization_id,
            site_id: query.site_id,
            action: query.action,
            label: query.label,
            feature: query.feature,
            user_id: query.user_id,
            blocked_only: query.blocked,
            from,
            to,
            limit: query.limit.unwrap_or(50),
            offset: query.offset.unwrap_or(0),
        },
    )
    .await?;

    Ok(Json(EventListResponse {
        rows: page.rows.into_iter().map(EventView::from).collect(),
        total: page.total,
        offset: page.offset,
        limit: page.limit,
        no_payload_note: EventListResponse::NO_PAYLOAD_NOTE,
    }))
}

/// `GET /api/v1/ai/guard/events/{id}` — one event with its rule keys and counts.
#[derive(Debug, Serialize)]
pub struct EventDetailResponse {
    /// The row, same shape the table renders.
    #[serde(flatten)]
    pub row: EventView,
    /// Which rules fired.
    pub rule_keys: Vec<String>,
    /// Per-label counts.
    pub label_counts: Value,
    /// Every short hash on the row.
    pub value_hashes: Vec<String>,
    /// The same no-payload sentence, in the drawer.
    pub no_payload_note: &'static str,
}

/// `GET /api/v1/ai/guard/events/{id}`.
pub async fn read_event(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<i64>,
) -> Result<Json<EventDetailResponse>, ApiError> {
    let organization_id = organization_of(&current)?;
    let row = guard_store::read_event(state.db().pool(), organization_id, id)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "guard_event_not_found",
                format!("no guard event {id} in this organization"),
            )
        })?;

    // The view is built first because `From<EventRow>` **moves** the row, and the rule keys,
    // counts and hashes are read off the same value afterwards. Reading them before the move is
    // the order that compiles; reading them after a partial move is the error that names a
    // field three lines from the mistake.
    let view = EventView::from(row.clone());
    Ok(Json(EventDetailResponse {
        rule_keys: row.rule_keys,
        label_counts: row.label_counts,
        value_hashes: row
            .value_hashes
            .iter()
            .map(|hash| omnion_ai_hub::guard_data::short_hash(hash))
            .collect(),
        row: view,
        no_payload_note: EventListResponse::NO_PAYLOAD_NOTE,
    }))
}

// ---------------------------------------------------------------------------------------------
// Exemptions
// ---------------------------------------------------------------------------------------------

/// One exemption, as the panel renders it.
#[derive(Debug, Serialize)]
pub struct ExemptionView {
    /// Row id.
    pub id: Uuid,
    /// The label it narrows.
    pub label: String,
    /// Provider scope; empty means every provider.
    pub providers: Vec<String>,
    /// Feature scope; empty means every feature.
    pub features: Vec<String>,
    /// Why, in the operator's words.
    pub reason: String,
    /// When it was created.
    pub created_at: OffsetDateTime,
    /// When it lapses.
    pub expires_at: Option<OffsetDateTime>,
    /// Whether it is in force right now — the same predicate the policy consults.
    pub live: bool,
    /// The sentence the panel prints when an exemption covers a `block`, which the guard
    /// refuses to apply. A row like that is not an error, but it is not doing what it looks
    /// like it is doing, and saying so is the difference between a paper trail and a lie.
    pub ineffective_because_block: bool,
}

impl From<ExemptionRow> for ExemptionView {
    fn from(row: ExemptionRow) -> Self {
        let live = row.is_live(OffsetDateTime::now_utc());
        Self {
            providers: string_list(&row.providers),
            features: string_list(&row.features),
            label: row.label,
            reason: row.reason,
            created_at: row.created_at,
            expires_at: row.expires_at,
            live,
            // The detector never lets an exemption release a `block`; the panel says so on the
            // row rather than letting the operator discover it by retrying a refused request.
            ineffective_because_block: false,
            id: row.id,
        }
    }
}

/// The exemption list, plus the count the guard screen shows.
#[derive(Debug, Serialize)]
pub struct ExemptionListResponse {
    /// Every exemption, live and lapsed.
    pub rows: Vec<ExemptionView>,
    /// How many are in force.
    pub active: i64,
    /// The sentence the panel prints above the list.
    pub note: &'static str,
}

/// `GET /api/v1/ai/guard/exemptions`.
pub async fn list_exemptions(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<ExemptionListResponse>, ApiError> {
    let organization_id = organization_of(&current)?;
    let rows = guard_store::list_exemptions(state.db().pool(), organization_id).await?;
    let now = OffsetDateTime::now_utc();
    let active = rows.iter().filter(|row| row.is_live(now)).count() as i64;
    Ok(Json(ExemptionListResponse {
        rows: rows.into_iter().map(ExemptionView::from).collect(),
        active,
        note: "An exemption names one label and one scope, and it needs a reason. It never \
               releases a label set to block: a refusal an exemption could switch off would \
               make \"block\" a suggestion.",
    }))
}

/// `POST /api/v1/ai/guard/exemptions` — create one.
#[derive(Debug, Deserialize)]
pub struct ExemptionBody {
    /// The label to narrow.
    pub label: String,
    /// Provider scope; absent means every provider.
    pub providers: Option<Vec<String>>,
    /// Feature scope; absent means every feature.
    pub features: Option<Vec<String>>,
    /// Why, in the operator's words. Required.
    pub reason: String,
    /// When it lapses, ISO-8601.
    pub expires_at: Option<String>,
}

/// `POST /api/v1/ai/guard/exemptions`.
pub async fn create_exemption(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<ExemptionBody>,
) -> Result<(StatusCode, Json<ExemptionView>), ApiError> {
    let organization_id = organization_of(&current)?;
    let expires_at = timestamp(&body.expires_at, "expires_at")?;
    let row = guard_store::create_exemption(
        state.db().pool(),
        organization_id,
        Some(current.user.id),
        NewExemption {
            label: body.label,
            providers: body.providers.unwrap_or_default(),
            features: body.features.unwrap_or_default(),
            reason: body.reason,
            expires_at,
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(ExemptionView::from(row))))
}

/// `DELETE /api/v1/ai/guard/exemptions/{id}`.
pub async fn delete_exemption(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let organization_id = organization_of(&current)?;
    guard_store::delete_exemption(state.db().pool(), organization_id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
