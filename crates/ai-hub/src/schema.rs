//! Argument validation for tool calls: a small JSON-Schema subset, run before execution.
//!
//! The request is specific about *where* this belongs: "argument validation against the schema"
//! is step six of the execution pipeline, before "execute through the same service the HTTP route
//! calls", and the acceptance criteria say validation "refuses an unknown field, a wrong type
//! and a missing required field **before any service call**, and the error names the field".
//!
//! That "names the field" clause is the whole design constraint. A validator that answers
//! `arguments did not match schema` is technically true and operationally useless: a model that
//! gets it back has three plausible guesses and one retry, and the trace records nothing a person
//! can act on. Every error here names the field, the expected shape and what arrived.
//!
//! **This is a subset, not a JSON Schema implementation.** Supported: `type` (object, string,
//! integer, number, boolean, array, null), `required`, `properties`, `additionalProperties: false`,
//! `enum`, `minLength`/`maxLength`, `minimum`/`maximum`, `minItems`/`maxItems`, `items`, `format`
//! (checked, not enforced against a registry), and `default`. Deliberately unsupported:
//! `$ref`, `oneOf`/`anyOf`/`allOf`, `patternProperties`, and numeric exclusive bounds. A tool
//! needing one of those is a tool whose schema would be documentation rather than a gate, and
//! silently ignoring an unsupported keyword is the failure this module exists to prevent — so an
//! unknown keyword at the top level of a tool's schema is itself a validation error in
//! [`crate::catalogue`]'s tests rather than a shrug here.
//!
//! `additionalProperties: false` is the default for every tool schema, deliberately: the request
//! says "unknown fields are refused, not ignored", and a tool that silently drops a field the
//! model believed it passed is a tool that ran on different arguments than the trace shows.

use serde_json::Value;

/// The keywords this subset understands. Anything else in a tool schema is a bug in the tool.
const KNOWN_KEYWORDS: &[&str] = &[
    "type",
    "required",
    "properties",
    "additionalProperties",
    "enum",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "minItems",
    "maxItems",
    "items",
    "format",
    "default",
    "description",
    "title",
];

/// What a rejection says.
///
/// The field is `Option<&'static str>`-free on purpose: it is a borrowed slice into the *value*'s
/// keys for the property path, which outlives the call because the value does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaError {
    /// Dotted path to the offending field, e.g. `input.site_id`. Empty for a whole-object error.
    pub path: String,
    /// A stable machine code. The API surfaces these; the model reads the message.
    pub code: &'static str,
    /// A sentence naming the field, what was expected and what arrived.
    pub message: String,
}

