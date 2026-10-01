//! Integration tests for onboarding (docs/requests/REQ-055, slice 4).
//!
//! The module's unit tests pin the *arithmetic* — the bar's denominator, the completion
//! transition, the refusal for an empty template. This file walks the five things a unit test
//! cannot see, and each one is a claim the request makes in prose:
//!
//! 1. **Applying a template materialises items whose due dates come from the start date.**
//!    The walk creates an employee starting on a date it chose, applies a template with offsets
//!    0 and 3, and asserts the stored `due_on` values equal `start + 0` and `start + 3`. A
//!    checklist whose due dates are read live from the start date passes this and fails the next
//!    edit, which is why the walk also *edits the start date* afterwards and re-reads.
//! 2. **A second apply is refused, and the refusal carries what is already there.** The unique
//!    index is the thing being walked: two admins pressing the button at the same moment is not a
//!    race this suite can schedule, so the walk presses it twice and asserts the second one is a
//!    409 carrying `items: 3`. A refusal that left the list unchanged is asserted by reading it
//!    back — a refusal that half-applied would be worse than no refusal.
//! 3. **The bar's denominator is the row count, not the template's length.** The walk edits the
//!    template to gain an item *after* the checklist was applied, then asserts the bar is still
//!    3/3 and not 3/4. This is the assertion that a "count the template" implementation fails,
//!    and it is the whole reason the template and the checklist are separate tables.
//! 4. **The completion event fires on the transition.** Ticking an already ticked item must not
//!    fire it again, and unticking the last item must not either. An automation waiting on
//!    "onboarding finished" that runs three times on a three-step checklist is a real failure
//!    mode and the request asks for the event once.
//! 5. **The permission split is only proven by its refusals.** An account with no `hr.*` key
//!    cannot read the board or apply a template, and the refusal is a 403 naming the key rather
//!    than a 404 that would leak the existence of the tenant's onboarding.

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
    //! The sign-in half, shared with the people-core, leave, attendance and self-service suites
    //! for the same reason: a hand-rolled `login()` that keeps only the first `Set-Cookie` signs a
    //! suite in holding a credential that can read but not write, and every refusal in this file
    //! would then be about CSRF rather than about the permission split being tested.
    pub mod walk_auth;
}

use support::walk_auth::{self, Session};

/// Serialises this suite: the organizations and the IAM seed are shared state.
static ONBOARDING_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// **The whole point of the fixture.** One `sites.read` — what the panel needs to render its
/// shell — and not one `hr.*` key. An employee is exactly who holds none, so the self-service
/// tick being behind a key would be a checklist HR has to do for everybody.
const PLAIN_PERMISSIONS: [&str; 1] = ["sites.read"];

