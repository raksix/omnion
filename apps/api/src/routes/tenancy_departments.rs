//! `/api/v1/organizations/{id}/departments` — the structure inside a tenant (REQ-005, slice 2).
//!
//! Slice 1 answered *who belongs to this organization*. This module answers *how they are
//! arranged*: the department tree, who sits in which department, and which roles a department
//! carries as a whole.
//!
//! Two rules run through every handler here:
//!
//! * **The organization comes from the session, never from the body.** The path id is checked
//!   against the caller's memberships, and a department that belongs to another tenant answers
//!   `404 department_not_found` exactly like one that does not exist — never `403`, which
//!   would confirm the id is real.
//! * **A department binding is a grant to the people in it.** Binding a role here stores the
//!   department's *key* in `role_bindings.resource_id`, which is what the resolver compares
//!   against; the Departments tab and the member drawer read the same rows, so the count on
//!   screen is the count that grants.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_identity::IdentityError;
use omnion_identity::departments::{
    self, Department, DepartmentChanges, DepartmentSummary, NewDepartment,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

use super::tenancy_members::organization_in_scope;

// ---------------------------------------------------------------------------------------------
// Response bodies
// ---------------------------------------------------------------------------------------------

/// One row of the Departments tab.
#[derive(Debug, Clone, Serialize)]
pub struct DepartmentBody {
    /// Primary key.
    pub id: Uuid,
    /// Stable address inside the organization — what a role binding stores.
    pub key: String,
    /// Display name.
    pub name: String,
    /// Free-text description.
    pub description: String,
    /// `active` or `archived`.
    pub status: String,
    /// Parent department, `None` for a root.
    pub parent_id: Option<Uuid>,
    /// Parent's key, so the tree reads without a second round trip.
    pub parent_key: Option<String>,
    /// How many accounts sit in the department.
    pub member_count: i64,
    /// How many live role bindings name this department.
    pub role_count: i64,
    /// Depth in the tree, 0 for a root.
    pub depth: i32,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

/// A role bound to a department as a whole.
#[derive(Debug, Clone, Serialize)]
pub struct DepartmentRoleBody {
    /// The binding.
    pub binding_id: Uuid,
    /// Role id.
    pub role_id: Uuid,
    /// Role key.
    pub role_key: String,
    /// Role name.
    pub role_name: String,
    /// Who granted it.
    pub granted_by: Option<Uuid>,
    /// When it stops applying, when it is temporary.
    pub expires_at: Option<OffsetDateTime>,
}

/// One account inside a department.
#[derive(Debug, Clone, Serialize)]
pub struct DepartmentMemberBody {
    /// The account.
    pub user_id: Uuid,
    /// Display name.
    pub display_name: String,
    /// Address.
    pub email: String,
    /// Account status.
    pub user_status: String,
    /// Membership status inside the organization.
    pub membership_status: String,
    /// Last session activity.
    pub last_active_at: Option<OffsetDateTime>,
}

/// The Departments tab payload: the tree plus the roles bound at each department.
#[derive(Debug, Clone, Serialize)]
pub struct DepartmentsResponse {
    /// Organization the tree belongs to.
    pub organization_id: Uuid,
    /// Every department, parents before children.
    pub departments: Vec<DepartmentBody>,
}

/// One department in full.
#[derive(Debug, Clone, Serialize)]
pub struct DepartmentDetailResponse {
    /// The department.
    pub department: DepartmentBody,
    /// Accounts inside it.
    pub members: Vec<DepartmentMemberBody>,
    /// Roles bound to the department as a whole.
    pub roles: Vec<DepartmentRoleBody>,
}

/// One member's view of a department — what the member drawer reads.
#[derive(Debug, Clone, Serialize)]
pub struct MemberDepartmentBody {
    /// The department.
    pub id: Uuid,
    /// Stable address.
    pub key: String,
    /// Display name.
    pub name: String,
    /// `active` or `archived`.
    pub status: String,
}

// ---------------------------------------------------------------------------------------------
// Request shapes
// ---------------------------------------------------------------------------------------------

/// Filters of `GET /api/v1/organizations/{id}/departments`.
#[derive(Debug, Default, Deserialize)]
pub struct DepartmentsQuery {
    /// Keep only this status.
    #[serde(default)]
    pub status: Option<String>,
    /// Substring match over name, key and description.
    #[serde(default)]
    pub q: Option<String>,
}

/// `POST /api/v1/organizations/{id}/departments`.
#[derive(Debug, Deserialize)]
pub struct CreateDepartmentRequest {
    /// Stable address; normalized to lowercase and never changed afterwards.
    pub key: String,
    /// Display name.
    pub name: String,
    /// Optional description.
    #[serde(default)]
    pub description: Option<String>,
    /// Parent department; omitted for a root.
    #[serde(default)]
    pub parent_id: Option<Uuid>,
}

/// `PATCH /api/v1/organizations/{id}/departments/{department_id}`.
#[derive(Debug, Deserialize)]
pub struct UpdateDepartmentRequest {
    /// New display name.
    #[serde(default)]
    pub name: Option<String>,
    /// New description.
    #[serde(default)]
    pub description: Option<String>,
    /// New parent. `null` promotes the department to a root, so the field has to be
    /// distinguished from "absent" — hence the double option.
    #[serde(default, deserialize_with = "double_option")]
    pub parent_id: Option<Option<Uuid>>,
    /// New status (`active` or `archived`).
    #[serde(default)]
    pub status: Option<String>,
}

impl UpdateDepartmentRequest {
    fn changes(self) -> DepartmentChanges {
        DepartmentChanges {
            name: self.name,
            description: self.description,
            parent_id: self.parent_id,
            status: self.status,
        }
    }
}

/// `POST /api/v1/organizations/{id}/departments/{department_id}/members`.
#[derive(Debug, Deserialize)]
pub struct AddDepartmentMemberRequest {
    /// The account to put into the department.
    pub user_id: Uuid,
}

/// `POST /api/v1/organizations/{id}/departments/{department_id}/roles`.
#[derive(Debug, Deserialize)]
pub struct BindDepartmentRoleRequest {
    /// Role to bind to the department as a whole.
    pub role_id: Uuid,
    /// Optional expiry — a temporary department grant.
    #[serde(default)]
    pub expires_at: Option<OffsetDateTime>,
}

/// Tell "field absent" from "field present and null".
///
/// `#[serde(default)]` alone cannot: a missing `parent_id` and an explicit `"parent_id": null`
/// would both arrive as `None`, and the second has to mean *promote to a root* while the first
/// means *leave it where it is*.
fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    serde::Deserialize::deserialize(deserializer).map(Some)
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// List the departments of one organization.
pub async fn list_departments(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(organization_id): Path<Uuid>,
    Query(query): Query<DepartmentsQuery>,
) -> Result<Json<DepartmentsResponse>, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;
    let summaries = departments::list_department_summaries(state.db().pool(), organization.id).await?;

    // `parent_key_of` looks a department's *parent id* up in this map, so the entry has to be
    // the parent's own key. Building it from each row's `parent_id` would store the child's key
    // under the parent's id, and every row would then report itself as its own parent.
    let parents: HashMap<Uuid, String> = summaries
        .iter()
        .filter_map(|row| Some((row.id, row.key.clone())))
        .collect();

    let needle = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_lowercase);
    let status = query
        .status
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_lowercase);

    let departments = summaries
        .into_iter()
        .filter(|row| match &status {
            Some(wanted) => row.status == *wanted,
            None => true,
        })
        .filter(|row| match &needle {
            Some(wanted) => {
                row.name.to_lowercase().contains(wanted)
                    || row.key.contains(wanted)
                    || row.description.to_lowercase().contains(wanted)
            }
            None => true,
        })
        .map(|row| body_of(&row, parent_key_of(&parents, row.parent_id)))
        .collect();

    Ok(Json(DepartmentsResponse {
        organization_id: organization.id,
        departments,
    }))
}

