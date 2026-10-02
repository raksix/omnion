//! The integration walk for REQ-003 **slice 2** — the action library's outbound half and the
//! run-detail controls (docs/requests/REQ-003).
//!
//! It shares the harness of `automation.rs` (the same throwaway database, the same engine
//! driver) and proves, one acceptance criterion per walk:
//!
//! * an `http_request` to a host outside `automation_settings.http_allowed_hosts` is
//!   **refused at write time, naming the host**, and never leaves the process;
//! * the same step to an allowed host is **delivered** and arrives carrying
//!   `x-omnion-signature`, `x-omnion-timestamp` and `x-omnion-run` — a receiver that
//!   re-derives the HMAC with the rule's key accepts it, and a different key does not;
//! * a `branch` step that does not hold **ends the run there**, the steps after it are closed
//!   as cancelled, and the run settles as *completed* (a branch that decided is not a
//!   failure); a branch that holds lets the run go on;
//! * a `stop` step ends the run with its reason in the trace;
//! * a step whose `on_error` is `continue` fails, keeps its failed row, and the run still
//!   completes — while a step that inherits the rule's `stop` policy fails the run;
//! * `timeout_ms` is honoured: a step that outlives its budget fails **naming the limit**;
//! * **Retry** re-runs the failed step and everything after it without sending the earlier
//!   e-mail a second time — the mail sink's count is the proof;
//! * a cancelled run cannot be retried.
//!
//! Every assertion reads the platform's own rows or its own HTTP surface: no test-only
//! shortcut reaches into the engine.
//!
//! When PostgreSQL is not reachable the suite skips itself with a printed reason.

use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_automation::outbound;
use omnion_automation::{AutomationActions, MailSettings};
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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
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
// Two loopback sinks: an HTTP receiver and an SMTP receiver
// ---------------------------------------------------------------------------------------------

/// One request the HTTP sink received.
#[derive(Debug, Clone, Default)]
struct Received {
    /// The request line and every header, lower-cased, in order.
    head: Vec<String>,
    /// The body, as text.
    body: String,
    /// The path, without the query.
    path: String,
    /// The method.
    method: String,
}

impl Received {
    /// The value of one header, case-insensitively.
    fn header(&self, name: &str) -> Option<&str> {
        self.head.iter().find_map(|line| {
            line.split_once(": ")
                .filter(|(key, _)| key == &name.to_ascii_lowercase())
                .map(|(_, value)| value)
        })
    }
}

/// A running HTTP sink that answers 200 to everything and records what arrived.
struct HttpSink {
    port: u16,
    received: Arc<Mutex<Vec<Received>>>,
    handle: JoinHandle<()>,
}

impl HttpSink {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the HTTP sink must bind");
        let port = listener.local_addr().expect("an address").port();
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = received.clone();

        let handle = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let sink = sink.clone();
                tokio::spawn(async move {
                    let _ = serve_http(stream, sink).await;
                });
            }
        });

        Self {
            port,
            received,
            handle,
        }
    }

    fn calls(&self) -> Vec<Received> {
        self.received.lock().expect("the sink's lock").clone()
    }
}

impl Drop for HttpSink {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Read one HTTP request off a connection and answer it.
async fn serve_http(mut stream: TcpStream, sink: Arc<Mutex<Vec<Received>>>) -> std::io::Result<()> {
    let mut raw = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buffer[..read]);
        if raw.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
    let mut lines = head.lines();

    let request_line = lines.next().unwrap_or_default().to_owned();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or("/").to_owned();
    let path = target.split('?').next().unwrap_or("/").to_owned();

    let headers: Vec<String> = lines
        .map(|line| {
            let (key, value) = line.split_once(": ").unwrap_or((line, ""));
            format!("{}: {}", key.to_ascii_lowercase(), value)
        })
        .collect();

    // The body is read only up to what actually arrived; the tests send small bodies.
    sink.lock().expect("the sink's lock").push(Received {
        head: headers,
        body: body.to_owned(),
        path,
        method,
    });

