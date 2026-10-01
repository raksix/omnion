//! `/api/v1/ai/approvals` — the review inbox and the class policy (REQ-101, slice 1).
//!
//! This module is the read/write face of `ai_approvals`. It exists on its own because the
//! inbox has a property no other AI route has: **every answer here is a decision that has not
//! been taken yet**, and the API has to keep "here is what would happen", "here is what you may
//! do about it" and "here is what actually happened" in three separate fields or the panel
//! starts implying consent.
//!
//! # The three-way answer of a decision
//!
//! [`DecisionOutcome`](omnion_ai_hub::approvals::DecisionOutcome) has seven arms, and every one
//! of them is reachable through [`approve`]. They collapse into three HTTP shapes, and the
//! collapse is deliberate: a reviewer who typed the wrong phrase, an approval that expired while
//! they were typing, and an approval somebody else decided two seconds earlier are all "you did
//! not decide this, here is the state now". None of them is an error — the row is fine, the
//! *caller's* intent did not land — so they answer `200` with `changed: false` and a code. A
//! handler that turned them into `409` would train the panel to show an error banner for a race
//! the reviewer did not cause.
//!
//! The one arm that *is* an error is a missing or unknown approval: `404`, never `403`, so the
//! inbox cannot become an existence oracle across tenants.
//!
//! # Why the policy screen is here and not in `ai_tools`
//!
//! Un-gating a class (`content_publish` → `allow`) is the strongest single act in the AI hub: it
//! says "from now on, agents may publish without asking". It is guarded by its own permission
//! (`ai.policies.manage`, distinct from `ai.approvals.act`) and, for a dangerous class, a typed
//! phrase naming the class — so the confirmation dialog and the API enforce the same rule rather
//! than the API trusting the dialog.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};
use serde_json::json;

use std::collections::BTreeSet;

use omnion_ai_hub::change_sets;
use omnion_ai_hub::approvals::DecisionOutcome;
use omnion_ai_hub::approvals::PolicyView;
use omnion_ai_hub::approvals::io::{
    self, ApprovalFilter, AuditRow, DbRevisionReader, PolicyChange,
};
use omnion_events::NewEvent;
use omnion_events::bus;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::routes::ai_agents::OrgQuery;
use crate::scope::resolve_organization;
use crate::state::AppState;

/// The window the inbox's `limit` is clamped to.
///
/// A review queue with no upper bound is a denial of service against the browser: a caller that
/// asks for a million rows gets them all serialized, preview objects and all. The inbox itself is
/// paged by the screen, and the cap only exists so one request cannot be unbounded.
const MAX_LIMIT: i64 = 200;
const DEFAULT_LIMIT: i64 = 50;

// -------------------------------------------------------------------------------------------
// Inbox
// -------------------------------------------------------------------------------------------

/// The inbox query, exactly the filters the spec lists: status, agent, tool, class, requester,
/// date range and a free-text search.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InboxQuery {
    pub status: Option<String>,
    pub agent: Option<uuid::Uuid>,
    pub tool: Option<String>,
    pub tool_class: Option<String>,
    pub requester: Option<uuid::Uuid>,
    pub from: Option<time::OffsetDateTime>,
    pub to: Option<time::OffsetDateTime>,
    pub q: Option<String>,
    pub limit: Option<i64>,
}

/// `GET /ai/approvals` — the inbox, plus the per-status counts the tab strip renders, the
/// pending total the sidebar badge shows, and **what this viewer may do with a row**.
///
/// The counts come out of the same store call as the rows, so the badge cannot disagree with
/// the list it sits beside — a mismatch there is how an operator stops trusting the badge.
///
/// [`io::Inbox`] is flattened rather than wrapped: the rows and the counts are different shapes
/// (a list and a map), and a wrapper object the client has to unwrap buys nothing.
#[derive(Debug, Clone, Serialize)]
pub struct InboxScreen {
    #[serde(flatten)]
    pub inbox: io::Inbox,
    pub viewer_permissions: BTreeSet<String>,
    /// The decision keys this viewer is missing. Served beside the rows so the inbox can
    /// render Approve/Reject **disabled and named** rather than enabled-then-refused — the same
    /// contract `/ai/permissions` keeps for its cells, and the write paths enforce it with a
    /// `403` naming the key, so the disabled state is a promise the API keeps.
    pub viewer_missing: BTreeSet<String>,
}

