//! Ingest: one batched beacon becomes raw rows (REQ-007, slice 1).
//!
//! The order of the checks in [`drop_reason`] is the promise: a switched-off site, a visitor
//! who sent `Do Not Track` or `Global Privacy Control`, a bot, an excluded address or path, and
//! the site's own sample rate are all decided **before** a single row is written. What is
//! dropped is counted in the day's `filtered` bucket — a dashboard that hides a drop without
//! counting it is the kind of quiet lie this module exists to avoid.
//!
//! What is stored is deliberately thin: one visit row (the session), one pageview row per page
//! and one row per event, sharing a daily-salted visitor hash. No cookie, no browser storage,
//! no raw address — see `visitor.rs` for the pieces and the migration for the schema checks
//! that keep it true.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use time::{Date, Duration, OffsetDateTime};
use uuid::Uuid;

use crate::agent;
use crate::error::{AnalyticsError, Result};
use crate::model::Settings;
use crate::visitor;

/// Largest body the collector accepts. A beacon is a few hundred bytes; anything larger is a
/// caller sending traffic it should send somewhere else.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// Most events one beacon may carry; the rest are dropped and counted.
pub const MAX_EVENTS_PER_BEACON: usize = 25;

/// Longest path the collector stores.
pub const MAX_PATH_LENGTH: usize = 2048;

/// Longest serialised properties object stored with an event.
pub const MAX_PROPERTIES_BYTES: usize = 4096;

/// A visit is a run of pageviews with no longer gap than this one.
pub const VISIT_GAP_MINUTES: i64 = 30;

// ---------------------------------------------------------------------------------------------
// The beacon
// ---------------------------------------------------------------------------------------------

/// One batched beacon, exactly as the tracking script sends it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Beacon {
    /// The page the visitor is on; absent for a pure event beacon.
    #[serde(default)]
    pub pageview: Option<Pageview>,
    /// Custom events recorded on that page.
    #[serde(default)]
    pub events: Vec<Event>,
    /// Campaign parameters the script read from the address bar.
    #[serde(default)]
    pub utm: Option<Utm>,
}

/// The pageview part of a beacon.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Pageview {
    /// Path (and query) as the visitor sees it.
    pub path: String,
    /// Document title, when the page has one.
    #[serde(default)]
    pub title: Option<String>,
    /// Where the visitor came from, when the browser said.
    #[serde(default)]
    pub referrer: Option<String>,
    /// How long the previous page was visible, when the script could measure it.
    #[serde(default)]
    pub duration_ms: Option<i32>,
    /// Deepest scroll position in percent.
    #[serde(default)]
    pub scroll_depth: Option<i16>,
    /// Viewport size, when the browser reports it.
    #[serde(default)]
    pub screen: Option<Screen>,
    /// Document language, when the page declares one.
    #[serde(default)]
    pub language: Option<String>,
}

/// Viewport size.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
pub struct Screen {
    /// Width in CSS pixels.
    pub width: i32,
    /// Height in CSS pixels.
    pub height: i32,
}

/// One custom event.
#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    /// Event name (`[a-z][a-z0-9_.-]{0,63}`).
    pub name: String,
    /// Optional monetary value.
    #[serde(default)]
    pub value: Option<f64>,
    /// Free-form properties; kept as an object and never as a place for personal data.
    #[serde(default)]
    pub properties: Option<serde_json::Value>,
}

/// Campaign parameters of the landing address.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Utm {
    /// `utm_source`.
    #[serde(default)]
    pub source: Option<String>,
    /// `utm_medium`.
    #[serde(default)]
    pub medium: Option<String>,
    /// `utm_campaign`.
    #[serde(default)]
    pub campaign: Option<String>,
    /// `utm_term`.
    #[serde(default)]
    pub term: Option<String>,
    /// `utm_content`.
    #[serde(default)]
    pub content: Option<String>,
}

impl Beacon {
    /// Parse and validate a request body.
    ///
    /// A body that is not JSON, a pageview whose path is not a path, and a beacon with nothing
    /// in it are refused with their own codes; everything else is dropped per item and counted,
    /// because a public write path must never let one bad item cost a page's worth of data.
    pub fn parse(body: &[u8]) -> Result<Self> {
        let beacon: Self = serde_json::from_slice(body)
            .map_err(|error| AnalyticsError::InvalidPayload(error.to_string()))?;

        if beacon.pageview.is_none() && beacon.events.is_empty() {
            return Err(AnalyticsError::EmptyBeacon);
        }
        if let Some(pageview) = &beacon.pageview {
            validate_path(&pageview.path)?;
        }

        Ok(beacon)
    }

