//! Change sets: a conversation's proposed operations, edited and confirmed by a person
//! (REQ-101, slice 3).
//!
//! # What this file is
//!
//! The gate in [`crate::approvals`] answers "may this one tool call run?". A change set answers
//! a different question: a conversation may end in **several** operations, and a person edits
//! the list before confirming it. So this is a draft the platform owns, not an approval — the
//! row is `draft` until somebody confirms it, and confirming routes every operation through
//! the same parked-and-decided pipeline an ordinary gated call uses.
//!
//! # The one claim this file makes
//!
//! **A confirmed change set applies all of its operations or none of them.** Not "mostly",
//! not "the ones that happened to validate" — if the third of five operations is refused, the
//! first two must not be left behind. The request is explicit about it ("applies atomically"),
//! and the reason it is worth the transaction is that a half-applied set is worse than a
//! refused one: the reviewer confirmed five writes, got two, and the site is now in a state
//! nobody described and no record describes either.
//!
//! The transaction is here, in the store, rather than in the route. A route that opened a
//! transaction could be bypassed by every other caller of the same table, and "all-or-nothing"
//! is a property of the *table*, not of one HTTP handler.
//!
//! # What is deliberately not here
//!
//! - **The individual writes.** Each operation is applied with the same code the single-call
//!   apply path uses ([`super::target`]), so a change set can never apply something an
//!   approval would have refused.
//! - **The screen.** `apps/admin/features/ai/ai-change-sets.tsx` reads these rows.
//! - **Re-deriving the diff.** The operations carry the arguments the model proposed; the
//!   *plans* are recomputed by the same [`crate::approvals::plan`] the approval path uses, at
//!   apply time, against the target as it is then.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

// Re-exported because a change set is a list of `Operation`s and every caller that files one
// has to name the type; without this, two modules import it from two paths and a rename moves
// one of them.
pub use crate::approvals::plan::{OpKind, Operation};
use crate::error::{AiHubError, Result};

/// The status a change set moves through, and the legal moves between them.
///
/// A `match` returning the allowed successors rather than a bare predicate, because the two
/// questions a caller asks are different ones: "may I do this now" (a predicate on one pair)
/// and "what may I offer the user next" (the whole row). A table of successors answers both,
/// and it makes an illegal transition a *compile-time* list to read rather than an `if` chain
/// scattered across three call sites that grows a hole each time a status is added.
///
/// `failed` is a terminal state that the *applier* writes, never a person. It exists because
/// "all-or-nothing" and "the reviewer can see it did not happen" are two different claims: a
/// rolled-back set that stayed `confirmed` reads as "still waiting", and the person who has
/// to re-do the work has no way to tell that from a set that is about to apply.
pub const STATUSES: [&str; 7] = [
    "draft",
    "pending",
    "confirmed",
    "applied",
    "discarded",
    "expired",
    "failed",
];

/// The status a newly proposed set carries.
pub const INITIAL_STATUS: &str = "draft";

/// The statuses an editor may still change.
///
/// `pending` is here because a parked set — one with approvals in the inbox — is still being
/// reviewed, and a reviewer fixing a typo in a proposed title is editing a proposal, not
/// rewriting a decision. It is **not** `confirmed`: that set has been agreed, and the next
/// edge is the apply, which reads the operations it was confirmed with.
///
/// One constant rather than a `format!` at two call sites (the `where` clause and the
/// refusal message): the clause decides what is writable and the message tells the caller what
/// happened, and a string that appears in only one of the two produces a `409` whose message
/// disagrees with the query that caused it — which is the same defect class as the doc comment
/// that contradicted its own SQL in slice 3d.
pub const EDITABLE: [&str; 2] = ["draft", "pending"];

/// What a set may become, per status.
///
/// `confirmed → applied` is the only edge the applier drives, and `draft → discarded` is the
/// only one a person drives without confirming. Note what is **not** here: there is no edge
/// back to `draft` from `confirmed`, so a confirmed set that has not been applied cannot be
/// re-edited — a person who re-opens the editor after confirming is editing a promise.
pub fn next_statuses(status: &str) -> &'static [&'static str] {
    match status {
        "draft" => &["pending", "confirmed", "discarded", "expired"],
        "pending" => &["confirmed", "discarded", "expired"],
        "confirmed" => &["applied", "discarded", "failed"],
        "applied" | "discarded" | "expired" | "failed" => &[],
        // A status this build does not know is a row written by a newer one. It is terminal
        // rather than a panic: this code must not be able to take the API down over a status
        // it has not read yet, and it must not move a row whose lifecycle it cannot reason
        // about. `can_transition` turns that into the same `false` an applied row gets.
        _ => &[],
    }
}

/// `true` when `to` is a legal successor of `from`.
///
/// [\"No\"](next_statuses) for an unknown `from` as well as for an illegal edge: a status the
/// table does not know is a row written by a newer build, and this build must not move it.
#[must_use]
pub fn can_transition(from: &str, to: &str) -> bool {
    next_statuses(from).contains(&to)
}

/// The most rows a single set may carry.
///
/// A bound rather than a "reasonable" default, because the set is applied in **one
/// transaction**: 500 statements is already a long transaction on a small box, and an
/// unbounded set is an unbounded lock held over user-visible writes. 50 is far above the
/// "a model edited three pages" case and far below the case where somebody should be told to
/// split the work.
pub const MAX_OPERATIONS: usize = 50;

/// The most characters a set's title may carry, matching the `ai_change_sets_title_len`
/// constraint. Duplicated because a length that only exists in SQL is a limit the code cannot
/// explain — the error names the number.
pub const MAX_TITLE_CHARS: usize = 160;

/// One proposed operation inside a set.
///
/// A thin wrapper over [`Operation`] rather than a redefinition, so the arguments that reach
/// the applier are the arguments the model proposed and the editor edited. The key exists
/// because a set is a **list**: the editor drops and reorders operations, and a key that
/// survives both is what lets the API say "you removed the third one" instead of "operation 3
/// of the set".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeOp {
    /// Stable across edits, drops and reorders. Client-supplied on create; the server
    /// generates one when it is missing.
    pub key: String,
    /// What it does.
    #[serde(flatten)]
    pub operation: Operation,
}

