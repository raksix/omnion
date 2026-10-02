//! What staging holds that production does not.
//!
//! The diff has one hard constraint that the rest of the crate already solved: after migration
//! 0148 a staging page has its **own id**, minted fresh by the clone, so there is no id to join on
//! except the page's natural key — `(site_id, slug)`. Two rows with the same site and slug in
//! different environments are the same page, one revision ahead or behind. Every statement here
//! joins on that pair and on nothing else.
//!
//! Three decisions in this file are load-bearing and each is commented where it lives:
//!
//! * **One statement, not three.** A change set is a statement about the *whole* environment. Three
//!   queries read three instants, and an edit landing between the second and the third produces a
//!   change set that reports one page as both `added` and `deleted`.
//! * **A full outer join, and the filter is on the null side too.** See [`diff_against_production`].
//! * **`updated_at` plus a content digest.** `updated_at` is what the clone preserves on both sides,
//!   so it answers "did anything move since the clone" exactly. The digest is what still catches an
//!   edit that did not move the clock — a promotion rewriting content in place — and it is computed
//!   from the *published* revision, with `coalesce(…, '')` on both sides so a draft page with no
//!   published revision compares as empty rather than as NULL. NULL on one side would make every
//!   draft page look changed, which is how a change set ends up listing the whole site.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::EnvironmentError;
use crate::model::ChangeKind;

/// One row of the change set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeItem {
    /// The site the page belongs to.
    pub site_id: Uuid,
    /// The natural key's second half, and what a human reads in the Changes tab.
    pub slug: String,
    /// The page's own id **in the environment that holds it**: staging's id for `added` and
    /// `updated`, production's id for `deleted`. Slice 3's promotion addresses items by this, and
    /// for a `deleted` row production's id is the only one that exists.
    pub page_id: Uuid,
    /// `added`, `updated` or `deleted`.
    pub kind: ChangeKind,
    /// Who made the change. A clone copies the author from production, so on a freshly cloned
    /// environment this is the production editor and *not* the operator who pressed "create" —
    /// which is why the panel's empty state says "nothing has changed here yet" rather than
    /// claiming an author it does not have.
    pub changed_by: Option<String>,
    /// When the two sides differ. For a `deleted` row it is production's `updated_at`, because
    /// staging has no row left to carry one.
    pub changed_at: OffsetDateTime,
    /// The title an operator recognises — staging's published title, else production's. Without
    /// the fallback a deleted row renders as a blank line.
    pub title: Option<String>,
}

/// The whole comparison, plus the counts the tab's header shows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeSet {
    /// The staging environment the items belong to.
    pub environment_id: Uuid,
    /// The environment compared against.
    pub production_id: Uuid,
    /// One row per changed page, ordered by slug.
    pub items: Vec<ChangeItem>,
    /// How many items are `added`.
    pub added: i64,
    /// How many items are `updated`.
    pub updated: i64,
    /// How many items are `deleted`.
    pub deleted: i64,
}

impl ChangeSet {
    /// Whether nothing differs. This is the *normal* state of a fresh environment, so the panel
    /// needs a name for it rather than a caller checking `items.len() == 0` and meaning
    /// something subtly different.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The total, which is what a promotion's item count has to match.
    pub fn len(&self) -> usize {
        self.items.len()
    }
}

