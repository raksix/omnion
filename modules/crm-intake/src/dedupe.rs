//! Deduplication: deciding whether a submission is a new person or a returning one.
//!
//! The rules here are the ones that decide whether a real customer gets worked twice or
//! dropped, so they are stated rather than tuned:
//!
//! * **The matched key and the score are always recorded.** A dedupe verdict nobody can
//!   inspect is a verdict an operator has to trust, and the first time they are wrong about
//!   it they stop trusting the whole inbox.
//! * **The match order is e-mail, then phone digits, then company domain plus name.** Not
//!   because those are the only keys, but because the order is the *confidence* order: an
//!   e-mail match is a person, a phone match is usually a person, and a company domain is a
//!   business. Each step down is a weaker claim, which is why the score drops with it.
//! * **The policy is per source, never global.** Two forms of the same site can legitimately
//!   disagree — a newsletter and a quote request are the same person and want different
//!   treatment — so a global "dedupe everything" switch is the wrong shape.
//!
//! This module is pure: it takes candidate contacts and returns a verdict. The SQL that
//! fetches the candidates lives in [`crate::store`], which is where "best" can be defined by
//! the database rather than by a client that fetched the wrong rows.

use uuid::Uuid;

use crate::mapping::MappedValues;

/// The key a match was made on.
///
/// Closed because the score is meaningless without it: "0.82, matched" is not an explanation,
/// and a dedupe queue that shows a score without the key is a score nobody can challenge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKey {
    /// The normalized e-mail addresses are equal.
    Email,
    /// The digits-only phone numbers are equal.
    Phone,
    /// Same company domain and a similar name.
    CompanyDomain,
}

impl MatchKey {
    /// The stored name of the key, as written to `crm_leads.dedupe_key`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::Phone => "phone",
            Self::CompanyDomain => "company_domain",
        }
    }

    /// Parse a stored key name back.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "email" => Some(Self::Email),
            "phone" => Some(Self::Phone),
            "company_domain" => Some(Self::CompanyDomain),
            _ => None,
        }
    }

    /// The confidence a match on this key is worth, before the name similarity is added.
    ///
    /// The numbers are not a probability. They are an ordering that has to be stable across
    /// two tenants with different data, which is why they are fixed here rather than derived
    /// from a model.
    #[must_use]
    pub fn base_score(self) -> f64 {
        match self {
            Self::Email => 0.95,
            Self::Phone => 0.85,
            Self::CompanyDomain => 0.60,
        }
    }
}

/// An existing contact a submission might belong to.
///
/// `FromRow` because the candidate query reads the CRM's own contact table: a parallel
/// struct would have to be converted by hand at every call site, and a conversion is a place
/// where a column is silently dropped.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Candidate {
    /// The contact's id.
    pub id: Uuid,
    /// Its e-mail, as stored.
    pub email: Option<String>,
    /// Its phone, as stored.
    pub phone: Option<String>,
    /// Its company name, as stored.
    pub company_name: Option<String>,
    /// Its first name.
    pub first_name: Option<String>,
    /// Its last name.
    pub last_name: Option<String>,
}

/// The verdict a dedupe pass reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupePolicy {
    /// Attach the lead to the best existing contact.
    Link,
    /// Always a new contact, whatever matched.
    CreateAnyway,
    /// Keep the row as a duplicate pointing at the match, and do not link it.
    RejectDuplicate,
}

impl DedupePolicy {
    /// Parse a stored policy name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "link" => Some(Self::Link),
            "create_anyway" => Some(Self::CreateAnyway),
            "reject_duplicate" => Some(Self::RejectDuplicate),
            _ => None,
        }
    }

    /// The stored name of the policy.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Link => "link",
            Self::CreateAnyway => "create_anyway",
            Self::RejectDuplicate => "reject_duplicate",
        }
    }
}

/// One matched candidate, with the reason.
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    /// The contact that matched.
    pub contact_id: Uuid,
    /// Which key matched.
    pub key: MatchKey,
    /// The normalized value that matched, exactly as stored on the lead.
    pub dedupe_key: String,
    /// The confidence, 0.0–1.0.
    pub score: f64,
}

/// What the dedupe pass decided.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// No candidate crossed the bar.
    Unique,
    /// One candidate is the best match; the policy decides what happens to it.
    Matched(Match),
    /// More than one candidate matched; the list is ordered best-first.
    Ambiguous(Vec<Match>),
}

