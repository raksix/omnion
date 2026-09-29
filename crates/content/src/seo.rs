//! The SEO toolkit: page metadata, redirect rules, the sitemap and the broken-link view
//! (REQ-064, slice 3).
//!
//! This is the only part of the CMS a crawler reads on every single page view, and that fact
//! decides the whole shape of the module. A visitor's request must never wait for a generator,
//! a crawl or a regex engine over the whole rule set, so:
//!
//! * **Metadata is columns, and the generated tags are derived.** The owner types a title, a
//!   description, an OG image and a schema type; the store builds the `<meta>` set and the
//!   JSON-LD body from the page's *own* fields. A hand-written JSON-LD block is the single
//!   most common way a CMS emits a tag every consumer silently ignores.
//!
//! * **Redirect evaluation is literal-first, and loops are refused at write time.** A rule that
//!   sends `/a` to `/b` while `/b` sends back to `/a` is not a thing the panel should discover
//!   by having a visitor's browser spin; [`save_redirect`] walks the target chain and refuses
//!   the rule that would close it.
//!
//! * **The pattern dialect is deliberately small.** A path arriving from the internet is matched
//!   against a pattern an owner typed, so the matcher supports literal text, `.` (one character),
//!   `*` (zero or more of the previous character) and `?` (optional) — anchored to the whole
//!   path, no alternation, no backtracking. A full regex engine here is a denial-of-service
//!   surface one owner can open on every request the site serves.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{ContentError, Result};
use crate::validation::validate_optional_text;

/// The schema.org types the generator can build, in the order the picker shows them.
///
/// A closed list on purpose: the panel's JSON-LD preview is only worth showing if the preview is
/// of something a consumer will actually read.
pub const STRUCTURED_DATA_TYPES: [&str; 6] = [
    "Article",
    "Organization",
    "FAQPage",
    "Product",
    "BreadcrumbList",
    "WebSite",
];

/// Twitter card variants the platform emits.
pub const TWITTER_CARDS: [&str; 2] = ["summary", "summary_large_image"];

/// `robots` directives the editor may compose.
pub const ROBOTS_DIRECTIVES: [&str; 4] = ["index", "noindex", "follow", "nofollow"];

/// How often a URL changes, for the sitemap.
pub const CHANGE_FREQUENCIES: [&str; 7] = [
    "always", "hourly", "daily", "weekly", "monthly", "yearly", "never",
];

/// Redirect rule kinds.
pub const REDIRECT_PATTERNS: [&str; 2] = ["literal", "regex"];

/// Redirect codes the manager offers.
pub const REDIRECT_STATUS_CODES: [i32; 2] = [301, 302];

/// Longest accepted SEO title. Google's own truncation is ~60 characters, but the field holds
/// what the owner typed; the *preview* is what warns.
pub const MAX_SEO_TITLE_LENGTH: usize = 255;

/// Longest accepted meta description.
pub const MAX_SEO_DESCRIPTION_LENGTH: usize = 500;

/// Longest accepted OG title/description (the platform's own text, not the page's).
pub const MAX_OG_TEXT_LENGTH: usize = 255;

/// Longest accepted canonical URL.
pub const MAX_URL_LENGTH: usize = 2_000;

/// Longest accepted robots.txt.
pub const MAX_ROBOTS_TXT_LENGTH: usize = 32_000;

/// Most redirects one site may hold.
///
/// A cap rather than a rule because a site with 20,000 redirects has a performance problem that
/// no index fixes, and the panel can say so instead of the site getting slower every week.
pub const MAX_REDIRECTS_PER_SITE: usize = 5_000;

/// The columns a redirect row is read with.
const REDIRECT_COLUMNS: &str = "id, organization_id, site_id, from_path, to_path, status_code, \
     pattern, enabled, hits, last_hit_at, created_by, created_at, updated_at";

/// One redirect rule.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Redirect {
    /// Primary key.
    pub id: Uuid,
    /// Owning organization.
    pub organization_id: Uuid,
    /// Site the rule belongs to.
    pub site_id: Uuid,
    /// Site-relative path the rule answers, without a query string.
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
    pub last_hit_at: Option<OffsetDateTime>,
    /// Author, when a person created it.
    pub created_by: Option<Uuid>,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

/// A redirect rule as the editor submits it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewRedirect {
    /// Site the rule belongs to.
    pub site_id: Uuid,
    /// Path the rule answers.
    pub from_path: String,
    /// Destination.
    pub to_path: String,
    /// 301 or 302; the default is a permanent move.
    pub status_code: Option<i32>,
    /// `literal` or `regex`.
    pub pattern: Option<String>,
    /// Whether it is evaluated.
    pub enabled: Option<bool>,
    /// Author.
    pub created_by: Option<Uuid>,
}

/// One broken internal link.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct BrokenLink {
    /// Primary key.
    pub id: Uuid,
    /// Site the link was found in.
    pub site_id: Uuid,
    /// Page carrying the link, when the crawl could attribute it.
    pub source_page_id: Option<Uuid>,
    /// The target as written in the body.
    pub target_url: String,
    /// The anchor text, when the link had any.
    pub anchor_text: Option<String>,
    /// HTTP status the checker saw, when it was allowed to ask.
    pub status: Option<i32>,
    /// Whether the owner dismissed it.
    pub ignored: bool,
    /// When the crawl saw it.
    pub found_at: OffsetDateTime,
}

/// Per-site SEO settings.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct SeoSettings {
    /// The site these settings belong to.
    pub site_id: Uuid,
    /// Owning organization.
    pub organization_id: Uuid,
    /// Page types the sitemap includes.
    pub sitemap_types: Vec<String>,
    /// Default priority for pages that have none.
    pub default_priority: f64,
    /// Default change frequency.
    pub default_change_frequency: String,
    /// The generated XML, stored rather than rebuilt per request.
    pub sitemap_xml: Option<String>,
    /// When it was last generated.
    pub sitemap_last_generated_at: Option<OffsetDateTime>,
    /// The site's robots.txt.
    pub robots_txt: String,
    /// Who last changed it.
    pub updated_by: Option<Uuid>,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

/// A page's SEO fields, as the editor reads and writes them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageSeo {
    /// Title override. Empty means "use the page's own title".
    #[serde(default)]
    pub seo_title: Option<String>,
    /// Description override.
    #[serde(default)]
    pub seo_description: Option<String>,
    /// Absolute canonical URL.
    #[serde(default)]
    pub canonical_url: Option<String>,
    /// OG title.
    #[serde(default)]
    pub og_title: Option<String>,
    /// OG description.
    #[serde(default)]
    pub og_description: Option<String>,
    /// Media the OG card points at.
    #[serde(default)]
    pub og_image_media_id: Option<Uuid>,
    /// `summary` or `summary_large_image`.
    #[serde(default)]
    pub twitter_card: String,
    /// Comma-separated directives.
    #[serde(default)]
    pub robots: String,
    /// One of [`STRUCTURED_DATA_TYPES`], or none.
    #[serde(default)]
    pub structured_data_type: Option<String>,
    /// Extra values merged into the generated JSON-LD (`headline`, `sku`, `faq`, …).
    #[serde(default)]
    pub structured_data: Value,
}

impl PageSeo {
    /// The Twitter card in effect, with the column default applied for a payload that omitted
    /// it. A blank string is a caller that sent `""`, and the column has a non-null default —
    /// so the blank is replaced rather than stored, in the same place as every other default.
    #[must_use]
    pub fn effective_twitter_card(&self) -> &str {
        if TWITTER_CARDS.contains(&self.twitter_card.as_str()) {
            &self.twitter_card
        } else {
            "summary_large_image"
        }
    }

    /// Whether the page asks to be indexed at all. `noindex` anywhere in the directive list
    /// wins, because a crawler reads `index` and `noindex` as a contradiction and picks by
    /// order; the editor's list is normalized so both cannot be present.
    #[must_use]
    pub fn is_indexable(&self) -> bool {
        !self
            .robots
            .split(',')
            .map(|directive| directive.trim().to_ascii_lowercase())
            .any(|directive| directive == "noindex")
    }
}