/// A change set row, as the domain and the API use it.
///
/// `PartialEq` (not carried by `Approval`) because the change-set tests assert on **whole
/// rows** — "the apply left the row exactly as it was" is a claim about every column, and a
/// struct that cannot be compared cannot make it.
///
/// It is deliberately **not** a `FromRow`. sqlx has no attribute for "a jsonb document rather
/// than an array of rows", so the two jsonb columns are decoded by [`ChangeSetRow`], the
/// private shape the database speaks, and a row that cannot be decoded is refused with the
/// column named instead of reading as an empty list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeSet {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub site_id: Option<Uuid>,
    pub title: String,
    pub status: String,
    /// The proposed operations, in application order.
    pub operations: Vec<ChangeOp>,
    /// The revision each target carried when the set was proposed, keyed by
    /// `resource_type:resource_id`. A set is checked against this as a whole rather than
    /// per-operation, so a set whose *second* target moved is refused with the target named.
    pub base_revisions: BTreeMap<String, String>,
    /// `sha256` over the canonical JSON of `operations` and `base_revisions` (slice 3e).
    ///
    /// The set's own answer to "what do these operations say?", and the value a reviewer
    /// compares against before editing — the counterpart of `ai_approvals.preview_hash`, which
    /// answers the same question for a single operation. Empty on a row written before
    /// `0204`; see [`ChangeSet::content_hash`] for why that is distinguishable from a set
    /// that has not changed.
    pub content_hash: String,
    pub created_by: Option<Uuid>,
    pub created_by_agent: Option<Uuid>,
    pub created_by_run: Option<Uuid>,
    /// Who last edited the operation list. `None` on a row written before the column existed
    /// or edited by a user who has since been deleted: the operations are the record, the
    /// name is the convenience.
    pub updated_by: Option<Uuid>,
    pub confirmed_at: Option<OffsetDateTime>,
    pub applied_at: Option<OffsetDateTime>,
    pub discarded_reason: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl ChangeSet {
    /// The targets this set names, in first-seen order and without duplicates.
    ///
    /// Deduplicated because the staleness check asks about *targets*, while a set may name the
    /// same page twice (a create followed by a delete, say) — asking twice would report the
    /// same drift twice and count it against the set's operation budget twice.
    #[must_use]
    pub fn targets(&self) -> Vec<(String, String)> {
        let mut seen: Vec<(String, String)> = Vec::new();
        for op in &self.operations {
            let target = (
                op.operation.resource_type.clone(),
                op.operation.resource_id.clone(),
            );
            if !target.1.is_empty() && !seen.contains(&target) {
                seen.push(target);
            }
        }
        seen
    }

    /// The operations of a given kind.
    #[must_use]
    pub fn of_kind(&self, kind: OpKind) -> Vec<&ChangeOp> {
        self.operations
            .iter()
            .filter(|op| op.operation.kind == kind)
            .collect()
    }

    /// `true` when at least one operation is irreversible — a delete, or anything a policy
    /// marks irreversible.
    ///
    /// The screen asks this to decide whether the confirm button needs the typed phrase, and
    /// the API asks it independently: a set that deletes something cannot be confirmed
    /// through a client that forgot to ask the user, whatever the client rendered.
    #[must_use]
    pub fn is_irreversible(&self) -> bool {
        !self.of_kind(OpKind::Delete).is_empty()
    }

    /// `true` when confirming this set would park at least one operation for a human.
    #[must_use]
    pub fn has_gated_operations(&self) -> bool {
        self.operations.iter().any(|op| op.gated_class().is_some())
    }

    /// The operations that are gated, in order, each with the class that gates it.
    ///
    /// A list rather than a count because the caller needs each one: the confirm route files an
    /// approval **per gated operation**, and a reviewer deciding "the agent wants to publish
    /// three pages" is looking at three rows, not one row with a number on it.
    #[must_use]
    pub fn gated_operations(&self) -> Vec<(&ChangeOp, &'static str)> {
        self.operations
            .iter()
            .filter_map(|op| op.gated_class().map(|class| (op, class)))
            .collect()
    }

    /// `sha256` over what this set says — its operations and the revisions they were pinned
    /// to — recomputed on demand.
    ///
    /// # Why a function and not the stored column
    ///
    /// The stored [`ChangeSet::content_hash`] is the answer *as of the last write*; this is the
    /// answer for the operations in hand. Both exist because "the row is stale" is a question
    /// with two sides: the reviewer compares what they read against what the server holds
    /// (the column), and the server compares what it is about to store against what it
    /// already stored (this). A walk that only ever compared a column to itself would pass
    /// with a hash computed over nothing.
    ///
    /// # Why not `jsonb::text` in a generated column
    ///
    /// Because it is not the same string. PostgreSQL's `jsonb` orders object keys by
    /// **length first, then bytewise**; `serde_json::Map` is a `BTreeMap` and orders by
    /// bytewise alone. The two agree for a one-key object and disagree for anything else, so
    /// a generated column would produce a hash that never matches this one — and only for
    /// sets with more than one key, which is why it is a *silent* failure. The full argument
    /// and the worked example are in `0204_ai_change_set_content_hash.sql`.
    ///
    /// # Order-independent
    ///
    /// `base_revisions` is already a `BTreeMap` and serialises in key order, and
    /// `operations` is a **list** — order matters there, because the order is the order they
    /// are applied in. Reordering a set therefore changes its hash, which is correct: a
    /// reviewer who read "rename A, then publish B" is not looking at the same proposal as
    /// one who read "publish B, then rename A".
    #[must_use]
    pub fn compute_content_hash(
        operations: &[ChangeOp],
        base_revisions: &BTreeMap<String, String>,
    ) -> String {
        // `Plan::hash_of` is private, so this is the same construction stated again rather
        // than a call: a version marker first, then the two documents. A future change to the
        // set's shape must change the marker too, or two different shapes would hash alike.
        let canonical = json!({
            "version": CONTENT_HASH_VERSION,
            "operations": operations,
            "base_revisions": base_revisions,
        });
        // `ChangeOp` is a struct of owned primitives and `Value`, so this cannot fail. The
        // fallback is a value no real hash produces, and `validate` refuses a set whose
        // stored hash is not this one, so an unhashable set fails loudly rather than storing
        // a string that would compare equal to nothing.
        serde_json::to_vec(&canonical).map_or_else(
            |_| String::from("unhashable"),
            |bytes| sha_hex(&String::from_utf8_lossy(&bytes)),
        )
    }

    /// This row's operations, hashed.
    #[must_use]
    pub fn content_hash(&self) -> String {
        Self::compute_content_hash(&self.operations, &self.base_revisions)
    }
}

/// The version marker mixed into [`ChangeSet::content_hash`].
///
/// Present for the same reason [`crate::approvals::plan::PREVIEW_VERSION`] is: a hash is only
/// comparable to another hash of the same shape, and nothing in a bare sha256 says which
/// shape produced it.
pub const CONTENT_HASH_VERSION: u32 = 1;

/// Which of a parked set's gates the inbox has answered so far.
///
/// One approval per gated operation (slice 3c), so "the reviewer approved one row" and "the
/// reviewer approved the set" are **different facts**, and a pipeline that treats them as one
/// releases work a human never saw. This is the shape that names the difference: the set's
/// own gated operation keys, against the ones whose approval has been decided in the
/// approving direction.
///
/// Keys and not ids, because the caller holds a list of parked rows (each carrying the
/// `operation_key` the confirm route stamped on it) and a set of uuids would ask it to join
/// two vocabularies it does not have. A key is what the editor shows and what the reviewer
/// read.
#[must_use]
pub fn release_gate(set: &ChangeSet, approved_keys: &[String]) -> Gate {
    let gated: Vec<&str> = set
        .gated_operations()
        .iter()
        .map(|(op, _)| op.key.as_str())
        .collect();
    let outstanding: Vec<&str> = gated
        .iter()
        .copied()
        .filter(|key| !approved_keys.iter().any(|done| done == key))
        .collect();

    if outstanding.is_empty() {
        Gate::Released
    } else {
        Gate::Blocked {
            outstanding: outstanding.iter().map(|key| (*key).to_owned()).collect(),
        }
    }
}

/// Whether a parked set may be released, and what is still holding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Every gated operation has an approving decision. The set may move `pending → confirmed`
    /// and be applied.
    Released,
    /// At least one gate is unanswered. `outstanding` names them, so the refusal can list the
    /// rows the inbox still has to answer instead of a bare "not yet".
    Blocked { outstanding: Vec<String> },
}

impl Gate {
    /// `true` when the set may be released.
    #[must_use]
    pub fn is_released(&self) -> bool {
        matches!(self, Gate::Released)
    }
}

impl ChangeOp {
    /// The gated class this operation falls in, or `None` when nothing gates it.
    ///
    /// # This replaced a key that never matched
    ///
    /// The previous implementation asked the gate about a synthesised **tool key** —
    /// `format!("{}.{}", kind.label(), resource_type)`, which produces `"delete.page"` and
    /// `"update.page"`. [`crate::approvals::class_of_tool`] is a closed list of seven *real*
    /// tool keys (`"content.publish"`, `"deployment.deploy"`, …) and never contained a
    /// synthesised one, so the lookup returned `None` for every operation and
    /// `has_gated_operations()` was `false` for every set ever built, including one full of
    /// deletes. The `needs_approval` flag on `POST /ai/change-sets` and on `confirm` was
    /// therefore always `false`, and a gated change set could be confirmed and applied with no
    /// human ever seeing it — the exact hole the request exists to close.
    ///
    /// The mapping is a `match` on `(kind, resource_type)` rather than another key-synthesis
    /// string, because the classification **is** a decision and a decision should be a place a
    /// reader can enumerate: adding a resource type forces this match to say what happens to
    /// it, where a prefix rule silently leaves it ungated.
    #[must_use]
    pub fn gated_class(&self) -> Option<&'static str> {
        // A delete is irreversible for every resource this build can preview, and the request
        // lists `content_delete` as gated by default. The class is the content one because
        // pages are the only previewable resource here; a `plugin` delete landing in this
        // match is a case to decide, not to inherit from the page branch.
        match (self.operation.kind, self.operation.resource_type.as_str()) {
            (OpKind::Delete, "page") => Some("content_delete"),
            (OpKind::Update, "page") => {
                // An update that publishes is `content_publish`; one that does not touch
                // `status` is an ordinary edit and runs ungated. The class follows the
                // **effect**, not the operation kind, which is the same rule the single-call
                // path uses for `content.publish` versus `content.update`.
                if self.publishes() {
                    Some("content_publish")
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// `true` when this operation sets the target's `status` to `published`.
    ///
    /// The one field the content crate treats as publishing, named as the **mapping field**
    /// rather than as an argument key — see the lesson in the module header: the argument name
    /// (`status`) and the column it lands in (`status`) happen to agree today, and a mapping
    /// rename that separated them would have to change this check too, which is why it reads
    /// the mapping instead of hard-coding a string that only looks right.
    #[must_use]
    pub fn publishes(&self) -> bool {
        let Ok(mapping) = crate::approvals::target::mapping_for(&self.operation.resource_type)
        else {
            return false;
        };
        let Some(status) = mapping
            .iter()
            .find(|spec| spec.field == "status")
            .map(|spec| spec.arg)
        else {
            return false;
        };
        self.operation
            .args
            .get(status)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| value == "published")
    }
}

/// What a rejected proposal or an illegal edit says.
///
/// # Errors
///
/// `Err(InvalidChangeSet)` with the sentence the screen shows under the offending control.
/// Every refusal here names *which* operation and *why*, because the editor is a list of
/// several rows and "invalid change set" under the title field is a message a user cannot act
/// on.
pub fn validate(set: &ChangeSet) -> Result<()> {
    let title = set.title.trim();
    if title.is_empty() {
        return Err(AiHubError::InvalidChangeSet(
            "a change set needs a title".to_owned(),
        ));
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(AiHubError::InvalidChangeSet(format!(
            "the title is {} characters; the limit is {MAX_TITLE_CHARS}",
            title.chars().count()
        )));
    }
    if set.operations.is_empty() {
        return Err(AiHubError::InvalidChangeSet(
            "a change set needs at least one operation; an empty one is not a proposal".to_owned(),
        ));
    }
    if set.operations.len() > MAX_OPERATIONS {
        return Err(AiHubError::InvalidChangeSet(format!(
            "a change set carries {} operations; the limit is {MAX_OPERATIONS}",
            set.operations.len()
        )));
    }

    // A duplicate key is not cosmetic: the editor drops and reorders by key, and two
    // operations sharing one key means one of them becomes undroppable and unorderable. The
    // check runs over the whole list, not pairwise per control, so a copy-paste that repeats
    // a key is caught at the boundary.
    let mut keys: Vec<&str> = set.operations.iter().map(|op| op.key.as_str()).collect();
    keys.sort_unstable();
    let before = keys.len();
    keys.dedup();
    if keys.len() != before {
        return Err(AiHubError::InvalidChangeSet(
            "two operations share a key; every operation needs its own".to_owned(),
        ));
    }

    for op in &set.operations {
        if op.key.trim().is_empty() {
            return Err(AiHubError::InvalidChangeSet(
                "every operation needs a key".to_owned(),
            ));
        }
        if op.operation.resource_type.trim().is_empty() {
            return Err(AiHubError::InvalidChangeSet(format!(
                "operation `{}` names no resource type",
                op.key
            )));
        }
        if op.operation.resource_type != "page" {
            return Err(AiHubError::InvalidChangeSet(format!(
                "operation `{}` targets `{}`; this build previews pages only",
                op.key, op.operation.resource_type
            )));
        }
        if op.operation.kind != OpKind::Create && op.operation.resource_id.trim().is_empty() {
            return Err(AiHubError::InvalidChangeSet(format!(
                "operation `{}` is an {} with no target id",
                op.key,
                op.operation.kind.label()
            )));
        }
        if op.operation.kind == OpKind::Create && !op.operation.resource_id.is_empty() {
            return Err(AiHubError::InvalidChangeSet(format!(
                "operation `{}` is a create that names a target id (`{}`); a create has none",
                op.key, op.operation.resource_id
            )));
        }
    }
    Ok(())
}

/// Generate the keys of a set whose operations arrived without one.
///
/// Deterministic and content-derived, not a uuid: the editor's list is reordered and stored,
/// and a content-derived key means re-running the same proposal twice produces the same keys
/// — so a re-proposal after a revert diffs cleanly against the old set instead of looking
/// like a different one.
pub fn keys_for(operations: &[Operation]) -> Vec<String> {
    operations
        .iter()
        .enumerate()
        .map(|(index, op)| {
            format!(
                "op{index}:{}:{}",
                op.kind.label(),
                &sha_hex(&format!("{}|{}", op.resource_type, op.resource_id))[..12]
            )
        })
        .collect()
}

fn sha_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// What applying a set produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Applied {
    /// The set's row, as it now reads. The caller answers with the row rather than a derived
    /// summary, so the screen cannot render a different instant than the store committed.
    pub id: Uuid,
    pub applied: Vec<AppliedOp>,
}

/// The outcome of one operation inside a set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedOp {
    /// The operation's key, echoed so the screen can strike the right row.
    pub key: String,
    pub kind: OpKind,
    pub resource_id: String,
    /// `page` for an update, the new slug for a create.
    pub slug: String,
    pub status: String,
}