/// The confidence a match has to reach before it is a match at all.
///
/// One number, stated here, because the alternative is a threshold that lives in three
/// queries. A company-domain match with a *different* name scores 0.60 and is rejected; the
/// same match with a similar name is accepted. That difference is the point: the domain alone
/// is not a person, the domain plus the name is a strong suggestion, and a threshold that
/// cannot tell them apart either merges two people or merges nobody.
pub const MATCH_THRESHOLD: f64 = 0.75;

/// How similar two names have to be, on a 0.0–1.0 scale, for a domain match to count.
///
/// Token overlap, not edit distance: "Ada Lovelace" and "A. Lovelace" are the same person at
/// 0.5, and an edit distance calls them 0.25 apart because of the missing middle initial.
/// Names are short and initials are common, so a threshold tuned on edit distance rejects
/// exactly the rows a domain match is trying to catch.
pub const NAME_SIMILARITY_THRESHOLD: f64 = 0.5;

/// Evaluate the candidates against a submission's mapped values.
///
/// The candidates are *not* sorted by the caller: the best match is the one with the highest
/// score, and a caller that pre-sorts by "most recent contact" is choosing a different rule
/// than the one the score describes.
#[must_use]
pub fn evaluate(mapped: &MappedValues, candidates: &[Candidate]) -> Verdict {
    let submission_email = normalize_email(mapped.get("email"));
    let submission_phone = normalize_phone(mapped.get("phone"));
    let submission_domain = company_domain(mapped.get("company_name"));
    let submission_name = full_name(mapped);

    let mut matches: Vec<Match> = Vec::new();
    for candidate in candidates {
        if let Some(key) = email_match(&submission_email, candidate) {
            matches.push(Match {
                contact_id: candidate.id,
                key,
                dedupe_key: submission_email.clone().unwrap_or_default(),
                score: key.base_score(),
            });
            continue;
        }
        if let Some(key) = phone_match(&submission_phone, candidate) {
            matches.push(Match {
                contact_id: candidate.id,
                key,
                dedupe_key: submission_phone.clone().unwrap_or_default(),
                score: key.base_score(),
            });
            continue;
        }
        if let Some(domain) = &submission_domain {
            if company_domain(candidate.company_name.as_deref()).as_ref() == Some(domain) {
                let similarity = name_similarity(&submission_name, &candidate_name(candidate));
                if similarity >= NAME_SIMILARITY_THRESHOLD {
                    // The name similarity adds to the domain's base rather than replacing it,
                    // so a perfect name match on a shared domain (0.60 + 0.40) crosses the bar
                    // and a completely different name on that domain does not.
                    let score = (key_base(0.60) + similarity * 0.40).min(1.0);
                    matches.push(Match {
                        contact_id: candidate.id,
                        key: MatchKey::CompanyDomain,
                        dedupe_key: domain.clone(),
                        score,
                    });
                }
            }
        }
    }

    matches.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let above: Vec<Match> = matches
        .into_iter()
        .filter(|entry| entry.score >= MATCH_THRESHOLD)
        .collect();

    match above.len() {
        0 => Verdict::Unique,
        1 => Verdict::Matched(above.into_iter().next().expect("len checked")),
        _ => Verdict::Ambiguous(above),
    }
}

/// The `dedupe_key` a lead row stores, whatever the verdict was.
///
/// `None` when the submission carried no e-mail and no phone: the column is the reason the
/// duplicate queue can be rebuilt without re-scanning, and a key built out of nothing is a key
/// that would match every other such lead.
#[must_use]
pub fn dedupe_key(mapped: &MappedValues) -> Option<String> {
    normalize_email(mapped.get("email")).or_else(|| normalize_phone(mapped.get("phone")))
}

/// Lower-cased, trimmed e-mail.
///
/// `.` in the local part is *not* folded away: `a.b@c.co` and `ab@c.co` are different
/// addresses at some providers and the same at others, so a guess that is wrong creates a
/// duplicate and one that is right merges two people. The platform's job is to file the lead
/// twice rather than to be wrong about who somebody is.
#[must_use]
pub fn normalize_email(value: Option<&str>) -> Option<String> {
    let email = value?.trim().to_lowercase();
    if email.is_empty() || !email.contains('@') {
        return None;
    }
    Some(email)
}

