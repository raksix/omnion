//! Omnion observability (docs/requests/REQ-126-observability-stack.md).
//!
//! One crate, one job per module, and one rule that decides who owns what:
//!
//! * [`schema`] — the log line. Every field the request names, in one struct, so a field cannot
//!   exist in the store and be missing from the explorer, or vice versa.
//! * [`context`] — the request-scoped binding. A middleware assigns a request id at the edge and
//!   binds the user and organization after authentication; every line emitted inside that task
//!   inherits all three, and a worker inherits the trace of whatever enqueued it.
//! * [`redact`] — the **single** redaction pass. Not a helper, not a convention: the module that
//!   REQ-037 owns (`omnion_secrets::redaction`) decides what a secret looks like, and this one
//!   decides what a *field* may contain, so a log line, a span attribute and an exporter payload
//!   are all filtered by the same rule.
//! * [`metrics`] — the metric registry. The families are declared, the labels are positional and
//!   closed, and the cardinality budget is enforced in one place with the loss counted and
//!   labelled rather than swallowed.
//! * [`metric_catalog`] — the registry's durable projection, so the panel documents what the
//!   process can record rather than what someone remembered to write down.
//! * [`store`] — the bounded store. The explorer reads it and nothing else; a request path never
//!   blocks on it.
//! * [`exporter_flush`] — the part of the exporter pipeline that actually MOVES telemetry: the
//!   fan-out that feeds the buffers and the loop that drains them. It lives beside
//!   [`exporter`] rather than in the API binary because the drain is worth testing without a
//!   database and the fan-out is worth testing without a request.
//!
//! ## Why a separate crate and not a module in `omnion-core`
//!
//! `omnion-core` already owns the *subscriber* (the JSON vs pretty switch) and it will keep
//! owning it. What arrives here is everything that needs to be **shared, testable on its own and
//! reachable from a worker binary that does not link the whole core**: the schema, the context
//! propagation, the redaction pass and the store. Two implementations of any of those drift, and
//! drift in this area is a leak.

pub mod context;
pub mod error;
pub mod exporter;
pub mod exporter_flush;
pub mod metric_catalog;
pub mod metrics;
pub mod redact;
pub mod schema;
pub mod store;
pub mod trace_store;
pub mod tracing_span;
pub mod tracing_spine;

pub use context::{LogContext, mint_span_id, mint_trace_id, trace_id_from_header};
pub use error::TelemetryError;
pub use exporter::{
    Batch, Collector as ExporterCollector, ExporterHealth, ExporterKind, FlushOutcome,
};
pub use exporter_flush::{batch_body, fan_out};
pub use metric_catalog::FamilyDeclaration;
pub use metrics::{FamilySpec, MetricKind, Registry, global as metrics_registry};
pub use redact::{REDACTED, redact_fields, redact_text};
pub use schema::{LogEntry, LogLevel, LogSource, NewLogEntry};
pub use trace_store::TraceFilter;
pub use tracing_span::{
    MAX_SPANS_PER_TRACE, Parent, SamplingDecision, Span, TraceContext, TraceParent, TraceRecord,
    TraceSummary,
};

/// The cap on a single log line's `fields` object.
///
/// A log line is not a data transport. A caller that wants to serialise a request body has found
/// the wrong API, and the answer is a truncation that is visible in the row rather than a line
/// that silently grows to a megabyte.
pub const MAX_FIELD_COUNT: usize = 32;

/// The cap on one field's rendered length, in characters.
pub const MAX_FIELD_CHARS: usize = 512;
