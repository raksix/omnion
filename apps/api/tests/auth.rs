//! Integration tests for identity: sign-in, session cookie, `/me` and sign-out.
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) — locally and in CI.
//! When PostgreSQL is not reachable the suite skips itself with a printed reason, so
//! `cargo test` stays usable on a machine without Docker.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, CsrfSecret, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, BootstrapOutcome, NewUser};
use omnion_identity::{IdentityError, sessions};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    body: Value,
}

/// Drive the real router without a network socket.
/// Drive the router from a real peer address.
///
/// `ClientAddress` and the rate-limit middleware both read `ConnectInfo<SocketAddr>` out of the
/// request extensions, and `into_make_service_with_connect_info` is the only thing that puts it
/// there. A request driven straight through `router().oneshot()` therefore arrives with NO
/// address — and that is not a neutral default. With no address the per-address refusal in
/// `sign_in` cannot run at all, so an in-process walk silently skipped the very rule a
/// brute-force walk is supposed to test. That is how this file came to have a lockout walk that
/// passed with the lockout fix reverted.
///
/// The address is therefore **this walk's own**, allocated per test rather than shared: the
/// `sign_in` limiter counts per address, so walks that share one loopback address spend each
/// other's budget and a sign-in round trip is refused `429` by a counter it never incremented.
/// `TEST_PEER` is the allocation point — every helper in this file routes through it, so a new
/// walk cannot reintroduce the sharing by forgetting to opt out.
///
/// **The IP is drawn as well as the port, because varying the port alone never isolated
/// anything.** The account limiter hashes the IP and ignores the port, and so does
/// `sign_in`'s own address rule (`recent_failures_from_address`) — so every walk in this file
/// shared the single `127.0.0.1` budget no matter which port it claimed, and the comment above
/// described an isolation the code did not have. It stayed invisible for as long as the address
/// ceiling was comfortably high: the enforced threshold was the IAM column's 10, giving a budget
/// of 30 failures that no single walk reached. When the sign-in path was switched to the
/// lockout document the operator actually edits — whose own default is 5 — the budget halved to
/// 15 and three walks in this file, none of which touches the threshold, began failing
/// `address_blocked` on 127.0.0.1. The product was right and the harness was wrong, which is the
/// only way that story ends well: the fix is to separate the counters, never to put 10 back.
/// `127.0.0.0/8` is entirely loopback, so the octets below are real, unroutable-free addresses.
fn test_peer() -> String {
    // `as_u128` rather than `simple()` — the latter is a Display formatter, not a value.
    let draw = Uuid::new_v4().as_u128();
    format!(
        "127.{}.{}.{}:{}",
        1 + (draw % 200) as u8,
        ((draw >> 8) % 250) as u8,
        1 + ((draw >> 16) % 250) as u8,
        51000 + ((draw >> 24) % 1000) as u16
    )
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    call_from(state, request, &test_peer()).await
}

/// `call`, from a named peer address. The brute-force walk passes its own so that its whole
/// sequence shares ONE counter — the thing being measured.
async fn call_from(state: &AppState, request: Request<Body>, peer: &str) -> TestResponse {
    let (mut parts, body) = request.into_parts();
    if let Ok(address) = peer.parse::<std::net::SocketAddr>() {
        parts
            .extensions
            .insert(axum::extract::ConnectInfo(address));
    }
    let response = routes::router(state.clone())
        .oneshot(Request::from_parts(parts, body))
        .await
        .expect("router must answer");

    let status = response.status();
    // ALL of them, joined. `Headers::get` returns the first `Set-Cookie` and sign-in sends
    // TWO — the session and the CSRF token — so reading one was reading the session and
    // concluding, correctly and wrongly, that the platform had issued no CSRF cookie.
    let set_cookie = {
        let values: Vec<String> = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_owned)
            .collect();
        (!values.is_empty()).then(|| values.join("; "))
    };
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("body must be JSON")
    };

    TestResponse {
        status,
        set_cookie,
        body,
    }
}