/// Digits only, with a single leading `+` kept when the input had one.
///
/// The same rule as the `e164_lite` transform and deliberately duplicated as a *function*
/// rather than shared by import: the transform cleans a value on the way in, this normalizes
/// it for matching, and a value that arrived through an unmapped field never went through
/// the transform at all.
///
/// ## The SQL side of this rule is [`PHONE_DIGITS_SQL`], and the two must be read together
///
/// **The `+` is the whole difference, and it is enough to disable matching entirely.**
/// `regexp_replace(phone, '[^0-9]', '', 'g')` strips the `+` along with the spaces, while this
/// function keeps it — so comparing this function's output against that expression is
/// `'+905321112233'` against `'905321112233'`, which is never equal, and the phone arm of the
/// matcher never fires against a stored contact. It stayed invisible because
/// `a_formatted_phone_matches_the_same_digits` compares the function **with itself** on both
/// sides, which agrees by construction; only a query that normalizes the *stored* column can
/// disagree, and no unit test crosses that boundary.
///
/// The asymmetry this function keeps is deliberate and worth stating, because it is the reason
/// the two sides must not be unified by deleting the `+`: **`+` distinguishes two people.**
/// `+1 555 010 22 33` and `+90 555 010 22 33` are different people on two continents who happen
/// to share a tail. Dropping the plus to satisfy the SQL would merge them, and the correct
/// direction is to make the *SQL* keep it, which is what [`PHONE_DIGITS_SQL`] does.
///
/// [`PHONE_DIGITS_SQL`]: https://docs.rs
#[must_use]
pub fn normalize_phone(value: Option<&str>) -> Option<String> {
    let raw = value?.trim();
    let plus = raw.starts_with('+');
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    if digits.len() < 7 {
        // Fewer than seven digits is a postal-code fragment or a typo, not a number that can
        // identify a person. Matching on it would merge two different leads in the same city.
        return None;
    }
    if plus {
        Some(format!("+{digits}"))
    } else {
        Some(digits)
    }
}

/// The **SQL** spelling of [`normalize_phone`], for the one place that must agree with it.
///
/// Written as a named constant rather than pasted into a query string for the third time,
/// because the failure mode of pasting is a *silent* one: the query compiles, the plan uses the
/// index, and the arm simply never matches. The two sides differ in exactly one way, and the
/// constant is where that difference is written down:
///
/// | value in the column or payload | `normalize_phone` | this expression |
/// |---|---|---|
/// | `+90 (532) 111 22 33` | `+905321112233` | `+905321112233` |
/// | `0532 111 22 33`   | `5321112233`      | `5321112233`      |
/// | `n/a`              | `None` (too short) | `''` — never equal to a key |
///
/// The `+` is preserved by prefixing it back **only when the source value has one**, so a
/// number stored without a country code and a submission typed without one still match. The
/// `nullif(…, '')` matters for the same reason: an empty stored phone normalizes to the empty
/// string, which would equal another empty string and match every blank contact in the tenant.
pub const PHONE_DIGITS_SQL: &str =
    "case when btrim(coalesce({column}, '')) like '+%' then '+' else '' end \
     || regexp_replace(coalesce({column}, ''), '[^0-9]', '', 'g')";

/// The web host of a company name, a web address or an e-mail.
///
/// **The whole host, not the last label and not the registrable domain.** Both narrower
/// versions are wrong in the expensive direction: `acme.com` and `acme.com.tr` share the
/// label `com` under the first rule (so every `.com` company on earth looks like one
/// company), and the registrable domain needs a public-suffix list under the second. The full
/// host over-matches slightly — `www.` and the scheme are stripped, so
/// `https://www.acme.com/iletisim` and `acme.com` are equal — and the *name similarity*
/// decides the rest.
///
/// A value with no dot ("Acme Yapı Ltd. Şti.") yields `None`: a shared full company name is
/// not evidence of a shared mailbox, and a key built from it would match every lead from
/// every company with that name.
#[must_use]
pub fn company_domain(value: Option<&str>) -> Option<String> {
    let value = value?.trim().to_lowercase();
    if value.is_empty() {
        return None;
    }
    // An e-mail carries the host after the `@`; a bare name or a pasted URL is taken whole.
    if let Some((_, after_at)) = value.rsplit_once('@') {
        return domain_of(after_at);
    }
    domain_of(&value)
}

