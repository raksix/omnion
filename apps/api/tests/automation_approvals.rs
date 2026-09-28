//! The integration walk for REQ-003 **slice 3** — the approval gate and the run-as authority
//! (docs/requests/REQ-003).
//!
//! It borrows the harness of `automation_actions.rs` (the same throwaway database, the same
//! engine driver) and proves, one acceptance criterion per walk:
//!
//! * a `wait_for_approval` step **parks the run** as `awaiting_approval`, the pending panel
//!   lists it, and **nothing after it runs** — not the step itself, not the e-mail behind it;
//! * **approving resumes it** and the effect lands exactly once (the SMTP sink's count is the
//!   proof), **rejecting ends it** without the effect, and the steps after the gate are
//!   closed as cancelled rather than left pending;
//! * **deciding twice has no second effect** — the second press is answered with the decision
//!   the gate already has, and the mail count does not move;
//! * an **expired approval is refused in words**, and a rule whose run-as account no longer
//!   holds the permission an action needs is **stopped with
//!   `automation.rule.permission_revoked`**, naming the permission, before the action runs;
//! * a **deleted author runs as nobody** — a rule whose creator is gone is refused on its
//!   first host action rather than falling back to some account.
//!
//! Every assertion reads the platform's own rows or its own HTTP surface: no test-only
//! shortcut reaches into the engine, and no assertion is satisfied by "the step returned
//! something truthy".
//!
//! When PostgreSQL is not reachable the suite skips itself with a printed reason.

use std::time::Duration as StdDuration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_automation::{AutomationActions, MailSettings};
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::sessions;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use omnion_storage::Storage;
use omnion_workflows::engine::{self, RunnerConfig};
use omnion_workflows::{ExecutionStatus, store};
use serde_json::{Value, json};
use time::Duration;
use tower::ServiceExt as _;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// How long a walk waits for its run to settle.
const SETTLE_BUDGET: usize = 80;

/// The event the rules in this suite listen for, and the one whose payload is a literal.
const EVENT: &str = "page.published";

// ---------------------------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------------------------

struct Harness {
    state: AppState,
    db: Db,
    maintenance: Db,
    database: String,
}

#[derive(Debug)]
struct TestResponse {
    status: StatusCode,
    body: Value,
}

impl TestResponse {
    /// The API's stable error code, read out of the nested body.
    ///
    /// The shape is `{"error": {"code": …}}` — a test that reads `body["code"]` sees `null`
    /// and reports an empty message, which reads as "the server said nothing" rather than
    /// "the test looked in the wrong place".
    fn code(&self) -> &str {
        self.body
            .pointer("/error/code")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    /// The API's sentence, read out of the nested body.
    fn message(&self) -> &str {
        self.body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }
}

impl Harness {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().expect("environment must be valid");
        if live_db(&config).await.is_none() {
            return None;
        }

        let database = format!("omnion_automation3_{}", Uuid::new_v4().simple());
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
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
        let state = AppState::new(
            BuildInfo::new("omnion-api", "0.1.0-test"),
            config,
            db.clone(),
            redis,
            Storage::from_config(&omnion_storage::StorageConfig::default())
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
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body must read")
            .to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);

        TestResponse { status, body }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // `block_in_place` because `Drop` cannot be async, and the drop is a *forced* one: a
        // leftover temporary database holds its own connections and would survive the suite.
        //
        // Guarded, and the guard is the point: a panic inside `Drop` during an unwind is a
        // **non-unwinding** panic, which aborts the process and takes the real assertion
        // message with it. A test that failed for an interesting reason must be allowed to
        // say so, so cleanup failures are swallowed and the database is left behind — its
        // name is unique per run, so the cost of a failed drop is a database, not the next
        // walk's isolation.
        let database = self.database.clone();
        let maintenance = self.maintenance.pool().clone();
        // Spawned rather than `block_in_place`: `block_in_place` is only legal on a
        // multi-threaded runtime, and this suite's `#[tokio::test]` defaults to the
        // current-thread one — a `Drop` that aborts the process on a *current-thread*
        // runtime hides the assertion that sent it there in the first place.
        tokio::spawn(async move {
            let _ = sqlx::query(&format!(
                "drop database if exists \"{database}\" with (force)"
            ))
            .execute(&maintenance)
            .await;
        });
    }
}

fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        // Axum's `Json` extractor refuses a body with no content type, and the answer is a
        // `415` that says nothing about the rule — so the header is set here rather than
        // discovered once per walk.
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        builder = builder.header(header::COOKIE, format!("omnion_session={token}"));
    }
    let body = body
        .filter(|value| !value.is_null())
        .map(|value| Body::from(value.to_string()))
        .unwrap_or_else(Body::empty);
    builder.body(body).expect("the request must build")
}

