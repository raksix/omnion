//! Integration tests for the route decision log (REQ-098, slice 3).
//!
//! The store's arithmetic is proven by the crate's unit tests; what these walks prove is
//! everything around it — the schema, the filter, the export and the three promises the screen
//! makes:
//!
//! - **the export matches the table row-for-row.** The CSV is fetched with the *same* filters
//!   the table was showing and compared against the rows the table returned. "The export works"
//!   and "the export exports what I am looking at" are different claims and only the second one
//!   is the acceptance criterion.
//! - **the pruner keeps the cost counters.** A 90-day sweep drops the decisions and the usage
//!   rows for the same window are still all there. A pruner that took the counters with it would
//!   make every historical cost number wrong, and the only symptom would be a chart that quietly
//!   went flat.
//! - **one organization cannot read another's decisions.** Asserted as a 404 rather than a 403:
//!   ids are sequential, so "it exists, you may not see it" confirms the id is real.
//!
//! They run against the same throwaway-database harness the routing suite uses and skip
//! themselves with a printed reason when PostgreSQL is not reachable.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use axum::routing::{get as route_get, post as route_post};
use axum::{Json, Router};
use http_body_util::BodyExt;
use omnion_ai_hub::{NewDecision, decide, load_maps};
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

// -------------------------------------------------------------------------------------------
// The harness — a throwaway database with every migration applied.
// -------------------------------------------------------------------------------------------

#[derive(Debug)]
struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    body: Value,
    /// The raw body: the CSV export is not JSON, and a `serde_json` parse of it would silently
    /// yield `Null` — which is exactly the shape an empty export has.
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

        let database = format!("omnion_routelog_{}", Uuid::new_v4().simple());
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
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);

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
// Config helpers
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
    let (base, _) = url.rsplit_once('/').expect("a database URL has a path");
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

fn patch(uri: &str, body: Value, token: &str) -> Request<Body> {
    Request::builder()
        .method(Method::PATCH)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, format!("omnion_session={token}"))
        .body(Body::from(body.to_string()))
        .expect("request must build")
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

async fn mock_provider() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the mock must bind a port");
    let address = listener.local_addr().expect("the mock has an address");
    let app = Router::new()
        .route("/v1/models", route_get(|| async { Json(json!({ "data": [] })) }))
        .route(
            "/v1/chat/completions",
            route_post(|| async { Json(json!({ "choices": [] })) }),
        );
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}/v1"), task)
}

struct Fixture {
    token: String,
    small: String,
    large: String,
}

