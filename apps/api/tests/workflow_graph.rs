//! Integration tests for the visual graph (docs/requests/REQ-086, slice 1).
//!
//! The unit tests in `crates/workflows/tests/graph.rs` prove what the validator and the
//! compiler decide. This file proves the three things only a real round trip can:
//!
//! 1. a save writes the graph **and** the compiled steps in one request, so a run started right
//!    afterwards executes the definition the canvas is showing;
//! 2. a stale revision is refused with a conflict carrying the *current* revision, and the
//!    refused save left the stored document untouched — the second half is the one a unit test
//!    cannot make, and it is the half that matters when two people are editing;
//! 3. tenancy, permissions and the 404s behave the way `routes::workflows` already does, so a
//!    graph is exactly as reachable as the definition it belongs to.
//!
//! Like the workflow suite it runs against the development stack and skips itself with a
//! printed reason when PostgreSQL is not reachable. It takes the same walk lock, because it
//! starts real runs of the same `workflows` rows the engine sweeps table-wide.

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
use omnion_workflows::engine::{self, RunnerConfig};
use serde_json::{Value, json};
use time::Duration;
use tokio::sync::{Mutex, MutexGuard};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// Key material for this suite's CSRF secret. It is set on the test state rather than read from
/// the environment, so the suite proves a real signed-in caller and not one that happened to
/// inherit a configured deployment.
const CSRF_SECRET: &str = "w10-graph-suite-csrf-key-material";

/// The lock that keeps one walk in flight (see `apps/api/tests/workflows.rs` for why).
fn walk_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Permission keys the workflow operator of this suite holds.
const WORKFLOW_PERMISSIONS: [&str; 3] = ["workflows.read", "workflows.manage", "workflows.run"];

/// The `graph_revision_conflict` code the editor branches on. Asserted by value so a rename
/// cannot make the canvas's conflict path silently unreachable.
const CONFLICT_CODE: &str = "graph_revision_conflict";

/// The `workflow_not_found` code `routes::workflows` also answers with. Asserted here because
/// `workflow_graph.rs` builds its own 404 rather than widening that module's visibility: this is
/// the line that keeps the two copies from drifting.
const NOT_FOUND_CODE: &str = "workflow_not_found";

struct TestResponse {
    status: StatusCode,
    /// Every `Set-Cookie` header, not just the first one. A sign-in sets **two** cookies — the
    /// session and the CSRF token — and `HeaderMap::get` hands back whichever arrived first.
    /// A helper that reads one of them is a helper that reads a coin toss.
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

/// A signed-in caller: the session cookie **and** the CSRF token, because the two travel
/// together and a half-signed-in caller is a caller whose every mutation is refused.
///
/// The two are derived from one sign-in response. Taking only the session — which is what the
/// older workflow suite does — is enough for a `GET` and produces a `403 csrf_unavailable` (or,
/// with a secret configured, `csrf_failed`) on the first `POST`, so the second half belongs in
/// the same value rather than in a second thing every call site has to remember to pass.
#[derive(Clone, Debug)]
struct Caller {
    session: String,
    csrf: String,
}

impl Caller {
    /// The `Cookie` header this caller presents on every request.
    fn cookie(&self) -> String {
        format!("omnion_session={}; omnion_csrf={}", self.session, self.csrf)
    }
}

fn request(method: Method, uri: &str, caller: Option<&Caller>, body: Option<Value>) -> Request<Body> {
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

/// A save carrying an `If-Match`, for the header path.
fn request_with_if_match(
    method: Method,
    uri: &str,
    caller: &Caller,
    body: Value,
    if_match: &str,
) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, caller.cookie())
        .header(header::IF_MATCH, if_match)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request must build")
}

async fn live_state() -> Option<(AppState, Db)> {
    // The CSRF secret is set *in the state* rather than read from the environment, and that is the
    // whole point: REQ-012's guard **refuses** rather than skips when no secret is configured, so
    // a suite that leaves it to the shell fails for a reason that has nothing to do with the
    // graph. `apps/api/tests/csrf.rs` fixed it the same way. A mutation this suite makes is then
    // refused with `csrf_failed` — not with `csrf_unavailable` — which is what the assertions
    // below are able to read.
    let mut config = Config::from_env().expect("environment must be valid");
    config.csrf = CsrfSecret::new(Some(CSRF_SECRET.to_owned()));

    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(err) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({err}) — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
            );
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

fn test_runner() -> RunnerConfig {
    RunnerConfig {
        tick: Duration::milliseconds(10),
        sweep: Duration::milliseconds(10),
        batch: 16,
        retry_base: Duration::milliseconds(30),
        retry_max: Duration::milliseconds(90),
        lease: Duration::seconds(30),
        ..RunnerConfig::default()
    }
}

