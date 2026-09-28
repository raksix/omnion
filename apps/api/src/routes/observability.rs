//! `/api/v1/observability/logs` — the bounded log explorer (REQ-126, slice 1).
//!
//! Three reads and no writes, because slice 1 ships the read path and the store; the exporters,
//! alert rules and settings the request also lists arrive in slices 3 and 4 with the behaviour
//! that writes them. What is here is the surface an operator uses when they already hold a request
//! id from an error banner and need to see what that request did.
//!
//! ## The three reads, and what each one is for
//!
//! * `GET /observability/logs` — the explorer. Filter by level, target, request id, trace id,
//!   source, window and text.
//! * `GET /observability/logs/requests/{request_id}` — **one request's lines, oldest first**.
//!   This is a separate route rather than a query parameter on purpose: the ordering is the
//!   opposite of the list (which is newest first, as every list is), and a caller who has to
//!   remember to add `&order=asc` to read a timeline will get it wrong.
//! * `GET /observability/logs/settings` / `PUT` — the one settings row.
//!
//! ## Why the filter values are validated before SQL
//!
//! `level` and `source` are closed sets, and a value outside them is a `400` with the accepted
//! list in the message rather than a query that returns nothing. A filter that silently matches
//! zero rows is indistinguishable from "the log store is empty", and that ambiguity is what makes
//! an operator conclude the platform is not logging.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_telemetry::store::{self, LogFilter, LogSettings};
use omnion_telemetry::{LogLevel, LogSource, TelemetryError};
use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

/// The log explorer's query string.
///
/// `level` is a list because the screen's control is a multi-select, and every value is checked
/// against the closed set before it reaches SQL — the same reasoning as REQ-125's action filter.
#[derive(Debug, Default, serde::Deserialize)]
pub struct LogQuery {
    /// Only these levels.
    #[serde(default, deserialize_with = "one_or_many")]
    pub level: Vec<String>,
    /// Only this module path prefix.
    pub target: Option<String>,
    /// Only lines of this request.
    pub request_id: Option<Uuid>,
    /// Only lines of this trace.
    pub trace_id: Option<String>,
    /// Only lines at or after this instant (RFC 3339).
    pub since: Option<String>,
    /// Only lines at or before this instant (RFC 3339).
    pub until: Option<String>,
    /// Only lines from this process: `api`, `worker` or `cli`.
    pub source: Option<String>,
    /// Only lines whose message contains this text.
    pub text: Option<String>,
    /// How many rows.
    pub limit: Option<i64>,
}

/// Accept `?level=info` as well as `?level=info&level=warn`.
///
/// A `Vec` in a query struct only deserializes the repeated form, so the single-value case — what
/// a link, a bookmark or a hand-typed URL produces — came back as a parse error. A filter only
/// its own client can satisfy is not a filter.
fn one_or_many<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(
        match <OneOrMany as serde::Deserialize>::deserialize(deserializer)? {
            OneOrMany::One(value) => vec![value],
            OneOrMany::Many(values) => values,
        },
    )
}

/// One log row, in the panel's shape.
#[derive(Debug, Serialize)]
pub struct LogView {
    /// The row's id.
    pub id: i64,
    /// When the line was emitted, RFC 3339.
    pub ts: String,
    /// How loud it is.
    pub level: String,
    /// The module path.
    pub target: String,
    /// The message, redacted at write time.
    pub message: String,
    /// The request this belongs to.
    pub request_id: Option<Uuid>,
    /// The trace this belongs to.
    pub trace_id: Option<String>,
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
    /// Structured detail, already redacted.
    pub fields: serde_json::Value,
}

impl From<store::LogRow> for LogView {
    fn from(row: store::LogRow) -> Self {
        Self {
            id: row.id,
            ts: row.ts.format(&Rfc3339).unwrap_or_default(),
            level: row.level,
            target: row.target,
            message: row.message,
            request_id: row.request_id,
            trace_id: row.trace_id,
            user_id: row.user_id,
            organization_id: row.organization_id,
            route: row.route,
            method: row.method,
            status: row.status,
            duration_ms: row.duration_ms,
            source: row.source,
            host: row.host,
            version: row.version,
            fields: row.fields,
        }
    }
}

/// The explorer response.
#[derive(Debug, Serialize)]
pub struct LogListResponse {
    /// The rows, newest first.
    pub entries: Vec<LogView>,
    /// The level names present in the store, so the filter's chips come from real data.
    pub levels: Vec<String>,
    /// The target names present, for the target filter.
    pub targets: Vec<String>,
    /// How many lines the store holds in total.
    pub stored_total: i64,
    /// The window the store will answer, in days.
    pub max_window_days: i64,
    /// The cap on one search, in rows.
    pub max_rows: i64,
}

