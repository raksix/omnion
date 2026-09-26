//! Integration tests for the AI Hub (phase P11, docs/06-AI-HUB.md).
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) on a throwaway database, and
//! the provider they talk to is a **mock**: an in-process OpenAI-compatible server this suite
//! starts on an ephemeral port. Nothing here leaves the machine, and nothing here needs a key
//! from a real vendor — the proof P11 asks for is that Omnion can connect an OpenAI-compatible
//! provider, route to one of its models, stream an answer through `/api/v1/ai/chat`, and report
//! a provider that refuses honestly.
//!
//! The walks cover: connecting a provider (with models), the key staying write-only, the model
//! registry and its default, an explicit `provider/model` address, a bare model key, the
//! default-model route, discovery against the provider's own `/models`, model replacement and
//! the default-model repair, a provider that answers `500`, the permission gate (`401` without
//! a session, `403` without `ai.chat`), and the audit trail every step leaves behind.
//!
//! When PostgreSQL is not reachable the suite skips itself with a printed reason, so
//! `cargo test` stays usable on a machine without Docker.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get as route_get, post as route_post};
use axum::{Json, Router};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::sessions;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{NewBinding, Scope};
use omnion_permissions::{bindings, roles as role_store};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// A running mock provider: its base URL and the task that serves it.
struct MockProvider {
    base_url: String,
    task: tokio::task::JoinHandle<()>,
}

impl MockProvider {
    /// Start the mock on an ephemeral port.
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock must bind a port");
        let address = listener.local_addr().expect("the mock has an address");

        let app = Router::new()
            .route("/v1/models", route_get(mock_models))
            .route("/v1/chat/completions", route_post(mock_chat));

        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            base_url: format!("http://{address}/v1"),
            task,
        }
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `GET /v1/models` — what a provider reports about itself.
async fn mock_models() -> Json<Value> {
    Json(json!({
        "object": "list",
        "data": [{ "id": "mock-small" }, { "id": "mock-large" }]
    }))
}

/// `POST /v1/chat/completions` — a fixed answer, streamed or in one piece.
///
/// The model `broken-model` makes the mock refuse the way a real provider refuses a request it
/// cannot serve, so the suite can prove the platform reports it instead of hiding it.
async fn mock_chat(Json(body): Json<Value>) -> Response {
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    if model == "broken-model" {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": { "message": "the mock cannot serve this model" } })),
        )
            .into_response();
    }

    let stream = body["stream"].as_bool().unwrap_or(false);
    let answer = format!("Hello from the mock ({model}).");

    if stream {
        let mut sse = String::new();
        for word in answer.split_inclusive(' ') {
            sse.push_str(&format!(
                "data: {}\n\n",
                json!({ "choices": [{ "delta": { "content": word } }] })
            ));
        }
        sse.push_str(&format!(
            "data: {}\n\n",
            json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] })
        ));
        sse.push_str(&format!(
            "data: {}\n\n",
            json!({
                "choices": [],
                "usage": { "prompt_tokens": 7, "completion_tokens": 4, "total_tokens": 11 }
            })
        ));
        sse.push_str("data: [DONE]\n\n");

        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(sse))
            .expect("the mock stream must build");
    }

    Json(json!({
        "choices": [{
            "message": { "role": "assistant", "content": answer },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 7, "completion_tokens": 4, "total_tokens": 11 }
    }))
    .into_response()
}

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    body: Value,
    text: String,
}

/// A throwaway database with every migration applied.
struct Harness {
    state: AppState,
    db: Db,
    maintenance: Db,
    database: String,
}

impl Harness {
    /// Open a fresh database with every migration applied.
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().expect("environment must be valid");
        live_db(&config).await?;

        let database = format!("omnion_ai_{}", Uuid::new_v4().simple());
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

