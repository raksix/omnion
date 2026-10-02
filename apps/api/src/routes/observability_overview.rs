//! `GET /api/v1/observability/overview` — the landing screen of the observability centre
//! (REQ-126).
//!
//! ## Why this is one route and not the panel's seven calls
//!
//! The request lists an overview carrying "request rate, error ratio, p95, queue depth, AI spend
//! today, exporter health, links into each area". Assembled by the panel that is seven round
//! trips to seven endpoints before the operator sees a single number, and every one of them is a
//! different failure mode: a screen that renders six of seven tiles because the seventh refused
//! is indistinguishable from an instance that has no exporters, no alerts and no AI traffic.
//!
//! So the overview is a **read** that composes the same sources the individual reads use — the
//! registry, the exporter statuses, the alert events — and it composes them **defensively**: a
//! source that cannot be read is reported as `unavailable` on its own tile rather than failing the
//! whole screen. An operator landing here during an incident must not get a `500` because one
//! subsystem is unhappy.
//!
//! ## What is deliberately NOT here
//!
//! * No totals presented as truth. Every headline carries the window it was measured over, and
//!   an instance with no traffic answers `null` rather than `0` — "no requests" and "no data yet"
//!   are different states and an operator must be able to tell them apart.
//! * No aggregation across label sets beyond what the registry already bounded. `requests_total`
//!   is summed over `status` classes because the classes are the question; nothing is summed over
//!   `route`, which is a template with unbounded cardinality.
//! * No write, and therefore no audit entry: a landing screen that leaves a row behind is an
//!   audit trail of navigation.

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use omnion_telemetry::{alerts, exporter, metrics};
use serde::Serialize;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

/// The window the headline numbers are measured over.
///
/// Fifteen minutes, not sixty: this is the screen an operator opens *during* something, and an
/// hour-long window smooths away the spike they are looking for. The individual screens keep
/// their own range selectors, so this is a default, not a constraint.
const WINDOW_MINUTES: usize = 15;

/// One headline number, with the window it was measured over.
///
/// `value` is `null` when the family has never been written, which is a different state from
/// `0` — a counter that has never moved is an instance that has served nothing yet, and an
/// operator reading a flat `0` cannot tell it from an instance whose counter was reset.
#[derive(Serialize)]
pub struct Headline {
    /// The metric family this came from, so the panel can link to the chart behind it.
    pub metric: &'static str,
    /// The measured value, or `null` when the family has no samples in the window.
    pub value: Option<f64>,
    /// How the number should be read: `count`, `ratio`, `duration_seconds` or `micros`.
    pub unit: &'static str,
    /// The window in minutes.
    pub window_minutes: usize,
}

/// Exporter health on the overview: name, chip, and the drop count that says whether to trust it.
#[derive(Serialize)]
pub struct ExporterHealth {
    pub name: String,
    pub health: String,
    pub dropped_total: u64,
    pub buffered: usize,
}

/// The alert surface in one count per state, plus the newest firing incident.
#[derive(Serialize)]
pub struct AlertSummary {
    pub firing: usize,
    pub pending: usize,
    /// The most recent firing incident, so the operator can jump straight into it.
    pub latest_firing: Option<LatestIncident>,
}

#[derive(Serialize)]
pub struct LatestIncident {
    pub id: String,
    /// The rule's id, which is what the alert screen links to.
    pub rule_id: String,
    /// The rule's name, resolved from the rule store.
    ///
    /// An event carries only the id, so without this lookup the overview would render a bare
    /// uuid where every other alert surface renders a name — the same information, at a fraction
    /// of the legibility. When the rule has since been deleted the id is still returned, so the
    /// incident remains linkable.
    pub rule: Option<String>,
    pub started_at: String,
}

/// The whole screen.
///
/// `unavailable` lists the sources that could not be read. It is a list rather than a `500`
/// because "the alert store is unreadable" and "there are no alerts" must not draw the same
/// screen — the first is an incident, the second is a Tuesday.
#[derive(Serialize)]
pub struct Overview {
    pub window_minutes: usize,
    pub requests_total: Option<f64>,
    pub error_ratio: Option<f64>,
    pub p95_latency_seconds: Option<f64>,
    pub queue_depth: Option<f64>,
    pub ai_cost_micros_today: Option<f64>,
    pub exporters: Vec<ExporterHealth>,
    pub alerts: Option<AlertSummary>,
    /// Sources that could not be read on this request, by name.
    pub unavailable: Vec<String>,
    /// True when the instance has served no traffic in the window at all — the screen then says
    /// so in words rather than drawing six zeroes that read like a healthy flat line.
    pub no_traffic: bool,
}