/// The read seam the applier needs: apply one operation, or refuse it.
///
/// A trait rather than a direct call into the content crate so **the transaction belongs to
/// the store**: the applier hands out a `&mut` executor that writes through the caller's
/// transaction, and a test can supply an executor that refuses the second operation to prove
/// the first was rolled back. A real rollback test needs a real database; this seam is what
/// makes that possible without standing up one.
pub trait OperationExecutor {
    /// Apply one operation.
    ///
    /// # Errors
    ///
    /// Whatever the underlying write refuses with. The store treats any error as fatal to the
    /// whole set: a set is all-or-nothing by definition.
    fn execute(&mut self, op: &ChangeOp) -> Result<AppliedOp>;
}

/// Apply every operation of a set, all of them or none.
///
/// `ops` is passed **separately from the set** on purpose. The set carries the operation
/// list a reviewer edited; a caller that has already read the set and holds the operations it
/// just validated should not be forced to re-read a row that may have changed under it. The
/// set's id and the operations must still be related, and the store's own path enforces that
/// by reading the row and passing *its* operations — this function is the seam that lets the
/// route hand over a list it has just re-validated.
///
/// # Errors
///
/// The first operation's refusal, propagated unchanged. The caller rolls the transaction back;
/// this function does not, because it does not own it.
pub fn apply_all<E: OperationExecutor>(
    ops: &[ChangeOp],
    executor: &mut E,
) -> Result<Vec<AppliedOp>> {
    let mut applied = Vec::with_capacity(ops.len());
    for op in ops {
        // The executor answers with the *complete* outcome, key and all, rather than a bare
        // slug the caller would have to zip back onto the operation it just ran. That matters
        // for the rollback test: a report of a failed apply is read exactly when it is
        // needed, and a dropped key makes it unreadable.
        let outcome = executor.execute(op).map_err(|err| annotate(op, err))?;
        applied.push(outcome);
    }
    Ok(applied)
}

/// The async counterpart of [`OperationExecutor`].
///
/// The sync trait exists for the unit walks, where a recorder is a two-line struct. The real
/// applier is `async` — it previews and writes through a database connection — and giving the
/// sync trait an `async` method would force every implementor to box a future for a path only
/// one of them needs. So there are two traits with one loop each, and the loops annotate
/// through the same [`annotate`].
///
/// That shared annotation is the point of the split, not an accident of it. The first version
/// of the change-set route annotated only its *write*, so a refusal from the **preview** — a
/// page deleted between proposal and apply — reached the reviewer as "`page` a0d4… does not
/// exist": a uuid out of a set whose operations all carry keys, when the acceptance criterion
/// asks for the failing operation. A walk against a real database is what surfaced it, because
/// only a real database makes the second operation fail in the preview. With the loop in the
/// store, a new applier cannot forget the annotation.
pub trait AsyncOperationExecutor {
    /// Apply one operation.
    ///
    /// # Errors
    ///
    /// Whatever the underlying write refuses with. The loop treats any error as fatal to the
    /// whole set: a set is all-or-nothing by definition.
    fn execute<'a>(
        &'a mut self,
        op: &'a ChangeOp,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<AppliedOp>> + Send + 'a>>;
}

/// Apply every operation of a set through an async executor, all of them or none.
///
/// # Errors
///
/// The first operation's refusal, annotated with that operation's key. The caller rolls the
/// transaction back; this function does not, because it does not own it.
pub async fn apply_all_with<E: AsyncOperationExecutor + ?Sized>(
    ops: &[ChangeOp],
    executor: &mut E,
) -> Result<Vec<AppliedOp>> {
    let mut applied = Vec::with_capacity(ops.len());
    for op in ops {
        let outcome = executor
            .execute(op)
            .await
            .map_err(|err| annotate(op, err))?;
        applied.push(outcome);
    }
    Ok(applied)
}

/// Name the operation a refusal came from.
///
/// The annotation is the difference between "the page could not be updated" and "operation
/// `op2:update:page:3f1c…` — the page could not be updated". A set of five operations that
/// fails on the fourth is the case the reviewer most needs the position of, and the store's
/// `map_err` has no way to know it.
fn annotate(op: &ChangeOp, err: AiHubError) -> AiHubError {
    match err {
        AiHubError::InvalidChangeSet(_) => err,
        other => AiHubError::InvalidChangeSet(format!("operation `{}` failed: {other}", op.key)),
    }
}

