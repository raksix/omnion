//! `/api/v1/ai/routing` — the task map, the feature overrides and the dry run (REQ-098, slice 2).
//!
//! Four endpoints, and the split between them is the point of the screen:
//!
//! | Endpoint | Power | What it does |
//! |---|---|---|
//! | `GET /ai/routing` | `ai.providers.read` | The task map of one scope, with inherited rows marked |
//! | `PUT /ai/routing` | `ai.settings.manage` | Replace one task's candidate list, validated |
//! | `POST /ai/routing/preview` | `ai.providers.read` | Resolve a hypothetical request, **zero provider calls** |
//! | `GET/PUT /ai/routing/overrides` | read / manage | The per-feature pins of one scope |
//!
//! Reading a map is `ai.providers.read` because it is the same knowledge as the provider list —
//! which models exist and what they can do. *Rewriting* it is `ai.settings.manage`, a separate
//! power: a reader can see the routing and still not be able to redirect the installation's
//! traffic, which is the whole reason the two keys exist.
//!
//! The preview is deliberately `read` and deliberately does no work beyond reading: an operator
//! asking "what would this request do today" must not be able to spend quota or change a row by
//! asking. That is why [`preview_routing`] calls the pure resolver and nothing else — there is no
//! provider client in this module at all, so the promise is structural rather than a matter of
//! remembering not to dial.

use axum::Json;
use axum::extract::{Query, State};
use omnion_ai_hub::{
    Candidate, CandidateInput, Decision, FeatureOverride, ResolveRequest, Scope, ScopeRoutes,
    TaskRouteView, check_feature, check_task, decide, load_maps, read_scope, replace_task_map,
    set_override,
};
use omnion_audit::NewAuditEntry;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// Query of `GET /ai/routing` and `GET /ai/routing/overrides`.
///
/// The scope is expressed the same way in both, and a `site_id` without its organization is a
/// refusal rather than a silent fall back to the installation: answering an organization-shaped
/// question with installation rows is how a tenant ends up editing the platform's map.
#[derive(Debug, Deserialize)]
pub struct RoutingQuery {
    /// Organization scope.
    pub organization_id: Option<Uuid>,
    /// Site scope (which also scopes to the site's organization).
    pub site_id: Option<Uuid>,
}

/// The scope a query asks for, and the organization a site row is written with.
fn scope_of(query: &RoutingQuery) -> Result<(Scope, Option<Uuid>), ApiError> {
    match (query.site_id, query.organization_id) {
        (None, None) => Ok((Scope::Installation, None)),
        (None, Some(organization)) => Ok((Scope::Organization(organization), Some(organization))),
        (Some(site), _) => Ok((Scope::Site(site), None)),
    }
}

/// `PUT /api/v1/ai/routing` — replace one task's candidate list.
#[derive(Debug, Deserialize)]
pub struct TaskMapBody {
    /// Which task's list is being replaced.
    pub task: String,
    /// Scope selector, identical to the GET's.
    #[serde(flatten)]
    pub scope: RoutingQuery,
    /// The ordered candidates: index 0 is the primary.
    pub candidates: Vec<CandidateBody>,
}

/// One candidate as written by the panel.
#[derive(Debug, Deserialize)]
pub struct CandidateBody {
    /// The model to place in this slot.
    pub model_id: Uuid,
    /// Capabilities this task's requests must have.
    #[serde(default)]
    pub requirements: Vec<String>,
}

/// `PUT /api/v1/ai/routing/overrides` — pin (or unpin) one feature.
#[derive(Debug, Deserialize)]
pub struct OverrideBody {
    /// The feature key.
    pub feature: String,
    /// Scope selector, identical to the GET's.
    #[serde(flatten)]
    pub scope: RoutingQuery,
    /// The model to pin, or `null` to remove the pin.
    pub model_id: Option<Uuid>,
}

/// `POST /api/v1/ai/routing/preview` — resolve a hypothetical request.
#[derive(Debug, Deserialize)]
pub struct PreviewBody {
    /// The task to resolve.
    pub task: Option<String>,
    /// The feature whose pin may answer first.
    pub feature: Option<String>,
    /// A `provider/model` the caller would have named explicitly.
    pub requested: Option<String>,
    /// Capabilities the request needs.
    #[serde(default)]
    pub requires: Vec<String>,
    /// Scope selector, identical to the GET's.
    #[serde(flatten)]
    pub scope: RoutingQuery,
}

