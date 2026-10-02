//! Pages and their revision history.
//!
//! A page is one addressable piece of content of one site (docs/01-VISION.md §7); its history
//! is a list of append-only revisions stamped with a `revision_no` (docs/05-VERSIONING.md §4).
//! The rules this module keeps:
//!
//! - a page always has at least one revision — revision 1 is written with the page;
//! - at most one revision is the working `draft` and at most one is `published` (the one
//!   visitors see); everything else is `archived`;
//! - editing content appends revision `n + 1` and archives the draft it supersedes;
//! - publishing freezes the draft and archives the revision it replaced;
//! - restoring copies an older revision forward as a new draft and records where it came from
//!   — history is never rewritten, which is what makes compare and restore possible.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{ContentError, Result};
use crate::model::{DEFAULT_PAGE_TYPE, NewPage, Page, PageChanges, PageRevision};
use crate::validation::{
    validate_body, validate_page_type, validate_slug, validate_status, validate_summary,
    validate_title,
};

/// Column list for every `Page` query.
const PAGE_COLUMNS: &str = "id, site_id, slug, page_type, status, published_revision_id, \
     created_by, created_at, updated_at";

/// Column list for every `PageRevision` query.
const REVISION_COLUMNS: &str = "id, page_id, revision_no, state, title, body, summary, \
     restored_from_id, created_by, created_at, published_at";

/// Create a page together with its first, draft revision.
///
/// Fails with [`ContentError::SlugTaken`] when the site already carries the slug.
pub async fn create_page(pool: &PgPool, new: NewPage) -> Result<(Page, PageRevision)> {
    let slug = validate_slug(&new.slug)?;
    let title = validate_title(&new.title)?;
    let body = validate_body(new.body.as_deref().unwrap_or_default())?;
    let summary = match new.summary.as_deref() {
        Some(summary) => validate_summary(summary)?,
        None => None,
    };
    let page_type = match new.page_type.as_deref() {
        Some(page_type) => validate_page_type(page_type)?,
        None => DEFAULT_PAGE_TYPE.to_owned(),
    };

    let mut tx = pool.begin().await?;

    let page_sql = format!(
        "insert into pages (site_id, slug, page_type, status, created_by) \
         values ($1, $2, $3, 'draft', $4) returning {PAGE_COLUMNS}"
    );
    let page: Page = sqlx::query_as(&page_sql)
        .bind(new.site_id)
        .bind(&slug)
        .bind(&page_type)
        .bind(new.created_by)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_page_write_error)?;

    let revision_sql = format!(
        "insert into page_revisions (page_id, revision_no, state, title, body, summary, created_by) \
         values ($1, 1, 'draft', $2, $3, $4, $5) returning {REVISION_COLUMNS}"
    );
    let revision: PageRevision = sqlx::query_as(&revision_sql)
        .bind(page.id)
        .bind(&title)
        .bind(&body)
        .bind(summary.as_deref())
        .bind(new.created_by)
        .fetch_one(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok((page, revision))
}

