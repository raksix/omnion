//! `/api/v1/iam` — subjects, scopes, groups, machine identities and the simulator
//! (REQ-006, slice 2).
//!
//! Slice 1 built the role side of the IAM surface; this module adds everything a role is
//! attached to: the user list and its detail tabs, bindings at every scope level with expiry,
//! groups and their membership, service accounts with their keys, the overview and the RBAC
//! simulator. The simulator answers with the verdict of the same resolution the route guard
//! runs — `omnion_permissions::simulate` explains, it never decides on its own.
//!
//! Cross-tenant rule, unchanged: an account with a primary organization works only inside it;
//! a platform account may name any organization.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_permissions::model::{NewSubjectBinding, ResourceContext, Scope, Subject};
use omnion_permissions::{
    PermissionsError, bindings, groups as group_store, service_accounts, simulate as simulator,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::auth::{ApiCaller, CurrentSession};
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::iam::{BindingBody, ScopeBody, record};
use crate::scope::{ensure_same_organization, resolve_organization};
use crate::state::AppState;

/// Largest page the user list answers.
const MAX_USER_PAGE: i64 = 200;

/// How many live bindings the overview counts as "expiring soon".
const EXPIRING_WINDOW_DAYS: i64 = 7;

// ---------------------------------------------------------------------------------------------
// Users
// ---------------------------------------------------------------------------------------------

/// One role chip on a user row.
#[derive(Debug, Clone, Serialize)]
pub struct RoleChipView {
    /// The role.
    pub role_id: Uuid,
    /// Role key.
    pub key: String,
    /// Role name.
    pub name: String,
    /// Where it applies.
    pub scope: ScopeBody,
    /// Whether the binding came through a group (the chip's `via`).
    pub via: &'static str,
    /// When a temporary binding runs out.
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
}

/// A row of the user list.
#[derive(Debug, Serialize)]
pub struct UserRowView {
    /// Account id.
    pub id: Uuid,
    /// E-mail.
    pub email: String,
    /// Display name.
    pub display_name: String,
    /// `active`, `invited` or `disabled`.
    pub status: String,
    /// Primary organization (`null` = platform account).
    pub organization_id: Option<Uuid>,
    /// Whether MFA is enforced for the account.
    pub mfa_enforced: bool,
    /// Last successful sign-in.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_sign_in_at: Option<OffsetDateTime>,
    /// Consecutive failed sign-ins.
    pub failed_sign_in_count: i32,
    /// Lockout end, when the account is locked.
    #[serde(with = "time::serde::rfc3339::option")]
    pub locked_until: Option<OffsetDateTime>,
    /// ABAC subject attributes.
    pub attributes: serde_json::Value,
    /// Creation timestamp.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Live role chips (own bindings plus the ones inherited through groups).
    pub roles: Vec<RoleChipView>,
    /// How many groups the account belongs to.
    pub group_count: i64,
}

/// Response body of `GET /api/v1/iam/users`.
#[derive(Debug, Serialize)]
pub struct UsersResponse {
    /// The page.
    pub users: Vec<UserRowView>,
    /// How many accounts the filter matched, ignoring the page.
    pub total: i64,
}

/// Query parameters of `GET /api/v1/iam/users`.
#[derive(Debug, Deserialize)]
pub struct UsersQuery {
    /// Free text over e-mail and display name.
    #[serde(default)]
    pub search: Option<String>,
    /// `active`, `invited` or `disabled`.
    #[serde(default)]
    pub status: Option<String>,
    /// Only accounts of this organization.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// Only accounts with this MFA requirement.
    #[serde(default)]
    pub mfa: Option<bool>,
    /// Only accounts that (directly or through a group) carry this role.
    #[serde(default)]
    pub role_id: Option<Uuid>,
    /// Page size (default 100).
    #[serde(default)]
    pub limit: Option<i64>,
    /// Page offset.
    #[serde(default)]
    pub offset: Option<i64>,
}

/// Row shape of the user list query.
#[derive(sqlx::FromRow)]
struct UserListRow {
    id: Uuid,
    email: String,
    display_name: String,
    status: String,
    organization_id: Option<Uuid>,
    mfa_enforced: bool,
    last_sign_in_at: Option<OffsetDateTime>,
    failed_sign_in_count: i32,
    locked_until: Option<OffsetDateTime>,
    attributes: serde_json::Value,
    created_at: OffsetDateTime,
    group_count: i64,
}

/// List accounts.
pub async fn list_users(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<UsersQuery>,
) -> Result<Json<UsersResponse>, ApiError> {
    let pool = state.db().pool();
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

    let search = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let limit = query.limit.unwrap_or(100).clamp(1, MAX_USER_PAGE);
    let offset = query.offset.unwrap_or(0).max(0);

    let rows: Vec<UserListRow> = sqlx::query_as(
        "select u.id, u.email, u.display_name, u.status, u.organization_id, u.mfa_enforced, \
                u.last_sign_in_at, u.failed_sign_in_count, u.locked_until, u.attributes, \
                u.created_at, \
                (select count(*) from group_members m where m.user_id = u.id) as group_count \
         from users u \
         where ($1::text is null or (u.email ilike '%' || $1 || '%' \
                or u.display_name ilike '%' || $1 || '%')) \
           and ($2::text is null or u.status = $2) \
           and ($3::uuid is null or u.organization_id = $3) \
           and ($4::boolean is null or u.mfa_enforced = $4) \
           and ($5::uuid is null or exists ( \
                select 1 from role_bindings b where b.role_id = $5 and b.revoked_at is null \
                  and (b.expires_at is null or b.expires_at > now()) \
                  and ((b.subject_type = 'user' and b.subject_id = u.id) \
                    or (b.subject_type = 'group' and b.subject_id in ( \
                          select group_id from group_members m where m.user_id = u.id))))) \
         order by u.created_at desc, u.id desc \
         limit $6 offset $7",
    )
    .bind(search)
    .bind(query.status.as_deref())
    .bind(organization_id)
    .bind(query.mfa)
    .bind(query.role_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
    .map_err(PermissionsError::Database)?;

    let total: i64 = sqlx::query_scalar(
        "select count(*) from users u \
         where ($1::text is null or (u.email ilike '%' || $1 || '%' \
                or u.display_name ilike '%' || $1 || '%')) \
           and ($2::text is null or u.status = $2) \
           and ($3::uuid is null or u.organization_id = $3) \
           and ($4::boolean is null or u.mfa_enforced = $4) \
           and ($5::uuid is null or exists ( \
                select 1 from role_bindings b where b.role_id = $5 and b.revoked_at is null \
                  and (b.expires_at is null or b.expires_at > now()) \
                  and ((b.subject_type = 'user' and b.subject_id = u.id) \
                    or (b.subject_type = 'group' and b.subject_id in ( \
                          select group_id from group_members m where m.user_id = u.id)))))",
    )
    .bind(search)
    .bind(query.status.as_deref())
    .bind(organization_id)
    .bind(query.mfa)
    .bind(query.role_id)
    .fetch_one(pool)
    .await
    .map_err(PermissionsError::Database)?;

    let ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let chips = role_chips(pool, &ids).await?;

    Ok(Json(UsersResponse {
        users: rows
            .into_iter()
            .map(|row| UserRowView {
                roles: chips.get(&row.id).cloned().unwrap_or_default(),
                id: row.id,
                email: row.email,
                display_name: row.display_name,
                status: row.status,
                organization_id: row.organization_id,
                mfa_enforced: row.mfa_enforced,
                last_sign_in_at: row.last_sign_in_at,
                failed_sign_in_count: row.failed_sign_in_count,
                locked_until: row.locked_until,
                attributes: row.attributes,
                created_at: row.created_at,
                group_count: row.group_count,
            })
            .collect(),
        total,
    }))
}

/// Live role chips of the listed accounts, own bindings plus the ones through their groups.
async fn role_chips(
    pool: &sqlx::PgPool,
    user_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, Vec<RoleChipView>>, ApiError> {
    #[derive(sqlx::FromRow)]
    struct ChipRow {
        subject_type: String,
        subject_id: Uuid,
        role_id: Uuid,
        key: String,
        name: String,
        scope_type: String,
        organization_id: Option<Uuid>,
        site_id: Option<Uuid>,
        resource_type: Option<String>,
        resource_id: Option<String>,
        expires_at: Option<OffsetDateTime>,
        member: Option<Uuid>,
    }

    if user_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    let rows: Vec<ChipRow> = sqlx::query_as(
        "select b.subject_type, b.subject_id, b.role_id, r.key, r.name, b.scope_type, \
                b.organization_id, b.site_id, b.resource_type, b.resource_id, b.expires_at, \
                m.user_id as member \
         from role_bindings b \
         join roles r on r.id = b.role_id \
         left join group_members m on b.subject_type = 'group' and m.group_id = b.subject_id \
         where b.revoked_at is null and (b.expires_at is null or b.expires_at > now()) \
           and (b.subject_type = 'user' and b.subject_id = any($1) \
                or (b.subject_type = 'group' and m.user_id = any($1))) \
         order by r.priority desc, r.key asc",
    )
    .bind(user_ids)
    .fetch_all(pool)
    .await
    .map_err(PermissionsError::Database)?;

    let mut map: std::collections::HashMap<Uuid, Vec<RoleChipView>> =
        std::collections::HashMap::new();
    for row in rows {
        let scope = Scope::from_parts(
            &row.scope_type,
            row.organization_id,
            row.site_id,
            row.resource_type.as_deref(),
            row.resource_id.as_deref(),
        )?;
        let chip = RoleChipView {
            role_id: row.role_id,
            key: row.key,
            name: row.name,
            scope: scope.into(),
            via: if row.subject_type == "group" {
                "group"
            } else {
                "direct"
            },
            expires_at: row.expires_at,
        };
        match row.subject_type.as_str() {
            // A group binding speaks for every member it matched.
            "group" => {
                if let Some(member) = row.member {
                    map.entry(member).or_default().push(chip);
                } else {
                    // The join returned no member (the group's own listing) — attach to nobody.
                    let _ = chip;
                }
            }
            _ => {
                map.entry(row.subject_id).or_default().push(chip);
            }
        }
    }

    // One chip per role: a direct binding and a group binding of the same role collapse.
    for chips in map.values_mut() {
        let mut seen: Vec<Uuid> = Vec::new();
        chips.retain(|chip| {
            if seen.contains(&chip.role_id) {
                return false;
            }
            seen.push(chip.role_id);
            true
        });
    }

    Ok(map)
}

/// `POST /api/v1/iam/users` — create an account.
#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    /// E-mail address.
    pub email: String,
    /// Display name.
    #[serde(default)]
    pub display_name: String,
    /// Organization the account belongs to; required for a platform account.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// Initial password; without one the account is created as `invited`.
    #[serde(default)]
    pub password: Option<String>,
    /// Role to attach right away.
    #[serde(default)]
    pub role_id: Option<Uuid>,
    /// Scope of the attached role (`global` for a platform role).
    #[serde(default)]
    pub role_scope_type: Option<String>,
    /// Expiry of the attached role as RFC 3339.
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// The account's own bindings, groups and the resulting picture — the detail screen.
pub async fn get_user(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(user_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = state.db().pool();
    let user = omnion_identity::users::find_by_id(pool, user_id)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "account_not_found",
                "no such account",
            )
        })?;

    if current.user.organization_id.is_some() {
        ensure_same_organization(&current, user.organization_id)?;
    }

    // The profile fields the editor needs live on `users` beside the columns the identity crate
    // models, so they are read here rather than widening the shared `User` shape.
    #[derive(sqlx::FromRow)]
    struct ProfileFlags {
        mfa_enforced: bool,
        attributes: serde_json::Value,
    }

    let flags: ProfileFlags =
        sqlx::query_as("select mfa_enforced, attributes from users where id = $1")
            .bind(user_id)
            .fetch_one(pool)
            .await
            .map_err(PermissionsError::Database)?;

    let own = bindings::bindings_of_subject(pool, Subject::User(user_id)).await?;
    let groups = group_store::member_groups(pool, user_id).await?;

    let mut group_views = Vec::new();
    for group_id in groups {
        if let Some(group) = group_store::find(pool, group_id).await? {
            group_views.push(json!({
                "id": group.id,
                "name": group.name,
                "slug": group.slug,
            }));
        }
    }

    Ok(Json(json!({
        "user": {
            "id": user.id,
            "email": user.email,
            "display_name": user.display_name,
            "status": user.status,
            "organization_id": user.organization_id,
            "mfa_enforced": flags.mfa_enforced,
            "attributes": flags.attributes,
            "created_at": user.created_at.format(&Rfc3339).unwrap_or_default(),
        },
        "bindings": own.iter().map(BindingBody::from).collect::<Vec<_>>(),
        "groups": group_views,
    })))
}

/// `PATCH /api/v1/iam/users/{id}`.
#[derive(Debug, Deserialize)]
pub struct UpdateUserRequest {
    /// New display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// New status: `active`, `invited` or `disabled`.
    #[serde(default)]
    pub status: Option<String>,
    /// New primary organization.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// Whether MFA is required for the account.
    #[serde(default)]
    pub mfa_enforced: Option<bool>,
    /// Replacement ABAC attributes (an object).
    #[serde(default)]
    pub attributes: Option<serde_json::Value>,
}

/// Update an account's profile, status, MFA requirement and attributes.
pub async fn update_user(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(user_id): Path<Uuid>,
    Json(body): Json<UpdateUserRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = state.db().pool();
    let user = omnion_identity::users::find_by_id(pool, user_id)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "account_not_found",
                "no such account",
            )
        })?;

    if current.user.organization_id.is_some() {
        ensure_same_organization(&current, user.organization_id)?;
    }

    if let Some(status) = body.status.as_deref()
        && !matches!(status, "active" | "invited" | "disabled")
    {
        return Err(ApiError::bad_request(
            "invalid_status",
            "status must be active, invited or disabled",
        ));
    }

    if let Some(display_name) = body.display_name.as_deref()
        && display_name.trim().is_empty()
    {
        return Err(ApiError::bad_request(
            "invalid_display_name",
            "a display name cannot be empty",
        ));
    }

    if let Some(attributes) = &body.attributes
        && !attributes.is_object()
    {
        return Err(ApiError::bad_request(
            "invalid_attributes",
            "attributes must be a JSON object",
        ));
    }

    if let Some(organization_id) = body.organization_id {
        ensure_same_organization(&current, Some(organization_id))?;
        resolve_organization(&current, Some(organization_id))?;
    }

    #[derive(sqlx::FromRow)]
    struct UpdatedUser {
        id: Uuid,
        email: String,
        display_name: String,
        status: String,
        organization_id: Option<Uuid>,
        mfa_enforced: bool,
        attributes: serde_json::Value,
    }

    let updated: UpdatedUser = sqlx::query_as(
        "update users set \
            display_name = coalesce($2, display_name), \
            status = coalesce($3, status), \
            organization_id = case when $4::uuid is null then organization_id else $4 end, \
            mfa_enforced = coalesce($5, mfa_enforced), \
            attributes = coalesce($6, attributes), \
            updated_at = now() \
         where id = $1 \
         returning id, email, display_name, status, organization_id, mfa_enforced, attributes",
    )
    .bind(user_id)
    .bind(body.display_name.as_deref().map(str::trim))
    .bind(body.status.as_deref())
    .bind(body.organization_id)
    .bind(body.mfa_enforced)
    .bind(body.attributes.clone())
    .fetch_one(pool)
    .await
    .map_err(PermissionsError::Database)?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.user_updated")
            .target("user", user_id.to_string())
            .metadata(json!({
                "changed": {
                    "display_name": body.display_name.is_some(),
                    "status": body.status,
                    "organization_id": body.organization_id,
                    "mfa_enforced": body.mfa_enforced,
                    "attributes": body.attributes.is_some(),
                }
            }))
            .ip_address(address.as_text())
            .organization(updated.organization_id),
    )
    .await?;

    Ok(Json(json!({
        "id": updated.id,
        "email": updated.email,
        "display_name": updated.display_name,
        "status": updated.status,
        "organization_id": updated.organization_id,
        "mfa_enforced": updated.mfa_enforced,
        "attributes": updated.attributes,
    })))
}

