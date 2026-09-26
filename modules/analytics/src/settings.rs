//! Per-site analytics settings: read, validate, write, and the snippet a site pastes.
//!
//! The validation here is the same list the settings screen renders as field errors — one
//! source, so a value the API accepts is a value the screen accepts.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{AnalyticsError, Result};
use crate::model::{
    MAX_EXCLUDED_IPS, MAX_EXCLUDED_PATHS, MAX_PATH_PATTERN, MAX_RETENTION_DAYS, MAX_SAMPLE_RATE,
    MIN_RETENTION_DAYS, MODES, Settings, SettingsChanges,
};
use crate::visitor::IpRule;

/// The column list every read uses; `excluded_ips` is cast to text so the module keeps working
/// without SQLx's `inet`/`cidr` codecs, and the settings screen sees one line per rule.
const SELECT: &str = "select site_id, tracking_enabled, mode, anonymize_ip, respect_dnt, \
     bot_filter, sample_rate, retention_days, excluded_paths, excluded_ips::text[] as excluded_ips, \
     updated_by, updated_at from analytics_settings";

/// Read a site's settings.
pub async fn find(pool: &PgPool, site_id: Uuid) -> Result<Option<Settings>> {
    let settings = sqlx::query_as::<_, Settings>(&format!("{SELECT} where site_id = $1"))
        .bind(site_id)
        .fetch_optional(pool)
        .await?;

    Ok(settings)
}

/// Read a site's settings, creating the defaults row when the site predates the migration (or
/// appeared between migrations in a multi-instance boot).
pub async fn ensure(pool: &PgPool, site_id: Uuid) -> Result<Settings> {
    if let Some(settings) = find(pool, site_id).await? {
        return Ok(settings);
    }

    sqlx::query(
        "insert into analytics_settings (site_id) values ($1) on conflict (site_id) do nothing",
    )
    .bind(site_id)
    .execute(pool)
    .await?;

    find(pool, site_id)
        .await?
        .ok_or(AnalyticsError::SettingsNotFound)
}

/// `true` when the changes are storable; otherwise the message names the field.
pub fn validate(changes: &SettingsChanges) -> Result<()> {
    if !MODES.contains(&changes.mode.as_str()) {
        return Err(AnalyticsError::InvalidSettings(format!(
            "mode must be one of {}",
            MODES.join(", ")
        )));
    }
    if !(1..=MAX_SAMPLE_RATE).contains(&changes.sample_rate) {
        return Err(AnalyticsError::InvalidSettings(format!(
            "sample_rate must be between 1 and {MAX_SAMPLE_RATE}"
        )));
    }
    if !(MIN_RETENTION_DAYS..=MAX_RETENTION_DAYS).contains(&changes.retention_days) {
        return Err(AnalyticsError::InvalidSettings(format!(
            "retention_days must be between {MIN_RETENTION_DAYS} and {MAX_RETENTION_DAYS}"
        )));
    }
    if changes.excluded_paths.len() > MAX_EXCLUDED_PATHS {
        return Err(AnalyticsError::InvalidSettings(format!(
            "excluded_paths may hold at most {MAX_EXCLUDED_PATHS} patterns"
        )));
    }
    for pattern in &changes.excluded_paths {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return Err(AnalyticsError::InvalidSettings(
                "excluded_paths may not contain blank lines".to_owned(),
            ));
        }
        if !pattern.starts_with('/') && !pattern.starts_with('*') {
            return Err(AnalyticsError::InvalidSettings(format!(
                "excluded path {pattern:?} must start with \"/\" (a path) or \"*\" (a suffix)"
            )));
        }
        if pattern.len() > MAX_PATH_PATTERN {
            return Err(AnalyticsError::InvalidSettings(format!(
                "excluded path {pattern:?} is longer than {MAX_PATH_PATTERN} characters"
            )));
        }
        if pattern.chars().any(char::is_whitespace) {
            return Err(AnalyticsError::InvalidSettings(format!(
                "excluded path {pattern:?} may not contain whitespace"
            )));
        }
    }
    if changes.excluded_ips.len() > MAX_EXCLUDED_IPS {
        return Err(AnalyticsError::InvalidSettings(format!(
            "excluded_ips may hold at most {MAX_EXCLUDED_IPS} entries"
        )));
    }
    for rule in &changes.excluded_ips {
        if IpRule::parse(rule).is_none() {
            return Err(AnalyticsError::InvalidSettings(format!(
                "excluded address {rule:?} is not an address or a network (one per line)"
            )));
        }
    }

    Ok(())
}