fn post_login(email: &str, password: &str) -> Request<Body> {
    let payload = json!({ "email": email, "password": password }).to_string();
    Request::builder()
        .method("POST")
        .uri("/api/v1/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(payload))
        .expect("request must build")
}

/// Sign out.
///
/// The CSRF token goes in the header AND the cookie: the double-submit check reads the header
/// and compares it against the cookie, so a cookie alone is the ambient-authority case and is
/// refused. This helper sent only the session cookie, which made `sign_in_me_and_sign_out_round_trip`
/// answer `403` where it expected `204` — and because that walk was the sign-in round trip, the
/// session-revocation half of it had been unproven for as long as the CSRF layer has existed.
/// Every other suite in this directory (`backups.rs`, `csrf.rs`, `media.rs`) has carried the
/// header all along, which is why this one was the odd case and not the rule.
fn post_logout(cookie: Option<&str>, csrf: Option<&str>) -> Request<Body> {
    let builder = Request::builder().method("POST").uri("/api/v1/auth/logout");
    let builder = match cookie {
        Some(token) => match csrf {
            Some(csrf) => builder
                .header(
                    header::COOKIE,
                    format!("omnion_session={token}; omnion_csrf={csrf}"),
                )
                .header("x-omnion-csrf", csrf),
            None => builder.header(header::COOKIE, format!("omnion_session={token}")),
        },
        None => builder,
    };
    builder.body(Body::empty()).expect("request must build")
}

fn get_me(cookie: Option<&str>) -> Request<Body> {
    let builder = Request::builder().method("GET").uri("/api/v1/me");
    let builder = match cookie {
        Some(token) => builder.header(header::COOKIE, format!("omnion_session={token}")),
        None => builder,
    };
    builder.body(Body::empty()).expect("request must build")
}

/// The session token a `Set-Cookie` header carries (`name=value` up to the first `;`).
/// Value of one named cookie out of the response's `Set-Cookie` header.
///
/// Sign-in issues BOTH `omnion_session` and `omnion_csrf`, joined into the single header the
/// test response keeps, so reading one by position is fragile; reading by name is the only
/// version of this that survives the platform adding a third cookie.
fn cookie_value(response: &TestResponse, name: &str) -> Option<String> {
    let header = response.set_cookie.as_deref()?;
    for part in header.split(';') {
        let part = part.trim();
        if let Some((key, value)) = part.split_once('=') {
            if key.trim() == name && !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

fn token_of(response: &TestResponse) -> String {
    let cookie = response
        .set_cookie
        .as_deref()
        .expect("the response must set a cookie");
    let pair = cookie.split(';').next().expect("cookie has a value");
    pair.split_once('=')
        .expect("cookie is name=value")
        .1
        .to_owned()
}

/// Object store of the test state.
///
/// These suites never touch the object store — that is the media suite's job — so the default
/// development configuration is enough: it opens without contacting anything.
fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

/// Connect to the compose PostgreSQL; `None` means the stack is not running.
async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&config.database).await {
        Ok(db) => Some(db),
        Err(err) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({err}) — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
            );
            None
        }
    }
}

/// A state whose database has all migrations applied.
/// A CSRF secret for this suite.
///
/// Sign-in issues no `omnion_csrf` cookie without one (`csrf_cookie_for` returns `None` rather
/// than an empty token, by design), and a cookie-authenticated POST with no token is refused
/// `403`. This file's sign-out helper sent only the session cookie, so
/// `sign_in_me_and_sign_out_round_trip` had been failing on that `403` for as long as the CSRF
/// layer existed — in CI too, since the workflow sets no secret either. The secret is set on the
/// suite's OWN state rather than in the environment, exactly as `tests/csrf.rs` does it, so a
/// developer's shell cannot change what this file proves.
const CSRF_SECRET: &str = "auth-suite-csrf-secret";

async fn live_state() -> Option<(AppState, Db)> {
    let mut config = Config::from_env().expect("environment must be valid");
    config.csrf = CsrfSecret::new(Some(CSRF_SECRET.to_owned()));
    let config = config;
    let db = live_db(&config).await?;
    db.migrate().await.expect("migrations must apply");

    let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
    let state = AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config,
        db.clone(),
        redis,
        test_storage(),
    );
    Some((state, db))
}

/// Create an account with a unique address so parallel runs cannot collide.
async fn test_user(db: &Db) -> (Uuid, String) {
    let email = format!("flow-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Integration Test".to_owned(),
            organization_id: None,
        },
    )
    .await
    .expect("creating a test account must succeed");
    (user.id, email)
}

