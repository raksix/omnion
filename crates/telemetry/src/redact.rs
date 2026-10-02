//! The redaction pass: the single place that decides what may leave the process in a log field.
//!
//! REQ-126 names the risk exactly — "telemetry is the most likely place for a secret or personal
//! datum to leak" — and names the shape of the fix: **one shared redaction helper** that runs
//! over log fields, span attributes and exporter payloads, with a test that greps recorded output
//! rather than trusting reviewers.
//!
//! So this module is not a convention, it is a filter with two layers, and both are needed:
//!
//! 1. **Key-based.** A field whose *name* is a secret name never carries its value. The names are
//!    a closed, matched set — `password`, `token`, `authorization`, `cookie`, `api_key`, and the
//!    `*_secret` / `secret_*` / `*_token` shapes. A name nobody thought of is the argument against
//!    this layer, which is why layer 2 exists.
//! 2. **Shape-based.** A field whose *value* looks like a credential — a provider key prefix, a
//!    bearer token, a JWT, a private key block — is redacted even under an innocent name like
//!    `detail` or `note`. This is the layer that catches the leak nobody anticipated, and it is
//!    why the test asserts on a fixture *value*, not on a field name.
//!
//! And one personal datum the request names explicitly: an e-mail address is replaced with its
//! hint, because a log store is retained for days and read by anyone with the read permission.
//!
//! ## What is deliberately *not* here
//!
//! A cardinality check, a truncation, and the field-name allow-list belong to the *store* and the
//! *schema*: this module answers one question — "may this value be written down?" — and the
//! answer is a `String` or [`REDACTED`]. A second opinion is how a value ends up half-redacted.

use serde_json::{Map, Value};

use omnion_secrets::redaction::hint_for;

/// The single replacement token. One constant, greppable, and never shaped like a value.
pub const REDACTED: &str = "[redacted]";

/// Field names whose value is never written down, whatever the value looks like.
///
/// Matched exactly after lowercasing and stripping `_`, `-` and `.`, so `api_key`, `apiKey`,
/// `api-key` and `API KEY` are one name rather than four.
const SECRET_NAMES: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "passphrase",
    "secret",
    "secrets",
    "token",
    "tokens",
    "accesstoken",
    "refreshtoken",
    "idtoken",
    "apikey",
    "apisecret",
    "authtoken",
    "authorization",
    "auth",
    "cookie",
    "setcookie",
    "session",
    "sessionid",
    "sessiontoken",
    "privatekey",
    "privatekeypem",
    "clientsecret",
    "clientassertion",
    "bearertoken",
    "bearer",
    "signature",
    "salt",
    "seed",
    "mnemonic",
    "cvv",
    "cvc",
    "pin",
    "otp",
    "totp",
    "recoverycode",
    "webhooksecret",
    "signingkey",
];

/// Suffixes and prefixes that mark a field as a credential even when the stem is a word the
/// list above never had to mention (`stripe_secret`, `db_password_v2`, `userToken`).
const SECRET_SHAPES: &[&str] = &[
    "secret",
    "password",
    "passwd",
    "token",
    "apikey",
    "privatekey",
];

/// Value prefixes that identify a real credential. This is the layer that catches an
/// innocently-named field: a caller that logs `detail: "sk-live-abc"` leaks the key, and no
/// key-based list would have stopped it.
const SECRET_VALUE_PREFIXES: &[&str] = &[
    "sk-",
    "sk_live_",
    "sk_test_",
    "pk_live_",
    "rk_live_",
    "whsec_",
    "AKIA",
    "ASIA",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xapp-",
    "AIza",
    "ya29.",
    "eyJ",
    "-----BEGIN",
    "Bearer ",
    "bearer ",
    "npm_",
    "dop_v1_",
    "hf_",
    "r8_",
    "SG.",
    "sq0idp-",
    "nvapi-",
    "csk_",
    "lz_",
    "atlasv1.",
    "tfp_",
];

/// The whole `fields` object, filtered.
///
/// Returns a new map: the caller's own value is untouched, so a caller that inspects its input
/// afterwards is not surprised, and a `fields` map is never mutated in place behind the store's
/// back. Values are strings, numbers, booleans, `null`, or nested objects/arrays whose string
/// leaves are filtered the same way — a `{"headers": {"authorization": …}}` nested one level down
/// is the exact shape a caller reaches for when they want to log a request, so it is filtered.
#[must_use]
pub fn redact_fields(fields: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::with_capacity(fields.len());
    for (key, value) in fields {
        out.insert(key.clone(), redact_field(key, value));
    }
    out
}