/// `GET /api/v1/observability/logs` — the bounded explorer.
pub async fn read_logs(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<LogQuery>,
) -> Result<Json<LogListResponse>, ApiError> {
    let levels = parse_levels(&query.level)?;
    let source = match query.source.as_deref() {
        None => None,
        Some(text) => Some(match text {
            "api" => LogSource::Api,
            "worker" => LogSource::Worker,
            "cli" => LogSource::Cli,
            other => {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_source",
                    format!("`source` must be one of api, worker, cli — got `{other}`"),
                ));
            }
        }),
    };

    let since = parse_instant(query.since.as_deref(), "since")?;
    let until = parse_instant(query.until.as_deref(), "until")?;
    store::validate_window(since, until).map_err(map_error)?;

    let filter = LogFilter {
        levels,
        target: query.target,
        request_id: query.request_id,
        trace_id: query.trace_id,
        since,
        until,
        source,
        text: query.text,
        organization_id: session.user.organization_id,
        limit: query.limit,
    };

    let pool = state.db().pool();
    let rows = store::search(pool, &filter).await.map_err(map_error)?;
    let levels = store::distinct_levels(pool).await.map_err(map_error)?;
    let targets = store::distinct_targets(pool, 50).await.map_err(map_error)?;
    let stored_total = store::count(pool).await.map_err(map_error)?;

    Ok(Json(LogListResponse {
        entries: rows.into_iter().map(LogView::from).collect(),
        levels,
        targets,
        stored_total,
        max_window_days: store::MAX_WINDOW_DAYS,
        max_rows: store::MAX_ROWS,
    }))
}

/// `GET /api/v1/observability/logs/requests/{request_id}` — one request, in order.
///
/// The route the error banner is for. Every line the request produced, across the API and the
/// workers, oldest first.
pub async fn read_request_lines(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(request_id): Path<Uuid>,
) -> Result<Json<LogListResponse>, ApiError> {
    let pool = state.db().pool();
    let mut rows = store::lines_for_request(pool, request_id)
        .await
        .map_err(map_error)?;
    // A tenant sees its own organization's lines; a platform account sees every line. A row with
    // no organization (a boot line, a runner tick) is visible to both, because it belongs to
    // nobody and hiding it would leave the most confusing rows invisible.
    if let Some(organization_id) = session.user.organization_id {
        rows.retain(|row| row.organization_id.is_none_or(|own| own == organization_id));
    }
    let stored_total = rows.len() as i64;
    let levels = store::distinct_levels(pool).await.map_err(map_error)?;
    let targets = store::distinct_targets(pool, 50).await.map_err(map_error)?;

    Ok(Json(LogListResponse {
        entries: rows.into_iter().map(LogView::from).collect(),
        levels,
        targets,
        stored_total,
        max_window_days: store::MAX_WINDOW_DAYS,
        max_rows: store::MAX_ROWS,
    }))
}

/// The settings the screen edits.
#[derive(Debug, Serialize)]
pub struct SettingsView {
    /// The level a module logs at unless it is overridden.
    pub log_level_default: String,
    /// Per-module raises.
    pub log_level_overrides: serde_json::Value,
    /// How many days are kept.
    pub logs_retention_days: i64,
    /// When the row was last written.
    pub updated_at: String,
    /// The cap retention is allowed to reach, so the screen can say why.
    pub max_retention_days: i64,
}

impl From<LogSettings> for SettingsView {
    fn from(settings: LogSettings) -> Self {
        Self {
            log_level_default: settings.log_level_default,
            log_level_overrides: settings.log_level_overrides,
            logs_retention_days: settings.logs_retention_days,
            updated_at: settings.updated_at.format(&Rfc3339).unwrap_or_default(),
            max_retention_days: store::MAX_WINDOW_DAYS,
        }
    }
}

/// The settings body.
///
/// `deny_unknown_fields` is on: a typo in a save (`retention_days` instead of
/// `logs_retention_days`) is otherwise accepted, ignored, and reported back as if it had been
/// saved. The field the operator meant is the one that matters.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsInput {
    /// The level a module logs at unless it is overridden.
    pub log_level_default: String,
    /// Per-module raises.
    #[serde(default)]
    pub log_level_overrides: serde_json::Value,
    /// How many days to keep.
    pub logs_retention_days: i64,
}

/// `GET /api/v1/observability/logs/settings`.
pub async fn read_settings(
    State(state): State<AppState>,
    // The guard has already resolved the caller; the handler only reads the single row, which is
    // installation-wide rather than tenant-scoped. The parameter is kept because dropping it would
    // make the route's authorization invisible at the call site.
    _session: CurrentSession,
) -> Result<Json<SettingsView>, ApiError> {
    let settings = store::load_settings(state.db().pool())
        .await
        .map_err(map_error)?;
    Ok(Json(SettingsView::from(settings)))
}

