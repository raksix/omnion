//! Bindings: `{{event.field}}` placeholders inside an action's parameters.
//!
//! A rule is written once and fires for many events, so its actions cannot carry fixed values
//! for things that change per event — the page that was published, the revision to comment on.
//! They carry placeholders instead:
//!
//! ```json
//! { "to": "editor@example.com",
//!   "subject": "Published: {{event.title}}",
//!   "body": "{{event.slug}} is live at revision {{event.revision_no}}." }
//! ```
//!
//! Resolution happens **when the run is materialised**, not when the step runs: the stored step
//! rows carry the values of the event that started the run, so a retry repeats exactly the first
//! attempt instead of re-reading a bus that may have moved on. That is also why validation can
//! only check the *shape* of a placeholder — whether `event.title` exists is a question only the
//! event can answer, and a placeholder the payload cannot fill refuses to start the run with a
//! message naming what the event did carry.
//!
//! The namespace is deliberately one: `event`, the payload of the recorded event. There is no
//! arithmetic, no function call, no way to reach anything else (docs/09-N8N-TEARDOWN.md §13
//! lesson 14).

use serde_json::{Map, Value};

use crate::error::{AutomationError, Result};

/// Opening of a placeholder.
const OPEN: &str = "{{";

/// Closing of a placeholder.
const CLOSE: &str = "}}";

/// Longest expression a placeholder may carry.
const MAX_EXPRESSION: usize = 160;

/// How many available keys a resolution failure names.
const KEYS_IN_MESSAGE: usize = 12;

/// Resolve every placeholder in an action's parameters against one event payload.
///
/// Objects and arrays are walked; a string that is exactly one placeholder becomes the resolved
/// value *as it is* (a number stays a number), and a string that mixes text and placeholders
/// becomes text. Anything else is copied through untouched.
pub fn resolve_params(params: &Value, payload: &Value) -> Result<Value> {
    resolve(params, payload, "$")
}

/// Check the placeholders of a value without an event: shapes, namespace and depth.
///
/// Called when a rule is written, so a mistyped placeholder is caught at the panel rather than
/// at the first event that fires the rule.
pub fn validate_bindings(value: &Value) -> Result<()> {
    walk(value, "$", &mut |path, text| {
        for expression in expressions(text)? {
            if !expression.starts_with("event") {
                return Err(AutomationError::invalid(
                    "invalid_binding",
                    format!(
                        "{{{expression}}} in {path} uses an unknown namespace; the only one \
                         available is `event`"
                    ),
                ));
            }
            // `event` alone would splice the whole payload into a string; it is almost always a
            // typo for a field, and it has no useful single-value form.
            if expression == "event" {
                return Err(AutomationError::invalid(
                    "invalid_binding",
                    format!("{{{expression}}} in {path} names no field, e.g. {{{{event.slug}}}}"),
                ));
            }
            let field = expression.trim_start_matches("event.").to_owned();
            if field.trim().is_empty() {
                return Err(AutomationError::invalid(
                    "invalid_binding",
                    format!("{{{expression}}} in {path} names no field"),
                ));
            }
        }
        Ok(())
    })
}

/// Walk every string of a JSON value and rewrite the ones carrying placeholders.
fn resolve(value: &Value, payload: &Value, path: &str) -> Result<Value> {
    match value {
        Value::String(text) => resolve_text(text, payload, path),
        Value::Array(items) => {
            let mut resolved = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                resolved.push(resolve(item, payload, &format!("{path}[{index}]"))?);
            }
            Ok(Value::Array(resolved))
        }
        Value::Object(map) => {
            let mut resolved = Map::with_capacity(map.len());
            for (key, item) in map {
                resolved.insert(
                    key.clone(),
                    resolve(item, payload, &format!("{path}.{key}"))?,
                );
            }
            Ok(Value::Object(resolved))
        }
        other => Ok(other.clone()),
    }
}

