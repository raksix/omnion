//! Credential walks for the developer portal (REQ-022, slice 1).
//!
//! **What this file is for.** The REQ's first acceptance criterion is *"a key authenticates on
//! a guarded endpoint and is rejected after revocation"*, and slice 1's done-when is *"a key
//! created through the API authenticates a guarded call and its request appears in the log with
//! the matched permission"*. Both are claims about **two halves meeting**: a store that issues
//! a credential nobody can present, and a guard that accepts one the store never issued. A unit
//! test over `issue()` proves the token is well-formed; a unit test over the guard proves the
//! guard has a branch. Neither proves the two are the same branch.
//!
//! So every walk here drives the **real router in process** against a **real database**, and
//! reads its answers back out of PostgreSQL rather than out of a response body wherever the
//! claim is about what was stored.
//!
//! The walks, and the defect each one exists because the obvious implementation gets it wrong:
//!
//! 1. `a_key_authenticates_a_guarded_call_and_dies_the_moment_it_is_revoked` — the criterion.
//!    Rotation is walked too, because "rotate" and "overwrite" look identical until you check
//!    whether the predecessor still authenticates.
//! 2. `a_key_cannot_be_minted_with_a_scope_its_issuer_does_not_hold` — the delegation rule.
//!    The store would happily store the row; only the route can refuse, so only a route walk
//!    sees it.
//! 3. `no_response_carries_a_secret_a_second_time` — the secret-hygiene claim. It greps the
//!    real JSON of every read for the token, the hash, **and** the raw prefix material, because
//!    a check that greps only for the token passes on a response that echoes the hash.
//! 4. `the_request_log_records_the_permission_the_guard_resolved_and_never_the_query_string`
//!    — the two log claims that only exist in the database.
//!
//! Every walk is `--test-threads=1`, and each creates and drops its own database, so a run never
//! touches the development database.

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

/// Password for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// The permission the sandbox probe route is guarded for. A key must carry this to pass, and the
/// delegation walk must fail without it.
const PROBE_SCOPE: &str = "content.pages.read";

/// A scope an operator can hold but a *reader* must not be able to delegate.
const UNDELEGATABLE: &str = "iam.users.manage";

/// The scope the narrow key in the middleware walk carries, and is **denied** the probe by.
///
/// It has to be a real catalogued scope rather than a made-up one, and it has to be a scope the
/// minting account holds — the delegation rule refuses a key whose scopes its issuer does not
/// hold, so a fixture that invents its own "wrong" scope gets `400 you do not hold it yourself`
/// and never reaches the `403` it was written to observe.
const NARROW_SCOPE: &str = "analytics.read";

// --------------------------------------------------------------------------------------------
// Harness
// --------------------------------------------------------------------------------------------

struct TestResponse {
    status: StatusCode,
    body: Value,
    text: String,
}

struct Harness {
    state: AppState,
    db: Db,
    maintenance: Db,
    database: String,
    csrf_secret: Vec<u8>,
}

impl Harness {
    async fn fresh() -> Option<Self> {
        // Same reason as `tests/security.rs`: the IP access layer refuses an address-less
        // request with `ip_unknown` while any rule is in force, and `oneshot` carries no
        // `ConnectInfo`. Set once, before the router exists.
        unsafe {
            std::env::set_var("OMNION_IP_ACCESS_ALLOW_UNADDRESSED", "1");
        }

        let mut config = Config::from_env().expect("environment must be valid");
        support::walk_auth::with_csrf_secret(&mut config);
        let csrf_secret = config
            .csrf
            .as_bytes()
            .expect("the suite just set a secret")
            .to_vec();
        // The log fingerprint refuses without a pepper, on purpose. The suite sets one so the
        // log walk measures the log and not the refusal.
        unsafe {
            std::env::set_var("OMNION_LOG_PEPPER", "developer-walk-pepper");
        }
        if live_db(&config).await.is_none() {
            return None;
        }

        let database = format!("omnion_developer_{}", Uuid::new_v4().simple());
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

        Some(Self { state, db, maintenance, database, csrf_secret })
    }

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

fn get(uri: &str, credential: Option<&str>) -> Request<Body> {
    request(Method::GET, uri, credential, None)
}

fn post(uri: &str, body: Value, credential: Option<&str>) -> Request<Body> {
    request(Method::POST, uri, credential, Some(body))
}

fn delete(uri: &str, credential: Option<&str>) -> Request<Body> {
    request(Method::DELETE, uri, credential, None)
}

fn request(
    method: Method,
    uri: &str,
    credential: Option<&str>,
    body: Option<Value>,
) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match credential {
        Some(value) => support::walk_auth::apply_credential(value, builder),
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

/// A request authenticated by a key rather than a session.
fn with_key(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request must build")
}

async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&config.database).await {
        Ok(db) => Some(db),
        Err(err) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({err})");
            None
        }
    }
}

fn maintenance_config(config: &Config) -> DatabaseConfig {
    DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 2,
    }
}

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

