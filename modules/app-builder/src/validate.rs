//! Artifact validation (docs/requests/REQ-045, slice 1).
//!
//! **Model output is untrusted input.** Every key the platform will later write into a live
//! table is validated here, against platform naming rules, before it is stored as anything
//! other than `pending`. The request says it outright: *"validate every key against platform
//! naming rules, never interpolate artifact text into SQL"*.
//!
//! Two decisions are worth keeping:
//!
//! * **Validation records findings; it does not repair.** An artifact carries the model's
//!   body *and* what the platform said about it, because a reviewer has to see the proposal
//!   and the referee's verdict side by side — a validator that silently rewrote the answer
//!   would leave nothing to review.
//! * **A reserved key is invalid, not renamed.** Renaming makes the reviewer's screen show a
//!   key nobody typed and the apply log name something the model never proposed; refusing
//!   names the collision instead.

use serde_json::Value;

use crate::model::{NewArtifact, REQUIRED_KINDS};

/// One thing wrong with an artifact.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Finding {
    /// Where in the artifact the problem is, e.g. `spec.fields[0].key`.
    pub path: String,
    /// What is wrong, in product language.
    pub message: String,
}

impl Finding {
    /// Build a finding.
    #[must_use]
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

/// Keys the platform owns and a generated app may not claim.
///
/// A generated entity named `users` would shadow the identity table; `organizations` the
/// tenancy spine. These are refused by name so the message can say which one, rather than
/// being refused by a prefix rule whose explanation is "that is reserved" and no more.
pub const RESERVED_KEYS: &[&str] = &[
    "user",
    "users",
    "organization",
    "organizations",
    "site",
    "sites",
    "role",
    "roles",
    "permission",
    "permissions",
    "session",
    "sessions",
    "audit",
    "audit_log",
    "workflow",
    "workflows",
    "app_builder_plans",
    "settings",
    "api_key",
    "api_keys",
];

/// Longest a generated key may be. The longest platform identifier is a uuid column name and
/// a longer one is a column nobody will type.
pub const MAX_KEY_LEN: usize = 64;

/// Longest a rationale may be. The review pane renders it beside the artifact, and a
/// rationale longer than this is a model that ignored the instruction to explain.
pub const MAX_RATIONALE_LEN: usize = 2000;

/// Shortest a generated key may be — a single character is not a name.
pub const MIN_KEY_LEN: usize = 2;

/// Longest a rationale may be.: usize = 2000;

/// The field types a generated entity may declare.
///
/// Deliberately a closed list rather than "whatever the database has": a generated app that
/// picked `jsonb` for a date would produce a schema no screen can render and no form can
/// validate.
pub const FIELD_TYPES: &[&str] = &[
    "text",
    "integer",
    "decimal",
    "boolean",
    "date",
    "timestamp",
    "reference",
    "enum",
];

/// Everything wrong with a plan's artifacts, and whether any of it blocks apply.
///
/// `required_missing` is kept beside the findings because the two answer different questions:
/// a finding says *this artifact is malformed*, and a missing required kind says *apply cannot
/// run*. A plan with a perfect entity and no report is not a plan with a broken artifact.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanValidation {
    /// Findings per artifact, keyed `kind/key`.
    pub findings: Vec<(String, Vec<Finding>)>,
    /// Required kinds the plan does not contain at all.
    pub required_missing: Vec<String>,
}

impl PlanValidation {
    /// `true` when nothing is wrong and nothing is missing.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.findings.iter().all(|(_, f)| f.is_empty()) && self.required_missing.is_empty()
    }

    /// The findings for one artifact, or an empty slice when it has none.
    #[must_use]
    pub fn findings_for(&self, artifact: &NewArtifact) -> &[Finding] {
        self.findings
            .iter()
            .find(|(id, _)| id == &artifact_id(&artifact.kind, &artifact.key))
            .map_or(&[], |(_, f)| f.as_slice())
    }
}

/// `"kind/key"` — how a finding list names an artifact.
#[must_use]
pub fn artifact_id(kind: &str, key: &str) -> String {
    format!("{kind}/{key}")
}

