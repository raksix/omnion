//! Integration tests for the event bus and webhook deliveries (phase P12,
//! docs/01-VISION.md §13).
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) on a throwaway database,
//! and the receiver they deliver to is **real**: an HTTP server this suite starts on an
//! ephemeral loopback port. Nothing here leaves the machine, and the proof P12 asks for is
//! exactly this — publishing a page records `page.published` and the platform POSTs one signed
//! delivery per subscribed endpoint, with a signature the receiver verifies over the exact
//! bytes it received.
//!
//! The walks cover: connecting an endpoint (the secret shown once, never again), an operator's
//! test delivery, the `page.published` fan-out, a receiver that refuses and the retry ladder
//! that follows, a delivery that runs out of attempts, the endpoint that was switched off while
//! its queue waited, the event feed, the tenancy scope (an event of one organization never
//! reaches another organization's endpoint) and the permission gates.
//!
//! When PostgreSQL is not reachable the suite skips itself with a printed reason, so
//! `cargo test` stays usable on a machine without Docker.

use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post as route_post;
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_events::engine::{self, RunReport, RunnerConfig};
use omnion_events::sender;
use omnion_events::signature;
use omnion_identity::sessions;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use serde_json::{Value, json};
use time::Duration;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// How long the retry ladder waits between attempts in this suite.
const RETRY_BASE_MS: u64 = 40;

// ---------------------------------------------------------------------------------------------
// The receiver: a real HTTP server on an ephemeral loopback port
// ---------------------------------------------------------------------------------------------

/// One delivery the receiver captured.
#[derive(Debug, Clone)]
struct Captured {
    /// `X-Omnion-Event`.
    event: String,
    /// `X-Omnion-Delivery`.
    delivery: String,
    /// `X-Omnion-Timestamp`.
    timestamp: i64,
    /// `X-Omnion-Signature`.
    signature: String,
    /// The raw body the signature covers.
    body: Vec<u8>,
}

impl Captured {
    /// The body as JSON.
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("the delivery body must be JSON")
    }
}

/// Shared state of one receiver.
#[derive(Clone)]
struct ReceiverState {
    seen: Arc<Mutex<Vec<Captured>>>,
    refuse_next: Arc<Mutex<usize>>,
    refuse_always: bool,
}

/// A running receiver: its URL and the task that serves it.
struct Receiver {
    url: String,
    state: ReceiverState,
    task: JoinHandle<()>,
}

impl Receiver {
    /// Start the receiver on an ephemeral port.
    async fn start(refuse_always: bool) -> Self {
        let state = ReceiverState {
            seen: Arc::new(Mutex::new(Vec::new())),
            refuse_next: Arc::new(Mutex::new(0)),
            refuse_always,
        };

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the receiver must bind a port");
        let address = listener.local_addr().expect("the receiver has an address");

        let app = Router::new()
            .route("/hooks/omnion", route_post(receive))
            .with_state(state.clone());

        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            url: format!("http://{address}/hooks/omnion"),
            state,
            task,
        }
    }

    /// Everything the receiver captured so far.
    fn captured(&self) -> Vec<Captured> {
        self.state.seen.lock().expect("the receiver lock").clone()
    }

    /// Refuse this many upcoming deliveries with a `500`.
    fn refuse_next(&self, count: usize) {
        *self.state.refuse_next.lock().expect("the receiver lock") = count;
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `POST /hooks/omnion` — capture the delivery, then accept or refuse it.
async fn receive(State(state): State<ReceiverState>, headers: HeaderMap, body: Bytes) -> Response {
    let read = |name: &str| -> String {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };

    let captured = Captured {
        event: read(signature::EVENT_HEADER),
        delivery: read(signature::DELIVERY_HEADER),
        timestamp: read(signature::TIMESTAMP_HEADER)
            .parse()
            .unwrap_or_default(),
        signature: read(signature::SIGNATURE_HEADER),
        body: body.to_vec(),
    };

    let refuse = {
        let mut budget = state.refuse_next.lock().expect("the receiver lock");
        if *budget > 0 {
            *budget -= 1;
            true
        } else {
            state.refuse_always
        }
    };

    state.seen.lock().expect("the receiver lock").push(captured);

    if refuse {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "the receiver is having a bad day",
        )
            .into_response()
    } else {
        (StatusCode::OK, "accepted").into_response()
    }
}