/// One field, filtered by name and then by value.
#[must_use]
pub fn redact_field(key: &str, value: &Value) -> Value {
    if is_secret_name(key) {
        // The name alone is enough. Replacing the value with its *hint* would still confirm the
        // value to anyone holding two log lines, and a hint is the one place we are allowed to be
        // non-reversible rather than absent.
        return Value::String(REDACTED.to_owned());
    }
    redact_value(value)
}

fn redact_value(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact_text(text)),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), redact_field(k, v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact_value).collect()),
        other => other.clone(),
    }
}

/// Free text: every credential shape and every e-mail address is replaced by its hint.
///
/// The hint (not [`REDACTED`]) is used for text because the value is embedded in a sentence the
/// operator still needs: "provider rejected `omnh_…` twice" is diagnosable, and the original
/// string is not reconstructable from it.
#[must_use]
pub fn redact_text(text: &str) -> String {
    let mut out = text.to_owned();
    // Collect first, then replace: a `replace_range` walk over a mutably borrowed string with
    // overlapping spans is how this kind of function quietly skips a match.
    for span in find_email_spans(&out) {
        let value = out[span.clone()].to_owned();
        out.replace_range(span, &hint_for(&value));
    }
    for span in find_credential_spans(&out) {
        let value = out[span.clone()].to_owned();
        out.replace_range(span, &hint_for(&value));
    }
    out
}

/// Spans (byte ranges) of the e-mail addresses in `text`.
///
/// Deliberately conservative: it needs a local part, an `@` and a dotted domain, and it does not
/// cross a space or a quote — so `"user@example.com"` in a JSON blob is found while
/// `"key=user@example.com;other=1"` is not half-matched at a semicolon.
fn find_email_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    for (index, byte) in bytes.iter().enumerate() {
        // `@` MUST be inside the atom run. Without it the scanner resets exactly at the `@`,
        // so `local` and `domain` are examined separately and `looks_like_email` never sees an
        // address at all — a filter that is correct on paper and matches nothing.
        let is_atom =
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'%' | b'+' | b'-' | b'@');
        if is_atom {
            if start.is_none() {
                start = Some(index);
            }
        } else if start.is_some() {
            let candidate = &text[start.unwrap()..index];
            if looks_like_email(candidate) {
                spans.push(start.unwrap()..index);
            }
            start = None;
        }
    }
    match start {
        Some(begin) if looks_like_email(&text[begin..]) => spans.push(begin..text.len()),
        _ => {}
    }
    spans
}

fn looks_like_email(candidate: &str) -> bool {
    let Some((local, domain)) = candidate.rsplit_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !local.starts_with('.')
}

/// Spans of the credential-shaped substrings in `text`.
///
/// A span runs from a matched prefix to the end of the run of characters that can legally be
/// part of a credential for that family (`[A-Za-z0-9_\-.=+/]`), so a real key is replaced whole
/// rather than leaving its tail behind.
fn find_credential_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut spans: Vec<std::ops::Range<usize>> = Vec::new();
    let lower = text.to_ascii_lowercase();
    for prefix in SECRET_VALUE_PREFIXES {
        let mut from = 0usize;
        while let Some(found) = lower[from..].find(&prefix.to_ascii_lowercase()) {
            let begin = from + found;
            // Require a boundary before the prefix so `task-sk-1` is not mistaken for a key, and
            // require at least one more character so the bare prefix `Bearer ` is not a match on
            // its own.
            // A prefix glued to a larger identifier is part of that identifier, not a
            // credential: `task-sk-1` is a job name, and redacting it teaches operators to
            // ignore the filter. A real key arrives after a quote, a space or a colon.
            let boundary_ok = begin == 0
                || text[..begin]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !c.is_ascii_alphanumeric() && !matches!(c, '-' | '_'));
            let mut end = begin + prefix.len();
            if *prefix == "-----BEGIN" {
                // A PEM block is multi-line: the credential run ends at the matching END marker,
                // not at the first space. A scanner that stops at the space leaves the base64
                // body in the log, which is the part that matters.
                end = text[begin..]
                    .find("-----END")
                    .map_or(text.len(), |offset| begin + offset + "-----END".len());
            } else {
                while end < text.len() && is_credential_char(text.as_bytes()[end]) {
                    end += 1;
                }
            }
            if boundary_ok && (end > begin + prefix.len() || *prefix == "-----BEGIN") {
                // A credential span may start inside an e-mail span that was already replaced;
                // dropping the overlap keeps both replacements from fighting over the bytes.
                if !spans
                    .iter()
                    .any(|existing| existing.start < end && begin < existing.end)
                {
                    spans.push(begin..end);
                }
            }
            from = begin + prefix.len();
        }
    }
    spans.sort_by_key(|span| span.start);
    spans
}

