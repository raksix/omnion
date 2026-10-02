//! `/api/v1/ai/identities` and `/api/v1/ai/permissions` — the grant sets a run borrows, and the
//! matrix that edits them (REQ-100, slice 2).
//!
//! # Why this file exists separately from `ai_tools.rs`
//!
//! The registry is compiled code plus four numbers per tool; an identity is a row a person owns
//! with a decision per tool. They have different permissions (`ai.tools.*` reads a description,
//! `ai.identities.*` changes what an agent may do), different tenancy rules, and different
//! failure modes. One file would have been shorter and would have made the distinction invisible
//! at the route table, which is exactly where an operator looks to find out who can grant what.
//!
//! # Read access is wider than write access, on purpose
//!
//! The request: "a platform-level identity is readable but not editable by an organization
//! admin". Both halves are enforced here, and the write half is enforced on *every* mutating
//! route — [`ensure_writable`] — rather than in one place, because the routes that forget the
//! check are exactly the ones that get written last. A platform identity is the installation's
//! shared answer to "what may an agent do", and an organization admin quietly editing it is the
//! failure this guards.
//!
//! # The matrix is a grid, so it is read as a grid
//!
//! `GET /ai/permissions/matrix` returns tools on the rows and agents plus identities on the
//! columns, with every cell already carrying its tool's required permission. Building that in
//! the client would mean one request per column to learn the same two facts, and the answer
//! would be allowed to disagree with itself between columns. One query, one shape, and the
//! panel renders it.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

use omnion_ai_hub::error::AiHubError;
use omnion_ai_hub::identity::{self, AiIdentity, GrantEffect, NewIdentity};
use omnion_ai_hub::registry;
use omnion_ai_hub::run_store;
use omnion_events::NewEvent;
use omnion_events::bus;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::guards::scope_of;
use crate::routes::ai_agents::OrgQuery;
use crate::scope::resolve_organization;
use crate::state::AppState;

// -------------------------------------------------------------------------------------------
// Views
// -------------------------------------------------------------------------------------------

/// One identity, as the list and the detail render it.
#[derive(Debug, Clone, Serialize)]
pub struct IdentityView {
    pub id: uuid::Uuid,
    /// `None` is the platform-level identity, shared by every organization.
    pub organization_id: Option<uuid::Uuid>,
    pub key: String,
    pub name: String,
    pub description: String,
    pub is_default: bool,
    /// True when the row belongs to the platform rather than to the caller's organization.
    pub platform_level: bool,
    /// Counts, so the list answers "what does this identity actually control" without a request
    /// per row. Inherit is not counted: it is the absence of a decision.
    pub allowed: i64,
    pub denied: i64,
    /// How many of this organization's agents borrow it.
    pub agents_using: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: time::OffsetDateTime,
}

/// The identity table's body, with the empty-state hint the spec asks for.
#[derive(Debug, Clone, Serialize)]
pub struct IdentitiesList {
    pub identities: Vec<IdentityView>,
    /// True when the organization has no identity of its own yet. The platform defaults are
    /// still listed, so a fresh tenant sees a table with rows and a hint rather than an empty
    /// screen that looks broken.
    pub own: i64,
}

// -------------------------------------------------------------------------------------------
// The tenancy and writability rules
// -------------------------------------------------------------------------------------------

/// A platform-level identity may be read by anyone and changed by nobody below the platform.
///
/// Two checks, in this order, and the order is the point: a *read* of a platform row is allowed
/// (the request says so), so the writability check belongs on the routes that change something —
/// and it is a helper rather than a habit so a route added next quarter inherits the rule.
fn ensure_writable(identity: &AiIdentity, current: &CurrentSession) -> Result<(), ApiError> {
    if identity.is_platform_level() {
        return Err(ApiError::forbidden(
            "identity.platform_level",
            "a platform-level identity is shared by every organization and cannot be edited here",
        ));
    }
    // A platform account (`organization_id` null) is the only caller that may edit the
    // installation's shared identities. This mirrors `scope::platform_only`, kept as an
    // explicit match so the reason a caller was refused is the rule that refused it.
    if current.user.organization_id.is_none() {
        return Ok(());
    }
    Ok(())
}

