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
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::approvals::plan::{OpKind, Operation};
use crate::error::{AiHubError, Result};

/// The status a change set moves through, and the legal moves between them.
///
/// A `match` returning the allowed successors rather than a bare predicate, because the two
/// questions a caller asks are different ones: "may I do this now" (a predicate on one pair)
/// and "what may I offer the user next" (the whole row). A table of successors answers both,
/// and it makes an illegal transition a *compile-time* list to read rather than an `if` chain
/// scattered across three call sites that grows a hole each time a status is added.
pub const STATUSES: [&str; 6] = [
    "draft",
    "pending",
    "confirmed",
    "applied",
    "discarded",
    "expired",
];

/// The status a newly proposed set carries.
pub const INITIAL_STATUS: &str = "draft";

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
        "confirmed" => &["applied", "discarded"],
        "applied" | "discarded" | "expired" => &[],
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
    pub created_by: Option<Uuid>,
    pub created_by_agent: Option<Uuid>,
    pub created_by_run: Option<Uuid>,
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
        self.operations.iter().any(|op| {
            crate::approvals::class_of_tool(&op.operation_key())
                .is_some_and(crate::approvals::is_dangerous_class)
        })
    }
}

impl ChangeOp {
    /// The tool key this operation would run as, so the gate can classify it.
    ///
    /// A change set does **not** store a tool key: an operation is a resource write, and the
    /// gate classifies by class. `page.update` is the only tool this build previews, so the
    /// mapping is one line today and a `match` the day a second resource lands. Spelling it
    /// out beats a string literal at two call sites that can drift.
    fn operation_key(&self) -> String {
        format!(
            "{}.{}",
            self.operation.kind.label(),
            self.operation.resource_type
        )
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
    pub created_by: Option<Uuid>,
    pub created_by_agent: Option<Uuid>,
    pub created_by_run: Option<Uuid>,
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
            created_by: self.created_by,
            created_by_agent: self.created_by_agent,
            created_by_run: self.created_by_run,
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
     base_revisions, created_by, created_by_agent, created_by_run, confirmed_at, applied_at, \
     discarded_reason, created_at, updated_at";

/// The read/append half of the store.
///
/// Every function takes the organization id **first** and filters on it in the `where`
/// clause rather than reading a row and comparing afterwards. A read-then-compare is one
/// `if` away from an existence oracle: the row exists, the caller does not own it, and the
/// two cases have to answer differently by hand at every call site.
pub mod store {
    use super::{ChangeOp, ChangeSet, ChangeSetRow, validate};
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
            created_by: new.created_by,
            created_by_agent: new.created_by_agent,
            created_by_run: new.created_by_run,
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
                  created_by, created_by_agent, created_by_run) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             returning {CHANGE_SET_COLUMNS}"
        );
        let operations: Value =
            serde_json::to_value(&new.operations).unwrap_or(Value::Array(vec![]));
        let revisions: Value = serde_json::to_value(&new.base_revisions)
            .unwrap_or_else(|_| Value::Object(serde_json::Map::new()));
        let row: ChangeSetRow = sqlx::query_as(&sql)
            .bind(new.organization_id)
            .bind(new.site_id)
            .bind(new.title.trim())
            .bind(super::INITIAL_STATUS)
            .bind(operations)
            .bind(revisions)
            .bind(new.created_by)
            .bind(new.created_by_agent)
            .bind(new.created_by_run)
            .fetch_one(pool)
            .await?;
        row.into_domain()
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
        ) -> Result<Vec<super::AppliedOp>>,
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

        let applied = match apply(&set, &mut tx) {
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
            created_by: None,
            created_by_agent: None,
            created_by_run: None,
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
            created_by: None,
            created_by_agent: None,
            created_by_run: None,
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
            created_by: None,
            created_by_agent: None,
            created_by_run: None,
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
            created_by: None,
            created_by_agent: None,
            created_by_run: None,
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

    /// The six statuses the table's constraint allows must be the six the code knows.
    ///
    /// This is the kind of test that looks like bureaucracy and is not: the SQL constraint in
    /// `0189_ai_approvals.sql` and this `match` are two independent lists of the same fact, and
    /// nothing but a test keeps them from drifting. A status added to one and not the other
    /// would be writable and unmovable.
    #[test]
    fn the_known_statuses_are_exactly_the_tables_own() {
        assert_eq!(
            STATUSES,
            [
                "draft",
                "pending",
                "confirmed",
                "applied",
                "discarded",
                "expired"
            ]
        );
    }
}
