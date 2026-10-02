//! The caller's own session is not revocable through the admin revoke endpoint.
//!
//! # Why this lives in its own file
//!
//! The claim could not be added to `tests/iam.rs` and have it mean anything. That suite signs in
//! and sends a bare `Cookie` header, so a test process — which has no `OMNION_CSRF_SECRET` — has
//! every cookie-authenticated write refused with `csrf_unavailable` before it is sent. **That
//! refusal is the product working**: the double-submit check has nothing to compare against and
//! says so by name. The suite's helper predates that tick, so it asserts a deployment that cannot
//! exist. That was measured rather than assumed — `tests/iam.rs` fails identically with and
//! without a change to it, at a policy save from 2026-09, which is why this is a separate file
//! rather than a new block in a red one.
//!
//! So this suite uses `support::walk_auth`, the shared sign-in helper built for exactly this
//! (keep every `Set-Cookie`, echo the token in the header) and gives its own state a CSRF secret
//! the way `tests/media_shares.rs` does. The claim is then measured somewhere it can be measured.
//!
//! # What is being prevented
//!
//! `DELETE /iam/sessions/{id}` stamps the row before the response is written. Handed the caller's
//! own `session_id`, the sign-in ends mid-request: the response goes out and every later call is
//! `401 invalid_session`. The QA walk did exactly that — `revokeButtons.first()` is the browser's
//! own session as often as any other — and then measured the login screen on sixteen routes and
//! filed every one as a defect on a screen it had never opened.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::seed;
use omnion_storage::Storage;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

mod support;

/// What one request answered.
struct TestResponse {
    status: StatusCode,
    set_cookies: Vec<String>,
    body: Value,
}

/// The peer address this run signs in from, unique per process.
///
/// The limiter counts sign-ins **by address**, so every suite on the box shares one `sign_in`
/// budget of 10 per 300 seconds. Sixteen sign-ins from other writers and this run is refused with
/// `429 rate_limited` before it reaches a single assertion — a failure that says the box is busy,
/// not that the guard is wrong. The address goes into the request extensions, which is where
/// `into_make_service_with_connect_info` puts it in production; setting `X-Forwarded-For` instead
/// would be proving the proxy path rather than the one the middleware reads.
fn peer() -> String {
    format!("203.0.113.{}", Uuid::new_v4().as_u128() % 254 + 1)
}

/// Drive the real router without a network socket.
async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let mut request = request;
    let address: std::net::SocketAddr = format!("{}:40000", peer())
        .parse()
        .expect("a peer address with a port parses");
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(address));

    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");

    let status = response.status();
    let set_cookies: Vec<String> = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_owned)
        .collect();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let raw = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes()
        .to_vec();
    let body = match content_type.as_deref() {
        Some(value) if value.starts_with("application/json") && !raw.is_empty() => {
            serde_json::from_slice(&raw).unwrap_or(Value::Null)
        }
        _ => Value::Null,
    };

    TestResponse {
        status,
        set_cookies,
        body,
    }
}

/// Build a JSON request; `token` is the packed credential (session **and** CSRF).
fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match token {
        Some(token) => crate::support::walk_auth::apply_credential(token, builder),
        None => builder,
    };
    match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .expect("request must build"),
        None => builder.body(Body::empty()).expect("request must build"),
    }
}

/// A scratch database and the state that talks to it.
///
/// The suite cannot use the configured database. That one is shared with every other writer on
/// the box, and it carries whichever migrations *their* branches applied: on the tick this was
/// written, it sat at 19 while this branch's newest file is `0018_webauthn.sql`, so `db.migrate()`
/// answered `VersionMissing(19)` and the test failed **before its first assertion**. A red that
/// says the box is mid-flight, not that the guard is wrong — and a guard test that cannot be
/// shown to fail cannot be shown to hold either.
struct Harness {
    state: AppState,
    db: Db,
    maintenance: Db,
    database: String,
}

impl Harness {
    async fn fresh() -> Option<Self> {
        let mut config = Config::from_env().ok()?;
        support::walk_auth::with_csrf_secret(&mut config);
        let maintenance = match Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, "postgres"),
            max_connections: 1,
        })
        .await
        {
            Ok(db) => db,
            Err(error) => {
                eprintln!("SKIP: PostgreSQL is not reachable ({error})");
                return None;
            }
        };
        let database = format!("omnion_selfrevoke_{}", Uuid::new_v4().as_u128());
        if let Err(error) = sqlx::query(&format!(r#"create database "{database}""#))
            .execute(maintenance.pool())
            .await
        {
            eprintln!("SKIP: a scratch database could not be created ({error})");
            return None;
        }
        let db = Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, &database),
            max_connections: 4,
        })
        .await
        .expect("the fresh database must connect");
        db.migrate().await.expect("migrations must apply");

        let storage = Storage::from_env().expect("the storage configuration must be valid");
        let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
        let state = AppState::new(
            BuildInfo::new("omnion-api", "0.0.0-test"),
            config,
            db.clone(),
            redis,
            storage,
        );
        Some(Self {
            state,
            db,
            maintenance,
            database,
        })
    }

    async fn dispose(self) {
        self.db.pool().close().await;
        let database = self.database;
        sqlx::query(&format!(
            r#"drop database if exists "{database}" with (force)"#
        ))
        .execute(self.maintenance.pool())
        .await
        .expect("the temporary database must be removed");
    }
}

