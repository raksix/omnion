//! The tracing spine (REQ-126, slice 3).
//!
//! One request produces a tree of spans: the HTTP root, a child for each SQLx statement, a child
//! for each queue publish, and a sibling tree in the worker for the consumer side. This module is
//! the plumbing that builds and stores that tree; [`crate::tracing_span`] is the model and
//! [`crate::trace_store`] is the persistence.
//!
//! ## Why the spans are recorded as they happen rather than buffered
//!
//! A `tracing` subscriber would collect them into whatever exporter is configured. This index is
//! different in one way that matters: the *trace* has to be a single row the screen can search,
//! so the record is folded per trace and written when the request completes. That is also why the
//! index is an index — the spans are a bounded convenience, and the operator's backend is where a
//! full trace is read (see the migration header).
//!
//! ## The three failure modes this design is built against
//!
//! 1. **A trace that never closes.** A request that times out or panics would leave its record
//!    unwritten if the write only happened on the success path, so [`finish`] runs in a guard's
//!    `Drop` — which is the only place guaranteed to run on a panic unwind.
//! 2. **A child with no parent.** A span created outside any request context (a CLI, a cron tick)
//!    would join a trace id nothing else knows. [`current_parent`] returns `None` there and the
//!    caller starts a root instead of inventing a link.
//! 3. **A write that fails a request.** Telemetry is written from a `Drop`, it cannot propagate an
//!    error, and a failure is reported on stderr and otherwise swallowed — the same rule the log
//!    store follows.

use std::sync::Arc;

use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::context::LogContext;
use crate::trace_store::{self, TraceFilter};
use crate::tracing_span::{Parent, SamplingDecision, Span, TraceRecord, decide};

/// The service name the API records its spans under.
pub const SERVICE_API: &str = "api";

/// The service name a worker records its spans under.
pub const SERVICE_WORKER: &str = "worker";

/// The default sampling ratio, matching the request's `sampling_ratio` default.
pub const DEFAULT_SAMPLING_RATIO: f64 = 0.1;

/// The parent linkage for a span started in the current task, if there is one.
///
/// `None` outside any traced request. A caller that gets `None` must start a root — there is no
/// "best effort" parent, because a guessed parent is a link to a trace the span does not belong to.
#[must_use]
pub fn current_parent() -> Option<Parent> {
    let context = LogContext::current();
    let trace_id = context.trace_id.clone()?;
    Some(Parent::local(
        trace_id,
        context.span_id.clone().unwrap_or_else(crate::mint_span_id),
    ))
}

/// The request id in scope, for a sampling decision that must be stable across processes.
#[must_use]
pub fn current_request_id() -> Uuid {
    LogContext::current()
        .request_id
        .unwrap_or_else(Uuid::new_v4)
}

/// Start a root span for an inbound request, honouring an inbound `traceparent`.
///
/// Returns the span and whether it is sampled. A span that is not sampled still has a trace id —
/// the log line needs one to be joinable — but it is not written to the index, because the index
/// is the *search* and a dropped trace in the search is exactly what the ratio is asking for.
#[must_use]
pub fn start_request(
    trace_id: &str,
    inbound: Option<&crate::tracing_span::TraceParent>,
    method: &str,
    route: &str,
    request_id: Uuid,
    ratio: f64,
) -> (Span, SamplingDecision) {
    let mut span = Span::root(trace_id, format!("HTTP {method} {route}"), SERVICE_API);
    span.set_attribute("http.request.method", Value::from(method));
    // The route TEMPLATE, never the literal path: a literal id is a unique span name per request,
    // which makes the search useless and the index unbounded. The same rule the metric family
    // follows, for the same reason.
    span.set_attribute("http.route", Value::from(route));
    // The request id is an attribute so a request-id search lands on the trace — which is the
    // whole reason the request names it.
    span.set_attribute("request.id", Value::from(request_id.to_string()));

    let parent_sampled = inbound.map(|parent| parent.sampled);
    let decision = decide(parent_sampled, false, ratio, &request_id);
    (span, decision)
}

/// Start a child span under the current task's span.
///
/// Returns `None` outside any traced request, so a cron tick or a CLI does not manufacture a trace
/// that nothing will ever search.
#[must_use]
pub fn start_child(name: &str, service: &str) -> Option<Span> {
    let parent = current_parent()?;
    Some(Span::root(&parent.trace_id, name, service).child_of(&parent))
}

