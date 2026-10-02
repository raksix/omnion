//! `/api/v1/content/*` — the read surface a third-party frontend integrates against (REQ-019, slice 2).
//!
//! Where `public.rs` serves the platform's own renderer anonymously and one page at a time, this
//! is the contract an integrator writes against: token-authenticated, paginated, field-selectable,
//! localized and cache-validatable. The rules that shape it, in order of how much damage getting
//! them wrong would do:
//!
//! 1. **Authentication is token-shaped, and it happens here, per route.** A `ContentToken` extractor
//!    resolves `Authorization: Bearer omn_…` through [`api_tokens::authenticate`], so a caller
//!    with no token gets `401 invalid_token` and a caller whose token expired gets `401
//!    token_expired` — the same verdicts the store already produces, rather than a second opinion
//!    computed in a middleware that could disagree with it.
//!
//! 2. **Scope is checked against the route, and the site scope is a filter, not a gate.** A token
//!    without `media:read` gets `403 insufficient_scope`; a token scoped to one site asking about
//!    another gets an empty list, not a `403` — because an empty list is what a legitimately empty
//!    site looks like, and a `403` would tell a caller that the site exists when it should not
//!    learn that at all.
//!
//! 3. **Only published rows are ever selected.** The queries below name `status = 'published'`
//!    and join the *published* revision rather than the latest one. A draft that is newer than
//!    what is live must not appear, and a caller must not be able to infer that one exists.

use axum::Json;
use axum::extract::{FromRequestParts, MatchedPath, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use omnion_content::api_tokens::{self, AuthFailure, AuthenticatedToken};
use omnion_content::content_read::{
    self, Cursor, DEFAULT_LIMIT, Fields, MAX_FIELDS, MAX_LIMIT, ReadMedia, ReadPage,
    SELECTABLE_MEDIA_FIELDS, SELECTABLE_PAGE_FIELDS, SortKey, SortSource,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::content_meter;
use crate::error::ApiError;
use crate::routes::content_api::auth_failure_response;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Token authentication
// ---------------------------------------------------------------------------------------------

/// A request that carries a valid content API token.
///
/// An extractor rather than a middleware so that the token is a value the handler can ask
/// questions of: a handler that needs `media:read` says so, and a handler that only reads pages
/// never has to know that scope exists.
///
/// **`Clone` because it travels through `http::Extensions`.** The Explorer's dispatcher
/// (REQ-019, slice 3) builds one of these and puts it into the request it sends through the real
/// router, and `Extensions::insert` requires the value to be cloneable. It is a plain wrapper
/// around an `AuthenticatedToken` that is already `Clone`, so the derive costs one shallow copy
/// of a small record and buys the dispatcher the ability to be the real request rather than a
/// parallel implementation of one.
#[derive(Debug, Clone)]
pub struct ContentToken(pub AuthenticatedToken);

impl ContentToken {
    /// Whether the token carries a scope.
    pub fn has_scope(&self, scope: &str) -> bool {
        self.0.token.scopes.iter().any(|granted| granted == scope)
    }

    /// The token's own requests-per-minute budget.
    ///
    /// Read from the row rather than from a constant, because the panel's create dialog offers
    /// two tiers and the whole point of a tier is that the *next request* is decided by the
    /// number the operator chose — not by the one the code was compiled with. The store already
    /// validates the range (`validate_rate_limit`), so a hand-edited row is bounded by the
    /// column's own check.
    pub fn rate_budget(&self) -> i32 {
        self.0.token.rate_limit_per_minute
    }

    /// Refuse a call the token is not scoped for, naming the scope it needed.
    pub fn require_scope(&self, scope: &str) -> Result<(), ApiError> {
        if self.has_scope(scope) {
            return Ok(());
        }
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "insufficient_scope",
            format!("this token does not carry the \"{scope}\" scope"),
        )
        .with_details(json!({ "required_scope": scope })))
    }

    /// The site a request addresses, intersected with the token's own scope.
    ///
    /// `Ok(None)` means **every site of the organization**, and it is reachable only when the
    /// caller named none AND the token is organization-wide. Naming a site the token is not
    /// scoped to yields an empty result rather than a refusal, and the caller cannot tell a site
    /// that does not exist from a site that holds nothing they may read.
    ///
    /// The out-of-scope case returns [`SITE_OUT_OF_SCOPE`] — a **sentinel the SQL never matches**,
    /// not `Uuid::nil()`. The earlier version returned the nil UUID and the call sites stripped it
    /// back to `None` with `.filter(|site| *site != Uuid::nil())`, which is exactly backwards: the
    /// strip turned "match nothing" into "match every site", so a single-site token asking about
    /// another site read **that other site's published pages** with a `200`. A sentinel is only
    /// safe if nothing in the chain can remove it, and that chain is three call sites deep — so
    /// the value carried here is a real UUID that no site can hold, and the SQL's
    /// `p.site_id = $n` does the rest.
    pub fn site_filter(&self, requested: Option<Uuid>) -> Result<Option<Uuid>, ApiError> {
        match (self.0.token.site_id, requested) {
            (None, _) => Ok(requested),
            (Some(scoped), None) => Ok(Some(scoped)),
            (Some(scoped), Some(asked)) if asked == scoped => Ok(Some(asked)),
            (Some(_), Some(_)) => Ok(Some(SITE_OUT_OF_SCOPE)),
        }
    }
}

/// A site id no site can hold, used to answer "out of scope" as an empty result.
///
/// Deliberately *not* [`Uuid::nil`]: nil was the earlier sentinel and two call sites removed it
/// before the query ran, which turned the refusal into a full-table read. A value that is merely
/// unlikely can be stripped by accident; this one cannot be produced by any insert, and the test
/// that pins it checks the value the **SQL receives** rather than the value this function returns.
pub const SITE_OUT_OF_SCOPE: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0001);

impl FromRequestParts<AppState> for ContentToken {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // The matched route, read BEFORE anything is spent. This is what the usage bucket and the
        // `X-RateLimit-*` headers name, and it is the only path string on this surface that does
        // not grow with the caller's data: `/content/pages/{slug}` is one row however many slugs
        // were read, while `uri.path()` would create a row per slug and make the usage tab
        // unusable for the one integration it exists to describe. Read here rather than in a
        // response layer because a layer that is mounted on the router cannot see the template.
        //
        // **The `/api/v1` prefix is stripped, and it has to be.** `MatchedPath` is absolute from
        // the application's root, so the row was keyed `/api/v1/content/pages` while the OpenAPI
        // document, the panel's copy button and the explorer's own snippet all say
        // `/content/pages`. One endpoint under two spellings means two rows in the usage table
        // and a caller reading the chart cannot tell which of them is the endpoint they call —
        // and the bug is invisible in the response, because the response never mentions either.
        // The rule is a *named constant* rather than a slice-and-hope: the prefix belongs to the
        // v1 tree's mount point, and the test below is what says the two agree.
        let route = parts
            .extensions
            .get::<MatchedPath>()
            .map(|path| surface_route(path.as_str()));

