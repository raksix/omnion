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

/// The "Updated" facet of the results screen: ranges of `entity_updated_at`.
///
/// `Never` is its own value rather than part of `Older`: a document with no timestamp is a
/// different fact from one that is old, and hiding it inside "Older" would make the counts lie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatedRange {
    /// Changed in the last day.
    Today,
    /// Changed in the last seven days.
    Week,
    /// Changed in the last thirty days.
    Month,
    /// Changed more than thirty days ago.
    Older,
    /// No timestamp was recorded for the entity.
    Never,
}

impl UpdatedRange {
    /// Parse the `updated` filter; `None` means "not one of ours".
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "today" => Some(Self::Today),
            "week" => Some(Self::Week),
            "month" => Some(Self::Month),
            "older" => Some(Self::Older),
            "never" => Some(Self::Never),
            _ => None,
        }
    }

    /// The stable value the API and the URLs carry.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Today => "today",
            Self::Week => "week",
            Self::Month => "month",
            Self::Older => "older",
            Self::Never => "never",
        }
    }

    /// The human label of the facet value.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Today => "Today",
            Self::Week => "This week",
            Self::Month => "This month",
            Self::Older => "Older than a month",
            Self::Never => "Never updated",
        }
    }
}

/// Which filter a facet is counting, so its own filter can be left out of its counts.
///
/// A facet's counts answer "what would adding this value do?" — that question only has an answer
/// when the facet's own filter is not applied to it. Every other filter stays, so the number the
/// rail shows is the number the click produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterKind {
    /// The `type:` / `types=` filter.
    Types,
    /// The `site:` / `site_id=` filter.
    Site,
    /// The `owner:` / `owner=` filter.
    Owner,
    /// The `language=` filter.
    Language,
    /// The `status=` / `is:` filter.
    Status,
    /// The `updated=` bucket filter.
    Updated,
}

/// The filters the results screen applies through query parameters, next to the ones a person can
/// type into the box.
///
/// Both sources land in the same `WHERE` clause and are OR-ed per kind (`type:page` plus
/// `types=media` means "pages or media"), which is what lets the facet rail and the query language
/// live side by side without one silently overriding the other.
#[derive(Debug, Clone, Default)]
pub struct SearchFilters {
    /// `types=` — provider keys or entity types, comma-separated in the URL.
    pub types: Vec<String>,
    /// `site_id=` — a site id, key or domain host.
    pub site: Option<String>,
    /// `owner=me` — the caller's own documents.
    pub owner_me: bool,
    /// `owner=<uuid>` — one account's documents.
    pub owner_id: Option<Uuid>,
    /// `language=` — an exact language code.
    pub language: Option<String>,
    /// `status=` — one tag (`draft`, `published`, `archived`, …).
    pub status: Option<String>,
    /// `updated=` — a range of the last change.
    pub updated: Option<UpdatedRange>,
    /// `before=YYYY-MM-DD` — exclusive upper bound on the last change.
    pub before: Option<OffsetDateTime>,
    /// `after=YYYY-MM-DD` — inclusive lower bound on the last change.
    pub after: Option<OffsetDateTime>,
}

impl SearchFilters {
    /// `true` when nothing is narrowed, so the request is exactly the query language's own.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
            && self.site.is_none()
            && !self.owner_me
            && self.owner_id.is_none()
            && self.language.is_none()
            && self.status.is_none()
            && self.updated.is_none()
            && self.before.is_none()
            && self.after.is_none()
    }
}

/// Parse a `YYYY-MM-DD` filter value; `None` when it is not a date.
#[must_use]
pub fn parse_filter_date(value: &str) -> Option<OffsetDateTime> {
    parse_date(value)
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
    /// Filters that arrived as query parameters (the facet rail's own source).
    pub filters: SearchFilters,
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
    /// Display name of the account that owns the entity, when it has one.
    pub owner: Option<String>,
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
    /// `indexing` · `failed` · `stale` · `ready` · `empty` — see [`provider_state`].
    pub state: &'static str,
    /// The most recent reindex pass, when one ever ran.
    pub last_run: Option<ReindexRun>,
}

