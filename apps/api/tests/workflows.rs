//! Integration tests for the workflow engine (phase P09, docs/requests/REQ-003).
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) and drive the engine the
//! way the background runner does: `engine::tick` for due steps, `engine::sweep` for the
//! wait/repair pass. What they prove is the three acceptance criteria of P09 — a three-step
//! run whose middle step fails once is retried and completes, a wait step parks the run and is
//! resumed later, and every lifecycle change leaves an audit row — plus the rules around it
//! (permissions, tenancy, cancellation, schedule trigger, definition validation).
//!
//! When PostgreSQL is not reachable the suite skips itself with a printed reason, so
//! `cargo test` stays usable on a machine without Docker.
//!
//! The walks run **one at a time** (see `walk_lock`): the engine's sweep pass works on the whole
//! `workflows` table, so two walks in flight could settle each other's rows.

use std::sync::{Arc, OnceLock};
use std::time::Duration as StdDuration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_automation::actions::AutomationActions;
use omnion_automation::mail::MailSettings;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use omnion_storage::Storage;
use omnion_workflows::engine::{self, RunnerConfig, SweepReport};
use omnion_workflows::store;
use omnion_workflows::{
    StepStatus, TriggerKind, WorkflowDefinition, WorkflowExecution, WorkflowStep,
};
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, MutexGuard};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// The lock that keeps one walk in flight.
///
/// `engine::sweep` is deliberately table-wide (it is what repairs an installation whose runner
/// stopped), and every walk of this suite calls it — so two walks running at once can settle
/// each other's rows: the wait walk parks a step and the reclaim walk's sweep finishes it, the
/// reclaim walk claims a step and the other walk's sweep takes it back. The walks assert on
/// timing, so they hold this lock for their whole run instead of racing on the shared
/// development database.
fn walk_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Permission keys the workflow operator of this suite holds.
const WORKFLOW_PERMISSIONS: [&str; 3] = ["workflows.read", "workflows.manage", "workflows.run"];

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    /// Every `Set-Cookie` the response carried, joined — a response can set more than one.
    set_cookie: Option<String>,
    body: Value,
}

/// Drive the real router without a network socket.
async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");

    let status = response.status();
    // `headers().get(SET_COOKIE)` returns the **first** value only, and a sign-in sets two: the
    // session and the CSRF token. Taking one is how a harness ends up unable to send a header
    // the browser is always holding — the failure then shows up as an unexplained `403` in a
    // test that never mentions CSRF. `get_all` is the only call that sees both.
    let set_cookie = {
        let values: Vec<String> = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_owned)
            .collect();
        (!values.is_empty()).then(|| values.join(","))
    };

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
        set_cookie,
        body,
    }
}

/// Build a JSON request; `token` becomes the session cookie.
fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    request_with_csrf(method, uri, token, None, body)
}

/// Build a JSON request that also carries a CSRF token, the way a browser does.
///
/// **This is the second door to the same room, and the reason it exists matters.** A
/// cookie-authenticated mutation is refused unless it carries `x-omnion-csrf`, and the value is
/// the `omnion_csrf` cookie the sign-in response set — the browser is *handed* a token, it
/// never derives one. The four-argument `request` above cannot express that, so every write
/// test in this file has been answering `403 csrf_failed` whenever a CSRF secret is
/// configured. The suite was green only while the secret was unset — green against the one
/// configuration that refuses every mutation — and it is invisible precisely because the read
/// tests keep passing.
///
/// A suite that quietly stops exercising the write paths and still reports a pass is worse
/// than a red one, so the door is here rather than a secret derivation: **deriving** the token
/// in the harness would test a client that does not exist, and would keep passing if the
/// server ever stopped setting the cookie. A read-safe method sends no header, matching the
/// server, and never putting a credential in a request that has no need for it.
fn request_with_csrf(
    method: Method,
    uri: &str,
    token: Option<&str>,
    csrf: Option<&str>,
    body: Option<Value>,
) -> Request<Body> {
    let header_token = if matches!(method, Method::GET | Method::HEAD | Method::OPTIONS) {
        None
    } else {
        csrf
    };
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::COOKIE, format!("omnion_session={token}"));
    }
    if let Some(header_value) = header_token {
        builder = builder.header(omnion_security::CSRF_HEADER, header_value);
    }

    match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .expect("request must build"),
        None => builder.body(Body::empty()).expect("request must build"),
    }
}

