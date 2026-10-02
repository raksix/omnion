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
        // `oneshot` carries no `ConnectInfo`, so a request through this harness has no peer
        // address — and the IP access layer refuses an address-less request with `ip_unknown`
        // while any rule is in force. That refusal is **correct** and is asserted deliberately by
        // `a_denied_network_cannot_reach_the_api`; it is simply not what the other walks are
        // measuring, so they would all be refused before reaching the guard they test.
        //
        // Set here, once, rather than in each walk: a suite that only passes when an operator
        // remembers an environment variable is a suite that silently stops testing. The flag is
        // off in every deployed configuration and `security_ip` logs it at boot when it is on.
        //
        // It is set inside the test process only — `std::env::set_var` is unsafe in a multi
        // threaded program, so the harness does it once before the router exists and the walks
        // are `--test-threads=1` throughout this file.
        unsafe {
            std::env::set_var("OMNION_IP_ACCESS_ALLOW_UNADDRESSED", "1");
        }

        let mut config = Config::from_env().expect("environment must be valid");
        // The CSRF secret, for the same reason `tests/events.rs` sets one: a cookie write with
        // no double-submit token is refused `403 csrf_failed` before the permission layer is
        // ever consulted, and a suite that measured the wrong refusal would look like a guard
        // that works.
        support::walk_auth::with_csrf_secret(&mut config);
        let csrf_secret = config
            .csrf
            .as_bytes()
            .expect("the suite just set a secret")
            .to_vec();
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
    format!(
        "127.{}.{}.{}",
        1 + (draw % 200) as u8,
        0 + ((draw >> 8) % 250) as u8,
        1 + ((draw >> 16) % 250) as u8
    )
}

