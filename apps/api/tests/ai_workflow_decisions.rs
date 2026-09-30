//! The AI workflow builder's **decision** surface (docs/requests/REQ-046, slice 4).
//!
//! Slice 3 put the console on the wire and rendered the approval bar honestly incomplete. This
//! suite is the proof that the bar is real: it drives the four routes a reviewer presses —
//! revise, approve, reject, test-run — plus the edited-definition save, against the same
//! in-process mock provider slice 3 uses, so a generation still really runs.
//!
//! The claims worth stating before the walks, because each of them is a way this half could
//! have been wrong and looked right:
//!
//! * **Approval creates a DISABLED workflow.** `POST /api/v1/workflows` arms a new rule by
//!   default (`enabled_by_default`). An approval that inherited that default would create an
//!   *armed schedule* out of a sentence nobody read, and every assertion here would still
//!   pass if it only checked that a workflow appeared. So the suite reads `workflows.enabled`
//!   out of the database.
//! * **A second approve answers `409` and NAMES the workflow.** Not merely "answers 409": a
//!   conflict that does not name what already happened leaves the operator with a message and
//!   no next step, which is the same as no message.
//! * **Approval is refused when the approver lacks a permission the steps need.** The rule
//!   runs as the approver (approval is what creates it), so the check is against the
//!   **approver's** effective permissions — and the spec's "never silently granted" means the
//!   refusal has to name the missing key.
//! * **A test run emits no event and writes no run.** The spec defines the dry run by its
//!   absence of effects, and the only way to prove an absence is to read the feed before and
//!   after rather than to assert the response shape.
//! * **An invalid save changes nothing.** Asserted against the stored definition, not against
//!   a `400`: a handler that validated *after* writing would answer `400` and leave a
//!   definition nobody approved.
//!
//! The harness — mock provider, scratch database, CSRF-aware credential — is slice 3's,
//! copied rather than reinvented. A second version of it would be a second answer to "what
//! does a walkthrough credential look like", and the one this file's claims depend on is the
//! one slice 3's walks are proven against.

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

/// A definition the engine accepts, used wherever the walk needs a *valid* answer.
///
/// Every step is an **engine** action (`echo`), so approving it needs no permission beyond
/// `workflows.manage` — the two walks about refused approvals use `HOST_DEFINITION` instead,
/// and the difference between the two is the whole claim.
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


/// The single `done` frame, or `None` when the generation did not finish.
fn done_frame(events: &[(String, Value)]) -> Option<Value> {
    events
        .iter()
        .find(|(event, _)| event == "done")
        .map(|(_, data)| data.clone())
}


/// The id of the draft a generation stored.
fn draft_id_of(events: &[(String, Value)]) -> Uuid {
    let done = done_frame(events).expect("the generation must finish");
    Uuid::parse_str(done["draft_id"].as_str().expect("draft_id is a string"))
        .expect("draft_id is a uuid")
}


/// A definition whose only step is a **host** action, so approval has a permission to check.
///
/// `ai.prompt` rather than `send_email` on purpose: it needs `ai.chat` — a key the owner holds
/// because the owner generated the draft with it — and the walk that must be *refused* is
/// the one where the approver holds `workflows.manage` but not `ai.chat`. Two walks, one
/// definition, and the difference between them is the approver's role.
const HOST_DEFINITION: &str = r#"{
  "title": "Summarise a ticket",
  "rationale": "One host step, so approval has a permission to check.",
  "definition": {
    "trigger": { "kind": "manual" },
    "steps": [
      { "name": "summarise", "kind": "task", "action": "ai.prompt",
        "params": { "prompt": "Summarise the record" } }
    ]
  }
}"#;

/// The prompt a reviewer asks for changes with.
const REVISION_NOTE: &str = "also email the sales owner when the invoice is fourteen days late";

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

// ---------------------------------------------------------------------------------------------
// Walks
// ---------------------------------------------------------------------------------------------