    let response = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 13\r\nconnection: close\r\n\r\n{\"ok\":true}\n";
    stream.write_all(response).await?;
    stream.flush().await
}

/// One message the SMTP sink received.
#[derive(Debug, Clone, PartialEq)]
struct Captured {
    commands: Vec<String>,
    data: String,
}

struct SmtpSink {
    port: u16,
    received: Arc<Mutex<Vec<Captured>>>,
    handle: JoinHandle<()>,
}

impl SmtpSink {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the SMTP sink must bind");
        let port = listener.local_addr().expect("an address").port();
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = received.clone();

        let handle = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let sink = sink.clone();
                tokio::spawn(async move {
                    let _ = serve_smtp(stream, sink).await;
                });
            }
        });

        Self {
            port,
            received,
            handle,
        }
    }

    fn messages(&self) -> Vec<Captured> {
        self.received.lock().expect("the sink's lock").clone()
    }
}

impl Drop for SmtpSink {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn serve_smtp(stream: TcpStream, sink: Arc<Mutex<Vec<Captured>>>) {
    // The greeting comes first and the reader/writer are split: an SMTP client that
    // connects and waits for `220` before it says anything will hang on a sink that only
    // replies to commands, and the hang looks exactly like "the mail server is down".
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

    sink.lock()
        .expect("the sink's lock")
        .push(Captured { commands, data });
}

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

impl Harness {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().expect("environment must be valid");
        if live_db(&config).await.is_none() {
            return None;
        }

        let database = format!("omnion_automation2_{}", Uuid::new_v4().simple());
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
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };

