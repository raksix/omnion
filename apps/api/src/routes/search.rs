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
    self, DEFAULT_PER_PAGE, HitPage, MAX_PER_PAGE, Query as SearchQuery, SearchRequest, Sort,
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

/// Query string of `GET /api/v1/search`.
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
            tags: hit.tags,
            updated_at: hit.entity_updated_at,
            score: hit.score,
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
    /// `ready` when the provider has rows, `empty` when it has none yet.
    pub state: &'static str,
}

/// Answer of `GET /api/v1/search/status`.
#[derive(Debug, Serialize)]
pub struct StatusResponse {
    /// One line per registered provider.
    pub providers: Vec<StatusBody>,
    /// Documents across every provider.
    pub documents: i64,
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
    let query = SearchQuery::parse(params.q.as_deref().unwrap_or_default()).map_err(|_| {
        ApiError::bad_request(
            "query_required",
            "the \"q\" query parameter must carry at least one non-space character",
        )
    })?;
    let sort = parse_sort(params.sort.as_deref())?;
    let providers = readable_providers(&state, &current).await?;

    let request = SearchRequest {
        query,
        providers,
        organization_id: current.user.organization_id,
        user_id: current.user.id,
        page: params.page.unwrap_or(1).max(1),
        per_page: params
            .per_page
            .unwrap_or(DEFAULT_PER_PAGE)
            .clamp(1, MAX_PER_PAGE),
        sort,
    };

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
        took_ms: started.elapsed().as_millis() as u64,
    }))
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

/// The providers the caller's own read permissions cover, in registry order.
async fn readable_providers(
    state: &AppState,
    current: &CurrentSession,
) -> Result<Vec<&'static str>, ApiError> {
    let permissions =
        effective_permissions(state.db().pool(), current.user.id, scope_of(&current.user)).await?;
    Ok(providers::PROVIDERS
        .iter()
        .filter(|spec| permissions.allows(spec.permission))
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
}