/// Rewrite one string: a lone placeholder becomes a value, anything else becomes text.
fn resolve_text(text: &str, payload: &Value, path: &str) -> Result<Value> {
    let found = expressions(text)?;
    if found.is_empty() {
        return Ok(Value::String(text.to_owned()));
    }

    // The common case by far: the whole string is one placeholder, and its type is preserved
    // (`revision_no` stays a number, a nested object stays an object).
    if found.len() == 1 {
        let expression = &found[0];
        let start = text.find(OPEN).unwrap_or_default();
        let end = text.rfind(CLOSE).unwrap_or_default() + CLOSE.len();
        if start == 0 && end == text.len() {
            return lookup(payload, expression, path).cloned();
        }
    }

    // Mixed text: every placeholder is rendered as text and spliced in, left to right.
    let mut rendered = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        rendered.push_str(&rest[..start]);
        let Some(end) = rest[start..].find(CLOSE) else {
            return Err(AutomationError::invalid(
                "invalid_binding",
                format!("a placeholder in {path} is never closed"),
            ));
        };
        let end = start + end + CLOSE.len();
        let expression = rest[start + OPEN.len()..end - CLOSE.len()].trim();
        let value = lookup(payload, expression, path)?;
        rendered.push_str(&display(value));
        rest = &rest[end..];
    }
    rendered.push_str(rest);

    Ok(Value::String(rendered))
}

/// Read `event.field.field` out of the payload.
fn lookup<'a>(payload: &'a Value, expression: &str, path: &str) -> Result<&'a Value> {
    let field = expression
        .strip_prefix("event.")
        .ok_or_else(|| {
            AutomationError::invalid(
                "invalid_binding",
                format!(
                    "{{{expression}}} in {path} is not a binding; the only namespace available \
                     is `event`"
                ),
            )
        })?
        .trim();
    if field.is_empty() {
        return Err(AutomationError::invalid(
            "invalid_binding",
            format!("{{{expression}}} in {path} names no field"),
        ));
    }

    let mut current = payload;
    for segment in field.split('.') {
        current = current.get(segment).ok_or_else(|| {
            AutomationError::invalid(
                "invalid_binding",
                format!(
                    "the event carries no `{field}` (asked for by {{event.{field}}} in {path}); \
                     it carries: {}",
                    available_keys(payload)
                ),
            )
        })?;
    }

    Ok(current)
}

/// The keys a payload carries, for a failure message a reader can act on.
fn available_keys(payload: &Value) -> String {
    let Some(map) = payload.as_object() else {
        return "no fields (the payload is not an object)".to_owned();
    };
    if map.is_empty() {
        return "no fields".to_owned();
    }

    let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let shown = keys.len().min(KEYS_IN_MESSAGE);
    let mut listed = keys[..shown].join(", ");
    if keys.len() > shown {
        listed.push_str(&format!(" ({} more)", keys.len() - shown));
    }
    listed
}

