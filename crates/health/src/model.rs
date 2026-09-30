//! The sample, the service report and the overview the panel reads.
//!
//! A **sample** is one metric, one value, one moment. That is the only thing this
//! crate ever writes per probe, and the reason is volume: a per-request sample
//! would put a row in the table per metric per HTTP request, and a busy panel
//! would bloat it within days (the request's own risk note says so). One row per
//! metric per interval is the ceiling, and aggregation happens on read.
//!
//! The **`ServiceReport`** is the other shape, and it is deliberately *not* the
//! same thing as a sample. A sample is a past fact; a report is what the last run
//! concluded right now. They are separate because the panel has to render "last
//! checked 40 seconds ago" and a service that has never run as two different
//! things, and a single `Option`-heavy row shape invites exactly the client that
//! collapses them.

use time::OffsetDateTime;

use crate::error::{HealthError, Result};
use crate::vocabulary::{
    HOST_METRICS, HOST_SERVICE, MAX_DETAIL_CHARS, SERVICES, STATE_WHEN_UNPROBED, all_services,
    is_finite, is_state,
};

/// One metric, one value, one moment.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Sample {
    /// The row's id.
    pub id: i64,
    /// Which service measured it.
    pub service: String,
    /// Which metric, e.g. `cpu_percent`.
    pub metric: String,
    /// The measured value.
    pub value: f64,
    /// The unit the value is in, e.g. `%`, `ms`, `bytes`.
    pub unit: String,
    /// The state of the service *at the moment the sample was taken*.
    pub state: String,
    /// Structured detail; the panel renders named fields and never a blob.
    pub detail: serde_json::Value,
    /// When it was measured.
    pub sampled_at: OffsetDateTime,
}

/// A sample before it is stored.
///
/// The split is what makes "a probe that returns a nonsense number cannot poison
/// the table" a constructor's job rather than the caller's: [`Sample::validate`]
/// refuses a non-finite value and an untrimmed detail blob, so every write path
/// — the runner, the manual run, a future sweep — inherits the same rules
/// without repeating them.
#[derive(Debug, Clone, PartialEq)]
pub struct NewSample {
    /// Which service measured it.
    pub service: String,
    /// Which metric.
    pub metric: String,
    /// The measured value.
    pub value: f64,
    /// The unit the value is in.
    pub unit: String,
    /// The state of the service when it was taken.
    pub state: String,
    /// Structured detail.
    pub detail: serde_json::Value,
}

impl NewSample {
    /// Rejects a sample the panel could not honestly render.
    ///
    /// * A `NaN` or an `Infinity` is refused rather than stored: they serialise to
    ///   `null` in JSON and to `NaN` in CSV, and the acceptance criteria name
    ///   "no `NaN`/`Infinity` text anywhere" as a visual check. Refusing at the
    ///   door is the only place that cannot be forgotten.
    /// * A state outside the vocabulary is refused, because the panel's colour map
    ///   is `Record<State, string>` and an unlisted key renders as `undefined` in a
    ///   class attribute.
    /// * A detail document over the cap is refused rather than truncated: a
    ///   truncated JSON object is not valid JSON, and the panel parses it.
    pub fn validate(&self) -> Result<()> {
        if !is_finite(self.value) {
            return Err(HealthError::invalid(format!(
                "metric {} of service {} is not a finite number",
                self.metric, self.service
            )));
        }
        if !is_state(&self.state) {
            return Err(HealthError::invalid(format!(
                "{} is not a service state; expected one of healthy, degraded, down, unknown",
                self.state
            )));
        }
        let rendered = self.detail.to_string();
        if rendered.chars().count() > MAX_DETAIL_CHARS {
            return Err(HealthError::invalid(format!(
                "the detail of {} / {} is larger than {MAX_DETAIL_CHARS} characters",
                self.service, self.metric
            )));
        }
        if !SERVICES.contains(&self.service.as_str())
            && self.service != HOST_SERVICE
        {
            return Err(HealthError::invalid(format!(
                "{} is not a registered service",
                self.service
            )));
        }
        Ok(())
    }
}

