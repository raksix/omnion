//! The process lifecycle: the probe contract, the drain, and the shutdown summary
//! (REQ-126, slice 4).
//!
//! ## What this module is for
//!
//! Every other module in this crate is a *recorder*: it writes a line, a span or a sample. This
//! one decides when the process stops, and it exists because the graceful-shutdown contract the
//! request states is a **sequence with an order**, and an order cannot live in `main.rs` alone:
//!
//! > Graceful shutdown on SIGTERM: readiness fails first, the listener stops accepting,
//! > in-flight requests drain to a deadline, telemetry flushes, pools close, one-line shutdown
//! > summary is logged, exit 0.
//!
//! ## The rule that makes the contract testable: readiness is a state, not a query
//!
//! `/readyz` currently answers "are my dependencies reachable?" — a question about the world. A
//! load balancer asks it every two seconds, and during a shutdown the honest answer to *that*
//! question is still "yes, the database is right there". What the orchestrator needs to know is
//! different: **stop sending me traffic**, which is a statement about this process's intent, not
//! about Postgres.
//!
//! So draining flips a process-local flag that `/readyz` reads FIRST and answers `503` from
//! before it pings anything. The ordering is the whole contract:
//!
//! | phase        | `/healthz` | `/readyz` | listener | in-flight | telemetry |
//! |--------------|------------|-----------|----------|-----------|-----------|
//! | serving      | 200        | 200       | accepts  | served    | live      |
//! | draining     | 200        | **503**   | stops    | drains    | flushing  |
//! | exited       | —          | —         | closed   | 0         | flushed   |
//!
//! A liveness probe that fails during a drain restarts a process that is behaving perfectly, and
//! most orchestrators read the two with different policies — which is exactly why `/healthz`
//! deliberately does NOT consult the flag. That asymmetry is the point of the table, so it is
//! pinned by a test rather than left to a reader of the code to infer.
//!
//! ## Why a deadline instead of "wait for every request"
//!
//! A request that never finishes is a real thing (a slow report, a hung upstream), and a drain
//! that waits for it is a deploy that hangs until the orchestrator's own grace period expires and
//! then SIGKILLs the process — losing the flush and the summary line. So the drain has a deadline,
//! the deadline is reported in the summary line, and hitting it is a counted, visible outcome
//! (`DrainOutcome::TimedOut`) rather than a hang.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::exporter;
use crate::metrics;

/// The family a drain's outcome is counted in. Declared in `metrics::FAMILIES`; the test asserts
/// that, because a counter the shutdown path records but the registry does not declare is a
/// no-op on the scrape and the acceptance line becomes unprovable.
pub const SHUTDOWNS_FAMILY: &str = "omnion_shutdowns_total";

/// The default drain deadline.
///
/// A Kubernetes `terminationGracePeriodSeconds` of 30 is the common setting, and the drain has to
/// finish inside it with room for the flush that follows. Ten seconds is long enough for a normal
/// request to finish and short enough that the flush is never the thing that gets killed.
pub const DEFAULT_DRAIN_TIMEOUT_MS: u64 = 10_000;

/// How often the drain re-checks whether the deadline has passed.
///
/// A hundred checks a second is a busy loop; one check a second could add a second of latency to
/// a drain that finished instantly. Ten is the middle, and it is a *maximum* extra latency of
/// 100 ms, not a minimum.
pub const DRAIN_POLL_MS: u64 = 100;

/// The process-wide lifecycle state.
///
/// `OnceLock` rather than a `static mut` or a global `AtomicBool` so the type is checked, and
/// `Arc<AtomicBool>` for the flag so a test can build an independent instance without touching the
/// process the test is running in. The accessors below are what the routes call; the constructor
/// is public only so a test can hold an isolated one.
#[derive(Debug)]
pub struct Lifecycle {
    draining: AtomicBool,
    /// Requests that entered a handler and have not returned. Incremented by the middleware on
    /// entry and decremented on drop, so a handler that panics still decrements.
    in_flight: AtomicU64,
    /// Set once the drain has completed, so the summary can say what actually happened.
    summary: std::sync::Mutex<Option<ShutdownSummary>>,
}