/// Point `url` at a different database on the same server, keeping any query string.
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

const PASSWORD: &str = "correct horse battery";

/// Sign in and pack the credential.
async fn sign_in(state: &AppState, email: &str) -> String {
    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/auth/login",
            None,
            Some(json!({ "email": email, "password": PASSWORD })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "login body: {}", response.body);
    crate::support::walk_auth::Session::from_set_cookies(&response.set_cookies).pack()
}

/// One account in one organization, owned.
async fn create_account(harness: &Harness) -> (Uuid, String, Uuid) {
    let organization_id: Uuid = sqlx::query_scalar(
        "insert into organizations (name, slug) values ($1, $2) returning id",
    )
    .bind("Self-Revoke Test Organization")
    .bind(format!("selfrevoke-{}", Uuid::new_v4().simple()))
    .fetch_one(harness.db.pool())
    .await
    .expect("the organization must be created");

    let email = format!("selfrevoke-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        harness.db.pool(),
        NewUser {
            organization_id: Some(organization_id),
            email: email.clone(),
            display_name: "Self Revoke".to_owned(),
            password: PASSWORD.to_owned(),
        },
    )
    .await
    .expect("the account must be created");
    seed::bind_owner(harness.db.pool(), user.id)
        .await
        .expect("the owner binding must be created");
    (user.id, email, organization_id)
}

#[tokio::test]
async fn the_session_you_are_signed_in_with_cannot_be_revoked() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    seed::ensure(harness.db.pool())
        .await
        .expect("the IAM seed must run");
    let (user_id, email, _organization_id) = create_account(&harness).await;
    let owner = sign_in(&harness.state, &email).await;

    // The list names the caller's own session, and — the row the defect turned on — still marks it
    // `revocable`, because "not revoked yet" is true of it. That is what the sessions screen
    // offered the button on.
    let listed = call(
        &harness.state,
        request(
            Method::GET,
            &format!("/api/v1/iam/sessions?user_id={user_id}"),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    let rows = listed.body["sessions"].as_array().expect("sessions array");
    let own = rows
        .iter()
        .find(|row| row["current"] == Value::Bool(true))
        .expect("the list names the caller's own session");
    assert_eq!(
        own["revocable"], Value::Bool(true),
        "this row is the one the screen used to offer Revoke on: {}",
        listed.body
    );
    let own_id = own["id"].as_str().expect("own session id").to_owned();

    // The refusal: 409, with the code a caller can branch on.
    let refused = call(
        &harness.state,
        request(
            Method::DELETE,
            &format!("/api/v1/iam/sessions/{own_id}"),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(
        refused.body["error"]["code"], "cannot_revoke_current_session",
        "{}",
        refused.body
    );

    // The sign-in that made the call still works — the entire point of the refusal.
    let me = call(
        &harness.state,
        request(Method::GET, "/api/v1/me", Some(&owner), None),
    )
    .await;
    assert_eq!(
        me.status,
        StatusCode::OK,
        "ending your own sign-in mid-request is the defect: {}",
        me.body
    );

    // And the row is unstamped in the DATABASE. A guard that answered 409 and still wrote would
    // pass all three assertions above, so this is the one that holds the line.
    let revoked_at: Option<time::OffsetDateTime> =
        sqlx::query_scalar("select revoked_at from sessions where id = $1")
            .bind(Uuid::parse_str(&own_id).expect("uuid"))
            .fetch_one(harness.db.pool())
            .await
            .expect("the session row must be readable");
    assert!(
        revoked_at.is_none(),
        "the refused call must not have stamped the row"
    );

    // The other half of the endpoint is untouched: a session of someone else still revokes,
    // otherwise the guard reads as "revoke is broken" rather than "your own is protected".
    let second = sign_in(&harness.state, &email).await;
    let listed = call(
        &harness.state,
        request(
            Method::GET,
            &format!("/api/v1/iam/sessions?user_id={user_id}"),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    let other = listed.body["sessions"]
        .as_array()
        .expect("sessions array")
        .iter()
        .find(|row| row["current"] == Value::Bool(false) && row["state"] == "live")
        .expect("a second live session of the same account")
        .clone();
    let other_id = other["id"].as_str().expect("session id").to_owned();
    let revoked = call(
        &harness.state,
        request(
            Method::DELETE,
            &format!("/api/v1/iam/sessions/{other_id}"),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.body);
    assert_eq!(revoked.body["state"], "revoked", "{}", revoked.body);
    let ended = call(
        &harness.state,
        request(Method::GET, "/api/v1/me", Some(&second), None),
    )
    .await;
    assert_eq!(
        ended.status,
        StatusCode::UNAUTHORIZED,
        "the revoked session must stop working on its next request"
    );

    // The scratch database is dropped whole, which is a stronger teardown than deleting rows one
    // table at a time: nothing this fixture wrote can outlive the test, and there is no row list
    // to fall behind when a later assertion adds another table.
    harness.dispose().await;
}