/// The decision keys the inbox and the review screen check against the viewer.
const DECISION_KEYS: [&str; 2] = ["ai.approvals.act", "ai.policies.manage"];

pub async fn list_approvals(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Query(query): Query<InboxQuery>,
) -> Result<Json<InboxScreen>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;

    let filter = ApprovalFilter {
        status: query.status.filter(|value| !value.is_empty()),
        agent: query.agent,
        tool: query.tool.filter(|value| !value.is_empty()),
        tool_class: query.tool_class.filter(|value| !value.is_empty()),
        requester: query.requester,
        from: query.from,
        to: query.to,
        q: query.q.filter(|value| !value.trim().is_empty()),
        limit: Some(query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)),
    };

    let inbox = io::list(state.db().pool(), organization, &filter)
        .await
        .map_err(ApiError::from)?;

    let (viewer_permissions, viewer_missing) =
        viewer_decision_keys(state.db().pool(), &current).await?;

    Ok(Json(InboxScreen {
        inbox,
        viewer_permissions,
        viewer_missing,
    }))
}

/// The viewer's decision keys, split the way the panel needs them.
///
/// The split is recomputed per request from the caller's **effective** permissions rather than
/// from the role's name: two people with the same role can differ, and a panel that guessed
/// from the role would show a button the API refuses.
async fn viewer_decision_keys(
    pool: &sqlx::PgPool,
    current: &CurrentSession,
) -> Result<(BTreeSet<String>, BTreeSet<String>), ApiError> {
    let effective = omnion_permissions::effective_permissions(
        pool,
        current.user.id,
        crate::guards::scope_of(&current.user),
    )
    .await?;

    let granted = DECISION_KEYS
        .iter()
        .filter(|key| effective.allows(**key))
        .map(|key| (*key).to_string())
        .collect();
    let missing = DECISION_KEYS
        .iter()
        .filter(|key| !effective.allows(**key))
        .map(|key| (*key).to_string())
        .collect();

    Ok((granted, missing))
}

/// One row, as the inbox and the review screen render it.
///
/// The store's `Approval` is the database shape: every column, including the internal ones the
/// panel has no use for. This view is the screen shape, and it makes exactly one addition of its
/// own — [`ApprovalView::decidable`] — because "may this person decide" is a question about the
/// **row's** state (pending, unexpired) and the panel must not re-derive it from two timestamps
/// it might round differently than the server does.
#[derive(Debug, Clone, Serialize)]
pub struct ApprovalView {
    #[serde(flatten)]
    pub approval: omnion_ai_hub::approvals::Approval,
    /// Whether the row is still decidable at all (pending and unexpired).
    pub decidable: bool,
    /// Whether this row demands a typed phrase, and what it is.
    ///
    /// The phrase is served to *any* reader of the inbox, not only to somebody about to type
    /// it. That looks like a leak and is not: the phrase is the resource's **name**, which the
    /// inbox already prints in the Resource column, so withholding it from the detail screen
    /// would buy nothing while making the confirmation dialog impossible to render.
    pub requires_confirmation: bool,
    pub confirmation_phrase: Option<String>,
}