/// The `<meta>` set the renderer emits for one page, with the tags already built.
///
/// The panel's SERP preview reads this rather than re-deriving the rules, so what the editor
/// sees is literally what a crawler reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeoTags {
    /// The title a search result shows.
    pub title: String,
    /// The description a search result shows.
    pub description: Option<String>,
    /// The canonical URL.
    pub canonical: Option<String>,
    /// Open Graph title.
    pub og_title: String,
    /// Open Graph description.
    pub og_description: Option<String>,
    /// Open Graph image URL, when the site has a media URL for it.
    pub og_image: Option<String>,
    /// Open Graph type — always `website` for a page.
    pub og_type: String,
    /// Twitter card variant.
    pub twitter_card: String,
    /// `index` or `noindex`.
    pub robots: String,
    /// The generated JSON-LD, as a string, or `None` when the page has no schema type.
    pub json_ld: Option<String>,
    /// Fields the chosen schema type wants and this page cannot supply, for the checklist.
    pub missing_fields: Vec<String>,
}

impl SeoTags {
    /// The title a search engine truncates at.
    pub const SERP_TITLE_LIMIT: usize = 60;
    /// The description a search engine truncates at.
    pub const SERP_DESCRIPTION_LIMIT: usize = 160;

    /// Whether the title fits in a search result without an ellipsis.
    #[must_use]
    pub fn title_fits(&self) -> bool {
        self.title.chars().count() <= Self::SERP_TITLE_LIMIT
    }

    /// Whether the description fits.
    #[must_use]
    pub fn description_fits(&self) -> bool {
        self.description
            .as_ref()
            .is_none_or(|value| value.chars().count() <= Self::SERP_DESCRIPTION_LIMIT)
    }
}

/// Everything the crawler needs about one page to build the sitemap and the JSON-LD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageSeoSource {
    /// The page.
    pub page_id: Uuid,
    /// Site the page belongs to.
    pub site_id: Uuid,
    /// Site-relative path, without a leading host.
    pub slug: String,
    /// Page type (`page`, `post`, …) — the sitemap's grouping key.
    pub page_type: String,
    /// The published revision's title.
    pub title: String,
    /// The published revision's summary, when it has one.
    pub summary: Option<String>,
    /// When the published revision was frozen.
    pub updated_at: OffsetDateTime,
    /// The site's primary host, for absolute URLs.
    pub site_host: String,
}

/// The SEO store.
#[derive(Debug, Clone)]
pub struct SeoStore {
    pool: PgPool,
}

impl SeoStore {
    /// A store over `pool`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    // -----------------------------------------------------------------------------------------
    // Page metadata
    // -----------------------------------------------------------------------------------------

