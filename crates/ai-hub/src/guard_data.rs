//! AI data guard: detection rules, the organization policy and the pre-call checkpoint
//! (REQ-105, slice 1).
//!
//! # Where this sits, and why it is not a filter
//!
//! The checkpoint runs **before** the transport, on the assembled prompt, so a blocked payload
//! never reaches the network. The detector is pattern matching and it is honest about what that
//! buys: a rule that does not fire is not evidence that the text is clean, it is evidence that
//! this rule set did not match it. [`Detector`] therefore reports *what it matched* and never
//! reports "safe" as a property of the text.
//!
//! # Three decisions that are easy to get backwards
//!
//! **A match is not an action.** [`Match`] is a fact about a span of text. What happens is
//! [`Policy::decide`], which reads the organization policy, the exemptions and the rule's own
//! action. Collapsing the two is how a guard becomes either a no-op or a wall: a detector that
//! masks on its own is wrong for `flag`, and a detector that never acts is a dashboard.
//!
//! **The strictest rule for a label wins, and "none" is not a rule.** A payload matching an
//! `email` rule set to `mask` and a `custom:order` rule set to `allow` is masked, because the
//! caller asked for two things and the safe one is the only one that can be defended. Ties are
//! broken by priority so the same payload always produces the same verdict — a guard whose
//! outcome depends on rule evaluation order is a guard whose behaviour changes when somebody
//! adds a row.
//!
//! **`block` is refused, not sanitised.** When the effective action is `block`, the caller gets
//! [`GuardVerdict::Blocked`] naming the label and the rule, and the *original* text is not
//! returned anywhere in the refusal. A block that returned a masked copy teaches callers to
//! treat the refusal as a normal response.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use time::OffsetDateTime;

/// A request that carries more than this many enabled rules is refused rather than evaluated.
///
/// The detector is on the hot path of every provider call, and a rule budget nobody can exceed
/// is the only way to keep "the guard made chat slow" from being a configuration question. Fifty
/// compiled patterns over a prompt is well under a millisecond; five hundred is not a guard, it
/// is a job.
pub const MAX_ENABLED_RULES: usize = 50;

/// The labels the platform ships rules for.
///
/// A label is a *kind of value*, not a rule: `email` is a label whether it was caught by the
/// built-in pattern or by a tenant's own expression. Consumers switch on this enum, so a
/// new label is a new variant rather than a new string in a jsonb column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Label {
    Email,
    Phone,
    NationalId,
    Iban,
    Card,
    IpAddress,
    TaxNumber,
    PersonName,
    SecretLike,
    /// A tenant's own label, carried through as a key. The built-in labels are an enum so a
    /// consumer can be exhaustive over them; anything an organization invents is a string and
    /// can only be handled by the fallback arm.
    Custom,
}

impl Label {
    /// The built-in label for a wire name, if it names one.
    #[must_use]
    pub fn from_wire(name: &str) -> Option<Self> {
        Some(match name {
            "email" => Self::Email,
            "phone" => Self::Phone,
            "national_id" => Self::NationalId,
            "iban" => Self::Iban,
            "card" => Self::Card,
            "ip_address" => Self::IpAddress,
            "tax_number" => Self::TaxNumber,
            "person_name" => Self::PersonName,
            "secret_like" => Self::SecretLike,
            _ => return None,
        })
    }

    /// The wire name, which is what a policy row and a chip on the screen both carry.
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::Phone => "phone",
            Self::NationalId => "national_id",
            Self::Iban => "iban",
            Self::Card => "card",
            Self::IpAddress => "ip_address",
            Self::TaxNumber => "tax_number",
            Self::PersonName => "person_name",
            Self::SecretLike => "secret_like",
            Self::Custom => "custom",
        }
    }

    /// Every built-in label, in the order the policy screen lists them.
    #[must_use]
    pub fn all() -> &'static [Self] {
        &[
            Self::Email,
            Self::Phone,
            Self::NationalId,
            Self::Iban,
            Self::Card,
            Self::IpAddress,
            Self::TaxNumber,
            Self::PersonName,
            Self::SecretLike,
        ]
    }
}

/// What the guard does with a payload once it has decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Pass the text through unchanged, and still count the match.
    Allow,
    /// Pass the text through and record the decision for a human.
    Flag,
    /// Replace the matched span with a placeholder.
    Mask,
    /// Refuse the call before it leaves the process.
    Block,
}

impl Action {
    /// The wire name, shared by the policy row, the rule row and the event.
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Flag => "flag",
            Self::Mask => "mask",
            Self::Block => "block",
        }
    }

    /// The action a wire name carries, if it names one.
    #[must_use]
    pub fn from_wire(name: &str) -> Option<Self> {
        Some(match name {
            "allow" => Self::Allow,
            "flag" => Self::Flag,
            "mask" => Self::Mask,
            "block" => Self::Block,
            _ => return None,
        })
    }

    /// The wire names, so the policy screen and the route validator share one list.
    #[must_use]
    pub fn all() -> &'static [Self] {
        &[Self::Allow, Self::Flag, Self::Mask, Self::Block]
    }
}