/// Connect to the compose PostgreSQL; `None` means the stack is not running.
async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&config.database).await {
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

/// A state whose database has all migrations applied and the IAM seed loaded.
async fn live_state() -> Option<(AppState, Db)> {
    let config = Config::from_env().expect("environment must be valid");
    let db = live_db(&config).await?;
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

/// How the suite drives the engine: fast batches, millisecond backoff — a retry that would
/// wait five seconds in development is proven in tens of milliseconds here.
fn test_runner() -> RunnerConfig {
    RunnerConfig {
        tick: Duration::milliseconds(10),
        sweep: Duration::milliseconds(10),
        batch: 16,
        retry_base: Duration::milliseconds(30),
        retry_max: Duration::milliseconds(90),
        // Longer than any test step takes, so the reclaim path only fires where a test asks for
        // it (the reclaim walk rebuilds this config with a one-second lease).
        lease: Duration::seconds(30),
        ..RunnerConfig::default()
    }
}

/// Remove rows a previous run of this suite left behind when it panicked before its cleanup.
///
/// The prefix is unique to this file, and the sweep runs once per process (the `OnceCell`),
/// before any fixture of this suite creates rows — a sibling test's fixture is never touched.
async fn sweep_leftovers(db: &Db) {
    static SWEPT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

    SWEPT
        .get_or_init(|| async {
            sqlx::query("delete from organizations where slug like 'workflow-fix-%'")
                .execute(db.pool())
                .await
                .expect("leftover organizations must be removed");
            sqlx::query("delete from users where email like 'workflow-%@omnion.test'")
                .execute(db.pool())
                .await
                .expect("leftover accounts must be removed");
        })
        .await;
}

/// One run as the assertions want to see it.
#[derive(Debug, Clone)]
struct RunState {
    status: String,
    error: Option<String>,
    steps: Vec<WorkflowStep>,
}

impl RunState {
    /// The attempts of the step at 1-based position `step_no`.
    fn attempts_of(&self, step_no: i32) -> i32 {
        self.step(step_no).attempts
    }

    /// The step at 1-based position `step_no`.
    fn step(&self, step_no: i32) -> &WorkflowStep {
        self.steps
            .iter()
            .find(|step| step.step_no == step_no)
            .unwrap_or_else(|| panic!("step {step_no} must exist in {:#?}", self.steps))
    }

    /// The stored status of the step at 1-based position `step_no`.
    fn status_of(&self, step_no: i32) -> StepStatus {
        self.step(step_no).status().expect("a known status")
    }
}

/// Read a run and its steps.
async fn run_state(state: &AppState, execution_id: Uuid) -> RunState {
    let execution = store::find_execution(state.db().pool(), execution_id)
        .await
        .expect("the execution must read")
        .expect("the execution must exist");
    let steps = store::list_steps(state.db().pool(), execution_id)
        .await
        .expect("the steps must read");

    RunState {
        status: execution.status,
        error: execution.error,
        steps,
    }
}

/// Tick until the condition holds, or fail the test with the last state seen.
async fn tick_until(
    state: &AppState,
    runner: &RunnerConfig,
    execution_id: Uuid,
    description: &str,
    condition: impl Fn(&RunState) -> bool,
) -> RunState {
    for _ in 0..400 {
        engine::tick(state.db().pool(), runner)
            .await
            .expect("a tick must run");
        let current = run_state(state, execution_id).await;
        if condition(&current) {
            return current;
        }
        tokio::time::sleep(StdDuration::from_millis(20)).await;
    }

    panic!(
        "the run never reached: {description}; last state: {:#?}",
        run_state(state, execution_id).await
    );
}

/// Drive the engine until the run is terminal.
async fn drive_until_settled(
    state: &AppState,
    runner: &RunnerConfig,
    execution_id: Uuid,
) -> RunState {
    tick_until(state, runner, execution_id, "a terminal state", |state| {
        state.status != "running"
    })
    .await
}

/// Two organizations with one site each, a platform Owner, a workflow operator of the first
/// organization and a plain member without any workflow permission.
struct Fixture {
    /// Holds the suite's walk lock for as long as the fixture lives.
    _walk: MutexGuard<'static, ()>,
    state: AppState,
    db: Db,
    platform_email: String,
    operator_email: String,
    member_email: String,
    organization_a: Uuid,
    organization_b: Uuid,
    accounts: Vec<Uuid>,
    organizations: Vec<Uuid>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        // One walk at a time — see `walk_lock`.
        let walk = walk_lock().lock().await;
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");
        sweep_leftovers(&db).await;

        let organization_a = create_organization_row(&db, "a", "Workflow Test A").await;
        let organization_b = create_organization_row(&db, "b", "Workflow Test B").await;

        // The platform Owner: no primary organization, so it may work across tenants.
        let (platform_id, platform_email) = create_account(&db, None).await;
        seed::bind_owner(db.pool(), platform_id)
            .await
            .expect("the owner binding must be created");

        // The workflow operator: the three workflow keys bound at organization scope.
        let (operator_id, operator_email) = create_account(&db, Some(organization_a)).await;
        let role = role_store::create_role(
            db.pool(),
            NewRole {
                organization_id: organization_a,
                key: format!("workflow-operator-{}", Uuid::new_v4().simple()),
                name: "Workflow Operator".to_owned(),
                description: "Runs the automations of one organization".to_owned(),
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

        // A plain member of the same organization, without any workflow permission.
        let (member_id, member_email) = create_account(&db, Some(organization_a)).await;

        Some(Self {
            _walk: walk,
            state,
            db,
            platform_email,
            operator_email,
            member_email,
            organization_a,
            organization_b,
            accounts: vec![platform_id, operator_id, member_id],
            organizations: vec![organization_a, organization_b],
        })
    }

    /// The platform Owner, signed in.
    async fn platform_token(&self) -> String {
        login(&self.state, &self.platform_email).await
    }

    /// The workflow operator of the first organization, signed in.
    async fn operator_token(&self) -> String {
        login(&self.state, &self.operator_email).await
    }

    /// The plain member of the first organization, signed in.
    async fn member_token(&self) -> String {
        login(&self.state, &self.member_email).await
    }

    /// Create a workflow through the API and return its id.
    async fn create_workflow(&self, token: &str, body: Value) -> String {
        let response = call(
            &self.state,
            request(Method::POST, "/api/v1/workflows", Some(token), Some(body)),
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

    /// Start a run through the API and return the execution id.
    async fn run(&self, token: &str, workflow_id: &str) -> String {
        let response = call(
            &self.state,
            request(
                Method::POST,
                &format!("/api/v1/workflows/{workflow_id}/run"),
                Some(token),
                None,
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::ACCEPTED,
            "run body: {}",
            response.body
        );
        id_of(&response.body)
    }

    /// Remove what this fixture created; runs and definitions cascade with the organizations.
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

/// Create an organization row with a unique, suite-scoped slug.
async fn create_organization_row(db: &Db, label: &str, name: &str) -> Uuid {
    let slug = format!("workflow-fix-{label}-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(name)
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// Create an account with a unique address so parallel runs cannot collide.
async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("workflow-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Workflow Test".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("creating a test account must succeed");
    (user.id, email)
}

/// Sign an account in and return the raw session token.
async fn login(state: &AppState, email: &str) -> String {
    login_with_csrf(state, email).await.0
}

/// Sign in and return the **session token and the CSRF token**.
///
/// The CSRF cookie is a second `Set-Cookie` on the same response, and the previous helper
/// kept only the first — which is exactly how a harness ends up unable to send a header a
/// real browser is always holding. It reads every `Set-Cookie` rather than the first one,
/// because a helper that silently drops the credential it needs is a helper whose failure
/// mode is an unexplained `403` in a test three files away.
async fn login_with_csrf(state: &AppState, email: &str) -> (String, String) {
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

    let cookies = response
        .set_cookie
        .clone()
        .expect("login must set the session cookie");
    let value_of = |name: &str| {
        cookies
            .split(',')
            .map(|pair| pair.split(';').next().unwrap_or(pair).trim())
            .find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                (key.trim() == name).then(|| value.to_owned())
            })
    };

    let session = value_of("omnion_session").expect("login sets a session cookie");
    // A secret that is not configured produces no CSRF cookie, and then none is needed. So the
    // token is optional here and absent is not a panic — a panic would make this helper
    // unusable in the one configuration the rest of the suite has always run in.
    let csrf = value_of("omnion_csrf").unwrap_or_default();
    (session, csrf)
}

/// The `id` field of a response body, as text.
fn id_of(body: &Value) -> String {
    body["id"]
        .as_str()
        .unwrap_or_else(|| panic!("body carries an id: {body}"))
        .to_owned()
}

/// How many audit rows carry one action for one target.
async fn audit_rows(db: &Db, action: &str, target_id: &str) -> i64 {
    sqlx::query_scalar("select count(*) from audit_log where action = $1 and target_id = $2")
        .bind(action)
        .bind(target_id)
        .fetch_one(db.pool())
        .await
        .expect("audit rows must read")
}

/// Wait until an audit row exists, then report how many there are.
///
/// The engine writes the state change first and the audit row immediately after — two writes a
/// few milliseconds apart. A probe that runs in that window sees the new state without its audit
/// row yet, so the positive assertions poll instead of failing the moment the state flips.
async fn wait_for_audit_rows(db: &Db, action: &str, target_id: &str) -> i64 {
    for _ in 0..300 {
        let rows = audit_rows(db, action, target_id).await;
        if rows > 0 {
            return rows;
        }
        tokio::time::sleep(StdDuration::from_millis(10)).await;
    }
    audit_rows(db, action, target_id).await
}

// ---------------------------------------------------------------------------------------------
// The P09 acceptance walks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_three_step_run_retries_a_transient_failure_and_completes() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let runner = test_runner();
    let token = fixture.operator_token().await;

    // Step 2 fails its first attempt: the run must survive it and finish.
    let workflow_id = fixture
        .create_workflow(
            &token,
            json!({
                "name": "Retry walk",
                "description": "Three steps with one transient failure",
                "organization_id": fixture.organization_a,
                "trigger": { "kind": "manual" },
                "steps": [
                    { "name": "prepare", "kind": "task", "action": "noop" },
                    {
                        "name": "unstable",
                        "kind": "task",
                        "action": "transient",
                        "params": { "fail_times": 1 },
                        "max_attempts": 3
                    },
                    {
                        "name": "finish",
                        "kind": "task",
                        "action": "echo",
                        "params": { "value": "done" }
                    }
                ]
            }),
        )
        .await;

    let execution_id = Uuid::parse_str(&fixture.run(&token, &workflow_id).await).expect("a uuid");

    let settled = drive_until_settled(&fixture.state, &runner, execution_id).await;
    assert_eq!(settled.status, "completed", "steps: {:#?}", settled.steps);
    assert_eq!(settled.attempts_of(1), 1, "the first step runs once");
    assert_eq!(
        settled.attempts_of(2),
        2,
        "the transient step failed once and succeeded on its second attempt"
    );
    assert_eq!(settled.attempts_of(3), 1, "the third step runs once");
    assert_eq!(
        settled.step(3).output.as_ref().expect("output")["value"],
        "done"
    );
    assert_eq!(settled.status_of(1), StepStatus::Succeeded);

    // The run reads back through the API, steps included.
    let detail = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflow-executions/{execution_id}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(detail.status, StatusCode::OK, "detail: {}", detail.body);
    assert_eq!(detail.body["status"], "completed");
    assert_eq!(detail.body["steps"].as_array().map(Vec::len), Some(3));
    assert_eq!(detail.body["steps"][1]["attempts"], 2);

    // Both lifecycle changes are in the audit trail.
    let target = execution_id.to_string();
    assert_eq!(
        wait_for_audit_rows(&fixture.db, "workflow.execution.started", &target).await,
        1
    );
    assert_eq!(
        wait_for_audit_rows(&fixture.db, "workflow.execution.completed", &target).await,
        1,
        "a settled run is audited exactly once"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_wait_step_parks_the_run_and_is_resumed_after_its_deadline() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let runner = test_runner();
    let token = fixture.operator_token().await;

    let workflow_id = fixture
        .create_workflow(
            &token,
            json!({
                "name": "Wait walk",
                "organization_id": fixture.organization_a,
                "trigger": { "kind": "manual" },
                "steps": [
                    { "name": "prepare", "kind": "task", "action": "noop" },
                    { "name": "pause", "kind": "wait", "params": { "seconds": 1 } }
                ]
            }),
        )
        .await;

    let execution_id = Uuid::parse_str(&fixture.run(&token, &workflow_id).await).expect("a uuid");

    // The run parks on the wait step instead of holding a connection open.
    let parked = tick_until(
        &fixture.state,
        &runner,
        execution_id,
        "a parked wait",
        |state| state.status_of(2) == StepStatus::Waiting,
    )
    .await;
    assert_eq!(parked.status, "running", "a parked run is still running");
    assert_eq!(parked.status_of(1), StepStatus::Succeeded);
    assert_eq!(parked.attempts_of(2), 1, "the wait was claimed once so far");

    // The wait is a write, not a sleep: the deadline is in the store.
    let deadline = parked.step(2).available_at;
    assert!(
        deadline > time::OffsetDateTime::now_utc(),
        "the wait deadline is in the future"
    );

    // Past the deadline the runner claims the parked step once more — that claim is the resume.
    tokio::time::sleep(StdDuration::from_millis(1_100)).await;
    let settled = drive_until_settled(&fixture.state, &runner, execution_id).await;
    assert_eq!(settled.status, "completed", "steps: {:#?}", settled.steps);
    assert_eq!(settled.attempts_of(2), 2, "park + resume are two claims");
    assert_eq!(
        settled.step(2).output.as_ref().expect("output")["resumed"],
        true
    );

    assert_eq!(
        wait_for_audit_rows(
            &fixture.db,
            "workflow.execution.completed",
            &execution_id.to_string()
        )
        .await,
        1
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_sweeper_resolves_a_wait_the_runner_never_got_to() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let runner = test_runner();
    let token = fixture.operator_token().await;

    let workflow_id = fixture
        .create_workflow(
            &token,
            json!({
                "name": "Swept wait",
                "organization_id": fixture.organization_a,
                "trigger": { "kind": "manual" },
                "steps": [
                    { "name": "prepare", "kind": "task", "action": "noop" },
                    { "name": "pause", "kind": "wait", "params": { "seconds": 1 } }
                ]
            }),
        )
        .await;

    let execution_id = Uuid::parse_str(&fixture.run(&token, &workflow_id).await).expect("a uuid");
    tick_until(
        &fixture.state,
        &runner,
        execution_id,
        "a parked wait",
        |state| state.status_of(2) == StepStatus::Waiting,
    )
    .await;

    // Past the deadline the sweep pass itself closes the wait and the run with it.
    tokio::time::sleep(StdDuration::from_millis(1_100)).await;
    let sweep: SweepReport = engine::sweep(fixture.state.db().pool(), &runner)
        .await
        .expect("the sweep must run");
    assert_eq!(sweep.waits_resolved, 1, "the due wait was resolved");

    let settled = run_state(&fixture.state, execution_id).await;
    assert_eq!(settled.status, "completed", "steps: {:#?}", settled.steps);
    assert_eq!(settled.status_of(2), StepStatus::Succeeded);
    assert_eq!(
        settled.attempts_of(2),
        1,
        "the sweep finishes the wait without claiming it again"
    );
    assert_eq!(
        wait_for_audit_rows(
            &fixture.db,
            "workflow.execution.completed",
            &execution_id.to_string()
        )
        .await,
        1,
        "the run the sweep closed is audited"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_step_whose_runner_stopped_is_reclaimed_by_the_sweeper() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // This walk needs the reclaim path, so its runner treats a one-second-old claim as lost.
    let runner = RunnerConfig {
        lease: Duration::seconds(1),
        ..test_runner()
    };
    let token = fixture.operator_token().await;

    let workflow_id = fixture
        .create_workflow(
            &token,
            json!({
                "name": "Reclaim walk",
                "organization_id": fixture.organization_a,
                "trigger": { "kind": "manual" },
                "steps": [
                    {
                        "name": "prepare",
                        "kind": "task",
                        "action": "noop",
                        "max_attempts": 2
                    },
                    { "name": "finish", "kind": "task", "action": "noop" }
                ]
            }),
        )
        .await;

    let execution_id = Uuid::parse_str(&fixture.run(&token, &workflow_id).await).expect("a uuid");

    // A runner claims the first step and stops before it finishes anything.
    let claimed = store::claim_due_step(fixture.state.db().pool())
        .await
        .expect("the claim must run")
        .expect("a due step exists");
    assert_eq!(claimed.step_no, 1);
    assert_eq!(claimed.attempts, 1, "the lost attempt is counted");
    assert_eq!(
        run_state(&fixture.state, execution_id).await.status_of(1),
        StepStatus::Running
    );

    // The lease has not run out yet, so nothing is taken back.
    let early = engine::sweep(fixture.state.db().pool(), &runner)
        .await
        .expect("the sweep must run");
    assert_eq!(early.steps_reclaimed, 0, "a fresh claim is not disturbed");

    // Past the lease the sweeper puts the step back on the queue …
    tokio::time::sleep(StdDuration::from_millis(1_100)).await;
    let late = engine::sweep(fixture.state.db().pool(), &runner)
        .await
        .expect("the second sweep must run");
    assert_eq!(late.steps_reclaimed, 1, "the abandoned claim was reclaimed");
    assert_eq!(
        run_state(&fixture.state, execution_id).await.status_of(1),
        StepStatus::Pending
    );

    // … and the run finishes on the next attempts.
    let settled = drive_until_settled(&fixture.state, &runner, execution_id).await;
    assert_eq!(settled.status, "completed", "steps: {:#?}", settled.steps);
    assert_eq!(settled.attempts_of(1), 2, "the reclaimed step ran again");

    fixture.cleanup().await;
}

#[tokio::test]
async fn cancelling_a_run_stops_it_and_closes_its_steps() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let runner = test_runner();
    let token = fixture.operator_token().await;

    let workflow_id = fixture
        .create_workflow(
            &token,
            json!({
                "name": "Cancel walk",
                "organization_id": fixture.organization_a,
                "trigger": { "kind": "manual" },
                "steps": [
                    { "name": "prepare", "kind": "task", "action": "noop" },
                    { "name": "pause", "kind": "wait", "params": { "seconds": 600 } }
                ]
            }),
        )
        .await;

    let execution_id = Uuid::parse_str(&fixture.run(&token, &workflow_id).await).expect("a uuid");
    tick_until(
        &fixture.state,
        &runner,
        execution_id,
        "a parked wait",
        |state| state.status_of(2) == StepStatus::Waiting,
    )
    .await;

    let cancelled = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflow-executions/{execution_id}/cancel"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(
        cancelled.status,
        StatusCode::OK,
        "cancel body: {}",
        cancelled.body
    );
    assert_eq!(cancelled.body["status"], "cancelled");

    let state = run_state(&fixture.state, execution_id).await;
    assert_eq!(state.status, "cancelled");
    assert_eq!(state.status_of(1), StepStatus::Succeeded);
    assert_eq!(state.status_of(2), StepStatus::Cancelled);

    // Nothing moves afterwards, and a second cancel is refused.
    engine::tick(fixture.state.db().pool(), &runner)
        .await
        .expect("a tick must run");
    let after = run_state(&fixture.state, execution_id).await;
    assert_eq!(after.status, "cancelled");
    assert_eq!(
        after.attempts_of(2),
        1,
        "the cancelled wait never ran again"
    );

    let again = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflow-executions/{execution_id}/cancel"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.body["error"]["code"], "execution_not_running");

    assert_eq!(
        wait_for_audit_rows(
            &fixture.db,
            "workflow.execution.cancelled",
            &execution_id.to_string()
        )
        .await,
        1
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_run_out_of_attempts_fails_and_says_why() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let runner = test_runner();
    let token = fixture.operator_token().await;

    let workflow_id = fixture
        .create_workflow(
            &token,
            json!({
                "name": "Failure walk",
                "organization_id": fixture.organization_a,
                "trigger": { "kind": "manual" },
                "steps": [
                    {
                        "name": "boom",
                        "kind": "task",
                        "action": "fail",
                        "params": { "message": "the integration is down" },
                        "max_attempts": 2
                    }
                ]
            }),
        )
        .await;

    let execution_id = Uuid::parse_str(&fixture.run(&token, &workflow_id).await).expect("a uuid");
    let settled = drive_until_settled(&fixture.state, &runner, execution_id).await;

    assert_eq!(settled.status, "failed");
    assert_eq!(settled.attempts_of(1), 2, "the step used both attempts");
    assert_eq!(settled.status_of(1), StepStatus::Failed);
    assert_eq!(
        settled.error.as_deref(),
        Some("the integration is down"),
        "the run reports the step's own message"
    );

    let target = execution_id.to_string();
    assert_eq!(
        wait_for_audit_rows(&fixture.db, "workflow.execution.failed", &target).await,
        1
    );
    assert_eq!(
        audit_rows(&fixture.db, "workflow.execution.completed", &target).await,
        0
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_schedule_trigger_starts_a_run_once_per_due_time() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let runner = test_runner();
    let token = fixture.operator_token().await;

    let workflow_id = fixture
        .create_workflow(
            &token,
            json!({
                "name": "Scheduled walk",
                "organization_id": fixture.organization_a,
                "trigger": { "kind": "schedule", "cron": "0 3 * * *" },
                "steps": [
                    { "name": "prepare", "kind": "task", "action": "noop" }
                ]
            }),
        )
        .await;

    // The creation response already carries the first due time.
    let created = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{workflow_id}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(created.body["trigger"], "schedule");
    assert_eq!(created.body["schedule"], "0 3 * * *");
    assert!(
        created.body["next_run_at"].as_str().is_some(),
        "a schedule is armed with a due time: {}",
        created.body
    );

    // Force the due time into the past instead of waiting for 03:00 UTC.
    sqlx::query("update workflows set next_run_at = now() - interval '1 second' where id = $1")
        .bind(Uuid::parse_str(&workflow_id).expect("a uuid"))
        .execute(fixture.db.pool())
        .await
        .expect("the due time must move");

    engine::tick(fixture.state.db().pool(), &runner)
        .await
        .expect("the tick must start the schedule");

    let runs = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{workflow_id}/executions"),
            Some(&token),
            None,
        ),
    )
    .await;
    let executions = runs.body["executions"]
        .as_array()
        .expect("executions must be an array");
    assert_eq!(executions.len(), 1, "the due schedule started one run");
    assert_eq!(executions[0]["trigger"], "schedule");

    // The next due time is in the future, so a second tick starts nothing.
    engine::tick(fixture.state.db().pool(), &runner)
        .await
        .expect("the second tick must run");
    let runs = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{workflow_id}/executions"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(
        runs.body["executions"].as_array().map(Vec::len),
        Some(1),
        "a schedule does not fire twice for one due time"
    );

    // The scheduled run itself completes like any other.
    let execution_id = executions[0]["id"].as_str().expect("an id");
    let settled = drive_until_settled(
        &fixture.state,
        &runner,
        Uuid::parse_str(execution_id).expect("a uuid"),
    )
    .await;
    assert_eq!(settled.status, "completed");

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_run_left_open_by_a_stopped_process_is_settled_by_the_sweeper() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let runner = test_runner();
    let token = fixture.operator_token().await;

    let workflow_id = fixture
        .create_workflow(
            &token,
            json!({
                "name": "Crash walk",
                "organization_id": fixture.organization_a,
                "trigger": { "kind": "manual" },
                "steps": [
                    { "name": "prepare", "kind": "task", "action": "noop" }
                ]
            }),
        )
        .await;

    let execution_id = Uuid::parse_str(&fixture.run(&token, &workflow_id).await).expect("a uuid");

    // Simulate the crash: the step finished, the process stopped before the run was settled.
    sqlx::query(
        "update workflow_steps set status = 'succeeded', finished_at = now(), \
         output = '{\"action\":\"noop\"}'::jsonb where execution_id = $1",
    )
    .bind(execution_id)
    .execute(fixture.db.pool())
    .await
    .expect("the step must be closed by hand");

    let stale = run_state(&fixture.state, execution_id).await;
    assert_eq!(stale.status, "running", "the run is still open");

    let sweep = engine::sweep(fixture.state.db().pool(), &runner)
        .await
        .expect("the sweep must run");
    assert_eq!(sweep.executions_settled, 1, "the sweeper settled the run");

    let settled = run_state(&fixture.state, execution_id).await;
    assert_eq!(settled.status, "completed");
    assert_eq!(
        wait_for_audit_rows(
            &fixture.db,
            "workflow.execution.completed",
            &execution_id.to_string()
        )
        .await,
        1,
        "the sweeper audits the settlement it performed"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_workflow_surface_is_permission_gated_and_tenant_scoped() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.operator_token().await;
    let member = fixture.member_token().await;
    let platform = fixture.platform_token().await;

    let workflow_id = fixture
        .create_workflow(
            &token,
            json!({
                "name": "Tenant walk",
                "organization_id": fixture.organization_a,
                "trigger": { "kind": "manual" },
                "steps": [ { "name": "prepare", "kind": "task", "action": "noop" } ]
            }),
        )
        .await;

    // Without a session every route is closed.
    for (method, uri) in [
        (Method::GET, "/api/v1/workflows".to_owned()),
        (Method::POST, "/api/v1/workflows".to_owned()),
        (Method::POST, format!("/api/v1/workflows/{workflow_id}/run")),
        (
            Method::GET,
            format!("/api/v1/workflows/{workflow_id}/executions"),
        ),
        (
            Method::GET,
            format!("/api/v1/workflow-executions/{}", Uuid::new_v4()),
        ),
    ] {
        let response = call(&fixture.state, request(method.clone(), &uri, None, None)).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(response.body["error"]["code"], "unauthenticated");
    }

    // A signed-in account without a workflow permission sees none of it.
    let denied = call(
        &fixture.state,
        request(Method::GET, "/api/v1/workflows", Some(&member), None),
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(denied.body["error"]["code"], "permission_denied");

    let denied_run = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflows/{workflow_id}/run"),
            Some(&member),
            None,
        ),
    )
    .await;
    assert_eq!(denied_run.status, StatusCode::FORBIDDEN);

    // The operator of the first organization lists its own workflows only …
    let listing = call(
        &fixture.state,
        request(Method::GET, "/api/v1/workflows", Some(&token), None),
    )
    .await;
    assert_eq!(listing.status, StatusCode::OK);
    let listed = listing.body["workflows"].as_array().expect("a list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], workflow_id.as_str());

    // … and cannot step into the other tenant.
    let foreign = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/workflows?organization_id={}",
                fixture.organization_b
            ),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(foreign.status, StatusCode::FORBIDDEN);
    assert_eq!(foreign.body["error"]["code"], "cross_organization");

    // The platform Owner opens a workflow in the second organization …
    let foreign_id = fixture
        .create_workflow(
            &platform,
            json!({
                "name": "Other tenant",
                "organization_id": fixture.organization_b,
                "trigger": { "kind": "manual" },
                "steps": [ { "name": "prepare", "kind": "task", "action": "noop" } ]
            }),
        )
        .await;

    // … and the operator of the first organization cannot read or run it.
    for (method, uri) in [
        (Method::GET, format!("/api/v1/workflows/{foreign_id}")),
        (Method::POST, format!("/api/v1/workflows/{foreign_id}/run")),
    ] {
        let response = call(
            &fixture.state,
            request(method.clone(), &uri, Some(&token), None),
        )
        .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{uri}");
        assert_eq!(
            response.body["error"]["code"], "cross_organization",
            "{uri}"
        );
    }

    // A foreign run id is out of scope the same way.
    let foreign_execution = fixture.run(&platform, &foreign_id).await;
    let reached = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflow-executions/{foreign_execution}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(reached.status, StatusCode::FORBIDDEN);

    // Editing and removing a definition is the operator's own business again.
    let updated = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/workflows/{workflow_id}"),
            Some(&token),
            Some(json!({
                "name": "Tenant walk (edited)",
                "organization_id": fixture.organization_a,
                "trigger": { "kind": "manual" },
                "steps": [
                    { "name": "prepare", "kind": "task", "action": "noop" },
                    { "name": "finish", "kind": "task", "action": "noop" }
                ]
            })),
        ),
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK, "update: {}", updated.body);
    assert_eq!(updated.body["step_count"], 2);

    let removed = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/workflows/{workflow_id}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);

    let gone = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{workflow_id}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    assert_eq!(gone.body["error"]["code"], "workflow_not_found");

    // Both definition changes are audited.
    assert_eq!(
        wait_for_audit_rows(&fixture.db, "workflow.updated", &workflow_id).await,
        1
    );
    assert_eq!(
        wait_for_audit_rows(&fixture.db, "workflow.deleted", &workflow_id).await,
        1
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn unusable_definitions_are_refused_with_the_engine_codes() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.operator_token().await;

    let cases: Vec<(&str, Value, &str)> = vec![
        (
            // `http_request` IS a known action, so the refusal here is about its *parameters*,
            // not about the name: the case used to name it as "unknown" and stopped being true
            // in slice 2, when the action library landed. An action the engine really does not
            // know is covered by the case below.
            "known action without its required parameter",
            json!([{ "name": "prepare", "kind": "task", "action": "http_request" }]),
            "invalid_step_params",
        ),
        (
            "an action the engine does not know",
            json!([{ "name": "prepare", "kind": "task", "action": "not_an_action" }]),
            "invalid_step_action",
        ),
        (
            "too many attempts",
            json!([{
                "name": "prepare",
                "kind": "task",
                "action": "noop",
                "max_attempts": 9
            }]),
            "invalid_max_attempts",
        ),
        (
            "a retried wait",
            json!([{ "name": "pause", "kind": "wait", "params": { "seconds": 5 }, "max_attempts": 2 }]),
            "invalid_max_attempts",
        ),
        (
            "a zero-second wait",
            json!([{ "name": "pause", "kind": "wait", "params": { "seconds": 0 } }]),
            "invalid_wait",
        ),
        ("no steps", json!([]), "invalid_steps"),
    ];

    for (label, steps, code) in cases {
        let response = call(
            &fixture.state,
            request(
                Method::POST,
                "/api/v1/workflows",
                Some(&token),
                Some(json!({
                    "name": format!("Broken: {label}"),
                    "organization_id": fixture.organization_a,
                    "trigger": { "kind": "manual" },
                    "steps": steps
                })),
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{label}");
        assert_eq!(response.body["error"]["code"], code, "{label}");
    }

    // A schedule needs a parseable cron, and a manual trigger carries none.
    let broken_cron = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/workflows",
            Some(&token),
            Some(json!({
                "name": "Broken cron",
                "organization_id": fixture.organization_a,
                "trigger": { "kind": "schedule", "cron": "not a cron" },
                "steps": [ { "name": "prepare", "kind": "task", "action": "noop" } ]
            })),
        ),
    )
    .await;
    assert_eq!(broken_cron.status, StatusCode::BAD_REQUEST);
    assert_eq!(broken_cron.body["error"]["code"], "invalid_cron");

    let missing_cron = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/workflows",
            Some(&token),
            Some(json!({
                "name": "Missing cron",
                "organization_id": fixture.organization_a,
                "trigger": { "kind": "schedule" },
                "steps": [ { "name": "prepare", "kind": "task", "action": "noop" } ]
            })),
        ),
    )
    .await;
    assert_eq!(missing_cron.status, StatusCode::BAD_REQUEST);
    assert_eq!(missing_cron.body["error"]["code"], "invalid_trigger");

    fixture.cleanup().await;
}

/// The engine's own view of a definition, exercised without a database.
#[test]
fn the_definition_rules_are_the_engine_rules() {
    let definition = WorkflowDefinition::new(
        omnion_workflows::Trigger::manual(),
        vec![
            omnion_workflows::StepDefinition::task("prepare", "noop", json!({})),
            omnion_workflows::StepDefinition::wait("pause", 10),
        ],
    )
    .expect("a manual definition is valid");
    assert_eq!(definition.steps.len(), 2);

    let workflow = store::WorkflowUpdate {
        name: "unused".to_owned(),
        description: String::new(),
        site_id: None,
        enabled: true,
        trigger: TriggerKind::Manual,
        schedule: None,
        trigger_event: None,
        conditions: json!([]),
        next_run_at: None,
        on_error: omnion_workflows::OnError::Stop,
        run_as_user_id: None,
        // Absent means "leave the stored bound alone" — the panel writes these, a whole-rule
        // write that has not learned about them must not reset an author's rate limit.
        rate_limit_per_hour: None,
        concurrency: None,
        steps: definition.steps_json().expect("steps serialise"),
    };
    assert_eq!(workflow.trigger, TriggerKind::Manual);

    // A stored run without a terminal state is "running" for the panel and for the sweeper.
    let execution = WorkflowExecution {
        id: Uuid::nil(),
        workflow_id: Uuid::nil(),
        organization_id: Uuid::nil(),
        status: "running".to_owned(),
        trigger_kind: "manual".to_owned(),
        triggered_by: None,
        started_at: time::OffsetDateTime::UNIX_EPOCH,
        finished_at: None,
        error: None,
        approval_id: None,
        event_payload: None,
        started_from_node: None,
    };
    assert!(!execution.is_terminal());
}

/// The graph store's own read path, against a real database.
///
/// `find_graph` selected `workflow_id` from `workflows`, whose primary key is `id`, so every
/// read failed with `column "workflow_id" does not exist` — and the builder's only symptom
/// was its error screen, which a walkthrough reporting "the builder did not open" cannot tell
/// apart from a missing page. A store that cannot read its own table is not vouched for by a
/// unit test on `project_steps`, so the guard is the statement against the real schema.
#[tokio::test]
async fn the_graph_store_reads_a_column_the_workflows_table_actually_has() {
    let Some((_state, db)) = live_state().await else {
        return;
    };

    // The schema is the assertion: every column `find_graph` selects must exist on `workflows`.
    // Reading the catalogue beats running the query, because a query that happens to hit no
    // row still proves nothing about a column that only some rows carry.
    //
    // `id` is the primary key, and `find_graph` reads it as `id as workflow_id` — so the
    // physical column list starts with `id`, not `workflow_id`. A test that asserted the alias
    // instead would have demanded the bug this test was written for.
    let named = [
        "id",
        "graph",
        "ui_state",
        "graph_version",
        "validated_at",
        "validation_error",
        "steps",
    ];
    for column in named {
        let exists: i64 = sqlx::query_scalar(
            "select count(*) from information_schema.columns \
             where table_schema = 'public' and table_name = 'workflows' and column_name = $1",
        )
        .bind(column)
        .fetch_one(db.pool())
        .await
        .expect("the catalogue is readable");
        assert_eq!(exists, 1, "workflows has no column named {column}");
    }

    // And the read itself must not be a database error. A column that does not exist arrives as
    // `WorkflowError::Database`, and it is the *only* way `find_graph` fails here: an absent id
    // is `Ok(None)`. Matching the variant rather than `is_err` is what makes the failure name
    // the column, which is the whole point of the guard.
    match omnion_workflows::graph_store::find_graph(db.pool(), uuid::Uuid::nil()).await {
        Ok(None) => {}
        Ok(Some(found)) => panic!(
            "a nil uuid must not match a workflow, got {}",
            found.workflow_id
        ),
        Err(err) => panic!("find_graph named a column the workflows table does not have: {err}"),
    }
}

/// A rule created through the UI must open in the builder on a graph the server will accept.
///
/// The column default for `workflows.graph` is `{"nodes":[],"edges":[]}`, so a rule created
/// after 0051 — every rule, since the backfill only covered the ones that existed — inherited
/// an empty graph. The builder opened on a blank canvas and refused the first save with
/// `graph_invalid` ("the graph has no nodes, a definition needs at least a trigger"), on a rule
/// the author had just created through that same screen. The insert is what makes the row born
/// valid, so the insert is where the proof belongs: a real insert, then the real validator.
#[tokio::test]
async fn a_new_rule_is_born_with_a_graph_the_server_will_save() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = login(&fixture.state, &fixture.operator_email).await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/workflows",
            Some(&token),
            Some(json!({
                "name": "Born with a valid graph",
                "organization_id": fixture.organization_a,
                "trigger": { "kind": "event", "event": "page.published" },
                "steps": [ { "name": "prepare", "kind": "task", "action": "noop" } ]
            })),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "create refused: {}",
        created.body
    );
    let workflow_id = created.body["id"]
        .as_str()
        .expect("a created rule has an id")
        .to_owned();

    // The row the insert actually wrote.
    let stored: Value = sqlx::query_as::<_, (Value, i32)>(
        "select graph, graph_version from workflows where id = $1::uuid",
    )
    .bind(Uuid::parse_str(&workflow_id).expect("an id parses"))
    .fetch_one(fixture.db.pool())
    .await
    .expect("the row is readable")
    .0;
    let graph = stored;
    let node_count = graph["nodes"].as_array().map_or(0, Vec::len);
    assert!(
        node_count > 0,
        "a new rule was born with no graph nodes: {graph}"
    );

    // The version the panel would be holding: it opens the graph, then saves what it read.
    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{workflow_id}/graph"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(
        read.status,
        StatusCode::OK,
        "reading a new rule's graph: {}",
        read.body
    );
    let current_version = read.body["graph_version"].clone();

    // And the server agrees: the graph it just wrote must validate, or the first save is a
    // refusal the author cannot do anything about.
    let saved = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/workflows/{workflow_id}/graph"),
            Some(&token),
            // The write envelope, not the bare graph: `graph_version` is what the server
            // compares, and a PUT without it is a refusal about the request, not the graph.
            Some(json!({ "graph": graph, "graph_version": current_version })),
        ),
    )
    .await;
    assert_eq!(
        saved.status,
        StatusCode::OK,
        "the graph a new rule is born with cannot be saved: {graph} -> {}",
        saved.body
    );
    assert_eq!(
        saved.body["projection"]["valid"],
        Value::Bool(true),
        "the graph a new rule is born with does not validate: {graph}"
    );

    fixture.cleanup().await;
}

