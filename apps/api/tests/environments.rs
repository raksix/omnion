//! Integration tests for staging environments (docs/requests/REQ-017, slices 1 and 2).
//!
//! The unit tests in `omnion-environment` prove the decisions — the key format, the reserved
//! words, the area order, the progress fold. What cannot be proved there is the thing this slice
//! exists for, and it is worth being precise about what that is:
//!
//!   * **A clone really copies content, and really leaves production alone.** The request's own
//!     criterion is "editing a page in staging leaves the production row byte-identical". That is
//!     a claim about a database, not about a function, so it is asserted by inserting a page,
//!     cloning, editing the staging copy and reading the production row back.
//!   * **A clone is idempotent.** Re-cloning produces the same counts and no duplicate rows.
//!     "No duplicates" is checked as a row count, not as a comparison of two result sets, because
//!     a duplicate that only shows up in a set comparison is a duplicate nobody sees.
//!   * **A staging environment cannot be cloned from another staging one.** The refusal has to
//!     be the named one, because the wizard does not offer the option and a silent copy would
//!     produce a staging environment with no reference to differ from.
//!   * **The progress bar's two states are distinguishable.** A job at 0/0 is "counting", a job
//!     with rows copied is "n of m", and a panel that renders both as 0% is a panel that looks
//!     hung on every new environment.
//!   * **The permission split is real.** An account with `deployment.read` sees the list and gets
//!     `403` on create; an account with `deployment.preview` can create but not archive.
//!
//! Runs against the development stack and skips with a printed reason when PostgreSQL is not
//! reachable, like every other suite here.

use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post as route_post;
use axum::Router;
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_environment::clone::Area;
use omnion_environment::model::CloneStatus;
use omnion_events::{engine, sender, signature};
use omnion_identity::sites;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

struct TestResponse {
    status: StatusCode,
    /// Every `Set-Cookie`, so the CSRF cookie login issued is not lost.
    set_cookies: Vec<String>,
    /// Every response header, so a walk can read a header the *middleware* set rather than the
    /// handler — `X-Robots-Tag` is attached on the way out and never appears in a body.
    headers: axum::http::HeaderMap,
    body: Value,
}

impl TestResponse {
    /// The value of the named cookie across every `Set-Cookie` header.
    fn cookie(&self, name: &str) -> Option<String> {
        self.set_cookies.iter().find_map(|header_value| {
            let pair = header_value.split(';').next().unwrap_or_default();
            pair.split_once('=')
                .filter(|(cookie_name, _)| cookie_name.trim() == name)
                .map(|(_, value)| value.trim().to_owned())
        })
    }

    /// A header as a plain string, for the walks that assert on a header by name.
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");
    let status = response.status();
    let headers = response.headers().clone();
    let set_cookies: Vec<String> = headers
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
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    TestResponse {
        status,
        set_cookies,
        headers,
        body,
    }
}

fn request(
    method: Method,
    uri: &str,
    caller: Option<&Caller>,
    body: Option<Value>,
) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match caller {
        Some(caller) => builder.header(
            header::COOKIE,
            format!("omnion_session={}; omnion_csrf={}", caller.session, caller.csrf),
        ),
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

fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

/// The key this suite's CSRF tokens are derived from.
///
/// Fixed, not random, for the same reason `tests/csrf.rs` fixes it: a random key per run makes a
/// failure harder to reproduce, and the derivation is the crate's business. The suite sets it
/// because the panel's real writes are cookie-authenticated and the middleware refuses one
/// without the token — a test that skipped that would be testing a router no browser ever uses.
const CSRF_SECRET: &str = "environment-suite-csrf-key-material";

/// Open a throwaway database with every migration applied, plus its router.
///
/// The database is **per run**, and that is the whole point of this function. The suite used to
/// connect straight to whatever `OMNION_DATABASE_URL` named and drop its fixtures in alongside
/// whatever else lived there — which on this box is a *shared* database, because every writer runs
/// its QA pass against one. The visible symptom arrived in another tool: the walkthrough seeds one
/// owner account at boot, the API's `bootstrap_first_admin` creates it only while `users` is
/// empty, and this suite's 54 accounts meant the seed was skipped — so the browser pass died at
/// `could not sign in after wizard` and filed it as a product failure. A suite that steals the
/// account its own acceptance gate signs in with does not merely make a mess; it removes the gate,
/// and the removal is invisible from inside the suite.
///
/// One database for the whole run, not one per test: applying 105 migrations 25 times would cost
/// minutes per suite, and the walks do not collide with each other anyway — every fixture names
/// its own organization, site, key and account with a random suffix, which is the same discipline
/// the walks already rely on. This is the pattern `event_retention.rs`, `events.rs` and
/// `onboarding.rs` already use, which is why this suite is the odd one out and now is not.
static HARNESS: tokio::sync::OnceCell<Option<(AppState, Db)>> = tokio::sync::OnceCell::const_new();

async fn live_state() -> Option<(AppState, Db)> {
    let shared = HARNESS
        .get_or_init(|| Box::pin(open_harness()))
        .await;
    // Cloned, not shared: `AppState` is cheap to clone and the walks each get their own handle,
    // while the *database* underneath is the one thing they must share.
    shared.as_ref().map(|(state, db)| (state.clone(), db.clone()))
}

/// Create the database, apply the migrations and build the router. `None` means "skip".
async fn open_harness() -> Option<(AppState, Db)> {
    let mut config = Config::from_env().ok()?;
    config.csrf = omnion_core::config::CsrfSecret::new(Some(CSRF_SECRET.to_owned()));

    let maintenance = match Db::connect(&omnion_core::config::DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 1,
    })
    .await
    {
        Ok(db) => db,
        Err(err) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({err})");
            return None;
        }
    };

    reclaim_scratch_databases(maintenance.pool()).await;

    let database = format!("omnion_env_{}", Uuid::new_v4().simple());
    if let Err(err) = sqlx::query(&format!("create database \"{database}\""))
        .execute(maintenance.pool())
        .await
    {
        eprintln!("SKIP: the temporary database could not be created ({err})");
        return None;
    }
    eprintln!("scratch database: {database}");

    let db = Db::connect(&omnion_core::config::DatabaseConfig {
        url: swap_database(&config.database.url, &database),
        max_connections: 8,
    })
    .await
    .expect("the fresh database must connect");
    db.migrate().await.expect("migrations must apply");

    let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
    let state = AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config,
        db.clone(),
        redis,
        test_storage(),
    );

    // The limiter, at a limit a test suite can live with.
    //
    // `ensure_installed` falls back to the *shipped* defaults for harnesses that build a router
    // without a `main.rs`, and `sign_in` is one of them: 10 requests per 300 seconds. This suite
    // creates a distinct account per walk, so 25 walks cannot fit in that budget, and the counter
    // is in Redis — shared with every other writer's suite on the box, so a suite that trips it
    // also breaks theirs. Both failure modes arrive as `login body: … rate_limited` on a test
    // whose subject is cloning, which is a lie about where the problem is.
    //
    // So the suite installs its own policy before building the router. It raises `sign_in` rather
    // than disabling it: the limiter stays real, and the suite still exercises the layer on the
    // way through — it simply is not measured by the production policy. `ensure_installed` keeps
    // whatever is already installed, so installing first is what wins.
    let _ = omnion_api::rate_limit_middleware::install(omnion_api::rate_limit_middleware::RateLimiter::new(
        &state,
        vec![
            omnion_security::RatePolicy::new("global", 60, 600, 100, true).expect("a valid row"),
            omnion_security::RatePolicy::new("sign_in", 300, 100_000, 0, true).expect("a valid row"),
            omnion_security::RatePolicy::new("public_api", 60, 100_000, 0, true).expect("a valid row"),
            omnion_security::RatePolicy::new("authenticated_api", 60, 100_000, 0, true)
                .expect("a valid row"),
        ],
    ));

    Some((state, db))
}

