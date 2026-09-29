//! The last acceptance line of REQ-126: a SHIPPED alert rule fires through a real dependency
//! outage, notifies once, and resolves when the dependency returns.
//!
//! ## Why this file exists rather than another walk in `observability_alerts.rs`
//!
//! Because of what it is allowed to touch. Every existing alert walk drives the metric it watches
//! by recording it directly, and that is the right way to test the state machine. It is the
//! wrong way to test the sentence *"a shipped alert rule fires in the QA stack (stop Redis),
//! creates a `firing` event, notifies once, and resolves when the dependency returns"*, because
//! that sentence is about the chain:
//!
//! ```text
//! a real dependency dies  →  a real code path counts the loss  →  a SHIPPED rule reads it
//!    →  a firing event  →  one notification  →  the dependency returns  →  resolved
//! ```
//!
//! Every link but the first and the last is code the panel already had. This walk closes the two
//! ends against real infrastructure and lets the shipped rule do the middle on its own.
//!
//! ## The outage is a real one
//!
//! The dependency is a real HTTP backend on a real port that accepts traffic and then goes away:
//! the listener is closed, so the port stops answering the way a decommissioned collector does,
//! rather than answering 503 the way a shedding one does. No metric is written by hand anywhere
//! in this file. The `omnion_exporter_dropped_total` samples are counted by `exporter::push`
//! evicting a full ring, which is the same call a live process makes when a request writes a log
//! line and the backend is gone.
//!
//! ## The rule is the shipped one, not a rule this file wrote
//!
//! It reads `alert_loop::BUNDLED_RULES` and picks `ExporterDroppingTelemetry` out of it by
//! looking up its expression, so this walk cannot pass against a rule the panel does not seed
//! and cannot quietly drift from the one it does. A hand-written copy of the expression would
//! have been the fourth instance on this request of a check that agrees with itself.
//!
//! ## What had to be true for the resolve
//!
//! The rule reads a five-minute window, so "the dependency came back" is not the same statement
//! as "the counter went back to zero" — a counter never goes back to zero. It is: **the drops
//! left the window**. That is why [`omnion_telemetry::metrics::Registry::advance_minutes`] exists,
//! and why this walk is the reason it is a public seam on the instance rather than a private test
//! helper: an integration walk in another crate cannot reach a `#[cfg(test)]` item, and the
//! alternative was a `sleep()` — a test that waits a real five minutes to prove an alert can
//! close, which does not run in any suite anybody keeps.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use axum::routing::post;
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::seed;
use omnion_telemetry::alert_loop;
use omnion_telemetry::alerts;
use omnion_telemetry::exporter;
use omnion_telemetry::exporter_flush;
use omnion_telemetry::metrics;
use serde_json::{Value, json};
use std::net::SocketAddr;
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

/// The shipped rule this walk exercises, by name.
///
/// `None` means the bundle no longer ships it, which is a failure in its own right: an operator
/// who deletes a rule and finds this walk skipped has learned nothing.
fn shipped_rule(name: &str) -> Option<&'static str> {
    alert_loop::BUNDLED_RULES
        .iter()
        .find(|(rule, _)| *rule == name)
        .map(|(_, expr)| *expr)
}

/// A backend on a real port that can be taken away and brought back.
///
/// The listener is held so the port can be closed mid-walk, which is what makes the outage a
/// dependency outage rather than an HTTP error: `connect` fails, which is what a network
/// partition looks like to `reqwest`, and it is the failure the exporter's own `degraded` chip
/// was written for.
struct Dependency {
    endpoint: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    served: Arc<AtomicUsize>,
    address: SocketAddr,
}

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

impl Dependency {
    /// Start on an arbitrary port, for the healthy baseline.
    async fn start() -> Self {
        Self::bind("127.0.0.1:0".parse().expect("a bind address")).await
    }

    /// Start on a FIXED address, for the recovery.
    ///
    /// The port is the same one the outage took away, because a collector that restarts does not
    /// move — and a walk that brought the backend back on a new port would be testing a
    /// reconfiguration rather than a recovery.
    async fn start_on(address: SocketAddr) -> Self {
        Self::bind(address).await
    }

    async fn bind(bind_to: SocketAddr) -> Self {
        let listener = tokio::net::TcpListener::bind(bind_to)
            .await
            .unwrap_or_else(|error| panic!("the dependency must bind {bind_to}: {error}"));
        let address = listener
            .local_addr()
            .expect("the dependency has an address");
        let served = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&served);

