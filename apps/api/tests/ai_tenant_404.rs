//! The **route-level** 404 for another tenant's agent runtime (REQ-099).
//!
//! `apps/api/tests/ai_agent_workspace.rs` already proves the store half: `get_file`,
//! `get_file_by_id`, `list_files` and `delete_file` all answer "not there" for an agent in
//! another organization, and the file is still on disk afterwards. That is the right place for
//! it, because a query that forgets its `organization_id` is a store bug.
//!
//! This suite exists because that proof is one layer too low to close the acceptance
//! criterion. The criterion says a caller is *refused* — and the route is a second thing
//! between the store and the caller, with a second chance to forget the check. Concretely,
//! `get_run_route` resolves the run through `run_store::get_run(pool, organization, id)` and
//! then calls `workspace::list_files(pool, agent_id)` with a **bare agent id**: the scoping that
//! makes that safe is the one the route performed two lines earlier. Delete the
//! `agent_in_scope` call from the download handler and every store-level test in the other
//! suite still passes.
//!
//! So these walks go **through the router**, as a real member of tenant A, against an agent
//! and a run that belong to tenant B:
//!
//! - the workspace file routes answer `404` for the foreign agent — list, download and delete
//!   alike, because a route that scopes the listing and forgets the delete is the common shape;
//! - the run and run-steps routes answer `404` for the foreign run;
//! - **and the answer is 404 rather than 403**, because a `403` on a sequential uuid is a free
//!   existence oracle: it answers "this id exists, you just may not see it" for every id in the
//!   table, which is a list endpoint with extra steps;
//! - **and nothing changed** — the foreign run and its file are still exactly where they were
//!   after every attempt, asserted by re-reading the rows.
//!
//! The harness is the one `ai_live_path.rs` uses: a throwaway database, a real `routes::router`,
//! an installer session, a tenant, and a member of it. Two tenants are made by creating a
//! second organization and a member inside it.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::rate_limit_middleware::RateLimiter;
use omnion_api::routes;
use omnion_api::state::AppState;
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
/// Fixed, not random, and **configured** rather than left absent. Without it every
/// cookie-authenticated write is answered `403 csrf_unavailable`, and the honest reading of
/// that failure is "this deployment has no secret" — not "this suite cannot create a tenant".
/// `tests/backups.rs` carries the same note for the same reason: the three shapes of a
/// cookie-authenticated write (no secret, wrong token, absent token) produce three different
/// errors, and a suite that cannot tell them apart is testing the reader, not the product.
const CSRF_SECRET: &str = "tenant-404-integration-suite-key-material";

struct TestResponse {
    status: StatusCode,
    /// **Every** `Set-Cookie`, in order. A single-cookie accessor is the defect `tests/csrf.rs`
    /// was written about: login sets two cookies, and a harness that reads the first one
    /// happily reports green while every write it makes is refused.
    set_cookie: Vec<String>,
    body: Value,
}

impl TestResponse {
    /// The value of one cookie by name, or an empty string when the response did not set it.
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

        let database = format!("omnion_tenant404_{}", Uuid::new_v4().simple());
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