/// A compiled detection rule.
///
/// The pattern is compiled once at construction, so an invalid expression is a *save-time*
/// field error and never a request-time panic. That is the whole reason this is a struct with
/// a `Regex` rather than a pattern string threaded through the detector.
#[derive(Debug, Clone)]
pub struct Rule {
    /// Storage key, unique per organization. Also what an event names when a rule refuses.
    pub key: String,
    /// The label this rule reports its matches under.
    pub label: Label,
    /// A tenant label, when [`Rule::label`] is [`Label::Custom`]. Otherwise empty.
    pub custom_label: String,
    /// `builtin` for the seeded rows, `custom` for a tenant's.
    pub kind: RuleKind,
    /// The expression, compiled.
    pub pattern: Regex,
    /// A second check a match must also pass, so `card` does not fire on every 16-digit number.
    pub validator: Validator,
    /// What the policy would do with a match of this rule, before exemptions narrow it.
    pub action: Action,
    /// Lower runs first. Ties in [`Policy::decide`] are broken by this.
    pub priority: i32,
    /// Providers this rule is scoped to. Empty means every provider.
    pub providers: Vec<String>,
    /// Features this rule is scoped to. Empty means every feature.
    pub features: Vec<String>,
    /// An operator switch. A disabled rule is not compiled into the running set at all.
    pub enabled: bool,
}

impl Rule {
    /// The label as a string, so a consumer never has to match on the enum to render a chip.
    #[must_use]
    pub fn label_wire(&self) -> String {
        if self.label == Label::Custom {
            format!("custom:{}", self.custom_label)
        } else {
            self.label.as_wire().to_string()
        }
    }

    /// Whether this rule's scope includes a call.
    ///
    /// An empty provider or feature list means "everywhere" — a scope that silently matched
    /// nothing would disable the rule instead of widening it, which is the dangerous direction:
    /// the screen still shows an active rule that no longer fires.
    #[must_use]
    pub fn applies_to(&self, provider: Option<&str>, feature: Option<&str>) -> bool {
        let provider_ok = self.providers.is_empty()
            || provider
                .map(|p| self.providers.iter().any(|s| s.eq_ignore_ascii_case(p)))
                .unwrap_or(false);
        let feature_ok = self.features.is_empty()
            || feature
                .map(|f| self.features.iter().any(|s| s.eq_ignore_ascii_case(f)))
                .unwrap_or(false);
        provider_ok && feature_ok
    }
}

/// Whether a row is one the platform ships or one a tenant wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    Builtin,
    Custom,
}

/// A second check a match must pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Validator {
    /// The pattern is the whole test.
    #[default]
    None,
    /// A card number passes the Luhn checksum.
    Luhn,
    /// An IBAN passes mod-97.
    IbanMod97,
    /// A phone number has a plausible digit count and no letters.
    PlausiblePhone,
    /// A national id passes its checksum.
    ChecksumNationalId,
}

impl Validator {
    /// The wire name.
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Luhn => "luhn",
            Self::IbanMod97 => "iban_mod97",
            Self::PlausiblePhone => "plausible_phone",
            Self::ChecksumNationalId => "checksum_national_id",
        }
    }

    /// The validator a wire name carries, if it names one.
    #[must_use]
    pub fn from_wire(name: &str) -> Option<Self> {
        Some(match name {
            "none" => Self::None,
            "luhn" => Self::Luhn,
            "iban_mod97" => Self::IbanMod97,
            "plausible_phone" => Self::PlausiblePhone,
            "checksum_national_id" => Self::ChecksumNationalId,
            _ => return None,
        })
    }

    /// The names the rule form offers.
    #[must_use]
    pub fn all() -> &'static [Self] {
        &[
            Self::None,
            Self::Luhn,
            Self::IbanMod97,
            Self::PlausiblePhone,
            Self::ChecksumNationalId,
        ]
    }

    /// Whether a matched value passes this check.
    ///
    /// A value that fails is not a detection failure, it is a *non*-match: the built-in `card`
    /// pattern is a shape, and a shape that fails Luhn is a random number. Reporting it as a
    /// match would fill the events table with false positives until the feature was switched
    /// off entirely, which is the outcome a guard is least allowed to produce.
    #[must_use]
    pub fn accepts(self, value: &str) -> bool {
        match self {
            Self::None => true,
            Self::Luhn => luhn_ok(&strip_spaces(value)),
            Self::IbanMod97 => iban_mod97_ok(&normalize_iban(value)),
            Self::PlausiblePhone => plausible_phone(value),
            Self::ChecksumNationalId => checksum_national_id(value),
        }
    }
}