/// An account with a session, plus its id.
async fn account(harness: &Harness, organization_id: Option<Uuid>) -> (Uuid, String) {
    let user = users::create_user(
        harness.db.pool(),
        NewUser {
            email: format!("developer-{}@omnion.test", Uuid::new_v4().simple()),
            password: PASSWORD.to_owned(),
            display_name: "Portal Walk".to_owned(),
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

/// Bind a role holding exactly these keys to one account.
async fn grant(harness: &Harness, user_id: Uuid, organization_id: Uuid, keys: &[&str]) {
    let role = role_store::create_role(
        harness.db.pool(),
        NewRole {
            organization_id,
            key: format!("developer-walk-{}", Uuid::new_v4().simple()),
            name: "Portal Walk".to_owned(),
            description: "The keys one walk needs".to_owned(),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the organization role must be created");

    let entries: Vec<RolePermissionInput> = keys
        .iter()
        .map(|key| RolePermissionInput { key: (*key).to_owned(), effect: Effect::Allow })
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
}

async fn create_organization_row(db: &Db) -> Uuid {
    let slug = format!("developer-walk-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind("Portal Walk")
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// Create a key through the API and return its id and its **one and only** plaintext.
async fn create_key(
    harness: &Harness,
    credential: &str,
    name: &str,
    scopes: &[&str],
) -> (Uuid, String) {
    let response = harness
        .call(post(
            "/api/v1/developer/api-keys",
            json!({ "name": name, "scopes": scopes }),
            Some(credential),
        ))
        .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "the key must be created: {}",
        response.text
    );
    let id = Uuid::parse_str(response.body["key"]["id"].as_str().expect("the id must be there"))
        .expect("the id must be a uuid");
    let token = response.body["token"]
        .as_str()
        .expect("the create response carries the token once")
        .to_owned();
    (id, token)
}

// --------------------------------------------------------------------------------------------
// Walks
// --------------------------------------------------------------------------------------------

/// The criterion: a key authenticates a guarded call, and revocation kills it at once.
#[tokio::test]
async fn a_key_authenticates_a_guarded_call_and_dies_the_moment_it_is_revoked() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let organization_id = create_organization_row(&harness.db).await;
    let (_user, credential) = account(&harness, Some(organization_id)).await;
    // `PROBE_SCOPE` is granted **here** on purpose: the delegation rule refuses a key carrying a
    // scope the issuer does not hold, so a walk that asks for one without holding it measures
    // the refusal, not the probe. (Three walks got this wrong on the first run and every one of
    // them failed at `400 you do not hold it yourself` — the rule working exactly as designed.)
    grant(
        &harness,
        _user,
        organization_id,
        &[
            "developer.read",
            "developer.keys.read",
            "developer.keys.manage",
            PROBE_SCOPE,
        ],
    )
    .await;

    let (_key_id, token) = create_key(&harness, &credential, "Publisher", &[PROBE_SCOPE]).await;

    // 1. It authenticates. `401` here would mean the store issued a credential the guard cannot
    //    read — the two halves never met, which is exactly what this walk exists to catch.
    let probe = harness
        .call(with_key("/api/v1/developer/sandbox/probe", &token))
        .await;
    assert_eq!(
        probe.status,
        StatusCode::OK,
        "a live key must authenticate on a guarded route: {}",
        probe.text
    );

    // 2. Its last-used timestamp moved, read **out of PostgreSQL** rather than from the list
    //    response: the detail screen shows the column, and only the column is the claim.
    let last_used: Option<time::OffsetDateTime> =
        sqlx::query_scalar("select last_used_at from api_keys where id = $1")
            .bind(_key_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the key row must be readable");
    assert!(
        last_used.is_some(),
        "a key that just authenticated cannot still read as never used — the list screen's \\
         'Last used' column would be permanently wrong"
    );

    // 3. Revoke it.
    let revoked = harness
        .call(delete(&format!("/api/v1/developer/api-keys/{_key_id}"), Some(&credential)))
        .await;
    assert_eq!(revoked.status, StatusCode::OK, "revoke: {}", revoked.text);

    // 4. **The same token, unchanged, is now refused.** Not "the old token" — the identical
    //    bytes that answered `200` two requests ago.
    let after = harness
        .call(with_key("/api/v1/developer/sandbox/probe", &token))
        .await;
    assert_eq!(
        after.status,
        StatusCode::UNAUTHORIZED,
        "a revoked key must stop authenticating: {}",
        after.text
    );

    // 5. And the row survives, because a request made before the revoke must still name the key
    //    that made it. A hard delete would leave those rows pointing at nothing.
    let still_there: (Option<time::OffsetDateTime>,) =
        sqlx::query_as("select revoked_at from api_keys where id = $1")
            .bind(_key_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the revoked row must still exist");
    assert!(
        still_there.0.is_some(),
        "revoke is a soft delete: the row is what the log names"
    );

    harness.dispose().await;
}

/// Rotation kills the predecessor at once and keeps its history.
#[tokio::test]
async fn rotation_kills_the_previous_secret_immediately_and_keeps_the_old_row() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let organization_id = create_organization_row(&harness.db).await;
    let (user, credential) = account(&harness, Some(organization_id)).await;
    grant(
        &harness,
        user,
        organization_id,
        &[
            "developer.read",
            "developer.keys.read",
            "developer.keys.manage",
            PROBE_SCOPE,
        ],
    )
    .await;

    let (old_id, old_token) = create_key(&harness, &credential, "Rotating", &[PROBE_SCOPE]).await;
    assert_eq!(
        harness
            .call(with_key("/api/v1/developer/sandbox/probe", &old_token))
            .await
            .status,
        StatusCode::OK
    );

    let rotated = harness
        .call(post(
            &format!("/api/v1/developer/api-keys/{old_id}/rotate"),
            json!({}),
            Some(&credential),
        ))
        .await;
    assert_eq!(rotated.status, StatusCode::OK, "rotate: {}", rotated.text);
    let new_token = rotated.body["token"]
        .as_str()
        .expect("rotate carries the new token once")
        .to_owned();
    let new_id = Uuid::parse_str(rotated.body["key"]["id"].as_str().expect("the successor id"))
        .expect("the successor id must parse");
    assert_ne!(new_id, old_id, "rotation must be a NEW row, or the history is gone");

    // The old secret, byte for byte the one that just worked.
    assert_eq!(
        harness
            .call(with_key("/api/v1/developer/sandbox/probe", &old_token))
            .await
            .status,
        StatusCode::UNAUTHORIZED,
        "rotation must invalidate the previous secret immediately, not at the next restart"
    );
    assert_eq!(
        harness
            .call(with_key("/api/v1/developer/sandbox/probe", &new_token))
            .await
            .status,
        StatusCode::OK,
        "the new secret must authenticate"
    );

    // The predecessor keeps its row — that is what keeps its usage history addressable.
    let predecessor: (Option<time::OffsetDateTime>, Option<Uuid>) =
        sqlx::query_as("select revoked_at, rotated_from from api_keys where id = $1")
            .bind(old_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the predecessor row must survive");
    assert!(
        predecessor.0.is_some(),
        "the predecessor is revoked, not deleted: its log rows still name it"
    );
    assert_eq!(
        predecessor.1, None,
        "the predecessor is the root of the chain, so it points at nothing"
    );

    let successor: Option<Uuid> =
        sqlx::query_scalar("select rotated_from from api_keys where id = $1")
            .bind(new_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the successor row must exist");
    assert_eq!(
        successor,
        Some(old_id),
        "the successor must name its predecessor, or the chain cannot be drawn"
    );

    harness.dispose().await;
}

/// The delegation rule: a key may only carry a scope its issuer holds.
#[tokio::test]
async fn a_key_cannot_be_minted_with_a_scope_its_issuer_does_not_hold() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let organization_id = create_organization_row(&harness.db).await;
    // This account holds the *manage* power but NOT `iam.users.manage`. That combination is the
    // whole point: without the rule, holding `developer.keys.manage` would be a pass to
    // everything.
    let (user, credential) = account(&harness, Some(organization_id)).await;
    grant(
        &harness,
        user,
        organization_id,
        &[
            "developer.read",
            "developer.keys.read",
            "developer.keys.manage",
            PROBE_SCOPE,
        ],
    )
    .await;

    // Refused, with the offending scope named.
    let refused = harness
        .call(post(
            "/api/v1/developer/api-keys",
            json!({ "name": "Escalation", "scopes": [PROBE_SCOPE, UNDELEGATABLE] }),
            Some(&credential),
        ))
        .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "a key may only delegate what the issuer holds: {}",
        refused.text
    );
    assert!(
        refused.text.contains(UNDELEGATABLE),
        "the refusal must name the scope it refused, not just say no: {}",
        refused.text
    );

    // And nothing was stored — the refusal is not cosmetic.
    let count: (i64,) = sqlx::query_as(
        "select count(*) from api_keys where organization_id = $1 and name = 'Escalation'",
    )
    .bind(organization_id)
    .fetch_one(harness.db.pool())
    .await
    .expect("the count must read");
    assert_eq!(count.0, 0, "a refused key must leave no row behind");

    // The legitimate half still works, or the rule is "refuse everything".
    let (_id, token) = create_key(&harness, &credential, "Legitimate", &[PROBE_SCOPE]).await;
    assert_eq!(
        harness
            .call(with_key("/api/v1/developer/sandbox/probe", &token))
            .await
            .status,
        StatusCode::OK
    );

    harness.dispose().await;
}

/// A key that lacks the route's scope is refused `403`, naming the missing scope.
#[tokio::test]
async fn a_key_without_the_routes_scope_is_refused_with_a_named_gap() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let organization_id = create_organization_row(&harness.db).await;
    let (user, credential) = account(&harness, Some(organization_id)).await;
    grant(
        &harness,
        user,
        organization_id,
        &["developer.read", "developer.keys.read", "developer.keys.manage", "analytics.read"],
    )
    .await;

    // The key carries a real, catalogued scope — just not the one the probe route requires.
    let (_id, token) = create_key(&harness, &credential, "Wrong scope", &["analytics.read"]).await;

    let refused = harness
        .call(with_key("/api/v1/developer/sandbox/probe", &token))
        .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a key without the scope is refused, not routed around: {}",
        refused.text
    );
    assert!(
        refused.text.contains(PROBE_SCOPE),
        "the refusal must name the scope to add, or the integrator is left guessing: {}",
        refused.text
    );

    harness.dispose().await;
}

/// Secret hygiene: nothing a read returns can carry the token or its hash.
#[tokio::test]
async fn no_response_carries_a_secret_a_second_time() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let organization_id = create_organization_row(&harness.db).await;
    let (user, credential) = account(&harness, Some(organization_id)).await;
    grant(
        &harness,
        user,
        organization_id,
        &[
            "developer.read",
            "developer.keys.read",
            "developer.keys.manage",
            "developer.logs.read",
            PROBE_SCOPE,
        ],
    )
    .await;

    let (id, token) = create_key(&harness, &credential, "Hygiene", &[PROBE_SCOPE]).await;
    // The hash is read straight from the row so the check can look for it. A check that greps
    // only for the token passes on a response that echoes the hash, which is just as fatal.
    let hash: (String,) =
        sqlx::query_as("select key_hash from api_keys where id = $1")
            .bind(id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the hash column must read");

    // Every read the panel makes, including the detail and the log list.
    for uri in [
        "/api/v1/developer/api-keys".to_owned(),
        format!("/api/v1/developer/api-keys/{id}"),
        "/api/v1/developer/logs".to_owned(),
    ] {
        let response = harness.call(get(&uri, Some(&credential))).await;
        assert_eq!(response.status, StatusCode::OK, "{uri}: {}", response.text);
        assert!(
            !response.text.contains(&token),
            "{uri} echoed the token a second time — the reveal-once guarantee is the whole \\
             product of this screen"
        );
        assert!(
            !response.text.contains(&hash.0),
            "{uri} echoed the stored hash. A SHA-256 of a 32-character secret is offline-\
             crackable, so a hash in a response body is the credential with extra steps"
        );
        assert!(
            !response.text.contains("secret"),
            "{uri} carries a field named secret: {}",
            response.text
        );
    }

    harness.dispose().await;
}

/// The log records the permission the guard resolved, and never the query string.
#[tokio::test]
async fn the_request_log_records_the_matched_permission_and_never_the_query_string() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let organization_id = create_organization_row(&harness.db).await;
    let (user, credential) = account(&harness, Some(organization_id)).await;
    grant(
        &harness,
        user,
        organization_id,
        &[
            "developer.read",
            "developer.keys.read",
            "developer.keys.manage",
            "developer.logs.read",
            PROBE_SCOPE,
        ],
    )
    .await;

    let (id, token) = create_key(&harness, &credential, "Logged", &[PROBE_SCOPE]).await;

    // One authenticated request, and one refused one — both belong in the log, because the
    // refusal is the row an operator actually wants ("why is my integration 403ing").
    let _ = harness
        .call(with_key("/api/v1/developer/sandbox/probe", &token))
        .await;

    // Write the rows through the crate's own recorder, the way the middleware does. A log test
    // that inserted rows by hand would prove the INSERT works and nothing about the path.
    let now = time::OffsetDateTime::now_utc();
    let identity = omnion_developer::ClientIdentity::new(
        None,
        None,
        Some(id),
        Some("omndev_live_abcdefghij"),
        Some(organization_id),
        Some(PROBE_SCOPE),
        Some("203.0.113.9"),
        Some("integration/1.0"),
    )
    .expect("the pepper is set by the harness");
    omnion_developer::logs_store::record(
        harness.db.pool(),
        "get",
        "/api/v1/developer/sandbox/probe",
        200,
        7,
        &identity,
        now,
    )
    .await
    .expect("the log row must be written");

    let row: (String, Option<String>, Option<String>, Option<time::OffsetDateTime>) =
        sqlx::query_as(
            "select path, permission, client_fingerprint, created_at from api_request_logs
              where api_key_id = $1",
        )
        .bind(id)
        .fetch_one(harness.db.pool())
        .await
        .expect("the log row must be readable");
    assert_eq!(row.0, "/api/v1/developer/sandbox/probe");
    assert_eq!(
        row.1.as_deref(),
        Some(PROBE_SCOPE),
        "the matched permission is the column that makes a 403 explainable; a log without it \
         cannot answer why"
    );
    let fingerprint = row
        .2
        .expect("a client address must leave a fingerprint, or the log cannot tell one client \
                 from another");
    assert!(
        !fingerprint.contains("203.0.113.9"),
        "the fingerprint must not contain the address in any form: {fingerprint}"
    );

    // The query string is stripped at the store, and this walks the real recorder.
    omnion_developer::logs_store::record(
        harness.db.pool(),
        "get",
        "/api/v1/media?access_token=super-secret-value",
        200,
        3,
        &identity,
        now,
    )
    .await
    .expect("the second log row must be written");
    let stored_path: (String,) =
        sqlx::query_as("select path from api_request_logs where path like '/api/v1/media%'")
            .fetch_one(harness.db.pool())
            .await
            .expect("the stripped path must be stored");
    assert_eq!(
        stored_path.0, "/api/v1/media",
        "a token in a query string is a credential, and this table has a CSV export"
    );

    // And the list serves it back without the secret.
    let listed = harness
        .call(get("/api/v1/developer/logs", Some(&credential)))
        .await;
    assert_eq!(listed.status, StatusCode::OK, "logs: {}", listed.text);
    assert!(
        !listed.text.contains("super-secret-value"),
        "the log screen must not re-introduce what the store stripped"
    );

    harness.dispose().await;
}

/// The criterion the whole request log rests on: **a request the platform served writes a row
/// because the platform served it.** (REQ-022, slice 2.)
///
/// The walk above proves the recorder works. It cannot prove the recorder is *called*, because
/// it is the thing calling it — which is exactly how slice 1 shipped a complete, tested, empty
/// request log: every store, filter, screen and walk in place, and no request in the platform
/// ever recorded by anything the platform did. A debugging surface that is silently blank is
/// the failure this table exists to catch, and it was the platform's own state.
///
/// So this walk never touches `logs_store::record`. It makes real requests through the real
/// router and reads the answer back out of PostgreSQL, in the shapes the middleware has to get
/// right separately: a **session** request (a person, and the permission the guard resolved), a
/// **key** request (the key's id, and the scope the guard checked), and a **refusal** (the row
/// an integrator most often needs, and the one a log that records only successes cannot make).
///
/// Two negative claims too, because a log layer is exactly the thing that can be wrong in both
/// directions at once: the platform's own probes must **not** be recorded, and a query string
/// must not survive into the table.
#[tokio::test]
async fn a_request_writes_its_own_log_row_and_nothing_else_does() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let organization_id = create_organization_row(&harness.db).await;
    let (user, credential) = account(&harness, Some(organization_id)).await;
    grant(
        &harness,
        user,
        organization_id,
        &[
            "developer.read",
            "developer.keys.read",
            "developer.keys.manage",
            "developer.logs.read",
            PROBE_SCOPE,
            // The delegation rule at work on this walk's own fixture: a key may only be
            // created with a scope its issuer holds, so the account that mints the *narrow*
            // key below has to hold the scope the narrow key is denied. A walk that forgot
            // this got `400 you do not hold it yourself` — the rule working, the fixture not
            // having noticed it exists.
            NARROW_SCOPE,
        ],
    )
    .await;

    let (key_id, token) = create_key(&harness, &credential, "Middlewared", &[PROBE_SCOPE]).await;

    // 1. A session request — the most common row this table will ever hold.
    let ok = harness
        .call(get("/api/v1/developer/api-keys", Some(&credential)))
        .await;
    assert_eq!(ok.status, StatusCode::OK, "the list must answer: {}", ok.text);

    // 2. A key request, on the one route that accepts one.
    let probe = harness
        .call(with_key("/api/v1/developer/sandbox/probe", &token))
        .await;
    assert_eq!(probe.status, StatusCode::OK, "the probe must answer: {}", probe.text);

    // 3. A refusal. `/developer/sandbox/probe` is the only route that accepts a key, and it is
    //    guarded for `content.pages.read`, so a key carrying only `analytics.read` is refused
    //    with `403 scope_missing` — and the row has to exist, because a log that cannot show a
    //    403 cannot answer "why is my integration being refused".
    //
    //    The first attempt at this walk asked a key for `/developer/logs` and got `401
    //    unauthenticated` — which was the **product being right and the fixture being wrong**:
    //    that route is session-only, so a bearer token there is not a key with the wrong scope,
    //    it is an unauthenticated caller. The fixture now asks the route that actually accepts
    //    keys, and the refusal it produces is a real scope gap.
    let (narrow_id, narrow_token) =
        create_key(&harness, &credential, "Narrow", &[NARROW_SCOPE]).await;
    let refused = harness
        .call(with_key("/api/v1/developer/sandbox/probe", &narrow_token))
        .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "the key lacks the scope, so the guard must refuse: {}",
        refused.text
    );
    assert_eq!(
        refused.body["error"]["code"].as_str(),
        Some("scope_missing"),
        "the refusal must name the gap rather than say the caller is anonymous: {}",
        refused.text
    );

    // ---- read it back out of PostgreSQL, not out of a response body ----
    let session_row: (String, Option<uuid::Uuid>, Option<String>, i16) = sqlx::query_as(
        "select path, api_key_id, permission, status
           from api_request_logs
          where organization_id = $1 and actor_user_id = $2 and path = $3",
    )
    .bind(organization_id)
    .bind(user)
    .bind("/api/v1/developer/api-keys")
    .fetch_one(harness.db.pool())
    .await
    .expect(
        "a request the platform served must have logged itself. If this fails, the layer is not \
         installed, or it sits outside the guards, or the guard is not publishing what it resolved.",
    );
    assert_eq!(
        session_row.0, "/api/v1/developer/api-keys",
        "the row must name the request the operator made"
    );
    assert_eq!(
        session_row.1, None,
        "a session is not a key: a key id here would be the log claiming a session authenticated \
         as something it did not"
    );
    assert_eq!(
        session_row.2.as_deref(),
        Some("developer.keys.read"),
        "the permission column is what makes a 403 explainable, and it must be the one the guard \
         actually resolved — not the route's own name, and not nothing"
    );
    assert_eq!(session_row.3, 200);

    // The key's own request, named by its id and the scope that was checked.
    let key_row: (i16, Option<String>) = sqlx::query_as(
        "select status, permission from api_request_logs where api_key_id = $1",
    )
    .bind(key_id)
    .fetch_one(harness.db.pool())
    .await
    .expect("a key-authenticated request must have logged itself");
    assert_eq!(key_row.0, 200);
    assert_eq!(
        key_row.1.as_deref(),
        Some(PROBE_SCOPE),
        "for a key, the matched scope *is* the authorization — the single most useful column in \
         the table for somebody debugging a broken integration"
    );

    // And the refusal, with the scope the guard asked for rather than the one it resolved. The
    // path is the probe's, because that is the route a key can reach — see the note on the
    // fixture above.
    let denied_row: (i16, Option<String>, Option<uuid::Uuid>) = sqlx::query_as(
        "select status, permission, api_key_id
           from api_request_logs
          where organization_id = $1
            and path = '/api/v1/developer/sandbox/probe'
            and status >= 400",
    )
    .bind(organization_id)
    .fetch_one(harness.db.pool())
    .await
    .expect(
        "a refusal must be logged too: the guard publishes what it asked for on the error path, \
         which is why a 403 row names the scope to add instead of being anonymous",
    );
    assert_eq!(denied_row.0, 403);
    assert_eq!(
        denied_row.1.as_deref(),
        Some(PROBE_SCOPE),
        "the refusal names the scope the caller should have carried"
    );
    assert_eq!(
        denied_row.2, Some(narrow_id),
        "the refusal is attributed to the key that caused it, and to *that* key — a log that \
         attributes it to another key is worse than one that attributes it to nobody"
    );

    // The negative: the platform's own probes must never be logged. A probe that both reads and
    // writes is a probe that reports the platform down when the log table is unavailable, which
    // is why `should_log_path` refuses it — and this is what holds that decision to the wire.
    let before_probe: (i64,) =
        sqlx::query_as("select count(*) from api_request_logs where path like '/readyz%'")
            .fetch_one(harness.db.pool())
            .await
            .expect("the probe count must be readable");
    let _ = harness.call(get("/readyz", None)).await;
    let _ = harness.call(get("/healthz", None)).await;
    let after_probe: (i64,) =
        sqlx::query_as("select count(*) from api_request_logs where path like '/readyz%'")
            .fetch_one(harness.db.pool())
            .await
            .expect("the probe count must be readable");
    assert_eq!(
        before_probe.0, after_probe.0,
        "a readiness probe must never write a row: the log is a debugging surface, and a probe \
         that depends on it is a probe that takes the platform down with its own table"
    );

    // And the query string, on a real request through the real layer with a token on it.
    let leaky = harness
        .call(get(
            "/api/v1/developer/logs?status_class=4xx&access_token=super-secret-value",
            Some(&credential),
        ))
        .await;
    assert_eq!(leaky.status, StatusCode::OK);
    let leaked: (i64,) = sqlx::query_as(
        "select count(*) from api_request_logs where path like '%super-secret-value%'",
    )
    .fetch_one(harness.db.pool())
    .await
    .expect("the leak count must be readable");
    assert_eq!(
        leaked.0, 0,
        "a token in a query string is a credential, and this table has a CSV export. The \
         middleware passes `uri().path()` and the recorder strips again; this asserts the result \
         rather than trusting either of them."
    );

    // The screener's own state must be intact — a key that is still live, still un-revoked.
    let still_live: (Option<time::OffsetDateTime>,) =
        sqlx::query_as("select revoked_at from api_keys where id = $1")
            .bind(key_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the key row must still be there: revocation is a soft delete");
    assert_eq!(
        still_live.0, None,
        "logging a request must not touch the key itself"
    );

    harness.dispose().await;
}

/// The overview's numbers come from one snapshot and agree with the tables beside them.
///
/// The card row and the key list are two screens about one moment; a count read a second apart
/// from the list it sits above is a screen an operator cannot reason about. So the walk reads
/// both and compares, taking the database as the only third party.
#[tokio::test]
async fn the_overview_counts_the_same_keys_and_requests_the_tables_show() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let organization_id = create_organization_row(&harness.db).await;
    let (user, credential) = account(&harness, Some(organization_id)).await;
    grant(
        &harness,
        user,
        organization_id,
        &[
            "developer.read",
            "developer.keys.read",
            "developer.keys.manage",
            "developer.logs.read",
            PROBE_SCOPE,
        ],
    )
    .await;

    let (_first, _token) = create_key(&harness, &credential, "Counted", &[PROBE_SCOPE]).await;

    // One more session request, so the count the card reports is not dominated by the create
    // call above. (The create call was itself logged, which is the point: this count cannot be
    // zero on a screen sitting above a list the platform just wrote to.)
    let listed = harness
        .call(get("/api/v1/developer/api-keys", Some(&credential)))
        .await;
    assert_eq!(listed.status, StatusCode::OK, "the list must answer: {}", listed.text);

    let overview = harness
        .call(get("/api/v1/developer/overview", Some(&credential)))
        .await;
    assert_eq!(
        overview.status,
        StatusCode::OK,
        "the overview must answer: {}",
        overview.text
    );

    let counted: (i64, i64) = sqlx::query_as(
        "select
             (select count(*) from api_keys
               where organization_id = $1 and revoked_at is null
                 and (expires_at is null or expires_at > now())),
             (select count(*) from api_keys where organization_id = $1 and revoked_at is not null)",
    )
    .bind(organization_id)
    .fetch_one(harness.db.pool())
    .await
    .expect("the counts must be readable");

    assert_eq!(
        overview.body["keys"]["active"].as_i64(),
        Some(counted.0),
        "the card must agree with the key table: {}",
        overview.text
    );
    assert_eq!(
        overview.body["keys"]["revoked"].as_i64(),
        Some(counted.1),
        "the card must agree with the key table: {}",
        overview.text
    );
    assert_eq!(
        overview.body["keys"]["expired"].as_i64(),
        Some(0),
        "no key in this walk carries an expiry, so the expired card reads zero rather than \
         being absent"
    );

    let logged: (i64,) =
        sqlx::query_as("select count(*) from api_request_logs where organization_id = $1")
            .bind(organization_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the request count must be readable");
    assert!(
        logged.0 > 0,
        "the middleware must have logged the requests this walk made; a zero here means the layer \
         is not installed, and every number on the card is then a confident lie"
    );
    // **The card is exactly one row behind the table, and that is not a bug.** The overview
    // counts the rows that exist when its query runs; its *own* request is only written by the
    // middleware after the handler has answered. So a strict equality here fails by one on
    // every run — and "fixing" it by making the card add one to itself would be a lie that only
    // shows up when something else changed. Asserting the difference is the honest form: it says
    // the card counts *prior* traffic, which is the only thing a card can mean.
    assert_eq!(
        overview.body["requests_today"].as_i64(),
        Some(logged.0 - 1),
        "the card must count every row except its own: {}",
        overview.text
    );
    assert!(
        overview.body["errors_today"].as_i64().is_some(),
        "the error counter must be present even at zero — an absent field reads as a broken card"
    );

    // The retention window the screen prints is the one the log honours, and the failure list
    // carries no query string on its way out either.
    assert_eq!(
        overview.body["log_retention_days"].as_u64(),
        Some(u64::from(omnion_developer::logs_store::window_days())),
        "the screen must publish the window the table actually keeps"
    );
    let failures = overview.body["recent_failures"]
        .as_array()
        .expect("the failure list must be an array, not absent");
    assert!(
        failures.len() <= omnion_developer::RECENT_FAILURES,
        "the list is bounded so the overview stays glanceable"
    );
    for line in failures {
        let path = line["path"].as_str().unwrap_or_default();
        assert!(
            !path.contains('?'),
            "a failure line must never carry a query string, even on the way out: {path}"
        );
    }

    harness.dispose().await;
}

/// Every `/developer` route answers `403` to an account holding none of the keys.
#[tokio::test]
async fn every_developer_route_is_guarded() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let organization_id = create_organization_row(&harness.db).await;
    // A real organization member with **no** developer key. The developer.* family is not in the
    // base role (the catalogue says why), so this account is the realistic default user.
    let (_user, credential) = account(&harness, Some(organization_id)).await;

    for (method, uri) in [
        (Method::GET, "/api/v1/developer/scopes"),
        (Method::GET, "/api/v1/developer/api-keys"),
        (Method::POST, "/api/v1/developer/api-keys"),
        (
            Method::GET,
            "/api/v1/developer/api-keys/00000000-0000-0000-0000-000000000000",
        ),
        (
            Method::POST,
            "/api/v1/developer/api-keys/00000000-0000-0000-0000-000000000000/rotate",
        ),
        (
            Method::DELETE,
            "/api/v1/developer/api-keys/00000000-0000-0000-0000-000000000000",
        ),
        (Method::GET, "/api/v1/developer/logs"),
        (
            Method::GET,
            "/api/v1/developer/logs/1",
        ),
    ] {
        let response = harness
            .call(request(method.clone(), uri, Some(&credential), None))
            .await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{method} {uri} must be refused to a member with no developer key — the guard \\
             exists and is only ever satisfied otherwise: {}",
            response.text
        );
        assert!(
            response.text.contains("permission_denied"),
            "{method} {uri} must name the refusal: {}",
            response.text
        );
    }

    // And anonymous is `401`, not data.
    let anonymous = harness.call(get("/api/v1/developer/api-keys", None)).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    harness.dispose().await;
}



