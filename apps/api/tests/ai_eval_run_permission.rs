//! **The eval run surface's permission split, over HTTP** (REQ-107, slice 6c).
//!
//! Every other walk on this request reads rows. This one drives the **router**, because the
//! acceptance row is about two layers at once and neither can prove it alone:
//!
//! > A caller without `ai.evals.run` sees `Run now` disabled with the permission named; the API
//! > answers `403` for the same call.
//!
//! The panel half is served by `GET /ai/evals/suites`, which reports `viewer_missing`; the API half
//! is `POST /ai/evals/suites/{key}/run`. Both halves read the same three keys, and the existing
//! source-level walk (`the_disabled_reason_names_a_key_the_mount_actually_guards`) proves the key
//! *lists* agree. It cannot prove either list is correct: a list of three strings the panel never
//! enables and a mount that never checks them agree perfectly, are both useless, and both stay
//! green forever.
//!
//! So this file builds the **real fixture** the panel was designed against — a role that can read
//! the suites and cannot spend tokens on them — and asks the server itself.
//!
//! ## Why a custom role and not a base one
//!
//! The seeded ladder is Owner → Administrator → Member. Owner holds every catalogue key
//! (`BasePermissions::All`), and `ai.evals.*` is in **no** base role's explicit list — which means
//! on a fresh installation an Administrator cannot run an eval at all, and Member cannot even read
//! one. That is a deliberate split the catalogue documents, but it leaves the interesting middle
//! ("may read, may not run") unreachable from a seeded role. The fixture therefore creates a role
//! carrying exactly `ai.evals.read`, which is the case the acceptance row is written about: a
//! person who has been given the QA audience and not the bill.
//!
//! ## The two halves are asserted together on purpose
//!
//! Either alone is satisfiable by a defect. A panel that showed an enabled button and an API that
//! answered `403` satisfies "the API answers 403". A panel that disabled the button for a reason
//! nobody could act on — and the API that never refused — satisfies "Run now is disabled". The
//! claim is that the two agree, so both are read from the same session in the same walk.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, CsrfSecret, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// The accounts this file creates.
const PASSWORD: &str = "correct horse battery";

/// A throwaway secret for the disposable stack.
///
/// Without it every cookie-authenticated POST is refused `403 csrf_unavailable` — a correct
/// refusal that is not the one under test, and a refusal whose code would make a missing-permission
/// walk pass for the wrong reason if it only asserted `403`. Configured here so the walk's `403`
/// can be attributed to the guard rather than to the middleware. Borrowed from `ai_guard_outbound`,
/// which documents the same trap.
const CSRF_SECRET: &str = "eval-run-permission-walk-only";

struct TestResponse {
    status: StatusCode,
    set_cookie: Vec<String>,
    body: Value,
    text: String,
}

impl TestResponse {
    /// One `Set-Cookie`'s value, by name.
    ///
    /// **The name is matched inside the search, not after it.** The first version took the first
    /// cookie that had an `=` and compared its name afterwards — and `Set-Cookie` carries the
    /// session *first*, so `omnion_csrf` was never found and every session in this file sent an
    /// empty CSRF header. Both walks then failed `403 csrf_failed` at the fixture's own step,
    /// which reads as a permission problem and is one. A helper that answers "not found" for a
    /// cookie that is present is worse than one that does not exist.
    fn cookie(&self, name: &str) -> String {
        self.set_cookie
            .iter()
            .filter_map(|header| header.split(';').next())
            .filter_map(|pair| pair.trim().split_once('='))
            .find(|(key, _)| key.trim() == name)
            .map(|(_, value)| value.to_owned())
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

        let database = format!("omnion_evalperm_{}", Uuid::new_v4().simple());
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

        Some(Self { state, db, maintenance, database })
    }

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
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        TestResponse { status, set_cookie, body, text }
    }

    /// Give this suite a sign-in budget of its own.
    ///
    /// The limiter is a process-wide cell filled from the **stored** policy document, and
    /// `sign_in` ships at ten per five minutes per IP — a ceiling every suite sharing this box is
    /// spending too. Borrowed verbatim from `ai_guard_outbound`, which measured what happens when a
    /// suite hits it: a `429` naming a limit it was never testing, which looks like a guard defect.
    fn raise_the_sign_in_ceiling(&self) {
        let policies: Vec<omnion_security::RatePolicy> = omnion_security::RatePolicy::defaults()
            .into_iter()
            .map(|mut policy| {
                if policy.scope == "sign_in" {
                    policy.limit = 10_000;
                }
                policy
            })
            .collect();
        let _ = omnion_api::rate_limit_middleware::install(omnion_api::rate_limit_middleware::RateLimiter::new(&self.state, policies));
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
    match Db::connect(&DatabaseConfig { url: config.database.url.clone(), max_connections: 1 }).await {
        Ok(db) => Some(db),
        Err(_) => None,
    }
}

