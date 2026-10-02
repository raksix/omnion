//! `/api/v1/seo/*` and the public `/api/v1/public/{sitemap.xml,robots.txt}` — REQ-064 slice 3.
//!
//! The SEO toolkit is three audiences sharing one feature, and the file is arranged by trust
//! level rather than by resource:
//!
//! * **The panel surface** reads and writes metadata, rules and settings. Every handler resolves
//!   the site through the caller's own organization *before* touching a row, so no id a caller
//!   supplies can widen what they see.
//!
//! * **The redirect resolver** is the one unauthenticated endpoint with side effects, because a
//!   rule that does not fire does not work. It counts a hit inside the same statement that
//!   returns the row, and it never tells a visitor anything except the location they were
//!   sent to.
//!
//! * **The sitemap and robots.txt** are served from the *stored* document rather than generated
//!   per request. A crawler hits these far harder than any page, and a generator on that path
//!   is a self-inflicted availability problem.
//!
//! The panel's `Test a path` button deliberately goes through [`SeoStore::test_redirect`]
//! rather than the resolver, so an owner probing their own rules does not fill the column they
//! are reading to decide whether a rule is still needed.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use omnion_audit::NewAuditEntry;
use omnion_content::seo::{
    BrokenLink, CHANGE_FREQUENCIES, NewRedirect, PageSeo, PageSeoSource, REDIRECT_PATTERNS,
    REDIRECT_STATUS_CODES, Redirect, STRUCTURED_DATA_TYPES, SeoSettings, SeoStore, SeoTags,
    TWITTER_CARDS,
};
use omnion_identity::Site;
use omnion_identity::sites;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::scope::ensure_same_organization;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// Everything the SEO screen needs to render, in one read.
///
/// One endpoint rather than six: the screen is a set of panels over ONE site, and a panel that
/// fetches its own slice is a screen with five loading states and five ways to show a number
/// from a different moment than its neighbour.
#[derive(Debug, Serialize)]
pub struct SeoOverviewBody {
    /// The site these settings are for.
    pub site: SiteRefBody,
    /// The closed vocabularies the editor offers, so the panel never hard-codes a list the
    /// store will later refuse.
    pub vocabulary: SeoVocabularyBody,
    /// The stored settings, with the sitemap preview the panel shows.
    pub settings: SettingsBody,
    /// The redirect rules.
    pub redirects: Vec<RedirectBody>,
    /// The broken internal links the last crawl found.
    pub broken_links: Vec<BrokenLinkBody>,
    /// Warnings the robots.txt editor raises — never refusals, always explanations.
    pub robots_warnings: Vec<String>,
}

/// The site's identity, as the SEO screen shows it in its header.
#[derive(Debug, Serialize)]
pub struct SiteRefBody {
    /// Site primary key.
    pub id: Uuid,
    /// Site handle.
    pub key: String,
    /// Site name.
    pub name: String,
    /// The host absolute URLs are built from.
    pub host: Option<String>,
}

impl From<(&Site, Option<String>)> for SiteRefBody {
    fn from((site, host): (&Site, Option<String>)) -> Self {
        Self {
            id: site.id,
            key: site.key.clone(),
            name: site.name.clone(),
            host,
        }
    }
}

/// The vocabularies the editor offers.
#[derive(Debug, Serialize)]
pub struct SeoVocabularyBody {
    /// schema.org types the generator can build.
    pub structured_data_types: Vec<String>,
    /// Twitter card variants.
    pub twitter_cards: Vec<String>,
    /// Redirect rule kinds.
    pub redirect_patterns: Vec<String>,
    /// Redirect codes.
    pub redirect_status_codes: Vec<i32>,
    /// Sitemap change frequencies.
    pub change_frequencies: Vec<String>,
    /// Page types this site actually has, for the sitemap's inclusion list.
    pub page_types: Vec<String>,
}

/// A site's stored settings, with the sitemap the editor previews.
#[derive(Debug, Serialize)]
pub struct SettingsBody {
    /// Page types the sitemap includes; empty means every published page.
    pub sitemap_types: Vec<String>,
    /// Default priority.
    pub default_priority: f64,
    /// Default change frequency.
    pub default_change_frequency: String,
    /// The generated XML, when the owner has run the generator at least once.
    pub sitemap_xml: Option<String>,
    /// When it was generated.
    #[serde(with = "time::serde::rfc3339::option")]
    pub sitemap_last_generated_at: Option<time::OffsetDateTime>,
    /// How many URLs the stored sitemap holds, so the panel can say "0 URLs" instead of showing
    /// an empty preview with no explanation.
    pub sitemap_url_count: usize,
    /// The site's robots.txt.
    pub robots_txt: String,
}

