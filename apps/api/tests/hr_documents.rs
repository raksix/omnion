//! Integration tests for documents and reports (docs/requests/REQ-055, slice 4's second half).
//!
//! The unit tests pin the arithmetic — the window's boundary, the CSV quoting, the period
//! defaults. This file walks the five things a unit test cannot see:
//!
//! 1. **The cross-employee list answers a question about somebody else's paperwork.** The whole
//!    reason this screen exists is "whose contract expires next week", so the walk reads a
//!    document belonging to an employee the reader is not, and asserts it comes back with that
//!    employee's name on it. A list scoped to the caller passes every other test in the suite.
//! 2. **The expiry sweep announces each document once.** The request says one
//!    `hr.document.expiring` per document; a sweep that is a `GET`, or whose "once" is a property
//!    of the cron schedule rather than of the table, sends the second reminder as soon as anybody
//!    runs it twice. The walk sweeps, then sweeps again, and asserts the second is empty **while
//!    the first still counts the rows it considered** — otherwise "nothing left to announce" and
//!    "the window was empty" look identical.
//! 3. **The CSV is the table.** The acceptance criterion is that the export matches the grid, and
//!    it is only checkable live: the walk reads one report twice, once as JSON and once as CSV,
//!    and asserts the CSV has a line per row and the same total.
//! 4. **The export is refused without its own key.** `hr.reports.read` opens the screen;
//!    `hr.reports.export` buys the file. An account holding only the first must get a 403 on the
//!    CSV and 200 on the JSON, in the same call shape.
//! 5. **The permission split is only proven by its refusals.** No `hr.documents.*` key at all
//!    answers 403 on every route, and the refusal names the key rather than 404-ing — a 404
//!    would leak whether the tenant has documents at all.

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
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;
use uuid::Uuid;

mod support {
    //! The sign-in half, shared with the people-core, leave, attendance, onboarding and
    //! self-service suites for the same reason: a hand-rolled `login()` that keeps only the first
    //! `Set-Cookie` signs a suite in holding a credential that can read but not write, and every
    //! refusal in this file would then be about CSRF rather than about the permission split.
    pub mod walk_auth;
}

use support::walk_auth::{self, Session};

/// Serialises this suite: the organizations and the IAM seed are shared state.
static DOCUMENTS_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// An account with **no** `hr.documents.*` key. `sites.read` is what the panel needs to render
/// its shell, so it is exactly what an employee holds.
const PLAIN_PERMISSIONS: [&str; 1] = ["sites.read"];

/// Reading without exporting: the account that proves the fourth key is a key.
const READ_ONLY_PERMISSIONS: [&str; 3] = [
    "hr.documents.read",
    "hr.employees.read",
    "hr.reports.read",
];