fn maintenance_config(config: &Config) -> DatabaseConfig {
    let url = config.database.url.clone();
    let (base, _) = url.rsplit_once('/').expect("a database URL has a path");
    DatabaseConfig { url: format!("{base}/postgres"), max_connections: 1 }
}

fn swap_database(url: &str, database: &str) -> String {
    let (base, _) = url.rsplit_once('/').expect("a database URL has a path");
    format!("{base}/{database}")
}

/// A session and the CSRF token that goes with it.
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
        // **The CSRF cookie is required, not optional.** Every cookie-authenticated POST in this
        // file goes through `RequireCsrf`, and a missing token answers `403 csrf_failed` — the
        // same status a missing permission answers, with a different code. A session that
        // captured an empty CSRF token would turn every write below into a permission refusal
        // that the walk would happily report as the thing it was testing.
        let csrf = response.cookie("omnion_csrf");
        assert!(
            !csrf.is_empty(),
            "the response must set a CSRF cookie; cookies were {:?}",
            response.set_cookie
        );
        Self { token, csrf }
    }

    /// The cookie and the CSRF header, in one place.
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

fn post(uri: &str, body: Value, session: Option<&Session>) -> Request<Body> {
    let builder = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    let builder = match session {
        Some(session) => session.auth(builder),
        None => builder,
    };
    builder.body(Body::from(body.to_string())).expect("request must build")
}

/// The harness, or a **panic**.
///
/// A skipped walk proves nothing, and a walk that skipped would leave the acceptance row's two
/// halves unproven while the suite summary still read green — which is the exact failure mode the
/// sibling files in this directory document at length.
macro_rules! perm {
    () => {
        match Harness::fresh().await {
            Some(harness) => harness,
            None => panic!(
                "PostgreSQL is not reachable, so every walk in this file would have SKIPPED. \
                 Set OMNION_DATABASE_URL to an existing database. A skip must not read as a pass."
            ),
        }
    };
}

struct Fixture {
    /// The reader: a signed-in member whose role carries `ai.evals.read` and nothing else.
    reader: Session,
    /// The tenant, so the suite created below belongs to it.
    organization: Uuid,
    /// The reader's own role, so a walk can grant the key under test **to the same session**
    /// rather than to a second account that would differ in a dozen other ways.
    role: Uuid,
}

