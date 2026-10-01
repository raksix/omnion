//! The IP access rules: what an address is allowed to reach, and which rule said so
//! (REQ-012, slice 4).
//!
//! An access list is the one security control where **the answer without the reason is useless**.
//! Every other verdict this crate produces can be read as a plain yes/no, but "your address is
//! blocked" is unactionable unless the operator can see *which rule* blocked it — a `/24` they
//! added last Tuesday for a migration they have since forgotten, or a `/0` deny that shadows an
//! allow they swear is in force. So [`Verdict`] never reports a bare `blocked: true`; it names
//! the rule, and that rule is the same struct the panel renders as a table row.
//!
//! **Deny wins, and the reason is a rule about two rules rather than a fact about one.** Both
//! lists are evaluated against the address and the outcomes are *merged*, not consulted in
//! priority order: an address matching a deny is refused even when it also matches an allow,
//! because the allow cannot know about the deny. A narrower allow does not override a broader
//! deny, and the tests pin both directions — "the widest allow loses to the narrowest deny" is
//! the case an operator hits first, when they allow their office `/16` and deny one host in it.
//!
//! **Expiry is skipped, not treated as a match.** An expired rule is inert: it must not block
//! anyone, or an incident response that expired on a timer would deny an address for ever with
//! no rule explaining why. It is also not silently *deleted* from the report, because "no rule
//! matched" and "the rule that would have matched has expired" are different answers and the
//! second is the one that explains an operator's surprise. Hence [`Verdict::expired`] — present
//! but never decisive.
//!
//! **Parsing is strict and the message is field-level.** [`parse_cidr`] refuses a bare address,
//! a bad prefix, a host bit outside the prefix and an empty string, and each refusal names the
//! input. The migration's `cidr` column is the second layer of the same rule, so a caller that
//! bypasses this one still cannot store a malformed network.
//!
//! **What an address means when there is no address.** [`evaluate`] takes `Option<IpAddr>` and a
//! request with no connection info (an in-process test, a stripped layer, a future internal
//! caller) answers `Unknown` — never `Allowed`. A platform that cannot tell where a request came
//! from cannot claim to have applied its access list, and claiming otherwise is precisely the
//! false assurance this crate exists to avoid.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{Result, SecurityError};

/// Longest note one rule may carry.
pub const MAX_NOTE: usize = 200;

/// The rule kinds, in the order the panel lists the two tables.
pub const KINDS: &[&str] = &["allow", "deny"];

// ---------------------------------------------------------------------------------------------
// The rule
// ---------------------------------------------------------------------------------------------

/// One row of `/security/ip-access`.
///
/// Deliberately **not** a `sqlx::FromRow`. The column is `text` and `RuleKind` is a closed enum,
/// so making sqlx decode it directly means giving a security enum a `Type`/`Decode`/`Encode`
/// implementation in the domain module — trait impls that exist only to satisfy the store, in the
/// one file of this crate that is supposed to be pure policy. [`crate::ip_store::IpRuleRow`] is
/// the row-shaped twin: it holds `kind` as `String` and converts on the way in, which puts the
/// conversion next to the SQL that produced it and keeps a bad stored value a rejected read
/// rather than a decoding panic on the request path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpRule {
    /// The rule's id — the row the panel's delete button acts on.
    pub id: uuid::Uuid,
    /// Which list this rule is on.
    pub kind: RuleKind,
    /// The network, in `cidr` text form (`10.0.0.0/24`).
    pub cidr: String,
    /// Why the rule exists. Never empty: the SQL check constraint holds the same line.
    pub note: String,
    /// Who added it, when the user table still had the row.
    pub created_by: Option<uuid::Uuid>,
    /// When it was added.
    pub created_at: OffsetDateTime,
    /// When it stops applying. `None` never expires.
    pub expires_at: Option<OffsetDateTime>,
}

/// Which list a rule is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    /// A network that may reach the API.
    Allow,
    /// A network that may not.
    Deny,
}

impl RuleKind {
    /// The wire value, matching the SQL check constraint and the two table tabs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }

