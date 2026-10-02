//! The catalogue as a machine-readable contract: a JSON Schema per event, and a sample that
//! validates against it.
//!
//! # Why this file exists
//!
//! The catalogue already answers *what an event is* — its name, its area, a sentence about it,
//! and the fields its payload carries with the kind of each. That is enough to render a list and
//! enough to build a picker's checkbox. It is **not** enough to answer the two questions a
//! developer asks before subscribing:
//!
//! 1. *What exactly will land in my receiver?* A field list with a kind per row reads as a
//!    table; a JSON Schema reads as a contract somebody can run in CI.
//! 2. *Can I try it?* A sample that does not satisfy its own schema is worse than no sample —
//!    a receiver that copies it, fails validation in production, and blames the platform.
//!
//! Both come out of the *same* registry row, which is the point: a second hand-written schema
//! file would drift from the table the emitters are checked against, and the drift is invisible
//! until a subscriber's parser breaks.
//!
//! # The validator is a subset, deliberately
//!
//! There is a real JSON Schema library in the ecosystem and this file does not use it, for
//! three reasons that are worth stating because they are the kind of thing a reviewer will ask:
//!
//! * **A vendor dependency in the events crate is a permanent cost.** The crate is deliberately
//!   small and is compiled into every writer's QA binary on a box with seven concurrent builds.
//!   `jsonschema` pulls a resolver, a network-aware registry and a `regex-automata` chain.
//! * **The schemas this module emits are not arbitrary.** They are produced by one function from
//!   one enum with six variants, so the keyword set is closed: `type`, `properties`,
//!   `required`, `additionalProperties`, `items` and `format`. A validator for a closed keyword
//!   set is a hundred lines; a validator for the whole specification is twenty thousand.
//! * **A validator that cannot run is not a validator.** The acceptance criterion says every
//!   sample must validate against its own schema, and the per-tick gate on a crowded box is
//!   `cargo test -p <the crate you touched> --quiet`. A check that needs a database, a compiled
//!   API binary or a network fetch is precisely the check that never runs.
//!
//! What the subset *does* enforce is stated in [`validate`], and the tests hold each keyword to
//! it. What it does not enforce is stated there too, so a reader never has to guess whether a
//! passing sample is a strong or a weak claim.

use serde_json::{Map, Value};

use crate::catalogue::{EventDefinition, Field, FieldKind};

/// The JSON Schema dialect the emitted documents declare.
///
/// 2020-12 rather than draft-07 because the keywords used here are identical in both and the
/// newer one is what a receiver's tooling will assume by default.
pub const DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// The `$id` every emitted schema carries.
///
/// A plain URN rather than an `https://` URL: a `$id` that looks fetchable invites a validator
/// to try to resolve it, and a request that leaves the machine over a name that is documentation
/// is a request nobody asked for. A URN says "this identifier is a name, not a place".
pub fn schema_id(event: &EventDefinition) -> String {
    let name = event.name;
    format!("urn:omnion:event:{name}:v1")
}

// ---------------------------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------------------------