        TestResponse { status, body }
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

fn get(uri: &str, token: Option<&str>) -> Request<Body> {
    request(Method::GET, uri, token, None)
}

fn post(uri: &str, body: Value, token: Option<&str>) -> Request<Body> {
    request(Method::POST, uri, token, Some(body))
}

fn put(uri: &str, body: Value, token: Option<&str>) -> Request<Body> {
    request(Method::PUT, uri, token, Some(body))
}

fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
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

/// Create an account and a session for it.
async fn account(harness: &Harness, organization_id: Option<Uuid>) -> (Uuid, String) {
    let user = users::create_user(
        harness.db.pool(),
        NewUser {
            email: format!("actions-{}@omnion.test", Uuid::new_v4().simple()),
            password: PASSWORD.to_owned(),
            display_name: "Actions Walk".to_owned(),
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
            key: format!("actions-walk-{}", Uuid::new_v4().simple()),
            name: "Actions Walk".to_owned(),
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

async fn create_organization_row(db: &Db, label: &str, name: &str) -> Uuid {
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(name)
        .bind(format!("{label}-{}", Uuid::new_v4()))
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

async fn drive_until_settled(
    harness: &Harness,
    actions: &AutomationActions,
    execution_id: Uuid,
) -> ExecutionStatus {
    for _ in 0..SETTLE_BUDGET {
        // `NoRunGuard`: these walks drive the engine's own steps, so the endless-loop guard
        // under test elsewhere has nothing to say about them and is left out on purpose.
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
                "ignored": step.ignored,
                "error": step.error,
                "output": step.output,
            })
        })
        .collect()
}

/// The number of times the SMTP sink saw a message to `to`.
fn mail_count(sink: &SmtpSink, to: &str) -> usize {
    sink.messages()
        .iter()
        .filter(|message| message.data.contains(to))
        .count()
}

/// Put a host into the outbound allow-list.
async fn allow_host(harness: &Harness, host: &str) {
    sqlx::query("update automation_settings set http_allowed_hosts = array[$1] where id = 1")
        .bind(host)
        .execute(harness.db.pool())
        .await
        .expect("the settings row must be writable");
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_disallowed_host_is_refused_at_write_time_and_never_leaves_the_process() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let organization_id = create_organization_row(&harness.db, "denied", "Denied").await;
    let site_id = create_site_row(&harness.db, organization_id, "main", "Main").await;
    let (user_id, token) = account(&harness, Some(organization_id)).await;
    grant(&harness, user_id, organization_id, &AUTOMATION_PERMISSIONS).await;

    let sink = HttpSink::start().await;
    let url = format!("http://127.0.0.1:{}/hook", sink.port);

    // The allow-list starts empty on a fresh database, and "nothing decided" must mean
    // "no host may be reached" rather than "any host".
    let refused = harness
        .call(post(
            "/api/v1/automations",
            json!({
                "organization_id": organization_id,
                "site_id": site_id,
                "name": "Ping an outside host",
                "event": "page.published",
                "actions": [{
                    "name": "call",
                    "kind": "task",
                    "action": "http_request",
                    "params": { "url": url, "method": "POST", "body": { "ok": true } }
                }]
            }),
            Some(&token),
        ))
        .await;

    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "a rule the platform could never run is refused when it is written: {:?}",
        refused.body
    );
    let message = refused.body["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        message.contains("127.0.0.1"),
        "the refusal names the host: {message}"
    );
    assert!(
        message.contains("no host is allowed"),
        "and says what an administrator has to do: {message}"
    );
    assert!(
        sink.calls().is_empty(),
        "nothing reached the receiver: the rule never existed"
    );

    // Now the same rule with the host allowed: it saves, and the dry run says what it
    // *would* call — without calling it.
    allow_host(&harness, "127.0.0.1").await;
    let written = harness
        .call(post(
            "/api/v1/automations",
            json!({
                "organization_id": organization_id,
                "site_id": site_id,
                "name": "Ping an allowed host",
                "event": "page.published",
                "actions": [{
                    "name": "call",
                    "kind": "task",
                    "action": "http_request",
                    "params": { "url": url, "method": "POST", "body": { "ok": true } }
                }]
            }),
            Some(&token),
        ))
        .await;
    assert_eq!(written.status, StatusCode::CREATED, "{:?}", written.body);
    assert!(
        sink.calls().is_empty(),
        "writing a rule does not call anything"
    );

    let automation_id = written.body["id"].as_str().expect("an id");
    let dry = harness
        .call(post(
            &format!("/api/v1/automations/{automation_id}/test"),
            json!({ "payload": { "status": "published" } }),
            Some(&token),
        ))
        .await;
    assert_eq!(dry.status, StatusCode::OK, "{:?}", dry.body);
    assert_eq!(
        dry.body["report"]["actions"][0]["outcome"], "would_call",
        "the dry run reports the call it would make"
    );
    assert!(
        sink.calls().is_empty(),
        "a dry run never calls: that is what makes it safe on an armed rule"
    );

    harness.dispose().await;
}

#[tokio::test]
async fn an_allowed_host_is_called_and_the_signature_verifies() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let organization_id = create_organization_row(&harness.db, "signed", "Signed").await;
    let site_id = create_site_row(&harness.db, organization_id, "main", "Main").await;
    let (user_id, token) = account(&harness, Some(organization_id)).await;
    grant(&harness, user_id, organization_id, &AUTOMATION_PERMISSIONS).await;

    let sink = HttpSink::start().await;
    allow_host(&harness, "127.0.0.1").await;

    let written = harness
        .call(post(
            "/api/v1/automations",
            json!({
                "organization_id": organization_id,
                "site_id": site_id,
                "name": "Call the receiver",
                "event": "page.published",
                "actions": [{
                    "name": "call",
                    "kind": "task",
                    "action": "http_request",
                    "params": {
                        "url": format!("http://127.0.0.1:{}/hook", sink.port),
                        "method": "POST",
                        "body": { "order": 7 }
                    }
                }]
            }),
            Some(&token),
        ))
        .await;
    assert_eq!(written.status, StatusCode::CREATED, "{:?}", written.body);
    let automation_id = written.body["id"].as_str().expect("an id").to_owned();

    // "Run now" on a rule whose conditions are empty: the actions really run.
    let started = harness
        .call(post(
            &format!("/api/v1/automations/{automation_id}/run"),
            Value::Null,
            Some(&token),
        ))
        .await;
    assert_eq!(started.status, StatusCode::ACCEPTED, "{:?}", started.body);
    let execution_id =
        Uuid::parse_str(started.body["execution_id"].as_str().expect("an id")).expect("a run id");

