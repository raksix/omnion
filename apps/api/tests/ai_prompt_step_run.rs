//! `ai.prompt` as an **ordinary task step** (docs/requests/REQ-046, the last open criterion).
//!
//! Slice 2 wired the action: a registry host action, a bounded budget, the AI Hub router as
//! its client, and an output shape (`text`) a later step can read. Every one of those is a
//! claim about a *shape*, and a shape that is never executed is a shape. This suite is the
//! missing half: a workflow that **contains** an `ai.prompt` step is run by the real engine
//! (`engine::tick`, the same entry point the background runner uses) against the same
//! in-process mock provider the other AI walks use, and the run is read back out of the
//! database.
//!
//! ## What could be wrong and still look right
//!
//! The criterion has three claims, and each has a way of passing without the platform doing
//! anything:
//!
//! * **"passes a template through the model"** — a step that never fires leaves the run
//!   `pending` forever, which reads as "still working" in every UI. The run is therefore
//!   *driven to a terminal status* and the status is asserted, not merely waited for.
//! * **"later steps read the output"** — the output is a JSON blob in a column, and a test
//!   that only checks `status = 'succeeded'` proves the engine moved a row. The second step
//!   therefore *consumes* `{{steps.1.output.text}}` and its own recorded output must contain
//!   the text the model returned. If the interpolation were broken the step would still
//!   succeed — with a literal `{{steps.1.output.text}}` in it — so the assertion is on the
//!   *value*.
//! * **"a provider failure is retried by the existing step backoff"** — this is the claim
//!   most likely to be true for the wrong reason. A step that gave up on the first failure
//!   and a step that retried **both** end in a terminal status, so the terminal status
//!   separates nothing. The only number that separates them is the provider's call counter,
//!   so the retry is measured on the mock and never inferred from the step row.
//!
//! ## Why the harness is copied rather than imported
//!
//! `ai_workflow_decisions.rs` owns a mock provider, a CSRF-aware credential and a scratch
//! database. This file is a *second* walk, and a walk with its own authentication teaches the
//! suite a second answer to "what does a credential look like" — which is how a suite ends up
//! testing its own guess. The harness below is that suite's, unchanged, for the same reason
//! its own walks are proven against it.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;
use tokio::net::TcpListener;
use tower::ServiceExt;
use uuid::Uuid;

mod support;

use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_api::rate_limit_middleware::RateLimiter;
use omnion_security::RatePolicy;

/// The answer the mock provider hands back for the *successful* `ai.prompt` step.
///
/// It has to be a string that cannot appear anywhere else in a run's output, because the
/// assertion is "the second step's recorded value is exactly this" — a fixture made of the
/// word "hello" would also match a step that had interpolated nothing.
const MODEL_ANSWER: &str = "The invoice is 14 days late and worth 4200.";

/// A definition with **two** steps: the model answers, and a later step reads that answer.
///
/// The second step is a **branch**, not an `echo` carrying a `{{steps.1.output.text}}`
/// placeholder, and that is the correction this suite exists to record. The `{{ }}` binding
/// namespace is `event` and nothing else — `binding::validate_bindings` refuses any expression
/// that does not start with `event`, because resolution happens when the run is *materialised*
/// from the recorded event and a placeholder naming a step does not exist at that moment. A step
/// therefore reads an earlier step's output through the **branch** vocabulary instead:
/// `steps.<number>.<field>`, evaluated against the run's own scope (`branch_scope`), where the
/// `ai.prompt` output's `text` is reached as `steps.1.text`.
///
/// The first version of this fixture put `{{steps.1.output.text}}` in an `echo` and the walk
/// passed the run as `completed` with the second step's recorded value being the **literal
/// string** `{{steps.1.output.text}}`. A step that interpolates nothing and a step that
/// interpolates correctly are both `succeeded`; only the recorded value tells them apart, which
/// is why the assertion below is on the value and not on the status.
const TWO_STEP_DEFINITION: &str = r#"{
  "trigger": { "kind": "manual" },
  "steps": [
    { "name": "summarise", "kind": "task", "action": "ai.prompt",
      "params": { "prompt": "Summarise this invoice in one sentence." } },
    { "name": "answered", "kind": "branch",
      "params": { "field": "steps.1.text", "operator": "equals", "value": "The invoice is 14 days late and worth 4200." } }
  ]
}"#;

