//! The database side of the registry: one function per source.
//!
//! Each source runs **one** SQL statement that narrows its own table down to candidate rows
//! (title/summary style matching, tenant-scoped, capped at [`MAX_CANDIDATES`] and ordered
//! newest-first), then the shared ranker decides which candidates match every term and in what
//! order they are shown. Pushing the narrowing into SQL keeps the ranker pure and keeps a
//! search from reading a whole table into memory at the sizes a real installation reaches.
//!
//! Scope: the caller's organization is bound as `$2` and `null` means "the whole platform" —
//! exactly the tenancy rule the rest of the panel runs on (`apps/api/src/scope.rs`): an account
//! with a primary organization sees inside it, a platform-level account sees everything.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::catalogue::SourceSpec;
use crate::error::Result;
use crate::query::{Hit, MAX_CANDIDATES, Query, freshness, score, sort_and_truncate};

/// Candidate row of the content source.
#[derive(Debug, sqlx::FromRow)]
struct PageRow {
    id: Uuid,
    site_id: Uuid,
    title: String,
    summary: Option<String>,
    slug: String,
    site_name: String,
    status: String,
    updated_at: OffsetDateTime,
    /// Title of the revision that matched (the latest one), which may differ from `title`.
    matched_title: String,
    /// Summary of the revision that matched.
    matched_summary: Option<String>,
}

/// Candidate row of the media source.
#[derive(Debug, sqlx::FromRow)]
struct MediaRow {
    id: Uuid,
    site_id: Uuid,
    filename: String,
    content_type: String,
    size_bytes: i64,
    site_name: String,
    created_at: OffsetDateTime,
}

/// Candidate row of the site source.
#[derive(Debug, sqlx::FromRow)]
struct SiteRow {
    id: Uuid,
    key: String,
    name: String,
    status: String,
    updated_at: OffsetDateTime,
}

/// Run one source of the registry.
///
/// Every key in [`crate::catalogue::SEARCH_SOURCES`] needs an arm here — the registry is the
/// single list, and an unknown key is a programming error rather than a runtime state.
pub async fn run(
    spec: &SourceSpec,
    pool: &PgPool,
    organization_id: Option<Uuid>,
    query: &Query,
    limit: usize,
) -> Result<Vec<Hit>> {
    match spec.key {
        "pages" => search_pages(pool, organization_id, query, limit).await,
        "media" => search_media(pool, organization_id, query, limit).await,
        "sites" => search_sites(pool, organization_id, query, limit).await,
        other => {
            debug_assert!(false, "source {other} has no query function");
            Ok(Vec::new())
        }
    }
}

/// Pages of every site in scope, matched on the title or summary of any of their revisions.
///
/// The hit shows the page's **latest** revision (the working draft, or the published one when
/// the draft was retired), while the match may ride an older revision: a page renamed yesterday
/// must still be findable by the name it carried last week. That is why the query reads the
/// matching revision in its own lateral join — the row carries both the shown title and the
/// matched one, and the ranker sees both.
pub async fn search_pages(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    query: &Query,
    limit: usize,
) -> Result<Vec<Hit>> {
    let rows = sqlx::query_as::<_, PageRow>(
        "select p.id, p.site_id, rev.title, rev.summary, p.slug, s.name as site_name, p.status, \
         p.updated_at, matched.title as matched_title, matched.summary as matched_summary \
         from pages p \
         join sites s on s.id = p.site_id \
         join lateral ( \
             select title, summary from page_revisions \
             where page_id = p.id order by revision_no desc limit 1 \
         ) rev on true \
         join lateral ( \
             select r.title, r.summary from page_revisions r \
             where r.page_id = p.id \
               and (lower(r.title) like any($1::text[]) \
                    or lower(coalesce(r.summary, '')) like any($1::text[])) \
             order by r.revision_no desc limit 1 \
         ) matched on true \
         where ($2::uuid is null or s.organization_id = $2) \
         order by p.updated_at desc, p.id asc \
         limit $3",
    )
    .bind(query.patterns())
    .bind(organization_id)
    .bind(MAX_CANDIDATES)
    .fetch_all(pool)
    .await?;

    let now = OffsetDateTime::now_utc();
    let mut hits = Vec::with_capacity(rows.len());
    for row in rows {
        let subtitle = format!("{} · /{} · {}", row.site_name, row.slug, row.status);
        // The matched revision and the summary participate in matching (the SQL matched them
        // too), capped so a long text cannot dominate the score.
        let summary: String = row
            .summary
            .as_deref()
            .unwrap_or("")
            .chars()
            .take(400)
            .collect();
        let matched_summary: String = row
            .matched_summary
            .as_deref()
            .unwrap_or("")
            .chars()
            .take(400)
            .collect();
        let Some(base) = score(
            query.terms(),
            &row.title,
            Some(&subtitle),
            &[
                row.slug.as_str(),
                row.site_name.as_str(),
                row.status.as_str(),
                summary.as_str(),
                row.matched_title.as_str(),
                matched_summary.as_str(),
            ],
        ) else {
            continue;
        };
        hits.push(Hit {
            source: "pages",
            id: row.id.to_string(),
            title: row.title,
            subtitle: Some(subtitle),
            url: format!("/pages?site={}&focus={}", row.site_id, row.id),
            score: base + freshness(Some(row.updated_at), now),
            updated_at: Some(row.updated_at),
        });
    }
    Ok(sort_and_truncate(hits, limit))
}