        // **A pre-authenticated token in the extensions wins, and this is the only thing that
        // check is for.** The Explorer's dispatcher (REQ-019, slice 3) builds the `ContentToken`
        // itself, from a row it loaded under a panel session, and puts it here — because a
        // token's plaintext is stored as a digest and cannot be replayed as a header on a second
        // request. Without this early return the extractor reads the placeholder header the
        // dispatcher sent, refuses it, and every call answers `401 invalid_token` — a screen that
        // looks wired up and dispatches nothing.
        //
        // It is deliberately a **get**, not an insert: an extractor that wrote its own value back
        // would make "the token came from the header" and "the token came from the row"
        // indistinguishable downstream, and this branch is the only place that knows which.
        if let Some(existing) = parts.extensions.get::<ContentToken>() {
            return Ok(existing.clone());
        }

        let raw = bearer_token(&parts.headers)
            .map_err(|error| error)
            .or_else(|primary| {
                // `?token=` is accepted for local experiments only, because a browser cannot set
                // an Authorization header on an <img> or a plain navigation. It is logged as a
                // warning at the call site rather than silently accepted, because a token in a
                // query string lands in access logs, browser history and Referer headers — the
                // reason it is documented as a local tool and not a feature.
                let from_query = parts.uri.query().and_then(|query| {
                    query
                        .split('&')
                        .filter_map(|pair| pair.split_once('='))
                        .find(|(key, _)| *key == "token")
                        .map(|(_, value)| percent_decode(value))
                });
                match from_query {
                    Some(value) => {
                        tracing::warn!(
                            "a content API request carried its token in the query string; \
                             use the Authorization header outside local experiments"
                        );
                        Ok(value)
                    }
                    None => Err(primary),
                }
            })?;

        // The `Result<_, AuthFailure>` is the same verdict slice 1 hands out: invalid / expired /
        // revoked are three different problems and each has its own code, so a caller whose
        // credential merely expired is not told to rotate a token that was fine.
        let authenticated =
            match api_tokens::authenticate_any_organization(state.db().pool(), &raw).await {
                Ok(authenticated) => authenticated,
                Err(failure) => return Err(auth_failure_response(&failure)),
            };

        let token = Self(authenticated);

        // The budget, and then the counter, in that order and both before the handler runs.
        //
        // **Why here and not in a middleware:** the platform's own limiter runs outside the
        // permission guards, and that is right for it — it keys on address and user. A content
        // token is neither, and a second limiter with its own key would be a second answer to
        // "has this client spent its budget". So this *is* the content surface's limiter, it
        // spends the same round trip the meter spends, and the two cannot disagree because the
        // refusal is recorded by the same call that incremented the counter.
        let route = route.unwrap_or_else(|| "unknown".to_owned());
        let verdict = content_meter::spend(state, token.0.token.id, token.rate_budget(), &route).await;

        // A successful call is the moment `last_used_at` means something, and it is written here
        // rather than in a background task: the Tokens tab's "last used" column would otherwise
        // lag by however long the flush interval is, and a person debugging an integration is
        // looking at that column while the integration is running.
        let _ = sqlx::query(
            "update api_tokens set last_used_at = now(), updated_at = now() where id = $1",
        )
        .bind(token.0.token.id)
        .execute(state.db().pool())
        .await;

        // The refusal is the extracter's to answer, and it is answered *before* the handler runs
        // so a throttled caller never reaches a query at all — a rate limit that still ran the
        // handler is a rate limit that only spent the database's time.
        if verdict.limited {
            return Err(rate_limited(&verdict, &route));
        }

        parts.extensions.insert(ContentCall {
            route,
            rate: verdict,
        });

        Ok(token)
    }
}

/// The `429` a token's exhausted budget answers with.
///
/// **`Retry-After` is the rest of the minute the caller is inside, not a constant.** A constant
/// `Retry-After: 5` on a per-minute budget is the classic limiter bug: a client that honours it
/// retries 0.8 s before the window rolls, is refused again, and its retry loop becomes the load
/// the limit exists to shed. The wait is derived from the same bucket index the counter used, so
/// the two cannot disagree about which minute is open.
///
/// The `X-RateLimit-*` headers are stamped here rather than in a response layer, because a layer
/// would have to re-derive the tier from a database row or trust a request header — and a header
/// is a value the caller sets.
pub fn rate_limited(verdict: &content_meter::Verdict, route: &str) -> ApiError {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let retry_after = content_meter::retry_after_seconds(now);
    ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        format!(
            "this token's budget of {}/minute is spent; the window resets in {retry_after}s",
            verdict.limit
        ),
    )
    .with_details(json!({
        "limit": verdict.limit,
        "count": verdict.count,
        "remaining": verdict.remaining(),
        "retry_after": retry_after,
        "endpoint": route,
    }))
    .with_retry_after(retry_after as i64)
}

/// The mount point the whole v1 tree hangs from, and the prefix every surface path carries.
pub const API_PREFIX: &str = "/api/v1";

/// The route a caller calls, with the API's own mount point removed.
///
/// A prefix that is right today and wrong after a version bump is a silent corruption of the
/// usage table's keys: the rows keep the old prefix, the flush keeps writing them, and the chart
/// quietly splits one endpoint in two. So the rule is one function, the constant it strips is
/// named, and the test at the bottom of this file fails if the mount point ever stops matching.
#[must_use]
pub fn surface_route(matched: &str) -> String {
    match matched.strip_prefix(API_PREFIX) {
        Some(route) if route.starts_with('/') => route.to_owned(),
        // Either the router was mounted somewhere else (a test, a sub-application) or the path is
        // exactly the prefix. Both are answered as-is rather than guessed at, so the row a reader
        // sees is the row the router really matched.
        _ => matched.to_owned(),
    }
}

/// What one content request spent, handed to the handler by the extractor.
///
/// Inserted into the request extensions so a handler can stamp the *actual* verdict on its
/// response. A middleware could do that too, but it would have to guess the token's tier (a
/// database read per request) or read the header the extractor already decided — and the second
/// of those is a response built from a request header, which is a value a caller can set.
#[derive(Debug, Clone)]
pub struct ContentCall {
    /// The matched route this call spent against.
    pub route: String,
    /// The limiter's verdict for this call.
    pub rate: content_meter::Verdict,
}

/// Read the [`ContentCall`] the extractor left, if it got that far.
///
/// An extractor rather than a field on [`ContentToken`] because Axum runs extractors
/// **in declaration order** and only the ones a handler lists: a handler that took
/// `token: ContentToken, call: ContentCall` would work, but six handlers repeating that pair is
/// six chances to leave the second one out and answer a response with no rate-limit headers. This
/// way forgetting it is not expressible.
impl FromRequestParts<AppState> for ContentCall {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<ContentCall>()
            .cloned()
            .unwrap_or(ContentCall {
                route: "unknown".to_owned(),
                rate: content_meter::Verdict {
                    count: 0,
                    limit: 0,
                    limited: false,
                    authoritative: false,
                },
            }))
    }
}

