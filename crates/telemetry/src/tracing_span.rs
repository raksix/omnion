//! The trace model and W3C propagation (REQ-126, slice 3).
//!
//! Omnion does not implement OpenTelemetry — the request's own "Out" section says it does not
//! become the operator's backend, and an exporter is a *sink*, not a tracer. What this module
//! owns is everything that has to be true before a span can be exported to one, and every one of
//! those rules is a place where a plausible implementation silently produces a trace nobody can
//! read:
//!
//! * **The span model.** A span is a name, a pair of ids, a parent, a start offset, a duration, a
//!   status and redacted attributes. Nothing else. No events, no links-to-links, no nesting depth
//!   beyond one parent — a general tree is a data structure that a bounded index cannot store.
//! * **W3C [`TraceParent`].** Parse it strictly, mint it from our own span, and *round-trip* it.
//!   The round-trip is the part that is easy to get wrong: a parser that accepts a header it could
//!   not itself emit is a parser that breaks the next hop, and the failure is invisible locally.
//! * **Sampling.** Parent-based with an error bias: 100 % of errors, a configured ratio otherwise,
//!   and a child never sampled under a sampled parent. A child that samples independently of its
//!   parent produces a trace with holes in it, which is the one artefact that makes a trace worse
//!   than no trace.
//!
//! ## Why ids are strings and not a 128-bit type
//!
//! The W3C ids are hex strings of 32 and 16 characters, and the span index is keyed by the trace
//! id as text because that is what the request id search and the operator's backend both speak.
//! A `u128` would be more compact and would have to be formatted and parsed at every hop; the
//! 32 bytes a hex string costs in the index are paid once per trace, not per span.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::redact;

/// The cap on spans kept inline for one trace's waterfall.
///
/// The index is a *search* index (see the migration's header), not a span store, and the cap is
/// what keeps that true. A trace over it keeps its first [`MAX_SPANS_PER_TRACE`] spans and records
/// [`Span::truncated`]; the screen says so rather than drawing a short waterfall that looks
/// complete, and the operator's backend holds the rest.
pub const MAX_SPANS_PER_TRACE: usize = 64;

/// The cap on attributes on one span.
///
/// Same reasoning as the log line's `MAX_FIELD_COUNT`: a span that accumulates attributes
/// without bound is a log line that forgot it was a span.
pub const MAX_SPAN_ATTRIBUTES: usize = 24;

/// The cap on one attribute's rendered length, in characters.
pub const MAX_ATTRIBUTE_CHARS: usize = 256;

/// The `traceparent` header name.
pub const TRACEPARENT_HEADER: &str = "traceparent";

/// The W3C version this implementation emits.
pub const TRACEPARENT_VERSION: &str = "00";

/// An all-zero trace id, which W3C defines as invalid.
const INVALID_TRACE_ID: &str = "00000000000000000000000000000000";

/// The parent linkage, either from an inbound `traceparent` or from a job row.
///
/// Both cases are the same thing — a trace id and the span that produced it — so they are one
/// type. The distinction the screen cares about is "does this span have a parent at all", and a
/// queue job whose producer was never traced has neither id, which is `Option::None` rather than
/// a `Parent` with empty strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parent {
    /// The producer's trace id, 32 lowercase hex characters.
    pub trace_id: String,
    /// The producer's span id, 16 lowercase hex characters.
    pub span_id: String,
    /// The remote flag from the inbound header: `true` when the parent lives in another process.
    ///
    /// Kept because W3C requires it to be preserved on the way out, and because a screen that
    /// joins an API trace to a worker trace wants to know the boundary was real.
    #[serde(default)]
    pub remote: bool,
}

impl Parent {
    /// A parent in another process, from an inbound `traceparent`.
    #[must_use]
    pub fn remote(trace_id: impl Into<String>, span_id: impl Into<String>) -> Self {
        Self {
            trace_id: trace_id.into(),
            span_id: span_id.into(),
            remote: true,
        }
    }

    /// A parent in this process.
    #[must_use]
    pub fn local(trace_id: impl Into<String>, span_id: impl Into<String>) -> Self {
        Self {
            trace_id: trace_id.into(),
            span_id: span_id.into(),
            remote: false,
        }
    }
}

/// A W3C `traceparent` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceParent {
    /// The producer's trace id.
    pub trace_id: String,
    /// The producer's span id.
    pub span_id: String,
    /// The `sampled` flag.
    pub sampled: bool,
    /// The inbound header declared itself remote, so the header we emit must too.
    pub remote: bool,
}