/// Generate one draft from the request's own example prompt and return its id.
async fn generate_draft(harness: &Harness, token: &str, organization_id: Uuid) -> Uuid {
    let generated = harness
        .call(post(
            "/api/v1/ai/workflows/generate",
            json!({
                "prompt": "If an invoice is 7 days overdue, email the customer; if 14 days \
                           overdue, create a task for the sales owner.",
                "organization_id": organization_id,
            }),
            Some(token),
        ))
        .await;
    assert_eq!(
        generated.status,
        StatusCode::OK,
        "the generation must stream: {}",
        generated.text
    );
    draft_id_of(&sse_events(&generated.text))
}

/// A member of the tenant, signed in, with a role holding exactly `keys`.
///
/// Built rather than picked from the seeded roles, for the reason slice 3's reader role
/// documents: no platform role expresses "may approve a draft but may not spend a
/// generation", and a fixture that reached for one would either die with `RowNotFound` or
/// start passing the day somebody added a role that quietly inherited `ai.chat`.
async fn member_with(
    harness: &Harness,
    organization_id: Uuid,
    keys: &[&str],
) -> String {
    let email = format!("member-{}@omnion.test", Uuid::new_v4().simple());
    let user = omnion_identity::users::create_user(
        harness.db.pool(),
        omnion_identity::users::NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Member".to_owned(),
            organization_id: None,
        },
    )
    .await
    .expect("the account must be created");
    attach(harness, user.id, organization_id).await;

    let role = omnion_permissions::roles::create_role(
        harness.db.pool(),
        omnion_permissions::model::NewRole {
            organization_id,
            key: format!("qa-role-{}", Uuid::new_v4().simple()),
            name: "QA role".to_owned(),
            description: "Built by the AI workflow decision walk".to_owned(),
            priority: 10,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the role must be created");
    let permissions = keys
        .iter()
        .map(|key| omnion_permissions::model::RolePermissionInput {
            key: (*key).to_owned(),
            effect: omnion_permissions::model::Effect::Allow,
        })
        .collect::<Vec<_>>();
    omnion_permissions::roles::set_role_permissions(harness.db.pool(), role.id, &permissions)
        .await
        .expect("the permission set must be written");
    bindings::grant_if_missing(
        harness.db.pool(),
        NewBinding {
            role_id: role.id,
            user_id: user.id,
            scope: Scope::Global,
            granted_by: None,
            expires_at: None,
        },
    )
    .await
    .expect("the binding must be written");

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
        "the member must be able to sign in: {:?}",
        signed_in.body
    );
    token_of(&signed_in)
}