/// REQ-004 slice 4: **a rule opened from a LIST row can be saved.**
///
/// The bug this closes was invisible from the builder and obvious from the API. The graph
/// write is guarded by `graph_version`, and that column existed — but only
/// `GET /workflows/{id}/graph` sent it. The two *list* routes (`/workflows` and
/// `/automations`, which is the surface the panel's rule list actually reads) never did, so
/// every client that started from a list row had nothing to quote. The honest fallback —
/// send `0` — is refused with `graph_version_required`, an error about a version the author
/// was never shown. Meanwhile a raw `fetch` to the same route, which had read the graph
/// detail first, returned a clean `409 graph_version_conflict`: the server fine, the client
/// broken, and the two looking like different products.
///
/// So the assertion is deliberately the *whole* round trip rather than "the field is
/// serialized": read the list, take the version off a row, PUT the graph that row names,
/// and expect `200`. A body-shape assertion would have passed while leaving every
/// list-originated save broken, which is the shape of the defect this test exists to prevent.
#[tokio::test]
async fn a_rule_opened_from_a_list_row_carries_the_version_a_save_must_quote() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = login_with_csrf(&fixture.state, &fixture.operator_email).await;

    let created = call(
        &fixture.state,
        request_with_csrf(
            Method::POST,
            "/api/v1/workflows",
            Some(&token),
            Some(&csrf),
            Some(json!({
                "name": "Opened from the list",
                "organization_id": fixture.organization_a,
                "trigger": { "kind": "event", "event": "page.published" },
                "steps": [ { "name": "prepare", "kind": "task", "action": "noop" } ]
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "create: {}", created.body);
    let workflow_id = created.body["id"]
        .as_str()
        .expect("a created rule has an id")
        .to_owned();

    // ---- Both list surfaces have to carry it ------------------------------------------------
    // Two, not one: the panel's rule list reads `/automations` and the workflow list reads
    // `/workflows`, and they are separate projections of the same row. Fixing one leaves the
    // other exactly as broken, which is how a defect survives the commit that claims to fix it.
    for label in ["/api/v1/workflows", "/api/v1/automations"] {
        let listed = call(
            &fixture.state,
            request(Method::GET, label, Some(&token), None),
        )
        .await;
        assert_eq!(listed.status, StatusCode::OK, "{label}: {}", listed.body);

        let row = listed.body["workflows"]
            .as_array()
            .or(listed.body["automations"].as_array())
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row["id"].as_str() == Some(workflow_id.as_str()))
            })
            .unwrap_or_else(|| {
                panic!(
                    "{label} does not list the rule just created: {}",
                    listed.body
                )
            });

        let quoted = row["graph_version"].as_i64().unwrap_or_else(|| {
            panic!(
                "{label} sends no graph_version, so a client saving from this row has nothing \
                 to quote: {row}"
            )
        });
        assert!(
            quoted >= 1,
            "{label} sends graph_version {quoted}; a save must quote a version from 1 up, and 0 \
             is the answer the server refuses as `graph_version_required`"
        );
    }

    // ---- And the save itself, using ONLY what the list gave us ------------------------------
    // The graph is read back for its content, but the *version* deliberately comes from the
    // list row: quoting the detail route's version would pass even with the list broken, which
    // is precisely the substitution that hid this in the first place.
    let listed = call(
        &fixture.state,
        request(Method::GET, "/api/v1/workflows", Some(&token), None),
    )
    .await;
    let from_list: i64 = listed.body["workflows"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["id"].as_str() == Some(workflow_id.as_str()))
        })
        .and_then(|row| row["graph_version"].as_i64())
        .expect("the list carries the version this test is about");

    let graph = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{workflow_id}/graph"),
            Some(&token),
            None,
        ),
    )
    .await
    .body["graph"]
    .clone();

    let saved = call(
        &fixture.state,
        request_with_csrf(
            Method::PUT,
            &format!("/api/v1/workflows/{workflow_id}/graph"),
            Some(&token),
            Some(&csrf),
            Some(json!({ "graph": graph, "graph_version": from_list })),
        ),
    )
    .await;
    assert_eq!(
        saved.status,
        StatusCode::OK,
        "a rule opened from a list row cannot be saved with the version that row carries: \
         quoted {from_list}, got {}",
        saved.body
    );
    assert_eq!(
        saved.body["graph_version"].as_i64(),
        Some(from_list + 1),
        "the write must advance the version exactly once"
    );

    fixture.cleanup().await;
}