impl From<SeoSettings> for SettingsBody {
    fn from(settings: SeoSettings) -> Self {
        let sitemap_url_count = settings
            .sitemap_xml
            .as_deref()
            .map_or(0, |xml| xml.matches("<url>").count());
        Self {
            sitemap_types: settings.sitemap_types,
            default_priority: settings.default_priority,
            default_change_frequency: settings.default_change_frequency,
            sitemap_xml: settings.sitemap_xml,
            sitemap_last_generated_at: settings.sitemap_last_generated_at,
            sitemap_url_count,
            robots_txt: settings.robots_txt,
        }
    }
}

/// A redirect rule as the list shows it.
#[derive(Debug, Serialize)]
pub struct RedirectBody {
    /// Primary key.
    pub id: Uuid,
    /// Path the rule answers.
    pub from_path: String,
    /// Where the request goes.
    pub to_path: String,
    /// 301 or 302.
    pub status_code: i32,
    /// `literal` or `regex`.
    pub pattern: String,
    /// Whether the rule is evaluated.
    pub enabled: bool,
    /// How many requests it has answered.
    pub hits: i64,
    /// Last time it answered.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_hit_at: Option<time::OffsetDateTime>,
}

impl From<Redirect> for RedirectBody {
    fn from(rule: Redirect) -> Self {
        Self {
            id: rule.id,
            from_path: rule.from_path,
            to_path: rule.to_path,
            status_code: rule.status_code,
            pattern: rule.pattern,
            enabled: rule.enabled,
            hits: rule.hits,
            last_hit_at: rule.last_hit_at,
        }
    }
}

/// A broken link as the view shows it.
#[derive(Debug, Serialize)]
pub struct BrokenLinkBody {
    /// Primary key.
    pub id: Uuid,
    /// The page carrying the link.
    pub source_page_id: Option<Uuid>,
    /// The slug of that page, for the "source" column.
    pub source_slug: Option<String>,
    /// The target as written.
    pub target_url: String,
    /// The anchor text.
    pub anchor_text: Option<String>,
    /// The status the crawl saw, when it checked one.
    pub status: Option<i32>,
    /// Whether the owner dismissed it.
    pub ignored: bool,
}

impl BrokenLinkBody {
    /// Join a stored row with the slug the panel shows instead of a bare UUID.
    async fn build(pool: &PgPool, link: &BrokenLink) -> Self {
        let source_slug = match link.source_page_id {
            Some(page_id) => sqlx::query_scalar("select slug from pages where id = $1")
                .bind(page_id)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten(),
            None => None,
        };
        Self {
            id: link.id,
            source_page_id: link.source_page_id,
            source_slug,
            target_url: link.target_url.clone(),
            anchor_text: link.anchor_text.clone(),
            status: link.status,
            ignored: link.ignored,
        }
    }
}

/// A page's SEO fields plus the tags they produce.
#[derive(Debug, Serialize)]
pub struct PageSeoBody {
    /// The editable fields.
    pub seo: PageSeo,
    /// The tag set a crawler would read, built by the same function the renderer uses.
    pub tags: SeoTags,
}