/// One reindex pass as the status screen reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReindexRun {
    /// Documents written by the pass.
    pub indexed: Option<i64>,
    /// Documents pruned by the pass.
    pub pruned: Option<i64>,
    /// Wall time of the pass, in milliseconds.
    pub duration_ms: Option<i64>,
    /// When it started.
    pub started_at: OffsetDateTime,
    /// When it finished; `None` while it is still running.
    pub finished_at: Option<OffsetDateTime>,
    /// Why it failed, when it did.
    pub error: Option<String>,
}

/// How long a pass may run before it is no longer counted as "indexing" — a pass that died with
/// the process must not leave a screen saying "indexing, started 3 days ago".
pub const REINDEX_RUNNING_WINDOW_MINUTES: i64 = 5;

/// How long a provider may go without a write before its state reads `stale`.
pub const STALE_AFTER_HOURS: i64 = 24;

/// Decide a provider's state from what the indexer actually did.
///
/// The order matters: a running pass beats everything (the screen polls exactly then), a failed
/// pass beats a document count (the count is from the last good pass), and `empty` beats `stale`
/// (nothing to be stale about).
#[must_use]
pub fn provider_state(
    documents: i64,
    last_indexed_at: Option<OffsetDateTime>,
    last_run: Option<&ReindexRun>,
    now: OffsetDateTime,
) -> &'static str {
    if let Some(run) = last_run {
        if run.finished_at.is_none()
            && run.started_at > now - time::Duration::minutes(REINDEX_RUNNING_WINDOW_MINUTES)
        {
            return "indexing";
        }
        if run.finished_at.is_some() && run.error.is_some() {
            return "failed";
        }
    }
    if documents == 0 {
        return "empty";
    }
    match last_indexed_at {
        Some(at) if at < now - time::Duration::hours(STALE_AFTER_HOURS) => "stale",
        _ => "ready",
    }
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
/// line up; it is the single place the scope and permission rules live. `omit` drops exactly one
/// filter kind, which is how a facet's own counts are computed without the facet narrowing itself.
fn push_conditions(
    builder: &mut QueryBuilder<'_, Postgres>,
    request: &SearchRequest,
    omit: Option<FilterKind>,
) {
    push_conditions_with(builder, request, &request.providers, omit);
}

