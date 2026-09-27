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
use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::IntervalStream;
use uuid::Uuid;

use omnion_events::{NewEvent, bus};
use omnion_identity::Site;
use omnion_identity::sites;
use omnion_module_analytics::collect::{self, Beacon, RequestMeta};
use omnion_module_analytics::goals::{self, Goal, GoalChanges, GoalPatch};
use omnion_module_analytics::privacy::{self, ErasureOutcome, PurgeOutcome, PurgeRecord, StoredField};
use omnion_module_analytics::realtime::{self, RealtimeSnapshot};
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

    // A finished goal is a fact the rest of the platform acts on (REQ-007 §Events): the bus
    // records it and queues the deliveries its subscribers asked for. Only a hit this beacon
    // actually wrote is announced — a re-sent beacon changed nothing and says nothing. A bus that
    // cannot record the fact is a warning, not a failed beacon: the visit is already stored, and
    // a marketing automation that misses one conversion must not also lose the traffic behind it.
    for reached in report.reached_goals.iter().filter(|hit| hit.is_final) {
        let emission = bus::emit(
            pool,
            NewEvent::new("analytics.goal_reached")
                .organization(site.organization_id)
                .site(site.id)
                .payload(serde_json::json!({
                    "goal_id": reached.goal_id,
                    "goal": reached.goal,
                    "step_position": reached.step_position,
                    "visitor": reached.visitor,
                    "path": reached.path,
                    "value": reached.value,
                })),
        )
        .await;
        if let Err(error) = emission {
            tracing::warn!(
                site_id = %site.id,
                goal_id = %reached.goal_id,
                error = %error,
                "the goal event could not be recorded"
            );
        }
    }

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

/// The settings of one site, plus the defaults the screen's "restore defaults" fills in and the
/// two pieces the data section of the screen needs (slice 4): the cutoff a purge would use right
/// now, the last run, and the "what we store" table.
#[derive(Debug, Serialize)]
pub struct SettingsResponse {
    /// The stored configuration.
    pub settings: Settings,
    /// The platform's defaults, sent by the server so the screen never hard-codes them.
    pub defaults: SettingsChanges,
    /// The cutoff `POST /analytics/purge` would use at this moment — the screen names it before
    /// the button is pressed, so "run purge now" never surprises its operator.
    #[serde(with = "time::serde::rfc3339")]
    pub purge_cutoff: OffsetDateTime,
    /// The most recent purge or erasure of this site, when there was one.
    pub last_purge: Option<PurgeRecord>,
    /// What the engine stores, column by column.
    pub storage: Vec<StoredField>,
}

/// The whole settings payload of one site.
async fn settings_payload(
    state: &AppState,
    site_id: Uuid,
) -> Result<SettingsResponse, ApiError> {
    let pool = state.db().pool();
    let settings = store::ensure(pool, site_id).await?;

    Ok(SettingsResponse {
        defaults: SettingsChanges::defaults(),
        purge_cutoff: privacy::cutoff_for(settings.retention_days, OffsetDateTime::now_utc()),
        last_purge: privacy::last_purge(pool, site_id).await?,
        storage: privacy::stored_fields(),
        settings,
    })
}

/// `GET /api/v1/analytics/settings` — how this site counts (docs/requests/REQ-007).
pub async fn get_settings(
    State(state): State<AppState>,
    Query(query): Query<SiteQuery>,
    current: CurrentSession,
) -> Result<Json<SettingsResponse>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;

    Ok(Json(settings_payload(&state, site.id).await?))
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
    store::update(state.db().pool(), site.id, &changes, Some(current.user.id)).await?;

    Ok(Json(settings_payload(&state, site.id).await?))
}

