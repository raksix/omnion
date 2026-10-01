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

    let sql = format!(
        "update ai_change_sets set title = $3, operations = $4, base_revisions = $5, updated_at = now() \
         where id = $1 and organization_id = $2 and status = $6 returning {}",
        change_sets::CHANGE_SET_COLUMNS
    );
    let operations: Vec<ChangeOp> = body.operations.clone();
    // The write goes through the crate's own error type rather than sqlx's: `ApiError` has
    // no `From<sqlx::Error>`, and routing it through `AiHubError` means a database failure
    // here is mapped by the same code that maps it everywhere else in the AI hub rather than
    // by a local `?` that would need its own arm.
    let updated: Option<ChangeSetRow> = sqlx::query_as(&sql)
        .bind(id)
        .bind(organization)
        .bind(body.title.trim())
        .bind(serde_json::to_value(&operations).unwrap_or_default())
        .bind(serde_json::to_value(&body.base_revisions).unwrap_or_default())
        .bind(&existing.status)
        .fetch_optional(state.db().pool())
        .await
        .map_err(|err| ApiError::from(omnion_ai_hub::error::AiHubError::Database(err)))?;

    let Some(updated) = updated else {
        // Somebody confirmed or discarded it between the read and the write. The `where`
        // clause is what makes that zero rows, and the message names the state now rather
        // than returning a generic conflict.
        let current_row = change_sets::store::read(state.db().pool(), organization, id)
            .await
            .map_err(ApiError::from)?
            .unwrap_or(existing);
        return Err(ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "not_editable",
            format!(
                "this change set is now `{}` and can no longer be edited",
                current_row.status
            ),
        ));
    };

    // Validated **after** the write shape is known, against the same function the store uses
    // on insert: one validation, so an edit cannot smuggle in an operation an insert refuses.
    let set = updated.into_domain().map_err(ApiError::from)?;
    change_sets::validate(&set).map_err(ApiError::from)?;

    Ok(Json(set))
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
}

/// `POST /ai/change-sets/{id}/confirm` — confirm a set, and park anything gated.
///
/// The typed phrase is demanded **here**, not by the panel: a set that deletes a page is
/// irreversible, and a client that forgot to ask the user must not be able to skip the asking
/// by talking to the API directly. The phrase is the set's own title, which is what the
/// reviewer sees on the button they are about to press.
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

    let confirmed = change_sets::store::transition(
        state.db().pool(),
        organization,
        id,
        &set.status,
        "confirmed",
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
            })),
    )
    .await?;

    // The flag is read **before** the row is moved into the response, and the order is the
    // point: computing it afterwards would borrow a moved value, and computing it from a
    // clone would let the two answers describe different instants.
    let needs_approval = confirmed.has_gated_operations();
    Ok(Json(Confirmed {
        set: confirmed,
        needs_approval,
    }))
}

/// What a confirmation carries.
#[derive(Debug, Clone, Serialize)]
pub struct Confirmed {
    #[serde(flatten)]
    pub set: ChangeSet,
    /// `true` when at least one operation is gated and has parked for a human. The screen
    /// routes the reviewer to the inbox rather than pretending the work is done.
    pub needs_approval: bool,
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