    /// The path this beacon is about, when it has one.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        self.pageview
            .as_ref()
            .map(|pageview| pageview.path.as_str())
    }
}

/// `true` when `path` is a path a browser could have been on.
pub fn validate_path(path: &str) -> Result<()> {
    if !path.starts_with('/') {
        return Err(AnalyticsError::InvalidPayload(
            "path must start with \"/\"".to_owned(),
        ));
    }
    if path.len() > MAX_PATH_LENGTH {
        return Err(AnalyticsError::InvalidPayload(format!(
            "path is longer than {MAX_PATH_LENGTH} characters"
        )));
    }
    if path.chars().any(char::is_control) {
        return Err(AnalyticsError::InvalidPayload(
            "path contains control characters".to_owned(),
        ));
    }
    Ok(())
}

/// `true` when an event name is storable (the schema checks the same shape).
#[must_use]
pub fn valid_event_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    let mut characters = name.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-'))
}

// ---------------------------------------------------------------------------------------------
// The decision
// ---------------------------------------------------------------------------------------------

/// Everything the request itself contributes to a decision (never the payload).
#[derive(Debug, Clone)]
pub struct RequestMeta {
    /// Address the request came from, when the runtime or the proxy reported one.
    pub address: Option<IpAddr>,
    /// User agent of the caller.
    pub user_agent: String,
    /// UTC day the beacon belongs to.
    pub day: Date,
    /// When the platform received it.
    pub now: OffsetDateTime,
    /// `DNT: 1`.
    pub dnt: bool,
    /// `Sec-GPC: 1`.
    pub gpc: bool,
}

/// Why a beacon was not stored, when it was not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// The site switched tracking off.
    TrackingOff,
    /// The visitor sent `Do Not Track` and the site honours it.
    DoNotTrack,
    /// The visitor sent `Global Privacy Control` and the site honours it.
    GlobalPrivacyControl,
    /// The caller is an automated agent and the site filters them.
    Bot,
    /// The address is on the site's exclusion list.
    ExcludedAddress,
    /// The path is on the site's exclusion list.
    ExcludedPath,
    /// The visitor is outside the site's sample.
    Sampled,
}

impl DropReason {
    /// Short label used in logs.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::TrackingOff => "tracking_off",
            Self::DoNotTrack => "dnt",
            Self::GlobalPrivacyControl => "gpc",
            Self::Bot => "bot",
            Self::ExcludedAddress => "excluded_address",
            Self::ExcludedPath => "excluded_path",
            Self::Sampled => "sampled",
        }
    }
}

/// The checks that do **not** need the visitor identifier: a switched-off site, the privacy
/// signals, the bot filter, the exclusion lists.
///
/// They run before anything is hashed or written, so a dropped beacon leaves no trace at all —
/// not a visit, not an event, not even the day's salt.
#[must_use]
pub fn policy_drop(
    settings: &Settings,
    meta: &RequestMeta,
    path: Option<&str>,
) -> Option<DropReason> {
    if !settings.tracking_enabled {
        return Some(DropReason::TrackingOff);
    }
    if settings.respect_dnt && meta.dnt {
        return Some(DropReason::DoNotTrack);
    }
    if settings.respect_dnt && meta.gpc {
        return Some(DropReason::GlobalPrivacyControl);
    }
    if settings.bot_filter && agent::is_bot(&meta.user_agent) {
        return Some(DropReason::Bot);
    }
    if let Some(address) = meta.address {
        let excluded = settings
            .excluded_ips
            .iter()
            .filter_map(|rule| visitor::IpRule::parse(rule))
            .any(|rule| rule.contains(address));
        if excluded {
            return Some(DropReason::ExcludedAddress);
        }
    }
    if let Some(path) = path {
        if settings
            .excluded_paths
            .iter()
            .any(|pattern| visitor::path_matches(pattern, path))
        {
            return Some(DropReason::ExcludedPath);
        }
    }

    None
}

/// The whole ingest decision, in one place, with no database in sight — so it is testable and
/// so the order of the promises is readable.
#[must_use]
pub fn drop_reason(
    settings: &Settings,
    meta: &RequestMeta,
    path: Option<&str>,
    visitor_hash: &str,
) -> Option<DropReason> {
    policy_drop(settings, meta, path).or_else(|| {
        if sampled_in(visitor_hash, settings.sample_rate) {
            None
        } else {
            Some(DropReason::Sampled)
        }
    })
}