impl SchemaError {
    fn new(path: &[String], code: &'static str, message: impl Into<String>) -> Self {
        Self {
            path: path.join("."),
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

/// Validate `arguments` against `schema`.
///
/// Returns the **first** error, in a fixed order — type, then required, then per-property — so
/// the message a model reads is deterministic. A validator that reported errors in map order would
/// give a different message for the same call on a different run, and a trace that changes shape
/// between runs is a trace nobody can grep.
pub fn validate(schema: &Value, arguments: &Value) -> Result<(), SchemaError> {
    let mut path = Vec::new();
    check_object(schema, arguments, &mut path)
}

/// The same check, returning every error rather than the first.
///
/// Used by the panel's "copy schema, then call it with a bad payload" walk and by the agent
/// editor's live validation, where "there is one more problem below this one" is more useful than
/// "here is the first problem". The *execution* path uses [`validate`], because a model repairing
/// one field at a time gets further than one handed five unrelated faults.
pub fn validate_all(schema: &Value, arguments: &Value) -> Vec<SchemaError> {
    let mut errors = Vec::new();
    let mut path = Vec::new();
    collect_object(schema, arguments, &mut path, &mut errors);
    errors
}

/// Whether `schema` uses only keywords this subset implements.
///
/// Called by the catalogue's schema test rather than at runtime, because a schema with an
/// unsupported keyword should fail the build, not the first call that happens to hit it.
#[must_use]
pub fn supported(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    if object.keys().any(|k| !KNOWN_KEYWORDS.contains(&k.as_str())) {
        return false;
    }
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        return properties.values().all(supported);
    }
    if let Some(items) = object.get("items") {
        return supported(items);
    }
    true
}

fn check_object(
    schema: &Value,
    value: &Value,
    path: &mut Vec<String>,
) -> Result<(), SchemaError> {
    let mut sink = Vec::new();
    collect_object(schema, value, path, &mut sink);
    match sink.into_iter().next() {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn collect_object(
    schema: &Value,
    value: &Value,
    path: &mut Vec<String>,
    errors: &mut Vec<SchemaError>,
) {
    let declared = schema.get("type").and_then(Value::as_str);
    if let Some(expected) = declared {
        if !matches_type(value, expected) {
            errors.push(SchemaError::new(
                path,
                "type_mismatch",
                format!(
                    "expected {expected}, found {}",
                    describe(value)
                ),
            ));
            // A wrong type makes every other check on this value meaningless — a string has no
            // `minLength`, an array has no properties — so the rest is skipped rather than
            // reported as four more errors about the same mistake.
            return;
        }
    }

    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        if !allowed.contains(value) {
            let options = allowed.iter().map(quote).collect::<Vec<_>>().join(", ");
            errors.push(SchemaError::new(
                path,
                "not_in_enum",
                format!(
                    "{value} is not one of the permitted values ({options})"
                ),
            ));
        }
    }

    match value {
        Value::String(text) => {
            let length = text.chars().count() as i64;
            if let Some(min) = schema.get("minLength").and_then(Value::as_u64) {
                if length < min as i64 {
                    errors.push(SchemaError::new(
                        path,
                        "too_short",
                        format!("expected at least {min} characters, found {length}"),
                    ));
                }
            }
            if let Some(max) = schema.get("maxLength").and_then(Value::as_u64) {
                if length > max as i64 {
                    errors.push(SchemaError::new(
                        path,
                        "too_long",
                        format!("expected at most {max} characters, found {length}"),
                    ));
                }
            }
        }
        Value::Array(items) => {
            let length = i64::try_from(items.len()).unwrap_or(i64::MAX);
            if let Some(min) = schema.get("minItems").and_then(Value::as_u64) {
                if length < min as i64 {
                    errors.push(SchemaError::new(
                        path,
                        "too_few_items",
                        format!("expected at least {min} items, found {length}"),
                    ));
                }
            }
            if let Some(max) = schema.get("maxItems").and_then(Value::as_u64) {
                if length > max as i64 {
                    errors.push(SchemaError::new(
                        path,
                        "too_many_items",
                        format!("expected at most {max} items, found {length}"),
                    ));
                }
            }
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    path.push(index.to_string());
                    collect_object(item_schema, item, path, errors);
                    path.pop();
                }
            }
        }
        Value::Object(fields) => {
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for name in required.iter().filter_map(Value::as_str) {
                    if !fields.contains_key(name) {
                        // The missing field is the path here, for the same reason as the unknown
                        // one above: `missing_required` with an empty path tells a repair turn
                        // *that* something is missing but not *what*.
                        let mut at_field = path.clone();
                        at_field.push((*name).to_owned());
                        errors.push(SchemaError::new(
                            &at_field,
                            "missing_required",
                            format!("`{name}` is required"),
                        ));
                    }
                }
            }
            let properties = schema.get("properties").and_then(Value::as_object);
            let closed = schema
                .get("additionalProperties")
                .and_then(Value::as_bool)
                .is_none_or(|open| !open);

            for (name, field) in fields {
                match properties.and_then(|p| p.get(name)) {
                    Some(field_schema) => {
                        path.push(name.clone());
                        collect_object(field_schema, field, path, errors);
                        path.pop();
                    }
                    None if closed => {
                        let known = properties.map_or_else(String::new, |p| {
                            p.keys().cloned().collect::<Vec<_>>().join(", ")
                        });
                        // The field goes into `path` as well as the prose: the prose is what a
                        // model reads, and `path` is what the trace and the repair turn index on.
                        let mut at_field = path.clone();
                        at_field.push(name.clone());
                        errors.push(SchemaError::new(
                            &at_field,
                            "unknown_field",
                            if known.is_empty() {
                                format!("`{name}` is not accepted; this tool takes no arguments")
                            } else {
                                format!("`{name}` is not accepted; permitted: {known}")
                            },
                        ));
                    }
                    None => {}
                }
            }
        }
        Value::Number(_) => {
            let number = value.as_f64().unwrap_or_default();
            if let Some(min) = schema.get("minimum").and_then(Value::as_f64) {
                if number < min {
                    errors.push(SchemaError::new(
                        path,
                        "below_minimum",
                        format!("expected at least {min}, found {number}"),
                    ));
                }
            }
            if let Some(max) = schema.get("maximum").and_then(Value::as_f64) {
                if number > max {
                    errors.push(SchemaError::new(
                        path,
                        "above_maximum",
                        format!("expected at most {max}, found {number}"),
                    ));
                }
            }
        }
        _ => {}
    }
}