async fn sweep_leftovers(db: &Db) {
    static SWEPT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    SWEPT
        .get_or_init(|| async {
            sqlx::query("delete from organizations where slug like 'w10-graph-%'")
                .execute(db.pool())
                .await
                .expect("leftover organizations must be removed");
            sqlx::query("delete from users where email like 'w10graph-%@omnion.test'")
                .execute(db.pool())
                .await
                .expect("leftover accounts must be removed");
        })
        .await;
}

async fn create_organization_row(db: &Db, label: &str, name: &str) -> Uuid {
    let slug = format!("w10-graph-{label}-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(name)
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("w10graph-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Graph Test".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("creating a test account must succeed");
    (user.id, email)
}

/// Sign an account in and return the caller: the session cookie plus the CSRF token the same
/// response handed out.
///
/// Both cookies are read from the *whole* `Set-Cookie` header rather than from the first
/// `name=value` pair, because the order two cookies arrive in is the router's business and not
/// something a test should depend on.
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
    assert_eq!(response.status, StatusCode::OK, "login body: {}", response.body);

    assert!(
        !response.set_cookies.is_empty(),
        "login must set the session cookie: {}",
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

fn id_of(body: &Value) -> String {
    body["id"]
        .as_str()
        .expect("the body must carry an id")
        .to_owned()
}

/// A graph that validates: a manual trigger and one `send_email`, wired together.
fn clean_graph() -> Value {
    json!({
        "nodes": [
            { "key": "start", "type": "manual_trigger", "label": "Start",
              "position": { "x": 0.0, "y": 0.0 } },
            { "key": "notify", "type": "send_email", "label": "Notify ops",
              "position": { "x": 200.0, "y": 0.0 },
              "params": {
                "to": "ops@example.com",
                "subject": "Orders",
                "body": "there are some",
                "credential_key": "smtp_prod"
              } }
        ],
        "connections": [
            { "from": "start", "from_port": "out", "to": "notify", "to_port": "in" }
        ]
    })
}

struct Fixture {
    _walk: MutexGuard<'static, ()>,
    state: AppState,
    db: Db,
    platform_email: String,
    operator_email: String,
    member_email: String,
    accounts: Vec<Uuid>,
    organizations: Vec<Uuid>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = walk_lock().lock().await;
        let (state, db) = live_state().await?;
        seed::ensure(db.pool()).await.expect("the IAM seed must run");
        sweep_leftovers(&db).await;

        let organization_a = create_organization_row(&db, "a", "Graph Test A").await;

        let (platform_id, platform_email) = create_account(&db, None).await;
        seed::bind_owner(db.pool(), platform_id)
            .await
            .expect("the owner binding must be created");

        let (operator_id, operator_email) = create_account(&db, Some(organization_a)).await;
        let role = role_store::create_role(
            db.pool(),
            NewRole {
                organization_id: organization_a,
                key: format!("w10-graph-operator-{}", Uuid::new_v4().simple()),
                name: "Graph Operator".to_owned(),
                description: "Edits the automations of one organization".to_owned(),
                priority: 400,
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

        let binding = NewBinding {
            role_id: role.id,
            user_id: operator_id,
            scope: Scope::Organization {
                organization_id: organization_a,
            },
            granted_by: Some(platform_id),
            expires_at: None,
        };
        bindings::validate(db.pool(), &binding)
            .await
            .expect("the binding must validate");
        bindings::grant(db.pool(), binding)
            .await
            .expect("the binding must be granted");

        let (member_id, member_email) = create_account(&db, Some(organization_a)).await;

        Some(Self {
            _walk: walk,
            state,
            db,
            platform_email,
            operator_email,
            member_email,
            accounts: vec![platform_id, operator_id, member_id],
            organizations: vec![organization_a],
        })
    }

    async fn platform(&self) -> Caller {
        login(&self.state, &self.platform_email).await
    }

    async fn operator(&self) -> Caller {
        login(&self.state, &self.operator_email).await
    }

    async fn member(&self) -> Caller {
        login(&self.state, &self.member_email).await
    }

    /// A workflow with a manual trigger and one `noop` step, so a run has something to do.
    async fn create_workflow(&self, caller: &Caller, name: &str) -> String {
        let response = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/workflows",
                Some(caller),
                Some(json!({
                    "name": name,
                    "description": "graph fixture",
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
        id_of(&response.body)
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

/// The stored `steps` of a workflow, read straight from the column.
async fn stored_steps(db: &Db, workflow_id: &str) -> Value {
    sqlx::query_scalar("select steps from workflows where id = $1::uuid")
        .bind(workflow_id)
        .fetch_one(db.pool())
        .await
        .expect("the workflow must read")
}

#[tokio::test]
async fn a_saved_graph_writes_the_compiled_steps_and_the_run_uses_them() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let caller = fixture.operator().await;
    let workflow = fixture.create_workflow(&caller, "Graph run end to end").await;

    let save = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/workflows/{workflow}/graph"),
            Some(&caller),
            Some(json!({ "graph": clean_graph(), "revision": 0 })),
        ),
    )
    .await;
    assert_eq!(save.status, StatusCode::OK, "save body: {}", save.body);
    assert_eq!(save.body["revision"], 1, "the first save is revision 1");
    assert_eq!(save.body["step_count"], 2, "trigger + one mail");
    assert_eq!(save.body["node_order"], json!(["start", "notify"]));

    // The half that matters: the same request wrote the steps the engine will run. Reading the
    // column directly is the proof that the two representations cannot drift — a save that wrote
    // only the graph would leave the old one-step definition here and the canvas would be lying.
    let steps = stored_steps(&fixture.db, &workflow).await;
    let names: Vec<&str> = steps
        .as_array()
        .expect("steps are an array")
        .iter()
        .map(|step| step["name"].as_str().expect("a step name"))
        .collect();
    assert_eq!(names, vec!["Start", "Notify ops"], "compiled step names");
    assert_eq!(steps[1]["action"], "send_email");

    // And a real run of it materialises those two steps, in that order.
    let run = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflows/{workflow}/run"),
            Some(&caller),
            None,
        ),
    )
    .await;
    assert_eq!(run.status, StatusCode::ACCEPTED, "run body: {}", run.body);
    let execution = id_of(&run.body);

    let report = engine::tick(fixture.state.db().pool(), &test_runner())
        .await
        .expect("the runner must tick");
    assert!(
        report.steps_run >= 1,
        "the run must have advanced its steps: {report:?}"
    );

    let stored: Vec<String> = sqlx::query_scalar(
        "select name from workflow_steps where execution_id = $1::uuid order by step_no",
    )
    .bind(&execution)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the steps must read");
    assert_eq!(
        stored,
        vec!["Start".to_owned(), "Notify ops".to_owned()],
        "the run materialised the compiled steps"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_stale_revision_is_a_conflict_and_changes_nothing() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let caller = fixture.operator().await;
    let workflow = fixture.create_workflow(&caller, "Graph conflict").await;

    // Two editors, both at revision 0.
    let first = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/workflows/{workflow}/graph"),
            Some(&caller),
            Some(json!({ "graph": clean_graph(), "revision": 0 })),
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "first save: {}", first.body);
    assert_eq!(first.body["revision"], 1);

    // The second editor still believes it holds revision 0. This is the whole point of the
    // revision column: its save is refused, and refused *with the current revision* so the
    // editor can offer compare-and-reload instead of a blind overwrite.
    let mut second_graph = clean_graph();
    second_graph["nodes"][1]["params"]["subject"] = json!("Overwritten");
    let second = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/workflows/{workflow}/graph"),
            Some(&caller),
            Some(json!({ "graph": second_graph, "revision": 0 })),
        ),
    )
    .await;
    assert_eq!(second.status, StatusCode::CONFLICT, "body: {}", second.body);
    assert_eq!(second.body["error"]["code"], CONFLICT_CODE);
    assert_eq!(second.body["error"]["details"]["current_revision"], 1);
    assert_eq!(second.body["error"]["details"]["loaded_revision"], 0);

    // The refused save must have changed nothing. Reading the column is the only way to know:
    // a handler that reported a conflict *after* writing would satisfy every assertion above.
    let steps = stored_steps(&fixture.db, &workflow).await;
    assert_eq!(steps[1]["params"]["subject"], "Orders", "the first save stands");

    // And the revision did not move, so the winner's own next save still works.
    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{workflow}/graph"),
            Some(&caller),
            None,
        ),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(read.body["revision"], 1);
    assert_eq!(read.body["node_count"], 2);

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_graph_with_issues_is_refused_with_every_issue_named() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let caller = fixture.operator().await;
    let workflow = fixture.create_workflow(&caller, "Graph invalid").await;

    // Three separate problems in one document: a node that names a type nothing provides, a
    // required parameter left out, and a node nothing connects to.
    let broken = json!({
        "nodes": [
            { "key": "start", "type": "manual_trigger", "label": "Start",
              "position": { "x": 0.0, "y": 0.0 } },
            { "key": "notify", "type": "send_email", "label": "Notify",
              "position": { "x": 200.0, "y": 0.0 },
              "params": { "to": "ops@example.com" } },
            { "key": "ghost", "type": "acme.nothing", "label": "Ghost",
              "position": { "x": 400.0, "y": 0.0 } }
        ],
        "connections": [
            { "from": "start", "from_port": "out", "to": "notify", "to_port": "in" }
        ]
    });

    let response = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/workflows/{workflow}/graph"),
            Some(&caller),
            Some(json!({ "graph": broken, "revision": 0 })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "body: {}",
        response.body
    );

    let issues = response.body["error"]["details"]["issues"]
        .as_array()
        .expect("the refusal must carry the issue list");
    let codes: Vec<&str> = issues
        .iter()
        .map(|issue| issue["code"].as_str().expect("a code"))
        .collect();
    assert!(
        codes.contains(&"node_param_required"),
        "the missing subject must be named: {codes:?}"
    );
    assert!(
        codes.contains(&"node_unknown_type"),
        "the uninstalled node must be named: {codes:?}"
    );
    assert!(
        codes.contains(&"graph_unreachable_node"),
        "the orphan must be named: {codes:?}"
    );
    // Each issue says which node it is about, or the panel cannot jump to it.
    let ghost = issues
        .iter()
        .find(|issue| issue["code"] == "node_unknown_type")
        .expect("the unknown type");
    assert_eq!(ghost["node_key"], "ghost");

    // Nothing was written: a refused save must not leave a half-stored document.
    let steps = stored_steps(&fixture.db, &workflow).await;
    assert_eq!(
        steps.as_array().map(Vec::len),
        Some(1),
        "the original one-step definition is untouched"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn validate_reports_without_writing() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let caller = fixture.operator().await;
    let workflow = fixture.create_workflow(&caller, "Graph validate only").await;

    let mut broken = clean_graph();
    broken["nodes"][1]["params"].as_object_mut().expect("an object").remove("body");

    let response = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflows/{workflow}/graph/validate"),
            Some(&caller),
            Some(json!({ "graph": broken, "revision": 0 })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);
    assert_eq!(response.body["valid"], false);
    assert_eq!(response.body["issue_count"], 1);
    assert_eq!(response.body["issues"][0]["code"], "node_param_required");
    assert!(
        response.body.get("step_count").is_none(),
        "an invalid graph projects no step count"
    );

    // A clean document answers with the count it *would* produce — this is what the canvas
    // shows next to "3 steps" while a person is still drawing.
    let good = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflows/{workflow}/graph/validate"),
            Some(&caller),
            Some(json!({ "graph": clean_graph(), "revision": 0 })),
        ),
    )
    .await;
    assert_eq!(good.status, StatusCode::OK);
    assert_eq!(good.body["valid"], true);
    assert_eq!(good.body["step_count"], 2);

    // Validation stored nothing: the workflow's own steps are still the fixture's one.
    let steps = stored_steps(&fixture.db, &workflow).await;
    assert_eq!(steps.as_array().map(Vec::len), Some(1));

    fixture.cleanup().await;
}