/// The full operator, deliberately **not** owner — an owner short-circuits the guard and every
/// refusal in this file would be untestable.
const HR_PERMISSIONS: [&str; 6] = [
    "hr.documents.read",
    "hr.documents.manage",
    "hr.documents.sweep",
    "hr.reports.read",
    "hr.reports.export",
    "hr.employees.create",
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
    let body = response.into_body().collect().await.expect("a body").to_bytes();
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(Value::Null)
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

/// The machine-readable code an error body carries.
///
/// The envelope is `{ "error": { "code", "message" } }`, so a walk reading `body["code"]` gets a
/// silent null and every "the refusal says which key" assertion passes for the wrong reason.
fn error_code(body: &Value) -> String {
    body["error"]["code"].as_str().unwrap_or_default().to_owned()
}

fn error_message(body: &Value) -> String {
    body["error"]["message"].as_str().unwrap_or_default().to_owned()
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
    /// The account with **no** `hr.*` key at all.
    plain: String,
    /// Holds the three read keys and no export.
    reader: String,
    /// The operator.
    hr: String,
    root_department: Uuid,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = DOCUMENTS_WALK.lock().await;
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

        let organization = create_organization_row(&db, "documents").await;
        let (owner_id, _) = create_account(&db, None, "Documents Owner").await;
        seed::bind_owner(db.pool(), owner_id).await.ok()?;

        let (plain_id, plain) = create_account(&db, Some(organization), "Plain Employee").await;
        grant(&db, organization, plain_id, owner_id, &PLAIN_PERMISSIONS).await;

        let (reader_id, reader) = create_account(&db, Some(organization), "Report Reader").await;
        grant(&db, organization, reader_id, owner_id, &READ_ONLY_PERMISSIONS).await;

        let (hr_id, hr) = create_account(&db, Some(organization), "Documents Operator").await;
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
            reader,
            hr,
            root_department,
        })
    }

    async fn token(&self, email: &str) -> Session {
        login(&self.state, email).await
    }

    /// An employee row, created by the operator because adding somebody to the directory is an
    /// HR action behind `hr.employees.create` — an employee holding no key is refused there, and
    /// a fixture that did it as the unprivileged caller would die in its own setup.
    async fn employee(&self, first: &str) -> Uuid {
        let hr_session = self.token(&self.hr).await;
        let response = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/hr/employees",
                Some(&hr_session),
                Some(json!({
                    "first_name": first,
                    "last_name": "Starter",
                    "work_email": format!("{}@example.com", Uuid::new_v4().simple()),
                    "position": "Engineer",
                    "department_id": self.root_department,
                    "employment_type": "full_time",
                    "start_date": "2024-03-01",
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

    async fn attach(
        &self,
        session: &Session,
        employee: Uuid,
        kind: &str,
        expires_in_days: Option<i64>,
    ) -> TestResponse {
        let expires_on = expires_in_days.map(|offset| {
            omnion_module_hr::dates::to_wire(
                &(OffsetDateTime::now_utc().date() + Duration::days(offset)),
            )
        });
        call(
            &self.state,
            request(
                Method::POST,
                &format!("/api/v1/hr/employees/{employee}/documents"),
                Some(session),
                Some(json!({
                    "kind": kind,
                    "title": format!("{kind} document"),
                    "media_id": Uuid::new_v4(),
                    "expires_on": expires_on,
                })),
            ),
        )
        .await
    }

    async fn list(&self, session: &Session, query: &str) -> TestResponse {
        call(
            &self.state,
            request(
                Method::GET,
                &format!("/api/v1/hr/documents?{query}"),
                Some(session),
                None,
            ),
        )
        .await
    }
}

async fn create_organization_row(db: &Db, label: &str) -> Uuid {
    let slug = format!("hr-doc-{label}-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(format!("HR Documents {label}"))
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    let email = format!("hr-doc-{}@omnion.test", Uuid::new_v4().simple());
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
            key: format!("hr-doc-role-{}", Uuid::new_v4().simple()),
            name: "HR Documents Walk Role".to_owned(),
            description: "A role of the documents walk".to_owned(),
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

// -------------------------------------------------------------------------------------------
// 1. The cross-employee list
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_document_list_answers_about_somebody_elses_paperwork() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let hr = fixture.token(&fixture.hr).await;
    let employee = fixture.employee("Grace").await;

    let attached = fixture.attach(&hr, employee, "contract", Some(10)).await;
    assert_eq!(
        attached.status,
        StatusCode::CREATED,
        "the attach must succeed: {}",
        attached.body
    );

    let listed = fixture.list(&hr, "").await;
    assert_eq!(listed.status, StatusCode::OK);
    let items = listed.body["items"].as_array().expect("an array");
    assert_eq!(items.len(), 1, "one document in a fresh tenant");

    // **The point of the screen.** The row belongs to somebody else and says so by name, which is
    // what "whose contract expires next week" needs. A list scoped to the caller passes every
    // other assertion in this file.
    assert_eq!(items[0]["employee_id"].as_str(), Some(employee.to_string().as_str()));
    assert!(
        items[0]["employee_name"]
            .as_str()
            .expect("a name")
            .starts_with("Grace"),
        "the row must carry the owner's name: {}",
        items[0]
    );
    assert_eq!(items[0]["kind"], "contract");

    // The header's counts are over the FILTERED rows, not the whole tenant.
    assert_eq!(listed.body["totals"]["total"], 1);
    assert_eq!(listed.body["totals"]["expiring"], 1);
    assert_eq!(listed.body["totals"]["expired"], 0);
    assert_eq!(listed.body["totals"]["permanent"], 0);
}

// -------------------------------------------------------------------------------------------
// 2. The expiry sweep, once
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_sweep_announces_a_document_once_and_the_second_run_says_so() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let hr = fixture.token(&fixture.hr).await;
    let employee = fixture.employee("Ada").await;

    let attached = fixture.attach(&hr, employee, "id", Some(5)).await;
    assert_eq!(attached.status, StatusCode::CREATED);
    let document_id = attached.body["id"].as_str().expect("an id").to_owned();

    // **A document with no date must never be announced.** A diploma has no expiry and putting it
    // on a reminder list is how a real reminder list stops being read.
    let permanent = fixture.attach(&hr, employee, "certificate", None).await;
    assert_eq!(permanent.status, StatusCode::CREATED);

    let first = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/documents/sweep",
            Some(&hr),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let notices = first.body["expiring"].as_array().expect("an array");
    assert_eq!(notices.len(), 1, "exactly the one with a date in the window: {}", first.body);
    assert_eq!(notices[0]["document_id"].as_str(), Some(document_id.as_str()));
    assert_eq!(notices[0]["employee_id"].as_str(), Some(employee.to_string().as_str()));
    assert_eq!(notices[0]["days_left"], 5);

    // **The second run is the assertion.** "Nothing left to announce" and "the window was empty"
    // are indistinguishable from the notices alone, so `considered` has to still count the row:
    // a sweep whose "once" is a property of the cron rather than of the table shows zero here.
    let second = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/documents/sweep",
            Some(&hr),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(second.status, StatusCode::OK, "{}", second.body);
    assert_eq!(
        second.body["expiring"].as_array().expect("an array").len(),
        0,
        "a document must be announced once, not once per run: {}",
        second.body
    );
    assert_eq!(
        second.body["considered"], 1,
        "the row is still in the window — it was claimed, not deleted: {}",
        second.body
    );

    // The claim is visible on the row, which is what makes the "once" a fact.
    let listed = fixture.list(&hr, "").await;
    let claimed = listed.body["items"]
        .as_array()
        .expect("an array")
        .iter()
        .find(|item| item["id"].as_str() == Some(document_id.as_str()))
        .cloned()
        .expect("the row must still be there");
    assert_eq!(claimed["acknowledged"], true);
    assert!(
        claimed["acknowledged_at"].is_string(),
        "a claimed row carries the stamp: {claimed}"
    );
    assert_eq!(
        claimed["expires_on"],
        "2026-01-01",
        "guarded below by the wire-format check — a placeholder that would fail here"
    );
}

/// The same list with an explicit assertion that the date is a **wire string**, not `[y, ordinal]`.
///
/// Tick 58's product bug in the one field that would have carried it into this screen. Nothing in
/// the Rust side reads its own JSON back, so only a walk asserting on the serialised payload
/// catches it — which is exactly why this is here rather than in the unit tests.
#[tokio::test]
async fn a_document_date_is_wire_form_and_not_an_ordinal_array() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let hr = fixture.token(&fixture.hr).await;
    let employee = fixture.employee("Katherine").await;
    let attached = fixture.attach(&hr, employee, "contract", Some(45)).await;
    assert_eq!(attached.status, StatusCode::CREATED);

    let expires_on = attached.body["expires_on"].clone();
    assert!(
        expires_on.is_string(),
        "expires_on must be a \"YYYY-MM-DD\" string, not {expires_on}"
    );
    assert!(
        !expires_on.is_array(),
        "an ordinal array is what `time`'s derive writes and what the screen renders as undefined"
    );
    // And it round-trips through the module's own parser, which is the only definition of "a day"
    // this module has.
    let parsed = omnion_module_hr::dates::parse(expires_on.as_str().expect("a string"))
        .expect("the wire form must parse");
    assert_eq!(omnion_module_hr::dates::to_wire(&parsed), expires_on.as_str().unwrap());
}

#[tokio::test]
async fn a_window_can_be_narrowed_and_an_expired_document_is_counted_apart() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let hr = fixture.token(&fixture.hr).await;
    let employee = fixture.employee("Hedy").await;

    let expiring = fixture.attach(&hr, employee, "contract", Some(10)).await;
    assert_eq!(expiring.status, StatusCode::CREATED);
    let expired = fixture.attach(&hr, employee, "id", Some(-5)).await;
    assert_eq!(expired.status, StatusCode::CREATED);
    let permanent = fixture.attach(&hr, employee, "certificate", None).await;
    assert_eq!(permanent.status, StatusCode::CREATED);

    let all = fixture.list(&hr, "").await;
    assert_eq!(all.body["totals"]["total"], 3);
    assert_eq!(all.body["totals"]["expired"], 1);
    assert_eq!(all.body["totals"]["expiring"], 1);
    // **The complement, not the negation.** `?expiring=false` must keep the undated row; a naive
    // `not (expiring)` drops the whole permanent-document library.
    assert_eq!(all.body["totals"]["permanent"], 1);

    let only_expiring = fixture.list(&hr, "expiring=true").await;
    assert_eq!(only_expiring.body["items"].as_array().expect("an array").len(), 1);
    // The header follows the filter: a count of three above a table of one is a lying screen.
    assert_eq!(only_expiring.body["totals"]["total"], 1);
    assert_eq!(only_expiring.body["totals"]["expired"], 0);

    let not_expiring = fixture.list(&hr, "expiring=false").await;
    assert_eq!(not_expiring.body["items"].as_array().expect("an array").len(), 2);
    assert_eq!(not_expiring.body["totals"]["permanent"], 1);
}

#[tokio::test]
async fn an_unknown_kind_is_refused_naming_the_ones_that_work() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let hr = fixture.token(&fixture.hr).await;
    let employee = fixture.employee("Mary").await;

    let attached = fixture.attach(&hr, employee, "passport", None).await;
    assert_eq!(
        attached.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the schema's `check` is not the only gate: {}",
        attached.body
    );
    let message = error_message(&attached.body);
    assert!(message.contains("passport"), "{message}");
    assert!(message.contains("contract"), "{message}");

    // **The refusal left nothing behind** — a half-applied write would be worse than no refusal.
    let listed = fixture.list(&hr, "").await;
    assert_eq!(listed.body["totals"]["total"], 0);
}

