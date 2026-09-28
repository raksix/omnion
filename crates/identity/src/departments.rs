//! Organization departments and department membership (docs/requests/REQ-005, slice 2).
//!
//! A membership (slice 1) answers *who belongs to this organization*. This module answers the
//! structure inside it: which department a person sits in, and which roles a department
//! carries as a whole.
//!
//! Two decisions are worth naming, because both are easy to get wrong later:
//!
//! * **A department is addressed by a stable `key`, not by its id.** A role binding at
//!   department scope stores the department in `role_bindings.resource_id` as text (that is
//!   what `Scope::Department` has compared since 0011), and the resolver compares that string
//!   against the request context. So the key *is* the contract: renaming the display name is
//!   free, and a key that changed would silently re-scope every role bound to it. The key
//!   therefore never changes after creation, and it is shaped like a site key.
//! * **A cycle is refused by walking the ancestors inside the transaction.** The schema's
//!   `parent_id <> id` check only catches a department made its own parent in one statement;
//!   the A → B → C → A case needs the walk, and the check alone would let it through.
//!
//! Department *membership* is separate from `organization_members` on purpose: belonging to the
//! organization and belonging to a department are different questions, and a person can be in
//! several departments while holding exactly one organization membership.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{IdentityError, Result};

/// Longest accepted department name.
const MAX_NAME_LENGTH: usize = 120;

/// Longest accepted department description.
const MAX_DESCRIPTION_LENGTH: usize = 1000;

/// Department statuses a row may carry.
pub const DEPARTMENT_STATUSES: [&str; 2] = ["active", "archived"];

/// The status a freshly created department starts in.
pub const DEFAULT_DEPARTMENT_STATUS: &str = "active";

/// Column list for every `Department` query, so the row shape stays in one place.
const DEPARTMENT_COLUMNS: &str =
    "id, organization_id, parent_id, key, name, description, status, created_at, updated_at";

/// A department as stored.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Department {
    /// Primary key.
    pub id: Uuid,
    /// Organization the department belongs to.
    pub organization_id: Uuid,
    /// Parent department, `None` for a root.
    pub parent_id: Option<Uuid>,
    /// Stable address inside the organization — this is what a role binding stores.
    pub key: String,
    /// Display name; renaming it changes nothing about the roles bound here.
    pub name: String,
    /// Free-text description.
    pub description: String,
    /// `active` or `archived`.
    pub status: String,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

/// A department to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDepartment {
    /// Organization the department belongs to.
    pub organization_id: Uuid,
    /// Parent department, `None` for a root.
    pub parent_id: Option<Uuid>,
    /// Stable key; validated and left untouched afterwards.
    pub key: String,
    /// Display name.
    pub name: String,
    /// Optional description.
    pub description: String,
}

/// Fields [`update_department`] may change. `None` leaves a field untouched.
///
/// There is deliberately no `key`: the key is the address a role binding stores, so changing it
/// would silently re-scope every role bound at this department.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DepartmentChanges {
    /// New display name.
    pub name: Option<String>,
    /// New description.
    pub description: Option<String>,
    /// New parent (`Some(None)` promotes to a root).
    pub parent_id: Option<Option<Uuid>>,
    /// New status.
    pub status: Option<String>,
}

impl DepartmentChanges {
    /// `true` when the request changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.description.is_none()
            && self.parent_id.is_none()
            && self.status.is_none()
    }
}

/// One row of the Departments tab: the department plus the counts the table shows.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct DepartmentSummary {
    /// The department row.
    pub id: Uuid,
    /// Organization the department belongs to.
    pub organization_id: Uuid,
    /// Parent department, `None` for a root.
    pub parent_id: Option<Uuid>,
    /// Stable address inside the organization.
    pub key: String,
    /// Display name.
    pub name: String,
    /// Free-text description.
    pub description: String,
    /// `active` or `archived`.
    pub status: String,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
    /// How many accounts sit in the department.
    pub member_count: i64,
    /// How many live role bindings name this department's key.
    pub role_count: i64,
    /// Depth in the tree, 0 for a root — the indentation the tree table reads.
    pub depth: i32,
}

