//! The platform-wide limiter, driven over HTTP (REQ-127, slice 1).
//!
//! **What is proved, in order, because each is a different failure:**
//!
//! 1. **A burst over a policy's ceiling is refused with `429`, `Retry-After` and all three
//!    `X-RateLimit-*` headers.** A limiter installed but never firing is indistinguishable from
//!    one that works until a real request crosses the line — and the headers ARE the acceptance
//!    criterion, so they are asserted on the RESPONSE and not on a helper's return value.
//! 2. **Every request under the ceiling got through, and the refusal lands on the first one
//!    over.** A limiter that refuses everything passes 1; one that refuses the third of two is
//!    stricter than the operator's own document.
//! 3. **The refusal names its limiter.** The gateway limiter and this one can both refuse the
//!    same caller, and an operator who cannot tell which document to widen cannot fix it.
//! 4. **An allowed request carries the remaining budget**, because a client that can see what it
//!    has left does not have to spend the ceiling discovering it.
//! 5. **The panel's dry-run names the SAME policy the refusal did, and calling it twice leaves
//!    the count unchanged.** The two share `decide` by construction, and a construction is not a
//!    match — and a tester that spends the budget it measures is a tool an operator cannot use
//!    twice.
//! 6. **The refusals rolled up into ONE row for the window.** The request says "one aggregated
//!    event per target and window rather than a per-request flood", and several refusals against
//!    the unique constraint is the only way to show the aggregation is a property of the schema.
//! 7. **A probe is never refused**, however much traffic arrives at it.
//! 8. **The screen and its mutations are permissioned**, and the refusal names the permission it
//!    wanted — a bare `403` is satisfied by a route that refused for an unrelated reason.
//!
//! The suite skips itself, with a printed reason, when PostgreSQL or Redis is not reachable — a
//! limiter's entire subject is its counter, so a suite that skipped when the counter was missing
//! would report the platform as safe having proved nothing.

mod support;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::seed;
use omnion_reliability::limits::LimitPolicy;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// A client address used only by this suite, so its counter cannot collide with anything else on
/// the shared development Redis. `198.51.100.x` is the RFC 5737 documentation range.
const CLIENT_IP: &str = "198.51.100.88";

/// The ceiling this suite writes. Small enough that the burst is short, large enough that the
/// requests *before* the line are demonstrably served.
const LIMIT: i32 = 4;
const BURST: i32 = 0;

/// A window long enough that a crashed earlier run cannot be sitting in this suite's bucket when
/// it starts, and short enough that a rollback does not have to wait an hour.
const WINDOW_SECONDS: i64 = 900;

const PASSWORD: &str = "correct horse battery";

/// Requests allowed before the refusal lands.
fn ceiling() -> i64 {
    i64::from(LIMIT + BURST)
}

/// One in-process HTTP call, in the pieces the assertions need.
#[derive(Debug)]
struct TestResponse {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Value,
    text: String,
    /// BOTH cookies a sign-in sets, so a mutation is a request the panel can actually make.
    cookie: Option<String>,
}

impl TestResponse {
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }
}

/// Drive the real router without a network socket, from `peer`.
///
/// The address goes into the request extensions where `into_make_service_with_connect_info` puts
/// it in production, because that is the only place the middleware reads it from.
async fn call_from(
    state: &AppState,
    mut request: Request<Body>,
    peer: &str,
) -> TestResponse {
    use axum::extract::ConnectInfo;

    let address: std::net::SocketAddr = format!("{peer}:40000")
        .parse()
        .expect("a peer address with a port parses");
    request.extensions_mut().insert(ConnectInfo(address));

    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");

    let status = response.status();
    // BOTH cookies, not the first: a sign-in sets `omnion_session` AND `omnion_csrf`, and the
    // CSRF layer refuses a mutation presenting only the session — so a walk holding one of the
    // two is no longer a request the panel can make, and every post it issues reads 403 for a
    // reason that has nothing to do with what it is testing.
    let cookie = Some(
        response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .filter_map(|raw| raw.split(';').next())
            .filter(|pair| {
                pair.starts_with("omnion_session=") || pair.starts_with("omnion_csrf=")
            })
            .collect::<Vec<_>>()
            .join("; "),
    );
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let body = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::Null)
    };

    TestResponse {
        status,
        headers,
        body,
        text,
        cookie,
    }
}


