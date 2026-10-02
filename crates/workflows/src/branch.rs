//! The comparison a `branch` step makes, and how the engine reads what it compares.
//!
//! A branch is the engine's own control step: "if this field does not stand in this
//! operator against this value, end the run". It is *not* the automation layer's condition
//! tree — those read the event that started the run, and a branch reads what a *step*
//! produced, which the engine alone knows.
//!
//! Two namespaces, and both are resolved against one object the engine builds per step:
//!
//! * `steps.<n>.<field>` — a field of a previous step's output, `n` being the step number
//!   the trace shows (so `steps.2.action` is step 2's action key);
//! * `event.<field>` — the same payload the trigger arrived with, so a branch can re-test the
//!   event at a later point in the run.
//!
//! A field that resolves to nothing is a **failed** comparison, not a silent pass: a branch
//! on a field no step produced would otherwise read as "keep going", which is the one
//! answer that hides a broken definition.

use serde_json::Value;

/// The `action` a branch step carries, so the engine can recognise one from a stored row.
pub const BRANCH_ACTION: &str = "branch";

/// The comparison a branch makes, in the closed operator set.
///
/// Deliberately the *same* nine operators the panel offers for a condition
/// (`omnion_automation::groups`) — an author who learned one does not have to learn two —
/// but owned here, because the engine evaluates a branch and may not depend on the layer
/// that authors the rules. The two sets are asserted equal in the automation crate's tests.
pub const OPERATORS: &[&str] = &[
    "equals",
    "not_equals",
    "contains",
    "not_contains",
    "starts_with",
    "ends_with",
    "in",
    "exists",
    "not_exists",
];

/// Operators that compare against nothing.
const VALUELESS: &[&str] = &["exists", "not_exists"];

/// Longest a branch's field path may be.
pub const MAX_FIELD: usize = 200;

/// Longest a stop reason may be.
pub const MAX_STOP_REASON: usize = 400;

/// Check a branch step's parameters, and say what is wrong in words.
pub fn validate_params(params: &Value) -> Result<(), String> {
    let field = params.get("field").and_then(Value::as_str).unwrap_or("");
    if field.trim().is_empty() {
        return Err("a branch step needs a `field` to read".to_owned());
    }
    if field.chars().count() > MAX_FIELD {
        return Err(format!("a branch field is at most {MAX_FIELD} characters"));
    }
    if !field.starts_with("steps.") && !field.starts_with("event.") {
        return Err(format!(
            "`{field}` is not something a branch can read; use `event.<field>` or \
             `steps.<number>.<field>`"
        ));
    }
    // The step number must be a *number*. `steps.one.ok` would otherwise be accepted here
    // and then fail at run time, long after the author could have been told.
    if let Some(number) = field
        .strip_prefix("steps.")
        .and_then(|rest| rest.split('.').next())
    {
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!(
                "`{field}` names a step that cannot exist; the step number must be a number, \
                 as in `steps.2.ok`"
            ));
        }
    }

    let operator = params.get("operator").and_then(Value::as_str).unwrap_or("");
    if !OPERATORS.contains(&operator) {
        return Err(format!(
            "`{operator}` is not a comparison; use one of: {}",
            OPERATORS.join(", ")
        ));
    }

    // A branch's value may be a `{{ }}` placeholder, so only its presence is checked.
    if !VALUELESS.contains(&operator) && params.get("value").is_none() {
        return Err(format!("the `{operator}` comparison needs a `value`"));
    }

    Ok(())
}

/// Read a dotted field out of a JSON object, one segment at a time.
#[must_use]
pub fn read_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cursor = value;
    for segment in path.split('.') {
        cursor = cursor.as_object()?.get(segment)?;
    }
    Some(cursor)
}

/// Evaluate one branch step against what the run knows right now.
///
/// `value` is the object the field is read from: `{"event": <payload>, "steps": {"1": …,
/// "2": …}}`, built by the engine. A field that resolves to nothing **fails** the branch
/// and the engine ends the run with a message that names the field — a branch on a step
/// that never produced that key is a broken definition, and quietly continuing would hide
/// it until somebody wondered why the "guard" never fired.
pub fn evaluate(params: &Value, value: &Value) -> Result<bool, String> {
    validate_params(params)?;

    let field = params
        .get("field")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let operator = params
        .get("operator")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();

    let found = read_path(value, field);

    // `exists` and `not_exists` are the two operators that *answer about absence*, so a
    // missing field is their subject rather than their failure. Every other operator
    // cannot compare something that is not there, and says so — a guard that silently read
    // as "keep going" would hide a broken definition.
    match operator {
        "exists" => return Ok(found.is_some_and(|value| !value.is_null())),
        "not_exists" => return Ok(found.map_or(true, serde_json::Value::is_null)),
        _ => {}
    }

    let Some(found) = found else {
        return Err(format!("`{field}` is not something this run knows yet"));
    };

    // The comparison value may itself be a placeholder the caller resolved; an unresolved
    // one is compared as text, which is what the definition said and what the trace shows.
    let expected = params.get("value").unwrap_or(&Value::Null);

    Ok(compare(found, operator, expected))
}

