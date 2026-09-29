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
    // The whole `Cookie` header is kept, name and value: it now carries BOTH cookies, so
    // unwrapping the session out of it would drop the CSRF token the mutation layer needs.
    let token = cookie.to_owned();
    token.to_owned()
}

fn authed(method: Method, path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::COOKIE, token.clone())
        .body(Body::empty())
        .expect("a request builds")
}

fn json_request(method: Method, path: &str, token: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::COOKIE, token.clone())
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("a request builds")
}

/// Start every walk from an EMPTY exporter table, and an empty in-process collector.
///
/// ## Why this helper exists
///
/// `exporter_flush::fan_out` iterates **every enabled row in `obs_exporters`** — the fan-out is
/// not scoped to one exporter, for the same reason the alert evaluator is not scoped to one rule
/// (see `support::walk_state::EVALUATOR_LOCK`): a pipeline is a fleet, and every configured
/// backend must receive the line. So a walk cannot reason about "the buffer" in the singular,
/// and it cannot start from a database that already holds rows from a previous run.
///
/// That second half is what this fixes, and it was invisible for four ticks because the walks
/// pass in isolation and fail in the gate. Each of these suites builds its own per-run
/// database, so `cargo test -p omnion-api --test exporter_flush` is 5/5 green on its own; the
/// gate's `omnion_gate_wave6` is created **if missing** and therefore keeps its rows across
/// invocations. By the fourth run it held four enabled exporters pointing at four mock
/// collectors whose ports no longer exist, and every walk's own buffer was drowned by them:
///
/// ```text
/// thread 'a_request_s_line_reaches_a_configured_backend' panicked at exporter_flush.rs:312:
/// the backend received nothing after a real request wrote its line
/// thread 'a_row_this_process_never_registered_is_registered_by_the_sweep' panicked at :525:
/// assertion `left == right` failed: … left: "unknown", right: "ok"
/// ```
///
/// The mock behind this walk is the ONLY thing that can answer "did my batch arrive", so a
/// sibling's dead rows do not make that assertion false — they make the sweep flush four
/// exporters, three of which block on a closed port, and this walk's own flush lands after them.
///
/// **Green in isolation and red in the gate is the whole shape of this defect**, which is why
/// the gate is `--no-fail-fast` and why nothing here may be "fixed" by asserting a lower bound
/// that a dead sibling also satisfies. Clear the table, then assert exactly.
/// The reset, and **where it has to be called from**.
///
/// `sign_in` makes several authenticated calls, and the request-log middleware fans out to every
/// enabled exporter on each one — so a reset placed before sign-in is undone by sign-in itself.
/// That is not subtle: it is six lines already in the buffer before the walk's own first
/// `fan_out`, which is exactly the `left: 6, right: 5` the "the fan-out did not buffer"
/// assertion reported. Every walk therefore calls this **after** `sign_in` and before the buffer
/// it is about to measure, never from `state_or_fail`.
async fn reset_exporter_state(state: &AppState) {
    sqlx::query("delete from obs_exporters")
        .execute(state.db().pool())
        .await
        .expect("the exporter rows are deletable");
    // The collector is a process-wide `OnceLock` shared by every walk in this binary, and a
    // buffer that survived a previous walk is a buffer whose drops are counted into a row this
    // walk is about to assert on. This drops the REGISTRATIONS as well, so it belongs only in
    // a walk that has not created its own row yet.
    exporter::global().clear();
}