/// Approval materialises a **disabled** workflow carrying the draft's own steps.
#[tokio::test]
async fn approval_materialises_a_disabled_workflow_with_the_drafts_steps() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let mock = MockProvider::start(Script::of(&[GOOD_DEFINITION])).await;
    let (_, token, organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &token, &mock).await;

    let draft_id = generate_draft(&harness, &token, organization_id).await;
    let approved = harness
        .call(post(
            &format!("/api/v1/ai/workflows/drafts/{draft_id}/approve"),
            json!({}),
            Some(&token),
        ))
        .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.text);

    let workflow_id = Uuid::parse_str(
        approved.body["workflow_id"]
            .as_str()
            .expect("the approval names a workflow"),
    )
    .expect("workflow_id is a uuid");

    // The **row**, not the response: the response is what the handler believed it wrote.
    let row: (bool, String, Uuid) =
        sqlx::query_as("select enabled, name, organization_id from workflows where id = $1")
            .bind(workflow_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the workflow row must exist");
    assert!(
        !row.0,
        "approval created an ARMED workflow — the review screen promises a disabled rule, \
         and the workflow API arms new rules by default"
    );
    assert_eq!(row.2, organization_id, "the rule belongs to the draft's tenant");
    assert!(!row.1.is_empty(), "a rule with no name is unidentifiable in a list");

    // The draft's own step list is what the rule runs — the spec's first criterion, proven
    // for the materialised rule rather than for the draft row.
    let steps: Value = sqlx::query_scalar("select steps from workflows where id = $1")
        .bind(workflow_id)
        .fetch_one(harness.db.pool())
        .await
        .expect("the steps must read");
    let names: Vec<&str> = steps
        .as_array()
        .expect("steps is an array")
        .iter()
        .filter_map(|step| step["name"].as_str())
        .collect();
    assert_eq!(names, vec!["find"], "the rule runs the draft's steps: {names:?}");

    // The draft moved to `activated` and points at the rule: the two writes are joined.
    let status: (String, Option<Uuid>) =
        sqlx::query_as("select status, workflow_id from ai_workflow_drafts where id = $1")
            .bind(draft_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the draft row must read");
    assert_eq!(status.0, "activated", "the draft records what it became");
    assert_eq!(status.1, Some(workflow_id));

    harness.dispose().await;
}

/// A second approve is a `409` that **names** the workflow the first one created.
#[tokio::test]
async fn a_second_approve_answers_409_naming_the_workflow_it_already_created() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let mock = MockProvider::start(Script::of(&[GOOD_DEFINITION])).await;
    let (_, token, organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &token, &mock).await;

    let draft_id = generate_draft(&harness, &token, organization_id).await;
    let first = harness
        .call(post(
            &format!("/api/v1/ai/workflows/drafts/{draft_id}/approve"),
            json!({}),
            Some(&token),
        ))
        .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.text);
    let workflow_id = first.body["workflow_id"]
        .as_str()
        .expect("workflow_id")
        .to_owned();

    let second = harness
        .call(post(
            &format!("/api/v1/ai/workflows/drafts/{draft_id}/approve"),
            json!({}),
            Some(&token),
        ))
        .await;
    assert_eq!(second.status, StatusCode::CONFLICT, "{}", second.text);
    let message = second.body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        message.contains(&workflow_id),
        "the conflict must name the workflow it already created, so the operator can go and \
         look at it: {message}"
    );

    // And no second workflow exists: the conflict is a fact about the database, not a
    // courtesy message.
    let count: (i64,) =
        sqlx::query_as("select count(*) from workflows where organization_id = $1")
            .bind(organization_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the count must read");
    assert_eq!(count.0, 1, "a second approve created a second rule");

    harness.dispose().await;
}