async fn remove_user(db: &Db, id: Uuid) {
    sqlx::query("delete from users where id = $1")
        .bind(id)
        .execute(db.pool())
        .await
        .expect("cleanup must run");
}

#[tokio::test]
async fn sign_in_me_and_sign_out_round_trip() {
    let Some((state, db)) = live_state().await else {
        return;
    };
    let (user_id, email) = test_user(&db).await;

    // Sign in.
    let login = call(&state, post_login(&email, PASSWORD)).await;
    assert_eq!(login.status, StatusCode::OK, "login body: {}", login.body);
    assert_eq!(login.body["user"]["email"], email);
    assert_eq!(login.body["user"]["status"], "active");

    let cookie = login
        .set_cookie
        .clone()
        .expect("login must set the session cookie");
    assert!(cookie.contains("HttpOnly"), "cookie: {cookie}");
    assert!(cookie.contains("SameSite=Lax"), "cookie: {cookie}");
    let token = token_of(&login);

    // The database stores the hash of the token, never the token itself.
    let stored_hash: Option<String> = sqlx::query_scalar(
        "select token_hash from sessions where user_id = $1 and revoked_at is null",
    )
    .bind(user_id)
    .fetch_optional(db.pool())
    .await
    .expect("session lookup must run");
    let stored_hash = stored_hash.expect("a live session must exist");
    assert_eq!(stored_hash, sessions::hash_token(&token));
    assert_ne!(stored_hash, token, "the raw token must never be stored");
    assert!(stored_hash.len() == 64);

    // `/me` resolves the session and records activity.
    let me = call(&state, get_me(Some(&token))).await;
    assert_eq!(me.status, StatusCode::OK, "me body: {}", me.body);
    assert_eq!(me.body["user"]["email"], email);
    assert!(me.body["user"].get("password_hash").is_none());

    let last_seen: Option<Option<OffsetDateTime>> =
        sqlx::query_scalar("select last_seen_at from sessions where token_hash = $1")
            .bind(&stored_hash)
            .fetch_optional(db.pool())
            .await
            .expect("session lookup must run");
    assert!(
        last_seen.flatten().is_some(),
        "`/me` must record session activity"
    );

    // Missing or unknown sessions are rejected.
    let anonymous = call(&state, get_me(None)).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    assert_eq!(anonymous.body["error"]["code"], "unauthenticated");

    let bogus_token = "a".repeat(64);
    let bogus = call(&state, get_me(Some(&bogus_token))).await;
    assert_eq!(bogus.status, StatusCode::UNAUTHORIZED);
    assert_eq!(bogus.body["error"]["code"], "invalid_session");

    // Sign out clears the cookie and revokes the session. It is a cookie-authenticated POST, so
    // it carries the CSRF token that sign-in issued alongside the session.
    let csrf = cookie_value(&login, "omnion_csrf").expect(
        "sign-in must issue an omnion_csrf cookie, or every cookie-authenticated write is refused",
    );
    let logout = call(&state, post_logout(Some(&token), Some(&csrf))).await;
    assert_eq!(logout.status, StatusCode::NO_CONTENT);
    let cleared = logout
        .set_cookie
        .clone()
        .expect("logout must clear the cookie");
    assert!(cleared.contains("Max-Age=0"), "cookie: {cleared}");

    let after_logout = call(&state, get_me(Some(&token))).await;
    assert_eq!(
        after_logout.status,
        StatusCode::UNAUTHORIZED,
        "the revoked session must not resolve"
    );

    let revoked: Option<Option<OffsetDateTime>> =
        sqlx::query_scalar("select revoked_at from sessions where token_hash = $1")
            .bind(&stored_hash)
            .fetch_optional(db.pool())
            .await
            .expect("session lookup must run");
    assert!(
        revoked.expect("session row must exist").is_some(),
        "logout must set revoked_at"
    );

    remove_user(&db, user_id).await;
}

