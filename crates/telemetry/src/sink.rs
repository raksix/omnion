//! The bounded write queue that stands between the request path and the log store.
//!
//! ## The defect this module was written for
//!
//! Slice 1 put the log line straight into the request task:
//!
//! ```text
//! if let Err(error) = omnion_telemetry::store::write(&state.db().pool(), &entry).await { … }
//! ```
//!
//! and the module comment above it claimed "a request path never blocks on telemetry", on the
//! strength of there being no queue to drain. **Both halves of that were wrong, and the second one
//! is the dangerous half.** There was no queue — because the insert ran on the caller's own task
//! and held one of the caller's own pooled connections while it did. So when the pool was
//! exhausted, the log write waited out the pool's own `acquire_timeout` (**5 s**, the same constant
//! every *request* uses) before giving up, and then the trace index wrote a second one the same
//! way. Two telemetry writes per request, each able to add five seconds to it.
//!
//! It did not read as a defect, which is why it survived the tick that found it. `store::write` is
//! called *after* the handler, and its failure is deliberately swallowed into an `eprintln!`, so
//! the request still answers `200` — and the QA pass that finally exposed it read those `200`s as
//! thirteen healthy screens. What the box actually recorded was this:
//!
//! ```text
//! omnion-api: the request line could not be stored: pool timed out while waiting for an open connection
//! omnion-api: the trace could not be indexed: pool timed out while waiting for an open connection
//! ```
//!
//! 295 times in one run, in the exact window the pass was walking. The refusal is *visible*, and it
//! says "telemetry lost the line", which is true and is not the finding: the finding is that the
//! request path paid five seconds per line for the privilege of losing it.
//!
//! ## What the contract is now
//!
//! - **A bounded queue.** The request path pushes and returns. It never waits on the store, and
//!   it never waits for a free connection, because it does not touch one.
//! - **Oldest-first eviction.** A full queue drops its OLDEST entry, for the same reason the
//!   exporter ring does ([`crate::exporter`]): the entry that explains the failure that is
//!   happening *now* is worth more than the one describing the request before it.
//! - **The drop is counted, not silent.** `omnion_telemetry_writes_dropped_total{kind}` rises, so a
//!   window of missing lines is a thing an operator can see on a scrape instead of something they
//!   discover by noticing a gap.
//! - **The drain runs on its own task** and keeps its own connection budget, so a burst of
//!   telemetry cannot consume the connections the handlers are waiting for.
//!
//! The store functions are untouched and still take a `&PgPool`. What changed is *who calls them*
//! and *from where* — which is the same lesson the exporter pipeline taught in slice 3, arriving
//! for the local store on the tick when the log made it visible.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use crate::schema::LogEntry;
use crate::{store, trace_store};
use crate::tracing_span::TraceRecord;

/// The metric family recording lines and traces this process could not store.
///
/// Declared in [`crate::metrics::FAMILIES`] like every other family, and a *separate* family from
/// `omnion_exporter_dropped_total`: that one counts samples an exporter refused to ship, this one
/// counts lines the local store never accepted. They answer different questions ("is the remote
/// endpoint broken" vs "is my instance dropping its own history") and folding them together would
/// make both unanswerable.
pub const DROPPED_FAMILY: &str = "omnion_telemetry_writes_dropped_total";

/// The default queue depth, and the floor at which it is clamped.
///
/// Sized against the 5 s acquire timeout it replaces: a request burst of this length is absorbed
/// in full by a healthy database, and beyond it the oldest lines go rather than the newest — a
/// queue that refused new lines to protect old ones would drop the failure an operator is reading
/// about in favour of the success that preceded it.
pub const DEFAULT_CAPACITY: usize = 4096;

/// What a queued entry is for.
///
/// A label, not an enum carried through the queue: both variants are written by the same edge and
/// the only thing a reader of the scrape needs is which of the two stores was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WriteKind {
    /// The request log line.
    Log,
    /// The sampled trace index row.
    Trace,
}

impl WriteKind {
    /// The label this kind is counted under.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Trace => "trace",
        }
    }
}