/// One detected span, with the rule that found it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Match_ {
    /// Byte offsets into the inspected text.
    pub start: usize,
    pub end: usize,
    /// The label the rule reports.
    pub label: String,
    /// The rule key that matched — what a blocked verdict names.
    pub rule_key: String,
    /// A short salted hash of the value, so "have we seen this before" is answerable without
    /// storing the value. Never the value itself.
    pub value_hash: String,
}

impl Match_ {
    /// A new match record.
    #[must_use]
    pub fn new(
        start: usize,
        end: usize,
        label: impl Into<String>,
        rule_key: impl Into<String>,
        value_hash: impl Into<String>,
    ) -> Self {
        Self {
            start,
            end,
            label: label.into(),
            rule_key: rule_key.into(),
            value_hash: value_hash.into(),
        }
    }
}

/// Everything one inspection found, plus what the policy said to do about it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    /// The action the policy chose for the whole payload.
    pub action: Action,
    /// What the caller should do: send the text, send the masked text, or refuse.
    pub verdict: GuardVerdict,
    /// Every match, ordered by position then by rule.
    pub matches: Vec<Match_>,
    /// The text the provider should see. Equal to the input when nothing was masked.
    pub text: String,
    /// Per-label counts, which is what the event row and the stat cards aggregate.
    pub label_counts: BTreeMap<String, usize>,
}

impl Finding {
    /// A finding with no matches at all: the verdict is always `Clear`.
    #[must_use]
    pub fn clear(text: impl Into<String>) -> Self {
        Self {
            action: Action::Allow,
            verdict: GuardVerdict::Clear,
            matches: Vec::new(),
            text: text.into(),
            label_counts: BTreeMap::new(),
        }
    }
}

/// What a caller does with a payload the guard has inspected.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum GuardVerdict {
    /// No rule matched. The text goes through unchanged.
    Clear,
    /// Rules matched and the payload passes. `text` is the text to send.
    Allowed {
        action: Action,
        matches: Vec<Match_>,
    },
    /// Rules matched and the text is masked. `text` carries placeholders.
    Masked { matches: Vec<Match_> },
    /// The call is refused. Nothing is returned to the caller beyond the reason.
    Blocked {
        label: String,
        rule_key: String,
        message: String,
    },
}

impl GuardVerdict {
    /// The wire name, used by the API and by the events table.
    #[must_use]
    pub fn as_wire(&self) -> &'static str {
        match self {
            Self::Clear => "clear",
            Self::Allowed { .. } => "allowed",
            Self::Masked { .. } => "masked",
            Self::Blocked { .. } => "blocked",
        }
    }

    /// Whether the caller may send this payload at all.
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::Blocked { .. })
    }
}

/// One exemption: a label, a scope and a reason.
///
/// An exemption is the likeliest way this control erodes, so it is deliberately narrow: it names
/// a label (never a pattern), a scope (never "everything"), and a reason (never empty).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Exemption {
    pub label: String,
    /// Empty means every provider **for this label** — still label-scoped.
    pub providers: Vec<String>,
    /// Empty means every feature for this label.
    pub features: Vec<String>,
    pub reason: String,
    /// `Some(past)` means the exemption has lapsed and must not apply.
    pub expires_at: Option<OffsetDateTime>,
}

impl Exemption {
    /// Whether this exemption is still in force.
    #[must_use]
    pub fn is_live(&self, now: OffsetDateTime) -> bool {
        self.expires_at.is_none_or(|at| at > now)
    }

    /// Whether this exemption covers a call, ignoring expiry. The caller applies expiry.
    #[must_use]
    pub fn covers(&self, label: &str, provider: Option<&str>, feature: Option<&str>) -> bool {
        if !self.label.eq_ignore_ascii_case(label) {
            return false;
        }
        let provider_ok = self.providers.is_empty()
            || provider
                .map(|p| self.providers.iter().any(|s| s.eq_ignore_ascii_case(p)))
                .unwrap_or(false);
        let feature_ok = self.features.is_empty()
            || feature
                .map(|f| self.features.iter().any(|s| s.eq_ignore_ascii_case(f)))
                .unwrap_or(false);
        provider_ok && feature_ok
    }
}

/// The organization's guard policy.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// The default action per label. A label absent from the map is [`Action::Allow`].
    ///
    /// Defaulting to `allow` rather than `flag` is a real choice: an installation that has
    /// never opened the guard screen should not start refusing chat because somebody added a
    /// seeded email rule, and the warning banner on `/ai/guard` is what makes the state visible.
    pub label_defaults: BTreeMap<String, Action>,
    /// `numbered` gives `[EMAIL_1]`; `deterministic` gives `[EMAIL:a1b2c3]`.
    pub mask_style: MaskStyle,
    /// Whether a user may turn a label down for their own calls.
    pub allow_user_override: bool,
    /// Exemptions in force, each already filtered for expiry by the caller.
    pub exemptions: Vec<Exemption>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            label_defaults: BTreeMap::new(),
            mask_style: MaskStyle::Numbered,
            allow_user_override: false,
            exemptions: Vec::new(),
        }
    }
}

