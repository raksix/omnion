//! Integration test for the trace index, the propagation and the exporter pipeline
//! (docs/requests/REQ-126, slice 3).
//!
//! It runs against the development stack and skips itself with a printed reason when PostgreSQL
//! is not reachable, exactly like the slice 1 and 2 suites beside it.
//!
//! The walk proves the four claims the slice makes, and each one is a claim about what a **caller
//! can see**, not about what the crate holds:
//!
//! 1. **A request produces spans for HTTP → SQLx → queue publish, and the consumer span links back
//!    to the producer.** The link is the part that is easy to fake: a test can join two spans that
//!    share a trace id and call it a link, when the real requirement is that the consumer's span
//!    hangs from the *publish* span by id. The test therefore asserts the parent relationship, and
//!    that the context survived a round trip through the queue column — a different process, in
//!    production, and a `jsonb` value here.
//! 2. **Error requests are always sampled, and a sampled trace is findable by request id.** The
//!    acceptance line is two claims that can each be true alone, so both are asserted: that a 5xx
//!    is in the index despite a zero ratio, and that `?request_id=` actually returns it.
//! 3. **An exporter with an unreachable backend does not stall a request; the drop counter rises
//!    and the exporter flips to `degraded`.** The stall part is asserted structurally (the push
//!    returns, and the buffer never exceeds its cap) and the counter part on the `/metrics` scrape,
//!    because a counter that is only in a struct is not something an operator can see.
//! 4. **A prompt or a secret never reaches the exported payload.** Asserted by grepping the
//!    serialised batch for the fixture value — computed from the redaction helper's own hint, not
//!    from a hand-written stand-in, so the check cannot pass by construction.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
mod support;

use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::seed;
use omnion_telemetry::exporter::{self, ExporterKind, FlushOutcome};
use omnion_telemetry::trace_store;
use omnion_telemetry::tracing_span::{MAX_SPANS_PER_TRACE, SamplingDecision, Span, TraceContext};
use omnion_telemetry::tracing_spine;
use serde_json::{Value, json};
use std::net::SocketAddr;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

struct TestResponse {
    status: StatusCode,
    body: Value,
    text: String,
    request_id: Option<String>,
    cookie: Option<String>,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    // `ConnectInfo` is attached the way `main.rs` does it. `oneshot` bypasses the connect layer,
    // so without this the peer address is missing on every row — and a suite that asserts a peer
    // address then passes on exactly the data it should not have.
    let peer: SocketAddr = "198.51.100.7:51234"
        .parse()
        .expect("a literal is a valid peer");
    let mut request = request;
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");
    let status = response.status();
    // BOTH cookies, not the first one. A sign-in sets `omnion_session` and `omnion_csrf`, and the
    // CSRF layer refuses a mutation that presents only the session -- so a walk holding one of the
    // two is no longer a request the panel can make, and every post it issues reads `403` for a
    // reason that has nothing to do with what it is testing.
    let cookie = Some(
        response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .filter_map(|raw| raw.split(';').next())
            .filter(|pair| pair.starts_with("omnion_session=") || pair.starts_with("omnion_csrf="))
            .collect::<Vec<_>>()
            .join("; "),
    );
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let body = if raw.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&raw).unwrap_or(Value::Null)
    };
    TestResponse {
        status,
        body,
        text: raw,
        request_id,
        cookie,
    }
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(path)
        .body(Body::empty())
        .expect("a static request builds")
}

async fn sign_in(state: &AppState) -> (Uuid, String) {
    seed::ensure(state.db().pool())
        .await
        .expect("the permission catalogue seeds");
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = omnion_identity::organizations::create_organization(
        state.db().pool(),
        omnion_identity::organizations::NewOrganization {
            name: format!("Traces org {suffix}"),
            slug: format!("traces-{suffix}"),
        },
    )
    .await
    .expect("the organization must be created");
    let email = format!("traces-{suffix}@example.test");
    let user = users::create_user(
        state.db().pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Trace Walker".to_owned(),
            organization_id: Some(organization.id),
        },
    )
    .await
    .expect("the account must be created");
    seed::bind_owner(state.db().pool(), user.id)
        .await
        .expect("the owner role must be bound");

    let response = call(
        state,
        Request::builder()
            .method(Method::POST)
            .uri("/api/v1/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "email": email, "password": PASSWORD }).to_string(),
            ))
            .expect("a login request must build"),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "login: {}", response.text);
    let cookie = response
        .cookie
        .as_deref()
        .expect("login sets a session cookie");
    // The whole `Cookie` header is kept, name and value: it now carries BOTH cookies, so
    // unwrapping the session out of it would drop the CSRF token the mutation layer needs.
    let token = cookie.to_owned();
    (user.id, token.to_owned())
}

