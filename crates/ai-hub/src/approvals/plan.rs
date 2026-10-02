//! The one field mapping, shared by preview and apply (REQ-101 slice 2).
//!
//! # The problem this module exists to make impossible
//!
//! The request's two criteria that are not testable by reading code are:
//!
//! 1. *Approving applies exactly the previewed operations: every written field value equals the
//!    value in the frozen preview, asserted per field in a test.*
//! 2. *The preview and the apply share one implementation: a test mutates a field mapping and
//!    fails both together.*
//!
//! The tempting shape — a `preview()` that renders a diff for the screen and an `apply()` that
//! calls the service — is how every approval system drifts: the diff renderer and the writer
//! each grow a private notion of "what this tool changes", and the reviewer approves a diff
//! the apply does not produce. The two halves agree until the first field with a default, a
//! truncation or a rename, and then they disagree silently.
//!
//! So both halves are pure functions over the same [`Mapping`], and the mapping is *data*: a
//! list of [`FieldSpec`] rather than code. [`plan`] reads it to validate and coerce a proposed
//! operation into a [`Plan`]; [`writes`] reads it to turn that plan into the exact column
//! writes the apply performs. Neither half contains a field name, a length limit or a
//! coercion of its own, so there is no second place to fall out of sync.
//!
//! # Why the coercion lives in the preview
//!
//! `plan` coerces every value through the spec's [`FieldKind`] **before** the diff is rendered,
//! and the diff carries the coerced value. A preview that renders the raw argument while the
//! apply writes the coerced one is precisely the promise the request forbids: the reviewer
//! sees a body as 9 000 characters and the platform stores the 4 000 the column holds. Putting
//! the coercion in the plan means the reviewer sees the value that will be written, and a
//! value the field cannot hold is a refusal at *preview* time rather than a failed apply.
//!
//! # Why the hash is over the diff and not over the request
//!
//! [`Plan::hash`] is the SHA-256 of the canonical form of the resolved diff. The request binds
//! a decision to this hash ("single-use and bound to the hash"), so the hash has to change
//! when anything a reviewer could be misled about changes — and it must **not** change when
//! nothing did. Hashing the incoming arguments would fail that: `{"title":"x"}` and
//! `{"title":"x","body":""}` are the same edit, and a decision bound to the first would refuse
//! the second for no reason. `serde_json`'s map is ordered, so serialising the resolved
//! structure is canonical without a hand-written key sorter.
//!
//! # What this module deliberately does not do
//!
//! It never touches a database. Reading the current state and writing the resolved values are
//! the caller's job, which is what makes the whole mapping unit-testable — and the agreement
//! criteria are testable here, against a mapping a test can mutate, without a migration.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::error::{AiHubError, Result};

/// Longest slug a page may carry (the `pages_slug_format` constraint, restated here so the
/// preview can refuse what the database would refuse).
pub const MAX_SLUG: usize = 96;
/// Longest revision title. The API's own validator has this ceiling and the preview must not
/// be more permissive than the writer.
pub const MAX_TITLE: usize = 200;
/// Longest summary.
pub const MAX_SUMMARY: usize = 500;

/// The preview format version. Bumped when the stored shape changes so an approval frozen
/// under the old shape is refused rather than misread.
pub const PREVIEW_VERSION: u32 = 1;

/// How one field is written. The kind decides coercion *and* validation, once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    /// Free text, trimmed, non-empty, at most [`MAX_TITLE`].
    Title,
    /// Long text, trimmed at the ends, no ceiling.
    Body,
    /// Optional short text; an empty string clears the value.
    Summary,
    /// URL path segment, lowercased, `[a-z0-9-]`, at most [`MAX_SLUG`].
    Slug,
    /// A closed vocabulary, so a preview can refuse a status the page row does not carry.
    Status,
}

/// One entry of the mapping: the tool argument, the column it lands in, and its kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldSpec {
    /// The tool argument's name, as the model wrote it.
    pub arg: &'static str,
    /// The column or model field the value is written to.
    pub field: &'static str,
    /// Coercion and validation for this field.
    pub kind: FieldKind,
}

/// The mapping itself. Slice 2 ships page updates; adding a resource is adding a spec and a
/// reader, never a second writer.
pub type Mapping = &'static [FieldSpec];

/// The page-update mapping. `slug` and `status` live on `pages`; the content fields live on
/// the draft revision, which is why one operation touches two rows and the apply is not a
/// single `update`.
pub const PAGE_UPDATE: Mapping = &[
    FieldSpec {
        arg: "slug",
        field: "slug",
        kind: FieldKind::Slug,
    },
    FieldSpec {
        arg: "title",
        field: "title",
        kind: FieldKind::Title,
    },
    FieldSpec {
        arg: "body",
        field: "body",
        kind: FieldKind::Body,
    },
    FieldSpec {
        arg: "summary",
        field: "summary",
        kind: FieldKind::Summary,
    },
    FieldSpec {
        arg: "status",
        field: "status",
        kind: FieldKind::Status,
    },
];

/// The page statuses the `pages_status_check` constraint allows.
pub const PAGE_STATUSES: &[&str] = &["draft", "published", "archived"];

/// What an operation does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    /// Insert a new resource.
    Create,
    /// Change fields of an existing resource.
    Update,
    /// Remove a resource and everything that depends on it.
    Delete,
}

impl OpKind {
    /// The word a change set shows in its operation list.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

/// One proposed operation, as a tool argument object plus its target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    /// Create, update or delete.
    pub kind: OpKind,
    /// `page` for the page mapping; carried so a plan states what it is about.
    pub resource_type: String,
    /// The target's id, empty for a create.
    pub resource_id: String,
    /// The tool arguments, keyed by [`FieldSpec::arg`].
    #[serde(default)]
    pub args: Value,
}

/// One resolved field change. `before` is `None` for a field the target does not have, and
/// `after` is `None` for a field the operation clears.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldDiff {
    /// The tool argument this came from.
    pub arg: String,
    /// The column it lands in, taken from the mapping — never from the argument name, so a
    /// rename in the mapping shows up in the plan and in the write together.
    pub field: String,
    /// The value on the target right now.
    pub before: Option<Value>,
    /// The value that will be written, already coerced.
    pub after: Option<Value>,
}

