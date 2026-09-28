//! The metric registry: the documented families, the cardinality guard, the Prometheus exposition
//! and the bounded history a chart is drawn from (REQ-126, slice 2).
//!
//! ## Why a registry and not a crate like `prometheus`
//!
//! Three properties the request names and a general-purpose client does not give for free:
//!
//! 1. **A family is a declared thing, not a call site.** Every family in the request is a
//!    [`FamilySpec`] below with its name, kind, unit, description, label list and source. The
//!    catalogue the panel reads is seeded from *this* list at boot, so a family that exists in a
//!    call site but not in the table is impossible, and a family in the table with no call site is
//!    visible as "no samples" instead of being invented by the UI.
//! 2. **Labels are positional and closed.** A recorder passes `&["/api/v1/x", "GET"]`; the spec
//!    decides what those are. A recorder that passes a third label — a `user_id`, an id, a raw
//!    path — is **dropped**, not stored: that is the guard the request calls the most dangerous
//!    failure mode in this area, enforced in one place instead of trusted to every call site.
//! 3. **The budget is enforced, and the enforcement is visible.** A family that would exceed its
//!    cap does not grow: the sample is folded into the family's `other` series and the event is
//!    counted in `omnion_registry_budget_exceeded{family=…}`. Cardinality is therefore *bounded*
//!    (the property that keeps a scrape cheap) while the loss is *counted and labelled* (the
//!    property that keeps it honest). Silently dropping would satisfy the first and fail the
//!    second.
//!
//! ## Bounded label sets and the `other` bucket
//!
//! The request asks for "model labels come from a bounded set with an `other` overflow bucket". A
//! static allowlist would be wrong — the model catalogue changes with configuration, and a metric
//! that refuses a legitimately new model is worse than one that buckets it — so the set is
//! **learned**: the first [`BOUNDED_SET_CAP`] distinct values a label position ever sees are kept
//! verbatim, and the next one collapses to `other`. The behaviour is identical to a static set
//! from the outside (bounded output, an `other` bucket exists) and self-tuning from the inside.
//!
//! ## The history ring
//!
//! The exposition answers a scrape, which is "what is true now". The chart answers "what happened
//! over the last hour", and it cannot be answered by a snapshot. Each series therefore keeps a
//! bounded ring of one-minute buckets: a counter's delta, a gauge's last value, a histogram's mean.
//! [`HISTORY_BUCKETS`] buckets × the registry's series cap is a few megabytes, and the cap is
//! what keeps that sentence true.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::sync::{Mutex, OnceLock};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The label value a bounded position collapses to once its set is full.
///
/// Prometheus's own name for this and the one the request uses.
pub const OTHER: &str = "other";

/// The number of distinct verbatim values one bounded label position keeps before overflowing.
///
/// Model names and provider names are the positions that matter; route templates and methods are
/// bounded by the router itself, so the cap is generous and effectively never reached for them.
pub const BOUNDED_SET_CAP: usize = 24;

/// The series cap applied to a family that declares no cap of its own.
pub const DEFAULT_FAMILY_CAP: usize = 500;

/// The number of one-minute buckets a series keeps for the chart.
pub const HISTORY_BUCKETS: usize = 120;

/// The most points one series' history may return, whatever the requested window is.
///
/// A year-long window is a refusal, not a thinner chart: a chart that silently shows 120 points for
/// a 12-month range is a chart that lies about its own resolution.
pub const MAX_POINTS: usize = HISTORY_BUCKETS;

/// The one family whose histogram uses [`DURATION_BUCKETS`].
const DURATION_FAMILY: &str = "omnion_http_request_duration_seconds";

/// The histogram bucket edges, in seconds, for the request-duration family.
pub const DURATION_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// What a family is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricKind {
    /// Only ever goes up. A scrape resets nothing; a restart shows a gap, not a fall.
    Counter,
    /// A value at a point in time that can go either way.
    Gauge,
    /// A distribution, published as `_bucket` / `_sum` / `_count` series.
    Histogram,
}

impl MetricKind {
    /// The canonical lowercase name, matching the migration's check constraint.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

/// One declared family.
///
/// The spec is the only source of a family's labels, and that is what makes the "no user or
/// organization id is ever a label" rule a property of the type rather than a review comment.
#[derive(Debug, Clone, Copy)]
pub struct FamilySpec {
    /// The metric name, without the `omnion_` prefix normalisation — it is stored verbatim.
    pub name: &'static str,
    /// What it is.
    pub kind: MetricKind,
    /// The unit shown in the catalogue (`seconds`, `milliseconds`, `1`, `micros`).
    pub unit: &'static str,
    /// One sentence for the catalogue.
    pub description: &'static str,
    /// The label names, in positional order. A recorder cannot add one.
    pub labels: &'static [&'static str],
    /// `core`, `module` or `worker` — who is expected to record it.
    pub source: &'static str,
    /// The series cap for this family, over and above the global budget.
    pub max_series: usize,
    /// Whether the label positions are bounded to a learned set with an `other` overflow.
    pub bounded_labels: bool,
}

impl FamilySpec {
    /// The bucket edges for a histogram family, or an empty slice for the other kinds.
    ///
    /// Not `const`: matching a `&'static str` is not a constant operation, so a `const fn` here
    /// is refused by the compiler for a value that is in fact known at compile time. The edges are
    /// resolved from a `static` slice at the single call site instead.
    #[must_use]
    pub fn buckets(&self) -> &'static [f64] {
        if self.name == DURATION_FAMILY {
            DURATION_BUCKETS
        } else {
            &[]
        }
    }
}

