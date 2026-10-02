//! The trace index: persistence and search (REQ-126, slice 3).
//!
//! Read the migration header first — this is an *index*, not a span store, and the cap on inline
//! spans is what keeps it one. What lives here is the three operations the screen needs and
//! nothing more:
//!
//! * [`upsert`] — fold a finished span into its trace, creating the row on the first span.
//! * [`search`] — the bounded trace search (request id, route, status, minimum duration, window).
//! * [`load`] — one trace with its waterfall.
//!
//! ## Why a span is never inserted on its own
//!
//! A trace is one row. Inserting spans individually would mean a `select *` per span to decide
//! whether the row exists, which is a read on the request path for every traced request. `upsert`
//! takes the whole `TraceRecord` the in-process collector holds, so the write is one statement and
//! the request path never reads.

use serde_json::Value;
use sqlx::PgPool;
// `PgRow::get` is a trait method; without this the column reads look like they are missing from
// the type, and the error names the method rather than the import.
use sqlx::Row;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::TelemetryError;
use crate::tracing_span::{Span, TraceRecord, TraceSummary};

/// The cap on rows one search returns, and the default below it.
pub const MAX_TRACE_ROWS: i64 = 200;
/// The default page size.
pub const DEFAULT_TRACE_ROWS: i64 = 50;

/// The search's filters.
///
/// Every field is optional and every one that is set is applied as an `and`, because a trace
/// search is an intersection: an operator narrowing from "errors" to "errors on this route in the
/// last ten minutes" is the whole point of the screen.
#[derive(Debug, Default, Clone)]
pub struct TraceFilter {
    /// Only traces of this request — the jump from a log line to its trace.
    pub request_id: Option<Uuid>,
    /// Only traces of this route template.
    pub route: Option<String>,
    /// Only `ok` or only `error`.
    pub status: Option<String>,
    /// Only traces at least this slow, in milliseconds.
    pub min_duration_ms: Option<i64>,
    /// Only traces with at least this many spans.
    pub min_spans: Option<i64>,
    /// Only traces at or after this instant.
    pub since: Option<OffsetDateTime>,
    /// How many rows.
    pub limit: i64,
}

impl TraceFilter {
    /// A filter with the page size bounded.
    #[must_use]
    pub fn with_limit(mut self, limit: Option<i64>) -> Self {
        self.limit = limit.unwrap_or(DEFAULT_TRACE_ROWS).clamp(1, MAX_TRACE_ROWS);
        self
    }
}

/// Create or replace one trace row.
pub async fn upsert(pool: &PgPool, record: &TraceRecord) -> Result<(), TelemetryError> {
    let spans = serde_json::to_value(&record.spans).map_err(|error| {
        TelemetryError::Telemetry(format!("the trace spans did not serialise: {error}"))
    })?;

    // `sampling` is written as a literal rather than bound: it is one of five closed values from
    // `SamplingDecision::as_str`, and a closed set in a query is cheaper to read than a parameter
    // whose every caller has to remember to derive.
    sqlx::query(
        "insert into obs_trace_index ( \
             trace_id, root_name, service, route, request_id, started_at, duration_ms, \
             span_count, spans_kept, spans_truncated, status, sampled, sampling, \
             backend_trace_url, spans) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) \
         on conflict (trace_id) do update set \
             duration_ms = greatest(obs_trace_index.duration_ms, excluded.duration_ms), \
             span_count = excluded.span_count, \
             spans_kept = excluded.spans_kept, \
             spans_truncated = excluded.spans_truncated, \
             status = excluded.status, \
             -- The request id and the route ARE updated, and this is not cosmetic. A consumer
             -- process appends its spans to a trace the producer opened, and a producer that
             -- replays under a new request would otherwise leave the FIRST request id on the row
             -- forever: a request-id search would then miss a trace that is genuinely that
             -- request's, which is the one lookup the screen exists for.
             request_id = coalesce(excluded.request_id, obs_trace_index.request_id), \
             sampling = excluded.sampling, \
             route = coalesce(excluded.route, obs_trace_index.route), \
             root_name = excluded.root_name, \
             backend_trace_url = coalesce(excluded.backend_trace_url, obs_trace_index.backend_trace_url), \
             spans = excluded.spans",
    )
    .bind(&record.trace_id)
    .bind(&record.root_name)
    .bind(&record.service)
    .bind(record.route.as_deref())
    .bind(record.request_id)
    .bind(record.started_at)
    .bind(record.duration_ms)
    .bind(record.span_count)
    .bind(record.spans_kept)
    .bind(record.spans_truncated)
    .bind(&record.status)
    .bind(record.sampled)
    .bind(&record.sampling)
    .bind(record.backend_trace_url.as_deref())
    .bind(&spans)
    .execute(pool)
    .await?;

    Ok(())
}