impl FieldDiff {
    /// `true` when this diff writes a different value than the one on the target.
    #[must_use]
    pub fn changes(&self) -> bool {
        self.before != self.after
    }
}

/// A dependent that a delete takes with it. The request wants the count *in the preview*
/// ("1 page, 4 revisions") and the same count in the applied result, so the count is computed
/// once by the caller and carried through rather than re-derived at apply time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cascade {
    /// What the dependent is, in plain language (`page revision`).
    pub label: String,
    /// How many rows go.
    pub count: i64,
}

/// A resolved, hashable description of one operation: the exact writes it will make.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// Create, update or delete.
    pub kind: OpKind,
    /// What it acts on.
    pub resource_type: String,
    /// Its id.
    pub resource_id: String,
    /// The human-readable name the reviewer reads and the confirmation phrase is checked
    /// against. Stored on the row, so a rename between preview and decision cannot move the
    /// phrase out from under the reviewer.
    pub label: String,
    /// The field-level diff, resolved and coerced.
    pub diffs: Vec<FieldDiff>,
    /// What a delete takes with it.
    pub cascades: Vec<Cascade>,
    /// The revision of the target the diff was computed against. Its disagreement with the
    /// target's current revision is what `stale` means.
    pub base_revision: String,
    /// The hash the decision is bound to.
    pub hash: String,
}

impl Plan {
    /// The frozen `preview` jsonb stored on the approval: the plan plus the mapping it was
    /// resolved with, so the review screen renders exactly what the apply will read.
    #[must_use]
    pub fn to_preview(&self, mapping: Mapping) -> Value {
        json!({
            "version": PREVIEW_VERSION,
            "kind": self.kind,
            "resource_type": self.resource_type,
            "resource_id": self.resource_id,
            "label": self.label,
            "base_revision": self.base_revision,
            "hash": self.hash,
            "diffs": self.diffs,
            "cascades": self.cascades,
            "mapping": mapping,
        })
    }

    /// Read a plan back out of a stored preview, so the apply walks the same structure the
    /// preview was rendered from.
    ///
    /// # Errors
    ///
    /// `Err(InvalidApproval)` when the preview is missing, is not an object of this version,
    /// or does not carry the operations the apply needs. A half-read preview is refused rather
    /// than applied with defaults — the reviewer approved a specific set of writes.
    pub fn from_preview(preview: &Value) -> Result<Self> {
        if !is_supported_preview(preview) {
            return Err(AiHubError::InvalidApproval(format!(
                "the stored preview is not a version {PREVIEW_VERSION} preview"
            )));
        }
        let kind = preview
            .get("kind")
            .and_then(Value::as_str)
            .and_then(|text| match text {
                "create" => Some(OpKind::Create),
                "update" => Some(OpKind::Update),
                "delete" => Some(OpKind::Delete),
                _ => None,
            })
            .ok_or_else(|| {
                AiHubError::InvalidApproval("the stored preview names no operation kind".to_owned())
            })?;
        let diffs = stored_diffs(preview).ok_or_else(|| {
            AiHubError::InvalidApproval("the stored preview has no readable diffs".to_owned())
        })?;
        Ok(Plan {
            kind,
            resource_type: text_of(preview, "resource_type")?,
            resource_id: text_of(preview, "resource_id").unwrap_or_default(),
            label: stored_label(preview, ""),
            diffs,
            cascades: stored_cascades(preview),
            base_revision: text_of(preview, "base_revision").unwrap_or_default(),
            hash: text_of(preview, "hash")?,
        })
    }
}

/// [`plan`], but a no-op comes back as a plan rather than a refusal.
///
/// `cascades` is taken and **kept**, not dropped. That was the first draft's reasoning — "a no-op
/// is an update or a create, and neither reads cascades" — and it is wrong for the case the
/// function exists to serve: a `delete` writes no field by construction, so **every** delete
/// looks like a no-op to [`changes_anything`], and the no-op path is therefore exactly the path
/// a delete takes. Passing `Vec::new()` here silently emptied a delete's cascade list, and the
/// walk that caught it (`a_delete_previews_its_cascades_and_writes_no_field`) reads the value
/// back out of the plan. A preview that drops the consequence of an operation is worse than one
/// that refuses it.
pub fn plan_allowing_no_op(
    mapping: Mapping,
    op: &Operation,
    current: &Value,
    label: &str,
    base_revision: &str,
    cascades: Vec<Cascade>,
) -> Result<Plan> {
    plan_with_diffs(mapping, op, current, label, base_revision, cascades)
}

/// Whether these diffs write anything.
///
/// The single answer to "is this operation a no-op?", and it exists as a function because two
/// callers need the fact and only one of them may act on it: [`plan`] refuses (a lone no-op
/// approval is a decision nobody can meaningfully make), while the change-set editor previews
/// a whole *list* and must be able to show a reviewer "this one writes nothing" so they can
/// drop it deliberately. Neither may re-derive it from the diffs — that is how the editor
/// ended up with a `no_op` flag the server could never set to true.
#[must_use]
pub fn changes_anything(diffs: &[FieldDiff]) -> bool {
    diffs.iter().any(FieldDiff::changes)
}

