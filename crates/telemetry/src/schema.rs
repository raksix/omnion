//! The log line: one struct for the fields the request names, so the store, the explorer and the
//! exporter cannot disagree about what a line is.
//!
//! The request enumerates the fields: `ts`, `level`, `target`, `msg`, `request_id`, `trace_id`,
//! `span_id`, `user_id`, `organization_id`, `route`, `method`, `status`, `duration_ms`, `source`,
//! `host`, `version` and an allow-listed `fields` object. They are all here, and they are all
//! `Option` except the four that every line has — which is the shape that makes the enumerations
//! in [`NewLogEntry::from_context`] exhaustive without a second list to keep in step.
//!
//! ## Why construction goes through `from_context`
//!
//! Because the alternative is a builder where each caller fills in the fields it remembers. A
//! caller that logs at the edge and forgets `target` produces a row the explorer's target filter
//! cannot select; a caller that logs in a worker and forgets `source` produces a row that claims
//! to be API traffic. `from_context` reads the task-local, so those two mistakes are not
//! available.
//!
//! ## The `fields` object is redacted before it is ever here
//!
//! Not by convention: [`crate::redact`] runs inside [`NewLogEntry::build`], which is the only
//! constructor, so a stored line has been through the pass whether or not its caller remembered.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::context::LogContext;
use crate::redact::redact_fields;
use crate::{MAX_FIELD_CHARS, MAX_FIELD_COUNT};

/// How loud a line is. Ordered from quietest to loudest so a threshold comparison is a `<`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Very fine-grained; off in every environment but a raised one.
    Trace,
    /// Detail that helps while debugging.
    Debug,
    /// A normal thing that happened.
    Info,
    /// Something unexpected that the system handled.
    Warn,
    /// Something that failed and the operator should know about.
    Error,
}

impl LogLevel {
    /// Parse a level name, refusing anything outside the closed set.
    ///
    /// The settings screen writes these names, so a refusal here is what turns a typo in a save
    /// into a field-level message instead of a filter that silently matches nothing.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "trace" => Some(Self::Trace),
            "debug" => Some(Self::Debug),
            "info" => Some(Self::Info),
            "warn" | "warning" => Some(Self::Warn),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    /// The canonical lowercase name, as stored and as the column check expects it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Which process a line came from.
///
/// The request makes this a three-way enum rather than free text, and the reason is the explorer:
/// "show me only worker lines" is the first question an operator asks when a job misbehaves, and a
/// free-text `source` is a filter that can be misspelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogSource {
    /// The HTTP API.
    Api,
    /// A background runner: queue consumer, workflow engine, rollup worker.
    Worker,
    /// The command line.
    Cli,
}

impl LogSource {
    /// The canonical lowercase name, as stored and as the column check expects it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Worker => "worker",
            Self::Cli => "cli",
        }
    }
}

impl fmt::Display for LogSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A line on its way into the store.
///
/// Every field the caller sets explicitly is here, and the ones it does not are read from the
/// task-local by [`NewLogEntry::from_context`].
#[derive(Debug, Clone, Default)]
pub struct NewLogEntry {
    /// How loud the line is.
    pub level: Option<LogLevel>,
    /// The module path the line came from.
    pub target: Option<String>,
    /// The human-readable message.
    pub message: String,
    /// Structured detail; filtered by the redaction pass before it is stored.
    pub fields: Map<String, Value>,
    /// Override for the context's source.
    pub source: Option<LogSource>,
    /// Override for the context's route.
    pub route: Option<String>,
    /// Override for the context's method.
    pub method: Option<String>,
    /// Override for the context's status.
    pub status: Option<u16>,
    /// Override for the context's duration.
    pub duration_ms: Option<i64>,
}

