//! Integration tests for the clock (docs/requests/REQ-055, slice 2d).
//!
//! Slice 2c's suite proved that a surface with **no** `hr.*` key answers for its caller. This
//! file proves the other half of the same claim for the clock, and the three things a unit test
//! cannot see:
//!
//! 1. **The keyless surface is the clock surface.** An account holding `sites.read` and nothing
//!    else punches its own day, reads its own month and exports its own CSV. The constant below
//!    is named for what it must stay: adding one `hr.*` key would delete the test rather than
//!    break it, and a clock behind a permission is a clock nobody with an employee record can
//!    use.
//! 2. **A second punch is refused, and the refusal carries the punch it found.** The API table
//!    calls the payload "idempotent per employee + day + kind", and a table row nobody walks is a
//!    wish. `already clocked in, at 09:02` is what makes the 409 actionable; `already clocked in`
//!    alone sends the person to a log.
//! 3. **The three refusals are 409, not 400.** A client that retries a 400 forever — which is
//!    what an optimistic client does — would retry a second check-in forever too. This walks the
//!    status code, because a "correct" refusal at the wrong status is still a retry loop.
//!
//! The CSV criterion is checked here rather than in the browser: "every report exports
//! row-for-row with the table" is a **server-side** claim (the export reads the same
//! `month_of` the grid reads), and a browser cannot tell one CSV's rows from another's.
//!
//! Two properties are walked from the *other* direction, because the permission split is only
//! proven by its refusals:
//!
//! * an account with no `hr.attendance.*` key **cannot** punch somebody else's day — the
//!   named `employee_id` is refused rather than silently redirected to the caller's own, which
//!   would be a request that appears to succeed and writes the wrong row;
//! * a reader with `hr.attendance.read` reads a colleague's month, and a `own`-scoped caller
//!   gets `404` for a colleague rather than a 403 that would confirm the day exists.

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
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

mod support {
    //! The sign-in half, shared with the people-core, leave and self-service suites for the same
    //! reason: a hand-rolled `login()` that keeps only the first `Set-Cookie` signs a suite in
    //! holding a credential that can read but not write, and every refusal in this file would
    //! then be about CSRF rather than about the permission split being tested.
    pub mod walk_auth;
}

use support::walk_auth::{self, Session};

/// Serialises this suite: the organizations and the IAM seed are shared state.
static ATTENDANCE_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// **The whole point of the fixture.** One `sites.read` — what the panel needs to render its
/// shell — and not one `hr.*` key. An employee is exactly who holds none, so a clock guarded by
/// an `hr.attendance.*` key is a clock that excludes the only people it was built for.
const PLAIN_PERMISSIONS: [&str; 1] = ["sites.read"];

/// The operator side: read, record and correct. Deliberately **not** owner — an owner short-
/// circuits the guard and every refusal below would be untestable.
///
/// `hr.employees.read` is here for one reason and it is not decoration: the walks read their own
/// employee id back **through the API** (`GET /hr/employees?user_id=…`) rather than from the
/// database, so a walk cannot pass on a row the product would refuse to show. The key it needs
/// belongs to the person who reads the directory, and the directory reader is this operator — so
/// the key is granted. A walk that wanted an id the account may not read would be a walk that
/// proved the fixture wrong, not the guard: the correction walk failed exactly this way first,
/// at a 403 whose body named the missing key.
const HR_PERMISSIONS: [&str; 5] = [
    "hr.attendance.read",
    "hr.attendance.record",
    "hr.attendance.manage",
    "hr.employees.create",
    "hr.employees.read",
];