impl ApprovalView {
    /// The clock is a parameter, not `now_utc()` inside: the same `of()` renders the row for the
    /// inbox (many rows, one instant) and for the detail screen, and a per-row `now_utc()` would
    /// make the answer depend on how long the query took.
    fn of(approval: omnion_ai_hub::approvals::Approval, now: time::OffsetDateTime) -> Self {
        let decidable = approval.is_decidable(now);
        Self {
            decidable,
            requires_confirmation: approval.requires_confirmation,
            confirmation_phrase: approval.confirmation_phrase.clone(),
            approval,
        }
    }
}

/// `GET /ai/approvals/{id}` — one request, with its audit trail.
///
/// The trail is part of the same response rather than a second endpoint: a reviewer reading a
/// decided request asks "who did this and why" as part of the same screen, and splitting it
/// means the screen makes a second request whose answer can be from a different moment.
pub async fn get_approval(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<ApprovalDetail>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;

    let approval = io::read(state.db().pool(), organization, id)
        .await
        .map_err(ApiError::from)?
        .ok_or(AiNotFound(id))?;

    let trail = io::audit_trail(state.db().pool(), organization, id)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(ApprovalDetail {
        approval: ApprovalView::of(approval, time::OffsetDateTime::now_utc()),
        audit: trail,
    }))
}

/// The review screen's body.
#[derive(Debug, Clone, Serialize)]
pub struct ApprovalDetail {
    pub approval: ApprovalView,
    /// Oldest first: a reviewer reads a request as a story, and "requested, then decided" in
    /// that order is the story.
    pub audit: Vec<AuditRow>,
}

/// A missing approval. A private newtype so the `404` is written once and the id is named in
/// the message without the message ever reaching a caller who should not learn the id exists.
#[derive(Debug)]
struct AiNotFound(uuid::Uuid);

impl From<AiNotFound> for ApiError {
    fn from(missing: AiNotFound) -> Self {
        Self::new(
            axum::http::StatusCode::NOT_FOUND,
            "approval_not_found",
            format!("no approval `{}` in this organization", missing.0),
        )
    }
}

// -------------------------------------------------------------------------------------------
// Decisions
// -------------------------------------------------------------------------------------------

/// What the review screen posts.
#[derive(Debug, Clone, Deserialize)]
pub struct DecisionBody {
    /// The phrase for an irreversible class. The server checks it exactly; the client must send
    /// `null`/absent until the field is non-empty, because sending `"yes"` is how a checkbox
    /// becomes a confirmation.
    #[serde(default)]
    pub confirmation: Option<String>,
    /// The revision the reviewer was looking at. When it no longer matches the row's base
    /// revision the decision answers `stale` instead of applying — time-of-check to time-of-use.
    #[serde(default)]
    pub current_revision: Option<String>,
}

/// The shape every decision answers with, whatever it decided.
#[derive(Debug, Clone, Serialize)]
pub struct DecisionResult {
    /// Whether a decision was **recorded**. A stale, expired or already-decided answer is `200`
    /// with `false`.
    pub changed: bool,
    /// The machine-readable outcome, so the screen branches on a code instead of on prose.
    pub code: Option<&'static str>,
    /// The row as it now stands.
    pub approval: ApprovalView,
    /// For a `stale` outcome: the revision the resource is at now, so Re-preview has something
    /// real to compute against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_revision: Option<String>,
}

/// `POST /ai/approvals/{id}/approve`
///
/// Every arm of [`DecisionOutcome`] is mapped here, and the mapping is the interesting part:
/// **`Stale` is the only one that carries a payload the screen needs**, because Re-preview is
/// useless without the revision to re-preview against.
pub async fn approve(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(id): Path<uuid::Uuid>,
    Json(body): Json<DecisionBody>,
) -> Result<Json<DecisionResult>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let actor = current.user.id;

    // The revision is read by the server, from the row, at decision time. The body's
    // `current_revision` is what the screen *saw*; it is recorded but never trusted, because a
    // client that echoed the stored base revision would otherwise pass the freshness check
    // every time (slice 1's flaw).
    let outcome = io::approve(
        state.db().pool(),
        organization,
        id,
        actor,
        body.confirmation.as_deref(),
        &DbRevisionReader,
        time::OffsetDateTime::now_utc(),
    )
    .await
    .map_err(ApiError::from)?;

    let result = render(outcome)?;

    if result.changed {
        bus::emit(
            state.db().pool(),
            NewEvent::new("ai.approval.decided")
                .organization(organization)
                .actor(actor)
                .payload(json!({
                    "approval_id": id,
                    "decision": "approved",
                })),
        )
        .await?;
    }

    Ok(Json(result))
}