/// REQ-004 slice 3: *Run from here* on a mid-graph node.
///
/// The criterion has three clauses and the response has to answer all three, so the test
/// reads the **stored rows** rather than the response body. A handler that returned a
/// plan-shaped payload while writing a full run would pass a body-only test and fail this
/// one — and the failure is the one the criterion is about: a run that quietly executes
/// the prefix the author asked to skip.
#[tokio::test]
async fn run_from_here_starts_at_the_node_and_marks_the_prefix_skipped() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = login(&fixture.state, &fixture.operator_email).await;
    let runner = test_runner();

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/workflows",
            Some(&token),
            Some(json!({
                "name": "Run from here",
                "organization_id": fixture.organization_a,
                "trigger": { "kind": "manual" },
                "steps": [ { "name": "prepare", "kind": "task", "action": "noop" } ]
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let workflow_id = created.body["id"]
        .as_str()
        .expect("a created rule has an id")
        .to_owned();

    // A four-step spine written through the real save path, so the graph the run reads is
    // the graph the server would keep: trigger → a → b → c → end.
    let current_version = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{workflow_id}/graph"),
            Some(&token),
            None,
        ),
    )
    .await
    .body["graph_version"]
        .clone();

    let spine = json!({
        "nodes": [
            { "id": "trigger", "type": "trigger.manual", "label": "Trigger",
              "params": { "kind": "manual" }, "position": { "x": 0, "y": 0 } },
            { "id": "a", "type": "action", "label": "a",
              "params": { "action": "noop", "parameters": {} }, "position": { "x": 1, "y": 0 } },
            { "id": "b", "type": "action", "label": "b",
              "params": { "action": "noop", "parameters": {} }, "position": { "x": 2, "y": 0 } },
            { "id": "c", "type": "action", "label": "c",
              "params": { "action": "noop", "parameters": {} }, "position": { "x": 3, "y": 0 } },
            { "id": "end", "type": "end", "label": "End", "params": {},
              "position": { "x": 4, "y": 0 } }
        ],
        "edges": [
            { "id": "e1", "source": "trigger", "source_port": "out", "target": "a" },
            { "id": "e2", "source": "a", "source_port": "success", "target": "b" },
            { "id": "e3", "source": "b", "source_port": "success", "target": "c" },
            { "id": "e4", "source": "c", "source_port": "success", "target": "end" }
        ]
    });

    let saved = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/workflows/{workflow_id}/graph"),
            Some(&token),
            Some(json!({ "graph": spine, "graph_version": current_version })),
        ),
    )
    .await;
    assert_eq!(
        saved.status,
        StatusCode::OK,
        "the spine was refused: {}",
        saved.body
    );
    assert_eq!(
        saved.body["projection"]["valid"],
        Value::Bool(true),
        "{}",
        saved.body
    );

    // Start at `b`: the run's first step is `b`, `a` is passed over, `c` and the end run.
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflows/{workflow_id}/run-from-node"),
            Some(&token),
            Some(json!({ "node_id": "b" })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::ACCEPTED,
        "Run from here was refused: {}",
        response.body
    );
    assert_eq!(response.body["started_from_node"], "b", "{}", response.body);

    let execution_id = response.body["id"]
        .as_str()
        .expect("an accepted run has an id")
        .to_owned();

    // **The stored rows**, not the response: this is the clause that matters.
    let rows: Vec<(i32, String, Option<String>)> = sqlx::query_as(
        "select step_no, status, skip_reason from workflow_steps \
         where execution_id = $1::uuid order by step_no asc",
    )
    .bind(Uuid::parse_str(&execution_id).expect("an id parses"))
    .fetch_all(fixture.db.pool())
    .await
    .expect("the run's steps are readable");

    let status_of = |no: i32| {
        rows.iter()
            .find(|(step_no, _, _)| *step_no == no)
            .map(|(_, status, _)| status.clone())
    };
    let reason_of = |no: i32| {
        rows.iter()
            .find(|(step_no, _, _)| *step_no == no)
            .and_then(|(_, _, reason)| reason.clone())
    };

    assert_eq!(
        status_of(1).as_deref(),
        Some("skipped"),
        "the step before the start did not stay skipped: {rows:?}"
    );
    assert_ne!(
        status_of(2).as_deref(),
        Some("skipped"),
        "the node the run started at was itself skipped: {rows:?}"
    );

    // **The trace says why** — and the reason names the node, which is the part that
    // distinguishes "the run was started down here on purpose" from "the engine got
    // lost". A reason that only said "skipped" would satisfy the word and not the clause.
    let reason = reason_of(1).expect("a skipped step must carry a reason");
    assert!(
        reason.contains("\"b\""),
        "the reason must name the node the run started at: {reason:?}"
    );

    // The engine runs the tail and settles the run: a skipped prefix is closed work, not
    // open work, so a run whose tail succeeded completes rather than waiting forever for a
    // step that will never be claimed.
    let settled = drive_until_settled(
        &fixture.state,
        &runner,
        Uuid::parse_str(&execution_id).expect("an id parses"),
    )
    .await;
    assert_eq!(
        settled.status, "completed",
        "a run whose prefix was skipped must still complete: {settled:?}"
    );
    assert_eq!(
        status_of(1).as_deref(),
        Some("skipped"),
        "the engine must not have touched the skipped prefix: {rows:?}"
    );
    assert_eq!(
        settled.step(1).attempts,
        0,
        "a skipped step is never claimed, so it spends no attempt: {:?}",
        settled.step(1)
    );

    // The run records where it started, so a trace read months later can say so.
    let started_from: Option<String> =
        sqlx::query_scalar("select started_from_node from workflow_executions where id = $1::uuid")
            .bind(Uuid::parse_str(&execution_id).expect("an id parses"))
            .fetch_one(fixture.db.pool())
            .await
            .expect("the run row is readable");
    assert_eq!(started_from.as_deref(), Some("b"));

    // Starting at the end is refused by name, because a run there would settle
    // `completed` having done nothing — the one answer that must never be given.
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflows/{workflow_id}/run-from-node"),
            Some(&token),
            Some(json!({ "node_id": "end" })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "starting at the end of the graph must be refused: {}",
        refused.body
    );
    assert_eq!(
        refused.body["error"]["code"], "nothing_to_run",
        "{}",
        refused.body
    );

    // A node the graph does not have is refused too, rather than quietly starting a full
    // run — the failure mode of a client that lost its canvas and guessed.
    let unknown = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflows/{workflow_id}/run-from-node"),
            Some(&token),
            Some(json!({ "node_id": "not-a-node" })),
        ),
    )
    .await;
    assert_eq!(
        unknown.status,
        StatusCode::BAD_REQUEST,
        "an unknown node must be refused: {}",
        unknown.body
    );

    // The two fields the canvas paints with, read off the *response body* rather than off
    // the database. Both were stored correctly and neither was on the wire, which is the
    // shape of bug a row-level test cannot see: the write worked, the read did not, and
    // the builder had nothing to paint with.
    let detail = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflow-executions/{execution_id}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(
        detail.status,
        StatusCode::OK,
        "the run must be readable: {}",
        detail.body
    );
    assert_eq!(
        detail.body["started_from_node"], "b",
        "the run has to say where it started: {}",
        detail.body
    );

    let steps = detail.body["steps"]
        .as_array()
        .expect("a run's steps are an array");
    let skipped_on_wire = steps
        .iter()
        .find(|step| step["status"] == "skipped")
        .unwrap_or_else(|| panic!("the skipped step is on the wire: {steps:?}"));
    assert_eq!(
        skipped_on_wire["node_id"], "a",
        "a step has to name the node it came from, or the canvas cannot paint it: \
         {skipped_on_wire}"
    );
    assert!(
        skipped_on_wire["skip_reason"].as_str().unwrap_or_default().contains("\"b\""),
        "the reason reaches the client verbatim — the trace's whole claim is that it says \
         WHY, and a reason that only exists in a column is not one: {skipped_on_wire}"
    );

    // A run whose steps are all attributed to a node, so the canvas has something to key
    // on. A step with a null node is not an error — a rule defined before the builder has
    // none — but it must be *absent* rather than an empty string, or a client that keys on
    // the value will paint one card with a status belonging to no node.
    assert!(
        steps
            .iter()
            .all(|step| step["node_id"].is_null() || step["node_id"].is_string()),
        "node_id is either absent or a string, never a number: {steps:?}"
    );

    fixture.cleanup().await;
}