/// The JSON Schema for one event's payload.
///
/// Two decisions the shape encodes:
///
/// * **`additionalProperties: false`.** A webhook receiver that switches on the payload's keys
///   has to be able to rely on the set being closed, and "the platform might add a field one
///   day" is exactly the sentence that makes a strict receiver impossible to write. Fields are
///   added by editing the row in `catalogue.rs`, which is a reviewed change, and the emitter
///   gate in `apps/api/tests/events.rs` refuses a payload that carries a field the row does not
///   declare — so a closed set is enforced, not merely intended.
/// * **A named envelope is NOT used.** The schema describes the payload object itself, because
///   the schema has to be something a receiver can paste into its own test fixture without first
///   learning what the platform wraps it in.
#[must_use]
pub fn schema_for(event: &EventDefinition) -> Value {
    let mut properties = Map::new();
    for field in event.payload_fields {
        properties.insert(field.name.to_owned(), field_schema(field));
    }

    let required: Vec<Value> = event
        .required_fields()
        .map(|field| Value::String(field.name.to_owned()))
        .collect();

    let mut schema = Map::new();
    schema.insert("$schema".to_owned(), Value::String(DIALECT.to_owned()));
    schema.insert("$id".to_owned(), Value::String(schema_id(event)));
    schema.insert("title".to_owned(), Value::String(event.name.to_owned()));
    schema.insert(
        "description".to_owned(),
        Value::String(event.description.to_owned()),
    );
    schema.insert("type".to_owned(), Value::String("object".to_owned()));
    schema.insert("properties".to_owned(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".to_owned(), Value::Array(required));
    }
    schema.insert("additionalProperties".to_owned(), Value::Bool(false));

    Value::Object(schema)
}

/// The schema for one field, as its `type` and `format` pair.
///
/// `format` is where the kind earns its keep: a receiver that validates in a language with a
/// UUID type should not have to be told what a `uuid` looks like, and `"format": "uuid"` is the
/// one keyword every language's validator already knows.
fn field_schema(field: &Field) -> Value {
    let (type_name, format) = match field.kind {
        // A UUID is a string with a format. There is deliberately no `type: "uuid"`: that is not
        // a JSON type, and a validator that does not recognise it answers "invalid type" for a
        // perfectly good payload.
        FieldKind::Uuid => ("string", Some("uuid")),
        FieldKind::String => ("string", None),
        FieldKind::Integer => ("integer", None),
        FieldKind::Boolean => ("boolean", None),
        FieldKind::Timestamp => ("string", Some("date-time")),
        // `Json` and `Any` are the two kinds that genuinely do not describe a type, because the
        // emitter does: a nested object whose fields belong to the module that builds it. An
        // object with no declared properties and `additionalProperties: true` is the honest
        // spelling of "some JSON, the shape is the emitter's business", and a receiver that
        // needs more should get a stricter row when the module ships.
        FieldKind::Json | FieldKind::Any => ("object", None),
    };

    let mut node = Map::new();
    node.insert("type".to_owned(), Value::String(type_name.to_owned()));
    if let Some(format) = format {
        node.insert("format".to_owned(), Value::String(format.to_owned()));
    }
    Value::Object(node)
}

// ---------------------------------------------------------------------------------------------
// Sample
// ---------------------------------------------------------------------------------------------

/// A payload that satisfies [`schema_for`] for this event.
///
/// The values are **placeholders, and they are obviously so**: a sample full of real-looking ids
/// is a sample somebody pastes into a production receiver and leaves there, and then the sample
/// is a fixture nobody believes any more. Every generated value therefore says what it is.
///
/// Optional fields are **included**. A sample that omits them cannot teach a receiver how to
/// parse the field that will eventually arrive, and "this payload is complete" is the whole
/// point of a sample — the required/optional distinction is already in the schema, which is
/// where a receiver reads it.
#[must_use]
pub fn sample_for(event: &EventDefinition) -> Value {
    let mut payload = Map::new();
    // Insertion order is the row's order, so the rendered sample reads in the same order as the
    // field table above it. `serde_json`'s `Map` preserves insertion order only when the
    // `preserve_order` feature is on; when it is not, the sample is still correct and merely
    // alphabetically ordered, which is why nothing here depends on the order.
    for field in event.payload_fields {
        payload.insert(field.name.to_owned(), sample_value(field, event.name));
    }
    Value::Object(payload)
}

/// One field's placeholder value.
fn sample_value(field: &Field, event: &str) -> Value {
    match field.kind {
        // A UUID-shaped placeholder, not a real one. It is a well-formed UUID so it passes the
        // `format` keyword, and it is obviously a placeholder so nobody ships it.
        FieldKind::Uuid => Value::String("00000000-0000-4000-8000-000000000000".to_owned()),
        FieldKind::String => Value::String(format!("<{} of {}>", field.name, event)),
        FieldKind::Integer => Value::from(0),
        FieldKind::Boolean => Value::Bool(false),
        // RFC 3339, so the `date-time` format check passes. The `T` separator and the `Z` offset
        // are both required by RFC 3339, and a sample written as `2026-01-01 12:00:00` would
        // fail its own schema — which is exactly the "sample that does not validate" bug this
        // module exists to prevent.
        FieldKind::Timestamp => Value::String("2026-01-01T12:00:00Z".to_owned()),
        FieldKind::Json | FieldKind::Any => Value::Object(Map::new()),
    }
}

// ---------------------------------------------------------------------------------------------
// The validator subset
// ---------------------------------------------------------------------------------------------

/// One thing wrong with a value, as a path and a sentence.
///
/// The path is a JSON pointer (`/health/service` for a nested object), because a receiver
/// author needs to know *which* field and a sentence that does not name it sends them looking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Where in the value it is, as a JSON pointer.
    pub path: String,
    /// What is wrong, in a sentence a person can act on.
    pub message: String,
}