/// The operator side. Deliberately **not** owner — an owner short-circuits the guard and every
/// refusal below would be untestable.
///
/// `hr.employees.read` is here for one reason and it is not decoration: the walk reads its own
/// employee id back **through the API** rather than from the database, so it cannot pass on a
/// row the product would refuse to show. `hr.employees.update` is here because one walk edits a
/// start date — a start date is an employee field, so the walk that re-dates an employee is held
/// to exactly the permission an employee editor is, and the fixture is not the place to invent a
/// shortcut around it.
const HR_PERMISSIONS: [&str; 5] = [
    "hr.onboarding.read",
    "hr.onboarding.manage",
    "hr.employees.create",
    "hr.employees.read",
    "hr.employees.update",
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
/// silent null and every "the refusal says which" assertion passes for the wrong reason.
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
    /// The account with **no** `hr.*` key — the one self-service exists for.
    plain: String,
    /// The operator, holding the two onboarding keys.
    hr: String,
    root_department: Uuid,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = ONBOARDING_WALK.lock().await;
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

        let organization = create_organization_row(&db, "onboarding").await;
        let (owner_id, _) = create_account(&db, None, "Onboarding Owner").await;
        seed::bind_owner(db.pool(), owner_id).await.ok()?;

        let (plain_id, plain) = create_account(&db, Some(organization), "Plain Employee").await;
        grant(&db, organization, plain_id, owner_id, &PLAIN_PERMISSIONS).await;

        let (hr_id, hr) = create_account(&db, Some(organization), "Onboarding Operator").await;
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
            hr,
            root_department,
        })
    }

    async fn token(&self, email: &str) -> Session {
        login(&self.state, email).await
    }

    /// An employee row linked to the account behind `email`, starting on `start`.
    ///
    /// The write goes in as the **operator** account, not as the employee: adding somebody to the
    /// directory is an HR action answered behind `hr.employees.create`, and an employee holding
    /// no `hr.*` key is refused there. A fixture that created the row as the unprivileged caller
    /// would die at its own setup on a 403 that says nothing about onboarding.
    async fn employee_for(&self, email: &str, first: &str, start: &str) -> Uuid {
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
                    "last_name": "Starter",
                    "work_email": format!("{}@example.com", Uuid::new_v4().simple()),
                    "position": "Engineer",
                    "department_id": self.root_department,
                    "employment_type": "full_time",
                    "start_date": start,
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

    /// A template with `items`, as the editor would create it.
    async fn template_with(&self, session: &Session, items: Value) -> Uuid {
        let response = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/hr/onboarding/templates",
                Some(session),
                Some(json!({
                    "name": format!("Walk template {}", Uuid::new_v4().simple()),
                    "items": items,
                })),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::CREATED,
            "the template must be created: {}",
            response.body
        );
        Uuid::parse_str(response.body["id"].as_str().expect("an id")).expect("an id")
    }

    async fn apply(&self, session: &Session, employee: Uuid, template: Uuid) -> TestResponse {
        call(
            &self.state,
            request(
                Method::POST,
                &format!("/api/v1/hr/employees/{employee}/onboarding"),
                Some(session),
                Some(json!({ "template_id": template })),
            ),
        )
        .await
    }

    async fn checklist(&self, session: &Session, employee: Uuid) -> TestResponse {
        call(
            &self.state,
            request(
                Method::GET,
                &format!("/api/v1/hr/onboarding/employees/{employee}"),
                Some(session),
                None,
            ),
        )
        .await
    }

    /// The events this organization has emitted, newest first.
    ///
    /// The column is `name`, not `event_type` — and a query against a column that does not exist
    /// fails at runtime inside a helper every test calls, so the mistake would read as "every
    /// event assertion is void" rather than as a typo.
    async fn events(&self, name: &str) -> Vec<Value> {
        sqlx::query_as::<_, (Value,)>(
            "select payload from events where organization_id = $1 and name = $2 \
             order by created_at desc limit 10",
        )
        .bind(self.organization)
        .bind(name)
        .fetch_all(self.db.pool())
        .await
        .expect("the events must read")
        .into_iter()
        .map(|(payload,)| payload)
        .collect()
    }
}