/// One trace with its waterfall.
pub async fn load(pool: &PgPool, trace_id: &str) -> Result<Option<TraceRecord>, TelemetryError> {
    let row = sqlx::query(
        "select trace_id, root_name, service, route, request_id, started_at, duration_ms, \
                span_count, spans_kept, spans_truncated, status, sampled, sampling, \
                backend_trace_url, spans \
         from obs_trace_index where trace_id = $1",
    )
    .bind(trace_id)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else { return Ok(None) };

    let spans: Vec<Span> = serde_json::from_value(row.get("spans")).map_err(|error| {
        TelemetryError::Telemetry(format!("the stored spans did not decode: {error}"))
    })?;

    Ok(Some(TraceRecord {
        trace_id: row.get("trace_id"),
        root_name: row.get("root_name"),
        service: row.get("service"),
        route: row.get("route"),
        request_id: row.get("request_id"),
        started_at: row.get("started_at"),
        // Same INT4 → i64 widening as the search projection, for the same reason.
        duration_ms: i64::from(row.get::<i32, _>("duration_ms")),
        span_count: i64::from(row.get::<i32, _>("span_count")),
        spans_kept: i64::from(row.get::<i32, _>("spans_kept")),
        spans_truncated: row.get("spans_truncated"),
        status: row.get("status"),
        sampled: row.get("sampled"),
        sampling: row.get("sampling"),
        backend_trace_url: row.get("backend_trace_url"),
        spans,
    }))
}

/// The bounded trace search.
pub async fn search(
    pool: &PgPool,
    filter: &TraceFilter,
) -> Result<Vec<TraceSummary>, TelemetryError> {
    // Every filter is a bound parameter and the list is assembled in Rust rather than concatenated
    // into SQL text: a search box that interpolates a route into a statement is a route template
    // the operator cannot type without quoting it.
    let mut clauses: Vec<String> = Vec::new();
    if filter.request_id.is_some() {
        clauses.push(format!("request_id = ${}", clauses.len() + 1));
    }
    if filter.route.is_some() {
        clauses.push(format!("route = ${}", clauses.len() + 1));
    }
    if filter.status.is_some() {
        clauses.push(format!("status = ${}", clauses.len() + 1));
    }
    if filter.min_duration_ms.is_some() {
        clauses.push(format!("duration_ms >= ${}", clauses.len() + 1));
    }
    if filter.min_spans.is_some() {
        clauses.push(format!("span_count >= ${}", clauses.len() + 1));
    }
    if filter.since.is_some() {
        clauses.push(format!("started_at >= ${}", clauses.len() + 1));
    }

    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!(" where {}", clauses.join(" and "))
    };
    let limit_param = clauses.len() + 1;

    let sql = format!(
        "select trace_id, root_name, service, route, request_id, started_at, duration_ms, \
                span_count, status, sampled, sampling \
         from obs_trace_index{where_clause} \
         order by started_at desc \
         limit ${limit_param}"
    );

    let mut query = sqlx::query(&sql);
    if let Some(value) = filter.request_id {
        query = query.bind(value);
    }
    if let Some(value) = &filter.route {
        query = query.bind(value.as_str());
    }
    if let Some(value) = &filter.status {
        query = query.bind(value.as_str());
    }
    if let Some(value) = filter.min_duration_ms {
        query = query.bind(value);
    }
    if let Some(value) = filter.min_spans {
        query = query.bind(value);
    }
    if let Some(value) = filter.since {
        query = query.bind(value);
    }
    let rows = query
        .bind(filter.limit.clamp(1, MAX_TRACE_ROWS))
        .fetch_all(pool)
        .await?;

    let summaries = rows
        .iter()
        .map(|row| {
            let started_at: OffsetDateTime = row.get("started_at");
            let sampled: bool = row.get("sampled");
            TraceSummary {
                trace_id: row.get("trace_id"),
                root_name: row.get("root_name"),
                service: row.get("service"),
                route: row.get("route"),
                request_id: row.get("request_id"),
                started_at: started_at
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_else(|_| started_at.to_string()),
                // `duration_ms` and `span_count` are `integer` (INT4) in the migration and `i64`
                // here. The cast is in the SQL, not in Rust: `i64::from(i32)` is exact, and a
                // decode that silently widened would report a wrong duration rather than fail.
                duration_ms: i64::from(row.get::<i32, _>("duration_ms")),
                span_count: i64::from(row.get::<i32, _>("span_count")),
                status: row.get("status"),
                sampled,
                sampling: row.get("sampling"),
            }
        })
        .collect();

    Ok(summaries)
}