/// Approval is refused when the approver lacks a permission the steps need.
#[tokio::test]
async fn approval_is_refused_when_the_approver_lacks_the_permission_a_step_needs() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let mock = MockProvider::start(Script::of(&[HOST_DEFINITION])).await;
    let (_, owner_token, organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &owner_token, &mock).await;

    // The OWNER generates, so the draft exists and the host step was validated — and the
    // owner holds `ai.chat` because generating needed it.
    let draft_id = generate_draft(&harness, &owner_token, organization_id).await;

    // The approver is somebody else: `workflows.manage` but **not** `ai.chat`. A rule whose
    // only step is `ai.prompt` costs money on every firing, so it rides `ai.chat`.
    let approver_token = member_with(
        &harness,
        organization_id,
        &["workflows.read", "workflows.manage"],
    )
    .await;

    let refused = harness
        .call(post(
            &format!("/api/v1/ai/workflows/drafts/{draft_id}/approve"),
            json!({}),
            Some(&approver_token),
        ))
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.text);
    let message = refused.body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        message.contains("ai.chat"),
        "the refusal must name the missing permission, or the operator cannot act on it: \
         {message}"
    );

    // The refusal changed nothing: no decision, no workflow. "Never silently granted" is
    // only true if the alternative is a rule that exists.
    let status: (String, Option<Uuid>) =
        sqlx::query_as("select status, workflow_id from ai_workflow_drafts where id = $1")
            .bind(draft_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the draft row must read");
    assert_eq!(status.0, "draft", "a refused approval still decided the draft");
    assert!(status.1.is_none());

    let rules: (i64,) =
        sqlx::query_as("select count(*) from workflows where organization_id = $1")
            .bind(organization_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the count must read");
    assert_eq!(rules.0, 0, "a refused approval created a rule anyway");

    harness.dispose().await;
}

/// A rejection needs a reason, and the reason lands on the row with the person who typed it.
#[tokio::test]
async fn a_rejection_needs_a_reason_and_keeps_it() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let mock = MockProvider::start(Script::of(&[GOOD_DEFINITION])).await;
    let (_, token, organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &token, &mock).await;

    let draft_id = generate_draft(&harness, &token, organization_id).await;

    let bare = harness
        .call(post(
            &format!("/api/v1/ai/workflows/drafts/{draft_id}/reject"),
            json!({ "reason": "   " }),
            Some(&token),
        ))
        .await;
    assert_eq!(
        bare.status,
        StatusCode::BAD_REQUEST,
        "a whitespace reason is no reason: {}",
        bare.text
    );
    let unchanged: (String,) =
        sqlx::query_as("select status from ai_workflow_drafts where id = $1")
            .bind(draft_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the draft row must read");
    assert_eq!(
        unchanged.0, "draft",
        "a refused rejection still decided the draft"
    );

    let rejected = harness
        .call(post(
            &format!("/api/v1/ai/workflows/drafts/{draft_id}/reject"),
            json!({ "reason": "this duplicates the weekly digest rule" }),
            Some(&token),
        ))
        .await;
    assert_eq!(rejected.status, StatusCode::OK, "{}", rejected.text);

    let row: (String, Option<String>, Option<Uuid>) = sqlx::query_as(
        "select status, decision_reason, decided_by from ai_workflow_drafts where id = $1",
    )
    .bind(draft_id)
    .fetch_one(harness.db.pool())
    .await
    .expect("the draft row must read");
    assert_eq!(row.0, "rejected");
    assert_eq!(
        row.1.as_deref(),
        Some("this duplicates the weekly digest rule"),
        "the reason a person typed is the reason the author of the rule will read"
    );
    assert!(row.2.is_some(), "a decision carries who made it");

    harness.dispose().await;
}

/// A revision spends a generation and replaces the definition.
#[tokio::test]
async fn a_revision_spends_one_generation_and_replaces_the_definition() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    // TWO answers: the first generation, then the revision. A script of one would panic
    // rather than quietly reuse an answer, which is the point of a script.
    let mock = MockProvider::start(Script::of(&[GOOD_DEFINITION, GOOD_DEFINITION])).await;
    let (_, token, organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &token, &mock).await;

    let draft_id = generate_draft(&harness, &token, organization_id).await;
    assert_eq!(mock.script.calls(), 1, "the generation cost one call");

    let revised = harness
        .call(post(
            &format!("/api/v1/ai/workflows/drafts/{draft_id}/revise"),
            json!({ "note": REVISION_NOTE }),
            Some(&token),
        ))
        .await;
    assert_eq!(revised.status, StatusCode::OK, "{}", revised.text);
    let done = done_frame(&sse_events(&revised.text))
        .expect("a revision must finish with a `done` frame");
    assert_eq!(done["status"], "draft", "the revised draft is a draft again");
    assert_eq!(
        mock.script.calls(),
        2,
        "a revision is one more generation, not a free edit"
    );

    // The note is on the row and the counter moved: a revision that changed the answer but
    // not the counter would make the spend line claim one answer where two were paid for.
    let row: (String, i32, Option<Uuid>) = sqlx::query_as(
        "select revision_note, revision_count, workflow_id from ai_workflow_drafts where id = $1",
    )
    .bind(draft_id)
    .fetch_one(harness.db.pool())
    .await
    .expect("the draft row must read");
    assert_eq!(row.0, REVISION_NOTE);
    assert_eq!(row.1, 1, "the revision counter records the round-trip");
    assert!(row.2.is_none(), "a revision cannot un-decide a rule");

    // A revision with no note is refused, and it costs nothing: the refusal happens before
    // the row is reset, so a draft is never left `generating` by a form that sent nothing.
    let bare = harness
        .call(post(
            &format!("/api/v1/ai/workflows/drafts/{draft_id}/revise"),
            json!({ "note": "  " }),
            Some(&token),
        ))
        .await;
    assert_eq!(bare.status, StatusCode::BAD_REQUEST, "{}", bare.text);
    assert_eq!(
        mock.script.calls(),
        2,
        "a refused revision reached the provider"
    );
    let status: (String,) =
        sqlx::query_as("select status from ai_workflow_drafts where id = $1")
            .bind(draft_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the draft row must read");
    assert_eq!(status.0, "draft", "a refused revision left the draft generating");

    harness.dispose().await;
}

/// An edited definition is revalidated, and an invalid save changes nothing.
#[tokio::test]
async fn an_edited_definition_is_revalidated_and_an_invalid_save_changes_nothing() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let mock = MockProvider::start(Script::of(&[GOOD_DEFINITION])).await;
    let (_, token, organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &token, &mock).await;

    let draft_id = generate_draft(&harness, &token, organization_id).await;
    let before: (Value,) =
        sqlx::query_as("select definition from ai_workflow_drafts where id = $1")
            .bind(draft_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the draft row must read");

    // An action the registry does not have. The message must NAME the actions that exist,
    // because the operator is editing a model's answer and needs to know what to write.
    let unknown_action = harness
        .call(send(
            Method::PATCH,
            &format!("/api/v1/ai/workflows/drafts/{draft_id}"),
            Some(&token),
            Some(json!({
                "definition": {
                    "trigger": { "kind": "manual" },
                    "steps": [{ "name": "call", "kind": "task",
                                "action": "send_telegram", "params": {} }],
                },
            })),
        ))
        .await;
    assert_eq!(
        unknown_action.status,
        StatusCode::BAD_REQUEST,
        "{}",
        unknown_action.text
    );
    let message = unknown_action.body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        message.contains("send_email"),
        "the refusal must name the actions that exist: {message}"
    );

    // A credential typed by a human is refused exactly as one a model wrote would be: the
    // save path uses the module's validator, not only the engine's.
    let credential = harness
        .call(send(
            Method::PATCH,
            &format!("/api/v1/ai/workflows/drafts/{draft_id}"),
            Some(&token),
            Some(json!({
                "definition": {
                    "trigger": { "kind": "manual" },
                    "steps": [{ "name": "call", "kind": "task", "action": "echo",
                                "params": { "value": "hi", "api_key": "sk-live-nope" } }],
                },
            })),
        ))
        .await;
    assert_eq!(
        credential.status,
        StatusCode::BAD_REQUEST,
        "{}",
        credential.text
    );
    assert_eq!(
        credential.body["error"]["code"], "secret_in_definition",
        "a credential typed into the editor is refused the same way: {}",
        credential.text
    );

    // The row is untouched after both refusals — the criterion is "changes nothing", and a
    // handler that validated after writing would answer 400 and leave a definition nobody
    // approved.
    let after: (Value,) =
        sqlx::query_as("select definition from ai_workflow_drafts where id = $1")
            .bind(draft_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the draft row must read");
    assert_eq!(
        after.0, before.0,
        "a refused save changed the stored definition"
    );

    // A valid save lands, and the response carries the revalidated draft.
    let saved = harness
        .call(send(
            Method::PATCH,
            &format!("/api/v1/ai/workflows/drafts/{draft_id}"),
            Some(&token),
            Some(json!({
                "definition": {
                    "trigger": { "kind": "manual" },
                    "steps": [
                        { "name": "find", "kind": "task", "action": "echo",
                          "params": { "value": "overdue invoices" } },
                        { "name": "shout", "kind": "task", "action": "echo",
                          "params": { "value": "on it" } },
                    ],
                },
            })),
        ))
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.text);
    assert_eq!(
        saved.body["steps"].as_array().map(Vec::len),
        Some(2),
        "the saved definition's steps are the ones that were sent: {}",
        saved.text
    );

    harness.dispose().await;
}