/// Render a resolved value inside mixed text.
fn display(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Every `{{…}}` expression of a string, in order, shape-checked.
fn expressions(text: &str) -> Result<Vec<String>> {
    let mut found = Vec::new();
    let mut rest = text;

    while let Some(start) = rest.find(OPEN) {
        let after = &rest[start + OPEN.len()..];
        let Some(end) = after.find(CLOSE) else {
            return Err(AutomationError::invalid(
                "invalid_binding",
                format!("a placeholder in {text:?} is never closed"),
            ));
        };

        let expression = after[..end].trim().to_owned();
        if expression.is_empty() {
            return Err(AutomationError::invalid(
                "invalid_binding",
                format!("{text:?} carries an empty placeholder"),
            ));
        }
        if expression.len() > MAX_EXPRESSION {
            return Err(AutomationError::invalid(
                "invalid_binding",
                format!("a placeholder is at most {MAX_EXPRESSION} characters"),
            ));
        }
        if expression.contains('{') || expression.contains('}') {
            return Err(AutomationError::invalid(
                "invalid_binding",
                format!("{{{expression}}} is not a usable placeholder"),
            ));
        }

        found.push(expression);
        rest = &after[end + CLOSE.len()..];
    }

    Ok(found)
}

/// Walk every string of a value and hand it to `check`, with its path.
fn walk(value: &Value, path: &str, check: &mut impl FnMut(&str, &str) -> Result<()>) -> Result<()> {
    match value {
        Value::String(text) => check(path, text),
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                walk(item, &format!("{path}[{index}]"), check)?;
            }
            Ok(())
        }
        Value::Object(map) => {
            for (key, item) in map {
                walk(item, &format!("{path}.{key}"), check)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload() -> Value {
        json!({
            "slug": "home",
            "title": "Release notes",
            "revision_id": "6f1a4a3c-0a2f-4a52-9d3a-3f4da1b1f0a2",
            "revision_no": 4,
            "summary": null,
        })
    }

    #[test]
    fn a_lone_placeholder_keeps_its_type() {
        let params = json!({ "revision_id": "{{event.revision_id}}", "revision_no": "{{event.revision_no}}" });
        let resolved = resolve_params(&params, &payload()).expect("resolves");
        assert_eq!(
            resolved["revision_id"],
            json!("6f1a4a3c-0a2f-4a52-9d3a-3f4da1b1f0a2")
        );
        assert_eq!(
            resolved["revision_no"],
            json!(4),
            "the number stays a number"
        );
    }

    #[test]
    fn mixed_text_is_rendered_with_the_values() {
        let params = json!({
            "subject": "Published: {{event.title}}",
            "body": "{{event.slug}} is live at revision {{event.revision_no}}.",
        });
        let resolved = resolve_params(&params, &payload()).expect("resolves");
        assert_eq!(resolved["subject"], json!("Published: Release notes"));
        assert_eq!(resolved["body"], json!("home is live at revision 4."));
    }

    #[test]
    fn nested_values_and_absent_fields() {
        let nested = json!({
            "author": { "name": "ada" },
            "tags": ["a", "b"],
            "summary": null,
        });
        let params = json!({
            "to": "{{event.author.name}}@example.com",
            "lone": "{{event.tags}}",
            "mixed": "tags: {{event.tags}}",
            "empty": "{{event.summary}}",
            "empty_mixed": "note: {{event.summary}}",
        });
        let resolved = resolve_params(&params, &nested).expect("resolves");
        assert_eq!(resolved["to"], json!("ada@example.com"));
        // A lone placeholder keeps the value's own shape …
        assert_eq!(resolved["lone"], json!(["a", "b"]));
        // … while inside text it is rendered.
        assert_eq!(resolved["mixed"], json!("tags: [\"a\",\"b\"]"));
        // A null field exists and resolves to null; rendered inside text it reads as nothing.
        assert_eq!(resolved["empty"], Value::Null);
        assert_eq!(resolved["empty_mixed"], json!("note: "));
    }

    #[test]
    fn parameters_without_placeholders_are_copied_through() {
        let params = json!({ "to": "ada@example.com", "retries": 2, "tags": ["x"], "flag": true });
        assert_eq!(
            resolve_params(&params, &payload()).expect("resolves"),
            params
        );
    }

    #[test]
    fn a_placeholder_the_event_cannot_fill_is_reported() {
        let params = json!({ "subject": "About {{event.nope}}" });
        let error = resolve_params(&params, &payload()).expect_err("cannot resolve");
        assert_eq!(error.code(), "invalid_binding");
        let message = error.to_string();
        assert!(message.contains("nope"), "{message}");
        assert!(
            message.contains("slug"),
            "the message lists what the event carries: {message}"
        );
    }

    #[test]
    fn validation_checks_shape_and_namespace() {
        assert!(validate_bindings(&json!({ "subject": "{{event.title}}" })).is_ok());
        assert!(validate_bindings(&json!({ "body": "no placeholders here" })).is_ok());

        for broken in [
            json!({ "subject": "{{site.name}}" }),
            json!({ "subject": "{{event}}" }),
            json!({ "subject": "{{}}" }),
            json!({ "subject": "{{event.title" }),
            json!({ "subject": "{{event.}}" }),
        ] {
            let error = validate_bindings(&broken).expect_err("refused");
            assert_eq!(error.code(), "invalid_binding", "{broken}");
        }
    }

    #[test]
    fn the_placeholder_cap_is_enforced() {
        let long = format!("{{{{event.{}}}}}", "a".repeat(MAX_EXPRESSION));
        assert_eq!(
            validate_bindings(&json!({ "subject": long }))
                .expect_err("too long")
                .code(),
            "invalid_binding"
        );
    }
}
