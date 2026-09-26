//! The query language and the search itself.
//!
//! A raw query string is parsed into terms plus *scoped filters* — `type:page`, `site:acme`,
//! `owner:me`, `before:2026-09-01`, `after:2026-06-01`, `is:draft` — and then translated into
//! one SQL statement over `search_documents`. Two properties are non-negotiable and both live
//! here:
//!
//! * **The narrowing happens inside the query.** The caller's organization, the providers their
//!   read permissions cover and every filter ride the `WHERE` clause, so a later page can never
//!   contain a row page one was not allowed to count (docs/requests/REQ-002, risks).
//! * **Nothing is invented.** An unknown `type:` value matches no provider and yields an honest
//!   empty result with a hint saying so; a token that cannot be parsed is reported as a hint
//!   instead of being silently dropped.
//!
//! Ranking is `ts_rank_cd` over the generated vector with the weights of `search_settings`
//! (title / tags / subtitle / body, see `0012_search.sql`), plus a small bonus when the title
//! matches as a prefix — the `pg_trgm` index carries both that and the similarity match.

use sqlx::{PgPool, Postgres, QueryBuilder};
use time::{Date, Month, OffsetDateTime};
use uuid::Uuid;

use crate::error::Result;
use crate::providers;

/// Longest accepted query, in characters.
pub const MAX_QUERY_LENGTH: usize = 200;
/// Hits per page when the caller does not ask.
pub const DEFAULT_PER_PAGE: i64 = 25;
/// Largest page a caller may ask for.
pub const MAX_PER_PAGE: i64 = 100;
/// Suggestions `suggest` answers with at most.
pub const SUGGEST_LIMIT: i64 = 8;

/// Why a raw string cannot be searched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryError {
    /// Nothing was typed at all.
    Empty,
}

/// One parsed query: the searchable terms and the scoped filters around them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    raw: String,
    terms: Vec<String>,
    types: Vec<String>,
    site: Option<String>,
    owner_me: bool,
    before: Option<OffsetDateTime>,
    after: Option<OffsetDateTime>,
    flags: Vec<String>,
    hints: Vec<String>,
}

impl Query {
    /// Parse a raw string; `Err(Empty)` when there is nothing to search for.
    ///
    /// A filter-only query (`type:page is:draft`) is valid on purpose: listing a slice of the
    /// index without a text term is a real question, and the results screen's facet rail needs
    /// it.
    pub fn parse(raw: &str) -> std::result::Result<Self, QueryError> {
        let trimmed: String = raw.trim().chars().take(MAX_QUERY_LENGTH).collect();
        if trimmed.is_empty() {
            return Err(QueryError::Empty);
        }

        let mut query = Self {
            raw: trimmed,
            terms: Vec::new(),
            types: Vec::new(),
            site: None,
            owner_me: false,
            before: None,
            after: None,
            flags: Vec::new(),
            hints: Vec::new(),
        };

        for token in query.raw.clone().split_whitespace() {
            let Some((name, value)) = token.split_once(':') else {
                query.terms.push(token.to_lowercase());
                continue;
            };
            let name = name.to_lowercase();
            let value = value.trim();
            if value.is_empty() {
                query.terms.push(token.to_lowercase());
                continue;
            }
            match name.as_str() {
                "type" => {
                    let value = value.to_lowercase();
                    if !providers::is_known_type(&value) {
                        query.hints.push(format!(
                            "no provider answers {value}: known types are {}",
                            known_types().join(", ")
                        ));
                    }
                    if !query.types.contains(&value) {
                        query.types.push(value);
                    }
                }
                "site" => query.site = Some(value.to_lowercase()),
                "owner" => {
                    if value.eq_ignore_ascii_case("me") {
                        query.owner_me = true;
                    } else {
                        query
                            .hints
                            .push("owner: only answers \"me\" today".to_owned());
                    }
                }
                "before" | "after" => match parse_date(value) {
                    Some(stamp) => {
                        if name == "before" {
                            query.before = Some(stamp);
                        } else {
                            query.after = Some(stamp);
                        }
                    }
                    None => query.hints.push(format!(
                        "{name}:{value} is not a date — write it as YYYY-MM-DD"
                    )),
                },
                "is" => {
                    let value = value.to_lowercase();
                    if matches!(value.as_str(), "draft" | "published") {
                        if !query.flags.contains(&value) {
                            query.flags.push(value);
                        }
                    } else {
                        query
                            .hints
                            .push(format!("is:{value} is not a known flag — draft, published"));
                    }
                }
                _ => query.terms.push(token.to_lowercase()),
            }
        }

        // Nothing usable: no terms, no filter and nothing to explain. A string that produced
        // only hints is answered (as an empty result with those hints) rather than refused —
        // the caller typed something and deserves to be told what went wrong.
        if query.terms.is_empty()
            && query.types.is_empty()
            && query.site.is_none()
            && !query.owner_me
            && query.before.is_none()
            && query.after.is_none()
            && query.flags.is_empty()
            && query.hints.is_empty()
        {
            return Err(QueryError::Empty);
        }

        Ok(query)
    }