/// The same conditions, with the provider scope replaced.
///
/// Every reader of the index scopes by the providers the caller's own keys cover. The count of
/// what lies *outside* that scope ([`hidden_count`]) is the one caller that needs the same
/// filters over a different provider list, so the scope is a parameter rather than a field read.
fn push_conditions_with(
    builder: &mut QueryBuilder<'_, Postgres>,
    request: &SearchRequest,
    providers: &[&'static str],
    omit: Option<FilterKind>,
) {
    builder.push("d.provider = any(");
    builder.push_bind(providers.to_vec());
    builder.push("::text[])");

    builder.push(" and (");
    builder.push_bind(request.organization_id);
    builder.push("::uuid is null or d.organization_id = ");
    builder.push_bind(request.organization_id);
    builder.push(")");

    let query = &request.query;
    let filters = &request.filters;

    if query.has_terms() {
        builder.push(" and (d.document @@ websearch_to_tsquery('simple', ");
        builder.push_bind(query.fts_input());
        builder.push(") or d.title ilike ");
        builder.push_bind(query.title_prefix());
        builder.push(" or d.title % ");
        builder.push_bind(query.raw().to_lowercase());
        builder.push(")");
    }

    if omit != Some(FilterKind::Types) {
        let mut types: Vec<String> = query.types().to_vec();
        for value in &filters.types {
            let value = value.trim().to_lowercase();
            if !value.is_empty() && !types.contains(&value) {
                types.push(value);
            }
        }
        if !types.is_empty() {
            builder.push(" and (d.provider = any(");
            builder.push_bind(types.clone());
            builder.push("::text[]) or d.entity_type = any(");
            builder.push_bind(types);
            builder.push("::text[]))");
        }
    }

    if omit != Some(FilterKind::Site) {
        // A site filter answers three spellings of the same thing: the site's id, its stable key
        // and any of its domain hosts. The facet rail sends the id, a person may type the key.
        let mut sites: Vec<String> = Vec::new();
        if let Some(site) = query.site() {
            sites.push(site.to_owned());
        }
        if let Some(site) = &filters.site {
            let site = site.trim().to_lowercase();
            if !site.is_empty() && !sites.contains(&site) {
                sites.push(site);
            }
        }
        if !sites.is_empty() {
            builder.push(" and (d.site_id::text = any(");
            builder.push_bind(sites.clone());
            builder.push("::text[]) or d.site_id in (select s.id from sites s where s.key = any(");
            builder.push_bind(sites.clone());
            builder.push(
                "::text[]) or exists (select 1 from site_domains dom where dom.site_id = s.id \
                 and dom.host = any(",
            );
            builder.push_bind(sites);
            builder.push("::text[]))))");
        }
    }

    if omit != Some(FilterKind::Owner) {
        let mut owners: Vec<Uuid> = Vec::new();
        if query.owner_me {
            owners.push(request.user_id);
        }
        if let Some(owner) = filters.owner_id {
            if !owners.contains(&owner) {
                owners.push(owner);
            }
        }
        if !owners.is_empty() {
            builder.push(" and d.owner_user_id = any(");
            builder.push_bind(owners);
            builder.push("::uuid[])");
        }
    }

    if omit != Some(FilterKind::Language) {
        if let Some(language) = &filters.language {
            builder.push(" and d.language = ");
            builder.push_bind(language.trim().to_lowercase());
        }
    }

    if omit != Some(FilterKind::Status) {
        let mut statuses: Vec<String> = query.flags().to_vec();
        if let Some(status) = &filters.status {
            let status = status.trim().to_lowercase();
            if !status.is_empty() && !statuses.contains(&status) {
                statuses.push(status);
            }
        }
        if !statuses.is_empty() {
            builder.push(" and d.tags && ");
            builder.push_bind(statuses);
            builder.push("::text[]");
        }
    }

    if omit != Some(FilterKind::Updated) {
        if let Some(range) = filters.updated {
            match range {
                UpdatedRange::Today => {
                    builder.push(" and d.entity_updated_at >= now() - interval '1 day'");
                }
                UpdatedRange::Week => {
                    builder.push(" and d.entity_updated_at >= now() - interval '7 days'");
                }
                UpdatedRange::Month => {
                    builder.push(" and d.entity_updated_at >= now() - interval '30 days'");
                }
                UpdatedRange::Older => {
                    builder.push(" and d.entity_updated_at < now() - interval '30 days'");
                }
                UpdatedRange::Never => {
                    builder.push(" and d.entity_updated_at is null");
                }
            };
        }
    }

    // The written dates and the parameters are both ranges and both apply: a query can carry
    // `after:2026-01-01` while the rail carries a bucket.
    let after = filters.after.or(query.after);
    if let Some(after) = after {
        builder.push(" and (d.entity_updated_at is not null and d.entity_updated_at >= ");
        builder.push_bind(after);
        builder.push(")");
    }

    let before = filters.before.or(query.before);
    if let Some(before) = before {
        builder.push(" and (d.entity_updated_at is not null and d.entity_updated_at < ");
        builder.push_bind(before);
        builder.push(")");
    }
}

/// One value of the facet rail: what a click applies, what a person reads, how much it would leave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FacetValue {
    /// The machine value (`pages`, a uuid, `draft`, `week`).
    pub value: String,
    /// The human label.
    pub label: String,
    /// Hits this value would leave under every other filter.
    pub count: i64,
}

/// One group of the facet rail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FacetGroup {
    /// Stable key (`type`, `site`, `owner`, `language`, `status`, `updated`).
    pub key: &'static str,
    /// Title the rail shows.
    pub title: &'static str,
    /// The values, strongest first.
    pub values: Vec<FacetValue>,
    /// How many further values exist beyond the ones listed.
    pub more: i64,
}

/// How many values one facet group lists before `more` takes over.
pub const FACET_LIMIT: usize = 12;