/// Create or update a redirect.
#[derive(Debug, Deserialize)]
pub struct RedirectBodyIn {
    /// Site the rule belongs to.
    pub site_id: Uuid,
    /// Path the rule answers.
    pub from_path: String,
    /// Where the request goes.
    pub to_path: String,
    /// 301 or 302.
    #[serde(default)]
    pub status_code: Option<i32>,
    /// `literal` or `regex`.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Whether the rule is evaluated.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Update a redirect in place; the site comes from the route, never the body.
#[derive(Debug, Deserialize)]
pub struct RedirectUpdateIn {
    /// Path the rule answers.
    pub from_path: String,
    /// Where the request goes.
    pub to_path: String,
    /// 301 or 302.
    #[serde(default)]
    pub status_code: Option<i32>,
    /// `literal` or `regex`.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Whether the rule is evaluated.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// What `Test a path` answers.
#[derive(Debug, Serialize)]
pub struct RedirectTestBody {
    /// The path that was tested.
    pub path: String,
    /// The rule that would answer it, when one does.
    pub matched: Option<RedirectBody>,
    /// Rules that would also match, so an owner can see the ambiguity rather than the first
    /// hit alone. A resolver that takes the first match and says nothing about the second is a
    /// site with a rule the owner did not mean to have.
    pub also_matched: Vec<RedirectBody>,
}

/// Write a site's settings.
#[derive(Debug, Deserialize)]
pub struct SettingsBodyIn {
    /// Page types the sitemap includes; empty means every published page.
    #[serde(default)]
    pub sitemap_types: Vec<String>,
    /// Default priority, 0.0–1.0.
    #[serde(default)]
    pub default_priority: Option<f64>,
    /// Default change frequency.
    #[serde(default)]
    pub default_change_frequency: Option<String>,
    /// The site's robots.txt.
    #[serde(default)]
    pub robots_txt: Option<String>,
}

/// Dismiss a broken link, or bring it back.
#[derive(Debug, Deserialize)]
pub struct BrokenLinkBodyIn {
    /// Whether the owner is dismissing it.
    pub ignored: bool,
}

/// The site query every SEO read takes.
#[derive(Debug, Deserialize)]
pub struct SiteQuery {
    /// The site to read.
    pub site_id: Uuid,
}

/// The list query for broken links.
#[derive(Debug, Deserialize)]
pub struct BrokenLinkQuery {
    /// The site to read.
    pub site_id: Uuid,
    /// Whether dismissed rows come back too.
    #[serde(default)]
    pub include_ignored: bool,
}

// ---------------------------------------------------------------------------------------------
// Panel surface
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/seo/settings` — everything the SEO screen draws, in one response.
pub async fn get_seo_overview(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<SiteQuery>,
) -> Result<Json<SeoOverviewBody>, ApiError> {
    let site = site_in_scope(&state, &current, params.site_id).await?;
    let store = SeoStore::new(state.db().pool().clone());
    let settings = store.read_settings(site.id).await?;
    let redirects = store.list_redirects(site.id).await?;
    let links = store.list_broken_links(site.id, false).await?;

    let mut bodies = Vec::with_capacity(links.len());
    for link in &links {
        bodies.push(BrokenLinkBody::build(state.db().pool(), link).await);
    }

    Ok(Json(SeoOverviewBody {
        site: SiteRefBody::from((&site, primary_host(state.db().pool(), site.id).await?)),
        vocabulary: SeoVocabularyBody {
            structured_data_types: STRUCTURED_DATA_TYPES
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            twitter_cards: TWITTER_CARDS.iter().map(|s| (*s).to_string()).collect(),
            redirect_patterns: REDIRECT_PATTERNS.iter().map(|s| (*s).to_string()).collect(),
            redirect_status_codes: REDIRECT_STATUS_CODES.to_vec(),
            change_frequencies: CHANGE_FREQUENCIES
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            page_types: distinct_page_types(state.db().pool(), site.id).await?,
        },
        settings: SettingsBody::from(settings.clone()),
        redirects: redirects.into_iter().map(RedirectBody::from).collect(),
        broken_links: bodies,
        robots_warnings: omnion_content::seo::validate_robots_txt(&settings.robots_txt),
    }))
}

/// `GET /api/v1/pages/{id}/seo` — the page tab's editable fields and the tags they produce.
pub async fn get_page_seo(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(page_id): Path<Uuid>,
) -> Result<Json<PageSeoBody>, ApiError> {
    let (page, _site) = page_in_scope(&state, &current, page_id).await?;
    let store = SeoStore::new(state.db().pool().clone());
    let seo = store.read_page_seo(page.site_id, page_id).await?;
    let source = page_seo_source(state.db().pool(), &page).await?;
    let og_image = media_url(state.db().pool(), seo.og_image_media_id).await;
    let tags = SeoStore::build_tags(&source, &seo, og_image.as_deref());
    Ok(Json(PageSeoBody { seo, tags }))
}

/// `PUT /api/v1/pages/{id}/seo` — write the page's fields and return the tags they produce.
///
/// The response carries the generated tags so the panel's SERP preview and JSON-LD view update
/// from the same call that saved them. A panel that re-derives the preview client-side has two
/// implementations of "what a crawler sees", and they drift on the first edge case.
pub async fn put_page_seo(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(page_id): Path<Uuid>,
    Json(input): Json<PageSeo>,
) -> Result<Json<PageSeoBody>, ApiError> {
    let (page, site) = page_in_scope(&state, &current, page_id).await?;
    let store = SeoStore::new(state.db().pool().clone());
    store.write_page_seo(page.site_id, page_id, &input).await?;

    let saved = store.read_page_seo(page.site_id, page_id).await?;
    let source = page_seo_source(state.db().pool(), &page).await?;
    let og_image = media_url(state.db().pool(), saved.og_image_media_id).await;
    let tags = SeoStore::build_tags(&source, &saved, og_image.as_deref());

    record(
        &state,
        &current,
        site.organization_id,
        "seo.page.update",
        page_id,
        json!({
            "structured_data_type": saved.structured_data_type,
            "robots": saved.robots,
            "indexable": tags.robots.starts_with("index"),
        }),
    )
    .await?;

    Ok(Json(PageSeoBody { seo: saved, tags }))
}

/// `POST /api/v1/seo/redirects` — create a rule.
pub async fn create_redirect(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(input): Json<RedirectBodyIn>,
) -> Result<(StatusCode, Json<RedirectBody>), ApiError> {
    let site = site_in_scope(&state, &current, input.site_id).await?;
    let store = SeoStore::new(state.db().pool().clone());
    let rule = store
        .create_redirect(&NewRedirect {
            site_id: site.id,
            from_path: input.from_path,
            to_path: input.to_path,
            status_code: input.status_code,
            pattern: input.pattern,
            enabled: input.enabled,
            created_by: Some(current.user.id),
        })
        .await?;
    record(
        &state,
        &current,
        site.organization_id,
        "seo.redirect.create",
        rule.id,
        json!({ "from": rule.from_path, "to": rule.to_path, "pattern": rule.pattern }),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(RedirectBody::from(rule))))
}

/// `PUT /api/v1/seo/redirects/{id}` — replace a rule's fields.
pub async fn update_redirect(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Json(input): Json<RedirectUpdateIn>,
) -> Result<Json<RedirectBody>, ApiError> {
    // The site is resolved from the rule itself, never from the body: a body carrying a
    // `site_id` would let a caller name another organization's site and get a 404 that
    // distinguishes "not yours" from "not there".
    let site = redirect_site_in_scope(&state, &current, id).await?;
    let store = SeoStore::new(state.db().pool().clone());
    let rule = store
        .update_redirect(
            site.id,
            id,
            &NewRedirect {
                site_id: site.id,
                from_path: input.from_path,
                to_path: input.to_path,
                status_code: input.status_code,
                pattern: input.pattern,
                enabled: input.enabled,
                created_by: Some(current.user.id),
            },
        )
        .await?;
    record(
        &state,
        &current,
        site.organization_id,
        "seo.redirect.update",
        id,
        json!({ "from": rule.from_path, "to": rule.to_path, "enabled": rule.enabled }),
    )
    .await?;
    Ok(Json(RedirectBody::from(rule)))
}

/// `DELETE /api/v1/seo/redirects/{id}` — remove a rule.
pub async fn delete_redirect(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let site = redirect_site_in_scope(&state, &current, id).await?;
    let store = SeoStore::new(state.db().pool().clone());
    store.delete_redirect(site.id, id).await?;
    record(
        &state,
        &current,
        site.organization_id,
        "seo.redirect.delete",
        id,
        json!({ "site_id": site.id }),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/seo/redirects/{id}/test` — what would answer this path, and what else would.
///
/// Deliberately not the resolver: a test does not count a hit, because an owner trying three
/// candidate rules should not leave three hits in the column they are reading to decide whether
/// any of the rules is needed.
pub async fn test_redirect(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Json(input): Json<TestPathIn>,
) -> Result<Json<RedirectTestBody>, ApiError> {
    let site = redirect_site_in_scope(&state, &current, id).await?;
    let store = SeoStore::new(state.db().pool().clone());
    let matched = store.test_redirect(site.id, &input.path).await?;
    let all = store.list_redirects(site.id).await?;
    let also_matched = all
        .into_iter()
        .filter(|rule| rule.enabled && Some(rule.id) != matched.as_ref().map(|m| m.id))
        .filter(|rule| {
            let path = input.path.split('?').next().unwrap_or(&input.path);
            if rule.pattern == "regex" {
                omnion_content::seo::matches_pattern(&rule.from_path, path)
            } else {
                rule.from_path == path
            }
        })
        .map(RedirectBody::from)
        .collect();

    Ok(Json(RedirectTestBody {
        path: input.path,
        matched: matched.map(RedirectBody::from),
        also_matched,
    }))
}

/// The body of a `Test a path` call.
#[derive(Debug, Deserialize)]
pub struct TestPathIn {
    /// The path to try.
    pub path: String,
}

/// A file of redirect rules, as text.
///
/// The CSV travels as a JSON string rather than as `multipart/form-data` on purpose: the panel
/// pastes or drops a file, and a `.txt` body with a JSON envelope is the one shape that survives
/// a proxy, a `curl` and a browser alike. The alternative — a multipart upload — is three parsers
/// and a temporary-file rule for a document that is, by construction, a few kilobytes of text.
#[derive(Debug, Deserialize)]
pub struct RedirectImportIn {
    /// Site the rules belong to.
    pub site_id: Uuid,
    /// The CSV itself.
    pub csv: String,
    /// Read the file and report what it holds **without writing anything**.
    ///
    /// This is the default a panel uses: a 400-row file is something an owner wants to read
    /// before they commit it, and the report is exactly what the parse already produces. The
    /// write is a second, explicit press.
    #[serde(default)]
    pub dry_run: Option<bool>,
}

/// A row the importer refused, in the report.
#[derive(Debug, Serialize)]
pub struct RedirectRejectionBody {
    /// 1-based line in the uploaded file, or 0 for a refusal about the file as a whole.
    pub line: usize,
    /// The row as it was read.
    pub row: String,
    /// Why it was refused.
    pub reason: String,
}

/// The answer to an import: what would happen, or what happened.
#[derive(Debug, Serialize)]
pub struct RedirectImportBody {
    /// `true` when the file had nothing refused.
    pub clean: bool,
    /// Rows written — 0 for a dry run, and 0 for a refused file.
    pub imported: usize,
    /// Rows that will be written, or would be on a dry run.
    pub accepted: usize,
    /// One sentence for the panel to show.
    pub summary: String,
    /// The refusals, in file order.
    pub rejected: Vec<RedirectRejectionBody>,
}

/// `POST /api/v1/seo/redirects/import` — read a CSV of rules, and (unless it is a dry run)
/// write the whole file or nothing.
///
/// The all-or-nothing contract is the reason this is one endpoint and not a per-row loop: an
/// owner who pastes 400 rows and reads "imported 397, 3 failed" reasonably concludes the other
/// 397 were saved. A refused file writes nothing and says which line stopped it, so the fix is
/// to edit the file rather than to audit a table nobody meant to change.
pub async fn import_redirects(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(input): Json<RedirectImportIn>,
) -> Result<(StatusCode, Json<RedirectImportBody>), ApiError> {
    let site = site_in_scope(&state, &current, input.site_id).await?;
    let store = SeoStore::new(state.db().pool().clone());

    let plan = omnion_content::seo_csv::parse_redirect_csv(&input.csv)?;

    // A file that closes a circle with a rule ALREADY stored is the same loop the per-row check
    // would have caught had the rows arrived in the other order, and it is invisible to the
    // parser because it has no database to ask. So the plan is checked against both.
    if let Some(reason) = omnion_content::seo_csv::refuse_circular_plan_with(
        &store.redirect_pairs(site.id).await?,
        &plan.accepted,
    ) {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(RedirectImportBody {
                clean: false,
                imported: 0,
                accepted: plan.accepted.len(),
                summary: reason,
                rejected: plan.rejected.iter().map(rejection_body).collect(),
            }),
        ));
    }

