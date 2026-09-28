//! Integration tests for task routing and feature overrides (REQ-098, slice 2).
//!
//! The resolver's own rules are proven by the crate's unit tests; what these walks prove is
//! everything around them — the schema, the scope arithmetic against real rows, the validation
//! on the write path, and the two promises the endpoints make:
//!
//! - **the dry run performs zero provider calls.** The provider in this file counts the requests
//!   it receives in a shared counter; the preview is asked to resolve against a map that names
//!   models this provider serves, and the counter must still read zero afterwards. Asserting
//!   "it looks like it does not call out" is not the same claim.
//! - **a rejected PUT leaves the previous map intact.** A write that fails validation half-way
//!   would leave a site with no primary and an installation with a stale one; the walk writes a
//!   good map, then a bad one, then reads the map back.
//!
//! They run against the same throwaway-database harness the catalog suite uses and skip
//! themselves with a printed reason when PostgreSQL is not reachable.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use axum::routing::{get as route_get, post as route_post};
use axum::{Json, Router};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::net::TcpListener;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

// -------------------------------------------------------------------------------------------
// A provider that counts every request it receives, so "zero provider calls" is measurable.
// -------------------------------------------------------------------------------------------

/// How many requests the mock provider has answered. Read after a dry run to prove the preview
/// never dialled out — a claim that cannot be counted is a claim that cannot be tested.
static PROVIDER_CALLS: AtomicUsize = AtomicUsize::new(0);

async fn counted(_body: String) -> Json<Value> {
    PROVIDER_CALLS.fetch_add(1, Ordering::SeqCst);
    Json(json!({ "object": "list", "data": [{ "id": "small" }, { "id": "large" }] }))
}

async fn mock_provider() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the mock must bind a port");
    let address = listener.local_addr().expect("the mock has an address");
    let app = Router::new()
        .route("/v1/models", route_get(counted))
        .route("/v1/chat/completions", route_post(counted));

    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{address}/v1"), task)
}

// -------------------------------------------------------------------------------------------
// The harness — a throwaway database with every migration applied.
// -------------------------------------------------------------------------------------------

struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    body: Value,
    /// The raw body, kept for the case a refusal is not JSON (a guard's plain-text 401, say) so
    /// the assertion can still read what actually came back.
    #[allow(dead_code)]
    text: String,
}

struct Harness {
    state: AppState,
    db: Db,
    maintenance: Db,
    database: String,
}

impl Harness {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().expect("environment must be valid");
        if live_db(&config).await.is_none() {
            return None;
        }

        let database = format!("omnion_rout_{}", Uuid::new_v4().simple());
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
        })
    }

    async fn call(&self, request: Request<Body>) -> TestResponse {
        let response = routes::router(self.state.clone())
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
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };

        TestResponse {
            status,
            set_cookie,
            body,
            text,
        }
    }

    async fn dispose(self) {
        self.db.pool().close().await;
        let database = self.database;
        sqlx::query(&format!("drop database if exists \"{database}\" with (force)"))
            .execute(self.maintenance.pool())
            .await
            .expect("the temporary database must be removed");
    }
}

// -------------------------------------------------------------------------------------------
// Config helpers (mirroring the catalog harness).
// -------------------------------------------------------------------------------------------

async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&DatabaseConfig {
        url: config.database.url.clone(),
        max_connections: 1,
    })
    .await
    {
        Ok(db) => Some(db),
        Err(_) => None,
    }
}

fn maintenance_config(config: &Config) -> DatabaseConfig {
    let url = config.database.url.clone();
    let (base, _) = url
        .rsplit_once('/')
        .expect("a database URL has a path");
    DatabaseConfig {
        url: format!("{base}/postgres"),
        max_connections: 1,
    }
}

fn swap_database(url: &str, database: &str) -> String {
    let (base, _) = url.rsplit_once('/').expect("a database URL has a path");
    format!("{base}/{database}")
}

fn token_of(response: &TestResponse) -> String {
    let cookie = response.set_cookie.as_deref().expect("a cookie must be set");
    cookie
        .split(';')
        .next()
        .expect("cookie has a value")
        .split_once('=')
        .expect("cookie is name=value")
        .1
        .to_owned()
}

fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match token {
        Some(token) => builder.header(header::COOKIE, format!("omnion_session={token}")),
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

fn get(uri: &str, token: Option<&str>) -> Request<Body> {
    request(Method::GET, uri, token, None)
}

fn post(uri: &str, body: Value, token: Option<&str>) -> Request<Body> {
    request(Method::POST, uri, token, Some(body))
}

fn put(uri: &str, body: Value, token: &str) -> Request<Body> {
    request(Method::PUT, uri, Some(token), Some(body))
}

// -------------------------------------------------------------------------------------------
// Fixtures
// -------------------------------------------------------------------------------------------

/// A signed-in owner with a provider and three models: a tooled one, a plain one and a
/// wide-context one. The flags are what the capability rules read, so they are part of the
/// fixture rather than an afterthought.
struct Fixture {
    token: String,
    small: String,
    large: String,
    wide: String,
}

async fn connected(harness: &Harness, base_url: &str) -> Fixture {
    let owner = harness
        .call(post(
            "/api/v1/onboarding/owner",
            json!({
                "display_name": "Grace Hopper",
                "email": format!("owner-{}@omnion.test", Uuid::new_v4().simple()),
                "password": PASSWORD,
            }),
            None,
        ))
        .await;
    assert_eq!(owner.status, StatusCode::CREATED, "{:?}", owner.body);
    let token = token_of(&owner);

    let created = harness
        .call(post(
            "/api/v1/ai/providers",
            json!({
                "name": "Route Mock",
                "base_url": base_url,
                "models": [
                    { "key": "small", "context_window": 8192, "supports_tools": true },
                    { "key": "large", "context_window": 16384 },
                    { "key": "wide", "context_window": 200000, "supports_tools": true }
                ]
            }),
            Some(&token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);

    let models = harness.call(get("/api/v1/ai/models", Some(&token))).await;
    let list = models.body["models"].as_array().expect("models array");
    let id_of = |key: &str| {
        list.iter()
            .find(|entry| entry["model_key"] == key)
            .map(|entry| entry["id"].as_str().expect("model id").to_owned())
            .unwrap_or_else(|| panic!("{key} must be registered"))
    };

    Fixture {
        token,
        small: id_of("small"),
        large: id_of("large"),
        wide: id_of("wide"),
    }
}

/// The candidates of one task in a routing response.
fn candidates_of<'a>(body: &'a Value, task: &str) -> &'a Vec<Value> {
    body["tasks"]
        .as_array()
        .expect("tasks array")
        .iter()
        .find(|row| row["task"] == task)
        .map(|row| row["candidates"].as_array().expect("candidates array"))
        .unwrap_or_else(|| panic!("{task} must have a row"))
}

/// The walk of a preview response, as `(model_id, outcome, reason)` triples.
fn walk_of(body: &Value) -> Vec<(Value, String, String)> {
    body["walk"]
        .as_array()
        .expect("walk array")
        .iter()
        .map(|entry| {
            (
                entry["model_id"].clone(),
                entry["outcome"].as_str().unwrap_or_default().to_owned(),
                entry["reason"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

// -------------------------------------------------------------------------------------------
// The walks
// -------------------------------------------------------------------------------------------

/// The dry run resolves the map and performs **no provider call**.
///
/// The counter is read before and after so the assertion is about the preview's own effect: a
/// provider that was never contacted by *this* request cannot be confused with one that was.
#[tokio::test]
async fn the_dry_run_resolves_a_map_without_calling_a_provider() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;

    let saved = harness
        .call(put(
            "/api/v1/ai/routing",
            json!({
                "task": "cheap",
                "candidates": [
                    { "model_id": fixture.small, "requirements": [] },
                    { "model_id": fixture.large, "requirements": [] }
                ]
            }),
            &fixture.token,
        ))
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);

    let before = PROVIDER_CALLS.load(Ordering::SeqCst);
    let preview = harness
        .call(post(
            "/api/v1/ai/routing/preview",
            json!({ "task": "cheap" }),
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(preview.status, StatusCode::OK, "{:?}", preview.body);
    let after = PROVIDER_CALLS.load(Ordering::SeqCst);

    assert_eq!(after, before, "the dry run must not dial a provider");
    assert_eq!(preview.body["rule"], json!("task_route"));
    assert_eq!(preview.body["model"]["model_id"], json!("small"));
    assert_eq!(preview.body["unresolved"], json!(false));
    // The primary and the unreached fallback are both in the walk, with their reasons.
    let walk = walk_of(&preview.body);
    assert_eq!(walk.len(), 2, "both candidates appear: {walk:?}");
    assert_eq!(walk[0].1, "chosen");
    assert_eq!(walk[1].1, "skipped");
    assert!(walk[1].2.contains("not reached"), "got: {}", walk[1].2);

    harness.dispose().await;
}

/// A fresh install can write its **first** route map.
///
/// This is the walk that was missing when the browser pass found the bug: the store inserted
/// `scope_key`, which migration 0045 declares `generated always as (...)`, and PostgreSQL
/// refuses any INSERT that names a generated column. The existing routing walks never caught
/// it because they all ran against a branch where 0045 had not been applied yet, so every
/// write in this file went down a path the production schema does not have. The write now
/// names only the two ids and lets the database derive the scope — the property that makes a
/// read and a write unable to disagree about what a scope is.
#[tokio::test]
async fn a_fresh_install_can_write_its_first_route_map() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;

    // Nothing is configured yet, and the first write must not touch a generated column.
    let before = harness
        .call(get("/api/v1/ai/routing", Some(&fixture.token)))
        .await;
    assert_eq!(before.status, StatusCode::OK, "{:?}", before.body);
    assert!(
        candidates_of(&before.body, "cheap").is_empty(),
        "a fresh install has no cheap route yet: {:?}",
        before.body
    );

    let saved = harness
        .call(put(
            "/api/v1/ai/routing",
            json!({
                "task": "cheap",
                "candidates": [{ "model_id": fixture.small, "requirements": [] }]
            }),
            &fixture.token,
        ))
        .await;
    assert_eq!(
        saved.status,
        StatusCode::OK,
        "the first route write was refused: {:?}",
        saved.body
    );
    assert_eq!(saved.body["error"].as_object(), None, "and it carried no error");

    // The row is really in the table, under the scope the database derived.
    let (count,): (i64,) =
        sqlx::query_as("select count(*) from ai_task_routes where task = 'cheap' and scope_key = 'installation'")
            .fetch_one(harness.db.pool())
            .await
            .expect("the row must be readable");
    assert_eq!(count, 1, "the derived scope must be the installation's");

    harness.dispose().await;
}

/// A two-fallback route degrades to the first fallback when the primary is switched off, and
/// the walk says which requirement/candidate caused the skip.
#[tokio::test]
async fn disabling_the_primary_degrades_to_the_first_fallback() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;

    harness
        .call(put(
            "/api/v1/ai/routing",
            json!({
                "task": "critical",
                "candidates": [
                    { "model_id": fixture.small, "requirements": ["tools"] },
                    { "model_id": fixture.large, "requirements": [] },
                    { "model_id": fixture.wide, "requirements": [] }
                ]
            }),
            &fixture.token,
        ))
        .await;

    // The primary answers while it is on.
    let before = harness
        .call(post(
            "/api/v1/ai/routing/preview",
            json!({ "task": "critical", "requires": ["tools"] }),
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(before.body["model"]["model_id"], json!("small"));
    assert_eq!(before.body["model"]["position"], json!(1));

    // Switch it off through the catalog's own PATCH, not by editing rows.
    let disabled = harness
        .call(request(
            Method::PATCH,
            &format!("/api/v1/ai/models/{}", fixture.small),
            Some(&fixture.token),
            Some(json!({ "enabled": false })),
        ))
        .await;
    assert_eq!(disabled.status, StatusCode::OK, "{:?}", disabled.body);

    let after = harness
        .call(post(
            "/api/v1/ai/routing/preview",
            json!({ "task": "critical", "requires": ["tools"] }),
            Some(&fixture.token),
        ))
        .await;
    let chosen = &after.body["model"];
    assert_eq!(chosen["model_id"], json!("wide"), "the first *usable* fallback answers");
    assert_eq!(chosen["position"], json!(3));

    // The walk carries all three candidates and names the skip.
    let walk = walk_of(&after.body);
    assert_eq!(walk.len(), 3, "every candidate is walked: {walk:?}");
    assert_eq!(walk[0].1, "skipped");
    assert!(walk[0].2.contains("switched off"), "got: {}", walk[0].2);
    assert!(
        walk[0].2.contains("small"),
        "the skip names the model, got: {}",
        walk[0].2
    );

    harness.dispose().await;
}

/// The resolution order is exact: a feature pin beats the task route, and the task route beats
/// the installation default — each asserted with a fixture that differs only in the rule under
/// test.
#[tokio::test]
async fn a_feature_pin_beats_the_task_route_and_the_route_beats_the_default() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;

    // Both configured at once, so only the order can decide.
    harness
        .call(put(
            "/api/v1/ai/routing",
            json!({
                "task": "cheap",
                "candidates": [{ "model_id": fixture.small, "requirements": [] }]
            }),
            &fixture.token,
        ))
        .await;
    let pinned = harness
        .call(put(
            "/api/v1/ai/routing/overrides",
            json!({ "feature": "copilot", "model_id": fixture.large }),
            &fixture.token,
        ))
        .await;
    assert_eq!(pinned.status, StatusCode::OK, "{:?}", pinned.body);

    // The feature wins over the task.
    let with_feature = harness
        .call(post(
            "/api/v1/ai/routing/preview",
            json!({ "task": "cheap", "feature": "copilot" }),
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(with_feature.body["rule"], json!("feature_override"));
    assert_eq!(with_feature.body["model"]["model_id"], json!("large"));

    // Without the feature, the task route answers (over the installation default).
    let task_only = harness
        .call(post(
            "/api/v1/ai/routing/preview",
            json!({ "task": "cheap" }),
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(task_only.body["rule"], json!("task_route"));
    assert_eq!(task_only.body["model"]["model_id"], json!("small"));

    // And an unrelated task with no map falls through to the installation default.
    let defaulted = harness
        .call(post(
            "/api/v1/ai/routing/preview",
            json!({ "task": "translation" }),
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(defaulted.body["rule"], json!("installation_default"));

    harness.dispose().await;
}

/// A routing PUT naming a model that cannot serve the task is **refused**, naming the task,
/// the candidate and the requirement — and the previous map survives the refusal.
#[tokio::test]
async fn a_capability_incompatible_route_is_refused_and_the_old_map_survives() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;

    // A good map first, so there is something to lose.
    harness
        .call(put(
            "/api/v1/ai/routing",
            json!({
                "task": "coding",
                "candidates": [{ "model_id": fixture.small, "requirements": ["tools"] }]
            }),
            &fixture.token,
        ))
        .await;

    // `large` claims no tools flag, and the coding task needs one.
    let refused = harness
        .call(put(
            "/api/v1/ai/routing",
            json!({
                "task": "coding",
                "candidates": [{ "model_id": fixture.large, "requirements": [] }]
            }),
            &fixture.token,
        ))
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{:?}", refused.body);
    let message = refused.body["message"].as_str().unwrap_or_default();
    assert!(message.contains("coding"), "names the task: {message}");
    assert!(message.contains("large"), "names the candidate: {message}");
    assert!(
        message.contains("tools"),
        "names the requirement: {message}"
    );

    // The refused write left the good map in place — a half-applied map is the state nobody can
    // explain from the panel.
    let read = harness.call(get("/api/v1/ai/routing", Some(&fixture.token))).await;
    let candidates = candidates_of(&read.body, "coding");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0]["model_id"], json!(fixture.small));

    harness.dispose().await;
}

/// Scopes do not leak: a site map is invisible to the installation, and two sites of one
/// organization diverge.
#[tokio::test]
async fn a_site_map_does_not_change_the_installation_or_a_sibling_site() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;

    // Create an organization and two sites directly — the tenancy rows are the substrate the
    // composite foreign key asserts against.
    let organization: (Uuid,) = sqlx::query_as(
        "insert into organizations (id, name, slug) values (gen_random_uuid(), 'Acme', $1) returning id",
    )
    .bind(format!("acme-{}", Uuid::new_v4().simple()))
    .fetch_one(harness.db.pool())
    .await
    .expect("the organization must be created");
    let organization = organization.0;

    // A plain function rather than an async closure: a closure that captured the pool would
    // move it out of `harness`, and the walk needs `harness` again for every call afterwards.
    async fn create_site(pool: &sqlx::PgPool, organization: Uuid, key: &str) -> Uuid {
        let row: (Uuid,) = sqlx::query_as(
            "insert into sites (organization_id, key, name) values ($1, $2, $2) returning id",
        )
        .bind(organization)
        .bind(key)
        .fetch_one(pool)
        .await
        .expect("the site must be created");
        row.0
    }
    let site_one = create_site(harness.db.pool(), organization, "one").await;
    let site_two = create_site(harness.db.pool(), organization, "two").await;

    // A site map for `cheap` naming only `wide`.
    let saved = harness
        .call(put(
            "/api/v1/ai/routing",
            json!({
                "task": "cheap",
                "site_id": site_one,
                "candidates": [{ "model_id": fixture.wide, "requirements": [] }]
            }),
            &fixture.token,
        ))
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);

    // Site one answers with its own model.
    let at_one = harness
        .call(post(
            "/api/v1/ai/routing/preview",
            json!({ "task": "cheap", "site_id": site_one }),
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(at_one.body["model"]["model_id"], json!("wide"));
    assert_eq!(at_one.body["model"]["scope"], json!({"site": site_one.to_string()}));

    // Site two, same organization, different answer.
    let at_two = harness
        .call(post(
            "/api/v1/ai/routing/preview",
            json!({ "task": "cheap", "site_id": site_two }),
            Some(&fixture.token),
        ))
        .await;
    assert_ne!(
        at_two.body["model"]["model_id"],
        json!("wide"),
        "a sibling site must not inherit the other site's map"
    );

    // The installation is untouched by the site's write.
    let at_install = harness.call(get("/api/v1/ai/routing", Some(&fixture.token))).await;
    let candidates = candidates_of(&at_install.body, "cheap");
    assert!(
        candidates.is_empty(),
        "a site-scoped write must not appear in the installation map"
    );

    harness.dispose().await;
}

/// An organization override, then its removal, restores the inherited model without touching
/// the task map.
#[tokio::test]
async fn removing_an_override_restores_the_inherited_model() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;

    // An installation-level pin, then an organization-level one that shadows it.
    harness
        .call(put(
            "/api/v1/ai/routing/overrides",
            json!({ "feature": "summarize", "model_id": fixture.small }),
            &fixture.token,
        ))
        .await;
    let organization: (Uuid,) = sqlx::query_as(
        "insert into organizations (id, name, slug) values (gen_random_uuid(), 'Beta', $1) returning id",
    )
    .bind(format!("beta-{}", Uuid::new_v4().simple()))
    .fetch_one(harness.db.pool())
    .await
    .expect("the organization must be created");
    let organization = organization.0;

    harness
        .call(put(
            "/api/v1/ai/routing/overrides",
            json!({ "feature": "summarize", "model_id": fixture.large, "organization_id": organization }),
            &fixture.token,
        ))
        .await;

    let shadowed = harness
        .call(post(
            "/api/v1/ai/routing/preview",
            json!({ "feature": "summarize", "organization_id": organization }),
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(shadowed.body["model"]["model_id"], json!("large"));

    // Remove the organization pin: the installation one is inherited again.
    let removed = harness
        .call(put(
            "/api/v1/ai/routing/overrides",
            json!({ "feature": "summarize", "model_id": null, "organization_id": organization }),
            &fixture.token,
        ))
        .await;
    assert_eq!(removed.status, StatusCode::OK, "{:?}", removed.body);

    let restored = harness
        .call(post(
            "/api/v1/ai/routing/preview",
            json!({ "feature": "summarize", "organization_id": organization }),
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(restored.body["model"]["model_id"], json!("small"));
    assert_eq!(restored.body["model"]["scope"], json!("installation"));

    harness.dispose().await;
}

/// An unknown task, feature or requirement is refused with the vocabulary that does exist.
#[tokio::test]
async fn an_unknown_key_is_refused_with_the_real_vocabulary() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;

    let task = harness
        .call(put(
            "/api/v1/ai/routing",
            json!({ "task": "fast", "candidates": [] }),
            &fixture.token,
        ))
        .await;
    assert_eq!(task.status, StatusCode::BAD_REQUEST, "{:?}", task.body);
    let message = task.body["message"].as_str().unwrap_or_default();
    assert!(message.contains("fast"), "{message}");
    assert!(message.contains("long_context"), "names the real tasks: {message}");

    let feature = harness
        .call(put(
            "/api/v1/ai/routing/overrides",
            json!({ "feature": "translation", "model_id": fixture.small }),
            &fixture.token,
        ))
        .await;
    assert_eq!(feature.status, StatusCode::BAD_REQUEST, "{:?}", feature.body);
    assert!(
        feature.body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("copilot"),
        "names the real features: {:?}",
        feature.body
    );

    let requirement = harness
        .call(post(
            "/api/v1/ai/routing/preview",
            json!({ "task": "cheap", "requires": ["speed"] }),
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(requirement.status, StatusCode::BAD_REQUEST, "{:?}", requirement.body);
    assert!(
        requirement.body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("tools, vision, long_context, json"),
        "names the four requirements: {:?}",
        requirement.body
    );

    harness.dispose().await;
}

/// A fresh install's routing screen lists all seven tasks with an empty state, and the
/// unresolved list names them — the empty state is the thing a screen gets wrong.
#[tokio::test]
async fn a_fresh_install_lists_every_task_as_unresolved() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;

    let read = harness.call(get("/api/v1/ai/routing", Some(&fixture.token))).await;
    assert_eq!(read.status, StatusCode::OK, "{:?}", read.body);

    let tasks = read.body["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 7, "every task has a row, configured or not");
    for task in tasks {
        assert_eq!(task["candidates"].as_array().expect("candidates").len(), 0);
        assert_eq!(task["empty"], json!(true));
        assert_eq!(task["inherited"], json!(false));
    }

    let unresolved = read.body["unresolved"].as_array().expect("unresolved array");
    assert_eq!(unresolved.len(), 7, "all seven need attention: {unresolved:?}");

    // The panel's legend is served, not hard-coded, so it cannot drift from the resolver.
    let rules = read.body["rules"].as_array().expect("rules array");
    assert_eq!(
        rules,
        &vec![
            json!("explicit"),
            json!("feature_override"),
            json!("task_route"),
            json!("installation_default"),
            json!("unresolved"),
        ]
    );

    harness.dispose().await;
}

/// Reading the map needs `ai.providers.read`; writing it needs `ai.settings.manage`, and a
/// caller without the write power must not be able to redirect traffic.
#[tokio::test]
async fn writing_a_route_map_needs_the_settings_power() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;

    // The owner holds both powers, so the write succeeds for them.
    let allowed = harness
        .call(put(
            "/api/v1/ai/routing",
            json!({
                "task": "cheap",
                "candidates": [{ "model_id": fixture.small, "requirements": [] }]
            }),
            &fixture.token,
        ))
        .await;
    assert_eq!(allowed.status, StatusCode::OK, "{:?}", allowed.body);

    // An anonymous caller is refused before the handler runs.
    let anonymous = harness
        .call(request(
            Method::PUT,
            "/api/v1/ai/routing",
            None,
            Some(json!({ "task": "cheap", "candidates": [] })),
        ))
        .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "{:?}", anonymous.body);

    harness.dispose().await;
}

/// A model that is removed from the registry leaves its route row with a null candidate that
/// the walk explains and the panel marks as needing attention.
#[tokio::test]
async fn a_removed_model_leaves_a_null_candidate_the_panel_marks() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;

    harness
        .call(put(
            "/api/v1/ai/routing",
            json!({
                "task": "cheap",
                "candidates": [
                    { "model_id": fixture.small, "requirements": [] },
                    { "model_id": fixture.large, "requirements": [] }
                ]
            }),
            &fixture.token,
        ))
        .await;

    // Delete the primary model's row directly: the `on delete set null` is the migration's
    // promise, and a disabled model would not exercise it.
    sqlx::query("delete from ai_models where id = $1::uuid")
        .bind(&fixture.small)
        .execute(harness.db.pool())
        .await
        .expect("the model row must be deletable");

    let read = harness.call(get("/api/v1/ai/routing", Some(&fixture.token))).await;
    let candidates = candidates_of(&read.body, "cheap");
    assert_eq!(candidates.len(), 2, "the row survives the removal");
    assert_eq!(candidates[0]["model_id"], Value::Null);
    assert_eq!(candidates[0]["needs_attention"], json!(true));
    assert!(
        candidates[0]["refusal"]
            .as_str()
            .unwrap_or_default()
            .contains("removed"),
        "the panel is told why: {:?}",
        candidates[0]
    );

    // The preview still answers with the surviving fallback and explains the hole.
    let preview = harness
        .call(post(
            "/api/v1/ai/routing/preview",
            json!({ "task": "cheap" }),
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(preview.body["model"]["model_id"], json!("large"));
    let walk = walk_of(&preview.body);
    assert!(
        walk[0].2.contains("removed"),
        "the walk explains the null candidate: {:?}",
        walk[0]
    );

    harness.dispose().await;
}