/// A test run reports a plan and has no effect at all: no event, no run, no provider call.
#[tokio::test]
async fn a_test_run_reports_a_plan_and_emits_no_event_and_writes_no_run() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let mock = MockProvider::start(Script::of(&[HOST_DEFINITION])).await;
    let (_, token, organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &token, &mock).await;

    let draft_id = generate_draft(&harness, &token, organization_id).await;

    let before_events = event_names(&harness, organization_id).await;
    let before_runs: (i64,) = sqlx::query_as("select count(*) from workflow_executions")
        .fetch_one(harness.db.pool())
        .await
        .expect("the run count must read");
    let before_calls = mock.script.calls();

    let tested = harness
        .call(post(
            &format!("/api/v1/ai/workflows/drafts/{draft_id}/test-run"),
            json!({}),
            Some(&token),
        ))
        .await;
    assert_eq!(tested.status, StatusCode::OK, "{}", tested.text);

    // The plan: the step, the action, that the HOST runs it, and the permission it needs.
    let steps = tested.body["steps"]
        .as_array()
        .expect("a plan has steps")
        .clone();
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert_eq!(steps[0]["name"], "summarise");
    assert_eq!(steps[0]["action"], "ai.prompt");
    assert_eq!(
        steps[0]["host"], json!(true),
        "a reviewer must be able to see which steps leave the process: {steps:?}"
    );
    assert_eq!(
        steps[0]["permission"], "ai.chat",
        "the plan names the permission the step needs: {steps:?}"
    );
    assert_eq!(tested.body["verdict"], "ready");

    // The definition that comes back is the engine's own normalised shape, not the stored
    // bytes — which is the point of a test run: it is what the engine WOULD run.
    let normalised = &tested.body["definition"];
    assert_eq!(
        normalised["steps"][0]["on_error"], "inherit",
        "the plan shows the defaults the engine fills in: {normalised}"
    );

    // **The absence**, which is what the criterion is about. Read before and after, so a
    // handler that emitted nothing AND did nothing still passes only if both are true.
    let after_events = event_names(&harness, organization_id).await;
    assert_eq!(
        after_events, before_events,
        "a test run emitted an event: {after_events:?}"
    );
    let after_runs: (i64,) = sqlx::query_as("select count(*) from workflow_executions")
        .fetch_one(harness.db.pool())
        .await
        .expect("the run count must read");
    assert_eq!(
        after_runs.0, before_runs.0,
        "a test run wrote an execution row: {} → {}",
        before_runs.0,
        after_runs.0
    );
    assert_eq!(
        mock.script.calls(),
        before_calls,
        "a test run reached the provider"
    );

    harness.dispose().await;
}

