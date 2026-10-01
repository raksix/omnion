//! `/api/v1/ai/change-sets` — a conversation's proposed operations, edited and confirmed
//! (REQ-101, slice 3).
//!
//! # What this screen is for
//!
//! The approval inbox answers "may this one tool call run?". A change set answers a different
//! question: a conversation may propose **several** operations, and a person edits the list —
//! drops one, reorders two, changes a value — before confirming it. The request is explicit
//! that the editor is part of the feature, not a convenience: "the user edits values, drops
//! operations, reorders them and confirms".
//!
//! # The three routes and what each one is allowed to decide
//!
//! - `POST /ai/change-sets` — append a proposal. Gated on `ai.approvals.read`, because a
//!   proposal is a *description* of work, and the person who files it is the one who will
//!   read it. It parks in the inbox like any other request.
//! - `PATCH /ai/change-sets/{id}` — the edit. `ai.approvals.read` again, for the same reason:
//!   editing a draft proposes a different set, it does not perform anything. A draft is a
//!   document; the permission that matters is the one on the *confirmation*.
//! - `POST /ai/change-sets/{id}/confirm` — the gate. `ai.approvals.act`, and a typed
//!   confirmation phrase when the set is irreversible. This is the only route here that can
//!   cause a write, so it is the only one that carries `act`.
//! - `POST /ai/change-sets/{id}/discard` — `ai.approvals.act`, with a reason, because a
//!   discarded set is a decision somebody took and the record has to say so.
//!
//! The split is the point: **filing and editing a proposal is not authority over it.** An
//! installation that lets every reader file change sets and only lets approvers confirm them
//! has a gate; one that gates the whole lifecycle behind `act` has a form nobody without the
//! permission can use to *ask* for something.
//!
//! # Why the apply is not a route parameter
//!
//! Confirming a set does **not** apply it here. Confirmation parks the set; the apply runs
//! through the same all-or-nothing transaction the store owns (see
//! [`omnion_ai_hub::change_sets`]), and a set containing a gated operation lands in the
//! approval inbox exactly as a single gated call does. A `?apply=true` on this route would
//! have been the tempting shortcut, and it would have put a five-write transaction behind one
//! query parameter.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};

use std::collections::BTreeSet;

use omnion_ai_hub::change_sets::store::NewChangeSet;
use omnion_ai_hub::change_sets::{self, AppliedOp, ChangeOp, ChangeSet, ChangeSetRow, Operation};
use omnion_events::NewEvent;
use omnion_events::bus;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::routes::ai_agents::OrgQuery;
use crate::scope::resolve_organization;
use crate::state::AppState;

/// The maximum a change set may carry, re-exported so the route's error message and the
/// store's are the same sentence about the same number.
pub const MAX_OPERATIONS: usize = change_sets::MAX_OPERATIONS;

/// `POST /ai/change-sets` — file a proposal.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateBody {
    pub title: String,
    /// The operations, in the order they would apply.
    pub operations: Vec<ProposedOp>,
    pub site_id: Option<uuid::Uuid>,
    /// The run this proposal came out of, when it came from a conversation.
    pub run_id: Option<uuid::Uuid>,
    pub agent_id: Option<uuid::Uuid>,
}

/// One operation as a client sends it.
///
/// Flattens onto [`Operation`] with a serde-level default for the key, so a caller that does
/// not care about keys (the common case: a model emitted a list) can omit them and get the
/// content-derived ones [`change_sets::keys_for`] produces. A caller that *does* edit a list
/// needs stable keys, and that is the same field either way — one shape, not two.
#[derive(Debug, Clone, Deserialize)]
pub struct ProposedOp {
    #[serde(default)]
    pub key: Option<String>,
    #[serde(flatten)]
    pub operation: Operation,
}

impl ProposedOp {
    /// The operation with a key, deriving one when the client sent none.
    fn into_change_op(self, derived: &str) -> ChangeOp {
        ChangeOp {
            key: self
                .key
                .map(|key| key.trim().to_owned())
                .filter(|key| !key.is_empty())
                .unwrap_or_else(|| derived.to_owned()),
            operation: self.operation,
        }
    }
}

/// What a created set answers with.
#[derive(Debug, Clone, Serialize)]
pub struct SetCreated {
    #[serde(flatten)]
    pub set: ChangeSet,
    /// The keys the store ended up with, echoed so the client can address an operation
    /// without re-deriving them.
    pub keys: Vec<String>,
    /// `true` when confirming this set would park at least one operation for a human. The
    /// editor shows this *before* the reviewer confirms, because a set that needs a second
    /// person should not be discoverable only at the confirmation step.
    pub needs_approval: bool,
}

