//! Permission-gate walks for the security centre (REQ-012).
//!
//! **What this file is for.** The REQ-012 criterion *"every endpoint enforces its catalogue
//! key; a forbidden call returns `403 permission_denied`"* carried a note that said the four
//! keys were in the catalogue and every route was behind a guard, and that "the 403 itself is
//! unproven until a pass calls an endpoint without the key". That is precisely the defect class
//! this REQ has hit six times already: a guard that is only ever satisfied is not a guard that
//! was checked. The security centre once sat behind `analytics.read` — a key named for a
//! different feature — and no walk noticed, because every walk signed in as an account holding
//! everything.
//!
//! So this suite signs in three different ways and calls every `/security` route from each:
//!
//! 1. **anonymous** — no session at all, which must be `401`, not a body of data;
//! 2. **a member with no security keys** — a real organization member, which must be `403`
//!    `permission_denied` naming the key that was missing;
//! 3. **a holder of exactly one key** — the case that exposes an inherited `route_layer`. An
//!    account holding `security.read` and nothing else must be refused the `security.scan` and
//!    `security.manage` routes. If a future edit nests the security router under a parent that
//!    declares a `route_layer`, or forgets a route's guard, this suite turns red on the route
//!    that regressed rather than on whichever account happens to be used in the admin UI.
//!
//! The third account is the whole point. Without it, a single extra key would satisfy every
//! route and the file would pass while the centre was open to somebody who may only look.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::sessions;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

mod support;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

// ---------------------------------------------------------------------------------------------
// The harness: a throwaway database with every migration applied
// ---------------------------------------------------------------------------------------------

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    body: Value,
    text: String,
}

/// A throwaway database and its router.
struct Harness {
    state: AppState,
    db: Db,
    maintenance: Db,
    database: String,
    csrf_secret: Vec<u8>,
}

impl Harness {
    /// Open a fresh database with every migration applied and the IAM seed loaded.
    async fn fresh() -> Option<Self> {
        let mut config = Config::from_env().expect("environment must be valid");
        // The CSRF secret, for the same reason `tests/events.rs` sets one: a cookie write with
        // no double-submit token is refused `403 csrf_failed` before the permission layer is
        // ever consulted, and a suite that measured the wrong refusal would look like a guard
        // that works.
        support::walk_auth::with_csrf_secret(&mut config);
        let csrf_secret = config.csrf.as_bytes().expect("the suite just set a secret").to_vec();
        live_db(&config).await?;

        let database = format!("omnion_security_{}", Uuid::new_v4().simple());
        let maintenance = Db::connect(&maintenance_config(&config))
            .await
            .expect("the maintenance connection must work");
        sqlx::query(&format!("create database \"{database}\""))
            .execute(maintenance.pool())
            .await
            .expect("the temporary database must be created");

        let db = Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, &database),
            max_connections: 4,
        })
        .await
        .expect("the fresh database must connect");
        db.migrate().await.expect("migrations must apply");
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
        let state = AppState::new(
            BuildInfo::new("omnion-api", "0.1.0-test"),
            config,
            db.clone(),
            redis,
            omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
                .expect("the default storage configuration is valid"),
        );

        Some(Self {
            state,
            db,
            maintenance,
            database,
            csrf_secret,
        })
    }

    /// Drive the router without a network socket.
    async fn call(&self, request: Request<Body>) -> TestResponse {
        let response = routes::router(self.state.clone())
            .oneshot(request)
            .await
            .expect("router must answer");

        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body must read")
            .to_bytes();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };

        TestResponse { status, body, text }
    }

    /// Drop the throwaway database.
    async fn dispose(self) {
        self.db.pool().close().await;
        let database = self.database;
        sqlx::query(&format!(
            "drop database if exists \"{database}\" with (force)"
        ))
        .execute(self.maintenance.pool())
        .await
        .expect("the temporary database must be removed");
    }
}

/// A GET request, optionally with a session cookie and its CSRF token.
fn get(uri: &str, token: Option<&str>) -> Request<Body> {
    request(Method::GET, uri, token, None)
}

/// A POST request carrying a JSON body.
fn post(uri: &str, body: Value, token: Option<&str>) -> Request<Body> {
    request(Method::POST, uri, token, Some(body))
}

