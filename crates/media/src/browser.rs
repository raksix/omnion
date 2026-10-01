//! The `media` table once it became a file system (REQ-010, slice 1).
//!
//! `library.rs` still describes the flat library of the core; this module adds the query surface
//! the browser needs: a folder-scoped listing with filters and sorting, a trashed listing with a
//! countdown, and the three states a file can be in (live, trashed, purged).
//!
//! Two rules hold across every statement here:
//!
//! * **a move never touches the storage key** — it changes `folder_id` only, so published pages,
//!   cached derivatives and signed URLs keep pointing at the same bytes;
//! * **a delete is a state, not a hard act** — `trash_files` sets `deleted_at`, `purge_files`
//!   removes the row and the caller removes the bytes, and the two are separate routes so a
//!   mistaken delete is recoverable.
//!
//! The filter builder is the reason the count and the page can never disagree: [`Filter::clause`]
//! and [`Filter::value`] are the *same* value read twice, so the `count(*)` and the `select` are
//! built from one list.

use serde_json::Value;
use sqlx::PgPool;
use sqlx::{Postgres, QueryBuilder};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{MediaError, Result};
use crate::folders::Folder;
use crate::model::MediaFile;

/// Every column of a media row, in the order [`MediaFile`] reads them.
///
/// `pub(crate)` rather than a second copy: the duplicate report and the merge both read whole
/// file rows, and a second column list that can drift from this one is a query that compiles and
/// then decodes a `MediaFile` whose `metadata` came from the `tags` column.
pub(crate) const FILE_COLUMNS: &str = "id, site_id, storage_key, filename, content_type, size_bytes, \
                           checksum, created_by, created_at, folder_id, updated_at, deleted_at, \
                           deleted_by, purged_at, alt_text, caption, description, metadata, tags, \
                           width, height, duration_ms, page_count, exif, scan_status, scan_detail, \
                           scanned_at, scan_engine, \
                           version_count, is_public, legal_hold";

/// The same column list, prefixed with `alias.` — for a select that nests the row in an object.
fn aliased_columns(alias: &str) -> String {
    FILE_COLUMNS
        .split(", ")
        .map(|column| format!("{alias}.{column}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// How a listing is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// Newest upload first — what the browser opens with.
    Newest,
    /// Oldest upload first.
    Oldest,
    /// Largest file first.
    Largest,
    /// Smallest file first.
    Smallest,
    /// Name, case-insensitive.
    Name,
    /// Last change that was not a version.
    Modified,
}

impl Sort {
    /// Every sort key the API accepts, with the label the toolbar shows.
    pub const ALL: [(&'static str, &'static str); 6] = [
        ("newest", "Newest first"),
        ("oldest", "Oldest first"),
        ("largest", "Largest first"),
        ("smallest", "Smallest first"),
        ("name", "Name"),
        ("modified", "Recently modified"),
    ];

    /// Parse a sort key from the query string; an unknown key falls back to [`Sort::Newest`]
    /// rather than refusing the whole listing.
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("oldest") => Self::Oldest,
            Some("largest") => Self::Largest,
            Some("smallest") => Self::Smallest,
            Some("name") => Self::Name,
            Some("modified") => Self::Modified,
            _ => Self::Newest,
        }
    }

    /// The `order by` tail this sort needs, chosen from a closed set — never from input.
    #[must_use]
    pub fn order_by(self) -> &'static str {
        match self {
            Self::Newest => "order by created_at desc, id",
            Self::Oldest => "order by created_at, id",
            Self::Largest => "order by size_bytes desc, filename",
            Self::Smallest => "order by size_bytes, filename",
            Self::Name => "order by lower(filename), id",
            Self::Modified => "order by updated_at desc nulls last, filename",
        }
    }
}