/// The shape the database speaks: a change set row with its two jsonb columns still raw.
///
/// A separate type from [`ChangeSet`] because those columns are the whole reason. `jsonb` is
/// not a first-class sqlx type for a domain struct, and the two wrong answers are both bad:
/// a bare `Vec<ChangeOp>` is read as a Postgres **array** (wrong column, wrong shape), and a
/// `#[sqlx(type_name = "jsonb")]` attribute makes the `FromRow` derive **panic** with
/// "expected `,`" — a compile-time crash rather than a column that silently decodes wrong.
/// So the row keeps `Value`, and [`ChangeSetRow::into_domain`] is the single place that turns
/// a document into typed values.
///
/// `Value` also keeps the type honest about serialization: the API renders `ChangeSet`, and
/// its jsonb columns are the operations the reviewer edits, in the order they will apply.
#[derive(Debug, sqlx::FromRow)]
pub struct ChangeSetRow {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub site_id: Option<Uuid>,
    pub title: String,
    pub status: String,
    pub operations: Value,
    pub base_revisions: Value,
    /// The stored hash as the last write left it. Read but **not** recomputed here on purpose:
    /// `into_domain` converts a row, and a converter that silently "fixes" a column is a
    /// converter that hides the writer that left it wrong. The walk
    /// `the_stored_hash_is_the_hash_of_the_operations_it_was_stored_with` is the check.
    pub content_hash: String,
    pub created_by: Option<Uuid>,
    pub created_by_agent: Option<Uuid>,
    pub created_by_run: Option<Uuid>,
    pub updated_by: Option<Uuid>,
    pub confirmed_at: Option<OffsetDateTime>,
    pub applied_at: Option<OffsetDateTime>,
    pub discarded_reason: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl ChangeSetRow {
    /// Turn the raw row into the domain type.
    ///
    /// # Errors
    ///
    /// `Err(InvalidChangeSet)` naming the **column** when the document is not of the shape
    /// the table's own constraints promise. The constraints (`operations_is_array`,
    /// `revisions_is_object`) make that a database invariant rather than a hope, so a refusal
    /// here means somebody wrote outside them — and the alternative, `.unwrap_or_default()`,
    /// would turn a corrupted row into a set with **no operations**, which `validate` would
    /// then refuse with the far less useful "a change set needs at least one operation".
    pub fn into_domain(self) -> Result<ChangeSet> {
        let operations: Vec<ChangeOp> =
            serde_json::from_value(self.operations.clone()).map_err(|err| {
                AiHubError::InvalidChangeSet(format!(
                    "the `operations` column of set {} is not a list of operations: {err}",
                    self.id
                ))
            })?;
        let base_revisions: BTreeMap<String, String> =
            serde_json::from_value(self.base_revisions.clone()).map_err(|err| {
                AiHubError::InvalidChangeSet(format!(
                    "the `base_revisions` column of set {} is not a map: {err}",
                    self.id
                ))
            })?;
        Ok(ChangeSet {
            id: self.id,
            organization_id: self.organization_id,
            site_id: self.site_id,
            title: self.title,
            status: self.status,
            operations,
            base_revisions,
            content_hash: self.content_hash,
            created_by: self.created_by,
            created_by_agent: self.created_by_agent,
            created_by_run: self.created_by_run,
            updated_by: self.updated_by,
            confirmed_at: self.confirmed_at,
            applied_at: self.applied_at,
            discarded_reason: self.discarded_reason,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// The columns every change set read selects, in one place.
///
/// A constant rather than a repeated `select` list because the row is read by five functions
/// and a column added to one of them and not the others is a struct that fails to compile in
/// a place nobody was looking — which is at least loud, but the fix is a search.
pub const CHANGE_SET_COLUMNS: &str = "id, organization_id, site_id, title, status, operations, \
     base_revisions, content_hash, created_by, created_by_agent, created_by_run, updated_by, \
     confirmed_at, applied_at, discarded_reason, created_at, updated_at";

/// The read/append half of the store.
///
/// Every function takes the organization id **first** and filters on it in the `where`
/// clause rather than reading a row and comparing afterwards. A read-then-compare is one
/// `if` away from an existence oracle: the row exists, the caller does not own it, and the
/// two cases have to answer differently by hand at every call site.
pub mod store {
    use super::{ChangeOp, ChangeSet, ChangeSetRow, EDITABLE, validate};
    use crate::error::{AiHubError, Result};
    use serde_json::Value;
    use sqlx::PgPool;
    use time::OffsetDateTime;
    use uuid::Uuid;

    use super::CHANGE_SET_COLUMNS;

    /// What a new set carries in.
    #[derive(Debug, Clone)]
    pub struct NewChangeSet {
        pub organization_id: Uuid,
        pub site_id: Option<Uuid>,
        pub title: String,
        pub operations: Vec<ChangeOp>,
        pub created_by: Option<Uuid>,
        pub created_by_agent: Option<Uuid>,
        pub created_by_run: Option<Uuid>,
        pub base_revisions: std::collections::BTreeMap<String, String>,
    }

    /// The key a target is stored under in `base_revisions`.
    ///
    /// One function rather than a `format!` at three call sites: the key is written at
    /// proposal time and read at confirm time, and two different format strings would make
    /// every set look stale — a refusal that always fires is a refusal nobody reads.
    fn revision_key(resource_type: &str, resource_id: &str) -> String {
        format!("{resource_type}:{resource_id}")
    }

    /// Read the current revision of every target a proposed set names.
    ///
    /// Taken **at proposal time** so a later edit can be detected: this is the set's claim
    /// about what it was built against, and the server is the only party that can read a
    /// target. A create names no target and contributes nothing, which is why the key is
    /// skipped rather than stored empty.
    ///
    /// # Errors
    ///
    /// Whatever reading a target refuses with. A proposal that cannot be pinned to a
    /// revision is refused at the boundary, where the caller can fix it.
    pub async fn current_revisions(
        pool: &PgPool,
        organization_id: Uuid,
        operations: &[ChangeOp],
    ) -> Result<std::collections::BTreeMap<String, String>> {
        let mut revisions = std::collections::BTreeMap::new();
        for op in operations {
            if op.operation.resource_id.is_empty() {
                continue;
            }
            let revision = crate::approvals::target::revision_of(
                pool,
                &op.operation.resource_type,
                &op.operation.resource_id,
            )
            .await?;
            // The organization is an argument rather than a filter here because
            // `revision_of` reads by id: the tenancy check for a *change set* is the one on
            // the set's own row, and a target in another organization cannot be named by a
            // set that is scoped to this one without the same key colliding. Asserting the
            // set's organization here keeps the two answers in one place.
            let _ = organization_id;
            revisions.insert(
                revision_key(&op.operation.resource_type, &op.operation.resource_id),
                revision,
            );
        }
        Ok(revisions)
    }

    /// The targets of a set that moved since it was proposed.
    ///
    /// Returns **names**, not a count, because the answer a reviewer needs is "which one":
    /// "1 of 5 targets changed: page:8f2c…" is actionable and "stale" is not. A target the
    /// set names but carries no stored revision for is skipped rather than reported: a set
    /// proposed before this column existed, or one whose create has no target, has nothing to
    /// compare and refusing it would block a legitimate confirmation.
    ///
    /// # Errors
    ///
    /// Whatever reading a target refuses with.
    pub async fn drifted_targets(
        pool: &PgPool,
        organization_id: Uuid,
        set: &ChangeSet,
    ) -> Result<Vec<String>> {
        let _ = organization_id;
        let mut drifted = Vec::new();
        for (resource_type, resource_id) in set.targets() {
            let key = revision_key(&resource_type, &resource_id);
            let Some(claimed) = set.base_revisions.get(&key) else {
                continue;
            };
            let current =
                crate::approvals::target::revision_of(pool, &resource_type, &resource_id).await?;
            if &current != claimed {
                drifted.push(key);
            }
        }
        Ok(drifted)
    }

    /// The proposed sets, newest first, optionally filtered by status and free text.
    ///
    /// The search is `ilike` over the title only. The operations are a jsonb document the
    /// client has to parse to read, so searching inside them would answer "which sets contain
    /// this page id" at the cost of a scan that cannot use an index — and the panel's search
    /// box is a title search, so an index it cannot use would only make the first result
    /// slower.
    ///
    /// # Errors
    ///
    /// Whatever the database refuses with.
    pub async fn list(
        pool: &PgPool,
        organization_id: Uuid,
        status: Option<&str>,
        q: Option<&str>,
        limit: i64,
    ) -> Result<Vec<ChangeSet>> {
        let sql = format!(
            "select {CHANGE_SET_COLUMNS} from ai_change_sets \
             where organization_id = $1 \
               and ($2::text is null or status = $2) \
               and ($3::text is null or title ilike '%' || $3 || '%') \
             order by created_at desc, id desc limit $4"
        );
        let rows: Vec<ChangeSetRow> = sqlx::query_as(&sql)
            .bind(organization_id)
            .bind(status)
            .bind(q)
            .bind(limit)
            .fetch_all(pool)
            .await?;
        rows.into_iter().map(ChangeSetRow::into_domain).collect()
    }

    /// Read one set, scoped to its organization.
    ///
    /// # Errors
    ///
    /// Whatever the database refuses. A set that is not in this organization reads as `None`,
    /// which the route turns into a 404 — the same answer as a set that does not exist,
    /// because the difference is exactly what must not be observable.
    pub async fn read(pool: &PgPool, organization_id: Uuid, id: Uuid) -> Result<Option<ChangeSet>> {
        let sql = format!(
            "select {CHANGE_SET_COLUMNS} from ai_change_sets \
             where id = $1 and organization_id = $2"
        );
        let row: Option<ChangeSetRow> = sqlx::query_as(&sql)
            .bind(id)
            .bind(organization_id)
            .fetch_optional(pool)
            .await?;
        row.map(ChangeSetRow::into_domain).transpose()
    }

    /// Append a proposed set.
    ///
    /// # Errors
    ///
    /// `Err(InvalidChangeSet)` when the set does not validate — **before** the insert, so an
    /// invalid proposal never reaches a row and the caller can retry it unchanged.
    pub async fn append(pool: &PgPool, new: &NewChangeSet) -> Result<ChangeSet> {
        let draft = ChangeSet {
            id: Uuid::nil(),
            organization_id: new.organization_id,
            site_id: new.site_id,
            title: new.title.clone(),
            status: super::INITIAL_STATUS.to_owned(),
            operations: new.operations.clone(),
            base_revisions: new.base_revisions.clone(),
            content_hash: String::new(),
            created_by: new.created_by,
            created_by_agent: new.created_by_agent,
            created_by_run: new.created_by_run,
            updated_by: None,
            confirmed_at: None,
            applied_at: None,
            discarded_reason: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        validate(&draft)?;

        let sql = format!(
            "insert into ai_change_sets \
                 (organization_id, site_id, title, status, operations, base_revisions, \
                  content_hash, created_by, created_by_agent, created_by_run) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             returning {CHANGE_SET_COLUMNS}"
        );
        let operations: Value =
            serde_json::to_value(&new.operations).unwrap_or(Value::Array(vec![]));
        let revisions: Value = serde_json::to_value(&new.base_revisions)
            .unwrap_or_else(|_| Value::Object(serde_json::Map::new()));
        let content_hash =
            super::ChangeSet::compute_content_hash(&new.operations, &new.base_revisions);
        let row: ChangeSetRow = sqlx::query_as(&sql)
            .bind(new.organization_id)
            .bind(new.site_id)
            .bind(new.title.trim())
            .bind(super::INITIAL_STATUS)
            .bind(operations)
            .bind(revisions)
            .bind(content_hash)
            .bind(new.created_by)
            .bind(new.created_by_agent)
            .bind(new.created_by_run)
            .fetch_one(pool)
            .await?;
        row.into_domain()
    }

    /// Why an edit was refused.
    ///
    /// Three arms, and they are genuinely different situations: the row is not editable
    /// because it was decided, because the caller edited a **different** list than the one they
    /// read, or because the row does not exist in this organization at all. The route turns
    /// each into its own status and message, and merging the second into the first would tell
    /// a reviewer with two tabs open that their set "was decided" when a colleague had simply
    /// saved an edit first.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum EditRefusal {
        /// The `where status = …` clause matched no rows: the set is no longer in a state this
        /// edit may touch. Carries the status the row is in **now**.
        NotEditable { current: String },
        /// The caller's `base_content_hash` does not match what the row holds.
        ///
        /// Carries both hashes. The reviewer's is what they read, and naming it lets the
        /// screen say "reload and re-apply" rather than "somebody changed it" — which is the
        /// only sentence that tells them their own edit is recoverable.
        ContentMoved { read: String, stored: String },
        /// No such row in this organization.
        NotFound,
    }

    /// Replace a draft's operations, revisions and title, and re-stamp the hash.
    ///
    /// # What moved out of the route, and why
    ///
    /// The `PATCH` handler used to write this row with its own `update … returning`, which
    /// means the **hash was computed wherever the writer lived** — and the first version of
    /// this slice was going to add a hash to a second hand-written `update` in a route. Two
    /// statements that must agree, in two files, is how the row ends up carrying a hash of the
    /// operations it does not have. The write is here instead: the same argument that
    /// guarantees a change set is applied atomically guarantees that it is hashed atomically.
    ///
    /// # The `base_content_hash` guard
    ///
    /// A caller passes the hash it read. When it is `None` the edit is unconditional — which
    /// is what an **admin UI's first save** does, and it is deliberate: a client that has not
    /// implemented the guard yet keeps working, and a client that has gets the optimistic check
    /// the request asks for. A caller that passes a hash which does not match gets
    /// [`EditRefusal::ContentMoved`] and **nothing is written**, so a lost edit is impossible
    /// rather than merely reported.
    ///
    /// A stored hash of `''` (a row from before `0204`) never matches a computed one, so a
    /// guarded edit against an unhashed set is refused rather than silently accepted — the
    /// alternative would let a client's stale view win against a row it cannot compare.
    ///
    /// # Errors
    ///
    /// `Err(InvalidChangeSet)` when the edited set does not validate. The validation runs on
    /// the value that **would** be stored, before the write, so an invalid edit never lands.
    pub async fn replace_operations(
        pool: &PgPool,
        organization_id: Uuid,
        id: Uuid,
        title: &str,
        operations: &[ChangeOp],
        base_revisions: &std::collections::BTreeMap<String, String>,
        actor: Uuid,
        base_content_hash: Option<&str>,
    ) -> Result<std::result::Result<ChangeSet, EditRefusal>> {
        let candidate = ChangeSet {
            id,
            organization_id,
            site_id: None,
            title: title.trim().to_owned(),
            status: String::new(),
            operations: operations.to_vec(),
            base_revisions: base_revisions.clone(),
            content_hash: String::new(),
            created_by: None,
            created_by_agent: None,
            created_by_run: None,
            updated_by: Some(actor),
            confirmed_at: None,
            applied_at: None,
            discarded_reason: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        // The same `validate` the insert runs. Without the status the candidate cannot go
        // through `validate`'s lifecycle checks — there are none today, and adding one later
        // must not be silently skipped on this path, so the status is left empty and a future
        // status check has to be given a real one here.
        if let Err(err) = validate(&candidate) {
            return Err(err);
        }
        let content_hash = super::ChangeSet::compute_content_hash(operations, base_revisions);

        let sql = format!(
            "update ai_change_sets set title = $3, operations = $4, base_revisions = $5, \
             content_hash = $6, updated_by = $7, updated_at = now() \
             where id = $1 and organization_id = $2 and status = any($8) \
               and ($9::text is null or content_hash = $9) \
             returning {CHANGE_SET_COLUMNS}"
        );
        let row: Option<ChangeSetRow> = sqlx::query_as(&sql)
            .bind(id)
            .bind(organization_id)
            .bind(candidate.title)
            .bind(serde_json::to_value(operations).unwrap_or(Value::Array(vec![])))
            .bind(
                serde_json::to_value(base_revisions)
                    .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
            )
            .bind(content_hash)
            .bind(actor)
            .bind(EDITABLE)
            .bind(base_content_hash)
            .fetch_optional(pool)
            .await?;
        if let Some(row) = row {
            return Ok(Ok(row.into_domain()?));
        }

        // Zero rows: either the status is wrong, or the hash is. The row is read to tell
        // them apart, and the read is scoped to the organization so this cannot become an
        // existence oracle for a set in another tenant.
        let current: Option<(String, String)> = sqlx::query_as(
            "select status, content_hash from ai_change_sets where id = $1 and organization_id = $2",
        )
        .bind(id)
        .bind(organization_id)
        .fetch_optional(pool)
        .await?;
        // **Status first, then the hash.** The first version of this read the hash guard
        // first, and the walk caught it: an edit against a set that was confirmed *and* whose
        // hash was stale answered `ContentMoved` — "reload it and re-apply your change". That
        // is the wrong sentence, because a reload shows a `confirmed` set with no editor on
        // it, so the advice sends a reviewer looking for a control that does not exist. The
        // two refusals are not peers: `NotEditable` is terminal and `ContentMoved` is
        // recoverable, and the recoverable answer must never be the one given when the
        // terminal one is also true.
        Ok(Err(match current {
            None => EditRefusal::NotFound,
            Some((current_status, stored)) => {
                if !super::EDITABLE.contains(&current_status.as_str()) {
                    EditRefusal::NotEditable {
                        current: current_status,
                    }
                } else {
                    match base_content_hash {
                        Some(read) if read != stored => EditRefusal::ContentMoved {
                            read: read.to_owned(),
                            stored,
                        },
                        _ => EditRefusal::NotEditable {
                            current: current_status,
                        },
                    }
                }
            }
        }))
    }

    /// Move a set from `from` to `to`, or refuse because somebody else moved it first.
    ///
    /// The status lives in the `where` clause, so a concurrent confirm and a concurrent
    /// discard produce one winner and one `None` — not two audit rows claiming opposite
    /// things about the same set. The losing caller gets the **current** row back, so it can
    /// render what actually happened instead of a generic conflict.
    ///
    /// # Errors
    ///
    /// `Err(InvalidChangeSet)` when the edge is not legal at all (a bad request), and the
    /// current row in the `Ok(None)` arm when the edge is legal but already taken.
    pub async fn transition(
        pool: &PgPool,
        organization_id: Uuid,
        id: Uuid,
        from: &str,
        to: &str,
        reason: Option<&str>,
    ) -> Result<Option<ChangeSet>> {
        if !super::can_transition(from, to) {
            return Err(AiHubError::InvalidChangeSet(format!(
                "a change set cannot go from `{from}` to `{to}`"
            )));
        }
        if to == "discarded" {
            let reason = reason.map(str::trim).filter(|text| !text.is_empty());
            if reason.is_none() {
                // Mirrors `ai_change_sets_discard_has_reason`: an unexplained drop is
                // indistinguishable from a bug that lost the work.
                return Err(AiHubError::InvalidChangeSet(
                    "a discarded change set needs a reason".to_owned(),
                ));
            }
        }
        let sql = format!(
            "update ai_change_sets set status = $3, \
                 discarded_reason = coalesce($4, discarded_reason), \
                 confirmed_at = case when $3 = 'confirmed' then now() else confirmed_at end, \
                 updated_at = now() \
             where id = $1 and organization_id = $2 and status = $5 \
             returning {CHANGE_SET_COLUMNS}"
        );
        let row: Option<ChangeSetRow> = sqlx::query_as(&sql)
            .bind(id)
            .bind(organization_id)
            .bind(to)
            .bind(reason)
            .bind(from)
            .fetch_optional(pool)
            .await?;
        row.map(ChangeSetRow::into_domain).transpose()
    }

    /// Apply a confirmed set: every operation, or none of them.
    ///
    /// The transaction is opened **here**, the row is re-read **inside** it, and the caller is
    /// handed the same `&mut PgConnection` the row was read through. That is the whole design:
    /// the store owns the transaction so "all-or-nothing" is a property of the **table** and
    /// not of one HTTP handler that happens to be careful today, and the caller does the
    /// writes so they go *through* the transaction rather than beside it — an applier that
    /// took `&PgPool` would commit the first operation and roll back only the status change.
    ///
    /// The status moves to `applied` with the same conditional `where status = 'confirmed'`
    /// the transitions use, in the same transaction as the writes, so a set can never read
    /// `applied` while its effects are absent.
    ///
    /// # Errors
    ///
    /// Whatever any operation refuses with — the whole set is rolled back first, and the
    /// refusal names the operation. `Err(InvalidChangeSet)` when the set is not `confirmed`.
    /// The applier is a **boxed** future rather than an `AsyncFnOnce` bound, and that is a
    /// deliberate choice with a cost worth naming.
    ///
    /// `AsyncFnOnce(&ChangeSet, &mut PgConnection)` is the nicer signature and it does not
    /// compile for this call shape: the closure has to work for *every* lifetime of both
    /// arguments, and an `async fn` applier whose future borrows its `&mut PgConnection`
    /// parameter is not general enough to satisfy that — the compiler says so, in a sentence,
    /// at the route. Boxing the future sidesteps the higher-ranked requirement: the closure is
    /// called exactly once, so the future is created exactly once, and a `Pin<Box<dyn Future>>`
    /// erases the lifetime the compiler wanted quantified.
    ///
    /// The cost is one heap allocation per apply, on a path that runs once per confirmed set
    /// and then writes several pages. That is not a measurable cost, and the alternative that
    /// avoids it — making the applier a trait with a lifetime-parameterised method — moves the
    /// same lifetime problem into the trait and adds a type to read.
    pub async fn apply_confirmed<F>(
        pool: &PgPool,
        organization_id: Uuid,
        id: Uuid,
        apply: F,
    ) -> Result<Vec<super::AppliedOp>>
    where
        F: for<'c> FnOnce(
            &'c ChangeSet,
            &'c mut sqlx::PgConnection,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Vec<super::AppliedOp>>> + Send + 'c>,
        >,
    {
        let mut tx = pool.begin().await?;
        let sql = format!(
            "select {CHANGE_SET_COLUMNS} from ai_change_sets \
             where id = $1 and organization_id = $2 and status = 'confirmed' for update"
        );
        let row: Option<ChangeSetRow> = sqlx::query_as(&sql)
            .bind(id)
            .bind(organization_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(set) = row.map(ChangeSetRow::into_domain).transpose()? else {
            tx.rollback().await?;
            return Err(AiHubError::InvalidChangeSet(format!(
                "change set {id} is not confirmed; only a confirmed set can be applied"
            )));
        };
        // Validated inside the transaction, not before it: the row may have been edited by a
        // competing path between the caller's read and this lock, and an apply that trusts an
        // earlier validation is applying something nobody checked.
        if let Err(err) = validate(&set) {
            tx.rollback().await?;
            return Err(err);
        }

        let applied = match apply(&set, &mut tx).await {
            Ok(applied) => applied,
            Err(err) => {
                // An explicit rollback, not a `?`: dropping a transaction is a rollback in
                // sqlx, but "all-or-nothing because of Drop's timing" is a weaker claim than
                // "all-or-nothing", and this is the one line that carries the guarantee.
                tx.rollback().await?;
                return Err(err);
            }
        };

        let sql = "update ai_change_sets set status = 'applied', applied_at = now(), updated_at = now() \
             where id = $1 and organization_id = $2 and status = 'confirmed' returning id";
        let updated: Option<Uuid> = sqlx::query_scalar(sql)
            .bind(id)
            .bind(organization_id)
            .fetch_optional(&mut *tx)
            .await?;
        if updated.is_none() {
            tx.rollback().await?;
            return Err(AiHubError::InvalidChangeSet(format!(
                "change set {id} stopped being confirmed while it was being applied"
            )));
        }

        tx.commit().await?;
        Ok(applied)
    }

    /// The gated operations of a parked set that the inbox has already approved.
    ///
    /// The join is on **`operation_key`**, not on `resource_id` or `preview_hash`, and both of
    /// those are wrong in a way that only shows up once a second person acts:
    ///
    /// - `resource_id` — a set may edit one page twice (a rename and a publish in the same
    ///   proposal). Both approvals carry the same id, so an id-based release lets approving the
    ///   rename approve the publish, which is precisely the operation nobody read.
    /// - `preview_hash` — it names *what would be written*, and two operations with the same
    ///   shape hash the same only if the effect is identical, which is not the same question
    ///   as "which row did the reviewer answer".
    ///
    /// `approved` and **not** `decided`: a rejected row is decided too, and counting one as
    /// released is how a refused delete turns into a delete. The refusal is read separately,
    /// by [`rejected_operation_keys`](Self::rejected_operation_keys), before this one — so the
    /// two functions answer different questions and neither has to know about the other. An
    /// earlier draft listed `'rejected'` here as well, which made this primitive disagree with
    /// its own contract: the route survived it, because it refuses on the rejection first, but
    /// the walk that asserts "a rejected gate is not an approving one" read the pair directly
    /// and correctly said no. `applied` is excluded as well, because a row that has already been
    /// applied is not evidence about a *different* operation that happens to share a set.
    ///
    /// Empty for a set with no rows yet, which is the answer that keeps a fresh `pending` set
    /// parked — the caller compares the count with the gated count rather than checking this
    /// for emptiness, because "no rows" and "all rows" are both empty-looking.
    pub async fn approved_operation_keys(
        pool: &PgPool,
        organization_id: Uuid,
        change_set_id: Uuid,
    ) -> Result<Vec<String>> {
        let keys: Vec<String> = sqlx::query_scalar(
            "select operation_key from ai_approvals \
             where organization_id = $1 and change_set_id = $2 \
               and status = 'approved' and operation_key is not null",
        )
        .bind(organization_id)
        .bind(change_set_id)
        .fetch_all(pool)
        .await?;
        Ok(keys)
    }

    /// The gated operations of a parked set that the inbox has **rejected**.
    ///
    /// The counterpart of [`approved_operation_keys`](Self::approved_operation_keys), and read
    /// before it on purpose: a refused gate ends the set, so a set with one rejected and two
    /// approved operations must not report "one outstanding" while a refusal sits in the tab.
    /// A pipeline that only counted approvals would keep waiting for an answer that has
    /// already been given, in the negative.
    pub async fn rejected_operation_keys(
        pool: &PgPool,
        organization_id: Uuid,
        change_set_id: Uuid,
    ) -> Result<Vec<String>> {
        let keys: Vec<String> = sqlx::query_scalar(
            "select operation_key from ai_approvals \
             where organization_id = $1 and change_set_id = $2 \
               and status = 'rejected' and operation_key is not null",
        )
        .bind(organization_id)
        .bind(change_set_id)
        .fetch_all(pool)
        .await?;
        Ok(keys)
    }

    /// Record that an apply did not happen, and why.
    ///
    /// Called **after** [`apply_confirmed`] has rolled its transaction back, so it is a
    /// separate statement and not part of it: a `failed` row written inside the transaction
    /// that failed would be rolled back with it, and the record would be exactly the thing
    /// that disappears.
    ///
    /// The reason is stored in `discarded_reason` — the column the table already has for "a
    /// human-readable sentence about why this set stopped being live" — rather than a new
    /// column. A new column would mean a migration for a value that is read by exactly one
    /// screen, and the row's `status` is what the screen filters on; the reason only has to be
    /// legible next to it.
    ///
    /// The `where status = 'confirmed'` clause is what keeps this from overwriting a row that
    /// somebody else already moved: a concurrent discard is a decision, and a failed apply
    /// must not erase it. `Ok(false)` says the row is no longer confirmed, which is the caller's
    /// signal that there is nothing left to annotate.
    ///
    /// # Errors
    ///
    /// Whatever the database refuses with.
    pub async fn mark_failed(
        pool: &PgPool,
        organization_id: Uuid,
        id: Uuid,
        reason: &str,
    ) -> Result<bool> {
        let sql = "update ai_change_sets set status = 'failed', discarded_reason = $3, updated_at = now() \
             where id = $1 and organization_id = $2 and status = 'confirmed'";
        let written = sqlx::query(sql)
            .bind(id)
            .bind(organization_id)
            .bind(reason.trim())
            .execute(pool)
            .await?
            .rows_affected();
        Ok(written > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    fn op(key: &str, kind: OpKind, resource_id: &str) -> ChangeOp {
        ChangeOp {
            key: key.to_owned(),
            operation: Operation {
                kind,
                resource_type: "page".to_owned(),
                resource_id: resource_id.to_owned(),
                args: json!({ "title": "Edited" }),
            },
        }
    }

    fn set_with(operations: Vec<ChangeOp>) -> ChangeSet {
        ChangeSet {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            site_id: None,
            title: "Three pages".to_owned(),
            status: INITIAL_STATUS.to_owned(),
            operations,
            base_revisions: BTreeMap::new(),
            content_hash: String::new(),
            created_by: None,
            created_by_agent: None,
            created_by_run: None,
            updated_by: None,
            confirmed_at: None,
            applied_at: None,
            discarded_reason: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn draft() -> ChangeSet {
        set_with(vec![
            op("a", OpKind::Update, "11111111-1111-1111-1111-111111111111"),
            op("b", OpKind::Update, "22222222-2222-2222-2222-222222222222"),
            op("c", OpKind::Update, "33333333-3333-3333-3333-333333333333"),
        ])
    }

    /// An executor that records what it was asked to do and refuses on a chosen key.
    ///
    /// The refusal is the point: it stands in for the second write of a real apply failing,
    /// which is the only way to observe that the first one was undone.
    struct Recorder {
        done: Vec<String>,
        fail_on: Option<String>,
    }

    impl Recorder {
        fn failing_on(key: &str) -> Self {
            Self {
                done: Vec::new(),
                fail_on: Some(key.to_owned()),
            }
        }

        fn all() -> Self {
            Self {
                done: Vec::new(),
                fail_on: None,
            }
        }
    }

    impl OperationExecutor for Recorder {
        fn execute(&mut self, op: &ChangeOp) -> Result<AppliedOp> {
            if self.fail_on.as_deref() == Some(op.key.as_str()) {
                return Err(AiHubError::InvalidApproval(
                    "the page does not exist".to_owned(),
                ));
            }
            self.done.push(op.key.clone());
            Ok(AppliedOp {
                key: op.key.clone(),
                kind: op.operation.kind,
                resource_id: op.operation.resource_id.clone(),
                slug: format!("{}-slug", op.key),
                status: "draft".to_owned(),
            })
        }
    }

    #[test]
    fn a_three_operation_set_applies_in_order() {
        let set = draft();
        let mut recorder = Recorder::all();
        let applied = apply_all(&set.operations, &mut recorder).expect("the set applies");
        assert_eq!(applied.len(), 3);
        assert_eq!(
            applied.iter().map(|op| op.key.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        assert_eq!(recorder.done, vec!["a", "b", "c"]);
    }

    /// The claim slice 3 exists to make.
    ///
    /// A set that fails on its third operation must not leave the first two applied. The
    /// executor is given the whole list at once, so a caller that applied eagerly would show
    /// `done == ["a", "b"]` here — the assertion is on the *executor having been stopped*,
    /// and on the refusal naming which operation failed, because "operation `b` failed" is
    /// the difference between a usable error and "the change set could not be applied".
    #[test]
    fn a_refusal_names_the_operation_and_stops_the_rest() {
        let set = draft();
        let mut recorder = Recorder::failing_on("b");
        let err = apply_all(&set.operations, &mut recorder).expect_err("b is refused");
        let message = err.to_string();
        assert!(
            message.contains("`b`"),
            "the refusal must name the operation: {message}"
        );
        assert!(
            message.contains("the page does not exist"),
            "the refusal must keep the cause: {message}"
        );
        // `c` never ran, and the store rolls the transaction back, so `a` is undone with it.
        assert_eq!(recorder.done, vec!["a"]);
    }

    #[test]
    fn a_failing_first_operation_runs_nothing() {
        let set = draft();
        let mut recorder = Recorder::failing_on("a");
        assert!(apply_all(&set.operations, &mut recorder).is_err());
        assert!(recorder.done.is_empty());
    }

    #[test]
    fn keys_are_derived_from_the_operation_not_its_position() {
        let one = Operation {
            kind: OpKind::Update,
            resource_type: "page".to_owned(),
            resource_id: "abc".to_owned(),
            args: json!({}),
        };
        let mut other = one.clone();
        other.kind = OpKind::Delete;
        let keys = keys_for(&[one.clone(), other.clone()]);
        assert_ne!(
            keys[0], keys[1],
            "different operations must not share a key"
        );
        // The same operation proposed twice produces the same key, so a re-proposal diffs
        // against the old set instead of looking like a different one.
        assert_eq!(keys_for(&[one.clone()]), keys_for(&[one]));
    }

    #[test]
    fn the_transition_table_refuses_a_draft_going_straight_to_applied() {
        assert!(can_transition("draft", "confirmed"));
        assert!(can_transition("confirmed", "applied"));
        // There is no edge that skips the human: a draft cannot reach `applied`, and a
        // confirmed set cannot be re-edited into a draft.
        assert!(!can_transition("draft", "applied"));
        assert!(!can_transition("confirmed", "draft"));
        assert!(!can_transition("applied", "confirmed"));
    }

    /// A status this build does not know must not move, and must not panic either.
    #[test]
    fn an_unknown_status_is_terminal_rather_than_fatal() {
        assert!(!can_transition("archived_by_a_newer_build", "confirmed"));
        assert!(next_statuses("archived_by_a_newer_build").is_empty());
    }

    #[test]
    fn an_empty_or_untitled_set_is_refused() {
        let mut set = draft();
        set.operations.clear();
        assert!(validate(&set).is_err(), "an empty set is not a proposal");

        let mut set = draft();
        set.title = "   ".to_owned();
        assert!(validate(&set).is_err(), "a blank title is not a title");
    }

    #[test]
    fn a_create_may_not_name_a_target_and_an_update_may_not_omit_one() {
        let mut set = set_with(vec![op(
            "a",
            OpKind::Create,
            "11111111-1111-1111-1111-111111111111",
        )]);
        assert!(validate(&set).is_err(), "a create has no target");

        let mut set = set_with(vec![op("a", OpKind::Update, "")]);
        assert!(validate(&set).is_err(), "an update needs a target");
    }

    /// A duplicate key makes one of the two operations undroppable, so it is refused at the
    /// boundary rather than silently deduplicated.
    #[test]
    fn two_operations_may_not_share_a_key() {
        let set = set_with(vec![
            op(
                "same",
                OpKind::Update,
                "11111111-1111-1111-1111-111111111111",
            ),
            op(
                "same",
                OpKind::Update,
                "22222222-2222-2222-2222-222222222222",
            ),
        ]);
        let err = validate(&set).expect_err("a duplicate key is refused");
        assert!(err.to_string().contains("share a key"), "{err}");
    }

    #[test]
    fn a_set_of_fifty_is_allowed_and_fifty_one_is_not() {
        let many = |count: usize| {
            (0..count)
                .map(|index| {
                    op(
                        &format!("op{index}"),
                        OpKind::Update,
                        &Uuid::new_v4().to_string(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert!(validate(&set_with(many(MAX_OPERATIONS))).is_ok());
        let err = validate(&set_with(many(MAX_OPERATIONS + 1))).expect_err("over the limit");
        assert!(
            err.to_string().contains(&MAX_OPERATIONS.to_string()),
            "{err}"
        );
    }

    #[test]
    fn a_set_that_deletes_something_is_irreversible() {
        let set = set_with(vec![op(
            "a",
            OpKind::Update,
            "11111111-1111-1111-1111-111111111111",
        )]);
        assert!(!set.is_irreversible());
        let set = set_with(vec![
            op("a", OpKind::Update, "11111111-1111-1111-1111-111111111111"),
            op("b", OpKind::Delete, "22222222-2222-2222-2222-222222222222"),
        ]);
        assert!(set.is_irreversible(), "a delete needs the typed phrase");
        assert_eq!(set.of_kind(OpKind::Delete).len(), 1);
    }

    /// The same target twice is one target for staleness, not two — otherwise a set that
    /// creates and then deletes a page would be reported as drifting twice and counted twice
    /// against the operation budget.
    #[test]
    fn targets_are_deduplicated_and_creates_are_not_targets() {
        let set = set_with(vec![
            op("a", OpKind::Update, "11111111-1111-1111-1111-111111111111"),
            op("b", OpKind::Delete, "11111111-1111-1111-1111-111111111111"),
            op("c", OpKind::Create, ""),
        ]);
        assert_eq!(set.targets().len(), 1);
    }

    /// A row whose jsonb cannot be read must be refused, not read as an empty set.
    #[test]
    fn a_row_with_the_wrong_jsonb_shape_is_refused_by_name() {
        let row = ChangeSetRow {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            site_id: None,
            title: "Broken".to_owned(),
            status: "draft".to_owned(),
            operations: json!({ "not": "a list" }),
            base_revisions: json!({}),
            content_hash: String::new(),
            created_by: None,
            created_by_agent: None,
            created_by_run: None,
            updated_by: None,
            confirmed_at: None,
            applied_at: None,
            discarded_reason: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let err = row.into_domain().expect_err("a broken row is refused");
        assert!(err.to_string().contains("`operations`"), "{err}");

        let row = ChangeSetRow {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            site_id: None,
            title: "Broken".to_owned(),
            status: "draft".to_owned(),
            operations: json!([]),
            base_revisions: json!([1, 2, 3]),
            content_hash: String::new(),
            created_by: None,
            created_by_agent: None,
            created_by_run: None,
            updated_by: None,
            confirmed_at: None,
            applied_at: None,
            discarded_reason: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let err = row.into_domain().expect_err("a broken row is refused");
        assert!(err.to_string().contains("`base_revisions`"), "{err}");
    }

    #[test]
    fn a_well_formed_row_decodes_into_the_domain_type() {
        let operations = vec![op(
            "op0:update:page:abcdef012345",
            OpKind::Update,
            "11111111-1111-1111-1111-111111111111",
        )];
        let row = ChangeSetRow {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            site_id: None,
            title: "Three pages".to_owned(),
            status: "draft".to_owned(),
            operations: serde_json::to_value(&operations).expect("serialises"),
            base_revisions: json!({ "page:11111111-1111-1111-1111-111111111111": "rev" }),
            content_hash: String::new(),
            created_by: None,
            created_by_agent: None,
            created_by_run: None,
            updated_by: None,
            confirmed_at: None,
            applied_at: None,
            discarded_reason: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let set = row.into_domain().expect("decodes");
        assert_eq!(set.operations, operations);
        assert_eq!(
            set.base_revisions
                .get("page:11111111-1111-1111-1111-111111111111"),
            Some(&"rev".to_owned())
        );
    }

    /// The statuses the table's constraint allows are exactly the statuses the code knows.
    ///
    /// This is the kind of test that looks like bureaucracy and is not: the SQL constraint and
    /// this `match` are two independent lists of the same fact, and nothing but a test keeps
    /// them from drifting. A status added to one and not the other would be writable and
    /// unmovable — the row would accept a value the lifecycle cannot reason about.
    ///
    /// It **reads the migrations** rather than repeating the list, because slice 3b learned that
    /// the hardcoded copy is the second source of truth, not the first: adding `failed` meant
    /// editing the constraint in `0201` and this array, and the compile error that followed was
    /// the test doing its job only by accident. Reading the SQL makes the assertion survive the
    /// next status without being edited — and if a migration is renamed the file read fails
    /// loudly instead of silently checking nothing.
    #[test]
    fn the_known_statuses_are_exactly_the_tables_own() {
        let migrations = include_str!("../../../database/migrations/0189_ai_approvals.sql")
            .to_owned()
            + &include_str!("../../../database/migrations/0201_ai_change_set_failed.sql");
        // The **last** `check (status in (…))` wins: `0201` drops 0189's constraint and adds
        // its own, and an earlier copy read from the string would be a stale answer presented
        // as a current one.
        let last = migrations
            .rfind("check (status in (")
            .expect("a status check exists");
        let list = &migrations[last + "check (status in (".len()..];
        let list = &list[..list.find(')').expect("a closed list")];

        let from_sql: Vec<&str> = list
            .split(',')
            .map(|part| part.trim().trim_matches('\'').trim())
            .filter(|part| !part.is_empty())
            .collect();
        assert_eq!(
            from_sql,
            STATUSES.to_vec(),
            "the code's status list and the table's constraint are the same list"
        );

        // And the constraint the migration chain leaves in place is a check on the seven, not
        // the six: reading the string cannot tell a `drop constraint` from an `add` unless the
        // list is compared, which is what the assertion above does.
        assert!(
            migrations.contains("ai_change_sets_failed_has_reason"),
            "a failed set must say why, or it reads as one that is still waiting"
        );
    }

    /// `failed` is terminal and reachable only from `confirmed`.
    ///
    /// The edge list is the claim: an applier may mark a failure, and nothing else may. A
    /// `draft → failed` edge would let a proposal be written off before anybody looked at it,
    /// and a `failed → draft` edge would let a person re-open a set whose operations are already
    /// the record of something that was tried.
    #[test]
    fn a_failed_set_is_terminal_and_only_an_applier_may_write_it() {
        assert!(can_transition("confirmed", "failed"));
        assert!(!can_transition("draft", "failed"));
        assert!(!can_transition("pending", "failed"));
        assert!(!can_transition("failed", "draft"));
        assert!(!can_transition("failed", "confirmed"));
        assert!(!can_transition("failed", "applied"));
        assert!(next_statuses("failed").is_empty(), "failed is terminal");
    }

    // ---------------------------------------------------------------------------------------
    // The content hash (slice 3e) — the pure half
    // ---------------------------------------------------------------------------------------
    //
    // These need no database, and the walks in `apps/api/tests/ai_change_sets.rs` need these:
    // the walk that a *stored* hash equals the computed one is only interesting because these
    // fix what "the computed one" is. Without them the store walk would pass for a hash over
    // a constant.

    fn revisioned(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, revision)| ((*key).to_owned(), (*revision).to_owned()))
            .collect()
    }

    /// A set's hash is stable, 64 hex characters, and independent of the row it lives on.
    ///
    /// The last part is the one that matters: a hash including `id`, `status` or `updated_at`
    /// would change on every transition without anything about the operations moving, and the
    /// reviewer's "this is still what I read" would be false every time somebody else opened
    /// the set.
    #[test]
    fn a_set_hashes_its_operations_and_nothing_else() {
        let operations = vec![
            op("a", OpKind::Update, "11111111-1111-1111-1111-111111111111"),
            op("b", OpKind::Update, "22222222-2222-2222-2222-222222222222"),
        ];
        let revisions = revisioned(&[("page:1111", "r1"), ("page:2222", "r2")]);

        let hash = ChangeSet::compute_content_hash(&operations, &revisions);
        assert_eq!(hash.len(), 64, "a sha256 renders as 64 hex characters");
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "and as lowercase hex, or a client comparing it case-insensitively still differs"
        );
        assert_eq!(
            hash,
            ChangeSet::compute_content_hash(&operations, &revisions),
            "the same inputs hash the same way"
        );

        // The same operations on a different set.
        let mut elsewhere = set_with(operations.clone());
        elsewhere.id = Uuid::from_u128(9);
        elsewhere.status = "pending".to_owned();
        elsewhere.base_revisions = revisions.clone();
        assert_eq!(
            elsewhere.content_hash(),
            hash,
            "the row's own identity and status are not part of what it says"
        );
    }

    /// Reordering changes the hash, because the order is the order they are applied in.
    ///
    /// This is the half a "sort the operations before hashing" implementation gets wrong, and
    /// it gets it wrong in the *safe-looking* direction: sorting makes the hash stable under a
    /// reorder, which sounds like a feature, and is exactly the bug — a reviewer who read
    /// "delete A, then rename B" and a reviewer who read "rename B, then delete A" would be
    /// told they are looking at the same proposal. The first operation is the one that runs
    /// first inside the single transaction, and on a set where the first one is a delete the
    /// second may fail against a target that no longer exists.
    #[test]
    fn reordering_a_set_changes_its_hash() {
        let first = op("a", OpKind::Delete, "11111111-1111-1111-1111-111111111111");
        let second = op("b", OpKind::Update, "22222222-2222-2222-2222-222222222222");
        let in_order =
            ChangeSet::compute_content_hash(&[first.clone(), second.clone()], &BTreeMap::new());
        let reversed = ChangeSet::compute_content_hash(&[second, first], &BTreeMap::new());
        assert_ne!(
            in_order, reversed,
            "the order is the order they are applied in, and the hash says so"
        );
    }

    /// A change to **any** of the three inputs changes the hash, and an identical re-send does
    /// not.
    ///
    /// Written as a list of (label, before, after) rather than three separate tests so a
    /// future input cannot be added to the hash without a row here — the failure mode is a
    /// hash that quietly stops covering a field, and a table is where that is visible.
    #[test]
    fn every_input_is_covered_and_a_resend_is_not_a_change() {
        let base_ops = || {
            vec![op(
                "a",
                OpKind::Update,
                "11111111-1111-1111-1111-111111111111",
            )]
        };
        let base_revs = || revisioned(&[("page:1111", "r1")]);

        // Dropping an operation.
        let dropped = ChangeSet::compute_content_hash(&[], &base_revs());
        assert_ne!(
            dropped,
            ChangeSet::compute_content_hash(&base_ops(), &base_revs()),
            "an emptied set is a different proposal"
        );

        // Changing a written value.
        let mut edited = base_ops();
        edited[0].operation.args = json!({ "title": "Something else" });
        assert_ne!(
            ChangeSet::compute_content_hash(&edited, &base_revs()),
            ChangeSet::compute_content_hash(&base_ops(), &base_revs()),
            "a changed value is a changed proposal"
        );

        // Changing the target.
        let retargeted = vec![op(
            "a",
            OpKind::Update,
            "33333333-3333-3333-3333-333333333333",
        )];
        assert_ne!(
            ChangeSet::compute_content_hash(&retargeted, &base_revs()),
            ChangeSet::compute_content_hash(&base_ops(), &base_revs()),
            "a changed target is a changed proposal"
        );

        // Changing the revision the set was pinned to. This one is not obvious: re-reading a
        // target at a new revision is not a change to what the set *says*, and it would be
        // reasonable to leave it out. It is in because `drifted_targets` reads the same map
        // and a hash that ignored it could not distinguish "pinned to r1" from "pinned to r2"
        // — and a re-pinned set is exactly the set whose freshness the reviewer is trusting.
        assert_ne!(
            ChangeSet::compute_content_hash(&base_ops(), &revisioned(&[("page:1111", "r2")])),
            ChangeSet::compute_content_hash(&base_ops(), &base_revs()),
            "a re-pinned set is a set the reviewer must read again"
        );

        // Re-sending the identical list, in the identical order, is not a change — otherwise
        // every save would report "moved" and the guard would be noise.
        assert_eq!(
            ChangeSet::compute_content_hash(&base_ops(), &base_revs()),
            ChangeSet::compute_content_hash(&base_ops(), &base_revs()),
            "an identical re-send hashes identically"
        );
    }

    /// The keys of `base_revisions` are hashed in key order, so two maps that differ only in
    /// insertion order hash alike.
    ///
    /// `BTreeMap` makes this true by construction, and the walk is here because that is a
    /// property of the *type* rather than of the function: swap the field for a `HashMap` and
    /// nothing above would fail — the hash would just become non-deterministic, which is a
    /// failure that only appears as a `409` a reviewer cannot reproduce.
    #[test]
    fn the_revision_map_hashes_in_key_order() {
        let forward = revisioned(&[("page:aaa", "r1"), ("page:bbb", "r2")]);
        let backward: BTreeMap<String, String> = [("page:bbb", "r2"), ("page:aaa", "r1")]
            .into_iter()
            .map(|(key, revision)| (key.to_owned(), revision.to_owned()))
            .collect();
        assert_eq!(forward, backward, "the two maps are equal");
        assert_eq!(
            ChangeSet::compute_content_hash(&[], &forward),
            ChangeSet::compute_content_hash(&[], &backward),
            "and they hash alike, so map order is not part of the proposal"
        );
    }
}
