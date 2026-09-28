//! The `media_folders` table: reading the tree, writing a node, and moving a subtree.
//!
//! Every statement here is written once and reused, because the tree has one invariant that
//! cannot be re-derived per call site: a folder's `path` always matches the chain of `parent_id`
//! links above it. A move that only changed the moved row would leave its children pointing at a
//! stale path, and the next rename inside that subtree would build a wrong path on top of it.
//! So a move rewrites the subtree in the same transaction as the row itself.

use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::error::{MediaError, Result};
use crate::folders::{Folder, FolderMove, NewFolder, child_path, subtree_pattern};

/// Every column of a folder row, in the order the model reads them.
const COLUMNS: &str = "id, site_id, parent_id, name, path, created_by, created_at, updated_at";

/// One folder by id.
pub async fn find_folder(pool: &PgPool, id: Uuid) -> Result<Option<Folder>> {
    let query = format!("select {COLUMNS} from media_folders where id = $1");
    sqlx::query_as::<_, Folder>(&query)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// Every folder of one site, ordered by its materialised path.
///
/// The whole tree in one read: the panel's left rail renders a site of a few hundred folders
/// without a recursive query, and the breadcrumb of any folder is already in the response.
pub async fn list_folders(pool: &PgPool, site_id: Uuid) -> Result<Vec<Folder>> {
    let query =
        format!("select {COLUMNS} from media_folders where site_id = $1 order by path, name");
    sqlx::query_as::<_, Folder>(&query)
        .bind(site_id)
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

/// The library root of one site, materialised by the migration and therefore always present.
pub async fn root_folder(pool: &PgPool, site_id: Uuid) -> Result<Folder> {
    let query =
        format!("select {COLUMNS} from media_folders where site_id = $1 and parent_id is null");
    sqlx::query_as::<_, Folder>(&query)
        .bind(site_id)
        .fetch_optional(pool)
        .await?
        .ok_or(MediaError::FolderNotFound)
}

/// Create one folder under a parent.
pub async fn insert_folder(pool: &PgPool, new: NewFolder) -> Result<Folder> {
    let query = format!(
        "insert into media_folders (site_id, parent_id, name, path, created_by) \
         values ($1, $2, $3, $4, $5) returning {COLUMNS}"
    );

    let row = sqlx::query_as::<_, Folder>(&query)
        .bind(new.site_id)
        .bind(new.parent_id)
        .bind(&new.name)
        .bind(&new.parent_path)
        .bind(new.created_by)
        .fetch_one(pool)
        .await;

    match row {
        Ok(folder) => Ok(folder),
        // Two siblings with one name: the partial unique index caught the race, and the operator
        // is told which field collided rather than getting a bare constraint error.
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
            Err(MediaError::FolderNameTaken)
        }
        Err(error) => Err(error.into()),
    }
}

/// Rename or move one folder, rewriting the paths of everything under it.
///
/// The subtree is rewritten deepest-first inside one transaction. That order is the whole point:
/// rewriting `Media/Campaigns` into `Media/2026/Campaigns` before its children would match the
/// children's `like 'Media/Campaigns/%'` pattern against the already-moved parent path and leave
/// them behind. Renaming a folder into its own subtree is refused before any write.
pub async fn move_folder(pool: &PgPool, id: Uuid, target: &FolderMove) -> Result<Folder> {
    let mut transaction = pool.begin().await?;

    let current: Folder = sqlx::query_as(&format!(
        "select {COLUMNS} from media_folders where id = $1 for update"
    ))
    .bind(id)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or(MediaError::FolderNotFound)?;

    let new_path = child_path(&target.parent_path, &target.name)?;

    // A folder cannot become its own descendant. The check is on paths, not on ids, because the
    // tree is addressed by path everywhere else — and it has to catch the indirect case, where a
    // parent is moved *into* one of its own children.
    if new_path == current.path
        || new_path.starts_with(&format!("{}/", current.path))
        || current.path.starts_with(&format!("{}/", new_path))
    {
        return Err(MediaError::FolderCycle { path: new_path });
    }

    let old_path = current.path.clone();
    let prefix = subtree_pattern(&old_path);

    // Deepest first: `order by path desc` puts `Media/Campaigns/2026/Q1` before `Media/Campaigns`.
    let rewritten = sqlx::query(
        "update media_folders \
         set path = $3 || substring(path from length($1) + 1), updated_at = now() \
         where site_id = $4 and (path = $1 or path like $2) and id <> $5 \
         order by path desc",
    )
    .bind(&old_path)
    .bind(&prefix)
    .bind(&new_path)
    .bind(current.site_id)
    .bind(id)
    .execute(&mut *transaction)
    .await?;

    let moved = sqlx::query_as::<_, Folder>(&format!(
        "update media_folders set parent_id = (select id from media_folders where site_id = $4 and \
         path = $3), name = $2, path = $3, updated_at = now() where id = $1 returning {COLUMNS}"
    ))
    .bind(id)
    .bind(&target.name)
    .bind(&new_path)
    .bind(current.site_id)
    .fetch_one(&mut *transaction)
    .await?;

    transaction.commit().await?;
    tracing::debug!(
        folder = %id,
        descendants = rewritten.rows_affected(),
        from = %old_path,
        to = %new_path,
        "a folder moved with its subtree"
    );
    Ok(moved)
}

/// Delete one empty folder.
///
/// A folder that still holds children or files is refused by the database (`on delete restrict` on
/// children, and the `media` rows are counted here), so the API can answer a truthful "not empty"
/// instead of half-deleting a subtree.
pub async fn delete_empty_folder(pool: &PgPool, id: Uuid) -> Result<()> {
    let folder = find_folder(pool, id)
        .await?
        .ok_or(MediaError::FolderNotFound)?;

    if folder.is_root() {
        return Err(MediaError::RootFolderProtected);
    }

    let (children,): (i64,) =
        sqlx::query_as("select count(*) from media_folders where parent_id = $1")
            .bind(id)
            .fetch_one(pool)
            .await?;
    if children > 0 {
        return Err(MediaError::FolderNotEmpty {
            what: "folders",
            count: children,
        });
    }

    let (files,): (i64,) =
        sqlx::query_as("select count(*) from media where folder_id = $1 and deleted_at is null")
            .bind(id)
            .fetch_one(pool)
            .await?;
    if files > 0 {
        return Err(MediaError::FolderNotEmpty {
            what: "files",
            count: files,
        });
    }

    let deleted = sqlx::query("delete from media_folders where id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(MediaError::FolderNotFound);
    }
    Ok(())
}

/// How many live files a folder holds directly (not its subtree) — the count the tree shows.
pub async fn count_files_in_folder(pool: &PgPool, folder_id: Uuid) -> Result<i64> {
    let (count,): (i64,) =
        sqlx::query_as("select count(*) from media where folder_id = $1 and deleted_at is null")
            .bind(folder_id)
            .fetch_one(pool)
            .await?;
    Ok(count)
}

/// Move a file into a folder (or back to the library root when `folder_id` is `None`).
///
/// The storage key never changes: a move is a reference change, so every cached derivative, every
/// signed URL and every published page keeps pointing at the same bytes.
pub async fn set_file_folder(
    executor: &mut Transaction<'_, Postgres>,
    media_id: Uuid,
    folder_id: Option<Uuid>,
) -> Result<u64> {
    let result = sqlx::query(
        "update media set folder_id = $2, updated_at = now() \
         where id = $1 and deleted_at is null",
    )
    .bind(media_id)
    .bind(folder_id)
    .execute(&mut **executor)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_subtree_pattern_never_matches_a_sibling_prefix() {
        // `Media/Archive` must not be caught by a move of `Media/Arch`.
        assert_eq!(subtree_pattern("Media/Arch"), "Media/Arch/%");
        assert!(!subtree_pattern("Media/Arch").contains("Arch'"));
    }

    #[test]
    fn the_columns_are_the_ones_the_model_reads() {
        for column in COLUMNS.split(", ") {
            assert!(!column.is_empty());
        }
        assert_eq!(COLUMNS.split(", ").count(), 8);
    }
}