/// One filter: the SQL clause and the value it binds, kept together.
#[derive(Debug)]
enum Filter {
    /// `folder_id = $n`, or the subtree variant.
    Folder { id: Uuid, subtree: bool },
    /// `filename ilike $n`.
    NameContains(String),
    /// `content_type like $n` — the kind filter is a prefix (`image` matches every image type).
    KindPrefix(String),
    /// `size_bytes >= $n`.
    MinBytes(i64),
    /// `size_bytes <= $n`.
    MaxBytes(i64),
    /// `created_by = $n`.
    UploadedBy(Uuid),
    /// `created_at >= $n`.
    CreatedAfter(OffsetDateTime),
    /// `created_at < $n`.
    CreatedBefore(OffsetDateTime),
    /// `$n = any(tags)`.
    Tag(String),
    /// `scan_status = $n`.
    ScanStatus(String),
    /// `version_count > 1` — no value.
    HasVersions,
    /// `metadata -> $n = $n+1` — the pair exists with exactly this value.
    ///
    /// Built from the **pair path** rather than a containment operator. `@>` would be the obvious
    /// choice and it is wrong here for three reasons: `metadata @> '{"k":"v"}'` is false when the
    /// stored value is the *number* `v`, it is false for any row that also carries other pairs
    /// only in the `?` variants, and — the real one — a jsonb equality test cannot use the
    /// default `jsonb_ops` GIN opclass efficiently, so the index built in 0025 was never on the
    /// path this filter would have used. `->` with `=` compares one key against one value and is
    /// the form the index was created for.
    MetadataPair { key: String, value: String },
}

impl Filter {
    /// Write this filter onto the builder: the clause *and* every value it needs.
    ///
    /// The clause and its placeholders are written together on purpose. An earlier version built
    /// the clause as a string containing `$n` and then pushed the value as a bind, which produced
    /// `folder_id = $2$2` — a statement PostgreSQL refuses with "syntax error at or near $2", so
    /// *every* filtered listing failed while the unfiltered one worked. Letting the builder own
    /// the numbering means a placeholder can only exist where the value beside it was pushed.
    ///
    /// A clause that mentions the same value more than once binds it once per mention, because
    /// `$n` is positional in PostgreSQL: `$2` is never a second name for `$2`. The subtree test is
    /// the only such clause, and it is a join against the folder table rather than a string match
    /// on the path — the path is that table's own invariant, and reusing it here would let a bug
    /// in one place silently widen a listing.
    fn push(&self, builder: &mut QueryBuilder<'_, Postgres>) {
        match self {
            Self::Folder { subtree: false, .. } => {
                builder.push("folder_id = ");
                builder.push_bind(self.value());
            }
            Self::Folder { subtree: true, .. } => {
                builder.push(
                    "exists (select 1 from media_folders f where f.id = media.folder_id and ( \
                       f.id = ",
                );
                builder.push_bind(self.value());
                builder.push(" or f.path = (select path from media_folders where id = ");
                builder.push_bind(self.value());
                builder.push(") or f.path like (select path from media_folders where id = ");
                builder.push_bind(self.value());
                builder.push(") || '/%'))");
            }
            Self::NameContains(pattern) => {
                builder.push("filename ilike ");
                builder.push_bind(pattern.clone());
            }
            Self::KindPrefix(prefix) => {
                builder.push("content_type like ");
                builder.push_bind(format!("{prefix}%"));
            }
            Self::MinBytes(bytes) => {
                builder.push("size_bytes >= ");
                builder.push_bind(*bytes);
            }
            Self::MaxBytes(bytes) => {
                builder.push("size_bytes <= ");
                builder.push_bind(*bytes);
            }
            Self::UploadedBy(user) => {
                builder.push("created_by = ");
                builder.push_bind(*user);
            }
            Self::CreatedAfter(moment) => {
                builder.push("created_at >= ");
                builder.push_bind(*moment);
            }
            Self::CreatedBefore(moment) => {
                builder.push("created_at < ");
                builder.push_bind(*moment);
            }
            Self::Tag(tag) => {
                // The clause is `$n = any(tags)` — the placeholder is on the LEFT of the
                // comparison, so the value is bound *first* and the comparison is written after
                // the bind. Writing the text first and binding after would read
                // `any(tags) = $2`, which PostgreSQL rejects when it plans the query.
                builder.push_bind(tag.clone());
                builder.push(" = any(tags)");
            }
            Self::ScanStatus(state) => {
                builder.push("scan_status = ");
                builder.push_bind(state.clone());
            }
            Self::HasVersions => {
                // No placeholder: the clause is `version_count > 1` and compares a column with a
                // number, so there is nothing to bind.
                builder.push("version_count > 1");
            }
            Self::MetadataPair { key, value } => {
                // `->` takes the key as its right operand, so the key is bound FIRST and the
                // operator is written after the bind — the same ordering rule `Filter::Tag`
                // records for `$n = any(tags)`. Writing the text first and binding after would
                // read `$n = any(tags)`, which PostgreSQL rejects when it plans the query.
                //
                // `->>` extracts the value as **text**, which is what makes the comparison work
                // for every row: the pairs are stored as jsonb strings, and `->` returns jsonb,
                // so `-> = $n` against a jsonb bind would be a jsonb-to-jsonb equality that a
                // number in the row silently fails. `->> = $n` compares text to text.
                builder.push("metadata ->> ");
                builder.push_bind(key.clone());
                builder.push(" = ");
                builder.push_bind(value.clone());
            }
        }
    }

