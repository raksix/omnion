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
    Effect, NewBinding, NewRole, PermissionSummary, Role, RolePermissionInput,
};
use omnion_permissions::{PermissionsError, Scope};
use omnion_permissions::{bindings, catalogue, evaluate, roles as role_store};
use serde::{Deserialize, Serialize};
use serde_json::json;
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
#[derive(Debug, Serialize)]
pub struct ScopeBody {
    /// `global`, `organization` or `site`.
    #[serde(rename = "type")]
    pub scope_type: &'static str,
    /// Organization of the scope, when it has one.
    pub organization_id: Option<Uuid>,
    /// Site of the scope, when it is site scoped.
    pub site_id: Option<Uuid>,
}

impl From<Scope> for ScopeBody {
    fn from(scope: Scope) -> Self {
        Self {
            scope_type: scope.scope_type(),
            organization_id: scope.organization_id(),
            site_id: scope.site_id(),
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
    /// Account that holds the role.
    pub user_id: Uuid,
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
    /// Creation timestamp, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<&omnion_permissions::RoleBinding> for BindingBody {
    fn from(binding: &omnion_permissions::RoleBinding) -> Self {
        Self {
            id: binding.id,
            role_id: binding.role_id,
            user_id: binding.user_id,
            scope: binding.scope.into(),
            granted_by: binding.granted_by,
            expires_at: binding.expires_at,
            revoked_at: binding.revoked_at,
            active: binding.is_active_at(OffsetDateTime::now_utc()),
            created_at: binding.created_at,
        }
    }
}

/// Response body of the binding endpoints.
#[derive(Debug, Serialize)]
pub struct BindingsResponse {
    /// Account the bindings belong to.
    pub user_id: Uuid,
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
}

/// `POST /api/v1/iam/bindings`.
#[derive(Debug, Deserialize)]
pub struct CreateBindingRequest {
    /// Account that receives the role.
    pub user_id: Uuid,
    /// Role to assign.
    pub role_id: Uuid,
    /// `global`, `organization` or `site`.
    pub scope_type: ScopeTypeBody,
    /// Organization of the scope.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// Site of the scope.
    #[serde(default)]
    pub site_id: Option<Uuid>,
    /// Optional expiry as an RFC 3339 timestamp (temporary roles).
    #[serde(default)]
    pub expires_at: Option<String>,
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
}

/// Query parameters of `GET /api/v1/iam/bindings`.
#[derive(Debug, Deserialize)]
pub struct BindingsQuery {
    /// Account to list; defaults to the caller.
    #[serde(default)]
    pub user_id: Option<Uuid>,
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

/// List the roles visible to the caller.
pub async fn list_roles(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<RolesResponse>, ApiError> {
    let pool = state.db().pool();
    let roles = role_store::list_roles(pool, current.user.organization_id).await?;
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

/// Replace the permission set of a role the organization owns.
pub async fn set_role_permissions(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(role_id): Path<Uuid>,
    address: ClientAddress,
    Json(body): Json<SetRolePermissionsRequest>,
) -> Result<Json<RoleBody>, ApiError> {
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

    role_store::set_role_permissions(pool, role_id, &entries).await?;
    let summaries = role_store::permission_summary(pool, &[role_id]).await?;
    let updated = role_store::find_role(pool, role_id)
        .await?
        .ok_or(PermissionsError::RoleNotFound)?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.role.permissions_updated")
            .target("role", role_id.to_string())
            .metadata(json!({
                "key": updated.key,
                "entries": entries.len(),
                "organization_id": updated.organization_id,
            }))
            .ip_address(address.as_text())
            .organization(updated.organization_id),
    )
    .await?;

    Ok(Json(RoleBody::new(
        &updated,
        summaries.get(&role_id).copied().unwrap_or_default(),
    )))
}

/// List the role assignments of an account (the caller's own by default).
pub async fn list_bindings(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<BindingsQuery>,
) -> Result<Json<BindingsResponse>, ApiError> {
    let user_id = query.user_id.unwrap_or(current.user.id);
    let bindings = bindings::list_for_user(state.db().pool(), user_id).await?;

    Ok(Json(BindingsResponse {
        user_id,
        bindings: bindings.iter().map(BindingBody::from).collect(),
    }))
}

/// Assign a role to an account.
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

    let new = NewBinding {
        role_id: body.role_id,
        user_id: body.user_id,
        scope,
        granted_by: Some(current.user.id),
        expires_at,
    };

    let pool = state.db().pool();
    bindings::validate(pool, &new).await?;
    let binding = bindings::grant(pool, new).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.binding.granted")
            .target("user", binding.user_id.to_string())
            .metadata(json!({
                "role_id": binding.role_id,
                "scope": binding.scope.describe(),
                "expires_at": expires_at.map(|value| value.to_string()),
            }))
            .ip_address(address.as_text())
            .organization(binding.scope.organization_id()),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(BindingBody::from(&binding))))
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

    let effective = omnion_permissions::effective_permissions(pool, user_id, scope).await?;
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
async fn record(state: &AppState, entry: NewAuditEntry) -> Result<(), ApiError> {
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
            user_id: Uuid::nil(),
            role_id: Uuid::nil(),
            scope_type,
            organization_id,
            site_id,
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
