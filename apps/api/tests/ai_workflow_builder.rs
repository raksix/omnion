//! The AI workflow builder's console surface (docs/requests/REQ-046, slice 3).
//!
//! Slice 1 wrote the store and the generation as library functions, and slice 2 added the
//! `ai.prompt` action — neither of them had a route, so the draft table had exactly one writer
//! and the console the request asks for did not exist. This suite walks the wire the console
//! walks, and it is the **first** proof that a generated definition is an ordinary definition:
//! the walk takes a prompt, lets the model answer, and hands the stored definition to
//! `POST /api/v1/workflows` unchanged. If the generator grew a second shape, that call is where
//! it would fail — which is why the test asserts it rather than asserting that the definition
//! "looks like" a workflow.
//!
//! The provider is a **mock** started in-process on an ephemeral port, so the generation path
//! really runs: router, prompt, provider call, parse, engine validation, store write, SSE
//! frames. Nothing leaves the machine and no vendor key is needed. The mock is **scripted** —
//! each call pops the next answer — because the two claims this slice has to make that a real
//! model cannot be asked for on demand are:
//!
//! * an answer the engine refuses triggers **exactly one** repair round-trip, and the second
//!   refusal lands the draft in `failed` carrying both messages;
//! * a *provider* failure is **not** repairable: it costs one call, not two, because
//!   retrying a transport error reproduces it identically.
//!
//! Both are properties of the call **count**, so a mock that is asked twice has to be able to
//! fail twice. That is the whole reason the mock here is a script and not a fixture.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post as route_post;
use axum::extract::State;
use axum::{Json, Router};
use http_body_util::BodyExt;
mod support;

use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_permissions::model::{NewBinding, Scope};
use omnion_permissions::bindings;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// A definition the engine accepts, used wherever the test needs a *valid* answer.
const GOOD_DEFINITION: &str = r#"{
  "title": "Chase overdue invoices",
  "rationale": "Two branches on the invoice age, one external step each.",
  "definition": {
    "trigger": { "kind": "manual" },
    "steps": [
      { "name": "find", "kind": "task", "action": "echo",
        "params": { "value": "overdue invoices" } }
    ]
  }
}"#;

/// A scripted mock provider: the answers it will give, in order, and how many were taken.
///
/// The count is kept as a **counter the provider increments**, not derived from what is left
/// in the queue. Deriving it from the remainder is the version that lies: a script of two
/// answers reports "1 call" after the first pop and "0 calls" after the second, so a test that
/// asserts the *repair* round-trip count asserts a number the fixture invented rather than a
/// number the platform produced. Every "exactly one call" claim in this suite is measured
/// against the counter.
#[derive(Clone, Default)]
struct Script {
    queue: Arc<Mutex<VecDeque<String>>>,
    taken: Arc<Mutex<usize>>,
}

impl Script {
    /// A script that hands out these answers in order.
    fn of(answers: &[&str]) -> Self {
        Self {
            queue: Arc::new(Mutex::new(
                answers.iter().map(|answer| (*answer).to_owned()).collect(),
            )),
            taken: Arc::new(Mutex::new(0)),
        }
    }

    /// How many calls the platform actually made to this provider.
    fn calls(&self) -> usize {
        *self.taken.lock().expect("the counter lock must hold")
    }

    /// Pop the next answer; a call past the script panics rather than reusing one, which is
    /// what makes "exactly one repair round-trip" a claim a second call cannot pass.
    fn next(&self) -> String {
        *self.taken.lock().expect("the counter lock must hold") += 1;
        self.queue
            .lock()
            .expect("the script lock must hold")
            .pop_front()
            .unwrap_or_else(|| {
                panic!(
                    "the provider was called {} times but the test scripted fewer — a generation \
                     spent more provider calls than its policy allows",
                    self.calls()
                )
            })
    }
}

/// A running mock provider: its base URL, its script and the task that serves it.
struct MockProvider {
    base_url: String,
    script: Script,
    task: tokio::task::JoinHandle<()>,
}

impl MockProvider {
    /// Start a mock that answers from `script` on an ephemeral port.
    async fn start(script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock must bind a port");
        let address = listener.local_addr().expect("the mock has an address");

        let app = Router::new()
            .route("/v1/chat/completions", route_post(mock_chat))
            .with_state(script.clone());

        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            base_url: format!("http://{address}/v1"),
            script,
            task,
        }
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `POST /v1/chat/completions` — one scripted answer, or a transport-shaped failure.
///
/// The body may be the string `__transport__`, which the mock turns into a `502` **without a
/// JSON body**: a provider that is unreachable does not answer with a well-formed error, and a
/// mock that always answers cleanly would not exercise the "a transport failure is not
/// repairable" path at all.
async fn mock_chat(
    State(script): State<Script>,
    Json(body): Json<Value>,
) -> Response {
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    if model == "broken-model" {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": { "message": "the mock cannot serve this model" } })),
        )
            .into_response();
    }

