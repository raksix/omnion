//! Roles: creation, lookup and permission sets.

use std::collections::BTreeMap;

use sqlx::PgPool;
use uuid::Uuid;

use crate::catalogue;
use crate::error::{PermissionsError, Result};
use crate::model::{
    Effect, MAX_INHERITANCE_DEPTH, NewRole, ParentChange, PermissionSummary, Role, RolePermission,
    RolePermissionInput, RoleSaveOutcome, RoleUpdate, validate_priority, validate_role_key,
    validate_role_name,
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
        // The chain the new role would sit in must stay inside the depth limit. The role has no
        // id yet, so a fresh one stands in for it — nothing inherits from it, so only the walk
        // upward matters here.
        let links = parent_links(pool, Some(organization_id)).await?;
        validate_parent_change(&links, Uuid::new_v4(), Some(parent_id))?;
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
    // A base role declares allow; a row left behind with another effect (an older seed, a manual
    // edit) must not keep overriding it, so the conflict path repairs the effect.
    let changed = sqlx::query(
        "insert into role_permissions (role_id, permission_key, effect) values ($1, $2, 'allow') \
         on conflict (role_id, permission_key) do update set effect = 'allow' \
         where role_permissions.effect <> 'allow'",
    )
    .bind(role_id)
    .bind(permission_key)
    .execute(pool)
    .await?
    .rows_affected()
        > 0;

    Ok(changed)
}

/// Drop every entry of a system role that the code no longer declares.
///
/// The base roles are owned by the platform: their permission list is defined in
/// `seed.rs`, not edited by customers, so a key that disappeared from the code (or moved to a
/// lower role) must not stay behind as a live grant. Returns how many rows went away.
pub(crate) async fn prune_entries(pool: &PgPool, role_id: Uuid, keep: &[&str]) -> Result<u64> {
    let keep: Vec<String> = keep.iter().map(|key| (*key).to_owned()).collect();
    let removed = sqlx::query(
        "delete from role_permissions where role_id = $1 and permission_key <> all($2)",
    )
    .bind(role_id)
    .bind(&keep)
    .execute(pool)
    .await?
    .rows_affected();

    Ok(removed)
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

/// The parent links of every role visible in one scope, keyed by role id.
///
/// Shared by the cycle check and the depth check: the walk needs the whole graph in memory, and
/// a scope holds tens of roles, not thousands.
async fn parent_links(
    pool: &PgPool,
    organization_id: Option<Uuid>,
) -> Result<BTreeMap<Uuid, Option<Uuid>>> {
    let rows: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
        "select id, inherits_role_id from roles \
         where organization_id is null or organization_id = $1",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().collect())
}

/// Validate a parent change against the whole graph.
///
/// Refuses a chain that would close into a cycle — the role appearing anywhere above its new
/// parent — and one that grows past [`MAX_INHERITANCE_DEPTH`] in either direction, which is the
/// documented depth limit (docs/07-IAM.md §4).
pub fn validate_parent_change(
    links: &BTreeMap<Uuid, Option<Uuid>>,
    role_id: Uuid,
    parent: Option<Uuid>,
) -> Result<()> {
    // Walk up from the new parent: the role must not appear, and the chain must stay short.
    let mut cursor = parent;
    let mut steps = 0usize;
    let mut visited = std::collections::BTreeSet::new();
    while let Some(id) = cursor {
        if id == role_id || !visited.insert(id) {
            return Err(PermissionsError::InheritanceCycle);
        }
        steps += 1;
        if steps > MAX_INHERITANCE_DEPTH {
            return Err(PermissionsError::InheritanceDepthExceeded {
                max: MAX_INHERITANCE_DEPTH,
            });
        }
        cursor = links.get(&id).copied().flatten();
    }

    // Walk down from the role: a long descendant chain counts against the same limit.
    let mut children: BTreeMap<Uuid, Vec<Uuid>> = BTreeMap::new();
    for (child, parent_link) in links {
        if let Some(parent_id) = parent_link {
            children.entry(*parent_id).or_default().push(*child);
        }
    }
    let mut frontier: Vec<(Uuid, usize)> = vec![(role_id, 0)];
    let mut seen = std::collections::BTreeSet::new();
    while let Some((id, depth)) = frontier.pop() {
        if !seen.insert(id) || depth > MAX_INHERITANCE_DEPTH {
            return Err(PermissionsError::InheritanceDepthExceeded {
                max: MAX_INHERITANCE_DEPTH,
            });
        }
        for child in children.get(&id).into_iter().flatten() {
            frontier.push((*child, depth + 1));
        }
    }

    Ok(())
}

