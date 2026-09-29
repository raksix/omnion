//! The CSRF round trip, driven over HTTP (REQ-012, slice 2).
//!
//! **Why this suite exists at all.** The slice shipped a middleware that refuses a
//! cookie-authenticated mutation without a token, and no test ever drove a *real* mutation
//! through the *real* router — so the layer went in, the suite stayed green, and every save in
//! the panel answered `403 csrf_failed` with nothing the client could have sent. Nothing was
//! broken in any unit: the token was simply never issued. The class of bug is "a guard and the
//! thing it guards were written in different files and never met", so the only test that can
//! catch it is one that signs in, takes what the response set, and posts it back.
//!
//! Three things are proved here, in this order, because each is a different failure:
//!
//! 1. **Sign-in issues the token.** A response that sets a session cookie and no `omnion_csrf`
//!    cookie is the defect, and it is invisible without reading the headers.
//! 2. **A cookie-authenticated mutation without the token is refused, and with it succeeds.**
//!    The second half is the half that was never proved: a guard that refuses everything is
//!    indistinguishable from a working one until you show a request that gets through.
//! 3. **A bearer key is exempt**, because a machine credential is not ambient authority and
//!    breaking service accounts to defend against an attack they are not exposed to would be a
//!    self-inflicted outage.
//!
//! The suite skips itself, with a printed reason, when PostgreSQL is not reachable, like every
//! other integration walk in `apps/api/tests`.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, CsrfSecret};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::seed;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// The key the tokens are derived from in this suite. Fixed, not random: the derivation is the
/// crate's business and has its own tests, and a random key per run would only make a failure
/// harder to reproduce.
const CSRF_SECRET: &str = "csrf-integration-suite-key-material";

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    /// **Every** `Set-Cookie`, in order. A single-cookie accessor is what hid the defect: login
    /// set one cookie the whole time, and the test read it happily.
    set_cookies: Vec<String>,
    body: Value,
}

impl TestResponse {
    /// The value of the named cookie across every `Set-Cookie` header, or `None`.
    fn cookie(&self, name: &str) -> Option<String> {
        for header_value in &self.set_cookies {
            let pair = header_value.split(';').next().unwrap_or_default();
            if let Some((cookie_name, value)) = pair.split_once('=') {
                if cookie_name.trim() == name {
                    return Some(value.trim().to_owned());
                }
            }
        }
        None
    }
}

/// Drive the real router without a network socket.
async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
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
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    // Not every answer is JSON — a `204` carries none, and an empty `Vec` is the right reading of
    // that rather than a parse panic that would blame the wrong thing entirely.
    let body = match std::str::from_utf8(&bytes) {
        Ok(text) if text.trim().is_empty() => Value::Null,
        Ok(text) => serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned())),
        Err(_) => Value::Null,
    };

    TestResponse {
        status,
        set_cookies,
        body,
    }
}

/// Object store of the test state. Never contacted — that is the media suite's job.
fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