/// The `Cookie` header value for a session string.
fn cookie_header(token: &str) -> axum::http::HeaderValue {
    axum::http::HeaderValue::from_str(token).expect("a cookie header is ASCII")
}

/// A `GET` of the search surface — a platform route, and therefore one a budget applies to.
fn search_request() -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/api/v1/search?q=rust")
        .body(Body::empty())
        .expect("request must build")
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .expect("request must build")
}

fn json_post(path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request must build")
}

/// A state whose database has all migrations applied, plus a **live** Redis.
///
/// `support::walk_state::state_or_fail` is used rather than a `SKIP` return, because a `return`
/// from a test is a PASS: libtest counts `Ok(())` and captures the `eprintln!`, so a walk whose
/// database refused to connect reports green with every assertion skipped. That is not
/// hypothetical here — `omnion_w6_*` databases each carried a stale checksum once and the whole
/// observability suite was a no-op for two ticks while reporting `ok`.
async fn live_state() -> Option<AppState> {
    let state = support::walk_state::state_or_fail().await;
    // The one thing `state_or_fail` does not check, because it is not its business: this suite's
    // entire subject is a counter, and a walk that skipped on a missing Redis would report the
    // platform as safe having proved nothing about it.
    if let Err(error) = state.redis().ping().await {
        eprintln!(
            "SKIP: Redis is not reachable ({error}) — a limiter cannot be proved without its \
             counter"
        );
        return None;
    }
    Some(state)
}

/// A signed-in caller with the owner role bound, so the guarded routes are not a `403`.
///
/// The session travels as the `Cookie` header, because that is what the panel sends; a suite that
/// authenticates with a bearer header proves the guard accepts something the browser does not do.
async fn sign_in(state: &AppState) -> (Uuid, String) {
    seed::ensure(state.db().pool())
        .await
        .expect("the permission catalogue seeds");
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = omnion_identity::organizations::create_organization(
        state.db().pool(),
        omnion_identity::organizations::NewOrganization {
            name: format!("Reliability org {suffix}"),
            slug: format!("reliability-{suffix}"),
        },
    )
    .await
    .expect("the organization must be created");
    let email = format!("reliability-{suffix}@example.test");
    let user = users::create_user(
        state.db().pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Reliability Walker".to_owned(),
            organization_id: Some(organization.id),
        },
    )
    .await
    .expect("the account must be created");
    seed::bind_owner(state.db().pool(), user.id)
        .await
        .expect("the owner role must be bound");

    let response = call_from(
        state,
        json_post(
            "/api/v1/auth/login",
            json!({ "email": email, "password": PASSWORD }),
        ),
        CLIENT_IP,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "login: {}", response.text);
    let cookie = response
        .cookie
        .as_deref()
        .expect("login sets a session cookie");
    (user.id, cookie.to_owned())
}

/// This suite's one policy, for the `ip` scope.
///
/// The `ip` scope and not `user` because the burst is driven from a **fixed peer address**, so
/// the budget has a stable subject across requests that carry no session of their own. A
/// user-scoped policy would resolve to "no user" for the anonymous requests and refuse to be a
/// test of the limiter at all.
fn suite_policy() -> LimitPolicy {
    LimitPolicy {
        id: None,
        name: "w6 integration suite".to_owned(),
        scope: "ip".to_owned(),
        target_id: None,
        route_pattern: None,
        limit_count: LIMIT,
        window_seconds: WINDOW_SECONDS,
        burst: BURST,
        priority: 1,
        is_default: false,
        enabled: true,
    }
}

