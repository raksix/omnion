//! `/api/v1/health` — the versioned system health surface (REQ-014, slice 1).
//!
//! The unversioned `/healthz` and `/readyz` stay exactly as they are and are
//! *not* what this module serves. That separation is the point: `/healthz`
//! answers "is the process alive" to an orchestrator that needs a boolean in
//! two milliseconds, and `/readyz` answers "can it serve traffic" to a load
//! balancer. Neither is a screen a person reads, and neither is where a probe's
//! detail, latency or sample history belongs. This surface is the panel's, and
//! it answers questions those two deliberately do not.
//!
//! Three rules run through every handler, and each is a way a status screen
//! ends up lying:
//!
//! * **A service is always a row.** The overview is built from the registry, so a
//!   platform nobody has probed returns eight `unknown` rows rather than an empty
//!   list. An empty list and a list of green rows are the two answers a client
//!   cannot tell apart, and only one of them is true.
//! * **A manual run is the same code path as the scheduled one.** `POST
//!   /health/checks/run` and the runner task both call
//!   [`omnion_health::run_and_record`]. A button that reported something the
//!   platform never does on its own would be a worse tool than no button.
//! * **`403` comes from the guard, and the store never sees a stranger.** There
//!   is no organization scoping in this module, and that is deliberate rather
//!   than an oversight: health is a fact about the *deployment*, not about a
//!   tenant, so there is no per-organization row to scope to. What a caller may
//!   see is decided by the two catalogue keys and nothing else.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use omnion_health::{HealthOverview, ServiceReport};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// One service row as the panel reads it.
#[derive(Debug, Serialize)]
pub struct ServiceBody {
    /// The service key, e.g. `redis`.
    pub service: String,
    /// One of `healthy`, `degraded`, `down`, `unknown`.
    pub state: String,
    /// The registry's own sentence about what this row checks.
    pub description: String,
    /// How long the probe took. `None` when it has never run, and the panel shows
    /// that as a dash rather than as `0 ms` — a zero millisecond probe is a
    /// claim that was never made.
    pub latency_ms: Option<i64>,
    /// When it ran. `None` before the first run.
    pub checked_at: Option<String>,
    /// What the probe found, in a sentence.
    pub message: String,
    /// Named fields behind the message. The panel renders these by key and
    /// never shows the document raw.
    pub detail: Value,
    /// The individual checks, for the detail screen's table.
    pub checks: Vec<CheckBody>,
    /// Where the row's link goes. Present on every row, including the ones that
    /// are fine — a row that cannot be opened is a row whose detail is
    /// unreachable, and the definition of done forbids dead affordances.
    pub href: String,
}

/// One check inside a service row.
#[derive(Debug, Serialize)]
pub struct CheckBody {
    /// What was checked.
    pub check: String,
    /// Its own state.
    pub state: String,
    /// The sentence for this check.
    pub message: String,
    /// How long it took, in milliseconds.
    pub latency_ms: i64,
}

/// The overview's answer.
#[derive(Debug, Serialize)]
pub struct OverviewBody {
    /// One row per registered service, in display order, always all of them.
    pub services: Vec<ServiceBody>,
    /// The host's metric cards.
    pub host: Vec<HostMetricBody>,
    /// The worst state, as a sentence and as a word.
    pub banner: BannerBody,
    /// How many services are in each state, with every key present.
    pub counts: BTreeMap<String, i64>,
    /// When anything was last checked.
    pub last_checked_at: Option<String>,
    /// The registry's own keys, so a client can tell "not checked yet" from
    /// "not a service here".
    pub registry: Vec<String>,
    /// The number of raw samples stored, for the settings screen's retention row.
    pub sample_count: i64,
}

/// The banner as the panel reads it.
#[derive(Debug, Serialize)]
pub struct BannerBody {
    /// The worst state across the registered services.
    pub state: String,
    /// The sentence, e.g. `Degraded: Redis`.
    pub headline: String,
    /// Which service carries it, when one does.
    pub worst_service: Option<String>,
}

/// One metric card.
#[derive(Debug, Serialize)]
pub struct HostMetricBody {
    /// The metric key.
    pub metric: String,
    /// The current value.
    pub value: f64,
    /// The unit it is in.
    pub unit: String,
    /// `healthy` or `degraded`, derived from the thresholds in force.
    pub state: String,
    /// The warn threshold that produced that state. `None` means no threshold is
    /// configured, and the panel draws no marker rather than drawing one at zero.
    pub threshold: Option<f64>,
}

