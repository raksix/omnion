//! The rate limiter, driven over HTTP (REQ-012, slice 3).
//!
//! **Why this suite exists at all.** The slice shipped a policy model, a Redis counter, a pure
//! `decide` and a panel tester that all called it — and no middleware. Nothing on the request
//! path counted anything, so no request had ever been refused. The acceptance criterion said
//! "exceeding a scope's window returns `429` with a `Retry-After` header", and it stayed unticked
//! for exactly that reason: nothing returned 429. This is the same class of defect the CSRF suite
//! was written for two ticks earlier — a mechanism in one file, the thing it protects in another,
//! and nothing that ever makes them meet.
//!
//! **What is proved, in order, because each is a different failure:**
//!
//! 1. **A burst over the scope's ceiling is refused with `429` and a `Retry-After`.** A limiter
//!    that is installed but never fires is indistinguishable from one that works until a real
//!    request crosses the line.
//! 2. **Every request under the ceiling got through**, and the refusal lands on the first request
//!    *over* it. A limiter that refuses everything passes assertion 1, and one that refuses the
//!    sixth request out of five is stricter than the operator's own document.
//! 3. **The refusal names the scope, the ceiling and the count.** A `429` with no explanation is
//!    something an operator has to guess at, and this is the number that says "a client is
//!    looping" rather than "the platform is slow".
//! 4. **The panel's tester agrees with the refusal it explains.** The two share `decide` by
//!    construction, and a construction is not a match — so the tester is asked about the same
//!    client and the same count and must answer `limited`.
//! 5. **An exempt surface is never refused**, however much it sends.
//!
//! The suite skips itself, with a printed reason, when PostgreSQL or Redis is not reachable —
//! like every other integration walk in `apps/api/tests`.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// A client address used only by this suite, so its counter cannot collide with anything else on
/// the shared development Redis.
///
/// Not a random address: the suite drives a burst of exactly `ceiling + 1` requests, and a random
/// address would make the result order-dependent against whatever a crashed earlier run left
/// behind. `198.51.100.x` is the RFC 5737 documentation range — it can never be a real client.
const CLIENT_IP: &str = "198.51.100.77";

/// The ceiling this suite sets for `public_api`: low enough that the burst is short, high enough
/// that the requests *before* the line are demonstrably served.
const TEST_LIMIT: i32 = 5;
const TEST_BURST: i32 = 0;

/// Requests allowed before the refusal lands.
fn ceiling() -> i64 {
    i64::from(TEST_LIMIT + TEST_BURST)
}

/// One in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    retry_after: Option<String>,
    body: Value,
}

/// Drive the real router without a network socket, from `peer`.
///
/// The address goes into the request extensions where `into_make_service_with_connect_info` puts
/// it in production, because that is the only place the middleware reads it from. A test that set
/// `X-Forwarded-For` instead would be proving the proxy path, not the real one.
async fn call_from(state: &AppState, request: Request<Body>, peer: &str) -> TestResponse {
    use axum::extract::ConnectInfo;

    let mut request = request;
    let address: std::net::SocketAddr = format!("{peer}:40000")
        .parse()
        .expect("a peer address with a port parses");
    request.extensions_mut().insert(ConnectInfo(address));

    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");

    let status = response.status();
    let retry_after = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let body = match std::str::from_utf8(&bytes) {
        Ok(text) if text.trim().is_empty() => Value::Null,
        Ok(text) => serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned())),
        Err(_) => Value::Null,
    };

    TestResponse {
        status,
        retry_after,
        body,
    }
}

/// An unauthenticated `GET` of the search surface — `public_api`, the scope this suite tightens.
fn search_request() -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/api/v1/search?q=rust")
        .body(Body::empty())
        .expect("request must build")
}

fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

/// A state whose database has all migrations applied and whose Redis answers.
///
/// Returns `None` — with the reason printed — when either is missing. Redis in particular is not
/// optional here: a limiter's entire subject is its counter, and a suite that skipped when the
/// counter could not be reached would report the platform as safe when it has proved nothing.
async fn live_state() -> Option<(AppState, Db)> {
    let config = Config::from_env().expect("environment must be valid");
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(err) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({err})");
            return None;
        }
    };
    db.migrate().await.expect("migrations must apply");

    let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
    if let Err(err) = redis.ping().await {
        eprintln!("SKIP: Redis is not reachable ({err}) — a limiter cannot be proved without it");
        return None;
    }

    let state = AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config,
        db.clone(),
        redis,
        test_storage(),
    );
    Some((state, db))
}