/// Files of the media libraries in scope, matched on the file name.
pub async fn search_media(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    query: &Query,
    limit: usize,
) -> Result<Vec<Hit>> {
    let rows = sqlx::query_as::<_, MediaRow>(
        "select m.id, m.site_id, m.filename, m.content_type, m.size_bytes, s.name as site_name, \
         m.created_at \
         from media m \
         join sites s on s.id = m.site_id \
         where lower(m.filename) like any($1::text[]) \
           and ($2::uuid is null or s.organization_id = $2) \
         order by m.created_at desc, m.id asc \
         limit $3",
    )
    .bind(query.patterns())
    .bind(organization_id)
    .bind(MAX_CANDIDATES)
    .fetch_all(pool)
    .await?;

    let now = OffsetDateTime::now_utc();
    let mut hits = Vec::with_capacity(rows.len());
    for row in rows {
        let subtitle = format!(
            "{} · {} · {}",
            row.site_name,
            row.content_type,
            human_size(row.size_bytes)
        );
        let Some(base) = score(
            query.terms(),
            &row.filename,
            Some(&subtitle),
            &[row.content_type.as_str(), row.site_name.as_str()],
        ) else {
            continue;
        };
        hits.push(Hit {
            source: "media",
            id: row.id.to_string(),
            title: row.filename,
            subtitle: Some(subtitle),
            url: format!("/media?site={}&focus={}", row.site_id, row.id),
            score: base + freshness(Some(row.created_at), now),
            updated_at: Some(row.created_at),
        });
    }
    Ok(sort_and_truncate(hits, limit))
}

/// Sites in scope, matched on the display name or the stable key.
pub async fn search_sites(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    query: &Query,
    limit: usize,
) -> Result<Vec<Hit>> {
    let rows = sqlx::query_as::<_, SiteRow>(
        "select s.id, s.key, s.name, s.status, s.updated_at \
         from sites s \
         where (lower(s.name) like any($1::text[]) or lower(s.key) like any($1::text[])) \
           and ($2::uuid is null or s.organization_id = $2) \
         order by s.updated_at desc, s.id asc \
         limit $3",
    )
    .bind(query.patterns())
    .bind(organization_id)
    .bind(MAX_CANDIDATES)
    .fetch_all(pool)
    .await?;

    let now = OffsetDateTime::now_utc();
    let mut hits = Vec::with_capacity(rows.len());
    for row in rows {
        let subtitle = format!("key “{}” · {}", row.key, row.status);
        let Some(base) = score(
            query.terms(),
            &row.name,
            Some(&subtitle),
            &[row.key.as_str(), row.status.as_str()],
        ) else {
            continue;
        };
        hits.push(Hit {
            source: "sites",
            id: row.id.to_string(),
            title: row.name,
            subtitle: Some(subtitle),
            url: "/sites".to_owned(),
            score: base + freshness(Some(row.updated_at), now),
            updated_at: Some(row.updated_at),
        });
    }
    Ok(sort_and_truncate(hits, limit))
}

/// A byte count a person can read (`1.2 MB`, `845 B`).
#[must_use]
pub fn human_size(bytes: i64) -> String {
    const KIB: f64 = 1024.0;
    let bytes = bytes.max(0) as f64;
    if bytes < KIB {
        return format!("{} B", bytes as i64);
    }
    let kib = bytes / KIB;
    if kib < KIB {
        return format!("{kib:.0} KB");
    }
    let mib = kib / KIB;
    if mib < KIB {
        return format!("{mib:.1} MB");
    }
    let gib = mib / KIB;
    format!("{gib:.1} GB")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_reads_like_a_person_wrote_it() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(845), "845 B");
        assert_eq!(human_size(2048), "2 KB");
        assert_eq!(human_size(1_248_576), "1.2 MB");
        assert_eq!(human_size(2_147_483_648), "2.0 GB");
        assert_eq!(human_size(-5), "0 B");
    }
}