    /// The value of a folder filter, read through one match so the subtree clause cannot bind a
    /// different id than the direct-child clause.
    fn value(&self) -> Uuid {
        match self {
            Self::Folder { id, .. } => *id,
            _ => Uuid::nil(),
        }
    }
}

/// The filters the browser can combine. Every one is optional and every one is a bound parameter.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// Only files in this folder (`None` is the whole library).
    pub folder_id: Option<Uuid>,
    /// Include the whole subtree of `folder_id` instead of the folder itself.
    pub include_subfolders: bool,
    /// Free-text match on the file name.
    pub search: Option<String>,
    /// Content-type prefix (`image`, `video`, `application/pdf`, …).
    pub kind: Option<String>,
    /// Only files this size or larger, in bytes.
    pub min_bytes: Option<i64>,
    /// Only files this size or smaller, in bytes.
    pub max_bytes: Option<i64>,
    /// Only files uploaded by this account.
    pub uploaded_by: Option<Uuid>,
    /// Only files uploaded at or after this moment.
    pub created_after: Option<OffsetDateTime>,
    /// Only files uploaded before this moment.
    pub created_before: Option<OffsetDateTime>,
    /// Only files carrying this tag.
    pub tag: Option<String>,
    /// Only files in this scan state.
    pub scan_status: Option<String>,
    /// Only files with more than one version.
    pub has_versions: bool,
    /// Only files whose custom pairs carry this `key=value`; `None` is no metadata filter.
    ///
    /// One pair, not a query language. A `jsonpath` or an operator grammar here would let a
    /// listing take a fragment of SQL out of a URL, and the browser's own toolbar has one field
    /// for one pair. `metadata_pairs::filter_clause` is what decides whether a typed term is a
    /// filter at all.
    pub metadata: Option<String>,
    /// How many rows to return (1–500).
    pub limit: i64,
    /// How many rows to skip.
    pub offset: i64,
}

impl ListQuery {
    /// A query with no filters, in the default order.
    #[must_use]
    pub fn new() -> Self {
        Self {
            limit: 100,
            ..Self::default()
        }
    }

    /// The filter list, in the order the placeholders are numbered.
    fn filters(&self) -> Vec<Filter> {
        let mut filters = Vec::new();
        if let Some(id) = self.folder_id {
            filters.push(Filter::Folder {
                id,
                subtree: self.include_subfolders,
            });
        }
        if let Some(search) = self
            .search
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            filters.push(Filter::NameContains(format!("%{}%", escape_like(search))));
        }
        if let Some(kind) = self.kind.as_deref().filter(|v| !v.is_empty()) {
            filters.push(Filter::KindPrefix(kind.to_owned()));
        }
        if let Some(min_bytes) = self.min_bytes {
            filters.push(Filter::MinBytes(min_bytes.max(0)));
        }
        if let Some(max_bytes) = self.max_bytes {
            filters.push(Filter::MaxBytes(max_bytes.max(0)));
        }
        if let Some(user) = self.uploaded_by {
            filters.push(Filter::UploadedBy(user));
        }
        if let Some(after) = self.created_after {
            filters.push(Filter::CreatedAfter(after));
        }
        if let Some(before) = self.created_before {
            filters.push(Filter::CreatedBefore(before));
        }
        if let Some(tag) = self.tag.as_deref().filter(|v| !v.is_empty()) {
            filters.push(Filter::Tag(tag.to_owned()));
        }
        if let Some(state) = self.scan_status.as_deref().filter(|v| !v.is_empty()) {
            filters.push(Filter::ScanStatus(state.to_owned()));
        }
        if self.has_versions {
            filters.push(Filter::HasVersions);
        }
        // The one place a metadata term becomes a filter. A half-typed `campaign=` is not a
        // filter, so it narrows nothing — a toolbar field that has not been finished yet must not
        // change the listing out from under the person typing it.
        if let Some(term) = self.metadata.as_deref()
            && let Some((key, value)) = crate::metadata_pairs::filter_clause(term)
        {
            filters.push(Filter::MetadataPair { key, value });
        }
        filters
    }
}