    /// Read one page's SEO fields.
    ///
    /// Scoped by `site_id` in the `where` clause rather than resolved and then checked: a
    /// "read it, then is it mine?" sequence is an existence oracle, and the whole point of
    /// `entry_in_scope` in `menus.rs` applies here too.
    pub async fn read_page_seo(&self, site_id: Uuid, page_id: Uuid) -> Result<PageSeo> {
        let row = sqlx::query_as::<
            _,
            (
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<Uuid>,
                String,
                String,
                Option<String>,
                Value,
            ),
        >(
            "select seo_title, seo_description, canonical_url, og_title, og_description, \
                    og_image_media_id, twitter_card, robots, structured_data_type, structured_data \
             from pages where id = $1 and site_id = $2",
        )
        .bind(page_id)
        .bind(site_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(ContentError::PageNotFound)?;

        Ok(PageSeo {
            seo_title: row.0,
            seo_description: row.1,
            canonical_url: row.2,
            og_title: row.3,
            og_description: row.4,
            og_image_media_id: row.5,
            twitter_card: row.6,
            robots: row.7,
            structured_data_type: row.8,
            structured_data: row.9,
        })
    }

    /// Write one page's SEO fields.
    pub async fn write_page_seo(&self, site_id: Uuid, page_id: Uuid, seo: &PageSeo) -> Result<()> {
        let seo = validate_page_seo(seo)?;

        // `where id = $1 and site_id = $2` with a `rows_affected` check, NOT a select-then-update.
        // The select-then-update version reports success for a page that belongs to another site
        // when nothing is written at all, and the panel then shows the owner their new title on a
        // page that never changed.
        let result = sqlx::query(
            "update pages set \
                    seo_title = $3, seo_description = $4, canonical_url = $5, \
                    og_title = $6, og_description = $7, og_image_media_id = $8, \
                    twitter_card = $9, robots = $10, \
                    structured_data_type = $11, structured_data = $12, \
                    updated_at = now() \
             where id = $1 and site_id = $2",
        )
        .bind(page_id)
        .bind(site_id)
        .bind(seo.seo_title.as_deref())
        .bind(seo.seo_description.as_deref())
        .bind(seo.canonical_url.as_deref())
        .bind(seo.og_title.as_deref())
        .bind(seo.og_description.as_deref())
        .bind(seo.og_image_media_id)
        .bind(seo.effective_twitter_card())
        .bind(seo.robots.as_str())
        .bind(seo.structured_data_type.as_deref())
        .bind(&seo.structured_data)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(ContentError::PageNotFound);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Tag generation
    // -----------------------------------------------------------------------------------------

    /// Build the tag set for one page.
    ///
    /// Pure: it takes the page's own fields plus the stored metadata and returns the tags. That
    /// is what lets the panel preview the exact output, and it is why there is no second
    /// implementation of "what does a search engine see" in the admin app.
    pub fn build_tags(
        source: &PageSeoSource,
        seo: &PageSeo,
        og_image_url: Option<&str>,
    ) -> SeoTags {
        let title = seo
            .seo_title
            .clone()
            .unwrap_or_else(|| source.title.clone());
        let description = seo
            .seo_description
            .clone()
            .or_else(|| source.summary.clone());
        let og_title = seo.og_title.clone().unwrap_or_else(|| title.clone());
        let og_description = seo.og_description.clone().or_else(|| description.clone());
        let robots = if seo.is_indexable() {
            "index,follow".to_string()
        } else {
            "noindex,follow".to_string()
        };

        let (json_ld, missing_fields) = match &seo.structured_data_type {
            Some(kind) => {
                let (body, missing) =
                    build_structured_data(kind, source, seo, &title, description.as_deref());
                (serde_json::to_string(&body).ok(), missing)
            }
            None => (None, Vec::new()),
        };

        SeoTags {
            title,
            description,
            canonical: seo.canonical_url.clone(),
            og_title,
            og_description,
            og_image: og_image_url.map(str::to_string),
            og_type: "website".to_string(),
            twitter_card: seo.effective_twitter_card().to_string(),
            robots,
            json_ld,
            missing_fields,
        }
    }

    // -----------------------------------------------------------------------------------------
    // Redirects
    // -----------------------------------------------------------------------------------------

    /// List a site's redirect rules, newest first.
    pub async fn list_redirects(&self, site_id: Uuid) -> Result<Vec<Redirect>> {
        let sql = format!(
            "select {REDIRECT_COLUMNS} from cms_seo_redirects where site_id = $1 order by created_at desc"
        );
        Ok(sqlx::query_as::<_, Redirect>(&sql)
            .bind(site_id)
            .fetch_all(&self.pool)
            .await?)
    }

    /// Create a redirect rule.
    ///
    /// Refuses, before writing, the three ways a redirect manager breaks a site: a `from` that
    /// is not a site-relative path, a chain that would loop, and a fifth thousandth rule.
    pub async fn create_redirect(&self, new: &NewRedirect) -> Result<Redirect> {
        let from_path = validate_redirect_path(&new.from_path, "from")?;
        let to_path = validate_redirect_path(&new.to_path, "to")?;
        let status_code = new.status_code.unwrap_or(301);
        if !REDIRECT_STATUS_CODES.contains(&status_code) {
            return Err(ContentError::InvalidRedirect(format!(
                "status code {status_code} is not 301 or 302"
            )));
        }
        let pattern = new.pattern.as_deref().unwrap_or("literal").to_string();
        if !REDIRECT_PATTERNS.contains(&pattern.as_str()) {
            return Err(ContentError::InvalidRedirect(format!(
                "pattern {pattern:?} is not literal or regex"
            )));
        }
        if from_path == to_path {
            return Err(ContentError::InvalidRedirect(
                "a rule cannot send a path to itself".to_string(),
            ));
        }
        if pattern == "regex" {
            validate_pattern(&from_path)?;
        }

        let count: i64 =
            sqlx::query_scalar("select count(*) from cms_seo_redirects where site_id = $1")
                .bind(new.site_id)
                .fetch_one(&self.pool)
                .await?;
        if count as usize >= MAX_REDIRECTS_PER_SITE {
            return Err(ContentError::InvalidRedirect(format!(
                "this site already holds {MAX_REDIRECTS_PER_SITE} redirect rules"
            )));
        }

        self.refuse_loop(new.site_id, &from_path, &to_path).await?;

        let sql = format!(
            "insert into cms_seo_redirects \
                 (organization_id, site_id, from_path, to_path, status_code, pattern, enabled, created_by) \
             select s.organization_id, $1, $2, $3, $4, $5, $6, $7 from sites s where s.id = $1 \
             returning {REDIRECT_COLUMNS}"
        );
        match sqlx::query_as::<_, Redirect>(&sql)
            .bind(new.site_id)
            .bind(&from_path)
            .bind(&to_path)
            .bind(status_code)
            .bind(&pattern)
            .bind(new.enabled.unwrap_or(true))
            .bind(new.created_by)
            .fetch_optional(&self.pool)
            .await?
        {
            Some(rule) => Ok(rule),
            None => Err(ContentError::SiteNotFound),
        }
    }

    /// Replace a rule's fields.
    pub async fn update_redirect(
        &self,
        site_id: Uuid,
        id: Uuid,
        new: &NewRedirect,
    ) -> Result<Redirect> {
        let from_path = validate_redirect_path(&new.from_path, "from")?;
        let to_path = validate_redirect_path(&new.to_path, "to")?;
        let status_code = new.status_code.unwrap_or(301);
        if !REDIRECT_STATUS_CODES.contains(&status_code) {
            return Err(ContentError::InvalidRedirect(format!(
                "status code {status_code} is not 301 or 302"
            )));
        }
        let pattern = new.pattern.as_deref().unwrap_or("literal").to_string();
        if !REDIRECT_PATTERNS.contains(&pattern.as_str()) {
            return Err(ContentError::InvalidRedirect(format!(
                "pattern {pattern:?} is not literal or regex"
            )));
        }
        if from_path == to_path {
            return Err(ContentError::InvalidRedirect(
                "a rule cannot send a path to itself".to_string(),
            ));
        }
        if pattern == "regex" {
            validate_pattern(&from_path)?;
        }
        self.refuse_loop(site_id, &from_path, &to_path).await?;

        let sql = format!(
            "update cms_seo_redirects set from_path = $3, to_path = $4, status_code = $5, \
                    pattern = $6, enabled = $7, updated_at = now() \
             where id = $1 and site_id = $2 returning {REDIRECT_COLUMNS}"
        );
        sqlx::query_as::<_, Redirect>(&sql)
            .bind(id)
            .bind(site_id)
            .bind(&from_path)
            .bind(&to_path)
            .bind(status_code)
            .bind(&pattern)
            .bind(new.enabled.unwrap_or(true))
            .fetch_optional(&self.pool)
            .await?
            .ok_or(ContentError::RedirectNotFound)
    }

    /// Delete a rule.
    pub async fn delete_redirect(&self, site_id: Uuid, id: Uuid) -> Result<()> {
        let result = sqlx::query("delete from cms_seo_redirects where id = $1 and site_id = $2")
            .bind(id)
            .bind(site_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(ContentError::RedirectNotFound);
        }
        Ok(())
    }

    /// Resolve a request path against a site's enabled rules and count the hit.
    ///
    /// Literal rules are evaluated before regex rules, and the first match wins — the order is
    /// the panel's documented order, because a site that has both `/old` and `/old.*` needs to
    /// know which one answers. The counter is incremented in the same statement that returns
    /// the row, so a hit cannot be lost to a crash between the two.
    pub async fn resolve_redirect(&self, site_id: Uuid, path: &str) -> Result<Option<Redirect>> {
        let path = path.split('?').next().unwrap_or(path);
        let rules = sqlx::query_as::<_, Redirect>(&format!(
            "select {REDIRECT_COLUMNS} from cms_seo_redirects where site_id = $1 and enabled \
             order by case pattern when 'literal' then 0 else 1 end, created_at"
        ))
        .bind(site_id)
        .fetch_all(&self.pool)
        .await?;

        let found = rules.iter().find(|rule| {
            if rule.pattern == "regex" {
                matches_pattern(&rule.from_path, path)
            } else {
                rule.from_path == path
            }
        });
        let Some(rule) = found else {
            return Ok(None);
        };

        let counted = sqlx::query_as::<_, Redirect>(&format!(
            "update cms_seo_redirects set hits = hits + 1, last_hit_at = now() \
             where id = $1 returning {REDIRECT_COLUMNS}"
        ))
        .bind(rule.id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(counted)
    }

    /// Preview what a path would do, without counting a hit.
    ///
    /// The panel's `Test a path` button. It must not increment: a test is the owner asking a
    /// question, and an owner poking at their own rules twenty times should not leave twenty
    /// hits in the column they are reading to decide whether a rule is needed.
    pub async fn test_redirect(&self, site_id: Uuid, path: &str) -> Result<Option<Redirect>> {
        let path = path.split('?').next().unwrap_or(path);
        let rules = sqlx::query_as::<_, Redirect>(&format!(
            "select {REDIRECT_COLUMNS} from cms_seo_redirects where site_id = $1 and enabled \
             order by case pattern when 'literal' then 0 else 1 end, created_at"
        ))
        .bind(site_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rules.into_iter().find(|rule| {
            if rule.pattern == "regex" {
                matches_pattern(&rule.from_path, path)
            } else {
                rule.from_path == path
            }
        }))
    }

    /// The rules that answer the same path as `candidate`, excluding its own id.
    ///
    /// The panel warns *before* saving rather than after the fact: two matching rules is a
    /// configuration where the owner cannot tell which one fires, and the only place that is
    /// cheap to explain is the form they are typing in.
    pub async fn conflicting_redirects(
        &self,
        site_id: Uuid,
        from_path: &str,
        pattern: &str,
        exclude: Option<Uuid>,
    ) -> Result<Vec<Redirect>> {
        let rules = self.list_redirects(site_id).await?;
        Ok(rules
            .into_iter()
            .filter(|rule| Some(rule.id) != exclude)
            .filter(|rule| {
                if rule.pattern == pattern {
                    rule.from_path == from_path
                } else if pattern == "regex" {
                    matches_pattern(from_path, &rule.from_path)
                } else {
                    matches_pattern(&rule.from_path, from_path)
                }
            })
            .collect())
    }

    /// Walk the target chain and refuse a rule that would close a loop.
    ///
    /// The walk is literal-only and bounded: a *regex* rule's target is a fixed path, so the
    /// chain is followed through the literal rules from there. Two rules that send each other
    /// back and forth is the classic "the site is down and the 404 is also a redirect" outage.
    async fn refuse_loop(&self, site_id: Uuid, from_path: &str, to_path: &str) -> Result<()> {
        let rules: BTreeSet<(String, String)> = sqlx::query_as::<_, (String, String)>(
            "select from_path, to_path from cms_seo_redirects where site_id = $1 and enabled",
        )
        .bind(site_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .collect();

        let mut current = to_path.to_string();
        let mut seen = BTreeSet::new();
        seen.insert(from_path.to_string());
        // The cap is not decoration: a chain of 5,000 rules exists, and without it this walk is
        // a request-time denial of service the owner can cause by accident.
        for _ in 0..64 {
            if !seen.insert(current.clone()) {
                return Err(ContentError::RedirectLoop(format!(
                    "'{from_path}' would send a visitor in a circle through '{current}'"
                )));
            }
            let Some((_, next)) = rules.iter().find(|(from, _)| from == &current) else {
                return Ok(());
            };
            current = next.clone();
        }
        Err(ContentError::RedirectLoop(format!(
            "'{from_path}' would join a chain of more than 64 redirects"
        )))
    }

    // -----------------------------------------------------------------------------------------
    // Sitemap and robots.txt
    // -----------------------------------------------------------------------------------------

    /// Read a site's SEO settings, creating the row on first use.
    pub async fn read_settings(&self, site_id: Uuid) -> Result<SeoSettings> {
        if let Some(row) = sqlx::query_as::<_, SeoSettings>(
            "select site_id, organization_id, sitemap_types, default_priority, \
                    default_change_frequency, sitemap_xml, sitemap_last_generated_at, robots_txt, \
                    updated_by, updated_at from cms_seo_settings where site_id = $1",
        )
        .bind(site_id)
        .fetch_optional(&self.pool)
        .await?
        {
            return Ok(row);
        }
        // `on conflict do nothing` rather than `do update`: two panels opening a fresh site at
        // the same moment must not reset each other's settings on every read.
        sqlx::query(
            "insert into cms_seo_settings (site_id, organization_id) \
             select id, organization_id from sites where id = $1 \
             on conflict (site_id) do nothing",
        )
        .bind(site_id)
        .execute(&self.pool)
        .await?;

        sqlx::query_as::<_, SeoSettings>(
            "select site_id, organization_id, sitemap_types, default_priority, \
                    default_change_frequency, sitemap_xml, sitemap_last_generated_at, robots_txt, \
                    updated_by, updated_at from cms_seo_settings where site_id = $1",
        )
        .bind(site_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(ContentError::SiteNotFound)
    }

    /// Write the parts of a site's settings the editor may change.
    pub async fn write_settings(
        &self,
        site_id: Uuid,
        actor: Option<Uuid>,
        sitemap_types: &[String],
        default_priority: f64,
        default_change_frequency: &str,
        robots_txt: &str,
    ) -> Result<SeoSettings> {
        // Read first so a site that has never been configured gets its row; `write` alone
        // would update zero rows on a fresh site and report success.
        self.read_settings(site_id).await?;

        if !(0.0..=1.0).contains(&default_priority) {
            return Err(ContentError::InvalidSeo(format!(
                "priority {default_priority} is between 0.0 and 1.0"
            )));
        }
        if !CHANGE_FREQUENCIES.contains(&default_change_frequency) {
            return Err(ContentError::InvalidSeo(format!(
                "change frequency {default_change_frequency:?} is not one the sitemap understands"
            )));
        }
        if robots_txt.len() > MAX_ROBOTS_TXT_LENGTH {
            return Err(ContentError::InvalidSeo(format!(
                "robots.txt is {} bytes; the limit is {MAX_ROBOTS_TXT_LENGTH}",
                robots_txt.len()
            )));
        }
        // Warnings, not a refusal: a site that blocks itself is a real configuration, and the
        // panel says so loudly in the editor rather than refusing to save a file the owner can
        // export by hand.
        let _warnings = validate_robots_txt(robots_txt);

        sqlx::query_as::<_, SeoSettings>(
            "update cms_seo_settings set sitemap_types = $2, default_priority = $3, \
                    default_change_frequency = $4, robots_txt = $5, updated_by = $6, updated_at = now() \
             where site_id = $1 \
             returning site_id, organization_id, sitemap_types, default_priority, \
                    default_change_frequency, sitemap_xml, sitemap_last_generated_at, robots_txt, \
                    updated_by, updated_at",
        )
        .bind(site_id)
        .bind(sitemap_types)
        .bind(default_priority)
        .bind(default_change_frequency)
        .bind(robots_txt)
        .bind(actor)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(ContentError::SiteNotFound)
    }

    /// Generate the sitemap XML and store it, returning the settings row that holds it.
    ///
    /// Only pages whose `robots` is indexable are listed. A sitemap that advertises a page the
    /// site also asks crawlers to skip is a contradiction every crawler resolves in the site's
    /// favour, i.e. the wrong way.
    pub async fn regenerate_sitemap(
        &self,
        site_id: Uuid,
        actor: Option<Uuid>,
    ) -> Result<SeoSettings> {
        let settings = self.read_settings(site_id).await?;
        let included: Vec<String> = if settings.sitemap_types.is_empty() {
            Vec::new()
        } else {
            settings.sitemap_types.clone()
        };

        let host: String = sqlx::query_scalar(
            "select host from site_domains where site_id = $1 and is_primary \
                                union all \
                               select host from site_domains where site_id = $1 order by 1 limit 1",
        )
        .bind(site_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(ContentError::SiteNotFound)?;

        let rows: Vec<(String, String, OffsetDateTime, String)> = sqlx::query_as(
            "select slug, page_type, updated_at, coalesce(seo_title, (select title from page_revisions \
                    where id = pages.published_revision_id), '') \
             from pages where site_id = $1 and status = 'published' \
               and (cardinality($2::text[]) = 0 or page_type = any($2)) \
               and robots not like '%noindex%' \
             order by page_type, slug",
        )
        .bind(site_id)
        .bind(&included)
        .fetch_all(&self.pool)
        .await?;

        let xml = render_sitemap(
            &host,
            &rows,
            &settings.default_change_frequency,
            settings.default_priority,
        );

        sqlx::query_as::<_, SeoSettings>(
            "update cms_seo_settings set sitemap_xml = $2, sitemap_last_generated_at = now(), \
                    updated_by = $3, updated_at = now() where site_id = $1 \
             returning site_id, organization_id, sitemap_types, default_priority, \
                    default_change_frequency, sitemap_xml, sitemap_last_generated_at, robots_txt, \
                    updated_by, updated_at",
        )
        .bind(site_id)
        .bind(&xml)
        .bind(actor)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(ContentError::SiteNotFound)
    }

    // -----------------------------------------------------------------------------------------
    // Broken links
    // -----------------------------------------------------------------------------------------

    /// Record broken links found by the crawl-lite pass, replacing the site's unignored rows.
    ///
    /// A replace rather than an insert-or-ignore: a link that has been fixed must *leave* the
    /// list, and an insert-or-ignore can only ever add.
    pub async fn replace_broken_links(&self, site_id: Uuid, found: &[FoundLink]) -> Result<usize> {
        sqlx::query("delete from cms_broken_links where site_id = $1 and not ignored")
            .bind(site_id)
            .execute(&self.pool)
            .await?;

        let mut written = 0;
        for link in found {
            let result = sqlx::query(
                "insert into cms_broken_links (site_id, source_page_id, target_url, anchor_text, status) \
                 values ($1, $2, $3, $4, $5) on conflict do nothing",
            )
            .bind(site_id)
            .bind(link.source_page_id)
            .bind(&link.target_url)
            .bind(link.anchor_text.as_deref())
            .bind(link.status)
            .execute(&self.pool)
            .await?;
            written += result.rows_affected() as usize;
        }
        Ok(written)
    }

    /// List the site's broken links, newest first.
    pub async fn list_broken_links(
        &self,
        site_id: Uuid,
        include_ignored: bool,
    ) -> Result<Vec<BrokenLink>> {
        let sql = "select id, site_id, source_page_id, target_url, anchor_text, status, ignored, found_at \
                   from cms_broken_links where site_id = $1 \
                     and (not ignored or $2) order by found_at desc";
        Ok(sqlx::query_as::<_, BrokenLink>(sql)
            .bind(site_id)
            .bind(include_ignored)
            .fetch_all(&self.pool)
            .await?)
    }

    /// Dismiss a broken link, or bring a dismissed one back.
    pub async fn set_link_ignored(&self, site_id: Uuid, id: Uuid, ignored: bool) -> Result<()> {
        let result =
            sqlx::query("update cms_broken_links set ignored = $3 where id = $1 and site_id = $2")
                .bind(id)
                .bind(site_id)
                .bind(ignored)
                .execute(&self.pool)
                .await?;
        if result.rows_affected() == 0 {
            return Err(ContentError::BrokenLinkNotFound);
        }
        Ok(())
    }

    /// Crawl the site's own pages for internal links that do not resolve, and store the result.
    ///
    /// "Crawl-lite": the walk is over the *stored* page bodies, and a target is broken when it
    /// names no page of this site. Nothing is fetched over HTTP — this pass must not be able to
    /// turn the site into a load generator pointed at itself, and must not depend on the public
    /// router being up in order to run.
    pub async fn crawl_internal_links(&self, site_id: Uuid) -> Result<Vec<BrokenLink>> {
        let pages: Vec<(Uuid, String)> = sqlx::query_as(
            "select id, slug from pages where site_id = $1 and status = 'published'",
        )
        .bind(site_id)
        .fetch_all(&self.pool)
        .await?;

        let known: BTreeSet<String> = pages
            .iter()
            .map(|(_, slug)| format!("/{slug}"))
            .chain(std::iter::once("/".to_string()))
            .collect();

        let mut found: Vec<FoundLink> = Vec::new();
        for (page_id, _) in &pages {
            let body: Option<String> = sqlx::query_scalar(
                "select body from page_revisions where id = \
                    (select published_revision_id from pages where id = $1)",
            )
            .bind(page_id)
            .fetch_optional(&self.pool)
            .await?;
            let Some(body) = body else { continue };
            for (target, anchor) in extract_links(&body) {
                if !is_internal_path(&target) || known.contains(&target) {
                    continue;
                }
                if found
                    .iter()
                    .any(|f| f.target_url == target && f.source_page_id == Some(*page_id))
                {
                    continue;
                }
                found.push(FoundLink {
                    source_page_id: Some(*page_id),
                    target_url: target,
                    anchor_text: anchor,
                    status: None,
                });
            }
        }

        self.replace_broken_links(site_id, &found).await?;
        self.list_broken_links(site_id, false).await
    }
}

/// How many times one token may consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quantifier {
    /// Exactly once — the token carries no quantifier.
    One,
    /// `*` — zero or more.
    ZeroOrMore,
    /// `?` — zero or one.
    Optional,
}

/// One compiled unit of the pattern dialect: a single character class and its quantifier.
///
/// Carrying the quantifier ON the token is what makes "a quantifier repeats the previous single
/// character" structural rather than emergent. The earlier shape — a separate `Repeat(index)`
/// token — had to name the token it applied to, and that index pointed at itself the moment the
/// base was popped, so `/post-*` matched nothing. Here a leading quantifier has no token to
/// attach to at all, which is the correct answer: `*5` is not a pattern, it is a typo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Token {
    /// The character this token matches, or `None` for `.` (any single character).
    char: Option<char>,
    /// How many of them it may consume.
    quantifier: Quantifier,
}

impl Token {
    /// Whether this token accepts `ch`.
    fn accepts(&self, ch: char) -> bool {
        match self.char {
            Some(expected) => expected == ch,
            None => true,
        }
    }
}

/// One link the crawl-lite pass found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundLink {
    /// Page the link was written in.
    pub source_page_id: Option<Uuid>,
    /// The target as written.
    pub target_url: String,
    /// The anchor text, when the link had any.
    pub anchor_text: Option<String>,
    /// The status, when the caller checked one.
    pub status: Option<i32>,
}

/// Build the JSON-LD body for one schema type, with the fields it wants and cannot get.
///
/// The `missing_fields` half is the point. An owner choosing `Product` on a page with no price
/// is being told *why* the tag will be thin, in the panel, at the moment they chose it — rather
/// than discovering it in a validator six months later.
fn build_structured_data(
    kind: &str,
    source: &PageSeoSource,
    seo: &PageSeo,
    title: &str,
    description: Option<&str>,
) -> (Value, Vec<String>) {
    let extras = seo.structured_data.as_object().cloned().unwrap_or_default();
    let origin = format!("https://{}", source.site_host);
    let url = format!("{origin}/{}", source.slug);
    let mut missing: Vec<String> = Vec::new();

    let body = match kind {
        "Article" => {
            if description.is_none() {
                missing.push("description".to_string());
            }
            merge(
                json!({
                    "@context": "https://schema.org",
                    "@type": "Article",
                    "headline": title,
                    "url": url,
                    "mainEntityOfPage": url,
                    "dateModified": source.updated_at.date().to_string(),
                }),
                &extras,
            )
        }
        "Organization" => {
            for field in ["logo", "sameAs"] {
                if !extras.contains_key(field) {
                    missing.push(field.to_string());
                }
            }
            merge(
                json!({
                    "@context": "https://schema.org",
                    "@type": "Organization",
                    "name": title,
                    "url": origin,
                }),
                &extras,
            )
        }
        "FAQPage" => {
            let questions = extras
                .get("mainEntity")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            if questions == 0 {
                missing.push("mainEntity (at least one question and answer)".to_string());
            }
            merge(
                json!({
                    "@context": "https://schema.org",
                    "@type": "FAQPage",
                    "url": url,
                }),
                &extras,
            )
        }
        "Product" => {
            for field in ["offers", "brand"] {
                if !extras.contains_key(field) {
                    missing.push(field.to_string());
                }
            }
            merge(
                json!({
                    "@context": "https://schema.org",
                    "@type": "Product",
                    "name": title,
                    "url": url,
                    "description": description,
                }),
                &extras,
            )
        }
        "BreadcrumbList" => {
            let items = extras
                .get("itemListElement")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            if items == 0 {
                missing.push("itemListElement (at least one crumb)".to_string());
            }
            merge(
                json!({
                    "@context": "https://schema.org",
                    "@type": "BreadcrumbList",
                }),
                &extras,
            )
        }
        _ => {
            // WebSite: the site name and its own search action are the only two fields that
            // matter, and the host is the one thing the store always knows.
            merge(
                json!({
                    "@context": "https://schema.org",
                    "@type": "WebSite",
                    "name": title,
                    "url": origin,
                }),
                &extras,
            )
        }
    };
    (body, missing)
}

/// Merge the owner's extra values under a generated body, refusing a key that would rewrite the
/// schema's own identity (`@context`/`@type`): a body whose `@type` says something else is not
/// the type the editor picked.
fn merge(base: Value, extras: &Map<String, Value>) -> Value {
    let mut out = base.as_object().cloned().unwrap_or_default();
    for (key, value) in extras {
        if key == "@context" || key == "@type" {
            continue;
        }
        out.insert(key.clone(), value.clone());
    }
    Value::Object(out)
}

/// Render the sitemap index-free single document. One file, grouped by type in the comment
/// headers, because a second level of `sitemapindex` buys nothing for a site of any size a
/// self-hosted instance serves.
fn render_sitemap(
    host: &str,
    rows: &[(String, String, OffsetDateTime, String)],
    frequency: &str,
    priority: f64,
) -> String {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
    for (slug, page_type, updated_at, _title) in rows {
        let loc = xml_escape(&format!("https://{host}/{slug}"));
        let lastmod = updated_at.date().to_string();
        xml.push_str("  <url>\n");
        xml.push_str(&format!("    <loc>{loc}</loc>\n"));
        xml.push_str(&format!("    <lastmod>{lastmod}</lastmod>\n"));
        xml.push_str(&format!("    <changefreq>{frequency}</changefreq>\n"));
        xml.push_str(&format!("    <priority>{priority:.1}</priority>\n"));
        let _ = page_type;
        xml.push_str("  </url>\n");
    }
    xml.push_str("</urlset>\n");
    xml
}

/// Escape the five XML entities, and refuse to emit a control character.
///
/// An unescaped `&` in a slug is not possible (the slug constraint forbids it) but a *host* can
/// carry one through a misconfigured domain row, and an unescaped `&` is a sitemap every
/// consumer rejects — with no error the owner can see.
fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' && c != '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// Pull `(href, anchor_text)` out of an HTML body.
///
/// Deliberately not a real HTML parser: the body is a sanitized fragment (see `sanitize.rs`) and
/// this pass only needs the links a human would click. A parser dependency would be a whole
/// HTML5 tree for a list of hrefs.
fn extract_links(body: &str) -> Vec<(String, Option<String>)> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        // Cheap case-insensitive scan for `<a`. Everything below works in *char* indices, and
        // the first version of this function mixed them — the tag was sliced from the char
        // vector but the closing `>` was found as a byte offset, so the slice was nonsense and
        // the scan returned nothing at all. One coordinate system, chosen once, at the top.
        if chars[index] != '<' {
            index += 1;
            continue;
        }
        let is_anchor = chars
            .get(index + 1)
            .is_some_and(|ch| ch.eq_ignore_ascii_case(&'a'))
            && chars
                .get(index + 2)
                .is_none_or(|ch| ch.is_whitespace() || *ch == '>');
        if !is_anchor {
            index += 1;
            continue;
        }
        let Some(close_bracket) = chars[index..].iter().position(|ch| *ch == '>') else {
            break;
        };
        let end = index + close_bracket;
        let open: String = chars[index..=end].iter().collect();
        let href = attribute(&open, "href");

        // The anchor text is what sits between this tag and its `</a>`, tags stripped. `</a>`
        // is matched case-insensitively because a body may carry `</A>` and the whole point of
        // this pass is to survive what a human actually typed.
        let inner_end = chars[end + 1..]
            .windows(4)
            .position(|window| {
                window
                    .iter()
                    .collect::<String>()
                    .eq_ignore_ascii_case("</a>")
            })
            .map(|relative| end + 1 + relative);
        let anchor =
            inner_end.map(|stop| strip_tags(&chars[end + 1..stop].iter().collect::<String>()));
        let anchor = anchor.filter(|text| !text.is_empty());

        if let Some(href) = href {
            out.push((href, anchor));
        }
        index = end + 1;
    }
    out
}

/// Read one attribute out of an opening tag.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{name}=");
    let at = lower.find(&needle)?;
    let rest = &tag[at + needle.len()..];
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let end = rest[1..].find(quote)?;
        let value = &rest[1..1 + end];
        if value.trim().is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    } else {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let value = &rest[..end];
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    }
}