/// Validate a department name: non-empty and reasonably short.
pub fn validate_department_name(name: &str) -> Result<String> {
    let name = name.trim().to_owned();
    if name.is_empty() || name.chars().count() > MAX_NAME_LENGTH {
        return Err(IdentityError::InvalidDepartment(format!(
            "name must be 1 to {MAX_NAME_LENGTH} characters"
        )));
    }
    Ok(name)
}

/// Validate a department description.
pub fn validate_description(description: &str) -> Result<String> {
    let description = description.trim().to_owned();
    if description.chars().count() > MAX_DESCRIPTION_LENGTH {
        return Err(IdentityError::InvalidDepartment(format!(
            "description must be at most {MAX_DESCRIPTION_LENGTH} characters"
        )));
    }
    Ok(description)
}

/// Validate a department key: the same shape a site key has, lowercase.
///
/// The key is normalized rather than rejected for case, because an operator typing `Marketing`
/// in a form means `marketing`; a key that is *not* shaped is still refused, since it could
/// never be addressed in a role binding.
pub fn validate_department_key(key: &str) -> Result<String> {
    let key = key.trim().to_lowercase();
    let shaped = !key.is_empty()
        && key.len() <= 64
        && key.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && key.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if shaped {
        return Ok(key);
    }
    Err(IdentityError::InvalidDepartment(format!(
        "key {key:?} must be lowercase letters, digits and dashes (1-64 characters)"
    )))
}

/// Validate a department status against the schema values.
pub fn validate_department_status(status: &str) -> Result<String> {
    let status = status.trim().to_lowercase();
    if DEPARTMENT_STATUSES.contains(&status.as_str()) {
        return Ok(status);
    }
    Err(IdentityError::InvalidDepartment(format!(
        "status {status:?} must be one of {}",
        DEPARTMENT_STATUSES.join(", ")
    )))
}

/// Create a department.
///
/// Fails with [`IdentityError::DepartmentKeyTaken`] when the key is used, and with
/// [`IdentityError::DepartmentNotFound`] when the parent is not a department of the same
/// organization — a cross-tenant parent would be a way to read another organization's tree.
pub async fn create_department(pool: &PgPool, new: NewDepartment) -> Result<Department> {
    let key = validate_department_key(&new.key)?;
    let name = validate_department_name(&new.name)?;
    let description = validate_description(&new.description)?;

    // The parent is checked inside the transaction that inserts, so a parent cannot be moved
    // out from under a create between the check and the write.
    let mut tx = pool.begin().await?;

    if let Some(parent_id) = new.parent_id {
        assert_parent_in_organization(&mut tx, new.organization_id, parent_id).await?;
    }

    let sql = format!(
        "insert into departments (organization_id, parent_id, key, name, description, status) \
         values ($1, $2, $3, $4, $5, $6) returning {DEPARTMENT_COLUMNS}"
    );

    let row = sqlx::query_as::<_, Department>(&sql)
        .bind(new.organization_id)
        .bind(new.parent_id)
        .bind(&key)
        .bind(&name)
        .bind(&description)
        .bind(DEFAULT_DEPARTMENT_STATUS)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_department_insert_error)?;

    tx.commit().await?;
    Ok(row)
}

/// Read one department of an organization. `None` when it is not one of theirs — the API turns
/// that into a 404, so another tenant's department is indistinguishable from a missing one.
pub async fn find_department(
    pool: &PgPool,
    organization_id: Uuid,
    department_id: Uuid,
) -> Result<Option<Department>> {
    let sql = format!(
        "select {DEPARTMENT_COLUMNS} from departments \
         where organization_id = $1 and id = $2"
    );
    Ok(sqlx::query_as::<_, Department>(&sql)
        .bind(organization_id)
        .bind(department_id)
        .fetch_optional(pool)
        .await?)
}

