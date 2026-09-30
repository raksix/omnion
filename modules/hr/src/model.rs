//! Shared vocabulary of the HR module: employment types and statuses, the visibility level a
//! record is read at, and the field-hiding rule the screens and the export share.
//!
//! Everything here is a **pure rule**, so it can be proven without a database and used from both
//! the SQL builder and the row reader: a rule that only the query enforced would be invisible to
//! the export, and a rule only the export enforced would already have leaked a field once.
//!
//! The sensitive-field rule is the one the request's risk note leads with. Personal e-mail,
//! personal phone, home address and emergency contact are **dropped from the response**, not
//! rendered grey, for anyone without `hr.employees.sensitive.read` — and dropped from the list,
//! the detail and the CSV export by the same function, so the three can never disagree.

use serde::{Deserialize, Serialize};

/// The four employment types the schema accepts.
pub const EMPLOYMENT_TYPES: [&str; 4] = ["full_time", "part_time", "contract", "intern"];

/// The three lifecycle statuses the schema accepts.
pub const EMPLOYEE_STATUSES: [&str; 3] = ["active", "on_leave", "terminated"];

/// The kinds of document an employee record carries.
pub const DOCUMENT_KINDS: [&str; 4] = ["contract", "id", "certificate", "other"];

/// The four employment types a select offers, in the order the form lists them.
pub const EMPLOYMENT_TYPE_LABELS: [(&str, &str); 4] = [
    ("full_time", "Full time"),
    ("part_time", "Part time"),
    ("contract", "Contract"),
    ("intern", "Intern"),
];

/// Longest a first or last name may be.
pub const MAX_NAME_LENGTH: usize = 80;

/// Longest a position title may be.
pub const MAX_POSITION_LENGTH: usize = 120;

/// Longest a note may be.
pub const MAX_NOTES_LENGTH: usize = 2000;

/// Longest an employee number may be.
pub const MAX_EMPLOYEE_NO_LENGTH: usize = 32;

/// How much of the organization's HR a caller sees.
///
/// This is a **data** decision, not a UI one: an employee that is not in the caller's scope is
/// answered `404`, exactly like a record that does not exist, because a `403` would tell a
/// stranger that the record is real.
///
/// The declaration order *is* the narrowness order — `Own` < `Team` < `All` — so a caller
/// narrowed by two bindings is narrowed by the tighter one, and `Ord` is what expresses that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// Only the caller's own record.
    Own,
    /// The caller's own record plus their direct reports.
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

/// The permission that opens the flagged personal fields to a role.
pub const SENSITIVE_FIELDS_READ: &str = "hr.employees.sensitive.read";

/// The fields hidden from a role without [`SENSITIVE_FIELDS_READ`].
///
/// Deliberately the four the request names and nothing else: salary and bank details are not in
/// the schema at all, so they cannot leak by being forgotten in this list.
pub const SENSITIVE_FIELDS: [&str; 4] = [
    "personal_email",
    "personal_phone",
    "address",
    "emergency_contact",
];

/// `true` when the field is one of the flagged ones.
#[must_use]
pub fn is_sensitive(field: &str) -> bool {
    SENSITIVE_FIELDS.contains(&field)
}

/// `true` when the employment type is one the schema accepts.
#[must_use]
pub fn is_employment_type(value: &str) -> bool {
    EMPLOYMENT_TYPES.contains(&value)
}

/// `true` when the status is one the schema accepts.
#[must_use]
pub fn is_employee_status(value: &str) -> bool {
    EMPLOYEE_STATUSES.contains(&value)
}

/// `true` when the address is shaped like an e-mail address.
///
/// Deliberately the same shape the CRM module uses and the schema checks, so a value the module
/// accepts is a value the database accepts and the "valid here, refused there" bug cannot exist.
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

/// The initials the employee list draws as an avatar.
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
    let trimmed = format!("{} {}", first.trim(), last.trim()).trim().to_owned();
    if trimmed.is_empty() {
        "Unnamed employee".to_owned()
    } else {
        trimmed
    }
}

