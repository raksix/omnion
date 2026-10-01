//! The outbound walk for the data guard (REQ-105, slice 1's second half).
//!
//! `ai_guard.rs` proves the guard against **rows**: the rules load, a tenant rule joins the
//! platform set, an invalid regex stores nothing, the event row cannot hold the payload, an
//! exemption narrows one label on one feature. Every one of those walks calls the crate or the
//! store directly, and its file header says so: "a counter that stays at zero would be
//! measuring this suite, not the product."
//!
//! That leaves the half of the request's first criterion that is actually about the guard —
//! **"masked to `[EMAIL_1]` before the provider call (proven with a stub provider that records
//! the exact body it received)", and the block half of the same sentence**. A store walk cannot
//! earn it. The checkpoint unit tests build a `Detector` from rules the *test* wrote and hand
//! them to `checkpoint`, so "a blocked payload never reaches the network" there is a claim
//! about `checkpoint`, not about the route — a checkpoint wired into nothing would pass every
//! one of them.
//!
//! So this suite puts a provider on the other end.
//!
//! The stub is an axum server the router dials for real. It records every request body it
//! receives into a shared `Arc<Mutex<Vec<String>>>`, which the walks read afterwards. That
//! single mechanism earns three claims that no other walk in this REQ can make:
//!
//! - **A masked turn leaves as `[EMAIL_1]`** — the body the provider saw contains the
//!   placeholder and does *not* contain the original address. Asserting both halves matters: a
//!   body check that only looked for the placeholder would also pass if the guard had appended
//!   it next to the real value.
//! - **The same value keeps the same placeholder across two calls in one request** — the
//!   request names the address twice, in two different messages, and the recorded body carries
//!   exactly one `[EMAIL_1]` and no `[EMAIL_2]` for the second one.
//! - **A blocked turn is never dialled at all** — `calls.len()` is *unchanged* across the
//!   refused request. This is the assertion that fails if the checkpoint is ever moved below
//!   the dial, or removed from the route while the unit tests keep passing.
//!
//! The harness is `ai_live_path.rs`'s, reused rather than forked: throwaway database, the
//! bootstrap owner, a tenant member (the log and the guard are both organization-filtered, so
//! a platform-level account would see an empty screen and the walk would blame the writer) and
//! a registered mock provider. It **skips** with a printed reason when PostgreSQL is absent,
//! matching the suite it borrows from — see the "skips and passes are different" note in
//! `ai_guard.rs`'s header for why the *other* file panics instead.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use axum::routing::{get as route_get, post as route_post};
use axum::{Json, Router};
use http_body_util::BodyExt;
use omnion_api::rate_limit_middleware::RateLimiter;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, CsrfSecret, DatabaseConfig};
// `RatePolicy` lives in the security crate, not in `omnion_api` — importing it from the
// middleware module is a private-import error, which cost one build round.
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_security::RatePolicy;
use serde_json::{Value, json};
use sqlx::Row;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

/// A CSRF secret, so the suite's writes are real writes.
///
/// The guard's routes are cookie-authenticated **writes** (`POST /ai/guard/test`,
/// `PUT /ai/guard/policy`) and `POST /ai/chat` is one too. Without a secret every one of them
/// answers `403 csrf_unavailable`, which is a correct answer for an installation that has not
/// set one — and which would have made every walk in this file fail for a reason that has
/// nothing to do with the guard. The asymmetry is the trap: the failure looks like a refusal
/// of the *product's* policy when it is the harness's missing configuration.
///
/// Borrowed from `ai_tenant_404.rs`, whose header records the same lesson.
const CSRF_SECRET: &str = "guard-outbound-integration-suite-key-material";

/// The email the walks use, and the string no provider may ever see.
const ADDRESS: &str = "ada.lovelace@omnion.test";
/// A card-shaped value: `luhn`-valid, and `card.builtin` is seeded at `block`, so it is the
/// refusal case that needs no policy edit to exercise.
const CARD: &str = "4111 1111 1111 1111";

// -------------------------------------------------------------------------------------------
// The harness
// -------------------------------------------------------------------------------------------

struct TestResponse {
    status: StatusCode,
    /// **Every** `Set-Cookie`, in order. A single-cookie accessor is the defect
    /// `tests/csrf.rs` was written about: a login sets two, and a harness that reads the first
    /// reports green while every write it makes is refused.
    set_cookie: Vec<String>,
    body: String,
}

impl TestResponse {
    /// One cookie's value by name, empty when the response did not set it.
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

struct Harness {
    state: AppState,
    db: Db,
    maintenance: Db,
    database: String,
}

impl Harness {
    async fn fresh() -> Option<Self> {
        let mut config = Config::from_env().expect("environment must be valid");
        config.csrf = CsrfSecret::new(Some(CSRF_SECRET.to_owned()));
        if live_db(&config).await.is_none() {
            return None;
        }

        let database = format!("omnion_guardout_{}", Uuid::new_v4().simple());
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

        Some(Self {
            state,
            db,
            maintenance,
            database,
        })
    }

