//! Integration tests for the automation layer (phase P13, docs/requests/REQ-003).
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) on a **throwaway database**,
//! and the walk is the real one: a rule is written through `/api/v1/automations`, a page is
//! published through `/api/v1/pages` (which records `page.published` on the bus), the matcher
//! reads the bus and starts the run, the workflow engine advances its steps, and the two actions
//! do what they say — one sends an email to an **SMTP server this suite starts on a loopback
//! port**, the other leaves a comment the API hands back.
//!
//! What they prove, one acceptance criterion per walk:
//!
//! * a rule fires on its event, its conditions hold, its `{{event.*}}` bindings resolve into the
//!   run's steps, both actions succeed, and the trail (audits, trigger counter) agrees;
//! * a rule whose conditions do not hold — and an event nobody listens for — starts nothing, and
//!   the cursor still moves;
//! * a binding the event cannot fill refuses to start the run and says why;
//! * a switched-off mail server fails the step with that reason instead of pretending;
//! * the surface is permission-gated and tenant-scoped.
//!
//! When PostgreSQL is not reachable the suite skips itself with a printed reason.

use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_automation::{AutomationActions, MailSettings, matcher};
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use omnion_storage::Storage;
use omnion_workflows::engine::{self, RunnerConfig};
use omnion_workflows::{ExecutionStatus, store};
use serde_json::{Value, json};
use time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tower::ServiceExt as _;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// Permission keys the automation operator of this suite holds.
const AUTOMATION_PERMISSIONS: [&str; 3] = ["workflows.read", "workflows.manage", "workflows.run"];

/// How long a walk waits for its run to settle.
const SETTLE_BUDGET: usize = 60;

// ---------------------------------------------------------------------------------------------
// The SMTP sink: a real server on an ephemeral loopback port
// ---------------------------------------------------------------------------------------------

/// One message the sink received.
#[derive(Debug, Clone, PartialEq)]
struct Captured {
    /// The command lines, in order.
    commands: Vec<String>,
    /// The message body (headers included), CRLF line endings intact.
    data: String,
}

/// A running SMTP sink.
struct SmtpSink {
    /// The port the sink listens on.
    port: u16,
    /// Everything it received so far.
    captured: Arc<Mutex<Vec<Captured>>>,
    /// The accept loop (dropped with the test process).
    _task: JoinHandle<()>,
}

impl SmtpSink {
    /// Start the sink on an ephemeral port.
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the smtp sink must bind a port");
        let port = listener
            .local_addr()
            .expect("the sink has an address")
            .port();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let shared = captured.clone();

        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let shared = shared.clone();
                tokio::spawn(async move { serve(stream, shared).await });
            }
        });

        Self {
            port,
            captured,
            _task: task,
        }
    }

    /// Everything the sink captured.
    fn messages(&self) -> Vec<Captured> {
        self.captured.lock().expect("the sink lock").clone()
    }

    /// The mail settings of a sender pointed at this sink.
    fn settings(&self) -> MailSettings {
        MailSettings::new("127.0.0.1", self.port, "omnion@localhost")
    }
}

/// Speak SMTP to one client until it says QUIT.
async fn serve(stream: TcpStream, captured: Arc<Mutex<Vec<Captured>>>) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    writer
        .write_all(b"220 omnion test sink ready\r\n")
        .await
        .ok();

    let mut commands: Vec<String> = Vec::new();
    let mut data = String::new();
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
            break;
        }
        let command = line.trim_end().to_owned();
        let upper = command.to_ascii_uppercase();
        commands.push(command);

        if upper.starts_with("EHLO") {
            writer
                .write_all(b"250-sink\r\n250-SIZE 102400\r\n250 AUTH PLAIN\r\n")
                .await
                .ok();
        } else if upper.starts_with("AUTH") {
            writer.write_all(b"235 authenticated\r\n").await.ok();
        } else if upper.starts_with("MAIL FROM") || upper.starts_with("RCPT TO") {
            writer.write_all(b"250 ok\r\n").await.ok();
        } else if upper == "DATA" {
            writer.write_all(b"354 go ahead\r\n").await.ok();
            loop {
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                if line == ".\r\n" {
                    break;
                }
                data.push_str(&line);
            }
            writer.write_all(b"250 queued\r\n").await.ok();
        } else if upper == "QUIT" {
            writer.write_all(b"221 bye\r\n").await.ok();
            break;
        }
    }

    captured
        .lock()
        .expect("the sink lock")
        .push(Captured { commands, data });
}

