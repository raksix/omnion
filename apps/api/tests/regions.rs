//! Integration tests for the edge region registry (REQ-035, slice 1).
//!
//! The twelve unit tests in `omnion-regions` prove the *decisions* — that an empty check
//! set aggregates to `degraded` and not to `healthy`, that the de-bounce holds a region
//! still, that a code the migration would refuse is refused here too. What a pure crate
//! cannot prove is the four things this slice is actually for, and they are worth naming
//! because each of them is a way the feature can be *green* and still wrong:
//!
//!   * **The registry is seeded and reachable.** A migration that creates `regions` and
//!     forgets the `insert` produces a panel with an empty state and no error, which looks
//!     exactly like a deployment that has not configured anything.
//!   * **Exactly one region is default, and the database refuses a second one.** The API
//!     clears the old default inside its transaction; what has to be proved is that the
//!     *constraint* also refuses a second, because the constraint is what a direct SQL
//!     insert, a migration or a future code path runs into.
//!   * **A stopped service moves the region, and an unchecked region is not green.** The
//!     aggregation is unit-proved, so what this adds is the wiring: checks recorded through
//!     the store reach the matrix, and a region with *no* rows at all renders `unknown`
//!     rather than disappearing from its own health row.
//!   * **The read and manage keys are separate.** An account holding only
//!     `platform.regions.read` must get `403` on the `PATCH` and `200` on the `GET`. This
//!     is the claim a merge is most likely to break, because collapsing two route guards
//!     into one is a one-character edit that passes every compile-time gate.
//!
//! Runs against the development stack and skips with a printed reason when PostgreSQL is
//! not reachable, like every other suite here.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store};
use omnion_regions::store::{self, NewCheck};
use omnion_regions::{Service, ServiceStatus};
use omnion_security::RatePolicy;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

mod support;

use support::walk_auth::{self, Session as Credentials};

const PASSWORD: &str = "correct horse battery";

struct TestResponse {
    status: StatusCode,
    body: Value,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
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

fn request(
    method: Method,
    uri: &str,
    credentials: Option<&Credentials>,
    body: Option<Value>,
) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    // `Session::apply` is the ONE place the two cookies and the CSRF header are attached, and
    // this suite uses it rather than rebuilding the header by hand: a hand-rolled version
    // that sends the cookie without the header is the exact shape the double-submit check
    // refuses, and it fails as `csrf_failed` — a code about the deployment rather than about
    // the request the walk actually made.
    let builder = match credentials {
        Some(credentials) => credentials.apply(builder),
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

async fn live_state() -> Option<(AppState, Db)> {
    let mut config = Config::from_env().ok()?;
    walk_auth::with_csrf_secret(&mut config);
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(err) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({err})");
            return None;
        }
    };
    db.migrate().await.expect("migrations must apply");
    // The catalogue is seeded from `omnion_permissions::CATALOGUE`, so a permission added
    // in this slice exists in code and in the role editor's list only AFTER this runs. The
    // omission is a specific and expensive failure: `role_permissions.permission_key` is a
    // foreign key, so creating the role fails with 23503 naming a key the developer can see
    // in `catalogue.rs` -- and the walk reports a *fixture* error for a permission that is
    // perfectly well catalogued.
    omnion_permissions::seed::ensure(db.pool())
        .await
        .expect("the permission catalogue must seed");
    let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
    let state = AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config,
        db.clone(),
        redis,
        test_storage(),
    );
    give_the_suite_its_own_rate_limit(&state);
    Some((state, db))
}