    let actions = AutomationActions::new(
        harness.db.pool().clone(),
        MailSettings::new("127.0.0.1", 1, "omnion@localhost").with_sending(false),
    );
    let status = drive_until_settled(&harness, &actions, execution_id).await;
    assert_eq!(status, ExecutionStatus::Completed);

    let calls = sink.calls();
    assert_eq!(calls.len(), 1, "the receiver was called exactly once");
    let call = &calls[0];
    assert_eq!(call.method, "POST");
    assert_eq!(call.path, "/hook");
    assert!(call.body.contains("\"order\":7"), "the body arrived");

    // The three headers a receiver needs, and the one that proves it is this platform.
    let signature = call.header("x-omnion-signature").expect("a signature");
    let timestamp: i64 = call
        .header("x-omnion-timestamp")
        .expect("a timestamp")
        .parse()
        .expect("a numeric timestamp");
    let run = call.header("x-omnion-run").expect("the run id");
    assert_eq!(
        run,
        execution_id.to_string(),
        "the run is the idempotency key"
    );

    // The rule's key is the one that verifies — a receiver re-derives it and compares.
    let secret: String = sqlx::query_scalar("select hook_secret from workflows where id = $1")
        .bind(Uuid::parse_str(&automation_id).expect("a uuid"))
        .fetch_one(harness.db.pool())
        .await
        .expect("the rule's signing key must be stored");
    assert!(!secret.is_empty(), "the key was minted on first use");

    assert!(
        outbound::verify(
            &secret,
            timestamp,
            "POST",
            "/hook",
            call.body.as_bytes(),
            signature
        ),
        "the receiver can re-derive the signature from the rule's key"
    );
    assert!(
        !outbound::verify(
            "a-different-key",
            timestamp,
            "POST",
            "/hook",
            call.body.as_bytes(),
            signature
        ),
        "and cannot with any other key"
    );
    assert!(
        !outbound::verify(
            &secret,
            timestamp,
            "POST",
            "/hook",
            b"{\"order\":8}",
            signature
        ),
        "nor with a tampered body"
    );

    harness.dispose().await;
}