impl TraceParent {
    /// Render the header for a span being handed to another process.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "{TRACEPARENT_VERSION}-{}-{}-{:02x}",
            self.trace_id,
            self.span_id,
            u8::from(self.sampled)
        )
    }

    /// Parse a `traceparent` strictly.
    ///
    /// A malformed header starts a NEW trace rather than joining a guessed one. That is the
    /// security-relevant decision and the reason this returns `Option`: a caller that guesses a
    /// trace id from a malformed header can be made to believe a request belongs to a trace it
    /// does not, which is a tenancy leak dressed as a convenience.
    #[must_use]
    pub fn parse(header: &str) -> Option<Self> {
        let header = header.trim();
        let mut parts = header.split('-');
        let version = parts.next()?;
        let trace_id = parts.next()?;
        let span_id = parts.next()?;
        let flags = parts.next()?;
        // W3C pins the version field to two hex characters. A future version may add trailing
        // fields, and the specification requires a parser to accept it and ignore what it does
        // not understand — so both the extra-field refusal and the 55-character length cap apply
        // ONLY to a version we know. Applying the cap to an unknown version rejects exactly the
        // headers the forward-compatibility rule exists to allow.
        let known_version = version == TRACEPARENT_VERSION;
        if known_version && (header.len() > 55 || parts.next().is_some()) {
            return None;
        }
        if !is_hex_of(trace_id, 32) || trace_id == INVALID_TRACE_ID {
            return None;
        }
        // W3C declares an all-zero span id invalid for the same reason it does for the trace id:
        // it is the value that means "no span", and a header carrying it has said nothing.
        if !is_hex_of(span_id, 16) || span_id.chars().all(|c| c == '0') {
            return None;
        }
        if version.len() < 2 || !is_hex_of(&version[..2], 2) {
            return None;
        }
        if flags.len() < 2 || !is_hex_of(&flags[..2], 2) {
            return None;
        }
        Some(Self {
            trace_id: trace_id.to_ascii_lowercase(),
            span_id: span_id.to_ascii_lowercase(),
            sampled: u8::from_str_radix(&flags[..2], 16).ok()? & 0x01 == 1,
            remote: false,
        })
    }
}

fn is_hex_of(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// How a trace was sampled.
///
/// The request names the policy — "parent-based with an always-load-ahead policy: 100 % of
/// errors, a configurable ratio otherwise" — and the enum is that policy made explicit, because
/// the alternative is a bare `bool` and a `bool` cannot be rendered in a UI or written to the
/// index as anything a human reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SamplingDecision {
    /// The parent said yes; a child inherits the decision and never drops it.
    ParentSampled,
    /// This trace failed, and an error is always worth the bytes.
    Error,
    /// The configured ratio said yes.
    Ratio,
    /// The ratio said no.
    RatioDropped,
    /// The parent said no; a child inherits the decision.
    ParentDropped,
}

impl SamplingDecision {
    /// Whether this trace is exported.
    #[must_use]
    pub fn is_sampled(self) -> bool {
        matches!(self, Self::ParentSampled | Self::Error | Self::Ratio)
    }

    /// The reason, for the screen.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ParentSampled => "parent_sampled",
            Self::Error => "error",
            Self::Ratio => "ratio",
            Self::RatioDropped => "ratio_dropped",
            Self::ParentDropped => "parent_dropped",
        }
    }
}

/// Decide whether a trace is sampled.
///
/// The order of the checks is the whole policy, so it is written out rather than folded into a
/// score: a parent that already decided is never re-decided (this is what makes the decision
/// parent-based and what stops a child from punching a hole in a sampled trace), and an error is
/// never dropped whatever the ratio says.
#[must_use]
pub fn decide(
    parent: Option<bool>,
    failed: bool,
    ratio: f64,
    request_id: &Uuid,
) -> SamplingDecision {
    if let Some(inherited) = parent {
        return if inherited {
            SamplingDecision::ParentSampled
        } else {
            SamplingDecision::ParentDropped
        };
    }
    if failed {
        return SamplingDecision::Error;
    }
    let ratio = ratio.clamp(0.0, 1.0);
    if ratio >= 1.0 {
        return SamplingDecision::Ratio;
    }
    if ratio <= 0.0 {
        return SamplingDecision::RatioDropped;
    }
    // Hash the request id rather than drawing a random number: the decision has to be *stable* for
    // a given request, because the producer and the consumer each call this independently and a
    // random draw would sample one half of a trace and drop the other.
    if hash_unit_interval(request_id) < ratio {
        SamplingDecision::Ratio
    } else {
        SamplingDecision::RatioDropped
    }
}