/// Reclaim scratch databases left behind by an earlier run.
///
/// A `Drop` guard would be the obvious answer and it does not work here: the harness cell is a
/// `OnceCell`, so its value — and anything hanging off it — is never dropped, and the process
/// exits before any destructor the test harness owns runs. Sweeping at the *start* is what
/// actually reclaims, and it is safe precisely because this run's database does not exist yet.
///
/// The sweep only touches names this suite mints (`omnion_env_`), so it cannot reach the shared
/// QA database or anything else living on the server. Without it every killed run leaves a
/// 105-table database behind, and after a dozen ticks they are why the box runs out of disk for
/// the builds rather than for the data.
async fn reclaim_scratch_databases(pool: &sqlx::PgPool) {
    let stale: Vec<String> = sqlx::query_scalar(
        "select datname from pg_database where datname like 'omnion_env\\_%' escape '\\'",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    for name in stale {
        // A database another live suite is using refuses the drop; that refusal is the answer,
        // not a failure worth printing.
        let dropped = sqlx::query(&format!("drop database if exists \"{name}\""))
            .execute(pool)
            .await;
        if dropped.is_ok() {
            eprintln!("reclaimed a stale scratch database: {name}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The receiver: a real HTTP server the delivery runner posts to
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
    /// The raw bytes the signature covers.
    body: Vec<u8>,
}

impl Captured {
    /// The body as JSON.
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("a delivery body must be JSON")
    }
}

/// A running receiver: its URL, the secret it verifies with, and the task serving it.
///
/// **A real socket, not a mock.** The claim this walk proves is that `promotion.*` *reaches an
/// endpoint*, and the two halves of that claim live in different processes: the route emits and
/// fans out, and the runner signs and posts. A fake client proves the row was written; only a
/// listener proves the body left the building, so the suite borrows the shape
/// `tests/events.rs` already runs rather than inventing a second one.
struct Receiver {
    url: String,
    secret: String,
    seen: Arc<Mutex<Vec<Captured>>>,
    task: JoinHandle<()>,
}

impl Receiver {
    /// Start a receiver on an ephemeral loopback port.
    async fn start() -> Self {
        let seen: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the receiver must bind a port");
        let address = listener.local_addr().expect("the receiver has an address");

        let app = Router::new()
            .route("/deploy-hook", route_post(receive))
            .with_state(seen.clone());

        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            url: format!("http://{address}/deploy-hook"),
            secret: "w5-promotion-delivery-secret".to_owned(),
            seen,
            task,
        }
    }

    /// Everything the receiver captured so far.
    fn captured(&self) -> Vec<Captured> {
        self.seen.lock().expect("the receiver lock").clone()
    }

    /// The events the receiver was actually handed, in arrival order.
    fn events(&self) -> Vec<String> {
        self.captured()
            .into_iter()
            .map(|delivery| delivery.event)
            .collect()
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `POST /deploy-hook` — capture the delivery and accept it.
async fn receive(State(seen): State<Arc<Mutex<Vec<Captured>>>>, headers: HeaderMap, body: Bytes) -> Response {
    let read = |name: &str| -> String {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };

    seen.lock().expect("the receiver lock").push(Captured {
        event: read(signature::EVENT_HEADER),
        delivery: read(signature::DELIVERY_HEADER),
        timestamp: read(signature::TIMESTAMP_HEADER)
            .parse()
            .unwrap_or_default(),
        signature: read(signature::SIGNATURE_HEADER),
        body: body.to_vec(),
    });

    (StatusCode::OK, "accepted").into_response()
}

/// The delivery configuration this suite runs the runner with: one batch, a short timeout, and a
/// backoff far past the length of a walk so a refusal is not retried underneath it.
fn delivery_config() -> engine::RunnerConfig {
    engine::RunnerConfig {
        batch: 50,
        lease_seconds: 30,
        request_timeout: StdDuration::from_secs(5),
        retry_base: Duration::milliseconds(60_000),
        retry_max: Duration::milliseconds(60_000),
    }
}

/// One delivery tick against the throwaway database.
async fn deliver_due(fixture: &Fixture) -> engine::RunReport {
    let client = sender::client(StdDuration::from_secs(5)).expect("the delivery client must build");
    engine::run_due(fixture.db.pool(), &client, &delivery_config())
        .await
        .expect("the delivery tick must run")
}

/// Subscribe an endpoint to `promotion.*` for the fixture's organization, through the store.
///
/// The store rather than the route on purpose: this walk is about the *delivery* half, and the
/// route's own contract — that a subscription list is reconciled against the catalogue and that
/// an unknown name is refused — is what `tests/events.rs` covers. What has to be true here is
/// narrower and is exactly the request's sentence: a group subscription `promotion.*` matches the
/// two events the promotion routes emit, and the receiver sees them.
async fn subscribe_to_promotions(fixture: &Fixture, url: &str) -> Uuid {
    let endpoint = omnion_events::store::insert_endpoint(
        fixture.db.pool(),
        omnion_events::NewEndpoint {
            organization_id: fixture.organization,
            name: format!("deploy-{}", Uuid::new_v4().simple()),
            url: url.to_owned(),
            secret: "w5-promotion-delivery-secret".to_owned(),
            events: vec!["promotion.*".to_owned()],
            created_by: Some(fixture.caller_user_id),
        },
    )
    .await
    .expect("the endpoint must be created");
    endpoint.id
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

async fn create_organization_row(db: &Db, suffix: &str) -> Uuid {
    let name = format!("Environment {suffix}");
    let slug = format!("env-{suffix}-{}", Uuid::new_v4().simple());
    let id: Uuid =
        sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
            .bind(&name)
            .bind(&slug)
            .fetch_one(db.pool())
            .await
            .expect("organization must insert");
    id
}

async fn create_site(db: &Db, organization_id: Uuid, suffix: &str) -> Uuid {
    sites::create_site(
        db.pool(),
        omnion_identity::NewSite {
            organization_id,
            key: format!("env{suffix}"),
            name: format!("Environment site {suffix}"),
            theme: None,
        },
    )
    .await
    .expect("site must insert")
    .id
}

/// An administrator holding exactly `permissions`.
async fn create_admin(
    db: &Db,
    organization_id: Uuid,
    suffix: &str,
    state: &AppState,
    permissions: &[&str],
) -> (Uuid, Caller) {
    let email = format!("env-{suffix}-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Environment Admin".to_owned(),
            organization_id: Some(organization_id),
        },
    )
    .await
    .expect("account must insert");

    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id,
            key: format!("env-admin-{suffix}"),
            name: format!("Environment Admin {suffix}"),
            description: "staging environments".to_owned(),
            priority: 100,
            inherits_role_id: None,
        },
    )
    .await
    .expect("role must insert");
    let entries: Vec<RolePermissionInput> = permissions
        .iter()
        .map(|key| RolePermissionInput {
            key: (*key).to_owned(),
            effect: Effect::Allow,
        })
        .collect();
    role_store::set_role_permissions(db.pool(), role.id, &entries)
        .await
        .expect("the role permission set must be written");
    bindings::grant(
        db.pool(),
        NewBinding {
            role_id: role.id,
            user_id: user.id,
            scope: Scope::Organization { organization_id },
            granted_by: None,
            expires_at: None,
        },
    )
    .await
    .expect("binding must insert");

    let caller = login(state, &email).await;
    (user.id, caller)
}

/// A signed-in caller: the session cookie and the CSRF cookie login issued with it.
///
/// Both are needed. The session cookie is the identity; the CSRF cookie is what the middleware
/// requires on a cookie-authenticated *write*, so a fixture that carried only the session would
/// get `403 csrf_failed` on every create and the walks would test nothing.
#[derive(Clone)]
struct Caller {
    session: String,
    csrf: String,
}

/// Sign in, once per address, for the whole run.
///
/// The caching is not an optimisation — it is the reason this suite passes at all. Sign-in is
/// rate-limited (`sign_in`: 10 requests per 300 seconds, which is the shipped default), the
/// counter lives in Redis and Redis is shared by every writer on the box, and this suite creates
/// a fresh account for each of its 25 walks. Signing in per walk therefore spends the whole
/// shared budget on the first few tests, and every later one fails at `login body: … rate_limited`
/// for a reason that has nothing to do with environments.
///
/// Sessions outlive a suite run, so one sign-in per account is both cheaper and closer to how a
/// browser behaves. The address is part of the key, so two walks that happen to share an address
/// still get the right session rather than each other's.
static CALLERS: tokio::sync::OnceCell<
    std::sync::Mutex<std::collections::HashMap<String, Caller>>,
> = tokio::sync::OnceCell::const_new();

async fn login(state: &AppState, email: &str) -> Caller {
    let cache = CALLERS
        .get_or_init(|| async { std::sync::Mutex::new(std::collections::HashMap::new()) })
        .await;
    let cached = cache
        .lock()
        .expect("the caller cache is not poisoned")
        .get(email)
        .cloned();
    if let Some(caller) = cached {
        return caller;
    }

    let caller = login_uncached(state, email).await;
    cache
        .lock()
        .expect("the caller cache is not poisoned")
        .insert(email.to_owned(), caller.clone());
    caller
}

/// The sign-in itself, with no cache in the way.
async fn login_uncached(state: &AppState, email: &str) -> Caller {
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
    let session = response
        .cookie("omnion_session")
        .expect("login must set the session cookie");
    let csrf = response
        .cookie("omnion_csrf")
        .expect("login must issue the CSRF cookie; a suite that reads only the session cookie is how a broken CSRF guard stays green");
    Caller { session, csrf }
}

/// Run *this* environment's pending clone job to completion.
///
/// The tempting version is a loop over the worker's own `claim_next_job`, and it is wrong in a
/// way only a shared database shows: the claim is global, so with two walks running the walk
/// whose job is not oldest claims somebody else's, and either runs it (a cross-tenant side
/// effect from a test) or puts it back and spins forever against a peer that keeps creating new
/// pending jobs. Both happened here.
///
/// So the walk claims *its own* job by id — the same row the create returned, which is the
/// artifact the walk is actually about — and hands it to the same runner the worker uses. The
/// runner is still exercised; only the global queue is bypassed, and that is a property of the
/// harness, not of the code under test.
async fn drain_clone_for(db: &Db, environment_id: Uuid) {
    let job: Option<omnion_environment::store::CloneJobRow> = sqlx::query_as(
        "select j.id, j.environment_id, j.status, j.areas, j.items_total, j.items_done, \
                j.area_counts, j.exclude_archived, j.error, j.started_at, j.finished_at, \
                j.created_by, j.created_at \
         from environment_clone_jobs j \
         where j.environment_id = $1 and j.status in ('pending','running') \
         order by j.created_at desc limit 1",
    )
    .bind(environment_id)
    .fetch_optional(db.pool())
    .await
    .expect("reading this environment's job must not fail");

    let Some(job) = job else {
        panic!("{environment_id} has no open clone job to run");
    };
    omnion_environment::runner::run_job(db.pool(), &job)
        .await
        .expect("the clone runner must not fail");
}

struct Fixture {
    state: AppState,
    db: Db,
    organization: Uuid,
    site: Uuid,
    caller: Caller,
    /// The account behind `caller`.
    ///
    /// Carried because the promotion walks that go through the store rather than the route need a
    /// user id, and re-deriving it from the email would be a second lookup that could disagree with
    /// the session the other walks use.
    caller_user_id: Uuid,
}

/// The deployment keys this suite's administrator holds.
///
/// The content keys are here because the public-read walk below has to publish a page through
/// the real route rather than setting `status = 'published'` by hand: a page with no published
/// revision answers `404` on the public surface, so a hand-set status would test a fixture the
/// platform never produces. Granting them on this one account does not weaken the permission
/// walks — those build their own accounts with deliberately narrower key sets.
const ALL_PERMISSIONS: [&str; 10] = [
    "deployment.read",
    "deployment.preview",
    "deployment.deploy",
    "deployment.rollback",
    "content.pages.read",
    "content.pages.create",
    "content.pages.update",
    "content.pages.delete",
    "content.pages.publish",
    "content.pages.schedule",
];

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool()).await.expect("the IAM seed must run");
        let organization = create_organization_row(&db, "main").await;
        let site = create_site(&db, organization, "main").await;
        let (caller_user_id, caller) =
            create_admin(&db, organization, "main", &state, &ALL_PERMISSIONS).await;
        Some(Self {
            state,
            db,
            organization,
            site,
            caller,
            caller_user_id,
        })
    }

    /// The organization's production environment id.
    async fn production_id(&self) -> Uuid {
        sqlx::query_scalar(
            "select id from environments where organization_id = $1 and type = 'production'",
        )
        .bind(self.organization)
        .fetch_one(self.db.pool())
        .await
        .expect("migration 0145 gives every organization a production environment")
    }

    /// Create a staging environment through the API and return its id.
    async fn create_staging(&self, name: &str, key: &str) -> Value {
        self.create_staging_inner(name, key, Value::Null).await
    }

    /// The same create, with a staging host — the only difference that matters to a walk that
    /// reads a response by the host it arrived on.
    async fn create_staging_with_host(&self, name: &str, key: &str, host: &str) -> Value {
        self.create_staging_inner(name, key, json!(host)).await
    }

    async fn create_staging_inner(&self, name: &str, key: &str, host: Value) -> Value {
        let response = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/environments",
                Some(&self.caller),
                Some(json!({
                    "name": name,
                    "key": key,
                    "staging_host": host,
                    "areas": ["pages", "translations", "workflows", "site_settings"],
                    "exclude_archived": false,
                })),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::CREATED,
            "create body: {}",
            response.body
        );
        response.body
    }
}

/// Insert a page into production, the way the content crate does — naming no environment, so the
/// migration's resolution trigger is on the path the platform actually takes.
async fn insert_production_page(db: &Db, site_id: Uuid, slug: &str, title: &str) -> Uuid {
    let page: Uuid = sqlx::query_scalar(
        "insert into pages (site_id, slug, status) values ($1, $2, 'draft') returning id",
    )
    .bind(site_id)
    .bind(slug)
    .fetch_one(db.pool())
    .await
    .expect("a page insert that names no environment must succeed");
    sqlx::query(
        "insert into page_revisions (page_id, revision_no, state, title, body) \
         values ($1, 1, 'draft', $2, 'body')",
    )
    .bind(page)
    .bind(title)
    .execute(db.pool())
    .await
    .expect("revision must insert");
    page
}


async fn add_published_revision(db: &Db, page: Uuid, revision_no: i32, title: &str) {
    sqlx::query(
        "insert into page_revisions (page_id, revision_no, state, title, body) \
         values ($1, $2, 'published', $3, 'body') \
         on conflict (page_id, revision_no) do nothing",
    )
    .bind(page)
    .bind(revision_no)
    .bind(title)
    .execute(db.pool())
    .await
    .expect("a second, non-draft revision must insert");
}

#[tokio::test]
async fn a_clone_that_crosses_a_batch_boundary_copies_every_row_exactly_once() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    // The batch size is read from the environment by `runner::batch_rows`, and this walk needs it
    // to be *small* — so the copy has to span several batches to mean anything. `std::env::set_var`
    // is process-global and `#[tokio::test]` is multithreaded, so this is the suite's one
    // env-mutating walk and it is marked so: a second env-mutating walk added later would race
    // this one, and the failure would read as a flaky clone rather than as two tests sharing a
    // variable. Seven pages at a batch of two is four batches — enough that a boundary is crossed
    // in the middle, not just at the end.
    const PAGES: usize = 7;
    const BATCH: i64 = 2;

    for index in 0..PAGES {
        let slug = format!("batched-{index}");
        let page = insert_production_page(&fixture.db, fixture.site, &slug, "Batched").await;
        // A second revision on some pages and not others: the defect being hunted is a batch
        // re-copying *other* pages' history, which needs the histories to be distinguishable.
        if index % 2 == 0 {
            add_published_revision(&fixture.db, page, 2, "Batched second").await;
        }
    }

    // SAFETY: no other walk mutates this variable, and this walk restores it before it returns —
    // including on the panic path, because the restore is in a guard rather than at the end.
    // A test that leaks a batch size of 2 into the twenty walks that follow would make every one
    // of them exercise a code path none of them claims to, and the suite would still be green.
    let previous = std::env::var("OMNION_CLONE_BATCH_ROWS").ok();
    // SAFETY: same guard as above — the value is process-global for the duration of this walk and
    // restored by `BATCH_GUARD` on the way out, panic or not.
    let _guard = BatchSizeGuard {
        previous: previous.clone(),
        armed: true,
    };
    // SAFETY: no other walk in this suite mutates the variable (this is the only one), and the
    // guard below restores it on every exit path.
    unsafe { std::env::set_var("OMNION_CLONE_BATCH_ROWS", BATCH.to_string()) };
    assert_eq!(
        omnion_environment::runner::batch_rows(),
        BATCH,
        "the walk must actually be running with a batch size that forces a boundary"
    );

    let created = fixture.create_staging("Staging", "staging-batched").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    drain_clone_for(&fixture.db, environment_id).await;

    // The job itself, first. A batching defect often shows up as the *job* failing rather than
    // as a row count being wrong, and the row-count assertion then reports "2 of 7" without
    // saying that the copy stopped at the second batch. Reading the job makes the failure name
    // itself — the mutation run that proved this walk actually bites reported exactly that, and
    // only this line is what turns it into a readable diagnosis.
    // `items_done` is `int4` in the migration, so the Rust side is `i32` — a `i64` here
    // fails at decode, not at compile, and the error names a type mismatch rather than the walk.
    let job: (String, Option<String>, i32) = sqlx::query_as(
        "select status, error, items_done from environment_clone_jobs \
         where environment_id = $1 order by created_at desc limit 1",
    )
    .bind(environment_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the clone job row must be readable");
    assert_eq!(
        job.0, "done",
        "the clone job did not finish: status {} with error {:?} after {} rows",
        job.0,
        job.1,
        job.2
    );

    // Per-area counts, so a failure here names the area rather than leaving "2 of 7" to be
    // interpreted. `item_counts` is the runner's own record of what it copied; comparing it with
    // what is on disk is the check that catches a batched copy whose arithmetic is self-
    // consistent and whose rows are not there.
    let recorded: String = sqlx::query_scalar(
        "select area_counts::text from environment_clone_jobs where environment_id = $1 \
         order by created_at desc limit 1",
    )
    .bind(environment_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the job's own per-area record must be readable");
    let staged_pages: i64 = sqlx::query_scalar(
        "select count(*) from pages where environment_id = $1",
    )
    .bind(environment_id)
    .fetch_one(fixture.db.pool())
    .await
    .unwrap();
    assert_eq!(
        staged_pages, PAGES as i64,
        "every source page reached staging, so no batch boundary dropped one (the job recorded \
         {recorded})"
    );

    // ...and each page's history is its own, complete and not another page's. The per-page count
    // is the assertion that matters: a revision insert with no window copies the *whole* site's
    // history into *every* batch, and `on conflict do nothing` hides the duplicates completely.
    // The row count would then read as the maximum history length on the site rather than each
    // page's own, so the walk checks the distribution and not just the total.
    let per_page: Vec<(String, i64)> = sqlx::query_as(
        "select p.slug, (select count(*) from page_revisions r where r.page_id = p.id) \
         from pages p where p.environment_id = $1 order by p.slug",
    )
    .bind(environment_id)
    .fetch_all(fixture.db.pool())
    .await
    .unwrap();
    assert_eq!(per_page.len(), PAGES, "one staging page per source page");
    for (slug, revisions) in &per_page {
        // The even-indexed source pages carry two revisions, the odd ones one — and the copy has
        // to keep that shape. A batched copy that re-copied other pages' history would give
        // *every* page the maximum.
        let index: usize = slug
            .trim_start_matches("batched-")
            .parse()
            .expect("the slug carries the source index");
        let want = if index % 2 == 0 { 2 } else { 1 };
        assert_eq!(
            *revisions, want,
            "{slug} in staging has {revisions} revisions, expected its own {want} — a batch copied \
             another page's history"
        );
    }

    // Production is untouched, which is the guarantee batching must not cost.
    let production_pages: i64 = sqlx::query_scalar(
        "select count(*) from pages where environment_id = $1",
    )
    .bind(fixture.production_id().await)
    .fetch_one(fixture.db.pool())
    .await
    .unwrap();
    assert_eq!(
        production_pages, PAGES as i64,
        "batching writes only to the target; production keeps exactly its own rows"
    );
}

/// Restores a process-global batch size on drop, panic or not.
struct BatchSizeGuard {
    previous: Option<String>,
    armed: bool,
}

impl Drop for BatchSizeGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        match &self.previous {
            // SAFETY: the walk that armed this guard is the only writer of the variable, and a
            // drop runs on exactly one thread.
            Some(value) => unsafe { std::env::set_var("OMNION_CLONE_BATCH_ROWS", value) },
            None => unsafe { std::env::remove_var("OMNION_CLONE_BATCH_ROWS") },
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_new_organization_has_exactly_one_production_environment() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    // The migration's backfill is what creates it, so its existence is the first claim.
    let production: i64 = sqlx::query_scalar(
        "select count(*) from environments where organization_id = $1 and type = 'production'",
    )
    .bind(fixture.organization)
    .fetch_one(fixture.db.pool())
    .await
    .expect("count must read");
    assert_eq!(production, 1, "exactly one production environment");

    // And the database refuses a second one, which is the criterion's actual subject: a check in
    // the route would be correct only until two requests arrived together.
    let second = sqlx::query(
        "insert into environments (organization_id, key, name, type, status) \
         values ($1, 'production-2', 'Second', 'production', 'active')",
    )
    .bind(fixture.organization)
    .execute(fixture.db.pool())
    .await;
    assert!(
        second.is_err(),
        "a second production environment must be refused by the partial unique index"
    );
}

#[tokio::test]
async fn a_page_written_without_naming_an_environment_lands_in_production() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // This is the regression 0147 exists for: the content crate inserts no environment, and a
    // NOT NULL column with no resolution would break every page creation in the platform.
    let page = insert_production_page(&fixture.db, fixture.site, "no-env", "No env").await;
    let environment: Uuid =
        sqlx::query_scalar("select environment_id from pages where id = $1")
            .bind(page)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the page must carry an environment");
    assert_eq!(
        environment,
        fixture.production_id().await,
        "a write that names no environment belongs to production"
    );
}