/// Deterministic sampling: the same visitor is either inside or outside the sample, so a
/// sampled report still counts people rather than a random subset of their pageviews.
#[must_use]
pub fn sampled_in(visitor_hash: &str, sample_rate: i32) -> bool {
    if sample_rate >= 100 {
        return true;
    }
    if sample_rate < 1 {
        return false;
    }

    let head = visitor_hash.get(..16).unwrap_or(visitor_hash);
    let bucket = u64::from_str_radix(head, 16).unwrap_or(0) % 100;
    bucket < u64::try_from(sample_rate).unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------
// The write
// ---------------------------------------------------------------------------------------------

/// What one ingest stored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Stored {
    /// Visits touched (created or extended).
    pub visits: u32,
    /// Pageviews written.
    pub pageviews: u32,
    /// Events written.
    pub events: u32,
}

/// What one ingest dropped, by reason.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Dropped {
    /// Bot traffic.
    pub bots: u32,
    /// Policy drops: the site switched tracking off, or the visitor sent `Do Not Track` or
    /// `Global Privacy Control`.
    pub policy: u32,
    /// Excluded paths or addresses.
    pub excluded: u32,
    /// Visitors outside the site's sample.
    pub sampled: u32,
    /// Items the collector could not store (a bad event name, oversized properties).
    pub invalid: u32,
}

impl Dropped {
    /// Record one drop.
    pub fn record(&mut self, reason: DropReason) {
        match reason {
            DropReason::TrackingOff | DropReason::DoNotTrack | DropReason::GlobalPrivacyControl => {
                self.policy += 1
            }
            DropReason::Bot => self.bots += 1,
            DropReason::ExcludedAddress | DropReason::ExcludedPath => self.excluded += 1,
            DropReason::Sampled => self.sampled += 1,
        }
    }

    /// How many beacons were dropped in total.
    #[must_use]
    pub fn total(&self) -> u32 {
        self.bots + self.policy + self.excluded + self.sampled
    }
}

/// Result of one ingest call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct IngestReport {
    /// Rows written.
    pub stored: Stored,
    /// What was not stored, and why.
    pub dropped: Dropped,
}

/// Record one beacon, or count why it was not recorded.
pub async fn ingest(
    pool: &PgPool,
    site_id: Uuid,
    settings: &Settings,
    meta: &RequestMeta,
    beacon: &Beacon,
) -> Result<IngestReport> {
    // The policy checks come first: a dropped beacon never reaches the identifier, so nothing
    // about it (not even the day's salt) is written.
    if let Some(reason) = policy_drop(settings, meta, beacon.path()) {
        let mut dropped = Dropped::default();
        dropped.record(reason);
        increment_filtered(pool, site_id, meta.day, i64::from(dropped.total())).await?;

        tracing::debug!(
            site_id = %site_id,
            reason = reason.label(),
            "beacon dropped"
        );

        return Ok(IngestReport {
            stored: Stored::default(),
            dropped,
        });
    }

    let salt = visitor::daily_salt(pool, meta.day).await?;
    let visitor_hash = visitor::hash(site_id, &salt, meta.address, &meta.user_agent);

    if !sampled_in(&visitor_hash, settings.sample_rate) {
        let mut dropped = Dropped::default();
        dropped.record(DropReason::Sampled);
        increment_filtered(pool, site_id, meta.day, i64::from(dropped.total())).await?;

        return Ok(IngestReport {
            stored: Stored::default(),
            dropped,
        });
    }

    let mut stored = Stored::default();
    let mut dropped = Dropped::default();

    let visit = open_visit(pool, site_id, settings, meta, beacon, &visitor_hash).await?;
    stored.visits = 1;

    if let Some(pageview) = &beacon.pageview {
        insert_pageview(pool, site_id, visit.id, pageview, meta.now, visit.is_new).await?;
        stored.pageviews = 1;
    }

    for (index, event) in beacon.events.iter().enumerate() {
        if index >= MAX_EVENTS_PER_BEACON {
            dropped.invalid += 1;
            continue;
        }
        if insert_event(pool, site_id, visit.id, event, beacon.path(), meta.now).await? {
            stored.events += 1;
        } else {
            dropped.invalid += 1;
        }
    }

    Ok(IngestReport { stored, dropped })
}

