//! `/api/v1/environments/{id}/promotions` and `/api/v1/promotions/{id}` (REQ-017 slice 3).
//!
//! Four writes and two reads, and the permission split is the point:
//!
//! | Route | Permission |
//! |---|---|
//! | `GET /environments/{id}/promotions` | `deployment.read` |
//! | `GET /promotions/{id}` | `deployment.read` |
//! | `POST /environments/{id}/promotions` | `deployment.preview` |
//! | `POST /promotions/{id}/cancel` | `deployment.preview` |
//! | `POST /promotions/{id}/approve` | `deployment.deploy` |
//!
//! Requesting a promotion is a *preview* action: it records an intent and changes nothing. It is
//! deliberately NOT `deployment.deploy`, so the account that fills a staging copy can ask for its
//! work to be considered without being able to push it. Approving is `deployment.deploy` — the one
//! route in this request that writes to production.
//!
//! # Self-approval
//!
//! The spec asks for two paths and one refusal, and the rule is more precise than "two people
//! must be involved". `self_approval_refused` fires when the approver is the requester **and**
//! lacks `deployment.deploy`. A caller who *does* hold `deployment.deploy` may approve their own
//! request: that is the single-tenant case, where refusing would leave a one-person team unable
//! to ship at all, and the deploy key is exactly the authority that says "I am allowed to decide".
//! Two-person approval is a workflow concern and belongs to REQ-069's policy engine — inventing
//! a second approval table here would put a half-built version of it in the platform core.
//!
//! # What the approver sees is what runs
//!
//! The route freezes the *live* change set into the row, computes the conflicts, and returns
//! them. The apply re-checks them. Nothing in this file re-reads staging content between the
//! request and the approval, which is the property the whole design exists to guarantee.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_environment::changes::ChangeSet;
use omnion_environment::error::EnvironmentError;
use omnion_environment::model::EnvironmentType;
use omnion_environment::promotion::{FrozenChangeSet, FrozenItem, needs_typed_confirmation};
use omnion_environment::promotion_store::{self, NewPromotion, PromotionRow};
use omnion_environment::store;
use omnion_events::NewEvent;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::routes::environments::organization_of;
use crate::state::AppState;

/// Whether the caller may decide to write to production, re-checked in the handler.
///
/// The route layer already guarded this route with `deployment.deploy`, so this is a *second*
/// question and not the same one: the guard answers "may this caller approve any promotion",
/// and this answers "may this caller approve **their own**". Both go through
/// `omnion_permissions::authorize` — the one decision path the guard, the list filters and the
/// simulator all use — so a policy that denies `deployment.deploy` to this account denies it here
/// too, rather than the two disagreeing because one of them read a different table.
async fn may_deploy(state: &AppState, current: &CurrentSession) -> Result<bool, ApiError> {
    let organization_id = organization_of(current)?;
    let decision = omnion_permissions::authorize(
        state.db().pool(),
        current.user.id,
        omnion_permissions::Scope::Organization { organization_id },
        "deployment.deploy",
    )
    .await?;
    Ok(matches!(decision, omnion_permissions::Decision::Allowed(_)))
}

// ---------------------------------------------------------------------------------------------
// Request and response shapes
// ---------------------------------------------------------------------------------------------

/// Body of `POST /api/v1/environments/{id}/promotions`.
///
/// An empty `items` means "everything that differs", which is what the dialog's primary button
/// sends. A partial selection is the bulk action's job ("select non-conflicting rows → Promote
/// selection"), so the field exists and is honoured rather than being reserved.
#[derive(Debug, Default, Deserialize)]
pub struct PromotionRequestInput {
    /// The `page_id`s to promote, by natural-key id from the change set. Empty = all of them.
    #[serde(default)]
    pub items: Vec<Uuid>,
}

