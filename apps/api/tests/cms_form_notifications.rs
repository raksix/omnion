//! The notification a form submission sends — REQ-064 slice 2, the criterion's fourth part.
//!
//! The criterion says a valid submission "sends the notification e-mail". The other three parts
//! are already proved in `cms_forms.rs`; this file exists because the one that was not is the
//! one that cannot be proved by reading a row back: a message either crossed a socket or it did
//! not, and a test that asserts on `notify_status` alone would pass against a transport that
//! wrote `sent` without sending anything.
//!
//! So this suite starts a real SMTP server on a loopback port — the same sink shape
//! `automation.rs` uses for the `send_email` step, because it is the same client underneath —
//! points a copy of the process configuration at it, and submits a form the way a visitor does.
//! Every claim below is read off the wire or off the row:
//!
//! * the message reached the sink, envelope to `RCPT TO`, with the builder's recipient;
//! * the subject and the body are the ones the builder wrote, and every answer is in it;
//! * the row records `sent` and an audit entry exists, so an owner can answer "did it go?";
//! * a form with no recipients sends nothing and records `skipped` with the reason — which is a
//!   different ticket from a failed send, and the two must not be indistinguishable;
//! * platform mail switched off is the same: `skipped`, with the reason, and nothing attempted;
//! * a refused transport is `failed`, and — the point of the whole design — the **submission is
//!   still stored and the visitor still gets 202**, because a contact form that loses a message
//!   because a mail server was down is worse than one that logs a failure.

use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

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
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the account this suite creates.
const PASSWORD: &str = "correct horse battery";

/// A cookie-authenticated write is refused outright when no CSRF secret is configured, so a
/// suite that forgets this tests 403s and calls them a broken form builder. It is a test value:
/// nothing here ever compares a signature.
const CSRF_SECRET: &str = "form-notification-suite-csrf-secret";

/// The builder's powers. `forms.submissions.read` is absent on purpose: reading the inbox is a
/// different question from designing the form.
const BUILDER_PERMISSIONS: [&str; 3] = ["forms.read", "forms.manage", "content.pages.read"];

/// How long a walk gives the submit route to finish its send. The send is a loopback SMTP
/// conversation, not a network call, but a generous budget is what keeps a loaded box from
/// turning a real pass into a flake.
const SEND_BUDGET: StdDuration = StdDuration::from_secs(20);

// ---------------------------------------------------------------------------------------------
// The SMTP sink
// ---------------------------------------------------------------------------------------------

/// One message the sink received.
#[derive(Debug, Clone)]
struct Captured {
    /// Envelope commands, in order.
    commands: Vec<String>,
    /// The message (headers and body), CRLF intact.
    data: String,
}

/// A running SMTP sink on an ephemeral loopback port.
struct SmtpSink {
    port: u16,
    captured: Arc<Mutex<Vec<Captured>>>,
}

impl SmtpSink {
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
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let shared = shared.clone();
                tokio::spawn(async move { serve(stream, shared).await });
            }
        });
        Self { port, captured }
    }

    fn messages(&self) -> Vec<Captured> {
        self.captured.lock().expect("the sink lock").clone()
    }
}