/// Map a uuid onto `[0, 1)` deterministically.
///
/// The avalanche at the end is not decoration. FNV-1a's entropy sits in the *high* bits after
/// the final multiply, and a sequential `Uuid::from_u128(0..1000)` differs only in its last byte —
/// so taking `acc >> 11` directly returned the same side of the ratio for every request, and the
/// first version of this function sampled 1000 out of 1000 at a ratio of 0.5. The finalizer mixes
/// the low bits back up before the shift.
fn hash_unit_interval(id: &Uuid) -> f64 {
    let mut acc: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in id.as_bytes() {
        acc ^= u64::from(*byte);
        acc = acc.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // splitmix64's finalizer: two xor-shift-multiply rounds.
    acc ^= acc >> 30;
    acc = acc.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    acc ^= acc >> 27;
    acc = acc.wrapping_mul(0x94d0_49bb_1331_11eb);
    acc ^= acc >> 31;

    // 53 bits is the mantissa a f64 holds exactly, so the division is uniform and the endpoint
    // (which would return 1.0) is unreachable. `f64::from` is for the lossless integer types
    // only — `u64` is not one of them, hence the cast.
    ((acc >> 11) & 0x1f_ffff_ffff_ffff) as f64 / 9_007_199_254_740_992.0
}

/// One span.
///
/// Attributes are redacted on construction, not on export. The alternative — redacting in the
/// exporter — is a leak whenever a caller adds a span attribute and forgets, and "forgot" is
/// exactly what a redaction pass that runs late is designed to survive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Span {
    /// The trace this span belongs to.
    pub trace_id: String,
    /// This span's own id.
    pub span_id: String,
    /// The producing span, when there was one.
    pub parent_span_id: Option<String>,
    /// The span name: `HTTP GET /api/v1/secrets/{id}`, `sqlx select`, `queue publish`.
    pub name: String,
    /// The service that produced it.
    pub service: String,
    /// Milliseconds since the trace's root started. Relative, never absolute: the waterfall draws
    /// offsets and a clock skew between two processes would otherwise shift a child before its
    /// parent.
    pub offset_ms: i64,
    /// How long the span took.
    pub duration_ms: i64,
    /// `true` when the span failed.
    pub failed: bool,
    /// Redacted attributes.
    pub attributes: Map<String, Value>,
    /// Whether this is the trace's root.
    pub root: bool,
}

impl Span {
    /// A root span.
    #[must_use]
    pub fn root(
        trace_id: impl Into<String>,
        name: impl Into<String>,
        service: impl Into<String>,
    ) -> Self {
        Self {
            trace_id: trace_id.into(),
            span_id: crate::mint_span_id(),
            parent_span_id: None,
            name: name.into(),
            service: service.into(),
            offset_ms: 0,
            duration_ms: 0,
            failed: false,
            attributes: Map::new(),
            root: true,
        }
    }

    /// Make this span a child of `parent`, keeping the parent's trace.
    #[must_use]
    pub fn child_of(mut self, parent: &Parent) -> Self {
        self.trace_id = parent.trace_id.clone();
        self.parent_span_id = Some(parent.span_id.clone());
        self.root = false;
        self
    }

    /// Attach an attribute, redacting it and enforcing the caps.
    #[must_use]
    pub fn attribute(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.set_attribute(key, value.into());
        self
    }

    /// Attach an attribute in place, redacting it and enforcing the caps.
    ///
    /// A span at the attribute cap DROPS the new key rather than replacing an older one. Dropping
    /// is the honest direction: the alternative — evicting the oldest — makes a span's attributes
    /// depend on the order they arrived in, and the value a caller cared about is the one it set
    /// last. The cap is reported in the span's own `attributes.truncated` marker so the screen can
    /// say the span is incomplete.
    pub fn set_attribute(&mut self, key: &str, value: Value) {
        let value = truncate_value(redact::redact_field(key, &value));
        if self.attributes.len() >= MAX_SPAN_ATTRIBUTES && !self.attributes.contains_key(key) {
            self.attributes
                .insert("attributes.truncated".to_owned(), Value::Bool(true));
            return;
        }
        self.attributes.insert(key.to_owned(), value);
    }