#[tokio::test]
async fn wrong_password_and_unknown_address_get_the_same_401() {
    let Some((state, db)) = live_state().await else {
        return;
    };
    let (user_id, email) = test_user(&db).await;

    let wrong = call(&state, post_login(&email, "definitely-not-the-password")).await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.body["error"]["code"], "invalid_credentials");
    assert!(
        wrong.set_cookie.is_none(),
        "a failed sign-in must not start a session"
    );

    let unknown_email = format!("missing-{}@omnion.test", Uuid::new_v4().simple());
    let unknown = call(&state, post_login(&unknown_email, PASSWORD)).await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);
    assert_eq!(unknown.body["error"]["code"], "invalid_credentials");

    let sessions: i64 = sqlx::query_scalar("select count(*) from sessions where user_id = $1")
        .bind(user_id)
        .fetch_one(db.pool())
        .await
        .expect("count must run");
    assert_eq!(sessions, 0, "failed sign-ins must not create sessions");

    remove_user(&db, user_id).await;
}

/// The account lockout must actually lock an account.
///
/// This walk exists because the feature shipped looking complete and never firing. The
/// per-address refusal and the per-account lockout were compared against **the same number**,
/// so from any one address the address rule fired on the attempt that would have incremented
/// the account counter. Every sign-in answered `403 address_blocked`, `users.failed_sign_in_count`
/// stayed at 0, and the "currently locked accounts" table on `/security/sign-in-protection` could
/// never have a row in it. Nothing above this test could see that: the account check ran, the
/// password check ran, and the answer the caller got was a refusal either way.
///
/// So the assertion is the whole difference between the two features: the account must reach
/// `locked_until` while the address is still being allowed, and a CORRECT password afterwards
/// must still be refused — otherwise the lock is a label rather than a lock.
#[tokio::test]
async fn repeated_wrong_passwords_lock_the_account_and_not_only_the_address() {
    let Some((state, db)) = live_state().await else {
        return;
    };
    let (user_id, email) = test_user(&db).await;

    // This walk's OWN address. Every walk in this file shares 127.0.0.1, so a brute-force walk
    // would otherwise inherit another walk's failure count and be refused by a rule it never
    // triggered — the same shared-table mistake as an unscoped `select count(*)`, one layer up.
    let peer = test_peer();
    let post_from_here = |password: &str| {
        let (mut parts, body) = post_login(&email, password).into_parts();
        if let Ok(address) = peer.parse::<std::net::SocketAddr>() {
            parts.extensions.insert(axum::extract::ConnectInfo(address));
        }
        Request::from_parts(parts, body)
    };

    // The account threshold for an organization-less account is the table default (10). Walk to
    // it one attempt at a time and stop as soon as the account is locked, so the test states the
    // real behaviour rather than a guess at how many attempts it takes.
    let mut locked_at = None;
    // The `sign_in` limiter counts per address with a ceiling of 10, and the account threshold
    // is also 10 — so whichever layer reaches its number first answers every later attempt and
    // the other never gets to speak. The limiter's counter is cleared before each attempt, which
    // leaves the ACCOUNT layer as the only thing deciding this walk's outcome, and its own
    // assertions below then say which layer spoke.
    let sign_in_policy = omnion_security::RatePolicy::defaults()
        .into_iter()
        .find(|policy| policy.scope == "sign_in")
        .expect("the sign_in policy must exist in the defaults");
    let limiter_client = omnion_security::ClientId {
        user_id: None,
        ip: peer.parse::<std::net::SocketAddr>().ok().map(|a| a.ip()),
    };
    // The state's OWN Redis handle: building a second one per attempt opened a fresh connection
    // to a shared server on every iteration and timed out, which is a harness fault that reads
    // as a product fault. The handle connects LAZILY behind a mutex, so it is warmed once here —
    // under a load of 80 on this box the first connect is the one that loses the race, and it is
    // not what this walk is measuring.
    let redis = state.redis().clone();
    redis
        .connection()
        .await
        .expect("the shared Redis must answer before this walk starts counting");
    for attempt in 1..=40 {
        // A clear that fails is tolerated, not fatal: the assertion below is about the ACCOUNT
        // counter, and a limiter that failed to be cleared only means this attempt may be
        // answered `429` instead of `401` — which the walk already tolerates. Turning a shared
        // Redis hiccup into a failed lockout test would be reporting the harness, not the product.
        let _ = omnion_security::forget(
            &redis,
            &sign_in_policy,
            &limiter_client,
            time::OffsetDateTime::now_utc().unix_timestamp(),
        )
        .await;
        let response = call(&state, post_from_here("definitely-not-the-password")).await;
        let failures: i32 = sqlx::query_scalar(
            "select failed_sign_in_count from users where id = $1",
        )
        .bind(user_id)
        .fetch_one(db.pool())
        .await
        .expect("the failure counter must be readable");

        if response.body["error"]["code"] == "account_locked" {
            locked_at = Some(attempt);
            assert_eq!(
                response.status,
                StatusCode::FORBIDDEN,
                "a locked account answers 403, not 401: {}",
                response.body
            );
            break;
        }
        // The limiter may answer instead, if its clear lost a race; the COUNTER assertion below
        // is what this walk is actually about, and it holds either way.
        if response.body["error"]["code"] == "rate_limited" {
            continue;
        }
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "attempt {attempt} must answer 401 until the account locks, got {}",
            response.body
        );
        assert!(
            failures >= attempt as i32,
            "every wrong password increments the account counter; after {attempt} it read {failures}"
        );
    }

    let (failures, locked_until): (i32, Option<OffsetDateTime>) = sqlx::query_as(
        "select failed_sign_in_count, locked_until from users where id = $1",
    )
    .bind(user_id)
    .fetch_one(db.pool())
    .await
    .expect("the account row must be readable");

    assert!(
        locked_at.is_some(),
        "no amount of wrong passwords locked the account: the counter reached {failures} and \
         locked_until is {locked_until:?}. The address rule is refusing before the account \
         counter can reach its own threshold."
    );
    assert!(
        locked_until.is_some_and(|until| until > OffsetDateTime::now_utc()),
        "the lock must be in the future, not a timestamp in the past"
    );

    // A lock that the correct password walks straight through is a label, not a lock.
    //
    // The limiter and the lockout are two independent layers and by now BOTH have refused this
    // walk: the `sign_in` ceiling is 10 per window and the account threshold is 10 attempts, so
    // the correct password is answered `429 rate_limited` before the account is ever consulted.
    // That is the limiter working, not the lock failing -- but asserting on it here would test
    // the wrong layer and pass for the wrong reason, so the counter is cleared first and the
    // lockout is asked on its own.
    let _: bool = sqlx::query_scalar(
        "update users set failed_sign_in_count = 0 where id = $1 returning true",
    )
    .bind(user_id)
    .fetch_one(db.pool())
    .await
    .expect("the counter must be resettable");
    omnion_identity::signin::clear_address_failures(db.pool())
        .await
        .expect("the address attempt log must be clearable");
    // The limiter's counter lives in REDIS, so clearing the database does not touch it. Its own
    // `forget` deletes the exact key the middleware counted, which is the only way to ask the
    // lockout a question without the limiter answering it first. Reaching for `FLUSHDB` here
    // would also delete every other suite's counters in this shared Redis.
    let _ = omnion_security::forget(
        &redis,
        &sign_in_policy,
        &limiter_client,
        time::OffsetDateTime::now_utc().unix_timestamp(),
    )
    .await;

    let correct = call(&state, post_from_here(PASSWORD)).await;
    assert_eq!(
        correct.body["error"]["code"],
        "account_locked",
        "a locked account must refuse the CORRECT password too, got {}",
        correct.body
    );
    assert!(
        correct.set_cookie.is_none(),
        "a locked account must not start a session"
    );

    remove_user(&db, user_id).await;
}

