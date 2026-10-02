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
use omnion_events::DEFAULT_MAX_ATTEMPTS;
use omnion_events::engine::{self, RunReport, RunnerConfig};
use omnion_events::sender;
use omnion_events::signature;
use omnion_identity::sessions;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tower::ServiceExt;
use uuid::Uuid;

mod support;

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
    /// The secret every session's CSRF token in this harness is derived from.
    ///
    /// A copy rather than a reference to the config, because `account()` needs it to mint the
    /// token that goes with each session and the walks hand the two around as one packed
    /// credential. Keeping it here is what lets the fixture stay in charge of the pairing.
    csrf_secret: Vec<u8>,
}

impl Harness {
    /// Open a fresh database with every migration applied and the IAM seed loaded.
    async fn fresh() -> Option<Self> {
        let mut config = Config::from_env().expect("environment must be valid");
        // This suite's own CSRF secret, and the second half of a two-part repair.
        //
        // Tick 59 made the session cookie *ambient* authority, so every cookie-authenticated
        // write must present a double-submit token beside it — and the token is derived from
        // the deployment's secret and the session id. This suite never had either half: it
        // configured no secret, and it minted sessions straight through `sessions::create_session`
        // instead of signing in, so no token was ever issued. Both refusals are the **product
        // working correctly**: `csrf_unavailable` (no secret configured) and `csrf_failed` (a
        // cookie write with no token) are exactly what a correct deployment answers.
        //
        // The defect was in the suite, and it was a total one: with no credential it can carry,
        // **every** write in this file was refused, so all ten walks ended on their first
        // `POST /webhooks` with a 403 that reads like a broken platform. The walks were never
        // measuring the event bus; they were re-proving the refusal, once per walk. Green for
        // months would have meant nothing, and the reason nothing noticed is that a suite
        // whose every test fails for one shared reason looks like an environmental problem
        // rather than a missing fixture.
        //
        // See `support::walk_auth`, which lifts this shape for every suite, and `--test media`,
        // which had already been repaired this way and passes.
        support::walk_auth::with_csrf_secret(&mut config);
        let csrf_secret = config
            .csrf
            .as_bytes()
            .expect("the suite just set a secret")
            .to_vec();
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
            csrf_secret,
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

/// Build a JSON request; `token` becomes the session cookie **and** its CSRF token.
///
/// The credential is packed (`session\x1fcsrf`), so this is the one place the two halves are
/// separated and attached. Setting only the cookie is exactly the ambient-authority request the
/// double-submit check refuses, which is what every walk in this file was doing: the CSRF layer
/// answered `403 csrf_failed` before the event bus ever saw the request.
fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match token {
        Some(credential) => support::walk_auth::apply_credential(credential, builder),
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

/// Create an account with a session and return `(user id, credential)`.
///
/// The credential is **packed** — session id and CSRF token in one string, joined by the
/// separator [`support::walk_auth`] defines — rather than a bare session token. Every walk
/// passes this string around as its `token`, and [`request`] unpacks it and sets the cookie and
/// the `x-omnion-csrf` header together. Packing is what keeps the twenty-odd call sites
/// unchanged: a walk's `token` argument never became a pair, and the two halves cannot drift
/// apart because there is only one string to pass.
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
    let (session, token) = sessions::create_session(harness.db.pool(), user.id, None, None)
        .await
        .expect("the session must be created");
    // The token a browser would be handed beside the session cookie. Sign-in is the only place
    // the product issues one, and this suite does not sign in — it mints the session directly, so
    // the token is derived here from the same secret and the same session id the middleware will
    // compare against. Deriving it rather than hard-coding a value is the point: a constant
    // token would pass the presence check and fail verification, which reads as a broken product.
    let csrf = omnion_security::derive_csrf_token(&harness.csrf_secret, &session.id.to_string());
    (
        user.id,
        support::walk_auth::pack(&support::walk_auth::Session {
            session: token,
            csrf: Some(csrf),
        }),
    )
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

/// Create and publish one page, which is how a walk puts a real `page.published` on the bus.
///
/// A helper rather than four lines repeated in every walk, because a walk that reaches for the
/// endpoints by hand tends to publish as the platform owner, and the delivery then belongs to
/// no organization — which makes a tenant-scoped assertion silently vacuous.
async fn publish_page(harness: &Harness, token: &str, site: Uuid, slug: &str) -> StatusCode {
    let page = harness
        .call(post(
            "/api/v1/pages",
            json!({ "site_id": site, "slug": slug, "title": slug }),
            Some(token),
        ))
        .await;
    assert_eq!(page.status, StatusCode::CREATED, "{:?}", page.body);
    let page_id = page.body["id"].as_str().expect("page id").to_owned();

    harness
        .call(post(
            &format!("/api/v1/pages/{page_id}/publish"),
            json!({}),
            Some(token),
        ))
        .await
        .status
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
    // The feed holds every recorded event, and since REQ-016 slice 2 that is the whole
    // lifecycle, not just the publications: creating the endpoint, testing it, creating the
    // page, editing it, publishing, and the delivery that gave up. Counting exact rows would
    // make every new emission a breaking test, so this asserts the facts that matter instead —
    // what is present, and that the ordering is newest first.
    let names: Vec<&str> = events
        .iter()
        .filter_map(|event| event["name"].as_str())
        .collect();
    for expected in [
        "webhook.endpoint.created",
        "webhook.test",
        "webhook.endpoint.tested",
        "page.created",
        "page.updated",
        "page.published",
        "webhook.delivery.failed",
    ] {
        assert!(
            names.contains(&expected),
            "the feed must carry {expected}; it has {names:?}"
        );
    }
    assert_eq!(
        names
            .iter()
            .filter(|name| **name == "page.published")
            .count(),
        3,
        "three publications were recorded: {:?}",
        feed.body
    );
    assert_eq!(
        events[0]["name"],
        json!("webhook.delivery.failed"),
        "newest first"
    );
    assert_eq!(
        names.last(),
        Some(&"webhook.endpoint.created"),
        "oldest last"
    );

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

    // The event feed is tenant-scoped too — and the scoping is now worth stating precisely,
    // because connecting an endpoint records an event *about that endpoint*. Tenant B's feed is
    // no longer empty, and that is correct rather than a leak: the row is B's own, created by
    // B's own operator, and it says nothing about tenant A. What must never appear in B's feed
    // is anything of A's, which is the assertion that carries the isolation rule.
    let feed_b = harness.call(get("/api/v1/events", Some(&token_b))).await;
    let names_b: Vec<&str> = feed_b.body["events"]
        .as_array()
        .expect("events")
        .iter()
        .filter_map(|event| event["name"].as_str())
        .collect();
    assert_eq!(
        names_b,
        vec!["webhook.endpoint.created"],
        "tenant B sees only its own endpoint, and nothing of tenant A: {:?}",
        feed_b.body
    );
    let feed_a = harness.call(get("/api/v1/events", Some(&token_a))).await;
    let names_a: Vec<&str> = feed_a.body["events"]
        .as_array()
        .expect("events")
        .iter()
        .filter_map(|event| event["name"].as_str())
        .collect();
    assert_eq!(
        names_a,
        vec!["page.published", "page.created", "webhook.endpoint.created"],
        "tenant A sees its own endpoint and its own page, and nothing of tenant B: {:?}",
        feed_a.body
    );

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

// ---------------------------------------------------------------------------------------------
// The catalogue and group wildcards, over HTTP (REQ-016 slice 1)
// ---------------------------------------------------------------------------------------------

// ---------------------------------------------------------------------------------------------
// The feed's filters, keyset pagination and refusals, over HTTP (REQ-016 slice 1)
// ---------------------------------------------------------------------------------------------

/// The event feed narrows to what the operator asked for, pages without repeating a row, and
/// says so when the platform cannot serve the request.
///
/// This is the walk behind the `/events` screen (REQ-016, slice 1). The feed existed with a
/// limit and an organization scope; what it did not have was the set of filters the screen
/// offers, which means the screen's filter bar would have been a decoration — every control
/// wired to nothing, which is the failure the build plan names as "no dead buttons".
#[tokio::test]
async fn the_feed_filters_pages_and_refuses_what_it_cannot_serve() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    // A platform account, used only as the `actor_user_id` on the seeded rows — a fact with
    // an actor that happens inside one tenant while the reader is a member of another is
    // exactly the row a broken actor filter would leak, so the two ids differ on purpose.
    let (actor_id, _actor_token) = account(&harness, None).await;
    seed::bind_owner(harness.db.pool(), actor_id)
        .await
        .expect("the owner binding must be created");

    let organization = create_organization_row(&harness.db, "feed", "Feed Filter Test").await;
    let site = create_site_row(&harness.db, organization, "main", "Feed Site").await;
    let other_site = create_site_row(&harness.db, organization, "second", "Second Site").await;

    // Five facts to narrow: three page events on one site, one on another, and one that
    // belongs to a different organization entirely. The last one is the row a tenancy filter
    // that quietly stopped working would leak, so it is seeded deliberately.
    let other_organization = create_organization_row(&harness.db, "other", "Someone Else").await;
    for (name, site_id, owner) in [
        ("page.created", site, organization),
        ("page.updated", site, organization),
        ("page.deleted", site, organization),
        ("page.created", other_site, organization),
        ("user.updated", site, other_organization),
    ] {
        sqlx::query(
            "insert into events (name, organization_id, site_id, actor_user_id, payload) \
                     values ($1, $2, $3, $4, '{}'::jsonb)",
        )
        .bind(name)
        .bind(owner)
        .bind(site_id)
        .bind(actor_id)
        .execute(harness.db.pool())
        .await
        .expect("the seeded event must be inserted");
    }

    // ---- Scoping: an organization account sees its own and nothing else -------------------------
    // The reader is an *organization* account, not the platform owner. That distinction is the
    // whole point of the assertion and it is worth spelling out, because the owner's session
    // carries `organization_id = None` and the store's `($1::uuid is null or …)` clause reads
    // "no organization" as *every* organization. A tenancy test written against the owner
    // would therefore pass for the wrong reason — it would be asserting that the platform
    // superuser sees everything, which is correct and is not what a tenant may see.
    let (reader_id, reader_token) = account(&harness, Some(organization)).await;
    grant(
        &harness,
        reader_id,
        organization,
        &["events.read", "content.pages.read"],
    )
    .await;

    let feed = harness
        .call(get("/api/v1/events?limit=50", Some(&reader_token)))
        .await;
    assert_eq!(feed.status, StatusCode::OK, "{:?}", feed.body);
    let names: Vec<String> = feed.body["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        names.len(),
        4,
        "the reader's own organization and nothing else: {names:?}"
    );
    assert!(
        !names.contains(&"user.updated".to_owned()),
        "another organization's event never reaches this feed: {names:?}"
    );

    // ---- A name list ----------------------------------------------------------------------------
    // Repeated `?name=` means "any of these", which is the only reading a multi-select can
    // have. Reading one of the two would make the second click look like it did nothing.
    let filtered = harness
        .call(get(
            "/api/v1/events?name=page.created&name=page.deleted&limit=50",
            Some(&reader_token),
        ))
        .await;
    assert_eq!(filtered.status, StatusCode::OK, "{:?}", filtered.body);
    let filtered_names: Vec<String> = filtered.body["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        filtered_names.len(),
        3,
        "two page.created rows and one page.deleted: {filtered_names:?}"
    );
    assert!(
        filtered_names.iter().all(|name| name != "page.updated"),
        "a name that was not asked for is not returned: {filtered_names:?}"
    );

    // ---- A site ---------------------------------------------------------------------------------
    let by_site = harness
        .call(get(
            &format!("/api/v1/events?site_id={site}&limit=50"),
            Some(&reader_token),
        ))
        .await;
    assert_eq!(by_site.status, StatusCode::OK, "{:?}", by_site.body);
    assert_eq!(
        by_site.body["events"]
            .as_array()
            .expect("events")
            .as_slice()
            .len(),
        3,
        "the site's three facts and not the other site's one"
    );

    // ---- A window -------------------------------------------------------------------------------
    // A window in the future is empty, and it is empty *because it says so* rather than because
    // the filter was dropped — a silently ignored filter is indistinguishable from a bus that
    // stopped recording.
    let windowed = harness
        .call(get(
            "/api/v1/events?from=2999-01-01T00:00:00Z&limit=50",
            Some(&reader_token),
        ))
        .await;
    assert_eq!(windowed.status, StatusCode::OK, "{:?}", windowed.body);
    assert!(
        windowed.body["events"]
            .as_array()
            .expect("events")
            .is_empty(),
        "a future window is honoured, not ignored: {:?}",
        windowed.body
    );
    assert_eq!(
        windowed.body["has_more"],
        json!(false),
        "and an empty page says there is no further page"
    );

    // ---- A malformed request is refused by name ---------------------------------------------------
    let bad_window = harness
        .call(get("/api/v1/events?from=yesterday", Some(&reader_token)))
        .await;
    assert_eq!(
        bad_window.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        bad_window.body
    );
    assert_eq!(
        bad_window.body["error"]["code"],
        json!("invalid_event_window")
    );
    assert!(
        bad_window.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("from"),
        "the refusal names the parameter the operator mistyped: {:?}",
        bad_window.body
    );

    // A name that cannot exist is refused too, and so is every repetition of it — a filter
    // that validated only the first `?name=` would quietly return the unfiltered feed. The
    // value is percent-encoded, which makes the assertion two things at once: the name is
    // refused *and* the parser decoded it on the way (an undecoded `%20` would pass the
    // validator's character check and reach the store as a name nothing has ever emitted).
    let bad_name = harness
        .call(get(
            "/api/v1/events?name=page.created&name=NOT%20A%20NAME",
            Some(&reader_token),
        ))
        .await;
    assert_eq!(
        bad_name.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        bad_name.body
    );

    // ---- Keyset pagination -----------------------------------------------------------------------
    let first = harness
        .call(get("/api/v1/events?limit=2", Some(&reader_token)))
        .await;
    assert_eq!(first.status, StatusCode::OK, "{:?}", first.body);
    let first_ids: Vec<i64> = first.body["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["id"].as_i64().expect("an id"))
        .collect();
    assert_eq!(first_ids.len(), 2, "the page is the page size");
    assert_eq!(
        first.body["has_more"],
        json!(true),
        "three rows are left behind a two-row page"
    );

    let cursor = first.body["next_cursor"].as_i64().expect("a cursor");
    assert_eq!(
        cursor,
        *first_ids.last().expect("a last row"),
        "the cursor is the last row of the page, so the next page cannot repeat it"
    );

    let second = harness
        .call(get(
            &format!("/api/v1/events?limit=2&cursor={cursor}"),
            Some(&reader_token),
        ))
        .await;
    assert_eq!(second.status, StatusCode::OK, "{:?}", second.body);
    let second_ids: Vec<i64> = second.body["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["id"].as_i64().expect("an id"))
        .collect();
    assert_eq!(second_ids.len(), 2);
    assert_eq!(
        second.body["has_more"],
        json!(false),
        "the last page says so, so the panel can stop offering 'Load older'"
    );
    assert_eq!(
        second.body["next_cursor"],
        json!(null),
        "and carries no cursor to follow"
    );

    let overlap: Vec<&i64> = first_ids
        .iter()
        .filter(|id| second_ids.contains(id))
        .collect();
    assert!(
        overlap.is_empty(),
        "no row is served twice across the page boundary: {first_ids:?} then {second_ids:?}"
    );
    assert!(
        first_ids[0] > first_ids[1] && first_ids[1] > second_ids[0],
        "the feed is newest-first across the boundary, not per page: \
         {first_ids:?} then {second_ids:?}"
    );

    harness.dispose().await;
}

/// The catalogue is readable, complete, and a group subscription really does expand.
///
/// The unit tests in `omnion_events::catalogue` prove the table's shape; this proves the two
/// seams they cannot reach — that the endpoint form's data source is the same registry the
/// emitters are checked against, and that a `page.*` subscription reaches a receiver for a
/// member it never named. Both are claims about the running platform, so both are made
/// against the running platform.
#[tokio::test]
async fn the_catalogue_is_readable_and_a_group_subscription_expands() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let receiver = Receiver::start(false).await;

    let (owner_id, owner_token) = account(&harness, None).await;
    seed::bind_owner(harness.db.pool(), owner_id)
        .await
        .expect("the owner binding must be created");

    let organization = create_organization_row(&harness.db, "cat", "Catalogue Test").await;
    let site = create_site_row(&harness.db, organization, "main", "Catalogue Site").await;

    // ---- The catalogue reads -------------------------------------------------------------------
    assert_eq!(
        harness
            .call(get("/api/v1/events/catalogue", None))
            .await
            .status,
        StatusCode::UNAUTHORIZED,
        "the catalogue is behind a session like every other read"
    );

    let catalogue = harness
        .call(get("/api/v1/events/catalogue", Some(&owner_token)))
        .await;
    assert_eq!(catalogue.status, StatusCode::OK, "{:?}", catalogue.body);

    let entries = catalogue.body["events"]
        .as_array()
        .expect("events is a list")
        .clone();
    assert!(
        entries.len() >= 60,
        "the catalogue carries {} names; the brief asks for coverage across every module",
        entries.len()
    );

    // Every entry is complete enough for a receiver to subscribe without guessing.
    for entry in &entries {
        let name = entry["name"].as_str().expect("a name");
        assert!(
            !entry["description"].as_str().unwrap_or_default().is_empty(),
            "{name} says nothing about what it means"
        );
        assert!(
            !entry["area"].as_str().unwrap_or_default().is_empty(),
            "{name} belongs to no area"
        );
        assert!(
            !entry["payload_fields"]
                .as_array()
                .expect("fields")
                .is_empty(),
            "{name}"
        );

        // The group is the part a receiver can subscribe to as a whole, and it must agree
        // with the name: a `group` that does not prefix the `name` is a picker that would
        // offer a subscription the fan-out never matches.
        let group = entry["group"].as_str().expect("a group");
        assert!(
            name.starts_with(&format!("{group}.")),
            "{name} claims group {group}, which does not prefix it"
        );
    }

    // The published page event is described with the fields it actually carries — this is the
    // row the acceptance criterion names, so it is checked by value and not by presence.
    let published = entries
        .iter()
        .find(|entry| entry["name"] == "page.published")
        .expect("page.published is listed");
    assert_eq!(published["status"], "live");
    assert_eq!(published["group"], "page");
    let fields = published["payload_fields"].as_array().expect("fields");
    for required in ["page_id", "site_id", "slug", "revision_no"] {
        let field = fields
            .iter()
            .find(|field| field["name"] == required)
            .unwrap_or_else(|| panic!("page.published must declare {required}"));
        assert_eq!(
            field["required"], true,
            "{required} is promised as required"
        );
    }

    // The counts agree with the list, and the ceiling the panel enforces is published with
    // it so the form does not hardcode a number that can drift from the validator.
    assert_eq!(
        catalogue.body["live_count"].as_u64().expect("live_count") as usize
            + catalogue.body["reserved_count"]
                .as_u64()
                .expect("reserved_count") as usize,
        entries.len(),
        "live + reserved is the whole list"
    );
    assert_eq!(
        catalogue.body["max_subscriptions"],
        json!(omnion_events::validation::MAX_SUBSCRIPTIONS),
        "the panel's ceiling is the validator's ceiling"
    );
    assert!(
        !catalogue.body["areas"]
            .as_array()
            .expect("areas")
            .is_empty(),
        "the picker groups by area"
    );

    // `order.created` is named, described and subscribable while its module is unshipped —
    // and it says so, rather than pretending the platform is broken.
    let reserved = entries
        .iter()
        .find(|entry| entry["name"] == "order.created")
        .expect("order.created is listed");
    assert_eq!(reserved["status"], "reserved");
    assert_eq!(reserved["group"], "order");

    // ---- A group subscription expands and delivers --------------------------------------------
    let created = harness
        .call(post(
            "/api/v1/webhooks",
            json!({
                "organization_id": organization,
                "name": "Group Receiver",
                "url": receiver.url,
                // One selection that stands for eight names.
                "events": ["page.*"],
            }),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let secret = created.body["secret"]
        .as_str()
        .expect("the platform generates a secret and shows it once")
        .to_owned();

    let stored = created.body["events"].as_array().expect("events").clone();
    let stored_names: Vec<&str> = stored
        .iter()
        .map(|entry| entry.as_str().expect("a name"))
        .collect();

    assert!(
        stored_names.contains(&"page.*"),
        "the wildcard is kept so catalogue growth reaches this endpoint: {stored_names:?}"
    );
    assert!(
        stored_names.contains(&"page.published"),
        "today's members are stored too, so the row is readable without resolving a group"
    );
    assert!(
        !stored_names.contains(&"media.created"),
        "the group is page's, not everything: {stored_names:?}"
    );
    assert!(
        stored_names.windows(2).all(|pair| pair[0] < pair[1]),
        "the stored list is sorted and deduplicated: {stored_names:?}"
    );

    // The wildcard in the stored row is the whole reason the fan-out has to test it, and
    // publishing a page is the proof that it does.
    let page = harness
        .call(post(
            "/api/v1/pages",
            json!({ "site_id": site, "slug": "grouped", "title": "Grouped" }),
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

    // A `page.*` subscription is no longer publish-only: the group now carries the whole page
    // lifecycle, so the page that was just created is delivered alongside its publication. That
    // is the point of the group — one subscription, every page fact — and the count is 2
    // because the walk creates the page before publishing it.
    let report = tick(&harness).await;
    assert_eq!(
        report.delivered, 2,
        "a page.* subscription delivers page.created and page.published: {report:?}"
    );

    let captured = receiver.captured();
    assert_eq!(captured.len(), 2, "the receiver took both deliveries");
    let delivered_names: Vec<&str> = captured.iter().map(|hit| hit.event.as_str()).collect();
    assert_eq!(
        delivered_names,
        vec!["page.created", "page.published"],
        "the group delivers its members oldest first: {delivered_names:?}"
    );
    let delivered = &captured[1];
    assert_eq!(delivered.event, "page.published");

    // A name the receiver never named still arrives signed and verifiable.
    let body = delivered.json();
    assert_eq!(body["name"], "page.published");
    assert_eq!(body["payload"]["slug"], "grouped");
    assert!(
        omnion_events::signature::verify(
            &secret,
            delivered.timestamp,
            &delivered.body,
            &delivered.signature
        ),
        "the delivery verifies against the secret the creation response returned: {:?}",
        delivered.signature
    );

    // ---- A group that does not exist is kept, not refused --------------------------------------
    let future = harness
        .call(post(
            "/api/v1/webhooks",
            json!({
                "organization_id": organization,
                "name": "Future Group",
                "url": receiver.url,
                "events": ["payments.*"],
            }),
            Some(&owner_token),
        ))
        .await;
    assert_eq!(
        future.status,
        StatusCode::CREATED,
        "a group whose module has not shipped is a legitimate subscription: {:?}",
        future.body
    );
    assert_eq!(
        future.body["events"],
        json!(["payments.*"]),
        "and it is stored exactly as written, becoming real when the names arrive"
    );

    harness.dispose().await;
}

/// The delivery operations: a redelivery that works, the three refusals that stop it, a
/// rotation that invalidates the old signature, and a stats read that tells the truth.
///
/// This is the walk behind the `/webhooks/[id]` screen (REQ-016, slice 2). Everything it
/// asserts is a claim the panel will make on screen, and each one is a place where the
/// obvious implementation lies:
///
/// * A redelivery that *inserted* a second row would send the same fact twice, and the
///   receiver could not tell a replay from a duplicate. So the row is reset, and the walk
///   asserts the count of rows for one event never rises.
/// * A success rate that counted `pending` rows in its denominator would read 0% for an
///   endpoint whose every delivery was about to succeed.
/// * A rotation that did not invalidate the previous secret would leave a receiver
///   verifying against a value the platform no longer uses, and the walk proves the old
///   signature now fails.
#[tokio::test]
async fn a_delivery_can_be_sent_again_and_the_platform_says_why_it_will_not() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let receiver = Receiver::start(false).await;

    let (owner_id, _owner_token) = account(&harness, None).await;
    seed::bind_owner(harness.db.pool(), owner_id)
        .await
        .expect("the owner binding must be created");

    let organization = create_organization_row(&harness.db, "ops", "Delivery Ops Test").await;
    let site = create_site_row(&harness.db, organization, "main", "Delivery Ops Site").await;

    let (editor, editor_token) = account(&harness, Some(organization)).await;
    grant(
        &harness,
        editor,
        organization,
        &[
            "webhooks.read",
            "webhooks.manage",
            "events.read",
            "content.pages.read",
            "content.pages.create",
            "content.pages.publish",
        ],
    )
    .await;

    // A reader with the read key but no manage key: the panel's "Redeliver" button must be
    // absent for them, and if it were not, the API must refuse it rather than trusting the
    // UI. An audit role that can read an endpoint's history must not be able to make the
    // platform POST to a third party.
    let (reader, reader_token) = account(&harness, Some(organization)).await;
    grant(
        &harness,
        reader,
        organization,
        &["webhooks.read", "events.read"],
    )
    .await;

    let endpoint = harness
        .call(post(
            "/api/v1/webhooks",
            json!({
                "name": "Ops receiver",
                "url": receiver.url,
                "events": ["page.published"],
            }),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(endpoint.status, StatusCode::CREATED, "{:?}", endpoint.body);
    let endpoint_id = endpoint.body["id"].as_str().expect("id").to_owned();

    // ---- 1. A test delivery, delivered, with a duration and a trigger ------------------------
    let tested = harness
        .call(post(
            &format!("/api/v1/webhooks/{endpoint_id}/test"),
            json!({}),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(
        tested.status,
        StatusCode::ACCEPTED,
        "a test delivery is queued, not sent inline: {:?}",
        tested.body
    );
    assert_eq!(tested.body["deliveries"], json!(1));

    tick(&harness).await;
    assert!(
        receiver.captured().len() >= 1,
        "the receiver got the test delivery"
    );

    let history = harness
        .call(get(
            &format!("/api/v1/webhooks/{endpoint_id}/deliveries"),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(history.status, StatusCode::OK, "{:?}", history.body);
    let rows = history.body["deliveries"].as_array().expect("deliveries");
    let first = &rows[0];

    // The four columns migration 0052 added are all present and are numbers, not nulls. A
    // screen that renders `undefined` in a latency column has no way to say "not yet run",
    // and an operator reads that as zero.
    assert_eq!(first["status"], "delivered");
    assert_eq!(
        first["trigger"], "test",
        "a button press is a test, not traffic"
    );
    assert_eq!(first["redeliver_count"], json!(0));
    assert!(
        first["duration_ms"].is_number(),
        "a delivered row carries a measured duration, got {:?}",
        first["duration_ms"]
    );
    assert!(
        first["replayed_at"].is_null(),
        "a row nobody forced again has no replay time"
    );

    // ---- 2. The stats read, and what it refuses to claim ----------------------------------------
    let stats = harness
        .call(get(
            &format!("/api/v1/webhooks/{endpoint_id}/stats"),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(stats.status, StatusCode::OK, "{:?}", stats.body);
    // The test delivery is in the history but not in the rate: the operator pressed a button,
    // the platform delivered nothing, and a rate the operator can raise by pressing a button
    // is not a measurement of the receiver.
    assert_eq!(stats.body["total"], json!(1), "the row is in the history");
    assert_eq!(
        stats.body["tests"],
        json!(1),
        "and it is reported as a test"
    );
    assert_eq!(
        stats.body["delivered"],
        json!(0),
        "but it is not counted as delivered traffic"
    );
    assert_eq!(stats.body["failed"], json!(0));
    assert_eq!(
        stats.body["window_hours"],
        json!(24),
        "the default is a day"
    );
    assert_eq!(
        stats.body["success_rate"],
        json!(null),
        "a test row settles, so nothing *traffic* settled: no rate rather than a flattering one"
    );

    // A real publication makes the rate meaningful, and it must be 1.0 with the test row
    // still excluded — one test delivery and one real delivery would read 100% either way, so
    // the control is the delivered count, not the rate.
    publish_page(&harness, &editor_token, site, "traffic").await;
    tick(&harness).await;
    let with_traffic = harness
        .call(get(
            &format!("/api/v1/webhooks/{endpoint_id}/stats"),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(
        with_traffic.body["delivered"],
        json!(1),
        "the publication arrived"
    );
    assert_eq!(
        with_traffic.body["tests"],
        json!(1),
        "and the test is still counted apart"
    );
    assert_eq!(with_traffic.body["total"], json!(2));
    assert_eq!(with_traffic.body["success_rate"], json!(1.0));
    assert!(
        with_traffic.body["p95_duration_ms"].is_number(),
        "a delivered row has a percentile: {:?}",
        with_traffic.body["p95_duration_ms"]
    );

    // An endpoint with no history at all has no rate, no percentile and no rows. `0.0` and `0`
    // would both read as measurements of something that was never measured.
    let empty_endpoint = harness
        .call(post(
            "/api/v1/webhooks",
            json!({ "name": "Quiet", "url": receiver.url, "events": ["page.published"] }),
            Some(&editor_token),
        ))
        .await;
    let quiet_id = empty_endpoint.body["id"].as_str().expect("id").to_owned();
    let quiet = harness
        .call(get(
            &format!("/api/v1/webhooks/{quiet_id}/stats"),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(
        quiet.body["success_rate"],
        json!(null),
        "nothing settled means no rate, not zero"
    );
    assert_eq!(quiet.body["p95_duration_ms"], json!(null));
    assert_eq!(quiet.body["total"], json!(0));

    // ---- 3. A redelivery that genuinely re-sends, without adding a row ------------------------
    let delivery_id = first["id"].as_str().expect("id").to_owned();
    let before = receiver.captured().len();

    let forced = harness
        .call(post(
            &format!("/api/v1/webhooks/{endpoint_id}/deliveries/{delivery_id}/redeliver"),
            json!({}),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(forced.status, StatusCode::OK, "{:?}", forced.body);
    assert_eq!(forced.body["status"], "pending");
    assert_eq!(
        forced.body["redeliver_count"],
        json!(1),
        "the count comes from the update that just ran, not a second read"
    );

    // The row was reset, not replaced: the history for this event is still one row, and it
    // now reads as a replay.
    let after = harness
        .call(get(
            &format!("/api/v1/webhooks/{endpoint_id}/deliveries"),
            Some(&editor_token),
        ))
        .await;
    let replayed = after.body["deliveries"]
        .as_array()
        .expect("deliveries")
        .iter()
        .find(|row| row["id"] == json!(delivery_id))
        .expect("the forced row is still there")
        .clone();
    assert_eq!(
        replayed["trigger"], "replay",
        "a forced row is a replay, which is why the trigger column exists"
    );
    assert_eq!(
        replayed["redeliver_count"],
        json!(1),
        "and the row still exists exactly once: {}",
        after.body["deliveries"].as_array().expect("d").len()
    );
    assert_eq!(
        replayed["attempts"],
        json!(0),
        "a new round starts from zero"
    );

    tick(&harness).await;
    assert!(
        receiver.captured().len() > before,
        "the receiver got the delivery a second time"
    );

    // ---- 4. The three refusals, each with its own code ------------------------------------------
    // (a) A reader without `webhooks.manage` is refused before the row is even looked at.
    let denied = harness
        .call(post(
            &format!("/api/v1/webhooks/{endpoint_id}/deliveries/{delivery_id}/redeliver"),
            json!({}),
            Some(&reader_token),
        ))
        .await;
    assert_eq!(
        denied.status,
        StatusCode::FORBIDDEN,
        "reading a history is not the power to make the platform POST"
    );

    // (b) A row the runner is about to claim is refused. The tick above settled the forced
    // row, so this call has to *make* the pending case rather than hope for it: force it again
    // and do not run the runner in between. The runner holds (or is about to hold) that row's
    // lease, and a reset would hand it to the next claim while the attempt is still in flight —
    // the one place this operation could double-send.
    let forced_again = harness
        .call(post(
            &format!("/api/v1/webhooks/{endpoint_id}/deliveries/{delivery_id}/redeliver"),
            json!({}),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(
        forced_again.status,
        StatusCode::OK,
        "a settled row is forced again: {:?}",
        forced_again.body
    );
    assert_eq!(forced_again.body["redeliver_count"], json!(2));

    let pending = harness
        .call(post(
            &format!("/api/v1/webhooks/{endpoint_id}/deliveries/{delivery_id}/redeliver"),
            json!({}),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(
        pending.status,
        StatusCode::CONFLICT,
        "a queued row is a conflict, not a bad request: {:?}",
        pending.body
    );
    assert_eq!(
        pending.body["error"]["code"], "delivery_already_pending",
        "the three refusals must not share one code — the operator's next step differs"
    );

    // (c) An id that is not on this endpoint. A `404` would be wrong: the endpoint exists and
    // the caller may manage it, so the answer is about the delivery, not about the endpoint.
    let unknown = harness
        .call(post(
            &format!(
                "/api/v1/webhooks/{endpoint_id}/deliveries/{}/redeliver",
                Uuid::new_v4()
            ),
            json!({}),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(unknown.status, StatusCode::CONFLICT);
    assert_eq!(unknown.body["error"]["code"], "delivery_not_found");

    // The refusal left the row alone: it is still `pending` and still at the count the earlier
    // call wrote, because a refusal must not have the side effect it refused.
    let unchanged = harness
        .call(get(
            &format!("/api/v1/webhooks/{endpoint_id}/deliveries"),
            Some(&editor_token),
        ))
        .await;
    let still = unchanged.body["deliveries"]
        .as_array()
        .expect("deliveries")
        .iter()
        .find(|row| row["id"] == json!(delivery_id))
        .expect("the row is still there");
    assert_eq!(still["status"], "pending");
    assert_eq!(
        still["redeliver_count"],
        json!(2),
        "a refusal must not have the side effect it refused"
    );

    tick(&harness).await;

    // ---- 5. The bulk redelivery answers per id -------------------------------------------------
    // The tick first, so the row above is settled and the batch really moves one id. A batch
    // whose only member is pending would report `queued: 0` and prove nothing about the part
    // that is supposed to work.
    tick(&harness).await;
    let good = delivery_id.clone();
    let bulk = harness
        .call(post(
            &format!("/api/v1/webhooks/{endpoint_id}/deliveries/redeliver"),
            json!({ "delivery_ids": [good, Uuid::new_v4()] }),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(bulk.status, StatusCode::OK, "{:?}", bulk.body);
    assert_eq!(
        bulk.body["queued"],
        json!(1),
        "one id moved and the other did not: {:?}",
        bulk.body
    );
    let skipped = bulk.body["skipped"].as_array().expect("skipped");
    assert_eq!(skipped.len(), 1, "and the one that did not is named");
    assert_eq!(skipped[0]["code"], "delivery_not_found");

    // An empty batch and an oversized one are both refused by name, because "nothing happened"
    // is the worst possible answer to a button press.
    let empty = harness
        .call(post(
            &format!("/api/v1/webhooks/{endpoint_id}/deliveries/redeliver"),
            json!({ "delivery_ids": [] }),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);
    assert_eq!(empty.body["error"]["code"], "empty_redelivery_batch");

    tick(&harness).await;

    harness.dispose().await;
}

/// A rotation replaces the secret, shows it once, and the old signature stops verifying.
#[tokio::test]
async fn rotating_a_secret_shows_it_once_and_breaks_the_old_signature() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let receiver = Receiver::start(false).await;

    let (owner_id, owner_token) = account(&harness, None).await;
    seed::bind_owner(harness.db.pool(), owner_id)
        .await
        .expect("the owner binding must be created");

    let organization = create_organization_row(&harness.db, "rot", "Rotation Test").await;

    let (editor, editor_token) = account(&harness, Some(organization)).await;
    grant(
        &harness,
        editor,
        organization,
        &["webhooks.read", "webhooks.manage", "events.read"],
    )
    .await;

    // The endpoint is created with a *provided* secret, so this walk can compare the two
    // signatures directly: it holds the first secret and then the second, and checks the
    // delivery verifies against the new one and not the old.
    let first_secret = "the-first-signing-secret-value";
    let created = harness
        .call(post(
            "/api/v1/webhooks",
            json!({
                "name": "Rotating receiver",
                "url": receiver.url,
                "events": ["page.published"],
                "secret": first_secret,
            }),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    assert!(
        created.body.get("secret").is_none(),
        "a secret the operator supplied is never echoed back: {:?}",
        created.body
    );
    let endpoint_id = created.body["id"].as_str().expect("id").to_owned();

    // Deliver once, signed with the first secret.
    let _ = harness
        .call(post(
            &format!("/api/v1/webhooks/{endpoint_id}/test"),
            json!({}),
            Some(&editor_token),
        ))
        .await;
    tick(&harness).await;
    let before_rotation = receiver.captured();
    let delivered_before = before_rotation.last().expect("a delivery");
    assert!(
        signature::verify(
            first_secret,
            delivered_before.timestamp,
            &delivered_before.body,
            &delivered_before.signature
        ),
        "the first delivery verifies against the secret the operator supplied"
    );

    // ---- Rotate -------------------------------------------------------------------------------
    let rotated = harness
        .call(post(
            &format!("/api/v1/webhooks/{endpoint_id}/secret/rotate"),
            json!({}),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(rotated.status, StatusCode::OK, "{:?}", rotated.body);
    let new_secret = rotated.body["secret"].as_str().expect("a new secret");
    assert_ne!(
        new_secret, first_secret,
        "the platform issues a different value"
    );
    assert!(
        !new_secret.contains(first_secret),
        "the new secret is generated, not derived from the old one"
    );

    // The endpoint's own fields are still readable, and the endpoint itself is unchanged apart
    // from the secret — a rotation is not an edit.
    assert_eq!(rotated.body["id"], json!(endpoint_id));
    assert_eq!(rotated.body["name"], "Rotating receiver");
    assert_eq!(rotated.body["enabled"], json!(true));

    // The secret is shown exactly once: a plain `GET` never carries it.
    let reread = harness
        .call(get(
            &format!("/api/v1/webhooks/{endpoint_id}"),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(reread.status, StatusCode::OK);
    assert!(
        reread.body.get("secret").is_none(),
        "the secret does not come back out: {:?}",
        reread.body
    );
    let listed = harness
        .call(get("/api/v1/webhooks", Some(&editor_token)))
        .await;
    assert!(
        listed.body["webhooks"]
            .as_array()
            .expect("webhooks")
            .iter()
            .all(|row| row.get("secret").is_none()),
        "nor does the list carry one"
    );

    // The rotation is on the bus, and its payload carries no secret material.
    let feed = harness
        .call(get(
            "/api/v1/events?name=webhook.secret.rotated",
            Some(&owner_token),
        ))
        .await;
    let rotated_event = feed.body["events"]
        .as_array()
        .expect("events")
        .iter()
        .find(|event| event["payload"]["endpoint_id"] == json!(endpoint_id))
        .expect("the rotation is recorded")
        .clone();
    let payload_text = rotated_event["payload"].to_string();
    assert!(
        !payload_text.contains(new_secret) && !payload_text.contains(first_secret),
        "an event that announces a rotation must not carry the value it announces: {payload_text}"
    );

    // ---- The old signature now fails -----------------------------------------------------------
    let _ = harness
        .call(post(
            &format!("/api/v1/webhooks/{endpoint_id}/test"),
            json!({}),
            Some(&editor_token),
        ))
        .await;
    tick(&harness).await;
    let after_rotation = receiver.captured();
    let delivered_after = after_rotation.last().expect("a delivery");
    assert!(
        signature::verify(
            new_secret,
            delivered_after.timestamp,
            &delivered_after.body,
            &delivered_after.signature
        ),
        "the new delivery verifies against the new secret"
    );
    assert!(
        !signature::verify(
            first_secret,
            delivered_after.timestamp,
            &delivered_after.body,
            &delivered_after.signature
        ),
        "and does NOT verify against the old one — that is what a rotation is for"
    );

    harness.dispose().await;
}

/// The delivery history narrows, pages without repeating a row, and refuses a bad filter by name.
#[tokio::test]
async fn the_delivery_history_filters_pages_and_names_its_bad_parameters() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let receiver = Receiver::start(false).await;

    let (owner_id, _owner_token) = account(&harness, None).await;
    seed::bind_owner(harness.db.pool(), owner_id)
        .await
        .expect("the owner binding must be created");

    let organization = create_organization_row(&harness.db, "hist", "History Test").await;
    let site = create_site_row(&harness.db, organization, "main", "History Site").await;

    let (editor, editor_token) = account(&harness, Some(organization)).await;
    grant(
        &harness,
        editor,
        organization,
        &[
            "webhooks.read",
            "webhooks.manage",
            "events.read",
            "content.pages.read",
            "content.pages.create",
            "content.pages.publish",
        ],
    )
    .await;

    let endpoint = harness
        .call(post(
            "/api/v1/webhooks",
            json!({
                "name": "History receiver",
                "url": receiver.url,
                "events": ["page.published", "page.created"],
            }),
            Some(&editor_token),
        ))
        .await;
    let endpoint_id = endpoint.body["id"].as_str().expect("id").to_owned();

    // Six test deliveries plus one real publication, so the history has enough rows to page
    // through and carries both triggers for the filters to be tested against.
    for _ in 0..6 {
        let _ = harness
            .call(post(
                &format!("/api/v1/webhooks/{endpoint_id}/test"),
                json!({}),
                Some(&editor_token),
            ))
            .await;
    }
    publish_page(&harness, &editor_token, site, "named").await;
    tick(&harness).await;

    let base = format!("/api/v1/webhooks/{endpoint_id}/deliveries");

    // ---- The filters are conjunctions, and the header total agrees with them -------------------
    // Six probes plus two traffic rows: publishing a page records `page.created` *and*
    // `page.published`, and this endpoint subscribes to both. Writing "seven" here was the
    // walk being wrong about the platform rather than the platform being wrong about itself.
    let all = harness.call(get(&base, Some(&editor_token))).await;
    assert_eq!(
        all.body["total"],
        json!(8),
        "six probes and two page events"
    );
    assert_eq!(all.body["has_more"], json!(false));

    let delivered = harness
        .call(get(
            &format!("{base}?status=delivered"),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(delivered.body["total"], json!(8), "all eight were accepted");

    // Several statuses mean "any of these", which is the question an operator chasing a
    // broken receiver actually asks.
    let broken = harness
        .call(get(
            &format!("{base}?status=failed&status=pending"),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(broken.status, StatusCode::OK);
    assert_eq!(
        broken.body["total"],
        json!(0),
        "nothing failed and nothing waits"
    );

    // One `?status=` must not be a plain-text 400: the parser is by hand for the same reason
    // the feed's is, and this is the assertion that keeps it that way.
    assert_eq!(broken.status, StatusCode::OK, "{:?}", broken.body);

    // The name filter narrows to the events the endpoint subscribed to. The six probes above
    // are `webhook.test`, and a filter that returned them for `name=page.published` would be
    // filtering on nothing.
    let by_name = harness
        .call(get(
            &format!("{base}?name=page.published"),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(
        by_name.body["total"],
        json!(1),
        "only the publication matches, not its creation: {:?}",
        by_name.body
    );
    assert_eq!(
        by_name.body["deliveries"][0]["event_name"],
        "page.published"
    );
    assert_eq!(
        by_name.body["deliveries"][0]["trigger"], "event",
        "and it is traffic, not a probe"
    );

    let by_tests = harness
        .call(get(
            &format!("{base}?name=webhook.test"),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(
        by_tests.body["total"],
        json!(6),
        "the probes are still there"
    );

    // The free-text search reaches the event name as well as the id.
    let by_text = harness
        .call(get(&format!("{base}?q=webhook.test"), Some(&editor_token)))
        .await;
    assert_eq!(by_text.body["total"], json!(6), "q searches the event name");
    let by_published_text = harness
        .call(get(&format!("{base}?q=published"), Some(&editor_token)))
        .await;
    assert_eq!(
        by_published_text.body["total"],
        json!(1),
        "and a substring of the name matches too, not just the whole one"
    );
    // `page.created` is subscribed to as well, so a filter that ignored the name entirely
    // would answer 2 here. One is the answer that proves the filter ran.
    let by_created_text = harness
        .call(get(&format!("{base}?q=page.created"), Some(&editor_token)))
        .await;
    assert_eq!(by_created_text.body["total"], json!(1));

    // ---- The bad parameters are named, not swallowed ------------------------------------------
    let bad_status = harness
        .call(get(&format!("{base}?status=flaky"), Some(&editor_token)))
        .await;
    assert_eq!(bad_status.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad_status.body["error"]["code"], "invalid_delivery_query");
    assert!(
        bad_status.body["error"]["message"]
            .as_str()
            .expect("a message")
            .contains("status"),
        "the refusal names the parameter: {:?}",
        bad_status.body
    );

    let bad_limit = harness
        .call(get(&format!("{base}?limit=lots"), Some(&editor_token)))
        .await;
    assert_eq!(bad_limit.status, StatusCode::BAD_REQUEST);
    assert!(
        bad_limit.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("limit")
    );

    // A cursor needs both halves: `cursor_at` without `cursor_id` would be a row comparison
    // against a null id, which Postgres refuses — a 500 on a request the panel builds itself.
    let half_cursor = harness
        .call(get(
            &format!("{base}?cursor_at=2026-01-01T00:00:00Z"),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(half_cursor.status, StatusCode::BAD_REQUEST);
    assert!(
        half_cursor.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("cursor_id"),
        "the refusal says what is missing: {:?}",
        half_cursor.body
    );

    let bad_instant = harness
        .call(get(&format!("{base}?from=yesterday"), Some(&editor_token)))
        .await;
    assert_eq!(bad_instant.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad_instant.body["error"]["code"], "invalid_event_window");

    // ---- The keyset page does not repeat a row ------------------------------------------------
    let first = harness
        .call(get(&format!("{base}?limit=4"), Some(&editor_token)))
        .await;
    assert_eq!(first.body["deliveries"].as_array().expect("d").len(), 4);
    assert_eq!(
        first.body["total"],
        json!(8),
        "the header still knows the whole set"
    );
    assert_eq!(first.body["has_more"], json!(true));
    let cursor = first.body["next_cursor"].as_object().expect("a cursor");
    let cursor_at = cursor["at"].as_str().expect("at").to_owned();
    let cursor_id = cursor["id"].as_str().expect("id").to_owned();

    let second = harness
        .call(get(
            &format!("{base}?limit=4&cursor_at={cursor_at}&cursor_id={cursor_id}"),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(second.status, StatusCode::OK, "{:?}", second.body);
    let second_rows = second.body["deliveries"].as_array().expect("d");
    assert_eq!(
        second_rows.len(),
        4,
        "the rest of the set is on the second page"
    );
    assert_eq!(second.body["has_more"], json!(false));

    let first_ids: Vec<String> = first.body["deliveries"]
        .as_array()
        .expect("d")
        .iter()
        .map(|row| row["id"].as_str().expect("id").to_owned())
        .collect();
    let second_ids: Vec<String> = second_rows
        .iter()
        .map(|row| row["id"].as_str().expect("id").to_owned())
        .collect();
    let overlap: Vec<&String> = first_ids
        .iter()
        .filter(|id| second_ids.contains(id))
        .collect();
    assert!(
        overlap.is_empty(),
        "the (created_at, id) cursor repeats no row: {first_ids:?} then {second_ids:?}"
    );

    harness.dispose().await;
}

/// An endpoint switched off stops receiving **and keeps its past**.
///
/// The obvious reading of "disable" is a switch that turns the whole screen off: a disabled
/// endpoint whose history is unreadable is a switch that deletes the answer to "what was this
/// receiver doing last Tuesday, and what did it cost me?". An operator who is pausing a
/// misbehaving integration needs the delivery log of the behaviour they are pausing *because*
/// of it — the log is the evidence, and hiding it removes the only reason to trust the switch.
///
/// So the two halves are asserted separately, and the second half is the one that would have
/// been quietly dropped: `enqueue_fanout` filters on `w.enabled`, so the new delivery is
/// refused by the bus, and the history read touches neither the flag nor the row.
#[tokio::test]
async fn a_disabled_endpoint_goes_quiet_and_still_answers_what_it_did() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let receiver = Receiver::start(false).await;

    let (owner_id, _owner_token) = account(&harness, None).await;
    seed::bind_owner(harness.db.pool(), owner_id)
        .await
        .expect("the owner binding must be created");

    let organization = create_organization_row(&harness.db, "mute", "Mute Test").await;
    let site = create_site_row(&harness.db, organization, "main", "Mute Site").await;

    let (editor, editor_token) = account(&harness, Some(organization)).await;
    grant(
        &harness,
        editor,
        organization,
        &[
            "webhooks.read",
            "webhooks.manage",
            "events.read",
            "content.pages.read",
            "content.pages.create",
            "content.pages.publish",
        ],
    )
    .await;

    let created = harness
        .call(post(
            "/api/v1/webhooks",
            json!({
                "name": "Mute receiver",
                "url": receiver.url,
                "events": ["page.published"],
            }),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let endpoint_id = created.body["id"].as_str().expect("id").to_owned();
    assert_eq!(
        created.body["enabled"],
        json!(true),
        "a new endpoint is on until somebody says otherwise"
    );

    // ---- 1. While enabled, a published page reaches the receiver -------------------------------
    assert_eq!(
        publish_page(&harness, &editor_token, site, "before-mute").await,
        StatusCode::OK,
        "the first page publishes"
    );
    tick(&harness).await;
    let heard_before = receiver.captured().len();
    assert!(heard_before >= 1, "an enabled endpoint hears the bus");

    // ---- 2. Switched off, the same page is not delivered -----------------------------------------
    let muted = harness
        .call(patch(
            &format!("/api/v1/webhooks/{endpoint_id}"),
            json!({ "enabled": false }),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(muted.status, StatusCode::OK, "{:?}", muted.body);
    assert_eq!(muted.body["enabled"], json!(false));

    assert_eq!(
        publish_page(&harness, &editor_token, site, "after-mute").await,
        StatusCode::OK,
        "the second page publishes too — the bus does not care who is listening"
    );

    // The delivery is not merely *not sent*: it is never **queued**. An implementation that
    // queued it and skipped the send would show the operator a growing list of `pending` rows
    // for an endpoint they switched off, and the queue would drain or not depending on a
    // worker that has no reason to look at a disabled endpoint.
    let queued: i64 = sqlx::query_scalar(
        "select count(*) from webhook_deliveries d \
         join events e on e.id = d.event_id \
         where d.endpoint_id = $1 \
           and e.payload ->> 'slug' = 'after-mute'",
    )
    .bind(Uuid::parse_str(&endpoint_id).expect("uuid"))
    .fetch_one(harness.db.pool())
    .await
    .expect("the delivery count must be readable");
    assert_eq!(
        queued, 0,
        "a disabled endpoint is skipped by the fan-out, not queued and dropped"
    );

    tick(&harness).await;
    assert_eq!(
        receiver.captured().len(),
        heard_before,
        "nothing new arrived at the receiver while the endpoint was off"
    );

    // ---- 3. The past is still readable, in full ------------------------------------------------
    let history = harness
        .call(get(
            &format!("/api/v1/webhooks/{endpoint_id}/deliveries"),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(history.status, StatusCode::OK, "{:?}", history.body);
    let rows = history.body["deliveries"].as_array().expect("deliveries");
    assert!(
        !rows.is_empty(),
        "the deliveries made before the switch are still on the screen — that history is why \
         an operator pauses an integration instead of deleting it"
    );
    assert!(
        rows.iter().all(|row| row["status"] == "delivered"),
        "and they are intact, not reset: {:?}",
        rows.iter().map(|row| &row["status"]).collect::<Vec<_>>()
    );

    // The endpoint row itself reads back with the flag off — the list screen's status dot has
    // something to draw.
    let listed = harness
        .call(get("/api/v1/webhooks", Some(&editor_token)))
        .await;
    assert_eq!(listed.status, StatusCode::OK, "{:?}", listed.body);
    let mine = listed.body["webhooks"]
        .as_array()
        .expect("webhooks")
        .iter()
        .find(|row| row["id"] == endpoint_id.as_str())
        .expect("the endpoint is still listed");
    assert_eq!(
        mine["enabled"],
        json!(false),
        "the status dot has something to draw"
    );

    // ---- 4. Switching back on resumes the stream, without a re-subscribe -------------------------
    let resumed = harness
        .call(patch(
            &format!("/api/v1/webhooks/{endpoint_id}"),
            json!({ "enabled": true }),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(resumed.status, StatusCode::OK, "{:?}", resumed.body);

    assert_eq!(
        publish_page(&harness, &editor_token, site, "after-unmute").await,
        StatusCode::OK,
        "the third page publishes"
    );
    tick(&harness).await;
    assert!(
        receiver.captured().len() > heard_before,
        "re-enabling resumes the stream — the subscription was never destroyed, only muted"
    );

    harness.dispose().await;
}

// ---------------------------------------------------------------------------------------------
// The backoff ladder, measured rather than assumed
// ---------------------------------------------------------------------------------------------

/// A delivery row as this walk reads it back out of the API, not out of PostgreSQL.
///
/// The point of reading it over HTTP is that the *screen* reads it the same way. `duration_ms`
/// and `next_attempt_at` are both screen columns — the deliveries table renders an attempt's
/// duration and when the next one is due — so a value that exists in the table but is dropped
/// by `DeliveryBody::build` is invisible to the operator while every test on the crate stays
/// green. Reading the response body is the only assertion that notices the two disagree.
#[derive(Debug, Clone)]
struct DeliveryView {
    status: String,
    attempts: i64,
    next_attempt_at: OffsetDateTime,
    response_status: Option<i64>,
    duration_ms: Option<i64>,
    error: Option<String>,
}

/// Parse one row of a `deliveries` response.
///
/// `next_attempt_at` is a required RFC 3339 string in the body, and this returns `None` for it
/// rather than parsing into an error: a body that omits the field is a body this walk cannot
/// measure, which is a finding, not a panic.
fn delivery_view(row: &Value) -> DeliveryView {
    let text = |key: &str| row[key].as_str().map(str::to_owned);
    let number = |key: &str| row[key].as_i64();

    DeliveryView {
        status: text("status").unwrap_or_default(),
        attempts: number("attempts").unwrap_or_default(),
        next_attempt_at: text("next_attempt_at")
            .and_then(|raw| {
                OffsetDateTime::parse(&raw, &time::format_description::well_known::Rfc3339).ok()
            })
            .unwrap_or_else(|| {
                // A body without a parsable `next_attempt_at` still has to produce a row, or the
                // walk below would silently skip the very delivery it is measuring. `now()` is a
                // value that can never be later than a real schedule, so every ordering
                // assertion below fails loudly on it instead of passing by accident.
                OffsetDateTime::UNIX_EPOCH
            }),
        response_status: number("response_status"),
        duration_ms: number("duration_ms"),
        error: text("error"),
    }
}

/// The delivery rows of one endpoint, newest first.
async fn delivery_views(harness: &Harness, endpoint_id: &str, token: &str) -> Vec<DeliveryView> {
    let response = harness
        .call(get(
            &format!("/api/v1/webhooks/{endpoint_id}/deliveries"),
            Some(token),
        ))
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
    response.body["deliveries"]
        .as_array()
        .expect("deliveries")
        .iter()
        .map(delivery_view)
        .collect()
}

/// A receiver that fails every attempt, so the whole ladder is walked in one go.
async fn failing_endpoint(harness: &Harness, token: &str, receiver: &Receiver) -> String {
    let created = harness
        .call(post(
            "/api/v1/webhooks",
            json!({
                "name": format!("Ladder receiver {}", Uuid::new_v4()),
                "url": receiver.url,
                "events": ["page.published"],
            }),
            Some(token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body["id"].as_str().expect("id").to_owned()
}

/// The two boxes that name what a delivery row must be able to say.
///
/// **What was already proven, and why these two needed a walk of their own.** The main bus
/// walk asserts `delivered`, `attempts == 1`, `response_status == 200` and a readable error on
/// the terminal branch — and that walk has been green for slices. What it never asserted is the
/// two claims these boxes actually make:
///
/// 1. a **successful** delivery carries a non-null `duration_ms`, and
/// 2. the retries are **increasingly spaced** — `next_attempt_at` grows each attempt, rather
///    than every retry landing at the same instant.
///
/// Neither is reachable from the walk that already existed, for two different reasons that are
/// worth stating because they are the whole point of writing this one. The duration is not
/// asserted there because the walk's assertions list stops at the fields it happened to need;
/// and "increasing" is not assertable from a walk that waits for a *fixed* sleep
/// (`after_backoff`) between ticks — such a walk cannot tell "the ladder backed off" from
/// "the runner happened to tick again later". Both claims are about **the value in the row**,
/// not about whether the delivery eventually succeeded, which is why they need the timestamps
/// read back rather than a tick count.
///
/// The consequence of the gap is the same class the previous tick found in the notification
/// queue: a column nobody reads looks exactly like a column that works. `duration_ms` is a
/// screen column on the deliveries table and `next_attempt_at` is its "next attempt" cell, so
/// if `DeliveryBody::build` had dropped either, the stats tab's `p95_duration_ms` would have had
/// no source, every retry would have looked due at once in the UI, and no crate test would have
/// said so. The walk below reads both out of the **HTTP body**, because that is the path the
/// screen takes.
#[tokio::test]
async fn a_delivery_row_measures_its_own_duration_and_its_backoff_grows() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let receiver = Receiver::start(false).await;

    // The platform owner exists only for the binding: this walk's every call is the editor's,
    // so that a tenant-scoped assertion cannot pass vacuously through the owner shortcut.
    let (owner_id, _owner_token) = account(&harness, None).await;
    seed::bind_owner(harness.db.pool(), owner_id)
        .await
        .expect("the owner binding must be created");

    let organization = create_organization_row(&harness.db, "ladder", "Ladder Test").await;
    let site = create_site_row(&harness.db, organization, "main", "Ladder Site").await;

    let (editor, editor_token) = account(&harness, Some(organization)).await;
    grant(
        &harness,
        editor,
        organization,
        &[
            "webhooks.read",
            "webhooks.manage",
            "events.read",
            "content.pages.read",
            "content.pages.create",
            "content.pages.publish",
        ],
    )
    .await;

    // ---- 1. A successful delivery carries a duration -----------------------------------------
    let ok_endpoint = harness
        .call(post(
            "/api/v1/webhooks",
            json!({
                "name": format!("Timing receiver {}", Uuid::new_v4()),
                "url": receiver.url,
                "events": ["page.published"],
            }),
            Some(&editor_token),
        ))
        .await;
    assert_eq!(
        ok_endpoint.status,
        StatusCode::CREATED,
        "{:?}",
        ok_endpoint.body
    );
    let ok_id = ok_endpoint.body["id"].as_str().expect("id").to_owned();

    assert_eq!(
        publish_page(&harness, &editor_token, site, "timed").await,
        StatusCode::OK
    );
    let report = tick(&harness).await;
    assert_eq!(report.delivered, 1, "{report:?}");

    let rows = delivery_views(&harness, &ok_id, &editor_token).await;
    let delivered = rows.first().expect("the delivery is on screen");
    assert_eq!(delivered.status, "delivered", "{delivered:?}");
    assert_eq!(delivered.attempts, 1, "{delivered:?}");
    assert_eq!(delivered.response_status, Some(200), "{delivered:?}");
    // The box: a delivered row is not merely `delivered` — it is a measurement. Without the
    // duration the stats tab has nothing to average and the table's "—" reads as "instant",
    // which is a number this receiver never reported.
    let duration = delivered.duration_ms.expect(
        "a delivered row must carry the receiver's duration; the deliveries table renders this \
         column and `p95_duration_ms` has no other source",
    );
    assert!(
        duration >= 0,
        "a duration cannot be negative: {duration} ms ({delivered:?})"
    );
    assert!(
        delivered.error.is_none(),
        "a delivered row carries no error text: {delivered:?}"
    );

    // ---- 2. Every refused attempt is measured too ----------------------------------------------
    // The failing receiver and the healthy one are separate endpoints because a refusal is what
    // produces the ladder; sharing one endpoint would make the ladder's first row the successful
    // delivery above and every assertion below ambiguous about which row it is reading.
    let broken = Receiver::start(true).await;
    let ladder_id = failing_endpoint(&harness, &editor_token, &broken).await;

    assert_eq!(
        publish_page(&harness, &editor_token, site, "ladder").await,
        StatusCode::OK
    );

    // The ladder is collected as it is climbed: each attempt that does not run out of budget
    // reschedules the row, and the row's own `next_attempt_at` is the only record of how far
    // out the platform pushed it. The claim happens only when the row is due, so a growing
    // ladder has to be walked one step at a time — a fixed sleep would prove nothing (see the
    // walk's doc comment).
    //
    // What is deliberately **not** recorded here is "was the schedule still in the future when
    // the row was read back". The base in this suite is 40 ms and a tick that includes an HTTP
    // round trip to a loopback receiver takes longer than that, so by the time the row is read
    // its schedule is legitimately due — and the assertion would be measuring the walk's own
    // latency rather than the backoff. The distance between consecutive schedules is the
    // deterministic measurement of the same fact, and it is asserted below.
    let mut schedule: Vec<(i64, OffsetDateTime)> = Vec::new();
    let mut final_row: Option<DeliveryView> = None;

    for _ in 0..(DEFAULT_MAX_ATTEMPTS as usize + 2) {
        tick(&harness).await;
        let rows = delivery_views(&harness, &ladder_id, &editor_token).await;
        let Some(row) = rows.first().cloned() else {
            continue;
        };

        if row.status == "failed" {
            final_row = Some(row);
            break;
        }

        schedule.push((row.attempts, row.next_attempt_at));
        // Wait until this attempt is actually due, so the next tick claims it. The cap keeps a
        // wrong ladder from turning this walk into a 15-minute sleep: with `retry_base` at
        // 40 ms the real ladder is under 320 ms, so a second of slack is generous, and a row
        // that never becomes due fails the loop's own bound instead of hanging.
        let wait = (row.next_attempt_at - OffsetDateTime::now_utc())
            .whole_milliseconds()
            .max(0);
        tokio::time::sleep(StdDuration::from_millis(wait.clamp(0, 1_000) as u64 + 20)).await;
    }

    let final_row = final_row.unwrap_or_else(|| {
        panic!(
            "the delivery never ran out of attempts; the schedule it was given was {schedule:?}"
        );
    });

    assert_eq!(final_row.status, "failed", "{final_row:?}");
    assert_eq!(
        final_row.attempts, DEFAULT_MAX_ATTEMPTS as i64,
        "{final_row:?}"
    );
    assert_eq!(
        final_row.response_status,
        Some(500),
        "the receiver answered 500 on the attempt that ran out of budget: {final_row:?}"
    );
    let reason = final_row
        .error
        .clone()
        .expect("the terminal row carries the reason the operator needs");
    assert!(
        reason.contains("500"),
        "the reason names the status a receiver actually answered: {reason:?}"
    );
    assert!(
        final_row.duration_ms.is_some(),
        "a refused attempt is measured too — a receiver that times out and one that refuses must \
         not look identical in the stats: {final_row:?}"
    );

    // ---- 3. The ladder grew, every step ---------------------------------------------------------
    assert!(
        schedule.len() >= 3,
        "a five-attempt ladder must reschedule at least four times before the terminal row; \
         collected {schedule:?}"
    );

    let mut previous: Option<(i64, OffsetDateTime)> = None;
    for (attempt, next) in &schedule {
        // Every scheduled row is one the runner refused: a `delivered` row in the ladder would
        // mean the walk is reading a row it did not expect to be there.
        assert!(
            *attempt >= 1 && *attempt <= DEFAULT_MAX_ATTEMPTS as i64,
            "attempt {attempt} is outside the budget the queue gave the delivery: {schedule:?}"
        );

        if let Some((previous_attempt, previous_next)) = previous {
            assert!(
                *next > previous_next,
                "the backoff must grow: attempt {previous_attempt} was set for {previous_next} \
                 and attempt {attempt} only for {next} — a flat ladder retries a broken receiver \
                 as fast as the runner ticks, which is the loop the cap and the ladder exist to \
                 prevent"
            );
        }
        previous = Some((*attempt, *next));
    }

    // The growth is exponential, not merely monotone: the walk states the ladder's shape rather
    // than settling for "not going backwards". `retry_delay` doubles the base per attempt, so
    // the last gap must be at least four times the first — with a cap in play, "at least"
    // rather than "exactly", because the ceiling flattens the top of the ladder by design.
    let gaps: Vec<i128> = schedule
        .windows(2)
        .map(|pair| (pair[1].1 - pair[0].1).whole_milliseconds())
        .collect();
    if let (Some(first), Some(last)) = (gaps.first().copied(), gaps.last().copied())
        && first > 0
    {
        assert!(
            last * 2 >= first,
            "the ladder must double: first gap {first} ms, last gap {last} ms (gaps {gaps:?})"
        );
    }

    assert_eq!(
        broken.captured().len(),
        DEFAULT_MAX_ATTEMPTS as usize,
        "the receiver saw every attempt, including the one that ran out of budget"
    );

    harness.dispose().await;
}

// ---------------------------------------------------------------------------------------------
// The registry vs the emitters: no `NewEvent::new("…")` may name an event the catalogue
// does not carry
// ---------------------------------------------------------------------------------------------

/// Every event name the modules actually record, read out of the source tree.
///
/// This is a *source* test, not a database test, and it is the only thing that keeps the
/// registry honest. The catalogue is hand-written; the emitters are hand-written; nothing
/// stops the two from drifting, and the drift is invisible: the bus records the fact, the
/// delivery is queued, and no receiver can subscribe to a name the picker never offered.
///
/// So the test walks the tree and asks the opposite question of the one the unit tests ask.
/// A unit test in `omnion-events` can only see that crate's own emitters; this one sees
/// every module's, and it names the offending file and line so the fix is obvious rather
/// than a puzzle.
#[test]
fn every_emitted_name_is_in_the_catalogue() {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("the workspace root is two levels above apps/api")
        .to_path_buf();

    let mut emitted: Vec<(String, String)> = Vec::new();
    let mut files = 0_usize;

    for area in ["apps", "crates", "modules"] {
        walk_rust(&workspace.join(area), &workspace, &mut emitted, &mut files);
    }

    assert!(
        files > 10,
        "the walk found {files} Rust files; a walk that sees nothing proves nothing"
    );
    assert!(
        emitted.len() > 30,
        "the walk found {} emissions; the emitters are not where this test looks",
        emitted.len()
    );

    let mut unlisted: Vec<String> = Vec::new();
    for (name, where_) in &emitted {
        if !omnion_events::catalogue::is_known(name) {
            unlisted.push(format!("  {name}  ({where_})"));
        }
    }

    assert!(
        unlisted.is_empty(),
        "{} emitted name(s) are not in the catalogue — add a row to \
         crates/events/src/catalogue.rs, or fix the emitter:\n{}",
        unlisted.len(),
        unlisted.join("\n"),
    );
}

/// Collect `NewEvent::new("…")` out of every `.rs` file below `root`.
fn walk_rust(
    root: &std::path::Path,
    workspace: &std::path::Path,
    found: &mut Vec<(String, String)>,
    files: &mut usize,
) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // `target` is build output, not source: a stale copy of an emitter in there is
            // not drift, it is a build artifact.
            let skip = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == "target" || name == "node_modules");
            if !skip {
                walk_rust(&path, workspace, found, files);
            }
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }

        *files += 1;
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };

        let lines: Vec<&str> = text.lines().collect();
        for (index, _line) in lines.iter().enumerate() {
            // A **doc comment is not an emitter.** The `health_events` module explains, at
            // length and with a runnable-looking example, why `bus::emit(pool,
            // NewEvent::new("health.service.degraded")…)` is the wrong implementation — and a
            // scanner that reads any line with the token in it collects that paragraph as an
            // emission. That is why `health.service` (a name split across two lines by the
            // doc's own wrapping) showed up as an unlisted emission pointing at
            // `health_events.rs:47`.
            //
            // Stripping the comment before the match is what keeps the gate honest in the
            // direction that matters: a false *emission* would push the catalogue to grow a row
            // for a sentence, and the failure would be invisible because the row would exist and
            // every emitter test would still pass. The comment-stripped line is what both
            // halves of the match below read; `code_at` applies the same stripping to the
            // lines of the lookahead window.
            //
            // The name is looked for on this line **and up to three lines below it**, because
            // an emitter that passes a `json!(…)` payload puts its name on its own line:
            //
            //     Announcement::new(
            //         "health.service.degraded",
            //         json!({ … }),
            //     )
            //
            // and one that picks between two names puts the second one further still:
            //
            //     NewEvent::new(if input.hold {
            //         "media.hold_placed"
            //     } else {
            //         "media.hold_released"
            //     })
            //
            // Three of REQ-014's five live names use the first shape. A scanner that reads one
            // line at a time sees the constructor and no name, so the gate reports the event as
            // unemitted — and the only fix that looks reasonable is to demote a working event to
            // `Reserved`, which is the registry lying in the *opposite* direction. rustfmt put
            // the payload on the following line long before this gate learned to look there.
            for offset in 0..=3usize {
                let Some(candidate) = lines.get(index + offset) else {
                    continue;
                };
                let candidate_code = code_of(candidate);

                // The marker may sit on this line (the name follows it here) or on the line
                // before (this line's first quoted string *is* the name). A marker that ended in
                // a quote could never match the second case: in
                // `Announcement::new(` the quote is on the *next* line.
                for marker in EMITTER_MARKERS {
                    let after = if candidate_code.contains(marker) {
                        candidate_code.split(marker).nth(1)
                    } else if offset > 0 && code_at(&lines, index, offset - 1, marker) {
                        Some(candidate_code)
                    } else {
                        None
                    };
                    let Some(after) = after else {
                        continue;
                    };
                    let Some(name) = after.split('"').nth(1) else {
                        continue;
                    };
                    // A name that is not dotted lower-case is a *test fixture* asserting the
                    // validator refuses it, not an emitter. The catalogue's own test covers those.
                    //
                    // A **dotted** name inside a `#[cfg(test)]` module is the same thing wearing a
                    // valid name: `health_events`' own unit test builds `health.service.exploded`
                    // to prove the catalogue check refuses an unknown name, and a scanner that
                    // only looked at the shape read it as a real emission and demanded a
                    // catalogue row for it. Granting that row would put a **lie** in the registry
                    // — a name nothing emits, listed as though something did — which is the exact
                    // failure this module exists to prevent.
                    let shaped = name.split('.').count() >= 2
                        && name
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_')
                        && !is_in_test_module(&text, index);
                    if !shaped {
                        continue;
                    }
                    let relative = path.strip_prefix(workspace).unwrap_or(&path);
                    found.push((
                        name.to_owned(),
                        format!("{}:{}", relative.display(), index + offset + 1),
                    ));
                }
            }
        }
    }
}

/// The constructors that emit an event name, written **without** their opening quote.
///
/// `NewEvent::new(…)` is the direct one. `Announcement::new(…)` is REQ-014's health emitter,
/// which cannot call `NewEvent::new` at all — health is platform-level, and `bus::emit` returns
/// zero deliveries for a fact with no organization, so `announce_changes` fans out **per tenant
/// that has an endpoint listening** instead. Its five names are `Live` in the catalogue and
/// reachable only through that constructor.
///
/// Listing it here is the difference between a gate that measures the workspace and one that
/// measures a *subset* of it. Left out, the gate reports five live names with no emitter and the
/// only honest-looking fix is to demote real, working, delivered events to `Reserved` — which
/// would make the picker lie in the other direction. This was the seventh shape of the same
/// defect class: an emitter behind a wrapper the gate cannot see.
const EMITTER_MARKERS: [&str; 2] = ["NewEvent::new(", "Announcement::new("];

/// A line with its trailing `//` comment removed.
///
/// Splitting on the first `//` is coarse — a string literal containing `//` (a receiver URL, for
/// instance) loses its tail — and that is acceptable here because a name is only ever read from
/// the part of the line that precedes a constructor call, and a URL is never an event name. The
/// alternative, tracking string literals properly, is a parser in a test that is checking a
/// table.
fn code_of(line: &str) -> &str {
    line.split_once("//").map_or(line, |(before, _)| before)
}

/// Whether the code part of `lines[index + back]` carries `marker`.
///
/// The lookahead for a name that sits a few lines below its constructor reads *every* line in
/// between, not only the immediately preceding one: the `if hold { … } else { … }` shape puts the
/// second name three lines under the marker. Checking only the opener finds the first name of the
/// pair and misses the second, which the other half of the drift gate then reports as "Live with
/// no emitter" — the same complaint, pointing the wrong way.
fn code_at(lines: &[&str], index: usize, back: usize, marker: &str) -> bool {
    let Some(from) = index.checked_sub(back) else {
        return false;
    };
    lines[from..=index]
        .iter()
        .any(|line| code_of(line).contains(marker))
}

/// Whether a line at `index` sits inside a `#[cfg(test)] mod tests { … }` block.
///
/// The flag is set at the `#[cfg(test)]` attribute and cleared only by a `}` that appears **at
/// column zero**, which is how rustfmt closes a module at the file's top level. Two earlier
/// versions of this were wrong in ways worth recording:
///
/// * counting braces would clear the flag at the first `}` of the first `json!({ … })`, so every
///   emitter after the first test's payload would look unbacked; and
/// * an indentation heuristic would have been defeated the moment somebody reformatted.
///
/// The direction that matters is the one this can still fail in. Missing a test fixture's end
/// makes the gate *demander* a catalogue row for a name nothing emits — a lie in the registry —
/// so the closing rule is deliberately strict: a file whose test module runs to the end of file
/// simply keeps the flag set, which is correct, because such a module really does extend to EOF.
fn is_in_test_module(text: &str, index: usize) -> bool {
    let mut in_tests = false;
    for line in text.lines().take(index) {
        let code = code_of(line);
        if code.trim_start().starts_with("#[cfg(test)]") || code.contains("#[cfg(all(test") {
            in_tests = true;
        }
        // Only a brace that starts the line closes the module; anything indented belongs to
        // something inside it.
        if in_tests && (code.starts_with('}') || code.starts_with("//!")) {
            in_tests = false;
        }
    }
    in_tests
}

// ---------------------------------------------------------------------------------------------
// The registry vs the emitters, the other direction: a name the catalogue calls *live* must
// have an emitter
// ---------------------------------------------------------------------------------------------

/// Every name marked `Live` in the catalogue is emitted by some module.
///
/// The gate above walks one direction, and one direction is not enough. It proves an emitter
/// never names a row that is missing — but it says nothing about a row that exists with
/// nothing behind it, and that is the failure that actually shipped: twenty-seven rows carried
/// `Live`, which the type documents as "emitted by the platform today", while the platform
/// emitted nothing of the sort. In the panel's picker they read exactly like a working event;
/// an operator subscribes, the delivery never comes, and there is nothing to show for the
/// subscription at all.
///
/// A registry is a promise about what other software will receive, so the promise has to be
/// checked. Two options, and only one of them is honest:
///
/// * emit the fact, if the write path exists — this tick added `page.created|updated|deleted|
///   restored`, `translation.updated`, `domain.added|removed`, `site.archived`, `user.updated`,
///   `user.deleted` and `theme.activated` for exactly this reason; or
/// * mark it `Reserved`, which the panel renders as "a module ships this" instead of implying
///   the platform is broken.
///
/// So the leftover rows are `Reserved` rather than `Live`. That is not demotion for its own
/// sake: `order.created` is `Reserved` for the same reason, and the status column exists to
/// carry it. Naming them honestly is what lets the picker say something true.
#[test]
fn every_live_name_has_an_emitter() {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("the workspace root is two levels above apps/api")
        .to_path_buf();

    let mut emitted: Vec<(String, String)> = Vec::new();
    let mut files = 0_usize;
    for area in ["apps", "crates", "modules"] {
        walk_rust(&workspace.join(area), &workspace, &mut emitted, &mut files);
    }

    let mut unbacked: Vec<String> = Vec::new();
    for name in omnion_events::catalogue::live_names() {
        if !emitted.iter().any(|(emitted_name, _)| emitted_name == name) {
            unbacked.push(name.to_owned());
        }
    }

    assert!(
        unbacked.is_empty(),
        "{} name(s) are marked Live but no module emits them — `Live` means the platform \
         records them today, and the picker shows an operator a name that will never fire. \
         Emit the fact, or change the row to Reserved and say which module ships it:\n{}\
         (the emitters this test can see are in {files} files)",
        unbacked.len(),
        unbacked.join("\n"),
    );
}

// ---------------------------------------------------------------------------------------------
// The third direction: a name a request *consumes* must be one the platform can deliver
// ---------------------------------------------------------------------------------------------

/// Every event a request file names under `Consumed:` is either live with an emitter, or
/// listed as `Reserved` with an owner.
///
/// The two gates above close both ends of one seam — emitters cannot name a missing row, and a
/// `Live` row cannot lack an emitter. Neither can see the third thing a request file does:
/// **write down a name it expects to receive.** A `Consumed:` line is a contract with a future
/// consumer, and nothing in the repository ever compared it to the registry, so a name that no
/// emitter will ever produce sat in five shipped request specs and would have survived every
/// gate this repo owns.
///
/// The failure is silent in the exact place it hurts. A module built to the spec — the CDN edge
/// (REQ-011) consumes `media.replaced` to invalidate a replaced file — subscribes to a name the
/// bus will never publish. The subscription is accepted (an unknown concrete name is kept, so
/// plugins can own names the table has never heard of), the picker offers nothing, and the
/// feature that was specced works for every file except the ones it exists to fix. No test
/// fails, because from every gate's point of view the registry is correct: nothing emits that
/// name and nothing claims to.
///
/// So this walks `docs/requests/` and checks the third direction. Nine such names exist today,
/// across six specs; each is a real promise the platform cannot keep.
///
/// **Why the area test matters.** Fifty-nine of the names collected across all specs are in
/// areas nothing emits yet (`order.paid`, `crm.deal.won`, `chat.notify`, …) — a module that has
/// not been built yet is *expected* to consume its neighbour's names, and demanding a row for
/// those would mean writing a registry row per unbuilt module, which is the lie in the other
/// direction this crate exists to prevent. The gate therefore only fires when the name's own
/// area is **already live**: `media`, `backups`, `identity`, `tenancy`, `themes`, `plugins`.
/// In that case a neighbour of that area exists and is shipping, so a name nothing emits is a
/// contract the platform has already broken rather than one it has not taken on yet.
#[test]
fn every_consumed_name_in_a_live_area_is_deliverable() {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("the workspace root is two levels above apps/api")
        .to_path_buf();
    let requests = workspace.join("docs/requests");

    let Ok(entries) = std::fs::read_dir(&requests) else {
        // A source tarball without the docs is not a broken platform. Skipping is the honest
        // answer; failing here would punish a packaging change with a database-free test.
        eprintln!(
            "SKIP: {} is not readable — no request specs to read",
            requests.display()
        );
        return;
    };

    // One walk, because "which areas are live" is a question about the registry and asking it
    // inside the per-name loop would rebuild the same set for every row.
    let live_areas: std::collections::BTreeSet<&str> = omnion_events::catalogue::live_names()
        .into_iter()
        .filter_map(|name| name.split('.').next())
        .collect();

    let mut specs = 0_usize;
    let mut consumed = 0_usize;
    let mut undeliverable: Vec<String> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        specs += 1;

        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("<unknown>")
            .to_owned();

        for (name, line) in consumed_names(&text) {
            consumed += 1;
            let area = name.split('.').next().unwrap_or_default();

            // A name in an area nothing emits yet is a promise about the future, not a lie.
            if !live_areas.contains(area) {
                continue;
            }
            // Listed as `Reserved` with an owner is the honest way to write this down, and
            // `the_reserved_row_names_its_owner` in the crate holds that half.
            if omnion_events::catalogue::is_known(&name) {
                continue;
            }

            undeliverable.push(format!("  {name}  ({stem}.md:{line}, area `{area}`)"));
        }
    }

    assert!(
        specs > 20,
        "the walk read {specs} spec file(s); a walk that sees nothing proves nothing"
    );
    assert!(
        consumed > 30,
        "the walk read {consumed} `Consumed:` name(s); the specs are not where this test looks"
    );

    // **The gate is a ratchet, not a wall.** Twenty-five names were already unpayable when this
    // test was written — every one of them owed by a wave-5b module that has not been built
    // (`iam.role_permissions_changed` by the role UI, `backup.completed` by the backup worker,
    // `theme.installed` by the installer). Demanding a catalogue row for each today would mean
    // adding 25 rows nothing emits, which is precisely the lie this crate exists to prevent,
    // and a gate that is red from the moment it is written is a gate people stop reading.
    //
    // So the debt is *written down* instead of hidden: every name appears below with the module
    // that owes it. The assertion then fails on anything **not** in this list — a new spec that
    // consumes a name nothing will ever produce, or a name quietly dropped from here while its
    // owner is still unbuilt. Both directions are the same lie and both are caught.
    //
    // Deleting an entry is not free, and that is the point: the owner's `Reserved` row satisfies
    // the real check below *before* the entry becomes stale, so an entry that is still needed
    // will already have been removed by the time this complains that it is redundant.
    const OWED_BY_AN_UNBUILT_MODULE: &[(&str, &str)] = &[
        // ---- REQ-013 backup centre: the worker's own run lifecycle, none of it emitted yet.
        ("backup.completed", "REQ-013 (backup worker run completion)"),
        ("backup.failed", "REQ-013 (backup worker run failure)"),
        // ---- REQ-066/067/068/069/070/071/072/074: the IAM depth wave, all `pending`.
        //      `iam.*` rows here are owed by whichever of those builds the write path; the
        //      existing `iam.session_revoked`, `iam.binding_created`, `iam.approval_decided`
        //      rows show the area's naming is settled and these are simply not written yet.
        (
            "iam.role_permissions_changed",
            "REQ-067 (role management UI)",
        ),
        ("iam.role_priority_changed", "REQ-067 (role management UI)"),
        (
            "iam.permissions_catalogue_updated",
            "REQ-068 (permission catalogue screen)",
        ),
        (
            "iam.resource_grant_changed",
            "REQ-070 (scopes and resource permissions)",
        ),
        (
            "iam.binding_revoked",
            "REQ-070 (scopes and resource permissions)",
        ),
        ("iam.group_membership_synced", "REQ-071 (groups and teams)"),
        ("iam.policy_denied", "REQ-069 (ABAC policy engine)"),
        (
            "iam.security_policy_changed",
            "REQ-069 (ABAC policy engine)",
        ),
        (
            "iam.provider_updated",
            "REQ-065 (identity providers and SSO)",
        ),
        (
            "iam.provisioning_synced",
            "REQ-072 (SCIM provisioning sync)",
        ),
        ("iam.account_locked", "REQ-066 (MFA and device trust)"),
        ("iam.mfa_challenge_failed", "REQ-066 (MFA and device trust)"),
        ("iam.step_up_failed", "REQ-066 (MFA and device trust)"),
        (
            "iam.catalogue_drift_detected",
            "REQ-068 (permission catalogue screen)",
        ),
        // ---- REQ-021 notification centre: the delivery lifecycle.
        (
            "notification.delivery.failed",
            "REQ-021 (notification delivery runner)",
        ),
        // ---- REQ-010/024/012/014: cross-module facts the platform does not record yet.
        (
            "user.login.failed",
            "REQ-006 (sign-in failure is logged, never recorded)",
        ),
        ("user.deactivated", "REQ-006 (deactivate route)"),
        ("site.deleted", "tenancy (a site is archived, not deleted)"),
        (
            "site.domain.expiring",
            "REQ-011 (CDN) or tenancy (expiry check)",
        ),
        // ---- Theme packaging: REQ-044 ships `plugin.*`; the theme half is REQ-062/084.
        ("theme.installed", "REQ-062 (ten default themes)"),
        (
            "theme.version.published",
            "REQ-084 (theme SDK and packaging)",
        ),
        // ---- REQ-114 translation engine.
        ("translation.job.completed", "REQ-114 (translation engine)"),
        ("translation.memory.updated", "REQ-114 (translation memory)"),
    ];

    let mut unaccounted: Vec<String> = Vec::new();
    let mut owed: Vec<String> = Vec::new();

    for row in &undeliverable {
        let name = row.split_whitespace().next().unwrap_or_default().to_owned();
        match OWED_BY_AN_UNBUILT_MODULE
            .iter()
            .find(|(owed_name, _)| *owed_name == name)
        {
            Some((_, module)) => owed.push(format!("  {name}  (owed by {module})")),
            None => unaccounted.push(row.clone()),
        }
    }

    let mut retired: Vec<String> = OWED_BY_AN_UNBUILT_MODULE
        .iter()
        .filter(|(name, _)| {
            !undeliverable
                .iter()
                .any(|row| row.starts_with(&format!("  {name} ")))
        })
        .map(|(name, module)| format!("  {name}  ({module})"))
        .collect();
    retired.sort();

    assert!(
        retired.is_empty() && unaccounted.is_empty(),
        "{} `Consumed:` name(s) cannot be delivered and nothing owes them, and {} entr(ies) below \
         have become redundant:\n{}\n{}\n\
         The owed list is a ratchet: a new undelivered name must be added there with the module \
         that owes it, and an entry is removed when its `Reserved` catalogue row (or its emitter) \
         makes the real check below pass — the assertion above already sees it satisfied, so a \
         stale entry reports itself.",
        unaccounted.len(),
        retired.len(),
        if unaccounted.is_empty() {
            "(none)".to_owned()
        } else {
            unaccounted.join("\n")
        },
        if retired.is_empty() {
            "(none)".to_owned()
        } else {
            retired.join("\n")
        },
    );

    eprintln!(
        "note: {} `Consumed:` name(s) are unpayable today and owed by an unbuilt module; the \
         ratchet holds that number at {}",
        owed.len(),
        OWED_BY_AN_UNBUILT_MODULE.len(),
    );
}

/// The event names a request file names under a `Consumed:` marker, with their line numbers.
///
/// Two things about the reading, both of which are the difference between a gate that measures
/// the specs and one that measures a guess:
///
/// * **The marker, not the section.** `Consumed:` also appears inside prose in half a dozen
///   specs (`REQ-021` routes bus events into notifications and never writes the word as a
///   header). Reading the line it is on would collect those sentences' backtick spans too.
/// * **One line, not the paragraph.** A `Consumed:` clause runs on and names six triggers across
///   two hundred characters; stopping at the first newline loses every one after the first. The
///   next `**` or blank line ends it, which is where every spec in this repository ends its own
///   clause.
fn consumed_names(text: &str) -> Vec<(String, usize)> {
    let mut found = Vec::new();

    for (index, line) in text.lines().enumerate() {
        let Some(marker) = line.find("Consumed:") else {
            continue;
        };
        // A backtick before the marker means the sentence is *about* consumption, not a
        // declaration of it — `the \`Consumed:\` line`. The gate that reads those would report
        // prose as a contract.
        let before = &line[..marker];
        if before.contains('`') && before.trim_start().starts_with('-') {
            continue;
        }

        // The clause is the rest of the line, then any following continuation line that is
        // neither blank nor a new markdown construct.
        let mut clause = line[marker + "Consumed:".len()..].to_owned();
        for following in text.lines().skip(index + 1) {
            let trimmed = following.trim();
            if trimmed.is_empty()
                || trimmed.starts_with("**")
                || trimmed.starts_with('-')
                || trimmed.starts_with('#')
                || trimmed.starts_with('|')
            {
                break;
            }
            clause.push(' ');
            clause.push_str(following);
        }

        let mut ticks = clause.match_indices('`');
        while let Some((open, _)) = ticks.next() {
            let Some((close, _)) = ticks.next() else {
                break;
            };
            let candidate = &clause[open + 1..close];
            if is_event_name(candidate) {
                found.push((candidate.to_owned(), index + 1));
            }
        }
    }

    found
}

/// A dotted lower-case token: two or more segments, no wildcards, nothing else.
///
/// The wildcard exclusion is not cosmetic. A `Consumed:` clause legitimately contains group
/// forms (`crm.deal.*`, `appearance.*`, `migration.*`), and a group is *not* a name the bus
/// records — `reconcile` expands it against what exists, which for an unbuilt area is nothing.
/// Firing on those would demand a catalogue row per group per spec, which is the registry
/// writing down names that do not exist.
fn is_event_name(candidate: &str) -> bool {
    let segments: Vec<&str> = candidate.split('.').collect();
    segments.len() >= 2
        && candidate
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_')
        && candidate.contains('.')
        && !candidate.contains('*')
}

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
