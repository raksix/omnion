//! Integration tests for the expression autocomplete (docs/requests/REQ-086, slice 3).
//!
//! `crates/workflows/src/completion.rs` proves what the candidate list decides. This file proves
//! the three things only a real round trip can, and each one is a claim the editor depends on:
//!
//! 1. the answer comes from **the graph the caller sent**, not the stored one. A completion
//!    list built from the saved graph answers against a topology the person has already
//!    changed on screen, and it does so at exactly the moment they are most likely to be
//!    trusting it;
//! 2. a **node key nobody sent is refused and named**, not answered with an empty list — an
//!    empty list reads as "nothing completes here", which is the same confident wrong answer
//!    the preview half already refuses to give;
//! 3. it is a **read**. A caller who may read a workflow and not manage it gets completions,
//!    because the only row the route reads is the one the reader could already read.
//!
//! The suite runs against the development stack and skips itself with a printed reason when
//! PostgreSQL is not reachable. It takes the same walk lock as `workflow_expressions.rs` —
//! both drive the same `workflows` rows, and two of them at once is two fixture sets racing
//! over the same organization sweep.

use std::sync::OnceLock;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, CsrfSecret};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use omnion_storage::Storage;
use serde_json::{Value, json};
use tokio::sync::{Mutex, MutexGuard};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

/// Set on the test state rather than read from the environment: the CSRF guard **refuses** when
/// no secret is configured, so a suite that leaves it to the shell fails on its first `POST` for
/// a reason that has nothing to do with completions.
const CSRF_SECRET: &str = "w10-complete-suite-csrf-key-material";

/// The refusal the route uses for a node key the caller's graph does not carry. Asserted by
/// value so a rename cannot quietly turn a named refusal into an empty list.
const NODE_MISSING_CODE: &str = "node_not_found";

/// The refusal `routes::workflow_graph` also uses for an unknown workflow, so the completion
/// route is exactly as reachable as the graph it belongs to.
const NOT_FOUND_CODE: &str = "workflow_not_found";

fn walk_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Only the read power, and deliberately short: if the completion route were ever given
/// `workflows.manage`, this list would stop being a meaningful test rather than the route
/// quietly getting stronger.
const WORKFLOW_PERMISSIONS: [&str; 1] = ["workflows.read"];

struct TestResponse {
    status: StatusCode,
    set_cookies: Vec<String>,
    body: Value,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
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
        .to_bytes()
        .to_vec();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };

    TestResponse {
        status,
        set_cookies,
        body,
    }
}

#[derive(Clone, Debug)]
struct Caller {
    session: String,
    csrf: String,
}

impl Caller {
    fn cookie(&self) -> String {
        format!("omnion_session={}; omnion_csrf={}", self.session, self.csrf)
    }
}

fn request(
    method: Method,
    uri: &str,
    caller: Option<&Caller>,
    body: Option<Value>,
) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match caller {
        Some(caller) => builder.header(header::COOKIE, caller.cookie()),
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

async fn live_state() -> Option<(AppState, Db)> {
    let mut config = Config::from_env().expect("environment must be valid");
    config.csrf = CsrfSecret::new(Some(CSRF_SECRET.to_owned()));

    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(err) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({err})");
            return None;
        }
    };
    db.migrate().await.expect("migrations must apply");

    let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
    let storage = Storage::from_env().expect("the storage configuration must be valid");
    let state = AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config,
        db.clone(),
        redis,
        storage,
    );
    Some((state, db))
}

async fn sweep_leftovers(db: &Db) {
    static SWEPT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    SWEPT
        .get_or_init(|| async {
            sqlx::query("delete from organizations where slug like 'w10-complete-%'")
                .execute(db.pool())
                .await
                .expect("leftover organizations must be removed");
            sqlx::query("delete from users where email like 'w10complete-%@omnion.test'")
                .execute(db.pool())
                .await
                .expect("leftover accounts must be removed");
        })
        .await;
}