/// An account whose **password** is what the walk drives, rather than a pre-made session.
///
/// The existing [`account`] helper hands back a session credential, which is the right shape for
/// a permission walk and the wrong one here: this walk has to guess a password and be refused, so
/// it needs the address and the password, not a session that would sail past the very layer under
/// test.
async fn login_account(harness: &Harness, organization_id: Option<Uuid>) -> (Uuid, String, String) {
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
async fn login_from(harness: &Harness, peer: &str, email: &str, password: &str) -> TestResponse {
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
        parts.extensions.insert(axum::extract::ConnectInfo(address));
    }
    harness.call(Request::from_parts(parts, body)).await
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
    (
        "/api/v1/security/overview",
        Method::GET,
        "security.read",
        "read",
    ),
    (
        "/api/v1/security/findings",
        Method::GET,
        "security.read",
        "read",
    ),
    (
        "/api/v1/security/findings.csv",
        Method::GET,
        "security.read",
        "read",
    ),
    (
        "/api/v1/security/headers",
        Method::GET,
        "security.read",
        "read",
    ),
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
        "/api/v1/security/ip-rules",
        Method::GET,
        "security.read",
        "read",
    ),
    (
        "/api/v1/security/ip-rules/test",
        Method::POST,
        "security.read",
        "write",
    ),
    (
        "/api/v1/security/sign-in-protection/probe",
        Method::GET,
        "security.read",
        "read",
    ),
    // Security events (REQ-012 slice 4). The export is `read` too, deliberately: it changes
    // nothing, and an operator whose role is to audit must be able to take the trail with them.
    (
        "/api/v1/security/events",
        Method::GET,
        "security.read",
        "read",
    ),
    (
        "/api/v1/security/events.csv",
        Method::GET,
        "security.read",
        "read",
    ),
    // The secret inventory (REQ-012 slice 4). Read-only: management belongs to the secrets
    // manager request, so there is no write method to guard and nothing to widen.
    (
        "/api/v1/security/secrets",
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
fn surface_request(
    method: Method,
    uri: &str,
    kind: &str,
    credential: Option<&str>,
) -> Request<Body> {
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
            response.status != StatusCode::UNAUTHORIZED && response.status != StatusCode::FORBIDDEN,
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
    let (victim, victim_email, victim_password) =
        login_account(&harness, Some(organization_id)).await;
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
        ip: peer
            .parse::<std::net::SocketAddr>()
            .ok()
            .map(|address| address.ip()),
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
        let response = login_from(
            &harness,
            &peer,
            &victim_email,
            "definitely-not-the-password",
        )
        .await;
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
    let (locked_until, counter): (Option<time::OffsetDateTime>, i32) =
        sqlx::query_as("select locked_until, failed_sign_in_count from users where id = $1")
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

// ---------------------------------------------------------------------------------------------
// Slice 4: the IP access list on the request path
// ---------------------------------------------------------------------------------------------

/// A request that presents itself as coming from `address`.
///
/// The access layer reads the peer address out of the `ConnectInfo` extension — the same source
/// the rate limiter uses — so the walk has to install that extension or it would be testing the
/// `ip_unknown` branch (which refuses too, for the wrong reason). This is the detail that makes
/// the difference between a walk that proves a CIDR is refused and one that proves requests
/// without an address are refused.
fn from_address(mut request: Request<Body>, address: std::net::IpAddr) -> Request<Body> {
    use axum::extract::ConnectInfo;
    request
        .extensions_mut()
        .insert(ConnectInfo(std::net::SocketAddr::new(address, 40_000)));
    request
}

fn address(text: &str) -> std::net::IpAddr {
    text.parse().expect("a fixture address must parse")
}

/// **The criterion: a denied CIDR cannot reach the API.**
///
/// Every other part of slice 4 is a table, a form and a route. This is the walk that would fail
/// if any of them were inert: a deny rule is written through the panel's own `POST`, and a real
/// request from inside that network is driven over the router — not the evaluator called directly,
/// not a unit test — and must come back refused, with the rule named in the body.
///
/// Two halves that are different from each other, because either alone would pass for the wrong
/// reason:
///
/// * The refusal must be **`ip_denied` naming the rule**, not `ip_unknown` and not the route's
///   own permission `403`. A request from an address *inside* a denied network and a request with
///   no address at all are both refused by the same layer; only the first proves the rule.
/// * The same request from *outside* the network must be served, proving the rule narrows rather
///   than the screen breaking everything — a layer that refused every caller would satisfy the
///   first half of this assertion on its own.
#[tokio::test]
async fn a_denied_network_cannot_reach_the_api() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: no live database is configured");
        return;
    };

    let organization = create_organization_row(&harness.db, "IP Access").await;
    let (operator_id, operator) = account(&harness, Some(organization)).await;
    grant(
        &harness,
        operator_id,
        organization,
        &["security.read", "security.manage", "security.ip.manage"],
    )
    .await;

    let inside = address("203.0.113.7");
    let outside = address("198.51.100.7");
    // A third address no rule covers, for the calls made *after* the self-blocking rule
    // exists. It stands for the operator who noticed they had locked themselves out and walked
    // to the CLI — see the note on the removal below.
    let elsewhere = address("192.0.2.7");

    // The premise, asserted rather than assumed: before the rule exists, the request is served.
    // A walk that only checked the "after" would pass just as well if the route were refusing
    // everything for some unrelated reason.
    let before = harness
        .call(from_address(
            get("/api/v1/security/overview", Some(&operator)),
            inside,
        ))
        .await;
    assert_eq!(
        before.status,
        StatusCode::OK,
        "the premise: nothing is denied yet, so this must be served: {}",
        before.text
    );

    // The rule is written through the panel's own endpoint — not inserted with SQL — so the walk
    // also covers the route, the audit row and the event.
    let created = harness
        .call(from_address(
            post(
                "/api/v1/security/ip-rules",
                json!({
                    "kind": "deny",
                    "cidr": "203.0.113.0/24",
                    "note": "the walk's own denied network",
                }),
                Some(&operator),
            ),
            outside,
        ))
        .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "the rule must be created: {}",
        created.text
    );
    assert_eq!(created.body["rule"]["cidr"], "203.0.113.0/24");
    assert_eq!(created.body["rule"]["kind"], "deny");

    // Now the criterion.
    let denied = harness
        .call(from_address(
            get("/api/v1/security/overview", Some(&operator)),
            inside,
        ))
        .await;
    assert_eq!(
        denied.status,
        StatusCode::FORBIDDEN,
        "a request from inside the denied network must be refused: {}",
        denied.text
    );
    assert_eq!(
        denied.body["error"]["code"], "ip_denied",
        "the refusal must come from the access list and not from some other guard: {}",
        denied.text
    );
    assert!(
        denied.body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("203.0.113.0/24")),
        "the refusal must name the rule, or the operator cannot remove it: {}",
        denied.text
    );

    // The other half: everything outside the network is still served. Without this, a layer that
    // refused every caller would satisfy the assertions above.
    let served = harness
        .call(from_address(
            get("/api/v1/security/overview", Some(&operator)),
            outside,
        ))
        .await;
    assert_eq!(
        served.status,
        StatusCode::OK,
        "a request from outside the denied network must still be served: {}",
        served.text
    );

    // And the rule is listed, so the screen shows what is doing the refusing.
    let listed = harness
        .call(from_address(
            get("/api/v1/security/ip-rules", Some(&operator)),
            outside,
        ))
        .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.text);
    assert_eq!(listed.body["deny_count"], 1, "{}", listed.text);

    // The tester answers the same question the layer does, for the same address.
    let tested = harness
        .call(from_address(
            post(
                "/api/v1/security/ip-rules/test",
                json!({ "address": "203.0.113.7" }),
                Some(&operator),
            ),
            outside,
        ))
        .await;
    assert_eq!(tested.status, StatusCode::OK, "{}", tested.text);
    assert_eq!(
        tested.body["blocked"], true,
        "the tester must agree with the layer: {}",
        tested.text
    );
    assert_eq!(tested.body["decision"], "deny", "{}", tested.text);
    assert_eq!(
        tested.body["matched_rule"]["cidr"], "203.0.113.0/24",
        "the tester must name the rule it matched: {}",
        tested.text
    );

    // The self-lockout warning. This half of the criterion is a *different* claim from the
    // refusal above — that a rule which blocks the caller is stored and warned about rather than
    // refused — and it needs its own rule, because the rule written above deliberately does NOT
    // cover this session's address (that is what let the walk keep driving requests at all).
    let self_blocking = harness
        .call(from_address(
            post(
                "/api/v1/security/ip-rules",
                json!({
                    "kind": "deny",
                    // 198.51.100.0/24 is the address the walk is calling from.
                    "cidr": "198.51.100.0/24",
                    "note": "the walk's own address, deliberately",
                }),
                Some(&operator),
            ),
            outside,
        ))
        .await;
    assert_eq!(
        self_blocking.status,
        StatusCode::CREATED,
        "a rule that blocks the caller must still be saved: {}",
        self_blocking.text
    );
    assert_eq!(
        self_blocking.body["blocks_you"], true,
        "the response must say the rule covers this session: {}",
        self_blocking.text
    );
    assert!(
        self_blocking.body["warning"]
            .as_str()
            .is_some_and(|warning| warning.contains("198.51.100.0/24")),
        "the warning must name the network that is about to refuse this session: {}",
        self_blocking.text
    );

    // And the warning was honest: the very next request from that address is refused.
    let caught = harness
        .call(from_address(
            get("/api/v1/security/overview", Some(&operator)),
            outside,
        ))
        .await;
    assert_eq!(
        caught.status,
        StatusCode::FORBIDDEN,
        "the warning promised this; the layer must keep the promise: {}",
        caught.text
    );

    // Remove it, so the rest of the walk is not run from a locked-out session.
    let self_rule_id = self_blocking.body["rule"]["id"]
        .as_str()
        .expect("the created rule must carry an id")
        .to_owned();
    let removed_self = harness
        .call(from_address(
            request(
                Method::DELETE,
                &format!("/api/v1/security/ip-rules/{self_rule_id}"),
                Some(&operator),
                None,
            ),
            // Driven from a third address, and the reason is worth recording. The rule this
            // walk just created covers `outside`, so a request from there would be refused
            // before the layer reached the route — which is the feature working correctly, and
            // not what the assertion is about. It is also the real operator experience: once a
            // deny covers your own address, you cannot remove it from the panel, and the only
            // way back is another network or the CLI. The REQ's risk note asks for exactly that
            // escape to keep working, and this is the walk that keeps it honest.
            elsewhere,
        ))
        .await;
    assert_eq!(
        removed_self.status,
        StatusCode::NO_CONTENT,
        "{}",
        removed_self.text
    );

    // Removing the rule restores the address on the *next* request, not at the next boot.
    let rule_id = created.body["rule"]["id"]
        .as_str()
        .expect("the created rule must carry an id")
        .to_owned();
    let deleted = harness
        .call(from_address(
            request(
                Method::DELETE,
                &format!("/api/v1/security/ip-rules/{rule_id}"),
                Some(&operator),
                None,
            ),
            elsewhere,
        ))
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.text);

    let restored = harness
        .call(from_address(
            get("/api/v1/security/overview", Some(&operator)),
            inside,
        ))
        .await;
    assert_eq!(
        restored.status,
        StatusCode::OK,
        "removing the rule must take effect now, not at the next restart: {}",
        restored.text
    );

    harness.dispose().await;
}