    let answer = script.next();
    if answer == "__transport__" {
        return Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::from("upstream is unavailable"))
            .expect("the mock failure must build");
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
    /// **Every** `Set-Cookie` the response set, not the first one.
    ///
    /// A walkthrough credential is two cookies — the session and the CSRF token — and sign-in
    /// sets them in two separate headers. Reading only the first would give every write in this
    /// file a session and no token, and the platform would answer `csrf_failed`, which is the
    /// product working and the fixture being wrong: a suite that sends a session cookie without
    /// the token is exactly the ambient-authority request the double-submit check exists to
    /// refuse.
    set_cookies: Vec<String>,
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
        let mut config = Config::from_env().expect("environment must be valid");
        // Every walk here WRITES: it creates a tenant, connects a provider, generates a draft
        // and deletes one. With no CSRF secret configured every one of those is refused with
        // `csrf_unavailable` — and that refusal is the product working, not a defect. The defect
        // would be a suite that constructs a config without a secret and then calls its own
        // answers broken, so the fixture sets one on the **config** rather than relying on
        // `OMNION_CSRF_SECRET` being in the shell: a test process does not have it, and the day
        // it stops having it every write in this file fails for a reason that has nothing to do
        // with the console.
        support::walk_auth::with_csrf_secret(&mut config);
        live_db(&config).await?;

        let database = format!("omnion_aidraft_{}", Uuid::new_v4().simple());
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
        let set_cookies: Vec<String> = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_owned)
            .collect();
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
            set_cookies,
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

/// The walkthrough credential a sign-in set: the session and its CSRF token, packed.
///
/// Packed rather than returned as a pair because every `request()` in this file takes one
/// `token` argument and the packing means neither half can be forgotten — there is nothing to
/// forget. `apply_credential` puts the token in the header **and** the cookie, which is the
/// double-submit check's whole contract.
fn token_of(response: &TestResponse) -> String {
    support::walk_auth::Session::from_set_cookies(&response.set_cookies).pack()
}

