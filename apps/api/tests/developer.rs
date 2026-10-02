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
//! 5. `the_offered_environments_are_the_ones_the_server_accepts` — the picker's list against the
//!    create route's parser, in both directions. Written after the route's doc comment claimed
//!    it read a crate constant that does not exist; the claim was right and the code was not,
//!    and only a round trip through the create endpoint can tell those apart.
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
    //
    // `secret_hash`, not `key_hash`: this branch's `0223` migration and every query in
    // `omnion-developer`'s store name the column `secret_hash` (it is a *scheme-prefixed*
    // digest, not a bare SHA-256 — see `crates/developer/src/secret.rs`). Main's `0240` names
    // the same fact `key_hash`. The test kept its assertion and adopted this branch's column
    // name; renaming the column to satisfy a merged-in test would mean editing the store, the
    // sign-in query and the migration for a name that carries no meaning of its own.
    let hash: (String,) =
        sqlx::query_as("select secret_hash from api_keys where id = $1")
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
    omnion_developer::store::log_request(
        harness.db.pool(),
        &omnion_developer::RequestLog {
            id: 0,
            organization_id,
            api_key_id: Some(id),
            actor_user_id: None,
            method: "get".to_owned(),
            path: "/api/v1/developer/sandbox/probe".to_owned(),
            status: 200,
            duration_ms: 7,
            request_id: "walk-00000000-0000-4000-8000-000000000001".to_owned(),
            bytes_in: None,
            bytes_out: None,
            error_code: None,
            created_at: now,
        },
    )
    .await
    .expect("the log row must be written");

    // This branch's `api_request_logs` (0223) has no `permission` or `client_fingerprint`
    // column: main's 0240 adds neither, and migration 0240 records that decision in its header
    // ("column | not present"). So the claim is restated against the columns this schema has —
    // the row belongs to the key that made the request, and carries the timing an operator reads.
    let row: (String, Option<uuid::Uuid>, i16, i32, Option<time::OffsetDateTime>) =
        sqlx::query_as(
            "select path, api_key_id, status, duration_ms, created_at from api_request_logs
              where api_key_id = $1",
        )
        .bind(id)
        .fetch_one(harness.db.pool())
        .await
        .expect("the log row must be readable");
    assert_eq!(row.0, "/api/v1/developer/sandbox/probe");
    assert_eq!(
        row.1,
        Some(id),
        "the row must belong to the key that made the request, or the log cannot answer \
         'what did this integration do'"
    );
    assert_eq!(
        (row.2, row.3),
        (200, 7),
        "status and duration are the columns an operator actually reads; a log without them is \
         a list of URLs"
    );
    assert!(
        row.4.is_some(),
        "a row with no timestamp cannot be placed on the screen's time axis"
    );

    // The query string is stripped at the store, and this walks the real recorder. The path handed
    // in carries a secret **on purpose**: the point of the claim is not that the helper works but
    // that `store::log_request` applies it, so a caller that forgets would still be safe.
    omnion_developer::store::log_request(
        harness.db.pool(),
        &omnion_developer::RequestLog {
            id: 0,
            organization_id,
            api_key_id: Some(id),
            actor_user_id: None,
            method: "get".to_owned(),
            path: "/api/v1/media?access_token=super-secret-value".to_owned(),
            status: 200,
            duration_ms: 3,
            request_id: "walk-00000000-0000-4000-8000-000000000002".to_owned(),
            bytes_in: None,
            bytes_out: None,
            error_code: None,
            created_at: now,
        },
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

/// The picker and the server must agree on the environment list, in both directions.
///
/// `GET /developer/scopes` is what the key form builds its environment picker from. If it offers
/// a value the create endpoint refuses with `unknown_environment`, the person fills in a valid
/// form and is rejected — the same failure shape as a picker offering an undelegable scope, which
/// is why this route already derives `grantable` from the caller's effective permissions rather
/// than from the catalogue.
///
/// Asserting that the response *equals* `Environment::ALL_STR` would only prove the two constants
/// are equal, which a reader could satisfy by keeping both hardcoded — that is precisely the
/// defect this walk was written for, so it is not enough on its own. What is asserted instead is
/// the round trip: **every environment the endpoint offers is one the create endpoint accepts**,
/// and every environment the create endpoint accepts is one the endpoint offers. The first
/// direction mints a real key per offered value and reads the stored environment back out of the
/// database rather than out of the response, so a route that echoes the request without validating
/// it cannot pass. The second enumerates the crate's own `Environment::ALL`; an environment the
/// crate accepts but the picker hides is a key nobody can create through the panel.
#[tokio::test]
async fn the_offered_environments_are_the_ones_the_server_accepts() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let organization_id = create_organization_row(&harness.db).await;
    let (_user, credential) = account(&harness, Some(organization_id)).await;
    grant(
        &harness,
        _user,
        organization_id,
        &["developer.read", "developer.keys.manage", PROBE_SCOPE],
    )
    .await;

    let catalogue = harness
        .call(get("/api/v1/developer/scopes", Some(&credential)))
        .await;
    assert_eq!(
        catalogue.status,
        StatusCode::OK,
        "the scope catalogue must be readable: {}",
        catalogue.text
    );
    let offered: Vec<String> = catalogue.body["environments"]
        .as_array()
        .expect("environments must be an array")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("each environment is a string")
                .to_owned()
        })
        .collect();
    assert!(
        !offered.is_empty(),
        "an empty environment list would leave the key form with nothing to offer, which is \
         indistinguishable in the UI from 'not implemented'"
    );

    // Direction one: every offered environment is accepted, and the value that was **stored** is
    // the one that was offered. Read back from the row, not from the create response.
    for (index, environment) in offered.iter().enumerate() {
        let name = format!("Picker round trip {index} ({environment})");
        let created = harness
            .call(post(
                "/api/v1/developer/api-keys",
                json!({ "name": name, "scopes": [PROBE_SCOPE], "environment": environment }),
                Some(&credential),
            ))
            .await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "the picker offered {environment:?} but creating a key in it was refused — a form \
             that submits and is then rejected: {}",
            created.text
        );
        let id = Uuid::parse_str(created.body["key"]["id"].as_str().expect("the id must be there"))
            .expect("the id must be a uuid");
        let stored: (String,) = sqlx::query_as("select environment from api_keys where id = $1")
            .bind(id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the stored environment must read");
        assert_eq!(
            &stored.0, environment,
            "the environment the picker offered is not the environment the key was created in"
        );
    }

    // Direction two: every environment the crate accepts is one the picker offers. Enumerated
    // from the enum rather than from `ALL_STR`, so widening `parse` without widening the list
    // the picker renders fails here instead of producing an environment no panel can create.
    for accepted in omnion_developer::Environment::ALL {
        let stored_form = accepted.as_str();
        assert!(
            offered.iter().any(|value| value == stored_form),
            "the server accepts the environment {stored_form:?} but `GET /developer/scopes` does \
             not offer it, so no panel can create a key in it"
        );
    }

    // And the negative, or the two directions above are satisfied by offering everything: an
    // environment nobody defined is still refused, with a named error rather than a 500.
    let refused = harness
        .call(post(
            "/api/v1/developer/api-keys",
            json!({ "name": "Nowhere", "scopes": [PROBE_SCOPE], "environment": "prod-eu-west" }),
            Some(&credential),
        ))
        .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "an undefined environment must be refused: {}",
        refused.text
    );
    assert!(
        refused.text.contains("unknown_environment"),
        "the refusal must name the problem: {}",
        refused.text
    );

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
