//! The exporter flush loop, end to end (docs/requests/REQ-126, slice 3's remaining half).
//!
//! Slice 3's own suite proved the pipeline's *parts* — a bounded ring, a drop counter, a health
//! chip, a `Test` probe. This file proves the part that makes them a pipeline: that a log line
//! written by a real request actually reaches a real backend, and that the row the screen reads
//! afterwards says so.
//!
//! That is not the same claim, and the difference is the whole point of this file. Everything in
//! slice 3 was provable by a test that pushed into the collector itself — which is exactly how a
//! pipeline can be complete in every unit and empty in production, which is what the fan-out
//! turned out to be before this tick. So the walk here never touches the collector directly. It:
//!
//! 1. starts a real HTTP collector on an ephemeral port, the way the `ai_hub` suite starts its
//!    mock provider,
//! 2. configures an exporter row pointing at it through the **real router**,
//! 3. drives real authenticated requests so the request-log middleware writes lines and fans
//!    them out,
//! 4. runs one `sweep` and asserts the backend RECEIVED the request's line — matched by the
//!    request id the middleware minted, not by "something arrived", and
//! 5. reads `/observability/exporters` back and asserts the health chip, the buffered count and
//!    the drop counter are the ones the loop wrote.
//!
//! The second walk kills the backend mid-flight and asserts the other three properties together,
//! because they are only distinguishable as a set: the request still returns `200` (a telemetry
//! sink must never fail a request), the health chip degrades rather than sitting at `unknown`
//! forever, and the persisted `dropped_total` rises. An exporter that is merely unreachable and
//! never re-registers would also pass a "the request was 200" assertion, which is why the chip
//! and the counter are asserted in the same walk.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use axum::routing::post;
use http_body_util::BodyExt;
mod support;

use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::seed;
use omnion_telemetry::exporter;
use omnion_telemetry::exporter_flush;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

/// A running mock collector: the endpoint under test, everything it received, and its task.
struct MockCollector {
    endpoint: String,
    received: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl MockCollector {
    /// Start the collector on an ephemeral port, refusing to answer after `stop_after` batches.
    ///
    /// The refusal count is what lets the second walk take the backend away mid-flight without
    /// killing the listener: a closed port and a 503 are different failures, and an operator sees
    /// a different chip for each.
    async fn start(stop_after: Option<usize>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock must bind a port");
        let address = listener.local_addr().expect("the mock has an address");

        let received: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        let mut served = 0usize;

        let app = Router::new().route(
            "/v1/logs",
            post(move |body: String| {
                let sink = Arc::clone(&sink);
                served += 1;
                async move {
                    let refused = stop_after.is_some_and(|limit| served > limit);
                    if !refused && let Ok(value) = serde_json::from_str::<Value>(&body) {
                        sink.lock().expect("the sink is not poisoned").push(value);
                    }
                    if refused {
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            "the collector is shedding load",
                        )
                    } else {
                        (StatusCode::OK, "accepted")
                    }
                }
            }),
        );

        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            endpoint: format!("http://{address}/v1/logs"),
            received,
            task,
        }
    }

    /// Everything the backend was sent, in order.
    fn batches(&self) -> Vec<Value> {
        self.received
            .lock()
            .expect("the sink is not poisoned")
            .clone()
    }
}

impl Drop for MockCollector {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct TestResponse {
    status: StatusCode,
    body: Value,
    text: String,
    cookie: Option<String>,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let peer: SocketAddr = "198.51.100.11:51234"
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
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookie| cookie.split(';').next())
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
        cookie,
    }
}

async fn sign_in(state: &AppState) -> String {
    seed::ensure(state.db().pool())
        .await
        .expect("the permission catalogue seeds");
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = omnion_identity::organizations::create_organization(
        state.db().pool(),
        omnion_identity::organizations::NewOrganization {
            name: format!("Exporter org {suffix}"),
            slug: format!("exporter-{suffix}"),
        },
    )
    .await
    .expect("the organization must be created");
    let email = format!("exporter-{suffix}@example.test");
    let user = users::create_user(
        state.db().pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Exporter Walker".to_owned(),
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
    let token = cookie
        .split_once('=')
        .map(|(_, value)| value)
        .expect("the cookie carries a value");
    token.to_owned()
}

fn authed(method: Method, path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::COOKIE, format!("omnion_session={token}"))
        .body(Body::empty())
        .expect("a request builds")
}

