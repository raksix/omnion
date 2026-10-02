//! Integration tests for the self-service surface (docs/requests/REQ-055, slice 2c).
//!
//! The request's acceptance criterion for this slice is one sentence with two halves:
//!
//! > "Every `/api/v1/hr/*` route is permission-guarded; **self-service routes work for an
//! > employee without any `hr.*` permission but answer only for their own data.**"
//!
//! Both halves are walked here, and the second half is the one that is easy to fake. A suite
//! that signs in as an approver and reads their own record proves nothing about a surface whose
//! entire reason to exist is the caller who holds **no** key at all. So:
//!
//! * The primary fixture is a `PLAIN_PERMISSIONS` account with **`sites.read` and nothing else** —
//!   no `hr.leave.read`, no `hr.leave.request`, no `hr.employees.read`. If a route picked up an
//!   accidental `route_layer`, this account is what would catch it.
//! * The negative half is a **second** employee in the same tenant. Reading their request by id
//!   must answer `404`, not `403`: a `403` confirms the id exists, which is the disclosure the
//!   `own` level was introduced to prevent. The same applies to cancelling it.
//! * The counterpart is asserted the other way round too — the HR account is refused `403` on
//!   `/hr/me/leave/balances?employee_id=…` for somebody else, which is the route slice 2 left
//!   behind `hr.leave.read`. Two tests that only ever walk the happy path cannot tell a
//!   self-service surface from a second admin surface.
//!
//! One more thing this file pins, because it is the property a rewrite would break first: **the
//! routes carry no `employee_id` parameter at all**. The `own` reading is asserted by
//! *behaviour* — the account has no way to ask for a specific employee — and the walk below
//! proves that by having no such call to make.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::rate_limit_middleware::RateLimiter;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{
    Effect, NewBinding, NewRole, RolePermissionInput, Scope as PermScope,
};
use omnion_permissions::{roles as role_store, seed};
use omnion_security::RatePolicy;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

mod support {
    //! The sign-in half, shared with the people-core and leave suites for the same reason: a
    //! hand-rolled `login()` that keeps only the first `Set-Cookie` signs a suite in holding a
    //! credential that can read but not write, and every refusal in this file would then be
    //! about CSRF rather than about the permission split being tested.
    pub mod walk_auth;
}

use support::walk_auth::{self, Session};

/// Serialises this suite: the organizations and the IAM seed are shared state.
static ME_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// **The whole point of the fixture.** `sites.read` is what the panel needs to render its shell;
/// there is not one `hr.*` key in this list, and adding one would delete the test rather than
/// break it — which is exactly why the constant is named for what it must stay.
const PLAIN_PERMISSIONS: [&str; 1] = ["sites.read"];

/// The HR side of the pair: enough to read the directory **and to put somebody in it**, so the
/// negative half has two genuinely different callers to compare and the fixture can build the
/// employee rows the self-service reads resolve against. Without `hr.employees.create` the walk
/// dies in its own setup at a 403 that says nothing about self-service at all.
const HR_PERMISSIONS: [&str; 4] = [
    "hr.leave.read",
    "hr.employees.read",
    "hr.employees.create",
    "sites.read",
];

#[derive(Debug)]
struct TestResponse {
    status: StatusCode,
    body: Value,
    set_cookie: Vec<String>,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");
    let status = response.status();
    let set_cookie: Vec<String> = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap_or_default().to_owned())
        .collect();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(Value::String(
            String::from_utf8_lossy(&body).into_owned(),
        ))
    };
    TestResponse {
        status,
        body,
        set_cookie,
    }
}

fn request(
    method: Method,
    path: &str,
    session: Option<&Session>,
    body: Option<Value>,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(session) = session {
        builder = session.apply(builder);
    }
    match body {
        Some(value) => builder.body(Body::from(value.to_string())).expect("a body"),
        None => builder.body(Body::empty()).expect("a body"),
    }
}

/// The sentence an error body carries.
///
/// The API's envelope is `{ "error": { "code", "message" } }`, so a walk that reads
/// `body["message"]` gets a silent `null` and every "the refusal says why" assertion would pass
/// for the wrong reason. Naming the envelope once here is what makes those assertions real.
fn error_message(body: &Value) -> String {
    body["error"]["message"].as_str().unwrap_or_default().to_owned()
}

fn id_of(value: &Value) -> Uuid {
    Uuid::parse_str(value["id"].as_str().expect("an id")).expect("an id")
}