/// `POST /ai/approvals/{id}/reject` — a reason is mandatory.
///
/// The store refuses a blank reason before it writes anything, so the handler does not repeat
/// the check: two copies of "a rejection needs a reason" is two places for the second one to be
/// forgotten.
pub async fn reject(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(id): Path<uuid::Uuid>,
    Json(body): Json<RejectBody>,
) -> Result<Json<DecisionResult>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let actor = current.user.id;

    let outcome = io::reject(
        state.db().pool(),
        organization,
        id,
        actor,
        &body.reason,
        &DbRevisionReader,
        time::OffsetDateTime::now_utc(),
    )
    .await
    .map_err(ApiError::from)?;

    let result = render(outcome)?;

    if result.changed {
        bus::emit(
            state.db().pool(),
            NewEvent::new("ai.approval.decided")
                .organization(organization)
                .actor(actor)
                .payload(json!({
                    "approval_id": id,
                    "decision": "rejected",
                })),
        )
        .await?;
    }

    Ok(Json(result))
}

/// `POST /ai/approvals/{id}/preview` — recompute the diff against the current revision.
///
/// # The permission is `read`, and that is not a slip
///
/// The spec's own table gives this route `ai.approvals.read`, and the reason is worth stating
/// because "re-preview rewrites a row" sounds like a decision: **it is not one.** Nothing is
/// approved, no run resumes and no resource changes. What it writes is a *newer description of
/// the same proposal* — the reviewer has not decided it yet, so there is nothing to overwrite.
/// Gating it behind `act` would make the banner's own remedy unavailable to exactly the people
/// who can see the request but cannot decide it, which is how a stale row becomes one nobody
/// can clear.
///
/// The body is ignored rather than rejected: a client that posts the diff it computed should
/// get the server's answer, not a `400` for offering. The server's value is the only one used,
/// which is the same rule the decision path follows.
pub async fn re_preview(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<RePreviewResult>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;

    let outcome = io::re_preview(state.db().pool(), organization, id)
        .await
        .map_err(ApiError::from)?;

    let (refreshed, approval) = match outcome {
        io::RePreview::Refreshed(approval) => (true, *approval),
        io::RePreview::Unchanged(approval) => (false, *approval),
    };

    Ok(Json(RePreviewResult {
        refreshed,
        code: (!refreshed).then_some("unchanged"),
        approval: ApprovalView::of(approval, time::OffsetDateTime::now_utc()),
    }))
}

/// What a re-preview answers.
///
/// `changed: false` with `code: "unchanged"` is the refusal the request asks for ("refuses when
/// the hash already matches"), expressed the same way the decision endpoints express a race:
/// a `200` the screen reads rather than an error it paints over a row that is fine.
#[derive(Debug, Clone, Serialize)]
pub struct RePreviewResult {
    /// Whether a **new** preview was written.
    pub refreshed: bool,
    pub code: Option<&'static str>,
    pub approval: ApprovalView,
}