/// Resolve one operation against the target's current values.
///
/// `current` is the target as it is now, keyed by the mapping's **field** names (build it with
/// [`snapshot`]). Returning the current state rather than a handle is what makes the plan
/// deterministic, hashable and testable, and it is why staleness is a *separate* check: this
/// function is about what the operation would write, and `base_revision` is the caller's claim
/// about what it was read from.
///
/// # Errors
///
/// `Err(InvalidApproval)` when an argument cannot be coerced for its field, when the operation
/// names a field the mapping does not have, or when the operation changes nothing at all — a
/// no-op approval is a decision a reviewer cannot meaningfully make.
pub fn plan(
    mapping: Mapping,
    op: &Operation,
    current: &Value,
    label: &str,
    base_revision: &str,
    cascades: Vec<Cascade>,
) -> Result<Plan> {
    let plan = plan_with_diffs(mapping, op, current, label, base_revision, cascades)?;
    // A delete is exempt, and the exemption is the whole reason the check lives here rather
    // than where it used to. A delete writes no field **by construction**, so
    // `changes_anything(&plan.diffs)` is `false` for every delete ever built — testing it
    // would make the check refuse the one operation class that legitimately changes something
    // (the target, and everything that cascades from it). The check moved out of
    // `plan_with_diffs` when the editor's list policy was added, and moving it *out* of the
    // delete's early return on the way is how that became a regression: two of this repo's own
    // walks went red on the first execution (`a_delete_previews_its_cascades_…` unit-side and
    // `approving_every_parked_row_…` walk-side) and the walk's message — "`page` changes
    // nothing" naming a **page** while parking a *delete* — is the tell.
    //
    // The predicate is therefore about "an update whose values already match", not about
    // "the diff list is empty", and it is written that way so the next reader does not have to
    // rediscover that a delete is a legitimate way to write no fields.
    if op.kind != OpKind::Delete && !changes_anything(&plan.diffs) {
        return Err(AiHubError::InvalidApproval(format!(
            "`{}` changes nothing — every value already matches",
            op.resource_type
        )));
    }
    Ok(plan)
}

/// The shared body of both policies: build the plan, in every case, and let the caller decide
/// what a no-op means.
///
/// Splitting it this way is the whole point — the diffs, their order and the hash are one
/// computation, and the difference between "refuse" and "show it" is a single check on the
/// finished list. Doing the check inline (as it was) meant the only way to get a no-op plan
/// was to re-run the whole function, and the caller that needed it — the change-set editor —
/// ended up rendering a `no_op` column the server could never fill.
pub fn plan_with_diffs(
    mapping: Mapping,
    op: &Operation,
    current: &Value,
    label: &str,
    base_revision: &str,
    cascades: Vec<Cascade>,
) -> Result<Plan> {
    if op.kind == OpKind::Delete {
        // A delete writes no fields. The diff is the target going away and the cascades are
        // the consequence — the request's "1 page, 4 revisions" line. An empty cascade list is
        // legitimate (a resource nothing depends on), so this is not a refusal.
        return Ok(Plan {
            kind: op.kind,
            resource_type: op.resource_type.clone(),
            resource_id: op.resource_id.clone(),
            label: label.to_owned(),
            diffs: Vec::new(),
            cascades,
            base_revision: base_revision.to_owned(),
            hash: hash_of(
                &op.kind,
                &op.resource_type,
                &op.resource_id,
                &[],
                base_revision,
            ),
        });
    }

    let object = match &op.args {
        Value::Object(map) => map.clone(),
        // An absent argument object means "no fields named", which the emptiness check below
        // turns into a refusal that says so. Parsing `"x"` as a map would be a panic in a
        // request path.
        other => {
            return Err(AiHubError::InvalidApproval(format!(
                "`{}` expects an object of field values, got {}",
                op.resource_type,
                kind_of(other)
            )));
        }
    };

    let mut diffs = Vec::with_capacity(object.len());
    for (name, value) in &object {
        let Some(spec) = mapping.iter().find(|spec| spec.arg == name) else {
            // Refusing an unknown field rather than ignoring it is the difference between "the
            // tool wrote what it was asked" and "the tool wrote what it recognised". A silently
            // dropped argument is a preview that under-reports the change.
            return Err(AiHubError::InvalidApproval(format!(
                "`{}` does not write `{name}`; it writes {}",
                op.resource_type,
                describe(mapping)
            )));
        };
        let before = current.get(spec.field).cloned();
        // `coerce` answers with the reason as a plain `String` because it is also the message
        // a mapping-specific test reads; the boundary into the crate's error happens here, at
        // the one place a refusal becomes an error.
        let after =
            coerce(spec.kind, value).map_err(|reason| AiHubError::InvalidApproval(reason))?;
        diffs.push(FieldDiff {
            arg: spec.arg.to_owned(),
            field: spec.field.to_owned(),
            before,
            after,
        });
    }

    // Deterministic order: the mapping's, not the model's. Two runs that ask for the same
    // fields in a different JSON order produce the same hash, so re-previewing an unchanged
    // request does not invalidate the reviewer's decision.
    diffs.sort_by(|a, b| {
        let rank = |diff: &FieldDiff| {
            mapping
                .iter()
                .position(|spec| spec.arg == diff.arg)
                .unwrap_or(usize::MAX)
        };
        rank(a).cmp(&rank(b)).then_with(|| a.arg.cmp(&b.arg))
    });

    Ok(Plan {
        kind: op.kind,
        resource_type: op.resource_type.clone(),
        resource_id: op.resource_id.clone(),
        label: label.to_owned(),
        hash: hash_of(
            &op.kind,
            &op.resource_type,
            &op.resource_id,
            &diffs,
            base_revision,
        ),
        diffs,
        cascades,
        base_revision: base_revision.to_owned(),
    })
}

/// The writes a plan performs.
///
/// This is the second half of the same implementation: it reads the mapping to find each
/// diff's **column**, so a mapping whose `field` changed writes the new column here exactly as
/// `plan` reported the new column in the preview. There is no other place that knows a column
/// name.
///
/// # Errors
///
/// `Err(InvalidApproval)` when a diff names a field the mapping does not have — a stored
/// preview from a different mapping, which must not be applied by guessing a column.
pub fn writes(plan: &Plan, mapping: Mapping) -> Result<Vec<Write>> {
    let mut out = Vec::with_capacity(plan.diffs.len());
    for diff in &plan.diffs {
        let Some(spec) = mapping.iter().find(|spec| spec.arg == diff.arg) else {
            return Err(AiHubError::InvalidApproval(format!(
                "the stored preview writes `{}`, which the current mapping does not know",
                diff.field
            )));
        };
        out.push(Write {
            field: spec.field.to_owned(),
            value: diff.after.clone(),
        });
    }
    Ok(out)
}

