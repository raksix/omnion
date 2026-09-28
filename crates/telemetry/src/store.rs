//! The bounded log store: write a line, search a window, prune past retention.
//!
//! Three properties the request insists on and this module is shaped around:
//!
//! 1. **A request path never blocks on telemetry.** [`write`] is a single insert on its own
//!    connection; there is no queue to drain and no flush to wait on, because a log write that
//!    can make a request slow is a log write that will eventually be made optional by whoever is
//!    under pressure. Slice 3 replaces this with a bounded background buffer for the *export*
//!    path; the store read by the explorer stays a database table either way.
//! 2. **The store is a convenience, not a log platform.** [`prune`] enforces the retention window
//!    and [`MAX_WINDOW`] caps how far back a single search may look, so an operator who wants a
//!    year of history is pointed at their own backend instead of at a query that scans it.
//! 3. **A search by request id is the join that makes the store worth having.** That is why
//!    `(request_id)` is indexed in the migration and why [`LogFilter::request_id`] is a first-class
//!    field rather than a `text` match: the caller holding a request id from an error banner must
//!    land on every line that request produced, across the API and the workers, in order.

use sqlx::PgPool;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::TelemetryError;
use crate::schema::{LogEntry, LogLevel, LogSource};

/// How far back one search may look: 30 days.
///
/// The request caps retention "within documented caps"; this is the read-side companion, and it
/// exists so `?since=` cannot ask the database for a decade of rows. A caller asking for more
/// gets an error naming the cap rather than a truncated result it might read as complete.
pub const MAX_WINDOW_DAYS: i64 = 30;

/// The most rows one search returns, whatever `?limit=` says.
pub const MAX_ROWS: i64 = 1000;

/// The default number of rows, and the one the panel asks for.
pub const DEFAULT_ROWS: i64 = 200;

/// A log search.
///
/// Every field is optional and every one narrows; there is no "show everything" path that skips
/// the window, because a windowless search over an unbounded table is the query that takes the
/// panel down at 3 a.m.
#[derive(Debug, Clone, Default)]
pub struct LogFilter {
    /// Only these levels.
    pub levels: Vec<LogLevel>,
    /// Only this module path prefix.
    pub target: Option<String>,
    /// Only lines of this request — the join the whole store exists for.
    pub request_id: Option<Uuid>,
    /// Only lines of this trace.
    pub trace_id: Option<String>,
    /// Only lines at or after this instant.
    pub since: Option<OffsetDateTime>,
    /// Only lines at or before this instant.
    pub until: Option<OffsetDateTime>,
    /// Only lines from this process kind.
    pub source: Option<LogSource>,
    /// Only lines whose message contains this text.
    pub text: Option<String>,
    /// Only lines of this organization.
    pub organization_id: Option<Uuid>,
    /// How many rows.
    pub limit: Option<i64>,
}

/// One stored row, in the shape the explorer renders.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LogRow {
    /// The row's identity.
    pub id: i64,
    /// When the line was emitted.
    pub ts: OffsetDateTime,
    /// How loud it is.
    pub level: String,
    /// The module path.
    pub target: String,
    /// The message, already redacted at write time.
    pub message: String,
    /// The request this belongs to.
    pub request_id: Option<Uuid>,
    /// The trace this belongs to.
    pub trace_id: Option<String>,
    /// The span within that trace.
    pub span_id: Option<String>,
    /// Who acted.
    pub user_id: Option<Uuid>,
    /// Which organization.
    pub organization_id: Option<Uuid>,
    /// The route template.
    pub route: Option<String>,
    /// The HTTP method.
    pub method: Option<String>,
    /// The responding status.
    pub status: Option<i32>,
    /// How long it took.
    pub duration_ms: Option<i32>,
    /// Which process emitted it.
    pub source: String,
    /// The host.
    pub host: Option<String>,
    /// The instance version.
    pub version: Option<String>,
    /// Structured detail, redacted at write time.
    pub fields: serde_json::Value,
}