/// One comparison, on the same terms the automation layer uses.
fn compare(found: &Value, operator: &str, expected: &Value) -> bool {
    match operator {
        "equals" => found == expected,
        "not_equals" => found != expected,
        "contains" => contains(found, expected),
        "not_contains" => !contains(found, expected),
        "starts_with" => text_of(found)
            .is_some_and(|found| text_of(expected).is_some_and(|exp| found.starts_with(&exp))),
        "ends_with" => text_of(found)
            .is_some_and(|found| text_of(expected).is_some_and(|exp| found.ends_with(&exp))),
        "in" => expected
            .as_array()
            .is_some_and(|list| list.iter().any(|item| item == found)),
        // Every other operator is refused by `validate_params`; answering `false` here keeps
        // an operator a future release adds from panicking an old row.
        _ => false,
    }
}

/// `contains` for a string or an array, whichever the field is.
fn contains(found: &Value, expected: &Value) -> bool {
    match (found, expected) {
        (Value::String(found), Value::String(expected)) => found.contains(expected.as_str()),
        (Value::Array(items), expected) => items.iter().any(|item| item == expected),
        _ => false,
    }
}

/// The text of a value, for the two text operators.
fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scope() -> Value {
        json!({
            "event": { "status": "published", "tags": ["news", "release"] },
            "steps": {
                "1": { "action": "echo", "value": "hello" },
                "2": { "action": "http_request", "status_code": 200, "ok": true }
            }
        })
    }

    fn branch(field: &str, operator: &str, value: Value) -> Value {
        json!({ "field": field, "operator": operator, "value": value })
    }

    #[test]
    fn a_branch_reads_the_event_and_the_steps_before_it() {
        let scope = scope();
        assert!(evaluate(&branch("steps.2.ok", "equals", json!(true)), &scope).expect("read"));
        assert!(
            evaluate(&branch("steps.2.status_code", "equals", json!(200)), &scope).expect("read")
        );
        assert!(
            evaluate(
                &branch("event.status", "equals", json!("published")),
                &scope
            )
            .expect("read")
        );
        assert!(!evaluate(&branch("steps.2.ok", "equals", json!(false)), &scope).expect("read"));
    }

    #[test]
    fn the_nine_operators_all_answer() {
        let scope = scope();
        let holds = |params: Value| evaluate(&params, &scope).expect("a field that exists");

        assert!(holds(branch("event.status", "not_equals", json!("draft"))));
        assert!(holds(branch("event.tags", "contains", json!("news"))));
        assert!(holds(branch("event.tags", "not_contains", json!("draft"))));
        assert!(holds(branch("event.status", "starts_with", json!("pub"))));
        assert!(holds(branch("event.status", "ends_with", json!("shed"))));
        assert!(holds(branch(
            "event.status",
            "in",
            json!(["draft", "published"])
        )));
        assert!(holds(branch("steps.2.ok", "exists", Value::Null)));
        assert!(holds(branch("steps.9.ok", "not_exists", Value::Null)));
    }

    #[test]
    fn a_field_nothing_produced_fails_the_branch_and_says_which() {
        // A guard on a field that does not exist must not read as "keep going": that is the
        // one answer that hides a broken definition.
        let scope = scope();
        let error = evaluate(
            &branch("steps.7.output.token", "equals", json!("x")),
            &scope,
        )
        .expect_err("step 7 never ran");
        assert!(error.contains("steps.7.output.token"), "{error}");

        // An event field the payload did not carry behaves the same way.
        let error = evaluate(&branch("event.author.email", "equals", json!("x")), &scope)
            .expect_err("the payload has no author");
        assert!(error.contains("event.author.email"), "{error}");
    }

    #[test]
    fn a_branch_that_names_something_unreadable_is_refused_when_written() {
        for broken in [
            json!({ "operator": "equals", "value": 1 }),
            json!({ "field": "  ", "operator": "equals", "value": 1 }),
            json!({ "field": "steps.1.ok" }),
            json!({ "field": "steps.1.ok", "operator": "matches" }),
            json!({ "field": "status", "operator": "equals", "value": 1 }),
            json!({ "field": "steps.one.ok", "operator": "equals", "value": 1 }),
            json!({ "field": "steps.1.ok", "operator": "equals" }),
        ] {
            assert!(
                validate_params(&broken).is_err(),
                "{broken} should be refused"
            );
        }

        // A placeholder is a value: resolution happens when the run is materialised.
        assert!(
            validate_params(&branch(
                "steps.1.value",
                "equals",
                json!("{{steps.0.value}}")
            ))
            .is_ok()
        );
        // `exists` and `not_exists` need nothing.
        assert!(validate_params(&branch("steps.1.value", "exists", Value::Null)).is_ok());
    }

    #[test]
    fn the_branch_operator_set_is_the_conditions_one() {
        // The two lists are compared by the automation crate's tests; this is the assertion
        // that keeps a future operator from being added to one side only.
        assert_eq!(OPERATORS.len(), 9);
        assert!(OPERATORS.contains(&"in"));
    }

    #[test]
    fn a_field_path_reads_only_objects() {
        let value = json!({ "a": { "b": "c" }, "list": [1, 2] });
        assert_eq!(read_path(&value, "a.b"), Some(&json!("c")));
        assert_eq!(
            read_path(&value, "list.0"),
            None,
            "an array is not an object"
        );
        assert_eq!(read_path(&value, "a.missing"), None);
        assert_eq!(read_path(&value, "missing.b"), None);
    }
}