impl Plan {
    /// The tool arguments this plan was resolved from — the **inverse** of [`plan`].
    ///
    /// Re-previewing needs it: to recompute a diff against a moved target you must re-plan the
    /// *same* edit, and a plan stores resolved values rather than arguments. Reading the fields
    /// back out is the only place that mapping from a column to an argument name lives, so it
    /// lives here beside the forward direction rather than in the caller that happens to need it
    /// — the same reason [`writes`] is in this module.
    ///
    /// The mapping is taken as an argument rather than looked up, so a preview read back under
    /// a resource type this build has no mapping for is **refused** instead of being re-planned
    /// with arguments the reader would then misapply.
    ///
    /// A cleared field comes back as an **empty string**, because that is the argument that
    /// re-coerces to the same `Null`. Two spellings of "cleared" reach this function and both
    /// have to be handled, which is the trap: `coerce` represents a clear as
    /// `Some(Value::Null)`, so `diff.after` is `Some(Null)` and not `None` — a `None` match
    /// alone passes `Value::Null` straight back as an argument, and `coerce` then refuses it as
    /// "expected text, got null". The symptom is a re-preview of a cleared field failing for a
    /// reason that has nothing to do with the field. The round trip is therefore idempotent by
    /// construction: preview, re-preview and apply all agree on the same write.
    ///
    /// # Errors
    ///
    /// `Err(InvalidApproval)` when a diff names an argument the mapping does not have — a stored
    /// preview from a different mapping, which must not be re-planned by guessing a name.
    pub fn arguments(&self, mapping: Mapping) -> Result<Value> {
        let mut args = Map::new();
        for diff in &self.diffs {
            if !mapping.iter().any(|spec| spec.arg == diff.arg) {
                return Err(AiHubError::InvalidApproval(format!(
                    "the stored preview writes `{}`, which the current mapping does not know",
                    diff.arg
                )));
            }
            // `None` and `Some(Null)` are the two spellings of "this operation clears the
            // field"; both become the empty string `coerce` turns back into a clear.
            args.insert(
                diff.arg.clone(),
                match &diff.after {
                    None | Some(Value::Null) => Value::from(""),
                    Some(value) => value.clone(),
                },
            );
        }
        Ok(Value::Object(args))
    }

    /// The operation this plan describes, rebuilt for a re-preview.
    ///
    /// `plan` is a pure function of `(mapping, operation, current)`, so recomputing a diff is
    /// `plan` with the *same operation* and a *fresh* current — which is exactly what
    /// [`Self::arguments`] plus the caller's read gives.
    pub fn operation(&self, mapping: Mapping) -> Result<Operation> {
        Ok(Operation {
            kind: self.kind,
            resource_type: self.resource_type.clone(),
            resource_id: self.resource_id.clone(),
            args: self.arguments(mapping)?,
        })
    }
}

/// One column write, derived from a plan and the same mapping that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Write {
    /// The column.
    pub field: String,
    /// The value to write, already coerced. `None` clears the column.
    pub value: Option<Value>,
}

/// The value a field is coerced to, or the reason the preview refuses.
///
/// Refusals are a `Result` rather than an `Option` because `Ok(None)` is a *clear* and
/// `Err(..)` is a *refusal*; collapsing them is how a preview renders "this field will be
/// emptied" for an argument the writer was never going to accept.
type Coerced = std::result::Result<Option<Value>, String>;

/// Coerce one value the way its field stores it.
///
/// The single place a length limit, a lowercase or a vocabulary exists in this module. Both
/// halves of the gate go through it, so a preview cannot accept a value the apply refuses.
fn coerce(kind: FieldKind, value: &Value) -> Coerced {
    let Some(text) = value.as_str() else {
        return Err(format!("expected text, got {}", kind_of(value)));
    };
    let trimmed = text.trim();
    Ok(match kind {
        FieldKind::Title => {
            if trimmed.is_empty() {
                return Err("the title cannot be empty".to_owned());
            }
            let length = trimmed.chars().count();
            if length > MAX_TITLE {
                return Err(format!(
                    "the title is {length} characters; the limit is {MAX_TITLE}"
                ));
            }
            Some(Value::from(trimmed))
        }
        FieldKind::Body => Some(Value::from(trimmed)),
        FieldKind::Summary => {
            let length = trimmed.chars().count();
            if length > MAX_SUMMARY {
                return Err(format!(
                    "the summary is {length} characters; the limit is {MAX_SUMMARY}"
                ));
            }
            // An empty summary is a cleared summary, not an empty string stored forever.
            Some(if trimmed.is_empty() {
                Value::Null
            } else {
                Value::from(trimmed)
            })
        }
        FieldKind::Slug => {
            let lower = trimmed.to_lowercase();
            if lower.is_empty() || lower.chars().count() > MAX_SLUG {
                return Err(format!("the slug must be 1 to {MAX_SLUG} characters"));
            }
            let shaped = lower
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !lower.starts_with('-')
                && !lower.ends_with('-');
            if !shaped {
                return Err(format!(
                    "the slug may only contain a-z, 0-9 and inner hyphens; `{trimmed}` does not"
                ));
            }
            Some(Value::from(lower))
        }
        FieldKind::Status => {
            if !PAGE_STATUSES.contains(&trimmed) {
                return Err(format!(
                    "`{trimmed}` is not a page status; use one of {}",
                    PAGE_STATUSES.join(", ")
                ));
            }
            Some(Value::from(trimmed))
        }
    })
}