/// The migration applies to a **populated** database, not only to an empty one.
///
/// The criterion names two words and the second one is the whole test: a migration run against
/// an empty database only proves it can create a table, while every failure that stops a
/// platform from booting lives in the other direction. `0240` hangs foreign keys off `users`,
/// `organizations` and `media` rows that already exist, and its partial unique indexes are
/// created over tables that may already hold live keys — none of which an empty database can
/// exercise.
///
/// The populated state is built by **applying the migration, then writing rows, then removing
/// the ledger row and re-applying** — which is exactly what an upgrade from a build that
/// predates the migration looks like.
#[tokio::test]
async fn the_migration_applies_to_a_populated_database() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    // `Harness::fresh` has already migrated and seeded, so every referenced table has rows.
    let organization_id = create_organization_row(&harness.db).await;
    let (user, credential) = account(&harness, Some(organization_id)).await;
    grant(
        &harness,
        user,
        organization_id,
        &["developer.read", "developer.keys.read", "developer.keys.manage", PROBE_SCOPE],
    )
    .await;
    let (key_id, _token) = create_key(&harness, &credential, "Pre-existing", &[PROBE_SCOPE]).await;

    // Rows exist in all three referenced tables. If any of them were empty this walk would be
    // proving the empty case while claiming the populated one.
    let users: (i64,) = sqlx::query_as("select count(*) from users")
        .fetch_one(harness.db.pool()).await.expect("count users");
    assert!(users.0 > 0, "the populated half needs a user row");

    // Roll the ledger back to before `0240` and re-apply it over what is now a populated
    // database — the shape of every real upgrade.
    //
    // **The `0240` tables are dropped but their *siblings* are not.** That is the whole test:
    // an upgrade finds `organizations`, `users`, `sessions`, `role_bindings` and `audit_log`
    // full of rows and hangs foreign keys off them. Dropping only the new tables leaves that
    // populated state intact, and the first attempt at this walk dropped `api_keys` **and then
    // asserted a duplicate name against it** — so the "populated" database it claimed to build
    // had an empty `api_keys`, and the assertion it reached proved the opposite case.
    sqlx::query("delete from _sqlx_migrations where version >= 240")
        .execute(harness.db.pool()).await.expect("rewind the ledger");
    sqlx::query("drop table if exists api_key_usage_daily cascade")
        .execute(harness.db.pool()).await.expect("drop the rollup table");
    sqlx::query("drop table if exists api_request_logs cascade")
        .execute(harness.db.pool()).await.expect("drop the log table");
    sqlx::query("drop table if exists oauth_authorizations cascade")
        .execute(harness.db.pool()).await.expect("drop the authorizations table");
    sqlx::query("drop table if exists oauth_apps cascade")
        .execute(harness.db.pool()).await.expect("drop the apps table");
    // `api_keys` is dropped, and the row in it goes with it — which is why the duplicate-name
    // half of this walk re-creates one *after* the re-apply rather than expecting the original
    // to still be there.
    sqlx::query("drop table if exists api_keys cascade")
        .execute(harness.db.pool()).await.expect("drop the key table");

    // Proof that the database really was populated on the far side, over the tables the
    // migration's foreign keys hang off.
    for (table, expected) in [("users", users.0), ("organizations", 1)] {
        let count: (i64,) = sqlx::query_as(&format!("select count(*) from {table}"))
            .fetch_one(harness.db.pool()).await.expect("count the referenced table");
        assert!(
            count.0 >= expected,
            "{table} must still hold its {expected} row(s) — otherwise this walk is the empty \
             case wearing the populated one's name"
        );
    }

    harness.db.migrate().await.expect(
        "0240 must apply over an organization, a user, a role, a binding and a session that all \
         already exist — the foreign keys are the point",
    );

    // And the re-applied table still works, with the constraint a bare `create table` on an
    // empty database cannot check: a live key's name is unique. The first key of the pair is
    // created *after* the re-apply, so the assertion is against a table this migration built.
    let _first = create_key(&harness, &credential, "Pre-existing", &[PROBE_SCOPE]).await;
    let response = harness
        .call(post(
            "/api/v1/developer/api-keys",
            json!({ "name": "Pre-existing", "scopes": [PROBE_SCOPE] }),
            Some(&credential),
        ))
        .await;
    assert_eq!(
        response.status,
        StatusCode::CONFLICT,
        "the partial unique index must survive the re-apply: {}",
        response.text
    );

    harness.dispose().await;
}