/// `PUT /api/v1/observability/logs/settings` — save the one settings row.
///
/// This is the one write in the file, and it is audited like any other: a change to what the
/// platform records, and to how long it keeps it, is exactly the kind of change an operator has
/// to be able to answer "who turned the retention down to a day and when".
pub async fn save_settings(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(input): Json<SettingsInput>,
) -> Result<Json<SettingsView>, ApiError> {
    // The level is validated here rather than by the column's check constraint, so the caller
    // gets a field-level message naming the accepted set instead of a constraint violation.
    let level = LogLevel::parse(&input.log_level_default).ok_or_else(|| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_log_level",
            format!(
                "`log_level_default` must be one of trace, debug, info, warn, error — got `{}`",
                input.log_level_default
            ),
        )
    })?;

    if !(1..=store::MAX_WINDOW_DAYS).contains(&input.logs_retention_days) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_retention",
            format!(
                "`logs_retention_days` must be between 1 and {} — the log store cannot answer a \
                 search older than it keeps",
                store::MAX_WINDOW_DAYS
            ),
        ));
    }

    let saved = store::save_settings(
        state.db().pool(),
        &LogSettings {
            log_level_default: level.as_str().to_owned(),
            log_level_overrides: input.log_level_overrides,
            logs_retention_days: input.logs_retention_days,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        },
        Some(session.user.id),
    )
    .await
    .map_err(map_error)?;

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(session.user.id, "observability.settings.updated")
            .organization(session.user.organization_id)
            .target("settings", "obs_log_settings")
            .metadata(serde_json::json!({
                "log_level_default": saved.log_level_default,
                "logs_retention_days": saved.logs_retention_days,
            })),
    )
    .await
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "audit_write_failed",
            error.to_string(),
        )
    })?;

    Ok(Json(SettingsView::from(saved)))
}

fn parse_levels(values: &[String]) -> Result<Vec<LogLevel>, ApiError> {
    values
        .iter()
        .map(|value| {
            LogLevel::parse(value).ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_level",
                    format!(
                        "`level` must be one of trace, debug, info, warn, error — got `{value}`"
                    ),
                )
            })
        })
        .collect()
}

fn parse_instant(text: Option<&str>, field: &str) -> Result<Option<OffsetDateTime>, ApiError> {
    match text {
        None => Ok(None),
        Some(text) => OffsetDateTime::parse(text, &Rfc3339).map(Some).map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_instant",
                format!("`{field}` must be an RFC 3339 instant, e.g. 2026-09-28T09:00:00Z"),
            )
        }),
    }
}

fn map_error(error: TelemetryError) -> ApiError {
    match error {
        TelemetryError::WindowTooWide { .. } => ApiError::new(
            StatusCode::BAD_REQUEST,
            "window_too_wide",
            error.to_string(),
        ),
        TelemetryError::Telemetry(message) => {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "log_store_failed", message)
        }
    }
}

/// Prove the mapping is what the acceptance criteria claim: a window beyond the cap is a `400`
/// naming the cap, and a store failure is a `500`. The status is the contract with the caller.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_level_outside_the_closed_set_is_refused_with_the_accepted_list() {
        let error = parse_levels(&["verbose".to_owned()]).expect_err("an unknown level must fail");
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert!(
            error.message().contains("trace, debug, info, warn, error"),
            "the refusal must name what is accepted: {}",
            error.message()
        );
    }

    #[test]
    fn the_level_filter_accepts_the_documented_names() {
        let levels = parse_levels(&[
            "info".to_owned(),
            "WARN".to_owned(),
            " error ".to_owned(),
        ])
        .expect("the documented names must parse");
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[1], LogLevel::Warn);
    }

    #[test]
    fn a_window_beyond_the_cap_is_a_four_hundred_naming_the_cap() {
        let since = (OffsetDateTime::now_utc() - time::Duration::days(400))
            .format(&Rfc3339)
            .unwrap();
        store::validate_window(
            parse_instant(Some(&since), "since").unwrap(),
            None,
        )
        .expect_err("a 400-day window must be refused");
        let error = map_error(
            store::validate_window(
                parse_instant(Some(&since), "since").unwrap(),
                None,
            )
            .expect_err("a 400-day window must be refused"),
        );
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert!(
            error.message().contains(&store::MAX_WINDOW_DAYS.to_string()),
            "the refusal must name the cap: {}",
            error.message()
        );
    }

    #[test]
    fn a_store_failure_is_a_five_hundred_not_a_four_hundred() {
        // A `400` invites a pointless retry; a store that cannot be read is not the caller's
        // fault and must not be reported as one.
        let error = map_error(TelemetryError::Telemetry("connection refused".to_owned()));
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
