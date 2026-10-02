//! The typed artifact generator (docs/requests/REQ-045, slice 3).
//!
//! One sentence in, typed artifacts out: entities, fields, UI, permissions, a role, a
//! workflow, notification templates and a report. Slice 1 wrote the store and the
//! validators, slice 2 wrote the review surface — and both of those were **unreachable**
//! from a prompt, because nothing ever asked a provider for anything. `POST /generate` was
//! registered and answered with a failure that named its own absence, which is the same
//! defect as a button that says "coming soon" wearing a status code.
//!
//! This file is that missing half, and it is deliberately the **unglamorous** half:
//!
//! * **The prompt is fixed and the answer is untrusted.** The schema lives in
//!   [`schema_prompt`] as a literal, so a model cannot be talked into a different plan
//!   format by the operator's own prompt. Everything the answer says is then read through
//!   [`normalize`], which invents nothing: a missing artifact is absent from the plan, and
//!   the blockers list names it.
//! * **Normalization is repair, not invention.** A model that writes `Kind: "text"` or
//!   `"String"` or `"TEXT"` meant `text`, and refusing the plan over a capital letter would
//!   make the feature useless in practice. But a model that invents a *type* the platform
//!   cannot render is not silently downgraded to `text` — that would produce a plan whose
//!   field silently stores something else. Those two are different repairs, and the code
//!   says so.
//! * **The reason a repair happened is not thrown away.** Every rewrite lands in the
//!   artifact's `rationale` as a sentence the reviewer reads, because a plan whose field
//!   types were silently changed is a plan whose reviewer cannot tell what they approved.
//! * **One provider call, no streaming retry.** The store is the authority on what landed;
//!   a stream is a progress report, not a source of truth. A partial answer is a failed
//!   plan with a reason, never a plan with half its artifacts and a green status.

use serde_json::{Value, json};

use crate::model::{NewArtifact, REQUIRED_KINDS};
use crate::validate::{FIELD_TYPES, validate_key};

/// Most artifacts one answer may produce, per kind.
///
/// A cap and not a hope: the answer is untrusted input, and a model that loops on one kind
/// would otherwise write as many rows as it had tokens. The cap is reported as a finding on
/// the artifact that was refused rather than as a silent truncation — a plan that quietly
/// dropped its tenth field is a plan nobody can trust.
const MAX_ARTIFACTS_PER_KIND: usize = 24;