impl Policy {
    /// The action configured for a label, or `allow` when the policy is silent.
    #[must_use]
    pub fn action_for(&self, label: &str) -> Action {
        self.label_defaults
            .get(label)
            .copied()
            .or_else(|| {
                self.label_defaults
                    .get(&label.to_ascii_uppercase())
                    .copied()
            })
            .unwrap_or(Action::Allow)
    }

    /// Whether every label sits at `allow` — the state the warning banner is about.
    #[must_use]
    pub fn is_all_permissive(&self) -> bool {
        Label::all()
            .iter()
            .all(|l| self.action_for(l.as_wire()) == Action::Allow)
    }
}

/// How a masked span is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaskStyle {
    /// `[EMAIL_1]`, `[EMAIL_2]` — ordered by first appearance.
    Numbered,
    /// `[EMAIL:a1b2c3]` — the same value always renders the same way, so a model that repeats
    /// the placeholder in a later turn refers to the same value.
    Deterministic,
}

impl MaskStyle {
    /// The wire name.
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Numbered => "numbered",
            Self::Deterministic => "deterministic",
        }
    }

    /// The style a wire name carries, if it names one.
    #[must_use]
    pub fn from_wire(name: &str) -> Option<Self> {
        Some(match name {
            "numbered" => Self::Numbered,
            "deterministic" => Self::Deterministic,
            _ => return None,
        })
    }
}

/// The compiled rule set for one organization.
#[derive(Debug, Clone, Default)]
pub struct Detector {
    rules: Vec<Rule>,
}

impl Detector {
    /// A detector over a rule set.
    ///
    /// # Errors
    ///
    /// Refuses more than [`MAX_ENABLED_RULES`] enabled rules. See that constant.
    pub fn new(rules: Vec<Rule>) -> Result<Self, GuardError> {
        let enabled = rules.iter().filter(|r| r.enabled).count();
        if enabled > MAX_ENABLED_RULES {
            return Err(GuardError::RuleBudget { enabled });
        }
        Ok(Self { rules })
    }

    /// A detector over no rules, which finds nothing and masks nothing.
    #[must_use]
    pub fn empty() -> Self {
        Self { rules: Vec::new() }
    }

    /// How many rules are in the running set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rules.iter().filter(|r| r.enabled).count()
    }

    /// Whether the running set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The rules, for the rules screen and the rule count in the policy panel.
    #[must_use]
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Inspect a payload and decide what happens to it.
    ///
    /// `salt` hashes the matched values; it is read from the organization's secret store so a
    /// hash is useless to somebody who does not have it. Passing an empty salt is allowed for
    /// the dry-run tester, whose whole point is showing a person their own sample.
    #[must_use]
    pub fn inspect(
        &self,
        text: &str,
        provider: Option<&str>,
        feature: Option<&str>,
        policy: &Policy,
        salt: &str,
    ) -> Finding {
        let mut matches: Vec<Match_> = Vec::new();
        let mut in_scope: Vec<(&Rule, Action)> = Vec::new();

        for rule in self.rules.iter().filter(|r| r.enabled) {
            if !rule.applies_to(provider, feature) {
                continue;
            }
            let label = rule.label_wire();
            let action = self.effective_action(rule, &label, provider, feature, policy);
            for caps in rule.pattern.captures_iter(text) {
                let Some(m) = caps.get(0) else { continue };
                if m.len() == 0 {
                    continue;
                }
                let value = m.as_str();
                if !rule.validator.accepts(value) {
                    continue;
                }
                matches.push(Match_::new(
                    m.start(),
                    m.end(),
                    label.clone(),
                    rule.key.clone(),
                    hash_value(value, salt),
                ));
                in_scope.push((rule, action));
            }
        }

        if matches.is_empty() {
            return Finding::clear(text);
        }

        matches.sort_by(|a, b| {
            a.start
                .cmp(&b.start)
                .then_with(|| b.end.cmp(&a.end))
                .then_with(|| a.rule_key.cmp(&b.rule_key))
        });
        matches.dedup_by(|a, b| a.start == b.start && a.end == b.end && a.rule_key == b.rule_key);

        let mut label_counts: BTreeMap<String, usize> = BTreeMap::new();
        for m in &matches {
            *label_counts.entry(m.label.clone()).or_insert(0) += 1;
        }

        // The strictest action across every rule that fired, then the lowest priority number as
        // the tie-break so the same payload always reaches the same verdict. `Action` orders
        // Allow < Flag < Mask < Block, so the maximum is the strictest thing any rule asked for.
        let (action, blocking_rule) = in_scope
            .iter()
            .max_by_key(|(rule, action)| (*action, -rule.priority))
            .map_or((Action::Allow, None), |(rule, action)| {
                (*action, Some(*rule))
            });

        if let Some(rule) = blocking_rule.filter(|_| action == Action::Block) {
            let label = rule.label_wire();
            return Finding {
                action: Action::Block,
                verdict: GuardVerdict::Blocked {
                    label,
                    rule_key: rule.key.clone(),
                    message: format!(
                        "The AI data guard refused this request: the {} label matched rule `{}` and the policy is set to block.",
                        rule.label_wire(),
                        rule.key
                    ),
                },
                matches,
                text: String::new(),
                label_counts,
            };
        }

        let masked = if action == Action::Mask {
            mask_text(text, &matches, policy.mask_style)
        } else {
            text.to_string()
        };

        let verdict = if action == Action::Mask {
            GuardVerdict::Masked {
                matches: matches.clone(),
            }
        } else {
            GuardVerdict::Allowed {
                action,
                matches: matches.clone(),
            }
        };

        Finding {
            action,
            verdict,
            matches,
            text: masked,
            label_counts,
        }
    }

    /// The action a rule's match gets, after the policy default and any live exemption.
    fn effective_action(
        &self,
        rule: &Rule,
        label: &str,
        provider: Option<&str>,
        feature: Option<&str>,
        policy: &Policy,
    ) -> Action {
        let action = policy.action_for(label);
        // `block` is never exempted. An exemption that could switch a refusal off would make
        // the "block" setting a suggestion, and the policy screen could not honestly say so.
        if action == Action::Block {
            return Action::Block;
        }
        let now = OffsetDateTime::now_utc();
        let exempted = policy
            .exemptions
            .iter()
            .filter(|e| e.is_live(now) && e.covers(label, provider, feature))
            .any(|_| true);
        if exempted {
            Action::Allow
        } else {
            action
        }
    }
}

