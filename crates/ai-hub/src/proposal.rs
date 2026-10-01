//! Reading a model's proposed operations out of a chat answer (REQ-101, slice 3g).
//!
//! # The gap this file closes
//!
//! Slice 3a built the change set end to end — file, edit, confirm, apply, land in the inbox
//! when an operation is gated. What it did **not** build is the entry point the request names:
//! *"a conversation may end in a proposed change set"* and the editor is *"opened from a chat
//! reply proposing operations"*. Every set so far was filed by a screen or a test, so the
//! pipeline had no producer a person would actually meet.
//!
//! # Why a fenced block and not a tool call
//!
//! The obvious implementation is to offer the model a `propose_changes` tool and run the
//! tool-calling loop. That is deliberately not done here, and the reason is in the chat route:
//! `POST /ai/chat` sends `tools: Vec::new()` **on purpose** — "the chat endpoint is a
//! conversation, not an agent: a run is what offers tools, and offering them here would let a
//! caller reach a tool the route never checked a permission for."
//!
//! So a proposal arrives as *text*, and it has to be read out of that text. This is the risk the
//! format is designed against, and the rule is that **a fenced block can only ever create a
//! draft**. Parsing is a claim, not an authority: even a perfectly parsed set is `draft`, an
//! editor the reviewer reads, and confirming it is a separate call behind `ai.approvals.act`
//! whose own typed confirmation and gate rules apply. A model that hallucinates a set has
//! invented a screen row, not a content change.
//!
//! # The format
//!
//! One fenced block, tagged `change-set`, holding one JSON object. The answer is
//!
//! ```text
//! Here's what I'd change.
//!
//! <a fence>change-set
//! {"title": "Fix the two weak pages", "operations": [
//!   {"kind": "update", "resource_type": "page", "resource_id": "…", "args": {"title": "…"}}
//! ]}
//! <a fence>
//! ```
//!
//! — that is, a three-backtick fence tagged `change-set`, an opening line, the JSON, and a
//! closing three-backtick fence. It is spelled `<a fence>` here because a nested fence inside a
//! fenced block ends the outer one, and a truncated doc comment is a doctest failure in a crate
//! that otherwise has none.
//!
//! The tag is required rather than any fence, because a chat answer routinely carries `json`
//! and `text` blocks that are *not* proposals — a model quoting an example is the common case,
//! and a plain `json` fence is exactly that.
//!
//! # What is refused, and why each refusal is here
//!
//! Every rule below is a way a proposal could describe work the platform cannot do, or could do
//! twice. A parser that "does its best" on any of them files a set that the reviewer cannot
//! apply — which is worse than filing nothing, because the reviewer is now looking at a plan.

use serde_json::Value;

use crate::change_sets::{
    self, ChangeOp, ChangeSet, MAX_OPERATIONS, MAX_TITLE_CHARS, OpKind, Operation, keys_for,
    validate_operation,
};
use crate::error::{AiHubError, Result};

/// The fence tag a proposal is written in. Part of the prompt, so it is one constant and not
/// a literal repeated by the caller, the parser and the walk.
pub const FENCE_TAG: &str = "change-set";

/// The most fences one answer may carry, read before any of them is parsed.
///
/// A model that emitted four blocks is either confused or being chatty, and neither is a
/// proposal. The chat route refuses with this count rather than picking the first block, which
/// would be a guess about which of them the reviewer was meant to see.
pub const MAX_BLOCKS: usize = 1;

/// The system instruction a caller appends when it wants proposals offered.
///
/// Kept here rather than in the route so the **format** and the **instruction that describes
/// the format** cannot drift: a prompt that says `propose_changes` while the parser reads
/// `change-set` produces a conversation that proposes nothing, with no error anywhere.
#[must_use]
pub fn system_instruction() -> String {
    format!(
        "When you can carry out changes to content, describe them instead of performing them. \
         Put them in a single fenced block tagged `{FENCE_TAG}` holding one JSON object: \
         {{\"title\": \"…\", \"operations\": [{{\"kind\": \"update\", \"resource_type\": \"page\", \
         \"resource_id\": \"…\", \"args\": {{\"title\": \"…\"}}}}]}}. \
         `kind` is create, update or delete; `resource_type` is `page`. \
         The block is a *proposal*: a person reviews every field before anything is written, \
         so never describe a change as done, and never write more than {MAX_OPERATIONS} operations. \
         Every other question is answered as prose."
    )
}

