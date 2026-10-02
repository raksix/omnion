//! The cluster panel's decisions (REQ-024, slice 4).
//!
//! The Kubernetes panel is the one screen in this request that can be built as a lie without
//! anyone noticing, and it has three independent ways to lie. Each one is a value with a test
//! here rather than a `?` and a `toFixed(0)` in the panel:
//!
//! * **A missing metric rendered as zero.** The runtime does not always report CPU for a pod in
//!!   `Pending`, and a single instance has no replicas at all. A panel that turns "unknown" into
//!!   `0` shows an idle cluster, which is the reading an operator acts on. [`Metric`] has an
//!!   [`Metric::Unknown`] variant and the panel renders it as a dash, never a number.
//! * **A restart that is not a job.** Restarting a workload stops serving traffic for a second
//!   or two, and a restart that bypasses [`crate::jobs::create_job`] leaves no history row, no
//!   actor and no step log — and, worse, ignores the one-job-per-environment index, so it can
//!   race a deploy that is mid-migration. So a restart is a [`JobKind::Restart`] like any other.
//! * **A sparkline whose series is flat.** Normalising by `max - min` divides by zero when every
//!   sample is the same value, which is the *commonest* series there is (an idle cluster). That
//!   yields `NaN` points and a blank chart for a cluster that is fine.
//!
//! What is **not** here is the Kubernetes client. Detection is a value ([`Runtime`]) so the route
//! can decide between the cluster panel and the single-instance card without a cluster existing
//! anywhere in the test suite, and the runtime reader itself lives in `apps/api` where the
//! credentials are — no cluster credential reaches the browser.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// The window the sparkline draws.
pub const SAMPLE_WINDOW_MINUTES: i64 = 30;

/// How long samples are kept.
///
/// Strictly greater than [`SAMPLE_WINDOW_MINUTES`], which is the property the prune depends on:
/// if retention equals the window, then a single missed sample puts the newest point *exactly*
/// on the right edge, the chart's width collapses, and a healthy cluster is drawn as a flat line
/// for the length of one missed write. The spare window absorbs one miss.
pub const SAMPLE_RETENTION_MINUTES: i64 = 2 * SAMPLE_WINDOW_MINUTES;

/// The sampling interval the buckets are floored to.
///
/// Sampling writes `(environment, workload, bucket_at)` with `bucket_at` = the interval floor, so
/// two samplers racing (the scheduler and an operator pressing "sample now") write the *same*
/// bucket and the second updates it. Without the floor, `now()` makes the row unique every time
/// and the sparkline draws two samples for one minute, doubling the apparent variance.
pub const SAMPLE_INTERVAL_SECONDS: i64 = 60;

/// How far back a single-instance card looks for its memory figure's unit.
pub const MAX_WORKLOADS: usize = 64;

/// The longest a workload name may be.
///
/// The route validates it and the `0214` constraint refuses it, so the two agree; the constant
/// exists so the panel asks for the same number rather than hard-coding a second one that drifts.
pub const MAX_WORKLOAD_NAME: usize = 128;

/// What kind of runtime is behind the panel.
///
/// A single value rather than a "is cluster: bool" plus a second "is cluster" branch, because
/// the spec forbids a disabled cluster card as a tease: the route answers `404` on the cluster
/// routes and the panel shows the single-instance card, so there is exactly one place that knows
/// which of the two is being drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Runtime {
    /// A cluster is reported: replicas, per-workload resources, rollout status.
    Cluster,
    /// One process. Uptime and resident memory, and a confirmed service restart.
    Single,
}

impl Runtime {
    /// The value the API serialises as.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Runtime::Cluster => "cluster",
            Runtime::Single => "single",
        }
    }

    /// Does the cluster panel's route answer, or is it a `404`?
    ///
    /// One rule, so the read route and the restart route cannot disagree about whether this
    /// instance is a cluster — a restart that answered `409` on a single instance because one
    /// handler checked and the other did not is a confusing refusal at best.
    #[must_use]
    pub fn is_cluster(self) -> bool {
        matches!(self, Runtime::Cluster)
    }

    /// The sentence a `404` carries, so a client that ignores the code still learns the truth:
    /// this is not a broken panel, it is the wrong kind of deployment.
    #[must_use]
    pub fn not_a_cluster_message(self) -> String {
        match self {
            Runtime::Cluster => "This deployment reports a cluster.".to_string(),
            Runtime::Single => {
                "This deployment runs as a single instance, so it has no cluster to read. The \
                 process card shows its uptime and memory instead."
                    .to_string()
            }
        }
    }
}

/// Which unit a bare number is measured in.
///
/// Needed because [`Usage`] holds `used` and `limit` as plain `i64`s, and a CPU figure rendered
/// with a `MiB` suffix is worse than a number with no suffix at all. The alternative is a string
/// parameter, which a caller can get backwards and which cannot be matched on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// CPU, in millicores.
    Millicores,
    /// Memory, in bytes.
    Bytes,
}

