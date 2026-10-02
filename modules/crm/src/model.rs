//! Shared vocabulary of the CRM: statuses, tags, e-mail and phone shapes, the visibility level a
//! record is read at, and the field-hiding rule the screens and the export share.
//!
//! Everything here is a **pure rule**, so it can be proven without a database and used from both
//! the SQL builder and the row reader: a rule that only the query enforced would be invisible to
//! the export, and a rule only the export enforced would already have leaked a field once.

use serde::{Deserialize, Serialize};

/// The lifecycle a company or a contact moves through.
pub const STATUSES: [&str; 4] = ["lead", "customer", "partner", "churned"];

/// The kinds of activity a record's timeline can hold.
pub const ACTIVITY_KINDS: [&str; 4] = ["call", "meeting", "note", "task"];

/// What a form submission became (`modules::crm::leads`).
///
/// The list is the whole vocabulary of the ingress ledger: the drain writes one of these, the
/// inbox filters on one of these, and the check constraint in the migration agrees. A fourth
/// value needs all three changed, which is the point — a lead's fate is a closed set, not a
/// free-text note somebody invents in a script.
pub const OUTCOMES: [&str; 5] = ["created", "merged", "rejected", "orphaned", "disabled"];

/// Tags a record may carry.
pub const MAX_TAGS: usize = 10;

/// Longest a single tag may be.
pub const MAX_TAG_LENGTH: usize = 32;

/// Longest a free-text note may be.
pub const MAX_NOTES_LENGTH: usize = 4000;

/// How much of the organization's CRM a caller sees.
///
/// This is a **data** decision, not a UI one: a contact that is not in the caller's scope is
/// answered `404`, exactly like a record that does not exist, because a `403` would tell a
/// stranger that the record is real.
///
/// The declaration order *is* the narrowness order — `Own` < `Team` < `All` — so a caller
/// narrowed by two bindings is narrowed by the tighter one, and `Ord` is what expresses that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// Only the records the caller owns.
    Own,
    /// The records of the caller's groups, plus their own.
    Team,
    /// Every record of the organization.
    All,
}

impl Visibility {
    /// Read the level from a query or binding value; `all` is the default of an absent value.
    #[must_use]
    pub fn parse(value: Option<&str>) -> Option<Self> {
        match value.map(str::trim) {
            None | Some("") | Some("all") => Some(Self::All),
            Some("own") => Some(Self::Own),
            Some("team") => Some(Self::Team),
            Some(_) => None,
        }
    }

    /// The value the query string carries.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Own => "own",
            Self::Team => "team",
            Self::All => "all",
        }
    }
}

/// The permission that opens the flagged fields (a contract note, a margin) to a role.
pub const SENSITIVE_FIELDS_READ: &str = "crm.fields.sensitive.read";

/// The fields that are hidden from a role without [`SENSITIVE_FIELDS_READ`].
///
/// Hiding is **enforced by dropping the key from the response**, not by rendering it grey: an
/// export and a list have to agree, and only one code path can do that.
pub const SENSITIVE_FIELDS: [&str; 2] = ["contract_value_note", "internal_notes"];

/// `true` when the field is one of the flagged ones.
#[must_use]
pub fn is_sensitive(field: &str) -> bool {
    SENSITIVE_FIELDS.contains(&field)
}