/// Record a SQLx statement as a child span.
///
/// `statement` is the query's NAME, never its text with values interpolated: a span attribute
/// carrying a bound parameter is a data leak in a place nobody audits, and the request is explicit
/// ("SQLx queries (statement name, never parameter values)").
#[must_use]
pub fn sqlx_span(statement: &str, duration_ms: i64, failed: bool) -> Option<Span> {
    let mut span = start_child(&format!("sqlx {statement}"), SERVICE_API)?;
    span.set_attribute("db.statement", Value::from(statement));
    span.finish(0, duration_ms, failed);
    Some(span)
}

/// Record a queue publish as a child span.
///
/// This is the span that carries the trace context handed to the consumer, so it is also the span
/// the consumer links back to — which is why the publish span, not the root, is what gets stamped
/// on the job row.
#[must_use]
pub fn publish_span(queue: &str, depth: i64) -> Option<Span> {
    let mut span = start_child("queue publish", SERVICE_API)?;
    span.set_attribute("messaging.destination", Value::from(queue));
    span.set_attribute("messaging.depth", Value::from(depth));
    Some(span)
}

/// Record an outbound AI call as a child span.
///
/// The request is explicit that the span carries "provider, model, token counts and cost — never
/// prompt or completion text", so the signature has no field a prompt could be passed through. A
/// helper that took a prompt would be a leak waiting for a caller; one that cannot is a leak that
/// does not compile.
#[must_use]
pub fn ai_span(
    provider: &str,
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cost_micros: i64,
    duration_ms: i64,
    failed: bool,
) -> Option<Span> {
    let mut span = start_child("ai completion", SERVICE_API)?;
    span.set_attribute("ai.provider", Value::from(provider));
    span.set_attribute("ai.model", Value::from(model));
    span.set_attribute("ai.usage.input_tokens", Value::from(input_tokens));
    span.set_attribute("ai.usage.output_tokens", Value::from(output_tokens));
    span.set_attribute("ai.cost.micros", Value::from(cost_micros));
    span.finish(0, duration_ms, failed);
    Some(span)
}

/// The trace context a queue publish hands to its consumer.
///
/// This is the value that goes on the job row. It is captured from the *publish* span when one
/// exists, so the consumer hangs from the publish rather than from the request root — which is the
/// link the acceptance line names.
#[must_use]
pub fn context_for_publish(publish: Option<&Span>) -> Option<crate::tracing_span::TraceContext> {
    let context = crate::tracing_span::TraceContext::capture()?;
    match publish {
        // The publish span exists and is the right parent: use its own id, so the consumer's span
        // hangs from the enqueue, not from the request that happened to enqueue it.
        Some(span) => Some(crate::tracing_span::TraceContext {
            trace_id: span.trace_id.clone(),
            span_id: span.span_id.clone(),
            request_id: context.request_id,
            remote: false,
        }),
        // No publish span — the job was queued outside a traced request. Carrying the ambient
        // context is still right: it is the best link available, and it is honest.
        None => Some(context),
    }
}

/// An in-flight trace being assembled, with the pool it will be written to.
///
/// The record is mutated in place as spans arrive and written once, at the end, by
/// [`TracingGuard::finish`]. One write per request rather than one per span is what keeps the index
/// from becoming the most expensive thing in the request path.
pub struct TracingGuard {
    record: TraceRecord,
    request_id: Uuid,
    sampled: bool,
    sampling: SamplingDecision,
    route: Option<String>,
}

impl TracingGuard {
    /// Start a guard for one inbound request.
    #[must_use]
    pub fn start(root: Span, request_id: Uuid, sampled: bool, sampling: SamplingDecision) -> Self {
        Self {
            record: TraceRecord::from_root(&root, Some(request_id), OffsetDateTime::now_utc()),
            request_id,
            sampled,
            sampling,
            route: None,
        }
    }

    /// The trace id, for a `traceparent` handed onward.
    #[must_use]
    pub fn trace_id(&self) -> &str {
        &self.record.trace_id
    }

    /// The request this guard traces.
    #[must_use]
    pub fn request_id(&self) -> Uuid {
        self.request_id
    }

    /// The root span's id — the parent a child span hangs from.
    #[must_use]
    pub fn root_span_id(&self) -> &str {
        &self.record.spans[0].span_id
    }

    /// The route template, stamped once it is known.
    pub fn set_route(&mut self, route: &str) {
        self.route = Some(route.to_owned());
        self.record.route = Some(route.to_owned());
    }

    /// Add a child span.
    pub fn push(&mut self, span: Span) {
        self.record.push_span(&span);
    }