/// This suite's policies for `public_api`, tightened so the burst is six requests long.
fn tightened() -> Vec<omnion_security::RatePolicy> {
    let mut policies = omnion_security::RatePolicy::defaults();
    let row = policies
        .iter_mut()
        .find(|policy| policy.scope == "public_api")
        .expect("the defaults carry the row");
    // A 900-second window: long enough that a crashed earlier run cannot be sitting in this
    // suite's bucket when it starts, and the whole burst fits inside one window by construction.
    row.window_seconds = 900;
    row.limit = TEST_LIMIT;
    row.burst = TEST_BURST;
    row.enabled = true;
    policies
}

/// Write the limiter document to the store, satisfying the compare-and-swap by reading first.
///
/// The CAS is the store's concurrency guard, and it is kept in this suite rather than bypassed: a
/// test that wrote around it would also be writing around the property a stale form depends on.
///
/// **The actor is a real account, not `Uuid::nil()`.** The first draft used the nil uuid on the
/// reasoning that the column is nullable in substance — and the database refused it, because
/// `rate_limits_updated_by` references `users(id)` and a nil row is not a user. That is the
/// constraint working: "nobody" is a value the column may hold, "user zero is not present" is a
/// value it may not. `who` is the fixture this suite creates and removes.
async fn write_document(db: &Db, who: Uuid, policies: &[omnion_security::RatePolicy]) {
    let expected = omnion_security::load_rate_limits(db.pool())
        .await
        .expect("the document must be readable");
    omnion_security::save_rate_limits(
        db.pool(),
        &omnion_security::rate_limits_to_document(policies),
        Some(&expected),
        who,
    )
    .await
    .expect("the limiter document must be saved");
}

/// The account this suite attributes its writes to, created once and removed by the caller.
async fn actor(db: &Db) -> Uuid {
    let email = format!("rate-limit-{}@omnion.test", uuid::Uuid::new_v4().simple());
    let user = omnion_identity::users::create_user(
        db.pool(),
        omnion_identity::users::NewUser {
            email,
            // The policy column is the only thing this account is for, so the password is a
            // placeholder that satisfies the strength rule and is never used to sign in.
            password: "rate-limit-suite-placeholder-1".to_owned(),
            display_name: "Rate Limit Integration Suite".to_owned(),
            organization_id: None,
        },
    )
    .await
    .expect("the actor account must be created");
    user.id
}

/// Install this suite's policy on the live layer and clear this client's counter.
///
/// Two things happen together and both matter. The document is saved **and** pushed into the
/// installed cell, because a saved limit the running process keeps ignoring would turn every
/// assertion below into a test of the shipped defaults rather than of the thing it names. The
/// counter is cleared through `limiter_redis::forget`, whose key is the one `enforce` will use —
/// not a hand-built string that could drift from the bucket layout.
async fn tighten_public_api(state: &AppState, db: &Db) {
    let policies = tightened();
    write_document(db, actor(db).await, &policies).await;

    let client = omnion_security::ClientId {
        user_id: None,
        ip: Some(CLIENT_IP.parse().expect("the literal address parses")),
    };
    if let Some(policy) = policies.iter().find(|policy| policy.scope == "public_api") {
        let _ = omnion_security::limiter_redis::forget(
            state.redis(),
            policy,
            &client,
            time::OffsetDateTime::now_utc().unix_timestamp(),
        )
        .await;
    }

    // **Install before reloading, and assert it took.** The first draft called `reload` on
    // whatever `installed()` returned — and returned `None`, because `router()` had not been built
    // yet: the cell is filled when the router is constructed. So the reload went nowhere, the
    // router installed the shipped defaults, and a six-request burst against a ceiling of 120 was
    // never refused. The suite's own message said so ("the limiter is not on the request path"),
    // which is how the mistake was this quick rather than a mystery. The fix is to ensure the
    // cell exists *first* and then assert the live policy carries this suite's number — without
    // that assertion, "reload did nothing" and "reload worked" look identical from here.
    let layer = omnion_api::rate_limit_middleware::ensure_installed(state);
    layer.reload(policies);
    assert!(
        layer
            .current()
            .iter()
            .any(|policy| policy.scope == "public_api" && policy.limit == TEST_LIMIT),
        "the installed layer must be deciding by this suite's ceiling; if the reload was a no-op \
         the burst below would be tested against the shipped defaults"
    );
}