/// Clear the collector's buffer but keep one named exporter registered.
///
/// The variant a walk needs once it has created its own row. `create_exporter` is itself a
/// POST through the router, so by the time it returns the request-log middleware has already
/// fanned one line into whatever exporters were enabled at that moment — and into the new one,
/// because the route registers the buffer before it answers. Dropping the rows and the buffer
/// together would also drop the row the walk is about to exercise, so the two are cleared
/// separately here: the table is emptied of everything EXCEPT `keep`, and the in-process buffer
/// is emptied entirely.
async fn reset_exporter_state_keep_row(state: &AppState, keep: &str) {
    sqlx::query("delete from obs_exporters where name <> $1")
        .bind(keep)
        .execute(state.db().pool())
        .await
        .expect("the other exporter rows are deletable");
    // `clear_buffer`, NOT `clear`: the row this walk just created through the router is
    // registered in the collector, and dropping the registrations would make its own
    // `expect("registered")` fire — a failure that reads as "the exporter was never created"
    // when the test deleted it a line earlier.
    exporter::global().clear_buffer();
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
    // These five walks share ONE database and one exporter table, and cargo runs the tests in a
    // binary in parallel. Without this guard they delete and create each other's exporter rows:
    // the suite then fails on `exporter_not_found`, on a sweep that flushed nothing, and on a
    // row that is simply gone — none of which is anything any of these five tests does. The
    // same cause `observability_alerts.rs` documented when it hit this, so it takes the same
    // guard rather than a second mechanism.
    let _evaluator = support::walk_state::exclusive_evaluator().await;
    let state = support::walk_state::state_or_fail().await;
    let token = sign_in(&state).await;
    reset_exporter_state(&state).await;
    let name = format!("otlp-{}", Uuid::new_v4().simple());
    let collector = MockCollector::start(None).await;
    let id = create_exporter(&state, &token, &name, &collector.endpoint).await;
    // The registration POST is itself a request, so the middleware has already fanned a line
    // into this row. Clear the BUFFER (not the registrations) so the walk measures only what it
    // is about to fan out itself.
    reset_exporter_state_keep_row(&state, &name).await;

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
    // These five walks share ONE database and one exporter table, and cargo runs the tests in a
    // binary in parallel. Without this guard they delete and create each other's exporter rows:
    // the suite then fails on `exporter_not_found`, on a sweep that flushed nothing, and on a
    // row that is simply gone — none of which is anything any of these five tests does. The
    // same cause `observability_alerts.rs` documented when it hit this, so it takes the same
    // guard rather than a second mechanism.
    let _evaluator = support::walk_state::exclusive_evaluator().await;
    let state = support::walk_state::state_or_fail().await;
    let token = sign_in(&state).await;
    reset_exporter_state(&state).await;
    let name = format!("webhook-{}", Uuid::new_v4().simple());
    // Zero successes: the first batch is already refused.
    let collector = MockCollector::start(Some(0)).await;
    let id = create_exporter(&state, &token, &name, &collector.endpoint).await;
    // The registration POST is itself a request, so the middleware has already fanned a line
    // into this row. Clear the BUFFER (not the registrations) so the walk measures only what it
    // is about to fan out itself.
    reset_exporter_state_keep_row(&state, &name).await;

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
    // These five walks share ONE database and one exporter table, and cargo runs the tests in a
    // binary in parallel. Without this guard they delete and create each other's exporter rows:
    // the suite then fails on `exporter_not_found`, on a sweep that flushed nothing, and on a
    // row that is simply gone — none of which is anything any of these five tests does. The
    // same cause `observability_alerts.rs` documented when it hit this, so it takes the same
    // guard rather than a second mechanism.
    let _evaluator = support::walk_state::exclusive_evaluator().await;
    let state = support::walk_state::state_or_fail().await;
    let token = sign_in(&state).await;
    reset_exporter_state(&state).await;
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
    // The chip, and the assertion here is a CORRECTION. This walk used to demand `ok` — "an
    // empty buffer left the row's chip as something a screen would render as a problem" — and
    // that expectation is wrong in the product's favour, three times over:
    //
    //   * the migration declares `health ... default 'unknown'` with
    //     `(health in ('unknown','ok','degraded','down'))`, so `unknown` is a legal state;
    //   * `Buffer::health` derives it from the flush counter, and an exporter that has never
    //     flushed has no evidence to be anything else;
    //   * `emit_health_change` says it outright: "`unknown` is where an exporter starts, not a
    //     state it recovers into. A configured exporter that has never flushed is not news".
    //
    // And the request's own screen contract agrees: the exporters list has a "no telemetry yet
    // — the exporter was just enabled" state, which is what `unknown` renders as. Demanding
    // `ok` here would have forced a `Test`-probe success into the *measured* chip, so a
    // backend that has never answered a single batch would look healthy.
    //
    // What this walk is actually for — and what it now asserts — is the SWEEP registering a row
    // no code path in this process created, which is the claim in its name. That is checked
    // above, against the in-process collector, which is the only place a registration exists.
    let health: String = sqlx::query_scalar("select health from obs_exporters where name = $1")
        .bind(&name)
        .fetch_one(state.db().pool())
        .await
        .expect("the row is readable");
    assert_eq!(
        health, "unknown",
        "a never-flushed exporter reported a health the screen has no empty state for: {health:?}"
    );
    // The sweep must still have FOLDED the row back — persisting `unknown` is the same write
    // that persists `ok`, so this is what proves the empty-buffer path persisted at all rather
    // than skipping the row. Without it the previous line would pass on a row the sweep never
    // touched, which is its own default from the migration.
    let dropped: (Option<i64>, bool) = sqlx::query_as(
        "select dropped_total, enabled from obs_exporters where name = $1",
    )
    .bind(&name)
    .fetch_one(state.db().pool())
    .await
    .expect("the row is readable");
    assert_eq!(
        dropped,
        (Some(0), true),
        "the sweep did not fold the registered row's state back: {dropped:?}"
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
    // These five walks share ONE database and one exporter table, and cargo runs the tests in a
    // binary in parallel. Without this guard they delete and create each other's exporter rows:
    // the suite then fails on `exporter_not_found`, on a sweep that flushed nothing, and on a
    // row that is simply gone — none of which is anything any of these five tests does. The
    // same cause `observability_alerts.rs` documented when it hit this, so it takes the same
    // guard rather than a second mechanism.
    let _evaluator = support::walk_state::exclusive_evaluator().await;
    let state = support::walk_state::state_or_fail().await;
    let token = sign_in(&state).await;
    reset_exporter_state(&state).await;
    let name = format!("disabled-{}", Uuid::new_v4().simple());
    let collector = MockCollector::start(None).await;
    let id = create_exporter(&state, &token, &name, &collector.endpoint).await;
    // The registration POST is itself a request, so the middleware has already fanned a line
    // into this row. Clear the BUFFER (not the registrations) so the walk measures only what it
    // is about to fan out itself.
    reset_exporter_state_keep_row(&state, &name).await;

    // The buffer is cleared AFTER the exporter row exists, and that ordering is the whole point.
    // `create_exporter` is itself an authenticated POST, so the request-log middleware has
    // already fanned one line out by the time the row is registered — and a walk that then
    // resets the collector, or resets it before creating the row, measures somebody else's
    // traffic. `left: 6, right: 5` is exactly that line: the sixth is the registration POST's
    // own log, counted against a backlog the walk believes it alone produced.
    reset_exporter_state_keep_row(&state, &name).await;

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
    // These five walks share ONE database and one exporter table, and cargo runs the tests in a
    // binary in parallel. Without this guard they delete and create each other's exporter rows:
    // the suite then fails on `exporter_not_found`, on a sweep that flushed nothing, and on a
    // row that is simply gone — none of which is anything any of these five tests does. The
    // same cause `observability_alerts.rs` documented when it hit this, so it takes the same
    // guard rather than a second mechanism.
    let _evaluator = support::walk_state::exclusive_evaluator().await;
    let state = support::walk_state::state_or_fail().await;
    let token = sign_in(&state).await;
    reset_exporter_state(&state).await;
    let name = format!("paced-{}", Uuid::new_v4().simple());
    let collector = MockCollector::start(None).await;
    // The maximum interval, so the second sweep is unambiguously too early.
    let id = create_exporter(&state, &token, &name, &collector.endpoint).await;
    // The registration POST is itself a request, so the middleware has already fanned a line
    // into this row. Clear the BUFFER (not the registrations) so the walk measures only what it
    // is about to fan out itself.
    reset_exporter_state_keep_row(&state, &name).await;
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