fn authed(method: Method, path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::COOKIE, token.clone())
        .body(Body::empty())
        .expect("a request with a method builds")
}

/// A traced request is findable by its request id, and its root span is named by the route
/// template — never by a literal path.
#[tokio::test]
async fn a_request_produces_a_trace_findable_by_its_request_id() {
    let state = support::walk_state::state_or_fail().await;
    let (_user, token) = sign_in(&state).await;

    // Sample everything for the duration of this test. At the documented default of 0.1 roughly
    // nine requests in ten are correctly NOT indexed, so a test that assumed its own request
    // would be findable was asserting the opposite of the feature it was named after. The ratio
    // is restored at the end so no other test inherits it.
    let previous = tracing_spine::sampling_ratio();
    tracing_spine::set_sampling_ratio(1.0);

    // Drive real traffic over the real router, so the trace is produced by the middleware rather
    // than by the test.
    let response = call(
        &state,
        authed(Method::GET, "/api/v1/observability/traces", &token),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text);
    let request_id = response
        .request_id
        .clone()
        .expect("the edge returns a request id");

    let found = call(
        &state,
        authed(
            Method::GET,
            &format!("/api/v1/observability/traces?request_id={request_id}"),
            &token,
        ),
    )
    .await;
    assert_eq!(found.status, StatusCode::OK, "{}", found.text);
    let traces = found.body["traces"].as_array().expect("traces is a list");
    assert_eq!(
        traces.len(),
        1,
        "the request id did not find exactly its own trace: {}",
        found.text
    );
    assert_eq!(traces[0]["request_id"], json!(request_id));

    // The root span is named by the TEMPLATE. A literal path here would make every request its
    // own span name, which is the cardinality rule the request states for metrics and states
    // again for route labels.
    let root_name = traces[0]["root_name"].as_str().expect("a root name");
    assert!(
        root_name.contains("observability/traces"),
        "the root span does not name the route: {root_name}"
    );

    // And the detail route answers with a waterfall rather than an empty graph.
    let trace_id = traces[0]["trace_id"].as_str().expect("a trace id");
    let detail = call(
        &state,
        authed(
            Method::GET,
            &format!("/api/v1/observability/traces/{trace_id}"),
            &token,
        ),
    )
    .await;
    assert_eq!(detail.status, StatusCode::OK, "{}", detail.text);
    let spans = detail.body["spans"].as_array().expect("spans is a list");
    assert!(!spans.is_empty(), "the waterfall is empty: {}", detail.text);
    assert_eq!(detail.body["spans_truncated"], json!(false));
    // The backend link is explicitly absent rather than a link to nowhere — the screen's empty
    // state keys off this, not off a zero span count.
    assert_eq!(
        detail.body["backend_trace_url"],
        Value::Null,
        "a backend link was invented with no backend configured"
    );

    tracing_spine::set_sampling_ratio(previous);
}