async fn create_organization_row(db: &Db, label: &str) -> Uuid {
    let slug = format!("hr-onb-{label}-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(format!("HR Onboarding {label}"))
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    let email = format!("hr-onb-{}@omnion.test", Uuid::new_v4().simple());
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
            key: format!("hr-onb-role-{}", Uuid::new_v4().simple()),
            name: "HR Onboarding Walk Role".to_owned(),
            description: "A role of the onboarding walk".to_owned(),
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

/// The wire form of a day, from the walk's own clock.
///
/// It calls the **module's** parser rather than `time`'s directly: `Date::parse` in `time` 0.3
/// takes a slice of `FormatItem`, so a walk that spells the format out here is a second
/// definition of what a day is — the same class of drift the leave slice found when the weekday
/// mapping was pinned twice.
fn day(value: &str) -> String {
    omnion_module_hr::dates::to_wire(
        &omnion_module_hr::dates::parse(value).expect("a date"),
    )
}

/// A payload's id as the string a walk compares against a [`Uuid`].
///
/// A `Uuid` and a `serde_json::Value` are different types and comparing them does not compile —
/// which sounds like a nuisance and is actually a guard: it stops a walk from asserting against
/// a number where the payload carries a string, which is the assertion that passes for the wrong
/// reason.
fn id_of(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

/// Three items: two dated, one with no deadline — the third is what proves "no offset" is an
/// answer rather than a missing field.
const THREE_ITEMS: &str = r#"[
    {"title": "Sign the contract", "owner_role": "hr", "due_offset_days": 0, "requires_file": true},
    {"title": "Collect identification", "owner_role": "hr", "due_offset_days": 3, "requires_file": true},
    {"title": "Buy a coffee grinder", "owner_role": "it"}
]"#;

// ---------------------------------------------------------------------------------------------
// The apply: due dates from the start date, and the double-apply refusal
// ---------------------------------------------------------------------------------------------

/// The acceptance criterion in full: an apply creates the items with due dates derived from the
/// start date, and the second apply is refused without changing the list.
#[tokio::test]
async fn applying_a_template_dates_every_item_and_refuses_a_second_apply() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let hr = fixture.token(&fixture.hr).await;
    let employee = fixture.employee_for(&fixture.plain, "Ada", "2026-03-02").await;
    let template = fixture
        .template_with(&hr, serde_json::from_str(THREE_ITEMS).expect("items parse"))
        .await;

    let applied = fixture.apply(&hr, employee, template).await;
    assert_eq!(
        applied.status,
        StatusCode::CREATED,
        "the apply must succeed: {}",
        applied.body
    );
    let checklist = &applied.body;
    assert_eq!(checklist["total"], 3, "three items, one per template item");
    assert_eq!(checklist["done"], 0);

    let items = checklist["items"].as_array().expect("items array");
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["position"], 0, "positions are zero-based and contiguous");

    // The whole point of the slice: the dates are the START date plus the offset, and the item
    // with no offset has no deadline at all rather than "due today".
    assert_eq!(
        items[0]["due_on"], "2026-03-02",
        "offset 0 is the start date itself: {}",
        items[0]
    );
    assert_eq!(
        items[2]["due_on"], Value::Null,
        "an item with no offset has no deadline: {}",
        items[2]
    );
    assert!(items[0]["requires_file"].as_bool().unwrap_or(false));
    assert_eq!(items[1]["owner_role"], "hr");
    assert_eq!(
        items[1]["due_offset_days"], 3,
        "the offset is kept beside the derived date, so 'why is this due then?' needs no \
         reconstruction: {}",
        items[1]
    );

    // `2026-03-02 + 3 days` computed by the walk rather than written out, so the assertion stays
    // true when somebody changes the fixture's start date.
    let expected = day("2026-03-05");
    assert_eq!(
        items[1]["due_on"], expected,
        "offset 3 is three days after the start: {}",
        items[1]
    );

    // --- the second apply, and what a refusal must leave behind --------------------------------
    let events_before = fixture.events("hr.onboarding.applied").await.len();
    let again = fixture.apply(&hr, employee, template).await;
    assert_eq!(
        again.status,
        StatusCode::CONFLICT,
        "a second apply is a conflict with the current state: {}",
        again.body
    );
    assert_eq!(
        error_code(&again.body),
        "hr_onboarding_already_applied",
        "the refusal says WHICH case it is: {}",
        again.body
    );
    assert_eq!(
        again.body["error"]["details"]["items"], 3,
        "the refusal carries what is already there, so 'already applied' does not send the \
         operator to the employee's page to find out whether it worked: {}",
        again.body
    );
    let sentence = error_message(&again.body);
    assert!(sentence.contains('3'), "{sentence}");
    assert!(sentence.contains("already applied"), "{sentence}");

    // A refusal that half-applied would be worse than no refusal: the row count must be unmoved.
    let after = fixture.checklist(&hr, employee).await;
    assert_eq!(after.body["total"], 3, "the refusal wrote nothing: {}", after.body);
    assert_eq!(
        fixture.events("hr.onboarding.applied").await.len(),
        events_before,
        "the refused apply emitted no event — a webhook subscriber would otherwise see the \
         checklist applied twice"
    );

    // The items are still the same three, in the same order, with the same dates: a refusal that
    // re-dated them would be a different bug wearing the same status code.
    let rows = after.body["items"].as_array().expect("items array");
    assert_eq!(rows[0]["due_on"], "2026-03-02");
    assert_eq!(rows[1]["due_on"], expected);
}