/// `POST /ai/change-sets` — file a proposed set of operations.
///
/// The operations are validated as a **whole** before the insert, so a proposal with a
/// duplicate key or a create that names a target never reaches a row: a caller can fix the
/// body it just sent rather than reconcile a half-created record.
pub async fn create(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Json(body): Json<CreateBody>,
) -> Result<Json<SetCreated>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let actor = current.user.id;

    if body.operations.is_empty() {
        return Err(ApiError::bad_request(
            "operations",
            "a change set needs at least one operation",
        ));
    }
    if body.operations.len() > MAX_OPERATIONS {
        return Err(ApiError::bad_request(
            "operations",
            format!(
                "a change set carries {} operations; the limit is {MAX_OPERATIONS}",
                body.operations.len()
            ),
        ));
    }

    // The derived keys are computed from the operations **before** the client's own keys are
    // applied, so a client that supplies no key at all gets the same key the same list would
    // get on the next run — which is what makes a re-proposal diffable.
    let derived = change_sets::keys_for(
        &body
            .operations
            .iter()
            .map(|op| op.operation.clone())
            .collect::<Vec<_>>(),
    );
    let operations = body
        .operations
        .into_iter()
        .zip(derived.iter())
        .map(|(op, key)| op.into_change_op(key))
        .collect::<Vec<_>>();

    let base_revisions =
        change_sets::store::current_revisions(state.db().pool(), organization, &operations)
            .await
            .map_err(ApiError::from)?;

    let set = change_sets::store::append(
        state.db().pool(),
        &NewChangeSet {
            organization_id: organization,
            site_id: body.site_id,
            title: body.title,
            operations,
            created_by: Some(actor),
            created_by_agent: body.agent_id,
            created_by_run: body.run_id,
            base_revisions,
        },
    )
    .await
    .map_err(ApiError::from)?;

    bus::emit(
        state.db().pool(),
        NewEvent::new("ai.changeset.proposed")
            .organization(organization)
            .actor(actor)
            .payload(serde_json::json!({
                "change_set_id": set.id,
                "title": set.title,
                "operations": set.operations.len(),
            })),
    )
    .await?;

    Ok(Json(SetCreated {
        needs_approval: set.has_gated_operations(),
        keys: set.operations.iter().map(|op| op.key.clone()).collect(),
        set,
    }))
}

/// `PATCH /ai/change-sets/{id}` — edit the draft.
///
/// The whole list is replaced, not patched in place: a set is a **list** the reviewer owns, and
/// a partial patch cannot express "move the third one to the top" or "drop the second one"
/// without a second vocabulary. The keys make the intent explicit — a client sends the list it
/// wants, and an operation that is gone from it is gone.
///
/// A row that is not a draft is refused. A confirmed set is a promise and an applied one is a
/// record; editing either would rewrite what was decided or what happened.
pub async fn update(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(id): Path<uuid::Uuid>,
    Json(body): Json<UpdateBody>,
) -> Result<Json<ChangeSet>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let actor = current.user.id;

    let existing = change_sets::store::read(state.db().pool(), organization, id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "change_set_not_found",
                format!("no change set `{id}` in this organization"),
            )
        })?;
    if existing.status != "draft" && existing.status != "pending" {
        return Err(ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "not_editable",
            format!(
                "this change set is `{}`; a set that has been confirmed, applied or discarded is a record",
                existing.status
            ),
        ));
    }
    if body.operations.is_empty() {
        return Err(ApiError::bad_request(
            "operations",
            "a change set needs at least one operation",
        ));
    }

    // The write is the **store's**, not this handler's (slice 3e). It used to be a hand-written
    // `update … returning` here, which is where the content hash had to be added — a second
    // statement that must agree with `append` about how a row is hashed, in a different file,
    // for no gain. The transaction and the hash are the same kind of guarantee: a property of
    // the table rather than of one HTTP handler.
    let stored = change_sets::store::replace_operations(
        state.db().pool(),
        organization,
        id,
        body.title.trim(),
        &body.operations,
        &body.base_revisions,
        actor,
        body.base_content_hash.as_deref(),
    )
    .await
    .map_err(ApiError::from)?;

    match stored {
        Ok(set) => Ok(Json(set)),
        Err(change_sets::store::EditRefusal::NotFound) => Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "change_set_not_found",
            format!("no change set `{id}` in this organization"),
        )),
        // Two different situations, two different messages, both `409`. "Somebody confirmed
        // it" and "somebody else saved an edit while you were typing" are the same HTTP
        // answer and nothing like the same sentence — a reviewer who is told the first one
        // goes looking for a decision that does not exist.
        Err(change_sets::store::EditRefusal::NotEditable { current }) => Err(ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "not_editable",
            format!("this change set is now `{current}` and can no longer be edited"),
        )),
        Err(change_sets::store::EditRefusal::ContentMoved { read, stored }) => Err(ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "content_moved",
            format!(
                "this set changed while you were editing it (you read `{}`, the server holds \
                     `{}`); reload it and re-apply your change",
                short(&read),
                short(&stored)
            ),
        )
        .with_details(serde_json::json!({
            "read_content_hash": read,
            "stored_content_hash": stored,
            "current_content_hash": existing.content_hash,
        }))),
    }
}