/// Check `value` against `schema`, and return every violation.
///
/// Enforced, and nothing beyond it:
///
/// * `type` for the six JSON types the emitter produces, including that an **integer accepts a
///   whole number and rejects a fractional one** — `1.0` is accepted because JSON has one
///   number type and `1.0` is a whole number; `1.5` is refused because it is not.
/// * `format: "uuid"` and `format: "date-time"`, checked structurally rather than with a
///   date library: a UUID is `8-4-4-4-12` lower-case hex, and an RFC 3339 timestamp is a date, a
///   `T`, a time and an offset. This is deliberately not a calendar check — `2026-13-45T99:99:99Z`
///   passes, and saying so here is better than a validator that appears to be checking more
///   than it is.
/// * `properties`, `required` and `additionalProperties: false`.
/// * `items`, for the array case. The emitted schemas declare no arrays today; the keyword is
///   implemented so a future row that needs one does not have to grow the validator, and so
///   the "no arbitrary JSON in these documents" claim is checkable.
///
/// **Not** enforced, and deliberately: `enum`, `const`, `minimum`/`maximum`, `pattern`,
/// `anyOf`/`oneOf`/`allOf`, `not`, `if`/`then`/`else`, and `$ref`. Nothing this module emits
/// uses any of them, and a subset validator that accepted them would be lying about its reach
/// the moment somebody added a row that used one.
pub fn validate(schema: &Value, value: &Value) -> Vec<Violation> {
    let mut problems = Vec::new();
    check(schema, value, "", &mut problems);
    problems
}

/// `true` when the value satisfies the schema.
#[must_use]
pub fn is_valid(schema: &Value, value: &Value) -> bool {
    validate(schema, value).is_empty()
}

fn check(schema: &Value, value: &Value, path: &str, out: &mut Vec<Violation>) {
    // A boolean schema is legal JSON Schema: `true` accepts everything, `false` nothing. The
    // emitted documents never use one, and accepting them costs two lines while a validator
    // that answers "I cannot understand this schema" for a legal document is a trap.
    match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            out.push(Violation {
                path: path.to_owned(),
                message: "nothing is accepted here".to_owned(),
            });
            return;
        }
        Value::Object(_) => {}
        // A non-schema where a schema belongs is a bug in the *emitter*, not in the value. It is
        // reported rather than ignored so a malformed document is loud instead of permissive.
        _ => {
            out.push(Violation {
                path: path.to_owned(),
                message: "the schema node is not an object or a boolean".to_owned(),
            });
            return;
        }
    }

    let schema = schema.as_object().expect("checked above");

    if let Some(expected) = schema.get("type").and_then(Value::as_str) {
        if !type_matches(expected, value) {
            out.push(Violation {
                path: path.to_owned(),
                message: format!("expected {expected}, found {}", type_name(value)),
            });
            // The keyword this document then expects (`properties` on an object) would produce
            // a second, more confusing violation against the same path. One complaint per
            // problem is the whole value of the message list.
            return;
        }
    }

    if let Some(format) = schema.get("format").and_then(Value::as_str) {
        if let Some(message) = format_problem(format, value) {
            out.push(Violation {
                path: path.to_owned(),
                message,
            });
        }
    }

    match value {
        Value::Object(object) => {
            if let Some(Value::Object(properties)) = schema.get("properties") {
                for (name, subschema) in properties {
                    if let Some(field) = object.get(name) {
                        check(subschema, field, &child(path, name), out);
                    }
                }
            }
            if let Some(Value::Array(required)) = schema.get("required") {
                for name in required.iter().filter_map(Value::as_str) {
                    if !object.contains_key(name) {
                        out.push(Violation {
                            path: child(path, name),
                            message: format!("{name} is required and is not in the payload"),
                        });
                    }
                }
            }
            if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                let known = schema.get("properties").and_then(Value::as_object);
                for name in object.keys() {
                    let declared = known.is_some_and(|properties| properties.contains_key(name));
                    if !declared {
                        out.push(Violation {
                            path: child(path, name),
                            message: format!(
                                "{name} is not declared by this event; the schema closes the \
                                 payload's field set"
                            ),
                        });
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(subschema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    check(subschema, item, &format!("{path}/{index}"), out);
                }
            }
        }
        _ => {}
    }
}

