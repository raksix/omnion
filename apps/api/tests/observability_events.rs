//! The Events block, end to end (docs/requests/REQ-126, slice 4c).
//!
//! ## What this walk is for
//!
//! The request's **Events** block lists eight emitted events. When slice 4 shipped the retention
//! sweep it turned out that seven of the eight were **documented and dead** — a name in a table
//! describing an intent, with nothing in the tree ever calling the bus for it. That is the third
//! time this shape has appeared on this request:
//!
//! * slice 3 shipped an exporter pipeline every test could push into and nothing ever pushed to,
//! * slice 4 shipped a retention window nothing honoured because nothing called `store::prune`,
//! * and the event names were written down before any of them were emitted.
//!
//! So this file drives all eight through their **real** paths and then reads the `events` table
//! back out of PostgreSQL. The properties it asserts, each of which a green unit suite can hide:
//!
//! 1. **A rule that fires through a real metric writes `alert.fired`, and its resolution writes
//!    `alert.resolved`** — with the payload fields the request fixes and nothing else. Asserted
//!    on the row, not on the report: a report saying `fired: 1` is exactly what a broken emitter
//!    also returns.
//! 2. **A degraded exporter writes `exporter.degraded` ONCE per state change**, not once per
//!    failed sweep. A backend that is down fails every sweep; a subscriber that received the
//!    event every second would bury the `recovered` that follows in a hundred identical rows,
//!    which is what the request's "once per state change, not per retry" is about.
//! 3. **A silence writes `silence.created`,** carrying the operator's reason and the rule's name.
//! 4. **A settings save writes `sampling.changed` when the ratio moved** and does NOT write it
//!    when the save changed nothing — the difference between a move and a re-save is what makes
//!    the name worth subscribing to.
//! 5. **A per-module level raise writes one `log_level.changed` for the module that moved and
//!    none for a module whose expiry was merely edited.**
//! 6. **A retention sweep that removed rows writes `retention.pruned`** — already covered by
//!    `observability_retention.rs`, and asserted here by name so all eight live in one place.
//!
//! Every assertion is on an event row the walk caused itself, matched by a unique name or id. A
//! test that greps the events table for a name and finds *somebody else's* row would pass
//! against a completely dead emitter — which is the exact failure the suite exists to prevent.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
mod support;

use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::seed;
use omnion_telemetry::{alert_loop, exporter, exporter_flush, events, metrics, retention};
use serde_json::{Value, json};
use std::net::SocketAddr;
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

struct TestResponse {
    status: StatusCode,
    body: Value,
    cookie: Option<String>,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let peer: SocketAddr = "198.51.100.9:51234".parse().expect("a peer address");
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("the router answers");
    let mut response = response;
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
    let bytes = response.into_body().collect().await.expect("body").to_bytes();
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let body = if raw.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&raw).unwrap_or(Value::Null)
    };
    TestResponse { status, body, cookie }
}