/// Raise the sign-in ceiling for this process only.
///
/// The limiter is a process-wide cell and the shipped `sign_in` policy allows ten attempts
/// per five minutes; each walk here signs two accounts in, so the surplus would land in the
/// middle of an assertion about regions as a `429 rate_limited`. Only the `sign_in` scope is
/// raised — the other ceilings are the ones a deployment ships.
fn give_the_suite_its_own_rate_limit(state: &AppState) {
    // The closure takes no arguments and captures `state`: the limiter is installed into a
    // process-wide cell keyed by nothing, so the *first* suite to install it wins for the
    // process. That is the helper's whole purpose -- twenty suites share one rate-limit
    // cell, and the sign_in budget is what a walk through nine fixtures spends.
    walk_auth::give_the_process_its_own_sign_in_budget(|| {
        // The RAISED budget is the caller's job, not the helper's: the helper only makes the
        // install happen once per process, and it deliberately does not know which scope a
        // given suite spends. Passing `RatePolicy::defaults()` straight through — which is
        // what this closure did on its first run — installs the SHIPPED ten-per-five-minutes
        // sign_in ceiling, and nine walks that sign in twice each then answer `429
        // rate_limited` in the middle of an assertion about regions. The `429` names the
        // client's budget, so it reads as a limiter problem rather than a fixture one.
        let policies: Vec<RatePolicy> = RatePolicy::defaults()
            .into_iter()
            .map(|mut policy| {
                if policy.scope == "sign_in" {
                    policy.limit = 10_000;
                }
                policy
            })
            .collect();
        let _ = omnion_api::rate_limit_middleware::install(
            omnion_api::rate_limit_middleware::RateLimiter::new(state, policies),
        );
    });
}

async fn create_organization_row(db: &Db, suffix: &str) -> Uuid {
    let name = format!("Regions {suffix}");
    let slug = format!("regions-{suffix}-{}", Uuid::new_v4().simple());
    let id: Uuid =
        sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
            .bind(&name)
            .bind(&slug)
            .fetch_one(db.pool())
            .await
            .expect("organization must insert");
    id
}

async fn create_admin(
    db: &Db,
    organization_id: Uuid,
    suffix: &str,
    state: &AppState,
    permissions: &[&str],
) -> (Uuid, Credentials) {
    let email = format!("regions-{suffix}-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Regions Admin".to_owned(),
            organization_id: Some(organization_id),
        },
    )
    .await
    .expect("account must insert");

    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id,
            key: format!("regions-admin-{suffix}"),
            name: format!("Regions Admin {suffix}"),
            description: "edge regions".to_owned(),
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

    let credentials = login(state, &email).await;
    (user.id, credentials)
}

async fn login(state: &AppState, email: &str) -> Credentials {
    // The credentials come out of the `Set-Cookie` HEADERS, not the JSON body. The first
    // version of this helper read `body["token"]`, and every walk then answered
    // `401 unauthenticated` — the token in the body is the API-key material, while a session
    // cookie is a value the browser keeps and the body does not carry. Nine red walks, and
    // the shape of the mistake is the same one twenty suites made before this module existed:
    // reading the credential from somewhere the platform does not put it.
    let response = routes::router(state.clone())
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(walk_auth::Session::login_body(email).to_string()))
                .expect("the sign-in request must build"),
        )
        .await
        .expect("the router must answer sign-in");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "sign-in for {email} must succeed"
    );
    Credentials::from_set_cookies(
        response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap_or_default().to_owned()),
    )
}

const BOTH: [&str; 2] = ["platform.regions.read", "platform.regions.manage"];
const READ_ONLY: [&str; 1] = ["platform.regions.read"];