/// A JSON pointer child. The leading `/` is part of the pointer, and the empty path is the root.
fn child(path: &str, name: &str) -> String {
    if path.is_empty() {
        format!("/{name}")
    } else {
        format!("{path}/{name}")
    }
}

/// `true` when the value is of the named JSON type.
///
/// The integer case is the one worth reading twice, and the reason it is not `is_number()` is
/// a bug this tick's own test found. JSON has exactly one number type, so `1.0` is a *whole
/// number* and `"type": "integer"` accepts it — that is what the JSON Schema specification says
/// about a number with a zero fractional part, and a receiver validating in JavaScript sees the
/// same. Checking `is_i64()` alone refuses `1.0`, which makes the platform's own samples and
/// every payload a JavaScript client serialises fail a schema that says they are integers.
/// What must be refused is `1.5`: a fractional part that rounds is silent data loss, and the
/// receiver only discovers it in production.
fn type_matches(expected: &str, value: &Value) -> bool {
    match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "integer" => {
            value.is_i64()
                || value.is_u64()
                // `serde_json` parses `1.0` into an f64, so the whole-number case has to be
                // asked for explicitly. `is_finite` first: JSON has no `Infinity`, but a value
                // that arrived from a non-JSON producer could, and `NaN.fract()` is `NaN` — a
                // comparison that is false for the right reason and a panic for the wrong one.
                || value
                    .as_f64()
                    .is_some_and(|number| number.is_finite() && number.fract() == 0.0)
        }
        "number" => value.is_number(),
        _ => false,
    }
}

/// The name a value's own type has, for the message.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Check one `format`, structurally. `None` means it passed.
fn format_problem(format: &str, value: &Value) -> Option<String> {
    let text = value.as_str()?;
    match format {
        "uuid" if !is_uuid_shaped(text) => Some(format!(
            "expected a UUID (8-4-4-4-12 lower-case hex), found {text:?}"
        )),
        "date-time" if !is_rfc3339_shaped(text) => Some(format!(
            "expected an RFC 3339 timestamp such as 2026-01-01T12:00:00Z, found {text:?}"
        )),
        // A format this subset does not check is not a failure. Refusing it would make the
        // validator reject a document it simply has not grown up to yet, which is the opposite
        // of what a "no unexpected keys" claim should do.
        _ => None,
    }
}

/// `8-4-4-4-12` and nothing else.
///
/// Upper-case hex is refused: RFC 4122 permits it and every Rust and JavaScript UUID library
/// emits the lower-case form, so a payload that differs is one a strict receiver downstream will
/// reject, and a sample that teaches the wrong case is a sample that costs somebody an evening.
fn is_uuid_shaped(text: &str) -> bool {
    let groups: Vec<&str> = text.split('-').collect();
    if groups.len() != 5 {
        return false;
    }
    const WIDTHS: [usize; 5] = [8, 4, 4, 4, 12];
    groups.iter().zip(WIDTHS).all(|(group, width)| {
        group.len() == width
            && group
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    })
}