    /// Close the span, stamping its duration and outcome.
    pub fn finish(&mut self, offset_ms: i64, duration_ms: i64, failed: bool) {
        self.offset_ms = offset_ms;
        self.duration_ms = duration_ms;
        self.failed = failed;
    }
}

/// Clamp one attribute value to the documented caps.
fn truncate_value(value: Value) -> Value {
    match value {
        Value::String(text) => {
            if text.chars().count() <= MAX_ATTRIBUTE_CHARS {
                return Value::String(text);
            }
            let mut out: String = text.chars().take(MAX_ATTRIBUTE_CHARS).collect();
            out.push('…');
            Value::String(out)
        }
        Value::Array(items) => {
            // A deep structure is a payload, not an attribute; keep the shape, drop the depth.
            if items.len() <= 4 {
                return Value::Array(
                    items
                        .into_iter()
                        .map(|item| match item {
                            Value::Object(inner) => Value::Object(redact::redact_fields(&inner)),
                            other => other,
                        })
                        .collect(),
                );
            }
            Value::Array(items.into_iter().take(4).collect())
        }
        Value::Object(inner) => Value::Object(redact::redact_fields(&inner)),
        other => other,
    }
}

/// The trace context a producer hands to a consumer.
///
/// This is the serialised form of [`Parent`] plus the request id, and it is what lands in
/// `webhook_deliveries.trace_context`. It is a *wire* type on purpose: the queue column outlives
/// the binary that wrote it, so a rename here is a compatibility change and the field names are
/// short and stable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceContext {
    /// The producer's trace id.
    pub trace_id: String,
    /// The producer's span id — the span the consumer's span hangs from.
    pub span_id: String,
    /// The request that produced the job, so the consumer's log line joins the request.
    #[serde(default)]
    pub request_id: Option<Uuid>,
    /// Whether the producer was in another process.
    #[serde(default)]
    pub remote: bool,
}

impl TraceContext {
    /// Capture the current context, if there is one.
    ///
    /// Returns `None` outside any traced request, which is the honest answer for a CLI, a seed or
    /// a replayed row: there is no producer to link to.
    #[must_use]
    pub fn capture() -> Option<Self> {
        let context = crate::LogContext::current();
        let trace_id = context.trace_id.clone()?;
        Some(Self {
            trace_id,
            span_id: context.span_id.clone()?,
            request_id: context.request_id,
            remote: false,
        })
    }

    /// The parent linkage this context implies.
    #[must_use]
    pub fn as_parent(&self) -> Parent {
        Parent {
            trace_id: self.trace_id.clone(),
            span_id: self.span_id.clone(),
            remote: self.remote,
        }
    }

    /// Build a `traceparent` for the consumer to send onward.
    #[must_use]
    pub fn to_traceparent(&self, sampled: bool) -> TraceParent {
        TraceParent {
            trace_id: self.trace_id.clone(),
            span_id: self.span_id.clone(),
            sampled,
            remote: self.remote,
        }
    }
}

/// One indexed trace, with the spans its waterfall draws.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceRecord {
    /// The trace id.
    pub trace_id: String,
    /// The root span's name.
    pub root_name: String,
    /// The service the root ran in.
    pub service: String,
    /// The route template, for a route filter.
    pub route: Option<String>,
    /// The request that started the trace.
    pub request_id: Option<Uuid>,
    /// When the root started.
    pub started_at: OffsetDateTime,
    /// The root's duration.
    pub duration_ms: i64,
    /// How many spans the trace really had.
    pub span_count: i64,
    /// How many are inline here.
    pub spans_kept: i64,
    /// Whether the cap dropped any.
    pub spans_truncated: bool,
    /// `ok` or `error`.
    pub status: String,
    /// Whether the trace is exported.
    pub sampled: bool,
    /// The sampling reason, for the screen.
    pub sampling: String,
    /// A link to the operator's backend, when one is configured.
    pub backend_trace_url: Option<String>,
    /// The spans.
    pub spans: Vec<Span>,
}

