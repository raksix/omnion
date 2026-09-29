//! Integration tests for the **live** resolve path (REQ-098, slice 4).
//!
//! Slice 3's walks record a decision by calling the crate directly. That proves the store and
//! the log, and it is exactly the weakness this suite exists to close: a log that only ever
//! receives rows from a test stays empty in production, and an empty log is indistinguishable
//! from a broken one. Nothing in the previous suite went *through* `POST /api/v1/ai/chat`.
//!
//! Three claims are proven here, and each is a claim about the endpoint rather than the crate:
//!
//! - **A real chat leaves a decision.** The endpoint resolves through the maps, records the row
//!   *before* dialling, and the row is readable through the log with the right task, rule and
//!   requested value. The walk runs against a mock provider, so the streaming path is exercised
//!   for real without a vendor.
//! - **The requested value is stored verbatim.** A request that names `Provider/model` must be
//!   answerable as "what did the caller ask for" — the question a fallback badge provokes. A log
//!   that stores the *resolved* pair instead cannot answer it, and the difference is invisible
//!   everywhere else.
//! - **A request nothing can answer emits `ai.route.unresolved` and is refused, not 500'd.**
//!   The event is the request's named alert hook; a refusal that only says "unresolved" sends
//!   the operator back to the routing screen to re-derive what the walk already said. The status
//!   is checked as `422`, because a `500` here tells the caller the platform is broken when the
//!   fix is one row on a screen.
//!
//! They run against the same throwaway-database harness the other AI suites use and skip
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
// The harness
// -------------------------------------------------------------------------------------------

struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    body: Value,
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

        let database = format!("omnion_livepath_{}", Uuid::new_v4().simple());
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
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);

        TestResponse {
            status,
            set_cookie,
            body,
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

fn get(uri: &str, token: Option<&str>) -> Request<Body> {
    let builder = Request::builder().method(Method::GET).uri(uri);
    let builder = match token {
        Some(token) => builder.header(header::COOKIE, format!("omnion_session={token}")),
        None => builder,
    };
    builder.body(Body::empty()).expect("request must build")
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
    let builder = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    let builder = match token {
        Some(token) => builder.header(header::COOKIE, format!("omnion_session={token}")),
        None => builder,
    };
    builder
        .body(Body::from(body.to_string()))
        .expect("request must build")
}

// -------------------------------------------------------------------------------------------
// Fixtures
// -------------------------------------------------------------------------------------------

/// A provider that answers a stream with one chunk and a usage block, so the chat endpoint
/// completes rather than hanging.
async fn mock_provider() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the mock must bind a port");
    let address = listener.local_addr().expect("the mock has an address");
    let app = Router::new()
        .route("/v1/models", route_get(|| async { Json(json!({ "data": [] })) }))
        .route(
            "/v1/chat/completions",
            route_post(|| async {
                Json(json!({
                    "choices": [{ "delta": { "content": "ok" }, "finish_reason": "stop" }],
                    "usage": { "prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7 }
                }))
            }),
        );
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}/v1"), task)
}

struct Fixture {
    /// A tenant member's session: the calls that are scoped to an organization.
    token: String,
    /// The installer's session: the calls that change the installation.
    owner_token: String,
    organization: Uuid,
    small: String,
}