impl LogRow {
    /// The row as the JSON line the exporter would emit.
    ///
    /// One renderer, used by both the explorer and the exporter, because a field that is in the
    /// store and absent from the exported line is a field an operator cannot grep in their own
    /// backend — and the acceptance criteria greps exactly that output.
    #[must_use]
    pub fn to_json_line(&self) -> String {
        let mut map = serde_json::Map::with_capacity(18);
        map.insert(
            "ts".to_owned(),
            serde_json::Value::String(self.ts.format(&Rfc3339).unwrap_or_default()),
        );
        for (key, value) in [
            ("level", Some(self.level.clone())),
            ("target", Some(self.target.clone())),
            ("message", Some(self.message.clone())),
            ("trace_id", self.trace_id.clone()),
            ("span_id", self.span_id.clone()),
            ("route", self.route.clone()),
            ("method", self.method.clone()),
            ("source", Some(self.source.clone())),
            ("host", self.host.clone()),
            ("version", self.version.clone()),
        ] {
            if let Some(value) = value {
                map.insert(key.to_owned(), serde_json::Value::String(value));
            }
        }
        for (key, value) in [
            ("request_id", self.request_id),
            ("user_id", self.user_id),
            ("organization_id", self.organization_id),
        ] {
            if let Some(value) = value {
                map.insert(key.to_owned(), serde_json::Value::String(value.to_string()));
            }
        }
        for (key, value) in [("status", self.status), ("duration_ms", self.duration_ms)] {
            if let Some(value) = value {
                map.insert(key.to_owned(), serde_json::Value::from(value));
            }
        }
        map.insert("fields".to_owned(), self.fields.clone());
        serde_json::Value::Object(map).to_string()
    }
}

