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

use std::time::Duration as StdDuration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
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
use time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// Permission keys the workflow operator of this suite holds.
const WORKFLOW_PERMISSIONS: [&str; 3] = ["workflows.read", "workflows.manage", "workflows.run"];

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
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
    response
        .set_cookie
        .clone()
        .expect("login must set the session cookie")
        .split(';')
        .next()
        .expect("cookie has a value")
        .split_once('=')
        .expect("cookie is name=value")
        .1
        .to_owned()
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
            "unknown action",
            json!([{ "name": "prepare", "kind": "task", "action": "http_request" }]),
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
        next_run_at: None,
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
    };
    assert!(!execution.is_terminal());
}
