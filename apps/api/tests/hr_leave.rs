//! Integration tests for leave (docs/requests/REQ-055, slice 2).
//!
//! The suite is written around the sentences the request's own acceptance criteria use, because a
//! test that asserts a status code is not asserting the sentence:
//!
//! * **"Leave days are computed from the organization's working days, half-days count as 0.5, and
//!   the number shown before submit equals the stored value."** So the walk asks the *preview*
//!   route what a range costs, submits exactly that range, and compares the preview with the
//!   stored `days` — and it does it for a range with a weekend in it, where the two
//!   implementations a form and a store each own would first disagree.
//! * **"An overlapping leave request of the same employee is refused with the conflicting dates."**
//!   The refusal is asserted on the *message*, because a refusal that says only "conflicts with
//!   another request" satisfies a status-code test and still sends the person to the list to guess.
//! * **"A request beyond the remaining balance is refused unless the type allows negative
//!   balances, and the balance card shows entitled/used/pending/remaining consistently before and
//!   after the decision."** The four numbers have to **add up** — entitled = used + pending +
//!   remaining — before and after, which is the assertion that catches an increment-in-place
//!   balance.
//! * **"Approving a request updates used_days, emits hr.leave.approved and makes the employee show
//!   as on leave for those dates."** All three, and the event payload is checked for the reason
//!   being *absent*: a payload that carries a person's reason to a third party's webhook is a
//!   leak, and no status code catches it.
//! * **"Cancelling a pending request releases the balance"** — and a cancelled request stops being
//!   an overlap, which is the one thing a naive "refuse any overlap" check gets wrong.
//! * a type with `requires_approval = false` is **approved on creation** and still emits
//!   `hr.leave.approved`: the direct-decide path the request's risk note asks for, which has to
//!   stay usable without REQ-059 installed.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::rate_limit_middleware::RateLimiter;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope as PermScope};
use omnion_permissions::{roles as role_store, seed};
use omnion_security::RatePolicy;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

mod support {
    //! The sign-in half, shared with the people-core suite for the same reason: a hand-rolled
    //! `login()` that keeps only the first `Set-Cookie` signs a suite in holding a credential that
    //! can read but not write, and every refusal in this file would then be about CSRF.
    pub mod walk_auth;
}

use support::walk_auth::{self, Session};

/// Serialises this suite: the organizations and the IAM seed are shared state.
static LEAVE_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A caller who may ask for leave and read their own, and nothing else.
///
/// The role that proves the split the permission family exists for: somebody who can raise a
/// request must not be able to approve it, edit the catalogue, or read a colleague's balance.
const REQUESTER_PERMISSIONS: [&str; 3] = ["hr.leave.read", "hr.leave.request", "sites.read"];

/// An approver: everything the requester has, plus the two powers they must not have themselves.
const APPROVER_PERMISSIONS: [&str; 7] = [
    "hr.leave.read",
    "hr.leave.request",
    "hr.leave.approve",
    "hr.leave.manage",
    "hr.employees.read",
    "hr.employees.create",
    "sites.read",
];

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

struct TestResponse {
    status: StatusCode,
    set_cookie: Vec<String>,
    body: Value,
}

impl std::fmt::Display for TestResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} {}", self.status.as_u16(), self.body)
    }
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
        serde_json::from_slice(&body).unwrap_or(Value::Null)
    };
    TestResponse {
        status,
        set_cookie,
        body,
    }
}