/// The lockout emits `security.lockout.triggered`, and **only once per lock** (REQ-012).
///
/// Two claims are asserted here and they are different from each other, which is the point of
/// the walk:
///
/// 1. **The event fires.** The event catalogue carries `security.lockout.triggered` and, until
///    this commit, nothing emitted it — the `security.*` group of the `/webhooks` picker
///    advertised a name that would never reach a receiver. The walk reads the row back out of
///    the `events` table rather than trusting the response, because a sign-in that is *refused*
///    and a sign-in that is *recorded* are two different claims and only the second one is the
///    criterion.
/// 2. **It fires ONCE, not once per attempt.** The counter that applies the lock keeps running
///    while the lock is in force, so an emitter placed on "the account is locked" rather than on
///    "this attempt applied the lock" would fire on every subsequent guess — and an attacker
///    chooses how many guesses to make. That is the difference between an event that records
///    the account that got caught and a volume metric of somebody's patience.
///
/// The walk also checks the payload carries the three fields the catalogue declares as
/// required, and that it carries **neither** the attempted password nor the client address —
/// a brute-force attempt is precisely the payload that must not be copied to a third party.
#[tokio::test]
async fn a_lockout_emits_the_event_once_and_carries_no_attempted_secret() {
    let Some((state, db)) = live_state().await else {
        return;
    };
    // One organization, one endpoint subscribed to the security group, and an account that
    // BELONGS to that organization. The membership is the load-bearing part:
    // `store::enqueue_fanout` returns zero for an event with no organization, so an account
    // with `organization_id = null` — which is what every other fixture in this file has —
    // would produce an event that is written, appears in `/events` as real, and reaches nobody.
    // That is the exact shape of an emitter that forgot `.organization(...)`, and it is why the
    // account is created inside the organization rather than with the shared helper.
    let organization_id: Uuid = sqlx::query_scalar(
        "insert into organizations (name, slug) values ($1, $2) returning id",
    )
    .bind("Lockout walk")
    .bind(format!("lockout-walk-{}", Uuid::new_v4().simple()))
    .fetch_one(db.pool())
    .await
    .expect("the organization must be created");

    let email = format!("lockout-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Lockout Walk".to_owned(),
            organization_id: Some(organization_id),
        },
    )
    .await
    .expect("the account must be created");
    let user_id = user.id;
    let endpoint_id: Uuid = sqlx::query_scalar(
        "insert into webhook_endpoints \
            (organization_id, name, url, secret, events, enabled) \
         values ($1, $2, $3, $4, $5, true) returning id",
    )
    .bind(organization_id)
    .bind("Lockout subscriber")
    .bind("https://receiver.invalid/security")
    .bind("lockout-walk-secret-value")
    .bind(vec!["security.*".to_owned()])
    .fetch_one(db.pool())
    .await
    .expect("the endpoint must be connected");

    // This walk's own address and its own limiter counter, for the reason the brute-force walk
    // above spells out: a shared loopback address spends another walk's budget.
    let peer = test_peer();
    let redis = state.redis().clone();
    redis
        .connection()
        .await
        .expect("the shared Redis must answer before this walk starts guessing");
    let sign_in_policy = omnion_security::RatePolicy::defaults()
        .into_iter()
        .find(|policy| policy.scope == "sign_in")
        .expect("the sign_in policy must exist in the defaults");
    let limiter_client = omnion_security::ClientId {
        user_id: None,
        ip: peer.parse::<std::net::SocketAddr>().ok().map(|a| a.ip()),
    };

    let count_events = || {
        let db = db.clone();
        async move {
            let count: i64 = sqlx::query_scalar(
                "select count(*) from events where name = 'security.lockout.triggered'",
            )
            .fetch_one(db.pool())
            .await
            .expect("the events table must be readable");
            count
        }
    };

    let before: i64 = count_events().await;

    // Walk to the lock. The account is in an organization, so the threshold is that
    // organization's `security_policies` row (the column default, 10) — the walk stops the
    // moment the account locks rather than guessing the number.
    let mut locked_at = None;
    for attempt in 1..=40 {
        let _ = omnion_security::forget(
            &redis,
            &sign_in_policy,
            &limiter_client,
            OffsetDateTime::now_utc().unix_timestamp(),
        )
        .await;
        let (mut parts, body) = post_login(&email, "definitely-not-the-password").into_parts();
        if let Ok(address) = peer.parse::<std::net::SocketAddr>() {
            parts
                .extensions
                .insert(axum::extract::ConnectInfo(address));
        }
        let response = call_from(
            &state,
            Request::from_parts(parts, body),
            &peer,
        )
        .await;
        if response.body["error"]["code"] == "account_locked" {
            locked_at = Some(attempt);
            break;
        }
    }
    assert!(
        locked_at.is_some(),
        "no amount of wrong passwords locked the account; the lockout never fired"
    );

    let after: i64 = count_events().await;
    assert_eq!(
        after - before,
        1,
        "the lock that just happened emitted {} events; it must emit exactly one",
        after - before
    );

    // The payload is read back out of the row, not out of the response: the response is a
    // refusal and says nothing about what was recorded.
    let (payload, organization_of_event): (Value, Option<Uuid>) = sqlx::query_as(
        "select payload, organization_id from events \
         where name = 'security.lockout.triggered' \
         order by created_at desc, id desc limit 1",
    )
    .fetch_one(db.pool())
    .await
    .expect("the emitted event must be readable");

    // The lock is what an operator needs to find, and the threshold that caused it is the
    // number they will immediately want to argue about.
    assert_eq!(
        payload["user_id"].as_str(),
        Some(user_id.to_string().as_str()),
        "the event must name the account that was locked: {payload}"
    );
    assert!(
        payload["attempts"].as_i64().is_some_and(|n| n >= 1),
        "the event must carry the threshold that fired: {payload}"
    );
    assert!(
        payload["lockout_minutes"].as_i64().is_some_and(|n| n >= 1),
        "the event must carry how long the lock lasts; a lockout with no duration in the \
         payload cannot be scheduled against: {payload}"
    );

    // Neither of these is a credential, but a brute-force attempt is the one payload nobody
    // should be copying to a third-party receiver, and a payload field is the easiest place for
    // it to start. The attempted password never reaches the row today; this asserts it.
    let text = payload.to_string();
    assert!(
        !text.contains("definitely-not-the-password"),
        "the attempted password must never reach the event bus: {text}"
    );
    assert!(
        !text.contains(&peer),
        "the client address must never reach the event bus: {text}"
    );

    // Now the second claim: further guesses against the ALREADY locked account must not add
    // another event. Without the `newly_locked` distinction this count is the number of times
    // the attacker chose to try again.
    for _ in 0..3 {
        let _ = omnion_security::forget(
            &redis,
            &sign_in_policy,
            &limiter_client,
            OffsetDateTime::now_utc().unix_timestamp(),
        )
        .await;
        let (mut parts, body) = post_login(&email, "definitely-not-the-password").into_parts();
        if let Ok(address) = peer.parse::<std::net::SocketAddr>() {
            parts
                .extensions
                .insert(axum::extract::ConnectInfo(address));
        }
        let response = call_from(&state, Request::from_parts(parts, body), &peer).await;
        assert_eq!(
            response.body["error"]["code"], "account_locked",
            "the account is locked, so every further guess is refused as a lockout"
        );
    }
    assert_eq!(
        count_events().await - before,
        1,
        "three more guesses against an already-locked account added events; the event records \
         that an account was caught, and an attacker chooses how many guesses to make"
    );

    // The event is attributed to the account's organization. This is the field that decides
    // whether the row below can exist at all, so it is asserted from the stored row rather than
    // trusted from the emitter.
    assert_eq!(
        organization_of_event,
        Some(organization_id),
        "the event must belong to the account's organization; without one it fans out to nobody"
    );

    // And the fan-out actually happened: an event nobody is subscribed to is not delivered.
    let queued: i64 = sqlx::query_scalar(
        "select count(*) from webhook_deliveries where endpoint_id = $1",
    )
    .bind(endpoint_id)
    .fetch_one(db.pool())
    .await
    .expect("the delivery queue must be readable");
    assert_eq!(
        queued, 1,
        "the lockout event queued {queued} deliveries for its one subscriber; it must queue \
         exactly the one"
    );

    remove_user(&db, user_id).await;
    sqlx::query("delete from webhook_endpoints where id = $1")
        .bind(endpoint_id)
        .execute(db.pool())
        .await
        .expect("the endpoint cleanup must run");
    sqlx::query("delete from organizations where id = $1")
        .bind(organization_id)
        .execute(db.pool())
        .await
        .expect("the organization cleanup must run");
}

