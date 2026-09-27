//! Privacy operations: the retention purge, visitor erasure, the storage table and the spike
//! watch (docs/requests/REQ-007, slice 4).
//!
//! Three promises are kept here, in one place, so a reviewer can read them together:
//!
//! * **Retention.** `purge` removes the raw rows of one site that fell out of its window —
//!   visits (and their pageviews with them), events, goal hits and the salts that outlived every
//!   site's window — and writes one `analytics_purges` row (kind `retention`) carrying the cutoff
//!   and how many rows went. Another site's rows are never touched: every statement is scoped by
//!   `site_id`, and the one shared table (the salts) is only pruned past the *longest* retention
//!   any site still asks for.
//! * **Erasure.** `erase_visitor` removes every visit, pageview, event and goal hit a visitor
//!   handle appears in — the handle being the daily-salted hash, never an address — and records
//!   the run as kind `erasure`. It is idempotent: erasing twice removes nothing the second time
//!   and still answers, because an erasure request that fails on a retry is worse than one that
//!   answers zero.
//! * **The table itself.** [`STORED_FIELDS`] is the "what we store" list the settings screen
//!   renders: one row per column that matters, with its purpose and whether it is personal data.
//!   It lives in the module so the screen can never describe a different schema than the code.
//!
//! Rounding the set out, [`detect_spike`] answers whether the hour that just closed ran more
//! than [`SPIKE_FACTOR`] times the trailing week's median hourly visitors — the fact behind
//! `analytics.traffic_spike`. Its emitted-once guard lives in [`spike_recorded`], which reads the
//! events the platform already recorded rather than keeping state that a restart would lose.

use serde::Serialize;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::{AnalyticsError, Result};
use crate::model::MAX_RETENTION_DAYS;

/// How many times the trailing median an hour's visitors have to beat to be a spike.
pub const SPIKE_FACTOR: f64 = 3.0;

/// The metric and dimension the hourly visitor count lives under in `analytics_hourly`.
const VISITORS_TOTAL: (&str, &str) = ("visitors", "total");

// ---------------------------------------------------------------------------------------------
// The storage table
// ---------------------------------------------------------------------------------------------

/// One row of the "what we store" table: a column, why it exists, and whether it is personal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StoredField {
    /// Table the column lives in.
    pub table: &'static str,
    /// Column (or column family) the row describes.
    pub column: &'static str,
    /// Why the platform keeps it, in one sentence a site owner can read.
    pub purpose: &'static str,
    /// Whether the column holds personal data.
    pub personal: bool,
}

/// What the analytics engine stores, in the order the settings screen renders it.
pub const STORED_FIELDS: &[StoredField] = &[
    StoredField {
        table: "analytics_visits",
        column: "visitor_hash",
        purpose: "A daily-salted hash of the site, the day\u{2019}s salt, the address and the \
                  browser — what makes two pageviews one visit without naming a person.",
        personal: true,
    },
    StoredField {
        table: "analytics_visits",
        column: "ip_prefix",
        purpose: "The network an address belongs to, truncated to /24 (IPv4) or /48 (IPv6); \
                  null while IP anonymization is on, which is the default.",
        personal: true,
    },
    StoredField {
        table: "analytics_visits",
        column: "country_code, language, device_type, os, browser, screen_width, \
                 screen_height, source, medium, campaign, term, content",
        purpose: "What the browser reported about the visit, so reports can group by device, \
                  country, language and campaign without profiling a person.",
        personal: false,
    },
    StoredField {
        table: "analytics_pageviews",
        column: "path, title, occurred_at, duration_ms, scroll_depth",
        purpose: "Which page was read, for how long and how far down — the raw material of the \
                  page report.",
        personal: false,
    },
    StoredField {
        table: "analytics_events",
        column: "name, path, value, properties",
        purpose: "Which action happened, as the page sent it. Form field values and addresses \
                  are never accepted here.",
        personal: false,
    },
    StoredField {
        table: "analytics_goal_hits",
        column: "visitor_hash, step_position, occurred_at",
        purpose: "Which visitor reached which step of a goal, so a funnel counts people rather \
                  than requests. Removed with the visitor on erasure.",
        personal: true,
    },
    StoredField {
        table: "analytics_salts",
        column: "day, salt",
        purpose: "The day\u{2019}s salt: it is what stops yesterday\u{2019}s rows from being \
                  linked to today\u{2019}s. Rotated at midnight and pruned with the raw data.",
        personal: false,
    },
    StoredField {
        table: "analytics_daily, analytics_hourly",
        column: "metric, dimension_kind, dimension_value, count",
        purpose: "Aggregated counts per day and hour, with no visitor handle and no address — \
                  the tables dashboards read and the reports fall back to past the raw window.",
        personal: false,
    },
    StoredField {
        table: "analytics_settings",
        column: "excluded_paths, excluded_ips",
        purpose: "The rules the collector applies before storing anything: a beacon from an \
                  excluded path or address is dropped, not stored and hidden.",
        personal: false,
    },
    StoredField {
        table: "analytics_purges",
        column: "kind, cutoff, rows_removed, actor_user_id, created_at",
        purpose: "The audit record of every purge and erasure: what ran, when, by whom, and how \
                  many rows it removed.",
        personal: false,
    },
];

