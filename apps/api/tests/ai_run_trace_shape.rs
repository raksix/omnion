//! The trace the **real runner** writes, and the acceptance box it is the only proof of (REQ-099).
//!
//! `ai_agent_runs.rs` proved the persister hears a cancellation. That is one event, and it says
//! nothing about the *shape* of what lands on disk — which is the whole of the unticked
//! criterion:
//!
//! > a run started from the panel streams steps into the detail view live, and the same run
//! > reloaded after completion shows an identical step list (replay matches SSE)
//!
//! The doc on [`run_store::append_event`] claims the trace the panel renders and the rows a
//! replay reads are the same rows, and the route's re-attach is built on exactly that promise.
//! So this suite drives **the runner's own persister** over a real run with the exact event
//! sequence the loop emits for a two-step run, and asserts the shape of the rows.
//!
//! ## The defect this suite was written for
//!
//! `append_event` mapped every event except `Error` to `StepKind::Note` and stored the whole
//! event as a jsonb blob in `arguments`, through `begin_step` — which always inserts `running`.
//! Three consequences, all of them product-level:
//!
//! 1. **Every step stayed `running` forever.** Nothing in the production path ever called
//!    `finish_step`; only the tests did. So a finished run carried steps in `running`.
//! 2. **Therefore the run's tokens and cost were zero.** `run_totals_match_steps` sums over
//!    `status = 'completed'`. No step ever reached that, so every real run reported 0 tokens and
//!    0 cost whatever the provider billed — and the panel's cost column is fed by that.
//! 3. **Therefore every finished run was permanently ambiguous on resume.** `resume_point` asks
//!    for the first step that is not `completed`; that is step 1, whose status is `running`, so
//!    the resume route answered `run.ambiguous_step` for a run that had already finished, and
//!    `run.complete` could never fire.
//!
//! The trace also lied about *what happened*: a `ToolCall` event rendered as `Note` carrying
//! `{"ToolCall":{…}}` with a null `tool` column, so the panel's tool name, kind label and
//! arguments block were all unavailable — while `ai_run_steps_tool_only_for_tool_kinds` exists
//! precisely to insist a tool kind carries its tool.
//!
//! The assertions are written against the *product's* vocabulary (kind, tool, status, tokens)
//! rather than against the serialized event, because a suite that asserts the json blob would
//! keep passing after the blob stopped being what the panel renders.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::ai_agent_runner::persister_for_tests;
use omnion_api::rate_limit_middleware::RateLimiter;
use omnion_api::state::AppState;
use omnion_ai_hub::agent::{AgentEvent, StepKind, StepStatus, StopReason, ToolCall};
use omnion_ai_hub::run_store::{self, NewAgent, NewRun};
use omnion_core::config::{Config, CsrfSecret, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_security::RatePolicy;
use serde_json::{Value, json};
use sqlx::Row;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

/// The key the CSRF tokens are derived from in this suite.
///
/// Fixed, not random, and **configured** rather than left absent: without it every
/// cookie-authenticated write answers `403 csrf_unavailable`, which reads as a product fact
/// rather than as a fixture that never set a secret up.
const CSRF_SECRET: &str = "ai-trace-shape-suite-key-material";

// -------------------------------------------------------------------------------------------
// Harness
// -------------------------------------------------------------------------------------------

struct TestResponse {
    status: StatusCode,
    /// **Every** `Set-Cookie`, in order. Login sets two cookies and a single-cookie accessor
    /// reports green while every write it makes is refused.
    set_cookie: Vec<String>,
    body: Value,
}

impl TestResponse {
    fn cookie(&self, name: &str) -> String {
        self.set_cookie
            .iter()
            .filter_map(|header| header.split(';').next())
            .filter_map(|pair| pair.split_once('='))
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_owned())
            .unwrap_or_default()
    }
}

/// A signed-in session and the CSRF token that goes with it.
///
/// The two travel together on purpose: passing only the session is the shape that produces a
/// green suite full of `403`s, and passing the token in the cookie jar instead of the header is
/// the shape that produces one that cannot write at all.
#[derive(Clone)]
struct Session {
    token: String,
    csrf: String,
}

impl Session {
    fn of(response: &TestResponse) -> Self {
        let token = response.cookie("omnion_session");
        assert!(
            !token.is_empty(),
            "the response must set a session cookie; cookies were {:?}",
            response.set_cookie
        );
        Self {
            token,
            csrf: response.cookie("omnion_csrf"),
        }
    }