/// Read one department with its members and the roles bound to it.
pub async fn get_department(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((organization_id, department_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<DepartmentDetailResponse>, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;
    let pool = state.db().pool();

    let row = load_summary(pool, organization.id, department_id).await?;
    let parents = parent_keys(pool, organization.id).await?;
    let members = member_bodies(pool, department_id).await?;
    let roles = role_bodies(pool, organization.id, &row.key).await?;

    Ok(Json(DepartmentDetailResponse {
        department: body_of(&row, parent_key_of(&parents, row.parent_id)),
        members,
        roles,
    }))
}

/// Create a department.
pub async fn create_department(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(organization_id): Path<Uuid>,
    address: crate::client_ip::ClientAddress,
    Json(body): Json<CreateDepartmentRequest>,
) -> Result<(StatusCode, Json<DepartmentBody>), ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;
    let pool = state.db().pool();

    let created = departments::create_department(
        pool,
        NewDepartment {
            organization_id: organization.id,
            parent_id: body.parent_id,
            key: body.key,
            name: body.name,
            description: body.description.unwrap_or_default(),
        },
    )
    .await?;

    bus::emit(
        state.db().pool(),
        NewEvent::new("organization.department.created")
            .organization(organization.id)
            .actor(current.user.id)
            .payload(json!({
                "organization_id": organization.id,
                "department_id": created.id,
                "department_key": created.key,
                "parent_id": created.parent_id,
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "organization.department.created")
            .target("organization_department", created.id.to_string())
            .metadata(json!({
                "department_id": created.id,
                "department_key": created.key,
                "parent_id": created.parent_id,
            }))
            .ip_address(address.as_text())
            .organization(organization.id),
    )
    .await?;

    let row = load_summary(pool, organization.id, created.id).await?;
    let parents = parent_keys(pool, organization.id).await?;
    Ok((
        StatusCode::CREATED,
        Json(body_of(&row, parent_key_of(&parents, row.parent_id))),
    ))
}

/// Rename, re-describe, re-parent or archive a department.
pub async fn update_department(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((organization_id, department_id)): Path<(Uuid, Uuid)>,
    address: crate::client_ip::ClientAddress,
    Json(body): Json<UpdateDepartmentRequest>,
) -> Result<Json<DepartmentBody>, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;
    let pool = state.db().pool();
    let changes = body.changes();

    let updated = departments::update_department(pool, organization.id, department_id, &changes)
        .await?;

    bus::emit(
        state.db().pool(),
        NewEvent::new("organization.department.updated")
            .organization(organization.id)
            .actor(current.user.id)
            .payload(json!({
                "organization_id": organization.id,
                "department_id": updated.id,
                "department_key": updated.key,
                "status": updated.status,
                "parent_id": updated.parent_id,
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "organization.department.updated")
            .target("organization_department", department_id.to_string())
            .metadata(json!({
                "department_id": updated.id,
                "department_key": updated.key,
                "status": updated.status,
                "parent_id": updated.parent_id,
            }))
            .ip_address(address.as_text())
            .organization(organization.id),
    )
    .await?;

    let row = load_summary(pool, organization.id, department_id).await?;
    let parents = parent_keys(pool, organization.id).await?;
    let parent_id = row.parent_id;
    Ok(Json(body_of(&row, parent_key_of(&parents, parent_id))))
}

/// Archive a department.
///
/// Archiving keeps the structure and stops it granting: the resolver only walks active
/// departments, so the roles bound here stop applying without anyone having to remember to
/// revoke them.
pub async fn archive_department(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((organization_id, department_id)): Path<(Uuid, Uuid)>,
    address: crate::client_ip::ClientAddress,
) -> Result<Json<DepartmentBody>, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;
    let pool = state.db().pool();

    let updated = departments::archive_department(pool, organization.id, department_id).await?;

    bus::emit(
        state.db().pool(),
        NewEvent::new("organization.department.archived")
            .organization(organization.id)
            .actor(current.user.id)
            .payload(json!({
                "organization_id": organization.id,
                "department_id": updated.id,
                "department_key": updated.key,
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "organization.department.archived")
            .target("organization_department", department_id.to_string())
            .metadata(json!({
                "department_id": updated.id,
                "department_key": updated.key,
            }))
            .ip_address(address.as_text())
            .organization(organization.id),
    )
    .await?;

    let row = load_summary(pool, organization.id, department_id).await?;
    let parents = parent_keys(pool, organization.id).await?;
    let parent_id = row.parent_id;
    Ok(Json(body_of(&row, parent_key_of(&parents, parent_id))))
}

/// Delete a department.
///
/// Refused while it still holds accounts, live role bindings or sub-departments; the store
/// names which. A quiet delete that dropped somebody's only role is the failure mode the
/// request calls out, so the answer has to be loud.
pub async fn delete_department(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((organization_id, department_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;
    let removed = departments::delete_department(state.db().pool(), organization.id, department_id)
        .await?;
    Ok(if removed {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    })
}

/// Put an account into a department.
pub async fn add_department_member(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((organization_id, department_id)): Path<(Uuid, Uuid)>,
    address: crate::client_ip::ClientAddress,
    Json(body): Json<AddDepartmentMemberRequest>,
) -> Result<StatusCode, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;
    let pool = state.db().pool();

    let added = departments::add_department_member(
        pool,
        organization.id,
        department_id,
        body.user_id,
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "organization.department.member_added")
            .target("organization_department", department_id.to_string())
            .metadata(json!({
                "department_id": department_id,
                "user_id": body.user_id,
            }))
            .ip_address(address.as_text())
            .organization(organization.id),
    )
    .await?;

    Ok(if added {
        StatusCode::CREATED
    } else {
        // Already in it: the pair is the primary key, so the second add is a no-op rather than
        // a duplicate. 200 says "it is there", which is the truth.
        StatusCode::OK
    })
}

/// Take an account out of a department.
pub async fn remove_department_member(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((organization_id, department_id, user_id)): Path<(Uuid, Uuid, Uuid)>,
    address: crate::client_ip::ClientAddress,
) -> Result<StatusCode, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;
    let pool = state.db().pool();

    // The department is resolved first so another tenant's id is a 404 before anything is
    // deleted — the membership row is keyed by department id alone.
    departments::find_department(pool, organization.id, department_id)
        .await?
        .ok_or_else(department_not_found)?;

    let removed = departments::remove_department_member(pool, department_id, user_id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "organization.department.member_removed")
            .target("organization_department", department_id.to_string())
            .metadata(json!({
                "department_id": department_id,
                "user_id": user_id,
            }))
            .ip_address(address.as_text())
            .organization(organization.id),
    )
    .await?;

    Ok(if removed {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    })
}

/// The departments one account sits in — what the member drawer reads.
pub async fn list_member_departments(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((organization_id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Vec<MemberDepartmentBody>>, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;

    let rows = departments::list_departments_of_user(state.db().pool(), organization.id, user_id)
        .await?;

    Ok(Json(
        rows.into_iter()
            .map(|row| MemberDepartmentBody {
                id: row.id,
                key: row.key,
                name: row.name,
                status: row.status,
            })
            .collect(),
    ))
}

/// Bind a role to a department as a whole.
///
/// The binding stores the department's *key*, which is the string the resolver compares against
/// a request's department context. A binding to a key that does not exist in this organization
/// is refused at save time rather than becoming a grant nobody can receive.
pub async fn bind_department_role(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((organization_id, department_id)): Path<(Uuid, Uuid)>,
    address: crate::client_ip::ClientAddress,
    Json(body): Json<BindDepartmentRoleRequest>,
) -> Result<(StatusCode, Json<DepartmentRoleBody>), ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;
    let pool = state.db().pool();

    let department = departments::find_department(pool, organization.id, department_id)
        .await?
        .ok_or_else(department_not_found)?;

    // The role must belong to this organization (or be a platform role) — a customer role must
    // not become a bridge to somebody else's, and a role from another tenant would resolve
    // against a graph that does not contain it.
    let role = omnion_permissions::roles::find_role(pool, body.role_id)
        .await?
        .ok_or_else(|| {
            ApiError::bad_request("invalid_request", format!("no role {}", body.role_id))
        })?;
    if let Some(role_organization) = role.organization_id {
        if role_organization != organization.id {
            return Err(ApiError::forbidden(
                "cross_organization",
                "that role belongs to another organization",
            ));
        }
    }

    let binding = omnion_permissions::bindings::grant_subject(
        pool,
        omnion_permissions::NewSubjectBinding {
            role_id: body.role_id,
            subject: omnion_permissions::Subject::Group(department.id),
            scope: omnion_permissions::Scope::Department {
                organization_id: organization.id,
                department: department.key.clone(),
            },
            granted_by: Some(current.user.id),
            expires_at: body.expires_at,
        },
    )
    .await?;

    bus::emit(
        state.db().pool(),
        NewEvent::new("organization.member.role_changed")
            .organization(organization.id)
            .actor(current.user.id)
            .payload(json!({
                "organization_id": organization.id,
                "department_id": department.id,
                "department_key": department.key,
                "role_id": body.role_id,
                "action": "bound",
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "organization.department.role_bound")
            .target("organization_department", department_id.to_string())
            .metadata(json!({
                "department_id": department.id,
                "department_key": department.key,
                "role_id": body.role_id,
                "binding_id": binding.id,
            }))
            .ip_address(address.as_text())
            .organization(organization.id),
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(DepartmentRoleBody {
            binding_id: binding.id,
            role_id: body.role_id,
            role_key: role.key,
            role_name: role.name,
            granted_by: binding.granted_by,
            expires_at: binding.expires_at,
        }),
    ))
}

/// Revoke a role bound to a department.
pub async fn unbind_department_role(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((organization_id, department_id, binding_id)): Path<(Uuid, Uuid, Uuid)>,
    address: crate::client_ip::ClientAddress,
) -> Result<StatusCode, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;
    let pool = state.db().pool();

    let department = departments::find_department(pool, organization.id, department_id)
        .await?
        .ok_or_else(department_not_found)?;

    // The binding has to be *this* department's binding: the id comes from the path, so it is
    // checked against the department rather than trusted.
    let binding = omnion_permissions::bindings::bindings_of_subject(
        pool,
        omnion_permissions::Subject::Group(department.id),
    )
    .await?
    .into_iter()
    .find(|row| row.id == binding_id)
    .ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "binding_not_found",
            "no such role binding on this department",
        )
    })?;

    omnion_permissions::bindings::revoke(pool, binding.id).await?;

    bus::emit(
        state.db().pool(),
        NewEvent::new("organization.member.role_changed")
            .organization(organization.id)
            .actor(current.user.id)
            .payload(json!({
                "organization_id": organization.id,
                "department_id": department.id,
                "department_key": department.key,
                "role_id": binding.role_id,
                "action": "revoked",
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "organization.department.role_revoked")
            .target("organization_department", department_id.to_string())
            .metadata(json!({
                "department_id": department.id,
                "department_key": department.key,
                "role_id": binding.role_id,
                "binding_id": binding.id,
            }))
            .ip_address(address.as_text())
            .organization(organization.id),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn body_of(row: &DepartmentSummary, parent_key: Option<String>) -> DepartmentBody {
    DepartmentBody {
        id: row.id,
        key: row.key.clone(),
        name: row.name.clone(),
        description: row.description.clone(),
        status: row.status.clone(),
        parent_id: row.parent_id,
        parent_key,
        member_count: row.member_count,
        role_count: row.role_count,
        depth: row.depth,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

/// The Departments-tab row of one department, including its counts.
async fn load_summary(
    pool: &sqlx::PgPool,
    organization_id: Uuid,
    department_id: Uuid,
) -> Result<DepartmentSummary, ApiError> {
    let rows = departments::list_department_summaries(pool, organization_id).await?;
    rows.into_iter()
        .find(|row| row.id == department_id)
        .ok_or_else(department_not_found)
}

/// Department id → key, so the tree renders its parent column without a second walk.
async fn parent_keys(
    pool: &sqlx::PgPool,
    organization_id: Uuid,
) -> Result<HashMap<Uuid, String>, ApiError> {
    let rows = departments::list_departments(pool, organization_id).await?;
    Ok(rows
        .into_iter()
        .map(|row: Department| (row.id, row.key))
        .collect())
}

async fn member_bodies(
    pool: &sqlx::PgPool,
    department_id: Uuid,
) -> Result<Vec<DepartmentMemberBody>, ApiError> {
    let rows = sqlx::query_as::<_, (Uuid, String, String, String, String, Option<OffsetDateTime>)>(
        "select u.id, u.display_name, u.email, u.status, \
                coalesce(m.status, 'active'), \
                (select max(s.last_seen_at) from sessions s where s.user_id = u.id \
                   and s.revoked_at is null) \
           from department_members dm \
           join users u on u.id = dm.user_id \
           left join organization_members m \
             on m.organization_id = u.organization_id and m.user_id = u.id \
          where dm.department_id = $1 \
          order by u.display_name, u.id",
    )
    .bind(department_id)
    .fetch_all(pool)
    .await
    .map_err(store)?;

    Ok(rows
        .into_iter()
        .map(
            |(user_id, display_name, email, user_status, membership_status, last_active_at)| {
                DepartmentMemberBody {
                    user_id,
                    display_name,
                    email,
                    user_status,
                    membership_status,
                    last_active_at,
                }
            },
        )
        .collect())
}

/// The live bindings that name `department_key` — the same rows the Departments tab counts.
async fn role_bodies(
    pool: &sqlx::PgPool,
    organization_id: Uuid,
    department_key: &str,
) -> Result<Vec<DepartmentRoleBody>, ApiError> {
    let rows = sqlx::query_as::<_, (Uuid, Uuid, String, String, Option<Uuid>, Option<OffsetDateTime>)>(
        "select b.id, r.id, r.key, r.name, b.granted_by, b.expires_at \
           from role_bindings b \
           join roles r on r.id = b.role_id \
          where b.scope_type = 'department' \
            and b.organization_id = $1 \
            and b.resource_id = $2 \
            and b.revoked_at is null \
            and (b.expires_at is null or b.expires_at > now()) \
          order by r.priority desc, r.name",
    )
    .bind(organization_id)
    .bind(department_key)
    .fetch_all(pool)
    .await
    .map_err(store)?;

    Ok(rows
        .into_iter()
        .map(
            |(binding_id, role_id, role_key, role_name, granted_by, expires_at)| {
                DepartmentRoleBody {
                    binding_id,
                    role_id,
                    role_key,
                    role_name,
                    granted_by,
                    expires_at,
                }
            },
        )
        .collect())
}

fn department_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "department_not_found",
        "no such department",
    )
}

fn store(err: sqlx::Error) -> ApiError {
    ApiError::from(IdentityError::Database(err))
}

/// Write one audit row. Departments are structure, so every change is worth a trail an
/// operator can read back when a role stops applying and nobody remembers why.
async fn record(state: &AppState, entry: NewAuditEntry) -> Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}

/// The parent department's key, when the department has a parent that is on this page.
///
/// A root has none, and a parent filtered out by the tab's search is reported as none too —
/// the tree still reads correctly because indentation comes from `depth`, which the query
/// already computed over the *unfiltered* tree.
fn parent_key_of(parents: &HashMap<Uuid, String>, parent_id: Option<Uuid>) -> Option<String> {
    parent_id.and_then(|id| parents.get(&id).cloned())
}