fn request(
    method: Method,
    path: &str,
    session: Option<&Session>,
    body: Option<Value>,
) -> Request<Body> {
    // `Session::apply` rather than a hand-rolled header: it is the helper that keeps the session
    // cookie and the CSRF token together, and a version of this function that sets only the
    // cookie signs the whole suite in holding a credential that can read but not write.
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
    approver: String,
    requester: String,
    /// The root department the TRIGGER seeded (0196).
    root_department: Uuid,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = LEAVE_WALK.lock().await;
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
            omnion_api::rate_limit_middleware::install(RateLimiter::new(&state, policies));
        });
        seed::ensure(db.pool()).await.ok()?;

        let organization = create_organization_row(&db, "leave").await;
        let (owner_id, _) = create_account(&db, None, "Leave Owner").await;
        seed::bind_owner(db.pool(), owner_id).await.ok()?;

        let (approver_id, approver) = create_account(&db, Some(organization), "Leave Approver").await;
        grant(&db, organization, approver_id, owner_id, &APPROVER_PERMISSIONS).await;

        let (requester_id, requester) =
            create_account(&db, Some(organization), "Leave Requester").await;
        grant(&db, organization, requester_id, owner_id, &REQUESTER_PERMISSIONS).await;

        let root_department: Option<Uuid> = sqlx::query_scalar(
            "select id from hr_departments where organization_id = $1 order by created_at limit 1",
        )
        .bind(organization)
        .fetch_optional(db.pool())
        .await
        .expect("the query must run");
        let root_department = root_department.expect(
            "a tenant created after 0196 is seeded by the trigger, not by a backfill",
        );

        Some(Self {
            _walk: walk,
            state,
            db,
            organization,
            approver,
            requester,
            root_department,
        })
    }

    async fn token(&self, email: &str) -> Session {
        login(&self.state, email).await
    }

    /// An employee row linked to the account behind `email`, so "my record" resolves.
    async fn employee_for(&self, session: &Session, email: &str, first: &str, last: &str) -> Value {
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
                Some(session),
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
            "the employee must be created: {}",
            response.body
        );
        response.body
    }

    /// The seeded ANNUAL type, found by code rather than by position in a list.
    async fn annual_type(&self, session: &Session) -> Value {
        let response = call(
            &self.state,
            request(Method::GET, "/api/v1/hr/leave/types", Some(session), None),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        response.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|entry| entry["code"] == "ANNUAL")
            .cloned()
            .expect("the trigger seeds an ANNUAL type for every tenant")
    }
}

async fn create_organization_row(db: &Db, label: &str) -> Uuid {
    let slug = format!("hr-leave-{}-{}", label, Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(format!("HR Leave {label}"))
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    let email = format!("hr-leave-{}@omnion.test", Uuid::new_v4().simple());
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
            key: format!("hr-leave-role-{}", Uuid::new_v4().simple()),
            name: "HR Leave Walk Role".to_owned(),
            description: "A role of the leave walk".to_owned(),
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
            Some(walk_auth::Session::login_body(email)),
        ),
    )
    .await;
    Session::from_set_cookies(response.set_cookie)
}

fn id_of(value: &Value) -> Uuid {
    Uuid::parse_str(value["id"].as_str().expect("an id")).expect("an id")
}

/// The sentence an error body carries.
///
/// The API's envelope is `{ "error": { "code", "message" } }`, so a walk that reads
/// `body["message"]` gets a silent `null` and every "the refusal says why" assertion passes for
/// the wrong reason — the assertion never looked at the message at all. Naming the envelope once
/// here is what makes those assertions real.
fn error_message(body: &Value) -> String {
    body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// The balance card for one type, read back from the API rather than from the database.
///
/// Reading the card through the same route the screen uses is the point: a test that queries
/// `hr_leave_balances` directly would pass while the card the person reads was built by different
/// code.
async fn card_of(fixture: &Fixture, session: &Session, employee_id: Uuid, year: i32) -> Value {
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/hr/leave/balances?employee_id={employee_id}&year={year}"
            ),
            Some(session),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    response.body["items"]
        .as_array()
        .expect("cards")
        .iter()
        .find(|card| card["code"] == "ANNUAL")
        .cloned()
        .unwrap_or(Value::Null)
}

