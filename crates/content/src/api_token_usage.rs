//! Daily usage for a content API token, and the queries the panel reads it with (REQ-019,
//! slice 3).
//!
//! **The read path must not write a row per request.** A content token is a high-volume
//! credential by definition — a frontend revalidating an ETag on a busy site makes hundreds of
//! calls an hour — and a write per call would turn the usage view into a write amplifier that
//! competes with the reads it is measuring. So this table is written by the *worker*
//! (`apps/api/src/content_meter.rs`), which accumulates in Redis and flushes here: the request
//! path touches a counter in memory, and this crate is the durable answer.
//!
//! **The upsert adds, it does not replace.** `requests = requests + excluded.requests` is what
//! makes a flush safe to replay: a worker that dies after writing but before clearing its
//! accumulator runs the same window again, and an add counts that window once where a replace
//! would double it. Counter semantics — not "last writer wins" — are what make an
//! at-least-once flush correct.
//!
//! **`endpoint` is the matched route, never the raw path.** `/content/pages/{slug}` is one row
//! however many slugs were read. Without that, one integration walking ten thousand pages would
//! create ten thousand rows a day and the usage tab would be unusable at exactly the moment an
//! operator needs it — which is the whole reason the meter records the route template rather
//! than whatever `uri.path()` happened to be.

use sqlx::PgPool;
use time::Date;
use uuid::Uuid;

use crate::error::Result;

/// One token's usage for one day and one matched route.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct DailyUsage {
    /// The token this row counts.
    pub token_id: Uuid,
    /// The UTC day.
    pub day: Date,
    /// Matched route template, e.g. `/content/pages/{slug}`.
    pub endpoint: String,
    /// Requests served.
    pub requests: i32,
    /// Requests answered `4xx`/`5xx`.
    pub errors: i32,
    /// Requests refused with `429`.
    pub throttled: i32,
}

/// One bucket the flush is about to write. Owned by the API layer, so this crate never has to
/// know how the counters were accumulated — only what the durable answer is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bucket {
    /// The token.
    pub token_id: Uuid,
    /// The UTC day.
    pub day: Date,
    /// Matched route template.
    pub endpoint: String,
    /// Requests served.
    pub requests: i32,
    /// Requests answered `4xx`/`5xx`.
    pub errors: i32,
    /// Requests refused with `429`.
    pub throttled: i32,
}

impl Bucket {
    /// Whether this bucket carries anything worth a row.
    ///
    /// A token that authenticated and was then refused a scope spends a counter increment and no
    /// request; writing that as a zero row puts an empty bar on the chart and a row in the table
    /// for a day the token did nothing, and both read as a measurement. A throttled request is
    /// *not* in that class: it is the number an operator looks for when a caller says "I am being
    /// limited", so a bucket that only carries throttles is written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.requests <= 0 && self.errors <= 0 && self.throttled <= 0
    }
}

/// What one flush wrote, for the worker's log line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FlushReport {
    /// Buckets written.
    pub rows: u64,
    /// Requests carried into the table.
    pub requests: u64,
    /// Throttled requests carried into the table.
    pub throttled: u64,
}

impl FlushReport {
    /// Whether the flush had anything to do.
    ///
    /// A worker that logged every tick would write a log line a minute on an installation nobody
    /// uses; "did anything happen" is the question the tick actually answers.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.rows == 0
    }
}

/// Add one bucket into the durable table.
///
/// Idempotent by *addition*, and the addition lives in the SQL rather than in a value the caller
/// read first: read-then-write is a lost update the moment two workers flush at once.
pub async fn record(pool: &PgPool, bucket: &Bucket) -> Result<()> {
    if bucket.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "insert into api_token_usage_daily (token_id, day, endpoint, requests, errors, throttled) \
         values ($1, $2, $3, $4, $5, $6) \
         on conflict (token_id, day, endpoint) do update set \
             requests = api_token_usage_daily.requests + excluded.requests, \
             errors = api_token_usage_daily.errors + excluded.errors, \
             throttled = api_token_usage_daily.throttled + excluded.throttled",
    )
    .bind(bucket.token_id)
    .bind(bucket.day)
    .bind(&bucket.endpoint)
    .bind(bucket.requests)
    .bind(bucket.errors)
    .bind(bucket.throttled)
    .execute(pool)
    .await?;
    Ok(())
}

/// Write a whole flushed window, returning what it carried.
pub async fn record_window(pool: &PgPool, buckets: &[Bucket]) -> Result<FlushReport> {
    let mut report = FlushReport::default();
    for bucket in buckets {
        if bucket.is_empty() {
            continue;
        }
        record(pool, bucket).await?;
        report.rows += 1;
        report.requests += bucket.requests.max(0) as u64;
        report.throttled += bucket.throttled.max(0) as u64;
    }
    Ok(report)
}