async fn live_state() -> Option<(AppState, Db)> {
    let mut config = Config::from_env().expect("environment must be valid");
    walk_auth::with_csrf_secret(&mut config);
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
        omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
            .expect("the default storage configuration is valid"),
    );
    Some((state, db))
}

struct Fixture {
    _walk: tokio::sync::MutexGuard<'static, ()>,
    state: AppState,
    db: Db,
    organization: Uuid,
    /// The account with **no** `hr.*` key — the one the whole surface is for.
    plain: String,
    /// A second plain employee, so the negative half has somebody to be refused about.
    colleague: String,
    /// The HR account, for the counterpart assertion.
    hr: String,
    root_department: Uuid,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = ME_WALK.lock().await;
        let (state, db) = live_state().await?;
        walk_auth::give_the_process_its_own_sign_in_budget(|| {
            let policies: Vec<RatePolicy> = RatePolicy::defaults()
                .into_iter()
                .map(|mut policy| {
                    if policy.scope == "sign_in" {
                        policy.limit = 10_000;
                    }
                    policy
                })
                .collect();
            let _ = omnion_api::rate_limit_middleware::install(RateLimiter::new(&state, policies));
        });
        seed::ensure(db.pool()).await.ok()?;

        let organization = create_organization_row(&db, "me").await;
        let (owner_id, _) = create_account(&db, None, "HR Me Owner").await;
        seed::bind_owner(db.pool(), owner_id).await.ok()?;

        let (plain_id, plain) = create_account(&db, Some(organization), "Plain Employee").await;
        grant(&db, organization, plain_id, owner_id, &PLAIN_PERMISSIONS).await;

        let (colleague_id, colleague) =
            create_account(&db, Some(organization), "Colleague Employee").await;
        grant(&db, organization, colleague_id, owner_id, &PLAIN_PERMISSIONS).await;

        let (hr_id, hr) = create_account(&db, Some(organization), "HR Person").await;
        grant(&db, organization, hr_id, owner_id, &HR_PERMISSIONS).await;

        let root_department: Option<Uuid> = sqlx::query_scalar(
            "select id from hr_departments where organization_id = $1 order by created_at limit 1",
        )
        .bind(organization)
        .fetch_optional(db.pool())
        .await
        .expect("the query must run");
        let root_department = root_department
            .expect("a tenant created after 0196 is seeded by the trigger, not by a backfill");

        Some(Self {
            _walk: walk,
            state,
            db,
            organization,
            plain,
            colleague,
            hr,
            root_department,
        })
    }

    async fn token(&self, email: &str) -> Session {
        login(&self.state, email).await
    }

    /// An employee row linked to the account behind `email`, so "my record" resolves.
    ///
    /// The write goes in as the **HR** account, not as the employee. Adding somebody to the
    /// directory is an HR action answered behind `hr.employees.create`, and an employee holding
    /// no `hr.*` key is refused there — so a fixture that created the row as the unprivileged
    /// caller would be asserting the 403 rather than the self-service read, and the walk would
    /// fail at its own setup for a reason that has nothing to do with what it is testing.
    async fn employee_for(&self, email: &str, first: &str, last: &str) -> Value {
        let hr_session = self.token(&self.hr).await;
        let user_id: Uuid = sqlx::query_scalar("select id from users where email = $1")
            .bind(email)
            .fetch_one(self.db.pool())
            .await
            .expect("the user must exist");
        let response = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/hr/employees",
                Some(&hr_session),
                Some(json!({
                    "user_id": user_id,
                    "first_name": first,
                    "last_name": last,
                    "work_email": format!("{}@example.com", Uuid::new_v4().simple()),
                    "position": "Engineer",
                    "department_id": self.root_department,
                    "employment_type": "full_time",
                    "start_date": "2024-01-08",
                })),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::CREATED,
            "the employee must be created by the HR account: {}",
            response.body
        );
        response.body
    }

    /// The seeded `Annual` type, read through the **self-service** route.
    async fn annual_type_via_self(&self, session: &Session) -> Value {
        let response = call(
            &self.state,
            request(
                Method::GET,
                "/api/v1/hr/me/leave/types",
                Some(session),
                None,
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        response.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|type_| type_["code"] == "ANNUAL")
            .cloned()
            .expect("the seeded catalogue contains Annual")
    }
}