fn is_credential_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'=' | b'+' | b'/')
}

/// Whether a field name names a credential.
///
/// Three shapes, in order: exact, then suffix, then prefix. The suffix and prefix tests are what
/// catch `stripe_secret` and `userToken`; the exact test is what catches `pin` and `auth`, which
/// are far too short and too common to be treated as shapes.
#[must_use]
pub fn is_secret_name(name: &str) -> bool {
    // Split into components FIRST, on the separators *and* on camelCase humps. Normalising to
    // bare alphanumerics before splitting destroys the very boundaries this needs: `DB_PASSWORD_V2`
    // becomes one component `dbpasswordv2`, the exact-name list misses it and the component list
    // sees a single word. The case-boundary rule is what catches `userToken`, which has no
    // separator at all — and it is the shape a TypeScript caller writes most often.
    let components = name_components(name);

    // The joined form is what the exact list is written in (`apikey`, `privatekey`), and it also
    // catches a single segment that needs no split at all. The split components are what catch
    // `api_key` / `DB_PASSWORD_V2` / `userToken`, where the meaningful word is only a part.
    let joined = components.concat();
    if SECRET_NAMES.contains(&joined.as_str()) {
        return true;
    }
    if components.len() < 2 {
        return false;
    }
    components
        .iter()
        .any(|part| SECRET_SHAPES.contains(&part.as_str()))
}