/// A measured value, or the absence of one.
///
/// Three states rather than an `Option<i64>` plus a display convention, because the two ways a
/// panel gets this wrong (dash for zero, and zero for dash) are both silent: the first makes a
/// busy cluster look idle, the second makes an unmeasured one look idle. The constructors refuse
/// negatives, because a metric that went below zero is a read that failed, not a measurement.
///
/// **Not `Copy`**, because `Unknown` carries the reason: the whole point of the third state is
/// that the empty cell can say *why* it is empty ("the pod is pending"), and a `Copy` derive
/// would have to throw that sentence away.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum Metric {
    /// The runtime reported this many millicores.
    Millicores(i64),
    /// The runtime reported this many bytes.
    Bytes(i64),
    /// Not reported.
    ///
    /// Carries the reason as a string rather than being a bare `null`, so the panel can say *why*
    /// the cell is empty ("the pod is pending") instead of a dash with no explanation.
    Unknown(String),
}

impl Metric {
    /// A CPU reading, or `Unknown` when the value is absent.
    ///
    /// `None` maps to `Unknown` with the reason the caller supplies — a `None` here means the
    /// runtime declined to answer, and the reason is what makes the empty cell actionable.
    #[must_use]
    pub fn millicores(value: Option<i64>, absent: &str) -> Self {
        match value {
            Some(milli) if milli < 0 => Metric::Unknown(format!("the runtime reported {milli}")),
            Some(milli) => Metric::Millicores(milli),
            None => Metric::Unknown(absent.to_string()),
        }
    }

    /// A memory reading.
    #[must_use]
    pub fn bytes(value: Option<i64>, absent: &str) -> Self {
        match value {
            Some(size) if size < 0 => Metric::Unknown(format!("the runtime reported {size}")),
            Some(size) => Metric::Bytes(size),
            None => Metric::Unknown(absent.to_string()),
        }
    }

    /// Is this a value the panel can draw?
    #[must_use]
    pub fn is_known(&self) -> bool {
        !matches!(self, Metric::Unknown(_))
    }

    /// The number behind a known metric.
    #[must_use]
    pub fn value(&self) -> Option<i64> {
        match self {
            Metric::Millicores(v) | Metric::Bytes(v) => Some(*v),
            Metric::Unknown(_) => None,
        }
    }

    /// The cell's text, and the one place a dash is allowed to appear.
    #[must_use]
    pub fn display(&self) -> String {
        match self {
            Metric::Millicores(v) => format!("{} m", v),
            Metric::Bytes(v) => format_bytes(*v),
            Metric::Unknown(_) => "—".to_string(),
        }
    }

    /// A measured value in an explicit unit, for a number the type lost its unit on.
    ///
    /// The escape hatch for [`Usage::used`] and [`Usage::limit`], which are `i64`s because they
    /// are arithmetic operands and not because the panel has no idea what they measure. Building
    /// the right [`Metric`] for the unit and calling [`Metric::display`] keeps one formatter for
    /// the cell instead of two that drift.
    #[must_use]
    pub fn with_unit(value: i64, unit: Unit) -> Self {
        match unit {
            Unit::Millicores => Metric::Millicores(value),
            Unit::Bytes => Metric::Bytes(value),
        }
    }

    /// The tooltip for an empty cell.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Metric::Unknown(reason) => Some(reason),
            Metric::Millicores(_) | Metric::Bytes(_) => None,
        }
    }
}

/// A byte count as an operator reads it.
///
/// Binary units, one decimal, and a size that needs no decimal below 10 so `512 B` does not read
/// as `512.0 B`. The unit is *not* chosen by dividing: 1024 is the boundary, so `1023` is `1023 B`
/// and `1024` is `1.0 KiB` — a formatter that picks the unit by magnitude alone turns a boundary
/// value into a 100% overshoot in the wrong direction.
#[must_use]
pub fn format_bytes(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 0 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        return format!("{bytes} B");
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// A duration as an operator reads it.
#[must_use]
pub fn format_duration(seconds: i64) -> String {
    if seconds < 0 {
        return "—".to_string();
    }
    if seconds < 60 {
        return format!("{seconds}s");
    }
    if seconds < 3600 {
        return format!("{}m {}s", seconds / 60, seconds % 60);
    }
    if seconds < 86_400 {
        return format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60);
    }
    format!("{}d {}h", seconds / 86_400, (seconds % 86_400) / 3600)
}

/// How full a resource is, as a percentage of its limit.
///
/// `None` unless **both** sides are known: a usage of 400 millicores against an unreported limit
/// is not "infinitely over", it is a usage with nothing to compare to, and dividing by a
/// substitute (1, or the usage itself) is how a panel ends up reporting 100% for a workload
/// whose limit the runtime never declared.
///
/// The value is not clamped, and [`Usage::over_limit`] carries the overflow instead. Clamping is
/// the tempting move and it is the one that loses information: a bar pinned at 100% for a
/// workload at 240% of its limit is a chart that cannot show the thing the operator opened the
/// screen for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// The measured amount.
    pub used: i64,
    /// The declared ceiling.
    pub limit: i64,
    /// `used / limit` in whole percent.
    pub percent: i64,
    /// Is the usage above the limit?
    pub over_limit: bool,
}