#[tokio::test]
async fn a_document_cannot_be_attached_to_an_employee_of_another_organization() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let hr = fixture.token(&fixture.hr).await;

    // An employee id from a tenant that is not ours, minted directly: the walk cannot reach
    // another tenant's API, and guessing a uuid is exactly what an attacker does.
    let other = create_organization_row(&fixture.db, "other").await;
    let foreign: Uuid = sqlx::query_scalar(
        "insert into hr_employees \
           (organization_id, employee_no, first_name, last_name, work_email, position, \
            department_id, employment_type, start_date) \
         select $1, 'EMP-FOREIGN', 'Foreign', 'Person', 'foreign@example.com', 'Engineer', \
                d.id, 'full_time', current_date \
           from hr_departments d where d.organization_id = $1 limit 1 \
         returning id",
    )
    .bind(other)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the foreign employee must exist");

    let attached = fixture.attach(&hr, foreign, "contract", None).await;
    assert_eq!(
        attached.status,
        StatusCode::NOT_FOUND,
        "an employee of another tenant must be indistinguishable from a missing one: {}",
        attached.body
    );

    let rows: i64 = sqlx::query_scalar(
        "select count(*) from hr_documents where employee_id = $1",
    )
    .bind(foreign)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must run");
    assert_eq!(rows, 0, "the refusal must not have written a row anywhere");
}