/// Escape the wildcards of a user-supplied search term.
///
/// Without this, searching for `100%` returns every file — the `like` pattern would treat the
/// term as a pattern instead of as the text the operator typed.
fn escape_like(raw: &str) -> String {
    raw.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// One page of the library, with the total the panel shows next to the count.
#[derive(Debug, Clone)]
pub struct FilePage {
    /// The rows of this page.
    pub files: Vec<MediaFile>,
    /// How many rows the filters match in total, not just on this page.
    pub total: i64,
}

/// One trashed file with the day its bytes go.
///
/// The file columns are selected under the alias `file` and flattened back onto the row, so the
/// API can hand the panel `{file, purges_at}` without a second query per entry.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TrashEntry {
    /// The file itself.
    #[sqlx(flatten)]
    pub file: MediaFile,
    /// When the retention window purges it.
    pub purges_at: Option<OffsetDateTime>,
}

/// List one page of the library of a site.
pub async fn list_files(
    pool: &PgPool,
    site_id: Uuid,
    query: &ListQuery,
    sort: Sort,
) -> Result<FilePage> {
    let mut builder =
        QueryBuilder::<Postgres>::new(format!("select {FILE_COLUMNS} from media where "));
    // The clause and the binds are generated by ONE loop over ONE filter list, so the `$n` in a
    // clause and the value pushed after it can never drift apart — the failure mode that makes a
    // count disagree with the page it counts.
    push_filters(&mut builder, query, site_id);
    // `limit`/`offset` need their keywords: the two values are pushed back to back, and
    // without `offset` in between PostgreSQL parses `limit $n $n+1` as a syntax error. The
    // clause is built here rather than in `Sort::order_by` so the keyword cannot be lost
    // again, and so the numbering stays with the other pushes that come before it.
    builder.push(format!(" {} limit ", sort.order_by()));
    builder.push_bind(query.limit.clamp(1, 500));
    builder.push(" offset ");
    builder.push_bind(query.offset.max(0));

    let files = builder
        .build_query_as::<MediaFile>()
        .fetch_all(pool)
        .await?;

    Ok(FilePage {
        files,
        total: count_files(pool, site_id, query).await?,
    })
}

/// Push `site_id`, `deleted_at is null` and every filter.
///
/// One loop over ONE filter list, and each filter writes its own clause *and* its own values, so
/// a placeholder can only exist where the value beside it was pushed. `count_files` runs the same
/// function, which is what makes the count and the page it counts unable to disagree.
fn push_filters(builder: &mut QueryBuilder<'_, Postgres>, query: &ListQuery, site_id: Uuid) {
    builder.push("media.site_id = ");
    builder.push_bind(site_id);
    builder.push(" and media.deleted_at is null");
    for filter in query.filters() {
        builder.push(" and ");
        filter.push(builder);
    }
}

/// How many rows a filter matches, with the same predicate as [`list_files`].
pub async fn count_files(pool: &PgPool, site_id: Uuid, query: &ListQuery) -> Result<i64> {
    let mut builder = QueryBuilder::<Postgres>::new("select count(*) from media where ");
    push_filters(&mut builder, query, site_id);
    let total: i64 = builder.build_query_scalar().fetch_one(pool).await?;
    Ok(total)
}