/// Drop tags and collapse whitespace, for the anchor text.
fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether a link target addresses this site: a root-relative path that is not the protocol
/// handler itself (`//evil.example`, which a naive `starts_with('/')` accepts and which resolves
/// to another origin entirely).
fn is_internal_path(target: &str) -> bool {
    target.starts_with('/') && !target.starts_with("//") && !target.starts_with("/\\")
}

/// Validate a redirect's `from`/`to` path.
///
/// Both are site-relative and carry no query or fragment: a redirect is about a page's address,
/// and matching on the query string turns every campaign link into a rule of its own.
fn validate_redirect_path(path: &str, field: &str) -> Result<String> {
    let trimmed = path.trim();
    if !trimmed.starts_with('/') {
        return Err(ContentError::InvalidRedirect(format!(
            "the {field} path must start with '/'"
        )));
    }
    if trimmed.starts_with("//") {
        return Err(ContentError::InvalidRedirect(format!(
            "the {field} path must not start with '//' — that addresses another host"
        )));
    }
    if trimmed.contains('?') || trimmed.contains('#') {
        return Err(ContentError::InvalidRedirect(format!(
            "the {field} path must not carry a query string or a fragment"
        )));
    }
    if trimmed.len() > MAX_URL_LENGTH {
        return Err(ContentError::InvalidRedirect(format!(
            "the {field} path is longer than {MAX_URL_LENGTH} characters"
        )));
    }
    Ok(trimmed.to_string())
}