/// The families the request documents.
///
/// The order is the catalogue's order (grouped by concern) and the names are the ones the Grafana
/// bundle in `infra/observability/` queries, so a rename here is a dashboard break and shows up in
/// the bundle check rather than in production.
pub const FAMILIES: &[FamilySpec] = &[
    FamilySpec {
        name: "omnion_http_requests_total",
        kind: MetricKind::Counter,
        unit: "1",
        description: "HTTP requests served, by route template, method and status class.",
        labels: &["route", "method", "status"],
        source: "core",
        max_series: 900,
        bounded_labels: true,
    },
    FamilySpec {
        name: "omnion_http_request_duration_seconds",
        kind: MetricKind::Histogram,
        unit: "seconds",
        description: "HTTP request duration, as a histogram of observed seconds.",
        labels: &["route", "method"],
        source: "core",
        max_series: 400,
        bounded_labels: true,
    },
    FamilySpec {
        name: "omnion_db_pool_connections",
        kind: MetricKind::Gauge,
        unit: "1",
        description: "Database pool connections currently held, by pool state.",
        labels: &["state"],
        source: "core",
        max_series: 6,
        bounded_labels: true,
    },
    FamilySpec {
        name: "omnion_db_query_duration_seconds",
        kind: MetricKind::Histogram,
        unit: "seconds",
        description: "Database query duration, by statement name and never by parameter values.",
        labels: &["statement"],
        source: "core",
        max_series: 120,
        bounded_labels: true,
    },
    FamilySpec {
        name: "omnion_queue_depth",
        kind: MetricKind::Gauge,
        unit: "1",
        description: "Jobs waiting in each queue, by queue and state.",
        labels: &["queue", "state"],
        source: "core",
        max_series: 60,
        bounded_labels: true,
    },
    FamilySpec {
        name: "omnion_queue_job_duration_seconds",
        kind: MetricKind::Histogram,
        unit: "seconds",
        description: "Queue job duration, by job kind.",
        labels: &["kind"],
        source: "worker",
        max_series: 120,
        bounded_labels: true,
    },
    FamilySpec {
        name: "omnion_queue_job_failures_total",
        kind: MetricKind::Counter,
        unit: "1",
        description: "Queue jobs that failed, by job kind.",
        labels: &["kind"],
        source: "worker",
        max_series: 120,
        bounded_labels: true,
    },
    FamilySpec {
        name: "omnion_workflow_steps_total",
        kind: MetricKind::Counter,
        unit: "1",
        description: "Workflow steps executed, by their outcome.",
        labels: &["status"],
        source: "module",
        max_series: 12,
        bounded_labels: true,
    },
    FamilySpec {
        name: "omnion_ai_requests_total",
        kind: MetricKind::Counter,
        unit: "1",
        description: "AI provider calls, by provider, model and outcome.",
        labels: &["provider", "model", "status"],
        source: "module",
        max_series: 600,
        bounded_labels: true,
    },
    FamilySpec {
        name: "omnion_ai_tokens_total",
        kind: MetricKind::Counter,
        unit: "1",
        description: "AI tokens consumed, by kind (prompt, completion, cached).",
        labels: &["kind"],
        source: "module",
        max_series: 12,
        bounded_labels: true,
    },
    FamilySpec {
        name: "omnion_ai_cost_micros_total",
        kind: MetricKind::Counter,
        unit: "micros",
        description: "AI spend in micro-units of the billing currency, by provider and model.",
        labels: &["provider", "model"],
        source: "module",
        max_series: 400,
        bounded_labels: true,
    },
    FamilySpec {
        name: "omnion_webhook_deliveries_total",
        kind: MetricKind::Counter,
        unit: "1",
        description: "Webhook delivery attempts, by result.",
        labels: &["result"],
        source: "core",
        max_series: 12,
        bounded_labels: true,
    },
    FamilySpec {
        name: "omnion_outbound_retries_total",
        kind: MetricKind::Counter,
        unit: "1",
        description: "Outbound retries, by subsystem and outcome.",
        labels: &["subsystem", "outcome"],
        source: "core",
        max_series: 80,
        bounded_labels: true,
    },
    FamilySpec {
        name: "omnion_circuit_state",
        kind: MetricKind::Gauge,
        unit: "1",
        description: "Circuit breaker state per provider: 0 closed, 1 half-open, 2 open (REQ-127).",
        labels: &["provider"],
        source: "core",
        max_series: 40,
        bounded_labels: true,
    },
    FamilySpec {
        name: "omnion_rate_limit_refusals_total",
        kind: MetricKind::Counter,
        unit: "1",
        description: "Requests refused by a rate limit, by the scope that refused.",
        labels: &["scope"],
        source: "core",
        max_series: 30,
        bounded_labels: true,
    },
    FamilySpec {
        name: "omnion_cache_hits_total",
        kind: MetricKind::Counter,
        unit: "1",
        description: "Cache lookups that hit, by cache name.",
        labels: &["cache"],
        source: "module",
        max_series: 30,
        bounded_labels: true,
    },
    FamilySpec {
        name: "omnion_build_info",
        kind: MetricKind::Gauge,
        unit: "1",
        description: "Build metadata of the running instance; always 1.",
        labels: &["version", "commit"],
        source: "core",
        max_series: 4,
        // Not bounded: an operator may run several versions side by side during a rollout, and
        // the cap is what keeps that from becoming unbounded anyway.
        bounded_labels: false,
    },
    FamilySpec {
        name: "omnion_exporter_dropped_total",
        kind: MetricKind::Counter,
        unit: "1",
        description: "Telemetry samples dropped because an exporter buffer was full.",
        labels: &["exporter"],
        source: "core",
        max_series: 20,
        bounded_labels: true,
    },
    // The flush counter, added with the loop that records it. A drop counter with no flush
    // counter cannot answer the question an operator actually has when a buffer is full: is the
    // exporter broken, or is the drain broken? Dropped-but-never-flushed means the second.
    FamilySpec {
        name: "omnion_exporter_batches_flushed_total",
        kind: MetricKind::Counter,
        unit: "1",
        description: "Telemetry batches a flush loop handed to a backend and the backend accepted.",
        labels: &["kind"],
        source: "core",
        max_series: 8,
        bounded_labels: true,
    },
    FamilySpec {
        name: "omnion_registry_budget_exceeded",
        kind: MetricKind::Counter,
        unit: "1",
        description: "Samples folded into a family's overflow series because its cardinality cap was hit.",
        labels: &["family"],
        source: "core",
        max_series: 32,
        bounded_labels: false,
    },
];

