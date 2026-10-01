//! The outbound checkpoint (REQ-105 slice 1).
//!
//! Everything else in the guard is a screen or a store. This module is the one place where a
//! payload is decided **before a provider can see it**, and the slice is closed by a property
//! rather than a shape: *a blocked payload never reaches the network, and an allowed one
//! arrives unchanged*.
//!
//! Two design decisions are worth stating, because both were the alternative and both are
//! plausible:
//!
//! 1. **The checkpoint takes the guard as an argument.** It does not load rules itself. Loading
//!    is a database round trip with a regex-compile, and this sits on the hot path of every AI
//!    request; a caller that already holds a [`crate::guard_store::LoadedGuard`] — the router, or
//!    a loop iterating several calls inside one organization — pays for it once. The test then
//!    also gets to hand in a [`Detector`] built from rules it wrote itself, with no database in
//!    the picture, which is what makes "blocked never leaves" a statement about *this* function
//!    rather than about the platform.
//! 2. **Auditing is best-effort and never changes the verdict.** The slice's own criterion for
//!    the event store is that the request's priority is the guard never silently changing the
//!    answer it reported. A database that refuses an audit row therefore logs and returns the
//!    finding untouched. Swallowing that error silently would be worse, so it is surfaced as an
//!    [`CheckpointReport::audit_error`] for the caller to decide on — the chat route warns.

use std::collections::BTreeMap;

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::AiHubError;
use crate::error::Result;
use crate::guard_data::{Action, GuardVerdict, MaskStyle, Policy};
use crate::guard_store::{self, LoadedGuard, NewEvent};

/// Who is making the call and what it is for. Everything the rules can be scoped by.
#[derive(Debug, Clone, Default)]
pub struct CheckpointContext {
    /// The tenant, when the caller has one.
    ///
    /// `None` is a real case rather than a hypothetical: a user with no organization is a normal
    /// account shape in this platform, and the rest of the AI family carries the organization as
    /// an `Option` for exactly that reason. Such a call is still **inspected** — silently
    /// skipping the guard for those users would make its coverage depend on how an account was
    /// created — but it cannot be audited per tenant, because `ai_guard_events.organization_id`
    /// is a real foreign key and a nil id would fail the insert. So the inspection runs and the
    /// event row is skipped, which is the honest outcome: a refusal still happens, and the events
    /// screen (which is per tenant) has nothing to show for a request that belongs to no tenant.
    pub organization_id: Option<Uuid>,
    /// The site, when the call is site-scoped.
    pub site_id: Option<Uuid>,
    /// The user whose message this is.
    pub user_id: Option<Uuid>,
    /// The agent run, when the call came out of one.
    pub run_id: Option<Uuid>,
    /// The provider that would answer — matched against a rule's provider scope.
    pub provider_id: Option<Uuid>,
    /// The provider's name. The rules screen stores provider *names*, so this is what scopes.
    pub provider: Option<String>,
    /// The feature key (`chat`, `agent`, …), the second scope axis.
    pub feature: Option<String>,
    /// Identifies this one request in the event log.
    pub request_id: Uuid,
}

impl CheckpointContext {
    /// A context for a plain call, with a caller-supplied request id.
    #[must_use]
    pub fn new(organization_id: Option<Uuid>, request_id: Uuid) -> Self {
        Self {
            organization_id,
            request_id,
            ..Self::default()
        }
    }
}