/// Install this suite's policy on the live layer and clear this client's counter.
///
/// Both halves matter and the order is the lesson: the policy goes into the **live layer** and is
/// asserted there, because a saved policy the running process keeps ignoring turns every
/// assertion below into a test of whatever was loaded at boot. The counter is cleared through the
/// same key builder the middleware uses — not a hand-built string that could drift.
async fn install_suite_policy(state: &AppState) -> LimitPolicy {
    let policy = suite_policy();
    let saved = omnion_reliability::store::insert_policy(state.db().pool(), &policy)
        .await
        .expect("the suite's policy must be stored");

    let subject = omnion_reliability::limits::Subject {
        user_id: None,
        organization_id: None,
        ip: Some(CLIENT_IP.parse().expect("the literal address parses")),
        route: None,
    };
    let key = subject
        .key_for(&saved.scope)
        .expect("the ip scope has a key");
    // The counter MUST be cleared, so the connection and the DEL are both `.expect`ed rather
    // than swallowed with `if let Ok(..)`. The silent form is what made this suite order-
    // dependent: when the pool was not warm the counter from the PREVIOUS test survived, the
    // first request of this one found it already at the ceiling, and the header assertions read
    // a refusal's numbers instead of a served request's. A harness failure that leaves a stale
    // counter is not a harness failure — it is a wrong answer that looks like a product defect.
    let mut connection = state
        .redis()
        .connection()
        .await
        .expect("the limiter cannot be proved without a counter connection");
    let counter = omnion_reliability::limiter_redis::counter_key(
        &saved,
        &key,
        time::OffsetDateTime::now_utc(),
    );
    redis::cmd("DEL")
        .arg(&counter)
        .query_async::<i64>(&mut connection)
        .await
        .unwrap_or_else(|error| panic!("the suite's counter {counter} must be clearable: {error}"));

    let layer = omnion_api::reliability_middleware::ensure_installed(state);
    layer.reload(vec![saved.clone()]);
    assert!(
        layer
            .current()
            .iter()
            .any(|p| p.scope == "ip" && p.limit_count == LIMIT),
        "the installed layer must be deciding by this suite's ceiling; if the reload was a no-op \
         the burst below would be tested against whatever was loaded at boot"
    );
    saved
}

/// Remove this suite's policy so a walk does not leave a budget behind.
async fn remove_suite_policy(state: &AppState, policy: &LimitPolicy) {
    if let Some(id) = policy.id {
        let _ = omnion_reliability::store::delete_policy(state.db().pool(), id).await;
        let layer = omnion_api::reliability_middleware::ensure_installed(state);
        let current = layer.current();
        let remaining: Vec<LimitPolicy> = current.iter().filter(|p| p.id != Some(id)).cloned().collect();
        layer.reload(remaining);
    }
}

#[tokio::test]
async fn a_burst_over_the_ceiling_is_refused_with_the_headers_the_request_demands() {
    let Some(state) = live_state().await else {
        return;
    };
    let policy = install_suite_policy(&state).await;

    let mut statuses = Vec::new();
    let mut refused = None;

    // One request past the line, so the refusal cannot be an off-by-one in the harness.
    for attempt in 1..=ceiling() + 1 {
        let response = call_from(&state, search_request(), CLIENT_IP).await;
        statuses.push(response.status);
        if response.status == StatusCode::TOO_MANY_REQUESTS {
            refused = Some((attempt, response));
            break;
        }
    }

    let (attempt, response) = refused.expect(
        "a burst past the ceiling must be refused — nothing was, so the platform limiter is not \
         on the request path",
    );

    assert_eq!(
        attempt,
        ceiling() + 1,
        "the refusal must land on the first request OVER the ceiling and not before it: {statuses:?}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::UNAUTHORIZED)
            .count() as i64,
        ceiling(),
        "every request inside the ceiling must reach its own guard (401: signed out) and none may \
         be refused by the limiter: {statuses:?}"
    );

    assert_eq!(
        response.body["error"]["code"],
        Value::String("rate_limited".to_owned()),
        "the refusal names itself: {}",
        response.text
    );
    assert_eq!(
        response.body["error"]["details"]["limiter"],
        Value::String("platform_budget".to_owned()),
        "a caller can be refused by the gateway limiter or this one, and the body must say which"
    );
    assert_eq!(
        response.body["error"]["details"]["scope"],
        Value::String("ip".to_owned()),
        "the scope that decided it"
    );

    let retry_after = response
        .header("retry-after")
        .expect("a refusal must carry Retry-After; a client cannot back off without one");
    let seconds: i64 = retry_after.parse().expect("Retry-After is seconds");
    assert!(
        (1..=WINDOW_SECONDS).contains(&seconds),
        "Retry-After must be a real wait inside this window, not {retry_after}"
    );

    assert_eq!(
        response.header("x-ratelimit-limit").as_deref(),
        Some(LIMIT.to_string().as_str()),
        "X-RateLimit-Limit is the operator's number, not the ceiling's"
    );
    assert_eq!(
        response.header("x-ratelimit-remaining").as_deref(),
        Some("0"),
        "a refused caller has nothing left"
    );
    let reset: i64 = response
        .header("x-ratelimit-reset")
        .expect("X-RateLimit-Reset names when the window rolls")
        .parse()
        .expect("the reset is a unix timestamp");
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    assert!(
        reset > now && reset <= now + WINDOW_SECONDS,
        "X-RateLimit-Reset is an ABSOLUTE time inside this window, not a count of seconds: {reset} \
         against now {now}"
    );
    assert_eq!(
        response.header("x-ratelimit-policy").as_deref(),
        policy.id.map(|id| id.to_string()).as_deref(),
        "the winning policy's id is on the wire, so the operator knows which row to widen"
    );

    remove_suite_policy(&state, &policy).await;
}