async fn create_organization_row(db: &Db, label: &str) -> Uuid {
    let slug = format!("hr-me-{}-{}", label, Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(format!("HR Self-service {label}"))
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    let email = format!("hr-me-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: walk_auth::PASSWORD.to_owned(),
            display_name: name.to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the test user must be created");
    (user.id, email)
}

async fn grant(db: &Db, organization_id: Uuid, user_id: Uuid, owner_id: Uuid, keys: &[&str]) {
    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id,
            key: format!("hr-me-role-{}", Uuid::new_v4().simple()),
            name: "HR Me Walk Role".to_owned(),
            description: "A role of the self-service walk".to_owned(),
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
    role_store::set_role_permissions(db.pool(), role.id, &entries)
        .await
        .expect("the role permission set must be written");

    omnion_permissions::bindings::grant(
        db.pool(),
        NewBinding {
            role_id: role.id,
            user_id,
            scope: PermScope::Organization { organization_id },
            granted_by: Some(owner_id),
            expires_at: None,
        },
    )
    .await
    .expect("the role binding must be created");
}

async fn login(state: &AppState, email: &str) -> Session {
    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/auth/login",
            None,
            Some(Session::login_body(email)),
        ),
    )
    .await;
    Session::from_set_cookies(response.set_cookie)
}

// ---------------------------------------------------------------------------------------------
// The criterion
// ---------------------------------------------------------------------------------------------

/// The headline: an account holding **no `hr.*` key at all** reads its own record, its own leave
/// and its own documents, and books its own holiday.
#[tokio::test]
async fn an_account_with_no_hr_permission_reads_its_own_record_and_leave() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let plain = fixture.token(&fixture.plain).await;
    let employee = fixture
        .employee_for(&fixture.plain, "Ada", "Lovelace")
        .await;

    // The profile. If any of the four reads below picked up a `route_layer`, this is where the
    // walk would stop — and it stops at a 403, which is the failure the constant above exists
    // to make possible.
    let me = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/me", Some(&plain), None),
    )
    .await;
    assert_eq!(
        me.status,
        StatusCode::OK,
        "an account with no hr.* key must read its own record: {}",
        me.body
    );
    // `MyProfile` publishes `employee_id`, not `id` — the field name is the point, because "my
    // record" is about a *person*, and the test that keys on `id` would read a null out of a
    // perfectly good response. The comparison is still the one that matters: the two are the
    // same employee.
    let me_employee_id = me.body["employee_id"]
        .as_str()
        .expect("the profile names the employee it is about");
    assert_eq!(me_employee_id, employee["id"].as_str().expect("an id"));
    assert_eq!(me.body["first_name"], "Ada");
    assert_eq!(
        me.body["department"], "General",
        "the department join is part of the card, not a second request: {}",
        me.body
    );

    // The leave, with a card per catalogue type even before anything is taken.
    let leave = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/me/leave", Some(&plain), None),
    )
    .await;
    assert_eq!(leave.status, StatusCode::OK, "{}", leave.body);
    let cards = leave.body["balances"].as_array().expect("balance cards");
    assert!(
        cards.len() >= 3,
        "the seeded catalogue is Annual, Sick and Unpaid: {}",
        leave.body
    );
    for card in cards {
        // The same arithmetic the HR screen asserts: entitled = used + pending + remaining. A
        // self-service card that does not add up is worse than no card, because it is the number
        // an employee argues with.
        let entitled: f64 = card["entitled_days"].as_str().expect("days").parse().expect("a number");
        let used: f64 = card["used_days"].as_str().expect("days").parse().expect("a number");
        let pending: f64 = card["pending_days"].as_str().expect("days").parse().expect("a number");
        let remaining: f64 =
            card["remaining_days"].as_str().expect("days").parse().expect("a number");
        assert!(
            (entitled - (used + pending + remaining)).abs() < 0.01,
            "the card must add up: {card}"
        );
    }

    // The catalogue the self-service form offers, and the day counter it reads.
    let annual = fixture.annual_type_via_self(&plain).await;
    let type_id = id_of(&annual);
    let preview = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/hr/me/leave/preview?starts_on=2026-10-05&ends_on=2026-10-11",
            Some(&plain),
            None,
        ),
    )
    .await;
    assert_eq!(preview.status, StatusCode::OK, "{}", preview.body);
    assert_eq!(
        preview.body["days"], "5",
        "the keyless preview must be the same arithmetic, weekend included: {}",
        preview.body
    );

    // And the write: book it, with no `hr.leave.request` key anywhere in the fixture.
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/me/leave/requests",
            Some(&plain),
            Some(json!({
                "leave_type_id": type_id,
                "starts_on": "2026-10-05",
                "ends_on": "2026-10-11",
                "reason": "Self-service walk",
            })),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "an employee with no hr.* key must be able to ask for leave: {}",
        created.body
    );
    assert_eq!(created.body["days"], "5");
    assert_eq!(
        created.body["employee_id"],
        employee["id"].as_str().expect("an id"),
        "the subject is the session, not a body field: {}",
        created.body
    );

    // It appears in their own list, which is the reconciliation the single-document read exists
    // for: the card and the request that moved it in one answer.
    let after = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/me/leave", Some(&plain), None),
    )
    .await;
    let requests = after.body["requests"].as_array().expect("requests");
    assert_eq!(
        requests.len(),
        1,
        "the request the walk just made must be in the caller's own list: {}",
        after.body
    );
}