/// The "what we store" table, as the settings screen reads it.
#[must_use]
pub fn stored_fields() -> Vec<StoredField> {
    STORED_FIELDS.to_vec()
}

// ---------------------------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------------------------

/// The cutoff a purge with this retention would use: midnight (UTC) of the day that many days
/// before `now`.
///
/// A day boundary rather than a moving instant on purpose: "keep 180 days" then means whole days,
/// so two purges run an hour apart agree on what the window is, and the screen can name the
/// cutoff before the button is pressed.
#[must_use]
pub fn cutoff_for(retention_days: i32, now: OffsetDateTime) -> OffsetDateTime {
    let days = i64::from(retention_days.clamp(1, MAX_RETENTION_DAYS));
    (now.date() - Duration::days(days))
        .midnight()
        .assume_utc()
}

/// What one retention run removed, table by table.
#[derive(Debug, Clone, Serialize)]
pub struct PurgeOutcome {
    /// The audit row this run wrote.
    pub purge_id: Uuid,
    /// Site the run was scoped to.
    pub site_id: Uuid,
    /// Always `retention` here; the audit table shares one shape with erasure.
    pub kind: String,
    /// Rows older than this instant were removed.
    #[serde(with = "time::serde::rfc3339")]
    pub cutoff: OffsetDateTime,
    /// Visits removed.
    pub visits: i64,
    /// Pageviews removed (with their visits).
    pub pageviews: i64,
    /// Events removed.
    pub events: i64,
    /// Goal hits removed.
    pub goal_hits: i64,
    /// Salt rows pruned.
    pub salts: i64,
    /// Everything above, added up — what the audit row stores.
    pub rows_removed: i64,
    /// When the run finished.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Run the retention purge of one site and record it.
pub async fn purge(
    pool: &PgPool,
    site_id: Uuid,
    retention_days: i32,
    actor: Option<Uuid>,
) -> Result<PurgeOutcome> {
    purge_at(pool, site_id, retention_days, actor, OffsetDateTime::now_utc()).await
}

/// [`purge`] with the clock handed in, so a test can name the cutoff it expects.
pub async fn purge_at(
    pool: &PgPool,
    site_id: Uuid,
    retention_days: i32,
    actor: Option<Uuid>,
    now: OffsetDateTime,
) -> Result<PurgeOutcome> {
    let cutoff = cutoff_for(retention_days, now);
    let mut transaction = pool.begin().await?;

    // Pageviews first: they are found through their visits, and deleting a visit would take them
    // along without a count. Then the events, then the visits they belonged to, then the goal
    // hits — every statement scoped to this site.
    let pageviews = sqlx::query(
        "delete from analytics_pageviews where site_id = $1 and visit_id in \
         (select id from analytics_visits where site_id = $1 and started_at < $2)",
    )
    .bind(site_id)
    .bind(cutoff)
    .execute(&mut *transaction)
    .await?
    .rows_affected() as i64;

    let events = sqlx::query("delete from analytics_events where site_id = $1 and occurred_at < $2")
        .bind(site_id)
        .bind(cutoff)
        .execute(&mut *transaction)
        .await?
        .rows_affected() as i64;

    let visits =
        sqlx::query("delete from analytics_visits where site_id = $1 and started_at < $2")
            .bind(site_id)
            .bind(cutoff)
            .execute(&mut *transaction)
            .await?
            .rows_affected() as i64;

    let goal_hits = sqlx::query(
        "delete from analytics_goal_hits where occurred_at < $2 and goal_id in \
         (select id from analytics_goals where site_id = $1)",
    )
    .bind(site_id)
    .bind(cutoff)
    .execute(&mut *transaction)
    .await?
    .rows_affected() as i64;

    // The salts are one shared table, so this is the one statement that is not scoped by site:
    // a salt is only read while it is the current day, and it is pruned past the longest
    // retention *any* site still asks for, so no site loses a salt inside its own window.
    let salts = sqlx::query(
        "delete from analytics_salts where day < current_date \
         - (select max(retention_days) from analytics_settings)",
    )
    .execute(&mut *transaction)
    .await?
    .rows_affected() as i64;

    let rows_removed = visits + pageviews + events + goal_hits;
    let purge_id = Uuid::new_v4();
    let created_at = OffsetDateTime::now_utc();

    sqlx::query(
        "insert into analytics_purges (id, site_id, kind, cutoff, rows_removed, actor_user_id) \
         values ($1, $2, 'retention', $3, $4, $5)",
    )
    .bind(purge_id)
    .bind(site_id)
    .bind(cutoff)
    .bind(rows_removed)
    .bind(actor)
    .execute(&mut *transaction)
    .await?;

    transaction.commit().await?;

    Ok(PurgeOutcome {
        purge_id,
        site_id,
        kind: "retention".to_owned(),
        cutoff,
        visits,
        pageviews,
        events,
        goal_hits,
        salts,
        rows_removed,
        created_at,
    })
}

// ---------------------------------------------------------------------------------------------
// Erasure
// ---------------------------------------------------------------------------------------------

/// `true` when `handle` is a visitor hash of the documented shape (`^[0-9a-f]{64}$`).
#[must_use]
pub fn is_visitor_hash(handle: &str) -> bool {
    handle.len() == 64
        && handle
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// What one erasure removed.
#[derive(Debug, Clone, Serialize)]
pub struct ErasureOutcome {
    /// The audit row this run wrote.
    pub purge_id: Uuid,
    /// Site the run was scoped to.
    pub site_id: Uuid,
    /// The handle that was erased (already a pseudonym, never an address).
    pub visitor: String,
    /// Visits removed.
    pub visits: i64,
    /// Pageviews removed.
    pub pageviews: i64,
    /// Events removed.
    pub events: i64,
    /// Goal hits removed.
    pub goal_hits: i64,
    /// Everything above, added up.
    pub rows_removed: i64,
    /// When the run finished.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Erase every row of one visitor handle inside one site, and record the run.
///
/// Idempotent by construction: the statements remove what is there, so a second call for the same
/// handle removes nothing and still writes its audit row — an erasure that fails on a retry would
/// be a compliance problem of its own.
pub async fn erase_visitor(
    pool: &PgPool,
    site_id: Uuid,
    handle: &str,
    actor: Option<Uuid>,
) -> Result<ErasureOutcome> {
    let handle = handle.trim().to_lowercase();
    if !is_visitor_hash(&handle) {
        return Err(AnalyticsError::InvalidVisitor(
            "a visitor handle is 64 lower-case hexadecimal characters".to_owned(),
        ));
    }

    let mut transaction = pool.begin().await?;

    let pageviews = sqlx::query(
        "delete from analytics_pageviews where site_id = $1 and visit_id in \
         (select id from analytics_visits where site_id = $1 and visitor_hash = $2)",
    )
    .bind(site_id)
    .bind(&handle)
    .execute(&mut *transaction)
    .await?
    .rows_affected() as i64;

    let events = sqlx::query(
        "delete from analytics_events where site_id = $1 and visit_id in \
         (select id from analytics_visits where site_id = $1 and visitor_hash = $2)",
    )
    .bind(site_id)
    .bind(&handle)
    .execute(&mut *transaction)
    .await?
    .rows_affected() as i64;

    let visits = sqlx::query("delete from analytics_visits where site_id = $1 and visitor_hash = $2")
        .bind(site_id)
        .bind(&handle)
        .execute(&mut *transaction)
        .await?
        .rows_affected() as i64;

    let goal_hits = sqlx::query(
        "delete from analytics_goal_hits where visitor_hash = $2 and goal_id in \
         (select id from analytics_goals where site_id = $1)",
    )
    .bind(site_id)
    .bind(&handle)
    .execute(&mut *transaction)
    .await?
    .rows_affected() as i64;

    let rows_removed = visits + pageviews + events + goal_hits;
    let purge_id = Uuid::new_v4();
    let created_at = OffsetDateTime::now_utc();

    sqlx::query(
        "insert into analytics_purges (id, site_id, kind, rows_removed, actor_user_id) \
         values ($1, $2, 'erasure', $3, $4)",
    )
    .bind(purge_id)
    .bind(site_id)
    .bind(rows_removed)
    .bind(actor)
    .execute(&mut *transaction)
    .await?;

    transaction.commit().await?;

    Ok(ErasureOutcome {
        purge_id,
        site_id,
        visitor: handle,
        visits,
        pageviews,
        events,
        goal_hits,
        rows_removed,
        created_at,
    })
}

// ---------------------------------------------------------------------------------------------
// The audit trail
// ---------------------------------------------------------------------------------------------

/// One row of `analytics_purges`, as the settings screen reads it.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct PurgeRecord {
    /// Primary key.
    pub id: Uuid,
    /// Site the run was scoped to (null for a run that covered every site).
    pub site_id: Option<Uuid>,
    /// `retention` or `erasure`.
    pub kind: String,
    /// Rows older than this instant went; null for an erasure, which has no cutoff.
    #[serde(with = "time::serde::rfc3339::option")]
    pub cutoff: Option<OffsetDateTime>,
    /// How many rows the run removed.
    pub rows_removed: i64,
    /// Account that ran it, when a person did.
    pub actor_user_id: Option<Uuid>,
    /// When the run finished.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// The columns every read of the audit trail uses.
const PURGE_COLUMNS: &str =
    "id, site_id, kind, cutoff, rows_removed, actor_user_id, created_at";

/// The most recent run of one site, whatever its kind.
pub async fn last_purge(pool: &PgPool, site_id: Uuid) -> Result<Option<PurgeRecord>> {
    let record = sqlx::query_as::<_, PurgeRecord>(&format!(
        "select {PURGE_COLUMNS} from analytics_purges where site_id = $1 \
         order by created_at desc limit 1"
    ))
    .bind(site_id)
    .fetch_optional(pool)
    .await?;

    Ok(record)
}

/// The most recent runs of one site, newest first.
pub async fn purge_history(pool: &PgPool, site_id: Uuid, limit: i64) -> Result<Vec<PurgeRecord>> {
    let records = sqlx::query_as::<_, PurgeRecord>(&format!(
        "select {PURGE_COLUMNS} from analytics_purges where site_id = $1 \
         order by created_at desc limit $2"
    ))
    .bind(site_id)
    .bind(limit.clamp(1, 100))
    .fetch_all(pool)
    .await?;

    Ok(records)
}

// ---------------------------------------------------------------------------------------------
// The spike watch
// ---------------------------------------------------------------------------------------------

/// One hour that ran far above its own trailing week.
#[derive(Debug, Clone, Serialize)]
pub struct SpikeFinding {
    /// Site the hour belongs to.
    pub site_id: Uuid,
    /// The completed hour that spiked.
    #[serde(with = "time::serde::rfc3339")]
    pub hour: OffsetDateTime,
    /// Visitors counted in that hour.
    pub visitors: i64,
    /// The trailing seven-day median of hourly visitors.
    pub median: f64,
    /// How many times the median the hour reached.
    pub factor: f64,
}

/// The start of `now`'s hour, in UTC.
#[must_use]
pub fn hour_start(now: OffsetDateTime) -> OffsetDateTime {
    now.replace_minute(0)
        .and_then(|value| value.replace_second(0))
        .and_then(|value| value.replace_nanosecond(0))
        .unwrap_or(now)
}

/// The label of an hour as it rides in a `traffic_spike` payload and in the emitted-once guard.
#[must_use]
pub fn hour_label(hour: OffsetDateTime) -> String {
    hour.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| hour.unix_timestamp().to_string())
}

/// The trailing window a spike is measured against.
pub const SPIKE_WINDOW: Duration = Duration::days(7);

/// `true` when the completed hour of `now` ran more than [`SPIKE_FACTOR`] times the median of the
/// seven days before it.
///
/// The median of zero is not a spike: the first hour of a site's life beats its own empty
/// history by any factor, and an alert nobody can act on is noise. The hour that just closed is
/// the subject — the hour in progress would answer differently every time it was asked.
pub async fn detect_spike(
    pool: &PgPool,
    site_id: Uuid,
    now: OffsetDateTime,
) -> Result<Option<SpikeFinding>> {
    let hour = hour_start(now) - Duration::hours(1);
    let (metric, dimension) = VISITORS_TOTAL;

    let visitors: i64 = sqlx::query_scalar(
        "select coalesce(sum(count), 0)::bigint from analytics_hourly \
         where site_id = $1 and metric = $2 and dimension_kind = $3 and bucket = $4",
    )
    .bind(site_id)
    .bind(metric)
    .bind(dimension)
    .bind(hour)
    .fetch_one(pool)
    .await?;

    if visitors <= 0 {
        return Ok(None);
    }

    let median: Option<f64> = sqlx::query_scalar(
        "select percentile_cont(0.5) within group (order by count) from analytics_hourly \
         where site_id = $1 and metric = $2 and dimension_kind = $3 \
         and bucket >= $4 and bucket < $5",
    )
    .bind(site_id)
    .bind(metric)
    .bind(dimension)
    .bind(hour - SPIKE_WINDOW)
    .bind(hour)
    .fetch_one(pool)
    .await?;

    let Some(median) = median else {
        return Ok(None);
    };
    if median <= 0.0 {
        return Ok(None);
    }

    let factor = visitors as f64 / median;
    if factor <= SPIKE_FACTOR {
        return Ok(None);
    }

    Ok(Some(SpikeFinding {
        site_id,
        hour,
        visitors,
        median,
        factor,
    }))
}

/// `true` when the platform already recorded a `traffic_spike` for this hour — the guard that
/// keeps a poll that runs every few seconds from announcing the same hour again and again.
///
/// It reads the events themselves rather than a counter in memory: a restart must not turn one
/// busy hour into a second notification.
pub async fn spike_recorded(pool: &PgPool, site_id: Uuid, hour: OffsetDateTime) -> Result<bool> {
    let count: i64 = sqlx::query_scalar(
        "select count(*)::bigint from events where site_id = $1 and name = 'analytics.traffic_spike' \
         and payload->>'hour' = $2",
    )
    .bind(site_id)
    .bind(hour_label(hour))
    .fetch_one(pool)
    .await?;

    Ok(count > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::{Date, Month};

    /// A fixed clock so the cutoff arithmetic is checked, not the wall clock.
    fn clock() -> OffsetDateTime {
        Date::from_calendar_date(2026, Month::September, 26)
            .expect("a valid day")
            .with_hms(14, 37, 12)
            .expect("a valid time")
            .assume_utc()
    }

    #[test]
    fn the_cutoff_is_the_midnight_that_many_days_back() {
        let cutoff = cutoff_for(7, clock());
        assert_eq!(cutoff.date(), Date::from_calendar_date(2026, Month::September, 19).unwrap());
        assert_eq!(cutoff.time(), time::Time::MIDNIGHT);

        // Whole days, so two runs an hour apart agree on the window.
        let later = clock() + Duration::hours(5);
        assert_eq!(cutoff_for(7, later), cutoff);

        // Both ends of the promised range, and a value outside it clamps rather than panicking.
        assert_eq!(cutoff_for(1, clock()).date().day(), 25);
        assert_eq!(cutoff_for(1080, clock()).date().year(), 2023);
        assert_eq!(cutoff_for(0, clock()), cutoff_for(1, clock()));
    }

    #[test]
    fn a_visitor_handle_is_checked_before_anything_is_deleted() {
        assert!(is_visitor_hash(&"a".repeat(64)));
        assert!(is_visitor_hash(&"0123456789abcdef".repeat(4)));
        assert!(!is_visitor_hash(&"a".repeat(63)));
        assert!(!is_visitor_hash(&"a".repeat(65)));
        assert!(!is_visitor_hash(&"A".repeat(64)), "upper-case is not the shape");
        assert!(!is_visitor_hash(&"g".repeat(64)), "g is not hexadecimal");
        assert!(!is_visitor_hash("203.0.113.9"), "an address is not a handle");
    }

    #[test]
    fn the_storage_table_names_every_personal_column() {
        let fields = stored_fields();
        assert!(fields.len() >= 8, "the table covers the schema");

        let personal: Vec<&str> = fields
            .iter()
            .filter(|field| field.personal)
            .map(|field| field.column)
            .collect();
        assert!(personal.iter().any(|column| column.contains("visitor_hash")));
        assert!(personal.iter().any(|column| column.contains("ip_prefix")));

        // The two columns the privacy promise turns on are described as personal, and nothing
        // in the table claims an address is stored whole.
        for field in &fields {
            assert!(!field.purpose.is_empty(), "every row says why it exists");
            assert!(
                !field.purpose.contains("raw address is stored"),
                "no row promises to store a raw address"
            );
        }
    }

    #[test]
    fn an_hour_has_one_label_for_both_the_payload_and_the_guard() {
        let hour = hour_start(clock());
        assert_eq!(hour.minute(), 0);
        assert_eq!(hour.second(), 0);
        assert_eq!(hour_label(hour), "2026-09-26T14:00:00Z");
    }
}