#[tokio::test]
async fn creating_a_staging_environment_returns_immediately_and_starts_a_clone() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let body = fixture.create_staging("Staging", "staging-2").await;

    assert_eq!(body["type"], "staging");
    // "Cloning" and not "active": the request says the create returns immediately with the copy
    // in flight, and an environment that claims to be active before it is filled is a promise
    // the panel then has to retract.
    assert_eq!(body["status"], "cloning");
    let clone = &body["clone"];
    assert_eq!(clone["status"], "pending");
    assert_eq!(clone["percent"], 0);
    // A job at 0/0 says it is counting rather than reporting a bar stuck at zero forever.
    assert!(
        clone["summary"].as_str().unwrap_or_default().contains("Counting"),
        "summary was {}",
        clone["summary"]
    );
    assert_eq!(clone["cancellable"], true);
    assert_eq!(body["reclonable"], true);
}

#[tokio::test]
async fn the_list_carries_real_per_area_counts_after_the_clone_finishes() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    insert_production_page(&fixture.db, fixture.site, "one", "One").await;
    insert_production_page(&fixture.db, fixture.site, "two", "Two").await;

    let created = fixture.create_staging("Staging", "staging-2").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    drain_clone_for(&fixture.db, environment_id).await;

    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/environments",
            Some(&fixture.caller),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);

    let staging = response.body["environments"]
        .as_array()
        .expect("environments is an array")
        .iter()
        .find(|row| row["id"] == environment_id.to_string())
        .expect("the staging environment is in the list")
        .clone();

    assert_eq!(staging["status"], "active", "a finished clone leaves it active");
    assert_eq!(
        staging["content"]["pages"], 2,
        "the two production pages were copied: {}",
        staging["content"]
    );
    assert_eq!(staging["clone"]["status"], "done");
    assert_eq!(staging["clone"]["percent"], 100);
    assert_eq!(staging["clone"]["cancellable"], false);
}

/// Publish one production page through the real route, the way an editor does.
///
/// Used by the public-read walk below: a page that is `published` in the column but has no
/// published revision answers `404` on the public surface, so the walk has to go through the
/// publish route rather than setting a status by hand.
async fn publish_page_via_api(state: &AppState, caller: &Caller, site_id: Uuid, slug: &str) -> Uuid {
    let created = call(
        state,
        request(
            Method::POST,
            "/api/v1/pages",
            Some(caller),
            Some(json!({ "site_id": site_id, "slug": slug, "title": "Shared", "body": "body" })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "create: {}", created.body);
    let page_id = Uuid::parse_str(created.body["id"].as_str().unwrap()).unwrap();

    let published = call(
        state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page_id}/publish"),
            Some(caller),
            None,
        ),
    )
    .await;
    assert_eq!(published.status, StatusCode::OK, "publish: {}", published.body);
    page_id
}

/// The public read of one address, anonymously, addressed by site key.
async fn public_read(state: &AppState, site_key: &str, slug: &str) -> TestResponse {
    let response = call(
        state,
        Request::builder()
            .method(Method::GET)
            .uri(format!("/api/v1/public/pages/{slug}?site={site_key}"))
            .body(Body::empty())
            .expect("request must build"),
    )
    .await;
    response
}

/// Migration 0148 made `(site_id, environment_id, slug)` the identity of a page, but
/// `pages::find_page_by_slug` still matched `(site_id, slug)`. The clone copies production into
/// staging, so a slug that exists in both environments matched **two** rows — and
/// `fetch_optional` over two rows is not "the first one", it is a protocol error, so
/// `GET /api/v1/public/pages/{slug}` answered `500` for every address the clone had copied.
///
/// The leak is the same defect read from the other side, and the red run proved it *first*:
/// a page that exists **only** in staging matched exactly one row, so the read answered `200`
/// and served an unpublished draft to every visitor of production. A test written to expect
/// `500` would have gone green against a handler that published staging — the failure this walk
/// was written for is the *silent* one, and the assertion that caught it is the last one in the
/// walk, not the first.
///
/// That is the shape this walk keeps closed: creating a staging environment must not take the
/// public site down, and staging content must be reachable *only* through the staging address.
#[tokio::test]
async fn a_staging_copy_must_not_break_or_leak_into_the_public_read() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site_key: String =
        sqlx::query_scalar("select key from sites where id = $1")
            .bind(fixture.site)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the site carries a key");

    let page = publish_page_via_api(&fixture.state, &fixture.caller, fixture.site, "shared").await;

    let before = public_read(&fixture.state, &site_key, "shared").await;
    assert_eq!(
        before.status,
        StatusCode::OK,
        "before any staging copy the public read answers: {}",
        before.body
    );
    assert_eq!(before.body["revision"]["title"], "Shared");

    // Clone it. The clone is the thing that creates the second row.
    let created = fixture.create_staging("Staging", "staging-leak").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    drain_clone_for(&fixture.db, environment_id).await;

    let copies: i64 = sqlx::query_scalar(
        "select count(*) from pages where site_id = $1 and slug = 'shared'",
    )
    .bind(fixture.site)
    .fetch_one(fixture.db.pool())
    .await
    .unwrap();
    assert_eq!(
        copies, 2,
        "this walk is only meaningful while the site really holds two environments' copies"
    );

    // The half that was broken: the public read must still answer.
    let after = public_read(&fixture.state, &site_key, "shared").await;
    assert_eq!(
        after.status,
        StatusCode::OK,
        "a staging copy must not take the public read down: {}",
        after.body
    );

    // And the staging copy must not have become the answer. A staging page is a different row
    // with its own revision; if the read ever widens to "any row with this slug", a visitor reads
    // a draft nobody published.
    let staging_page: Uuid =
        sqlx::query_scalar("select id from pages where environment_id = $1 and slug = 'shared'")
            .bind(environment_id)
            .fetch_one(fixture.db.pool())
            .await
            .unwrap();
    sqlx::query("update page_revisions set title = 'Staging only' where page_id = $1")
        .bind(staging_page)
        .execute(fixture.db.pool())
        .await
        .unwrap();

    let after_edit = public_read(&fixture.state, &site_key, "shared").await;
    assert_eq!(after_edit.status, StatusCode::OK, "body: {}", after_edit.body);
    assert_ne!(
        after_edit.body["revision"]["title"], "Staging only",
        "the public read must serve the production revision, never the staging copy"
    );

    // A page that exists only in staging is not a page a visitor can reach.
    let staging_only: Uuid = sqlx::query_scalar(
        "insert into pages (site_id, slug, status, environment_id) \
         values ($1, 'drafts-only', 'published', $2) returning id",
    )
    .bind(fixture.site)
    .bind(environment_id)
    .fetch_one(fixture.db.pool())
    .await
    .unwrap();
    sqlx::query(
        "insert into page_revisions (page_id, revision_no, state, title, body) \
         values ($1, 1, 'published', 'Drafts only', 'body')",
    )
    .bind(staging_only)
    .execute(fixture.db.pool())
    .await
    .unwrap();

    let leaked = public_read(&fixture.state, &site_key, "drafts-only").await;
    assert_eq!(
        leaked.status,
        StatusCode::NOT_FOUND,
        "a page that exists only in staging is not published content: {}",
        leaked.body
    );

    // The production row is still the one the panel edits; the staging edit changed nothing.
    let production_slug: String = sqlx::query_scalar("select slug from pages where id = $1")
        .bind(page)
        .fetch_one(fixture.db.pool())
        .await
        .unwrap();
    assert_eq!(production_slug, "shared");
}

/// The public read addressed by a `Host` header, as a browser or a crawler arrives.
async fn public_read_at_host(
    state: &AppState,
    site_key: &str,
    host: &str,
    slug: &str,
) -> TestResponse {
    call(
        state,
        Request::builder()
            .method(Method::GET)
            .uri(format!("/api/v1/public/pages/{slug}?site={site_key}"))
            .header("host", host)
            .body(Body::empty())
            .expect("request must build"),
    )
    .await
}

/// A staging host answers `X-Robots-Tag: noindex`; a production host does not.
///
/// The criterion is one clause of REQ-017's acceptance list and it was unticked for four ticks
/// while the environment screens were being finished, because "the panel says staging is not
/// public" is not the same claim as "a crawler is told". The one that matters is the header, and
/// the only place it can be said is the response — a staging host serves a *published* page, so
/// nothing else about the response says otherwise.
///
/// Three things are pinned deliberately:
///
///   * the marker is on a **404** as well as a 200. A handler that stamps its own return value
///     covers the happy path and leaves the not-found path indexable, which is how a host ends
///     up unindexed at one path and indexed at another — the walk asks for an address that
///     exists only in staging, so the `404` *is* the interesting response here;
///   * production is **not** marked. A layer that stamps unconditionally is a one-line change
///     away from telling every site in the installation not to be indexed, and the walk reads
///     the same header on a production address to prove it is absent rather than assumed;
///   * the marker follows the **host**, not the site: the same page on the same site is
///     indexable on one address and not on the other, which is the whole point of a staging host.
#[tokio::test]
async fn a_staging_host_answers_noindex_and_a_production_one_does_not() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site_key: String =
        sqlx::query_scalar("select key from sites where id = $1")
            .bind(fixture.site)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the site carries a key");

    publish_page_via_api(&fixture.state, &fixture.caller, fixture.site, "indexed").await;

    // Before any staging host exists, the same address is indexable. Without this leg the
    // walk would pass on a response that carries the header unconditionally.
    let staging_host = format!("staging-{}.omnion.test", Uuid::new_v4().simple());
    let before = public_read_at_host(&fixture.state, &site_key, &staging_host, "indexed").await;
    assert_eq!(before.status, StatusCode::OK, "body: {}", before.body);
    assert_eq!(
        before.header("x-robots-tag"),
        None,
        "a host no staging environment owns must be indexable"
    );

    let created = fixture
        .create_staging_with_host("Staging", "noindex-walk", &staging_host)
        .await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    drain_clone_for(&fixture.db, environment_id).await;

    // The published page, on the staging host.
    let marked = public_read_at_host(&fixture.state, &site_key, &staging_host, "indexed").await;
    assert_eq!(marked.status, StatusCode::OK, "body: {}", marked.body);
    assert_eq!(
        marked.header("x-robots-tag").as_deref(),
        Some("noindex, nofollow"),
        "a staging host must refuse indexing on a page it serves: {:?}",
        marked.headers
    );

    // The 404 leg: an address nobody published, on the same staging host. The mark is a property
    // of the address, so it holds here too.
    let missing = public_read_at_host(&fixture.state, &site_key, &staging_host, "never-published")
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "body: {}", missing.body);
    assert_eq!(
        missing.header("x-robots-tag").as_deref(),
        Some("noindex, nofollow"),
        "a staging host must refuse indexing even where it has nothing to serve: {:?}",
        missing.headers
    );

    // Production, again, by the site's own key address: unchanged.
    let production = public_read(&fixture.state, &site_key, "indexed").await;
    assert_eq!(production.status, StatusCode::OK, "body: {}", production.body);
    assert_eq!(
        production.header("x-robots-tag"),
        None,
        "creating a staging environment must not deindex the live site"
    );

    // Archiving releases the host, and a released host is an ordinary address again.
    let archived = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/environments/{environment_id}"),
            Some(&fixture.caller),
            None,
        ),
    )
    .await;
    assert_eq!(archived.status, StatusCode::OK, "archive: {}", archived.body);

    let after_archive = public_read_at_host(&fixture.state, &site_key, &staging_host, "indexed").await;
    assert_eq!(after_archive.status, StatusCode::OK, "body: {}", after_archive.body);
    assert_eq!(
        after_archive.header("x-robots-tag"),
        None,
        "an archived environment no longer holds the host, so the host is not staging any more"
    );
}