/// One planned facet: the SQL of its value and label, the joins it needs and the filter it must
/// leave out of its own counts.
struct FacetPlan {
    key: &'static str,
    title: &'static str,
    omit: FilterKind,
    /// A condition that belongs to the facet itself (`d.site_id is not null`), pushed right after
    /// `where` — a null value is not a facet value.
    prelude: &'static str,
    value: &'static str,
    label: &'static str,
    joins: &'static str,
}

/// The five column-backed facets. The sixth — `updated` — is a computed range and lives in
/// [`updated_facet`], because its value is a bucket of time rather than a column.
const FACET_PLANS: &[FacetPlan] = &[
    FacetPlan {
        key: "type",
        title: "Type",
        omit: FilterKind::Types,
        prelude: "",
        value: "d.provider",
        label: "d.provider",
        joins: "",
    },
    FacetPlan {
        key: "site",
        title: "Site",
        omit: FilterKind::Site,
        prelude: "d.site_id is not null and ",
        value: "d.site_id::text",
        label: "coalesce(s.name, d.site_id::text)",
        joins: "left join sites s on s.id = d.site_id",
    },
    FacetPlan {
        key: "owner",
        title: "Owner",
        omit: FilterKind::Owner,
        prelude: "d.owner_user_id is not null and ",
        value: "d.owner_user_id::text",
        label: "coalesce(nullif(ou.display_name, ''), ou.email, d.owner_user_id::text)",
        joins: "left join users ou on ou.id = d.owner_user_id",
    },
    FacetPlan {
        key: "language",
        title: "Language",
        omit: FilterKind::Language,
        prelude: "",
        value: "d.language",
        label: "d.language",
        joins: "",
    },
    FacetPlan {
        key: "status",
        title: "Status",
        omit: FilterKind::Status,
        prelude: "array_length(d.tags, 1) >= 1 and ",
        value: "facet_tag.tag",
        label: "facet_tag.tag",
        joins: "cross join lateral unnest(d.tags) as facet_tag(tag)",
    },
];

/// Row shape of a facet value.
#[derive(Debug, sqlx::FromRow)]
struct FacetRow {
    value: String,
    label: String,
    count: i64,
}

/// Every facet group of a request, each counted without its own filter.
pub async fn facets(pool: &PgPool, request: &SearchRequest) -> Result<Vec<FacetGroup>> {
    let mut groups: Vec<FacetGroup> = Vec::with_capacity(FACET_PLANS.len() + 1);
    for plan in FACET_PLANS {
        groups.push(facet_group(pool, request, plan).await?);
    }
    groups.push(updated_facet(pool, request).await?);
    Ok(groups)
}

/// Count one planned facet's values.
async fn facet_group(
    pool: &PgPool,
    request: &SearchRequest,
    plan: &FacetPlan,
) -> Result<FacetGroup> {
    let mut builder: QueryBuilder<'_, Postgres> = QueryBuilder::new(format!(
        "select {value} as value, {label} as label, count(*)::bigint as count \
         from search_documents d {joins} where {prelude}",
        value = plan.value,
        label = plan.label,
        joins = plan.joins,
        prelude = plan.prelude,
    ));
    push_conditions(&mut builder, request, Some(plan.omit));
    builder.push(format!(
        " group by 1, 2 order by count desc, 1 asc limit {}",
        FACET_LIMIT + 1
    ));
    let mut rows: Vec<FacetRow> = builder.build_query_as().fetch_all(pool).await?;

    let truncated = rows.len() > FACET_LIMIT;
    if truncated {
        rows.truncate(FACET_LIMIT);
    }
    let mut more = 0;
    if truncated {
        // How many further values exist — counted, never guessed, so "12 of 31" is true.
        let mut counter: QueryBuilder<'_, Postgres> = QueryBuilder::new(format!(
            "select count(*) from (select 1 from search_documents d {joins} where {prelude}",
            joins = plan.joins,
            prelude = plan.prelude,
        ));
        push_conditions(&mut counter, request, Some(plan.omit));
        counter.push(format!(
            " group by {value}) distinct_values",
            value = plan.value
        ));
        let total: i64 = counter.build_query_scalar().fetch_one(pool).await?;
        more = (total - rows.len() as i64).max(0);
    }

    Ok(FacetGroup {
        key: plan.key,
        title: plan.title,
        values: rows
            .into_iter()
            .map(|row| FacetValue {
                // The type facet's value is a provider key; its label is the registry's title, so
                // the rail reads "Pages", not "pages".
                label: if plan.key == "type" {
                    providers::provider(&row.value).map_or(row.label, |spec| spec.title.to_owned())
                } else {
                    row.label
                },
                value: row.value,
                count: row.count,
            })
            .collect(),
        more,
    })
}