// ---------------------------------------------------------------------------------------------
// The harness: a throwaway database with every migration applied
// ---------------------------------------------------------------------------------------------

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    body: Value,
}

/// A throwaway database, its router, and the rows the fixture wrote.
struct Harness {
    state: AppState,
    db: Db,
    maintenance: Db,
    database: String,
}

impl Harness {
    /// Open a fresh database with every migration applied and the IAM seed loaded.
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().expect("environment must be valid");
        live_db(&config).await?;

        let database = format!("omnion_automation_{}", Uuid::new_v4().simple());
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

    /// Drive the router without a network socket.
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
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };

        TestResponse { status, body }
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

/// A GET request, optionally with a session cookie.
fn get(uri: &str, token: Option<&str>) -> Request<Body> {
    request(Method::GET, uri, token, None)
}

/// A POST request carrying a JSON body.
fn post(uri: &str, body: Value, token: Option<&str>) -> Request<Body> {
    request(Method::POST, uri, token, Some(body))
}

/// A PUT request carrying a JSON body.
fn put(uri: &str, body: Value, token: Option<&str>) -> Request<Body> {
    request(Method::PUT, uri, token, Some(body))
}

/// A DELETE request.
fn delete(uri: &str, token: Option<&str>) -> Request<Body> {
    request(Method::DELETE, uri, token, None)
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

/// Create an account with a session and return `(user id, token)`.
async fn account(harness: &Harness, organization_id: Option<Uuid>) -> (Uuid, String) {
    let user = users::create_user(
        harness.db.pool(),
        NewUser {
            email: format!("automation-{}@omnion.test", Uuid::new_v4().simple()),
            password: PASSWORD.to_owned(),
            display_name: "Walk".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    let (_, token) =
        omnion_identity::sessions::create_session(harness.db.pool(), user.id, None, None)
            .await
            .expect("the session must be created");
    (user.id, token)
}

/// Bind a role with exactly these permission keys to one account, at organization scope.
async fn grant(harness: &Harness, user_id: Uuid, organization_id: Uuid, keys: &[&str]) {
    let role = role_store::create_role(
        harness.db.pool(),
        NewRole {
            organization_id,
            key: format!("automation-walk-{}", Uuid::new_v4().simple()),
            name: "Automation Walk".to_owned(),
            description: "The keys one walk needs".to_owned(),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the organization role must be created");

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

/// Create an organization row with a unique, suite-scoped slug.
async fn create_organization_row(db: &Db, label: &str, name: &str) -> Uuid {
    let slug = format!("automation-walk-{label}-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(name)
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// Create a site row inside an organization.
async fn create_site_row(db: &Db, organization_id: Uuid, key: &str, name: &str) -> Uuid {
    sqlx::query_scalar(
        "insert into sites (organization_id, key, name) values ($1, $2, $3) returning id",
    )
    .bind(organization_id)
    .bind(key)
    .bind(name)
    .fetch_one(db.pool())
    .await
    .expect("the test site must be created")
}

/// The engine configuration the walks use: a fast backoff so retries are observable.
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

/// Create a page through the API and publish it.
///
/// Answers `(page_id, revision_id, revision_no)` of the revision the publication froze.
async fn publish_a_page(
    harness: &Harness,
    token: &str,
    site_id: Uuid,
    slug: &str,
) -> (Uuid, Uuid, i64) {
    let created = harness
        .call(post(
            "/api/v1/pages",
            json!({ "site_id": site_id, "slug": slug, "title": "Release notes" }),
            Some(token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let page_id = Uuid::parse_str(created.body["id"].as_str().expect("page id")).expect("uuid");

    let published = harness
        .call(post(
            &format!("/api/v1/pages/{page_id}/publish"),
            Value::Null,
            Some(token),
        ))
        .await;
    assert_eq!(published.status, StatusCode::OK, "{:?}", published.body);

    // The history is the platform's own answer to "which revision is live".
    let revisions = harness
        .call(get(
            &format!("/api/v1/pages/{page_id}/revisions"),
            Some(token),
        ))
        .await;
    assert_eq!(revisions.status, StatusCode::OK, "{:?}", revisions.body);
    let newest = &revisions.body["revisions"][0];
    let revision_id = Uuid::parse_str(newest["id"].as_str().expect("revision id")).expect("uuid");
    let revision_no = newest["revision_no"].as_i64().expect("revision number");

    (page_id, revision_id, revision_no)
}

/// Run the engine until the given run reaches a terminal state.
async fn drive_until_settled(
    harness: &Harness,
    actions: &AutomationActions,
    execution_id: Uuid,
) -> ExecutionStatus {
    for _ in 0..SETTLE_BUDGET {
        engine::tick_with(harness.db.pool(), &runner_config(), actions)
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

/// Count audit rows of one action for one target.
async fn audit_rows(harness: &Harness, action: &str, target_id: &str) -> i64 {
    sqlx::query_scalar("select count(*) from audit_log where action = $1 and target_id = $2")
        .bind(action)
        .bind(target_id)
        .fetch_one(harness.db.pool())
        .await
        .expect("the audit log must be readable")
}

/// A rule that mails the editor and comments on the published revision.
fn welcome_rule(organization_id: Uuid, site_id: Uuid) -> Value {
    json!({
        "organization_id": organization_id,
        "site_id": site_id,
        "name": "Welcome the editor",
        "description": "Tells the editor a page went live.",
        "event": "page.published",
        "conditions": [
            { "field": "status", "operator": "equals", "value": "published" },
            { "field": "title", "operator": "contains", "value": "Release" }
        ],
        "actions": [
            {
                "name": "tell the editor",
                "kind": "task",
                "action": "send_email",
                "params": {
                    "to": "editor@example.com",
                    "subject": "Published: {{event.title}}",
                    "body": "{{event.slug}} is live at revision {{event.revision_no}}."
                },
                "max_attempts": 3
            },
            {
                "name": "note it on the revision",
                "kind": "task",
                "action": "comment_revision",
                "params": {
                    "revision_id": "{{event.revision_id}}",
                    "body": "Automation: {{event.title}} went live."
                },
                "max_attempts": 1
            }
        ]
    })
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_automation_runs_end_to_end_on_a_page_published_trigger() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let sink = SmtpSink::start().await;

    // The platform Owner: the wizard's account, with no primary organization.
    let (owner_id, owner_token) = account(&harness, None).await;
    seed::bind_owner(harness.db.pool(), owner_id)
        .await
        .expect("the owner binding must be created");

    let organization = create_organization_row(&harness.db, "a", "Automation Test A").await;
    let site = create_site_row(&harness.db, organization, "main", "Automation Site").await;

    // Nothing on the surface without a session.
    assert_eq!(
        harness.call(get("/api/v1/automations", None)).await.status,
        StatusCode::UNAUTHORIZED
    );

    // The vocabulary the builder reads: a closed set, and the two real actions are marked.
    let catalogue = harness
        .call(get("/api/v1/automations/catalogue", Some(&owner_token)))
        .await;
    assert_eq!(catalogue.status, StatusCode::OK, "{:?}", catalogue.body);
    assert_eq!(catalogue.body["events"][0], "page.published");
    assert!(
        catalogue.body["actions"]
            .as_array()
            .expect("actions")
            .iter()
            .any(|action| action["key"] == "send_email" && action["host"] == true),
        "{:?}",
        catalogue.body["actions"]
    );
    println!(
        "catalogue: events={:?} operators={} actions={}",
        catalogue.body["events"],
        catalogue.body["condition_operators"]
            .as_array()
            .map(Vec::len)
            .unwrap_or_default(),
        catalogue.body["actions"]
            .as_array()
            .map(Vec::len)
            .unwrap_or_default()
    );

    // Write the rule.
    let created = harness
        .call(post(
            "/api/v1/automations",
            welcome_rule(organization, site),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let automation_id = created.body["id"]
        .as_str()
        .expect("automation id")
        .to_owned();
    assert_eq!(created.body["event"], "page.published");
    assert_eq!(created.body["conditions"].as_array().map(Vec::len), Some(2));
    assert_eq!(created.body["actions"].as_array().map(Vec::len), Some(2));
    assert_eq!(created.body["trigger_count"], 0);
    println!(
        "automation: id={} event={} conditions={} actions={}",
        automation_id, created.body["event"], created.body["conditions"], created.body["actions"]
    );

    // The matcher runs since boot: a never-advanced cursor is seeded to the end of the bus, so
    // only what happens from now on can fire a rule. On an empty bus that seed lands on 0, which
    // is exactly why the drain reads every later event instead of replaying a history.
    assert_eq!(
        matcher::seed_cursor(harness.db.pool())
            .await
            .expect("the cursor must seed"),
        Some(0),
        "nothing is on the bus yet"
    );

    // A page is published: the bus records `page.published`.
    let (page_id, revision_id, revision_no) =
        publish_a_page(&harness, &owner_token, site, "release-notes").await;

    // One matcher tick: the fact meets the rule.
    let report = matcher::drain(harness.db.pool(), 100)
        .await
        .expect("the matcher must run");
    assert_eq!(report.evaluated, 1, "one event was evaluated");
    assert_eq!(report.matched, 1, "the rule matched: {report:?}");
    assert_eq!(report.skipped, 0);
    assert_eq!(report.runs.len(), 1);
    let execution_id = report.runs[0];
    println!(
        "matcher: evaluated={} matched={} skipped={} run={}",
        report.evaluated, report.matched, report.skipped, execution_id
    );

    // The run carries the *resolved* steps: the bindings became the values of the event.
    let steps = store::list_steps(harness.db.pool(), execution_id)
        .await
        .expect("the steps must be readable");
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].action.as_deref(), Some("send_email"));
    assert_eq!(steps[0].params["subject"], "Published: Release notes");
    assert_eq!(steps[1].action.as_deref(), Some("comment_revision"));
    assert_eq!(
        steps[1].params["revision_id"],
        revision_id.to_string(),
        "the placeholder became the published revision"
    );

    // The engine advances the steps with the process's host actions installed.
    let actions = AutomationActions::new(harness.db.pool().clone(), sink.settings());
    let status = drive_until_settled(&harness, &actions, execution_id).await;
    assert_eq!(status, ExecutionStatus::Completed);

    let steps = store::list_steps(harness.db.pool(), execution_id)
        .await
        .expect("the steps must be readable");
    assert_eq!(steps[0].status, "succeeded", "{:?}", steps[0].error);
    assert_eq!(steps[1].status, "succeeded", "{:?}", steps[1].error);
    assert_eq!(
        steps[0].output.as_ref().expect("output")["to"],
        "editor@example.com"
    );
    println!(
        "engine: run={:?} step1={:?} step2={:?}",
        status.as_str(),
        steps[0].output.as_ref().expect("output"),
        steps[1].output.as_ref().expect("output")
    );

    // The email really travelled: the sink read a message on the wire.
    let messages = sink.messages();
    assert_eq!(messages.len(), 1, "the sink saw exactly one message");
    let message = &messages[0];
    assert!(
        message
            .data
            .contains("Subject: Published: Release notes\r\n"),
        "{}",
        message.data
    );
    assert!(
        message.data.contains("To: editor@example.com\r\n"),
        "{}",
        message.data
    );
    assert!(
        message
            .data
            .contains(&format!("release-notes is live at revision {revision_no}.")),
        "{}",
        message.data
    );
    assert!(
        message
            .commands
            .iter()
            .any(|c| c == "MAIL FROM:<omnion@localhost>"),
        "{:?}",
        message.commands
    );
    println!(
        "sink: to=editor@example.com subject=\"Published: Release notes\" bytes={} commands={}",
        message.data.len(),
        message.commands.len()
    );

    // The comment the action wrote is readable through the content surface.
    let comments = harness
        .call(get(
            &format!("/api/v1/pages/{page_id}/revisions/{revision_id}/comments"),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(comments.status, StatusCode::OK, "{:?}", comments.body);
    let written = comments.body["comments"].as_array().expect("comments");
    assert_eq!(written.len(), 1, "{:?}", comments.body);
    assert_eq!(written[0]["source"], "automation");
    assert_eq!(written[0]["author_user_id"], Value::Null);
    assert_eq!(written[0]["body"], "Automation: Release notes went live.");
    println!("comments: {:?}", comments.body["comments"]);

    // The rule remembers that it fired, and both sides of the match are in the audit trail.
    let after = harness
        .call(get(
            &format!("/api/v1/automations/{automation_id}"),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(after.status, StatusCode::OK, "{:?}", after.body);
    assert_eq!(after.body["trigger_count"], 1);
    assert!(after.body["last_triggered_at"].is_string());

    assert_eq!(
        audit_rows(&harness, "automation.created", &automation_id).await,
        1
    );
    assert_eq!(
        audit_rows(
            &harness,
            "automation.rule.matched",
            &execution_id.to_string()
        )
        .await,
        1
    );
    assert_eq!(
        audit_rows(
            &harness,
            "workflow.execution.started",
            &execution_id.to_string()
        )
        .await,
        1
    );
    assert_eq!(
        audit_rows(
            &harness,
            "workflow.execution.completed",
            &execution_id.to_string()
        )
        .await,
        1
    );

    // The cursor moved past the event: a second tick has nothing to do.
    let second = matcher::drain(harness.db.pool(), 100)
        .await
        .expect("the second tick must run");
    assert!(second.is_idle(), "{second:?}");
    assert_eq!(
        matcher::event_cursor(harness.db.pool())
            .await
            .expect("the cursor must read"),
        report.cursor
    );

    harness.dispose().await;
}

#[tokio::test]
async fn a_rule_that_does_not_hold_and_an_event_nobody_listens_for_start_nothing() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    let (owner_id, owner_token) = account(&harness, None).await;
    seed::bind_owner(harness.db.pool(), owner_id)
        .await
        .expect("the owner binding must be created");

    let organization = create_organization_row(&harness.db, "b", "Automation Test B").await;
    let site = create_site_row(&harness.db, organization, "main", "Automation Site").await;

    // A rule that only fires for a title this walk is not going to publish.
    let mut body = welcome_rule(organization, site);
    body["name"] = json!("Only the other title");
    body["conditions"] = json!([
        { "field": "title", "operator": "equals", "value": "Something else entirely" }
    ]);
    let created = harness
        .call(post("/api/v1/automations", body, Some(&owner_token)))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);

    // A second rule that listens for an event this walk never records.
    let mut other_event = welcome_rule(organization, site);
    other_event["name"] = json!("Another event");
    other_event["event"] = json!("manga.updated");
    other_event["conditions"] = json!([]);
    let created = harness
        .call(post("/api/v1/automations", other_event, Some(&owner_token)))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);

    publish_a_page(&harness, &owner_token, site, "draft-notes").await;

    let report = matcher::drain(harness.db.pool(), 100)
        .await
        .expect("the matcher must run");
    assert_eq!(report.evaluated, 1);
    assert_eq!(report.matched, 0, "nothing fired: {report:?}");
    assert_eq!(report.skipped, 1, "the rule was evaluated and did not hold");

    // No run exists for either rule.
    let executions: i64 =
        sqlx::query_scalar("select count(*) from workflow_executions where trigger_kind = 'event'")
            .fetch_one(harness.db.pool())
            .await
            .expect("the runs must be countable");
    assert_eq!(executions, 0);

    // …and the cursor still moved, so the next event is the one being watched.
    assert_eq!(report.cursor, 1);
    let second = matcher::drain(harness.db.pool(), 100)
        .await
        .expect("the second tick must run");
    assert!(second.is_idle(), "{second:?}");

    harness.dispose().await;
}

#[tokio::test]
async fn a_binding_the_event_cannot_fill_refuses_to_start_the_run() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    let (owner_id, owner_token) = account(&harness, None).await;
    seed::bind_owner(harness.db.pool(), owner_id)
        .await
        .expect("the owner binding must be created");

    let organization = create_organization_row(&harness.db, "c", "Automation Test C").await;
    let site = create_site_row(&harness.db, organization, "main", "Automation Site").await;

    // `{{event.author}}` is a well-shaped binding, but the publication payload has no author —
    // exactly the sort of rule that would otherwise send an empty value.
    let mut body = welcome_rule(organization, site);
    body["name"] = json!("Reads a field the event does not carry");
    body["conditions"] = json!([]);
    body["actions"] = json!([
        {
            "name": "tell the editor",
            "kind": "task",
            "action": "send_email",
            "params": {
                "to": "editor@example.com",
                "subject": "By {{event.author}}",
                "body": "A page went live."
            },
            "max_attempts": 1
        }
    ]);
    let created = harness
        .call(post("/api/v1/automations", body, Some(&owner_token)))
        .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "the shape is valid, so the rule is stored: {:?}",
        created.body
    );
    let automation_id = created.body["id"]
        .as_str()
        .expect("automation id")
        .to_owned();

    publish_a_page(&harness, &owner_token, site, "unfillable").await;

    let report = matcher::drain(harness.db.pool(), 100)
        .await
        .expect("the matcher must run");
    assert_eq!(report.evaluated, 1);
    assert_eq!(
        report.matched, 0,
        "no run may start with an unfilled binding"
    );
    assert_eq!(report.skipped, 1);

    let executions: i64 =
        sqlx::query_scalar("select count(*) from workflow_executions where trigger_kind = 'event'")
            .fetch_one(harness.db.pool())
            .await
            .expect("the runs must be countable");
    assert_eq!(executions, 0);

    // The refusal is in the trail, naming the rule and the field.
    let skips = audit_rows(&harness, "automation.rule.skipped", &automation_id).await;
    assert_eq!(skips, 1, "the skip is audited");
    let reason: String = sqlx::query_scalar(
        "select metadata->>'reason' from audit_log \
         where action = 'automation.rule.skipped' and target_id = $1",
    )
    .bind(&automation_id)
    .fetch_one(harness.db.pool())
    .await
    .expect("the reason must be readable");
    assert!(reason.contains("author"), "{reason}");
    assert!(
        reason.contains("slug"),
        "the message lists what the event carries: {reason}"
    );
    println!("skipped: {reason}");

    harness.dispose().await;
}

#[tokio::test]
async fn a_switched_off_mail_server_fails_the_step_with_that_reason() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    let (owner_id, owner_token) = account(&harness, None).await;
    seed::bind_owner(harness.db.pool(), owner_id)
        .await
        .expect("the owner binding must be created");

    let organization = create_organization_row(&harness.db, "d", "Automation Test D").await;
    let site = create_site_row(&harness.db, organization, "main", "Automation Site").await;

    let mut body = welcome_rule(organization, site);
    body["name"] = json!("Mail is off");
    body["conditions"] = json!([]);
    body["actions"] = json!([
        {
            "name": "tell the editor",
            "kind": "task",
            "action": "send_email",
            "params": {
                "to": "editor@example.com",
                "subject": "Published: {{event.title}}",
                "body": "{{event.slug}} is live."
            },
            "max_attempts": 1
        }
    ]);
    let created = harness
        .call(post("/api/v1/automations", body, Some(&owner_token)))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);

    publish_a_page(&harness, &owner_token, site, "mail-off").await;
    let report = matcher::drain(harness.db.pool(), 100)
        .await
        .expect("the matcher must run");
    assert_eq!(report.matched, 1, "{report:?}");
    let execution_id = report.runs[0];

    // A process whose platform email is switched off: the step must say so, not silently do
    // nothing and not fail with a connection error nobody can act on.
    let actions = AutomationActions::new(
        harness.db.pool().clone(),
        MailSettings::new("127.0.0.1", 1, "omnion@localhost").with_sending(false),
    );
    let status = drive_until_settled(&harness, &actions, execution_id).await;
    assert_eq!(status, ExecutionStatus::Failed);

    let steps = store::list_steps(harness.db.pool(), execution_id)
        .await
        .expect("the steps must be readable");
    let error = steps[0].error.clone().unwrap_or_default();
    assert!(error.contains("switched off"), "{error}");
    assert!(error.contains("OMNION_MAIL_ENABLED"), "{error}");
    println!("engine: run=failed step_error={error:?}");

    // And the run's failure is audited like any other.
    assert_eq!(
        audit_rows(
            &harness,
            "workflow.execution.failed",
            &execution_id.to_string()
        )
        .await,
        1
    );

    harness.dispose().await;
}

#[tokio::test]
async fn the_automation_surface_is_permission_gated_and_tenant_scoped() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    let (owner_id, owner_token) = account(&harness, None).await;
    seed::bind_owner(harness.db.pool(), owner_id)
        .await
        .expect("the owner binding must be created");

    let alpha = create_organization_row(&harness.db, "e", "Automation Test E").await;
    let beta = create_organization_row(&harness.db, "f", "Automation Test F").await;
    let alpha_site = create_site_row(&harness.db, alpha, "main", "Alpha Site").await;

    // The editor works inside alpha with the automation keys.
    let (editor_id, editor_token) = account(&harness, Some(alpha)).await;
    grant(&harness, editor_id, alpha, &AUTOMATION_PERMISSIONS).await;

    // A member of alpha without the keys.
    let (member_id, member_token) = account(&harness, Some(alpha)).await;
    grant(&harness, member_id, alpha, &["content.pages.read"]).await;

    // The editor may write a rule of their own organization…
    let created = harness
        .call(post(
            "/api/v1/automations",
            welcome_rule(alpha, alpha_site),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let rule_id = created.body["id"].as_str().expect("rule id").to_owned();

    // … and read it back.
    assert_eq!(
        harness
            .call(get(
                &format!("/api/v1/automations/{rule_id}"),
                Some(&editor_token)
            ))
            .await
            .status,
        StatusCode::OK
    );

    // A rule of another organization is out of reach, and so is the catalogue without a key.
    assert_eq!(
        harness
            .call(get(
                &format!("/api/v1/automations?organization_id={beta}"),
                Some(&editor_token)
            ))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        harness
            .call(get("/api/v1/automations/catalogue", Some(&member_token)))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        harness
            .call(post(
                "/api/v1/automations",
                welcome_rule(alpha, alpha_site),
                Some(&member_token)
            ))
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    // A platform account reaches a rule of any tenant it names.
    assert_eq!(
        harness
            .call(get(
                &format!("/api/v1/automations/{rule_id}"),
                Some(&owner_token)
            ))
            .await
            .status,
        StatusCode::OK
    );

    // A workflow that is not an automation is not on this surface at all.
    let manual = harness
        .call(post(
            "/api/v1/workflows",
            json!({
                "organization_id": alpha,
                "name": "Not an automation",
                "trigger": { "kind": "manual" },
                "steps": [
                    { "name": "prepare", "kind": "task", "action": "noop", "params": {} }
                ]
            }),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(manual.status, StatusCode::CREATED, "{:?}", manual.body);
    let manual_id = manual.body["id"].as_str().expect("workflow id").to_owned();
    assert_eq!(
        harness
            .call(get(
                &format!("/api/v1/automations/{manual_id}"),
                Some(&owner_token)
            ))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // A rule may be replaced as a whole: the change is stored and audited.
    let updated = harness
        .call(put(
            &format!("/api/v1/automations/{rule_id}"),
            json!({
                "name": "Welcome the editor (quiet)",
                "description": "Now only comments.",
                "enabled": false,
                "event": "page.published",
                "conditions": [],
                "actions": [
                    {
                        "name": "note it",
                        "kind": "task",
                        "action": "comment_revision",
                        "params": { "revision_id": "{{event.revision_id}}", "body": "live" },
                        "max_attempts": 1
                    }
                ]
            }),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(updated.status, StatusCode::OK, "{:?}", updated.body);
    assert_eq!(updated.body["enabled"], false);
    assert_eq!(updated.body["conditions"].as_array().map(Vec::len), Some(0));
    assert_eq!(updated.body["actions"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        audit_rows(&harness, "automation.updated", &rule_id).await,
        1
    );

    // Removal closes the surface for it.
    assert_eq!(
        harness
            .call(delete(
                &format!("/api/v1/automations/{rule_id}"),
                Some(&editor_token)
            ))
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        harness
            .call(get(
                &format!("/api/v1/automations/{rule_id}"),
                Some(&owner_token)
            ))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        audit_rows(&harness, "automation.deleted", &rule_id).await,
        1
    );

    harness.dispose().await;
}

// ---------------------------------------------------------------------------------------------
// Stack helpers
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
