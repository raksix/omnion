//! Role bindings: attaching a role to an account at a scope.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{PermissionsError, Result};
use crate::model::{NewBinding, RoleBinding, Scope};
use crate::roles;

/// Column list for every binding query.
const BINDING_COLUMNS: &str = "id, role_id, user_id, scope_type, organization_id, site_id, \
     granted_by, expires_at, revoked_at, created_at";

/// Row shape of `role_bindings`; `scope_type`/`organization_id`/`site_id` fold into [`Scope`].
#[derive(sqlx::FromRow)]
struct BindingRow {
    id: Uuid,
    role_id: Uuid,
    user_id: Uuid,
    scope_type: String,
    organization_id: Option<Uuid>,
    site_id: Option<Uuid>,
    granted_by: Option<Uuid>,
    expires_at: Option<time::OffsetDateTime>,
    revoked_at: Option<time::OffsetDateTime>,
    created_at: time::OffsetDateTime,
}

impl BindingRow {
    fn into_binding(self) -> Result<RoleBinding> {
        let scope = Scope::from_parts(&self.scope_type, self.organization_id, self.site_id)?;
        Ok(RoleBinding {
            id: self.id,
            role_id: self.role_id,
            user_id: self.user_id,
            scope,
            granted_by: self.granted_by,
            expires_at: self.expires_at,
            revoked_at: self.revoked_at,
            created_at: self.created_at,
        })
    }
}

/// Assign a role to an account.
///
/// Fails with [`PermissionsError::AlreadyBound`] when the same role is already assigned to the
/// account at the same scope — including through a temporary binding that has not expired yet.
/// Expired rows are retired first, so a temporary role can be granted again afterwards.
pub async fn grant(pool: &PgPool, new: NewBinding) -> Result<RoleBinding> {
    retire_expired(pool, &new).await?;

    let sql = format!(
        "insert into role_bindings (role_id, user_id, scope_type, organization_id, site_id, \
         granted_by, expires_at) values ($1, $2, $3, $4, $5, $6, $7) returning {BINDING_COLUMNS}"
    );

    let row: BindingRow = sqlx::query_as(&sql)
        .bind(new.role_id)
        .bind(new.user_id)
        .bind(new.scope.scope_type())
        .bind(new.scope.organization_id())
        .bind(new.scope.site_id())
        .bind(new.granted_by)
        .bind(new.expires_at)
        .fetch_one(pool)
        .await
        .map_err(map_binding_error)?;

    row.into_binding()
}

/// Mark the expired rows of one (role, account, scope) combination as revoked.
///
/// The uniqueness index keeps working on `revoked_at is null`, so an expired temporary role
/// must be retired before the same combination can be granted again.
async fn retire_expired(pool: &PgPool, new: &NewBinding) -> Result<()> {
    sqlx::query(
        "update role_bindings set revoked_at = now() \
         where role_id = $1 and user_id = $2 and scope_type = $3 \
           and organization_id is not distinct from $4 \
           and site_id is not distinct from $5 \
           and revoked_at is null \
           and expires_at is not null and expires_at <= now()",
    )
    .bind(new.role_id)
    .bind(new.user_id)
    .bind(new.scope.scope_type())
    .bind(new.scope.organization_id())
    .bind(new.scope.site_id())
    .execute(pool)
    .await?;

    Ok(())
}

/// Assign a role unless the account already holds it at that scope (the seed path).
pub async fn grant_if_missing(pool: &PgPool, new: NewBinding) -> Result<Option<RoleBinding>> {
    match grant(pool, new).await {
        Ok(binding) => Ok(Some(binding)),
        Err(PermissionsError::AlreadyBound) => Ok(None),
        Err(other) => Err(other),
    }
}

/// Revoke a binding. Returns `true` when a live binding was revoked.
pub async fn revoke(pool: &PgPool, binding_id: Uuid) -> Result<bool> {
    let revoked = sqlx::query(
        "update role_bindings set revoked_at = now() where id = $1 and revoked_at is null",
    )
    .bind(binding_id)
    .execute(pool)
    .await?
    .rows_affected()
        > 0;

    Ok(revoked)
}