/// The security-event timeline shows BOTH real sources, and the walk that would have caught
/// the audit-only projection (REQ-012, slice 4).
///
/// **What this is for.** The REQ words the screen as "a security-event table **from the audit
/// trail**", and a route built over `audit_log` alone answers `200` with an empty table on a
/// platform where every one of its own requirements is met — the empty table looks like a
/// working filter. Sign-ins live in `sign_in_attempts`, which no amount of auditing produces
/// rows in, because a failed sign-in happens before there is a session and therefore before
/// there is an actor to write an audit entry for.
///
/// So this walk seeds **one row in each of the two tables** and asserts both come back, and then
/// asserts the *filters* keep working across the seam — a category that matches on the audit side
/// and nothing on the sign-in side is exactly the defect this screen is built to avoid, and it is
/// only visible if both halves are checked.
#[tokio::test]
async fn the_timeline_merges_the_audit_trail_and_the_sign_in_log() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: no live database is configured");
        return;
    };

    let organization = create_organization_row(&harness.db, "Events").await;
    let (operator_id, operator) = account(&harness, Some(organization)).await;
    grant(
        &harness,
        operator_id,
        organization,
        &["security.read", "security.manage"],
    )
    .await;

    // One audit row, written through the API's own route so the action name is a real one rather
    // than a string the test invented.
    //
    // The policy store refuses a CSP that is not a policy: `default-src` is what every other
    // source list is measured against, and one of `script-src` / `script-src-elem` is required
    // because without it scripts are unconstrained. Both refusals happened here first — the walk
    // was seeding a row that the product correctly declined to write, and reading that as "the
    // save worked" would have been the same mistake in a new place.
    let header = harness
        .call(put(
            "/api/v1/security/headers",
            json!({
                "csp_mode": "enforce",
                "csp": [
                    { "directive": "default-src", "values": ["'self'"] },
                    { "directive": "script-src", "values": ["'self'"] },
                ],
            }),
            Some(&operator),
        ))
        .await;
    assert_eq!(
        header.status,
        StatusCode::OK,
        "the header save must land an audit row to project: {}",
        header.text
    );

    // One sign-in row, written the way the platform writes it: a wrong password against a real
    // account. This is the row an audit-only projection cannot see.
    let victim = format!("events-victim-{}@omnion.test", Uuid::new_v4().simple());
    sqlx::query(
        "insert into sign_in_attempts (email, organization_id, ip_address, user_agent, outcome) \
         values ($1, $2, '198.51.100.9', 'Mozilla/5.0 (Windows NT 10.0)', 'failed')",
    )
    .bind(&victim)
    .bind(organization)
    .execute(harness.db.pool())
    .await
    .expect("the sign-in attempt must be recorded");

    let page = harness
        .call(get("/api/v1/security/events", Some(&operator)))
        .await;
    assert_eq!(
        page.status,
        StatusCode::OK,
        "the timeline must answer: {}",
        page.text
    );

    let events = page.body["events"].as_array().expect("events is an array");
    let sources: Vec<&str> = events
        .iter()
        .filter_map(|event| event["source"].as_str())
        .collect();

    assert!(
        sources.contains(&"audit"),
        "the audit row is missing from the timeline: {}",
        page.text
    );
    assert!(
        sources.contains(&"sign_in"),
        "THE BUG THIS WALK EXISTS FOR: the failed sign-in does not appear. The screen would \
         render an empty sign-in list on a platform where nothing is wrong. events={:?}",
        events
            .iter()
            .map(|event| event["action"].as_str().unwrap_or("?"))
            .collect::<Vec<_>>()
    );

    // The counters agree with the rows, so "50 of 312" is a true sentence.
    assert!(page.body["total"].as_i64().unwrap_or(0) >= 2);
    assert!(page.body["audit_count"].as_i64().unwrap_or(0) >= 1);
    assert!(page.body["sign_in_count"].as_i64().unwrap_or(0) >= 1);

    // A sign-in row has NO actor, and that absence is a fact rather than a rendering fault:
    // nobody was authenticated. The address is the identity the operator hunts with.
    let sign_in_row = events
        .iter()
        .find(|event| event["source"] == "sign_in")
        .expect("the sign-in row is present");
    assert!(
        sign_in_row["actor"].is_null(),
        "a failed sign-in has no actor: {}",
        sign_in_row
    );
    // `ip_address::text` on an `inet` column keeps the prefix, so a single address reads as
    // `/32`. That is Postgres's own canonical text and is what an operator copying the value
    // out of the screen gets — asserting the bare address would force the query to strip a
    // prefix the column is supposed to carry.
    assert_eq!(sign_in_row["client_ip"], "198.51.100.9/32");
    assert_eq!(
        sign_in_row["refused"], true,
        "a wrong password is a refusal. PAYLOAD: {sign_in_row}"
    );

    // The id carries its source, because both tables have an identity column starting at 1 and
    // an identity without it would drop one of two real rows as a duplicate.
    for event in events {
        let id = event["id"].as_str().expect("every row has an id");
        assert!(
            id.starts_with("audit:") || id.starts_with("sign_in:"),
            "{id} does not name its source"
        );
    }

    // -- the filters, across the seam -----------------------------------------------------------------
    //
    // Each of these is a claim that a filter on the audit vocabulary cannot silently ignore the
    // sign-in vocabulary. A category returning rows from one side only is the exact defect.

    let sign_ins_only = harness
        .call(get(
            "/api/v1/security/events?category=sign_in",
            Some(&operator),
        ))
        .await;
    assert_eq!(
        sign_ins_only.status,
        StatusCode::OK,
        "category=sign_in must be answerable. BODY: {}",
        sign_ins_only.text
    );
    let sign_in_events = sign_ins_only.body["events"]
        .as_array()
        .expect("events is an array");
    assert!(
        sign_in_events
            .iter()
            .any(|event| event["source"] == "sign_in" && event["action"] == "failed"),
        "category=sign_in must find the failed attempt: {}",
        sign_ins_only.text
    );

    let audit_only = harness
        .call(get(
            "/api/v1/security/events?category=settings_change",
            Some(&operator),
        ))
        .await;
    assert_eq!(audit_only.status, StatusCode::OK);
    let audit_events = audit_only.body["events"]
        .as_array()
        .expect("events is an array");
    assert!(
        audit_events
            .iter()
            .any(|event| event["action"] == "security.headers.updated"),
        "category=settings_change must find the header save: {}",
        audit_only.text
    );
    // The two halves do not bleed into each other.
    assert!(
        !audit_events
            .iter()
            .any(|event| event["source"] == "sign_in"),
        "a settings change cannot be a sign-in attempt: {}",
        audit_only.text
    );

    // The source filter decides before any query runs.
    let sign_in_source = harness
        .call(get(
            "/api/v1/security/events?source=sign_in",
            Some(&operator),
        ))
        .await;
    assert!(
        sign_in_source.body["events"]
            .as_array()
            .expect("events is an array")
            .iter()
            .all(|event| event["source"] == "sign_in"),
        "source=sign_in must not return audit rows: {}",
        sign_in_source.text
    );

    // An unknown filter value is refused **by name**, never silently dropped into "no filter" —
    // an unparsed category shows the operator an unfiltered list they read as a filtered one.
    let unknown = harness
        .call(get(
            "/api/v1/security/events?category=denials",
            Some(&operator),
        ))
        .await;
    assert_eq!(
        unknown.status,
        StatusCode::BAD_REQUEST,
        "an unknown category must be refused: {}",
        unknown.text
    );
    assert_eq!(unknown.body["error"]["code"], "invalid_security_input");

    // -- the export -------------------------------------------------------------------------------
    //
    // It is the **whole** filter, not the page: an operator who filters and exports 50 of 300 has
    // produced a document that reads as a complete list and is not one.
    let export = harness
        .call(get("/api/v1/security/events.csv?limit=1", Some(&operator)))
        .await;
    assert_eq!(
        export.status,
        StatusCode::OK,
        "the export must answer: {}",
        export.text
    );
    let header_line = export.text.lines().next().unwrap_or_default();
    for column in ["occurred_at", "category", "action", "outcome", "client_ip"] {
        assert!(
            header_line.contains(column),
            "the export header is missing {column}: {header_line}"
        );
    }
    let exported_rows = export.text.lines().count().saturating_sub(1);
    assert!(
        exported_rows >= 2,
        "limit=1 must NOT page the export — it carries the whole filter. rows={exported_rows}: {}",
        export.text
    );
    assert!(
        export.text.contains("sign_in"),
        "the export must carry both sources: {}",
        export.text
    );

    // A credential in the audit metadata must not reach the file. A settings change writes the
    // policy it changed into its metadata, and this file is the thing an operator emails.
    //
    // Checked as a **key** in the digest column rather than as a substring of the whole file:
    // the first version of this assertion was `!export.text.contains("password")`, which is red
    // on every correct export, because the sign-in outcome renders as the prose "wrong password
    // or unknown account". Prose that describes an attack is not a credential, and a test that
    // cannot distinguish the two trains an operator to distrust the export.
    //
    // The digest is the only column that carries key *names*, and the shape a leaked one takes is
    // `secret=<value>` — a bare `=` after a credential-shaped key.
    for leak in [
        "secret=",
        "password=",
        "token=",
        "authorization=",
        "credential=",
    ] {
        assert!(
            !export.text.contains(leak),
            "the audit metadata leaked {leak} into the export: {}",
            export.text
        );
    }
    // The digest that *is* present proves the column works rather than being blank. It renders as
    // `key=shape` pairs sorted and capped, so the assertion matches the first pair rather than
    // the whole digest — `csp_mode=set, directive_count=2`, not `csp_mode=set` on its own.
    assert!(
        export.text.contains("csp_mode=set"),
        "the header save's digest should be present. DIGEST FORMAT: {}",
        export.text
    );

    harness.dispose().await;
}
#[tokio::test]
async fn a_malformed_cidr_is_refused_with_a_field_level_message() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: no live database is configured");
        return;
    };

    let organization = create_organization_row(&harness.db, "IP Validation").await;
    let (operator_id, operator) = account(&harness, Some(organization)).await;
    grant(&harness, operator_id, organization, &["security.ip.manage"]).await;

    for (cidr, why) in [
        ("not-an-ip", "not an address"),
        ("203.0.113.0/33", "a v4 prefix past 32"),
        ("2001:db8::/129", "a v6 prefix past 128"),
        ("203.0.113.1/8", "host bits set outside the prefix"),
        ("", "empty"),
    ] {
        let refused = harness
            .call(post(
                "/api/v1/security/ip-rules",
                json!({ "kind": "deny", "cidr": cidr, "note": "validation walk" }),
                Some(&operator),
            ))
            .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{cidr:?} ({why}) must be refused: {}",
            refused.text
        );
        assert_eq!(refused.body["error"]["code"], "invalid_security_input");
    }

    // The two forms that are *not* errors, so the refusals above cannot be an artefact of a
    // parser that refuses everything.
    for (cidr, stored) in [
        ("203.0.113.0/24", "203.0.113.0/24"),
        ("203.0.113.7", "203.0.113.7/32"),
        ("2001:db8::/32", "2001:db8::/32"),
    ] {
        let created = harness
            .call(post(
                "/api/v1/security/ip-rules",
                json!({ "kind": "deny", "cidr": cidr, "note": "accepted form" }),
                Some(&operator),
            ))
            .await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "{cidr:?} is valid and must be accepted: {}",
            created.text
        );
        assert_eq!(created.body["rule"]["cidr"], stored);
    }

    // A note is required, for the reason the migration's check constraint gives.
    let blank = harness
        .call(post(
            "/api/v1/security/ip-rules",
            json!({ "kind": "deny", "cidr": "198.51.100.0/24", "note": "   " }),
            Some(&operator),
        ))
        .await;
    assert_eq!(
        blank.status,
        StatusCode::BAD_REQUEST,
        "an unexplained rule must be refused: {}",
        blank.text
    );

    // The same network twice is refused rather than silently replacing the first.
    let duplicate = harness
        .call(post(
            "/api/v1/security/ip-rules",
            json!({ "kind": "deny", "cidr": "203.0.113.0/24", "note": "second try" }),
            Some(&operator),
        ))
        .await;
    assert_eq!(
        duplicate.status,
        StatusCode::BAD_REQUEST,
        "a duplicate must be refused, not silently upserted: {}",
        duplicate.text
    );

    harness.dispose().await;
}