/// One promotion as the Promotions tab and the dialog read it.
#[derive(Debug, Serialize)]
pub struct PromotionBody {
    /// Primary key.
    pub id: Uuid,
    /// The staging environment the changes come from.
    pub environment_id: Uuid,
    /// Where they land.
    pub target_environment_id: Uuid,
    /// `pending_approval`, `approved`, `running`, `done`, `failed` or `cancelled`.
    pub status: String,
    /// How many items the frozen set holds.
    pub item_count: usize,
    /// How many write a production row (deletions do not).
    pub write_count: usize,
    /// Count of `added` items.
    pub added: usize,
    /// Count of `updated` items.
    pub updated: usize,
    /// Count of `deleted` items.
    pub deleted: usize,
    /// The conflicting item ids, as the dialog's conflict list.
    pub conflicts: Vec<Uuid>,
    /// Whether the dialog must ask for a typed confirmation — a *decision computed on the server*
    /// so the panel and the promotion can never disagree about the threshold.
    pub requires_typed_confirmation: bool,
    /// Who requested it.
    pub requested_by: Option<Uuid>,
    /// Who approved it.
    pub approved_by: Option<Uuid>,
    /// When it was approved.
    pub approved_at: Option<time::OffsetDateTime>,
    /// The step log, as the dialog's timeline reads it.
    pub steps: Vec<StepBody>,
    /// The failure message.
    pub error: Option<String>,
    /// When it was requested.
    pub created_at: time::OffsetDateTime,
    /// When it finished.
    pub finished_at: Option<time::OffsetDateTime>,
}

/// One entry of the promotion's step log.
#[derive(Debug, Serialize)]
pub struct StepBody {
    /// `validate`, `apply`, `audit` or `done`.
    pub step: String,
    /// When it completed, as the `time` crate's array form — the shape every timestamp in this API
    /// serialises to.
    pub at: time::OffsetDateTime,
    /// A short phrase a person can read.
    pub detail: String,
}

impl From<&omnion_environment::promotion::StepEntry> for StepBody {
    fn from(entry: &omnion_environment::promotion::StepEntry) -> Self {
        StepBody {
            step: entry.step.clone(),
            at: entry.at,
            detail: entry.detail.clone(),
        }
    }
}

impl PromotionBody {
    /// Flatten a stored promotion.
    ///
    /// The counts come from the *decoded* change set rather than from stored columns. A promotion
    /// whose counts disagree with its own frozen items is worse than one with no counts: the
    /// dialog's whole job is to tell the operator what they are about to publish.
    pub fn build(row: &PromotionRow) -> Result<PromotionBody, ApiError> {
        let change_set = row.change_set()?;
        let conflicts: Vec<Uuid> =
            serde_json::from_value(row.conflicts.clone()).unwrap_or_default();
        Ok(PromotionBody {
            id: row.id,
            environment_id: row.environment_id,
            target_environment_id: row.target_environment_id,
            status: row.status.clone(),
            item_count: change_set.item_count(),
            write_count: change_set.writes(),
            added: change_set.added(),
            updated: change_set.updated(),
            deleted: change_set.deleted(),
            requires_typed_confirmation: needs_typed_confirmation(change_set.item_count()),
            conflicts,
            requested_by: row.requested_by,
            approved_by: row.approved_by,
            approved_at: row.approved_at,
            steps: row.steps().iter().map(StepBody::from).collect(),
            error: row.error.clone(),
            created_at: row.created_at,
            finished_at: row.finished_at,
        })
    }
}

/// The frozen set as the dialog's summary reads it, so the panel does not re-derive the counts.
#[derive(Debug, Serialize)]
pub struct FrozenBody {
    /// The staging environment.
    pub environment_id: Uuid,
    /// The target.
    pub target_environment_id: Uuid,
    /// One object per item.
    pub items: Vec<FrozenItem>,
}

/// The answer to a promotion request: the record, plus the change set it froze.
///
/// Both in one response because the dialog opens on this answer and has to show the operator what
/// was frozen. A second request to render it would be a second instant, and an edit landing
/// between them would show the dialog a set the record does not hold.
#[derive(Debug, Serialize)]
pub struct PromotionRequestedBody {
    /// The record.
    pub promotion: PromotionBody,
    /// The frozen change set.
    pub changes: FrozenBody,
}

// ---------------------------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/environments/{id}/promotions` — the promotion history of one environment.
pub async fn list_promotions(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<PromotionBody>>, ApiError> {
    let organization_id = organization_of(&current)?;
    let pool = state.db().pool();
    // Tenancy first: an environment of another organization answers 404 before a promotion is
    // read, and a promotion id leaks the environment id it belongs to.
    store::find(pool, organization_id, id).await?;

    let rows = promotion_store::list(pool, id, 50).await?;
    let bodies = rows
        .iter()
        .map(PromotionBody::build)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(bodies))
}