    fn auth(self: &Self, builder: axum::http::request::Builder) -> axum::http::request::Builder {
        builder
            .header(header::COOKIE, format!("omnion_session={}", self.token))
            .header("x-omnion-csrf", self.csrf.clone())
    }
}

struct Harness {
    state: AppState,
    db: Db,
    maintenance: Db,
    database: String,
    organization_id: Uuid,
}

impl Harness {
    async fn fresh() -> Option<Self> {
        let mut config = Config::from_env().expect("environment must be valid");
        config.csrf = CsrfSecret::new(Some(CSRF_SECRET.to_owned()));
        if live_db(&config).await.is_none() {
            return None;
        }

        let database = format!("omnion_traceshape_{}", Uuid::new_v4().simple());
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

        // Only the sign-in ceiling moves: the limiter is a process-wide cell this box's other
        // suites share, and a walk refused with a `429` would be testing the reader.
        let policies: Vec<RatePolicy> = RatePolicy::defaults()
            .into_iter()
            .map(|mut policy| {
                if policy.scope == "sign_in" {
                    policy.limit = 10_000;
                }
                policy
            })
            .collect();
        omnion_api::rate_limit_middleware::install(RateLimiter::new(&state, policies));

        // A tenant, written the way the platform writes one. The database is this suite's own,
        // so the id the insert returns is unambiguous.
        let organization_id: Uuid = sqlx::query_scalar(
            "insert into organizations (name, slug) values ('Trace Shape', $1) returning id",
        )
        .bind(format!("trace-shape-{}", Uuid::new_v4().simple()))
        .fetch_one(db.pool())
        .await
        .expect("the organization must be creatable");

        Some(Self {
            state,
            db,
            maintenance,
            database,
            organization_id,
        })
    }

    async fn call(&self, request: Request<Body>) -> TestResponse {
        let response = omnion_api::routes::router(self.state.clone())
            .oneshot(request)
            .await
            .expect("router must answer");
        let status = response.status();
        let set_cookie = response
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
        TestResponse {
            status,
            set_cookie,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    async fn dispose(self) {
        self.db.pool().close().await;
        let database = self.database;
        sqlx::query(&format!("drop database if exists \"{database}\" with (force)"))
            .execute(self.maintenance.pool())
            .await
            .expect("the temporary database must be removed");
    }
}

async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&DatabaseConfig {
        url: config.database.url.clone(),
        max_connections: 1,
    })
    .await
    {
        Ok(db) => Some(db),
        Err(_) => None,
    }
}

fn maintenance_config(config: &Config) -> DatabaseConfig {
    let url = config.database.url.clone();
    let (base, _) = url.rsplit_once('/').expect("a database URL has a path");
    DatabaseConfig {
        url: format!("{base}/postgres"),
        max_connections: 1,
    }
}

fn swap_database(url: &str, database: &str) -> String {
    let (base, _) = url.rsplit_once('/').expect("a database URL has a path");
    format!("{base}/{database}")
}

// -------------------------------------------------------------------------------------------
// Request builders
// -------------------------------------------------------------------------------------------

fn get(uri: &str, session: Option<&Session>) -> Request<Body> {
    let builder = Request::builder().method(Method::GET).uri(uri);
    let builder = match session {
        Some(session) => session.auth(builder),
        None => builder,
    };
    builder.body(Body::empty()).expect("request must build")
}

fn post(uri: &str, body: Value, session: Option<&Session>) -> Request<Body> {
    let builder = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    let builder = match session {
        Some(session) => session.auth(builder),
        None => builder,
    };
    builder
        .body(Body::from(body.to_string()))
        .expect("request must build")
}

/// The seeded role that may read an agent run, looked up rather than named.
///
/// A literal role id makes the suite fail on a permission it never meant to be testing the day
/// the seeded ids move.
async fn ai_role(harness: &Harness) -> Uuid {
    sqlx::query("select id from roles where key in ('administrator','admin','editor') \
                 order by case key when 'administrator' then 0 when 'admin' then 1 else 2 end limit 1")
        .fetch_one(harness.db.pool())
        .await
        .expect("an installation ships at least one role that may use the AI")
        .get::<Uuid, _>("id")
}