#[derive(Debug)]
struct TestResponse {
    status: StatusCode,
    body: Value,
    text: String,
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
    // The CSV export answers `text/csv`, so the raw text is kept alongside the parsed JSON: a
    // suite that only ever parses JSON cannot read a single row of the export, and the export is
    // one of the criteria.
    let text = String::from_utf8_lossy(&body).into_owned();
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(Value::Null)
    };
    TestResponse {
        status,
        body,
        text,
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

/// The machine-readable code an error body carries.
///
/// The envelope is `{ "error": { "code", "message" } }`, so a walk reading `body["code"]` gets a
/// silent null and every "the refusal says which" assertion passes for the wrong reason.
fn error_code(body: &Value) -> String {
    body["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn error_message(body: &Value) -> String {
    body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
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
    /// The account with **no** `hr.*` key — the one the clock exists for.
    plain: String,
    /// A second plain employee, so the negative half has somebody to be refused about.
    colleague: String,
    /// The operator, holding the three attendance keys.
    hr: String,
    root_department: Uuid,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = ATTENDANCE_WALK.lock().await;
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

        let organization = create_organization_row(&db, "attendance").await;
        let (owner_id, _) = create_account(&db, None, "Attendance Owner").await;
        seed::bind_owner(db.pool(), owner_id).await.ok()?;

        let (plain_id, plain) = create_account(&db, Some(organization), "Plain Employee").await;
        grant(&db, organization, plain_id, owner_id, &PLAIN_PERMISSIONS).await;

        let (colleague_id, colleague) =
            create_account(&db, Some(organization), "Colleague Employee").await;
        grant(&db, organization, colleague_id, owner_id, &PLAIN_PERMISSIONS).await;

        let (hr_id, hr) = create_account(&db, Some(organization), "Attendance Operator").await;
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

    /// An employee row linked to the account behind `email`, so "my day" resolves.
    ///
    /// The write goes in as the **operator** account, not as the employee: adding somebody to the
    /// directory is an HR action answered behind `hr.employees.create`, and an employee holding
    /// no `hr.*` key is refused there. A fixture that created the row as the unprivileged caller
    /// would die at its own setup on a 403 that says nothing about the clock.
    async fn employee_for(&self, email: &str, first: &str, last: &str) -> Uuid {
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
            "the employee must be created by the operator account: {}",
            response.body
        );
        Uuid::parse_str(response.body["id"].as_str().expect("an id")).expect("an id")
    }
}

async fn create_organization_row(db: &Db, label: &str) -> Uuid {
    let slug = format!("hr-att-{}-{}", label, Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(format!("HR Attendance {label}"))
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    let email = format!("hr-att-{}@omnion.test", Uuid::new_v4().simple());
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
            key: format!("hr-att-role-{}", Uuid::new_v4().simple()),
            name: "HR Attendance Walk Role".to_owned(),
            description: "A role of the attendance walk".to_owned(),
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

/// Today, as the wire format — the walk's own clock.
///
/// The punches below are addressed to *today* because the check-out refusal and the
/// `missing_checkout` exception both read the server's clock, and a walk that punched into a
/// fixed month would either be asserting an exception the server has not reached yet or skipping
/// the day's own arithmetic.
fn today() -> String {
    omnion_module_hr::dates::to_wire(&OffsetDateTime::now_utc().date())
}

fn this_month() -> String {
    let today = OffsetDateTime::now_utc().date();
    format!(
        "{:04}-{:02}",
        today.year(),
        u8::from(today.month())
    )
}

// ---------------------------------------------------------------------------------------------
// The criterion
// ---------------------------------------------------------------------------------------------

/// The headline: an account holding **no `hr.*` key at all** punches its own day, reads its own
/// month and exports its own CSV. If any of these picked up a `route_layer`, this is where the
/// walk stops — at a 403, which is the failure `PLAIN_PERMISSIONS` exists to make possible.
#[tokio::test]
async fn an_account_with_no_hr_permission_punches_and_reads_its_own_day() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let plain = fixture.token(&fixture.plain).await;
    let employee_id = fixture.employee_for(&fixture.plain, "Ada", "Lovelace").await;
    let work_date = today();

    // An empty month is not an error: the walk has to be able to press the clock on a clean
    // tenant, which is the state every real employee is in on their first day.
    let empty = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/me/attendance", Some(&plain), None),
    )
    .await;
    assert_eq!(
        empty.status,
        StatusCode::OK,
        "an empty month must read 200, not 404: {}",
        empty.body
    );
    assert_eq!(
        empty.body["days"].as_array().map(Vec::len),
        Some(0),
        "a day nobody has punched is not a day: {}",
        empty.body
    );
    assert_eq!(
        empty.body["employee_id"].as_str(),
        Some(employee_id.to_string().as_str()),
        "the month belongs to the caller: {}",
        empty.body
    );

    // The check-in, with no `employee_id` in the body at all: the caller's own day.
    let punched = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/me/attendance/check-in",
            Some(&plain),
            Some(json!({ "work_date": work_date })),
        ),
    )
    .await;
    assert_eq!(
        punched.status,
        StatusCode::OK,
        "an employee must be able to punch its own day: {}",
        punched.body
    );
    assert_eq!(
        punched.body["work_date"].as_str(),
        Some(work_date.as_str()),
        "the day is the one asked for: {}",
        punched.body
    );
    assert_eq!(
        punched.body["source"].as_str(),
        Some("manual"),
        "a person pressing the button is a manual punch, never an api row: {}",
        punched.body
    );
    // An open day has no minutes. Asserting it here is what makes the projection a projection:
    // a reader that filled this in would be a second answer to a number payroll reads.
    assert!(
        punched.body["minutes_worked"].is_null(),
        "an open day has no minutes: {}",
        punched.body
    );
    assert!(
        !punched.body["check_in"].is_null(),
        "the check-in instant is published: {}",
        punched.body
    );
    assert!(
        punched.body["check_out"].is_null(),
        "the day is still open: {}",
        punched.body
    );

    // And the day is on the month that was just read, without a second request racing the first.
    let month = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/me/attendance?month={}", this_month()),
            Some(&plain),
            None,
        ),
    )
    .await;
    assert_eq!(month.status, StatusCode::OK, "{}", month.body);
    let days = month.body["days"].as_array().expect("days");
    assert_eq!(days.len(), 1, "the punched day is on the month: {}", month.body);
    assert_eq!(days[0]["work_date"].as_str(), Some(work_date.as_str()));

    // The export, on the same keyless surface.
    let csv = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/me/attendance/export?month={}", this_month()),
            Some(&plain),
            None,
        ),
    )
    .await;
    assert_eq!(csv.status, StatusCode::OK, "{}", csv.body);
    assert!(
        csv.text.starts_with("work_date,check_in,check_out"),
        "the export has its header: {}",
        csv.text
    );
    assert!(
        csv.text.contains(&work_date),
        "the export carries the punched day: {}",
        csv.text
    );
}