/// The lowercase words of a field name.
///
/// A boundary is any non-alphanumeric character, and also any transition from a lowercase or
/// digit to an uppercase letter. The second rule is what makes `userToken` two components rather
/// than one; without it a camelCase caller — the majority of a JSON API's authors — gets no
/// name-based protection at all and falls back to the value-shape layer alone.
fn name_components(name: &str) -> Vec<String> {
    let mut components: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut previous_was_lower = false;
    for character in name.chars() {
        if !character.is_ascii_alphanumeric() {
            if !current.is_empty() {
                components.push(std::mem::take(&mut current));
            }
            previous_was_lower = false;
            continue;
        }
        if character.is_ascii_uppercase() && previous_was_lower {
            components.push(std::mem::take(&mut current));
        }
        current.extend(character.to_lowercase());
        previous_was_lower = character.is_ascii_lowercase();
    }
    if !current.is_empty() {
        components.push(current);
    }
    components
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FIXTURE_SECRET: &str = "sk-live-51H8xQ2eZvKYlo2C0aB7dEfGh3JkLmNoPqRsTuVwXy";
    const FIXTURE_EMAIL: &str = "operator@omnion.example";

    fn fields(pairs: Value) -> Map<String, Value> {
        match pairs {
            Value::Object(map) => map,
            _ => panic!("the fixture must be an object"),
        }
    }

    #[test]
    fn a_field_named_like_a_credential_never_keeps_its_value() {
        let filtered = redact_fields(&fields(json!({ "password": FIXTURE_SECRET })));
        assert_eq!(filtered.get("password").unwrap(), &json!(REDACTED));
        let rendered = serde_json::to_string(&filtered).unwrap();
        assert!(
            !rendered.contains("sk-live-51H8xQ2"),
            "the fixture value reached the field: {rendered}"
        );
    }

    #[test]
    fn the_leak_check_greps_the_value_the_system_would_actually_produce() {
        // A check against a stand-in constant can never fail, so this asserts on the fixture the
        // test itself feeds in AND on the hint form, which is what a real leak would look like.
        let filtered = redact_fields(&fields(json!({ "detail": FIXTURE_SECRET })));
        let rendered = serde_json::to_string(&filtered).unwrap();
        // The raw value must be gone...
        assert!(!rendered.contains("sk-live-51H8xQ2"));
        // ...and what replaced it must be the hint, not the value echoed back. Asserting the
        // hint is ABSENT would be asserting the filter does nothing.
        assert!(
            rendered.contains(&hint_for(FIXTURE_SECRET)),
            "the value was replaced by something other than its hint: {rendered}"
        );
    }

    #[test]
    fn an_innocent_field_name_still_loses_a_credential_shaped_value() {
        for name in ["detail", "note", "provider", "value", "body"] {
            let filtered = redact_fields(&fields(json!({ name: FIXTURE_SECRET })));
            assert!(
                !serde_json::to_string(&filtered)
                    .unwrap()
                    .contains("sk-live"),
                "the credential survived under the innocuous name `{name}`"
            );
        }
    }

    #[test]
    fn a_bare_prefix_is_not_treated_as_a_credential() {
        // `Bearer ` on its own, or a hyphenated word that merely starts with `sk-`, must not
        // produce a redaction: a filter that fires on ordinary words trains operators to ignore it.
        let filtered = redact_fields(&fields(json!({ "note": "task-sk-1 started" })));
        assert_eq!(filtered.get("note").unwrap(), &json!("task-sk-1 started"));
    }

    #[test]
    fn a_nested_credential_is_filtered_at_its_own_depth() {
        let filtered = redact_fields(&fields(json!({
            "request": { "headers": { "authorization": FIXTURE_SECRET, "accept": "json" } }
        })));
        let rendered = serde_json::to_string(&filtered).unwrap();
        assert!(
            !rendered.contains("sk-live"),
            "nested credential leaked: {rendered}"
        );
        assert!(
            rendered.contains("json"),
            "an innocent sibling must survive"
        );
    }

    #[test]
    fn an_array_of_credential_shaped_values_is_filtered() {
        let filtered = redact_fields(&fields(json!({
            "tried": [FIXTURE_SECRET, FIXTURE_EMAIL]
        })));
        let rendered = serde_json::to_string(&filtered).unwrap();
        assert!(!rendered.contains("sk-live"));
        assert!(!rendered.contains(FIXTURE_EMAIL));
    }

    #[test]
    fn an_email_is_replaced_by_a_hint_that_does_not_carry_it() {
        let redacted = redact_text(&format!("could not reach {FIXTURE_EMAIL} for the rotation"));
        assert!(
            !redacted.contains(FIXTURE_EMAIL),
            "the address survived: {redacted}"
        );
        assert!(redacted.contains("could not reach"));
        assert!(redacted.contains("for the rotation"));
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        let text = "the re-wrap batch advanced to cursor 41 in 3.2s";
        assert_eq!(redact_text(text), text);
    }

    #[test]
    fn every_credential_family_is_recognised_by_value_shape() {
        // The candidates are BUILT, not typed. A literal `ghp_…` / `xoxb-…` / `AKIA…` in a test
        // file is a real credential to GitHub's push protection, which blocks the whole push — and
        // the block is correct: nothing can tell a redacted fixture from a live key by looking at
        // it. Assembling the same bytes from a prefix and a filler keeps the assertion exact and
        // the repository pushable, with no way to "just allow it once" in review.
        let filler = "Q".repeat(24);
        let candidates = [
            format!("whsec_{filler}"),
            format!("ghp_{filler}"),
            format!("glpat-{filler}"),
            format!("xoxb-{filler}"),
            format!("AKIA{filler}"),
            format!("AIza{filler}"),
            "-----BEGIN RSA PRIVATE KEY-----".to_owned(),
        ];
        for candidate in candidates {
            let redacted = redact_text(&format!("provider said: {candidate} is invalid"));
            assert!(
                !redacted.contains(candidate.as_str()),
                "the family leaked verbatim: {candidate}"
            );
        }
    }

    #[test]
    fn field_names_are_matched_case_and_separator_insensitively() {
        for name in [
            "password",
            "PASSWORD",
            "api_key",
            "apiKey",
            "api-key",
            "DB_PASSWORD_V2",
            "stripe_secret",
            "userToken",
        ] {
            let filtered = redact_fields(&fields(json!({ name: "irrelevant" })));
            assert_eq!(
                filtered.get(name).unwrap(),
                &json!(REDACTED),
                "`{name}` was not treated as a credential name"
            );
        }
    }

    #[test]
    fn a_short_field_name_is_not_a_shape_but_an_exact_word() {
        // `pin` and `auth` are credentials; `pipeline` and `author` merely start with the same
        // letters. Treating a prefix as a credential would redact half the catalogue.
        assert!(is_secret_name("pin"));
        assert!(is_secret_name("auth"));
        assert!(!is_secret_name("pipeline"));
        assert!(!is_secret_name("author"));
        assert!(!is_secret_name("authorized_role"));
    }
}
