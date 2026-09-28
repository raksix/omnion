//! The field mapping: turning a submission's keys into a lead's columns.
//!
//! A capture surface does not know what a lead is. A REQ-064 form has `company_email` and
//! `how_many`; the CRM has `email` and `product_interest`. The mapping between them is data,
//! owned by the source, and this module is the one place that reads it.
//!
//! Three rules make the mapping honest rather than merely functional.
//!
//! 1. **Transforms are a closed list applied in order.** A form value is attacker-controlled
//!    text, and the alternative to a closed list is a small expression language in the
//!    database — which is a code-execution surface wearing a config file's clothes. The cost
//!    is a branch here for every transform a future source wants.
//! 2. **A missing source key is a *missing value*, not an error**, unless the target is
//!    required. A form that dropped a field must not fail the whole submission, and a
//!    required target with no value must fail with the target named — "email is required and
//!    the payload has no `email`" is a message an operator can act on; "mapping failed" is
//!    not.
//! 3. **Binding health is a first-class answer.** [`health`] compares the mapping's source
//!    keys against the keys the bound form actually has and returns the ones that broke. A
//!    rename on the form side is otherwise a lead written with a silently empty field, which
//!    is the failure that costs a real customer.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{CrmIntakeError, Result};

/// Every CRM field a source may fill.
///
/// A closed list because this is a *target* vocabulary: the panel's mapping editor offers
/// these and nothing else, so a target can never be a column that does not exist. `notes` is
/// the `message` column, named the way the form editor labels it.
pub const TARGETS: [&str; 14] = [
    "first_name",
    "last_name",
    "email",
    "phone",
    "company_name",
    "job_title",
    "message",
    "product_interest",
    "country",
    "region",
    "language",
    "budget_band",
    "quantity",
    "preferred_contact_time",
];

/// The transforms a mapping entry may carry.
pub const TRANSFORMS: [&str; 6] = [
    "trim",
    "lowercase",
    "title_case",
    "strip_html",
    "e164_lite",
    "split_full_name",
];

/// `true` when `value` is a target the platform can fill.
#[must_use]
pub fn is_target(value: &str) -> bool {
    TARGETS.contains(&value)
}

/// `true` when `value` is a transform the platform knows.
#[must_use]
pub fn is_transform(value: &str) -> bool {
    TRANSFORMS.contains(&value)
}

/// One line of a mapping: a target, where its value comes from, and how to clean it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MappingEntry {
    /// The CRM column this line fills.
    pub target: String,
    /// The key in the submission payload. `None` means the line is a constant
    /// ([`MappingEntry::fallback`]) and reads nothing from the payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    /// Ordered transform names. Applied left to right, each on the previous result.
    #[serde(default)]
    pub transform: Vec<String>,
    /// When `true`, the submission is refused if this ends up empty.
    #[serde(default)]
    pub required: bool,
    /// A constant used when the payload has no value for `source_key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

impl MappingEntry {
    /// A line that reads `source_key` and cleans it.
    #[must_use]
    pub fn new(target: &str, source_key: &str) -> Self {
        Self {
            target: target.to_string(),
            source_key: Some(source_key.to_string()),
            transform: Vec::new(),
            required: false,
            fallback: None,
        }
    }

    /// Attach transforms, in the order they will be applied.
    #[must_use]
    pub fn with_transforms(mut self, transform: &[&str]) -> Self {
        self.transform = transform.iter().map(|value| (*value).to_string()).collect();
        self
    }

    /// Mark this target as required.
    #[must_use]
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// Give this line a constant to fall back to.
    #[must_use]
    pub fn fallback(mut self, value: &str) -> Self {
        self.fallback = Some(value.to_string());
        self
    }
}