/// The hash the decision is bound to.
///
/// Over the *resolved* diff, the target and the base revision — never over the raw arguments.
/// Reordering the same fields or restating a value identically produces the same hash, and
/// changing any written value, the target or the revision it was computed against does not.
fn hash_of(
    kind: &OpKind,
    resource_type: &str,
    resource_id: &str,
    diffs: &[FieldDiff],
    base_revision: &str,
) -> String {
    let mut ordered = diffs.to_vec();
    // Sorted by (field, arg) so the hash does not depend on the mapping's declaration order
    // either: re-declaring the mapping is not a change to what the reviewer saw.
    ordered.sort_by(|a, b| a.field.cmp(&b.field).then_with(|| a.arg.cmp(&b.arg)));
    let canonical = json!({
        "version": PREVIEW_VERSION,
        "kind": kind,
        "resource_type": resource_type,
        "resource_id": resource_id,
        "base_revision": base_revision,
        "diffs": ordered,
    });
    // `Plan`'s fields are all owned primitives and `Value`, so this cannot fail; the fallback
    // is the shape rather than an `unwrap` at a call site.
    serde_json::to_vec(&canonical).map_or_else(
        |_| String::from("unhashable"),
        |bytes| format!("{:x}", Sha256::digest(bytes)),
    )
}

/// The JSON type name of a value, for a message that names what arrived.
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "text",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

/// The mapping's fields as one sentence, so an unknown-argument refusal tells the caller
/// what *is* writable instead of only what is not.
fn describe(mapping: Mapping) -> String {
    mapping
        .iter()
        .map(|spec| spec.arg)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A string field of a stored preview, or an error naming it.
fn text_of(preview: &Value, key: &str) -> Result<String> {
    preview
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| AiHubError::InvalidApproval(format!("the stored preview has no `{key}`")))
}

/// `true` when a stored preview is one this build knows how to apply.
#[must_use]
pub fn is_supported_preview(preview: &Value) -> bool {
    preview
        .get("version")
        .and_then(Value::as_u64)
        .is_some_and(|version| version == u64::from(PREVIEW_VERSION))
}

/// The diffs of a stored preview, or `None` when it is not a supported one.
#[must_use]
pub fn stored_diffs(preview: &Value) -> Option<Vec<FieldDiff>> {
    if !is_supported_preview(preview) {
        return None;
    }
    let array = preview.get("diffs")?.as_array()?;
    let mut out = Vec::with_capacity(array.len());
    for entry in array {
        out.push(FieldDiff {
            arg: entry.get("arg")?.as_str()?.to_owned(),
            field: entry.get("field")?.as_str()?.to_owned(),
            before: entry.get("before").cloned(),
            after: entry.get("after").cloned(),
        });
    }
    Some(out)
}

/// The cascades of a stored preview, defaulting to none.
#[must_use]
pub fn stored_cascades(preview: &Value) -> Vec<Cascade> {
    let Some(array) = preview.get("cascades").and_then(Value::as_array) else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(|entry| {
            Some(Cascade {
                label: entry.get("label")?.as_str()?.to_owned(),
                count: entry.get("count")?.as_i64()?,
            })
        })
        .collect()
}

/// The label a preview carries, falling back to the id (and then to nothing, which the caller
/// renders as the id).
#[must_use]
pub fn stored_label(preview: &Value, resource_id: &str) -> String {
    preview
        .get("label")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map_or_else(|| resource_id.to_owned(), str::to_owned)
}

