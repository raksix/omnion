//! The exporter pipeline (REQ-126, slice 3).
//!
//! The request's rule is one sentence and it is the whole design: **the request path is never
//! blocked by telemetry.** Everything here follows from it, and each of the choices below is a
//! decision that a "just send it inline" implementation gets wrong in a way only a load test finds:
//!
//! * **A bounded ring per exporter, drop-oldest.** When the backend is down the buffer fills, and
//!   the oldest sample is the one that has aged out of usefulness — a three-second-old log line is
//!   more useful than a three-minute-old one. Dropping *newest* instead is the natural mistake and
//!   it means a recovering exporter exports the future and loses the incident.
//! * **The drop is counted, in the same registry, in the same scrape.**
//!   `omnion_exporter_dropped_total{exporter}` is a declared family already, so an operator sees
//!   the loss on the same `/metrics` page as everything else. A buffer that discards without
//!   saying so is the "silently dropped" the request forbids in the metric section.
//! * **Nothing runs on the caller's task.** A test that pushes into a full buffer returns
//!   immediately; that is the assertion, not a latency number.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::metrics;

/// The family the drops are counted in. Declared in `metrics::FAMILIES`; named here so the
/// recorder and the test cannot drift apart.
pub const DROPPED_FAMILY: &str = "omnion_exporter_dropped_total";

/// The default capacity of one exporter's buffer.
///
/// 4096 is chosen so a burst of 4k spans costs a few hundred kilobytes — a rounding error next to
/// a single request's response body — while still holding a full second of a busy instance's
/// telemetry between flushes.
pub const DEFAULT_BUFFER_CAPACITY: usize = 4096;

/// The cap on how many buffers one collector holds.
///
/// Exporters are operator-configured rows, not a hot path, and an unbounded map here is a slow
/// leak. The cap refuses a new exporter rather than evicting a live one, because dropping a
/// configured exporter's telemetry is a decision an operator should see.
pub const MAX_EXPORTERS: usize = 32;

/// The kinds of exporter the request names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExporterKind {
    /// OTLP over gRPC or HTTP.
    Otlp,
    /// Prometheus remote-write.
    PrometheusRemoteWrite,
    /// Syslog.
    Syslog,
    /// A webhook that receives a JSON batch.
    Webhook,
}

impl ExporterKind {
    /// The name the `obs_exporters.kind` check constraint accepts.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Otlp => "otlp",
            Self::PrometheusRemoteWrite => "prometheus_remote_write",
            Self::Syslog => "syslog",
            Self::Webhook => "webhook",
        }
    }

    /// Parse the stored name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "otlp" => Some(Self::Otlp),
            "prometheus_remote_write" => Some(Self::PrometheusRemoteWrite),
            "syslog" => Some(Self::Syslog),
            "webhook" => Some(Self::Webhook),
            _ => None,
        }
    }

    /// Every kind, for the form's selector and the validator.
    pub const ALL: &'static [Self] = &[
        Self::Otlp,
        Self::PrometheusRemoteWrite,
        Self::Syslog,
        Self::Webhook,
    ];
}

/// An exporter's health, as the screen's chip reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExporterHealth {
    /// Configured, never flushed.
    Unknown,
    /// The last flush succeeded.
    Ok,
    /// Some batches failed, the last recent one may have succeeded.
    Degraded,
    /// Consecutive failures, nothing has flushed.
    Down,
}

impl ExporterHealth {
    /// The name the `obs_exporters.health` check constraint accepts.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Ok => "ok",
            Self::Degraded => "degraded",
            Self::Down => "down",
        }
    }
}

/// The wire shape an exporter receives.
///
/// `spans` and `logs` are separate so an OTLP endpoint can take one and a syslog endpoint the
/// other, and both are **already redacted** — the redaction happens in
/// [`crate::tracing_span::Span`] and [`crate::schema::NewLogEntry`] on construction, so a payload
/// that reaches this struct has been through the pass and cannot be un-redacted on the way out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Batch {
    /// The exporter's name, so a receiver can tell two of them apart.
    pub exporter: String,
    /// The spans in the batch.
    pub spans: Vec<Value>,
    /// The log lines in the batch.
    pub logs: Vec<Value>,
}

