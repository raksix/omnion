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
use axum::response::IntoResponse;
use omnion_audit::NewAuditEntry;
use omnion_telemetry::metric_catalog::{self, FamilyDeclaration};
use omnion_telemetry::metrics::{self, LabelCatalogue, MAX_POINTS};
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

/* ── the metric registry and its catalogue (REQ-126, slice 2) ────────────────────────────────────
 *
 * Three endpoints: the exposition a Prometheus scrapes, the catalogue the panel reads, and the
 * bounded query a chart is drawn from. The selector rule is the part worth stating once: a
 * selector may only name a declared family and label values the registry has actually seen, and
 * anything else is a `400` naming the family — because a chart that renders an empty graph for an
 * unknown selector is indistinguishable from a chart of a metric that is genuinely idle, and an
 * operator cannot act on the first.
 */

/// The exposition's content type, verbatim.
///
/// Prometheus's text format has a canonical content type and a scraper that receives
/// `text/plain` instead of it will, in most configurations, refuse the body rather than guess.
const EXPOSITION_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// `GET /metrics` — the Prometheus exposition.
///
/// Unauthenticated, and that is a deliberate decision the request states rather than an oversight:
/// a scraper on the same network has no session, and the alternative (a token) is the operator's
/// to configure for a public interface. What is *not* exposed by accident is anything the registry
/// does not declare — the exposition is a rendering of the declared families and nothing else, so
/// there is no path by which a future field becomes public without appearing in `FAMILIES` and
/// being reviewed as a family.
pub async fn metrics_exposition() -> impl IntoResponse {
    let registry = metrics::global();
    let text = registry.render();
    // The response is `text/plain` and nothing else: a JSON error body on this path would be read
    // by a scraper as a malformed exposition, so a failure here is logged rather than shaped.
    tracing::debug!(bytes = text.len(), "metrics exposition rendered");
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                EXPOSITION_CONTENT_TYPE,
            ),
            // A scrape must never be answered from a cache: a stale `/metrics` is a monitoring
            // system reporting numbers that stopped happening.
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        text,
    )
}

/// One catalogue row, in the panel's shape.
#[derive(Debug, Serialize)]
pub struct CatalogEntryView {
    /// The exposition name.
    pub name: String,
    /// `counter`, `gauge` or `histogram`.
    pub kind: String,
    /// The unit shown next to the chart.
    pub unit: String,
    /// The one-sentence description.
    pub description: String,
    /// The label names, positionally.
    pub labels: Vec<String>,
    /// `core`, `module` or `worker`.
    pub source: String,
    /// The series the registry holds right now.
    pub cardinality_estimate: i32,
    /// The cap this family is held to.
    pub cardinality_budget: i32,
    /// Whether the cap is enforced for this family.
    pub budgeted: bool,
    /// When the family last recorded a sample, if it ever has.
    pub last_seen_at: Option<String>,
    /// `true` when this build emits it and has samples.
    pub live: bool,
    /// `true` when the family is over its cap and is folding samples into `other`.
    pub over_budget: bool,
}

impl From<metric_catalog::CatalogRow> for CatalogEntryView {
    fn from(row: metric_catalog::CatalogRow) -> Self {
        // Computed before the move: a family this build does not declare keeps its row, so
        // "documented" and "live" are two different columns rather than one value the panel has
        // to interpret.
        let live = metrics::family(&row.name).is_some();
        Self {
            name: row.name,
            kind: row.kind,
            unit: row.unit,
            description: row.description,
            labels: row.labels,
            source: row.source,
            cardinality_estimate: row.cardinality_estimate,
            cardinality_budget: row.cardinality_budget,
            budgeted: row.budgeted,
            last_seen_at: row
                .last_seen_at
                .map(|at| at.format(&Rfc3339).unwrap_or_default()),
            live,
            over_budget: false,
        }
    }
}

/// The catalogue response.
#[derive(Debug, Serialize)]
pub struct CatalogResponse {
    /// The families, grouped by source then name.
    pub families: Vec<CatalogEntryView>,
    /// The label positions and the values observed, so the selector builder is fed from data.
    pub label_catalogues: Vec<LabelCatalogue>,
    /// The families currently folding samples into their overflow series.
    pub over_budget: Vec<String>,
    /// The registry's global series cap.
    pub global_budget: usize,
    /// The cap on the points one chart may return.
    pub max_points: usize,
}