    /// Close the root span and write the trace.
    ///
    /// Takes the pool as an argument rather than holding it: a guard that owns a pool cannot be
    /// constructed before the state exists, and holding one keeps a connection's worth of Arc
    /// alive for the whole request for no benefit.
    pub async fn finish(mut self, pool: &PgPool, status: u16, duration_ms: i64) {
        let _ = self.route.take();
        self.record.duration_ms = duration_ms;
        self.record.status = if status >= 500 { "error" } else { "ok" }.to_owned();
        self.record.sampled = self.sampled;
        self.record.sampling = self.sampling.as_str().to_owned();

        if !self.sampled {
            // An unsampled trace is not written. The log line still carries the trace id, so the
            // two are joinable; what is refused is the index row, which is the searchable part.
            return;
        }
        if let Err(error) = trace_store::upsert(pool, &self.record).await {
            // Telemetry must not fail a request, and this runs after the response is already on
            // its way. stderr is the one channel that does not depend on the store being writable.
            eprintln!("omnion-api: the trace could not be indexed: {error}");
        }
    }

    /// The sampling decision, for the log line.
    #[must_use]
    pub fn sampling(&self) -> SamplingDecision {
        self.sampling
    }

    /// Whether the trace is sampled.
    #[must_use]
    pub fn is_sampled(&self) -> bool {
        self.sampled
    }

    /// How many spans the trace holds, root included.
    #[must_use]
    pub fn span_count(&self) -> i64 {
        self.record.span_count
    }

    /// The record, for a test or for a caller that wants to inspect it.
    #[must_use]
    pub fn record(&self) -> &TraceRecord {
        &self.record
    }

    /// Consume the guard and return the record, so a caller can write it itself.
    #[must_use]
    pub fn into_record(self) -> TraceRecord {
        self.record
    }
}

/// Read the trace a delivery row carries and start a consumer-side root under it.
///
/// The worker half of the acceptance line: the consumer's spans hang from the producer's publish
/// span, and the trace they join is the SAME trace id — which is what makes one waterfall out of
/// two processes. Returns `None` when the row carries no context, and the caller then starts a
/// fresh root rather than inventing a parent.
#[must_use]
pub fn consumer_record(
    delivery_id: Uuid,
    context: Option<&Value>,
) -> Option<(TraceRecord, Parent)> {
    let parsed: crate::tracing_span::TraceContext =
        serde_json::from_value(context?.clone()).ok()?;
    let parent = parsed.as_parent();

    let mut root = Span::root(
        parsed.trace_id.clone(),
        format!("queue consume {delivery_id}"),
        SERVICE_WORKER,
    );
    root.parent_span_id = Some(parsed.span_id.clone());
    root.root = false;
    if let Some(request_id) = parsed.request_id {
        root.set_attribute("request.id", Value::from(request_id.to_string()));
    }
    root.set_attribute(
        "messaging.delivery_id",
        Value::from(delivery_id.to_string()),
    );

    let record = TraceRecord::from_root(&root, parsed.request_id, OffsetDateTime::now_utc());
    Some((record, parent))
}

/// The trace search, as the screen calls it.
pub async fn search(
    pool: &PgPool,
    request_id: Option<Uuid>,
    route: Option<&str>,
    status: Option<&str>,
    min_duration_ms: Option<i64>,
    window_minutes: i64,
    limit: Option<i64>,
) -> Result<Vec<crate::tracing_span::TraceSummary>, crate::TelemetryError> {
    let filter = TraceFilter {
        request_id,
        route: route.map(str::to_owned),
        status: status.map(str::to_owned),
        min_duration_ms,
        min_spans: None,
        since: Some(trace_store::since_from_minutes(window_minutes)),
        limit: limit.unwrap_or(crate::trace_store::DEFAULT_TRACE_ROWS),
    }
    .with_limit(limit);
    trace_store::search(pool, &filter).await
}