/// `POST /api/v1/analytics/purge` — run the retention purge now.
///
/// The audit row is written by the purge itself, inside the same transaction as the deletions;
/// the platform event is the second half, so a subscriber learns what happened without reading
/// the database. A bus that cannot record the fact is a warning, not a failed purge: the rows
/// are gone, which is what the operator asked for.
pub async fn purge(
    State(state): State<AppState>,
    Query(query): Query<SiteQuery>,
    current: CurrentSession,
) -> Result<Json<PurgeOutcome>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let pool = state.db().pool();
    let settings = store::ensure(pool, site.id).await?;
    let outcome =
        privacy::purge(pool, site.id, settings.retention_days, Some(current.user.id)).await?;

    let emission = bus::emit(
        pool,
        NewEvent::new("analytics.retention_purged")
            .organization(site.organization_id)
            .site(site.id)
            .actor(current.user.id)
            .payload(serde_json::json!({
                "purge_id": outcome.purge_id,
                "kind": outcome.kind,
                "cutoff": outcome.cutoff,
                "rows_removed": outcome.rows_removed,
                "visits": outcome.visits,
                "pageviews": outcome.pageviews,
                "events": outcome.events,
                "goal_hits": outcome.goal_hits,
            })),
    )
    .await;
    if let Err(error) = emission {
        tracing::warn!(site_id = %site.id, error = %error, "the purge event could not be recorded");
    }

    Ok(Json(outcome))
}

/// `DELETE /api/v1/analytics/visitors/{hash}` — erase every row of one visitor handle.
///
/// The handle is already a pseudonym (the daily-salted hash), never an address; the answer
/// carries how many rows went, so an operator can tell "erased a visitor" from "that handle never
/// existed here" without reading the database.
pub async fn erase_visitor(
    State(state): State<AppState>,
    axum::extract::Path(handle): axum::extract::Path<String>,
    Query(query): Query<SiteQuery>,
    current: CurrentSession,
) -> Result<Json<ErasureOutcome>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let pool = state.db().pool();
    let outcome = privacy::erase_visitor(pool, site.id, &handle, Some(current.user.id)).await?;

    let emission = bus::emit(
        pool,
        NewEvent::new("analytics.erasure_completed")
            .organization(site.organization_id)
            .site(site.id)
            .actor(current.user.id)
            .payload(serde_json::json!({
                "purge_id": outcome.purge_id,
                "visitor": outcome.visitor,
                "rows_removed": outcome.rows_removed,
                "visits": outcome.visits,
                "pageviews": outcome.pageviews,
                "events": outcome.events,
                "goal_hits": outcome.goal_hits,
            })),
    )
    .await;
    if let Err(error) = emission {
        tracing::warn!(site_id = %site.id, error = %error, "the erasure event could not be recorded");
    }

    Ok(Json(outcome))
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
    let report =
        reports::event_detail(state.db().pool(), request.site.id, &name, request.range).await?;

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
            let mut response = axum::response::Response::new(axum::body::Body::from(report.csv()));
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