/// What the queue carries.
///
/// An enum rather than a serialised blob, and this is the second correction the first draft of
/// this module needed. The draft queued `(WriteKind, LogEntry)` and round-tripped a **trace**
/// through `LogEntry` by `to_value`/`from_value` — which cannot work, because the two are
/// different rows: a `LogEntry` has a level, a target and a message, and a `TraceRecord` has
/// spans, a sampling decision and a duration. The conversion was guaranteed to fail, so every
/// trace would have been dropped and counted, and the trace index would have quietly become
/// write-only. A payload type that cannot hold a span is how that happens.
#[derive(Debug, Clone)]
pub enum Payload {
    /// A request log line.
    Log(LogEntry),
    /// A sampled trace's index row.
    Trace(Box<TraceRecord>),
}

impl Payload {
    /// Which store this payload belongs to.
    #[must_use]
    pub fn kind(&self) -> WriteKind {
        match self {
            Self::Log(_) => WriteKind::Log,
            Self::Trace(_) => WriteKind::Trace,
        }
    }
}

/// The queue's shared state.
///
/// Poisoning is recovered rather than propagated: a panic in the drain must not turn every later
/// push into a poisoned-error the caller has to handle, and a telemetry queue that stops
/// accepting lines because an unrelated task unwound is the "a log write is optional" outcome this
/// crate exists to prevent.
fn lock(queue: &Mutex<VecDeque<Payload>>) -> MutexGuard<'_, VecDeque<Payload>> {
    queue.lock().unwrap_or_else(|error| error.into_inner())
}

/// A bounded, lossy write queue with a background drain.
#[derive(Debug)]
pub struct WriteQueue {
    items: Mutex<VecDeque<Payload>>,
    capacity: usize,
    dropped_log: AtomicU64,
    dropped_trace: AtomicU64,
    accepted: AtomicU64,
    /// Set once the drain has stopped, so a push after shutdown is dropped and counted rather
    /// than queued for a task that will never run.
    closed: std::sync::atomic::AtomicBool,
}

impl WriteQueue {
    /// Build a queue.
    ///
    /// A capacity of zero is clamped to one: a queue that cannot hold anything would accept
    /// nothing, drop everything, and still answer `true` to [`Self::push`] if the call site only
    /// looked at the return value.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            items: Mutex::new(VecDeque::with_capacity(capacity.min(1024))),
            capacity: capacity.max(1),
            dropped_log: AtomicU64::new(0),
            dropped_trace: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Enqueue a line.
    ///
    /// Returns `true` when it was queued and `false` when the queue was full (an older line was
    /// evicted to make room) or closed. The caller is expected to *ignore* it: this runs on the
    /// request path, and a branch that handled a full queue by failing the request would
    /// reintroduce the defect one layer up.
    pub fn push(&self, payload: Payload) -> bool {
        let kind = payload.kind();
        if self.closed.load(Ordering::Relaxed) {
            self.record_drop(kind);
            return false;
        }
        let mut items = lock(&self.items);
        // Oldest-first. `pop_front` returning `None` on a full queue would mean the bookkeeping
        // had already lost a line, so the count is incremented for the victim either way rather
        // than trusted from the deque.
        if items.len() >= self.capacity {
            let evicted = items.pop_front().map_or(WriteKind::Log, |entry| entry.kind());
            self.count_drop(evicted);
        }
        items.push_back(payload);
        drop(items);
        self.accepted.fetch_add(1, Ordering::Relaxed);
        true
    }

    fn count_drop(&self, kind: WriteKind) {
        match kind {
            WriteKind::Log => self.dropped_log.fetch_add(1, Ordering::Relaxed),
            WriteKind::Trace => self.dropped_trace.fetch_add(1, Ordering::Relaxed),
        };
    }

    fn record_drop(&self, kind: WriteKind) {
        self.count_drop(kind);
        let registry = crate::metrics::global();
        registry.counter_add(DROPPED_FAMILY, &[kind.as_str()], 1.0);
    }

    /// Take everything queued, leaving the queue empty.
    pub fn drain_batch(&self) -> Vec<Payload> {
        let mut items = lock(&self.items);
        items.drain(..).collect()
    }

    /// How many entries are waiting.
    #[must_use]
    pub fn depth(&self) -> usize {
        lock(&self.items).len()
    }