/// A definition whose only step is the model, used by the retry walk.
///
/// `max_attempts: 3` is the whole point of this fixture and it is **not** the default:
/// `StepDefinition::max_attempts` serialises with `#[serde(default = "one")]`, so a step that
/// does not name it is allowed exactly one attempt and the backoff is never reached. The first
/// run of this walk proved it — the 502 landed the step in `failed` at `attempts = 1`, which
/// reads as "the engine does not retry provider failures" and is in fact "the step never asked
/// to be retried". A criterion about the retry path has to be proved on a step that opted into
/// it; proving it on a step that did not would have measured the default, not the mechanism.
const ONE_STEP_DEFINITION: &str = r#"{
  "trigger": { "kind": "manual" },
  "steps": [
    { "name": "summarise", "kind": "task", "action": "ai.prompt",
      "params": { "prompt": "Summarise this invoice in one sentence." },
      "max_attempts": 3 }
  ]
}"#;

struct TestResponse {
    status: StatusCode,
    set_cookies: Vec<String>,
    body: Value,
    text: String,
}

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
        // Every walk here WRITES: it creates a tenant, connects a provider, creates a rule and
        // runs it. With no CSRF secret configured every one of those is refused with
        // `csrf_unavailable` — and that refusal is the product working, not a defect. The
        // fixture sets one on the **config** rather than relying on `OMNION_CSRF_SECRET` being
        // in the shell, because a test process does not have it.
        support::walk_auth::with_csrf_secret(&mut config);
        live_db(&config).await?;

        let database = format!("omnion_aistep_{}", Uuid::new_v4().simple());
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

        // The sign-in budget is raised for this process, once, before any walk runs.
        give_the_suite_its_own_sign_in_budget(&state);

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

/// A scripted mock provider: the answers it will give, in order, and how many were taken.
///
/// The count is a **counter the provider increments**, not derived from what is left in the
/// queue. Deriving it from the remainder is the version that lies, and it is the number the
/// retry claim depends on.
#[derive(Clone, Default)]
struct Script {
    queue: Arc<Mutex<VecDeque<String>>>,
    taken: Arc<Mutex<usize>>,
}

impl Script {
    fn of(answers: &[&str]) -> Self {
        Self {
            queue: Arc::new(Mutex::new(
                answers.iter().map(|a| (*a).to_owned()).collect(),
            )),
            taken: Arc::new(Mutex::new(0)),
        }
    }

    /// How many calls the platform actually made to this provider.
    fn calls(&self) -> usize {
        *self.taken.lock().expect("the counter lock must hold")
    }