/// Speak SMTP to one client until it disconnects.
async fn serve(stream: TcpStream, captured: Arc<Mutex<Vec<Captured>>>) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut commands: Vec<String> = Vec::new();
    let mut data = String::new();
    let mut in_data = false;

    writer
        .write_all(b"220 omnion form sink ready\r\n")
        .await
        .ok();

    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
            break;
        }
        let command = line.trim_end().to_owned();
        if in_data {
            if command == "." {
                in_data = false;
                commands.push("DATA".to_owned());
                captured.lock().expect("the sink lock").push(Captured {
                    commands: commands.clone(),
                    data: data.clone(),
                });
                data.clear();
                writer.write_all(b"250 2.0.0 queued\r\n").await.ok();
            } else {
                // A leading dot is the protocol's escape for a literal dot in the message.
                data.push_str(command.strip_prefix('.').unwrap_or(&command));
                data.push_str("\r\n");
            }
            continue;
        }

        let upper = command.trim_start().to_uppercase();
        if upper.starts_with("MAIL FROM") || upper.starts_with("RCPT TO") {
            commands.push(command);
            writer.write_all(b"250 2.1.0 ok\r\n").await.ok();
        } else if upper.starts_with("DATA") {
            in_data = true;
            writer
                .write_all(b"354 end with a single dot\r\n")
                .await
                .ok();
        } else if upper.starts_with("QUIT") {
            writer.write_all(b"221 2.0.0 bye\r\n").await.ok();
            break;
        } else {
            writer.write_all(b"250 2.0.0 ok\r\n").await.ok();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The process under test
// ---------------------------------------------------------------------------------------------

struct TestResponse {
    status: StatusCode,
    /// Every `Set-Cookie` header the response set.
    cookies: Vec<String>,
    body: Value,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");
    let status = response.status();
    let cookies: Vec<String> = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok().map(str::to_owned))
        .collect();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("the body must collect")
        .to_bytes();
    TestResponse {
        status,
        cookies,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    }
}

/// The two halves of a signed-in call: the session cookie and the CSRF token, both of which
/// sign-in hands back.
#[derive(Debug, Clone)]
struct Credentials {
    session: String,
    csrf: Option<String>,
}

impl Credentials {
    fn session(&self) -> String {
        format!("omnion_session={}", self.session)
    }

    fn csrf(&self) -> Option<&str> {
        self.csrf.as_deref()
    }
}

/// Every `Set-Cookie` a sign-in issued, joined the way a browser would send them.
fn credentials_from(set_cookies: &[String]) -> Credentials {
    let mut session = None;
    let mut csrf = None;
    for raw in set_cookies {
        // One response may set several cookies; they arrive as separate header values, and a
        // caller that only looks at the first one silently loses the CSRF token.
        for piece in raw.split(", ") {
            let Some((name, value)) = piece.split(';').next().unwrap_or_default().split_once('=')
            else {
                continue;
            };
            match name {
                "omnion_session" => session = Some(value.to_owned()),
                "omnion_csrf" => csrf = Some(value.to_owned()),
                _ => {}
            }
        }
    }
    Credentials {
        session: session.expect("login must set the session cookie"),
        csrf,
    }
}

fn request(
    method: Method,
    path: &str,
    token: Option<&Credentials>,
    body: Option<Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(session) = token.as_deref().map(Credentials::session) {
        builder = builder.header(header::COOKIE, session);
    }
    if let Some(csrf) = token.as_deref().and_then(Credentials::csrf) {
        // The session cookie is ambient authority, so a write has to prove the caller could
        // read the page. The panel sends this header; a walk that forgets it is testing the
        // CSRF middleware and calling the result a broken form builder.
        builder = builder.header("x-omnion-csrf", csrf);
    }
    match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&body).expect("body must serialize"),
            ))
            .expect("request must build"),
        None => builder.body(Body::empty()).expect("request must build"),
    }
}

/// The state a form submit runs inside, with platform mail pointed at `sink`.
///
/// The configuration is a COPY with the mail block replaced: the process-wide environment is
/// shared with every other suite in this binary, and an `std::env::set_var` here would make the
/// result depend on which test the runner happened to schedule first.
async fn state_with_mail(sink_port: u16, enabled: bool) -> (Option<AppState>, Option<Db>) {
    let mut config = Config::from_env().expect("environment must be valid");
    config.csrf = CsrfSecret::new(Some(CSRF_SECRET.to_owned()));
    config.mail.enabled = enabled;
    config.mail.host = "127.0.0.1".to_owned();
    config.mail.port = sink_port;
    config.mail.from = "omnion@localhost".to_owned();
    config.mail.username = None;
    config.mail.password = None;

    let mut db_config = config.database.clone();
    db_config.max_connections = 4;
    let db = match Db::connect(&db_config).await {
        Ok(db) => db,
        Err(error) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({error})");
            return (None, None);
        }
    };
    db.migrate().await.expect("migrations must apply");
    let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
    let storage = omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid");
    let state = AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config,
        db.clone(),
        redis,
        storage,
    );
    (Some(state), Some(db))
}