/// Create an account (invited or with a password) and optionally attach a role in the same call.
pub async fn create_user(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let pool = state.db().pool();
    let organization_id = match resolve_organization(&current, body.organization_id) {
        Ok(organization_id) => Some(organization_id),
        // A platform account may create another platform account by naming no organization.
        Err(error) if body.organization_id.is_none() && current.user.organization_id.is_none() => {
            let _ = error;
            None
        }
        Err(error) => return Err(error),
    };

    let email = omnion_identity::users::normalize_email(&body.email)?;

    let (user_id, status) = match body.password.as_deref() {
        Some(password) => {
            let user = omnion_identity::users::create_user(
                pool,
                omnion_identity::users::NewUser {
                    email: email.clone(),
                    password: password.to_owned(),
                    display_name: body.display_name.clone(),
                    organization_id,
                },
            )
            .await?;
            (user.id, user.status)
        }
        None => {
            let inserted: Uuid = sqlx::query_scalar(
                "insert into users (email, display_name, organization_id, status) \
                 values ($1, $2, $3, 'invited') returning id",
            )
            .bind(&email)
            .bind(body.display_name.trim())
            .bind(organization_id)
            .fetch_one(pool)
            .await
            .map_err(|err| match err {
                sqlx::Error::Database(ref db_error) if db_error.is_unique_violation() => {
                    ApiError::new(
                        StatusCode::CONFLICT,
                        "email_taken",
                        "this address already has an account",
                    )
                }
                other => PermissionsError::Database(other).into(),
            })?;
            (inserted, "invited".to_owned())
        }
    };

    if let Some(role_id) = body.role_id {
        let scope =
            scope_for_user_binding(&current, organization_id, body.role_scope_type.as_deref())?;
        let expires_at = match body.expires_at.as_deref() {
            Some(value) => Some(OffsetDateTime::parse(value, &Rfc3339).map_err(|_| {
                ApiError::bad_request("invalid_expiry", "expires_at must be an RFC 3339 timestamp")
            })?),
            None => None,
        };
        let new = NewSubjectBinding {
            role_id,
            subject: Subject::User(user_id),
            scope,
            granted_by: Some(current.user.id),
            expires_at,
        };
        bindings::validate_subject(pool, &new).await?;
        bindings::grant_subject(pool, new).await?;
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.user_created")
            .target("user", user_id.to_string())
            .metadata(json!({ "email": email, "status": status, "role_id": body.role_id }))
            .ip_address(address.as_text())
            .organization(organization_id),
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": user_id,
            "email": email,
            "status": status,
            "organization_id": organization_id,
        })),
    ))
}