/// What the last run concluded about one service.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ServiceReport {
    /// The service key, e.g. `redis`.
    pub service: String,
    /// One of [`crate::vocabulary::STATES`].
    pub state: String,
    /// How long the probe took, in milliseconds. `None` when the probe never ran.
    pub latency_ms: Option<i64>,
    /// When the probe ran. `None` when it never has.
    pub checked_at: Option<OffsetDateTime>,
    /// The sentence an operator reads. Never empty for a non-`unknown` state: a
    /// red row without a reason is a row that costs the reader a dig.
    pub message: String,
    /// Named fields behind the message. The panel renders these by key.
    pub detail: serde_json::Value,
    /// The registry's own description of what this probe checks, so the detail
    /// screen can explain the row without a second call.
    pub checks: Vec<CheckDescription>,
}

/// One named thing a probe verified, for the detail screen's checks table.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CheckDescription {
    /// What was checked, e.g. `ping`.
    pub check: String,
    /// Its own state — a service can be `degraded` while each of its checks reads
    /// `healthy`, and that disagreement is information, not a bug.
    pub state: String,
    /// The sentence for this one check.
    pub message: String,
    /// How long it took.
    pub latency_ms: i64,
}

impl ServiceReport {
    /// A service that has never been probed.
    ///
    /// Constructed with the registry's own key list rather than from whatever the
    /// store happened to return, which is what guarantees the overview always
    /// carries every registered row.
    #[must_use]
    pub fn unprobed(service: &str, description: &'static str) -> Self {
        Self {
            service: service.to_string(),
            state: STATE_WHEN_UNPROBED.to_string(),
            latency_ms: None,
            checked_at: None,
            message: description.to_string(),
            detail: serde_json::json!({}),
            checks: Vec::new(),
        }
    }

    /// The full overview: one row per registered service, always.
    ///
    /// `reports` holds whatever the store returned — every service that has a
    /// sample, and nothing else. Anything in `reports` that is not a registered
    /// service is dropped, and anything registered that is missing from `reports`
    /// becomes an `unknown` row. The asymmetry is the point: a service that has
    /// never been probed must be *visible*, or the screen's "everything is fine"
    /// banner is true for the wrong reason.
    #[must_use]
    pub fn overview(
        reports: Vec<ServiceReport>,
        descriptions: &[(&str, &'static str)],
        host: Vec<HostMetric>,
    ) -> HealthOverview {
        let mut services: Vec<ServiceReport> = Vec::with_capacity(SERVICES.len() + 1);
        for key in all_services() {
            match reports.iter().find(|report| report.service == key) {
                Some(report) => services.push(report.clone()),
                None => {
                    let description = descriptions
                        .iter()
                        .find(|(candidate, _)| *candidate == key)
                        .map_or("Not checked yet.", |(_, text)| *text);
                    services.push(Self::unprobed(key, description));
                }
            }
        }
        let worst = crate::vocabulary::worst(
            services
                .iter()
                .map(|report| (report.service.as_str(), report.state.as_str())),
        );
        let summary = match worst {
            None => HealthBanner {
                state: STATE_WHEN_UNPROBED.to_string(),
                headline: "No services have been checked yet".to_string(),
                worst_service: None,
            },
            Some(("down", service)) => HealthBanner {
                state: "down".to_string(),
                headline: format!("Down: {}", title_case(service)),
                worst_service: Some(service.to_string()),
            },
            Some(("degraded", service)) => HealthBanner {
                state: "degraded".to_string(),
                headline: format!("Degraded: {}", title_case(service)),
                worst_service: Some(service.to_string()),
            },
            Some(("healthy", _)) => HealthBanner {
                state: "healthy".to_string(),
                // The one place this screen is allowed to reassure, and only when
                // every registered row really is healthy. `all_services()` is
                // walked above, so an unprobed row can never be in this branch.
                headline: "All systems operational".to_string(),
                worst_service: None,
            },
            // Anything that is neither healthy, degraded nor down. `unknown` is
            // its own sentence and it is deliberately *not* the healthy branch:
            // a platform nobody looked at must not read as operational.
            Some((other, service)) => HealthBanner {
                state: other.to_string(),
                headline: format!("{}: {}", title_case(other), title_case(service)),
                worst_service: Some(service.to_string()),
            },
        };
        HealthOverview {
            services,
            host,
            banner: summary,
            last_checked_at: reports
                .iter()
                .filter_map(|report| report.checked_at)
                .max(),
        }
    }
}

/// The banner's three fields, so the panel renders the platform's conclusion
/// without recomputing it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct HealthBanner {
    /// The worst state across the registered services.
    pub state: String,
    /// The sentence, e.g. `All systems operational`.
    pub headline: String,
    /// Which service carries the worst state, when one does.
    pub worst_service: Option<String>,
}

/// One host metric, already thresholded.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct HostMetric {
    /// The metric key, e.g. `cpu_percent`.
    pub metric: String,
    /// The current value.
    pub value: f64,
    /// The unit it is in.
    pub unit: String,
    /// `healthy`, `degraded` or `down` — derived from the thresholds in force,
    /// not chosen by the caller.
    pub state: String,
    /// The warn threshold that produced that state, when one is configured.
    pub threshold: Option<f64>,
}

