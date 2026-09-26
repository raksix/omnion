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
use omnion_core::config::{Config, DatabaseConfig};
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
async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");

    let status = response.status();
    let set_cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
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

fn post_logout(cookie: Option<&str>) -> Request<Body> {
    let builder = Request::builder().method("POST").uri("/api/v1/auth/logout");
    let builder = match cookie {
        Some(token) => builder.header(header::COOKIE, format!("omnion_session={token}")),
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
async fn live_state() -> Option<(AppState, Db)> {
    let config = Config::from_env().expect("environment must be valid");
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

    // Sign out clears the cookie and revokes the session.
    let logout = call(&state, post_logout(Some(&token))).await;
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