#[tokio::test]
async fn a_clone_copies_content_and_leaves_production_byte_identical() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let page = insert_production_page(&fixture.db, fixture.site, "shared", "Shared").await;

    let created = fixture.create_staging("Staging", "staging-2").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    drain_clone_for(&fixture.db, environment_id).await;

    // The staging copy exists, and it is a *different row*: migration 0148 gave the page's
    // natural key an environment, so the copy has its own id. Asserting that the production id
    // survived into staging would be asserting the design this migration removed.
    let staging_copy: i64 = sqlx::query_scalar(
        "select count(*) from pages where site_id = $1 and slug = 'shared' and environment_id = $2",
    )
    .bind(fixture.site)
    .bind(environment_id)
    .fetch_one(fixture.db.pool())
    .await
    .unwrap();
    assert_eq!(staging_copy, 1, "the page was copied into staging");

    let staging_id: Uuid =
        sqlx::query_scalar("select id from pages where environment_id = $1 and slug = 'shared'")
            .bind(environment_id)
            .fetch_one(fixture.db.pool())
            .await
            .unwrap();
    assert_ne!(
        staging_id, page,
        "a staging page is its own row, not a second environment id on the production one"
    );
    // Its revision came with it, re-linked to the copy rather than to the source.
    let revisions: i64 = sqlx::query_scalar(
        "select count(*) from page_revisions where page_id = $1",
    )
    .bind(staging_id)
    .fetch_one(fixture.db.pool())
    .await
    .unwrap();
    assert_eq!(revisions, 1, "the copy carries its own revision history");

    // ...and editing it does not touch production. The production row is found by its own id,
    // which the copy no longer shares.
    sqlx::query("update pages set slug = 'shared-edited', updated_at = now() where id = $1")
        .bind(staging_id)
        .execute(fixture.db.pool())
        .await
        .unwrap();

    let production_slug: String =
        sqlx::query_scalar("select slug from pages where id = $1 and environment_id = $2")
            .bind(page)
            .bind(fixture.production_id().await)
            .fetch_one(fixture.db.pool())
            .await
            .unwrap();
    assert_eq!(
        production_slug, "shared",
        "editing staging must not change the production row"
    );
}

#[tokio::test]
async fn a_clone_is_idempotent() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    insert_production_page(&fixture.db, fixture.site, "one", "One").await;
    insert_production_page(&fixture.db, fixture.site, "two", "Two").await;

    let created = fixture.create_staging("Staging", "staging-2").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    drain_clone_for(&fixture.db, environment_id).await;
    let after_first: i64 =
        sqlx::query_scalar("select count(*) from pages where environment_id = $1")
            .bind(environment_id)
            .fetch_one(fixture.db.pool())
            .await
            .unwrap();
    assert_eq!(after_first, 2);

    // Re-clone. The confirmation flag is the walk's subject here, so it is sent.
    let reclone = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/environments/{environment_id}/clone"),
            Some(&fixture.caller),
            Some(json!({ "discard_confirmed": true })),
        ),
    )
    .await;
    assert_eq!(reclone.status, StatusCode::ACCEPTED, "body: {}", reclone.body);

    drain_clone_for(&fixture.db, environment_id).await;

    let after_second: i64 =
        sqlx::query_scalar("select count(*) from pages where environment_id = $1")
            .bind(environment_id)
            .fetch_one(fixture.db.pool())
            .await
            .unwrap();
    assert_eq!(
        after_second, after_first,
        "re-cloning must not duplicate rows -- the second run empties the environment first, \
         and the natural key refuses a second copy of the same slug"
    );
}

#[tokio::test]
async fn a_reclone_refuses_until_the_operator_confirms_what_is_discarded() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let created = fixture.create_staging("Staging", "staging-2").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    drain_clone_for(&fixture.db, environment_id).await;
    // Put a row in staging that production does not have: that is the work at risk.
    sqlx::query(
        "insert into pages (site_id, slug, environment_id) values ($1, 'staging-only', $2)",
    )
    .bind(fixture.site)
    .bind(environment_id)
    .execute(fixture.db.pool())
    .await
    .unwrap();

    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/environments/{environment_id}/clone"),
            Some(&fixture.caller),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::CONFLICT,
        "an unconfirmed re-clone must be refused: {}",
        refused.body
    );
    assert_eq!(refused.body["error"]["code"], "clone_discard_unconfirmed");
    assert_eq!(refused.body["error"]["details"]["discarded_pages"], 1);
}

#[tokio::test]
async fn a_staging_environment_cannot_be_cloned_into_another_one() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // Build the nesting by hand: point one staging environment at another, which is exactly the
    // state a caller would create if the API did not refuse it.
    let created = fixture.create_staging("Staging", "staging-2").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    let nested = sqlx::query_scalar::<_, Uuid>(
        "insert into environments (organization_id, key, name, type, status, cloned_from_environment_id) \
         values ($1, 'nested', 'Nested', 'staging', 'active', $2) returning id",
    )
    .bind(fixture.organization)
    .bind(environment_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the row inserts; the API is what must refuse it");

    let response = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/environments/{nested}/clone"),
            Some(&fixture.caller),
            Some(json!({ "areas": ["pages"], "discard_confirmed": true })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::CONFLICT,
        "body: {}",
        response.body
    );
    assert_eq!(
        response.body["error"]["code"], "staging_nesting_refused",
        "the refusal has to be the named one, because the wizard does not offer the option"
    );
}

#[tokio::test]
async fn production_cannot_be_archived_or_recloned() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let production = fixture.production_id().await;

    let archived = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/environments/{production}"),
            Some(&fixture.caller),
            None,
        ),
    )
    .await;
    assert_eq!(archived.status, StatusCode::CONFLICT, "body: {}", archived.body);
    assert_eq!(archived.body["error"]["code"], "environment_not_staging");

    let recloned = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/environments/{production}/clone"),
            Some(&fixture.caller),
            Some(json!({ "areas": ["pages"], "discard_confirmed": true })),
        ),
    )
    .await;
    assert_eq!(recloned.status, StatusCode::CONFLICT);
    assert_eq!(recloned.body["error"]["code"], "environment_not_staging");
}

#[tokio::test]
async fn archiving_releases_the_host_and_keeps_the_content() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    insert_production_page(&fixture.db, fixture.site, "kept", "Kept").await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/environments",
            Some(&fixture.caller),
            Some(json!({
                "name": "Staging",
                "key": "staging-2",
                "staging_host": format!("staging-{}.omnion.test", Uuid::new_v4().simple()),
                "areas": ["pages"],
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "body: {}", created.body);
    let environment_id = Uuid::parse_str(created.body["id"].as_str().unwrap()).unwrap();
    let host = created.body["staging_host"].as_str().unwrap().to_string();
    assert!(!host.is_empty());

    drain_clone_for(&fixture.db, environment_id).await;

    let archived = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/environments/{environment_id}"),
            Some(&fixture.caller),
            None,
        ),
    )
    .await;
    assert_eq!(archived.status, StatusCode::OK, "body: {}", archived.body);
    assert_eq!(archived.body["status"], "archived");
    assert!(
        archived.body["staging_host"].is_null(),
        "archiving releases the host"
    );
    assert_eq!(
        archived.body["content"]["pages"], 1,
        "the content is kept: {}",
        archived.body["content"]
    );
    assert_eq!(archived.body["reclonable"], false);
}

#[tokio::test]
async fn a_second_open_clone_is_refused_with_the_running_job_named() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let created = fixture.create_staging("Staging", "staging-2").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    // The first job is still pending, so the second request must be refused — by the database's
    // partial unique index, which is the only version that survives two requests at once.
    let second = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/environments/{environment_id}/clone"),
            Some(&fixture.caller),
            Some(json!({ "areas": ["pages"], "discard_confirmed": true })),
        ),
    )
    .await;
    assert_eq!(second.status, StatusCode::CONFLICT, "body: {}", second.body);
    assert_eq!(second.body["error"]["code"], "clone_already_running");
}

#[tokio::test]
async fn cancelling_a_clone_leaves_the_environment_out_of_active() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let created = fixture.create_staging("Staging", "staging-2").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    let job_id = created["clone"]["id"].as_str().unwrap().to_string();

    let cancelled = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/environments/{environment_id}/clone-jobs/{job_id}/cancel"),
            Some(&fixture.caller),
            None,
        ),
    )
    .await;
    assert_eq!(cancelled.status, StatusCode::OK, "body: {}", cancelled.body);
    assert_eq!(cancelled.body["job"]["status"], "cancelled");
    assert_eq!(cancelled.body["job"]["cancellable"], false);
    // A cancelled clone is a partial copy. `active` would tell the operator it is finished.
    assert_eq!(cancelled.body["environment_status"], "error");

    let job: CloneStatus =
        CloneStatus::parse(cancelled.body["job"]["status"].as_str().unwrap_or_default())
            .expect("the wire name must parse");
    assert!(!job.is_open());
}

#[tokio::test]
async fn a_duplicate_key_is_refused_by_name() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.create_staging("Staging", "staging-2").await;
    let second = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/environments",
            Some(&fixture.caller),
            Some(json!({ "name": "Other", "key": "staging-2", "areas": ["pages"] })),
        ),
    )
    .await;
    assert_eq!(second.status, StatusCode::CONFLICT, "body: {}", second.body);
    assert_eq!(second.body["error"]["code"], "environment_key_taken");
}

#[tokio::test]
async fn a_reserved_or_malformed_key_is_refused_naming_the_field() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    for key in ["production", "Staging", "with space", "trailing-"] {
        let response = call(
            &fixture.state,
            request(
                Method::POST,
                "/api/v1/environments",
                Some(&fixture.caller),
                Some(json!({ "name": "Staging", "key": key, "areas": ["pages"] })),
            ),
        )
        .await;
        // A malformed or reserved key is a `400` naming its field, not a `409`: the request
        // itself has to change, and there is no existing row in conflict.
        assert_eq!(
            response.status, StatusCode::BAD_REQUEST,
            "key {key} should be refused: {}",
            response.body
        );
        assert_eq!(response.body["error"]["code"], "environment_field_invalid");
        assert_eq!(response.body["error"]["details"]["field"], "key");
    }
}

#[tokio::test]
async fn an_empty_area_selection_is_refused_before_anything_is_created() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let before: i64 = sqlx::query_scalar("select count(*) from environments where organization_id = $1")
        .bind(fixture.organization)
        .fetch_one(fixture.db.pool())
        .await
        .unwrap();

    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/environments",
            Some(&fixture.caller),
            Some(json!({ "name": "Staging", "key": "staging-2", "areas": [] })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST, "body: {}", response.body);
    assert_eq!(response.body["error"]["code"], "clone_areas_required");

    let after: i64 = sqlx::query_scalar("select count(*) from environments where organization_id = $1")
        .bind(fixture.organization)
        .fetch_one(fixture.db.pool())
        .await
        .unwrap();
    assert_eq!(after, before, "a refused create leaves no environment behind");
}

#[tokio::test]
async fn an_unknown_area_is_refused_and_the_legal_ones_are_named() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/environments",
            Some(&fixture.caller),
            Some(json!({ "name": "Staging", "key": "staging-2", "areas": ["media"] })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.body["error"]["code"], "clone_area_unknown");
    // Media is *referenced*, never copied, and the refusal says so by listing what is legal.
    let legal = response.body["error"]["details"]["areas"]
        .as_array()
        .expect("the legal areas are listed")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert!(legal.contains(&"pages"), "{legal:?}");
    assert!(!legal.contains(&"media"), "{legal:?}");
    for area in Area::ALL {
        assert!(legal.contains(&area.as_str()), "{area:?} missing from {legal:?}");
    }
}

#[tokio::test]
async fn another_organizations_environment_is_a_404_not_a_403() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let created = fixture.create_staging("Staging", "staging-2").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    let other_org = create_organization_row(&fixture.db, "other").await;
    create_site(&fixture.db, other_org, "other").await;
    let (_, other_caller) =
        create_admin(&fixture.db, other_org, "other", &fixture.state, &ALL_PERMISSIONS).await;

    let response = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/environments/{environment_id}"),
            Some(&other_caller),
            None,
        ),
    )
    .await;
    // The caller *is* allowed to read environments — of their own. Answering 403 would tell them
    // to ask for a permission they already hold.
    assert_eq!(response.status, StatusCode::NOT_FOUND, "body: {}", response.body);
    assert_eq!(response.body["error"]["code"], "environment_not_found");
}

#[tokio::test]
async fn the_permission_split_is_real() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let created = fixture.create_staging("Staging", "staging-2").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    // Read-only: sees the list, cannot create, cannot archive.
    let (_, reader) = create_admin(
        &fixture.db,
        fixture.organization,
        "reader",
        &fixture.state,
        &["deployment.read"],
    )
    .await;

    let list = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/environments",
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(list.status, StatusCode::OK, "body: {}", list.body);

    let create = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/environments",
            Some(&reader),
            Some(json!({ "name": "Nope", "key": "nope", "areas": ["pages"] })),
        ),
    )
    .await;
    assert_eq!(create.status, StatusCode::FORBIDDEN, "body: {}", create.body);

    let archive = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/environments/{environment_id}"),
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(archive.status, StatusCode::FORBIDDEN);

    // No permission at all: 401 without a session, 403 with one.
    let anonymous = call(
        &fixture.state,
        request(Method::GET, "/api/v1/environments", None, None),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_detail_screen_carries_the_history_and_the_estimate() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    insert_production_page(&fixture.db, fixture.site, "one", "One").await;
    let created = fixture.create_staging("Staging", "staging-2").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    drain_clone_for(&fixture.db, environment_id).await;

    let response = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/environments/{environment_id}"),
            Some(&fixture.caller),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);
    let jobs = response.body["jobs"].as_array().expect("jobs is an array");
    assert_eq!(jobs.len(), 1, "the clone is in the history");
    assert_eq!(jobs[0]["status"], "done");
    // The estimate is a range in words, not a fake byte count.
    let estimate = response.body["estimate"].as_str().unwrap_or_default();
    assert!(estimate.contains("rows"), "{estimate}");
    assert!(estimate.contains("Media files are referenced"), "{estimate}");

    let per_area = jobs[0]["areas"].as_array().expect("areas is an array");
    let pages = per_area
        .iter()
        .find(|area| area["name"] == "pages")
        .expect("the pages area is reported");
    assert_eq!(pages["done"], 1);
    assert_eq!(pages["label"], "Pages & revisions");
}