#[tokio::test]
async fn a_branch_ends_the_run_and_a_stop_says_why() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let organization_id = create_organization_row(&harness.db, "branch", "Branch").await;
    let site_id = create_site_row(&harness.db, organization_id, "main", "Main").await;
    let (user_id, token) = account(&harness, Some(organization_id)).await;
    grant(&harness, user_id, organization_id, &AUTOMATION_PERMISSIONS).await;

    let sink = SmtpSink::start().await;
    let actions = AutomationActions::new(
        harness.db.pool().clone(),
        MailSettings::new("127.0.0.1", sink.port, "omnion@localhost"),
    );

    // Step 2 branches on what step 1 produced, step 3 must never run, and a stop closes
    // the rule that took the other branch.
    let written = harness
        .call(post(
            "/api/v1/automations",
            json!({
                "organization_id": organization_id,
                "site_id": site_id,
                "name": "Branch on the first step",
                "event": "page.published",
                "actions": [
                    { "name": "decide", "kind": "task", "action": "echo",
                      "params": { "value": "no" } },
                    { "name": "only if it said yes", "kind": "branch",
                      "params": { "field": "steps.1.value", "operator": "equals", "value": "yes" } },
                    { "name": "tell the editor", "kind": "task", "action": "send_email",
                      "params": { "to": "editor@example.com", "subject": "Yes", "body": "yes" } }
                ]
            }),
            Some(&token),
        ))
        .await;
    assert_eq!(written.status, StatusCode::CREATED, "{:?}", written.body);
    let automation_id = written.body["id"].as_str().expect("an id").to_owned();

    let started = harness
        .call(post(
            &format!("/api/v1/automations/{automation_id}/run"),
            Value::Null,
            Some(&token),
        ))
        .await;
    assert_eq!(started.status, StatusCode::ACCEPTED, "{:?}", started.body);
    let execution_id =
        Uuid::parse_str(started.body["execution_id"].as_str().expect("an id")).expect("a run id");
    let status = drive_until_settled(&harness, &actions, execution_id).await;

    assert_eq!(
        status,
        ExecutionStatus::Completed,
        "a branch that decided is not a failure: the run did what it was told"
    );

    let steps = steps_of(&harness, execution_id).await;
    assert_eq!(steps[0]["status"], "succeeded");
    assert_eq!(
        steps[1]["status"], "succeeded",
        "the branch itself succeeded"
    );
    assert_eq!(
        steps[1]["output"]["branch"]["holds"], false,
        "and the trace says the comparison did not hold"
    );
    assert_eq!(
        steps[2]["status"], "cancelled",
        "the step after it never ran, and the trace says so"
    );
    assert!(
        mail_count(&sink, "editor@example.com") == 0,
        "so no message was sent — the branch is what stopped it"
    );

    // A stop step, in a rule of its own, with a reason a person can read.
    let stopping = harness
        .call(post(
            "/api/v1/automations",
            json!({
                "organization_id": organization_id,
                "site_id": site_id,
                "name": "Stop on purpose",
                "event": "page.published",
                "actions": [
                    { "name": "decide", "kind": "task", "action": "echo",
                      "params": { "value": "stop" } },
                    { "name": "halt", "kind": "stop",
                      "params": { "reason": "the editor asked for a manual review" } },
                    { "name": "never", "kind": "task", "action": "send_email",
                      "params": { "to": "nobody@example.com", "subject": "x", "body": "x" } }
                ]
            }),
            Some(&token),
        ))
        .await;
    assert_eq!(stopping.status, StatusCode::CREATED, "{:?}", stopping.body);
    let stopping_id = stopping.body["id"].as_str().expect("an id").to_owned();

    let started = harness
        .call(post(
            &format!("/api/v1/automations/{stopping_id}/run"),
            Value::Null,
            Some(&token),
        ))
        .await;
    assert_eq!(started.status, StatusCode::ACCEPTED, "{:?}", started.body);
    let stop_run =
        Uuid::parse_str(started.body["execution_id"].as_str().expect("an id")).expect("a run id");
    assert_eq!(
        drive_until_settled(&harness, &actions, stop_run).await,
        ExecutionStatus::Completed
    );

    let steps = steps_of(&harness, stop_run).await;
    assert_eq!(steps[1]["kind"], "stop");
    assert_eq!(steps[1]["status"], "succeeded");
    assert_eq!(
        steps[1]["output"]["reason"], "the editor asked for a manual review",
        "the trace carries the reason an operator reads"
    );
    assert_eq!(steps[2]["status"], "cancelled");
    assert_eq!(mail_count(&sink, "nobody@example.com"), 0);

    harness.dispose().await;
}