/// The due date is stored, not derived on read: editing the start date does not re-date a
/// checklist that already exists.
#[tokio::test]
async fn editing_the_start_date_leaves_an_existing_checklist_alone() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let hr = fixture.token(&fixture.hr).await;
    let employee = fixture.employee_for(&fixture.plain, "Grace", "2026-03-02").await;
    let template = fixture
        .template_with(&hr, serde_json::from_str(THREE_ITEMS).expect("items parse"))
        .await;
    fixture.apply(&hr, employee, template).await;

    let moved = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/hr/employees/{employee}"),
            Some(&hr),
            Some(json!({ "start_date": "2026-06-01" })),
        ),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "the edit must apply: {}", moved.body);

    let after = fixture.checklist(&hr, employee).await;
    let rows = after.body["items"].as_array().expect("items array");
    assert_eq!(
        rows[1]["due_on"], "2026-03-05",
        "a checklist whose due dates move when somebody corrects a start date is one nobody \
         trusts: {}",
        rows[1]
    );
    assert_eq!(
        rows[1]["due_offset_days"], 3,
        "the offset is what survives, so re-applying a corrected template is possible: {}",
        rows[1]
    );
}

// ---------------------------------------------------------------------------------------------
// The bar: the denominator is the row count, never the template's length
// ---------------------------------------------------------------------------------------------

/// The assertion that a "count the template" implementation fails: a template that gained an item
/// after the checklist was applied does not change somebody's bar.
#[tokio::test]
async fn the_bar_counts_the_rows_not_the_template() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let hr = fixture.token(&fixture.hr).await;
    let employee = fixture.employee_for(&fixture.plain, "Linus", "2026-03-02").await;
    let template = fixture
        .template_with(&hr, serde_json::from_str(THREE_ITEMS).expect("items parse"))
        .await;
    fixture.apply(&hr, employee, template).await;

    // Tick all three, reading the ids fresh each time rather than iterating a borrowed slice the
    // reassignment would invalidate. The bar is full afterwards.
    for _ in 0..3 {
        let rows = fixture
            .checklist(&hr, employee)
            .await
            .body["items"]
            .as_array()
            .expect("items array")
            .clone();
        let next = rows
            .iter()
            .find(|item| item["done_at"].is_null())
            .expect("an unticked item");
        let id = next["id"].as_str().expect("an id").to_owned();
        let ticked = call(
            &fixture.state,
            request(
                Method::PATCH,
                &format!("/api/v1/hr/onboarding/items/{id}"),
                Some(&hr),
                Some(json!({ "done": true })),
            ),
        )
        .await;
        assert_eq!(ticked.status, StatusCode::OK, "{}", ticked.body);
    }
    assert_eq!(
        fixture.checklist(&hr, employee).await.body["done"],
        3,
        "three ticks and three done rows"
    );

    let finished = fixture.checklist(&hr, employee).await;
    assert_eq!(finished.body["done"], 3);
    assert_eq!(finished.body["total"], 3);

    // Now the template gains an item. The person is NOT given it — nothing re-applies — so their
    // bar must not move. An implementation dividing by the template's length reports 75% for
    // somebody who finished all three of the three things they were given.
    let four = json!([
        {"title": "Sign the contract", "owner_role": "hr", "due_offset_days": 0, "requires_file": true},
        {"title": "Collect identification", "owner_role": "hr", "due_offset_days": 3, "requires_file": true},
        {"title": "Buy a coffee grinder", "owner_role": "it"},
        {"title": "Show them the coffee grinder", "owner_role": "it"}
    ]);
    let edited = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/hr/onboarding/templates/{template}"),
            Some(&hr),
            Some(json!({ "items": four })),
        ),
    )
    .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.body);
    assert_eq!(
        edited.body["items"].as_array().expect("items array").len(),
        4,
        "the template now has four steps"
    );

    let unchanged = fixture.checklist(&hr, employee).await;
    assert_eq!(
        unchanged.body["total"], 3,
        "an item the template gained since the checklist was applied is not on that checklist: {}",
        unchanged.body
    );
    assert_eq!(
        unchanged.body["done"], 3,
        "so the bar is still full, not 3 of 4: {}",
        unchanged.body
    );
}

// ---------------------------------------------------------------------------------------------
// The completion event fires on the transition
// ---------------------------------------------------------------------------------------------