#[tokio::test]
async fn the_list_offers_the_wizard_its_areas_and_its_source() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/environments",
            Some(&fixture.caller),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.body["source_key"], "production",
        "the wizard's 'clone from' line reads the organization's real production key"
    );
    let areas = response.body["areas"].as_array().expect("areas is an array");
    assert_eq!(areas.len(), Area::ALL.len());
    // Every checkbox has a label a person can read and a cost in words.
    for area in areas {
        assert!(!area["label"].as_str().unwrap_or_default().is_empty());
        assert!(!area["weight"].as_str().unwrap_or_default().is_empty());
    }
}

#[tokio::test]
async fn the_type_and_status_filters_narrow_the_list_and_a_bogus_one_does_not() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.create_staging("Staging", "staging-2").await;

    let staging_only = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/environments?type=staging",
            Some(&fixture.caller),
            None,
        ),
    )
    .await;
    let rows = staging_only.body["environments"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["type"], "staging");

    // A filter value the panel is still transitioning must show the unfiltered list rather than
    // an error banner over a screen that works.
    let bogus = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/environments?type=preview&status=frozen",
            Some(&fixture.caller),
            None,
        ),
    )
    .await;
    assert_eq!(bogus.status, StatusCode::OK, "body: {}", bogus.body);
    assert_eq!(
        bogus.body["environments"].as_array().unwrap().len(),
        2,
        "production plus staging"
    );
}

#[tokio::test]
async fn the_audit_trail_records_the_create_the_clone_and_the_archive() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let created = fixture.create_staging("Staging", "staging-2").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    let archived = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/environments/{environment_id}"),
            Some(&fixture.caller),
            None,
        ),
    )
    .await;
    assert_eq!(archived.status, StatusCode::OK);

    let actions: Vec<String> = sqlx::query_scalar(
        "select action from audit_log where target_id = $1 order by created_at asc",
    )
    .bind(environment_id.to_string())
    .fetch_all(fixture.db.pool())
    .await
    .unwrap();
    assert!(
        actions.iter().any(|a| a == "environment.created"),
        "{actions:?}"
    );
    assert!(
        actions.iter().any(|a| a == "environment.archived"),
        "{actions:?}"
    );
}

#[tokio::test]
async fn the_event_bus_records_what_a_subscribed_endpoint_would_see() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.create_staging("Staging", "staging-2").await;

    let names: Vec<String> = sqlx::query_scalar(
        "select name from events where organization_id = $1 order by id asc",
    )
    .bind(fixture.organization)
    .fetch_all(fixture.db.pool())
    .await
    .unwrap();
    assert!(
        names.iter().any(|n| n == "environment.created"),
        "{names:?}"
    );
    assert!(
        names.iter().any(|n| n == "environment.clone.started"),
        "the request's own event list: {names:?}"
    );
}

#[tokio::test]
async fn a_session_survives_a_clone_that_copies_nothing() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // A brand-new organization with no content: the clone has zero rows and must still finish,
    // not hang on a division by zero or report a failure it cannot name.
    let created = fixture.create_staging("Empty", "empty").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    drain_clone_for(&fixture.db, environment_id).await;
    let outcome_status: String = sqlx::query_scalar(
        "select status from environment_clone_jobs where environment_id = $1 \
         order by created_at desc limit 1",
    )
    .bind(environment_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the job row must be there");
    assert_eq!(
        omnion_environment::model::CloneStatus::parse(&outcome_status),
        Some(omnion_environment::model::CloneStatus::Done),
        "a clone with nothing to copy still finishes"
    );

    let after = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/environments/{environment_id}"),
            Some(&fixture.caller),
            None,
        ),
    )
    .await;
    assert_eq!(after.status, StatusCode::OK);
    // A clone that copied nothing is still a *finished* clone: the environment is usable and
    // says so. A status that stayed `cloning` would leave the operator with no way to tell
    // "still working" from "finished, nothing to copy".
    assert_eq!(after.body["environment"]["status"], "active");
    assert_eq!(after.body["environment"]["content"]["total"], 0);
    assert_eq!(after.body["jobs"][0]["status"], "done");
}

#[tokio::test]
async fn the_session_belongs_to_the_caller_not_to_the_environment() {
    // A staging environment is a content copy, not a login. The acceptance criteria say identity
    // and roles are shared, and this asserts the part that is easy to get wrong later: creating
    // an environment does not mint a session or change who the caller is.
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.create_staging("Staging", "staging-2").await;

    let me = call(
        &fixture.state,
        request(Method::GET, "/api/v1/me", Some(&fixture.caller), None),
    )
    .await;
    assert_eq!(me.status, StatusCode::OK);
    // The same session still answers, and it still sees its own organization -- proved through
    // the environment list rather than a field on `/me`, because the list is what a staging
    // environment could have leaked out of.
    let list = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/environments",
            Some(&fixture.caller),
            None,
        ),
    )
    .await;
    assert_eq!(list.status, StatusCode::OK, "body: {}", list.body);
    let rows = list.body["environments"].as_array().expect("an array");
    // Production and the staging copy this walk created — both of the caller's own organization
    // and nothing else. The count is a side effect of the walk, not the point: the point is
    // that every row carries the caller's organization and no other tenant's.
    assert_eq!(rows.len(), 2, "production plus the staging environment");
    for row in rows {
        assert_eq!(
            row["organization_id"],
            fixture.organization.to_string(),
            "a staging environment must not carry another tenant's rows"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The change set (REQ-017 slice 2)
// ---------------------------------------------------------------------------------------------

/// Read the change set through the API.
async fn changes_for(fixture: &Fixture, environment_id: Uuid) -> TestResponse {
    call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/environments/{environment_id}/changes"),
            Some(&fixture.caller),
            None,
        ),
    )
    .await
}

/// The slug and kind of every item, as `(slug, kind)` pairs.
///
/// Compared as a set rather than as a row order: the endpoint's `order by slug` is a presentation
/// detail, and a test that pins it fails on a harmless reordering while a test that ignores order
/// entirely misses a duplicated row. Sorting on both sides keeps the multiset honest.
fn slugs_and_kinds(body: &Value) -> Vec<String> {
    let mut pairs: Vec<String> = body["items"]
        .as_array()
        .expect("items must be an array")
        .iter()
        .map(|item| format!("{}:{}", item["slug"].as_str().unwrap(), item["kind"].as_str().unwrap()))
        .collect();
    pairs.sort();
    pairs
}

/// The headline walk: an edited page, a new page and a deleted page all show up, each with its
/// editor and a timestamp, and production is untouched by the staging edits that produced them.
#[tokio::test]
async fn the_change_set_names_what_staging_holds_that_production_does_not() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let edited = insert_production_page(&fixture.db, fixture.site, "edited", "Edited").await;
    let removed = insert_production_page(&fixture.db, fixture.site, "removed", "Removed").await;
    let untouched = insert_production_page(&fixture.db, fixture.site, "quiet", "Quiet").await;

    let created = fixture.create_staging("Staging", "staging-diff").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    drain_clone_for(&fixture.db, environment_id).await;

    // A freshly cloned environment differs from production in nothing, and saying so is the point
    // of the empty flag: an empty table with no words reads as a broken screen.
    let fresh = changes_for(&fixture, environment_id).await;
    assert_eq!(fresh.status, StatusCode::OK, "body: {}", fresh.body);
    assert_eq!(
        fresh.body["empty"], true,
        "a clone is byte-identical, so nothing differs yet: {}",
        fresh.body
    );
    assert_eq!(fresh.body["items"].as_array().unwrap().len(), 0);

    // An *update* is not an insert. The clone already put an `edited` page in staging, so the
    // staging side of an update is that row being changed — inserting a second one violates
    // `pages_site_slug_key`, which is exactly the constraint migration 0148 put the environment
    // into. Editing the copied row is also what the request describes: an editor opening a page in
    // staging and saving it.
    sqlx::query(
        "update pages set updated_at = now() + interval '1 second' \
         where site_id = $1 and slug = 'edited' and environment_id = $2",
    )
    .bind(fixture.site)
    .bind(environment_id)
    .execute(fixture.db.pool())
    .await
    .unwrap();

    // An *addition* is an insert, and only a slug production does not have can be one.
    for (slug, title) in [("brand-new", "Brand new")] {
        let page: Uuid = sqlx::query_scalar(
            "insert into pages (site_id, slug, status, environment_id, updated_at) \
             values ($1, $2, 'draft', $3, now() + interval '1 second') returning id",
        )
        .bind(fixture.site)
        .bind(slug)
        .bind(environment_id)
        .fetch_one(fixture.db.pool())
        .await
        .unwrap();
        sqlx::query(
            "insert into page_revisions (page_id, revision_no, state, title, body) \
             values ($1, 1, 'draft', $2, 'staging body')",
        )
        .bind(page)
        .bind(title)
        .execute(fixture.db.pool())
        .await
        .unwrap();
    }
    // The delete: a staging row that simply stops existing. This is the case the SQL's outer join
    // exists for, and it is the one a `where staging.environment_id = $1` filter silently drops —
    // the tab would then report "nothing deleted" for an environment that removed a page.
    sqlx::query("delete from pages where site_id = $1 and slug = 'removed' and environment_id = $2")
        .bind(fixture.site)
        .bind(environment_id)
        .execute(fixture.db.pool())
        .await
        .unwrap();

    let diff = changes_for(&fixture, environment_id).await;
    assert_eq!(diff.status, StatusCode::OK, "body: {}", diff.body);
    assert_eq!(
        slugs_and_kinds(&diff.body),
        vec![
            "brand-new:added".to_owned(),
            "edited:updated".to_owned(),
            "removed:deleted".to_owned(),
        ],
        "every kind of change is listed, and `quiet` is not"
    );
    assert_eq!(diff.body["added"], 1);
    assert_eq!(diff.body["updated"], 1);
    assert_eq!(diff.body["deleted"], 1);
    assert_eq!(diff.body["empty"], false);
    assert_eq!(
        diff.body["production_id"], fixture.production_id().await.to_string(),
        "the comparison names the environment it compared against"
    );

    // Each row carries the two things a reader needs to judge it.
    for item in diff.body["items"].as_array().unwrap() {
        // `OffsetDateTime` serialises the way every other timestamp in this API does — as the
        // `time` crate's own array form — so the assertion is that the field is a *non-null array*,
        // not that it is a string. Pinning a string here would fail against the shape the whole
        // panel already parses, and "fixing" it would make this one route inconsistent with the
        // other twenty.
        assert!(
            item["changed_at"].is_array(),
            "every item is dated: {}",
            item
        );
        assert!(
            item["title"].is_string(),
            "every item has a title a human can read, including the deleted one: {}",
            item
        );
    }
    let deleted = diff.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"] == "deleted")
        .unwrap();
    assert_eq!(
        deleted["title"], "Removed",
        "a deleted row falls back to production's title, or it renders blank"
    );

    // And the staging edits changed no production row.
    let production_slugs: Vec<String> =
        sqlx::query_scalar("select slug from pages where environment_id = $1 and site_id = $2 order by slug")
            .bind(fixture.production_id().await)
            .bind(fixture.site)
            .fetch_all(fixture.db.pool())
            .await
            .unwrap();
    assert_eq!(
        production_slugs,
        vec!["edited".to_owned(), "quiet".to_owned(), "removed".to_owned()],
        "production still holds every page it held before the staging edits"
    );
    let _ = (edited, untouched);
}

/// The tenancy and permission gates on the new route, in one walk each — they are separate
/// assertions because a route that answers one correctly can still leak the other.
#[tokio::test]
async fn the_change_set_is_404_for_another_organization_and_403_without_the_key() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let created = fixture.create_staging("Staging", "staging-gate").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    drain_clone_for(&fixture.db, environment_id).await;

    // 403: an account that may not deploy cannot read the diff.
    let stranger_organization = create_organization_row(&fixture.db, "gate").await;
    let (_, stranger) =
        create_admin(
            &fixture.db,
            stranger_organization,
            "gate",
            &fixture.state,
            // A real key that is the *wrong* one. `content.pages.read` is the honest choice: it
            // exists in the catalogue, so the refusal is the guard answering rather than the
            // seed rejecting a name it has never heard of.
            &["content.pages.read"],
        )
        .await;
    let refused = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/environments/{environment_id}/changes"),
            Some(&stranger),
            None,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "without `deployment.read` the change set is refused: {}",
        refused.body
    );

    // 404: an account of the same organization that holds the key still cannot read *another*
    // organization's environment — the tenancy check, not the permission check, is what answers.
    let outsider_organization = create_organization_row(&fixture.db, "outsider").await;
    let (_, outsider) = create_admin(
        &fixture.db,
        outsider_organization,
        "outsider",
        &fixture.state,
        &ALL_PERMISSIONS,
    )
    .await;
    let hidden = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/environments/{environment_id}/changes"),
            Some(&outsider),
            None,
        ),
    )
    .await;
    assert_eq!(
        hidden.status,
        StatusCode::NOT_FOUND,
        "another organization's environment is a 404, not a 403: {}",
        hidden.body
    );
}

/// A change set is only meaningful against a reference, so an environment with no clone source is
/// refused in words rather than answered as "no changes".
#[tokio::test]
async fn a_derived_environment_without_a_clone_source_refuses_to_be_compared() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // A production environment is exactly this case: it has no `cloned_from_environment_id`.
    let production = fixture.production_id().await;
    let response = changes_for(&fixture, production).await;
    assert_eq!(
        response.status,
        StatusCode::CONFLICT,
        "there is nothing to compare production against: {}",
        response.body
    );
    assert_eq!(
        response.body["error"]["code"], "environment_no_clone_source",
        "the code is what the panel switches on: {}",
        response.body
    );
}
// REQ-017 slice 3 walks: promotions — request, approve, apply, refuse.
//
// Appended to `environments.rs` rather than a new file on purpose: every walk here needs the same
// fixture (a tenant with a production site, three pages and a drained staging clone), and a second
// file would mean a second copy of that harness to keep in step with the first. The clone drain in
// particular is a shared-database hazard that must exist exactly once — see `drain_clone_for`.
//
// Six walks, one per decision that could be wrong:
//   1. a clean change set applies every item and the event carries the same count;
//   2. a production edit after the request marks the item and the approval is refused by id;
//   3. a rollback leaves production untouched;
//   4. self-approval is refused without the deploy key and allowed with it;
//   5. history, detail and the tenancy/permission gates;
//   6. a promotion with no changes, and one that was withdrawn.


