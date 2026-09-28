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
//! * [`store`] — the bounded store. The explorer reads it and nothing else; a request path never
//!   blocks on it.
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
pub mod redact;
pub mod schema;
pub mod store;

pub use context::{LogContext, mint_span_id, mint_trace_id, trace_id_from_header};
pub use error::TelemetryError;
pub use redact::{REDACTED, redact_fields, redact_text};
pub use schema::{LogEntry, LogLevel, LogSource, NewLogEntry};

/// The cap on a single log line's `fields` object.
///
/// A log line is not a data transport. A caller that wants to serialise a request body has found
/// the wrong API, and the answer is a truncation that is visible in the row rather than a line
/// that silently grows to a megabyte.
pub const MAX_FIELD_COUNT: usize = 32;

/// The cap on one field's rendered length, in characters.
pub const MAX_FIELD_CHARS: usize = 512;