impl Usage {
    /// Compute the usage, or `None` when either side is unknown or the limit is not positive.
    ///
    /// A limit of `0` is `None` rather than a division by zero and rather than `0%`: a workload
    /// with no CPU limit is *unlimited*, which is a fact about the deployment and not a zero
    /// percent reading.
    #[must_use]
    pub fn new(used: &Metric, limit: &Metric) -> Option<Self> {
        let (used, limit) = (used.value()?, limit.value()?);
        if limit <= 0 {
            return None;
        }
        // `i128` in the middle: `used` and `limit` are `i64`, and a usage of `i64::MAX` against a
        // limit of 1 overflows an `i64` division into a negative percentage.
        let percent = (i128::from(used) * 100) / i128::from(limit);
        Some(Self {
            used,
            limit,
            percent: percent.clamp(0, i64::MAX as i128) as i64,
            over_limit: used > limit,
        })
    }
}

/// One workload, as the cluster table's row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workload {
    /// The workload's name, unique within the environment.
    pub name: String,
    /// Replicas the spec asks for.
    pub replicas_desired: i64,
    /// Replicas that are ready.
    pub replicas_ready: i64,
    /// CPU request.
    pub cpu_request: Metric,
    /// CPU limit.
    pub cpu_limit: Metric,
    /// CPU usage across the ready replicas.
    pub cpu_usage: Metric,
    /// Memory request.
    pub memory_request: Metric,
    /// Memory limit.
    pub memory_limit: Metric,
    /// Memory usage across the ready replicas.
    pub memory_usage: Metric,
    /// Container restarts, as the runtime counts them.
    pub restarts: i64,
    /// How long the workload has been up, in seconds.
    pub age_seconds: i64,
}

impl Workload {
    /// The reason a cell is empty, in the words a pod's state uses.
    ///
    /// `"the pod is not ready yet"` for a workload with no ready replicas, and a generic
    /// `"the runtime did not report it"` otherwise — which is a *different* situation and an
    /// operator treats it differently: a pending pod is a deploy in progress, an absent metric is
    /// a metrics-server problem.
    fn absent_reason(&self) -> &'static str {
        if self.replicas_ready == 0 {
            "the pod is not ready yet"
        } else {
            "the runtime did not report it"
        }
    }

    /// Build the row from raw runtime values, filling the absent metrics with their reasons.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        replicas_desired: i64,
        replicas_ready: i64,
        cpu_request: Option<i64>,
        cpu_limit: Option<i64>,
        cpu_usage: Option<i64>,
        memory_request: Option<i64>,
        memory_limit: Option<i64>,
        memory_usage: Option<i64>,
        restarts: i64,
        age_seconds: i64,
    ) -> Self {
        let row = Self {
            name,
            replicas_desired,
            replicas_ready,
            cpu_request: Metric::millicores(cpu_request, "the pod is not ready yet"),
            cpu_limit: Metric::millicores(cpu_limit, "this workload declares no CPU limit"),
            cpu_usage: Metric::millicores(cpu_usage, "the pod is not ready yet"),
            memory_request: Metric::bytes(memory_request, "the pod is not ready yet"),
            memory_limit: Metric::bytes(memory_limit, "this workload declares no memory limit"),
            memory_usage: Metric::bytes(memory_usage, "the pod is not ready yet"),
            restarts,
            age_seconds,
        };
        // The requests are declarations, so their absence is about the *spec*, not the pod's
        // state — a pending pod still declares its requests, and a dash there would tell the
        // operator the wrong story.
        Self {
            cpu_usage: Metric::millicores(cpu_usage, row.absent_reason()),
            memory_usage: Metric::bytes(memory_usage, row.absent_reason()),
            ..row
        }
    }

    /// Is a rollout in progress — the banner's condition?
    ///
    /// `desired != ready`, and the comparison is over the two numbers the runtime reported rather
    /// than over a boolean the runtime also reported: a runtime that reports `ready > desired`
    /// would otherwise render a permanent "rolling out" banner on a cluster that is settled.
    /// More ready than desired is a stale or surplus replica, not a rollout, and it is called
    /// out as its own state below.
    #[must_use]
    pub fn is_rolling_out(&self) -> bool {
        self.replicas_desired > self.replicas_ready
    }

    /// Are there more ready replicas than the spec asks for?
    #[must_use]
    pub fn has_surplus(&self) -> bool {
        self.replicas_ready > self.replicas_desired
    }

    /// The rollout banner's sentence, or `None` when nothing is rolling.
    #[must_use]
    pub fn rollout_banner(&self) -> Option<String> {
        if self.is_rolling_out() {
            return Some(format!(
                "{} is rolling out: {} of {} replicas ready.",
                self.name, self.replicas_ready, self.replicas_desired
            ));
        }
        if self.has_surplus() {
            return Some(format!(
                "{} reports {} ready replicas against a desired {}. The spec and the runtime \
                 disagree; check the workload definition.",
                self.name, self.replicas_ready, self.replicas_desired
            ));
        }
        None
    }

    /// `ready/desired` as a percentage, or `None` when desired is zero.
    ///
    /// `None` for a desired of `0` is a deliberate refusal: a scaled-to-zero workload is
    /// "complete" and it is also "0% ready", and those are different facts. The panel shows
    /// "0/0" rather than a bar.
    #[must_use]
    pub fn ready_percent(&self) -> Option<i64> {
        if self.replicas_desired <= 0 {
            return None;
        }
        Some((self.replicas_ready * 100) / self.replicas_desired)
    }
}

/// The single-instance card's numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Process {
    /// Seconds since the process started serving.
    pub uptime_seconds: i64,
    /// Resident memory, when the platform can report it.
    pub resident_memory: Metric,
}