    /// Drive the router without a network socket.
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

/// The session token a `Set-Cookie` header carries.
fn token_of(response: &TestResponse) -> String {
    let cookie = response
        .set_cookie
        .as_deref()
        .expect("the response must set a cookie");
    cookie
        .split(';')
        .next()
        .expect("cookie has a value")
        .split_once('=')
        .expect("cookie is name=value")
        .1
        .to_owned()
}

/// Build a JSON request; `token` becomes the session cookie.
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

/// A GET request, optionally with a session cookie.
fn get(uri: &str, token: Option<&str>) -> Request<Body> {
    request(Method::GET, uri, token, None)
}

/// A POST request carrying a JSON body.
fn post(uri: &str, body: Value, token: Option<&str>) -> Request<Body> {
    request(Method::POST, uri, token, Some(body))
}

/// A chat request: one user message.
fn chat(model: Option<&str>) -> Value {
    let mut body = json!({
        "messages": [{ "role": "user", "content": "Say hello" }]
    });
    if let Some(model) = model {
        body["model"] = json!(model);
    }
    body
}

/// The `(event, data)` pairs of one `text/event-stream` body.
fn sse_events(body: &str) -> Vec<(String, Value)> {
    let mut events = Vec::new();
    for frame in body.split("\n\n") {
        let mut event = String::from("message");
        let mut data = String::new();
        for line in frame.lines() {
            if let Some(name) = line.strip_prefix("event: ") {
                event = name.trim().to_owned();
            }
            if let Some(payload) = line.strip_prefix("data: ") {
                data.push_str(payload);
            }
        }
        if !data.is_empty() {
            let parsed = serde_json::from_str(&data).unwrap_or(Value::Null);
            events.push((event, parsed));
        }
    }
    events
}

/// The answer a stream carried, in arrival order.
fn streamed_answer(events: &[(String, Value)]) -> String {
    events
        .iter()
        .filter(|(event, _)| event == "delta")
        .filter_map(|(_, data)| data["content"].as_str())
        .collect()
}

/// Create an account with a session and return `(user id, token)`.
async fn account(harness: &Harness, email: &str) -> (Uuid, String) {
    let user = users::create_user(
        harness.db.pool(),
        NewUser {
            email: email.to_owned(),
            password: PASSWORD.to_owned(),
            display_name: "Walk".to_owned(),
            organization_id: None,
        },
    )
    .await
    .expect("the account must be created");
    let (_, token) = sessions::create_session(harness.db.pool(), user.id, None, None)
        .await
        .expect("the session must be created");
    (user.id, token)
}

#[tokio::test]
async fn the_ai_hub_connects_a_provider_and_streams_a_chat_through_it() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let mock = MockProvider::start().await;

    // The owner of a fresh installation (the wizard creates it and signs it in).
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
    let token = token_of(&owner);

    // Nothing on the AI surface without a session.
    assert_eq!(
        harness.call(get("/api/v1/ai/providers", None)).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        harness
            .call(post("/api/v1/ai/chat", chat(None), None))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );

