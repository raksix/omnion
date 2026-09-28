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
use crate::folders::{
    Folder, FolderMove, NewFolder, ROOT_FOLDER_NAME, child_path, subtree_pattern,
};

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

/// The library root of one site, materialised on first use.
///
/// The migration seeds one root per site that existed when it ran, which is the right thing for a
/// backfill — but a site created *afterwards* has none, and the browser needs a stable root id on
/// its very first read, not a 404 it has to recover from. So this is not a lookup: it is a
/// get-or-create. The partial unique index `media_folders_root_name_idx (site_id) where
/// parent_id is null` is what makes the race safe, and the loser of the race re-reads the row the
/// winner inserted rather than reporting a name collision to an operator who did nothing wrong.
pub async fn root_folder(pool: &PgPool, site_id: Uuid) -> Result<Folder> {
    let query =
        format!("select {COLUMNS} from media_folders where site_id = $1 and parent_id is null");
    if let Some(existing) = sqlx::query_as::<_, Folder>(&query)
        .bind(site_id)
        .fetch_optional(pool)
        .await?
    {
        return Ok(existing);
    }

    let insert = format!(
        "insert into media_folders (site_id, parent_id, name, path) \
         values ($1, null, $2, $2) on conflict do nothing returning {COLUMNS}"
    );
    match sqlx::query_as::<_, Folder>(&insert)
        .bind(site_id)
        .bind(ROOT_FOLDER_NAME)
        .fetch_optional(pool)
        .await
    {
        Ok(Some(root)) => Ok(root),
        // Another request materialised it between the read and the write: read the winner's row.
        Ok(None) | Err(_) => sqlx::query_as::<_, Folder>(&query)
            .bind(site_id)
            .fetch_optional(pool)
            .await?
            .ok_or(MediaError::FolderNotFound),
    }
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
/// The subtree is rewritten in ONE statement inside one transaction, and the whole subtree — the
/// folder's own row included — is rewritten by that same expression. Two properties make that
/// correct without any ordering:
///
/// 1. A single `UPDATE` evaluates every row's `where` and every `set` expression against the
///    **pre-update** snapshot, so a child is never matched against an already-moved parent path.
///    (An earlier version also asked for `order by path desc` "deepest first" — PostgreSQL has no
///    `ORDER BY` in `UPDATE`, and it does not need one: the ordering premise was the bug, not its
///    absence.)
/// 2. The rewritten set excludes the folder's own row and the second statement writes that row,
///    so the folder and its children can never disagree about the new prefix.
///
/// Renaming a folder into its own subtree is refused before any write.
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

    // A move to where the folder already is a no-op, not a refusal. "Move to the root" on a
    // top-level folder, or a screen that always sends the whole form, would otherwise get a
    // `folder_cycle` that names a cycle where there is none — the most misleading answer the route
    // can give, because nothing was even attempted.
    if new_path == current.path {
        transaction.commit().await?;
        return Ok(current);
    }

    // A folder cannot become its own descendant. The check is on paths, not on ids, because the
    // tree is addressed by path everywhere else — and it has to catch the indirect case, where a
    // parent is moved *into* one of its own children.
    if new_path.starts_with(&format!("{}/", current.path))
        || current.path.starts_with(&format!("{}/", new_path))
    {
        return Err(MediaError::FolderCycle { path: new_path });
    }

    let old_path = current.path.clone();
    let prefix = subtree_pattern(&old_path);

    let rewritten = sqlx::query(
        "update media_folders \
         set path = $3 || substring(path from length($1) + 1), updated_at = now() \
         where site_id = $4 and (path = $1 or path like $2) and id <> $5",
    )
    .bind(&old_path)
    .bind(&prefix)
    .bind(&new_path)
    .bind(current.site_id)
    .bind(id)
    .execute(&mut *transaction)
    .await?;

    let moved = sqlx::query_as::<_, Folder>(&format!(
        "update media_folders set \
           parent_id = (select id from media_folders where site_id = $5 and path = $4), \
           name = $2, path = $3, updated_at = now() \
         where id = $1 returning {COLUMNS}"
    ))
    .bind(id)
    .bind(&target.name)
    .bind(&new_path)
    // `$4` is the *parent's* path, not the folder's new one. Resolving the parent by the moved
    // folder's own new path returned the moved row itself, which left `parent_id = id` for every
    // move and collided with `media_folders_root_name_idx` the first time a folder was pulled up to
    // the root — a rename alone was enough to break it.
    .bind(&target.parent_path)
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