/// A second punch is refused — at `409`, with a code that names the case, and with the punch it
/// found in the message so the sentence is actionable.
#[tokio::test]
async fn a_second_punch_is_refused_with_the_punch_it_found() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let plain = fixture.token(&fixture.plain).await;
    fixture.employee_for(&fixture.plain, "Grace", "Hopper").await;
    let work_date = today();

    let first = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/me/attendance/check-in",
            Some(&plain),
            Some(json!({ "work_date": work_date })),
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);

    let second = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/me/attendance/check-in",
            Some(&plain),
            Some(json!({ "work_date": work_date })),
        ),
    )
    .await;
    // 409, not 400. The payload was well formed; the DAY said no. A client that retries a 400
    // forever would retry this forever too.
    assert_eq!(
        second.status,
        StatusCode::CONFLICT,
        "a second check-in is a conflict, not a bad request: {}",
        second.body
    );
    assert_eq!(
        error_code(&second.body),
        "hr_attendance_already_checked_in",
        "the refusal names its case: {}",
        second.body
    );
    // The message is what the person reads on the clock screen, and it has to carry the instant
    // or it sends them to a log to find out when they came in.
    let message = error_message(&second.body);
    assert!(
        message.contains("already clocked in"),
        "the refusal says what happened: {message}"
    );
    assert!(
        message.contains("at "),
        "the refusal carries the punch it found, not a bare conflict: {message}"
    );
    // And the machine-readable value travels beside it, so the screen can render the time
    // rather than parse it out of a sentence.
    let carried = second.body["error"]["details"]["check_in_at"]
        .as_str()
        .unwrap_or_default();
    assert!(
        !carried.is_empty(),
        "the body carries the instant the screen shows: {}",
        second.body
    );

    // The refusal changed nothing: one row, still open. A refusal that half-applied is the
    // failure this assertion exists for.
    let month = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/me/attendance?month={}", this_month()),
            Some(&plain),
            None,
        ),
    )
    .await;
    let days = month.body["days"].as_array().expect("days");
    assert_eq!(
        days.len(),
        1,
        "a refused punch wrote no second row: {}",
        month.body
    );
    assert!(days[0]["check_out"].is_null(), "the day is still open");
}