/// The instruction the generator sends.
///
/// A literal rather than something built from the prompt: the schema *is* the contract, and
/// a caller who could append to it could describe a different application format and have it
/// stored as if the platform had asked for it. The operator's sentence goes in the user
/// turn, where it belongs.
#[must_use]
pub fn schema_prompt(prompt: &str) -> String {
    format!(
        "You design enterprise applications for a platform. Answer with ONE JSON object and \
         nothing else — no prose, no markdown, no code fence.\n\
         \n\
         The object is a plan:\n\
         {{\n\
         \x20 \"title\": \"short human name\",\n\
         \x20 \"artifacts\": [\n\
         \x20   {{\"kind\": \"entity\", \"key\": \"leave_request\", \"spec\": {{\"label\": \"Leave \
         request\", \"plural_label\": \"Leave requests\"}}, \"rationale\": \"why\"}},\n\
         \x20   {{\"kind\": \"field\", \"key\": \"leave_type\", \"parent_key\": \"leave_request\", \
         \"spec\": {{\"key\": \"leave_type\", \"label\": \"Leave type\", \"type\": \"enum\", \
         \"options\": [\"annual\", \"sick\"]}}, \"rationale\": \"why\"}},\n\
         \x20   {{\"kind\": \"ui\", \"key\": \"leave_request_list\", \"parent_key\": \
         \"leave_request\", \"spec\": {{\"screen\": \"list\", \"columns\": [\"leave_type\"]}}, \
         \"rationale\": \"why\"}},\n\
         \x20   {{\"kind\": \"permission\", \"key\": \"leave_request.read\", \
         \"parent_key\": \"leave_request\", \"spec\": {{\"key\": \"leave_request.read\", \
         \"description\": \"Read leave requests\"}}, \"rationale\": \"why\"}},\n\
         \x20   {{\"kind\": \"role\", \"key\": \"leave_approver\", \"spec\": {{\"name\": \"Leave \
         approver\", \"permissions\": [\"leave_request.read\", \"leave_request.update\"]}}, \
         \"rationale\": \"why\"}},\n\
         \x20   {{\"kind\": \"workflow\", \"key\": \"leave_approval\", \
         \"parent_key\": \"leave_request\", \"spec\": {{\"trigger\": \"record.created\", \
         \"steps\": [{{\"name\": \"Notify manager\", \"action\": \"notify\"}}]}}, \
         \"rationale\": \"why\"}},\n\
         \x20   {{\"kind\": \"notification\", \"key\": \"leave_requested\", \
         \"parent_key\": \"leave_approval\", \"spec\": {{\"title\": \"Leave requested\", \
         \"body\": \"A leave request needs approval\", \"channel\": \"in_app\"}}, \
         \"rationale\": \"why\"}},\n\
         \x20   {{\"kind\": \"report\", \"key\": \"leave_request_summary\", \
         \"parent_key\": \"leave_request\", \"spec\": {{\"title\": \"Leave requests\", \
         \"group_by\": \"leave_type\", \"metric\": \"count\"}}, \"rationale\": \"why\"}}\n\
         \x20 ]\n\
         }}\n\
         \n\
         Rules:\n\
         - A `key` is lower_snake_case, starts with a letter, and is the artifact's own name.\n\
         - A `permission` key is `domain.action` in lower_snake_case — it is the one key with a dot.\n\
         - Every artifact carries a `rationale`: one sentence on why it belongs in this app.\n\
         - An `entity` needs at least three `field`s and at least one `ui` screen.\n\
         - An `enum` field carries its `options`.\n\
         - The application is described by the user's request below. Design for that request.\n\
         \n\
         User request:\n{prompt}"
    )
}

/// One artifact as the model's answer proposed it, before any repair.
#[derive(Debug, Clone)]
struct RawArtifact {
    kind: String,
    key: String,
    parent_key: Option<String>,
    spec: Value,
    rationale: String,
}

/// What one answer became.
#[derive(Debug, Clone, Default)]
pub struct Generated {
    /// The name the model gave the plan, if it gave one.
    pub title: Option<String>,
    /// Artifacts, in the order the kinds are applied.
    pub artifacts: Vec<NewArtifact>,
    /// Repairs the platform made to the model's answer, as sentences a reviewer can read.
    pub notes: Vec<String>,
}

/// Turn one model answer into artifacts.
///
/// Every artifact leaves here **with its findings**, and the caller stores it through
/// [`crate::store::insert_artifact`] — which derives the status from them. An artifact the
/// validator refuses is stored as `invalid` and appears in the blockers list; it is not
/// dropped, because "the model proposed something the platform cannot do, and here is what"
/// is the review screen's entire job.
#[must_use]
pub fn normalize(answer: &Value) -> Generated {
    let mut out = Generated {
        title: answer
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_owned),
        ..Generated::default()
    };

    let Some(list) = answer.get("artifacts").and_then(Value::as_array) else {
        out.notes.push(
            "the answer carried no `artifacts` list, so the plan is empty; the prompt asks for one"
                .to_owned(),
        );
        return out;
    };

    let mut per_kind: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    // A (kind, key) pair already written: two artifacts with one name is a collision apply
    // could only resolve by guessing, so the second one is refused by name.
    let mut seen: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();

    for (index, entry) in list.iter().enumerate() {
        let path = format!("artifacts[{index}]");
        let Some(raw) = read_raw(entry, &path, &mut out.notes) else {
            continue;
        };

        let taken = per_kind.entry(raw.kind.clone()).or_insert(0);
        if *taken >= MAX_ARTIFACTS_PER_KIND {
            out.notes.push(format!(
                "`{}` artifact `{}` was refused: a plan may hold at most {MAX_ARTIFACTS_PER_KIND} \
                 artifacts of one kind, and the answer offered more",
                raw.kind, raw.key
            ));
            continue;
        }
        *taken += 1;

        if !seen.insert((raw.kind.clone(), raw.key.clone())) {
            out.notes.push(format!(
                "`{}` artifact `{}` was refused: the answer offered the same key twice, and two \
                 artifacts may not share one name",
                raw.kind, raw.key
            ));
            continue;
        }

        let artifact = build(&raw, &path, &mut out.notes);
        out.artifacts.push(artifact);
    }

    // The order the apply runner walks, so the plan's artifact ids read in the order they
    // are created rather than in the order the model happened to mention them.
    out.artifacts
        .sort_by_key(|artifact| kind_rank(&artifact.kind).unwrap_or(usize::MAX));

    out
}