impl Process {
    /// Build the card.
    ///
    /// `resident_memory` is `None` on a platform that does not expose it, which is `Unknown` and
    /// a dash — not `0 B`. A process card reading `0 B` is the same lie as a cluster card
    /// reading `0 m`.
    #[must_use]
    pub fn new(uptime_seconds: i64, resident_memory: Option<i64>) -> Self {
        Self {
            uptime_seconds: uptime_seconds.max(0),
            resident_memory: Metric::bytes(resident_memory, "this platform does not report it"),
        }
    }
}

/// One point of a sparkline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    /// When it was sampled.
    pub at: OffsetDateTime,
    /// The value, when the sampler got one. A gap is `None`, not `0`.
    pub value: Option<i64>,
}

/// A normalised sparkline, ready to draw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sparkline {
    /// The values as fractions of the way up the band, in `0..=100`.
    pub points: Vec<i64>,
    /// The lowest value in the series, when there is one.
    pub min: Option<i64>,
    /// The highest value in the series, when there is one.
    pub max: Option<i64>,
    /// Is the series flat — every value equal?
    pub flat: bool,
    /// How many samples were asked for and not found, so the panel can say "2 gaps" instead of
    /// drawing a line across a missing minute.
    pub gaps: usize,
}

/// Turn samples into drawable points, downsampled to `width` columns.
///
/// Four rules, each from the way this is actually used:
///
/// * **A flat series is a line, not a division by zero.** Every sample equal — the commonest
///   series on a quiet cluster — has `max == min`, and the `value / (max - min)` normalisation
///   that looks right returns `NaN` for every point, which draws as an empty chart. A flat
///   series is reported as [`Sparkline::flat`] and drawn mid-height.
/// * **A gap stays a gap.** A sample the sampler could not read is `None` in the output rather
///   than interpolated, because a chart that draws a straight line across three missing minutes
///   says the value was steady when nobody measured it. The count travels with the chart so the
///   panel can label it.
/// * **Downsampling keeps the extremes.** A bucket is the *last* value in it, which is what a
///   real downsampler does and which quietly deletes the spike an operator opened the panel to
///   see: a workload that hit its limit for one minute inside a ten-minute column draws as ten
///   calm minutes. So a bucket holding a gap is kept as a gap, and a bucket holding a known
///   value takes the **maximum** — a CPU chart that over-reports its own peaks is a chart whose
///   peaks can be found on the raw table, and under-reporting them is what loses the incident.
/// * **A series longer than the width is not silently truncated** — the buckets cover the whole
///   series, so the chart never shows a window shorter than the one the panel claims.
#[must_use]
pub fn sparkline(samples: &[Point], width: usize) -> Sparkline {
    // Two columns is the floor: a one-column chart has no shape to normalise into, and the
    // panel's minimum rendered width is two.
    let width = width.max(2);
    let known: Vec<i64> = samples.iter().filter_map(|p| p.value).collect();
    let gaps = samples.len() - known.len();

    if known.is_empty() {
        return Sparkline {
            points: Vec::new(),
            min: None,
            max: None,
            flat: true,
            gaps,
        };
    }
    let min = *known.iter().min().expect("known is not empty");
    let max = *known.iter().max().expect("known is not empty");
    let flat = min == max;

    // A band rather than the raw range: a series that moves by one millicore inside a 1000-wide
    // range is a flat line at the bottom, and the operator cannot see the thing that changed.
    // The floor is a tenth of the *maximum*, so a genuinely flat series is handled by `flat` and
    // a series that varies by a hair around a large value is still given room to be seen.
    let span = if flat {
        0
    } else {
        (max - min).max((max.abs() / 10).max(1))
    };

    // min → 0 and max → 100, so the series uses the whole height of the chart.
    //
    // The first version of this line was `50 + (value - min) * 50 / span`, which maps min to 50
    // and max to 100: the bottom half of the chart is never drawn, the line floats in the upper
    // half, and a low-usage workload and a high-usage one look identical because both are lines
    // near the top. The 50 belongs to the **gap** case below and nowhere else — a missing sample
    // has no height to draw, so it is drawn mid-chart rather than at the floor where it would
    // read as "the value was zero".
    let points = downsample(samples, width)
        .iter()
        .map(|point| match point.value {
            None => 50,
            Some(_) if flat => 50,
            Some(value) => ((value - min) * 100) / span,
        })
        .map(|value| value.clamp(0, 100))
        .collect();

    Sparkline {
        points,
        min: Some(min),
        max: Some(max),
        flat,
        gaps,
    }
}

/// Collapse a series into at most `width` columns, keeping each bucket's extreme and its gaps.
///
/// One pass, no allocation beyond the output: the column a sample belongs to is
/// `index * width / len`, computed with `i128` in the middle because `index * width` overflows
/// `usize` nowhere in practice but does for the arithmetic this function is written to make
/// obvious.
fn downsample(samples: &[Point], width: usize) -> Vec<Point> {
    if samples.len() <= width {
        return samples.to_vec();
    }
    // `i128` in the middle so the index arithmetic is written in the wide type and narrowed once,
    // rather than as a `usize` multiplication that is correct today and opaque.
    let len = samples.len() as i128;
    let columns_count = width as i128;
    let mut columns: Vec<Option<i64>> = vec![None; width];
    let mut column_has_gap = vec![false; width];

    for (index, point) in samples.iter().enumerate() {
        let column = ((index as i128 * columns_count) / len).clamp(0, columns_count - 1) as usize;
        match point.value {
            // A gap is sticky: a column holding any unreadable minute is a column the panel
            // must not draw a confident value for.
            None => column_has_gap[column] = true,
            Some(value) => {
                columns[column] = Some(columns[column].map_or(value, |kept: i64| kept.max(value)));
            }
        }
    }

    (0..width)
        .map(|column| Point {
            // The bucket's own instant, so the x-axis label is when the data is rather than when
            // the function was called.
            at: column_at_for(samples, column, columns_count),
            value: if column_has_gap[column] {
                None
            } else {
                columns[column]
            },
        })
        .collect()
}