/// The organization a content request is being authenticated against.
///
/// A content request has no session — that is the whole point of the surface — so there is no
/// `CurrentSession` to read an organization from, and the honest answer is that the *caller*
/// does not know its own organization until the token says so.
///
/// Which makes the store's `authenticate(pool, organization_id, presented)` signature a problem:
/// it compares the row's organization against the one it is handed, which is right for a panel
/// route (the session knows its tenant) and wrong here. The token's prefix is unique per
/// installation, so the correct check for an unauthenticated surface is **none at all**: there is
/// no second tenant to confuse it with, and the row that comes back is the authority for every
/// scope, site and organization decision downstream.
///
/// Rather than change the store's signature in a slice that is supposed to be about reading
/// content, the comparison is satisfied by looking the prefix up first and passing the row's own
/// organization back. That is a deliberate no-op rather than a shortcut, and the reason is
/// recorded in `authenticate` itself so the next reader does not "fix" it into a real filter:
/// cross-tenant isolation on this surface comes from the token's `site_id` and from the
/// `scopes` on the row, not from a tenant the caller cannot name.

/// Read the bearer credential, or explain what is missing.
fn bearer_token(headers: &HeaderMap) -> Result<String, ApiError> {
    let header = headers
        .get(header::AUTHORIZATION)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "invalid_token",
                "this endpoint needs a content API token",
            )
        })?
        .to_str()
        .map_err(|_| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "invalid_token",
                "the Authorization header is not readable text",
            )
        })?;
    let value = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "invalid_token",
                "send the token as `Authorization: Bearer omn_…`",
            )
        })?
        .trim();
    if value.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "invalid_token",
            "the Authorization header carries no token",
        ));
    }
    Ok(value.to_string())
}

/// Minimal percent-decoding for the `?token=` fallback.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------------------------
// Query shapes
// ---------------------------------------------------------------------------------------------

/// Everything a content list call may ask for.
///
/// One struct for pages and posts because they are the same query with a different `page_type`
/// filter: two structs would drift, and the drift would show up as "the pages endpoint accepts a
/// parameter the posts endpoint silently ignores".
#[derive(Debug, Default, Deserialize)]
pub struct ContentListQuery {
    /// Site to read: a uuid. Omitted reads the token's own scope.
    #[serde(default)]
    pub site: Option<Uuid>,
    /// Language tag for the translation overlay.
    #[serde(default)]
    pub locale: Option<String>,
    /// Content type filter, e.g. `page` or `post`.
    #[serde(default)]
    pub r#type: Option<String>,
    /// Slug prefix filter, exact match on the address.
    #[serde(default)]
    pub slug: Option<String>,
    /// Row limit, 1–100.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Opaque keyset cursor from a previous page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Comma-separated field selection.
    #[serde(default)]
    pub fields: Option<String>,
    /// Sort order.
    #[serde(default)]
    pub sort: Option<String>,
    /// Only rows changed at or after this RFC 3339 instant.
    #[serde(default)]
    pub updated_since: Option<String>,
}

/// Media list parameters: the same shape minus the content-type and locale filters, which have no
/// meaning for a file.
#[derive(Debug, Default, Deserialize)]
pub struct MediaListQuery {
    /// Site to read.
    #[serde(default)]
    pub site: Option<Uuid>,
    /// MIME type filter, e.g. `image/png`.
    #[serde(default)]
    pub mime: Option<String>,
    /// Filename filter, exact match.
    #[serde(default)]
    pub filename: Option<String>,
    /// Row limit, 1–100.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Opaque keyset cursor.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Comma-separated field selection.
    #[serde(default)]
    pub fields: Option<String>,
    /// Sort order.
    #[serde(default)]
    pub sort: Option<String>,
    /// Only rows changed at or after this RFC 3339 instant.
    #[serde(default)]
    pub updated_since: Option<String>,
}

/// A validated list request: the caller's wishes, already checked.
struct ListRequest {
    site_id: Option<Uuid>,
    locale: Option<String>,
    limit: i64,
    sort: SortKey,
    /// The **qualified** SQL expression `sort` resolved to, e.g. `p.updated_at` or `r.title`.
    ///
    /// The sort and the column it names are resolved together, by
    /// [`SortKey::expression`], and this field is the single copy the ORDER BY, the keyset
    /// predicate and the cursor all read. That is the point: the three used to name the sort
    /// separately, so a sort could order by one column and page by another, and nothing about
    /// the request would disagree.
    column: &'static str,
    fields: Fields,
    cursor: Option<Cursor>,
    updated_since: Option<OffsetDateTime>,
    page_type: Option<String>,
    slug: Option<String>,
    mime: Option<String>,
    filename: Option<String>,
}

impl ListRequest {
    /// Validate a page/post list call.
    fn for_pages(token: &ContentToken, query: &ContentListQuery) -> Result<Self, ApiError> {
        // No `.filter(...)` here, and that omission is the fix: the filter was there to strip an
        // out-of-scope sentinel and it stripped the *wrong* meaning. `site_filter` now returns
        // `SITE_OUT_OF_SCOPE` for that case, so it has to reach the SQL untouched.
        let site_id = token.site_filter(query.site)?;
        let sort = page_sort_of(query.sort.as_deref())?;
        Ok(Self {
            site_id,
            locale: query
                .locale
                .as_deref()
                .map(content_read::parse_locale)
                .transpose()
                .map_err(|error| bad_parameter("locale", error.to_string()))?,
            limit: limit_of(query.limit)?,
            sort,
            column: sort
                .expression(SortSource::Pages)
                .expect("a page sort is checked before it gets here"),
            fields: Fields::parse(query.fields.as_deref(), &SELECTABLE_PAGE_FIELDS)
                .map_err(|error| bad_parameter("fields", error.to_string()))?,
            cursor: query
                .cursor
                .as_deref()
                .map(content_read::decode_cursor)
                .transpose()
                .map_err(|error| bad_parameter("cursor", error.to_string()))?,
            updated_since: query
                .updated_since
                .as_deref()
                .map(content_read::parse_updated_since)
                .transpose()
                .map_err(|error| bad_parameter("updated_since", error.to_string()))?,
            page_type: query.r#type.clone(),
            slug: query.slug.clone(),
            mime: None,
            filename: None,
        })
    }

    /// Validate a media list call.
    fn for_media(token: &ContentToken, query: &MediaListQuery) -> Result<Self, ApiError> {
        // A file has no title, so this relation supports two of the three sorts. The refusal is a
        // `400` naming `sort` and listing the two — previously the third sort was accepted here
        // and became `m.title` in the query, which is a `500` on a documented parameter.
        let sort = media_sort_of(query.sort.as_deref())?;
        Ok(Self {
            site_id: token.site_filter(query.site)?,
            locale: None,
            limit: limit_of(query.limit)?,
            sort,
            column: sort
                .expression(SortSource::Media)
                .expect("a media sort is checked before it gets here"),
            fields: Fields::parse(query.fields.as_deref(), &SELECTABLE_MEDIA_FIELDS)
                .map_err(|error| bad_parameter("fields", error.to_string()))?,
            cursor: query
                .cursor
                .as_deref()
                .map(content_read::decode_cursor)
                .transpose()
                .map_err(|error| bad_parameter("cursor", error.to_string()))?,
            updated_since: query
                .updated_since
                .as_deref()
                .map(content_read::parse_updated_since)
                .transpose()
                .map_err(|error| bad_parameter("updated_since", error.to_string()))?,
            page_type: None,
            slug: None,
            mime: query.mime.clone(),
            filename: query.filename.clone(),
        })
    }

