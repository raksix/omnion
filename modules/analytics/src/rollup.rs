//! Rollups: raw rows in, hourly and daily buckets out (REQ-007, slice 1).
//!
//! A bucket run **deletes its own rows and writes what it just computed**. That single choice is
//! what makes the worker safe to run at any cadence, from any number of instances, and after a
//! crash: the same raw rows always produce the same bucket, so running a bucket twice leaves the
//! table byte-for-byte identical (the acceptance criterion, proven by a test that snapshots the
//! table between two runs).
//!
//! `filtered` is the one metric the rollup never touches — it counts the beacons the collector
//! dropped, is incremented there, and would be erased by a recompute. The deletion is narrowed
//! to the metrics listed in [`DIMENSIONS`] for exactly that reason.
//!
//! Time is UTC: a day is the UTC calendar day and an hour a UTC hour. The window is deliberately
//! small — the last days for daily buckets, the last hours for hourly ones — because a bucket
//! that has already been computed from final data does not change when it is recomputed.

use sqlx::PgPool;
use time::{Date, Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::Result;

/// Days each tick recomputes: yesterday and today, because both can still receive rows.
pub const DAILY_WINDOW_DAYS: i64 = 2;

/// Hours each tick recomputes: enough for "today by hour" plus the tail of yesterday.
pub const HOURLY_WINDOW_HOURS: i64 = 3;

/// How long hourly buckets stay queryable; the schema's counterpart to "the last 48 hours".
pub const HOURLY_RETENTION_HOURS: i64 = 48;

/// Distinct values kept per dimension and bucket; the rest are folded into `(other)` so one
/// busy day cannot turn a rollup table into a second copy of the raw data.
pub const MAX_DIMENSION_VALUES: usize = 50;

/// The metric and dimension pairs a bucket run computes.
pub const DIMENSIONS: &[(&str, &str)] = &[
    ("pageviews", "total"),
    ("pageviews", "path"),
    ("visitors", "total"),
    ("visitors", "device"),
    ("visitors", "browser"),
    ("visitors", "os"),
    ("visitors", "country"),
    ("visitors", "language"),
    ("visitors", "referrer"),
    ("events", "total"),
    ("events", "name"),
    ("downloads", "total"),
    ("downloads", "file"),
    ("forms", "total"),
    ("conversions", "total"),
];

/// What one tick did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RollupReport {
    /// Sites rolled up.
    pub sites: usize,
    /// Buckets recomputed (daily + hourly).
    pub buckets: u64,
    /// Rollup rows written.
    pub rows: u64,
    /// Hourly rows pruned past the retention window.
    pub pruned: u64,
}

impl RollupReport {
    /// `true` when the tick found nothing to do — the runner stays quiet for it.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.buckets == 0 && self.rows == 0 && self.pruned == 0
    }
}

/// The distinct metrics a bucket run owns.
#[must_use]
pub fn metrics() -> Vec<String> {
    let mut metrics: Vec<String> = DIMENSIONS
        .iter()
        .map(|(metric, _)| (*metric).to_owned())
        .collect();
    metrics.sort();
    metrics.dedup();
    metrics
}

/// One tick of the rollup worker: every site's recent daily and hourly buckets, then the prune.
pub async fn tick(pool: &PgPool, now: OffsetDateTime) -> Result<RollupReport> {
    let sites = tracked_sites(pool).await?;
    let mut report = RollupReport::default();

    for site_id in sites {
        for offset in 0..DAILY_WINDOW_DAYS {
            let day = now.date() - Duration::days(offset);
            report.rows += rollup_day(pool, site_id, day).await?;
            report.buckets += 1;
        }

        let hour = hour_start(now);
        for offset in 0..HOURLY_WINDOW_HOURS {
            let bucket = hour - Duration::hours(offset);
            report.rows += rollup_hour(pool, site_id, bucket).await?;
            report.buckets += 1;
        }

        report.pruned +=
            prune_hourly(pool, site_id, now - Duration::hours(HOURLY_RETENTION_HOURS)).await?;
        report.sites += 1;
    }

    Ok(report)
}

/// Sites that have an analytics configuration — the ones a rollup can be computed for.
pub async fn tracked_sites(pool: &PgPool) -> Result<Vec<Uuid>> {
    let sites = sqlx::query_scalar("select site_id from analytics_settings order by site_id")
        .fetch_all(pool)
        .await?;

    Ok(sites)
}