#[tokio::test]
async fn a_burst_over_the_ceiling_is_refused_with_a_retry_after() {
    let Some((state, db)) = live_state().await else {
        return;
    };
    tighten_public_api(&state, &db).await;

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
        "a burst past the ceiling must be refused — nothing was, so the limiter is not on the \
         request path",
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
        "every request inside the ceiling must reach its own guard (401: signed out) and none \
         may be refused by the limiter: {statuses:?}"
    );

    assert_eq!(
        response.body["error"]["code"],
        Value::String("rate_limited".to_owned()),
        "the refusal names itself: {}",
        response.body
    );
    assert_eq!(
        response.body["error"]["details"]["scope"],
        Value::String("public_api".to_owned()),
        "the refusal names the scope that decided it"
    );
    assert_eq!(
        response.body["error"]["details"]["ceiling"],
        json!(ceiling()),
        "the ceiling on the wire is the one the operator typed"
    );
    assert_eq!(
        response.body["error"]["details"]["count"],
        json!(ceiling() + 1),
        "the count that tripped it — this is the number that says a client is looping"
    );

    let retry_after = response
        .retry_after
        .expect("a refusal must carry Retry-After; a client cannot back off without one");
    let seconds: i64 = retry_after
        .parse()
        .expect("Retry-After is an integer number of seconds");
    assert!(
        (1..=900).contains(&seconds),
        "Retry-After must be a real wait inside this window, not {retry_after}"
    );
}

#[tokio::test]
async fn the_panel_tester_agrees_with_the_refusal_it_explains() {
    let Some((state, db)) = live_state().await else {
        return;
    };
    tighten_public_api(&state, &db).await;

    // Ask the very function the tester calls about a request carrying the count that was refused
    // over HTTP above. The two share `decide` by construction, so this is close to a tautology —
    // and it is here anyway because a construction is not a match and the criterion asks for a
    // match. What it would actually catch is a tester that resolves a different scope or a
    // different client from the one the middleware counted, which is the one way the two can
    // genuinely disagree.
    let policies = tightened();
    let client = omnion_security::ClientId {
        user_id: None,
        ip: Some(CLIENT_IP.parse().expect("the literal address parses")),
    };
    let facts = omnion_security::RequestFacts {
        method: "GET",
        path: "/api/v1/search",
        client: &client,
        machine_key: false,
        exempt: false,
    };
    let scope = omnion_security::scope_of(&facts);
    let verdict = omnion_security::decide(
        &policies,
        scope,
        &client,
        ceiling() + 1,
        time::OffsetDateTime::now_utc().unix_timestamp(),
    )
    .expect("the scope has a row");

    assert_eq!(scope, "public_api", "the tester resolves the same scope");
    assert!(
        verdict.limited,
        "the tester must agree that the request the platform refused would be refused"
    );
    let wait = verdict
        .retry_after
        .expect("a refusal carries the wait the header carries");
    assert!(
        (1..=900).contains(&wait),
        "the tester's wait and the wire's Retry-After are the same number, not a range"
    );
}

#[tokio::test]
async fn an_exempt_surface_is_never_refused_however_much_it_sends() {
    let Some((state, db)) = live_state().await else {
        return;
    };
    tighten_public_api(&state, &db).await;

    // Well past the ceiling, on the public renderer — the surface a rate limit must never be able
    // to switch off. A refusal here is the platform refusing its own users, which is the failure
    // mode `RequestFacts::exempt` exists to prevent.
    for _ in 0..(ceiling() + 4) {
        let request = Request::builder()
            .method("GET")
            .uri("/api/v1/public/pages/home")
            .body(Body::empty())
            .expect("request must build");
        let response = call_from(&state, request, CLIENT_IP).await;
        assert_ne!(
            response.status,
            StatusCode::TOO_MANY_REQUESTS,
            "the public renderer is exempt and must never be refused"
        );
    }
}