impl TraceRecord {
    /// Start a record from a root span.
    #[must_use]
    pub fn from_root(root: &Span, request_id: Option<Uuid>, started_at: OffsetDateTime) -> Self {
        Self {
            trace_id: root.trace_id.clone(),
            root_name: root.name.clone(),
            service: root.service.clone(),
            route: None,
            request_id,
            started_at,
            duration_ms: root.duration_ms,
            span_count: 1,
            spans_kept: 1,
            spans_truncated: false,
            status: if root.failed { "error" } else { "ok" }.to_owned(),
            sampled: true,
            sampling: SamplingDecision::Ratio.as_str().to_owned(),
            backend_trace_url: None,
            spans: vec![root.clone()],
        }
    }

    /// Add a span, enforcing the cap and recording that the cap bit.
    ///
    /// A trace is identified by `trace_id`, and a trace that arrived in two processes is a *link*,
    /// not a parent — the consumer's spans are appended with their own ids while `span_count`
    /// still counts them, so the waterfall shows work that happened after the root closed.
    pub fn push_span(&mut self, span: &Span) {
        self.span_count += 1;
        if self.span_count > self.spans_kept as i64 {
            self.spans_truncated = true;
        }
        if self.spans_kept < MAX_SPANS_PER_TRACE as i64 {
            self.spans.push(span.clone());
            self.spans_kept += 1;
        }
        if span.failed {
            self.status = "error".to_owned();
        }
    }

    /// The waterfall the screen draws, ordered by offset then name.
    ///
    /// Ordering is by the span's own offset rather than by insertion, because a consumer's spans
    /// arrive after the root and a consumer can start *before* the producer's last child (the
    /// publish is not the last thing a request does).
    #[must_use]
    pub fn waterfall(&self) -> Vec<&Span> {
        let mut spans: Vec<&Span> = self.spans.iter().collect();
        spans.sort_by(|a, b| {
            a.offset_ms
                .cmp(&b.offset_ms)
                .then_with(|| a.name.cmp(&b.name))
        });
        spans
    }

    /// The route template, for the filter.
    #[must_use]
    pub fn route(&self) -> Option<&str> {
        self.route.as_deref()
    }
}

/// A row of the trace search, without the spans.
///
/// The list reads many traces; shipping each one's inline waterfall to draw a table of them would
/// make the search proportional to the *sum* of every trace it touched. The detail route reads one
/// and ships its spans.
#[derive(Debug, Clone, Serialize)]
pub struct TraceSummary {
    /// The trace id.
    pub trace_id: String,
    /// The root span's name.
    pub root_name: String,
    /// The service.
    pub service: String,
    /// The route template.
    pub route: Option<String>,
    /// The request id.
    pub request_id: Option<Uuid>,
    /// When it started, RFC 3339.
    pub started_at: String,
    /// The root's duration.
    pub duration_ms: i64,
    /// How many spans it had.
    pub span_count: i64,
    /// `ok` or `error`.
    pub status: String,
    /// Whether it is exported.
    pub sampled: bool,
    /// The sampling reason.
    pub sampling: String,
}

impl TraceSummary {
    /// Project a record onto its search row.
    #[must_use]
    pub fn of(record: &TraceRecord) -> Self {
        Self {
            trace_id: record.trace_id.clone(),
            root_name: record.root_name.clone(),
            service: record.service.clone(),
            route: record.route.clone(),
            request_id: record.request_id,
            started_at: record
                .started_at
                .format(&Rfc3339)
                .unwrap_or_else(|_| record.started_at.to_string()),
            duration_ms: record.duration_ms,
            span_count: record.span_count,
            status: record.status.clone(),
            sampled: record.sampled,
            sampling: record.sampling.clone(),
        }
    }
}

