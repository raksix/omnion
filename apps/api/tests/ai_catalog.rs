//! Integration tests for the model catalog (docs/requests/REQ-098, slice 1).
//!
//! The catalog is the screen an operator reads to answer three questions: what can this model
//! do, what does it cost, and what will ever ask for it. These walks prove the answers come
//! from the *stored* row and that they stay honest under edit:
//!
//! - a price edit changes what the **next** request is estimated at, and never re-prices a call
//!   that already happened (the request's central accounting invariant);
//! - a half-priced model reports an **unknown** cost rather than the half it knows;
//! - the capability chips narrow the listing server-side, and an unknown flag is a refusal
//!   rather than a silently narrower table;
//! - a negative or zero price is refused with both halves named;
//! - an unknown status filter is refused rather than answered with an empty list.
//!
//! They run against the same throwaway-database harness the AI hub suite uses and skip
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
use sqlx::Row;
use tokio::net::TcpListener;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

// -------------------------------------------------------------------------------------------
// A provider that answers `/models` and one chat, so the walk is end to end rather than mocked
// at the database.
// -------------------------------------------------------------------------------------------

async fn mock_models() -> Json<Value> {
    Json(json!({
        "object": "list",
        "data": [
            { "id": "mock-small" },
            { "id": "mock-large" }
        ]
    }))
}

async fn mock_chat_silent() -> Json<Value> {
    Json(json!({}))
}

/// Start a mock provider and return its base URL.
async fn mock_provider() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the mock must bind a port");
    let address = listener.local_addr().expect("the mock has an address");
    let app = Router::new()
        .route("/v1/models", route_get(mock_models))
        .route("/v1/chat/completions", route_post(mock_chat_silent));

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

        let database = format!("omnion_cat_{}", Uuid::new_v4().simple());
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
        None => builder
            .body(Body::empty())
            .expect("request must build"),
    }
}

fn get(uri: &str, token: Option<&str>) -> Request<Body> {
    request(Method::GET, uri, token, None)
}

fn post(uri: &str, body: Value, token: Option<&str>) -> Request<Body> {
    request(Method::POST, uri, token, Some(body))
}

fn patch(uri: &str, body: Value, token: &str) -> Request<Body> {
    request(Method::PATCH, uri, Some(token), Some(body))
}

/// Connect a provider with two models and return `(session token, provider id, model ids)`.
async fn connected(harness: &Harness, base_url: &str) -> (String, String, String, String) {
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
                "name": "Catalog Mock",
                "base_url": base_url,
                "models": [
                    { "key": "mock-small", "context_window": 32768, "supports_tools": true },
                    { "key": "mock-large", "context_window": 131072 }
                ]
            }),
            Some(&token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let provider_id = created.body["id"].as_str().expect("provider id").to_owned();

    let models = harness
        .call(get("/api/v1/ai/models", Some(&token)))
        .await;
    let list = models.body["models"].as_array().expect("models array");
    let by_key = |key: &str| {
        list.iter()
            .find(|entry| entry["model_key"] == key)
            .map(|entry| entry["id"].as_str().expect("model id").to_owned())
            .unwrap_or_else(|| panic!("{key} must be registered"))
    };

    (token, provider_id, by_key("mock-small"), by_key("mock-large"))
}

/// The models in a listing response, as `(model_key, price)` pairs.
fn rows_of(body: &Value) -> Vec<(String, Value)> {
    body["models"]
        .as_array()
        .expect("models array")
        .iter()
        .map(|entry| {
            (
                entry["model_key"].as_str().expect("model key").to_owned(),
                entry["price"].clone(),
            )
        })
        .collect()
}

// -------------------------------------------------------------------------------------------
// The walks
// -------------------------------------------------------------------------------------------