/// Compile-check a pattern: balanced `*`? no, but a pattern may not be empty, may not exceed
/// the path limit, and may not carry alternation or grouping — the dialect does not have them,
/// and silently treating `|` as a literal would produce a rule that matches nothing and looks
/// saved.
pub fn validate_pattern(pattern: &str) -> Result<()> {
    if pattern.is_empty() {
        return Err(ContentError::InvalidRedirect(
            "a pattern cannot be empty".to_string(),
        ));
    }
    if pattern.len() > MAX_URL_LENGTH {
        return Err(ContentError::InvalidRedirect(format!(
            "a pattern is longer than {MAX_URL_LENGTH} characters"
        )));
    }
    for token in ['|', '(', ')', '[', ']', '{', '}', '+', '^', '$'] {
        if pattern.contains(token) {
            return Err(ContentError::InvalidRedirect(format!(
                "the pattern dialect is literal text with '.', '*' and '?' — {token:?} is not part of it"
            )));
        }
    }
    Ok(())
}

/// Match `value` against the platform's path pattern dialect, anchored to the whole string.
///
/// The dialect: literal characters match themselves, `.` matches exactly one character, `*`
/// repeats the **previous single character** zero or more times, and `?` makes the previous
/// single character optional. There is no alternation and no backtracking, so the match is
/// linear in the length of the path — which is the property that keeps this off the request path
/// as a denial-of-service surface.
///
/// Written as a small NFA over a token list rather than as a recursive matcher: a
/// backtracking `*` is exactly the shape that lets `*a*a*a*…` blow up, and the token form makes
/// the "previous single character" rule explicit instead of emergent.
#[must_use]
pub fn matches_pattern(pattern: &str, value: &str) -> bool {
    let mut tokens: Vec<Token> = Vec::new();
    for ch in pattern.chars() {
        let quantifier = match ch {
            '*' => Quantifier::ZeroOrMore,
            '?' => Quantifier::Optional,
            _ => {
                tokens.push(Token {
                    char: if ch == '.' { None } else { Some(ch) },
                    quantifier: Quantifier::One,
                });
                continue;
            }
        };
        // The quantifier attaches to the token already compiled. A pattern that *starts* with
        // one has nothing to attach to, so it cannot match: `*5` is a typo, not "any number of
        // noughts", and quietly reading it as anything else is how a rule matches nothing while
        // looking saved.
        let Some(last) = tokens.last_mut() else {
            return false;
        };
        // A second quantifier on one token (`a**`, `a*?`) collapses to the first: the dialect has
        // no nesting, and dropping the second is what a reader of the pattern expects.
        if last.quantifier == Quantifier::One {
            last.quantifier = quantifier;
        }
    }

    // NFA over token positions, with the "this token matches zero times" transition kept as its
    // own step instead of being folded into the per-character one.
    //
    // That separation is the whole correctness point, and two earlier versions got it wrong in
    // opposite directions. Folding the skip into the character loop and pushing `position + 1`
    // advances the pattern *without consuming a character*, so `/post-*` happily matched
    // `/post-1`: the `-` was skipped, the position moved on, and the `1` was then "consumed" by
    // a token that had already been passed. Applying the skip as an epsilon closure — a separate
    // pass over the state set, run before each character and once at the end — is what makes the
    // state set mean "positions reachable having consumed exactly the input so far".
    let mut current: Vec<usize> = vec![0];
    for ch in value.chars() {
        current = epsilon_closure(&tokens, &current);
        let mut next: Vec<usize> = Vec::new();
        for position in &current {
            let Some(token) = tokens.get(*position) else {
                continue;
            };
            if !token.accepts(ch) {
                continue;
            }
            match token.quantifier {
                // Exactly one, or exactly one of at most one: the character is consumed either
                // way and the token is done with.
                Quantifier::One | Quantifier::Optional => next.push(position + 1),
                // Consume it and keep going — the closure before the *next* character is what
                // offers the option to stop.
                Quantifier::ZeroOrMore => next.push(*position),
            }
        }
        next.sort_unstable();
        next.dedup();
        if next.is_empty() {
            return false;
        }
        current = next;
    }

    // Anchored: the whole value must be consumed, after one last closure so a trailing `*` or
    // `?` may match nothing. A pattern therefore has to end in `*` to allow a suffix
    // (`/product/.*`), and the panel's field help says exactly that in words.
    epsilon_closure(&tokens, &current).contains(&tokens.len())
}