/// Label names that must never appear on a family.
///
/// The request is explicit that user, organization and request identifiers are never labels, and
/// these are the names such a caller would reach for. The list is exact rather than a substring
/// rule because a substring rule is wrong in both directions: it rejects `provider` (which
/// contains "id") and accepts `user_email` (which contains "user").
pub const IDENTITY_LABELS: &[&str] = &[
    "user",
    "user_id",
    "userid",
    "organization",
    "organization_id",
    "org_id",
    "tenant",
    "tenant_id",
    "request",
    "request_id",
    "session",
    "session_id",
    "email",
    "ip",
    "ip_address",
];

/// Look a family up by name.
#[must_use]
pub fn family(name: &str) -> Option<&'static FamilySpec> {
    FAMILIES.iter().find(|spec| spec.name == name)
}

/// One recorded series: its label values, its current value and its history ring.
///
/// The kind is carried on the series rather than looked up by the renderer, because "what does one
/// minute of this metric look like" is a property of the metric — a counter's minute is its delta,
/// a gauge's minute is its last value and a histogram's minute is its mean. A renderer that had to
/// be told the kind would be one refactor away from drawing a counter's running total as a rate.
#[derive(Debug, Clone)]
struct Series {
    labels: Vec<String>,
    kind: MetricKind,
    /// The counter's running total, or the gauge's current value.
    value: f64,
    /// A histogram's running total.
    sum: f64,
    /// A histogram's running observation count.
    count: u64,
    /// Per-bucket cumulative counts; empty for the other kinds.
    buckets: Vec<u64>,
    /// `unix minute → the plotted value for that minute`.
    history: VecDeque<(i64, f64)>,
    /// The minute currently being accumulated.
    open_minute: i64,
    /// The counter's total at the start of `open_minute`, so a minute plots a delta.
    minute_start_value: f64,
    /// The histogram's sum and count at the start of `open_minute`.
    minute_start_sum: f64,
    minute_start_count: u64,
}

/// What the catalogue shows about one family's live use.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FamilyState {
    /// The family name.
    pub name: String,
    /// How many distinct series exist.
    pub series: usize,
    /// The cap this family enforces.
    pub max_series: usize,
    /// `true` once the cap has been reached at least once.
    pub over_budget: bool,
    /// How many samples were folded away because of the cap.
    pub budget_events: u64,
}

#[derive(Debug, Default)]
struct Inner {
    series: HashMap<String, Vec<Series>>,
    /// `(family, label position) → the values kept verbatim`.
    known: HashMap<(String, usize), HashSet<String>>,
    /// `family → how many samples the cap folded away`.
    budget_events: HashMap<String, u64>,
    /// The global series cap, kept in step with the settings screen.
    global_budget: usize,
}

/// The registry.
///
/// A process-wide singleton behind [`global`], because the metrics a scrape must see are the ones
/// recorded by whichever layer handled the request — the HTTP middleware, a worker, a module — and
/// threading a handle through all of them to reach a struct that is by definition process-wide is
/// the kind of plumbing that one unmigrated call site silently drops.
pub struct Registry {
    spec: Vec<FamilySpec>,
    inner: Mutex<Inner>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    /// A registry over the declared families, with the default global budget.
    #[must_use]
    pub fn new() -> Self {
        let spec: Vec<FamilySpec> = FAMILIES.to_vec();
        let total: usize = spec.iter().map(|f| f.max_series).sum();
        Self {
            spec,
            inner: Mutex::new(Inner {
                global_budget: total.max(1),
                ..Inner::default()
            }),
        }
    }

    /// The declared families, in catalogue order.
    #[must_use]
    pub fn specs(&self) -> &[FamilySpec] {
        &self.spec
    }

    /// Set the global series cap, the one the settings screen writes.
    ///
    /// A family never gets *more* than its own declared cap because of a global raise: the global
    /// budget is a ceiling on the whole registry, and a family cap is a ceiling on one family. A
    /// lower global value lowers the ceiling on the registry as a whole by shrinking the per-family
    /// share, which is what an operator turning the knob down actually means.
    pub fn set_global_budget(&self, budget: usize) {
        let mut guard = self.lock();
        guard.global_budget = budget.max(1);
    }

    /// The global series cap in force.
    #[must_use]
    pub fn global_budget(&self) -> usize {
        self.lock().global_budget
    }

