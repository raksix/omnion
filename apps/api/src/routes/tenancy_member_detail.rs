//! `GET /api/v1/organizations/{id}/members/{user_id}` and
//! `POST/DELETE /api/v1/organizations/{id}/members/{user_id}/role-bindings` — the member
//! drawer (docs/requests/REQ-005, slice 4).
//!
//! The Members tab lists; the drawer is where an administrator *acts on one person*. The REQ
//! names what it has to carry — identity, membership status, role bindings with scope and
//! expiry, department memberships, and that member's recent audit — and the QA plan opens it
//! and extends a binding. Until this module existed, none of the three binding operations the
//! spec lists ("add / extend / revoke") was reachable from the tenant screen: IAM owns the
//! generic `/iam/bindings` surface, which is a platform surface and deliberately does not speak
//! a tenant's member.
//!
//! Three decisions this module depends on:
//!
//! * **One read, one screen.** The drawer needs four different slices of the truth and a person
//!   looking at a colleague must not see a half-filled panel, so the handler joins them into a
//!   single response rather than making the panel fire four requests and reconcile.
//! * **Every write is scoped to the organization in the path.** The handler resolves the tenant
//!   through [`organization_in_scope`] first, so a member id from another tenant is a `404`
//!   rather than a cross-tenant grant — the isolation rule the whole request is built on.
//! * **Revoke is a revoke, not a delete.** `role_bindings` rows are history; revoking sets
//!   `revoked_at`, and the drawer's audit section is the only place that says so, because a
//!   panel that silently erased grants would make the trail lie.
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_permissions::model::{NewSubjectBinding, RoleBinding};
use omnion_permissions::{Subject, bindings, roles as role_store};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::tenancy_departments::MemberDepartmentBody;
use crate::routes::tenancy_members::{organization_in_scope, organization_in_scope_for_write};
use crate::state::AppState;

/// How many audit rows the drawer's trail shows.
///
/// Ten is not an arbitrary number: it is enough to answer "what has this person been doing in
/// this tenant lately" without turning a side panel into the Audit tab, which the organization
/// already has for the full trail.
const DRAWER_AUDIT_LIMIT: i64 = 10;

/// How many bindings the drawer lists. A person in a real installation holds a handful; a
/// hundred-row list in a side panel is a sign the binding table is the thing to audit, not the
/// thing to scroll.
const DRAWER_BINDING_LIMIT: i64 = 100;

// ---------------------------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------------------------