/// A state whose database has all migrations applied **and** whose CSRF secret is set.
async fn live_state() -> Option<(AppState, Db)> {
    let mut config = Config::from_env().expect("environment must be valid");
    config.csrf = CsrfSecret::new(Some(CSRF_SECRET.to_owned()));

    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(err) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({err}) — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
            );
            return None;
        }
    };
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
async fn test_user(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("csrf-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "CSRF Integration Test".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("creating a test account must succeed");
    (user.id, email)
}

/// An account that holds enough power to reach a real state-changing endpoint.
///
/// **Why a fixture and not a bare user.** The first draft of this suite signed in a fresh
/// account and posted to `PATCH /api/v1/me` — a route that only ever answered `GET`. The request
/// came back `405` and the test asserted `200`, so the suite failed for a reason that had nothing
/// to do with CSRF; the fix was to pick an endpoint that exists *and* an account allowed to
/// reach it. `PUT /api/v1/notifications/preferences` is that endpoint: it is behind
/// `notifications.manage`, it changes a row, and it is reachable by an owner of a throwaway
/// organization.
async fn owner_fixture(db: &Db) -> (Uuid, Uuid, String) {
    seed::ensure(db.pool())
        .await
        .expect("the IAM seed must run");

    let slug = format!("csrf-{}", Uuid::new_v4().simple());
    let organization_id: Uuid =
        sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
            .bind("CSRF Test Organization")
            .bind(&slug)
            .fetch_one(db.pool())
            .await
            .expect("the test organization must be created");

    let (owner_id, email) = test_user(db, Some(organization_id)).await;
    seed::bind_owner(db.pool(), owner_id)
        .await
        .expect("the owner binding must be created");
    (organization_id, owner_id, email)
}

async fn remove_user(db: &Db, id: Uuid) {
    sqlx::query("delete from role_bindings where user_id = $1")
        .bind(id)
        .execute(db.pool())
        .await
        .expect("binding cleanup must run");
    sqlx::query("delete from users where id = $1")
        .bind(id)
        .execute(db.pool())
        .await
        .expect("cleanup must run");
}

async fn remove_organization(db: &Db, id: Uuid) {
    sqlx::query("delete from organizations where id = $1")
        .bind(id)
        .execute(db.pool())
        .await
        .expect("organization cleanup must run");
}

/// Sign in and return the whole response, so a test can read every cookie it set.
async fn sign_in(state: &AppState, email: &str) -> TestResponse {
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "email": email, "password": PASSWORD }).to_string(),
        ))
        .expect("request must build");
    let response = call(state, request).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the sign-in must succeed: {}",
        response.body
    );
    response
        .cookie("omnion_session")
        .expect("sign-in must set the session cookie");
    response
}

/// Sign in, returning the session token and the CSRF token the response handed out.
async fn signed_in(state: &AppState, email: &str) -> (String, String) {
    let response = sign_in(state, email).await;
    let session = response
        .cookie("omnion_session")
        .expect("sign-in must set the session cookie");
    let token = response.cookie("omnion_csrf").expect(
        "sign-in must hand the browser a CSRF token; without one every cookie-authenticated \
         mutation is refused and the panel cannot save anything",
    );
    (session, token)
}

/// `PUT /api/v1/notifications/preferences` — a real mutation the owner can reach.
fn put_preferences(session: &str, csrf: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("PUT")
        .uri("/api/v1/notifications/preferences")
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::COOKIE,
            match csrf {
                Some(token) => format!("omnion_session={session}; omnion_csrf={token}"),
                None => format!("omnion_session={session}"),
            },
        );
    if let Some(token) = csrf {
        builder = builder.header("x-omnion-csrf", token);
    }
    builder
        .body(Body::from(
            json!({
                "cells": [{ "category": "security", "channel": "email", "enabled": true }],
                "settings": { "digest_cadence": "off" }
            })
            .to_string(),
        ))
        .expect("request must build")
}

/// The same request, authenticated by a machine key instead of a browser session.
fn put_preferences_with_bearer(session: &str) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri("/api/v1/notifications/preferences")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, format!("omnion_session={session}"))
        .header(
            header::AUTHORIZATION,
            "Bearer qa-machine-key-not-a-real-secret",
        )
        .body(Body::from(
            json!({
                "cells": [{ "category": "security", "channel": "email", "enabled": true }],
                "settings": { "digest_cadence": "off" }
            })
            .to_string(),
        ))
        .expect("request must build")
}

#[tokio::test]
async fn sign_in_issues_the_token_the_mutation_layer_asks_for() {
    let Some((state, db)) = live_state().await else {
        return;
    };
    let (_organization, owner_id, email) = owner_fixture(&db).await;
    let response = sign_in(&state, &email).await;

    // The token cookie must be *readable by script* — that is the whole mechanism, the browser
    // copies it into a header. HttpOnly here would make every save fail with a token nobody can
    // send, which is the same silent wall this suite was written to catch.
    let token_cookie = response
        .set_cookies
        .iter()
        .find(|value| value.starts_with("omnion_csrf="))
        .expect(
            "sign-in must hand the browser a CSRF token; without one every cookie-authenticated \
             mutation is refused and the panel cannot save anything",
        );
    assert!(
        !token_cookie.contains("HttpOnly"),
        "the token cookie has to be readable by the browser: {token_cookie}"
    );
    assert!(token_cookie.contains("SameSite=Strict"), "{token_cookie}");

    // The session cookie is *not* readable — a token that leaked without the session would be
    // the only thing standing between a dump and a write.
    let session_cookie = response
        .set_cookies
        .iter()
        .find(|value| value.starts_with("omnion_session="))
        .expect("sign-in must set the session cookie");
    assert!(
        session_cookie.contains("HttpOnly"),
        "the session cookie must stay HttpOnly: {session_cookie}"
    );

    let token = response
        .cookie("omnion_csrf")
        .expect("the token cookie is present");
    assert!(!token.is_empty(), "a token may not be empty");

    remove_user(&db, owner_id).await;
}