    // A provider with two models, connected against the mock.
    let created = harness
        .call(post(
            "/api/v1/ai/providers",
            json!({
                "name": "Mock AI",
                "base_url": format!("{}/", mock.base_url),
                "api_key": "sk-mock-secret",
                "models": [
                    { "key": "mock-small", "context_window": 32768 },
                    "mock-large"
                ],
            }),
            Some(&token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let provider_id = created.body["id"].as_str().expect("provider id").to_owned();
    assert_eq!(created.body["name"], json!("Mock AI"));
    assert_eq!(
        created.body["base_url"],
        json!(mock.base_url),
        "the trailing slash is normalized away"
    );
    assert_eq!(created.body["has_api_key"], json!(true));
    assert_eq!(created.body["model_count"], json!(2));
    assert!(
        !created.text.contains("sk-mock-secret"),
        "the stored key never comes back: {}",
        created.text
    );

    // A second provider with the same name is refused.
    let duplicate = harness
        .call(post(
            "/api/v1/ai/providers",
            json!({ "name": "mock ai", "base_url": mock.base_url }),
            Some(&token),
        ))
        .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT);
    assert_eq!(duplicate.body["error"]["code"], "provider_name_taken");

    // A base URL the platform cannot use is refused before anything is stored.
    let unusable = harness
        .call(post(
            "/api/v1/ai/providers",
            json!({ "name": "No Scheme", "base_url": "api.example.com/v1" }),
            Some(&token),
        ))
        .await;
    assert_eq!(unusable.status, StatusCode::BAD_REQUEST);
    assert_eq!(unusable.body["error"]["code"], "invalid_provider");

    // The registry: both models, told apart by their provider.
    let models = harness.call(get("/api/v1/ai/models", Some(&token))).await;
    assert_eq!(models.status, StatusCode::OK);
    let listed = models.body["models"].as_array().expect("models are a list");
    assert_eq!(listed.len(), 2, "{:?}", models.body);
    let small = listed
        .iter()
        .find(|model| model["model_key"] == "mock-small")
        .expect("mock-small is registered");
    assert_eq!(small["provider_name"], json!("Mock AI"));
    assert_eq!(small["model_id"], json!("Mock AI/mock-small"));
    assert_eq!(small["context_window"], json!(32768));
    assert_eq!(small["supports_streaming"], json!(true));
    let defaults: Vec<&Value> = listed
        .iter()
        .filter(|model| model["is_default"] == json!(true))
        .collect();
    assert_eq!(
        defaults.len(),
        1,
        "connecting a provider leaves exactly one default model"
    );

    // Discovery asks the provider what it serves.
    let discovered = harness
        .call(post(
            &format!("/api/v1/ai/providers/{provider_id}/discover-models"),
            json!({}),
            Some(&token),
        ))
        .await;
    assert_eq!(discovered.status, StatusCode::OK, "{:?}", discovered.body);
    assert_eq!(
        discovered.body["models"],
        json!(["mock-large", "mock-small"]),
        "the provider's own list comes back sorted"
    );

    // The chat, streamed, addressed as `provider/model`.
    let streamed = harness
        .call(post(
            "/api/v1/ai/chat",
            chat(Some("Mock AI/mock-large")),
            Some(&token),
        ))
        .await;
    assert_eq!(streamed.status, StatusCode::OK, "{}", streamed.text);
    let events = sse_events(&streamed.text);
    let start = events
        .iter()
        .find(|(event, _)| event == "start")
        .expect("a start frame opens the stream");
    assert_eq!(start.1["provider"], json!("Mock AI"));
    assert_eq!(start.1["model"], json!("mock-large"));
    assert_eq!(start.1["protocol"], json!("openai_compatible"));
    let answer = "Hello from the mock (mock-large).";
    assert_eq!(
        streamed_answer(&events),
        answer,
        "the deltas reassemble into the answer: {}",
        streamed.text
    );
    let done = events
        .iter()
        .find(|(event, _)| event == "done")
        .expect("a done frame closes the stream");
    assert_eq!(done.1["finish_reason"], json!("stop"));
    assert_eq!(done.1["usage"]["total_tokens"], json!(11));
    assert_eq!(done.1["chars"], json!(answer.chars().count()));

    // A bare model key resolves across the connected providers.
    let bare = harness
        .call(post(
            "/api/v1/ai/chat",
            chat(Some("mock-small")),
            Some(&token),
        ))
        .await;
    assert_eq!(
        streamed_answer(&sse_events(&bare.text)),
        "Hello from the mock (mock-small)."
    );

    // No model at all: the router takes the installation's default.
    let defaulted = harness
        .call(post("/api/v1/ai/chat", chat(None), Some(&token)))
        .await;
    assert_eq!(defaulted.status, StatusCode::OK, "{}", defaulted.text);
    assert!(
        streamed_answer(&sse_events(&defaulted.text)).starts_with("Hello from the mock ("),
        "the default model answers: {}",
        defaulted.text
    );

    // A provider that refuses is reported, not hidden.
    let refused = harness
        .call(post(
            "/api/v1/ai/chat",
            chat(Some("broken-model")),
            Some(&token),
        ))
        .await;
    // The registry does not carry `broken-model`, so this one is refused before any provider
    // call — which is its own honest answer.
    assert_eq!(refused.status, StatusCode::NOT_FOUND);
    assert_eq!(refused.body["error"]["code"], "model_not_found");

    // Register it, and the provider's own refusal arrives as a stream frame.
    let replaced = harness
        .call(request(
            Method::PUT,
            &format!("/api/v1/ai/providers/{provider_id}/models"),
            Some(&token),
            Some(json!({ "models": ["mock-small", "broken-model"] })),
        ))
        .await;
    assert_eq!(replaced.status, StatusCode::OK, "{:?}", replaced.body);
    assert_eq!(replaced.body["models"].as_array().expect("models").len(), 2);

    let refusing = harness
        .call(post(
            "/api/v1/ai/chat",
            chat(Some("broken-model")),
            Some(&token),
        ))
        .await;
    assert_eq!(refusing.status, StatusCode::OK, "{}", refusing.text);
    let failure = sse_events(&refusing.text)
        .into_iter()
        .find(|(event, _)| event == "error")
        .expect("an error frame reports the refusal");
    assert_eq!(failure.1["code"], json!("provider_error"));
    assert!(
        failure.1["message"]
            .as_str()
            .expect("a message")
            .contains("500"),
        "the provider's status is in the report: {failure:?}"
    );

    // Switching a model off repairs the default; the registry keeps exactly one.
    let broken = replaced.body["models"]
        .as_array()
        .expect("models")
        .iter()
        .find(|model| model["model_key"] == "broken-model")
        .expect("broken-model is registered")
        .clone();
    let broken_id = broken["id"].as_str().expect("id").to_owned();
    let disabled = harness
        .call(request(
            Method::PATCH,
            &format!("/api/v1/ai/models/{broken_id}"),
            Some(&token),
            Some(json!({ "enabled": false })),
        ))
        .await;
    assert_eq!(disabled.status, StatusCode::OK, "{:?}", disabled.body);
    assert_eq!(disabled.body["enabled"], json!(false));

    let after = harness.call(get("/api/v1/ai/models", Some(&token))).await;
    let enabled_defaults = after.body["models"]
        .as_array()
        .expect("models")
        .iter()
        .filter(|model| model["is_default"] == json!(true))
        .count();
    assert_eq!(enabled_defaults, 1, "{:?}", after.body);

    // Audit: the connection, the model change and every chat left a row.
    let audit = harness.call(get("/api/v1/iam/audit", Some(&token))).await;
    assert_eq!(audit.status, StatusCode::OK, "{:?}", audit.body);
    let actions: Vec<String> = audit.body["entries"]
        .as_array()
        .expect("audit entries")
        .iter()
        .filter_map(|entry| entry["action"].as_str().map(str::to_owned))
        .collect();
    for expected in [
        "ai.provider.connected",
        "ai.provider.models_replaced",
        "ai.model.updated",
        "ai.chat.completed",
        "ai.chat.failed",
    ] {
        assert!(
            actions.contains(&expected.to_owned()),
            "{expected} must be audited: {actions:?}"
        );
    }

    // Removing the provider takes its models with it and leaves the AI Hub without a default.
    let removed = harness
        .call(request(
            Method::DELETE,
            &format!("/api/v1/ai/providers/{provider_id}"),
            Some(&token),
            None,
        ))
        .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    assert_eq!(
        harness
            .call(get("/api/v1/ai/providers", Some(&token)))
            .await
            .body["providers"],
        json!([]),
        "the provider list is empty again"
    );
    let orphaned = harness
        .call(post("/api/v1/ai/chat", chat(None), Some(&token)))
        .await;
    assert_eq!(orphaned.status, StatusCode::CONFLICT);
    assert_eq!(orphaned.body["error"]["code"], "no_default_model");

    harness.dispose().await;
}

#[tokio::test]
async fn the_chat_asks_for_the_ai_chat_permission() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let mock = MockProvider::start().await;

    let owner = harness
        .call(post(
            "/api/v1/onboarding/owner",
            json!({
                "display_name": "Owner",
                "email": format!("owner-{}@omnion.test", Uuid::new_v4().simple()),
                "password": PASSWORD,
            }),
            None,
        ))
        .await;
    let owner_token = token_of(&owner);

    // A member: signed in, but the member role holds no AI key.
    let (member_id, member_token) = account(
        &harness,
        &format!("member-{}@omnion.test", Uuid::new_v4().simple()),
    )
    .await;
    let member_role = role_store::find_role_by_key(harness.db.pool(), None, "member")
        .await
        .expect("the member role must be readable")
        .expect("the member role exists");
    bindings::grant_if_missing(
        harness.db.pool(),
        NewBinding {
            role_id: member_role.id,
            user_id: member_id,
            scope: Scope::Global,
            granted_by: None,
            expires_at: None,
        },
    )
    .await
    .expect("the member binding must be written");

    // The Owner connects the provider.
    let created = harness
        .call(post(
            "/api/v1/ai/providers",
            json!({ "name": "Mock AI", "base_url": mock.base_url, "models": ["mock-small"] }),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);

    // The member may not chat, and may not read the provider list either.
    let denied = harness
        .call(post("/api/v1/ai/chat", chat(None), Some(&member_token)))
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{:?}", denied.body);
    assert_eq!(denied.body["error"]["code"], json!("permission_denied"));

    let hidden = harness
        .call(get("/api/v1/ai/providers", Some(&member_token)))
        .await;
    assert_eq!(hidden.status, StatusCode::FORBIDDEN);

    // The owner chats through the same installation.
    let allowed = harness
        .call(post("/api/v1/ai/chat", chat(None), Some(&owner_token)))
        .await;
    assert_eq!(allowed.status, StatusCode::OK, "{}", allowed.text);
    assert!(streamed_answer(&sse_events(&allowed.text)).starts_with("Hello from the mock"));

    // The member's own account record is untouched by any of this.
    let member = users::find_by_id(harness.db.pool(), member_id)
        .await
        .expect("the member must read")
        .expect("the member exists");
    assert_eq!(member.status, "active");

    harness.dispose().await;
}

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

/// Maintenance connection (`postgres` database) used to create and drop throwaway databases.
fn maintenance_config(config: &Config) -> DatabaseConfig {
    DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 1,
    }
}

/// Replace the database name in a PostgreSQL connection string.
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
