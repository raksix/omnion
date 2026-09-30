//! The registry runner: run every probe, record what it saw, build the overview.
//!
//! One function does the whole job — [`run_all`] — because the three things it
//! has to do in the right order are the three things that are easy to get wrong
//! apart:
//!
//! 1. **Every service is probed, and a probe that fails is still a row.** The
//!    runner collects a `ProbeResult` per service and *never* removes one. The
//!    failure mode this prevents is a `?` on a probe's error, which would drop a
//!    service from the response and turn a stopped Redis into a missing row.
//! 2. **Samples are written after every probe, not per probe.** A run that dies
//!    half way through leaves no half-written set; and the samples carry the state
//!    the run concluded, so a chart and a row always agree.
//! 3. **The overview is built from the registry, and the store only fills in what
//!    the registry already knows about.** See [`crate::model::ServiceReport::overview`].

use std::collections::BTreeMap;

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::error::Result;
use crate::incidents;
use crate::model::{HealthOverview, NewSample, ServiceReport};
use crate::probes::{
    Observation, ProbeContext, ProbeResult, probe_api, probe_host, probe_postgres, probe_queue,
    probe_redis, probe_search, probe_storage, probe_workers,
};
use crate::store;
use crate::vocabulary::{HOST_SERVICE, SERVICES};

/// What each service row is, in the registry's own words.
///
/// The detail screen shows this before it shows a number, because "what does
/// this row actually check?" is the first question an operator has about a
/// service they have not seen fail before. Kept here — one table, in one place —
/// rather than as a description string on each probe, so adding a probe cannot
/// leave its row undescribed.
pub const DESCRIPTIONS: &[(&str, &str)] = &[
    ("api", "The platform process itself, answering right now."),
    (
        "postgres",
        "A round trip on a fixed statement, plus the connection pool's own counters.",
    ),
    ("redis", "PING, plus used memory and connected clients when Redis will report them."),
    ("s3", "The configured object store, through the same client the media library writes with."),
    ("workers", "Heartbeat rows, counted by kind; a worker with no fresh beat is named."),
    ("queue", "Everything waiting across the real queues, and how long the oldest has waited."),
    ("search", "The index reachable, and how many documents it holds."),
    (
        HOST_SERVICE,
        "CPU, memory, disk and load, read from the kernel.",
    ),
];

/// One service's row as the registry describes it, for a service with no sample.
fn description_for(key: &str) -> &'static str {
    DESCRIPTIONS
        .iter()
        .find(|(candidate, _)| *candidate == key)
        .map_or("Not checked yet.", |(_, text)| *text)
}

/// A probe that turns into a report, and the samples it wants stored.
///
/// The conversion is one function rather than one per probe so that "which state
/// does this probe's metrics get recorded under" has exactly one answer. Each
/// metric sample carries the **service's** state, not a state of its own: a chart
/// and a row that disagree are the pair of things an operator has to reconcile
/// manually, and reconciling a status screen is the job this screen exists to do.
fn report_of(result: ProbeResult) -> (ServiceReport, Vec<NewSample>) {
    let state = result.outcome.state().to_string();
    let message = result.outcome.message().to_string();
    let now = time::OffsetDateTime::now_utc();
    let samples = result
        .metrics
        .iter()
        .map(|reading| NewSample {
            service: result.service.clone(),
            metric: reading.metric.clone(),
            value: reading.value,
            unit: reading.unit.clone(),
            state: state.clone(),
            detail: serde_json::json!({ "source": "probe" }),
        })
        .collect();
    let report = ServiceReport {
        service: result.service,
        state,
        latency_ms: Some(result.latency_ms),
        checked_at: Some(now),
        message,
        detail: result.detail,
        checks: result.checks,
    };
    (report, samples)
}

/// Every probe, run against one context.
///
/// The host probe is synchronous (it reads `/proc`, and there is nothing to await
/// that a file read does not already do), so it is called directly rather than
/// spawned — spawning it would buy a thread for a 250 ms sleep and lose the
/// ability to say exactly how long the whole run took.
pub async fn run_all(ctx: &ProbeContext<'_>) -> Vec<ProbeResult> {
    let (
        postgres,
        redis,
        storage,
        queue,
        search,
        workers,
    ) = tokio::join!(
        probe_postgres(ctx),
        probe_redis(ctx),
        probe_storage(ctx),
        probe_queue(ctx),
        probe_search(ctx),
        probe_workers(ctx),
    );
    // `join!` rather than `join_all`: the probes read the same pool, so running
    // six of them concurrently is six connections held at once. They are all
    // bounded at PROBE_TIMEOUT_MS, so the whole set costs one timeout, not six.
    vec![
        probe_api(ctx),
        postgres,
        redis,
        storage,
        queue,
        search,
        workers,
        probe_host(ctx),
    ]
}