/// The "Updated" facet: how many hits fall in each range of the last change.
async fn updated_facet(pool: &PgPool, request: &SearchRequest) -> Result<FacetGroup> {
    let bucket = "case when d.entity_updated_at is null then 'never' \
        when d.entity_updated_at >= now() - interval '1 day' then 'today' \
        when d.entity_updated_at >= now() - interval '7 days' then 'week' \
        when d.entity_updated_at >= now() - interval '30 days' then 'month' \
        else 'older' end";
    let mut builder: QueryBuilder<'_, Postgres> = QueryBuilder::new(format!(
        "select {bucket} as value, {bucket} as label, count(*)::bigint as count \
         from search_documents d where "
    ));
    push_conditions(&mut builder, request, Some(FilterKind::Updated));
    builder.push(" group by 1, 2");
    let mut rows: Vec<FacetRow> = builder.build_query_as().fetch_all(pool).await?;

    const ORDER: [&str; 5] = ["today", "week", "month", "older", "never"];
    rows.sort_by_key(|row| {
        ORDER
            .iter()
            .position(|value| *value == row.value)
            .unwrap_or(ORDER.len())
    });

    Ok(FacetGroup {
        key: "updated",
        title: "Updated",
        values: rows
            .into_iter()
            .map(|row| FacetValue {
                label: UpdatedRange::parse(&row.value)
                    .map_or(row.label, |range| range.label().to_owned()),
                value: row.value,
                count: row.count,
            })
            .collect(),
        more: 0,
    })
}

/// Run a query and answer one page of hits plus the total.
pub async fn search(pool: &PgPool, request: &SearchRequest) -> Result<HitPage> {
    let weights = weights(pool).await?;
    let per_page = request.per_page.clamp(1, MAX_PER_PAGE);
    let page = request.page.max(1);
    let offset = (page - 1) * per_page;

    let mut builder: QueryBuilder<'_, Postgres> = QueryBuilder::new(
        "select d.provider, d.entity_type, d.entity_id, d.title, d.subtitle, d.url, \
         ou.display_name as owner, d.tags, d.entity_updated_at, ",
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
    builder.push(
        " as score, count(*) over () as total from search_documents d \
         left join users ou on ou.id = d.owner_user_id where ",
    );
    push_conditions(&mut builder, request, None);

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
    owner: Option<String>,
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
            owner: self.owner,
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
    push_conditions(&mut builder, request, None);
    let total: i64 = builder.build_query_scalar().fetch_one(pool).await?;
    Ok(total)
}

/// How many hits each provider contributes to a query — the palette's section counts.
pub async fn counts(pool: &PgPool, request: &SearchRequest) -> Result<Vec<ProviderCount>> {
    let mut builder: QueryBuilder<'_, Postgres> = QueryBuilder::new(
        "select d.provider as provider, count(*)::bigint as count from search_documents d where ",
    );
    push_conditions(&mut builder, request, None);
    builder.push(" group by d.provider order by count desc, d.provider asc");
    let rows: Vec<ProviderCount> = builder.build_query_as().fetch_all(pool).await?;
    Ok(rows)
}