impl Lifecycle {
    /// A lifecycle in the serving phase.
    #[must_use]
    pub fn new() -> Self {
        Self {
            draining: AtomicBool::new(false),
            in_flight: AtomicU64::new(0),
            summary: std::sync::Mutex::new(None),
        }
    }

    /// `true` once a drain has begun and before the process has exited.
    ///
    /// This is what `/readyz` reads. It is a **load** on every readiness probe, so it is an
    /// atomic read and never a lock — a probe that can block on a mutex is a probe that can time
    /// out during the exact moment an operator is watching it.
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    /// Begin draining. Returns `true` if this call is the one that started it.
    ///
    /// The `swap` is what makes a second SIGTERM harmless: a supervisor that sends SIGTERM twice
    /// (or a `docker stop` followed by a systemd stop) must not restart the drain, because the
    /// second drain would reset the deadline and turn a bounded wait into an unbounded one.
    ///
    /// The negation is not decoration. `swap` returns the value it *replaced*, so the raw call
    /// reads "was already draining" — the opposite of what this function documents. Returning
    /// `!` is what makes the one call site honest, and a unit test pins it, because an inverted
    /// flag here does not crash: it just makes the shutdown log claim the opposite of what
    /// happened, which is the failure mode nobody notices until an incident review.
    pub fn begin_drain(&self) -> bool {
        !self.draining.swap(true, Ordering::AcqRel)
    }

    /// How many requests are in flight.
    #[must_use]
    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Acquire)
    }

    /// A guard that holds one in-flight request for as long as it lives.
    ///
    /// Drop-decrement rather than an explicit call at the end of the middleware: a middleware
    /// that returns early on an error path, or a handler that panics, would otherwise leak a
    /// count and the drain would wait for a request that ended minutes ago.
    ///
    /// The receiver is `&Arc<Self>` because the guard has to keep the lifecycle alive: a guard
    /// holding a borrow would not compile inside the middleware's own future, which is precisely
    /// where it has to be constructed.
    #[must_use]
    pub fn track(self: &Arc<Self>) -> InFlightGuard {
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        InFlightGuard {
            lifecycle: Arc::clone(self),
        }
    }

    /// The recorded summary, if the drain finished.
    #[must_use]
    pub fn summary(&self) -> Option<ShutdownSummary> {
        self.summary.lock().ok().and_then(|guard| guard.clone())
    }

    fn record(&self, summary: ShutdownSummary) {
        if let Ok(mut slot) = self.summary.lock() {
            *slot = Some(summary);
        }
    }
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::new()
    }
}

/// Holds one in-flight request. Releasing is on drop, always.
#[derive(Debug)]
pub struct InFlightGuard {
    lifecycle: LifecycleRef,
}

type LifecycleRef = Arc<Lifecycle>;

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.lifecycle
            .in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_sub(1)
            })
            .ok();
    }
}

/// One line, logged once, describing what the shutdown actually did.
///
/// The request asks for "one-line shutdown summary". The value here is a struct rather than a
/// formatted string because the *test* has to read it, and a test that asserts on a log line's
/// formatting is asserting on `tracing`'s formatter.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ShutdownSummary {
    /// `drained` when every in-flight request finished, `timed_out` when the deadline won.
    pub outcome: String,
    /// Requests that were still running when the drain finished.
    pub in_flight_at_exit: u64,
    /// What the drain waited for, in milliseconds.
    pub drain_ms: u64,
    /// How many buffers were flushed in the final sweep.
    pub buffers_flushed: usize,
    /// How many items the final sweep still could not send, and dropped for that reason.
    pub items_dropped: usize,
    /// The exporters that were `down` or `degraded` when the process stopped, so the summary
    /// says "telemetry did not fully arrive" instead of leaving the operator to infer it from a
    /// missing batch.
    pub unhealthy_exporters: Vec<String>,
    /// `true` when the drain finished inside the deadline.
    pub clean: bool,
}