#[tokio::test]
async fn an_ignored_failure_still_completes_and_an_inherited_one_does_not() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let organization_id = create_organization_row(&harness.db, "policy", "Policy").await;
    let site_id = create_site_row(&harness.db, organization_id, "main", "Main").await;
    let (user_id, token) = account(&harness, Some(organization_id)).await;
    grant(&harness, user_id, organization_id, &AUTOMATION_PERMISSIONS).await;

    let sink = SmtpSink::start().await;
    let actions = AutomationActions::new(
        harness.db.pool().clone(),
        MailSettings::new("127.0.0.1", sink.port, "omnion@localhost"),
    );

    // Step 1 always fails; step 2 must still send. The failure is a `fail` action, so the
    // walk needs no outside system to make it fail.
    let written = harness
        .call(post(
            "/api/v1/automations",
            json!({
                "organization_id": organization_id,
                "site_id": site_id,
                "name": "Carry on past a failure",
                "event": "page.published",
                "actions": [
                    { "name": "this one fails", "kind": "task", "action": "fail",
                      "params": { "message": "the upstream is down" },
                      "on_error": "continue", "max_attempts": 1 },
                    { "name": "tell the editor anyway", "kind": "task", "action": "send_email",
                      "params": { "to": "editor@example.com", "subject": "Still ran", "body": "yes" } }
                ]
            }),
            Some(&token),
        ))
        .await;
    assert_eq!(written.status, StatusCode::CREATED, "{:?}", written.body);
    let automation_id = written.body["id"].as_str().expect("an id").to_owned();

    let started = harness
        .call(post(
            &format!("/api/v1/automations/{automation_id}/run"),
            Value::Null,
            Some(&token),
        ))
        .await;
    assert_eq!(started.status, StatusCode::ACCEPTED, "{:?}", started.body);
    let execution_id =
        Uuid::parse_str(started.body["execution_id"].as_str().expect("an id")).expect("a run id");
    assert_eq!(
        drive_until_settled(&harness, &actions, execution_id).await,
        ExecutionStatus::Completed,
        "a run whose only failure was deliberately outlived is not a failure"
    );

    let steps = steps_of(&harness, execution_id).await;
    assert_eq!(
        steps[0]["status"], "failed",
        "the row stays failed: the trace must not pretend the step succeeded"
    );
    assert_eq!(steps[0]["ignored"], true, "and it is marked as outlived");
    assert_eq!(steps[0]["error"], "the upstream is down");
    assert_eq!(
        steps[1]["status"], "succeeded",
        "so the next step still ran"
    );
    assert_eq!(mail_count(&sink, "editor@example.com"), 1);

    // The same shape without the step's own policy: the rule inherits `stop`, which is
    // what every rule did before the policy existed, and the run fails.
    let strict = harness
        .call(post(
            "/api/v1/automations",
            json!({
                "organization_id": organization_id,
                "site_id": site_id,
                "name": "Stop at the first failure",
                "event": "page.published",
                "actions": [
                    { "name": "this one fails", "kind": "task", "action": "fail",
                      "params": { "message": "the upstream is down" }, "max_attempts": 1 },
                    { "name": "never runs", "kind": "task", "action": "send_email",
                      "params": { "to": "other@example.com", "subject": "x", "body": "x" } }
                ]
            }),
            Some(&token),
        ))
        .await;
    let strict_id = strict.body["id"].as_str().expect("an id").to_owned();
    let started = harness
        .call(post(
            &format!("/api/v1/automations/{strict_id}/run"),
            Value::Null,
            Some(&token),
        ))
        .await;
    assert_eq!(started.status, StatusCode::ACCEPTED, "{:?}", started.body);
    let strict_run =
        Uuid::parse_str(started.body["execution_id"].as_str().expect("an id")).expect("a run id");
    assert_eq!(
        drive_until_settled(&harness, &actions, strict_run).await,
        ExecutionStatus::Failed
    );
    assert_eq!(steps_of(&harness, strict_run).await[0]["status"], "failed");
    assert_eq!(
        mail_count(&sink, "other@example.com"),
        0,
        "and the message after the failure was not sent"
    );

    harness.dispose().await;
}