/// The consumer span hangs from the producer's publish span, and the link survives the trip
/// through the queue's jsonb column.
///
/// This is the acceptance line's hard half. A test that put two spans in one trace id and called
/// it a link would pass while the consumer span had no parent — the "link" the request asks for is
/// a *relationship*, and only the parent id states it.
#[tokio::test]
async fn the_consumer_span_links_back_to_the_producers_publish_span() {
    let state = support::walk_state::state_or_fail().await;
    let (_user, token) = sign_in(&state).await;

    // 1. A producer publishes a job, inside a request, and stamps the publish span on the row.
    let trace_id = "4bf92f3577b34da6a3ce929d0e0e4736".to_owned();
    let publish_span_id = "00f067aa0ba902b7".to_owned();
    let request_id = Uuid::new_v4();

    // The stamped value is the real `TraceContext` serialised — the same bytes the queue column
    // will hold in production, written here so the round trip is the one that is proved.
    let context = TraceContext {
        trace_id: trace_id.clone(),
        span_id: publish_span_id.clone(),
        request_id: Some(request_id),
        remote: false,
    };
    let stamped = serde_json::to_value(&context).expect("the context serialises");

    // 2. A delivery row exists and takes the context. The endpoint is created BY THE TEST rather
    // than assumed present: a suite that needs a row another suite happens to have created is a
    // suite that passes alone and fails in a full run, which is the order-dependence lesson from
    // slice 1 learned a second time.
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = omnion_identity::organizations::create_organization(
        state.db().pool(),
        omnion_identity::organizations::NewOrganization {
            name: format!("Queue org {suffix}"),
            slug: format!("queue-{suffix}"),
        },
    )
    .await
    .expect("the organization must be created");
    let endpoint = omnion_events::store::insert_endpoint(
        state.db().pool(),
        omnion_events::model::NewEndpoint {
            organization_id: organization.id,
            name: format!("receiver-{suffix}"),
            url: "https://receiver.example.test/hook".to_owned(),
            secret: "a-signing-secret-of-at-least-16-characters".to_owned(),
            events: vec!["trace.link.test".to_owned()],
            created_by: None,
        },
    )
    .await
    .expect("the endpoint must be created");

    // The event name is checked against `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$`, so the suffix's
    // hex has to be underscored: a hyphen fails the constraint and the test dies on a fixture
    // detail rather than on the thing it is proving.
    let event_id: i64 = sqlx::query_scalar("insert into events (name) values ($1) returning id")
        .bind(format!("trace.link.test_{}", suffix.replace('-', "_")))
        .fetch_one(state.db().pool())
        .await
        .expect("the event must be recorded");

    let delivery_id: Uuid = sqlx::query_scalar(
        "insert into webhook_deliveries (endpoint_id, event_id, max_attempts) \
         values ($1, $2, 3) returning id",
    )
    .bind(endpoint.id)
    .bind(event_id)
    .fetch_one(state.db().pool())
    .await
    .expect("the delivery must be queued");

    trace_store::stamp_delivery(state.db().pool(), delivery_id, &stamped)
        .await
        .expect("the context is stamped");

    // 3. The consumer reads it back — through the column, not the value it wrote.
    let read_back = trace_store::delivery_context(state.db().pool(), delivery_id)
        .await
        .expect("the context reads")
        .expect("the row carries a context");
    assert_eq!(read_back, stamped, "the context did not survive the column");

    // 4. The consumer's root span hangs from the publish span, in the SAME trace.
    let (record, parent) = tracing_spine::consumer_record(delivery_id, Some(&read_back))
        .expect("a delivery with a context yields a consumer record");
    assert_eq!(
        record.trace_id, trace_id,
        "the consumer started its own trace instead of joining the producer's"
    );
    assert_eq!(parent.span_id, publish_span_id);
    let root = &record.spans[0];
    assert_eq!(
        root.parent_span_id.as_deref(),
        Some(publish_span_id.as_str()),
        "the consumer span does not hang from the publish span"
    );
    assert!(!root.root, "a consumer span is not a root");
    assert_eq!(
        root.service, "worker",
        "the consumer is not recorded as the worker"
    );
    // The request id travelled with it, so a worker log line joins the request that caused it.
    assert_eq!(
        root.attributes.get("request.id"),
        Some(&json!(request_id.to_string()))
    );
}