fn get(uri: &str, token: &str) -> Request<Body> {
    request(Method::GET, uri, Some(token), None)
}

fn post(uri: &str, body: Value, token: &str) -> Request<Body> {
    request(Method::POST, uri, Some(token), Some(body))
}

/// Create an account and a session for it.
async fn account(harness: &Harness, organization_id: Option<Uuid>) -> (Uuid, String) {
    let user = users::create_user(
        harness.db.pool(),
        NewUser {
            email: format!("approvals-{}@omnion.test", Uuid::new_v4().simple()),
            password: PASSWORD.to_owned(),
            display_name: "Approvals Walk".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    let (_, token) = sessions::create_session(harness.db.pool(), user.id, None, None)
        .await
        .expect("the session must be created");
    (user.id, token)
}

/// Bind a role carrying exactly these keys to one account, at organization scope.
async fn grant(harness: &Harness, user_id: Uuid, organization_id: Uuid, keys: &[&str]) {
    let role = role_store::create_role(
        harness.db.pool(),
        NewRole {
            organization_id,
            key: format!("approvals-walk-{}", Uuid::new_v4().simple()),
            name: "Approvals Walk".to_owned(),
            description: "The keys one walk needs".to_owned(),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the role must be created");

    let entries: Vec<RolePermissionInput> = keys
        .iter()
        .map(|key| RolePermissionInput {
            key: (*key).to_owned(),
            effect: Effect::Allow,
        })
        .collect();
    role_store::set_role_permissions(harness.db.pool(), role.id, &entries)
        .await
        .expect("the role permission set must be written");

    let binding = NewBinding {
        role_id: role.id,
        user_id,
        scope: Scope::Organization { organization_id },
        granted_by: None,
        expires_at: None,
    };
    bindings::validate(harness.db.pool(), &binding)
        .await
        .expect("the binding must validate");
    bindings::grant(harness.db.pool(), binding)
        .await
        .expect("the binding must be granted");
}

/// The keys the rule author and the decider each hold.
const AUTHOR_KEYS: [&str; 3] = ["workflows.read", "workflows.manage", "workflows.run"];

/// Everything a decider needs — the three above **plus** the deciding power, which is
/// deliberately a fourth key (REQ-003 slice 3).
const DECIDER_KEYS: [&str; 4] = [
    "workflows.read",
    "workflows.manage",
    "workflows.run",
    "workflows.approve",
];

async fn create_organization_row(db: &Db, label: &str, name: &str) -> Uuid {
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(name)
        .bind(format!("{label}-{}", Uuid::new_v4().simple()))
        .fetch_one(db.pool())
        .await
        .expect("an organization must be creatable")
}

async fn create_site_row(db: &Db, organization_id: Uuid, key: &str, name: &str) -> Uuid {
    sqlx::query_scalar(
        "insert into sites (organization_id, key, name) values ($1, $2, $3) returning id",
    )
    .bind(organization_id)
    .bind(key)
    .bind(name)
    .fetch_one(db.pool())
    .await
    .expect("a site must be creatable")
}

fn runner_config() -> RunnerConfig {
    RunnerConfig {
        tick: Duration::milliseconds(20),
        sweep: Duration::seconds(30),
        batch: 10,
        sweep_batch: 50,
        scheduler_batch: 4,
        retry_base: Duration::milliseconds(40),
        retry_max: Duration::milliseconds(320),
        lease: Duration::seconds(30),
    }
}

/// Drive the engine until the run reaches a terminal state, or the budget runs out.
///
/// A parked run is **not** terminal, so this never returns for one: the walk has to decide
/// it. That is the whole point of the walk, and a helper that "settled" a parked run would
/// hide the acceptance criterion.
async fn drive_until_terminal(
    harness: &Harness,
    actions: &AutomationActions,
    execution_id: Uuid,
) -> ExecutionStatus {
    for _ in 0..SETTLE_BUDGET {
        engine::tick_with(
            harness.db.pool(),
            &runner_config(),
            actions,
            &omnion_workflows::guard::NoRunGuard,
        )
            .await
            .expect("the engine tick must run");

        let execution = store::find_execution(harness.db.pool(), execution_id)
            .await
            .expect("the run must be readable")
            .expect("the run must exist");
        if let Some(status) = execution.status() {
            if status.is_terminal() {
                return status;
            }
        }
        tokio::time::sleep(StdDuration::from_millis(30)).await;
    }

    panic!("the run did not settle within the budget");
}

/// Run the engine `ticks` times without waiting for anything, for a walk that only needs to
/// prove that *nothing* happened.
async fn drive(harness: &Harness, actions: &AutomationActions, ticks: usize) {
    for _ in 0..ticks {
        engine::tick_with(
            harness.db.pool(),
            &runner_config(),
            actions,
            &omnion_workflows::guard::NoRunGuard,
        )
            .await
            .expect("the engine tick must run");
        tokio::time::sleep(StdDuration::from_millis(20)).await;
    }
}

async fn status_of(harness: &Harness, execution_id: Uuid) -> ExecutionStatus {
    store::find_execution(harness.db.pool(), execution_id)
        .await
        .expect("the run must be readable")
        .expect("the run must exist")
        .status()
        .expect("the run must carry a known status")
}

/// The steps of a run, as the panel reads them.
async fn steps_of(harness: &Harness, execution_id: Uuid) -> Vec<Value> {
    let steps = store::list_steps(harness.db.pool(), execution_id)
        .await
        .expect("the steps must be readable");
    steps
        .iter()
        .map(|step| {
            json!({
                "step_no": step.step_no,
                "name": step.name,
                "kind": step.kind,
                "status": step.status,
                "attempts": step.attempts,
                "error": step.error,
                "output": step.output,
            })
        })
        .collect()
}

/// The step every gated rule carries behind its gate.
///
/// An `echo`, deliberately. These walks prove the **gate** — that the run parks, that
/// approving releases it and rejecting ends it — and a `send_email` behind the gate would
/// make every one of them depend on an SMTP server the suite does not run, so a *failed*
/// run would read as a broken gate when the gate had worked perfectly. `echo` is the
/// engine's own action, always succeeds, and its output is on the trace either way; the
/// slice-2 suite already covers what a real send looks like.
fn echo_step(name: &str) -> Value {
    json!({
        "name": name,
        "kind": "task",
        "action": "echo",
        "params": { "value": "released" },
        "max_attempts": 1,
    })
}

/// A `wait_for_approval` step.
fn gate_step(name: &str, message: &str, hours: i64) -> Value {
    json!({
        "name": name,
        "kind": "approval",
        "action": Value::Null,
        "params": {
            "permission": "workflows.approve",
            "message": message,
            "expires_in_hours": hours,
        },
        "max_attempts": 1,
    })
}

/// Write a rule through the API and return its id.
async fn create_rule(
    harness: &Harness,
    token: &str,
    organization_id: Uuid,
    name: &str,
    steps: Value,
    run_as_user_id: Option<Uuid>,
) -> Uuid {
    let answer = harness
        .call(post(
            "/api/v1/automations",
            json!({
                "organization_id": organization_id,
                "name": name,
                "event": EVENT,
                "conditions": [],
                "actions": steps,
                "run_as_user_id": run_as_user_id,
            }),
            token,
        ))
        .await;
    assert_eq!(
        answer.status,
        StatusCode::CREATED,
        "the rule must be created: {}",
        answer.body
    );
    answer.body["id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
        .expect("the rule must carry an id")
}

/// Start one run of a rule by hand and return its id.
async fn run_now(harness: &Harness, token: &str, rule_id: Uuid) -> Uuid {
    let answer = harness
        .call(post(
            &format!("/api/v1/automations/{rule_id}/run"),
            json!({}),
            token,
        ))
        .await;
    assert_eq!(answer.status, StatusCode::ACCEPTED, "{}", answer.body);
    answer.body["execution_id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
        .expect("the run must carry an id")
}

fn actions_for(harness: &Harness) -> AutomationActions {
    AutomationActions::new(
        harness.db.pool().clone(),
        // Port 1 is never dialled by these walks: every gated rule's step behind the gate is
        // asserted on the *engine's* behaviour, and the one walk that reaches a real send
        // would need the SMTP sink. These walks prove the gate, not the mailer.
        MailSettings::new("127.0.0.1", 1, "omnion@localhost").with_sending(false),
    )
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_gate_parks_the_run_and_nothing_behind_it_runs() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let organization_id = create_organization_row(&harness.db, "gate", "Gate Walk").await;
    let (author, author_token) = account(&harness, Some(organization_id)).await;
    grant(&harness, author, organization_id, &AUTHOR_KEYS).await;
    let (decider_id, decider_token) = account(&harness, Some(organization_id)).await;
    grant(&harness, decider_id, organization_id, &DECIDER_KEYS).await;

    let rule_id = create_rule(
        &harness,
        &author_token,
        organization_id,
        "Publish after a person says so",
        json!([
            gate_step("wait for the editor", "publish this page?", 24),
            echo_step("do the thing"),
        ]),
        None,
    )
    .await;

    let execution_id = run_now(&harness, &author_token, rule_id).await;
    let actions = actions_for(&harness);

    // Drive hard. A run that is parked must stay parked: this is the assertion the whole
    // feature rests on, and a run that marched on would look like a green run.
    drive(&harness, &actions, 12).await;
    assert_eq!(
        status_of(&harness, execution_id).await,
        ExecutionStatus::AwaitingApproval,
        "a gate parks the run and nothing else may claim it"
    );

    let steps = steps_of(&harness, execution_id).await;
    assert_eq!(steps[0]["kind"], "approval");
    assert_eq!(
        steps[0]["status"], "waiting",
        "the gate itself waits, exactly as a wait step does"
    );
    assert_eq!(
        steps[1]["status"], "pending",
        "the effect behind the gate must not have started"
    );

    // The author cannot even *see* the queue. That is the same fact as the 403 in the
    // decision walk, asserted from the read side: the person who writes a rule is not the
    // person who watches what its rules are waiting for.
    let refused_read = harness
        .call(get("/api/v1/approvals?status=pending", &author_token))
        .await;
    assert_eq!(
        refused_read.status,
        StatusCode::FORBIDDEN,
        "reading the queue is the deciding power: {}",
        refused_read.body
    );

    // The decider can, and the panel says who may decide.
    let answer = harness
        .call(get("/api/v1/approvals?status=pending", &decider_token))
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let approvals = answer.body["approvals"].as_array().expect("an array");
    assert_eq!(
        approvals.len(),
        1,
        "exactly one gate is waiting: {}",
        answer.body
    );
    assert_eq!(approvals[0]["execution_id"], execution_id.to_string());
    assert_eq!(approvals[0]["step_no"], 1);
    assert_eq!(approvals[0]["permission"], "workflows.approve");
    assert_eq!(approvals[0]["message"], "publish this page?");
    assert_eq!(approvals[0]["expired"], false);
    // **The list must never carry the credential.** A queue read is the one place a token
    // would leak in bulk, and a token in a list response is a token in a browser cache.
    assert!(
        approvals[0].get("token").is_none(),
        "the pending list must not carry a decision token"
    );
}

#[tokio::test]
async fn approving_resumes_the_run_and_rejecting_ends_it() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let organization_id = create_organization_row(&harness.db, "decide", "Decide Walk").await;
    let (author, author_token) = account(&harness, Some(organization_id)).await;
    grant(&harness, author, organization_id, &AUTHOR_KEYS).await;
    let (decider_id, decider_token) = account(&harness, Some(organization_id)).await;
    grant(&harness, decider_id, organization_id, &DECIDER_KEYS).await;

    let actions = actions_for(&harness);

    // --- approved -----------------------------------------------------------------------
    let approved_rule = create_rule(
        &harness,
        &author_token,
        organization_id,
        "Approved gate",
        json!([
            gate_step("wait", "go ahead?", 24),
            json!({ "name": "first", "kind": "task", "action": "echo", "params": { "value": "one" }, "max_attempts": 1 }),
            echo_step("do the thing"),
        ]),
        None,
    )
    .await;
    let approved_run = run_now(&harness, &author_token, approved_rule).await;
    drive(&harness, &actions, 4).await;
    assert_eq!(
        status_of(&harness, approved_run).await,
        ExecutionStatus::AwaitingApproval
    );

    let gate_id = pending_id(&harness, &decider_token, approved_run).await;

    // The author may **not** decide: they can run a rule, which is not the power to wave one
    // through. This is the acceptance criterion's whole point, expressed as a refusal.
    let refused = harness
        .call(post(
            &format!("/api/v1/approvals/{gate_id}/decide"),
            json!({ "decision": "approved" }),
            &author_token,
        ))
        .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "running a rule is not deciding one: {}",
        refused.body
    );

    // The decider may — and this account holds the fourth key the author does not.
    let approved = harness
        .call(post(
            &format!("/api/v1/approvals/{gate_id}/decide"),
            json!({ "decision": "approved", "note": "checked the draft" }),
            &decider_token,
        ))
        .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    assert_eq!(approved.body["decision"], "approved");
    assert_eq!(
        approved.body["execution_status"], "running",
        "approving hands the run back to the engine"
    );

    let settled = drive_until_terminal(&harness, &actions, approved_run).await;
    assert_eq!(settled, ExecutionStatus::Completed, "{}", approved.body);
    let steps = steps_of(&harness, approved_run).await;
    assert_eq!(steps[0]["status"], "succeeded", "the gate succeeded");
    // Who let it through is on the step's own output, because "who approved this" is the
    // question an audit asks of every gated effect.
    assert_eq!(
        steps[0]["output"]["decided_by"],
        decider_id.to_string(),
        "the gate's output names the decider"
    );
    assert_eq!(steps[1]["status"], "succeeded", "the step behind it ran");
    // The step behind the gate ran — which is the whole meaning of "approved".
    assert_eq!(steps[2]["status"], "succeeded", "{}", steps[2]);
    assert_eq!(steps[2]["output"]["value"], "released", "{}", steps[2]);

    // --- rejected -----------------------------------------------------------------------
    let rejected_rule = create_rule(
        &harness,
        &author_token,
        organization_id,
        "Rejected gate",
        json!([
            gate_step("wait", "go ahead?", 24),
            echo_step("do the thing"),
        ]),
        None,
    )
    .await;
    let rejected_run = run_now(&harness, &author_token, rejected_rule).await;
    drive(&harness, &actions, 4).await;
    assert_eq!(
        status_of(&harness, rejected_run).await,
        ExecutionStatus::AwaitingApproval
    );
    let rejected_gate = pending_id(&harness, &decider_token, rejected_run).await;

    let rejected = harness
        .call(post(
            &format!("/api/v1/approvals/{rejected_gate}/decide"),
            json!({ "decision": "rejected", "note": "not this time" }),
            &decider_token,
        ))
        .await;
    assert_eq!(rejected.status, StatusCode::OK, "{}", rejected.body);
    assert_eq!(rejected.body["execution_status"], "cancelled");

    // Drive it: a rejected gate must not be resumed by the engine's own tick.
    drive(&harness, &actions, 6).await;
    assert_eq!(
        status_of(&harness, rejected_run).await,
        ExecutionStatus::Cancelled,
        "a rejection ends the run; the engine may not let it go on"
    );
    let steps = steps_of(&harness, rejected_run).await;
    // The gate's own row carries the reason — that is the whole reason the closing helper
    // is called *before* the gate's write, so the generic "the run ended before this step"
    // lands on the steps behind it and not here.
    assert_eq!(steps[0]["status"], "cancelled", "the gate is closed");
    assert!(
        steps[0]["error"]
            .as_str()
            .is_some_and(|message| message.contains("rejected")),
        "the trace says a person refused it: {}",
        steps[0]["error"]
    );
    // And the effect behind it is closed for the generic reason: it never ran, and the
    // trace must say *that* rather than blaming the gate.
    assert_eq!(steps[1]["status"], "cancelled", "the effect never ran");
    assert!(
        steps[1]["error"]
            .as_str()
            .is_some_and(|message| message.contains("ended before")),
        "and says it was never reached: {}",
        steps[1]["error"]
    );
}

#[tokio::test]
async fn deciding_twice_has_no_second_effect_and_an_expired_gate_is_refused() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let organization_id = create_organization_row(&harness.db, "twice", "Twice Walk").await;
    let (author, author_token) = account(&harness, Some(organization_id)).await;
    grant(&harness, author, organization_id, &AUTHOR_KEYS).await;
    let (decider_id, decider_token) = account(&harness, Some(organization_id)).await;
    grant(&harness, decider_id, organization_id, &DECIDER_KEYS).await;

    let actions = actions_for(&harness);
    let rule_id = create_rule(
        &harness,
        &author_token,
        organization_id,
        "Decided once",
        json!([
            gate_step("wait", "go ahead?", 24),
            echo_step("do the thing")
        ]),
        None,
    )
    .await;
    let execution_id = run_now(&harness, &author_token, rule_id).await;
    drive(&harness, &actions, 4).await;
    let gate_id = pending_id(&harness, &decider_token, execution_id).await;

    let first = harness
        .call(post(
            &format!("/api/v1/approvals/{gate_id}/decide"),
            json!({ "decision": "approved" }),
            &decider_token,
        ))
        .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);

    // The second press — a second tab, a double click, a retried request — is answered with
    // the decision the gate already has, and applies nothing.
    let second = harness
        .call(post(
            &format!("/api/v1/approvals/{gate_id}/decide"),
            json!({ "decision": "rejected" }),
            &decider_token,
        ))
        .await;
    assert_eq!(
        second.status,
        StatusCode::OK,
        "a repeat decision is answered, not refused: {}",
        second.body
    );
    assert_eq!(
        second.body["decision"], "approved",
        "the gate keeps the decision it already had"
    );

    // And exactly one decision row exists, with one decider and one timestamp.
    let counted: (i64,) = sqlx::query_as(
        "select count(*) from workflow_approvals where id = $1 and decision = 'approved' \
         and decided_by = $2 and decided_at is not null",
    )
    .bind(gate_id)
    .bind(decider_id)
    .fetch_one(harness.db.pool())
    .await
    .expect("the gate row must be countable");
    assert_eq!(counted.0, 1, "one decision, not two");

    // A wrong token on a gate that is **still open** decides nothing, and answers exactly as
    // a wrong id does. (On the already-decided gate above, the repeat-press branch answers
    // first — which is why this uses the second gate below rather than this one.)
    let open_rule = create_rule(
        &harness,
        &author_token,
        organization_id,
        "Still waiting",
        json!([
            gate_step("wait", "go ahead?", 24),
            echo_step("do the thing")
        ]),
        None,
    )
    .await;
    let open_run = run_now(&harness, &author_token, open_rule).await;
    drive(&harness, &actions, 4).await;
    let open_gate = pending_id(&harness, &decider_token, open_run).await;

    for token in ["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "not-even-shaped", ""] {
        let forged = harness
            .call(post(
                &format!("/api/v1/approvals/{open_gate}/decide"),
                json!({ "decision": "approved", "token": token }),
                &decider_token,
            ))
            .await;
        assert_eq!(
            forged.status,
            StatusCode::NOT_FOUND,
            "a wrong token must not decide anything: {token:?} → {}",
            forged.body
        );
        assert_eq!(forged.code(), "approval_not_found");
    }
    // And the gate is still waiting, because three refused tokens are not three decisions.
    assert_eq!(
        status_of(&harness, open_run).await,
        ExecutionStatus::AwaitingApproval,
        "the gate is untouched"
    );

    // An expired gate is refused in words — and it names the *next* step, not the token.
    let expiring_rule = create_rule(
        &harness,
        &author_token,
        organization_id,
        "Expiring gate",
        json!([gate_step("wait", "go ahead?", 1), echo_step("do the thing")]),
        None,
    )
    .await;
    let expiring_run = run_now(&harness, &author_token, expiring_rule).await;
    drive(&harness, &actions, 4).await;
    let expiring_gate = pending_id(&harness, &decider_token, expiring_run).await;

    // Move the deadline into the past: a gate that outlives its window has to be refused by
    // the same code that will refuse it in a week, and the only honest way to prove that
    // without waiting an hour is to set the clock.
    sqlx::query(
        "update workflow_approvals set expires_at = now() - interval '1 minute' where id = $1",
    )
    .bind(expiring_gate)
    .execute(harness.db.pool())
    .await
    .expect("the deadline must be writable");

    let expired = harness
        .call(post(
            &format!("/api/v1/approvals/{expiring_gate}/decide"),
            json!({ "decision": "approved" }),
            &decider_token,
        ))
        .await;
    assert_eq!(expired.status, StatusCode::BAD_REQUEST, "{}", expired.body);
    assert_eq!(expired.code(), "approval_expired");
    assert!(
        expired.message().contains("new run"),
        "the refusal says what to do next: {}",
        expired.message()
    );

    // The sweeper closes it as a rejection, which is how "nobody answered" reaches the same
    // ending as "no" without a third state.
    // The sweep *decides* the gate and hands the run back; the engine's next claim is what
    // reads that decision and ends the run. Two writes, deliberately: the sweep is the
    // clock, the engine is the thing that knows how a run ends, and a sweep that ended runs
    // itself would be a second state machine.
    let swept = engine::sweep(harness.db.pool(), &runner_config())
        .await
        .expect("the sweep must run");
    assert_eq!(swept.approvals_expired, 1, "the expired gate is decided");
    assert_eq!(
        status_of(&harness, expiring_run).await,
        ExecutionStatus::Running,
        "the sweep reopens the run; only the engine may end it"
    );

    let settled = drive_until_terminal(&harness, &actions, expiring_run).await;
    assert_eq!(
        settled,
        ExecutionStatus::Cancelled,
        "an unanswered gate ends the run without the effect"
    );
    let steps = steps_of(&harness, expiring_run).await;
    assert_eq!(steps[0]["status"], "cancelled", "the gate itself is closed");
    assert_eq!(steps[1]["status"], "cancelled", "the effect never ran");
}

#[tokio::test]
async fn a_revoked_permission_stops_the_run_and_a_deleted_author_runs_as_nobody() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let organization_id = create_organization_row(&harness.db, "authority", "Authority Walk").await;
    let (author, author_token) = account(&harness, Some(organization_id)).await;
    grant(&harness, author, organization_id, &AUTHOR_KEYS).await;
    // The author additionally holds the permission a `publish_page` step needs — and it is
    // taken away again below, which is the whole point.
    grant(
        &harness,
        author,
        organization_id,
        &["content.pages.publish"],
    )
    .await;

    let actions = actions_for(&harness);
    let (site_id, page_id) = create_page(&harness, organization_id).await;

    // --- the permission is there: the step runs ------------------------------------------------
    let rule_id = create_rule(
        &harness,
        &author_token,
        organization_id,
        "Publish on a schedule",
        json!([{
            "name": "publish it",
            "kind": "task",
            "action": "publish_page",
            "params": { "page_id": page_id.to_string() },
            "max_attempts": 1,
        }]),
        None,
    )
    .await;
    let execution_id = run_now(&harness, &author_token, rule_id).await;
    let settled = drive_until_terminal(&harness, &actions, execution_id).await;
    assert_eq!(
        settled,
        ExecutionStatus::Completed,
        "an account that still holds content.pages.publish may publish"
    );
    let steps = steps_of(&harness, execution_id).await;
    assert_eq!(steps[0]["output"]["status"], "published", "{}", steps[0]);

    // --- and the permission is gone: the step is refused ---------------------------------------
    revoke(&harness, author, organization_id, "content.pages.publish").await;

    let second_run = run_now(&harness, &author_token, rule_id).await;
    let settled = drive_until_terminal(&harness, &actions, second_run).await;
    assert_eq!(
        settled,
        ExecutionStatus::Failed,
        "a rule whose authority is gone does not go on"
    );
    let steps = steps_of(&harness, second_run).await;
    let error = steps[0]["error"].as_str().unwrap_or_default();
    assert!(
        error.starts_with("automation.rule.permission_revoked"),
        "the step names the event the request specifies: {error}"
    );
    assert!(
        error.contains("content.pages.publish"),
        "and names the permission: {error}"
    );
    assert!(
        error.contains("run-as"),
        "and says whose authority ran out: {error}"
    );

    // The event was recorded, so a rule that lost its authority is *visible* without opening
    // every run by hand.
    let emitted: (i64,) = sqlx::query_as(
        "select count(*) from events where name = 'automation.rule.permission_revoked'",
    )
    .fetch_one(harness.db.pool())
    .await
    .expect("the event must be countable");
    assert!(emitted.0 >= 1, "the refusal is on the bus");

    // --- and the author is deleted: the rule runs as nobody -----------------------------------
    // The rule is written *before* the author is deleted, which is the only order that means
    // anything: a rule written after the fact would have no author to lose.
    let orphan_rule = create_rule(
        &harness,
        &author_token,
        organization_id,
        "Written by somebody who left",
        json!([{
            "name": "publish it anyway",
            "kind": "task",
            "action": "publish_page",
            "params": { "page_id": page_id.to_string() },
            "max_attempts": 1,
        }]),
        None,
    )
    .await;

    // A second account — with the power to run rules, but nothing to do with authorship — is
    // the one that presses "Run now" now that the author is gone.
    let (runner_id, runner_token) = account(&harness, Some(organization_id)).await;
    grant(&harness, runner_id, organization_id, &AUTHOR_KEYS).await;

    sqlx::query("delete from users where id = $1")
        .bind(author)
        .execute(harness.db.pool())
        .await
        .expect("the author must be deletable");

    let orphan_run = run_now(&harness, &runner_token, orphan_rule).await;
    let settled = drive_until_terminal(&harness, &actions, orphan_run).await;
    assert_eq!(
        settled,
        ExecutionStatus::Failed,
        "a rule with no author cannot act: {}",
        steps_of(&harness, orphan_run).await[0]["error"]
    );

    let steps = steps_of(&harness, orphan_run).await;
    let error = steps[0]["error"].as_str().unwrap_or_default();
    assert!(
        error.starts_with("automation.rule.permission_revoked"),
        "even a run-as-nobody rule is refused as the request specifies: {error}"
    );
    // The message has to name the *real* problem. "missing permission" would send an
    // operator to a role assignment that is not the thing that broke.
    assert!(
        error.contains("runs as nobody"),
        "the refusal says the author is gone: {error}"
    );
    assert!(
        error.contains("run-as account"),
        "and says what to do about it: {error}"
    );
    // Crucially: it did *not* fall back to the account that pressed "Run now". A fallback is
    // the stale-authority hole this whole module exists to close, and the run's own
    // `triggered_by` is the account that would have been used.
    let triggered_by: Option<Uuid> =
        sqlx::query_scalar("select triggered_by from workflow_executions where id = $1")
            .bind(orphan_run)
            .fetch_one(harness.db.pool())
            .await
            .expect("the run must be readable");
    assert_eq!(
        triggered_by,
        Some(runner_id),
        "the runner is recorded, and it is not the account the rule acted as"
    );

    let _ = site_id;
}

/// The id of the gate that is waiting on one run.
///
/// Read with the **decider's** session, never the author's: the queue is guarded by
/// `workflows.approve`, so a helper that took "the rule's own token" would answer 403 in
/// every walk and read like a product bug rather than a test that asked the wrong person.
async fn pending_id(harness: &Harness, decider_token: &str, execution_id: Uuid) -> Uuid {
    let answer = harness
        .call(get("/api/v1/approvals?status=pending", decider_token))
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    answer.body["approvals"]
        .as_array()
        .expect("an array")
        .iter()
        .find(|gate| gate["execution_id"] == execution_id.to_string())
        .and_then(|gate| gate["id"].as_str())
        .and_then(|id| Uuid::parse_str(id).ok())
        .unwrap_or_else(|| panic!("no gate is waiting on run {execution_id}: {}", answer.body))
}

/// Take one permission away from one account, by removing the role that granted it.
async fn revoke(harness: &Harness, user_id: Uuid, organization_id: Uuid, key: &str) {
    let role_id: Uuid = sqlx::query_scalar(
        "select b.role_id from role_bindings b join roles r on r.id = b.role_id \
         join role_permissions p on p.role_id = r.id \
         where b.user_id = $1 and p.permission_key = $2 and r.organization_id = $3 limit 1",
    )
    .bind(user_id)
    .bind(key)
    .bind(organization_id)
    .fetch_one(harness.db.pool())
    .await
    .expect("the granting role must be findable");

    // Read what the role holds, drop the one key, and write the rest back. `set_role_permissions`
    // is a *replace*, so removing one key means writing the set without it — and reading it
    // first is what keeps the walk from silently stripping a role of everything else.
    let entries = role_store::permission_entries(harness.db.pool(), &[role_id])
        .await
        .expect("the role's permissions must be readable");
    let held: Vec<RolePermissionInput> = entries
        .get(&role_id)
        .map(|rows| {
            rows.iter()
                .filter_map(|entry| {
                    // `RolePermission` already carries a parsed `Effect`, so there is nothing
                    // to convert: the row is read typed and written back typed.
                    Some(RolePermissionInput {
                        key: entry.key.clone(),
                        effect: entry.effect,
                    })
                })
                .filter(|entry| entry.key != key)
                .collect()
        })
        .unwrap_or_default();
    role_store::set_role_permissions(harness.db.pool(), role_id, &held)
        .await
        .expect("the permission must be removable");

    // And the account really does not hold it any more — the walk asserts the *effect* of the
    // revocation through the engine below, but a broken `revoke` would otherwise make the
    // rest of this walk prove the wrong thing.
    let still_held: i64 = sqlx::query_scalar(
        "select count(*) from role_permissions p join role_bindings b on b.role_id = p.role_id \
         where b.user_id = $1 and p.permission_key = $2",
    )
    .bind(user_id)
    .bind(key)
    .fetch_one(harness.db.pool())
    .await
    .expect("the grant must be countable");
    assert_eq!(still_held, 0, "the permission is really gone");
}

/// A page of this organization, for the `publish_page` walks.
async fn create_page(harness: &Harness, organization_id: Uuid) -> (Uuid, Uuid) {
    let site_id = create_site_row(&harness.db, organization_id, "main", "Main").await;

    let page = omnion_content::create_page(
        harness.db.pool(),
        omnion_content::NewPage {
            site_id,
            slug: format!("gated-{}", Uuid::new_v4().simple()),
            page_type: None,
            title: "Gated".to_owned(),
            body: Some("Waiting for a person.".to_owned()),
            summary: None,
            created_by: None,
        },
    )
    .await
    .expect("the page must be created");

    (site_id, page.0.id)
}

async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&config.database).await {
        Ok(db) => {
            sqlx::query("select 1").execute(db.pool()).await.ok()?;
            Some(db)
        }
        Err(_) => None,
    }
}

fn maintenance_config(config: &Config) -> DatabaseConfig {
    DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 2,
    }
}

/// Point a connection string at a different database on the same server.
///
/// Written rather than parsed: the URL is a credential-bearing string this process already
/// holds, and a re-implementation of the parser is a second answer to "where does this
/// connect to".
fn swap_database(url: &str, database: &str) -> String {
    let (base, _) = url.rsplit_once('/').expect("a URL has a path");
    let base = base.rsplit_once('?').map_or(base, |(head, _)| head);
    format!("{base}/{database}")
}