    if !plan.is_clean() {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(RedirectImportBody {
                clean: false,
                imported: 0,
                accepted: plan.accepted.len(),
                summary: format!("nothing was imported — {}", plan.summary()),
                rejected: plan.rejected.iter().map(rejection_body).collect(),
            }),
        ));
    }

    if input.dry_run.unwrap_or(false) {
        return Ok((
            StatusCode::OK,
            Json(RedirectImportBody {
                clean: true,
                imported: 0,
                accepted: plan.accepted.len(),
                summary: format!("{} — nothing was written (dry run)", plan.summary()),
                rejected: Vec::new(),
            }),
        ));
    }

    let written = store
        .import_redirects(site.id, &plan.accepted, Some(current.user.id))
        .await?;
    record(
        &state,
        &current,
        site.organization_id,
        "seo.redirect.import",
        site.id,
        json!({ "rows": written.len(), "site_id": site.id }),
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(RedirectImportBody {
            clean: true,
            imported: written.len(),
            accepted: plan.accepted.len(),
            summary: format!("imported {} redirect rule(s)", written.len()),
            rejected: Vec::new(),
        }),
    ))
}

/// `GET /api/v1/seo/redirects/export?site_id=` — the site's rules as a CSV file.
///
/// Served as a download rather than as JSON because the caller wants a file, and the header is
/// the whole difference between "save this" and "here is a string".
pub async fn export_redirects(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<RedirectExportQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let store = SeoStore::new(state.db().pool().clone());
    let csv = store.export_redirects(site.id).await?;
    record(
        &state,
        &current,
        site.organization_id,
        "seo.redirect.export",
        site.id,
        json!({ "site_id": site.id }),
    )
    .await?;

    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"redirects.csv\"",
            ),
        ],
        csv,
    ))
}