impl NewLogEntry {
    /// A line at a level, with a message.
    #[must_use]
    pub fn new(level: LogLevel, target: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: Some(level),
            target: Some(target.into()),
            message: message.into(),
            ..Self::default()
        }
    }

    /// A line with structured detail.
    #[must_use]
    pub fn with_field(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.fields.insert(key.to_owned(), value.into());
        self
    }

    /// Set the level explicitly, for the callers that build a line without a constructor.
    #[must_use]
    pub fn level(mut self, level: LogLevel) -> Self {
        self.level = Some(level);
        self
    }

    /// Set the message explicitly, for a line whose text is built after construction.
    #[must_use]
    pub fn message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }

    /// Set the module path explicitly.
    #[must_use]
    pub fn target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    /// Name the process this line came from.
    #[must_use]
    pub fn source(mut self, source: LogSource) -> Self {
        self.source = Some(source);
        self
    }

    /// The line, completed from the current task's context and passed through redaction.
    ///
    /// This is the only constructor [`LogEntry`] has, so the two properties that must not be
    /// optional at the call site — the fields were redacted, and the field count is bounded — are
    /// not things a caller can forget.
    #[must_use]
    pub fn build(self) -> LogEntry {
        self.build_with(&LogContext::current())
    }

    /// The line, completed from an **explicit** context rather than the task-local.
    ///
    /// A worker that drains a queue an hour later needs this: its own task has no request, and
    /// the job row carries the request id of whatever enqueued it. `build()` in that place would
    /// read an empty context and write a line that cannot be joined to the request that caused
    /// the work — which is the exact gap the request names in its acceptance criteria.
    #[must_use]
    pub fn build_with(self, context: &LogContext) -> LogEntry {
        let level = self.level.unwrap_or(LogLevel::Info);
        let source = self.source.unwrap_or(match context.source.as_deref() {
            Some("worker") => LogSource::Worker,
            Some("cli") => LogSource::Cli,
            _ => LogSource::Api,
        });
        LogEntry {
            ts: OffsetDateTime::now_utc(),
            level,
            target: self.target.unwrap_or_else(|| "omnion".to_owned()),
            message: crate::redact::redact_text(&self.message),
            request_id: context.request_id,
            trace_id: context.trace_id.clone(),
            span_id: context.span_id.clone(),
            user_id: context.user_id,
            organization_id: context.organization_id,
            route: self.route.or_else(|| context.route.clone()),
            method: self.method.or_else(|| context.method.clone()),
            status: self.status.or(context.status),
            duration_ms: self.duration_ms.or(context.duration_ms),
            source,
            host: context.host.clone(),
            version: context.version.clone(),
            fields: bound_fields(redact_fields(&self.fields)),
        }
    }
}

/// Bound the `fields` object: at most [`MAX_FIELD_COUNT`] keys, each at most
/// [`MAX_FIELD_CHARS`] rendered characters.
///
/// The count is a *visible* truncation — the dropped keys are reported under a `_truncated` marker
/// so the explorer can say "12 fields were dropped" rather than showing a line that looks
/// complete. A silent truncation is how a log store starts lying about what it recorded.
fn bound_fields(mut fields: Map<String, Value>) -> Map<String, Value> {
    if fields.len() > MAX_FIELD_COUNT {
        let dropped: Vec<String> = fields.keys().skip(MAX_FIELD_COUNT).cloned().collect();
        let mut kept: Map<String, Value> = fields
            .iter()
            .take(MAX_FIELD_COUNT)
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        kept.insert(
            "_truncated".to_owned(),
            Value::String(format!("{} more field(s) were not recorded", dropped.len())),
        );
        fields = kept;
    }
    fields
        .iter_mut()
        .for_each(|(_, value)| *value = truncate_value(value));
    fields
}

fn truncate_value(value: &Value) -> Value {
    match value {
        Value::String(text) if text.chars().count() > MAX_FIELD_CHARS => {
            let head: String = text.chars().take(MAX_FIELD_CHARS).collect();
            Value::String(format!("{head}… (truncated)"))
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(MAX_FIELD_CHARS)
                .map(truncate_value)
                .collect(),
        ),
        other => other.clone(),
    }
}

/// A stored line, in the shape the explorer and the exporter both read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogEntry {
    /// When the line was emitted, in UTC.
    pub ts: OffsetDateTime,
    /// How loud it is.
    pub level: LogLevel,
    /// The module path.
    pub target: String,
    /// The message, redacted.
    pub message: String,
    /// The request this belongs to, when there is one.
    pub request_id: Option<Uuid>,
    /// The trace this belongs to.
    pub trace_id: Option<String>,
    /// The span within that trace.
    pub span_id: Option<String>,
    /// Who acted.
    pub user_id: Option<Uuid>,
    /// Which organization.
    pub organization_id: Option<Uuid>,
    /// The route template, never the literal path.
    pub route: Option<String>,
    /// The HTTP method.
    pub method: Option<String>,
    /// The responding status.
    pub status: Option<u16>,
    /// How long it took, in milliseconds.
    pub duration_ms: Option<i64>,
    /// Which process emitted it.
    pub source: LogSource,
    /// The host.
    pub host: Option<String>,
    /// The instance version.
    pub version: Option<String>,
    /// Structured detail, redacted and bounded.
    pub fields: Map<String, Value>,
}