/// Validate one artifact on its own.
///
/// Returns the findings in the order a reviewer should read them: identity first (a key that
/// cannot be stored makes everything after it moot), then the body.
#[must_use]
pub fn validate_artifact(artifact: &NewArtifact) -> Vec<Finding> {
    let mut findings = Vec::new();

    // A permission artifact's key is a `domain.action` permission key, and the dot is part of
    // that vocabulary rather than a character the storage rule objects to — so the
    // kind-specific pass owns that key and the generic storage check would report the same
    // mis-casing a second time, for a string it is not actually judging.
    if artifact.kind != "permission" {
        findings.extend(validate_key(&artifact.key, "key"));
    }

    // The artifact's own key is the single source of truth for its name. A `spec.key` that
    // agrees is NOT validated again — reporting the same reserved word twice, once per path,
    // leaves a reviewer choosing which of two true sentences to fix first, and this module
    // already does that check once. A `spec.key` that DISAGREES is a finding of its own:
    // two names for one artifact is a collision apply would have to resolve by guessing.
    if let Some(spec_key) = artifact.spec.get("key").and_then(Value::as_str) {
        if spec_key != artifact.key {
            findings.push(Finding::new(
                "spec.key",
                format!(
                    "the body calls this artifact `{spec_key}` while the plan lists it as `{}`; \
                     the plan's key is the one apply uses",
                    artifact.key
                ),
            ));
        }
    }

    if artifact.rationale.trim().is_empty() {
        findings.push(Finding::new(
            "rationale",
            "the artifact carries no rationale — nothing to review",
        ));
    } else if artifact.rationale.chars().count() > MAX_RATIONALE_LEN {
        findings.push(Finding::new(
            "rationale",
            format!(
                "the rationale is {} characters; the limit is {MAX_RATIONALE_LEN}",
                artifact.rationale.chars().count()
            ),
        ));
    }

    // Every artifact's body is an object — the migration's check refuses anything else, and a
    // refusal there would read as a crash rather than as a validation finding.
    if !artifact.spec.is_object() {
        findings.push(Finding::new(
            "spec",
            "the artifact body is not an object, so it has no fields to read",
        ));
        return findings;
    }

    findings.extend(match artifact.kind.as_str() {
        "entity" => validate_entity(artifact),
        "field" => validate_field(artifact),
        "permission" => validate_permission(artifact),
        "workflow" => validate_workflow(artifact),
        _ => Vec::new(),
    });

    findings
}

/// Validate a whole plan, and report the required kinds it never produced.
///
/// The per-artifact findings come from [`validate_artifact`] — one rule, applied per row —
/// and the missing-kinds pass is separate because it is a property of the *set*, not of any
/// artifact. A plan is only applicable when both are quiet.
#[must_use]
pub fn validate_plan(artifacts: &[NewArtifact]) -> PlanValidation {
    let findings = artifacts
        .iter()
        .map(|artifact| {
            (
                artifact_id(&artifact.kind, &artifact.key),
                validate_artifact(artifact),
            )
        })
        .collect();

    let present_kinds: Vec<&str> = artifacts.iter().map(|a| a.kind.as_str()).collect();
    let required_missing = REQUIRED_KINDS
        .iter()
        .filter(|kind| !present_kinds.contains(kind))
        .map(|kind| (*kind).to_owned())
        .collect();

    PlanValidation {
        findings,
        required_missing,
    }
}