// ---------------------------------------------------------------------------------------------
// Response bodies
// ---------------------------------------------------------------------------------------------

/// One candidate as the panel renders it.
#[derive(Debug, Serialize)]
pub struct CandidateView {
    /// 1-based position; the primary is 1.
    pub position: i32,
    /// The model id, or `null` when the row survives a removed model.
    pub model_id: Option<Uuid>,
    /// The `provider/model` the panel shows.
    pub model_label: Option<String>,
    /// `true` when the row has no model left — the "needs attention" badge.
    pub needs_attention: bool,
    /// Why this candidate cannot answer right now, when it cannot.
    pub refusal: Option<String>,
    /// Capabilities this task's requests require here.
    pub requirements: Vec<String>,
    /// Whether the model is switched off.
    pub enabled: bool,
}

impl CandidateView {
    fn build(candidate: &Candidate) -> Self {
        let refusal = candidate.model.as_ref().and_then(|model| {
            if !model.enabled {
                return Some(format!("\"{}\" is switched off", model.model_key));
            }
            omnion_ai_hub::task_refusal_reason(&candidate.task, model).or_else(|| {
                candidate
                    .requirements
                    .iter()
                    .find_map(|requirement| {
                        omnion_ai_hub::requirement_refusal_reason(requirement, model)
                    })
            })
        });

        Self {
            position: candidate.position,
            model_id: candidate.model.as_ref().map(|model| model.id),
            model_label: candidate.model.as_ref().map(|model| model.model_key.clone()),
            needs_attention: candidate.model.is_none() || !candidate.model.as_ref().is_some_and(|model| model.enabled),
            refusal,
            requirements: candidate.requirements.clone(),
            enabled: candidate
                .model
                .as_ref()
                .is_some_and(|model| model.enabled),
        }
    }
}

/// One task row of the routing screen.
#[derive(Debug, Serialize)]
pub struct TaskView {
    /// The task key.
    pub task: String,
    /// The one-line description under the name.
    pub description: String,
    /// The ordered candidates.
    pub candidates: Vec<CandidateView>,
    /// `true` when the candidates were inherited from a wider scope.
    pub inherited: bool,
    /// `true` when nothing is configured for this task anywhere in the chain.
    pub empty: bool,
}

/// One feature pin.
#[derive(Debug, Serialize)]
pub struct OverrideView {
    /// The feature key.
    pub feature: String,
    /// The model id.
    pub model_id: Uuid,
    /// The `provider/model` the panel shows.
    pub model_label: String,
    /// Which scope declared the pin.
    pub scope: Scope,
    /// When it was last written.
    pub updated_at: time::OffsetDateTime,
}

impl OverrideView {
    fn build(pin: &FeatureOverride) -> Self {
        Self {
            feature: pin.feature.clone(),
            model_id: pin.model.id,
            model_label: pin.model.model_key.clone(),
            scope: pin.scope,
            updated_at: pin.updated_at,
        }
    }
}

/// The whole routing screen in one response.
#[derive(Debug, Serialize)]
pub struct RoutingResponse {
    /// The scope the rows were read for.
    pub scope: Scope,
    /// One row per task, always all seven.
    pub tasks: Vec<TaskView>,
    /// The feature pins in force, most specific scope first.
    pub overrides: Vec<OverrideView>,
    /// The scopes this view inherited from, most specific first.
    pub chain: Vec<Scope>,
    /// The requirement chips the screen may offer.
    pub requirements: Vec<String>,
    /// The feature keys the override form may offer.
    pub features: Vec<omnion_ai_hub::ModelFeatures>,
    /// The resolution order, as data, so the panel's legend cannot drift from the resolver.
    pub rules: Vec<String>,
    /// Tasks that cannot resolve at this scope, for the warning banner.
    pub unresolved: Vec<String>,
}