    /// How many rows the listing query must actually fetch.
    ///
    /// **`limit + 1`, not `limit`.** The extra row is the over-fetch the `next_cursor` decision
    /// is made from, and it was missing: `fetch_limit` returned `self.limit`, so the query
    /// returned exactly the page, `fetched > request.limit` was never true, and **every list
    /// response claimed it was the last page**. A client paginating five rows received two rows
    /// and `next_cursor: null` and concluded the site had two published pages.
    ///
    /// The tell is that the symptom looks like a *data* problem and is a *query* problem — and no
    /// assertion on any single page can see it. Only a walk that expects more pages than the
    /// first can, which is why the cursor test is a loop and not a `count == 2` check.
    fn fetch_limit(&self) -> i64 {
        self.limit + 1
    }
}

/// Validate a `limit`, naming the ceiling when it is the ceiling that was missed.
fn limit_of(raw: Option<i64>) -> Result<i64, ApiError> {
    match raw {
        None => Ok(DEFAULT_LIMIT),
        Some(value) if value < 1 => Err(bad_parameter("limit", "limit must be at least 1".into())),
        Some(value) if value > MAX_LIMIT => Err(bad_parameter(
            "limit",
            format!("limit must be at most {MAX_LIMIT}"),
        )),
        Some(value) => Ok(value),
    }
}

/// Validate a `sort` for the pages/posts relation, defaulting to the one the docs call the
/// contract.
fn page_sort_of(raw: Option<&str>) -> Result<SortKey, ApiError> {
    page_sort(raw.unwrap_or("updated_at"))
}

/// Validate a `sort` for the media relation.
fn media_sort_of(raw: Option<&str>) -> Result<SortKey, ApiError> {
    media_sort(raw.unwrap_or("updated_at"))
}

/// `sort` for a page list, as a `400` naming the parameter.
fn page_sort(raw: &str) -> Result<SortKey, ApiError> {
    SortKey::parse_for(raw, SortSource::Pages)
        .map_err(|error| bad_parameter("sort", error.to_string()))
}

/// `sort` for a media list, as a `400` naming the parameter.
///
/// A separate function from [`page_sort`] because the *vocabulary* differs: a file has no title,
/// so a caller who copy-pasted the pages' `?sort=title` gets told what media actually sorts by
/// rather than a `500` about a column named `m.title`.
fn media_sort(raw: &str) -> Result<SortKey, ApiError> {
    SortKey::parse_for(raw, SortSource::Media)
        .map_err(|error| bad_parameter("sort", error.to_string()))
}

/// A `400 invalid_parameter` that names the parameter in both the message and the details.
///
/// The API layer already puts `details.field` where the panel reads it; filling it here means the
/// Explorer can highlight the offending input without a second parse of the message text.
fn bad_parameter(field: &str, message: String) -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "invalid_parameter",
        message.clone(),
    )
    .with_details(json!({ "field": field, "message": message }))
}

// ---------------------------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------------------------

/// The envelope every list answers with.
///
/// `count` is the number of items in *this* page, not the total: a total would need a second
/// count query on every request, and the total changes between the count and the rows anyway.
/// `next_cursor` is the honest signal that more exist.
#[derive(Debug, Serialize)]
pub struct ListEnvelope {
    /// The rows.
    pub items: Vec<Value>,
    /// Cursor for the next page, or `null` at the end.
    pub next_cursor: Option<String>,
    /// Items in this page.
    pub count: usize,
}

/// A read response plus the cache headers that make it revalidatable.
///
/// Public because it is a handler's return type, and Axum requires a route's `IntoResponse`
/// output to be nameable from the module that registers the route. Its *fields* stay private:
/// the router needs to know the type exists, not to build one.
pub struct ReadResponse {
    /// The envelope, serialized by `IntoResponse`.
    body: Value,
    /// Weak validator over the rows.
    etag: String,
    /// Always `200` today; a `304` path is slice 3's rate-limit work, not this slice's.
    status: StatusCode,
    /// What the request spent, when the extractor got far enough to know.
    ///
    /// `None` is a real case, not a forgotten assignment: `MatchedPath` is absent when a
    /// handler is called directly (which every unit test does), and a response that stamped
    /// `X-RateLimit-Remaining: 0` from a missing verdict would be claiming a measurement the
    /// platform never made. **The headers are therefore omitted entirely rather than guessed.**
    spent: Option<content_meter::Verdict>,
}

impl IntoResponse for ReadResponse {
    fn into_response(self) -> Response {
        let etag = HeaderValue::from_str(&self.etag)
            .unwrap_or_else(|_| HeaderValue::from_static("\"w/unknown\""));
        // `must-revalidate` with `max-age=0` is the point of the ETag: a CDN may store the body
        // and revalidate it cheaply, and a stale body is never served without a round trip.
        let mut response = (self.status, Json(self.body)).into_response();
        let headers = response.headers_mut();
        headers.insert(header::ETAG, etag);
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=0, must-revalidate"),
        );
        // The rate-limit headers are the surface's contract with a client that wants to back off
        // politely: it reads `remaining`, and when it reaches 0 it waits for `Retry-After`. Both
        // come from the extractor's own verdict, so the number a client trusts is the number the
        // counter holds.
        if let Some(verdict) = self.spent {
            if verdict.authoritative {
                stamp_rate_limit(headers, &verdict);
            }
        }
        response
    }
}