#[tokio::test]
async fn disabled_accounts_cannot_sign_in() {
    let Some((state, db)) = live_state().await else {
        return;
    };
    let (user_id, email) = test_user(&db).await;

    sqlx::query("update users set status = 'disabled' where id = $1")
        .bind(user_id)
        .execute(db.pool())
        .await
        .expect("the status update must run");

    let login = call(&state, post_login(&email, PASSWORD)).await;
    assert_eq!(login.status, StatusCode::FORBIDDEN);
    assert_eq!(login.body["error"]["code"], "account_disabled");
    assert!(login.set_cookie.is_none());

    remove_user(&db, user_id).await;
}

#[tokio::test]
async fn email_addresses_are_case_insensitive_accounts() {
    let Some((_state, db)) = live_state().await else {
        return;
    };
    let email = format!("Case-{}@Omnion.Test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Case Test".to_owned(),
            organization_id: None,
        },
    )
    .await
    .expect("the first account must be created");
    assert_eq!(
        user.email,
        email.to_lowercase(),
        "addresses are stored lower"
    );

    let duplicate = users::create_user(
        db.pool(),
        NewUser {
            email: email.to_lowercase(),
            password: PASSWORD.to_owned(),
            display_name: "Duplicate".to_owned(),
            organization_id: None,
        },
    )
    .await
    .expect_err("the same address in another case must be rejected");
    assert!(matches!(duplicate, IdentityError::EmailTaken));

    remove_user(&db, user.id).await;
}