// ---------------------------------------------------------------------------------------------
// Goals (panel)
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/analytics/goals` — every goal of a site with how it did in the range.
#[derive(Debug, Serialize)]
pub struct GoalsResponse {
    /// First day the numbers cover.
    pub from: Date,
    /// Last day the numbers cover.
    pub to: Date,
    /// The goals, ordered by name.
    pub goals: Vec<goals::GoalSummary>,
}

/// `GET /api/v1/analytics/goals` — the goal list with conversions, rates and last hit.
pub async fn goals_index(
    State(state): State<AppState>,
    Query(query): Query<ReportQuery>,
    current: CurrentSession,
) -> Result<Json<GoalsResponse>, ApiError> {
    let request = ReportRequest::resolve(&state, &current, &query).await?;
    let goals = goals::list(state.db().pool(), request.site.id, request.range).await?;

    Ok(Json(GoalsResponse {
        from: request.range.from,
        to: request.range.to,
        goals,
    }))
}

/// `POST /api/v1/analytics/goals` — create a goal, optionally a funnel of steps.
pub async fn goal_create(
    State(state): State<AppState>,
    Query(query): Query<ReportQuery>,
    current: CurrentSession,
    Json(changes): Json<GoalChanges>,
) -> Result<(StatusCode, Json<Goal>), ApiError> {
    let request = ReportRequest::resolve(&state, &current, &query).await?;
    let goal = goals::create(
        state.db().pool(),
        request.site.id,
        Some(current.user.id),
        &changes,
    )
    .await?;

    Ok((StatusCode::CREATED, Json(goal)))
}

/// `GET /api/v1/analytics/goals/{id}` — one goal with its steps, as the editor reads it.
pub async fn goal_get(
    State(state): State<AppState>,
    axum::extract::Path(goal_id): axum::extract::Path<Uuid>,
    Query(query): Query<ReportQuery>,
    current: CurrentSession,
) -> Result<Json<Goal>, ApiError> {
    let request = ReportRequest::resolve(&state, &current, &query).await?;
    let goal = goals::get(state.db().pool(), request.site.id, goal_id).await?;

    Ok(Json(goal))
}

/// `PATCH /api/v1/analytics/goals/{id}` — update what the body carries, keep the rest.
///
/// The switch, the name and the funnel are all partial updates of one resource: a screen that
/// turns a goal off must not have to re-send the match it never showed, and a screen that edits
/// the last step must not have to re-send the first.
pub async fn goal_patch(
    State(state): State<AppState>,
    axum::extract::Path(goal_id): axum::extract::Path<Uuid>,
    Query(query): Query<ReportQuery>,
    current: CurrentSession,
    Json(changes): Json<GoalPatch>,
) -> Result<Json<Goal>, ApiError> {
    let request = ReportRequest::resolve(&state, &current, &query).await?;
    let goal = goals::patch(state.db().pool(), request.site.id, goal_id, &changes).await?;

    Ok(Json(goal))
}

/// `DELETE /api/v1/analytics/goals/{id}` — remove a goal; its steps and hits go with it.
pub async fn goal_delete(
    State(state): State<AppState>,
    axum::extract::Path(goal_id): axum::extract::Path<Uuid>,
    Query(query): Query<ReportQuery>,
    current: CurrentSession,
) -> Result<StatusCode, ApiError> {
    let request = ReportRequest::resolve(&state, &current, &query).await?;
    goals::delete(state.db().pool(), request.site.id, goal_id).await?;

    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/analytics/goals/{id}/funnel` — one goal's steps for the range.
pub async fn goal_funnel(
    State(state): State<AppState>,
    axum::extract::Path(goal_id): axum::extract::Path<Uuid>,
    Query(query): Query<ReportQuery>,
    current: CurrentSession,
) -> Result<Json<goals::Funnel>, ApiError> {
    let request = ReportRequest::resolve(&state, &current, &query).await?;
    let funnel = goals::funnel(state.db().pool(), request.site.id, goal_id, request.range).await?;

    Ok(Json(funnel))
}

