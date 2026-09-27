//! `/api/v1/iam` — roles, role assignments, effective permissions and the audit trail.
//!
//! v0 of the IAM surface (docs/07-IAM.md): the permission catalogue, fully custom roles with
//! explicit allow/deny entries, scoped role bindings, the effective permission set of an
//! account (the seed of the permission simulator, docs/07-IAM.md §18) and read access to the
//! audit trail. Every privileged action records an audit row in the same request.
//!
//! Cross-tenant rule for this v0 surface: an account with a primary organization may only act
//! inside that organization; platform-level accounts (no primary organization) may act on any
//! organization. The guard layer already established that the caller holds the route's
//! permission in their own scope.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_permissions::model::{
    Effect, NewRole, NewSubjectBinding, ParentChange, PermissionSummary, Role, RolePermission,
    RolePermissionInput, RoleUpdate, Subject,
};
use omnion_permissions::{PermissionsError, Scope};
use omnion_permissions::{bindings, catalogue, evaluate, roles as role_store, versions};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::guards::scope_of;
use crate::scope::{ensure_same_organization, resolve_organization};
use crate::state::AppState;

/// Hierarchy position a role gets when the request does not pick one: between the Editor (300)
/// and Moderator (500) base roles.
const DEFAULT_ROLE_PRIORITY: i32 = 400;

/// Audit rows returned when the request does not ask for a size.
const DEFAULT_AUDIT_LIMIT: i64 = 50;

/// Upper bound for the audit page size.
const MAX_AUDIT_LIMIT: i64 = 200;

// ---------------------------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------------------------

/// One catalogue entry.
#[derive(Debug, Serialize)]
pub struct PermissionBody {
    /// Stable key.
    pub key: String,
    /// UI grouping.
    pub category: String,
    /// What the permission allows.
    pub description: String,
}

/// Response body of `GET /api/v1/iam/permissions`.
#[derive(Debug, Serialize)]
pub struct PermissionsResponse {
    /// The catalogue, in catalogue order.
    pub permissions: Vec<PermissionBody>,
}

/// Public representation of a role.
#[derive(Debug, Serialize)]
pub struct RoleBody {
    /// Role id.
    pub id: Uuid,
    /// Owning organization (`null` = platform role).
    pub organization_id: Option<Uuid>,
    /// Stable key.
    pub key: String,
    /// Display name.
    pub name: String,
    /// What the role is for.
    pub description: String,
    /// Hierarchy position.
    pub priority: i32,
    /// Parent role, when the role inherits.
    pub inherits_role_id: Option<Uuid>,
    /// Whether inherited permissions apply.
    pub inherit_permissions: bool,
    /// Platform-managed role.
    pub is_system: bool,
    /// Explicit allows in the role's own set.
    pub allowed_permissions: i64,
    /// Explicit denies in the role's own set.
    pub denied_permissions: i64,
    /// Creation timestamp, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl RoleBody {
    fn new(role: &Role, summary: PermissionSummary) -> Self {
        Self {
            id: role.id,
            organization_id: role.organization_id,
            key: role.key.clone(),
            name: role.name.clone(),
            description: role.description.clone(),
            priority: role.priority,
            inherits_role_id: role.inherits_role_id,
            inherit_permissions: role.inherit_permissions,
            is_system: role.is_system,
            allowed_permissions: summary.allowed,
            denied_permissions: summary.denied,
            created_at: role.created_at,
        }
    }
}

/// Response body of `GET /api/v1/iam/roles` and the role mutations.
#[derive(Debug, Serialize)]
pub struct RolesResponse {
    /// Roles visible to the caller: platform roles plus the organization's own.
    pub roles: Vec<RoleBody>,
}

/// Where a role applies, as returned to clients.
#[derive(Debug, Clone, Serialize)]
pub struct ScopeBody {
    /// `global`, `organization`, `site`, `department`, `module` or `resource`.
    #[serde(rename = "type")]
    pub scope_type: &'static str,
    /// Organization of the scope, when it has one.
    pub organization_id: Option<Uuid>,
    /// Site of the scope, when it is site scoped.
    pub site_id: Option<Uuid>,
    /// Kind of resource (`path`), when the scope names one.
    pub resource_type: Option<String>,
    /// The resource pattern, department key or module key, when the scope names one.
    pub resource_id: Option<String>,
}

impl From<Scope> for ScopeBody {
    fn from(scope: Scope) -> Self {
        Self {
            scope_type: scope.scope_type(),
            organization_id: scope.organization_id(),
            site_id: scope.site_id(),
            resource_type: scope.resource_type().map(str::to_owned),
            resource_id: scope.resource_id().map(str::to_owned),
        }
    }
}

/// Public representation of a role assignment.
#[derive(Debug, Serialize)]
pub struct BindingBody {
    /// Binding id.
    pub id: Uuid,
    /// Role that is assigned.
    pub role_id: Uuid,
    /// Who the role is bound to: `user`, `group` or `service_account`.
    pub subject_type: String,
    /// Id of the subject.
    pub subject_id: Uuid,
    /// Account that holds the role, when the subject is a person.
    pub user_id: Option<Uuid>,
    /// Where the role applies.
    pub scope: ScopeBody,
    /// Who granted it (`null` = the platform).
    pub granted_by: Option<Uuid>,
    /// When it stops applying (temporary roles).
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    /// When it was revoked.
    #[serde(with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<OffsetDateTime>,
    /// Whether the binding applies right now.
    pub active: bool,
    /// Whether the binding carried a window that has run out (shown as expired, not deleted).
    pub expired: bool,
    /// Creation timestamp, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<&omnion_permissions::RoleBinding> for BindingBody {
    fn from(binding: &omnion_permissions::RoleBinding) -> Self {
        let now = OffsetDateTime::now_utc();
        Self {
            id: binding.id,
            role_id: binding.role_id,
            subject_type: binding.subject.subject_type().to_owned(),
            subject_id: binding.subject.id(),
            user_id: binding.user_id,
            scope: binding.scope.clone().into(),
            granted_by: binding.granted_by,
            expires_at: binding.expires_at,
            revoked_at: binding.revoked_at,
            active: binding.is_active_at(now),
            expired: binding.is_expired_at(now),
            created_at: binding.created_at,
        }
    }
}

/// Response body of the binding endpoints.
#[derive(Debug, Serialize)]
pub struct BindingsResponse {
    /// Who the bindings speak about, when the request named a subject.
    pub subject_type: Option<String>,
    /// Id of the subject, when the request named one.
    pub subject_id: Option<Uuid>,
    /// Role assignments, newest first.
    pub bindings: Vec<BindingBody>,
}

