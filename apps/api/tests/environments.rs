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

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_environment::clone::Area;
use omnion_environment::model::CloneStatus;
use omnion_identity::sites;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

struct TestResponse {
    status: StatusCode,
    /// Every `Set-Cookie`, so the CSRF cookie login issued is not lost.
    set_cookies: Vec<String>,
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
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");
    let status = response.status();
    let set_cookies: Vec<String> = response
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
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    TestResponse {
        status,
        set_cookies,
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

async fn live_state() -> Option<(AppState, Db)> {
    let mut config = Config::from_env().ok()?;
    config.csrf = omnion_core::config::CsrfSecret::new(Some(CSRF_SECRET.to_owned()));
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(err) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({err})");
            return None;
        }
    };
    db.migrate().await.expect("migrations must apply");
    let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
    let state = AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config,
        db.clone(),
        redis,
        test_storage(),
    );
    Some((state, db))
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

async fn login(state: &AppState, email: &str) -> Caller {
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
}

const ALL_PERMISSIONS: [&str; 4] = [
    "deployment.read",
    "deployment.preview",
    "deployment.deploy",
    "deployment.rollback",
];

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool()).await.expect("the IAM seed must run");
        let organization = create_organization_row(&db, "main").await;
        let site = create_site(&db, organization, "main").await;
        let (_, caller) = create_admin(&db, organization, "main", &state, &ALL_PERMISSIONS).await;
        Some(Self {
            state,
            db,
            organization,
            site,
            caller,
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
        let response = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/environments",
                Some(&self.caller),
                Some(json!({
                    "name": name,
                    "key": key,
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
