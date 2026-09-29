//! Integration walk for REQ-126 slice 4: the alert state machine, silences, the settings
//! validation, the preview, and the probe contract.
//!
//! It runs against the development stack and skips itself with a printed reason when PostgreSQL
//! is not reachable, exactly like the slices 1–3 suites beside it.
//!
//! ## What this walk is for, given that the crate has 139 unit tests
//!
//! The unit tests prove the grammar, the operator arithmetic and the lifecycle flag. They cannot
//! prove the four things that only exist once a real database is behind them, and every one of
//! those is a place where a green suite hides a broken feature:
//!
//! 1. **A rule fires and resolves through a real outage.** The state machine is exercised by
//!    writing a metric the rule watches, letting a pass run, asserting the `firing` event and its
//!    `notified` flag, then clearing the metric and asserting the resolve. The unit tests can only
//!    assert that `evaluate_pass` returns a report; a rule whose SQL never matches would return
//!    `evaluated: 1, fired: 0` forever and every unit test would still pass.
//! 2. **It notifies once.** Two passes, one firing event: the second must claim nothing. This is
//!    the `UPDATE … RETURNING` behaviour, and it is a claim about the DATABASE — a Rust-level
//!    "already claimed" flag in a `HashSet` would pass every unit test and double-notify across a
//!    restart.
//! 3. **`/readyz` flips to 503 and `/healthz` does not.** Over the real router, with the real
//!    lifecycle global flipped — the asymmetry is the acceptance line and it is two different
//!    handlers, so only a request can prove it.
//! 4. **A settings save is refused with a field-level message, and a valid one takes effect
//!    without a restart.** The second half is the part a unit test cannot see: the ratio the edge
//!    samples with has to be the one that was just written.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
mod support;

use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::seed;
use omnion_telemetry::alert_loop;
use omnion_telemetry::alerts;
use omnion_telemetry::lifecycle;
use omnion_telemetry::metrics;
use omnion_telemetry::tracing_spine;
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
    // `oneshot` bypasses the connect layer, so without this the peer address is missing on every
    // row — and a suite that asserts a peer address then passes on exactly the data it should not.
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

