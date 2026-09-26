//! `/api/v1/public` — the read surface the public site renderer consumes.
//!
//! `apps/web` renders a site's published pages from here. Unlike the panel surface this router
//! is deliberately **unauthenticated**: it answers with published revisions only
//! (docs/05-VERSIONING.md §6 — drafts and history never leave the panel), it exposes no
//! internal identifiers, and it resolves the addressed site from the request itself:
//!
//! 1. `?site=` — a host when the value contains a dot (`omnion.test`), otherwise a site key
//!    (`main`);
//! 2. the `X-Forwarded-Host`/`Host` header, so a site answers on its own domain;
//! 3. the installation's only site, when there is exactly one — the convenience that lets a
//!    fresh single-site install render without any configuration.
//!
//! An address that matches nothing answers `404`, and it answers the *same* 404 whether the
//! page does not exist or is simply not published yet: the public surface never discloses a
//! draft. Themes, blocks and translation overlays build on this response in later phases.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use omnion_content::{ContentError, pages};
use omnion_identity::Site;
use omnion_identity::sites;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use time::OffsetDateTime;

use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------------------------

/// Public identity of the site a response belongs to.
#[derive(Debug, Serialize)]
pub struct PublicSiteBody {
    /// Stable handle of the site inside its organization.
    pub key: String,
    /// Display name.
    pub name: String,
    /// Theme the renderer activates for this site; the renderer falls back to its default for a
    /// key it does not know, so a visitor never sees a broken page.
    pub theme: String,
}

/// Public identity of the page.
#[derive(Debug, Serialize)]
pub struct PublicPageBody {
    /// Address of the page inside its site.
    pub slug: String,
    /// Content type key (`page` today; the content type builder extends the set).
    pub page_type: String,
    /// Last change, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// The published revision a visitor sees.
#[derive(Debug, Serialize)]
pub struct PublicRevisionBody {
    /// Monotonic revision number inside the page.
    pub revision_no: i32,
    /// Title.
    pub title: String,
    /// Body.
    pub body: String,
    /// Summary, when the author wrote one.
    pub summary: Option<String>,
    /// When the revision was published, when it ever was.
    #[serde(with = "time::serde::rfc3339::option")]
    pub published_at: Option<OffsetDateTime>,
}

/// Response of `GET /api/v1/public/pages/{slug}`.
#[derive(Debug, Serialize)]
pub struct PublishedPageResponse {
    /// Site the page belongs to.
    pub site: PublicSiteBody,
    /// The page itself.
    pub page: PublicPageBody,
    /// The revision visitors see.
    pub revision: PublicRevisionBody,
}

// ---------------------------------------------------------------------------------------------
// Request shapes
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/public/pages/{slug}`.
#[derive(Debug, Deserialize)]
pub struct PublicPageQuery {
    /// Site hint: a host when it contains a dot, otherwise a site key.
    #[serde(default)]
    pub site: Option<String>,
}

/// What a `?site=` value addresses.
#[derive(Debug, PartialEq, Eq)]
enum SiteHint<'value> {
    /// A domain registered on a site (`site.example.org`).
    Host(&'value str),
    /// A site key (`main`).
    Key(&'value str),
}

/// Classify a `?site=` value: keys never contain a dot (`sites::validate_key`), so a dotted
/// value can only be a host.
fn classify_hint(value: &str) -> SiteHint<'_> {
    if value.contains('.') {
        SiteHint::Host(value)
    } else {
        SiteHint::Key(value)
    }
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/public/pages/{slug}` — the published page behind an address.
pub async fn get_published_page(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Query(query): Query<PublicPageQuery>,
    headers: HeaderMap,
) -> Result<Json<PublishedPageResponse>, ApiError> {
    let pool = state.db().pool();
    let site = resolve_site(pool, query.site.as_deref(), &headers).await?;

    let page = match pages::find_page_by_slug(pool, site.id, &slug).await {
        Ok(page) => page,
        // A public address that is not a slug shape is simply not found — a `400` would leak
        // the panel's validation rules to a visitor.
        Err(ContentError::InvalidSlug(_)) => None,
        Err(error) => return Err(error.into()),
    };

    // Unknown, unpublished and archived addresses answer alike, and a page that is published
    // but has no published revision does too: nothing tells a visitor what the panel holds.
    let Some(page) = page.filter(|page| page.status == "published") else {
        return Err(page_not_found(&slug));
    };
    let Some(revision) = pages::published_revision(pool, page.id).await? else {
        return Err(page_not_found(&slug));
    };

    Ok(Json(PublishedPageResponse {
        site: PublicSiteBody {
            key: site.key,
            name: site.name,
            theme: site.theme,
        },
        page: PublicPageBody {
            slug: page.slug,
            page_type: page.page_type,
            updated_at: page.updated_at,
        },
        revision: PublicRevisionBody {
            revision_no: revision.revision_no,
            title: revision.title,
            body: revision.body,
            summary: revision.summary,
            published_at: revision.published_at,
        },
    }))
}

/// The `404` of the public surface: one shape for "not here", never "not published".
fn page_not_found(slug: &str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "page_not_found",
        format!("no published page at {slug:?}"),
    )
}