/// A price edit changes the cost the catalog reports, and both halves are stamped with today.
#[tokio::test]
async fn a_price_edit_lands_and_both_halves_are_reported() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let (token, _provider, small, _large) = connected(&harness, &base_url).await;

    // A freshly discovered model has no price at all — not a price of zero.
    let before = harness.call(get("/api/v1/ai/models", Some(&token))).await;
    let (_, price) = rows_of(&before.body)
        .into_iter()
        .find(|(key, _)| key == "mock-small")
        .expect("mock-small is registered");
    assert_eq!(price["input_micros_per_mtok"], Value::Null);
    assert_eq!(price["output_micros_per_mtok"], Value::Null);
    assert_eq!(price["complete"], json!(false));
    assert_eq!(price["age_days"], Value::Null, "an unwritten price has no age");

    // Write one.
    let saved = harness
        .call(patch(
            &format!("/api/v1/ai/models/{small}"),
            json!({
                "input_cost_micros_per_mtok": 15_000,
                "output_cost_micros_per_mtok": 60_000,
                "price_source": "manual"
            }),
            &token,
        ))
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
    assert_eq!(saved.body["price"]["input_micros_per_1k"], json!(15));
    assert_eq!(saved.body["price"]["output_micros_per_1k"], json!(60));
    assert_eq!(saved.body["price"]["complete"], json!(true));
    assert_eq!(saved.body["price"]["source"], json!("manual"));

    // The listing agrees with the PATCH — this is the case where a client filtered its own
    // list and would have kept showing the old number.
    let after = harness.call(get("/api/v1/ai/models", Some(&token))).await;
    let (_, price) = rows_of(&after.body)
        .into_iter()
        .find(|(key, _)| key == "mock-small")
        .expect("mock-small is still registered");
    assert_eq!(price["input_micros_per_1k"], json!(15));
    assert_eq!(price["age_days"], json!(0), "a price written now is zero days old");
    assert_eq!(price["stale"], json!(false));

    harness.dispose().await;
}

/// The request's central invariant: a price edit re-prices the **next** request and nothing
/// else. The usage row a call already wrote keeps the token counts it recorded.
#[tokio::test]
async fn a_price_edit_never_re_prices_a_call_that_already_happened() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let (token, provider_id, small, _large) = connected(&harness, &base_url).await;

    harness
        .call(patch(
            &format!("/api/v1/ai/models/{small}"),
            json!({
                "input_cost_micros_per_mtok": 1_000,
                "output_cost_micros_per_mtok": 2_000
            }),
            &token,
        ))
        .await;

    // What a call costs is derived from the price *at the moment it ran*. This row is the
    // evidence that a later price edit cannot reach back into it: the token counts are stored,
    // and nothing recomputes them from the catalog.
    //
    // **What this walk does not prove, and where the real proof is.** It asserts the count of
    // usage rows is unchanged by a price edit, and it does so on a provider that has served
    // *nothing* — `before.0 == 0` right below. A table with no rows cannot be retro-edited, so
    // the assertion would also pass against an implementation that re-derived every cost from
    // the current price. The half that actually matters — a row written *before* the edit still
    // totalling what it was billed — is
    // `a_price_edit_moves_new_requests_and_leaves_a_written_history_alone`, which writes one.
    // Both walks stay: this one proves a price edit touches no usage row at all, which is the
    // other half of "only new requests".
    let before: (i64, Option<i64>) = sqlx::query_as(
        "select count(*)::bigint, sum(prompt_tokens)::int from ai_provider_usage \
         where provider_id = $1",
    )
    .bind(uuid::Uuid::parse_str(&provider_id).expect("provider id is a uuid"))
    .fetch_one(harness.db.pool())
    .await
    .expect("the usage query must run");
    assert_eq!(before.0, 0, "no call has been made yet");

    // Raise the price a hundredfold.
    harness
        .call(patch(
            &format!("/api/v1/ai/models/{small}"),
            json!({
                "input_cost_micros_per_mtok": 100_000,
                "output_cost_micros_per_mtok": 200_000
            }),
            &token,
        ))
        .await;

    // The usage table is untouched, and the catalog carries the new number — the two are
    // separate on purpose, because a bill that changes after the fact is worse than one that
    // was approximately right.
    let after: (i64, Option<i64>) = sqlx::query_as(
        "select count(*)::bigint, sum(prompt_tokens)::int from ai_provider_usage \
         where provider_id = $1",
    )
    .bind(uuid::Uuid::parse_str(&provider_id).expect("provider id is a uuid"))
    .fetch_one(harness.db.pool())
    .await
    .expect("the usage query must run");
    assert_eq!(after.0, before.0, "a price edit writes no usage row");
    assert_eq!(after.1, before.1, "a price edit rewrites no token count");

    let listing = harness.call(get("/api/v1/ai/models", Some(&token))).await;
    let (_, price) = rows_of(&listing.body)
        .into_iter()
        .find(|(key, _)| key == "mock-small")
        .expect("mock-small is registered");
    assert_eq!(price["input_micros_per_1k"], json!(100), "the new price is live");

    harness.dispose().await;
}

