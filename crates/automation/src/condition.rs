//! Conditions: what an event payload must satisfy before a rule's actions run.
//!
//! The comparison set is closed and small on purpose (docs/09-N8N-TEARDOWN.md §13 lesson 14:
//! no dynamic code in the core process, ever). A condition names a field of the event payload, an
//! operator, and — except for the two existence operators — the value to compare with:
//!
//! ```json
//! { "field": "status", "operator": "equals", "value": "published" }
//! { "field": "title",  "operator": "contains", "value": "Release" }
//! { "field": "revision_no", "operator": "exists" }
//! ```
//!
//! Every condition of a rule must hold for the rule to fire. A field that is not in the payload
//! makes a condition false (except `not_exists`, which is *about* absence) — a rule that asks a
//! question the event does not answer does not fire, and says so in its skip audit.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{AutomationError, Result};

/// Most conditions one rule may carry.
pub const MAX_CONDITIONS: usize = 10;

/// Most segments a field path may have (`payload.a.b` style nesting).
pub const MAX_FIELD_SEGMENTS: usize = 5;

/// Longest single segment of a field path.
pub const MAX_FIELD_SEGMENT: usize = 64;

/// Longest string a comparison may use.
pub const MAX_COMPARISON_TEXT: usize = 512;

/// How a payload field is compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionOperator {
    /// The field equals the value exactly (JSON equality: a number is not a string).
    Equals,
    /// The field exists and differs from the value.
    NotEquals,
    /// The field is a string containing the value.
    Contains,
    /// The field is a string that does not contain the value.
    NotContains,
    /// The field is a string starting with the value.
    StartsWith,
    /// The field is a string ending with the value.
    EndsWith,
    /// The field's value is one of the values in an array.
    In,
    /// The field is present and not null.
    Exists,
    /// The field is absent or null.
    NotExists,
}

impl ConditionOperator {
    /// Every operator, in catalogue order.
    pub const ALL: [Self; 9] = [
        Self::Equals,
        Self::NotEquals,
        Self::Contains,
        Self::NotContains,
        Self::StartsWith,
        Self::EndsWith,
        Self::In,
        Self::Exists,
        Self::NotExists,
    ];

    /// Canonical name stored in the database and used in a request body.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Equals => "equals",
            Self::NotEquals => "not_equals",
            Self::Contains => "contains",
            Self::NotContains => "not_contains",
            Self::StartsWith => "starts_with",
            Self::EndsWith => "ends_with",
            Self::In => "in",
            Self::Exists => "exists",
            Self::NotExists => "not_exists",
        }
    }

    /// Parse a stored value.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|op| op.as_str() == raw)
    }

    /// `true` when the operator compares against a value.
    #[must_use]
    pub const fn needs_value(self) -> bool {
        !matches!(self, Self::Exists | Self::NotExists)
    }

    /// `true` when the operator is only meaningful for text fields.
    #[must_use]
    pub const fn is_text_operator(self) -> bool {
        matches!(
            self,
            Self::Contains | Self::NotContains | Self::StartsWith | Self::EndsWith
        )
    }
}

/// One condition of a rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Condition {
    /// Field path into the event payload (`slug`, `revision_no`, `author.name`).
    pub field: String,
    /// How the field is compared.
    pub operator: ConditionOperator,
    /// Value to compare with; absent for `exists`/`not_exists`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

impl Condition {
    /// A condition that compares a field with a value.
    #[must_use]
    pub fn compare(field: impl Into<String>, operator: ConditionOperator, value: Value) -> Self {
        Self {
            field: field.into(),
            operator,
            value: Some(value),
        }
    }

    /// A condition about a field's presence.
    #[must_use]
    pub fn presence(field: impl Into<String>, operator: ConditionOperator) -> Self {
        Self {
            field: field.into(),
            operator,
            value: None,
        }
    }

