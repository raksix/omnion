//! The custom key/value pairs an editor hangs on a file (`media.metadata`).
//!
//! # The column exists and nothing ever used it
//!
//! `0025_file_manager.sql` gave every media row a `metadata jsonb` column and a GIN index over
//! it, and `UpdateFileBody` has carried a `metadata` field since the first draft of the file
//! route — and a search of the tree found **no writer and no reader**. Every row in every
//! installation was `{}`, so the index was scanning an empty object per row and the REQ's own
//! scope ("custom key/value pairs … with a GIN index", "a metadata filter in the browser") was
//! satisfied by a column. It is the uncalled-column defect class one level up from the uncalled
//! `prune_candidates` function: a promise in the schema that no code path keeps.
//!
//! # Why the values are text and not arbitrary JSON
//!
//! A `jsonb` column can hold anything, and "anything" is the problem. A nested object written
//! by one panel release is read by no filter, so it is invisible until somebody hand-writes a
//! query for it; an array has no single meaning for `metadata @> '{"k":"v"}'`, so the containment
//! test that the GIN index serves silently returns false for it; and a number or a boolean makes
//! `metadata @> '{"campaign":"2026"}'` false for a value that *is* 2026 as a number.
//! [`MetadataPairs::parse`] therefore reduces an operator's input to a flat string-to-string map:
//! everything written is searchable with the same one operator, and everything written is
//! comparable with `=` when the panel renders the row.
//!
//! The cap matters more than it looks. The column has **no size limit**: `jsonb` will hold
//! several megabytes, and a panel that accepts an unbounded editor's paste turns one field into
//! an unbounded row. [`MAX_PAIRS`] and [`MAX_VALUE_LENGTH`] are the ceiling, and they are
//! refused with the *key* named rather than truncated, because a truncated license number is
//! still a license number on the screen.

use serde_json::{Map, Value};

use crate::error::{MediaError, Result};

/// How many pairs one file may carry.
pub const MAX_PAIRS: usize = 40;

/// The longest key, in bytes.
pub const MAX_KEY_LENGTH: usize = 60;

/// The longest value, in bytes.
pub const MAX_VALUE_LENGTH: usize = 500;

/// The longest total, in bytes, across every key and value of one file.
///
/// [`MAX_PAIRS`] times [`MAX_VALUE_LENGTH`] is 20 kB on its own, and forty 500-character values
/// is a row that every listing, every duplicate scan and every API key response drags along.
pub const MAX_TOTAL_LENGTH: usize = 8_000;

/// A file's custom pairs, validated.
///
/// This is the only shape a pair may have. It is a `BTreeMap` rather than a `HashMap` so a
/// serialized row is byte-stable: two saves of the same pairs produce the same stored jsonb,
/// which is what makes "nothing changed" detectable and keeps the API response order stable
/// between two reads of an unchanged row.
pub type MetadataPairs = std::collections::BTreeMap<String, String>;