/// A half-priced model is honestly half-priced: `complete` is false and the missing half is
/// null rather than a zero that would read as free.
#[tokio::test]
async fn a_half_priced_model_reports_an_unknown_cost_not_a_cheap_one() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let (token, _provider, small, _large) = connected(&harness, &base_url).await;

    let saved = harness
        .call(patch(
            &format!("/api/v1/ai/models/{small}"),
            json!({ "input_cost_micros_per_mtok": 5_000 }),
            &token,
        ))
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);

    let price = &saved.body["price"];
    assert_eq!(price["input_micros_per_1k"], json!(5));
    assert_eq!(price["output_micros_per_1k"], Value::Null, "not zero — unknown");
    assert_eq!(price["complete"], json!(false));

    // And it is still erasable: clearing the half that exists leaves a model with no price at
    // all, which is the state a mis-typed price can be undone from.
    let cleared = harness
        .call(patch(
            &format!("/api/v1/ai/models/{small}"),
            json!({ "input_cost_micros_per_mtok": null }),
            &token,
        ))
        .await;
    assert_eq!(cleared.status, StatusCode::OK, "{:?}", cleared.body);
    assert_eq!(cleared.body["price"]["input_micros_per_mtok"], Value::Null);
    assert_eq!(
        cleared.body["price"]["age_days"],
        Value::Null,
        "a cleared price has no age"
    );

    harness.dispose().await;
}

/// A price that is not a price is refused, and the refusal names **both** halves when both are
/// wrong — an operator who typed two bad numbers should not have to save twice to find out.
#[tokio::test]
async fn a_bad_price_is_refused_with_both_halves_named() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let (token, _provider, small, _large) = connected(&harness, &base_url).await;

    for (label, body) in [
        ("negative", json!({ "input_cost_micros_per_mtok": -1 })),
        ("zero", json!({ "output_cost_micros_per_mtok": 0 })),
        (
            "absurd",
            json!({ "input_cost_micros_per_mtok": 2_000_000_000_i64 }),
        ),
    ] {
        let refused = harness
            .call(patch(
                &format!("/api/v1/ai/models/{small}"),
                body,
                &token,
            ))
            .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{label} must be refused: {:?}",
            refused.body
        );
        let message = refused.text.to_lowercase();
        assert!(message.contains("micros per million"), "{label}: {message}");
    }

    // Both halves wrong at once, one message.
    let both = harness
        .call(patch(
            &format!("/api/v1/ai/models/{small}"),
            json!({ "input_cost_micros_per_mtok": -1, "output_cost_micros_per_mtok": 0 }),
            &token,
        ))
        .await;
    assert_eq!(both.status, StatusCode::BAD_REQUEST, "{:?}", both.body);
    let message = both.text.to_lowercase();
    assert!(message.contains("input"), "{message}");
    assert!(message.contains("output"), "{message}");

    // Nothing landed: a refused write leaves the stored price empty, not half-applied. A
    // half-applied price would pair a new input rate with an old output rate and neither is
    // what anybody typed.
    let listing = harness.call(get("/api/v1/ai/models", Some(&token))).await;
    let (_, price) = rows_of(&listing.body)
        .into_iter()
        .find(|(key, _)| key == "mock-small")
        .expect("mock-small is registered");
    assert_eq!(price["input_micros_per_mtok"], Value::Null);
    assert_eq!(price["output_micros_per_mtok"], Value::Null);

    harness.dispose().await;
}