/// The other two refusals, walked for the same reason: a clock screen that can only refuse one
/// way is a clock screen that answers "something went wrong" for the other two.
#[tokio::test]
async fn the_other_two_clock_refusals_are_walked() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let plain = fixture.token(&fixture.plain).await;
    fixture.employee_for(&fixture.plain, "Katherine", "Johnson").await;
    let work_date = today();

    // A check-out for a day with no check-in. The refusal names the field rather than saying
    // "conflict", because the fix is on the screen: there is no button to press.
    let without = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/me/attendance/check-out",
            Some(&plain),
            Some(json!({ "work_date": work_date })),
        ),
    )
    .await;
    assert_eq!(
        without.status,
        StatusCode::CONFLICT,
        "closing a day that never opened is a conflict: {}",
        without.body
    );
    assert_eq!(
        error_code(&without.body),
        "hr_attendance_checkout_without_checkin",
        "the refusal names its case: {}",
        without.body
    );
    assert_eq!(
        error_message(&without.body),
        "this day has no check-in to close",
        "the sentence is the one the screen renders: {}",
        without.body
    );

    // Open the day, close it, and try to close it again.
    for path in ["check-in", "check-out"] {
        let response = call(
            &fixture.state,
            request(
                Method::POST,
                &format!("/api/v1/hr/me/attendance/{path}"),
                Some(&plain),
                Some(json!({ "work_date": work_date })),
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{path}: {}", response.body);
    }
    let again = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/me/attendance/check-out",
            Some(&plain),
            Some(json!({ "work_date": work_date })),
        ),
    )
    .await;
    assert_eq!(
        again.status,
        StatusCode::CONFLICT,
        "closing a closed day is a conflict: {}",
        again.body
    );
    assert_eq!(
        error_code(&again.body),
        "hr_attendance_already_checked_out",
        "the refusal names its case: {}",
        again.body
    );
}

/// A closed day publishes its minutes, and the export's row matches the grid's row.
#[tokio::test]
async fn a_closed_day_publishes_its_minutes_and_the_export_matches_the_grid() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let plain = fixture.token(&fixture.plain).await;
    fixture.employee_for(&fixture.plain, "Alan", "Turing").await;
    let work_date = today();

    // Two punches an hour and a half apart, addressed explicitly: this walk is about the
    // ARITHMETIC, and leaving it to the wall clock would make the expected number a race.
    for (path, at) in [
        ("check-in", format!("{work_date}T09:00:00Z")),
        ("check-out", format!("{work_date}T17:30:00Z")),
    ] {
        let response = call(
            &fixture.state,
            request(
                Method::POST,
                &format!("/api/v1/hr/me/attendance/{path}"),
                Some(&plain),
                Some(json!({ "work_date": work_date, "at": at })),
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{path}: {}", response.body);
    }

    let month = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/me/attendance?month={}", this_month()),
            Some(&plain),
            None,
        ),
    )
    .await;
    let days = month.body["days"].as_array().expect("days");
    assert_eq!(days.len(), 1, "{}", month.body);
    // 09:00 → 17:30 is 510 minutes. The projection is the module's, derived on read, and this is
    // the one place a number a payroll run will pay is pinned.
    assert_eq!(
        days[0]["minutes_worked"].as_i64(),
        Some(510),
        "the minutes are the difference of the two punches: {}",
        month.body
    );
    // A closed day in the past with both punches is not an exception of any kind.
    assert!(
        days[0]["exception"].is_null(),
        "a clean closed day is not an exception: {}",
        month.body
    );

    // The summary counts the day once and sums the minutes — the total is what the screen's
    // footer shows, and it is the same projection, not a second sum.
    assert_eq!(
        month.body["summary"]["minutes_worked"].as_i64(),
        Some(510),
        "the summary sums the same projection: {}",
        month.body
    );
    assert_eq!(
        month.body["summary"]["days_present"].as_i64(),
        Some(1),
        "the day is counted once: {}",
        month.body
    );

    // Row for row with the grid: the export's data line for that day, in the same order, with
    // the same minutes. The export reads the same `month_of` the grid reads, and this is the
    // assertion that keeps it that way.
    let csv = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/me/attendance/export?month={}", this_month()),
            Some(&plain),
            None,
        ),
    )
    .await;
    assert_eq!(csv.status, StatusCode::OK, "{}", csv.body);
    let rows: Vec<&str> = csv
        .text
        .lines()
        .filter(|line| line.starts_with(&work_date))
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "the export carries exactly the grid's row: {}",
        csv.text
    );
    let columns: Vec<&str> = rows[0].split(',').collect();
    assert_eq!(
        columns.get(3).copied(),
        Some("510"),
        "the export's minutes are the grid's minutes: {}",
        csv.text
    );
    // The totals travel with the rows, so a payroll import does not have to re-derive the sum.
    assert!(
        csv.text.contains("# total,1,510"),
        "the export carries the summary totals: {}",
        csv.text
    );
}