/// The scope a role gets when it is attached to a freshly created account.
fn scope_for_user_binding(
    current: &CurrentSession,
    organization_id: Option<Uuid>,
    requested: Option<&str>,
) -> Result<Scope, ApiError> {
    let organization_id = organization_id.or(current.user.organization_id);
    match requested.unwrap_or("organization") {
        "global" => Ok(Scope::Global),
        "organization" => match organization_id {
            Some(organization_id) => Ok(Scope::Organization { organization_id }),
            None => Err(ApiError::bad_request(
                "invalid_scope",
                "an organization scope needs an organization",
            )),
        },
        other => Err(ApiError::bad_request(
            "invalid_scope",
            format!("unknown scope type {other:?} — expected global or organization"),
        )),
    }
}

// ---------------------------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------------------------

/// A group row of the list.
#[derive(Debug, Serialize)]
pub struct GroupView {
    /// Group id.
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// URL-safe key.
    pub slug: String,
    /// What the group is for.
    pub description: String,
    /// Owning organization.
    pub organization_id: Uuid,
    /// Live members.
    pub member_count: i64,
    /// Live role bindings attached to the group.
    pub role_count: i64,
    /// Creation timestamp.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Response body of `GET /api/v1/iam/groups`.
#[derive(Debug, Serialize)]
pub struct GroupsResponse {
    /// The groups.
    pub groups: Vec<GroupView>,
    /// Organization the list was read for.
    pub organization_id: Uuid,
}

/// Query parameters of `GET /api/v1/iam/groups`.
#[derive(Debug, Deserialize)]
pub struct GroupsQuery {
    /// Organization to list; a platform account must name one.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// List the groups of an organization.
pub async fn list_groups(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<GroupsQuery>,
) -> Result<Json<GroupsResponse>, ApiError> {
    let organization_id = resolve_organization(&current, query.organization_id)?;
    let groups = group_store::list(state.db().pool(), organization_id).await?;

    Ok(Json(GroupsResponse {
        groups: groups
            .into_iter()
            .map(|summary| GroupView {
                id: summary.group.id,
                name: summary.group.name,
                slug: summary.group.slug,
                description: summary.group.description,
                organization_id: summary.group.organization_id,
                member_count: summary.member_count,
                role_count: summary.role_count,
                created_at: summary.group.created_at,
            })
            .collect(),
        organization_id,
    }))
}

/// `POST /api/v1/iam/groups`.
#[derive(Debug, Deserialize)]
pub struct CreateGroupRequest {
    /// Display name.
    pub name: String,
    /// What the group is for.
    #[serde(default)]
    pub description: String,
    /// Organization to create it in.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// Create a group.
pub async fn create_group(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<CreateGroupRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let organization_id = resolve_organization(&current, body.organization_id)?;
    let group = group_store::create(
        state.db().pool(),
        group_store::NewGroup {
            organization_id,
            name: body.name,
            description: body.description,
        },
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.group_created")
            .target("group", group.id.to_string())
            .metadata(json!({ "name": group.name, "slug": group.slug }))
            .ip_address(address.as_text())
            .organization(organization_id),
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": group.id,
            "name": group.name,
            "slug": group.slug,
            "description": group.description,
            "organization_id": group.organization_id,
        })),
    ))
}