/// The visit a beacon belongs to: an open one, or a new one.
struct Visit {
    id: i64,
    is_new: bool,
}

/// Find the visitor's open visit or open a new one.
async fn open_visit(
    pool: &PgPool,
    site_id: Uuid,
    settings: &Settings,
    meta: &RequestMeta,
    beacon: &Beacon,
    visitor_hash: &str,
) -> Result<Visit> {
    let cutoff = meta.now - Duration::minutes(VISIT_GAP_MINUTES);
    let open: Option<(i64, i32)> = sqlx::query_as(
        "select id, pageview_count from analytics_visits \
         where site_id = $1 and visitor_hash = $2 and last_seen_at >= $3 \
         order by last_seen_at desc limit 1",
    )
    .bind(site_id)
    .bind(visitor_hash)
    .bind(cutoff)
    .fetch_optional(pool)
    .await?;

    if let Some((id, _pageviews_before)) = open {
        let path = beacon.path();

        sqlx::query(
            "update analytics_visits set last_seen_at = $2, \
             pageview_count = pageview_count + $3, is_bounce = (pageview_count + $3) <= 1, \
             exit_path = coalesce($4, exit_path) where id = $1",
        )
        .bind(id)
        .bind(meta.now)
        .bind(i32::from(beacon.pageview.is_some()))
        .bind(path)
        .execute(pool)
        .await?;

        return Ok(Visit { id, is_new: false });
    }

    let pageview = beacon.pageview.as_ref();
    let (referrer_host, referrer_path) =
        split_referrer(pageview.and_then(|p| p.referrer.as_deref()));
    let utm = beacon.utm.clone().unwrap_or_default();
    let screen = pageview.and_then(|p| p.screen);
    let ip_prefix = if settings.anonymize_ip {
        None
    } else {
        meta.address.map(visitor::truncate)
    };

    let id: i64 = sqlx::query_scalar(
        "insert into analytics_visits (site_id, visitor_hash, started_at, last_seen_at, \
         pageview_count, is_bounce, entry_path, exit_path, referrer_host, referrer_path, \
         source, medium, campaign, term, content, device_type, os, browser, screen_width, \
         screen_height, language, ip_prefix) \
         values ($1, $2, $3, $3, $4, $5, $6, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, \
         $16, $17, $18, $19, $20::inet) returning id",
    )
    .bind(site_id)
    .bind(visitor_hash)
    .bind(meta.now)
    .bind(i32::from(pageview.is_some()))
    .bind(pageview.is_none())
    .bind(pageview.map(|p| p.path.clone()))
    .bind(referrer_host)
    .bind(referrer_path)
    .bind(clean(&utm.source))
    .bind(clean(&utm.medium))
    .bind(clean(&utm.campaign))
    .bind(clean(&utm.term))
    .bind(clean(&utm.content))
    .bind(agent::device_type(&meta.user_agent))
    .bind(agent::os(&meta.user_agent))
    .bind(agent::browser(&meta.user_agent))
    .bind(screen.map(|screen| screen.width))
    .bind(screen.map(|screen| screen.height))
    .bind(pageview.and_then(|p| p.language.clone()))
    .bind(ip_prefix)
    .fetch_one(pool)
    .await?;

    Ok(Visit { id, is_new: true })
}

/// Write one pageview and close the previous one in the same visit.
async fn insert_pageview(
    pool: &PgPool,
    site_id: Uuid,
    visit_id: i64,
    pageview: &Pageview,
    now: OffsetDateTime,
    is_new_visit: bool,
) -> Result<()> {
    if !is_new_visit {
        // The previous pageview of this visit is where the visitor came from, so it becomes the
        // visit's exit point until another pageview replaces it.
        sqlx::query(
            "update analytics_pageviews set is_exit = true where visit_id = $1 and is_exit = false",
        )
        .bind(visit_id)
        .execute(pool)
        .await?;
    }

    sqlx::query(
        "insert into analytics_pageviews (site_id, visit_id, path, title, occurred_at, \
         duration_ms, scroll_depth, is_entry, is_exit) values ($1, $2, $3, $4, $5, $6, $7, $8, false)",
    )
    .bind(site_id)
    .bind(visit_id)
    .bind(&pageview.path)
    .bind(clean(&pageview.title))
    .bind(now)
    .bind(pageview.duration_ms.map(|value| value.clamp(0, 86_400_000)))
    .bind(pageview.scroll_depth.map(|value| value.clamp(0, 100)))
    .bind(is_new_visit)
    .execute(pool)
    .await?;

    Ok(())
}