/// How many documents a query matches outside the caller's own read scope.
///
/// The answer to "was the search really empty, or is the result hidden from me?" — a count and
/// nothing else: no titles, no ids and no per-provider split, because naming what lies outside a
/// reader's scope would hand them the index anyway. The caller's own filters (the `type:` values
/// included) still narrow it, so the number matches the search the reader actually made. A caller
/// who covers every enabled provider is answered `0` without a query being run — nothing is
/// hidden from them by definition.
pub async fn hidden_count(pool: &PgPool, request: &SearchRequest) -> Result<i64> {
    let hidden = unreadable_providers(pool, &request.providers).await?;
    if hidden.is_empty() {
        return Ok(0);
    }
    let mut builder: QueryBuilder<'_, Postgres> =
        QueryBuilder::new("select count(*) from search_documents d where ");
    push_conditions_with(&mut builder, request, &hidden, None);
    let total: i64 = builder.build_query_scalar().fetch_one(pool).await?;
    Ok(total)
}

/// The registered providers the installation has enabled that the caller's own read set does not
/// cover — the scope a search answers around rather than in.
async fn unreadable_providers(
    pool: &PgPool,
    readable: &[&'static str],
) -> Result<Vec<&'static str>> {
    let enabled = enabled_providers(pool).await?;
    Ok(providers::PROVIDERS
        .iter()
        .filter(|spec| enabled.iter().any(|key| key == spec.key) && !readable.contains(&spec.key))
        .map(|spec| spec.key)
        .collect())
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

    // The newest pass of each provider. A pass that never finished within the running window
    // reads as indexing; one that failed keeps saying so until a later pass succeeds.
    let runs: Vec<RunRow> = sqlx::query_as(
        "select r.provider, r.indexed, r.pruned, r.duration_ms, r.started_at, r.finished_at, \
         r.error from search_reindex_runs r \
         where r.id = (select max(id) from search_reindex_runs where provider = r.provider)",
    )
    .fetch_all(pool)
    .await?;

    let now = OffsetDateTime::now_utc();
    Ok(providers::PROVIDERS
        .iter()
        .map(|spec| {
            let row = rows.iter().find(|(key, _, _)| key == spec.key);
            let documents = row.map_or(0, |(_, count, _)| *count);
            let last_indexed_at = row.and_then(|(_, _, at)| *at);
            let last_run = runs
                .iter()
                .find(|run| run.provider == spec.key)
                .map(RunRow::to_run);
            ProviderStatus {
                provider: spec.key,
                title: spec.title,
                documents,
                last_indexed_at,
                state: provider_state(documents, last_indexed_at, last_run.as_ref(), now),
                last_run,
            }
        })
        .collect())
}

/// Row shape of the newest reindex pass of one provider.
#[derive(Debug, sqlx::FromRow)]
struct RunRow {
    provider: String,
    indexed: Option<i64>,
    pruned: Option<i64>,
    duration_ms: Option<i64>,
    started_at: OffsetDateTime,
    finished_at: Option<OffsetDateTime>,
    error: Option<String>,
}

impl RunRow {
    /// The pass as the status screen reads it.
    fn to_run(&self) -> ReindexRun {
        ReindexRun {
            indexed: self.indexed,
            pruned: self.pruned,
            duration_ms: self.duration_ms,
            started_at: self.started_at,
            finished_at: self.finished_at,
            error: self.error.clone(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Settings: the ranking weights and the enabled providers
// ---------------------------------------------------------------------------------------------

/// The four ranking weights, as a person writes them (`0..=10`, integers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Weights {
    /// Weight of a title match.
    pub title: i32,
    /// Weight of a tag match.
    pub tags: i32,
    /// Weight of a subtitle match.
    pub subtitle: i32,
    /// Weight of a body match.
    pub body: i32,
}

/// The weights an installation starts with.
pub const DEFAULT_WEIGHTS: Weights = Weights {
    title: 6,
    tags: 4,
    subtitle: 3,
    body: 1,
};

/// The installation's search settings as the screen reads and writes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSettings {
    /// Ranking weights.
    pub weights: Weights,
    /// Provider keys that answer a query.
    pub enabled_providers: Vec<String>,
    /// When the row was last written.
    pub updated_at: Option<OffsetDateTime>,
}

/// Why a settings write was refused — each variant is a sentence the screen can show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsError {
    /// A weight outside `0..=10`.
    OutOfRange(&'static str),
    /// The body outweighs the title, which would rank a mention above a name.
    TitleBelowBody,
    /// A provider key no build knows.
    UnknownProvider(String),
    /// Every provider switched off — search would answer nothing.
    NoProviders,
}

impl SettingsError {
    /// The stable code the API answers with.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::OutOfRange(_) => "weight_out_of_range",
            Self::TitleBelowBody => "title_below_body",
            Self::UnknownProvider(_) => "unknown_provider",
            Self::NoProviders => "no_providers",
        }
    }
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfRange(name) => write!(
                formatter,
                "the {name} weight must be a whole number between 0 and 10"
            ),
            Self::TitleBelowBody => write!(
                formatter,
                "the title weight must be at least the body weight — a name outranks a mention"
            ),
            Self::UnknownProvider(key) => {
                write!(formatter, "{key} is not a search provider this build knows")
            }
            Self::NoProviders => write!(
                formatter,
                "at least one provider must stay enabled, or search would answer nothing"
            ),
        }
    }
}

