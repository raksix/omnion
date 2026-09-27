//! Role version history (docs/07-IAM.md §17).
//!
//! Every role change appends one row to `role_versions`: the role's own fields plus its
//! permission set as they stood at that moment. The history tab reads consecutive versions and
//! draws their diff; the matrix save compares the caller's version against the latest one, so a
//! screen that was left open cannot silently overwrite somebody else's change.
//!
//! The diff itself is pure ([`diff`], [`diff_versions`]) and unit-tested without a database.

use std::collections::BTreeMap;

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::Result;
use crate::model::{
    Effect, PermissionChange, Role, RoleDiff, RolePermission, RoleVersion,
};

/// Column list for every version query, so the row shape stays in one place.
const VERSION_COLUMNS: &str = "id, role_id, version, name, description, priority, \
     inherits_role_id, inherit_permissions, permissions, change, changed_by, created_at";

/// Diff two permission sets.
///
/// Keys are compared exactly once each: a key only in `after` was added, a key only in `before`
/// was removed, and a key whose effect flipped was changed.
#[must_use]
pub fn diff(before: &[RolePermission], after: &[RolePermission]) -> RoleDiff {
    let before_map: BTreeMap<&str, Effect> = before
        .iter()
        .map(|entry| (entry.key.as_str(), entry.effect))
        .collect();
    let after_map: BTreeMap<&str, Effect> = after
        .iter()
        .map(|entry| (entry.key.as_str(), entry.effect))
        .collect();

    let mut diff = RoleDiff::default();
    for (key, effect) in &after_map {
        match before_map.get(key) {
            None => diff.added.push(RolePermission {
                key: (*key).to_owned(),
                effect: *effect,
            }),
            Some(previous) if previous != effect => diff.changed.push(PermissionChange {
                key: (*key).to_owned(),
                from: Some(*previous),
                to: Some(*effect),
            }),
            Some(_) => {}
        }
    }
    for (key, effect) in &before_map {
        if !after_map.contains_key(key) {
            diff.removed.push(RolePermission {
                key: (*key).to_owned(),
                effect: *effect,
            });
        }
    }

    diff
}

/// Diff two stored versions (what the history tab shows between two rows).
#[must_use]
pub fn diff_versions(previous: &RoleVersion, next: &RoleVersion) -> RoleDiff {
    diff(&previous.entries(), &next.entries())
}

/// Append a version row for `role` with `entries` as its permission set.
///
/// The version number is `max(version) + 1` for the role, computed inside the same statement.
pub async fn record(
    pool: &PgPool,
    role: &Role,
    entries: &[RolePermission],
    change: &str,
    changed_by: Option<Uuid>,
) -> Result<RoleVersion> {
    let permissions = serde_json::Value::Array(
        entries
            .iter()
            .map(|entry| {
                serde_json::json!({ "key": entry.key, "effect": entry.effect.as_str() })
            })
            .collect(),
    );

    let sql = format!(
        "insert into role_versions (role_id, version, name, description, priority, \
         inherits_role_id, inherit_permissions, permissions, change, changed_by) values \
         ($1, (select coalesce(max(version), 0) + 1 from role_versions where role_id = $1), \
          $2, $3, $4, $5, $6, $7, $8, $9) returning {VERSION_COLUMNS}"
    );

    let version: RoleVersion = sqlx::query_as(&sql)
        .bind(role.id)
        .bind(&role.name)
        .bind(&role.description)
        .bind(role.priority)
        .bind(role.inherits_role_id)
        .bind(role.inherit_permissions)
        .bind(&permissions)
        .bind(change)
        .bind(changed_by)
        .fetch_one(pool)
        .await?;

    Ok(version)
}

/// Every version of a role, newest first.
pub async fn list(pool: &PgPool, role_id: Uuid) -> Result<Vec<RoleVersion>> {
    let sql = format!(
        "select {VERSION_COLUMNS} from role_versions where role_id = $1 order by version desc"
    );
    let rows: Vec<RoleVersion> = sqlx::query_as(&sql).bind(role_id).fetch_all(pool).await?;
    Ok(rows)
}