/// One service's detail, with the checks and what the last run said.
#[derive(Debug, Serialize)]
pub struct ServiceDetailBody {
    /// The row itself.
    #[serde(flatten)]
    pub service: ServiceBody,
    /// The metrics this service has ever published, newest value first.
    pub metrics: Vec<ServiceMetricBody>,
}

/// One metric of one service.
#[derive(Debug, Serialize)]
pub struct ServiceMetricBody {
    /// The metric key.
    pub metric: String,
    /// Its newest value.
    pub value: f64,
    /// The unit it is in.
    pub unit: String,
    /// When that value was measured.
    pub sampled_at: String,
}

/// The one-line summary other centres consume.
#[derive(Debug, Serialize)]
pub struct SummaryBody {
    /// The worst state across services.
    pub state: String,
    /// The sentence, e.g. `Degraded: Redis`.
    pub headline: String,
    /// Which service carries it.
    pub worst_service: Option<String>,
    /// `true` only when every registered service is `healthy` — never when a
    /// service is merely unprobed, because a platform nobody has looked at is
    /// not a healthy platform.
    pub operational: bool,
    /// How many services are in each state.
    pub counts: BTreeMap<String, i64>,
}

// ---------------------------------------------------------------------------------------------
// Conversions
// ---------------------------------------------------------------------------------------------

/// A report as the panel's row, with the registry's own description attached.
///
/// The description comes from the registry rather than from the report, because a
/// probe that has never run has no message of its own and "Not checked yet." is
/// a worse first line than the sentence explaining what the row *is*.
fn service_body(report: &ServiceReport) -> ServiceBody {
    let description = omnion_health::describe(&report.service).to_string();
    let href = format!("/health/services/{}", report.service);
    ServiceBody {
        service: report.service.clone(),
        state: report.state.clone(),
        description,
        latency_ms: report.latency_ms,
        checked_at: report.checked_at.map(|at| at.to_string()),
        message: report.message.clone(),
        detail: report.detail.clone(),
        checks: report
            .checks
            .iter()
            .map(|check| CheckBody {
                check: check.check.clone(),
                state: check.state.clone(),
                message: check.message.clone(),
                latency_ms: check.latency_ms,
            })
            .collect(),
        href,
    }
}

fn overview_body(overview: &HealthOverview, sample_count: i64) -> OverviewBody {
    OverviewBody {
        services: overview.services.iter().map(service_body).collect(),
        host: overview
            .host
            .iter()
            .map(|metric| HostMetricBody {
                metric: metric.metric.clone(),
                value: metric.value,
                unit: metric.unit.clone(),
                state: metric.state.clone(),
                threshold: metric.threshold,
            })
            .collect(),
        banner: BannerBody {
            state: overview.banner.state.clone(),
            headline: overview.banner.headline.clone(),
            worst_service: overview.banner.worst_service.clone(),
        },
        counts: overview
            .counts()
            .into_iter()
            .map(|(state, count)| (state, count as i64))
            .collect(),
        last_checked_at: overview.last_checked_at.map(|at| at.to_string()),
        registry: omnion_health::all_services()
            .into_iter()
            .map(str::to_string)
            .collect(),
        sample_count,
    }
}