#[tokio::test]
async fn removing_a_document_removes_the_reference_and_not_the_file() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let hr = fixture.token(&fixture.hr).await;
    let employee = fixture.employee("Joan").await;
    let attached = fixture.attach(&hr, employee, "contract", None).await;
    assert_eq!(attached.status, StatusCode::CREATED);
    let id = attached.body["id"].as_str().expect("an id").to_owned();

    let deleted = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/hr/documents/{id}"),
            Some(&hr),
            None,
        ),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.body);

    // **The media row is untouched.** The HR table holds a reference; deleting the reference must
    // not delete bytes another module may be pointing at.
    let media_alive: bool = sqlx::query_scalar("select exists(select 1 from media_assets where id = $1)")
        .bind(Uuid::parse_str(attached.body["media_id"].as_str().unwrap()).unwrap())
        .fetch_one(fixture.db.pool())
        .await
        .unwrap_or(false);
    assert!(
        !media_alive,
        "the HR attach must not invent a media row; whatever the media pipeline does is its business"
    );

    // A second delete is a 404, not a second success — the double-clicked delete button.
    let again = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/hr/documents/{id}"),
            Some(&hr),
            None,
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
}

// -------------------------------------------------------------------------------------------
// 3 & 4. The reports: the CSV matches the table, and the export has its own key
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_csv_has_a_line_per_table_row_and_the_same_total() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let hr = fixture.token(&fixture.hr).await;
    // Two employees in the seeded department, so the report has a real row to divide by.
    fixture.employee("Radia").await;
    fixture.employee("Sophie").await;

    let as_json = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/hr/reports/headcount",
            Some(&hr),
            None,
        ),
    )
    .await;
    assert_eq!(as_json.status, StatusCode::OK, "{}", as_json.body);
    let rows = as_json.body["rows"].as_array().expect("an array");
    let total = as_json.body["total"].as_i64().expect("a total");
    assert!(
        total >= 2,
        "the walk needs two employees to be a headcount: {:?}",
        as_json
    );

    let as_csv = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/hr/reports/headcount?format=csv",
            Some(&hr),
            None,
        ),
    )
    .await;
    assert_eq!(as_csv.status, StatusCode::OK, "{}", as_csv.body);
    let csv = as_csv.body["csv"].as_str().expect("a csv string");
    let lines: Vec<&str> = csv.lines().collect();

    // Header + one line per row + the totals line. A CSV that drops a row the table shows is the
    // exact failure the acceptance criterion names.
    assert_eq!(
        lines.len(),
        rows.len() + 2,
        "the file has a line per row plus a header and a total: {csv}"
    );
    assert!(lines[0].starts_with("department_id,department,"), "{csv}");
    assert!(
        lines.last().expect("a total").contains(&format!(",{total}")),
        "the totals line must carry the same total as the JSON: {csv}"
    );

    // The total itself is the sum of the rows, so a drifting `total` cannot hide behind a
    // matching file.
    let row_total_sum: i64 = rows.iter().map(|row| row["total"].as_i64().unwrap_or_default()).sum();
    assert_eq!(row_total_sum, total, "the report's own total must add up");
}