/// Read one department by its stable key — how a role binding resolves its target.
pub async fn find_department_by_key(
    pool: &PgPool,
    organization_id: Uuid,
    key: &str,
) -> Result<Option<Department>> {
    let sql = format!(
        "select {DEPARTMENT_COLUMNS} from departments \
         where organization_id = $1 and key = $2"
    );
    Ok(sqlx::query_as::<_, Department>(&sql)
        .bind(organization_id)
        .bind(key)
        .fetch_optional(pool)
        .await?)
}

/// Every department of an organization, roots first, then by name.
///
/// The ordering is done in SQL with a recursive walk rather than by sorting in Rust: the tree
/// order *is* the answer the Departments tab shows, and a recursive CTE keeps the store honest
/// if a client asks for the list without caring about order.
pub async fn list_departments(pool: &PgPool, organization_id: Uuid) -> Result<Vec<Department>> {
    let sql = format!(
        "with recursive tree as ( \
            select d.*, 0 as depth, array[d.key] as path \
              from departments d \
             where d.organization_id = $1 and d.parent_id is null \
            union all \
            select d.*, t.depth + 1, t.path || d.key \
              from departments d \
              join tree t on d.parent_id = t.id \
             where d.organization_id = $1 \
         ) \
         select id, organization_id, parent_id, key, name, description, status, \
                created_at, updated_at \
           from tree \
          order by path, name, id"
    );
    Ok(sqlx::query_as::<_, Department>(&sql)
        .bind(organization_id)
        .fetch_all(pool)
        .await?)
}