/// Recompute one UTC day of one site; returns the number of rows written.
pub async fn rollup_day(pool: &PgPool, site_id: Uuid, day: Date) -> Result<u64> {
    let from = day.midnight().assume_utc();
    let to = from + Duration::days(1);
    let metrics = metrics();

    let mut transaction = pool.begin().await?;

    sqlx::query(
        "delete from analytics_daily where site_id = $1 and day = $2 and metric = any($3::text[])",
    )
    .bind(site_id)
    .bind(day)
    .bind(&metrics)
    .execute(&mut *transaction)
    .await?;

    let mut written = 0_u64;
    for (metric, dimension) in DIMENSIONS {
        for (value, count) in
            bucket_rows(&mut *transaction, site_id, from, to, metric, dimension).await?
        {
            sqlx::query(
                "insert into analytics_daily (site_id, day, metric, dimension_kind, dimension_value, count) \
                 values ($1, $2, $3, $4, $5, $6) \
                 on conflict (site_id, day, metric, dimension_kind, dimension_value) \
                 do update set count = excluded.count",
            )
            .bind(site_id)
            .bind(day)
            .bind(metric)
            .bind(dimension)
            .bind(&value)
            .bind(count)
            .execute(&mut *transaction)
            .await?;
            written += 1;
        }
    }

    transaction.commit().await?;
    Ok(written)
}

/// Recompute one UTC hour of one site; returns the number of rows written.
pub async fn rollup_hour(pool: &PgPool, site_id: Uuid, bucket: OffsetDateTime) -> Result<u64> {
    let from = bucket;
    let to = bucket + Duration::hours(1);
    let metrics = metrics();

    let mut transaction = pool.begin().await?;

    sqlx::query(
        "delete from analytics_hourly where site_id = $1 and bucket = $2 and metric = any($3::text[])",
    )
    .bind(site_id)
    .bind(bucket)
    .bind(&metrics)
    .execute(&mut *transaction)
    .await?;

    let mut written = 0_u64;
    for (metric, dimension) in DIMENSIONS {
        for (value, count) in
            bucket_rows(&mut *transaction, site_id, from, to, metric, dimension).await?
        {
            sqlx::query(
                "insert into analytics_hourly (site_id, bucket, metric, dimension_kind, dimension_value, count) \
                 values ($1, $2, $3, $4, $5, $6) \
                 on conflict (site_id, bucket, metric, dimension_kind, dimension_value) \
                 do update set count = excluded.count",
            )
            .bind(site_id)
            .bind(bucket)
            .bind(metric)
            .bind(dimension)
            .bind(&value)
            .bind(count)
            .execute(&mut *transaction)
            .await?;
            written += 1;
        }
    }

    transaction.commit().await?;
    Ok(written)
}

/// Delete hourly buckets that fell out of the 48-hour window.
pub async fn prune_hourly(pool: &PgPool, site_id: Uuid, before: OffsetDateTime) -> Result<u64> {
    let removed = sqlx::query("delete from analytics_hourly where site_id = $1 and bucket < $2")
        .bind(site_id)
        .bind(before)
        .execute(pool)
        .await?
        .rows_affected();

    Ok(removed)
}

/// The aggregate rows of one metric and dimension over `[from, to)`.
async fn bucket_rows<'executor, E>(
    executor: E,
    site_id: Uuid,
    from: OffsetDateTime,
    to: OffsetDateTime,
    metric: &str,
    dimension: &str,
) -> Result<Vec<(String, i64)>>
where
    E: sqlx::PgExecutor<'executor>,
{
    let sql = match (metric, dimension) {
        ("pageviews", "total") => {
            "select ''::text as value, count(*)::bigint as count from analytics_pageviews \
             where site_id = $1 and occurred_at >= $2 and occurred_at < $3"
        }
        ("pageviews", "path") => {
            "select path as value, count(*)::bigint as count from analytics_pageviews \
             where site_id = $1 and occurred_at >= $2 and occurred_at < $3 group by path"
        }
        ("visitors", "total") => {
            "select ''::text as value, count(distinct visitor_hash)::bigint as count \
             from analytics_visits where site_id = $1 and started_at >= $2 and started_at < $3"
        }
        ("visitors", "device") => {
            "select coalesce(device_type, '(unknown)') as value, \
             count(distinct visitor_hash)::bigint as count from analytics_visits \
             where site_id = $1 and started_at >= $2 and started_at < $3 group by 1"
        }
        ("visitors", "browser") => {
            "select coalesce(browser, '(unknown)') as value, \
             count(distinct visitor_hash)::bigint as count from analytics_visits \
             where site_id = $1 and started_at >= $2 and started_at < $3 group by 1"
        }
        ("visitors", "os") => {
            "select coalesce(os, '(unknown)') as value, \
             count(distinct visitor_hash)::bigint as count from analytics_visits \
             where site_id = $1 and started_at >= $2 and started_at < $3 group by 1"
        }
        ("visitors", "country") => {
            "select coalesce(country_code::text, '(unknown)') as value, \
             count(distinct visitor_hash)::bigint as count from analytics_visits \
             where site_id = $1 and started_at >= $2 and started_at < $3 group by 1"
        }
        ("visitors", "language") => {
            "select coalesce(language, '(unknown)') as value, \
             count(distinct visitor_hash)::bigint as count from analytics_visits \
             where site_id = $1 and started_at >= $2 and started_at < $3 group by 1"
        }
        ("visitors", "referrer") => {
            "select coalesce(referrer_host, '(direct)') as value, \
             count(distinct visitor_hash)::bigint as count from analytics_visits \
             where site_id = $1 and started_at >= $2 and started_at < $3 group by 1"
        }
        ("events", "total") => {
            "select ''::text as value, count(*)::bigint as count from analytics_events \
             where site_id = $1 and occurred_at >= $2 and occurred_at < $3"
        }
        ("events", "name") => {
            "select name as value, count(*)::bigint as count from analytics_events \
             where site_id = $1 and occurred_at >= $2 and occurred_at < $3 group by name"
        }
        ("downloads", "total") => {
            "select ''::text as value, count(*)::bigint as count from analytics_events \
             where site_id = $1 and name = 'download' and occurred_at >= $2 and occurred_at < $3"
        }
        ("downloads", "file") => {
            "select coalesce(properties->>'file', '(unknown)') as value, count(*)::bigint as count \
             from analytics_events where site_id = $1 and name = 'download' \
             and occurred_at >= $2 and occurred_at < $3 group by 1"
        }
        ("forms", "total") => {
            "select ''::text as value, count(*)::bigint as count from analytics_events \
             where site_id = $1 and name = 'form_submit' and occurred_at >= $2 and occurred_at < $3"
        }
        ("conversions", "total") => {
            "select ''::text as value, count(distinct hits.visitor_hash)::bigint as count \
             from analytics_goal_hits hits join analytics_goals goals on goals.id = hits.goal_id \
             where goals.site_id = $1 and hits.occurred_at >= $2 and hits.occurred_at < $3"
        }
        _ => return Ok(Vec::new()),
    };

    let rows: Vec<(String, i64)> = sqlx::query_as(sql)
        .bind(site_id)
        .bind(from)
        .bind(to)
        .fetch_all(executor)
        .await?;

    Ok(cap_dimensions(rows))
}