/// The decision events reach the organization's feed, carrying identifiers and nothing else.
#[tokio::test]
async fn approving_rejecting_and_generating_are_all_on_the_event_feed() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let mock = MockProvider::start(Script::of(&[GOOD_DEFINITION, GOOD_DEFINITION])).await;
    let (_, token, organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &token, &mock).await;

    let approved_draft = generate_draft(&harness, &token, organization_id).await;
    let approved = harness
        .call(post(
            &format!("/api/v1/ai/workflows/drafts/{approved_draft}/approve"),
            json!({}),
            Some(&token),
        ))
        .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.text);
    let workflow_id = approved.body["workflow_id"]
        .as_str()
        .expect("workflow_id")
        .to_owned();

    let rejected_draft = generate_draft(&harness, &token, organization_id).await;
    let rejected = harness
        .call(post(
            &format!("/api/v1/ai/workflows/drafts/{rejected_draft}/reject"),
            json!({ "reason": "the platform already sends a digest on Mondays" }),
            Some(&token),
        ))
        .await;
    assert_eq!(rejected.status, StatusCode::OK, "{}", rejected.text);

    let names = event_names(&harness, organization_id).await;
    for expected in [
        "ai.workflow_draft.generated",
        "ai.workflow_draft.approved",
        "ai.workflow_draft.rejected",
    ] {
        assert!(
            names.iter().any(|name| name == expected),
            "the feed carries no {expected}: {names:?}"
        );
    }

    // The approved event carries the workflow id, because a receiver whose job is "tell the
    // channel we now have a rule" cannot do it from a draft id alone.
    let payload: (Value,) = sqlx::query_as(
        "select payload from events where name = 'ai.workflow_draft.approved' limit 1",
    )
    .fetch_one(harness.db.pool())
    .await
    .expect("the approved event must read");
    assert_eq!(payload.0["workflow_id"].as_str(), Some(workflow_id.as_str()));
    assert!(
        payload.0.get("prompt").is_none() && payload.0.get("definition").is_none(),
        "the event carries more than identifiers: {payload:?}"
    );

    harness.dispose().await;
}

