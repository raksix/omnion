//! Which groups a `when_group` rule is allowed to see, and where each one came from.
//!
//! A `when_group` rule used to read exactly one thing: the group claim the token carried. That is
//! the whole story for an interactive sign-in — the IdP puts the person in `["engineering"]` and
//! the rule matches. It is **not** the story for a provisioned account, and the difference is the
//! whole reason this module exists.
//!
//! A SCIM connector creates the account and puts the person in a group through `/scim/v2/Groups`.
//! Nothing in that path ever produces an OIDC assertion, so on the next sign-in the token carries
//! whatever the IdP's own policy says and usually **no group claim at all** — the membership lives
//! in `group_members`, in the database, written by the connector. A rule that reads only the claim
//! therefore cannot match, and the failure looks like a misconfigured rule: the operator edits a
//! correct rule, re-runs the dry run against a sample that *does* carry a group claim, watches it
//! match, and concludes the rule is fine. It is not — it has never run against the person it
//! governs. This is the gap the `iam.group_membership_synced` event was emitted into and then left
//! pointing at: the event fires, and no rule reads its subject.
//!
//! So the evaluator takes its groups from an explicit [`GroupContext`] rather than from the
//! identity, and the caller says which sources it consulted. That is the design decision worth
//! stating, because the obvious alternative is a silent union:
//!
//! * **The sources are named, not merged.** A [`GroupSource`] carries the values *and* where they
//!   came from, and the dry run reports the same split. An operator whose rule did not fire can
//!   read "the token carried no groups, and the account belongs to none" instead of "the rule is
//!   wrong" — which is the difference between a five-minute fix and an afternoon.
//! * **Membership is a *separate* source with its own precedence, and a value coming from it says
//!   so.** [`GroupContext::values`] returns the union for matching (a person is in the group,
//!   that is the fact), while [`GroupContext::source_of`] reports which source supplied the
//!   winning value. A rule that fired on stored membership is a different operational fact from
//!   one that fired on the token, and the audit carries the difference.
//! * **The claim is authoritative when the two disagree.** An interactive IdP is the operator's
//!   live statement about who this person is *right now*; the database row is the connector's
//!   statement from *whenever it last ran*. When a token says `engineering` and the account is
//!   recorded in `contractors`, the union matches either rule and the operator has a real
//!   ambiguity. Rather than pick silently, [`GroupContext::authoritative`] reports the claim's
//!   values and the caller — the sign-in path — can decide. It does not: a grant is never
//!   withheld because a second source disagreed, because "the person is in the group" is true in
//!   both readings, and a role that is *not* granted because of a bookkeeping disagreement is
//!   indistinguishable from a broken rule. The disagreement is reported, not enforced.
//!
//! What this crate deliberately does **not** do is read the database. It has no pool, and the
//! membership query belongs to `omnion-permissions`, which owns the `groups` tables. Keeping the
//! split means the evaluator stays a pure function of two documents — which is the property the
//! dry run's value rests on — and the one place that reads a table is the one place that can be
//! proved against a real one.

use serde::{Deserialize, Serialize};

/// Above this a directory cannot have named more groups than this; the bound stops a connector
/// that sends a group's whole membership as one claim from turning a rule evaluation into a
/// memory problem. A directory in a real deployment has hundreds of groups at the very outside.
pub const MAX_GROUP_VALUES: usize = 512;

/// Where one set of group values came from.
///
/// The variant is data rather than a label because the two sources are not equally trustworthy
/// and the caller has to be able to tell them apart in an audit line: a group the *token* asserted
/// is a statement the identity provider made about a live sign-in, and a group the *account* is
/// recorded in is a statement a connector made whenever it last ran. Those have different
/// revocation behaviour, and an operator debugging "why did this person get the role" needs to be
/// able to read which one fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupSource {
    /// The group claim the provider's assertion or token carried. Live, per sign-in, and absent
    /// for a protocol that does not put groups in the token at all.
    Claim,
    /// The `group_members` rows a SCIM connector or an administrator wrote. Durable across
    /// sign-ins, and present for exactly the accounts that were provisioned rather than
    /// interactive.
    Membership,
}