/// The Departments tab rows: every department with its member count, its role count and its
/// depth in the tree, ordered the same way [`list_departments`] orders them.
///
/// The role count joins on `resource_id = departments.key` rather than on an id, because that
/// is literally how a department binding stores its target.
pub async fn list_department_summaries(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<Vec<DepartmentSummary>> {
    let sql = format!(
        "with recursive tree as ( \
            select d.*, 0 as depth, array[d.key] as path \
              from departments d \
             where d.organization_id = $1 and d.parent_id is null \
            union all \
            select d.*, t.depth + 1, t.path || d.key \
              from departments d \
              join tree t on d.parent_id = t.id \
             where d.organization_id = $1 \
         ) \
         select t.id, t.organization_id, t.parent_id, t.key, t.name, t.description, t.status, \
                t.created_at, t.updated_at, t.depth, \
                (select count(*) from department_members dm where dm.department_id = t.id) \
                    as member_count, \
                (select count(*) from role_bindings rb \
                  where rb.scope_type = 'department' \
                    and rb.revoked_at is null \
                    and rb.organization_id = t.organization_id \
                    and rb.resource_id = t.key) \
                    as role_count \
           from tree t \
          order by t.path, t.name, t.id"
    );
    Ok(sqlx::query_as::<_, DepartmentSummary>(&sql)
        .bind(organization_id)
        .fetch_all(pool)
        .await?)
}

/// Rename, re-describe, re-parent or archive a department.
pub async fn update_department(
    pool: &PgPool,
    organization_id: Uuid,
    department_id: Uuid,
    changes: &DepartmentChanges,
) -> Result<Department> {
    let current = find_department(pool, organization_id, department_id)
        .await?
        .ok_or(IdentityError::DepartmentNotFound)?;
    if changes.is_empty() {
        return Ok(current);
    }

    let name = match &changes.name {
        Some(name) => Some(validate_department_name(name)?),
        None => None,
    };
    let description = match &changes.description {
        Some(description) => Some(validate_description(description)?),
        None => None,
    };
    let status = match &changes.status {
        Some(status) => Some(validate_department_status(status)?),
        None => None,
    };

    let mut tx = pool.begin().await?;

    if let Some(parent) = &changes.parent_id {
        match parent {
            Some(parent_id) => {
                if *parent_id == department_id {
                    return Err(IdentityError::DepartmentCycle);
                }
                // A move is only legal when the new parent is not already a descendant — that is
                // the A → B → C → A case the schema check cannot see.
                assert_not_descendant(&mut tx, organization_id, department_id, *parent_id).await?;
                assert_parent_in_organization(&mut tx, organization_id, *parent_id).await?;
            }
            // Promoting to a root cannot create a cycle: a root has no ancestors.
            None => {}
        }
    }

    let updated = sqlx::query_as::<_, Department>(&format!(
        "update departments set \
            name = coalesce($3, name), \
            description = coalesce($4, description), \
            parent_id = case when $5 then $6 else parent_id end, \
            status = coalesce($7, status), \
            updated_at = now() \
         where organization_id = $1 and id = $2 \
         returning {DEPARTMENT_COLUMNS}"
    ))
    .bind(organization_id)
    .bind(department_id)
    .bind(name)
    .bind(description)
    .bind(changes.parent_id.is_some())
    .bind(changes.parent_id.flatten())
    .bind(status)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(IdentityError::DepartmentNotFound)?;

    tx.commit().await?;
    Ok(updated)
}

/// Archive a department without deleting it.
///
/// Deleting a department would take its role bindings with it (they point at the key through
/// `resource_id`, so they are not cascaded by the database), which is a silent permission
/// change. Archiving keeps the structure and the history; the roles stay bound and stop
/// matching once the department leaves the tree, which is what the operator meant.
pub async fn archive_department(
    pool: &PgPool,
    organization_id: Uuid,
    department_id: Uuid,
) -> Result<Department> {
    update_department(
        pool,
        organization_id,
        department_id,
        &DepartmentChanges {
            status: Some("archived".to_owned()),
            ..DepartmentChanges::default()
        },
    )
    .await
}

/// Delete a department outright. `false` when the row was already gone.
///
/// Refuses while the department still holds members or live role bindings: a delete that
/// silently dropped a member's only role is exactly the kind of quiet data loss the request
/// lists as a risk.
pub async fn delete_department(
    pool: &PgPool,
    organization_id: Uuid,
    department_id: Uuid,
) -> Result<bool> {
    let department = find_department(pool, organization_id, department_id)
        .await?
        .ok_or(IdentityError::DepartmentNotFound)?;

    let members: i64 = sqlx::query_scalar(
        "select count(*) from department_members where department_id = $1",
    )
    .bind(department_id)
    .fetch_one(pool)
    .await?;
    if members > 0 {
        return Err(IdentityError::InvalidDepartment(format!(
            "{members} account(s) are still in this department — remove them or archive it"
        )));
    }

    let roles: i64 = sqlx::query_scalar(
        "select count(*) from role_bindings \
         where scope_type = 'department' and revoked_at is null \
           and organization_id = $1 and resource_id = $2",
    )
    .bind(organization_id)
    .bind(&department.key)
    .fetch_one(pool)
    .await?;
    if roles > 0 {
        return Err(IdentityError::InvalidDepartment(format!(
            "{roles} role binding(s) still name this department — revoke them or archive it"
        )));
    }

    let children: i64 = sqlx::query_scalar(
        "select count(*) from departments where organization_id = $1 and parent_id = $2",
    )
    .bind(organization_id)
    .bind(department_id)
    .fetch_one(pool)
    .await?;
    if children > 0 {
        return Err(IdentityError::InvalidDepartment(format!(
            "{children} sub-department(s) still hang under it — move or delete them first"
        )));
    }

    let removed = sqlx::query("delete from departments where organization_id = $1 and id = $2")
        .bind(organization_id)
        .bind(department_id)
        .execute(pool)
        .await?
        .rows_affected()
        > 0;
    let _ = department;
    Ok(removed)
}

/// Put an account into a department.
///
/// A department of *another* organization is refused: without that check, a platform account
/// with `organizations.manage` on two tenants could wire them together.
pub async fn add_department_member(
    pool: &PgPool,
    organization_id: Uuid,
    department_id: Uuid,
    user_id: Uuid,
) -> Result<bool> {
    find_department(pool, organization_id, department_id)
        .await?
        .ok_or(IdentityError::DepartmentNotFound)?;

    // The account has to belong to the organization: a department is a way of splitting *its*
    // people, and admitting an outsider would give them the department's roles.
    let is_member: bool = sqlx::query_scalar(
        "select exists (select 1 from organization_members \
          where organization_id = $1 and user_id = $2 and status <> 'suspended')",
    )
    .bind(organization_id)
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    if !is_member {
        return Err(IdentityError::MemberNotFound);
    }

    let inserted = sqlx::query(
        "insert into department_members (department_id, user_id) values ($1, $2) \
         on conflict do nothing",
    )
    .bind(department_id)
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected()
        > 0;
    Ok(inserted)
}

/// Take an account out of a department. `false` when the pair was not there.
pub async fn remove_department_member(
    pool: &PgPool,
    department_id: Uuid,
    user_id: Uuid,
) -> Result<bool> {
    let removed = sqlx::query("delete from department_members where department_id = $1 and user_id = $2")
        .bind(department_id)
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected()
        > 0;
    Ok(removed)
}

/// The departments one account sits in, inside one organization.
pub async fn list_departments_of_user(
    pool: &PgPool,
    organization_id: Uuid,
    user_id: Uuid,
) -> Result<Vec<Department>> {
    // The column list is qualified for the join, so it is spelled out rather than reused from
    // the flat constant.
    let sql = "select d.id, d.organization_id, d.parent_id, d.key, d.name, d.description, \
                      d.status, d.created_at, d.updated_at \
                 from departments d \
                   join department_members dm on dm.department_id = d.id \
                where d.organization_id = $1 and dm.user_id = $2 \
                order by d.name, d.id";
    Ok(sqlx::query_as::<_, Department>(sql)
        .bind(organization_id)
        .bind(user_id)
        .fetch_all(pool)
        .await?)
}

/// The accounts in one department.
pub async fn list_department_member_ids(
    pool: &PgPool,
    department_id: Uuid,
) -> Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "select user_id from department_members where department_id = $1 order by user_id",
    )
    .bind(department_id)
    .fetch_all(pool)
    .await?)
}