/// The kinds apply needs, missing from the plan, named.
///
/// The same list the store's `required_kinds_present` uses, reported here so a caller can
/// tell "the model proposed nothing for the report" from "the platform dropped it".
#[must_use]
pub fn missing_required_kinds(artifacts: &[NewArtifact]) -> Vec<String> {
    let present: std::collections::BTreeSet<&str> =
        artifacts.iter().map(|a| a.kind.as_str()).collect();
    REQUIRED_KINDS
        .iter()
        .filter(|kind| !present.contains(*kind))
        .map(|kind| (*kind).to_owned())
        .collect()
}

/// Where a kind sits in the apply order.
#[must_use]
pub fn kind_rank(kind: &str) -> Option<usize> {
    crate::model::KINDS.iter().position(|known| *known == kind)
}

// ---------------------------------------------------------------------------------------------
// Reading the answer
// ---------------------------------------------------------------------------------------------

/// Read one entry as a [`RawArtifact`], or record why it is not one.
fn read_raw(entry: &Value, path: &str, notes: &mut Vec<String>) -> Option<RawArtifact> {
    let Some(object) = entry.as_object() else {
        notes.push(format!(
            "{path} was refused: an artifact is an object with `kind`, `key`, `spec` and \
             `rationale`, and this was {}",
            shape_of(entry)
        ));
        return None;
    };

    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let Some(kind) = canonical_kind(&kind) else {
        notes.push(format!(
            "{path} was refused: `{}` is not an artifact kind this platform builds; the kinds \
             are {}",
            object
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("(nothing)"),
            REQUIRED_KINDS.join(", ")
        ));
        return None;
    };

    let key = object
        .get("key")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_owned();
    if key.is_empty() {
        notes.push(format!(
            "{path} was refused: it names no `key`, so nothing could refer to it"
        ));
        return None;
    }

    let parent_key = object
        .get("parent_key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|parent| !parent.is_empty())
        .map(str::to_owned);

    // A spec that is not an object is replaced rather than refused: every validator reads
    // named fields out of it, and an artifact with no body cannot be reviewed at all. What
    // it was is named in the note.
    let raw_spec = object.get("spec").cloned().unwrap_or_else(|| json!({}));
    let (spec, replaced) = match raw_spec.as_object() {
        Some(_) => (raw_spec, false),
        None => {
            notes.push(format!(
                "`{kind}` artifact `{key}` had a {}-shaped body and was given an empty one; \
                 the fields it needed could not be read from a {}",
                shape_of(&raw_spec),
                shape_of(&raw_spec)
            ));
            (json!({}), true)
        }
    };

    let rationale = object
        .get("rationale")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_owned();

    Some(RawArtifact {
        kind,
        key,
        parent_key,
        spec,
        rationale: if rationale.is_empty() && !replaced {
            // Empty is NOT repaired. The validator's "the artifact carries no rationale —
            // nothing to review" finding is the review screen's job to surface, and
            // inventing one here would erase the difference between a model that explained
            // itself and one that did not.
            rationale
        } else {
            rationale
        },
    })
}

/// The kind as the platform spells it, when the model meant one of ours.
///
/// Spelling only — `workflow_definition`, `screen`, `screens` and `list` all *mean* a kind,
/// but guessing which is a guess about intent, and a wrong guess writes a live artifact the
/// operator never proposed. Case and whitespace are not intent.
fn canonical_kind(raw: &str) -> Option<String> {
    let collapsed = raw.trim().to_ascii_lowercase().replace([' ', '-'], "_");
    crate::model::KINDS
        .iter()
        .find(|known| **known == collapsed)
        .map(|known| (*known).to_owned())
}

// ---------------------------------------------------------------------------------------------
// Repairing one artifact
// ---------------------------------------------------------------------------------------------

/// Turn a raw entry into the artifact that will be stored and reviewed.
fn build(raw: &RawArtifact, path: &str, notes: &mut Vec<String>) -> NewArtifact {
    let mut repairs: Vec<String> = Vec::new();
    let mut spec = raw.spec.clone();

    let key = match repair_key(&raw.kind, &raw.key) {
        Repaired::Same(key) => key,
        Repaired::Fixed { key, note } => {
            repairs.push(note);
            key
        }
    };

    // The parent is repaired with **the same rule the parent's own key was**, which is not
    // the rule this artifact's key used: a parent is always an entity or a workflow, so the
    // permission path's dot handling would be wrong for it.
    //
    // This is not bookkeeping. Repairing the key and leaving the parent alone produces a
    // field whose `parent_key` names an artifact that does not exist in the plan — the
    // review tree shows the field loose and the apply runner has nothing to attach it to.
    // The walk caught exactly that: the entity was repaired to `leave_request` and its
    // field still pointed at `Leave Request`.
    let parent_key = raw
        .parent_key
        .as_deref()
        .map(|parent| {
            match repair_key("entity", parent) {
                Repaired::Same(repaired) => {
                    if repaired != parent {
                        repairs.push(format!(
                            "the `{parent}` it belongs to was read as `{repaired}`"
                        ));
                    }
                    Some(repaired)
                }
                // `repair_key` for a non-permission kind always yields `Same`; the arm exists so
                // that a future change to the permission path cannot silently make a parent key
                // un-repaired.
                Repaired::Fixed { key, note } => {
                    repairs.push(note);
                    Some(key)
                }
            }
        })
        .unwrap_or(None);

    // `spec.key` must agree with the plan's key: the validator reports disagreement, and a
    // repair here would hide a real ambiguity behind a quiet edit. The two are the same
    // string in every well-formed answer, so writing it in is bookkeeping, not a rewrite.
    if let Some(object) = spec.as_object_mut() {
        let current = object.get("key").and_then(Value::as_str).map(str::to_owned);
        match current {
            Some(existing) if existing == key => {}
            _ => {
                object.insert("key".to_owned(), Value::String(key.clone()));
                if current.is_some() {
                    repairs.push(format!(
                        "the body called itself `{key}`'s old name `{}`; the plan's key is the one \
                         apply uses",
                        current.unwrap_or_default()
                    ));
                }
            }
        }
    }

    // Field types are the one place a repair can change behaviour, so it is the one place
    // that is loud.
    if raw.kind == "field" {
        if let Some((was, now)) = repair_field_type(&mut spec, path) {
            repairs.push(format!("the field type `{was}` was read as `{now}`"));
        }
    }

    // A field with no type cannot be stored as something the platform can render, and
    // defaulting it to `text` would silently turn a number into a string. It is refused by
    // the validator instead, which is the honest half.
    let rationale = if repairs.is_empty() {
        raw.rationale.clone()
    } else {
        let mut text = raw.rationale.clone();
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str("Platform adjustments: ");
        text.push_str(&repairs.join("; "));
        text.push('.');
        for note in &repairs {
            notes.push(format!("`{}` artifact `{key}`: {note}", raw.kind));
        }
        text
    };

    NewArtifact {
        kind: raw.kind.clone(),
        key,
        parent_key,
        ordinal: 0,
        spec,
        rationale,
        validation: Value::Array(Vec::new()),
    }
}

enum Repaired {
    Same(String),
    Fixed { key: String, note: String },
}

/// The artifact key as the platform can store it.
///
/// A key is the one field where **inventing** is the right repair and a mis-cased model's
/// answer would otherwise make the whole plan unreviewable: `Leave Request` and
/// `leave-request` both mean `leave_request`, and no reviewer wants to be told their model
/// used a hyphen. The repair is only ever *toward* the nearest legal spelling of the words
/// the model wrote — it never picks a different word, and it never invents a key for an
/// artifact that named none.
fn repair_key(kind: &str, raw: &str) -> Repaired {
    let trimmed = raw.trim();
    let lowered = trimmed.to_ascii_lowercase();

    // A permission key is `domain.action`: it keeps its dot, and each half is normalized
    // separately so `Leave-Request.Read` becomes `leave_request.read` rather than losing
    // its shape.
    if kind == "permission" {
        return match lowered.split_once('.') {
            Some((domain, action)) => {
                let domain = slug_of(domain);
                let action = slug_of(action);
                let key = format!("{domain}.{action}");
                if key == trimmed {
                    Repaired::Same(key)
                } else {
                    let note = format!("`{trimmed}` was read as `{key}`");
                    Repaired::Fixed { key, note }
                }
            }
            // No dot: the validator reports "not a domain.action key" by name, and adding
            // an action the model did not choose would be inventing part of a permission.
            None => Repaired::Same(lowered),
        };
    }

    let key = slug_of(trimmed);
    if key == trimmed {
        Repaired::Same(key)
    } else {
        let note = format!("`{trimmed}` was read as `{key}`");
        Repaired::Fixed { key, note }
    }
}

/// `words` → `words`, keeping only what a key may contain.
///
/// Unicode letters and digits are kept as they are: the platform's naming rule is
/// `^[a-z][a-z0-9_]*$`, and a Turkish or German word is a letter the model meant. Anything
/// that is not a letter, a digit or a separator becomes a separator, and runs of separators
/// collapse to one underscore.
fn slug_of(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_separator = false;
    for character in raw.chars() {
        if character.is_alphanumeric() {
            if pending_separator && !out.is_empty() {
                out.push('_');
            }
            pending_separator = false;
            for lowered in character.to_lowercase() {
                out.push(lowered);
            }
        } else {
            pending_separator = true;
        }
    }
    let trimmed = out.trim_matches('_').to_owned();
    // A key that starts with a digit or an underscore is not a key; the leading separator is
    // dropped rather than prefixed, because `0_days` means `days` and `_0_days` means
    // something the model did not write.
    let without_leading = trimmed.trim_start_matches(|c: char| c == '_' || c.is_ascii_digit());
    if without_leading.is_empty() {
        // Nothing usable survived — digits only, or punctuation only. The validator reports
        // the empty key by name; inventing words here would be the one repair that decides
        // what the artifact *is*.
        return trimmed;
    }
    without_leading.to_owned()
}

/// Read a field's `type` the way the platform spells it.
///
/// Returns the before/after when it changed anything, so the caller can say so out loud. A
/// type the platform genuinely cannot render is **not** repaired: `photo` is not `text`, and
/// downgrading it would build a plan that stores something other than what was asked for.
/// The validator names it and the reviewer decides.
fn repair_field_type(spec: &mut Value, path: &str) -> Option<(String, String)> {
    let object = spec.as_object_mut()?;
    let raw = object
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .map(str::to_owned)?;
    let lowered = raw.to_ascii_lowercase();

    let canonical = match lowered.as_str() {
        "int" | "integer" | "number" | "count" => "integer",
        "float" | "double" | "money" | "currency" | "decimal" | "number_decimal" => "decimal",
        "bool" | "boolean" | "toggle" | "checkbox" => "boolean",
        "date" => "date",
        "datetime" | "timestamp" | "datetime_local" => "timestamp",
        "enum" | "select" | "choice" | "options" => "enum",
        "ref" | "reference" | "relation" | "fk" => "reference",
        other => {
            if FIELD_TYPES.contains(&other) {
                other
            } else {
                // Unknown type: handed back unchanged for the validator to name.
                let _ = path;
                return None;
            }
        }
    };

    if canonical == raw {
        return None;
    }
    object.insert("type".to_owned(), Value::String(canonical.to_owned()));
    Some((raw, canonical.to_owned()))
}

/// What a JSON value is, in words a reviewer can act on.
fn shape_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "object",
    }
}