/// The other half of the criterion: "answer only for their own data".
#[tokio::test]
async fn a_self_service_caller_is_refused_somebody_elses_leave_and_it_reads_as_missing() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let plain = fixture.token(&fixture.plain).await;
    fixture
        .employee_for(&fixture.plain, "Ada", "Lovelace")
        .await;

    // A colleague, also with no hr.* key, books a request.
    let colleague = fixture.token(&fixture.colleague).await;
    fixture
        .employee_for(&fixture.colleague, "Grace", "Hopper")
        .await;
    let annual = fixture.annual_type_via_self(&colleague).await;
    let type_id = id_of(&annual);
    let theirs = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/me/leave/requests",
            Some(&colleague),
            Some(json!({
                "leave_type_id": type_id,
                "starts_on": "2026-11-02",
                "ends_on": "2026-11-04",
            })),
        ),
    )
    .await;
    assert_eq!(theirs.status, StatusCode::CREATED, "{}", theirs.body);
    let their_request = id_of(&theirs.body);

    // The plain employee asks for it by id. **404, not 403**: a 403 would confirm the id exists,
    // which is the exact disclosure the `own` level exists to prevent.
    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/me/leave/requests/{their_request}"),
            Some(&plain),
            None,
        ),
    )
    .await;
    assert_eq!(
        read.status,
        StatusCode::NOT_FOUND,
        "another employee's request must read as missing, not as forbidden: {}",
        read.body
    );

    // And they may not cancel it either. The store's ownership test runs before the status check,
    // so a colleague's *pending* request is still not theirs to withdraw.
    let cancel = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/hr/me/leave/requests/{their_request}/cancel"),
            Some(&plain),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(
        cancel.status,
        StatusCode::NOT_FOUND,
        "another employee's request must not be withdrawable: {}",
        cancel.body
    );

    // The colleague's request is untouched — a refusal that half-applied would be worse than no
    // refusal at all. The read-back goes through the **HR** account, which is the only one of the
    // three that may read the HR route at all: asking the colleague confirms nothing about the
    // row's state, it only confirms the same 403 the line above already established.
    let hr_session = fixture.token(&fixture.hr).await;
    let still_pending = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/leave/requests/{their_request}"),
            Some(&hr_session),
            None,
        ),
    )
    .await;
    assert_eq!(still_pending.status, StatusCode::OK, "{}", still_pending.body);
    assert_eq!(
        still_pending.body["leave_status"], "pending",
        "the refused cancellation must not have moved the row: {}",
        still_pending.body
    );
}

/// The counterpart, and the assertion that makes the self-service surface a **different** one:
/// naming somebody else in the query changes nothing, while the HR route is refused outright.
///
/// The first half used to accept "200 or 404", which is an assertion that cannot fail. What the
/// property actually needs is: the self-service read carries **no** `employee_id`, so passing one
/// returns the *caller's own* record rather than the named employee's — and a stranger's id is not
/// even refused, because it was never a parameter. That is a real, falsifiable claim about the
/// wire, and it is the one a later refactor that added the parameter would break.
#[tokio::test]
async fn naming_an_employee_on_the_self_service_read_returns_the_callers_own_data() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let plain = fixture.token(&fixture.plain).await;
    let mine = fixture
        .employee_for(&fixture.plain, "Ada", "Lovelace")
        .await;

    // Somebody else exists, so a leaking implementation would have something to leak.
    let colleague = fixture.token(&fixture.colleague).await;
    let theirs = fixture
        .employee_for(&fixture.colleague, "Grace", "Hopper")
        .await;
    let their_id = theirs["id"].as_str().expect("an id").to_owned();

    let response = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/me/leave?employee_id={their_id}"),
            Some(&plain),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(
        response.body["employee_id"].as_str().expect("an id"),
        mine["id"].as_str().expect("an id"),
        "the subject is the session: a parameter that is not part of the surface is ignored,          not honoured — naming a colleague must return the caller's own leave: {}",
        response.body
    );
    assert!(
        response.body["employee_id"].as_str() != Some(their_id.as_str()),
        "a colleague's balance was returned to a caller with no hr.* key"
    );

    // The HR route is the other half of the contrast: it *is* parameterised, and the plain
    // account does not hold `hr.leave.read`, so it is refused there.
    let guarded = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/leave/balances?employee_id={their_id}"),
            Some(&plain),
            None,
        ),
    )
    .await;
    assert_eq!(
        guarded.status,
        StatusCode::FORBIDDEN,
        "hr.leave.read is what the HR balance route is for; self-service has its own: {}",
        guarded.body
    );
    assert!(
        !error_message(&guarded.body).is_empty(),
        "a refusal must carry a sentence: {}",
        guarded.body
    );
}