/// RFC 3339's shape, not its calendar: `YYYY-MM-DDThh:mm:ss[.frac](Z|±hh:mm)`.
///
/// The lower-case `t`/`z` are refused because RFC 3339 permits them and JSON Schema's
/// `date-time` format follows RFC 3339 — but a *sample* should show the form every parser
/// accepts without thinking, which is the upper-case one.
///
/// **The offset is checked, not skipped.** An earlier version accepted `Z` and refused
/// `+03:00`, which is backwards in the one way that matters: a Turkish deployment, or any
/// receiver in a non-UTC zone, is exactly the reader who writes `+03:00`, and the schema they
/// were handed refused their own timestamp. The offset's own shape is checked too — sign, five
/// characters, digits in the right places — because a "something at the end" tail would have
/// accepted `Zgarbage`.
fn is_rfc3339_shaped(text: &str) -> bool {
    let bytes = text.as_bytes();
    // Shortest legal form is `2026-01-01T00:00:00Z` — 20 characters.
    if bytes.len() < 20 {
        return false;
    }
    let digits_at =
        |positions: &[usize]| positions.iter().all(|index| bytes[*index].is_ascii_digit());
    let head_is_right = digits_at(&[0, 1, 2, 3])
        && bytes[4] == b'-'
        && digits_at(&[5, 6])
        && bytes[7] == b'-'
        && digits_at(&[8, 9])
        && bytes[10] == b'T'
        && digits_at(&[11, 12])
        && bytes[13] == b':'
        && digits_at(&[14, 15])
        && bytes[16] == b':'
        && digits_at(&[17, 18]);
    if !head_is_right {
        return false;
    }

    match bytes[19] {
        b'Z' => bytes.len() == 20,
        b'.' => {
            // A fractional part of at least one digit, then the offset.
            let rest = &text[20..];
            let Some((index, _)) = rest
                .char_indices()
                .find(|(_, character)| *character == 'Z' || *character == '+' || *character == '-')
            else {
                return false;
            };
            if index == 0 || !rest[..index].bytes().all(|byte| byte.is_ascii_digit()) {
                return false;
            }
            offset_is_shaped(&rest[index..])
        }
        // Anything that is not `Z` or a fraction has to be an offset itself.
        _ => offset_is_shaped(&text[19..]),
    }
}