async fn create_organization_row(db: &Db, label: &str, name: &str) -> Uuid {
    let slug = format!("w10-complete-{label}-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(name)
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("w10complete-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Complete Test".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("creating a test account must succeed");
    (user.id, email)
}

/// Sign in and return the session cookie **and** the CSRF token, which travel together.
async fn login(state: &AppState, email: &str) -> Caller {
    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/auth/login",
            None,
            Some(json!({ "email": email, "password": PASSWORD })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "login body: {}",
        response.body
    );

    let value_of = |name: &str| {
        response
            .set_cookies
            .iter()
            .filter_map(|header| header.split(';').next())
            .filter_map(|pair| pair.trim().split_once('='))
            .find(|(cookie, _)| *cookie == name)
            .map(|(_, value)| value.to_owned())
            .unwrap_or_else(|| panic!("login must set `{name}`: {:?}", response.set_cookies))
    };
    Caller {
        session: value_of("omnion_session"),
        csrf: value_of("omnion_csrf"),
    }
}

/// The pinned sample the completion is asked about — the same contract the preview holds to.
fn sample() -> Value {
    json!({
        "event": { "title": "Published", "author": { "name": "ada" } },
        "node":  { "count": 2 }
    })
}

/// A three-node graph: `trigger_1 → fetch_1 → notify_1`.
fn graph() -> Value {
    json!({
        "nodes": [
            { "key": "trigger_1", "type": "manual_trigger", "label": "When clicked",
              "position": { "x": 0.0, "y": 0.0 } },
            { "key": "fetch_1", "type": "http_request", "label": "Fetch",
              "position": { "x": 260.0, "y": 0.0 } },
            { "key": "notify_1", "type": "http_request", "label": "Notify",
              "position": { "x": 520.0, "y": 0.0 } }
        ],
        "connections": [
            { "from": "trigger_1", "from_port": "main", "to": "fetch_1", "to_port": "in" },
            { "from": "fetch_1", "from_port": "main", "to": "notify_1", "to_port": "in" }
        ],
        "notes": []
    })
}

/// The candidate labels, in the order the route returned them.
fn labels(body: &Value) -> Vec<String> {
    body["candidates"]
        .as_array()
        .expect("candidates must be a list")
        .iter()
        .filter_map(|candidate| candidate["label"].as_str())
        .map(str::to_owned)
        .collect()
}

struct Fixture {
    _walk: MutexGuard<'static, ()>,
    state: AppState,
    db: Db,
    owner_email: String,
    reader_email: String,
    accounts: Vec<Uuid>,
    organizations: Vec<Uuid>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = walk_lock().lock().await;
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");
        sweep_leftovers(&db).await;

        let organization = create_organization_row(&db, "a", "Complete Test A").await;

        let (owner_id, owner_email) = create_account(&db, Some(organization)).await;
        seed::bind_owner(db.pool(), owner_id)
            .await
            .expect("the owner binding must be created");

        // A reader with `workflows.read` and nothing else.
        let (reader_id, reader_email) = create_account(&db, Some(organization)).await;
        let role = role_store::create_role(
            db.pool(),
            NewRole {
                organization_id: organization,
                key: format!("w10-complete-reader-{}", Uuid::new_v4().simple()),
                name: "Complete Reader".to_owned(),
                description: "May read a workflow and complete its expressions".to_owned(),
                priority: 300,
                inherits_role_id: None,
            },
        )
        .await
        .expect("the organization role must be created");
        let entries: Vec<RolePermissionInput> = WORKFLOW_PERMISSIONS
            .iter()
            .map(|key| RolePermissionInput {
                key: (*key).to_owned(),
                effect: Effect::Allow,
            })
            .collect();
        role_store::set_role_permissions(db.pool(), role.id, &entries)
            .await
            .expect("the role permission set must be written");
        bindings::grant(
            db.pool(),
            NewBinding {
                role_id: role.id,
                user_id: reader_id,
                scope: Scope::Organization {
                    organization_id: organization,
                },
                granted_by: Some(owner_id),
                expires_at: None,
            },
        )
        .await
        .expect("the binding must be granted");

        Some(Self {
            _walk: walk,
            state,
            db,
            owner_email,
            reader_email,
            accounts: vec![owner_id, reader_id],
            organizations: vec![organization],
        })
    }

    async fn owner(&self) -> Caller {
        login(&self.state, &self.owner_email).await
    }

    async fn reader(&self) -> Caller {
        login(&self.state, &self.reader_email).await
    }

    /// A workflow whose **stored** graph is empty, so every completion below can only have
    /// come from the body the caller sent. That is the whole point of member (1).
    async fn create_empty_workflow(&self, caller: &Caller) -> String {
        let response = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/workflows",
                Some(caller),
                Some(json!({
                    "name": format!("Complete fixture {}", Uuid::new_v4().simple()),
                    "description": "expression completion fixture",
                    "enabled": true,
                    "trigger": { "kind": "manual" },
                    "steps": [
                        { "name": "start", "kind": "task", "action": "noop", "params": {} }
                    ]
                })),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::CREATED,
            "create body: {}",
            response.body
        );
        response.body["id"]
            .as_str()
            .expect("the body must carry an id")
            .to_owned()
    }

    async fn complete(
        &self,
        caller: &Caller,
        workflow: &str,
        node_key: &str,
        prefix: &str,
        namespaces: Option<Value>,
    ) -> TestResponse {
        let mut body = json!({ "node_key": node_key, "prefix": prefix, "graph": graph() });
        if let Some(namespaces) = namespaces {
            body["namespaces"] = namespaces;
        }
        call(
            &self.state,
            request(
                Method::POST,
                &format!("/api/v1/workflows/{workflow}/graph/expressions/complete"),
                Some(caller),
                Some(body),
            ),
        )
        .await
    }

    /// The same call with an arbitrary body, for the refusal paths.
    async fn complete_raw(&self, caller: &Caller, workflow: &str, body: Value) -> TestResponse {
        call(
            &self.state,
            request(
                Method::POST,
                &format!("/api/v1/workflows/{workflow}/graph/expressions/complete"),
                Some(caller),
                Some(body),
            ),
        )
        .await
    }

    async fn cleanup(&self) {
        sqlx::query("delete from users where id = any($1)")
            .bind(&self.accounts)
            .execute(self.db.pool())
            .await
            .expect("account cleanup must run");
        sqlx::query("delete from organizations where id = any($1)")
            .bind(&self.organizations)
            .execute(self.db.pool())
            .await
            .expect("organization cleanup must run");
    }
}