/// An installer, an organization and a member of it — the shape every tenant-scoped walk needs.
async fn install(harness: &Harness) -> Session {
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
    let owner = Session::of(&owner);

    let email = format!("trace-{}@omnion.test", Uuid::new_v4().simple());
    let member = harness
        .call(post(
            "/api/v1/iam/users",
            json!({
                "email": email,
                "display_name": "Trace Reader",
                "organization_id": harness.organization_id,
                "password": PASSWORD,
                "role_id": ai_role(harness).await,
            }),
            Some(&owner),
        ))
        .await;
    assert_eq!(member.status, StatusCode::CREATED, "{:?}", member.body);

    // `POST /iam/users` creates the row; it does not sign the new member in. Reading a session
    // cookie off that response is a fixture that gets nothing and asserts nothing.
    let signed_in = harness
        .call(post(
            "/api/v1/auth/login",
            json!({ "email": email, "password": PASSWORD }),
            None,
        ))
        .await;
    assert_eq!(signed_in.status, StatusCode::OK, "{:?}", signed_in.body);
    assert_eq!(
        signed_in.body["user"]["organization_id"],
        json!(harness.organization_id.to_string()),
        "the session is tenant-scoped, which is what the run route filters on"
    );
    Session::of(&signed_in)
}

// -------------------------------------------------------------------------------------------
// The event sequence of a two-step run
// -------------------------------------------------------------------------------------------

/// Exactly what `loop_engine` publishes for: step 1 calls a tool, step 2 answers.
///
/// Written out event by event rather than produced by the engine, because the claim under test
/// is about the **persister's** reading of the engine's events. A fixture generated by the same
/// loop the production path uses could agree with a wrong persister for the wrong reason.
async fn publish_a_two_step_run(harness: &Harness, run_id: Uuid) {
    let persister = persister_for_tests(
        harness.db.pool().clone(),
        run_id,
        omnion_ai_hub::CancelHandle::new(),
    );

    persister(AgentEvent::StepStarted {
        step_no: 1,
        kind: StepKind::Message,
    })
    .await;
    persister(AgentEvent::ToolCall {
        step_no: 1,
        call: ToolCall::new("page.search", json!({ "q": "pricing" })),
    })
    .await;
    persister(AgentEvent::ToolResult {
        step_no: 1,
        tool: "page.search".to_owned(),
        summary: "three pages".to_owned(),
        failed: false,
    })
    .await;
    persister(AgentEvent::Usage {
        step_no: 1,
        prompt_tokens: 31,
        completion_tokens: 6,
    })
    .await;
    persister(AgentEvent::StepStarted {
        step_no: 2,
        kind: StepKind::Message,
    })
    .await;
    persister(AgentEvent::Text {
        step_no: 2,
        delta: "The pricing page is /pricing.".to_owned(),
    })
    .await;
    persister(AgentEvent::Usage {
        step_no: 2,
        prompt_tokens: 12,
        completion_tokens: 9,
    })
    .await;
    // The engine sends `Done` to the **sink** only — it is the loop's own return value, not a
    // fact about a step. It is published here anyway, because a persister that treats it as one
    // more `note` is a trace whose last row says a run finished rather than that it did.
    persister(AgentEvent::Done {
        steps: 2,
        stop_reason: StopReason::FinalAnswer,
    })
    .await;
}

async fn agent_and_run(harness: &Harness, key: &str) -> Uuid {
    let mut new_agent = NewAgent::with_defaults(harness.organization_id, key.to_owned(), "Trace shape");
    new_agent.system_prompt = "You answer questions.".to_owned();
    let agent = run_store::create_agent(harness.db.pool(), &new_agent)
        .await
        .expect("the agent must be creatable");

    let mut new_run = NewRun::default_for(harness.organization_id, agent.id);
    new_run.trigger = "agent".to_owned();
    new_run.goal = "What plans do you publish?".to_owned();
    run_store::create_run(harness.db.pool(), &new_run)
        .await
        .expect("the run must be creatable")
        .id
}

// -------------------------------------------------------------------------------------------
// The walks
// -------------------------------------------------------------------------------------------