// ---------------------------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------------------------

struct Fixture {
    state: AppState,
    db: Db,
    sink: SmtpSink,
    org: Uuid,
    site: Uuid,
    site_key: String,
    email: String,
}

impl Fixture {
    /// A fixture whose platform mail points at its own sink.
    async fn new() -> Option<Self> {
        let sink = SmtpSink::start().await;
        let (state, db) = state_with_mail(sink.port, true).await;
        let (state, db) = (state?, db?);
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let org = Uuid::new_v4();
        sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
            .bind(org)
            .bind("Form Notify Org")
            .bind(format!("notify-{}", Uuid::new_v4().simple()))
            .execute(db.pool())
            .await
            .expect("the organization must be created");

        let site = Uuid::new_v4();
        let site_key = format!("ntf{}", &Uuid::new_v4().simple().to_string()[..8]);
        sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
            .bind(site)
            .bind(org)
            .bind(&site_key)
            .bind("Notify Site")
            .execute(db.pool())
            .await
            .expect("the site must be created");

        let email = format!("notify-{}@example.test", Uuid::new_v4().simple());
        let user = users::create_user(
            db.pool(),
            NewUser {
                email: email.clone(),
                display_name: "Notify Tester".to_owned(),
                password: PASSWORD.to_owned(),
                organization_id: Some(org),
            },
        )
        .await
        .expect("the account must be created");
        grant(db.pool(), org, user.id, &BUILDER_PERMISSIONS).await;

        Some(Self {
            state,
            db,
            sink,
            org,
            site,
            site_key,
            email,
        })
    }

    /// Sign in, once, and keep both credentials.
    async fn token(&self) -> Credentials {
        let response = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/auth/login",
                None,
                Some(json!({ "email": &self.email, "password": PASSWORD })),
            ),
        )
        .await;
        assert!(
            response.status.is_success(),
            "login for {} answered {}: {:?}",
            self.email,
            response.status,
            response.body
        );
        credentials_from(&response.cookies)
    }

    /// A published form with the given recipients.
    async fn published_form(&self, token: &Credentials, key: &str, recipients: &[&str]) -> String {
        let created = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/forms",
                Some(token),
                Some(json!({
                    "site_id": self.site,
                    "key": key,
                    "name": "Contact us",
                    "notify_emails": recipients,
                    "notify_subject": "Enquiry: {{form_name}}",
                    "fields": [
                        { "key": "email", "label": "E-mail", "field_type": "text", "required": true, "width": "half", "rules": { "email": true }, "options": [] },
                        { "key": "message", "label": "Message", "field_type": "textarea", "required": true, "width": "full", "rules": {}, "options": [] }
                    ]
                })),
            ),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
        let id = created.body["id"].as_str().expect("an id").to_owned();

        let published = call(
            &self.state,
            request(
                Method::POST,
                &format!("/api/v1/forms/{id}/publish"),
                Some(token),
                Some(json!({ "status": "published" })),
            ),
        )
        .await;
        assert_eq!(published.status, StatusCode::OK, "{}", published.body);
        id
    }

    /// Submit the way a visitor does, with a fill time comfortably past the floor.
    async fn submit(&self, form_key: &str) -> TestResponse {
        call(
            &self.state,
            request(
                Method::POST,
                &format!("/api/v1/public/forms/{form_key}/submit?site={}", self.site_key),
                None,
                Some(json!({
                    "answers": { "email": "visitor@example.test", "message": "I would like a quote." },
                    "filled_at_ms": 60_000,
                    "source_path": "/contact"
                })),
            ),
        )
        .await
    }

    /// The stored notification record for a form's latest submission.
    async fn notify_row(&self, form_id: &str) -> (Option<String>, Option<String>, Option<String>) {
        let row: Option<(Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
            "select notify_status, notify_error, case when notified_at is null then null \
             else 'set' end from cms_form_submissions where form_id = $1 \
             order by created_at desc limit 1",
        )
        .bind(Uuid::parse_str(form_id).expect("a form id"))
        .fetch_optional(self.db.pool())
        .await
        .expect("the row must read");
        row.unwrap_or((None, None, None))
    }

    /// Give the route's send a moment to land, then stop looking.
    async fn settle(&self) {
        for _ in 0..60 {
            if !self.sink.messages().is_empty() {
                return;
            }
            tokio::time::sleep(StdDuration::from_millis(100)).await;
        }
    }
}