/// A reader may read a draft and may not decide one — and none of the refusals reached the
/// store, which is the part a `403` alone does not prove.
#[tokio::test]
async fn a_reader_may_read_a_draft_and_may_not_decide_one() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let mock = MockProvider::start(Script::of(&[GOOD_DEFINITION])).await;
    let (_, token, organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &token, &mock).await;
    let draft_id = generate_draft(&harness, &token, organization_id).await;

    let reader_token = member_with(&harness, organization_id, &["workflows.read"]).await;

    // Reading works: the list and the detail.
    let listed = harness
        .call(get("/api/v1/ai/workflows/drafts", Some(&reader_token)))
        .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.text);
    let detail = harness
        .call(get(
            &format!("/api/v1/ai/workflows/drafts/{draft_id}"),
            Some(&reader_token),
        ))
        .await;
    assert_eq!(detail.status, StatusCode::OK, "{}", detail.text);

    for (name, uri, body) in [
        ("approve", "approve", json!({})),
        ("reject", "reject", json!({ "reason": "no" })),
        ("test-run", "test-run", json!({})),
        ("revise", "revise", json!({ "note": "please change it" })),
    ] {
        let refused = harness
            .call(post(
                &format!("/api/v1/ai/workflows/drafts/{draft_id}/{uri}"),
                body,
                Some(&reader_token),
            ))
            .await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "a reader must not {name}: {}",
            refused.text
        );
    }

    let status: (String, Option<Uuid>) =
        sqlx::query_as("select status, workflow_id from ai_workflow_drafts where id = $1")
            .bind(draft_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the draft row must read");
    assert_eq!(
        status.0, "draft",
        "a reader's refused calls still decided the draft"
    );
    assert!(
        status.1.is_none(),
        "a reader's refused approve still made a rule"
    );
    let rules: (i64,) =
        sqlx::query_as("select count(*) from workflows where organization_id = $1")
            .bind(organization_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the count must read");
    assert_eq!(
        rules.0, 0,
        "a reader's refused approve still created a rule"
    );

    harness.dispose().await;
}

/// Another organization gets a `404` on every decision, and the draft survives untouched.
#[tokio::test]
async fn another_organization_gets_a_404_on_every_decision_and_the_draft_survives() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let mock = MockProvider::start(Script::of(&[GOOD_DEFINITION])).await;
    let (_, token, organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &token, &mock).await;
    let draft_id = generate_draft(&harness, &token, organization_id).await;

    // A second tenant on the same installation.
    //
    // **The owner wizard is once per installation** — a second call answers
    // `409 already_installed`, which is the product working, not a rule that tenants are
    // single. So the second tenant is built the way an administrator provisioning a second
    // customer would: an ordinary account plus its own organization row. The slug is
    // unique platform-wide, so a fixed one would make the *insert* fail instead and point
    // the reader at the wrong statement.
    let other_organization: Uuid = sqlx::query_scalar(
        "insert into organizations (name, slug) values ($1, $2) returning id",
    )
    .bind("Other Organization")
    .bind(format!("other-org-{}", Uuid::new_v4().simple()))
    .fetch_one(harness.db.pool())
    .await
    .expect("the second tenant must exist");
    // The keys are the ones a tenant operator really holds — and every key the four routes
    // below name, including `ai.chat` and `workflows.run`.
    //
    // **The list is the point, and it was found the hard way.** The first version granted
    // `workflows.read` + `workflows.manage`, and `test-run` came back **403
    // `workflows.run`** instead of 404. Read as a product defect that would be alarming — the
    // tenant boundary leaking — and it is the opposite: the permission middleware runs first,
    // the member really did lack the key, and 403 was the correct answer. The walk wanted to
    // isolate **tenancy**, so it has to hand the caller everything tenancy is not. One test
    // asserting four routes can only isolate one variable if all four answer the same
    // question, and a `403` mixed into a `404` sweep means the fixture moved the question.
    let other_token = member_with(
        &harness,
        other_organization,
        &[
            "workflows.read",
            "workflows.manage",
            "workflows.run",
            "ai.chat",
        ],
    )
    .await;
    assert_ne!(
        other_organization, organization_id,
        "the second tenant must be its own"
    );

    // A `404`, not a `403`: distinguishing "not yours" from "no such draft" tells the
    // caller which ids exist anywhere in the platform.
    for (uri, body) in [
        ("approve", json!({})),
        ("reject", json!({ "reason": "not mine" })),
        ("test-run", json!({})),
        ("revise", json!({ "note": "not mine either" })),
    ] {
        let refused = harness
            .call(post(
                &format!("/api/v1/ai/workflows/drafts/{draft_id}/{uri}"),
                body,
                Some(&other_token),
            ))
            .await;
        assert_eq!(
            refused.status,
            StatusCode::NOT_FOUND,
            "another tenant must get a 404 from {uri}, not a 403: {}",
            refused.text
        );
    }

    // And the save path is scoped the same way: a PATCH by another tenant is a 404, not a
    // silent overwrite of somebody else's definition.
    let patched = harness
        .call(send(
            Method::PATCH,
            &format!("/api/v1/ai/workflows/drafts/{draft_id}"),
            Some(&other_token),
            Some(json!({
                "definition": {
                    "trigger": { "kind": "manual" },
                    "steps": [{ "name": "theirs", "kind": "task", "action": "noop",
                                "params": {} }],
                },
            })),
        ))
        .await;
    assert_eq!(patched.status, StatusCode::NOT_FOUND, "{}", patched.text);

    let row: (String, Value) =
        sqlx::query_as("select status, definition from ai_workflow_drafts where id = $1")
            .bind(draft_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the draft row must read");
    assert_eq!(row.0, "draft", "another tenant's calls decided the draft");
    assert_eq!(
        row.1["steps"][0]["name"], "find",
        "another tenant overwrote the definition: {}",
        row.1
    );

    harness.dispose().await;
}

/// A draft that is already a rule cannot be decided, revised or edited again — the record of
/// the decision is what those refusals protect.
#[tokio::test]
async fn a_draft_that_is_already_a_rule_cannot_be_decided_or_revised_again() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let mock = MockProvider::start(Script::of(&[GOOD_DEFINITION])).await;
    let (_, token, organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &token, &mock).await;

    let draft_id = generate_draft(&harness, &token, organization_id).await;
    assert_eq!(
        harness
            .call(post(
                &format!("/api/v1/ai/workflows/drafts/{draft_id}/approve"),
                json!({}),
                Some(&token),
            ))
            .await
            .status,
        StatusCode::OK
    );

    for (uri, body) in [
        ("revise", json!({ "note": "change it again" })),
        ("reject", json!({ "reason": "changed my mind" })),
    ] {
        let refused = harness
            .call(post(
                &format!("/api/v1/ai/workflows/drafts/{draft_id}/{uri}"),
                body,
                Some(&token),
            ))
            .await;
        assert_eq!(
            refused.status,
            StatusCode::CONFLICT,
            "{uri} on a draft that is a rule must be a conflict: {}",
            refused.text
        );
        assert!(
            refused.body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("builder"),
            "the refusal must say where to change the rule instead: {}",
            refused.text
        );
    }

    // A test run still works: a rule that exists is a rule somebody may want to check, and
    // a dry run has no effect to be careful about.
    assert_eq!(
        harness
            .call(post(
                &format!("/api/v1/ai/workflows/drafts/{draft_id}/test-run"),
                json!({}),
                Some(&token),
            ))
            .await
            .status,
        StatusCode::OK
    );

    // And the save path refuses too: a definition edited after the decision would leave the
    // decision describing a different rule than the one stored beside it.
    let patched = harness
        .call(send(
            Method::PATCH,
            &format!("/api/v1/ai/workflows/drafts/{draft_id}"),
            Some(&token),
            Some(json!({
                "definition": {
                    "trigger": { "kind": "manual" },
                    "steps": [{ "name": "different", "kind": "task", "action": "noop",
                                "params": {} }],
                },
            })),
        ))
        .await;
    assert_eq!(patched.status, StatusCode::CONFLICT, "{}", patched.text);

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