#[tokio::test]
async fn a_step_that_outlives_its_timeout_fails_naming_the_limit() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let organization_id = create_organization_row(&harness.db, "timeout", "Timeout").await;
    let site_id = create_site_row(&harness.db, organization_id, "main", "Main").await;
    let (user_id, token) = account(&harness, Some(organization_id)).await;
    grant(&harness, user_id, organization_id, &AUTOMATION_PERMISSIONS).await;

    // A receiver that accepts the connection and then says nothing: the step's budget is
    // the only thing that can end the wait.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a listener must bind");
    let port = listener.local_addr().expect("an address").port();
    let silent = tokio::spawn(async move {
        // Hold the connection open without answering.
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });

    allow_host(&harness, "127.0.0.1").await;
    let actions = AutomationActions::new(
        harness.db.pool().clone(),
        MailSettings::new("127.0.0.1", 1, "omnion@localhost").with_sending(false),
    )
    .with_http(outbound::HttpSettings {
        enabled: true,
        timeout: StdDuration::from_secs(30),
    });

    let written = harness
        .call(post(
            "/api/v1/automations",
            json!({
                "organization_id": organization_id,
                "site_id": site_id,
                "name": "Call a silent host",
                "event": "page.published",
                "actions": [{
                    "name": "call",
                    "kind": "task",
                    "action": "http_request",
                    "params": { "url": format!("http://127.0.0.1:{}/slow", port) },
                    "timeout_ms": 250
                }]
            }),
            Some(&token),
        ))
        .await;
    assert_eq!(written.status, StatusCode::CREATED, "{:?}", written.body);
    let automation_id = written.body["id"].as_str().expect("an id").to_owned();

    // A timeout outside the engine's ceiling is refused when the rule is written.
    let refused = harness
        .call(post(
            "/api/v1/automations",
            json!({
                "organization_id": organization_id,
                "site_id": site_id,
                "name": "Wait too long",
                "event": "page.published",
                "actions": [{
                    "name": "call", "kind": "task", "action": "http_request",
                    "params": { "url": format!("http://127.0.0.1:{}/slow", port) },
                    "timeout_ms": 900_000
                }]
            }),
            Some(&token),
        ))
        .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        refused.body
    );
    assert_eq!(
        refused.body["error"]["code"], "invalid_step_timeout",
        "the ceiling is stated, not guessed: {:?}",
        refused.body
    );

    let started = harness
        .call(post(
            &format!("/api/v1/automations/{automation_id}/run"),
            Value::Null,
            Some(&token),
        ))
        .await;
    assert_eq!(started.status, StatusCode::ACCEPTED, "{:?}", started.body);
    let execution_id =
        Uuid::parse_str(started.body["execution_id"].as_str().expect("an id")).expect("a run id");
    assert_eq!(
        drive_until_settled(&harness, &actions, execution_id).await,
        ExecutionStatus::Failed
    );

    let steps = steps_of(&harness, execution_id).await;
    let error = steps[0]["error"].as_str().unwrap_or_default();
    assert_eq!(steps[0]["attempts"], 1, "one attempt was made");
    assert!(
        error.contains("250 ms"),
        "the failure names the limit the step was given: {error}"
    );

    silent.abort();
    harness.dispose().await;
}