// ---------------------------------------------------------------------------------------------
// The secret inventory (REQ-012, slice 4)
// ---------------------------------------------------------------------------------------------

/// **The walk this slice exists for: no secret value reaches the inventory response.**
///
/// It is not enough for the row type to lack a `value` field — a `select *` in the store can
/// put one in the response regardless. So the claim is made against the bytes the route returns
/// from a database that really holds material:
///
/// 1. a webhook endpoint whose signing secret is a recognisable literal;
/// 2. a service-account key whose hash is another;
/// 3. a confirmed TOTP factor whose ciphertext is a third;
/// 4. an identity provider naming a credential reference.
///
/// Then the response body is walked field by field and every one of those literals must be
/// absent. A scan for the *column names* would not be enough on its own: a store that selected
/// the column under an alias, or a renderer that inlined it into the note text, would pass it.
/// So the values themselves are the probe, which is the only version of this test that fails
/// when the actual leak happens.
///
/// The positives are asserted too, because a route that answers an empty object satisfies every
/// containment check in this test while showing an operator a blank screen.
#[tokio::test]
async fn no_secret_value_reaches_the_inventory_response() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: no live database is configured");
        return;
    };

    const WEBHOOK_SECRET: &str = "whsec_LIVEVALUE_webhook_signing_0123456789";
    const KEY_HASH: &str = "$argon2id$v=19$LIVEVALUE$serviceaccountkeyhashvalue";
    const TOTP_CIPHERTEXT: &str = "enc:v1:LIVEVALUE:totpciphertext";

    let organization = create_organization_row(&harness.db, "Secrets").await;
    let (operator_id, operator) = account(&harness, Some(organization)).await;
    grant(&harness, operator_id, organization, &["security.read"]).await;

    // One row per source that holds real material, written the way the platform writes them.
    sqlx::query(
        "insert into webhook_endpoints (organization_id, name, url, secret, events) \
         values ($1, 'inventory-walk', 'https://example.test/hook', $2, array['security.lockout.triggered'])",
    )
    .bind(organization)
    .bind(WEBHOOK_SECRET)
    .execute(harness.db.pool())
    .await
    .expect("the webhook endpoint must exist for the walk to mean anything");

    let service_account: Uuid = sqlx::query_scalar(
        "insert into service_accounts (organization_id, name, prefix) \
         values ($1, 'inventory-walk', 'sa_walk') returning id",
    )
    .bind(organization)
    .fetch_one(harness.db.pool())
    .await
    .expect("the service account must exist");
    sqlx::query(
        "insert into service_account_keys (service_account_id, prefix, secret_hash) \
         values ($1, 'sk_walk_live', $2)",
    )
    .bind(service_account)
    .bind(KEY_HASH)
    .execute(harness.db.pool())
    .await
    .expect("the key must exist");

    let (victim_id, victim_email) = account(&harness, Some(organization)).await;
    sqlx::query(
        "insert into mfa_factors (user_id, kind, secret_ciphertext, confirmed_at) \
         values ($1, 'totp', $2, now())",
    )
    .bind(victim_id)
    .bind(TOTP_CIPHERTEXT)
    .execute(harness.db.pool())
    .await
    .expect("the factor must exist");
    assert!(
        !victim_email.is_empty(),
        "the account helper must have created a real account"
    );

    sqlx::query(
        "insert into auth_providers (organization_id, slug, kind, name, secret_ref) \
         values ($1, 'walk-sso', 'oidc', 'Walk SSO', 'OMNION_WALK_PROVIDER_SECRET')",
    )
    .bind(organization)
    .execute(harness.db.pool())
    .await
    .expect("the provider must exist");

    let response = harness
        .call(get("/api/v1/security/secrets", Some(&operator)))
        .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the inventory must load: {}",
        response.text
    );

    // -- the containment claim ------------------------------------------------------------------
    for literal in [WEBHOOK_SECRET, KEY_HASH, TOTP_CIPHERTEXT] {
        assert!(
            !response.text.contains(literal),
            "THE LEAK THIS WALK EXISTS FOR: the inventory response contains a stored secret \
             ({literal:.12}…). The store selected a value column."
        );
    }
    // The reference — a *name* — must be present, because that is the difference between a
    // projection over references and a dump: the operator can see what is configured without
    // being able to read it.
    assert!(
        response.text.contains("OMNION_WALK_PROVIDER_SECRET"),
        "the credential NAME must be reported — a reference is not a secret: {}",
        response.text
    );

    // No row may declare a field that could hold material, whatever the value.
    let body: Value = serde_json::from_str(&response.text).expect("the body is JSON");
    let rows = body["secrets"].as_array().expect("`secrets` is an array");
    assert!(!rows.is_empty(), "the inventory must not be empty");
    for row in rows {
        let object = row.as_object().expect("each row is an object");
        for key in object.keys() {
            let lowered = key.to_lowercase();
            for forbidden in [
                "value",
                "secret",
                "ciphertext",
                "hash",
                "token",
                "preview",
                "plaintext",
                "password",
            ] {
                assert!(
                    !lowered.contains(forbidden),
                    "the inventory row declares a {forbidden} field: {key}"
                );
            }
        }
    }

    // -- the positives, or the test above passes on an empty screen --------------------------
    let names: Vec<String> = rows
        .iter()
        .filter_map(|row| row["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        names.iter().any(|n| n == "WEBHOOK_SIGNING_KEY"),
        "the webhook material must be counted, not hidden: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "SERVICE_ACCOUNT_KEY"),
        "the service-account material must be counted: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "MFA_TOTP_SECRET"),
        "the MFA material must be counted: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "OMNION_CSRF_SECRET"),
        "the environment secrets must be listed: {names:?}"
    );

    // The count is what replaced the material, so it must be a real number and not a constant.
    let webhooks = rows
        .iter()
        .find(|row| row["name"] == "WEBHOOK_SIGNING_KEY")
        .expect("the webhook row exists");
    assert!(
        webhooks["material_count"].as_i64().unwrap_or_default() >= 1,
        "the count must reflect the endpoint this walk created: {webhooks}"
    );
    assert!(
        !webhooks["note"].as_str().unwrap_or_default().is_empty(),
        "a material row must explain what it counts: {webhooks}"
    );

    // And the screen must be told what it cannot see — a limitation field the panel renders as a
    // permanent note, because an inventory that looks exhaustive when it is not is the defect.
    let limitation = body["limitation"].as_str().unwrap_or_default();
    assert!(
        limitation.contains("never values") && limitation.contains("maintained by hand"),
        "the response must state its own limits: {limitation}"
    );

    // -- no state reads as healthy --------------------------------------------------------------
    for state in ["healthy", "ok", "valid", "good"] {
        assert!(
            !response.text.contains(&format!("\"{state}\"")),
            "the inventory offers a {state} state"
        );
    }

    harness.dispose().await;
}

