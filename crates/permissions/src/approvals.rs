//! Permission requests and their approvals (docs/07-IAM.md §16; REQ-006, slice 4b).
//!
//! A person who needs a permission they do not hold asks for it with a justification; an
//! approver grants a **window** or refuses. An approval is not a flag — it is a real, time-boxed
//! role binding: the request carries the binding it produced, the resolver stops counting the
//! grant the moment it runs out, and the row stays for the audit trail.
//!
//! The grant is modelled with a small generated role per (organization, permission key) pair —
//! one role, one allow entry, many time-boxed bindings. That keeps the promise of the request
//! ("this one permission, this one window") exactly, without a second permission model beside
//! the roles the platform already resolves.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::catalogue;
use crate::error::{PermissionsError, Result};
use crate::model::{Effect, NewRole, NewSubjectBinding, RolePermissionInput, Scope, Subject};
use crate::roles;

/// Shortest window an approval may grant (docs/07-IAM.md §16 range).
pub const MIN_GRANT_MINUTES: i32 = 5;

/// Longest window an approval may grant (30 days).
pub const MAX_GRANT_MINUTES: i32 = 43_200;

/// Priority of a generated grant role: below every base role, because a requested window must
/// never outrank an assigned one when priorities are compared.
const GRANT_ROLE_PRIORITY: i32 = 100;

/// Column list of every request query, joins included.
const REQUEST_COLUMNS: &str = "pr.id, pr.organization_id, pr.requester_id, pr.permission_key, \
     pr.resource_type, pr.resource_id, pr.justification, pr.status, pr.decided_by, pr.decided_at, \
     pr.decision_note, pr.grant_minutes, pr.binding_id, pr.created_at, \
     u.email as requester_email, u.display_name as requester_name, \
     d.email as decider_email, b.expires_at as grant_expires_at, \
     (b.revoked_at is not null) as grant_revoked";

/// The one join every request read shares.
const REQUEST_FROM: &str = "from permission_requests pr \
     join users u on u.id = pr.requester_id \
     left join users d on d.id = pr.decided_by \
     left join role_bindings b on b.id = pr.binding_id";

/// One permission request as the panel reads it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PermissionRequest {
    /// Primary key.
    pub id: Uuid,
    /// Organization the request belongs to.
    pub organization_id: Uuid,
    /// Who asked.
    pub requester_id: Uuid,
    /// Permission key asked for.
    pub permission_key: String,
    /// Resource kind the ask is limited to (`path`), when it is.
    pub resource_type: Option<String>,
    /// Resource pattern the ask is limited to (`/blog/*`), when it is.
    pub resource_id: Option<String>,
    /// Why it is needed.
    pub justification: String,
    /// `pending`, `approved`, `rejected` or `expired`.
    pub status: String,
    /// Who decided.
    pub decided_by: Option<Uuid>,
    /// When it was decided.
    pub decided_at: Option<OffsetDateTime>,
    /// The approver's note (absent until a decision carries one).
    pub decision_note: Option<String>,
    /// Window the approval granted, in minutes.
    pub grant_minutes: Option<i32>,
    /// Binding the approval produced.
    pub binding_id: Option<Uuid>,
    /// When it was asked.
    pub created_at: OffsetDateTime,
    /// E-mail of the requester.
    pub requester_email: String,
    /// Display name of the requester.
    pub requester_name: String,
    /// E-mail of the decider, when there is one.
    pub decider_email: Option<String>,
    /// When the granted binding runs out.
    pub grant_expires_at: Option<OffsetDateTime>,
    /// Whether the granted binding was revoked early.
    pub grant_revoked: Option<bool>,
}

/// What a request asks for.
#[derive(Debug, Clone)]
pub struct NewPermissionRequest {
    /// Exact permission key from the catalogue.
    pub permission_key: String,
    /// Optional resource kind (`path`).
    pub resource_type: Option<String>,
    /// Optional resource pattern (`/blog/*`).
    pub resource_id: Option<String>,
    /// Why it is needed.
    pub justification: String,
}