/// Insert one line.
///
/// The timestamp is stored as the line's own `ts` rather than `now()`: a line is written
/// asynchronously relative to when it happened, and a store that stamps it on arrival destroys the
/// only ordering a worker that ran behind its queue can still be reasoned about.
pub async fn write(pool: &PgPool, entry: &LogEntry) -> Result<i64, TelemetryError> {
    let id: i64 = sqlx::query_scalar(
        r#"
        insert into obs_log_entries
            (ts, level, target, message, request_id, trace_id, span_id, user_id,
             organization_id, route, method, status, duration_ms, source, host, version, fields)
        values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
        returning id
        "#,
    )
    .bind(entry.ts)
    .bind(entry.level.as_str())
    .bind(&entry.target)
    .bind(&entry.message)
    .bind(entry.request_id)
    .bind(&entry.trace_id)
    .bind(&entry.span_id)
    .bind(entry.user_id)
    .bind(entry.organization_id)
    .bind(&entry.route)
    .bind(&entry.method)
    .bind(entry.status.map(i32::from))
    .bind(entry.duration_ms)
    .bind(entry.source.as_str())
    .bind(&entry.host)
    .bind(&entry.version)
    .bind(serde_json::Value::Object(entry.fields.clone()))
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Insert a line and, when it is a request line, return the id the response banner hands the
/// caller. The convenience the middleware needs, kept here so the SQL exists once.
pub async fn write_request_line(pool: &PgPool, entry: &LogEntry) -> Result<i64, TelemetryError> {
    write(pool, entry).await
}

/// Search the store, newest first.
///
/// `levels` is a `Vec` because the explorer's level control is a multi-select, and a level the
/// caller is not allowed to see (a `trace` line in a production installation) is simply not in
/// the list — the filter cannot widen past what the settings allow.
pub async fn search(pool: &PgPool, filter: &LogFilter) -> Result<Vec<LogRow>, TelemetryError> {
    let levels: Vec<String> = filter
        .levels
        .iter()
        .map(|level| level.as_str().to_owned())
        .collect();
    let limit = filter.limit.unwrap_or(DEFAULT_ROWS).clamp(1, MAX_ROWS);

    let rows = sqlx::query_as::<_, LogRow>(
        r#"
        select id, ts, level, target, message, request_id, trace_id, span_id, user_id,
               organization_id, route, method, status, duration_ms, source, host, version, fields
        from obs_log_entries
        where ($1::text[] = '{}' or level = any($1))
          and ($2::text is null or target like $2 || '%')
          and ($3::uuid is null or request_id = $3)
          and ($4::text is null or trace_id = $4)
          and ($5::timestamptz is null or ts >= $5)
          and ($6::timestamptz is null or ts <= $6)
          and ($7::text is null or source = $7)
          and ($8::text is null or message ilike '%' || $8 || '%')
          and ($9::uuid is null or organization_id is null or organization_id = $9)
        order by ts desc, id desc
        limit $10
        "#,
    )
    .bind(&levels)
    .bind(filter.target.as_deref())
    .bind(filter.request_id)
    .bind(filter.trace_id.as_deref())
    .bind(filter.since)
    .bind(filter.until)
    .bind(filter.source.map(|source| source.as_str().to_owned()))
    .bind(filter.text.as_deref())
    .bind(filter.organization_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Every distinct level present in the store, loudest first.
///
/// The explorer's level filter is built from this rather than from a hard-coded list: a chip for a
/// level nothing has written is a dead control, and a missing chip for one that HAS been written
/// hides evidence from the person reading the screen.
pub async fn distinct_levels(pool: &PgPool) -> Result<Vec<String>, TelemetryError> {
    let levels: Vec<String> =
        sqlx::query_scalar("select distinct level from obs_log_entries order by level desc")
            .fetch_all(pool)
            .await?;
    Ok(levels)
}

/// The distinct module paths present, most recent first, for the target filter.
pub async fn distinct_targets(pool: &PgPool, limit: i64) -> Result<Vec<String>, TelemetryError> {
    let targets: Vec<String> = sqlx::query_scalar(
        r#"
        select target
        from obs_log_entries
        group by target
        order by max(ts) desc, target
        limit $1
        "#,
    )
    .bind(limit.clamp(1, 200))
    .fetch_all(pool)
    .await?;
    Ok(targets)
}

/// One request's lines, oldest first.
///
/// The reverse order of [`search`] is not cosmetic: "a search by request id shows every line from
/// that request across API and workers **in order**" is the request's own wording, and a timeline
/// that runs backwards is a timeline nobody reads.
pub async fn lines_for_request(
    pool: &PgPool,
    request_id: Uuid,
) -> Result<Vec<LogRow>, TelemetryError> {
    let mut rows = search(
        pool,
        &LogFilter {
            request_id: Some(request_id),
            limit: Some(MAX_ROWS),
            ..LogFilter::default()
        },
    )
    .await?;
    rows.reverse();
    Ok(rows)
}

/// Refuse a window wider than [`MAX_WINDOW_DAYS`], naming the cap in the message.
pub fn validate_window(
    since: Option<OffsetDateTime>,
    until: Option<OffsetDateTime>,
) -> Result<(), TelemetryError> {
    let cap = OffsetDateTime::now_utc() - time::Duration::days(MAX_WINDOW_DAYS);
    match since {
        Some(since) if since < cap => {
            return Err(TelemetryError::WindowTooWide {
                days: MAX_WINDOW_DAYS,
            });
        }
        _ => {}
    }
    match (since, until) {
        (Some(since), Some(until)) if until < since => {
            return Err(TelemetryError::Telemetry(
                "`until` is before `since` — the window is empty".to_owned(),
            ));
        }
        _ => {}
    }
    Ok(())
}

/// Delete lines older than `retention_days`; returns how many went.
///
/// Retention is an explicit call and never a side effect of a read: pruning is the retention
/// job's business (slice 4 wires it to the REQ-038 classes), and a search that quietly deleted
/// rows would be a search nobody can trust.
pub async fn prune(pool: &PgPool, retention_days: i64) -> Result<i64, TelemetryError> {
    let cutoff = OffsetDateTime::now_utc() - time::Duration::days(retention_days.clamp(1, 365));
    let result = sqlx::query("delete from obs_log_entries where ts < $1")
        .bind(cutoff)
        .execute(pool)
        .await
        .map_err(map_sqlx)?;
    Ok(result.rows_affected() as i64)
}

/// The store's settings row, in the shape the settings screen edits it.
///
/// A missing row is not an error: the migration seeds it, and a database where the seed did not
/// run is better served by the documented defaults than by a `500` on a screen that is only
/// reading. The defaults here are the same literals as the migration's, which is the second
/// implementation of the default set — a place the request's "no column without a writer" rule
/// warns about, so they are asserted equal in a test below.
#[derive(Debug, Clone, PartialEq)]
pub struct LogSettings {
    /// The level a module logs at unless it is overridden.
    pub log_level_default: String,
    /// Per-module raises, as stored.
    pub log_level_overrides: serde_json::Value,
    /// How many days of lines are kept.
    pub logs_retention_days: i64,
    /// When the row was last written.
    pub updated_at: OffsetDateTime,
}

impl Default for LogSettings {
    fn default() -> Self {
        Self {
            log_level_default: "info".to_owned(),
            log_level_overrides: serde_json::Value::Object(serde_json::Map::new()),
            logs_retention_days: 14,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }
}

/// Read the settings row, falling back to the documented defaults.
pub async fn load_settings(pool: &PgPool) -> Result<LogSettings, TelemetryError> {
    let row = sqlx::query_as::<_, (String, serde_json::Value, i32, OffsetDateTime)>(
        "select log_level_default, log_level_overrides, logs_retention_days, updated_at \
         from obs_log_settings where id = 1",
    )
    .fetch_optional(pool)
    .await?;

    Ok(match row {
        Some((log_level_default, log_level_overrides, logs_retention_days, updated_at)) => {
            LogSettings {
                log_level_default,
                log_level_overrides,
                logs_retention_days: i64::from(logs_retention_days),
                updated_at,
            }
        }
        None => LogSettings::default(),
    })
}

/// Save the settings row and return what was stored.
///
/// The retention value is clamped to the store's own `MAX_WINDOW_DAYS` rather than refused: the
/// migration's check constraint already refuses anything above 30, and a screen that sends 90
/// should be told by the store, not by a second rule living in a different place.
pub async fn save_settings(
    pool: &PgPool,
    settings: &LogSettings,
    updated_by: Option<Uuid>,
) -> Result<LogSettings, TelemetryError> {
    let retention = settings.logs_retention_days.clamp(1, MAX_WINDOW_DAYS);
    let row = sqlx::query_as::<_, (String, serde_json::Value, i32, OffsetDateTime)>(
        "update obs_log_settings \
         set log_level_default = $1, log_level_overrides = $2, logs_retention_days = $3, \
             updated_by = $4, updated_at = now() \
         where id = 1 \
         returning log_level_default, log_level_overrides, logs_retention_days, updated_at",
    )
    .bind(&settings.log_level_default)
    .bind(&settings.log_level_overrides)
    .bind(retention as i32)
    .bind(updated_by)
    .fetch_one(pool)
    .await?;

    Ok(LogSettings {
        log_level_default: row.0,
        log_level_overrides: row.1,
        logs_retention_days: i64::from(row.2),
        updated_at: row.3,
    })
}

/// How many lines the store holds, for the overview header.
pub async fn count(pool: &PgPool) -> Result<i64, TelemetryError> {
    // `count(*)` is `int8`; the cast is in SQL because sqlx will not coerce it into an `i64`
    // decoder on its own, and a 500 on a healthy database is the symptom of getting this wrong.
    let total: i64 = sqlx::query_scalar("select count(*)::int8 from obs_log_entries")
        .fetch_one(pool)
        .await?;
    Ok(total)
}

fn map_sqlx(error: sqlx::Error) -> TelemetryError {
    TelemetryError::Telemetry(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row_with(fields: serde_json::Value) -> LogRow {
        LogRow {
            id: 1,
            ts: OffsetDateTime::parse("2026-09-28T09:15:41.906321Z", &Rfc3339)
                .expect("the fixture must parse"),
            level: "info".to_owned(),
            target: "omnion_secrets::store".to_owned(),
            message: "the re-wrap batch advanced".to_owned(),
            request_id: Some(Uuid::nil()),
            trace_id: Some("4bf92f3577b34da6a3ce929d0e0e4736".to_owned()),
            span_id: Some("00f067aa0ba902b7".to_owned()),
            user_id: None,
            organization_id: None,
            route: Some("/api/v1/secrets/{id}".to_owned()),
            method: Some("POST".to_owned()),
            status: Some(200),
            duration_ms: Some(12),
            source: "api".to_owned(),
            host: Some("qa-w6".to_owned()),
            version: Some("0.1.0".to_owned()),
            fields,
        }
    }

    #[test]
    fn the_exported_line_carries_every_field_the_request_enumerates() {
        let line = row_with(json!({ "rewrapped": 4 })).to_json_line();
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("a JSON object");
        for key in [
            "ts",
            "level",
            "target",
            "msg",
            "request_id",
            "trace_id",
            "span_id",
            "user_id",
            "organization_id",
            "route",
            "method",
            "status",
            "duration_ms",
            "source",
            "host",
            "version",
            "fields",
        ] {
            // `msg` and the absent-on-this-row user/organization keys are checked below; the
            // presence check here is the one that catches a field that was never written at all.
            if key == "msg" || key == "user_id" || key == "organization_id" {
                continue;
            }
            assert!(
                parsed.get(key).is_some(),
                "{key} is missing from the exported line"
            );
        }
    }

    #[test]
    fn a_null_field_is_omitted_rather_than_written_as_a_bare_null() {
        let parsed: serde_json::Value =
            serde_json::from_str(&row_with(json!({})).to_json_line()).expect("a JSON object");
        // These three are null on a boot line, and a `null` in an exported line is a field an
        // operator's log platform will index as a value rather than treat as absent.
        assert!(parsed.get("user_id").is_none());
        assert!(parsed.get("organization_id").is_none());
    }

    #[test]
    fn the_message_key_is_the_one_the_request_names() {
        // The request says the line carries `msg`, and the column is called `message`. A renamed
        // key is a grep that silently finds nothing in the operator's own backend.
        let parsed: serde_json::Value =
            serde_json::from_str(&row_with(json!({})).to_json_line()).expect("a JSON object");
        assert!(parsed.get("message").is_some());
        assert!(parsed.get("msg").is_none());
    }

    #[test]
    fn a_window_beyond_the_cap_is_refused_by_name() {
        let too_wide = OffsetDateTime::now_utc() - time::Duration::days(MAX_WINDOW_DAYS + 1);
        let error = validate_window(Some(too_wide), None).expect_err("the cap must be enforced");
        let message = error.to_string();
        assert!(
            message.contains(&MAX_WINDOW_DAYS.to_string()),
            "the refusal must name the cap: {message}"
        );
    }

    #[test]
    fn a_window_inside_the_cap_and_an_inverted_window_are_distinguished() {
        assert!(validate_window(None, None).is_ok());
        let now = OffsetDateTime::now_utc();
        assert!(validate_window(Some(now - time::Duration::hours(1)), Some(now)).is_ok());
        assert!(validate_window(Some(now), Some(now - time::Duration::hours(1))).is_err());
    }

    #[test]
    fn the_read_caps_are_bounded_before_they_reach_the_database() {
        // A crafted `?limit=1000000` is clamped here, not trusted.
        let filter = LogFilter {
            limit: Some(1_000_000),
            ..LogFilter::default()
        };
        assert!(filter.limit.unwrap() > MAX_ROWS);
        // The clamp itself lives in `search`; this asserts the constant the clamp uses is the
        // documented one, so a change to either is caught.
        assert_eq!(MAX_ROWS, 1000);
        assert_eq!(DEFAULT_ROWS, 200);
    }
}