/// The trace rows carry the loop's own vocabulary: a tool call is a `tool_call` with its tool
/// named, and the answer is a `message` — not a column of `note` rows holding serialized events.
#[tokio::test]
async fn a_tool_call_is_a_tool_call_and_not_a_note_carrying_json() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let run = agent_and_run(&harness, "shape").await;
    publish_a_two_step_run(&harness, run).await;

    let steps = run_store::list_steps(harness.db.pool(), run)
        .await
        .expect("the steps must be readable");

    let call = steps
        .iter()
        .find(|step| step.kind == "tool_call")
        .unwrap_or_else(|| {
            panic!(
                "no tool_call step: the trace collapsed every event into a note — {}",
                describe(&steps)
            )
        });
    assert_eq!(
        call.tool.as_deref(),
        Some("page.search"),
        "the tool name is its own column, not a field inside a json blob"
    );
    assert_eq!(
        call.arguments
            .as_ref()
            .and_then(|arguments| arguments.get("q"))
            .and_then(Value::as_str),
        Some("pricing"),
        "the arguments are the call's own, not a serialized event: {:?}",
        call.arguments
    );

    // The result is a *detail of* the call, not a second row: one loop step is one act, and the
    // act is the model asking. A separate `tool_result` row would double the trace's length for
    // a run that made one call, and the schema's unique `(run_id, step_no)` is the reason the
    // writer folds rather than appends.
    let summary = call
        .result
        .as_ref()
        .and_then(|result| result.get("summary"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            panic!("the tool's result belongs to the call's row — {}", describe(&steps))
        });
    assert_eq!(summary, "three pages", "what the tool returned is on the call's row");
    assert_eq!(
        call.result.as_ref().and_then(|result| result.get("failed")),
        Some(&json!(false)),
        "and whether it failed, because a failed tool and an empty one read the same otherwise"
    );

    harness.dispose().await;
}

/// Every step the loop finished is `completed`. This is the assertion with the widest blast
/// radius: `run_totals_match_steps` and `resume_point` both filter on it, so steps stuck in
/// `running` mean a run reports no cost and can never be resumed.
#[tokio::test]
async fn a_finished_runs_steps_are_completed_and_not_left_running() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let run = agent_and_run(&harness, "closed").await;
    publish_a_two_step_run(&harness, run).await;
    // The runner closes the open steps the moment the run ends, before it writes the run row.
    // Driven here explicitly so the walk tests the *trace*, not the runner's control flow.
    omnion_ai_hub::run_store::close_open_steps(harness.db.pool(), run)
        .await
        .expect("the open step must be closable");

    let steps = run_store::list_steps(harness.db.pool(), run)
        .await
        .expect("the steps must be readable");
    let still_running: Vec<i32> = steps
        .iter()
        .filter(|step| step.status == "running")
        .map(|step| step.step_no)
        .collect();
    assert!(
        still_running.is_empty(),
        "a run that reached Done must not leave steps running; these are {still_running:?} — a \
         step left running makes the run's tokens and cost zero and its resume ambiguous"
    );

    harness.dispose().await;
}

/// The numbers the panel shows, recomputed by the same store functions the route calls.
///
/// Asserted through `run_totals_match_steps` rather than by restating its SQL: a walk that
/// computes the answer its own way proves its own arithmetic.
#[tokio::test]
async fn a_finished_run_reports_the_tokens_the_provider_charged() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let run = agent_and_run(&harness, "totals").await;
    publish_a_two_step_run(&harness, run).await;
    omnion_ai_hub::run_store::close_open_steps(harness.db.pool(), run)
        .await
        .expect("the open step must be closable");

    // Closed the way the runner closes it, so the run row carries the recomputed totals.
    run_store::finish_run(harness.db.pool(), run, StepStatus::Completed, StopReason::FinalAnswer, None)
        .await
        .expect("the run must finish");

    let totals = run_store::run_totals_match_steps(harness.db.pool(), run)
        .await
        .expect("the totals must be computable");
    assert_eq!(
        (totals.prompt_tokens, totals.completion_tokens),
        (43, 15),
        "31+12 prompt and 6+9 completion tokens, summed over the run's own completed steps"
    );

    // The run row agrees with its own steps — the panel's stored-vs-recomputed pair.
    let row: (i32, i32, i64, Option<String>, Option<String>) = sqlx::query_as(
        "select prompt_tokens, completion_tokens, cost_micros, status, stop_reason \
         from ai_runs where id = $1",
    )
    .bind(run)
    .fetch_one(harness.db.pool())
    .await
    .expect("the run row must be readable");
    assert_eq!(
        (row.0, row.1),
        (43, 15),
        "the stored totals match the rows the run left behind"
    );
    assert_eq!(row.3.as_deref(), Some("completed"));
    assert_eq!(row.4.as_deref(), Some("final_answer"));

    harness.dispose().await;
}