/// Which site's rules to export.
#[derive(Debug, Deserialize)]
pub struct RedirectExportQuery {
    /// Site the rules belong to.
    pub site_id: Uuid,
}

fn rejection_body(rejection: &omnion_content::seo_csv::CsvRejection) -> RedirectRejectionBody {
    RedirectRejectionBody {
        line: rejection.line,
        row: rejection.row.clone(),
        reason: rejection.reason.clone(),
    }
}

/// `PUT /api/v1/sites/{site_id}/seo/settings` — write the site's settings.
pub async fn put_settings(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(site_id): Path<Uuid>,
    Json(input): Json<SettingsBodyIn>,
) -> Result<Json<SettingsBody>, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;
    let store = SeoStore::new(state.db().pool().clone());
    let existing = store.read_settings(site.id).await?;
    let saved = store
        .write_settings(
            site.id,
            Some(current.user.id),
            &input.sitemap_types,
            input.default_priority.unwrap_or(existing.default_priority),
            input
                .default_change_frequency
                .as_deref()
                .unwrap_or(&existing.default_change_frequency),
            input.robots_txt.as_deref().unwrap_or(&existing.robots_txt),
        )
        .await?;
    record(
        &state,
        &current,
        site.organization_id,
        "seo.settings.update",
        site.id,
        json!({
            "sitemap_types": saved.sitemap_types,
            "robots_bytes": saved.robots_txt.len(),
        }),
    )
    .await?;
    Ok(Json(SettingsBody::from(saved)))
}

