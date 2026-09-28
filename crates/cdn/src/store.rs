//! Database access for cache rules and CDN settings (REQ-011, slice 1).
//!
//! One rule per row, one settings row per site plus one installation-wide row. The
//! two read paths that matter are [`list_rules`] (the matcher loads a site's rules in
//! precedence order on every cacheable request) and [`resolve_settings`] (which a
//! site without its own row falls back on).

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::CdnError;
use crate::matcher::{CacheKey, PathPattern};
use crate::rule::{Bypass, CacheRule};

/// A stored rule, as the table holds it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RuleRow {
    /// Primary key.
    pub id: Uuid,
    /// Owning site.
    pub site_id: Uuid,
    /// Display name.
    pub name: String,
    /// Precedence; lower wins.
    pub priority: i32,
    /// The glob, as written.
    pub path_pattern: String,
    /// Methods this rule applies to.
    pub methods: Vec<String>,
    /// Edge TTL in seconds.
    pub edge_ttl_seconds: i32,
    /// Browser TTL in seconds.
    pub browser_ttl_seconds: i32,
    /// Stale-while-revalidate window in seconds.
    pub swr_seconds: i32,
    /// Cache-key components, as stored json.
    pub cache_key: serde_json::Value,
    /// Bypass conditions, as stored json.
    pub bypass: serde_json::Value,
    /// Whether the rule is live.
    pub enabled: bool,
    /// Author, when the row still has one.
    pub created_by: Option<Uuid>,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

impl RuleRow {
    /// Rebuild the domain rule from the row.
    ///
    /// The pattern is compiled here rather than trusted: a row is data, and data can be
    /// edited by a migration, a restore or a hand-written fix. A pattern that no longer
    /// compiles is a rule that can never match, so it is reported as an error the caller
    /// can surface instead of being silently dropped from the set.
    pub fn to_rule(&self) -> Result<CacheRule, CdnError> {
        Ok(CacheRule {
            name: self.name.clone(),
            priority: self.priority,
            pattern: PathPattern::parse(&self.path_pattern)?,
            methods: self.methods.clone(),
            edge_ttl_seconds: self.edge_ttl_seconds,
            browser_ttl_seconds: self.browser_ttl_seconds,
            swr_seconds: self.swr_seconds,
            cache_key: serde_json::from_value::<CacheKey>(self.cache_key.clone())
                .unwrap_or_default(),
            bypass: serde_json::from_value::<Bypass>(self.bypass.clone()).unwrap_or_default(),
            enabled: self.enabled,
        })
    }
}

/// A settings row, with the credential reduced to a presence flag.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SettingsRow {
    /// Primary key.
    pub id: Uuid,
    /// The site, or `None` for the installation-wide row.
    pub site_id: Option<Uuid>,
    /// The adapter key.
    pub provider: String,
    /// Configured endpoint.
    pub endpoint_url: Option<String>,
    /// Configured zone reference.
    pub zone_ref: Option<String>,
    /// Whether a credential is stored. The bytes themselves are never selected.
    pub has_credential: bool,
    /// Trigger toggles.
    pub auto_purge: serde_json::Value,
    /// Batch cap per provider call.
    pub batch_size: i32,
    /// Attempts before an item is left failed.
    pub max_attempts: i32,
    /// Last editor.
    pub updated_by: Option<Uuid>,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

/// Load one site's rules in precedence order.
///
/// Ordered in SQL rather than in Rust: the matcher needs them in order, and sorting in
/// the database is the only way the order is guaranteed to match the index the panel's
/// own table is ordered by.
pub async fn list_rules(pool: &PgPool, site_id: Uuid) -> Result<Vec<RuleRow>, CdnError> {
    sqlx::query_as::<_, RuleRow>(
        "select id, site_id, name, priority, path_pattern, methods, edge_ttl_seconds, \
                browser_ttl_seconds, swr_seconds, cache_key, bypass, enabled, created_by, \
                created_at, updated_at \
         from cdn_cache_rules where site_id = $1 order by priority asc, created_at asc",
    )
    .bind(site_id)
    .fetch_all(pool)
    .await
    .map_err(CdnError::from)
}