/// The capability chips narrow the **listing**, and the filter is a conjunction: a row shows
/// only when it claims every selected flag.
#[tokio::test]
async fn the_capability_chips_narrow_the_listing_server_side() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let (token, _provider, _small, _large) = connected(&harness, &base_url).await;

    // Every model: both.
    let all = harness.call(get("/api/v1/ai/models", Some(&token))).await;
    assert_eq!(rows_of(&all.body).len(), 2);

    // Only mock-small claims tools.
    let tools = harness
        .call(get("/api/v1/ai/models?capability=tools", Some(&token)))
        .await;
    assert_eq!(tools.status, StatusCode::OK, "{:?}", tools.body);
    let keys: Vec<String> = rows_of(&tools.body).into_iter().map(|(key, _)| key).collect();
    assert_eq!(keys, vec!["mock-small".to_owned()], "the filter narrows the server's list");

    // A conjunction, not a disjunction: asking for tools and vision — neither model claims
    // both — selects nothing. A disjunction would have returned both and hidden the fact that
    // neither can do the other half.
    let both = harness
        .call(get(
            "/api/v1/ai/models?capability=tools,vision",
            Some(&token),
        ))
        .await;
    assert_eq!(both.status, StatusCode::OK, "{:?}", both.body);
    assert!(
        rows_of(&both.body).is_empty(),
        "a conjunction of two unclaimed flags selects nothing"
    );

    // An empty selection is no filter at all — a cleared chip row must not empty the table.
    let cleared = harness
        .call(get("/api/v1/ai/models?capability=", Some(&token)))
        .await;
    assert_eq!(rows_of(&cleared.body).len(), 2);

    // An unknown flag is a refusal, not a quietly narrower table.
    let unknown = harness
        .call(get(
            "/api/v1/ai/models?capability=tools,telepathy",
            Some(&token),
        ))
        .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST, "{:?}", unknown.body);

    harness.dispose().await;
}

/// Search reaches the model key, the display name and the provider name, and a blank search is
/// no filter rather than an empty table.
#[tokio::test]
async fn search_reaches_the_key_the_label_and_the_provider() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let (token, _provider, _small, _large) = connected(&harness, &base_url).await;

    for (label, needle, expected) in [
        ("key", "SMALL", 1),
        ("provider", "catalog", 2),
        ("absent", "nothing-here", 0),
    ] {
        let found = harness
            .call(get(
                &format!("/api/v1/ai/models?q={needle}"),
                Some(&token),
            ))
            .await;
        assert_eq!(found.status, StatusCode::OK, "{label}: {:?}", found.body);
        assert_eq!(
            rows_of(&found.body).len(),
            expected,
            "{label} search: {:?}",
            found.body
        );
    }

    // A blank needle is not a search that matches nothing.
    let blank = harness
        .call(get("/api/v1/ai/models?q=%20%20", Some(&token)))
        .await;
    assert_eq!(rows_of(&blank.body).len(), 2, "a blank search is no filter");

    harness.dispose().await;
}