/// `POST /api/v1/sites/{site_id}/seo/sitemap/regenerate` — rebuild and store the sitemap.
pub async fn regenerate_sitemap(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(site_id): Path<Uuid>,
) -> Result<Json<SettingsBody>, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;
    let store = SeoStore::new(state.db().pool().clone());
    let saved = store
        .regenerate_sitemap(site.id, Some(current.user.id))
        .await?;
    record(
        &state,
        &current,
        site.organization_id,
        "seo.sitemap.regenerate",
        site.id,
        json!({ "urls": saved.sitemap_xml.as_deref().map_or(0, |xml| xml.matches("<url>").count()) }),
    )
    .await?;
    Ok(Json(SettingsBody::from(saved)))
}

/// `GET /api/v1/seo/broken-links` — the crawl-lite result.
pub async fn list_broken_links(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<BrokenLinkQuery>,
) -> Result<Json<Vec<BrokenLinkBody>>, ApiError> {
    let site = site_in_scope(&state, &current, params.site_id).await?;
    let store = SeoStore::new(state.db().pool().clone());
    let links = store
        .list_broken_links(site.id, params.include_ignored)
        .await?;
    let mut bodies = Vec::with_capacity(links.len());
    for link in &links {
        bodies.push(BrokenLinkBody::build(state.db().pool(), link).await);
    }
    Ok(Json(bodies))
}

/// `POST /api/v1/seo/broken-links/scan` — run the crawl now and store what it found.
pub async fn scan_broken_links(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(input): Json<ScanIn>,
) -> Result<Json<Vec<BrokenLinkBody>>, ApiError> {
    let site = site_in_scope(&state, &current, input.site_id).await?;
    let store = SeoStore::new(state.db().pool().clone());
    let links = store.crawl_internal_links(site.id).await?;
    let mut bodies = Vec::with_capacity(links.len());
    for link in &links {
        bodies.push(BrokenLinkBody::build(state.db().pool(), link).await);
    }
    record(
        &state,
        &current,
        site.organization_id,
        "seo.broken_links.scan",
        site.id,
        json!({ "found": bodies.len() }),
    )
    .await?;
    Ok(Json(bodies))
}

/// The body of a crawl request.
#[derive(Debug, Deserialize)]
pub struct ScanIn {
    /// The site to crawl.
    pub site_id: Uuid,
}