    /// Evaluate the condition against one event payload.
    ///
    /// A malformed comparison inside an already-validated list counts as "does not hold" rather
    /// than panicking: stored rows are validated when they are written, and a rule that cannot be
    /// read must never take the matcher down.
    #[must_use]
    pub fn holds(&self, payload: &Value) -> bool {
        let found = resolve_field(payload, &self.field);

        match self.operator {
            ConditionOperator::Exists => found.is_some_and(|value| !value.is_null()),
            ConditionOperator::NotExists => found.is_none_or(Value::is_null),
            ConditionOperator::Equals => match (found, &self.value) {
                (Some(found), Some(wanted)) => found == wanted,
                _ => false,
            },
            ConditionOperator::NotEquals => match (found, &self.value) {
                (Some(found), Some(wanted)) => found != wanted,
                _ => false,
            },
            ConditionOperator::Contains => {
                text(found, &self.value, |haystack: &str, needle: &str| {
                    haystack.contains(needle)
                })
            }
            ConditionOperator::NotContains => {
                text(found, &self.value, |haystack: &str, needle: &str| {
                    !haystack.contains(needle)
                })
            }
            ConditionOperator::StartsWith => {
                text(found, &self.value, |haystack: &str, needle: &str| {
                    haystack.starts_with(needle)
                })
            }
            ConditionOperator::EndsWith => {
                text(found, &self.value, |haystack: &str, needle: &str| {
                    haystack.ends_with(needle)
                })
            }
            ConditionOperator::In => match (found, &self.value) {
                (Some(found), Some(Value::Array(values))) => values.iter().any(|v| v == found),
                _ => false,
            },
        }
    }
}

/// Compare two strings when both sides are text.
fn text(
    found: Option<&Value>,
    wanted: &Option<Value>,
    predicate: impl Fn(&str, &str) -> bool,
) -> bool {
    match (
        found.and_then(Value::as_str),
        wanted.as_ref().and_then(Value::as_str),
    ) {
        (Some(found), Some(wanted)) => predicate(found, wanted),
        _ => false,
    }
}

/// Walk a dotted field path into a JSON object.
#[must_use]
pub fn resolve_field<'a>(payload: &'a Value, field: &str) -> Option<&'a Value> {
    let mut current = payload;
    for segment in field.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

/// Evaluate a whole condition list: every condition must hold.
#[must_use]
pub fn all_hold(conditions: &[Condition], payload: &Value) -> bool {
    conditions.iter().all(|condition| condition.holds(payload))
}

/// Check a condition list before it is stored.
pub fn validate(conditions: &[Condition]) -> Result<()> {
    if conditions.len() > MAX_CONDITIONS {
        return Err(AutomationError::invalid(
            "invalid_conditions",
            format!("a rule carries at most {MAX_CONDITIONS} conditions"),
        ));
    }

    for condition in conditions {
        validate_field(&condition.field)?;

        match (&condition.operator, &condition.value) {
            (operator, None) if operator.needs_value() => {
                return Err(AutomationError::invalid(
                    "invalid_conditions",
                    format!(
                        "the {} condition on {:?} needs a `value`",
                        operator.as_str(),
                        condition.field
                    ),
                ));
            }
            (operator, Some(_)) if !operator.needs_value() => {
                return Err(AutomationError::invalid(
                    "invalid_conditions",
                    format!(
                        "the {} condition on {:?} takes no `value`",
                        operator.as_str(),
                        condition.field
                    ),
                ));
            }
            _ => {}
        }

        if let (true, Some(value)) = (condition.operator.is_text_operator(), &condition.value) {
            match value.as_str() {
                Some(text) if text.chars().count() <= MAX_COMPARISON_TEXT => {}
                Some(_) => {
                    return Err(AutomationError::invalid(
                        "invalid_conditions",
                        format!(
                            "the {} condition compares at most {MAX_COMPARISON_TEXT} characters",
                            condition.operator.as_str()
                        ),
                    ));
                }
                None => {
                    return Err(AutomationError::invalid(
                        "invalid_conditions",
                        format!(
                            "the {} condition on {:?} compares text with text",
                            condition.operator.as_str(),
                            condition.field
                        ),
                    ));
                }
            }
        }

        if condition.operator == ConditionOperator::In {
            match &condition.value {
                Some(Value::Array(values)) if !values.is_empty() => {}
                _ => {
                    return Err(AutomationError::invalid(
                        "invalid_conditions",
                        format!(
                            "the in condition on {:?} needs a non-empty array of values",
                            condition.field
                        ),
                    ));
                }
            }
        }
    }

    Ok(())
}