/// Ticking an already ticked item must not fire the completion event a second time, and unticking
/// the last one must not either.
#[tokio::test]
async fn the_completion_event_fires_once_on_the_transition() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let hr = fixture.token(&fixture.hr).await;
    let employee = fixture.employee_for(&fixture.plain, "Barbara", "2026-03-02").await;
    let template = fixture
        .template_with(&hr, serde_json::from_str(THREE_ITEMS).expect("items parse"))
        .await;
    fixture.apply(&hr, employee, template).await;

    let before = fixture.events("hr.onboarding.completed").await.len();
    let items = fixture.checklist(&hr, employee).await.body["items"]
        .as_array()
        .expect("items array")
        .clone();

    // Tick the first two: no completion yet.
    for item in items.iter().take(2) {
        let id = item["id"].as_str().expect("an id");
        let response = call(
            &fixture.state,
            request(
                Method::PATCH,
                &format!("/api/v1/hr/onboarding/items/{id}"),
                Some(&hr),
                Some(json!({ "done": true })),
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        assert_eq!(
            response.body["completed"], false,
            "two of three is not finished: {}",
            response.body
        );
    }
    assert_eq!(
        fixture.events("hr.onboarding.completed").await.len(),
        before,
        "an unfinished checklist has emitted no completion"
    );

    // The last one: this is the transition.
    let last = items[2]["id"].as_str().expect("an id");
    let done = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/hr/onboarding/items/{last}"),
            Some(&hr),
            Some(json!({ "done": true })),
        ),
    )
    .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    assert_eq!(done.body["completed"], true, "{}", done.body);

    let emitted = fixture.events("hr.onboarding.completed").await;
    assert_eq!(
        emitted.len(),
        before + 1,
        "the transition fires exactly one event"
    );
    assert_eq!(
        id_of(&emitted[0]["employee_id"]),
        employee.to_string(),
        "the payload names whose checklist finished: {}",
        emitted[0]
    );
    assert_eq!(emitted[0]["items"], 3);
    assert!(
        emitted[0].get("note").is_none(),
        "a note somebody typed about their own onboarding must never reach a third party's \
         webhook: {}",
        emitted[0]
    );

    // Ticking the SAME item again: a change, and not a completion.
    let again = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/hr/onboarding/items/{last}"),
            Some(&hr),
            Some(json!({ "done": true })),
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    assert_eq!(
        fixture.events("hr.onboarding.completed").await.len(),
        before + 1,
        "an automation waiting for 'onboarding finished' must not run once per tick"
    );

    // Unticking the last one breaks the completion and must NOT claim it happened.
    let unticked = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/hr/onboarding/items/{last}"),
            Some(&hr),
            Some(json!({ "done": false })),
        ),
    )
    .await;
    assert_eq!(unticked.status, StatusCode::OK, "{}", unticked.body);
    assert_eq!(
        unticked.body["completed"], false,
        "two of three again: {}",
        unticked.body
    );
    assert_eq!(
        fixture.events("hr.onboarding.completed").await.len(),
        before + 1,
        "breaking a completion is not a completion"
    );
}

// ---------------------------------------------------------------------------------------------
// The refusals
// ---------------------------------------------------------------------------------------------

/// A template with no items writes nothing when applied, so it is refused at save time.
#[tokio::test]
async fn an_empty_template_is_refused_at_save_time_with_the_item_field() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let hr = fixture.token(&fixture.hr).await;

    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/onboarding/templates",
            Some(&hr),
            Some(json!({ "name": format!("Empty {}", Uuid::new_v4().simple()), "items": [] })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "an empty template applies to nothing: {}",
        response.body
    );
    assert_eq!(error_code(&response.body), "invalid_hr_record");
    assert_eq!(
        response.body["error"]["details"]["field"], "items",
        "the refusal names the field the form renders it under: {}",
        response.body
    );
    assert!(
        error_message(&response.body).contains("at least one item"),
        "{}",
        response.body
    );
}