#[tokio::test]
async fn retry_re_runs_the_tail_without_sending_the_earlier_email_twice() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };

    let organization_id = create_organization_row(&harness.db, "retry", "Retry").await;
    let site_id = create_site_row(&harness.db, organization_id, "main", "Main").await;
    let (user_id, token) = account(&harness, Some(organization_id)).await;
    grant(&harness, user_id, organization_id, &AUTOMATION_PERMISSIONS).await;

    let sink = SmtpSink::start().await;
    let actions = AutomationActions::new(
        harness.db.pool().clone(),
        MailSettings::new("127.0.0.1", sink.port, "omnion@localhost"),
    );

    // Step 1 sends and succeeds; step 2 fails. A retry from step 2 must not repeat step 1 —
    // that is the whole point of leaving the succeeded rows alone.
    let written = harness
        .call(post(
            "/api/v1/automations",
            json!({
                "organization_id": organization_id,
                "site_id": site_id,
                "name": "Send then fail",
                "event": "page.published",
                "actions": [
                    { "name": "tell the editor", "kind": "task", "action": "send_email",
                      "params": { "to": "editor@example.com", "subject": "Hi", "body": "once" } },
                    { "name": "this one fails", "kind": "task", "action": "fail",
                      "params": { "message": "the upstream is down" }, "max_attempts": 1 }
                ]
            }),
            Some(&token),
        ))
        .await;
    assert_eq!(written.status, StatusCode::CREATED, "{:?}", written.body);
    let automation_id = written.body["id"].as_str().expect("an id").to_owned();

    let started = harness
        .call(post(
            &format!("/api/v1/automations/{automation_id}/run"),
            Value::Null,
            Some(&token),
        ))
        .await;
    assert_eq!(started.status, StatusCode::ACCEPTED, "{:?}", started.body);
    let execution_id =
        Uuid::parse_str(started.body["execution_id"].as_str().expect("an id")).expect("a run id");
    assert_eq!(
        drive_until_settled(&harness, &actions, execution_id).await,
        ExecutionStatus::Failed
    );
    assert_eq!(mail_count(&sink, "editor@example.com"), 1);

    // The run detail says the controls it offers: a failed run can be retried.
    let detail = harness
        .call(get(
            &format!("/api/v1/workflow-executions/{execution_id}"),
            Some(&token),
        ))
        .await;
    assert_eq!(detail.status, StatusCode::OK, "{:?}", detail.body);
    assert_eq!(detail.body["can_retry"], true);
    assert_eq!(
        detail.body["can_cancel"], false,
        "a settled run is not cancellable"
    );
    assert_eq!(detail.body["steps"][0]["on_error"], "inherit");
    assert_eq!(detail.body["steps"][0]["timeout_ms"], 30_000);

    // Retry the failed step.
    let retried = harness
        .call(post(
            &format!("/api/v1/workflow-executions/{execution_id}/retry-step"),
            json!({ "step_no": 2 }),
            Some(&token),
        ))
        .await;
    assert_eq!(retried.status, StatusCode::OK, "{:?}", retried.body);
    assert_eq!(
        retried.body["requeued"], 1,
        "only the failed step went back on the queue: {:?}",
        retried.body
    );

    assert_eq!(
        drive_until_settled(&harness, &actions, execution_id).await,
        ExecutionStatus::Failed,
        "it fails again — the same definition fails the same way"
    );
    assert_eq!(
        mail_count(&sink, "editor@example.com"),
        1,
        "and the earlier message was NOT sent a second time"
    );

    // Resume from step 1 re-runs the whole tail, which is the other, coarser control.
    let resumed = harness
        .call(post(
            &format!("/api/v1/workflow-executions/{execution_id}/resume-from"),
            json!({ "step_no": 1 }),
            Some(&token),
        ))
        .await;
    assert_eq!(resumed.status, StatusCode::OK, "{:?}", resumed.body);
    assert_eq!(
        resumed.body["requeued"], 1,
        "step 1 had already succeeded, so only step 2"
    );

    // The resume above re-opened the run, so let it fail again before asking about a step
    // that does not exist: the run's own state is checked first, and a run that is still
    // going is refused before its steps are even read.
    drive_until_settled(&harness, &actions, execution_id).await;

    let missing = harness
        .call(post(
            &format!("/api/v1/workflow-executions/{execution_id}/retry-step"),
            json!({ "step_no": 99 }),
            Some(&token),
        ))
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "{:?}", missing.body);
    assert_eq!(missing.body["error"]["code"], "step_not_found");

    let zero = harness
        .call(post(
            &format!("/api/v1/workflow-executions/{execution_id}/retry-step"),
            json!({ "step_no": 0 }),
            Some(&token),
        ))
        .await;
    assert_eq!(zero.status, StatusCode::BAD_REQUEST, "{:?}", zero.body);
    assert_eq!(zero.body["error"]["code"], "invalid_step_no");

    // A cancelled run is not retried: that was somebody's decision.
    let cancellable = harness
        .call(post(
            &format!("/api/v1/automations/{automation_id}/run"),
            Value::Null,
            Some(&token),
        ))
        .await;
    assert_eq!(
        cancellable.status,
        StatusCode::ACCEPTED,
        "{:?}",
        cancellable.body
    );
    let second = Uuid::parse_str(cancellable.body["execution_id"].as_str().expect("an id"))
        .expect("a run id");
    let cancelled = harness
        .call(post(
            &format!("/api/v1/workflow-executions/{second}/cancel"),
            Value::Null,
            Some(&token),
        ))
        .await;
    assert_eq!(cancelled.status, StatusCode::OK, "{:?}", cancelled.body);

    let refused = harness
        .call(post(
            &format!("/api/v1/workflow-executions/{second}/retry-step"),
            json!({ "step_no": 1 }),
            Some(&token),
        ))
        .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        refused.body
    );
    assert_eq!(refused.body["error"]["code"], "execution_cancelled");

    harness.dispose().await;
}

// ---------------------------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------------------------

/// `Some` when the development database is reachable, `None` with a printed reason.
async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&config.database).await {
        Ok(db) => Some(db),
        Err(err) => {
            eprintln!("skipping the automation integration walk: {err}");
            None
        }
    }
}

fn maintenance_config(config: &Config) -> DatabaseConfig {
    DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 2,
    }
}

/// The same URL against a different database name.
fn swap_database(url: &str, database: &str) -> String {
    let (base, _) = url.rsplit_once('/').unwrap_or((url, ""));
    let base = base.split('?').next().unwrap_or(base);
    format!("{base}/{database}")
}
