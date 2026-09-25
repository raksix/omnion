//! Roles: creation, lookup and permission sets.

use std::collections::BTreeMap;

use sqlx::PgPool;
use uuid::Uuid;

use crate::catalogue;
use crate::error::{PermissionsError, Result};
use crate::model::{
    Effect, NewRole, PermissionSummary, Role, RolePermission, RolePermissionInput,
    validate_priority, validate_role_key, validate_role_name,
};

/// Column list for every `Role` query, so the row shape stays in one place.
const ROLE_COLUMNS: &str = "id, organization_id, key, name, description, priority, \
     inherits_role_id, inherit_permissions, is_system, created_at, updated_at";

/// Create a role an organization owns.
pub async fn create_role(pool: &PgPool, new: NewRole) -> Result<Role> {
    let organization_id = new.organization_id;
    let key = validate_role_key(&new.key)?;
    let name = validate_role_name(&new.name)?;
    let priority = validate_priority(new.priority)?;

    if let Some(parent_id) = new.inherits_role_id {
        let parent = find_role(pool, parent_id)
            .await?
            .ok_or(PermissionsError::RoleNotFound)?;
        // A customer role may inherit a platform role or a role of its own organization —
        // never a role that belongs to somebody else.
        let allowed =
            parent.organization_id.is_none() || parent.organization_id == Some(organization_id);
        if !allowed {
            return Err(PermissionsError::CrossOrganizationInheritance);
        }
    }

    let sql = format!(
        "insert into roles (organization_id, key, name, description, priority, inherits_role_id) \
         values ($1, $2, $3, $4, $5, $6) returning {ROLE_COLUMNS}"
    );

    sqlx::query_as::<_, Role>(&sql)
        .bind(organization_id)
        .bind(&key)
        .bind(&name)
        .bind(new.description.trim())
        .bind(priority)
        .bind(new.inherits_role_id)
        .fetch_one(pool)
        .await
        .map_err(map_role_insert_error)
}

/// Insert a role row for the platform seed.
pub(crate) async fn insert_system_role(
    pool: &PgPool,
    key: &str,
    name: &str,
    description: &str,
    priority: i32,
) -> Result<Role> {
    let sql = format!(
        "insert into roles (organization_id, key, name, description, priority, is_system) \
         values (null, $1, $2, $3, $4, true) returning {ROLE_COLUMNS}"
    );

    sqlx::query_as::<_, Role>(&sql)
        .bind(key)
        .bind(name)
        .bind(description)
        .bind(priority)
        .fetch_one(pool)
        .await
        .map_err(map_role_insert_error)
}

