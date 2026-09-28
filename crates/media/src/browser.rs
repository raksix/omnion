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
const FILE_COLUMNS: &str = "id, site_id, storage_key, filename, content_type, size_bytes, \
                           checksum, created_by, created_at, folder_id, updated_at, deleted_at, \
                           deleted_by, purged_at, alt_text, caption, description, metadata, tags, \
                           width, height, duration_ms, page_count, scan_status, scan_detail, \
                           version_count, is_public";

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
}

impl Filter {
    /// The SQL clause of this filter, with `$n` as its placeholder.
    fn clause(&self, index: usize) -> String {
        match self {
            Self::Folder { subtree: false, .. } => format!("folder_id = ${index}"),
            // The subtree test is a join against the folder table, not a string match on the
            // path: the path is that table's own invariant, and reusing it here would make a bug
            // in one place silently widen a listing.
            Self::Folder { subtree: true, .. } => format!(
                "exists (select 1 from media_folders f where f.id = media.folder_id and ( \
                   f.id = ${index} \
                   or f.path = (select path from media_folders where id = ${index}) \
                   or f.path like (select path from media_folders where id = ${index}) || '/%'))"
            ),
            Self::NameContains(_) => format!("filename ilike ${index}"),
            Self::KindPrefix(_) => format!("content_type like ${index}"),
            Self::MinBytes(_) => format!("size_bytes >= ${index}"),
            Self::MaxBytes(_) => format!("size_bytes <= ${index}"),
            Self::UploadedBy(_) => format!("created_by = ${index}"),
            Self::CreatedAfter(_) => format!("created_at >= ${index}"),
            Self::CreatedBefore(_) => format!("created_at < ${index}"),
            Self::Tag(_) => format!("${index} = any(tags)"),
            Self::ScanStatus(_) => format!("scan_status = ${index}"),
            Self::HasVersions => "version_count > 1".to_owned(),
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
    builder.push(format!(" {} limit ", sort.order_by()));
    builder.push_bind(query.limit.clamp(1, 500));
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

/// Push `site_id = $1`, `deleted_at is null` and every filter, numbering the placeholders here.
fn push_filters(builder: &mut QueryBuilder<'_, Postgres>, query: &ListQuery, site_id: Uuid) {
    builder.push("media.site_id = ");
    builder.push_bind(site_id);
    builder.push(" and media.deleted_at is null");
    for (offset, filter) in query.filters().iter().enumerate() {
        // `$1` is the site id, so the first filter is `$2`.
        builder.push(format!(" and {}", filter.clause(offset + 2)));
        push_filter(builder, filter);
    }
}

/// Push a filter's value onto a builder.
///
/// Written as statements rather than one `push_bind` expression because the `has-versions` filter
/// is a comparison against a constant and binds nothing.
fn push_filter(builder: &mut QueryBuilder<'_, Postgres>, filter: &Filter) {
    match filter {
        Filter::Folder { id, .. } => {
            builder.push_bind(*id);
        }
        Filter::NameContains(pattern) => {
            builder.push_bind(pattern.clone());
        }
        Filter::KindPrefix(prefix) => {
            builder.push_bind(format!("{prefix}%"));
        }
        Filter::MinBytes(bytes) | Filter::MaxBytes(bytes) => {
            builder.push_bind(*bytes);
        }
        Filter::UploadedBy(user) => {
            builder.push_bind(*user);
        }
        Filter::CreatedAfter(moment) | Filter::CreatedBefore(moment) => {
            builder.push_bind(*moment);
        }
        Filter::Tag(tag) => {
            builder.push_bind(tag.clone());
        }
        Filter::ScanStatus(state) => {
            builder.push_bind(state.clone());
        }
        Filter::HasVersions => {
            // No placeholder: the clause is `version_count > 1` and compares a column with a
            // number, so there is nothing to bind.
        }
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
    let query = format!(
        "select {} , deleted_at + make_interval(days => $2) as purges_at \
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
pub async fn trash_summary(pool: &PgPool, site_id: Uuid) -> Result<(i64, i64)> {
    let (files, bytes): (i64, i64) = sqlx::query_as(
        "select count(*), coalesce(sum(size_bytes), 0) from media \
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

    #[test]
    fn the_placeholder_numbers_follow_the_filter_order() {
        let filters = ListQuery {
            folder_id: Some(Uuid::nil()),
            search: Some("a".to_owned()),
            has_versions: true,
            ..ListQuery::new()
        }
        .filters();
        let clauses: Vec<String> = filters
            .iter()
            .enumerate()
            .map(|(offset, filter)| filter.clause(offset + 2))
            .collect();
        assert_eq!(clauses[0], "folder_id = $2");
        assert_eq!(clauses[1], "filename ilike $3");
        assert_eq!(clauses[2], "version_count > 1");
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
        let clause = Filter::Folder {
            id: Uuid::nil(),
            subtree: true,
        }
        .clause(2);
        assert!(clause.contains("f.path like"));
        // The direct-child filter is a plain equality, so a folder listing never walks the tree.
        assert_eq!(
            Filter::Folder {
                id: Uuid::nil(),
                subtree: false
            }
            .clause(2),
            "folder_id = $2"
        );
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