// ------------------------------------------------------------------------------------------
// Criterion 3 — "Retry this node" re-runs only that node (REQ-004 slice 3)
// ------------------------------------------------------------------------------------------

/// A minimal SMTP server that records what it was sent.
///
/// It exists because the criterion names the proof: *"without duplicating earlier side
/// effects (proven with the mail sink)"*. A status column cannot see a duplicated
/// e-mail — a run whose first step re-runs looks **identical** to one that did not, and the
/// only difference is a message that left the process. So the assertion that matters is a
/// count of messages, not a comparison of statuses.
///
/// The greeting is sent before anything is read. An SMTP client that connects and waits
/// for `220` before it says anything hangs on a sink that only replies to commands, and the
/// hang is indistinguishable from "the mail server is down".
struct MailSink {
    port: u16,
    received: Arc<Mutex<Vec<String>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl MailSink {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mail sink must bind");
        let port = listener.local_addr().expect("an address").port();
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = received.clone();

        let handle = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let sink = sink.clone();
                tokio::spawn(async move {
                    let _ = serve_one(stream, sink).await;
                });
            }
        });

        Self {
            port,
            received,
            handle,
        }
    }

    /// How many messages carried `needle` in their body or headers.
    async fn count(&self, needle: &str) -> usize {
        self.received
            .lock()
            .await
            .iter()
            .filter(|message| message.contains(needle))
            .count()
    }
}