/// The four numbers of a card, in hundredths, asserted to add up.
///
/// `entitled == used + pending + remaining` is the invariant the acceptance criterion asks for
/// ("the balance card shows entitled/used/pending/remaining consistently"), and it is the only
/// assertion that catches an increment-in-place balance: a card can be internally plausible and
/// still drift from the request rows a week later.
fn assert_card_adds_up(card: &Value, label: &str) {
    let number = |key: &str| -> f64 {
        card[key]
            .as_str()
            .and_then(|text| text.parse::<f64>().ok())
            .unwrap_or_else(|| panic!("{label}: {key} is not a number in {card}"))
    };
    let (entitled, used, pending, remaining) = (
        number("entitled_days"),
        number("used_days"),
        number("pending_days"),
        number("remaining_days"),
    );
    assert!(
        (entitled - (used + pending + remaining)).abs() < 0.001,
        "{label}: {entitled} entitled must equal {used} used + {pending} pending + {remaining} \
         remaining, in {card}"
    );
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

/// The seeded catalogue exists for a tenant born after the migration, and it is what a request
/// form offers.
#[tokio::test]
async fn a_new_tenant_is_seeded_with_a_leave_catalogue_and_balances_render_for_every_type() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let approver = fixture.token(&fixture.approver).await;
    fixture.employee_for(&approver, &fixture.approver, "Grace", "Hopper").await;

    let annual = fixture.annual_type(&approver).await;
    assert_eq!(annual["code"], "ANNUAL");
    assert_eq!(
        annual["annual_days"].as_str().expect("a day count"),
        "14.00",
        "the seed grants 14 days, and it is editable — the value is the seed's, not a hard-coded one"
    );
    assert_eq!(annual["requires_approval"], true);

    // The card exists for every type even before a single request: a type an organization has
    // never used must still show its entitlement, or the select of leave types quietly loses
    // every type nobody has taken yet.
    let response = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/leave/balances", Some(&approver), None),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let cards = response.body["items"].as_array().expect("cards");
    assert!(
        cards.len() >= 3,
        "the seeded catalogue is Annual, Sick and Unpaid: {}",
        response.body
    );
    for card in cards {
        assert_card_adds_up(card, "a fresh card");
        assert!(
            card.get("entitled_days").is_some(),
            "a card without an entitlement is not a card: {card}"
        );
    }
}

/// The headline criterion: the preview equals the stored value, and a weekend is not charged.
#[tokio::test]
async fn the_days_the_form_previews_are_the_days_the_request_is_charged() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let approver = fixture.token(&fixture.approver).await;
    fixture
        .employee_for(&approver, &fixture.approver, "Grace", "Hopper")
        .await;
    let annual = fixture.annual_type(&approver).await;
    let type_id = id_of(&annual);

    // Mon 2026-10-05 → Sun 2026-10-11 is 7 calendar days and 5 working days. This is the range
    // a form and a store disagree on: one implementation counts the days between the dates.
    let preview = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/hr/leave/requests/preview?starts_on=2026-10-05&ends_on=2026-10-11",
            Some(&approver),
            None,
        ),
    )
    .await;
    assert_eq!(preview.status, StatusCode::OK, "{}", preview.body);
    assert_eq!(
        preview.body["days"], "5",
        "a weekend inside a request is not leave: {}",
        preview.body
    );
    assert_eq!(preview.body["working_days"], 5);

    // A half-day is exactly 0.5, from the SAME route the form calls.
    let half = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/hr/leave/requests/preview?starts_on=2026-10-05&ends_on=2026-10-05&half_day=true",
            Some(&approver),
            None,
        ),
    )
    .await;
    assert_eq!(half.body["days"], "0.5", "{}", half.body);

    // And the stored value equals the preview, for both shapes.
    let full = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/leave/requests",
            Some(&approver),
            Some(json!({
                "leave_type_id": type_id,
                "starts_on": "2026-10-05",
                "ends_on": "2026-10-11",
                "reason": "A week away",
            })),
        ),
    )
    .await;
    assert_eq!(full.status, StatusCode::CREATED, "{}", full.body);
    assert_eq!(
        full.body["days"], preview.body["days"],
        "the number the form showed must be the number the row carries"
    );

    // A weekend-only range is refused rather than stored as a zero-day request: the schema's
    // `days > 0` would otherwise be the only thing standing between a person and a Saturday.
    let saturday = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/leave/requests",
            Some(&approver),
            Some(json!({
                "leave_type_id": type_id,
                "starts_on": "2026-10-10",
                "ends_on": "2026-10-11",
            })),
        ),
    )
    .await;
    assert_eq!(
        saturday.status,
        StatusCode::BAD_REQUEST,
        "a weekend is not leave: {}",
        saturday.body
    );
    assert!(
        error_message(&saturday.body)
            .contains("working day"),
        "the refusal says why: {}",
        saturday.body
    );
}