/// The status filter separates the two, an unknown value is a refusal, and sorting puts an
/// unpriced model last rather than first.
#[tokio::test]
async fn the_status_filter_and_the_price_order_behave() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let (token, _provider, small, _large) = connected(&harness, &base_url).await;

    // Switch one off.
    harness
        .call(patch(
            &format!("/api/v1/ai/models/{small}"),
            json!({ "enabled": false }),
            &token,
        ))
        .await;

    let enabled = harness
        .call(get("/api/v1/ai/models?status=enabled", Some(&token)))
        .await;
    let disabled = harness
        .call(get("/api/v1/ai/models?status=disabled", Some(&token)))
        .await;
    let keys_enabled: Vec<String> = rows_of(&enabled.body).into_iter().map(|(k, _)| k).collect();
    let keys_disabled: Vec<String> = rows_of(&disabled.body).into_iter().map(|(k, _)| k).collect();
    assert_eq!(keys_enabled, vec!["mock-large".to_owned()]);
    assert_eq!(keys_disabled, vec!["mock-small".to_owned()]);

    // Both is both.
    let all = harness.call(get("/api/v1/ai/models", Some(&token))).await;
    assert_eq!(rows_of(&all.body).len(), 2);

    // An unknown status is a refusal: an empty list would be indistinguishable from an empty
    // registry, and the operator would conclude the wrong thing.
    let unknown = harness
        .call(get("/api/v1/ai/models?status=perhaps", Some(&token)))
        .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST, "{:?}", unknown.body);

    // Price order: an unpriced model is not the cheapest one, so it sorts last.
    //
    // **Both** models are unpriced here, so the price columns tie and the **model key** is the
    // tiebreak — which is `mock-large` before `mock-small`, alphabetically. The expectation here
    // used to say the opposite while its own failure message said "the key order is the
    // tiebreak": the assertion contradicted the rule printed beside it, and it only passed
    // earlier by accident, because the inverted `None`-first sort returned insertion order that
    // happened to match it. `an_unpriced_model_sorts_last_on_price` is the test that actually
    // pins the ordering, and it has one priced model and one that is not.
    let by_price = harness
        .call(get("/api/v1/ai/models?sort=price", Some(&token)))
        .await;
    assert_eq!(by_price.status, StatusCode::OK, "{:?}", by_price.body);
    let keys: Vec<String> = rows_of(&by_price.body).into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        keys,
        vec!["mock-large".to_owned(), "mock-small".to_owned()],
        "with no price at all the key order is the tiebreak, and `mock-large` sorts first"
    );

    // An unknown sort key falls back rather than erroring: a stale bookmark should not turn a
    // working table into an error page.
    let unknown_sort = harness
        .call(get("/api/v1/ai/models?sort=by_vibes", Some(&token)))
        .await;
    assert_eq!(unknown_sort.status, StatusCode::OK, "{:?}", unknown_sort.body);

    harness.dispose().await;
}

/// An unpriced model sorts *after* a priced one, which is the whole point of ordering by cost.
#[tokio::test]
async fn an_unpriced_model_sorts_last_on_price() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let (token, _provider, _small, large) = connected(&harness, &base_url).await;

    harness
        .call(patch(
            &format!("/api/v1/ai/models/{large}"),
            json!({
                "input_cost_micros_per_mtok": 10_000,
                "output_cost_micros_per_mtok": 30_000
            }),
            &token,
        ))
        .await;

    let by_price = harness
        .call(get("/api/v1/ai/models?sort=price", Some(&token)))
        .await;
    let keys: Vec<String> = rows_of(&by_price.body).into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        keys,
        vec!["mock-large".to_owned(), "mock-small".to_owned()],
        "the priced model comes first; the unpriced one is not 'free'"
    );

    harness.dispose().await;
}

/// The catalog needs a session, like every other AI surface.
#[tokio::test]
async fn the_catalog_asks_for_a_session() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    assert_eq!(
        harness.call(get("/api/v1/ai/models", None)).await.status,
        StatusCode::UNAUTHORIZED
    );
    harness.dispose().await;
}