/// `POST /ai/approvals/{id}/apply` — run the frozen preview.
///
/// This is the half that makes the request's "the apply reads the same module the preview
/// did" claim true at the HTTP boundary. The route does not re-derive a diff and it does not
/// trust the body's values: it reads the **stored** preview, hands it back to
/// `approvals::plan` for its writes, and applies those. A caller that posts different values
/// is ignored, because the reviewer approved the stored ones.
///
/// The write itself goes through `content::pages::update_page`, so the content crate's own
/// validation runs — a preview can therefore never describe a write the content layer refuses.
pub async fn apply(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<AppliedResult>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;
    let actor = current.user.id;

    let approval = io::read(state.db().pool(), organization, id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "approval_not_found",
                format!("no approval `{id}` in this organization"),
            )
        })?;

    // Only an **approved** row may be applied, and the check is here rather than in the
    // applier because the applier is about writes. A decision that has not been taken is not a
    // license to write, and re-deciding is `already_decided`'s job.
    if approval.status != "approved" {
        return Err(ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "not_approved",
            "an approval must be approved before its preview can be applied",
        ));
    }
    if approval.applied_at.is_some() {
        return Err(ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "already_applied",
            "this approval was already applied",
        ));
    }

    // # An approval that came out of a change set releases the SET, not one operation
    //
    // Slice 3c files one approval per gated operation, and left nothing to release the set
    // when the answers arrived: the bridge parked, and no code path moved `pending →
    // confirmed`. A set with a gated operation could therefore be approved, row by row, and
    // then sit there forever — the pipeline the request asks for existed only in the
    // direction that files work.
    //
    // So an apply on a **set-bound** row is a release, and it is a different code path from
    // the single-operation one below, deliberately:
    //
    // - **Every** gated operation must be approved first (`release_gate`). Approving one row
    //   of a three-page publish is not approving the set, and a bridge that released on the
    //   first decision would be the "a second person releases it" promise answered by a
    //   signature.
    // - The writes go through `apply_confirmed`, so the set is applied by the **same**
    //   all-or-nothing transaction a set applied through `/apply` uses. Two apply paths with
    //   one write is a bug waiting for the version where one of them forgets a field.
    let mut released_set: Option<change_sets::ChangeSet> = None;
    if let Some(change_set_id) = approval.change_set_id {
        let set = release_parked_set(&state, organization, actor, change_set_id).await?;
        released_set = Some(set);
    }

    if released_set.is_some() {
        io::mark_applied(state.db().pool(), organization, id, Some(actor))
            .await
            .map_err(ApiError::from)?;

        bus::emit(
            state.db().pool(),
            NewEvent::new("ai.changeset.released")
                .organization(organization)
                .actor(actor)
                .payload(json!({
                    "approval_id": id,
                    "change_set_id": approval.change_set_id,
                })),
        )
        .await?;

        let set = released_set.expect("checked above");
        return Ok(Json(AppliedResult {
            applied: true,
            resource_type: set.operations[0].operation.resource_type.clone(),
            resource_id: set.operations[0].operation.resource_id.clone(),
            slug: set.operations[0].operation.resource_id.clone(),
            status: set.status.clone(),
            fields_written: set.operations.len(),
        }));
    }

    let plan = omnion_ai_hub::approvals::plan::Plan::from_preview(&approval.preview)
        .map_err(ApiError::from)?;
    let change = omnion_ai_hub::approvals::target::changes_for(&plan).map_err(ApiError::from)?;

    let page_id: uuid::Uuid = plan.resource_id.parse().map_err(|_| {
        ApiError::bad_request("resource_id", "the approval's preview names no page id")
    })?;

    let page = omnion_content::pages::update_page(
        state.db().pool(),
        page_id,
        &omnion_content::model::PageChanges {
            slug: change.slug,
            title: change.title,
            body: change.body,
            summary: change.summary,
        },
        Some(actor),
    )
    .await
    .map_err(ApiError::from)?;

    // `applied_at` is written **only after** the content write succeeded, so a crash between
    // the two leaves the row approved-but-unapplied and a retry is safe. Writing it first
    // would make a failed apply indistinguishable from a finished one.
    io::mark_applied(state.db().pool(), organization, id, Some(actor))
        .await
        .map_err(ApiError::from)?;

    bus::emit(
        state.db().pool(),
        NewEvent::new("ai.approval.applied")
            .organization(organization)
            .actor(actor)
            .payload(json!({
                "approval_id": id,
                "resource_type": plan.resource_type,
                "resource_id": plan.resource_id,
            })),
    )
    .await?;

    Ok(Json(AppliedResult {
        applied: true,
        resource_type: plan.resource_type,
        resource_id: plan.resource_id,
        slug: page.slug,
        status: page.status,
        fields_written: plan.diffs.len(),
    }))
}