/// Look a page up by id.
pub async fn find_page(pool: &PgPool, id: Uuid) -> Result<Option<Page>> {
    let sql = format!("select {PAGE_COLUMNS} from pages where id = $1");
    sqlx::query_as::<_, Page>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// Look a page up by site and slug — the address the public renderer resolves (phase P07).
pub async fn find_page_by_slug(pool: &PgPool, site_id: Uuid, slug: &str) -> Result<Option<Page>> {
    let slug = validate_slug(slug)?;
    let sql = format!("select {PAGE_COLUMNS} from pages where site_id = $1 and slug = $2");
    sqlx::query_as::<_, Page>(&sql)
        .bind(site_id)
        .bind(&slug)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// Pages of a site, oldest first; `status` filters when given.
pub async fn list_pages(pool: &PgPool, site_id: Uuid, status: Option<&str>) -> Result<Vec<Page>> {
    let status = match status {
        Some(status) => Some(validate_status(status)?),
        None => None,
    };
    let sql = format!(
        "select {PAGE_COLUMNS} from pages \
         where site_id = $1 and ($2::text is null or status = $2) \
         order by created_at asc, id asc"
    );
    sqlx::query_as::<_, Page>(&sql)
        .bind(site_id)
        .bind(status)
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

/// Apply `changes` to a page.
///
/// A slug change renames the page; a content change (title, body or summary) appends the next
/// revision as the working draft and archives the draft it supersedes. An empty change set
/// returns the page untouched.
pub async fn update_page(
    pool: &PgPool,
    id: Uuid,
    changes: &PageChanges,
    editor: Option<Uuid>,
) -> Result<Page> {
    let current = find_page(pool, id)
        .await?
        .ok_or(ContentError::PageNotFound)?;
    if changes.is_empty() {
        return Ok(current);
    }

    let mut tx = pool.begin().await?;

    if let Some(slug) = &changes.slug {
        let slug = validate_slug(slug)?;
        let sql = format!(
            "update pages set slug = $2, updated_at = now() where id = $1 returning {PAGE_COLUMNS}"
        );
        let _renamed: Page = sqlx::query_as(&sql)
            .bind(id)
            .bind(&slug)
            .fetch_one(&mut *tx)
            .await
            .map_err(map_page_write_error)?;
    }

    if changes.touches_content() {
        // A page always carries at least one revision, so the newest one is the base the new
        // draft is built from.
        let base_sql = format!(
            "select {REVISION_COLUMNS} from page_revisions where page_id = $1 \
             order by revision_no desc limit 1"
        );
        let base: PageRevision = sqlx::query_as(&base_sql)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;

        let title = match &changes.title {
            Some(title) => validate_title(title)?,
            None => base.title.clone(),
        };
        let body = match &changes.body {
            Some(body) => validate_body(body)?,
            None => base.body.clone(),
        };
        let summary = match &changes.summary {
            Some(summary) => validate_summary(summary)?,
            None => base.summary.clone(),
        };

        sqlx::query(
            "update page_revisions set state = 'archived' \
             where page_id = $1 and state = 'draft'",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;

        let insert_sql = format!(
            "insert into page_revisions (page_id, revision_no, state, title, body, summary, created_by) \
             values ($1, $2, 'draft', $3, $4, $5, $6) returning {REVISION_COLUMNS}"
        );
        let _draft: PageRevision = sqlx::query_as(&insert_sql)
            .bind(id)
            .bind(base.revision_no + 1)
            .bind(&title)
            .bind(&body)
            .bind(summary.as_deref())
            .bind(editor)
            .fetch_one(&mut *tx)
            .await?;

        sqlx::query("update pages set updated_at = now() where id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }

    let sql = format!("select {PAGE_COLUMNS} from pages where id = $1");
    let updated: Page = sqlx::query_as(&sql).bind(id).fetch_one(&mut *tx).await?;
    tx.commit().await?;

    Ok(updated)
}

/// Publish the page's working draft (docs/05-VERSIONING.md §6).
///
/// The draft becomes `published` — visitors now see it — and the revision it replaced becomes
/// `archived`. The page keeps a pointer at the published revision, so the renderer never has
/// to search the history. Fails with [`ContentError::NoDraftRevision`] when there is nothing
/// to publish.
pub async fn publish_page(pool: &PgPool, id: Uuid) -> Result<(Page, PageRevision)> {
    if find_page(pool, id).await?.is_none() {
        return Err(ContentError::PageNotFound);
    }

    let mut tx = pool.begin().await?;

    let draft_sql = format!(
        "select {REVISION_COLUMNS} from page_revisions where page_id = $1 and state = 'draft'"
    );
    let draft: Option<PageRevision> = sqlx::query_as(&draft_sql)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(draft) = draft else {
        return Err(ContentError::NoDraftRevision);
    };

    sqlx::query(
        "update page_revisions set state = 'archived' \
         where page_id = $1 and state = 'published'",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;

    let promote_sql = format!(
        "update page_revisions set state = 'published', published_at = now() \
         where id = $1 returning {REVISION_COLUMNS}"
    );
    let published: PageRevision = sqlx::query_as(&promote_sql)
        .bind(draft.id)
        .fetch_one(&mut *tx)
        .await?;

    let page_sql = format!(
        "update pages set status = 'published', published_revision_id = $2, updated_at = now() \
         where id = $1 returning {PAGE_COLUMNS}"
    );
    let page: Page = sqlx::query_as(&page_sql)
        .bind(id)
        .bind(published.id)
        .fetch_one(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok((page, published))
}

/// Restore an earlier revision: copy it forward as a new draft.
///
/// History is never rewritten — the older content becomes a new draft revision that records
/// the row it came from (`restored_from_id`), and the draft it supersedes is archived. The
/// caller publishes the restored draft when it is ready (docs/05-VERSIONING.md §4, §6).
pub async fn restore_revision(
    pool: &PgPool,
    page_id: Uuid,
    revision_id: Uuid,
    restored_as: Option<Uuid>,
) -> Result<PageRevision> {
    if find_page(pool, page_id).await?.is_none() {
        return Err(ContentError::PageNotFound);
    }

    let mut tx = pool.begin().await?;

    let source_sql =
        format!("select {REVISION_COLUMNS} from page_revisions where id = $1 and page_id = $2");
    let source: Option<PageRevision> = sqlx::query_as(&source_sql)
        .bind(revision_id)
        .bind(page_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(source) = source else {
        return Err(ContentError::RevisionNotFound);
    };

    let next_no: i32 = sqlx::query_scalar(
        "select coalesce(max(revision_no), 0) + 1 from page_revisions where page_id = $1",
    )
    .bind(page_id)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        "update page_revisions set state = 'archived' \
         where page_id = $1 and state = 'draft'",
    )
    .bind(page_id)
    .execute(&mut *tx)
    .await?;

    let insert_sql = format!(
        "insert into page_revisions \
         (page_id, revision_no, state, title, body, summary, restored_from_id, created_by) \
         values ($1, $2, 'draft', $3, $4, $5, $6, $7) returning {REVISION_COLUMNS}"
    );
    let restored: PageRevision = sqlx::query_as(&insert_sql)
        .bind(page_id)
        .bind(next_no)
        .bind(&source.title)
        .bind(&source.body)
        .bind(source.summary.as_deref())
        .bind(source.id)
        .bind(restored_as)
        .fetch_one(&mut *tx)
        .await?;

    sqlx::query("update pages set updated_at = now() where id = $1")
        .bind(page_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(restored)
}

/// Delete a page, its revisions and their translation rows. `false` when it was already gone.
///
/// Translations are keyed by resource id (no foreign key — the table is resource-agnostic), so
/// they are removed explicitly rather than left pointing at revisions that no longer exist.
pub async fn delete_page(pool: &PgPool, id: Uuid) -> Result<bool> {
    let mut tx = pool.begin().await?;

    sqlx::query(
        "delete from translations \
         where resource_type = $1 and resource_id in \
         (select id from page_revisions where page_id = $2)",
    )
    .bind(crate::model::REVISION_RESOURCE)
    .bind(id)
    .execute(&mut *tx)
    .await?;

    let deleted = sqlx::query("delete from pages where id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        > 0;

    tx.commit().await?;
    Ok(deleted)
}

/// The page's working draft, when it has one.
pub async fn current_draft(pool: &PgPool, page_id: Uuid) -> Result<Option<PageRevision>> {
    revision_in_state(pool, page_id, "draft").await
}

/// The revision visitors currently see, when the page has been published.
pub async fn published_revision(pool: &PgPool, page_id: Uuid) -> Result<Option<PageRevision>> {
    revision_in_state(pool, page_id, "published").await
}

/// The newest revision of a page — the `Current` row of the history view.
pub async fn latest_revision(pool: &PgPool, page_id: Uuid) -> Result<Option<PageRevision>> {
    let sql = format!(
        "select {REVISION_COLUMNS} from page_revisions where page_id = $1 \
         order by revision_no desc limit 1"
    );
    sqlx::query_as::<_, PageRevision>(&sql)
        .bind(page_id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// Every revision of a page, newest first (the history list of the admin panel).
pub async fn list_revisions(pool: &PgPool, page_id: Uuid) -> Result<Vec<PageRevision>> {
    let sql = format!(
        "select {REVISION_COLUMNS} from page_revisions where page_id = $1 \
         order by revision_no desc"
    );
    sqlx::query_as::<_, PageRevision>(&sql)
        .bind(page_id)
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

/// Look one revision up inside a page.
pub async fn find_revision(
    pool: &PgPool,
    page_id: Uuid,
    revision_id: Uuid,
) -> Result<Option<PageRevision>> {
    let sql =
        format!("select {REVISION_COLUMNS} from page_revisions where id = $1 and page_id = $2");
    sqlx::query_as::<_, PageRevision>(&sql)
        .bind(revision_id)
        .bind(page_id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

async fn revision_in_state(
    pool: &PgPool,
    page_id: Uuid,
    state: &str,
) -> Result<Option<PageRevision>> {
    let sql =
        format!("select {REVISION_COLUMNS} from page_revisions where page_id = $1 and state = $2");
    sqlx::query_as::<_, PageRevision>(&sql)
        .bind(page_id)
        .bind(state)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// A unique violation on `pages` is the per-site slug. Any other write failure is a database
/// error the operator has to look at.
fn map_page_write_error(err: sqlx::Error) -> ContentError {
    match err {
        sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
            ContentError::SlugTaken
        }
        other => ContentError::Database(other),
    }
}