/// An overlap is refused, and the refusal names the dates that clash.
#[tokio::test]
async fn an_overlapping_request_is_refused_with_the_conflicting_dates() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let approver = fixture.token(&fixture.approver).await;
    fixture
        .employee_for(&approver, &fixture.approver, "Grace", "Hopper")
        .await;
    let type_id = id_of(&fixture.annual_type(&approver).await);

    let first = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/leave/requests",
            Some(&approver),
            Some(json!({
                "leave_type_id": type_id,
                "starts_on": "2026-11-02",
                "ends_on": "2026-11-04",
                "reason": "Family",
            })),
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    let first_id = id_of(&first.body);

    // Overlapping in the middle: Mon 3 – Wed 4 sits inside the first request.
    let clash = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/leave/requests",
            Some(&approver),
            Some(json!({
                "leave_type_id": type_id,
                "starts_on": "2026-11-03",
                "ends_on": "2026-11-03",
            })),
        ),
    )
    .await;
    assert_eq!(clash.status, StatusCode::CONFLICT, "{}", clash.body);
    let message = error_message(&clash.body);
    assert!(
        message.contains(&first_id.to_string()),
        "the refusal names the request that holds the days: {message}"
    );
    assert!(
        message.contains("2026-11-02") && message.contains("2026-11-04"),
        "the refusal names BOTH dates of the conflict, not just that one exists: {message}"
    );

    // A range that starts the day after is not an overlap — the boundary is inclusive and this
    // is where an off-by-one would refuse a legitimate request.
    let after = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/leave/requests",
            Some(&approver),
            Some(json!({
                "leave_type_id": type_id,
                "starts_on": "2026-11-05",
                "ends_on": "2026-11-05",
            })),
        ),
    )
    .await;
    assert_eq!(
        after.status,
        StatusCode::CREATED,
        "the day after a request ends is free: {}",
        after.body
    );
}

/// The balance: refused when it would go negative, and the four numbers add up throughout.
#[tokio::test]
async fn the_balance_is_refused_when_it_would_go_negative_and_always_adds_up() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let approver = fixture.token(&fixture.approver).await;
    let employee = fixture
        .employee_for(&approver, &fixture.approver, "Grace", "Hopper")
        .await;
    let employee_id = id_of(&employee);
    let type_id = id_of(&fixture.annual_type(&approver).await);

    // Before: 14 entitled, nothing used, nothing pending.
    let before = card_of(&fixture, &approver, employee_id, 2027).await;
    assert_card_adds_up(&before, "before any request");
    assert_eq!(before["entitled_days"], "14");
    assert_eq!(before["remaining_days"], "14");
    assert_eq!(
        before["seeded"], false,
        "no balance row exists yet, and the card says so rather than pretending it does"
    );

    // 14 days of entitlement, asked for as three working weeks in one go.
    let too_much = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/leave/requests",
            Some(&approver),
            Some(json!({
                "leave_type_id": type_id,
                "starts_on": "2027-06-07",
                "ends_on": "2027-07-02",
            })),
        ),
    )
    .await;
    assert_eq!(
        too_much.status,
        StatusCode::CONFLICT,
        "20 working days against 14 must be refused: {}",
        too_much.body
    );
    let message = error_message(&too_much.body);
    for number in ["14", "20"] {
        assert!(
            message.contains(number),
            "the refusal carries the numbers a person needs ({number}): {message}"
        );
    }

    // A request inside the entitlement is accepted, and the card now counts it as PENDING.
    let ok = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/leave/requests",
            Some(&approver),
            Some(json!({
                "leave_type_id": type_id,
                "starts_on": "2027-06-07",
                "ends_on": "2027-06-11",
            })),
        ),
    )
    .await;
    assert_eq!(ok.status, StatusCode::CREATED, "{}", ok.body);
    assert_eq!(ok.body["days"], "5");
    assert_eq!(ok.body["leave_status"], "pending");

    let pending = card_of(&fixture, &approver, employee_id, 2027).await;
    assert_card_adds_up(&pending, "with a pending request");
    assert_eq!(pending["pending_days"], "5");
    assert_eq!(pending["used_days"], "0");
    assert_eq!(pending["remaining_days"], "9");
    assert_eq!(
        pending["seeded"], true,
        "the balance row exists now, because the request created it"
    );
}