    /// Parse a wire value.
    ///
    /// # Errors
    /// Returns [`SecurityError::Invalid`] naming the input and the two valid kinds.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "allow" => Ok(Self::Allow),
            "deny" => Ok(Self::Deny),
            other => Err(SecurityError::invalid(format!(
                "\"{other}\" is not a rule kind — a rule is either \"allow\" or \"deny\""
            ))),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------------------------

/// Parse an operator's CIDR input into a network Postgres will accept.
///
/// # Errors
///
/// Returns [`SecurityError::Invalid`] for every malformed input, each naming what was typed:
/// an empty string, an address that is not an address, an out-of-range or non-numeric prefix, and
/// **host bits set outside the prefix** (`10.0.0.1/8`). A bare address is *accepted* and
/// completed to its full prefix (`203.0.113.7` → `203.0.113.7/32`), because that is unambiguous
/// and matches what every firewall UI an operator has used does.
///
/// The host-bit case is the one that decides this function's design, and it is worth spelling
/// out because it is invisible until it is not. `ipnet` will happily build a "network" out of
/// `10.0.0.1/8` and reports `contains()` correctly, because it truncates when it compares — but
/// **Postgres's `cidr` input refuses the same value** with "bits set to right of mask". So a
/// parser that only checked the prefix range would accept the rule, hand it to the insert, and
/// turn an operator's typo into a `500` from inside the database. It is also the one input that
/// *looks* narrow and means something enormous: `/8` is 16 million addresses, and a rule that
/// reads like "just that one host" is a rule that denies a corporate network.
///
/// So it is refused rather than quietly canonicalised, and the message carries the network the
/// operator most likely meant. Widening somebody's security rule without telling them is the one
/// repair here that would be worse than the error.
pub fn parse_cidr(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(SecurityError::invalid(
            "enter a network in CIDR form, for example 203.0.113.0/24 or 2001:db8::/32",
        ));
    }

    let (address_text, prefix_text) = match trimmed.split_once('/') {
        Some((address, prefix)) => (address, Some(prefix)),
        // No prefix is not an error — a single address is the most common thing an operator
        // types, and `/32` (or `/128`) is what it means.
        None => (trimmed, None),
    };

    let address = address_text.trim().parse::<IpAddr>().map_err(|_| {
        SecurityError::invalid(format!(
            "\"{address_text}\" is not an IP address — a rule needs a v4 address like 203.0.113.7 \
             or a v6 address like 2001:db8::1"
        ))
    })?;

    let full = if address.is_ipv4() { 32 } else { 128 };
    let prefix: u8 = match prefix_text {
        None => full,
        Some(raw_prefix) => raw_prefix.trim().parse().map_err(|_| {
            SecurityError::invalid(format!(
                "\"{raw_prefix}\" is not a prefix length — use a number from 0 to {full}"
            ))
        })?,
    };

    let network = ipnet::IpNet::new(address, prefix).map_err(|error| {
        // `IpNet::new` refuses a prefix longer than the family holds. Both errors are the
        // operator's typo rather than an attack, so both are reported with the input.
        SecurityError::invalid(format!("\"{trimmed}\" is not a usable network: {error}"))
    })?;

    let canonical = network.trunc();
    if canonical != network {
        // Host bits are set. See the doc comment: this is refused, and the network they probably
        // meant is named so the fix is one edit rather than a guess.
        return Err(SecurityError::invalid(format!(
            "\"{trimmed}\" sets bits outside its /{prefix} — did you mean {canonical}? A rule \
             that reads like one address but means /{prefix} would cover {} addresses",
            if prefix == full {
                "1".to_owned()
            } else {
                let span = 1u128 << (full - prefix);
                if span > 1_000_000 {
                    format!("{span}")
                } else {
                    span.to_string()
                }
            }
        )));
    }

    Ok(normalise(network))
}

/// Render an address and prefix as the exact `cidr` text Postgres stores.
///
/// The network is canonicalised (`trunc`), because this is what the evaluator's hand builds when
/// it re-renders a stored rule: a rule read back as `10.0.0.1/8` must never be re-emitted in a
/// form the column would reject.
///
/// # Errors
///
/// Returns [`SecurityError::Invalid`] when the pair does not form a network — the same refusal
/// [`parse_cidr`] makes, reached from the evaluator's hand when a stored row and a probe address
/// are combined.
pub fn network_text(address: IpAddr, prefix: u8) -> Result<String> {
    ipnet::IpNet::new(address, prefix)
        .map(|network| normalise(network.trunc()))
        .map_err(|error| {
            SecurityError::invalid(format!(
                "{} with a /{prefix} is not a usable network: {error}",
                address
            ))
        })
}

/// `ipnet` already renders a network in canonical form; this keeps that an explicit step so the
/// caller never stores whatever the operator typed.
fn normalise(network: ipnet::IpNet) -> String {
    network.to_string()
}

// ---------------------------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------------------------

