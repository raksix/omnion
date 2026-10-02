//! Integration tests for the expression preview (docs/requests/REQ-086, slice 3).
//!
//! `crates/workflows/src/expression.rs` proves what the evaluator decides. This file proves the
//! three things only a real round trip can, and each one is a claim the canvas depends on:
//!
//! 1. the route answers with **the caller's own sample data** — a preview that could reach live
//!    data would be able to read a row the person editing cannot see;
//! 2. a refusal is a `422` naming the field it belongs to, so the inspector can put the message
//!    on the row that caused it rather than in a banner at the top of the canvas;
//! 3. it is a **read**. A caller who may read a workflow and not manage it gets previews, and
//!    the same is true of a reader who passes no namespaces at all: nothing is stored, so
//!    nothing needs the power to store it.
//!
//! The suite runs against the development stack and skips itself with a printed reason when
//! PostgreSQL is not reachable. It takes the same walk lock as `workflow_graph.rs`, which owns
//! the shared helpers' subject matter — both drive the same `workflows` rows the engine sweeps.

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
/// a reason that has nothing to do with the preview.
const CSRF_SECRET: &str = "w10-preview-suite-csrf-key-material";

/// The `expression_invalid` code the canvas branches on. Asserted by value so a rename cannot
/// quietly turn a preview refusal into an unhandled response.
const INVALID_CODE: &str = "expression_invalid";

/// The refusal `routes::workflow_graph` also uses for an unknown workflow, so the preview is
/// exactly as reachable as the definition it belongs to.
const NOT_FOUND_CODE: &str = "workflow_not_found";

fn walk_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Only the read power. The point of member (3) above is that this list is deliberately short:
/// if the preview route were ever given `workflows.manage`, member would stop being a
/// meaningful test rather than the route quietly getting stronger.
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
            sqlx::query("delete from organizations where slug like 'w10-preview-%'")
                .execute(db.pool())
                .await
                .expect("leftover organizations must be removed");
            sqlx::query("delete from users where email like 'w10preview-%@omnion.test'")
                .execute(db.pool())
                .await
                .expect("leftover accounts must be removed");
        })
        .await;
}