/// Read one group with its members and attached roles.
pub async fn get_group(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(group_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = state.db().pool();
    let group = group_store::find(pool, group_id)
        .await?
        .ok_or(PermissionsError::GroupNotFound)?;
    ensure_same_organization(&current, Some(group.organization_id))?;

    let members = group_store::list_members(pool, group_id).await?;
    let roles = group_store::roles_of_group(pool, group_id).await?;

    Ok(Json(json!({
        "group": {
            "id": group.id,
            "name": group.name,
            "slug": group.slug,
            "description": group.description,
            "organization_id": group.organization_id,
        },
        "members": members.iter().map(|member| json!({
            "user_id": member.user_id,
            "email": member.email,
            "display_name": member.display_name,
            "status": member.status,
            "joined_at": member.created_at.format(&Rfc3339).unwrap_or_default(),
        })).collect::<Vec<_>>(),
        "roles": roles.iter().map(|binding| json!({
            "binding_id": binding.id,
            "role_id": binding.role_id,
            "scope": ScopeBody::from(binding.scope.clone()),
            "expires_at": binding.expires_at.map(|value| value.format(&Rfc3339).unwrap_or_default()),
        })).collect::<Vec<_>>(),
    })))
}

/// `PATCH /api/v1/iam/groups/{id}`.
#[derive(Debug, Deserialize)]
pub struct UpdateGroupRequest {
    /// New display name.
    #[serde(default)]
    pub name: Option<String>,
    /// New description.
    #[serde(default)]
    pub description: Option<String>,
}

/// Update a group.
pub async fn update_group(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(group_id): Path<Uuid>,
    Json(body): Json<UpdateGroupRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = state.db().pool();
    let group = group_store::find(pool, group_id)
        .await?
        .ok_or(PermissionsError::GroupNotFound)?;
    ensure_same_organization(&current, Some(group.organization_id))?;

    let updated = group_store::update(
        pool,
        group_id,
        body.name.as_deref(),
        body.description.as_deref(),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.group_updated")
            .target("group", group_id.to_string())
            .metadata(json!({ "name": updated.name, "slug": updated.slug }))
            .ip_address(address.as_text())
            .organization(updated.organization_id),
    )
    .await?;

    Ok(Json(json!({
        "id": updated.id,
        "name": updated.name,
        "slug": updated.slug,
        "description": updated.description,
    })))
}

/// Delete a group; the roles it carried stop applying immediately.
pub async fn delete_group(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(group_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = state.db().pool();
    let group = group_store::find(pool, group_id)
        .await?
        .ok_or(PermissionsError::GroupNotFound)?;
    ensure_same_organization(&current, Some(group.organization_id))?;

    let revoked = bindings::revoke_for_subject(pool, Subject::Group(group_id)).await?;
    let deleted = group_store::delete(pool, group_id).await?;
    if !deleted {
        return Err(PermissionsError::GroupNotFound.into());
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.group_deleted")
            .target("group", group_id.to_string())
            .metadata(json!({ "name": group.name, "revoked_bindings": revoked }))
            .ip_address(address.as_text())
            .organization(group.organization_id),
    )
    .await?;

    Ok(Json(
        json!({ "deleted": true, "revoked_bindings": revoked }),
    ))
}

/// `PUT /api/v1/iam/groups/{id}/members`.
#[derive(Debug, Deserialize)]
pub struct SetGroupMembersRequest {
    /// The complete membership the group should end up with.
    pub user_ids: Vec<Uuid>,
}

/// Replace the membership of a group.
pub async fn set_group_members(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(group_id): Path<Uuid>,
    Json(body): Json<SetGroupMembersRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = state.db().pool();
    let group = group_store::find(pool, group_id)
        .await?
        .ok_or(PermissionsError::GroupNotFound)?;
    ensure_same_organization(&current, Some(group.organization_id))?;

    // Every named account must exist and belong to the group's organization.
    for user_id in &body.user_ids {
        let user = omnion_identity::users::find_by_id(pool, *user_id)
            .await?
            .ok_or_else(|| {
                ApiError::bad_request("unknown_account", format!("no account with id {user_id}"))
            })?;
        if user.organization_id != Some(group.organization_id) {
            return Err(ApiError::bad_request(
                "cross_organization_member",
                "every member must belong to the group's organization",
            ));
        }
    }

    group_store::replace_members(pool, group_id, &body.user_ids, Some(current.user.id)).await?;
    let members = group_store::list_members(pool, group_id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.group_members_changed")
            .target("group", group_id.to_string())
            .metadata(json!({ "member_count": members.len() }))
            .ip_address(address.as_text())
            .organization(group.organization_id),
    )
    .await?;

    Ok(Json(json!({
        "group_id": group_id,
        "member_count": members.len(),
    })))
}

// ---------------------------------------------------------------------------------------------
// Service accounts
// ---------------------------------------------------------------------------------------------

/// A service account row of the list.
#[derive(Debug, Serialize)]
pub struct ServiceAccountView {
    /// Identity id.
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// What it is for.
    pub description: String,
    /// Display prefix.
    pub prefix: String,
    /// Owning organization.
    pub organization_id: Uuid,
    /// Whether the identity may authenticate.
    pub active: bool,
    /// Last time one of its keys authenticated.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_used_at: Option<OffsetDateTime>,
    /// Keys that can still authenticate.
    pub active_keys: i64,
    /// Live role bindings attached to it.
    pub role_count: i64,
    /// Creation timestamp.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Response body of `GET /api/v1/iam/service-accounts`.
#[derive(Debug, Serialize)]
pub struct ServiceAccountsResponse {
    /// The identities.
    pub service_accounts: Vec<ServiceAccountView>,
    /// Organization the list was read for.
    pub organization_id: Uuid,
}

/// Query parameters of `GET /api/v1/iam/service-accounts`.
#[derive(Debug, Deserialize)]
pub struct ServiceAccountsQuery {
    /// Organization to list; a platform account must name one.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// List machine identities.
pub async fn list_service_accounts(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<ServiceAccountsQuery>,
) -> Result<Json<ServiceAccountsResponse>, ApiError> {
    let organization_id = resolve_organization(&current, query.organization_id)?;
    let now = OffsetDateTime::now_utc();
    let accounts = service_accounts::list(state.db().pool(), organization_id).await?;

    Ok(Json(ServiceAccountsResponse {
        service_accounts: accounts
            .into_iter()
            .map(|summary| {
                let account = summary.account;
                let active = account.is_active_at(now);
                ServiceAccountView {
                    id: account.id,
                    name: account.name,
                    description: account.description,
                    prefix: account.prefix,
                    organization_id: account.organization_id,
                    active,
                    last_used_at: account.last_used_at,
                    active_keys: summary.active_keys,
                    role_count: summary.role_count,
                    created_at: account.created_at,
                }
            })
            .collect(),
        organization_id,
    }))
}

/// `POST /api/v1/iam/service-accounts`.
#[derive(Debug, Deserialize)]
pub struct CreateServiceAccountRequest {
    /// Display name.
    pub name: String,
    /// What it is for.
    #[serde(default)]
    pub description: String,
    /// Organization to create it in.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// Optional first key label; when given, a key is issued in the same call.
    #[serde(default)]
    pub key_label: Option<String>,
    /// Organization-level scope for a role attached right away.
    #[serde(default)]
    pub role_id: Option<Uuid>,
}

/// Create a machine identity, optionally with its first key.
pub async fn create_service_account(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<CreateServiceAccountRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let pool = state.db().pool();
    let organization_id = resolve_organization(&current, body.organization_id)?;

    let account = service_accounts::create(
        pool,
        service_accounts::NewServiceAccount {
            organization_id,
            name: body.name,
            description: body.description,
            created_by: Some(current.user.id),
        },
    )
    .await?;

    let mut key_token: Option<String> = None;
    if let Some(label) = body.key_label.as_deref() {
        let issued = service_accounts::issue_key(pool, account.id, label, None).await?;
        key_token = Some(issued.token);
    }

    if let Some(role_id) = body.role_id {
        let new = NewSubjectBinding {
            role_id,
            subject: Subject::ServiceAccount(account.id),
            scope: Scope::Organization { organization_id },
            granted_by: Some(current.user.id),
            expires_at: None,
        };
        bindings::validate_subject(pool, &new).await?;
        bindings::grant_subject(pool, new).await?;
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.serviceaccount_created")
            .target("service_account", account.id.to_string())
            .metadata(json!({
                "name": account.name,
                "prefix": account.prefix,
                "key_issued": key_token.is_some(),
                "role_id": body.role_id,
            }))
            .ip_address(address.as_text())
            .organization(organization_id),
    )
    .await?;

    // The key is shown once, here.
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": account.id,
            "name": account.name,
            "prefix": account.prefix,
            "organization_id": account.organization_id,
            "key": key_token,
        })),
    ))
}