/// Write one event; `false` means the event was not storable and was counted instead.
async fn insert_event(
    pool: &PgPool,
    site_id: Uuid,
    visit_id: i64,
    event: &Event,
    path: Option<&str>,
    now: OffsetDateTime,
) -> Result<bool> {
    if !valid_event_name(&event.name) {
        return Ok(false);
    }

    let properties = match &event.properties {
        None => serde_json::json!({}),
        Some(value) if value.is_object() => value.clone(),
        Some(_) => return Ok(false),
    };
    if serde_json::to_string(&properties).map_or(true, |text| text.len() > MAX_PROPERTIES_BYTES) {
        return Ok(false);
    }

    let value = match event.value {
        None => None,
        Some(value) if value.is_finite() => Some(value),
        Some(_) => return Ok(false),
    };

    sqlx::query(
        "insert into analytics_events (site_id, visit_id, name, path, value, properties, occurred_at) \
         values ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(site_id)
    .bind(visit_id)
    .bind(&event.name)
    .bind(path)
    .bind(value)
    .bind(&properties)
    .bind(now)
    .execute(pool)
    .await?;

    Ok(true)
}

/// Count beacons the collector dropped in the day's `filtered` bucket.
///
/// This is the one row the rollup never recomputes — it counts decisions, not stored rows — so
/// it is incremented here and read as-is by the dashboards.
pub async fn increment_filtered(pool: &PgPool, site_id: Uuid, day: Date, count: i64) -> Result<()> {
    if count <= 0 {
        return Ok(());
    }

    sqlx::query(
        "insert into analytics_daily (site_id, day, metric, dimension_kind, dimension_value, count) \
         values ($1, $2, 'filtered', 'total', '', $3) \
         on conflict (site_id, day, metric, dimension_kind, dimension_value) \
         do update set count = analytics_daily.count + excluded.count",
    )
    .bind(site_id)
    .bind(day)
    .bind(count)
    .execute(pool)
    .await?;

    Ok(())
}

/// Split a referrer URL into its host and path; anything unusable stays out of the row.
fn split_referrer(referrer: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(referrer) = referrer.map(str::trim).filter(|value| !value.is_empty()) else {
        return (None, None);
    };

    let without_scheme = referrer
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(referrer);
    let (authority, rest) = match without_scheme.split_once('/') {
        Some((authority, rest)) => (authority, format!("/{rest}")),
        None => (without_scheme, String::new()),
    };

    let host = authority
        .split('@')
        .next_back()
        .unwrap_or(authority)
        .split(':')
        .next()
        .unwrap_or(authority)
        .trim()
        .to_ascii_lowercase();
    if host.is_empty() || !host.contains('.') {
        return (None, None);
    }

    let path = match rest.split_once('#') {
        Some((path, _)) => path.to_owned(),
        None => rest,
    };
    let path = path
        .split_once('?')
        .map(|(path, _)| path.to_owned())
        .unwrap_or(path);

    let host = host.strip_prefix("www.").unwrap_or(&host).to_owned();
    let path = if path.is_empty() { None } else { Some(path) };

    (Some(host), path)
}

/// Trim a free-text field and turn an empty one into `null`.
fn clean(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| text.chars().take(512).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            site_id: Uuid::nil(),
            tracking_enabled: true,
            mode: "cookieless".to_owned(),
            anonymize_ip: true,
            respect_dnt: true,
            bot_filter: true,
            sample_rate: 100,
            retention_days: 180,
            excluded_paths: vec!["/admin/*".to_owned()],
            excluded_ips: vec!["203.0.113.0/24".to_owned()],
            updated_by: None,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn meta() -> RequestMeta {
        RequestMeta {
            address: Some("198.51.100.9".parse().unwrap()),
            user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                         (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36"
                .to_owned(),
            day: Date::from_calendar_date(2026, time::Month::September, 26).unwrap(),
            now: OffsetDateTime::UNIX_EPOCH,
            dnt: false,
            gpc: false,
        }
    }

    #[test]
    fn a_plain_beacon_is_accepted() {
        assert_eq!(
            drop_reason(&settings(), &meta(), Some("/pricing"), &"a".repeat(64)),
            None
        );
    }

    #[test]
    fn every_promise_has_its_own_drop_reason_in_order() {
        let hash = "a".repeat(64);

        let mut off = settings();
        off.tracking_enabled = false;
        assert_eq!(
            drop_reason(&off, &meta(), Some("/pricing"), &hash),
            Some(DropReason::TrackingOff)
        );

        let mut dnt = meta();
        dnt.dnt = true;
        assert_eq!(
            drop_reason(&settings(), &dnt, Some("/pricing"), &hash),
            Some(DropReason::DoNotTrack)
        );

        let mut gpc = meta();
        gpc.gpc = true;
        assert_eq!(
            drop_reason(&settings(), &gpc, Some("/pricing"), &hash),
            Some(DropReason::GlobalPrivacyControl)
        );

        let mut bot = meta();
        bot.user_agent = "Googlebot/2.1 (+http://www.google.com/bot.html)".to_owned();
        assert_eq!(
            drop_reason(&settings(), &bot, Some("/pricing"), &hash),
            Some(DropReason::Bot)
        );

        let mut excluded_address = meta();
        excluded_address.address = Some("203.0.113.9".parse().unwrap());
        assert_eq!(
            drop_reason(&settings(), &excluded_address, Some("/pricing"), &hash),
            Some(DropReason::ExcludedAddress)
        );

        assert_eq!(
            drop_reason(&settings(), &meta(), Some("/admin/users"), &hash),
            Some(DropReason::ExcludedPath)
        );

        let mut sampled = settings();
        sampled.sample_rate = 1;
        assert_eq!(
            drop_reason(&sampled, &meta(), Some("/pricing"), &"f".repeat(64)),
            Some(DropReason::Sampled)
        );
    }

    #[test]
    fn a_site_that_does_not_respect_signals_still_counts_a_dnt_visitor() {
        let mut loose = settings();
        loose.respect_dnt = false;
        let mut dnt = meta();
        dnt.dnt = true;

        assert_eq!(
            drop_reason(&loose, &dnt, Some("/pricing"), &"a".repeat(64)),
            None
        );
    }

    #[test]
    fn sampling_is_deterministic_per_visitor() {
        let hash = "a".repeat(64);
        assert!(sampled_in(&hash, 100));
        assert_eq!(sampled_in(&hash, 50), sampled_in(&hash, 50));
        assert!(!sampled_in(&hash, 0), "a rate below one keeps nobody");
        // A hash whose head is 0b…01 (bucket 1) is inside a 10% sample but outside a 1% one.
        let bucket_one = format!("{:016x}{}", 1_u64, "b".repeat(48));
        assert!(sampled_in(&bucket_one, 10));
        assert!(!sampled_in(&bucket_one, 1));
    }

    #[test]
    fn malformed_beacons_are_refused_with_their_own_codes() {
        assert!(matches!(
            Beacon::parse(b"not json"),
            Err(AnalyticsError::InvalidPayload(_))
        ));
        assert!(matches!(
            Beacon::parse(b"{}"),
            Err(AnalyticsError::EmptyBeacon)
        ));
        assert!(matches!(
            Beacon::parse(br#"{"pageview":{"path":"pricing"}}"#),
            Err(AnalyticsError::InvalidPayload(_))
        ));
        assert!(
            Beacon::parse(br#"{"events":[{"name":"cta_click"}]}"#).is_ok(),
            "an event-only beacon is a beacon"
        );
        assert!(Beacon::parse(br#"{"pageview":{"path":"/pricing"}}"#).is_ok());
    }

    #[test]
    fn event_names_follow_the_schema() {
        assert!(valid_event_name("cta_click"));
        assert!(valid_event_name("signup.completed"));
        assert!(valid_event_name("download"));
        assert!(!valid_event_name("Cta"));
        assert!(!valid_event_name("2fast"));
        assert!(!valid_event_name("with space"));
        assert!(!valid_event_name(&"a".repeat(65)));
        assert!(!valid_event_name(""));
    }

    #[test]
    fn referrers_are_split_into_host_and_path() {
        let (host, path) = split_referrer(Some("https://www.google.com/search?q=omnion"));
        assert_eq!(host.as_deref(), Some("google.com"));
        assert_eq!(path.as_deref(), Some("/search"));

        let (host, path) = split_referrer(Some("https://news.example.org/a/b#section"));
        assert_eq!(host.as_deref(), Some("news.example.org"));
        assert_eq!(path.as_deref(), Some("/a/b"));

        let (host, path) = split_referrer(Some("random text"));
        assert_eq!(host, None);
        assert_eq!(path, None);

        let (host, path) = split_referrer(None);
        assert_eq!(host, None);
        assert_eq!(path, None);
    }
}