    /// Pop the next answer; a call past the script panics rather than reusing one, which is
    /// what makes the retry claim a claim a *second* call cannot pass.
    fn next(&self) -> String {
        *self.taken.lock().expect("the counter lock must hold") += 1;
        self.queue
            .lock()
            .expect("the script lock must hold")
            .pop_front()
            .unwrap_or_else(|| {
                panic!(
                    "the provider was called more times than the test scripted — the step spent \
                     more provider calls than its policy allows"
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
    async fn start(script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock must bind a port");
        let address = listener.local_addr().expect("the mock has an address");

        let app = axum::Router::new()
            .route("/v1/chat/completions", axum::routing::post(mock_chat))
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
/// mock that always answered cleanly would never exercise the retry path at all.
async fn mock_chat(
    axum::extract::State(script): axum::extract::State<Script>,
    axum::Json(_body): axum::Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let answer = script.next();
    if answer == "__transport__" {
        return axum::response::Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::from("upstream is unavailable"))
            .expect("the mock failure must build");
    }

    axum::Json(json!({
        "choices": [{
            "message": { "role": "assistant", "content": answer },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 7, "completion_tokens": 4, "total_tokens": 11 }
    }))
    .into_response()
}

fn request(
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Request<Body> {
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

fn post(uri: &str, body: Value, token: Option<&str>) -> Request<Body> {
    request(Method::POST, uri, token, Some(body))
}

fn token_of(response: &TestResponse) -> String {
    support::walk_auth::Session::from_set_cookies(&response.set_cookies).pack()
}

fn give_the_suite_its_own_sign_in_budget(state: &AppState) {
    let policies: Vec<RatePolicy> = RatePolicy::defaults()
        .into_iter()
        .map(|mut policy| {
            if policy.scope == "sign_in" {
                policy.limit = 10_000;
            }
            policy
        })
        .collect();
    omnion_api::rate_limit_middleware::install(RateLimiter::new(state, policies));
}

/// Sign in a fresh installation and give the owner a tenant to work in.
///
/// The owner account the wizard creates has no organization, and a rule belongs to one — so the
/// walkthrough's own `ensure-organization` step exists for the same reason.
async fn owner_with_tenant(harness: &Harness) -> (String, Uuid) {
    let owner = harness
        .call(post(
            "/api/v1/onboarding/owner",
            json!({
                "display_name": "Ada Lovelace",
                "email": format!("owner-{}@omnion.test", Uuid::new_v4().simple()),
                "password": "correct horse battery",
            }),
            None,
        ))
        .await;
    assert_eq!(owner.status, StatusCode::CREATED, "{}", owner.text);
    let token = token_of(&owner);

    let slug = format!("tenant-{}", Uuid::new_v4().simple());
    harness
        .call(post(
            "/api/v1/onboarding/organization",
            json!({ "name": "Acme", "slug": slug }),
            Some(&token),
        ))
        .await;

    // The wizard answers a *status* body rather than the tenant it created, so the id is read
    // from the account it attached the tenant to. Reading it from the response is a `None` that
    // surfaces several assertions later as a complaint about workflows.
    let user_id = Uuid::parse_str(owner.body["user"]["id"].as_str().expect("user.id"))
        .expect("user.id is a uuid");
    let organization_id: Uuid =
        sqlx::query_scalar("select organization_id from users where id = $1")
            .bind(user_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the owner must have a tenant");

    (token, organization_id)
}

/// Connect the mock as the platform's default provider.
async fn connect(harness: &Harness, token: &str, mock: &MockProvider) {
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
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
}

/// Create a workflow holding `definition` and return its id.
///
/// The create body carries `trigger` and `steps` at the **top level** — `WorkflowInput`
/// deserializes them as required fields, so a body that nests them under a `definition` key is
/// refused with `422 missing field 'trigger'` before any workflow code runs, and the walk then
/// spends its whole budget reporting "the rule was never created".
async fn create_workflow(
    harness: &Harness,
    token: &str,
    name: &str,
    definition: &str,
) -> Uuid {
    let parsed: Value =
        serde_json::from_str(definition).expect("the fixture definition must parse");
    let created = harness
        .call(post(
            "/api/v1/workflows",
            json!({
                "name": name,
                "description": "ai.prompt run probe",
                "trigger": parsed["trigger"],
                "steps": parsed["steps"],
            }),
            Some(token),
        ))
        .await;
    assert_eq!(
        created.status, StatusCode::CREATED,
        "the rule must be created — otherwise the run walk proves nothing: {}",
        created.text
    );
    let id = created.body["workflow"]["id"]
        .as_str()
        .or_else(|| created.body["id"].as_str())
        .unwrap_or_else(|| {
            panic!("the create must carry a workflow id: {}", created.text)
        });
    Uuid::parse_str(id).expect("the workflow id is a uuid")
}

/// Press Run and return the execution id.
async fn run_now(harness: &Harness, token: &str, workflow_id: Uuid) -> Uuid {
    let started = harness
        .call(post(
            &format!("/api/v1/workflows/{workflow_id}/run"),
            json!({}),
            Some(token),
        ))
        .await;
    assert_eq!(
        started.status, StatusCode::ACCEPTED,
        "the run must be accepted: {}",
        started.text
    );
    let id = started.body["execution"]["id"]
        .as_str()
        .or_else(|| started.body["id"].as_str())
        .unwrap_or_else(|| {
            panic!("the run must carry an execution id: {}", started.text)
        });
    Uuid::parse_str(id).expect("the execution id is a uuid")
}

/// Drive the engine until `execution_id` reaches a terminal status, or give up.
///
/// The loop is the background runner's own entry point (`engine::tick`), not a private helper,
/// because the claim under test is that an `ai.prompt` step runs *as the engine runs any other
/// step*. A test that drove the step by hand would prove the action works and say nothing about
/// the engine.
async fn drive_to_settle(harness: &Harness, execution_id: Uuid) -> Option<String> {
    let actions = omnion_automation::AutomationActions::new(
        harness.db.pool().clone(),
        omnion_automation::MailSettings::new("127.0.0.1", 1, "omnion@localhost")
            .with_sending(false),
    );
    let guard = omnion_workflows::guard::NoRunGuard;
    // A fast backoff, and deliberately not the production one: the retry walk needs the *first*
    // retry to land inside the test, and `retry_base` of five seconds would make this a
    // 30-second walk to prove a claim about the *mechanism* rather than the delay.
    let config = omnion_workflows::engine::RunnerConfig {
        tick: time::Duration::milliseconds(20),
        retry_base: time::Duration::milliseconds(20),
        retry_max: time::Duration::milliseconds(80),
        ..Default::default()
    };

    for _ in 0..600 {
        let _ = omnion_workflows::engine::tick_with(
            harness.db.pool(),
            &config,
            &actions,
            &guard,
        )
        .await;
        let status: Option<String> =
            sqlx::query_scalar("select status from workflow_executions where id = $1")
                .bind(execution_id)
                .fetch_one(harness.db.pool())
                .await
                .ok();
        if let Some(status) = status.as_deref() {
            if status != "pending" && status != "running" {
                return Some(status.to_owned());
            }
        }
        tokio::time::sleep(StdDuration::from_millis(20)).await;
    }
    None
}

/// The recorded output of one step, read from the database rather than the API.
///
/// The API's step shape is a *view*; the column is the record, and the criterion says the
/// output is visible in the execution's step rows.
async fn step_output(
    harness: &Harness,
    execution_id: Uuid,
    step_no: i32,
) -> Option<Value> {
    sqlx::query_scalar::<_, Option<Value>>(
        "select output from workflow_steps where execution_id = $1 and step_no = $2",
    )
    .bind(execution_id)
    .bind(step_no)
    .fetch_one(harness.db.pool())
    .await
    .ok()
    .flatten()
}

/// The criterion's first two claims: the step runs, and a later step reads its output.
#[tokio::test]
async fn an_ai_prompt_step_runs_and_a_later_step_reads_its_output() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let mock = MockProvider::start(Script::of(&[MODEL_ANSWER])).await;
    let (token, _organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &token, &mock).await;

    let workflow_id = create_workflow(
        &harness,
        &token,
        &format!("QA ai.prompt {}", Uuid::new_v4().simple()),
        TWO_STEP_DEFINITION,
    )
    .await;
    let execution_id = run_now(&harness, &token, workflow_id).await;

    let status = drive_to_settle(&harness, execution_id).await;
    assert_eq!(
        status.as_deref(),
        Some("completed"),
        "the run must reach a terminal status, not sit in `pending` — a step that never fires \
         reads as \"still working\" in every screen"
    );

    // The model was actually asked, exactly once.
    assert_eq!(
        mock.script.calls(),
        1,
        "the `ai.prompt` step must call the provider exactly once"
    );

    // The answer is in the step's own output row, under the documented shape.
    let first = step_output(&harness, execution_id, 1)
        .await
        .expect("step 1 must record an output");
    assert_eq!(first["action"], "ai.prompt", "{first}");
    assert_eq!(
        first["text"], MODEL_ANSWER,
        "the model's answer must be the step's output text: {first}"
    );
    assert!(
        first["model"].as_str().is_some_and(|m| !m.is_empty()),
        "the step echoes the model that answered — the first question an operator asks of a rule \
         whose behaviour changed: {first}"
    );

    // **The load-bearing assertion.** The branch read `steps.1.text` — a key the
    // `ai.prompt` step's own output produced — and the branch *held*, so the run was allowed to
    // finish. A branch that read nothing is a run that ends early with "`steps.1.text` is not
    // something this run knows yet", and a branch that read the wrong value is a run that ends
    // early for a different reason. The proof that the later step genuinely consumed the AI
    // step's output is therefore both halves: the branch's recorded verdict says it held, and
    // the comparison it held *against* is the text the mock returned.
    let branch_step = sqlx::query_as::<_, (String, i32, Option<Value>)>(
        "select status, step_no, output from workflow_steps \
         where execution_id = $1 and step_no = 2",
    )
    .bind(execution_id)
    .fetch_one(harness.db.pool())
    .await
    .expect("the branch step must be recorded");

    assert_eq!(
        branch_step.0, "succeeded",
        "the branch must succeed — it is the step that read the AI step's output: {:?}",
        branch_step.2
    );
    let recorded = branch_step.2.expect("a branch records what it decided");
    assert_eq!(
        recorded["branch"]["field"], "steps.1.text",
        "the branch must have read the AI step's output, not something else: {recorded}"
    );
    assert_eq!(
        recorded["branch"]["holds"], json!(true),
        "the branch must HOLD — `steps.1.text` has to be exactly the text the model returned, or \
         a later step does not read the AI step's output: {recorded}"
    );
    assert!(
        !recorded.to_string().contains("{{"),
        "the recorded branch still carries an unresolved template expression: {recorded}"
    );

    // The branch compared `steps.1.text` with `equals` against MODEL_ANSWER and the engine
    // records only `{field, holds}`. `holds: true` is therefore the load-bearing half and the
    // *field* is what makes it mean anything: the recorded field is the one the definition
    // named, so "held" is a statement about that field's value, not about any field. The
    // control below is what stops this from being a tautology — a branch that reads nothing at
    // all cannot hold, because `evaluate` answers `Err` (and ends the run) for a field the run
    // does not know, rather than a false verdict.
    let params_field: String =
        sqlx::query_scalar("select params->>'field' from workflow_steps where execution_id = $1 and step_no = 2")
            .bind(execution_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the branch step keeps its own parameters");
    assert_eq!(
        params_field, "steps.1.text",
        "the comparison the branch actually ran is the one this test claims: {params_field}"
    );
    let params_operator: String =
        sqlx::query_scalar("select params->>'operator' from workflow_steps where execution_id = $1 and step_no = 2")
            .bind(execution_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the branch step keeps its own parameters");
    assert_eq!(
        params_operator, "equals",
        "an `equals` against the exact sentence the mock returned is the only comparison that \
         proves the value survived the round trip: {params_operator}"
    );
    let params_value: Option<String> =
        sqlx::query_scalar("select params->>'value' from workflow_steps where execution_id = $1 and step_no = 2")
            .bind(execution_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the branch step keeps its own parameters");
    assert_eq!(
        params_value.as_deref(),
        Some(MODEL_ANSWER),
        "the branch must have been comparing against the text the provider returned, otherwise \
         `holds` is a statement about some other comparison: {params_value:?}"
    );

    harness.dispose().await;
}

/// The criterion's third claim: a provider failure is retried by the step backoff.
#[tokio::test]
async fn a_provider_failure_is_retried_by_the_step_backoff() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    // Transport failure first, good answer second. A `502` with no body is what an unreachable
    // provider looks like, and it is the class the engine is supposed to retry.
    let mock = MockProvider::start(Script::of(&["__transport__", MODEL_ANSWER])).await;
    let (token, _organization_id) = owner_with_tenant(&harness).await;
    connect(&harness, &token, &mock).await;

    let workflow_id = create_workflow(
        &harness,
        &token,
        &format!("QA ai.prompt retry {}", Uuid::new_v4().simple()),
        ONE_STEP_DEFINITION,
    )
    .await;
    let execution_id = run_now(&harness, &token, workflow_id).await;

    let status = drive_to_settle(&harness, execution_id).await;
    assert_eq!(
        status.as_deref(),
        Some("completed"),
        "a run whose only provider call failed once must still complete: the step backoff is \
         what turns one transport failure into a completed run"
    );

    // The retry is measured on the provider, not inferred from the step row. A step that gave
    // up would *also* end in a terminal status — the call count is the only number that
    // separates "retried" from "gave up".
    assert_eq!(
        mock.script.calls(),
        2,
        "the step must have called the provider twice — once for the failure and once for the \
         retry — so the retry is measured rather than assumed"
    );

    let output = step_output(&harness, execution_id, 1)
        .await
        .expect("the retried step must record an output");
    assert_eq!(
        output["text"], MODEL_ANSWER,
        "the output must be the *successful* answer, not the failure: {output}"
    );

    harness.dispose().await;
}

async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 1,
    })
    .await
    {
        Ok(db) => Some(db),
        Err(err) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({err})");
            None
        }
    }
}

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

fn maintenance_config(config: &Config) -> DatabaseConfig {
    DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 1,
    }
}