/// The first twelve characters of a hash, for a message a person reads.
///
/// A full sha256 in a `409` is a column of hex in a toast, and the part that identifies it is
/// the prefix; the whole value is in the error's `details` for a client that wants to compare
/// it programmatically.
fn short(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

/// What an edit carries.
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateBody {
    pub title: String,
    pub operations: Vec<ChangeOp>,
    /// The revisions the *edited* targets carry. Re-read server-side when absent is not
    /// possible from here, so a client that edits a target's values sends the revision it
    /// based them on; a set whose stored revision disagrees is refused at confirm time.
    #[serde(default)]
    pub base_revisions: std::collections::BTreeMap<String, String>,
    /// The `content_hash` the editor was looking at when it started (slice 3e).
    ///
    /// `#[serde(default)]` into an `Option` rather than a required field: a client that has
    /// not implemented the guard keeps saving, and one that has gets the optimistic check the
    /// request asks for. See [`change_sets::store::replace_operations`] for why an absent
    /// guard is a deliberate first step and not an oversight.
    #[serde(default)]
    pub base_content_hash: Option<String>,
}

/// `POST /ai/change-sets/{id}/confirm` — confirm a set, and park anything gated.
///
/// The typed phrase is demanded **here**, not by the panel: a set that deletes a page is
/// irreversible, and a client that forgot to ask the user must not be able to skip the asking
/// by talking to the API directly. The phrase is the set's own title, which is what the
/// reviewer sees on the button they are about to press.
///
/// # A gated set parks; an ungated one is applied
///
/// This is slice 3c, and it is the piece that makes "one pipeline, one screen" true. Before
/// it, `confirm` moved a set to `confirmed` and answered `needs_approval`, and nothing ever
/// acted on that flag: `apply` accepted any `confirmed` set, so a set full of deletes could
/// be confirmed and applied with **no human ever seeing it**. That is the whole point of
/// REQ-101 refused by the shape of a field, so the two arms are now structurally different:
///
/// - **Gated** — the set moves `draft → pending`, one approval is filed per gated operation,
///   and the *inbox* is where it is decided. A second person releases it.
/// - **Ungated** — the set is confirmed and applied right here, through the same all-or-nothing
///   store transaction every apply uses, and the answer carries what happened.
///
/// The alternative — always park, and let the inbox release a plain title edit — was refused:
/// a gate on everything is a gate nobody reads, and the request's own risk note names approval
/// fatigue as the failure mode.
pub async fn confirm(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(id): Path<uuid::Uuid>,
    Json(body): Json<ConfirmBody>,
) -> Result<Json<Confirmed>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let actor = current.user.id;

    let set = change_sets::store::read(state.db().pool(), organization, id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "change_set_not_found",
                format!("no change set `{id}` in this organization"),
            )
        })?;

    // The stale check runs **before** the transition, and against the server's own read of
    // each target: a set whose second page moved since it was proposed must not be confirmed
    // on the strength of a revision the client supplied.
    let drift = change_sets::store::drifted_targets(state.db().pool(), organization, &set)
        .await
        .map_err(ApiError::from)?;
    if !drift.is_empty() {
        return Err(ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "stale",
            format!(
                "{} of this set's targets changed since it was proposed: {}",
                drift.len(),
                drift.join(", ")
            ),
        ));
    }

    if set.is_irreversible() {
        let expected = set.title.trim().to_lowercase();
        let given = body.confirmation_phrase.trim().to_lowercase();
        if given != expected {
            return Err(ApiError::bad_request(
                "confirmation_phrase",
                format!(
                    "this set deletes content, so confirming it needs its title typed out (`{expected}`)"
                ),
            ));
        }
    }

    let gated = set.gated_operations();
    let parked = park_gated_operations(&state, organization, actor, &set, &gated).await?;
    let needs_approval = !parked.is_empty();

    // A set with nothing gated is confirmed here; one that parked is `pending`, because
    // `pending → confirmed` is the edge the inbox's release takes and `draft → confirmed` is
    // the edge this route takes for a set that needs no second person.
    let target = if needs_approval {
        "pending"
    } else {
        "confirmed"
    };
    let confirmed = change_sets::store::transition(
        state.db().pool(),
        organization,
        id,
        &set.status,
        target,
        None,
    )
    .await
    .map_err(ApiError::from)?
    .ok_or_else(|| {
        ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "not_confirmable",
            format!(
                "this change set is now `{}` and was decided already",
                set.status
            ),
        )
    })?;

    bus::emit(
        state.db().pool(),
        NewEvent::new("ai.changeset.confirmed")
            .organization(organization)
            .actor(actor)
            .payload(serde_json::json!({
                "change_set_id": id,
                "operations": confirmed.operations.len(),
                "irreversible": confirmed.is_irreversible(),
                "parked_for_approval": needs_approval,
            })),
    )
    .await?;

    // The flag is read **before** the row is moved into the response, and the order is the
    // point: computing it afterwards would borrow a moved value, and computing it from a
    // clone would let the two answers describe different instants.
    let confirmed_gated = confirmed.has_gated_operations();
    Ok(Json(Confirmed {
        set: confirmed,
        needs_approval: confirmed_gated,
        approvals: parked,
        applied: false,
    }))
}