async fn create_organization_row(db: &Db, label: &str, name: &str) -> Uuid {
    let slug = format!("w10-preview-{label}-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(name)
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("w10preview-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Preview Test".to_owned(),
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

/// The sample a preview is asked to evaluate against. Shape taken from the REQ: pinned data,
/// nothing the server fetched.
fn sample() -> Value {
    json!({
        "node": {
            "items": [ { "title": "First" }, { "title": "Second" } ],
            "count": 2,
            "author": { "name": "ada", "email": "ada@example.com" },
            "summary": null
        },
        "vars": { "site": "example.com" }
    })
}

/// The preview of one field, found by name.
fn preview_named<'a>(body: &'a Value, field: &str) -> &'a Value {
    body["previews"]
        .as_array()
        .expect("previews must be a list")
        .iter()
        .find(|preview| preview["field"] == json!(field))
        .unwrap_or_else(|| panic!("no preview for `{field}`: {body}"))
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

        let organization = create_organization_row(&db, "a", "Preview Test A").await;

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
                key: format!("w10-preview-reader-{}", Uuid::new_v4().simple()),
                name: "Preview Reader".to_owned(),
                description: "May read a workflow and preview its expressions".to_owned(),
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

    /// A workflow with one node, so the preview has a graph to be scoped to.
    async fn create_workflow(&self, caller: &Caller) -> String {
        let response = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/workflows",
                Some(caller),
                Some(json!({
                    "name": format!("Preview fixture {}", Uuid::new_v4().simple()),
                    "description": "expression preview fixture",
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

    async fn preview(
        &self,
        caller: &Caller,
        workflow: &str,
        params: Value,
        namespaces: Option<Value>,
    ) -> TestResponse {
        let mut body = json!({ "params": params });
        if let Some(namespaces) = namespaces {
            body["namespaces"] = namespaces;
        }
        call(
            &self.state,
            request(
                Method::POST,
                &format!("/api/v1/workflows/{workflow}/graph/expressions/preview"),
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
async fn the_preview_evaluates_against_the_samples_the_caller_sent() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    let owner = fixture.owner().await;
    let workflow = fixture.create_workflow(&owner).await;

    let response = fixture
        .preview(
            &owner,
            &workflow,
            json!({
                "to": "{{node.author.email}}",
                "subject": "By {{node.author.name}}: {{node.count}} orders",
                "retries": 3
            }),
            Some(sample()),
        )
        .await;

    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);
    assert_eq!(response.body["workflow_id"], json!(workflow));
    // Two of the three parameters carry an expression; the constant is not a preview.
    assert_eq!(
        response.body["preview_count"],
        json!(2),
        "{}",
        response.body
    );

    // Indexed by NAME, never by position. `serde_json::Map` preserves insertion order but
    // only by default feature, so a positional assertion is a claim about a cargo feature
    // rather than about this route — and it fails as "subject where to was expected",
    // which reads like the server returned the wrong field's answer.
    let to = preview_named(&response.body, "to");
    assert_eq!(
        to["value"],
        json!("ada@example.com"),
        "the preview resolves to the caller's own sample, not to anything the server read"
    );
    assert_eq!(to["typed"], json!(true), "a lone expression keeps its type");

    let subject = preview_named(&response.body, "subject");
    assert_eq!(
        subject["rendered"],
        json!("By ada: 2 orders"),
        "{}",
        subject
    );

    let namespaces: Vec<&str> = response.body["namespaces"]
        .as_array()
        .expect("namespaces must be a list")
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    assert_eq!(
        namespaces,
        ["node", "vars"],
        "the canvas autocompletes from this"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_reader_who_cannot_manage_still_gets_a_preview() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    let owner = fixture.owner().await;
    let workflow = fixture.create_workflow(&owner).await;
    let reader = fixture.reader().await;

    let response = fixture
        .preview(
            &reader,
            &workflow,
            json!({ "count": "{{node.count}}" }),
            Some(sample()),
        )
        .await;

    assert_eq!(
        response.status,
        StatusCode::OK,
        "a preview stores nothing, so it needs no power to store: {}",
        response.body
    );
    assert_eq!(response.body["previews"][0]["value"], json!(2));

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_refusal_is_a_422_naming_the_field_it_belongs_to() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    let owner = fixture.owner().await;
    let workflow = fixture.create_workflow(&owner).await;

    let response = fixture
        .preview(
            &owner,
            &workflow,
            json!({ "to": "{{node.author.emaill}}" }),
            Some(sample()),
        )
        .await;

    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "body: {}",
        response.body
    );
    assert_eq!(response.body["error"]["code"], json!(INVALID_CODE));
    let message = response.body["error"]["message"]
        .as_str()
        .expect("a refusal must carry a message");
    assert!(message.contains("to:"), "the field is named: {message}");
    assert!(
        message.contains("author.name"),
        "the message lists what the sample carries: {message}"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_preview_without_sample_data_refuses_rather_than_guessing() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    let owner = fixture.owner().await;
    let workflow = fixture.create_workflow(&owner).await;

    // No `namespaces` at all. The route must not fall back to anything: an invented sample is
    // an answer to a question nobody asked, and it would look identical to a real one.
    let response = fixture
        .preview(
            &owner,
            &workflow,
            json!({ "to": "{{node.author.email}}" }),
            None,
        )
        .await;

    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "body: {}",
        response.body
    );
    assert_eq!(response.body["error"]["code"], json!(INVALID_CODE));

    // And a field with no expression in it is not a refusal and not a preview.
    let constant = fixture
        .preview(&owner, &workflow, json!({ "subject": "just text" }), None)
        .await;
    assert_eq!(constant.status, StatusCode::OK, "body: {}", constant.body);
    assert_eq!(constant.body["preview_count"], json!(0));

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_preview_is_scoped_and_guarded_like_the_graph_it_belongs_to() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    let owner = fixture.owner().await;
    let workflow = fixture.create_workflow(&owner).await;

    // Signed out.
    let anonymous = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflows/{workflow}/graph/expressions/preview"),
            None,
            Some(json!({ "params": {}, "namespaces": sample() })),
        ),
    )
    .await;
    assert!(
        anonymous.status == StatusCode::UNAUTHORIZED || anonymous.status == StatusCode::FORBIDDEN,
        "signed out: {}",
        anonymous.body
    );

    // A workflow that does not exist, answered by a reader who could otherwise preview.
    let missing = fixture
        .preview(
            &fixture.reader().await,
            &Uuid::new_v4().to_string(),
            json!({ "to": "{{node.count}}" }),
            Some(sample()),
        )
        .await;
    assert_eq!(
        missing.status,
        StatusCode::NOT_FOUND,
        "body: {}",
        missing.body
    );
    assert_eq!(missing.body["error"]["code"], json!(NOT_FOUND_CODE));

    fixture.cleanup().await;
}