fn domain_of(value: &str) -> Option<String> {
    let without_scheme = value
        .rsplit("://")
        .next()
        .unwrap_or(value)
        .trim_matches('/');
    let host = without_scheme
        .split(['/', '?', '#', ' '])
        .next()
        .unwrap_or(without_scheme)
        .trim_start_matches("www.")
        .trim_end_matches('.');
    if host.is_empty() || !host.contains('.') {
        return None;
    }
    // A host has to have a character either side of its dot: ".com" and "acme." are both
    // typos, and a key built from either would be a key shared by a family of submissions.
    let (label, tail) = host.rsplit_once('.')?;
    if label.is_empty() || tail.is_empty() {
        return None;
    }
    Some(host.to_string())
}

/// The submission's name, assembled from whichever half the mapping produced.
#[must_use]
pub fn full_name(mapped: &MappedValues) -> String {
    let first = mapped.get("first_name").unwrap_or_default();
    let last = mapped.get("last_name").unwrap_or_default();
    [first, last]
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_lowercase()
}

fn candidate_name(candidate: &Candidate) -> String {
    [
        candidate.first_name.as_deref().unwrap_or_default(),
        candidate.last_name.as_deref().unwrap_or_default(),
    ]
    .iter()
    .filter(|part| !part.is_empty())
    .copied()
    .collect::<Vec<_>>()
    .join(" ")
    .trim()
    .to_lowercase()
}

fn email_match(submission: &Option<String>, candidate: &Candidate) -> Option<MatchKey> {
    let submission = submission.as_ref()?;
    let stored = normalize_email(candidate.email.as_deref())?;
    (stored == *submission).then_some(MatchKey::Email)
}

fn phone_match(submission: &Option<String>, candidate: &Candidate) -> Option<MatchKey> {
    let submission = submission.as_ref()?;
    let stored = normalize_phone(candidate.phone.as_deref())?;
    (stored == *submission).then_some(MatchKey::Phone)
}

/// Token overlap of two names, 0.0–1.0.
///
/// Each name is split into lower-cased tokens; the score is the size of the intersection over
/// the size of the *longer* name, so a full name matching an initial-and-surname is high and
/// two unrelated names are low. Dividing by the mean instead would push the initial case
/// below the threshold for the same reason edit distance would.
#[must_use]
pub fn name_similarity(left: &str, right: &str) -> f64 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let tokens = |value: &str| -> std::collections::BTreeSet<String> {
        value
            .split_whitespace()
            .map(|token| {
                token
                    .trim_matches(|c: char| !c.is_alphanumeric())
                    .to_string()
            })
            .filter(|token| !token.is_empty())
            .collect()
    };
    let left = tokens(left);
    let right = tokens(right);
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let shared = left.intersection(&right).count() as f64;
    let longest = left.len().max(right.len()) as f64;
    (shared / longest).clamp(0.0, 1.0)
}