/// Store a full settings update and answer with the stored row.
pub async fn update(
    pool: &PgPool,
    site_id: Uuid,
    changes: &SettingsChanges,
    actor: Option<Uuid>,
) -> Result<Settings> {
    validate(changes)?;
    ensure(pool, site_id).await?;

    let paths: Vec<String> = changes
        .excluded_paths
        .iter()
        .map(|pattern| pattern.trim().to_owned())
        .collect();
    let addresses: Vec<String> = changes
        .excluded_ips
        .iter()
        .map(|rule| rule.trim().to_owned())
        .collect();

    sqlx::query(
        "update analytics_settings set tracking_enabled = $2, mode = $3, anonymize_ip = $4, \
         respect_dnt = $5, bot_filter = $6, sample_rate = $7, retention_days = $8, \
         excluded_paths = $9, excluded_ips = $10::text[]::cidr[], updated_by = $11, \
         updated_at = now() where site_id = $1",
    )
    .bind(site_id)
    .bind(changes.tracking_enabled)
    .bind(&changes.mode)
    .bind(changes.anonymize_ip)
    .bind(changes.respect_dnt)
    .bind(changes.bot_filter)
    .bind(changes.sample_rate)
    .bind(changes.retention_days)
    .bind(&paths)
    .bind(&addresses)
    .bind(actor)
    .execute(pool)
    .await?;

    find(pool, site_id)
        .await?
        .ok_or(AnalyticsError::SettingsNotFound)
}

/// The snippet a site pastes into its pages: one deferred script, addressed by its own domain,
/// carrying the resolved site key so one installation can serve many sites.
#[must_use]
pub fn tracking_snippet(script_url: &str, site_key: &str) -> String {
    format!("<script defer src=\"{script_url}\" data-site=\"{site_key}\"></script>")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> SettingsChanges {
        SettingsChanges {
            tracking_enabled: true,
            mode: "cookieless".to_owned(),
            anonymize_ip: true,
            respect_dnt: true,
            bot_filter: true,
            sample_rate: 100,
            retention_days: 180,
            excluded_paths: vec!["/admin/*".to_owned()],
            excluded_ips: vec!["203.0.113.0/24".to_owned()],
        }
    }

    #[test]
    fn a_faithful_update_validates() {
        assert!(validate(&valid()).is_ok());
    }

    #[test]
    fn each_promise_has_a_floor_and_a_ceiling() {
        let mut changes = valid();
        changes.mode = "sometimes".to_owned();
        assert!(validate(&changes).is_err());

        changes = valid();
        changes.sample_rate = 0;
        assert!(validate(&changes).is_err());
        changes.sample_rate = 101;
        assert!(validate(&changes).is_err());

        changes = valid();
        changes.retention_days = 6;
        assert!(validate(&changes).is_err());
        changes.retention_days = 1081;
        assert!(validate(&changes).is_err());

        changes = valid();
        changes.excluded_paths = vec!["admin".to_owned()];
        assert!(
            validate(&changes).is_err(),
            "a path pattern needs its slash"
        );

        changes = valid();
        changes.excluded_paths = vec!["/with space".to_owned()];
        assert!(validate(&changes).is_err());

        changes = valid();
        changes.excluded_ips = vec!["999.1.1.1".to_owned()];
        assert!(validate(&changes).is_err());

        changes = valid();
        changes.excluded_ips = vec!["".to_owned()];
        assert!(
            validate(&changes).is_err(),
            "a blank line is a typo, not a rule"
        );
    }

    #[test]
    fn the_snippet_carries_the_site_and_the_script() {
        let snippet = tracking_snippet("//omnion.test/analytics.js", "main");
        assert!(snippet.starts_with("<script defer src=\"//omnion.test/analytics.js\""));
        assert!(snippet.contains("data-site=\"main\""));
    }
}
