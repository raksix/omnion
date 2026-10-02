//! The edge: a request id for every request, and the one line that describes it.
//!
//! This is the module the request describes in one sentence — *"a middleware assigns the request
//! id at the edge (REQ-040 already returns it as a header), binds the user/org context after
//! authentication, and every log line inside that task inherits both"* — and getting the order
//! right is the whole job.
//!
//! ## The order, and why each step is where it is
//!
//! 1. **Assign the id before the handler runs.** If the id were minted inside the handler, a
//!    request that timed out, crashed or was rejected before reaching a handler would have no id
//!    at all — and those are exactly the requests an operator is holding an id-less complaint
//!    about.
//! 2. **Honour an inbound `traceparent`.** A request that arrives from another service already
//!    has a trace; minting a fresh one severs the join the request's propagation requirement is
//!    about. A malformed header is ignored rather than trusted.
//! 3. **Read the actor off the *request* after the inner service ran.** This is the part that is
//!    easy to get wrong: the route guard puts the resolved `CurrentSession` into the
//!    **request's** extensions (`guards.rs`), not the response's, and the request has been moved
//!    into the inner service by then. So the layer captures a clone of the request's extensions
//!    *before* calling, and reads the session from that snapshot afterwards. Reading
//!    `response.extensions()` — the obvious guess — finds nothing on every route, which is a log
//!    store where `user_id` is structurally always null while every test still passes.
//! 4. **Echo the id on the response**, including on a refusal, so the banner an operator reads
//!    and the id that finds the line are the same string.
//!
//! ## What this deliberately does not do
//!
//! It does not decide whether a line is written. [`omnion_telemetry::store::write`] persists it,
//! and a failure there is written to stderr and swallowed. A log write that could fail a request
//! is a log write that will eventually be disabled by whoever is under pressure, and a disabled
//! log is worse than a missing one because nobody is told.

use std::time::Instant;

use axum::body::Body;
use axum::extract::{MatchedPath, Request, State};
use axum::http::{HeaderName, HeaderValue, Response};
use axum::middleware::Next;
use uuid::Uuid;

use omnion_telemetry::tracing_spine::{self, TracingGuard};
use omnion_telemetry::{
    LogContext, LogLevel, LogSource, NewLogEntry, mint_trace_id, trace_id_from_header,
};

use crate::auth::CurrentSession;
use crate::state::AppState;

/// The response header the request id is returned in.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// The inbound W3C propagation header.
const TRACEPARENT: &str = "traceparent";