/// File one approval per gated operation, and return the rows it filed.
///
/// The preview each approval freezes is computed by the **same** [`target::preview`] the
/// single-call path uses, from the same `Operation` the reviewer read in the editor. A hand
/// written preview here would be a second description of the same intent, and the two would
/// disagree exactly when it matters — the reviewer approved the editor's diff, the inbox shows
/// the approval's.
///
/// `change_set_id` is stamped on every row, which is what makes the inbox entry link back to
/// the set the operations belong to; the column exists in `0189` and until now nothing wrote
/// it.
async fn park_gated_operations(
    state: &AppState,
    organization: uuid::Uuid,
    actor: uuid::Uuid,
    set: &change_sets::ChangeSet,
    gated: &[(&change_sets::ChangeOp, &'static str)],
) -> Result<Vec<ParkedApproval>, ApiError> {
    let mut parked = Vec::with_capacity(gated.len());
    for (op, class) in gated {
        let mapping = omnion_ai_hub::approvals::target::mapping_for(&op.operation.resource_type)
            .map_err(ApiError::from)?;
        let plan =
            omnion_ai_hub::approvals::target::preview(state.db().pool(), mapping, &op.operation)
                .await
                .map_err(ApiError::from)?;

        // The policy is read per operation rather than assumed, so an installation that
        // switched a class to `allow` files nothing for it and this row simply does not exist.
        // That is the same resolution `gate()` performs on the single-call path, called
        // through it so the two cannot read a different row.
        let policy =
            omnion_ai_hub::approvals::io::policy_for(state.db().pool(), organization, class)
                .await
                .map_err(ApiError::from)?;
        if !policy.requires_approval() {
            continue;
        }

        let requested = omnion_ai_hub::approvals::io::request(
            state.db().pool(),
            &omnion_ai_hub::approvals::io::NewApproval {
                organization_id: organization,
                site_id: set.site_id,
                run_id: set.created_by_run,
                step_id: None,
                agent_id: set.created_by_agent,
                identity_id: None,
                // The real tool key for this class, not a synthesised one: `class_of_tool` is
                // the gate's own vocabulary, and a key it does not know would be refused by
                // the very store writing the row.
                tool_key: tool_key_for(class).to_owned(),
                tool_class: (*class).to_owned(),
                resource_type: Some(op.operation.resource_type.clone()),
                resource_id: Some(op.operation.resource_id.clone()),
                resource_label: Some(plan.label.clone()).filter(|label| !label.is_empty()),
                title: format!("{} a page: {}", op.operation.kind.label(), plan.label),
                summary: format!(
                    "The agent proposed to {} a page as part of the change set “{}”.",
                    op.operation.kind.label(),
                    set.title
                ),
                operation_count: 1,
                preview: plan.to_preview(mapping),
                preview_hash: plan.hash.clone(),
                base_revision: Some(plan.base_revision.clone()),
                requested_by: Some(actor),
                model_id: None,
                risk: risk_for(class).to_owned(),
                policy,
                requested_at: time::OffsetDateTime::now_utc(),
                change_set_id: Some(set.id),
                // The editor's own key for this operation, so the release path can answer
                // "which operation may now run?" without re-deriving the classification that
                // filed this row (migration `0203`).
                operation_key: Some(op.key.clone()),
            },
        )
        .await
        .map_err(ApiError::from)?;

        parked.push(ParkedApproval {
            id: requested.approval().id,
            operation_key: op.key.clone(),
            class: (*class).to_owned(),
            status: requested.approval().status.clone(),
        });
    }
    Ok(parked)
}

/// The real tool key for a gated class, so `class_of_tool` and the store agree.
///
/// A `match` rather than the first key that maps to the class, because the two are not
/// interchangeable: `content.rollback` is a `content_delete` that *restores*, so using it as
/// the key for a delete proposal would describe an approval the reviewer believes reverses
/// something.
fn tool_key_for(class: &str) -> &'static str {
    match class {
        "content_publish" => "content.publish",
        "content_delete" => "content.delete",
        _ => "content.publish",
    }
}

/// The risk band a parked class carries into the inbox's Risk column.
fn risk_for(class: &str) -> &'static str {
    match class {
        "content_delete" | "deployment" | "database_operation" => "high",
        _ => "medium",
    }
}