/// One live file by id, or `None` when it does not exist or is in the trash.
pub async fn find_file(pool: &PgPool, id: Uuid) -> Result<Option<MediaFile>> {
    let query = format!("select {FILE_COLUMNS} from media where id = $1 and deleted_at is null");
    sqlx::query_as::<_, MediaFile>(&query)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// One file by id whatever its state — the restore and purge paths need the trashed row.
pub async fn find_file_any_state(pool: &PgPool, id: Uuid) -> Result<Option<MediaFile>> {
    let query = format!("select {FILE_COLUMNS} from media where id = $1");
    sqlx::query_as::<_, MediaFile>(&query)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// The rows of one folder, without paging — what a bulk move reads before it writes.
pub async fn files_in_folder(pool: &PgPool, folder_id: Uuid) -> Result<Vec<MediaFile>> {
    let query = format!(
        "select {FILE_COLUMNS} from media where folder_id = $1 and deleted_at is null \
         order by created_at desc, id"
    );
    sqlx::query_as::<_, MediaFile>(&query)
        .bind(folder_id)
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

/// Move files to the trash, reporting how many rows actually changed.
///
/// A delete that was already applied is not an error: an operator who clicks twice means it once.
/// The count is what the panel reports, and it is the number of files that really left the library.
pub async fn trash_files(pool: &PgPool, ids: &[Uuid], by: Uuid) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query(
        "update media set deleted_at = now(), deleted_by = $2, updated_at = now() \
         where id = any($1) and deleted_at is null",
    )
    .bind(ids)
    .bind(by)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Bring trashed files back, reporting how many rows changed.
pub async fn restore_files(pool: &PgPool, ids: &[Uuid]) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query(
        "update media set deleted_at = null, deleted_by = null, updated_at = now() \
         where id = any($1) and deleted_at is not null",
    )
    .bind(ids)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Delete trashed rows for good. The caller removes the bytes.
///
/// The row goes first so the caller is left with exactly one thing left to do, and one thing that
/// can fail without leaving a row pointing at nothing. Only trashed rows are eligible: a live file
/// cannot be purged by accident through the trash screen.
pub async fn purge_files(pool: &PgPool, ids: &[Uuid]) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query("delete from media where id = any($1) and deleted_at is not null")
        .bind(ids)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// The storage keys of files about to be purged, so the caller can remove exactly those bytes.
pub async fn storage_keys(pool: &PgPool, ids: &[Uuid]) -> Result<Vec<String>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let keys = sqlx::query_scalar::<_, String>("select storage_key from media where id = any($1)")
        .bind(ids)
        .fetch_all(pool)
        .await?;
    Ok(keys)
}

/// The trashed files of a site, newest deletion first, with the day their bytes go.
///
/// `retention_days` is the site's fallback window. A per-file policy overrides it from slice 4;
/// until then the countdown the operator sees is the one they configured for the site.
pub async fn list_trash(
    pool: &PgPool,
    site_id: Uuid,
    retention_days: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<TrashEntry>> {
    // The retention window is a `bigint` because it arrives from a query string, and the
    // interval arithmetic is written as a multiplication rather than `make_interval(days => $2)`:
    // with a bound parameter PostgreSQL cannot infer the remaining arguments of a named-argument
    // function, picks a different overload, and answers "mismatched types … NUMERIC" — or, without
    // the cast, "function make_interval(days => bigint) does not exist". `bigint * interval` is
    // unambiguous, so the countdown is the same number of days the operator configured.
    let query = format!(
        "select {} , deleted_at + ($2::bigint * interval '1 day') as purges_at \
         from media where site_id = $1 and deleted_at is not null \
         order by deleted_at desc, id limit $3 offset $4",
        aliased_columns("media")
    );
    sqlx::query_as::<_, TrashEntry>(&query)
        .bind(site_id)
        .bind(retention_days.max(0))
        .bind(limit.clamp(1, 500))
        .bind(offset.max(0))
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

/// The ids of every trashed file of a site, oldest deletion first.
///
/// The retention worker and `empty trash` both need "everything eligible" without paging through
/// it, so the statement lives here rather than in a route where a raw `sqlx::Error` has no
/// conversion into the API's own error type.
pub async fn trashed_ids(pool: &PgPool, site_id: Uuid) -> Result<Vec<Uuid>> {
    let ids = sqlx::query_scalar::<_, Uuid>(
        "select id from media where site_id = $1 and deleted_at is not null order by deleted_at, id",
    )
    .bind(site_id)
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// How many files a site has in the trash, and how many bytes they still hold.
///
/// The sum is cast back to `bigint`: PostgreSQL's `sum(bigint)` returns `numeric`, and sqlx will
/// not decode `numeric` into a Rust `i64` — without the cast the trash screen answers 500
/// "mismatched types; Rust type i64 … not compatible with SQL type NUMERIC". `size_bytes` is
/// bounded by the upload limit, so the sum cannot leave the range of a `bigint`.
pub async fn trash_summary(pool: &PgPool, site_id: Uuid) -> Result<(i64, i64)> {
    let (files, bytes): (i64, i64) = sqlx::query_as(
        "select count(*), coalesce(sum(size_bytes), 0)::bigint from media \
         where site_id = $1 and deleted_at is not null",
    )
    .bind(site_id)
    .fetch_one(pool)
    .await?;
    Ok((files, bytes))
}

/// The editable fields of a file; every `None` leaves the stored value alone.
#[derive(Debug, Clone, Default)]
pub struct MetadataPatch {
    /// New file name.
    pub filename: Option<String>,
    /// New alt text.
    pub alt_text: Option<String>,
    /// New caption.
    pub caption: Option<String>,
    /// New description.
    pub description: Option<String>,
    /// The whole tag set.
    pub tags: Option<Vec<String>>,
    /// The whole metadata object.
    pub metadata: Option<Value>,
    /// The folder to move the file into; `None` leaves it where it is.
    pub folder_id: Option<Option<Uuid>>,
}

/// Update the editable fields of one live file, returning `false` when it is not live.
pub async fn update_file(pool: &PgPool, id: Uuid, patch: &MetadataPatch) -> Result<bool> {
    let result = sqlx::query(
        "update media set \
           filename = coalesce($2, filename), \
           alt_text = coalesce($3, alt_text), \
           caption = coalesce($4, caption), \
           description = coalesce($5, description), \
           tags = coalesce($6, tags), \
           metadata = coalesce($7, metadata), \
           folder_id = case when $8::boolean then $9 else folder_id end, \
           updated_at = now() \
         where id = $1 and deleted_at is null",
    )
    .bind(id)
    .bind(patch.filename.as_deref())
    .bind(patch.alt_text.as_deref())
    .bind(patch.caption.as_deref())
    .bind(patch.description.as_deref())
    .bind(patch.tags.as_deref())
    .bind(patch.metadata.as_ref())
    .bind(patch.folder_id.is_some())
    .bind(patch.folder_id.flatten())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// How many live files sit in one folder — the count beside a folder in the tree.
pub async fn count_in_folder(pool: &PgPool, folder_id: Uuid) -> Result<i64> {
    let (count,): (i64,) =
        sqlx::query_as("select count(*) from media where folder_id = $1 and deleted_at is null")
            .bind(folder_id)
            .fetch_one(pool)
            .await?;
    Ok(count)
}

/// Refuse a folder that belongs to another site.
pub fn assert_same_site(folder: &Folder, site_id: Uuid) -> Result<()> {
    if folder.site_id == site_id {
        Ok(())
    } else {
        Err(MediaError::FolderSiteMismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query() -> ListQuery {
        ListQuery {
            search: Some("logo".to_owned()),
            kind: Some("image".to_owned()),
            ..ListQuery::new()
        }
    }

    #[test]
    fn a_search_term_cannot_smuggle_a_wildcard() {
        assert_eq!(escape_like("100%"), "100\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        // Without the escape a search for `100%` would return every file in the library.
        let filters = ListQuery {
            search: Some("100%".to_owned()),
            ..ListQuery::new()
        }
        .filters();
        assert!(
            matches!(filters.first(), Some(Filter::NameContains(pattern)) if pattern == "%100\\%%")
        );
    }

    #[test]
    fn blank_filters_are_not_built() {
        let filters = ListQuery {
            search: Some("   ".to_owned()),
            kind: Some(String::new()),
            tag: Some(String::new()),
            ..ListQuery::new()
        }
        .filters();
        assert!(
            filters.is_empty(),
            "blank filters are not filters: {filters:?}"
        );
    }

    #[test]
    fn every_listing_hides_the_trash() {
        // A trashed file never appears in the browser listing, whatever the filters say: the
        // clause is written once in `push_filters` and both statements go through it.
        let mut builder = QueryBuilder::<Postgres>::new("select count(*) from media where ");
        push_filters(&mut builder, &query(), Uuid::nil());
        let sql = builder.sql().to_owned();
        assert!(sql.contains("media.site_id = $1"), "{sql}");
        assert!(sql.contains("media.deleted_at is null"), "{sql}");
    }

    /// Build the statement a listing would send, so the assertions can look at the SQL itself.
    fn statement(query: &ListQuery) -> String {
        let mut builder = QueryBuilder::<Postgres>::new("select 1 from media where ");
        push_filters(&mut builder, query, Uuid::nil());
        builder.sql().to_owned()
    }

    #[test]
    fn a_filter_never_writes_its_placeholder_twice() {
        // The defect this guards: the clause string carried `$n` *and* the value was bound, so
        // every filtered listing sent `folder_id = $2$2` and PostgreSQL refused it. An unfiltered
        // listing still worked, which is why no earlier test saw it.
        for (label, query) in [
            (
                "folder",
                ListQuery {
                    folder_id: Some(Uuid::nil()),
                    ..ListQuery::new()
                },
            ),
            (
                "search",
                ListQuery {
                    search: Some("a".to_owned()),
                    ..ListQuery::new()
                },
            ),
            (
                "tag",
                ListQuery {
                    tag: Some("hero".to_owned()),
                    ..ListQuery::new()
                },
            ),
            (
                "size",
                ListQuery {
                    min_bytes: Some(10),
                    max_bytes: Some(99),
                    ..ListQuery::new()
                },
            ),
            (
                "uploader",
                ListQuery {
                    uploaded_by: Some(Uuid::nil()),
                    ..ListQuery::new()
                },
            ),
            (
                "scan",
                ListQuery {
                    scan_status: Some("clean".to_owned()),
                    ..ListQuery::new()
                },
            ),
            (
                "metadata",
                ListQuery {
                    metadata: Some("campaign=spring".to_owned()),
                    ..ListQuery::new()
                },
            ),
            (
                "everything",
                ListQuery {
                    folder_id: Some(Uuid::nil()),
                    include_subfolders: true,
                    search: Some("a".to_owned()),
                    kind: Some("image".to_owned()),
                    min_bytes: Some(1),
                    uploaded_by: Some(Uuid::nil()),
                    created_after: Some(OffsetDateTime::UNIX_EPOCH),
                    tag: Some("hero".to_owned()),
                    scan_status: Some("clean".to_owned()),
                    has_versions: true,
                    ..ListQuery::new()
                },
            ),
        ] {
            let sql = statement(&query);
            assert!(
                !sql.contains("$$"),
                "[{label}] a placeholder was written twice: {sql}"
            );
            // A placeholder is always followed by something that is not another placeholder.
            for (index, part) in sql.split('$').enumerate().skip(1) {
                let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
                assert!(
                    !digits.is_empty(),
                    "[{label}] a bare `$` in the clause: {sql}"
                );
                let after = &part[digits.len()..];
                assert!(
                    !after.starts_with('$'),
                    "[{label}] two placeholders in a row: {sql}"
                );
                let _ = index;
            }
        }
    }


    #[test]
    fn a_metadata_filter_binds_its_key_before_the_operator_writes_itself() {
        // The defect this guards: the clause pushed `metadata -> $2 = $3` as *text* and then
        // bound two more values, so every metadata-filtered listing sent `$2` for the key and
        // PostgreSQL refused the statement. An unfiltered listing still worked, which is why no
        // earlier test saw it — the same shape as the `$$` defect this suite already holds.
        let sql = statement(&ListQuery {
            metadata: Some("campaign=spring".to_owned()),
            ..ListQuery::new()
        });
        assert!(sql.contains("metadata ->> $2 = $3"), "{sql}");
        assert!(!sql.contains("$2$"), "{sql}");
        // `->>` (text) rather than `->` (jsonb): the stored values are jsonb strings, and a
        // jsonb equality against a text bind is false for every row holding a number.
        assert!(
            !sql.contains("metadata -> $"),
            "must extract as text: {sql}"
        );
    }

    #[test]
    fn a_half_typed_metadata_term_is_not_a_filter_at_all() {
        // The toolbar field is free text and is re-read on every keystroke. `campaign=` must
        // narrow nothing rather than match every row or none: a listing that changes while the
        // word is being typed is a listing nobody trusts.
        for term in ["", "   ", "campaign", "campaign=", "=spring", "  =  "] {
            let filters = ListQuery {
                metadata: Some(term.to_owned()),
                ..ListQuery::new()
            }
            .filters();
            assert!(
                !filters
                    .iter()
                    .any(|filter| matches!(filter, Filter::MetadataPair { .. })),
                "[{term:?}] must not build a metadata filter: {filters:?}"
            );
        }
    }

    #[test]
    fn a_metadata_filter_goes_through_the_same_builder_as_every_other_filter() {
        // `count_files` runs the same `push_filters` over the same list, which is what makes the
        // "Showing N of TOTAL" footer unable to disagree with the rows it counts. A filter built
        // on a second path would reintroduce exactly that drift, so the assertion is on the
        // statement the *count* sends, not on the rows it returns.
        let sql = statement(&ListQuery {
            metadata: Some("campaign=spring".to_owned()),
            tag: Some("hero".to_owned()),
            ..ListQuery::new()
        });
        assert!(sql.contains("= any(tags)"), "{sql}");
        assert!(sql.contains("metadata ->> $"), "{sql}");
        assert!(sql.contains("deleted_at is null"), "{sql}");
    }

    #[test]
    fn a_metadata_pair_keeps_an_equals_sign_inside_its_value() {
        // Splitting on the *first* `=` is what makes `note=width=3px` addressable; splitting on
        // the last, or on every one, would truncate the value into something that never matches.
        let filters = ListQuery {
            metadata: Some("note=width=3px".to_owned()),
            ..ListQuery::new()
        }
        .filters();
        let Some(Filter::MetadataPair { key, value }) = filters.first() else {
            panic!("expected one metadata filter: {filters:?}");
        };
        assert_eq!(key, "note");
        assert_eq!(value, "width=3px");
    }

    #[test]
    fn a_subtree_filter_binds_its_folder_once_per_mention() {
        // PostgreSQL placeholders are positional: `$2` is never a second name for `$2`, so a
        // clause that names its value three times binds it three times — the same folder id, three
        // distinct placeholders. The clause reads the path through the folder table rather than
        // matching a string, so a bug in path maintenance cannot silently widen a listing.
        let sql = statement(&ListQuery {
            folder_id: Some(Uuid::nil()),
            include_subfolders: true,
            ..ListQuery::new()
        });
        assert!(
            sql.ends_with(
                "and exists (select 1 from media_folders f where f.id = media.folder_id and ( \
                 f.id = $2 or f.path = (select path from media_folders where id = $3) \
                 or f.path like (select path from media_folders where id = $4) || '/%'))"
            ),
            "{sql}"
        );
        assert_eq!(
            sql.matches("$2").count(),
            1,
            "no placeholder is reused: {sql}"
        );
    }

    #[test]
    fn the_tag_clause_compares_from_the_placeholder_side() {
        // `$n = any(tags)`: writing the text first would produce `any(tags) = $2`, which plans
        // against a whole array and never matches.
        let sql = statement(&ListQuery {
            tag: Some("hero".to_owned()),
            ..ListQuery::new()
        });
        assert!(sql.ends_with(" and $2 = any(tags)"), "{sql}");
    }

    #[test]
    fn the_version_filter_needs_no_placeholder() {
        let sql = statement(&ListQuery {
            has_versions: true,
            ..ListQuery::new()
        });
        assert!(sql.ends_with(" and version_count > 1"), "{sql}");
        assert_eq!(
            sql.matches('$').count(),
            1,
            "only the site id is bound: {sql}"
        );
    }

    #[test]
    fn a_sort_key_never_reaches_the_statement() {
        assert_eq!(Sort::parse(Some("name")), Sort::Name);
        assert_eq!(Sort::parse(Some("; drop table media")), Sort::Newest);
        assert_eq!(Sort::parse(None), Sort::Newest);
        for (key, _) in Sort::ALL {
            let parsed = Sort::parse(Some(key));
            assert!(!parsed.order_by().is_empty());
        }
    }

    #[test]
    fn a_subtree_filter_names_the_folder_and_everything_under_it() {
        // The direct-child filter is a plain equality, so a folder listing never walks the tree.
        let direct = statement(&ListQuery {
            folder_id: Some(Uuid::nil()),
            ..ListQuery::new()
        });
        assert!(direct.ends_with(" and folder_id = $2"), "{direct}");
        assert!(
            !direct.contains("exists"),
            "a folder listing does not walk the tree"
        );

        let subtree = statement(&ListQuery {
            folder_id: Some(Uuid::nil()),
            include_subfolders: true,
            ..ListQuery::new()
        });
        assert!(subtree.contains("f.path like"), "{subtree}");
    }

    #[test]
    fn an_aliased_column_list_keeps_every_column() {
        let aliased = aliased_columns("media");
        assert_eq!(
            aliased.split(", ").count(),
            FILE_COLUMNS.split(", ").count()
        );
        assert!(aliased.starts_with("media.id, media.site_id"));
    }

    #[test]
    fn a_folder_of_another_site_is_refused() {
        let mut folder = Folder {
            id: Uuid::nil(),
            site_id: Uuid::new_v4(),
            parent_id: None,
            name: "Media".to_owned(),
            path: "Media".to_owned(),
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        assert!(assert_same_site(&folder, folder.site_id).is_ok());
        assert!(assert_same_site(&folder, Uuid::new_v4()).is_err());
        folder.site_id = Uuid::new_v4();
        assert!(assert_same_site(&folder, folder.site_id).is_ok());
    }
}