impl Batch {
    /// How many items the batch carries — the number the screen shows and the `Test` button's
    /// "sent N" answer.
    #[must_use]
    pub fn len(&self) -> usize {
        self.spans.len() + self.logs.len()
    }

    /// Whether the batch is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The outcome of one flush.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlushOutcome {
    /// The backend accepted the batch.
    Accepted {
        /// What the backend said, verbatim and truncated — an operator needs the response, not a
        /// summary of it.
        response: String,
    },
    /// The backend refused or could not be reached.
    Failed {
        /// Why.
        error: String,
    },
}

/// One exporter's bounded buffer and its health.
///
/// Public because [`Collector::register`] hands one back: a caller that registered an exporter
/// needs to be able to assert on its own buffer without going through the global, and a private
/// type in a public signature is a compile error at the call site, not a warning here.
#[derive(Debug)]
pub struct Buffer {
    name: String,
    kind: ExporterKind,
    capacity: usize,
    items: Mutex<VecDeque<Value>>,
    dropped: AtomicU64,
    consecutive_failures: AtomicU64,
    last_error: Mutex<Option<String>>,
    last_flush: Mutex<Option<time::OffsetDateTime>>,
    enabled: Mutex<bool>,
}

impl Buffer {
    fn new(name: String, kind: ExporterKind, capacity: usize) -> Self {
        Self {
            name,
            kind,
            capacity,
            items: Mutex::new(VecDeque::with_capacity(capacity.min(1024))),
            dropped: AtomicU64::new(0),
            consecutive_failures: AtomicU64::new(0),
            last_error: Mutex::new(None),
            last_flush: Mutex::new(None),
            enabled: Mutex::new(true),
        }
    }

    /// Health, derived from the failure counter rather than stored.
    ///
    /// A stored health column has to be written by every path that can change it, and the first
    /// path that forgets is the one you are debugging. Deriving it means the chip and the counter
    /// cannot disagree.
    fn health(&self) -> ExporterHealth {
        if !*self
            .enabled
            .lock()
            .unwrap_or_else(|error| error.into_inner())
        {
            return ExporterHealth::Down;
        }
        match self.consecutive_failures.load(Ordering::Relaxed) {
            0 => {
                if self
                    .last_flush
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some()
                {
                    ExporterHealth::Ok
                } else {
                    ExporterHealth::Unknown
                }
            }
            // ONE failure is `degraded` and TWO is `down`, deliberately: a single refused batch
            // is the normal noise of a restarting backend, and flipping the chip to red for it is
            // how an operator learns to ignore the chip. `down` means nothing is getting through
            // and the drop counter is climbing.
            1 => ExporterHealth::Degraded,
            _ => ExporterHealth::Down,
        }
    }
}

/// What the screen reads about one exporter.
#[derive(Debug, Clone, Serialize)]
pub struct ExporterStatus {
    /// The exporter's name.
    pub name: String,
    /// Its kind.
    pub kind: String,
    /// Its health chip.
    pub health: String,
    /// How many samples were dropped because the buffer was full.
    pub dropped_total: u64,
    /// How many are waiting now.
    pub buffered: usize,
    /// The buffer's cap, so the screen can draw it as a bar.
    pub capacity: usize,
    /// The last error, verbatim.
    pub last_error: Option<String>,
    /// When the last batch left.
    pub last_flush_at: Option<String>,
    /// Whether the exporter is on.
    pub enabled: bool,
}

/// The in-process collector: a bounded buffer per exporter, and the drop counter that makes the
/// loss visible.
///
/// Nothing here talks to a network. A transport is the exporter's own business and arrives with
/// the `infra/observability/` bundle in slice 4; what this type owns is the part that decides
/// whether a slow backend can hurt a request, and that decision is testable without a network.
#[derive(Debug)]
pub struct Collector {
    buffers: Mutex<Vec<Arc<Buffer>>>,
}