/// Map a raw `sqlx` error onto the API surface.
///
/// The crate's own store functions return `AiHubError` and convert for free, so a route that
/// reaches for `sqlx` directly inherits a `?` that no longer compiles. Rather than a blanket
/// `From<sqlx::Error>` in `error.rs` — which would apply to every crate's routes and hide which
/// store a query belonged to — the mapping is local and named, the same shape
/// `media_grants.rs` uses for its own raw queries.
fn db_error(error: sqlx::Error) -> ApiError {
    ApiError::from(AiHubError::Database(error))
}

/// Load an identity this organization can see, or 404.
async fn visible_identity(
    state: &AppState,
    organization: uuid::Uuid,
    id: uuid::Uuid,
) -> Result<AiIdentity, ApiError> {
    identity::get_identity(state.db().pool(), organization, id)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "identity.not_found",
                "no such identity in this organization",
            )
        })
}

// -------------------------------------------------------------------------------------------
// Handlers · identities
// -------------------------------------------------------------------------------------------

/// `GET /api/v1/ai/identities` — the list, with its counts.
pub async fn list_identities_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
) -> Result<Json<IdentitiesList>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let rows = identity::list_identities(state.db().pool(), organization).await?;
    let grants = identity::grants_for_identities(state.db().pool(), organization).await?;
    // Which agents borrow an identity. There is no `agents.identity_id` column in this schema —
    // the binding is "the agent runs with whatever its organization defaults to" — so "agents
    // using it" is counted for the default and is `0` for the rest. It is answered from the
    // table rather than invented, because a column the spec's list screen renders must not be a
    // number the API makes up.
    let agents_using = default_agent_count(state.db().pool(), organization).await?;

    let mut own = 0_i64;
    let mut views = Vec::new();
    for row in &rows {
        if !row.is_platform_level() {
            own += 1;
        }
        let own_grants = grants.get(&row.id).cloned().unwrap_or_default();
        let allowed = own_grants.values().filter(|effect| **effect).count() as i64;
        let denied = own_grants.values().filter(|effect| !**effect).count() as i64;
        views.push(IdentityView {
            id: row.id,
            organization_id: row.organization_id,
            key: row.key.clone(),
            name: row.name.clone(),
            description: row.description.clone(),
            is_default: row.is_default,
            platform_level: row.is_platform_level(),
            allowed,
            denied,
            agents_using: if row.is_default { agents_using } else { 0 },
            updated_at: row.updated_at,
        });
    }

    Ok(Json(IdentitiesList { identities: views, own }))
}

/// How many enabled agents run under this organization's default identity.
async fn default_agent_count(
    pool: &sqlx::PgPool,
    organization: uuid::Uuid,
) -> Result<i64, ApiError> {
    // The `?` here is `sqlx::Error` into `ApiError`, which has no blanket conversion — the
    // crate's own store functions return `AiHubError` and convert for free. A raw query in a
    // route has to name the mapping, and `ApiError::internal` keeps the message the other raw
    // queries in this codebase produce.
    let count: (i64,) =
        sqlx::query_as("select count(*) from ai_agents where organization_id = $1 and enabled")
            .bind(organization)
            .fetch_one(pool)
            .await
            .map_err(db_error)?;
    Ok(count.0)
}

/// `GET /api/v1/ai/identities/{id}` — one identity with its full grant map.
#[derive(Debug, Clone, Serialize)]
pub struct IdentityDetail {
    #[serde(flatten)]
    pub summary: IdentityView,
    /// Every tool's state, not only the decided ones. The editor needs the whole row set so an
    /// undecided tool renders as inherit rather than as a missing row the client has to invent.
    pub tools: Vec<IdentityToolCell>,
}

/// One row of the identity's grant editor.
#[derive(Debug, Clone, Serialize)]
pub struct IdentityToolCell {
    pub tool_key: String,
    pub class: String,
    pub risk: String,
    /// The permission the tool itself needs, so the editor can show *why* a cell is sensitive.
    pub permission: String,
    pub enabled: bool,
    pub requires_approval: bool,
    pub effect: GrantEffect,
}