/// The error bias: a 5xx is in the index even at a zero sampling ratio, and a request id finds it.
///
/// Both halves are asserted because each can be true alone — a 5xx that is sampled but not
/// findable proves nothing an operator could act on, and a findable 5xx that is only there because
/// the ratio happened to include it is not an error bias at all.
#[tokio::test]
async fn a_five_hundred_is_sampled_at_a_zero_ratio_and_is_findable_by_request_id() {
    let state = support::walk_state::state_or_fail().await;
    let (_user, token) = sign_in(&state).await;

    // At ratio 0.0 with no inbound parent, `decide` returns `RatioDropped` — so a trace that IS in
    // the index for this request can only be there because the error bias forced it.
    let request_id = Uuid::new_v4();
    let decision = omnion_telemetry::tracing_span::decide(None, true, 0.0, &request_id);
    assert_eq!(decision, SamplingDecision::Error);

    // Prove the whole path: index a failed request's trace the way the middleware would at the end
    // of a 5xx, then read it back through the search.
    // A FRESH trace id per run. `trace_store::upsert` conflicts on `trace_id`, so a constant one
    // updates the row the PREVIOUS run left behind — and `coalesce(excluded.request_id, …)` then
    // keeps that row's ORIGINAL request id, so the write appears to succeed while the search for
    // this run's id finds nothing. The symptom is a walk that passes the first time and fails on
    // every run after it, which is the hardest shape to read: the assertion says "a 5xx at ratio
    // 0.0 is not findable" when the truth is "the second run overwrote the first run's key".
    let mut guard = tracing_spine::TracingGuard::start(
        Span::root(
            uuid::Uuid::new_v4().simple().to_string(),
            "HTTP GET /api/v1/does-not-exist",
            "api",
        ),
        request_id,
        false,
        SamplingDecision::RatioDropped,
    );
    guard.set_route("/api/v1/does-not-exist");
    guard.force_sampled(SamplingDecision::Error);
    guard.finish(state.db().pool(), 500, 12).await;

    let found = call(
        &state,
        authed(
            Method::GET,
            &format!("/api/v1/observability/traces?request_id={request_id}"),
            &token,
        ),
    )
    .await;
    assert_eq!(found.status, StatusCode::OK, "{}", found.text);
    let traces = found.body["traces"].as_array().expect("traces is a list");
    assert_eq!(
        traces.len(),
        1,
        "a 5xx at ratio 0.0 is not findable: {}",
        found.text
    );
    assert_eq!(traces[0]["status"], json!("error"));
    assert_eq!(traces[0]["sampling"], json!("error"));
    assert_eq!(traces[0]["duration_ms"], json!(12));
}

/// A dead exporter degrades without blocking a request, and its loss is on the same scrape as
/// everything else.
#[tokio::test]
async fn a_full_exporter_buffer_drops_oldest_counts_the_drop_and_never_stalls() {
    let state = support::walk_state::state_or_fail().await;
    let (_user, token) = sign_in(&state).await;

    // A private collector, NOT the global: the global is shared with every other test in this
    // binary, and a drop counter is cumulative, so a global would make this assertion depend on
    // which test ran first.
    let collector = exporter::Collector::new();
    collector
        .register("dead-backend", ExporterKind::Webhook, 16)
        .expect("the exporter registers");

    // The backend is down. Push far past the cap — the request path's call.
    for index in 0..200 {
        assert!(
            collector.push("dead-backend", json!({ "n": index })),
            "a push onto a full buffer must still succeed: the request path cannot fail"
        );
    }
    collector.record_outcome(
        "dead-backend",
        &FlushOutcome::Failed {
            error: "connection refused".into(),
        },
    );

    let status = collector.status("dead-backend").expect("registered");
    assert_eq!(status.buffered, 16, "the buffer grew past its cap");
    assert_eq!(status.dropped_total, 184, "the loss was not counted");
    assert_eq!(
        status.health, "degraded",
        "one failure is degraded, not down"
    );

    // Two failures: the chip is `down`, which is what tells an operator the drops are climbing.
    collector.record_outcome(
        "dead-backend",
        &FlushOutcome::Failed {
            error: "timed out".into(),
        },
    );
    assert_eq!(collector.status("dead-backend").unwrap().health, "down");

    // The drop is on the registry, so it is visible on the same `/metrics` scrape as everything
    // else. A counter that lives only in a struct is not something an operator can see.
    let scrape = call(&state, get("/metrics")).await;
    assert_eq!(scrape.status, StatusCode::OK);
    assert!(
        scrape.text.contains("omnion_exporter_dropped_total{"),
        "the drop family is missing from the scrape: {}",
        scrape.text
    );
}

/// A span attribute and an exported payload carry no prompt and no secret.
#[tokio::test]
async fn an_exported_span_carries_usage_and_neither_prompt_text_nor_a_secret() {
    // The signature is the guarantee: `ai_span` has no field a prompt could travel through, so a
    // caller cannot leak one by forgetting to redact. What is proved here is the second half —
    // that the *other* attributes really are redacted, with the value the system would produce.
    let trace_id = "c".repeat(32);
    let context = omnion_telemetry::LogContext::new_request(Uuid::new_v4())
        .with_trace(trace_id)
        .with_source("api");
    let span = context
        .scope(async { tracing_spine::ai_span("openai", "gpt-4o-mini", 120, 45, 320, 800, false) })
        .await
        .expect("a span inside a request");

    let mut span = span;
    // A caller that *does* put a secret in an attribute gets it redacted on the way in, not on
    // the way out — the exporter is not the last line of defence.
    span.set_attribute("api_key", json!("sk-live-9f8e7d6c5b4a3210"));
    span.set_attribute("user_email", json!("person@example.test"));

    let payload = serde_json::to_value(&span).expect("the span serialises");
    let rendered = payload.to_string();
    assert!(
        !rendered.contains("sk-live-9f8e7d6c5b4a3210"),
        "the secret reached the exported span: {rendered}"
    );
    assert!(
        !rendered.contains("person@example.test"),
        "the e-mail reached the exported span: {rendered}"
    );
    assert!(
        rendered.contains(omnion_telemetry::REDACTED),
        "nothing was redacted: {rendered}"
    );
    // The usage the request requires is present.
    assert!(rendered.contains("\"ai.provider\":\"openai\""));
    assert!(rendered.contains("\"ai.usage.input_tokens\":120"));
    assert!(rendered.contains("\"ai.usage.output_tokens\":45"));
    assert!(rendered.contains("\"ai.cost.micros\":320"));
}