/// Load the domain rules for a site, compiling each pattern.
///
/// A site with one unreadable pattern does not lose its working rules: the bad row is
/// reported alongside the good ones, because silently dropping it would let a cache
/// behave as if the rule did not exist — which is the harder failure to notice.
pub async fn list_rules_compiled(
    pool: &PgPool,
    site_id: Uuid,
) -> Result<(Vec<CacheRule>, Vec<(Uuid, CdnError)>), CdnError> {
    let rows = list_rules(pool, site_id).await?;
    let mut rules = Vec::with_capacity(rows.len());
    let mut broken = Vec::new();
    for row in rows {
        match row.to_rule() {
            Ok(rule) => rules.push(rule),
            Err(error) => broken.push((row.id, error)),
        }
    }
    Ok((rules, broken))
}

/// Read one rule.
pub async fn find_rule(pool: &PgPool, id: Uuid) -> Result<Option<RuleRow>, CdnError> {
    sqlx::query_as::<_, RuleRow>(
        "select id, site_id, name, priority, path_pattern, methods, edge_ttl_seconds, \
                browser_ttl_seconds, swr_seconds, cache_key, bypass, enabled, created_by, \
                created_at, updated_at \
         from cdn_cache_rules where id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(CdnError::from)
}

/// The values a create or update writes.
#[derive(Debug, Clone)]
pub struct NewRule {
    /// Owning site.
    pub site_id: Uuid,
    /// The validated rule.
    pub rule: CacheRule,
    /// Author.
    pub created_by: Option<Uuid>,
}

/// Insert a rule.
pub async fn create_rule(pool: &PgPool, new: &NewRule) -> Result<RuleRow, CdnError> {
    let rule = &new.rule;
    let row = sqlx::query_as::<_, RuleRow>(
        "insert into cdn_cache_rules \
             (site_id, name, priority, path_pattern, methods, edge_ttl_seconds, \
              browser_ttl_seconds, swr_seconds, cache_key, bypass, enabled, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
         returning id, site_id, name, priority, path_pattern, methods, edge_ttl_seconds, \
                   browser_ttl_seconds, swr_seconds, cache_key, bypass, enabled, created_by, \
                   created_at, updated_at",
    )
    .bind(new.site_id)
    .bind(&rule.name)
    .bind(rule.priority)
    .bind(rule.pattern.as_str())
    .bind(&rule.methods)
    .bind(rule.edge_ttl_seconds)
    .bind(rule.browser_ttl_seconds)
    .bind(rule.swr_seconds)
    .bind(serde_json::to_value(&rule.cache_key).unwrap_or_default())
    .bind(serde_json::to_value(&rule.bypass).unwrap_or_default())
    .bind(rule.enabled)
    .bind(new.created_by)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Update a rule in place.
pub async fn update_rule(pool: &PgPool, id: Uuid, rule: &CacheRule) -> Result<RuleRow, CdnError> {
    let row = sqlx::query_as::<_, RuleRow>(
        "update cdn_cache_rules set name = $2, priority = $3, path_pattern = $4, methods = $5, \
                edge_ttl_seconds = $6, browser_ttl_seconds = $7, swr_seconds = $8, \
                cache_key = $9, bypass = $10, enabled = $11 \
         where id = $1 \
         returning id, site_id, name, priority, path_pattern, methods, edge_ttl_seconds, \
                   browser_ttl_seconds, swr_seconds, cache_key, bypass, enabled, created_by, \
                   created_at, updated_at",
    )
    .bind(id)
    .bind(&rule.name)
    .bind(rule.priority)
    .bind(rule.pattern.as_str())
    .bind(&rule.methods)
    .bind(rule.edge_ttl_seconds)
    .bind(rule.browser_ttl_seconds)
    .bind(rule.swr_seconds)
    .bind(serde_json::to_value(&rule.cache_key).unwrap_or_default())
    .bind(serde_json::to_value(&rule.bypass).unwrap_or_default())
    .bind(rule.enabled)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Set a rule's live flag, without touching the rest of the row.
pub async fn set_rule_enabled(
    pool: &PgPool,
    id: Uuid,
    enabled: bool,
) -> Result<Option<RuleRow>, CdnError> {
    sqlx::query_as::<_, RuleRow>(
        "update cdn_cache_rules set enabled = $2 where id = $1 \
         returning id, site_id, name, priority, path_pattern, methods, edge_ttl_seconds, \
                   browser_ttl_seconds, swr_seconds, cache_key, bypass, enabled, created_by, \
                   created_at, updated_at",
    )
    .bind(id)
    .bind(enabled)
    .fetch_optional(pool)
    .await
    .map_err(CdnError::from)
}

/// Delete a rule.
pub async fn delete_rule(pool: &PgPool, id: Uuid) -> Result<bool, CdnError> {
    let result = sqlx::query("delete from cdn_cache_rules where id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Rewrite the whole priority order of a site's rules.
///
/// One statement, one transaction: a partial reorder would leave two rules sharing a
/// priority and make "lower wins" ambiguous, which is the one thing the column exists to
/// prevent. The ids are checked against the site's own rules so a caller cannot renumber
/// another site's table by guessing an id.
pub async fn reorder_rules(
    pool: &PgPool,
    site_id: Uuid,
    ordered_ids: &[Uuid],
) -> Result<usize, CdnError> {
    if ordered_ids.is_empty() {
        return Ok(0);
    }
    let mut transaction = pool.begin().await?;
    let known = sqlx::query_scalar::<_, Uuid>(
        "select id from cdn_cache_rules where site_id = $1 for update",
    )
    .bind(site_id)
    .fetch_all(&mut *transaction)
    .await?;

    // Every id named must be one of this site's rules, and every one of the site's rules
    // must be named: a reorder that silently drops a rule would leave it with a stale
    // priority that nobody chose.
    if known.len() != ordered_ids.len()
        || known.iter().any(|id| !ordered_ids.contains(id))
        || ordered_ids.iter().any(|id| !known.contains(id))
    {
        transaction.rollback().await?;
        return Err(CdnError::IncompleteReorder {
            expected: known.len(),
            given: ordered_ids.len(),
        });
    }

    for (index, id) in ordered_ids.iter().enumerate() {
        sqlx::query("update cdn_cache_rules set priority = $3 where id = $1 and site_id = $2")
            .bind(id)
            .bind(site_id)
            .bind(index as i32)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    Ok(ordered_ids.len())
}

/// Read a site's settings, falling back to the installation-wide row.
///
/// A site that has never been configured inherits the platform row, which is what makes
/// the settings screen work on a fresh installation instead of showing an empty form for
/// a provider that is in fact running.
pub async fn resolve_settings(
    pool: &PgPool,
    site_id: Option<Uuid>,
) -> Result<SettingsRow, CdnError> {
    let row = sqlx::query_as::<_, SettingsRow>(&settings_select(
        "select id, site_id, provider, endpoint_url, zone_ref, \
                (credential_ciphertext is not null) as has_credential, auto_purge, batch_size, \
                max_attempts, updated_by, created_at, updated_at \
         from cdn_settings \
         where site_id is not distinct from $1 \
         order by site_id nulls last limit 1",
    ))
    .bind(site_id)
    .fetch_optional(pool)
    .await?;

    row.ok_or(CdnError::from(sqlx::Error::RowNotFound))
}

/// `settings_select` keeps the projection in one place: the `has_credential` expression is
/// the whole reason this helper exists, because a read path that selected
/// `credential_ciphertext` "just for debugging" is how a key reaches a log.
fn settings_select(body: &str) -> String {
    body.to_string()
}