/// A key the platform will store: lowercase snake_case, no reserved word.
#[must_use]
pub fn validate_key(key: &str, path: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    let trimmed = key.trim();

    if trimmed.len() < MIN_KEY_LEN {
        findings.push(Finding::new(
            path,
            format!("`{key}` is shorter than the {MIN_KEY_LEN}-character minimum"),
        ));
        return findings;
    }
    if trimmed.chars().count() > MAX_KEY_LEN {
        findings.push(Finding::new(
            path,
            format!(
                "`{}` is {} characters; the limit is {MAX_KEY_LEN}",
                key.trim(),
                trimmed.chars().count()
            ),
        ));
        return findings;
    }
    if trimmed != key {
        findings.push(Finding::new(
            path,
            "the key has leading or trailing whitespace, which the platform will not store",
        ));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        findings.push(Finding::new(
            path,
            format!(
                "`{key}` must be lowercase ASCII letters, digits and underscores — it becomes a column, a route and a permission key"
            ),
        ));
    } else if trimmed.starts_with('_') || trimmed.ends_with('_') {
        findings.push(Finding::new(
            path,
            format!("`{key}` starts or ends with an underscore"),
        ));
    } else if trimmed.contains("__") {
        findings.push(Finding::new(
            path,
            format!("`{key}` contains a double underscore"),
        ));
    }

    // The reserved check runs on the trimmed name and after the shape check on purpose: a key
    // that is not even a legal key is reported once, with the shape as the reason, rather
    // than twice with the reserved word as a second one.
    if RESERVED_KEYS.contains(&trimmed) {
        findings.push(Finding::new(
            path,
            format!("`{key}` is a reserved platform key and cannot name a generated artifact"),
        ));
    }

    findings
}

fn validate_entity(artifact: &NewArtifact) -> Vec<Finding> {
    let mut findings = Vec::new();

    let label = artifact
        .spec
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if label.trim().is_empty() {
        findings.push(Finding::new(
            "spec.label",
            "the entity has no label to show",
        ));
    }

    if let Some(plural) = artifact.spec.get("plural_label").and_then(Value::as_str) {
        if plural.trim().is_empty() {
            findings.push(Finding::new(
                "spec.plural_label",
                "the plural label is empty; the list screen's heading would be blank",
            ));
        }
    }

    findings
}

fn validate_field(artifact: &NewArtifact) -> Vec<Finding> {
    let mut findings = Vec::new();

    // The key was checked above (and required to agree with the plan's own key), so this
    // pass does not check it again — it reads the label and the type.
    if artifact.spec.get("key").and_then(Value::as_str).is_none() {
        findings.push(Finding::new("spec.key", "the field body names no key"));
    }

    // A field's parent is what turns a column into a column *of something*. Without it the
    // apply runner has nothing to attach the field to, and the review tree shows it loose.
    match artifact.parent_key.as_deref() {
        None => findings.push(Finding::new(
            "parent_key",
            "the field names no entity to belong to",
        )),
        Some(parent) if parent.trim().is_empty() => {
            findings.push(Finding::new(
                "parent_key",
                "the field's entity is an empty string",
            ));
        }
        Some(_) => {}
    }

    let Some(label) = artifact.spec.get("label").and_then(Value::as_str) else {
        findings.push(Finding::new("spec.label", "the field has no label"));
        return findings;
    };
    if label.trim().is_empty() {
        findings.push(Finding::new("spec.label", "the field's label is empty"));
    }

    let Some(field_type) = artifact.spec.get("type").and_then(Value::as_str) else {
        findings.push(Finding::new("spec.type", "the field has no type"));
        return findings;
    };
    if !FIELD_TYPES.contains(&field_type) {
        findings.push(Finding::new(
            "spec.type",
            format!(
                "`{field_type}` is not a field type the platform can render; use one of: {}",
                FIELD_TYPES.join(", ")
            ),
        ));
    }
    // A required reference with no target is a form that cannot be filled in. Caught here
    // rather than at apply, because apply must never be the first place this is discovered.
    if field_type == "reference" {
        let target = artifact
            .spec
            .get("references")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if target.trim().is_empty() {
            findings.push(Finding::new(
                "spec.references",
                "the field is a reference but names no entity to point at",
            ));
        }
    }

    findings
}