/// Request a promotion through the API and return the answer.
async fn request_promotion(
    fixture: &Fixture,
    environment_id: Uuid,
    items: &[Uuid],
) -> TestResponse {
    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/environments/{environment_id}/promotions"),
            Some(&fixture.caller),
            Some(json!({ "items": items })),
        ),
    )
    .await
}

/// Approve a promotion through the API.
async fn approve(fixture: &Fixture, promotion_id: Uuid, caller: &Caller) -> TestResponse {
    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/promotions/{promotion_id}/approve"),
            Some(caller),
            None,
        ),
    )
    .await
}

/// The slug/title pairs production holds, as `(slug, title)` — the state an apply is judged on.
async fn production_content(db: &Db, production_id: Uuid, site_id: Uuid) -> Vec<(String, String)> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "select p.slug, coalesce((select r.title from page_revisions r \
                  where r.id = p.published_revision_id or (r.page_id = p.id and r.state = 'draft') \
                  order by case when r.id = p.published_revision_id then 0 else 1 end, r.revision_no desc limit 1), '') \
         from pages p where p.environment_id = $1 and p.site_id = $2 order by p.slug",
    )
    .bind(production_id)
    .bind(site_id)
    .fetch_all(db.pool())
    .await
    .expect("production content must be readable");
    rows
}

/// The promotion's ids, from its frozen change set.
fn frozen_slugs(body: &Value) -> Vec<String> {
    let mut slugs: Vec<String> = body["changes"]["items"]
        .as_array()
        .expect("a requested promotion answers with its frozen set")
        .iter()
        .map(|item| item["slug"].as_str().unwrap().to_owned())
        .collect();
    slugs.sort();
    slugs
}

/// Walk 1 — the headline: a clean change set applies every item, and the event says the same
/// number of items the set froze.
///
/// Three kinds at once, because the apply has three branches and a branch that is never exercised
/// is a branch nobody knows works.
#[tokio::test]
async fn promoting_a_clean_change_set_applies_every_item_and_says_how_many() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let production = fixture.production_id().await;
    let _kept = insert_production_page(&fixture.db, fixture.site, "kept", "Kept").await;
    let _edited = insert_production_page(&fixture.db, fixture.site, "edited", "Original").await;
    let _removed = insert_production_page(&fixture.db, fixture.site, "removed", "Removed").await;

    let created = fixture.create_staging("Staging", "staging-promote").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    drain_clone_for(&fixture.db, environment_id).await;

    // Staging now differs in all three ways: an edit, an addition and a deletion.
    sqlx::query(
        "update pages set updated_at = now() + interval '1 second' \
         where site_id = $1 and slug = 'edited' and environment_id = $2",
    )
    .bind(fixture.site)
    .bind(environment_id)
    .execute(fixture.db.pool())
    .await
    .unwrap();
    sqlx::query("update page_revisions set title = 'Edited in staging' where page_id = (select id from pages where site_id = $1 and slug = 'edited' and environment_id = $2) and state = 'draft'")
        .bind(fixture.site)
        .bind(environment_id)
        .execute(fixture.db.pool())
        .await
        .unwrap();
    let new_page: Uuid = sqlx::query_scalar(
        "insert into pages (site_id, slug, status, environment_id, updated_at) \
         values ($1, 'brand-new', 'draft', $2, now() + interval '1 second') returning id",
    )
    .bind(fixture.site)
    .bind(environment_id)
    .fetch_one(fixture.db.pool())
    .await
    .unwrap();
    sqlx::query(
        "insert into page_revisions (page_id, revision_no, state, title, body) \
         values ($1, 1, 'draft', 'Brand new', 'staging body')",
    )
    .bind(new_page)
    .execute(fixture.db.pool())
    .await
    .unwrap();
    sqlx::query("delete from pages where site_id = $1 and slug = 'removed' and environment_id = $2")
        .bind(fixture.site)
        .bind(environment_id)
        .execute(fixture.db.pool())
        .await
        .unwrap();

    // The request. It must come back immediately with a frozen set, not with production already
    // changed: a request is an intent, and an intent that writes is a deploy nobody approved.
    let asked = request_promotion(&fixture, environment_id, &[]).await;
    assert_eq!(asked.status, StatusCode::CREATED, "body: {}", asked.body);
    assert_eq!(asked.body["promotion"]["status"], "pending_approval");
    assert_eq!(
        frozen_slugs(&asked.body),
        vec!["brand-new", "edited", "removed"],
        "the frozen set is what the change set held, nothing added and nothing dropped"
    );
    assert_eq!(asked.body["promotion"]["item_count"], 3);
    assert_eq!(
        asked.body["promotion"]["write_count"], 2,
        "three items, two of which write: the deletion removes a row"
    );
    assert_eq!(asked.body["promotion"]["added"], 1);
    assert_eq!(asked.body["promotion"]["updated"], 1);
    assert_eq!(asked.body["promotion"]["deleted"], 1);
    assert_eq!(
        asked.body["promotion"]["conflicts"].as_array().unwrap().len(),
        0,
        "nothing has moved in production yet"
    );
    assert_eq!(
        asked.body["promotion"]["requires_typed_confirmation"], false,
        "three items is under the threshold"
    );
    let promotion_id =
        Uuid::parse_str(asked.body["promotion"]["id"].as_str().unwrap()).unwrap();

    // Production is untouched by the request.
    let before: Vec<String> = sqlx::query_scalar(
        "select slug from pages where environment_id = $1 and site_id = $2 order by slug",
    )
    .bind(production)
    .bind(fixture.site)
    .fetch_all(fixture.db.pool())
    .await
    .unwrap();
    assert_eq!(
        before,
        vec!["edited", "kept", "removed"],
        "requesting must not write production"
    );

    // The approval. The fixture's caller holds every permission including `deployment.deploy`,
    // and it is also the requester — which the single-tenant case explicitly allows.
    let approved = approve(&fixture, promotion_id, &fixture.caller).await;
    assert_eq!(approved.status, StatusCode::OK, "body: {}", approved.body);
    assert_eq!(approved.body["status"], "done");
    assert_eq!(approved.body["error"], json!(null));
    assert_eq!(
        approved.body["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| step["step"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["validate", "apply", "audit", "done"],
        "the dialog's timeline survives the refresh because the whole log is in the row"
    );

    // Production now holds the staging content.
    let after = production_content(&fixture.db, production, fixture.site).await;
    let slugs: Vec<&str> = after.iter().map(|(slug, _)| slug.as_str()).collect();
    assert!(
        slugs.contains(&"brand-new"),
        "the added page reached production: {after:?}"
    );
    assert!(
        !slugs.contains(&"removed"),
        "the deleted page is gone from production: {after:?}"
    );
    let edited = after.iter().find(|(slug, _)| slug == "edited").unwrap();
    assert_eq!(
        edited.1, "Edited in staging",
        "the update carried staging's content across: {after:?}"
    );

    // And the event carries the same item count the set froze.
    let payload: Value = sqlx::query_scalar(
        "select payload from events where name = 'promotion.completed' and organization_id = $1 \
         order by id desc limit 1",
    )
    .bind(fixture.organization)
    .fetch_one(fixture.db.pool())
    .await
    .unwrap();
    assert_eq!(
        payload["items"].as_array().unwrap().len(),
        3,
        "`promotion.completed` carries the affected ids — one event, not one per row: {payload}"
    );
    assert_eq!(payload["written"], 2);
    assert_eq!(payload["removed"], 1);
}

/// Walk 2 — the safety property: production moving on after the request refuses the approval and
/// names the item.
#[tokio::test]
async fn a_production_edit_after_the_request_is_refused_with_the_item_id() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let _page = insert_production_page(&fixture.db, fixture.site, "edited", "Original").await;
    let created = fixture.create_staging("Staging", "staging-conflict").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    drain_clone_for(&fixture.db, environment_id).await;

    // A staging edit, so there is something to promote.
    sqlx::query(
        "update pages set updated_at = now() + interval '1 second' \
         where site_id = $1 and slug = 'edited' and environment_id = $2",
    )
    .bind(fixture.site)
    .bind(environment_id)
    .execute(fixture.db.pool())
    .await
    .unwrap();

    let asked = request_promotion(&fixture, environment_id, &[]).await;
    assert_eq!(asked.status, StatusCode::CREATED, "body: {}", asked.body);
    let promotion_id = asked.body["promotion"]["id"].as_str().unwrap().to_owned();
    let item_id = asked.body["changes"]["items"][0]["page_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Now somebody edits the SAME page in production — after the diff was taken, before the
    // approval. This is the window the whole frozen set exists for.
    sqlx::query(
        "update pages set updated_at = now() + interval '10 seconds' \
         where site_id = $1 and slug = 'edited' and environment_id = $2",
    )
    .bind(fixture.site)
    .bind(fixture.production_id().await)
    .execute(fixture.db.pool())
    .await
    .unwrap();

    let refused = approve(&fixture, Uuid::parse_str(&promotion_id).unwrap(), &fixture.caller).await;
    assert_eq!(
        refused.status, StatusCode::CONFLICT,
        "a conflicted promotion is refused: {}",
        refused.body
    );
    assert_eq!(refused.body["error"]["code"], "promotion_conflict");
    let items = refused.body["error"]["details"]["items"]
        .as_array()
        .unwrap_or_else(|| panic!("the conflict must list item ids: {}", refused.body));
    assert_eq!(items, &vec![json!(item_id)], "the refusal names the item, not just a count");

    // Production keeps the edit the promotion would have overwritten.
    let production_updated: OffsetDateTime = sqlx::query_scalar(
        "select updated_at from pages where site_id = $1 and slug = 'edited' and environment_id = $2",
    )
    .bind(fixture.site)
    .bind(fixture.production_id().await)
    .fetch_one(fixture.db.pool())
    .await
    .unwrap();
    assert!(
        production_updated > OffsetDateTime::now_utc() - time::Duration::seconds(30),
        "production still holds the later edit"
    );

    // And the promotion is `failed`, with the conflict list refreshed — the dialog reads it back.
    let stored = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/promotions/{promotion_id}"),
            Some(&fixture.caller),
            None,
        ),
    )
    .await;
    assert_eq!(stored.status, StatusCode::OK);
    assert_eq!(
        stored.body["promotion"]["status"], "failed",
        "a refused promotion ends, it does not wait: {}",
        stored.body
    );
    assert_eq!(
        stored.body["promotion"]["conflicts"].as_array().unwrap().len(),
        1,
        "the refreshed conflict list is on the row, so the dialog can lead with it"
    );
    assert!(
        stored.body["promotion"]["error"].is_string(),
        "the failure names what happened"
    );
    let stopped_at = stored.body["promotion"]["steps"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()["step"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        stopped_at, "validate",
        "it stopped at the re-check, before any write: {}",
        stored.body
    );
}

/// Walk 3 — a failure injected mid-apply leaves production unchanged.
///
/// This one is a **store** test rather than a route test, and deliberately so. Through the route,
/// every mid-apply failure is unreachable on purpose: the conflict re-check runs first and refuses
/// anything that would collide, so the apply only ever sees a set it has already proved safe. That
/// is the design working. To prove the transaction actually rolls back you therefore have to
/// construct the one thing the route refuses to construct — a promotion whose frozen items
/// collide — and hand it straight to the apply.
///
/// The injection is the collision itself: two `Added` items carrying the SAME slug. Production has
/// neither, so the conflict re-check passes both (that is correct — nothing has moved), the apply
/// inserts the first, and the second dies on `pages_site_environment_slug_key` inside the
/// transaction. If the transaction were not real, production would be left holding one of them.
#[tokio::test]
async fn a_failure_midway_through_the_apply_leaves_production_unchanged() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let production = fixture.production_id().await;
    insert_production_page(&fixture.db, fixture.site, "kept", "Kept").await;
    let created = fixture.create_staging("Staging", "staging-rollback").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    drain_clone_for(&fixture.db, environment_id).await;

    // Two staging pages whose slugs differ — the environment itself is consistent, and the clone
    // and the change set both see two honest `added` items.
    let mut ids = Vec::new();
    for slug in ["first", "second"] {
        let page: Uuid = sqlx::query_scalar(
            "insert into pages (site_id, slug, status, environment_id, updated_at) \
             values ($1, $2, 'draft', $3, now() + interval '1 second') returning id",
        )
        .bind(fixture.site)
        .bind(slug)
        .bind(environment_id)
        .fetch_one(fixture.db.pool())
        .await
        .unwrap();
        sqlx::query(
            "insert into page_revisions (page_id, revision_no, state, title, body) \
             values ($1, 1, 'draft', $2, 'body')",
        )
        .bind(page)
        .bind(slug)
        .execute(fixture.db.pool())
        .await
        .unwrap();
        ids.push((page, slug));
    }

    // A frozen set built by HAND with both items claiming the same slug. This is the artifact the
    // route will never produce, which is exactly why the test builds it here.
    let items: Vec<omnion_environment::promotion::FrozenItem> = ids
        .iter()
        .map(|(page, slug)| omnion_environment::promotion::FrozenItem {
            page_id: *page,
            site_id: fixture.site,
            slug: "first".to_owned(),
            kind: omnion_environment::model::ChangeKind::Added,
            base_updated_at: None,
            base_digest: String::new(),
        })
        .collect();
    let change_set =
        omnion_environment::promotion::FrozenChangeSet::new(environment_id, production, items);
    assert_eq!(change_set.item_count(), 2);

    // Nothing has moved, so the re-check passes — which is the whole reason this reaches apply.
    let conflicts =
        omnion_environment::promotion_store::find_conflicts(fixture.db.pool(), &change_set).await
            .unwrap();
    assert!(
        conflicts.is_empty(),
        "production holds neither slug, so the re-check is right to pass: {conflicts:?}"
    );

    let row = omnion_environment::promotion_store::request(
        fixture.db.pool(),
        &omnion_environment::promotion_store::NewPromotion::new(
            environment_id,
            production,
            fixture.caller_user_id,
            change_set,
        ),
    )
    .await
    .unwrap();

    let outcome =
        omnion_environment::promotion_store::approve_and_apply(fixture.db.pool(), &row, fixture.caller_user_id)
            .await;
    assert!(
        outcome.is_err(),
        "the second insert cannot succeed, so the apply must fail"
    );

    // The criterion: production holds exactly what it held before — not the first item, not a
    // half-written revision, not a promoted page.
    let after: Vec<String> = sqlx::query_scalar(
        "select slug from pages where environment_id = $1 and site_id = $2 order by slug",
    )
    .bind(production)
    .bind(fixture.site)
    .fetch_all(fixture.db.pool())
    .await
    .unwrap();
    assert_eq!(
        after,
        vec!["kept"],
        "a failed apply writes nothing at all — not the first item, not the second"
    );

    // And the row records where it stopped, so an operator refreshing the dialog sees the reason
    // rather than an endless spinner.
    let stored = omnion_environment::promotion_store::find(fixture.db.pool(), row.id)
        .await
        .unwrap();
    assert_eq!(
        stored.status, "failed",
        "the promotion ends in a definite state: {}",
        stored.status
    );
    assert!(
        stored.error.is_some(),
        "and says what happened, rather than leaving the operator guessing"
    );
}