async fn grant(pool: &sqlx::PgPool, organization_id: Uuid, user_id: Uuid, keys: &[&str]) {
    let role = role_store::create_role(
        pool,
        NewRole {
            organization_id,
            key: format!("notifier-{}", &Uuid::new_v4().simple().to_string()[..8]),
            name: "Form Notifier".to_owned(),
            description: "Builds forms in the notification suite".to_owned(),
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
    role_store::set_role_permissions(pool, role.id, &entries)
        .await
        .expect("the role permission set must be written");
    let binding = NewBinding {
        role_id: role.id,
        user_id,
        scope: Scope::Organization { organization_id },
        granted_by: None,
        expires_at: None,
    };
    bindings::validate(pool, &binding)
        .await
        .expect("the binding must validate");
    bindings::grant(pool, binding)
        .await
        .expect("the binding must be granted");
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_submission_reaches_the_mail_server_with_the_builders_own_words() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.token().await;
    let form_id = fixture
        .published_form(&token, "contact", &["owner@example.test"])
        .await;

    let submitted = fixture.submit("contact").await;
    assert_eq!(
        submitted.status,
        StatusCode::ACCEPTED,
        "{:?}",
        submitted.body
    );
    assert_eq!(submitted.body["stored"], json!(true));

    fixture.settle().await;
    let messages = fixture.sink.messages();
    assert_eq!(
        messages.len(),
        1,
        "the message must actually cross the socket, not just be marked sent: {:?}",
        messages
    );
    let message = &messages[0];

    // The envelope, read off the wire: the builder's address is the one that was told.
    assert!(
        message
            .commands
            .iter()
            .any(|c| c == "RCPT TO:<owner@example.test>"),
        "envelope must carry the builder's recipient: {:?}",
        message.commands
    );
    assert!(
        message.commands.iter().any(|c| c.starts_with("DATA")),
        "the body must have been transferred: {:?}",
        message.commands
    );

    // The subject is the builder's template, rendered — not the template, and not a default
    // that forgot to render.
    assert!(
        message.data.contains("Subject: Enquiry: Contact us"),
        "rendered subject missing from the wire:\n{}",
        message.data
    );
    // And every answer is in it, under the field's LABEL, in the builder's own vocabulary.
    assert!(
        message.data.contains("E-mail: visitor@example.test"),
        "answers must be labelled, not keyed:\n{}",
        message.data
    );
    assert!(
        message.data.contains("Message: I would like a quote."),
        "every answer must travel:\n{}",
        message.data
    );
    assert!(
        message.data.contains("Received: "),
        "the owner needs to know when it arrived:\n{}",
        message.data
    );

    // The row agrees, and an owner can answer "did it go?" from the inbox rather than from a
    // log line that rotated away.
    let (status, error, stamp) = fixture.notify_row(&form_id).await;
    assert_eq!(status.as_deref(), Some("sent"), "notify_error: {error:?}");
    assert_eq!(error, None);
    assert_eq!(
        stamp.as_deref(),
        Some("set"),
        "the attempt must be timestamped"
    );

    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where organization_id = $1 and action = 'form.notification'",
    )
    .bind(fixture.org)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the audit count must read");
    assert_eq!(
        audited, 1,
        "a message leaving the platform is an auditable act"
    );
}

#[tokio::test]
async fn a_form_with_no_recipients_sends_nothing_and_says_which_ticket_it_is() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.token().await;
    let form_id = fixture.published_form(&token, "no-recipients", &[]).await;

    let submitted = fixture.submit("no-recipients").await;
    assert_eq!(submitted.status, StatusCode::ACCEPTED);
    assert_eq!(
        submitted.body["stored"],
        json!(true),
        "the message is still kept"
    );

    // Nothing on the wire, and a REASON — because "no recipients" is an owner's fix in the
    // builder and "platform mail is off" is an operator's fix in the environment, and a single
    // "did not send" would leave them guessing which one they are looking at.
    tokio::time::sleep(StdDuration::from_millis(300)).await;
    assert!(
        fixture.sink.messages().is_empty(),
        "a form with no recipients must not put anything on the wire"
    );
    let (status, error, _) = fixture.notify_row(&form_id).await;
    assert_eq!(status.as_deref(), Some("skipped"));
    assert_eq!(error.as_deref(), Some("no recipients configured"));
}