async fn connected(harness: &Harness, base_url: &str) -> Fixture {
    let owner = harness
        .call(post(
            "/api/v1/onboarding/owner",
            json!({
                "display_name": "Ada Lovelace",
                "email": format!("owner-{}@omnion.test", Uuid::new_v4().simple()),
                "password": PASSWORD,
            }),
            None,
        ))
        .await;
    assert_eq!(owner.status, StatusCode::CREATED, "{:?}", owner.body);
    let owner_token = token_of(&owner);

    // The first owner is a **platform-level** account: its `organization_id` is null, which is
    // correct (nobody has created a tenant yet) and is why the organization has to be created
    // explicitly below before the tenancy walk can mean anything.
    assert!(
        owner.body["user"]["organization_id"].is_null(),
        "the first owner is platform-level: {:?}",
        owner.body["user"]
    );

    let organization_created = harness
        .call(post(
            "/api/v1/onboarding/organization",
            json!({ "name": "Acme" }),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(
        organization_created.status,
        StatusCode::OK,
        "{:?}",
        organization_created.body
    );

    // The status body names the organization but not its id, and inventing one here would make
    // the tenancy assertion compare the event against a uuid nothing else knows. The database is
    // the authority: this suite owns the whole database, so "the only organization in it" is the
    // organization the request was made in.
    let organization = sqlx::query("select id from organizations order by created_at limit 1")
        .fetch_one(harness.db.pool())
        .await
        .expect("the organization the bootstrap created must be readable")
        .get::<Uuid, _>("id");

    // The role that carries `ai.chat` and `ai.usage.read`, looked up rather than hard-coded:
    // a fixture that names a role id literal breaks the day the seeded ids move, and the whole
    // suite would fail on a permission it never meant to be testing.
    let roles = harness
        .call(get("/api/v1/iam/roles", Some(&owner_token)))
        .await;
    assert_eq!(roles.status, StatusCode::OK, "{:?}", roles.body);
    let admin_role = roles.body["roles"]
        .as_array()
        .expect("roles array")
        .iter()
        .find(|role| role["key"] == "administrator" || role["key"] == "admin")
        .or_else(|| {
            roles.body["roles"]
                .as_array()
                .expect("roles array")
                .iter()
                .find(|role| role["key"] == "editor")
        })
        .and_then(|role| role["id"].as_str())
        .expect("an installation ships at least one role that may use the AI")
        .to_owned();

    // A **member** of that organization, and the session the walks actually use.
    //
    // This is not ceremony. The decision log is filtered by the caller's organization, and a
    // platform account has none — so a chat made as the owner writes an installation-wide row
    // that the owner then cannot read back, and every assertion below would see an empty log
    // and blame the writer. The walks therefore run as a tenant, which is also the realistic
    // case: the requests that matter come from a site, not from the installer.
    let member_email = format!("member-{}@omnion.test", Uuid::new_v4().simple());
    let member = harness
        .call(post(
            "/api/v1/iam/users",
            json!({
                "email": member_email,
                "display_name": "Grace Hopper",
                "organization_id": organization,
                "password": PASSWORD,
                "role_id": admin_role,
            }),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(member.status, StatusCode::CREATED, "{:?}", member.body);

    let signed_in = harness
        .call(post(
            "/api/v1/auth/login",
            json!({ "email": member_email, "password": PASSWORD }),
            None,
        ))
        .await;
    assert_eq!(
        signed_in.status,
        StatusCode::OK,
        "the member must be able to sign in: {:?}",
        signed_in.body
    );
    assert_eq!(
        signed_in.body["user"]["organization_id"],
        json!(organization.to_string()),
        "the session is tenant-scoped, which is what the log filter reads"
    );
    let token = token_of(&signed_in);

    // The provider and the model toggle are **installation** actions (`ai.providers.manage`),
    // so they are made with the owner's session. A member may use the model but not register
    // one — which is the shape a real installation has, and why the two sessions are kept apart
    // rather than one session doing everything.
    let created = harness
        .call(post(
            "/api/v1/ai/providers",
            json!({
                "name": "Live Mock",
                "base_url": base_url,
                "models": [{ "key": "small", "context_window": 8192 }]
            }),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);

    let models = harness.call(get("/api/v1/ai/models", Some(&owner_token))).await;
    let small = models.body["models"][0]["id"]
        .as_str()
        .expect("model id")
        .to_owned();

    Fixture {
        token,
        owner_token,
        organization,
        small,
    }
}

/// Switch a model off through the catalog's own PATCH.
///
/// Not a direct `update ai_models` — a test that edits the row behind the API proves the row,
/// not that the panel's own toggle produces the state the walk depends on. This is the same
/// discipline `disabling_the_primary_degrades_to_the_first_fallback` follows.
async fn switch_off(harness: &Harness, fixture: &Fixture) {
    let off = harness
        .call(patch(
            &format!("/api/v1/ai/models/{}", fixture.small),
            json!({ "enabled": false }),
            &fixture.owner_token,
        ))
        .await;
    assert_eq!(off.status, StatusCode::OK, "{:?}", off.body);
}

fn chat_body(model: Option<&str>, feature: Option<&str>) -> Value {
    json!({
        "model": model,
        "feature": feature,
        "messages": [{ "role": "user", "content": "hello" }],
    })
}

// -------------------------------------------------------------------------------------------
// The walks
// -------------------------------------------------------------------------------------------

/// A real chat leaves a decision row, and the row says what the caller asked for.
///
/// This is the walk the previous slice could not write: it goes through the endpoint, so it
/// fails if the endpoint stops recording, and it reads the log back through the same endpoint
/// the panel reads. The mock provider means the stream genuinely runs.
#[tokio::test]
async fn a_real_chat_leaves_a_decision_row() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;

    let chat = harness
        .call(post("/api/v1/ai/chat", chat_body(None, None), Some(&fixture.token)))
        .await;
    // The stream opens; the body is SSE rather than JSON, which is why only the status is
    // asserted here and the *row* is what the rest of the walk reads.
    assert_eq!(
        chat.status,
        StatusCode::OK,
        "the chat must open: {:?}",
        chat.body
    );

    let log = harness
        .call(get("/api/v1/ai/logs/decisions", Some(&fixture.token)))
        .await;
    assert_eq!(log.status, StatusCode::OK, "{:?}", log.body);
    assert_eq!(
        log.body["total"],
        json!(1),
        "exactly one decision per request, never two: {:?}",
        log.body
    );

    let row = &log.body["rows"][0];
    assert_eq!(row["task"], json!("chat"));
    assert_eq!(row["unresolved"], json!(false));
    // A chat that named no model routes through the installation default, and the rule must
    // say so — "the log says a model answered" is not the claim; "it says *why* that model" is.
    assert_eq!(row["rule"], json!("installation_default"));
    assert_eq!(row["fallback_index"], json!(0));
    assert!(
        row["reason"].as_str().is_some_and(|r| !r.is_empty()),
        "the row carries a reason: {row}"
    );

    harness.dispose().await;
}

/// The row stores what the caller **asked for**, not what answered.
///
/// A request naming `Live Mock/small` must be answerable as "the caller pinned this model" —
/// which is the first question a fallback badge raises. A log that stored the resolved pair
/// would look identical in every other column, so this is asserted directly.
#[tokio::test]
async fn the_decision_stores_the_requested_model_verbatim() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;

    let chat = harness
        .call(post(
            "/api/v1/ai/chat",
            chat_body(Some("Live Mock/small"), None),
            Some(&fixture.token),
        ))
        .await;
    assert_eq!(chat.status, StatusCode::OK, "{:?}", chat.body);

    let log = harness
        .call(get("/api/v1/ai/logs/decisions", Some(&fixture.token)))
        .await;
    let row = &log.body["rows"][0];
    assert_eq!(
        row["requested"],
        json!("Live Mock/small"),
        "the caller's own value is kept: {row}"
    );
    assert_eq!(row["rule"], json!("explicit"));
    // The caller is recorded in the **row**, not in the list view: the panel's table does not
    // show an account, and adding a column it does not render would be a field nobody reads. The
    // column is asserted where it is real — in the store — rather than through a view that
    // deliberately omits it.
    let caller: (Option<Uuid>,) = sqlx::query_as("select user_id from ai_route_decisions limit 1")
        .fetch_one(harness.db.pool())
        .await
        .expect("the decision row must be readable");
    assert!(
        caller.0.is_some(),
        "the row names the account that made the request: {caller:?}"
    );

    harness.dispose().await;
}

/// A request nothing can answer is refused with `422` and emits `ai.route.unresolved`.
///
/// Two halves, and the event half is the one that matters: an operator cannot watch the route
/// screen, but they can watch a webhook. The event is read from the store rather than inferred
/// from the refusal, because "the endpoint returned 422" and "an alert fired" are different
/// claims and only the second one keeps somebody awake.
#[tokio::test]
async fn a_request_nothing_can_answer_is_refused_and_announced() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;

    // Switch the only model off, so nothing in any map can answer.
    switch_off(&harness, &fixture).await;

    let chat = harness
        .call(post("/api/v1/ai/chat", chat_body(None, None), Some(&fixture.token)))
        .await;
    assert_eq!(
        chat.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an unresolvable request is the operator's, not a server fault: {:?}",
        chat.body
    );
    assert_eq!(
        chat.body["error"]["code"],
        json!("ai.route.unresolved"),
        "the code is what a client branches on: {:?}",
        chat.body
    );
    let message = chat.body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("decision #"),
        "the refusal hands over the decision that explains it: {message}"
    );

    // The decision row is written even though nothing answered — that is the promise of writing
    // the row *before* dialling, and it is what the operator opens.
    let log = harness
        .call(get("/api/v1/ai/logs/decisions", Some(&fixture.token)))
        .await;
    assert_eq!(log.body["total"], json!(1), "the refusal is logged: {:?}", log.body);
    assert_eq!(log.body["rows"][0]["unresolved"], json!(true));
    assert_eq!(log.body["rows"][0]["rule"], json!("unresolved"));

    // And the event fired. Read from the bus's own table so the assertion is about an event
    // having been published, not about the endpoint having returned an error.
    let events = sqlx::query(
        "select name from events where name = 'ai.route.unresolved'",
    )
    .fetch_all(harness.db.pool())
    .await
    .expect("the event log must read");
    assert_eq!(
        events.len(),
        1,
        "exactly one alert per unresolvable request, not one per attempt: {events:?}"
    );

    harness.dispose().await;
}

/// The organization that owns the request is the one the event is scoped to.
///
/// An event carrying somebody else's organization would deliver this tenant's routing failure to
/// another tenant's webhook, which is a leak in the direction nobody tests for.
#[tokio::test]
async fn the_unresolved_event_carries_the_requesting_organization() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (base_url, _mock) = mock_provider().await;
    let fixture = connected(&harness, &base_url).await;

    switch_off(&harness, &fixture).await;

    let _ = harness
        .call(post("/api/v1/ai/chat", chat_body(None, None), Some(&fixture.token)))
        .await;

    let row = sqlx::query(
        "select organization_id, payload::text as payload from events \
         where name = 'ai.route.unresolved' limit 1",
    )
    .fetch_one(harness.db.pool())
    .await
    .expect("the event must be stored");
    assert_eq!(
        row.get::<Option<Uuid>, _>("organization_id"),
        Some(fixture.organization),
        "the event is scoped to the organization that made the request"
    );
    let payload: Value = serde_json::from_str(row.get::<String, _>("payload").as_str())
        .expect("the payload is jsonb and must parse");
    assert_eq!(payload["task"], json!("chat"));
    assert!(
        payload["decision_id"].as_i64().is_some(),
        "the payload points at the log row, so the webhook and the log are one fact: {payload}"
    );

    harness.dispose().await;
}