/// Expand a state set with the "a quantified token matches zero times" transition.
///
/// Purely structural: it never looks at the input, which is the property that keeps a mid-match
/// state set from silently meaning "consumed one more character than it did".
fn epsilon_closure(tokens: &[Token], states: &[usize]) -> Vec<usize> {
    let mut out = states.to_vec();
    let mut index = 0;
    while index < out.len() {
        let position = out[index];
        let skippable = tokens.get(position).is_some_and(|token| {
            token.quantifier == Quantifier::ZeroOrMore || token.quantifier == Quantifier::Optional
        });
        if skippable {
            let next = position + 1;
            if next <= tokens.len() && !out.contains(&next) {
                out.push(next);
                index += 1;
                continue;
            }
        }
        index += 1;
    }
    out
}

/// Validate a page's SEO payload, with the defaults the columns would have applied.
pub fn validate_page_seo(seo: &PageSeo) -> Result<PageSeo> {
    let mut out = seo.clone();

    out.seo_title = validate_optional_text(seo.seo_title.as_deref(), MAX_SEO_TITLE_LENGTH)?;
    out.seo_description =
        validate_optional_text(seo.seo_description.as_deref(), MAX_SEO_DESCRIPTION_LENGTH)?;
    out.og_title = validate_optional_text(seo.og_title.as_deref(), MAX_OG_TEXT_LENGTH)?;
    out.og_description = validate_optional_text(seo.og_description.as_deref(), MAX_OG_TEXT_LENGTH)?;

    if let Some(canonical) = &out.canonical_url {
        let canonical = canonical.trim();
        if canonical.is_empty() {
            out.canonical_url = None;
        } else {
            if !canonical.starts_with("http://") && !canonical.starts_with("https://") {
                return Err(ContentError::InvalidSeo(format!(
                    "the canonical URL must be absolute — '{canonical}' is not"
                )));
            }
            if canonical.len() > MAX_URL_LENGTH {
                return Err(ContentError::InvalidSeo(format!(
                    "the canonical URL is longer than {MAX_URL_LENGTH} characters"
                )));
            }
            out.canonical_url = Some(canonical.to_string());
        }
    }

    if !TWITTER_CARDS.contains(&out.effective_twitter_card()) {
        return Err(ContentError::InvalidSeo(format!(
            "the twitter card must be one of {}",
            TWITTER_CARDS.join(", ")
        )));
    }
    if out.twitter_card.is_empty() {
        out.twitter_card = "summary_large_image".to_string();
    }

    out.robots = normalize_robots(&out.robots)?;

    match &out.structured_data_type {
        Some(kind) if kind.trim().is_empty() => out.structured_data_type = None,
        Some(kind) => {
            if !STRUCTURED_DATA_TYPES.contains(&kind.as_str()) {
                return Err(ContentError::InvalidSeo(format!(
                    "'{kind}' is not a schema this generator builds — pick one of {}",
                    STRUCTURED_DATA_TYPES.join(", ")
                )));
            }
            out.structured_data_type = Some(kind.clone());
        }
        None => {}
    }

    // `Value::Null` is what `#[serde(default)]` hands a payload that omitted the key, and the
    // column default is `'{}'` — so null is *absence*, not a malformed value, and treating it
    // as one makes an unedited page unsaveable.
    if out.structured_data.is_null() {
        out.structured_data = json!({});
    }
    if !out.structured_data.is_object() {
        return Err(ContentError::InvalidSeo(
            "structured data must be a JSON object".to_string(),
        ));
    }
    Ok(out)
}