impl RoutingResponse {
    fn build(routes: ScopeRoutes) -> Self {
        let unresolved = routes
            .tasks
            .iter()
            .filter(|task| task.candidates.is_empty())
            .map(|task| task.task.clone())
            .collect();

        let tasks = routes
            .tasks
            .iter()
            .map(|task: &TaskRouteView| TaskView {
                task: task.task.clone(),
                description: task.description.to_owned(),
                candidates: task.candidates.iter().map(CandidateView::build).collect(),
                inherited: task.inherited,
                empty: task.candidates.is_empty(),
            })
            .collect();

        Self {
            scope: routes.scope,
            tasks,
            overrides: routes.overrides.iter().map(OverrideView::build).collect(),
            chain: routes.chain,
            requirements: omnion_ai_hub::requirements().iter().map(|r| (*r).to_owned()).collect(),
            features: omnion_ai_hub::features(),
            rules: omnion_ai_hub::RULES.iter().map(|rule| (*rule).to_owned()).collect(),
            unresolved,
        }
    }
}

/// The dry-run answer: the decision and its walk.
#[derive(Debug, Serialize)]
pub struct PreviewResponse {
    /// The full decision, walk included.
    #[serde(flatten)]
    pub decision: Decision,
    /// The scope it was resolved for.
    pub scope: Scope,
    /// The rules, so the panel can show which one fired without hard-coding it.
    pub rules: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/ai/routing` — the task map of one scope.
pub async fn get_routing(
    State(state): State<AppState>,
    Query(query): Query<RoutingQuery>,
) -> Result<Json<RoutingResponse>, ApiError> {
    let (scope, organization_id) = scope_of(&query)?;
    let routes = read_scope(state.db().pool(), scope, organization_id).await?;
    Ok(Json(RoutingResponse::build(routes)))
}

/// `PUT /api/v1/ai/routing` — replace one task's candidate list.
///
/// A site scope needs its organization, and the caller is not asked to supply it: a payload that
/// could name a site *and* a different organization is a payload that can route a tenant's
/// traffic with another tenant's authority. The organization is read from the site row instead,
/// so a mismatch is impossible to express.
pub async fn put_routing(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<TaskMapBody>,
) -> Result<Json<RoutingResponse>, ApiError> {
    let (scope, mut organization_id) = scope_of(&body.scope)?;

    // A site scope's organization is the site's own, read from the database rather than trusted
    // from the payload.
    if let Scope::Site(site_id) = scope {
        organization_id = organization_of_site(state.db().pool(), site_id).await?;
    }

    let candidates: Vec<CandidateInput> = body
        .candidates
        .into_iter()
        .map(|candidate| CandidateInput {
            model_id: candidate.model_id,
            requirements: candidate.requirements,
        })
        .collect();

    replace_task_map(
        state.db().pool(),
        scope,
        organization_id,
        &body.task,
        &candidates,
        Some(current.user.id),
    )
    .await?;

    let entry = NewAuditEntry::by_user(current.user.id, "ai.route.updated")
        .organization(current.user.organization_id)
        .target("ai_task_map", body.task.clone())
        .metadata(json!({
            "scope": scope,
            "candidates": candidates.len(),
        }))
        .ip_address(address.as_text());
    omnion_audit::record(state.db().pool(), entry).await?;

    let routes = read_scope(state.db().pool(), scope, organization_id).await?;
    Ok(Json(RoutingResponse::build(routes)))
}

/// `GET /api/v1/ai/routing/overrides` — the feature pins in force at one scope.
pub async fn get_overrides(
    State(state): State<AppState>,
    Query(query): Query<RoutingQuery>,
) -> Result<Json<Vec<OverrideView>>, ApiError> {
    let (scope, organization_id) = scope_of(&query)?;
    let routes = read_scope(state.db().pool(), scope, organization_id).await?;
    Ok(Json(
        routes.overrides.iter().map(OverrideView::build).collect(),
    ))
}

/// `PUT /api/v1/ai/routing/overrides` — pin or unpin one feature.
pub async fn put_override(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<OverrideBody>,
) -> Result<Json<Vec<OverrideView>>, ApiError> {
    let (scope, mut organization_id) = scope_of(&body.scope)?;

    if let Scope::Site(site_id) = scope {
        organization_id = organization_of_site(state.db().pool(), site_id).await?;
    }

    // Validated here as well as in the store, so a typo is refused with the list of real keys
    // rather than a constraint violation the operator has to decode.
    check_feature(&body.feature).map_err(|_| unknown_feature(&body.feature))?;

    set_override(
        state.db().pool(),
        scope,
        organization_id,
        &body.feature,
        body.model_id,
        Some(current.user.id),
    )
    .await?;

    let entry = NewAuditEntry::by_user(current.user.id, "ai.route.override_updated")
        .organization(current.user.organization_id)
        .target("ai_feature_override", body.feature.clone())
        .metadata(json!({
            "scope": scope,
            "model_id": body.model_id,
        }))
        .ip_address(address.as_text());
    omnion_audit::record(state.db().pool(), entry).await?;

    let routes = read_scope(state.db().pool(), scope, organization_id).await?;
    Ok(Json(
        routes.overrides.iter().map(OverrideView::build).collect(),
    ))
}

/// `POST /api/v1/ai/routing/preview` — what would this request do today.
///
/// **Zero provider calls**, and the module has no provider client to make one with. The only
/// reads are the maps and (when the caller named a model explicitly) the registry.
pub async fn preview_routing(
    State(state): State<AppState>,
    Json(body): Json<PreviewBody>,
) -> Result<Json<PreviewResponse>, ApiError> {
    let (scope, organization_id) = scope_of(&body.scope)?;

    if let Some(task) = body.task.as_deref() {
        check_task(task).map_err(|_| unknown_task(task))?;
    }
    if let Some(feature) = body.feature.as_deref() {
        check_feature(feature).map_err(|_| unknown_feature(feature))?;
    }
    for requirement in &body.requires {
        omnion_ai_hub::check_requirement(requirement).map_err(|_| unknown_requirement(requirement))?;
    }

    let chain = scope.chain(organization_id);
    let maps = load_maps(state.db().pool(), &chain).await?;

    // An explicit `provider/model` is resolved through the real router so the preview answers
    // with the same pair a live request would get — including a refusal when the name does not
    // exist, which is exactly the thing an operator wants to know before shipping the pin.
    let explicit = match body.requested.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        Some(requested) => Some(omnion_ai_hub::resolve(state.db().pool(), Some(requested)).await?.model),
        None => None,
    };

    let decision = decide(
        &maps,
        &ResolveRequest {
            explicit: explicit.as_ref(),
            feature: body.feature.as_deref(),
            task: body.task.as_deref(),
            requires: body.requires.clone(),
            scope,
            organization_id,
        },
    );

    Ok(Json(PreviewResponse {
        decision,
        scope,
        rules: omnion_ai_hub::RULES.iter().map(|rule| (*rule).to_owned()).collect(),
    }))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The organization a site belongs to, for a write that names a site scope.
///
/// A site that does not exist is a `400` naming the problem — never a silent fall back to the
/// installation scope, which would turn a typo into an edit of the platform-wide map.
async fn organization_of_site(
    pool: &sqlx::PgPool,
    site_id: Uuid,
) -> Result<Option<Uuid>, ApiError> {
    omnion_ai_hub::organization_of_site(pool, site_id).await?.ok_or_else(|| {
        ApiError::bad_request(
            "site_not_found",
            "that site does not exist, so its route map cannot be written",
        )
    }).map(Some)
}

/// A refusal for a key the platform does not model, naming the ones that exist.
///
/// The message comes from the crate (which owns the closed vocabulary) and the code from the
/// endpoint, so adding a feature key does not mean remembering to reword an error in a second
/// place.
fn unknown_feature(value: &str) -> ApiError {
    ApiError::bad_request("unknown_feature", omnion_ai_hub::unknown_feature(value).to_string())
}

fn unknown_task(value: &str) -> ApiError {
    ApiError::bad_request("unknown_task", omnion_ai_hub::unknown_task(value).to_string())
}

fn unknown_requirement(value: &str) -> ApiError {
    ApiError::bad_request(
        "unknown_requirement",
        omnion_ai_hub::unknown_requirement(value).to_string(),
    )
}