// ---------------------------------------------------------------------------------------------
// Realtime (panel)
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/analytics/realtime` — the snapshot of the last half hour.
pub async fn realtime(
    State(state): State<AppState>,
    Query(query): Query<SiteQuery>,
    current: CurrentSession,
) -> Result<Json<RealtimeSnapshot>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let snapshot =
        realtime::snapshot(state.db().pool(), site.id, OffsetDateTime::now_utc()).await?;

    Ok(Json(snapshot))
}

/// How many realtime streams one site may hold open at once.
///
/// Realtime is polling under the hood, so each stream is a query every few seconds: the cap is
/// what keeps a wall of forgotten tabs from becoming the site's own load test. A reader who is
/// over the cap gets a `429` and the screen keeps its last snapshot on screen.
const MAX_REALTIME_STREAMS: usize = 8;

/// How often a stream answers with a fresh snapshot.
const REALTIME_TICK: Duration = Duration::from_secs(5);

/// How many snapshots one stream may carry before it ends (half an hour at [`REALTIME_TICK`]).
///
/// `EventSource` reconnects on its own, which is the point: the next connection re-checks the
/// reader's session and the site's scope, and a closed tab stops costing anything.
const REALTIME_STREAM_TICKS: usize = 360;

/// `GET /api/v1/analytics/realtime/stream` — server-sent snapshots of the last half hour.
pub async fn realtime_stream(
    State(state): State<AppState>,
    Query(query): Query<SiteQuery>,
    current: CurrentSession,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let guard = RealtimeStreams::acquire(site.id)?;
    let pool = state.db().pool().clone();
    let site_id = site.id;

    let stream = IntervalStream::new(tokio::time::interval(REALTIME_TICK))
        .take(REALTIME_STREAM_TICKS)
        .then(move |_| {
            let pool = pool.clone();
            // The guard travels with the stream: when the browser drops it, the slot is free.
            let _slot = &guard;
            async move {
                match realtime::snapshot(&pool, site_id, OffsetDateTime::now_utc()).await {
                    Ok(snapshot) => Ok(SseEvent::default().event("snapshot").data(
                        serde_json::to_string(&snapshot).unwrap_or_else(|_| "{}".to_owned()),
                    )),
                    Err(error) => Ok(SseEvent::default().event("error").data(error.to_string())),
                }
            }
        });

    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

/// The per-site count of live realtime streams.
#[derive(Debug, Default)]
struct RealtimeStreams {
    counts: Mutex<HashMap<Uuid, usize>>,
}

impl RealtimeStreams {
    /// One process-wide counter; the streams are answered by every API instance, so this is a
    /// per-instance guardrail exactly like the beacon limiter, and the shared counter arrives
    /// with the Redis-backed limiter.
    fn counts() -> &'static RealtimeStreams {
        static COUNTS: OnceLock<RealtimeStreams> = OnceLock::new();
        COUNTS.get_or_init(RealtimeStreams::default)
    }

    /// Take one slot of a site, or refuse when the site already holds too many.
    fn acquire(site_id: Uuid) -> Result<RealtimeStreamGuard, ApiError> {
        let counts = Self::counts();
        let mut live = counts
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let entry = live.entry(site_id).or_insert(0);
        if *entry >= MAX_REALTIME_STREAMS {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too_many_streams",
                format!("a site may hold {MAX_REALTIME_STREAMS} realtime streams at once"),
            ));
        }
        *entry += 1;

        Ok(RealtimeStreamGuard { site_id })
    }

    /// Give one slot back.
    fn release(site_id: Uuid) {
        let counts = Self::counts();
        let mut live = counts
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if let Some(entry) = live.get_mut(&site_id) {
            *entry = entry.saturating_sub(1);
            if *entry == 0 {
                live.remove(&site_id);
            }
        }
    }
}

/// One held stream slot, released when the stream is dropped.
#[derive(Debug)]
struct RealtimeStreamGuard {
    site_id: Uuid,
}

impl Drop for RealtimeStreamGuard {
    fn drop(&mut self) {
        RealtimeStreams::release(self.site_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn the_realtime_stream_cap_is_per_site_and_frees_its_slots() {
        let site = Uuid::new_v4();
        let other = Uuid::new_v4();

        let mut held = Vec::new();
        for _ in 0..MAX_REALTIME_STREAMS {
            held.push(RealtimeStreams::acquire(site).expect("a slot inside the cap"));
        }
        let refused = RealtimeStreams::acquire(site).unwrap_err();
        assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);

        // Another site has its own budget.
        let other_guard = RealtimeStreams::acquire(other).expect("another site has its own slots");

        // Dropping one stream frees exactly one slot.
        held.pop();
        let again = RealtimeStreams::acquire(site).expect("the freed slot is available");
        drop(again);
        drop(other_guard);
        for guard in held {
            drop(guard);
        }
    }

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