/// `PATCH /api/v1/seo/broken-links/{id}` — dismiss a link, or bring it back.
pub async fn set_broken_link_ignored(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Json(input): Json<BrokenLinkBodyIn>,
) -> Result<StatusCode, ApiError> {
    let site = broken_link_site_in_scope(&state, &current, id).await?;
    let store = SeoStore::new(state.db().pool().clone());
    store.set_link_ignored(site.id, id, input.ignored).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/public/{host}/sitemap.xml` — the stored sitemap, or a 404 telling the crawler
/// the owner has never generated one.
///
/// A 404 rather than an empty document: an empty `<urlset>` reads to a crawler as "this site
/// has no pages", which is a ranking decision made on the owner's behalf by a file they never
/// wrote.
pub async fn public_sitemap(
    State(state): State<AppState>,
    Path(host): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let site = public_site(state.db().pool(), &host).await?;
    let settings = SeoStore::new(state.db().pool().clone())
        .read_settings(site.id)
        .await?;
    let Some(xml) = settings.sitemap_xml else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "sitemap_not_generated",
            "this site has no generated sitemap yet",
        ));
    };
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        xml,
    ))
}

/// `GET /api/v1/public/{host}/robots.txt` — the stored robots.txt.
pub async fn public_robots(
    State(state): State<AppState>,
    Path(host): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let site = public_site(state.db().pool(), &host).await?;
    let settings = SeoStore::new(state.db().pool().clone())
        .read_settings(site.id)
        .await?;
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        settings.robots_txt,
    ))
}

/// `GET /api/v1/public/{host}/redirect?path=…` — resolve one path and answer the hop.
///
/// The only public handler with a side effect, and the only unauthenticated route a visitor's
/// request can be *sent* along. It returns the status and the location and nothing else: no
/// rule id, no hit counter, no pattern kind. A redirect endpoint that explains itself is a
/// tool for mapping a site's rules.
pub async fn public_redirect(
    State(state): State<AppState>,
    Path(host): Path<String>,
    Query(params): Query<RedirectQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let site = public_site(state.db().pool(), &host).await?;
    let store = SeoStore::new(state.db().pool().clone());
    let Some(rule) = store.resolve_redirect(site.id, &params.path).await? else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "no_redirect",
            "no redirect rule answers this path",
        ));
    };
    let code =
        StatusCode::from_u16(rule.status_code as u16).unwrap_or(StatusCode::MOVED_PERMANENTLY);
    Ok((code, [(header::LOCATION, rule.to_path)]))
}

/// The query of the public resolver.
#[derive(Debug, Deserialize)]
pub struct RedirectQuery {
    /// The path to resolve.
    pub path: String,
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The house translation of a raw `sqlx::Error` into the API surface.
///
/// Mirrors `menus::menu_in_scope`: a store read that fails is an internal error naming what it
/// was reading. The content crate's own `ContentError` carries the dependency/unavailable split,
/// but these three handlers touch rows the store does not own (the scope lookups), so they need
/// the mapping spelled out rather than inherited.
fn store_error(action: &str, error: sqlx::Error) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        format!("{action}: {error}"),
    )
}

/// Resolve a site through the caller's organization.
async fn site_in_scope(
    state: &AppState,
    current: &CurrentSession,
    site_id: Uuid,
) -> Result<Site, ApiError> {
    let site = sites::find_site(state.db().pool(), site_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "site_not_found", "no such site"))?;
    ensure_same_organization(current, Some(site.organization_id))?;
    Ok(site)
}

/// Resolve a page, then the site it belongs to.
async fn page_in_scope(
    state: &AppState,
    current: &CurrentSession,
    page_id: Uuid,
) -> Result<(omnion_content::Page, Site), ApiError> {
    let page = omnion_content::pages::find_page(state.db().pool(), page_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "page_not_found", "no such page"))?;
    // The site comes back with the page because the audit row needs the *site's* organization:
    // `pages` has no organization column of its own, and reaching through `current.user` instead
    // would be a 400 for the platform owner — the account a fresh install creates.
    let site = site_in_scope(state, current, page.site_id).await?;
    Ok((page, site))
}

/// Resolve the site a redirect rule belongs to, without trusting a body-supplied site.
async fn redirect_site_in_scope(
    state: &AppState,
    current: &CurrentSession,
    id: Uuid,
) -> Result<Site, ApiError> {
    let row: Option<Uuid> =
        sqlx::query_scalar("select site_id from cms_seo_redirects where id = $1")
            .bind(id)
            .fetch_optional(state.db().pool())
            .await
            .map_err(|error| store_error("reading the redirect rule", error))?;
    let Some(site_id) = row else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "redirect_not_found",
            "no such redirect rule",
        ));
    };
    site_in_scope(state, current, site_id).await
}

/// Resolve the site a broken-link row belongs to.
async fn broken_link_site_in_scope(
    state: &AppState,
    current: &CurrentSession,
    id: Uuid,
) -> Result<Site, ApiError> {
    let row: Option<Uuid> =
        sqlx::query_scalar("select site_id from cms_broken_links where id = $1")
            .bind(id)
            .fetch_optional(state.db().pool())
            .await
            .map_err(|error| store_error("reading the broken link", error))?;
    let Some(site_id) = row else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "broken_link_not_found",
            "no such broken link",
        ));
    };
    site_in_scope(state, current, site_id).await
}

/// Resolve a public site from a host or a site key.
async fn public_site(pool: &PgPool, host: &str) -> Result<Site, ApiError> {
    let found = if host.contains('.') {
        sites::find_site_by_host(pool, host).await?
    } else {
        // The *global* key lookup, not the organization-scoped one: a public route has no
        // session and therefore no organization to scope by, which is the same resolution order
        // `public.rs` documents for every other public read.
        sites::find_site_by_global_key(pool, host).await?
    };
    found.ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "site_not_found", "no such site"))
}

/// The host absolute URLs are built from: the primary domain, else the first one.
async fn primary_host(pool: &PgPool, site_id: Uuid) -> Result<Option<String>, ApiError> {
    let host: Option<String> = sqlx::query_scalar(
        "select host from site_domains where site_id = $1 and is_primary \
         union all select host from site_domains where site_id = $1 order by 1 limit 1",
    )
    .bind(site_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| store_error("reading the site's primary host", error))?;
    Ok(host)
}

/// The page types this site actually holds, for the sitemap's inclusion list.
///
/// Derived from the rows rather than kept in a column: a checkbox list built from a stored
/// vocabulary shows owners types their site has never had, and a type with no pages in the
/// sitemap is a checkbox that does nothing.
async fn distinct_page_types(pool: &PgPool, site_id: Uuid) -> Result<Vec<String>, ApiError> {
    let types: Vec<String> = sqlx::query_scalar(
        "select distinct page_type from pages where site_id = $1 order by page_type",
    )
    .bind(site_id)
    .fetch_all(pool)
    .await
    .map_err(|error| store_error("reading the site's page types", error))?;
    Ok(types)
}

/// The public URL of a media row, when it still exists.
///
/// A deleted OG image degrades the page to "no card" rather than to a 500: `og_image_media_id`
/// is `on delete set null`, so the column empties itself, and this returns `None` rather than
/// inventing a URL that 404s in every share card on the internet.
async fn media_url(pool: &PgPool, media_id: Option<Uuid>) -> Option<String> {
    let id = media_id?;
    let key: Option<String> = sqlx::query_scalar("select storage_key from media where id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
    key.map(|storage_key| format!("/media/{storage_key}"))
}

/// Everything the tag generator needs about one page.
async fn page_seo_source(
    pool: &PgPool,
    page: &omnion_content::Page,
) -> Result<PageSeoSource, ApiError> {
    let row: Option<(i32, String, Option<String>, time::OffsetDateTime)> = sqlx::query_as(
        "select revision_no, title, summary, coalesce(published_at, created_at) from page_revisions \
         where id = $1",
    )
    .bind(page.published_revision_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| store_error("reading the published revision", error))?;
    let Some((_, title, summary, updated_at)) = row else {
        // A page with no published revision has no tags to generate, and the panel's tab says so
        // rather than rendering an empty form for a page nobody can see yet.
        return Ok(PageSeoSource {
            page_id: page.id,
            site_id: page.site_id,
            slug: page.slug.clone(),
            page_type: page.page_type.clone(),
            title: page.slug.clone(),
            summary: None,
            updated_at: time::OffsetDateTime::now_utc(),
            site_host: String::new(),
        });
    };
    Ok(PageSeoSource {
        page_id: page.id,
        site_id: page.site_id,
        slug: page.slug.clone(),
        page_type: page.page_type.clone(),
        title,
        summary,
        updated_at,
        site_host: primary_host(pool, page.site_id).await?.unwrap_or_default(),
    })
}

/// Write one audit row, ignoring an audit failure.
///
/// The pattern the rest of the API uses: a panel action that succeeded must not be reported as
/// failed because the audit insert could not reach the database.
async fn record(
    state: &AppState,
    current: &CurrentSession,
    organization_id: Uuid,
    action: &'static str,
    target: Uuid,
    metadata: Value,
) -> Result<(), ApiError> {
    let _ = omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, action)
            .organization(organization_id)
            .target("seo", target)
            .metadata(metadata),
    )
    .await;
    Ok(())
}