/// Role reference in an effective-permission entry.
#[derive(Debug, Serialize)]
pub struct GrantSourceBody {
    /// Role id.
    pub role_id: Uuid,
    /// Role key.
    pub role_key: String,
    /// Role name.
    pub role_name: String,
    /// Role priority.
    pub role_priority: i32,
    /// How the permission reached the account: `explicit_allow`, `inherited_allow`,
    /// `explicit_deny` or `inherited_deny`.
    pub via: &'static str,
}

impl From<&evaluate::Grant> for GrantSourceBody {
    fn from(grant: &evaluate::Grant) -> Self {
        Self {
            role_id: grant.role_id,
            role_key: grant.role_key.clone(),
            role_name: grant.role_name.clone(),
            role_priority: grant.role_priority,
            via: via_name(grant.via),
        }
    }
}

/// One permission of the effective set.
#[derive(Debug, Serialize)]
pub struct EffectiveEntryBody {
    /// Permission key.
    pub key: String,
    /// Role that decided it.
    pub source: GrantSourceBody,
}

/// Response body of `GET /api/v1/iam/effective-permissions`.
#[derive(Debug, Serialize)]
pub struct EffectivePermissionsResponse {
    /// Account the set belongs to.
    pub user_id: Uuid,
    /// Scope the set was resolved in.
    pub scope: ScopeBody,
    /// Permissions the account holds.
    pub granted: Vec<EffectiveEntryBody>,
    /// Permissions an explicit deny removes.
    pub denied: Vec<EffectiveEntryBody>,
    /// How many permissions are granted (after denies).
    pub granted_count: usize,
}

/// One audit row.
#[derive(Debug, Serialize)]
pub struct AuditBody {
    /// Row id.
    pub id: i64,
    /// Action name.
    pub action: String,
    /// `user`, `agent`, `service` or `system`.
    pub actor_type: String,
    /// Account that acted, when a person did.
    pub actor_user_id: Option<Uuid>,
    /// Organization the action belongs to.
    pub organization_id: Option<Uuid>,
    /// Kind of the target.
    pub target_type: Option<String>,
    /// Identifier of the target.
    pub target_id: Option<String>,
    /// Structured detail.
    pub metadata: serde_json::Value,
    /// Peer address, when known.
    pub ip_address: Option<String>,
    /// When it was recorded, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<omnion_audit::AuditEntry> for AuditBody {
    fn from(entry: omnion_audit::AuditEntry) -> Self {
        Self {
            id: entry.id,
            action: entry.action,
            actor_type: entry.actor_type,
            actor_user_id: entry.actor_user_id,
            organization_id: entry.organization_id,
            target_type: entry.target_type,
            target_id: entry.target_id,
            metadata: entry.metadata,
            ip_address: entry.ip_address,
            created_at: entry.created_at,
        }
    }
}

/// Response body of `GET /api/v1/iam/audit`.
#[derive(Debug, Serialize)]
pub struct AuditResponse {
    /// Entries, newest first.
    pub entries: Vec<AuditBody>,
}