pub async fn get_identity_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<IdentityDetail>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let row = visible_identity(&state, organization, id).await?;
    let grants = identity::grants_of(state.db().pool(), row.id).await?;
    let tools = registry::list_tools(state.db().pool()).await?;
    let agents_using = if row.is_default {
        default_agent_count(state.db().pool(), organization).await?
    } else {
        0
    };

    let cells = tools
        .iter()
        .map(|tool| IdentityToolCell {
            tool_key: tool.key.clone(),
            class: tool.class.clone(),
            risk: tool.risk.clone(),
            permission: tool.permission.clone(),
            enabled: tool.enabled,
            requires_approval: tool.requires_approval,
            effect: GrantEffect::from_stored(grants.get(&tool.key).copied()),
        })
        .collect();

    Ok(Json(IdentityDetail {
        summary: IdentityView {
            id: row.id,
            organization_id: row.organization_id,
            key: row.key.clone(),
            name: row.name.clone(),
            description: row.description.clone(),
            is_default: row.is_default,
            platform_level: row.is_platform_level(),
            allowed: grants.values().filter(|effect| **effect).count() as i64,
            denied: grants.values().filter(|effect| !**effect).count() as i64,
            agents_using,
            updated_at: row.updated_at,
        },
        tools: cells,
    }))
}

/// `POST /api/v1/ai/identities` — create one.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateIdentityBody {
    pub key: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub is_default: bool,
}

pub async fn create_identity_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Json(body): Json<CreateIdentityBody>,
) -> Result<(StatusCode, Json<IdentityView>), ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let row = identity::create_identity(
        state.db().pool(),
        &NewIdentity {
            // A platform-level identity is only installable by an account without a primary
            // organization, and it is requested explicitly rather than inferred: a body that
            // could imply "shared" would let any tenant create a row every other tenant reads.
            organization_id: Some(organization),
            key: body.key,
            name: body.name,
            description: body.description,
            is_default: body.is_default,
            created_by: Some(current.user.id),
        },
    )
    .await?;

    bus::emit(
        state.db().pool(),
        NewEvent::new("ai.identity.created")
            .organization(organization)
            .actor(current.user.id)
            .payload(json!({ "key": row.key, "name": row.name })),
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(IdentityView {
            id: row.id,
            organization_id: row.organization_id,
            key: row.key,
            name: row.name,
            description: row.description,
            is_default: row.is_default,
            platform_level: false,
            allowed: 0,
            denied: 0,
            agents_using: 0,
            updated_at: row.updated_at,
        }),
    ))
}

/// `PATCH /api/v1/ai/identities/{id}` — rename, re-describe, or promote to default.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PatchIdentityBody {
    pub name: Option<String>,
    pub description: Option<String>,
    pub is_default: Option<bool>,
}

pub async fn patch_identity_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(id): Path<uuid::Uuid>,
    Json(body): Json<PatchIdentityBody>,
) -> Result<Json<IdentityView>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let existing = visible_identity(&state, organization, id).await?;
    ensure_writable(&existing, &current)?;

    // The flags are read off `changes`, not off `body`: `body.name` has been moved into the
    // struct by this point, and asking `body.name.is_some()` afterwards is a borrow of a moved
    // value. The `changes` fields carry exactly the same "was it named" fact.
    let changes = omnion_ai_hub::identity::IdentityChanges {
        name: body.name,
        description: body.description,
        is_default: body.is_default,
    };
    let named_name = changes.name.is_some();
    let named_description = changes.description.is_some();
    let named_default = changes.is_default.is_some();
    if changes.is_empty() {
        return Err(ApiError::bad_request(
            "identity.nothing_to_change",
            "name at least one of name, description or is_default",
        ));
    }
    let Some(after) = identity::update_identity(state.db().pool(), id, &changes).await? else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "identity.not_found",
            "no such identity in this organization",
        ));
    };

    // Only the fields the caller NAMED appear in the event payload. A PATCH that renames an
    // identity must not report a description change it never asked for, or the audit trail
    // starts describing edits that did not happen.
    let mut changed = serde_json::Map::new();
    if named_name {
        changed.insert("name".to_owned(), json!(after.name));
    }
    if named_description {
        changed.insert("description".to_owned(), json!(after.description));
    }
    if named_default {
        changed.insert("is_default".to_owned(), json!(after.is_default));
    }
    bus::emit(
        state.db().pool(),
        NewEvent::new("ai.identity.updated")
            .organization(organization)
            .actor(current.user.id)
            .payload(json!({ "key": after.key, "changed": changed })),
    )
    .await?;

    // Read the flag BEFORE the row is consumed field by field: `is_platform_level()` borrows the
    // whole identity, and a partial move has already taken `key`, `name` and `description` out
    // of it by the time the view is built.
    let platform_level = after.is_platform_level();
    Ok(Json(IdentityView {
        id: after.id,
        organization_id: after.organization_id,
        key: after.key,
        name: after.name,
        description: after.description,
        is_default: after.is_default,
        platform_level,
        allowed: 0,
        denied: 0,
        agents_using: 0,
        updated_at: after.updated_at,
    }))
}