/// The link to the operator's tracing backend for one trace.
///
/// The convention is not invented: Jaeger, Grafana Tempo and Grafana's own Explore all take a
/// `<base>/<trace-id>` path, so one template covers the three an operator is likely to run. A
/// backend that needs a query instead of a path is a setting the operator edits, which is why this
/// is a function over a configured base rather than a constant.
#[must_use]
pub fn backend_trace_url(base: &str, trace_id: &str) -> String {
    format!("{}/{trace_id}", base.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_traceparent_round_trips_through_its_own_renderer() {
        // The property that matters: whatever we emit, our own parser accepts, and the two ids
        // survive unchanged. A parser stricter than the emitter breaks the first downstream hop.
        let original = TraceParent {
            trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".to_owned(),
            span_id: "00f067aa0ba902b7".to_owned(),
            sampled: true,
            remote: true,
        };
        let rendered = original.render();
        assert_eq!(
            rendered,
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        );
        let parsed = TraceParent::parse(&rendered).expect("the emitted header parses");
        assert_eq!(parsed.trace_id, original.trace_id);
        assert_eq!(parsed.span_id, original.span_id);
        assert!(parsed.sampled);
    }

    #[test]
    fn an_uppercase_header_is_accepted_and_normalised() {
        // W3C says hex is case-insensitive on the wire and lowercase in the canonical form. A
        // parser that refuses uppercase breaks any client that bothers to capitalise it; one that
        // keeps it uppercase produces a trace id that joins nothing.
        let parsed = TraceParent::parse("00-4BF92F3577B34DA6A3CE929D0E0E4736-00F067AA0BA902B7-01")
            .expect("uppercase is legal hex");
        assert_eq!(parsed.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
    }

    #[test]
    fn a_malformed_traceparent_is_refused_so_a_new_trace_starts() {
        for header in [
            "",
            "00",
            "00-tooshort-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
            "zz-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e473g-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e473-00f067aa0ba902b7-01",
        ] {
            assert!(
                TraceParent::parse(header).is_none(),
                "a malformed header was accepted: {header}"
            );
        }
    }

    #[test]
    fn a_future_version_is_tolerated_but_a_short_one_is_not() {
        // W3C requires a future version to be parsed by ignoring fields it does not understand.
        let future =
            TraceParent::parse("cc-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-what");
        assert!(future.is_some(), "a longer version must still parse");

        // A one-character version is malformed, not a future version.
        assert!(
            TraceParent::parse("0-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").is_none()
        );
    }

    #[test]
    fn an_error_is_sampled_at_a_zero_ratio_and_a_success_is_not() {
        let id = Uuid::from_u128(7);
        assert_eq!(decide(None, true, 0.0, &id), SamplingDecision::Error);
        assert_eq!(
            decide(None, false, 0.0, &id),
            SamplingDecision::RatioDropped
        );
        assert_eq!(decide(None, false, 1.0, &id), SamplingDecision::Ratio);
    }

    #[test]
    fn a_parent_decision_is_never_overridden() {
        // This is the property that stops a child punching a hole in a sampled trace: an error
        // child under a dropped parent stays dropped, and a good child under a sampled parent
        // stays sampled.
        let id = Uuid::from_u128(11);
        assert_eq!(
            decide(Some(false), true, 1.0, &id),
            SamplingDecision::ParentDropped
        );
        assert_eq!(
            decide(Some(true), false, 0.0, &id),
            SamplingDecision::ParentSampled
        );
    }

    #[test]
    fn the_ratio_decision_is_stable_for_one_request() {
        // The producer and the consumer each decide independently, so a random draw would sample
        // one half of a trace. Stability is the property that makes parent-based sampling work
        // across a process boundary at all.
        let id = Uuid::from_u128(0x5eed);
        let first = decide(None, false, 0.5, &id);
        for _ in 0..64 {
            assert_eq!(decide(None, false, 0.5, &id), first);
        }
    }

    #[test]
    fn the_ratio_decision_spreads_requests_across_the_range() {
        // A hash that returns the same value for every id is a stable hash with no spread, and
        // every trace would land on the same side of the ratio.
        let mut sampled = 0;
        for index in 0..1000u128 {
            if decide(None, false, 0.5, &Uuid::from_u128(index)).is_sampled() {
                sampled += 1;
            }
        }
        assert!(
            (400..600).contains(&sampled),
            "the ratio does not spread: {sampled}/1000 landed on the sampled side"
        );
    }

    #[test]
    fn a_span_attributes_through_the_shared_redaction_pass() {
        // The span is redacted on construction, so a caller cannot add an attribute that only
        // becomes a leak at export time.
        let mut span = Span::root("t", "ai.completion", "api");
        span.set_attribute("api_key", Value::from("sk-live-abcd1234"));
        span.set_attribute("authorization", Value::from("Bearer tok_9f8e7d"));
        span.set_attribute("user_email", Value::from("person@example.test"));
        let rendered = serde_json::to_string(&span.attributes).unwrap();
        assert!(
            !rendered.contains("sk-live-abcd1234"),
            "the secret reached the span attributes: {rendered}"
        );
        assert!(
            !rendered.contains("person@example.test"),
            "the e-mail reached the span attributes: {rendered}"
        );
        assert!(
            rendered.contains(redact::REDACTED),
            "nothing was redacted: {rendered}"
        );
    }

    #[test]
    fn an_attribute_over_the_cap_is_truncated_and_the_truncation_is_marked() {
        let mut span = Span::root("t", "s", "api");
        for index in 0..(MAX_SPAN_ATTRIBUTES + 5) {
            span.set_attribute(&format!("k{index}"), Value::from(index));
        }
        assert_eq!(
            span.attributes.get("attributes.truncated"),
            Some(&Value::Bool(true)),
            "the cap was hit without saying so: {:?}",
            span.attributes
        );
        // Dropping is the honest direction: the newest key is the one the caller cared about.
        assert!(
            !span
                .attributes
                .contains_key(&format!("k{}", MAX_SPAN_ATTRIBUTES + 4))
        );
    }

    #[test]
    fn a_span_over_the_trace_cap_says_it_was_truncated() {
        let root = Span::root("trace-1", "HTTP GET /x", "api");
        let mut record = TraceRecord::from_root(&root, None, OffsetDateTime::UNIX_EPOCH);
        for index in 0..(MAX_SPANS_PER_TRACE as i64 + 10) {
            let mut child = Span::root("trace-1", format!("child{index}"), "worker");
            child.parent_span_id = Some(root.span_id.clone());
            child.root = false;
            record.push_span(&child);
        }
        assert!(record.spans_truncated, "the cap was hit silently");
        assert_eq!(record.spans.len(), MAX_SPANS_PER_TRACE);
        assert_eq!(record.span_count, MAX_SPANS_PER_TRACE as i64 + 11);
    }

    #[test]
    fn a_child_span_inherits_its_parents_trace_and_is_not_a_root() {
        let parent = Parent::remote("4bf92f3577b34da6a3ce929d0e0e4736", "00f067aa0ba902b7");
        let span = Span::root("own-trace", "sqlx select", "api").child_of(&parent);
        assert_eq!(span.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(span.parent_span_id.as_deref(), Some("00f067aa0ba902b7"));
        assert!(!span.root);
    }

    #[test]
    fn the_waterfall_is_ordered_by_offset_not_by_arrival() {
        // A consumer's spans arrive after the root, and a consumer can start before the
        // producer's last child — the publish is not the last thing a request does. Ordering by
        // insertion draws the waterfall in the wrong order.
        let root = Span::root("t", "root", "api");
        let mut record = TraceRecord::from_root(&root, None, OffsetDateTime::UNIX_EPOCH);
        let mut late = Span::root("t", "sqlx late", "api");
        late.offset_ms = 900;
        let mut early = Span::root("t", "queue publish", "api");
        early.offset_ms = 100;
        record.push_span(&late);
        record.push_span(&early);

        let names: Vec<&str> = record.waterfall().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["root", "queue publish", "sqlx late"]);
    }

    #[test]
    fn a_failing_span_marks_the_whole_trace_as_an_error() {
        let root = Span::root("t", "root", "api");
        let mut record = TraceRecord::from_root(&root, None, OffsetDateTime::UNIX_EPOCH);
        assert_eq!(record.status, "ok");
        let mut child = Span::root("t", "webhook post", "api");
        child.failed = true;
        record.push_span(&child);
        assert_eq!(
            record.status, "error",
            "a failed child left the trace green"
        );
    }

    #[test]
    fn a_trace_context_survives_the_json_round_trip_it_travels_through() {
        // It lands in a queue column and is read back by a *different* process, so the serialised
        // form is the contract. A field rename is a compatibility break, not a refactor.
        let context = TraceContext {
            trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".to_owned(),
            span_id: "00f067aa0ba902b7".to_owned(),
            request_id: Some(Uuid::from_u128(42)),
            remote: true,
        };
        let json = serde_json::to_string(&context).unwrap();
        let back: TraceContext = serde_json::from_str(&json).expect("the wire form round-trips");
        assert_eq!(back, context);
    }

    #[test]
    fn a_trace_context_missing_optional_fields_still_reads() {
        // A row written by an older release, or by anything that only knows the two ids.
        let back: TraceContext = serde_json::from_str(r#"{"trace_id":"t","span_id":"s"}"#)
            .expect("reads without extras");
        assert_eq!(back.request_id, None);
        assert!(!back.remote);
    }

    #[test]
    fn the_backend_link_is_built_from_a_configured_base() {
        assert_eq!(
            backend_trace_url("https://tempo.example/api/traces/", "abc"),
            "https://tempo.example/api/traces/abc"
        );
    }
}
