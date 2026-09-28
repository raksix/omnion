//! The version history of a file (REQ-010, slice 2).
//!
//! Three rules hold the history straight, and each of them is a place a shortcut produces a
//! plausible-looking wrong answer:
//!
//! 1. **A version is append-only.** A restore copies an old version's *bytes* to a new key and
//!    appends that copy as the newest version. It never rewrites an old row and never renumbers,
//!    so a version number a reader has already seen keeps meaning the same bytes forever.
//! 2. **The version number comes from the database, not from the caller.** `next_version` reads
//!    `max(version)` under a row lock on the `media` row, so two concurrent replaces cannot both
//!    claim number 4 — a `version_count = max(version) + 1` computed in Rust reads the table
//!    twice with a window in between, which is how a gap appears in a history.
//! 3. **The number is not reused.** A version that is pruned leaves a hole rather than letting a
//!    later one take its place, so `unique (media_id, version)` stays a fact about bytes rather
//!    than about time.

use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{MediaError, Result};
use crate::probe::MediaProbe;

/// How many bytes of a file the version store will read to describe it.
pub const PROBE_BYTES: usize = crate::probe::HEADER_BYTES;

/// Largest note a version carries.
pub const MAX_NOTE_LENGTH: usize = 500;

/// One version of one file.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MediaVersion {
    /// Primary key.
    pub id: Uuid,
    /// File this version belongs to.
    pub media_id: Uuid,
    /// Version number: 1, 2, 3…
    pub version: i32,
    /// Object key of this version's bytes.
    pub storage_key: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Hex-encoded SHA-256 of these bytes.
    pub checksum: String,
    /// Content type these bytes were stored with.
    pub content_type: String,
    /// Pixel width, when the format carried one.
    pub width: Option<i32>,
    /// Pixel height.
    pub height: Option<i32>,
    /// What the uploader said about this version.
    pub note: String,
    /// Who created it; `None` on a backfilled version 1.
    pub created_by: Option<Uuid>,
    /// When it was created.
    pub created_at: OffsetDateTime,
}

impl MediaVersion {
    /// Size in bytes as an unsigned number.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size_bytes.max(0).unsigned_abs()
    }
}

/// A new version, as the caller describes it.
#[derive(Debug, Clone)]
pub struct NewVersion {
    /// The version number the row will carry — assigned by [`next_version`], never by the caller.
    pub version: i32,
    /// Object key the bytes were written to.
    pub storage_key: String,
    /// Size of the bytes.
    pub size_bytes: i64,
    /// Hex-encoded SHA-256 of the bytes.
    pub checksum: String,
    /// Content type the bytes are stored with.
    pub content_type: String,
    /// What the header of the bytes said.
    pub probe: MediaProbe,
    /// What the uploader said about this version.
    pub note: String,
    /// Who uploaded it.
    pub created_by: Option<Uuid>,
}

impl NewVersion {
    /// Describe an upload, reducing the note to a bounded form.
    #[must_use]
    pub fn new(
        storage_key: impl Into<String>,
        size_bytes: i64,
        checksum: impl Into<String>,
        content_type: impl Into<String>,
        probe: MediaProbe,
        created_by: Option<Uuid>,
    ) -> Self {
        Self {
            version: 0,
            storage_key: storage_key.into(),
            size_bytes,
            checksum: checksum.into(),
            content_type: content_type.into(),
            probe,
            note: String::new(),
            created_by,
        }
    }

    /// Attach a note, reduced to a single line and capped.
    #[must_use]
    pub fn with_note(mut self, note: &str) -> Self {
        self.note = normalize_note(note);
        self
    }