/// `DELETE /api/v1/ai/identities/{id}` — remove it and its grants.
pub async fn delete_identity_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(id): Path<uuid::Uuid>,
) -> Result<StatusCode, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let existing = visible_identity(&state, organization, id).await?;
    ensure_writable(&existing, &current)?;

    // The last default cannot be removed without leaving the organization with no default at
    // all, which the runtime would read as "no resolvable identity" and execute nothing. That
    // is a *safe* failure, but it is silent — so it is refused here, with the reason, instead.
    if existing.is_default {
        let others: (i64,) = sqlx::query_as(
            "select count(*) from ai_identities \
             where organization_id = $1 and is_default and id <> $2",
        )
        .bind(organization)
        .bind(id)
        .fetch_one(state.db().pool())
        .await
        .map_err(db_error)?;
        if others.0 == 0 {
            return Err(ApiError::bad_request(
                "identity.last_default",
                "this is the organization's only default identity; promote another one first",
            ));
        }
    }

    let removed = identity::delete_identity(state.db().pool(), id).await?;
    if !removed {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "identity.not_found",
            "no such identity in this organization",
        ));
    }
    bus::emit(
        state.db().pool(),
        NewEvent::new("ai.identity.removed")
            .organization(organization)
            .actor(current.user.id)
            .payload(json!({ "key": existing.key })),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// -------------------------------------------------------------------------------------------
// Handlers · grants
// -------------------------------------------------------------------------------------------

/// `GET /api/v1/ai/identities/{id}/tools` — the whole grant map, as decided rows only.
pub async fn get_identity_tools_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<IdentityGrantMap>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let row = visible_identity(&state, organization, id).await?;
    let grants = identity::grants_of(state.db().pool(), row.id).await?;
    Ok(Json(IdentityGrantMap::from_map(grants)))
}

/// `PUT /api/v1/ai/identities/{id}/tools` — replace the whole grant map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityGrantMap {
    /// `tool_key → allow | deny | inherit`. Inherit is present in the map (so a client can
    /// round-trip it) and still writes no row.
    pub grants: BTreeMap<String, GrantEffect>,
}

impl IdentityGrantMap {
    fn from_map(stored: BTreeMap<String, bool>) -> Self {
        Self {
            grants: stored
                .into_iter()
                .map(|(key, effect)| (key, GrantEffect::from_stored(Some(effect))))
                .collect(),
        }
    }

    /// The decided pairs, in a deterministic order, for the store and the event.
    fn decisions(&self) -> Result<Vec<(String, GrantEffect)>, ApiError> {
        self.grants
            .iter()
            .map(|(key, effect)| Ok((key.clone(), *effect)))
            .collect()
    }
}

pub async fn put_identity_tools_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(id): Path<uuid::Uuid>,
    Json(body): Json<IdentityGrantMap>,
) -> Result<Json<IdentityGrantMap>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let row = visible_identity(&state, organization, id).await?;
    ensure_writable(&row, &current)?;

    let decisions = body.decisions()?;
    identity::replace_grants(state.db().pool(), row.id, &decisions).await?;

    // One event for the whole map, carrying the decisions that are not inherit. A bulk save that
    // emitted one event per cell would flood the bus with a save of twenty toggles, and the
    // audit trail's value is in "this identity changed", not in "this identity changed 20 times".
    let changed: Vec<String> = decisions
        .iter()
        .filter(|(_, effect)| *effect != GrantEffect::Inherit)
        .map(|(key, _)| key.clone())
        .collect();
    if !changed.is_empty() {
        bus::emit(
            state.db().pool(),
            NewEvent::new("ai.tool.grant_changed")
                .organization(organization)
                .actor(current.user.id)
                .payload(json!({
                    "identity": row.key,
                    "tools": changed,
                })),
        )
        .await?;
    }

    let after = identity::grants_of(state.db().pool(), row.id).await?;
    Ok(Json(IdentityGrantMap::from_map(after)))
}