/// Normalize a `robots` directive list.
///
/// `index` and `noindex` are opposites; a list carrying both is a contradiction a crawler
/// resolves by order, so the panel refuses to store it rather than letting the owner's order
/// decide. `noarchive`, `nosnippet` and `noimageindex` pass through — the platform does not
/// interpret them, so dropping them would be worse than carrying them.
fn normalize_robots(value: &str) -> Result<String> {
    let mut index = false;
    let mut noindex = false;
    let mut follow = false;
    let mut nofollow = false;
    let mut passthrough: Vec<String> = Vec::new();

    for directive in value.split(',') {
        let directive = directive.trim().to_ascii_lowercase();
        if directive.is_empty() {
            continue;
        }
        match directive.as_str() {
            "index" => index = true,
            "noindex" => noindex = true,
            "follow" => follow = true,
            "nofollow" => nofollow = false || nofollow,
            "all" => {
                index = true;
                follow = true;
            }
            "none" => {
                noindex = true;
                nofollow = true;
            }
            other => {
                if ROBOTS_DIRECTIVES.contains(&other) {
                    return Err(ContentError::InvalidSeo(format!(
                        "'{other}' is not a robots directive"
                    )));
                }
                passthrough.push(other.to_string());
            }
        }
    }
    if index && noindex {
        return Err(ContentError::InvalidSeo(
            "a page cannot be both 'index' and 'noindex' — pick one".to_string(),
        ));
    }
    if follow && nofollow {
        return Err(ContentError::InvalidSeo(
            "a page cannot be both 'follow' and 'nofollow' — pick one".to_string(),
        ));
    }

    let mut out: Vec<&str> = Vec::new();
    if noindex {
        out.push("noindex");
    } else {
        out.push("index");
    }
    if nofollow {
        out.push("nofollow");
    } else {
        out.push("follow");
    }
    out.extend(passthrough.iter().map(String::as_str));
    Ok(out.join(","))
}

/// Check a robots.txt for the mistakes that make a site invisible, and name them.
///
/// Returns the *warnings* rather than refusing: a site that blocks itself is a real
/// configuration the owner may have done on purpose, and the useful thing is to say so loudly
/// in the editor instead of refusing to save a file the owner can export by hand.
#[must_use]
pub fn validate_robots_txt(robots: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut disallow_all = false;
    let mut has_user_agent = false;
    for line in robots.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, ':');
        let (Some(field), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        match field.trim().to_ascii_lowercase().as_str() {
            "user-agent" => has_user_agent = true,
            "disallow" => {
                if value.trim() == "/" {
                    disallow_all = true;
                }
            }
            _ => {}
        }
    }
    if disallow_all {
        warnings.push(
            "this robots.txt disallows every path for at least one user-agent, so the site \
             asks search engines not to read it"
                .to_string(),
        );
    }
    if !has_user_agent {
        warnings
            .push("this robots.txt names no User-agent, so its rules apply to nobody".to_string());
    }
    warnings
}