/// Read one machine identity with its keys.
pub async fn get_service_account(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(account_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = state.db().pool();
    let account = service_accounts::find(pool, account_id)
        .await?
        .ok_or(PermissionsError::ServiceAccountNotFound)?;
    ensure_same_organization(&current, Some(account.organization_id))?;

    let keys = service_accounts::list_keys(pool, account_id).await?;
    let roles = bindings::bindings_of_subject(pool, Subject::ServiceAccount(account_id)).await?;
    let now = OffsetDateTime::now_utc();

    Ok(Json(json!({
        "account": {
            "id": account.id,
            "name": account.name,
            "description": account.description,
            "prefix": account.prefix,
            "organization_id": account.organization_id,
            "active": account.is_active_at(now),
            "last_used_at": account.last_used_at.map(|value| value.format(&Rfc3339).unwrap_or_default()),
        },
        "keys": keys.iter().map(|key| json!({
            "id": key.id,
            "prefix": key.prefix,
            "label": key.label,
            "active": key.is_active_at(now),
            "expires_at": key.expires_at.map(|value| value.format(&Rfc3339).unwrap_or_default()),
            "last_used_at": key.last_used_at.map(|value| value.format(&Rfc3339).unwrap_or_default()),
            "revoked_at": key.revoked_at.map(|value| value.format(&Rfc3339).unwrap_or_default()),
            "created_at": key.created_at.format(&Rfc3339).unwrap_or_default(),
        })).collect::<Vec<_>>(),
        "roles": roles.iter().map(BindingBody::from).collect::<Vec<_>>(),
    })))
}

/// Delete a machine identity; its keys and its role bindings go with it.
pub async fn delete_service_account(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(account_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = state.db().pool();
    let account = service_accounts::find(pool, account_id)
        .await?
        .ok_or(PermissionsError::ServiceAccountNotFound)?;
    ensure_same_organization(&current, Some(account.organization_id))?;

    let revoked = bindings::revoke_for_subject(pool, Subject::ServiceAccount(account_id)).await?;
    let deleted = service_accounts::delete(pool, account_id).await?;
    if !deleted {
        return Err(PermissionsError::ServiceAccountNotFound.into());
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.serviceaccount_deleted")
            .target("service_account", account_id.to_string())
            .metadata(json!({ "name": account.name, "revoked_bindings": revoked }))
            .ip_address(address.as_text())
            .organization(account.organization_id),
    )
    .await?;

    Ok(Json(
        json!({ "deleted": true, "revoked_bindings": revoked }),
    ))
}

/// `POST /api/v1/iam/service-accounts/{id}/keys`.
#[derive(Debug, Deserialize)]
pub struct IssueKeyRequest {
    /// Human label (`ci`, `backup`, …).
    #[serde(default)]
    pub label: String,
    /// Expiry as RFC 3339.
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// Issue a key; the token is returned exactly once.
pub async fn issue_service_account_key(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(account_id): Path<Uuid>,
    Json(body): Json<IssueKeyRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let pool = state.db().pool();
    let account = service_accounts::find(pool, account_id)
        .await?
        .ok_or(PermissionsError::ServiceAccountNotFound)?;
    ensure_same_organization(&current, Some(account.organization_id))?;

    let expires_at = match body.expires_at.as_deref() {
        Some(value) => Some(OffsetDateTime::parse(value, &Rfc3339).map_err(|_| {
            ApiError::bad_request("invalid_expiry", "expires_at must be an RFC 3339 timestamp")
        })?),
        None => None,
    };

    let issued = service_accounts::issue_key(pool, account_id, &body.label, expires_at).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.serviceaccount_key_issued")
            .target("service_account", account_id.to_string())
            .metadata(json!({
                "key_id": issued.key.id,
                "prefix": issued.key.prefix,
                "label": issued.key.label,
                "expires_at": expires_at.map(|value| value.to_string()),
            }))
            .ip_address(address.as_text())
            .organization(account.organization_id),
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": issued.key.id,
            "prefix": issued.key.prefix,
            "label": issued.key.label,
            "token": issued.token,
            "expires_at": expires_at.map(|value| value.format(&Rfc3339).unwrap_or_default()),
        })),
    ))
}