/// Look a role up by id.
pub async fn find_role(pool: &PgPool, id: Uuid) -> Result<Option<Role>> {
    let sql = format!("select {ROLE_COLUMNS} from roles where id = $1");
    sqlx::query_as::<_, Role>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// Look a role up by key. `organization_id = None` searches the platform roles.
pub async fn find_role_by_key(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    key: &str,
) -> Result<Option<Role>> {
    let sql = format!(
        "select {ROLE_COLUMNS} from roles \
         where key = $1 and (organization_id is not distinct from $2)"
    );
    sqlx::query_as::<_, Role>(&sql)
        .bind(key)
        .bind(organization_id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// Platform roles plus the roles of one organization, highest priority first.
pub async fn list_roles(pool: &PgPool, organization_id: Option<Uuid>) -> Result<Vec<Role>> {
    let sql = format!(
        "select {ROLE_COLUMNS} from roles \
         where organization_id is null or organization_id = $1 \
         order by priority desc, key asc"
    );
    sqlx::query_as::<_, Role>(&sql)
        .bind(organization_id)
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

/// The permission entries of several roles at once, keyed by role.
pub async fn permission_entries(
    pool: &PgPool,
    role_ids: &[Uuid],
) -> Result<BTreeMap<Uuid, Vec<RolePermission>>> {
    if role_ids.is_empty() {
        return Ok(BTreeMap::new());
    }

    #[derive(sqlx::FromRow)]
    struct EntryRow {
        role_id: Uuid,
        permission_key: String,
        effect: String,
    }

    let rows: Vec<EntryRow> = sqlx::query_as(
        "select role_id, permission_key, effect from role_permissions \
         where role_id = any($1) order by permission_key asc",
    )
    .bind(role_ids)
    .fetch_all(pool)
    .await?;

    let mut grouped: BTreeMap<Uuid, Vec<RolePermission>> = BTreeMap::new();
    for row in rows {
        let entry = RolePermission {
            key: row.permission_key,
            effect: Effect::from_stored(&row.effect)?,
        };
        grouped.entry(row.role_id).or_default().push(entry);
    }

    Ok(grouped)
}

/// Replace a role's permission set with `entries` (docs/07-IAM.md §5).
///
/// Platform roles are managed by the platform, so editing one is refused — an organization
/// that needs different rules creates its own role and inherits from a base role.
pub async fn set_role_permissions(
    pool: &PgPool,
    role_id: Uuid,
    entries: &[RolePermissionInput],
) -> Result<Vec<RolePermission>> {
    let role = find_role(pool, role_id)
        .await?
        .ok_or(PermissionsError::RoleNotFound)?;
    if role.is_system {
        return Err(PermissionsError::SystemRole);
    }

    // Validate and de-duplicate before touching the database: one row per key.
    let mut validated: BTreeMap<String, Effect> = BTreeMap::new();
    for entry in entries {
        catalogue::expect_known(&entry.key)?;
        validated.insert(entry.key.clone(), entry.effect);
    }

    let mut tx = pool.begin().await?;
    sqlx::query("delete from role_permissions where role_id = $1")
        .bind(role_id)
        .execute(&mut *tx)
        .await?;

    for (key, effect) in &validated {
        sqlx::query(
            "insert into role_permissions (role_id, permission_key, effect) values ($1, $2, $3)",
        )
        .bind(role_id)
        .bind(key)
        .bind(effect.as_str())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    sqlx::query("update roles set updated_at = now() where id = $1")
        .bind(role_id)
        .execute(pool)
        .await?;

    let mut entries = permission_entries(pool, &[role_id]).await?;
    Ok(entries.remove(&role_id).unwrap_or_default())
}

/// Ensure one permission entry exists on a role (used when the catalogue grows).
pub(crate) async fn ensure_allow_entry(
    pool: &PgPool,
    role_id: Uuid,
    permission_key: &str,
) -> Result<bool> {
    let inserted = sqlx::query(
        "insert into role_permissions (role_id, permission_key, effect) values ($1, $2, 'allow') \
         on conflict (role_id, permission_key) do nothing",
    )
    .bind(role_id)
    .bind(permission_key)
    .execute(pool)
    .await?
    .rows_affected()
        > 0;

    Ok(inserted)
}

/// Allow/deny counts for a set of roles.
pub async fn permission_summary(
    pool: &PgPool,
    role_ids: &[Uuid],
) -> Result<BTreeMap<Uuid, PermissionSummary>> {
    if role_ids.is_empty() {
        return Ok(BTreeMap::new());
    }

    #[derive(sqlx::FromRow)]
    struct SummaryRow {
        role_id: Uuid,
        allowed: i64,
        denied: i64,
    }

    let rows: Vec<SummaryRow> = sqlx::query_as(
        "select role_id, \
                count(*) filter (where effect = 'allow') as allowed, \
                count(*) filter (where effect = 'deny') as denied \
         from role_permissions where role_id = any($1) group by role_id",
    )
    .bind(role_ids)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.role_id,
                PermissionSummary {
                    allowed: row.allowed,
                    denied: row.denied,
                },
            )
        })
        .collect())
}

/// Attach a permission entry to a role (the seed path; customer edits go through
/// [`set_role_permissions`]).
pub(crate) async fn add_entry(
    pool: &PgPool,
    role_id: Uuid,
    key: &str,
    effect: Effect,
) -> Result<()> {
    sqlx::query(
        "insert into role_permissions (role_id, permission_key, effect) values ($1, $2, $3) \
         on conflict (role_id, permission_key) do nothing",
    )
    .bind(role_id)
    .bind(key)
    .bind(effect.as_str())
    .execute(pool)
    .await?;

    Ok(())
}

fn map_role_insert_error(err: sqlx::Error) -> PermissionsError {
    match err {
        sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
            PermissionsError::RoleKeyTaken
        }
        other => PermissionsError::Database(other),
    }
}