impl GroupSource {
    /// The name the API and the audit use.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claim => "claim",
            Self::Membership => "membership",
        }
    }
}

/// The groups one identity is evaluated against, split by where each set came from.
///
/// The struct is deliberately *not* `impl From<Identity>`. Building it is a decision — which
/// sources did the caller consult, and did the membership read succeed — and a `From` would hide
/// that decision behind a conversion that every call site looks identical while meaning different
/// things.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupContext {
    /// The group claim the token carried.
    claim: Vec<String>,
    /// The groups the account is recorded in.
    membership: Vec<String>,
    /// Set when the membership read failed. A failed read is **not** an empty membership: an
    /// account whose groups could not be read must not evaluate as "in no groups", because that
    /// grants the default role to somebody who was about to be granted a real one.
    membership_unavailable: bool,
}

impl GroupContext {
    /// A context carrying only the token's claim — what an interactive sign-in has before any
    /// database read.
    #[must_use]
    pub fn from_claim(values: impl IntoIterator<Item = String>) -> Self {
        let mut context = Self::default();
        context.push_claim(values);
        context
    }

    /// A context built from both sources, the normal case for a provisioned account.
    #[must_use]
    pub fn new(
        claim: impl IntoIterator<Item = String>,
        membership: impl IntoIterator<Item = String>,
    ) -> Self {
        let mut context = Self::from_claim(claim);
        context.push_membership(membership);
        context
    }

    /// Add token group values, dropping blanks and duplicates.
    ///
    /// The values are compared case-insensitively, because a directory that says `Engineering`
    /// and an operator's rule that says `engineering` are the same group with two spellings, and
    /// the evaluator's `equals` operator is already case-insensitive. Trimming and de-duplicating
    /// here is what keeps a rule with a `regex` operator from matching a padded value that the
    /// panel displays without the padding.
    pub fn push_claim(&mut self, values: impl IntoIterator<Item = String>) {
        push_unique(&mut self.claim, values, MAX_GROUP_VALUES);
    }

    /// Add stored membership values, under the same normalisation.
    pub fn push_membership(&mut self, values: impl IntoIterator<Item = String>) {
        push_unique(&mut self.membership, values, MAX_GROUP_VALUES);
    }

    /// Record that the membership read failed, so [`Self::membership_unavailable`] is true.
    #[must_use]
    pub fn with_membership_unavailable(mut self) -> Self {
        self.membership_unavailable = true;
        self
    }

    /// Whether the membership read failed.
    ///
    /// The sign-in path refuses to resolve a *group* rule when this is true and returns a reason
    /// that says so, rather than resolving as though the person were in no groups: "no rule
    /// matched → default role" is a statement about the rules, and it would be false.
    #[must_use]
    pub const fn membership_unavailable(&self) -> bool {
        self.membership_unavailable
    }

    /// The token's group values.
    #[must_use]
    pub fn claim_values(&self) -> &[String] {
        &self.claim
    }

    /// The stored membership values.
    #[must_use]
    pub fn membership_values(&self) -> &[String] {
        &self.membership
    }

