//! The shapes the analytics module stores and answers with.

use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;

/// The tracking modes a site may choose.
pub const MODES: [&str; 2] = ["cookieless", "cookie"];

/// Longest excluded-path list a site may keep, and the longest single pattern.
pub const MAX_EXCLUDED_PATHS: usize = 200;
/// Longest excluded-address list a site may keep.
pub const MAX_EXCLUDED_IPS: usize = 200;
/// Longest single path pattern.
pub const MAX_PATH_PATTERN: usize = 200;

/// Upper bound of the sampling rate, and of retention.
pub const MAX_SAMPLE_RATE: i32 = 100;
/// Retention bounds promised to a site: a week is the floor, three years the ceiling.
pub const MIN_RETENTION_DAYS: i32 = 7;
/// See [`MIN_RETENTION_DAYS`].
pub const MAX_RETENTION_DAYS: i32 = 1080;

/// One site's analytics configuration (a row of `analytics_settings`).
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Settings {
    /// Site this configuration belongs to.
    pub site_id: Uuid,
    /// Whether the collector records anything at all for this site.
    pub tracking_enabled: bool,
    /// `cookieless` (default) or `cookie`.
    pub mode: String,
    /// Whether addresses are dropped entirely (default) instead of truncated.
    pub anonymize_ip: bool,
    /// Whether `Do Not Track` and `Global Privacy Control` are honoured.
    pub respect_dnt: bool,
    /// Whether automated agents are filtered at ingest.
    pub bot_filter: bool,
    /// Share of visitors kept, 1–100.
    pub sample_rate: i32,
    /// How long raw rows are kept, 7–1080 days.
    pub retention_days: i32,
    /// Paths whose beacons are dropped.
    pub excluded_paths: Vec<String>,
    /// Addresses (single or networks) whose beacons are dropped.
    pub excluded_ips: Vec<String>,
    /// Account that last changed the configuration.
    pub updated_by: Option<Uuid>,
    /// Last change, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// A full settings update: the settings screen sends every field, so a partial write cannot
/// leave a site with a mix of old and new promises.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SettingsChanges {
    /// Whether the collector records anything at all for this site.
    pub tracking_enabled: bool,
    /// `cookieless` or `cookie`.
    pub mode: String,
    /// Whether addresses are dropped entirely.
    pub anonymize_ip: bool,
    /// Whether `Do Not Track` and `Global Privacy Control` are honoured.
    pub respect_dnt: bool,
    /// Whether automated agents are filtered.
    pub bot_filter: bool,
    /// Share of visitors kept, 1–100.
    pub sample_rate: i32,
    /// How long raw rows are kept, 7–1080 days.
    pub retention_days: i32,
    /// Paths whose beacons are dropped.
    #[serde(default)]
    pub excluded_paths: Vec<String>,
    /// Addresses (single or networks) whose beacons are dropped.
    #[serde(default)]
    pub excluded_ips: Vec<String>,
}

impl SettingsChanges {
    /// The defaults of a fresh site, as the settings screen's "restore defaults" fills them in.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            tracking_enabled: true,
            mode: "cookieless".to_owned(),
            anonymize_ip: true,
            respect_dnt: true,
            bot_filter: true,
            sample_rate: 100,
            retention_days: 180,
            excluded_paths: Vec::new(),
            excluded_ips: Vec::new(),
        }
    }
}