    /// The raw (trimmed, capped) query text.
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// The text terms, lower-cased, in the order they were written.
    #[must_use]
    pub fn terms(&self) -> &[String] {
        &self.terms
    }

    /// The `type:` filters, lower-cased; unknown values are kept so they match nothing.
    #[must_use]
    pub fn types(&self) -> &[String] {
        &self.types
    }

    /// The `site:` filter (a site key, a site id or a domain host), lower-cased.
    #[must_use]
    pub fn site(&self) -> Option<&str> {
        self.site.as_deref()
    }

    /// The `is:` flags (`draft`, `published`).
    #[must_use]
    pub fn flags(&self) -> &[String] {
        &self.flags
    }

    /// Everything the parser could not use, in caller-facing language.
    #[must_use]
    pub fn hints(&self) -> &[String] {
        &self.hints
    }

    /// `true` when the query carries text to rank (as opposed to filters only).
    #[must_use]
    pub fn has_terms(&self) -> bool {
        !self.terms.is_empty()
    }

    /// The full-text input (`websearch_to_tsquery` takes user text as it is written).
    #[must_use]
    fn fts_input(&self) -> String {
        self.terms.join(" ")
    }

    /// The `ILIKE` pattern that answers "the title starts with what I typed".
    #[must_use]
    fn title_prefix(&self) -> String {
        format!("{}%", escape_like(&self.terms.join(" ")))
    }
}

/// Every type name a `type:` filter may use, for the hint text.
#[must_use]
pub fn known_types() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = Vec::new();
    for spec in providers::PROVIDERS {
        names.push(spec.key);
        names.push(spec.entity_type);
    }
    names
}

/// How the hits are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// Best match first (the default).
    Relevance,
    /// Recently changed first.
    Newest,
    /// Alphabetical by title.
    Title,
}

impl Sort {
    /// Parse the `sort` parameter; `None` means "not one of ours".
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "relevance" | "" => Some(Self::Relevance),
            "newest" => Some(Self::Newest),
            "title" => Some(Self::Title),
            _ => None,
        }
    }
}

/// One search request, already narrowed to what the caller may see.
#[derive(Debug, Clone)]
pub struct SearchRequest {
    /// The parsed query.
    pub query: Query,
    /// Provider keys the caller's read permissions cover.
    pub providers: Vec<&'static str>,
    /// Caller's organization; `None` means a platform-level account (sees everything).
    pub organization_id: Option<Uuid>,
    /// Caller's account id, for `owner:me`.
    pub user_id: Uuid,
    /// One-based page number.
    pub page: i64,
    /// Hits per page.
    pub per_page: i64,
    /// Ordering.
    pub sort: Sort,
}

