//! What counts as a valid generated definition (docs/requests/REQ-046).
//!
//! The claim this module exists to make is one sentence long: **a generated definition is the
//! same shape the workflow API already accepts, and it is checked by the same validator.**
//! There is no second shape and no second validator, because a second one is where a model
//! answer becomes something the engine has never run.
//!
//! Two rules are added on top of [`omnion_workflows`], and both are about *where a model's
//! answer may point* rather than about whether the engine could run it:
//!
//! * **No credential-shaped parameters.** The model writes a definition; a definition that
//!   names `api_key`, a bearer token or a `sk-…` string would put a secret into a row the
//!   console displays, exports in a bulk action and sends to every webhook subscriber. The
//!   action registry has no such parameter, so refusing the *names* costs nothing — and the
//!   check is on the parameter **key and value**, not on the action, because the action is
//!   what a model is most likely to invent.
//! * **The answer must be JSON, not prose.** A model asked for JSON will sometimes answer
//!   with a fenced block and a sentence of preamble; the first is recoverable and the second
//!   is a strong hint the answer is about to be refused, so the extraction happens here, once,
//!   with the reason it could not be extracted named.

use omnion_workflows::definition::WorkflowDefinition;
use serde_json::Value;

use crate::error::{AiWorkflowError, Result};

/// Longest a model answer may be before it is refused as prose rather than JSON.
///
/// A definition the engine can run is bounded: at most 50 steps, each with a name and a small
/// parameter object. Four times that in characters is generous for a real answer and small
/// enough that a model which ignored the instruction cannot make the console hold a megabyte
/// of markdown per draft.
pub const MAX_ANSWER_BYTES: usize = 64 * 1024;

/// Longest a title the model invents may be.
pub const MAX_MODEL_TITLE_LEN: usize = 120;

/// Parameter keys that would put a secret into a draft row.
///
/// Matched case-insensitively and on the *whole key*, not as a substring: a parameter called
/// `to` or `body` is legitimate on `send_email` and must not be refused, and a substring test
/// for `key` would refuse `apiKeyId` and, worse, `keyword_filter`.
pub const SECRET_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "access_token",
    "auth",
    "authorization",
    "bearer",
    "client_secret",
    "credential",
    "credentials",
    "password",
    "private_key",
    "refresh_token",
    "secret",
    "session_token",
    "token",
];

/// A value shape that is a secret no matter which key it sits under.
const SECRET_VALUE_PREFIXES: &[&str] = &[
    "bearer ",
    "basic ",
    "eyj",  // a JWT
    "ghp_", // a GitHub token
    "sk-",  // the OpenAI-compatible convention
    "xoxb-", // a Slack bot token
    "xoxp-", // a Slack user token
];

/// One parsed answer.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedDefinition {
    /// The model's own title, trimmed and bounded; **empty** when it sent none usable, so
    /// the caller decides the fallback rather than this function inventing one. A generated
    /// title is a model artefact and belongs next to the answer; the console shows it, but a
    /// draft nobody named is still findable by its prompt.
    pub title: String,
    /// The model's explanation, markdown, when it sent one.
    pub rationale: Option<String>,
    /// The definition as stored: `{ trigger, steps }`.
    pub definition: Value,
}

/// Read one model answer into a title, a rationale and a definition.
///
/// Pure, and the seam the repair round-trip measures: `generate` calls it, and when it
/// refuses, the refusal's message is what the repair prompt quotes back to the model. So the
/// messages here are written to be *sent to a model* as well as shown to a person — "step
/// \"notify\" names the action \"smtp.send\", which is not one of: …" tells the model exactly
/// what to change, and a message like "invalid JSON" would not.
pub fn parse_answer(answer: &str) -> Result<ParsedDefinition> {
    let raw = answer.trim();
    if raw.is_empty() {
        return Err(AiWorkflowError::invalid(
            "empty_answer",
            "the model answered with nothing — ask again, or pick a different model",
        ));
    }
    if raw.len() > MAX_ANSWER_BYTES {
        return Err(AiWorkflowError::invalid(
            "answer_too_long",
            format!(
                "the model's answer is {} bytes; a workflow definition is at most {MAX_ANSWER_BYTES}",
                raw.len()
            ),
        ));
    }

    let value = extract_json(raw)?;
    let rationale = read_rationale(&value);
    // The engine's own validator, on the same shape `POST /workflows` accepts. Its own code
    // (`invalid_step_action`, `invalid_cron`, …) is carried through untouched, so the console
    // and the repair prompt both read the engine's vocabulary rather than a translation.
    let definition = normalise(&value)?;
    validate(&definition)?;

    Ok(ParsedDefinition {
        title: read_title(&value),
        rationale,
        definition,
    })
}