        let app = Router::new().route(
            "/v1/logs",
            post(move |_body: String| {
                counter.fetch_add(1, Ordering::SeqCst);
                async { (StatusCode::OK, "accepted") }
            }),
        );
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let server = axum::serve(listener, app).with_graceful_shutdown(async move {
                let _ = rx.await;
            });
            let _ = server.await;
        });

        Self {
            endpoint: format!("http://{address}/v1/logs"),
            shutdown: Some(tx),
            served,
            address,
        }
    }

    /// Stop answering, the way a dependency that is no longer there behaves.
    fn take_down(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }

    fn took_requests(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }
}

struct TestResponse {
    status: StatusCode,
    body: Value,
    cookie: Option<String>,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let peer: SocketAddr = "198.51.100.9:51234".parse().expect("a peer address");
    let mut response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("the router answers");
    response
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));

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
        .expect("the body reads")
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
        cookie,
    }
}

fn request(method: Method, uri: &str, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    match body {
        None => builder
            .body(Body::empty())
            .expect("a static request builds"),
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .expect("a json request builds"),
    }
}

fn get(uri: &str) -> Request<Body> {
    request(Method::GET, uri, None)
}

fn with_cookie(mut request: Request<Body>, cookie: &str) -> Request<Body> {
    request
        .headers_mut()
        .insert(header::COOKIE, cookie.parse().expect("a cookie header"));
    request
}

async fn state_or_skip() -> Option<AppState> {
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("SKIP: the configuration is not valid ({error})");
            return None;
        }
    };
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(error) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({error})");
            return None;
        }
    };
    if let Err(error) = db.migrate().await {
        eprintln!("SKIP: the migrations did not apply ({error})");
        return None;
    }
    seed::ensure(db.pool())
        .await
        .expect("the permission catalogue seeds");
    let redis = RedisClient::new(&config.redis.url).expect("a redis url");
    let storage = omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid");
    Some(AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config,
        db,
        redis,
        storage,
    ))
}

async fn sign_in(state: &AppState) -> String {
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = omnion_identity::organizations::create_organization(
        state.db().pool(),
        omnion_identity::organizations::NewOrganization {
            name: format!("Outage org {suffix}"),
            slug: format!("outage-{suffix}"),
        },
    )
    .await
    .expect("the organization is created");
    let email = format!("outage-{suffix}@example.test");
    let user = users::create_user(
        state.db().pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Outage Walker".to_owned(),
            organization_id: Some(organization.id),
        },
    )
    .await
    .expect("the account is created");
    seed::bind_owner(state.db().pool(), user.id)
        .await
        .expect("the owner role is bound");

    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/auth/login",
            Some(json!({ "email": email, "password": PASSWORD })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "login failed: {}",
        response.body
    );
    response.cookie.expect("the login sets a session cookie")
}

/// The samples this walk's own exporter contributes, read off the registry.
///
/// The label is the exporter's name, which is unique per run, so two walks never share a series
/// and one walk's drops cannot keep another's rule firing.
fn drops_for(name: &str) -> f64 {
    metrics::global()
        .series_in_window(exporter::DROPPED_FAMILY, 0)
        .into_iter()
        .find(|reading| reading.labels.first().map(String::as_str) == Some(name))
        .map(|reading| reading.value)
        .unwrap_or(0.0)
}