/// A finished run is resumable as *finished*: `resume_point` finds nothing, so the route says
/// "every step completed" rather than the misleading `run.ambiguous_step`.
///
/// On the defect this was the third symptom, and a walk asserting only "the resume is refused"
/// would pass against either message — so the signal itself is asserted.
#[tokio::test]
async fn a_finished_run_has_no_resume_point() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let run = agent_and_run(&harness, "resumable").await;
    publish_a_two_step_run(&harness, run).await;
    omnion_ai_hub::run_store::close_open_steps(harness.db.pool(), run)
        .await
        .expect("the open step must be closable");
    run_store::finish_run(harness.db.pool(), run, StepStatus::Completed, StopReason::FinalAnswer, None)
        .await
        .expect("the run must finish");

    let point = run_store::resume_point(harness.db.pool(), run)
        .await
        .expect("the resume point must be computable");
    assert_eq!(
        point, None,
        "a run whose steps are all completed has no resume point; a non-null one is a step left \
         running, which the route reports as a tool that may already have fired"
    );

    harness.dispose().await;
}

/// The route's own reading of the same run, through the router.
///
/// The walks above are about the store. This one is about the document the browser renders: the
/// `steps` array `GET /ai/runs/{id}` returns *is* the accordion, and its `status` is what the
/// badge is coloured from. A store fix the route still reshapes would leave the screen lying.
#[tokio::test]
async fn the_run_detail_the_panel_reads_shows_the_trace_shape() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let run = agent_and_run(&harness, "panel").await;
    publish_a_two_step_run(&harness, run).await;
    omnion_ai_hub::run_store::close_open_steps(harness.db.pool(), run)
        .await
        .expect("the open step must be closable");
    run_store::finish_run(harness.db.pool(), run, StepStatus::Completed, StopReason::FinalAnswer, None)
        .await
        .expect("the run must finish");

    let session = install(&harness).await;
    let detail = harness
        .call(get(&format!("/api/v1/ai/runs/{run}"), Some(&session)))
        .await;
    assert_eq!(detail.status, StatusCode::OK, "{:?}", detail.body);

    let steps = detail.body["steps"]
        .as_array()
        .unwrap_or_else(|| panic!("the run detail carries its trace: {:?}", detail.body));
    assert!(!steps.is_empty(), "a run that did work has a trace");

    let kinds: Vec<&str> = steps
        .iter()
        .map(|step| step["kind"].as_str().unwrap_or("?"))
        .collect();
    assert!(
        kinds.contains(&"tool_call"),
        "the panel's trace must name the tool call, not fold it into a note: {kinds:?}"
    );
    for step in steps {
        assert_eq!(
            step["status"],
            json!("completed"),
            "step {} is still {} on a finished run",
            step["step_no"],
            step["status"]
        );
    }
    // Two loop steps produce two rows. This is the assertion that catches a one-row-per-event
    // writer, which reported eight steps for a two-step run.
    assert_eq!(
        steps.len(),
        2,
        "one row per loop step, not one per event: the kinds were {kinds:?}"
    );
    // The header's count and the trace it heads are two reads of one fact. Read through the
    // field the view actually publishes (`completed_steps`), not a name invented here.
    assert_eq!(
        detail.body["telemetry"]["completed_steps"],
        json!(2),
        "the header counts the same two steps the trace lists: {:?}",
        detail.body["telemetry"]
    );
    assert_eq!(
        detail.body["prompt_tokens"],
        json!(43),
        "the run row's own token column, written by finish_run from its completed steps"
    );

    harness.dispose().await;
}

/// The trace a person reads: one line per fact, with the answer visible.
///
/// `list_steps` is what the accordion and the transcript copy both read, so a walk that asserted
/// the route alone would miss a step that is well-formed in the database and unreadable here.
#[tokio::test]
async fn the_answer_is_readable_as_text_and_not_only_as_a_serialized_event() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let run = agent_and_run(&harness, "readable").await;
    publish_a_two_step_run(&harness, run).await;

    let steps = run_store::list_steps(harness.db.pool(), run)
        .await
        .expect("the steps must be readable");
    let answer = steps
        .iter()
        .find(|step| step.kind == "message")
        .filter(|step| step.result.is_some())
        .and_then(|step| step.result.as_ref())
        .and_then(|result| result.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            panic!(
                "the model's answer must be readable as text in a result column — {}",
                describe(&steps)
            )
        });
    assert_eq!(answer, "The pricing page is /pricing.");

    harness.dispose().await;
}

/// One line per step for a failure message, so a red assertion names what the trace actually
/// holds rather than asking the reader to open the database.
fn describe(steps: &[run_store::Step]) -> String {
    steps
        .iter()
        .map(|step| {
            format!(
                "#{} kind={} tool={:?} status={} args={:?}",
                step.step_no,
                step.kind,
                step.tool,
                step.status,
                step.arguments
            )
        })
        .collect::<Vec<_>>()
        .join("\n    ")
}
