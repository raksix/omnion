//! Groups (teams): a set of accounts that can carry role bindings of its own.
//!
//! A group is a subject like any other — the roles attached to it grant to every member exactly
//! the way a personal binding does, and membership changes take effect on the next request
//! because resolution never caches.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::bindings;
use crate::error::{PermissionsError, Result};
use crate::model::Subject;

/// Longest accepted group name.
const MAX_GROUP_NAME_LENGTH: usize = 80;

/// A group (team) of an organization.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Group {
    /// Primary key.
    pub id: Uuid,
    /// Owning organization.
    pub organization_id: Uuid,
    /// Display name.
    pub name: String,
    /// URL-safe key, unique within the organization.
    pub slug: String,
    /// What the group is for.
    pub description: String,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

/// A group with the counts the list screen shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSummary {
    /// The group.
    pub group: Group,
    /// Live members.
    pub member_count: i64,
    /// Live role bindings attached to the group.
    pub role_count: i64,
}

/// A group to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewGroup {
    /// Owning organization.
    pub organization_id: Uuid,
    /// Display name.
    pub name: String,
    /// What the group is for.
    pub description: String,
}

/// A membership row with the account's details.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct GroupMember {
    /// The account.
    pub user_id: Uuid,
    /// Account e-mail.
    pub email: String,
    /// Display name.
    pub display_name: String,
    /// Account status.
    pub status: String,
    /// When the member joined.
    pub created_at: OffsetDateTime,
}

/// Column list of every group query.
const GROUP_COLUMNS: &str = "id, organization_id, name, slug, description, created_at, updated_at";

/// Create a group; the slug is derived from the name.
pub async fn create(pool: &PgPool, new: NewGroup) -> Result<Group> {
    let name = validate_name(&new.name)?;
    let slug = slugify(&name);

    let sql = format!(
        "insert into groups (organization_id, name, slug, description) \
         values ($1, $2, $3, $4) returning {GROUP_COLUMNS}"
    );

    let group: Group = sqlx::query_as(&sql)
        .bind(new.organization_id)
        .bind(&name)
        .bind(&slug)
        .bind(new.description.trim())
        .fetch_one(pool)
        .await
        .map_err(map_group_error)?;

    Ok(group)
}

/// A group and its counts, for the list screen.
pub async fn list(pool: &PgPool, organization_id: Uuid) -> Result<Vec<GroupSummary>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: Uuid,
        organization_id: Uuid,
        name: String,
        slug: String,
        description: String,
        created_at: OffsetDateTime,
        updated_at: OffsetDateTime,
        member_count: i64,
        role_count: i64,
    }

    let rows: Vec<Row> = sqlx::query_as(
        "select g.id, g.organization_id, g.name, g.slug, g.description, g.created_at, \
                g.updated_at, \
                (select count(*) from group_members m where m.group_id = g.id) as member_count, \
                (select count(*) from role_bindings b \
                  where b.subject_type = 'group' and b.subject_id = g.id \
                    and b.revoked_at is null \
                    and (b.expires_at is null or b.expires_at > now())) as role_count \
         from groups g \
         where g.organization_id = $1 \
         order by lower(g.name) asc",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| GroupSummary {
            group: Group {
                id: row.id,
                organization_id: row.organization_id,
                name: row.name,
                slug: row.slug,
                description: row.description,
                created_at: row.created_at,
                updated_at: row.updated_at,
            },
            member_count: row.member_count,
            role_count: row.role_count,
        })
        .collect())
}

/// Look a group up by id.
pub async fn find(pool: &PgPool, id: Uuid) -> Result<Option<Group>> {
    let sql = format!("select {GROUP_COLUMNS} from groups where id = $1");
    let group: Option<Group> = sqlx::query_as(&sql).bind(id).fetch_optional(pool).await?;
    Ok(group)
}

/// Update a group's name and description.
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    name: Option<&str>,
    description: Option<&str>,
) -> Result<Group> {
    let current = find(pool, id)
        .await?
        .ok_or(PermissionsError::GroupNotFound)?;

    let name = match name {
        Some(name) => validate_name(name)?,
        None => current.name.clone(),
    };
    let slug = slugify(&name);
    let description = description.map(str::trim).unwrap_or(&current.description);

    let sql = format!(
        "update groups set name = $2, slug = $3, description = $4, updated_at = now() \
         where id = $1 returning {GROUP_COLUMNS}"
    );

    let group: Group = sqlx::query_as(&sql)
        .bind(id)
        .bind(&name)
        .bind(&slug)
        .bind(description)
        .fetch_one(pool)
        .await
        .map_err(map_group_error)?;

    Ok(group)
}

/// Delete a group: its memberships go with it, and the role bindings it carried are revoked —
/// a deleted group must not keep granting.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<bool> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "update role_bindings set revoked_at = now() \
         where subject_type = 'group' and subject_id = $1 and revoked_at is null",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    let deleted = sqlx::query("delete from groups where id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        > 0;
    tx.commit().await?;

    Ok(deleted)
}

