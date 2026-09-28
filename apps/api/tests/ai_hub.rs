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
use axum::extract::Path;
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

/// A running mock provider: its base URLs (one per protocol shape) and the task serving them.
///
/// It answers the OpenAI-compatible, the messages and the generateContent shapes from one
/// process, so the platform's three adapters are proven against a real socket rather than
/// against a fixture — and a dead port (`:1`) stands in for an endpoint that is simply not there.
struct MockProvider {
    base_url: String,
    /// A second prefix of the same mock, serving Gemini's own model-list shape.
    gemini_base_url: String,
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
            .route("/gemini/v1/models", route_get(mock_gemini_models))
            .route("/gemini/v1/models/{op}", route_post(mock_generate))
            .route("/v1/chat/completions", route_post(mock_chat))
            .route("/v1/messages", route_post(mock_messages))
            // axum allows one parameter per path segment, so the mock takes the whole
            // `{model}:generateContent` tail — the platform sends exactly that.
            .route("/v1/models/{op}", route_post(mock_generate));

        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            base_url: format!("http://{address}/v1"),
            gemini_base_url: format!("http://{address}/gemini/v1"),
            task,
        }
    }

    /// A base URL nothing is listening on.
    fn dead_base_url() -> String {
        "http://127.0.0.1:1/v1".to_owned()
    }
}

/// `POST /v1/messages` — the messages protocol, streamed or whole.
async fn mock_messages(Json(body): Json<Value>) -> Response {
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    if body["messages"]
        .as_array()
        .is_none_or(|turns| turns.is_empty())
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": { "message": "messages must not be empty" } })),
        )
            .into_response();
    }

    if body["stream"].as_bool().unwrap_or(false) {
        let mut sse = String::new();
        sse.push_str(&format!(
            "event: message_start\ndata: {}\n\n",
            json!({ "type": "message_start", "message": { "usage": { "input_tokens": 6 } } })
        ));
        sse.push_str(&format!(
            "event: content_block_delta\ndata: {}\n\n",
            json!({ "type": "content_block_delta", "delta": { "type": "text_delta", "text": "Hello from messages." } })
        ));
        sse.push_str(&format!(
            "event: message_delta\ndata: {}\n\n",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn" },
                "usage": { "output_tokens": 5 },
            })
        ));
        sse.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/event-stream")],
            sse,
        )
            .into_response();
    }

    Json(json!({
        "id": "msg_mock",
        "model": model,
        "content": [{ "type": "text", "text": "Hello from messages." }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 6, "output_tokens": 5 },
    }))
    .into_response()
}

/// `POST /v1/models/{model}:generateContent` — the generateContent shape.
async fn mock_generate(Path(op): Path<String>, Json(body): Json<Value>) -> Response {
    let streaming = op.ends_with(":streamGenerateContent");

    if body["contents"]
        .as_array()
        .is_none_or(|turns| turns.is_empty())
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": { "message": "contents must not be empty" } })),
        )
            .into_response();
    }

    let answer = json!({
        "candidates": [{
            "content": { "parts": [{ "text": "Hello from Gemini." }] },
            "finishReason": "STOP",
        }],
        "usageMetadata": { "promptTokenCount": 3, "candidatesTokenCount": 4, "totalTokenCount": 7 },
    });

    // The platform always asks for a stream, so the mock answers the streaming operation: the
    // same chunk shape, then a final chunk carrying the finish reason and the usage.
    if streaming {
        let mut sse = String::new();
        sse.push_str(&format!(
            "data: {}\n\n",
            json!({ "candidates": [{ "content": { "parts": [{ "text": "Hello from Gemini." }] } }] })
        ));
        sse.push_str(&format!(
            "data: {}\n\n",
            json!({
                "candidates": [{ "content": { "parts": [] }, "finishReason": "STOP" }],
                "usageMetadata": { "promptTokenCount": 3, "candidatesTokenCount": 4, "totalTokenCount": 7 },
            })
        ));
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/event-stream")],
            sse,
        )
            .into_response();
    }

    Json(answer).into_response()
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