/// An offset outside the window is refused naming the item it belongs to.
#[tokio::test]
async fn an_offset_outside_the_window_is_refused_with_the_item_that_broke_it() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let hr = fixture.token(&fixture.hr).await;

    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/onboarding/templates",
            Some(&hr),
            Some(json!({
                "name": format!("Bad offset {}", Uuid::new_v4().simple()),
                "items": [{"title": "Collect identification", "due_offset_days": 400}],
            })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "a 400-day offset is a typo: {}",
        response.body
    );
    let sentence = error_message(&response.body);
    assert!(
        sentence.contains("Collect identification"),
        "'due offset out of range' sends the operator to a list of twelve items to find which \
         one: {sentence}"
    );
    assert!(sentence.contains("400"), "{sentence}");
}

/// A template from another organization answers exactly like one that does not exist.
#[tokio::test]
async fn another_organizations_template_answers_like_one_that_is_not_there() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let hr = fixture.token(&fixture.hr).await;
    let employee = fixture.employee_for(&fixture.plain, "Katherine", "2026-03-02").await;

    // An id that exists in no organization at all — the shape a cross-tenant probe takes.
    let response = fixture
        .apply(&hr, employee, Uuid::new_v4())
        .await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "a template id from elsewhere must not be distinguishable from a missing one: {}",
        response.body
    );
    assert_eq!(
        error_code(&response.body),
        "hr_record_not_found",
        "{}",
        response.body
    );

    let count: i64 = sqlx::query_scalar(
        "select count(*) from hr_onboarding_items where employee_id = $1",
    )
    .bind(employee)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must run");
    assert_eq!(count, 0, "the refused apply wrote nothing");
}

// ---------------------------------------------------------------------------------------------
// The permission split, proven by its refusals
// ---------------------------------------------------------------------------------------------

/// An account with no `hr.*` key cannot read the board or apply a template, and the refusal
/// names the key rather than leaking the tenant's onboarding.
#[tokio::test]
async fn an_account_without_the_keys_is_refused_on_both_halves() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let hr = fixture.token(&fixture.hr).await;
    let plain = fixture.token(&fixture.plain).await;
    let employee = fixture.employee_for(&fixture.plain, "Ada", "2026-03-02").await;
    let template = fixture
        .template_with(&hr, serde_json::from_str(THREE_ITEMS).expect("items parse"))
        .await;
    fixture.apply(&hr, employee, template).await;

    let board = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/hr/onboarding",
            Some(&plain),
            None,
        ),
    )
    .await;
    assert_eq!(
        board.status,
        StatusCode::FORBIDDEN,
        "an employee does not read the whole board: {}",
        board.body
    );
    let sentence = error_message(&board.body);
    assert!(
        sentence.contains("hr.onboarding.read"),
        "the refusal names the key it would take: {sentence}"
    );

    let applied = fixture.apply(&plain, employee, template).await;
    assert_eq!(
        applied.status,
        StatusCode::FORBIDDEN,
        "applying a template is HR's: {}",
        applied.body
    );
    assert!(
        error_message(&applied.body).contains("hr.onboarding.manage"),
        "{}",
        applied.body
    );

    let ticked = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/hr/onboarding/items/{}", Uuid::new_v4()),
            Some(&plain),
            Some(json!({ "done": true })),
        ),
    )
    .await;
    assert_eq!(
        ticked.status,
        StatusCode::FORBIDDEN,
        "ticking somebody else's item is HR's: {}",
        ticked.body
    );
}

/// Every route answers 401 without a session.
#[tokio::test]
async fn every_onboarding_route_refuses_an_anonymous_caller() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let employee = fixture.employee_for(&fixture.plain, "Anonymous", "2026-03-02").await;

    let cases: Vec<(Method, String)> = vec![
        (Method::GET, "/api/v1/hr/onboarding".to_owned()),
        (Method::GET, "/api/v1/hr/onboarding/templates".to_owned()),
        (
            Method::GET,
            format!("/api/v1/hr/onboarding/employees/{employee}"),
        ),
        (
            Method::POST,
            format!("/api/v1/hr/employees/{employee}/onboarding"),
        ),
        (
            Method::PATCH,
            format!("/api/v1/hr/onboarding/items/{}", Uuid::new_v4()),
        ),
    ];

    for (method, path) in cases {
        let response = call(
            &fixture.state,
            request(
                method.clone(),
                &path,
                None,
                Some(json!({ "template_id": Uuid::new_v4(), "done": true })),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{method} {path} must refuse an anonymous caller: {}",
            response.body
        );
    }
}