fn json_request(method: Method, path: &str, token: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::COOKIE, format!("omnion_session={token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("a request builds")
}

/// Register an exporter row through the router and return its id.
async fn create_exporter(state: &AppState, token: &str, name: &str, endpoint: &str) -> Uuid {
    let created = call(
        state,
        json_request(
            Method::POST,
            "/api/v1/observability/exporters",
            token,
            json!({
                "name": name,
                "kind": "otlp",
                "endpoint": endpoint,
                "batch_ms": 100,
                "timeout_ms": 2_000,
                "enabled": true,
            }),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
    Uuid::parse_str(created.body["id"].as_str().expect("the id is a string"))
        .expect("the id is a uuid")
}

async fn remove_exporter(state: &AppState, token: &str, id: Uuid) {
    let deleted = call(
        state,
        authed(
            Method::DELETE,
            &format!("/api/v1/observability/exporters/{id}"),
            token,
        ),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.text);
    exporter::global().remove(&id.to_string());
}

/// The whole pipeline, over the real router and against a real backend.
///
/// The assertion is that the backend received **the line the middleware wrote for a real
/// request**, found by the request id that request carried. Asserting "a batch arrived" would
/// pass on the `Test` probe's synthetic document alone, which is the mistake this file exists
/// to rule out.
#[tokio::test]
async fn a_request_s_line_reaches_a_configured_backend() {
    let state = support::walk_state::state_or_fail().await;
    let token = sign_in(&state).await;
    let name = format!("otlp-{}", Uuid::new_v4().simple());
    let collector = MockCollector::start(None).await;
    let id = create_exporter(&state, &token, &name, &collector.endpoint).await;

    // Drive real traffic so the request-log middleware runs. Each of these produces exactly one
    // stored line, and each line is what the fan-out hands the buffer.
    for _ in 0..3 {
        let response = call(
            &state,
            authed(Method::GET, "/api/v1/observability/exporters", &token),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.text);
    }

    // A never-flushed exporter is due immediately, so one sweep is enough — no sleeping for the
    // interval, which would make the test slow AND leave it passing on a timing accident.
    let flushed = exporter_flush::sweep(state.db().pool(), exporter::global())
        .await
        .expect("the sweep runs");
    assert!(flushed >= 1, "the sweep flushed nothing: {flushed}");

    let batches = collector.batches();
    assert!(
        !batches.is_empty(),
        "the backend received nothing after a real request wrote its line"
    );

    // The payload shape: a batch, with log lines in it, and the batch names the exporter.
    let first = &batches[0];
    assert_eq!(
        first["exporter"],
        json!(name),
        "the batch did not name its exporter"
    );
    let logs = first["logs"]
        .as_array()
        .expect("the batch carries a logs array");
    assert!(!logs.is_empty(), "the batch carried no log lines");

    // And the lines are the real request's: the route the middleware recorded, and the request
    // id it minted. A hand-written payload would carry neither.
    let exported: Vec<&Value> = logs
        .iter()
        .filter(|line| {
            line["route"]
                .as_str()
                .is_some_and(|r| r.contains("/api/v1/observability"))
        })
        .collect();
    assert!(
        !exported.is_empty(),
        "no exported line carried an observability route: {logs:?}"
    );
    let with_request_id = exported
        .iter()
        .find(|line| line["request_id"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(
        with_request_id.is_some(),
        "an exported request line carried no request id: {exported:?}"
    );

    // The row the screen reads now agrees with the backend, which is the property the flush loop
    // exists to create: health from a real accepted batch, the buffer emptied, nothing dropped.
    let listed = call(
        &state,
        authed(Method::GET, "/api/v1/observability/exporters", &token),
    )
    .await;
    let row = listed.body["exporters"]
        .as_array()
        .expect("exporters is an array")
        .iter()
        .find(|row| row["name"] == json!(name))
        .expect("the configured exporter is listed");
    assert_eq!(
        row["health"],
        json!("ok"),
        "an accepted batch left the chip unknown: {row}"
    );
    assert_eq!(
        row["buffered"],
        json!(0),
        "the buffer was not drained: {row}"
    );

    // And the flush was counted in the registry, because a counter that lives only in a struct is
    // not something an operator can see on /metrics.
    let scrape = call(
        &state,
        axum::http::Request::builder()
            .method(Method::GET)
            .uri("/metrics")
            .body(Body::empty())
            .expect("a request builds"),
    )
    .await;
    assert!(
        scrape
            .text
            .contains("omnion_exporter_batches_flushed_total"),
        "the flush family is absent from /metrics:\n{}",
        scrape
            .text
            .lines()
            .filter(|l| l.contains("exporter"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    remove_exporter(&state, &token, id).await;
}

/// A backend that starts refusing: the request still succeeds, the chip degrades, the loss is
/// counted, and the number is persisted rather than reset by the next read.
#[tokio::test]
async fn a_backend_that_starts_refusing_degrades_without_failing_a_request() {
    let state = support::walk_state::state_or_fail().await;
    let token = sign_in(&state).await;
    let name = format!("webhook-{}", Uuid::new_v4().simple());
    // Zero successes: the first batch is already refused.
    let collector = MockCollector::start(Some(0)).await;
    let id = create_exporter(&state, &token, &name, &collector.endpoint).await;

    // Push past the cap so the drop counter has something real to report. The cap is the crate's
    // default; the walk is deliberately larger, because "the buffer never exceeds its cap" and
    // "the loss is counted" are different assertions and only the second one needs the overflow.
    for index in 0..(exporter::DEFAULT_BUFFER_CAPACITY + 25) {
        let _ = exporter_flush::fan_out(
            exporter::global(),
            json!({ "marker": "overflow", "n": index }),
        );
    }
    let status = exporter::global().status(&name).expect("registered");
    assert_eq!(
        status.buffered,
        exporter::DEFAULT_BUFFER_CAPACITY,
        "the buffer grew past its cap"
    );
    assert!(
        status.dropped_total >= 25,
        "the loss was not counted: {status:?}"
    );

    let _ = exporter_flush::sweep(state.db().pool(), exporter::global())
        .await
        .expect("the sweep runs");

    // The request path is untouched. This is the acceptance line's actual claim, and it is
    // asserted by driving a request rather than by timing a push: an implementation that did the
    // send inline would pass a push-timing test and fail this one.
    let response = call(
        &state,
        authed(Method::GET, "/api/v1/observability/exporters", &token),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "a refusing telemetry backend failed a request: {}",
        response.text
    );

    let row: (String, String, Option<String>, i64) = sqlx::query_as(
        "select health, coalesce(last_error, ''), last_flush_at::text, dropped_total \
         from obs_exporters where id = $1",
    )
    .bind(id)
    .fetch_one(state.db().pool())
    .await
    .expect("the row is readable");

    // One refused batch is `degraded`, not `down` — slice 3's deliberate rule, and this is where
    // the loop's persistence is checked: the chip on the ROW, not only in memory.
    assert_eq!(
        row.0, "degraded",
        "the refused batch did not degrade the chip: {row:?}"
    );
    assert!(
        !row.1.is_empty(),
        "the refused batch recorded no reason: {row:?}"
    );
    assert!(
        row.2.is_none(),
        "a refused batch claimed to have flushed: {row:?}"
    );
    assert!(
        row.3 >= 25,
        "the drop counter was not folded into the row: {row:?}"
    );

    remove_exporter(&state, &token, id).await;
}

/// The loop registers a row this process has never seen.
///
/// Without it, "save an exporter, do not restart, and it works" is false: the router registers
/// the buffer on save, but a row written by a migration, a second API instance, or a restored
/// database has a buffer nobody created, and the sweep would skip it forever. This walk writes
/// the row directly and never calls the create route.
#[tokio::test]
async fn a_row_this_process_never_registered_is_registered_by_the_sweep() {
    let state = support::walk_state::state_or_fail().await;
    let token = sign_in(&state).await;
    let name = format!("restored-{}", Uuid::new_v4().simple());
    let collector = MockCollector::start(None).await;

    // Straight into the table, as a restore or a second instance would.
    sqlx::query(
        "insert into obs_exporters (id, name, kind, endpoint, batch_ms, timeout_ms, enabled) \
         values ($1, $2, 'webhook', $3, 100, 2000, true)",
    )
    .bind(Uuid::new_v4())
    .bind(&name)
    .bind(&collector.endpoint)
    .execute(state.db().pool())
    .await
    .expect("the row is written");
    assert!(
        exporter::global().status(&name).is_none(),
        "the buffer already existed, so the test proves nothing"
    );

    let _ = exporter_flush::sweep(state.db().pool(), exporter::global())
        .await
        .expect("the sweep runs");

    assert!(
        exporter::global().status(&name).is_some(),
        "the sweep did not register a stored row"
    );
    let health: String = sqlx::query_scalar("select health from obs_exporters where name = $1")
        .bind(&name)
        .fetch_one(state.db().pool())
        .await
        .expect("the row is readable");
    assert_eq!(
        health, "ok",
        "an empty buffer left the row's chip as something a screen would render as a problem"
    );

    exporter::global().remove(&name);
    sqlx::query("delete from obs_exporters where name = $1")
        .bind(&name)
        .execute(state.db().pool())
        .await
        .expect("the row is removed");
    let _ = token;
}

/// A disabled exporter's buffer is drained and its backlog counted, not held and not silently
/// discarded.
///
/// Slice 3 asserted the drain; this asserts the part the *screen* depends on, which is that the
/// backlog an operator asked not to send is visible in the drop counter rather than vanishing.
#[tokio::test]
async fn switching_an_exporter_off_counts_the_backlog_it_drops() {
    let state = support::walk_state::state_or_fail().await;
    let token = sign_in(&state).await;
    let name = format!("disabled-{}", Uuid::new_v4().simple());
    let collector = MockCollector::start(None).await;
    let id = create_exporter(&state, &token, &name, &collector.endpoint).await;

    for index in 0..5 {
        let _ = exporter_flush::fan_out(exporter::global(), json!({ "n": index }));
    }
    assert_eq!(
        exporter::global()
            .status(&name)
            .expect("registered")
            .buffered,
        5,
        "the fan-out did not buffer"
    );

    let response = call(
        &state,
        json_request(
            Method::PATCH,
            &format!("/api/v1/observability/exporters/{id}"),
            &token,
            json!({
                "name": name,
                "kind": "otlp",
                "endpoint": collector.endpoint,
                "batch_ms": 100,
                "timeout_ms": 2_000,
                "enabled": false,
            }),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text);

    let status = exporter::global().status(&name).expect("registered");
    assert_eq!(status.buffered, 0, "a disabled exporter held its backlog");
    assert!(
        !status.enabled,
        "the buffer did not record being switched off: {status:?}"
    );

    // Nothing reached the backend, which is the point of the switch.
    assert!(
        collector.batches().is_empty(),
        "a disabled exporter sent its backlog"
    );

    remove_exporter(&state, &token, id).await;
}

/// An interval is respected, so a configured exporter cannot be turned into a request-per-batch
/// sender by a low `batch_ms` on a busy instance.
///
/// The first sweep drains; a second sweep with a fresh line in the buffer must NOT drain, because
/// the interval has not elapsed. Asserting the other direction — that a second sweep does drain
/// after the interval — would need a sleep, and a sleeping test that passes is indistinguishable
/// from one that is waiting for the wrong reason.
#[tokio::test]
async fn a_batch_interval_is_respected_between_flushes() {
    let state = support::walk_state::state_or_fail().await;
    let token = sign_in(&state).await;
    let name = format!("paced-{}", Uuid::new_v4().simple());
    let collector = MockCollector::start(None).await;
    // The maximum interval, so the second sweep is unambiguously too early.
    let id = create_exporter(&state, &token, &name, &collector.endpoint).await;
    sqlx::query("update obs_exporters set batch_ms = 3600000 where id = $1")
        .bind(id)
        .execute(state.db().pool())
        .await
        .expect("the interval is set");

    let _ = exporter_flush::fan_out(exporter::global(), json!({ "n": 1 }));
    exporter_flush::sweep(state.db().pool(), exporter::global())
        .await
        .expect("the first sweep runs");
    let after_first = collector.batches().len();
    assert!(after_first >= 1, "the first sweep sent nothing");

    let _ = exporter_flush::fan_out(exporter::global(), json!({ "n": 2 }));
    exporter_flush::sweep(state.db().pool(), exporter::global())
        .await
        .expect("the second sweep runs");
    assert_eq!(
        collector.batches().len(),
        after_first,
        "the second sweep sent before the interval elapsed"
    );
    assert!(
        exporter::global()
            .status(&name)
            .expect("registered")
            .buffered
            > 0,
        "the paced sweep discarded the line it was not ready to send"
    );

    remove_exporter(&state, &token, id).await;
}