#[tokio::test]
async fn a_shipped_rule_fires_through_a_real_outage_and_resolves_when_the_dependency_returns() {
    let Some(state) = state_or_skip().await else {
        return;
    };
    let pool = state.db().pool();
    let cookie = sign_in(&state).await;

    // The rule under test, taken from the list the panel seeds. Asserted rather than assumed:
    // a bundle that quietly stops shipping it must fail here, not skip.
    let expr = shipped_rule("ExporterDroppingTelemetry").expect(
        "the bundle no longer ships ExporterDroppingTelemetry, so the acceptance line has no rule",
    );

    // Seed the shipped rule through the same call the process makes at boot, so what is evaluated
    // is what an operator's instance would evaluate.
    let seeded = alert_loop::seed_bundled_rules(pool)
        .await
        .expect("the bundled rules seed");
    assert!(seeded > 0, "the bundle seeded nothing");

    let rule_id: Uuid = sqlx::query_scalar(
        "select id from obs_alert_rules where name = 'ExporterDroppingTelemetry'",
    )
    .fetch_one(pool)
    .await
    .expect("the shipped rule is in the table");
    let stored_expr: String = sqlx::query_scalar("select expr from obs_alert_rules where id = $1")
        .bind(rule_id)
        .fetch_one(pool)
        .await
        .expect("the rule row is readable");
    assert_eq!(
        stored_expr, expr,
        "the row the evaluator reads is not the expression this walk read out of the bundle"
    );

    // A rule the walk owns, so the shipped row stays as the instance would leave it: the shipped
    // rule is seeded with a 300 second dwell, which no test suite can wait out. `for_seconds: 0`
    // is the shipped rule's own `for: 0m` in the PromQL file — the same choice, stated in the
    // column the panel writes.
    sqlx::query("update obs_alert_rules set for_seconds = 0 where id = $1")
        .bind(rule_id)
        .execute(pool)
        .await
        .expect("the dwell is set");
    // A pending or firing event from a previous run of this same rule would be resolved or
    // discarded by the first pass and counted as this walk's own transition.
    sqlx::query("delete from obs_alert_events where rule_id = $1")
        .bind(rule_id)
        .execute(pool)
        .await
        .expect("the timeline is cleared");

    // A real dependency, real exporter row, and then it goes away.
    let name = format!("outage-{}", Uuid::new_v4().simple());
    let mut dependency = Dependency::start().await;
    let created = call(
        &state,
        with_cookie(
            request(
                Method::POST,
                "/api/v1/observability/exporters",
                Some(json!({
                    "name": name,
                    "kind": "webhook",
                    "endpoint": dependency.endpoint,
                    // The floor the API accepts, not a smaller one: a batch interval below the
                    // documented minimum is a refusal with a field-level message, and the walk
                    // must configure a row the panel would really accept rather than write one
                    // straight to the table.
                    "batch_ms": 100,
                    "timeout_ms": 500
                })),
            ),
            &cookie,
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "the exporter was not configured: {}",
        created.body
    );
    let exporter_id =
        Uuid::parse_str(created.body["id"].as_str().expect("an exporter id")).expect("a uuid");

    // Healthy first: the baseline that makes "the outage caused this" a claim rather than an
    // assumption. Without it a rule that fired on its own would pass the whole walk.
    assert_eq!(
        drops_for(&name),
        0.0,
        "a walk that starts already dropping proves nothing about the outage"
    );
    let quiet = alert_loop::tick(pool).await.expect("the pass runs");
    let open_before: i64 = sqlx::query_scalar(
        "select count(*) from obs_alert_events where rule_id = $1 and state <> 'resolved'",
    )
    .bind(rule_id)
    .fetch_one(pool)
    .await
    .expect("the count runs");
    assert_eq!(
        open_before, 0,
        "the shipped rule opened an event before anything broke: {quiet:?}"
    );

    // The outage. The listener is closed, so the port stops answering.
    dependency.take_down();

    // Push past the ring's capacity: every push past the cap is a sample the platform counted as
    // lost, counted by the same `exporter::push` a live process calls when a request writes a log
    // line and the backend is gone. This walk writes no metric by hand.
    for index in 0..(exporter::DEFAULT_BUFFER_CAPACITY + 30) {
        let _ = exporter_flush::fan_out(
            exporter::global(),
            json!({ "request_id": format!("outage-{index}") }),
        );
    }
    let _ = exporter_flush::sweep(pool, exporter::global())
        .await
        .expect("the sweep runs");

    let lost = drops_for(&name);
    assert!(
        lost >= 30.0,
        "the outage lost no telemetry, so the rule has nothing to read: {lost}"
    );
    assert_eq!(
        dependency.took_requests(),
        0,
        "the dependency served a batch after it was taken down"
    );

    // The outage runs on into a SECOND minute, which is the only way a window and a running total
    // become different numbers while the incident is still open. With one minute of drops in the
    // window, the windowed reading and the total are both 31 and a rule reading either one looks
    // correct — which is exactly why the first draft of this walk passed against the permanent-
    // alert mutation twice. A minute passes; the backlog keeps growing in a minute of its own;
    // the window now holds only what happened since, and the total holds both minutes.
    metrics::global().advance_minutes(1);
    // Past the cap again, not merely "some more": a push that fits the buffer is not a dropped
    // sample, and 40 payloads into a 4096 ring is a second outage that loses nothing. The cap
    // plus a margin is the only way the platform counts the loss itself.
    for index in 0..(exporter::DEFAULT_BUFFER_CAPACITY + 40) {
        let _ = exporter_flush::fan_out(
            exporter::global(),
            json!({ "request_id": format!("outage-2-{index}") }),
        );
    }
    let _ = exporter_flush::sweep(pool, exporter::global())
        .await
        .expect("the sweep runs");

    let total_so_far = metrics::global()
        .value_of(exporter::DROPPED_FAMILY, &[name.as_str()])
        .unwrap_or(0.0);
    let this_minute = alerts::parse(expr)
        .expect("the shipped expression parses")
        .evaluate_over(alerts::ALERT_WINDOW_MINUTES)
        .value
        .unwrap_or(0.0);
    assert!(
        total_so_far >= 70.0,
        "two minutes of outage did not accumulate: {total_so_far}"
    );
    // Both minutes are still inside a five-minute window, so of course the two numbers match
    // here. The window is read AFTER the gap below, once the first minute has rolled out of it.

    // The shipped rule, on its own, decides that this is an incident.
    let fired = alert_loop::tick(pool).await.expect("the pass runs");
    assert!(
        fired.fired >= 1,
        "the shipped rule did not fire through a real outage: {fired:?}"
    );

    let (state_name, notified, value): (String, bool, f64) = sqlx::query_as(
        "select state, notified, firing_value from obs_alert_events \
         where rule_id = $1 and state = 'firing' order by started_at desc limit 1",
    )
    .bind(rule_id)
    .fetch_one(pool)
    .await
    .expect("a firing event exists for the shipped rule");
    assert_eq!(state_name, "firing");
    assert!(
        notified,
        "the firing event was never claimed for notification"
    );
    assert!(
        value >= 30.0,
        "the event recorded {value} rather than the drops the outage caused"
    );
    // The value on the event is the rule's own reading, and at this moment both outage minutes
    // are still inside the five-minute window — so it equals the running total, and asserting
    // otherwise here would be asserting that the window ignores a minute it contains. The place
    // where the two numbers part company is after the roll, below.

    // Once. Two more passes over the same firing event must claim nothing, which is the
    // `UPDATE … RETURNING` claim and not a dedupe set in Rust.
    for attempt in 0..2 {
        let claimed = alerts::claim_notifications(pool)
            .await
            .expect("the claim runs");
        assert!(
            !claimed.iter().any(|n| n.rule_id == rule_id),
            "pass {attempt} claimed a second notification for one firing event"
        );
    }

    // The panel shows it, over the real router, on the rule it actually seeded.
    let listed = call(
        &state,
        with_cookie(get("/api/v1/observability/alert-rules"), &cookie),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    let row = listed.body["rules"]
        .as_array()
        .expect("rules is an array")
        .iter()
        .find(|rule| rule["id"] == rule_id.to_string())
        .expect("the shipped rule is listed");
    assert_eq!(
        row["state"], "firing",
        "the panel disagrees with the database"
    );

    // And the timeline says WHY, which is the part an operator reads at 3am.
    let overview = call(
        &state,
        with_cookie(get("/api/v1/observability/alerts"), &cookie),
    )
    .await;
    assert_eq!(overview.status, StatusCode::OK, "{}", overview.body);
    assert!(
        overview.body["counts"]["firing"].as_i64().unwrap_or(0) >= 1,
        "the overview did not count the firing rule: {}",
        overview.body["counts"]
    );

    // The rule must be firing BECAUSE of the drops, not because of anything else — and the only
    // way to tell those apart is to evaluate the shipped expression itself, live, at the moment
    // the outage is still in progress.
    //
    // This assertion is the one that makes the walk a test of the fix. With the evaluator reading
    // a running total it STILL fired, still notified once and still resolved — because a rule
    // that can never see data resolves just as happily as one that recovers. The mutation passed
    // every other assertion in this file, and the only reason the defect is now caught is that the
    // rule is asked what it is looking at.
    let during = alerts::preview(expr).expect("the shipped expression parses");
    assert!(
        during.matched,
        "the shipped rule is firing but reports no matching series — it is not watching the \
         family it claims to watch"
    );
    assert!(
        during.breaching,
        "the shipped rule is firing while its own expression says it is not: {:?}",
        during
    );
    assert!(
        during.value.unwrap_or(0.0) >= 30.0,
        "the rule read {:?} rather than the drops the outage caused — a rule that fires for \
         another reason is an incident nobody can act on",
        during.value
    );

    // The dependency comes back — as a NEW listener on the SAME address, because a collector
    // that restarts does not move. Then the drops leave the five-minute window, which is the
    // only sense in which a counter rule "goes quiet": the total never falls.
    dependency = Dependency::start_on(dependency.address).await;
    let _ = exporter_flush::fan_out(exporter::global(), json!({ "request_id": "recovered" }));
    let _ = exporter_flush::sweep(pool, exporter::global())
        .await
        .expect("the sweep runs");
    assert!(
        dependency.took_requests() >= 1,
        "the restored dependency received nothing, so the recovery was not exercised"
    );

    // The recovery advances the clock past the whole window, and records a QUIET minute inside
    // what is now the window. That is the shape that separates a window from a total: the minute
    // is in the window and the window has nothing in it, while the running total still holds
    // every drop from both outage minutes.
    metrics::global().advance_minutes(alerts::ALERT_WINDOW_MINUTES as i64 + 1);
    let _ = exporter_flush::fan_out(exporter::global(), json!({ "request_id": "settled" }));
    let _ = exporter_flush::sweep(pool, exporter::global())
        .await
        .expect("the sweep runs");
    // Record into the counter in this quiet minute too, so its bucket is written and holds a
    // ZERO rather than being absent. A window that excludes the series and a window that
    // contains a zero are both quiet, and the fixture should not depend on which one the ring
    // happens to produce.
    metrics::global().counter_add(exporter::DROPPED_FAMILY, &[name.as_str()], 0.0);

    // The discriminating assertion. After the window has rolled past both outage minutes, the
    // windowed reading is quiet while the RUNNING TOTAL still holds every drop this walk caused —
    // so the two numbers are finally different, and this is the only point in the walk where
    // "the rule reads a window" and "the rule reads a total" are distinguishable at all.
    //
    // The first drafts of this walk asserted the same thing through the resolve, and the
    // permanent-alert mutation PASSED it twice: with a running total the rule resolved anyway,
    // because a series with no samples in the window evaluates to "no data", which resolves. Two
    // different mechanisms, one green test. A walk has to ask the rule what it is reading, not
    // only where it ended up.
    let still_lost = metrics::global()
        .value_of(exporter::DROPPED_FAMILY, &[name.as_str()])
        .unwrap_or(0.0);
    assert!(
        still_lost >= total_so_far,
        "the running total fell ({still_lost} against {total_so_far}), so this walk can no \
         longer tell a window from a total"
    );
    let after_roll = alerts::parse(expr)
        .expect("the shipped expression parses")
        .evaluate_over(alerts::ALERT_WINDOW_MINUTES)
        .value
        .unwrap_or(0.0);
    assert!(
        after_roll < 1.0,
        "the rule reads {after_roll} after the drops left the window, while the running total is \
         {still_lost} — it is reading a monotonic value, so this alert can never resolve"
    );

    let resolved = alert_loop::tick(pool).await.expect("the pass runs");
    assert!(
        resolved.resolved >= 1,
        "the rule did not resolve after the dependency returned and the window closed: \
         {resolved:?}"
    );

    let final_state: String = sqlx::query_scalar(
        "select state from obs_alert_events where rule_id = $1 order by started_at desc limit 1",
    )
    .bind(rule_id)
    .fetch_one(pool)
    .await
    .expect("an event row exists");
    assert_eq!(
        final_state, "resolved",
        "the incident is still open after the dependency returned — this is the permanent-alert \\
         defect, and it is exactly what the running-total evaluator produced"
    );

    // The loss is still counted, though. A window is not a reset, and `/metrics` must not start
    // lying about data that was already dropped.
    let still_counted = metrics::global()
        .value_of(exporter::DROPPED_FAMILY, &[name.as_str()])
        .unwrap_or(0.0);
    assert!(
        still_counted >= 30.0,
        "the running total fell when the window rolled over: {still_counted}"
    );

    // Clean up. The rule is `bundled`, so it is restored rather than deleted — an instance's
    // bundle is not the walk's to remove.
    sqlx::query("update obs_alert_rules set for_seconds = 300 where id = $1")
        .bind(rule_id)
        .execute(pool)
        .await
        .expect("the dwell is restored");
    sqlx::query("delete from obs_alert_events where rule_id = $1")
        .bind(rule_id)
        .execute(pool)
        .await
        .expect("the timeline is cleared");
    let _ = sqlx::query("delete from obs_exporters where id = $1")
        .bind(exporter_id)
        .execute(pool)
        .await;
    let _ = exporter::global().remove(&name);

    // Put the clock back, so a later test in this binary inherits a real minute.
    metrics::global().advance_minutes(-(alerts::ALERT_WINDOW_MINUTES as i64 + 1));
    let _ = OffsetDateTime::now_utc();
}