/// A PUT request carrying a JSON body.
fn put(uri: &str, body: Value, token: Option<&str>) -> Request<Body> {
    request(Method::PUT, uri, token, Some(body))
}

/// Build a JSON request; `token` is the packed `session\x1fcsrf` credential.
///
/// Both halves are attached by [`support::walk_auth::apply_credential`], which is the only place
/// they are separated: setting the cookie alone is exactly the ambient-authority request the
/// double-submit check refuses, and a guard test that measured that refusal instead of the
/// permission decision would be green for the wrong reason.
fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match token {
        Some(credential) => support::walk_auth::apply_credential(credential, builder),
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

/// Connect to the compose PostgreSQL; `None` means the stack is not running.
async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&config.database).await {
        Ok(db) => Some(db),
        Err(err) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({err})");
            None
        }
    }
}

/// The connection the suite creates its throwaway database through.
fn maintenance_config(config: &Config) -> DatabaseConfig {
    DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 2,
    }
}

/// Point a connection string at a different database.
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

/// Create an account with a session and return `(user id, packed credential)`.
async fn account(harness: &Harness, organization_id: Option<Uuid>) -> (Uuid, String) {
    let user = users::create_user(
        harness.db.pool(),
        NewUser {
            email: format!("security-{}@omnion.test", Uuid::new_v4().simple()),
            password: PASSWORD.to_owned(),
            display_name: "Walk".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    let (session, token) = sessions::create_session(harness.db.pool(), user.id, None, None)
        .await
        .expect("the session must be created");
    let csrf = omnion_security::derive_csrf_token(&harness.csrf_secret, &session.id.to_string());
    (
        user.id,
        support::walk_auth::pack(&support::walk_auth::Session {
            session: token,
            csrf: Some(csrf),
        }),
    )
}

/// Bind a role with exactly these permission keys to one account, at organization scope.
async fn grant(harness: &Harness, user_id: Uuid, organization_id: Uuid, keys: &[&str]) -> Uuid {
    let role = role_store::create_role(
        harness.db.pool(),
        NewRole {
            organization_id,
            key: format!("security-walk-{}", Uuid::new_v4().simple()),
            name: "Security Walk".to_owned(),
            description: "The keys one walk needs".to_owned(),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the organization role must be created");

    let entries: Vec<RolePermissionInput> = keys
        .iter()
        .map(|key| RolePermissionInput {
            key: (*key).to_owned(),
            effect: Effect::Allow,
        })
        .collect();
    role_store::set_role_permissions(harness.db.pool(), role.id, &entries)
        .await
        .expect("the role permission set must be written");

    let binding = NewBinding {
        role_id: role.id,
        user_id,
        scope: Scope::Organization { organization_id },
        granted_by: None,
        expires_at: None,
    };
    bindings::validate(harness.db.pool(), &binding)
        .await
        .expect("the binding must validate");
    bindings::grant(harness.db.pool(), binding)
        .await
        .expect("the binding must be granted");

    role.id
}

/// Create an organization row with a unique, suite-scoped slug.
async fn create_organization_row(db: &Db, name: &str) -> Uuid {
    let slug = format!("security-walk-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(name)
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// A real loopback address for this walk, with a per-walk port **and IP**.
///
/// Not shared, and not absent. `ClientAddress` and the rate-limit middleware both read
/// `ConnectInfo<SocketAddr>` out of the request extensions, so a request driven straight through
/// `oneshot()` arrives with **no** address — which is not a neutral default: the per-address
/// refusal cannot run, so a brute-force walk silently skips the rule it is measuring.
///
/// **The IP is drawn too, not just the port, and that is the part that matters.** Varying the port
/// alone does NOT isolate a walk from its neighbours: the ACCOUNT rule this walk measures is
/// account-scoped, but the ADDRESS rule it must *not* trip reads `recent_failures_from_address`,
/// which keys on the IP alone and ignores the port entirely. Every walk in a file that varies only
/// the port therefore shares one address budget, and a walk that guesses a dozen passwords spends
/// the budget its neighbours are still asserting against. This was found the hard way: halving the
/// enforced address limit (the document's own threshold of 5 against the legacy 10) turned three
/// unrelated auth walks red with `address_blocked` from 127.0.0.1 — failures that pointed at the
/// sign-in path and had nothing to do with it. The second octet is what separates the counters.
fn peer_address() -> String {
    // `as_u128` rather than `simple()` — the latter is a Display formatter, not a value.
    let draw = Uuid::new_v4().as_u128();
    format!("127.{}.{}.{}", 1 + (draw % 200) as u8, 0 + ((draw >> 8) % 250) as u8, 1 + ((draw >> 16) % 250) as u8)
}

/// An account whose **password** is what the walk drives, rather than a pre-made session.
///
/// The existing [`account`] helper hands back a session credential, which is the right shape for
/// a permission walk and the wrong one here: this walk has to guess a password and be refused, so
/// it needs the address and the password, not a session that would sail past the very layer under
/// test.
async fn login_account(
    harness: &Harness,
    organization_id: Option<Uuid>,
) -> (Uuid, String, String) {
    let email = format!("lockout-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        harness.db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Lockout Walk".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    (user.id, email, PASSWORD.to_owned())
}

/// One `POST /auth/login`, from a named peer address.
///
/// The request goes through `request` (so the credential packing stays in one place) and then has
/// the peer inserted into its extensions, which is what `call` cannot do for a caller that needs
/// a specific address.
async fn login_from(
    harness: &Harness,
    peer: &str,
    email: &str,
    password: &str,
) -> TestResponse {
    let payload = json!({ "email": email, "password": password }).to_string();
    let builder = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/auth/login")
        .header(header::CONTENT_TYPE, "application/json");
    let (mut parts, body) = builder
        .body(Body::from(payload))
        .expect("request must build")
        .into_parts();
    if let Ok(address) = peer.parse::<std::net::SocketAddr>() {
        parts
            .extensions
            .insert(axum::extract::ConnectInfo(address));
    }
    harness
        .call(Request::from_parts(parts, body))
        .await
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

/// Every `/security` route, with the permission key the request needs to get past the guard.
///
/// This table is the suite's own source of truth, and it is deliberately written out by hand
/// rather than scraped from `routes/mod.rs`. A census read out of the router would read the
/// guard out of the same line it reads the path from, so a route that lost its guard would be
/// compared against itself and pass. Here the path and the key are two claims about the code
/// that a person made, and each walk checks both: the refusal below must name the key.
const SURFACE: &[(&str, Method, &str, &str)] = &[
    ("/api/v1/security/overview", Method::GET, "security.read", "read"),
    ("/api/v1/security/findings", Method::GET, "security.read", "read"),
    ("/api/v1/security/findings.csv", Method::GET, "security.read", "read"),
    ("/api/v1/security/headers", Method::GET, "security.read", "read"),
    (
        "/api/v1/security/rate-limits",
        Method::GET,
        "security.read",
        "read",
    ),
    (
        "/api/v1/security/sign-in-protection",
        Method::GET,
        "security.read",
        "read",
    ),
    (
        "/api/v1/security/locked-accounts",
        Method::GET,
        "security.read",
        "read",
    ),
    (
        "/api/v1/security/rate-limits/test",
        Method::GET,
        "security.read",
        "read",
    ),
    (
        "/api/v1/security/sign-in-protection/probe",
        Method::GET,
        "security.read",
        "read",
    ),
    (
        "/api/v1/security/checks/run",
        Method::POST,
        "security.scan",
        "write",
    ),
    (
        "/api/v1/security/findings/import",
        Method::POST,
        "security.scan",
        "write",
    ),
    (
        "/api/v1/security/findings/bulk",
        Method::POST,
        "security.manage",
        "write",
    ),
    (
        "/api/v1/security/findings/{id}",
        Method::PUT,
        "security.manage",
        "write",
    ),
    (
        "/api/v1/security/headers",
        Method::PUT,
        "security.manage",
        "write",
    ),
    (
        "/api/v1/security/rate-limits",
        Method::PUT,
        "security.manage",
        "write",
    ),
    (
        "/api/v1/security/sign-in-protection",
        Method::PUT,
        "security.manage",
        "write",
    ),
];

/// A request to one entry of [`SURFACE`], with `:id` and `:user_id` filled in.
///
/// The bodies are deliberately minimal and mostly *wrong*: a walk that reaches the handler
/// would get a validation error, which is still a refusal and still not a `403`. What these
/// walks assert is that the request is turned away **before** the body is looked at — a guard
/// is a gate, not a parser.
fn surface_request(method: Method, uri: &str, kind: &str, credential: Option<&str>) -> Request<Body> {
    let uri = uri.replace("{id}", &Uuid::new_v4().to_string());
    let body = match kind {
        "write" => Some(json!({ "note": "walk" })),
        _ => None,
    };
    request(method, &uri, credential, body)
}

/// No session is `401` on every `/security` route, and never a page of data.
#[tokio::test]
async fn every_security_route_refuses_an_anonymous_caller() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    for (uri, method, key, kind) in SURFACE {
        let response = harness
            .call(surface_request(method.clone(), uri, kind, None))
            .await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} ({key}) answered {:?} to an anonymous caller; every /security route \
             must be behind a session",
            response.status
        );
    }

    harness.dispose().await;
}

/// A member of the organization holding **no** security key is `403 permission_denied`,
/// and the message names the key that was missing.
#[tokio::test]
async fn a_member_without_the_key_is_refused_everywhere() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    let organization_id = create_organization_row(&harness.db, "No keys").await;
    let (_member, member_token) = account(&harness, Some(organization_id)).await;

    for (uri, method, key, kind) in SURFACE {
        let response = harness
            .call(surface_request(
                method.clone(),
                uri,
                kind,
                Some(&member_token),
            ))
            .await;

        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{method} {uri} answered {:?} to an account with no security key; it must be 403",
            response.status
        );

        // The refusal is the useful half. `guards::explain_denial` promises a body that names
        // the permission, so an operator reading the screen can see which key to ask for —
        // and a route whose guard names a key that is not in the catalogue would otherwise be
        // refused with the same body as one that is.
        let code = response.body["error"]["code"]
            .as_str()
            .or_else(|| response.body["code"].as_str())
            .unwrap_or_else(|| panic!("{method} {uri} has no error code: {}", response.text));
        assert_eq!(
            code, "permission_denied",
            "{method} {uri} refused with {code:?} rather than permission_denied"
        );
        assert!(
            response.text.contains(key),
            "{method} {uri} must name the \"{key}\" permission it needs; body was: {}",
            response.text
        );
    }

    harness.dispose().await;
}

/// Holding **one** key is not holding the others.
///
/// This is the walk that would have caught the `analytics.read` inheritance. An account with
/// `security.read` and nothing else may look at the centre; it must be refused every write and
/// every scan. If this test ever fails, the fix is not to widen the reader role — it is to find
/// the route whose guard moved.
#[tokio::test]
async fn holding_one_key_does_not_grant_the_next_one() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    let organization_id = create_organization_row(&harness.db, "Reader only").await;
    let (reader, reader_token) = account(&harness, Some(organization_id)).await;
    grant(&harness, reader, organization_id, &["security.read"]).await;

    for (uri, method, key, kind) in SURFACE {
        let response = harness
            .call(surface_request(
                method.clone(),
                uri,
                kind,
                Some(&reader_token),
            ))
            .await;

        if *key == "security.read" {
            // The reader's own routes must actually open. A guard this test treats as satisfied
            // without evidence is exactly the "only ever satisfied" failure it exists to stop.
            assert_ne!(
                response.status,
                StatusCode::FORBIDDEN,
                "{method} {uri} is declared security.read and refused the security.read holder: {}",
                response.text
            );
            assert_ne!(
                response.status,
                StatusCode::UNAUTHORIZED,
                "{method} {uri} is declared security.read and refused an authenticated reader"
            );
            continue;
        }

        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{method} {uri} needs {key}, and an account holding only security.read answered \
             {:?} instead of 403 — a guard was inherited, moved or dropped",
            response.status
        );
    }

    harness.dispose().await;
}

/// A holder of the full key set reaches the handlers, so the refusals above are the guards and
/// not a surface that answers `403` to everybody.
///
/// Without this, "every route refuses a member with no keys" would also be satisfied by a
/// centre whose routes are broken. The full holder must get past the guard on every route; what
/// happens next is up to the handler (a `400` from validation is the honest outcome of the
/// deliberately-wrong bodies above).
#[tokio::test]
async fn the_full_holder_passes_the_guard_on_every_route() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    let organization_id = create_organization_row(&harness.db, "Full holder").await;
    let (holder, holder_token) = account(&harness, Some(organization_id)).await;
    grant(
        &harness,
        holder,
        organization_id,
        &["security.read", "security.scan", "security.manage"],
    )
    .await;

    for (uri, method, key, kind) in SURFACE {
        let response = harness
            .call(surface_request(
                method.clone(),
                uri,
                kind,
                Some(&holder_token),
            ))
            .await;

        assert!(
            response.status != StatusCode::UNAUTHORIZED
                && response.status != StatusCode::FORBIDDEN,
            "{method} {uri} refuses the account that holds {key}; the route is unreachable for \
             everybody. Body: {}",
            response.text
        );
    }

    harness.dispose().await;
}