#[tokio::test]
async fn if_match_is_honoured_and_a_disagreement_is_refused() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let caller = fixture.operator().await;
    let workflow = fixture.create_workflow(&caller, "Graph if-match").await;

    // The quoted form HTTP specifies, and it agrees with the body.
    let agreed = call(
        &fixture.state,
        request_with_if_match(
            Method::PUT,
            &format!("/api/v1/workflows/{workflow}/graph"),
            &caller,
            json!({ "graph": clean_graph(), "revision": 0 }),
            "\"0\"",
        ),
    )
    .await;
    assert_eq!(agreed.status, StatusCode::OK, "body: {}", agreed.body);
    assert_eq!(agreed.body["revision"], 1);

    // A disagreement is refused rather than resolved: a caller that does not know its own
    // revision must not be allowed to have one of its two numbers silently win.
    let split = call(
        &fixture.state,
        request_with_if_match(
            Method::PUT,
            &format!("/api/v1/workflows/{workflow}/graph"),
            &caller,
            json!({ "graph": clean_graph(), "revision": 1 }),
            "7",
        ),
    )
    .await;
    assert_eq!(split.status, StatusCode::BAD_REQUEST, "body: {}", split.body);
    assert_eq!(split.body["error"]["code"], "graph_revision_ambiguous");

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_graph_is_reachable_exactly_when_its_workflow_is() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let operator = fixture.operator().await;
    let member = fixture.member().await;
    let platform = fixture.platform().await;
    let workflow = fixture.create_workflow(&operator, "Graph scope").await;

    // A plain member of the same organization holds none of the three workflow keys, so the
    // graph is refused exactly as the definition is.
    let refused = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{workflow}/graph"),
            Some(&member),
            None,
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "body: {}", refused.body);

    // A platform Owner is not refused: it is the account that administers a tenant's workflows.
    let allowed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{workflow}/graph"),
            Some(&platform),
            None,
        ),
    )
    .await;
    assert_eq!(allowed.status, StatusCode::OK, "body: {}", allowed.body);
    assert_eq!(allowed.body["revision"], 0, "a new graph is at revision 0");
    assert_eq!(allowed.body["node_count"], 0, "and empty");
    assert_eq!(allowed.body["graph"]["nodes"], json!([]));

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_graph_of_a_workflow_that_is_gone_is_a_404_with_the_shared_code() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let caller = fixture.operator().await;
    let missing = Uuid::new_v4().to_string();

    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{missing}/graph"),
            Some(&caller),
            None,
        ),
    )
    .await;
    assert_eq!(read.status, StatusCode::NOT_FOUND, "body: {}", read.body);
    assert_eq!(
        read.body["error"]["code"], NOT_FOUND_CODE,
        "the graph routes answer with the same code the workflow routes do"
    );

    // A save against a workflow that was deleted mid-edit is a 404, not a conflict: the row
    // is gone, and "you are out of date" would send the editor looking for a revision that
    // does not exist.
    let save = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/workflows/{missing}/graph"),
            Some(&caller),
            Some(json!({ "graph": clean_graph(), "revision": 0 })),
        ),
    )
    .await;
    assert_eq!(save.status, StatusCode::NOT_FOUND, "body: {}", save.body);
    assert_eq!(save.body["error"]["code"], NOT_FOUND_CODE);

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_sticky_note_survives_a_save_and_never_becomes_a_step() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let caller = fixture.operator().await;
    let workflow = fixture.create_workflow(&caller, "Graph notes").await;

    let mut document = clean_graph();
    document["notes"] = json!([{
        "id": "n1",
        "position": { "x": 40.0, "y": 60.0 },
        "text": "ask about the rate limit"
    }]);

    let save = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/workflows/{workflow}/graph"),
            Some(&caller),
            Some(json!({ "graph": document, "revision": 0 })),
        ),
    )
    .await;
    assert_eq!(save.status, StatusCode::OK, "body: {}", save.body);
    assert_eq!(save.body["step_count"], 2, "a note is not a step");

    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{workflow}/graph"),
            Some(&caller),
            None,
        ),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(read.body["note_count"], 1);
    assert_eq!(
        read.body["graph"]["notes"][0]["text"],
        "ask about the rate limit"
    );
    assert_eq!(
        read.body["graph"]["notes"][0]["color"], "amber",
        "the default colour survives the round trip"
    );

    let steps = stored_steps(&fixture.db, &workflow).await;
    assert_eq!(steps.as_array().map(Vec::len), Some(2));

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_manually_moved_node_keeps_its_position_across_a_reload() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let caller = fixture.operator().await;
    let workflow = fixture.create_workflow(&caller, "Graph positions").await;

    // Save, then move one node and save again at the new revision — the shape a person gets by
    // dragging a node and pressing ⌘S. The engine's own `update_workflow` rewrites `steps`
    // without touching `graph`, so this also proves the two columns are independent.
    let first = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/workflows/{workflow}/graph"),
            Some(&caller),
            Some(json!({ "graph": clean_graph(), "revision": 0 })),
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK);

    let mut moved = clean_graph();
    moved["nodes"][1]["position"] = json!({ "x": 642.5, "y": -113.25 });
    let second = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/workflows/{workflow}/graph"),
            Some(&caller),
            Some(json!({ "graph": moved, "revision": 1 })),
        ),
    )
    .await;
    assert_eq!(second.status, StatusCode::OK, "body: {}", second.body);
    assert_eq!(second.body["revision"], 2);

    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{workflow}/graph"),
            Some(&caller),
            None,
        ),
    )
    .await;
    assert_eq!(read.body["revision"], 2);
    // Floats survive: 642.5 is not an integer a canvas would ever produce, and rounding it
    // would make every dragged node snap to a grid the person did not choose.
    assert_eq!(read.body["graph"]["nodes"][1]["position"]["x"], 642.5);
    assert_eq!(read.body["graph"]["nodes"][1]["position"]["y"], -113.25);

    fixture.cleanup().await;
}