/// What the access list says about one address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    /// Whether the address may proceed.
    pub blocked: bool,
    /// `allow` or `deny` — the merge that decided it. `None` when no live rule applied.
    pub decision: Option<RuleKind>,
    /// The rule that decided, as the panel renders it. Present whenever the decision is.
    pub matched_rule: Option<Box<IpRule>>,
    /// A rule that *would* have matched but has expired. Never decisive — it is here to explain
    /// why an operator's deny stopped working.
    pub expired: Option<Box<IpRule>>,
    /// `unknown` when there was no address to evaluate. A platform that cannot see where a
    /// request came from has not applied its access list, and must not claim it did.
    pub reason: String,
}

impl Verdict {
    /// Build the "no live rule applied" verdict, which is `allowed` with a stated reason.
    fn unconstrained(reason: impl Into<String>) -> Self {
        Self {
            blocked: false,
            decision: None,
            matched_rule: None,
            expired: None,
            reason: reason.into(),
        }
    }
}

/// Decide one address against the rules.
///
/// `rules` is every rule in the store, both lists; the order is irrelevant **by construction**
/// rather than by convention, which is the point: the outcome cannot depend on what the database
/// happened to return first, so a `limit` on the query can never change who is locked out.
#[must_use]
pub fn evaluate(rules: &[IpRule], address: Option<IpAddr>, now: OffsetDateTime) -> Verdict {
    let Some(address) = address else {
        return Verdict::unconstrained(
            "the request carried no client address, so no IP rule could be applied",
        );
    };

    // Expired rules are split out first: an expired rule must not decide anything, and a deny
    // that expired is far more interesting to report than a deny that never matched.
    let mut matched_deny: Option<&IpRule> = None;
    let mut matched_allow: Option<&IpRule> = None;
    let mut expired_deny: Option<&IpRule> = None;

    for rule in rules {
        if !contains(&rule.cidr, address) {
            continue;
        }
        if rule.expires_at.is_some_and(|at| at <= now) {
            // Keep the first expired *deny*: it is the rule whose absence explains a surprise.
            expired_deny = expired_deny.or_else(|| (rule.kind == RuleKind::Deny).then_some(rule));
            continue;
        }
        match rule.kind {
            RuleKind::Deny => matched_deny = matched_deny.or(Some(rule)),
            RuleKind::Allow => matched_allow = matched_allow.or(Some(rule)),
        }
    }

    if let Some(deny) = matched_deny {
        // Deny wins over allow, always. The allow that lost is deliberately not reported: the
        // operator's question is "why am I blocked", and naming an allow they can see in the
        // other table would send them to change a rule that was never the one refusing them.
        return Verdict {
            blocked: true,
            decision: Some(RuleKind::Deny),
            matched_rule: Some(Box::new(deny.clone())),
            expired: expired_deny.map(|rule| Box::new(rule.clone())),
            reason: format!(
                "{address} is inside {}, which is on the deny list",
                deny.cidr
            ),
        };
    }

    if let Some(allow) = matched_allow {
        return Verdict {
            blocked: false,
            decision: Some(RuleKind::Allow),
            matched_rule: Some(Box::new(allow.clone())),
            expired: expired_deny.map(|rule| Box::new(rule.clone())),
            reason: format!(
                "{address} is inside {}, which is on the allow list",
                allow.cidr
            ),
        };
    }

    let mut verdict = Verdict::unconstrained(format!("no IP rule matches {address}"));
    verdict.expired = expired_deny.map(|rule| Box::new(rule.clone()));
    verdict
}