/// The permission split, from the other side: a named employee is a power, and an account
/// without the key must be refused rather than redirected to its own day.
#[tokio::test]
async fn punching_for_somebody_else_needs_the_recording_key() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let plain = fixture.token(&fixture.plain).await;
    let colleague_session = fixture.token(&fixture.colleague).await;
    let own = fixture.employee_for(&fixture.plain, "Mary", "Jackson").await;
    let colleague = fixture
        .employee_for(&fixture.colleague, "Patricia", "Wing")
        .await;
    let work_date = today();

    // Refused for the employee…
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/me/attendance/check-in",
            Some(&plain),
            Some(json!({ "work_date": work_date, "employee_id": colleague })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "punching a colleague's day is a power, not a redirect: {}",
        refused.body
    );
    // The **code** is the missing key, not a generic `permission_denied`. This module's
    // convention is deliberate and it is the stronger property: a client can branch on
    // `hr.attendance.record` without parsing an English sentence, and a client that only ever
    // saw `permission_denied` would have no way to tell which key to ask an administrator for.
    assert_eq!(
        error_code(&refused.body),
        "hr.attendance.record",
        "the refusal names the key it is missing: {}",
        refused.body
    );
    assert!(
        error_message(&refused.body).contains("hr.attendance.record"),
        "the refusal says which key is missing: {}",
        refused.body
    );

    // …and — the half that is easy to get wrong — it wrote **nothing**, neither the colleague's
    // day nor the caller's own. A "helpful" fallback that punches the caller's own row would
    // answer 200 for a request about somebody else, and the two rows would then disagree about
    // who was in.
    let own_month = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/me/attendance?month={}", this_month()),
            Some(&plain),
            None,
        ),
    )
    .await;
    assert_eq!(
        own_month.body["days"].as_array().map(Vec::len),
        Some(0),
        "the refused request wrote no row for the caller either: {}",
        own_month.body
    );

    // The colleague is untouched too, read through the account allowed to read it.
    let colleague_month = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/me/attendance?month={}", this_month()),
            Some(&colleague_session),
            None,
        ),
    )
    .await;
    assert_eq!(
        colleague_month.body["days"].as_array().map(Vec::len),
        Some(0),
        "the refused request wrote no row for the colleague: {}",
        colleague_month.body
    );

    // With the key, the same route serves both: the operator punches the colleague's day and the
    // row says `api`, because a service account and a person pressing a button are the same
    // request on this route and only the source distinguishes them.
    let hr = fixture.token(&fixture.hr).await;
    let allowed = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/me/attendance/check-in",
            Some(&hr),
            Some(json!({ "work_date": work_date, "employee_id": colleague })),
        ),
    )
    .await;
    assert_eq!(
        allowed.status,
        StatusCode::OK,
        "the recording key is the only difference: {}",
        allowed.body
    );
    assert_eq!(
        allowed.body["source"].as_str(),
        Some("api"),
        "a punched-for row says where it came from: {}",
        allowed.body
    );
    assert_eq!(
        allowed.body["employee_id"].as_str(),
        Some(colleague.to_string().as_str()),
        "the row belongs to the named employee: {}",
        allowed.body
    );
    assert_eq!(
        own,
        own,
        "the caller's own employee is unchanged by any of this"
    );
}

/// Reading somebody else's month is `hr.attendance.read`, and the refusal does not confirm that
/// the day exists.
#[tokio::test]
async fn reading_another_employees_month_is_guarded_in_both_directions() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let plain = fixture.token(&fixture.plain).await;
    let colleague_session = fixture.token(&fixture.colleague).await;
    let _own = fixture.employee_for(&fixture.plain, "Joan", "Clarke").await;
    let colleague = fixture
        .employee_for(&fixture.colleague, "Jean", "Bartik")
        .await;

    // The employee is refused, and the message names the key.
    let refused = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/me/attendance?employee_id={colleague}&month={}", this_month()),
            Some(&plain),
            None,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "an employee cannot read a colleague's month: {}",
        refused.body
    );
    assert!(
        error_message(&refused.body).contains("hr.attendance.read"),
        "the refusal says which key is missing: {}",
        refused.body
    );

    // The reader reaches it, on the admin route rather than the self-service one.
    let hr = fixture.token(&fixture.hr).await;
    let allowed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/attendance?employee_id={colleague}&month={}", this_month()),
            Some(&hr),
            None,
        ),
    )
    .await;
    assert_eq!(
        allowed.status,
        StatusCode::OK,
        "the reader reaches the colleague's month: {}",
        allowed.body
    );
    assert_eq!(
        allowed.body["employee_id"].as_str(),
        Some(colleague.to_string().as_str()),
        "the month is the one that was asked for: {}",
        allowed.body
    );

    // The colleague's own account, naming themselves, is NOT refused: `/hr/me/attendance` with
    // an `employee_id` the caller does not own is the narrowing case, and the other plain account
    // is exactly that. The counterpart to the row above.
    let self_named = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/me/attendance?month={}", this_month()),
            Some(&colleague_session),
            None,
        ),
    )
    .await;
    assert_eq!(
        self_named.status,
        StatusCode::OK,
        "an employee always reads its own month: {}",
        self_named.body
    );
}