// ---------------------------------------------------------------------------------------------
// Request shapes
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/iam/roles`.
#[derive(Debug, Deserialize)]
pub struct CreateRoleRequest {
    /// Stable key (`marketing-manager`).
    pub key: String,
    /// Display name.
    pub name: String,
    /// What the role is for.
    #[serde(default)]
    pub description: String,
    /// Hierarchy position; defaults to 400.
    #[serde(default)]
    pub priority: Option<i32>,
    /// Parent role to inherit from.
    #[serde(default)]
    pub inherits_role_id: Option<Uuid>,
    /// Organization to create the role in; defaults to the caller's own.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// One allow/deny entry in a role update.
#[derive(Debug, Deserialize)]
pub struct PermissionEntryBody {
    /// Permission key from the catalogue.
    pub key: String,
    /// `allow` or `deny`.
    pub effect: EffectBody,
}

/// Allow or deny, as it appears in JSON.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffectBody {
    /// Explicit allow.
    Allow,
    /// Explicit deny.
    Deny,
}

impl From<EffectBody> for Effect {
    fn from(effect: EffectBody) -> Self {
        match effect {
            EffectBody::Allow => Self::Allow,
            EffectBody::Deny => Self::Deny,
        }
    }
}

/// `PUT /api/v1/iam/roles/{id}/permissions`.
#[derive(Debug, Deserialize)]
pub struct SetRolePermissionsRequest {
    /// The complete set the role should end up with.
    pub permissions: Vec<PermissionEntryBody>,
    /// The version the caller read the role at; a mismatch refuses the save (`409`).
    #[serde(default)]
    pub expected_version: Option<i32>,
}

/// `POST /api/v1/iam/bindings`.
#[derive(Debug, Deserialize)]
pub struct CreateBindingRequest {
    /// Account that receives the role (the pre-subject shape; `subject_type` defaults to `user`).
    #[serde(default)]
    pub user_id: Option<Uuid>,
    /// Who receives the role: `user`, `group` or `service_account`.
    #[serde(default)]
    pub subject_type: Option<SubjectTypeBody>,
    /// Id of the subject; defaults to `user_id`.
    #[serde(default)]
    pub subject_id: Option<Uuid>,
    /// Role to assign.
    pub role_id: Uuid,
    /// `global`, `organization`, `site`, `department`, `module` or `resource`.
    pub scope_type: ScopeTypeBody,
    /// Organization of the scope.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// Site of the scope.
    #[serde(default)]
    pub site_id: Option<Uuid>,
    /// Department key, for a department scope.
    #[serde(default)]
    pub department: Option<String>,
    /// Module key, for a module scope.
    #[serde(default)]
    pub module: Option<String>,
    /// Kind of resource (`path`), for a resource scope.
    #[serde(default)]
    pub resource_type: Option<String>,
    /// The resource pattern, for a resource scope.
    #[serde(default)]
    pub resource_id: Option<String>,
    /// Optional expiry as an RFC 3339 timestamp (temporary roles).
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// Who a binding attaches to, as it appears in JSON.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectTypeBody {
    /// A person.
    User,
    /// A group (team).
    Group,
    /// A machine identity.
    ServiceAccount,
}

impl SubjectTypeBody {
    /// Value the API answers with.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Group => "group",
            Self::ServiceAccount => "service_account",
        }
    }
}

/// Scope selector, as it appears in JSON.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeTypeBody {
    /// Platform level.
    Global,
    /// An organization.
    Organization,
    /// One site.
    Site,
    /// One department.
    Department,
    /// One module.
    Module,
    /// One resource (a path glob).
    Resource,
}

/// Query parameters of `GET /api/v1/iam/bindings`.
#[derive(Debug, Deserialize)]
pub struct BindingsQuery {
    /// Account to list; defaults to the caller.
    #[serde(default)]
    pub user_id: Option<Uuid>,
    /// `user`, `group` or `service_account`.
    #[serde(default)]
    pub subject_type: Option<String>,
    /// Id of the subject to list.
    #[serde(default)]
    pub subject_id: Option<Uuid>,
    /// Only bindings carrying this role.
    #[serde(default)]
    pub role_id: Option<Uuid>,
    /// Only bindings that apply right now.
    #[serde(default)]
    pub live: Option<bool>,
    /// Organization filter (platform bindings are always included).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// Page size, 1..=500 (default 200).
    #[serde(default)]
    pub limit: Option<i64>,
}

/// Query parameters of `GET /api/v1/iam/effective-permissions`.
#[derive(Debug, Deserialize)]
pub struct EffectivePermissionsQuery {
    /// Account to resolve; defaults to the caller.
    #[serde(default)]
    pub user_id: Option<Uuid>,
    /// Scope override.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// Site scope override.
    #[serde(default)]
    pub site_id: Option<Uuid>,
    /// Resource path the resolution should take into account (`/blog/hello`).
    #[serde(default)]
    pub path: Option<String>,
}

/// Query parameters of `GET /api/v1/iam/audit`.
#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    /// Page size, 1..=200 (default 50).
    #[serde(default)]
    pub limit: Option<i64>,
    /// Organization filter.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// Return the permission catalogue.
pub async fn list_permissions() -> Json<PermissionsResponse> {
    let permissions = catalogue::CATALOGUE
        .iter()
        .map(|entry| PermissionBody {
            key: entry.key.to_owned(),
            category: entry.category.to_owned(),
            description: entry.description.to_owned(),
        })
        .collect();

    Json(PermissionsResponse { permissions })
}

/// Query parameters of `GET /api/v1/iam/roles`.
#[derive(Debug, Deserialize)]
pub struct RolesQuery {
    /// Tenant whose roles to list; a platform account must name one to see it (a tenant account
    /// is always answered for its own organization).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// List the roles visible to the caller.
///
/// Platform roles always answer. A tenant account sees its own organization's roles; a platform
/// account names the tenant it wants with `?organization_id=`, so a role created for a tenant is
/// not invisible to the account that created it.
pub async fn list_roles(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<RolesQuery>,
) -> Result<Json<RolesResponse>, ApiError> {
    let pool = state.db().pool();
    let organization_id = match current.user.organization_id {
        Some(own) => {
            if let Some(requested) = query.organization_id {
                if requested != own {
                    return Err(crate::scope::cross_organization());
                }
            }
            Some(own)
        }
        None => query.organization_id,
    };

    let roles = role_store::list_roles(pool, organization_id).await?;
    let ids: Vec<Uuid> = roles.iter().map(|role| role.id).collect();
    let summaries = role_store::permission_summary(pool, &ids).await?;

    Ok(Json(RolesResponse {
        roles: roles
            .iter()
            .map(|role| RoleBody::new(role, summaries.get(&role.id).copied().unwrap_or_default()))
            .collect(),
    }))
}

/// Create a custom role.
pub async fn create_role(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<CreateRoleRequest>,
) -> Result<(StatusCode, Json<RoleBody>), ApiError> {
    let organization_id = resolve_organization(&current, body.organization_id)?;
    let pool = state.db().pool();

    let role = role_store::create_role(
        pool,
        NewRole {
            organization_id,
            key: body.key,
            name: body.name,
            description: body.description,
            priority: body.priority.unwrap_or(DEFAULT_ROLE_PRIORITY),
            inherits_role_id: body.inherits_role_id,
        },
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.role.created")
            .organization(organization_id)
            .target("role", role.id.to_string())
            .metadata(json!({ "key": role.key, "priority": role.priority }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(RoleBody::new(&role, PermissionSummary::default())),
    ))
}

// ---------------------------------------------------------------------------------------------
// Role depth (REQ-006, slice 1): detail, edit, delete, duplicate, preview and version history
// ---------------------------------------------------------------------------------------------

/// One permission entry as the API returns it.
#[derive(Debug, Serialize)]
pub struct PermissionEntryView {
    /// Permission key.
    pub key: String,
    /// `allow` or `deny`.
    pub effect: &'static str,
}

impl From<&RolePermission> for PermissionEntryView {
    fn from(entry: &RolePermission) -> Self {
        Self {
            key: entry.key.clone(),
            effect: entry.effect.as_str(),
        }
    }
}

/// One entry whose effect flipped between two versions.
#[derive(Debug, Serialize)]
pub struct ChangedEntryView {
    /// Permission key.
    pub key: String,
    /// Effect before.
    pub from: &'static str,
    /// Effect after.
    pub to: &'static str,
}

/// What a save (or a preview) changes about a role's permission set.
#[derive(Debug, Serialize)]
pub struct RoleDiffView {
    /// Keys the new set adds.
    pub added: Vec<PermissionEntryView>,
    /// Keys whose effect flips.
    pub changed: Vec<ChangedEntryView>,
    /// Keys the new set drops.
    pub removed: Vec<PermissionEntryView>,
}

impl From<&omnion_permissions::RoleDiff> for RoleDiffView {
    fn from(diff: &omnion_permissions::RoleDiff) -> Self {
        Self {
            added: diff.added.iter().map(PermissionEntryView::from).collect(),
            changed: diff
                .changed
                .iter()
                .map(|change| ChangedEntryView {
                    key: change.key.clone(),
                    from: change.from.map(Effect::as_str).unwrap_or("inherit"),
                    to: change.to.map(Effect::as_str).unwrap_or("inherit"),
                })
                .collect(),
            removed: diff.removed.iter().map(PermissionEntryView::from).collect(),
        }
    }
}

/// A role as another role's chain or member list refers to it.
#[derive(Debug, Serialize)]
pub struct RoleRefView {
    /// Role id.
    pub id: Uuid,
    /// Stable key.
    pub key: String,
    /// Display name.
    pub name: String,
    /// Hierarchy position.
    pub priority: i32,
    /// Platform-managed role.
    pub is_system: bool,
}

impl From<&Role> for RoleRefView {
    fn from(role: &Role) -> Self {
        Self {
            id: role.id,
            key: role.key.clone(),
            name: role.name.clone(),
            priority: role.priority,
            is_system: role.is_system,
        }
    }
}

/// Response body of `GET /api/v1/iam/roles/{id}`.
#[derive(Debug, Serialize)]
pub struct RoleDetailResponse {
    /// The role itself, with its allow/deny counts.
    pub role: RoleBody,
    /// The role's own permission entries (not the inherited ones).
    pub permissions: Vec<PermissionEntryView>,
    /// Roles this one inherits from, nearest first.
    pub chain: Vec<RoleRefView>,
    /// Roles that inherit from this one directly.
    pub inherited_by: Vec<RoleRefView>,
    /// Live bindings carrying this role.
    pub member_count: i64,
    /// Latest version number (`0` when the role has no history yet).
    pub version: i32,
}

/// One version of a role, with the diff against the version before it.
#[derive(Debug, Serialize)]
pub struct RoleVersionView {
    /// One-based version number.
    pub version: i32,
    /// Name at this version.
    pub name: String,
    /// Description at this version.
    pub description: String,
    /// Priority at this version.
    pub priority: i32,
    /// What kind of change produced it.
    pub change: String,
    /// Who made the change.
    pub changed_by: Option<Uuid>,
    /// When it was written, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// The permission set at this version.
    pub permissions: Vec<PermissionEntryView>,
    /// What changed against the previous version.
    pub diff: RoleDiffView,
    /// How many entries the diff touches.
    pub diff_total: usize,
}

/// Response body of `GET /api/v1/iam/roles/{id}/versions`.
#[derive(Debug, Serialize)]
pub struct RoleVersionsResponse {
    /// The role the history belongs to.
    pub role_id: Uuid,
    /// Versions, newest first.
    pub versions: Vec<RoleVersionView>,
}

/// Response body of the permission save.
#[derive(Debug, Serialize)]
pub struct RoleSaveResponse {
    /// The role after the save.
    pub role: RoleBody,
    /// The set as written.
    pub permissions: Vec<PermissionEntryView>,
    /// What the save changed.
    pub diff: RoleDiffView,
    /// The version number the save wrote.
    pub version: i32,
}

/// Response body of the preview (`POST /api/v1/iam/roles/{id}/preview`).
#[derive(Debug, Serialize)]
pub struct RolePreviewResponse {
    /// What saving this set would change.
    pub diff: RoleDiffView,
    /// Why the set would be refused, in caller-facing language (empty when it is acceptable).
    pub problems: Vec<String>,
    /// The version the preview was computed against.
    pub version: i32,
    /// `true` when the set equals what the role already holds.
    pub unchanged: bool,
}

/// `PATCH /api/v1/iam/roles/{id}`.
#[derive(Debug, Deserialize)]
pub struct UpdateRoleRequest {
    /// New display name.
    #[serde(default)]
    pub name: Option<String>,
    /// New description.
    #[serde(default)]
    pub description: Option<String>,
    /// New hierarchy position.
    #[serde(default)]
    pub priority: Option<i32>,
    /// Whether inherited permissions apply.
    #[serde(default)]
    pub inherit_permissions: Option<bool>,
    /// New parent role.
    #[serde(default)]
    pub inherits_role_id: Option<Uuid>,
    /// Set to `true` to detach the role from its parent (takes precedence over
    /// `inherits_role_id`).
    #[serde(default)]
    pub detach_parent: Option<bool>,
}

/// `POST /api/v1/iam/roles/{id}/duplicate`.
#[derive(Debug, Deserialize)]
pub struct DuplicateRoleRequest {
    /// Key of the copy.
    pub key: String,
    /// Display name of the copy.
    pub name: String,
    /// Organization the copy belongs to; defaults to the caller's own.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// Read one role's detail.
pub async fn get_role(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(role_id): Path<Uuid>,
) -> Result<Json<RoleDetailResponse>, ApiError> {
    let pool = state.db().pool();
    let role = role_store::find_role(pool, role_id)
        .await?
        .ok_or(PermissionsError::RoleNotFound)?;
    ensure_same_organization(&current, role.organization_id)?;

    let permissions = role_store::permission_entries(pool, &[role_id])
        .await?
        .remove(&role_id)
        .unwrap_or_default();
    let summary = role_store::permission_summary(pool, &[role_id]).await?;
    let chain = role_store::ancestors(pool, role_id).await?;
    let inherited_by = role_store::children(pool, role_id).await?;
    let member_count = bindings::count_live_for_role(pool, role_id).await?;
    let version = versions::latest_version(pool, role_id).await?.unwrap_or(0);

    Ok(Json(RoleDetailResponse {
        role: RoleBody::new(&role, summary.get(&role_id).copied().unwrap_or_default()),
        permissions: permissions.iter().map(PermissionEntryView::from).collect(),
        chain: chain.iter().map(RoleRefView::from).collect(),
        inherited_by: inherited_by.iter().map(RoleRefView::from).collect(),
        member_count,
        version,
    }))
}

/// Update a custom role's fields and its parent link.
pub async fn update_role(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(role_id): Path<Uuid>,
    address: ClientAddress,
    Json(body): Json<UpdateRoleRequest>,
) -> Result<Json<RoleBody>, ApiError> {
    let pool = state.db().pool();
    let role = role_store::find_role(pool, role_id)
        .await?
        .ok_or(PermissionsError::RoleNotFound)?;
    ensure_same_organization(&current, role.organization_id)?;

    let parent = if body.detach_parent == Some(true) {
        ParentChange::Clear
    } else if let Some(parent_id) = body.inherits_role_id {
        ParentChange::Set(parent_id)
    } else {
        ParentChange::Keep
    };

    let updated = role_store::update_role(
        pool,
        role_id,
        RoleUpdate {
            name: body.name,
            description: body.description,
            priority: body.priority,
            parent,
            inherit_permissions: body.inherit_permissions,
        },
    )
    .await?;

    // A field change is a version too, so the history tab covers the whole role and not only
    // its permission set.
    let entries = role_store::permission_entries(pool, &[role_id])
        .await?
        .remove(&role_id)
        .unwrap_or_default();
    versions::record(pool, &updated, &entries, "updated", Some(current.user.id)).await?;

    let summary = role_store::permission_summary(pool, &[role_id]).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.role.updated")
            .target("role", role_id.to_string())
            .metadata(json!({
                "key": updated.key,
                "priority": updated.priority,
                "inherits_role_id": updated.inherits_role_id,
                "inherit_permissions": updated.inherit_permissions,
            }))
            .ip_address(address.as_text())
            .organization(updated.organization_id),
    )
    .await?;

    Ok(Json(RoleBody::new(
        &updated,
        summary.get(&role_id).copied().unwrap_or_default(),
    )))
}

/// Delete a custom role that carries no live binding.
pub async fn delete_role(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(role_id): Path<Uuid>,
    address: ClientAddress,
) -> Result<StatusCode, ApiError> {
    let pool = state.db().pool();
    let role = role_store::find_role(pool, role_id)
        .await?
        .ok_or(PermissionsError::RoleNotFound)?;
    ensure_same_organization(&current, role.organization_id)?;

    role_store::delete_role(pool, role_id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.role.deleted")
            .target("role", role_id.to_string())
            .metadata(json!({ "key": role.key, "name": role.name }))
            .ip_address(address.as_text())
            .organization(role.organization_id),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// Clone a role — the way an organization customises a platform role.
pub async fn duplicate_role(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(role_id): Path<Uuid>,
    address: ClientAddress,
    Json(body): Json<DuplicateRoleRequest>,
) -> Result<(StatusCode, Json<RoleBody>), ApiError> {
    let pool = state.db().pool();
    let source = role_store::find_role(pool, role_id)
        .await?
        .ok_or(PermissionsError::RoleNotFound)?;
    ensure_same_organization(&current, source.organization_id)?;

    let organization_id = resolve_organization(&current, body.organization_id)?;
    let role =
        role_store::duplicate_role(pool, role_id, organization_id, &body.key, &body.name).await?;

    let entries = role_store::permission_entries(pool, &[role.id])
        .await?
        .remove(&role.id)
        .unwrap_or_default();
    versions::record(pool, &role, &entries, "duplicated", Some(current.user.id)).await?;

    let summary = role_store::permission_summary(pool, &[role.id]).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.role.duplicated")
            .organization(organization_id)
            .target("role", role.id.to_string())
            .metadata(json!({
                "source_role_id": source.id,
                "key": role.key,
                "entries": entries.len(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(RoleBody::new(
            &role,
            summary.get(&role.id).copied().unwrap_or_default(),
        )),
    ))
}

/// Read a role's version history, each version with its diff.
pub async fn list_role_versions(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(role_id): Path<Uuid>,
) -> Result<Json<RoleVersionsResponse>, ApiError> {
    let pool = state.db().pool();
    let role = role_store::find_role(pool, role_id)
        .await?
        .ok_or(PermissionsError::RoleNotFound)?;
    ensure_same_organization(&current, role.organization_id)?;

    let rows = versions::list(pool, role_id).await?;
    let mut views = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        // The list is newest first, so the version before this one is the next row.
        let diff = match rows.get(index + 1) {
            Some(previous) => versions::diff(&previous.entries(), &row.entries()),
            None => versions::diff(&[], &row.entries()),
        };
        views.push(RoleVersionView {
            version: row.version,
            name: row.name.clone(),
            description: row.description.clone(),
            priority: row.priority,
            change: row.change.clone(),
            changed_by: row.changed_by,
            created_at: row.created_at,
            permissions: row
                .entries()
                .iter()
                .map(PermissionEntryView::from)
                .collect(),
            diff: RoleDiffView::from(&diff),
            diff_total: diff.total(),
        });
    }

    Ok(Json(RoleVersionsResponse {
        role_id,
        versions: views,
    }))
}

/// Preview a permission set: what it would change and why it would be refused.
///
/// Nothing is written here — the screen shows the diff before the save, and the same validation
/// runs again inside the save itself.
pub async fn preview_role_permissions(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(role_id): Path<Uuid>,
    Json(body): Json<SetRolePermissionsRequest>,
) -> Result<Json<RolePreviewResponse>, ApiError> {
    let pool = state.db().pool();
    let role = role_store::find_role(pool, role_id)
        .await?
        .ok_or(PermissionsError::RoleNotFound)?;
    ensure_same_organization(&current, role.organization_id)?;
    if role.is_system {
        return Err(PermissionsError::SystemRole.into());
    }

    let mut problems: Vec<String> = Vec::new();
    let mut validated: BTreeMap<String, Effect> = BTreeMap::new();
    for entry in &body.permissions {
        if !catalogue::is_known(&entry.key) {
            problems.push(format!("unknown permission key: {}", entry.key));
            continue;
        }
        if validated
            .insert(entry.key.clone(), entry.effect.into())
            .is_some()
        {
            problems.push(format!("duplicate entry: {}", entry.key));
        }
    }
    problems.sort();
    problems.dedup();

    let current_entries = role_store::permission_entries(pool, &[role_id])
        .await?
        .remove(&role_id)
        .unwrap_or_default();
    let proposed: Vec<RolePermission> = validated
        .iter()
        .map(|(key, effect)| RolePermission {
            key: key.clone(),
            effect: *effect,
        })
        .collect();
    let diff = versions::diff(&current_entries, &proposed);
    let version = versions::latest_version(pool, role_id).await?.unwrap_or(0);

    Ok(Json(RolePreviewResponse {
        diff: RoleDiffView::from(&diff),
        problems,
        version,
        unchanged: diff.is_empty(),
    }))
}

/// Replace the permission set of a role the organization owns.
///
/// The save is atomic: an unknown key, a duplicate entry or a stale `expected_version` refuses
/// the whole request before anything is written, and the answer carries the diff that was
/// applied plus the version number the history now shows.
pub async fn set_role_permissions(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(role_id): Path<Uuid>,
    address: ClientAddress,
    Json(body): Json<SetRolePermissionsRequest>,
) -> Result<Json<RoleSaveResponse>, ApiError> {
    let pool = state.db().pool();
    let role = role_store::find_role(pool, role_id)
        .await?
        .ok_or(PermissionsError::RoleNotFound)?;
    ensure_same_organization(&current, role.organization_id)?;

    let entries: Vec<RolePermissionInput> = body
        .permissions
        .iter()
        .map(|entry| RolePermissionInput {
            key: entry.key.clone(),
            effect: entry.effect.into(),
        })
        .collect();

    let outcome = role_store::replace_role_permissions(
        pool,
        role_id,
        &entries,
        body.expected_version,
        Some(current.user.id),
    )
    .await?;

    let summaries = role_store::permission_summary(pool, &[role_id]).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.role.permissions_changed")
            .target("role", role_id.to_string())
            .metadata(json!({
                "key": outcome.role.key,
                "entries": outcome.entries.len(),
                "version": outcome.version,
                "added": outcome.diff.added.len(),
                "changed": outcome.diff.changed.len(),
                "removed": outcome.diff.removed.len(),
                "organization_id": outcome.role.organization_id,
            }))
            .ip_address(address.as_text())
            .organization(outcome.role.organization_id),
    )
    .await?;

    Ok(Json(RoleSaveResponse {
        role: RoleBody::new(
            &outcome.role,
            summaries.get(&role_id).copied().unwrap_or_default(),
        ),
        permissions: outcome
            .entries
            .iter()
            .map(PermissionEntryView::from)
            .collect(),
        diff: RoleDiffView::from(&outcome.diff),
        version: outcome.version,
    }))
}

/// One member of a role — a subject the role currently or formerly applied to.
#[derive(Debug, Serialize)]
pub struct RoleMemberView {
    /// `user`, `group` or `service_account`.
    pub subject_type: String,
    /// Id of the subject.
    pub subject_id: Uuid,
    /// The account, when the subject is a person.
    pub user_id: Option<Uuid>,
    /// What to show: the account's e-mail, the group's or the identity's name.
    pub label: String,
    /// Where the role applies.
    pub scope: ScopeBody,
    /// When it stops applying (temporary roles).
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    /// When it was revoked.
    #[serde(with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<OffsetDateTime>,
    /// Whether the binding applies right now.
    pub active: bool,
    /// Whether the binding's window has run out (shown as expired, not deleted).
    pub expired: bool,
    /// When it was granted.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Response body of `GET /api/v1/iam/roles/{id}/members`.
#[derive(Debug, Serialize)]
pub struct RoleMembersResponse {
    /// The role the members carry.
    pub role_id: Uuid,
    /// Members, live bindings first.
    pub members: Vec<RoleMemberView>,
}

/// Read who carries a role.
pub async fn list_role_members(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(role_id): Path<Uuid>,
) -> Result<Json<RoleMembersResponse>, ApiError> {
    let pool = state.db().pool();
    let role = role_store::find_role(pool, role_id)
        .await?
        .ok_or(PermissionsError::RoleNotFound)?;
    ensure_same_organization(&current, role.organization_id)?;

    #[derive(sqlx::FromRow)]
    struct MemberRow {
        subject_type: String,
        subject_id: Uuid,
        user_id: Option<Uuid>,
        label: String,
        scope_type: String,
        organization_id: Option<Uuid>,
        site_id: Option<Uuid>,
        resource_type: Option<String>,
        resource_id: Option<String>,
        expires_at: Option<OffsetDateTime>,
        revoked_at: Option<OffsetDateTime>,
        created_at: OffsetDateTime,
    }

    let rows: Vec<MemberRow> = sqlx::query_as(
        "select b.subject_type, b.subject_id, b.user_id, \
                coalesce(u.email, g.name, s.name, b.subject_id::text) as label, \
                b.scope_type, b.organization_id, b.site_id, b.resource_type, b.resource_id, \
                b.expires_at, b.revoked_at, b.created_at \
         from role_bindings b \
         left join users u on b.subject_type = 'user' and u.id = b.subject_id \
         left join groups g on b.subject_type = 'group' and g.id = b.subject_id \
         left join service_accounts s on b.subject_type = 'service_account' and s.id = b.subject_id \
         where b.role_id = $1 \
         order by (b.revoked_at is null) desc, b.created_at desc, b.id desc",
    )
    .bind(role_id)
    .fetch_all(pool)
    .await
    .map_err(PermissionsError::Database)?;

    let now = OffsetDateTime::now_utc();
    let mut members = Vec::with_capacity(rows.len());
    for row in rows {
        let scope = Scope::from_parts(
            &row.scope_type,
            row.organization_id,
            row.site_id,
            row.resource_type.as_deref(),
            row.resource_id.as_deref(),
        )?;
        let active = row.revoked_at.is_none() && row.expires_at.is_none_or(|expires| expires > now);
        let expired =
            row.revoked_at.is_none() && row.expires_at.is_some_and(|expires| expires <= now);
        members.push(RoleMemberView {
            subject_type: row.subject_type,
            subject_id: row.subject_id,
            user_id: row.user_id,
            label: row.label,
            scope: scope.into(),
            expires_at: row.expires_at,
            revoked_at: row.revoked_at,
            active,
            expired,
            created_at: row.created_at,
        });
    }

    Ok(Json(RoleMembersResponse { role_id, members }))
}

/// List role assignments: the caller's own by default, any subject or role on request.
pub async fn list_bindings(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<BindingsQuery>,
) -> Result<Json<BindingsResponse>, ApiError> {
    let pool = state.db().pool();

    // The pre-subject shape still works: `?user_id=` names a person.
    let subject = match (&query.subject_type, query.subject_id, query.user_id) {
        (None, None, Some(user_id)) => Some(Subject::User(user_id)),
        (None, None, None) => Some(Subject::User(current.user.id)),
        (Some(kind), Some(id), _) => Some(subject_from_parts(kind, id)?),
        (Some(_), None, _) => {
            return Err(ApiError::bad_request(
                "invalid_subject",
                "subject_id is required when subject_type is given",
            ));
        }
        (None, Some(_id), _) => {
            return Err(ApiError::bad_request(
                "invalid_subject",
                "subject_type is required when subject_id is given",
            ));
        }
    };

    let organization_id = match current.user.organization_id {
        Some(own) => {
            if let Some(requested) = query.organization_id
                && requested != own
            {
                return Err(crate::scope::cross_organization());
            }
            Some(own)
        }
        None => query.organization_id,
    };

    let filter = bindings::BindingFilter {
        organization_id,
        subject,
        role_id: query.role_id,
        live_only: query.live.unwrap_or(false),
        limit: query.limit.unwrap_or(200),
    };
    let rows = bindings::list(pool, &filter).await?;

    Ok(Json(BindingsResponse {
        subject_type: filter
            .subject
            .map(|subject| subject.subject_type().to_owned()),
        subject_id: filter.subject.map(|subject| subject.id()),
        bindings: rows.iter().map(BindingBody::from).collect(),
    }))
}

/// Assign a role to a subject (person, group or machine identity).
pub async fn create_binding(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<CreateBindingRequest>,
) -> Result<(StatusCode, Json<BindingBody>), ApiError> {
    let scope = scope_from_request(&body)?;
    ensure_same_organization(&current, scope.organization_id())?;

    let expires_at = match body.expires_at.as_deref() {
        Some(value) => Some(OffsetDateTime::parse(value, &Rfc3339).map_err(|_| {
            ApiError::bad_request("invalid_expiry", "expires_at must be an RFC 3339 timestamp")
        })?),
        None => None,
    };

    let subject_kind = body
        .subject_type
        .map_or("user", |kind| kind.as_str())
        .to_owned();
    let subject_id = body
        .subject_id
        .or(body.user_id)
        .ok_or_else(|| ApiError::bad_request("invalid_subject", "a binding needs its subject"))?;
    let subject = subject_from_parts(&subject_kind, subject_id)?;

    let new = NewSubjectBinding {
        role_id: body.role_id,
        subject,
        scope,
        granted_by: Some(current.user.id),
        expires_at,
    };

    let pool = state.db().pool();
    bindings::validate_subject(pool, &new).await?;
    let binding = bindings::grant_subject(pool, new).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.binding.granted")
            .target(
                binding.subject.subject_type(),
                binding.subject.id().to_string(),
            )
            .metadata(json!({
                "role_id": binding.role_id,
                "scope": binding.scope.describe(),
                "subject_type": binding.subject.subject_type(),
                "expires_at": expires_at.map(|value| value.to_string()),
            }))
            .ip_address(address.as_text())
            .organization(binding.scope.organization_id()),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(BindingBody::from(&binding))))
}

/// Revoke a role assignment.
pub async fn delete_binding(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(binding_id): Path<Uuid>,
) -> Result<Json<BindingBody>, ApiError> {
    let pool = state.db().pool();
    let bindings_list = bindings::list(
        pool,
        &bindings::BindingFilter {
            organization_id: None,
            subject: None,
            role_id: None,
            live_only: false,
            limit: 500,
        },
    )
    .await?;
    let binding = bindings_list
        .into_iter()
        .find(|binding| binding.id == binding_id)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "binding_not_found",
                "no such role assignment",
            )
        })?;

    if current.user.organization_id.is_some() {
        ensure_same_organization(&current, binding.scope.organization_id())?;
    }

    // The safety invariants (docs/07-IAM.md §19, REQ-006 slice 4a): an organization keeps at
    // least one live owner or administrator binding, and nobody removes their own last
    // privileged binding. A refused change leaves the store untouched.
    if let Some(refusal) =
        omnion_permissions::check_binding_revocation(pool, binding_id, current.user.id).await?
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            refusal.code,
            refusal.message,
        ));
    }

    let revoked = bindings::revoke(pool, binding_id).await?;
    if !revoked {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "binding_already_revoked",
            "this role assignment was already revoked",
        ));
    }

    let refreshed = bindings::list(
        pool,
        &bindings::BindingFilter {
            organization_id: None,
            subject: Some(binding.subject),
            role_id: None,
            live_only: false,
            limit: 500,
        },
    )
    .await?
    .into_iter()
    .find(|candidate| candidate.id == binding_id)
    .unwrap_or(binding);

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.binding.revoked")
            .target(
                refreshed.subject.subject_type(),
                refreshed.subject.id().to_string(),
            )
            .metadata(json!({
                "role_id": refreshed.role_id,
                "scope": refreshed.scope.describe(),
            }))
            .ip_address(address.as_text())
            .organization(refreshed.scope.organization_id()),
    )
    .await?;

    Ok(Json(BindingBody::from(&refreshed)))
}

/// Build a subject from its stored kind.
fn subject_from_parts(kind: &str, id: Uuid) -> Result<Subject, ApiError> {
    match kind {
        "user" => Ok(Subject::User(id)),
        "group" => Ok(Subject::Group(id)),
        "service_account" => Ok(Subject::ServiceAccount(id)),
        other => Err(ApiError::bad_request(
            "invalid_subject",
            format!("unknown subject type {other:?} — expected user, group or service_account"),
        )),
    }
}

/// Resolve the effective permission set of an account.
///
/// Reading one's own set needs no permission — it answers "what may I do here", which every
/// signed-in account is entitled to. Reading somebody else's needs `iam.roles.read`.
pub async fn effective_permissions(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<EffectivePermissionsQuery>,
) -> Result<Json<EffectivePermissionsResponse>, ApiError> {
    let pool = state.db().pool();
    let user_id = query.user_id.unwrap_or(current.user.id);

    if user_id != current.user.id {
        let decision = omnion_permissions::authorize(
            pool,
            current.user.id,
            scope_of(&current.user),
            "iam.roles.read",
        )
        .await?;
        if !decision.is_allowed() {
            return Err(ApiError::forbidden(
                "permission_denied",
                "reading another account's permissions requires the \"iam.roles.read\" permission",
            ));
        }
    }

    let target = omnion_identity::users::find_by_id(pool, user_id)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "account_not_found",
                "no such account",
            )
        })?;

    let scope = resolve_scope(query.site_id, query.organization_id, target.organization_id);
    if current.user.organization_id.is_some() {
        ensure_same_organization(&current, scope.organization_id())?;
    }

    let effective = omnion_permissions::effective_permissions_in(
        pool,
        user_id,
        scope.clone(),
        query.path.as_deref(),
    )
    .await?;
    let denied_keys: Vec<String> = effective.denials().keys().cloned().collect();

    let granted = effective
        .grants()
        .iter()
        .filter(|(key, _)| !denied_keys.contains(key))
        .map(|(key, grant)| EffectiveEntryBody {
            key: key.clone(),
            source: GrantSourceBody::from(grant),
        })
        .collect();
    let denied = effective
        .denials()
        .iter()
        .map(|(key, grant)| EffectiveEntryBody {
            key: key.clone(),
            source: GrantSourceBody::from(grant),
        })
        .collect();

    Ok(Json(EffectivePermissionsResponse {
        user_id,
        scope: scope.into(),
        granted,
        denied,
        granted_count: effective.len(),
    }))
}

/// Read the audit trail.
pub async fn list_audit(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<AuditQuery>,
) -> Result<Json<AuditResponse>, ApiError> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_AUDIT_LIMIT)
        .clamp(1, MAX_AUDIT_LIMIT);

    let organization_id = match current.user.organization_id {
        Some(own) => {
            if let Some(requested) = query.organization_id {
                if requested != own {
                    return Err(ApiError::forbidden(
                        "cross_organization",
                        "the audit trail of another organization is not visible here",
                    ));
                }
            }
            Some(own)
        }
        None => query.organization_id,
    };

    let entries = omnion_audit::recent(state.db().pool(), organization_id, limit).await?;

    Ok(Json(AuditResponse {
        entries: entries.into_iter().map(AuditBody::from).collect(),
    }))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Write an audit row; a privileged action is not reported as successful without one.
pub(crate) async fn record(state: &AppState, entry: NewAuditEntry) -> Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}

/// Build a scope from a binding request.
fn scope_from_request(body: &CreateBindingRequest) -> Result<Scope, ApiError> {
    match body.scope_type {
        ScopeTypeBody::Global => Ok(Scope::Global),
        ScopeTypeBody::Organization => {
            let organization_id = body.organization_id.ok_or_else(|| {
                ApiError::bad_request(
                    "invalid_scope",
                    "organization_id is required for an organization scope",
                )
            })?;
            Ok(Scope::Organization { organization_id })
        }
        ScopeTypeBody::Site => {
            let site_id = body.site_id.ok_or_else(|| {
                ApiError::bad_request("invalid_scope", "site_id is required for a site scope")
            })?;
            Ok(Scope::Site {
                organization_id: body.organization_id,
                site_id,
            })
        }
        ScopeTypeBody::Department => {
            let organization_id = body.organization_id.ok_or_else(|| {
                ApiError::bad_request(
                    "invalid_scope",
                    "organization_id is required for a department scope",
                )
            })?;
            let department = body
                .department
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    ApiError::bad_request(
                        "invalid_scope",
                        "department is required for a department scope",
                    )
                })?;
            Ok(Scope::Department {
                organization_id,
                department: department.to_owned(),
            })
        }
        ScopeTypeBody::Module => {
            if body.organization_id.is_none() && body.site_id.is_none() {
                return Err(ApiError::bad_request(
                    "invalid_scope",
                    "a module scope needs an organization or a site",
                ));
            }
            let module = body
                .module
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    ApiError::bad_request("invalid_scope", "module is required for a module scope")
                })?;
            Ok(Scope::Module {
                organization_id: body.organization_id,
                site_id: body.site_id,
                module: module.to_owned(),
            })
        }
        ScopeTypeBody::Resource => {
            if body.organization_id.is_none() && body.site_id.is_none() {
                return Err(ApiError::bad_request(
                    "invalid_scope",
                    "a resource scope needs an organization or a site",
                ));
            }
            let resource_id = body
                .resource_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    ApiError::bad_request(
                        "invalid_scope",
                        "resource_id is required for a resource scope",
                    )
                })?;
            Ok(Scope::Resource {
                organization_id: body.organization_id,
                site_id: body.site_id,
                resource_type: body
                    .resource_type
                    .clone()
                    .unwrap_or_else(|| "path".to_owned()),
                resource_id: resource_id.to_owned(),
            })
        }
    }
}

/// Default scope of a resolve request: the site when asked for, else the organization, else the
/// account's own organization, else the platform level.
fn resolve_scope(
    site_id: Option<Uuid>,
    organization_id: Option<Uuid>,
    account_organization_id: Option<Uuid>,
) -> Scope {
    if let Some(site_id) = site_id {
        return Scope::Site {
            organization_id: organization_id.or(account_organization_id),
            site_id,
        };
    }
    match organization_id.or(account_organization_id) {
        Some(organization_id) => Scope::Organization { organization_id },
        None => Scope::Global,
    }
}

fn via_name(via: evaluate::Via) -> &'static str {
    match via {
        evaluate::Via::ExplicitAllow => "explicit_allow",
        evaluate::Via::InheritedAllow => "inherited_allow",
        evaluate::Via::ExplicitDeny => "explicit_deny",
        evaluate::Via::InheritedDeny => "inherited_deny",
        evaluate::Via::Policy => "policy",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding_request(
        scope_type: ScopeTypeBody,
        organization_id: Option<Uuid>,
        site_id: Option<Uuid>,
    ) -> CreateBindingRequest {
        CreateBindingRequest {
            user_id: Some(Uuid::nil()),
            subject_type: None,
            subject_id: None,
            role_id: Uuid::nil(),
            scope_type,
            organization_id,
            site_id,
            department: None,
            module: None,
            resource_type: None,
            resource_id: None,
            expires_at: None,
        }
    }

    #[test]
    fn scopes_are_built_from_the_request_shape() {
        let organization_id = Uuid::new_v4();
        let site_id = Uuid::new_v4();

        assert_eq!(
            scope_from_request(&binding_request(ScopeTypeBody::Global, None, None))
                .expect("global"),
            Scope::Global
        );
        assert_eq!(
            scope_from_request(&binding_request(
                ScopeTypeBody::Organization,
                Some(organization_id),
                None
            ))
            .expect("organization"),
            Scope::Organization { organization_id }
        );
        assert_eq!(
            scope_from_request(&binding_request(
                ScopeTypeBody::Site,
                Some(organization_id),
                Some(site_id)
            ))
            .expect("site"),
            Scope::Site {
                organization_id: Some(organization_id),
                site_id
            }
        );

        assert!(
            scope_from_request(&binding_request(ScopeTypeBody::Organization, None, None)).is_err(),
            "an organization scope needs an organization"
        );
        assert!(
            scope_from_request(&binding_request(ScopeTypeBody::Site, None, None)).is_err(),
            "a site scope needs a site"
        );
    }

    #[test]
    fn resolve_scope_prefers_the_most_specific_scope() {
        let organization_id = Uuid::new_v4();
        let other_organization = Uuid::new_v4();
        let site_id = Uuid::new_v4();

        assert_eq!(
            resolve_scope(None, None, Some(organization_id)),
            Scope::Organization { organization_id }
        );
        assert_eq!(
            resolve_scope(None, Some(other_organization), Some(organization_id)),
            Scope::Organization {
                organization_id: other_organization
            }
        );
        assert_eq!(
            resolve_scope(Some(site_id), None, Some(organization_id)),
            Scope::Site {
                organization_id: Some(organization_id),
                site_id
            }
        );
        assert_eq!(resolve_scope(None, None, None), Scope::Global);
    }

    #[test]
    fn effect_bodies_map_onto_the_model() {
        assert_eq!(Effect::from(EffectBody::Allow), Effect::Allow);
        assert_eq!(Effect::from(EffectBody::Deny), Effect::Deny);
        assert_eq!(via_name(evaluate::Via::ExplicitDeny), "explicit_deny");
    }
}