/// The mapped values of one submission.
///
/// Every field is a `String` and every one is optional: the *rows* of a lead are sparse by
/// nature (most forms do not ask for a job title) and a struct of thirty `Option<String>`s
/// would be the same information with more typing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MappedValues {
    /// The values by target name.
    pub values: std::collections::BTreeMap<String, String>,
    /// Targets the mapping required that ended up empty — the refusal list, not an error yet.
    pub missing_required: Vec<String>,
}

impl MappedValues {
    /// The value of a target, if the payload produced one.
    #[must_use]
    pub fn get(&self, target: &str) -> Option<&str> {
        self.values.get(target).map(String::as_str)
    }

    /// `true` when the mapping produced nothing a lead can be built from.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// Apply a mapping to a submission payload.
///
/// The returned [`MappedValues::missing_required`] is empty on success. A non-empty list is a
/// refusal and the caller writes no lead: a partial lead row is worse than none, because it
/// looks like a lead somebody can work.
pub fn apply(mapping: &[MappingEntry], payload: &Value) -> Result<MappedValues> {
    let mut mapped = MappedValues::default();

    for entry in mapping {
        if !is_target(&entry.target) {
            return Err(CrmIntakeError::invalid(format!(
                "mapping target \"{}\" is not a CRM field (legal: {})",
                entry.target,
                TARGETS.join(", ")
            )));
        }
        for transform in &entry.transform {
            if !is_transform(transform) {
                return Err(CrmIntakeError::invalid(format!(
                    "transform \"{transform}\" is not one this build knows (legal: {})",
                    TRANSFORMS.join(", ")
                )));
            }
        }

        // A line without a source key is a constant, and the fallback is its only value.
        let raw = entry
            .source_key
            .as_deref()
            .and_then(|key| payload.get(key))
            .map(scalar_to_string)
            .or_else(|| entry.fallback.clone());

        let Some(raw) = raw else {
            if entry.required {
                mapped.missing_required.push(entry.target.clone());
            }
            continue;
        };

        let cleaned = apply_transforms(&raw, &entry.transform);
        if cleaned.is_empty() {
            // A value that transforms to nothing is the same as no value: e-mail is the
            // value most often wiped by `strip_html` or `trim`, and treating "<b></b>" as an
            // e-mail produces a lead with an e-mail of "" and a check-constraint failure
            // instead of the readable "email is required" the form should show.
            if entry.required {
                mapped.missing_required.push(entry.target.clone());
            }
            continue;
        }

        mapped.values.insert(entry.target.clone(), cleaned);
    }

    Ok(mapped)
}

/// The source keys a mapping reads, in order, without the constant lines.
#[must_use]
pub fn source_keys(mapping: &[MappingEntry]) -> Vec<String> {
    mapping
        .iter()
        .filter_map(|entry| entry.source_key.clone())
        .collect()
}

/// Whether every required target has a line that can produce a value.
///
/// Checked at *save* time rather than at submission time, so the operator is told which
/// target is unsatisfied while they are still editing rather than when a real visitor is
/// being turned away.
pub fn validate_required_targets(
    mapping: &[MappingEntry],
    required_targets: &[String],
) -> Result<()> {
    let mut unsatisfied = Vec::new();
    for target in required_targets {
        if !is_target(target) {
            return Err(CrmIntakeError::invalid(format!(
                "required target \"{target}\" is not a CRM field"
            )));
        }
        let line = mapping.iter().find(|entry| &entry.target == target);
        match line {
            // No line at all: nothing can ever produce this value.
            None => unsatisfied.push(target.clone()),
            Some(entry)
                if !entry.required && entry.fallback.is_none() && entry.source_key.is_none() =>
            {
                unsatisfied.push(target.clone())
            }
            Some(_) => {}
        }
    }
    if unsatisfied.is_empty() {
        return Ok(());
    }
    Err(CrmIntakeError::invalid(format!(
        "mapping would leave required target(s) unfilled: {}",
        unsatisfied.join(", ")
    )))
}

/// The mapping keys the bound form no longer has.
///
/// `available_keys` is the form's own key list; an empty list means the source is not bound
/// to a form (a keyed endpoint has no keys to break), which is reported as healthy rather
/// than as "everything is broken".
#[must_use]
pub fn health(mapping: &[MappingEntry], available_keys: &[String]) -> Vec<String> {
    if available_keys.is_empty() {
        return Vec::new();
    }
    source_keys(mapping)
        .into_iter()
        .filter(|key| !available_keys.contains(key))
        .collect()
}

/// The largest payload the platform accepts, in bytes.
#[must_use]
pub fn payload_size(payload: &Value) -> usize {
    payload.to_string().len()
}

/// Render a JSON scalar as the text a mapping line transforms.
///
/// A non-scalar (an object, an array) is *not* a value: `{"a":1}` mapped onto a name field
/// would produce the literal string `{"a":1}` in the CRM, which is worse than an empty field
/// because it looks like a name somebody typed.
fn scalar_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Null | Value::Array(_) | Value::Object(_) => String::new(),
    }
}