/// One tenant, one suite with an enabled case, and a reader who may look but not spend.
async fn reader_with_a_suite(harness: &Harness) -> Fixture {
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
    assert_eq!(owner.status, StatusCode::CREATED, "{}", owner.text);
    let owner_session = Session::of(&owner);

    let organization_created = harness
        .call(post(
            "/api/v1/onboarding/organization",
            json!({ "name": "Eval Permission Walk" }),
            Some(&owner_session),
        ))
        .await;
    assert_eq!(
        organization_created.status, StatusCode::OK,
        "{}",
        organization_created.text
    );
    let organization: Uuid = sqlx::query_scalar("select id from organizations order by created_at limit 1")
        .fetch_one(harness.db.pool())
        .await
        .expect("the organization the bootstrap created must be readable");

    // A real suite with a real case, so `POST …/run` is refused by the **permission** and not by
    // one of the four earlier refusals the route makes (no such suite, disabled, not ready, gate
    // with no baseline). A walk that skipped that setup would prove `403` for a suite that does
    // not exist — and a route that refused everything would satisfy it.
    let provider = Uuid::new_v4();
    sqlx::query(
        "insert into ai_providers (id, name, kind, base_url, enabled) \
         values ($1, $2, 'cloud', 'https://models.invalid/v1', true)",
    )
    .bind(provider)
    .bind(format!("evalperm-provider-{provider}"))
    .execute(harness.db.pool())
    .await
    .expect("the provider must be created");
    let model = Uuid::new_v4();
    sqlx::query(
        "insert into ai_models (id, provider_id, model_key, display_name) \
         values ($1, $2, $3, $3)",
    )
    .bind(model)
    .bind(provider)
    .bind("walk-model")
    .execute(harness.db.pool())
    .await
    .expect("the model must be created");

    let suite = harness
        .call(post(
            "/api/v1/ai/evals/suites",
            json!({
                "key": "permission-walk",
                "name": "Permission walk",
                "target": "model",
                "model_id": model,
                "threshold_percent": 90,
            }),
            Some(&owner_session),
        ))
        .await;
    assert_eq!(suite.status, StatusCode::CREATED, "{}", suite.text);
    let case = harness
        .call(post(
            "/api/v1/ai/evals/suites/permission-walk/cases",
            json!({
                "name": "greeting",
                "input": { "prompt": "say hello" },
                "expected": { "contains": ["hello"] },
            }),
            Some(&owner_session),
        ))
        .await;
    assert_eq!(case.status, StatusCode::CREATED, "{}", case.text);

    // The reader's role: a catalogue key and no more. Created directly rather than through
    // `/api/v1/iam/roles` because the shape under test is the *effective* permission set, and the
    // role API would let a walk accidentally grant more than it means to.
    let role = Uuid::new_v4();
    sqlx::query(
        "insert into roles (id, name, key, priority, organization_id) \
         values ($1, 'Eval reader', $2, 100, $3)",
    )
    .bind(role)
    .bind(format!("eval-reader-{}", role.simple()))
    .bind(organization)
    .execute(harness.db.pool())
    .await
    .expect("the reader's role must be created");
    sqlx::query(
        "insert into role_permissions (role_id, permission_key, effect) values ($1, $2, 'allow')",
    )
    .bind(role)
    .bind("ai.evals.read")
    .execute(harness.db.pool())
    .await
    .expect("the read grant must be written");

    let reader_email = format!("reader-{}@omnion.test", Uuid::new_v4().simple());
    let reader = harness
        .call(post(
            "/api/v1/iam/users",
            json!({
                "email": reader_email,
                "display_name": "Read Only",
                "organization_id": organization,
                "password": PASSWORD,
                "role_id": role,
            }),
            Some(&owner_session),
        ))
        .await;
    assert_eq!(reader.status, StatusCode::CREATED, "{}", reader.text);

    let signed_in = harness
        .call(post(
            "/api/v1/auth/login",
            json!({ "email": reader_email, "password": PASSWORD }),
            None,
        ))
        .await;
    assert_eq!(
        signed_in.status, StatusCode::OK,
        "the reader must be able to sign in: {}",
        signed_in.text
    );
    let reader_session = Session::of(&signed_in);

    // The binding is **read back**, not written. `POST /iam/users` with a `role_id` binds it, so
    // the first version of this fixture inserted its own row on top and the walk died on
    // `role_bindings_active_key` — which says "this binding already exists" and nothing about the
    // permission split under test. Asserting that the user's effective binding is the role we
    // created is also a stronger claim than an insert succeeded: a route that accepted `role_id`
    // and ignored it would leave the user unbound, and a walk that inserted its own row would have
    // hidden exactly that.
    let user_id: Uuid = sqlx::query_scalar("select id from users where email = $1")
        .bind(&reader_email)
        .fetch_one(harness.db.pool())
        .await
        .expect("the reader's user row must be readable");
    let bound: Vec<Uuid> = sqlx::query_scalar(
        "select role_id from role_bindings \
         where user_id = $1 and organization_id = $2 and revoked_at is null",
    )
    .bind(user_id)
    .bind(organization)
    .fetch_all(harness.db.pool())
    .await
    .expect("the reader's bindings must be readable");
    assert_eq!(
        bound,
        vec![role],
        "the reader must hold exactly the role this walk created, and nothing else"
    );

    Fixture { reader: reader_session, organization, role }
}