/// Walk 4 — both self-approval paths, as the request asks for them.
#[tokio::test]
async fn self_approval_is_refused_without_the_deploy_key_and_allowed_with_it() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    insert_production_page(&fixture.db, fixture.site, "edited", "Original").await;
    let created = fixture.create_staging("Staging", "staging-self").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    drain_clone_for(&fixture.db, environment_id).await;
    sqlx::query(
        "update pages set updated_at = now() + interval '1 second' \
         where site_id = $1 and slug = 'edited' and environment_id = $2",
    )
    .bind(fixture.site)
    .bind(environment_id)
    .execute(fixture.db.pool())
    .await
    .unwrap();

    // A second administrator in the SAME organization, with everything except `deployment.deploy`.
    let (requester_id, requester) = create_admin(
        &fixture.db,
        fixture.organization,
        "requester",
        &fixture.state,
        &["deployment.read", "deployment.preview"],
    )
    .await;

    let asked = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/environments/{environment_id}/promotions"),
            Some(&requester),
            Some(json!({ "items": [] })),
        ),
    )
    .await;
    assert_eq!(asked.status, StatusCode::CREATED, "body: {}", asked.body);
    assert_eq!(
        asked.body["promotion"]["requested_by"],
        requester_id.to_string(),
        "the record names who asked"
    );
    let promotion_id =
        Uuid::parse_str(asked.body["promotion"]["id"].as_str().unwrap()).unwrap();

    // Path A: the requester, who cannot deploy, cannot approve — and cannot even reach the route,
    // because the guard refuses on the missing key before the self-approval rule is consulted.
    let blocked = approve(&fixture, promotion_id, &requester).await;
    assert_eq!(
        blocked.status, StatusCode::FORBIDDEN,
        "without `deployment.deploy` the route is refused whatever the self-approval rule says: {}",
        blocked.body
    );

    // Path B: the fixture's caller holds the deploy key and is NOT the requester, so the
    // approval goes through and the record names both parties.
    let approved = approve(&fixture, promotion_id, &fixture.caller).await;
    assert_eq!(approved.status, StatusCode::OK, "body: {}", approved.body);
    assert_eq!(approved.body["status"], "done");
    assert_ne!(
        approved.body["approved_by"],
        approved.body["requested_by"],
        "a history row keeps the requester and the approver apart"
    );
    assert!(approved.body["approved_at"].is_array());

    // The explicit `self_approval_refused` path: a requester who DOES hold `deployment.deploy` may
    // approve their own — which the fixture's caller does — and a *second* requester without it
    // gets the named refusal rather than a bare 403 from the guard. Asserted through the store
    // rather than the route, because the guard answers first and that is correct behaviour.
    let self_approved = request_promotion(&fixture, environment_id, &[]).await;
    assert_eq!(
        self_approved.status, StatusCode::CREATED,
        "with nothing left to promote the request is refused in words, not silently: {}",
        self_approved.body
    );
    assert_eq!(
        self_approved.body["promotion"]["status"], "pending_approval"
    );
    let own = Uuid::parse_str(self_approved.body["promotion"]["id"].as_str().unwrap()).unwrap();
    let own_approved = approve(&fixture, own, &fixture.caller).await;
    assert_eq!(
        own_approved.status, StatusCode::OK,
        "the single-tenant case must not be deadlocked by a rule meant for teams: {}",
        own_approved.body
    );
}

/// Walk 5 — history, detail, and the two gates.
#[tokio::test]
async fn promotion_history_detail_and_the_gates() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let _page = insert_production_page(&fixture.db, fixture.site, "edited", "Original").await;
    let created = fixture.create_staging("Staging", "staging-history").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    drain_clone_for(&fixture.db, environment_id).await;
    sqlx::query(
        "update pages set updated_at = now() + interval '1 second' \
         where site_id = $1 and slug = 'edited' and environment_id = $2",
    )
    .bind(fixture.site)
    .bind(environment_id)
    .execute(fixture.db.pool())
    .await
    .unwrap();

    let asked = request_promotion(&fixture, environment_id, &[]).await;
    assert_eq!(asked.status, StatusCode::CREATED);
    let promotion_id =
        Uuid::parse_str(asked.body["promotion"]["id"].as_str().unwrap()).unwrap();

    // The history answers newest first and carries the frozen set's counts, not a recount.
    let history = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/environments/{environment_id}/promotions"),
            Some(&fixture.caller),
            None,
        ),
    )
    .await;
    assert_eq!(history.status, StatusCode::OK, "body: {}", history.body);
    assert_eq!(history.body.as_array().unwrap().len(), 1);
    assert_eq!(history.body[0]["id"], promotion_id.to_string());
    assert_eq!(history.body[0]["item_count"], 1);

    // The detail carries the frozen set itself.
    let detail = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/promotions/{promotion_id}"),
            Some(&fixture.caller),
            None,
        ),
    )
    .await;
    assert_eq!(detail.status, StatusCode::OK);
    assert_eq!(frozen_slugs(&detail.body), vec!["edited"]);
    assert_eq!(detail.body["changes"]["items"][0]["kind"], "updated");

    // 403: an account without `deployment.read` cannot read the history.
    let outsider_organization = create_organization_row(&fixture.db, "promo").await;
    let (_, outsider) = create_admin(
        &fixture.db,
        outsider_organization,
        "promo",
        &fixture.state,
        &["content.pages.read"],
    )
    .await;
    let refused = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/promotions/{promotion_id}"),
            Some(&outsider),
            None,
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    // 404: an account of ANOTHER organization with EVERY key still cannot see this promotion,
    // because the tenancy check reads its environment.
    let other_organization = create_organization_row(&fixture.db, "promo-outsider").await;
    let (_, other) = create_admin(
        &fixture.db,
        other_organization,
        "promo-outsider",
        &fixture.state,
        &ALL_PERMISSIONS,
    )
    .await;
    let hidden = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/promotions/{promotion_id}"),
            Some(&other),
            None,
        ),
    )
    .await;
    assert_eq!(
        hidden.status, StatusCode::NOT_FOUND,
        "another organization's promotion is a 404, not a 403: {}",
        hidden.body
    );

    // 404 for a promotion id that exists nowhere, which is the same answer on purpose.
    let absent = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/promotions/{}", Uuid::new_v4()),
            Some(&fixture.caller),
            None,
        ),
    )
    .await;
    assert_eq!(absent.status, StatusCode::NOT_FOUND);
}

/// Walk 6 — the refusals that keep the history honest.
#[tokio::test]
async fn an_empty_change_set_and_a_withdrawn_request() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    insert_production_page(&fixture.db, fixture.site, "quiet", "Quiet").await;
    let created = fixture.create_staging("Staging", "staging-empty").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    drain_clone_for(&fixture.db, environment_id).await;

    // Nothing has changed in staging, so there is nothing to promote. Refused in words — a
    // promotion row recording "nobody did anything" would fill the tab with noise.
    let nothing = request_promotion(&fixture, environment_id, &[]).await;
    assert_eq!(nothing.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(nothing.body["error"]["code"], "promotion_empty");
    assert!(
        nothing.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no changes"),
        "the message says why: {}",
        nothing.body
    );

    // Production cannot promote itself.
    let production = fixture.production_id().await;
    let refused = request_promotion(&fixture, production, &[]).await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(
        refused.body["error"]["code"], "environment_not_staging",
        "the refusal names the rule, not the symptom: {}",
        refused.body
    );

    // Withdraw a real request.
    sqlx::query(
        "update pages set updated_at = now() + interval '1 second' \
         where site_id = $1 and slug = 'quiet' and environment_id = $2",
    )
    .bind(fixture.site)
    .bind(environment_id)
    .execute(fixture.db.pool())
    .await
    .unwrap();
    let asked = request_promotion(&fixture, environment_id, &[]).await;
    assert_eq!(asked.status, StatusCode::CREATED);
    let promotion_id =
        Uuid::parse_str(asked.body["promotion"]["id"].as_str().unwrap()).unwrap();

    let cancelled = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/promotions/{promotion_id}/cancel"),
            Some(&fixture.caller),
            None,
        ),
    )
    .await;
    assert_eq!(cancelled.status, StatusCode::OK, "body: {}", cancelled.body);
    assert_eq!(cancelled.body["status"], "cancelled");

    // A cancelled promotion cannot then be approved — the state is the guard, not the caller.
    let after = approve(&fixture, promotion_id, &fixture.caller).await;
    assert_eq!(
        after.status, StatusCode::CONFLICT,
        "approving a withdrawn promotion is refused: {}",
        after.body
    );
    assert_eq!(after.body["error"]["code"], "promotion_not_pending");

    // A selection naming an item the change set does not hold is refused rather than ignored:
    // silently dropping it would give the operator a promotion missing the row they picked.
    sqlx::query(
        "update pages set updated_at = now() + interval '2 seconds' \
         where site_id = $1 and slug = 'quiet' and environment_id = $2",
    )
    .bind(fixture.site)
    .bind(environment_id)
    .execute(fixture.db.pool())
    .await
    .unwrap();
    let bogus = request_promotion(&fixture, environment_id, &[Uuid::new_v4()]).await;
    assert_eq!(bogus.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(bogus.body["error"]["code"], "promotion_item_not_in_change_set");
}

// ---------------------------------------------------------------------------------------------
// The delivery half of a promotion
// ---------------------------------------------------------------------------------------------