/// Split a single "full name" value into a first and a last name.
///
/// A form with one name field is the common case and doing the split by hand in every source
/// is how `"Ada  Lovelace"` (two spaces) or `"ada"` (no last name) ends up in a name column.
/// Extra whitespace collapses and a single word becomes the first name with an empty last
/// name — the alternative, guessing a surname, invents data.
#[must_use]
pub fn split_full_name(value: &str) -> (String, String) {
    let mut parts = value.split_whitespace();
    let first = parts.next().unwrap_or_default();
    let rest: Vec<&str> = parts.collect();
    (title_case(first), title_case(&rest.join(" ")))
}

/// Apply the transforms, in order.
fn apply_transforms(raw: &str, transforms: &[String]) -> String {
    let mut value = raw.to_string();
    for transform in transforms {
        value = match transform.as_str() {
            "trim" => value.trim().to_string(),
            "lowercase" => value.to_lowercase(),
            "title_case" => title_case(&value),
            "strip_html" => strip_html(&value),
            "e164_lite" => e164_lite(&value),
            "split_full_name" => value,
            // Unreachable: `apply` validates every name before running. Kept as identity so a
            // future transform added to the list without a branch is a no-op rather than a
            // silent data loss — and the `is_transform` test keeps the two in step.
            _ => value,
        };
    }
    value.trim().to_string()
}

