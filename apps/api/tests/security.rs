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