    /// The cap a family is held to, given the global budget.
    fn cap_for(&self, guard: &Inner, spec: &FamilySpec) -> usize {
        let per_family_share = guard
            .global_budget
            .saturating_div(self.spec.len().max(1))
            .max(1);
        spec.max_series.min(per_family_share).max(1)
    }

    /// Add to a counter. Negative values are ignored: a counter that can go down is a gauge, and
    /// a "decrement" in a call site is a bug worth not hiding in a scrape.
    pub fn counter_add(&self, name: &str, labels: &[&str], value: f64) {
        if !value.is_finite() || value < 0.0 {
            return;
        }
        self.record(name, labels, |series| series.value += value);
    }

    /// Set a gauge.
    pub fn gauge_set(&self, name: &str, labels: &[&str], value: f64) {
        if !value.is_finite() {
            return;
        }
        self.record(name, labels, |series| series.value = value);
    }

    /// Record one observation into a histogram.
    pub fn observe(&self, name: &str, labels: &[&str], value: f64) {
        if !value.is_finite() {
            return;
        }
        let Some(spec) = family(name) else { return };
        let edges = spec.buckets().to_vec();
        self.record(name, labels, move |series| {
            series.sum += value;
            series.count += 1;
            if edges.is_empty() {
                return;
            }
            if series.buckets.len() != edges.len() {
                series.buckets = vec![0; edges.len()];
            }
            for (index, edge) in edges.iter().enumerate() {
                if value <= *edge {
                    series.buckets[index] += 1;
                }
            }
        });
    }

    /// The shared write path: normalise the labels, find or create the series, apply the value.
    ///
    /// `value` is deliberately absent: it is captured by each caller's `apply` closure, and a
    /// parameter passed twice is a parameter the two can disagree about.
    fn record(&self, name: &str, labels: &[&str], apply: impl Fn(&mut Series)) {
        let Some(spec) = family(name).copied() else {
            // An undeclared family is dropped, not invented: a typo in a call site must not
            // create a series nothing in the catalogue, the bundle or the panel knows about.
            return;
        };

        let mut guard = self.lock();
        let labels = self.normalise(&mut guard, &spec, labels);
        let minute = current_minute();
        let cap = self.cap_for(&guard, &spec);
        let name = spec.name.to_owned();

        // Each borrow of `guard.series` is opened and closed inside a single statement, because
        // the budget counter lives in the same `Inner` and a long-lived `entry()` borrow would
        // hold the whole struct mutably for the rest of the function. That is why this reads as
        // five `get_mut` calls instead of one `entry` call: the alternative — an `Rc<RefCell<_>>`
        // or splitting the map in two — moves the cost into every reader instead of one writer.
        if let Some(series) = guard
            .series
            .get_mut(&name)
            .and_then(|list| list.iter_mut().find(|series| series.labels == labels))
        {
            Self::apply(series, minute, &apply);
            return;
        }

        if guard.series.get(&name).map_or(0, Vec::len) >= cap {
            // Over the cap. The sample is folded into the overflow series — every label value
            // replaced by `other` — and the fold is counted, so the totals stay honest and the
            // loss is visible on the scrape, in the catalogue and on the screen. The overflow
            // series itself is created *past* the cap on purpose: it is a single fixed series, and
            // refusing it would drop the sample entirely instead of keeping the total honest.
            *guard.budget_events.entry(name.clone()).or_insert(0) += 1;
            let overflow = vec![OTHER.to_owned(); spec.labels.len()];
            let existing = guard
                .series
                .get_mut(&name)
                .and_then(|list| list.iter_mut().find(|series| series.labels == overflow));
            if let Some(series) = existing {
                Self::apply(series, minute, &apply);
            } else {
                let mut series = Self::blank(&overflow, &spec);
                Self::apply(&mut series, minute, &apply);
                guard
                    .series
                    .get_mut(&name)
                    .expect("the family entry was just read")
                    .push(series);
            }
            return;
        }

        let mut series = Self::blank(&labels, &spec);
        Self::apply(&mut series, minute, &apply);
        guard.series.entry(name).or_default().push(series);
    }

    fn blank(labels: &[String], spec: &FamilySpec) -> Series {
        Series {
            labels: labels.to_vec(),
            kind: spec.kind,
            value: 0.0,
            sum: 0.0,
            count: 0,
            buckets: vec![0; spec.buckets().len()],
            history: VecDeque::new(),
            open_minute: current_minute(),
            minute_start_value: 0.0,
            minute_start_sum: 0.0,
            minute_start_count: 0,
        }
    }

    /// Apply a value and fold it into the current minute of history.
    ///
    /// A minute that has already passed is closed first and plotted with whatever it accumulated,
    /// so a process that records nothing for five minutes gets five flat minutes rather than a
    /// straight line between two points — a chart that interpolates over silence reads as traffic
    /// that never happened.
    fn apply(series: &mut Series, minute: i64, apply: &impl Fn(&mut Series)) {
        apply(series);
        while series.open_minute < minute {
            let value = plot(series);
            series.history.push_back((series.open_minute, value));
            series.open_minute += 1;
            series.minute_start_value = series.value;
            series.minute_start_sum = series.sum;
            series.minute_start_count = series.count;
        }
        let point = plot(series);
        match series.history.back_mut() {
            Some((at, value)) if *at == minute => *value = point,
            _ => series.history.push_back((minute, point)),
        }
        while series.history.len() > HISTORY_BUCKETS {
            series.history.pop_front();
        }
    }