/// Read a field of a generated tag set for the panel's preview table.
#[must_use]
pub fn tag_label(tag: &SeoTags) -> String {
    if tag.title.is_empty() {
        "(no title)".to_string()
    } else if tag.title_fits() {
        tag.title.clone()
    } else {
        format!(
            "{}…",
            tag.title
                .chars()
                .take(SeoTags::SERP_TITLE_LIMIT)
                .collect::<String>()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> PageSeoSource {
        PageSeoSource {
            page_id: Uuid::nil(),
            site_id: Uuid::nil(),
            slug: "about".to_string(),
            page_type: "page".to_string(),
            title: "About the studio".to_string(),
            summary: Some("Who we are and what we build.".to_string()),
            updated_at: OffsetDateTime::UNIX_EPOCH,
            site_host: "example.com".to_string(),
        }
    }

    #[test]
    fn an_empty_seo_payload_gets_the_column_defaults() {
        let seo = validate_page_seo(&PageSeo::default()).expect("an empty payload is valid");
        assert_eq!(seo.twitter_card, "summary_large_image");
        assert_eq!(seo.robots, "index,follow");
        assert!(seo.is_indexable());
    }

    #[test]
    fn a_relative_canonical_is_refused_because_it_resolves_against_whatever_host_was_reached() {
        let mut seo = PageSeo::default();
        seo.canonical_url = Some("/about".to_string());
        let error = validate_page_seo(&seo).expect_err("a relative canonical is a bug in the site");
        assert!(error.to_string().contains("must be absolute"));
    }

    #[test]
    fn a_misspelled_schema_type_is_refused_rather_than_emitted_as_a_dead_tag() {
        let mut seo = PageSeo::default();
        seo.structured_data_type = Some("Articel".to_string());
        let error = validate_page_seo(&seo).expect_err("a typo is not a schema type");
        assert_eq!(error.code(), "invalid_seo");
    }

    #[test]
    fn robots_refuses_a_list_that_says_both_yes_and_no() {
        let mut seo = PageSeo::default();
        seo.robots = "index,noindex".to_string();
        let error = validate_page_seo(&seo).expect_err("a contradiction is not a choice");
        assert!(error.to_string().contains("pick one"));

        let mut seo = PageSeo::default();
        seo.robots = "NOINDEX, Follow".to_string();
        let seo = validate_page_seo(&seo).expect("case is normalized");
        assert_eq!(seo.robots, "noindex,follow");
        assert!(!seo.is_indexable());
    }

    #[test]
    fn the_tags_fall_back_to_the_page_own_title_and_summary() {
        let tags = SeoStore::build_tags(&source(), &PageSeo::default(), None);
        assert_eq!(tags.title, "About the studio");
        assert_eq!(tags.og_title, "About the studio");
        assert_eq!(
            tags.description.as_deref(),
            Some("Who we are and what we build.")
        );
        assert_eq!(tags.robots, "index,follow");
        assert!(tags.json_ld.is_none());
        assert!(tags.title_fits());
    }

    #[test]
    fn a_page_with_no_schema_type_emits_no_json_ld_and_no_missing_field_noise() {
        let tags = SeoStore::build_tags(&source(), &PageSeo::default(), Some("https://cdn/x.png"));
        assert!(tags.json_ld.is_none());
        assert!(tags.missing_fields.is_empty());
        assert_eq!(tags.og_image.as_deref(), Some("https://cdn/x.png"));
    }

    #[test]
    fn choosing_product_lists_the_fields_the_page_cannot_supply() {
        let mut seo = PageSeo::default();
        seo.structured_data_type = Some("Product".to_string());
        let tags = SeoStore::build_tags(&source(), &seo, None);
        assert!(tags.missing_fields.contains(&"offers".to_string()));
        assert!(tags.missing_fields.contains(&"brand".to_string()));
        let body: Value = serde_json::from_str(tags.json_ld.as_deref().expect("json-ld")).unwrap();
        assert_eq!(body["@type"], "Product");
        assert_eq!(body["name"], "About the studio");
    }

    #[test]
    fn an_owners_extra_values_cannot_rewrite_the_schemas_identity() {
        let mut seo = PageSeo::default();
        seo.structured_data_type = Some("WebSite".to_string());
        seo.structured_data = json!({"@type": "Person", "sameAs": ["https://social/x"]});
        let tags = SeoStore::build_tags(&source(), &seo, None);
        let body: Value = serde_json::from_str(tags.json_ld.as_deref().expect("json-ld")).unwrap();
        assert_eq!(body["@type"], "WebSite");
        assert_eq!(body["sameAs"][0], "https://social/x");
    }

    #[test]
    fn a_website_schema_needs_nothing_missing_because_the_host_is_known() {
        let mut seo = PageSeo::default();
        seo.structured_data_type = Some("WebSite".to_string());
        let tags = SeoStore::build_tags(&source(), &seo, None);
        assert!(tags.missing_fields.is_empty());
    }

    #[test]
    fn the_pattern_dialect_repeats_the_previous_character_and_anchors_the_whole_path() {
        assert!(matches_pattern("/old", "/old"));
        assert!(!matches_pattern("/old", "/older"));
        // `/post-*` is "zero or more of the character before it", i.e. more dashes. The
        // construct an owner reaching for a wildcard suffix actually writes is `.*` — any
        // character, repeated — and both forms are pinned here so the dialect cannot drift.
        assert!(matches_pattern("/post-*", "/post"));
        assert!(matches_pattern("/post-*", "/post-"));
        assert!(matches_pattern("/post-*", "/post--"));
        assert!(!matches_pattern("/post-*", "/post-1"));
        assert!(matches_pattern("/product/.*", "/product/widget-blue"));
        assert!(matches_pattern("/product/.*", "/product/"));
        assert!(!matches_pattern("/product/.*", "/other/widget"));
        assert!(matches_pattern("/a.b", "/axb"));
        assert!(!matches_pattern("/a.b", "/ab"));
        // `?` makes the character before it optional — so it is "page, maybe with a dash", and
        // `/page-1` is NOT a match. An owner wanting a one-character tail writes `/page-?x`.
        assert!(matches_pattern("/page-?", "/page"));
        assert!(matches_pattern("/page-?", "/page-"));
        assert!(!matches_pattern("/page-?", "/page-1"));
        assert!(matches_pattern("/page-?x", "/pagex"));
        assert!(matches_pattern("/page-?x", "/page-x"));
        // Two optional characters in a row, each keeping its own state — the case a matcher
        // carrying a single "current position" instead of a state set gets wrong. The pattern is
        // four tokens wide, so exactly these four lengths can match it.
        assert!(matches_pattern("/a?b?c", "/c"));
        assert!(matches_pattern("/a?b?c", "/ac"));
        assert!(matches_pattern("/a?b?c", "/bc"));
        assert!(matches_pattern("/a?b?c", "/abc"));
        assert!(!matches_pattern("/a?b?c", "/abdc"));
    }

    #[test]
    fn a_leading_quantifier_does_not_swallow_the_pattern() {
        // The bug this pins: `*` used to stay in the item list, so this matched the two
        // characters `5*` and no repetition was possible at all.
        assert!(!matches_pattern("*5", "5"));
        assert!(!matches_pattern("*5", "555"));
        assert!(matches_pattern("5*", "5"));
        assert!(matches_pattern("5*", "55555"));
    }

    #[test]
    fn the_dialect_refuses_the_tokens_it_does_not_have() {
        for pattern in ["/a|b", "/(a)", "/[a]", "/a+", "/^a$", "/a{2}"] {
            assert!(
                validate_pattern(pattern).is_err(),
                "{pattern} is not in the dialect"
            );
        }
        assert!(validate_pattern("/old.*").is_ok());
        assert!(validate_pattern("").is_err());
    }

    #[test]
    fn a_path_outside_this_origin_is_not_an_internal_link() {
        assert!(is_internal_path("/about"));
        assert!(!is_internal_path("//evil.example/x"));
        assert!(!is_internal_path("https://elsewhere.example/x"));
        assert!(!is_internal_path("mailto:a@b.c"));
    }

    #[test]
    fn redirect_paths_are_site_relative_and_carry_no_query() {
        assert_eq!(validate_redirect_path("  /old  ", "from").unwrap(), "/old");
        assert!(validate_redirect_path("https://elsewhere.example/", "from").is_err());
        assert!(validate_redirect_path("//evil.example/", "from").is_err());
        assert!(validate_redirect_path("/old?utm=1", "from").is_err());
        assert!(validate_redirect_path("/old#top", "from").is_err());
    }

    #[test]
    fn a_robots_txt_that_blocks_the_world_is_accepted_and_warned_about() {
        let warnings = validate_robots_txt("User-agent: *\nDisallow: /\n");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("asks search engines not to read it"));

        let warnings = validate_robots_txt("Disallow: /admin\n");
        assert!(
            warnings.iter().any(|w| w.contains("no User-agent")),
            "a file naming no agent applies to nobody: {warnings:?}"
        );
        assert!(validate_robots_txt("User-agent: *\nAllow: /\n").is_empty());
    }

    #[test]
    fn the_sitemap_escapes_the_xml_entities_and_lists_what_it_was_given() {
        let rows = vec![(
            "a&b".to_string(),
            "page".to_string(),
            OffsetDateTime::UNIX_EPOCH,
            "A & B".to_string(),
        )];
        let xml = render_sitemap("example.com", &rows, "weekly", 0.8);
        assert!(xml.contains("<loc>https://example.com/a&amp;b</loc>"));
        assert!(xml.contains("<changefreq>weekly</changefreq>"));
        assert!(xml.contains("<priority>0.8</priority>"));
        assert!(xml.starts_with("<?xml"));
    }

    #[test]
    fn the_crawl_finds_the_hrefs_of_a_sanitized_body() {
        let body = "<p>See <a href=\"/about\">about us</a> and \
                    <a class='x' href='/news?page=2' target=\"_blank\">the news</a>, \
                    plus <a href='https://elsewhere.example/'>off-site</a>.</p>";
        let links = extract_links(body);
        assert_eq!(links.len(), 3);
        assert_eq!(
            links[0],
            ("/about".to_string(), Some("about us".to_string()))
        );
        assert_eq!(
            links[1],
            ("/news?page=2".to_string(), Some("the news".to_string()))
        );
        assert_eq!(links[2].0, "https://elsewhere.example/");
    }

    #[test]
    fn a_tag_the_owner_never_typed_is_reported_as_missing_rather_than_invented() {
        let tags = SeoStore::build_tags(&source(), &PageSeo::default(), None);
        assert_eq!(tag_label(&tags), "About the studio");
        let long = SeoStore::build_tags(
            &source(),
            &PageSeo {
                seo_title: Some(
                    "a title so much longer than a search result will ever show that the \
                     editor has to decide what to cut"
                        .to_string(),
                ),
                ..PageSeo::default()
            },
            None,
        );
        assert!(!long.title_fits());
        assert!(tag_label(&long).ends_with('…'));
    }
}