/// How many accounts sit in a department.
pub async fn count_department_members(pool: &PgPool, department_id: Uuid) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "select count(*) from department_members where department_id = $1",
    )
    .bind(department_id)
    .fetch_one(pool)
    .await?)
}

/// The keys of every department an account sits in — the extra roles the resolver must fold
/// into their effective set when a request names one of those departments.
///
/// A request inside a department has to count the account's own bindings *and* the ones bound
/// to the department; this is the list that makes the second half possible.
pub async fn department_keys_of_user(
    pool: &PgPool,
    organization_id: Uuid,
    user_id: Uuid,
) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "select d.key from departments d \
           join department_members dm on dm.department_id = d.id \
          where d.organization_id = $1 and dm.user_id = $2 and d.status = 'active'",
    )
    .bind(organization_id)
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

/// The department keys a request context names, expanded through the tree.
///
/// A department binding on "engineering" is meant to reach the people in "engineering" —
/// whether a request arrives tagged with the parent or with a leaf is a detail of how the
/// caller tagged it. Expanding descendants here means the operator does not have to bind the
/// role at every level to make it cover a team.
pub async fn expand_department_scope(
    pool: &PgPool,
    organization_id: Uuid,
    department: &str,
) -> Result<Vec<String>> {
    let sql = "with recursive tree as ( \
                   select d.id, d.key from departments d \
                    where d.organization_id = $1 and d.key = $2 \
                   union all \
                   select d.id, d.key from departments d join tree t on d.parent_id = t.id \
                    where d.organization_id = $1 \
               ) \
               select key from tree";
    Ok(sqlx::query_scalar(sql)
        .bind(organization_id)
        .bind(department)
        .fetch_all(pool)
        .await?)
}

/// The live department-scoped role bindings that apply to `department` — the bindings an
/// account in that department gains on top of their own.
///
/// This is the read that makes the slice's promise true: a role bound to a department shows up
/// in the effective set of everybody in it, and disappears when they leave.
pub async fn bindings_for_department(
    pool: &PgPool,
    organization_id: Uuid,
    department: &str,
) -> Result<Vec<Uuid>> {
    let keys = expand_department_scope(pool, organization_id, department).await?;
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_scalar(
        "select role_id from role_bindings \
          where scope_type = 'department' and revoked_at is null \
            and organization_id = $1 and resource_id = any($2) \
            and (expires_at is null or expires_at > now())",
    )
    .bind(organization_id)
    .bind(&keys)
    .fetch_all(pool)
    .await?)
}

