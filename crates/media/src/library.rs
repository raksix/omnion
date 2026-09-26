//! The `media` table: rows that describe objects in the bucket.
//!
//! Rows are the library's source of truth — the bucket holds bytes, this table holds what they
//! are. A row is written only after its object is stored, so a row always points at something
//! that exists.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{MediaError, Result};
use crate::model::{Media, NewMedia};

/// Every column of a media row, in the order the model reads them.
const COLUMNS: &str = "id, site_id, storage_key, filename, content_type, size_bytes, checksum, \
                       created_by, created_at";

/// Insert one media row.
pub async fn insert_media(pool: &PgPool, new: NewMedia) -> Result<Media> {
    let query = format!(
        "insert into media (site_id, storage_key, filename, content_type, size_bytes, checksum, \
         created_by) values ($1, $2, $3, $4, $5, $6, $7) returning {COLUMNS}"
    );

    let row = sqlx::query_as::<_, Media>(&query)
        .bind(new.site_id)
        .bind(&new.storage_key)
        .bind(&new.filename)
        .bind(&new.content_type)
        .bind(new.size_bytes)
        .bind(&new.checksum)
        .bind(new.created_by)
        .fetch_one(pool)
        .await;

    match row {
        Ok(media) => Ok(media),
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
            Err(MediaError::KeyTaken)
        }
        Err(error) => Err(error.into()),
    }
}

/// One media row by id.
pub async fn find_media(pool: &PgPool, id: Uuid) -> Result<Option<Media>> {
    let query = format!("select {COLUMNS} from media where id = $1");
    sqlx::query_as::<_, Media>(&query)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// The media library of one site, newest first.
pub async fn list_media(pool: &PgPool, site_id: Uuid) -> Result<Vec<Media>> {
    let query =
        format!("select {COLUMNS} from media where site_id = $1 order by created_at desc, id");
    sqlx::query_as::<_, Media>(&query)
        .bind(site_id)
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

/// Remove one media row; `true` when a row was there.
pub async fn delete_media(pool: &PgPool, id: Uuid) -> Result<bool> {
    let result = sqlx::query("delete from media where id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}