/// Check a field path: dotted, alphanumeric-with-underscores, bounded.
fn validate_field(field: &str) -> Result<()> {
    let trimmed = field.trim();
    if trimmed.is_empty() {
        return Err(AutomationError::invalid(
            "invalid_conditions",
            "every condition names the payload field it tests",
        ));
    }

    let segments: Vec<&str> = trimmed.split('.').collect();
    if segments.len() > MAX_FIELD_SEGMENTS {
        return Err(AutomationError::invalid(
            "invalid_conditions",
            format!("a field path has at most {MAX_FIELD_SEGMENTS} segments"),
        ));
    }
    for segment in &segments {
        let well_formed = !segment.is_empty()
            && segment.len() <= MAX_FIELD_SEGMENT
            && segment.starts_with(|first: char| first.is_ascii_alphabetic() || first == '_')
            && segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !well_formed {
            return Err(AutomationError::invalid(
                "invalid_conditions",
                format!("{segment:?} is not a usable payload field name"),
            ));
        }
    }

    Ok(())
}

/// Serialize a single condition, as the rule builders send it.
pub fn to_json(condition: &Condition) -> Value {
    serde_json::to_value(condition).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload() -> Value {
        json!({
            "slug": "home",
            "status": "published",
            "revision_no": 4,
            "title": "Release notes",
            "draft": false,
            "tags": ["news", "release"],
            "author": { "name": "ada" },
        })
    }

    #[test]
    fn the_operators_round_trip() {
        for operator in ConditionOperator::ALL {
            assert_eq!(
                ConditionOperator::parse(operator.as_str()),
                Some(operator),
                "{operator:?}"
            );
        }
        assert_eq!(ConditionOperator::parse("matches_regex"), None);
        assert!(!ConditionOperator::Exists.needs_value());
        assert!(ConditionOperator::Equals.needs_value());
        assert!(ConditionOperator::Contains.is_text_operator());
        assert!(!ConditionOperator::Equals.is_text_operator());
    }

    #[test]
    fn comparisons_are_exact_and_typed() {
        assert!(
            Condition::compare("status", ConditionOperator::Equals, json!("published"))
                .holds(&payload())
        );
        assert!(
            !Condition::compare("status", ConditionOperator::Equals, json!("draft"))
                .holds(&payload())
        );
        // A number is not a string: the author must write the value as the payload carries it.
        assert!(
            !Condition::compare("revision_no", ConditionOperator::Equals, json!("4"))
                .holds(&payload())
        );
        assert!(
            Condition::compare("revision_no", ConditionOperator::Equals, json!(4))
                .holds(&payload())
        );
        assert!(
            Condition::compare("draft", ConditionOperator::Equals, json!(false)).holds(&payload())
        );
        assert!(
            !Condition::compare("draft", ConditionOperator::NotEquals, json!(false))
                .holds(&payload())
        );
    }

    #[test]
    fn text_operators_only_compare_text() {
        assert!(
            Condition::compare("title", ConditionOperator::Contains, json!("Release"))
                .holds(&payload())
        );
        assert!(
            Condition::compare("title", ConditionOperator::NotContains, json!("Draft"))
                .holds(&payload())
        );
        assert!(
            Condition::compare("slug", ConditionOperator::StartsWith, json!("ho"))
                .holds(&payload())
        );
        assert!(
            Condition::compare("slug", ConditionOperator::EndsWith, json!("me")).holds(&payload())
        );
        // A number field is not text, so a text comparison does not hold.
        assert!(
            !Condition::compare("revision_no", ConditionOperator::Contains, json!("4"))
                .holds(&payload())
        );
        assert!(
            !Condition::compare("missing", ConditionOperator::Contains, json!("x"))
                .holds(&payload())
        );
    }

    #[test]
    fn membership_and_presence() {
        assert!(
            Condition::compare("slug", ConditionOperator::In, json!(["home", "about"]))
                .holds(&payload())
        );
        assert!(
            !Condition::compare("slug", ConditionOperator::In, json!(["about"])).holds(&payload())
        );
        assert!(
            !Condition::compare("slug", ConditionOperator::In, json!("home")).holds(&payload())
        );

        assert!(Condition::presence("title", ConditionOperator::Exists).holds(&payload()));
        assert!(!Condition::presence("missing", ConditionOperator::Exists).holds(&payload()));
        assert!(Condition::presence("missing", ConditionOperator::NotExists).holds(&payload()));
        // An explicit null counts as absent.
        let nulls = json!({ "summary": null });
        assert!(!Condition::presence("summary", ConditionOperator::Exists).holds(&nulls));
        assert!(Condition::presence("summary", ConditionOperator::NotExists).holds(&nulls));
    }

    #[test]
    fn nested_fields_are_reachable() {
        assert!(
            Condition::compare("author.name", ConditionOperator::Equals, json!("ada"))
                .holds(&payload())
        );
        assert!(resolve_field(&payload(), "author.name").is_some());
        assert!(resolve_field(&payload(), "author.email").is_none());
        assert!(resolve_field(&payload(), "nope.deep").is_none());
    }

    #[test]
    fn every_condition_of_a_rule_must_hold() {
        let conditions = vec![
            Condition::compare("status", ConditionOperator::Equals, json!("published")),
            Condition::compare("title", ConditionOperator::Contains, json!("Release")),
        ];
        assert!(all_hold(&conditions, &payload()));

        let mut failing = conditions.clone();
        failing.push(Condition::compare(
            "slug",
            ConditionOperator::Equals,
            json!("about"),
        ));
        assert!(!all_hold(&failing, &payload()));
        assert!(
            all_hold(&[], &payload()),
            "a rule without conditions always holds"
        );
    }

    #[test]
    fn a_condition_list_is_checked_before_it_is_stored() {
        assert!(validate(&[]).is_ok());
        assert!(
            validate(&[Condition::compare(
                "status",
                ConditionOperator::Equals,
                json!("published")
            )])
            .is_ok()
        );
        assert!(validate(&[Condition::presence("title", ConditionOperator::Exists)]).is_ok());
        assert!(
            validate(&[Condition::compare(
                "title",
                ConditionOperator::In,
                json!(["a", "b"])
            )])
            .is_ok()
        );

        // Missing value, forbidden value, wrong value kind, empty membership, bad field names.
        for broken in [
            Condition::presence("status", ConditionOperator::Equals),
            Condition::compare("status", ConditionOperator::Exists, json!("x")),
            Condition::compare("title", ConditionOperator::Contains, json!(7)),
            Condition::compare("title", ConditionOperator::In, json!([])),
            Condition::compare("", ConditionOperator::Equals, json!("x")),
            Condition::compare("status!", ConditionOperator::Equals, json!("x")),
            Condition::compare("a.b.c.d.e.f", ConditionOperator::Equals, json!("x")),
        ] {
            let error = validate(std::slice::from_ref(&broken)).expect_err("refused");
            assert_eq!(error.code(), "invalid_conditions", "{broken:?}");
        }

        let many =
            vec![Condition::presence("title", ConditionOperator::Exists); MAX_CONDITIONS + 1];
        assert_eq!(
            validate(&many).expect_err("the cap holds").code(),
            "invalid_conditions"
        );
    }

    #[test]
    fn conditions_round_trip_through_json() {
        let condition = Condition::compare("status", ConditionOperator::Equals, json!("published"));
        let stored = serde_json::to_value(&condition).expect("serialises");
        assert_eq!(stored["operator"], "equals");
        assert_eq!(stored["value"], "published");
        let read: Condition = serde_json::from_value(stored).expect("reads back");
        assert_eq!(read, condition);

        // An existence condition stores no value at all.
        let presence = Condition::presence("title", ConditionOperator::Exists);
        let stored = serde_json::to_value(&presence).expect("serialises");
        assert!(stored.get("value").is_none());
        assert_eq!(
            serde_json::from_value::<Condition>(stored).expect("reads back"),
            presence
        );

        // A typo in a stored condition is reported, not silently dropped.
        assert!(
            serde_json::from_value::<Condition>(json!({
                "field": "status", "operator": "equals", "value": "x", "caseSensitive": true
            }))
            .is_err()
        );
    }
}