// ---------------------------------------------------------------------------------------------
// The harness: a throwaway database with every migration applied
// ---------------------------------------------------------------------------------------------

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    body: Value,
    text: String,
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

        let database = format!("omnion_events_{}", Uuid::new_v4().simple());
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
            omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
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
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };

        TestResponse { status, body, text }
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

/// A PATCH request carrying a JSON body.
fn patch(uri: &str, body: Value, token: Option<&str>) -> Request<Body> {
    request(Method::PATCH, uri, token, Some(body))
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
            email: format!("events-{}@omnion.test", Uuid::new_v4().simple()),
            password: PASSWORD.to_owned(),
            display_name: "Walk".to_owned(),
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

/// Bind a role with exactly these permission keys to one account, at organization scope.
async fn grant(harness: &Harness, user_id: Uuid, organization_id: Uuid, keys: &[&str]) -> Uuid {
    let role = role_store::create_role(
        harness.db.pool(),
        NewRole {
            organization_id,
            key: format!("events-walk-{}", Uuid::new_v4().simple()),
            name: "Events Walk".to_owned(),
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

    role.id
}

/// Create an organization row with a unique, suite-scoped slug.
async fn create_organization_row(db: &Db, label: &str, name: &str) -> Uuid {
    let slug = format!("events-walk-{label}-{}", Uuid::new_v4().simple());
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

/// The runner configuration the walks use: a fast backoff so the retry ladder is observable.
fn runner_config() -> RunnerConfig {
    RunnerConfig {
        batch: 50,
        lease_seconds: 30,
        request_timeout: StdDuration::from_secs(5),
        retry_base: Duration::milliseconds(RETRY_BASE_MS as i64),
        retry_max: Duration::milliseconds((RETRY_BASE_MS * 8) as i64),
    }
}

/// One delivery tick against the throwaway database.
async fn tick(harness: &Harness) -> RunReport {
    let client = sender::client(StdDuration::from_secs(5)).expect("the delivery client must build");
    engine::run_due(harness.db.pool(), &client, &runner_config())
        .await
        .expect("the delivery tick must run")
}

/// Wait out the longest backoff ladder step this suite can produce.
async fn after_backoff() {
    tokio::time::sleep(StdDuration::from_millis(RETRY_BASE_MS * 8 + 100)).await;
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_bus_records_events_and_delivers_signed_webhooks() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let receiver = Receiver::start(false).await;

    // The platform Owner: the wizard's account, with no primary organization.
    let (owner_id, owner_token) = account(&harness, None).await;
    seed::bind_owner(harness.db.pool(), owner_id)
        .await
        .expect("the owner binding must be created");

    let organization = create_organization_row(&harness.db, "a", "Events Test A").await;
    let site = create_site_row(&harness.db, organization, "main", "Events Site").await;

    // Nothing on the surface without a session.
    assert_eq!(
        harness.call(get("/api/v1/webhooks", None)).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        harness.call(get("/api/v1/events", None)).await.status,
        StatusCode::UNAUTHORIZED
    );

    // Connect the receiver. The platform generates the secret and shows it exactly once.
    let created = harness
        .call(post(
            "/api/v1/webhooks",
            json!({
                "organization_id": organization,
                "name": "Receiver",
                "url": receiver.url,
                "events": ["page.published", "webhook.test"],
            }),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let endpoint_id = created.body["id"].as_str().expect("endpoint id").to_owned();
    let secret = created.body["secret"]
        .as_str()
        .expect("the generated secret is shown once")
        .to_owned();
    assert_eq!(secret.len(), 64, "a generated secret is 32 bytes, hex");
    assert_eq!(
        created.body["events"],
        json!(["page.published", "webhook.test"]),
        "the subscription list comes back sorted"
    );
    assert_eq!(created.body["enabled"], json!(true));

    // A second endpoint with the same name inside the organization is refused.
    let duplicate = harness
        .call(post(
            "/api/v1/webhooks",
            json!({
                "organization_id": organization,
                "name": "receiver",
                "url": receiver.url,
                "events": ["page.published"],
            }),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(
        duplicate.status,
        StatusCode::CONFLICT,
        "{:?}",
        duplicate.body
    );
    assert_eq!(duplicate.body["error"]["code"], "webhook_name_taken");

    // …and a URL the platform cannot post to is refused before anything is stored.
    let unusable = harness
        .call(post(
            "/api/v1/webhooks",
            json!({
                "organization_id": organization,
                "name": "No Scheme",
                "url": "hooks.example.test/omnion",
                "events": ["page.published"],
            }),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(
        unusable.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        unusable.body
    );
    assert_eq!(unusable.body["error"]["code"], "invalid_webhook_endpoint");

    // The secret never comes back out of the API.
    let listed = harness
        .call(get("/api/v1/webhooks", Some(&owner_token)))
        .await;
    assert_eq!(listed.status, StatusCode::OK, "{:?}", listed.body);
    assert_eq!(
        listed.body["webhooks"].as_array().expect("webhooks").len(),
        1
    );
    assert!(
        !listed.text.contains(&secret),
        "the stored secret must never come back: {}",
        listed.text
    );

    // The operator's test delivery: queued, then sent by one tick.
    let tested = harness
        .call(post(
            &format!("/api/v1/webhooks/{endpoint_id}/test"),
            json!({}),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(tested.status, StatusCode::ACCEPTED, "{:?}", tested.body);
    assert_eq!(tested.body["deliveries"], json!(1));

    let report = tick(&harness).await;
    assert_eq!(report.claimed, 1, "one delivery was due");
    assert_eq!(report.delivered, 1, "{report:?}");

    let captured = receiver.captured();
    assert_eq!(captured.len(), 1, "the receiver saw exactly one delivery");
    let test_delivery = &captured[0];
    assert_eq!(test_delivery.event, "webhook.test");
    assert!(
        Uuid::parse_str(&test_delivery.delivery).is_ok(),
        "the delivery header carries an id: {:?}",
        test_delivery.delivery
    );
    assert!(
        signature::verify(
            &secret,
            test_delivery.timestamp,
            &test_delivery.body,
            &test_delivery.signature
        ),
        "the signature must verify against the received bytes: {:?}",
        test_delivery.signature
    );
    let test_body = test_delivery.json();
    assert_eq!(test_body["name"], json!("webhook.test"));
    assert_eq!(
        test_body["organization_id"],
        json!(organization.to_string())
    );
    assert_eq!(test_body["payload"]["endpoint_name"], json!("Receiver"));

    // The queue history of the endpoint shows the delivered attempt.
    let deliveries = harness
        .call(get(
            &format!("/api/v1/webhooks/{endpoint_id}/deliveries"),
            Some(&owner_token),
        ))
        .await;
    let rows = deliveries.body["deliveries"]
        .as_array()
        .expect("deliveries")
        .clone();
    assert_eq!(rows.len(), 1, "{:?}", deliveries.body);
    assert_eq!(rows[0]["event_name"], json!("webhook.test"));
    assert_eq!(rows[0]["status"], json!("delivered"));
    assert_eq!(rows[0]["attempts"], json!(1));
    assert_eq!(rows[0]["response_status"], json!(200));
    assert_eq!(
        rows[0]["id"],
        json!(test_delivery.delivery),
        "the delivery header is the id the queue knows"
    );

    // Publishing a page records `page.published` and queues the fan-out.
    let page = harness
        .call(post(
            "/api/v1/pages",
            json!({ "site_id": site, "slug": "home", "title": "Welcome" }),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(page.status, StatusCode::CREATED, "{:?}", page.body);
    let page_id = page.body["id"].as_str().expect("page id").to_owned();

    let published = harness
        .call(post(
            &format!("/api/v1/pages/{page_id}/publish"),
            json!({}),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(published.status, StatusCode::OK, "{:?}", published.body);

    let feed = harness
        .call(get(
            "/api/v1/events?name=page.published",
            Some(&owner_token),
        ))
        .await;
    assert_eq!(feed.status, StatusCode::OK, "{:?}", feed.body);
    let events = feed.body["events"].as_array().expect("events").clone();
    assert_eq!(events.len(), 1, "{:?}", feed.body);
    assert_eq!(events[0]["payload"]["slug"], json!("home"));
    assert_eq!(events[0]["site_id"], json!(site.to_string()));
    assert_eq!(events[0]["actor_user_id"], json!(owner_id.to_string()));

    let report = tick(&harness).await;
    assert_eq!(report.delivered, 1, "{report:?}");

    let captured = receiver.captured();
    assert_eq!(captured.len(), 2, "the receiver saw the publication too");
    let publication = &captured[1];
    assert_eq!(publication.event, "page.published");
    assert!(
        signature::verify(
            &secret,
            publication.timestamp,
            &publication.body,
            &publication.signature
        ),
        "the publication delivery is signed with the same secret"
    );
    let publication_body = publication.json();
    assert_eq!(publication_body["payload"]["slug"], json!("home"));
    assert_eq!(publication_body["payload"]["title"], json!("Welcome"));
    assert_eq!(publication_body["site_id"], json!(site.to_string()));

    // A receiver that refuses: the delivery stays queued with a backoff, then succeeds.
    harness
        .call(patch(
            &format!("/api/v1/pages/{page_id}"),
            json!({ "title": "Welcome again" }),
            Some(&owner_token),
        ))
        .await;
    let republished = harness
        .call(post(
            &format!("/api/v1/pages/{page_id}/publish"),
            json!({}),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(republished.status, StatusCode::OK, "{:?}", republished.body);

    receiver.refuse_next(1);
    let report = tick(&harness).await;
    assert_eq!(
        report.retried, 1,
        "the refusal is scheduled for another attempt: {report:?}"
    );
    assert_eq!(report.failed, 0);

    let deliveries = harness
        .call(get(
            &format!("/api/v1/webhooks/{endpoint_id}/deliveries"),
            Some(&owner_token),
        ))
        .await;
    let rows = deliveries.body["deliveries"]
        .as_array()
        .expect("deliveries")
        .clone();
    assert_eq!(rows[0]["status"], json!("pending"));
    assert_eq!(rows[0]["attempts"], json!(1));
    assert_eq!(rows[0]["response_status"], json!(500));
    assert!(
        rows[0]["error"]
            .as_str()
            .expect("an error message")
            .contains("500"),
        "the refusal is reported: {:?}",
        rows[0]
    );

    after_backoff().await;
    let report = tick(&harness).await;
    assert_eq!(report.delivered, 1, "the retry delivers: {report:?}");

    let deliveries = harness
        .call(get(
            &format!("/api/v1/webhooks/{endpoint_id}/deliveries"),
            Some(&owner_token),
        ))
        .await;
    let rows = deliveries.body["deliveries"]
        .as_array()
        .expect("deliveries")
        .clone();
    assert_eq!(rows[0]["status"], json!("delivered"));
    assert_eq!(
        rows[0]["attempts"],
        json!(2),
        "the second attempt is the one that landed"
    );
    assert_eq!(rows[0]["response_status"], json!(200));
    assert_eq!(
        receiver.captured().len(),
        4,
        "one test delivery, two publications and one retry reached the receiver"
    );

    // A receiver that never accepts: the delivery runs out of attempts and stays failed.
    let broken = Receiver::start(true).await;
    let broken_endpoint = harness
        .call(post(
            "/api/v1/webhooks",
            json!({
                "organization_id": organization,
                "name": "Broken Receiver",
                "url": broken.url,
                "events": ["page.published"],
            }),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(
        broken_endpoint.status,
        StatusCode::CREATED,
        "{:?}",
        broken_endpoint.body
    );
    let broken_id = broken_endpoint.body["id"]
        .as_str()
        .expect("endpoint id")
        .to_owned();

    harness
        .call(patch(
            &format!("/api/v1/pages/{page_id}"),
            json!({ "title": "Welcome a third time" }),
            Some(&owner_token),
        ))
        .await;
    let third = harness
        .call(post(
            &format!("/api/v1/pages/{page_id}/publish"),
            json!({}),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(third.status, StatusCode::OK, "{:?}", third.body);

    let mut last = RunReport::default();
    let mut settled = None;
    for _ in 0..7 {
        last = tick(&harness).await;
        let deliveries = harness
            .call(get(
                &format!("/api/v1/webhooks/{broken_id}/deliveries"),
                Some(&owner_token),
            ))
            .await;
        let rows = deliveries.body["deliveries"]
            .as_array()
            .expect("deliveries")
            .clone();
        if rows[0]["status"] == json!("failed") {
            settled = Some(rows[0].clone());
            break;
        }
        after_backoff().await;
    }

    let row = settled.unwrap_or_else(|| {
        panic!("the delivery never ran out of attempts: {last:?}");
    });
    assert_eq!(row["status"], json!("failed"));
    assert_eq!(row["attempts"], json!(5), "{row:?}");
    assert_eq!(row["max_attempts"], json!(5));
    assert_eq!(row["response_status"], json!(500));
    assert!(
        row["error"].as_str().expect("an error").contains("500"),
        "{row:?}"
    );
    assert_eq!(
        broken.captured().len(),
        5,
        "the receiver saw every attempt, including the one that ran out of budget"
    );

    // The feed holds every recorded event, and the audit trail holds the operator's actions.
    let feed = harness
        .call(get("/api/v1/events?limit=50", Some(&owner_token)))
        .await;
    let events = feed.body["events"].as_array().expect("events").clone();
    assert_eq!(events.len(), 4, "{:?}", feed.body);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["name"] == json!("page.published"))
            .count(),
        3,
        "three publications were recorded: {:?}",
        feed.body
    );
    assert_eq!(events[0]["name"], json!("page.published"), "newest first");
    assert_eq!(events[3]["name"], json!("webhook.test"), "oldest last");

    let audit = harness
        .call(get("/api/v1/iam/audit", Some(&owner_token)))
        .await;
    let actions: Vec<String> = audit.body["entries"]
        .as_array()
        .expect("audit entries")
        .iter()
        .filter_map(|entry| entry["action"].as_str().map(str::to_owned))
        .collect();
    for expected in [
        "webhook.endpoint.created",
        "webhook.endpoint.tested",
        "page.published",
    ] {
        assert!(
            actions.contains(&expected.to_owned()),
            "{expected} must be audited: {actions:?}"
        );
    }

    harness.dispose().await;
}

#[tokio::test]
async fn webhooks_are_scoped_per_organization_and_permission_guarded() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let receiver_a = Receiver::start(false).await;
    let receiver_b = Receiver::start(false).await;

    let (owner_id, owner_token) = account(&harness, None).await;
    seed::bind_owner(harness.db.pool(), owner_id)
        .await
        .expect("the owner binding must be created");

    let org_a = create_organization_row(&harness.db, "a", "Events Scope A").await;
    let org_b = create_organization_row(&harness.db, "b", "Events Scope B").await;
    let site_a = create_site_row(&harness.db, org_a, "main", "Events Scope Site A").await;

    // Two organization accounts with the webhook keys, and one member without any.
    let (editor_a, token_a) = account(&harness, Some(org_a)).await;
    grant(
        &harness,
        editor_a,
        org_a,
        &[
            "webhooks.read",
            "webhooks.manage",
            "events.read",
            "content.pages.read",
            "content.pages.create",
            "content.pages.update",
            "content.pages.publish",
        ],
    )
    .await;

    let (editor_b, token_b) = account(&harness, Some(org_b)).await;
    grant(
        &harness,
        editor_b,
        org_b,
        &["webhooks.read", "webhooks.manage", "events.read"],
    )
    .await;

    let (member, member_token) = account(&harness, Some(org_a)).await;
    let _ = member;

    // The gate: anonymous, and a member without the key.
    assert_eq!(
        harness.call(get("/api/v1/webhooks", None)).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        harness
            .call(get("/api/v1/webhooks", Some(&member_token)))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        harness
            .call(post(
                "/api/v1/webhooks",
                json!({ "name": "Nope", "url": receiver_a.url, "events": ["page.published"] }),
                Some(&member_token),
            ))
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    // Each organization connects its own endpoint; the organization account needs no body
    // organization_id — it works inside its own tenant by definition.
    let endpoint_a = harness
        .call(post(
            "/api/v1/webhooks",
            json!({ "name": "Receiver A", "url": receiver_a.url, "events": ["page.published"] }),
            Some(&token_a),
        ))
        .await;
    assert_eq!(
        endpoint_a.status,
        StatusCode::CREATED,
        "{:?}",
        endpoint_a.body
    );
    let endpoint_a_id = endpoint_a.body["id"].as_str().expect("id").to_owned();
    assert_eq!(endpoint_a.body["organization_id"], json!(org_a.to_string()));

    let endpoint_b = harness
        .call(post(
            "/api/v1/webhooks",
            json!({ "name": "Receiver B", "url": receiver_b.url, "events": ["page.published"] }),
            Some(&token_b),
        ))
        .await;
    assert_eq!(
        endpoint_b.status,
        StatusCode::CREATED,
        "{:?}",
        endpoint_b.body
    );
    let endpoint_b_id = endpoint_b.body["id"].as_str().expect("id").to_owned();

    // The list is tenant-scoped; the platform account sees both.
    let list_a = harness.call(get("/api/v1/webhooks", Some(&token_a))).await;
    assert_eq!(
        list_a.body["webhooks"].as_array().expect("webhooks").len(),
        1
    );
    assert_eq!(list_a.body["webhooks"][0]["id"], json!(endpoint_a_id));

    let list_owner = harness
        .call(get("/api/v1/webhooks", Some(&owner_token)))
        .await;
    assert_eq!(
        list_owner.body["webhooks"]
            .as_array()
            .expect("webhooks")
            .len(),
        2,
        "the platform account works across tenants"
    );

    // Reading or changing another organization's endpoint is a `403`, not a `404`.
    assert_eq!(
        harness
            .call(get(
                &format!("/api/v1/webhooks/{endpoint_a_id}"),
                Some(&token_b)
            ))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        harness
            .call(patch(
                &format!("/api/v1/webhooks/{endpoint_a_id}"),
                json!({ "enabled": false }),
                Some(&token_b),
            ))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        harness
            .call(get(
                &format!("/api/v1/webhooks/{endpoint_a_id}/deliveries"),
                Some(&token_b)
            ))
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    // A publication of organization A reaches A's endpoint and never B's.
    let page = harness
        .call(post(
            "/api/v1/pages",
            json!({ "site_id": site_a, "slug": "home", "title": "Tenant A home" }),
            Some(&token_a),
        ))
        .await;
    assert_eq!(page.status, StatusCode::CREATED, "{:?}", page.body);
    let page_id = page.body["id"].as_str().expect("page id").to_owned();
    let published = harness
        .call(post(
            &format!("/api/v1/pages/{page_id}/publish"),
            json!({}),
            Some(&token_a),
        ))
        .await;
    assert_eq!(published.status, StatusCode::OK, "{:?}", published.body);

    let report = tick(&harness).await;
    assert_eq!(report.delivered, 1, "{report:?}");
    assert_eq!(receiver_a.captured().len(), 1);
    assert_eq!(receiver_b.captured().len(), 0, "tenant B receives nothing");

    let deliveries_b = harness
        .call(get(
            &format!("/api/v1/webhooks/{endpoint_b_id}/deliveries"),
            Some(&token_b),
        ))
        .await;
    assert_eq!(
        deliveries_b.status,
        StatusCode::OK,
        "{:?}",
        deliveries_b.body
    );
    assert_eq!(
        deliveries_b.body["deliveries"]
            .as_array()
            .expect("deliveries")
            .len(),
        0,
        "no delivery was queued for tenant B: {:?}",
        deliveries_b.body
    );

    // The event feed is tenant-scoped too.
    let feed_b = harness.call(get("/api/v1/events", Some(&token_b))).await;
    assert_eq!(feed_b.body["events"], json!([]));
    let feed_a = harness.call(get("/api/v1/events", Some(&token_a))).await;
    assert_eq!(feed_a.body["events"].as_array().expect("events").len(), 1);

    // An endpoint switched off while its queue waits: the queued delivery settles as failed
    // instead of sitting pending forever.
    let queued = harness
        .call(post(
            "/api/v1/webhooks",
            json!({ "name": "Receiver A spare", "url": receiver_a.url, "events": ["page.published"] }),
            Some(&token_a),
        ))
        .await;
    assert_eq!(queued.status, StatusCode::CREATED, "{:?}", queued.body);
    let spare_id = queued.body["id"].as_str().expect("id").to_owned();

    harness
        .call(patch(
            &format!("/api/v1/pages/{page_id}"),
            json!({ "title": "Tenant A home, revised" }),
            Some(&token_a),
        ))
        .await;
    let republished = harness
        .call(post(
            &format!("/api/v1/pages/{page_id}/publish"),
            json!({}),
            Some(&token_a),
        ))
        .await;
    assert_eq!(republished.status, StatusCode::OK, "{:?}", republished.body);

    let disabled = harness
        .call(patch(
            &format!("/api/v1/webhooks/{spare_id}"),
            json!({ "enabled": false }),
            Some(&token_a),
        ))
        .await;
    assert_eq!(disabled.status, StatusCode::OK, "{:?}", disabled.body);
    assert_eq!(disabled.body["enabled"], json!(false));

    let report = tick(&harness).await;
    assert_eq!(
        report.cancelled, 1,
        "the switched-off endpoint settled its queue: {report:?}"
    );
    assert_eq!(
        report.delivered, 1,
        "the live endpoint still received the event"
    );

    let deliveries = harness
        .call(get(
            &format!("/api/v1/webhooks/{spare_id}/deliveries"),
            Some(&token_a),
        ))
        .await;
    let rows = deliveries.body["deliveries"]
        .as_array()
        .expect("deliveries")
        .clone();
    assert_eq!(rows.len(), 1, "{:?}", deliveries.body);
    assert_eq!(rows[0]["status"], json!("failed"));
    assert!(
        rows[0]["error"]
            .as_str()
            .expect("an error")
            .contains("switched off"),
        "{:?}",
        rows[0]
    );

    // A switched-off endpoint can still be tested (that is what the test delivery is for).
    let tested = harness
        .call(post(
            &format!("/api/v1/webhooks/{spare_id}/test"),
            json!({}),
            Some(&token_a),
        ))
        .await;
    assert_eq!(tested.status, StatusCode::ACCEPTED, "{:?}", tested.body);
    assert_eq!(
        tested.body["deliveries"],
        json!(1),
        "the test bypasses the switch"
    );

    // Removing an endpoint takes its queue with it.
    let removed = harness
        .call(request(
            Method::DELETE,
            &format!("/api/v1/webhooks/{spare_id}"),
            Some(&token_a),
            None,
        ))
        .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    assert_eq!(
        harness
            .call(get(&format!("/api/v1/webhooks/{spare_id}"), Some(&token_a)))
            .await
            .status,
        StatusCode::NOT_FOUND
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