/// The inventory is read-only: a mutation is refused by the router, not by the client.
///
/// Management belongs to the secrets manager request. A screen that could edit a reference would
/// invite an operator to believe it can rotate a secret, and rotating one means replacing a value
/// in an environment and redeploying.
#[tokio::test]
async fn the_inventory_cannot_be_written_through() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: no live database is configured");
        return;
    };

    let organization = create_organization_row(&harness.db, "SecretsWrite").await;
    let (operator_id, operator) = account(&harness, Some(organization)).await;
    grant(
        &harness,
        operator_id,
        organization,
        &["security.read", "security.manage", "security.scan"],
    )
    .await;

    // Every write verb, with the **full** key set so the refusal cannot be mistaken for a
    // permission problem: this is the route shape refusing, not the guard.
    for (method, path) in [
        ("POST", "/api/v1/security/secrets"),
        ("PUT", "/api/v1/security/secrets"),
        ("DELETE", "/api/v1/security/secrets"),
        ("PATCH", "/api/v1/security/secrets"),
    ] {
        let request = match method {
            "POST" => post(path, json!({ "name": "X" }), Some(&operator)),
            "PUT" => put(path, json!({ "name": "X" }), Some(&operator)),
            // Auth goes through the shared credential helper rather than a
            // hand-built `Bearer` header: the walk credential is a packed
            // session+csrf pair, and assembling one by hand here produced a
            // request that failed at header parsing instead of at the route —
            // which reads as a product defect and is not one.
            other => request(
                Method::from_bytes(other.as_bytes()).expect("a known method"),
                path,
                Some(&operator),
                None,
            ),
        };
        let response = harness.call(request).await;
        assert!(
            matches!(
                response.status,
                StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_FOUND
            ),
            "{method} {path} must not exist, but answered {}: {}",
            response.status,
            response.text
        );
    }

    harness.dispose().await;
}

// ---------------------------------------------------------------------------------------------
// The `security.finding.opened` webhook (REQ-012, slice 4d)
// ---------------------------------------------------------------------------------------------