/// `POST /api/v1/environments/{id}/promotions` — request a promotion of a frozen change set.
pub async fn request_promotion(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Json(input): Json<PromotionRequestInput>,
) -> Result<(StatusCode, Json<PromotionRequestedBody>), ApiError> {
    let organization_id = organization_of(&current)?;
    let pool = state.db().pool();
    let environment = store::find(pool, organization_id, id).await?;

    // Promoting production "into" production is not a thing, and the refusal names it rather than
    // reporting an empty change set — which is what the comparison would otherwise produce.
    if environment.kind() != EnvironmentType::Staging {
        return Err(EnvironmentError::NotStaging {
            key: environment.key,
        }
        .into());
    }

    let production_id = environment.cloned_from_environment_id.ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "environment_no_clone_source",
            "This environment has no clone source, so there is nothing to promote it to.",
        )
    })?;
    store::find(pool, organization_id, production_id).await?;

    let change_set =
        omnion_environment::changes::diff_against_production(pool, id, production_id).await?;

    // An empty selection is refused in words. Silently freezing nothing would create a promotion
    // that is permanently `done` with 0 items, and the Promotions tab would fill with rows that
    // record nobody having done anything.
    if change_set.is_empty() && input.items.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "promotion_empty",
            "This environment has no changes since the clone, so there is nothing to promote.",
        ));
    }

    let frozen = freeze(&change_set, &input.items, id, production_id, pool).await?;
    let conflicts = promotion_store::find_conflicts(pool, &frozen).await?;

    let row = promotion_store::request(
        pool,
        &NewPromotion::new(id, production_id, current.user.id, frozen.clone())
            .with_conflicts(conflicts.clone()),
    )
    .await?;

    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(current.user.id, "promotion.requested")
            .target("promotion", row.id.to_string())
            .metadata(json!({
                "environment_id": id,
                "environment_key": environment.key,
                "items": frozen.item_count(),
                "conflicts": conflicts.len(),
            })),
    )
    .await?;
    omnion_events::bus::emit(
        pool,
        NewEvent::new("promotion.requested")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "promotion_id": row.id,
                "environment_id": id,
                "items": frozen.item_count(),
                "conflicts": conflicts.len(),
            })),
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(PromotionRequestedBody {
            promotion: PromotionBody::build(&row)?,
            changes: FrozenBody {
                environment_id: frozen.environment_id,
                target_environment_id: frozen.target_environment_id,
                items: frozen.items,
            },
        }),
    ))
}