/// REQ-098 slice 5 — **a price edit moves new requests only.** A usage row that was *written*
/// keeps the
/// cost it was billed, and the row after the edit is priced at the new number.
///
/// This is the acceptance criterion the whole cost snapshot exists for, and the way to prove it
/// is to make the two sides of the claim in one walk: the *old* row's stored total is read back
/// after the catalog's own PATCH has changed the price, and compared to a figure computed by
/// hand from the old rate. A screen that re-derived history would render the new number here, so
/// asserting only "the old row still exists" would pass against the exact implementation the
/// criterion forbids.
///
/// The two calls go through `record_usage` with an explicit snapshot rather than a live chat,
/// because the criterion is about the *store* and a live call would couple it to a mock
/// provider's token reporting. The snapshot is passed the way the runtime passes it, so the
/// arithmetic under test is the same one production uses.
#[tokio::test]
async fn a_price_edit_moves_new_requests_and_leaves_a_written_history_alone() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let (token, provider, small, _large) = connected(&harness, &base_url).await;

    // The operator prices the model for the first time: 1_000 micros in, 3_000 out.
    let priced = harness
        .call(patch(
            &format!("/api/v1/ai/models/{small}"),
            json!({
                "input_cost_micros_per_mtok": 1_000,
                "output_cost_micros_per_mtok": 3_000
            }),
            &token,
        ))
        .await;
    assert_eq!(priced.status, StatusCode::OK, "{:?}", priced.body);

    let provider_uuid = Uuid::parse_str(&provider).expect("a provider id is a uuid");
    let price_at_call_time = omnion_ai_hub::cost::ModelPrice {
        input_micros_per_mtok: Some(1_000),
        output_micros_per_mtok: Some(3_000),
    };

    // One call, 200_000 prompt and 100_000 completion tokens: 200_000/1e6 * 1_000 = 200 micros
    // in, 100_000/1e6 * 3_000 = 300 micros out, 500 in total. The counts are chosen so both
    // sides are whole numbers and the total is a figure a human can re-derive by hand — the
    // point of the assertion is that the stored value did not change, not that the arithmetic
    // is hard.
    let first = record_cost(
        &harness,
        provider_uuid,
        "mock-small",
        price_at_call_time,
        Some(200_000),
        Some(100_000),
    )
    .await;

    // The operator notices the output price was a factor of three too high.
    let corrected = harness
        .call(patch(
            &format!("/api/v1/ai/models/{small}"),
            json!({ "output_cost_micros_per_mtok": 30_000 }),
            &token,
        ))
        .await;
    assert_eq!(corrected.status, StatusCode::OK, "{:?}", corrected.body);

    // The next identical call is billed at the new price: 200 in + 3_000 out = 3_200 micros.
    let second = record_cost(
        &harness,
        provider_uuid,
        "mock-small",
        omnion_ai_hub::cost::ModelPrice {
            input_micros_per_mtok: Some(1_000),
            output_micros_per_mtok: Some(30_000),
        },
        Some(200_000),
        Some(100_000),
    )
    .await;

    // The load-bearing assertion: the row written BEFORE the edit still totals what it was
    // billed. Reading it after the edit is the whole point — a reader that joined the catalog
    // for the current price would now report 3 here.
    let first_cost = stored_cost(&harness, first).await;
    assert_eq!(
        first_cost["cost_total_micros"],
        json!(500),
        "the historical row keeps the price that was in force when it was made: {first_cost}"
    );
    assert_eq!(
        first_cost["cost_input_micros_per_mtok"],
        json!(1_000),
        "the snapshot carries the rate too, so the total can be re-derived and audited"
    );
    assert_eq!(
        first_cost["cost_output_micros_per_mtok"],
        json!(3_000),
        "the corrected rate must not reach back into a row that was already written: {first_cost}"
    );

    // And the new row is genuinely at the new price — otherwise the assertion above would also
    // pass against an implementation that never prices anything.
    let second_cost = stored_cost(&harness, second).await;
    assert_eq!(
        second_cost["cost_total_micros"],
        json!(3_200),
        "the new call is billed at the edited price: {second_cost}"
    );
    assert_eq!(second_cost["cost_output_micros_per_mtok"], json!(30_000));

    harness.dispose().await;
}

/// An unpriced model and a call whose endpoint reported no usage both store a **null** cost, and
/// a free model stores a **zero**. These are three different sentences in a costs screen, and
/// collapsing any two of them is the failure this test exists to prevent — in particular,
/// `unwrap_or(0)` on the token counts would make "unknown" render as "free".
#[tokio::test]
async fn an_unknown_cost_is_null_and_a_free_one_is_zero() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let (token, provider, small, _large) = connected(&harness, &base_url).await;
    let provider_uuid = Uuid::parse_str(&provider).expect("a provider id is a uuid");

    // A stream that ended without a usage frame, on a model that *is* priced.
    harness
        .call(patch(
            &format!("/api/v1/ai/models/{small}"),
            json!({ "input_cost_micros_per_mtok": 1_000, "output_cost_micros_per_mtok": 3_000 }),
            &token,
        ))
        .await;
    let unpriced_call = record_cost(
        &harness,
        provider_uuid,
        "mock-small",
        omnion_ai_hub::cost::ModelPrice {
            input_micros_per_mtok: Some(1_000),
            output_micros_per_mtok: Some(3_000),
        },
        None,
        None,
    )
    .await;

    let stored = stored_cost(&harness, unpriced_call).await;
    assert_eq!(
        stored["cost_total_micros"],
        json!(null),
        "no usage block means the cost is unknown, and unknown is not zero: {stored}"
    );
    // The instant is still stamped: "we priced this and the answer was unknown" is a different
    // statement from "nobody ever looked".
    assert_ne!(
        stored["cost_calculated_at"],
        json!(null),
        "the attempt is recorded even when it produced no number: {stored}"
    );

    // A model with an explicit price of zero is *known* and free — a real measurement.
    let free = record_cost(
        &harness,
        provider_uuid,
        "mock-small",
        omnion_ai_hub::cost::ModelPrice {
            input_micros_per_mtok: Some(0),
            output_micros_per_mtok: Some(0),
        },
        Some(1_000_000),
        Some(1_000_000),
    )
    .await;
    assert_eq!(
        stored_cost(&harness, free).await["cost_total_micros"],
        json!(0),
        "a free model is a measurement of zero, which is not the same as an absent cost"
    );

    harness.dispose().await;
}