/// **The walk this piece exists for: an opened finding travels as an identity, not as content.**
///
/// The request's Events section names `security.finding.opened` as the one security event an
/// operator subscribes to, and that makes it the first security payload on the platform that
/// fans out to a receiver *outside* the operator's own infrastructure by default. Everything the
/// REQ does with a finding — its title, its description, its evidence — is content that came
/// from outside: a package name a CI vendor chose, prose from the report, the raw entry itself.
/// So the claim to prove is narrow and worth proving against bytes:
///
/// 1. ingest a report whose finding carries **recognisable literals in every content field**;
/// 2. let the platform emit and queue the fan-out to a subscribed endpoint;
/// 3. read the queued payload back and assert every one of those literals is **absent**,
///    while the fields a receiver needs to triage are **present**.
///
/// A name scan would pass this walk. The literals themselves are the probe — including one
/// placed where only a careless payload builder would put it: the finding's `note`, a field no
/// receiver needs and that a future editor could plausibly add without thinking.
///
/// **The negatives are not enough on their own**, and the second half of this walk is the part
/// that catches the emitter that fires nothing: the same report ingested twice must queue a
/// delivery the *first* time and **none** the second. An emitter on the `upsert` regardless of
/// its `created` branch passes every containment assertion above while paging a receiver every
/// morning for a finding that is a year old.
#[tokio::test]
async fn an_opened_finding_reaches_a_receiver_as_an_identity_and_not_as_content() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: no live database is configured");
        return;
    };

    // Four literals, one per field the emitter could plausibly reach for. They are deliberately
    // long and recognisable so a substring scan cannot miss a truncated or re-encoded copy.
    const TITLE_LITERAL: &str = "LIVEVALUE-DEPENDENCY-TITLE";
    const DESCRIPTION_LITERAL: &str = "LIVEVALUE-REPORT-DESCRIPTION-PROSE";
    const EVIDENCE_LITERAL: &str = "LIVEVALUE-EVIDENCE-ENTRY";
    const NOTE_LITERAL: &str = "LIVEVALUE-OPERATOR-NOTE";

    let organization = create_organization_row(&harness.db, "FindingEvents").await;
    let (operator_id, operator) = account(&harness, Some(organization)).await;
    grant(&harness, operator_id, organization, &["security.scan"]).await;

    // An endpoint subscribed to the one name. Its `secret` is a real value and is deliberately
    // in the table: if any part of the delivery path inlined the endpoint row into the payload,
    // the literal scan below would catch it.
    sqlx::query(
        "insert into webhook_endpoints (organization_id, name, url, secret, events) \
         values ($1, 'security-events-walk', 'https://example.test/hook', $2, \
                 array['security.finding.opened'])",
    )
    .bind(organization)
    .bind("whsec_LIVEVALUE_SIGNINGSECRET_0123456789")
    .execute(harness.db.pool())
    .await
    .expect("the subscribed endpoint must exist for the fan-out to mean anything");

    let report = json!({
        "findings": [{
            "title": TITLE_LITERAL,
            "severity": "critical",
            "description": DESCRIPTION_LITERAL,
            "component": "walk-dependency",
            "version": "1.2.3",
            "fixed_in": "1.2.4",
            "evidence": EVIDENCE_LITERAL,
            "note": NOTE_LITERAL,
        }]
    });

    let first = harness
        .call(post(
            "/api/v1/security/findings/import",
            json!({ "source": "dependency", "report": report.clone() }),
            Some(&operator),
        ))
        .await;
    assert_eq!(
        first.status,
        StatusCode::OK,
        "the first ingest must succeed: {}",
        first.text
    );
    assert_eq!(
        first.body["created"], 1,
        "the first ingest opens the finding: {}",
        first.text
    );
    assert_eq!(
        first.body["refreshed"], 0,
        "nothing was known before: {}",
        first.text
    );

    // -- the delivery a subscriber receives ----------------------------------------------------
    let queued: Vec<(Value, Uuid)> = sqlx::query_as(
        "select e.payload, d.endpoint_id from webhook_deliveries d \
         join events e on e.id = d.event_id \
         where e.name = 'security.finding.opened' and e.organization_id = $1",
    )
    .bind(organization)
    .fetch_all(harness.db.pool())
    .await
    .expect("the queued delivery must be readable");

    assert_eq!(
        queued.len(),
        1,
        "opening one finding to one subscribed endpoint must queue exactly one delivery, got {}",
        queued.len()
    );

    let (payload, endpoint_id) = queued.into_iter().next().expect("checked above");
    let endpoints: Uuid = sqlx::query_scalar(
        "select id from webhook_endpoints where organization_id = $1 \
         and name = 'security-events-walk'",
    )
    .bind(organization)
    .fetch_one(harness.db.pool())
    .await
    .expect("the endpoint row must be readable");
    assert_eq!(
        endpoint_id, endpoints,
        "the delivery must be addressed to the endpoint that subscribed to the name"
    );

    // THE CLAIM. Values, not column names: a payload that aliased the field, nested it or
    // inlined it into another string would pass a scan for `"title"` and fail this one.
    let rendered = payload.to_string();
    for literal in [
        TITLE_LITERAL,
        DESCRIPTION_LITERAL,
        EVIDENCE_LITERAL,
        NOTE_LITERAL,
        "whsec_LIVEVALUE_SIGNINGSECRET_0123456789",
    ] {
        assert!(
            !rendered.contains(literal),
            "THE LEAK THIS WALK EXISTS FOR: the delivery payload carries the finding's content \
             ({literal:.24}…). A receiver holds content it cannot un-send."
        );
    }
    // Field names are asserted too — the weaker half, and the one that reads as the strong one
    // if you stop here. A `title: null` satisfies it while still telling the receiver a title
    // exists, and the literal scan above is what makes null acceptable rather than fatal.
    let object = payload.as_object().expect("the payload is an object");
    for forbidden in ["title", "description", "evidence", "note"] {
        assert!(
            !object.contains_key(forbidden),
            "the payload declares {forbidden}: {rendered}"
        );
    }

    // -- the positives, or the walk above passes on a payload nobody can use --------------------
    let finding_id = payload["finding_id"]
        .as_str()
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .unwrap_or_else(|| {
            panic!(
                "the payload must carry the finding's id — a receiver cannot open what it cannot \
                 name: {rendered}"
            )
        });
    assert_eq!(
        payload["severity"], "critical",
        "severity is the one thing a receiver cannot compute from an id: {rendered}"
    );
    assert_eq!(
        payload["source"], "dependency",
        "the receiver must be able to tell a CI report from a platform check: {rendered}"
    );
    assert_eq!(
        payload["component"], "walk-dependency",
        "the triage triple is the package: {rendered}"
    );
    assert_eq!(
        payload["component_version"], "1.2.3",
        "…the version present…: {rendered}"
    );
    assert_eq!(
        payload["fixed_in"], "1.2.4",
        "…and the version that fixes it: {rendered}"
    );

    // The id must be a **real, readable** finding rather than a string the emitter invented —
    // otherwise "go look at finding X" sends the receiver to a 404 on every delivery.
    let stored: Option<String> = sqlx::query_scalar(
        "select title from security_findings where id = $1 and organization_id = $2",
    )
    .bind(finding_id)
    .bind(organization)
    .fetch_optional(harness.db.pool())
    .await
    .expect("the finding table must be readable");
    assert_eq!(
        stored.as_deref(),
        Some(TITLE_LITERAL),
        "the delivered id must resolve to the finding that opened: {stored:?}"
    );

    // -- the emitter must not fire on a finding that was already known ------------------------
    // This is the half a containment walk cannot supply. A nightly CI job re-ingests the same
    // report every morning; an endpoint that paged on all of them would be muted by the second
    // run, and the operator would learn to ignore it — which is the outcome a "webhook" that
    // works perfectly well has silently produced.
    let second = harness
        .call(post(
            "/api/v1/security/findings/import",
            json!({ "source": "dependency", "report": report }),
            Some(&operator),
        ))
        .await;
    assert_eq!(
        second.status,
        StatusCode::OK,
        "the second ingest must succeed: {}",
        second.text
    );
    assert_eq!(
        second.body["created"], 0,
        "the finding was already known: {}",
        second.text
    );
    assert_eq!(
        second.body["refreshed"], 1,
        "re-ingesting is a refresh, not an opening: {}",
        second.text
    );

    let after: i64 = sqlx::query_scalar(
        "select count(*) from webhook_deliveries d \
         join events e on e.id = d.event_id \
         where e.name = 'security.finding.opened' and e.organization_id = $1",
    )
    .bind(organization)
    .fetch_one(harness.db.pool())
    .await
    .expect("the delivery count must be readable");
    assert_eq!(
        after, 1,
        "re-ingesting an already-known finding queued {after} deliveries; it must queue none — \
         an event that fires on every CI run is an event an operator learns to ignore"
    );

    harness.dispose().await;
}