/// Freeze the selected rows of a live change set.
///
/// A selection that names an id the change set does not contain is refused rather than ignored:
/// the operator selected a row and got a promotion without it, which is the silent-wrong-answer
/// class this request is most careful about. Empty means all.
async fn freeze(
    change_set: &ChangeSet,
    selection: &[Uuid],
    environment_id: Uuid,
    production_id: Uuid,
    pool: &sqlx::PgPool,
) -> Result<FrozenChangeSet, ApiError> {
    let selected: Vec<Uuid> = if selection.is_empty() {
        change_set.items.iter().map(|item| item.page_id).collect()
    } else {
        let known: Vec<Uuid> = change_set.items.iter().map(|item| item.page_id).collect();
        let unknown: Vec<Uuid> = selection
            .iter()
            .filter(|id| !known.contains(id))
            .copied()
            .collect();
        if !unknown.is_empty() {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "promotion_item_not_in_change_set",
                format!(
                    "{} selected item(s) are not in this environment's change set: {}",
                    unknown.len(),
                    unknown
                        .iter()
                        .map(|id| id.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
        selection.to_vec()
    };

    let mut items = Vec::with_capacity(selected.len());
    for row in change_set
        .items
        .iter()
        .filter(|row| selected.contains(&row.page_id))
    {
        let (base_updated_at, base_digest) = production_baseline(pool, production_id, row).await?;
        items.push(FrozenItem::freeze(row, base_updated_at, base_digest));
    }
    Ok(FrozenChangeSet::new(environment_id, production_id, items))
}

/// Production's timestamp and digest for one change-set row, which is what gets frozen.
///
/// For an `added` row there is no production side and both values are absent — and `FrozenItem::freeze`
/// drops them again, because a baseline for a row that does not exist is a contradiction the
/// conflict check would later trip over.
async fn production_baseline(
    pool: &sqlx::PgPool,
    production_id: Uuid,
    row: &omnion_environment::changes::ChangeItem,
) -> Result<(Option<time::OffsetDateTime>, String), ApiError> {
    let state: Option<(Option<time::OffsetDateTime>, Option<String>)> = sqlx::query_as(
        "select p.updated_at, \
                encode(sha256(convert_to(coalesce(r.title, '') || chr(1) || coalesce(r.body, ''), 'UTF8')), 'hex') \
         from pages p left join page_revisions r on r.id = p.published_revision_id \
         where p.environment_id = $1 and p.site_id = $2 and p.slug = $3",
    )
    .bind(production_id)
    .bind(row.site_id)
    .bind(&row.slug)
    .fetch_optional(pool)
    .await
    .map_err(|err| ApiError::from(EnvironmentError::Store { message: err.to_string() }))?;
    Ok(match state {
        Some((updated_at, digest)) => (updated_at, digest.unwrap_or_default()),
        None => (None, String::new()),
    })
}

/// `GET /api/v1/promotions/{id}` — one promotion: its frozen change set, its conflicts, its steps.
pub async fn get_promotion(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<PromotionDetailBody>, ApiError> {
    let organization_id = organization_of(&current)?;
    let pool = state.db().pool();
    let row = promotion_store::find(pool, id).await?;
    // The promotion is found by id first and the tenancy check reads its environment. A promotion
    // of another organization is a 404 on the environment, and it is the environment that is
    // scoped — the promotion row itself carries no organization column, by design.
    store::find(pool, organization_id, row.environment_id).await?;

    let change_set = row.change_set()?;
    Ok(Json(PromotionDetailBody {
        promotion: PromotionBody::build(&row)?,
        changes: FrozenBody {
            environment_id: change_set.environment_id,
            target_environment_id: change_set.target_environment_id,
            items: change_set.items,
        },
    }))
}

/// One promotion with the set it froze.
#[derive(Debug, Serialize)]
pub struct PromotionDetailBody {
    /// The record.
    pub promotion: PromotionBody,
    /// The frozen change set.
    pub changes: FrozenBody,
}

/// `POST /api/v1/promotions/{id}/approve` — approve and apply the frozen change set to production.
pub async fn approve_promotion(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<PromotionBody>, ApiError> {
    let organization_id = organization_of(&current)?;
    let pool = state.db().pool();
    let row = promotion_store::find(pool, id).await?;
    let environment = store::find(pool, organization_id, row.environment_id).await?;

    // Self-approval is refused only for a requester who cannot deploy. See the module header for
    // why this is not simply "never your own promotion".
    if row.requested_by == Some(current.user.id) && !may_deploy(&state, &current).await? {
        promotion_store::fail(
            pool,
            row.id,
            "the requester may not approve their own promotion",
            omnion_environment::promotion::Step::Validate,
        )
        .await?;
        return Err(EnvironmentError::SelfApprovalRefused.into());
    }

    // The store owns the failure bookkeeping, on purpose. It is the only layer that knows whether
    // the row was ever claimed as `running`, and a caller-side `fail()` was leaving a failed apply
    // stuck in `running` whenever the failure happened before the route saw it — which then made
    // `promotions_single_running` refuse every later promotion of that environment.
    let outcome = promotion_store::approve_and_apply(pool, &row, current.user.id).await?;

    promotion_store::finish(pool, row.id).await?;

    // The audit entry and the event, after the apply committed. `promotion.completed` carries the
    // affected ids — one event, not one per row, which is the request's explicit instruction: a
    // per-row `page.published` would flood every subscriber of a large deploy.
    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(current.user.id, "promotion.completed")
            .target("promotion", row.id.to_string())
            .metadata(json!({
                "environment_id": environment.id,
                "environment_key": environment.key,
                "written": outcome.written,
                "removed": outcome.removed,
                "items": outcome.affected.len(),
            })),
    )
    .await?;
    omnion_events::bus::emit(
        pool,
        NewEvent::new("promotion.completed")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "promotion_id": row.id,
                "environment_id": environment.id,
                "written": outcome.written,
                "removed": outcome.removed,
                "items": outcome.affected,
            })),
    )
    .await?;

    let updated = promotion_store::find(pool, id).await?;
    Ok(Json(PromotionBody::build(&updated)?))
}

/// `POST /api/v1/promotions/{id}/cancel` — withdraw a promotion that has not started.
pub async fn cancel_promotion(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<PromotionBody>, ApiError> {
    let organization_id = organization_of(&current)?;
    let pool = state.db().pool();
    let row = promotion_store::find(pool, id).await?;
    store::find(pool, organization_id, row.environment_id).await?;

    // Only the requester may withdraw their own request. Somebody else's pending promotion is
    // cancelled by refusing to approve it, not by cancelling it — the audit trail of "this was
    // asked and this was refused" is worth more than a tidy list.
    if let Some(requester) = row.requested_by {
        if requester != current.user.id {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "promotion_not_yours",
                "Only the person who requested this promotion can withdraw it.",
            ));
        }
    }

    let cancelled = promotion_store::cancel(pool, &row).await?;
    Ok(Json(PromotionBody::build(&cancelled)?))
}