/// Approving moves pending to used, and a second decision is refused rather than double-charged.
#[tokio::test]
async fn approving_moves_pending_to_used_and_a_second_decision_is_refused() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let approver = fixture.token(&fixture.approver).await;
    let employee = fixture
        .employee_for(&approver, &fixture.approver, "Grace", "Hopper")
        .await;
    let employee_id = id_of(&employee);
    let type_id = id_of(&fixture.annual_type(&approver).await);

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/leave/requests",
            Some(&approver),
            Some(json!({
                "leave_type_id": type_id,
                "starts_on": "2027-08-02",
                "ends_on": "2027-08-04",
                "reason": "Not mine to read",
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let request_id = id_of(&created.body);

    let approved = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/hr/leave/requests/{request_id}/decision"),
            Some(&approver),
            Some(json!({ "decision": "approve", "comment": "Enjoy" })),
        ),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    assert_eq!(approved.body["leave_status"], "approved");
    assert_eq!(approved.body["decision_comment"], "Enjoy");

    let after = card_of(&fixture, &approver, employee_id, 2027).await;
    assert_card_adds_up(&after, "after approval");
    assert_eq!(after["used_days"], "3", "approved days are spent");
    assert_eq!(after["pending_days"], "0", "and no longer pending");
    assert_eq!(after["remaining_days"], "11");

    // The second click of a double-clicked approve button, and of a second approver racing the
    // first: refused, and the balance is NOT charged twice.
    let again = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/hr/leave/requests/{request_id}/decision"),
            Some(&approver),
            Some(json!({ "decision": "approve" })),
        ),
    )
    .await;
    assert_eq!(
        again.status,
        StatusCode::CONFLICT,
        "a decided request cannot be decided again: {}",
        again.body
    );
    let unchanged = card_of(&fixture, &approver, employee_id, 2027).await;
    assert_eq!(
        unchanged["used_days"], "3",
        "a refused second decision must not move the balance"
    );
    assert_card_adds_up(&unchanged, "after a refused second decision");

    // The event went on the bus, and its payload carries ids and dates and **not the reason**.
    let payload: Option<Value> = sqlx::query_scalar(
        "select payload from events where name = 'hr.leave.approved' and organization_id = $1 \
         order by created_at desc limit 1",
    )
    .bind(fixture.organization)
    .fetch_optional(fixture.db.pool())
    .await
    .expect("the event query must run");
    let payload = payload.expect("approving a request emits hr.leave.approved");
    assert_eq!(payload["leave_request_id"], request_id.to_string());
    assert_eq!(payload["days"], "3");
    assert!(
        payload.get("reason").is_none(),
        "a person's reason is their own words and a subscriber may be a third party's webhook: \
         {payload}"
    );
    assert!(payload.get("decision_comment").is_none(), "{payload}");
}