/// One role binding, as the drawer shows it.
#[derive(Debug, Clone, Serialize)]
pub struct MemberBindingBody {
    /// Binding id — what `revoke` and `extend` name.
    pub id: Uuid,
    /// The role.
    pub role_id: Uuid,
    /// Role key.
    pub role_key: String,
    /// Role name.
    pub role_name: String,
    /// Who holds it (`user`; the drawer is a person).
    pub subject_type: String,
    /// Who it was granted to.
    pub subject_id: Uuid,
    /// Where it applies, in the scope vocabulary (`global`, `organization`, `site`, …).
    pub scope_type: String,
    /// The organization of the scope, when it names one.
    pub organization_id: Option<Uuid>,
    /// The site of the scope, when it names one.
    pub site_id: Option<Uuid>,
    /// The department key, for a department scope.
    pub department: Option<String>,
    /// The module key, for a module scope.
    pub module: Option<String>,
    /// The resource pattern, for a resource scope.
    pub resource_id: Option<String>,
    /// When the binding stops applying, RFC 3339.
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    /// When it was revoked, RFC 3339.
    #[serde(with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<OffsetDateTime>,
    /// Whether it still applies right now.
    pub active: bool,
    /// Whether a temporary window has run out.
    pub expired: bool,
    /// When it was granted, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// One audit row, as the drawer's trail shows it.
#[derive(Debug, Clone, Serialize)]
pub struct MemberAuditBody {
    /// The action, as recorded.
    pub action: String,
    /// The account that performed it, when there was one.
    pub actor_user_id: Option<Uuid>,
    /// Their display name, or `system` for a row nobody performed.
    pub actor_name: String,
    /// Kind of the target.
    pub target_type: Option<String>,
    /// Identifier of the target.
    pub target_id: Option<String>,
    /// When it happened, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Response of `GET /api/v1/organizations/{id}/members/{user_id}`.
#[derive(Debug, Serialize)]
pub struct MemberDetailResponse {
    /// The organization the member belongs to.
    pub organization_id: Uuid,
    /// The membership row id.
    pub membership_id: Uuid,
    /// The account.
    pub user_id: Uuid,
    /// Display name.
    pub display_name: String,
    /// Address.
    pub email: String,
    /// Account status (`active`, `invited`, `disabled`).
    pub user_status: String,
    /// Membership status (`active`, `invited`, `suspended`).
    pub status: String,
    /// Whether this is the account's home organization.
    pub is_primary: bool,
    /// When the membership became real.
    #[serde(with = "time::serde::rfc3339::option")]
    pub joined_at: Option<OffsetDateTime>,
    /// When the account was last seen.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_active_at: Option<OffsetDateTime>,
    /// The bindings this person holds inside this organization, live ones first.
    pub bindings: Vec<MemberBindingBody>,
    /// The departments they sit in.
    pub departments: Vec<MemberDepartmentBody>,
    /// Their most recent acts in this tenant, newest first.
    pub recent_audit: Vec<MemberAuditBody>,
}

/// `POST /api/v1/organizations/{id}/members/{user_id}/role-bindings`.
#[derive(Debug, Deserialize)]
pub struct GrantMemberRoleRequest {
    /// The role to grant.
    pub role_id: Uuid,
    /// Where it applies: `global`, `organization`, `site` or `department`. A member binding
    /// defaults to `organization`, because "this person may do this inside this tenant" is the
    /// only shape a tenant administrator can safely hand out.
    #[serde(default)]
    pub scope_type: Option<String>,
    /// Site of the scope, for a `site` scope.
    #[serde(default)]
    pub site_id: Option<Uuid>,
    /// Department key, for a `department` scope.
    #[serde(default)]
    pub department: Option<String>,
    /// Optional expiry, RFC 3339 — a temporary grant.
    #[serde(default)]
    pub expires_at: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/organizations/{id}/members/{user_id}` — everything the member drawer renders.
///
/// The organization is resolved first, so a member of another tenant is a `404` before any of
/// this person's rows is read: the drawer must not be a way to learn that a user id exists
/// somewhere else.
pub async fn get_member(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((organization_id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<MemberDetailResponse>, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;

    let detail = member_detail(&state, organization.id, user_id).await?;
    Ok(Json(detail))
}

/// `POST /api/v1/organizations/{id}/members/{user_id}/role-bindings` — grant a role.
///
/// The grant is refused unless the *member* is in this tenant. Granting a role to somebody who
/// is not a member is how a tenant ends up with a binding it can never explain — the role would
/// sit in the table applying to nobody, and the effective-permissions screen would show it as
/// noise rather than as a mistake.
pub async fn grant_member_role(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path((organization_id, user_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<GrantMemberRoleRequest>,
) -> Result<(StatusCode, Json<MemberBindingBody>), ApiError> {
    let organization = organization_in_scope_for_write(&state, &current, organization_id).await?;

    // The member has to be here. This is the assertion that makes the drawer safe to open from
    // any tenant, and it is checked before the role is even looked at.
    member_detail(&state, organization.id, user_id).await?;

    let expires_at = match body.expires_at.as_deref() {
        Some(raw) => Some(OffsetDateTime::parse(
            raw,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|_| {
            ApiError::bad_request(
                "invalid_expiry",
                format!("{raw:?} is not an RFC 3339 timestamp"),
            )
        })?),
        None => None,
    };

    let scope = scope_for_member(
        organization.id,
        body.scope_type.as_deref(),
        body.site_id,
        body.department.as_deref(),
    )?;

    // A role is only grantable inside the organization that owns it. Granting tenant A's role to
    // tenant B's member would produce a binding that resolves in a tenant whose catalogue does
    // not carry the role — a grant nobody can exercise and nobody can revoke cleanly.
    let role = role_store::find_role(state.db().pool(), body.role_id)
        .await?
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "role_not_found", "no such role")
        })?;
    if role.organization_id != Some(organization.id) {
        return Err(ApiError::forbidden(
            "cross_organization",
            "this role belongs to another organization",
        ));
    }

    let new = NewSubjectBinding {
        role_id: role.id,
        subject: Subject::User(user_id),
        scope: scope.clone(),
        granted_by: Some(current.user.id),
        expires_at,
    };

    let pool = state.db().pool();
    bindings::validate_subject(pool, &new).await?;
    bindings::validate_scope_target(pool, &scope).await?;
    let binding = bindings::grant_subject(pool, new).await?;

    // The drawer, the effective-permissions screen and the tenant's own trail all have to agree
    // that this grant happened, so the audit row is written here rather than left to the IAM
    // surface's own route.
    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(current.user.id, "organization.member.role_changed")
            .target("user", user_id.to_string())
            .metadata(json!({
                "organization_id": organization.id,
                "role_id": role.id,
                "role_key": role.key,
                "scope": scope.describe(),
                "change": "granted",
                "binding_id": binding.id,
            }))
            .ip_address(address.as_text())
            .organization(organization.id),
    )
    .await?;

    bus::emit(
        pool,
        NewEvent::new("organization.member.role_changed")
            .organization(organization.id)
            .actor(current.user.id)
            .payload(json!({
                "user_id": user_id,
                "role_id": role.id,
                "scope": scope.describe(),
                "change": "granted",
            })),
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(binding_body(state.db().pool(), &binding).await?),
    ))
}

/// `DELETE /api/v1/organizations/{id}/members/{user_id}/role-bindings/{binding_id}` — revoke.
///
/// The binding is looked up *within this organization* rather than by id alone, so a binding id
/// from another tenant is the same `404` as a member from another tenant. Revoking across a
/// tenant boundary would be a write the tenant has no business making, and answering `200` for
/// it would confirm the id exists.
pub async fn revoke_member_role(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path((organization_id, user_id, binding_id)): Path<(Uuid, Uuid, Uuid)>,
) -> Result<Json<MemberBindingBody>, ApiError> {
    let organization = organization_in_scope_for_write(&state, &current, organization_id).await?;

    let binding = bindings::list(
        state.db().pool(),
        &bindings::BindingFilter {
            organization_id: Some(organization.id),
            subject: Some(Subject::User(user_id)),
            role_id: None,
            live_only: false,
            limit: DRAWER_BINDING_LIMIT,
        },
    )
    .await?
    .into_iter()
    .find(|row| row.id == binding_id)
    .ok_or_else(binding_not_found)?;

    if !bindings::revoke(state.db().pool(), binding.id).await? {
        return Err(binding_not_found());
    }

    let refreshed = bindings::list(
        state.db().pool(),
        &bindings::BindingFilter {
            organization_id: Some(organization.id),
            subject: Some(Subject::User(user_id)),
            role_id: None,
            live_only: false,
            limit: DRAWER_BINDING_LIMIT,
        },
    )
    .await?
    .into_iter()
    .find(|row| row.id == binding_id)
    .ok_or_else(binding_not_found)?;

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "organization.member.role_changed")
            .target("user", user_id.to_string())
            .metadata(json!({
                "organization_id": organization.id,
                "role_id": binding.role_id,
                "scope": binding.scope.describe(),
                "change": "revoked",
                "binding_id": binding.id,
            }))
            .ip_address(address.as_text())
            .organization(organization.id),
    )
    .await?;

    bus::emit(
        state.db().pool(),
        NewEvent::new("organization.member.role_changed")
            .organization(organization.id)
            .actor(current.user.id)
            .payload(json!({
                "user_id": user_id,
                "role_id": binding.role_id,
                "scope": binding.scope.describe(),
                "change": "revoked",
            })),
    )
    .await?;

    Ok(Json(binding_body(state.db().pool(), &refreshed).await?))
}

/// `PATCH /api/v1/organizations/{id}/members/{user_id}/role-bindings/{binding_id}` — extend.
///
/// "Extend" is the third operation the REQ names and the one the platform had no verb for. A
/// temporary grant (`expires_at`) could be created and revoked but never lengthened, so the only
/// way to give somebody another month was to revoke and re-grant — which loses the grant's
/// history and, worse, re-runs whatever the grant's creation side effect was.
///
/// It is deliberately a *patch of the same row* rather than a new row: the alternative produces
/// two live bindings for one role, and the effective-permissions screen would then show the
/// role twice with different windows, neither of which is the truth.
pub async fn extend_member_role(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path((organization_id, user_id, binding_id)): Path<(Uuid, Uuid, Uuid)>,
    Json(body): Json<ExtendMemberRoleRequest>,
) -> Result<Json<MemberBindingBody>, ApiError> {
    let organization = organization_in_scope_for_write(&state, &current, organization_id).await?;

    let expires_at = OffsetDateTime::parse(&body.expires_at, &time::format_description::well_known::Rfc3339)
        .map_err(|_| {
            ApiError::bad_request(
                "invalid_expiry",
                format!("{:?} is not an RFC 3339 timestamp", body.expires_at),
            )
        })?;

    // Only a window that ends in the future is an extension. Accepting a past date would let a
    // caller "extend" a grant into a state that is already expired, which reads as success and
    // leaves a row that does nothing.
    if expires_at <= OffsetDateTime::now_utc() {
        return Err(ApiError::bad_request(
            "expiry_in_the_past",
            "a temporary grant can only be extended to a later time",
        ));
    }

    let existing = bindings::list(
        state.db().pool(),
        &bindings::BindingFilter {
            organization_id: Some(organization.id),
            subject: Some(Subject::User(user_id)),
            role_id: None,
            live_only: false,
            limit: DRAWER_BINDING_LIMIT,
        },
    )
    .await?
    .into_iter()
    .find(|row| row.id == binding_id)
    .ok_or_else(binding_not_found)?;

    // A revoked binding is not extended. Re-opening it would let a row that the trail says was
    // revoked come back to life, and the audit would be telling a lie.
    if existing.revoked_at.is_some() {
        return Err(ApiError::bad_request(
            "binding_revoked",
            "this grant was revoked; grant the role again instead of extending it",
        ));
    }

    let updated = bindings::extend_expiry(state.db().pool(), binding_id, expires_at)
        .await?
        .ok_or_else(binding_not_found)?;

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "organization.member.role_changed")
            .target("user", user_id.to_string())
            .metadata(json!({
                "organization_id": organization.id,
                "role_id": existing.role_id,
                "scope": existing.scope.describe(),
                "change": "extended",
                "binding_id": binding_id,
                "expires_at": body.expires_at,
                "previous_expires_at": existing.expires_at.map(|value| value.to_string()),
            }))
            .ip_address(address.as_text())
            .organization(organization.id),
    )
    .await?;

    Ok(Json(binding_body(state.db().pool(), &updated).await?))
}

/// `PATCH /api/v1/organizations/{id}/members/{user_id}/role-bindings/{binding_id}`.
#[derive(Debug, Deserialize)]
pub struct ExtendMemberRoleRequest {
    /// The new expiry, RFC 3339. It must be later than now; a temporary grant cannot be
    /// extended backwards.
    pub expires_at: String,
}

// ---------------------------------------------------------------------------------------------
// Shared reads
// ---------------------------------------------------------------------------------------------

/// Everything the drawer renders for one member, joined from four places.
///
/// Each piece is fetched independently and the *whole* read fails if any of them does: a drawer
/// showing an identity and an empty binding list because one query failed is worse than a drawer
/// that says it could not load, because the empty list is indistinguishable from "this person
/// holds nothing".
async fn member_detail(
    state: &AppState,
    organization_id: Uuid,
    user_id: Uuid,
) -> Result<MemberDetailResponse, ApiError> {
    let pool = state.db().pool();
    let membership = omnion_identity::memberships::find_member(pool, organization_id, user_id)
        .await?
        .ok_or_else(member_not_found)?;

    let account = omnion_identity::users::find_by_id(pool, user_id)
        .await?
        .ok_or_else(member_not_found)?;

    let binding_rows = bindings::list(
        pool,
        &bindings::BindingFilter {
            organization_id: Some(organization_id),
            subject: Some(Subject::User(user_id)),
            role_id: None,
            live_only: false,
            limit: DRAWER_BINDING_LIMIT,
        },
    )
    .await?;

    let mut binding_bodies = Vec::with_capacity(binding_rows.len());
    for row in &binding_rows {
        binding_bodies.push(binding_body(pool, row).await?);
    }

    let departments = omnion_identity::departments::list_departments_of_user(
        pool,
        organization_id,
        user_id,
    )
    .await?
    .into_iter()
    .map(|row| MemberDepartmentBody {
        id: row.id,
        key: row.key,
        name: row.name,
        status: row.status,
    })
    .collect();

    // The member's own trail *in this tenant*. Filtering by the actor is the honest reading of
    // "this member's recent audit": a grant an administrator made *to* them is filed against
    // the administrator, and that row belongs on the administrator's drawer and in the
    // organization's Audit tab — which has both. Pulling it here too would show a colleague's
    // private trail in a panel that is otherwise about this one person.
    let recent_audit = member_audit(pool, organization_id, user_id).await?;

    // `last_active_at` is not a column on the membership: it is the newest live session of the
    // account, which is the same fact the Members tab shows, so the drawer cannot disagree with
    // the row it was opened from.
    let last_active_at = sqlx::query_scalar::<_, Option<OffsetDateTime>>(
        "select max(last_seen_at) from sessions where user_id = $1 and revoked_at is null",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .map_err(omnion_identity::IdentityError::Database)?;

    Ok(MemberDetailResponse {
        organization_id,
        membership_id: membership.id,
        user_id,
        display_name: if account.display_name.trim().is_empty() {
            account.email.clone()
        } else {
            account.display_name.clone()
        },
        email: account.email,
        user_status: account.status,
        status: membership.status,
        is_primary: membership.is_primary,
        joined_at: membership.joined_at,
        last_active_at,
        bindings: binding_bodies,
        departments,
        recent_audit,
    })
}

/// The member's most recent acts in one tenant, newest first.
async fn member_audit(
    pool: &sqlx::PgPool,
    organization_id: Uuid,
    user_id: Uuid,
) -> Result<Vec<MemberAuditBody>, ApiError> {
    let (rows, _) = omnion_audit::filtered(
        pool,
        &omnion_audit::AuditFilter {
            organization_id: Some(organization_id),
            actor_user_id: Some(user_id),
            action: None,
            actor_type: None,
            since: None,
        },
        DRAWER_AUDIT_LIMIT,
    )
    .await?;

    // `actor_name` is not a column on the row — the Audit tab resolves it from the accounts
    // table. Doing the same here is one extra query for ten rows, and it keeps the drawer from
    // rendering a blank name where the Audit tab renders a name.
    let names = actor_names(pool, &rows).await;

    Ok(rows
        .into_iter()
        .map(|row| {
            let actor_user_id = row.actor_user_id;
            let actor_name = actor_user_id
                .and_then(|id| names.get(&id).cloned())
                .or_else(|| Some("system".to_owned()))
                .unwrap_or_else(|| "system".to_owned());
            MemberAuditBody {
                action: row.action,
                actor_user_id,
                actor_name,
                target_type: row.target_type,
                target_id: row.target_id,
                created_at: row.created_at,
            }
        })
        .collect())
}

/// Display names for the accounts behind a page of audit rows.
async fn actor_names(
    pool: &sqlx::PgPool,
    rows: &[omnion_audit::AuditEntry],
) -> HashMap<Uuid, String> {
    let ids: Vec<Uuid> = rows.iter().filter_map(|row| row.actor_user_id).collect();
    if ids.is_empty() {
        return HashMap::new();
    }
    sqlx::query_as::<_, (Uuid, String)>("select id, display_name from users where id = any($1)")
        .bind(&ids)
        .fetch_all(pool)
        .await
        .map(|rows| rows.into_iter().collect())
        .unwrap_or_default()
}

/// One binding plus the role's key and name, which the drawer shows and the raw row does not
/// carry.
async fn binding_body(
    pool: &sqlx::PgPool,
    binding: &RoleBinding,
) -> Result<MemberBindingBody, ApiError> {
    let role = role_store::find_role(pool, binding.role_id).await?;
    let now = OffsetDateTime::now_utc();

    Ok(MemberBindingBody {
        id: binding.id,
        role_id: binding.role_id,
        role_key: role.as_ref().map_or_else(String::new, |row| row.key.clone()),
        role_name: role
            .as_ref()
            .map_or_else(|| "Unknown role".to_owned(), |row| row.name.clone()),
        subject_type: binding.subject.subject_type().to_owned(),
        subject_id: binding.subject.id(),
        scope_type: binding.scope.scope_type().to_owned(),
        organization_id: binding.scope.organization_id(),
        site_id: binding.scope.site_id(),
        department: match &binding.scope {
            omnion_permissions::model::Scope::Department { department, .. } => {
                Some(department.clone())
            }
            _ => None,
        },
        module: match &binding.scope {
            omnion_permissions::model::Scope::Module { module, .. } => Some(module.clone()),
            _ => None,
        },
        resource_id: match &binding.scope {
            omnion_permissions::model::Scope::Resource { resource_id, .. } => {
                Some(resource_id.clone())
            }
            _ => None,
        },
        expires_at: binding.expires_at,
        revoked_at: binding.revoked_at,
        active: binding.is_active_at(now),
        expired: binding.is_expired_at(now),
        created_at: binding.created_at,
    })
}

/// Build the scope of a member grant.
///
/// The default is `organization`: inside a tenant, "this person may do this here" is the shape
/// that cannot escape. `global` is refused rather than ignored — a tenant administrator who asks
/// for it is asking for something the platform owns, and silently downgrading the scope would
/// make the grant do something other than what the panel said.
fn scope_for_member(
    organization_id: Uuid,
    scope_type: Option<&str>,
    site_id: Option<Uuid>,
    department: Option<&str>,
) -> Result<omnion_permissions::model::Scope, ApiError> {
    use omnion_permissions::model::Scope;

    match scope_type.unwrap_or("organization") {
        "organization" => Ok(Scope::Organization { organization_id }),
        "site" => {
            let site_id = site_id.ok_or_else(|| {
                ApiError::bad_request(
                    "site_required",
                    "a site scope needs the site it applies to",
                )
            })?;
            Ok(Scope::Site {
                organization_id: Some(organization_id),
                site_id,
            })
        }
        "department" => {
            let department = department.ok_or_else(|| {
                ApiError::bad_request(
                    "department_required",
                    "a department scope needs the department key",
                )
            })?;
            Ok(Scope::Department {
                organization_id,
                department: department.to_owned(),
            })
        }
        other => Err(ApiError::bad_request(
            "unsupported_scope",
            format!(
                "{other:?} is not a scope a member grant can take; a tenant grants organization, \
                 site or department"
            ),
        )),
    }
}

fn member_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "member_not_found",
        "this account is not a member of the organization",
    )
}

fn binding_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "binding_not_found",
        "this member holds no such grant in this organization",
    )
}