/// The next free employee number, the shape the form suggests: `EMP-0001`.
///
/// Counts what exists rather than tracking a sequence, because a number is unique per
/// organization and an organization may have had records deleted. Gaps are filled, which is what
/// a person reading a list of numbers expects.
#[must_use]
pub fn suggest_employee_no(taken: i64) -> String {
    format!("EMP-{:04}", taken + 1)
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
    fn the_levels_are_ordered_from_the_narrowest_to_the_widest() {
        // The API resolves a doubly-narrowed caller with this order, so it is a contract and not
        // an accident of the declaration.
        assert!(Visibility::Own < Visibility::Team);
        assert!(Visibility::Team < Visibility::All);
    }

    #[test]
    fn the_flagged_fields_are_exactly_the_ones_the_request_names() {
        // A test that pins the list rather than its length: adding a field to the schema without
        // adding it here would make it readable by everyone, and the count alone would not say so.
        assert_eq!(
            SENSITIVE_FIELDS,
            ["personal_email", "personal_phone", "address", "emergency_contact"]
        );
        for field in SENSITIVE_FIELDS {
            assert!(is_sensitive(field), "{field} must be flagged");
        }
        for field in ["work_email", "position", "notes", "phone"] {
            assert!(!is_sensitive(field), "{field} is not a flagged field");
        }
    }

    #[test]
    fn the_vocabulary_matches_the_schema_checks() {
        for value in EMPLOYMENT_TYPES {
            assert!(is_employment_type(value), "{value}");
        }
        assert!(!is_employment_type("freelance"));
        for value in EMPLOYEE_STATUSES {
            assert!(is_employee_status(value), "{value}");
        }
        assert!(!is_employee_status("left"));
    }

    #[test]
    fn the_email_and_phone_shapes_match_the_schema_checks() {
        for good in ["ada@example.com", "a.b+c@sub.example.co.uk"] {
            assert!(is_email(good), "{good} should be accepted");
        }
        for bad in ["", "ada", "ada@", "@example.com", "ada@example", "a b@example.com"] {
            assert!(!is_email(bad), "{bad} should be refused");
        }
        for good in ["+90 532 000 00 00", "05320000000", "(0532) 000-00-00"] {
            assert!(is_phone(good), "{good} should be accepted");
        }
        for bad in ["", "+", "12345", "call me"] {
            assert!(!is_phone(bad), "{bad} should be refused");
        }
    }

    #[test]
    fn the_suggested_number_follows_the_count_not_a_counter() {
        assert_eq!(suggest_employee_no(0), "EMP-0001");
        assert_eq!(suggest_employee_no(41), "EMP-0042");
    }

    #[test]
    fn initials_and_display_name_never_render_empty() {
        assert_eq!(initials("ada", "lovelace"), "AL");
        assert_eq!(initials("ada", ""), "A");
        assert_eq!(initials("", "lovelace"), "L");
        assert_eq!(initials("", ""), "?");

        assert_eq!(display_name("Ada", "Lovelace"), "Ada Lovelace");
        assert_eq!(display_name("", ""), "Unnamed employee");
    }

    #[test]
    fn clean_maps_blank_to_absent() {
        assert_eq!(clean(Some("  ".to_owned())), None);
        assert_eq!(clean(Some(" engineer ".to_owned())), Some("engineer".to_owned()));
        assert_eq!(clean(None), None);
    }

    #[test]
    fn the_json_shape_a_screen_may_receive_carries_no_salary_key() {
        // The risk note asks for payroll to be absent from the schema. A payload that never had
        // the key is the cheapest place to notice somebody adding it back.
        let row = json!({
            "employee_no": "EMP-0001",
            "position": "Engineer",
            "work_email": "ada@example.com",
            "personal_email": "private@example.com",
        });
        assert!(row.get("salary").is_none());
        assert!(row.get("base_salary").is_none());
        assert!(row.get("bank_account").is_none());
    }
}