/// Rejecting records the comment on the timeline; cancelling releases the days.
#[tokio::test]
async fn a_rejection_shows_its_comment_and_a_cancellation_releases_the_balance() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let approver = fixture.token(&fixture.approver).await;
    let employee = fixture
        .employee_for(&approver, &fixture.approver, "Grace", "Hopper")
        .await;
    let employee_id = id_of(&employee);
    let type_id = id_of(&fixture.annual_type(&approver).await);

    // Rejected: the comment is visible on the timeline and nothing is charged.
    let rejected_request = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/leave/requests",
            Some(&approver),
            Some(json!({
                "leave_type_id": type_id,
                "starts_on": "2027-09-06",
                "ends_on": "2027-09-07",
            })),
        ),
    )
    .await;
    let rejected_id = id_of(&rejected_request.body);
    let rejected = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/hr/leave/requests/{rejected_id}/decision"),
            Some(&approver),
            Some(json!({ "decision": "reject", "comment": "Release week" })),
        ),
    )
    .await;
    assert_eq!(rejected.body["leave_status"], "rejected");

    let detail = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/leave/requests/{rejected_id}"),
            Some(&approver),
            None,
        ),
    )
    .await;
    assert_eq!(detail.status, StatusCode::OK, "{}", detail.body);
    let timeline = detail.body["timeline"].as_array().expect("a timeline");
    assert!(
        timeline
            .iter()
            .any(|step| step["kind"] == "rejected"
                && step["comment"] == "Release week"),
        "the comment the approver wrote is on the timeline: {}",
        detail.body
    );
    assert!(
        detail.body["can_decide"] == false,
        "a decided request offers no decision panel: {}",
        detail.body
    );
    let after_reject = card_of(&fixture, &approver, employee_id, 2027).await;
    assert_eq!(
        after_reject["used_days"], "0",
        "a rejected request is never charged"
    );
    assert_eq!(after_reject["remaining_days"], "14");
    assert_card_adds_up(&after_reject, "after a rejection");

    // Cancelled: the days it held are released, and it stops being an overlap.
    let cancelled_request = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/leave/requests",
            Some(&approver),
            Some(json!({
                "leave_type_id": type_id,
                "starts_on": "2027-10-04",
                "ends_on": "2027-10-06",
            })),
        ),
    )
    .await;
    let cancelled_id = id_of(&cancelled_request.body);
    let held = card_of(&fixture, &approver, employee_id, 2027).await;
    assert_eq!(held["pending_days"], "3");

    let cancelled = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/hr/leave/requests/{cancelled_id}/cancel"),
            Some(&approver),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(cancelled.status, StatusCode::OK, "{}", cancelled.body);
    assert_eq!(cancelled.body["leave_status"], "cancelled");

    let released = card_of(&fixture, &approver, employee_id, 2027).await;
    assert_eq!(
        released["pending_days"], "0",
        "cancelling releases the days it was holding"
    );
    assert_eq!(released["remaining_days"], "14");
    assert_card_adds_up(&released, "after a cancellation");

    // The same dates are free again. A cancelled request is history, not a double booking.
    let again = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/leave/requests",
            Some(&approver),
            Some(json!({
                "leave_type_id": type_id,
                "starts_on": "2027-10-04",
                "ends_on": "2027-10-06",
            })),
        ),
    )
    .await;
    assert_eq!(
        again.status,
        StatusCode::CREATED,
        "a cancelled request does not hold its dates: {}",
        again.body
    );

    // An approved request is history: cancelling it is refused rather than erasing it.
    let approved_id = id_of(&again.body);
    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/hr/leave/requests/{approved_id}/decision"),
            Some(&approver),
            Some(json!({ "decision": "approve" })),
        ),
    )
    .await;
    let refuse = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/hr/leave/requests/{approved_id}/cancel"),
            Some(&approver),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(
        refuse.status,
        StatusCode::CONFLICT,
        "approved leave is history, not a pending request: {}",
        refuse.body
    );
}

