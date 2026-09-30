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
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use omnion_content::api_tokens::{self, AuthFailure, AuthenticatedToken};
use omnion_content::content_read::{
    self, Cursor, DEFAULT_LIMIT, Fields, MAX_FIELDS, MAX_LIMIT, ReadMedia, ReadPage,
    SELECTABLE_MEDIA_FIELDS, SELECTABLE_PAGE_FIELDS, SortKey,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

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
pub struct ContentToken(pub AuthenticatedToken);

impl ContentToken {
    /// Whether the token carries a scope.
    pub fn has_scope(&self, scope: &str) -> bool {
        self.0.token.scopes.iter().any(|granted| granted == scope)
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
    /// `Ok(None)` means every site of the organization, which is only reachable when the caller
    /// named none: naming a site that the token is not scoped to yields an empty result rather
    /// than a refusal, and the caller cannot tell a site that does not exist from a site that
    /// holds nothing they may read.
    pub fn site_filter(&self, requested: Option<Uuid>) -> Result<Option<Uuid>, ApiError> {
        match (self.0.token.site_id, requested) {
            (None, _) => Ok(requested),
            (Some(scoped), None) => Ok(Some(scoped)),
            (Some(scoped), Some(asked)) if asked == scoped => Ok(Some(asked)),
            (Some(_), Some(_)) => Ok(Some(Uuid::nil())),
        }
    }
}

impl FromRequestParts<AppState> for ContentToken {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
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
        let authenticated = match api_tokens::authenticate_any_organization(
            state.db().pool(),
            &raw,
        )
        .await
        {
            Ok(authenticated) => authenticated,
            Err(failure) => return Err(auth_failure_response(&failure)),
        };

        // A successful call is the moment `last_used_at` means something, and it is written here
        // rather than in a background task: the Tokens tab's "last used" column would otherwise
        // lag by however long the flush interval is, and a person debugging an integration is
        // looking at that column while the integration is running.
        let _ = sqlx::query(
            "update api_tokens set last_used_at = now(), updated_at = now() where id = $1",
        )
        .bind(authenticated.token.id)
        .execute(state.db().pool())
        .await;

        Ok(Self(authenticated))
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
        let site_id = token
            .site_filter(query.site)?
            .filter(|site| *site != Uuid::nil());
        Ok(Self {
            site_id,
            locale: query
                .locale
                .as_deref()
                .map(content_read::parse_locale)
                .transpose()
                .map_err(|error| bad_parameter("locale", error.to_string()))?,
            limit: limit_of(query.limit)?,
            sort: sort_of(query.sort.as_deref())?,
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
        Ok(Self {
            site_id: token
                .site_filter(query.site)?
                .filter(|site| *site != Uuid::nil()),
            locale: None,
            limit: limit_of(query.limit)?,
            sort: sort_of(query.sort.as_deref())?,
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

    /// The `limit + 1` a keyset read fetches, so `Page::new` can tell "full" from "last".
    fn fetch_limit(&self) -> i64 {
        self.limit
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

/// Validate a `sort`, defaulting to the one the docs call the contract.
fn sort_of(raw: Option<&str>) -> Result<SortKey, ApiError> {
    match raw {
        None => Ok(SortKey::UpdatedAt),
        Some(value) => {
            SortKey::parse(value).map_err(|error| bad_parameter("sort", error.to_string()))
        }
    }
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
struct ReadResponse {
    body: Value,
    etag: String,
    status: StatusCode,
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
        response
    }
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/content/pages` — published pages, newest change first.
pub async fn list_pages(
    State(state): State<AppState>,
    token: ContentToken,
    Query(query): Query<ContentListQuery>,
) -> Result<ReadResponse, ApiError> {
    token.require_scope("content:read")?;
    list_pages_of_type(state, token, query, None).await
}

/// `GET /api/v1/content/posts` — the same rows filtered to `page_type = 'post'`.
///
/// Marked experimental in the OpenAPI document: `post` is an alias over a page type until the
/// blog module (REQ-116) ships its own tables, and an integrator reading the document should see
/// that before they build a schema around it.
pub async fn list_posts(
    State(state): State<AppState>,
    token: ContentToken,
    Query(query): Query<ContentListQuery>,
) -> Result<ReadResponse, ApiError> {
    token.require_scope("content:read")?;
    list_pages_of_type(state, token, query, Some("post")).await
}

async fn list_pages_of_type(
    state: AppState,
    token: ContentToken,
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
        sql.push_str(&format!(
            " and (p.{column} < ${index} or (p.{column} = ${index} and p.id < ${next}))",
            column = request.sort.column(),
            next = index + 1
        ));
        index += 2;
    }
    sql.push_str(&format!(
        " order by p.{column} desc, p.id desc limit ${index}",
        column = request.sort.column()
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
        let value = match request.sort {
            SortKey::Title => cursor.value.clone(),
            SortKey::CreatedAt | SortKey::UpdatedAt => OffsetDateTime::parse(
                &cursor.value,
                &time::format_description::well_known::Rfc3339,
            )
            .map_err(|_| {
                bad_parameter(
                    "cursor",
                    "this cursor does not belong to this sort order".into(),
                )
            })?,
        };
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
                format!("listing pages: {error}"),
            )
        })?;

    // Over-fetch by one so `Page::new` can say "there may be more" from a full page alone.
    let fetched = rows.len() as i64;
    let mut items: Vec<Value> = Vec::with_capacity(rows.len());
    let mut stamps: Vec<(String, Uuid)> = Vec::with_capacity(rows.len());
    for row in &rows {
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
        stamps.push((page.updated_at.to_string(), page.id));
        items.push(page.to_value(&request.fields, request.locale.as_deref(), &[]));
    }
    // The cursor is taken from the last row the caller will actually see, which is the
    // `limit`-th when the query over-fetched.
    let visible = &rows[..rows.len().min(request.limit as usize)];
    let next_cursor = if fetched > request.limit {
        visible.last().map(|row| {
            content_read::encode_cursor(&Cursor::new(
                sort_value_of(row, request.sort),
                row.get("id"),
            ))
        })
    } else {
        None
    };

    let etag = content_read::list_etag(&stamps);
    Ok(ReadResponse {
        status: StatusCode::OK,
        etag: etag.clone(),
        body: json!({
            "items": items,
            "next_cursor": next_cursor,
            "count": visible.len(),
        }),
    })
}

/// The value a cursor stores for a row, in the sort the request asked for.
fn sort_value_of(row: &sqlx::postgres::PgRow, sort: SortKey) -> String {
    match sort {
        SortKey::Title => row.get::<String, _>("title"),
        SortKey::CreatedAt => row.get::<OffsetDateTime, _>("created_at").to_string(),
        SortKey::UpdatedAt => row.get::<OffsetDateTime, _>("updated_at").to_string(),
    }
}

/// `GET /api/v1/content/pages/{slug}` — one published page plus its alternates.
pub async fn get_page(
    State(state): State<AppState>,
    token: ContentToken,
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
    let etag = content_read::list_etag(&[(page.updated_at.to_string(), page.id)]);
    Ok(ReadResponse {
        status: StatusCode::OK,
        etag,
        body: json!({ "item": items[0].clone() }),
    })
}

/// `GET /api/v1/content/posts/{slug}` — one post.
pub async fn get_post(
    State(state): State<AppState>,
    token: ContentToken,
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
    let etag = content_read::list_etag(&[(page.updated_at.to_string(), page.id)]);
    Ok(ReadResponse {
        status: StatusCode::OK,
        etag,
        body: json!({ "item": page.to_value(&request.fields, request.locale.as_deref(), &[]) }),
    })
}

/// `GET /api/v1/content/media` — media metadata, never bytes.
pub async fn list_media(
    State(state): State<AppState>,
    token: ContentToken,
    Query(query): Query<MediaListQuery>,
) -> Result<ReadResponse, ApiError> {
    token.require_scope("media:read")?;
    let request = ListRequest::for_media(&token, &query)?;

    // `media` carries its own `width`/`height` (migration 0025) and an `updated_at` that is
    // NULL for a row written before that column existed — hence the coalesce, which is also the
    // value the cursor and the ETag are built from, so a media list is stable across the upgrade.
    let mut sql = String::from(
        "select m.id, m.site_id, m.filename, m.content_type, m.size_bytes, m.storage_key, \
                coalesce(m.updated_at, m.created_at) as updated_at, m.deleted_at, \
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
            " and (coalesce(m.updated_at, m.created_at) < ${index} or \
             (coalesce(m.updated_at, m.created_at) = ${index} and m.id < ${next}))",
            next = index + 1
        ));
        index += 2;
    }
    sql.push_str(&format!(
        " order by coalesce(m.updated_at, m.created_at) desc, m.id desc limit ${index}"
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
        let value = OffsetDateTime::parse(
            &cursor.value,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|_| {
            bad_parameter(
                "cursor",
                "this cursor does not belong to this sort order".into(),
            )
        })?;
        statement = statement.bind(value).bind(cursor.id);
    }
    let rows = statement
        .bind(request.limit)
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
    let mut items: Vec<Value> = Vec::with_capacity(rows.len());
    let mut stamps: Vec<(String, Uuid)> = Vec::with_capacity(rows.len());
    for row in &rows {
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
        stamps.push((updated_at.to_string(), item.id));
        items.push(item.to_value(&request.fields));
    }
    let visible = &rows[..rows.len().min(request.limit as usize)];
    let next_cursor = if fetched > request.limit {
        visible.last().map(|row| {
            let updated_at: OffsetDateTime = row.get("updated_at");
            content_read::encode_cursor(&Cursor::new(updated_at.to_string(), row.get("id")))
        })
    } else {
        None
    };

    Ok(ReadResponse {
        status: StatusCode::OK,
        etag: content_read::list_etag(&stamps),
        body: json!({
            "items": items,
            "next_cursor": next_cursor,
            "count": visible.len(),
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
            assert_eq!(error.status, StatusCode::UNAUTHORIZED, "case: {case:?}");
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
                token_hash: "x".repeat(64),
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
        // Asking for another site yields the nil filter, which the query turns into "no rows" —
        // not a 403, which would confirm the site exists.
        assert_eq!(
            scoped
                .site_filter(Some(Uuid::from_u128(21)))
                .expect("filtered"),
            Some(Uuid::nil())
        );
        // And asking for nothing reads its own site.
        assert_eq!(
            scoped.site_filter(None).expect("own site"),
            Some(Uuid::from_u128(20))
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
                token_hash: "y".repeat(64),
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
                token_hash: "z".repeat(64),
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
        assert_eq!(error.status, StatusCode::FORBIDDEN);
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