    /// Every value a group rule may compare against: the claim first, then the membership, each
    /// de-duplicated against the other.
    ///
    /// The order is the claim's own order, which is stable and is what the trace reports, so a
    /// dry run and a sign-in that agree on the match also agree on *which* value matched.
    #[must_use]
    pub fn values(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.claim.len() + self.membership.len());
        for value in &self.claim {
            out.push(value.clone());
        }
        for value in &self.membership {
            if !contains_ci(&out, value) {
                out.push(value.clone());
            }
        }
        out
    }

    /// Which source supplied `value`, if any of them did.
    ///
    /// This is the answer to "why did this rule fire", and it is deliberately not folded into
    /// the match: two values from different sources are the same fact to the evaluator and
    /// different facts to an operator.
    #[must_use]
    pub fn source_of(&self, value: &str) -> Option<GroupSource> {
        if contains_ci(&self.claim, value) {
            Some(GroupSource::Claim)
        } else if contains_ci(&self.membership, value) {
            Some(GroupSource::Membership)
        } else {
            None
        }
    }

    /// The values the *token* asserted, which are the ones an interactive directory considers
    /// live.
    ///
    /// A caller that has to choose between a stale record and a fresh assertion reads this. The
    /// sign-in path does not choose: it reports the disagreement in the audit reason and grants on
    /// the union, because a role withheld over a bookkeeping mismatch is a support ticket nobody
    /// can reproduce, while a role granted one time too wide is visible in the same audit line.
    #[must_use]
    pub fn authoritative(&self) -> &[String] {
        &self.claim
    }

    /// A one-word summary for the dry run and the audit: what a group rule would have had to read.
    ///
    /// The three shapes are the three states an operator actually has to tell apart, and they have
    /// three different fixes — configure a group claim, fix the connector, or fix the rule.
    #[must_use]
    pub fn summary(&self) -> GroupSummary {
        // The failure arm is tested **first**, and that ordering is the point. Matched last — as it
        // nearly was — `(false, true, _) => ClaimOnly` swallows a context whose membership read
        // failed but whose token carried a claim, and the two states then differ only in a flag
        // nobody renders. A read that failed is not an empty read, whatever the claim says, so it
        // has to be able to win the match.
        if self.membership_unavailable {
            return GroupSummary::MembershipUnavailable;
        }
        match (self.claim.is_empty(), self.membership.is_empty()) {
            (false, false) => GroupSummary::Both,
            (false, true) => GroupSummary::ClaimOnly,
            (true, false) => GroupSummary::MembershipOnly,
            (true, true) => GroupSummary::None,
        }
    }
}

/// What a group rule had to work with, in the vocabulary the panel renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupSummary {
    /// The token carried groups and the account is recorded in some.
    Both,
    /// The token carried groups; the account is in none stored.
    ClaimOnly,
    /// The token carried none; the account is recorded in some. **The provisioned case** — a
    /// connector put the person in a group and the IdP sends no claim, and a rule that reads
    /// only the claim cannot see this state at all.
    MembershipOnly,
    /// Neither source had anything.
    None,
    /// The membership could not be read, so "none" is not a fact about the person.
    MembershipUnavailable,
}

impl GroupSummary {
    /// The API's spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::ClaimOnly => "claim_only",
            Self::MembershipOnly => "membership_only",
            Self::None => "none",
            Self::MembershipUnavailable => "membership_unavailable",
        }
    }

    /// A sentence for the dry run and the audit.
    ///
    /// Each of the five says what it is *not*, because the sentence's job is to stop an operator
    /// from concluding the rule is broken.
    #[must_use]
    pub fn sentence(self) -> &'static str {
        match self {
            Self::Both => "groups came from the token claim and from stored membership",
            Self::ClaimOnly => {
                "groups came from the token claim; the account is in no stored group"
            }
            Self::MembershipOnly => {
                "the token carried no group claim; the account's stored groups were used"
            }
            Self::None => "no group claim and no stored membership, so a group rule cannot match",
            Self::MembershipUnavailable => {
                "stored membership could not be read, so a group rule was not evaluated"
            }
        }
    }
}

/// Push into `target`, skipping blanks and case-insensitive duplicates, up to `cap`.
fn push_unique(target: &mut Vec<String>, values: impl IntoIterator<Item = String>, cap: usize) {
    for value in values {
        if target.len() >= cap {
            return;
        }
        let trimmed = value.trim();
        if trimmed.is_empty() || contains_ci(target, trimmed) {
            continue;
        }
        target.push(trimmed.to_owned());
    }
}