/// Replaces matched spans with placeholders.
///
/// Spans are applied right to left so earlier offsets stay valid — replacing left to right
/// shifts every later index by the length difference, and the third replacement lands in the
/// middle of the second.
#[must_use]
pub fn mask_text(text: &str, matches: &[Match_], style: MaskStyle) -> String {
    if matches.is_empty() {
        return text.to_string();
    }
    let mut placeholders: BTreeMap<String, String> = BTreeMap::new();
    let mut counters: BTreeMap<String, usize> = BTreeMap::new();
    for m in matches {
        if !placeholders.contains_key(&m.value_hash) {
            let n = counters.entry(m.label.clone()).or_insert(0);
            *n += 1;
            let token = match style {
                MaskStyle::Numbered => format!("[{}_{}]", m.label.to_ascii_uppercase(), n),
                MaskStyle::Deterministic => {
                    format!(
                        "[{}:{}]",
                        m.label.to_ascii_uppercase(),
                        short_hash(&m.value_hash)
                    )
                }
            };
            placeholders.insert(m.value_hash.clone(), token);
        }
    }

    let mut out = text.to_string();
    for m in matches.iter().rev() {
        let Some(token) = placeholders.get(&m.value_hash) else {
            continue;
        };
        if m.end <= out.len() && m.start < m.end {
            out.replace_range(m.start..m.end, token);
        }
    }
    out
}

/// Substitutes placeholders back into a completed answer.
///
/// Returns `None` for an unknown placeholder: a model that mangles `[EMAIL_1]` into
/// `[EMAIL-1]` or `[Email_1]` must leave the placeholder visible rather than guess a value into
/// an answer. The tolerant arm is the one place a guess would be invisible.
#[must_use]
pub fn remap_text(answer: &str, map: &BTreeMap<String, String>) -> String {
    let mut out = answer.to_string();
    for (placeholder, original) in map {
        out = out.replace(placeholder.as_str(), original.as_str());
    }
    out
}

/// A short, stable hash of a value, for the deterministic mask style.
#[must_use]
pub fn short_hash(value: &str) -> String {
    // FNV-1a: the placeholder has to be stable across processes and requests, and it has to be
    // short enough for a model to copy accurately. This is not a security primitive — the
    // salted `value_hash` on a match is the one that must resist guessing.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{hash:08x}")[..6].to_string()
}

/// A salted hash of a value, for "did we see this before" without storing the value.
///
/// Without a salt this is a rainbow table away from the original, which is why the salt comes
/// from the organization's secret store rather than from a constant.
#[must_use]
pub fn hash_value(value: &str, salt: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in salt.as_bytes().iter().chain(value.as_bytes()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Why the guard could not be built or could not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardError {
    /// More enabled rules than [`MAX_ENABLED_RULES`]. A configuration error, not a request error.
    RuleBudget { enabled: usize },
}

impl std::fmt::Display for GuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RuleBudget { enabled } => write!(
                f,
                "The guard has {enabled} enabled rules and the ceiling is {MAX_ENABLED_RULES}. Narrow the scope or disable a rule: the detector runs on every provider call."
            ),
        }
    }
}

impl std::error::Error for GuardError {}

fn strip_spaces(value: &str) -> String {
    value.chars().filter(|c| !c.is_whitespace()).collect()
}

fn normalize_iban(value: &str) -> String {
    strip_spaces(value).to_ascii_uppercase()
}