/// What the checkpoint did.
///
/// A `Masked` finding carries the text to send; `Allowed` carries the text unchanged. Both are
/// the *only* text that may reach a provider, and they are equal to the input in the `Clear`
/// and `Allowed` cases — which is the property the slice closes on.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointReport {
    /// The verdict the policy reached.
    pub verdict: GuardVerdict,
    /// The text a provider may see: the input, or the input with placeholders.
    pub outbound: String,
    /// Matches in the input, for the caller's own use. Never persisted.
    pub label_counts: BTreeMap<String, usize>,
    /// Total matches.
    pub match_count: i32,
    /// Which rules fired.
    pub rule_keys: Vec<String>,
    /// Salted hashes of the matched values. Hashes, never values — this is the only form in
    /// which a match may be recorded anywhere.
    pub value_hashes: Vec<String>,
    /// Set when the audit row could not be written. The verdict above still stands.
    pub audit_error: Option<String>,
    /// Placeholder → original for the spans this inspection replaced (REQ-105 slice 2).
    ///
    /// **`None` for a payload that was not masked**, which is not the same as an empty map: an
    /// empty map on a masked payload would mean the mask ran and its values could not be
    /// recovered, and a caller that treated the two alike would show a requester a placeholder
    /// forever with nothing behind it. `None` says "there is nothing to put back", which is the
    /// only true thing about a clear payload.
    pub remap: Option<crate::guard_remap::RemapMap>,
}

impl CheckpointReport {
    /// Whether this inspection replaced anything with a placeholder.
    ///
    /// A `match` on the verdict rather than a read of the map, because the verdict is the
    /// authority on what the policy did and the two can only be reconciled in one direction: a
    /// report whose verdict says `Masked` but whose map is empty means the mask ran and the
    /// values were lost, which is a fault to surface rather than paper over.
    #[must_use]
    pub fn is_masked(&self) -> bool {
        matches!(self.verdict, GuardVerdict::Masked { .. })
    }

    /// The requester's own view of an answer: placeholders replaced by the values behind them.
    ///
    /// Falls through unchanged when there is no map, so a caller does not branch on `Option` at
    /// every call site — and a payload that was never masked has nothing to substitute.
    #[must_use]
    pub fn substitute(&self, text: &str) -> String {
        self.remap
            .as_ref()
            .map_or_else(|| text.to_owned(), |map| map.substitute(text))
    }

    /// The view anyone else gets: originals turned back into placeholders.
    ///
    /// The mirror of [`Self::substitute`], and not redundant. An answer that quotes the value the
    /// user typed is *already* substituted in the text a caller holds, so a second reader of that
    /// same text needs the reverse substitution — otherwise the value leaks into an audit row, a
    /// shared transcript or an export while the guard screen still claims the payload was masked.
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        self.remap
            .as_ref()
            .map_or_else(|| text.to_owned(), |map| map.redact(text))
    }

    /// Whether any payload in this batch was masked.
    #[must_use]
    pub fn batch_is_masked(reports: &[Self]) -> bool {
        reports.iter().any(Self::is_masked)
    }

    /// The requester's view of a whole batch, substituted positionally.
    ///
    /// The batch case needs its own function because the reports and the texts are two parallel
    /// vectors, and pairing them is exactly where an off-by-one would substitute one turn's
    /// address into another turn's answer. A length mismatch substitutes nothing rather than
    /// guessing the pairing — the un-substituted text is visible, a wrong substitution is not.
    #[must_use]
    pub fn substitute_batch(reports: &[Self], texts: &[String]) -> Vec<String> {
        if reports.len() != texts.len() {
            return texts.to_vec();
        }
        reports
            .iter()
            .zip(texts.iter())
            .map(|(report, text)| report.substitute(text))
            .collect()
    }

    /// The one answer map for a whole request, merged across every message it contained.
    ///
    /// The batch helpers above pair reports with texts **positionally**, which is the right
    /// pairing for per-message decisions and the wrong one for an answer: the answer is a single
    /// text produced after every message was masked, and each message numbered its own
    /// `[EMAIL_1]`. Merging is therefore not a convenience but the only correct way to read one
    /// token across a conversation — see [`crate::guard_remap::RemapMap::merge`].
    #[must_use]
    pub fn merged_map(reports: &[Self]) -> crate::guard_remap::RemapMap {
        let maps: Vec<Option<crate::guard_remap::RemapMap>> =
            reports.iter().map(|report| report.remap.clone()).collect();
        crate::guard_remap::RemapMap::merge(&maps)
    }

    /// The redaction of a whole batch of texts.
    #[must_use]
    pub fn redact_batch(reports: &[Self], texts: &[String]) -> Vec<String> {
        if reports.len() != texts.len() {
            return texts.to_vec();
        }
        reports
            .iter()
            .zip(texts.iter())
            .map(|(report, text)| report.redact(text))
            .collect()
    }
}