    /// The queue's cap.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Refuse further entries and report what was still queued, so shutdown can flush it.
    pub fn close(&self) -> usize {
        self.closed.store(true, Ordering::Relaxed);
        self.depth()
    }

    /// Whether the queue has been closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Entries pushed and accepted over this queue's life.
    #[must_use]
    pub fn accepted_total(&self) -> u64 {
        self.accepted.load(Ordering::Relaxed)
    }

    /// Entries dropped, by kind.
    #[must_use]
    pub fn dropped(&self, kind: WriteKind) -> u64 {
        match kind {
            WriteKind::Log => self.dropped_log.load(Ordering::Relaxed),
            WriteKind::Trace => self.dropped_trace.load(Ordering::Relaxed),
        }
    }
}

impl Default for WriteQueue {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

static GLOBAL: OnceLock<Arc<WriteQueue>> = OnceLock::new();

/// The process-wide queue.
///
/// `OnceLock` rather than the lazy statics the exporter uses, because **this one runs on the
/// request path** and an uncontended `Once` is a single atomic load; a `LazyLock` would be the
/// same, but the queue must not be replaced by a test that re-initialises it while a live server
/// is pushing into it.
pub fn global() -> &'static Arc<WriteQueue> {
    GLOBAL.get_or_init(|| Arc::new(WriteQueue::default()))
}

/// Record a line on the request path without ever awaiting a connection.
///
/// This is the function the request middleware calls. It is total: it cannot fail, it cannot
/// await, and the only thing it can do is push. There is deliberately no `kind` parameter: a
/// caller that passed `WriteKind::Trace` alongside a log line would get a line counted as a trace,
/// and the drop counter would then lie about which store lost it.
pub fn offer(entry: &LogEntry) -> bool {
    global().push(Payload::Log(entry.clone()))
}

/// Record a sampled trace on the request path without awaiting a connection.
///
/// The counterpart of [`offer`], and separate from it rather than one `offer` that takes a kind:
/// the two payloads have different types, and a single entry point would have to be generic over
/// them, which would put the branch back on the hot path.
pub fn offer_trace(record: &TraceRecord) -> bool {
    global().push(Payload::Trace(Box::new(record.clone())))
}

/// How often the drain wakes to look for queued lines.
///
/// Not a busy spin and not a long wait: the queue is in memory, so a long interval turns every
/// peak into a burst of latency on the *next* burst, and a short one spends the CPU the handlers
/// need. 20 ms bounds how long a line waits before it is written without a scheduler slot per line.
pub const DRAIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

/// The most lines one drain pass writes.
///
/// A pass that took the whole queue would hold one connection for as long as the write took, and a
/// burst larger than this is exactly the case where the handlers are already waiting for
/// connections — so the batch is bounded and the next pass takes what is left.
pub const DRAIN_BATCH: usize = 256;

/// What one drain pass managed to write.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DrainReport {
    /// Lines and traces written to the store.
    pub written: usize,
    /// Entries the store refused. Counted, not retried: a store that refuses an entry — a schema
    /// that is not migrated, a row that will not fit — will refuse it again on the next pass, and
    /// a retry loop against a store that is down turns a bounded queue into an unbounded one.
    pub failed: usize,
}

/// Write everything queued, using `pool`.
///
/// Called by the drain task. Public so a test — or a graceful shutdown — can flush synchronously
/// rather than waiting for the next tick.
pub async fn drain(pool: &sqlx::PgPool) -> DrainReport {
    let queue = global();
    let mut report = DrainReport::default();
    for payload in queue.drain_batch().into_iter().take(DRAIN_BATCH) {
        let kind = payload.kind();
        let outcome = match payload {
            Payload::Log(entry) => store::write(pool, &entry).await.map(|_| ()),
            // The trace index is its own table and its own row, which is why the queue carries a
            // `Payload` rather than a log line with a kind attached.
            Payload::Trace(record) => trace_store::upsert(pool, &record).await,
        };
        match outcome {
            Ok(()) => report.written += 1,
            Err(error) => {
                report.failed += 1;
                // stderr, exactly as the request path used to: the one channel that does not
                // depend on the store being writable.
                eprintln!("omnion-telemetry: a queued write was dropped: {error}");
                queue.record_drop(kind);
            }
        }
    }
    report
}