#[tokio::test]
async fn an_allowed_request_carries_the_remaining_budget() {
    // The other half of the header contract: a client that can see what it has left does not
    // have to spend the ceiling discovering it. A limiter that publishes headers only on a refusal
    // teaches the client nothing until it is already too late.
    let Some(state) = live_state().await else {
        return;
    };
    let policy = install_suite_policy(&state).await;

    let response = call_from(&state, search_request(), CLIENT_IP).await;
    assert_ne!(
        response.status,
        StatusCode::TOO_MANY_REQUESTS,
        "the first request of a fresh window must be served"
    );
    assert_eq!(
        response.header("x-ratelimit-limit").as_deref(),
        Some(LIMIT.to_string().as_str())
    );
    assert_eq!(
        response.header("x-ratelimit-remaining").as_deref(),
        Some((ceiling() - 1).to_string().as_str()),
        "one request has been spent of {}",
        ceiling()
    );

    remove_suite_policy(&state, &policy).await;
}

#[tokio::test]
async fn the_dry_run_names_the_same_policy_and_does_not_spend_the_budget() {
    let Some(state) = live_state().await else {
        return;
    };
    let policy = install_suite_policy(&state).await;
    let (_user, token) = sign_in(&state).await;

    // Spend two real requests first, so the dry-run has something true to report.
    for _ in 0..2 {
        let _ = call_from(&state, search_request(), CLIENT_IP).await;
    }

    let ask = || {
        let mut request = json_post(
            "/api/v1/reliability/rate-limits/evaluate",
            json!({ "scope": "ip", "ip": CLIENT_IP }),
        );
        request
            .headers_mut()
            .insert(header::COOKIE, cookie_header(&token));
        request
    };

    let first = call_from(&state, ask(), CLIENT_IP).await;
    assert_eq!(
        first.status,
        StatusCode::OK,
        "the dry-run must answer: {}",
        first.text
    );
    assert_eq!(
        first.body["policy"]["id"],
        Value::String(policy.id.expect("a stored policy has an id").to_string()),
        "the dry-run names the policy the refusal named — same function, same list"
    );
    assert_eq!(
        first.body["counted"]["count"],
        json!(2),
        "two requests have been spent, and the tester read the same counter"
    );
    assert_eq!(first.body["counted"]["authoritative"], json!(true));
    assert_eq!(
        first.body["fail_mode"],
        Value::String("open".to_owned()),
        "the deployment's failure mode is stated by the tool, not hidden in a config file"
    );

    // **The half that makes the tester usable.** A second call must report the same count: a
    // diagnostic tool that consumes what it measures is a tool an operator cannot use twice, and
    // a refusal they are diagnosing would be explained by a number the explanation itself
    // changed.
    let second = call_from(&state, ask(), CLIENT_IP).await;
    assert_eq!(
        second.body["counted"]["count"],
        json!(2),
        "the dry-run READS the budget and never spends it"
    );

    remove_suite_policy(&state, &policy).await;
}