impl CheckpointReport {
    /// Whether the caller may proceed to dial the provider.
    #[must_use]
    pub fn may_send(&self) -> bool {
        !self.verdict.is_blocked()
    }

    /// The [`AiHubError::GuardBlocked`] this report should be raised as.
    ///
    /// [`None`] when the call was not blocked — turning an allowed call into an error here would
    /// be the worst possible bug in this module, so it is a `match` and not a conversion.
    #[must_use]
    pub fn blocked_error(&self) -> Option<AiHubError> {
        match &self.verdict {
            GuardVerdict::Blocked {
                label,
                rule_key,
                message,
            } => Some(AiHubError::GuardBlocked {
                label: label.clone(),
                rule_key: rule_key.clone(),
                message: message.clone(),
            }),
            _ => None,
        }
    }
}

/// Run the outbound checkpoint.
///
/// This is the function a provider call must pass through. It never returns `Err` for a policy
/// decision: a blocked call is a correct answer, delivered as [`CheckpointReport::verdict`], and
/// it is the *caller* that turns it into an error so it can pick the right status code. What
/// can fail here is the guard's own configuration, which is an installation fault rather than a
/// user's.
pub async fn checkpoint(
    pool: &PgPool,
    guard: &LoadedGuard,
    ctx: &CheckpointContext,
    text: &str,
    salt: &str,
) -> Result<CheckpointReport> {
    let finding = guard.detector.inspect(
        text,
        ctx.provider.as_deref(),
        ctx.feature.as_deref(),
        &guard.policy,
        salt,
    );

    // A clear payload produces no event row at all. The audit is a record of the guard acting,
    // and a payload with nothing to guard is not the guard acting — writing one row per ordinary
    // message would drown the very events an operator opens this screen to read.
    if matches!(finding.verdict, GuardVerdict::Clear) {
        return Ok(CheckpointReport {
            verdict: finding.verdict,
            outbound: text.to_owned(),
            label_counts: finding.label_counts,
            match_count: 0,
            rule_keys: Vec::new(),
            value_hashes: Vec::new(),
            audit_error: None,
            remap: None,
        });
    }

    let rule_keys = collect_rule_keys(&finding.matches);
    let value_hashes = collect_hashes(&finding.matches);
    let action = finding.action.as_wire().to_owned();
    let match_count = i32::try_from(finding.matches.len()).unwrap_or(i32::MAX);
    let blocked = finding.verdict.is_blocked();

    // REQ-105 slice 2: the re-map is built HERE, and nowhere else, because this is the only frame
    // where the original text and the spans that index it are both in hand. Building it later —
    // in a route, from the masked outbound text — would mean recovering values by searching a
    // provider's own output for something shaped like a token, and a near-miss there writes the
    // wrong value into a real answer.
    //
    // `mask_tokens` re-derives the placeholders with the same ordinals `mask_text` used, so the
    // map and the mask come from one source and cannot disagree about which token is which value.
    // The map is built for a `Masked` verdict only: an `Allowed` payload passed through untouched
    // (an exemption), so there is nothing to put back and substituting would corrupt it.
    let remap = if matches!(finding.verdict, GuardVerdict::Masked { .. }) {
        let tokens = mask_tokens(&finding.matches, guard.policy.mask_style);
        Some(crate::guard_remap::RemapMap::from_matches(
            text,
            &finding.text,
            &finding.matches,
            &tokens,
        ))
    } else {
        None
    };

    // Audited before the verdict is returned, including for a blocked call: the refusal is
    // precisely the event nobody must be able to argue did not happen.
    //
    // Skipped when the caller has no organization: the event row's `organization_id` is a
    // foreign key, and a nil id would fail the insert for every call from an organization-less
    // account. The inspection above already happened, so a refusal still stands — only the audit
    // is missing, and the caller can see that in the absent event rather than in a wrong row.
    let audit_error = match ctx.organization_id {
        None => None,
        Some(organization_id) => {
            let event = NewEvent {
                organization_id,
                site_id: ctx.site_id,
                user_id: ctx.user_id,
                request_id: ctx.request_id,
                run_id: ctx.run_id,
                provider_id: ctx.provider_id,
                feature: ctx.feature.clone(),
                action: action.clone(),
                rule_keys: rule_keys.clone(),
                label_counts: finding.label_counts.clone(),
                match_count,
                value_hashes: value_hashes.clone(),
                error_code: blocked.then(|| "ai_guard_blocked".to_owned()),
            };
            match guard_store::record_event(pool, event).await {
                Ok(_) => None,
                Err(error) => Some(error.to_string()),
            }
        }
    };

    Ok(CheckpointReport {
        outbound: finding.text,
        verdict: finding.verdict,
        label_counts: finding.label_counts,
        match_count,
        rule_keys,
        value_hashes,
        audit_error,
        remap,
    })
}