/// What a confirmation says about the rows it parked.
///
/// `applied: false` on a parked set is the honest answer and not a placeholder: nothing has
/// happened yet, and the screen routes to the inbox because of it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ParkedApproval {
    pub id: uuid::Uuid,
    /// Which of the set's operations this row is about, so the editor can highlight it.
    pub operation_key: String,
    pub class: String,
    pub status: String,
}

/// What a confirmation answers.
#[derive(Debug, Clone, Serialize)]
pub struct Confirmed {
    #[serde(flatten)]
    pub set: ChangeSet,
    /// `true` when at least one operation is gated and has parked for a human. The screen
    /// routes the reviewer to the inbox rather than pretending the work is done.
    pub needs_approval: bool,
    /// The rows filed, empty for a set that needed no second person.
    pub approvals: Vec<ParkedApproval>,
    /// `false` for a parked set. The ungated arm applies through the store, and that route
    /// answers with the operations it wrote instead.
    pub applied: bool,
}

/// What a confirmation asks for.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ConfirmBody {
    /// The set's own title, required when the set is irreversible.
    #[serde(default)]
    pub confirmation_phrase: String,
}

/// `POST /ai/change-sets/{id}/discard` — drop a set, with a reason.
///
/// A reason is required by the database constraint as well as here, and the double check is
/// deliberate: an unexplained drop is indistinguishable from a bug that lost the work, and
/// the error is much cheaper to prevent at the boundary than to explain afterwards.
pub async fn discard(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(id): Path<uuid::Uuid>,
    Json(body): Json<DiscardBody>,
) -> Result<Json<ChangeSet>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let actor = current.user.id;

    let discarded = change_sets::store::transition(
        state.db().pool(),
        organization,
        id,
        "draft",
        "discarded",
        Some(&body.reason),
    )
    .await
    .map_err(ApiError::from)?
    .ok_or_else(|| {
        ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "not_discardable",
            "this change set is no longer a draft, so it can no longer be discarded",
        )
    })?;

    bus::emit(
        state.db().pool(),
        NewEvent::new("ai.changeset.discarded")
            .organization(organization)
            .actor(actor)
            .payload(serde_json::json!({
                "change_set_id": id,
                "reason": body.reason,
            })),
    )
    .await?;

    Ok(Json(discarded))
}

/// What a discard carries.
#[derive(Debug, Clone, Deserialize)]
pub struct DiscardBody {
    pub reason: String,
}