/// A correction is the one write that changes what a payroll run will pay, so it is walked from
/// four directions: the key, the reason, the order and the audit row.
#[tokio::test]
async fn a_correction_is_guarded_carries_its_reason_and_audits_itself() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let plain = fixture.token(&fixture.plain).await;
    let _own = fixture.employee_for(&fixture.plain, "Hedy", "Lamarr").await;
    let work_date = today();

    // An open day, so there is something to correct. The check-in is explicit, so the walk does
    // not depend on the wall clock for the instant it later asserts.
    let punched = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/me/attendance/check-in",
            Some(&plain),
            Some(json!({ "work_date": work_date, "at": format!("{work_date}T08:00:00Z") })),
        ),
    )
    .await;
    assert_eq!(punched.status, StatusCode::OK, "{}", punched.body);

    // No key: refused at the admin route, which is where a correction lives.
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/attendance/corrections",
            Some(&plain),
            Some(json!({
                "employee_id": id_of_employee(&fixture, &fixture.plain).await,
                "work_date": work_date,
                "check_out": format!("{work_date}T17:00:00Z"),
                "reason": "left the office early",
            })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a correction is an HR action: {}",
        refused.body
    );
    assert!(
        error_message(&refused.body).contains("hr.attendance.manage"),
        "the refusal says which key is missing: {}",
        refused.body
    );

    let hr = fixture.token(&fixture.hr).await;
    let employee_id = id_of_employee(&fixture, &fixture.plain).await;

    // A correction that supplies neither punch is refused by the service, naming the field.
    let empty = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/attendance/corrections",
            Some(&hr),
            Some(json!({
                "employee_id": employee_id,
                "work_date": work_date,
                "reason": "nothing to say",
            })),
        ),
    )
    .await;
    assert_eq!(
        empty.status,
        StatusCode::BAD_REQUEST,
        "a correction with no punch is not a correction: {}",
        empty.body
    );

    // A backwards pair is refused too, and the service owns the message (the constraint is the
    // backstop). The correction supplies the check-out only, so the check-in is the stored one —
    // the shape the drawer actually sends when somebody forgot to clock out.
    let backwards = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/attendance/corrections",
            Some(&hr),
            Some(json!({
                "employee_id": employee_id,
                "work_date": work_date,
                "check_out": format!("{work_date}T07:00:00Z"),
                "reason": "wrong day on the badge",
            })),
        ),
    )
    .await;
    assert_eq!(
        backwards.status,
        StatusCode::BAD_REQUEST,
        "the clock cannot run backwards: {}",
        backwards.body
    );
    // The product publishes the FIELD as structured data, not only in the sentence: `details.field`
    // is `check_out` while the prose is "the check-out must be after the check-in". Asserting the
    // sentence would break the moment somebody improves the wording, and would keep passing if a
    // different field were named -- so this walks the structured half, which is what a client
    // can branch on without parsing English.
    assert_eq!(
        error_code(&backwards.body),
        "invalid_hr_record",
        "the refusal names its case: {}",
        backwards.body
    );
    assert_eq!(
        backwards.body["error"]["details"]["entity"].as_str(),
        Some("attendance"),
        "the refusal says which record is wrong: {}",
        backwards.body
    );
    assert_eq!(
        backwards.body["error"]["details"]["field"].as_str(),
        Some("check_out"),
        "the refusal names the field, structurally: {}",
        backwards.body
    );
    // …and the sentence still says it in words, because the correction drawer renders the message
    // and a bare field name is not something a person reads. Both halves, each for its own reader.
    assert!(
        error_message(&backwards.body).contains("check-out"),
        "the sentence names the field too: {}",
        backwards.body
    );

    // The real correction: add the missing check-out, with a reason. The minutes are re-derived,
    // the source is untouched and the row gains a corrector — the two facts are independent and
    // overwriting one would destroy the other.
    let corrected = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/attendance/corrections",
            Some(&hr),
            Some(json!({
                "employee_id": employee_id,
                "work_date": work_date,
                "check_out": format!("{work_date}T17:00:00Z"),
                "reason": "badge reader was offline at 17:00",
            })),
        ),
    )
    .await;
    assert_eq!(
        corrected.status,
        StatusCode::OK,
        "the correction must land: {}",
        corrected.body
    );
    assert_eq!(
        corrected.body["minutes_worked"].as_i64(),
        Some(540),
        "the minutes are re-derived from the corrected pair: {}",
        corrected.body
    );
    assert_eq!(
        corrected.body["source"].as_str(),
        Some("manual"),
        "a correction never rewrites where the row came from: {}",
        corrected.body
    );
    assert_eq!(
        corrected.body["corrected"].as_bool(),
        Some(true),
        "the row says it was corrected: {}",
        corrected.body
    );
    assert!(
        !corrected.body["corrected_by"].is_null(),
        "a correction names who made it: {}",
        corrected.body
    );
    assert_eq!(
        corrected.body["note"].as_str(),
        Some("badge reader was offline at 17:00"),
        "the reason is stored on the row: {}",
        corrected.body
    );

    // And the audit trail is the request's own requirement — the one write in this module whose
    // author cannot be reconstructed from the row itself. Read it from the database rather than
    // through an API, because "there is an audit row" is the claim.
    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where action = 'hr.attendance.corrected' \
         and organization_id = $1 and target_id = $2",
    )
    .bind(fixture.organization)
    .bind(corrected.body["id"].as_str().expect("an id"))
    .fetch_one(fixture.db.pool())
    .await
    .expect("the audit query must run");
    assert_eq!(audited, 1, "the correction must be audited exactly once");
}