/// Whether a key needs no repair at all, for the callers that only want the answer.
#[must_use]
pub fn key_is_clean(kind: &str, key: &str) -> bool {
    matches!(repair_key(kind, key), Repaired::Same(existing) if existing == key)
        && validate_key(key, "key").is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::validate_artifact;

    fn artifacts_of(answer: Value) -> Vec<NewArtifact> {
        normalize(&answer).artifacts
    }

    fn kinds_of(answer: Value) -> Vec<String> {
        artifacts_of(answer)
            .into_iter()
            .map(|artifact| artifact.kind)
            .collect()
    }

    fn by_kind(answer: Value, kind: &str) -> NewArtifact {
        artifacts_of(answer)
            .into_iter()
            .find(|artifact| artifact.kind == kind)
            .expect("the answer must hold an artifact of that kind")
    }

    #[test]
    fn the_answer_is_applied_in_the_order_the_runner_walks() {
        // The model lists the report first; apply creates the entity before it, so the plan
        // must not read in the order the answer happened to arrive in.
        let answer = json!({ "artifacts": [
            { "kind": "report", "key": "summary", "spec": {}, "rationale": "why" },
            { "kind": "entity", "key": "vehicle", "spec": { "label": "Vehicle" }, "rationale": "why" },
            { "kind": "field", "key": "plate", "parent_key": "vehicle",
              "spec": { "label": "Plate", "type": "text" }, "rationale": "why" },
        ]});
        assert_eq!(
            kinds_of(answer),
            vec!["entity", "field", "report"],
            "a plan reads in the order apply walks, not the order the model typed"
        );
    }

    #[test]
    fn a_mis_cased_key_is_read_as_its_legal_spelling_and_the_reader_is_told() {
        let answer = json!({ "artifacts": [
            { "kind": "entity", "key": "Leave Request", "spec": { "label": "Leave request" },
              "rationale": "why" },
        ]});
        let artifact = by_kind(answer, "entity");
        assert_eq!(artifact.key, "leave_request");
        assert!(
            artifact
                .rationale
                .contains("`Leave Request` was read as `leave_request`"),
            "a rewrite the reviewer cannot see is a rewrite they did not approve: {}",
            artifact.rationale
        );
        assert!(
            validate_artifact(&artifact).is_empty(),
            "the repaired key is legal, so the plan reviews as clean rather than carrying a \
             finding the reviewer cannot act on: {:?}",
            validate_artifact(&artifact)
        );
    }

    #[test]
    fn a_field_type_is_read_as_the_platform_spelling_and_the_change_is_loud() {
        let answer = json!({ "artifacts": [
            { "kind": "field", "key": "days", "parent_key": "leave_request",
              "spec": { "label": "Days", "type": "Int" }, "rationale": "why" },
        ]});
        let artifact = by_kind(answer, "field");
        assert_eq!(artifact.spec["type"], "integer");
        assert!(
            artifact.rationale.contains("`Int` was read as `integer`"),
            "changing a field's type changes what it stores, so it is stated: {}",
            artifact.rationale
        );
        assert!(
            validate_artifact(&artifact)
                .iter()
                .all(|finding| finding.path != "spec.type"),
            "the repaired type is legal, so the type is not reported twice"
        );
    }

    #[test]
    fn a_type_the_platform_cannot_render_is_never_downgraded_to_text() {
        // The tempting repair: an unknown type becomes `text` and the plan applies. It is
        // exactly the defect this module exists to avoid — a field that stores something
        // other than what was asked for, in a plan nobody was told about.
        let answer = json!({ "artifacts": [
            { "kind": "field", "key": "photo", "parent_key": "vehicle",
              "spec": { "label": "Photo", "type": "photo" }, "rationale": "why" },
        ]});
        let artifact = by_kind(answer, "field");
        assert_eq!(
            artifact.spec["type"], "photo",
            "an unknown type is left exactly as written"
        );
        let findings = validate_artifact(&artifact);
        assert!(
            findings.iter().any(|f| f.path == "spec.type"),
            "and the validator names it: {findings:?}"
        );
    }

    #[test]
    fn a_repaired_key_brings_its_parent_with_it() {
        // The defect this test was written after: the entity's key was repaired to
        // `leave_request` while its field's `parent_key` stayed `Leave Request`, so the plan
        // held a field belonging to an artifact that was not in it. The repair is only
        // complete if the *reference* is repaired the same way as the name it points at.
        let answer = json!({ "artifacts": [
            { "kind": "entity", "key": "Leave Request", "spec": { "label": "Leave request" },
              "rationale": "why" },
            { "kind": "field", "key": "Days", "parent_key": "Leave Request",
              "spec": { "label": "Days", "type": "text" }, "rationale": "why" },
        ]});
        let field = by_kind(answer, "field");
        assert_eq!(field.key, "days");
        assert_eq!(
            field.parent_key.as_deref(),
            Some("leave_request"),
            "the parent is repaired by the same rule the parent's own key used"
        );
        assert!(
            field
                .rationale
                .contains("`Leave Request` was read as `leave_request`"),
            "and the reader is told the field now points elsewhere: {}",
            field.rationale
        );

        // A parent the model spelled correctly is left exactly as written — a repair that
        // fires on every artifact would be indistinguishable from one that fires on none.
        let clean = json!({ "artifacts": [
            { "kind": "entity", "key": "leave_request", "spec": { "label": "L" },
              "rationale": "why" },
            { "kind": "field", "key": "days", "parent_key": "leave_request",
              "spec": { "label": "Days", "type": "text" }, "rationale": "only why" },
        ]});
        let field = by_kind(clean, "field");
        assert_eq!(
            field.rationale, "only why",
            "a plan that needed no repair carries the model's own words and nothing else"
        );
    }

    #[test]
    fn a_permission_key_keeps_its_dot_and_each_half_is_normalized() {
        let answer = json!({ "artifacts": [
            { "kind": "permission", "key": "Leave-Request.Read",
              "spec": { "description": "Read leave requests" }, "rationale": "why" },
        ]});
        let artifact = by_kind(answer, "permission");
        assert_eq!(
            artifact.key, "leave_request.read",
            "a permission key's dot is its vocabulary, not a character to strip"
        );
        assert!(
            validate_artifact(&artifact).is_empty(),
            "and the repaired key passes the permission validator as it stands"
        );
    }

    #[test]
    fn a_key_the_model_named_nothing_for_is_not_invented() {
        let answer = json!({ "artifacts": [
            { "kind": "entity", "spec": { "label": "Vehicle" }, "rationale": "why" },
        ]});
        let generated = normalize(&answer);
        assert!(
            generated.artifacts.is_empty(),
            "a nameless artifact cannot be referred to, so it is refused rather than named"
        );
        assert!(
            generated
                .notes
                .iter()
                .any(|note| note.contains("names no `key`")),
            "and it is refused by name, not silently dropped: {:?}",
            generated.notes
        );
    }

    #[test]
    fn two_artifacts_with_one_key_are_refused_by_name() {
        let answer = json!({ "artifacts": [
            { "kind": "entity", "key": "vehicle", "spec": { "label": "Vehicle" },
              "rationale": "why" },
            { "kind": "entity", "key": "vehicle", "spec": { "label": "Vehicle again" },
              "rationale": "why" },
        ]});
        let generated = normalize(&answer);
        assert_eq!(generated.artifacts.len(), 1, "one key, one artifact");
        assert!(
            generated
                .notes
                .iter()
                .any(|note| note.contains("same key twice")),
            "the refusal names the collision: {:?}",
            generated.notes
        );
    }

    #[test]
    fn a_missing_rationale_is_left_missing_so_the_validator_can_see_it() {
        let answer = json!({ "artifacts": [
            { "kind": "entity", "key": "vehicle", "spec": { "label": "Vehicle" } },
        ]});
        let artifact = by_kind(answer, "entity");
        let findings = validate_artifact(&artifact);
        assert!(
            findings.iter().any(|f| f.path == "rationale"),
            "the module does not invent a rationale: that would erase the difference between \
             a model that explained itself and one that did not"
        );
    }

    #[test]
    fn a_non_object_spec_becomes_an_empty_one_and_says_so() {
        let answer = json!({ "artifacts": [
            { "kind": "entity", "key": "vehicle", "spec": "a vehicle", "rationale": "why" },
        ]});
        let generated = normalize(&answer);
        assert_eq!(generated.artifacts.len(), 1, "the artifact is reviewable");
        assert!(
            generated.artifacts[0].spec.is_object(),
            "a string-shaped body is replaced by an object the validators can read: {:?}",
            generated.artifacts[0].spec
        );
        assert!(
            // `spec.key` is written by `build` on every artifact — the validator reports a
            // `spec.key` that disagrees with the plan's, so an empty object would be a
            // finding the reviewer cannot act on.
            generated.artifacts[0].spec.get("key").is_some(),
            "and it carries the artifact's own name"
        );
        assert!(
            generated
                .notes
                .iter()
                .any(|note| note.contains("string-shaped")),
            "what the body was is named: {:?}",
            generated.notes
        );
    }

    #[test]
    fn a_missing_required_kind_is_named_rather_than_counted() {
        let answer = json!({ "artifacts": [
            { "kind": "entity", "key": "vehicle", "spec": { "label": "Vehicle" },
              "rationale": "why" },
        ]});
        let missing = missing_required_kinds(&artifacts_of(answer));
        assert!(
            missing.contains(&"report".to_owned()) && missing.contains(&"workflow".to_owned()),
            "the list names what is absent: {missing:?}"
        );
        assert_eq!(missing.len(), REQUIRED_KINDS.len() - 1);
    }

    #[test]
    fn the_same_artifact_twice_in_one_answer_is_one_row() {
        let answer = json!({ "artifacts": [
            { "kind": "entity", "key": "vehicle", "spec": { "label": "Vehicle" },
              "rationale": "why" },
        ]});
        assert_eq!(normalize(&answer).artifacts.len(), 1);
        assert_eq!(
            normalize(&answer).artifacts.len(),
            1,
            "and normalizing twice is stable"
        );
    }

    #[test]
    fn a_kind_the_platform_does_not_build_is_refused_by_name() {
        let answer = json!({ "artifacts": [
            { "kind": "dashboard", "key": "overview", "spec": {}, "rationale": "why" },
        ]});
        let generated = normalize(&answer);
        assert!(generated.artifacts.is_empty());
        assert!(
            generated
                .notes
                .iter()
                .any(|note| note.contains("dashboard")),
            "the refusal quotes what the model asked for: {:?}",
            generated.notes
        );
    }

    #[test]
    fn a_body_whose_key_disagrees_with_the_plan_is_corrected_and_said() {
        let answer = json!({ "artifacts": [
            { "kind": "entity", "key": "vehicle", "spec": { "key": "vehicles", "label": "V" },
              "rationale": "why" },
        ]});
        let artifact = by_kind(answer, "entity");
        assert_eq!(
            artifact.spec["key"], "vehicle",
            "the plan's key is the one apply uses"
        );
        assert!(
            validate_artifact(&artifact).is_empty(),
            "so the spec.key disagreement finding never fires"
        );
    }

    #[test]
    fn an_answer_with_no_artifacts_list_says_so_instead_of_returning_an_empty_plan() {
        let generated = normalize(&json!({ "title": "Something" }));
        assert!(generated.artifacts.is_empty());
        assert!(
            generated
                .notes
                .iter()
                .any(|note| note.contains("no `artifacts` list")),
            "an empty plan with no note reads as a successful generation of nothing"
        );
    }
}