/// A type with `requires_approval = false` is approved on creation — the direct-decide path the
/// request's risk note asks for, which has to work without REQ-059 installed.
#[tokio::test]
async fn a_type_that_needs_no_approval_is_approved_on_creation_and_still_emits() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let approver = fixture.token(&fixture.approver).await;
    let employee = fixture
        .employee_for(&approver, &fixture.approver, "Grace", "Hopper")
        .await;
    let employee_id = id_of(&employee);

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/leave/types",
            Some(&approver),
            Some(json!({
                "name": "Sabbatical",
                "code": "SABBATICAL",
                "annual_days": 10,
                "requires_approval": false,
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let type_id = id_of(&created.body);

    let asked = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/leave/requests",
            Some(&approver),
            Some(json!({
                "leave_type_id": type_id,
                "starts_on": "2027-11-01",
                "ends_on": "2027-11-02",
            })),
        ),
    )
    .await;
    assert_eq!(asked.status, StatusCode::CREATED, "{}", asked.body);
    assert_eq!(
        asked.body["leave_status"], "approved",
        "a type the organization set to not need approval is approved on creation: {}",
        asked.body
    );

    // The same event still fires. An automation waiting on `hr.leave.approved` would otherwise
    // never run in exactly the organization that believes it is running unattended.
    let emitted: i64 = sqlx::query_scalar(
        "select count(*) from events where name = 'hr.leave.approved' and organization_id = $1",
    )
    .bind(fixture.organization)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the event query must run");
    assert_eq!(
        emitted, 1,
        "the direct-decide path emits the same event as the approved path"
    );

    let card = card_of(&fixture, &approver, employee_id, 2027).await;
    assert_card_adds_up(&card, "a type with no approval step");
}

/// A caller who can request leave cannot approve it, read a colleague's balance, or edit the
/// catalogue — and every route answers 401 without a session.
#[tokio::test]
async fn the_split_between_requesting_and_approving_is_real() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let approver = fixture.token(&fixture.approver).await;
    let employee = fixture
        .employee_for(&approver, &fixture.approver, "Grace", "Hopper")
        .await;
    let type_id = id_of(&fixture.annual_type(&approver).await);

    let requester = fixture.token(&fixture.requester).await;
    let theirs = fixture
        .employee_for(&approver, &fixture.requester, "Alan", "Turing")
        .await;
    let theirs_id = id_of(&theirs);

    // Their own request, for themselves: allowed with no HR power at all.
    let own = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/leave/requests",
            Some(&requester),
            Some(json!({
                "leave_type_id": type_id,
                "starts_on": "2027-12-06",
                "ends_on": "2027-12-07",
            })),
        ),
    )
    .await;
    assert_eq!(
        own.status,
        StatusCode::CREATED,
        "an employee may request their own leave: {}",
        own.body
    );
    assert_eq!(
        own.body["employee_id"], theirs_id.to_string(),
        "an omitted employee_id is the caller, not the first employee in the list"
    );
    let own_id = id_of(&own.body);

    // Deciding it: refused. Asking and agreeing are different acts by different people, and a
    // key that did both would make the approval chain a formality.
    let self_approve = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/hr/leave/requests/{own_id}/decision"),
            Some(&requester),
            Some(json!({ "decision": "approve" })),
        ),
    )
    .await;
    assert_eq!(
        self_approve.status,
        StatusCode::FORBIDDEN,
        "a person may not approve their own holiday: {}",
        self_approve.body
    );

    // Somebody else's balance: refused, even though they can read their own.
    let colleague = call(
        &fixture.state,
        request(
            Method::GET,
            // `as_str()`, not `{}` on the `Value`: formatting a JSON string prints it WITH its
            // quotes, and a quote is not a legal URI character — the request never leaves the
            // process and the walk fails on an `InvalidUri` instead of on a 403.
            &format!(
                "/api/v1/hr/leave/balances?employee_id={}",
                employee["id"].as_str().expect("an id")
            ),
            Some(&requester),
            None,
        ),
    )
    .await;
    assert_eq!(
        colleague.status,
        StatusCode::FORBIDDEN,
        "a colleague's entitlement is not the requester's to read: {}",
        colleague.body
    );

    // The catalogue: refused. Raising the entitlement one is measured against is an HR decision.
    let catalogue = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/leave/types",
            Some(&requester),
            Some(json!({ "name": "Endless", "code": "ENDLESS", "annual_days": 365 })),
        ),
    )
    .await;
    assert_eq!(
        catalogue.status,
        StatusCode::FORBIDDEN,
        "leave policy is not the requester's to write: {}",
        catalogue.body
    );

    // 401 without a session, on a read and on a write.
    for (method, path) in [
        (Method::GET, "/api/v1/hr/leave/requests".to_owned()),
        (
            Method::POST,
            "/api/v1/hr/leave/requests".to_owned(),
        ),
        (Method::GET, "/api/v1/hr/leave/balances".to_owned()),
        (Method::GET, "/api/v1/hr/leave/types".to_owned()),
        (Method::GET, "/api/v1/hr/leave/calendar".to_owned()),
    ] {
        let anonymous = call(
            &fixture.state,
            request(method.clone(), &path, None, Some(json!({}))),
        )
        .await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "{method} {path} must refuse an anonymous caller: {}",
            anonymous.body
        );
    }
}