#[tokio::test]
async fn the_candidates_follow_the_graph_the_caller_sent_not_the_stored_one() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    let owner = fixture.owner().await;
    // The stored graph of this workflow is empty, so anything upstream in the answer came from
    // the request body — which is what the canvas actually holds.
    let workflow = fixture.create_empty_workflow(&owner).await;

    let response = fixture
        .complete(&owner, &workflow, "notify_1", "", Some(sample()))
        .await;

    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);
    assert_eq!(response.body["node_key"], json!("notify_1"));
    assert_eq!(
        response.body["upstream_nodes"],
        json!(["fetch_1"]),
        "only the direct parent is in scope: {}",
        response.body
    );

    let found = labels(&response.body);
    for expected in ["$json", "$node", "$item", "$vars", "fetch_1", "event.title"] {
        assert!(
            found.iter().any(|label| label == expected),
            "`{expected}` must be offered: {found:?}"
        );
    }
    // The trigger is two hops away. Offering it would suggest an expression the step cannot
    // read today, and would stop resolving the moment the middle wire was deleted.
    assert!(
        !found.iter().any(|label| label == "trigger_1"),
        "a grandparent must not be offered: {found:?}"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_sources_are_named_so_a_person_can_choose_between_two_candidates() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    let owner = fixture.owner().await;
    let workflow = fixture.create_empty_workflow(&owner).await;

    let response = fixture
        .complete(&owner, &workflow, "notify_1", "", Some(sample()))
        .await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);

    let source_of = |label: &str| {
        response.body["candidates"]
            .as_array()
            .expect("candidates must be a list")
            .iter()
            .find(|candidate| candidate["label"] == json!(label))
            .map(|candidate| candidate["source"].clone())
            .unwrap_or_else(|| panic!("no candidate for `{label}`: {}", response.body))
    };

    assert_eq!(source_of("$item"), json!("runtime"));
    assert_eq!(source_of("event.title"), json!("sample"));
    assert_eq!(source_of("fetch_1"), json!("upstream"));

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_prefix_narrows_the_list_and_an_impossible_one_is_an_empty_success() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    let owner = fixture.owner().await;
    let workflow = fixture.create_empty_workflow(&owner).await;

    let narrowed = fixture
        .complete(&owner, &workflow, "notify_1", "$va", Some(sample()))
        .await;
    assert_eq!(narrowed.status, StatusCode::OK, "body: {}", narrowed.body);
    assert_eq!(labels(&narrowed.body), vec!["$vars".to_owned()]);

    // Three characters into a namespace the person had not chosen yet. Refusing this would be
    // a worse product than offering everything; a list that empties mid-typing is the thing
    // people report as "autocomplete does not work".
    let partial = fixture
        .complete(&owner, &workflow, "notify_1", "$it", Some(sample()))
        .await;
    assert_eq!(labels(&partial.body), vec!["$item".to_owned()]);

    // Nothing matches: a success with an empty list, not an error. It is the most common
    // answer while somebody types a namespace name.
    let none = fixture
        .complete(&owner, &workflow, "notify_1", "zzzz", Some(sample()))
        .await;
    assert_eq!(none.status, StatusCode::OK, "body: {}", none.body);
    assert_eq!(none.body["candidate_count"], json!(0));
    assert!(none.body["candidates"].as_array().is_some_and(Vec::is_empty));

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_node_key_the_graph_does_not_carry_is_refused_and_named() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    let owner = fixture.owner().await;
    let workflow = fixture.create_empty_workflow(&owner).await;

    let response = fixture
        .complete(&owner, &workflow, "no_such_node", "", Some(sample()))
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND, "body: {}", response.body);
    assert_eq!(response.body["error"]["code"], json!(NODE_MISSING_CODE));
    // The message names the node, so the editor can say which row asked.
    let message = response.body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("no_such_node"),
        "the refusal must name the node: {message:?}"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_completion_is_a_read_a_reader_may_make_and_a_stranger_may_not() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    let owner = fixture.owner().await;
    let workflow = fixture.create_empty_workflow(&owner).await;

    // A caller who may read the workflow and not manage it. The graph travels in the body, so
    // the only row the route reads is one this caller could already read.
    let reader = fixture.complete(
        &fixture.reader().await,
        &workflow,
        "notify_1",
        "",
        Some(sample()),
    )
    .await;
    assert_eq!(reader.status, StatusCode::OK, "body: {}", reader.body);
    assert!(!reader.body["candidates"].as_array().is_some_and(Vec::is_empty));

    // Signed out.
    let anonymous = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflows/{workflow}/graph/expressions/complete"),
            None,
            Some(json!({ "node_key": "notify_1", "prefix": "", "graph": graph() })),
        ),
    )
    .await;
    assert!(
        anonymous.status == StatusCode::UNAUTHORIZED || anonymous.status == StatusCode::FORBIDDEN,
        "signed out: {}",
        anonymous.body
    );

    // A workflow that does not exist, answered by a reader who could otherwise complete.
    let missing = fixture
        .complete(
            &fixture.reader().await,
            &Uuid::new_v4().to_string(),
            "notify_1",
            "",
            Some(sample()),
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "body: {}", missing.body);
    assert_eq!(missing.body["error"]["code"], json!(NOT_FOUND_CODE));

    fixture.cleanup().await;
}