/// Update a role's own fields and its parent link.
///
/// Platform (system) roles are managed by the platform: an organization that needs different
/// rules duplicates a base role instead of editing it.
pub async fn update_role(pool: &PgPool, role_id: Uuid, update: RoleUpdate) -> Result<Role> {
    let role = find_role(pool, role_id)
        .await?
        .ok_or(PermissionsError::RoleNotFound)?;
    if role.is_system {
        return Err(PermissionsError::SystemRole);
    }

    let name = match update.name {
        Some(value) => Some(validate_role_name(&value)?),
        None => None,
    };
    let description = update.description.map(|value| value.trim().to_owned());
    let priority = match update.priority {
        Some(value) => Some(validate_priority(value)?),
        None => None,
    };

    let parent = match update.parent {
        ParentChange::Keep => role.inherits_role_id,
        ParentChange::Clear => None,
        ParentChange::Set(new_parent) => {
            if new_parent == role_id {
                return Err(PermissionsError::SelfInheritance);
            }
            let parent_role = find_role(pool, new_parent)
                .await?
                .ok_or(PermissionsError::RoleNotFound)?;
            let allowed = parent_role.organization_id.is_none()
                || parent_role.organization_id == role.organization_id;
            if !allowed {
                return Err(PermissionsError::CrossOrganizationInheritance);
            }
            let links =
                parent_links(pool, role.organization_id.or(parent_role.organization_id)).await?;
            validate_parent_change(&links, role_id, Some(new_parent))?;
            Some(new_parent)
        }
    };

    let sql = format!(
        "update roles set name = coalesce($2, name), description = coalesce($3, description), \
         priority = coalesce($4, priority), inherits_role_id = $5, \
         inherit_permissions = coalesce($6, inherit_permissions), updated_at = now() \
         where id = $1 returning {ROLE_COLUMNS}"
    );

    sqlx::query_as::<_, Role>(&sql)
        .bind(role_id)
        .bind(name)
        .bind(description)
        .bind(priority)
        .bind(parent)
        .bind(update.inherit_permissions)
        .fetch_one(pool)
        .await
        .map_err(Into::into)
}

/// Remove a role the organization owns.
///
/// Refused while the role still carries live bindings (a role that grants something cannot
/// vanish silently) and for platform roles. Roles that inherit from this one keep their link
/// cleared by the schema (`on delete set null`) — they lose the inherited set, not themselves.
pub async fn delete_role(pool: &PgPool, role_id: Uuid) -> Result<()> {
    let role = find_role(pool, role_id)
        .await?
        .ok_or(PermissionsError::RoleNotFound)?;
    if role.is_system {
        return Err(PermissionsError::SystemRole);
    }

    let live = crate::bindings::count_live_for_role(pool, role_id).await?;
    if live > 0 {
        return Err(PermissionsError::RoleHasBindings(live));
    }

    sqlx::query("delete from roles where id = $1")
        .bind(role_id)
        .execute(pool)
        .await?;

    Ok(())
}