/// The two checks whose probe used to be a frozen `unknown` answer the policy they were meant
/// to read: `csp_configured` and `rate_limiting`.
///
/// **The defect this walk was written for.** Both probes in `gather` returned a literal
/// `Probe::unreadable("… configured in slice 2/3; nothing to verify yet")`. Slices 2 and 3
/// **shipped**: `0135_security_headers.sql` creates and seeds `security_settings.headers`, and
/// `0151_security_rate_limits.sql` adds `rate_limits` to the same singleton row, both of which
/// the `/security/headers` and `/security/rate-limits` screens have been writing through for
/// the whole of REQ-012. So `/security` reported *"`csp_configured`: Not checked yet — header
/// policy is configured in slice 2; nothing to verify yet"*, with the row's own action link
/// pointing at the page where the policy was plainly on screen. The score sat permanently
/// depressed and the reason it gave was a slice number, which is the worst kind of wrong on a
/// screen whose entire job is to be truthful.
///
/// **Why the row is not enough.** The response is computed from a freshly gathered environment
/// *and* the last stored result (`to_overview` returns the stored row when one exists), so a
/// response can report `pass` for a check whose stored row says `unknown`, and vice versa. Every
/// assertion below therefore reads `security_check_results` **out of PostgreSQL** — the same
/// rule as tick 105's manifest checksum, tick 106's `last_run_at` and REQ-010's purge walk: a
/// route can compute a correct answer and never store it.
///
/// **The four states, and which of them are new.** `warn` for report-only was already
/// reachable in the unit tests; what had never been reachable through a route was `pass`. The
/// walk proves the whole ladder through the real endpoints, because "all scopes disabled" and
/// "an enforcing policy" are the two states an operator actually produces, and a probe that can
/// only answer one of them is half a check.
///
/// Every write goes through `PUT` as the operator, not SQL: the point is that the *panel's*
/// action changes the answer, which is the whole claim. Writing the column directly would prove
/// the query works and nothing about whether a person can fix the row.
#[tokio::test]
async fn a_saved_header_and_rate_policy_change_the_posture_row_that_used_to_be_frozen() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    let organization = create_organization_row(&harness.db, "Posture Probes").await;
    let (operator_id, operator) = account(&harness, Some(organization)).await;
    grant(
        &harness,
        operator_id,
        organization,
        &["security.read", "security.manage", "security.scan"],
    )
    .await;

    /// One check's **stored** row, read straight out of PostgreSQL.
    ///
    /// `store::latest_results` is exactly this query, so this is not a second implementation —
    /// it is the read the route itself does, pointed at by key instead of enumerated.
    async fn stored_state(
        harness: &Harness,
        organization: Uuid,
        key: &str,
    ) -> Option<(String, serde_json::Value)> {
        let row: Option<(String, serde_json::Value)> = sqlx::query_as(
            "select state, detail from security_check_results r \
             where r.organization_id = $1 and r.check_key = $2 \
               and r.id = (select newest.id from security_check_results newest \
                           where newest.organization_id = $1 and newest.check_key = $2 \
                           order by newest.checked_at desc, newest.id desc limit 1)",
        )
        .bind(organization)
        .bind(key)
        .fetch_optional(harness.db.pool())
        .await
        .expect("the stored posture row must be readable");
        row
    }

    // -----------------------------------------------------------------------------------------
    // The premise: with nothing ever saved, both checks must be `unknown` and must SAY why.
    // Asserted before anything is written, because a walk that only checks the "after" would
    // pass just as well if the probe answered `pass` unconditionally.
    // -----------------------------------------------------------------------------------------
    let first = harness
        .call(post("/api/v1/security/checks/run", json!({}), Some(&operator)))
        .await;
    assert_eq!(
        first.status,
        StatusCode::OK,
        "the first run must be recorded: {}",
        first.text
    );

    // The premise is asserted rather than assumed, and **it is not `unknown`** — the first
    // version of this walk assumed it was and failed with `left: "fail", right: "unknown"`,
    // which is worth recording rather than papering over.
    //
    // A fresh install is not an unverified platform. Migrations `0135` and `0151` seed
    // `security_settings` with `headers = {}` and `rate_limits = []`, and both readers treat an
    // empty document as **the baseline**, deliberately: a missing settings row must never mean
    // "send no headers" (`an_unreadable_document_falls_back_to_the_baseline_rather_than_to_no_headers`
    // pins it). So the honest first-run answers are `warn` — the baseline CSP is report-only, so
    // it reports and does not protect — and `pass` for the limiter, whose defaults are enabled.
    //
    // Both are real states with real reasons, and that is the point: the old frozen probe
    // answered `unknown` for both, which is the one state that means *nobody knows*.
    let (csp_state, csp_detail) = stored_state(&harness, organization, "csp_configured")
        .await
        .expect("the first run must have stored a row for csp_configured");
    assert_eq!(
        csp_state, "warn",
        "the seeded baseline is a report-only policy of FOUR directives, so the first run must \
         say so rather than claim a pass or hide behind `unknown`: {}",
        first.text
    );
    // `detail.fact` is the probe's own value, which is where the count lives: the top level of
    // `detail` is the sentence and the reason an operator reads, and reading the count out of
    // it would be reading a UI field as if it were the store.
    assert_eq!(
        csp_detail["fact"]["directives"], 4,
        "and the directive count must be the parsed policy's, not zero — the check read the \
         count as an array once and reported `fail` with `no directives` on a platform whose \
         baseline has four: {csp_detail}"
    );
    assert_eq!(
        csp_detail["fact"]["mode"], "report_only",
        "and the fact must name the mode the middleware will actually send: {csp_detail}"
    );

    let (rate_state, rate_first_detail) = stored_state(&harness, organization, "rate_limiting")
        .await
        .expect("the first run must have stored a row for rate_limiting");
    assert_eq!(
        rate_state, "pass",
        "an empty limiter document resolves to the default policy, which is enabled — and a \
         check that said `unknown` here was withholding a fact the platform had: {}",
        first.text
    );
    assert!(
        rate_first_detail["fact"]["enabled_scopes"]
            .as_array()
            .is_some_and(|scopes| !scopes.is_empty()),
        "a passing rate-limit row must name the scopes it counted: {rate_first_detail}"
    );

    // -----------------------------------------------------------------------------------------
    // Now the operator enforces a CSP through the panel's own endpoint.
    // -----------------------------------------------------------------------------------------
    let saved = harness
        .call(put(
            "/api/v1/security/headers",
            json!({
                "csp_mode": "enforce",
                // `script-src` is not optional: `HeaderPolicy::new` refuses a policy without it
                // ("a policy needs \"script-src\" or \"script-src-elem\""), and the first
                // draft of this walk did exactly that and read the refusal as a bug. It was the
                // fixture. A validator that holds here is the validator working.
                "csp": [
                    { "directive": "default-src", "values": ["'self'"] },
                    { "directive": "script-src", "values": ["'self'"] },
                ],
                "hsts_max_age_seconds": 31_536_000,
                "hsts_include_subdomains": true,
                "hsts_preload": false,
                "content_type_options": true,
                "referrer_policy": "strict-origin-when-cross-origin",
                "permissions_policy": ["camera=()"],
            }),
            Some(&operator),
        ))
        .await;
    assert_eq!(
        saved.status,
        StatusCode::OK,
        "the policy must be savable by the operator who holds the key: {}",
        saved.text
    );

    // The second run's stored row is the assertion — not the response, and not the panel.
    let second = harness
        .call(post("/api/v1/security/checks/run", json!({}), Some(&operator)))
        .await;
    assert_eq!(
        second.status,
        StatusCode::OK,
        "the second run must be recorded: {}",
        second.text
    );

    let (csp_after, csp_after_detail) = stored_state(&harness, organization, "csp_configured")
        .await
        .expect("the second run must have stored a row for csp_configured");
    assert_eq!(
        csp_after, "pass",
        "a policy this operator just enforced, read back out of the database, must be a `pass` — \
         and for ever, not only after a restart"
    );
    assert_eq!(
        csp_after_detail["fact"]["mode"], "enforce",
        "the stored fact must be the policy the middleware will send, not the request's shape: \
         {csp_after_detail}"
    );
    assert_eq!(
        csp_after_detail["fact"]["directives"], 2,
        "the directive count is read from the same parsed policy the header middleware builds, \
         so the row cannot claim a policy the platform does not hold: {csp_after_detail}"
    );

    // -----------------------------------------------------------------------------------------
    // And the honest middle: report-only is a `warn`, never a green row.
    // -----------------------------------------------------------------------------------------
    let report_only = harness
        .call(put(
            "/api/v1/security/headers",
            json!({
                "csp_mode": "report_only",
                "csp": [
                    { "directive": "default-src", "values": ["'self'"] },
                    { "directive": "script-src", "values": ["'self'"] },
                ],
                "hsts_max_age_seconds": 31_536_000,
                "hsts_include_subdomains": true,
                "hsts_preload": false,
                "content_type_options": true,
                "referrer_policy": "strict-origin-when-cross-origin",
                "permissions_policy": [],
            }),
            Some(&operator),
        ))
        .await;
    assert_eq!(
        report_only.status,
        StatusCode::OK,
        "switching back to report-only must be savable: {}",
        report_only.text
    );
    harness
        .call(post("/api/v1/security/checks/run", json!({}), Some(&operator)))
        .await;
    let (csp_warn, _) = stored_state(&harness, organization, "csp_configured")
        .await
        .expect("the third run must have stored a row");
    assert_eq!(
        csp_warn, "warn",
        "a header that only reports violations is not a policy, and a green row here is the \
         over-claim this whole screen exists to prevent"
    );

    // -----------------------------------------------------------------------------------------
    // The limiter, same three steps — and the state that had no writer at all: every scope off.
    // -----------------------------------------------------------------------------------------
    let all_scopes: Vec<Value> = omnion_security::RATE_SCOPES
        .iter()
        .map(|scope| {
            json!({
                "scope": scope,
                "window_seconds": 60,
                "limit": 600,
                "burst": 0,
                "enabled": *scope != "sign_in",
            })
        })
        .collect();

    let saved_limits = harness
        .call(put(
            "/api/v1/security/rate-limits",
            json!(all_scopes),
            Some(&operator),
        ))
        .await;
    assert_eq!(
        saved_limits.status,
        StatusCode::OK,
        "the limiter document must be savable: {}",
        saved_limits.text
    );
    harness
        .call(post("/api/v1/security/checks/run", json!({}), Some(&operator)))
        .await;
    let (rate_after, rate_detail) = stored_state(&harness, organization, "rate_limiting")
        .await
        .expect("the fourth run must have stored a row");
    assert_eq!(
        rate_after, "pass",
        "four of five scopes enabled is a limiter that is limiting, read back out of the \
         database"
    );
    assert_eq!(
        rate_detail["fact"]["enabled_scopes"].as_array().map(Vec::len),
        Some(4),
        "the row must name the scopes it counted, so a wrong count is visible on the screen \
         rather than inferred: {rate_detail}"
    );

    // The one that had no writer: a document with every scope disabled.
    let none_enabled: Vec<Value> = omnion_security::RATE_SCOPES
        .iter()
        .map(|scope| {
            json!({
                "scope": scope,
                "window_seconds": 60,
                "limit": 600,
                "burst": 0,
                "enabled": false,
            })
        })
        .collect();
    let off = harness
        .call(put(
            "/api/v1/security/rate-limits",
            json!(none_enabled),
            Some(&operator),
        ))
        .await;
    assert_eq!(
        off.status,
        StatusCode::OK,
        "disabling every scope is a legal configuration and must be savable: {}",
        off.text
    );
    harness
        .call(post("/api/v1/security/checks/run", json!({}), Some(&operator)))
        .await;
    let (rate_off, rate_off_detail) = stored_state(&harness, organization, "rate_limiting")
        .await
        .expect("the fifth run must have stored a row");
    assert_ne!(
        rate_off, "pass",
        "a limiter with every scope disabled is a limiter that is not limiting, and `pass` here \
         is the single most expensive row on the screen"
    );
    assert_eq!(
        rate_off, "fail",
        "and the state is the failing one, not `unknown`: this platform read the document and \
         knows exactly what it says: {rate_off_detail}"
    );

    // The last claim, and the one the whole walk exists for: **neither row may still be frozen.**
    let mut still_frozen: Vec<&str> = Vec::new();
    for key in ["csp_configured", "rate_limiting"] {
        let Some((_, detail)) = stored_state(&harness, organization, key).await else {
            continue;
        };
        let reason = detail.get("reason").and_then(Value::as_str).unwrap_or_default();
        if reason.contains("nothing to verify yet") {
            still_frozen.push(key);
        }
    }
    assert!(
        still_frozen.is_empty(),
        "a check whose reason still names a slice as missing is a check that stopped reading the \
         platform: {still_frozen:?}"
    );

    harness.dispose().await;
}