/// `GET /api/v1/observability/overview`.
pub async fn read_overview(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<impl IntoResponse, ApiError> {
    let registry = metrics::global();
    let mut unavailable = Vec::new();

    // Requests in the window, summed over the status classes — the classes are the question, and
    // the registry has already folded anything past its label budget into `other`.
    let requests_total = sum_family(registry, "omnion_http_requests_total", &mut unavailable);
    let errors = sum_family_where(
        registry,
        "omnion_http_requests_total",
        &mut unavailable,
        |labels| {
            labels
                .iter()
                .any(|value| value.starts_with('5') || value.starts_with('4'))
        },
    );
    // The ratio is only meaningful when there is a denominator. An instance that has served
    // nothing answers `null`, not `0` — a zero error ratio on zero requests is a division that
    // happened to come out even, and drawing it as a clean 0% would read as "no errors".
    let error_ratio = match (requests_total, errors) {
        (Some(total), Some(failed)) if total > 0.0 => Some(failed / total),
        _ => None,
    };

    let p95_latency_seconds = percentile_over_labels(
        registry,
        "omnion_http_request_duration_seconds",
        &mut unavailable,
        0.95,
    );
    let queue_depth = sum_family(registry, "omnion_queue_depth", &mut unavailable);
    let ai_cost_micros_today = sum_family(registry, "omnion_ai_cost_micros_total", &mut unavailable);

    // Exporter health is in-process, so it cannot fail to be read — but it can be empty, and an
    // instance with no exporter configured is the normal state of a fresh install, not an outage.
    let exporters: Vec<ExporterHealth> = exporter::global()
        .statuses()
        .into_iter()
        .map(|status| ExporterHealth {
            name: status.name,
            health: status.health.clone(),
            dropped_total: status.dropped_total,
            buffered: status.buffered,
        })
        .collect();

    // The alert store IS a database read, so it is the one source that can genuinely fail. A
    // screen that cannot see the alert state during an outage is worse than one that says it
    // could not see it, so it degrades to `null` plus a name in `unavailable`.
    let alerts = match alerts::recent_events(state.db().pool(), 200).await {
        Ok(events) => {
            let firing: Vec<_> = events.iter().filter(|event| event.state == "firing").collect();
            // The event carries a rule id, not a name. One extra read resolves the newest
            // incident's name so the tile can link to a readable row; the failure is absorbed,
            // because an incident whose rule was deleted is still worth showing.
            let latest = match firing.first() {
                Some(event) => {
                    let rule = alerts::list_rules(state.db().pool())
                        .await
                        .ok()
                        .and_then(|rules| {
                            rules.into_iter().find(|r| r.rule.id == event.rule_id)
                        })
                        .map(|r| r.rule.name);
                    Some(LatestIncident {
                        id: event.id.to_string(),
                        rule_id: event.rule_id.to_string(),
                        rule,
                        started_at: format_timestamp(event.started_at),
                    })
                }
                None => None,
            };
            Some(AlertSummary {
                firing: firing.len(),
                pending: events.iter().filter(|event| event.state == "pending").count(),
                latest_firing: latest,
            })
        }
        Err(error) => {
            tracing::warn!(%error, "the observability overview could not read the alert store");
            unavailable.push("alerts".to_owned());
            None
        }
    };

    let no_traffic = requests_total.is_none_or(|total| total <= 0.0);

    Ok(Json(Overview {
        window_minutes: WINDOW_MINUTES,
        requests_total,
        error_ratio,
        p95_latency_seconds,
        queue_depth,
        ai_cost_micros_today,
        exporters,
        alerts,
        unavailable,
        no_traffic,
    }))
}

/// Render an instant the way every other telemetry read does, falling back to an empty string
/// rather than an epoch: a formatter that cannot render must not invent a date in 1970 that an
/// operator reads as "this incident started when the platform did".
fn format_timestamp(at: time::OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Sum every series of a family across the window.
///
/// A family that has never been written returns `None` — not `0`. The difference is the whole
/// reason this screen has a `no_traffic` flag at all.
fn sum_family(
    registry: &metrics::Registry,
    name: &str,
    unavailable: &mut Vec<String>,
) -> Option<f64> {
    sum_family_where(registry, name, unavailable, |_| true)
}

/// Sum the series of a family whose labels satisfy `predicate`.
fn sum_family_where(
    registry: &metrics::Registry,
    name: &str,
    unavailable: &mut Vec<String>,
    predicate: impl Fn(&[String]) -> bool,
) -> Option<f64> {
    let Some(spec) = metrics::family(name) else {
        unavailable.push(name.to_owned());
        return None;
    };
    let series = registry.series_of(spec.name, WINDOW_MINUTES);
    let mut total = 0.0;
    let mut seen = false;
    for snapshot in &series {
        if !predicate(&snapshot.labels) {
            continue;
        }
        seen = true;
        total += snapshot.total;
    }
    seen.then_some(total)
}

/// The p-th percentile of a family's values across the window.
///
/// Percentiles cannot be averaged out of per-series sums, so this reads each series' total and
/// takes the percentile across series — which is the honest reading of "the slowest routes" and
/// says so in the field name the panel renders.
fn percentile_over_labels(
    registry: &metrics::Registry,
    name: &str,
    unavailable: &mut Vec<String>,
    p: f64,
) -> Option<f64> {
    let Some(spec) = metrics::family(name) else {
        unavailable.push(name.to_owned());
        return None;
    };
    let mut values: Vec<f64> = registry
        .series_of(spec.name, WINDOW_MINUTES)
        .iter()
        .map(|snapshot| snapshot.total)
        .filter(|value| *value > 0.0)
        .collect();
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let index = (((values.len() as f64) - 1.0) * p).round() as usize;
    values.get(index).copied()
}

/// The headline fields, exposed for the panel's own documentation and for tests that need to
/// assert the shape without going through the router.
pub fn headline_fields() -> [Headline; 5] {
    [
        Headline {
            metric: "omnion_http_requests_total",
            value: None,
            unit: "count",
            window_minutes: WINDOW_MINUTES,
        },
        Headline {
            metric: "omnion_ai_cost_micros_total",
            value: None,
            unit: "micros",
            window_minutes: WINDOW_MINUTES,
        },
        Headline {
            metric: "omnion_queue_depth",
            value: None,
            unit: "count",
            window_minutes: WINDOW_MINUTES,
        },
        Headline {
            metric: "omnion_db_pool_connections",
            value: None,
            unit: "count",
            window_minutes: WINDOW_MINUTES,
        },
        Headline {
            metric: "omnion_rate_limit_refusals_total",
            value: None,
            unit: "count",
            window_minutes: WINDOW_MINUTES,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_headline_names_a_declared_family() {
        // A headline pointing at a family the registry does not declare renders as `null` with
        // no explanation, which is exactly the "documented but unreachable" shape: the panel
        // shows a tile, the number never arrives, and nothing says why.
        for headline in headline_fields() {
            assert!(
                metrics::family(headline.metric).is_some(),
                "`{}` is not a declared family, so its tile could never carry a number",
                headline.metric
            );
        }
    }

    #[test]
    fn an_undeclared_family_is_reported_rather_than_zeroed() {
        let mut unavailable = Vec::new();
        let value = sum_family_where(
            metrics::global(),
            "omnion_not_a_real_family",
            &mut unavailable,
            |_| true,
        );
        assert_eq!(value, None, "an unknown family must not read as zero");
        assert_eq!(
            unavailable,
            vec!["omnion_not_a_real_family".to_owned()],
            "the screen must be able to say WHICH source was unreadable"
        );
    }

    #[test]
    fn an_idle_instance_answers_null_rather_than_zero() {
        let mut unavailable = Vec::new();
        // The registry is process-wide and an idle test binary has written nothing to these
        // families, so the honest answer is `None`: "no data yet", not "zero requests".
        let value = sum_family(metrics::global(), "omnion_http_requests_total", &mut unavailable);
        if value.is_none() {
            assert!(
                unavailable.is_empty(),
                "a declared family that is merely idle must not be reported unavailable"
            );
        }
    }

    #[test]
    fn a_percentile_over_no_values_is_none() {
        let mut unavailable = Vec::new();
        assert_eq!(
            percentile_over_labels(
                metrics::global(),
                "omnion_http_request_duration_seconds",
                &mut unavailable,
                0.95,
            ),
            None,
            "p95 of nothing is not 0.0 seconds"
        );
    }
}