/// Pull the JSON object out of an answer that may be wrapped in a fence or a sentence.
///
/// Three shapes are tried in a fixed order, and the order is the rule: fence, then whole
/// answer, then the outermost brace pair. The fence first because a model that wraps its
/// answer in one almost always puts *only* the answer inside; the whole answer second because
/// an unfenced answer that is already JSON is the common case and must not be scanned for
/// braces; braces last because it is the only one that can find an object inside prose.
///
/// **The brace scan is not a JSON parser** and is treated as a locator, not a validator: it
/// counts braces while ignoring those inside strings, so a `}` in a quoted prompt does not
/// end the object early. Everything it locates then goes through the real parser.
fn extract_json(raw: &str) -> Result<Value> {
    // The fenced body when there is a fence, else the whole answer. A fenced body is tried
    // first because a model that fences its answer puts *only* the answer inside; the whole
    // answer second because an unfenced answer that is already JSON is the common case and
    // must not be scanned for braces.
    let candidate = fenced_body(raw).unwrap_or_else(|| raw.to_owned());

    if let Ok(value) = serde_json::from_str::<Value>(candidate.trim()) {
        return Ok(value);
    }
    // The last resort, and the only one that can find an object inside prose.
    let inner = match outer_braces(&candidate) {
        Some(inner) => inner,
        None => return Err(unparsable()),
    };
    serde_json::from_str::<Value>(inner).map_err(|_| unparsable())
}

/// The one refusal message for an answer that is not JSON, named once so the two call sites
/// cannot drift apart.
fn unparsable() -> AiWorkflowError {
    AiWorkflowError::invalid(
        "unparsable_answer",
        "the model did not answer with a JSON object. Answer with exactly one JSON object \
         holding `title`, `rationale` and `definition`, and nothing outside the object.",
    )
}

/// The text between a pair of ``` fences, when the answer has one.
///
/// Both layouts are read: the body on the fence's own line (```` ```json {"a":1} ``` ````) and
/// the body on the lines below it. The language tag is dropped rather than required, because a
/// model that writes ```javascript around a JSON object still wrote JSON, and refusing it here
/// would spend a repair round-trip on a formatting difference.
///
/// **Not a pair search**: the first fence opens and the next one closes, whatever is between.
/// A body that itself contains ``` ends early — which costs nothing, because the real parser
/// runs on the result and simply refuses, and the brace fallback then finds the object.
fn fenced_body(raw: &str) -> Option<String> {
    let mut lines = raw.lines();
    let open = lines.find(|line| line.trim().starts_with("```"))?;
    let after_open = open.trim_start_matches('`').trim();

    let mut body = String::new();
    // Content on the opening line itself: ` ```json {"a": 1} ``` `.
    let (same_line, same_line_closed) = match after_open.find("```") {
        Some(end) => (Some(after_open[..end].to_owned()), true),
        None => (None, false),
    };
    match same_line {
        Some(text) if same_line_closed && !text.trim().is_empty() => {
            // ```json {...}``` — drop a bare language tag, keep anything that is not one.
            let body = text.trim();
            let body = body
                .strip_prefix("json")
                .filter(|rest| rest.starts_with('{'))
                .unwrap_or(body);
            return Some(body.to_owned());
        }
        Some(_) => return None,
        None => {}
    }

    for line in lines {
        if line.trim().starts_with("```") {
            break;
        }
        body.push_str(line);
        body.push('\n');
    }
    let body = body.trim();
    (!body.is_empty()).then(|| body.to_owned())
}