/// Write one usage row through the store, exactly as the runtime does, and return its id.
async fn record_cost(
    harness: &Harness,
    provider: Uuid,
    model_key: &str,
    price: omnion_ai_hub::cost::ModelPrice,
    prompt_tokens: Option<i32>,
    completion_tokens: Option<i32>,
) -> i64 {
    omnion_ai_hub::health_store::record_usage(
        harness.db.pool(),
        omnion_ai_hub::health_store::NewUsage {
            provider_id: provider,
            model_key: Some(model_key.to_owned()),
            task: "chat".to_owned(),
            outcome: "ok".to_owned(),
            http_status: Some(200),
            prompt_tokens,
            completion_tokens,
            latency_ms: 120,
            substituted_from: None,
            first_byte_at: None,
            cost: omnion_ai_hub::cost::call_cost(price, prompt_tokens, completion_tokens),
        },
    )
    .await
    .expect("the usage row records");
    // `record_usage` returns `()` — the store's contract is deliberately about the write, not
    // the id — so the row is found by its own ordering. `max(id)` per provider is unambiguous
    // here because the two calls in each walk are strictly ordered by the assertions between
    // them, and it is read *after* the insert rather than guessed before it.
    sqlx::query_scalar("select max(id) from ai_provider_usage where provider_id = $1")
        .bind(provider)
        .fetch_one(harness.db.pool())
        .await
        .expect("the newest row id reads")
}

/// One stored usage row's cost columns, read back as JSON so the assertions can distinguish
/// `null` from `0` — which `json!(0)` and `json!(null)` do, and which a `try_get::<i64,_>` on the
/// nullable column cannot.
async fn stored_cost(harness: &Harness, id: i64) -> Value {
    let row = sqlx::query(
        "select cost_input_micros_per_mtok, cost_output_micros_per_mtok, cost_total_micros, \
         cost_calculated_at from ai_provider_usage where id = $1",
    )
    .bind(id)
    .fetch_one(harness.db.pool())
    .await
    .expect("the cost columns read");

    let total: Option<i64> = sqlx::Row::get(&row, "cost_total_micros");
    // Built by hand rather than through serde so the value's own nullness survives: a
    // `Value::Null` here is the assertion's subject, not an accident of decoding.
    json!({
        "cost_input_micros_per_mtok": row.get::<Option<i64>, _>("cost_input_micros_per_mtok"),
        "cost_output_micros_per_mtok": row.get::<Option<i64>, _>("cost_output_micros_per_mtok"),
        "cost_total_micros": total,
        "cost_calculated_at": row.get::<Option<time::OffsetDateTime>, _>("cost_calculated_at")
            .map(|at| at.to_string()),
    })
}

// -------------------------------------------------------------------------------------------
// The harness's own helpers
// -------------------------------------------------------------------------------------------

/// Connect to the compose PostgreSQL; `None` means the stack is not running.
async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 1,
    })
    .await
    {
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

/// The maintenance connection, which points at `postgres` rather than a test database.
fn maintenance_config(config: &Config) -> DatabaseConfig {
    DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 1,
    }
}

/// Point a database URL at a different database on the same server.
fn swap_database(url: &str, database: &str) -> String {
    let (base, _) = url
        .rsplit_once('/')
        .expect("a database URL carries a path");
    let base = base.split('?').next().expect("a query-free base");
    format!("{base}/{database}")
}