/// Clone a role into `organization_id` under a new key and name.
///
/// The copy carries the source's description, priority, inheritance switch and permission set.
/// Its parent link is kept only when the parent is usable in the target organization (a platform
/// role or a role of that organization); otherwise the copy starts detached.
pub async fn duplicate_role(
    pool: &PgPool,
    source_id: Uuid,
    organization_id: Uuid,
    key: &str,
    name: &str,
) -> Result<Role> {
    let source = find_role(pool, source_id)
        .await?
        .ok_or(PermissionsError::RoleNotFound)?;
    let allowed =
        source.organization_id.is_none() || source.organization_id == Some(organization_id);
    if !allowed {
        return Err(PermissionsError::CrossOrganizationInheritance);
    }

    let key = validate_role_key(key)?;
    let name = validate_role_name(name)?;

    let parent = match source.inherits_role_id {
        Some(parent_id) => {
            let parent_role = find_role(pool, parent_id)
                .await?
                .ok_or(PermissionsError::RoleNotFound)?;
            let usable = parent_role.organization_id.is_none()
                || parent_role.organization_id == Some(organization_id);
            usable.then_some(parent_id)
        }
        None => None,
    };

    let entries = permission_entries(pool, &[source_id]).await?;
    let entries = entries.get(&source_id).cloned().unwrap_or_default();

    let mut tx = pool.begin().await?;
    let sql = format!(
        "insert into roles (organization_id, key, name, description, priority, \
         inherits_role_id, inherit_permissions) values ($1, $2, $3, $4, $5, $6, $7) \
         returning {ROLE_COLUMNS}"
    );
    let role: Role = sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(&key)
        .bind(&name)
        .bind(&source.description)
        .bind(source.priority)
        .bind(parent)
        .bind(source.inherit_permissions)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_role_insert_error)?;

    for entry in &entries {
        sqlx::query(
            "insert into role_permissions (role_id, permission_key, effect) values ($1, $2, $3)",
        )
        .bind(role.id)
        .bind(&entry.key)
        .bind(entry.effect.as_str())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    Ok(role)
}

/// The chain of roles above `role_id`, nearest parent first.
pub async fn ancestors(pool: &PgPool, role_id: Uuid) -> Result<Vec<Role>> {
    let mut chain = Vec::new();
    let mut cursor = find_role(pool, role_id)
        .await?
        .ok_or(PermissionsError::RoleNotFound)?
        .inherits_role_id;
    let mut visited = std::collections::BTreeSet::new();

    while let Some(id) = cursor {
        if !visited.insert(id) {
            break; // defensive: a cycle in stored data ends the walk instead of looping
        }
        let Some(role) = find_role(pool, id).await? else {
            break;
        };
        cursor = role.inherits_role_id;
        chain.push(role);
    }

    Ok(chain)
}