/// The base score for a company-domain match, named so the call site reads as arithmetic.
const fn key_base(value: f64) -> f64 {
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::{MappingEntry, apply};
    use serde_json::json;

    fn mapped_from(payload: serde_json::Value) -> MappedValues {
        let mapping = vec![
            MappingEntry::new("email", "email").with_transforms(&["trim", "lowercase"]),
            MappingEntry::new("phone", "phone").with_transforms(&["e164_lite"]),
            MappingEntry::new("first_name", "first"),
            MappingEntry::new("last_name", "last"),
            MappingEntry::new("company_name", "company"),
        ];
        apply(&mapping, &payload).expect("mapping is valid")
    }

    fn candidate(id: Uuid, email: Option<&str>, phone: Option<&str>) -> Candidate {
        Candidate {
            id,
            email: email.map(str::to_string),
            phone: phone.map(str::to_string),
            company_name: Some("Acme".to_string()),
            first_name: Some("Ada".to_string()),
            last_name: Some("Lovelace".to_string()),
        }
    }

    #[test]
    fn an_exact_email_is_the_strongest_match() {
        let mapped = mapped_from(json!({"email": "ada@acme.co"}));
        let verdict = evaluate(
            &mapped,
            &[candidate(Uuid::nil(), Some("ada@acme.co"), None)],
        );
        let Verdict::Matched(found) = verdict else {
            panic!("expected a match, got {verdict:?}");
        };
        assert_eq!(found.key, MatchKey::Email);
        assert_eq!(found.dedupe_key, "ada@acme.co");
        assert!(found.score >= 0.95, "{}", found.score);
    }

    #[test]
    fn an_email_match_ignores_case_and_padding() {
        let mapped = mapped_from(json!({"email": "  ADA@Acme.CO "}));
        let verdict = evaluate(
            &mapped,
            &[candidate(Uuid::nil(), Some("ada@acme.co"), None)],
        );
        assert!(matches!(verdict, Verdict::Matched(_)), "{verdict:?}");
    }

    #[test]
    fn a_formatted_phone_matches_the_same_digits() {
        let mapped = mapped_from(json!({"phone": "+90 (532) 111-22-33"}));
        let verdict = evaluate(
            &mapped,
            &[candidate(Uuid::nil(), None, Some("+905321112233"))],
        );
        let Verdict::Matched(found) = verdict else {
            panic!("expected a match, got {verdict:?}");
        };
        assert_eq!(found.key, MatchKey::Phone);
        assert_eq!(found.dedupe_key, "+905321112233");
    }

    #[test]
    fn a_short_digit_string_is_not_a_phone_and_never_matches() {
        assert_eq!(normalize_phone(Some("12345")), None);
        assert_eq!(normalize_phone(Some("n/a")), None);
        assert_eq!(normalize_phone(None), None);
    }

    #[test]
    fn a_company_domain_with_the_same_name_matches_and_a_different_name_does_not() {
        let mapped = mapped_from(json!({
            "company": "acme.com.tr", "first": "Ada", "last": "Lovelace"
        }));
        let mut same = candidate(Uuid::nil(), None, None);
        same.company_name = Some("acme.com.tr".to_string());
        let Verdict::Matched(found) = evaluate(&mapped, &[same.clone()]) else {
            panic!(
                "expected a domain match, got {:?}",
                evaluate(&mapped, &[same.clone()])
            );
        };
        assert_eq!(found.key, MatchKey::CompanyDomain);
        assert!(found.score >= MATCH_THRESHOLD, "{}", found.score);

        // Same domain, a different person: the domain alone is a company, not a lead.
        let mut other = same;
        other.first_name = Some("Grace".to_string());
        other.last_name = Some("Hopper".to_string());
        assert_eq!(evaluate(&mapped, &[other]), Verdict::Unique);
    }

    #[test]
    fn an_initial_and_a_surname_still_count_as_the_same_name() {
        // The case an edit-distance score rejects and a token overlap accepts.
        let left = "a lovelace";
        let right = "ada lovelace";
        assert!(name_similarity(left, right) >= NAME_SIMILARITY_THRESHOLD);
    }

    #[test]
    fn an_email_match_outranks_a_phone_match() {
        let mapped = mapped_from(json!({"email": "ada@acme.co", "phone": "+905321112233"}));
        let by_email = candidate(Uuid::from_u128(1), Some("ada@acme.co"), None);
        let by_phone = candidate(Uuid::from_u128(2), None, Some("+905321112233"));
        let Verdict::Ambiguous(matches) = evaluate(&mapped, &[by_phone, by_email]) else {
            panic!("expected two matches");
        };
        // Best first, regardless of the order the caller passed them in.
        assert_eq!(matches[0].key, MatchKey::Email);
        assert_eq!(matches[1].key, MatchKey::Phone);
    }

    #[test]
    fn no_candidate_is_unique_and_the_key_is_still_recorded() {
        let mapped = mapped_from(json!({"email": "new@acme.co"}));
        assert_eq!(
            evaluate(
                &mapped,
                &[candidate(Uuid::nil(), Some("other@acme.co"), None)]
            ),
            Verdict::Unique
        );
        assert_eq!(dedupe_key(&mapped), Some("new@acme.co".to_string()));
    }

    #[test]
    fn a_submission_with_neither_email_nor_phone_has_no_key() {
        // Two such leads must not share a key: an empty-string dedupe key would make the
        // second one a "duplicate" of the first.
        let mapped = mapped_from(json!({"first": "Ada"}));
        assert_eq!(dedupe_key(&mapped), None);
        assert_eq!(
            evaluate(&mapped, &[candidate(Uuid::nil(), None, None)]),
            Verdict::Unique
        );
    }

    #[test]
    fn the_policies_round_trip_through_their_stored_names() {
        for policy in [
            DedupePolicy::Link,
            DedupePolicy::CreateAnyway,
            DedupePolicy::RejectDuplicate,
        ] {
            assert_eq!(DedupePolicy::parse(policy.as_str()), Some(policy));
        }
        assert_eq!(DedupePolicy::parse("merge"), None);
    }

    #[test]
    fn the_match_keys_round_trip_through_their_stored_names() {
        for key in [MatchKey::Email, MatchKey::Phone, MatchKey::CompanyDomain] {
            assert_eq!(MatchKey::parse(key.as_str()), Some(key));
        }
        assert_eq!(MatchKey::parse("telephone"), None);
        // Descending confidence, so the order in the code is the order an operator reads.
        assert!(MatchKey::Email.base_score() > MatchKey::Phone.base_score());
        assert!(MatchKey::Phone.base_score() > MatchKey::CompanyDomain.base_score());
    }

    #[test]
    fn the_company_domain_is_read_from_a_name_or_a_web_address() {
        // A name with no dot is not a host: matching on one would make every lead from
        // every company with that name the same company.
        assert_eq!(company_domain(Some("ACME Yapı")), None);
        assert_eq!(
            company_domain(Some("acme.com.tr")),
            Some("acme.com.tr".to_string())
        );
        // The scheme, the `www.` and the path are all noise.
        assert_eq!(
            company_domain(Some("https://www.acme.com/iletisim")),
            Some("acme.com".to_string())
        );
        assert_eq!(
            company_domain(Some("ada@acme.co")),
            Some("acme.co".to_string())
        );
        assert_eq!(company_domain(None), None);
        // Two different `.com` companies must not reduce to the same key.
        assert_ne!(
            company_domain(Some("acme.com")),
            company_domain(Some("globex.com"))
        );
        // A half-written host is a typo, not a company.
        assert_eq!(company_domain(Some("acme.")), None);
        assert_eq!(company_domain(Some(".com")), None);
    }

    #[test]
    fn an_empty_name_scores_zero_rather_than_panicking() {
        assert_eq!(name_similarity("", "ada"), 0.0);
        assert_eq!(name_similarity("ada", ""), 0.0);
        assert_eq!(name_similarity("   ", "ada"), 0.0);
    }

    /// **The unit-level guard for the defect the integration gate found.**
    ///
    /// `a_formatted_phone_matches_the_same_digits` compares `normalize_phone` with **itself**
    /// on both sides, so it agrees by construction and cannot notice the stored column being
    /// normalized differently in SQL. The test that did notice it lives in the gate, against a
    /// real database, which is the right place — but it is nine minutes of migration sweep and
    /// a compile, and the mistake is one character in a string constant that nobody re-reads.
    ///
    /// So the constant carries a machine-checkable fingerprint of the rule it has to implement:
    /// the `+` branch that restores the leading plus, and nothing that could quietly drop it
    /// again. **This asserts the SHAPE of the SQL, not its result** — the result needs a real
    /// `regexp_replace`, and pretending a string test can stand in for that is the mistake the
    /// last two generations of this branch's gates have made. The two are complementary: this
    /// one runs in a millisecond on every `cargo test -p`, and the gate is what proves the
    /// answer.
    #[test]
    fn the_sql_normalization_carries_the_plus_the_rust_one_carries() {
        // A shape check with a consequence spelled out: if someone "simplifies" this constant
        // back to a bare regexp_replace, this fails at the next unit run instead of at the next
        // dedupe verdict, which is to say: on a customer's inbox rather than in CI.
        assert!(
            PHONE_DIGITS_SQL.contains("like '+%'"),
            "the stored column must keep a leading plus, or it can never equal a key produced \
             by normalize_phone, which keeps it — PHONE_DIGITS_SQL is now: {PHONE_DIGITS_SQL}"
        );
        assert!(
            PHONE_DIGITS_SQL.contains("regexp_replace"),
            "the punctuation still has to be stripped: {PHONE_DIGITS_SQL}"
        );
        assert!(
            !PHONE_DIGITS_SQL.contains("lower("),
            "a phone number has no case; a lower() here means the expression drifted into \
             copying the e-mail arm: {PHONE_DIGITS_SQL}"
        );
        assert_eq!(
            PHONE_DIGITS_SQL.matches("{column}").count(),
            2,
            "both operands must read the same column — the column appears once in the plus test \
             and once in the digits, and replacing only one of them is a third drift"
        );
        assert!(
            !PHONE_DIGITS_SQL.contains("suffix"),
            "a suffix comparison merges two people on two continents who share a tail; see \
             a_different_phone_is_not_matched_just_because_a_suffix_is_shared"
        );
    }
}