/// `POST /ai/change-sets/{id}/apply` — run a confirmed set, all of it or none of it.
///
/// This is the route slice 3a left as a seam: the store opens the transaction and hands the
/// applier the same `&mut PgConnection`, so the writes below go **through** it rather than
/// beside it. An applier that took `&PgPool` — which is what
/// `content::pages::update_page` takes — would commit operation 1 on its way to operation 3,
/// and the refusal at 3 would leave two pages rewritten by a set the reviewer was told was
/// all-or-nothing. `update_page_in` is the same writer on a connection it does not own, so
/// there is still exactly one implementation of "append the next revision".
///
/// The order on a refusal is the whole point of the route: roll back, **then** record. A
/// `failed` row written inside the transaction that just failed is the record that vanishes,
/// and the person who has to re-do the work would be looking at a set that still reads
/// `confirmed` — indistinguishable from one that is about to apply.
///
/// # Errors
///
/// `409 change_set_failed` naming the operation, after the set has been rolled back and
/// marked `failed`. `409 not_confirmable` when the set is not `confirmed` at all.
pub async fn apply(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<AppliedSet>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let actor = current.user.id;

    // The set is read once here so the refusal message can name the operation by key, and the
    // store re-reads it **inside** the transaction. Two reads of the same row is not a race: the
    // store's read is the one that decides, and this one only supplies the sentence.
    let known = change_sets::store::read(state.db().pool(), organization, id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "change_set_not_found",
                format!("no change set `{id}` in this organization"),
            )
        })?;
    if known.status != "confirmed" {
        return Err(ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "not_confirmable",
            format!(
                "this change set is `{}`; only a confirmed set can be applied",
                known.status
            ),
        ));
    }

    let applied = apply_set(&state, organization, actor, id).await;

    // The failure half lives in `apply_set`, so the release path gets the rollback, the
    // `failed` row and the event with it rather than a version that reports success on a set
    // whose third operation was refused.
    let applied = applied?;

    bus::emit(
        state.db().pool(),
        NewEvent::new("ai.changeset.applied")
            .organization(organization)
            .actor(actor)
            .payload(serde_json::json!({
                "change_set_id": id,
                "operations": applied.len(),
            })),
    )
    .await?;

    Ok(Json(AppliedSet {
        applied: true,
        set: change_sets::store::read(state.db().pool(), organization, id)
            .await
            .map_err(ApiError::from)?
            .ok_or_else(|| {
                ApiError::new(
                    axum::http::StatusCode::NOT_FOUND,
                    "change_set_not_found",
                    format!("no change set `{id}` in this organization"),
                )
            })?,
        operations: applied,
    }))
}

/// Apply a confirmed set and, on a refusal, record the failure.
///
/// Extracted from [`apply`] so the **release** path can reach it: a parked set is confirmed by
/// the inbox's decision and then applied by exactly this function, which is what makes "one
/// pipeline, one screen" a statement about the code and not about two handlers that happen to
/// agree. Two apply paths with one write is a bug waiting for the version where one of them
/// forgets a field; one apply path with two callers cannot drift like that.
///
/// `mark_failed` and `ai.changeset.failed` live here for the same reason — the ordering that
/// makes them mean anything is *after* the rollback, and rollback is a property of
/// `apply_confirmed`, so the statement that depends on it belongs next to it.
pub(crate) async fn apply_set(
    state: &AppState,
    organization: uuid::Uuid,
    actor: uuid::Uuid,
    id: uuid::Uuid,
) -> Result<Vec<AppliedOp>, ApiError> {
    let applied = change_sets::store::apply_confirmed(
        state.db().pool(),
        organization,
        id,
        move |set, connection| Box::pin(apply_operations(set, connection, actor)),
    )
    .await;

    match applied {
        Ok(applied) => Ok(applied),
        Err(err) => {
            // Rolled back already: `apply_confirmed` owns the transaction. What is left is to
            // make the refusal visible to whoever has to fix it.
            let reason = err.to_string();
            let recorded =
                change_sets::store::mark_failed(state.db().pool(), organization, id, &reason)
                    .await
                    .map_err(ApiError::from)?;

            bus::emit(
                state.db().pool(),
                NewEvent::new("ai.changeset.failed")
                    .organization(organization)
                    .actor(actor)
                    .payload(serde_json::json!({
                        "change_set_id": id,
                        "reason": reason,
                    })),
            )
            .await?;

            // Only annotate when the row is still ours. `Ok(false)` means a competing discard
            // won the race and the row already says why it stopped — a second, different
            // reason on the same record is a worse answer than a conflict.
            let code = if recorded {
                "change_set_failed"
            } else {
                "not_confirmable"
            };
            Err(ApiError::new(
                axum::http::StatusCode::CONFLICT,
                code,
                reason,
            ))
        }
    }
}