/// A proposal read out of an answer, before it has been pinned to revisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    /// What the reviewer sees in the inbox.
    pub title: String,
    /// The operations, keyed and validated, ready for [`crate::change_sets::store::append`].
    pub operations: Vec<ChangeOp>,
}

/// Read the proposal an answer carries, if it carries one.
///
/// `None` means **the answer proposed nothing** and is a normal outcome — most answers do.
/// `Err` means the answer *claimed* to propose something and the claim is unusable, which is
/// different: the caller should tell the person their answer could not be read rather than
/// silently drop it, because a set that is filed and lost is worse than one that is refused
/// with a reason.
///
/// # Errors
///
/// - `InvalidChangeSet` — a block was present but is not a usable proposal, with the reason
///   naming what to fix.
/// - More than [`MAX_BLOCKS`] blocks: "this answer proposed several change sets; one at a time".
pub fn parse(answer: &str) -> Result<Option<Proposal>> {
    let blocks = fenced(answer, FENCE_TAG);

    match blocks.len() {
        0 => Ok(None),
        1 => Ok(Some(from_block(&blocks[0])?)),
        count => Err(AiHubError::InvalidChangeSet(format!(
            "this answer carries {count} `{FENCE_TAG}` blocks; propose one change set at a time"
        ))),
    }
}

/// Every ````-fenced block of `tag` in `text`, in order, without the fence lines.
///
/// Written by hand rather than with a line scanner over the whole answer because the fence has
/// to be matched on its **own line**. An answer containing ```` ```change-set ```` inline in a
/// sentence is quoting the format, and treating that as a block would let a model produce a
/// proposal by mentioning one.
fn fenced(text: &str, tag: &str) -> Vec<String> {
    let open = format!("```{tag}");
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        match current.as_mut() {
            // Inside a block: the closing fence is ` ``` ` alone, or a longer run, but not a
            // new opening fence (which would be the start of a *second* block, and a model's
            // unterminated block must not swallow the rest of the answer).
            Some(body) => {
                if trimmed == "```" || trimmed.starts_with("``` ") {
                    blocks.push(body.clone());
                    current = None;
                } else {
                    body.push_str(line);
                    body.push('\n');
                }
            }
            None => {
                if trimmed.starts_with(&open) {
                    current = Some(String::new());
                }
            }
        }
    }

    // An unterminated block is not a proposal. A model that opened a fence and never closed it
    // has produced a truncated answer, and reading the remainder of the conversation as JSON is
    // how a set ends up carrying the next turn's text.
    blocks
}

/// The operations a caller may offer, as a plain list the route can turn into operations.
pub fn from_block(block: &str) -> Result<Proposal> {
    let value: Value = serde_json::from_str(block.trim()).map_err(|error| {
        AiHubError::InvalidChangeSet(format!("the `{FENCE_TAG}` block is not JSON: {error}"))
    })?;

    let Some(object) = value.as_object() else {
        return Err(AiHubError::InvalidChangeSet(format!(
            "the `{FENCE_TAG}` block is a {}; it must be a JSON object",
            kind_of(&value)
        )));
    };

    // Rejected as unknown keys rather than ignored. A model that wrote `"operation"` where the
    // format says `"operations"` has made a mistake, and silently reading zero operations out of
    // it would file an empty set — or, worse, a set built from whatever the parser could
    // salvage while the reviewer believes they read what the model said.
    reject_unknown(object, &["title", "operations"])?;

    let title = object
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .ok_or_else(|| AiHubError::InvalidChangeSet("the proposal needs a title".to_owned()))?;
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(AiHubError::InvalidChangeSet(format!(
            "the title is {} characters; the limit is {MAX_TITLE_CHARS}",
            title.chars().count()
        )));
    }

    let raw = object
        .get("operations")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            AiHubError::InvalidChangeSet("the proposal needs an `operations` list".to_owned())
        })?;
    if raw.is_empty() {
        return Err(AiHubError::InvalidChangeSet(
            "a change set needs at least one operation; an empty one is not a proposal".to_owned(),
        ));
    }
    if raw.len() > MAX_OPERATIONS {
        return Err(AiHubError::InvalidChangeSet(format!(
            "the proposal carries {} operations; the limit is {MAX_OPERATIONS}",
            raw.len()
        )));
    }

    let operations = parse_operations(raw)?;

    // The **store's** rules, not a copy of them: a parser that accepted a shape the store
    // refuses would file a row the confirm could never apply.
    let draft = ChangeSet {
        id: uuid::Uuid::nil(),
        organization_id: uuid::Uuid::nil(),
        site_id: None,
        title: title.to_owned(),
        status: change_sets::INITIAL_STATUS.to_owned(),
        operations: operations.clone(),
        base_revisions: Default::default(),
        content_hash: String::new(),
        created_by: None,
        created_by_agent: None,
        created_by_run: None,
        updated_by: None,
        confirmed_at: None,
        applied_at: None,
        discarded_reason: None,
        created_at: time::OffsetDateTime::UNIX_EPOCH,
        updated_at: time::OffsetDateTime::UNIX_EPOCH,
    };
    change_sets::validate(&draft)?;

    Ok(Proposal {
        title: title.to_owned(),
        operations,
    })
}