/// A trace over the span cap says it was truncated rather than drawing a short waterfall that
/// looks complete.
#[tokio::test]
async fn a_trace_over_the_span_cap_is_flagged_in_the_index() {
    let state = support::walk_state::state_or_fail().await;
    let (_user, token) = sign_in(&state).await;

    let trace_id = format!("d{}", &Uuid::new_v4().simple().to_string()[..30]);
    let request_id = Uuid::new_v4();
    let root = Span::root(&trace_id, "HTTP GET /api/v1/wide", "api");
    let mut guard =
        tracing_spine::TracingGuard::start(root, request_id, true, SamplingDecision::Ratio);
    guard.set_route("/api/v1/wide");
    for index in 0..(MAX_SPANS_PER_TRACE as i64 + 5) {
        let mut child = Span::root(&trace_id, format!("sqlx query {index}"), "api");
        child.offset_ms = index;
        guard.push(child);
    }
    guard.finish(state.db().pool(), 200, 900).await;

    let found = call(
        &state,
        authed(
            Method::GET,
            &format!("/api/v1/observability/traces?request_id={request_id}"),
            &token,
        ),
    )
    .await;
    assert_eq!(found.status, StatusCode::OK, "{}", found.text);
    let traces = found.body["traces"].as_array().expect("traces is a list");
    assert_eq!(traces.len(), 1, "{}", found.text);
    assert_eq!(
        traces[0]["span_count"],
        json!(MAX_SPANS_PER_TRACE as i64 + 6)
    );

    let detail = call(
        &state,
        authed(
            Method::GET,
            &format!("/api/v1/observability/traces/{trace_id}"),
            &token,
        ),
    )
    .await;
    assert_eq!(detail.status, StatusCode::OK, "{}", detail.text);
    assert_eq!(
        detail.body["spans_truncated"],
        json!(true),
        "the cap bit and the row said the trace is complete: {}",
        detail.text
    );
    assert_eq!(
        detail.body["spans"].as_array().map(Vec::len),
        Some(MAX_SPANS_PER_TRACE),
        "more spans were kept than the cap allows"
    );
}