/// One hit of the index.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Hit {
    /// Provider key (`pages`).
    pub provider: String,
    /// Document type (`page`).
    pub entity_type: String,
    /// Entity id inside its own domain.
    pub entity_id: String,
    /// Title the palette shows first.
    pub title: String,
    /// Supporting line.
    pub subtitle: String,
    /// Panel route a click opens.
    pub url: String,
    /// Tags stored with the document (page status, content type …).
    pub tags: Vec<String>,
    /// When the entity itself last changed.
    pub entity_updated_at: Option<OffsetDateTime>,
    /// Rank inside this answer.
    pub score: f32,
}

/// One page of hits.
#[derive(Debug, Clone)]
pub struct HitPage {
    /// The hits of this page, best first.
    pub hits: Vec<Hit>,
    /// Total hits the query matches, across every page.
    pub total: i64,
}

/// How many hits one provider contributes to a query (the palette's section counts).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProviderCount {
    /// Provider key.
    pub provider: String,
    /// Hits the provider contributes.
    pub count: i64,
}

/// One provider's line on the search status screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderStatus {
    /// Provider key.
    pub provider: &'static str,
    /// Display title.
    pub title: &'static str,
    /// Documents the provider has in the index.
    pub documents: i64,
    /// When the provider's rows were last written.
    pub last_indexed_at: Option<OffsetDateTime>,
    /// `ready` when the provider has rows, `empty` when it has none to show yet.
    pub state: &'static str,
}

/// One prefix suggestion.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Suggestion {
    /// Title of the suggested document.
    pub title: String,
    /// Panel route it opens.
    pub url: String,
    /// Provider it belongs to.
    pub provider: String,
}

/// The ranking weights, in the order `ts_rank_cd` wants them: `{body, subtitle, tags, title}`.
///
/// `ts_rank_cd` only accepts weights in `0..=1`, while `search_settings.weights` is written the
/// way a person reasons about it (title 6, tags 4, subtitle 3, body 1 — the defaults). The
/// values are therefore normalised against the largest one before they are bound, which keeps
/// the ordering the operator chose and stays inside what PostgreSQL accepts. The title weight
/// ends up as `1.0` by construction: the title is the field that matters most.
const DEFAULT_SETTINGS_WEIGHTS: [f64; 4] = [1.0, 3.0, 4.0, 6.0];

/// Read the installation's ranking weights from `search_settings` and normalise them.
async fn weights(pool: &PgPool) -> Result<[f32; 4]> {
    let stored: Option<serde_json::Value> =
        sqlx::query_scalar("select weights from search_settings where id = 1")
            .fetch_optional(pool)
            .await?;

    let read = |key: &str, fallback: f64| -> f64 {
        stored
            .as_ref()
            .and_then(|value| value.get(key))
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(fallback)
            .clamp(0.0, 10.0)
    };
    let body = read("body", DEFAULT_SETTINGS_WEIGHTS[0]);
    let subtitle = read("subtitle", DEFAULT_SETTINGS_WEIGHTS[1]);
    let tags = read("tags", DEFAULT_SETTINGS_WEIGHTS[2]);
    let title = read("title", DEFAULT_SETTINGS_WEIGHTS[3]);

    let largest = body.max(subtitle).max(tags).max(title);
    if largest <= 0.0 {
        // Every field muted is a mistake, not a ranking: fall back to the defaults.
        return Ok(normalised(DEFAULT_SETTINGS_WEIGHTS));
    }
    Ok(normalised([body, subtitle, tags, title]))
}

/// Scale four settings weights into `0..=1` against their largest value.
fn normalised(weights: [f64; 4]) -> [f32; 4] {
    let largest = weights.iter().cloned().fold(f64::MIN, f64::max).max(1.0);
    [
        (weights[0] / largest) as f32,
        (weights[1] / largest) as f32,
        (weights[2] / largest) as f32,
        (weights[3] / largest) as f32,
    ]
}