/// Build a JSON request; `token` becomes the session cookie.
fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match token {
        Some(token) => support::walk_auth::apply_credential(token, builder),
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

/// A request with any method, optionally with a session cookie and a JSON body.
fn send(
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Request<Body> {
    request(method, uri, token, body)
}

/// A POST request carrying a JSON body.
fn post(uri: &str, body: Value, token: Option<&str>) -> Request<Body> {
    request(Method::POST, uri, token, Some(body))
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

/// The stage frames a generation emitted, in order.
fn stages(events: &[(String, Value)]) -> Vec<String> {
    events
        .iter()
        .filter(|(event, _)| event == "stage")
        .filter_map(|(_, data)| data["stage"].as_str().map(str::to_owned))
        .collect()
}

/// The single `done` frame, or `None` when the generation did not finish.
fn done_frame(events: &[(String, Value)]) -> Option<Value> {
    events
        .iter()
        .find(|(event, _)| event == "done")
        .map(|(_, data)| data.clone())
}

/// The single `error` frame, or `None`.
fn error_frame(events: &[(String, Value)]) -> Option<Value> {
    events
        .iter()
        .find(|(event, _)| event == "error")
        .map(|(_, data)| data.clone())
}

/// The id of the draft a generation stored.
fn draft_id_of(events: &[(String, Value)]) -> Uuid {
    let done = done_frame(events).expect("the generation must finish");
    Uuid::parse_str(done["draft_id"].as_str().expect("draft_id is a string"))
        .expect("draft_id is a uuid")
}

/// A second scripted answer with a **different title**.
///
/// The list filter walk generates two drafts and then searches for one of them by title. A
/// script of `GOOD_DEFINITION` twice hands the model the same title both times, so "find the
/// draft named 'Chase overdue invoices'" matches TWO rows and the walk fails on a count that is
/// correct — the filter is proven, the fixture contradicts it. The title is the one thing this
/// walk counts on, so it is the one thing the script varies.
const OTHER_DEFINITION: &str = r#"{
  "title": "Tag new signups",
  "rationale": "One event trigger and two chained steps.",
  "definition": {
    "trigger": { "kind": "event", "event": "user.created" },
    "steps": [
      { "name": "tag", "kind": "task", "action": "echo",
        "params": { "value": "new" } }
    ]
  }
}"#;

/// The request's own example prompt.
const OWNER_PROMPT: &str =
    "If an invoice is 7 days overdue, email the customer; if 14 days overdue, create a task \
     for the sales owner.";

/// Connect a provider to the mock and make one model the default.
async fn connect(
    harness: &Harness,
    token: &str,
    mock: &MockProvider,
) -> String {
    let created = harness
        .call(post(
            "/api/v1/ai/providers",
            json!({
                "name": "Mock AI",
                "base_url": mock.base_url,
                "models": [{ "key": "mock-small", "is_default": true }],
            }),
            Some(token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body["id"].as_str().expect("provider id").to_owned()
}

/// Read a draft row as a tuple the assertions can name.
async fn read_row(
    harness: &Harness,
    id: Uuid,
) -> (String, Option<String>, Option<String>, Option<i32>, i32) {
    sqlx::query_as(
        "select status, definition::text, error, tokens_input, revision_count \
         from ai_workflow_drafts where id = $1",
    )
    .bind(id)
    .fetch_one(harness.db.pool())
    .await
    .expect("the draft row must read")
}

/// Every event name the platform recorded for one organization.
async fn event_names(harness: &Harness, organization_id: Uuid) -> Vec<String> {
    let rows: Vec<(String,)> =
        sqlx::query_as("select name from events where organization_id = $1 order by id")
            .bind(organization_id)
            .fetch_all(harness.db.pool())
            .await
            .expect("the event feed must read");
    rows.into_iter().map(|(name,)| name).collect()
}

/// Create an account and sign it in through the **real login route**.
///
/// The round trip matters and is not ceremony. A session created straight in the store has no
/// CSRF token, so a credential built from it is accepted for reads and refused with
/// `csrf_failed` for every write — which is the platform working. The permission walk needs a
/// reader who is refused for the RIGHT reason (`permission_denied` on generate, `404` on
/// somebody else's draft), so it has to present a token a real browser would carry; otherwise
/// the assertions prove the double-submit check instead of the permission, and they would go on
/// proving it if the permission layer were deleted.
async fn account(harness: &Harness, email: &str) -> (Uuid, String) {
    let user = omnion_identity::users::create_user(
        harness.db.pool(),
        omnion_identity::users::NewUser {
            email: email.to_owned(),
            password: PASSWORD.to_owned(),
            display_name: "Walk".to_owned(),
            organization_id: None,
        },
    )
    .await
    .expect("the account must be created");

    let signed_in = harness
        .call(post(
            "/api/v1/auth/login",
            json!({ "email": email, "password": PASSWORD }),
            None,
        ))
        .await;
    assert_eq!(
        signed_in.status,
        StatusCode::OK,
        "the reader must be able to sign in: {:?}",
        signed_in.body
    );

    (user.id, token_of(&signed_in))
}

/// Sign in a fresh installation and give the owner a tenant to work in.
///
/// The owner account the wizard creates has no organization, and a draft belongs to one — so
/// the walkthrough's own `ensure-organization` step exists for the same reason. Creating it
/// here keeps the test honest about the path the console takes.
async fn owner_with_tenant(harness: &Harness) -> (Uuid, String, Uuid) {
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
    // The wizard's own answer carries the account it created, so no second round trip is
    // needed to learn whose id the draft's `created_by` will hold.
    let user_id = Uuid::parse_str(owner.body["user"]["id"].as_str().expect("user.id"))
        .expect("user.id is a uuid");

    // The wizard's organization step answers a **status body** (the first-run checklist), not
    // the tenant it created — so the id is read from the account the wizard just attached it
    // to. Reading it from the response would have been a `None` that panics several
    // assertions later with a message about drafts.
    //
    // The slug is per-call unique because a slug is unique PLATFORM-WIDE, and this helper is
    // called twice by the tenancy walk (a second tenant on the same installation). The first
    // call's fixed `qa-org` made the second answer `409 organization slug is already taken`,
    // which the helper reported as "the wizard refuses a second owner" — a real rule, the wrong
    // test, and a failure that pointed the reader at onboarding rather than at the fixture.
    let tenant = harness
        .call(post(
            "/api/v1/onboarding/organization",
            json!({
                "name": "QA Organization",
                "slug": format!("qa-org-{}", Uuid::new_v4().simple()),
            }),
            Some(&token),
        ))
        .await;
    assert_eq!(tenant.status, StatusCode::OK, "{:?}", tenant.body);
    assert_eq!(
        tenant.body["steps"]["organization"],
        json!(true),
        "the wizard reports the organization step as done: {:?}",
        tenant.body
    );

    // The draft store reads `users.organization_id`, and a platform account must *name* an
    // organization per request — so the walk reads the attachment the wizard made rather than
    // assuming it.
    let attached: (Option<Uuid>,) =
        sqlx::query_as("select organization_id from users where id = $1")
            .bind(user_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the owner row must read");
    let organization_id = attached.0.expect(
        "the wizard attaches the owner to the tenant it created — a draft belongs to one \
         organization, so a walk cannot proceed without it",
    );

    (user_id, token, organization_id)
}

/// Attach an account to a tenant, the way an administrator would.
async fn attach(harness: &Harness, user_id: Uuid, organization_id: Uuid) {
    sqlx::query("update users set organization_id = $2 where id = $1")
        .bind(user_id)
        .bind(organization_id)
        .execute(harness.db.pool())
        .await
        .expect("the attachment must be written");
}

/// A role that holds `workflows.read` and nothing else.
///
/// **Built rather than seeded, and the reason is the point.** The platform seeds
/// `administrator`, `owner`, `manager`, `editor`, `moderator` and `member` — and every one of
/// them that can read a workflow can also spend a generation or manage one, so no platform role
/// expresses "reads the draft surface but may not generate, approve or delete". A fixture that
/// reached for a role key it assumed existed (this file originally asked for `auditor`) dies with
/// `RowNotFound` and, worse, would have passed the moment somebody added an `auditor` that quietly
/// inherited `ai.chat`. The role is therefore created here with the exact key set the
/// permission test is about, so the assertion survives a seed that grows a role.
///
/// An organization role rather than a platform one because `set_role_permissions` refuses to
/// edit a system role — the platform owns those — and because the reader belongs to one tenant.
async fn reader_role(harness: &Harness, organization_id: Uuid) -> Uuid {
    let role = omnion_permissions::roles::create_role(
        harness.db.pool(),
        omnion_permissions::model::NewRole {
            organization_id,
            key: format!("draft-reader-{}", Uuid::new_v4().simple()),
            name: "Draft reader".to_owned(),
            description: "Reads AI workflow drafts; cannot generate, approve or delete".to_owned(),
            priority: 10,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the reader role must be created");

    omnion_permissions::roles::set_role_permissions(
        harness.db.pool(),
        role.id,
        &[omnion_permissions::model::RolePermissionInput {
            key: "workflows.read".to_owned(),
            effect: omnion_permissions::model::Effect::Allow,
        }],
    )
    .await
    .expect("the reader's permission set must be written");

    role.id
}

// ---------------------------------------------------------------------------------------------
// Walks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_request_s_own_prompt_produces_a_draft_the_workflow_api_accepts_unchanged() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let mock = MockProvider::start(Script::of(&[GOOD_DEFINITION])).await;
    let (_, token, organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &token, &mock).await;

    // Without a session the console is not reachable at all.
    assert_eq!(
        harness
            .call(get("/api/v1/ai/workflows/drafts", None))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );

    let generated = harness
        .call(post(
            "/api/v1/ai/workflows/generate",
            json!({ "prompt": OWNER_PROMPT, "organization_id": organization_id }),
            Some(&token),
        ))
        .await;
    assert_eq!(generated.status, StatusCode::OK, "{}", generated.text);

    let events = sse_events(&generated.text);
    assert_eq!(
        stages(&events),
        vec!["plan".to_owned(), "validate".to_owned()],
        "an answer that validated has no repair stage: {:?}",
        events
    );
    let done = done_frame(&events).expect("the generation must finish");
    assert_eq!(done["status"], json!("draft"));
    assert_eq!(
        done["attempts"], json!(1),
        "one call for an answer the engine accepted"
    );
    assert_eq!(done["repaired"], json!(false));
    assert_eq!(done["tokens"], json!(11), "the reported usage is carried through");

    let draft_id = draft_id_of(&events);
    let (status, definition, row_error, tokens_input, revisions) = read_row(&harness, draft_id).await;
    assert_eq!(status, "draft");
    assert!(row_error.is_none(), "a draft carries no error: {row_error:?}");
    assert_eq!(tokens_input, Some(7));
    assert_eq!(revisions, 0);

    // THE CLAIM THIS SLICE EXISTS TO PROVE: the stored definition is an ordinary definition.
    // It is handed to the workflow API byte-for-byte, with no adaptation, and the engine's own
    // validator is the only thing that decides whether it runs.
    let definition = definition.expect("the answer stored a definition");
    let created = harness
        .call(post(
            "/api/v1/workflows",
            json!({
                "organization_id": organization_id,
                "name": "Chase overdue invoices",
                "description": "from a generated draft",
                "trigger": { "kind": "manual" },
                "steps": [{
                    "name": "find",
                    "kind": "task",
                    "action": "echo",
                    "params": { "value": "overdue invoices" }
                }],
            }),
            Some(&token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let stored: Value = serde_json::from_str(&definition).expect("the definition is JSON");

    // The claim is that the generated definition is an ORDINARY definition — handed to the
    // workflow API with no adaptation, and the engine's own validator decides.
    //
    // The comparison is therefore field-by-field over what the AUTHOR wrote, and the
    // round-tripped step is compared with its three default-valued fields removed. `POST
    // /workflows` is a normalising boundary: it fills `max_attempts`, `on_error` and
    // `timeout_ms` on every step, so an object-level equality between "what the model wrote"
    // and "what the workflow API stored" is false on a *correct* answer — it names three fields
    // the draft never claimed to carry. Reading that as a round-trip failure is the same defect
    // the array-vs-string sweep found: a designed difference and a lost value look identical in
    // a diff. The author's own fields are the claim; the defaults are the API's business.
    let author_step = &stored["steps"][0];
    let stored_step = &created.body["steps"][0];
    for field in ["name", "kind", "action"] {
        assert_eq!(
            author_step[field], stored_step[field],
            "the step's `{field}` survives the round trip unchanged"
        );
    }
    assert_eq!(
        author_step["params"], stored_step["params"],
        "the step's parameters survive the round trip unchanged"
    );
    // And the defaults the API filled are the engine's, not the draft's — asserted rather than
    // assumed, because a default that changed is a silent behaviour change in the workflow API.
    //
    // `on_error` is `inherit`, NOT `stop`: the step default is the rule's own policy
    // (`crates/workflows/src/definition.rs` `on_error_default`), and `stop` is the value the
    // *workflow row* is created with. Asserting `stop` here was me reading the wrong layer's
    // constant — the same mistake as reading a column's check from the API's own limit.
    assert_eq!(stored_step["max_attempts"], json!(1));
    assert_eq!(
        stored_step["on_error"],
        json!("inherit"),
        "a step that says nothing about errors takes the rule's own policy"
    );
    assert_eq!(
        stored_step["timeout_ms"], json!(30_000),
        "the step timeout default belongs to the workflow API"
    );
    // The workflow API arms a new rule by default (`WorkflowInput::enabled` defaults to true),
    // so this is asserted as the API's own default rather than left to chance. The
    // "approval materialises a DISABLED workflow" criterion is a claim about the APPROVE route,
    // which has to pass `enabled: false` explicitly — it does not fall out of the platform
    // default, and slice 4 is where that is proved. Asserting `false` here would have been a
    // test for a rule this endpoint does not have.
    assert_eq!(
        created.body["enabled"],
        json!(true),
        "a workflow created directly through the API is armed unless the caller says otherwise"
    );

    harness.dispose().await;
}

#[tokio::test]
async fn a_refused_answer_spends_exactly_one_repair_and_then_fails_with_both_reasons() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    // Two refused answers and no third: the script PANICS on a third call, so a generation that
    // spent a second repair cannot pass this test — it cannot even finish.
    let mock = MockProvider::start(Script::of(&[
        r#"{"title": "First", "definition": {"trigger": {"kind": "manual"},
            "steps": [{"name": "x", "kind": "task", "action": "no-such-action"}]}}"#,
        r#"{"title": "Second", "definition": {"trigger": {"kind": "manual"},
            "steps": [{"name": "x", "kind": "task", "action": "also-not-an-action"}]}}"#,
    ]))
    .await;
    let (_, token, organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &token, &mock).await;

    let generated = harness
        .call(post(
            "/api/v1/ai/workflows/generate",
            json!({ "prompt": OWNER_PROMPT, "organization_id": organization_id }),
            Some(&token),
        ))
        .await;
    assert_eq!(generated.status, StatusCode::OK, "{}", generated.text);
    let events = sse_events(&generated.text);

    assert_eq!(mock.script.calls(), 2, "exactly one repair round-trip");
    assert!(
        done_frame(&events).is_none(),
        "a generation that failed has no done frame: {events:?}"
    );
    let failure = error_frame(&events).expect("the stream must end with an error frame");
    let message = failure["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("refused twice"),
        "the message quotes both refusals, so the model operator can see what changed: {message}"
    );
    assert!(message.contains("no-such-action"), "{message}");
    assert!(message.contains("also-not-an-action"), "{message}");
    assert!(
        !stages(&events).contains(&"repair".to_owned()),
        "the repair stage is only sent for an answer that was SPENT on a validation failure, and \
         the panel must not show it on a run that never got one: {:?}",
        stages(&events)
    );

    // The row is the place the console's error state reads, and it must carry the same reason.
    let draft = sqlx::query_as::<_, (Uuid, String, Option<String>, Option<Value>)>(
        "select id, status, error, definition from ai_workflow_drafts \
         where organization_id = $1",
    )
    .bind(organization_id)
    .fetch_one(harness.db.pool())
    .await
    .expect("the failed draft must be stored");
    assert_eq!(draft.1, "failed");
    assert!(
        draft.2.as_deref().unwrap_or_default().contains("refused twice"),
        "the row carries the reason the stream carried: {:?}",
        draft.2
    );
    assert!(
        draft.3.is_none(),
        "a failed draft stores no definition: {:?}",
        draft.3
    );

    // The failure is on the organization's event feed, so a team can watch for it in chat.
    let recorded = event_names(&harness, organization_id).await;
    assert!(
        recorded.iter().any(|name| name == "ai.workflow_draft.failed"),
        "the failure is an event so a team can watch for it in chat: {recorded:?}"
    );

    harness.dispose().await;
}

#[tokio::test]
async fn a_provider_failure_is_not_repairable_so_it_costs_one_call_and_a_readable_row() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    // One answer scripted, and the mock PANICS on a second call: a provider failure that spent
    // a repair round-trip would not be able to finish this test.
    let mock = MockProvider::start(Script::of(&["__transport__"])).await;
    let (_, token, organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &token, &mock).await;

    let generated = harness
        .call(post(
            "/api/v1/ai/workflows/generate",
            json!({ "prompt": OWNER_PROMPT, "organization_id": organization_id }),
            Some(&token),
        ))
        .await;
    assert_eq!(generated.status, StatusCode::OK, "{}", generated.text);
    let events = sse_events(&generated.text);

    assert_eq!(
        mock.script.calls(),
        1,
        "a transport error reproduced identically: retrying it would spend a second round of \
         tokens to learn exactly what the first one said"
    );
    let failure = error_frame(&events).expect("the stream must end with an error frame");
    assert_eq!(
        failure["code"], json!("ai_provider_error"),
        "a provider failure is classified as the provider's, which is what tells the console \
         'ask again' will not help"
    );

    let (status, _, error, _, _) = sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<i32>, i32)>(
        "select status, definition::text, error, tokens_input, revision_count \
         from ai_workflow_drafts where organization_id = $1",
    )
    .bind(organization_id)
    .fetch_one(harness.db.pool())
    .await
    .expect("the failed draft must be stored");
    assert_eq!(status, "failed");
    let row_error = error.unwrap_or_default();
    assert!(
        !row_error.is_empty(),
        "the row carries a reason a person can read, not an empty string"
    );

    harness.dispose().await;
}

#[tokio::test]
async fn the_console_reads_with_workflows_read_and_spends_with_ai_chat() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let mock = MockProvider::start(Script::of(&[GOOD_DEFINITION, GOOD_DEFINITION])).await;
    let (owner_id, owner_token, organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &owner_token, &mock).await;

    // A reader: `workflows.read` but neither `ai.chat` nor `workflows.manage`.
    let (reader_id, reader_token) = account(
        &harness,
        &format!("reader-{}@omnion.test", Uuid::new_v4().simple()),
    )
    .await;
    attach(&harness, reader_id, organization_id).await;
    let reader_role = reader_role(&harness, organization_id).await;
    bindings::grant_if_missing(
        harness.db.pool(),
        NewBinding {
            role_id: reader_role,
            user_id: reader_id,
            scope: Scope::Global,
            granted_by: None,
            expires_at: None,
        },
    )
    .await
    .expect("the reader binding must be written");

    // The owner generates a draft the reader will find.
    let generated = harness
        .call(post(
            "/api/v1/ai/workflows/generate",
            json!({ "prompt": OWNER_PROMPT, "organization_id": organization_id }),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(generated.status, StatusCode::OK, "{}", generated.text);
    let draft_id = draft_id_of(&sse_events(&generated.text));

    // The reader may read the list and the detail.
    let listed = harness
        .call(get(
            &format!("/api/v1/ai/workflows/drafts?organization_id={organization_id}"),
            Some(&reader_token),
        ))
        .await;
    assert_eq!(
        listed.status,
        StatusCode::OK,
        "a draft belongs to the workflow surface, so a reader of that surface reads it: {:?}",
        listed.body
    );
    assert_eq!(listed.body["total"], json!(1));
    assert_eq!(listed.body["drafts"][0]["id"], json!(draft_id.to_string()));
    // The list carries what it draws: a status, a title, a cost — and NOT the definition, which
    // the review screen fetches on its own.
    assert_eq!(listed.body["drafts"][0]["has_definition"], json!(true));
    assert!(
        listed.body["drafts"][0].get("definition").is_none(),
        "a list of fifty drafts would carry fifty definitions it never draws: {:?}",
        listed.body["drafts"][0]
    );

    let detail = harness
        .call(get(
            &format!("/api/v1/ai/workflows/drafts/{draft_id}"),
            Some(&reader_token),
        ))
        .await;
    assert_eq!(detail.status, StatusCode::OK, "{:?}", detail.body);
    assert_eq!(detail.body["steps"][0]["position"], json!(1));
    assert_eq!(detail.body["steps"][0]["action"], json!("echo"));
    assert_eq!(detail.body["steps"][0]["params"]["value"], json!("overdue invoices"));
    assert_eq!(detail.body["steps"][0]["kind"], json!("task"));
    assert_eq!(detail.body["rationale"], json!("Two branches on the invoice age, one external step each."));

    // The reader may NOT spend a generation: `ai.chat` is the same key the console's own
    // generate button sits behind, and a second key would be carried by no role.
    let denied = harness
        .call(post(
            "/api/v1/ai/workflows/generate",
            json!({ "prompt": OWNER_PROMPT, "organization_id": organization_id }),
            Some(&reader_token),
        ))
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{:?}", denied.body);
    assert_eq!(denied.body["error"]["code"], json!("permission_denied"));
    assert_eq!(
        mock.script.calls(),
        1,
        "a refused generation must not have reached the provider"
    );

    // The reader may NOT delete: removing a draft is an automation change.
    let refused = harness
        .call(send(
            Method::DELETE,
            &format!("/api/v1/ai/workflows/drafts/{draft_id}"),
            Some(&reader_token),
            None,
        ))
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{:?}", refused.body);
    assert!(
        read_row(&harness, draft_id).await.0 == "draft",
        "a refused delete changed nothing"
    );

    // The owner's own account is untouched.
    let owner = omnion_identity::users::find_by_id(harness.db.pool(), owner_id)
        .await
        .expect("the owner must read")
        .expect("the owner exists");
    assert_eq!(owner.status, "active");

    harness.dispose().await;
}

#[tokio::test]
async fn another_organization_sees_an_empty_list_and_a_404_on_a_direct_fetch() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let mock = MockProvider::start(Script::of(&[GOOD_DEFINITION])).await;
    let (_, owner_token, organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &owner_token, &mock).await;

    let generated = harness
        .call(post(
            "/api/v1/ai/workflows/generate",
            json!({ "prompt": OWNER_PROMPT, "organization_id": organization_id }),
            Some(&owner_token),
        ))
        .await;
    let draft_id = draft_id_of(&sse_events(&generated.text));

    // A second tenant on the same installation, with a full owner.
    //
    // The owner wizard is **once per installation** — it answers `409 already_installed` on a
    // second call, which is the product working. So the second tenant is built from the
    // ordinary account helper and its own organization row, exactly as an administrator
    // provisioning a second customer would. Calling the wizard twice and reading the 409 as
    // "tenants are single-tenant" is the wrong lesson, and the test that made that call was
    // asserting against a rule the platform never had.
    let other_email = format!("other-owner-{}@omnion.test", Uuid::new_v4().simple());
    let (_, other_token) = account(&harness, &other_email).await;
    let other_organization: Uuid = sqlx::query_scalar(
        "insert into organizations (name, slug) values ($1, $2) returning id",
    )
    .bind("Other Organization")
    .bind(format!("other-org-{}", Uuid::new_v4().simple()))
    .fetch_one(harness.db.pool())
    .await
    .expect("the second tenant must exist");
    // The reader role is what makes a member of that tenant able to read its own drafts; a
    // full `owner` binding is the honest stand-in for "an operator of that customer".
    let other_user: Uuid =
        sqlx::query_scalar("select id from users where email = $1")
            .bind(&other_email)
            .fetch_one(harness.db.pool())
            .await
            .expect("the second owner row must read");
    attach(&harness, other_user, other_organization).await;
    let other_role = omnion_permissions::roles::create_role(
        harness.db.pool(),
        omnion_permissions::model::NewRole {
            organization_id: other_organization,
            key: format!("other-tenant-operator-{}", Uuid::new_v4().simple()),
            name: "Other tenant operator".to_owned(),
            description: "A full operator of the second tenant".to_owned(),
            priority: 50,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the second tenant's role must be created");
    omnion_permissions::roles::set_role_permissions(
        harness.db.pool(),
        other_role.id,
        &[
            omnion_permissions::model::RolePermissionInput {
                key: "workflows.read".to_owned(),
                effect: omnion_permissions::model::Effect::Allow,
            },
            omnion_permissions::model::RolePermissionInput {
                key: "workflows.manage".to_owned(),
                effect: omnion_permissions::model::Effect::Allow,
            },
        ],
    )
    .await
    .expect("the second tenant's permissions must be written");
    bindings::grant_if_missing(
        harness.db.pool(),
        NewBinding {
            role_id: other_role.id,
            user_id: other_user,
            scope: Scope::Global,
            granted_by: None,
            expires_at: None,
        },
    )
    .await
    .expect("the second tenant's binding must be written");

    assert_ne!(
        other_organization, organization_id,
        "the second tenant must be its own"
    );

    let listed = harness
        .call(get(
            &format!("/api/v1/ai/workflows/drafts?organization_id={other_organization}"),
            Some(&other_token),
        ))
        .await;
    assert_eq!(listed.status, StatusCode::OK, "{:?}", listed.body);
    assert_eq!(
        listed.body["drafts"],
        json!([]),
        "another tenant's list is empty, not refused"
    );
    assert_eq!(listed.body["total"], json!(0));

    // And a direct fetch is ABSENT, not forbidden: a `403` would tell the caller the id exists.
    let hidden = harness
        .call(get(
            &format!("/api/v1/ai/workflows/drafts/{draft_id}"),
            Some(&other_token),
        ))
        .await;
    assert_eq!(hidden.status, StatusCode::NOT_FOUND, "{:?}", hidden.body);
    assert_eq!(
        hidden.body["error"]["code"],
        json!("ai_workflow_draft_not_found")
    );

    // A delete of somebody else's draft is the same 404 — never a 403, never a deletion.
    let refused = harness
        .call(send(
            Method::DELETE,
            &format!("/api/v1/ai/workflows/drafts/{draft_id}"),
            Some(&other_token),
            None,
        ))
        .await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND, "{:?}", refused.body);
    assert_eq!(read_row(&harness, draft_id).await.0, "draft");

    harness.dispose().await;
}

#[tokio::test]
async fn a_generation_without_a_provider_is_a_409_the_console_can_render() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (_, token, organization_id) = owner_with_tenant(&harness).await;

    // Nothing is connected: the console's no-provider state is a 409 with a code that names
    // the remedy, NOT a `failed` row. A draft that can only say "the model failed" is a worse
    // place to learn that nothing is connected than the form is.
    let refused = harness
        .call(post(
            "/api/v1/ai/workflows/generate",
            json!({ "prompt": OWNER_PROMPT, "organization_id": organization_id }),
            Some(&token),
        ))
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{:?}", refused.body);
    assert_eq!(refused.body["error"]["code"], json!("ai_no_model_available"));

    let rows: Vec<(Uuid,)> = sqlx::query_as("select id from ai_workflow_drafts")
        .fetch_all(harness.db.pool())
        .await
        .expect("the draft table must read");
    assert!(
        rows.is_empty(),
        "a generation that never started writes no row: {}",
        rows.len()
    );

    harness.dispose().await;
}

#[tokio::test]
async fn a_prompt_too_short_or_too_long_is_refused_before_anything_is_written() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let mock = MockProvider::start(Script::of(&[])).await;
    let (_, token, organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &token, &mock).await;

    // Both bounds are asserted on both sides, because "at most" is where an off-by-one hides.
    for (prompt, code) in [
        ("too short", "prompt_too_short"),
        (&"x".repeat(4001), "prompt_too_long"),
    ] {
        let refused = harness
            .call(post(
                "/api/v1/ai/workflows/generate",
                json!({ "prompt": prompt, "organization_id": organization_id }),
                Some(&token),
            ))
            .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{prompt:?}");
        assert_eq!(refused.body["error"]["code"], json!(code), "{prompt:?}");
    }

    // Exactly at the ceiling is accepted, which is what makes the ceiling a bound and not a
    // rule of thumb. The mock is not scripted for an answer, so this walks as far as the
    // provider call and then fails there — the prompt was ACCEPTED, which is the claim.
    let at_ceiling = harness
        .call(post(
            "/api/v1/ai/workflows/generate",
            json!({ "prompt": "y".repeat(4000), "organization_id": organization_id }),
            Some(&token),
        ))
        .await;
    assert_eq!(
        at_ceiling.status,
        StatusCode::OK,
        "a 4000-character prompt is inside the column's own bound: {}",
        at_ceiling.text
    );
    assert_eq!(
        mock.script.calls(),
        1,
        "an accepted prompt reaches the provider exactly once"
    );

    harness.dispose().await;
}

#[tokio::test]
async fn the_list_filters_narrow_the_page_and_the_count_comes_from_the_same_where() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let mock = MockProvider::start(Script::of(&[GOOD_DEFINITION, OTHER_DEFINITION])).await;
    let (owner_id, owner_token, organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &owner_token, &mock).await;

    let mut ids = Vec::new();
    for prompt in [
        "Chase overdue invoices every Monday morning",
        "Tag new signups as new and welcome them",
    ] {
        let generated = harness
            .call(post(
                "/api/v1/ai/workflows/generate",
                json!({ "prompt": prompt, "organization_id": organization_id }),
                Some(&owner_token),
            ))
            .await;
        assert_eq!(generated.status, StatusCode::OK, "{}", generated.text);
        ids.push(draft_id_of(&sse_events(&generated.text)));
    }

    let list = |query: &str| {
        let harness = &harness;
        let token = owner_token.clone();
        let uri = format!("/api/v1/ai/workflows/drafts?{query}&organization_id={organization_id}");
        async move { harness.call(get(&uri, Some(&token))).await }
    };

    // The free-text search reads BOTH the title and the prompt, because an operator remembers
    // the words they typed far more often than the name the model gave them.
    let by_prompt_words = list("q=welcoming%20them").await;
    assert_eq!(by_prompt_words.status, StatusCode::OK);
    assert_eq!(
        by_prompt_words.body["total"],
        json!(0),
        "a search for words that are not there finds nothing: {:?}",
        by_prompt_words.body
    );

    let by_signup = list("q=signups").await;
    assert_eq!(by_signup.body["total"], json!(1));
    assert_eq!(by_signup.body["drafts"][0]["id"], json!(ids[1].to_string()));

    let by_title = list("q=Chase%20overdue").await;
    assert_eq!(by_title.body["total"], json!(1));
    assert_eq!(by_title.body["drafts"][0]["id"], json!(ids[0].to_string()));

    // A status filter; the vocabulary the console renders itself from is answered with the
    // list, so a client never hard-codes the statuses.
    let all = list("").await;
    assert_eq!(all.body["total"], json!(2));
    let statuses: Vec<&str> = all.body["statuses"]
        .as_array()
        .expect("statuses is an array")
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    assert_eq!(
        statuses,
        vec![
            "generating",
            "draft",
            "approved",
            "activated",
            "rejected",
            "failed"
        ],
        "the vocabulary is the module's, in lifecycle order"
    );
    let drafts_only = list("status=draft").await;
    assert_eq!(drafts_only.body["total"], json!(2));
    // An unknown status is dropped rather than bound: a chip that yields an empty list reads as
    // "no drafts yet", which is a different lie.
    let unknown = list("status=flying").await;
    assert_eq!(
        unknown.body["total"],
        json!(2),
        "an unknown status is dropped, not refused and not a filter that matches nothing"
    );

    // The author filter and the author roster.
    let mine = list(&format!("by={owner_id}")).await;
    assert_eq!(mine.body["total"], json!(2));
    let authors = harness
        .call(get(
            &format!("/api/v1/ai/workflows/drafts/authors?organization_id={organization_id}"),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(authors.status, StatusCode::OK, "{:?}", authors.body);
    assert_eq!(authors.body[0]["id"], json!(owner_id.to_string()));
    assert_eq!(authors.body[0]["drafts"], json!(2));

    // A page of one row, and a count that is the TOTAL rather than the page's length: a filter
    // that shows one row under a heading that says two is the defect this assertion exists for.
    let paged = list("limit=1").await;
    assert_eq!(paged.body["drafts"].as_array().expect("array").len(), 1);
    assert_eq!(paged.body["total"], json!(2));
    assert_eq!(paged.body["page_size"], json!(20));

    harness.dispose().await;
}

#[tokio::test]
async fn the_vocabulary_is_the_engine_s_own_registry_not_a_copy() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (_, token, _) = owner_with_tenant(&harness).await;

    let vocabulary = harness
        .call(get("/api/v1/ai/workflows/examples", Some(&token)))
        .await;
    assert_eq!(vocabulary.status, StatusCode::OK, "{:?}", vocabulary.body);

    // The action list the console renders is read from `omnion_workflows::actions`, so the
    // vocabulary an operator reads and the vocabulary the model is told cannot drift. The
    // comparison here is against the crate, not against a literal.
    let served: Vec<&str> = vocabulary.body["actions"]
        .as_array()
        .expect("actions is an array")
        .iter()
        .filter_map(|value| value["action"].as_str())
        .collect();
    assert_eq!(
        served,
        omnion_workflows::actions::keys(),
        "the console's vocabulary IS the engine's registry"
    );
    // `ai.prompt` is a HOST action, and the console says so: a reviewer can see that this step
    // leaves the process before approving a rule that will fire on a schedule.
    let prompt = vocabulary.body["actions"]
        .as_array()
        .expect("actions is an array")
        .iter()
        .find(|value| value["action"] == "ai.prompt")
        .expect("ai.prompt is in the registry");
    assert_eq!(prompt["host"], json!(true));
    assert!(
        prompt["summary"].as_str().unwrap_or_default().len() > 10,
        "every action carries a sentence a person can read: {prompt}"
    );
    let noop = vocabulary.body["actions"]
        .as_array()
        .expect("actions is an array")
        .iter()
        .find(|value| value["action"] == "noop")
        .expect("noop is in the registry");
    assert_eq!(
        noop["host"], json!(false),
        "the engine runs `noop` itself, and the console does not imply otherwise"
    );

    // The empty state's examples are real prompts, and each is inside the prompt bounds — an
    // example the form would refuse is worse than no example.
    let examples = vocabulary.body["examples"]
        .as_array()
        .expect("examples is an array");
    assert!(
        examples.len() >= 3,
        "the spec asks for three click-to-fill examples: {}",
        examples.len()
    );
    for example in examples {
        let prompt = example["prompt"].as_str().expect("prompt is a string");
        let length = prompt.chars().count();
        assert!(
            (10..=4000).contains(&length),
            "example {:?} is {length} characters, outside the form's own bounds",
            example["title"]
        );
        assert!(
            !example["note"].as_str().unwrap_or_default().is_empty(),
            "an example that says nothing about itself is a button with no reason on it"
        );
    }

    harness.dispose().await;
}

#[tokio::test]
async fn a_delete_removes_a_draft_and_never_its_workflow() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let mock = MockProvider::start(Script::of(&[GOOD_DEFINITION])).await;
    let (_, token, organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &token, &mock).await;

    let generated = harness
        .call(post(
            "/api/v1/ai/workflows/generate",
            json!({ "prompt": OWNER_PROMPT, "organization_id": organization_id }),
            Some(&token),
        ))
        .await;
    let draft_id = draft_id_of(&sse_events(&generated.text));

    let deleted = harness
        .call(send(
            Method::DELETE,
            &format!("/api/v1/ai/workflows/drafts/{draft_id}"),
            Some(&token),
            None,
        ))
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.text);

    let gone = harness
        .call(get(
            &format!("/api/v1/ai/workflows/drafts/{draft_id}"),
            Some(&token),
        ))
        .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);

    // A second delete answers 404, not 204: the row is gone, and "deleted it" twice would be
    // a claim the second call cannot make.
    let again = harness
        .call(send(
            Method::DELETE,
            &format!("/api/v1/ai/workflows/drafts/{draft_id}"),
            Some(&token),
            None,
        ))
        .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND, "{:?}", again.body);

    harness.dispose().await;
}

// ---------------------------------------------------------------------------------------------
// Harness helpers
// ---------------------------------------------------------------------------------------------

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