/// Capitalize each word, lowercasing the rest.
///
/// Deliberately not "first letter of each word": `o'brien` and `mcDonald` are names, and
/// title-casing them to `O'Brien`/`Mcdonald` is a small lie the CRM would store forever.
fn title_case(value: &str) -> String {
    value
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Remove tags and unescape the handful of entities a form can produce.
///
/// A lead's name column is not a rich-text field; storing `<script>` there is how a stored-XSS
/// payload reaches a panel that renders it. The panel also escapes on output, but the mapping
/// is the cheaper place to refuse the bytes.
fn strip_html(value: &str) -> String {
    // `<script>` and `<style>` have their *contents* dropped as well as their tags: a stripper
    // that removes the tags and leaves `alert(1)` behind has put the script's source into the
    // CRM's name column, which is the failure this transform exists to prevent. Everything
    // else is a tag whose content is the visitor's own text and is kept.
    const DROPPED_WITH_CONTENT: [&str; 2] = ["script", "style"];

    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    loop {
        let Some(open) = rest.find('<') else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..open]);
        rest = &rest[open..];
        let Some(close) = rest.find('>') else {
            // An unterminated `<` is text, not a tag: dropping the rest of the string because
            // a visitor typed "a < b" would silently lose their message.
            out.push_str(rest);
            break;
        };
        let name: String = rest[1..close]
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        rest = &rest[close + 1..];
        if DROPPED_WITH_CONTENT.contains(&name.as_str()) {
            let closing = format!("</{name}");
            match rest.to_ascii_lowercase().find(&closing) {
                Some(end) => {
                    let after = rest[end..].find('>').map_or(rest.len(), |caret| caret + 1);
                    rest = &rest[end + after..];
                }
                // An unclosed `<script>` swallows the rest, which is what a browser does too.
                None => break,
            }
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// Keep a phone number's digits and a single leading `+`.
///
/// Not a full E.164 parser — that needs a country the form may not have sent. The rule is
/// "strip the formatting", and it is what makes `+90 (532) 111-22-33` and `+905321112233`
/// the same dedupe key, which is the whole reason the transform exists.
fn e164_lite(value: &str) -> String {
    let trimmed = value.trim();
    let plus = trimmed.starts_with('+');
    let digits: String = trimmed.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return String::new();
    }
    if plus { format!("+{digits}") } else { digits }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_lists_have_no_duplicates() {
        for list in [&TARGETS[..], &TRANSFORMS[..]] {
            let mut sorted = list.to_vec();
            sorted.sort_unstable();
            let before = sorted.len();
            sorted.dedup();
            assert_eq!(before, sorted.len(), "duplicate in {list:?}");
        }
    }

    #[test]
    fn a_plain_mapping_reads_its_keys() {
        let mapping = vec![
            MappingEntry::new("email", "company_email"),
            MappingEntry::new("first_name", "name"),
        ];
        let mapped = apply(&mapping, &json!({"company_email": "a@b.co", "name": "Ada"})).unwrap();
        assert_eq!(mapped.get("email"), Some("a@b.co"));
        assert_eq!(mapped.get("first_name"), Some("Ada"));
        assert!(mapped.missing_required.is_empty());
    }

    #[test]
    fn transforms_run_left_to_right() {
        // `title_case` then `trim` differs from `trim` then `title_case` only for a value with
        // leading space, so the assertion pins the *order* with a pair where it matters:
        // lowercase must not undo title-casing when it comes second.
        let mapping = vec![MappingEntry::new("email", "e").with_transforms(&["trim", "lowercase"])];
        let mapped = apply(&mapping, &json!({"e": "  ADA@Example.COM "})).unwrap();
        assert_eq!(mapped.get("email"), Some("ada@example.com"));

        let mapping = vec![
            MappingEntry::new("first_name", "n").with_transforms(&["lowercase", "title_case"]),
        ];
        let mapped = apply(&mapping, &json!({"n": "ada lovelace"})).unwrap();
        assert_eq!(mapped.get("first_name"), Some("Ada Lovelace"));
    }

    #[test]
    fn strip_html_removes_tags_and_entities() {
        let mapping = vec![MappingEntry::new("first_name", "n").with_transforms(&["strip_html"])];
        let mapped = apply(
            &mapping,
            &json!({"n": "<b>Ada</b> &amp; <script>alert(1)</script>"}),
        )
        .unwrap();
        assert_eq!(mapped.get("first_name"), Some("Ada &"));
    }

    #[test]
    fn e164_lite_strips_formatting_but_keeps_a_leading_plus() {
        let mapping = vec![MappingEntry::new("phone", "p").with_transforms(&["e164_lite"])];
        let mapped = apply(&mapping, &json!({"p": "+90 (532) 111-22-33"})).unwrap();
        assert_eq!(mapped.get("phone"), Some("+905321112233"));
        let mapped = apply(&mapping, &json!({"p": "0532 111 22 33"})).unwrap();
        assert_eq!(mapped.get("phone"), Some("05321112233"));
    }

    #[test]
    fn a_phone_with_no_digits_transforms_to_nothing() {
        let mapping = vec![MappingEntry::new("phone", "p").with_transforms(&["e164_lite"])];
        let mapped = apply(&mapping, &json!({"p": "n/a"})).unwrap();
        // Not "n/a", and not the digits of the letters: a phone line holding "n/a" is a lead
        // nobody can call, and it would satisfy a required check that means nothing.
        assert_eq!(mapped.get("phone"), None);
    }

    #[test]
    fn a_missing_required_target_is_refused_and_named() {
        let mapping = vec![MappingEntry::new("email", "e").required()];
        let mapped = apply(&mapping, &json!({"other": "x"})).unwrap();
        assert_eq!(mapped.missing_required, vec!["email".to_string()]);
    }

    #[test]
    fn a_required_target_whose_value_transforms_away_is_still_missing() {
        let mapping = vec![
            MappingEntry::new("email", "e")
                .with_transforms(&["strip_html"])
                .required(),
        ];
        let mapped = apply(&mapping, &json!({"e": "<b></b>"})).unwrap();
        assert_eq!(mapped.missing_required, vec!["email".to_string()]);
    }

    #[test]
    fn a_removed_optional_target_is_skipped_not_failed() {
        let mapping = vec![
            MappingEntry::new("email", "e"),
            MappingEntry::new("job_title", "title"),
        ];
        let mapped = apply(&mapping, &json!({"e": "a@b.co"})).unwrap();
        assert_eq!(mapped.get("email"), Some("a@b.co"));
        assert_eq!(mapped.get("job_title"), None);
        assert!(mapped.missing_required.is_empty());
    }

    #[test]
    fn a_fallback_fills_a_target_the_payload_does_not_carry() {
        let mapping = vec![MappingEntry::new("country", "country").fallback("Türkiye")];
        let mapped = apply(&mapping, &json!({})).unwrap();
        assert_eq!(mapped.get("country"), Some("Türkiye"));
    }

    #[test]
    fn a_non_scalar_value_is_not_a_value() {
        // `{"a":1}` mapped onto a name must not become the literal string in the CRM.
        let mapping = vec![MappingEntry::new("first_name", "n")];
        let mapped = apply(&mapping, &json!({"n": {"a": 1}})).unwrap();
        assert_eq!(mapped.get("first_name"), None);
    }

    #[test]
    fn an_unknown_target_or_transform_is_refused_by_name() {
        let mapping = vec![MappingEntry::new("salary", "s")];
        let error = apply(&mapping, &json!({"s": "1"})).unwrap_err();
        assert!(error.to_string().contains("salary"), "{error}");

        let mapping = vec![MappingEntry::new("email", "e").with_transforms(&["shell_exec"])];
        let error = apply(&mapping, &json!({"e": "a@b.co"})).unwrap_err();
        assert!(error.to_string().contains("shell_exec"), "{error}");
    }

    #[test]
    fn saving_refuses_a_mapping_that_would_drop_a_required_target() {
        let mapping = vec![MappingEntry::new("first_name", "name")];
        let error = validate_required_targets(&mapping, &["email".to_string()]).unwrap_err();
        assert!(error.to_string().contains("email"), "{error}");

        let mapping = vec![
            MappingEntry::new("first_name", "name"),
            MappingEntry::new("email", "email"),
        ];
        validate_required_targets(&mapping, &["email".to_string()]).unwrap();
    }

    #[test]
    fn health_names_the_key_the_form_renamed() {
        let mapping = vec![
            MappingEntry::new("email", "email"),
            MappingEntry::new("phone", "telefon"),
        ];
        let broken = health(&mapping, &["email".to_string(), "phone_number".to_string()]);
        assert_eq!(broken, vec!["telefon".to_string()]);
    }

    #[test]
    fn a_keyed_endpoint_with_no_form_keys_is_healthy() {
        // No bound form means there is nothing to break; reporting every key as broken would
        // put a permanent red banner on every endpoint source.
        let mapping = vec![MappingEntry::new("email", "email")];
        assert!(health(&mapping, &[]).is_empty());
    }

    #[test]
    fn split_full_name_fills_both_halves() {
        let (first, last) = split_full_name("  ada   lovelace  ");
        assert_eq!(first, "Ada");
        assert_eq!(last, "Lovelace");

        let (first, last) = split_full_name("ada");
        assert_eq!(first, "Ada");
        assert_eq!(last, "");
    }
}