fn contains_ci(haystack: &[String], needle: &str) -> bool {
    haystack
        .iter()
        .any(|value| value.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    #[test]
    fn a_claim_only_context_is_the_interactive_sign_in() {
        let context = GroupContext::from_claim(strings(&["engineering"]));
        assert_eq!(context.summary(), GroupSummary::ClaimOnly);
        assert_eq!(context.values(), strings(&["engineering"]));
        assert_eq!(context.source_of("engineering"), Some(GroupSource::Claim));
    }

    #[test]
    fn the_provisioned_case_is_visible_where_it_used_to_be_invisible() {
        // A SCIM connector put the person in a group; the IdP sends no group claim. Before the
        // membership source existed this context was indistinguishable from "in no groups", and a
        // rule written against it could never be debugged because the panel showed an empty list
        // rather than an empty *claim*.
        let context = GroupContext::new(Vec::new(), strings(&["engineering"]));
        assert_eq!(context.summary(), GroupSummary::MembershipOnly);
        assert_eq!(context.values(), strings(&["engineering"]));
        assert_eq!(
            context.source_of("engineering"),
            Some(GroupSource::Membership)
        );
        assert_eq!(
            GroupSummary::MembershipOnly.sentence(),
            "the token carried no group claim; the account's stored groups were used"
        );
    }

    #[test]
    fn a_value_in_both_sources_reads_as_the_claim_first() {
        // Both sources say it, so the live assertion is the one reported. The *set* is the same
        // either way; only the audit's "which source" differs, and the live one is the honest
        // answer to "what did the identity provider just say".
        let context = GroupContext::new(strings(&["engineering"]), strings(&["engineering"]));
        assert_eq!(context.summary(), GroupSummary::Both);
        assert_eq!(context.values(), strings(&["engineering"]));
        assert_eq!(context.source_of("engineering"), Some(GroupSource::Claim));
    }

    #[test]
    fn a_failed_membership_read_is_not_an_empty_membership() {
        // This is the whole reason the flag exists. Silently treating a failed read as "in no
        // groups" would resolve a group rule as a miss and grant the default role to somebody who
        // was about to be granted a real one — a privilege change caused by a database blip.
        let context =
            GroupContext::new(strings(&["engineering"]), Vec::new()).with_membership_unavailable();
        assert!(context.membership_unavailable());
        assert_eq!(context.summary(), GroupSummary::MembershipUnavailable);
        assert_ne!(
            context.summary(),
            GroupSummary::ClaimOnly,
            "a failed read must not collapse into the claim-only state"
        );
    }

    #[test]
    fn the_claim_is_the_authoritative_reading_when_the_sources_disagree() {
        let context = GroupContext::new(strings(&["engineering"]), strings(&["contractors"]));
        assert_eq!(
            context.authoritative(),
            strings(&["engineering"]).as_slice()
        );
        // …but the union still matches either, because both are true statements about different
        // things and withholding a grant over a bookkeeping mismatch is not an option.
        assert_eq!(
            context.values(),
            strings(&["engineering", "contractors"]),
            "a value from either source is a value the person holds"
        );
    }

    #[test]
    fn blank_and_duplicate_values_never_reach_a_rule() {
        let mut context = GroupContext::default();
        context.push_claim(strings(&["engineering", "  ", "Engineering", " oncall "]));
        assert_eq!(context.values(), strings(&["engineering", "oncall"]));
    }

    #[test]
    fn a_directory_cannot_make_a_rule_evaluation_unbounded() {
        let mut context = GroupContext::default();
        context.push_claim((0..MAX_GROUP_VALUES * 3).map(|i| format!("g{i}")));
        assert_eq!(context.values().len(), MAX_GROUP_VALUES);
    }

    #[test]
    fn an_unread_value_reports_no_source_rather_than_guessing() {
        let context = GroupContext::new(strings(&["engineering"]), Vec::new());
        assert_eq!(context.source_of("sales"), None);
    }
}