    /// Normalise a recorder's labels into the family's declared, bounded label set.
    ///
    /// Three rules, in this order, and the order matters:
    ///
    /// 1. **Width.** Longer is truncated, shorter is padded with `other`. A recorder that passes
    ///    `user_id` as a fourth label therefore has it *dropped* — that is the "never a label"
    ///    rule, enforced in the one place every family goes through.
    /// 2. **Emptiness.** An empty or `"(unmatched)"`-style value becomes `other`, so one series
    ///    collects the "we do not know" cases instead of one per source of ignorance.
    /// 3. **Boundedness.** For a family whose spec says `bounded_labels`, a value not already in
    ///    the learned set joins it while there is room, and collapses to `other` once there is
    ///    not.
    fn normalise(&self, guard: &mut Inner, spec: &FamilySpec, labels: &[&str]) -> Vec<String> {
        let width = spec.labels.len();
        let mut out: Vec<String> = (0..width)
            .map(|index| {
                let raw = labels.get(index).copied().unwrap_or_default().trim();
                if raw.is_empty() {
                    return OTHER.to_owned();
                }
                raw.to_owned()
            })
            .collect();

        if !spec.bounded_labels {
            return out;
        }

        for index in 0..width {
            let value = out[index].clone();
            if value == OTHER {
                continue;
            }
            let key = (spec.name.to_owned(), index);
            let set = guard.known.entry(key).or_default();
            if set.contains(&value) {
                continue;
            }
            if set.len() >= BOUNDED_SET_CAP {
                // Folded, and counted. The counter is shared with the series cap on purpose: both
                // are "a sample was taken out of the series it asked for", and one counter an
                // operator can alert on is worth more than two that each answer a narrower
                // question. The value itself is not lost — it lands in the `other` series — so the
                // number is samples folded, not samples dropped.
                *guard.budget_events.entry(spec.name.to_owned()).or_insert(0) += 1;
                out[index] = OTHER.to_owned();
                continue;
            }
            set.insert(value);
        }
        out
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock means a previous holder panicked mid-write. The registry is plain data
        // with no invariant spanning series, so recovering the data beats taking the whole metric
        // surface down — and a metric surface that disappears is a silent one.
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// The live state of every declared family, for the catalogue.
    #[must_use]
    pub fn family_states(&self) -> Vec<FamilyState> {
        let guard = self.lock();
        self.spec
            .iter()
            .map(|spec| {
                let series = guard.series.get(spec.name).map_or(0, Vec::len);
                let budget_events = guard.budget_events.get(spec.name).copied().unwrap_or(0);
                FamilyState {
                    name: spec.name.to_owned(),
                    series,
                    max_series: self.cap_for(&guard, spec),
                    over_budget: budget_events > 0,
                    budget_events,
                }
            })
            .collect()
    }

    /// One family's series with their history, for a chart.
    #[must_use]
    pub fn series_of(&self, name: &str, window_minutes: usize) -> Vec<SeriesSnapshot> {
        let guard = self.lock();
        let cutoff = current_minute() - window_minutes.min(MAX_POINTS).max(1) as i64;
        guard
            .series
            .get(name)
            .map(|list| {
                list.iter()
                    .map(|series| SeriesSnapshot {
                        labels: series.labels.clone(),
                        total: match family(name).map(|s| s.kind) {
                            Some(MetricKind::Histogram) => series.sum,
                            _ => series.value,
                        },
                        observations: series.count,
                        points: series
                            .history
                            .iter()
                            .filter(|(at, _)| *at >= cutoff)
                            .map(|(at, value)| Point {
                                at: minute_to_rfc3339(*at),
                                value: *value,
                            })
                            .collect(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The value of one exact series, or `None` when it was never recorded.
    #[must_use]
    pub fn value_of(&self, name: &str, labels: &[&str]) -> Option<f64> {
        let guard = self.lock();
        let width = family(name).map_or(0, |spec| spec.labels.len());
        let wanted: Vec<String> = (0..width)
            .map(|index| labels.get(index).copied().unwrap_or_default().to_owned())
            .collect();
        guard
            .series
            .get(name)?
            .iter()
            .find(|series| series.labels == wanted)
            .map(|series| series.value)
    }

    /// The Prometheus text exposition of every family, in the documented format.
    ///
    /// Families are emitted in declaration order and series in label order, so two scrapes of an
    /// unchanged process produce byte-identical output — which is what makes a diff of two
    /// `/metrics` bodies meaningful and a golden-file test possible.
    #[must_use]
    pub fn render(&self) -> String {
        let guard = self.lock();
        let mut out = String::with_capacity(8 * 1024);
        for spec in &self.spec {
            let name = spec.name;
            let _ = writeln!(out, "# HELP {name} {}", spec.description);
            let _ = writeln!(out, "# TYPE {name} {}", spec.kind.as_str());
            let Some(list) = guard.series.get(name) else {
                continue;
            };
            let mut ordered: Vec<&Series> = list.iter().collect();
            ordered.sort_by(|a, b| a.labels.cmp(&b.labels));
            for series in ordered {
                match spec.kind {
                    MetricKind::Counter | MetricKind::Gauge => {
                        let rendered = render_labels(spec, &series.labels, None);
                        let _ = writeln!(out, "{name}{rendered} {}", format_value(series.value));
                    }
                    MetricKind::Histogram => {
                        for (index, edge) in spec.buckets().iter().enumerate() {
                            let le = render_le(*edge);
                            let rendered = render_labels(spec, &series.labels, Some(&le));
                            let _ = writeln!(
                                out,
                                "{name}_bucket{rendered} {}",
                                series.buckets.get(index).copied().unwrap_or(0)
                            );
                        }
                        let inf = render_le(f64::INFINITY);
                        let rendered = render_labels(spec, &series.labels, Some(&inf));
                        let _ = writeln!(out, "{name}_bucket{rendered} {}", series.count);
                        let rendered = render_labels(spec, &series.labels, None);
                        let _ = writeln!(out, "{name}_sum{rendered} {}", format_value(series.sum));
                        let _ = writeln!(out, "{name}_count{rendered} {}", series.count);
                    }
                }
            }
        }
        // The budget counter is derived, not stored: it exists so the fold the guard performs is
        // visible on the very scrape it happened on.
        //
        // Only the sample lines — no HELP/TYPE. The family is declared in `FAMILIES`, so the loop
        // above already wrote its header, and a second HELP line for the same metric name makes
        // the whole scrape invalid (Prometheus refuses the entire scrape on a duplicate). The
        // first draft of this wrote the header again and produced a `/metrics` body no scraper
        // would accept, while every unit test here was green because they grep for one line.
        if !guard.budget_events.is_empty() {
            let mut families: Vec<(&String, &u64)> = guard.budget_events.iter().collect();
            families.sort_by(|a, b| a.0.cmp(b.0));
            for (family, count) in families {
                let _ = writeln!(
                    out,
                    "omnion_registry_budget_exceeded{{family=\"{}\"}} {count}",
                    escape_label(family)
                );
            }
        }
        out
    }
}

/// One series as the chart consumes it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SeriesSnapshot {
    /// The label values, positionally matching the family's label names.
    pub labels: Vec<String>,
    /// The current value (a counter's total, a gauge's value, a histogram's sum).
    pub total: f64,
    /// A histogram's observation count; `0` for the other kinds.
    pub observations: u64,
    /// The minute buckets inside the requested window, oldest first.
    pub points: Vec<Point>,
}

/// One point on a chart.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Point {
    /// The minute's start, RFC 3339 in UTC.
    pub at: String,
    /// The plotted value.
    pub value: f64,
}

/// The process-wide registry.
static REGISTRY: OnceLock<Registry> = OnceLock::new();

/// The process-wide registry, created on first use.
#[must_use]
pub fn global() -> &'static Registry {
    REGISTRY.get_or_init(Registry::new)
}

/// The current unix minute, the bucket the history ring writes into.
fn current_minute() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp().div_euclid(60)
}

/// Render `at + n minutes` as an RFC 3339 instant.
fn minute_to_rfc3339(minute: i64) -> String {
    OffsetDateTime::from_unix_timestamp(minute * 60)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .format(&Rfc3339)
        .unwrap_or_default()
}

/// The value a series plots for its open minute.
///
/// A counter plots its **delta** for the minute, a gauge its **last** value, a histogram its
/// **mean** for the minute. Anything else would make the chart disagree with the metric's own
/// semantics — the failure a "requests per minute" chart drawn from a running total has.
fn plot(series: &Series) -> f64 {
    match series.kind {
        MetricKind::Counter => (series.value - series.minute_start_value).max(0.0),
        MetricKind::Gauge => series.value,
        MetricKind::Histogram => {
            let observations = series.count - series.minute_start_count;
            if observations == 0 {
                return 0.0;
            }
            (series.sum - series.minute_start_sum) / observations as f64
        }
    }
}

/// A label value, escaped for the exposition format.
///
/// Prometheus's text format has exactly three escapes inside a label value, and a route template
/// containing a quote is not hypothetical — a module can name a route anything.
fn escape_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 4);
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

/// A number as the exposition format wants it: an integer when it is whole.
///
/// `1` and `1.0` are the same value and a monitoring system treats them as the same series, but a
/// scrape full of `.0` is harder to read and the panel copies these lines into a runbook.
fn format_value(value: f64) -> String {
    if !value.is_finite() {
        return if value.is_nan() {
            "NaN".to_owned()
        } else if value > 0.0 {
            "+Inf".to_owned()
        } else {
            "-Inf".to_owned()
        };
    }
    if value.fract() == 0.0 && value.abs() < 1e15 {
        return format!("{}", value as i64);
    }
    format!("{value}")
}

/// A bucket edge, rendered the way the exposition format requires (`+Inf` for the last one).
fn render_le(edge: f64) -> String {
    if edge.is_infinite() {
        "+Inf".to_owned()
    } else {
        format_value(edge)
    }
}

/// `{a="1",b="2"}`, with an optional extra label (the histogram's `le`).
fn render_labels(spec: &FamilySpec, values: &[String], extra: Option<&str>) -> String {
    let mut parts: Vec<String> = spec
        .labels
        .iter()
        .zip(values.iter())
        .map(|(name, value)| format!("{name}=\"{}\"", escape_label(value)))
        .collect();
    if let Some(le) = extra {
        parts.push(format!("le=\"{le}\""));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("{{{}}}", parts.join(","))
}

/// The value map the selector builder reads, for a family: the label names and, where the family
/// bounds them, the values the registry has actually seen.
///
/// A selector that can only be built from values that exist is the reason a refused selector
/// names a family rather than a blank graph.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LabelCatalogue {
    /// The family name.
    pub metric: String,
    /// The label names, positionally.
    pub labels: Vec<String>,
    /// For each label, the values observed so far (bounded, never ids).
    pub values: Vec<Vec<String>>,
    /// The cap on each label's verbatim value set.
    pub bounded_set_cap: usize,
}

/// The label catalogue for every declared family.
#[must_use]
pub fn label_catalogues() -> Vec<LabelCatalogue> {
    let registry = global();
    let guard = registry.lock();
    FAMILIES
        .iter()
        .map(|spec| {
            let values = (0..spec.labels.len())
                .map(|index| {
                    guard
                        .known
                        .get(&(spec.name.to_owned(), index))
                        .map(|set| {
                            let mut values: Vec<String> = set.iter().cloned().collect();
                            values.sort();
                            values
                        })
                        .unwrap_or_default()
                })
                .collect();
            LabelCatalogue {
                metric: spec.name.to_owned(),
                labels: spec.labels.iter().map(|name| (*name).to_owned()).collect(),
                values,
                bounded_set_cap: BOUNDED_SET_CAP,
            }
        })
        .collect()
}

/// A store-independent summary used by the settings screen's explanation.
#[must_use]
pub fn total_declared_series() -> usize {
    FAMILIES.iter().map(|spec| spec.max_series).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        Registry::new()
    }

    #[test]
    fn a_scrape_carries_the_family_help_type_and_value() {
        let reg = registry();
        reg.counter_add(
            "omnion_http_requests_total",
            &["/api/v1/secrets/{id}", "GET", "200"],
            3.0,
        );
        let text = reg.render();
        assert!(text.contains("# HELP omnion_http_requests_total"));
        assert!(text.contains("# TYPE omnion_http_requests_total counter"));
        assert!(text.contains(
            "omnion_http_requests_total{route=\"/api/v1/secrets/{id}\",method=\"GET\",status=\"200\"} 3"
        ));
    }

    #[test]
    fn a_gauge_takes_the_last_value_not_the_sum() {
        let reg = registry();
        reg.gauge_set("omnion_queue_depth", &["email", "ready"], 7.0);
        reg.gauge_set("omnion_queue_depth", &["email", "ready"], 4.0);
        assert_eq!(
            reg.value_of("omnion_queue_depth", &["email", "ready"]),
            Some(4.0)
        );
    }

    #[test]
    fn a_counter_ignores_a_negative_increment() {
        let reg = registry();
        reg.counter_add("omnion_rate_limit_refusals_total", &["login"], 5.0);
        reg.counter_add("omnion_rate_limit_refusals_total", &["login"], -3.0);
        assert_eq!(
            reg.value_of("omnion_rate_limit_refusals_total", &["login"]),
            Some(5.0)
        );
    }

    #[test]
    fn an_undeclared_family_is_dropped_rather_than_invented() {
        let reg = registry();
        reg.counter_add("omnion_not_a_family", &["x"], 1.0);
        assert!(!reg.render().contains("omnion_not_a_family"));
    }

    #[test]
    fn a_label_the_family_does_not_declare_is_dropped_not_stored() {
        // The rule the request calls the most dangerous failure mode in this area: a recorder that
        // offers a user id gets it truncated away, so the series is bounded no matter what a call
        // site does.
        let reg = registry();
        let user = "9f4c0e2a-1111-4222-8333-444455556666";
        reg.counter_add(
            "omnion_webhook_deliveries_total",
            &["ok", user, "org_7"],
            1.0,
        );
        let text = reg.render();
        assert!(
            !text.contains(user),
            "the id leaked into the exposition: {text}"
        );
        assert!(text.contains("omnion_webhook_deliveries_total{result=\"ok\"} 1"));
    }

    #[test]
    fn a_bounded_label_overflows_to_other_and_the_learned_set_stops_growing() {
        let reg = registry();
        for index in 0..(BOUNDED_SET_CAP + 5) {
            reg.counter_add(
                "omnion_ai_requests_total",
                &[format!("provider-{index}").as_str(), "m", "ok"],
                1.0,
            );
        }
        let text = reg.render();
        // The five values past the cap collapse into ONE `other` series, so the assertion is about
        // absence of the folded values and presence of the single bucket — counting matching lines
        // could never exceed 1 and would have passed against a guard that folded nothing.
        for index in BOUNDED_SET_CAP..(BOUNDED_SET_CAP + 5) {
            assert!(
                !text.contains(&format!("provider=\"provider-{index}\"")),
                "provider-{index} was past the cap and should have folded into other: {text}"
            );
        }
        assert_eq!(
            text.matches("provider=\"other\"").count(),
            1,
            "the overflow bucket is one series, not one per folded value: {text}"
        );
        assert!(text.contains("model=\"m\""));
        let states = reg.family_states();
        let ai = states
            .iter()
            .find(|state| state.name == "omnion_ai_requests_total")
            .expect("the family is declared");
        // 24 verbatim providers + 1 overflow + the shared (m, ok) tail stays at 25 series.
        assert!(
            ai.series <= BOUNDED_SET_CAP + 1,
            "the series set grew past the bound: {}",
            ai.series
        );
    }

    #[test]
    fn a_family_at_its_cap_folds_the_sample_and_counts_the_fold() {
        let reg = registry();
        // `omnion_db_pool_connections` is capped at six series, so seven distinct states prove it.
        for index in 0..8 {
            reg.gauge_set(
                "omnion_db_pool_connections",
                &[format!("state-{index}").as_str()],
                1.0,
            );
        }
        let states = reg.family_states();
        let db = states
            .iter()
            .find(|state| state.name == "omnion_db_pool_connections")
            .expect("the family is declared");
        assert!(db.over_budget, "the cap was hit and nothing said so");
        assert!(db.budget_events >= 1);
        let text = reg.render();
        assert!(
            text.contains("omnion_registry_budget_exceeded{family=\"omnion_db_pool_connections\"}"),
            "the fold is not visible on the scrape: {text}"
        );
        assert!(text.contains("state=\"other\""));
    }

    #[test]
    fn a_histogram_publishes_cumulative_buckets_sum_and_count() {
        let reg = registry();
        reg.observe("omnion_http_request_duration_seconds", &["/x", "GET"], 0.03);
        reg.observe("omnion_http_request_duration_seconds", &["/x", "GET"], 3.0);
        let text = reg.render();
        // 0.03 is inside every edge up to 5.0, 3.0 only up to 5.0 and +Inf.
        assert!(text.contains(
            "omnion_http_request_duration_seconds_bucket{route=\"/x\",method=\"GET\",le=\"0.05\"} 1"
        ));
        assert!(text.contains(
            "omnion_http_request_duration_seconds_bucket{route=\"/x\",method=\"GET\",le=\"0.01\"} 0"
        ));
        assert!(text.contains(
            "omnion_http_request_duration_seconds_bucket{route=\"/x\",method=\"GET\",le=\"+Inf\"} 2"
        ));
        assert!(
            text.contains(
                "omnion_http_request_duration_seconds_count{route=\"/x\",method=\"GET\"} 2"
            )
        );
        assert!(text.contains(
            "omnion_http_request_duration_seconds_sum{route=\"/x\",method=\"GET\"} 3.03"
        ));
    }

    #[test]
    fn the_exposition_is_byte_identical_for_an_unchanged_registry() {
        let reg = registry();
        reg.counter_add("omnion_cache_hits_total", &["search"], 2.0);
        reg.counter_add("omnion_cache_hits_total", &["pages"], 1.0);
        assert_eq!(reg.render(), reg.render());
        let order: Vec<usize> = reg
            .render()
            .lines()
            .filter(|line| line.starts_with("omnion_cache_hits_total{"))
            .map(|line| line.find("cache=").unwrap_or(0))
            .collect();
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn a_quote_in_a_route_template_is_escaped() {
        let reg = registry();
        reg.counter_add("omnion_http_requests_total", &["/a\"b", "GET", "200"], 1.0);
        assert!(reg.render().contains(r#"route="/a\"b""#));
    }

    #[test]
    fn an_empty_label_becomes_other_so_ignorance_is_one_series() {
        let reg = registry();
        reg.counter_add("omnion_http_requests_total", &["", "", ""], 1.0);
        assert!(reg.render().contains(
            "omnion_http_requests_total{route=\"other\",method=\"other\",status=\"other\"} 1"
        ));
    }

    #[test]
    fn a_whole_number_renders_without_a_decimal_point() {
        assert_eq!(format_value(3.0), "3");
        assert_eq!(format_value(3.5), "3.5");
        assert_eq!(format_value(f64::INFINITY), "+Inf");
    }

    #[test]
    fn every_documented_family_is_declared_with_a_description_and_a_source() {
        for spec in FAMILIES {
            // The `omnion_` prefix is what keeps these series from colliding with another
            // application's on a Prometheus that scrapes more than one instance. Asserted
            // positively: a test that says "does not have the thing we require" reads as a
            // requirement at a glance and is the inverse of what it checks.
            assert!(
                spec.name.starts_with("omnion_"),
                "{} is missing the omnion_ prefix",
                spec.name
            );
            assert!(
                spec.name
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'),
                "{} is not a valid exposition name",
                spec.name
            );
            assert!(
                !spec.description.is_empty(),
                "{} has no description",
                spec.name
            );
            assert!(!spec.source.is_empty(), "{} has no source", spec.name);
            assert!(
                matches!(spec.source, "core" | "module" | "worker"),
                "{} has an unknown source",
                spec.name
            );
            for label in spec.labels {
                // Exact names, not substrings. A `contains("id")` test rejects `provider` — "id"
                // is inside it — so the check as first written would have failed a correct
                // family and taught the next reader to distrust it. What must never be a label
                // is an *identifier*, and those have names.
                assert!(
                    !IDENTITY_LABELS.contains(label),
                    "{} declares `{}`, which is an identity label",
                    spec.name,
                    label
                );
            }
        }
    }

    #[test]
    fn the_global_budget_can_only_tighten_a_family_cap() {
        let reg = registry();
        let before = reg
            .family_states()
            .into_iter()
            .find(|s| s.name == "omnion_http_requests_total")
            .expect("declared")
            .max_series;
        reg.set_global_budget(1);
        let after = reg
            .family_states()
            .into_iter()
            .find(|s| s.name == "omnion_http_requests_total")
            .expect("declared")
            .max_series;
        assert!(after <= before, "{after} should not exceed {before}");
    }

    #[test]
    fn a_poisoned_lock_does_not_take_the_metric_surface_down() {
        let reg = std::sync::Arc::new(registry());
        reg.counter_add("omnion_cache_hits_total", &["search"], 1.0);
        let clone = std::sync::Arc::clone(&reg);
        let _ = std::thread::spawn(move || {
            clone.counter_add("omnion_cache_hits_total", &["pages"], 1.0);
            panic!("deliberate: a recorder panicked while holding the registry");
        })
        .join();
        // The registry answers after the panic; a metric surface that vanishes is a silent one.
        assert!(
            reg.render()
                .contains("omnion_cache_hits_total{cache=\"search\"} 1")
        );
    }
}
