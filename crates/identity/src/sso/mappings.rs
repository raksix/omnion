//! Storing and reading the attribute map (REQ-065, slice 2).
//!
//! The language lives in [`super::attributes`]; this is the half that talks to
//! `provider_attribute_mappings`. It is deliberately small: the only interesting operation is
//! [`replace_map`], which is a **transaction**, because "save" on an editor must be all-or-nothing.
//!
//! Why a transaction and not a delete-then-insert: the map is read by the sign-in path, and a
//! window in which a provider has *no* email mapping is a window in which a real sign-in is
//! refused for a reason that has nothing to do with the person signing in. Delete-then-insert
//! without a transaction would need a lock on the sign-in path to be safe, and a lock is a worse
//! answer than a transaction that is atomic by construction.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{IdentityError, Result};
use crate::sso::attributes::{AttributeMap, AttributeMapping, TargetField, Transform};

/// One stored row: the editor row plus its identity.
///
/// The editor row is [`AttributeMapping`] rather than a second struct of the same shape, because a
/// copy would drift and a drift here is a migration error discovered at runtime instead of at
/// compile time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMapping {
    /// Row id.
    pub id: Uuid,
    /// The provider it belongs to.
    pub provider_id: Uuid,
    /// The editor row.
    pub mapping: AttributeMapping,
    /// When it was created.
    pub created_at: OffsetDateTime,
}

/// The columns, kept in one place so a schema change is a one-line edit.
const COLUMNS: &str = "id, provider_id, source_attr, target_field, transform, transform_arg, \
                       required, position, created_at";

/// A row as it comes back from the database, where the enum names are text.
///
/// A private shape rather than the stored row itself: the enums arrive as text, and parsing them
/// through `FromRow` would mean a failed parse takes the whole list down. One corrupt row should
/// cost one row, not the editor.
#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    provider_id: Uuid,
    source_attr: String,
    target_field: String,
    transform: String,
    transform_arg: Option<String>,
    required: bool,
    position: i32,
    created_at: OffsetDateTime,
}

impl From<Row> for StoredMapping {
    fn from(row: Row) -> Self {
        // The database's `check` constraints make these two parses infallible, and a parse that
        // failed here would be a migration that did not land — which the migration gate proves
        // separately. `unwrap_or` keeps a corrupt row from taking a whole list down with it: the
        // editor shows the rest of the map and this row falls back to the first field, which is
        // visible and fixable rather than fatal.
        let mapping = AttributeMapping {
            source_attr: row.source_attr,
            target_field: TargetField::parse(&row.target_field).unwrap_or(TargetField::Email),
            transform: Transform::parse(&row.transform).unwrap_or(Transform::None),
            transform_arg: row.transform_arg,
            required: row.required,
            position: row.position,
        };
        Self {
            id: row.id,
            provider_id: row.provider_id,
            mapping,
            created_at: row.created_at,
        }
    }
}

/// Read a provider's whole map, in editor order.
pub async fn list_mappings(pool: &PgPool, provider_id: Uuid) -> Result<Vec<StoredMapping>> {
    let rows = sqlx::query_as::<_, Row>(&format!(
        "select {COLUMNS} from provider_attribute_mappings \
         where provider_id = $1 order by position, target_field"
    ))
    .bind(provider_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(StoredMapping::from).collect())
}

/// Read a provider's map as the language type, ready to project.
pub async fn load_map(pool: &PgPool, provider_id: Uuid) -> Result<AttributeMap> {
    let rows = list_mappings(pool, provider_id).await?;
    Ok(AttributeMap::new(
        rows.into_iter().map(|row| row.mapping).collect(),
    ))
}

/// Replace a provider's map with exactly what was submitted.
///
/// Atomic: the whole map is validated **before** the first delete, and the writes run in one
/// transaction, so a sign-in either sees the old map or the new one and never an empty one. A
/// provider with an empty map is a legitimate state — that is how an operator clears it — so an
/// empty list is written as a delete of every row rather than refused.
pub async fn replace_map(
    pool: &PgPool,
    provider_id: Uuid,
    map: AttributeMap,
) -> Result<Vec<StoredMapping>> {
    let problems = map.validate();
    if !problems.is_empty() {
        return Err(IdentityError::InvalidProvider(
            problems
                .iter()
                .map(|problem| problem.message.clone())
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }

    let mut tx = pool.begin().await?;

    // The provider has to exist, and this is where that is proved: a `delete` against a deleted
    // provider succeeds silently, so a map written for a provider that was removed a moment ago
    // would be a set of rows nobody can ever read. The insert below would fail on the foreign key
    // anyway, but a *cleared* map (delete only) would not.
    let exists: Option<Uuid> = sqlx::query_scalar("select id from auth_providers where id = $1")
        .bind(provider_id)
        .fetch_optional(&mut *tx)
        .await?;
    if exists.is_none() {
        tx.rollback().await?;
        return Err(IdentityError::InvalidProvider(format!(
            "provider {provider_id} does not exist"
        )));
    }

    sqlx::query("delete from provider_attribute_mappings where provider_id = $1")
        .bind(provider_id)
        .execute(&mut *tx)
        .await?;

    for row in &map.rows {
        sqlx::query(
            "insert into provider_attribute_mappings \
               (provider_id, source_attr, target_field, transform, transform_arg, required, position) \
             values ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(provider_id)
        .bind(row.source_attr.trim())
        .bind(row.target_field.as_str())
        .bind(row.transform.as_str())
        .bind(row.transform_arg.as_deref())
        .bind(row.required)
        .bind(row.position)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    // Read back rather than reconstructing from the request: a trigger, a default or a future
    // column would otherwise be invisible to the very response that reports the write.
    list_mappings(pool, provider_id).await
}

/// How many rows a provider's map carries — the editor's badge.
pub async fn count_mappings(pool: &PgPool, provider_id: Uuid) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from provider_attribute_mappings where provider_id = $1",
    )
    .bind(provider_id)
    .fetch_one(pool)
    .await?;
    Ok(count)
}