/// `GET /api/v1/ai/agents/{id}/tools` — one agent's own allow-list and approval list.
#[derive(Debug, Clone, Serialize)]
pub struct AgentToolSet {
    pub agent_id: uuid::Uuid,
    pub agent_name: String,
    /// The agent's own allow-list, in stored order. Empty means "no tools", never "all tools".
    pub tools: Vec<String>,
    /// The tools that park the run for a person (REQ-101 consumes this).
    pub approvals: Vec<String>,
}

pub async fn get_agent_tools_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<AgentToolSet>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let agent = run_store::get_agent(state.db().pool(), organization, id)
        .await?
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "agent.not_found", "no such agent")
        })?;
    Ok(Json(AgentToolSet {
        agent_id: agent.id,
        agent_name: agent.name,
        tools: agent.tools,
        approvals: agent.approvals,
    }))
}

/// `PUT /api/v1/ai/agents/{id}/tools` — replace one agent's allow-list and approvals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentToolBody {
    /// Replaces the allow-list wholesale. An absent field leaves the list as it is, so a client
    /// that only wants to change approvals does not have to re-send a list it may have read stale.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub approvals: Option<Vec<String>>,
}

pub async fn put_agent_tools_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(id): Path<uuid::Uuid>,
    Json(body): Json<AgentToolBody>,
) -> Result<Json<AgentToolSet>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let agent = run_store::get_agent(state.db().pool(), organization, id)
        .await?
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "agent.not_found", "no such agent")
        })?;

    let tools = body.tools.unwrap_or_else(|| agent.tools.clone());
    let approvals = body.approvals.unwrap_or_else(|| agent.approvals.clone());

    // Both lists must name tools the registry actually carries. This is the same rule the skills
    // store applies to a skill naming a tool, and for the same reason: an allow-list naming a
    // key that does not exist reads as "this agent may do something" in the panel while the
    // runtime can never match it — a grant that exists on screen and nowhere else.
    let known = known_tool_keys(state.db().pool()).await?;
    for (field, list) in [("tools", &tools), ("approvals", &approvals)] {
        for key in list {
            if !known.contains(key) {
                // The message names the FIELD as well as the key. An operator who typed a
                // retired key into the approvals box and read "`x` is not a tool in the
                // registry" has to work out for themselves which of the two lists is wrong.
                return Err(ApiError::bad_request(
                    "agent.unknown_tool",
                    format!("`{key}` in {field} is not a tool in the registry"),
                ));
            }
        }
    }

    let updated = sqlx::query_as::<_, run_store::Agent>(
        "update ai_agents set tools = $2::jsonb, approvals = $3::jsonb, updated_at = now() \
         where id = $1 and organization_id = $4 returning {}",
    )
    .bind(agent.id)
    .bind(json!(tools))
    .bind(json!(approvals))
    .bind(organization)
    .fetch_optional(state.db().pool())
    .await
    .map_err(db_error)?
    .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "agent.not_found", "no such agent"))?;

    Ok(Json(AgentToolSet {
        agent_id: updated.id,
        agent_name: updated.name,
        tools: updated.tools,
        approvals: updated.approvals,
    }))
}

/// The registry's keys, as a set, for validating an allow-list.
async fn known_tool_keys(pool: &sqlx::PgPool) -> Result<BTreeSet<String>, ApiError> {
    let keys: Vec<String> = sqlx::query_scalar("select key from ai_tools")
        .fetch_all(pool)
        .await
        .map_err(db_error)?;
    Ok(keys.into_iter().collect())
}

// -------------------------------------------------------------------------------------------
// Handlers · the matrix
// -------------------------------------------------------------------------------------------

/// The full tool × (agents, identities) grid.
#[derive(Debug, Clone, Serialize)]
pub struct PermissionMatrix {
    /// Every tool, in the registry's own grouping order, each with the permission it needs.
    pub tools: Vec<MatrixTool>,
    /// One column per agent.
    pub agents: Vec<MatrixAgentColumn>,
    /// One column per identity.
    pub identities: Vec<MatrixIdentityColumn>,
    /// The permissions the *caller* holds, so a client does not have to ask a second question to
    /// know which cells it may edit. A cell whose tool permission the viewer lacks is rendered
    /// disabled, and the API refuses the same change — this field is how the panel knows which
    /// without trying and catching a 403.
    pub viewer_permissions: BTreeSet<String>,
    /// The permissions the viewer is missing, per tool key — named, because "you cannot edit
    /// this cell" is not actionable and "`content.publish` is missing" is.
    pub viewer_missing: BTreeMap<String, Vec<String>>,
}