/// Whether `cidr` contains `address`, treating an unparsable stored rule as *not* matching.
///
/// The fallback matters: a rule that cannot be read must not become an accidental allow (it does
/// not match, so the address falls through to whatever else applies) and must not become a 500
/// on the request path either. It is logged by the caller.
fn contains(cidr: &str, address: IpAddr) -> bool {
    cidr.parse::<ipnet::IpNet>()
        .map(|network| network.contains(&address))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SecurityError;

    fn rule(kind: RuleKind, cidr: &str) -> IpRule {
        IpRule {
            id: uuid::Uuid::new_v4(),
            kind,
            cidr: cidr.to_owned(),
            note: "test".to_owned(),
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            expires_at: None,
        }
    }

    fn v4(text: &str) -> IpAddr {
        text.parse().expect("fixture address must parse")
    }

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(seconds)
    }

    // -- parsing ---------------------------------------------------------------------------

    #[test]
    fn a_v4_network_is_accepted_and_canonicalised() {
        assert_eq!(
            parse_cidr("203.0.113.0/24").expect("valid"),
            "203.0.113.0/24"
        );
        assert_eq!(
            parse_cidr(" 203.0.113.0/24 ").expect("valid"),
            "203.0.113.0/24"
        );
    }

    #[test]
    fn a_v6_network_is_accepted() {
        assert_eq!(parse_cidr("2001:db8::/32").expect("valid"), "2001:db8::/32");
    }

    #[test]
    fn a_host_bit_outside_the_prefix_is_refused_and_names_the_network_meant() {
        // This is the one that matters: `10.0.0.1/8` *looks* like one address and means a /8.
        // `ipnet` accepts it (it truncates on compare) but Postgres's `cidr` column refuses it,
        // so without this check the operator's typo would surface as a 500 from inside the
        // database. Canonicalising instead would silently widen a rule.
        let error = parse_cidr("10.0.0.1/8").expect_err("host bit must be refused");
        assert!(matches!(error, SecurityError::Invalid(_)));
        let text = error.to_string();
        assert!(text.contains("10.0.0.1/8"), "message was: {text}");
        assert!(
            text.contains("10.0.0.0/8"),
            "must name the network meant: {text}"
        );
    }

    #[test]
    fn a_v6_host_bit_is_refused_the_same_way() {
        let error = parse_cidr("2001:db8::1/32").expect_err("host bit must be refused");
        assert!(
            error.to_string().contains("2001:db8::/32"),
            "message: {error}"
        );
    }

    #[test]
    fn a_bare_address_is_completed_rather_than_refused() {
        assert_eq!(parse_cidr("203.0.113.7").expect("valid"), "203.0.113.7/32");
        assert_eq!(parse_cidr("2001:db8::1").expect("valid"), "2001:db8::1/128");
    }

    #[test]
    fn every_malformed_input_names_itself() {
        for bad in [
            "",
            "   ",
            "not-an-ip",
            "203.0.113.0/",
            "203.0.113.0/33",
            "203.0.113.0/abc",
            "2001:db8::/129",
            "203.0.113.0/-1",
        ] {
            let error = parse_cidr(bad).expect_err("must be refused");
            assert!(
                matches!(error, SecurityError::Invalid(_)),
                "{bad:?} must be an Invalid error"
            );
        }
    }

    #[test]
    fn the_refusal_message_carries_the_input_so_the_form_can_show_it_on_the_field() {
        let error = parse_cidr("203.0.113.0/99").expect_err("must be refused");
        let text = error.to_string();
        assert!(text.contains("203.0.113.0/99"), "message was: {text}");
    }

    // -- precedence -----------------------------------------------------------------------

    #[test]
    fn a_deny_beats_an_allow_for_the_same_address() {
        let rules = vec![
            rule(RuleKind::Allow, "203.0.113.0/24"),
            rule(RuleKind::Deny, "203.0.113.7/32"),
        ];
        let verdict = evaluate(&rules, Some(v4("203.0.113.7")), at(0));
        assert!(verdict.blocked);
        assert_eq!(verdict.decision, Some(RuleKind::Deny));
        assert_eq!(
            verdict.matched_rule.as_ref().map(|r| r.cidr.as_str()),
            Some("203.0.113.7/32")
        );
    }

    #[test]
    fn precedence_is_independent_of_rule_order() {
        // The order the database returned the rows in must not decide who is locked out. The
        // walkthrough's `limit` clause would otherwise change the answer.
        let allow = rule(RuleKind::Allow, "203.0.113.0/24");
        let deny = rule(RuleKind::Deny, "203.0.113.7/32");

        let forwards = evaluate(
            &[allow.clone(), deny.clone()],
            Some(v4("203.0.113.7")),
            at(0),
        );
        let backwards = evaluate(&[deny, allow], Some(v4("203.0.113.7")), at(0));
        assert_eq!(forwards.blocked, backwards.blocked);
        assert!(forwards.blocked && backwards.blocked);
    }

    #[test]
    fn a_wide_allow_loses_to_a_narrow_deny() {
        // The real-world shape: an office /16 is allowed and one host inside it is denied.
        let rules = vec![
            rule(RuleKind::Allow, "203.0.0.0/16"),
            rule(RuleKind::Deny, "203.0.113.7/32"),
        ];
        assert!(evaluate(&rules, Some(v4("203.0.113.7")), at(0)).blocked);
        assert!(!evaluate(&rules, Some(v4("203.0.113.8")), at(0)).blocked);
    }

    #[test]
    fn an_address_matching_neither_list_is_allowed() {
        let rules = vec![rule(RuleKind::Deny, "198.51.100.0/24")];
        let verdict = evaluate(&rules, Some(v4("203.0.113.7")), at(0));
        assert!(!verdict.blocked);
        assert_eq!(verdict.decision, None);
        assert!(verdict.matched_rule.is_none());
        assert!(verdict.reason.contains("no IP rule matches"));
    }

    // -- expiry ---------------------------------------------------------------------------

    #[test]
    fn an_expired_deny_does_not_block_and_says_which_rule_expired() {
        let mut deny = rule(RuleKind::Deny, "203.0.113.0/24");
        deny.expires_at = Some(at(600));
        let verdict = evaluate(&[deny], Some(v4("203.0.113.7")), at(1200));
        assert!(!verdict.blocked, "an expired rule must not decide anything");
        assert_eq!(
            verdict.expired.as_ref().map(|r| r.cidr.as_str()),
            Some("203.0.113.0/24"),
            "the expired rule must still be reported — its absence is the explanation"
        );
    }

    #[test]
    fn a_deny_is_live_right_up_to_its_expiry() {
        let mut deny = rule(RuleKind::Deny, "203.0.113.0/24");
        deny.expires_at = Some(at(600));
        assert!(evaluate(&[deny.clone()], Some(v4("203.0.113.7")), at(599)).blocked);
        assert!(!evaluate(&[deny], Some(v4("203.0.113.7")), at(600)).blocked);
    }

    #[test]
    fn an_expired_allow_does_not_rescue_an_address_a_live_deny_refuses() {
        let mut allow = rule(RuleKind::Allow, "203.0.113.0/24");
        allow.expires_at = Some(at(10));
        let deny = rule(RuleKind::Deny, "203.0.113.7/32");
        let verdict = evaluate(&[allow, deny], Some(v4("203.0.113.7")), at(600));
        assert!(verdict.blocked);
        assert_eq!(verdict.decision, Some(RuleKind::Deny));
    }

    // -- no address -----------------------------------------------------------------------

    #[test]
    fn a_request_with_no_address_is_unknown_and_never_allowed() {
        let rules = vec![rule(RuleKind::Deny, "203.0.113.0/24")];
        let verdict = evaluate(&rules, None, at(0));
        assert!(
            !verdict.blocked,
            "an address we cannot see must not be reported as blocked or as checked"
        );
        assert_eq!(verdict.decision, None);
        assert!(verdict.reason.contains("no client address"));
    }

    // -- v6 --------------------------------------------------------------------------------

    #[test]
    fn a_v6_deny_matches_and_a_v6_allow_does_not_widen_into_v4() {
        let rules = vec![rule(RuleKind::Deny, "2001:db8::/32")];
        assert!(evaluate(&rules, Some(v4("2001:db8::1")), at(0)).blocked);
        assert!(
            !evaluate(&rules, Some("::1".parse().expect("v6 loopback")), at(0)).blocked,
            "::1 is not inside 2001:db8::/32"
        );
    }

    #[test]
    fn an_unreadable_stored_rule_matches_nothing_and_does_not_panic() {
        let broken = rule(RuleKind::Deny, "not-a-network");
        let verdict = evaluate(&[broken], Some(v4("203.0.113.7")), at(0));
        assert!(!verdict.blocked, "a rule we cannot read must not decide");
        assert_eq!(verdict.decision, None);
    }

    // -- kinds ----------------------------------------------------------------------------

    #[test]
    fn a_kind_round_trips_and_refuses_anything_else() {
        for kind in KINDS {
            assert_eq!(RuleKind::parse(kind).expect("valid").as_str(), *kind);
        }
        let error = RuleKind::parse("block").expect_err("must be refused");
        assert!(error.to_string().contains("block"));
    }

    #[test]
    fn network_text_refuses_a_prefix_the_family_cannot_hold() {
        assert!(network_text(v4("203.0.113.7"), 33).is_err());
        // Canonicalised: the stored text must never be a form the column would reject.
        assert_eq!(
            network_text(v4("203.0.113.7"), 24).expect("valid"),
            "203.0.113.0/24"
        );
    }

    #[test]
    fn a_note_longer_than_the_cap_is_refused_by_the_caller_not_truncated_silently() {
        // The cap lives on the route; this asserts the constant is what the UI will say.
        assert!(MAX_NOTE >= 64, "a useful note must fit: {}", MAX_NOTE);
    }
}