impl std::error::Error for SettingsError {}

/// Hold a settings value to the rules the form promises.
pub fn validate_settings(settings: &SearchSettings) -> std::result::Result<(), SettingsError> {
    let weights = settings.weights;
    for (name, value) in [
        ("title", weights.title),
        ("tags", weights.tags),
        ("subtitle", weights.subtitle),
        ("body", weights.body),
    ] {
        if !(0..=10).contains(&value) {
            return Err(SettingsError::OutOfRange(name));
        }
    }
    if weights.title < weights.body {
        return Err(SettingsError::TitleBelowBody);
    }
    if settings.enabled_providers.is_empty() {
        return Err(SettingsError::NoProviders);
    }
    for key in &settings.enabled_providers {
        if providers::provider(key).is_none() {
            return Err(SettingsError::UnknownProvider(key.clone()));
        }
    }
    Ok(())
}

/// Read the installation's settings; a missing row answers the defaults.
pub async fn read_settings(pool: &PgPool) -> Result<SearchSettings> {
    let row: Option<(serde_json::Value, Vec<String>, Option<OffsetDateTime>)> = sqlx::query_as(
        "select weights, enabled_providers, updated_at from search_settings where id = 1",
    )
    .fetch_optional(pool)
    .await?;

    let Some((weights, enabled, updated_at)) = row else {
        return Ok(SearchSettings {
            weights: DEFAULT_WEIGHTS,
            enabled_providers: providers::provider_keys()
                .into_iter()
                .map(str::to_owned)
                .collect(),
            updated_at: None,
        });
    };

    let read = |key: &str, fallback: i32| -> i32 {
        weights
            .get(key)
            .and_then(serde_json::Value::as_i64)
            .map_or(fallback, |value| value as i32)
    };

    Ok(SearchSettings {
        weights: Weights {
            title: read("title", DEFAULT_WEIGHTS.title),
            tags: read("tags", DEFAULT_WEIGHTS.tags),
            subtitle: read("subtitle", DEFAULT_WEIGHTS.subtitle),
            body: read("body", DEFAULT_WEIGHTS.body),
        },
        enabled_providers: enabled,
        updated_at,
    })
}

/// Write the installation's settings and answer the stored row.
pub async fn write_settings(pool: &PgPool, settings: &SearchSettings) -> Result<SearchSettings> {
    let weights = serde_json::json!({
        "title": settings.weights.title,
        "tags": settings.weights.tags,
        "subtitle": settings.weights.subtitle,
        "body": settings.weights.body,
    });
    sqlx::query(
        "insert into search_settings (id, weights, enabled_providers) values (1, $1, $2) \
         on conflict (id) do update set weights = excluded.weights, \
         enabled_providers = excluded.enabled_providers, updated_at = now()",
    )
    .bind(&weights)
    .bind(settings.enabled_providers.clone())
    .execute(pool)
    .await?;
    read_settings(pool).await
}