/// Assign the context, run the request, record the line.
///
/// `Next` is last so every extractor before it may consume the request body — the same ordering
/// rule every handler in this codebase follows.
pub async fn request_context(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response<Body> {
    let request_id = Uuid::new_v4();
    let started = Instant::now();

    // The drain's in-flight count (REQ-126, slice 4). Taken HERE, before anything else, and held
    // for the whole request by the guard's Drop — so a request that is refused, that panics or
    // that is cancelled still releases it. Counting "requests that reached a handler" instead
    // would leave the drain waiting for a request that ended at the router.
    let _in_flight = omnion_telemetry::lifecycle::global().track();

    // An inbound trace is honoured; a malformed one is ignored. Accepting a header that is not
    // the shape W3C defines produces a trace id no collector will ever join to.
    let inbound_trace = request
        .headers()
        .get(TRACEPARENT)
        .and_then(|value| value.to_str().ok())
        .and_then(trace_id_from_header);
    let trace_id = inbound_trace.unwrap_or_else(mint_trace_id);
    let method = request.method().as_str().to_owned();
    let host = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    // The root span and the guard that will write it. Started HERE, before the context and the
    // scope, so the trace exists for the whole request — a span started inside the scope would
    // miss everything the middleware itself did, and the inbound `traceparent`'s sampling decision
    // has to be made once, by the edge, so every child inherits it.
    let inbound_parent = request
        .headers()
        .get(TRACEPARENT)
        .and_then(|value| value.to_str().ok())
        .and_then(omnion_telemetry::TraceParent::parse);
    // The route template is not known until the router has matched, so the span is started with a
    // placeholder and named once it is — the name is set from `context.route` after the scope.
    let (root_span, sampling) = tracing_spine::start_request(
        &trace_id,
        inbound_parent.as_ref(),
        &method,
        "(unmatched)",
        request_id,
        // The runtime ratio, not the constant: the settings screen writes it, and a middleware
        // pinned to the compile-time default would ignore every save the operator makes.
        tracing_spine::sampling_ratio(),
    );
    let root_span_id = root_span.span_id.clone();
    let mut tracing_guard =
        TracingGuard::start(root_span, request_id, sampling.is_sampled(), sampling);

    let mut context = LogContext::new_request(request_id)
        .with_trace(trace_id)
        .with_source("api");
    // The root span's id goes into the task-local context, which is how a child span created
    // deeper in the stack (a SQLx span, a queue publish) knows what to hang from. Without it
    // `current_parent()` falls back to a fresh id and every child becomes its own trace root —
    // which looks like a working trace in the index and is in fact a scatter of singletons.
    context.span_id = Some(root_span_id.clone());
    context.method = Some(method.clone());
    context.host = host;
    context.version = Some(state.build().version.to_owned());

    let state_for_line = state.clone();
    // Everything the line needs is read out of the context INSIDE the scope, once the inner layers
    // have run: the guard writes the actor into it (`LogContext::bind_actor`) and the completed
    // context is read back here. The extensions route is not available to an outer layer — a
    // mutation made deeper in the chain never propagates up — which is why the context is the
    // channel and not a snapshot.
    //
    // Reading it *outside* the scope is the trap: the task-local only exists while the scope is
    // installed, so `LogContext::current()` there returns the default context with no request id
    // and the line is written with a null id that no header can ever match.
    let (response, completed) = context
        .clone()
        .scope(async {
            // `MatchedPath` is inserted into the REQUEST's extensions by the router, on its way in
            // — and the request is consumed by `next.run`, so the template has to be read from the
            // request *before* it is handed over, or not at all. The response's copy is preferred
            // where it exists (a nested router publishes the full path there); this is the
            // fallback that is always present for a matched route.
            let matched = request
                .extensions()
                .get::<MatchedPath>()
                .map(|path| path.as_str().to_owned());
            let response = next.run(request).await;

            LogContext::bind_outcome(
                matched.or_else(|| {
                    response
                        .extensions()
                        .get::<MatchedPath>()
                        .map(|path| path.as_str().to_owned())
                }),
                Some(response.status().as_u16()),
                Some(started.elapsed().as_millis() as i64),
            );
            let completed = LogContext::current();
            (response, completed)
        })
        .await;

    let mut context = completed;
    // A nested `/api/v1` router publishes the full path (`/api/v1/observability/logs`) on the
    // response, while the request's own `MatchedPath` is the sub-router's fragment
    // (`/observability/logs`). The nest prefix is prepended here so the stored template is the
    // one a caller would type, and so the same route does not appear twice under two names.
    if let Some(route) = context.route.as_deref()
        && !route.starts_with('/')
    {
        context.route = Some(format!("/api/v1{route}"));
    }
    // A guard that refused binds no actor, and the line says so. A route the router never matched
    // has no template, and `(unmatched)` is what the explorer's target filter then shows — better
    // than a plausible-looking path that was never served.
    if context.route.is_none() {
        context.route = Some("(unmatched)".to_owned());
    }
    context.status = Some(response.status().as_u16());
    context.duration_ms = Some(started.elapsed().as_millis() as i64);

    // The route template is only known now, so the root span is renamed and the guard is told.
    // An unmatched route is left as `(unmatched)` rather than given a plausible-looking path,
    // which is the same rule the log line follows.
    {
        let record = tracing_guard.record_mut();
        record.route = context.route.clone();
        record.root_name = format!(
            "HTTP {method} {}",
            context.route.as_deref().unwrap_or("(unmatched)")
        );
    }
    if let Some(route) = context.route.as_deref() {
        tracing_guard.set_route(route);
    }

    let mut response = response;
    if let Ok(value) = HeaderValue::from_str(&request_id.to_string()) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(REQUEST_ID_HEADER), value);
    }

    let level = if response.status().is_server_error() {
        LogLevel::Error
    } else if response.status().is_client_error() {
        LogLevel::Warn
    } else {
        LogLevel::Info
    };
    let entry = NewLogEntry::new(
        level,
        "omnion_api::request",
        format!(
            "{} {}",
            method,
            context.route.as_deref().unwrap_or("(unmatched)")
        ),
    )
    .with_field("status", i64::from(context.status.unwrap_or(0)))
    .with_field("duration_ms", context.duration_ms.unwrap_or(0))
    .source(LogSource::Api)
    .build_with(&context);

    // The two HTTP metric families, recorded from the SAME completed context the line is built
    // from, so a family can never report a route the line disagrees with. The label is the route
    // *template* — the request is explicit about this, and it is a cardinality rule as much as a
    // privacy one: a literal id would make every request its own series.
    //
    // The status label is the class, not the code. A 4xx and a 5xx are the two things an operator
    // alerts on, and a family keyed by the exact code has a series per code per route per method
    // for no query anybody writes.
    {
        let route = context
            .route
            .clone()
            .unwrap_or_else(|| "(unmatched)".to_owned());
        let status = context.status.unwrap_or(0);
        let class = if status >= 500 {
            "5xx"
        } else if status >= 400 {
            "4xx"
        } else {
            "2xx"
        };
        let registry = omnion_telemetry::metrics::global();
        registry.counter_add(
            "omnion_http_requests_total",
            &[route.as_str(), method.as_str(), class],
            1.0,
        );
        registry.observe(
            "omnion_http_request_duration_seconds",
            &[route.as_str(), method.as_str()],
            context.duration_ms.unwrap_or(0) as f64 / 1000.0,
        );
    }

    // The line is QUEUED, not written. It used to be written here, on the caller's task, holding
    // one of the caller's own pooled connections while it did — and when the pool was exhausted
    // that write waited out the pool's own 5 s `acquire_timeout` before giving up. Two writes per
    // request (the line, then the trace index), each able to add five seconds to it. The module
    // comment at the top of `store.rs` claimed "a request path never blocks on telemetry" on the
    // strength of there being no queue to drain, which was the wrong reason: the absence of a
    // queue was not a protection, it WAS the stall.
    //
    // The evidence is in the API's own stderr, 295 times in one QA run:
    //   "the request line could not be stored: pool timed out while waiting for an open connection"
    // Every one of those is a request that spent seconds on a log line it did not get. The
    // bounded queue is `omnion_telemetry::sink`; its drops are counted, never silent.
    let _ = omnion_telemetry::sink::offer(&entry);

    // The exporter fan-out (REQ-126 slice 3's remaining half). It is a ring push per configured
    // exporter and cannot fail a request — that is the whole contract of the buffer. It is fired
    // here rather than in the drain because an exporter is a *ship*, not a *store*: its ring
    // carries payloads the local store may later drop, and an operator's remote backend keeping a
    // line the local explorer lost is a feature, not a disagreement. The comment that used to sit
    // here said the opposite, and the reason it was written was a real one: the local store and
    // the exporter must not disagree about which lines exist. They now can, and deliberately so —
    // the counter that says so is `omnion_telemetry_writes_dropped_total`, and a gap the operator
    // can see beats a synchronous write the operator cannot afford.
    let _ = omnion_telemetry::exporter_flush::fan_out(
        omnion_telemetry::exporter::global(),
        serde_json::to_value(&entry).unwrap_or(serde_json::Value::Null),
    );

    // The trace is queued on the same path, after the log line, so a trace that is searchable
    // always has its log line present too — an operator who finds a trace in the search can always
    // see the line that explains it. A 5xx is re-decided here as an error: the sampling bias is
    // "100 % of errors", and a request that failed has to be sampled even though the edge could
    // not know that when it made the decision.
    let status = response.status().as_u16();
    if status >= 500 {
        tracing_guard.force_sampled(omnion_telemetry::SamplingDecision::Error);
    }
    tracing_guard.finish_offline(status, context.duration_ms.unwrap_or(0));

    response
}