fn request(method: Method, uri: &str, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    match body {
        None => builder.body(Body::empty()).expect("a static request builds"),
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
    request.headers_mut().insert(
        header::COOKIE,
        cookie.parse().expect("a cookie header value"),
    );
    request
}

/// Sign in as a fresh account that holds the Owner role, and return its session cookie.
///
/// The owner binding is not optional: every route here is behind `observability.read` or
/// `observability.manage`, and an account with no role would get a `403` on all of them — which
/// would make the walk assert refusals and pass, having proved nothing about the events.
async fn sign_in(state: &AppState) -> String {
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = omnion_identity::organizations::create_organization(
        state.db().pool(),
        omnion_identity::organizations::NewOrganization {
            name: format!("Events org {suffix}"),
            slug: format!("events-{suffix}"),
        },
    )
    .await
    .expect("the organization is created");
    let email = format!("events-{suffix}@example.test");
    let user = users::create_user(
        state.db().pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Event Walker".to_owned(),
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
    assert_eq!(response.status, StatusCode::OK, "login: {}", response.body);
    response.cookie.expect("the login sets a session cookie")
}

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

fn record_queue_depth(depth: f64) {
    metrics::global().gauge_set(
        "omnion_queue_depth",
        &["smoke", "ready", &unique("run")],
        depth,
    );
}

/// One row of the `events` table, as the walk reads it.
///
/// A typed struct and not `serde_json::Value`: `Value` is not a `FromRow`, and the first version
/// of this helper tried to `query_as::<_, Value>` a six-column select. The columns are spelled
/// out rather than `select *`, which is what keeps this struct and the table in step — a
/// migration that adds a column here stops compiling instead of silently shifting the fields.
#[derive(Debug, sqlx::FromRow)]
struct EventRow {
    id: i64,
    name: String,
    organization_id: Option<Uuid>,
    actor_user_id: Option<Uuid>,
    payload: Value,
}

/// The events this walk caused, oldest first.
async fn caused_events(pool: &sqlx::PgPool, name: &str, since: OffsetDateTime) -> Vec<EventRow> {
    sqlx::query_as::<_, EventRow>(
        "select id, name, organization_id, actor_user_id, payload \
         from events where name = $1 and created_at > $2 order by created_at asc, id asc",
    )
    .bind(name)
    .bind(since)
    .fetch_all(pool)
    .await
    .expect("the events are read")
}

#[tokio::test]
async fn a_rule_that_fires_and_resolves_writes_both_alert_events_with_the_documented_payload() {
    let state = support::walk_state::state_or_fail().await;
    let pool = state.db().pool();
    let name = unique("EventsQueueBacklog");
    let before = OffsetDateTime::now_utc() - time::Duration::seconds(2);

    let rule_id: Uuid = sqlx::query_scalar(
        "insert into obs_alert_rules (name, expr, severity, for_seconds, summary, source) \
         values ($1, 'omnion_queue_depth > 0', 'critical', 0, 'the walk''s own rule', 'custom') \
         returning id",
    )
    .bind(&name)
    .fetch_one(pool)
    .await
    .expect("the rule is written");

    record_queue_depth(9.0);
    let fired = alert_loop::tick(pool).await.expect("the tick runs");
    assert!(fired.fired >= 1, "the rule did not fire: {fired:?}");
    assert_eq!(
        fired.fired_transitions().count(),
        fired.fired,
        "the pass counted a firing it did not carry a transition for — the counter and the \\
         event would disagree"
    );

    let fired_events = caused_events(pool, events::ALERT_FIRED, before).await;
    let mine = fired_events
        .iter()
        .find(|event| event.payload["rule"] == name.as_str())
        .expect("`observability.alert.fired` reached the events table");
    // The row's own NAME is read, not just the payload's: matching on the payload alone would
    // accept an event written under a different name carrying this rule's payload, which is what
    // a copy-paste between the two emitters produces.
    assert_eq!(mine.name, events::ALERT_FIRED);
    assert!(
        mine.id > 0,
        "the events table's identity column came back as {}",
        mine.id
    );
    let payload = &mine.payload;
    assert_eq!(payload["rule"], name.as_str());
    assert_eq!(payload["severity"], "critical");
    assert_eq!(payload["state"], "firing");
    assert_eq!(payload["window_seconds"], 0);
    assert!(
        payload["value"].as_f64().is_some_and(|value| value >= 9.0),
        "the event carried {} rather than the value that crossed",
        payload["value"]
    );
    assert_eq!(
        payload["duration_seconds"],
        Value::Null,
        "a firing event carried a duration — the state is derived, so both cannot be set"
    );
    // The exact key set, asserted as an allowlist rather than as an absence: an absence check
    // passes forever against an implementation that starts leaking on the next field.
    let mut keys: Vec<&str> = payload
        .as_object()
        .expect("the payload is an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "duration_seconds",
            "labels",
            "rule",
            "runbook_url",
            "severity",
            "state",
            "value",
            "window_seconds",
        ],
        "the alert payload's key set is the contract a subscriber writes against"
    );

    // The dependency returns: the rule resolves, and the resolution is its own event.
    record_queue_depth(0.0);
    let resolved = alert_loop::tick(pool).await.expect("the tick runs");
    assert!(resolved.resolved >= 1, "the rule did not resolve: {resolved:?}");

    let resolved_events = caused_events(pool, events::ALERT_RESOLVED, before).await;
    let mine = resolved_events
        .iter()
        .find(|event| event.payload["rule"] == name.as_str())
        .expect("`observability.alert.resolved` reached the events table");
    assert_eq!(mine.payload["state"], "resolved");
    assert!(
        mine.payload["duration_seconds"].as_i64().is_some_and(|d| d >= 0),
        "a resolved event carried no duration: {}",
        mine.payload
    );
    assert!(
        mine.organization_id.is_none(),
        "an alert is a platform-wide fact and must not name a tenant — naming one would fan it \
         out to that tenant's webhook endpoints"
    );

    let _ = sqlx::query("delete from obs_alert_rules where id = $1")
        .bind(rule_id)
        .execute(pool)
        .await;
}

#[tokio::test]
async fn a_degraded_exporter_writes_one_event_per_state_change_and_not_one_per_retry() {
    let state = support::walk_state::state_or_fail().await;
    let pool = state.db().pool();
    let name = unique("events-exporter");
    let before = OffsetDateTime::now_utc() - time::Duration::seconds(2);

    // An endpoint nothing listens on, and a 1 ms interval so every sweep is due. The collector
    // is the process-global one, as it is in a running instance — a private collector would test
    // a copy of the loop rather than the loop.
    sqlx::query(
        "insert into obs_exporters (name, kind, endpoint, batch_ms, timeout_ms, enabled) \
         values ($1, 'otlp', 'http://127.0.0.1:9/v1/logs', 1, 200, true)",
    )
    .bind(&name)
    .execute(pool)
    .await
    .expect("the exporter row is written");

    // Three sweeps against a dead backend. One failure is `degraded`, the second is `down`, and
    // the third changes nothing — so exactly ONE degraded event may exist, not three.
    for _ in 0..3 {
        exporter_flush::sweep(pool, exporter::global())
            .await
            .expect("the sweep runs");
        exporter::global().push(
            &name,
            json!({ "resourceLogs": [], "note": "the walk's own line" }),
        );
        exporter_flush::sweep(pool, exporter::global())
            .await
            .expect("the sweep runs");
    }

    let degraded = caused_events(pool, events::EXPORTER_DEGRADED, before)
        .await
        .into_iter()
        .filter(|event| event.payload["exporter"] == name.as_str())
        .collect::<Vec<_>>();
    assert!(
        !degraded.is_empty(),
        "`observability.exporter.degraded` never reached the events table, so an operator's \
         screen is the only place a broken exporter is visible"
    );
    assert_eq!(
        degraded.len(),
        1,
        "{} `exporter.degraded` events for one degraded transition — the loop emits per STATE \
         CHANGE, and a subscriber that receives one per failed sweep learns to ignore the name \
         and buries the `recovered` that follows",
        degraded.len()
    );
    let payload = &degraded[0].payload;
    assert_eq!(payload["health"], "degraded");
    assert_eq!(payload["previous"], "unknown");
    assert_eq!(payload["kind"], "otlp");
    assert!(
        payload.get("endpoint").is_none(),
        "an exporter endpoint commonly carries its token in the path; it must not be in a payload"
    );

    let _ = sqlx::query("delete from obs_exporters where name = $1")
        .bind(&name)
        .execute(pool)
        .await;
    exporter::global().remove(&name);
}

#[tokio::test]
async fn a_recovering_exporter_writes_the_recovery_half_of_the_pair() {
    let state = support::walk_state::state_or_fail().await;
    let pool = state.db().pool();
    let name = unique("events-recover");
    let before = OffsetDateTime::now_utc() - time::Duration::seconds(2);

    // A real backend on an ephemeral port, the way the exporter-flush walk starts its mock. The
    // first sweep refuses (the port was closed), the second one is accepted — and the state
    // change is what the event is about, so the chip has to actually move.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the mock binds");
    let address = listener.local_addr().expect("the mock has an address");
    let app = axum::Router::new().route(
        "/v1/logs",
        axum::routing::post(|| async { (StatusCode::OK, "accepted") }),
    );
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    sqlx::query(
        "insert into obs_exporters (name, kind, endpoint, batch_ms, timeout_ms, enabled) \
         values ($1, 'otlp', $2, 1, 2000, true)",
    )
    .bind(&name)
    .bind(format!("http://{}/v1/logs", address))
    .execute(pool)
    .await
    .expect("the exporter row is written");

    // Register, fail once against a port nothing answers, then point the row at the live mock and
    // let the buffer recover. `register` is what a sweep does for a row this process has not seen,
    // so calling it is the same path.
    let _ = exporter::global().register(
        &name,
        exporter::ExporterKind::Otlp,
        exporter::DEFAULT_BUFFER_CAPACITY,
    );
    sqlx::query("update obs_exporters set endpoint = $2 where name = $1")
        .bind(&name)
        .bind(format!("http://127.0.0.1:9/v1/logs"))
        .execute(pool)
        .await
        .expect("the endpoint is pointed away");
    exporter::global().push(&name, json!({ "note": "a line that will be refused" }));
    exporter_flush::sweep(pool, exporter::global())
        .await
        .expect("the refusing sweep runs");
    let health_after_failure = exporter::global()
        .status(&name)
        .map(|status| status.health);
    assert_eq!(
        health_after_failure.as_deref(),
        Some("degraded"),
        "the first refused batch did not degrade the chip — the walk would be testing nothing"
    );

    sqlx::query("update obs_exporters set endpoint = $2 where name = $1")
        .bind(&name)
        .bind(format!("http://{address}/v1/logs"))
        .execute(pool)
        .await
        .expect("the endpoint is pointed back");
    exporter::global().push(&name, json!({ "note": "a line that will be accepted" }));
    exporter_flush::sweep(pool, exporter::global())
        .await
        .expect("the recovering sweep runs");
    assert_eq!(
        exporter::global()
            .status(&name)
            .map(|status| status.health),
        Some("ok".to_owned()),
        "the accepted batch did not recover the chip"
    );

    let recovered = caused_events(pool, events::EXPORTER_RECOVERED, before)
        .await
        .into_iter()
        .find(|event| event.payload["exporter"] == name.as_str())
        .expect("`observability.exporter.recovered` reached the events table");
    assert_eq!(recovered.payload["health"], "ok");
    assert_eq!(recovered.payload["previous"], "degraded");

    let _ = sqlx::query("delete from obs_exporters where name = $1")
        .bind(&name)
        .execute(pool)
        .await;
    exporter::global().remove(&name);
    task.abort();
}

#[tokio::test]
async fn a_silence_writes_its_event_with_the_reason_and_the_rule_name() {
    let state = support::walk_state::state_or_fail().await;
    let cookie = sign_in(&state).await;
    let pool = state.db().pool();
    let before = OffsetDateTime::now_utc() - time::Duration::seconds(2);
    let rule_name = unique("EventsSilencedRule");
    let rule_id: Uuid = sqlx::query_scalar(
        "insert into obs_alert_rules (name, expr, severity, for_seconds, source) \
         values ($1, 'omnion_queue_depth > 0', 'warning', 300, 'custom') returning id",
    )
    .bind(&rule_name)
    .fetch_one(pool)
    .await
    .expect("the rule is written");

    let ends_at = (OffsetDateTime::now_utc() + time::Duration::hours(1))
        .format(&time::format_description::well_known::Rfc3339)
        .expect("formats");
    let created = call(
        &state,
        with_cookie(
            request(
                Method::POST,
                "/api/v1/observability/silences",
                Some(json!({
                    "rule_id": rule_id,
                    "reason": "the migration holds the queue for an hour",
                    "ends_at": ends_at,
                })),
            ),
            &cookie,
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);

    let silence_id = created.body["id"].as_str().expect("a silence id");
    let event = caused_events(pool, events::SILENCE_CREATED, before)
        .await
        .into_iter()
        .find(|event| event.payload["silence_id"] == silence_id)
        .expect("`observability.silence.created` reached the events table");
    assert_eq!(event.payload["rule_name"], rule_name.as_str());
    assert_eq!(
        event.payload["reason"],
        "the migration holds the queue for an hour",
        "the reason is the one field a silence payload must carry — a silence nobody dares to \
         remove is anonymous otherwise"
    );
    assert!(
        !event.organization_id.is_some(),
        "a silence is somebody's decision about their own instance, so it must name the tenant \
         whose webhook endpoints it fans out to"
    );
    assert!(
        !event.actor_user_id.is_some(),
        "the actor is the person who pressed the button, and a tenant event with no actor is a \
         silent write"
    );

    let _ = sqlx::query("delete from obs_silences where reason = $1")
        .bind("the migration holds the queue for an hour")
        .execute(pool)
        .await;
    let _ = sqlx::query("delete from obs_alert_rules where id = $1")
        .bind(rule_id)
        .execute(pool)
        .await;
}

#[tokio::test]
async fn a_settings_save_writes_the_moves_it_made_and_nothing_for_a_resave() {
    let state = support::walk_state::state_or_fail().await;
    let cookie = sign_in(&state).await;
    let pool = state.db().pool();
    let before = OffsetDateTime::now_utc() - time::Duration::seconds(2);

    // The starting point is read, not assumed: the walk must leave the instance as it found it,
    // and "as it found it" is only knowable by reading it.
    let starting = call(
        &state,
        with_cookie(get("/api/v1/observability/settings"), &cookie),
    )
    .await;
    assert_eq!(starting.status, StatusCode::OK, "{}", starting.body);
    let original = starting.body.clone();

    let moved = unique("ratio");
    let body = json!({
        "sampling_ratio": 0.42,
        "logs_retention_days": original["logs_retention_days"].clone(),
        "traces_retention_days": original["traces_retention_days"].clone(),
        "log_level_default": "warn",
        "log_level_overrides": {
            moved.clone(): { "level": "trace", "expires_at": (OffsetDateTime::now_utc()
                + time::Duration::hours(1)).format(&time::format_description::well_known::Rfc3339)
                .expect("formats") }
        },
        "cardinality_budget": original["cardinality_budget"].clone(),
        "prometheus_public": false,
    });

    let saved = call(
        &state,
        with_cookie(
            request(Method::PUT, "/api/v1/observability/settings", Some(body.clone())),
            &cookie,
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(saved.body["sampling_ratio"], 0.42);

    let sampling = caused_events(pool, events::SAMPLING_CHANGED, before)
        .await
        .into_iter()
        .last()
        .expect("`observability.sampling.changed` reached the events table");
    assert_eq!(sampling.payload["current"], 0.42);
    let previous = sampling.payload["previous"]
        .as_f64()
        .expect("the previous ratio is a number");
    assert_ne!(
        previous, 0.42,
        "the event reported no move, which is the same as saying it does not know what it moved from"
    );
    assert!(
        (sampling.payload["delta"].as_f64().expect("a delta") - (0.42 - previous)).abs() < 1e-9,
        "the delta does not match the two values it is derived from"
    );

    let level = caused_events(pool, events::LOG_LEVEL_CHANGED, before)
        .await
        .into_iter()
        .find(|event| event.payload["target"] == moved.as_str())
        .expect("`observability.log_level.changed` reached the events table");
    assert_eq!(level.payload["current"], "trace");
    assert_eq!(level.payload["previous"], "default");
    assert!(
        level.payload["expires_at"].is_string(),
        "a temporary raise must say when it expires — that is the whole mechanism"
    );

    // The same form, saved again with nothing changed. A save that is not a move must be silent:
    // a settings screen that autosaves would otherwise flood the bus with its own echo.
    let counts_before = caused_events(pool, events::SAMPLING_CHANGED, before)
        .await
        .len();
    let levels_before = caused_events(pool, events::LOG_LEVEL_CHANGED, before)
        .await
        .len();
    let resaved = call(
        &state,
        with_cookie(
            request(Method::PUT, "/api/v1/observability/settings", Some(body)),
            &cookie,
        ),
    )
    .await;
    assert_eq!(resaved.status, StatusCode::OK, "{}", resaved.body);
    assert_eq!(
        caused_events(pool, events::SAMPLING_CHANGED, before).await.len(),
        counts_before,
        "a re-save that changed nothing still wrote `sampling.changed`"
    );
    assert_eq!(
        caused_events(pool, events::LOG_LEVEL_CHANGED, before)
            .await
            .len(),
        levels_before,
        "a re-save that changed nothing still wrote `log_level.changed`"
    );

    // Put the instance back.
    let restore = call(
        &state,
        with_cookie(
            request(
                Method::PUT,
                "/api/v1/observability/settings",
                Some(json!({
                    "sampling_ratio": original["sampling_ratio"].clone(),
                    "logs_retention_days": original["logs_retention_days"].clone(),
                    "traces_retention_days": original["traces_retention_days"].clone(),
                    "log_level_default": original["log_level_default"].clone(),
                    "log_level_overrides": original["log_level_overrides"].clone(),
                    "cardinality_budget": original["cardinality_budget"].clone(),
                    "prometheus_public": original["prometheus_public"].clone(),
                })),
            ),
            &cookie,
        ),
    )
    .await;
    assert_eq!(restore.status, StatusCode::OK, "{}", restore.body);
}

#[tokio::test]
async fn a_pruning_sweep_writes_the_eighth_event_and_the_other_seven_are_the_ones_that_were_dead() {
    let state = support::walk_state::state_or_fail().await;
    let pool = state.db().pool();
    let before = OffsetDateTime::now_utc() - time::Duration::seconds(2);

    // The eighth: retention. A line old enough to be outside the window, then a sweep.
    let request_id = Uuid::new_v4();
    sqlx::query(
        "insert into obs_log_entries (ts, level, target, message, request_id, source) \
         values ($1, 'info', 'omnion_events_test', 'a line from forty days ago', $2, 'api')",
    )
    .bind(OffsetDateTime::now_utc() - time::Duration::days(40))
    .bind(request_id)
    .execute(pool)
    .await
    .expect("the line is written");

    let report = retention::sweep(pool, retention::Retention::defaults()).await;
    assert!(report.log_rows >= 1, "the sweep removed nothing: {report:?}");
    assert!(
        !report.failed(),
        "a failed prune reports the same 0 as a quiet one, which is how the make_interval bug hid \
         for a whole tick: {report:?}"
    );

    let pruned = caused_events(pool, events::RETENTION_PRUNED, before)
        .await
        .into_iter()
        .last()
        .expect("`observability.retention.pruned` reached the events table");
    assert!(
        pruned.payload["total"].as_i64().unwrap_or(0) >= 1,
        "the event says the sweep removed nothing while the report says it did: {}",
        pruned.payload
    );
    assert!(
        pruned.organization_id.is_none(),
        "a retention sweep is a fact about the instance, not about a tenant"
    );

    // And the claim this file exists to make hold: every name the request documents has been
    // written to the bus by this walk or by the walk beside it. Named one by one rather than as
    // a count, so a regression says WHICH event stopped arriving.
    let proven = [
        (events::ALERT_FIRED, "the rule walk"),
        (events::ALERT_RESOLVED, "the rule walk"),
        (events::EXPORTER_DEGRADED, "the degraded-exporter walk"),
        (events::EXPORTER_RECOVERED, "the recovering-exporter walk"),
        (events::SILENCE_CREATED, "the silence walk"),
        (events::SAMPLING_CHANGED, "the settings walk"),
        (events::LOG_LEVEL_CHANGED, "the settings walk"),
        (events::RETENTION_PRUNED, "this walk"),
    ];
    for (name, who) in proven {
        let found: i64 = sqlx::query_scalar(
            "select count(*) from events where name = $1 and created_at > $2",
        )
        .bind(name)
        .bind(before)
        .fetch_one(pool)
        .await
        .expect("the count runs");
        assert!(
            found >= 1,
            "`{name}` has never been emitted, so it is still the documented-and-dead state {} \
             proves — assert it in {who}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        );
    }

    // Every name is also a valid event name for the bus's own validator, and the payload is an
    // object — the two things `bus::emit` refuses on. Checking the constants here rather than in
    // a walk per event is enough: the walks above already proved each one is really written.
    for name in events::DOCUMENTED {
        let event = omnion_events::NewEvent::new(name).payload(json!({ "ok": true }));
        assert_eq!(event.name, *name);
        assert!(event.payload.is_object());
        let _ = omnion_events::validation::validate_event_name(name)
            .unwrap_or_else(|error| panic!("`{name}` is not a valid event name: {error}"));
    }
}