#[tokio::test]
async fn platform_mail_switched_off_skips_the_send_and_names_the_setting() {
    let sink = SmtpSink::start().await;
    let (state, db) = state_with_mail(sink.port, false).await;
    let (state, db) = match (state, db) {
        (Some(state), Some(db)) => (state, db),
        _ => return,
    };
    seed::ensure(db.pool())
        .await
        .expect("the IAM seed must run");

    let org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(org)
        .bind("Mail Off Org")
        .bind(format!("mailoff-{}", Uuid::new_v4().simple()))
        .execute(db.pool())
        .await
        .expect("the organization must be created");
    let site = Uuid::new_v4();
    let site_key = format!("mof{}", &Uuid::new_v4().simple().to_string()[..8]);
    sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
        .bind(site)
        .bind(org)
        .bind(&site_key)
        .bind("Mail Off Site")
        .execute(db.pool())
        .await
        .expect("the site must be created");
    let email = format!("mailoff-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            display_name: "Mail Off".to_owned(),
            password: PASSWORD.to_owned(),
            organization_id: Some(org),
        },
    )
    .await
    .expect("the account must be created");
    grant(db.pool(), org, user.id, &BUILDER_PERMISSIONS).await;

    let fixture = Fixture {
        state,
        db,
        sink,
        org,
        site,
        site_key,
        email,
    };
    let token = fixture.token().await;
    let form_id = fixture
        .published_form(&token, "mail-off", &["owner@example.test"])
        .await;

    let submitted = fixture.submit("mail-off").await;
    assert_eq!(submitted.status, StatusCode::ACCEPTED);
    assert_eq!(submitted.body["stored"], json!(true));

    tokio::time::sleep(StdDuration::from_millis(300)).await;
    assert!(
        fixture.sink.messages().is_empty(),
        "a process with mail switched off must not attempt a connection"
    );
    let (status, error, _) = fixture.notify_row(&form_id).await;
    assert_eq!(status.as_deref(), Some("skipped"));
    assert_eq!(error.as_deref(), Some("platform email is switched off"));
}