/// Prune trace rows past the retention window.
///
/// The retention class is the log store's own (REQ-038), and the point of the function is what it
/// does *not* touch: audit rows and health incidents live in other tables and are not reachable
/// from here. A retention job that deleted an audit row would be a compliance bug, so the absence
/// is the interesting property and the test asserts the audit row survives.
pub async fn prune(pool: &PgPool, retention_days: i64) -> Result<i64, TelemetryError> {
    // `make_interval`'s `days` parameter is `integer`, NOT `bigint`. Binding an `i64` sends a
    // bigint, and PostgreSQL has no implicit bigint→integer cast for named parameters, so the
    // whole statement fails with `function make_interval(days => bigint) does not exist` — on
    // EVERY call, for EVERY window.
    //
    // This shipped broken and nothing caught it for a tick, because the retention sweep treats
    // a failed prune as a warning and carries on: `trace_rows: 0` looks exactly like "nothing was
    // old enough to prune". The integration walk is what found it, by asserting the row was
    // GONE from PostgreSQL rather than trusting the report. The cast is in SQL because that is
    // where the type lives.
    let result = sqlx::query(
        "delete from obs_trace_index \
         where started_at < now() - make_interval(days => $1::int)",
    )
    .bind(i32::try_from(retention_days.clamp(1, MAX_TRACE_RETENTION_DAYS)).unwrap_or(1))
    .execute(pool)
    .await?;
    Ok(i64::try_from(result.rows_affected()).unwrap_or(i64::MAX))
}

/// How many traces the index holds.
pub async fn count(pool: &PgPool) -> Result<i64, TelemetryError> {
    let total: i64 = sqlx::query_scalar("select count(*)::bigint from obs_trace_index")
        .fetch_one(pool)
        .await?;
    Ok(total)
}

/// The trace context a delivery row carries, if any.
pub async fn delivery_context(
    pool: &PgPool,
    delivery_id: Uuid,
) -> Result<Option<Value>, TelemetryError> {
    let context = sqlx::query_scalar("select trace_context from webhook_deliveries where id = $1")
        .bind(delivery_id)
        .fetch_optional(pool)
        .await?;
    Ok(context)
}

/// Stamp the producing request's context onto a delivery row at enqueue time.
///
/// This is the write that makes "the consumer span links back to the producer" true, and it has to
/// happen at enqueue: the consumer runs later, possibly in another process, and by then the
/// producer's task-local context is gone. There is no way to reconstruct the link after the fact,
/// which is why this is a column and not something the consumer infers.
pub async fn stamp_delivery(
    executor: impl sqlx::PgExecutor<'_>,
    delivery_id: Uuid,
    context: &Value,
) -> Result<(), TelemetryError> {
    sqlx::query("update webhook_deliveries set trace_context = $2 where id = $1")
        .bind(delivery_id)
        .bind(context)
        .execute(executor)
        .await?;
    Ok(())
}

/// The default trace retention, in days.
///
/// Seven is the request's default for `traces_retention_days`, and the constant is here so the
/// pruning job and the settings screen cannot disagree about what "the default" means.
pub const DEFAULT_TRACE_RETENTION_DAYS: i64 = 7;

/// The cap on trace retention, matching the log store's documented cap.
pub const MAX_TRACE_RETENTION_DAYS: i64 = 30;

/// Refuse a retention window beyond the cap, naming it.
pub fn validate_retention(days: i64) -> Result<i64, TelemetryError> {
    if !(0..=MAX_TRACE_RETENTION_DAYS).contains(&days) {
        return Err(TelemetryError::WindowTooWide {
            days: MAX_TRACE_RETENTION_DAYS,
        });
    }
    Ok(days)
}

/// The `since` bound for a search, from a window in minutes.
#[must_use]
pub fn since_from_minutes(minutes: i64) -> OffsetDateTime {
    OffsetDateTime::now_utc()
        - Duration::minutes(minutes.clamp(1, 60 * 24 * MAX_TRACE_RETENTION_DAYS as i64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_page_size_is_clamped_to_the_documented_bounds() {
        assert_eq!(
            TraceFilter::default().with_limit(None).limit,
            DEFAULT_TRACE_ROWS
        );
        assert_eq!(TraceFilter::default().with_limit(Some(0)).limit, 1);
        assert_eq!(
            TraceFilter::default().with_limit(Some(10_000)).limit,
            MAX_TRACE_ROWS
        );
    }

    #[test]
    fn a_retention_beyond_the_cap_is_refused_and_the_cap_is_named() {
        assert!(validate_retention(DEFAULT_TRACE_RETENTION_DAYS).is_ok());
        let error = validate_retention(365).expect_err("a year of traces is refused");
        let message = error.to_string();
        assert!(
            message.contains("30 days"),
            "the refusal must name the cap: {message}"
        );
    }

    #[test]
    fn a_window_is_clamped_into_a_real_range() {
        // A zero or negative window would produce a `since` in the future, and the search would
        // answer "no traces" for every query — which reads exactly like a broken feature.
        let now = OffsetDateTime::now_utc();
        let since = since_from_minutes(-5);
        assert!(since < now, "a negative window produced a future bound");
        let far = since_from_minutes(10_000_000);
        assert!(far < now, "an absurd window was not clamped");
    }
}