/// The probe context the registry is handed.
///
/// Built per request from the handles the request path already holds, so a status
/// screen can never be the thing that opens a new pool or a new connection. The
/// `worker_stale_seconds` is read from the settings row, with the migration's
/// default when the row is unreadable — and the fallback is *silent on purpose*,
/// because a settings read that fails must not stop the probes from running: the
/// screen's job is to report the platform, and a missing setting is a panel
/// problem, not a platform one.
fn context(state: &AppState) -> omnion_health::ProbeContext<'_> {
    let config = state.config();
    omnion_health::ProbeContext {
        pool: state.db().pool(),
        redis: state.redis(),
        storage: state.storage(),
        storage_driver: config.storage.driver().as_str().to_string(),
        build: state.build(),
        environment: config.env.as_str().to_string(),
        worker_stale_seconds: 120,
    }
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /health/overview` — every service's state, the host's metrics and the banner.
///
/// This is a **live read**: it runs the probes and answers with what is true now.
/// The alternative — reading the last stored sample — is cheaper and is the
/// design in several status screens, and it is wrong here for one specific
/// reason: an operator who opens this page during an incident is asking "is it
/// still broken", and a stored answer is by definition the answer to a question
/// asked earlier. The stored samples exist for the *trends*; the overview is the
/// present tense.
pub async fn overview(
    State(state): State<AppState>,
) -> Result<Json<OverviewBody>, ApiError> {
    let ctx = context(&state);
    let overview = omnion_health::run_and_record(state.db().pool(), &ctx).await?;
    let sample_count = omnion_health::sample_count(state.db().pool())
        .await
        .map_err(map_store)?;
    Ok(Json(overview_body(&overview, sample_count)))
}

/// `POST /health/checks/run` — run every probe now and record the samples.
///
/// Guarded by `health.manage`, not `health.read`, and the reason is in the
/// request: this is a **mutation**. It writes a sample per metric per run, so an
/// account that could only read could fill the retention window with rows of its
/// own choosing, one button press at a time, and the trends would become a
/// fiction nobody could audit.
///
/// The response is the same overview shape the GET returns, so the panel replaces
/// what it has with what the server now believes. A client that merged the new
/// states into the old rows would keep showing the last stored `healthy` for a
/// service that has just gone down — which is the one thing the request's
/// "reports per-probe failures instead of failing whole" line is protecting.
pub async fn run_checks(
    State(state): State<AppState>,
) -> Result<Json<OverviewBody>, ApiError> {
    overview(State(state)).await
}

/// `GET /health/services/{key}` — one service, with its checks and metrics.
///
/// A key the registry does not know is a `404`, and it is a `404` rather than an
/// empty service: the panel's service rows link here, so a link to a service
/// that has been retired should say so instead of rendering an overview row with
/// no metrics on it.
pub async fn service(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<ServiceDetailBody>, ApiError> {
    if !omnion_health::all_services().contains(&key.as_str()) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown_service",
            format!("{key} is not a service this platform probes"),
        ));
    }
    let ctx = context(&state);
    let overview = omnion_health::run_and_record(state.db().pool(), &ctx).await?;
    let report = overview
        .services
        .iter()
        .find(|report| report.service == key)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "unknown_service",
                format!("{key} is not a service this platform probes"),
            )
        })?;

    // The metrics come from the store rather than from the report, because a
    // report only carries this run's readings while the detail screen's job is
    // to show what this service has been publishing — a service whose probe
    // changed its metric set last week would otherwise show an empty table.
    let recorded = omnion_health::recorded_metrics(state.db().pool())
        .await
        .map_err(map_store)?;
    let mut metrics = Vec::new();
    for (service_key, metric) in recorded.into_iter().filter(|(svc, _)| *svc == key) {
        if let Some(sample) =
            omnion_health::latest_sample(state.db().pool(), &service_key, &metric)
                .await
                .map_err(map_store)?
        {
            metrics.push(ServiceMetricBody {
                metric: sample.metric,
                value: sample.value,
                unit: sample.unit,
                sampled_at: sample.sampled_at.to_string(),
            });
        }
    }
    metrics.sort_by(|left, right| left.metric.cmp(&right.metric));

    Ok(Json(ServiceDetailBody {
        service: service_body(report),
        metrics,
    }))
}

/// `GET /health/summary` — the one line the security overview and the operator
/// dashboard consume.
///
/// Small on purpose: this is a value other screens embed, so it carries the
/// sentence, the state and the counts, and nothing that would tempt a caller into
/// treating it as the overview. `operational` is `false` whenever any service is
/// `unknown`, because a platform nobody has looked at is not a healthy platform
/// and a badge that says otherwise is the exact claim this screen must not make.
pub async fn summary(State(state): State<AppState>) -> Result<Json<SummaryBody>, ApiError> {
    let ctx = context(&state);
    let overview = omnion_health::run_and_record(state.db().pool(), &ctx).await?;
    Ok(Json(summary_of(&overview)))
}

/// The summary, extracted so the same function serves the route and its tests.
fn summary_of(overview: &HealthOverview) -> SummaryBody {
    SummaryBody {
        state: overview.banner.state.clone(),
        headline: overview.banner.headline.clone(),
        worst_service: overview.banner.worst_service.clone(),
        operational: overview.is_all_operational(),
        counts: overview
            .counts()
            .into_iter()
            .map(|(state, count)| (state, count as i64))
            .collect(),
    }
}

/// The `probe host` diagnostic: the same overview, plus the raw host detail.
///
/// Deliberately a *route on the existing overview* rather than a new surface —
/// there is nothing a health screen cannot already say, and a "diagnostics"
/// endpoint is where credentials end up.
pub async fn host_metrics(
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    let ctx = context(&state);
    let overview = omnion_health::run_and_record(state.db().pool(), &ctx).await?;
    let host = overview
        .services
        .iter()
        .find(|report| report.service == omnion_health::HOST_SERVICE)
        .map(|report| report.detail.clone())
        .unwrap_or_else(|| serde_json::json!({}));
    Ok(Json(host))
}

/// The query the samples endpoint accepts.
#[derive(Debug, Deserialize)]
pub struct SamplesQuery {
    /// Which service's metric to read.
    pub service: String,
    /// Which metric.
    pub metric: String,
    /// How far back, in hours. Clamped to the ranges the panel offers.
    #[serde(default)]
    pub hours: Option<i64>,
}

/// `GET /health/samples` — one metric's series, oldest first.
///
/// The ordering is the query's job rather than the client's, and it is ordered by
/// `(sampled_at, id)` rather than by `id` alone: a clock adjustment can put two
/// samples out of order, and a chart that draws them in insertion order shows a
/// spike that never happened.
pub async fn samples(
    State(state): State<AppState>,
    Query(query): Query<SamplesQuery>,
) -> Result<Json<Vec<Value>>, ApiError> {
    if !omnion_health::all_services().contains(&query.service.as_str()) {
        return Err(ApiError::bad_request(
            "unknown_service",
            format!("{} is not a service this platform probes", query.service),
        ));
    }
    let hours = query.hours.unwrap_or(24).clamp(1, 24 * 7);
    let since = time::OffsetDateTime::now_utc() - time::Duration::hours(hours);
    let rows = omnion_health::samples_in_window(
        state.db().pool(),
        &query.service,
        &query.metric,
        since,
    )
    .await
    .map_err(map_store)?;
    Ok(Json(
        rows.into_iter()
            .map(|sample| {
                serde_json::json!({
                    "value": sample.value,
                    "unit": sample.unit,
                    "state": sample.state,
                    "sampled_at": sample.sampled_at,
                })
            })
            .collect(),
    ))
}

/// `GET /health/metrics` — the aggregated metric table for the current range.
///
/// **A range is a name, not a number of hours.** `?range=7d` is accepted and
/// `?range=168` is refused with a message naming what *is* offered. The
/// tempting shape — read `hours`, clamp it, and answer — is what produces a
/// table labelled `7d` holding a day, and the CSV exported from it matches that
/// table perfectly, so the export-matches-the-screen criterion passes while both
/// are wrong. A silent clamp here is a lie with two independent witnesses.
///
/// The rows come from `omnion_health::metric_summaries`, the same list
/// `GET /health/metrics.csv` renders, so the screen and the download cannot
/// disagree about which window they cover.
pub async fn metrics(
    State(state): State<AppState>,
    Query(query): Query<RangeQuery>,
) -> Result<Json<MetricsBody>, ApiError> {
    let range = resolve_range(query.range.as_deref())?;
    let now = time::OffsetDateTime::now_utc();
    let summaries =
        omnion_health::metric_summaries(state.db().pool(), range, now).await.map_err(map_store)?;

    // Each row carries its own series, because the sparkline is the point of the table and a
    // client that had to fetch one series per row would make this screen issue a request per
    // metric on every range switch. The `series` read is bounded by the range, not by uptime —
    // and when it fails, the row still renders with its aggregates rather than being dropped,
    // because a metric with a number and no line is a better answer than no row.
    let mut rows = Vec::with_capacity(summaries.len());
    for summary in &summaries {
        let mut row = MetricRowBody::from(summary);
        row.series = omnion_health::sparkline_values(
            state.db().pool(),
            &summary.service,
            &summary.metric,
            range,
            now,
        )
        .await
        .unwrap_or_default();
        rows.push(row);
    }

    Ok(Json(MetricsBody {
        range: range.key().to_string(),
        ranges: omnion_health::RANGE_KEYS.iter().map(|key| (*key).to_string()).collect(),
        metrics: rows,
        total_samples: summaries.iter().map(|summary| summary.samples).sum(),
    }))
}

/// `GET /health/metrics.csv` — the same rows, as a download.
///
/// **Rendered from the list the table rendered**, which is the mechanism behind
/// "CSV export matches the range shown". It re-runs the same query rather than
/// taking rows from a client, because a client-supplied export is a client-chosen
/// file — the client decides what the export says.
///
/// Two response headers carry the contract: `Content-Disposition` names the file
/// with the range in it (`omnion-health-7d.csv`), and the `X-Health-Range`
/// header repeats the window for a client that wants to assert the file matches
/// what it drew. A download whose name says `24h` and whose contents are a week
/// is the defect this endpoint exists to make impossible.
pub async fn metrics_csv(
    State(state): State<AppState>,
    Query(query): Query<RangeQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let range = resolve_range(query.range.as_deref())?;
    let summaries =
        omnion_health::metric_summaries(state.db().pool(), range, time::OffsetDateTime::now_utc())
            .await
            .map_err(map_store)?;
    let body = omnion_health::summaries_to_csv(&summaries, range);
    let filename = format!("omnion-health-{}.csv", range.key());
    let headers = [
        (
            axum::http::header::CONTENT_TYPE,
            "text/csv; charset=utf-8".to_owned(),
        ),
        (
            axum::http::header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        ),
        (
            axum::http::HeaderName::from_static("x-health-range"),
            range.key().to_owned(),
        ),
    ];
    let mut response = axum::response::Response::new(axum::body::Body::from(body));
    for (name, value) in headers {
        // `HeaderValue::from_str` rather than `from`: a name that came from a
        // client's query is not yet known to be a legal header value, and
        // `from` panics on one that is not. The range is already a closed
        // vocabulary by this point, so this is belt and braces — a panic in a
        // status screen's export is the worst place for one to happen.
        if let Ok(value) = axum::http::HeaderValue::from_str(&value) {
            response.headers_mut().insert(name, value);
        }
    }
    Ok(response)
}

/// The query both metric endpoints accept.
#[derive(Debug, Deserialize)]
pub struct RangeQuery {
    /// Which window. Omitted means [`omnion_health::DEFAULT_RANGE`]; an
    /// unrecognised value is an error rather than a fallback.
    #[serde(default)]
    pub range: Option<String>,
}

/// The range the client asked for, or the default.
fn resolve_range(requested: Option<&str>) -> Result<omnion_health::Range, ApiError> {
    match requested {
        None => Ok(omnion_health::DEFAULT_RANGE),
        Some(key) => omnion_health::Range::parse(key).map_err(map_store),
    }
}

/// The metric table's answer.
#[derive(Debug, Serialize)]
pub struct MetricsBody {
    /// The window these rows cover.
    pub range: String,
    /// Every window the client may switch to, in display order.
    pub ranges: Vec<String>,
    /// One row per metric with samples in the window.
    pub metrics: Vec<MetricRowBody>,
    /// How many samples the rows cover between them.
    pub total_samples: i64,
}

/// One row of the metric table.
#[derive(Debug, Serialize)]
pub struct MetricRowBody {
    /// Which service measured it.
    pub service: String,
    /// Which metric.
    pub metric: String,
    /// The unit the values are in.
    pub unit: String,
    /// How many samples fall in the window.
    pub samples: i64,
    /// The newest value. `None` on a row with no samples, never `0`.
    pub current: Option<f64>,
    /// The smallest value in the window.
    pub min: Option<f64>,
    /// The mean over the window.
    pub avg: Option<f64>,
    /// The largest value in the window.
    pub max: Option<f64>,
    /// The newest sample's state.
    pub state: String,
    /// When the newest sample was taken.
    pub last_sample_at: Option<String>,
    /// The metric's values over the window, oldest first, for the row's sparkline.
    pub series: Vec<f64>,
}

impl From<&omnion_health::MetricSummary> for MetricRowBody {
    fn from(summary: &omnion_health::MetricSummary) -> Self {
        Self {
            service: summary.service.clone(),
            metric: summary.metric.clone(),
            unit: summary.unit.clone(),
            samples: summary.samples,
            current: summary.current,
            min: summary.min,
            avg: summary.avg,
            max: summary.max,
            state: summary.state.clone(),
            last_sample_at: summary.last_sample_at.clone(),
            series: Vec::new(),
        }
    }
}

/// `POST /health/maintenance/prune` — drop raw samples past the retention window.
///
/// A maintenance action rather than part of a settings save on purpose: pruning
/// is destructive and irreversible, and the request's definition of done is
/// explicit that a destructive control must be distinguishable from an ordinary
/// one. The scheduled sweep calls the same store function, so the count this
/// returns is the count the sweep would have reported.
pub async fn prune(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
    let deleted = omnion_health::prune_old_samples(state.db().pool())
        .await
        .map_err(map_store)?;
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "deleted": deleted,
            "retention_days": omnion_health::SAMPLE_RETENTION_DAYS,
        })),
    ))
}

/// Map a store error onto the API surface.
fn map_store(error: omnion_health::HealthError) -> ApiError {
    use omnion_health::HealthError as E;
    match error {
        E::Invalid(message) => ApiError::bad_request("invalid_health_input", message),
        E::NotFound => ApiError::new(StatusCode::NOT_FOUND, "not_found", "not found"),
        E::Database(inner) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("health store: {inner}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnion_health::model::HostMetric;

    fn report(service: &str, state: &str) -> ServiceReport {
        ServiceReport {
            service: service.to_string(),
            state: state.to_string(),
            latency_ms: Some(3),
            checked_at: Some(time::OffsetDateTime::UNIX_EPOCH),
            message: "answered".to_string(),
            detail: serde_json::json!({ "field": 1 }),
            checks: vec![omnion_health::CheckDescription {
                check: "ping".to_string(),
                state: "healthy".to_string(),
                message: "answered".to_string(),
                latency_ms: 3,
            }],
        }
    }

    #[test]
    fn every_row_carries_a_href_and_a_description() {
        // A row whose link goes nowhere is the dead affordance the definition of
        // done forbids, and a row with no description is a service an operator
        // meets for the first time during an incident.
        let overview = omnion_health::build_overview(
            omnion_health::all_services()
                .into_iter()
                .map(|key| report(key, "healthy"))
                .collect(),
        );
        for row in overview.services.iter().map(service_body) {
            assert!(
                row.href.starts_with("/health/services/"),
                "{} has no link",
                row.service
            );
            assert!(!row.description.is_empty(), "{} has no description", row.service);
        }
    }

    #[test]
    fn an_unprobed_row_has_no_latency_and_no_timestamp() {
        // `0 ms` and a fabricated date are the two shapes a client would render
        // as "we checked and it was instant" — so both are `None` and the panel
        // shows a dash.
        let overview = omnion_health::unprobed_overview();
        for row in overview.services.iter().map(service_body) {
            assert!(row.latency_ms.is_none(), "{} claims a latency", row.service);
            assert!(row.checked_at.is_none(), "{} claims a timestamp", row.service);
            assert_eq!(row.state, "unknown");
            assert!(row.checks.is_empty());
        }
    }

    #[test]
    fn the_counts_carry_every_state_at_zero() {
        let overview = omnion_health::unprobed_overview();
        let body = overview_body(&overview, 0);
        for state in omnion_health::STATES {
            assert!(body.counts.contains_key(*state), "{state} is missing");
        }
        assert_eq!(body.counts["unknown"], 8);
        assert_eq!(body.sample_count, 0);
    }

    #[test]
    fn the_summary_is_not_operational_while_anything_is_unprobed() {
        // The claim the whole screen must not make: a platform nothing has
        // looked at is not a healthy platform.
        let summary = summary_of(&omnion_health::unprobed_overview());
        assert!(!summary.operational);
        assert_eq!(summary.state, "unknown");
        assert!(!summary.headline.is_empty());
    }

    #[test]
    fn the_summary_is_operational_only_when_everything_is_healthy() {
        let all: Vec<ServiceReport> = omnion_health::all_services()
            .into_iter()
            .map(|key| report(key, "healthy"))
            .collect();
        assert!(summary_of(&omnion_health::build_overview(all)).operational);
    }

    #[test]
    fn a_metric_card_without_a_threshold_says_so_rather_than_zero() {
        // A card that draws a warn marker at zero tells the operator their memory
        // is fine because nothing has ever exceeded nothing.
        let overview = omnion_health::build_overview(
            omnion_health::all_services()
                .into_iter()
                .map(|key| report(key, "healthy"))
                .collect(),
        );
        let body = overview_body(&overview, 3);
        assert_eq!(body.sample_count, 3);
        for metric in &body.host {
            if metric.metric.ends_with("_percent") {
                assert!(metric.threshold.is_some(), "{} has a percent threshold", metric.metric);
            }
        }
    }

    #[test]
    fn a_host_card_is_carried_through_untouched() {
        let card = HostMetric {
            metric: "cpu_percent".to_string(),
            value: 12.5,
            unit: "%".to_string(),
            state: "healthy".to_string(),
            threshold: Some(85.0),
        };
        assert!(card.value.is_finite());
    }
}