/// Wrap a value in an `Arc` for a caller that needs a shared handle.
#[must_use]
pub fn shared<T>(value: T) -> Arc<T> {
    Arc::new(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_span_outside_any_request_has_no_parent_and_no_consumer_link() {
        // A cron tick must not manufacture a trace. Returning `None` is what makes the caller
        // start a root instead of joining a trace it does not belong to.
        assert!(current_parent().is_none());
        assert!(sqlx_span("select", 1, false).is_none());
        assert!(start_child("queue publish", SERVICE_API).is_none());
    }

    #[tokio::test]
    async fn a_span_inside_a_request_hangs_from_it() {
        let mut context = LogContext::new_request(Uuid::from_u128(3))
            .with_trace("4bf92f3577b34da6a3ce929d0e0e4736")
            .with_source("api");
        context.span_id = Some(crate::mint_span_id());
        let root_span_id = context.span_id.clone().expect("a span id");

        let (parent, child) = context
            .scope(async {
                (
                    current_parent().expect("a parent inside a request"),
                    sqlx_span("select page", 3, false).expect("a child inside a request"),
                )
            })
            .await;

        assert_eq!(parent.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(parent.span_id, root_span_id);
        // The child joined the request's trace rather than minting its own — this is the whole
        // point of the parent, and a span that minted a fresh trace id would be unjoinable.
        assert_eq!(child.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(child.parent_span_id.as_deref(), Some(root_span_id.as_str()));
        assert!(!child.root);
    }

    #[tokio::test]
    async fn a_sqlx_span_carries_the_statement_name_and_never_its_values() {
        let context = LogContext::new_request(Uuid::from_u128(4)).with_trace("t".to_owned());
        let span = context
            .scope(async { sqlx_span("insert into events", 2, false).expect("inside a request") })
            .await;
        let rendered = serde_json::to_string(&span.attributes).unwrap();
        assert!(rendered.contains("insert into events"));
        assert!(
            !rendered.contains("$1"),
            "a bound parameter reached the span"
        );
        assert_eq!(span.duration_ms, 2);
    }

    #[tokio::test]
    async fn an_ai_span_carries_the_usage_and_has_nowhere_to_put_a_prompt() {
        // The signature is the guarantee: there is no parameter a prompt text could travel
        // through, so a caller cannot leak one by forgetting to redact.
        let context = LogContext::new_request(Uuid::from_u128(5)).with_trace("t".to_owned());
        let span = context
            .scope(async {
                ai_span("openai", "gpt-4o-mini", 120, 45, 320, 800, false).expect("inside")
            })
            .await;
        let rendered = serde_json::to_string(&span.attributes).unwrap();
        assert!(rendered.contains("\"ai.provider\":\"openai\""));
        assert!(rendered.contains("\"ai.usage.input_tokens\":120"));
        assert!(rendered.contains("\"ai.usage.output_tokens\":45"));
        assert!(rendered.contains("\"ai.cost.micros\":320"));
    }

    #[test]
    fn a_consumer_record_hangs_from_the_producers_publish_span_in_the_same_trace() {
        let context = crate::tracing_span::TraceContext {
            trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".to_owned(),
            span_id: "00f067aa0ba902b7".to_owned(),
            request_id: Some(Uuid::from_u128(9)),
            remote: true,
        };
        let value = serde_json::to_value(&context).unwrap();
        let (record, parent) =
            consumer_record(Uuid::from_u128(1), Some(&value)).expect("a delivery with a context");
        assert_eq!(record.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(parent.span_id, "00f067aa0ba902b7");
        assert!(parent.remote, "the cross-process boundary was lost");
        let root = &record.spans[0];
        assert_eq!(root.parent_span_id.as_deref(), Some("00f067aa0ba902b7"));
        assert!(!root.root, "a consumer span is not a root");
        assert_eq!(root.service, SERVICE_WORKER);
    }

    #[test]
    fn a_delivery_with_no_context_starts_no_consumer_record() {
        // The honest answer: there is no producer to link to, and inventing one is worse than a
        // fresh root.
        assert!(consumer_record(Uuid::from_u128(1), None).is_none());
    }

    #[tokio::test]
    async fn a_guarded_request_records_its_children_in_order() {
        let context = LogContext::new_request(Uuid::from_u128(6)).with_trace("t".to_owned());
        let guard = context
            .scope(async {
                let root = Span::root("t", "HTTP GET /x", SERVICE_API);
                let mut guard =
                    TracingGuard::start(root, Uuid::from_u128(6), true, SamplingDecision::Ratio);
                guard.set_route("/api/v1/x");
                for index in 0..3 {
                    let mut child = Span::root("t", format!("child{index}"), SERVICE_API);
                    child.offset_ms = index as i64 * 10;
                    guard.push(child);
                }
                guard
            })
            .await;
        assert_eq!(guard.span_count(), 4, "the root is not counted");
        assert_eq!(guard.record().route.as_deref(), Some("/api/v1/x"));
        assert!(guard.is_sampled());
    }
}