/// Release a parked change set, once the inbox has answered **every** one of its gates.
///
/// Three refusals, each naming the row that caused it, because the screen's job here is to
/// tell a reviewer what is still outstanding rather than to say "no":
///
/// - `409 change_set_gates_outstanding` — some gated operation has no approving decision. The
///   message lists the keys, and the keys are what the inbox shows.
/// - `409 not_confirmable` — the set moved on (applied, discarded or failed) since this
///   approval was filed, so there is nothing to release.
/// - `409 change_set_rejected` — a gate was answered with a rejection. A rejected set is
///   **not** applied and not re-confirmed: the reviewer refused, and the correct outcome is a
///   set somebody has to re-propose, not one that proceeds without its refused operation.
///
/// The transition is the same conditional `pending → confirmed` the store's other transitions
/// use, so two people releasing the same set produce one winner and one conflict rather than
/// two applies of the same operations.
async fn release_parked_set(
    state: &AppState,
    organization: uuid::Uuid,
    actor: uuid::Uuid,
    change_set_id: uuid::Uuid,
) -> Result<change_sets::ChangeSet, ApiError> {
    let set = change_sets::store::read(state.db().pool(), organization, change_set_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "change_set_not_found",
                format!("this approval belongs to change set {change_set_id}, which is gone"),
            )
        })?;

    // A rejected gate ends the set rather than stalling it. Read **before** the gate check, so
    // the message is the useful one: a reviewer looking at a refused delete should be told it
    // was refused, not that "1 gate is outstanding" while a rejected row sits in the tab.
    let refused = change_sets::store::rejected_operation_keys(
        state.db().pool(),
        organization,
        change_set_id,
    )
    .await
    .map_err(ApiError::from)?;

    if !refused.is_empty() {
        return Err(ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "change_set_rejected",
            format!(
                "this change set was rejected at {}; re-propose it rather than applying it",
                refused.join(", ")
            ),
        ));
    }

    let approved =
        change_sets::store::approved_operation_keys(state.db().pool(), organization, change_set_id)
            .await
            .map_err(ApiError::from)?;

    match change_sets::release_gate(&set, &approved) {
        change_sets::Gate::Blocked { outstanding } => Err(ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "change_set_gates_outstanding",
            format!(
                "this change set still has {} gate(s) nobody answered: {}",
                outstanding.len(),
                outstanding.join(", ")
            ),
        )),
        change_sets::Gate::Released => {
            let confirmed = change_sets::store::transition(
                state.db().pool(),
                organization,
                change_set_id,
                "pending",
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
                        "this change set is now `{}` and was released already",
                        set.status
                    ),
                )
            })?;

            debug_assert_eq!(confirmed.status, "confirmed");

            crate::routes::ai_change_sets::apply_set(state, organization, actor, change_set_id)
                .await?;

            // Re-read rather than deriving: the screen must not render a status the store did
            // not commit, and this row's `applied` is the whole claim of the call.
            change_sets::store::read(state.db().pool(), organization, change_set_id)
                .await
                .map_err(ApiError::from)?
                .ok_or_else(|| {
                    ApiError::new(
                        axum::http::StatusCode::NOT_FOUND,
                        "change_set_not_found",
                        format!("change set {change_set_id} vanished while it was being applied"),
                    )
                })
        }
    }
}