/// Push the shared `WHERE` conditions of a query, binds included.
///
/// Both the hit query and the per-provider counts call this in the same order, so their binds
/// line up; it is the single place the scope and permission rules live.
fn push_conditions(builder: &mut QueryBuilder<'_, Postgres>, request: &SearchRequest) {
    builder.push("d.provider = any(");
    builder.push_bind(request.providers.clone());
    builder.push("::text[])");

    builder.push(" and (");
    builder.push_bind(request.organization_id);
    builder.push("::uuid is null or d.organization_id = ");
    builder.push_bind(request.organization_id);
    builder.push(")");

    let query = &request.query;
    if query.has_terms() {
        builder.push(" and (d.document @@ websearch_to_tsquery('simple', ");
        builder.push_bind(query.fts_input());
        builder.push(") or d.title ilike ");
        builder.push_bind(query.title_prefix());
        builder.push(" or d.title % ");
        builder.push_bind(query.raw().to_lowercase());
        builder.push(")");
    }

    if !query.types().is_empty() {
        let types = query.types().to_vec();
        builder.push(" and (d.provider = any(");
        builder.push_bind(types.clone());
        builder.push("::text[]) or d.entity_type = any(");
        builder.push_bind(types);
        builder.push("::text[]))");
    }

    if let Some(site) = &query.site {
        builder.push(" and (d.site_id::text = ");
        builder.push_bind(site.clone());
        builder.push(" or d.site_id in (select s.id from sites s where s.key = ");
        builder.push_bind(site.clone());
        builder.push(
            " or exists (select 1 from site_domains dom where dom.site_id = s.id and dom.host = ",
        );
        builder.push_bind(site.clone());
        builder.push(")))");
    }

    if query.owner_me {
        builder.push(" and d.owner_user_id = ");
        builder.push_bind(request.user_id);
    }

    if let Some(after) = query.after {
        builder.push(" and (d.entity_updated_at is not null and d.entity_updated_at >= ");
        builder.push_bind(after);
        builder.push(")");
    }

    if let Some(before) = query.before {
        builder.push(" and (d.entity_updated_at is not null and d.entity_updated_at < ");
        builder.push_bind(before);
        builder.push(")");
    }

    if !query.flags().is_empty() {
        builder.push(" and d.tags && ");
        builder.push_bind(query.flags().to_vec());
        builder.push("::text[]");
    }
}

/// Run a query and answer one page of hits plus the total.
pub async fn search(pool: &PgPool, request: &SearchRequest) -> Result<HitPage> {
    let weights = weights(pool).await?;
    let per_page = request.per_page.clamp(1, MAX_PER_PAGE);
    let page = request.page.max(1);
    let offset = (page - 1) * per_page;

    let mut builder: QueryBuilder<'_, Postgres> = QueryBuilder::new(
        "select d.provider, d.entity_type, d.entity_id, d.title, d.subtitle, d.url, d.tags, \
         d.entity_updated_at, ",
    );
    if request.query.has_terms() {
        builder.push("ts_rank_cd(");
        builder.push_bind(weights.to_vec());
        builder.push("::float4[], d.document, websearch_to_tsquery('simple', ");
        builder.push_bind(request.query.fts_input());
        builder.push(")) + case when d.title ilike ");
        builder.push_bind(request.query.title_prefix());
        builder.push(" then 0.5::real else 0.0::real end");
    } else {
        builder.push("0.0::real");
    }
    builder.push(" as score, count(*) over () as total from search_documents d where ");
    push_conditions(&mut builder, request);

    builder.push(" order by ");
    match request.sort {
        Sort::Relevance => builder
            .push("score desc, d.entity_updated_at desc nulls last, d.title asc, d.provider asc"),
        Sort::Newest => {
            builder.push("d.entity_updated_at desc nulls last, d.title asc, d.provider asc")
        }
        Sort::Title => builder.push("d.title asc, d.provider asc"),
    };
    builder.push(" limit ");
    builder.push_bind(per_page);
    builder.push(" offset ");
    builder.push_bind(offset);

    let rows: Vec<HitWithTotal> = builder.build_query_as().fetch_all(pool).await?;

    if rows.is_empty() {
        let total = count(pool, request).await?;
        return Ok(HitPage {
            hits: Vec::new(),
            total,
        });
    }

    let total = rows[0].total;
    Ok(HitPage {
        hits: rows.into_iter().map(HitWithTotal::into_hit).collect(),
        total,
    })
}