#[tokio::test]
async fn an_unknown_field_is_refused_rather_than_answered_from_defaults() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    let owner = fixture.owner().await;
    let workflow = fixture.create_empty_workflow(&owner).await;

    // A typo in the key would otherwise be answered with a list built from defaults: four
    // runtime namespaces, no upstream — which reads as "this node has no parents".
    let response = fixture
        .complete_raw(
            &owner,
            &workflow,
            json!({ "node_key": "notify_1", "prefx": "", "graph": graph() }),
        )
        .await;
    assert!(
        response.status == StatusCode::UNPROCESSABLE_ENTITY
            || response.status == StatusCode::BAD_REQUEST,
        "a misspelled key must be refused: {} {}",
        response.status,
        response.body
    );

    // A missing graph is the same class of caller bug and gets the same answer, rather than a
    // completion list for an empty graph.
    let no_graph = fixture
        .complete_raw(
            &owner,
            &workflow,
            json!({ "node_key": "notify_1", "prefix": "" }),
        )
        .await;
    assert!(
        no_graph.status == StatusCode::UNPROCESSABLE_ENTITY
            || no_graph.status == StatusCode::BAD_REQUEST,
        "a missing graph must be refused: {} {}",
        no_graph.status,
        no_graph.body
    );

    fixture.cleanup().await;
}