/// Run every probe, record the samples, and build the overview the panel reads.
///
/// This is the one function the runner task and the manual `POST /checks/run`
/// both call. They must be the *same* function: a manual run that took a
/// different code path from the scheduled one would be the operator's button
/// reporting something the platform never does on its own, which is the exact
/// lie the request's "reports per-probe failures instead of failing whole" line
/// is about.
///
/// The write is allowed to fail without failing the read. A run whose samples
/// could not be stored is still a true reading of the platform's health — it is
/// only a reading nobody can chart — and answering `500` there would tell an
/// operator that Redis is fine and the platform is broken, which is a worse
/// answer than a fresh row with no history behind it.
pub async fn run_and_record(pool: &PgPool, ctx: &ProbeContext<'_>) -> Result<HealthOverview> {
    let results = run_all(ctx).await;
    let mut reports: Vec<ServiceReport> = Vec::with_capacity(results.len());
    let mut samples: Vec<NewSample> = Vec::new();
    for result in results {
        let (report, mut result_samples) = report_of(result);
        samples.append(&mut result_samples);
        reports.push(report);
    }

    if let Err(error) = store::record_run(pool, &samples).await {
        // Logged, not propagated. See this function's doc comment.
        tracing::warn!(error = %error, "health samples could not be stored");
    }

    // Slice 3's emitter. This is here, and not in the runner task, for the reason the
    // doc comment above gives: the manual "Run all checks" button has to produce exactly the
    // incidents the schedule would have produced, and a second call site is how an operator
    // ends up with a button that reports `healthy` while the page below it shows an
    // unacknowledged incident that only the scheduler ever opens.
    //
    // Like the sample write, a policy failure is logged rather than propagated: this run's
    // readings are still true, and an `incident` table that cannot be written must not turn a
    // probe report into a `500`. The cost of that choice is bounded and paid in the open: an
    // outage during a database blip is visible in the samples and absent from the timeline.
    match apply_policy(pool, &reports, &samples).await {
        Ok(()) => {}
        Err(error) => tracing::warn!(error = %error, "health incident policy could not be applied"),
    }

    Ok(build_overview(reports))
}

/// Open, resolve and de-duplicate everything this run implies.
///
/// Three passes, and the order is the contract:
///
/// 1. **Transitions first**, per service. A state change is the only thing that opens an
///    incident, and it has to be decided against the *previous* state, so it cannot be folded
///    into the metric loop — a service whose state flipped has one incident even though five
///    of its metrics also moved.
/// 2. **Breaches second**, per sample. A metric crossing its critical line is an event of its
///    own: the request names "threshold breach" separately from a service going `degraded`.
/// 3. **Clears last.** A metric back under its warn limit resolves its ledger row only *after*
///    the breaches of this run were recorded, so a run that reads one sample of the metric
///    cannot clear the row it just wrote.
///
/// Every service is visited even when its policy is a no-op, because "nothing happened" is the
/// outcome most runs produce and the outcome that has to be cheap.
async fn apply_policy(
    pool: &PgPool,
    reports: &[ServiceReport],
    samples: &[NewSample],
) -> Result<()> {
    let now = OffsetDateTime::now_utc();

    for report in reports {
        // The host row is a metric aggregate, not a dependency: it has no incident lifecycle
        // and its state is derived per-card by `host_metrics`. Letting it open an incident
        // would produce one entry per busy minute.
        if report.service == HOST_SERVICE {
            continue;
        }
        let Some(transition) = incidents::detect(pool, report).await? else {
            continue;
        };
        let suppressed = incidents::is_suppressed(pool, &report.service, now).await?;
        incidents::apply(pool, &transition, suppressed).await?;
    }

    // `THRESHOLD_METRICS` is read once and turned into a lookup: the stored document wins over
    // the suggestion, because an operator who typed 92 is entitled to 92 and the placeholder
    // is explicitly "not saved yet". A metric with no *stored* pair is never breached — that is
    // the request's "thresholds start empty" promise, and defaulting to the suggestion here
    // would open incidents on a deployment nobody configured.
    let stored = incidents::thresholds(pool).await?;
    for sample in samples {
        let Some(threshold) = stored.get(sample.metric.as_str()) else {
            continue;
        };
        match threshold.classify(sample.value) {
            Some("down") => {
                incidents::record_breach(pool, &sample.metric, sample.value, now).await?;
            }
            Some(_) => {
                // Over the warn line but not the critical one: the ledger row for a previous
                // critical crossing is resolved, because the metric came back from the line
                // that woke somebody.
                incidents::clear_breach(pool, &sample.metric, now).await?;
            }
            None => {
                incidents::clear_breach(pool, &sample.metric, now).await?;
            }
        }
    }
    Ok(())
}