/// Revoke one key.
pub async fn revoke_service_account_key(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path((account_id, key_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = state.db().pool();
    let account = service_accounts::find(pool, account_id)
        .await?
        .ok_or(PermissionsError::ServiceAccountNotFound)?;
    ensure_same_organization(&current, Some(account.organization_id))?;

    let revoked = service_accounts::revoke_key(pool, account_id, key_id).await?;
    if !revoked {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "key_already_revoked",
            "this key was already revoked",
        ));
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.serviceaccount_key_revoked")
            .target("service_account", account_id.to_string())
            .metadata(json!({ "key_id": key_id }))
            .ip_address(address.as_text())
            .organization(account.organization_id),
    )
    .await?;

    Ok(Json(json!({ "revoked": true })))
}

// ---------------------------------------------------------------------------------------------
// Simulator
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/iam/simulations`.
#[derive(Debug, Deserialize)]
pub struct SimulationRequest {
    /// `user`, `group` or `service_account`.
    #[serde(default)]
    pub subject_type: Option<String>,
    /// Id of the subject; defaults to the caller.
    #[serde(default)]
    pub subject_id: Option<Uuid>,
    /// The permission to ask about.
    pub permission: String,
    /// Organization the question is asked in.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// Site the question is asked on.
    #[serde(default)]
    pub site_id: Option<Uuid>,
    /// Department context.
    #[serde(default)]
    pub department: Option<String>,
    /// Module context.
    #[serde(default)]
    pub module: Option<String>,
    /// Resource path (`/blog/hello`).
    #[serde(default)]
    pub path: Option<String>,
}

/// Answer one simulator query: the verdict plus the chain that produced it.
///
/// The route accepts a session or a service-account key (`Authorization: Bearer`), so a machine
/// can ask the decision path without a person in the loop.
pub async fn run_simulation(
    State(state): State<AppState>,
    caller: ApiCaller,
    Json(body): Json<SimulationRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = state.db().pool();

    // Without an explicit subject the question is about the caller: the account behind the
    // session, or the machine identity that presented the key.
    let default_subject = match &caller {
        ApiCaller::Session(session) => Subject::User(session.user.id),
        ApiCaller::Machine(machine) => machine.account,
    };
    let subject = match (body.subject_type.as_deref(), body.subject_id) {
        (None, None) => default_subject,
        (None, Some(id)) => Subject::User(id),
        (Some(kind), Some(id)) => match kind {
            "user" => Subject::User(id),
            "group" => Subject::Group(id),
            "service_account" => Subject::ServiceAccount(id),
            other => {
                return Err(ApiError::bad_request(
                    "invalid_subject",
                    format!(
                        "unknown subject type {other:?} — expected user, group or service_account"
                    ),
                ));
            }
        },
        (Some(_), None) => {
            return Err(ApiError::bad_request(
                "invalid_subject",
                "subject_id is required when subject_type is given",
            ));
        }
    };

    // A tenant account (or a machine) only asks questions inside its own organization.
    let organization_id = match caller.organization_id() {
        Some(own) => {
            if let Some(requested) = body.organization_id
                && requested != own
            {
                return Err(crate::scope::cross_organization());
            }
            Some(own)
        }
        None => body.organization_id,
    };

    let context = ResourceContext {
        organization_id,
        site_id: body.site_id,
        department: body.department.clone(),
        module: body.module.clone(),
        path: body.path.clone(),
    };

    let report = simulator::simulate(pool, subject, &body.permission, &context).await?;

    Ok(Json(json!({
        "allowed": report.allowed,
        "reason": report.reason,
        "subject": subject.describe(),
        "permission": body.permission,
        "context": {
            "organization_id": context.organization_id,
            "site_id": context.site_id,
            "department": context.department,
            "module": context.module,
            "path": context.path,
        },
        "source": report.source.as_ref().map(|source| json!({
            "role_id": source.role_id,
            "role_key": source.role_key,
            "role_name": source.role_name,
            "role_priority": source.role_priority,
            "via": source.via,
        })),
        "chain": report.steps.iter().map(|step| json!({
            "binding_id": step.binding_id,
            "role_id": step.role_id,
            "role_key": step.role_key,
            "role_name": step.role_name,
            "role_priority": step.role_priority,
            "subject": step.subject,
            "scope": step.scope,
            "state": step.state,
            "counts": step.counts,
            "effect": step.effect,
            "via": step.via,
            "inherited_from": step.inherited_from,
        })).collect::<Vec<_>>(),
        "considered": report.considered,
        "counted": report.counted,
        "note": report.note,
    })))
}

// ---------------------------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------------------------

/// The IAM overview: what exists, what runs out soon, what happened last.
pub async fn overview(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<GroupsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = state.db().pool();
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

    let users: i64 = sqlx::query_scalar(
        "select count(*) from users where ($1::uuid is null or organization_id = $1)",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await
    .map_err(PermissionsError::Database)?;

    let roles: i64 = sqlx::query_scalar(
        "select count(*) from roles where organization_id is null or organization_id = $1",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await
    .map_err(PermissionsError::Database)?;

    let groups: i64 = sqlx::query_scalar(
        "select count(*) from groups where ($1::uuid is null or organization_id = $1)",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await
    .map_err(PermissionsError::Database)?;

    let service_accounts_count: i64 = sqlx::query_scalar(
        "select count(*) from service_accounts where ($1::uuid is null or organization_id = $1)",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await
    .map_err(PermissionsError::Database)?;

    let live_bindings: i64 = sqlx::query_scalar(
        "select count(*) from role_bindings \
         where revoked_at is null and (expires_at is null or expires_at > now()) \
           and ($1::uuid is null or organization_id = $1 or scope_type = 'global')",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await
    .map_err(PermissionsError::Database)?;

    let expiring = bindings::expiring_within(pool, organization_id, EXPIRING_WINDOW_DAYS).await?;

    let mut role_names: std::collections::HashMap<Uuid, (String, String)> =
        std::collections::HashMap::new();
    let recent = omnion_audit::recent(pool, organization_id, 10).await?;

    let mut wanted: Vec<Uuid> = Vec::new();
    for binding in &expiring {
        if !wanted.contains(&binding.role_id) {
            wanted.push(binding.role_id);
        }
    }
    for role_id in wanted {
        if let Some(role) = omnion_permissions::roles::find_role(pool, role_id).await? {
            role_names.insert(role_id, (role.key, role.name));
        }
    }

    Ok(Json(json!({
        "organization_id": organization_id,
        "counts": {
            "users": users,
            "roles": roles,
            "groups": groups,
            "service_accounts": service_accounts_count,
            "live_bindings": live_bindings,
            "expiring_soon": expiring.len() as i64,
        },
        "expiring": expiring.iter().map(|binding| json!({
            "binding_id": binding.id,
            "role_id": binding.role_id,
            "role_key": role_names.get(&binding.role_id).map(|(key, _)| key.clone()),
            "role_name": role_names.get(&binding.role_id).map(|(_, name)| name.clone()),
            "subject": binding.subject.describe(),
            "scope": binding.scope.describe(),
            "expires_at": binding.expires_at.map(|value| value.format(&Rfc3339).unwrap_or_default()),
        })).collect::<Vec<_>>(),
        "recent": recent.iter().map(|entry| json!({
            "id": entry.id,
            "action": entry.action,
            "actor_user_id": entry.actor_user_id,
            "target_type": entry.target_type,
            "target_id": entry.target_id,
            "created_at": entry.created_at.format(&Rfc3339).unwrap_or_default(),
        })).collect::<Vec<_>>(),
    })))
}