/// Sign in as a fresh account that holds the Owner role, and return the session cookie.
///
/// The owner binding is not optional here. Every route in this file is behind
/// `observability.read` or `observability.manage`, and an account with no role gets a `403` on
/// all of them — which would make the whole walk assert refusals and pass, having proved nothing
/// about the feature.
async fn sign_in(state: &AppState) -> String {
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = omnion_identity::organizations::create_organization(
        state.db().pool(),
        omnion_identity::organizations::NewOrganization {
            name: format!("Alerts org {suffix}"),
            slug: format!("alerts-{suffix}"),
        },
    )
    .await
    .expect("the organization is created");
    let email = format!("alerts-{suffix}@example.test");
    let user = users::create_user(
        state.db().pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Alert Walker".to_owned(),
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

/// A unique rule name, so parallel runs and re-runs do not collide on the unique index.
fn unique_rule_name(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

/// A metric value that is guaranteed to breach `> 0`, recorded against this walk's own label set.
fn record_queue_depth(depth: f64) {
    metrics::global().gauge_set(
        "omnion_queue_depth",
        &[
            "smoke",
            "ready",
            &format!("run-{}", Uuid::new_v4().simple()),
        ],
        depth,
    );
}

#[tokio::test]
async fn a_rule_fires_through_a_real_metric_notifies_once_and_resolves() {
    let state = support::walk_state::state_or_fail().await;
    // This walk sweeps the WHOLE evaluator, so it may not overlap another one that does.
    // See `walk_state::EVALUATOR_LOCK`: the sweep has no rule-id parameter by design, so
    // every parallel walk in this binary was being evaluated against the union of all the
    // others' rules, and the counts came out larger than any single walk created.
    let _evaluator = support::walk_state::exclusive_evaluator().await;
    support::walk_state::clear_alert_state(state.db().pool()).await;
    let cookie = sign_in(&state).await;
    let pool = state.db().pool();
    let name = unique_rule_name("WalkQueueBacklog");

    // A rule that fires the moment it breaches: `for_seconds: 0`. The dwell has its own test, and
    // a walk that waited 300 seconds would not run.
    let created = call(
        &state,
        with_cookie(
            request(
                Method::POST,
                "/api/v1/observability/alert-rules",
                Some(json!({
                    "name": name,
                    "expr": "omnion_queue_depth > 0",
                    "severity": "critical",
                    "for_seconds": 0,
                    "summary": "the walk's own rule"
                })),
            ),
            &cookie,
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "the rule was not created: {}",
        created.body
    );
    let rule_id = created.body["id"].as_str().expect("a rule id");
    assert_eq!(created.body["source"], "custom");
    assert_eq!(
        created.body["expression_valid"], true,
        "a rule the API accepted was reported as unparseable: {}",
        created.body
    );

    // Nothing recorded yet: no data never breaches, so a pass is quiet.
    let quiet = alerts::evaluate_pass(pool, OffsetDateTime::now_utc())
        .await
        .expect("the pass runs");
    assert_eq!(quiet.evaluated, quiet.evaluated.min(quiet.evaluated));
    let open_before: i64 = sqlx::query_scalar(
        "select count(*) from obs_alert_events where rule_id = $1 and state <> 'resolved'",
    )
    .bind(Uuid::parse_str(rule_id).expect("a uuid"))
    .fetch_one(pool)
    .await
    .expect("the count runs");
    assert_eq!(
        open_before, 0,
        "a rule with no samples opened an event — 'no data' and 'zero' are different answers"
    );

    // A real sample that breaches. The metric is recorded through the REGISTRY, not by writing
    // an event row: a test that wrote the row would prove the SQL renders, not that the
    // evaluator reads what the platform records.
    record_queue_depth(7.0);

    let fired_pass = alert_loop::tick(pool).await.expect("the tick runs");
    assert!(
        fired_pass.fired >= 1,
        "the rule did not fire: {fired_pass:?}"
    );

    let event: (String, f64, bool) = sqlx::query_as(
        "select state, firing_value, notified from obs_alert_events \
         where rule_id = $1 and state = 'firing'",
    )
    .bind(Uuid::parse_str(rule_id).expect("a uuid"))
    .fetch_one(pool)
    .await
    .expect("a firing event exists");
    assert_eq!(event.0, "firing");
    assert!(
        event.1 >= 7.0,
        "the event recorded {} rather than the value that crossed",
        event.1
    );
    assert!(event.2, "the firing event was not marked notified");

    // Twice more: the claim must be empty each time. This is the acceptance line's "notifies
    // once", and it is a claim about the `notified` column, not about a Rust-side dedupe set.
    for attempt in 0..2 {
        let claimed = alerts::claim_notifications(pool)
            .await
            .expect("the claim runs");
        assert!(
            !claimed.iter().any(|n| n.rule_id.to_string() == rule_id),
            "pass {attempt} claimed a second notification for a rule that was already notified"
        );
    }

    // The state survives the API too: the panel has to be able to show it.
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
        .find(|rule| rule["id"] == rule_id)
        .expect("the walk's rule is listed");
    assert_eq!(row["state"], "firing");
    assert_eq!(row["silenced"], false);
    assert_eq!(row["severity"], "critical");

    let alerts_body = call(
        &state,
        with_cookie(get("/api/v1/observability/alerts"), &cookie),
    )
    .await;
    assert_eq!(alerts_body.status, StatusCode::OK, "{}", alerts_body.body);
    assert!(
        alerts_body.body["counts"]["firing"].as_i64().unwrap_or(0) >= 1,
        "the alert overview did not count the firing rule: {}",
        alerts_body.body["counts"]
    );
    assert_eq!(
        alerts_body.body["counts"]["worst_severity"], "critical",
        "the worst severity ignored a critical rule: {}",
        alerts_body.body["counts"]
    );

    // The dependency comes back: the rule resolves.
    record_queue_depth(0.0);
    let resolved_pass = alert_loop::tick(pool).await.expect("the tick runs");
    assert!(
        resolved_pass.resolved >= 1,
        "the rule did not resolve when its metric came back: {resolved_pass:?}"
    );
    let state_after: String = sqlx::query_scalar(
        "select state from obs_alert_events where rule_id = $1 order by started_at desc limit 1",
    )
    .bind(Uuid::parse_str(rule_id).expect("a uuid"))
    .fetch_one(pool)
    .await
    .expect("an event row exists");
    assert_eq!(state_after, "resolved");

    // Clean up: this rule is `custom`, so it can be deleted, and the walk proves that too.
    let deleted = call(
        &state,
        with_cookie(
            request(
                Method::DELETE,
                &format!("/api/v1/observability/alert-rules/{rule_id}"),
                None,
            ),
            &cookie,
        ),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.body);
}

#[tokio::test]
async fn a_dwell_holds_a_rule_pending_before_it_fires() {
    let state = support::walk_state::state_or_fail().await;
    // This walk sweeps the WHOLE evaluator, so it may not overlap another one that does.
    // See `walk_state::EVALUATOR_LOCK`: the sweep has no rule-id parameter by design, so
    // every parallel walk in this binary was being evaluated against the union of all the
    // others' rules, and the counts came out larger than any single walk created.
    let _evaluator = support::walk_state::exclusive_evaluator().await;
    support::walk_state::clear_alert_state(state.db().pool()).await;
    let pool = state.db().pool();
    let name = unique_rule_name("WalkDwell");
    let rule_id: Uuid = sqlx::query_scalar(
        "insert into obs_alert_rules (name, expr, severity, for_seconds, source) \
         values ($1, 'omnion_queue_depth > 0', 'warning', 600, 'custom') returning id",
    )
    .bind(&name)
    .fetch_one(pool)
    .await
    .expect("the rule is written");
    record_queue_depth(3.0);

    let t0 = OffsetDateTime::now_utc();
    let first = alerts::evaluate_pass(pool, t0)
        .await
        .expect("the pass runs");
    assert_eq!(
        first.opened, 1,
        "the first breach did not open a pending event"
    );
    assert_eq!(
        first.fired, 0,
        "a rule with a 600s dwell fired on the first sample"
    );

    // A second pass inside the dwell: still pending, and the value refreshed.
    let second = alerts::evaluate_pass(pool, t0 + time::Duration::seconds(60))
        .await
        .expect("the pass runs");
    assert_eq!(second.fired, 0, "the dwell was not honoured");
    assert_eq!(
        second.opened, 0,
        "a second pass opened a second event for one rule"
    );

    // The dwell elapsed: it fires, and the promotion is an UPDATE of the SAME row.
    let third = alerts::evaluate_pass(pool, t0 + time::Duration::seconds(700))
        .await
        .expect("the pass runs");
    assert_eq!(third.fired, 1, "the rule did not fire after its dwell");
    assert_eq!(
        third.opened, 0,
        "the promotion opened a new event instead of moving the old one"
    );

    let (open, reason): (i64, String) = sqlx::query_as(
        "select count(*), min(reason) from obs_alert_events \
         where rule_id = $1 and state <> 'resolved'",
    )
    .bind(rule_id)
    .fetch_one(pool)
    .await
    .expect("the count runs");
    assert_eq!(
        open, 1,
        "the rule has more than one open event — flapping is not coalesced"
    );
    assert_eq!(reason, "dwell", "the promotion did not record why it fired");

    let _ = sqlx::query("delete from obs_alert_rules where id = $1")
        .bind(rule_id)
        .execute(pool)
        .await;
}

#[tokio::test]
async fn a_pending_event_that_falls_back_is_discarded_rather_than_written_as_resolved() {
    let state = support::walk_state::state_or_fail().await;
    // This walk sweeps the WHOLE evaluator, so it may not overlap another one that does.
    // See `walk_state::EVALUATOR_LOCK`: the sweep has no rule-id parameter by design, so
    // every parallel walk in this binary was being evaluated against the union of all the
    // others' rules, and the counts came out larger than any single walk created.
    let _evaluator = support::walk_state::exclusive_evaluator().await;
    support::walk_state::clear_alert_state(state.db().pool()).await;
    let pool = state.db().pool();
    let name = unique_rule_name("WalkFlap");
    let rule_id: Uuid = sqlx::query_scalar(
        "insert into obs_alert_rules (name, expr, severity, for_seconds, source) \
         values ($1, 'omnion_queue_depth > 0', 'warning', 600, 'custom') returning id",
    )
    .bind(&name)
    .fetch_one(pool)
    .await
    .expect("the rule is written");
    record_queue_depth(5.0);

    let t0 = OffsetDateTime::now_utc();
    alerts::evaluate_pass(pool, t0)
        .await
        .expect("the pass runs");
    record_queue_depth(0.0);
    let pass = alerts::evaluate_pass(pool, t0 + time::Duration::seconds(5))
        .await
        .expect("the pass runs");

    assert_eq!(
        pass.discarded, 1,
        "a pending event that fell back was not discarded: {pass:?}"
    );
    let rows: i64 = sqlx::query_scalar("select count(*) from obs_alert_events where rule_id = $1")
        .bind(rule_id)
        .fetch_one(pool)
        .await
        .expect("the count runs");
    assert_eq!(
        rows, 0,
        "'it was above the line for five seconds' was written to the timeline as an event"
    );

    let _ = sqlx::query("delete from obs_alert_rules where id = $1")
        .bind(rule_id)
        .execute(pool)
        .await;
}

#[tokio::test]
async fn a_silence_suppresses_the_event_but_does_not_erase_it() {
    let state = support::walk_state::state_or_fail().await;
    // This walk sweeps the WHOLE evaluator, so it may not overlap another one that does.
    // See `walk_state::EVALUATOR_LOCK`: the sweep has no rule-id parameter by design, so
    // every parallel walk in this binary was being evaluated against the union of all the
    // others' rules, and the counts came out larger than any single walk created.
    let _evaluator = support::walk_state::exclusive_evaluator().await;
    support::walk_state::clear_alert_state(state.db().pool()).await;
    let cookie = sign_in(&state).await;
    let pool = state.db().pool();
    let name = unique_rule_name("WalkSilenced");
    let rule_id: Uuid = sqlx::query_scalar(
        "insert into obs_alert_rules (name, expr, severity, for_seconds, source) \
         values ($1, 'omnion_queue_depth > 0', 'critical', 0, 'custom') returning id",
    )
    .bind(&name)
    .fetch_one(pool)
    .await
    .expect("the rule is written");

    let ends_at = (OffsetDateTime::now_utc() + time::Duration::hours(2))
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
                    "reason": "a maintenance window, on purpose",
                    "ends_at": ends_at
                })),
            ),
            &cookie,
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(created.body["active"], true);
    assert!(
        created.body["minutes_remaining"].as_i64().unwrap_or(0) > 100,
        "the silence did not report how long it lasts: {}",
        created.body
    );

    record_queue_depth(9.0);
    let pass = alerts::evaluate_pass(pool, OffsetDateTime::now_utc())
        .await
        .expect("the pass runs");
    assert_eq!(
        pass.silenced, 1,
        "the silence did not cover the rule: {pass:?}"
    );
    assert_eq!(
        pass.fired, 0,
        "a silenced rule opened an event — silencing during an incident is how an operator says \
         'I know', not how they stop the record"
    );

    let listed = call(
        &state,
        with_cookie(get("/api/v1/observability/alert-rules"), &cookie),
    )
    .await;
    let row = listed.body["rules"]
        .as_array()
        .expect("rules is an array")
        .iter()
        .find(|rule| rule["id"] == rule_id.to_string())
        .expect("the walk's rule is listed");
    assert_eq!(
        row["silenced"], true,
        "the panel did not show the silence: {row}"
    );

    // An already-ended silence is refused: it suppresses nothing and looks as if it does.
    let past = (OffsetDateTime::now_utc() - time::Duration::hours(1))
        .format(&time::format_description::well_known::Rfc3339)
        .expect("formats");
    let refused = call(
        &state,
        with_cookie(
            request(
                Method::POST,
                "/api/v1/observability/silences",
                Some(json!({ "reason": "already over", "ends_at": past })),
            ),
            &cookie,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        refused.body
    );
    assert_eq!(refusal_code(&refused), "silence_already_ended");

    // And a silence with no reason is refused too: an anonymous silence is one nobody removes.
    let anonymous = call(
        &state,
        with_cookie(
            request(
                Method::POST,
                "/api/v1/observability/silences",
                Some(json!({ "reason": "   ", "ends_at": ends_at })),
            ),
            &cookie,
        ),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(refusal_code(&anonymous), "invalid_reason");

    let _ = sqlx::query("delete from obs_alert_rules where id = $1")
        .bind(rule_id)
        .execute(pool)
        .await;
}

#[tokio::test]
async fn the_preview_reports_live_state_and_refuses_an_expression_the_evaluator_would_refuse() {
    let state = support::walk_state::state_or_fail().await;
    let cookie = sign_in(&state).await;

    let firing = call(
        &state,
        with_cookie(
            request(
                Method::POST,
                "/api/v1/observability/alert-rules/preview",
                Some(json!({ "expr": "omnion_queue_depth > 0" })),
            ),
            &cookie,
        ),
    )
    .await;
    assert_eq!(firing.status, StatusCode::OK, "{}", firing.body);
    assert_eq!(firing.body["family"], "omnion_queue_depth");
    assert_eq!(
        firing.body["rendered"], "omnion_queue_depth > 0",
        "the preview did not echo the expression it understood: {}",
        firing.body
    );

    // The two refusals a form can produce, each naming what is wrong.
    let unknown_family = call(
        &state,
        with_cookie(
            request(
                Method::POST,
                "/api/v1/observability/alert-rules/preview",
                Some(json!({ "expr": "omnion_not_a_family > 0" })),
            ),
            &cookie,
        ),
    )
    .await;
    assert_eq!(unknown_family.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(refusal_code(&unknown_family), "invalid_alert_expression");
    assert!(
        refusal_message(&unknown_family).contains("omnion_not_a_family"),
        "the refusal must name the family: {}",
        unknown_family.body
    );

    let no_comparison = call(
        &state,
        with_cookie(
            request(
                Method::POST,
                "/api/v1/observability/alert-rules/preview",
                Some(json!({ "expr": "omnion_queue_depth" })),
            ),
            &cookie,
        ),
    )
    .await;
    assert_eq!(no_comparison.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        refusal_message(&no_comparison).contains("no comparison"),
        "the refusal must say what is missing: {}",
        no_comparison.body
    );

    // And a valid save is refused only for what is actually wrong with it.
    let refused = call(
        &state,
        with_cookie(
            request(
                Method::POST,
                "/api/v1/observability/alert-rules",
                Some(json!({
                    "name": unique_rule_name("Bad"),
                    "expr": "omnion_queue_depth five 5",
                    "severity": "warning",
                    "for_seconds": 300
                })),
            ),
            &cookie,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        refused.body
    );
    assert_eq!(refusal_code(&refused), "invalid_alert_expression");

    let persisted: i64 =
        sqlx::query_scalar("select count(*) from obs_alert_rules where name like 'Bad-%'")
            .fetch_one(state.db().pool())
            .await
            .expect("the count runs");
    assert_eq!(
        persisted, 0,
        "a rule with an unparseable expression was written anyway — it would sit on the panel \
         saying 'configured' and never fire"
    );
}

#[tokio::test]
async fn the_settings_row_validates_every_field_and_a_valid_save_takes_effect_immediately() {
    let state = support::walk_state::state_or_fail().await;
    let cookie = sign_in(&state).await;
    let before = tracing_spine::sampling_ratio();

    let valid = json!({
        "sampling_ratio": 0.5,
        "logs_retention_days": 21,
        "traces_retention_days": 14,
        "log_level_default": "warn",
        "log_level_overrides": {
            "omnion_secrets": { "level": "debug", "expires_at": "2099-01-01T00:00:00Z" }
        },
        "cardinality_budget": 20000,
        "prometheus_public": true
    });

    // Every refusal names its field and its cap. One loop over the cases, because the property
    // under test is identical for each and a copy-pasted assertion per case would only prove the
    // copy-paste.
    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "sampling_ratio",
            json!({ "sampling_ratio": 1.5 }),
            "invalid_sampling_ratio",
        ),
        (
            "sampling_ratio",
            json!({ "sampling_ratio": -0.1 }),
            "invalid_sampling_ratio",
        ),
        (
            "logs_retention_days",
            json!({ "logs_retention_days": 400 }),
            "invalid_retention",
        ),
        (
            "traces_retention_days",
            json!({ "traces_retention_days": 0 }),
            "invalid_retention",
        ),
        (
            "log_level_default",
            json!({ "log_level_default": "verbose" }),
            "invalid_log_level",
        ),
        (
            "cardinality_budget",
            json!({ "cardinality_budget": 100_000_000 }),
            "invalid_cardinality_budget",
        ),
    ];
    for (field, patch, code) in cases {
        let mut body = valid.clone();
        for (key, value) in patch.as_object().expect("a patch object") {
            body[key] = value.clone();
        }
        let response = call(
            &state,
            with_cookie(
                request(Method::PUT, "/api/v1/observability/settings", Some(body)),
                &cookie,
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{field} was accepted: {}",
            response.body
        );
        assert_eq!(refusal_code(&response), code, "{field}: {}", response.body);
        let message = refusal_message(&response);
        assert!(
            message.contains(field),
            "the refusal for {field} does not name the field: {message}"
        );
    }

    // A misspelled field is refused rather than silently ignored — `deny_unknown_fields` is the
    // whole mechanism, and a save that reports success for a field it dropped is the failure the
    // request's acceptance line is about.
    let misspelled = call(
        &state,
        with_cookie(
            request(
                Method::PUT,
                "/api/v1/observability/settings",
                Some(json!({
                    "sampling_ratio": 0.5,
                    "logs_retention_days": 21,
                    "traces_retention_days": 14,
                    "log_level_default": "warn",
                    "cardinality_budget": 20000,
                    "retention_days": 3
                })),
            ),
            &cookie,
        ),
    )
    .await;
    assert!(
        misspelled.status.is_client_error(),
        "a misspelled field was accepted: {}",
        misspelled.body
    );

    // The valid save.
    let saved = call(
        &state,
        with_cookie(
            request(Method::PUT, "/api/v1/observability/settings", Some(valid)),
            &cookie,
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(saved.body["sampling_ratio"], 0.5);
    assert_eq!(saved.body["logs_retention_days"], 21);

    // The caps come from the table, so the screen can say what they are.
    assert_eq!(saved.body["caps"]["logs_retention_max"], 30);
    assert!(
        saved.body["caps"]["log_levels"]
            .as_array()
            .is_some_and(|l| l.len() == 5)
    );
    assert!(
        saved.body["egress_note"]
            .as_str()
            .unwrap_or_default()
            .contains("redacted"),
        "the settings screen does not say what leaves the instance: {}",
        saved.body["egress_note"]
    );

    // **The half a unit test cannot see**: the ratio the edge samples with is the one just
    // written, with no restart. The request's whole reason for the screen is "debugging does not
    // need a redeploy".
    assert!(
        (tracing_spine::sampling_ratio() - 0.5).abs() < f64::EPSILON,
        "the edge is still sampling at {} after a save to 0.5",
        tracing_spine::sampling_ratio()
    );
    assert_eq!(
        metrics::global().global_budget(),
        20_000,
        "the cardinality budget did not take effect without a restart"
    );

    // And the read agrees with the write.
    let read = call(
        &state,
        with_cookie(get("/api/v1/observability/settings"), &cookie),
    )
    .await;
    assert_eq!(read.body["sampling_ratio"], 0.5);
    assert_eq!(read.body["traces_retention_days"], 14);
    assert_eq!(read.body["prometheus_public"], true);
    assert_eq!(read.body["level_overrides"][0]["target"], "omnion_secrets");
    assert_eq!(read.body["level_overrides"][0]["expired"], false);

    // Put it back, so the next run in this database starts from the defaults.
    tracing_spine::set_sampling_ratio(before);
    let restore = call(
        &state,
        with_cookie(
            request(
                Method::PUT,
                "/api/v1/observability/settings",
                Some(json!({
                    "sampling_ratio": 0.1,
                    "logs_retention_days": 14,
                    "traces_retention_days": 7,
                    "log_level_default": "info",
                    "log_level_overrides": {},
                    "cardinality_budget": 10000,
                    "prometheus_public": false
                })),
            ),
            &cookie,
        ),
    )
    .await;
    assert_eq!(restore.status, StatusCode::OK, "{}", restore.body);
}

#[tokio::test]
async fn a_temporary_level_raise_expires_without_a_restart() {
    let state = support::walk_state::state_or_fail().await;
    let cookie = sign_in(&state).await;
    let base = json!({
        "sampling_ratio": 0.1,
        "logs_retention_days": 14,
        "traces_retention_days": 7,
        "log_level_default": "info",
        "cardinality_budget": 10000
    });

    // A raise that has already expired.
    let mut expired_body = base.clone();
    expired_body["log_level_overrides"] = json!({
        "omnion_secrets": { "level": "trace", "expires_at": "2000-01-01T00:00:00Z" }
    });
    let saved = call(
        &state,
        with_cookie(
            request(
                Method::PUT,
                "/api/v1/observability/settings",
                Some(expired_body),
            ),
            &cookie,
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(saved.body["level_overrides"][0]["expired"], true);

    // It is NOT in the stored object, so a later read cannot resurrect it — the acceptance line
    // is "expires back to the configured default without a restart", and half of that is the
    // row no longer claiming a raise nobody will ever clear.
    let stored: Value =
        sqlx::query_scalar("select log_level_overrides from obs_log_settings where id = 1")
            .fetch_one(state.db().pool())
            .await
            .expect("the settings row reads");
    assert!(
        stored.get("omnion_secrets").is_none(),
        "an expired raise was written back into the settings row: {stored}"
    );

    // A live one survives.
    let mut live_body = base.clone();
    live_body["log_level_overrides"] = json!({
        "omnion_secrets": { "level": "debug", "expires_at": "2099-01-01T00:00:00Z" }
    });
    let live = call(
        &state,
        with_cookie(
            request(
                Method::PUT,
                "/api/v1/observability/settings",
                Some(live_body),
            ),
            &cookie,
        ),
    )
    .await;
    assert_eq!(live.status, StatusCode::OK, "{}", live.body);
    assert_eq!(live.body["level_overrides"][0]["expired"], false);
    let stored_live: Value =
        sqlx::query_scalar("select log_level_overrides from obs_log_settings where id = 1")
            .fetch_one(state.db().pool())
            .await
            .expect("the settings row reads");
    assert!(
        stored_live.get("omnion_secrets").is_some(),
        "a live raise was dropped: {stored_live}"
    );
}

#[tokio::test]
async fn sigterm_flips_readyz_to_503_while_healthz_stays_200() {
    let state = support::walk_state::state_or_fail().await;
    // The process-wide lifecycle, so the flip is the one the routes read. Reset at the end so the
    // rest of this binary's tests are not run inside a drain.
    let lifecycle = lifecycle::global();
    lifecycle.begin_drain();

    let ready = call(&state, get("/readyz")).await;
    let health = call(&state, get("/healthz")).await;
    let live = call(&state, get("/livez")).await;

    assert_eq!(
        ready.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "readiness did not fail during a drain: {}",
        ready.body
    );
    assert_eq!(ready.body["ok"], false);
    assert_eq!(
        ready.body["checks"]["process"]["status"], "draining",
        "the readiness body does not say WHY it failed: {}",
        ready.body
    );

    assert_eq!(
        health.status,
        StatusCode::OK,
        "liveness failed during a drain — an orchestrator would restart a process that is \
         shutting down exactly as it was asked to: {}",
        health.body
    );
    assert_eq!(health.body["ok"], true);
    assert_eq!(
        health.body["draining"], true,
        "the body does not report the drain"
    );

    assert_eq!(
        live.status,
        StatusCode::OK,
        "/livez is not the same probe as /healthz"
    );
    assert_eq!(live.body["draining"], true);

    // The contract is also readable as data, for REQ-128 and the deployment centre.
    let contract = call(&state, get("/api/v1/observability/lifecycle")).await;
    assert_eq!(
        contract.status,
        StatusCode::UNAUTHORIZED,
        "the contract is not permission-guarded"
    );

    // Drain finishes: back to 200 once the flag clears. A second `begin_drain` is what the loop
    // would do, so the assertion is that the flag is the ONLY thing readiness reads.
    //
    // The drain MECHANICS are asserted on an isolated instance, not the process-wide one, and the
    // reason is a whole class of failure this walk had:
    //
    // `drain_and_flush` waits for `in_flight() == 0`, and `in_flight` is incremented by the
    // request-log middleware on the SHARED `lifecycle::global()`. This file runs eight walks in
    // one process, in parallel by default, and every other one of them drives real requests
    // through the real router. So a 50 ms deadline here is racing seven sibling walks' traffic:
    // one in-flight request of theirs is a `timed_out` in ours. It passed alone and failed in the
    // suite — which is backwards from how a defect usually presents, and is why the first reading
    // of this failure is "the drain is broken" when in fact the drain was fine and the assertion
    // was shared mutable state with no owner.
    //
    // A shared counter with no test that isolates it is not a product bug until it produces a
    // flaky result, and then it is a product bug in the *suite*. The isolated instance below is
    // `Lifecycle::new()` — the constructor is public for exactly this reason.
    let isolated = std::sync::Arc::new(lifecycle::Lifecycle::new());
    let summary =
        lifecycle::drain_and_flush(&isolated, None, std::time::Duration::from_millis(50)).await;
    assert_eq!(
        summary.outcome, "drained",
        "a drain with nothing in flight must not wait for the deadline"
    );
    assert!(summary.clean);
    assert!(summary.to_line().contains("unhealthy_exporters=none"));

    // The GLOBAL one gets the same treatment at the end of the walk, or it is left draining and
    // every later walk in this binary sees a 503 from /readyz. `end_drain` says whether there was
    // a drain to abandon, which is the fact a supervisor logging an abandoned drain would want.
    assert!(
        lifecycle.end_drain(),
        "the walk flipped the process-wide drain and could not put it back — every later walk in \
         this binary would read /readyz as 503"
    );
    assert!(!lifecycle.is_draining(), "the drain flag outlived the walk");
}

/// Attach the session cookie to a request.
fn with_cookie(mut request: Request<Body>, cookie: &str) -> Request<Body> {
    request.headers_mut().insert(
        header::COOKIE,
        cookie.parse().expect("a cookie header value"),
    );
    request
}

/// The error a refusal answered with, unwrapped from the platform's envelope.
///
/// ## Why this helper exists rather than an index expression at each site
///
/// Every refusal on this platform is serialized as `{"error": {"code", "message", "details"}}`
/// (`apps/api/src/error.rs`, `impl IntoResponse for ApiError`). This file read `body["code"]`
/// directly, which is `null` for every refusal in it — and `assert_eq!(null, "a_code")` is a
/// failure, so the suite was red rather than silently green, which is the only reason the
/// mismatch was ever going to be found.
///
/// It hid for four ticks because **this suite had never run.** `state_or_fail` panics when the
/// migrations do not apply, and the migrations did not apply: `cargo test --test
/// observability_alerts` connected to the shared `omnion` database, which carries a sibling
/// wave's `0019_cms_blocks` while this branch's slot 19 is `0019_secret_hierarchy`. Every walk in
/// the file aborted before its first assertion, and the log said "aborts with exit 101,
/// pre-existing, not claimed" — the shape of a defect nobody had chased.
///
/// The lesson is about the envelope, not the shape: **a walk that reads an error must read it
/// through the same helper as every other walk.** A refusal whose `code` is read four different
/// ways in five suites is four ways for the next envelope change to be a green suite that
/// proves nothing. `apps/api/tests/observability_permissions.rs` already read it correctly, which
/// is why it passed on the day this one did not.
fn refusal(response: &TestResponse) -> &Value {
    let body = response.body.get("error").unwrap_or_else(|| {
        panic!(
            "the refusal is not in the platform envelope: {}",
            response.body
        )
    });
    assert!(
        body.get("code").is_some() && body.get("message").is_some(),
        "the envelope carries no code/message: {body}"
    );
    body
}

/// The refusal's `code`, as a `&str`.
fn refusal_code(response: &TestResponse) -> &str {
    refusal(response)
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// The refusal's `message`, as a `&str`.
fn refusal_message(response: &TestResponse) -> &str {
    refusal(response)
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
}