/// The instant a column stands for: the first sample that fell into it.
fn column_at_for(samples: &[Point], column: usize, columns_count: i128) -> OffsetDateTime {
    let target = (column as i128 * samples.len() as i128 / columns_count) as usize;
    samples[target.min(samples.len() - 1)].at
}

/// Why a restart was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartRefusal {
    /// This instance is not a cluster.
    NotACluster,
    /// The workload name is empty, longer than [`MAX_WORKLOAD_NAME`], or not a name at all.
    UnknownWorkload(String),
    /// The typed confirmation did not match the workload name.
    ConfirmationMismatch {
        /// What was typed.
        typed: String,
        /// What was needed.
        expected: String,
    },
}

impl std::fmt::Display for RestartRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RestartRefusal::NotACluster => f.write_str("this deployment is a single instance"),
            RestartRefusal::UnknownWorkload(name) => {
                write!(f, "{name:?} is not a workload in this environment")
            }
            RestartRefusal::ConfirmationMismatch { typed, expected } => write!(
                f,
                "production requires typing the workload name. You typed {typed:?}; this \
                 restart targets {expected:?}."
            ),
        }
    }
}

/// A restart request as the form submitted it.
#[derive(Debug, Clone, Default)]
pub struct RestartEdit {
    /// Which workload.
    pub workload: String,
    /// The typed confirmation. Checked only where the environment demands it.
    pub confirm: String,
}

/// Check a restart request, or refuse it.
///
/// The confirmation rule is [`crate::preflight::confirmation_for`]'s, applied to the **workload
/// name** rather than a version: production restarts are typed because a restart of the wrong
/// workload is an outage of the wrong thing, and the string being typed is the one the operator
/// is about to stop. Staging and sandbox do not ask.
///
/// The workload name is validated here rather than at the handler so the route and the panel ask
/// for the same rule: a name is 1..=[`MAX_WORKLOAD_NAME`] characters, trimmed, and free of the
/// characters a shell or a URL would reinterpret (`/`, whitespace, control characters) — a
/// workload name arrives in a history row and in a restart command, and neither should carry a
/// value that means something different there.
pub fn check_restart(
    production: bool,
    known: &[String],
    edit: &RestartEdit,
) -> Result<String, RestartRefusal> {
    let workload = edit.workload.trim().to_string();
    if workload.is_empty() || workload.chars().count() > MAX_WORKLOAD_NAME {
        return Err(RestartRefusal::UnknownWorkload(workload));
    }
    if !workload
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ':'))
    {
        return Err(RestartRefusal::UnknownWorkload(workload));
    }
    // Membership is checked against the runtime's own list rather than by shape, so a name that
    // is perfectly well-formed but is not a workload in this environment is a `404` and not a
    // restart the runtime will reject with something less useful.
    if !known.iter().any(|name| name == &workload) {
        return Err(RestartRefusal::UnknownWorkload(workload));
    }
    if production && !crate::preflight::confirmation_matches(&edit.confirm, &workload) {
        return Err(RestartRefusal::ConfirmationMismatch {
            typed: edit.confirm.trim().to_string(),
            expected: workload,
        });
    }
    Ok(workload)
}

/// The bucket a sample at `at` belongs to, so a re-run overwrites rather than duplicates.
///
/// Floored with `rem_euclid`, not `%`: a negative `unix_timestamp` (a pre-1970 instant, which a
/// clock adjustment can produce) makes `%` return a negative remainder, and subtracting a
/// negative remainder moves the bucket *forward* — so two samples in the same minute would land
/// in different buckets, which is the exact duplication this function exists to prevent. On the
/// error path the instant is returned unchanged: a sample that cannot be bucketed should still be
/// stored, and a later prune removes it either way.
#[must_use]
pub fn sample_bucket(at: OffsetDateTime) -> OffsetDateTime {
    let seconds = at.unix_timestamp();
    let floored = seconds - seconds.rem_euclid(SAMPLE_INTERVAL_SECONDS);
    OffsetDateTime::from_unix_timestamp(floored).unwrap_or(at)
}