/// Apply one set's operations, in order, on the caller's connection.
///
/// The applier lives here and not in `change_sets` because it calls the **content crate**,
/// which the AI hub deliberately does not depend on: the dependency runs the other way, or the
/// content layer would not be usable without the AI hub. A route is a module of the binary, so
/// a walk cannot call this function — which is what the `OperationExecutor` trait on the store
/// is for. The walk supplies an executor that writes real pages through
/// `content::pages::update_page_in` and drives `apply_all`, so the loop, the transaction and
/// the writer are all exercised without a second copy of this function.
///
/// Each operation is re-planned against the target **as it is inside the transaction**, by the
/// same `preview_on` the single-call approval path uses. That is what makes "a change set can
/// never apply something an approval would have refused" structural rather than a promise.
async fn apply_operations(
    set: &ChangeSet,
    connection: &mut sqlx::PgConnection,
    editor: uuid::Uuid,
) -> Result<Vec<AppliedOp>, omnion_ai_hub::error::AiHubError> {
    // The loop is the store's `apply_all_with`, not a copy of it, and that is a correction
    // rather than a style choice. This file used to annotate only its own **write**, so a
    // refusal from the preview — a page deleted between proposal and apply — reached the
    // reviewer as "`page` a0d4… does not exist": a uuid out of a set whose operations all carry
    // keys, and the acceptance criterion asks for the failing operation. A walk against a real
    // database is what surfaced it, because only a real database can make the second operation
    // fail in the preview rather than in the writer.
    struct PageApplier<'a> {
        connection: &'a mut sqlx::PgConnection,
        editor: uuid::Uuid,
    }

    impl change_sets::AsyncOperationExecutor for PageApplier<'_> {
        fn execute<'a>(
            &'a mut self,
            op: &'a ChangeOp,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<AppliedOp, omnion_ai_hub::error::AiHubError>,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(apply_one(op, self.connection, self.editor))
        }
    }

    let mut applier = PageApplier { connection, editor };
    change_sets::apply_all_with(&set.operations, &mut applier).await
}

/// One operation, through the same plan → change → writer chain the approval apply uses.
///
/// `preview_on` is the *same function* the single-call approval path previews with, run on
/// the applier's connection: the diff this writes is computed from the state inside the
/// transaction, by the module that decides what a page write means. There is no second
/// interpretation of the operation's arguments here to drift from the reviewer's diff.
async fn apply_one(
    op: &ChangeOp,
    connection: &mut sqlx::PgConnection,
    editor: uuid::Uuid,
) -> Result<AppliedOp, omnion_ai_hub::error::AiHubError> {
    let mapping = omnion_ai_hub::approvals::target::mapping_for(&op.operation.resource_type)?;
    let plan =
        omnion_ai_hub::approvals::target::preview_on(&mut *connection, mapping, &op.operation)
            .await?;
    let change = omnion_ai_hub::approvals::target::changes_for(&plan)?;

    // # A delete has to be deleted, not written with nothing
    //
    // This is a defect the walks caught, not a branch that was always here. `is_empty()` below
    // asks "does this change name any field to write", which is the right question for an
    // **update** and the wrong one for a **delete**: a delete's plan carries no diffs at all —
    // the diff IS the target going away (see `plan::preview_on`) — so a delete arrived here
    // with an empty change, fell through `is_empty()`'s update branch into
    // `update_page_in` with every field `None`, and wrote nothing while the transaction
    // committed and the row reported `applied`.
    //
    // The failure mode is the worst one available: a reviewer approves "delete these two
    // pages", the inbox says the set was applied, the audit trail says applied, and both
    // pages are still there. Nothing errors, so nothing reports it.
    //
    // `preview_on` has already proved the target exists (it read the current row to build the
    // diff and the cascades), so reaching here means the delete is applicable.
    if op.operation.kind == omnion_ai_hub::approvals::plan::OpKind::Delete {
        let page_id = page_id_of(op)?;
        let deleted = omnion_content::pages::delete_page_in(connection, page_id)
            .await
            .map_err(|err| {
                omnion_ai_hub::error::AiHubError::InvalidChangeSet(format!(
                    "operation `{}` could not delete page {page_id}: {err}",
                    op.key
                ))
            })?;

        if !deleted {
            // The row is gone although the plan saw it moments ago — a concurrent delete
            // inside this transaction. Reported as applied either way is defensible (the
            // reviewer asked for the page to not exist, and it does not), so this is a
            // deliberate no-op rather than a refusal.
            return Ok(AppliedOp {
                key: op.key.clone(),
                kind: op.operation.kind,
                resource_id: op.operation.resource_id.clone(),
                slug: String::new(),
                status: "deleted".to_owned(),
            });
        }

        return Ok(AppliedOp {
            key: op.key.clone(),
            kind: op.operation.kind,
            resource_id: op.operation.resource_id.clone(),
            // The slug is carried by the row that is now gone, and the editor is not going to
            // invent one: an empty slug is the honest answer, and the panel shows the resource
            // id for a delete anyway.
            slug: String::new(),
            status: "deleted".to_owned(),
        });
    }

    if change.is_empty() {
        // A no-op apply would otherwise report success for an operation that wrote nothing.
        // `plan` already refuses an operation that changes no field, so reaching this means
        // the writer cannot express what the plan describes — which is a refusal, not a
        // silently skipped write.
        return Err(omnion_ai_hub::error::AiHubError::InvalidChangeSet(format!(
            "operation `{}` previews a change this build cannot write",
            op.key
        )));
    }

    let page_id = page_id_of(op)?;
    let page = omnion_content::pages::update_page_in(
        connection,
        page_id,
        &omnion_content::model::PageChanges {
            slug: change.slug,
            title: change.title,
            body: change.body,
            summary: change.summary,
        },
        Some(editor),
    )
    .await
    .map_err(|err| {
        // The page id belongs in the message: the key names a row of the reviewer's list and
        // the id names the thing that refused. `annotate` adds the key around the whole
        // pipeline, so this layer only adds what only it knows.
        omnion_ai_hub::error::AiHubError::InvalidChangeSet(format!(
            "page {page_id} could not be written: {err}"
        ))
    })?;

    Ok(AppliedOp {
        key: op.key.clone(),
        kind: op.operation.kind,
        resource_id: op.operation.resource_id.clone(),
        slug: page.slug,
        status: page.status,
    })
}