#[tokio::test]
async fn bootstrap_creates_the_first_administrator_on_a_fresh_database() {
    let config = Config::from_env().expect("environment must be valid");
    // Skip the suite when the stack is not running.
    let Some(_reachable) = live_db(&config).await else {
        return;
    };

    let database = format!("omnion_bootstrap_{}", Uuid::new_v4().simple());
    let maintenance_db = Db::connect(&DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 1,
    })
    .await
    .expect("the maintenance connection must work");
    sqlx::query(&format!("create database \"{database}\""))
        .execute(maintenance_db.pool())
        .await
        .expect("the temporary database must be created");

    let fresh = Db::connect(&DatabaseConfig {
        url: swap_database(&config.database.url, &database),
        max_connections: 2,
    })
    .await
    .expect("the fresh database must connect");
    fresh
        .migrate()
        .await
        .expect("migrations must apply on a fresh database");

    // A fresh install has no accounts until the bootstrap runs.
    assert!(!users::has_any(fresh.pool()).await.expect("count must run"));

    let outcome = users::bootstrap_first_admin(fresh.pool(), "Admin@Omnion.Test", PASSWORD)
        .await
        .expect("the bootstrap must succeed");
    let BootstrapOutcome::Created { user_id, email } = outcome else {
        panic!("expected the first administrator to be created, got {outcome:?}");
    };
    assert_eq!(email, "admin@omnion.test", "the address is normalized");
    assert_eq!(
        users::count_users(fresh.pool())
            .await
            .expect("count must run"),
        1
    );

    let hash: String = sqlx::query_scalar("select password_hash from users where id = $1")
        .bind(user_id)
        .fetch_one(fresh.pool())
        .await
        .expect("the hash must be readable");
    assert!(hash.starts_with("$argon2id$"), "hash: {hash}");
    assert_ne!(
        hash, PASSWORD,
        "the plaintext password must never be stored"
    );
    assert!(
        omnion_identity::verify_password(PASSWORD, hash)
            .await
            .expect("verification must run"),
        "the stored hash must verify the configured password"
    );

    // Booting again is a no-op: the database is not empty any more.
    let again = users::bootstrap_first_admin(fresh.pool(), "admin@omnion.test", PASSWORD)
        .await
        .expect("the second bootstrap must be a no-op");
    assert_eq!(again, BootstrapOutcome::SkippedExistingUsers);
    assert_eq!(
        users::count_users(fresh.pool())
            .await
            .expect("count must run"),
        1
    );

    fresh.pool().close().await;
    sqlx::query(&format!(
        "drop database if exists \"{database}\" with (force)"
    ))
    .execute(maintenance_db.pool())
    .await
    .expect("the temporary database must be removed");
}

/// Replace the database name in a PostgreSQL connection string.
fn swap_database(url: &str, database: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let prefix = base
        .rsplit_once('/')
        .expect("the URL must contain a database path")
        .0;
    match query {
        Some(query) => format!("{prefix}/{database}?{query}"),
        None => format!("{prefix}/{database}"),
    }
}

#[test]
fn swap_database_keeps_credentials_and_query() {
    assert_eq!(
        swap_database("postgres://omnion:omnion@127.0.0.1:5433/omnion", "postgres"),
        "postgres://omnion:omnion@127.0.0.1:5433/postgres"
    );
    assert_eq!(
        swap_database("postgres://user:pw@db:5432/omnion?sslmode=disable", "other"),
        "postgres://user:pw@db:5432/other?sslmode=disable"
    );
}