/// What an apply answers.
#[derive(Debug, Clone, Serialize)]
pub struct AppliedResult {
    pub applied: bool,
    pub resource_type: String,
    pub resource_id: String,
    /// The slug the page carries now, so the screen can link straight at it.
    pub slug: String,
    /// Its status now — `draft` for an update that did not publish.
    pub status: String,
    /// How many mapped fields the frozen preview actually wrote. `0` on a delete, whose
    /// consequence is the row going away rather than a field changing.
    pub fields_written: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RejectBody {
    pub reason: String,
}

/// Map the store's seven arms onto one response type.
///
/// Written once and called from both decision handlers on purpose: the approve and reject
/// paths have *identical* outcome shapes (both go through `decide`), and a handler that renders
/// its own copy is how "reject a stale approval" ends up answering `409` while "approve a stale
/// approval" answers `200` — two answers to one rule.
fn render(outcome: DecisionOutcome) -> Result<DecisionResult, ApiError> {
    let (changed, code, approval, current_revision) = match outcome {
        DecisionOutcome::Decided(approval) => (true, None, *approval, None),
        DecisionOutcome::AlreadyDecided(approval) => {
            (false, Some("already_decided"), *approval, None)
        }
        DecisionOutcome::Expired(approval) => (false, Some("expired"), *approval, None),
        DecisionOutcome::Stale { current_revision } => {
            // `Stale` carries no approval: the store refuses before it reads one back. The
            // screen needs the row to re-render against, so it is re-read here — and a row that
            // vanished between the two reads is a genuine 404 rather than a silent empty.
            return Err(ApiError::new(
                axum::http::StatusCode::CONFLICT,
                "stale",
                format!(
                    "the resource changed since this preview was taken; it is now at revision `{current_revision}`"
                ),
            ));
        }
        DecisionOutcome::ConfirmationRequired { .. } => {
            return Err(ApiError::new(
                axum::http::StatusCode::PRECONDITION_REQUIRED,
                "confirmation_required",
                "this operation is irreversible: type the resource's name to confirm it",
            ));
        }
        DecisionOutcome::ConfirmationMismatch { .. } => {
            return Err(ApiError::new(
                axum::http::StatusCode::PRECONDITION_REQUIRED,
                "confirmation_mismatch",
                "the confirmation phrase does not match the resource's name",
            ));
        }
        DecisionOutcome::AlreadyPending => {
            return Err(ApiError::new(
                axum::http::StatusCode::CONFLICT,
                "already_pending",
                "this run step already has a pending approval",
            ));
        }
    };

    Ok(DecisionResult {
        changed,
        code,
        approval: ApprovalView::of(approval, time::OffsetDateTime::now_utc()),
        current_revision,
    })
}

// -------------------------------------------------------------------------------------------
// Class policy
// -------------------------------------------------------------------------------------------

/// `GET /ai/approvals/policies` — the six classes, resolved.
pub async fn list_policies(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
) -> Result<Json<PolicyScreen>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;

    let policies = io::policies(state.db().pool(), organization)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(PolicyScreen {
        policies,
        classes: io::class_options()
            .into_iter()
            .map(|(key, label, irreversible)| ClassOption {
                key,
                label,
                irreversible,
            })
            .collect(),
    }))
}