impl Drop for MailSink {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn serve_one(stream: TcpStream, sink: Arc<Mutex<Vec<String>>>) -> std::io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    writer
        .write_all(b"220 omnion test sink ready\r\n")
        .await?;

    let mut body = String::new();
    let mut envelope = String::new();
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        let command = line.trim_end().to_owned();
        let upper = command.to_ascii_uppercase();

        if upper.starts_with("EHLO") {
            writer
                .write_all(b"250-sink\r\n250-SIZE 102400\r\n250 AUTH PLAIN\r\n")
                .await?;
        } else if upper.starts_with("AUTH") {
            writer.write_all(b"235 authenticated\r\n").await?;
        } else if upper.starts_with("MAIL FROM") || upper.starts_with("RCPT TO") {
            envelope.push_str(&command);
            envelope.push('\n');
            writer.write_all(b"250 ok\r\n").await?;
        } else if upper == "DATA" {
            writer.write_all(b"354 go ahead\r\n").await?;
            loop {
                line.clear();
                if reader.read_line(&mut line).await? == 0 {
                    break;
                }
                if line == ".\r\n" {
                    break;
                }
                body.push_str(&line);
            }
            writer.write_all(b"250 queued\r\n").await?;
        } else if upper == "QUIT" {
            writer.write_all(b"221 bye\r\n").await?;
            break;
        }
    }

    // The envelope is stored with the body so a test can assert on the recipient rather
    // than on a subject line it also has to choose.
    sink.lock().await.push(format!("{envelope}{body}"));
    Ok(())
}

/// Tick the engine with a real action handler, the way a process with host actions does.
///
/// `drive_until_settled` in this file ticks with the **no-op** handler, which is right for
/// the synthetic steps the other walks use and wrong for this one: a `send_email` step run
/// against `NoActionHandler` reports a host action it cannot do, so the step fails for a
/// reason that has nothing to do with retrying — and the criterion's mail count would be
/// zero before the retry ever happened, which is a test that passes for the wrong reason.
async fn drive_until_settled_with(
    state: &AppState,
    runner: &RunnerConfig,
    actions: &AutomationActions,
    execution_id: Uuid,
) -> RunState {
    for _ in 0..400 {
        engine::tick_with(
            state.db().pool(),
            runner,
            actions,
            &omnion_workflows::NoRunGuard,
        )
        .await
        .expect("a tick must run");

        let current = run_state(state, execution_id).await;
        if current.status != "running" {
            return current;
        }
        tokio::time::sleep(StdDuration::from_millis(20)).await;
    }

    panic!(
        "the run never settled; last state: {:#?}",
        run_state(state, execution_id).await
    );
}