struct Fixture {
    state: AppState,
    db: Db,
    organization: Uuid,
    manager: Credentials,
    reader: Credentials,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        let organization = create_organization_row(&db, "w5").await;
        let (_, manager) = create_admin(&db, organization, "manager", &state, &BOTH).await;
        let (_, reader) = create_admin(&db, organization, "reader", &state, &READ_ONLY).await;
        Some(Self {
            state,
            db,
            organization,
            manager,
            reader,
        })
    }

    /// One authenticated call. Async, like every other suite here: a sync wrapper around an
    /// async router needs a nested runtime, and a nested `block_on` on a current-thread
    /// runtime is a deadlock — the failure mode of the clever version is a hang, which is
    /// the worst thing a test can do to a box seven writers share.
    async fn send(
        &self,
        method: Method,
        uri: &str,
        credentials: &Credentials,
        body: Option<Value>,
    ) -> TestResponse {
        call(&self.state, request(method, uri, Some(credentials), body)).await
    }

    async fn get(&self, uri: &str, credentials: &Credentials) -> TestResponse {
        self.send(Method::GET, uri, credentials, None).await
    }

    async fn patch(&self, uri: &str, credentials: &Credentials, body: Value) -> TestResponse {
        self.send(Method::PATCH, uri, credentials, Some(body)).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_registry_is_seeded_with_the_three_regions_the_brief_names() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    let response = fixture.get("/api/v1/regions", &fixture.manager).await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);

    let regions = response.body["regions"].as_array().expect("regions must be an array");
    let codes: Vec<&str> = regions
        .iter()
        .filter_map(|r| r["code"].as_str())
        .collect();
    for expected in ["tr-ankara", "eu-frankfurt", "us-virginia"] {
        assert!(
            codes.contains(&expected),
            "{expected} must be seeded; got {codes:?}"
        );
    }

    // The default is FIRST, not merely present: the list read orders by `is_default desc`,
    // and a panel that renders the default last makes the operator hunt for it.
    assert_eq!(
        regions[0]["code"], "tr-ankara",
        "the default region must sort first"
    );
    assert_eq!(regions[0]["is_default"], json!(true));
    assert_eq!(response.body["multi_region_active"], json!(true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exactly_one_region_is_default_and_the_database_refuses_a_second() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let pool = fixture.db.pool();

    // The API's own transaction clears the old default, so this is the *application* half.
    let response = fixture.patch(
        "/api/v1/regions/eu-frankfurt",
        &fixture.manager,
        json!({ "is_default": true }),
    ).await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);

    let (count,): (i64,) =
        sqlx::query_as("select count(*) from regions where is_default")
            .fetch_one(pool)
            .await
            .expect("the count must read");
    assert_eq!(count, 1, "promoting a second region must clear the first");

    // The *constraint* half, and the one that matters for everything this API does not own:
    // a migration, a restore, or a future code path that writes the column directly. Without
    // this, "the API keeps the invariant" is a claim about three handlers.
    let direct = sqlx::query("update regions set is_default = true where code = 'us-virginia'")
        .execute(pool)
        .await;
    assert!(
        direct.is_err(),
        "the partial unique index must refuse a second default written outside the API"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_region_with_no_health_data_is_unknown_and_never_green() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    // Delete every check so the region's row is genuinely empty rather than merely old.
    sqlx::query("delete from region_health_checks")
        .execute(fixture.db.pool())
        .await
        .expect("the delete must apply");

    let response = fixture.get("/api/v1/regions", &fixture.manager).await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);

    // The matrix still has a cell for every region × service — the grid is built from
    // `regions`, not from the checks, so a region cannot vanish from its own health row.
    let cells = response.body["health"]["cells"]
        .as_array()
        .expect("cells must be an array");
    let region_count = response.body["regions"].as_array().unwrap().len();
    assert_eq!(
        cells.len(),
        region_count * 7,
        "the matrix is regions x seven services, including the unchecked ones"
    );

    let stale: Vec<&str> = response.body["health"]["stale_region_codes"]
        .as_array()
        .expect("stale_region_codes must be an array")
        .iter()
        .filter_map(|c| c.as_str())
        .collect();
    assert!(
        stale.contains(&"tr-ankara"),
        "a region with no checks must be listed as stale; got {stale:?}"
    );

    // The claim the REQ makes, in the panel's own vocabulary: `unknown`, not green. Asserted
    // as a *string* rather than a boolean because the failure this prevents is a panel that
    // renders an empty badge, and `degraded` would also be "not green" while being a lie
    // about a service nobody has asked about.
    for cell in cells {
        assert_eq!(
            cell["status"], "unknown",
            "an unchecked service must read unknown, not {}",
            cell["status"]
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopping_service_moves_the_region_within_the_threshold() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let pool = fixture.db.pool();
    // A region of this walk's own, so the walk does not depend on the seeded rows' state —
    // another suite's check rows in the same database would otherwise decide this test.
    let code = format!("tr-w5-{}", &Uuid::new_v4().simple().to_string()[..8]);
    sqlx::query(
        "insert into regions (code, display_name, country_group, status, api_endpoint, \
             storage_bucket, cache_namespace) \
         values ($1, $2, 'tr', 'healthy', $3, $4, $1)",
    )
    .bind(&code)
    .bind("W5 walk region")
    .bind(format!("api.{code}.omnion.test"))
    .bind(format!("bucket-{code}"))
    .execute(pool)
    .await
    .expect("the walk's own region must insert");

    let healthy = |service: Service| NewCheck {
        region_code: code.clone(),
        service,
        status: ServiceStatus::Healthy,
        latency_ms: Some(12),
        detail: json!({}),
    };

    // One healthy round for every service, so the region starts green.
    for service in Service::ALL {
        store::record_check(pool, &healthy(service)).await.expect("a check must record");
    }
    let response = fixture.get("/api/v1/regions", &fixture.manager).await;
    let view = response.body["regions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["code"] == json!(code))
        .expect("the walk's region must appear in the list");
    assert_eq!(view["effective_status"], "healthy", "body: {}", view);

    // Three CONSECUTIVE failures is the threshold. One is not enough, and that asymmetry is
    // the de-bounce the REQ's risks section asks for — so both halves are asserted here
    // rather than only the end state, because a fixture that writes three rows at once would
    // pass against a rule that had no threshold at all.
    store::record_check(
        pool,
        &NewCheck {
            region_code: code.clone(),
            service: Service::Api,
            status: ServiceStatus::Down,
            latency_ms: None,
            detail: json!({ "why": "connection refused" }),
        },
    )
    .await
    .expect("a check must record");
    let response = fixture.get("/api/v1/regions", &fixture.manager).await;
    let view = response.body["regions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["code"] == json!(code))
        .expect("the walk's region must appear in the list");
    assert_eq!(
        view["effective_status"], "degraded",
        "one missed check must degrade, not fail over"
    );

    for _ in 0..2 {
        store::record_check(
            pool,
            &NewCheck {
                region_code: code.clone(),
                service: Service::Api,
                status: ServiceStatus::Down,
                latency_ms: None,
                detail: json!({ "why": "connection refused" }),
            },
        )
        .await
        .expect("a check must record");
    }
    let response = fixture.get("/api/v1/regions", &fixture.manager).await;
    let view = response.body["regions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["code"] == json!(code))
        .expect("the walk's region must appear in the list");
    assert_eq!(
        view["effective_status"], "down",
        "three consecutive failures must move the region to down"
    );
    // And the derived status is reported *next to* the stored one, so an operator can see
    // that the checker moved it rather than a person.
    assert_eq!(view["derived_status"], "down");
    assert_eq!(view["status"], "healthy", "the stored status is unchanged by a check");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reading_regions_and_changing_them_are_different_permissions() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    let read = fixture.get("/api/v1/regions", &fixture.reader).await;
    assert_eq!(
        read.status,
        StatusCode::OK,
        "an account holding the read key must see the registry: {}",
        read.body
    );

    let write = fixture.patch(
        "/api/v1/regions/tr-ankara",
        &fixture.reader,
        json!({ "display_name": "Renamed by an account that may not" }),
    ).await;
    assert_eq!(
        write.status,
        StatusCode::FORBIDDEN,
        "the read key must not reach the manage route: {}",
        write.body
    );
    // The refusal must name the permission, because "403" with no reason is the one answer
    // that leaves the operator with nothing to act on.
    let message = write.body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("platform.regions.manage"),
        "the refusal must name the missing key; got {message:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_region_cannot_be_marked_healthy_by_hand() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    // Marking `degraded` is legitimate — an operator recording an incident by hand.
    let degrade = fixture.patch(
        "/api/v1/regions/tr-ankara",
        &fixture.manager,
        json!({ "status": "degraded" }),
    ).await;
    assert_eq!(degrade.status, StatusCode::OK, "body: {}", degrade.body);

    // Marking `healthy` is not: the checker owns that word, and a hand-painted green on a
    // region whose database is down is precisely the failure this slice is about.
    let heal = fixture.patch(
        "/api/v1/regions/tr-ankara",
        &fixture.manager,
        json!({ "status": "healthy" }),
    ).await;
    assert_eq!(
        heal.status,
        StatusCode::FORBIDDEN,
        "healthy is the checker's word, not an operator's: {}",
        heal.body
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_region_or_a_patch_that_changes_nothing_is_refused_named() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    let missing = fixture.get("/api/v1/regions/xx-nowhere", &fixture.manager).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "body: {}", missing.body);
    assert!(
        missing.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("xx-nowhere"),
        "the 404 must name the code that was looked for"
    );

    let empty = fixture.patch(
        "/api/v1/regions/tr-ankara",
        &fixture.manager,
        json!({}),
    ).await;
    assert_eq!(
        empty.status,
        StatusCode::BAD_REQUEST,
        "a patch that changes nothing must say so rather than succeed: {}",
        empty.body
    );

    // And a name outside the length rule names its FIELD, so the panel can put the message
    // under the input rather than in a banner above a form that has one.
    let short = fixture.patch(
        "/api/v1/regions/tr-ankara",
        &fixture.manager,
        json!({ "display_name": "x" }),
    ).await;
    assert_eq!(short.status, StatusCode::BAD_REQUEST, "body: {}", short.body);
    assert_eq!(short.body["error"]["details"]["field"], "display_name");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_last_default_cannot_be_demoted() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    // Put the default back on the seeded region, then try to remove it.
    fixture.patch(
        "/api/v1/regions/tr-ankara",
        &fixture.manager,
        json!({ "is_default": true }),
    ).await;
    let demote = fixture.patch(
        "/api/v1/regions/tr-ankara",
        &fixture.manager,
        json!({ "is_default": false }),
    ).await;
    assert_eq!(
        demote.status,
        StatusCode::CONFLICT,
        "a registry with no default has nothing for routing to point at: {}",
        demote.body
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_is_audited_with_the_actor_and_the_before_and_after() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let before: String = sqlx::query_scalar("select display_name from regions where code = 'tr-ankara'")
        .fetch_one(fixture.db.pool())
        .await
        .expect("the seeded region must be readable");

    let renamed = format!("Ankara renamed {}", &Uuid::new_v4().simple().to_string()[..6]);
    let response = fixture.patch(
        "/api/v1/regions/tr-ankara",
        &fixture.manager,
        json!({ "display_name": renamed }),
    ).await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);
    assert_eq!(response.body["region"]["display_name"], json!(renamed));

    // The diff is the claim the REQ makes — "with actor and diff" — and it is read back out
    // of the audit table rather than from the response, because a response that echoes its
    // own input proves nothing about the log an auditor reads.
    let metadata: Value = sqlx::query_scalar(
        "select metadata from audit_log where action = 'region.updated' \
         order by created_at desc limit 1",
    )
    .fetch_one(fixture.db.pool())
    .await
    .expect("the change must be audited");
    let changes = metadata["changes"].as_array().expect("changes must be an array");
    let name_change = changes
        .iter()
        .find(|c| c["field"] == "display_name")
        .expect("the rename must appear in the diff");
    assert_eq!(name_change["from"], json!(before), "the diff must carry the previous value");
    assert_eq!(name_change["to"], json!(renamed));
}