/// `Z`, or `±hh:mm`.
///
/// A bare `-` cannot be an offset on its own, which is why the length is six: this function
/// is only ever handed what follows the seconds, so a `-` here is a sign, never a date hyphen.
fn offset_is_shaped(text: &str) -> bool {
    if text == "Z" {
        return true;
    }
    let bytes = text.as_bytes();
    bytes.len() == 6
        && (bytes[0] == b'+' || bytes[0] == b'-')
        && bytes[1].is_ascii_digit()
        && bytes[2].is_ascii_digit()
        && bytes[3] == b':'
        && bytes[4].is_ascii_digit()
        && bytes[5].is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogue::CATALOGUE;

    /// The invariant the whole module exists to keep.
    ///
    /// Every row in the table, without exception: its sample must satisfy its own schema. A
    /// test that picked a few names would prove those names and leave the other sixty-two to be
    /// discovered by a subscriber, which is the exact failure this file was written to prevent.
    #[test]
    fn every_catalogue_sample_validates_against_its_own_schema() {
        let mut checked = 0_usize;
        for entry in CATALOGUE {
            let schema = schema_for(entry);
            let sample = sample_for(entry);
            let problems = validate(&schema, &sample);
            assert!(
                problems.is_empty(),
                "the sample for {} does not validate against its own schema: {problems:#?}\n\
                 schema: {schema}\nsample: {sample}",
                entry.name
            );
            checked += 1;
        }
        assert!(
            checked >= 60,
            "the walk checked {checked} names; a walk that sees almost nothing proves nothing"
        );
    }

    /// The schema is a *contract*, so it has to say the field set is closed.
    ///
    /// Without `additionalProperties: false` a receiver cannot tell a typo'd field name from a
    /// field the platform has not shipped, and the platform's answer to "will this change?" is
    /// always "not without a review", which is what the keyword encodes.
    #[test]
    fn the_payload_field_set_is_closed() {
        for entry in CATALOGUE {
            let schema = schema_for(entry);
            assert_eq!(
                schema.get("additionalProperties"),
                Some(&Value::Bool(false)),
                "{} must close its payload's field set",
                entry.name
            );
        }
    }

    /// The schema and the sample come out of the same row, and this holds them to it.
    ///
    /// A hand-written sample per event would be the obvious way to build this and the wrong
    /// one: it drifts, and the drift is invisible because both halves are still individually
    /// plausible. Instead every declared field is checked on both sides.
    #[test]
    fn the_schema_declares_exactly_the_fields_the_row_declares() {
        for entry in CATALOGUE {
            let schema = schema_for(entry);
            let properties = schema
                .get("properties")
                .and_then(Value::as_object)
                .unwrap_or_else(|| panic!("{} must declare properties", entry.name));
            let sample = sample_for(entry);
            let sample_object = sample
                .as_object()
                .unwrap_or_else(|| panic!("{} must produce an object sample", entry.name));

            assert_eq!(
                properties.len(),
                entry.payload_fields.len(),
                "{}: the schema declares {} properties for {} fields",
                entry.name,
                properties.len(),
                entry.payload_fields.len()
            );
            for field in entry.payload_fields {
                assert!(
                    properties.contains_key(field.name),
                    "{}: the schema omits {}",
                    entry.name,
                    field.name
                );
                assert!(
                    sample_object.contains_key(field.name),
                    "{}: the sample omits {}",
                    entry.name,
                    field.name
                );
            }
        }
    }

    /// A sample is copied into a receiver, so its values have to be recognisable as placeholders.
    ///
    /// The exception is stated rather than left implied: the UUID is a well-formed all-zero value
    /// so it satisfies the `format` keyword, and it is still unmistakably not a real id.
    #[test]
    fn a_sample_carries_placeholders_and_never_a_value_that_looks_real() {
        let published = crate::catalogue::lookup("page.published").expect("listed");
        let sample = sample_for(published);
        let text = serde_json::to_string(&sample).expect("serialises");

        assert!(
            text.contains("<slug of page.published>"),
            "a string field carries a value that names itself: {text}"
        );
        assert!(
            !text.contains("00000000-0000-4000-8000-000000000001"),
            "the UUID placeholder must not look like a sequence a receiver would count up"
        );
    }

    // ---- The validator subset, one keyword at a time ------------------------------------------------

    /// A payload missing a required field is refused, by name.
    #[test]
    fn a_missing_required_field_is_named() {
        let schema = schema_for(crate::catalogue::lookup("page.published").expect("listed"));
        let problems = validate(&schema, &json!({}));
        assert!(
            problems
                .iter()
                .any(|problem| problem.message.contains("slug") && problem.path == "/slug"),
            "the refusal names the field and its path: {problems:#?}"
        );
    }

    /// A payload carrying a field the row does not declare is refused — the closed set.
    #[test]
    fn an_undeclared_field_is_refused() {
        let schema = schema_for(crate::catalogue::lookup("page.published").expect("listed"));
        let problems = validate(&schema, &json!({ "not_a_field": 1 }));
        assert!(
            problems
                .iter()
                .any(|problem| problem.path == "/not_a_field"),
            "an undeclared field is refused with its path: {problems:#?}"
        );
    }

    /// A whole number is an integer; a fractional one is not.
    ///
    /// JSON has one number type, so `1.0` is a whole number and `1.5` is not — and a validator
    /// that used `is_number()` would accept both and then round, which is a data loss a receiver
    /// only discovers in production.
    #[test]
    fn an_integer_is_a_whole_number_and_nothing_else() {
        let schema = json!({ "type": "integer" });
        assert!(is_valid(&schema, &json!(1)));
        assert!(is_valid(&schema, &json!(1.0)));
        assert!(!is_valid(&schema, &json!(1.5)));
        assert!(!is_valid(&schema, &json!("1")));
    }

    /// A uuid must be shaped like one, and the case matters downstream.
    #[test]
    fn the_uuid_format_is_structural_and_lower_case() {
        let schema = json!({ "type": "string", "format": "uuid" });
        assert!(is_valid(
            &schema,
            &json!("00000000-0000-4000-8000-000000000000")
        ));
        assert!(!is_valid(
            &schema,
            &json!("00000000-0000-4000-8000-00000000")
        ));
        assert!(
            !is_valid(&schema, &json!("00000000-0000-4000-8000-00000000000G")),
            "a non-hex character is not a uuid"
        );
        assert!(
            !is_valid(&schema, &json!("00000000-0000-4000-8000-00000000000A")),
            "upper-case hex is refused: every UUID library emits the lower-case form, and a \
             sample that teaches the other one costs a receiver an evening"
        );
    }

    /// The date-time format is the shape RFC 3339 requires, not a calendar check.
    ///
    /// The offset cases are here because a version of this validator got them backwards, and
    /// backwards in the one direction that reaches a real user: `Z` accepted, `+03:00` refused.
    #[test]
    fn the_date_time_format_is_structural() {
        let schema = json!({ "type": "string", "format": "date-time" });
        assert!(is_valid(&schema, &json!("2026-01-01T12:00:00Z")));
        assert!(is_valid(&schema, &json!("2026-01-01T12:00:00.123Z")));
        assert!(is_valid(&schema, &json!("2026-01-01T12:00:00+03:00")));
        assert!(
            is_valid(&schema, &json!("2026-01-01T12:00:00.123+03:00")),
            "a fractional part and an offset combine"
        );
        assert!(
            is_valid(&schema, &json!("2026-01-01T12:00:00-05:00")),
            "a negative offset is an offset, not a date hyphen"
        );
        assert!(!is_valid(&schema, &json!("2026-01-01 12:00:00Z")));
        assert!(!is_valid(&schema, &json!("2026-01-01T12:00:00")));
        assert!(!is_valid(&schema, &json!("2026-01-01")));
    }

    /// A timestamp that is *nearly* right is refused, and each way of being nearly right is
    /// named: a trailing `Z` with junk behind it, an offset with too few characters, a fraction
    /// with no digits.
    ///
    /// A "starts with something date-ish" check would pass all six of these, and a receiver
    /// validating a real delivery with one is exactly the case this format exists for.
    #[test]
    fn a_timestamp_that_is_almost_right_is_refused() {
        let schema = json!({ "type": "string", "format": "date-time" });
        for bad in [
            "2026-01-01T12:00:00Zgarbage",
            "2026-01-01T12:00:00+3:00",
            "2026-01-01T12:00:00+03:0",
            "2026-01-01T12:00:00.Z",
            "2026-01-01T12:00:00.",
            "2026-01-01T12:00:00",
            "2026-01-01T12:00:00+",
        ] {
            assert!(
                !is_valid(&schema, &json!(bad)),
                "{bad} is not RFC 3339 and must be refused"
            );
        }
    }

    /// A nested violation carries the path that leads to it.
    #[test]
    fn a_nested_problem_carries_its_path() {
        let schema = json!({
            "type": "object",
            "properties": {
                "health": {
                    "type": "object",
                    "properties": { "service": { "type": "string" } },
                    "required": ["service"]
                }
            },
            "required": ["health"]
        });
        let problems = validate(&schema, &json!({ "health": {} }));
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert_eq!(problems[0].path, "/health/service");
    }

    /// An array's items are checked, and the path carries the index.
    #[test]
    fn array_items_are_checked_by_index() {
        let schema = json!({
            "type": "object",
            "properties": { "names": { "type": "array", "items": { "type": "string" } } }
        });
        let problems = validate(&schema, &json!({ "names": ["ok", 7] }));
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert_eq!(problems[0].path, "/names/1");
    }

    /// A format this subset does not check is not a failure.
    ///
    /// A validator that refused what it cannot judge would reject every future document the
    /// moment a row used a keyword it had not grown into, and the fix would be to delete the
    /// check — which is how a "nothing leaks" assertion gets switched off.
    #[test]
    fn an_unknown_format_is_not_a_failure() {
        assert!(is_valid(
            &json!({ "type": "string", "format": "email" }),
            &json!("not-an-email")
        ));
    }

    /// A schema that is not a schema is reported rather than ignored.
    ///
    /// Permissiveness here would be the worst possible failure mode: a malformed document would
    /// validate everything, and the samples would "pass" against a document that constrains
    /// nothing.
    #[test]
    fn a_malformed_schema_is_loud_rather_than_permissive() {
        let problems = validate(&json!("not a schema"), &json!({}));
        assert_eq!(problems.len(), 1);
        assert!(problems[0].message.contains("not an object or a boolean"));
    }

    /// Boolean schemas are legal and mean what they say.
    #[test]
    fn a_boolean_schema_is_honoured() {
        assert!(is_valid(&json!(true), &json!({ "anything": 1 })));
        assert!(!is_valid(&json!(false), &json!({})));
    }
}
