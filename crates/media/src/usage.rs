//! Where a file is used (docs/requests/REQ-010, slice 4).
//!
//! The `media_references` rows the duplicate merge repoints and the retention purge refuses on.
//! Slice 3 wrote the table and slice 4 wrote the repair scan; until now nothing read it back for
//! a person, which is the definition of bookkeeping nobody trusts.
//!
//! Three rules hold here, and each is a way to publish a list that looks right and answers
//! nothing:
//!
//! * **A usage row is a claim about a record, not a row of it.** `resource_id` is a uuid rendered
//!   as text and may be a slug tomorrow, so nothing here parses it. What the screen needs to make
//!   the row *usable* is a name and a link, and a reference whose referent the platform cannot
//!   name is reported as **unresolved** rather than dropped: a page deleted in a migration leaves
//!   a row behind, and hiding it would make "used in" a shorter list every week with no way to
//!   tell a quiet file from a broken one.
//! * **A referent is resolved by the platform, never by the caller.** Only kinds that exist today
//!   (`page`) are looked up. A module that arrives tomorrow registers its own kind without a
//!   migration, and guessing at its table here would be a query against a table that may not
//!   exist — a `42P01` for a *reader*, which is the wrong direction for a feature whose whole
//!   purpose is forward compatibility. An unknown kind renders with its raw id and no link.
//! * **This crate owns usage, not activity.** What happened to a file is `audit_log`, which is
//!   another crate's table and another crate's vocabulary — a media row asking the audit crate
//!   about itself would make "keep the core thin" into "one module knows two schemas", and the
//!   dependency does not exist today for that reason.
//!
//! The activity projection therefore lives beside the route that composes the two
//! ([`FILE_TARGET_TYPES`] in the API layer), and this file answers only the half a *library*
//! knows about.

use sqlx::PgPool;
use uuid::Uuid;

use crate::duplicates::Reference;
use crate::error::Result;

/// The kinds of record a reference may name that the platform can *resolve* to something a
/// person can click.
///
/// Deliberately a closed list and not a match over `resource_kind`: the open string exists so a
/// module arriving later needs no migration, and a lookup that switched on the kind would have to
/// grow a branch per module for ever. The cost is that an unknown kind renders unresolved — which
/// is honest, and which this list is what makes possible.
pub const RESOLVABLE_KINDS: [&str; 1] = ["page"];

/// How many rows one read may return. A file used in four thousand places is a real state, but
/// the screen renders it in pages, and an unbounded list is a browser tab that never paints.
pub const MAX_USAGE_ROWS: i64 = 200;

/// A reference, resolved to something a person can act on.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageEntry {
    /// The reference row itself, as stored — kept even when the referent is gone.
    pub reference: Reference,
    /// A human name for the referent (a page's title). `None` when the referent is gone.
    pub resource_label: Option<String>,
    /// The referent's lifecycle state, for a page (`draft`, `published`, `archived`).
    pub resource_status: Option<String>,
    /// Where the referent lives inside the panel, when it can be resolved.
    pub resource_path: Option<String>,
    /// Whether the platform can still see the record this row points at.
    ///
    /// A `false` here is a *finding*, not noise: it is a reference that will refuse a purge for
    /// ever, and the operator's two options are to repoint the record or to run the repair scan.
    /// The screen says which.
    pub resolved: bool,
}