/// Row shape with the window count riding along.
#[derive(Debug, sqlx::FromRow)]
struct HitWithTotal {
    provider: String,
    entity_type: String,
    entity_id: String,
    title: String,
    subtitle: String,
    url: String,
    tags: Vec<String>,
    entity_updated_at: Option<OffsetDateTime>,
    score: f32,
    total: i64,
}

impl HitWithTotal {
    fn into_hit(self) -> Hit {
        Hit {
            provider: self.provider,
            entity_type: self.entity_type,
            entity_id: self.entity_id,
            title: self.title,
            subtitle: self.subtitle,
            url: self.url,
            tags: self.tags,
            entity_updated_at: self.entity_updated_at,
            score: self.score,
        }
    }
}

/// Count the hits of a query (used when the page itself is empty).
async fn count(pool: &PgPool, request: &SearchRequest) -> Result<i64> {
    let mut builder: QueryBuilder<'_, Postgres> =
        QueryBuilder::new("select count(*) from search_documents d where ");
    push_conditions(&mut builder, request);
    let total: i64 = builder.build_query_scalar().fetch_one(pool).await?;
    Ok(total)
}

/// How many hits each provider contributes to a query — the palette's section counts.
pub async fn counts(pool: &PgPool, request: &SearchRequest) -> Result<Vec<ProviderCount>> {
    let mut builder: QueryBuilder<'_, Postgres> = QueryBuilder::new(
        "select d.provider as provider, count(*)::bigint as count from search_documents d where ",
    );
    push_conditions(&mut builder, request);
    builder.push(" group by d.provider order by count desc, d.provider asc");
    let rows: Vec<ProviderCount> = builder.build_query_as().fetch_all(pool).await?;
    Ok(rows)
}