/// Stamp `X-RateLimit-Limit` / `-Remaining` on a response.
///
/// **Only when the verdict was a measurement.** An unreachable counter answers `authoritative:
/// false` and the headers are then *absent* rather than zero, because a client that reads
/// `remaining: 0` from a counter nobody could read will back off a token that is not being
/// limited — the meter failing open would throttle the caller by accident.
fn stamp_rate_limit(headers: &mut axum::http::HeaderMap, verdict: &content_meter::Verdict) {
    // `HeaderName::from_static` rather than `&str`: a `&str` key makes the compiler infer a
    // lifetime, and a closure that captured one `'static` literal and another call site that
    // passed a computed string are two different types — the closure signature is the only place
    // that can say both are the same name.
    fn set(headers: &mut axum::http::HeaderMap, name: axum::http::HeaderName, value: String) {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
    set(
        headers,
        axum::http::HeaderName::from_static("x-ratelimit-limit"),
        verdict.limit.to_string(),
    );
    if let Some(remaining) = verdict.remaining() {
        set(
            headers,
            axum::http::HeaderName::from_static("x-ratelimit-remaining"),
            remaining.to_string(),
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/content/pages` — published pages, newest change first.
pub async fn list_pages(
    State(state): State<AppState>,
    token: ContentToken,
    call: ContentCall,
    Query(query): Query<ContentListQuery>,
) -> Result<ReadResponse, ApiError> {
    token.require_scope("content:read")?;
    list_pages_of_type(state, token, call, query, None).await
}

/// `GET /api/v1/content/posts` — the same rows filtered to `page_type = 'post'`.
///
/// Marked experimental in the OpenAPI document: `post` is an alias over a page type until the
/// blog module (REQ-116) ships its own tables, and an integrator reading the document should see
/// that before they build a schema around it.
pub async fn list_posts(
    State(state): State<AppState>,
    token: ContentToken,
    call: ContentCall,
    Query(query): Query<ContentListQuery>,
) -> Result<ReadResponse, ApiError> {
    token.require_scope("content:read")?;
    list_pages_of_type(state, token, call, query, Some("post")).await
}

async fn list_pages_of_type(
    state: AppState,
    token: ContentToken,
    call: ContentCall,
    query: ContentListQuery,
    fixed_type: Option<&'static str>,
) -> Result<ReadResponse, ApiError> {
    let request = ListRequest::for_pages(&token, &query)?;
    let mut sql = String::from(
        "select p.id, p.slug, p.page_type, p.site_id, p.updated_at, p.created_at, \
                p.published_revision_id, r.title, r.body, r.summary, r.revision_no \
         from pages p \
         join page_revisions r on r.id = p.published_revision_id \
         where p.status = 'published'",
    );
    // Predicates are appended in a fixed order and every value is bound, so the clause count
    // depends on which filters are present — never on their values.
    let mut index = 1;
    if let Some(site_id) = request.site_id {
        sql.push_str(&format!(" and p.site_id = ${index}"));
        index += 1;
    }
    if let Some(page_type) = fixed_type.or(request.page_type.as_deref()) {
        sql.push_str(&format!(" and p.page_type = ${index}"));
        index += 1;
    }
    if let Some(slug) = &request.slug {
        sql.push_str(&format!(" and p.slug = ${index}"));
        index += 1;
    }
    if let Some(since) = request.updated_since {
        sql.push_str(&format!(" and p.updated_at > ${index}"));
        index += 1;
    }
    if let Some(cursor) = &request.cursor {
        // The keyset predicate, in the sort's own direction. `or` rather than `and` on the id is
        // what makes rows with an identical sort value come out in a stable, non-repeating
        // order — without the tiebreak, two pages can return the same row twice.
        //
        // `request.column` is the *qualified expression* the sort resolved to, not a name this
        // file assembles. It used to be `format!("p.{}", sort.column())`, which is how a
        // documented `?sort=title` became `p.title` — a column that has never existed on `pages`,
        // because the title is the revision's — and every call with it was a 500.
        sql.push_str(&format!(
            " and ({column} < ${index} or ({column} = ${index} and p.id < ${next}))",
            column = request.column,
            next = index + 1
        ));
        index += 2;
    }
    sql.push_str(&format!(
        " order by {column} desc, p.id desc limit ${index}",
        column = request.column
    ));

    let mut statement = sqlx::query(&sql);
    if let Some(site_id) = request.site_id {
        statement = statement.bind(site_id);
    }
    if let Some(page_type) = fixed_type.or(request.page_type.as_deref()) {
        statement = statement.bind(page_type);
    }
    if let Some(slug) = &request.slug {
        statement = statement.bind(slug);
    }
    if let Some(since) = request.updated_since {
        statement = statement.bind(since);
    }
    if let Some(cursor) = &request.cursor {
        // The cursor carries the sort *value* as a string, because a title is a string and a
        // timestamp is not. It is parsed back to the column's type here, and a cursor that
        // cannot be parsed as the requested sort is refused rather than compared: a caller that
        // changes `sort` mid-walk would otherwise get a page from the wrong ordering that looks
        // like a legitimate one.
        //
        // **It is bound as the parsed `OffsetDateTime`, never as the string.** It used to be
        // `map(|parsed| parsed.to_string())` — parsed correctly, then handed to PostgreSQL as
        // `text`. `timestamptz < text` has no operator, so PostgreSQL answered
        // `500 operator does not exist: timestamp with time zone < text` and the walk died on
        // page two. Parsing and then re-stringifying threw away the type it had just recovered.
        //
        // The two sorts bind **two different types**, so they cannot be one `match` value: sqlx's
        // `Query` is typed by the binds it has already seen, and a `String` and an
        // `OffsetDateTime` are not the same statement. Each arm binds inside its own `if`, which
        // is also the only way this stays a *compile* error rather than a runtime cast.
        if matches!(request.sort, SortKey::Title) {
            statement = statement.bind(cursor.value.clone());
        } else {
            let value = content_read::cursor_instant(&cursor.value)
                .map_err(|error| bad_parameter("cursor", error.to_string()))?;
            statement = statement.bind(value);
        }
        statement = statement.bind(cursor.id);
    }
    let rows = statement
        .bind(request.fetch_limit())
        .fetch_all(state.db().pool())
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                format!("listing pages: {error}"),
            )
        })?;

    // The over-fetch happens in SQL only. **The row that exists to decide `next_cursor` must not
    // be rendered**: the two list handlers built `items` from `rows` (which includes it) while
    // `count` and the cursor were taken from `visible` (which does not), so a `limit=2` call
    // answered `count: 2` with **three** items, and a caller honouring `count` skipped one of
    // them and then received it again on the next page. The walk in
    // `content_read_surface` is what caught it: five rows at two per page produced seven slugs
    // across four pages.
    //
    // `visible` is the whole answer now — items, count, cursor and ETag all come from it, so
    // "what the caller was told" and "what the caller is sent" cannot disagree.
    let visible = &rows[..rows.len().min(request.limit as usize)];
    let fetched = rows.len() as i64;
    let mut items: Vec<Value> = Vec::with_capacity(visible.len());
    let mut stamps: Vec<(String, Uuid)> = Vec::with_capacity(visible.len());
    for row in visible {
        let page = ReadPage {
            id: row.get("id"),
            slug: row.get("slug"),
            page_type: row.get("page_type"),
            site_id: row.get("site_id"),
            updated_at: row.get("updated_at"),
            created_at: row.get("created_at"),
            published_revision_id: row.get("published_revision_id"),
            title: row.get("title"),
            body: row.get("body"),
            summary: row.get("summary"),
            revision: row.get("revision_no"),
        };
        stamps.push((content_read::stamp(page.updated_at), page.id));
        items.push(page.to_value(&request.fields, request.locale.as_deref(), &[]));
    }
    let next_cursor = if content_read::continues(fetched, request.limit) {
        visible.last().map(|row| {
            content_read::encode_cursor(&Cursor::new(
                sort_value_of(row, request.sort, SortSource::Pages),
                row.get("id"),
            ))
        })
    } else {
        None
    };

    let etag = content_read::list_etag(&stamps);
    Ok(ReadResponse {
        status: StatusCode::OK,
        spent: Some(call.rate),
        etag,
        body: json!({
            "items": items,
            "next_cursor": next_cursor,
            "count": items.len(),
        }),
    })
}

/// The value a cursor stores for a row, in the sort the request asked for.
///
/// **Both timestamp ends go through [`content_read::stamp`].** The pages half of this function
/// formatted RFC 3339 while the media half used `OffsetDateTime::to_string()` — the driver's
/// `Display`, which writes `2026-09-30 21:53:21.509904 +00:00:00` — and the media *reader* parsed
/// RFC 3339. Each half was internally consistent; the walk crossed a format boundary. It has now
/// been fixed once in each direction across two ticks, which is the proof that a format has to be
/// a function both halves call, not a convention each half follows.
///
/// The row is read by the alias [`SortKey::read_as`] names, not by a name derived here: the
/// media default sorts on `coalesce(updated_at, created_at)`, and that value is not `updated_at`.
fn sort_value_of(row: &sqlx::postgres::PgRow, sort: SortKey, source: SortSource) -> String {
    let alias = sort
        .read_as(source)
        .expect("a sort that reached a query can be read back");
    match sort {
        SortKey::Title => row.get::<String, _>(alias),
        SortKey::CreatedAt | SortKey::UpdatedAt => {
            content_read::stamp(row.get::<OffsetDateTime, _>(alias))
        }
    }
}

/// `GET /api/v1/content/pages/{slug}` — one published page plus its alternates.
pub async fn get_page(
    State(state): State<AppState>,
    token: ContentToken,
    call: ContentCall,
    Path(slug): Path<String>,
    Query(query): Query<ContentListQuery>,
) -> Result<ReadResponse, ApiError> {
    token.require_scope("content:read")?;
    let request = ListRequest::for_pages(&token, &query)?;
    let site = request.site_id;
    let row = sqlx::query(
        "select p.id, p.slug, p.page_type, p.site_id, p.updated_at, p.created_at, \
                p.published_revision_id, r.title, r.body, r.summary, r.revision_no \
         from pages p \
         join page_revisions r on r.id = p.published_revision_id \
         where p.status = 'published' and p.slug = $1 and ($2::uuid is null or p.site_id = $2) \
         limit 1",
    )
    .bind(&slug)
    .bind(site)
    .fetch_optional(state.db().pool())
    .await
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("reading a page: {error}"),
        )
    })?
    // A slug that does not exist and a slug that exists but is not published answer the same
    // `404`, for the same reason the public surface does: a `403`-shaped difference here would
    // let a caller enumerate unpublished content by watching the status code.
    .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such page"))?;

    let page = ReadPage {
        id: row.get("id"),
        slug: row.get("slug"),
        page_type: row.get("page_type"),
        site_id: row.get("site_id"),
        updated_at: row.get("updated_at"),
        created_at: row.get("created_at"),
        published_revision_id: row.get("published_revision_id"),
        title: row.get("title"),
        body: row.get("body"),
        summary: row.get("summary"),
        revision: row.get("revision_no"),
    };
    let items = vec![page.to_value(&request.fields, request.locale.as_deref(), &[])];
    let etag = content_read::list_etag(&[(content_read::stamp(page.updated_at), page.id)]);
    Ok(ReadResponse {
        status: StatusCode::OK,
        spent: Some(call.rate),
        etag,
        body: json!({ "item": items[0].clone() }),
    })
}

/// `GET /api/v1/content/posts/{slug}` — one post.
pub async fn get_post(
    State(state): State<AppState>,
    token: ContentToken,
    call: ContentCall,
    Path(slug): Path<String>,
    Query(query): Query<ContentListQuery>,
) -> Result<ReadResponse, ApiError> {
    token.require_scope("content:read")?;
    let mut query = query;
    // The post surface is the page surface with a type filter, applied here rather than in a
    // second handler so the two cannot disagree about which rows a post is.
    let site = query.site;
    query.r#type = Some("post".to_string());
    let request = ListRequest::for_pages(&token, &query)?;
    let site_id = request.site_id.or(site);
    let row = sqlx::query(
        "select p.id, p.slug, p.page_type, p.site_id, p.updated_at, p.created_at, \
                p.published_revision_id, r.title, r.body, r.summary, r.revision_no \
         from pages p \
         join page_revisions r on r.id = p.published_revision_id \
         where p.status = 'published' and p.page_type = 'post' and p.slug = $1 \
           and ($2::uuid is null or p.site_id = $2) \
         limit 1",
    )
    .bind(&slug)
    .bind(site_id)
    .fetch_optional(state.db().pool())
    .await
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("reading a post: {error}"),
        )
    })?
    .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such post"))?;

    let page = ReadPage {
        id: row.get("id"),
        slug: row.get("slug"),
        page_type: row.get("page_type"),
        site_id: row.get("site_id"),
        updated_at: row.get("updated_at"),
        created_at: row.get("created_at"),
        published_revision_id: row.get("published_revision_id"),
        title: row.get("title"),
        body: row.get("body"),
        summary: row.get("summary"),
        revision: row.get("revision_no"),
    };
    let etag = content_read::list_etag(&[(content_read::stamp(page.updated_at), page.id)]);
    Ok(ReadResponse {
        status: StatusCode::OK,
        spent: Some(call.rate),
        etag,
        body: json!({ "item": page.to_value(&request.fields, request.locale.as_deref(), &[]) }),
    })
}

/// `GET /api/v1/content/media` — media metadata, never bytes.
pub async fn list_media(
    State(state): State<AppState>,
    token: ContentToken,
    call: ContentCall,
    Query(query): Query<MediaListQuery>,
) -> Result<ReadResponse, ApiError> {
    token.require_scope("media:read")?;
    let request = ListRequest::for_media(&token, &query)?;

    // `media` carries its own `width`/`height` (migration 0025) and an `updated_at` that is
    // NULL for a row written before that column existed — hence the coalesce, which is also the
    // value the cursor and the ETag are built from, so a media list is stable across the upgrade.
    //
    // **The coalesce is selected twice under two names.** `cursor_stamp` is what the sort resolves
    // to, so the cursor is built from the very expression the ORDER BY and the keyset predicate
    // compare; `updated_at` stays for the item's own `updated_at` field, which is a *different*
    // question — "when was this file itself changed", not "where does this list sort me". Reading
    // one out of the other is how a list can be ordered by `created_at` while paginating by
    // `updated_at`, and neither query would say so.
    let mut sql = String::from(
        "select m.id, m.site_id, m.filename, m.content_type, m.size_bytes, m.storage_key, \
                coalesce(m.updated_at, m.created_at) as updated_at, \
                coalesce(m.updated_at, m.created_at) as cursor_stamp, \
                m.created_at, m.deleted_at, \
                m.width, m.height, m.alt_text, m.description \
         from media m \
         where m.deleted_at is null",
    );
    let mut index = 1;
    if let Some(site_id) = request.site_id {
        sql.push_str(&format!(" and m.site_id = ${index}"));
        index += 1;
    }
    if let Some(mime) = &request.mime {
        sql.push_str(&format!(" and m.content_type = ${index}"));
        index += 1;
    }
    if let Some(filename) = &request.filename {
        sql.push_str(&format!(" and m.filename = ${index}"));
        index += 1;
    }
    if let Some(since) = request.updated_since {
        sql.push_str(&format!(
            " and coalesce(m.updated_at, m.created_at) > ${index}"
        ));
        index += 1;
    }
    if let Some(cursor) = &request.cursor {
        sql.push_str(&format!(
            " and ({column} < ${index} or ({column} = ${index} and m.id < ${next}))",
            column = request.column,
            next = index + 1
        ));
        index += 2;
    }
    sql.push_str(&format!(
        " order by {column} desc, m.id desc limit ${index}",
        column = request.column
    ));

    let mut statement = sqlx::query(&sql);
    if let Some(site_id) = request.site_id {
        statement = statement.bind(site_id);
    }
    if let Some(mime) = &request.mime {
        statement = statement.bind(mime);
    }
    if let Some(filename) = &request.filename {
        statement = statement.bind(filename);
    }
    if let Some(since) = request.updated_since {
        statement = statement.bind(since);
    }
    if let Some(cursor) = &request.cursor {
        // The parsed instant, bound as the instant. See the pages list for why binding the string
        // was a 500 — this endpoint had the same shape with the same failure waiting behind it.
        let value = content_read::cursor_instant(&cursor.value)
            .map_err(|error| bad_parameter("cursor", error.to_string()))?;
        statement = statement.bind(value).bind(cursor.id);
    }
    let rows = statement
        .bind(request.fetch_limit())
        .fetch_all(state.db().pool())
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                format!("listing media: {error}"),
            )
        })?;

    let fetched = rows.len() as i64;
    // As on the pages list: only the rows the caller asked for are rendered, and `count` counts
    // exactly those. The over-fetched row decides `next_cursor` and is not part of the answer.
    let visible = &rows[..rows.len().min(request.limit as usize)];
    let mut items: Vec<Value> = Vec::with_capacity(visible.len());
    let mut stamps: Vec<(String, Uuid)> = Vec::with_capacity(visible.len());
    for row in visible {
        let updated_at: OffsetDateTime = row.get("updated_at");
        let item = ReadMedia {
            id: row.get("id"),
            slug: row.get("filename"),
            site_id: row.get("site_id"),
            updated_at,
            filename: row.get("filename"),
            mime: row.get("content_type"),
            size_bytes: row.get("size_bytes"),
            width: row.get("width"),
            height: row.get("height"),
            storage_key: row.get("storage_key"),
            alt_text: row.get("alt_text"),
            description: row.get("description"),
            deleted_at: row.get("deleted_at"),
        };
        stamps.push((content_read::stamp(updated_at), item.id));
        items.push(item.to_value(&request.fields));
    }
    let next_cursor = if content_read::continues(fetched, request.limit) {
        visible.last().map(|row| {
            content_read::encode_cursor(&Cursor::new(
                sort_value_of(row, request.sort, SortSource::Media),
                row.get("id"),
            ))
        })
    } else {
        None
    };

    Ok(ReadResponse {
        status: StatusCode::OK,
        spent: Some(call.rate),
        etag: content_read::list_etag(&stamps),
        body: json!({
            "items": items,
            "next_cursor": next_cursor,
            "count": items.len(),
        }),
    })
}

/// `GET /api/v1/content/openapi.json` — the contract this surface implements.
///
/// Served from the same endpoint table the routes are documented by, so the document cannot
/// describe a route that does not exist or omit one that does. The base URL is taken from the
/// request's own authority rather than from configuration: a hard-coded host produces a document
/// whose every example is wrong on every installation but one.
pub async fn openapi_document(
    State(_state): State<AppState>,
    token: ContentToken,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    token.require_scope("content:read")?;
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    let document = crate::routes::content_openapi::document(&format!("https://{host}/api/v1"));
    Ok(Json(document).into_response())
}

/// `GET /api/v1/content/sites` — the sites this token may read.
pub async fn list_sites(
    State(state): State<AppState>,
    token: ContentToken,
) -> Result<Json<Vec<Value>>, ApiError> {
    token.require_scope("content:read")?;
    let sites = match token.0.token.site_id {
        Some(site_id) => sqlx::query(
            "select id, key, name, theme, status from sites where id = $1 and status <> 'archived'",
        )
        .bind(site_id)
        .fetch_all(state.db().pool())
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                format!("listing sites: {error}"),
            )
        })?,
        None => sqlx::query(
            "select id, key, name, theme, status from sites \
             where organization_id = $1 and status <> 'archived' order by key asc",
        )
        .bind(token.0.token.organization_id)
        .fetch_all(state.db().pool())
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                format!("listing sites: {error}"),
            )
        })?,
    };
    let items = sites
        .iter()
        .map(|row| {
            json!({
                "id": row.get::<Uuid, _>("id").to_string(),
                "key": row.get::<String, _>("key"),
                "name": row.get::<String, _>("name"),
                "theme": row.get::<String, _>("theme"),
                "status": row.get::<String, _>("status"),
            })
        })
        .collect();
    Ok(Json(items))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(value: Option<&str>) -> HeaderMap {
        let mut map = HeaderMap::new();
        if let Some(value) = value {
            map.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(value).expect("header"),
            );
        }
        map
    }

    #[test]
    fn a_bearer_token_is_read_with_either_case() {
        assert_eq!(
            bearer_token(&headers(Some("Bearer omn_abc_def"))).expect("read"),
            "omn_abc_def"
        );
        assert_eq!(
            bearer_token(&headers(Some("bearer omn_abc_def"))).expect("read"),
            "omn_abc_def"
        );
    }

    #[test]
    fn a_missing_or_malformed_authorization_header_is_a_401_invalid_token() {
        for case in [
            None,
            Some("omn_abc_def"),
            Some("Basic dXNlcjpwdw=="),
            Some("Bearer "),
        ] {
            let error = bearer_token(&headers(case)).expect_err("must be refused");
            assert_eq!(error.status(), StatusCode::UNAUTHORIZED, "case: {case:?}");
        }
    }

    #[test]
    fn a_token_scoped_to_one_site_filters_rather_than_refuses() {
        let scoped = ContentToken(AuthenticatedToken {
            token: omnion_content::api_tokens::ApiToken {
                id: Uuid::nil(),
                organization_id: Uuid::from_u128(1),
                site_id: Some(Uuid::from_u128(20)),
                name: "frontend".into(),
                prefix: "omn_00000000".into(),
                scopes: vec!["content:read".into()],
                allowed_origins: vec![],
                rate_limit_per_minute: 120,
                expires_at: None,
                revoked_at: None,
                last_used_at: None,
                created_by: None,
                created_at: OffsetDateTime::now_utc(),
            },
        });
        // Asking for its own site is allowed.
        assert_eq!(
            scoped
                .site_filter(Some(Uuid::from_u128(20)))
                .expect("allowed"),
            Some(Uuid::from_u128(20))
        );
        // Asking for another site yields a filter that matches NO site — not a 403, which would
        // confirm the site exists.
        assert_eq!(
            scoped
                .site_filter(Some(Uuid::from_u128(21)))
                .expect("filtered"),
            Some(SITE_OUT_OF_SCOPE)
        );
        // And asking for nothing reads its own site.
        assert_eq!(
            scoped.site_filter(None).expect("own site"),
            Some(Uuid::from_u128(20))
        );
    }

    /// The out-of-scope sentinel must survive the whole chain into the query.
    ///
    /// The bug this pins is a **cross-site read**: `site_filter` returned `Uuid::nil()` and both
    /// call sites stripped it with `.filter(|site| *site != Uuid::nil())` before binding, so
    /// "match nothing" became "no site predicate" — and a single-site token asking about another
    /// site got that site's published pages back with a `200`. The unit test above passed the
    /// whole time, because it asserted the value `site_filter` *returned* and never the value the
    /// SQL *received*.
    ///
    /// So the assertion here is on `ListRequest`, which is what the query builder reads.
    #[test]
    fn the_out_of_scope_sentinel_reaches_the_query_instead_of_being_stripped() {
        let scoped = ContentToken(AuthenticatedToken {
            token: omnion_content::api_tokens::ApiToken {
                id: Uuid::nil(),
                organization_id: Uuid::from_u128(1),
                site_id: Some(Uuid::from_u128(20)),
                name: "frontend".into(),
                prefix: "omn_00000000".into(),
                scopes: vec!["content:read".into()],
                allowed_origins: vec![],
                rate_limit_per_minute: 120,
                expires_at: None,
                revoked_at: None,
                last_used_at: None,
                created_by: None,
                created_at: OffsetDateTime::now_utc(),
            },
        });
        let query = ContentListQuery {
            site: Some(Uuid::from_u128(21)),
            ..ContentListQuery::default()
        };
        let request = ListRequest::for_pages(&scoped, &query).expect("a filtered list is valid");
        assert_eq!(
            request.site_id,
            Some(SITE_OUT_OF_SCOPE),
            "a site predicate must be bound; None would read every site"
        );
        // And the media list, which had the identical strip, must not have it either.
        let media = MediaListQuery {
            site: Some(Uuid::from_u128(21)),
            ..MediaListQuery::default()
        };
        let media_request =
            ListRequest::for_media(&scoped, &media).expect("a filtered list is valid");
        assert_eq!(media_request.site_id, Some(SITE_OUT_OF_SCOPE));
    }

    /// The sentinel must not be a value a site could ever hold, and `Uuid::nil()` is out.
    #[test]
    fn the_out_of_scope_sentinel_is_not_the_nil_uuid() {
        assert_ne!(
            SITE_OUT_OF_SCOPE,
            Uuid::nil(),
            "nil was the earlier sentinel and two call sites stripped it before the query ran"
        );
    }

    #[test]
    fn an_organization_wide_token_may_name_any_site() {
        let open = ContentToken(AuthenticatedToken {
            token: omnion_content::api_tokens::ApiToken {
                id: Uuid::nil(),
                organization_id: Uuid::from_u128(1),
                site_id: None,
                name: "wide".into(),
                prefix: "omn_11111111".into(),
                scopes: vec!["content:read".into()],
                allowed_origins: vec![],
                rate_limit_per_minute: 120,
                expires_at: None,
                revoked_at: None,
                last_used_at: None,
                created_by: None,
                created_at: OffsetDateTime::now_utc(),
            },
        });
        assert_eq!(
            open.site_filter(Some(Uuid::from_u128(30))).expect("any"),
            Some(Uuid::from_u128(30))
        );
        assert_eq!(open.site_filter(None).expect("none named"), None);
    }

    #[test]
    fn a_scope_the_token_lacks_is_a_403_naming_it() {
        let token = ContentToken(AuthenticatedToken {
            token: omnion_content::api_tokens::ApiToken {
                id: Uuid::nil(),
                organization_id: Uuid::from_u128(1),
                site_id: None,
                name: "pages only".into(),
                prefix: "omn_22222222".into(),
                scopes: vec!["content:read".into()],
                allowed_origins: vec![],
                rate_limit_per_minute: 120,
                expires_at: None,
                revoked_at: None,
                last_used_at: None,
                created_by: None,
                created_at: OffsetDateTime::now_utc(),
            },
        });
        assert!(token.require_scope("content:read").is_ok());
        let error = token
            .require_scope("media:read")
            .expect_err("no media scope");
        assert_eq!(error.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn a_limit_outside_the_ceiling_is_refused_with_the_ceiling_named() {
        assert_eq!(limit_of(None).expect("default"), DEFAULT_LIMIT);
        assert_eq!(limit_of(Some(1)).expect("ok"), 1);
        assert_eq!(limit_of(Some(MAX_LIMIT)).expect("ok"), MAX_LIMIT);
        assert!(limit_of(Some(0)).is_err());
        assert!(limit_of(Some(MAX_LIMIT + 1)).is_err());
    }

    #[test]
    fn percent_decoding_reads_a_token_out_of_a_query_string() {
        assert_eq!(percent_decode("omn_abc_def"), "omn_abc_def");
        assert_eq!(percent_decode("omn%5Fabc"), "omn_abc");
        // A stray percent is kept rather than dropped: a mangled token fails authentication,
        // which is the honest outcome, and a silently-truncated one might not.
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn a_recorded_route_is_the_one_the_caller_typed() {
        // The defect this test exists for: `MatchedPath` is absolute from the application root,
        // so the usage row was keyed `/api/v1/content/pages` while the OpenAPI document, the
        // panel's copy button and the explorer's snippet all say `/content/pages`. The response
        // never mentions either, so nothing above the meter could tell — the chart simply grew
        // two rows for one endpoint.
        assert_eq!(surface_route("/api/v1/content/pages"), "/content/pages");
        assert_eq!(
            surface_route("/api/v1/content/pages/{slug}"),
            "/content/pages/{slug}",
            "the template keeps its braces: a slug is not its own row"
        );
        assert_eq!(surface_route("/api/v1/content/media"), "/content/media");
    }

    #[test]
    fn the_stripped_prefix_is_the_one_the_router_is_actually_mounted_at() {
        // The constant and the mount point are two pieces of the same fact, and a version bump
        // that moves the tree would leave the stripper removing a prefix nothing carries — at
        // which point every row keeps `/api/v2/...` and the chart splits again, silently. The
        // mount point is read out of the source rather than restated here, so a change to one
        // without the other is a compile-visible mismatch instead of a quiet data corruption.
        let source = include_str!("mod.rs");
        let needle = format!(".nest(\"{API_PREFIX}\"");
        assert!(
            source.contains(&needle),
            "the v1 tree is no longer mounted at {API_PREFIX} — update the constant the usage \
             rows are keyed against, or the chart will split every endpoint in two"
        );
    }

    #[test]
    fn a_path_that_is_not_under_the_api_prefix_is_answered_as_it_matched() {
        // A handler called outside the v1 tree — a test, a sub-application — has a matched path
        // with no prefix to remove. Guessing (prepending, or stripping the first segment) would
        // write a row under a route the router never matched, which is the same split this
        // function exists to prevent.
        assert_eq!(surface_route("/content/pages"), "/content/pages");
        assert_eq!(surface_route("/api/v1"), "/api/v1", "the bare prefix is not a route");
        // And a near-miss prefix is not stripped: `/api/v10/...` must keep its own name.
        assert_eq!(surface_route("/api/v10/content/pages"), "/api/v10/content/pages");
    }

    #[test]
    fn the_field_whitelists_agree_with_the_error_they_refuse() {
        assert!(SELECTABLE_PAGE_FIELDS.len() <= MAX_FIELDS);
        assert!(SELECTABLE_MEDIA_FIELDS.len() <= MAX_FIELDS);
        for field in content_read::IDENTITY_FIELDS {
            assert!(
                SELECTABLE_PAGE_FIELDS.contains(&field),
                "{field} must be selectable on pages"
            );
        }
    }
}