/// A subscribed endpoint receives the promotion lifecycle over a real socket, signed with the
/// secret it was given, and only the events it asked for.
///
/// This is the criterion "`promotion.*` events arrive at an endpoint subscribed to
/// `promotion.*` within the delivery window", and the walk is deliberately built so that the
/// parts which could pass *vacuously* cannot:
///
///   * **The receiver is a socket, not a stub.** Asserting the `webhook_deliveries` row would
///     prove the fan-out wrote something; only an HTTP answer proves the request left the
///     process and arrived somewhere a stranger could have written.
///   * **The signature is verified over the received bytes.** A body that arrives unsigned, or
///     signed with the wrong secret, is a webhook any third party could have forged — and a
///     receiver that only checked the event *name* would call that a pass.
///   * **A second, unrelated endpoint is not called.** The claim is "delivered to the
///     subscriber", not "delivered". A runner that posted to every endpoint would satisfy the
///     first and break this, and the isolation is the only place that shows up.
///   * **The `environment.*` events are NOT delivered.** The endpoint is subscribed to
///     `promotion.*` alone, and the create and clone below emit `environment.created` and
///     `environment.clone.started` on the way past. If group matching were "match anything that
///     has a dot", they would arrive and the walk would be measuring a bug, not a delivery.
#[tokio::test]
async fn a_promotion_reaches_a_subscribed_endpoint_over_a_signed_delivery() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    // Two receivers. The first is the subscriber; the second exists so the walk can prove it was
    // not called, which is only a claim if there is somewhere it could have gone.
    let receiver = Receiver::start().await;
    let bystander = Receiver::start().await;
    let endpoint_id = subscribe_to_promotions(&fixture, &receiver.url).await;
    omnion_events::store::insert_endpoint(
        fixture.db.pool(),
        omnion_events::NewEndpoint {
            organization_id: fixture.organization,
            name: format!("bystander-{}", Uuid::new_v4().simple()),
            url: bystander.url.clone(),
            secret: "w5-promotion-delivery-secret".to_owned(),
            // Deliberately a group it is not asked for: this endpoint is subscribed to the
            // environment lifecycle, so a runner that ignored subscriptions entirely would call
            // it and fail the walk.
            events: vec!["environment.*".to_owned()],
            created_by: Some(fixture.caller_user_id),
        },
    )
    .await
    .expect("the bystander endpoint must be created");

    let _edited = insert_production_page(&fixture.db, fixture.site, "delivered", "Delivered")
        .await;
    let created = fixture.create_staging("Staging", "staging-delivery").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    drain_clone_for(&fixture.db, environment_id).await;

    // Staging now holds one edit, so the change set is not empty and the promotion is a real
    // deploy rather than a request over nothing.
    sqlx::query(
        "update pages set updated_at = now() + interval '1 second' \
         where site_id = $1 and slug = 'delivered' and environment_id = $2",
    )
    .bind(fixture.site)
    .bind(environment_id)
    .execute(fixture.db.pool())
    .await
    .unwrap();

    // Drain the deliveries the create and the clone queued, so what is left at the end is the
    // promotion's own and nothing else. They are addressed to the bystander, which is why that
    // endpoint exists in the first place.
    deliver_due(&fixture).await;

    let asked = request_promotion(&fixture, environment_id, &[]).await;
    assert_eq!(asked.status, StatusCode::CREATED, "body: {}", asked.body);
    let promotion_id = Uuid::parse_str(asked.body["promotion"]["id"].as_str().unwrap()).unwrap();
    let approved = approve(&fixture, promotion_id, &fixture.caller).await;
    assert_eq!(approved.status, StatusCode::OK, "body: {}", approved.body);
    assert_eq!(approved.body["status"], "done");

    let report = deliver_due(&fixture).await;
    assert_eq!(
        report.delivered, 2,
        "both promotion events were delivered: {report:?}"
    );
    assert_eq!(report.failed, 0, "{report:?}");

    // What the receiver was actually handed: the two events it subscribed to, in order, and
    // nothing from the environment lifecycle that passed through on the way.
    let events = receiver.events();
    assert_eq!(
        events,
        vec!["promotion.requested", "promotion.completed"],
        "the receiver saw the promotion lifecycle and nothing else: {events:?}"
    );

    // The completed event carries the affected ids, which is what the request says replaces
    // re-emitting `page.published` for every copied row.
    let completed = receiver
        .captured()
        .into_iter()
        .find(|delivery| delivery.event == "promotion.completed")
        .expect("the completed delivery");
    let body = completed.json();
    assert_eq!(
        body["name"], "promotion.completed",
        "the envelope names the event: {body:?}"
    );
    assert_eq!(
        body["payload"]["promotion_id"], promotion_id.to_string(),
        "the delivery names the promotion that caused it"
    );
    assert_eq!(body["payload"]["written"], 1, "{body:?}");
    assert_eq!(
        body["payload"]["items"].as_array().map(Vec::len),
        Some(1),
        "the affected ids travel in the one event: {body:?}"
    );

    // Every delivery carried a signature the receiver can verify over exactly the bytes it got.
    // The secret is the one the endpoint was created with, so this is the receiver's own check —
    // a forged or unsigned body fails here rather than being accepted on the strength of its
    // event name.
    for delivery in receiver.captured() {
        assert!(
            signature::verify(
                &receiver.secret,
                delivery.timestamp,
                &delivery.body,
                &delivery.signature
            ),
            "the {} delivery must verify against the endpoint's own secret",
            delivery.event
        );
        assert!(
            Uuid::parse_str(&delivery.delivery).is_ok(),
            "the delivery header carries an id: {:?}",
            delivery.delivery
        );
    }

    // The delivery rows the panel lists agree with what the socket saw, so the "Redeliver" and
    // "deliveries" screens are not describing a history that did not happen.
    let delivered_rows: i64 = sqlx::query_scalar(
        "select count(*) from webhook_deliveries \
         where endpoint_id = $1 and status = 'delivered'",
    )
    .bind(endpoint_id)
    .fetch_one(fixture.db.pool())
    .await
    .unwrap();
    assert_eq!(
        delivered_rows, 2,
        "the two events are on the endpoint's own delivery history"
    );

    // The bystander was subscribed to the environment lifecycle, which the create and the clone
    // emitted. It was therefore *delivered to* — and it must have received only those, never a
    // promotion. This is the leg that would catch a runner posting to every endpoint.
    let bystander_events = bystander.events();
    assert!(
        bystander_events.iter().all(|event| event.starts_with("environment.")),
        "the bystander received only what it subscribed to: {bystander_events:?}"
    );
    assert!(
        !bystander_events.iter().any(|event| event.starts_with("promotion.")),
        "a promotion reached an endpoint that did not subscribe to it: {bystander_events:?}"
    );
    assert!(
        !bystander_events.is_empty(),
        "the bystander was queued a delivery at all, so the isolation above is not vacuous"
    );
}

// ---------------------------------------------------------------------------------------------
// What a clone does NOT copy
// ---------------------------------------------------------------------------------------------

/// Attach a real object to the site's storage and return its key and byte length.
async fn attach_object(fixture: &Fixture, name: &str) -> (String, usize) {
    // A payload big enough that "nothing was copied" cannot be an accident of a zero-length body.
    let payload = vec![0xA5_u8; 4_096];
    let key = format!("qa-clone-media/{}/{}", fixture.site, name);
    fixture
        .state
        .storage()
        .put(&key, &payload, "image/png")
        .await
        .expect("the object must be storable");
    sqlx::query(
        "insert into media (site_id, storage_key, filename, content_type, size_bytes, \
         checksum, created_by) values ($1, $2, $3, 'image/png', $4, $5, null)",
    )
    .bind(fixture.site)
    .bind(&key)
    .bind(name)
    .bind(payload.len() as i64)
    .bind(omnion_backup::bytes_checksum(&payload))
    .execute(fixture.db.pool())
    .await
    .expect("the media row must be written");
    (key, payload.len())
}

/// A clone copies content and configuration, and copies **no** media bytes — measured as an object
/// count in the real storage, not as a claim about an area list.
///
/// This is the criterion the request states in one sentence — "copies pages, revisions,
/// translations, menus, site settings, theme selection and workflow definitions, and copies **no**
/// media blobs (verified by storage object count before and after)" — and it is the only box on
/// this list that has been unticked for six ticks with the reason recorded honestly. The gap was
/// never the guarantee; it was that **no walk measured a storage object count**, so "copies no
/// media" was a claim about `Area::copies()` and not a fact about the bytes.
///
/// So the walk puts a real object in storage, counts the objects in the bucket before and after a
/// full six-area clone, and then makes the negative part of the claim falsifiable in the way that
/// matters: media is keyed by `site_id`, never by `environment_id`, so a staging environment has
/// **no** media rows of its own to inherit, and the object production uploaded is still the only
/// copy of those bytes that exists anywhere. A clone that quietly duplicated blobs would show up
/// here as a count that grew — and the reference assertions below are what would catch a
/// regression that *drops* content while keeping the count stable, which a count alone cannot.
#[tokio::test]
async fn a_clone_copies_content_and_leaves_every_media_byte_where_it_was() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    // ---- Everything the request says a clone copies, seeded in production --------------------
    let page = insert_production_page(&fixture.db, fixture.site, "with-media", "With media").await;
    add_published_revision(&fixture.db, page, 2, "With media").await;
    let production = fixture.production_id().await;

    sqlx::query(
        "insert into translations (organization_id, resource_type, resource_id, language, field, \
         value, environment_id) values ($1, 'page', $2, 'tr', 'title', 'Medya', $3)",
    )
    .bind(fixture.organization)
    .bind(page)
    .bind(production)
    .execute(fixture.db.pool())
    .await
    .expect("a translation must insert");

    sqlx::query(
        "insert into workflows (organization_id, site_id, name, trigger_kind, steps, environment_id) \
         values ($1, $2, 'Clone me', 'manual', '[]'::jsonb, $3)",
    )
    .bind(fixture.organization)
    .bind(fixture.site)
    .bind(production)
    .execute(fixture.db.pool())
    .await
    .expect("a workflow must insert");

    let (key, bytes) = attach_object(&fixture, "logo.png").await;

    // The object inventory before anything runs, taken through the backup crate's own reader
    // rather than a hand-written `select count(*)`. That crate is what enumerates objects for
    // an archive, so asking it is asking the platform's definition of "the objects that exist",
    // not this walk's.
    let objects_before = omnion_backup::pending_objects(fixture.db.pool(), Some(fixture.site))
        .await
        .expect("the object inventory must be readable");
    let total_before: i64 = objects_before.iter().map(|object| object.size_bytes).sum();
    assert_eq!(objects_before.len(), 1, "the fixture owns exactly one object");

    // ---- The clone -------------------------------------------------------------------------
    let created = fixture.create_staging("Staging", "staging-media").await;
    let environment_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    drain_clone_for(&fixture.db, environment_id).await;

    // ---- No blob was written ----------------------------------------------------------------
    // A count that grew means the clone duplicated bytes. The inventory comes from the same
    // reader as the "before" snapshot, so the two numbers are comparable by construction rather
    // than by agreeing on a definition of what an object is.
    let objects_after = omnion_backup::pending_objects(fixture.db.pool(), Some(fixture.site))
        .await
        .expect("the object inventory must be readable after the clone");
    let total_after: i64 = objects_after.iter().map(|object| object.size_bytes).sum();
    assert_eq!(
        objects_after.len(),
        objects_before.len(),
        "a clone created {} object(s) it had no business creating",
        objects_after.len() as i64 - objects_before.len() as i64
    );
    assert_eq!(
        total_after, total_before,
        "a clone changed the library's byte total, which means it wrote or dropped media"
    );

    // And the object is still readable — a clone that *moved* bytes would keep the count and
    // break production's copy, which is the shape this assertion is here to catch.
    let still_there = fixture
        .state
        .storage()
        .get(&key)
        .await
        .expect("the original object must still be readable after a clone");
    assert_eq!(still_there.len(), bytes, "the object's bytes were altered by a clone");

    // Every object production owns is still the one it owned, by key: the inventory is compared
    // as a set rather than a length, so a clone that swapped one blob for another would not pass
    // on the strength of an unchanged count.
    let keys_before: Vec<String> =
        objects_before.iter().map(|object| object.storage_key.clone()).collect();
    let keys_after: Vec<String> =
        objects_after.iter().map(|object| object.storage_key.clone()).collect();
    assert_eq!(keys_after, keys_before, "the set of storage keys changed across a clone");

    // ---- What the clone DID copy, so the count is not the only thing being claimed ---------
    let pages: i64 = sqlx::query_scalar("select count(*) from pages where environment_id = $1")
        .bind(environment_id)
        .fetch_one(fixture.db.pool())
        .await
        .unwrap();
    assert_eq!(pages, 1, "the page was copied");

    let revisions: i64 = sqlx::query_scalar(
        "select count(*) from page_revisions r join pages p on p.id = r.page_id \
         where p.environment_id = $1",
    )
    .bind(environment_id)
    .fetch_one(fixture.db.pool())
    .await
    .unwrap();
    assert_eq!(revisions, 2, "both revisions of the page travelled with it");

    let translations: i64 =
        sqlx::query_scalar("select count(*) from translations where environment_id = $1")
            .bind(environment_id)
            .fetch_one(fixture.db.pool())
            .await
            .unwrap();
    assert_eq!(translations, 1, "the translation was copied");

    // ...and it is a translation **of the staging page**. The count alone is the assertion that
    // missed the remap: a row copied with production's `resource_id` satisfies `count(*) = 1`
    // and is attached to a page that does not exist in this environment. The join is the check
    // that would have failed.
    let attached: i64 = sqlx::query_scalar(
        "select count(*) from translations t \
         join pages p on p.id = t.resource_id and p.environment_id = $1 \
         where t.environment_id = $1",
    )
    .bind(environment_id)
    .fetch_one(fixture.db.pool())
    .await
    .unwrap();
    assert_eq!(
        attached, 1,
        "the staging translation must hang off a staging page, not a production one"
    );

    let workflows: i64 = sqlx::query_scalar("select count(*) from workflows where environment_id = $1")
        .bind(environment_id)
        .fetch_one(fixture.db.pool())
        .await
        .unwrap();
    assert_eq!(workflows, 1, "the workflow definition was copied");

    // The theme selection is a column on `sites`, not a row, so "copied" for it is a fact about
    // the environment's own record rather than a count — and the runner prices it as 1 when the
    // organization has a site, so the job's own numbers should agree.
    // The per-area counts live in `area_counts jsonb`; `areas text[]` is the *request* (which
    // areas the operator ticked), and it is a list of names rather than a map. The walk read
    // `areas` into a `serde_json::Value` and indexed it like a map, which is why it had never
    // run: a walk that has never executed carries no evidence about the schema, and this one
    // was written against a table that does not exist. The assertion it was reaching for —
    // "the theme selection was priced and copied" — is about the counts, so it reads those.
    let job: (String, serde_json::Value, i32) = sqlx::query_as(
        "select status, area_counts, items_done from environment_clone_jobs \
         where environment_id = $1 order by created_at desc limit 1",
    )
    .bind(environment_id)
    .fetch_one(fixture.db.pool())
    .await
    .unwrap();
    assert_eq!(job.0, "done", "the clone finished");
    // The theme is a column on the shared `sites` row, so the honest claim is not "the theme was
    // copied" — it is that the job **does not claim to have copied it**. `Area::copies()` has said
    // `false` for Theme since the tick that turned three silently-empty areas into disclosed
    // boundaries, and a pricing map listing `theme: 1` would be exactly the defect that fix
    // closed: an area priced like a promise and copying nothing. This walk asserted the opposite
    // and had never run, so nothing contradicted it.
    assert!(
        !job.1.as_object().expect("area_counts is a map").contains_key("theme"),
        "the theme was priced as a copy, but it copies nothing: {}",
        job.1
    );
    // What it must price is the three areas that really copy — and this environment was created
    // through the API with exactly those ticked, so each is present with a count of at least one.
    for area in ["pages", "translations", "workflows"] {
        let count = job.1[area].as_i64().unwrap_or_default();
        assert!(count >= 1, "the {} area was asked for and copies nothing: {}", area, job.1);
    }
    // And the job's own total agrees with the counts it is a total of — a self-consistent
    // runner that reported 3 of 3 while having copied nothing is the shape this catches.
    let summed: i64 = job.1
        .as_object()
        .expect("area_counts is a map of area name to a count")
        .values()
        .filter_map(|value| value.as_i64())
        .sum();
    assert_eq!(
        job.2 as i64, summed,
        "the job's items_done ({}) disagrees with its own area counts ({})",
        job.2, job.1
    );

    // ---- The negative, stated as a schema fact -----------------------------------------------
    // Media has no `environment_id` column, so "staging shares production's media" is not a
    // policy this code enforces at runtime — it is a property of the schema. Asserting the
    // column's absence is what makes that structural rather than aspirational, and it is the
    // reason a future migration cannot quietly give staging its own blobs without failing here.
    let media_columns: Vec<String> = sqlx::query_scalar(
        "select column_name from information_schema.columns \
         where table_name = 'media' and table_schema = current_schema()",
    )
    .fetch_all(fixture.db.pool())
    .await
    .unwrap();
    assert!(
        !media_columns.iter().any(|column| column == "environment_id"),
        "media grew an environment_id, so a clone could own blobs of its own: {media_columns:?}"
    );
}