#[tokio::test]
async fn a_refused_transport_is_recorded_and_the_submission_is_still_stored() {
    // A port nothing is listening on: the transport is genuinely unavailable, which is the
    // ordinary shape of "the mail server is down".
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port must be reservable");
        listener.local_addr().expect("an address").port()
    };
    let (state, db) = state_with_mail(closed, true).await;
    let (state, db) = match (state, db) {
        (Some(state), Some(db)) => (state, db),
        _ => return,
    };
    seed::ensure(db.pool())
        .await
        .expect("the IAM seed must run");

    let org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(org)
        .bind("Mail Down Org")
        .bind(format!("maildown-{}", Uuid::new_v4().simple()))
        .execute(db.pool())
        .await
        .expect("the organization must be created");
    let site = Uuid::new_v4();
    let site_key = format!("mld{}", &Uuid::new_v4().simple().to_string()[..8]);
    sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
        .bind(site)
        .bind(org)
        .bind(&site_key)
        .bind("Mail Down Site")
        .execute(db.pool())
        .await
        .expect("the site must be created");
    let email = format!("maildown-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            display_name: "Mail Down".to_owned(),
            password: PASSWORD.to_owned(),
            organization_id: Some(org),
        },
    )
    .await
    .expect("the account must be created");
    grant(db.pool(), org, user.id, &BUILDER_PERMISSIONS).await;

    let fixture = Fixture {
        state,
        db,
        sink: SmtpSink {
            port: closed,
            captured: Arc::new(Mutex::new(Vec::new())),
        },
        org,
        site,
        site_key,
        email,
    };
    let token = fixture.token().await;
    let form_id = fixture
        .published_form(&token, "mail-down", &["owner@example.test"])
        .await;

    let submitted = fixture.submit("mail-down").await;

    // The visiter's message is the product. A mail server being down is an operator's outage
    // and must not become a lost contact form submission — or a 500 that makes the visitor
    // press send again and leave five copies behind.
    assert_eq!(
        submitted.status,
        StatusCode::ACCEPTED,
        "a failed notification must never fail the submission: {:?}",
        submitted.body
    );
    assert_eq!(submitted.body["stored"], json!(true));

    let (status, _, stamp) = fixture.notify_row(&form_id).await;
    assert_eq!(status.as_deref(), Some("failed"));
    assert_eq!(
        stamp.as_deref(),
        Some("set"),
        "even a failure is timestamped"
    );

    // And the row is really in the inbox, which is the half that matters.
    // Counted against THIS form, not "is there a submission somewhere": the suite may share a
    // database with an earlier run of itself, and a count that a previous run can inflate is a
    // count that stops proving anything.
    let rows: i64 = sqlx::query_scalar(
        "select count(*) from cms_form_submissions s join cms_forms f on f.id = s.form_id \
         where f.id = $1",
    )
    .bind(Uuid::parse_str(&form_id).expect("a form id"))
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must read");
    assert_eq!(
        rows, 1,
        "the message must be kept whatever the transport did"
    );
}

#[tokio::test]
async fn the_schema_refuses_a_notification_state_nobody_writes() {
    let (_, db) = state_with_mail(1, true).await;
    let Some(db) = db else {
        return;
    };
    let org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(org)
        .bind("Schema Org")
        .bind(format!("schema-{}", Uuid::new_v4().simple()))
        .execute(db.pool())
        .await
        .expect("the organization must be created");
    let site = Uuid::new_v4();
    sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
        .bind(site)
        .bind(org)
        .bind(format!("sch{}", &Uuid::new_v4().simple().to_string()[..8]))
        .bind("Schema Site")
        .execute(db.pool())
        .await
        .expect("the site must be created");
    let form = Uuid::new_v4();
    // The form row is written the way the schema demands: a `message` action needs a
    // submit_message, and its own CHECK is the thing that says so.
    sqlx::query(
        "insert into cms_forms (id, site_id, organization_id, key, name, submit_action, \
         submit_message) values ($1, $2, $3, 'schema-check', 'Schema check', 'message', \
         'Thanks — we will be in touch.')",
    )
    .bind(form)
    .bind(site)
    .bind(org)
    .execute(db.pool())
    .await
    .expect("the form must be created");
    let submission = Uuid::new_v4();
    sqlx::query("insert into cms_form_submissions (id, form_id, site_id) values ($1, $2, $3)")
        .bind(submission)
        .bind(form)
        .bind(site)
        .execute(db.pool())
        .await
        .expect("the submission must be created");

    // A fourth state is a fourth meaning, and a row that claims one is a bug report waiting to
    // be filed. The CHECK is the only thing that refuses it.
    let bogus =
        sqlx::query("update cms_form_submissions set notify_status = 'queued' where id = $1")
            .bind(submission)
            .execute(db.pool())
            .await;
    assert!(
        bogus.is_err(),
        "an unwritten notification state must be refused"
    );

    // And a timestamp with no state is a partial write, which is worse than none: it looks like
    // a decision somebody made.
    let partial = sqlx::query("update cms_form_submissions set notified_at = now() where id = $1")
        .bind(submission)
        .execute(db.pool())
        .await;
    assert!(
        partial.is_err(),
        "an attempt with no state must be refused: it claims a decision that was never recorded"
    );
}