    #[cfg(test)]
    fn row(&self, media_id: Uuid) -> MediaVersion {
        let (width, height, _, _) = self.probe.columns();
        MediaVersion {
            id: Uuid::nil(),
            media_id,
            version: self.version,
            storage_key: self.storage_key.clone(),
            size_bytes: self.size_bytes,
            checksum: self.checksum.clone(),
            content_type: self.content_type.clone(),
            width,
            height,
            note: self.note.clone(),
            created_by: self.created_by,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }
}

/// Reduce a version note to one bounded line.
#[must_use]
pub fn normalize_note(raw: &str) -> String {
    let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= MAX_NOTE_LENGTH {
        return one_line;
    }
    one_line.chars().take(MAX_NOTE_LENGTH).collect()
}

/// A transaction the media library opened, so a caller never has to name `sqlx`.
///
/// The number a version gets and the row that carries it have to be written in the same
/// transaction, and that transaction has to belong to *this* crate: a route that opened it
/// itself would be holding a `sqlx::Error` where everything else here is a [`MediaError`], and
/// the two do not share a `From` impl — so the honest boundary is that the library opens and
/// commits its own transaction and hands back only its own errors.
pub type VersionTransaction<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

/// Open the transaction a replace or a restore runs in.
pub async fn begin_version(pool: &sqlx::PgPool) -> Result<VersionTransaction<'static>> {
    Ok(pool.begin().await?)
}

/// Commit a version transaction.
pub async fn commit_version(transaction: VersionTransaction<'_>) -> Result<()> {
    transaction.commit().await?;
    Ok(())
}

/// The next free version number of one file.
///
/// The `media` row is locked for the duration of the statement, so two replaces that arrive
/// together are serialised: the first takes 4, releases the lock, the second then reads a `max`
/// that includes it and takes 5. Reading `max(version)` without the lock would let both take 4
/// and the second would fail the unique index — with a "duplicate key" message that names the
/// index and not the cause. The lock is a scalar subquery, which is where PostgreSQL permits
/// `FOR UPDATE`: an aggregate over `media_versions` is not a row, so locking it is a no-op.
pub async fn next_version(connection: &mut sqlx::PgConnection, media_id: Uuid) -> Result<i32> {
    let next: (i32,) = sqlx::query_as(
        "select coalesce((select max(version) from media_versions where media_id = $1), 0) + 1 \
         from media where id = $1 for update",
    )
    .bind(media_id)
    .fetch_optional(connection)
    .await?
    .ok_or(MediaError::NotFound)?;
    Ok(next.0)
}

/// Append one version and make it the current one.
///
/// The insert and the update are one statement, so a reader can never see `version_count` say 3
/// with only two rows in the table: either both land or neither does.
pub async fn append_version(
    connection: &mut sqlx::PgConnection,
    media_id: Uuid,
    version: &NewVersion,
    columns: (Option<i32>, Option<i32>, Option<i32>, Option<i32>),
    exif: &crate::exif::Exif,
) -> Result<MediaVersion> {
    let (width, height, duration_ms, page_count) = columns;
    let row: MediaVersion = sqlx::query_as(
        "insert into media_versions (media_id, version, storage_key, size_bytes, checksum, \
           content_type, width, height, note, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         returning id, media_id, version, storage_key, size_bytes, checksum, content_type, \
           width, height, note, created_by, created_at",
    )
    .bind(media_id)
    .bind(version.version)
    .bind(&version.storage_key)
    .bind(version.size_bytes)
    .bind(&version.checksum)
    .bind(&version.content_type)
    .bind(width)
    .bind(height)
    .bind(&version.note)
    .bind(version.created_by)
    .fetch_one(&mut *connection)
    .await?;

    // The current pointer moves to the new bytes in the same transaction the caller runs in, so
    // the panel read path follows the newest version while the old keys stay downloadable.
    //
    // The geometry is written *oriented* (`swap` decides) and the record is written with it, for
    // the reason [`fill_exif`] gives: a row holding the orientation from one statement and the
    // dimensions from another is a picture half-rotated. The version row itself keeps what the
    // header said, so a version list shows the file as the camera stored it while the panel shows
    // it as the browser draws it — one fact, two honest readings.
    let swap = matches!(exif.orientation, Some(5..=8));
    sqlx::query(
        "update media set storage_key = $2, size_bytes = $3, checksum = $4, content_type = $5, \
           width = case when $11 then coalesce($7, $6) else coalesce($6, width) end, \
           height = case when $11 then coalesce($6, $7) else coalesce($7, height) end, \
           duration_ms = coalesce($8, duration_ms), page_count = coalesce($9, page_count), \
           exif = $12, \
           version_count = $10, updated_at = now() \
         where id = $1",
    )
    .bind(media_id)
    .bind(&version.storage_key)
    .bind(version.size_bytes)
    .bind(&version.checksum)
    .bind(&version.content_type)
    .bind(width)
    .bind(height)
    .bind(duration_ms)
    .bind(page_count)
    .bind(version.version)
    .bind(swap)
    .bind(if exif.is_empty() {
        None::<serde_json::Value>
    } else {
        Some(exif.to_value())
    })
    .execute(&mut *connection)
    .await?;

    Ok(row)
}