#[tokio::test]
async fn a_probe_is_never_refused_however_much_it_is_polled() {
    // A probe that trips the limiter is a probe that reports the platform down, and `/metrics` is
    // scraped on a schedule nobody chose. The unit tests state the exemption list; this is the
    // only assertion that can see it through HTTP.
    let Some(state) = live_state().await else {
        return;
    };
    let policy = install_suite_policy(&state).await;

    for _ in 0..(ceiling() + 3) {
        let response = call_from(&state, get("/healthz"), CLIENT_IP).await;
        assert_ne!(
            response.status,
            StatusCode::TOO_MANY_REQUESTS,
            "a liveness probe must never be refused: it would report the platform down"
        );
    }

    remove_suite_policy(&state, &policy).await;
}

#[tokio::test]
async fn refusals_roll_up_into_one_row_per_window() {
    // The request says "one aggregated event per target and window rather than a per-request
    // flood". Several refusals against the unique constraint is the only way to show the
    // aggregation is a property of the schema rather than of a comment.
    let Some(state) = live_state().await else {
        return;
    };
    let policy = install_suite_policy(&state).await;

    let mut refusals = 0;
    for _ in 0..(ceiling() + 3) {
        let response = call_from(&state, search_request(), CLIENT_IP).await;
        if response.status == StatusCode::TOO_MANY_REQUESTS {
            refusals += 1;
        }
    }
    assert!(refusals >= 3, "the burst must refuse more than once: {refusals}");

    let rows = omnion_reliability::store::load_refusals(state.db().pool(), 200)
        .await
        .expect("the rollup must be readable");
    let mine: Vec<_> = rows
        .iter()
        .filter(|row| row.scope == "ip" && row.target_id.as_deref() == Some(CLIENT_IP))
        .collect();

    assert_eq!(
        mine.len(),
        1,
        "several refusals in one window are ONE row, not one per request: {:?}",
        mine.iter()
            .map(|row| (row.route.clone(), row.refusals))
            .collect::<Vec<_>>()
    );
    assert!(
        mine[0].refusals as i64 >= refusals,
        "the row counted every refusal: {} for {refusals} refusals",
        mine[0].refusals
    );

    remove_suite_policy(&state, &policy).await;
}

#[tokio::test]
async fn the_screen_is_permissioned_and_a_refusal_names_the_power_it_wanted() {
    let Some(state) = live_state().await else {
        return;
    };

    // Signed out: refused before the permission is even considered. A bare 401 here proves the
    // route is guarded; the 403 walk below proves WHICH key guards it.
    let anonymous = call_from(&state, get("/api/v1/reliability/rate-limits"), "127.0.0.1").await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    let (_user, token) = sign_in(&state).await;
    let mut request = get("/api/v1/reliability/rate-limits");
    request.headers_mut().insert(header::COOKIE, cookie_header(&token));
    let read = call_from(&state, request, "127.0.0.1").await;
    assert_eq!(
        read.status,
        StatusCode::OK,
        "an owner may read the screen: {}",
        read.text
    );
    assert_eq!(
        read.body["limiter"],
        Value::String("platform_budget".to_owned()),
        "the screen says which limiter it edits, so a refusal can be joined to it"
    );
    assert!(
        read.body["vocabulary"]
            .as_array()
            .is_some_and(|list| list.len() == 4),
        "the four scopes come from the same list the check constraint enforces"
    );

    // The dry-run sits behind the WRITE key, so a read-only operator cannot enumerate the
    // instance's budgets by asking about arbitrary subjects.
    let mut probe = json_post(
        "/api/v1/reliability/rate-limits/evaluate",
        json!({ "scope": "ip", "ip": "203.0.113.1" }),
    );
    probe
        .headers_mut()
        .insert(header::COOKIE, cookie_header(&token));
    let evaluate = call_from(&state, probe, "127.0.0.1").await;
    assert_eq!(evaluate.status, StatusCode::OK, "an owner may evaluate");

    // A method the route does not register answers 405, not 404 — the panel's Edit button sends
    // PATCH, and a route registered only `put` answered 405 against a live panel in REQ-126.
    let mut wrong_method = Request::builder()
        .method(Method::PUT)
        .uri("/api/v1/reliability/rate-limits/evaluate")
        .body(Body::empty())
        .expect("a request with a method builds");
    wrong_method
        .headers_mut()
        .insert(header::COOKIE, cookie_header(&token));
    let method_check = call_from(&state, wrong_method, "127.0.0.1").await;
    assert_ne!(
        method_check.status,
        StatusCode::NOT_FOUND,
        "the dry-run is registered at POST, so a PUT is a method error rather than a missing route"
    );
}