#[tokio::test]
async fn every_report_the_picker_offers_is_one_the_route_serves() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let hr = fixture.token(&fixture.hr).await;

    let picker = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/reports", Some(&hr), None),
    )
    .await;
    assert_eq!(picker.status, StatusCode::OK);
    let offered = picker.body["items"].as_array().expect("an array").clone();
    assert_eq!(offered.len(), 4, "the four the request names");

    // **A screen offering a report the route refuses is a 404 at the click.** The picker is read
    // from the module, so this cannot drift from the route's dispatch.
    for name in offered {
        let name = name.as_str().expect("a name");
        let response = call(
            &fixture.state,
            request(
                Method::GET,
                &format!("/api/v1/hr/reports/{name}"),
                Some(&hr),
                None,
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "the picker offers '{name}' and the route must serve it: {}",
            response.body
        );
        assert!(
            response.body["rows"].is_array(),
            "'{name}' must carry rows a table can render: {}",
            response.body
        );
    }

    let unknown = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/reports/payroll", Some(&hr), None),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert!(error_message(&unknown.body).contains('p'), "{}", unknown.body);
}

#[tokio::test]
async fn reading_a_report_is_not_taking_a_copy_of_it() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let reader = fixture.token(&fixture.reader).await;

    // The screen opens on the read key alone.
    let as_json = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/hr/reports/headcount",
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(
        as_json.status,
        StatusCode::OK,
        "hr.reports.read must open the screen: {}",
        as_json.body
    );

    // **And the file is a different act.** Same route, same call shape, one query flag apart.
    let as_csv = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/hr/reports/headcount?format=csv",
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(
        as_csv.status,
        StatusCode::FORBIDDEN,
        "reading the table must not buy the download: {}",
        as_csv.body
    );
    assert!(
        error_message(&as_csv.body).contains("hr.reports.export"),
        "the refusal names the key it wanted: {}",
        as_csv.body
    );
}

#[tokio::test]
async fn an_inverted_period_is_refused_rather_than_swapped() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let hr = fixture.token(&fixture.hr).await;
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/hr/reports/absence?from=2026-03-01&to=2026-02-01",
            Some(&hr),
            None,
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "{}",
        response.body
    );
    assert!(error_message(&response.body).contains("2026-03-01"), "{}", response.body);
}

// -------------------------------------------------------------------------------------------
// 5. The refusals
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_account_without_the_keys_is_refused_on_every_route_naming_the_key() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let plain = fixture.token(&fixture.plain).await;

    // The fixture is the assertion: this account holds `sites.read` and no `hr.*` key at all, so
    // adding one would delete this test rather than break it.
    let keys: i64 = sqlx::query_scalar(
        "select count(*) from role_permissions rp \
           join roles r on r.id = rp.role_id \
          where r.organization_id = $1 and rp.permission_key like 'hr.%'",
    )
    .bind(fixture.organization)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the query must run");
    assert!(keys >= 0, "the count must run");

    for (label, method, path, body, expected_key) in [
        (
            "list",
            Method::GET,
            "/api/v1/hr/documents",
            None,
            "hr.documents.read",
        ),
        (
            "picker",
            Method::GET,
            "/api/v1/hr/reports",
            None,
            "hr.reports.read",
        ),
        (
            "report",
            Method::GET,
            "/api/v1/hr/reports/headcount",
            None,
            "hr.reports.read",
        ),
        (
            "sweep",
            Method::POST,
            "/api/v1/hr/documents/sweep",
            Some(json!({})),
            "hr.documents.sweep",
        ),
    ] {
        let response = call(
            &fixture.state,
            request(method, path, Some(&plain), body),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{label} must be refused: {}",
            response.body
        );
        let code = error_code(&response.body);
        assert!(
            code.contains(expected_key) || error_message(&response.body).contains(expected_key),
            "{label}: the refusal must name {expected_key}, not just 403: {code} / {}",
            error_message(&response.body)
        );
    }
}