/// Resolve the site a request addresses, following the documented order.
async fn resolve_site(
    pool: &PgPool,
    hint: Option<&str>,
    headers: &HeaderMap,
) -> Result<Site, ApiError> {
    if let Some(value) = hint.map(str::trim).filter(|value| !value.is_empty()) {
        let site = match classify_hint(value) {
            SiteHint::Host(host) => sites::find_site_by_host(pool, host).await?,
            SiteHint::Key(key) => sites::find_site_by_global_key(pool, key).await?,
        };
        return site.ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "site_not_found",
                format!("no site answers to {value:?} — use one of its domains or its key"),
            )
        });
    }

    if let Some(host) = request_host(headers) {
        if let Some(site) = sites::find_site_by_host(pool, &host).await? {
            return Ok(site);
        }
    }

    let all = sites::list_sites(pool).await?;
    match all.as_slice() {
        [only] => Ok(only.clone()),
        [] => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "site_not_found",
            "this installation has no site yet",
        )),
        _ => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "site_not_found",
            "this request does not address one site: register the site's domain or send \
             ?site=<host|key>",
        )),
    }
}

/// The host the visitor asked for: `X-Forwarded-Host` first (a renderer or an edge proxy sets
/// it), then `Host`. The port is dropped and the value lower-cased.
fn request_host(headers: &HeaderMap) -> Option<String> {
    let raw = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))?
        .to_str()
        .ok()?
        .split(',')
        .next()?
        .trim()
        .to_lowercase();
    if raw.is_empty() {
        return None;
    }

    let host = match raw.strip_prefix('[') {
        // An IPv6 literal keeps everything up to the closing bracket: `[::1]:8080` → `::1`.
        Some(rest) => rest.split(']').next().unwrap_or(rest),
        None => raw.split(':').next().unwrap_or(raw.as_str()),
    };

    if host.is_empty() {
        None
    } else {
        Some(host.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn a_dotted_hint_is_a_host_and_a_bare_one_a_key() {
        assert_eq!(classify_hint("main"), SiteHint::Key("main"));
        assert_eq!(classify_hint("omnion.test"), SiteHint::Host("omnion.test"));
    }

    #[test]
    fn the_host_header_is_lowercased_and_loses_its_port() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::HOST,
            HeaderValue::from_static("Site.Example.ORG:8080"),
        );
        assert_eq!(request_host(&headers).as_deref(), Some("site.example.org"));

        headers.insert(header::HOST, HeaderValue::from_static("[::1]:3200"));
        assert_eq!(request_host(&headers).as_deref(), Some("::1"));

        assert_eq!(request_host(&HeaderMap::new()), None);
    }

    #[test]
    fn the_forwarded_host_wins_and_only_its_first_entry_counts() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("internal:8080"));
        headers.insert(
            "x-forwarded-host",
            HeaderValue::from_static("public.example.org, edge.example.org"),
        );
        assert_eq!(
            request_host(&headers).as_deref(),
            Some("public.example.org")
        );
    }
}