/// The roles that inherit from `role_id` directly.
pub async fn children(pool: &PgPool, role_id: Uuid) -> Result<Vec<Role>> {
    let sql = format!(
        "select {ROLE_COLUMNS} from roles where inherits_role_id = $1 order by priority desc, key asc"
    );
    sqlx::query_as(&sql)
        .bind(role_id)
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

/// Replace a role's permission set, validate it atomically and record a version.
///
/// This is the matrix save: unknown keys and duplicate entries fail the whole request before
/// anything is written, `expected_version` refuses a save built on a stale read, and the diff
/// against what the role held before is returned so the screen can show what it changed.
pub async fn replace_role_permissions(
    pool: &PgPool,
    role_id: Uuid,
    entries: &[RolePermissionInput],
    expected_version: Option<i32>,
    changed_by: Option<Uuid>,
) -> Result<RoleSaveOutcome> {
    let role = find_role(pool, role_id)
        .await?
        .ok_or(PermissionsError::RoleNotFound)?;
    if role.is_system {
        return Err(PermissionsError::SystemRole);
    }

    let mut unknown: Vec<String> = Vec::new();
    let mut duplicates: Vec<String> = Vec::new();
    let mut validated: BTreeMap<String, Effect> = BTreeMap::new();
    for entry in entries {
        if !catalogue::is_known(&entry.key) {
            unknown.push(entry.key.clone());
            continue;
        }
        if validated.insert(entry.key.clone(), entry.effect).is_some() {
            duplicates.push(entry.key.clone());
        }
    }
    unknown.sort();
    unknown.dedup();
    duplicates.sort();
    duplicates.dedup();
    if !unknown.is_empty() || !duplicates.is_empty() {
        return Err(PermissionsError::InvalidEntries {
            unknown,
            duplicates,
        });
    }

    if let Some(expected) = expected_version {
        let current = crate::versions::latest_version(pool, role_id)
            .await?
            .unwrap_or(0);
        if current != expected {
            return Err(PermissionsError::VersionConflict { expected, current });
        }
    }

    let before = permission_entries(pool, &[role_id])
        .await?
        .remove(&role_id)
        .unwrap_or_default();

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
    sqlx::query("update roles set updated_at = now() where id = $1")
        .bind(role_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let after: Vec<RolePermission> = validated
        .iter()
        .map(|(key, effect)| RolePermission {
            key: key.clone(),
            effect: *effect,
        })
        .collect();

    let version = crate::versions::record(pool, &role, &after, "permissions", changed_by).await?;
    let diff = crate::versions::diff(&before, &after);
    let updated = find_role(pool, role_id)
        .await?
        .ok_or(PermissionsError::RoleNotFound)?;

    Ok(RoleSaveOutcome {
        role: updated,
        entries: after,
        diff,
        version: version.version,
    })
}

fn map_role_insert_error(err: sqlx::Error) -> PermissionsError {
    match err {
        sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
            PermissionsError::RoleKeyTaken
        }
        other => PermissionsError::Database(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a link map from `(role, parent)` pairs.
    fn links(pairs: &[(Uuid, Option<Uuid>)]) -> BTreeMap<Uuid, Option<Uuid>> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn a_self_parent_is_refused() {
        let role = Uuid::new_v4();
        let graph = links(&[(role, None)]);

        // Reaching the role itself — here directly — closes a cycle.
        assert!(matches!(
            validate_parent_change(&graph, role, Some(role)),
            Err(PermissionsError::InheritanceCycle)
        ));
    }

    #[test]
    fn a_two_step_cycle_is_refused() {
        // B already inherits A; pointing A at B would close A → B → A.
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let graph = links(&[(a, None), (b, Some(a))]);

        assert!(matches!(
            validate_parent_change(&graph, a, Some(b)),
            Err(PermissionsError::InheritanceCycle)
        ));
        // The reverse direction stays legal: B may inherit A.
        assert!(validate_parent_change(&graph, b, Some(a)).is_ok());
    }

    #[test]
    fn a_chain_past_the_depth_limit_is_refused() {
        // A chain of nine roles above the candidate parent is one step too deep.
        let role = Uuid::new_v4();
        let deep: Vec<Uuid> = (0..9).map(|_| Uuid::new_v4()).collect();
        let mut pairs: Vec<(Uuid, Option<Uuid>)> = vec![(role, None)];
        for (index, id) in deep.iter().enumerate() {
            pairs.push((*id, deep.get(index + 1).copied()));
        }
        let graph = links(&pairs);

        assert!(matches!(
            validate_parent_change(&graph, role, Some(deep[0])),
            Err(PermissionsError::InheritanceDepthExceeded {
                max: MAX_INHERITANCE_DEPTH
            })
        ));

        // One level shorter is accepted (eight ancestors is the documented maximum).
        assert!(validate_parent_change(&graph, role, Some(deep[1])).is_ok());
    }

    #[test]
    fn a_long_descendant_chain_counts_against_the_limit() {
        // The role sits at the top of a ten-deep chain: attaching a ninth ancestor would push
        // the overall chain past the limit even though the walk upward is short.
        let root = Uuid::new_v4();
        let children: Vec<Uuid> = (0..9).map(|_| Uuid::new_v4()).collect();

        let mut pairs: Vec<(Uuid, Option<Uuid>)> = vec![(root, None)];
        pairs.push((children[0], Some(root)));
        for window in children.windows(2) {
            pairs.push((window[1], Some(window[0])));
        }
        let graph = links(&pairs);

        // Nine-deep descendant chain: refused outright.
        assert!(matches!(
            validate_parent_change(&graph, root, None),
            Err(PermissionsError::InheritanceDepthExceeded { .. })
        ));
    }

    #[test]
    fn a_plain_parent_change_is_accepted() {
        let role = Uuid::new_v4();
        let parent = Uuid::new_v4();
        let graph = links(&[(role, None), (parent, None)]);

        assert!(validate_parent_change(&graph, role, Some(parent)).is_ok());
        assert!(validate_parent_change(&graph, role, None).is_ok());
    }
}