/// `GET /v1/models` on a Gemini-shaped provider: the same call, the other list shape.
///
/// The mock routes by method above, so this handler is bound to its own path and the Gemini
/// provider in the suite is pointed at it — the shape difference is the point being proven.
async fn mock_gemini_models() -> Json<Value> {
    Json(json!({
        "models": [
            { "name": "models/gemini-1.5-pro" },
            { "name": "models/gemini-1.5-flash" },
        ]
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

    // Discovery asks the provider what it serves, and answers a diff. Slice 1 proved the list;
    // slice 2 made this a diff, so the assertion is the diff's own shape — the endpoint's list
    // under `reported`, and what applying it would do under the counts.
    let discovered = harness
        .call(post(
            &format!("/api/v1/ai/providers/{provider_id}/discover-models"),
            json!({}),
            Some(&token),
        ))
        .await;
    assert_eq!(discovered.status, StatusCode::OK, "{:?}", discovered.body);
    assert_eq!(
        discovered.body["reported"],
        json!(["mock-large", "mock-small"]),
        "the provider's own list comes back sorted"
    );
    assert_eq!(
        discovered.body["stored_count"], 2,
        "both models are registered"
    );
    assert_eq!(
        discovered.body["up_to_date"], true,
        "the endpoint serves exactly what the registry holds, so the diff is empty"
    );
    assert_eq!(
        discovered.body["lines"].as_array().map(Vec::len),
        Some(0),
        "a registry that already matches produces no lines"
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

/// REQ-097 slice 1 over the real router: the protocol list, the three adapters, and the
/// connection test — green against a live endpoint, and naming the step against a dead one.
#[tokio::test]
async fn the_three_protocols_connect_test_and_stream_through_one_normalised_shape() {
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
    let token = token_of(&owner);

    // The form's vocabulary: three protocols, and the bounds the same constants enforce.
    let protocols = harness
        .call(get("/api/v1/ai/protocols", Some(&token)))
        .await;
    assert_eq!(protocols.status, StatusCode::OK, "{:?}", protocols.body);
    let keys: Vec<&str> = protocols.body["protocols"]
        .as_array()
        .expect("a protocol list")
        .iter()
        .map(|entry| entry["protocol"].as_str().expect("a key"))
        .collect();
    assert_eq!(
        keys,
        vec!["openai_compatible", "anthropic_messages", "google_gemini"]
    );
    assert_eq!(protocols.body["bounds"]["timeout_ms_min"], 1000);
    assert_eq!(protocols.body["bounds"]["timeout_ms_max"], 120000);
    assert_eq!(protocols.body["bounds"]["max_retries_max"], 5);

    // A protocol outside the three is refused, and the refusal names the supported values.
    let refused = harness
        .call(post(
            "/api/v1/ai/providers",
            json!({ "name": "Robot", "protocol": "grpc", "base_url": mock.base_url }),
            Some(&token),
        ))
        .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        refused.body
    );
    assert_eq!(refused.body["error"]["code"], "invalid_provider");
    let message = refused.body["error"]["message"]
        .as_str()
        .unwrap_or_default();
    for supported in ["openai_compatible", "anthropic_messages", "google_gemini"] {
        assert!(
            message.contains(supported),
            "the refusal names {supported}: {message}"
        );
    }

    // Each protocol connects, keeps its kind, and starts with an unknown health verdict.
    for (protocol, kind, base_url, model_key, expected_answer) in [
        (
            "openai_compatible",
            "local",
            mock.base_url.as_str(),
            "mock-small",
            "Hello from the mock",
        ),
        (
            "anthropic_messages",
            "cloud",
            mock.base_url.as_str(),
            "mock-large",
            "Hello from messages.",
        ),
        // Gemini's own list shape is served from the same mock under a second prefix, so the
        // adapter's `models/`-stripping is proven against a real body rather than a fixture.
        (
            "google_gemini",
            "cloud",
            mock.gemini_base_url.as_str(),
            "gemini-1.5-pro",
            "Hello from Gemini.",
        ),
    ] {
        let created = harness
            .call(post(
                "/api/v1/ai/providers",
                json!({
                    "name": format!("Provider {protocol}"),
                    "protocol": protocol,
                    "kind": kind,
                    "base_url": base_url,
                    "timeout_ms": 20000,
                    "max_retries": 2,
                    "priority": 50,
                    "models": [model_key],
                }),
                Some(&token),
            ))
            .await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "{protocol}: {:?}",
            created.body
        );
        assert_eq!(created.body["protocol"], protocol);
        assert_eq!(created.body["kind"], kind);
        assert_eq!(created.body["last_health"], "unknown");
        assert_eq!(created.body["timeout_ms"], 20000);
        assert_eq!(created.body["max_retries"], 2);
        assert_eq!(created.body["priority"], 50);
        assert!(
            created.body.get("api_key").is_none(),
            "{protocol}: the key never comes back"
        );

        // The connection test: five steps, every applicable one green, on a live endpoint.
        let provider_id = created.body["id"].as_str().expect("an id");
        let tested = harness
            .call(post(
                &format!("/api/v1/ai/providers/{provider_id}/test"),
                json!({}),
                Some(&token),
            ))
            .await;
        assert_eq!(tested.status, StatusCode::OK, "{protocol}: {}", tested.text);
        assert_eq!(tested.body["ok"], true, "{protocol}: {}", tested.text);
        let steps = tested.body["steps"].as_array().expect("steps");
        assert_eq!(
            steps.len(),
            5,
            "{protocol}: the five steps are always reported"
        );
        for step in steps {
            // A plain-http local endpoint has no certificate to check, so that step is skipped —
            // and it says so rather than ticking itself green. Every other step must be `ok`.
            if step["step"] == "tls" && step["status"] == "skipped" {
                assert_eq!(step["note"], "the endpoint is plain http", "{protocol}");
                continue;
            }
            assert_eq!(step["status"], "ok", "{protocol}: {}", step);
        }

        assert!(tested.body["total_ms"].as_i64().is_some());
        assert_eq!(tested.body["protocol"], protocol);

        // And a streamed answer through the same adapter reaches the panel in one shape.
        let streamed = harness
            .call(post(
                "/api/v1/ai/chat",
                chat(Some(&format!("Provider {protocol}/{model_key}"))),
                Some(&token),
            ))
            .await;
        assert_eq!(
            streamed.status,
            StatusCode::OK,
            "{protocol}: {}",
            streamed.text
        );
        let events = sse_events(&streamed.text);
        assert_eq!(
            events[0].0, "start",
            "{protocol}: the start frame comes first"
        );
        assert_eq!(
            events[0].1["protocol"], protocol,
            "{protocol}: the start frame names the protocol it spoke"
        );
        let answer = streamed_answer(&events);
        assert!(
            answer.contains(expected_answer),
            "{protocol}: the answer came through the adapter, got: {answer}"
        );
        let done = events
            .iter()
            .find(|(name, _)| name == "done")
            .expect("a done frame");
        assert!(
            done.1["usage"].is_null() || done.1["usage"]["total_tokens"].is_number(),
            "{protocol}: usage is reported or honestly null, never invented: {}",
            done.1
        );
    }

    harness.dispose().await;
}

/// The connection test against an endpoint that is not there names the step and stops there.
#[tokio::test]
async fn the_connection_test_names_the_failing_step_of_a_dead_endpoint() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

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
    let token = token_of(&owner);

    let created = harness
        .call(post(
            "/api/v1/ai/providers",
            json!({
                "name": "Local Ollama",
                "protocol": "openai_compatible",
                "kind": "local",
                "base_url": MockProvider::dead_base_url(),
            }),
            Some(&token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let provider_id = created.body["id"].as_str().expect("an id");

    let tested = harness
        .call(post(
            &format!("/api/v1/ai/providers/{provider_id}/test"),
            json!({}),
            Some(&token),
        ))
        .await;
    assert_eq!(
        tested.status,
        StatusCode::OK,
        "a failing test is a report, not an HTTP failure"
    );
    assert_eq!(tested.body["ok"], false);
    assert_eq!(tested.body["failing_step"], "resolve");
    let steps = tested.body["steps"].as_array().expect("steps");
    assert_eq!(steps[0]["status"], "failed");
    assert!(
        steps[0]["error"].as_str().unwrap_or_default().len() > 0,
        "the failing step carries its own reason"
    );
    // Nothing after the first failure claims a verdict.
    for step in steps.iter().skip(1) {
        assert_eq!(step["status"], "pending", "{}", step);
    }

    // The verdict is stored, so the list shows a provider that failed its test.
    let listed = harness
        .call(get("/api/v1/ai/providers", Some(&token)))
        .await;
    let row = listed.body["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .find(|provider| provider["name"] == "Local Ollama")
        .expect("the provider");
    assert_eq!(row["last_health"], "down");
    assert!(
        row["last_error"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    );
    assert!(row["last_checked_at"].as_str().is_some());

    // And the failure is audited under its own event.
    let audit = harness.call(get("/api/v1/iam/audit", Some(&token))).await;
    assert_eq!(audit.status, StatusCode::OK, "{}", audit.text);
    let actions: Vec<&str> = audit.body["entries"]
        .as_array()
        .expect("audit entries")
        .iter()
        .filter_map(|entry| entry["action"].as_str())
        .collect();
    assert!(
        actions.contains(&"ai.provider.test_failed"),
        "a failed test is audited under its own event: {actions:?}"
    );

    harness.dispose().await;
}

/// A provider pointed at a platform metadata endpoint is refused before a socket is opened.
#[tokio::test]
async fn a_metadata_endpoint_is_never_dialled() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

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
    let token = token_of(&owner);

    let created = harness
        .call(post(
            "/api/v1/ai/providers",
            json!({
                "name": "Cloud metadata",
                "kind": "cloud",
                "base_url": "http://169.254.169.254/latest/meta-data",
            }),
            Some(&token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let provider_id = created.body["id"].as_str().expect("an id");

    let tested = harness
        .call(post(
            &format!("/api/v1/ai/providers/{provider_id}/test"),
            json!({}),
            Some(&token),
        ))
        .await;
    assert_eq!(tested.body["ok"], false);
    assert_eq!(tested.body["failing_step"], "resolve");
    assert!(
        tested.body["steps"][0]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("metadata endpoint"),
        "{}",
        tested.body["steps"][0]["error"]
    );

    harness.dispose().await;
}

/// The connection test is a `manage` power, and the numeric bounds are enforced where the form
/// validates them.
#[tokio::test]
async fn the_runtime_columns_and_the_test_permission_are_enforced() {
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

    // Every numeric bound is refused with the same code the form reads.
    for (field, value) in [
        ("timeout_ms", 10),
        ("timeout_ms", 200_000),
        ("max_retries", -1),
        ("max_retries", 9),
        ("priority", 0),
        ("priority", 5000),
    ] {
        let refused = harness
            .call(post(
                "/api/v1/ai/providers",
                json!({
                    "name": format!("Bounds {field} {value}"),
                    "base_url": mock.base_url,
                    field: value,
                }),
                Some(&owner_token),
            ))
            .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{field}={value}: {:?}",
            refused.body
        );
        assert_eq!(refused.body["error"]["code"], "invalid_provider");
    }

    let bad_kind = harness
        .call(post(
            "/api/v1/ai/providers",
            json!({ "name": "Odd kind", "kind": "on-premises", "base_url": mock.base_url }),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(bad_kind.status, StatusCode::BAD_REQUEST);
    assert!(
        bad_kind.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("cloud, local"),
        "{:?}",
        bad_kind.body
    );

    // A valid provider, then the test: the owner may run it, a member may not.
    let created = harness
        .call(post(
            "/api/v1/ai/providers",
            json!({
                "name": "Local",
                "kind": "local",
                "base_url": mock.base_url,
                "timeout_ms": 5000,
                "max_retries": 0,
                "priority": 10,
                "models": ["mock-small"],
            }),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let provider_id = created.body["id"].as_str().expect("an id").to_owned();

    let denied = harness
        .call(post(
            &format!("/api/v1/ai/providers/{provider_id}/test"),
            json!({}),
            Some(&member_token),
        ))
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{:?}", denied.body);
    assert_eq!(denied.body["error"]["code"], "permission_denied");

    // An update carries the runtime columns through, and each is validated in place.
    let updated = harness
        .call(request(
            Method::PATCH,
            &format!("/api/v1/ai/providers/{provider_id}"),
            Some(&owner_token),
            Some(json!({
                "kind": "cloud",
                "timeout_ms": 45000,
                "max_retries": 3,
                "priority": 20,
            })),
        ))
        .await;
    assert_eq!(updated.status, StatusCode::OK, "{:?}", updated.body);
    assert_eq!(updated.body["kind"], "cloud");
    assert_eq!(updated.body["timeout_ms"], 45000);
    assert_eq!(updated.body["max_retries"], 3);
    assert_eq!(updated.body["priority"], 20);

    // The failover chain is the enabled providers in priority order, ties broken by name.
    let second = harness
        .call(post(
            "/api/v1/ai/providers",
            json!({
                "name": "Aaa local",
                "base_url": mock.base_url,
                "priority": 20,
                "models": ["mock-small"],
            }),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(second.status, StatusCode::CREATED, "{:?}", second.body);
    let chain = omnion_ai_hub::failover_chain(harness.db.pool())
        .await
        .expect("the chain reads");
    assert_eq!(
        chain
            .iter()
            .map(|provider| provider.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Aaa local", "Local"],
        "a shared priority is broken by the name, so the order is total"
    );

    // An unknown provider id is a 404, never a 403 and never another installation's row.
    let unknown = harness
        .call(post(
            &format!("/api/v1/ai/providers/{}/test", Uuid::new_v4()),
            json!({}),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND, "{:?}", unknown.body);
    assert_eq!(unknown.body["error"]["code"], "provider_not_found");

    harness.dispose().await;
}

/// Slice 2 (REQ-097): the capability flags are data the registry edits and the router enforces,
/// and discovery is a reviewable diff rather than a write.
///
/// The mock serves `mock-small` and `mock-large`; the walk registers one model by hand, edits
/// its flags, asks for a stream it cannot serve, reads a diff, applies it, and reads the diff
/// again — which has to be empty, or the first apply did not do what it said.
#[tokio::test]
async fn the_capability_flags_and_the_discovery_diff_are_one_row_the_router_reads() {
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

    // One model registered by hand, with the flags the walk will read and edit.
    let created = harness
        .call(post(
            "/api/v1/ai/providers",
            json!({
                "name": "Flags",
                "kind": "local",
                "base_url": mock.base_url,
                "models": [{
                    "key": "mock-small",
                    "display_name": "Small",
                    "context_window": 8192,
                    "max_output_tokens": 2048,
                    "supports_tools": true,
                }],
            }),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let provider_id = created.body["id"].as_str().expect("an id").to_owned();

    let listed = harness
        .call(get("/api/v1/ai/models", Some(&owner_token)))
        .await;
    assert_eq!(listed.status, StatusCode::OK);
    let model = listed.body["models"]
        .as_array()
        .expect("a model list")
        .first()
        .expect("the registered model")
        .clone();
    let model_id = model["id"].as_str().expect("a model id").to_owned();

    // The panel's catalog and the row's own claims come from the same response, and `chat` is
    // already on because a registered model is a chat model by construction.
    let catalog: Vec<&str> = model["capability_catalog"]
        .as_array()
        .expect("a catalog")
        .iter()
        .filter_map(|entry| entry["capability"].as_str())
        .collect();
    assert_eq!(
        catalog,
        vec![
            "chat",
            "streaming",
            "tools",
            "vision",
            "json_mode",
            "embeddings",
            "image_generation",
            "audio_generation",
            "transcription",
            "list_models",
        ],
        "the closed vocabulary travels with the row"
    );
    assert_eq!(model["max_output_tokens"], 2048);
    let claimed: Vec<&str> = model["capabilities"]
        .as_array()
        .expect("claimed")
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    assert!(claimed.contains(&"chat"));
    assert!(claimed.contains(&"tools"));
    assert!(!claimed.contains(&"vision"), "nothing published vision yet");

    // Switching streaming off is refused by the router before a call leaves the process: the
    // chat answers 400 with the model and the capability, not a provider error.
    let refused = harness
        .call(request(
            Method::PATCH,
            &format!("/api/v1/ai/models/{model_id}"),
            Some(&owner_token),
            Some(json!({ "supports_streaming": false })),
        ))
        .await;
    assert_eq!(refused.status, StatusCode::OK, "{:?}", refused.body);
    assert_eq!(refused.body["supports_streaming"], false);

    let chat_refused = harness
        .call(post(
            "/api/v1/ai/chat",
            chat(Some("Flags/mock-small")),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(
        chat_refused.status,
        StatusCode::BAD_REQUEST,
        "a model that cannot stream must refuse the stream: {:?}",
        chat_refused.body
    );
    assert_eq!(chat_refused.body["error"]["code"], "capability_unsupported");
    let message = chat_refused.body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(message.contains("mock-small"), "{message}");
    assert!(message.contains("streaming"), "{message}");

    // The token limits are checked where the operator edits them, not at request time.
    let too_big = harness
        .call(request(
            Method::PATCH,
            &format!("/api/v1/ai/models/{model_id}"),
            Some(&owner_token),
            Some(json!({ "max_output_tokens": 0 })),
        ))
        .await;
    assert_eq!(
        too_big.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        too_big.body
    );
    assert_eq!(too_big.body["error"]["code"], "invalid_model");

    let over_context = harness
        .call(request(
            Method::PATCH,
            &format!("/api/v1/ai/models/{model_id}"),
            Some(&owner_token),
            Some(json!({ "context_window": 1024, "max_output_tokens": 4096 })),
        ))
        .await;
    assert_eq!(
        over_context.status,
        StatusCode::BAD_REQUEST,
        "an answer longer than the context that holds it: {:?}",
        over_context.body
    );

    // The flags the panel shows equal the flags the router reads — asserted against the same
    // row, in one request, after streaming is switched back on.
    let restored = harness
        .call(request(
            Method::PATCH,
            &format!("/api/v1/ai/models/{model_id}"),
            Some(&owner_token),
            Some(json!({ "supports_streaming": true, "supports_vision": true })),
        ))
        .await;
    assert_eq!(restored.status, StatusCode::OK);
    let shown: Vec<&str> = restored.body["capabilities"]
        .as_array()
        .expect("capabilities")
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    assert!(shown.contains(&"streaming"), "{shown:?}");
    assert!(shown.contains(&"vision"), "{shown:?}");
    assert_eq!(restored.body["supports_vision"], true);

    // Discovery reads the endpoint and writes nothing: the diff names both sides.
    let discovered = harness
        .call(post(
            &format!("/api/v1/ai/providers/{provider_id}/discover-models"),
            json!({}),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(discovered.status, StatusCode::OK, "{:?}", discovered.body);
    assert_eq!(discovered.body["added"], 1, "mock-large is new");
    assert_eq!(discovered.body["removed"], 0);
    assert_eq!(discovered.body["up_to_date"], false);
    assert_eq!(discovered.body["reported_count"], 2);
    assert_eq!(discovered.body["stored_count"], 1);

    // Reading a diff does not change the registry: the model count is still one.
    let unchanged = harness
        .call(get("/api/v1/ai/models", Some(&owner_token)))
        .await;
    assert_eq!(
        unchanged.body["models"].as_array().map(Vec::len),
        Some(1),
        "a discovery read must never write"
    );

    // Applying adds exactly the models the diff named, and keeps the flags the operator gave
    // the row that survived.
    let applied = harness
        .call(post(
            &format!("/api/v1/ai/providers/{provider_id}/apply-discovery"),
            json!({}),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(applied.status, StatusCode::OK, "{:?}", applied.body);
    assert_eq!(applied.body["added"], 1);

    let after = harness
        .call(get("/api/v1/ai/models", Some(&owner_token)))
        .await;
    let models = after.body["models"].as_array().expect("a model list");
    assert_eq!(models.len(), 2, "the added model is there");
    let kept = models
        .iter()
        .find(|model| model["model_key"] == "mock-small")
        .expect("the hand-registered model");
    assert_eq!(
        kept["supports_vision"], true,
        "an apply must not reset flags"
    );
    assert_eq!(kept["max_output_tokens"], 2048);
    let added = models
        .iter()
        .find(|model| model["model_key"] == "mock-large")
        .expect("the discovered model");
    assert_eq!(
        added["supports_vision"], false,
        "a discovered model claims nothing it was not told"
    );
    assert_eq!(added["supports_streaming"], true);

    // Re-running discovery over the same endpoint reports an empty diff — the case that makes
    // the first apply trustworthy.
    let second = harness
        .call(post(
            &format!("/api/v1/ai/providers/{provider_id}/discover-models"),
            json!({}),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(second.body["up_to_date"], true, "{:?}", second.body);
    assert_eq!(second.body["added"], 0);
    assert_eq!(second.body["removed"], 0);
    assert_eq!(
        second.body["lines"].as_array().map(Vec::len),
        Some(0),
        "a second run must find nothing to do"
    );

    // A removal is reported too: the registry carries a model the endpoint does not serve.
    let planted = harness
        .call(request(
            Method::PUT,
            &format!("/api/v1/ai/providers/{provider_id}/models"),
            Some(&owner_token),
            Some(json!({ "models": [
                { "key": "mock-small" },
                { "key": "mock-large" },
                { "key": "retired-model" },
            ] })),
        ))
        .await;
    assert_eq!(planted.status, StatusCode::OK, "{:?}", planted.body);

    let third = harness
        .call(post(
            &format!("/api/v1/ai/providers/{provider_id}/discover-models"),
            json!({}),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(third.body["removed"], 1, "{:?}", third.body);
    assert_eq!(third.body["lines"][0]["action"], "removed");
    assert_eq!(third.body["lines"][0]["model_key"], "retired-model");

    // The chat still works on the model the walk edited, so an edit did not break the pair.
    let answered = harness
        .call(post(
            "/api/v1/ai/chat",
            chat(Some("Flags/mock-small")),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(answered.status, StatusCode::OK, "{:?}", answered.body);
    assert!(!streamed_answer(&sse_events(&answered.text)).is_empty());

    harness.dispose().await;
}

/// Slice 3 (REQ-097): the health samples, the computed status and the failover order.
///
/// The unit tests in `crates/ai-hub` prove the *rules* without a database; this walk proves the
/// SQL those rules run against, which is where the interesting mistakes are — a `percentile_cont`
/// that returns the wrong type, a verdict written beside the sample instead of with it, an order
/// PUT that quietly drops a provider.
///
/// It takes samples through the store (the same call `POST /ai/providers/{id}/probe` makes) and
/// then reads the verdict back out of the row, so a status the panel shows and a status the
/// database holds cannot drift apart without a test failing.
#[tokio::test]
async fn health_samples_compute_a_status_and_the_order_is_a_permutation() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

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
    let owner_token = bearer(&owner.body["token"]);

    let created = harness
        .call(post(
            "/api/v1/ai/providers",
            json!({
                "name": "Health probe target",
                "base_url": "http://127.0.0.1:1/v1",
                "priority": 10,
                "models": ["mock-small"],
            }),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let provider_id = created.body["id"].as_str().expect("an id").to_owned();
    let provider_uuid = Uuid::parse_str(&provider_id).expect("a uuid");

    // A fresh provider has never been probed, so its status is `unknown` — not `ok`, and not
    // `degraded` for a fault nobody has observed yet.
    let fresh = omnion_ai_hub::health_store::health_summary(harness.db.pool(), provider_uuid, 24)
        .await
        .expect("the summary reads");
    assert_eq!(fresh.status, "unknown");
    assert_eq!(fresh.sample_count, 0);
    assert!(
        fresh.uptime_percent.is_none(),
        "a provider with no samples has no uptime number, not 100%"
    );

    // One failed probe is a sample and a transition, but not a downgrade to `down`: one dropped
    // packet is not an outage.
    let (sample, transition) = omnion_ai_hub::health_store::record_sample(
        harness.db.pool(),
        omnion_ai_hub::health_store::NewSample {
            provider_id: provider_uuid,
            ok: false,
            latency_ms: 0,
            http_status: None,
            error: Some("connection refused".to_owned()),
        },
    )
    .await
    .expect("the sample records");
    assert_eq!(
        sample.status, "down",
        "the sample carries what the probe saw"
    );
    assert_eq!(sample.error.as_deref(), Some("connection refused"));
    assert_eq!(
        transition.map(|(from, to)| (from.as_str(), to.as_str())),
        Some(("unknown", "degraded")),
        "the verdict is computed from the samples, not copied from one"
    );

    // Three consecutive failures take it down, and each step reports its own transition so the
    // caller knows when to emit `ai.provider.health_changed`.
    let mut seen: Vec<(&'static str, &'static str)> = Vec::new();
    for _ in 0..2 {
        let (_, transition) = omnion_ai_hub::health_store::probe_now(
            harness.db.pool(),
            provider_uuid,
            omnion_ai_hub::health_store::NewSample {
                provider_id: provider_uuid,
                ok: false,
                latency_ms: 0,
                http_status: Some(503),
                error: Some("503 service unavailable".to_owned()),
            },
        )
        .await
        .expect("the probe records");
        if let Some((from, to)) = transition {
            seen.push((from.as_str(), to.as_str()));
        }
    }
    assert_eq!(
        seen,
        vec![("degraded", "down")],
        "the status only reports a transition when it actually changed"
    );

    // The stored verdict agrees with the computation, read straight out of the row.
    let row: (String,) = sqlx::query_as("select last_health from ai_providers where id = $1")
        .bind(provider_uuid)
        .fetch_one(harness.db.pool())
        .await
        .expect("the provider row reads");
    assert_eq!(row.0, "down");

    // Recovery: the degraded window keeps it degraded on the first success, and the second success
    // inside a clear window would be what returns it to `ok`.
    let samples =
        omnion_ai_hub::health_store::recent_samples(harness.db.pool(), provider_uuid, 24, 50)
            .await
            .expect("the samples read");
    assert_eq!(samples.len(), 3, "one sample per probe, newest first");

    let summary = omnion_ai_hub::health_store::health_summary(harness.db.pool(), provider_uuid, 24)
        .await
        .expect("the summary reads");
    assert_eq!(summary.status, "down");
    assert_eq!(summary.sample_count, 3);
    assert!(
        (summary.uptime_percent.expect("uptime") - 0.0).abs() < 0.01,
        "three failures is 0% uptime, not a rounding of some other number"
    );
    assert_eq!(
        summary.last_error.as_deref(),
        Some("503 service unavailable"),
        "the header leads with the endpoint's own words"
    );

    // A provider taken down is still in the chain — the operator decides that, not the probe — but
    // the preview says what state it is in.
    let preview = omnion_ai_hub::health_store::failover_preview(harness.db.pool())
        .await
        .expect("the chain reads");
    let entry = preview
        .iter()
        .find(|entry| entry.name == "Health probe target")
        .expect("the provider is in the chain");
    assert_eq!(entry.rank, 1);
    assert_eq!(entry.health, "down");

    // The order PUT is a permutation, not a free-for-all: a list that drops the only enabled
    // provider is refused rather than leaving the installation with an empty chain.
    let wrong =
        omnion_ai_hub::health_store::set_failover_order(harness.db.pool(), &[Uuid::new_v4()]).await;
    assert!(wrong.is_err(), "an order of strangers is refused");

    let empty = omnion_ai_hub::health_store::set_failover_order(harness.db.pool(), &[]).await;
    assert!(empty.is_err(), "an empty chain is refused");

    // A real permutation is accepted, and the stored priorities come back in that order.
    omnion_ai_hub::health_store::set_failover_order(harness.db.pool(), &[provider_uuid])
        .await
        .expect("a valid order persists");
    let after = omnion_ai_hub::health_store::failover_preview(harness.db.pool())
        .await
        .expect("the chain reads");
    assert_eq!(after.first().map(|entry| entry.rank), Some(1));

    // The usage counters sum the rows the runtime recorded — the acceptance criterion is an
    // equality with the table, not an estimate.
    for (index, outcome) in ["ok", "ok", "error"].iter().enumerate() {
        omnion_ai_hub::health_store::record_usage(
            harness.db.pool(),
            omnion_ai_hub::health_store::NewUsage {
                provider_id: provider_uuid,
                model_key: Some("mock-small".to_owned()),
                task: "chat".to_owned(),
                outcome: (*outcome).to_owned(),
                http_status: Some(200),
                prompt_tokens: Some(10 + index as i32),
                completion_tokens: Some(5),
                latency_ms: 100 + index as i32,
                substituted_from: None,
                first_byte_at: None,
            },
        )
        .await
        .expect("the usage records");
    }

    let usage = omnion_ai_hub::health_store::usage_summary(harness.db.pool(), provider_uuid, 24)
        .await
        .expect("the usage reads");
    assert_eq!(usage.requests, 3);
    assert_eq!(usage.errors, 1);
    assert_eq!(usage.prompt_tokens, 10 + 11 + 12);
    assert_eq!(usage.completion_tokens, 15);
    assert_eq!(usage.missing_usage, 0, "every row here reported its tokens");
    assert!(
        (usage.error_rate_percent - 33.33).abs() < 0.1,
        "one error in three calls is 33%, got {}",
        usage.error_rate_percent
    );
    assert_eq!(usage.by_day.len(), 1, "all three calls are from today");

    // A call that reported no usage is counted as unknown, not as zero — otherwise a stream that
    // ended without a usage frame would quietly become a real number in the totals.
    omnion_ai_hub::health_store::record_usage(
        harness.db.pool(),
        omnion_ai_hub::health_store::NewUsage {
            provider_id: provider_uuid,
            model_key: Some("mock-small".to_owned()),
            task: "chat".to_owned(),
            outcome: "ok".to_owned(),
            http_status: Some(200),
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 90,
            substituted_from: None,
            first_byte_at: None,
        },
    )
    .await
    .expect("the usage records");

    let with_unknown =
        omnion_ai_hub::health_store::usage_summary(harness.db.pool(), provider_uuid, 24)
            .await
            .expect("the usage reads");
    assert_eq!(with_unknown.requests, 4);
    assert_eq!(with_unknown.missing_usage, 1);
    assert_eq!(
        with_unknown.prompt_tokens, 33,
        "the unknown call contributes nothing and is reported separately"
    );

    // Pruning drops what is outside the window and keeps what is inside it.
    let pruned = omnion_ai_hub::health_store::prune(harness.db.pool(), 30)
        .await
        .expect("the pruner runs");
    assert_eq!(pruned, 0, "nothing here is 30 days old yet");
    let still_there =
        omnion_ai_hub::health_store::recent_samples(harness.db.pool(), provider_uuid, 24, 50)
            .await
            .expect("the samples read");
    assert_eq!(
        still_there.len(),
        3,
        "a prune inside the window changes nothing"
    );

    // Removing the provider takes its samples and its counters with it: they are facts about a
    // connection that no longer exists, not history worth keeping.
    let removed = harness
        .call(request(
            Method::DELETE,
            &format!("api/v1/ai/providers/{provider_id}"),
            Some(&owner_token),
            None,
        ))
        .await;
    assert_eq!(removed.status, StatusCode::OK, "{:?}", removed.body);

    let orphan_samples: (i64,) =
        sqlx::query_as("select count(*) from ai_provider_health where provider_id = $1")
            .bind(provider_uuid)
            .fetch_one(harness.db.pool())
            .await
            .expect("the count reads");
    assert_eq!(orphan_samples.0, 0, "the samples went with the provider");

    let orphan_usage: (i64,) =
        sqlx::query_as("select count(*) from ai_provider_usage where provider_id = $1")
            .bind(provider_uuid)
            .fetch_one(harness.db.pool())
            .await
            .expect("the count reads");
    assert_eq!(orphan_usage.0, 0, "the counters went with it too");

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