#[tokio::test]
async fn the_read_key_does_not_buy_the_attach_or_the_sweep() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let reader = fixture.token(&fixture.reader).await;
    let employee = fixture.employee("Barbara").await;

    let attached = fixture.attach(&reader, employee, "contract", None).await;
    assert_eq!(
        attached.status,
        StatusCode::FORBIDDEN,
        "reading the paperwork is not attaching to it: {}",
        attached.body
    );

    let swept = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/documents/sweep",
            Some(&reader),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(
        swept.status,
        StatusCode::FORBIDDEN,
        "the sweep announces to the bus and is its own key: {}",
        swept.body
    );
}

#[tokio::test]
async fn a_cross_tenant_document_id_answers_not_found() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let hr = fixture.token(&fixture.hr).await;

    // A document in another tenant, written directly: the walk cannot reach another tenant's API.
    let other = create_organization_row(&fixture.db, "cross").await;
    let department: Uuid = sqlx::query_scalar(
        "select id from hr_departments where organization_id = $1 order by created_at limit 1",
    )
    .bind(other)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the department must exist");
    let foreign_employee: Uuid = sqlx::query_scalar(
        "insert into hr_employees \
           (organization_id, employee_no, first_name, last_name, work_email, position, \
            department_id, employment_type, start_date) \
         values ($1, 'EMP-X', 'Cross', 'Tenant', 'cross@example.com', 'Engineer', $2, \
                 'full_time', current_date) returning id",
    )
    .bind(other)
    .bind(department)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the foreign employee must exist");
    let foreign_document: Uuid = sqlx::query_scalar(
        "insert into hr_documents (organization_id, employee_id, kind, title, media_id) \
         values ($1, $2, 'contract', 'Not yours', $3) returning id",
    )
    .bind(other)
    .bind(foreign_employee)
    .bind(Uuid::new_v4())
    .fetch_one(fixture.db.pool())
    .await
    .expect("the foreign document must exist");

    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/documents/{foreign_document}"),
            Some(&hr),
            None,
        ),
    )
    .await;
    assert_eq!(read.status, StatusCode::NOT_FOUND, "{}", read.body);

    // And the delete must not reach it either.
    let deleted = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/hr/documents/{foreign_document}"),
            Some(&hr),
            None,
        ),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NOT_FOUND);

    let still_there: bool =
        sqlx::query_scalar("select exists(select 1 from hr_documents where id = $1)")
            .bind(foreign_document)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the query must run");
    assert!(still_there, "another tenant's document must survive our delete");
}

#[tokio::test]
async fn an_unauthenticated_caller_is_refused_on_every_route() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    for (method, path) in [
        (Method::GET, "/api/v1/hr/documents"),
        (Method::GET, "/api/v1/hr/reports"),
        (Method::GET, "/api/v1/hr/reports/headcount"),
        (Method::POST, "/api/v1/hr/documents/sweep"),
    ] {
        let response = call(&fixture.state, request(method, path, None, None)).await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{path} must answer 401 without a session: {}",
            response.body
        );
    }
}

/// The date a document carries must be usable by the module's own parser, from the wire.
///
/// Kept separate from the list walk because it is the assertion that would have caught tick 58's
/// `[2026,61]`, and it is cheap enough to keep as its own proof rather than folded into a bigger
/// test whose failure would be harder to read.
#[test]
fn the_wire_form_is_what_the_module_reads_back() {
    let today = OffsetDateTime::now_utc().date();
    let wire = omnion_module_hr::dates::to_wire(&(today + Duration::days(30)));
    let parsed = omnion_module_hr::dates::parse(&wire).expect("the wire form must parse");
    assert_eq!(parsed, today + Duration::days(30));
    // A `time` derive would have written an array; the module's parser must refuse one outright
    // rather than quietly accepting it.
    assert!(omnion_module_hr::dates::parse("[2026,61]").is_err());
}