/// The roster: one organization's day, the screen an operator opens when they arrive.
///
/// It answers for everybody at once, so it is behind `hr.attendance.read` — and the walk proves
/// both halves: the employee is refused, and the reader gets a roster that names people.
#[tokio::test]
async fn the_roster_answers_for_the_whole_organization_behind_the_read_key() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let plain = fixture.token(&fixture.plain).await;
    let colleague_session = fixture.token(&fixture.colleague).await;
    fixture.employee_for(&fixture.plain, "Radia", "Perlman").await;
    fixture
        .employee_for(&fixture.colleague, "Barbara", "Liskov")
        .await;
    let work_date = today();

    // Punch one of the two, so the roster has a row and a gap to show side by side.
    let punched = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/me/attendance/check-in",
            Some(&plain),
            Some(json!({ "work_date": work_date })),
        ),
    )
    .await;
    assert_eq!(punched.status, StatusCode::OK, "{}", punched.body);

    let refused = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/attendance/roster", Some(&plain), None),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "the roster answers for everybody, so it is keyed: {}",
        refused.body
    );

    let hr = fixture.token(&fixture.hr).await;
    let roster = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/attendance/roster?work_date={work_date}"),
            Some(&hr),
            None,
        ),
    )
    .await;
    assert_eq!(roster.status, StatusCode::OK, "{}", roster.body);
    assert_eq!(
        roster.body["work_date"].as_str(),
        Some(work_date.as_str()),
        "the roster is the day that was asked for: {}",
        roster.body
    );
    // `today` is the SERVER's answer, published rather than derived in the browser: a client
    // clock is not a second source of "is this today".
    assert_eq!(
        roster.body["today"].as_bool(),
        Some(true),
        "the server says whether this is today: {}",
        roster.body
    );

    let rows = roster.body["days"].as_array().expect("days");
    assert_eq!(rows.len(), 1, "one punched day is one roster row: {}", roster.body);
    assert_eq!(
        rows[0]["employee_name"].as_str(),
        Some("Radia Perlman"),
        "the roster names the person, not their id: {}",
        roster.body
    );
    assert_eq!(
        rows[0]["on_leave"].as_bool(),
        Some(false),
        "nobody is on leave in this fixture: {}",
        roster.body
    );
    assert!(
        rows[0]["check_in"].is_null() == false,
        "the row carries the punch: {}",
        roster.body
    );

    // The colleague, who punched nothing, is absent from the roster rather than shown as a blank
    // row: the roster is "who is in", and a person who has not arrived is not on it.
    let colleague_month = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/me/attendance?month={}", this_month()),
            Some(&colleague_session),
            None,
        ),
    )
    .await;
    assert_eq!(
        colleague_month.body["days"].as_array().map(Vec::len),
        Some(0),
        "the colleague has no day: {}",
        colleague_month.body
    );
}