/// The policy screen's body. `classes` is served beside the rows so the form can render its
/// labels without hard-coding the six names a second time in the client.
#[derive(Debug, Clone, Serialize)]
pub struct PolicyScreen {
    pub policies: Vec<PolicyView>,
    pub classes: Vec<ClassOption>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClassOption {
    pub key: &'static str,
    pub label: &'static str,
    /// Whether the class is irreversible — the three that always demand the typed phrase.
    pub irreversible: bool,
}

/// `PUT /ai/approvals/policies/{class}` — set one class's organization policy.
///
/// The typed phrase is checked **here**, in the route, and not in the store: the phrase names a
/// *class*, and `ai_approval_policies` has no column for it. The store answers "may this class be
/// set to allow" as data; whether this caller deserves to have said it is the route's question,
/// and putting it in the store would have meant a column that exists only to be read by a
/// handler three layers up.
pub async fn put_policy(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(tool_class): Path<String>,
    Json(body): Json<PolicyBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;

    // Un-gating is the dangerous direction. `allow` means "from now on this class runs without
    // asking", and the request requires a typed confirmation naming the class for it — for
    // **every** class, not only the irreversible three, because the risk is cumulative.
    if body.mode == "allow" {
        let expected = format!("set {tool_class} to allow");
        let typed = body
            .confirmation
            .as_deref()
            .map(str::trim)
            .unwrap_or_default();
        if typed.is_empty() {
            return Err(ApiError::new(
                axum::http::StatusCode::PRECONDITION_REQUIRED,
                "confirmation_required",
                format!("type `{expected}` to un-gate this class"),
            ));
        }
        if typed != expected {
            return Err(ApiError::new(
                axum::http::StatusCode::PRECONDITION_REQUIRED,
                "confirmation_mismatch",
                format!("the confirmation phrase must be exactly `{expected}`"),
            ));
        }
    }

    let change = PolicyChange {
        tool_class,
        mode: body.mode,
        typed_confirmation: body.typed_confirmation,
        expires_minutes: body.expires_minutes,
    };

    io::set_policy(
        state.db().pool(),
        organization,
        &change,
        current.user.id,
        time::OffsetDateTime::now_utc(),
    )
    .await
    .map_err(ApiError::from)?;

    bus::emit(
        state.db().pool(),
        NewEvent::new("ai.approval.policy_changed")
            .organization(organization)
            .actor(current.user.id)
            .payload(json!({
                "tool_class": change.tool_class,
                "mode": change.mode,
            })),
    )
    .await?;

    let policies = io::policies(state.db().pool(), organization)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(serde_json::json!({ "policies": policies })))
}

#[derive(Debug, Clone, Deserialize)]
pub struct PolicyBody {
    pub mode: String,
    #[serde(default)]
    pub typed_confirmation: Option<bool>,
    #[serde(default)]
    pub expires_minutes: Option<i32>,
    /// The phrase, required by the handler above when `mode == "allow"`.
    #[serde(default)]
    pub confirmation: Option<String>,
}

/// `DELETE /ai/approvals/policies/{class}` — drop the override, back to the platform default.
///
/// A reset is not a "policy change" and must not be a typed confirmation: removing an override
/// can only *tighten* towards the fail-closed default, so it carries no risk worth a phrase.
pub async fn delete_policy(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
    Path(tool_class): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let organization = resolve_organization(&current, scope.organization_id)?;

    let removed = io::reset_policy(state.db().pool(), organization, &tool_class)
        .await
        .map_err(ApiError::from)?;

    if !removed {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "policy_not_found",
            format!("no organization override for `{tool_class}`"),
        ));
    }

    let policies = io::policies(state.db().pool(), organization)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(serde_json::json!({ "policies": policies })))
}

// -------------------------------------------------------------------------------------------
// The sweeper
// -------------------------------------------------------------------------------------------

/// `POST /ai/approvals/sweep` — expire what is due, now.
///
/// Slice 1 does not run a background task (REQ-099 owns the scheduler), so expiry would only
/// happen on the next read — which means an inbox nobody opens keeps showing `pending` rows that
/// can never be decided. This endpoint is the manual half of that, and it is also what a
/// deployment's own cron calls; it is deliberately **not** permission-guarded beyond
/// `ai.approvals.act`, because expiring a row destroys a decision opportunity and must not be
/// something a viewer can trigger.
pub async fn sweep(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope): Query<OrgQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _ = resolve_organization(&current, scope.organization_id)?;
    let expired = io::expire_due(state.db().pool(), time::OffsetDateTime::now_utc(), 100)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({ "expired": expired.len() })))
}