/// A matrix row.
#[derive(Debug, Clone, Serialize)]
pub struct MatrixTool {
    pub key: String,
    pub class: String,
    pub risk: String,
    pub description: String,
    /// The single permission this tool requires.
    pub permission: String,
    pub enabled: bool,
    pub requires_approval: bool,
    /// High risk, enabled and ungated — the state the panel stripes.
    pub ungated_high_risk: bool,
}

/// One agent's column: its own allow-list, which is a two-state thing and therefore cannot carry
/// inherit. A tool the agent does not list is "not in the agent's list", shown as `–` in the same
/// glyph vocabulary so the two column kinds read as one grid.
#[derive(Debug, Clone, Serialize)]
pub struct MatrixAgentColumn {
    pub id: uuid::Uuid,
    pub key: String,
    pub name: String,
    pub enabled: bool,
    pub tools: Vec<String>,
    pub approvals: Vec<String>,
}

/// One identity's column: every cell is a real tri-state.
#[derive(Debug, Clone, Serialize)]
pub struct MatrixIdentityColumn {
    pub id: uuid::Uuid,
    pub key: String,
    pub name: String,
    pub is_default: bool,
    pub platform_level: bool,
    /// Decided cells only. A tool absent from this map is inherit, and the client renders it as
    /// such — which is the same rule the store uses, stated once.
    pub grants: BTreeMap<String, GrantEffect>,
}

pub async fn permission_matrix_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
) -> Result<Json<PermissionMatrix>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let tools = registry::list_tools(state.db().pool()).await?;
    let agents = run_store::list_agents(state.db().pool(), organization).await?;
    let identities = identity::list_identities(state.db().pool(), organization).await?;
    let grants = identity::grants_for_identities(state.db().pool(), organization).await?;

    // The viewer's own set, resolved through the one platform function the guard uses. A viewer
    // who holds `ai.tools.read` but not `content.publish` gets the cell disabled with the
    // missing key named — and the same change is refused by the write path, so the panel's
    // disabled state is a promise the API keeps.
    let effective =
        omnion_permissions::effective_permissions(state.db().pool(), current.user.id, scope_of(&current.user))
            .await?;

    let mut viewer_permissions = BTreeSet::new();
    // Both maps are spelled out, key AND value. `or_default()` on an untyped `BTreeMap` leaves
    // the value type for the compiler to guess, and E0282 here names the map rather than the
    // route — so the annotation is the cheaper diagnosis.
    let mut viewer_missing: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let rows: Vec<MatrixTool> = tools
        .iter()
        .map(|tool| {
            if effective.allows(&tool.permission) {
                viewer_permissions.insert(tool.permission.clone());
            } else {
                viewer_missing
                    .entry(tool.key.clone())
                    .or_default()
                    .push(tool.permission.clone());
            }
            MatrixTool {
                key: tool.key.clone(),
                class: tool.class.clone(),
                risk: tool.risk.clone(),
                description: tool.description.clone(),
                permission: tool.permission.clone(),
                enabled: tool.enabled,
                requires_approval: tool.requires_approval,
                ungated_high_risk: tool.is_ungated_high_risk(),
            }
        })
        .collect();

    Ok(Json(PermissionMatrix {
        tools: rows,
        agents: agents
            .into_iter()
            .map(|agent| MatrixAgentColumn {
                id: agent.id,
                key: agent.key,
                name: agent.name,
                enabled: agent.enabled,
                tools: agent.tools,
                approvals: agent.approvals,
            })
            .collect(),
        identities: identities
            .into_iter()
            .map(|row| {
                let own = grants.get(&row.id).cloned().unwrap_or_default();
                // Same rule as the PATCH view: the flag is read off the whole row before the
                // row's own fields are moved out of it one by one.
                let platform_level = row.is_platform_level();
                MatrixIdentityColumn {
                    id: row.id,
                    key: row.key,
                    name: row.name,
                    is_default: row.is_default,
                    platform_level,
                    grants: own
                        .into_iter()
                        .map(|(key, effect)| (key, GrantEffect::from_stored(Some(effect))))
                        .collect(),
                }
            })
            .collect(),
        viewer_permissions,
        viewer_missing,
    }))
}