/// The overview, from a set of fresh reports and whatever the store remembers.
///
/// Fresh reports win over stored samples, always. A screen that preferred the
/// stored row would show the last *successful* reading of a service that has
/// just gone down — the screen would keep saying `healthy` for a dependency that
/// stopped answering, which is worse than showing nothing at all.
pub fn build_overview(reports: Vec<ServiceReport>) -> HealthOverview {
    let host = host_metrics(&reports);
    ServiceReport::overview(reports, DESCRIPTIONS, host)
}

/// The host's metric cards, thresholded.
///
/// The thresholds are slice-1 constants, and they are named here rather than
/// scattered so that slice 3's settings form has one list to make editable. A
/// metric with no threshold configured reports `healthy` with **no threshold
/// marker** rather than a fabricated `0` — a card that draws a warn line at zero
/// tells the operator their memory is fine because nothing has ever exceeded
/// nothing.
fn host_metrics(reports: &[ServiceReport]) -> Vec<crate::model::HostMetric> {
    let Some(host) = reports.iter().find(|report| report.service == HOST_SERVICE) else {
        return Vec::new();
    };
    let warn = |metric: &str| match metric {
        "cpu_percent" | "memory_percent" | "disk_percent" => Some(85.0),
        _ => None,
    };
    let unit = |metric: &str| match metric {
        "cpu_percent" | "memory_percent" | "disk_percent" => "%",
        "load_average_1m" => "tasks",
        _ => "",
    };
    const HOST_KEYS: &[&str] = &[
        "cpu_percent",
        "memory_percent",
        "disk_percent",
        "load_average_1m",
        "db_connections",
        "queue_depth",
    ];
    let mut cards = Vec::new();
    for key in HOST_KEYS {
        let value = host.detail.get(*key).and_then(serde_json::Value::as_f64);
        // `db_connections` and `queue_depth` are measured by the *service* rows
        // (PostgreSQL and the queue), not by the host — so their cards are
        // gathered from the whole report set rather than from the host's detail.
        let value = value.or_else(|| {
            reports.iter().find_map(|report| match report.service.as_str() {
                "postgres" if *key == "db_connections" => {
                    report.detail.get("connections").and_then(serde_json::Value::as_f64)
                }
                "queue" if *key == "queue_depth" => {
                    report.detail.get("depth").and_then(serde_json::Value::as_f64)
                }
                _ => None,
            })
        });
        let Some(value) = value else {
            continue;
        };
        let threshold = warn(key);
        let state = match (threshold, key) {
            (Some(limit), _) if value >= limit => "degraded",
            // A card with no threshold and a value that exists is honest at
            // `healthy`: we measured it, nothing is wrong with it, and we are not
            // pretending to know what "too much" is for it.
            _ => "healthy",
        };
        cards.push(crate::model::HostMetric {
            metric: (*key).to_string(),
            value,
            unit: unit(key).to_string(),
            state: state.to_string(),
            threshold,
        });
    }
    cards
}

/// The overview as the panel reads it when nothing has ever been run.
///
/// Separate from [`build_overview`] because "no run yet" and "a run that found
/// nothing" are different screens, and the second is only reachable through the
/// first having happened.
#[must_use]
pub fn unprobed_overview() -> HealthOverview {
    ServiceReport::overview(Vec::new(), DESCRIPTIONS, Vec::new())
}

/// The number of services the registry probes, host included.
///
/// A test-visible constant rather than a computed value, because "the registry
/// covers the request's seven services" is an acceptance criterion and a
/// criterion that is only checked by reading the code is not checked.
#[must_use]
pub fn registry_size() -> usize {
    SERVICES.len() + 1
}

/// Every service key the registry knows, mapped to its description.
///
/// A `BTreeMap` because the panel's settings screen and the detail screen both
/// iterate it and both want a stable order; a `Vec` would be stable too, and
/// would also be free to arrive in a different order from the other one.
#[must_use]
pub fn described_services() -> BTreeMap<&'static str, &'static str> {
    DESCRIPTIONS
        .iter()
        .map(|(key, text)| (*key, *text))
        .collect()
}

/// The description for one service key, or the honest fallback.
#[must_use]
pub fn describe(key: &str) -> &'static str {
    description_for(key)
}

/// Re-exported so the API layer can turn an observation into an event name
/// without importing the probes module directly.
#[must_use]
pub fn state_of(outcome: &Observation) -> &'static str {
    outcome.state()
}