/// The provider keys a query may answer from: enabled by the settings **and** known to the build.
pub async fn enabled_providers(pool: &PgPool) -> Result<Vec<String>> {
    let settings = read_settings(pool).await?;
    Ok(settings
        .enabled_providers
        .into_iter()
        .filter(|key| providers::provider(key).is_some())
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

    #[test]
    fn updated_ranges_parse_and_read_like_a_person_wrote_them() {
        assert_eq!(UpdatedRange::parse("today"), Some(UpdatedRange::Today));
        assert_eq!(UpdatedRange::parse(" WEEK "), Some(UpdatedRange::Week));
        assert_eq!(UpdatedRange::parse("never"), Some(UpdatedRange::Never));
        assert_eq!(UpdatedRange::parse("century"), None);
        assert_eq!(UpdatedRange::Today.as_str(), "today");
        assert_eq!(UpdatedRange::Older.label(), "Older than a month");
    }

    #[test]
    fn a_providers_state_is_read_from_what_the_indexer_did() {
        let now = OffsetDateTime::now_utc();

        // A pass that is still running.
        let running = ReindexRun {
            indexed: None,
            pruned: None,
            duration_ms: None,
            started_at: now - time::Duration::seconds(3),
            finished_at: None,
            error: None,
        };
        assert_eq!(
            provider_state(4, Some(now), Some(&running), now),
            "indexing"
        );

        // A pass that died with the process must not say "indexing" forever.
        let abandoned = ReindexRun {
            started_at: now - time::Duration::hours(2),
            ..running.clone()
        };
        assert_eq!(provider_state(4, Some(now), Some(&abandoned), now), "ready");

        // A failed pass beats a document count from the last good one.
        let failed = ReindexRun {
            finished_at: Some(now),
            error: Some("connection closed".to_owned()),
            ..running.clone()
        };
        assert_eq!(provider_state(4, Some(now), Some(&failed), now), "failed");

        // Empty beats stale; stale beats ready.
        assert_eq!(provider_state(0, None, None, now), "empty");
        assert_eq!(
            provider_state(4, Some(now - time::Duration::hours(26)), None, now),
            "stale"
        );
        assert_eq!(
            provider_state(4, Some(now - time::Duration::minutes(5)), None, now),
            "ready"
        );
    }

    /// A settings value with the production defaults, for the validation tests.
    fn default_settings() -> SearchSettings {
        SearchSettings {
            weights: DEFAULT_WEIGHTS,
            enabled_providers: providers::provider_keys()
                .into_iter()
                .map(str::to_owned)
                .collect(),
            updated_at: None,
        }
    }

    #[test]
    fn settings_validation_names_what_is_wrong() {
        assert!(validate_settings(&default_settings()).is_ok());

        let mut too_big = default_settings();
        too_big.weights.body = 11;
        assert_eq!(
            validate_settings(&too_big).map_err(|error| error.code()),
            Err("weight_out_of_range")
        );

        let mut body_heavy = default_settings();
        body_heavy.weights.title = 2;
        body_heavy.weights.body = 5;
        assert_eq!(
            validate_settings(&body_heavy).map_err(|error| error.code()),
            Err("title_below_body")
        );

        let mut unknown = default_settings();
        unknown.enabled_providers.push("unicorns".to_owned());
        assert_eq!(
            validate_settings(&unknown).map_err(|error| error.code()),
            Err("unknown_provider")
        );

        let mut none = default_settings();
        none.enabled_providers.clear();
        assert_eq!(
            validate_settings(&none).map_err(|error| error.code()),
            Err("no_providers")
        );
    }

    #[test]
    fn every_facet_plan_names_its_own_filter_and_sql() {
        for plan in FACET_PLANS {
            assert!(!plan.key.is_empty() && !plan.title.is_empty());
            assert!(!plan.value.is_empty() && !plan.label.is_empty());
            assert!(
                plan.value.starts_with("d.") || plan.value.starts_with("facet_tag."),
                "{} must read a column of the document",
                plan.key
            );
        }
        let keys: Vec<&str> = FACET_PLANS.iter().map(|plan| plan.key).collect();
        assert_eq!(keys, vec!["type", "site", "owner", "language", "status"]);
    }
}