/// Title-prefix suggestions for the palette's first paint.
pub async fn suggest(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    providers: &[&'static str],
    prefix: &str,
) -> Result<Vec<Suggestion>> {
    let prefix = prefix.trim().to_lowercase();
    if providers.is_empty() || prefix.is_empty() {
        return Ok(Vec::new());
    }
    let pattern = format!("{}%", escape_like(&prefix));
    let rows: Vec<Suggestion> = sqlx::query_as(
        "select d.title, d.url, d.provider from search_documents d \
         where d.provider = any($1::text[]) \
           and ($2::uuid is null or d.organization_id = $2) \
           and lower(d.title) like $3 \
         order by d.title asc, d.provider asc \
         limit $4",
    )
    .bind(providers.to_vec())
    .bind(organization_id)
    .bind(pattern)
    .bind(SUGGEST_LIMIT)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One line per provider for the status screen.
pub async fn status(pool: &PgPool) -> Result<Vec<ProviderStatus>> {
    let rows: Vec<(String, i64, Option<OffsetDateTime>)> = sqlx::query_as(
        "select provider, count(*)::bigint, max(indexed_at) from search_documents \
         group by provider",
    )
    .fetch_all(pool)
    .await?;

    Ok(providers::PROVIDERS
        .iter()
        .map(|spec| {
            let row = rows.iter().find(|(key, _, _)| key == spec.key);
            let documents = row.map_or(0, |(_, count, _)| *count);
            ProviderStatus {
                provider: spec.key,
                title: spec.title,
                documents,
                last_indexed_at: row.and_then(|(_, _, at)| *at),
                state: if documents > 0 { "ready" } else { "empty" },
            }
        })
        .collect())
}

/// Escape the wildcards of a `LIKE` pattern so a literal `%` in a query stays literal.
#[must_use]
pub fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Parse `YYYY-MM-DD` into midnight UTC.
fn parse_date(value: &str) -> Option<OffsetDateTime> {
    let mut parts = value.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u8 = parts.next()?.parse().ok()?;
    let day: u8 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let month = Month::try_from(month).ok()?;
    let date = Date::from_calendar_date(year, month, day).ok()?;
    Some(date.midnight().assume_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(input: &str) -> Query {
        Query::parse(input).expect("the query must parse")
    }

    #[test]
    fn an_empty_query_is_refused() {
        assert_eq!(Query::parse(""), Err(QueryError::Empty));
        assert_eq!(Query::parse("   "), Err(QueryError::Empty));
    }

    #[test]
    fn plain_terms_are_lower_cased() {
        let query = parsed("Release Notes");
        assert_eq!(query.terms(), &["release".to_owned(), "notes".to_owned()]);
        assert!(query.has_terms());
        assert!(query.hints().is_empty());
    }

    #[test]
    fn scoped_filters_do_not_become_terms() {
        let query = parsed(
            "release type:page site:acme owner:me after:2026-06-01 before:2026-09-01 is:draft",
        );
        assert_eq!(query.terms(), &["release".to_owned()]);
        assert_eq!(query.types(), &["page".to_owned()]);
        assert_eq!(query.site(), Some("acme"));
        assert!(query.owner_me);
        assert_eq!(query.flags(), &["draft".to_owned()]);
        assert!(query.before.is_some() && query.after.is_some());
        assert!(query.hints().is_empty(), "hints: {:?}", query.hints());
    }

    #[test]
    fn a_filter_only_query_is_valid() {
        let query = parsed("type:page is:draft");
        assert!(!query.has_terms());
        assert_eq!(query.types(), &["page".to_owned()]);
    }

    #[test]
    fn an_unknown_type_is_kept_and_explained() {
        let query = parsed("unicorn type:unicorn");
        assert_eq!(query.types(), &["unicorn".to_owned()]);
        assert_eq!(query.hints().len(), 1);
        assert!(query.hints()[0].contains("no provider answers"));
    }

    #[test]
    fn a_malformed_date_is_a_hint_not_a_silent_drop() {
        let query = parsed("release before:soon");
        assert_eq!(query.terms(), &["release".to_owned()]);
        assert!(query.before.is_none());
        assert_eq!(query.hints().len(), 1);
        assert!(query.hints()[0].contains("YYYY-MM-DD"));
    }

    #[test]
    fn unknown_flags_and_owners_are_explained() {
        let query = parsed("owner:ada is:banana");
        assert_eq!(query.hints().len(), 2);
        assert!(query.flags().is_empty());
        assert!(!query.owner_me);
    }

    #[test]
    fn a_token_without_a_value_stays_a_term() {
        let query = parsed("type:");
        assert_eq!(query.terms(), &["type:".to_owned()]);
        assert!(query.types().is_empty());
    }

    #[test]
    fn long_queries_are_capped() {
        let long = "x".repeat(500);
        let query = parsed(&long);
        assert!(query.raw().chars().count() <= MAX_QUERY_LENGTH);
    }

    #[test]
    fn suggests_escape_their_wildcards() {
        assert_eq!(escape_like("100%_"), "100\\%\\_");
    }

    #[test]
    fn weights_are_scaled_into_the_range_ts_rank_accepts() {
        let defaults = normalised(DEFAULT_SETTINGS_WEIGHTS);
        assert!(
            (defaults[3] - 1.0).abs() < f32::EPSILON,
            "the title is the top weight"
        );
        assert!(defaults.iter().all(|weight| (0.0..=1.0).contains(weight)));
        assert!(
            defaults[2] > defaults[1] && defaults[1] > defaults[0],
            "order is kept"
        );

        // A muted field stays muted, and the title still tops the scale.
        let custom = normalised([1.0, 3.0, 4.0, 10.0]);
        assert!((custom[3] - 1.0).abs() < 1e-6);
        assert!(custom[0] < 0.2);
    }

    #[test]
    fn sort_parses_its_three_values() {
        assert_eq!(Sort::parse("relevance"), Some(Sort::Relevance));
        assert_eq!(Sort::parse("newest"), Some(Sort::Newest));
        assert_eq!(Sort::parse("title"), Some(Sort::Title));
        assert_eq!(Sort::parse("sideways"), None);
    }
}