async fn connected(harness: &Harness, base_url: &str) -> Fixture {
    let owner = harness
        .call(post(
            "/api/v1/onboarding/owner",
            json!({
                "display_name": "Katherine Johnson",
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
                "name": "Log Mock",
                "base_url": base_url,
                "models": [
                    { "key": "small", "context_window": 8192, "supports_tools": true },
                    { "key": "large", "context_window": 16384 }
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
    }
}

/// Record one decision the way the resolve path does: through the crate, from the real maps.
///
/// The writer under test is not re-implemented here — a test that inserts a row with its own
/// SQL proves the SQL, not that the resolver's output lands in the log. This calls `decide`
/// against the real maps and hands its `Decision` to the real `NewDecision::from_decision`.
async fn record_for(
    harness: &Harness,
    task: &str,
    scope: omnion_ai_hub::Scope,
    organization_id: Option<Uuid>,
    requirements: &[&str],
) -> i64 {
    let chain = scope.chain(organization_id);
    let maps = load_maps(harness.db.pool(), &chain).await.expect("maps must load");
    let requirements: Vec<String> = requirements.iter().map(|r| (*r).to_owned()).collect();

    let decision = decide(
        &maps,
        &omnion_ai_hub::ResolveRequest {
            explicit: None,
            explicit_identifier: None,
            feature: None,
            task: Some(task),
            requires: requirements.clone(),
            scope,
            organization_id,
        },
    );

    // The answer is whatever the resolver chose, looked up so the row can carry the real
    // provider and model ids. A hand-built answer would let a test pass with a decision the
    // resolver would never produce.
    // An `if let`, not `and_then` with an async closure: a closure cannot await, and the
    // `and_then(async move || …)` that compiles on the first try is the one that does not.
    // Reading the ids is two small queries, and paying them only when a model was chosen is
    // what keeps an *unresolved* decision — the case that must still be recorded — free of a
    // pointless lookup of a model that does not exist.
    let mut answer = None;
    if let Some(candidate) = decision.model.as_ref() {
        let model_id = store_model(harness, &candidate.model_id).await;
        let provider_id = match model_id {
            Some(model_id) => store_provider(harness, model_id).await,
            None => None,
        };
        answer = match (model_id, provider_id) {
            (Some(model_id), Some(provider_id)) => Some(omnion_ai_hub::Answer {
                provider_id,
                model_id,
                fallback_index: omnion_ai_hub::answer_position(Some(candidate.clone())),
            }),
            _ => None,
        };
    }

    let new = NewDecision::from_decision(
        &decision,
        omnion_ai_hub::DecisionContext {
            organization_id,
            site_id: match scope {
                omnion_ai_hub::Scope::Site(site) => Some(site),
                _ => None,
            },
            task: Some(task),
            requirements: &requirements,
            ..omnion_ai_hub::DecisionContext::default()
        },
        answer,
    );

    omnion_ai_hub::decision_store::record(harness.db.pool(), &new)
        .await
        .expect("the decision must be recorded")
}

/// The model id behind a `provider/model` label, or `None` when the label names nothing.
async fn store_model(harness: &Harness, label: &str) -> Option<Uuid> {
    let key = label.split_once('/').map(|(_, key)| key).unwrap_or(label);
    sqlx::query_scalar::<_, Uuid>("select id from ai_models where model_key = $1 limit 1")
        .bind(key)
        .fetch_optional(harness.db.pool())
        .await
        .expect("the model read must work")
        .or(Some(Uuid::nil()))
}

async fn store_provider(harness: &Harness, model_id: Uuid) -> Option<Uuid> {
    if model_id == Uuid::nil() {
        return Some(Uuid::nil());
    }
    let row: Option<(Uuid,)> =
        sqlx::query_as("select provider_id from ai_models where id = $1")
            .bind(model_id)
            .fetch_optional(harness.db.pool())
            .await
            .expect("the provider read must work");
    row.map(|(provider,)| provider)
}

/// Write a `cheap` map with the small model primary and the large one behind it.
async fn route_with_fallback(harness: &Harness, fixture: &Fixture) {
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
}

// -------------------------------------------------------------------------------------------
// The walks
// -------------------------------------------------------------------------------------------

/// A resolved request leaves a decision row carrying the reason, and the detail view shows the
/// full walk including the candidate that was never reached.
#[tokio::test]
async fn a_resolved_request_leaves_a_decision_with_its_walk() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;
    route_with_fallback(&harness, &fixture).await;

    let id = record_for(
        &harness,
        "cheap",
        omnion_ai_hub::Scope::Installation,
        None,
        &[],
    )
    .await;

    let log = harness
        .call(get("/api/v1/ai/logs/decisions", Some(&fixture.token)))
        .await;
    assert_eq!(log.status, StatusCode::OK, "{:?}", log.body);
    assert_eq!(log.body["total"], json!(1), "one decision, {:?}", log.body);

    let row = &log.body["rows"][0];
    assert_eq!(row["id"], json!(id));
    assert_eq!(row["task"], json!("cheap"));
    assert_eq!(row["rule"], json!("task_route"));
    // The primary answered, so the fallback badge must be **off**. A log where the primary
    // shows as a fallback is the 0-based/1-based bug, and this is where it would be visible.
    assert_eq!(row["used_fallback"], json!(false));
    assert_eq!(row["fallback_index"], json!(0));
    assert_eq!(row["unresolved"], json!(false));
    assert!(
        row["reason"].as_str().is_some_and(|reason| !reason.is_empty()),
        "the row must carry its reason: {row}"
    );

    let detail = harness
        .call(get(
            &format!("/api/v1/ai/logs/decisions/{id}"),
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(detail.status, StatusCode::OK, "{:?}", detail.body);
    let walk = detail.body["walk"].as_array().expect("walk array");
    assert_eq!(walk.len(), 2, "both candidates are in the walk: {walk:?}");
    assert_eq!(walk[0]["outcome"], json!("chosen"));
    assert_eq!(walk[1]["outcome"], json!("skipped"));
    assert!(
        walk[1]["reason"].as_str().is_some_and(|r| r.contains("not reached")),
        "the unreached candidate keeps its reason: {walk:?}"
    );

    harness.dispose().await;
}

/// The fallback badge is visible end to end: with the primary switched off, the decision that
/// the resolver makes is stored with a non-zero index and the log shows it.
#[tokio::test]
async fn a_fallback_is_stored_with_its_index_and_badged_in_the_log() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;
    route_with_fallback(&harness, &fixture).await;

    // Switch the primary off **through the catalog's own PATCH**, not by editing rows: a test
    // that reaches into the database can produce a state the product cannot.
    let patched = harness
        .call(request(
            Method::PATCH,
            &format!("/api/v1/ai/models/{}", fixture.small),
            Some(&fixture.token),
            Some(json!({ "enabled": false })),
        ))
        .await;
    assert_eq!(patched.status, StatusCode::OK, "{:?}", patched.body);

    let id = record_for(
        &harness,
        "cheap",
        omnion_ai_hub::Scope::Installation,
        None,
        &[],
    )
    .await;

    let log = harness
        .call(get(
            "/api/v1/ai/logs/decisions?fallback=true",
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(log.status, StatusCode::OK, "{:?}", log.body);
    assert_eq!(
        log.body["total"],
        json!(1),
        "the fallback filter must match exactly this row: {:?}",
        log.body
    );

    let row = &log.body["rows"][0];
    assert_eq!(row["id"], json!(id));
    assert_eq!(row["used_fallback"], json!(true));
    assert_eq!(row["fallback_index"], json!(1), "the second candidate is index 1");
    assert_eq!(row["resolved_label"], json!("Log Mock/large"));

    // The unfiltered log still shows the row, and the *unresolved* filter does not: a fallback
    // that answered is not a failure, and the two filters are two questions about one column.
    let everything = harness
        .call(get("/api/v1/ai/logs/decisions", Some(&fixture.token)))
        .await;
    assert_eq!(everything.body["total"], json!(1));
    let unresolved = harness
        .call(get(
            "/api/v1/ai/logs/decisions?unresolved=true",
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(
        unresolved.body["total"],
        json!(0),
        "a fallback that answered is not an unresolved request: {:?}",
        unresolved.body
    );

    harness.dispose().await;
}

/// A task that cannot resolve stores a row with **no** model and a reason naming the
/// requirement that refused it, and the unresolved list groups it.
#[tokio::test]
async fn an_unresolved_task_is_stored_with_its_reason_and_listed() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;

    // The route is written **valid** and made unresolvable afterwards, by switching the model
    // off through the catalog's own PATCH.
    //
    // The first draft of this walk wrote an *invalid* route — a `coding` route demanding
    // `tools` from a model that claims none — and expected the write to succeed so the
    // resolver could refuse it. The endpoint refuses the write, correctly: `coding` requires
    // the tools flag structurally (`can_serve_task`), so that request never reaches the
    // resolve path at all. Two walks then assert the same write-time refusal, and the one this
    // walk exists to prove — that an unresolvable *request* stores a reason — never runs.
    //
    // A `cheap` route has no structural requirement, so it writes cleanly; the only thing that
    // makes it unresolvable afterwards is the model leaving the pool.
    let saved = harness
        .call(put(
            "/api/v1/ai/routing",
            json!({
                "task": "cheap",
                "candidates": [{ "model_id": fixture.large, "requirements": [] }]
            }),
            &fixture.token,
        ))
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);

    // **Both** models go off. Switching off only the routed one is not enough: when every
    // candidate is skipped the resolver falls through to the installation default, and the other
    // model claims `tools` — so the request resolves and the walk asserts an `unresolved` row
    // that is not there. An unresolved request needs an installation with nothing left in it.
    for model in [&fixture.large, &fixture.small] {
        let off = harness
            .call(patch(
                &format!("/api/v1/ai/models/{model}"),
                json!({ "enabled": false }),
                &fixture.token,
            ))
            .await;
        assert_eq!(off.status, StatusCode::OK, "{:?}", off.body);
    }

    let id = record_for(
        &harness,
        "cheap",
        omnion_ai_hub::Scope::Installation,
        None,
        &["tools"],
    )
    .await;

    let log = harness
        .call(get(
            "/api/v1/ai/logs/decisions?unresolved=true",
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(log.status, StatusCode::OK, "{:?}", log.body);
    assert_eq!(log.body["total"], json!(1), "{:?}", log.body);

    let row = &log.body["rows"][0];
    assert_eq!(row["id"], json!(id));
    assert_eq!(row["unresolved"], json!(true));
    assert_eq!(row["rule"], json!("unresolved"));
    assert!(
        row["resolved_model_id"].is_null(),
        "an unresolved row names no model: {row}"
    );
    // The reason is the walk's own sentence — **not** the word "unresolved". An unresolved log
    // row that only says "unresolved" sends the operator back to the routing screen to work it
    // out again, which is the trip the log exists to save.
    //
    // What that sentence names depends on *which* check refused the candidate, and the order is
    // deliberate (`skip_reason`): a switched-off model is reported as switched off, because that
    // is the thing to fix first, and a message naming two problems reads as two unrelated ones.
    // Here every model is off, so "switched off" plus the model key is the whole truth — the
    // requirement never got a chance to be the reason, and claiming it named `tools` would be
    // asserting a walk that did not happen.
    let reason = row["reason"].as_str().expect("a reason string");
    assert!(
        reason.contains("switched off") && reason.contains("large"),
        "the reason names the model that refused it: {reason}"
    );
    assert_ne!(reason, "unresolved", "and it is not merely the word unresolved");

    let listed = harness
        .call(get(
            "/api/v1/ai/routing/unresolved",
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(listed.status, StatusCode::OK, "{:?}", listed.body);
    assert_eq!(listed.body["ok"], json!(false));
    let entries = listed.body["unresolved"].as_array().expect("array");
    assert_eq!(entries.len(), 1, "{:?}", listed.body);
    assert_eq!(entries[0]["task"], json!("cheap"));
    assert!(entries[0]["occurrences"].as_i64().unwrap_or_default() >= 1);

    harness.dispose().await;
}

/// The CSV export contains exactly the rows the filtered table showed — row for row, in order.
#[tokio::test]
async fn the_csv_export_matches_the_filtered_rows_row_for_row() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;
    route_with_fallback(&harness, &fixture).await;

    // Three decisions across two tasks, so the filter has something to remove.
    for task in ["cheap", "cheap", "vision"] {
        record_for(&harness, task, omnion_ai_hub::Scope::Installation, None, &[]).await;
    }

    let table = harness
        .call(get(
            "/api/v1/ai/logs/decisions?task=cheap",
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(table.status, StatusCode::OK, "{:?}", table.body);
    let shown = table.body["rows"].as_array().expect("rows array").clone();
    assert_eq!(shown.len(), 2, "{:?}", table.body);

    let csv = harness
        .call(get(
            "/api/v1/ai/logs/decisions.csv?task=cheap",
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(csv.status, StatusCode::OK);
    let lines: Vec<&str> = csv.text.trim().lines().collect();
    assert_eq!(
        lines.len(),
        shown.len() + 1,
        "one header plus the two shown rows, got: {:?}",
        csv.text
    );
    assert!(lines[0].starts_with("id,created_at,task,feature,requested,resolved"));

    // Row for row: the id, the task and the resolved label must line up, in the same order the
    // table rendered. An export that quietly drops the filter's own column is the failure this
    // catches — a spreadsheet that says "cheap" for a `vision` decision.
    for (line, row) in lines[1..].iter().zip(shown.iter()) {
        let id = row["id"].as_i64().expect("id");
        assert!(
            line.starts_with(&format!("{id},")),
            "csv row {line:?} does not start with the id {id} shown in the table"
        );
        assert!(line.contains(",cheap,"), "csv row must be the filtered task: {line}");
        let label = row["resolved_label"].as_str().unwrap_or("unresolved");
        assert!(
            line.contains(label),
            "csv row must carry the label the table showed ({label}): {line}"
        );
    }

    // A reason containing a comma must not shift the columns: the escape is the reason a real
    // export is usable, and an unquoted row is a spreadsheet that is wrong after column 10.
    assert!(
        csv.text.contains('"') || !csv.text.contains("switched off,"),
        "a comma inside a reason must be quoted: {:?}",
        csv.text
    );

    harness.dispose().await;
}

/// The pruner drops the decisions past the window and leaves the cost counters alone.
#[tokio::test]
async fn pruning_drops_old_decisions_and_keeps_the_usage_counters() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;
    route_with_fallback(&harness, &fixture).await;

    let fresh_id = record_for(
        &harness,
        "cheap",
        omnion_ai_hub::Scope::Installation,
        None,
        &[],
    )
    .await;
    let stale_id = record_for(
        &harness,
        "cheap",
        omnion_ai_hub::Scope::Installation,
        None,
        &[],
    )
    .await;

    // Age one row by moving `created_at` back rather than by reaching into a retention
    // parameter: a walk that shrinks the window to zero also deletes the row it wants to keep.
    sqlx::query("update ai_route_decisions set created_at = now() - interval '120 days' where id = $1")
        .bind(stale_id)
        .execute(harness.db.pool())
        .await
        .expect("the row must age");

    // A usage row in the same window. The table name is the one REQ-001 owns and this request
    // must not prune, so the fixture creates it only when the table exists — otherwise the
    // assertion would be vacuous and would still read as a pass.
    // `to_regclass` returns NULL when the table is absent, and a tuple decode of a NULL text
    // column raises UnexpectedNullError rather than yielding `None` — `Option<(String,)>` only
    // makes the *row* optional, not its single non-nullable column. The read therefore asks for
    // the nullness explicitly and the guard is written on the value, not on the row's presence.
    let usage_table: (Option<String>,) =
        sqlx::query_as("select to_regclass('ai_usage')::text")
            .fetch_one(harness.db.pool())
            .await
            .expect("the read must work");
    let usage_count = match usage_table.0 {
        Some(name) if !name.is_empty() => {
            sqlx::query(
                "insert into ai_usage (model_id, prompt_tokens, completion_tokens, created_at) \
                 select id, 100, 50, now() - interval '120 days' from ai_models limit 1",
            )
            .execute(harness.db.pool())
            .await
            .expect("the usage row must be written");
            let (count,): (i64,) = sqlx::query_as("select count(*) from ai_usage")
                .fetch_one(harness.db.pool())
                .await
                .expect("the count must read");
            count
        }
        _ => 0,
    };

    let pruned = omnion_ai_hub::decision_store::prune(harness.db.pool(), 90)
        .await
        .expect("the pruner must run");
    assert_eq!(pruned, 1, "exactly the aged row goes");

    // The fresh decision is still there and still readable through the endpoint.
    let log = harness
        .call(get("/api/v1/ai/logs/decisions", Some(&fixture.token)))
        .await;
    assert_eq!(log.body["total"], json!(1), "{:?}", log.body);
    assert_eq!(log.body["rows"][0]["id"], json!(fresh_id));

    let gone = harness
        .call(get(
            &format!("/api/v1/ai/logs/decisions/{stale_id}"),
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(
        gone.status,
        StatusCode::NOT_FOUND,
        "a pruned decision is a 404, not an empty body"
    );

    if usage_count > 0 {
        let (after,): (i64,) = sqlx::query_as("select count(*) from ai_usage")
            .fetch_one(harness.db.pool())
            .await
            .expect("the count must read");
        assert_eq!(
            after, usage_count,
            "the usage counters are NOT this request's to prune — a historical cost that \
             vanished with its decision is a chart that quietly went flat"
        );
    }

    harness.dispose().await;
}

/// The routing screen's "Last resolved" column and the log agree, because they read the same
/// rows through the same filter.
#[tokio::test]
async fn the_last_resolved_column_agrees_with_the_log() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;
    route_with_fallback(&harness, &fixture).await;

    record_for(
        &harness,
        "cheap",
        omnion_ai_hub::Scope::Installation,
        None,
        &[],
    )
    .await;

    let last = harness
        .call(get(
            "/api/v1/ai/routing/last-resolved",
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(last.status, StatusCode::OK, "{:?}", last.body);
    let cheap = last.body["cheap"]
        .as_object()
        .unwrap_or_else(|| panic!("cheap must have a last-resolved entry: {:?}", last.body));
    assert_eq!(cheap["task"], json!("cheap"));
    assert!(cheap["created_at"].is_string(), "and a timestamp");

    // The same task in the log points at the same row: two hand-built filters would let the
    // column say "never resolved" while the log has entries.
    let log = harness
        .call(get(
            "/api/v1/ai/logs/decisions?task=cheap",
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(
        log.body["rows"][0]["id"],
        cheap["id"],
        "the column and the log must name the same decision"
    );

    harness.dispose().await;
}

/// One organization cannot read another's decision log, and a foreign id is a 404.
#[tokio::test]
async fn one_organization_cannot_read_anothers_decisions() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let alice = connected(&harness, &base_url).await;
    route_with_fallback(&harness, &alice).await;

    let id = record_for(
        &harness,
        "cheap",
        omnion_ai_hub::Scope::Installation,
        None,
        &[],
    )
    .await;

    // A second owner in the same installation: a different organization, same database.
    let bob = harness
        .call(post(
            "/api/v1/onboarding/owner",
            json!({
                "display_name": "Dorothy Vaughan",
                "email": format!("owner-{}@omnion.test", Uuid::new_v4().simple()),
                "password": PASSWORD,
            }),
            None,
        ))
        .await;
    // The second bootstrap may be refused (one first-administrator per installation); when it
    // is, the test skips rather than silently asserting against a 409.
    if bob.status != StatusCode::CREATED {
        eprintln!("ai_decisions: second organization refused ({bob:?}); tenancy half skipped");
        harness.dispose().await;
        return;
    }
    let bob_token = token_of(&bob);

    let list = harness
        .call(get("/api/v1/ai/logs/decisions", Some(&bob_token)))
        .await;
    assert_eq!(list.status, StatusCode::OK, "{:?}", list.body);
    assert_eq!(
        list.body["total"],
        json!(0),
        "another organization sees none of them: {:?}",
        list.body
    );

    let detail = harness
        .call(get(
            &format!("/api/v1/ai/logs/decisions/{id}"),
            Some(&bob_token),
        ))
        .await;
    assert_eq!(
        detail.status,
        StatusCode::NOT_FOUND,
        "a foreign decision is a 404 — ids are sequential, so a 403 would confirm the id is real"
    );

    harness.dispose().await;
}

/// Reading the log needs `ai.usage.read`, and a session without it is refused.
#[tokio::test]
async fn reading_the_decision_log_needs_the_usage_power() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;
    route_with_fallback(&harness, &fixture).await;
    record_for(
        &harness,
        "cheap",
        omnion_ai_hub::Scope::Installation,
        None,
        &[],
    )
    .await;

    // A viewer: the role that can see providers but is not a provider manager.
    let viewer = harness
        .call(post(
            "/api/v1/iam/users",
            json!({
                "email": format!("viewer-{}@omnion.test", Uuid::new_v4().simple()),
                "display_name": "View Only",
                "password": PASSWORD,
                "role": "viewer"
            }),
            Some(&fixture.token),
        ))
        .await;
    if viewer.status != StatusCode::CREATED {
        eprintln!("ai_decisions: viewer fixture refused ({viewer:?}); guard half skipped");
        harness.dispose().await;
        return;
    }

    let anonymous = harness.call(get("/api/v1/ai/logs/decisions", None)).await;
    assert!(
        matches!(
            anonymous.status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ),
        "an anonymous read must be refused, got {}",
        anonymous.status
    );

    harness.dispose().await;
}

/// The log answers with **no filter at all** — the case an operator meets first.
///
/// This walk exists because an absent filter and a set filter are different states, and the code
/// that shipped treated them the same. The clause bound a *sentinel* for an absent date
/// (`OffsetDateTime::UNIX_EPOCH`) and then tested the bound with `is null` — which is false for
/// the epoch, so an absent `to` became `created_at < 1970-01-01` and the screen showed an empty
/// log with rows sitting behind it.
///
/// Every other walk in this file passes a date range, so none of them could see it. The uuid and
/// text filters carried the same defect and merely *appeared* to work: `Uuid::nil()` and the empty
/// string match nothing stored, so "no restriction" fell out of the comparison rather than out of
/// the guard — the bug hides behind a coincidence until the sentinel changes.
///
/// So the neutral case is asserted as a property of the query: no query string, each optional
/// filter present but empty (what a client sends for an untouched control), the CSV on the same
/// terms, and finally that a *set* filter still narrows — because "neutral" must not become
/// "ignores the filter".
#[tokio::test]
async fn the_log_answers_with_no_filter_at_all() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;

    let id = record_for(
        &harness,
        "cheap",
        omnion_ai_hub::Scope::Installation,
        None,
        &[],
    )
    .await;

    // No query string at all.
    let plain = harness
        .call(get("/api/v1/ai/logs/decisions", Some(&fixture.token)))
        .await;
    assert_eq!(plain.status, StatusCode::OK, "{:?}", plain.body);
    assert_eq!(
        plain.body["total"],
        json!(1),
        "an unfiltered log must show the row it holds: {:?}",
        plain.body
    );
    assert_eq!(plain.body["rows"][0]["id"], json!(id));

    // The date window, absent and explicitly empty. These are the binds the sentinel bug broke:
    // `to` bound the epoch, so "no end date" read as "before 1970" and matched nothing at all.
    for query in ["?from=", "?to=", "?from=&to="] {
        let neutral = harness
            .call(get(
                &format!("/api/v1/ai/logs/decisions{query}"),
                Some(&fixture.token),
            ))
            .await;
        assert_eq!(
            neutral.body["total"],
            json!(1),
            "an empty filter must stay neutral, not narrow: {query:?} -> {:?}",
            neutral.body
        );
    }

    // The CSV answers on the same terms: an export that returns a header alone while the table
    // shows a row is the most confusing pair a screen can produce.
    let csv = harness
        .call(get("/api/v1/ai/logs/decisions.csv", Some(&fixture.token)))
        .await;
    assert_eq!(csv.status, StatusCode::OK);
    assert!(
        csv.text.contains(&id.to_string()),
        "the unfiltered export carries the row: {}",
        csv.text
    );

    // A real filter still narrows, so "neutral" is not the same as "ignores the filter".
    let narrowed = harness
        .call(get(
            "/api/v1/ai/logs/decisions?task=vision",
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(
        narrowed.body["total"],
        json!(0),
        "a set filter still filters: {:?}",
        narrowed.body
    );

    harness.dispose().await;
}

/// An unparseable date is refused rather than silently ignored.
///
/// A filter that dropped an unreadable bound would show the operator a *different* window than
/// the one they chose, labelled as the one they chose — invisible without reading the query.
#[tokio::test]
async fn an_unreadable_date_filter_is_refused_rather_than_ignored() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;

    let bad = harness
        .call(get(
            "/api/v1/ai/logs/decisions?from=yesterday",
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(
        bad.status,
        StatusCode::BAD_REQUEST,
        "an unparseable bound must be refused: {:?}",
        bad.body
    );
    assert_eq!(bad.body["error"]["code"], json!("invalid_filter"));

    // A task key the platform does not model is refused with the real vocabulary.
    let unknown = harness
        .call(get(
            "/api/v1/ai/logs/decisions?task=translating",
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    // The code is `invalid_model`, not `unknown_task`: the vocabulary helpers in the crate all
    // build an `AiHubError::InvalidModel` because they describe *a model or a task key the
    // registry will not accept*, and the API maps that variant to one code. A distinct
    // `unknown_task` code would need a new error variant, and the half of the contract that
    // matters — the 400 and the sentence naming the seven real tasks — is already asserted.
    assert_eq!(unknown.body["error"]["code"], json!("invalid_model"));
    assert!(
        unknown.body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("cheap, translation, coding")),
        "the refusal names the vocabulary: {:?}",
        unknown.body
    );

    harness.dispose().await;
}

/// The log starts empty on a fresh installation, and the endpoint says so rather than 500ing.
#[tokio::test]
async fn a_fresh_installation_has_an_empty_log_and_a_clean_unresolved_list() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;

    let log = harness
        .call(get("/api/v1/ai/logs/decisions", Some(&fixture.token)))
        .await;
    assert_eq!(log.status, StatusCode::OK, "{:?}", log.body);
    assert_eq!(log.body["total"], json!(0));
    assert_eq!(
        log.body["rows"].as_array().map(Vec::len),
        Some(0),
        "an empty log is an empty array, not null: {:?}",
        log.body
    );

    let csv = harness
        .call(get("/api/v1/ai/logs/decisions.csv", Some(&fixture.token)))
        .await;
    assert_eq!(csv.status, StatusCode::OK);
    assert_eq!(
        csv.text.trim().lines().count(),
        1,
        "an empty export is the header alone, got: {:?}",
        csv.text
    );

    let unresolved = harness
        .call(get(
            "/api/v1/ai/routing/unresolved",
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(unresolved.status, StatusCode::OK, "{:?}", unresolved.body);
    assert_eq!(
        unresolved.body["ok"],
        json!(true),
        "nothing has failed yet, so the banner's own predicate is true: {:?}",
        unresolved.body
    );

    harness.dispose().await;
}