#[tokio::test]
async fn retry_this_node_reruns_one_step_and_sends_no_second_email() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    // The fixture holds the walk lock itself for its whole lifetime (see `Fixture::new`),
    // so there is no second acquisition here: taking it again would deadlock the walk
    // against itself, which is a hang with no message.
    let token = login(&fixture.state, &fixture.operator_email).await;
    let runner = test_runner();

    let sink = MailSink::start().await;
    let actions = AutomationActions::new(
        fixture.state.db().pool().clone(),
        MailSettings::new("127.0.0.1", sink.port, "omnion@localhost"),
    );

    // trigger → notify (sends mail) → boom (fails) → settle (records the run)
    //
    // The three-node spine is the shape the criterion needs and no shorter one proves it:
    // a side effect BEFORE the failing node, and a step AFTER it. Retrying the middle node
    // must not re-send the first one's mail (the criterion) and must not silently re-run
    // the third (which is the difference between this and a tail re-run).
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/workflows",
            Some(&token),
            Some(json!({
                "organization_id": fixture.organization_a,
                "name": "Retry this node",
                "trigger": { "kind": "manual" },
                "steps": [ { "name": "prepare", "kind": "task", "action": "noop" } ]
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let workflow_id = created.body["id"]
        .as_str()
        .expect("a created rule has an id")
        .to_owned();

    let version = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{workflow_id}/graph"),
            Some(&token),
            None,
        ),
    )
    .await
    .body["graph_version"]
        .clone();

    let spine = json!({
        "nodes": [
            { "id": "trigger", "type": "trigger.manual", "label": "Trigger",
              "params": { "kind": "manual" }, "position": { "x": 0, "y": 0 } },
            { "id": "notify", "type": "action", "label": "Tell the editor",
              "params": { "action": "send_email",
                          "parameters": { "to": "editor@example.com",
                                          "subject": "Published", "body": "once" } },
              "position": { "x": 1, "y": 0 } },
            { "id": "boom", "type": "action", "label": "Upstream is down",
              "params": { "action": "fail",
                          "parameters": { "message": "the upstream is down" } },
              "position": { "x": 2, "y": 0 } },
            { "id": "settle", "type": "action", "label": "Record",
              "params": { "action": "noop", "parameters": {} },
              "position": { "x": 3, "y": 0 } },
            { "id": "end", "type": "end", "label": "End", "params": {},
              "position": { "x": 4, "y": 0 } }
        ],
        "edges": [
            { "id": "e1", "source": "trigger", "source_port": "out", "target": "notify" },
            { "id": "e2", "source": "notify", "source_port": "success", "target": "boom" },
            { "id": "e3", "source": "boom", "source_port": "success", "target": "settle" },
            { "id": "e4", "source": "settle", "source_port": "success", "target": "end" }
        ]
    });

    let saved = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/workflows/{workflow_id}/graph"),
            Some(&token),
            Some(json!({ "graph": spine, "graph_version": version })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "the spine was refused: {}", saved.body);

    // Run it once: the mail goes out, the second step fails, the run stops there.
    let started = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflows/{workflow_id}/run"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(started.status, StatusCode::ACCEPTED, "{}", started.body);
    let execution_id =
        Uuid::parse_str(started.body["id"].as_str().expect("a run has an id")).expect("a uuid");

    let first = drive_until_settled_with(&fixture.state, &runner, &actions, execution_id).await;
    assert_eq!(first.status, "failed", "the second step was meant to fail: {first:?}");
    assert_eq!(
        sink.count("editor@example.com").await,
        1,
        "exactly one message, before the retry"
    );

    // The node click. `boom` is the failed one; `notify` succeeded and is exactly the node
    // whose side effect must not be duplicated.
    let retried = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflow-executions/{execution_id}/retry-node"),
            Some(&token),
            Some(json!({ "node_id": "boom" })),
        ),
    )
    .await;
    assert_eq!(
        retried.status,
        StatusCode::OK,
        "Retry this node was refused: {}",
        retried.body
    );

    // **One row.** The response's own count is the assertion, because it is the only value
    // that notices a `WHERE` clause widened back to `step_no >= N` — which would re-send the
    // e-mail below, but only *after* this line had already passed.
    assert_eq!(
        retried.body["requeued"], 1,
        "a node retry re-opens exactly one step: {}",
        retried.body
    );
    assert_eq!(retried.body["node_id"], "boom", "{}", retried.body);

    // **The stored rows**, read out of PostgreSQL: the step that went back on the queue is
    // the one that failed, and its neighbours are exactly as they were.
    let after: Vec<(i32, String)> = sqlx::query_as(
        "select step_no, status from workflow_steps where execution_id = $1::uuid \
         order by step_no asc",
    )
    .bind(execution_id)
    .fetch_all(fixture.state.db().pool())
    .await
    .expect("the run's steps are readable");
    let status_of = |no: i32| {
        after
            .iter()
            .find(|(step_no, _)| *step_no == no)
            .map(|(_, status)| status.clone())
    };

    assert_eq!(
        status_of(1).as_deref(),
        Some("succeeded"),
        "the step that sent the mail must be left alone: {after:?}"
    );
    assert_eq!(
        status_of(2).as_deref(),
        Some("pending"),
        "the failed node is the one that goes back on the queue: {after:?}"
    );
    // **The tail is left exactly as the stop policy closed it.** This is the difference from
    // a tail re-run and it is worth being precise about which direction it points: a tail
    // re-run would have re-opened steps 3 and 4 to `pending` and run them again, while this
    // re-opened one row. The engine then claims step 2, and if it succeeds the run settles
    // without ever touching the cancelled tail — which is the whole promise of "this node,
    // on its own".
    assert_eq!(
        status_of(3).as_deref(),
        Some("cancelled"),
        "the tail must stay closed: a tail re-run would have re-opened it: {after:?}"
    );
    assert_eq!(status_of(4).as_deref(), Some("cancelled"), "{after:?}");

    // The attempts counter is zeroed, and **this assertion is the second thing the walk
    // corrected**. The first draft reasoned that re-running one node is not a new budget
    // and left the counter alone — and the database refused the write outright:
    // `workflow_steps_attempts_shape` caps `attempts` at `max_attempts`, and
    // `claim_due_step` increments on claim, so a re-queued step that had spent its budget
    // produces a row the engine is forbidden to claim. The retry would have been accepted,
    // audited, and then never run. The counter is therefore read BEFORE the retry, and the
    // step is given `max_attempts` enough headroom to show the reset is real rather than
    // incidental.
    let reset: i32 = sqlx::query_scalar(
        "select attempts from workflow_steps where execution_id = $1::uuid and step_no = 2",
    )
    .bind(execution_id)
    .fetch_one(fixture.state.db().pool())
    .await
    .expect("the retried step is readable");
    assert_eq!(
        reset, 0,
        "the re-queued step spends no attempt until it is claimed again, which is what lets \
         the engine claim it at all: `workflow_steps_attempts_shape` caps attempts at \
         max_attempts and the claim increments"
    );

    // **The proof the criterion names.** Run it out: the failed node fails again (the same
    // definition fails the same way) and the mail count must not move.
    let second = drive_until_settled_with(&fixture.state, &runner, &actions, execution_id).await;
    assert_eq!(second.status, "failed", "it fails again: {second:?}");
    assert_eq!(
        sink.count("editor@example.com").await,
        1,
        "THE CLAUSE: the earlier e-mail was NOT sent a second time by retrying one node. \
         A tail re-run would make this 2."
    );

    // A node that did not fail is refused by name, and the refusal is a **different** one
    // from a node the run never reached: both are "nothing to do", and the two sentences
    // are the only thing that tells an operator which of the two happened.
    let succeeded = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflow-executions/{execution_id}/retry-node"),
            Some(&token),
            Some(json!({ "node_id": "notify" })),
        ),
    )
    .await;
    assert_eq!(
        succeeded.status,
        StatusCode::BAD_REQUEST,
        "a node that succeeded has nothing to retry: {}",
        succeeded.body
    );
    assert_eq!(
        succeeded.body["error"]["code"], "nothing_to_retry",
        "{}",
        succeeded.body
    );

    let absent = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflow-executions/{execution_id}/retry-node"),
            Some(&token),
            Some(json!({ "node_id": "island" })),
        ),
    )
    .await;
    assert_eq!(
        absent.status,
        StatusCode::BAD_REQUEST,
        "a node with no step is refused too: {}",
        absent.body
    );
    assert_eq!(
        absent.body["error"]["code"], "node_not_in_run",
        "a node the run never touched is a different answer from one that succeeded: {}",
        absent.body
    );

    // The two refusals reach the client with sentences, because a refusal that only refuses
    // teaches an operator nothing about what to do next.
    let message = absent.body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        !message.is_empty(),
        "a refusal must carry a message: {}",
        absent.body
    );

    // The audit trail names the node, so "who re-ran that" is answerable months later.
    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where action = 'workflow.retry.node' \
         and organization_id = $1",
    )
    .bind(fixture.organization_a)
    .fetch_one(fixture.state.db().pool())
    .await
    .expect("the audit log is readable");
    assert_eq!(audited, 1, "the retry left exactly one audit row");

    fixture.cleanup().await;
}