/// The operations out of the raw list, keyed with the same [`keys_for`] the route's own
/// `create` uses, so a proposal and a hand-filed set are the same shape.
fn parse_operations(raw: &[Value]) -> Result<Vec<ChangeOp>> {
    // Two passes, deliberately: the keys are derived from the operations, so the operations
    // have to exist before any key can be computed. A single pass that keyed as it went would
    // have to guess the key of operation 2 from operations 0 and 1 alone.
    let mut operations = Vec::with_capacity(raw.len());
    for (index, entry) in raw.iter().enumerate() {
        operations.push(operation(entry, index)?);
    }

    let derived = keys_for(&operations);
    let keyed = operations
        .into_iter()
        .zip(derived.iter())
        .map(|(operation, key)| ChangeOp {
            key: key.clone(),
            operation,
        })
        .collect::<Vec<_>>();

    for op in &keyed {
        validate_operation(op)?;
    }
    Ok(keyed)
}

/// One entry of the `operations` list, as an [`Operation`].
fn operation(value: &Value, index: usize) -> Result<Operation> {
    let position = format!("operation {}", index + 1);
    let Some(object) = value.as_object() else {
        return Err(AiHubError::InvalidChangeSet(format!(
            "{position} is a {}; every operation is an object",
            kind_of(value)
        )));
    };
    reject_unknown(object, &["kind", "resource_type", "resource_id", "args"]).map_err(|error| {
        // The same message, prefixed with which operation: an `args` list at index 4 of 9 is a
        // fact the reviewer can act on and "unknown key" alone is not.
        AiHubError::InvalidChangeSet(format!("{position}: {error}"))
    })?;

    let kind = match object.get("kind").and_then(Value::as_str) {
        Some("create") => OpKind::Create,
        Some("update") => OpKind::Update,
        Some("delete") => OpKind::Delete,
        Some(other) => {
            return Err(AiHubError::InvalidChangeSet(format!(
                "{position} has kind `{other}`; it is create, update or delete"
            )));
        }
        None => {
            return Err(AiHubError::InvalidChangeSet(format!(
                "{position} names no kind; it is create, update or delete"
            )));
        }
    };

    let resource_type = object
        .get("resource_type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();

    let resource_id = object
        .get("resource_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();

    // A create has no target id and an update or delete must name one. Both directions are
    // refused **here** as well as by the store, because the message the reviewer sees matters:
    // `validate_operation` names the operation by its derived key (`op0:update:9a1c…`), which is
    // the right thing in the editor and the wrong thing in a chat answer where there is no
    // editor yet and no key on screen.
    if kind != OpKind::Create && resource_id.is_empty() {
        return Err(AiHubError::InvalidChangeSet(format!(
            "{position} is an {} with no target id",
            kind.label()
        )));
    }
    if kind == OpKind::Create && !resource_id.is_empty() {
        return Err(AiHubError::InvalidChangeSet(format!(
            "{position} is a create that names a target id (`{resource_id}`); a create has none"
        )));
    }

    let args = match object.get("args") {
        None | Some(Value::Null) => Value::Object(serde_json::Map::new()),
        Some(value) if value.is_object() => value.clone(),
        Some(value) => {
            return Err(AiHubError::InvalidChangeSet(format!(
                "{position} has a {} for `args`; it is an object of field values",
                kind_of(value)
            )));
        }
    };

    Ok(Operation {
        kind,
        resource_type,
        resource_id,
        args,
    })
}

/// Refuse a key the format does not define.
fn reject_unknown(object: &serde_json::Map<String, Value>, allowed: &[&str]) -> Result<()> {
    let unknown = object
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(AiHubError::InvalidChangeSet(format!(
        "unknown field{} {}; this format has {}",
        if unknown.len() == 1 { "" } else { "s" },
        unknown
            .iter()
            .map(|key| format!("`{key}`"))
            .collect::<Vec<_>>()
            .join(", "),
        allowed
            .iter()
            .map(|key| format!("`{key}`"))
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// The name of a JSON value's kind, for a message about a value of the wrong shape.
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    /// A well-formed block, so a test reads as "this one field changed".
    fn block(body: &str) -> String {
        format!("Here is what I would do.\n\n```{FENCE_TAG}\n{body}\n```\n")
    }

    /// "This answer proposed nothing", as an assertion.
    ///
    /// A helper rather than `assert_eq!(parse(x), Ok(None))` because `AiHubError` carries no
    /// `PartialEq` — and the interesting half of that claim is not the value but the *kind* of
    /// result. A panic message that says "expected Ok(None), got Err(InvalidChangeSet(…))" is a
    /// better failure than a compile error would be, and this function names the case: an
    /// answer with no block must be `None`, never a refusal. A parser that refused prose would
    /// make every ordinary chat answer an error.
    #[track_caller]
    fn assert_no_proposal(answer: &str, why: &str) {
        match parse(answer) {
            Ok(None) => {}
            Ok(Some(proposal)) => panic!(
                "{why}: read a proposal of {} operations",
                proposal.operations.len()
            ),
            Err(error) => panic!("{why}: refused with {error}"),
        }
    }

    const ONE: &str = r#"{"title": "Rename the page", "operations": [
        {"kind": "update", "resource_type": "page", "resource_id": "11111111-1111-1111-1111-111111111111",
         "args": {"title": "A better title"}}
    ]}"#;

    #[test]
    fn an_answer_with_no_block_proposes_nothing() {
        assert_no_proposal(
            "The page you mean is the third one; I renamed nothing.",
            "prose must read as 'no proposal', not as a refusal",
        );
    }

    #[test]
    fn an_empty_answer_proposes_nothing() {
        assert_no_proposal("", "an empty answer proposes nothing");
        assert_no_proposal("   \n  ", "a blank answer proposes nothing");
    }

    // **The rule this file exists to defend.** A chat answer routinely quotes an example: a
    // model explaining the format writes a ```json block, and one explaining *this* request
    // writes the fence tag inline in a sentence. Reading either as a proposal means a model can
    // "propose" work by talking about proposing it, and the reviewer gets a set nobody chose.
    #[test]
    fn a_json_block_is_not_a_proposal() {
        let answer = "Here is the shape:\n\n```json\n{\"title\": \"x\", \"operations\": []}\n```";
        assert_no_proposal(answer, "a ```json fence is not a proposal");
    }

    #[test]
    fn a_text_block_is_not_a_proposal() {
        let answer = "```text\nsome code\n```";
        assert_no_proposal(answer, "a ```text fence is not a proposal");
    }

    #[test]
    fn the_tag_mentioned_inside_a_sentence_does_not_open_a_block() {
        let answer = "You can wrap it in a ```change-set block like this one.";
        assert_no_proposal(
            answer,
            "the fence must be on its own line, or a model can propose work by quoting the format",
        );
    }

    #[test]
    fn a_mention_of_the_tag_in_a_fenced_sentence_is_not_a_block() {
        let answer = "```\nuse a ```change-set block for changes\n```";
        assert_no_proposal(answer, "a mention inside a code block is not a proposal");
    }

    #[test]
    fn a_well_formed_block_becomes_a_keyed_proposal() {
        let parsed = parse(&block(ONE))
            .expect("the block is valid")
            .expect("it proposes");
        assert_eq!(parsed.title, "Rename the page");
        assert_eq!(parsed.operations.len(), 1);

        let op = &parsed.operations[0];
        assert_eq!(op.operation.kind, OpKind::Update);
        assert_eq!(op.operation.resource_type, "page");
        assert_eq!(
            op.operation.resource_id, "11111111-1111-1111-1111-111111111111",
            "the target survives the round trip"
        );
        assert_eq!(op.operation.args["title"], json!("A better title"));
        assert!(
            op.key.starts_with("op0:update:"),
            "the key must be the same content-derived shape `keys_for` produces, not a uuid: {}",
            op.key
        );
    }

    #[test]
    fn the_key_is_the_one_keys_for_produces() {
        let parsed = parse(&block(ONE)).expect("valid").expect("a proposal");
        let expected = keys_for(
            &parsed
                .operations
                .iter()
                .map(|op| op.operation.clone())
                .collect::<Vec<_>>(),
        );
        let got = parsed
            .operations
            .iter()
            .map(|op| op.key.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            got, expected,
            "a proposal and a hand-filed set must carry the same keys, or the editor addresses one and not the other"
        );
    }

    // A model's whole answer can be an unterminated block, and a truncated answer must not be
    // read as a proposal. The failure this prevents is quiet and nasty: the rest of the
    // conversation is scanned for JSON and a set is filed carrying the next turn's text.
    #[test]
    fn an_unterminated_block_proposes_nothing() {
        let answer = "```change-set\n{\"title\": \"cut off";
        assert_no_proposal(answer, "a truncated fence is not a proposal");
    }

    #[test]
    fn a_second_block_is_refused_rather_than_picking_one() {
        let answer = format!(
            "{}{}",
            block(ONE),
            block(&ONE.replace("Rename the page", "And another"))
        );
        let err = parse(&answer).expect_err("two blocks is not one proposal");
        let message = err.to_string();
        assert!(
            message.contains('2') && message.contains("one change set at a time"),
            "the refusal must say how many and what to do instead: {message}"
        );
    }

    #[test]
    fn a_block_that_is_not_json_is_refused_with_the_reason() {
        let err = parse(&block("{ not json")).expect_err("a block is a claim");
        let message = err.to_string();
        assert!(
            message.contains(FENCE_TAG) && message.contains("not JSON"),
            "the refusal names the block and the problem: {message}"
        );
    }

    #[test]
    fn a_block_that_is_a_list_is_refused() {
        let err = parse(&block(r#"[{"title": "x"}]"#)).expect_err("a list is not a proposal");
        assert!(err.to_string().contains("must be a JSON object"), "{}", err);
    }

    // A misspelled key is a mistake the model made, and the reviewer needs to be told which one.
    // Reading zero operations out of it instead would file an empty set, or a set built from
    // whatever survived while the reviewer believes they read the whole proposal.
    #[test]
    fn a_misspelled_field_is_refused_rather_than_ignored() {
        let err = parse(&block(
            r#"{"title": "t", "operation": [{"kind": "delete", "resource_type": "page",
                "resource_id": "11111111-1111-1111-1111-111111111111"}]}"#,
        ))
        .expect_err("`operation` is not a field of this format");
        let message = err.to_string();
        assert!(
            message.contains("`operation`") && message.contains("`operations`"),
            "the refusal names the field and the spelling: {message}"
        );
    }

    #[test]
    fn a_misspelled_operation_field_names_the_operation() {
        let err = parse(&block(
            r#"{"title": "t", "operations": [
                {"kind": "update", "resource_type": "page",
                 "resource_id": "11111111-1111-1111-1111-111111111111", "arg": {"title": "x"}}]}"#,
        ))
        .expect_err("`arg` is not a field of an operation");
        let message = err.to_string();
        assert!(
            message.contains("operation 1:") && message.contains("`arg`"),
            "the refusal must say WHICH operation and which field: {message}"
        );
    }

    #[test]
    fn a_title_that_is_missing_or_blank_is_refused() {
        for body in [
            r#"{"operations": [{"kind": "delete", "resource_type": "page",
                "resource_id": "11111111-1111-1111-1111-111111111111"}]}"#,
            r#"{"title": "   ", "operations": [{"kind": "delete", "resource_type": "page",
                "resource_id": "11111111-1111-1111-1111-111111111111"}]}"#,
        ] {
            let err = parse(&block(body)).expect_err("a proposal needs a title");
            assert!(err.to_string().contains("title"), "{}", err);
        }
    }

    #[test]
    fn an_over_long_title_is_refused_with_the_limit() {
        let long = "x".repeat(MAX_TITLE_CHARS + 1);
        let err = parse(&block(
            &json!({
                "title": long,
                "operations": [{"kind": "delete", "resource_type": "page",
                    "resource_id": "11111111-1111-1111-1111-111111111111"}]
            })
            .to_string(),
        ))
        .expect_err("the title is over the limit");
        assert!(
            err.to_string().contains(&MAX_TITLE_CHARS.to_string()),
            "{}",
            err
        );
    }

    #[test]
    fn an_empty_operations_list_is_refused() {
        let err = parse(&block(r#"{"title": "t", "operations": []}"#))
            .expect_err("an empty set is not a proposal");
        assert!(
            err.to_string().contains("at least one operation"),
            "{}",
            err
        );
    }

    #[test]
    fn a_missing_operations_list_is_refused() {
        let err = parse(&block(r#"{"title": "t"}"#)).expect_err("there are no operations");
        assert!(err.to_string().contains("operations"), "{}", err);
    }

    #[test]
    fn an_operations_field_that_is_not_a_list_is_refused() {
        let err = parse(&block(
            r#"{"title": "t", "operations": {"kind": "delete"}}"#,
        ))
        .expect_err("a single operation is not a list");
        assert!(err.to_string().contains("`operations` list"), "{}", err);
    }

    #[test]
    fn an_over_long_operations_list_is_refused_with_the_limit() {
        let one = json!({"kind": "delete", "resource_type": "page",
                         "resource_id": "11111111-1111-1111-1111-111111111111"});
        let body = json!({
            "title": "t",
            "operations": vec![one; MAX_OPERATIONS + 1],
        })
        .to_string();
        let err = parse(&block(&body)).expect_err("over the limit");
        assert!(
            err.to_string().contains(&MAX_OPERATIONS.to_string()),
            "{}",
            err
        );
    }

    #[test]
    fn an_unknown_kind_is_refused_with_the_three_legal_ones() {
        for kind in ["archive", "Update", "upsert", ""] {
            let body = json!({
                "title": "t",
                "operations": [{"kind": kind, "resource_type": "page",
                                 "resource_id": "11111111-1111-1111-1111-111111111111"}],
            })
            .to_string();
            let err = parse(&block(&body)).expect_err("the kind is not one of the three");
            assert!(
                err.to_string().contains("create, update or delete"),
                "kind `{kind}`: {}",
                err
            );
        }
    }

    #[test]
    fn a_missing_kind_is_refused() {
        let err = parse(&block(
            r#"{"title": "t", "operations": [{"resource_type": "page",
                "resource_id": "11111111-1111-1111-1111-111111111111"}]}"#,
        ))
        .expect_err("no kind is no operation");
        assert!(err.to_string().contains("names no kind"), "{}", err);
    }

    // The two directions, both refused by the store — and the parser runs the store's rules so
    // the two cannot disagree. A set carrying a create with a target id is one the applier
    // would refuse at confirm, with the reviewer already holding it.
    #[test]
    fn a_delete_with_no_target_is_refused() {
        let err = parse(&block(
            r#"{"title": "t", "operations": [{"kind": "delete", "resource_type": "page"}]}"#,
        ))
        .expect_err("a delete with no target is not an operation");
        assert!(err.to_string().contains("no target id"), "{}", err);
    }

    #[test]
    fn a_create_that_names_a_target_is_refused() {
        let err = parse(&block(
            r#"{"title": "t", "operations": [{"kind": "create", "resource_type": "page",
                "resource_id": "11111111-1111-1111-1111-111111111111"}]}"#,
        ))
        .expect_err("a create has no target");
        assert!(
            err.to_string().contains("a create has none"),
            "the message must say what to change: {}",
            err
        );
    }

    // A `create` with no id is legal — `validate_operation` says so — and the parser must not
    // add a rule of its own. A test that only ever proposed deletes would never notice.
    #[test]
    fn a_create_with_no_target_is_accepted() {
        let parsed = parse(&block(
            r#"{"title": "New page", "operations": [
                {"kind": "create", "resource_type": "page", "args": {"title": "Fresh"}}]}"#,
        ))
        .expect("a create is legal")
        .expect("it proposes");
        assert_eq!(parsed.operations[0].operation.kind, OpKind::Create);
        assert!(parsed.operations[0].operation.resource_id.is_empty());
    }

    // **The rule the store owns, run from the store.** `resource_type` is the rule that decides
    // whether the applier has a reader at all; a parser that accepted `theme` would file a set
    // no preview can ever resolve.
    #[test]
    fn a_resource_type_this_build_cannot_apply_is_refused() {
        let err = parse(&block(
            r#"{"title": "t", "operations": [{"kind": "update", "resource_type": "theme",
                "resource_id": "11111111-1111-1111-1111-111111111111"}]}"#,
        ))
        .expect_err("there is no theme reader");
        assert!(
            err.to_string().contains("pages only"),
            "the refusal must come from the store's own rule: {}",
            err
        );
    }

    #[test]
    fn a_missing_resource_type_is_refused() {
        let err = parse(&block(
            r#"{"title": "t", "operations": [{"kind": "update",
                "resource_id": "11111111-1111-1111-1111-111111111111"}]}"#,
        ))
        .expect_err("an operation with no resource type names nothing");
        assert!(err.to_string().contains("resource type"), "{}", err);
    }

    #[test]
    fn args_that_are_not_an_object_are_refused() {
        let err = parse(&block(
            r#"{"title": "t", "operations": [{"kind": "update", "resource_type": "page",
                "resource_id": "11111111-1111-1111-1111-111111111111", "args": ["title"]}]}"#,
        ))
        .expect_err("a list of field names is not field values");
        assert!(err.to_string().contains("`args`"), "{}", err);
    }

    #[test]
    fn missing_args_become_an_empty_object_rather_than_a_refusal() {
        // A delete writes no field, so "no args" is the honest shape for one — refusing it
        // would make the one operation that legitimately carries no values impossible to
        // propose.
        let parsed = parse(&block(
            r#"{"title": "t", "operations": [{"kind": "delete", "resource_type": "page",
                "resource_id": "11111111-1111-1111-1111-111111111111"}]}"#,
        ))
        .expect("a delete proposes no values")
        .expect("it proposes");
        assert_eq!(
            parsed.operations[0].operation.args,
            json!({}),
            "absent args are an empty object, not a null the writer has to special-case"
        );
    }

    #[test]
    fn a_three_operation_proposal_keeps_its_order() {
        let id = "11111111-1111-1111-1111-111111111111";
        let other = "22222222-2222-2222-2222-222222222222";
        let parsed = parse(&block(&format!(
            r#"{{"title": "Three", "operations": [
                {{"kind": "update", "resource_type": "page", "resource_id": "{id}",
                  "args": {{"title": "First"}}}},
                {{"kind": "create", "resource_type": "page", "args": {{"title": "Second"}}}},
                {{"kind": "delete", "resource_type": "page", "resource_id": "{other}"}}]}}"#
        )))
        .expect("valid")
        .expect("it proposes");
        let kinds = parsed
            .operations
            .iter()
            .map(|op| op.operation.kind)
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![OpKind::Update, OpKind::Create, OpKind::Delete],
            "the order a model proposed is the order the reviewer reads and the order they apply"
        );
    }

    #[test]
    fn the_instruction_and_the_parser_agree_on_the_tag() {
        // The two drift silently: a prompt that says one tag and a parser that reads another
        // produces a conversation that proposes nothing, with no error anywhere.
        assert!(
            system_instruction().contains(FENCE_TAG),
            "the instruction must name the tag the parser reads"
        );
        assert!(
            system_instruction().contains("create, update or delete"),
            "the instruction must name the kinds the parser accepts"
        );
        assert!(
            system_instruction().contains("page"),
            "the instruction must name the resource type the store can apply"
        );
    }

    #[test]
    fn a_parsed_set_survives_the_store_validate() {
        // Belt and braces, and the specific regression worth pinning: the parser runs the
        // store's rules itself, so anything it returns is a set `validate` accepts.
        let parsed = parse(&block(ONE)).expect("valid").expect("a proposal");
        let draft = ChangeSet {
            id: uuid::Uuid::nil(),
            organization_id: uuid::Uuid::nil(),
            site_id: None,
            title: parsed.title.clone(),
            status: change_sets::INITIAL_STATUS.to_owned(),
            operations: parsed.operations.clone(),
            base_revisions: Default::default(),
            content_hash: String::new(),
            created_by: None,
            created_by_agent: None,
            created_by_run: None,
            updated_by: None,
            confirmed_at: None,
            applied_at: None,
            discarded_reason: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        change_sets::validate(&draft).expect("a parsed set is a valid set");
    }
}