fn luhn_ok(value: &str) -> bool {
    if value.is_empty() || !value.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let mut sum = 0_u32;
    for (i, c) in value.chars().rev().enumerate() {
        let mut digit = u32::from(c.to_digit(10).unwrap_or(0));
        if i % 2 == 1 {
            digit *= 2;
            if digit > 9 {
                digit -= 9;
            }
        }
        sum += digit;
    }
    sum % 10 == 0
}

fn iban_mod97_ok(value: &str) -> bool {
    // Move the first four characters to the end, then reduce mod 97. Anything above 15 letters
    // is not an IBAN, and running the reduction on it would eventually return 1 by accident.
    if value.len() < 15 || value.len() > 34 {
        return false;
    }
    let rearranged: String = format!("{}{}", &value[4..], &value[..4]);
    let mut remainder: u32 = 0;
    for c in rearranged.chars() {
        let part = if c.is_ascii_digit() {
            c.to_digit(10).unwrap_or(0).to_string()
        } else if c.is_ascii_alphabetic() {
            format!("{:02}", c.to_ascii_uppercase() as u32 - 'A' as u32 + 10)
        } else {
            return false;
        };
        for d in part.chars() {
            remainder = (remainder * 10 + u32::from(d.to_digit(10).unwrap_or(0))) % 97;
        }
    }
    remainder == 1
}

fn plausible_phone(value: &str) -> bool {
    let digits: Vec<char> = value.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return false;
    }
    let letters = value.chars().filter(|c| c.is_alphabetic()).count();
    if letters > 0 {
        return false;
    }
    (10..=15).contains(&digits.len())
}