/// *Listen for a real event* (REQ-004 slice 3, criterion 5): a real bus event lands in the
/// inspector within one matcher tick, and a listener that nobody answers expires leaving no
/// live row behind.
///
/// The criterion is two claims and the walk is shaped to make each of them **fail loudly** if
/// it is not true:
///
/// * *"within one matcher tick"* is asserted as exactly one `matcher::drain` between the arm
///   and the capture. A second drain would still pass a `captured_at` check, and a listener
///   that needed two ticks would be a broken one.
/// * *"leaving no stray token"* is asserted against the **matcher's own predicate**, not
///   against the row count: `prune` deletes expired rows, but a matcher that filters on
///   `consumed_at` alone would happily fill a dead row, and only a check that asks "would a
///   live-listener query still see this?" distinguishes the two. That is why the expired
///   case drives a *real event* through the matcher and asserts nothing was captured.
#[tokio::test]
async fn listen_for_a_real_event_captures_once_then_expires() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let token = login(&fixture.state, &fixture.operator_email).await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/workflows",
            Some(&token),
            Some(json!({
                "organization_id": fixture.organization_a,
                "name": "Listen for a real event",
                "trigger": { "kind": "event", "event": "page.published" },
                "steps": [ { "name": "record", "kind": "task", "action": "noop" } ]
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let workflow_id = created.body["id"]
        .as_str()
        .expect("a created rule has an id")
        .to_owned();

    // ---------------------------------------------------------------------------------
    // The refusals come first, because they are the ones that cost an author 15 minutes
    // ---------------------------------------------------------------------------------
    let no_node = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflows/{workflow_id}/listen"),
            Some(&token),
            Some(json!({ "node_id": "" })),
        ),
    )
    .await;
    assert_eq!(
        no_node.status,
        StatusCode::BAD_REQUEST,
        "a listener with no node is refused: {}",
        no_node.body
    );
    assert_eq!(no_node.body["error"]["code"], "node_required");

    let unknown = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflows/{workflow_id}/listen"),
            Some(&token),
            Some(json!({ "node_id": "not-a-node" })),
        ),
    )
    .await;
    assert_eq!(
        unknown.status,
        StatusCode::BAD_REQUEST,
        "a node that is not on the canvas is refused rather than armed for ever: {}",
        unknown.body
    );
    assert_eq!(unknown.body["error"]["code"], "unknown_node");

    // ---------------------------------------------------------------------------------
    // Arm, on the rule's real trigger node
    // ---------------------------------------------------------------------------------
    let trigger_node = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{workflow_id}/graph"),
            Some(&token),
            None,
        ),
    )
    .await
    .body["graph"]["nodes"][0]["id"]
        .as_str()
        .expect("a rule is born with a trigger node")
        .to_owned();

    let armed = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflows/{workflow_id}/listen"),
            Some(&token),
            Some(json!({ "node_id": trigger_node })),
        ),
    )
    .await;
    assert_eq!(
        armed.status,
        StatusCode::CREATED,
        "arming was refused: {}",
        armed.body
    );
    assert_eq!(armed.body["listener"]["status"], "armed", "{}", armed.body);
    // The window is REQ-004's fifteen minutes, asserted rather than assumed — a constant
    // that drifts to 5 or 60 is still a "listener" and still passes every other line here.
    //
    // Asserted on the **gap between the two timestamps**, not on the countdown. The
    // countdown is `whole_seconds()` of a window that began microseconds before the read, so
    // it is 899 by the time the response lands; pinning it to 900 would be asserting a
    // rounding rule rather than the window, and the gap is the thing the criterion names.
    let armed_at = OffsetDateTime::parse(
        armed.body["listener"]["armed_at"]
            .as_str()
            .expect("an arming time"),
        &time::format_description::well_known::Rfc3339,
    )
    .expect("the arming time is rfc3339, as every timestamp on this API is");
    let expires_at = OffsetDateTime::parse(
        armed.body["listener"]["expires_at"]
            .as_str()
            .expect("an expiry"),
        &time::format_description::well_known::Rfc3339,
    )
    .expect("the expiry is rfc3339");
    assert_eq!(
        expires_at - armed_at,
        time::Duration::minutes(15),
        "the criterion names fifteen minutes: {}",
        armed.body
    );
    // And the countdown the panel draws is the same window, never negative and never longer.
    let countdown = armed.body["expires_in_seconds"].as_i64().unwrap_or(-1);
    assert!(
        (899..=900).contains(&countdown),
        "the countdown is the same window, within a second of rounding: {countdown}"
    );
    let token_handle = armed.body["token"]
        .as_str()
        .expect("arming answers with the token, once")
        .to_owned();

    // The event name is what the author is told it is waiting for, and it is the trigger's
    // own `params.event` — not the node's label, which is what an author typed on a card.
    assert_eq!(
        armed.body["listener"]["event_name"], "page.published",
        "the armed row says out loud what it waits for: {}",
        armed.body
    );

    // The token is never stored in the clear. This is the check that makes "leaving no
    // stray token" mean something: a dump of the table hands over no armed listener.
    let stored: Vec<String> = sqlx::query_scalar(
        "select token_hash from workflow_test_listeners where workflow_id = $1::uuid",
    )
    .bind(&workflow_id)
    .fetch_all(fixture.state.db().pool())
    .await
    .expect("the listener table is readable");
    assert_eq!(stored.len(), 1, "one armed listener");
    assert!(
        !stored[0].contains(&token_handle),
        "only the hash is stored, never the cleartext token"
    );

    // ---------------------------------------------------------------------------------
    // One matcher tick, one real bus event → captured
    // ---------------------------------------------------------------------------------
    let event_id = omnion_events::bus::emit(
        fixture.state.db().pool(),
        omnion_events::model::NewEvent::new("page.published")
            .organization(fixture.organization_a)
            .actor(fixture.accounts[0])
            .payload(json!({ "page_id": "a-real-page", "slug": "release-notes" })),
    )
    .await
    .expect("the bus must record the event")
    .event
    .id;

    let report = omnion_automation::matcher::drain(fixture.state.db().pool(), 100)
        .await
        .expect("the matcher must run");
    assert!(
        report.evaluated >= 1 && report.matched >= 1,
        "the rule must have matched the real event: {report:?}"
    );

    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{workflow_id}/listeners"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.body);
    assert_eq!(read.body["armed"], 0, "the listener is spent: {}", read.body);
    let captured = read.body["captured"]
        .as_object()
        .expect("the read answers with the newest capture, so the panel need not pick it");
    assert_eq!(captured["status"], "captured", "{captured:?}");
    assert_eq!(
        captured["event_id"], event_id,
        "the capture names the bus event that filled it: {captured:?}"
    );
    // The payload is what the author could not otherwise know — this is the whole point of
    // the feature, so it is asserted rather than left to the panel.
    assert_eq!(
        captured["payload"]["slug"], "release-notes",
        "the captured payload is the real one: {captured:?}"
    );
    assert!(
        captured["payload_text"]
            .as_str()
            .is_some_and(|text| text.contains("release-notes")),
        "the inspector has a readable rendering: {captured:?}"
    );

    // The token reads its own row back, which is what a caller scripts against.
    let by_token = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{workflow_id}/listeners/{token_handle}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(by_token.status, StatusCode::OK, "{}", by_token.body);
    assert_eq!(by_token.body["status"], "captured");

    // One shot: a second event does not produce a second capture, and does not resurrect the
    // spent row.
    omnion_events::bus::emit(
        fixture.state.db().pool(),
        omnion_events::model::NewEvent::new("page.published")
            .organization(fixture.organization_a)
            .payload(json!({ "page_id": "a-later-page" })),
    )
    .await
    .expect("the bus must record the second event");
    omnion_automation::matcher::drain(fixture.state.db().pool(), 100)
        .await
        .expect("the second matcher tick must run");

    let after = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{workflow_id}/listeners"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(
        after.body["listeners"]
            .as_array()
            .map(Vec::len)
            .unwrap_or_default(),
        1,
        "a second event must not open a second row: {}",
        after.body
    );
    assert_eq!(
        after.body["captured"]["payload"]["page_id"], "a-real-page",
        "the first capture is the one that stands: {}",
        after.body
    );

    // ---------------------------------------------------------------------------------
    // The expiry half, against the matcher's own predicate
    // ---------------------------------------------------------------------------------
    let expired_armed = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflows/{workflow_id}/listen"),
            Some(&token),
            Some(json!({ "node_id": trigger_node })),
        ),
    )
    .await;
    assert_eq!(expired_armed.status, StatusCode::CREATED, "{}", expired_armed.body);
    let listener_id = Uuid::parse_str(expired_armed.body["listener"]["id"].as_str().expect("id"))
        .expect("a listener id is a uuid");

    // Close the window by hand. Waiting fifteen minutes is not a thing a walk may do, and
    // the claim under test is the *predicate* the matcher uses, not the clock.
    //
    // **`created_at` moves with it**, and that is the migration's own constraint talking:
    // `workflow_test_listeners_expiry_after_creation` refuses a row that expires before it
    // was armed, which is a rule the schema holds against every writer — a sweep that
    // back-dated `expires_at` alone would hit the same wall, so moving both columns is what
    // a clock crossing the boundary actually looks like to the database.
    sqlx::query(
        "update workflow_test_listeners \
           set created_at = now() - interval '16 minutes', \
               expires_at = now() - interval '1 minute' \
         where id = $1",
    )
    .bind(listener_id)
    .execute(fixture.state.db().pool())
    .await
    .expect("the listener row is writable");

    // A real event goes through the real matcher. If the matcher's UPDATE omitted
    // `expires_at > now()`, this fills a dead row and the panel reports a capture for an
    // event nobody was watching — the exact failure the criterion's second clause names.
    omnion_events::bus::emit(
        fixture.state.db().pool(),
        omnion_events::model::NewEvent::new("page.published")
            .organization(fixture.organization_a)
            .payload(json!({ "page_id": "after-expiry" })),
    )
    .await
    .expect("the bus must record the third event");
    omnion_automation::matcher::drain(fixture.state.db().pool(), 100)
        .await
        .expect("the third matcher tick must run");

    let expired_read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{workflow_id}/listeners"),
            Some(&token),
            None,
        ),
    )
    .await;
    let expired_row = expired_read.body["listeners"]
        .as_array()
        .expect("an array of listeners")
        .iter()
        .find(|row| row["id"] == listener_id.to_string())
        .expect("the expired row is still listed — a row that vanished is indistinguishable \
                  from one that was never armed");
    assert_eq!(
        expired_row["status"], "expired",
        "an expired listener says so instead of vanishing: {expired_row:?}"
    );
    assert!(
        expired_row["payload"].is_null(),
        "an expired listener captures nothing: {expired_row:?}"
    );
    // The one place a "no stray token" claim is actually proven: the live-listener query the
    // matcher runs must not see this row, which is the definition of "not armed".
    let live: i64 = sqlx::query_scalar(
        "select count(*) from workflow_test_listeners          where workflow_id = $1::uuid and consumed_at is null and expires_at > now()",
    )
    .bind(&workflow_id)
    .fetch_one(fixture.state.db().pool())
    .await
    .expect("the listener table is readable");
    assert_eq!(live, 0, "an expired listener leaves no live token behind");

    // And the panel's own countdown is closed: a negative "expires in −94s" is a number only
    // a clock comparison produces.
    assert_eq!(
        expired_row["expires_in_seconds"], 0,
        "the countdown never goes negative: {expired_row:?}"
    );

    // Arming is `workflows.run` and reading is `workflows.read` — the REQ's own split, and
    // it is asserted from *both* sides because the guard is the only thing standing between
    // "may start this rule" and "may leave it listening for an event nobody is watching".
    //
    // The member of the fixture holds neither key, so both surfaces answer 403. The operator
    // holds both, which the rest of this walk already proved.
    let member_token = login(&fixture.state, &fixture.member_email).await;
    let member_read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{workflow_id}/listeners"),
            Some(&member_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        member_read.status,
        StatusCode::FORBIDDEN,
        "reading a rule's listeners needs workflows.read: {}",
        member_read.body
    );

    let member_arm = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflows/{workflow_id}/listen"),
            Some(&member_token),
            Some(json!({ "node_id": trigger_node })),
        ),
    )
    .await;
    assert_eq!(
        member_arm.status,
        StatusCode::FORBIDDEN,
        "arming needs workflows.run — it is the first half of running the rule for real: {}",
        member_arm.body
    );

    // A *different* tenant's rule is not reachable with a valid token either: the guard passes
    // and the scope check refuses. This is the check that stops a listener from being armed
    // on somebody else's rule by guessing an id.
    //
    // The rule is created by the **platform owner**, because the organization-A operator
    // cannot create a rule in organization B at all — it is refused at creation with
    // `cross_organization`. The owner holds the keys across tenants, which is exactly the
    // power that must then be *withheld* from the operator when it comes to arming.
    let platform_token = login(&fixture.state, &fixture.platform_email).await;
    let other_rule = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/workflows",
            Some(&platform_token),
            Some(json!({
                "organization_id": fixture.organization_b,
                "name": "Another tenant's rule",
                "trigger": { "kind": "event", "event": "page.published" },
                "steps": [ { "name": "record", "kind": "task", "action": "noop" } ]
            })),
        ),
    )
    .await;
    assert_eq!(other_rule.status, StatusCode::CREATED, "{}", other_rule.body);
    let other_id = other_rule.body["id"].as_str().expect("an id").to_owned();

    //
    // It answers `403 cross_organization` and not `404`, which is the platform's standing
    // decision for every scoped surface (the scope check runs *after* the rule is found, so
    // a 404 here would mean "no such rule" — a claim about the id space this API does not
    // make). What matters for this criterion is that it is **not** a success and not a 200.
    let across = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/workflows/{other_id}/listen"),
            Some(&token),
            Some(json!({ "node_id": "trigger" })),
        ),
    )
    .await;
    assert_eq!(
        across.status,
        StatusCode::FORBIDDEN,
        "another tenant's rule cannot be armed from here: {}",
        across.body
    );
    assert_eq!(across.body["error"]["code"], "cross_organization");
    let across_read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/workflows/{other_id}/listeners"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(
        across_read.status,
        StatusCode::FORBIDDEN,
        "and its listeners are not readable either: {}",
        across_read.body
    );

    // The refusal left nothing behind in that tenant — a cross-tenant arm that "failed" but
    // wrote a row is the failure this whole paragraph exists to prevent.
    let leaked: i64 = sqlx::query_scalar(
        "select count(*) from workflow_test_listeners where workflow_id = $1::uuid",
    )
    .bind(&other_id)
    .fetch_one(fixture.state.db().pool())
    .await
    .expect("the listener table is readable");
    assert_eq!(leaked, 0, "a refused arm must leave no row behind");

    // The audit trail names the arming, so "who left this rule listening" is answerable.
    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where action = 'workflow.listener_armed' \
         and organization_id = $1",
    )
    .bind(fixture.organization_a)
    .fetch_one(fixture.state.db().pool())
    .await
    .expect("the audit log is readable");
    assert_eq!(audited, 2, "each arming leaves exactly one audit row");

    fixture.cleanup().await;
}