/// Keep the busiest [`MAX_DIMENSION_VALUES`] values and fold the tail into `(other)`.
///
/// Sorted by count and then by value so the result is deterministic: two runs over the same raw
/// rows write the same rows in the same order.
#[must_use]
pub fn cap_dimensions(mut rows: Vec<(String, i64)>) -> Vec<(String, i64)> {
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    if rows.len() <= MAX_DIMENSION_VALUES {
        return rows;
    }

    let rest: i64 = rows[MAX_DIMENSION_VALUES..]
        .iter()
        .map(|(_, count)| *count)
        .sum();
    let mut kept = rows[..MAX_DIMENSION_VALUES].to_vec();
    kept.push(("(other)".to_owned(), rest));
    kept
}

/// Start of the UTC hour `now` falls in.
fn hour_start(now: OffsetDateTime) -> OffsetDateTime {
    now.replace_minute(0)
        .and_then(|value| value.replace_second(0))
        .and_then(|value| value.replace_nanosecond(0))
        .unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtered_is_not_a_recomputed_metric() {
        assert!(
            !metrics().contains(&"filtered".to_owned()),
            "the rollup must never delete the counter the collector increments"
        );
        assert!(metrics().contains(&"pageviews".to_owned()));
        assert_eq!(metrics().len(), 6, "six metric names across the pairs");
    }

    #[test]
    fn a_bucket_caps_its_dimensions_and_folds_the_rest() {
        let rows: Vec<(String, i64)> = (0..60)
            .map(|index| (format!("/page-{index:02}"), 100 - index))
            .collect();
        let capped = cap_dimensions(rows);
        assert_eq!(capped.len(), MAX_DIMENSION_VALUES + 1);
        assert_eq!(capped.last().expect("a tail").0, "(other)");
        assert_eq!(
            capped.last().expect("a tail").1,
            455,
            "the ten values below the cap (50 down to 41) are folded into the tail, not lost"
        );

        // Deterministic: the same input caps the same way, whatever order it arrives in.
        let flipped: Vec<(String, i64)> = (0..60)
            .rev()
            .map(|index| (format!("/page-{index:02}"), 100 - index))
            .collect();
        assert_eq!(cap_dimensions(flipped), capped);
    }

    #[test]
    fn a_small_bucket_is_left_alone() {
        let rows = vec![("/".to_owned(), 3), ("/pricing".to_owned(), 1)];
        assert_eq!(cap_dimensions(rows.clone()), rows);
    }

    #[test]
    fn hour_buckets_start_on_the_hour() {
        let now = Date::from_calendar_date(2026, time::Month::September, 26)
            .unwrap()
            .with_hms(13, 47, 12)
            .unwrap()
            .assume_utc();
        let start = hour_start(now);
        assert_eq!(start.hour(), 13);
        assert_eq!(start.minute(), 0);
        assert_eq!(start.second(), 0);
    }
}