/// The approver's decision.
#[derive(Debug, Clone)]
pub struct ApprovalDecision {
    /// `true` grants a window, `false` refuses.
    pub approve: bool,
    /// Window length in minutes; required for an approval.
    pub grant_minutes: Option<i32>,
    /// Note the approver leaves.
    pub note: String,
}

/// Filter of the request list.
#[derive(Debug, Clone, Default)]
pub struct RequestFilter {
    /// Only this status, when set (`pending`, `approved`, `rejected`, `expired`).
    pub status: Option<String>,
    /// Only requests of this account (the "my requests" view).
    pub requester_id: Option<Uuid>,
}

/// Open a request. The permission key must exist in the catalogue; the shape of a resource is
/// checked here so an unusable ask never becomes a pending row.
pub async fn request(
    pool: &PgPool,
    organization_id: Uuid,
    requester_id: Uuid,
    new: &NewPermissionRequest,
) -> Result<PermissionRequest> {
    let permission_key = new.permission_key.trim().to_owned();
    if !catalogue::is_known(&permission_key) {
        return Err(PermissionsError::UnknownPermission(permission_key));
    }

    let justification = new.justification.trim().to_owned();
    if justification.chars().count() > 500 {
        return Err(PermissionsError::InvalidRequest(
            "the justification is longer than 500 characters".to_owned(),
        ));
    }

    let (resource_type, resource_id) = match (
        new.resource_type.as_deref().map(str::trim),
        new.resource_id.as_deref().map(str::trim),
    ) {
        (Some(kind), Some(pattern)) if !kind.is_empty() && !pattern.is_empty() => {
            if kind != "path" {
                return Err(PermissionsError::InvalidRequest(format!(
                    "unknown resource kind `{kind}`"
                )));
            }
            (Some(kind.to_owned()), Some(pattern.to_owned()))
        }
        (None, None) => (None, None),
        _ => {
            return Err(PermissionsError::InvalidRequest(
                "a resource needs both a kind (`path`) and a pattern".to_owned(),
            ));
        }
    };

    let sql = format!(
        "with inserted as ( \
           insert into permission_requests \
             (organization_id, requester_id, permission_key, resource_type, resource_id, \
              justification) \
           values ($1, $2, $3, $4, $5, $6) returning * \
         ) select {REQUEST_COLUMNS} \
         from inserted pr \
         join users u on u.id = pr.requester_id \
         left join users d on d.id = pr.decided_by \
         left join role_bindings b on b.id = pr.binding_id"
    );

    sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(requester_id)
        .bind(&permission_key)
        .bind(resource_type.as_deref())
        .bind(resource_id.as_deref())
        .bind(&justification)
        .fetch_one(pool)
        .await
        .map_err(PermissionsError::from)
}

/// The organization's requests, newest first. Lapsed approvals are retired first, so a reader
/// never sees an `approved` row whose window is already over.
pub async fn list(
    pool: &PgPool,
    organization_id: Uuid,
    filter: &RequestFilter,
) -> Result<Vec<PermissionRequest>> {
    expire_lapsed(pool, organization_id).await?;

    let status = match filter.status.as_deref() {
        Some("all") | None => None,
        Some(value @ ("pending" | "approved" | "rejected" | "expired")) => Some(value.to_owned()),
        Some(other) => {
            return Err(PermissionsError::InvalidRequest(format!(
                "unknown status filter `{other}`"
            )));
        }
    };

    let sql = format!(
        "select {REQUEST_COLUMNS} {REQUEST_FROM} where pr.organization_id = $1 \
         and ($2::text is null or pr.status = $2) \
         and ($3::uuid is null or pr.requester_id = $3) \
         order by pr.created_at desc limit 200"
    );

    sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(status)
        .bind(filter.requester_id)
        .fetch_all(pool)
        .await
        .map_err(PermissionsError::from)
}