/// The number on `/security/sign-in-protection` is the number that locks an account (REQ-012).
///
/// **This walk exists because the screen and the request path disagreed while every test in the
/// platform stayed green.** The sign-in-protection screen edits `security_settings.lockout`. The
/// sign-in path locked accounts from `security_policies.lockout_attempts` — a different table,
/// from a different migration, with a different default (10). Four of the six fields an operator
/// tunes (`window_seconds`, `progressive_delay`, `base_delay_seconds`, `reset_on_success`) had
/// **no reader anywhere on the request path** at all.
///
/// That is why the assertion is not "a lock happens". `tests/auth.rs` already proves that, and
/// it passed with the defect in place. This walk is: **save a threshold through the screen's own
/// route, then count the wrong passwords it takes to lock.** Two implementations of one policy
/// can only be told apart at the attempt where their numbers differ.
///
/// Three choices, each of which could otherwise have made this walk pass for the wrong reason:
///
/// * **The document is written through `PUT /security/sign-in-protection`,** not by an `update`
///   on the column. Writing the column directly would prove only that the column is read, and
///   would leave a broken save route green.
/// * **The IAM row is seeded to a number that contradicts the document** (10 against a tuned 3),
///   and the contradiction is *asserted* rather than assumed. Without that assertion a run in
///   which both numbers happened to coincide would pass silently.
/// * **The guesses go through `POST /auth/login` on the real router** rather than through
///   `signin::sign_in` directly, because the two differ in exactly the place that matters: the
///   limiter middleware and the route are what decide whether the account layer is reached at
///   all.
#[tokio::test]
async fn the_threshold_on_the_screen_is_the_threshold_that_locks() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    let organization_id = create_organization_row(&harness.db, "Enforced threshold").await;

    // The legacy IAM document, seeded with its table default — the number the sign-in path used
    // to read. The walk asserts it differs from the number saved below; that difference is the
    // whole reason the assertion has any power.
    let legacy_attempts: i32 = sqlx::query_scalar(
        "insert into security_policies (organization_id) values ($1) \
         returning lockout_attempts",
    )
    .bind(organization_id)
    .fetch_one(harness.db.pool())
    .await
    .expect("the IAM policy row must be created");
    assert_eq!(
        legacy_attempts, 10,
        "the IAM column default is what made the two documents disagree; if this ever changes, \
         this walk's premise needs re-reading rather than re-tuning"
    );

    // The account whose lockout is measured, and the operator who tunes the policy. Two
    // accounts on purpose: the one being locked must not be the one holding the permission, or a
    // walk that signed in as the wrong account would be measuring itself.
    let (victim, victim_email, victim_password) = login_account(&harness, Some(organization_id)).await;
    let (operator_id, operator_token) = account(&harness, Some(organization_id)).await;
    // Both keys, deliberately: the probe is `security.read` while the save is
    // `security.manage`. Granting only the write key looked tidier and then read as a broken
    // tester at the very end of the walk — the refusal was correct and the walk was wrong. The
    // permission catalogue is what decides this, so the grant follows the route table.
    grant(
        &harness,
        operator_id,
        organization_id,
        &["security.read", "security.manage"],
    )
    .await;

    let tuned_attempts = 3;
    assert_ne!(
        tuned_attempts, legacy_attempts,
        "the document must contradict the legacy column, or this walk proves nothing"
    );

    // Saved through the route the screen calls, so a broken save cannot pass this walk.
    let saved = harness
        .call(put(
            "/api/v1/security/sign-in-protection",
            json!({
                "window_seconds": 900,
                "attempts": tuned_attempts,
                "lockout_minutes": 15,
                "progressive_delay": true,
                "base_delay_seconds": 2,
                "reset_on_success": true,
            }),
            Some(&operator_token),
        ))
        .await;
    assert_eq!(
        saved.status,
        StatusCode::OK,
        "the screen's own save route must accept the document: {}",
        saved.text
    );
    assert_eq!(
        saved.body["policy"]["attempts"], tuned_attempts,
        "the saved document must read back the tuned number: {}",
        saved.text
    );

    // Walk to the lock and count the attempts. The limiter's own counter is cleared before each
    // one so the ACCOUNT layer is what answers: the `sign_in` scope's ceiling and a threshold of
    // three are not the same rule, and a test that let the limiter answer would be measuring the
    // wrong layer while reading as a lockout.
    let peer = peer_address();
    let redis = harness.state.redis().clone();
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
        ip: peer.parse::<std::net::SocketAddr>().ok().map(|address| address.ip()),
    };

    let mut locked_at: Option<u32> = None;
    for attempt in 1..=12_u32 {
        let _ = omnion_security::forget(
            &redis,
            &sign_in_policy,
            &limiter_client,
            time::OffsetDateTime::now_utc().unix_timestamp(),
        )
        .await;
        let response = login_from(&harness, &peer, &victim_email, "definitely-not-the-password").await;
        match response.body["error"]["code"].as_str() {
            Some("account_locked") => {
                locked_at = Some(attempt);
                break;
            }
            // `invalid_credentials` is the correct answer for every attempt *before* the
            // threshold, so it is the walk's expected middle, not a failure. The limiter may
            // answer instead if its clear lost the race; both are retried rather than asserted on,
            // because the assertion below is about *which document decides*, and `locked_at` is
            // the only number this walk actually claims.
            Some("invalid_credentials") | Some("rate_limited") => continue,
            Some(other) => panic!(
                "attempt {attempt} answered {other:?}; a wrong password must answer \
                 invalid_credentials until the account locks: {}",
                response.text
            ),
            None => panic!(
                "attempt {attempt} SUCCEEDED with a wrong password: {}",
                response.text
            ),
        }
    }

    assert_eq!(
        locked_at,
        Some(tuned_attempts as u32),
        "the document saved on the screen says {tuned_attempts} failures lock an account, and the \
         account locked after a different number. The sign-in path is reading \
         security_policies.lockout_attempts ({legacy_attempts}) instead of the document the \
         operator edited."
    );

    // And the account is genuinely locked, read from the row rather than inferred from the
    // refusal: a response code and a stored lock are two different claims.
    let (locked_until, counter): (Option<time::OffsetDateTime>, i32) = sqlx::query_as(
        "select locked_until, failed_sign_in_count from users where id = $1",
    )
    .bind(victim)
    .fetch_one(harness.db.pool())
    .await
    .expect("the account row must be readable");
    assert!(
        locked_until.is_some_and(|until| until > time::OffsetDateTime::now_utc()),
        "the lock must be in the future, not a timestamp in the past"
    );
    assert_eq!(
        counter, tuned_attempts,
        "the account's own counter must show the failures that were counted, so the panel's \
         'currently locked' row explains itself"
    );

    // The probe on the same screen must agree with what the sign-in path just did. This is the
    // tester's whole contract, and it is checked against a REAL locked account rather than a
    // hand-built count: a tester that agreed with the screen and disagreed with the platform is
    // the exact failure this walk's first half is about.
    let probed = harness
        .call(post(
            "/api/v1/security/sign-in-protection/probe",
            json!({ "user_id": victim }),
            Some(&operator_token),
        ))
        .await;
    assert_eq!(
        probed.status,
        StatusCode::OK,
        "the tester must read the locked account: {}",
        probed.text
    );
    assert_eq!(
        probed.body["would_lock"], true,
        "the tester says the account would not lock, and it is locked: {}",
        probed.text
    );
    assert_eq!(
        probed.body["attempts_remaining"], 0,
        "a locked account has no attempts left: {}",
        probed.text
    );

    let _ = victim_password;
    harness.dispose().await;
}