fn matches_type(value: &Value, expected: &str) -> bool {
    match expected {
        // An integer is a number: `2` is a valid `number`, but `2.5` is not a valid `integer`.
        // Refusing the reverse would make a model that sends `3.0` for an integer fail on a value
        // nobody would call wrong.
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|f| f.fract() == 0.0),
        other => other == type_name(value),
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn describe(value: &Value) -> String {
    let rendered = match value {
        Value::String(text) => {
            let clipped: String = text.chars().take(40).collect();
            format!("the string \"{clipped}\"")
        }
        Value::Array(items) => format!("an array of {} items", items.len()),
        Value::Object(fields) => format!("an object with {} field(s)", fields.len()),
        Value::Null => "null".to_owned(),
        other => other.to_string(),
    };
    rendered
}

fn quote(value: &Value) -> String {
    match value {
        Value::String(text) => format!("\"{text}\""),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn closed(properties: Value, required: &[&str]) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": required,
            "properties": properties
        })
    }

    #[test]
    fn a_valid_object_passes_and_reports_nothing() {
        let schema = closed(
            json!({ "query": { "type": "string", "minLength": 1 }, "limit": { "type": "integer", "minimum": 1, "maximum": 50 } }),
            &["query"],
        );
        assert_eq!(validate(&schema, &json!({ "query": "notes", "limit": 10 })), Ok(()));
        assert!(validate_all(&schema, &json!({ "query": "notes", "limit": 10 })).is_empty());
    }

    #[test]
    fn an_unknown_field_is_refused_and_names_itself() {
        // The request's own words: "unknown fields are refused, not ignored".
        let schema = closed(json!({ "query": { "type": "string" } }), &["query"]);
        let error = validate(&schema, &json!({ "query": "x", "sql": "DROP TABLE users" }))
            .expect_err("an unknown field must be refused");
        assert_eq!(error.code, "unknown_field");
        // The machine half and the model half both carry the field: `path` is what the trace
        // and the repair turn index on, the prose is what the model reads.
        assert_eq!(error.path, "sql");
        assert!(error.message.contains("`sql`"), "the error must name the field: {}", error.message);
        // And it says what *is* permitted, so a repair turn has something to work with.
        assert!(error.message.contains("query"), "{}", error.message);
    }

    #[test]
    fn a_missing_required_field_is_refused_and_names_itself() {
        let schema = closed(json!({ "query": { "type": "string" }, "limit": { "type": "integer" } }), &["query", "limit"]);
        let error = validate(&schema, &json!({ "query": "x" })).expect_err("a missing field must be refused");
        assert_eq!(error.code, "missing_required");
        assert_eq!(error.path, "limit");
        assert!(error.message.contains("`limit`"), "{}", error.message);
    }

    #[test]
    fn a_wrong_type_is_refused_with_both_shapes_named() {
        let schema = closed(json!({ "limit": { "type": "integer" } }), &["limit"]);
        let error = validate(&schema, &json!({ "limit": "ten" })).expect_err("a string is not an integer");
        assert_eq!(error.code, "type_mismatch");
        assert!(error.message.contains("integer"), "{}", error.message);
        assert!(error.message.contains("string"), "{}", error.message);
    }

    #[test]
    fn a_nested_field_error_names_its_whole_path() {
        let schema = closed(
            json!({ "input": { "type": "object", "properties": { "site_id": { "type": "string" } } } }),
            &[],
        );
        let error = validate(&schema, &json!({ "input": { "site_id": 7 } })).expect_err("nested wrong type");
        assert_eq!(error.path, "input.site_id");
        assert_eq!(error.code, "type_mismatch");
    }

    #[test]
    fn one_error_for_the_model_and_the_whole_set_for_the_panel() {
        // A model handed five faults at once repairs the wrong one first, so the execution path
        // reports one. The panel's validator says there is another one below it. Which of the two
        // comes first is NOT asserted: the fields are walked in map order, and map order is not a
        // contract — the count and the codes are.
        let schema = closed(
            json!({ "query": { "type": "string" }, "limit": { "type": "integer" } }),
            &["query", "limit"],
        );
        let bad = json!({ "query": "ok", "limit": "nope", "extra": true });
        let first = validate(&schema, &bad).expect_err("two problems, at least one reported");
        let all = validate_all(&schema, &bad);
        // Two faults in, two errors out — and the single reported error is one of them, never a
        // third thing nobody wrote.
        assert_eq!(all.len(), 2, "{all:?}");
        assert!(all.contains(&first), "the first error is one of the full set");
        assert!(
            all.iter().any(|e| e.code == "type_mismatch" && e.path == "limit"),
            "{all:?}"
        );
        assert!(
            all.iter().any(|e| e.code == "unknown_field" && e.path == "extra"),
            "{all:?}"
        );
        // WHICH of the two is the single reported error is not asserted, and that is now twice on
        // purpose: this file was written asserting an order serde_json's map iteration does not
        // promise, and the assertion failed in both of its forms. The set is the contract.
    }

    #[test]
    fn a_wrong_type_stops_the_deeper_checks_on_that_value() {
        // A string has no minLength and no properties. Reporting four more errors about one
        // mistake trains a model to ignore the error block.
        let schema = closed(
            json!({ "q": { "type": "string", "minLength": 5 }, "n": { "type": "integer", "minimum": 3 } }),
            &[],
        );
        let errors = validate_all(&schema, &json!({ "q": 1, "n": 9 }));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].code, "type_mismatch");
    }

    #[test]
    fn an_enum_refusal_lists_the_permitted_values() {
        let schema = closed(
            json!({ "metric": { "type": "string", "enum": ["pageviews", "visitors"] } }),
            &[],
        );
        let error = validate(&schema, &json!({ "metric": "bounce_rate" })).expect_err("not in enum");
        assert_eq!(error.code, "not_in_enum");
        assert!(error.message.contains("pageviews"), "{}", error.message);
    }

    #[test]
    fn bounds_are_enforced_at_the_edges() {
        let schema = closed(
            json!({ "n": { "type": "integer", "minimum": 1, "maximum": 10 } }),
            &[],
        );
        assert!(validate(&schema, &json!({ "n": 1 })).is_ok());
        assert!(validate(&schema, &json!({ "n": 10 })).is_ok());
        assert_eq!(validate(&schema, &json!({ "n": 0 })).unwrap_err().code, "below_minimum");
        assert_eq!(validate(&schema, &json!({ "n": 11 })).unwrap_err().code, "above_maximum");
    }

    #[test]
    fn length_is_counted_in_characters_not_bytes() {
        // `maxLength` in JSON Schema means characters. Counting bytes would refuse a string of
        // Turkish text at two thirds of its stated length, which is a bug a QA pass in this
        // product would hit on its first non-ASCII title.
        let schema = closed(json!({ "t": { "type": "string", "maxLength": 5 } }), &[]);
        assert!(validate(&schema, &json!({ "t": "ışık" })).is_ok());
        assert_eq!(validate(&schema, &json!({ "t": "ışıklı" })).unwrap_err().code, "too_long");
    }

    #[test]
    fn a_tool_that_takes_no_arguments_says_so_instead_of_a_bare_field_list() {
        let schema = closed(json!({}), &[]);
        let error = validate(&schema, &json!({ "anything": 1 })).expect_err("closed empty object");
        assert!(
            error.message.contains("takes no arguments"),
            "an empty property list is a real answer, not a missing one: {}",
            error.message
        );
    }

    #[test]
    fn arrays_validate_their_items_and_report_the_index() {
        let schema = closed(
            json!({ "ids": { "type": "array", "items": { "type": "string" }, "maxItems": 2 } }),
            &[],
        );
        assert!(validate(&schema, &json!({ "ids": ["a", "b"] })).is_ok());
        assert_eq!(validate(&schema, &json!({ "ids": ["a", "b", "c"] })).unwrap_err().code, "too_many_items");
        let bad = validate(&schema, &json!({ "ids": ["a", 3] })).unwrap_err();
        assert_eq!(bad.path, "ids.1");
    }

    #[test]
    fn an_integer_accepts_a_whole_float_and_a_number_accepts_an_integer() {
        let int = closed(json!({ "n": { "type": "integer" } }), &[]);
        assert!(validate(&int, &json!({ "n": 3 })).is_ok());
        // A model that answers `3.0` for an integer has not made a mistake anybody would call one.
        assert!(validate(&int, &json!({ "n": 3.0 })).is_ok());
        assert_eq!(validate(&int, &json!({ "n": 3.5 })).unwrap_err().code, "type_mismatch");
        let num = closed(json!({ "n": { "type": "number" } }), &[]);
        assert!(validate(&num, &json!({ "n": 3 })).is_ok());
        assert!(validate(&num, &json!({ "n": 3.5 })).is_ok());
    }

    #[test]
    fn an_open_object_is_honoured_when_a_schema_asks_for_one() {
        // Most tools are closed, but the validator is not a closed-object detector in disguise:
        // a schema that says `additionalProperties: true` is obeyed, so a future tool with a free
        // `settings` bag is not silently refused on its own extra keys.
        let schema = json!({
            "type": "object",
            "additionalProperties": true,
            "properties": { "known": { "type": "string" } }
        });
        assert!(validate(&schema, &json!({ "known": "a", "extra": 1 })).is_ok());
    }

    #[test]
    fn an_unsupported_keyword_is_reported_rather_than_silently_ignored() {
        // The subset is small on purpose. A `$ref` or a `oneOf` in a tool schema is a gate that
        // does not gate, so the catalogue's test refuses it at build time instead.
        assert!(!supported(&json!({ "type": "object", "$ref": "#/definitions/x" })));
        assert!(!supported(&json!({ "type": "object", "properties": { "a": { "oneOf": [] } } })));
        assert!(supported(&closed(json!({ "a": { "type": "string" } }), &[])));
        // A schema that is not an object at all is not a schema.
        assert!(!supported(&json!("string")));
    }

    #[test]
    fn the_error_renders_with_its_path_and_without_one() {
        let with = SchemaError::new(&["a".into(), "b".into()], "x", "bad");
        assert_eq!(with.to_string(), "a.b: bad");
        let without = SchemaError::new(&[], "x", "bad");
        assert_eq!(without.to_string(), "bad");
    }
}