/// Turn a caller-supplied object into validated pairs.
///
/// Accepts a JSON object whose values are strings, numbers or booleans, and **refuses** anything
/// else — a nested object or an array is a `400` naming the key, never silently stringified.
///
/// The refusal is the point. `[object Object]` and `1,2,3` are the two strings a naive
/// `to_string()` produces, and both are indistinguishable from a value an operator typed: the
/// row reads back as populated, the filter finds it, and the structure that a future release
/// would want to interpret has been flattened into prose. Silently coercing is how a column ends
/// up holding a thing nobody can query.
pub fn parse(raw: &Value) -> Result<MetadataPairs> {
    let object = match raw {
        Value::Object(map) => map,
        Value::Null => return Ok(MetadataPairs::new()),
        other => {
            return Err(MediaError::InvalidMetadata {
                field: "metadata".to_owned(),
                reason: format!(
                    "expected an object of key/value pairs, found {}",
                    json_kind(other)
                ),
            });
        }
    };

    let mut pairs = MetadataPairs::new();
    let mut total = 0usize;

    for (key, value) in object {
        let key = key.trim();
        if key.is_empty() {
            return Err(MediaError::InvalidMetadata {
                field: "metadata".to_owned(),
                reason: "a key cannot be blank".to_owned(),
            });
        }
        if key.len() > MAX_KEY_LENGTH {
            return Err(MediaError::InvalidMetadata {
                field: format!("metadata.{key}"),
                reason: format!("the key is {key:?} is longer than {MAX_KEY_LENGTH} bytes"),
            });
        }
        // A key that is only punctuation is a blank row in the editor that an operator cannot
        // see and cannot delete, because the editor hides the keys it does not understand.
        if !key
            .chars()
            .any(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
        {
            return Err(MediaError::InvalidMetadata {
                field: format!("metadata.{key}"),
                reason: "a key needs at least one letter, digit, `_`, `-` or `.`".to_owned(),
            });
        }

        let text = scalar_text(value).ok_or_else(|| MediaError::InvalidMetadata {
            field: format!("metadata.{key}"),
            reason: format!(
                "the value must be text, a number or a boolean — found {}",
                json_kind(value)
            ),
        })?;

        if text.len() > MAX_VALUE_LENGTH {
            return Err(MediaError::InvalidMetadata {
                field: format!("metadata.{key}"),
                reason: format!(
                    "the value of {key:?} is {} bytes; the longest accepted value is \
                     {MAX_VALUE_LENGTH}",
                    text.len()
                ),
            });
        }

        total += key.len() + text.len();
        if total > MAX_TOTAL_LENGTH {
            return Err(MediaError::InvalidMetadata {
                field: "metadata".to_owned(),
                reason: format!(
                    "the pairs together are over the {MAX_TOTAL_LENGTH} byte limit; remove one \
                     or shorten a value"
                ),
            });
        }

        if let Some(existing) = pairs.insert(key.to_owned(), text) {
            // Two keys that differ only by surrounding whitespace are the same key, and the last
            // one written would win — which is not an error a person can see.
            return Err(MediaError::InvalidMetadata {
                field: format!("metadata.{key}"),
                reason: format!("the key {key:?} is already set (as {existing:?})"),
            });
        }
    }

    if pairs.len() > MAX_PAIRS {
        return Err(MediaError::InvalidMetadata {
            field: "metadata".to_owned(),
            reason: format!(
                "a file carries at most {MAX_PAIRS} pairs; {} were sent",
                pairs.len()
            ),
        });
    }

    Ok(pairs)
}

/// The text a scalar jsonb value carries, or `None` for a container.
///
/// A number is rendered the way an operator would read it, and a whole float is rendered as the
/// integer it is: `serde_json` prints `1.0` for `1.0`, so an editor who typed `1` would read `1.0`
/// back and conclude the value was mangled — and a filter for `year=1` would match nothing. The
/// test caught this on `main` before it was ever written: the *first* draft of this function used
/// `number.to_string()` and the round-trip test failed with `"1.0"` where `"1"` was expected.
fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => {
            if let Some(whole) = number.as_i64() {
                // An integer, however it arrived (a `1`, a `1.0`, an exponent that resolves to a
                // whole number), is stored as the digits a person typed.
                Some(whole.to_string())
            } else if let Some(float) = number.as_f64() {
                if float.fract() == 0.0 && float.abs() < 9.007_199_254_740_992e15 {
                    Some(format!("{}", float as i64))
                } else {
                    Some(float.to_string())
                }
            } else {
                Some(number.to_string())
            }
        }
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// The jsonb type of a value, in the words a person reading the message knows.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "text",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

/// Serialize validated pairs into the jsonb the row stores.
///
/// Built from a [`Map`] rather than `serde_json::to_value` so a key that is not valid unicode in
/// some future caller cannot fail the *write* after the validation passed.
#[must_use]
pub fn to_json(pairs: &MetadataPairs) -> Value {
    let mut map = Map::with_capacity(pairs.len());
    for (key, value) in pairs {
        map.insert(key.clone(), Value::String(value.clone()));
    }
    Value::Object(map)
}

/// Read a stored row back into pairs, dropping nothing and inventing nothing.
///
/// A row written before this release is `{}`, and one written by a future release with a nested
/// value comes back as the empty map rather than as a crash: the *editor* is where a shape this
/// release cannot represent is refused, not the detail screen that has to render whatever the
/// database holds.
#[must_use]
pub fn from_json(raw: &Value) -> MetadataPairs {
    let mut pairs = MetadataPairs::new();
    let Some(object) = raw.as_object() else {
        return pairs;
    };
    for (key, value) in object {
        if let Some(text) = scalar_text(value) {
            pairs.insert(key.clone(), text);
        }
    }
    pairs
}

/// The one clause a metadata filter writes, and the value it binds.
///
/// Returns `None` for a term that is not a filter at all — blank, or a `key` with no `value` —
/// so a half-filled field narrows nothing instead of matching everything or nothing at random.
#[must_use]
pub fn filter_clause(term: &str) -> Option<(String, String)> {
    let (key, value) = term.split_once('=')?;
    let key = key.trim();
    let value = value.trim();
    if key.is_empty() || value.is_empty() {
        return None;
    }
    if key.len() > MAX_KEY_LENGTH || value.len() > MAX_VALUE_LENGTH {
        return None;
    }
    Some((key.to_owned(), value.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_empty_object_is_an_empty_set_and_not_an_error() {
        // `{}` is what every row written before this release holds, so it must keep parsing.
        assert!(parse(&json!({})).expect("empty parses").is_empty());
        assert!(parse(&Value::Null).expect("null parses").is_empty());
    }

    #[test]
    fn strings_numbers_and_booleans_all_survive_a_round_trip() {
        let pairs = parse(&json!({
            "campaign": "spring-2026",
            "licence": "CC-BY-4.0",
            "count": 12,
            "approved": true,
            "ratio": 1.5,
        }))
        .expect("scalars parse");
        assert_eq!(pairs["campaign"], "spring-2026");
        assert_eq!(pairs["count"], "12");
        assert_eq!(pairs["approved"], "true");
        assert_eq!(pairs["ratio"], "1.5");
        // A save and a re-read must produce the same row, or every save looks like a change.
        let stored = to_json(&pairs);
        assert_eq!(from_json(&stored), pairs);
        assert_eq!(to_json(&parse(&stored).expect("reparses")), stored);
    }

    #[test]
    fn a_whole_float_comes_back_as_the_number_the_operator_typed() {
        // `serde_json` prints `1.0` for a whole f64. An editor who typed `1` must not read
        // `1.0` back and conclude the value was mangled.
        let pairs = parse(&json!({ "year": 1.0 })).expect("float parses");
        assert_eq!(pairs["year"], "1");
    }

    #[test]
    fn a_nested_object_is_refused_and_names_the_key() {
        let error = parse(&json!({ "shoot": { "lens": "50mm" } })).expect_err("nested refused");
        let message = error.to_string();
        assert!(message.contains("metadata.shoot"), "{message}");
        assert!(message.contains("an object"), "{message}");
        // The flattening this release refuses: `[object Object]` would have been searchable and
        // uninterpretable at the same time.
        assert!(!message.contains("object Object"), "{message}");
    }

    #[test]
    fn a_list_is_refused_and_names_the_key() {
        let error = parse(&json!({ "tags": ["a", "b"] })).expect_err("list refused");
        let message = error.to_string();
        assert!(message.contains("metadata.tags"), "{message}");
        assert!(message.contains("a list"), "{message}");
    }

    #[test]
    fn a_null_value_is_refused_rather_than_saved_as_the_word_null() {
        // A cleared input sends `null`. Storing the string "null" makes a pair that can never be
        // emptied, and filtering for it returns the rows nobody meant to tag.
        let error = parse(&json!({ "campaign": Value::Null })).expect_err("null refused");
        assert!(error.to_string().contains("metadata.campaign"), "{error}");
    }

    #[test]
    fn a_blank_key_is_refused() {
        let error = parse(&json!({ "   ": "x" })).expect_err("blank key refused");
        assert!(error.to_string().contains("cannot be blank"), "{error}");
    }

    #[test]
    fn a_punctuation_only_key_is_refused() {
        // The editor hides keys it does not recognise, so a row of `!!!` is invisible and
        // undeletable from the panel.
        let error = parse(&json!({ "***": "x" })).expect_err("punctuation key refused");
        assert!(error.to_string().contains("metadata.***"), "{error}");
    }

    #[test]
    fn an_over_long_key_is_refused_and_not_truncated() {
        let long = "k".repeat(MAX_KEY_LENGTH + 1);
        let error = parse(&json!({ long.clone(): "x" })).expect_err("long key refused");
        assert!(error.to_string().contains("longer than"), "{error}");
    }

    #[test]
    fn an_over_long_value_is_refused_with_the_length_it_found() {
        let long = "v".repeat(MAX_VALUE_LENGTH + 7);
        let error = parse(&json!({ "licence": long.clone() })).expect_err("long value refused");
        let message = error.to_string();
        assert!(message.contains("metadata.licence"), "{message}");
        assert!(
            message.contains(&(MAX_VALUE_LENGTH + 7).to_string()),
            "{message}"
        );
    }

    #[test]
    fn too_many_pairs_is_refused_with_the_number_sent() {
        let mut object = Map::new();
        for index in 0..=MAX_PAIRS {
            object.insert(format!("k{index}"), json!("v"));
        }
        let error = parse(&Value::Object(object)).expect_err("too many refused");
        let message = error.to_string();
        assert!(message.contains("at most 40"), "{message}");
        assert!(message.contains("41 were sent"), "{message}");
    }

    #[test]
    fn the_total_size_is_capped_even_when_every_pair_is_legal() {
        // 40 pairs of 500 bytes are each individually legal and together are 20 kB, so the
        // per-value cap alone lets an unbounded row through.
        let big = "v".repeat(MAX_VALUE_LENGTH);
        let mut object = Map::new();
        for index in 0..MAX_PAIRS {
            object.insert(format!("key{index}"), json!(big));
        }
        let error = parse(&Value::Object(object)).expect_err("total refused");
        assert!(error.to_string().contains("byte limit"), "{error}");
    }

    #[test]
    fn two_keys_that_differ_only_by_whitespace_are_refused_not_merged() {
        // A map would silently keep the last one. The caller sent two keys and gets told so.
        let error = parse(&json!({ " campaign": "a", "campaign": "b" })).expect_err("dup refused");
        assert!(error.to_string().contains("already set"), "{error}");
    }

    #[test]
    fn a_top_level_value_that_is_not_an_object_names_what_it_found() {
        let error = parse(&json!([1, 2, 3])).expect_err("array refused");
        assert!(error.to_string().contains("a list"), "{error}");
    }

    #[test]
    fn every_stored_value_is_text_whatever_arrived() {
        // The deliberate shape: a number arrives as a number and is stored as its digits, so the
        // one filter operator (`=`) works on every row regardless of how the value was typed. My
        // first draft of this test asserted the jsonb came back *type for type* and failed with
        // `Number(4)` against `String("4")` — the code was right and the test was wrong: a
        // column that stores three json types is a column with three comparison rules.
        let pairs =
            parse(&json!({ "campaign": "spring", "licence": "CC0", "pages": 4 })).expect("parses");
        assert_eq!(
            to_json(&pairs),
            json!({ "campaign": "spring", "licence": "CC0", "pages": "4" })
        );
    }

    #[test]
    fn a_stored_row_this_release_cannot_represent_reads_as_empty_not_as_a_crash() {
        // A row written by a future release, or by a direct SQL insert, must still render.
        let stored = json!({ "campaign": "spring", "shoot": { "lens": "50mm" } });
        let pairs = from_json(&stored);
        assert_eq!(
            pairs.len(),
            1,
            "the pair this release understands survives: {pairs:?}"
        );
        assert_eq!(pairs["campaign"], "spring");
    }

    #[test]
    fn a_filter_term_splits_into_a_key_and_a_value() {
        assert_eq!(
            filter_clause("campaign=spring-2026"),
            Some(("campaign".to_owned(), "spring-2026".to_owned()))
        );
        assert_eq!(
            filter_clause("  licence = CC-BY-4.0  "),
            Some(("licence".to_owned(), "CC-BY-4.0".to_owned()))
        );
    }

    #[test]
    fn a_half_filled_filter_term_is_not_a_filter() {
        // A field with only a key typed narrows nothing. Treating it as "key exists" would show
        // a result the operator did not ask for; treating it as an error would nag on every
        // keystroke. It is neither until the term is finished.
        assert_eq!(filter_clause(""), None);
        assert_eq!(filter_clause("campaign"), None);
        assert_eq!(filter_clause("campaign="), None);
        assert_eq!(filter_clause("=spring"), None);
        assert_eq!(filter_clause("   =   "), None);
    }

    #[test]
    fn a_filter_term_longer_than_the_column_is_not_a_filter() {
        let long = "v".repeat(MAX_VALUE_LENGTH + 1);
        assert_eq!(filter_clause(&format!("k={long}")), None);
        let long_key = "k".repeat(MAX_KEY_LENGTH + 1);
        assert_eq!(filter_clause(&format!("{long_key}=v")), None);
    }

    #[test]
    fn an_equals_inside_a_value_is_kept() {
        // Splitting on the *first* `=` keeps `note=width=3px` addressable.
        assert_eq!(
            filter_clause("note=width=3px"),
            Some(("note".to_owned(), "width=3px".to_owned()))
        );
    }
}