/// The page an operation targets, or a refusal that names the key and the id.
///
/// Extracted because the delete branch and the update branch now both need it, and a second
/// copy of the same parse is a second copy of the same message that can drift.
fn page_id_of(op: &ChangeOp) -> Result<uuid::Uuid, omnion_ai_hub::error::AiHubError> {
    op.operation.resource_id.parse().map_err(|_| {
        omnion_ai_hub::error::AiHubError::InvalidChangeSet(format!(
            "operation `{}` targets `{}`, which is not a page id",
            op.key, op.operation.resource_id
        ))
    })
}

/// What an apply answers.
///
/// The row is re-read rather than derived: the screen cannot then render a status the store
/// did not commit.
#[derive(Debug, Clone, Serialize)]
pub struct AppliedSet {
    pub applied: bool,
    #[serde(flatten)]
    pub set: ChangeSet,
    /// One row per operation, in the order they were applied.
    pub operations: Vec<AppliedOp>,
}

/// `GET /ai/change-sets` — the proposed sets, newest first.
///
/// Paged and filtered the same way the approval inbox is, because the two screens are read
/// by the same person minutes apart and a list that cannot be filtered the same way makes one
/// of them the odd one out.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListQuery {
    pub status: Option<String>,
    pub q: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SetList {
    pub sets: Vec<ChangeSet>,
    pub viewer_permissions: BTreeSet<String>,
}

pub async fn list(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Query(query): Query<ListQuery>,
) -> Result<Json<SetList>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_SET_LIMIT)
        .clamp(1, MAX_SET_LIMIT);

    let sets = change_sets::store::list(
        state.db().pool(),
        organization,
        query.status.as_deref(),
        query.q.as_deref(),
        limit,
    )
    .await
    .map_err(ApiError::from)?;

    // Effective permissions, not the raw set on the session: the panel renders the confirm
    // and discard controls from this list, and a list computed from the role's name would
    // offer a button the API then refuses. The same helper the approval inbox uses, so the
    // two screens cannot disagree about what a viewer may do.
    let effective = omnion_permissions::effective_permissions(
        state.db().pool(),
        current.user.id,
        crate::guards::scope_of(&current.user),
    )
    .await?;
    let viewer_permissions = DECISION_KEYS
        .iter()
        .filter(|key| effective.allows(**key))
        .map(|key| (*key).to_string())
        .collect();

    Ok(Json(SetList {
        sets,
        viewer_permissions,
    }))
}

/// The decision keys a change set screen checks the viewer against, and the same two the
/// approval inbox offers. They are shared deliberately: a viewer who may approve a request
/// may confirm a set, and one who may not may still *file* a proposal.
const DECISION_KEYS: [&str; 2] = ["ai.approvals.act", "ai.approvals.read"];

/// The default and maximum page sizes for the set list.
const DEFAULT_SET_LIMIT: i64 = 50;
const MAX_SET_LIMIT: i64 = 200;

/// The outcome type the store returns from an apply, re-exported so a caller reading this
/// module sees the same type the store does.
pub type AppliedSetOps = Vec<AppliedOp>;