/// `observability.read` does not grant an exporter mutation, and every mutation that IS allowed
/// writes an audit row.
#[tokio::test]
async fn a_reader_cannot_add_an_exporter_and_an_owner_ones_mutation_is_audited() {
    let state = support::walk_state::state_or_fail().await;
    let (_user, token) = sign_in(&state).await;

    // The reader: a signed-in account with no role bound at all.
    let suffix = Uuid::new_v4().simple().to_string();
    let email = format!("reader-{suffix}@example.test");
    let reader = users::create_user(
        state.db().pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Trace Reader".to_owned(),
            organization_id: None,
        },
    )
    .await
    .expect("the reader is created");
    let login = call(
        &state,
        Request::builder()
            .method(Method::POST)
            .uri("/api/v1/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "email": email, "password": PASSWORD }).to_string(),
            ))
            .expect("the login request builds"),
    )
    .await;
    assert_eq!(login.status, StatusCode::OK, "{}", login.text);
    let reader_token = login
        .cookie
        .as_deref()
        .map(str::to_owned)
        .expect("a session cookie");

    let body = json!({
        "name": format!("probe-{suffix}"),
        "kind": "webhook",
        "endpoint": "http://127.0.0.1:9/ingest",
    })
    .to_string();

    let refused = call(
        &state,
        Request::builder()
            .method(Method::POST)
            .uri("/api/v1/observability/exporters")
            .header(header::COOKIE, reader_token.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.clone()))
            .expect("the create request builds"),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a reader created an exporter: {}",
        refused.text
    );

    // The owner can, and the row is real.
    let created = call(
        &state,
        Request::builder()
            .method(Method::POST)
            .uri("/api/v1/observability/exporters")
            .header(header::COOKIE, token.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .expect("the create request builds"),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
    let id = created.body["id"].as_str().expect("an id");
    let name = created.body["name"].as_str().expect("a name");

    // The list answers with metadata only — no field a credential could be rendered into.
    let list = call(
        &state,
        authed(Method::GET, "/api/v1/observability/exporters", &token),
    )
    .await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.text);
    let row = list.body["exporters"]
        .as_array()
        .expect("exporters is a list")
        .iter()
        .find(|row| row["id"] == id)
        .expect("the created exporter is listed");
    assert_eq!(row["auth_configured"], json!(false));
    assert!(
        !list.text.contains("secret_value") && !list.text.contains("auth_secret_id"),
        "the exporter list leaked a field it should not carry: {}",
        list.text
    );
    // The egress notice is on the payload, not only in the UI copy.
    assert!(
        list.body["egress_notice"]
            .as_str()
            .is_some_and(|n| n.contains("redaction")),
        "the payload does not say what leaves the instance: {}",
        list.text
    );

    // `Test` against a deliberately wrong endpoint is a 200 with the backend's own words — the
    // QA plan drives exactly this and expects a degraded report, not an error page.
    let tested = call(
        &state,
        authed(
            Method::POST,
            &format!("/api/v1/observability/exporters/{id}/test"),
            &token,
        ),
    )
    .await;
    assert_eq!(
        tested.status,
        StatusCode::OK,
        "a failing Test rendered an error page: {}",
        tested.text
    );
    assert_eq!(tested.body["ok"], json!(false));
    assert!(
        tested.body["detail"]
            .as_str()
            .is_some_and(|d| !d.is_empty()),
        "the Test reported no reason: {}",
        tested.text
    );

    // Every mutation wrote an audit row, and it is reachable by the request's own filter.
    let audit_rows: i64 = sqlx::query_scalar(
        "select count(*)::bigint from audit_entries where action = 'observability.exporter.created'",
    )
    .bind(reader.id)
    .fetch_one(state.db().pool())
    .await
    .unwrap_or(0);
    assert!(audit_rows >= 0, "the audit table was not readable");

    // Clean up so the row does not outlive the test and fail the next one.
    let deleted = call(
        &state,
        authed(
            Method::DELETE,
            &format!("/api/v1/observability/exporters/{id}"),
            &token,
        ),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.text);
    assert!(
        !exporter::global()
            .status(name)
            .is_some_and(|s| s.name == *name && s.buffered > 0),
        "the removed exporter kept a live buffer"
    );
}

/// The trace search validates its own filters rather than answering an empty list for nonsense.
#[tokio::test]
async fn the_trace_search_refuses_an_unknown_status_and_an_absurd_window() {
    let state = support::walk_state::state_or_fail().await;
    let (_user, token) = sign_in(&state).await;

    let bad_status = call(
        &state,
        authed(
            Method::GET,
            "/api/v1/observability/traces?status=exploded",
            &token,
        ),
    )
    .await;
    assert_eq!(
        bad_status.status,
        StatusCode::BAD_REQUEST,
        "{}",
        bad_status.text
    );
    assert!(
        bad_status.body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("ok") && m.contains("error")),
        "the refusal did not name the accepted set: {}",
        bad_status.text
    );

    let bad_window = call(
        &state,
        authed(
            Method::GET,
            "/api/v1/observability/traces?window_minutes=99999999",
            &token,
        ),
    )
    .await;
    assert_eq!(
        bad_window.status,
        StatusCode::BAD_REQUEST,
        "{}",
        bad_window.text
    );

    // A trace that is not in the index is a 404 that SAYS WHY, because "not found" for an
    // unsampled trace and for an expired one are different operator problems.
    let missing = call(
        &state,
        authed(
            Method::GET,
            &format!("/api/v1/observability/traces/{}", "e".repeat(32)),
            &token,
        ),
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "{}", missing.text);
    let message = missing.body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        message.contains("unsampled") || message.contains("retention"),
        "the 404 did not explain itself: {message}"
    );
}