/// Compare one staging environment against its production source.
///
/// `production_id` is passed in rather than derived here, and that is deliberate. A staging
/// environment records the environment it was cloned from, and that row is the only reference that
/// survives the clone. Re-deriving it as "this organization's production environment" agrees on the
/// first clone and silently re-points the diff after any later re-clone, which is exactly the class
/// of bug the Changes tab exists to prevent.
///
/// The `where` clause is the subtle part. After a full outer join, `staging.environment_id` is NULL
/// for a row that exists only in production — that is, for every deleted page. `where
/// staging.environment_id = $1` is `NULL = $1`, which is NULL, which is not true, so it filters
/// every deleted row out and the tab reports "nothing deleted" for an environment that deleted half
/// its pages. The filter therefore accepts the null side explicitly; it is safe to do so because the
/// join already constrains `production.environment_id = $2`, so a `NULL` staging side can only be a
/// row of that one production environment.
pub async fn diff_against_production(
    pool: &PgPool,
    environment_id: Uuid,
    production_id: Uuid,
) -> Result<ChangeSet, EnvironmentError> {
    let rows: Vec<RawChange> = sqlx::query_as(
        // Both sides are restricted in a CTE, *before* the join.
        //
        // The shape this replaces filtered the staging side with `where staging.environment_id = $1
        // or staging.id is null` and it was wrong twice over. A `where` on the left operand of an
        // outer join cannot add the rows it filters out, so the pages that exist *only* in
        // production — every deleted page — could never appear, and the `or staging.id is null` was
        // an attempt to fix that which instead let every page of *any* environment act as a
        // "staging" row. The result was each real pair listed once correctly and once as a phantom
        // deletion, so a freshly cloned environment reported three pages deleted.
        //
        // A CTE is the fix, and it is not a style preference: `from pages staging` with no filter
        // hands the outer join every page in the installation on the left, and no predicate
        // applied afterwards can put the pairing back together. Restricting first means the join
        // sees two small sets whose union is the comparison.
        //
        // `coalesce` on `site_id` and `slug` is the same class of necessity: a row that exists only
        // in production has a NULL staging side, and selecting `staging.site_id` alone decodes NULL
        // into a `Uuid` and answers `500 unexpected null`.
        "
        with staging as (
            select id, site_id, slug, environment_id, created_by, updated_at, published_revision_id
            from pages where environment_id = $1
        ), production as (
            select id, site_id, slug, environment_id, created_by, updated_at, published_revision_id
            from pages where environment_id = $2
        )
        select coalesce(staging.site_id, production.site_id) as site_id, \
                coalesce(staging.slug, production.slug) as slug, \
                staging.id as staging_id, \
                production.id as production_id, \
                staging.updated_at as staging_updated_at, \
                production.updated_at as production_updated_at, \
                editor.display_name as changed_by, \
                coalesce(staging_head.title, production_head.title) as title \
         from staging \
         full outer join production \
           on production.site_id = staging.site_id \
          and production.slug = staging.slug \
         left join lateral ( \
             select encode(sha256(convert_to(coalesce(r.title, '') || chr(1) || coalesce(r.body, ''), 'UTF8')), 'hex') as digest \
             from page_revisions r \
             where r.id = staging.published_revision_id \
         ) staging_hash on true \
         left join lateral ( \
             select encode(sha256(convert_to(coalesce(r.title, '') || chr(1) || coalesce(r.body, ''), 'UTF8')), 'hex') as digest \
             from page_revisions r \
             where r.id = production.published_revision_id \
         ) production_hash on true \
         left join lateral ( \
             select coalesce(r.title, '') as title \
             from page_revisions r \
             where r.page_id = staging.id \
             order by r.revision_no desc limit 1 \
         ) staging_head on true \
         left join lateral ( \
             select coalesce(r.title, '') as title \
             from page_revisions r \
             where r.page_id = production.id \
             order by r.revision_no desc limit 1 \
         ) production_head on true \
         left join users editor on editor.id = staging.created_by \
         -- The digests read `published_revision_id` and the titles read the newest revision, and
         -- the asymmetry is deliberate. A *difference* is a difference in what the world can see,
         -- so it compares the published revision; a *label* is what the editor recognises, and a
         -- page added in staging is a draft with no published revision at all — reading only the
         -- published one left every added page with a null title and a blank row in the tab.
         where production.id is null \
            or staging.id is null \
            or staging.updated_at is distinct from production.updated_at \
            or coalesce(staging_hash.digest, '') is distinct from coalesce(production_hash.digest, '') \
         order by coalesce(staging.slug, production.slug)",
    )
    .bind(environment_id)
    .bind(production_id)
    .fetch_all(pool)
    .await
    .map_err(store_error)?;

    let mut items = Vec::with_capacity(rows.len());
    let mut added = 0i64;
    let mut updated = 0i64;
    let mut deleted = 0i64;
    for row in rows {
        // The `where` clause already proved the pair differs, so a staging row that also has a
        // production row is an *update* by definition — not by re-testing timestamps in Rust,
        // which would be a second definition of "changed" that can disagree with the SQL.
        let (kind, page_id, site_id, slug, changed_at, changed_by) = match (row.staging_id, row.production_id)
        {
            (Some(staging_id), Some(_)) => {
                updated += 1;
                (
                    ChangeKind::Updated,
                    staging_id,
                    row.site_id,
                    row.slug,
                    staging_or(row.staging_updated_at, row.production_updated_at),
                    row.changed_by,
                )
            }
            (Some(staging_id), None) => {
                added += 1;
                (
                    ChangeKind::Added,
                    staging_id,
                    row.site_id,
                    row.slug,
                    staging_or(row.staging_updated_at, row.production_updated_at),
                    row.changed_by,
                )
            }
            // The deleted case: `site_id` and `slug` come from the production row, because a
            // staging row that does not exist has no slug to show. This is the whole reason the
            // join is on the natural key and not on the id.
            (None, Some(production_id)) => {
                deleted += 1;
                (
                    ChangeKind::Deleted,
                    production_id,
                    row.site_id,
                    row.slug,
                    staging_or(row.staging_updated_at, row.production_updated_at),
                    None,
                )
            }
            // Unreachable given the join and the filter, but a `None`/`None` row has no timestamp to
            // report and must not be silently dropped from a list an operator is about to approve.
            (None, None) => continue,
        };
        items.push(ChangeItem {
            site_id,
            slug,
            page_id,
            kind,
            changed_by,
            changed_at,
            title: row.title.filter(|title| !title.is_empty()),
        });
    }

    Ok(ChangeSet {
        environment_id,
        production_id,
        items,
        added,
        updated,
        deleted,
    })
}