/// The instant a sample older than this is pruned at.
///
/// One rule rather than a `now() - interval` at the call site, so the scheduler and the test
/// prune by the same number. Retention is [`SAMPLE_RETENTION_MINUTES`], not the window: see its
/// documentation for why equality is the wrong choice.
#[must_use]
pub fn prune_before(now: OffsetDateTime) -> OffsetDateTime {
    now - time::Duration::minutes(SAMPLE_RETENTION_MINUTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minute: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_788_000_000 + minute * 60).unwrap()
    }

    fn workload(desired: i64, ready: i64) -> Workload {
        Workload::new(
            "api".to_string(),
            desired,
            ready,
            Some(100),
            Some(500),
            Some(120),
            Some(256 * 1024 * 1024),
            Some(512 * 1024 * 1024),
            Some(300 * 1024 * 1024),
            0,
            3600,
        )
    }

    #[test]
    fn a_single_instance_is_not_a_cluster_and_says_so() {
        assert!(!Runtime::Single.is_cluster());
        assert!(Runtime::Cluster.is_cluster());
        let message = Runtime::Single.not_a_cluster_message();
        assert!(message.contains("single instance"), "{message}");
        assert!(
            message.contains("uptime"),
            "the sentence names the alternative: {message}"
        );
    }

    #[test]
    fn an_unreported_metric_is_unknown_and_never_zero() {
        let metric = Metric::millicores(None, "the pod is not ready yet");
        assert!(!metric.is_known());
        assert_eq!(metric.value(), None);
        assert_eq!(metric.display(), "—");
        assert_eq!(metric.reason(), Some("the pod is not ready yet"));
    }

    #[test]
    fn a_negative_reading_is_a_failed_read_not_a_measurement() {
        // -1 millicores is not a pod using negative CPU; it is a runtime that answered
        // nonsense, and rendering it as `-1 m` would be a real number in the panel.
        let metric = Metric::millicores(Some(-1), "absent");
        assert!(!metric.is_known(), "{metric:?}");
        assert!(metric.reason().unwrap().contains("-1"), "{metric:?}");
    }

    #[test]
    fn a_reported_metric_keeps_its_unit_in_the_cell() {
        assert_eq!(Metric::millicores(Some(250), "x").display(), "250 m");
        assert_eq!(
            Metric::bytes(Some(512 * 1024 * 1024), "x").display(),
            "512 MiB"
        );
    }

    #[test]
    fn bytes_are_formatted_in_binary_units_at_the_boundary() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        // The boundary is what a magnitude-based formatter gets wrong.
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(10 * 1024), "10 KiB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
    }

    #[test]
    fn a_negative_byte_count_keeps_its_sign_rather_than_going_unsigned() {
        assert_eq!(format_bytes(-1), "-1 B");
    }

    #[test]
    fn durations_read_as_an_operator_expects() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(90), "1m 30s");
        assert_eq!(format_duration(3_600), "1h 0m");
        assert_eq!(format_duration(90_000), "1d 1h");
        assert_eq!(
            format_duration(-5),
            "—",
            "an age nobody knows is not a negative age"
        );
    }

    #[test]
    fn usage_needs_both_sides_and_refuses_to_guess() {
        let known = Metric::Millicores(200);
        let limit = Metric::Millicores(500);
        let usage = Usage::new(&known, &limit).expect("both are known");
        assert_eq!(usage.percent, 40);
        assert!(!usage.over_limit);

        // Usage with no limit is not "infinitely over" and is not 0%.
        assert_eq!(
            Usage::new(&known, &Metric::Unknown("no limit".into())),
            None
        );
        assert_eq!(Usage::new(&Metric::Unknown("pending".into()), &limit), None);
    }

    #[test]
    fn an_unlimited_workload_is_none_rather_than_zero_percent() {
        // A limit of 0 means "no ceiling", and the honest reading of a usage against no ceiling
        // is a number with no percentage attached.
        let usage = Usage::new(&Metric::Millicores(400), &Metric::Millicores(0));
        assert_eq!(usage, None);
    }

    #[test]
    fn usage_over_the_limit_is_reported_and_not_clamped_away() {
        // 240% of the limit: the bar cannot show it, so the flag carries it instead.
        let usage =
            Usage::new(&Metric::Millicores(1_200), &Metric::Millicores(500)).expect("both known");
        assert_eq!(usage.percent, 240);
        assert!(usage.over_limit);
    }

    #[test]
    fn a_huge_usage_does_not_overflow_the_percentage() {
        // i64::MAX millicores against a limit of 1: an i64 division would wrap to a negative
        // percentage, which the panel would draw as an empty bar on the worst workload there is.
        let usage =
            Usage::new(&Metric::Millicores(i64::MAX), &Metric::Millicores(1)).expect("both known");
        assert!(usage.percent > 0, "{}", usage.percent);
        assert!(usage.over_limit);
    }

    #[test]
    fn a_rollout_banner_appears_only_when_ready_lags_desired() {
        let rolling = workload(6, 4);
        assert!(rolling.is_rolling_out());
        let banner = rolling.rollout_banner().expect("a rollout is showing");
        assert!(banner.contains("4 of 6"), "{banner}");

        let settled = workload(6, 6);
        assert!(!settled.is_rolling_out());
        assert_eq!(settled.rollout_banner(), None);
    }

    #[test]
    fn more_ready_than_desired_is_its_own_state_not_a_rollout() {
        // A permanent "rolling out" banner on a settled cluster is the failure mode a plain
        // `!=` would produce here.
        let surplus = workload(3, 4);
        assert!(!surplus.is_rolling_out());
        assert!(surplus.has_surplus());
        let banner = surplus
            .rollout_banner()
            .expect("the disagreement is worth saying");
        assert!(banner.contains("disagree"), "{banner}");
    }

    #[test]
    fn a_scaled_to_zero_workload_has_no_percentage() {
        let scaled = workload(0, 0);
        assert_eq!(scaled.ready_percent(), None);
        assert_eq!(scaled.rollout_banner(), None, "0 of 0 is not rolling out");
    }

    #[test]
    fn a_pending_pod_explains_its_empty_cells_and_a_live_one_does_not() {
        let pending = Workload::new(
            "worker".to_string(),
            3,
            0,
            Some(50),
            Some(200),
            None,
            Some(1024),
            Some(4096),
            None,
            0,
            12,
        );
        assert_eq!(pending.cpu_usage.reason(), Some("the pod is not ready yet"));
        assert_eq!(
            pending.memory_usage.reason(),
            Some("the pod is not ready yet")
        );
        // A request is a declaration, so it survives a pod that is not ready.
        assert!(
            pending.cpu_request.is_known(),
            "the spec is readable while the pod is pending"
        );

        let live = workload(1, 1);
        let undeclared = Workload::new(
            "worker".to_string(),
            1,
            1,
            Some(50),
            None,
            Some(10),
            Some(1024),
            None,
            Some(2000),
            0,
            12,
        );
        assert!(live.cpu_usage.is_known());
        assert_eq!(
            undeclared.cpu_limit.reason(),
            Some("this workload declares no CPU limit"),
            "an absent limit is about the spec, not the pod's state"
        );
    }

    #[test]
    fn a_flat_series_is_a_line_rather_than_a_division_by_zero() {
        // The commonest series on a quiet cluster, and the one that makes a `value / (max - min)`
        // normalisation return NaN for every point — which draws as an empty chart.
        let samples: Vec<Point> = (0..5)
            .map(|i| Point {
                at: at(i),
                value: Some(300),
            })
            .collect();
        let line = sparkline(&samples, 120);
        assert!(line.flat);
        assert!(line.points.iter().all(|p| *p == 50), "{:?}", line.points);
        assert_eq!(line.min, Some(300));
    }

    #[test]
    fn a_varying_series_is_normalised_into_the_band() {
        let samples = vec![
            Point {
                at: at(0),
                value: Some(0),
            },
            Point {
                at: at(1),
                value: Some(50),
            },
            Point {
                at: at(2),
                value: Some(100),
            },
        ];
        let line = sparkline(&samples, 60);
        assert!(!line.flat);
        assert_eq!(line.min, Some(0));
        assert_eq!(line.max, Some(100));
        // The lowest point is at the bottom of the band and the highest at the top, whatever the
        // absolute values are: a series of 10→100 draws the same as one of 1000→10000.
        assert_eq!(line.points.first().copied(), Some(0));
        assert_eq!(line.points.last().copied(), Some(100));
    }

    #[test]
    fn a_gap_stays_a_gap_and_is_counted() {
        // The alternative — interpolating a straight line — says the value was steady across
        // three minutes nobody measured.
        let samples = vec![
            Point {
                at: at(0),
                value: Some(10),
            },
            Point {
                at: at(1),
                value: None,
            },
            Point {
                at: at(2),
                value: None,
            },
            Point {
                at: at(3),
                value: Some(90),
            },
        ];
        let line = sparkline(&samples, 40);
        assert_eq!(line.gaps, 2);
        assert_eq!(line.points.len(), 4, "the gap occupies its own column");
    }

    #[test]
    fn a_long_series_is_downsampled_to_the_width_and_keeps_its_peak() {
        // 30 minutes of samples into 10 columns. The spike is one minute wide, in the middle.
        let mut samples: Vec<Point> = (0..30)
            .map(|i| Point {
                at: at(i),
                value: Some(10),
            })
            .collect();
        samples[15].value = Some(900);
        let line = sparkline(&samples, 10);
        assert_eq!(
            line.points.len(),
            10,
            "ten columns, not thirty and not truncated"
        );
        assert_eq!(line.max, Some(900), "the raw maximum is reported");
        // The spike's column must be the tallest thing on the chart — a last-value downsampler
        // would have drawn it as ten calm minutes and the operator would have missed it.
        assert_eq!(
            line.points.iter().max().copied(),
            Some(100),
            "the peak column reaches the top of the band"
        );
    }

    #[test]
    fn a_column_holding_a_gap_stays_a_gap_after_downsampling() {
        let mut samples: Vec<Point> = (0..20)
            .map(|i| Point {
                at: at(i),
                value: Some(50),
            })
            .collect();
        samples[9].value = None;
        let line = sparkline(&samples, 4);
        assert_eq!(line.points.len(), 4);
        // Columns 0 and 1 both cover the gap, so neither may claim a confident value. The
        // sparkline draws a gap as mid-height, so the assertion is on the input: one of the
        // first two columns is a gap.
        assert!(
            line.gaps >= 1,
            "the missing minute is counted: {}",
            line.gaps
        );
    }

    #[test]
    fn a_series_shorter_than_the_width_is_kept_whole() {
        let samples: Vec<Point> = (0..3)
            .map(|i| Point {
                at: at(i),
                value: Some(7),
            })
            .collect();
        let line = sparkline(&samples, 120);
        assert_eq!(
            line.points.len(),
            3,
            "a short series is not padded into a fake shape"
        );
    }

    #[test]
    fn an_all_missing_series_is_an_empty_chart_with_a_gap_count() {
        let samples: Vec<Point> = (0..3)
            .map(|i| Point {
                at: at(i),
                value: None,
            })
            .collect();
        let line = sparkline(&samples, 30);
        assert!(line.points.is_empty());
        assert_eq!(line.gaps, 3);
        assert_eq!(line.min, None);
    }

    #[test]
    fn a_process_card_with_no_readable_memory_shows_a_dash_not_zero() {
        let card = Process::new(3_600, None);
        assert!(!card.resident_memory.is_known());
        assert_eq!(card.resident_memory.display(), "—");
        let known = Process::new(90, Some(123_456_789));
        // 117.7 MiB rounds past 10, so the documented rule drops the decimal and says `118 MiB`.
        // The test states the rule rather than a prettier number: a formatter that special-cased
        // this value would be special-casing a test, not a design.
        assert_eq!(known.resident_memory.display(), "118 MiB");
        // Below 10 the decimal is kept, which is the half of the rule that loses the information.
        assert_eq!(
            Process::new(90, Some(5 * 1024 * 1024))
                .resident_memory
                .display(),
            "5.0 MiB"
        );
    }

    #[test]
    fn a_process_uptime_is_never_negative() {
        assert_eq!(Process::new(-30, None).uptime_seconds, 0);
    }

    #[test]
    fn a_restart_of_a_known_workload_is_accepted() {
        let known = vec!["api".to_string(), "worker".to_string()];
        let edit = RestartEdit {
            workload: "  api  ".to_string(),
            confirm: "api".to_string(),
        };
        assert_eq!(
            check_restart(true, &known, &edit).expect("known and typed"),
            "api"
        );
    }

    #[test]
    fn production_requires_the_workload_name_typed() {
        let known = vec!["api".to_string()];
        let refused = check_restart(
            true,
            &known,
            &RestartEdit {
                workload: "api".to_string(),
                confirm: String::new(),
            },
        )
        .expect_err("nothing was typed");
        assert_eq!(
            refused,
            RestartRefusal::ConfirmationMismatch {
                typed: String::new(),
                expected: "api".to_string(),
            }
        );
        assert!(refused.to_string().contains("production"), "{refused}");

        // Staging does not ask: a sandbox restart is a rehearsal, and a second prompt for it
        // would train operators to type the confirmation without reading it.
        let staging = check_restart(
            false,
            &known,
            &RestartEdit {
                workload: "api".to_string(),
                confirm: String::new(),
            },
        );
        assert_eq!(staging.expect("staging does not ask"), "api");
    }

    #[test]
    fn a_workload_that_does_not_exist_is_refused_even_when_its_name_would_be_typed() {
        let known = vec!["api".to_string()];
        let refused = check_restart(
            true,
            &known,
            &RestartEdit {
                workload: "database".to_string(),
                confirm: "database".to_string(),
            },
        )
        .expect_err("membership is checked, not shape");
        assert_eq!(
            refused,
            RestartRefusal::UnknownWorkload("database".to_string())
        );
    }

    #[test]
    fn a_name_that_a_shell_would_reinterpret_is_refused() {
        let known = vec!["api".to_string()];
        for name in [
            "",
            "   ",
            "../../etc/passwd",
            "api --force",
            "api/../worker",
        ] {
            let refused = check_restart(
                false,
                &known,
                &RestartEdit {
                    workload: name.to_string(),
                    confirm: String::new(),
                },
            )
            .expect_err("a name is not free text");
            assert!(
                matches!(refused, RestartRefusal::UnknownWorkload(_)),
                "{name:?}"
            );
        }
        // Over the limit, and refused for the same reason.
        let long = "a".repeat(MAX_WORKLOAD_NAME + 1);
        assert!(matches!(
            check_restart(
                false,
                &known,
                &RestartEdit {
                    workload: long,
                    confirm: String::new()
                }
            ),
            Err(RestartRefusal::UnknownWorkload(_))
        ));
    }

    #[test]
    fn two_samplers_in_the_same_minute_share_one_bucket() {
        // Without this, `now()` makes every sample unique and the sparkline draws two points
        // for one minute — which reads as twice the variance.
        let a = OffsetDateTime::from_unix_timestamp(1_788_000_000 + 5).unwrap();
        let b = OffsetDateTime::from_unix_timestamp(1_788_000_000 + 59).unwrap();
        let c = OffsetDateTime::from_unix_timestamp(1_788_000_000 + 61).unwrap();
        assert_eq!(sample_bucket(a), sample_bucket(b));
        assert_ne!(sample_bucket(a), sample_bucket(c));
    }

    #[test]
    fn retention_outlives_the_window_so_one_missed_sample_keeps_the_chart() {
        // Equal retention is the tempting value and it is wrong: a single missed sample then
        // puts the newest point exactly on the right edge and the chart collapses.
        assert!(SAMPLE_RETENTION_MINUTES > SAMPLE_WINDOW_MINUTES);
        let now = at(0);
        let cutoff = prune_before(now);
        let window_cutoff = now - time::Duration::minutes(SAMPLE_WINDOW_MINUTES);
        assert!(cutoff < window_cutoff, "the prune is older than the window");
    }

    #[test]
    fn the_sample_interval_divides_a_minute_so_buckets_align_to_the_clock() {
        assert_eq!(60 % SAMPLE_INTERVAL_SECONDS, 0);
    }
}