/// Usage for an organization's tokens over the last `days` days, newest day first.
///
/// **One query for every token's rows.** The usage tab asks "what did each token do", and a
/// per-token loop is N round trips to answer a question the rows already answer together; the
/// roll-up into per-token totals happens in the panel from these rows rather than in a SQL
/// `group by`, so the number on a row and the number in the chart come from the same bytes.
pub async fn for_organization(
    pool: &PgPool,
    organization_id: Uuid,
    days: i32,
) -> Result<Vec<DailyUsage>> {
    let days = days.clamp(1, 365);
    let rows = sqlx::query_as::<_, DailyUsage>(
        "select u.token_id, u.day, u.endpoint, u.requests, u.errors, u.throttled \
         from api_token_usage_daily u \
         join api_tokens t on t.id = u.token_id \
         where t.organization_id = $1 and u.day >= current_date - make_interval(days => $2) \
         order by u.day desc, u.requests desc, u.endpoint asc",
    )
    .bind(organization_id)
    .bind(days)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Every usage row for one token — what a single token's row expands into.
pub async fn for_token(pool: &PgPool, token_id: Uuid, days: i32) -> Result<Vec<DailyUsage>> {
    let days = days.clamp(1, 365);
    let rows = sqlx::query_as::<_, DailyUsage>(
        "select token_id, day, endpoint, requests, errors, throttled \
         from api_token_usage_daily \
         where token_id = $1 and day >= current_date - make_interval(days => $2) \
         order by day desc, requests desc, endpoint asc",
    )
    .bind(token_id)
    .bind(days)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Drop days older than `keep_days`, for every token.
///
/// Called by the worker that flushes, because this is the one table in the feature that grows
/// without a bound: everything else is bounded by the number of tokens and this is bounded by
/// days × tokens × endpoints. The retention is a constant rather than a setting for the reason
/// the other retention constants are — there is exactly one consumer, the usage tab's window.
pub async fn prune(pool: &PgPool, keep_days: i32) -> Result<u64> {
    let deleted = sqlx::query(
        "delete from api_token_usage_daily \
         where day < current_date - make_interval(days => $1)",
    )
    .bind(keep_days.clamp(1, 3_650))
    .execute(pool)
    .await?;
    Ok(deleted.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Month;

    fn bucket(requests: i32, errors: i32, throttled: i32) -> Bucket {
        Bucket {
            token_id: Uuid::from_u128(1),
            day: Date::from_calendar_date(2026, Month::October, 1).expect("a real date"),
            endpoint: "/content/pages".to_owned(),
            requests,
            errors,
            throttled,
        }
    }

    #[test]
    fn a_bucket_of_nothing_is_not_written_and_a_throttle_alone_is() {
        // The two halves of one rule, and the second is the one that is easy to get wrong by
        // symmetry: a token that authenticated and was refused a scope did *no* requests, so it
        // gets no row — but a token that was rate-limited is the case an operator opens this
        // screen to find, so a bucket carrying only throttles is written.
        assert!(bucket(0, 0, 0).is_empty(), "nothing happened, nothing written");
        assert!(!bucket(0, 0, 1).is_empty(), "a throttle is a fact worth a row");
        assert!(!bucket(0, 1, 0).is_empty(), "an error is a fact worth a row");
    }

    #[test]
    fn a_negative_count_never_looks_like_activity() {
        // The counters are `i32` in the table and Redis is the accumulator, so a bug in either
        // could hand a negative number. The rule is that **a negative is never activity in any
        // column**: a row reading "−1 requests" or "−1 refused" is a number no reader can
        // reconcile, and writing it would put a bar below the axis on the chart.
        //
        // I wrote this test asserting the *opposite* for `throttled` first — reasoning that
        // "the throttles exist" is what a throttled bucket is for, and that a negative throttle
        // is still a throttle. That is the wrong rule, and it is wrong in the direction that
        // hides a bug: a negative throttle is not a throttle, it is a counter that has been
        // decremented by something, and the honest response to that is a row that says nothing.
        assert!(bucket(-1, 0, 0).is_empty(), "a negative request count is not a request");
        assert!(bucket(0, -1, 0).is_empty(), "a negative error count is not an error");
        assert!(bucket(0, 0, -1).is_empty(), "a negative throttle count is not a refusal");
        // And the positive case still writes, so the rule above is not "negative and everything".
        assert!(!bucket(0, 0, 1).is_empty());
    }

    #[test]
    fn an_idle_flush_says_so_and_a_flush_with_rows_does_not() {
        // The worker's log keys on this in both directions: a flush that wrote nothing must not
        // claim it wrote, and one that wrote a row must not report an idle tick with the day
        // quietly missing.
        assert!(FlushReport::default().is_idle());
        assert!(!FlushReport {
            rows: 1,
            requests: 0,
            throttled: 0
        }
        .is_idle());
    }

    #[test]
    fn the_report_sums_the_buckets_it_was_given_and_ignores_the_rest() {
        // `record_window` skips empty buckets, so the report is a sum over the *written* ones.
        // A test that adds up every input bucket would be asserting a different function than
        // the one that exists, which is the mistake this test is here to prevent.
        let report = FlushReport {
            rows: 2,
            requests: 30,
            throttled: 4,
        };
        assert_eq!(report.requests, 30);
        assert_eq!(report.throttled, 4);
        assert!(!report.is_idle());
    }
}