/// The text between the first `{` and its matching `}`, ignoring braces inside strings.
fn outer_braces(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (index, byte) in bytes.iter().enumerate().skip(start) {
        if in_string {
            // A backslash escapes the next byte, so `\\"` does not close the string.
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=index]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Reduce the answer to the definition object, refusing a shape that is not one.
///
/// Three accepted inputs, in this order: `{"definition": {…}}` (what the prompt asks for),
/// `{…}` whose own keys are the definition's, and a bare `{…}`. A *list* is refused here
/// rather than coerced — a model that answered `[{"name": …}]` has produced steps without a
/// trigger, and quietly wrapping it in a manual trigger would be inventing the half it
/// omitted.
fn normalise(value: &Value) -> Result<Value> {
    let Some(object) = value.as_object() else {
        return Err(AiWorkflowError::invalid(
            "answer_not_an_object",
            "the model's answer must be a JSON object, not a list or a bare string",
        ));
    };

    if let Some(nested) = object.get("definition") {
        if !nested.is_object() {
            return Err(AiWorkflowError::invalid(
                "answer_not_an_object",
                "`definition` must be a JSON object holding `trigger` and `steps`",
            ));
        }
        return Ok(nested.clone());
    }

    let holds_definition =
        object.contains_key("trigger") || object.contains_key("steps");
    if holds_definition {
        // A definition with steps and no trigger is refused HERE, with the code that names the
        // missing half, rather than handed to the engine. The engine's own refusal for it is a
        // *deserialisation* failure ("the definition is not a workflow the engine accepts:
        // missing field `trigger`"), which is true and useless: a model reading a repair prompt
        // needs to be told that `trigger` is absent, not that JSON is shaped oddly. The
        // distinction is the whole reason this module has its own validation at all.
        if object.contains_key("steps") && !object.contains_key("trigger") {
            return Err(AiWorkflowError::invalid(
                "answer_has_no_definition",
                "the answer has `steps` but no `trigger`. Every workflow needs a `trigger` \
                 object — `{\"kind\": \"manual\"}`, or `{\"kind\": \"schedule\", \"cron\": ...}`, \
                 or `{\"kind\": \"event\", \"event\": ...}` — alongside its `steps`.",
            ));
        }
        return Ok(Value::Object(object.clone()));
    }

    Err(AiWorkflowError::invalid(
        "answer_has_no_definition",
        "the model's answer has no `definition`. Answer with `definition` holding `trigger` \
         and `steps`, or with `trigger` and `steps` at the top level.",
    ))
}

/// Run the engine's validator, and the two rules that are only about a *generated* answer.
pub fn validate(definition: &Value) -> Result<()> {
    let parsed: WorkflowDefinition = serde_json::from_value(definition.clone()).map_err(|err| {
        AiWorkflowError::invalid(
            "invalid_draft_definition",
            format!("the definition is not a workflow the engine accepts: {err}"),
        )
    })?;
    // The engine's own message and code: `invalid_step_action` naming the actions that exist,
    // `invalid_cron` naming the schedule it could not read. Translating them here would give
    // the repair prompt a second vocabulary to reason about, and the model's own vocabulary is
    // the one it can act on.
    parsed.validate().map_err(|error| AiWorkflowError::invalid(error.code(), error.to_string()))?;

    if let Some(found) = find_secret(definition) {
        return Err(AiWorkflowError::invalid(
            "secret_in_definition",
            format!(
                "`{found}` looks like a credential. A generated definition may only name the \
                 parameters of the actions this platform has; put the value in the action's own \
                 credential store instead."
            ),
        ));
    }

    Ok(())
}

/// The first secret-shaped key or value in the definition, if there is one.
///
/// Recurses through the whole value, because a model that cannot put a token in `params` may
/// put it in a nested object the validator never looks at — and the console prints the whole
/// definition, so "the validator checked `params`" is not a claim about what is stored.
fn find_secret(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => map.iter().find_map(|(key, nested)| {
            let key_is_secret = normalise_key(key);
            if SECRET_KEYS.contains(&key_is_secret.as_str()) {
                return Some(key.clone());
            }
            if value_is_secret(nested) {
                return Some(key.clone());
            }
            find_secret(nested)
        }),
        Value::Array(items) => items.iter().find_map(find_secret),
        _ => None,
    }
}

/// `true` when a value is a string that starts with a credential-shaped prefix.
///
/// Case-insensitive on the prefix and **trimmed** first, because `" Bearer abc"` is the same
/// secret as `"Bearer abc"` and a check on the raw string would let the leading space through.
fn value_is_secret(value: &Value) -> bool {
    let Some(text) = value.as_str() else {
        return false;
    };
    let trimmed = text.trim().to_ascii_lowercase();
    SECRET_VALUE_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

/// A parameter key reduced to the shape the secret list is written in.
fn normalise_key(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect::<String>()
        .to_ascii_lowercase()
}

/// The model's own title, trimmed and bounded; **empty** when it sent none usable, so the
/// caller decides the fallback rather than this function inventing one.
///
/// `String` rather than `Option` because the console's fallback is the caller's decision (a
/// generated title is a model artefact and belongs beside the answer), and an `Option` here
/// would only add a third shape for two states that already have one.
fn read_title(value: &Value) -> String {
    value
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map_or_else(String::new, |title| truncate_chars(title, MAX_MODEL_TITLE_LEN))
}

/// The model's own rationale, if it sent one.
fn read_rationale(value: &Value) -> Option<String> {
    value
        .get("rationale")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| text.to_owned())
}

/// Cut a string to `max` characters, on a character boundary.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The request's own example, as a model would answer it.
    fn overdue_answer() -> &'static str {
        r#"{
          "title": "Overdue invoice chase",
          "rationale": "Two reminders, then an owner task.",
          "definition": {
            "trigger": { "kind": "schedule", "cron": "0 9 * * *" },
            "steps": [
              { "name": "find overdue", "kind": "task", "action": "noop", "params": {} },
              { "name": "chase", "kind": "task", "action": "noop", "params": {} }
            ]
          }
        }"#
    }

    #[test]
    fn the_requests_own_example_parses_and_validates() {
        let parsed = parse_answer(overdue_answer()).expect("the example is a valid answer");
        assert_eq!(parsed.title, "Overdue invoice chase");
        assert_eq!(
            parsed.rationale.as_deref(),
            Some("Two reminders, then an owner task.")
        );
        // It is an ordinary definition: the engine's own type accepts it, which is the claim
        // the whole request rests on.
        let engine: WorkflowDefinition =
            serde_json::from_value(parsed.definition.clone()).expect("the engine accepts it");
        engine.validate().expect("and it validates");
    }

    #[test]
    fn a_fenced_answer_with_preamble_is_read() {
        let answer = format!(
            "Sure! Here is the workflow you asked for:\n\n```json\n{}\n```\n\nLet me know if \
             you want it to run hourly.",
            overdue_answer()
        );
        let parsed = parse_answer(&answer).expect("the fence is unwrapped");
        assert_eq!(parsed.title, "Overdue invoice chase");
    }

    #[test]
    fn an_unfenced_answer_with_a_sentence_around_it_is_read() {
        let answer = format!(
            "Here it is: {}\nLet me know when you want to arm it.",
            overdue_answer()
        );
        assert!(parse_answer(&answer).is_ok(), "the brace scan finds the object");
    }

    #[test]
    fn a_brace_inside_a_quoted_prompt_does_not_end_the_object_early() {
        // The failure this guards: a scan that counts every `}` returns the text up to the
        // first one inside the string, which is not JSON, so a perfectly good answer is
        // reported as "the model did not answer with a JSON object" — and the repair prompt
        // then asks a model to fix an answer that was never broken.
        let answer = r#"{
          "title": "Tidy braces",
          "definition": {
            "trigger": { "kind": "manual" },
            "steps": [
              { "name": "ask", "kind": "task", "action": "echo",
                "params": { "value": "print {\"nested\": 1} for me" } }
            ]
          }
        }"#;
        let parsed = parse_answer(answer).expect("a brace inside a string is not the end");
        assert_eq!(parsed.title, "Tidy braces");
    }

    #[test]
    fn an_answer_with_the_definition_at_the_top_level_is_accepted() {
        let answer = r#"{
          "trigger": { "kind": "manual" },
          "steps": [ { "name": "noop", "kind": "task", "action": "noop", "params": {} } ]
        }"#;
        let parsed = parse_answer(answer).expect("the definition may stand alone");
        assert!(parsed.title.is_empty(), "with no title of its own");
        parsed
            .definition
            .as_object()
            .expect("a definition object");
    }

    #[test]
    fn prose_with_no_object_is_refused_with_a_message_a_model_can_act_on() {
        let error = parse_answer("I'd be happy to help, but I need more detail.")
            .expect_err("prose is not a definition");
        assert_eq!(error.code(), "unparsable_answer");
        // The message is what the repair prompt quotes back, so it has to say what to send.
        assert!(error.to_string().contains("JSON"), "{error}");
    }

    #[test]
    fn an_empty_answer_is_its_own_failure() {
        assert_eq!(
            parse_answer("   \n ").expect_err("nothing is not an answer").code(),
            "empty_answer"
        );
    }

    #[test]
    fn a_list_of_steps_without_a_trigger_is_refused_not_invented() {
        let error = parse_answer(r#"{"steps": [ {"name": "x", "kind": "task", "action": "noop"} ]}"#)
            .expect_err("half a definition is refused");
        assert_eq!(error.code(), "answer_has_no_definition");
    }

    #[test]
    fn a_bare_list_is_refused() {
        let error = parse_answer("[1, 2, 3]").expect_err("a list is not an object");
        assert_eq!(error.code(), "answer_not_an_object");
    }

    #[test]
    fn the_engine_speaks_in_its_own_vocabulary() {
        // The repair prompt quotes this message, so it must name the actions that exist —
        // "invalid definition" would leave a model guessing which of the four it invented.
        let error = parse_answer(
            r#"{"definition": {"trigger": {"kind": "manual"},
              "steps": [{"name": "send", "kind": "task", "action": "smtp.send", "params": {}}]}}"#,
        )
        .expect_err("an action the platform does not have is refused");
        assert_eq!(error.code(), "invalid_step_action");
        assert!(error.to_string().contains("noop"), "{error}");
    }

    #[test]
    fn a_broken_cron_is_refused_in_the_engine_own_words() {
        let error = parse_answer(
            r#"{"definition": {"trigger": {"kind": "schedule", "cron": "every morning"},
              "steps": [{"name": "x", "kind": "task", "action": "noop", "params": {}}]}}"#,
        )
        .expect_err("a prose schedule is not a cron");
        assert_eq!(error.code(), "invalid_cron");
    }

    #[test]
    fn a_definition_carrying_a_credential_is_refused_wherever_it_sits() {
        for needle in [
            r#""params": {"api_key": "abc"}"#,
            r#""params": {"apiKey": "abc"}"#,
            r#""params": {"nested": {"password": "hunter2"}}"#,
            r#""params": {"list": [{"secret": "x"}]}"#,
        ] {
            let answer = format!(
                r#"{{"definition": {{"trigger": {{"kind": "manual"}},
                 "steps": [{{"name": "call", "kind": "task", "action": "noop", {needle}}}]}}}}"#
            );
            let error = parse_answer(&answer)
                .expect_err("a credential in a draft is refused wherever it sits");
            assert_eq!(error.code(), "secret_in_definition", "{needle}");
        }
    }

    #[test]
    fn a_credential_shaped_value_is_refused_under_a_harmless_key() {
        // The name check alone would pass this: `to` is a legitimate `send_email` parameter.
        let answer = r#"{"definition": {"trigger": {"kind": "manual"},
          "steps": [{"name": "call", "kind": "task", "action": "echo",
            "params": {"value": "sk-live-0123456789"}}]}}"#;
        let error = parse_answer(answer).expect_err("the value is a key no matter its label");
        assert_eq!(error.code(), "secret_in_definition");
    }

    #[test]
    fn legitimate_parameters_are_not_mistaken_for_secrets() {
        // Every one of these is a real parameter of a real action. A substring rule ("any key
        // containing `key`", "any key containing `auth`") refuses them all, which would make
        // the secret check refuse the very actions the prompt tells the model to use.
        let answer = r#"{"definition": {"trigger": {"kind": "manual"},
          "steps": [{"name": "mail", "kind": "task", "action": "send_email",
            "params": {"to": "{{steps.1.output.customer.email}}",
                       "subject": "Your invoice", "body": "Please pay"}},
                   {"name": "noop", "kind": "task", "action": "noop", "params": {}}]}}"#;
        assert!(parse_answer(answer).is_ok(), "the real action set is not a credential");
    }

    #[test]
    fn the_secret_list_matches_whole_keys_case_insensitively() {
        assert_eq!(normalise_key("API_Key"), "api_key");
        assert_eq!(normalise_key("apiKey"), "apikey");
        assert_eq!(normalise_key("api-key"), "apikey");
        // And does NOT collapse to one of the listed names when it is not one.
        assert!(!SECRET_KEYS.contains(&normalise_key("keyword_filter").as_str()));
        assert!(!SECRET_KEYS.contains(&normalise_key("author").as_str()));
    }

    #[test]
    fn a_leading_space_does_not_hide_a_credential_value() {
        assert!(value_is_secret(&Value::String("  Bearer abc".to_owned())));
        assert!(value_is_secret(&Value::String("SK-live-1".to_owned())));
        assert!(!value_is_secret(&Value::String("send the email".to_owned())));
        // A number is not a secret, and a *stringified* one is not either — a definition may
        // legitimately carry `"port": 587`. The test is here because `Value::String(42.into())`
        // is a **compile error** (`String: From<i32>` does not exist), which is the cheapest
        // possible proof that a "clever" fixture would never have run: the non-string arm has
        // to be written as a real number.
        assert!(!value_is_secret(&Value::Number(42.into())));
        assert!(!value_is_secret(&Value::String("42".to_owned())));
        assert!(!value_is_secret(&Value::Bool(true)));
    }

    #[test]
    fn a_long_title_is_cut_on_a_character_boundary() {
        let value = serde_json::json!({
            "title": "ş".repeat(MAX_MODEL_TITLE_LEN + 10),
            "definition": {
                "trigger": { "kind": "manual" },
                "steps": [{ "name": "x", "kind": "task", "action": "noop", "params": {} }],
            },
        });
        let parsed = parse_answer(&serde_json::to_string(&value).expect("serialises"))
            .expect("the rest of the answer is fine");
        // Character boundary, not byte: cutting at MAX bytes here would split a `ş` and
        // store an invalid UTF-8 sequence, which the row's insert would refuse — the refusal
        // naming a title rather than the model's answer.
        assert_eq!(parsed.title.chars().count(), MAX_MODEL_TITLE_LEN);
        assert!(parsed.title.chars().all(|c| c == 'ş'));
    }

    #[test]
    fn a_fence_holding_the_answer_on_one_line_is_read() {
        let answer = format!("```json\n{}\n```", overdue_answer());
        assert!(parse_answer(&answer).is_ok(), "the common layout");
    }

    #[test]
    fn a_fence_with_an_unfamiliar_language_tag_is_still_json() {
        // A model that writes ```javascript around a JSON object wrote JSON. Refusing it here
        // would spend the single repair round-trip on a formatting difference and then fail
        // the draft for a reason the author cannot see.
        let answer = format!("```javascript\n{}\n```", overdue_answer());
        assert!(parse_answer(&answer).is_ok(), "the tag does not change the body");
    }
}
