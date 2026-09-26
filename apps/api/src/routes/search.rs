//! `/api/v1/search` — the platform's one search box (docs/requests/REQ-002).
//!
//! The engine (`omnion-search`) owns the index, the query language and the ranking; this module
//! is the HTTP shape around it. Three promises it keeps:
//!
//! * **`search.read` is the box, not the content.** Every signed-in account may search; which
//!   providers answer is decided per request from the caller's own permission set, so an editor
//!   without `users.read` gets pages and media and simply no user rows — no error, no leak.
//! * **The scope rides the query.** The caller's organization is a `WHERE` clause, never a
//!   post-filter: page two of a result set can not contain something page one was not allowed
//!   to count.
//! * **Index operations are their own power.** Rebuilding an index is `search.manage`, and it
//!   leaves an audit row plus a `search.reindexed` event, because it is a platform act.

use axum::Json;
use axum::extract::{Query as QueryParams, State};
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_permissions::effective_permissions;
use omnion_search::indexer;
use omnion_search::providers;
use omnion_search::query::{
    self, DEFAULT_PER_PAGE, HitPage, MAX_PER_PAGE, Query as SearchQuery, SearchFilters,
    SearchRequest, Sort, UpdatedRange,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::guards::scope_of;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Request shapes
// ---------------------------------------------------------------------------------------------

/// Query string of `GET /api/v1/search` (and of `/search/export`, which accepts the same set).
///
/// The text lives in `q` — including the scoped syntax a person can type — while every filter the
/// facet rail applies has its own parameter, so a result set is a URL and a link is a filter.
#[derive(Debug, Deserialize)]
pub struct SearchParams {
    /// The query text, including the scoped syntax (`type:page site:acme is:draft`).
    pub q: Option<String>,
    /// One-based page number; `1` when absent.
    pub page: Option<i64>,
    /// Hits per page; `1..=100`, default 25.
    pub per_page: Option<i64>,
    /// `relevance` (default), `newest` or `title`.
    pub sort: Option<String>,
    /// Comma-separated provider keys or entity types (`pages,media`).
    pub types: Option<String>,
    /// A site id, key or domain host.
    pub site_id: Option<String>,
    /// `me`, or the uuid of one account.
    pub owner: Option<String>,
    /// An exact language code (`en`, `tr`).
    pub language: Option<String>,
    /// One status tag (`draft`, `published`, `archived`).
    pub status: Option<String>,
    /// `today`, `week`, `month`, `older` or `never`.
    pub updated: Option<String>,
    /// Exclusive upper bound on the last change (`YYYY-MM-DD`).
    pub before: Option<String>,
    /// Inclusive lower bound on the last change (`YYYY-MM-DD`).
    pub after: Option<String>,
    /// `true` asks for the facet rail's counts next to the hits.
    pub facets: Option<String>,
}

/// Query string of `GET /api/v1/search/export`: the search parameters plus an optional selection.
#[derive(Debug, Deserialize)]
pub struct ExportParams {
    /// Everything `GET /api/v1/search` accepts.
    #[serde(flatten)]
    pub search: SearchParams,
    /// `provider:entity_type:entity_id` keys, comma-separated — export exactly these hits.
    pub selected: Option<String>,
}

/// Query string of `GET /api/v1/search/suggest`.
#[derive(Debug, Deserialize)]
pub struct SuggestParams {
    /// The prefix to suggest for.
    pub q: Option<String>,
}

/// Body of `POST /api/v1/search/reindex`.
#[derive(Debug, Deserialize)]
pub struct ReindexBody {
    /// One provider key, or nothing for every provider.
    pub provider: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------------------------

/// One hit as the API answers it.
#[derive(Debug, Serialize)]
pub struct HitBody {
    /// Provider key (`pages`).
    pub provider: String,
    /// Document type (`page`).
    pub entity_type: String,
    /// Entity id inside its own domain.
    pub entity_id: String,
    /// Title.
    pub title: String,
    /// Supporting line.
    pub subtitle: String,
    /// Panel route a click opens.
    pub url: String,
    /// Display name of the account the entity belongs to, when it has one.
    pub owner: Option<String>,
    /// Tags stored with the document.
    pub tags: Vec<String>,
    /// When the entity last changed, RFC 3339.
    #[serde(with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,
    /// Rank inside this answer.
    pub score: f32,
}

impl From<query::Hit> for HitBody {
    fn from(hit: query::Hit) -> Self {
        Self {
            provider: hit.provider,
            entity_type: hit.entity_type,
            entity_id: hit.entity_id,
            title: hit.title,
            subtitle: hit.subtitle,
            url: hit.url,
            owner: hit.owner,
            tags: hit.tags,
            updated_at: hit.entity_updated_at,
            score: hit.score,
        }
    }
}

/// One value of a facet rail.
#[derive(Debug, Serialize)]
pub struct FacetValueBody {
    /// The machine value (`pages`, a uuid, `draft`, `week`).
    pub value: String,
    /// The human label.
    pub label: String,
    /// Hits this value would leave under every other filter.
    pub count: i64,
}

/// One group of the facet rail.
#[derive(Debug, Serialize)]
pub struct FacetGroupBody {
    /// Stable key (`type`, `site`, `owner`, `language`, `status`, `updated`).
    pub key: &'static str,
    /// Title the rail shows.
    pub title: &'static str,
    /// The values, strongest first.
    pub values: Vec<FacetValueBody>,
    /// How many further values exist beyond the ones listed.
    pub more: i64,
}

impl From<query::FacetGroup> for FacetGroupBody {
    fn from(group: query::FacetGroup) -> Self {
        Self {
            key: group.key,
            title: group.title,
            values: group
                .values
                .into_iter()
                .map(|value| FacetValueBody {
                    value: value.value,
                    label: value.label,
                    count: value.count,
                })
                .collect(),
            more: group.more,
        }
    }
}

/// One provider's share of a result set (`pages: 3`) — the palette's section counts.
#[derive(Debug, Serialize)]
pub struct CountBody {
    /// Provider key.
    pub provider: String,
    /// Display title.
    pub title: &'static str,
    /// Panel route of the section.
    pub route: &'static str,
    /// Hits the provider contributes.
    pub count: i64,
}

/// The whole answer of one search.
#[derive(Debug, Serialize)]
pub struct SearchResponse {
    /// The query as it was understood (trimmed and capped).
    pub query: String,
    /// The text terms.
    pub terms: Vec<String>,
    /// The `type:` filters.
    pub types: Vec<String>,
    /// The `is:` flags.
    pub flags: Vec<String>,
    /// Everything the parser refused, in caller-facing language.
    pub hints: Vec<String>,
    /// The hits of this page, best first.
    pub hits: Vec<HitBody>,
    /// Total hits the query matches.
    pub total: i64,
    /// One-based page number.
    pub page: i64,
    /// Hits per page.
    pub per_page: i64,
    /// Which provider contributed how many (ordered by count).
    pub counts: Vec<CountBody>,
    /// The facet rail's counts; only answered when `facets=true` was asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facets: Option<Vec<FacetGroupBody>>,
    /// How long the search took, in milliseconds.
    pub took_ms: u64,
}

/// One prefix suggestion.
#[derive(Debug, Serialize)]
pub struct SuggestionBody {
    /// Title of the suggested document.
    pub title: String,
    /// Panel route it opens.
    pub url: String,
    /// Provider it belongs to.
    pub provider: String,
}

/// Answer of `GET /api/v1/search/suggest`.
#[derive(Debug, Serialize)]
pub struct SuggestResponse {
    /// The suggestions, at most eight.
    pub suggestions: Vec<SuggestionBody>,
}

/// The caller's own recent searches.
#[derive(Debug, Serialize)]
pub struct RecentResponse {
    /// The queries, newest first (at most twenty are kept).
    pub queries: Vec<String>,
}

/// One reindex pass as the status screen reads it.
#[derive(Debug, Serialize)]
pub struct ReindexRunBody {
    /// Documents the pass wrote, when it finished.
    pub indexed: Option<i64>,
    /// Documents the pass pruned, when it finished.
    pub pruned: Option<i64>,
    /// Wall time, in milliseconds.
    pub duration_ms: Option<i64>,
    /// When the pass started, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    /// When it finished; `null` while it is running.
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    /// Why it failed, when it did.
    pub error: Option<String>,
}

impl From<query::ReindexRun> for ReindexRunBody {
    fn from(run: query::ReindexRun) -> Self {
        Self {
            indexed: run.indexed,
            pruned: run.pruned,
            duration_ms: run.duration_ms,
            started_at: run.started_at,
            finished_at: run.finished_at,
            error: run.error,
        }
    }
}

/// One provider's line on the status screen.
#[derive(Debug, Serialize)]
pub struct StatusBody {
    /// Provider key.
    pub provider: &'static str,
    /// Display title.
    pub title: &'static str,
    /// Documents in the index.
    pub documents: i64,
    /// When its rows were last written, RFC 3339.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_indexed_at: Option<OffsetDateTime>,
    /// `indexing`, `failed`, `stale`, `ready` or `empty`.
    pub state: &'static str,
    /// The most recent pass, when one ever ran.
    pub last_run: Option<ReindexRunBody>,
}

/// Answer of `GET /api/v1/search/status`.
#[derive(Debug, Serialize)]
pub struct StatusResponse {
    /// One line per registered provider.
    pub providers: Vec<StatusBody>,
    /// Documents across every provider.
    pub documents: i64,
}

/// The ranking weights as the settings screen writes them.
#[derive(Debug, Serialize, Deserialize)]
pub struct WeightsBody {
    /// Weight of a title match.
    pub title: i32,
    /// Weight of a tag match.
    pub tags: i32,
    /// Weight of a subtitle match.
    pub subtitle: i32,
    /// Weight of a body match.
    pub body: i32,
}

impl From<query::Weights> for WeightsBody {
    fn from(weights: query::Weights) -> Self {
        Self {
            title: weights.title,
            tags: weights.tags,
            subtitle: weights.subtitle,
            body: weights.body,
        }
    }
}

/// Answer of `GET /api/v1/search/settings`.
#[derive(Debug, Serialize)]
pub struct SettingsResponse {
    /// The stored ranking weights.
    pub weights: WeightsBody,
    /// The weights an installation starts with, so "restore defaults" is the server's own answer.
    pub defaults: WeightsBody,
    /// Provider keys that answer a query, in registry order.
    pub enabled_providers: Vec<String>,
    /// Every provider key this build knows, in registry order.
    pub available_providers: Vec<String>,
    /// When the row was last written, RFC 3339.
    #[serde(with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,
}

impl SettingsResponse {
    /// Build the answer from a stored settings value.
    #[must_use]
    pub fn from_settings(settings: query::SearchSettings) -> Self {
        let available: Vec<String> = providers::provider_keys()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let enabled = available
            .iter()
            .filter(|key| settings.enabled_providers.contains(key))
            .cloned()
            .collect();
        Self {
            weights: settings.weights.into(),
            defaults: query::DEFAULT_WEIGHTS.into(),
            enabled_providers: enabled,
            available_providers: available,
            updated_at: settings.updated_at,
        }
    }
}

/// Body of `PUT /api/v1/search/settings`.
#[derive(Debug, Deserialize)]
pub struct SaveSettingsBody {
    /// The ranking weights to store.
    pub weights: WeightsBody,
    /// The provider keys that should answer a query.
    pub enabled_providers: Vec<String>,
}

/// One provider's reindex result.
#[derive(Debug, Serialize)]
pub struct ReindexReportBody {
    /// Provider key.
    pub provider: &'static str,
    /// Documents written.
    pub indexed: u64,
    /// Documents pruned.
    pub pruned: u64,
    /// Wall time in milliseconds.
    pub duration_ms: u64,
}

/// Answer of `POST /api/v1/search/reindex`.
#[derive(Debug, Serialize)]
pub struct ReindexResponse {
    /// One report per provider that was rebuilt.
    pub providers: Vec<ReindexReportBody>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// Search every provider the caller's read permissions cover.
pub async fn search(
    State(state): State<AppState>,
    current: CurrentSession,
    QueryParams(params): QueryParams<SearchParams>,
) -> Result<Json<SearchResponse>, ApiError> {
    let request = search_request(&state, &current, &params).await?;
    let wants_facets = params
        .facets
        .as_deref()
        .is_some_and(|value| matches!(value.trim().to_lowercase().as_str(), "true" | "1" | "yes"));

    let started = std::time::Instant::now();
    let HitPage { hits, total } = query::search(state.db().pool(), &request).await?;
    // The history is a convenience, never the answer: a failed write is logged and the search
    // still answers.
    if let Err(error) = record_recent(state.db().pool(), current.user.id, request.query.raw()).await
    {
        tracing::warn!(
            code = error.code(),
            "the search history could not be written"
        );
    }
    let counts = query::counts(state.db().pool(), &request).await?;
    let facets = if wants_facets {
        Some(
            query::facets(state.db().pool(), &request)
                .await?
                .into_iter()
                .map(FacetGroupBody::from)
                .collect(),
        )
    } else {
        None
    };

    let counts = counts
        .into_iter()
        .filter_map(|row| {
            let spec = providers::provider(&row.provider)?;
            Some(CountBody {
                provider: row.provider,
                title: spec.title,
                route: spec.route,
                count: row.count,
            })
        })
        .collect();

    Ok(Json(SearchResponse {
        query: request.query.raw().to_owned(),
        terms: request.query.terms().to_vec(),
        types: request.query.types().to_vec(),
        flags: request.query.flags().to_vec(),
        hints: request.query.hints().to_vec(),
        hits: hits.into_iter().map(HitBody::from).collect(),
        total,
        page: request.page,
        per_page: request.per_page,
        counts,
        facets,
        took_ms: started.elapsed().as_millis() as u64,
    }))
}

/// How many hits one export may carry before it is truncated.
pub const EXPORT_MAX_ROWS: i64 = 5_000;

/// How many hits one export page holds while it walks the result set.
const EXPORT_PAGE: i64 = 500;

/// Export the current query as CSV — one row per hit, in the answer's own order.
///
/// With `selected=` the file holds exactly the rows the caller picked (the results screen sends
/// them when a selection exists); without it, the whole result set, walked in pages up to
/// [`EXPORT_MAX_ROWS`]. The count is a header the screen can check against, because a silent
/// truncation would make the file lie about the result set.
pub async fn export(
    State(state): State<AppState>,
    current: CurrentSession,
    QueryParams(params): QueryParams<ExportParams>,
) -> Result<axum::response::Response, ApiError> {
    let request = search_request(&state, &current, &params.search).await?;
    let selected = parse_selection(params.selected.as_deref())?;

    let mut rows: Vec<query::Hit> = Vec::new();
    let mut page = request.page.max(1);
    loop {
        let mut paged = request.clone();
        paged.page = page;
        paged.per_page = EXPORT_PAGE;
        let answer = query::search(state.db().pool(), &paged).await?;
        let fetched = answer.hits.len() as i64;
        rows.extend(answer.hits);
        let exhausted = fetched < EXPORT_PAGE || rows.len() as i64 >= EXPORT_MAX_ROWS;
        if exhausted || rows.len() as i64 >= EXPORT_MAX_ROWS {
            break;
        }
        page += 1;
    }

    // A selection filters the walked rows, so the file matches what the screen showed as checked.
    if let Some(selected) = &selected {
        rows.retain(|hit| {
            selected.contains(&format!(
                "{}:{}:{}",
                hit.provider, hit.entity_type, hit.entity_id
            ))
        });
    }

    let truncated = request.query.has_terms() && rows.len() as i64 > EXPORT_MAX_ROWS;
    if rows.len() as i64 > EXPORT_MAX_ROWS {
        rows.truncate(EXPORT_MAX_ROWS as usize);
    }

    let mut body = String::from("title,type,provider,owner,updated,tags,url,subtitle\n");
    for hit in &rows {
        let updated = hit
            .entity_updated_at
            .map(|at| at.date().to_string())
            .unwrap_or_default();
        body.push_str(
            &[
                csv_field(&hit.title),
                csv_field(&hit.entity_type),
                csv_field(&hit.provider),
                csv_field(hit.owner.as_deref().unwrap_or_default()),
                csv_field(&updated),
                csv_field(&hit.tags.join(" ")),
                csv_field(&hit.url),
                csv_field(&hit.subtitle),
            ]
            .join(","),
        );
        body.push('\n');
    }

    let filename = format!(
        "omnion-search-{}.csv",
        time::OffsetDateTime::now_utc().date()
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
        (
            axum::http::HeaderName::from_static("x-export-truncated"),
            truncated.to_string(),
        ),
    ];
    let mut response = axum::response::Response::new(axum::body::Body::from(body));
    for (name, value) in headers {
        if let Ok(value) = value.parse() {
            response.headers_mut().insert(name, value);
        }
    }
    Ok(response)
}

/// One CSV field: quoted when it has to be, with inner quotes doubled (RFC 4180).
#[must_use]
pub fn csv_field(value: &str) -> String {
    let needs_quotes = value
        .chars()
        .any(|ch| matches!(ch, ',' | '"' | '\n' | '\r'))
        || value.starts_with(' ')
        || value.ends_with(' ');
    if !needs_quotes {
        return value.to_owned();
    }
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// Parse the `selected=` list into the set of row keys it names.
fn parse_selection(
    raw: Option<&str>,
) -> Result<Option<std::collections::HashSet<String>>, ApiError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let keys: std::collections::HashSet<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect();
    if keys.is_empty() {
        return Ok(None);
    }
    // A key is `provider:entity_type:entity_id` — anything else is a client mistake worth naming.
    for key in &keys {
        if key.split(':').count() != 3 {
            return Err(ApiError::bad_request(
                "invalid_selection",
                "selected must be a comma-separated list of provider:entity_type:entity_id keys",
            ));
        }
    }
    Ok(Some(keys))
}

/// The installation's search settings.
pub async fn settings(State(state): State<AppState>) -> Result<Json<SettingsResponse>, ApiError> {
    let settings = query::read_settings(state.db().pool()).await?;
    Ok(Json(SettingsResponse::from_settings(settings)))
}

/// Save the ranking weights and the enabled providers.
pub async fn save_settings(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<SaveSettingsBody>,
) -> Result<Json<SettingsResponse>, ApiError> {
    let settings = query::SearchSettings {
        weights: query::Weights {
            title: body.weights.title,
            tags: body.weights.tags,
            subtitle: body.weights.subtitle,
            body: body.weights.body,
        },
        enabled_providers: body
            .enabled_providers
            .iter()
            .map(|key| key.trim().to_lowercase())
            .filter(|key| !key.is_empty())
            .collect(),
        updated_at: None,
    };

    if let Err(error) = query::validate_settings(&settings) {
        return Err(ApiError::bad_request(error.code(), error.to_string()));
    }

    let stored = query::write_settings(state.db().pool(), &settings).await?;

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "search.settings.updated")
            .target("search_settings", "1")
            .metadata(json!({
                "weights": {
                    "title": stored.weights.title,
                    "tags": stored.weights.tags,
                    "subtitle": stored.weights.subtitle,
                    "body": stored.weights.body,
                },
                "enabled_providers": stored.enabled_providers,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(Json(SettingsResponse::from_settings(stored)))
}

/// Title-prefix suggestions for the palette's first paint.
pub async fn suggest(
    State(state): State<AppState>,
    current: CurrentSession,
    QueryParams(params): QueryParams<SuggestParams>,
) -> Result<Json<SuggestResponse>, ApiError> {
    let prefix = params.q.as_deref().unwrap_or("").trim().to_owned();
    if prefix.is_empty() {
        return Ok(Json(SuggestResponse {
            suggestions: Vec::new(),
        }));
    }

    let providers = readable_providers(&state, &current).await?;
    let rows = query::suggest(
        state.db().pool(),
        current.user.organization_id,
        &providers,
        &prefix,
    )
    .await?;

    Ok(Json(SuggestResponse {
        suggestions: rows
            .into_iter()
            .map(|row| SuggestionBody {
                title: row.title,
                url: row.url,
                provider: row.provider,
            })
            .collect(),
    }))
}

/// The index's per-provider health.
pub async fn status(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<StatusResponse>, ApiError> {
    // Reading the index's shape is reading your own platform; the guard is `search.read`, and
    // the counts themselves name no content.
    let readable = readable_providers(&state, &current).await?;
    let lines = query::status(state.db().pool()).await?;
    let documents = lines.iter().map(|line| line.documents).sum();

    Ok(Json(StatusResponse {
        providers: lines
            .into_iter()
            .filter(|line| readable.contains(&line.provider))
            .map(|line| StatusBody {
                provider: line.provider,
                title: line.title,
                documents: line.documents,
                last_indexed_at: line.last_indexed_at,
                state: line.state,
                last_run: line.last_run.map(ReindexRunBody::from),
            })
            .collect(),
        documents,
    }))
}

/// Rebuild one provider's index, or every provider's.
pub async fn reindex(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<ReindexBody>,
) -> Result<Json<ReindexResponse>, ApiError> {
    let reports = match body.provider.as_deref().map(str::trim) {
        None | Some("") => indexer::reindex_all(state.db().pool()).await?,
        Some(key) => vec![indexer::reindex(state.db().pool(), key).await?],
    };

    for report in &reports {
        // The audit trail is part of the operation, not an afterthought: it names who rebuilt
        // what, and the event lets an installation watch its own search health.
        omnion_audit::record(
            state.db().pool(),
            NewAuditEntry::by_user(current.user.id, "search.reindexed")
                .target("search_provider", report.provider)
                .metadata(json!({
                    "indexed": report.indexed,
                    "pruned": report.pruned,
                    "duration_ms": report.duration_ms,
                }))
                .ip_address(address.as_text()),
        )
        .await?;

        bus::emit(
            state.db().pool(),
            NewEvent::new("search.reindexed")
                .actor(current.user.id)
                .payload(json!({
                    "provider": report.provider,
                    "documents": report.indexed,
                    "pruned": report.pruned,
                    "duration_ms": report.duration_ms,
                })),
        )
        .await?;
    }

    Ok(Json(ReindexResponse {
        providers: reports
            .into_iter()
            .map(|report| ReindexReportBody {
                provider: report.provider,
                indexed: report.indexed,
                pruned: report.pruned,
                duration_ms: report.duration_ms,
            })
            .collect(),
    }))
}

/// The caller's own recent searches, newest first.
pub async fn recent(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<RecentResponse>, ApiError> {
    let queries: Vec<String> = sqlx::query_scalar(
        "select query from search_recent where user_id = $1 \
         order by created_at desc, id desc limit 20",
    )
    .bind(current.user.id)
    .fetch_all(state.db().pool())
    .await
    .map_err(store)?;

    Ok(Json(RecentResponse { queries }))
}

/// Forget the caller's recent searches.
pub async fn clear_recent(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<axum::http::StatusCode, ApiError> {
    sqlx::query("delete from search_recent where user_id = $1")
        .bind(current.user.id)
        .execute(state.db().pool())
        .await
        .map_err(store)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Keep the caller's newest 20 queries, newest first.
async fn record_recent(pool: &sqlx::PgPool, user_id: Uuid, query: &str) -> Result<(), ApiError> {
    sqlx::query("insert into search_recent (user_id, query) values ($1, $2)")
        .bind(user_id)
        .bind(query)
        .execute(pool)
        .await
        .map_err(store)?;
    sqlx::query(
        "delete from search_recent where user_id = $1 and id not in ( \
             select id from search_recent where user_id = $1 \
             order by created_at desc, id desc limit 20) \
           and query <> $2",
    )
    .bind(user_id)
    .bind(query)
    .execute(pool)
    .await
    .map_err(store)?;
    Ok(())
}

/// The API shape of a raw store failure: the search crate's own `Store` variant carries it, so
/// the status mapping (a pool that cannot hand out a connection is retryable) stays in one place.
fn store(error: sqlx::Error) -> ApiError {
    omnion_search::SearchError::Store(error).into()
}

/// Parse the `sort` parameter; an unknown value is refused rather than defaulted away.
fn parse_sort(raw: Option<&str>) -> Result<Sort, ApiError> {
    match raw {
        None => Ok(Sort::Relevance),
        Some(value) => Sort::parse(value).ok_or_else(|| {
            ApiError::bad_request("unknown_sort", "sort is one of relevance, newest or title")
        }),
    }
}

/// Turn the query string into the filter set the engine narrows with.
///
/// Everything is validated here: an unparsable date or a malformed owner is a `400` naming the
/// parameter, never a silently ignored filter that makes the screen's chip lie about the results.
fn filters_from(params: &SearchParams) -> Result<SearchFilters, ApiError> {
    let mut filters = SearchFilters {
        types: params
            .types
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .map(|value| value.trim().to_lowercase())
            .filter(|value| !value.is_empty())
            .collect(),
        site: params
            .site_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        language: params
            .language
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        status: params
            .status
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        ..SearchFilters::default()
    };

    if let Some(owner) = params
        .owner
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        if owner.eq_ignore_ascii_case("me") {
            filters.owner_me = true;
        } else {
            filters.owner_id = Some(Uuid::parse_str(owner).map_err(|_| {
                ApiError::bad_request(
                    "invalid_owner",
                    "owner is either \"me\" or the id of one account",
                )
            })?);
        }
    }

    if let Some(updated) = params
        .updated
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        filters.updated = Some(UpdatedRange::parse(updated).ok_or_else(|| {
            ApiError::bad_request(
                "unknown_updated_range",
                "updated is one of today, week, month, older or never",
            )
        })?);
    }

    for (name, raw) in [("before", &params.before), ("after", &params.after)] {
        let Some(raw) = raw.as_deref().map(str::trim).filter(|v| !v.is_empty()) else {
            continue;
        };
        let stamp = query::parse_filter_date(raw).ok_or_else(|| {
            ApiError::bad_request(
                "invalid_date",
                format!("{name} must be a date written as YYYY-MM-DD"),
            )
        })?;
        if name == "before" {
            filters.before = Some(stamp);
        } else {
            filters.after = Some(stamp);
        }
    }

    Ok(filters)
}

/// Build the engine's request from the query string and the caller's own permissions.
async fn search_request(
    state: &AppState,
    current: &CurrentSession,
    params: &SearchParams,
) -> Result<SearchRequest, ApiError> {
    let query = SearchQuery::parse(params.q.as_deref().unwrap_or_default()).map_err(|_| {
        ApiError::bad_request(
            "query_required",
            "the \"q\" query parameter must carry at least one non-space character",
        )
    })?;
    let sort = parse_sort(params.sort.as_deref())?;
    let providers = readable_providers(state, current).await?;

    Ok(SearchRequest {
        query,
        filters: filters_from(params)?,
        providers,
        organization_id: current.user.organization_id,
        user_id: current.user.id,
        page: params.page.unwrap_or(1).max(1),
        per_page: params
            .per_page
            .unwrap_or(DEFAULT_PER_PAGE)
            .clamp(1, MAX_PER_PAGE),
        sort,
    })
}

/// The providers the caller's own read permissions cover **and** the installation has enabled,
/// in registry order.
async fn readable_providers(
    state: &AppState,
    current: &CurrentSession,
) -> Result<Vec<&'static str>, ApiError> {
    let permissions =
        effective_permissions(state.db().pool(), current.user.id, scope_of(&current.user)).await?;
    let enabled = query::enabled_providers(state.db().pool()).await?;
    Ok(providers::PROVIDERS
        .iter()
        .filter(|spec| permissions.allows(spec.permission) && enabled.iter().any(|k| k == spec.key))
        .map(|spec| spec.key)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_defaults_and_refuses_unknown_values() {
        assert_eq!(parse_sort(None).expect("default"), Sort::Relevance);
        assert_eq!(parse_sort(Some("newest")).expect("newest"), Sort::Newest);
        let error = parse_sort(Some("sideways")).expect_err("refused");
        assert_eq!(error.code(), "unknown_sort");
    }

    /// The nine parameters of a results-screen URL, as the wire spells them.
    fn params(query: &str) -> SearchParams {
        SearchParams {
            q: Some(query.to_owned()),
            page: None,
            per_page: None,
            sort: None,
            types: None,
            site_id: None,
            owner: None,
            language: None,
            status: None,
            updated: None,
            before: None,
            after: None,
            facets: None,
        }
    }

    #[test]
    fn filters_read_every_parameter_they_carry() {
        let mut parameters = params("release");
        parameters.types = Some("Pages, MEDIA ,,".to_owned());
        parameters.site_id = Some("acme".to_owned());
        parameters.owner = Some("me".to_owned());
        parameters.language = Some("TR".to_owned());
        parameters.status = Some("draft".to_owned());
        parameters.updated = Some("week".to_owned());
        parameters.before = Some("2026-09-01".to_owned());
        parameters.after = Some("2026-06-01".to_owned());

        let filters = filters_from(&parameters).expect("the filters must parse");
        assert_eq!(filters.types, vec!["pages".to_owned(), "media".to_owned()]);
        assert_eq!(filters.site.as_deref(), Some("acme"));
        assert!(filters.owner_me);
        assert_eq!(filters.language.as_deref(), Some("TR"));
        assert_eq!(filters.status.as_deref(), Some("draft"));
        assert_eq!(filters.updated, Some(UpdatedRange::Week));
        assert!(filters.before.is_some() && filters.after.is_some());
    }

    #[test]
    fn an_unusable_filter_is_named_not_ignored() {
        let mut bad_owner = params("release");
        bad_owner.owner = Some("ada".to_owned());
        assert_eq!(
            filters_from(&bad_owner).expect_err("refused").code(),
            "invalid_owner"
        );

        let mut bad_date = params("release");
        bad_date.before = Some("soon".to_owned());
        assert_eq!(
            filters_from(&bad_date).expect_err("refused").code(),
            "invalid_date"
        );

        let mut bad_range = params("release");
        bad_range.updated = Some("century".to_owned());
        assert_eq!(
            filters_from(&bad_range).expect_err("refused").code(),
            "unknown_updated_range"
        );
    }

    #[test]
    fn csv_fields_quote_only_what_they_must() {
        assert_eq!(csv_field("Release notes"), "Release notes");
        assert_eq!(csv_field("Notes, and more"), "\"Notes, and more\"");
        assert_eq!(csv_field("He said \"hi\""), "\"He said \"\"hi\"\"\"");
        assert_eq!(csv_field("two\nlines"), "\"two\nlines\"");
        assert_eq!(csv_field(" padded "), "\" padded \"");
    }

    #[test]
    fn a_selection_must_name_rows_and_nothing_else() {
        assert!(parse_selection(None).expect("none").is_none());
        assert!(parse_selection(Some("  ")).expect("blank").is_none());
        let keys = parse_selection(Some("pages:page:1, media:media:2"))
            .expect("two keys")
            .expect("some");
        assert_eq!(keys.len(), 2);
        assert!(keys.contains("pages:page:1"));
        let error = parse_selection(Some("pages:page")).expect_err("refused");
        assert_eq!(error.code(), "invalid_selection");
    }

    #[test]
    fn settings_answers_the_defaults_and_the_catalogue() {
        let answer = SettingsResponse::from_settings(query::SearchSettings {
            weights: query::DEFAULT_WEIGHTS,
            enabled_providers: vec!["pages".to_owned(), "unicorns".to_owned()],
            updated_at: None,
        });
        assert_eq!(answer.weights.title, 6);
        assert_eq!(answer.weights.body, 1);
        assert_eq!(answer.defaults.title, 6);
        // Only known keys are answered, in registry order.
        assert_eq!(answer.enabled_providers, vec!["pages".to_owned()]);
        assert_eq!(answer.available_providers.len(), providers::PROVIDERS.len());
    }
}