/// The same checkpoint, but it raises the refusal as an error.
///
/// This is the shape a route wants: `?` on the call turns "the policy said no" into the right
/// `403`, and the audit row has already been written by the time the error propagates.
pub async fn checkpoint_or_refuse(
    pool: &PgPool,
    guard: &LoadedGuard,
    ctx: &CheckpointContext,
    text: &str,
    salt: &str,
) -> Result<CheckpointReport> {
    let report = checkpoint(pool, guard, ctx, text, salt).await?;
    if let Some(error) = report.blocked_error() {
        return Err(error);
    }
    Ok(report)
}

/// Load the guard for a tenant and run the checkpoint in one step.
///
/// For callers that make one call per request and have no reason to cache a compiled rule set
/// across calls; the router, which resolves many calls in one organization, uses [`checkpoint`]
/// with its own [`crate::guard_store::load_guard`].
pub async fn checkpoint_load(
    pool: &PgPool,
    ctx: &CheckpointContext,
    text: &str,
    salt: &str,
) -> Result<CheckpointReport> {
    // With no tenant there are only the platform rules, which `list_rules` returns for any
    // organization id; the nil id therefore asks for exactly that set and nothing else.
    let organization_id = ctx.organization_id.unwrap_or(Uuid::nil());
    let guard = guard_store::load_guard(pool, organization_id).await?;
    checkpoint_or_refuse(pool, &guard, ctx, text, salt).await
}

/// Inspect every message of an assembled prompt and return the texts to send.
///
/// The guard sits on the *assembled* payload, not on one string: a prompt is the system
/// instructions, the memory, the retrieved chunks and the turns, and a customer number split
/// across two of them is still a customer number. Each message is inspected separately so a
/// match carries a message index, and the **strictest verdict wins** — a conversation where one
/// turn is clean and another is not is a conversation that does not get sent.
///
/// Returns the masked texts positionally, plus the reports so the caller can audit once per
/// request instead of once per message.
pub async fn checkpoint_messages(
    pool: &PgPool,
    guard: &LoadedGuard,
    ctx: &CheckpointContext,
    messages: &[String],
    salt: &str,
) -> Result<(Vec<String>, Vec<CheckpointReport>)> {
    let mut outbound = Vec::with_capacity(messages.len());
    let mut reports = Vec::new();
    let mut worst: Option<usize> = None;

    for message in messages {
        let report = checkpoint(pool, guard, ctx, message, salt).await?;
        if report.verdict.is_blocked() && worst.is_none() {
            worst = Some(reports.len());
        }
        outbound.push(report.outbound.clone());
        reports.push(report);
    }

    if let Some(index) = worst {
        // Re-raise the *first* refusal's error: which turn was refused is the caller's detail
        // to add, and the sentence naming the label and the rule is already here.
        return Err(reports[index]
            .blocked_error()
            .unwrap_or(AiHubError::InvalidChatRequest(
                "the data guard refused this message".to_owned(),
            )));
    }

    Ok((outbound, reports))
}