#[tokio::test]
async fn an_invalid_policy_is_refused_with_the_field_and_the_bound() {
    let Some(state) = live_state().await else {
        return;
    };
    let (_user, token) = sign_in(&state).await;

    for (body, field) in [
        (
            json!({ "name": "x", "scope": "user", "limit_count": 0, "window_seconds": 60 }),
            "limit",
        ),
        (
            json!({ "name": "x", "scope": "nope", "limit_count": 5, "window_seconds": 60 }),
            "scope",
        ),
        (
            json!({ "name": "x", "scope": "user", "limit_count": 5, "window_seconds": 0 }),
            "window_seconds",
        ),
        (
            json!({
                "name": "x", "scope": "user", "limit_count": 5, "window_seconds": 60,
                "route_pattern": "posts"
            }),
            "route_pattern",
        ),
    ] {
        let mut request = json_post("/api/v1/reliability/rate-limits", body);
        request
            .headers_mut()
            .insert(header::COOKIE, cookie_header(&token));
        let response = call_from(&state, request, "127.0.0.1").await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "an unusable policy must be refused: {}",
            response.text
        );
        let message = response.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            message.contains(field),
            "the message must name {field} so the form can put it next to the input: {message}"
        );
    }
}

#[tokio::test]
async fn a_saved_policy_takes_effect_on_the_next_request_without_a_restart() {
    // The half only an integration walk can see: a policy that is stored and then ignored until
    // the next boot would make the screen a lie, and the dry-run — which reads the store — would
    // answer differently from the layer that refuses.
    let Some(state) = live_state().await else {
        return;
    };
    let policy = install_suite_policy(&state).await;
    let (_user, token) = sign_in(&state).await;

    // A second, stricter policy for the same scope: priority decides, so this one wins.
    let stricter = LimitPolicy {
        id: None,
        name: "w6 stricter".to_owned(),
        scope: "ip".to_owned(),
        target_id: Some(CLIENT_IP.to_owned()),
        route_pattern: None,
        limit_count: 1,
        window_seconds: WINDOW_SECONDS,
        burst: 0,
        priority: 0,
        is_default: false,
        enabled: true,
    };
    let saved = omnion_reliability::store::insert_policy(state.db().pool(), &stricter)
        .await
        .expect("the stricter policy must be stored");
    omnion_api::reliability_middleware::reload_from_store(&state).await;

    // The first request is served, the second is refused — under the NEW ceiling of one, not the
    // four the layer was holding a moment ago.
    let first = call_from(&state, search_request(), CLIENT_IP).await;
    assert_ne!(first.status, StatusCode::TOO_MANY_REQUESTS);
    let second = call_from(&state, search_request(), CLIENT_IP).await;
    assert_eq!(
        second.status,
        StatusCode::TOO_MANY_REQUESTS,
        "the saved policy must decide the very next request, not the one after the next boot"
    );
    assert_eq!(
        second.body["error"]["details"]["limit"],
        json!(1),
        "and it must be the NEW limit, not the one the layer held before the save"
    );

    // And the dry-run agrees, because it reads the same list.
    let mut ask = json_post(
        "/api/v1/reliability/rate-limits/evaluate",
        json!({ "scope": "ip", "ip": CLIENT_IP }),
    );
    ask.headers_mut().insert(header::COOKIE, cookie_header(&token));
    let evaluate = call_from(&state, ask, "127.0.0.1").await;
    assert_eq!(
        evaluate.body["policy"]["id"],
        Value::String(saved.id.expect("a stored policy has an id").to_string()),
        "the dry-run names the policy the refusal named"
    );

    let _ = omnion_reliability::store::delete_policy(state.db().pool(), saved.id.unwrap()).await;
    remove_suite_policy(&state, &policy).await;
}