/// The members of a group, oldest first.
pub async fn list_members(pool: &PgPool, group_id: Uuid) -> Result<Vec<GroupMember>> {
    let rows: Vec<GroupMember> = sqlx::query_as(
        "select u.id as user_id, u.email, u.display_name, u.status, m.created_at \
         from group_members m join users u on u.id = m.user_id \
         where m.group_id = $1 \
         order by m.created_at asc, u.email asc",
    )
    .bind(group_id)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Add a member. Returns `false` when they were already a member.
pub async fn add_member(
    pool: &PgPool,
    group_id: Uuid,
    user_id: Uuid,
    added_by: Option<Uuid>,
) -> Result<bool> {
    let inserted = sqlx::query(
        "insert into group_members (group_id, user_id, added_by) values ($1, $2, $3) \
         on conflict (group_id, user_id) do nothing",
    )
    .bind(group_id)
    .bind(user_id)
    .bind(added_by)
    .execute(pool)
    .await?
    .rows_affected()
        > 0;

    Ok(inserted)
}

/// Remove a member. Returns `true` when they were one.
pub async fn remove_member(pool: &PgPool, group_id: Uuid, user_id: Uuid) -> Result<bool> {
    let removed = sqlx::query("delete from group_members where group_id = $1 and user_id = $2")
        .bind(group_id)
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected()
        > 0;

    Ok(removed)
}

/// Replace the whole membership of a group with `user_ids` (the members tab's save).
pub async fn replace_members(
    pool: &PgPool,
    group_id: Uuid,
    user_ids: &[Uuid],
    added_by: Option<Uuid>,
) -> Result<()> {
    let mut tx = pool.begin().await?;

    sqlx::query("delete from group_members where group_id = $1 and user_id <> all($2)")
        .bind(group_id)
        .bind(user_ids)
        .execute(&mut *tx)
        .await?;

    if !user_ids.is_empty() {
        sqlx::query(
            "insert into group_members (group_id, user_id, added_by) \
             select $1, member_id, $3 from unnest($2::uuid[]) as member_id \
             on conflict (group_id, user_id) do nothing",
        )
        .bind(group_id)
        .bind(user_ids)
        .bind(added_by)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

/// The ids of the groups an account belongs to — the join the resolver reads for every request.
pub async fn member_groups(pool: &PgPool, user_id: Uuid) -> Result<Vec<Uuid>> {
    let ids: Vec<Uuid> =
        sqlx::query_scalar("select group_id from group_members where user_id = $1")
            .bind(user_id)
            .fetch_all(pool)
            .await?;
    Ok(ids)
}

/// The live role bindings attached to a group (what the group grants its members).
pub async fn roles_of_group(
    pool: &PgPool,
    group_id: Uuid,
) -> Result<Vec<crate::model::RoleBinding>> {
    let bindings = bindings::bindings_of_subject(pool, Subject::Group(group_id)).await?;
    let now = OffsetDateTime::now_utc();
    Ok(bindings
        .into_iter()
        .filter(|binding| binding.is_active_at(now))
        .collect())
}

/// Validate a group name.
pub fn validate_name(name: &str) -> Result<String> {
    let name = name.trim().to_owned();
    if name.is_empty() || name.chars().count() > MAX_GROUP_NAME_LENGTH {
        return Err(PermissionsError::InvalidGroupName(name));
    }
    Ok(name)
}

/// Derive a group slug from its name: lowercase, ASCII letters, digits and dashes.
#[must_use]
pub fn slugify(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut pending_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(ch.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    if slug.is_empty() {
        return "group".to_owned();
    }
    if !slug.starts_with(|c: char| c.is_ascii_lowercase()) {
        slug.insert(0, 'g');
    }
    slug.truncate(60);
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

fn map_group_error(err: sqlx::Error) -> PermissionsError {
    match err {
        sqlx::Error::Database(ref db_error) if db_error.is_unique_violation() => {
            PermissionsError::GroupNameTaken
        }
        other => PermissionsError::Database(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_lowercase_slugs() {
        assert_eq!(slugify("Marketing Team"), "marketing-team");
        assert_eq!(slugify("  R&D  "), "r-d");
        assert_eq!(slugify("Yönetim"), "y-netim");
        assert_eq!(slugify(""), "group");
        assert_eq!(slugify("123"), "g123");
        assert_eq!(slugify("Ürün Ekibi"), "r-n-ekibi");
    }

    #[test]
    fn names_are_bounded() {
        assert_eq!(validate_name("  Editors ").expect("valid"), "Editors");
        assert!(validate_name("   ").is_err());
        assert!(validate_name(&"n".repeat(MAX_GROUP_NAME_LENGTH + 1)).is_err());
    }
}