    /// Give this suite a rate-limit budget of its own.
    ///
    /// The limiter is a process-wide cell the router fills from the **stored** document, and
    /// the stored `sign_in` scope is ten per five minutes. This suite signs in once per
    /// tenant, but the *other* suites sharing this box are signing in too, and they all count
    /// against the same key — so a walk can be refused with a `429` naming a rate limit it was
    /// never testing. Only the sign-in ceiling moves: the others are what a deployment ships,
    /// and raising those would let a suite become the reason a genuinely over-budget request
    /// stops being refused. (`tests/backups.rs` carries the same helper for the same reason.)
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
        omnion_api::rate_limit_middleware::install(RateLimiter::new(&self.state, policies));
    }

    async fn call(&self, request: Request<Body>) -> TestResponse {
        let response = routes::router(self.state.clone())
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
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);

        TestResponse {
            status,
            set_cookie,
            body,
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

/// A signed-in session and the CSRF token that goes with it.
///
/// The two travel together on purpose. Passing only the session is the shape that produces a
/// green suite full of `403`s, and passing the CSRF token in the *cookie* jar instead of the
/// header is the shape that produces one that cannot write at all.
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

    /// The session cookie and the CSRF header, in one place so a new call site cannot forget
    /// half of the pair.
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

fn delete(uri: &str, session: &Session) -> Request<Body> {
    session
        .auth(Request::builder().method(Method::DELETE).uri(uri))
        .body(Body::empty())
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
// Fixtures
// -------------------------------------------------------------------------------------------

/// One tenant with a member session, an agent, a workspace file and a run.
struct Tenant {
    session: Session,
    organization: Uuid,
    agent_id: Uuid,
    run_id: Uuid,
    file_id: Uuid,
    file_path: String,
}

#[tokio::test]
async fn another_tenants_workspace_and_runs_answer_404_and_change_nothing() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
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
    assert_eq!(owner.status, StatusCode::CREATED, "{:?}", owner.body);
    let owner = Session::of(&owner);

    // The role an AI-using member needs, looked up rather than hard-coded.
    let roles = harness.call(get("/api/v1/iam/roles", Some(&owner))).await;
    assert_eq!(roles.status, StatusCode::OK, "{:?}", roles.body);
    let admin_role = roles.body["roles"]
        .as_array()
        .expect("roles array")
        .iter()
        .find(|role| role["key"] == "administrator" || role["key"] == "admin")
        .or_else(|| {
            roles.body["roles"]
                .as_array()
                .expect("roles array")
                .iter()
                .find(|role| role["key"] == "editor")
        })
        .and_then(|role| role["id"].as_str())
        .expect("an installation ships at least one role that may use the AI")
        .to_owned();

    // Two tenants. The order matters: each is created *and* populated before the next exists,
    // so a walk can never pass because the second organization was never made.
    let mut tenants: Vec<Tenant> = Vec::new();
    for name in ["Acme", "Globex"] {
        // `POST /onboarding/organization` is the **once-per-installation** wizard step, and it
        // says so: the second call answers `409 step_already_done`. It is the wrong door for a
        // second tenant — the tenancy route is the one an operator uses, and the refusal is
        // the proof that the wizard is not the multi-tenant API.
        let created = harness
            .call(post(
                "/api/v1/organizations",
                json!({ "name": name, "slug": name.to_lowercase() }),
                Some(&owner),
            ))
            .await;
        // `slug` is required rather than derived, and the refusal is a `422` whose body is
        // **null** — what a missing required field looks like when the extractor answers
        // before the handler does. An assertion that compared only status codes would have
        // called that a different suite's problem.
        assert_eq!(created.status, StatusCode::CREATED, "{name}: {:?}", created.body);
        let organization = Uuid::parse_str(
            created.body["id"]
                .as_str()
                .expect("the created organization carries an id"),
        )
        .expect("an id is a uuid");

        // `POST /iam/users` creates the row; it does not sign the new member in. Reading a
        // session cookie off that response is how this suite first asked for one and got none
        // — an `assert_eq!(status, CREATED)` passes happily on a response that is not a
        // session. So: create, then **log in as them**, and every call after this is a real
        // member's request rather than the installer's wearing a borrowed token.
        let email = format!("member-{}@omnion.test", Uuid::new_v4().simple());
        let member = harness
            .call(post(
                "/api/v1/iam/users",
                json!({
                    "email": email,
                    "display_name": format!("Member of {name}"),
                    "organization_id": organization,
                    "password": PASSWORD,
                    "role_id": admin_role,
                }),
                Some(&owner),
            ))
            .await;
        assert_eq!(member.status, StatusCode::CREATED, "{:?}", member.body);

        let signed_in = harness
            .call(post(
                "/api/v1/auth/login",
                json!({ "email": email, "password": PASSWORD }),
                None,
            ))
            .await;
        assert_eq!(
            signed_in.status, StatusCode::OK,
            "{}: {:?}",
            name, signed_in.body
        );
        tenants.push(Tenant {
            session: Session::of(&signed_in),
            organization,
            agent_id: Uuid::nil(),
            run_id: Uuid::nil(),
            file_id: Uuid::nil(),
            file_path: String::new(),
        });
    }
    assert_ne!(
        tenants[0].organization, tenants[1].organization,
        "the two tenants must be different organizations or nothing here is a cross-tenant read"
    );

    // An agent, a workspace file and a run per tenant, written **through the API** as that
    // tenant's own member. Writing the rows by hand would prove a rule about the fixture
    // rather than about the product.
    for (tenant, name) in tenants.iter_mut().zip(["acme", "globex"]) {
        let agent = harness
            .call(post(
                "/api/v1/ai/agents",
                json!({
                    "key": format!("{name}-agent"),
                    "name": format!("{name} agent"),
                    "system_prompt": "You answer questions.",
                }),
                Some(&tenant.session),
            ))
            .await;
        // 200, not 201: the agent route answers `StatusCode::OK` for a create. Asserting 201
        // would have been a suite that could never pass, and "fixing" it by accepting any
        // 2xx would have thrown away the only thing an assertion here is for.
        assert_eq!(agent.status, StatusCode::OK, "{name}: {:?}", agent.body);
        tenant.agent_id = Uuid::parse_str(
            agent.body["id"]
                .as_str()
                .expect("the created agent carries an id"),
        )
        .expect("an id is a uuid");

        // Two facts about this route, both learned the hard way. The upload is a `POST` to
        // `/files` — adding `{*path}` gets a `405`, because that route is `GET`/`DELETE` only.
        // And the path is a **`path` form field**, not the URL and not the filename: the route
        // refuses to take it from the uploaded name on purpose, so a fixture that guessed
        // either way got a `400` reading like a permissions problem.
        let boundary = "w7tenant404boundary";
        let payload = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"path\"\r\n\r\n\
             notes.md\r\n\
             --{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"notes.md\"\r\n\
             Content-Type: text/markdown\r\n\r\n{name} notes\r\n--{boundary}--\r\n"
        );
        let upload = Request::builder()
            .method(Method::POST)
            .uri(format!(
                "/api/v1/ai/agents/{}/files?organization_id={}",
                tenant.agent_id, tenant.organization
            ))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .header(
                header::COOKIE,
                format!("omnion_session={}", tenant.session.token),
            )
            .header("x-omnion-csrf", tenant.session.csrf.clone())
            .body(Body::from(payload))
            .expect("the upload request must build");
        let uploaded = harness.call(upload).await;
        assert_eq!(uploaded.status, StatusCode::CREATED, "{}: {:?}", name, uploaded.body);
        tenant.file_id = Uuid::parse_str(
            uploaded.body["id"]
                .as_str()
                .expect("the uploaded file carries an id"),
        )
        .expect("an id is a uuid");
        tenant.file_path = uploaded.body["path"]
            .as_str()
            .expect("the uploaded file carries its path")
            .to_owned();

        // A finished run, so the run routes have something real to refuse.
        let started = harness
            .call(post(
                &format!(
                    "/api/v1/ai/agents/{}/runs?organization_id={}",
                    tenant.agent_id, tenant.organization
                ),
                json!({ "goal": "What plans do you publish?" }),
                Some(&tenant.session),
            ))
            .await;
        // The runtime is disabled in a test build and the provider pool is empty, so the run is
        // refused *or* queued; both are normal answers. What must not happen is a 500.
        //
        // A run *row* is still needed, and a refusal leaves none. So: ask first, then write the
        // row directly **scoped to this tenant** — not "the newest run in the table", which is
        // how a walk ends up testing tenant A's refusal against tenant B's run. The id comes
        // back in the test fixture so the routes have the real thing to refuse.
        assert!(
            started.status.is_success() || started.status == StatusCode::SERVICE_UNAVAILABLE,
            "{name}: a run start must answer, not crash: {:?}",
            started.body
        );
        // `ai_runs` has **no `created_at`** — the orderable stamps are `started_at` and
        // `finished_at`, and the indexes are built on `started_at`. A fixture that reaches for
        // `created_at` on a table that never grew one gets a `42703` and reads as a schema
        // problem, which is exactly what it is: a column the fixture invented.
        let existing = sqlx::query(
            "select id from ai_runs where agent_id = $1 order by coalesce(started_at, finished_at) desc nulls last, id desc limit 1",
        )
        .bind(tenant.agent_id)
        .fetch_optional(harness.db.pool())
        .await
        .expect("the run lookup must answer")
        .map(|row| row.get::<Uuid, _>("id"));
        tenant.run_id = match existing {
            Some(id) => id,
            None => {
                let id = Uuid::new_v4();
                // A finished run needs both stamps, and `ai_runs_finished_has_stamp` and
                // `ai_runs_reason_implies_finished` are two constraints that fire on a row a
                // hand writes: the row is a fixture, but the database is the product's.
                sqlx::query(
                    "insert into ai_runs (id, organization_id, agent_id, goal, status, \
                     stop_reason, started_at, finished_at) \
                     values ($1, $2, $3, $4, 'completed', 'final_answer', now(), now())",
                )
                .bind(id)
                .bind(tenant.organization)
                .bind(tenant.agent_id)
                .bind("What plans do you publish?")
                .execute(harness.db.pool())
                .await
                .expect("the fixture run row must be writable");
                id
            }
        };
    }

    let intruder = &tenants[0];
    let victim = &tenants[1];

    // ---- the workspace routes ------------------------------------------------------------------------
    //
    // `?organization_id` is the intruder's own, so the request is *in scope* — it is the
    // foreign agent id that has to be refused. Sending the victim's organization would test
    // something else entirely: that the session may not act inside another tenant, which is
    // true but is not the acceptance criterion.
    let listed = harness
        .call(get(
            &format!(
                "/api/v1/ai/agents/{}/files?organization_id={}",
                victim.agent_id, intruder.organization
            ),
            Some(&intruder.session),
        ))
        .await;
    assert_eq!(
        listed.status,
        StatusCode::NOT_FOUND,
        "a foreign workspace must not be listable: {:?}",
        listed.body
    );

    let downloaded = harness
        .call(get(
            &format!(
                "/api/v1/ai/agents/{}/files/{}?organization_id={}",
                victim.agent_id,
                urlencode(&victim.file_path),
                intruder.organization
            ),
            Some(&intruder.session),
        ))
        .await;
    assert_eq!(
        downloaded.status,
        StatusCode::NOT_FOUND,
        "a foreign workspace file must not download: {:?}",
        downloaded.body
    );

    let deleted = harness
        .call(delete(
            &format!(
                "/api/v1/ai/agents/{}/files/{}?organization_id={}",
                victim.agent_id,
                urlencode(&victim.file_path),
                intruder.organization
            ),
            &intruder.session,
        ))
        .await;
    assert_eq!(
        deleted.status,
        StatusCode::NOT_FOUND,
        "a foreign workspace file must not be deletable: {:?}",
        deleted.body
    );

    // ---- the run routes -----------------------------------------------------------------------------
    for (label, uri) in [
        (
            "the run detail",
            format!(
                "/api/v1/ai/runs/{}?organization_id={}",
                victim.run_id, intruder.organization
            ),
        ),
        (
            "the run's steps",
            format!(
                "/api/v1/ai/runs/{}/steps?organization_id={}",
                victim.run_id, intruder.organization
            ),
        ),
    ] {
        let response = harness.call(get(&uri, Some(&intruder.session))).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{} must not be readable across organizations: {:?}",
            label,
            response.body
        );
        // 404, not 403: the status itself is the assertion about existence. A 403 here would
        // confirm the uuid is real to anybody willing to enumerate the table.
        assert_ne!(
            response.status,
            StatusCode::FORBIDDEN,
            "{}: 403 is an existence oracle, the criterion asks for 404",
            label
        );
    }

    // ---- and nothing moved ---------------------------------------------------------------------------
    //
    // A refused read is half a promise. These four rows are the other half: every attempt above
    // must have left the victim's workspace file and run exactly as they were.
    let file = sqlx::query("select id, path, size_bytes from ai_agent_files where id = $1")
        .bind(victim.file_id)
        .fetch_one(harness.db.pool())
        .await
        .expect("the victim's file must still be there after three refusals");
    assert_eq!(
        file.get::<String, _>("path"),
        victim.file_path,
        "the file's path must be untouched"
    );
    let still_there = sqlx::query("select agent_id from ai_runs where id = $1")
        .bind(victim.run_id)
        .fetch_one(harness.db.pool())
        .await
        .expect("the victim's run must still be there")
        .get::<Uuid, _>("agent_id");
    assert_eq!(
        still_there, victim.agent_id,
        "the run must still belong to the agent it was created for"
    );

    let file_count: i64 = sqlx::query_scalar("select count(*) from ai_agent_files where agent_id = $1")
        .bind(victim.agent_id)
        .fetch_one(harness.db.pool())
        .await
        .expect("the workspace must still be countable");
    assert_eq!(
        file_count, 1,
        "the refused DELETE must not have removed a row"
    );

    // ---- the control: a tenant can read its own ------------------------------------------------------
    //
    // Without this the suite could pass with every route answering 404 for everybody, which is
    // the failure a "refuses the foreign id" assertion cannot see.
    let own = harness
        .call(get(
            &format!(
                "/api/v1/ai/agents/{}/files?organization_id={}",
                victim.agent_id, victim.organization
            ),
            Some(&victim.session),
        ))
        .await;
    assert_eq!(
        own.status, StatusCode::OK,
        "the control: a tenant must still read its own workspace: {:?}",
        own.body
    );
    assert_eq!(
        own.body["files"].as_array().map(Vec::len),
        Some(1),
        "the control: the owner's own listing has the file it uploaded: {:?}",
        own.body
    );

    harness.dispose().await;
}

/// A path in a URL, encoded. A space in a filename is a `400` from the router rather than a
/// verdict from the route, and a walk that cannot tell those apart reports the wrong defect.
fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}