/// The absence calendar shows approved leave, and marks a range that runs past the window.
#[tokio::test]
async fn the_absence_calendar_shows_approved_leave_and_marks_a_range_that_continues() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let approver = fixture.token(&fixture.approver).await;
    fixture
        .employee_for(&approver, &fixture.approver, "Grace", "Hopper")
        .await;
    let type_id = id_of(&fixture.annual_type(&approver).await);

    // Pending leave is NOT an absence yet: drawing it would put a person on a calendar for a
    // holiday their manager has not agreed to.
    let pending = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/leave/requests",
            Some(&approver),
            Some(json!({
                "leave_type_id": type_id,
                "starts_on": "2027-04-05",
                "ends_on": "2027-04-07",
            })),
        ),
    )
    .await;
    let pending_id = id_of(&pending.body);

    let before = calendar(&fixture, &approver, "2027-04-01", "2027-04-30").await;
    assert_eq!(
        before["bars"].as_array().map(Vec::len),
        Some(0),
        "a pending request is not an absence: {}",
        before
    );

    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/hr/leave/requests/{pending_id}/decision"),
            Some(&approver),
            Some(json!({ "decision": "approve" })),
        ),
    )
    .await;

    // The request runs 5–7 April; the window is 1–10 April, so it is fully inside.
    let inside = calendar(&fixture, &approver, "2027-04-01", "2027-04-10").await;
    let bars = inside["bars"].as_array().expect("bars");
    assert_eq!(bars.len(), 1, "{}", inside);
    assert_eq!(bars[0]["leave_type_name"], "Annual");
    assert_eq!(bars[0]["continues_after"], false);
    assert_eq!(bars[0]["continues_before"], false);
    assert_eq!(
        inside["employees"].as_array().map(Vec::len),
        Some(1),
        "the row labels come from the same rows as the bars: {}",
        inside
    );

    // A window that starts mid-request marks the continuation, so the grid does not draw a bar
    // that appears to end at the window's edge.
    let partial = calendar(&fixture, &approver, "2027-04-06", "2027-04-30").await;
    let bars = partial["bars"].as_array().expect("bars");
    assert_eq!(bars.len(), 1, "a range that started before the window is still a bar: {partial}");
    assert_eq!(bars[0]["continues_before"], true);
    assert_eq!(bars[0]["starts_on"], "2027-04-05", "the real start, not the window's");
}

/// Read the calendar through the route the screen uses.
async fn calendar(fixture: &Fixture, session: &Session, from: &str, to: &str) -> Value {
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/leave/calendar?from={from}&to={to}"),
            Some(session),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    response.body
}