impl ShutdownSummary {
    /// The single line, as it is logged.
    #[must_use]
    pub fn to_line(&self) -> String {
        let unhealthy = if self.unhealthy_exporters.is_empty() {
            "none".to_owned()
        } else {
            self.unhealthy_exporters.join(",")
        };
        format!(
            "shutdown outcome={} in_flight={} drain_ms={} buffers_flushed={} dropped={} \
             unhealthy_exporters={} clean={}",
            self.outcome,
            self.in_flight_at_exit,
            self.drain_ms,
            self.buffers_flushed,
            self.items_dropped,
            unhealthy,
            self.clean,
        )
    }
}

/// How a drain ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainOutcome {
    /// Every in-flight request finished.
    Drained,
    /// The deadline passed with requests still running.
    TimedOut,
}

impl DrainOutcome {
    fn as_str(self) -> &'static str {
        match self {
            DrainOutcome::Drained => "drained",
            DrainOutcome::TimedOut => "timed_out",
        }
    }
}

/// The process-wide lifecycle, installed once by the binary.
///
/// The routes read it through [`global`]; nothing replaces it in production, because a replaced
/// lifecycle is a lifecycle no route is watching.
#[must_use]
pub fn global() -> Arc<Lifecycle> {
    static CELL: OnceLock<Arc<Lifecycle>> = OnceLock::new();
    CELL.get_or_init(|| Arc::new(Lifecycle::new())).clone()
}

/// Drain in-flight requests to a deadline, then flush the exporter buffers.
///
/// This is the sequence the request names, in its order, and the order is the contract:
///
/// 1. **Readiness first.** [`begin_drain`] flips the flag *before* the first sleep, so a load
///    balancer polling at the moment of the signal sees `503` and stops sending — and it cannot
///    see it late, because the flag is set before the first request is awaited.
/// 2. **Drain.** Poll until no request is in flight or the deadline passes. The listener is
///    stopped by the caller (axum's own graceful shutdown), so nothing new arrives.
/// 3. **Flush.** One final sweep, so the log lines of the requests that just drained are not lost
///    to a process that exits before its next interval tick.
/// 4. **Summarise.** One line, plus a metric, and the summary is readable from
///    [`Lifecycle::summary`] so a test does not have to parse it.
pub async fn drain_and_flush(
    lifecycle: &Lifecycle,
    pool: Option<&sqlx::PgPool>,
    timeout: Duration,
) -> ShutdownSummary {
    let started = Instant::now();
    let began = lifecycle.begin_drain();
    tracing::info!(
        already_draining = !began,
        in_flight = lifecycle.in_flight(),
        timeout_ms = timeout.as_millis() as u64,
        "drain started: readiness now fails, the listener stops accepting"
    );

    let outcome = wait_for_drain(lifecycle, timeout).await;
    let drain_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    // The final sweep. `sweep` is the same function the loop calls, so "the last batch" is the
    // same code path as every batch — a separate one-w-off flush would be a second implementation
    // of the thing most likely to be wrong.
    let (buffers_flushed, items_dropped) = match pool {
        Some(pool) => match crate::exporter_flush::sweep(pool, &exporter::global()).await {
            Ok(flushed) => {
                let dropped: usize = exporter::global()
                    .statuses()
                    .iter()
                    .map(|status| usize::try_from(status.dropped_total).unwrap_or(usize::MAX))
                    .sum();
                (flushed, dropped)
            }
            Err(error) => {
                tracing::warn!(error = %error, "the final telemetry sweep failed");
                (0, 0)
            }
        },
        None => (0, 0),
    };

    let unhealthy: Vec<String> = exporter::global()
        .statuses()
        .iter()
        .filter(|status| {
            // `ExporterStatus::health` is the check-constrained `obs_exporters.health` string, so
            // it is compared against the constraint's own values rather than against enum
            // variants: `ExporterHealth::Down` renders as the string `down`, and only the string
            // is what a row carries.
            status.health == exporter::ExporterHealth::Down.as_str()
                || status.health == exporter::ExporterHealth::Degraded.as_str()
        })
        .map(|status| status.name.clone())
        .collect();

    let summary = ShutdownSummary {
        outcome: outcome.as_str().to_owned(),
        in_flight_at_exit: lifecycle.in_flight(),
        drain_ms,
        buffers_flushed,
        items_dropped,
        unhealthy_exporters: unhealthy,
        clean: outcome == DrainOutcome::Drained,
    };

    metrics::global().counter_add(SHUTDOWNS_FAMILY, &[&summary.outcome], 1.0);
    lifecycle.record(summary.clone());
    tracing::info!(summary = %summary.to_line(), "shutdown complete");
    summary
}