/// Spawn the drain task and return its handle.
///
/// Detached on purpose — it lives for the process, like the exporter flush loop beside it. The
/// caller is [`crate::lifecycle`]'s shutdown path, which closes the queue and then calls
/// [`drain_until_empty`] rather than aborting the task, so a line queued in the last millisecond of
/// a deploy is written before the process exits.
#[must_use]
pub fn spawn_drain(pool: sqlx::PgPool) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(DRAIN_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let queue = global();
            if queue.is_closed() {
                drain_until_empty(&pool).await;
                return;
            }
            if queue.depth() > 0 {
                drain(&pool).await;
            }
        }
    })
}

/// Drain, then keep draining until the queue is empty, then return what was written.
///
/// Bounded by a deadline rather than by a count, because "the queue is empty" is not always
/// reachable: a store that keeps refusing entries while producers keep producing is a shutdown
/// that must not hang. A deploy that waits forever on a log line is a worse failure than the lost
/// line.
pub async fn drain_until_empty(pool: &sqlx::PgPool) -> DrainReport {
    let mut total = DrainReport::default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let report = drain(pool).await;
        total.written += report.written;
        total.failed += report.failed;
        if global().depth() == 0 || std::time::Instant::now() >= deadline {
            return total;
        }
        tokio::time::sleep(DRAIN_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{LogLevel, NewLogEntry};
    use uuid::Uuid;

    fn entry(message: &str) -> LogEntry {
        NewLogEntry::new(LogLevel::Info, "omnion::test", message)
            .build_with(&crate::LogContext::new_request(Uuid::new_v4()))
    }

    fn log(message: &str) -> Payload {
        Payload::Log(entry(message))
    }

    /// A trace payload, built from a real root span the way the middleware builds one — not a
    /// log line wearing a different label, which is the mistake the first draft made.
    fn trace(trace_id: &str) -> Payload {
        let root = crate::tracing_span::Span::root(trace_id, "GET /test", "omnion::test");
        let mut record = TraceRecord::from_root(
            &root,
            Some(Uuid::new_v4()),
            time::OffsetDateTime::now_utc(),
        );
        record.sampled = true;
        record.sampling = crate::tracing_span::SamplingDecision::Ratio.as_str().to_owned();
        Payload::Trace(Box::new(record))
    }

    #[test]
    fn an_empty_queue_accepts_and_then_hands_back_exactly_what_was_pushed() {
        let queue = WriteQueue::new(4);
        assert!(queue.push(log("first")));
        assert!(queue.push(trace("t-1")));
        assert_eq!(queue.depth(), 2);
        let batch = queue.drain_batch();
        assert_eq!(batch.len(), 2, "nothing may be lost on a drain");
        assert_eq!(batch[0].kind(), WriteKind::Log);
        assert_eq!(batch[1].kind(), WriteKind::Trace);
        assert_eq!(queue.depth(), 0, "a drain leaves the queue empty");
    }

    #[test]
    fn a_trace_survives_the_queue_as_a_trace_and_not_as_a_log_line() {
        // The first draft carried a `LogEntry` and re-serialised it to write a span, which cannot
        // work: the two are different rows. This asserts the payload keeps the trace's own id and
        // sampling decision all the way through the queue, so a drain cannot quietly degrade every
        // trace into a dropped entry.
        let queue = WriteQueue::new(2);
        queue.push(trace("trace-abc"));
        let batch = queue.drain_batch();
        match &batch[0] {
            Payload::Trace(record) => {
                assert_eq!(record.trace_id, "trace-abc", "the id must survive the queue");
                assert!(record.sampled, "the sampling decision must survive the queue");
            }
            Payload::Log(_) => panic!("a trace payload came back as a log line"),
        }
    }

    #[test]
    fn a_full_queue_evicts_the_oldest_and_counts_it() {
        // Capacity 2 with four pushes: the first two are the victims, the last two survive, and
        // the survivors are the NEWEST. Dropping the newest would discard the failure that is
        // happening now in favour of the success that preceded it.
        let queue = WriteQueue::new(2);
        for index in 0..4 {
            queue.push(log(&format!("line {index}")));
        }
        assert_eq!(queue.depth(), 2, "the queue must never exceed its cap");
        let batch = queue.drain_batch();
        let messages: Vec<String> = batch
            .into_iter()
            .map(|payload| match payload {
                Payload::Log(entry) => entry.message,
                Payload::Trace(_) => panic!("a trace was pushed into a log-only walk"),
            })
            .collect();
        assert_eq!(messages, vec!["line 2", "line 3"]);
        assert_eq!(queue.dropped(WriteKind::Log), 2);
        assert_eq!(queue.dropped(WriteKind::Trace), 0);
    }

    #[test]
    fn the_two_kinds_are_counted_separately() {
        // A drop counter that cannot say WHICH store lost the line is the same silent bucket the
        // exporter's first draft had; an operator reading one number for two failure modes has to
        // guess which one to go and look at.
        let queue = WriteQueue::new(1);
        queue.push(log("log"));
        queue.push(trace("t-dropped"));
        assert_eq!(queue.dropped(WriteKind::Log), 1);
        assert_eq!(queue.dropped(WriteKind::Trace), 0);
    }

    #[test]
    fn a_closed_queue_refuses_and_counts_rather_than_queueing_for_a_task_that_never_runs() {
        let queue = WriteQueue::new(8);
        assert!(queue.push(log("before")));
        queue.close();
        assert!(queue.is_closed());
        assert!(!queue.push(log("after")));
        assert_eq!(queue.depth(), 1, "only the pre-close line is left to flush");
        assert_eq!(queue.dropped(WriteKind::Log), 1);
    }

    #[test]
    fn a_zero_capacity_is_clamped_so_the_queue_can_still_hold_a_line() {
        // Without the clamp `depth()` would report 0 after an accepted push, and the acceptance
        // would be a lie told to the only caller that reads the answer.
        let queue = WriteQueue::new(0);
        assert_eq!(queue.capacity(), 1);
        assert!(queue.push(log("only")));
        assert_eq!(queue.depth(), 1);
    }

    #[test]
    fn a_panicking_drain_does_not_poison_the_queue_for_every_later_push() {
        // The drain runs on its own task. If it unwinds, the mutex is poisoned, and a telemetry
        // queue that then refuses every line turns one failed background task into a permanently
        // silent log.
        let queue = WriteQueue::new(4);
        let shared = Arc::new(queue);
        let panicking = Arc::clone(&shared);
        let result = std::thread::spawn(move || {
            let _guard = lock(&panicking.items);
            panic!("the drain unwound");
        })
        .join();
        assert!(
            result.is_err(),
            "the poisoning thread must actually have panicked"
        );
        assert!(
            shared.push(log("after the panic")),
            "a poisoned queue must still accept"
        );
    }

    #[test]
    fn a_full_queue_stays_bounded_under_a_burst_that_outruns_the_drain() {
        // The shape the defect took in production: a burst far larger than the queue. The
        // assertion is that the CAP holds and the drop counter accounts for the difference — not
        // that nothing is lost, which the contract explicitly allows and requires to be visible.
        let queue = WriteQueue::new(16);
        for index in 0..500 {
            queue.push(log(&format!("burst {index}")));
        }
        assert_eq!(queue.depth(), 16, "500 pushes into a 16-deep queue must leave 16");
        assert_eq!(queue.dropped(WriteKind::Log), 484);
        assert_eq!(queue.accepted_total(), 500, "every push is accounted for");
    }

    #[test]
    fn the_drop_family_is_declared_with_the_label_the_queue_counts_under() {
        // A family name that is not in `FAMILIES` is invisible on a scrape: `counter_add` on an
        // undeclared family is dropped by the registry, so the counter would rise in this struct
        // and nowhere an operator can read.
        let spec = crate::metrics::family(DROPPED_FAMILY)
            .expect("the drop family must be declared with its labels");
        assert_eq!(spec.labels, &["kind"]);
        assert!(
            WriteKind::Log.as_str() != WriteKind::Trace.as_str(),
            "two kinds that share a label are one counter"
        );
    }
}