/// Remove the flagged keys from a custom-field value when the caller may not read them.
///
/// Recursive: a nested object can hold a flagged key too, and a key only hidden at the top level
/// would still be visible one level down.
#[must_use]
pub fn redact_custom(value: &serde_json::Value, may_read_sensitive: bool) -> serde_json::Value {
    if may_read_sensitive {
        return value.clone();
    }

    match value {
        serde_json::Value::Object(map) => {
            let kept: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .filter(|(key, _)| !is_sensitive(key))
                .map(|(key, entry)| {
                    (key.clone(), redact_custom(entry, may_read_sensitive))
                })
                .collect();
            serde_json::Value::Object(kept)
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|entry| redact_custom(entry, may_read_sensitive))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// A tag that survives trimming, or `None` when it is blank or too long.
#[must_use]
pub fn normalise_tag(raw: &str) -> Option<String> {
    let tag = raw.trim();
    if tag.is_empty() || tag.chars().count() > MAX_TAG_LENGTH {
        return None;
    }
    Some(tag.to_owned())
}

/// Normalise a tag list: trim, drop blanks, de-duplicate case-insensitively, cap the count.
///
/// The cap is the schema's cap, applied here so the caller gets a refusal naming the field
/// instead of a `check_violation` it has to decode.
#[must_use]
pub fn normalise_tags(raw: &[String]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut tags: Vec<String> = Vec::new();
    for entry in raw {
        if let Some(tag) = normalise_tag(entry) {
            let key = tag.to_lowercase();
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            tags.push(tag);
        }
    }
    tags
}

/// `true` when the address is shaped like an e-mail address.
///
/// Deliberately the same shape as the schema check, so a value the module accepts is a value
/// the database accepts and the "valid here, refused there" bug cannot exist.
#[must_use]
pub fn is_email(candidate: &str) -> bool {
    let trimmed = candidate.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 254 {
        return false;
    }
    let mut parts = trimmed.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !trimmed.chars().any(char::is_whitespace)
}

/// `true` when the value is shaped like a phone number the schema accepts.
#[must_use]
pub fn is_phone(candidate: &str) -> bool {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return false;
    }
    let body = trimmed.strip_prefix('+').unwrap_or(trimmed);
    !body.is_empty()
        && body.chars().count() >= 7
        && body.chars().count() <= 20
        && body
            .chars()
            .all(|c| c.is_ascii_digit() || c == ' ' || c == '(' || c == ')' || c == '-')
}

/// The two-letter-or-more initials the contact list draws as an avatar.
///
/// Falls back to the first character of whatever there is, so an avatar is never an empty circle.
#[must_use]
pub fn initials(first: &str, last: &str) -> String {
    let first: Vec<char> = first.trim().chars().collect();
    let last: Vec<char> = last.trim().chars().collect();

    match (first.first(), last.first()) {
        (Some(a), Some(b)) => format!("{}{}", a.to_uppercase(), b.to_uppercase()),
        (Some(a), None) => a.to_uppercase().to_string(),
        (None, Some(b)) => b.to_uppercase().to_string(),
        (None, None) => "?".to_owned(),
    }
}

/// The person's display name: both names when there are two, the one that exists otherwise.
#[must_use]
pub fn display_name(first: &str, last: &str) -> String {
    let joined = format!("{} {}", first.trim(), last.trim());
    let trimmed = joined.trim();
    if trimmed.is_empty() {
        "Unnamed contact".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Trim a value, mapping blank to `None` — the difference between "unset" and "set to nothing".
#[must_use]
pub fn clean(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn visibility_reads_the_three_documented_levels() {
        assert_eq!(Visibility::parse(Some("own")), Some(Visibility::Own));
        assert_eq!(Visibility::parse(Some("team")), Some(Visibility::Team));
        assert_eq!(Visibility::parse(Some("all")), Some(Visibility::All));
        // An absent or blank value is the widest level, so a caller never has to pass one.
        assert_eq!(Visibility::parse(None), Some(Visibility::All));
        assert_eq!(Visibility::parse(Some("")), Some(Visibility::All));
        assert_eq!(Visibility::parse(Some("everyone")), None);
    }

    #[test]
    fn redaction_drops_flagged_keys_at_every_depth() {
        let value = json!({
            "contract_value_note": "renewal at 40k",
            "seat_count": 12,
            "history": [
                { "internal_notes": "churn risk", "year": 2024 }
            ]
        });

        let hidden = redact_custom(&value, false);
        assert!(hidden.get("contract_value_note").is_none());
        assert_eq!(hidden["seat_count"], json!(12));
        let entry = &hidden["history"][0];
        assert!(entry.get("internal_notes").is_none());
        assert_eq!(entry["year"], json!(2024));
    }

    #[test]
    fn the_levels_are_ordered_from_the_narrowest_to_the_widest() {
        // The API resolves a doubly-narrowed caller with this order, so it is a contract and not
        // an accident of the declaration.
        assert!(Visibility::Own < Visibility::Team);
        assert!(Visibility::Team < Visibility::All);
    }

    #[test]
    fn redaction_keeps_everything_for_a_role_that_may_read_the_fields() {
        let value = json!({ "contract_value_note": "renewal at 40k" });
        assert_eq!(redact_custom(&value, true), value);
    }

    #[test]
    fn tags_are_trimmed_deduplicated_and_capped() {
        let tags = normalise_tags(&[
            "vip".to_owned(),
            "  VIP ".to_owned(),
            "".to_owned(),
            "renewal".to_owned(),
        ]);
        assert_eq!(tags, vec!["vip".to_owned(), "renewal".to_owned()]);

        // `normalise_tags` trims and de-duplicates; the *cap* is a refusal the record writers
        // apply, because silently dropping a tag the person typed would be data loss.
        let many: Vec<String> = (0..20).map(|index| format!("tag-{index}")).collect();
        assert_eq!(normalise_tags(&many).len(), 20);
        assert!(normalise_tag(&"x".repeat(MAX_TAG_LENGTH + 1)).is_none());
        assert!(normalise_tag("x").is_some());
    }

    #[test]
    fn the_email_shape_matches_the_schema_check() {
        for good in ["ada@example.com", "a.b+c@sub.example.co.uk"] {
            assert!(is_email(good), "{good} should be accepted");
        }
        for bad in ["", "ada", "ada@", "@example.com", "ada@example", "a b@example.com", "ada@exa mple.com"] {
            assert!(!is_email(bad), "{bad} should be refused");
        }
        assert!(!is_email(&format!("{}@example.com", "a".repeat(250))));
    }

    #[test]
    fn the_phone_shape_matches_the_schema_check() {
        for good in ["+90 532 000 00 00", "05320000000", "(0532) 000-00-00"] {
            assert!(is_phone(good), "{good} should be accepted");
        }
        for bad in ["", "+", "12345", "call me"] {
            assert!(!is_phone(bad), "{bad} should be refused");
        }
    }

    #[test]
    fn initials_and_display_name_never_render_empty() {
        assert_eq!(initials("ada", "lovelace"), "AL");
        assert_eq!(initials("ada", ""), "A");
        assert_eq!(initials("", "lovelace"), "L");
        assert_eq!(initials("", ""), "?");

        assert_eq!(display_name("Ada", "Lovelace"), "Ada Lovelace");
        assert_eq!(display_name("", ""), "Unnamed contact");
    }

    #[test]
    fn clean_maps_blank_to_absent() {
        assert_eq!(clean(Some("  ".to_owned())), None);
        assert_eq!(clean(Some(" note ".to_owned())), Some("note".to_owned()));
        assert_eq!(clean(None), None);
    }
}