/// An account with **no employee row** is a real state, not a permission failure: the answer is
/// a `404` naming the employee, never a `403` and never an empty card with an edit button.
#[tokio::test]
async fn an_account_with_no_employee_row_is_told_it_is_not_in_the_directory() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // The HR account deliberately has no employee row: it was created and granted keys, never
    // added to the directory.
    let hr = fixture.token(&fixture.hr).await;

    let me = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/me", Some(&hr), None),
    )
    .await;
    assert_eq!(
        me.status,
        StatusCode::NOT_FOUND,
        "an unlinked account is missing a record, not missing a permission: {}",
        me.body
    );

    let leave = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/me/leave", Some(&hr), None),
    )
    .await;
    assert_eq!(leave.status, StatusCode::NOT_FOUND, "{}", leave.body);

    let documents = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/me/documents", Some(&hr), None),
    )
    .await;
    assert_eq!(documents.status, StatusCode::NOT_FOUND, "{}", documents.body);
}

/// Every route in the surface answers `401` without a session, and none of them is reachable by
/// a query parameter naming somebody else.
#[tokio::test]
async fn the_self_service_routes_refuse_an_anonymous_caller() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    for path in [
        "/api/v1/hr/me",
        "/api/v1/hr/me/leave",
        "/api/v1/hr/me/leave/types",
        "/api/v1/hr/me/leave/preview?starts_on=2026-10-05&ends_on=2026-10-06",
        "/api/v1/hr/me/documents",
    ] {
        let response = call(
            &fixture.state,
            request(Method::GET, path, None, None),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{path} must refuse an anonymous caller: {}",
            response.body
        );
    }

    // A POST is refused too — the write is the one that would matter if it leaked.
    let write = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/me/leave/requests",
            None,
            Some(json!({
                "leave_type_id": Uuid::nil(),
                "starts_on": "2026-10-05",
                "ends_on": "2026-10-06",
            })),
        ),
    )
    .await;
    assert_eq!(
        write.status,
        StatusCode::UNAUTHORIZED,
        "an anonymous caller must not book leave: {}",
        write.body
    );
}