/// One request by id.
pub async fn find(pool: &PgPool, id: Uuid) -> Result<Option<PermissionRequest>> {
    let sql = format!("select {REQUEST_COLUMNS} {REQUEST_FROM} where pr.id = $1");
    sqlx::query_as(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(PermissionsError::from)
}

/// Retire approvals whose window has run out: the grant stops counting on its own, and the
/// request says `expired` instead of claiming an access that no longer exists.
pub async fn expire_lapsed(pool: &PgPool, organization_id: Uuid) -> Result<u64> {
    let result = sqlx::query(
        "update permission_requests pr set status = 'expired' \
         where pr.organization_id = $1 and pr.status = 'approved' and pr.binding_id is not null \
           and exists (select 1 from role_bindings b \
                       where b.id = pr.binding_id \
                         and b.expires_at is not null and b.expires_at <= now())",
    )
    .bind(organization_id)
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}

/// Decide a request: approval produces a time-boxed binding, refusal just closes the row.
///
/// The two writes happen in one transaction, so an approval can never exist without the grant
/// it promises, and a grant can never exist without the request that asked for it.
pub async fn decide(
    pool: &PgPool,
    id: Uuid,
    decider_id: Uuid,
    decision: &ApprovalDecision,
) -> Result<PermissionRequest> {
    let note = decision.note.trim().to_owned();

    let minutes = if decision.approve {
        let minutes = decision.grant_minutes.ok_or_else(|| {
            PermissionsError::InvalidRequest(
                "an approval needs a window (grant_minutes)".to_owned(),
            )
        })?;
        if !(MIN_GRANT_MINUTES..=MAX_GRANT_MINUTES).contains(&minutes) {
            return Err(PermissionsError::InvalidRequest(format!(
                "the window must be between {MIN_GRANT_MINUTES} and {MAX_GRANT_MINUTES} minutes"
            )));
        }
        Some(minutes)
    } else {
        None
    };

    // Load the row and its permission first: the grant role is created outside the transaction
    // (it is idempotent and shared by every request for the same permission).
    let existing = find(pool, id)
        .await?
        .ok_or(PermissionsError::RequestNotFound)?;
    if existing.status != "pending" {
        return Err(PermissionsError::RequestAlreadyDecided);
    }

    let grant_role_id = if decision.approve {
        Some(ensure_grant_role(pool, existing.organization_id, &existing.permission_key).await?)
    } else {
        None
    };

    let mut tx = pool.begin().await?;

    // Re-check under the row lock: two approvers pressing at once cannot both succeed.
    let status: String =
        sqlx::query_scalar("select status from permission_requests where id = $1 for update")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(PermissionsError::RequestNotFound)?;
    if status != "pending" {
        return Err(PermissionsError::RequestAlreadyDecided);
    }

    let binding_id = match (decision.approve, grant_role_id, minutes) {
        (true, Some(role_id), Some(minutes)) => {
            let expires_at =
                OffsetDateTime::now_utc() + time::Duration::minutes(i64::from(minutes));
            let scope = match (
                existing.resource_type.as_deref(),
                existing.resource_id.as_deref(),
            ) {
                (Some(kind), Some(pattern)) => Scope::Resource {
                    organization_id: Some(existing.organization_id),
                    site_id: None,
                    resource_type: kind.to_owned(),
                    resource_id: pattern.to_owned(),
                },
                _ => Scope::Organization {
                    organization_id: existing.organization_id,
                },
            };

            // The same insert `bindings::grant_subject` runs, inside this transaction so the
            // decision and its grant commit together. Expired rows of the same combination are
            // retired first, because the liveness index keeps working on `revoked_at is null`.
            sqlx::query(
                "update role_bindings set revoked_at = now() \
                 where role_id = $1 and subject_type = 'user' and subject_id = $2 \
                   and revoked_at is null and expires_at is not null and expires_at <= now()",
            )
            .bind(role_id)
            .bind(existing.requester_id)
            .execute(&mut *tx)
            .await?;

            let binding: Uuid = sqlx::query_scalar(
                "insert into role_bindings (role_id, user_id, subject_type, subject_id, \
                 scope_type, organization_id, site_id, resource_type, resource_id, granted_by, \
                 expires_at) \
                 values ($1, $2, 'user', $2, $3, $4, $5, $6, $7, $8, $9) returning id",
            )
            .bind(role_id)
            .bind(existing.requester_id)
            .bind(scope.scope_type())
            .bind(scope.organization_id())
            .bind(scope.site_id())
            .bind(scope.resource_type())
            .bind(scope.resource_id())
            .bind(decider_id)
            .bind(expires_at)
            .fetch_one(&mut *tx)
            .await?;

            Some(binding)
        }
        _ => None,
    };

    let status = if decision.approve {
        "approved"
    } else {
        "rejected"
    };
    sqlx::query(
        "update permission_requests set status = $2, decided_by = $3, decided_at = now(), \
         decision_note = $4, grant_minutes = $5, binding_id = $6 where id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(decider_id)
    .bind(&note)
    .bind(minutes)
    .bind(binding_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    find(pool, id)
        .await?
        .ok_or(PermissionsError::RequestNotFound)
}

/// The generated role that carries one requested permission.
///
/// One per (organization, permission key): the same permission asked for twice reuses it, and
/// every approval is pinned to its own time-boxed binding. Idempotent and race-safe — a lost
/// insert race re-reads the row the winner wrote.
pub async fn ensure_grant_role(
    pool: &PgPool,
    organization_id: Uuid,
    permission_key: &str,
) -> Result<Uuid> {
    let key = grant_role_key(permission_key);

    if let Some(role) = roles::find_role_by_key(pool, Some(organization_id), &key).await? {
        return Ok(role.id);
    }

    let name = format!("Time-boxed: {permission_key}");
    match roles::create_role(
        pool,
        NewRole {
            organization_id,
            key: key.clone(),
            name,
            description: "Generated by an approved permission request; bound only for the \
                          approved window."
                .to_owned(),
            priority: GRANT_ROLE_PRIORITY,
            inherits_role_id: None,
        },
    )
    .await
    {
        Ok(role) => {
            roles::set_role_permissions(
                pool,
                role.id,
                &[RolePermissionInput {
                    key: permission_key.to_owned(),
                    effect: Effect::Allow,
                }],
            )
            .await?;
            Ok(role.id)
        }
        Err(PermissionsError::RoleKeyTaken) => {
            // Another approver created it a moment ago.
            roles::find_role_by_key(pool, Some(organization_id), &key)
                .await?
                .map(|role| role.id)
                .ok_or(PermissionsError::RoleNotFound)
        }
        Err(other) => Err(other),
    }
}

/// Role key of a requested permission: `users.read` → `grant-users-read`.
#[must_use]
pub fn grant_role_key(permission_key: &str) -> String {
    let mut sanitized = String::with_capacity(permission_key.len() + 6);
    sanitized.push_str("grant-");
    // The literal already ends in a separator, so a following separator-character is a no-op.
    let mut last_dash = true;
    for character in permission_key.chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            sanitized.push(character);
            last_dash = false;
        } else if !last_dash {
            sanitized.push('-');
            last_dash = true;
        }
    }
    while sanitized.ends_with('-') {
        sanitized.pop();
    }
    sanitized
}

/// A binding to create for an approval (kept beside the SQL for the tests that read it back).
#[must_use]
pub fn grant_binding(
    role_id: Uuid,
    requester_id: Uuid,
    scope: Scope,
    decider_id: Uuid,
    expires_at: OffsetDateTime,
) -> NewSubjectBinding {
    NewSubjectBinding {
        role_id,
        subject: Subject::User(requester_id),
        scope,
        granted_by: Some(decider_id),
        expires_at: Some(expires_at),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grant_role_key_is_a_valid_role_key() {
        assert_eq!(grant_role_key("users.read"), "grant-users-read");
        assert_eq!(
            grant_role_key("iam.policies.manage"),
            "grant-iam-policies-manage"
        );
        assert_eq!(
            grant_role_key("analytics.settings.manage"),
            "grant-analytics-settings-manage"
        );
        assert!(crate::model::validate_role_key(&grant_role_key("users.read")).is_ok());
    }

    #[test]
    fn the_window_range_matches_the_documented_bounds() {
        assert_eq!(MIN_GRANT_MINUTES, 5);
        assert_eq!(MAX_GRANT_MINUTES, 43_200);
    }
}