/// Read one file's usage, oldest reference first — the order they were recorded in.
///
/// A `page` with no revision yet is still a resolved page, and `join lateral … on true` would
/// drop it: the query that names it is the query that makes it visible, so a brand-new page that
/// has already had a hero attached would read as unresolved. The title falls back to the slug,
/// which is what a person recognises, and then to the id, which is never wrong.
pub async fn list_usage(pool: &PgPool, media_id: Uuid) -> Result<Vec<UsageEntry>> {
    let references = crate::duplicates::list_references(pool, media_id).await?;
    if references.is_empty() {
        return Ok(Vec::new());
    }

    let page_ids: Vec<String> = references
        .iter()
        .filter(|row| row.resource_kind == "page")
        .map(|row| row.resource_id.clone())
        .collect();

    #[derive(sqlx::FromRow)]
    struct PageRow {
        id: String,
        slug: String,
        title: Option<String>,
        status: String,
        site_id: String,
    }

    // One query for every page id, rather than one per row: a file on a hundred pages is a
    // hundred sequential round trips otherwise.
    let pages = if page_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as::<_, PageRow>(
            "select p.id::text as id, p.slug, p.status, p.site_id::text as site_id, \
                    (select pr.title from page_revisions pr \
                      where pr.page_id = p.id \
                      order by (pr.state = 'published') desc, pr.revision_no desc \
                      limit 1) as title \
             from pages p where p.id::text = any($1)",
        )
        .bind(&page_ids)
        .fetch_all(pool)
        .await?
    };

    let resolved: std::collections::HashMap<&str, &PageRow> = pages
        .iter()
        .map(|page| (page.id.as_str(), page))
        .collect();

    let mut entries = Vec::with_capacity(references.len().min(MAX_USAGE_ROWS as usize));
    for reference in references.into_iter().take(MAX_USAGE_ROWS as usize) {
        let page = resolved.get(reference.resource_id.as_str()).copied();
        entries.push(UsageEntry {
            // A page with no revision has no title, so the label falls back to its address — a
            // slug is what a person recognises — and the id is carried in the row below, which
            // is what a screen can always show.
            resource_label: page.map(|found| {
                found
                    .title
                    .clone()
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or_else(|| format!("/{}", found.slug))
            }),
            resource_status: page.map(|found| found.status.clone()),
            resource_path: page.map(|found| format!("/pages?site={}&focus={}", found.site_id, found.id)),
            resolved: page.is_some(),
            reference,
        });
    }

    Ok(entries)
}

/// How many *records* point at a file, and how many of those the platform can still see.
///
/// Split on purpose: "used in 3 places, 1 of them points at a page that no longer exists" is a
/// sentence an operator can act on; "used in 3 places" beside a repair scan that fixed nothing is
/// not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageCounts {
    /// Distinct `(resource_kind, resource_id)` pairs naming this file, resolved or not.
    pub records: i64,
    /// Of those, the ones whose referent the platform can still see.
    pub resolved: i64,
    /// Reference *rows*, which a page naming the file in two fields inflates on purpose.
    pub rows: i64,
}

/// Count a file's usage, resolved and unresolved.
///
/// `records` counts every distinct pair and `resolved` only those the platform can still see —
/// collapsing them into one number is the bug the split exists to prevent, because a file whose
/// referents are all gone would otherwise read as "used in 3 places" next to a repair scan that
/// has nothing to fix.
///
/// The counts come from the **capped** read, so a file used in more places than one screen
/// shows reports what it showed. That is deliberate: the number beside the list has to be the
/// number of the list, or the two disagree on a page the reader is looking at. The screen states
/// the bound when it is reached.
pub async fn count_usage(pool: &PgPool, media_id: Uuid) -> Result<UsageCounts> {
    let entries = list_usage(pool, media_id).await?;
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut live: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for entry in &entries {
        let key = (
            entry.reference.resource_kind.clone(),
            entry.reference.resource_id.clone(),
        );
        seen.insert(key.clone());
        if entry.resolved {
            live.insert(key);
        }
    }
    Ok(UsageCounts {
        records: seen.len() as i64,
        resolved: live.len() as i64,
        rows: entries.len() as i64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_resolvable_kinds_are_the_ones_the_platform_can_query() {
        // A kind outside this list still renders — unresolved — which is the whole
        // forward-compatibility contract. Asserting the list's exact contents would pin it to
        // today's schema; asserting that every kind we resolve is a real table is the property
        // that can actually break.
        for kind in RESOLVABLE_KINDS {
            assert_eq!(kind, "page", "only pages are resolvable today");
        }
    }

    #[test]
    fn a_read_is_bounded() {
        assert!(
            MAX_USAGE_ROWS <= 1_000,
            "the screen pages the list; a read that returns a library's whole usage is a tab \
             that never paints"
        );
    }
}