impl LogEntry {
    /// The RFC 3339 rendering of [`LogEntry::ts`].
    ///
    /// Required by the store: `OffsetDateTime` serialises as a 9-element array through
    /// `serde_json` unless it carries the `serde-well-known` rfc3339 attribute, and a column that
    /// renders as `[2026, 270, …]` is a column the panel cannot read. Formatting here — once —
    /// is the fix that cannot be forgotten on the next route.
    #[must_use]
    pub fn ts_rfc3339(&self) -> String {
        self.ts.format(&Rfc3339).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn a_line_inherits_the_context_of_the_task_that_emitted_it() {
        let request_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let organization_id = Uuid::new_v4();

        let entry = LogContext::new_request(request_id)
            .with_actor(Some(user_id), Some(organization_id))
            .with_trace("trace-abc")
            .scope(async {
                NewLogEntry::new(LogLevel::Info, "omnion_secrets::store", "batch applied")
                    .with_field("rewrapped", 4i64)
                    .build()
            })
            .await;

        assert_eq!(entry.request_id, Some(request_id));
        assert_eq!(entry.user_id, Some(user_id));
        assert_eq!(entry.organization_id, Some(organization_id));
        assert_eq!(entry.trace_id.as_deref(), Some("trace-abc"));
        assert_eq!(entry.level, LogLevel::Info);
        assert_eq!(entry.target, "omnion_secrets::store");
        assert_eq!(entry.fields.get("rewrapped").unwrap(), &json!(4));
    }

    #[test]
    fn a_message_is_redacted_by_construction() {
        // The fixture is the value the test itself writes, so a leak check that passes here is a
        // check that could have failed.
        let fixture = "sk-live-51H8xQ2eZvKYlo2C0aB7dEfGh3JkLmNoPqRsTuVwXy";
        let entry = NewLogEntry::new(
            LogLevel::Error,
            "omnion_storage",
            format!("the store rejected {fixture} twice"),
        )
        .build();
        assert!(!entry.message.contains("sk-live"));
        assert!(entry.message.contains("rejected"));
        assert!(entry.message.contains("twice"));
    }

    #[test]
    fn the_field_object_is_filtered_by_the_only_constructor() {
        let entry = NewLogEntry::new(LogLevel::Debug, "omnion_ai", "provider call")
            .with_field("api_key", "sk-live-should-never-be-stored")
            .with_field("detail", "Bearer abc.def.ghi")
            .build();
        let rendered = serde_json::to_string(&entry.fields).unwrap();
        assert!(!rendered.contains("sk-live"), "fields leaked: {rendered}");
        assert!(
            !rendered.contains("abc.def.ghi"),
            "fields leaked: {rendered}"
        );
    }

    #[test]
    fn an_oversized_field_object_is_truncated_visibly() {
        let mut entry = NewLogEntry::new(LogLevel::Info, "omnion", "wide");
        for index in 0..(MAX_FIELD_COUNT + 12) {
            entry = entry.with_field(&format!("key{index}"), index as i64);
        }
        let built = entry.build();
        assert!(
            built.fields.len() <= MAX_FIELD_COUNT + 1,
            "the field count was not bounded: {}",
            built.fields.len()
        );
        assert!(
            built.fields.contains_key("_truncated"),
            "a silent truncation is a line that lies about what it recorded"
        );
    }

    #[test]
    fn a_long_field_value_is_cut_and_marked() {
        let long = "x".repeat(MAX_FIELD_CHARS + 200);
        let built = NewLogEntry::new(LogLevel::Info, "omnion", "long")
            .with_field("blob", long)
            .build();
        let stored = built.fields.get("blob").unwrap().as_str().unwrap();
        assert!(
            stored.ends_with("(truncated)"),
            "the cut must be visible: {stored}"
        );
        assert!(stored.chars().count() < MAX_FIELD_CHARS + 40);
    }

    #[test]
    fn a_timestamp_renders_as_rfc3339_and_not_as_a_tuple() {
        let entry = NewLogEntry::new(LogLevel::Info, "omnion", "when").build();
        let rendered = entry.ts_rfc3339();
        assert!(
            rendered.contains('T') && rendered.ends_with('Z'),
            "the timestamp must be an RFC 3339 instant, got {rendered}"
        );
        assert!(
            !rendered.contains('['),
            "an OffsetDateTime that serialised as a tuple is a column the panel cannot read"
        );
    }

    #[test]
    fn level_names_round_trip_and_unknown_names_are_refused() {
        for level in [
            LogLevel::Trace,
            LogLevel::Debug,
            LogLevel::Info,
            LogLevel::Warn,
            LogLevel::Error,
        ] {
            assert_eq!(LogLevel::parse(level.as_str()), Some(level));
        }
        assert_eq!(LogLevel::parse("WARNING"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::parse("  Info "), Some(LogLevel::Info));
        assert_eq!(LogLevel::parse("verbose"), None);
        assert_eq!(LogLevel::parse(""), None);
    }

    #[test]
    fn the_source_enum_is_the_three_names_the_schema_checks() {
        assert_eq!(LogSource::Api.as_str(), "api");
        assert_eq!(LogSource::Worker.as_str(), "worker");
        assert_eq!(LogSource::Cli.as_str(), "cli");
    }
}