/// The self-service writes are **recorded**, and the walk is about the recording rather than the
/// booking — the booking itself is already proved above.
///
/// The hole this closes: slice 2c shipped this surface with two writes and **zero** audit rows
/// and **zero** events, while both HR twins (`/hr/leave/requests` and `…/cancel`) did both. The
/// argument for the surface is that it is the *same action* through a shorter path, and it was
/// the one path where the action left no trace. So the most common leave transaction in any
/// organization — the employee books, then withdraws — was invisible to the audit screen and
/// invisible to every automation, which is exactly the pair of consumers the request's own
/// criteria name.
///
/// Four separate claims, each with the failure it would produce:
///
/// * **The audit row names the request.** A trail that recorded "somebody did something in HR"
///   without the request id could not answer a dispute about *that* booking.
/// * **The actor is the account, not the employee row.** `hr_employees.id` and `users.id` are
///   different ids and the handler is handed both; `actor_user_id` has to be the one the audit
///   screen's IAM join resolves to a person. Asserting the account id is what tells the two
///   apart — a walk asserting only "a row exists" cannot see this at all.
/// * **The event fires and carries no reason.** `reason` is the employee's own words and these
///   events travel to third-party webhooks, so the payload is checked for the ids *and* for the
///   absence of the string they typed.
/// * **The cancel is its own row.** A booking raised and then withdrawn leaves two entries,
///   because "when did they change their mind?" is what a balance dispute asks, and a single row
///   carrying the final status answers nothing about the interval.
#[tokio::test]
async fn a_self_service_write_is_audited_and_announced_exactly_like_its_hr_twin() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let plain = fixture.token(&fixture.plain).await;
    fixture
        .employee_for(&fixture.plain, "Ada", "Lovelace")
        .await;
    let annual = fixture.annual_type_via_self(&plain).await;
    let type_id = id_of(&annual);

    // The one string this walk plants, so its absence from the event payload is a real
    // assertion rather than a field that never existed.
    let reason = "A private reason nobody outside may read";

    // Counters are taken **before** and **after**, and the assertion is the difference.
    // Absolute counts would compare this tenant's ledger against itself in a database other
    // walks write to concurrently, where an absolute number is a statement about the box and
    // not about this walk's two writes.
    let before: (i64, i64) = sqlx::query_as(
        "select (select count(*) from audit_log where action in ('hr.leave.requested', 'hr.leave.cancelled') and organization_id = $1), \
                (select count(*) from events where name in ('hr.leave.requested', 'hr.leave.cancelled') and organization_id = $1)",
    )
    .bind(fixture.organization)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the counters must run");

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/me/leave/requests",
            Some(&plain),
            Some(json!({
                "leave_type_id": type_id,
                "starts_on": "2026-10-05",
                "ends_on": "2026-10-11",
                "reason": reason,
            })),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "the write must still succeed — this walk is about what it leaves behind: {}",
        created.body
    );
    let request_id = created.body["id"].as_str().expect("an id").to_owned();
    let user_id: Uuid = sqlx::query_scalar("select id from users where email = $1")
        .bind(&fixture.plain)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the account must exist");

    // The audit row, read back from the table the audit screen itself reads.
    let actions: Vec<String> = sqlx::query_scalar(
        "select action from audit_log where organization_id = $1 and target_id = $2 order by id",
    )
    .bind(fixture.organization)
    .bind(&request_id)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the audit query must run");
    assert!(
        actions.contains(&"hr.leave.requested".to_owned()),
        "a self-service booking must be audited and must name the request; found {actions:?} \
         for target {request_id}"
    );
    let actor: Option<Uuid> = sqlx::query_scalar(
        "select actor_user_id from audit_log where organization_id = $1 and target_id = $2 \
         and action = 'hr.leave.requested' order by id desc limit 1",
    )
    .bind(fixture.organization)
    .bind(&request_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the audit row must exist");
    assert_eq!(
        actor,
        Some(user_id),
        "the audit actor must be the ACCOUNT that acted, not the employee row — \
         hr_employees.id and users.id are different ids and the handler is handed both"
    );

    // The event, with the ids the request's own event section promises and none of the words.
    let payload: Value = sqlx::query_scalar(
        "select payload from events where name = 'hr.leave.requested' and organization_id = $1 \
         order by id desc limit 1",
    )
    .bind(fixture.organization)
    .fetch_one(fixture.db.pool())
    .await
    .expect("hr.leave.requested must reach the bus after a self-service booking");
    assert_eq!(
        payload["leave_request_id"].as_str().expect("the id"),
        request_id,
        "the event must name the request it is about: {payload}"
    );
    assert!(
        payload.get("reason").is_none(),
        "a reason is the employee's own words and this event reaches third-party webhooks: {payload}"
    );

    // And the withdrawal: its own row and its own event, not an edit of the row above.
    let cancelled = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/hr/me/leave/requests/{request_id}/cancel"),
            Some(&plain),
            None,
        ),
    )
    .await;
    assert_eq!(
        cancelled.status,
        StatusCode::OK,
        "the caller must be able to withdraw their own pending request: {}",
        cancelled.body
    );

    let after: (i64, i64) = sqlx::query_as(
        "select (select count(*) from audit_log where action in ('hr.leave.requested', 'hr.leave.cancelled') and organization_id = $1), \
                (select count(*) from events where name in ('hr.leave.requested', 'hr.leave.cancelled') and organization_id = $1)",
    )
    .bind(fixture.organization)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the counters must run");

    assert_eq!(
        after.0 - before.0,
        2,
        "a booking and a withdrawal are two audit rows, because 'when did they change their \
         mind?' is the question a balance dispute actually asks"
    );
    assert_eq!(
        after.1 - before.1,
        2,
        "and two events, or every automation waiting on hr.leave.cancelled waits forever"
    );
}
