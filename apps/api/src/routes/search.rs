//! `/api/v1/search` — the one search box (docs/requests/REQ-002).
//!
//! One request searches every source the caller may read and answers with the hits grouped per
//! source, in the order the palette lists them. Two rules make the surface trustworthy:
//!
//! * **The result set is the caller's.** There is no `search.read` key, because searching is not
//!   a new power: each group's permission (`content.pages.read`, `media.read`, `sites.read` …)
//!   decides whether the caller sees it at all, resolved once through the IAM graph
//!   (`effective_permissions`) and answered as `skipped` with the reason when it is not held.
//!   The SQL of every source is tenant-scoped with the same rule the rest of the panel uses.
//! * **Nothing is invented.** An empty group means "this source has no match", never a
//!   placeholder row; a query with no terms is refused (`400 query_required`) instead of
//!   returning something.
//!
//! The engine itself is `omnion-search` (`crate::routes` lists the surfaces); this module is
//! the HTTP shape around it: parse `q`, validate `sources`, run the permitted sources and
//! serialize.

use axum::Json;
use axum::extract::{Query, State};
use omnion_permissions::effective_permissions;
use omnion_search::query::{DEFAULT_GROUP_LIMIT, MAX_GROUP_LIMIT, Query as SearchQuery};
use omnion_search::{catalogue, sources};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::guards::scope_of;
use crate::state::AppState;

/// Query string of `GET /api/v1/search`.
#[derive(Debug, Deserialize)]
pub struct SearchParams {
    /// The text to search for; required, at least one non-space character.
    pub q: Option<String>,
    /// Hits per group; `1..=20`, default 5.
    pub limit: Option<usize>,
    /// Comma-separated source keys to restrict the search to; default: every readable source.
    pub sources: Option<String>,
}

/// One hit as the API answers it.
#[derive(Debug, Serialize)]
pub struct HitBody {
    /// Identifier inside the hit's own domain.
    pub id: String,
    /// Title the palette shows first.
    pub title: String,
    /// Supporting line (site, slug, content type …).
    pub subtitle: Option<String>,
    /// Panel route a click opens.
    pub url: String,
    /// Ranking score (only comparable inside one response).
    pub score: f64,
    /// When the record last changed, RFC 3339, when the source knows.
    #[serde(with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,
}

impl From<&omnion_search::Hit> for HitBody {
    fn from(hit: &omnion_search::Hit) -> Self {
        Self {
            id: hit.id.clone(),
            title: hit.title.clone(),
            subtitle: hit.subtitle.clone(),
            url: hit.url.clone(),
            score: hit.score,
            updated_at: hit.updated_at,
        }
    }
}

/// One group of hits — one source's answer.
#[derive(Debug, Serialize)]
pub struct GroupBody {
    /// Source key (`pages`).
    pub source: &'static str,
    /// Display title (`Pages`).
    pub title: &'static str,
    /// One line describing the source.
    pub hint: &'static str,
    /// Hits, best first; empty when this source has no match.
    pub hits: Vec<HitBody>,
}

/// A source the caller may not read, named with the reason it is missing.
#[derive(Debug, Serialize)]
pub struct SkippedBody {
    /// Source key.
    pub source: &'static str,
    /// Display title.
    pub title: &'static str,
    /// Why the source is missing (`permission`).
    pub reason: &'static str,
}

/// The whole answer of one search.
#[derive(Debug, Serialize)]
pub struct SearchResponse {
    /// The query as it was understood (trimmed and capped).
    pub query: String,
    /// The parsed terms.
    pub terms: Vec<String>,
    /// One entry per searched source, in palette order.
    pub groups: Vec<GroupBody>,
    /// Sources left out because the caller lacks their read permission.
    pub skipped: Vec<SkippedBody>,
    /// How long the sources took, in milliseconds.
    pub took_ms: u64,
}

/// Search every readable source for `q`.
pub async fn search(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<SearchParams>,
) -> Result<Json<SearchResponse>, ApiError> {
    let query = SearchQuery::parse(params.q.as_deref().unwrap_or_default()).ok_or_else(|| {
        ApiError::bad_request(
            "query_required",
            "the \"q\" query parameter must carry at least one non-space character",
        )
    })?;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_GROUP_LIMIT)
        .clamp(1, MAX_GROUP_LIMIT);
    let requested = requested_sources(params.sources.as_deref())?;

    let pool = state.db().pool();
    let permissions = effective_permissions(pool, current.user.id, scope_of(&current.user)).await?;

    let started = std::time::Instant::now();
    let mut groups = Vec::new();
    let mut skipped = Vec::new();

    for spec in catalogue::SEARCH_SOURCES {
        if let Some(keys) = &requested
            && !keys.iter().any(|key| key == spec.key)
        {
            continue;
        }
        if !permissions.allows(spec.permission) {
            skipped.push(SkippedBody {
                source: spec.key,
                title: spec.title,
                reason: "permission",
            });
            continue;
        }
        let hits = sources::run(spec, pool, current.user.organization_id, &query, limit).await?;
        groups.push(GroupBody {
            source: spec.key,
            title: spec.title,
            hint: spec.hint,
            hits: hits.iter().map(HitBody::from).collect(),
        });
    }

    Ok(Json(SearchResponse {
        query: query.raw().to_owned(),
        terms: query.terms().to_vec(),
        groups,
        skipped,
        took_ms: started.elapsed().as_millis() as u64,
    }))
}

/// Validate the `sources` parameter against the registry; `None` means "every source".
fn requested_sources(raw: Option<&str>) -> Result<Option<Vec<String>>, ApiError> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };

    let mut keys = Vec::new();
    for key in raw.split(',') {
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        if catalogue::source(key).is_none() {
            return Err(ApiError::bad_request(
                "unknown_source",
                format!(
                    "\"{key}\" is not a search source; known sources: {}",
                    catalogue::source_keys().join(", ")
                ),
            ));
        }
        if !keys.iter().any(|known| known == key) {
            keys.push(key.to_owned());
        }
    }

    Ok(if keys.is_empty() { None } else { Some(keys) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_filter_means_every_source() {
        assert_eq!(requested_sources(None).expect("valid"), None);
        assert_eq!(requested_sources(Some("   ")).expect("valid"), None);
        assert_eq!(requested_sources(Some(",")).expect("valid"), None);
    }

    #[test]
    fn a_filter_is_trimmed_and_deduplicated() {
        let keys = requested_sources(Some(" pages , media,pages "))
            .expect("valid")
            .expect("some");
        assert_eq!(keys, vec!["pages", "media"]);
    }

    #[test]
    fn an_unknown_source_is_refused() {
        let error = requested_sources(Some("pages,nope")).expect_err("refused");
        assert_eq!(error.code(), "unknown_source");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }
}