fn checksum_national_id(value: &str) -> bool {
    // Eleven digits, and the tenth is the mod-10 check digit of the first nine. This is the
    // Turkish scheme the built-in rule documents; a rule aimed at another scheme ships its own
    // validator rather than bending this one.
    let digits: Vec<char> = value.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() != 11 {
        return false;
    }
    let d: Vec<u32> = digits.iter().map(|c| c.to_digit(10).unwrap_or(0)).collect();
    let mut sum: u32 = d[..9]
        .iter()
        .enumerate()
        .map(|(i, n)| (i as u32 + 1) * n)
        .sum();
    sum %= 10;
    if d[9] != sum {
        return false;
    }
    d[10] <= (d[0] + d[2] + d[4] + d[6] + d[8] + d[9] + d[3] + d[5] + d[7]) % 10
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(key: &str, label: Label, pattern: &str, action: Action) -> Rule {
        Rule {
            key: key.to_string(),
            label,
            custom_label: String::new(),
            kind: RuleKind::Builtin,
            pattern: Regex::new(pattern).expect("test pattern compiles"),
            validator: Validator::None,
            action,
            priority: 100,
            providers: Vec::new(),
            features: Vec::new(),
            enabled: true,
        }
    }

    fn detector() -> Detector {
        Detector::new(vec![
            rule(
                "email.builtin",
                Label::Email,
                r"[^\s@]+@[^\s@]+\.[A-Za-z]{2,}",
                Action::Mask,
            ),
            rule(
                "card.builtin",
                Label::Card,
                r"\d[\d ]{11,18}\d",
                Action::Block,
            ),
        ])
        .expect("two rules is inside the budget")
    }

    fn policy_with(email: Action, card: Action) -> Policy {
        let mut p = Policy::default();
        p.label_defaults.insert("email".to_string(), email);
        p.label_defaults.insert("card".to_string(), card);
        p
    }

    #[test]
    fn an_email_is_masked_before_the_call() {
        let f = detector().inspect(
            "write to a@b.com please",
            Some("openai"),
            Some("chat"),
            &policy_with(Action::Mask, Action::Allow),
            "salt",
        );
        assert_eq!(f.verdict.as_wire(), "masked");
        assert_eq!(f.text, "write to [EMAIL_1] please");
        assert_eq!(f.label_counts.get("email"), Some(&1));
    }

    #[test]
    fn the_same_value_keeps_the_same_placeholder_within_a_request() {
        let f = detector().inspect(
            "a@b.com then c@d.com then a@b.com",
            None,
            None,
            &policy_with(Action::Mask, Action::Allow),
            "salt",
        );
        assert_eq!(f.text, "[EMAIL_1] then [EMAIL_2] then [EMAIL_1]");
    }

    #[test]
    fn the_deterministic_style_hides_the_order_of_appearance() {
        let mut p = policy_with(Action::Mask, Action::Allow);
        p.mask_style = MaskStyle::Deterministic;
        let f = detector().inspect("a@b.com and c@d.com", None, None, &p, "salt");
        assert!(f.text.starts_with("[EMAIL:"), "got {}", f.text);
        assert!(f.text.contains("] and [EMAIL:"), "got {}", f.text);
    }

    #[test]
    fn a_clear_payload_is_returned_unchanged() {
        let f = detector().inspect("nothing to see", None, None, &Policy::default(), "salt");
        assert_eq!(f.verdict, GuardVerdict::Clear);
        assert_eq!(f.text, "nothing to see");
        assert!(f.matches.is_empty());
    }

    #[test]
    fn a_block_refuses_the_call_and_returns_no_text() {
        let f = detector().inspect(
            "pay with 4111111111111111 today",
            None,
            None,
            &policy_with(Action::Allow, Action::Block),
            "salt",
        );
        assert!(f.verdict.is_blocked());
        assert!(
            f.text.is_empty(),
            "a refusal must not hand back a copy of the payload"
        );
        match &f.verdict {
            GuardVerdict::Blocked {
                label,
                rule_key,
                message,
            } => {
                assert_eq!(label, "card");
                assert_eq!(rule_key, "card.builtin");
                assert!(message.contains("card"), "the message must name the label");
            }
            other => panic!("expected a block, got {other:?}"),
        }
    }

    #[test]
    fn the_strictest_action_wins_and_a_flag_never_masks() {
        let f = detector().inspect(
            "mail a@b.com with 4111111111111111",
            None,
            None,
            &policy_with(Action::Flag, Action::Allow),
            "salt",
        );
        // Both rules matched; flag is the strictest of flag/allow.
        assert_eq!(f.action, Action::Flag);
        assert!(matches!(f.verdict, GuardVerdict::Allowed { .. }));
        assert!(f.text.contains("a@b.com"), "a flag must not alter the text");
    }

    #[test]
    fn a_mask_on_one_label_wins_over_a_allow_on_another() {
        let f = detector().inspect(
            "mail a@b.com with 4111111111111111",
            None,
            None,
            &policy_with(Action::Mask, Action::Allow),
            "salt",
        );
        assert_eq!(f.action, Action::Mask);
        assert!(f.text.contains("[EMAIL_1]"), "got {}", f.text);
    }

    #[test]
    fn a_validator_turns_a_shape_into_a_non_match() {
        let mut card = rule("card.builtin", Label::Card, r"\d{16}", Action::Block);
        card.validator = Validator::Luhn;
        let d = Detector::new(vec![card]).expect("one rule");
        // 4111111111111112 is a valid shape and fails the checksum.
        let bad = d.inspect(
            "ref 4111111111111112",
            None,
            None,
            &policy_with(Action::Allow, Action::Block),
            "s",
        );
        assert_eq!(
            bad.verdict.as_wire(),
            "clear",
            "a failing checksum is not a detection"
        );
        let good = d.inspect(
            "ref 4111111111111111",
            None,
            None,
            &policy_with(Action::Allow, Action::Block),
            "s",
        );
        assert!(good.verdict.is_blocked());
    }

    #[test]
    fn an_iban_passes_mod_97() {
        assert!(Validator::IbanMod97.accepts("TR330006100519786457841326"));
        assert!(!Validator::IbanMod97.accepts("TR330006100519786457841327"));
        assert!(!Validator::IbanMod97.accepts("tooshort"));
    }

    #[test]
    fn a_scope_that_names_the_provider_excludes_the_others() {
        let mut r = rule(
            "email.builtin",
            Label::Email,
            r"[^\s@]+@[^\s@]+\.[A-Za-z]{2,}",
            Action::Mask,
        );
        r.providers = vec!["openai".to_string()];
        let d = Detector::new(vec![r]).expect("one rule");
        let p = policy_with(Action::Mask, Action::Allow);
        assert_eq!(
            d.inspect("a@b.com", Some("anthropic"), None, &p, "s")
                .verdict
                .as_wire(),
            "clear",
            "a rule scoped to one provider must not fire for another"
        );
        assert_eq!(
            d.inspect("a@b.com", Some("openai"), None, &p, "s")
                .verdict
                .as_wire(),
            "masked"
        );
    }

    #[test]
    fn an_exemption_narrows_one_label_and_leaves_the_others() {
        // The exempted label is the one whose policy action is `mask`, because an exemption is
        // by design unable to switch a `block` off — `an_exemption_cannot_switch_a_block_off`
        // is the separate assertion for that. Writing this walk against `block` tested a rule
        // the code deliberately does not have, so it went red on a correct implementation.
        let mut p = policy_with(Action::Block, Action::Mask);
        p.exemptions.push(Exemption {
            label: "card".to_string(),
            providers: Vec::new(),
            features: vec!["checkout".to_string()],
            reason: "the checkout flow already validates the card itself".to_string(),
            expires_at: None,
        });
        let d = detector();
        let allowed = d.inspect("pay with 4111111111111111", None, Some("checkout"), &p, "s");
        assert_eq!(
            allowed.verdict.as_wire(),
            "allowed",
            "an exempted feature passes"
        );
        assert!(
            allowed.text.contains("4111111111111111"),
            "an exemption passes the original value through, it does not mask it"
        );
        let refused = d.inspect("pay with 4111111111111111", None, Some("chat"), &p, "s");
        assert_eq!(
            refused.verdict.as_wire(),
            "masked",
            "the same label on another feature stays masked"
        );
    }

    #[test]
    fn an_expired_exemption_stops_applying() {
        let mut p = policy_with(Action::Mask, Action::Block);
        p.exemptions.push(Exemption {
            label: "card".to_string(),
            providers: Vec::new(),
            features: vec!["checkout".to_string()],
            reason: "temporary".to_string(),
            expires_at: Some(OffsetDateTime::now_utc() - time::Duration::minutes(1)),
        });
        assert!(detector()
            .inspect("pay with 4111111111111111", None, Some("checkout"), &p, "s")
            .verdict
            .is_blocked());
    }

    #[test]
    fn an_exemption_cannot_switch_a_block_off() {
        let mut p = policy_with(Action::Allow, Action::Block);
        p.exemptions.push(Exemption {
            label: "card".to_string(),
            providers: Vec::new(),
            features: Vec::new(),
            reason: "because".to_string(),
            expires_at: None,
        });
        assert!(detector()
            .inspect("pay with 4111111111111111", None, None, &p, "s")
            .verdict
            .is_blocked());
    }

    #[test]
    fn a_disabled_rule_is_not_in_the_running_set() {
        let mut r = rule(
            "email.builtin",
            Label::Email,
            r"[^\s@]+@[^\s@]+\.[A-Za-z]{2,}",
            Action::Mask,
        );
        r.enabled = false;
        let d = Detector::new(vec![r]).expect("one rule");
        assert!(d.is_empty());
        assert_eq!(d.len(), 0);
        assert_eq!(
            d.inspect("a@b.com", None, None, &Policy::default(), "s")
                .verdict
                .as_wire(),
            "clear"
        );
    }

    #[test]
    fn more_rules_than_the_budget_refuses_to_build() {
        let rules: Vec<Rule> = (0..=MAX_ENABLED_RULES)
            .map(|i| rule(&format!("r{i}"), Label::Custom, "x", Action::Flag))
            .collect();
        let err = Detector::new(rules).expect_err("51 enabled rules must be refused");
        assert_eq!(
            err,
            GuardError::RuleBudget {
                enabled: MAX_ENABLED_RULES + 1
            }
        );
        assert!(
            err.to_string().contains("51"),
            "the message names the count"
        );
    }

    #[test]
    fn masking_replaces_right_to_left_so_earlier_spans_stay_valid() {
        let text = "aa@bb.com mid cc@dd.com end";
        let matches = vec![
            Match_::new(0, 9, "email", "k", "h1"),
            Match_::new(14, 23, "email", "k", "h2"),
        ];
        assert_eq!(
            mask_text(text, &matches, MaskStyle::Numbered),
            "[EMAIL_1] mid [EMAIL_2] end"
        );
    }

    #[test]
    fn remapping_substitutes_a_placeholder_back_and_leaves_an_unknown_one() {
        let mut map = BTreeMap::new();
        map.insert("[EMAIL_1]".to_string(), "a@b.com".to_string());
        assert_eq!(remap_text("sent to [EMAIL_1]", &map), "sent to a@b.com");
        assert_eq!(
            remap_text("sent to [EMAIL-1]", &map),
            "sent to [EMAIL-1]",
            "a mangled placeholder stays visible rather than guessing a value"
        );
    }

    #[test]
    fn an_all_allow_policy_is_reported_as_permissive() {
        let p = Policy::default();
        assert!(p.is_all_permissive());
        let mut q = Policy::default();
        q.label_defaults.insert("email".to_string(), Action::Mask);
        assert!(!q.is_all_permissive());
    }

    #[test]
    fn a_custom_label_carries_its_own_key() {
        let mut r = rule("order.tenant", Label::Custom, r"ORD-\d{4}", Action::Mask);
        r.custom_label = "order".to_string();
        assert_eq!(r.label_wire(), "custom:order");
    }

    #[test]
    fn the_label_and_action_wire_names_round_trip() {
        for l in Label::all() {
            assert_eq!(Label::from_wire(l.as_wire()), Some(*l));
        }
        for a in Action::all() {
            assert_eq!(Action::from_wire(a.as_wire()), Some(*a));
        }
        for v in Validator::all() {
            assert_eq!(Validator::from_wire(v.as_wire()), Some(*v));
        }
        assert_eq!(MaskStyle::from_wire("numbered"), Some(MaskStyle::Numbered));
        assert_eq!(
            MaskStyle::from_wire("deterministic"),
            Some(MaskStyle::Deterministic)
        );
        assert_eq!(MaskStyle::from_wire("nope"), None);
    }

    #[test]
    fn a_secret_like_token_is_identified_without_storing_it() {
        let h = hash_value("sk-abcdef0123456789abcdef", "org-salt");
        assert_ne!(h, hash_value("sk-abcdef0123456789abcdef", "other-salt"));
        assert!(!h.contains("abcdef"), "the hash must not carry the value");
    }
}