/// The audit failures from a batch of reports, if any.
///
/// One audit row failing is not a reason to refuse a call, but it *is* something an operator
/// needs to see, so it is collected rather than dropped.
#[must_use]
pub fn audit_failures(reports: &[CheckpointReport]) -> Vec<String> {
    reports
        .iter()
        .filter_map(|report| report.audit_error.clone())
        .collect()
}

/// The placeholder a masked value was replaced with, for a given style.
///
/// Exposed because the tester's preview and the mask-style documentation both need to state the
/// shape without duplicating the format strings, which is how a numbered and a deterministic
/// placeholder drift apart and half the answers come back un-substituted.
#[must_use]
pub fn placeholder(style: MaskStyle, label: &str, ordinal: usize, value_hash: &str) -> String {
    match style {
        MaskStyle::Numbered => format!("[{}_{ordinal}]", label.to_uppercase()),
        MaskStyle::Deterministic => format!(
            "[{}:{}]",
            label.to_uppercase(),
            &value_hash[..value_hash.len().min(8)]
        ),
    }
}

/// The placeholder each distinct matched value was written as, keyed by value hash.
///
/// This exists because `mask_text` builds its tokens **inline** and does not return them, so
/// nothing else in the platform can know which token stands for which value. Slice 2 needs
/// exactly that, and the alternative — re-deriving the token in a second place — had already
/// drifted once: `mask_text` renders the deterministic style with
/// `guard_data::short_hash(value_hash)` while `placeholder()` (this file) rendered it with
/// `value_hash[..8]`, so a deterministic placeholder produced by one was not the one the other
/// looked for. Every answer would have come back with its placeholders still visible.
///
/// So the derivation lives in **one** place and `mask_text` is not asked to reproduce it: this
/// function is the single source, and the drift is impossible by construction rather than by
/// vigilance.
pub fn mask_tokens(
    matches: &[crate::guard_data::Match_],
    style: MaskStyle,
) -> BTreeMap<String, String> {
    let mut tokens: BTreeMap<String, String> = BTreeMap::new();
    let mut ordinals: BTreeMap<String, usize> = BTreeMap::new();
    for m in matches {
        if tokens.contains_key(&m.value_hash) {
            continue;
        }
        let n = ordinals.entry(m.label.clone()).or_insert(0);
        *n += 1;
        tokens.insert(
            m.value_hash.clone(),
            placeholder(style, &m.label, *n, &m.value_hash),
        );
    }
    tokens
}

fn collect_rule_keys(matches: &[crate::guard_data::Match_]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for m in matches {
        if !keys.contains(&m.rule_key) {
            keys.push(m.rule_key.clone());
        }
    }
    keys.sort();
    keys
}

fn collect_hashes(matches: &[crate::guard_data::Match_]) -> Vec<String> {
    let mut hashes: Vec<String> = Vec::new();
    for m in matches {
        if !hashes.contains(&m.value_hash) {
            hashes.push(m.value_hash.clone());
        }
    }
    hashes
}

/// The salt a real call should hash with, from the organization's secret store.
///
/// The value-hash is what makes an event row joinable ("the same address was seen twice")
/// without storing the address. A per-installation salt read from the secret store keeps a hash
/// in one tenant's log from being a lookup key in another's — and a *global constant* salt would
/// make every hash in the installation a rainbow-table target, which is the opposite of the
/// point. Missing configuration falls back to the organization id, which is not secret but is at
/// least not shared across tenants, and the `allow_user_override`-independent truth is that a
/// hash is only ever compared against hashes from the same tenant.
#[must_use]
pub fn value_salt(organization_id: Uuid, configured: Option<&str>) -> String {
    match configured.map(str::trim).filter(|s| !s.is_empty()) {
        Some(salt) => salt.to_owned(),
        None => organization_id.to_string(),
    }
}

/// The action a policy takes for a label when no rule says otherwise.
///
/// Re-exported so a caller building a report does not have to reach past this module for the
/// one enum value it needs.
pub fn action_name(action: Action) -> &'static str {
    action.as_wire()
}