/// Write what the header of the bytes said onto the file's own row.
///
/// Every value is a `coalesce`, so a format the probe could not read leaves the column it does
/// not know alone — a replacement that arrives without a readable header must not erase the
/// dimensions of the file it is replacing, or the detail screen's preview box collapses to a
/// default ratio for a file that still has one.
pub async fn fill_dimensions(
    pool: &sqlx::PgPool,
    media_id: Uuid,
    columns: (Option<i32>, Option<i32>, Option<i32>, Option<i32>),
) -> Result<()> {
    let (width, height, duration_ms, page_count) = columns;
    sqlx::query(
        "update media set \
           width = coalesce($2, width), \
           height = coalesce($3, height), \
           duration_ms = coalesce($4, duration_ms), \
           page_count = coalesce($5, page_count) \
         where id = $1",
    )
    .bind(media_id)
    .bind(width)
    .bind(height)
    .bind(duration_ms)
    .bind(page_count)
    .execute(pool)
    .await?;
    Ok(())
}

/// Write the camera record and the *oriented* geometry onto the file's own row.
///
/// One statement, because the two are a single fact: the geometry of a stored photograph and the
/// record describing it are read together by every layout that reserves a box, and a row holding
/// the orientation from one call and the dimensions from another is a picture half-rotated.
///
/// Three decisions the statement is built around:
///
///  * **The record is replaced, not merged.** A replacement's bytes are a different photograph;
///    leaving the previous body's lens on a file that has been re-shot is a wrong fact, not a
///    stale cache. A file whose format carries no EXIF clears the column rather than inheriting
///    the previous version's — a screenshot uploaded over a camera original must not keep
///    claiming to have been shot on a body it was never near.
///  * **The geometry written is the geometry on screen.** A picture stored sideways
///    (`orientation` 5–8) is a portrait photograph as every browser will draw it, so the columns
///    a grid reserves have to be the ones a reader sees — and the transformation route compares a
///    preset's size against these columns to decide whether it would enlarge the source, which
///    would be the wrong answer for a file the panel displays rotated. The raw orientation stays
///    in the record, so a downloader that applies it does not rotate a second time.
///  * **A file with no record is not a file with an empty one.** The column is null, so "we never
///    read a camera block" and "the camera said nothing" stay two different rows in a report
///    rather than collapsing into the same `{}`.
pub async fn fill_exif(
    pool: &sqlx::PgPool,
    media_id: Uuid,
    exif: &crate::exif::Exif,
    dimensions: (Option<i32>, Option<i32>),
) -> Result<()> {
    // The swap is expressed in SQL rather than here, because `coalesce` has to see the *stored*
    // columns to fall back on them: a rotation cannot be computed in Rust from values the row
    // only holds, and recomputing it there would mean reading the row first.
    let rotated = matches!(exif.orientation, Some(5..=8));
    sqlx::query(
        "update media set \
           exif = $2, \
           width = case when $5 then coalesce($4, $3) else coalesce($3, width) end, \
           height = case when $5 then coalesce($3, $4) else coalesce($4, height) end \
         where id = $1",
    )
    .bind(media_id)
    .bind(if exif.is_empty() {
        None::<serde_json::Value>
    } else {
        Some(exif.to_value())
    })
    .bind(dimensions.0)
    .bind(dimensions.1)
    .bind(rotated)
    .execute(pool)
    .await?;
    Ok(())
}