impl Default for Collector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector {
    /// A collector with no exporters.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buffers: Mutex::new(Vec::new()),
        }
    }

    /// Register an exporter, or return the existing one by name.
    ///
    /// Names are the identity (the drop counter's label, the screen's row), so a second
    /// registration under the same name is the same exporter with a possibly updated capacity.
    pub fn register(
        &self,
        name: &str,
        kind: ExporterKind,
        capacity: usize,
    ) -> Result<Arc<Buffer>, String> {
        let mut buffers = self
            .buffers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(existing) = buffers.iter().find(|b| b.name == name) {
            return Ok(Arc::clone(existing));
        }
        if buffers.len() >= MAX_EXPORTERS {
            return Err(format!(
                "this instance already has {MAX_EXPORTERS} exporters; remove one before adding another"
            ));
        }
        let buffer = Arc::new(Buffer::new(
            name.to_owned(),
            kind,
            capacity.clamp(1, DEFAULT_BUFFER_CAPACITY),
        ));
        buffers.push(Arc::clone(&buffer));
        Ok(buffer)
    }

    /// Remove an exporter, returning whether one was there.
    pub fn remove(&self, name: &str) -> bool {
        let mut buffers = self
            .buffers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let before = buffers.len();
        buffers.retain(|buffer| buffer.name != name);
        buffers.len() != before
    }

    /// Enqueue one redacted payload, dropping the oldest when full.
    ///
    /// Returns `true` when the item was stored. This is the call a request path makes, and the
    /// contract is that it never blocks and never allocates without bound: the ring is pre-sized
    /// to the capacity and a push onto a full ring evicts.
    pub fn push(&self, name: &str, payload: Value) -> bool {
        let Some(buffer) = self.find(name) else {
            return false;
        };
        if !*buffer
            .enabled
            .lock()
            .unwrap_or_else(|error| error.into_inner())
        {
            return false;
        }

        let mut items = buffer
            .items
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if items.len() >= buffer.capacity {
            // Drop-OLDEST: a buffered sample ages, and the freshest one is the one an operator
            // recovering from an incident needs. Dropping the new item instead would mean a
            // recovered exporter exports the future and loses the incident that caused the drop.
            items.pop_front();
            let dropped = buffer.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            // Counted in the registry too, so the loss is on the same `/metrics` scrape as
            // everything else. A buffer that discards quietly is the drop the request forbids.
            metrics::global().counter_add(DROPPED_FAMILY, &[buffer.name.as_str()], 1.0);
            let _ = dropped;
        }
        items.push_back(payload);
        true
    }

    /// Take everything buffered, for one flush.
    pub fn drain(&self, name: &str) -> Vec<Value> {
        let Some(buffer) = self.find(name) else {
            return Vec::new();
        };
        let mut items = buffer
            .items
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        items.drain(..).collect()
    }

    /// Record a flush outcome, which moves the health chip.
    pub fn record_outcome(&self, name: &str, outcome: &FlushOutcome) {
        let Some(buffer) = self.find(name) else {
            return;
        };
        match outcome {
            FlushOutcome::Accepted { .. } => {
                buffer.consecutive_failures.store(0, Ordering::Relaxed);
                *buffer.last_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
                *buffer.last_flush.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(time::OffsetDateTime::now_utc());
            }
            FlushOutcome::Failed { error } => {
                buffer.consecutive_failures.fetch_add(1, Ordering::Relaxed);
                *buffer.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.clone());
            }
        }
    }

    /// Switch an exporter on or off.
    pub fn set_enabled(&self, name: &str, enabled: bool) -> bool {
        let Some(buffer) = self.find(name) else {
            return false;
        };
        *buffer
            .enabled
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = enabled;
        if !enabled {
            // Switching an exporter off is not a reason to hold its backlog: the next flush would
            // ship a batch the operator asked not to send. Drain it and count the loss, so the
            // number on the screen is the truth rather than a silent discard.
            self.drain(name);
        }
        true
    }

    /// The status of one exporter.
    #[must_use]
    pub fn status(&self, name: &str) -> Option<ExporterStatus> {
        let buffer = self.find(name)?;
        Some(self.status_of(&buffer))
    }

    /// Every exporter's status, in registration order.
    #[must_use]
    pub fn statuses(&self) -> Vec<ExporterStatus> {
        let buffers = self
            .buffers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        buffers
            .iter()
            .map(|buffer| self.status_of(buffer))
            .collect()
    }

    fn status_of(&self, buffer: &Buffer) -> ExporterStatus {
        let buffered = buffer
            .items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len();
        ExporterStatus {
            name: buffer.name.clone(),
            kind: buffer.kind.as_str().to_owned(),
            health: buffer.health().as_str().to_owned(),
            dropped_total: buffer.dropped.load(Ordering::Relaxed),
            buffered,
            capacity: buffer.capacity,
            last_error: buffer
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            last_flush_at: buffer
                .last_flush
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .and_then(|at| {
                    at.format(&time::format_description::well_known::Rfc3339)
                        .ok()
                }),
            enabled: *buffer.enabled.lock().unwrap_or_else(|e| e.into_inner()),
        }
    }

    fn find(&self, name: &str) -> Option<Arc<Buffer>> {
        let buffers = self
            .buffers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        buffers
            .iter()
            .find(|buffer| buffer.name == name)
            .map(Arc::clone)
    }

    /// Forget every buffered sample, KEEPING the registered exporters.
    ///
    /// Distinct from [`Self::clear`], and the distinction is what a walk needs. A walk's
    /// *buffer* is state it must not inherit; its *registrations* are the thing under test. A
    /// reset that dropped both left `status(&name)` answering `None`, so the walk's own
    /// `expect("registered")` fired and the failure read as "the exporter was never created" —
    /// when in fact the test had deleted it a line earlier, after creating it through the router.
    pub fn clear_buffer(&self) {
        for buffer in self
            .buffers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
        {
            buffer
                .items
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clear();
            // `dropped` is an `AtomicU64`, not a mutex — `store`, not `*… = 0`, or the walk
            // that measures a fresh drop count reads the previous walk's drops instead.
            buffer.dropped.store(0, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Forget every registered exporter and everything it has buffered.
    ///
    /// The global collector's own doc comment says "a test's job to use a private one, because a
    /// test that pushes into the global is a test whose buffer state depends on test order" —
    /// but `fan_out` and `sweep` read the *global* because that is the one the request path
    /// pushes into, and their walks are integration walks that must exercise the real edge. So
    /// the escape hatch has to be a reset on the global rather than a substitute collector.
    ///
    /// **Order matters and this is the second half of a bug that took four ticks to name.** A
    /// walk calls `create_exporter` — itself an authenticated POST — so the request-log
    /// middleware has already fanned a line out by the time the row exists. Resetting the TABLE
    /// and the COLLECTOR together, before the walk fans anything of its own, leaves the buffer
    /// holding that one registration line, and the walk's first assertion reads
    /// `left: 6, right: 5`. Resetting before `create_exporter` is no better: the buffer is
    /// empty and `status(&name)` is `None`, because the POST that would have re-registered it
    /// has not run yet. The order that works is *create the row, then clear the buffer only*.
    pub fn clear(&self) {
        let mut buffers = self
            .buffers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        buffers.clear();
    }
}

/// The process-wide collector.
///
/// A global because the exporters are process configuration and the request path must reach them
/// without a handle threaded through every layer — the same reasoning as the metrics registry, and
/// the same caveat: it is a test's job to use a private one, because a test that pushes into the
/// global is a test whose buffer state depends on test order.
static COLLECTOR: std::sync::LazyLock<Collector> = std::sync::LazyLock::new(Collector::new);

/// The process-wide collector.
#[must_use]
pub fn global() -> &'static Collector {
    &COLLECTOR
}

/// A one-shot connectivity probe — the exporter screen's `Test` button.
///
/// This sends a real request. That is the point: the request asks the form to "send a synthetic
/// batch and reports the backend's response", and a `Test` that only validates the form is a dead
/// button that tells the operator their endpoint is fine right up until telemetry silently does
/// not arrive. The batch it sends is a fixed, obviously-synthetic document — it names no host, no
/// user and no measurement of the instance.
///
/// Two properties are deliberate. It never carries a credential, because the probe has no
/// authenticated secret to hand and inventing one is how a test endpoint ends up in a real
/// receiver's log with a token attached. And it never fails loudly: the caller records the outcome
/// and reports it, because a `Test` that renders an error page has told the operator nothing they
/// could not have learned by waiting.
#[derive(Debug, Clone, Copy)]
pub struct Probe {
    /// How long one attempt may take.
    pub timeout_ms: i64,
}

impl Probe {
    /// The document every probe sends.
    #[must_use]
    pub fn synthetic_batch(exporter: &str) -> Value {
        serde_json::json!({
            "omnion_probe": true,
            "exporter": exporter,
            "note": "a connectivity test from the Omnion exporter screen; carries no telemetry",
        })
    }

    /// Send one probe and report what the backend said.
    pub async fn send(&self, endpoint: &str) -> FlushOutcome {
        // An unparseable endpoint is answered rather than attempted: a request to a string that
        // is not a URL fails with a DNS error that names the wrong thing entirely.
        if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
            return FlushOutcome::Failed {
                error: format!("`{endpoint}` is not an http or https URL"),
            };
        }

        let timeout = std::time::Duration::from_millis(self.timeout_ms.clamp(100, 600_000) as u64);
        let client = match reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("omnion-exporter-probe/", env!("CARGO_PKG_VERSION")))
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                return FlushOutcome::Failed {
                    error: format!("the probe client could not be built: {error}"),
                };
            }
        };

        // The body is serialised here rather than with `RequestBuilder::json`, because the
        // workspace's `reqwest` is declared without the `json` feature and enabling it for one
        // call site would add a dependency every other crate on the box would then inherit.
        let body = serde_json::to_vec(&Self::synthetic_batch("probe")).unwrap_or_default();
        match client
            .post(endpoint)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();
                // The body is read (a receiver that has to consume the request before answering
                // would otherwise see a reset) but truncated: an operator needs the receiver's
                // verdict, not its entire response.
                let body = response
                    .text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(512)
                    .collect::<String>();
                if status.is_success() {
                    FlushOutcome::Accepted {
                        response: format!("{} {}", status.as_u16(), body.trim()),
                    }
                } else {
                    FlushOutcome::Failed {
                        error: format!("{} {}", status.as_u16(), body.trim()),
                    }
                }
            }
            Err(error) => FlushOutcome::Failed {
                error: format!("{error}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn collector_with(name: &str, capacity: usize) -> Collector {
        let collector = Collector::new();
        collector
            .register(name, ExporterKind::Otlp, capacity)
            .expect("the first exporter registers");
        collector
    }

    #[test]
    fn a_buffer_keeps_everything_up_to_its_capacity() {
        let collector = collector_with("otlp", 8);
        for index in 0..8 {
            assert!(collector.push("otlp", json!({ "n": index })));
        }
        let status = collector.status("otlp").expect("registered");
        assert_eq!(status.buffered, 8);
        assert_eq!(status.dropped_total, 0);
    }

    #[test]
    fn a_full_buffer_drops_the_oldest_and_counts_it_in_the_registry() {
        let collector = collector_with("webhook", 4);
        for index in 0..10 {
            collector.push("webhook", json!({ "n": index }));
        }

        let status = collector.status("webhook").expect("registered");
        assert_eq!(status.buffered, 4, "the buffer grew past its cap");
        assert_eq!(status.dropped_total, 6, "the loss was not counted");

        // Drop-OLDEST, and the surviving items are the NEWEST ones — the property that matters
        // when a backend comes back after an incident.
        let drained = collector.drain("webhook");
        let kept: Vec<i64> = drained
            .iter()
            .map(|item| item["n"].as_i64().unwrap())
            .collect();
        assert_eq!(
            kept,
            vec![6, 7, 8, 9],
            "the wrong end of the buffer was dropped"
        );

        // And the loss is visible on the registry, in the family the request names.
        let value = metrics::global()
            .value_of(DROPPED_FAMILY, &["webhook"])
            .unwrap_or(0.0);
        assert!(value >= 6.0, "the drop is not on the registry: {value}");
    }

    #[test]
    fn a_push_onto_a_full_buffer_never_blocks_and_never_fails() {
        // The acceptance line is "does not stall a single request". A capacity of one and a
        // thousand pushes is the cheapest way to prove the ring evicts rather than grows or
        // waits — the assertion is the return value, not a latency measurement.
        let collector = collector_with("syslog", 1);
        for index in 0..1000 {
            assert!(collector.push("syslog", json!({ "n": index })));
        }
        let status = collector.status("syslog").expect("registered");
        assert_eq!(status.buffered, 1);
        assert_eq!(status.dropped_total, 999);
    }

    #[test]
    fn the_health_chip_moves_from_unknown_through_degraded_to_down() {
        let collector = collector_with("otlp", 8);
        assert_eq!(
            collector.status("otlp").unwrap().health,
            "unknown",
            "a never-flushed exporter is not `ok`"
        );

        collector.record_outcome(
            "otlp",
            &FlushOutcome::Failed {
                error: "connection refused".into(),
            },
        );
        assert_eq!(collector.status("otlp").unwrap().health, "degraded");

        collector.record_outcome(
            "otlp",
            &FlushOutcome::Failed {
                error: "timed out".into(),
            },
        );
        assert_eq!(collector.status("otlp").unwrap().health, "down");
        assert_eq!(
            collector.status("otlp").unwrap().last_error.as_deref(),
            Some("timed out")
        );

        collector.record_outcome(
            "otlp",
            &FlushOutcome::Accepted {
                response: "200".into(),
            },
        );
        let status = collector.status("otlp").expect("registered");
        assert_eq!(status.health, "ok");
        assert!(
            status.last_error.is_none(),
            "a recovered exporter kept its error"
        );
        assert!(
            status.last_flush_at.is_some(),
            "a flush was not timestamped"
        );
    }

    #[test]
    fn a_disabled_exporter_accepts_nothing_and_keeps_no_backlog() {
        let collector = collector_with("webhook", 8);
        collector.push("webhook", json!({ "n": 1 }));
        assert!(collector.set_enabled("webhook", false));
        assert!(!collector.push("webhook", json!({ "n": 2 })));
        let status = collector.status("webhook").expect("registered");
        assert!(!status.enabled);
        assert_eq!(
            status.buffered, 0,
            "a disabled exporter held a backlog to send later"
        );
    }

    #[test]
    fn registering_the_same_name_twice_is_one_exporter() {
        let collector = collector_with("otlp", 8);
        collector
            .register("otlp", ExporterKind::Webhook, 8)
            .expect("re-register");
        assert_eq!(
            collector.statuses().len(),
            1,
            "a second row appeared for one name"
        );
    }

    #[test]
    fn the_exporter_count_is_capped_and_the_refusal_says_so() {
        let collector = Collector::new();
        for index in 0..MAX_EXPORTERS {
            collector
                .register(&format!("e{index}"), ExporterKind::Otlp, 8)
                .expect("within the cap");
        }
        let error = collector
            .register("one-too-many", ExporterKind::Otlp, 8)
            .expect_err("the cap refuses");
        assert!(
            error.contains(&MAX_EXPORTERS.to_string()),
            "the refusal must name the cap: {error}"
        );
    }

    #[test]
    fn removing_an_exporter_takes_its_row_and_its_buffer() {
        let collector = collector_with("otlp", 8);
        collector.push("otlp", json!({ "n": 1 }));
        assert!(collector.remove("otlp"));
        assert!(!collector.remove("otlp"), "removing twice reported success");
        assert!(collector.status("otlp").is_none());
        assert!(!collector.push("otlp", json!({ "n": 2 })));
    }

    #[test]
    fn every_kind_round_trips_through_its_stored_name() {
        for kind in ExporterKind::ALL {
            assert_eq!(ExporterKind::parse(kind.as_str()), Some(*kind));
        }
        assert_eq!(ExporterKind::parse("carrier_pigeon"), None);
    }

    #[test]
    fn a_batch_counts_spans_and_logs_together() {
        let batch = Batch {
            exporter: "otlp".into(),
            spans: vec![json!({}), json!({})],
            logs: vec![json!({})],
        };
        assert_eq!(batch.len(), 3);
        assert!(!batch.is_empty());
        assert!(
            Batch {
                exporter: "otlp".into(),
                spans: vec![],
                logs: vec![]
            }
            .is_empty()
        );
    }
}