/// The latest version number of a role (`None` when it has no history yet).
pub async fn latest_version(pool: &PgPool, role_id: Uuid) -> Result<Option<i32>> {
    let version: Option<i32> =
        sqlx::query_scalar("select max(version) from role_versions where role_id = $1")
            .bind(role_id)
            .fetch_one(pool)
            .await?;
    Ok(version)
}

/// The latest stored version of a role.
pub async fn latest(pool: &PgPool, role_id: Uuid) -> Result<Option<RoleVersion>> {
    let sql = format!(
        "select {VERSION_COLUMNS} from role_versions where role_id = $1 order by version desc limit 1"
    );
    let row: Option<RoleVersion> = sqlx::query_as(&sql)
        .bind(role_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, effect: Effect) -> RolePermission {
        RolePermission {
            key: key.to_owned(),
            effect,
        }
    }

    fn version(role_id: Uuid, number: i32, entries: &[RolePermission]) -> RoleVersion {
        RoleVersion {
            id: Uuid::new_v4(),
            role_id,
            version: number,
            name: "Role".to_owned(),
            description: String::new(),
            priority: 400,
            inherits_role_id: None,
            inherit_permissions: true,
            permissions: serde_json::Value::Array(
                entries
                    .iter()
                    .map(|entry| {
                        serde_json::json!({ "key": entry.key, "effect": entry.effect.as_str() })
                    })
                    .collect(),
            ),
            change: "permissions".to_owned(),
            changed_by: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn an_identical_set_has_no_diff() {
        let set = [
            entry("content.pages.read", Effect::Allow),
            entry("media.upload", Effect::Deny),
        ];
        let diff = diff(&set, &set);
        assert!(diff.is_empty());
        assert_eq!(diff.total(), 0);
    }

    #[test]
    fn additions_removals_and_flips_are_told_apart() {
        let before = [
            entry("content.pages.read", Effect::Allow),
            entry("media.upload", Effect::Allow),
            entry("users.read", Effect::Deny),
        ];
        let after = [
            entry("content.pages.read", Effect::Allow),
            entry("media.upload", Effect::Deny),
            entry("users.create", Effect::Allow),
        ];

        let diff = diff(&before, &after);
        assert_eq!(diff.added, vec![entry("users.create", Effect::Allow)]);
        assert_eq!(diff.removed, vec![entry("users.read", Effect::Deny)]);
        assert_eq!(
            diff.changed,
            vec![PermissionChange {
                key: "media.upload".to_owned(),
                from: Some(Effect::Allow),
                to: Some(Effect::Deny),
            }]
        );
        assert_eq!(diff.total(), 3);
        assert!(!diff.is_empty());
    }

    #[test]
    fn version_rows_parse_their_entries_back() {
        let role_id = Uuid::new_v4();
        let row = version(
            role_id,
            2,
            &[
                entry("content.pages.read", Effect::Allow),
                entry("media.delete", Effect::Deny),
            ],
        );
        let entries = row.entries();
        assert_eq!(entries.len(), 2);
        assert!(entries.contains(&entry("content.pages.read", Effect::Allow)));
        assert!(entries.contains(&entry("media.delete", Effect::Deny)));

        // A malformed row never panics; it simply contributes nothing it cannot read.
        let mut broken = row.clone();
        broken.permissions = serde_json::json!([{ "key": "content.pages.read" }, "nonsense"]);
        assert!(broken.entries().is_empty());
    }

    #[test]
    fn consecutive_versions_diff_like_their_sets() {
        let role_id = Uuid::new_v4();
        let first = version(
            role_id,
            1,
            &[entry("content.pages.read", Effect::Allow)],
        );
        let second = version(
            role_id,
            2,
            &[
                entry("content.pages.read", Effect::Allow),
                entry("content.pages.update", Effect::Allow),
            ],
        );

        let diff = diff_versions(&first, &second);
        assert_eq!(
            diff.added,
            vec![entry("content.pages.update", Effect::Allow)]
        );
        assert!(diff.removed.is_empty());
        assert!(diff.changed.is_empty());
    }
}