/// `GET /api/v1/observability/metrics/catalog` — the documented families.
pub async fn read_catalog(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<Json<CatalogResponse>, ApiError> {
    let rows = metric_catalog::list_with_state(state.db().pool())
        .await
        .map_err(map_error)?;
    let over = metric_catalog::over_budget_families(state.db().pool())
        .await
        .map_err(map_error)?;
    let families: Vec<CatalogEntryView> = rows
        .into_iter()
        .map(|row| {
            let mut view = CatalogEntryView::from(row);
            view.over_budget = over.iter().any(|name| *name == view.name);
            view
        })
        .collect();
    Ok(Json(CatalogResponse {
        families,
        label_catalogues: metrics::label_catalogues(),
        over_budget: over,
        global_budget: metrics::global().global_budget(),
        max_points: MAX_POINTS,
    }))
}

/// The chart query's query string.
#[derive(Debug, Default, serde::Deserialize)]
pub struct MetricQuery {
    /// The family name.
    pub metric: Option<String>,
    /// One window in minutes, capped at `MAX_POINTS`.
    pub window_minutes: Option<i64>,
}

/// One chart's worth of data.
#[derive(Debug, Serialize)]
pub struct MetricQueryResponse {
    /// The family requested.
    pub metric: String,
    /// Its kind, so the panel renders a rate and a total differently.
    pub kind: String,
    /// Its unit.
    pub unit: String,
    /// The label names, positionally — the x-axis of a multi-series chart.
    pub labels: Vec<String>,
    /// The window the points cover, in minutes.
    pub window_minutes: usize,
    /// The cap on points, so a caller can see it was clamped.
    pub max_points: usize,
    /// The series, each with its labels and its minute buckets.
    pub series: Vec<metrics::SeriesSnapshot>,
    /// The PromQL for the same selection, ready to paste into a dashboard.
    pub promql: String,
    /// `true` when the family is declared but has never recorded a sample in this process.
    pub no_samples: bool,
}

/// `GET /api/v1/observability/metrics/query` — a bounded chart for one catalogue selector.
///
/// The window is minutes, and it is refused above [`MAX_POINTS`]. A 30-day window on a
/// one-minute-resolution ring is not a chart with fewer points, it is a chart that silently
/// resampled — so the caller is told the cap and the cap is also returned in the body.
pub async fn read_metric_query(
    // The state is part of the handler's shape and unused by the body: the registry is
    // process-wide, which is the whole reason a chart does not need a database round trip.
    _state: State<AppState>,
    _session: CurrentSession,
    Query(query): Query<MetricQuery>,
) -> Result<Json<MetricQueryResponse>, ApiError> {
    let name = query.metric.unwrap_or_default();
    let spec = metrics::family(&name).ok_or_else(|| {
        // A refusal that names the family, not a blank graph: an unknown selector and an idle
        // metric look identical on screen, and only one of them is the operator's to fix.
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "unknown_metric",
            format!("`metric` must be a declared family; `{name}` is not one of them"),
        )
    })?;

    let window = query.window_minutes.unwrap_or(60);
    if window < 1 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_window",
            "`window_minutes` must be at least 1",
        ));
    }
    let window = window.min(MAX_POINTS as i64) as usize;

    let series = metrics::global().series_of(spec.name, window);
    let labels: Vec<String> = spec.labels.iter().map(|label| (*label).to_owned()).collect();
    // The PromQL is built from the first *real* series, never from the overflow bucket. The
    // overflow series is sorted into the list like any other, and an operator who clicks "copy as
    // PromQL" and pastes `omnion_circuit_state{provider="other"}` into a dashboard has been handed
    // the aggregate of everything the cap folded — which is a number they did not ask for and
    // cannot tell apart from a series the operator configured themselves.
    let representative = series
        .iter()
        .find(|snapshot| !snapshot.labels.iter().all(|value| value == "other"))
        .or_else(|| series.first());
    let promql = promql_for(spec, &representative.map(|s| s.labels.clone()).unwrap_or_default());

    Ok(Json(MetricQueryResponse {
        metric: spec.name.to_owned(),
        kind: spec.kind.as_str().to_owned(),
        unit: spec.unit.to_owned(),
        labels,
        window_minutes: window,
        max_points: MAX_POINTS,
        no_samples: series.is_empty(),
        series,
        promql,
    }))
}

/// The PromQL for a selection, so the panel's "copy as PromQL" affordance copies something real.
///
/// A histogram is rendered as a `rate(...)` over the matching series, the others as a raw
/// selector: a gauge's `rate()` is meaningless and an operator who pasted one would spend an
/// afternoon wondering why it was flat.
fn promql_for(spec: &metrics::FamilySpec, values: &[String]) -> String {
    let matcher = if values.is_empty() {
        String::new()
    } else {
        let parts: Vec<String> = spec
            .labels
            .iter()
            .zip(values.iter())
            .map(|(name, value)| format!("{name}=\"{}\"", value.replace('"', "\\\"")))
            .collect();
        format!("{{{}}}", parts.join(","))
    };
    match spec.kind {
        metrics::MetricKind::Histogram => format!("rate({}_sum{matcher}[5m])", spec.name),
        metrics::MetricKind::Counter => {
            if spec.name.ends_with("_total") {
                format!("rate({}{matcher}[5m])", spec.name)
            } else {
                format!("increase({}{matcher}[5m])", spec.name)
            }
        }
        metrics::MetricKind::Gauge => format!("{}{matcher}", spec.name),
    }
}

/// `POST /api/v1/observability/metrics/sync` — re-seed the catalogue from the registry.
///
/// The boot already does this, so this endpoint exists for the one case boot cannot cover: a
/// module that is enabled at runtime and registers its families after the API started. It is a
/// write and it is audited, because "the catalogue now documents a family that did not exist five
/// minutes ago" is a change an operator wants a trail for.
pub async fn sync_catalog(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<CatalogResponse>, ApiError> {
    let declarations: Vec<FamilyDeclaration> = metrics::FAMILIES
        .iter()
        .map(|spec| {
            let series = metrics::global()
                .family_states()
                .into_iter()
                .find(|state| state.name == spec.name)
                .map_or(0, |state| state.series);
            FamilyDeclaration::from_spec(spec, series)
        })
        .collect();
    let written = metric_catalog::sync_from_registry(state.db().pool(), &declarations)
        .await
        .map_err(map_error)?;

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(session.user.id, "observability.metrics.catalog_synced")
            .organization(session.user.organization_id)
            .target("catalog", "obs_metric_catalog")
            .metadata(serde_json::json!({ "families_written": written })),
    )
    .await
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "audit_write_failed",
            error.to_string(),
        )
    })?;

    read_catalog(State(state), session).await
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
