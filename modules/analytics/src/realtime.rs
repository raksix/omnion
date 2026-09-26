//! The realtime view (REQ-007, slice 3): what is happening right now, over the raw rows.
//!
//! Realtime is deliberately **not** a rollup and not a WebSocket: it reads the raw rows of the
//! last half hour and answers a plain snapshot, which the API hands to the browser once and then
//! every few seconds as a server-sent event. Rolling it up first would make the screen quiet for
//! a bucket's worth of time, and "realtime" that lags a bucket is not realtime.
//!
//! The windows are fixed ([`REALTIME_WINDOWS`]): five minutes answers "who is here now", thirty
//! minutes answers "what just happened". A visitor counts when the visit was seen inside the
//! window — the visit is the session, so a reader of three pages in a row is one visitor.

use serde::Serialize;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::Result;

/// The two windows the realtime screen counts over, in minutes: now, and the half hour.
pub const REALTIME_WINDOWS: [i64; 2] = [5, 30];

/// Longest list the current-pages table answers with.
pub const MAX_REALTIME_PAGES: i64 = 10;

/// Longest event feed the snapshot carries.
pub const MAX_REALTIME_EVENTS: i64 = 20;

/// Where a window starts.
#[must_use]
pub fn window_start(now: OffsetDateTime, minutes: i64) -> OffsetDateTime {
    now - Duration::minutes(minutes.max(0))
}

/// One window's counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RealtimeCounters {
    /// The window, in minutes.
    pub window_minutes: i64,
    /// Distinct visitors seen in the window.
    pub visitors: i64,
    /// Pageviews recorded in the window.
    pub pageviews: i64,
    /// Custom events recorded in the window.
    pub events: i64,
    /// Goal hits recorded in the window.
    pub conversions: i64,
}

/// One page someone is on right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RealtimePage {
    /// Path being read.
    pub path: String,
    /// Distinct visitors on it inside the window.
    pub visitors: i64,
    /// Pageviews it recorded inside the window.
    pub views: i64,
    /// Last time it was read.
    pub last_seen: OffsetDateTime,
}

/// One thing that just happened on the site.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RealtimeEvent {
    /// Event name (`download`, `form_submit`, or a custom one).
    pub name: String,
    /// Page it happened on, when it happened on one.
    pub path: Option<String>,
    /// Value the caller attached, when there was one.
    pub value: Option<f64>,
    /// When it happened.
    pub occurred_at: OffsetDateTime,
}

/// Everything the realtime screen shows, as one snapshot.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RealtimeSnapshot {
    /// When the snapshot was taken.
    pub generated_at: OffsetDateTime,
    /// The five-minute window.
    pub last_5: RealtimeCounters,
    /// The thirty-minute window.
    pub last_30: RealtimeCounters,
    /// Pages being read inside the thirty-minute window.
    pub pages: Vec<RealtimePage>,
    /// The most recent events inside the thirty-minute window.
    pub events: Vec<RealtimeEvent>,
}

/// Read the snapshot of one site.
pub async fn snapshot(
    pool: &PgPool,
    site_id: Uuid,
    now: OffsetDateTime,
) -> Result<RealtimeSnapshot> {
    let mut counters = Vec::with_capacity(REALTIME_WINDOWS.len());
    for minutes in REALTIME_WINDOWS {
        counters.push(counters_of(pool, site_id, now, minutes).await?);
    }

    let since = window_start(now, 30);
    Ok(RealtimeSnapshot {
        generated_at: now,
        last_5: counters.first().copied().unwrap_or_default(),
        last_30: counters.get(1).copied().unwrap_or_default(),
        pages: pages(pool, site_id, since).await?,
        events: events(pool, site_id, since).await?,
    })
}

/// Count one window: visitors, pageviews, events and goal hits.
async fn counters_of(
    pool: &PgPool,
    site_id: Uuid,
    now: OffsetDateTime,
    minutes: i64,
) -> Result<RealtimeCounters> {
    let since = window_start(now, minutes);

    let visitors: i64 = sqlx::query_scalar(
        "select count(distinct visitor_hash)::bigint from analytics_visits \
         where site_id = $1 and last_seen_at >= $2",
    )
    .bind(site_id)
    .bind(since)
    .fetch_one(pool)
    .await?;

    let pageviews: i64 = sqlx::query_scalar(
        "select count(*)::bigint from analytics_pageviews where site_id = $1 and occurred_at >= $2",
    )
    .bind(site_id)
    .bind(since)
    .fetch_one(pool)
    .await?;

    let events: i64 = sqlx::query_scalar(
        "select count(*)::bigint from analytics_events where site_id = $1 and occurred_at >= $2",
    )
    .bind(site_id)
    .bind(since)
    .fetch_one(pool)
    .await?;

    let conversions: i64 = sqlx::query_scalar(
        "select count(*)::bigint from analytics_goal_hits hits \
         join analytics_goals goals on goals.id = hits.goal_id \
         where goals.site_id = $1 and hits.occurred_at >= $2",
    )
    .bind(site_id)
    .bind(since)
    .fetch_one(pool)
    .await?;

    Ok(RealtimeCounters {
        window_minutes: minutes,
        visitors,
        pageviews,
        events,
        conversions,
    })
}

/// The pages being read inside the window, busiest first.
async fn pages(pool: &PgPool, site_id: Uuid, since: OffsetDateTime) -> Result<Vec<RealtimePage>> {
    let rows: Vec<(String, i64, i64, OffsetDateTime)> = sqlx::query_as(
        "select pageviews.path, \
                count(distinct visits.visitor_hash)::bigint as visitors, \
                count(*)::bigint as views, \
                max(pageviews.occurred_at) as last_seen \
         from analytics_pageviews pageviews \
         join analytics_visits visits on visits.id = pageviews.visit_id \
         where pageviews.site_id = $1 and pageviews.occurred_at >= $2 \
         group by pageviews.path \
         order by visitors desc, views desc, pageviews.path \
         limit $3",
    )
    .bind(site_id)
    .bind(since)
    .bind(MAX_REALTIME_PAGES)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(path, visitors, views, last_seen)| RealtimePage {
            path,
            visitors,
            views,
            last_seen,
        })
        .collect())
}

/// The most recent events inside the window, newest first.
async fn events(pool: &PgPool, site_id: Uuid, since: OffsetDateTime) -> Result<Vec<RealtimeEvent>> {
    // `value::float8`: the column is `numeric`, and a report that decodes a `numeric` as a float
    // is a report that fails on the first event that carries a value.
    let rows: Vec<(String, Option<String>, Option<f64>, OffsetDateTime)> = sqlx::query_as(
        "select name, path, value::float8, occurred_at from analytics_events \
         where site_id = $1 and occurred_at >= $2 \
         order by occurred_at desc, id desc limit $3",
    )
    .bind(site_id)
    .bind(since)
    .bind(MAX_REALTIME_EVENTS)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(name, path, value, occurred_at)| RealtimeEvent {
            name,
            path,
            value,
            occurred_at,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_starts_the_documented_number_of_minutes_ago() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let start = window_start(now, 30);
        assert_eq!((now - start).whole_minutes(), 30);
        assert_eq!(now - window_start(now, 5), Duration::minutes(5));
        // A negative window is not a window that reaches into the future.
        assert_eq!(window_start(now, -5), now);
    }
}