fn validate_permission(artifact: &NewArtifact) -> Vec<Finding> {
    let mut findings = Vec::new();

    let key = artifact
        .spec
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // Permission keys are `domain.action`, so the dot is part of the rule rather than a
    // character to reject. Splitting first means the domain half is held to the platform's
    // naming rules and the action half to the same ones.
    //
    // The two halves are the unit, and each is checked **once**. Checking the whole key as
    // well would report `Leave.Read` three times for two mistakes, which reads as a
    // validator that cannot count; the halves' messages name the half, so a reviewer is
    // told which side to fix.
    match key.split_once('.') {
        None => findings.push(Finding::new(
            "spec.key",
            format!("`{key}` is not a `domain.action` permission key"),
        )),
        Some((domain, action)) => {
            if domain.is_empty() || action.is_empty() {
                findings.push(Finding::new(
                    "spec.key",
                    format!("`{key}` has an empty domain or action"),
                ));
            }
            // A reserved domain is refused and a mis-shaped one is refused, and they are
            // independent edits: `Users.read` is fixed by the case, `users.read` by the
            // name, and fixing one does not fix the other.
            findings.extend(validate_key(domain, "spec.key"));
            findings.extend(validate_key(action, "spec.key"));
        }
    }

    let description = artifact
        .spec
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if description.trim().is_empty() {
        findings.push(Finding::new(
            "spec.description",
            "the permission has no description, and the IAM catalogue shows one per key",
        ));
    }

    findings
}

