//! `/api/v1/analytics` — the panel surface — and the public collection endpoint
//! `POST /api/v1/public/analytics/collect` (docs/requests/REQ-007, slice 1).
//!
//! Two audiences, two rules:
//!
//! * **The collection endpoint is public** (the tracking script of a rendered site posts to it)
//!   and therefore carries no permission guard. What protects it is what a public write path
//!   can be protected by: a body cap, a per-site-and-caller rate limit, a site resolved from
//!   the request itself, and a collector that decides *before* writing — bots, privacy signals,
//!   exclusions and sampling are dropped and counted, not stored and hidden.
//! * **The settings and snippet endpoints are panel surface**: `analytics.read` to see how a
//!   site is configured and what to paste, `analytics.settings.manage` to change it. Both
//!   resolve the site through the caller's own organization (`crate::scope`), so an account
//!   can never read or change another tenant's promises.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use omnion_identity::Site;
use omnion_identity::sites;
use omnion_module_analytics::collect::{self, Beacon, RequestMeta};
use omnion_module_analytics::reports::{self, DateRange, Filters, Granularity, Report};
use omnion_module_analytics::settings as store;
use omnion_module_analytics::{Settings, SettingsChanges};

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::scope::ensure_same_organization;
use crate::state::AppState;

/// Window the beacon rate limit counts over.
const RATE_WINDOW: Duration = Duration::from_secs(60);

/// Upper bound on the limiter's own memory: past this many live buckets, stale ones are dropped
/// instead of letting a busy hour of spoofed callers grow the map without end.
const RATE_BUCKETS: usize = 10_000;