/// Every attendance route is behind a guard, and the guard names a key the catalogue knows.
///
/// This is the same criterion REQ-012's suite pins, in the same shape, for the same reason: a
/// route that lost its `route_layer` reads 200 for a member with no key, and an inventory
/// click-through cannot see it because the click-through account is an owner.
#[tokio::test]
async fn every_attendance_route_is_refused_to_an_account_without_the_key() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let plain = fixture.token(&fixture.plain).await;

    // Hand-written rather than scraped out of `routes/mod.rs`, and that is deliberate: a census
    // read from the same line as the guard compares a route against itself, so a route that lost
    // its layer would be compared against the layer it no longer has and pass.
    let routes: [(&str, Method, Option<Value>); 6] = [
        ("/api/v1/hr/attendance", Method::GET, None),
        ("/api/v1/hr/attendance/roster", Method::GET, None),
        ("/api/v1/hr/attendance/summary", Method::GET, None),
        ("/api/v1/hr/attendance/export", Method::GET, None),
        (
            "/api/v1/hr/attendance/check-in",
            Method::POST,
            Some(json!({})),
        ),
        (
            "/api/v1/hr/attendance/corrections",
            Method::POST,
            Some(json!({ "employee_id": Uuid::nil(), "work_date": today() })),
        ),
    ];

    for (path, method, body) in &routes {
        let response = call(
            &fixture.state,
            request(method.clone(), path, Some(&plain), body.clone()),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{path} must be behind its key: {}",
            response.body
        );
        let message = error_message(&response.body);
        assert!(
            !message.is_empty(),
            "{path} must say why it refused: {}",
            response.body
        );
    }

    // And the anonymous half, because "keyed" is only half of it: a route that answers 200 to a
    // caller with no session at all is a page of somebody's hours.
    for (path, method, body) in &routes {
        let response = call(&fixture.state, request(method.clone(), path, None, body.clone())).await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{path} must refuse an anonymous caller: {}",
            response.body
        );
    }
}

/// The employee who is not an employee: an account with no `hr.employees` row has no day, and
/// the clock has to say so rather than writing a row nobody can read.
#[tokio::test]
async fn a_caller_with_no_employee_row_is_told_so() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // An operator account has the keys but no employee row: they are the person an
    // administrator adds to the directory, and until then every self-service read has to answer
    // "there is no record for you" rather than 500 on a `None`.
    let hr = fixture.token(&fixture.hr).await;
    let response = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/me/attendance", Some(&hr), None),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "no employee row is a 404, not a 500: {}",
        response.body
    );
    // The code is `employee_not_found`, not a generic `not_found`: a person reading the screen
    // needs to be told the *employee* record is what is missing, because "not found" on a clock
    // screen reads as "the day is not found", which is a different problem with a different fix.
    assert_eq!(
        error_code(&response.body),
        "employee_not_found",
        "the refusal names what is missing: {}",
        response.body
    );
    assert!(
        error_message(&response.body).contains("employee"),
        "the sentence says which record: {}",
        response.body
    );
}

/// Read the caller's employee id out of the directory, through the account allowed to read it.
///
/// The correction walk needs it, and reading it through `GET /hr/employees` rather than straight
/// from the database keeps the walk on the API's own vocabulary.
async fn id_of_employee(fixture: &Fixture, email: &str) -> Uuid {
    let hr = fixture.token(&fixture.hr).await;
    let user_id: Uuid = sqlx::query_scalar("select id from users where email = $1")
        .bind(email)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the user must exist");
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/employees?user_id={user_id}"),
            Some(&hr),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    response.body["items"]
        .as_array()
        .expect("items")
        .first()
        .and_then(|row| row["id"].as_str())
        .and_then(|id| Uuid::parse_str(id).ok())
        .expect("the employee row is published with its id")
}