/// **A reader is told which key is missing, and is refused with it.**
///
/// The acceptance row, both halves, in one walk. Asserted in this order because each step is a
/// precondition for the one after it:
///
/// 1. the reader can **read** — otherwise the `403` below would be a role that was never wired,
///    which looks identical from the outside and proves nothing about `ai.evals.run`;
/// 2. `viewer_missing` **names `ai.evals.run`** — the panel's `title` is built from this list, so a
///    list without the key is a disabled button whose reason is a generic sentence;
/// 3. `viewer_missing` does **not** name `ai.evals.read` — a reader who is told they are missing
///    the key they hold gets a reason that is simply false;
/// 4. the run is refused **`403 permission_denied`** — named by code, not just status. The
///    middleware refuses writes with `403 csrf_unavailable` and a missing suite answers `404`, so
///    status alone cannot tell "you may not" from "that is not there".
#[tokio::test]
async fn a_caller_without_the_run_key_is_told_which_one_and_the_api_refuses_them() {
    let harness = perm!();
    let fixture = reader_with_a_suite(&harness).await;

    // 1. The read half. Without this the whole walk is a statement about a broken fixture.
    let listed = harness
        .call(get("/api/v1/ai/evals/suites", Some(&fixture.reader)))
        .await;
    assert_eq!(
        listed.status, StatusCode::OK,
        "the reader holds ai.evals.read and must be able to list the suites: {}",
        listed.text
    );
    let suites = listed.body["suites"].as_array().cloned().unwrap_or_default();
    assert_eq!(
        suites.len(),
        1,
        "the reader must see the tenant's own suite: {}",
        listed.text
    );

    // 2 and 3. The list the disabled button's reason is built from.
    let missing: Vec<String> = listed.body["viewer_missing"]
        .as_array()
        .map(|keys| {
            keys.iter()
                .filter_map(|key| key.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        missing.contains(&"ai.evals.run".to_owned()),
        "the panel can only disable Run now with a reason if the server names the key: {missing:?}"
    );
    assert!(
        !missing.contains(&"ai.evals.read".to_owned()),
        "the reader holds the read key and must not be told they are missing it: {missing:?}"
    );

    // 4. The write half, with the exact status and code.
    let refused = harness
        .call(post(
            "/api/v1/ai/evals/suites/permission-walk/run",
            json!({ "kind": "manual" }),
            Some(&fixture.reader),
        ))
        .await;
    assert_eq!(
        refused.status, StatusCode::FORBIDDEN,
        "a reader without ai.evals.run must be refused: {}",
        refused.text
    );
    assert_eq!(
        refused.body["error"]["code"], json!("permission_denied"),
        "the refusal must name the guard that refused it, not the CSRF middleware and not a \
         missing suite: {}",
        refused.text
    );

    // And nothing was queued. A refusal that still created a run would spend tokens on the very
    // audience whose whole point is that they may not spend tokens.
    let runs: i64 = sqlx::query_scalar(
        "select count(*) from ai_eval_runs r join ai_eval_suites s on s.id = r.suite_id \
         where s.organization_id = $1",
    )
    .bind(fixture.organization)
    .fetch_one(harness.db.pool())
    .await
    .expect("the run count must be readable");
    assert_eq!(runs, 0, "a refused run must leave no run row behind");

    harness.dispose().await;
}

/// **An Owner holds the run key, so the button is enabled and the same call is accepted.**
///
/// The control for the walk above, and it is the only thing that makes the walk above mean
/// something. `viewer_missing` is computed from `effective_permissions`, so a bug that returned the
/// empty set for *everyone* would satisfy every assertion in the reader walk: the reader would see
/// `ai.evals.run` named, `POST …/run` would 403, and nothing would be wrong. The reader walk's
/// step 1 (the read succeeds) narrows it but does not close it — a permission evaluator that
/// returns nothing at all also lets a read through by refusing the *list* route, which step 1
/// would have caught. What step 1 cannot catch is a viewer that reports no missing keys for an
/// owner, which leaves the button permanently enabled with a title that explains nothing.
///
/// So this walk asserts the other end of the same list for a caller that **does** hold the key:
/// `viewer_missing` is empty for the keys the panel disables on, and the run is accepted with
/// `202`. A queued run is left behind on purpose and cleaned by the throwaway database — the
/// runner is not started here, so it settles nothing and reads as `queued` forever, which is the
/// correct state for a row nobody claimed.
#[tokio::test]
async fn a_caller_with_the_run_key_is_told_of_nothing_and_the_api_accepts_them() {
    let harness = perm!();
    let fixture = reader_with_a_suite(&harness).await;

    // Promote the reader's own role with the one key under test, so **the same session** is the
    // counter-case rather than a second account — a second account would differ in a dozen other
    // ways and prove nothing about this key. The role id comes back from the fixture instead of
    // being looked up by a LIKE on the e-mail: a lookup that matched the wrong row would promote
    // nobody and the walk would fail for a reason it never states.
    sqlx::query(
        "insert into role_permissions (role_id, permission_key, effect) values ($1, $2, 'allow')",
    )
    .bind(fixture.role)
    .bind("ai.evals.run")
    .execute(harness.db.pool())
    .await
    .expect("the run grant must be written");

    let listed = harness
        .call(get("/api/v1/ai/evals/suites", Some(&fixture.reader)))
        .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.text);
    let missing: Vec<String> = listed.body["viewer_missing"]
        .as_array()
        .map(|keys| keys.iter().filter_map(|key| key.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    assert!(
        !missing.contains(&"ai.evals.run".to_owned()),
        "a caller holding ai.evals.run must not be told they are missing it: {missing:?}"
    );

    let started = harness
        .call(post(
            "/api/v1/ai/evals/suites/permission-walk/run",
            json!({ "kind": "manual" }),
            Some(&fixture.reader),
        ))
        .await;
    assert_eq!(
        started.status, StatusCode::ACCEPTED,
        "the same caller must now be able to start the run: {}",
        started.text
    );

    harness.dispose().await;
}