/// The whole history of one file, newest first.
pub async fn list_versions(pool: &sqlx::PgPool, media_id: Uuid) -> Result<Vec<MediaVersion>> {
    let rows = sqlx::query_as(
        "select id, media_id, version, storage_key, size_bytes, checksum, content_type, width, \
           height, note, created_by, created_at \
         from media_versions where media_id = $1 order by version desc",
    )
    .bind(media_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One version of one file, or `None` when the file has no such version.
pub async fn find_version(
    pool: &sqlx::PgPool,
    media_id: Uuid,
    version: i32,
) -> Result<Option<MediaVersion>> {
    let row = sqlx::query_as(
        "select id, media_id, version, storage_key, size_bytes, checksum, content_type, width, \
           height, note, created_by, created_at \
         from media_versions where media_id = $1 and version = $2",
    )
    .bind(media_id)
    .bind(version)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// How many versions one file has, counted rather than trusted from the counter column.
pub async fn count_versions(pool: &sqlx::PgPool, media_id: Uuid) -> Result<i64> {
    let (count,): (i64,) =
        sqlx::query_as("select count(*) from media_versions where media_id = $1")
            .bind(media_id)
            .fetch_one(pool)
            .await?;
    Ok(count)
}

/// Every storage key a file owns, newest first — what a purge has to remove.
///
/// The current key comes from the `media` row and the history from the table, so a file whose
/// counter disagrees with its table still hands the purge every key it holds.
pub async fn all_storage_keys(pool: &sqlx::PgPool, media_id: Uuid) -> Result<Vec<String>> {
    let keys = sqlx::query_scalar(
        "select storage_key from media where id = $1 \
         union \
         select storage_key from media_versions where media_id = $1",
    )
    .bind(media_id)
    .fetch_all(pool)
    .await?;
    Ok(keys)
}

/// Write the current row's bytes into the history as version 1.
///
/// An upload that lands before the history table existed — or on a row that no backfill reached —
/// still needs a version 1, or the detail screen opens on a file with an empty history. The
/// insert is conditional, so a file that already has a version 1 keeps it.
pub async fn ensure_version_one(
    pool: &sqlx::PgPool,
    media_id: Uuid,
    version: &NewVersion,
) -> Result<Option<MediaVersion>> {
    let row: Option<MediaVersion> = sqlx::query_as(
        "insert into media_versions (media_id, version, storage_key, size_bytes, checksum, \
           content_type, width, height, note, created_by) \
         select $1, 1, storage_key, size_bytes, checksum, content_type, width, height, $2, $3 \
         from media where id = $1 \
         on conflict (media_id, version) do nothing \
         returning id, media_id, version, storage_key, size_bytes, checksum, content_type, \
           width, height, note, created_by, created_at",
    )
    .bind(media_id)
    .bind(&version.note)
    .bind(version.created_by)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Reduce a probe's numbers to a description for the version row.
#[must_use]
pub fn probe_of(head: &[u8], content_type: &str) -> MediaProbe {
    crate::probe::probe(content_type, head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::MediaProbe;

    #[test]
    fn a_note_is_one_bounded_line() {
        assert_eq!(normalize_note("  fixed   the\ncrop  "), "fixed the crop");
        let long = "x".repeat(MAX_NOTE_LENGTH + 50);
        let reduced = normalize_note(&long);
        assert_eq!(reduced.chars().count(), MAX_NOTE_LENGTH);
    }

    #[test]
    fn an_empty_note_is_allowed() {
        let version = NewVersion::new("k", 1, "c", "image/png", MediaProbe::default(), None);
        assert_eq!(version.with_note("   ").note, "");
    }

    #[test]
    fn a_new_version_carries_what_the_header_said() {
        let probe = MediaProbe {
            width: Some(1200),
            height: Some(800),
            duration_ms: None,
            page_count: None,
        };
        let version = NewVersion::new("sites/1/a.png", 10, "abc", "image/png", probe, None)
            .with_note("second crop");
        let row = version.row(Uuid::nil());
        assert_eq!(row.width, Some(1200));
        assert_eq!(row.height, Some(800));
        assert_eq!(row.note, "second crop");
        assert_eq!(row.media_id, Uuid::nil());
    }

    #[test]
    fn a_probe_without_a_duration_leaves_the_column_alone() {
        // A replaced text file must not inherit the previous version's page count.
        let probe = MediaProbe {
            page_count: Some(4),
            ..MediaProbe::default()
        };
        let (_, _, duration, pages) = probe.columns();
        assert_eq!(duration, None);
        assert_eq!(pages, Some(4));
    }
}