fn validate_workflow(artifact: &NewArtifact) -> Vec<Finding> {
    let mut findings = Vec::new();

    let trigger = artifact
        .spec
        .get("trigger")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if trigger.trim().is_empty() {
        findings.push(Finding::new(
            "spec.trigger",
            "the workflow has no trigger; it could never run",
        ));
    }

    // A workflow with no steps is a trigger that fires into nothing, and the engine accepts
    // the definition — so the finding has to come from here or from nowhere.
    let steps = artifact.spec.get("steps").and_then(Value::as_array);
    match steps {
        None => findings.push(Finding::new("spec.steps", "the workflow has no step list")),
        Some(steps) if steps.is_empty() => findings.push(Finding::new(
            "spec.steps",
            "the workflow has no steps; it would fire into nothing",
        )),
        Some(steps) => {
            for (index, step) in steps.iter().enumerate() {
                let Some(name) = step.get("name").and_then(Value::as_str) else {
                    findings.push(Finding::new(
                        format!("spec.steps[{index}].name"),
                        "the step has no name",
                    ));
                    continue;
                };
                if name.trim().is_empty() {
                    findings.push(Finding::new(
                        format!("spec.steps[{index}].name"),
                        "the step's name is empty",
                    ));
                }
                if step.get("action").and_then(Value::as_str).is_none() {
                    findings.push(Finding::new(
                        format!("spec.steps[{index}].action"),
                        format!("step `{name}` names no action, so the engine has nothing to run"),
                    ));
                }
            }
        }
    }

    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entity(key: &str) -> NewArtifact {
        NewArtifact {
            kind: "entity".into(),
            key: key.into(),
            parent_key: None,
            ordinal: 0,
            spec: json!({ "key": key, "label": "Leave request" }),
            rationale: "Leave requests are what the app is for.".into(),
            validation: json!([]),
        }
    }

    #[test]
    fn a_well_formed_artifact_has_no_findings() {
        assert!(validate_artifact(&entity("leave_request")).is_empty());
    }

    #[test]
    fn a_reserved_key_is_invalid_and_is_named_rather_than_renamed() {
        let findings = validate_artifact(&entity("users"));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].message.contains("reserved platform key"),
            "{findings:?}"
        );
        assert!(findings[0].message.contains("users"), "{findings:?}");
    }

    #[test]
    fn a_key_that_is_not_storeable_is_refused_for_its_shape_and_not_also_for_reservation() {
        // `Users` is both mis-shaped and reserved. Reporting both would leave a reviewer
        // choosing which of two true sentences to fix first; the shape is upstream.
        let findings = validate_artifact(&entity("Users"));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].message.contains("lowercase ASCII"),
            "{findings:?}"
        );
    }

    #[test]
    fn each_naming_defect_is_named_separately() {
        for (key, expected) in [
            ("_leading", "underscore"),
            ("trailing_", "underscore"),
            ("double__underscore", "double underscore"),
            ("with space", "lowercase ASCII"),
            ("a", "minimum"),
            ("Capitalised", "lowercase ASCII"),
        ] {
            let findings = validate_key(key, "key");
            assert!(
                findings.iter().any(|f| f.message.contains(expected)),
                "key {key:?} should be refused for {expected:?}, got {findings:?}"
            );
        }
    }

    #[test]
    fn a_key_with_surrounding_whitespace_is_refused_for_it_and_for_nothing_else() {
        let findings = validate_key(" padded ", "key");
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].message.contains("whitespace"), "{findings:?}");
    }

    #[test]
    fn an_overlong_key_names_its_own_length() {
        let key = "k".repeat(MAX_KEY_LEN + 1);
        let findings = validate_key(&key, "key");
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].message.contains(&MAX_KEY_LEN.to_string()),
            "{findings:?}"
        );
    }

    #[test]
    fn an_artifact_without_a_rationale_says_so() {
        let mut artifact = entity("leave_request");
        artifact.rationale = "  ".into();
        let findings = validate_artifact(&artifact);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].message.contains("nothing to review"),
            "{findings:?}"
        );
    }

    #[test]
    fn a_non_object_body_is_refused_and_stops_the_kind_specific_pass() {
        let mut artifact = entity("leave_request");
        artifact.spec = json!("leave_request");
        let findings = validate_artifact(&artifact);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].path, "spec");
    }

    #[test]
    fn a_field_with_no_entity_to_belong_to_is_named() {
        let artifact = NewArtifact {
            kind: "field".into(),
            key: "start_date".into(),
            parent_key: None,
            ordinal: 0,
            spec: json!({ "key": "start_date", "label": "Start date", "type": "date" }),
            rationale: "A request has a start.".into(),
            validation: json!([]),
        };
        let findings = validate_artifact(&artifact);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].path, "parent_key");
    }

    #[test]
    fn a_reference_without_a_target_is_refused_before_apply_can_find_it() {
        let artifact = NewArtifact {
            kind: "field".into(),
            key: "employee".into(),
            parent_key: Some("leave_request".into()),
            ordinal: 0,
            spec: json!({ "key": "employee", "label": "Employee", "type": "reference" }),
            rationale: "A request belongs to somebody.".into(),
            validation: json!([]),
        };
        let findings = validate_artifact(&artifact);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].path, "spec.references");
    }

    #[test]
    fn a_field_type_the_platform_cannot_render_names_the_ones_it_can() {
        let artifact = NewArtifact {
            kind: "field".into(),
            key: "payload".into(),
            parent_key: Some("leave_request".into()),
            ordinal: 0,
            spec: json!({ "key": "payload", "label": "Payload", "type": "jsonb" }),
            rationale: "A request has a payload.".into(),
            validation: json!([]),
        };
        let findings = validate_artifact(&artifact);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].message.contains("jsonb"), "{findings:?}");
        assert!(findings[0].message.contains("text"), "{findings:?}");
    }

    #[test]
    fn a_permission_key_without_its_dot_is_refused() {
        let missing_dot = NewArtifact {
            kind: "permission".into(),
            key: "leave_read".into(),
            parent_key: None,
            ordinal: 0,
            spec: json!({ "key": "leave_read", "description": "Read leave requests" }),
            rationale: "Somebody has to read them.".into(),
            validation: json!([]),
        };
        let findings = validate_artifact(&missing_dot);
        assert!(
            findings.iter().any(|f| f.message.contains("domain.action")),
            "{findings:?}"
        );
    }

    #[test]
    fn a_workflow_with_no_trigger_and_no_steps_is_two_findings_not_one() {
        let artifact = NewArtifact {
            kind: "workflow".into(),
            key: "leave_approval".into(),
            parent_key: None,
            ordinal: 0,
            spec: json!({}),
            rationale: "A request is approved.".into(),
            validation: json!([]),
        };
        let findings = validate_artifact(&artifact);
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert_eq!(findings[0].path, "spec.trigger");
        assert_eq!(findings[1].path, "spec.steps");
    }

    #[test]
    fn a_step_with_no_action_is_named_by_its_index_and_its_name() {
        let artifact = NewArtifact {
            kind: "workflow".into(),
            key: "leave_approval".into(),
            parent_key: None,
            ordinal: 0,
            spec: json!({
                "trigger": "record.created",
                "steps": [
                    { "name": "notify", "action": "notify.email" },
                    { "name": "ask_manager" }
                ]
            }),
            rationale: "A request is approved.".into(),
            validation: json!([]),
        };
        let findings = validate_artifact(&artifact);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].path, "spec.steps[1].action");
        assert!(findings[0].message.contains("ask_manager"), "{findings:?}");
    }

    #[test]
    fn a_plan_is_not_clean_while_a_required_kind_is_absent_even_if_every_artifact_is() {
        let artifacts = vec![entity("leave_request")];
        let validation = validate_plan(&artifacts);
        assert!(!validation.is_clean());
        // Six kinds are missing, and `role` is deliberately not one of them: a generated app
        // with no role is a lesser risk than one with a role nobody approved.
        assert_eq!(
            validation.required_missing.len(),
            6,
            "{:?}",
            validation.required_missing
        );
        assert!(validation.required_missing.contains(&"report".to_owned()));
        assert!(!validation.required_missing.contains(&"role".to_owned()));
        assert!(validation.findings_for(&artifacts[0]).is_empty());
    }

    #[test]
    fn a_body_whose_key_disagrees_with_the_plans_is_named_rather_than_silently_winning() {
        let mut artifact = entity("leave_request");
        artifact.spec["key"] = json!("leave-request");
        let findings = validate_artifact(&artifact);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].path, "spec.key");
        assert!(
            findings[0]
                .message
                .contains("the plan's key is the one apply uses"),
            "{findings:?}"
        );
    }

    #[test]
    fn a_key_that_agrees_is_reported_once_not_once_per_path() {
        // The artifact list and the body are two columns holding one name. Validating both
        // would print the same reserved word twice, and a reviewer would have to guess which
        // copy is the one to fix.
        let findings = validate_artifact(&entity("users"));
        let reserved = findings
            .iter()
            .filter(|f| f.message.contains("reserved platform key"))
            .count();
        assert_eq!(reserved, 1, "{findings:?}");
    }

    #[test]
    fn a_mis_shaped_permission_key_reports_each_half_exactly_once() {
        let artifact = NewArtifact {
            kind: "permission".into(),
            key: "Leave.Read".into(),
            parent_key: None,
            ordinal: 0,
            spec: json!({ "key": "Leave.Read", "description": "Read leave requests" }),
            rationale: "Somebody has to read them.".into(),
            validation: json!([]),
        };
        let findings = validate_artifact(&artifact);
        // Two mistakes — a capital in each half — and no third finding restating the key.
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert!(
            findings.iter().any(|f| f.message.contains("`Leave`")),
            "{findings:?}"
        );
        assert!(
            findings.iter().any(|f| f.message.contains("`Read`")),
            "{findings:?}"
        );
    }

    #[test]
    fn a_reserved_permission_domain_is_refused_on_its_own() {
        let artifact = NewArtifact {
            kind: "permission".into(),
            key: "users.read".into(),
            parent_key: None,
            ordinal: 0,
            spec: json!({ "key": "users.read", "description": "Read users" }),
            rationale: "Somebody has to read them.".into(),
            validation: json!([]),
        };
        let findings = validate_artifact(&artifact);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].message.contains("reserved platform key"),
            "{findings:?}"
        );
    }

    #[test]
    fn findings_are_addressable_by_the_artifact_they_belong_to() {
        let mut broken = entity("users");
        broken.ordinal = 3;
        let validation = validate_plan(&[entity("leave_request"), broken.clone()]);
        assert!(validation.findings_for(&broken).len() == 1);
        let clean = entity("leave_request");
        assert!(validation.findings_for(&clean).is_empty());
        // Two artifacts with the same key and kind are the same address by construction,
        // which is why the migration makes that pair unique.
        assert_eq!(validation.findings.len(), 2);
    }

    #[test]
    fn findings_serialise_into_the_column_the_migration_checks() {
        let findings = validate_artifact(&entity("Users"));
        let json = serde_json::to_value(&findings).expect("findings serialise");
        assert!(json.is_array());
        let first = &json[0];
        assert!(first["path"].is_string());
        assert!(first["message"].is_string());
    }
}