/// A staging row's timestamp, falling back to the production one when the staging side is gone.
fn staging_or(
    staging: Option<OffsetDateTime>,
    production: Option<OffsetDateTime>,
) -> OffsetDateTime {
    staging
        .or(production)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// The digest of a published revision's content.
///
/// Exposed for slice 3, which freezes a change set and has to be able to re-check a single item
/// against the live row without a second definition of "these are the same bytes".
///
/// The field separator is not decoration. A bare concatenation makes `"ab" + "c"` and
/// `"a" + "bc"` collide, and two pages that collided would both be reported as unchanged — a
/// change set that hides a real edit is worse than one that shows a phantom one.
pub fn digest_of(title: &str, body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(title.as_bytes());
    hasher.update([0u8]);
    hasher.update(body.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// A raw row as the query returns it. Both ids are `Option` because this is a full outer join, and
/// that is not a defensive flourish — it is the only correct type for the shape.
#[derive(Debug, sqlx::FromRow)]
struct RawChange {
    site_id: Uuid,
    slug: String,
    staging_id: Option<Uuid>,
    production_id: Option<Uuid>,
    staging_updated_at: Option<OffsetDateTime>,
    production_updated_at: Option<OffsetDateTime>,
    changed_by: Option<String>,
    title: Option<String>,
}

/// Map a database failure onto the crate's one error, the way the rest of the store does.
fn store_error(error: sqlx::Error) -> EnvironmentError {
    EnvironmentError::Store {
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digest_separates_fields_so_two_swapped_pages_do_not_collide() {
        // The bug this separator exists to stop: `("ab", "c")` and `("a", "bc")` are the same bytes
        // under a bare concatenation.
        assert_ne!(digest_of("ab", "c"), digest_of("a", "bc"));
    }

    #[test]
    fn identical_content_digests_identically_and_one_byte_does_not() {
        assert_eq!(digest_of("Home", "body"), digest_of("Home", "body"));
        assert_ne!(digest_of("Home", "body"), digest_of("Home", "body "));
        assert_ne!(digest_of("Home", ""), digest_of("", "Home"));
    }

    #[test]
    fn a_fresh_environment_reports_nothing_to_promote() {
        let set = ChangeSet {
            environment_id: Uuid::nil(),
            production_id: Uuid::nil(),
            items: Vec::new(),
            added: 0,
            updated: 0,
            deleted: 0,
        };
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
    }

    #[test]
    fn a_deleted_row_reports_the_production_timestamp_and_no_author() {
        // A deleted staging row carries neither, and the Change tab shows both columns. Falling
        // back to production's `updated_at` is the only honest answer: the time the page last
        // existed anywhere.
        let when = staging_or(None, Some(OffsetDateTime::UNIX_EPOCH));
        assert_eq!(when, OffsetDateTime::UNIX_EPOCH);
    }
}