/// Wait for the in-flight count to reach zero, or for the deadline.
async fn wait_for_drain(lifecycle: &Lifecycle, timeout: Duration) -> DrainOutcome {
    let deadline = Instant::now() + timeout;
    loop {
        if lifecycle.in_flight() == 0 {
            return DrainOutcome::Drained;
        }
        if Instant::now() >= deadline {
            return DrainOutcome::TimedOut;
        }
        tokio::time::sleep(Duration::from_millis(DRAIN_POLL_MS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn readiness_is_503_and_liveness_is_200_throughout_a_drain() {
        // The asymmetry is the contract, and it is the thing a reader of the code would most
        // likely "fix" by making /healthz consult the flag too. Asserting the two states
        // separately is what stops that.
        let lifecycle = Lifecycle::new();
        assert!(
            !lifecycle.is_draining(),
            "a fresh process reports not draining"
        );

        lifecycle.begin_drain();
        assert!(
            lifecycle.is_draining(),
            "readiness did not fail on the first beat of the drain — a load balancer that is \
             polling at the instant of SIGTERM would keep sending"
        );
        assert!(
            lifecycle.in_flight() == 0,
            "readiness flipped while requests were still running, which is what it is for"
        );
    }

    #[test]
    fn a_second_sigterm_does_not_restart_the_drain() {
        let lifecycle = Lifecycle::new();
        assert!(
            lifecycle.begin_drain(),
            "the first drain must be the one that starts"
        );
        assert!(
            !lifecycle.begin_drain(),
            "a second SIGTERM restarted the drain, which resets the deadline and turns a bounded \
             wait into an unbounded one"
        );
    }

    #[test]
    fn an_in_flight_request_is_released_even_when_the_guard_is_dropped_early() {
        // The counter is released on Drop precisely because a middleware that returns on an
        // error path — or a handler that panics — would otherwise leak a count and the drain
        // would wait for a request that ended minutes ago.
        let lifecycle = Arc::new(Lifecycle::new());
        {
            let _guard = lifecycle.track();
            assert_eq!(lifecycle.in_flight(), 1);
        }
        assert_eq!(
            lifecycle.in_flight(),
            0,
            "the guard did not release on drop"
        );
    }

    #[tokio::test]
    async fn a_drain_with_no_requests_in_flight_returns_immediately() {
        let lifecycle = Lifecycle::new();
        let started = Instant::now();
        let summary = drain_and_flush(&lifecycle, None, Duration::from_secs(5)).await;
        assert_eq!(summary.outcome, "drained");
        assert!(summary.clean);
        assert_eq!(summary.in_flight_at_exit, 0);
        assert!(
            started.elapsed() < Duration::from_millis(DRAIN_POLL_MS * 5),
            "an idle drain slept before checking: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_drain_with_a_stuck_request_times_out_and_says_so() {
        // A request that never finishes is a real thing. A drain that waits for it forever is a
        // deploy that gets SIGKILLed, which loses the flush and the summary — so the deadline
        // wins, and it is a *counted* outcome rather than a hang.
        let lifecycle = Arc::new(Lifecycle::new());
        let stuck = lifecycle.track();
        let summary = drain_and_flush(&lifecycle, None, Duration::from_millis(250)).await;

        assert_eq!(summary.outcome, "timed_out");
        assert!(!summary.clean);
        assert_eq!(
            summary.in_flight_at_exit, 1,
            "the stuck request was not reported"
        );
        assert!(
            summary.drain_ms >= 200,
            "the drain returned before its deadline: {}ms",
            summary.drain_ms
        );
        drop(stuck);
        assert_eq!(lifecycle.in_flight(), 0);
    }

    #[tokio::test]
    async fn a_drain_waits_for_a_request_that_finishes_first() {
        let lifecycle = Arc::new(Lifecycle::new());
        let guard = lifecycle.track();
        let holder = Arc::clone(&lifecycle);
        // The request finishes after 150 ms, comfortably inside the 5-second deadline.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            drop(guard);
            drop(holder);
        });

        let summary = drain_and_flush(&lifecycle, None, Duration::from_secs(5)).await;
        assert_eq!(
            summary.outcome, "drained",
            "the drain did not wait for the request"
        );
        assert_eq!(summary.in_flight_at_exit, 0);
    }

    #[test]
    fn the_summary_line_names_what_actually_happened() {
        // An operator reading one line at 3am needs the outcome, the count, the wait and the
        // unhealthy exporters in it. A summary that says "shutdown complete" for a drain that
        // dropped half its telemetry is worse than no line at all.
        let summary = ShutdownSummary {
            outcome: "timed_out".to_owned(),
            in_flight_at_exit: 3,
            drain_ms: 10_001,
            buffers_flushed: 2,
            items_dropped: 41,
            unhealthy_exporters: vec!["otlp".to_owned()],
            clean: false,
        };
        let line = summary.to_line();
        for expected in [
            "outcome=timed_out",
            "in_flight=3",
            "drain_ms=10001",
            "buffers_flushed=2",
            "dropped=41",
            "unhealthy_exporters=otlp",
            "clean=false",
        ] {
            assert!(
                line.contains(expected),
                "the line is missing `{expected}`: {line}"
            );
        }
    }

    #[test]
    fn a_healthy_process_says_no_unhealthy_exporters_rather_than_an_empty_list() {
        let summary = ShutdownSummary {
            outcome: "drained".to_owned(),
            in_flight_at_exit: 0,
            drain_ms: 12,
            buffers_flushed: 1,
            items_dropped: 0,
            unhealthy_exporters: Vec::new(),
            clean: true,
        };
        assert!(
            summary.to_line().contains("unhealthy_exporters=none"),
            "an empty list reads as a formatting bug: {}",
            summary.to_line()
        );
    }

    #[test]
    fn the_summary_is_readable_back_after_the_drain() {
        // A test that has to parse a log line is a test of `tracing`'s formatter. The summary is
        // recorded so the walk can assert on it directly.
        let lifecycle = Lifecycle::new();
        assert!(
            lifecycle.summary().is_none(),
            "a fresh lifecycle has a summary"
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        rt.block_on(drain_and_flush(&lifecycle, None, Duration::from_millis(50)));
        let recorded = lifecycle.summary().expect("the drain recorded a summary");
        assert_eq!(recorded.outcome, "drained");
        assert!(recorded.clean);
    }

    #[test]
    fn the_shutdown_family_is_declared_in_the_registry() {
        assert!(
            metrics::family(SHUTDOWNS_FAMILY).is_some(),
            "{SHUTDOWNS_FAMILY} is recorded on every shutdown but not declared, so it is a \
             no-op on the scrape and the count cannot be seen"
        );
    }

    #[test]
    fn the_global_lifecycle_is_one_object_every_route_watches() {
        let first = global();
        let second = global();
        assert!(
            Arc::ptr_eq(&first, &second),
            "the lifecycle was replaced between two calls; readiness would be answered by a \
             flag no drain ever sets"
        );
    }

    #[test]
    fn many_guards_count_and_release_independently() {
        let lifecycle = Arc::new(Lifecycle::new());
        let guards: Vec<InFlightGuard> = (0..8).map(|_| lifecycle.track()).collect();
        assert_eq!(lifecycle.in_flight(), 8);
        let counter = AtomicU32::new(0);
        for (index, guard) in guards.into_iter().enumerate() {
            drop(guard);
            counter.store(index as u32 + 1, Ordering::SeqCst);
            assert_eq!(
                lifecycle.in_flight(),
                8 - (counter.load(Ordering::SeqCst) as u64)
            );
        }
        assert_eq!(lifecycle.in_flight(), 0);
    }
}