/// The whole overview payload.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct HealthOverview {
    /// One row per registered service, never fewer.
    pub services: Vec<ServiceReport>,
    /// The host's own metrics.
    pub host: Vec<HostMetric>,
    /// The worst state, as a sentence.
    pub banner: HealthBanner,
    /// The newest sample moment across all services, for the header's "last
    /// checked".
    pub last_checked_at: Option<OffsetDateTime>,
}

impl HealthOverview {
    /// `true` when nothing is wrong — and deliberately *not* true when a service
    /// is merely unprobed, so the auto-refresh spinner can say "we do not know
    /// yet" rather than "fine".
    #[must_use]
    pub fn is_all_operational(&self) -> bool {
        self.banner.state == "healthy"
    }

    /// The number of services in each state, for the summary tiles.
    #[must_use]
    pub fn counts(&self) -> std::collections::BTreeMap<String, usize> {
        let mut counts: std::collections::BTreeMap<String, usize> =
            crate::vocabulary::STATES
                .iter()
                .map(|state| ((*state).to_string(), 0usize))
                .collect();
        for report in &self.services {
            *counts.entry(report.state.clone()).or_insert(0) += 1;
        }
        counts
    }
}

/// `redis` → `Redis`, `s3` → `S3`. Only ever used for the banner's sentence, so
/// the mapping is a small match rather than a general title-caser: `s3` and `api`
/// both break a naive rule that capitalises every alphabetic run.
fn title_case(service: &str) -> String {
    match service {
        "s3" => "S3".to_string(),
        "api" => "API".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

/// The metric keys the host probe publishes, re-exported so the panel and the
/// threshold form read one list.
pub fn host_metrics() -> &'static [&'static str] {
    HOST_METRICS
}

/// A number of seconds as the sentence an operator reads.
///
/// A duration in raw seconds is a number only the machine is sure about: "43200"
/// on a queue row is a question ("43200 what?") that costs a reader a division.
/// The units are coarse on purpose — an operator deciding whether to page needs
/// "2 h", not "1 h 12 m 3 s" — and a negative value is rendered as "0 s" rather
/// than as a negative duration, because the only way to get one is a clock that
/// moved backwards between two reads and a negative age is not worth showing.
#[must_use]
pub fn humanize_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let remainder = seconds % 60;
    if days > 0 {
        format!("{days} d {hours} h")
    } else if hours > 0 {
        format!("{hours} h {minutes} m")
    } else if minutes > 0 {
        format!("{minutes} m {remainder} s")
    } else {
        format!("{remainder} s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::HealthError;
    use crate::vocabulary::rank;

    fn sample(service: &str, value: f64) -> NewSample {
        NewSample {
            service: service.to_string(),
            metric: "latency_ms".to_string(),
            value,
            unit: "ms".to_string(),
            state: "healthy".to_string(),
            detail: serde_json::json!({}),
        }
    }

    #[test]
    fn a_nan_sample_is_refused_at_the_door() {
        let mut bad = sample("redis", f64::NAN);
        assert!(bad.validate().is_err());
        bad.value = f64::INFINITY;
        let error = bad.validate().unwrap_err();
        assert!(
            matches!(error, HealthError::Invalid(_)),
            "a non-finite value is the caller's fault, not a store failure"
        );
    }

    #[test]
    fn an_unknown_state_and_an_unknown_service_are_both_refused() {
        let mut bad = sample("redis", 1.0);
        bad.state = "ok".to_string();
        assert!(bad.validate().is_err(), "ok is not a service state");

        let mut wrong = sample("redis", 1.0);
        wrong.service = "postgres-cluster".to_string();
        assert!(
            wrong.validate().is_err(),
            "a service outside the registry cannot be stored"
        );
    }

    #[test]
    fn an_oversized_detail_is_refused_not_truncated() {
        let mut big = sample("redis", 1.0);
        big.detail = serde_json::json!({ "note": "x".repeat(MAX_DETAIL_CHARS + 10) });
        assert!(big.validate().is_err());
    }

    #[test]
    fn a_plain_sample_is_accepted() {
        assert!(sample("redis", 12.5).validate().is_ok());
        assert!(sample(HOST_SERVICE, 3.0).validate().is_ok());
    }

    fn report(service: &str, state: &str) -> ServiceReport {
        ServiceReport {
            service: service.to_string(),
            state: state.to_string(),
            latency_ms: Some(4),
            checked_at: Some(OffsetDateTime::UNIX_EPOCH),
            message: "answered".to_string(),
            detail: serde_json::json!({}),
            checks: Vec::new(),
        }
    }

    #[test]
    fn the_overview_always_has_a_row_for_every_registered_service() {
        // Two services probed, the other six not: the six must still be rows, and
        // they must be `unknown` — a fresh database has no samples at all and a
        // screen that renders five rows is a screen that will render five rows on
        // a half-migrated database too.
        let overview = ServiceReport::overview(
            vec![report("api", "healthy"), report("redis", "down")],
            &[],
            Vec::new(),
        );
        assert_eq!(overview.services.len(), 8);
        let unprobed: Vec<&str> = overview
            .services
            .iter()
            .filter(|r| r.state == "unknown")
            .map(|r| r.service.as_str())
            .collect();
        assert_eq!(unprobed.len(), 6);
        assert!(!overview.is_all_operational());
    }

    #[test]
    fn a_stored_row_for_a_service_nobody_registered_is_dropped() {
        // The mirror of the test above: a report for a key that is not in the
        // registry must not become a ninth row. A migration that renames a
        // service leaves old samples behind, and the overview must not grow a row
        // nobody's screen knows how to colour.
        let overview = ServiceReport::overview(
            vec![report("api", "healthy"), report("mystery", "down")],
            &[],
            Vec::new(),
        );
        assert_eq!(overview.services.len(), 8);
        assert!(overview.services.iter().all(|r| r.service != "mystery"));
        assert_eq!(
            overview.banner.state, "unknown",
            "dropping an unknown service must not also promote the banner to healthy"
        );
    }

    #[test]
    fn the_banner_names_the_worst_service() {
        let overview = ServiceReport::overview(
            vec![
                report("api", "healthy"),
                report("postgres", "healthy"),
                report("redis", "degraded"),
                report("s3", "healthy"),
                report("workers", "healthy"),
                report("queue", "healthy"),
                report("search", "healthy"),
                report(HOST_SERVICE, "healthy"),
            ],
            &[],
            Vec::new(),
        );
        assert_eq!(overview.banner.state, "degraded");
        assert_eq!(overview.banner.worst_service.as_deref(), Some("redis"));
        assert_eq!(overview.banner.headline, "Degraded: Redis");
        assert!(!overview.is_all_operational());
    }

    #[test]
    fn everything_healthy_says_so_in_words() {
        let all: Vec<ServiceReport> = crate::vocabulary::all_services()
            .into_iter()
            .map(|key| report(key, "healthy"))
            .collect();
        let overview = ServiceReport::overview(all, &[], Vec::new());
        assert!(overview.is_all_operational());
        assert_eq!(overview.counts()["healthy"], 8);
    }

    #[test]
    fn the_counts_carry_every_state_even_at_zero() {
        // The panel's tiles index the map directly; a missing key renders
        // `undefined` in a count badge.
        let overview = ServiceReport::overview(vec![report("api", "healthy")], &[], Vec::new());
        let counts = overview.counts();
        for state in crate::vocabulary::STATES {
            assert!(counts.contains_key(*state), "{state} is always present");
        }
        assert_eq!(counts["healthy"], 1);
        assert_eq!(counts["down"], 0);
    }

    #[test]
    fn the_banner_spells_the_acronyms_the_way_an_operator_reads_them() {
        assert_eq!(title_case("s3"), "S3");
        assert_eq!(title_case("api"), "API");
        assert_eq!(title_case("redis"), "Redis");
        assert_eq!(title_case(""), "");
    }

    #[test]
    fn ranking_is_exported_for_the_summary_route() {
        // The ranking is the whole of "which service names the banner", so it is
        // asserted here as well as in the vocabulary's own module: this is the
        // copy the API layer reads, and a reordered ranking that left this file
        // passing would be a banner that names the wrong service.
        assert!(rank("down") > rank("degraded"));
        assert!(rank("degraded") > rank("unknown"));
        assert!(rank("unknown") > rank("healthy"));
    }
}
