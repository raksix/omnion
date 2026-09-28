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

use omnion_telemetry::{LogContext, LogLevel, LogSource, NewLogEntry, mint_trace_id, trace_id_from_header};

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

    let mut context = LogContext::new_request(request_id)
        .with_trace(trace_id)
        .with_source("api");
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
        let route = context.route.clone().unwrap_or_else(|| "(unmatched)".to_owned());
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

    // A telemetry write that fails must not fail the request. It goes to stderr — the one channel
    // that does not depend on the store being writable.
    if let Err(error) = omnion_telemetry::store::write(&state_for_line.db().pool(), &entry).await {
        eprintln!("omnion-api: the request line could not be stored: {error}");
    }

    response
}