/// Read a target's current values out of a flat map, for [`plan`]'s `current` argument.
///
/// Kept here so the reader and the mapping agree on keying: `current` is keyed by
/// [`FieldSpec::field`], and a caller that keys it by argument name gets a diff whose `before`
/// is always `None` — which renders exactly like a create.
#[must_use]
pub fn snapshot(mapping: Mapping, values: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    for spec in mapping {
        if let Some(value) = values.get(spec.field) {
            out.insert(spec.field.to_owned(), value.clone());
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn update(args: Value) -> Operation {
        Operation {
            kind: OpKind::Update,
            resource_type: "page".to_owned(),
            resource_id: "11111111-1111-1111-1111-111111111111".to_owned(),
            args,
        }
    }

    /// A page as it is now. `summary` carries text rather than null on purpose: clearing a
    /// summary that is already null really is a no-op, and the "empty summary clears" test
    /// would then be asserting the emptiness check instead of the clear.
    fn current() -> Value {
        json!({
            "slug": "about",
            "title": "About us",
            "body": "Hello",
            "summary": "who we are",
            "status": "draft",
        })
    }

    fn map_of(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => panic!("a map"),
        }
    }

    fn ok_plan(args: Value) -> Plan {
        plan(
            PAGE_UPDATE,
            &update(args),
            &current(),
            "About us",
            "r7",
            Vec::new(),
        )
        .expect("the plan resolves")
    }

    // --- criterion: "Approving applies exactly the previewed operations: every written field
    // value equals the value in the frozen preview, asserted per field in a test." ---

    #[test]
    fn every_written_value_is_the_previewed_value() {
        let plan = ok_plan(json!({ "title": "About our team", "summary": "who we are" }));

        // The review screen renders `plan.diffs`. The apply performs `writes`. The criterion is
        // that they are the same values, per field — so the comparison is per field, not a
        // count and not a rendered string.
        let applied = writes(&plan, PAGE_UPDATE).expect("the plan applies");
        assert_eq!(
            applied.len(),
            plan.diffs.len(),
            "one write per previewed field"
        );

        for diff in &plan.diffs {
            let write = applied
                .iter()
                .find(|write| write.field == diff.field)
                .unwrap_or_else(|| panic!("`{}` was previewed but never written", diff.field));
            assert_eq!(
                write.value, diff.after,
                "field `{}` is written a different value than the preview showed",
                diff.field
            );
        }
    }

    #[test]
    fn the_frozen_preview_agrees_with_the_write_it_produces() {
        // The stored jsonb is what the reviewer decided on, so the agreement has to hold
        // *through storage*: read the plan back out of the frozen value and compare again.
        let plan = ok_plan(json!({ "title": "About our team", "body": "Hello there" }));
        let stored = plan.to_preview(PAGE_UPDATE);
        let reread = Plan::from_preview(&stored).expect("the frozen preview reads back");

        let from_store = writes(&reread, PAGE_UPDATE).expect("the stored plan applies");
        for (diff, write) in plan.diffs.iter().zip(from_store.iter()) {
            assert_eq!(diff.field, write.field);
            assert_eq!(diff.after, write.value);
        }
    }

    #[test]
    fn a_field_the_operation_omits_is_neither_previewed_nor_written() {
        let plan = ok_plan(json!({ "title": "Only the title" }));
        assert_eq!(plan.diffs.len(), 1, "one field named, one field previewed");

        let applied = writes(&plan, PAGE_UPDATE).expect("the plan applies");
        assert!(
            applied.iter().all(|write| write.field != "body"),
            "an omitted argument must not be written — that is how an edit silently resets a \
             field the model never mentioned"
        );
    }

    // --- criterion: "The preview and the apply share one implementation: a test mutates a
    // field mapping and fails both together." ---

    #[test]
    fn mutating_the_mapping_moves_both_halves_together() {
        // A mapping that writes `title` into a different column. That is the kind of change
        // that historically landed in one half only.
        const REMAPPED: Mapping = &[
            FieldSpec {
                arg: "slug",
                field: "slug",
                kind: FieldKind::Slug,
            },
            FieldSpec {
                arg: "title",
                field: "headline",
                kind: FieldKind::Title,
            },
            FieldSpec {
                arg: "body",
                field: "body",
                kind: FieldKind::Body,
            },
            FieldSpec {
                arg: "summary",
                field: "summary",
                kind: FieldKind::Summary,
            },
        ];

        let op = update(json!({ "title": "About our team" }));
        let plan = plan(REMAPPED, &op, &current(), "About us", "r7", Vec::new())
            .expect("the plan resolves");

        // Half one: the preview now *names* the new column.
        let previewed = plan
            .diffs
            .iter()
            .find(|diff| diff.arg == "title")
            .expect("the title is previewed");
        assert_eq!(
            previewed.field, "headline",
            "the preview still names the old column, so the reviewer approves a write to a \
             column the apply no longer uses"
        );

        // Half two: the apply now writes the new column and nothing to the old one.
        let applied = writes(&plan, REMAPPED).expect("the plan applies");
        let write = applied
            .iter()
            .find(|write| write.field == "headline")
            .expect("the remapped column is written");
        assert_eq!(write.value, previewed.after);
        assert!(
            applied.iter().all(|write| write.field != "title"),
            "the apply still writes the column the mapping dropped"
        );
    }

    #[test]
    fn a_limit_in_the_mapping_moves_the_refusal_to_the_preview() {
        // The title's limit lives in this module, so the preview refuses an over-long value —
        // asserted here because a preview that accepts what the apply refuses is the exact
        // promise the request forbids.
        let error = plan(
            PAGE_UPDATE,
            &update(json!({ "title": "x".repeat(MAX_TITLE + 1) })),
            &current(),
            "About us",
            "r1",
            Vec::new(),
        )
        .expect_err("a title over the limit is refused while previewing");
        assert!(
            error.to_string().contains("characters"),
            "the refusal must name the limit it broke: {error}"
        );

        plan(
            PAGE_UPDATE,
            &update(json!({ "title": "x".repeat(MAX_TITLE) })),
            &current(),
            "About us",
            "r1",
            Vec::new(),
        )
        .expect("a title exactly on the limit is accepted");
    }

    #[test]
    fn a_stored_diff_the_mapping_dropped_is_refused_rather_than_guessed() {
        // A preview frozen under a mapping that wrote `headline`, read by a build whose mapping
        // no longer knows the argument. Applying it would have to guess a column.
        let frozen = json!({
            "version": PREVIEW_VERSION,
            "kind": "update",
            "resource_type": "page",
            "resource_id": "p1",
            "label": "About us",
            "base_revision": "r7",
            "hash": "abc",
            "diffs": [{ "arg": "headline", "field": "headline", "before": null, "after": "x" }],
        });
        let stored = Plan::from_preview(&frozen).expect("the frozen preview reads back");
        let error = writes(&stored, PAGE_UPDATE).expect_err("an unknown field is refused");
        assert!(
            error.to_string().contains("headline"),
            "the refusal must name the field it could not place: {error}"
        );
    }

    // --- the reviewer's view has to be stable ---

    #[test]
    fn the_hash_ignores_argument_order_and_the_mapping_order() {
        let one = ok_plan(json!({ "title": "A", "body": "B" }));
        let other = ok_plan(json!({ "body": "B", "title": "A" }));
        assert_eq!(
            one.hash, other.hash,
            "JSON key order must not invalidate a decision"
        );

        const REORDERED: Mapping = &[
            FieldSpec {
                arg: "body",
                field: "body",
                kind: FieldKind::Body,
            },
            FieldSpec {
                arg: "slug",
                field: "slug",
                kind: FieldKind::Slug,
            },
            FieldSpec {
                arg: "summary",
                field: "summary",
                kind: FieldKind::Summary,
            },
            FieldSpec {
                arg: "title",
                field: "title",
                kind: FieldKind::Title,
            },
            FieldSpec {
                arg: "status",
                field: "status",
                kind: FieldKind::Status,
            },
        ];
        let remapped = plan(
            REORDERED,
            &update(json!({ "title": "A", "body": "B" })),
            &current(),
            "About us",
            "r7",
            Vec::new(),
        )
        .expect("the plan resolves");
        assert_eq!(
            one.hash, remapped.hash,
            "re-declaring the mapping is not a change to what the reviewer saw"
        );
    }

    #[test]
    fn the_hash_moves_when_a_written_value_moves() {
        let one = ok_plan(json!({ "title": "A" }));
        let padded = ok_plan(json!({ "title": "A " }));
        assert_eq!(one.hash, padded.hash, "coercion happens before the hash");

        let moved = ok_plan(json!({ "title": "B" }));
        assert_ne!(one.hash, moved.hash);
    }

    #[test]
    fn the_hash_moves_when_the_base_revision_moves() {
        let at_r7 = ok_plan(json!({ "title": "A" }));
        let at_r8 = plan(
            PAGE_UPDATE,
            &update(json!({ "title": "A" })),
            &current(),
            "About us",
            "r8",
            Vec::new(),
        )
        .expect("the plan resolves");
        assert_ne!(
            at_r7.hash, at_r8.hash,
            "a decision must not be reusable against a different base"
        );
    }

    #[test]
    fn the_hash_moves_when_the_target_moves() {
        let on_p1 = plan(
            PAGE_UPDATE,
            &update(json!({ "title": "A" })),
            &current(),
            "About us",
            "r7",
            Vec::new(),
        )
        .expect("the plan resolves");
        let on_p2 = plan(
            PAGE_UPDATE,
            &Operation {
                resource_id: "p2".to_owned(),
                ..update(json!({ "title": "A" }))
            },
            &current(),
            "About us",
            "r7",
            Vec::new(),
        )
        .expect("the plan resolves");
        assert_ne!(on_p1.hash, on_p2.hash);
    }

    // --- the refusals, which are the reviewer's protection ---

    #[test]
    fn an_unknown_argument_is_refused_rather_than_ignored() {
        let error = plan(
            PAGE_UPDATE,
            &update(json!({ "titel": "typo" })),
            &current(),
            "About us",
            "r7",
            Vec::new(),
        )
        .expect_err("an unknown field is refused");
        let message = error.to_string();
        assert!(
            message.contains("titel"),
            "the refusal names the argument: {message}"
        );
        assert!(
            message.contains("slug"),
            "and what is writable instead: {message}"
        );
    }

    #[test]
    fn an_empty_title_is_refused_but_an_empty_summary_clears() {
        plan(
            PAGE_UPDATE,
            &update(json!({ "title": "   " })),
            &current(),
            "About us",
            "r7",
            Vec::new(),
        )
        .expect_err("a blank title is refused");

        let cleared = ok_plan(json!({ "summary": "" }));
        let diff = cleared
            .diffs
            .iter()
            .find(|diff| diff.field == "summary")
            .expect("the summary is previewed");
        assert_eq!(
            diff.after,
            Some(Value::Null),
            "an empty summary clears the value"
        );
    }

    #[test]
    fn a_non_text_value_is_refused_with_the_type_it_got() {
        let error = plan(
            PAGE_UPDATE,
            &update(json!({ "title": 7 })),
            &current(),
            "About us",
            "r7",
            Vec::new(),
        )
        .expect_err("a number is not a title");
        assert!(error.to_string().contains("a number"), "{error}");
    }

    #[test]
    fn a_slug_is_lowercased_and_a_bad_one_is_refused() {
        let resolved = ok_plan(json!({ "slug": "About-Us" }));
        let diff = resolved
            .diffs
            .iter()
            .find(|diff| diff.field == "slug")
            .expect("previewed");
        assert_eq!(diff.after, Some(Value::from("about-us")));

        for bad in [
            "-leading",
            "trailing-",
            "has space",
            "Ünïcode",
            "",
            &"x".repeat(MAX_SLUG + 1),
        ] {
            let outcome = plan(
                PAGE_UPDATE,
                &update(json!({ "slug": bad })),
                &current(),
                "About us",
                "r7",
                Vec::new(),
            );
            assert!(outcome.is_err(), "`{bad}` was accepted as a slug");
        }
    }

    #[test]
    fn a_status_outside_the_page_vocabulary_is_refused() {
        let error = plan(
            PAGE_UPDATE,
            &update(json!({ "status": "live" })),
            &current(),
            "About us",
            "r7",
            Vec::new(),
        )
        .expect_err("`live` is not a page status");
        assert!(error.to_string().contains("draft"), "{error}");
    }

    #[test]
    fn an_operation_that_changes_nothing_is_refused() {
        let error = plan(
            PAGE_UPDATE,
            &update(json!({ "title": "About us" })),
            &current(),
            "About us",
            "r7",
            Vec::new(),
        )
        .expect_err("writing the value that is already there is not a decision");
        assert!(error.to_string().contains("changes nothing"), "{error}");
    }

    #[test]
    fn a_non_object_argument_is_refused_rather_than_panicking() {
        for bad in [json!("title"), json!(7), json!([1, 2]), Value::Null] {
            let outcome = plan(
                PAGE_UPDATE,
                &update(bad.clone()),
                &current(),
                "About us",
                "r7",
                Vec::new(),
            );
            assert!(outcome.is_err(), "{bad} was accepted as an argument object");
        }
    }

    // --- deletes ---

    #[test]
    fn a_delete_previews_its_cascades_and_writes_no_field() {
        let cascades = vec![
            Cascade {
                label: "page revision".to_owned(),
                count: 4,
            },
            Cascade {
                label: "comment".to_owned(),
                count: 2,
            },
        ];
        let op = Operation {
            kind: OpKind::Delete,
            resource_type: "page".to_owned(),
            resource_id: "p1".to_owned(),
            args: Value::Null,
        };
        let plan = plan(
            PAGE_UPDATE,
            &op,
            &current(),
            "About us",
            "r7",
            cascades.clone(),
        )
        .expect("a delete resolves");

        assert_eq!(
            plan.cascades, cascades,
            "the counts the reviewer reads are the counts kept"
        );
        assert!(
            writes(&plan, PAGE_UPDATE)
                .expect("a delete applies")
                .is_empty(),
            "a delete writes no column; the cascade is the consequence"
        );
    }

    // --- the stored shape ---

    #[test]
    fn a_preview_round_trips_through_storage() {
        let plan = ok_plan(json!({ "title": "About our team", "summary": "who we are" }));
        let stored = plan.to_preview(PAGE_UPDATE);

        assert!(is_supported_preview(&stored));
        assert_eq!(stored_diffs(&stored).expect("diffs"), plan.diffs);
        assert_eq!(stored_label(&stored, &plan.resource_id), "About us");
        assert_eq!(stored_cascades(&stored), Vec::new());

        let reread = Plan::from_preview(&stored).expect("it reads back");
        assert_eq!(reread.hash, plan.hash, "the hash survives the round trip");
        assert_eq!(reread.base_revision, plan.base_revision);
        assert_eq!(reread.resource_id, plan.resource_id);
    }

    #[test]
    fn a_preview_from_another_version_is_refused_rather_than_misread() {
        let stored = ok_plan(json!({ "title": "About our team" })).to_preview(PAGE_UPDATE);

        let mut future = stored.clone();
        future["version"] = json!(PREVIEW_VERSION + 1);
        assert!(!is_supported_preview(&future));
        assert!(stored_diffs(&future).is_none());
        assert!(Plan::from_preview(&future).is_err());

        // And a preview with no version at all — the shape slice 1 wrote by hand.
        assert!(Plan::from_preview(&json!({ "operations": [] })).is_err());
    }

    #[test]
    fn snapshot_keys_by_field_not_by_argument() {
        let values = map_of(json!({
            "title": "About us",
            "unrelated": "ignored",
        }));
        let snapshot = snapshot(PAGE_UPDATE, &values);
        assert_eq!(snapshot.get("title"), Some(&Value::from("About us")));
        assert_eq!(
            snapshot.get("unrelated"),
            None,
            "unmapped keys are not part of a target"
        );
    }

    #[test]
    fn the_current_state_is_read_by_column_name_so_before_is_never_a_false_create() {
        // The trap this guards: a caller keys `current` by argument name, every `before` comes
        // out `None`, and the preview claims the operation creates every field it touches.
        let values = map_of(json!({ "title": "About us" }));
        let plan = plan(
            PAGE_UPDATE,
            &update(json!({ "title": "New" })),
            &snapshot(PAGE_UPDATE, &values),
            "About us",
            "r1",
            Vec::new(),
        )
        .expect("the plan resolves");

        let diff = plan
            .diffs
            .iter()
            .find(|diff| diff.field == "title")
            .expect("previewed");
        assert_eq!(
            diff.before,
            Some(Value::from("About us")),
            "the value already on the target must appear as `before`"
        );
        assert!(diff.changes());
    }

    #[test]
    fn every_field_in_the_page_mapping_is_reachable() {
        // A spec nobody can name is a spec the model can never fill, and the test suite would
        // not notice because no test uses it.
        for spec in PAGE_UPDATE {
            let args = json!({ spec.arg: "ok" });
            let outcome = plan(
                PAGE_UPDATE,
                &update(args),
                &json!({}),
                "About us",
                "r1",
                Vec::new(),
            );
            let refusal = outcome
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default();
            assert!(
                !refusal.contains(&format!("does not write `{}`", spec.arg)),
                "`{}` is in the mapping but is refused as unknown",
                spec.arg
            );
        }
    }

    #[test]
    fn re_planning_a_plan_from_its_own_arguments_reproduces_the_same_write() {
        // The re-preview contract in one assertion: `plan` is pure in `(mapping, op, current)`,
        // so `arguments` inverts it exactly. Without this, a re-preview would re-derive a
        // *different* edit from the frozen one and the reviewer would be asked to approve
        // something the request never proposed.
        let original = ok_plan(json!({ "title": "About our team", "summary": "a short line" }));
        let args = original
            .arguments(PAGE_UPDATE)
            .expect("the arguments are readable");
        let replayed = plan(
            PAGE_UPDATE,
            &update(args),
            &current(),
            "About us",
            "r7",
            Vec::new(),
        )
        .expect("the replayed plan resolves");
        assert_eq!(
            replayed.diffs, original.diffs,
            "the replay must write exactly the fields the frozen preview wrote"
        );
        assert_eq!(replayed.hash, original.hash, "and bind to the same hash");
    }

    #[test]
    fn a_cleared_summary_round_trips_as_the_argument_that_clears_it_again() {
        // `after: Null` is how the preview spells a cleared summary. Reading it back as an
        // argument has to produce `""` — the one input that coerces back to `Null`. Reading it
        // back as `null` would be refused by `coerce` (it is not text), and reading it back as
        // the string "null" would write four characters into the page.
        let original = ok_plan(json!({ "summary": "" }));
        assert_eq!(original.diffs[0].after, Some(Value::Null));
        let args = original.arguments(PAGE_UPDATE).expect("readable");
        assert_eq!(args.get("summary"), Some(&Value::from("")));
        let replayed = plan(
            PAGE_UPDATE,
            &update(args),
            &current(),
            "About us",
            "r7",
            Vec::new(),
        )
        .expect("replays");
        assert_eq!(
            replayed.diffs[0].after,
            Some(Value::Null),
            "a re-preview must still clear, not store an empty string"
        );
        assert_eq!(replayed.hash, original.hash);
    }

    #[test]
    fn a_mapping_that_no_longer_knows_a_stored_argument_is_refused_rather_than_guessed() {
        // The stored preview is the frozen record. If the mapping is renamed under it, the
        // re-preview must say so instead of dropping the field — a re-preview that silently
        // re-plans fewer fields is a narrower proposal presented as the same one.
        let stored = ok_plan(json!({ "title": "New" }));
        let narrower: Mapping = &[FieldSpec {
            arg: "body",
            field: "body",
            kind: FieldKind::Body,
        }];
        let err = stored
            .arguments(narrower)
            .expect_err("`title` is not in this mapping")
            .to_string();
        assert!(
            err.contains("title"),
            "the refusal must name the field, got: {err}"
        );
    }

    #[test]
    fn the_rebuilt_operation_keeps_the_kind_and_the_target() {
        // A re-preview that dropped the `delete` kind would turn a deletion into an update —
        // the exact "approve a change that did not happen" failure the request forbids. The
        // target travels with it for the same reason: a re-preview against another row is not a
        // re-preview.
        let plan = Plan {
            kind: OpKind::Delete,
            resource_type: "page".to_owned(),
            resource_id: "22222222-2222-2222-2222-222222222222".to_owned(),
            label: "About us".to_owned(),
            diffs: Vec::new(),
            cascades: vec![Cascade {
                label: "page revisions".to_owned(),
                count: 3,
            }],
            base_revision: "r7".to_owned(),
            hash: "h".to_owned(),
        };
        let operation = plan
            .operation(PAGE_UPDATE)
            .expect("a delete carries no arguments");
        assert_eq!(operation.kind, OpKind::Delete);
        assert_eq!(
            operation.resource_id,
            "22222222-2222-2222-2222-222222222222"
        );
        assert_eq!(operation.resource_type, "page");
    }
}