/// A JSON view of a policy's per-label defaults, for a screen that wants one object rather than
/// a map walk.
#[must_use]
pub fn policy_defaults_json(policy: &Policy) -> Value {
    let mut map = serde_json::Map::new();
    for (label, action) in &policy.label_defaults {
        map.insert(label.clone(), Value::from(action.as_wire()));
    }
    Value::Object(map)
}

/// Whether every label in this policy is `allow` — the state the guard screen's warning banner
/// exists to make visible.
#[must_use]
pub fn is_all_permissive(policy: &Policy) -> bool {
    policy.is_all_permissive()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard_data::{Detector, Label, Match_, Rule, RuleKind, Validator};

    fn rule(key: &str, pattern: &str, action: Action) -> Rule {
        Rule {
            key: key.to_owned(),
            label: Label::Email,
            custom_label: String::new(),
            kind: RuleKind::Builtin,
            pattern: regex::Regex::new(pattern).expect("a valid pattern"),
            validator: Validator::None,
            action,
            priority: 100,
            providers: Vec::new(),
            features: Vec::new(),
            enabled: true,
        }
    }

    fn loaded(rules: Vec<Rule>, policy: Policy) -> LoadedGuard {
        LoadedGuard {
            detector: Detector::new(rules).expect("a budget within the cap"),
            policy,
        }
    }

    fn blocking_policy() -> Policy {
        Policy {
            label_defaults: BTreeMap::from([("email".to_owned(), Action::Block)]),
            ..Policy::default()
        }
    }

    fn match_at(start: usize, end: usize, key: &str) -> Match_ {
        Match_::new(
            start,
            end,
            "email".to_owned(),
            key.to_owned(),
            "deadbeef".to_owned(),
        )
    }

    #[test]
    fn a_blocked_payload_is_the_only_thing_that_becomes_an_error() {
        let guard = loaded(
            vec![rule("email_block", r"[a-z]+@[a-z]+\.com", Action::Block)],
            blocking_policy(),
        );
        // The pure half of the slice: no pool, no database, and the refusal is still exact.
        let finding = guard.detector.inspect(
            "write to ada@lovelace.com please",
            None,
            None,
            &guard.policy,
            "salt",
        );
        assert!(
            finding.verdict.is_blocked(),
            "the verdict was {:?}",
            finding.verdict
        );
    }

    #[test]
    fn a_clear_payload_sends_its_input_unchanged() {
        let guard = loaded(
            vec![rule("email_block", r"[a-z]+@[a-z]+\.com", Action::Block)],
            blocking_policy(),
        );
        let text = "hello there, nothing to guard";
        let finding = guard
            .detector
            .inspect(text, None, None, &guard.policy, "salt");
        assert_eq!(finding.verdict, GuardVerdict::Clear);
        assert_eq!(
            finding.text, text,
            "a clean payload must arrive byte-identical"
        );
    }

    #[test]
    fn a_masked_payload_replaces_the_value_and_keeps_the_rest() {
        let guard = loaded(
            vec![rule("email_mask", r"[a-z]+@[a-z]+\.com", Action::Mask)],
            Policy {
                label_defaults: BTreeMap::from([("email".to_owned(), Action::Mask)]),
                ..Policy::default()
            },
        );
        let finding = guard.detector.inspect(
            "write to ada@lovelace.com please",
            None,
            None,
            &guard.policy,
            "salt",
        );
        assert!(
            !finding.text.contains("ada@lovelace.com"),
            "the value survived the mask: {}",
            finding.text
        );
        assert!(finding.text.starts_with("write to "));
        assert!(finding.text.ends_with(" please"));
    }

    #[test]
    fn the_blocked_error_names_the_label_and_the_rule() {
        let guard = loaded(
            vec![rule("email_block", r"[a-z]+@[a-z]+\.com", Action::Block)],
            blocking_policy(),
        );
        let report = CheckpointReport {
            verdict: guard
                .detector
                .inspect("ada@lovelace.com", None, None, &guard.policy, "salt")
                .verdict,
            outbound: String::new(),
            label_counts: BTreeMap::new(),
            match_count: 0,
            rule_keys: vec!["email_block".to_owned()],
            value_hashes: Vec::new(),
            audit_error: None,
            remap: None,
        };
        let error = report.blocked_error().expect("a refusal raises an error");
        assert_eq!(error.code(), "ai_guard_blocked");
        let sentence = error.to_string();
        assert!(
            sentence.contains("email") && sentence.contains("email_block"),
            "the refusal must name the label and the rule, got: {sentence}"
        );
    }

    #[test]
    fn an_allowed_payload_never_becomes_an_error() {
        let guard = loaded(
            vec![rule("email_block", r"[a-z]+@[a-z]+\.com", Action::Block)],
            blocking_policy(),
        );
        let report = CheckpointReport {
            verdict: guard
                .detector
                .inspect("nothing here", None, None, &guard.policy, "salt")
                .verdict,
            outbound: "nothing here".to_owned(),
            label_counts: BTreeMap::new(),
            match_count: 0,
            rule_keys: Vec::new(),
            value_hashes: Vec::new(),
            audit_error: None,
            remap: None,
        };
        assert!(report.may_send());
        assert!(report.blocked_error().is_none());
    }

    #[test]
    fn rule_keys_are_unique_sorted_and_hashes_never_repeat() {
        let matches = vec![
            match_at(0, 5, "b_rule"),
            match_at(6, 9, "a_rule"),
            match_at(10, 12, "b_rule"),
        ];
        assert_eq!(collect_rule_keys(&matches), vec!["a_rule", "b_rule"]);
        assert_eq!(collect_hashes(&matches), vec!["deadbeef"]);
    }

    #[test]
    fn an_audit_failure_does_not_change_the_verdict() {
        let report = CheckpointReport {
            verdict: GuardVerdict::Clear,
            outbound: "text".to_owned(),
            label_counts: BTreeMap::new(),
            match_count: 0,
            rule_keys: Vec::new(),
            value_hashes: Vec::new(),
            audit_error: Some("connection refused".to_owned()),
            remap: None,
        };
        assert!(
            report.may_send(),
            "a missing audit row must not refuse a call"
        );
        assert_eq!(
            audit_failures(&[report]),
            vec!["connection refused".to_owned()]
        );
    }

    #[test]
    fn the_salt_is_never_shared_between_tenants_by_accident() {
        let one = Uuid::new_v4();
        let two = Uuid::new_v4();
        assert_ne!(value_salt(one, None), value_salt(two, None));
        assert_eq!(value_salt(one, Some("  ")), value_salt(one, None));
        assert_eq!(value_salt(one, Some("configured")), "configured");
    }

    #[test]
    fn both_mask_styles_produce_a_placeholder_the_caller_can_substitute() {
        let numbered = placeholder(MaskStyle::Numbered, "email", 2, "abcdef123456");
        assert_eq!(numbered, "[EMAIL_2]");
        let deterministic = placeholder(MaskStyle::Deterministic, "email", 2, "abcdef123456");
        assert_eq!(deterministic, "[EMAIL:abcdef12]");
        // A short hash must not panic on a slice past its end — the tester shows whatever the
        // digest produced, and a panic in the preview is a panic in the panel.
        assert_eq!(
            placeholder(MaskStyle::Deterministic, "iban", 1, "ab"),
            "[IBAN:ab]"
        );
    }

    #[test]
    fn an_all_allow_policy_is_recognised_as_the_warning_banner_state() {
        assert!(is_all_permissive(&Policy::default()));
        assert!(!is_all_permissive(&blocking_policy()));
    }

    #[test]
    fn the_wire_names_survive_the_json_view() {
        let json = policy_defaults_json(&Policy {
            label_defaults: BTreeMap::from([("email".to_owned(), Action::Block)]),
            ..Policy::default()
        });
        assert_eq!(json["email"], Value::from("block"));
        assert_eq!(action_name(Action::Mask), "mask");
    }
}