async fn assert_parent_in_organization(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    organization_id: Uuid,
    parent_id: Uuid,
) -> Result<()> {
    let exists: bool = sqlx::query_scalar(
        "select exists (select 1 from departments where id = $1 and organization_id = $2)",
    )
    .bind(parent_id)
    .bind(organization_id)
    .fetch_one(&mut **tx)
    .await?;
    if exists {
        return Ok(());
    }
    // Deliberately the same error as a missing department: a parent id from another tenant must
    // not be distinguishable from one that does not exist.
    Err(IdentityError::DepartmentNotFound)
}

/// Refuse a move that would put `department_id` inside its own subtree.
async fn assert_not_descendant(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    organization_id: Uuid,
    department_id: Uuid,
    new_parent_id: Uuid,
) -> Result<()> {
    // Walk *up* from the proposed parent: if the department being moved is anywhere on that
    // chain, the move closes a loop. The walk is bounded by the visited set, so a cycle that
    // already exists cannot hang the request.
    let mut cursor = Some(new_parent_id);
    let mut guard: Vec<Uuid> = Vec::new();
    while let Some(current) = cursor {
        if current == department_id {
            return Err(IdentityError::DepartmentCycle);
        }
        if guard.contains(&current) {
            return Err(IdentityError::DepartmentCycle);
        }
        guard.push(current);
        cursor = sqlx::query_scalar(
            "select parent_id from departments where id = $1 and organization_id = $2",
        )
        .bind(current)
        .bind(organization_id)
        .fetch_optional(&mut **tx)
        .await?
        .flatten();
    }
    Ok(())
}

fn map_department_insert_error(err: sqlx::Error) -> IdentityError {
    match err {
        sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
            if db_err.constraint() == Some("departments_org_key_key") {
                IdentityError::DepartmentKeyTaken
            } else {
                IdentityError::Database(err)
            }
        }
        // The `parent_id <> id` check is the database's last word on a self-parenting insert.
        sqlx::Error::Database(ref db_err) if db_err.constraint() == Some("departments_not_own_parent") => {
            IdentityError::DepartmentCycle
        }
        other => IdentityError::Database(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_normalized_rather_than_rejected_for_case() {
        // An operator typing `Marketing` in the form means `marketing`; the key is the
        // address a role binding stores, so a case difference must not create a second one.
        assert_eq!(validate_department_key("Marketing").unwrap(), "marketing");
        assert_eq!(validate_department_key("  IT  ").unwrap(), "it");
        assert_eq!(validate_department_key("a-b-9").unwrap(), "a-b-9");
    }

    #[test]
    fn an_unaddressable_key_is_refused() {
        // A key that could never be named in a role binding is worse than no key at all.
        for bad in ["", "-marketing", "marketing-", "mark eting", "a/b", "ünicode"] {
            assert!(
                validate_department_key(bad).is_err(),
                "{bad:?} should not be a usable department key"
            );
        }
    }

    #[test]
    fn a_name_has_to_carry_something() {
        assert!(validate_department_name("   ").is_err());
        assert!(validate_department_name(&"x".repeat(121)).is_err());
        assert_eq!(validate_department_name(" Marketing ").unwrap(), "Marketing");
    }

    #[test]
    fn a_description_is_bounded() {
        assert_eq!(validate_description("  a team  ").unwrap(), "a team");
        assert!(validate_description(&"x".repeat(1001)).is_err());
    }

    #[test]
    fn only_the_schema_statuses_are_accepted() {
        assert_eq!(validate_department_status(" Active ").unwrap(), "active");
        assert!(validate_department_status("deleted").is_err());
    }

    #[test]
    fn a_change_set_reports_whether_it_changes_anything() {
        let mut changes = DepartmentChanges::default();
        assert!(changes.is_empty());
        changes.name = Some("Marketing".to_owned());
        assert!(!changes.is_empty());
    }
}