/// Every binding of an account, live ones first.
pub async fn list_for_user(pool: &PgPool, user_id: Uuid) -> Result<Vec<RoleBinding>> {
    let sql = format!(
        "select {BINDING_COLUMNS} from role_bindings where user_id = $1 \
         order by created_at desc, id desc"
    );

    let rows: Vec<BindingRow> = sqlx::query_as(&sql).bind(user_id).fetch_all(pool).await?;
    rows.into_iter().map(BindingRow::into_binding).collect()
}

/// Live bindings of an account that apply inside `scope`.
///
/// Platform bindings always apply; organization bindings apply inside their organization and
/// site bindings inside their site. Expired (temporary) and revoked rows never count.
pub async fn active_for_scope(
    pool: &PgPool,
    user_id: Uuid,
    scope: Scope,
) -> Result<Vec<RoleBinding>> {
    let sql = format!(
        "select {BINDING_COLUMNS} from role_bindings \
         where user_id = $1 \
           and revoked_at is null \
           and (expires_at is null or expires_at > now()) \
           and (scope_type = 'global' \
                or (scope_type = 'organization' and organization_id = $2) \
                or (scope_type = 'site' and site_id = $3)) \
         order by created_at desc, id desc"
    );

    let rows: Vec<BindingRow> = sqlx::query_as(&sql)
        .bind(user_id)
        .bind(scope.organization_id())
        .bind(scope.site_id())
        .fetch_all(pool)
        .await?;

    rows.into_iter().map(BindingRow::into_binding).collect()
}

/// How many live bindings carry a role (docs/07-IAM.md §20: the last Owner cannot be
/// removed).
pub async fn count_live_for_role(pool: &PgPool, role_id: Uuid) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from role_bindings \
         where role_id = $1 and revoked_at is null and (expires_at is null or expires_at > now())",
    )
    .bind(role_id)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Validate a binding before it is written: the role must exist and may only be used inside
/// its own organization, the account must exist, and a site scope must name a site that exists
/// and belongs to the scope's organization.
pub async fn validate(pool: &PgPool, new: &NewBinding) -> Result<()> {
    let role = roles::find_role(pool, new.role_id)
        .await?
        .ok_or(PermissionsError::RoleNotFound)?;

    if let (Some(role_org), Some(scope_org)) = (role.organization_id, new.scope.organization_id())
        && role_org != scope_org
    {
        return Err(PermissionsError::InvalidBinding(
            "the role belongs to another organization".to_owned(),
        ));
    }

    if let Scope::Site {
        organization_id,
        site_id,
    } = new.scope
    {
        let site_organization: Option<Uuid> =
            sqlx::query_scalar("select organization_id from sites where id = $1")
                .bind(site_id)
                .fetch_optional(pool)
                .await?;
        match site_organization {
            None => {
                return Err(PermissionsError::InvalidBinding("unknown site".to_owned()));
            }
            Some(site_organization)
                if organization_id
                    .is_some_and(|organization_id| organization_id != site_organization) =>
            {
                return Err(PermissionsError::InvalidBinding(
                    "the site belongs to another organization".to_owned(),
                ));
            }
            Some(_) => {}
        }
    }

    let account_exists: bool =
        sqlx::query_scalar("select exists (select 1 from users where id = $1)")
            .bind(new.user_id)
            .fetch_one(pool)
            .await?;
    if !account_exists {
        return Err(PermissionsError::InvalidBinding(
            "unknown account".to_owned(),
        ));
    }

    Ok(())
}

fn map_binding_error(err: sqlx::Error) -> PermissionsError {
    match err {
        sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
            PermissionsError::AlreadyBound
        }
        sqlx::Error::Database(ref db_err) if db_err.is_foreign_key_violation() => {
            PermissionsError::InvalidBinding("unknown role or account".to_owned())
        }
        other => PermissionsError::Database(other),
    }
}