// ---------------------------------------------------------------------------------------------
// Collection (public)
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/public/analytics/collect`.
#[derive(Debug, Deserialize)]
pub struct CollectQuery {
    /// Site hint: a host when it contains a dot, otherwise a site key — the same rule the rest
    /// of the public surface follows.
    #[serde(default)]
    pub site: Option<String>,
}

/// Record one batched beacon.
///
/// Answers `202 Accepted` with what was stored and what was dropped: the tracking script never
/// reads it, but an operator debugging a quiet dashboard can, and `curl` proves the endpoint.
pub async fn collect(
    State(state): State<AppState>,
    Query(query): Query<CollectQuery>,
    headers: HeaderMap,
    address: ClientAddress,
    body: Bytes,
) -> Result<(StatusCode, Json<collect::IngestReport>), ApiError> {
    if body.len() > collect::MAX_BODY_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!("a beacon may not exceed {} bytes", collect::MAX_BODY_BYTES),
        ));
    }

    let pool = state.db().pool();
    let site = crate::routes::public::resolve_site(pool, query.site.as_deref(), &headers).await?;

    let address = visitor_address(&headers, address.0);
    let budget = state.config().analytics.collect_per_minute;
    if !limiter().allows(&rate_key(site.id, address), budget, Instant::now()) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            format!("at most {budget} beacons per minute per site and caller"),
        ));
    }

    let settings = store::ensure(pool, site.id).await?;
    let beacon = Beacon::parse(&body)?;

    let now = OffsetDateTime::now_utc();
    let meta = RequestMeta {
        address,
        user_agent: user_agent(&headers),
        country: edge_country(&headers),
        day: now.date(),
        now,
        // `DNT: 1` and `Sec-GPC: 1` are the two signals a browser sends without any script.
        dnt: header_is(&headers, "dnt", "1"),
        gpc: header_is(&headers, "sec-gpc", "1"),
    };

    let report = collect::ingest(pool, site.id, &settings, &meta, &beacon).await?;

    Ok((StatusCode::ACCEPTED, Json(report)))
}

// ---------------------------------------------------------------------------------------------
// Settings and snippet (panel)
// ---------------------------------------------------------------------------------------------

/// `GET`/`PUT /api/v1/analytics/settings`.
#[derive(Debug, Deserialize)]
pub struct SiteQuery {
    /// Site the settings belong to.
    pub site_id: Uuid,
}

/// The settings of one site, plus the defaults the screen's "restore defaults" fills in.
#[derive(Debug, Serialize)]
pub struct SettingsResponse {
    /// The stored configuration.
    pub settings: Settings,
    /// The platform's defaults, sent by the server so the screen never hard-codes them.
    pub defaults: SettingsChanges,
}

/// `GET /api/v1/analytics/settings` — how this site counts (docs/requests/REQ-007).
pub async fn get_settings(
    State(state): State<AppState>,
    Query(query): Query<SiteQuery>,
    current: CurrentSession,
) -> Result<Json<SettingsResponse>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let settings = store::ensure(state.db().pool(), site.id).await?;

    Ok(Json(SettingsResponse {
        settings,
        defaults: SettingsChanges::defaults(),
    }))
}

/// `PUT /api/v1/analytics/settings` — replace the configuration in one write.
///
/// A full body on purpose: a partial write is how a site ends up with last month's retention and
/// this month's tracking switch. Validation answers the field that failed, which is what the
/// screen renders beside the input.
pub async fn put_settings(
    State(state): State<AppState>,
    Query(query): Query<SiteQuery>,
    current: CurrentSession,
    Json(changes): Json<SettingsChanges>,
) -> Result<Json<SettingsResponse>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let settings =
        store::update(state.db().pool(), site.id, &changes, Some(current.user.id)).await?;

    Ok(Json(SettingsResponse {
        settings,
        defaults: SettingsChanges::defaults(),
    }))
}

/// `GET /api/v1/analytics/snippet` — what a site pastes into its pages.
#[derive(Debug, Serialize)]
pub struct SnippetResponse {
    /// Site the snippet tracks.
    pub site: SnippetSite,
    /// Where the tracking script is served from.
    pub script_url: String,
    /// Where the script posts its beacons.
    pub collect_url: String,
    /// The single line to paste.
    pub snippet: String,
}

/// Public identity of the site a snippet belongs to.
#[derive(Debug, Serialize)]
pub struct SnippetSite {
    /// Stable handle inside the organization.
    pub key: String,
    /// Display name.
    pub name: String,
}

/// `GET /api/v1/analytics/snippet` — the script tag with the resolved site key.
pub async fn snippet(
    State(state): State<AppState>,
    Query(query): Query<SiteQuery>,
    current: CurrentSession,
    headers: HeaderMap,
) -> Result<Json<SnippetResponse>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let host = snippet_host(&state, &site, &headers).await;

    let script_url = format!("//{host}/analytics.js");
    let collect_url = format!("//{host}/api/v1/public/analytics/collect");
    let snippet = store::tracking_snippet(&script_url, &site.key);

    Ok(Json(SnippetResponse {
        site: SnippetSite {
            key: site.key,
            name: site.name,
        },
        script_url,
        collect_url,
        snippet,
    }))
}

/// The host a snippet should address: the site's primary domain when it has one, the host the
/// operator reached the panel on otherwise, and a development placeholder as the last resort.
async fn snippet_host(state: &AppState, site: &Site, headers: &HeaderMap) -> String {
    let domains = sites::list_domains(state.db().pool(), site.id)
        .await
        .unwrap_or_default();
    if let Some(primary) = domains
        .iter()
        .find(|domain| domain.is_primary)
        .or_else(|| domains.first())
    {
        return primary.host.clone();
    }

    request_host(headers).unwrap_or_else(|| format!("{}.omnion.test", site.key))
}

// ---------------------------------------------------------------------------------------------
// Reports (panel)
// ---------------------------------------------------------------------------------------------

/// Query string every report endpoint accepts.
///
/// One shape for all of them on purpose: the screens share one toolbar, so they share one URL
/// vocabulary — the range, the comparison switch, the granularity, the five filters, the sort and
/// the page. A report simply ignores what it does not read.
#[derive(Debug, Deserialize)]
pub struct ReportQuery {
    /// Site the report is about.
    pub site_id: Uuid,
    /// First UTC day (`YYYY-MM-DD`); seven days back when absent.
    pub from: Option<String>,
    /// Last UTC day; today when absent.
    pub to: Option<String>,
    /// `1`/`true` asks for the previous period beside every number.
    pub compare: Option<String>,
    /// `hour`, `day` or `auto` (the range decides).
    pub granularity: Option<String>,
    /// Path substring filter.
    pub path: Option<String>,
    /// Title substring filter.
    pub title: Option<String>,
    /// Device filter.
    pub device: Option<String>,
    /// Country filter.
    pub country: Option<String>,
    /// Source filter.
    pub source: Option<String>,
    /// Sort key of the page report.
    pub sort: Option<String>,
    /// `asc` or `desc`.
    pub dir: Option<String>,
    /// One-based page of the page report.
    pub page: Option<i64>,
    /// Rows per page of the page report.
    pub per_page: Option<i64>,
    /// Group-by mode of the sources report.
    pub group: Option<String>,
    /// Event name of the event detail.
    pub event: Option<String>,
    /// Which report to export.
    pub report: Option<String>,
    /// `csv` (default) or `json`.
    pub format: Option<String>,
}

/// The resolved inputs of one report request.
struct ReportRequest {
    /// Site the caller may read.
    site: Site,
    /// The validated range.
    range: DateRange,
    /// The validated filters.
    filters: Filters,
}

impl ReportRequest {
    /// Resolve a request: the site in the caller's own organization, the range, the filters.
    async fn resolve(
        state: &AppState,
        current: &CurrentSession,
        query: &ReportQuery,
    ) -> Result<Self, ApiError> {
        let site = site_in_scope(state, current, query.site_id).await?;
        let today = OffsetDateTime::now_utc().date();
        let from = parse_day(query.from.as_deref(), today - time::Duration::days(6))?;
        let to = parse_day(query.to.as_deref(), today)?;
        let range = DateRange::new(from, to)?;
        let filters = Filters::new(
            query.path.clone(),
            query.title.clone(),
            query.device.clone(),
            query.country.clone(),
            query.source.clone(),
        )?;

        Ok(Self {
            site,
            range,
            filters,
        })
    }

    /// `true` when the caller asked for the previous period beside this one.
    fn compare(&self, query: &ReportQuery) -> bool {
        query
            .compare
            .as_deref()
            .map(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false)
    }
}

/// `GET /api/v1/analytics/overview` — the headline numbers, the series and the side panels.
pub async fn overview(
    State(state): State<AppState>,
    Query(query): Query<ReportQuery>,
    current: CurrentSession,
) -> Result<Json<reports::Overview>, ApiError> {
    let request = ReportRequest::resolve(&state, &current, &query).await?;
    let settings = store::ensure(state.db().pool(), request.site.id).await?;
    let granularity = Granularity::parse(query.granularity.as_deref(), request.range)?;
    let today = OffsetDateTime::now_utc().date();

    let report = reports::overview(
        state.db().pool(),
        request.site.id,
        request.range,
        request.compare(&query),
        granularity,
        settings.retention_days,
        today,
    )
    .await?;

    Ok(Json(report))
}

/// `GET /api/v1/analytics/pages` — the page report, filtered, sorted and paged.
pub async fn pages(
    State(state): State<AppState>,
    Query(query): Query<ReportQuery>,
    current: CurrentSession,
) -> Result<Json<reports::PagesReport>, ApiError> {
    let request = ReportRequest::resolve(&state, &current, &query).await?;
    let report = reports::pages(
        state.db().pool(),
        request.site.id,
        request.range,
        &request.filters,
        query.sort.as_deref(),
        query.dir.as_deref(),
        query.page,
        query.per_page,
    )
    .await?;

    Ok(Json(report))
}

/// `GET /api/v1/analytics/pages/series` — one page's own series for the drawer.
///
/// The path rides as a query parameter rather than as a path segment: a URL path is not a path,
/// and `/pages/%2Fpricing/series` would make every router on the way a participant in the
/// encoding question.
pub async fn page_series(
    State(state): State<AppState>,
    Query(query): Query<ReportQuery>,
    current: CurrentSession,
) -> Result<Json<Vec<reports::SeriesPoint>>, ApiError> {
    let request = ReportRequest::resolve(&state, &current, &query).await?;
    let path = query
        .path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_report_query",
                "the series of a page needs its path",
            )
        })?;
    let granularity = Granularity::parse(query.granularity.as_deref(), request.range)?;

    let series = reports::page_series(
        state.db().pool(),
        request.site.id,
        path,
        request.range,
        granularity,
    )
    .await?;

    Ok(Json(series))
}

/// `GET /api/v1/analytics/sources` — referrers and UTM, grouped as the caller asks.
pub async fn sources(
    State(state): State<AppState>,
    Query(query): Query<ReportQuery>,
    current: CurrentSession,
) -> Result<Json<reports::SourcesReport>, ApiError> {
    let request = ReportRequest::resolve(&state, &current, &query).await?;
    let report = reports::sources(
        state.db().pool(),
        request.site.id,
        request.range,
        &request.filters,
        query.group.as_deref(),
    )
    .await?;

    Ok(Json(report))
}

/// `GET /api/v1/analytics/audience` — devices, browsers, systems, screens, languages, countries.
pub async fn audience(
    State(state): State<AppState>,
    Query(query): Query<ReportQuery>,
    current: CurrentSession,
) -> Result<Json<reports::AudienceReport>, ApiError> {
    let request = ReportRequest::resolve(&state, &current, &query).await?;
    let report = reports::audience(
        state.db().pool(),
        request.site.id,
        request.range,
        &request.filters,
    )
    .await?;

    Ok(Json(report))
}

/// `GET /api/v1/analytics/events` — custom events with their counts and values.
pub async fn events(
    State(state): State<AppState>,
    Query(query): Query<ReportQuery>,
    current: CurrentSession,
) -> Result<Json<reports::EventsReport>, ApiError> {
    let request = ReportRequest::resolve(&state, &current, &query).await?;
    let report = reports::events(
        state.db().pool(),
        request.site.id,
        request.range,
        &request.filters,
    )
    .await?;

    Ok(Json(report))
}

/// `GET /api/v1/analytics/events/{name}` — one event: counts, days and property breakdown.
pub async fn event_detail(
    State(state): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    Query(query): Query<ReportQuery>,
    current: CurrentSession,
) -> Result<Json<reports::EventDetail>, ApiError> {
    let request = ReportRequest::resolve(&state, &current, &query).await?;
    let report = reports::event_detail(
        state.db().pool(),
        request.site.id,
        &name,
        request.range,
    )
    .await?;

    Ok(Json(report))
}

/// `GET /api/v1/analytics/downloads` — downloads by file and by page.
pub async fn downloads(
    State(state): State<AppState>,
    Query(query): Query<ReportQuery>,
    current: CurrentSession,
) -> Result<Json<reports::DownloadsReport>, ApiError> {
    let request = ReportRequest::resolve(&state, &current, &query).await?;
    let report = reports::downloads(
        state.db().pool(),
        request.site.id,
        request.range,
        &request.filters,
    )
    .await?;

    Ok(Json(report))
}

/// `GET /api/v1/analytics/forms` — submissions, completion and abandonment per form.
pub async fn forms(
    State(state): State<AppState>,
    Query(query): Query<ReportQuery>,
    current: CurrentSession,
) -> Result<Json<reports::FormsReport>, ApiError> {
    let request = ReportRequest::resolve(&state, &current, &query).await?;
    let report = reports::forms(
        state.db().pool(),
        request.site.id,
        request.range,
        &request.filters,
    )
    .await?;

    Ok(Json(report))
}

/// `GET /api/v1/analytics/export` — the report the screen showed, as a file.
///
/// The file is built from the same query the screen ran (its range, its filters, its sort), so
/// the rows in the file and the rows on the screen are the same rows — and the count rides in a
/// header, because a silently truncated export would make the file lie.
pub async fn export(
    State(state): State<AppState>,
    Query(query): Query<ReportQuery>,
    current: CurrentSession,
) -> Result<axum::response::Response, ApiError> {
    let request = ReportRequest::resolve(&state, &current, &query).await?;
    let report = build_report(&state, &request, &query).await?;
    let format = query.format.as_deref().unwrap_or("csv").trim();

    match format {
        "json" => Ok(Json(report).into_response()),
        "csv" => {
            let (_, rows) = report.csv_rows();
            let filename = format!(
                "omnion-analytics-{}-{}.csv",
                report.key(),
                request.range.label()
            );
            let headers = [
                (
                    axum::http::header::CONTENT_TYPE,
                    "text/csv; charset=utf-8".to_owned(),
                ),
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{filename}\""),
                ),
                (
                    axum::http::HeaderName::from_static("x-export-rows"),
                    rows.len().to_string(),
                ),
            ];
            let mut response =
                axum::response::Response::new(axum::body::Body::from(report.csv()));
            for (name, value) in headers {
                if let Ok(value) = value.parse() {
                    response.headers_mut().insert(name, value);
                }
            }

            Ok(response)
        }
        other => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_report_query",
            format!("format \"{other}\" is not one of csv, json"),
        )),
    }
}

/// Build the report `report=` names, with the same inputs the screens use.
async fn build_report(
    state: &AppState,
    request: &ReportRequest,
    query: &ReportQuery,
) -> Result<Report, ApiError> {
    let pool = state.db().pool();
    let site_id = request.site.id;
    let key = query.report.as_deref().map(str::trim).unwrap_or("overview");

    let report = match key {
        "overview" => {
            let settings = store::ensure(pool, site_id).await?;
            let granularity = Granularity::parse(query.granularity.as_deref(), request.range)?;
            let today = OffsetDateTime::now_utc().date();
            Report::Overview(
                reports::overview(
                    pool,
                    site_id,
                    request.range,
                    request.compare(query),
                    granularity,
                    settings.retention_days,
                    today,
                )
                .await?,
            )
        }
        "pages" => Report::Pages(
            reports::pages(
                pool,
                site_id,
                request.range,
                &request.filters,
                query.sort.as_deref(),
                query.dir.as_deref(),
                query.page,
                query.per_page,
            )
            .await?,
        ),
        "sources" => Report::Sources(
            reports::sources(
                pool,
                site_id,
                request.range,
                &request.filters,
                query.group.as_deref(),
            )
            .await?,
        ),
        "audience" => Report::Audience(
            reports::audience(pool, site_id, request.range, &request.filters).await?,
        ),
        "events" => {
            Report::Events(reports::events(pool, site_id, request.range, &request.filters).await?)
        }
        "downloads" => Report::Downloads(
            reports::downloads(pool, site_id, request.range, &request.filters).await?,
        ),
        "forms" => {
            Report::Forms(reports::forms(pool, site_id, request.range, &request.filters).await?)
        }
        other => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_report_query",
                format!(
                    "report \"{other}\" is not one of overview, pages, sources, audience, \
                     events, downloads, forms"
                ),
            ));
        }
    };

    Ok(report)
}

/// Read a day (`YYYY-MM-DD`), falling back when the caller named none.
fn parse_day(value: Option<&str>, fallback: Date) -> Result<Date, ApiError> {
    match value.map(str::trim).filter(|text| !text.is_empty()) {
        None => Ok(fallback),
        Some(text) => reports::day_from_str(text).ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_report_query",
                format!("\"{text}\" is not a day (YYYY-MM-DD)"),
            )
        }),
    }
}

/// The country an edge reported for the caller, when it reported one.
///
/// The platform never geolocates an address itself: a deployment's edge (Cloudflare, a load
/// balancer, a CDN in front of the API) resolved it, and only the two-letter answer is stored.
/// The headers are read in the order the common edges set them.
fn edge_country(headers: &HeaderMap) -> Option<String> {
    for name in ["cf-ipcountry", "x-vercel-ip-country", "x-country-code"] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok())
            && let Some(country) = collect::country_from_header(Some(value))
        {
            return Some(country);
        }
    }

    None
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Resolve a site and prove the caller may look at it (docs/07-IAM.md §7).
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

/// The address a beacon is attributed to.
///
/// `X-Forwarded-For` is read first because the platform's own edge sets it; the socket address
/// is the fallback. The value is used for the daily hash, the exclusion lists and the rate
/// limit — never for authorization — and a deployment that does not strip inbound
/// `X-Forwarded-For` lets a caller pick their own bucket, which is why the rate limit is a
/// guardrail rather than a defence (see `docs/qa` receipts and the deployment doc).
fn visitor_address(headers: &HeaderMap, socket: Option<IpAddr>) -> Option<IpAddr> {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .and_then(|value| value.trim().parse::<IpAddr>().ok())
        .or(socket)
}

/// The user agent, or an empty string when the caller sent none (which the bot filter reads as
/// "not a person").
fn user_agent(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// `true` when a header carries exactly this value.
fn header_is(headers: &HeaderMap, name: &'static str, value: &str) -> bool {
    headers
        .get(name)
        .and_then(|header| header.to_str().ok())
        .map(str::trim)
        == Some(value)
}

/// The host the panel was reached on, lower-cased and without its port.
fn request_host(headers: &HeaderMap) -> Option<String> {
    let raw = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(axum::http::header::HOST))?
        .to_str()
        .ok()?
        .split(',')
        .next()?
        .trim()
        .to_lowercase();
    let host = raw.split(':').next().unwrap_or(raw.as_str()).to_owned();
    if host.is_empty() { None } else { Some(host) }
}

/// Rate-limit key of one site and caller.
fn rate_key(site_id: Uuid, address: Option<IpAddr>) -> String {
    format!(
        "{site_id}|{}",
        address.map(|ip| ip.to_string()).unwrap_or_default()
    )
}

/// The process-wide limiter of the public collection endpoint.
///
/// Per instance on purpose (slice 1 has no shared store in the request path): a fleet of API
/// instances each allows the budget, which is a guardrail against a runaway script, not a
/// billing meter. The shared limiter arrives with Redis-backed counters.
fn limiter() -> &'static BeaconLimiter {
    static LIMITER: OnceLock<BeaconLimiter> = OnceLock::new();
    LIMITER.get_or_init(BeaconLimiter::default)
}

/// Fixed-window counter kept in memory.
#[derive(Debug, Default)]
struct BeaconLimiter {
    buckets: Mutex<HashMap<String, (Instant, u64)>>,
}

impl BeaconLimiter {
    /// Count one beacon against `key`; `false` means the budget is spent for this window.
    fn allows(&self, key: &str, budget: u64, now: Instant) -> bool {
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if buckets.len() > RATE_BUCKETS {
            buckets.retain(|_, (started, _)| now.duration_since(*started) < RATE_WINDOW);
        }

        let entry = buckets.entry(key.to_owned()).or_insert((now, 0));
        if now.duration_since(entry.0) >= RATE_WINDOW {
            *entry = (now, 0);
        }
        entry.1 += 1;

        entry.1 <= budget
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn the_forwarded_address_wins_and_the_socket_is_the_fallback() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.9, 10.0.0.1"),
        );
        let socket: IpAddr = "198.51.100.4".parse().unwrap();
        assert_eq!(
            visitor_address(&headers, Some(socket)),
            Some("203.0.113.9".parse().unwrap())
        );

        let empty = HeaderMap::new();
        assert_eq!(visitor_address(&empty, Some(socket)), Some(socket));
        assert_eq!(visitor_address(&empty, None), None);
    }

    #[test]
    fn privacy_headers_are_read_verbatim() {
        let mut headers = HeaderMap::new();
        headers.insert("dnt", HeaderValue::from_static("1"));
        headers.insert("sec-gpc", HeaderValue::from_static("0"));
        assert!(header_is(&headers, "dnt", "1"));
        assert!(!header_is(&headers, "sec-gpc", "1"));
        assert!(!header_is(&headers, "dnt", "0"));
    }

    #[test]
    fn the_rate_limiter_spends_its_budget_and_refills_with_the_next_window() {
        let limiter = BeaconLimiter::default();
        let start = Instant::now();
        let key = "site|203.0.113.9";

        for _ in 0..3 {
            assert!(limiter.allows(key, 3, start));
        }
        assert!(
            !limiter.allows(key, 3, start),
            "the fourth beacon is over budget"
        );
        assert!(
            limiter.allows("other|203.0.113.9", 3, start),
            "another key has its own budget"
        );
        assert!(
            limiter.allows(key, 3, start + RATE_WINDOW),
            "the next window starts empty"
        );
    }

    #[test]
    fn a_host_header_loses_its_port() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            HeaderValue::from_static("Omnion.TEST:18080"),
        );
        assert_eq!(request_host(&headers).as_deref(), Some("omnion.test"));
    }
}