#[tokio::test]
async fn a_mutation_is_refused_without_the_token_and_allowed_with_it() {
    let Some((state, db)) = live_state().await else {
        return;
    };
    let (organization_id, owner_id, email) = owner_fixture(&db).await;
    let (session, token) = signed_in(&state, &email).await;

    // Without it: refused, and the code names CSRF rather than signing the operator out.
    let refused = call(&state, put_preferences(&session, None)).await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a cookie-authenticated mutation without a token must be refused: {}",
        refused.body
    );
    assert_eq!(refused.body["error"]["code"], "csrf_failed");

    // With it: allowed. A guard that refuses everything passes the assertion above, so this half
    // is the one that proves the control is a control and not a wall.
    let allowed = call(&state, put_preferences(&session, Some(&token))).await;
    assert_eq!(
        allowed.status,
        StatusCode::OK,
        "the token sign-in issued must be the token the layer accepts: {}",
        allowed.body
    );

    // And the change really landed — a 200 on a request that did nothing would be the same
    // defect wearing a different hat, so read the row back out of the database.
    let stored: bool = sqlx::query_scalar(
        "select exists (select 1 from notification_preferences where user_id = $1)",
    )
    .bind(owner_id)
    .fetch_one(db.pool())
    .await
    .expect("the preference row must be readable");
    assert!(
        stored,
        "an accepted mutation has to change the row it names"
    );

    remove_user(&db, owner_id).await;
    remove_organization(&db, organization_id).await;
}

#[tokio::test]
async fn a_token_from_another_session_is_refused() {
    let Some((state, db)) = live_state().await else {
        return;
    };
    let (first_org, first_id, first_email) = owner_fixture(&db).await;
    let (second_org, second_id, second_email) = owner_fixture(&db).await;
    let (first_session, _first_token) = signed_in(&state, &first_email).await;
    let (_second_session, second_token) = signed_in(&state, &second_email).await;

    // The first session's cookie with the second session's token: the exact shape of a token
    // lifted from one browser and replayed into another. Deriving from the session id is what
    // makes this a mismatch rather than a pass.
    let refused = call(&state, put_preferences(&first_session, Some(&second_token))).await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a token must not travel between sessions: {}",
        refused.body
    );
    assert_eq!(refused.body["error"]["code"], "csrf_failed");

    remove_user(&db, first_id).await;
    remove_user(&db, second_id).await;
    remove_organization(&db, first_org).await;
    remove_organization(&db, second_org).await;
}

#[tokio::test]
async fn a_bearer_machine_key_is_not_asked_for_a_token() {
    let Some((state, db)) = live_state().await else {
        return;
    };
    let (organization_id, owner_id, email) = owner_fixture(&db).await;
    let (session, _token) = signed_in(&state, &email).await;

    // A bearer credential is not ambient authority — the browser does not attach it by itself —
    // so requiring a token here would break every service account to defend against an attack
    // they are not exposed to. The request still carries the session cookie, so this proves the
    // exemption is about *how* the caller authenticated, not about the cookie being absent.
    let response = call(&state, put_preferences_with_bearer(&session)).await;

    // The point is the code it is *not*. A key this suite made up resolves to nothing, so the
    // answer is the guard's — but it must never be the CSRF layer's, because a service account
    // would then be broken by a control it was never exposed to.
    assert_ne!(
        response.body["error"]["code"],
        Value::String("csrf_failed".to_owned()),
        "a bearer key must not be asked for a CSRF token: {}",
        response.body
    );

    remove_user(&db, owner_id).await;
    remove_organization(&db, organization_id).await;
}