    /// Make the request and read the whole body.
    ///
    /// Collected as `String` rather than `Value` because the chat route answers **SSE**, not
    /// JSON: a JSON parse that fails is not an error here, it is the normal answer for a
    /// streaming endpoint, and a `Value` field would collapse "the stream opened" and "the
    /// stream did not open" into the same `Null`. The status and the raw text are both
    /// available to every walk.
    async fn call(&self, request: Request<Body>) -> TestResponse {
        let response = routes::router(self.state.clone())
            .oneshot(request)
            .await
            .expect("router must answer");
        let status = response.status();
        let set_cookie: Vec<String> = response
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
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    /// A chat request whose response body is JSON, for the routes that are not streams.
    async fn call_json(&self, request: Request<Body>) -> (StatusCode, Value) {
        let response = self.call(request).await;
        let body = serde_json::from_str(&response.body).unwrap_or(Value::Null);
        (response.status, body)
    }

    /// Drop the throwaway database.
    ///
    /// Every walk ends with this. It is not hygiene: the box runs a `max_connections = 100`
    /// PostgreSQL shared with seven other writer loops, so five walks that leaked one database
    /// and four connections each would starve the other loops — and starve *themselves*, which
    /// is the worse failure. It showed up here as a false green: with the checkpoint bypassed,
    /// four of the five walks reported `ok` not because the guard worked but because
    /// `Harness::fresh` could no longer reach the server and returned `None`, which this file
    /// reads as "skip, carry on". A skipped walk is not a passing walk.
    /// Give this suite a rate-limit budget of its own.
    ///
    /// The limiter is a process-wide cell the router fills from the **stored** document, and
    /// the stored `sign_in` scope is ten per five minutes per IP. This file has five walks and
    /// each one signs in — and the suites sharing this box are signing in too, all counted
    /// against the same key, so a walk gets refused with a `429` naming a limit it was never
    /// testing. That is not hypothetical: it is what the fifth walk did on the first full run,
    /// and the failure looked like a guard defect.
    ///
    /// Only the sign-in ceiling moves. The others are what a deployment ships, and raising
    /// those would let a suite become the reason a genuinely over-budget request stops being
    /// refused. Borrowed from `ai_tenant_404.rs`, which records the same reasoning.
    fn raise_the_sign_in_ceiling(&self) {
        let policies: Vec<RatePolicy> = RatePolicy::defaults()
            .into_iter()
            .map(|mut policy| {
                if policy.scope == "sign_in" {
                    policy.limit = 10_000;
                }
                policy
            })
            .collect();
        let _ = omnion_api::rate_limit_middleware::install(RateLimiter::new(&self.state, policies));
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

/// A signed-in session and the CSRF token that goes with it.
///
/// The two travel together on purpose. Passing only the session is the shape that produces five
/// `403 csrf_unavailable`s and a walk that blames the guard; passing the CSRF token in the
/// *cookie* jar instead of the header is the shape that produces a suite which cannot write at
/// all. `auth()` is the only place either is attached, so a new call site cannot forget half of
/// the pair — the failure this harness's first run actually produced.
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

    /// The session cookie and the CSRF header, in one place.
    fn auth(self: &Self, builder: axum::http::request::Builder) -> axum::http::request::Builder {
        builder
            .header(header::COOKIE, format!("omnion_session={}", self.token))
            .header("x-omnion-csrf", self.csrf.clone())
    }
}

fn get(uri: &str, session: Option<&Session>) -> Request<Body> {
    let builder = Request::builder().method(Method::GET).uri(uri);
    let builder = match session {
        Some(session) => session.auth(builder),
        None => builder,
    };
    builder.body(Body::empty()).expect("request must build")
}

fn put(uri: &str, body: Value, session: &Session) -> Request<Body> {
    session
        .auth(
            Request::builder()
                .method(Method::PUT)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json"),
        )
        .body(Body::from(body.to_string()))
        .expect("request must build")
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

// -------------------------------------------------------------------------------------------
// The recording stub provider
// -------------------------------------------------------------------------------------------

/// Every body the provider received, in order, shared with the walks.
///
/// The point of the whole file. A `Mutex<Vec<String>>` rather than an `mpsc` channel because
/// the assertion is "how many calls arrived and what did each one say", and a channel would
/// make the count a property of the *receiver's* timing — a walk that read before the spawn
/// task had been scheduled would see zero calls and call it a pass.
type Received = Arc<Mutex<Vec<String>>>;

/// A provider that records the exact body it was sent, and answers one stream chunk.
///
/// The recorded body is the *raw request text*, not the parsed message: the claim is about what
/// left the process, and a walk that re-serialized a parsed struct would prove the struct was
/// serialized rather than that the wire body carried the placeholder.
async fn recording_provider() -> (String, Received, tokio::task::JoinHandle<()>) {
    let received: Received = Arc::new(Mutex::new(Vec::new()));

    let recorder = Arc::clone(&received);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the stub must bind a port");
    let address = listener.local_addr().expect("the stub has an address");

    let app = Router::new()
        .route(
            "/v1/models",
            route_get(|| async { Json(json!({ "data": [] })) }),
        )
        .route(
            "/v1/chat/completions",
            route_post(move |body: String| {
                let recorder = Arc::clone(&recorder);
                async move {
                    recorder.lock().expect("recorder").push(body);
                    Json(json!({
                        "choices": [{ "delta": { "content": "ok" }, "finish_reason": "stop" }],
                        "usage": { "prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7 }
                    }))
                }
            }),
        );
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}/v1"), received, task)
}

/// A snapshot of what the stub received.
///
/// Plain, not `async`: the recorder is a `std::sync::Mutex`, not a channel, so there is
/// nothing to await. Marking it `async` would make every call site a future that has to be
/// `.await`ed before it can be `len()`ed — which is exactly the shape that produced a
/// wall of `no method named len found for opaque type` errors on the first build.
fn calls(received: &Received) -> Vec<String> {
    received.lock().expect("recorder").clone()
}

/// The harness, or a panic.
///
/// Deliberately **not** `let Some(h) = … else { return }`. This file was written with that
/// skip, borrowed from `ai_live_path.rs`, and the cost of the borrow was measured during the
/// proven-to-fail run: with the checkpoint bypassed, four of five walks printed `ok` while the
/// real reason was that `Harness::fresh` could not reach PostgreSQL any more and handed back
/// `None`. A skip that reads as a pass is worse than a red gate, because it survives it — and
/// the same trap is already documented in `ai_guard.rs`, whose harness panics for this reason.
///
/// `ai_guard.rs` sets `OMNION_DATABASE_URL` to the w7 database on port 5433.
macro_rules! guard {
    () => {
        match Harness::fresh().await {
            Some(harness) => harness,
            None => panic!(
                "PostgreSQL is not reachable, so every walk in this file would have SKIPPED — \
                 and a skip must not read as a pass. Set OMNION_DATABASE_URL to an existing \
                 database; on this box the QA stack's is the w7 database on port 5433."
            ),
        }
    };
}

// -------------------------------------------------------------------------------------------
// Fixtures
// -------------------------------------------------------------------------------------------

struct Fixture {
    /// A tenant member's session — the one every guarded call is made with. The guard is
    /// organization-filtered, so a platform-level account would resolve the *platform* rows
    /// only and this suite would measure the wrong installation.
    ///
    /// The policy is saved with **this** session too, and that is load-bearing rather than
    /// incidental: the owner is platform-level (`organization_id` null), so an owner-scoped
    /// policy would land on the nil organization and every call made as the member would
    /// resolve `allow` — which looks exactly like a guard that does nothing.
    member: Session,
    /// The installer's session. Used to register the provider, and kept in the fixture because
    /// the two sessions are the whole point of the split: an installation action made by the
    /// installer, an organization-scoped one made by the member.
    owner: Session,
    organization: Uuid,
}

async fn connected(harness: &Harness, base_url: &str) -> Fixture {
    harness.raise_the_sign_in_ceiling();

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
    assert_eq!(owner.status, StatusCode::CREATED, "{}", owner.body);
    let owner_session = Session::of(&owner);
    // The first owner is **platform-level**: its `organization_id` is null, because nobody has
    // created a tenant yet. Asserted because it is the reason the member below exists — a
    // platform account resolves the platform guard rows only, so every masked/blocked claim in
    // this file would silently be measuring a different installation.
    let owner_body: Value = serde_json::from_str(&owner.body).unwrap_or(Value::Null);
    assert!(
        owner_body["user"]["organization_id"].is_null(),
        "the first owner is platform-level: {}",
        owner.body
    );

    let organization_created = harness
        .call(post(
            "/api/v1/onboarding/organization",
            json!({ "name": "Acme" }),
            Some(&owner_session),
        ))
        .await;
    assert_eq!(
        organization_created.status,
        StatusCode::OK,
        "{}",
        organization_created.body
    );

    let organization = sqlx::query("select id from organizations order by created_at limit 1")
        .fetch_one(harness.db.pool())
        .await
        .expect("the organization the bootstrap created must be readable")
        .get::<Uuid, _>("id");

    // The role is looked up rather than named: a fixture carrying a role id literal breaks the
    // day the seeded ids move, and the whole suite would fail on a permission it never meant to
    // be testing.
    let roles = harness
        .call(get("/api/v1/iam/roles", Some(&owner_session)))
        .await;
    assert_eq!(roles.status, StatusCode::OK, "{}", roles.body);
    let parsed: Value = serde_json::from_str(&roles.body).unwrap_or(Value::Null);
    let admin_role = parsed["roles"]
        .as_array()
        .expect("roles array")
        .iter()
        .find(|role| role["key"] == "administrator" || role["key"] == "admin")
        .or_else(|| {
            parsed["roles"]
                .as_array()
                .expect("roles array")
                .iter()
                .find(|role| role["key"] == "editor")
        })
        .and_then(|role| role["id"].as_str())
        .expect("an installation ships at least one role that may use the AI")
        .to_owned();

    let member_email = format!("member-{}@omnion.test", Uuid::new_v4().simple());
    let member = harness
        .call(post(
            "/api/v1/iam/users",
            json!({
                "email": member_email,
                "display_name": "Grace Hopper",
                "organization_id": organization,
                "password": PASSWORD,
                "role_id": admin_role,
            }),
            Some(&owner_session),
        ))
        .await;
    assert_eq!(member.status, StatusCode::CREATED, "{}", member.body);

    let signed_in = harness
        .call(post(
            "/api/v1/auth/login",
            json!({ "email": member_email, "password": PASSWORD }),
            None,
        ))
        .await;
    assert_eq!(
        signed_in.status,
        StatusCode::OK,
        "the member must be able to sign in: {}",
        signed_in.body
    );
    let member_session = Session::of(&signed_in);

    // The provider registration is `ai.providers.manage`, an installation action, so it uses
    // the owner's session. A member may use the model but not register one.
    let created = harness
        .call(post(
            "/api/v1/ai/providers",
            json!({
                "name": "Recording Stub",
                "base_url": base_url,
                "models": [{ "key": "small", "context_window": 8192 }]
            }),
            Some(&owner_session),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);

    Fixture {
        member: member_session,
        owner: owner_session,
        organization,
    }
}

/// Raise one label to `mask` and one to `block` through the guard's own policy route.
///
/// Through the API rather than an `update ai_guard_policy` because the panel's control *is*
/// this map: a walk that edited the row behind the endpoint would prove the row, not that
/// saving the panel produces the state the walk depends on.
///
/// The **owner's** session, because the policy is `ai.guard.manage` — an installation
/// action. The member session is deliberately kept out of it: a walk that saved the policy as
/// the member would be asserting nothing about who may hold that power, and the route layer
/// is where that is decided.
///
/// `email` to `mask` is what the masking criterion needs. `card` is seeded at `block` already,
/// but it is set here too so the refusal does not depend on the migration's exact wording —
/// a fixture whose block case breaks when somebody renames a default fails for the wrong
/// reason.
async fn mask_email_and_block_card(harness: &Harness, session: &Session, owner: &Session) {
    let (status, body) = harness
        .call_json(put(
            "/api/v1/ai/guard/policy",
            json!({
                "label_defaults": { "email": "mask", "card": "block" },
                "mask_style": "numbered"
            }),
            session,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "the policy must save: {body}");

    // Read it back through the route the panel reads, so a save that silently stored nothing
    // fails here rather than three walks later with an unexplained allow.
    let (status, body) = harness
        .call_json(get("/api/v1/ai/guard/policy", Some(session)))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["label_defaults"]["email"], "mask",
        "the policy must read back as saved: {body}"
    );

    // The installer is **inside** the tenant by the time this runs, so its view is the tenant's own.
    //
    // `create_organization` deliberately calls `users::set_user_organization(actor, org)`
    // (`crates/onboarding/src/steps.rs`), because `scope::resolve_organization` answers every
    // org-scoped route from `users.organization_id` and the wizard's actor would otherwise hold a
    // NULL and be refused with `organization_required` on every screen.
    //
    // An earlier version of this line asserted the installer still read a *permissive platform*
    // policy (`all_permissive == true`), which stopped being true the moment that call landed —
    // it fails every walk in this file, not just one. The fact worth asserting now is the one the
    // fixture exists for: **one tenant, one policy**, seen identically by both sessions. A store
    // that scoped the policy by something other than the organization would show them
    // disagreeing here.
    let (status, owner_view) = harness
        .call_json(get("/api/v1/ai/guard/policy", Some(owner)))
        .await;
    assert_eq!(status, StatusCode::OK, "{owner_view}");
    assert_eq!(
        owner_view["label_defaults"], body["label_defaults"],
        "the installer and the member are in the same tenant, so they must read the same policy: \
         owner={owner_view} member={body}"
    );
    assert_eq!(
        owner_view["has_policy_row"], true,
        "the tenant save must be readable by the installer's session too: {owner_view}"
    );
}

fn chat_with(messages: Vec<Value>) -> Value {
    json!({ "messages": messages })
}

fn user(text: &str) -> Value {
    json!({ "role": "user", "content": text })
}

fn assistant(text: &str) -> Value {
    json!({ "role": "assistant", "content": text })
}

// -------------------------------------------------------------------------------------------
// The walks
// -------------------------------------------------------------------------------------------

/// The stub records the **masked** body: `[EMAIL_1]` leaves, the address does not.
///
/// Both halves are asserted. A body check that only looked for the placeholder would pass on a
/// guard that appended it while leaving the original in place, which is the one failure mode a
/// substring test cannot see.
#[tokio::test]
async fn a_masked_turn_leaves_as_a_placeholder_and_the_address_never_leaves() {
    let harness = guard!();
    let (base_url, received, _stub) = recording_provider().await;
    let fixture = connected(&harness, &base_url).await;
    mask_email_and_block_card(&harness, &fixture.member, &fixture.owner).await;

    let chat = harness
        .call(post(
            "/api/v1/ai/chat",
            chat_with(vec![user(&format!("Write to {ADDRESS} about the invoice"))]),
            Some(&fixture.member),
        ))
        .await;
    assert_eq!(
        chat.status,
        StatusCode::OK,
        "the masked chat must still be answered: {}",
        chat.body
    );

    let seen = calls(&received);
    assert_eq!(
        seen.len(),
        1,
        "exactly one provider call must have been dialled, got {seen:?}"
    );
    assert!(
        seen[0].contains("[EMAIL_1]"),
        "the placeholder must reach the provider; body was: {}",
        seen[0]
    );
    assert!(
        !seen[0].contains(ADDRESS),
        "THE ADDRESS MUST NOT REACH THE PROVIDER; body was: {}",
        seen[0]
    );

    harness.dispose().await;
}

/// The same value keeps the same placeholder across two messages in one request.
///
/// `mask_text` keys the map by `value_hash`, so a second occurrence is the same key and gets the
/// same token. That is the property a multi-turn conversation depends on: if the second mention
/// became `[EMAIL_2]` the model would see two different addresses for one person, and the
/// re-mapping half of the criterion would have no single original to substitute back.
#[tokio::test]
async fn the_same_value_keeps_one_placeholder_across_two_turns() {
    let harness = guard!();
    let (base_url, received, _stub) = recording_provider().await;
    let fixture = connected(&harness, &base_url).await;
    mask_email_and_block_card(&harness, &fixture.member, &fixture.owner).await;

    let chat = harness
        .call(post(
            "/api/v1/ai/chat",
            chat_with(vec![
                user(&format!("Mail {ADDRESS} first")),
                assistant("Sent."),
                user(&format!("Then mail {ADDRESS} again")),
            ]),
            Some(&fixture.member),
        ))
        .await;
    assert_eq!(chat.status, StatusCode::OK, "{}", chat.body);

    let seen = calls(&received);
    assert_eq!(seen.len(), 1, "one call must have been dialled: {seen:?}");
    assert_eq!(
        seen[0].matches("[EMAIL_1]").count(),
        2,
        "both mentions must carry the SAME placeholder: {}",
        seen[0]
    );
    assert!(
        !seen[0].contains("[EMAIL_2]"),
        "one value must not produce a second placeholder: {}",
        seen[0]
    );
    assert!(
        !seen[0].contains(ADDRESS),
        "THE ADDRESS MUST NOT REACH THE PROVIDER; body was: {}",
        seen[0]
    );

    harness.dispose().await;
}

/// A blocked turn is **never dialled**. The counter is the assertion.
///
/// This is the walk that fails if the checkpoint is ever moved below the dial, or dropped from
/// the route while `guard_checkpoint.rs`'s unit tests keep passing green. Nothing else in the
/// REQ can see it: a store walk has no provider to not-call.
///
/// The baseline is read *before* the blocked request and compared after, rather than asserting
/// an absolute zero. Registration itself does not dial the provider — `/v1/models` is the only
/// other route the stub serves and the catalog does not call it here — but a baseline makes the
/// claim "this request added no call" instead of "this suite happened to make none", which
/// survives the registration path growing a health check later.
#[tokio::test]
async fn a_blocked_turn_is_refused_and_the_provider_is_never_dialled() {
    let harness = guard!();
    let (base_url, received, _stub) = recording_provider().await;
    let fixture = connected(&harness, &base_url).await;
    mask_email_and_block_card(&harness, &fixture.member, &fixture.owner).await;

    // A clean request first, so the suite proves the stub is reachable *and* that a permitted
    // call does arrive. Without it, "zero calls after the blocked request" would also be
    // satisfied by a stub nobody could reach — a green walk that measured a dead socket.
    let allowed = harness
        .call(post(
            "/api/v1/ai/chat",
            chat_with(vec![user("hello")]),
            Some(&fixture.member),
        ))
        .await;
    assert_eq!(allowed.status, StatusCode::OK, "{}", allowed.body);
    let baseline = calls(&received).len();
    assert_eq!(
        baseline, 1,
        "the permitted call must have reached the stub, or the refusal proves nothing"
    );

    let blocked = harness
        .call(post(
            "/api/v1/ai/chat",
            chat_with(vec![user(&format!("My card is {CARD}, is it valid?"))]),
            Some(&fixture.member),
        ))
        .await;
    assert_eq!(
        blocked.status,
        StatusCode::FORBIDDEN,
        "a blocked payload must be refused with 403: {}",
        blocked.body
    );
    // The refusal has to be legible, or it is a support ticket instead of a five-second fix:
    // the code names the control and the label names the value.
    let parsed: Value = serde_json::from_str(&blocked.body).unwrap_or(Value::Null);
    let code = parsed["error"]["code"].as_str().unwrap_or_default();
    assert_eq!(
        code, "ai_guard_blocked",
        "the refusal must name the control: {}",
        blocked.body
    );
    let rendered = blocked.body.to_ascii_lowercase();
    assert!(
        rendered.contains("card"),
        "the refusal must name the label that fired: {}",
        blocked.body
    );

    assert_eq!(
        calls(&received).len(),
        baseline,
        "A BLOCKED REQUEST MUST NOT REACH THE PROVIDER; the stub recorded {:?}",
        calls(&received)
    );

    // The refusal is on the record, read back through the endpoint the events screen reads.
    // `ai_guard.rs` proves the stored row cannot hold the payload; this proves the row exists at
    // all after a refusal, which is the "nobody must be able to argue it did not happen" half.
    //
    // Read as the **member**, not the owner. The events list is organization-filtered and the
    // bootstrap owner is platform-level (`organization_id` null), so an owner-scoped read sees
    // zero rows — which is the same "empty screen, blame the writer" trap `ai_live_path.rs`
    // documents for the decision log, and the reason this file keeps the two sessions apart.
    let (status, events) = harness
        .call_json(get(
            "/api/v1/ai/guard/events?blocked=true",
            Some(&fixture.member),
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the events screen must answer: {events}"
    );
    let rows = events["rows"].as_array().expect("rows array");
    assert_eq!(
        events["total"], 1,
        "exactly one refusal must be on the record: {events}"
    );
    assert_eq!(rows.len(), 1, "one row on the page: {events}");
    assert_eq!(rows[0]["action"], "block", "{events}");
    assert_eq!(rows[0]["error_code"], "ai_guard_blocked", "{events}");
    assert!(
        !events.to_string().contains(CARD),
        "the events response must not carry the refused value: {events}"
    );

    harness.dispose().await;
}

/// A clean payload leaves the body untouched and files no event row.
///
/// The other side of the same checkpoint. Without it the three walks above would pass against a
/// guard that masks or refuses *everything*, which is a guard nobody can use and a set of
/// assertions that would all still be green. The "unchanged" half is what makes `Clear` mean
/// what it says.
#[tokio::test]
async fn a_clean_payload_is_sent_unchanged_and_leaves_no_event_row() {
    let harness = guard!();
    let (base_url, received, _stub) = recording_provider().await;
    let fixture = connected(&harness, &base_url).await;
    mask_email_and_block_card(&harness, &fixture.member, &fixture.owner).await;

    let chat = harness
        .call(post(
            "/api/v1/ai/chat",
            chat_with(vec![user("What is the capital of France?")]),
            Some(&fixture.member),
        ))
        .await;
    assert_eq!(chat.status, StatusCode::OK, "{}", chat.body);

    let seen = calls(&received);
    assert_eq!(seen.len(), 1, "the call must have been dialled: {seen:?}");
    assert!(
        seen[0].contains("What is the capital of France?"),
        "a clear payload must leave unchanged: {}",
        seen[0]
    );

    // No event row: the audit records the guard acting, and a payload with nothing to guard is
    // not the guard acting. One row per ordinary message would drown the events an operator
    // opens the screen to read.
    let count: (i64,) =
        sqlx::query_as("select count(*) from ai_guard_events where organization_id = $1")
            .bind(fixture.organization)
            .fetch_one(harness.db.pool())
            .await
            .expect("the event table must be readable");
    assert_eq!(
        count.0, 0,
        "a clear payload must file no event row for this tenant"
    );

    harness.dispose().await;
}

/// The tester answers a verdict and **never dials the provider**.
///
/// The `ai_guard.rs` header defers this claim to the checkpoint's route; this is where it is
/// earned. A tester that dialled would be an oracle over the tenant's rule set *and* a way to
/// send whatever an operator pasted to a vendor — the two together are why "no provider call" is
/// a criterion rather than a nicety.
///
/// Read straight out of the database rather than through an endpoint on purpose: this is the
/// claim that the value never reaches storage, so the storage itself has to be what is inspected.
/// An endpoint would only prove the endpoint redacts.
async fn audit_rows(harness: &Harness, organization: &Uuid) -> Vec<Value> {
    let rows: Vec<(String, Value)> = sqlx::query_as::<_, (String, Value)>(
        "SELECT action, metadata FROM audit_log WHERE organization_id = $1 ORDER BY created_at DESC",
    )
    .bind(organization)
    .fetch_all(harness.db.pool())
    .await
    .expect("the audit rows must be readable");
    rows.into_iter()
        .map(|(action, metadata)| json!({ "action": action, "metadata": metadata }))
        .collect()
}

/// The same stub, answering in **SSE framing** instead of a bare JSON body.
///
/// A separate function rather than a flag on [`echoing_provider`]: the difference is the whole
/// point of the test, and a boolean at the call site would read as a formatting option when it is
/// the wire contract. The client parses `data:` lines and treats anything else as no answer at
/// all, so a stub that replies with plain JSON fails with "the provider streamed no answer and no
/// finish reason" — which is a fault of the stub and reads exactly like a product fault.
async fn streaming_echoing_provider() -> (String, Received, tokio::task::JoinHandle<()>) {
    let received: Received = Arc::new(Mutex::new(Vec::new()));

    let recorder = Arc::clone(&received);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the stub must bind a port");
    let address = listener.local_addr().expect("the stub has an address");

    let app = Router::new()
        .route(
            "/v1/models",
            route_get(|| async { Json(json!({ "data": [] })) }),
        )
        .route(
            "/v1/chat/completions",
            route_post(move |body: String| {
                let recorder = Arc::clone(&recorder);
                async move {
                    recorder.lock().expect("recorder").push(body.clone());
                    let echoed = serde_json::from_str::<Value>(&body)
                        .ok()
                        .and_then(|parsed| {
                            parsed["messages"].as_array().and_then(|messages| {
                                messages
                                    .iter()
                                    .rev()
                                    .find(|message| message["role"] == "user")
                                    .and_then(|message| {
                                        message["content"].as_str().map(String::from)
                                    })
                            })
                        })
                        .unwrap_or_default();

                    let frame = json!({
                        "choices": [{ "delta": { "content": echoed }, "finish_reason": "stop" }],
                        "usage": { "prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7 }
                    })
                    .to_string();
                    let body = format!("data: {frame}\n\ndata: [DONE]\n\n");
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        body,
                    )
                }
            }),
        );
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}/v1"), received, task)
}

/// The `done` frame of a chat stream, read out of the SSE body.
fn done_frame(body: &str) -> Value {
    body.split("\n\n")
        .filter(|frame| frame.contains("event: done"))
        .filter_map(|frame| {
            frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .and_then(|data| serde_json::from_str::<Value>(data).ok())
        })
        .next()
        .unwrap_or_else(|| panic!("the stream carried no done frame: {body}"))
}

/// Every `delta` payload of a chat stream, in order.
fn delta_frames(body: &str) -> Vec<String> {
    body.split("\n\n")
        .filter(|frame| frame.contains("event: delta"))
        .filter_map(|frame| {
            frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .and_then(|data| serde_json::from_str::<Value>(data).ok())
                .and_then(|payload| payload["content"].as_str().map(String::from))
        })
        .collect()
}

/// The answer is re-mapped on completion: the requester gets the original, the audit does not.
///
/// Both halves are read off the same request. The `done` frame is the requester's own view and
/// must contain the address the user typed; the audit row is a second reader's view and must not.
/// Asserting only the first would pass against a route that substituted everywhere — which is the
/// leak this control exists to stop.
#[tokio::test]
async fn the_requester_reads_the_original_back_and_the_audit_row_does_not() {
    let harness = guard!();
    let (base_url, received, _stub) = streaming_echoing_provider().await;
    let fixture = connected(&harness, &base_url).await;
    mask_email_and_block_card(&harness, &fixture.member, &fixture.owner).await;

    let chat = harness
        .call(post(
            "/api/v1/ai/chat",
            chat_with(vec![user(&format!("Write to {ADDRESS} about the invoice"))]),
            Some(&fixture.member),
        ))
        .await;
    assert_eq!(chat.status, StatusCode::OK, "{}", chat.body);

    // The provider saw the placeholder, so the echo carries it back — which is the precondition
    // for the substitution proving anything.
    let seen = calls(&received);
    assert_eq!(seen.len(), 1, "one call must have been dialled: {seen:?}");
    assert!(
        seen[0].contains("[EMAIL_1]") && !seen[0].contains(ADDRESS),
        "the provider must have seen the placeholder and not the address: {}",
        seen[0]
    );

    let done = done_frame(&chat.body);
    let answer = done["answer"]
        .as_str()
        .unwrap_or_else(|| panic!("the done frame carried no answer: {done}"));
    assert!(
        answer.contains(ADDRESS),
        "THE REQUESTER MUST SEE THE ORIGINAL BACK; the answer was: {answer}"
    );
    assert!(
        !answer.contains("[EMAIL_1]"),
        "the requester's own answer must carry no leftover token: {answer}"
    );

    // The audit row is written by the same request, so it is the second reader.
    let audit = audit_rows(&harness, &fixture.organization).await;
    let completed = audit
        .iter()
        .find(|row| row["action"] == "ai.chat.completed")
        .unwrap_or_else(|| {
            panic!(
                "the completed chat wrote no audit row; rows were: {}",
                serde_json::to_string_pretty(&audit).unwrap_or_default()
            )
        });
    let recorded = completed["metadata"]["answer"].as_str().unwrap_or("");
    assert!(
        !recorded.contains(ADDRESS),
        "THE AUDIT ROW MUST NOT CARRY THE ORIGINAL; it read: {recorded}"
    );
    assert!(
        recorded.contains("[EMAIL_1]"),
        "the audit row must carry the placeholder: {recorded}"
    );

    harness.dispose().await;
}

/// While streaming the deltas carry the placeholder; the finished message carries the original.
///
/// The criterion's two halves are asserted separately because they are two different claims: the
/// first is about not leaking mid-stream, the second about the answer being coherent afterwards.
#[tokio::test]
async fn the_deltas_carry_the_placeholder_and_the_finished_answer_the_original() {
    let harness = guard!();
    let (base_url, _received, _stub) = streaming_echoing_provider().await;
    let fixture = connected(&harness, &base_url).await;
    mask_email_and_block_card(&harness, &fixture.member, &fixture.owner).await;

    let chat = harness
        .call(post(
            "/api/v1/ai/chat",
            chat_with(vec![user(&format!("Write to {ADDRESS} about the invoice"))]),
            Some(&fixture.member),
        ))
        .await;
    assert_eq!(chat.status, StatusCode::OK, "{}", chat.body);

    let streamed: String = delta_frames(&chat.body).concat();
    assert!(
        streamed.contains("[EMAIL_1]"),
        "the streamed deltas must carry the placeholder, since a delta already read cannot be \
         recalled: {streamed}"
    );
    assert!(
        !streamed.contains(ADDRESS),
        "THE ADDRESS MUST NOT APPEAR MID-STREAM; the deltas read: {streamed}"
    );

    let answer = done_frame(&chat.body)["answer"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        answer.contains(ADDRESS),
        "the completed message must carry the original: {answer}"
    );

    harness.dispose().await;
}

/// Two different addresses in two turns both become `[EMAIL_1]`, so the token is withheld.
///
/// This is the walk that caught the merge bug: a naive merge hands the reader one of the two
/// values, and which one is a coin toss. The assertion is that **neither** address comes back —
/// a wrong answer here is worse than a token left on screen, because it is plausible.
#[tokio::test]
async fn an_ambiguous_placeholder_is_withheld_rather_than_guessed() {
    let harness = guard!();
    let (base_url, _received, _stub) = streaming_echoing_provider().await;
    let fixture = connected(&harness, &base_url).await;
    mask_email_and_block_card(&harness, &fixture.member, &fixture.owner).await;

    let chat = harness
        .call(post(
            "/api/v1/ai/chat",
            chat_with(vec![
                user(&format!("Write to {ADDRESS} about the invoice")),
                assistant("Noted."),
                user("Then write to grace@example.test about the receipt"),
            ]),
            Some(&fixture.member),
        ))
        .await;
    assert_eq!(chat.status, StatusCode::OK, "{}", chat.body);

    let done = done_frame(&chat.body);
    let answer = done["answer"].as_str().unwrap_or_default();
    assert!(
        !answer.contains(ADDRESS) && !answer.contains("grace@example.test"),
        "AN AMBIGUOUS TOKEN MUST PUT NEITHER ADDRESS BACK; the answer read: {answer}"
    );
    assert!(
        answer.contains("[EMAIL_1]"),
        "the token must stay visible rather than vanish: {answer}"
    );
    let withheld = done["guard_withheld"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        withheld,
        vec![json!("[EMAIL_1]")],
        "the withheld token must be reported so the screen can say so"
    );

    harness.dispose().await;
}

#[tokio::test]
async fn the_tester_answers_a_verdict_without_dialling_the_provider() {
    let harness = guard!();
    let (base_url, received, _stub) = recording_provider().await;
    let fixture = connected(&harness, &base_url).await;
    mask_email_and_block_card(&harness, &fixture.member, &fixture.owner).await;

    let (status, body) = harness
        .call_json(post(
            "/api/v1/ai/guard/test",
            json!({ "payload": format!("Ping {ADDRESS} about {CARD}") }),
            Some(&fixture.member),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "the tester must answer: {body}");
    assert_eq!(
        body["verdict"], "blocked",
        "the card label is at block: {body}"
    );
    assert_eq!(body["would_block"], true, "{body}");
    assert_eq!(body["blocked_label"], "card", "{body}");
    // Eight of the nine seeded rules are enabled; `person_name.builtin` ships disabled, so the
    // running count is eight and `len()` counts *enabled* rules only. Asserting the number is
    // deliberate: it makes the walk notice if the seed grows a rule or somebody re-enables the
    // name list, which would otherwise be a silent change to what every tenant runs.
    assert_eq!(body["rules_evaluated"], 8, "eight seeded rules run: {body}");
    // A blocked finding carries NO outbound text at all — `Finding` builds `text` empty for the
    // `Blocked` arm, so there is no half-masked body for a caller to mistake for a payload that
    // was sent. Asserting the emptiness is the point: a walk that only checked for the
    // placeholder here would pass against a guard that masked its way to a block.
    assert_eq!(
        body["masked_text"], "",
        "a blocked verdict must carry no outbound text: {body}"
    );
    // The matches carry salted hashes, never the value — the same rule the event row obeys.
    let rendered = body["matches"].to_string();
    assert!(
        !rendered.contains(ADDRESS) && !rendered.contains(CARD),
        "a match may never carry the value: {body}"
    );

    assert_eq!(
        calls(&received).len(),
        0,
        "THE TESTER MUST NOT DIAL THE PROVIDER; the stub recorded {:?}",
        calls(&received)
    );

    harness.dispose().await;
}
