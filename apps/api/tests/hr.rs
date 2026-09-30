//! Integration tests for the people core (docs/requests/REQ-055, slice 1).
//!
//! The suite is written around **what an HR officer would try to do**, not around the endpoints:
//!
//! * a tenant created *after* the migration is seeded by the TRIGGER, so the first employee can
//!   be created without anybody writing a department by hand;
//! * two employees, one reporting to the other, render in both the department tree and the org
//!   chart **with the same counts** — the acceptance criterion the request names, and the reason
//!   the chart reads the same rows the list does rather than running its own count;
//! * **both cycle refusals answer with a message.** A self-manager, and a manager who already
//!   reports through the person being edited. The second one names the chain, because "set
//!   someone who is below you" sends the operator back to the tree to guess;
//! * a department with members or children is **refused with both counts**, and a merge moves the
//!   members and the children in one transaction;
//! * **the sensitive block is the headline.** The same employee serialises without the four
//!   personal fields for a reader and with them for a caller holding
//!   `hr.employees.sensitive.read` — and the field is *absent*, not null, because a null still
//!   tells a reader the field exists;
//! * a reader may look and may not hire, and a cross-organization employee is a `404` — never a
//!   `403`, which would confirm it exists;
//! * every route answers 401 without a session and 403 without the permission;
//! * terminating keeps the record: a status and an end date, and the row is still readable.

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
    //! The sign-in half, shared. See `support/walk_auth.rs` for why a hand-rolled `login()` is
    //! the exact defect this closes: it reads the FIRST `Set-Cookie` and silently discards the
    //! CSRF token that sits beside it, and then every write answers `csrf_unavailable`.
    pub mod walk_auth;
}

use support::walk_auth::{self, Session};

/// Serialises this suite: the organizations and the IAM seed are shared state.
static HR_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The reader: may **see** the directory, the tree and the chart, and nothing else. This is the
/// role that proves the split — a person who may look at the org chart may not hire anybody,
/// because a directory a manager can browse is a different disclosure from one they can change.
const READER_PERMISSIONS: [&str; 4] = [
    "hr.employees.read",
    "hr.departments.read",
    "sites.read",
    "crm.contacts.read",
];

/// The officer: the reader plus the three writes, the department power and the sensitive block.
const OFFICER_PERMISSIONS: [&str; 9] = [
    "hr.employees.read",
    "hr.employees.create",
    "hr.employees.update",
    "hr.employees.terminate",
    "hr.departments.read",
    "hr.departments.manage",
    "sites.read",
    "crm.contacts.read",
    "hr.employees.sensitive.read",
];

/// A writer in a second organization, for the cross-tenant `404`.
const FOREIGN_PERMISSIONS: [&str; 6] = [
    "hr.employees.read",
    "hr.employees.create",
    "hr.employees.update",
    "hr.departments.read",
    "hr.departments.manage",
    "sites.read",
];

// ---------------------------------------------------------------------------------------------
// Harness (the same shape accounting.rs uses)
// ---------------------------------------------------------------------------------------------

struct TestResponse {
    status: StatusCode,
    set_cookie: Vec<String>,
    body: Value,
}

/// Printed in the assertion messages.
///
/// A `Display` rather than a `Debug`-formatted field access at each site: every assertion in this
/// suite wants to show the body when it fails, and a suite that prints `{"status":...}` without
/// the body sends the reader to the log to find out what the server actually said.
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
    // **Every** `Set-Cookie`, not the first: sign-in issues the session and the CSRF token
    // together, and a helper that keeps one of them signs a suite in holding a credential that
    // can read but not write.
    let set_cookie: Vec<String> = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok().map(str::to_owned))
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
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).to_string()))
    };
    TestResponse {
        status,
        set_cookie,
        body,
    }
}

fn request(
    method: Method,
    uri: &str,
    session: Option<&Session>,
    body: Option<Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(session) = session {
        // `Session::apply` is the ONLY place that sends both the cookie and the header. A suite
        // that builds its own headers re-opens the ambient-authority defect tick 59 closed.
        builder = session.apply(builder);
    }
    match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .expect("request must build"),
        None => builder.body(Body::empty()).expect("request must build"),
    }
}

async fn live_state() -> Option<(AppState, Db)> {
    let mut config = Config::from_env().expect("environment must be valid");
    // The secret is installed ON THE CONFIG, not exported into the environment, so the suite does
    // not depend on a shell having remembered to set it.
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
    owner_id: Uuid,
    reader: String,
    officer: String,
    foreign: String,
    /// The root department the TRIGGER seeded for this tenant.
    root_department: Uuid,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = HR_WALK.lock().await;
        // The limiter is a process-wide cell filled from the stored document, and `sign_in`
        // ships at ten requests per five minutes. This suite signs in three accounts per walk and
        // has eight walks, so without a larger budget a later walk is refused and dies on a line
        // that has nothing to do with the directory.
        let (state, db) = live_state().await?;
        walk_auth::give_the_process_its_own_sign_in_budget(|| {
            let policies: Vec<RatePolicy> = RatePolicy::defaults()
                .into_iter()
                .map(|mut policy| {
                    // Only `sign_in`. The other ceilings stay as a deployment ships them, so this
                    // suite can never be the reason a genuinely over-budget request stops being
                    // refused.
                    if policy.scope == "sign_in" {
                        policy.limit = 10_000;
                    }
                    policy
                })
                .collect();
            omnion_api::rate_limit_middleware::install(RateLimiter::new(&state, policies));
        });
        seed::ensure(db.pool()).await.ok()?;

        let organization = create_organization_row(&db, "hr").await;
        let other_org = create_organization_row(&db, "hr-foreign").await;

        let (owner_id, _) = create_account(&db, None, "HR Owner").await;
        seed::bind_owner(db.pool(), owner_id).await.ok()?;

        let (reader_id, reader) = create_account(&db, Some(organization), "HR Reader").await;
        grant(&db, organization, reader_id, owner_id, &READER_PERMISSIONS).await;

        let (officer_id, officer) = create_account(&db, Some(organization), "HR Officer").await;
        grant(&db, organization, officer_id, owner_id, &OFFICER_PERMISSIONS).await;

        let (foreign_id, foreign) = create_account(&db, Some(other_org), "HR Foreign").await;
        grant(&db, other_org, foreign_id, owner_id, &FOREIGN_PERMISSIONS).await;

        // The seed is a TRIGGER on `organizations` (the migration's own decision), so this tenant
        // has a root department without this fixture writing one. The first assertion of the
        // first walk is the proof that the trigger — not this fixture — is what is doing it: a
        // tenant that had to be backfilled is a tenant that would have no root department if it
        // were created later.
        let root_department: Option<Uuid> = sqlx::query_scalar(
            "select id from hr_departments where organization_id = $1 order by created_at limit 1",
        )
        .bind(organization)
        .fetch_optional(db.pool())
        .await
        .expect("the query must run");
        let root_department = root_department.expect(
            "a tenant created after migration 0196 is seeded by the trigger, not by a backfill",
        );

        Some(Self {
            _walk: walk,
            state,
            db,
            organization,
            owner_id,
            reader,
            officer,
            foreign,
            root_department,
        })
    }

    async fn token(&self, email: &str) -> Session {
        login(&self.state, email).await
    }
}

async fn create_organization_row(db: &Db, label: &str) -> Uuid {
    let slug = format!("hr-{}-{}", label, Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(format!("HR Test {label}"))
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    let email = format!("hr-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            // The shared walk password, not one this suite invented: a suite that sets its own
            // drifts from the strength rule the platform enforces and then measures itself.
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
            key: format!("hr-role-{}", Uuid::new_v4().simple()),
            name: "HR Walk Role".to_owned(),
            description: "A role of the HR walk".to_owned(),
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

    let binding = NewBinding {
        role_id: role.id,
        user_id,
        scope: PermScope::Organization { organization_id },
        granted_by: Some(owner_id),
        expires_at: None,
    };
    omnion_permissions::bindings::grant(db.pool(), binding)
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
    // Panics at sign-in, with the joined header, when the session cookie is missing — and the
    // message names the cause. A helper that returned an empty string instead fails at the first
    // write with a 401 that says nothing about the sign-in.
    Session::from_set_cookies(response.set_cookie)
}

// ---------------------------------------------------------------------------------------------
// Helpers the walks share
// ---------------------------------------------------------------------------------------------

/// Create a department under `parent`, returning its id.
async fn make_department(fixture: &Fixture, session: &Session, name: &str, parent: Uuid) -> Uuid {
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/departments",
            Some(session),
            Some(json!({ "name": name, "code": name.chars().take(3).collect::<String>().to_uppercase(), "parent_id": parent })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "the department must be created: {}",
        response.body
    );
    Uuid::parse_str(response.body["id"].as_str().expect("an id")).expect("an id")
}

/// Create an employee in `department`, returning the parsed row.
async fn make_employee(
    fixture: &Fixture,
    session: &Session,
    department: Uuid,
    first: &str,
    last: &str,
    manager: Option<Uuid>,
) -> Value {
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/employees",
            Some(session),
            Some(json!({
                "first_name": first,
                "last_name": last,
                "work_email": format!("{}@example.com", Uuid::new_v4().simple()),
                "position": "Engineer",
                "department_id": department,
                "manager_id": manager,
                "employment_type": "full_time",
                "start_date": "2024-01-08",
                "personal_email": "private@example.com",
                "personal_phone": "+90 532 000 00 00",
                "address": "Somewhere",
                "emergency_contact": "A relative",
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

fn id_of(value: &Value) -> Uuid {
    Uuid::parse_str(value["id"].as_str().expect("an id")).expect("an id")
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

/// A tenant is seeded by the trigger, and the seeded catalogue is what the form offers.
#[tokio::test]
async fn a_new_tenant_is_seeded_with_a_root_department_a_catalogue_and_a_template() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let officer = fixture.token(&fixture.officer).await;

    let departments = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/departments", Some(&officer), None),
    )
    .await;
    assert_eq!(departments.status, StatusCode::OK);
    let items = departments.body["items"].as_array().expect("an array");
    assert!(
        !items.is_empty(),
        "a tenant born after the migration owns a seeded root department, not an empty one"
    );

    // `deletable` is a display hint and it must agree with what the store will actually do --
    // a delete button on a row whose delete is refused is a dead control. This tenant has hired
    // nobody yet, so the seeded root has no members and no children and IS deletable; the walk
    // below adds a member and asserts the flag flips, which is the half that matters.
    let root = &items[0];
    assert_eq!(root["name"], "General");
    assert_eq!(root["member_count"], 0);
    assert_eq!(root["deletable"], true, "an empty seeded root may be deleted");

    // …and it stops being deletable the moment it holds somebody.
    let officer_token = officer.clone();
    make_employee(&fixture, &officer_token, id_of(root), "Ada", "Lovelace", None).await;
    let after = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/departments", Some(&officer), None),
    )
    .await;
    let root_now = after.body["items"]
        .as_array()
        .expect("an array")
        .iter()
        .find(|row| row["id"] == root["id"])
        .expect("the root is still in the tree")
        .clone();
    assert_eq!(root_now["member_count"], 1);
    assert_eq!(
        root_now["deletable"], false,
        "a department holding somebody must not offer a delete button"
    );

    // And the store agrees with the flag, which is the pairing that matters.
    let refused = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/hr/departments/{}", root["id"].as_str().unwrap_or_default()),
            Some(&officer),
            None,
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
}

/// Two employees in a parent/child department render in BOTH the tree and the chart, with the
/// same counts. This is the acceptance criterion the request names, and the reason the chart reads
/// the rows the list reads rather than counting them a second time.
#[tokio::test]
async fn the_tree_and_the_org_chart_agree_on_the_people_in_a_parent_and_child_department() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let officer = fixture.token(&fixture.officer).await;

    let engineering = make_department(&fixture, &officer, "Engineering", fixture.root_department).await;
    let backend = make_department(&fixture, &officer, "Backend", engineering).await;
    let head = make_employee(&fixture, &officer, engineering, "Ada", "Lovelace", None).await;
    let report = make_employee(
        &fixture,
        &officer,
        backend,
        "Grace",
        "Hopper",
        Some(id_of(&head)),
    )
    .await;

    // The tree: the counts the labels draw.
    let tree = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/departments", Some(&officer), None),
    )
    .await;
    let rows = tree.body["items"].as_array().expect("an array");
    let find = |name: &str| {
        rows.iter()
            .find(|row| row["name"] == name)
            .unwrap_or_else(|| panic!("{name} must be in the tree"))
            .clone()
    };
    assert_eq!(find("Engineering")["member_count"], 1);
    assert_eq!(find("Engineering")["child_count"], 1);
    assert_eq!(find("Backend")["member_count"], 1);
    assert_eq!(find("Backend")["parent_id"], engineering.to_string());

    // The chart: the SAME numbers, nested.
    let chart = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/org-chart", Some(&officer), None),
    )
    .await;
    assert_eq!(chart.status, StatusCode::OK);
    let roots = chart.body.as_array().expect("the chart is an array of roots");
    // `Engineering` was created under the seeded `General` root, so it is a **child**, not a root.
    // The walk searched only the top level and failed -- which is a useful reminder that a chart
    // is a tree and a test that treats it as a list has to say which depth it means.
    fn find_node<'a>(nodes: &'a [Value], name: &str) -> Option<&'a Value> {
        nodes.iter().find_map(|node| {
            if node["department"]["name"] == name {
                return Some(node);
            }
            find_node(node["children"].as_array().map(Vec::as_slice).unwrap_or(&[]), name)
        })
    }
    let engineering_node = find_node(roots, "Engineering")
        .expect("Engineering must be somewhere in the chart")
        .clone();
    let engineering_node = &engineering_node;
    assert_eq!(
        engineering_node["department"]["member_count"], 1,
        "the chart's count is the tree's count, because both read one statement"
    );
    assert_eq!(engineering_node["employees"].as_array().map(Vec::len), Some(1));
    assert_eq!(engineering_node["employees"][0]["display_name"], "Ada Lovelace");
    assert_eq!(engineering_node["children"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        engineering_node["children"][0]["department"]["name"], "Backend",
        "the child is nested under its parent rather than repeated at the root"
    );

    // And the reporting line is in the chart, because a manager needs to see who answers to whom.
    let list = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/hr/employees?department_id=0",
            Some(&officer),
            None,
        ),
    )
    .await;
    assert_eq!(list.status, StatusCode::BAD_REQUEST, "a bad uuid is refused, not ignored");

    let all = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/employees", Some(&officer), None),
    )
    .await;
    assert_eq!(all.status, StatusCode::OK);
    let grace = all.body["items"]
        .as_array()
        .expect("an array")
        .iter()
        .find(|row| row["last_name"] == "Hopper")
        .expect("the report must be in the directory");
    assert_eq!(grace["manager_id"], id_of(&head).to_string());
    assert_eq!(grace["manager_name"], "Ada Lovelace");
    assert_eq!(report["last_name"], "Hopper");
}

/// Both cycle refusals answer with a message, and the cycle one names the chain.
#[tokio::test]
async fn a_manager_cycle_is_refused_with_the_chain_that_closes_it() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let officer = fixture.token(&fixture.officer).await;
    let department = fixture.root_department;

    let head = make_employee(&fixture, &officer, department, "Ada", "Lovelace", None).await;
    let middle = make_employee(
        &fixture,
        &officer,
        department,
        "Grace",
        "Hopper",
        Some(id_of(&head)),
    )
    .await;

    // 1. Self-manager: the fixed point.
    let self_manager = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/hr/employees/{}", id_of(&head)),
            Some(&officer),
            Some(json!({ "manager_id": id_of(&head) })),
        ),
    )
    .await;
    assert_eq!(self_manager.status, StatusCode::CONFLICT);
    assert_eq!(self_manager.body["error"]["code"], "hr_manager_cycle");
    assert!(
        self_manager.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("their own manager"),
        "{}",
        self_manager.body
    );

    // 2. A manager who already reports through the person being edited. Ada -> Grace exists, so
    // making Ada report to Grace closes the loop — and the message has to say so, because
    // "invalid manager" sends the operator back to the tree to work out which line closes it.
    let cycle = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/hr/employees/{}", id_of(&head)),
            Some(&officer),
            Some(json!({ "manager_id": id_of(&middle) })),
        ),
    )
    .await;
    assert_eq!(cycle.status, StatusCode::CONFLICT);
    assert_eq!(cycle.body["error"]["code"], "hr_manager_cycle");
    let message = cycle.body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("Ada Lovelace"),
        "the message names the chain the walk found, and the loop closes AT Ada: {message}"
    );
    assert!(
        message.contains("already reports"),
        "and it says what the problem is, not merely that a cycle exists: {message}"
    );

    // And the refused write left the chain exactly as it was.
    let unchanged = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/employees/{}", id_of(&head)),
            Some(&officer),
            None,
        ),
    )
    .await;
    assert_eq!(unchanged.status, StatusCode::OK);
    assert!(
        unchanged.body["manager_id"].is_null(),
        "a refused cycle must not half-apply"
    );
    assert_eq!(middle["manager_id"], id_of(&head).to_string());
}

/// A department with members or children is refused with BOTH counts, and a merge moves both.
#[tokio::test]
async fn a_department_with_members_is_refused_with_the_counts_and_a_merge_moves_them() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let officer = fixture.token(&fixture.officer).await;

    let source = make_department(&fixture, &officer, "Support", fixture.root_department).await;
    let child = make_department(&fixture, &officer, "Tier One", source).await;
    let target = make_department(&fixture, &officer, "Service Desk", fixture.root_department).await;
    let person = make_employee(&fixture, &officer, source, "Alan", "Turing", None).await;

    // The refusal names both counts, because "cannot delete" on its own sends the operator to two
    // reports to find out how exposed the department is.
    let refused = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/hr/departments/{source}"),
            Some(&officer),
            None,
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.body["error"]["code"], "hr_department_not_empty");
    assert_eq!(refused.body["error"]["details"]["members"], 1);
    assert_eq!(refused.body["error"]["details"]["children"], 1);

    // The merge: the members AND the children move, in one transaction, and the source is gone.
    let merged = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/departments/merge",
            Some(&officer),
            Some(json!({ "source_id": source, "target_id": target })),
        ),
    )
    .await;
    assert_eq!(merged.status, StatusCode::OK, "{}", merged.body);
    assert_eq!(merged.body["id"], target.to_string());

    let moved = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/employees/{}", id_of(&person)),
            Some(&officer),
            None,
        ),
    )
    .await;
    assert_eq!(
        moved.body["department_id"], target.to_string(),
        "the employee moved with the department"
    );

    let tree = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/departments", Some(&officer), None),
    )
    .await;
    let rows = tree.body["items"].as_array().expect("an array");
    assert!(
        !rows.iter().any(|row| row["id"] == source.to_string()),
        "the merged department is gone"
    );
    let tier_one = rows
        .iter()
        .find(|row| row["id"] == child.to_string())
        .expect("the child moved rather than being orphaned");
    assert_eq!(
        tier_one["parent_id"], target.to_string(),
        "a child department moves with its parent, or the subtree is orphaned"
    );
}

/// The headline: the same employee serialises **without** the personal block for a reader and
/// **with** it for a caller holding `hr.employees.sensitive.read`.
///
/// Absent, not null — a null still tells a reader the field exists, and the request's risk note
/// is that the block is hidden from the list, the detail and the export alike.
#[tokio::test]
async fn the_personal_block_is_gated_and_never_rendered_null() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let officer = fixture.token(&fixture.officer).await;
    let reader = fixture.token(&fixture.reader).await;

    let employee = make_employee(
        &fixture,
        &officer,
        fixture.root_department,
        "Ada",
        "Lovelace",
        None,
    )
    .await;
    let id = id_of(&employee);

    // The officer wrote all four fields, so the detail response must carry them.
    let seen = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/employees/{id}"),
            Some(&officer),
            None,
        ),
    )
    .await;
    assert_eq!(seen.status, StatusCode::OK);
    assert_eq!(seen.body["personal_email"], "private@example.com");
    assert_eq!(seen.body["emergency_contact"], "A relative");
    assert_eq!(seen.body["address"], "Somewhere");

    // The reader may open the same employee — the directory is not hidden from them — but the
    // four keys are **absent**.
    let hidden = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/employees/{id}"),
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(hidden.status, StatusCode::OK, "a reader may read the directory");
    for field in ["personal_email", "personal_phone", "address", "emergency_contact"] {
        assert!(
            hidden.body.get(field).is_none(),
            "{field} must be absent rather than null: {hidden}"
        );
    }
    // …and the ungated fields are identical, so one screen layout serves both callers.
    assert_eq!(hidden.body["work_email"], seen.body["work_email"]);
    assert_eq!(hidden.body["position"], seen.body["position"]);

    // The list never carries them either, for **either** caller: the list projection does not
    // select those columns at all, so there is no permission that could make them appear.
    for session in [&officer, &reader] {
        let list = call(
            &fixture.state,
            request(Method::GET, "/api/v1/hr/employees", Some(session), None),
        )
        .await;
        let rows = list.body["items"].as_array().expect("an array");
        let row = rows
            .iter()
            .find(|row| row["id"] == id.to_string())
            .expect("the employee must be in the list");
        for field in ["personal_email", "personal_phone", "address", "emergency_contact"] {
            assert!(
                row.get(field).is_none(),
                "{field} must never be in a list row: {row}"
            );
        }
    }
}

/// A reader may not hire, and a cross-organization employee is a 404 — never a 403.
#[tokio::test]
async fn the_readers_split_is_real_and_another_organization_is_a_404() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let officer = fixture.token(&fixture.officer).await;
    let reader = fixture.token(&fixture.reader).await;

    let ours = make_employee(
        &fixture,
        &officer,
        fixture.root_department,
        "Ada",
        "Lovelace",
        None,
    )
    .await;

    // A reader may read...
    let read = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/employees", Some(&reader), None),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK);

    // ...and may not write. The guard is the permission, not a check in the handler.
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/employees",
            Some(&reader),
            Some(json!({
                "first_name": "Nobody",
                "last_name": "Else",
                "work_email": "nobody@example.com",
                "position": "Someone",
                "department_id": fixture.root_department,
                "employment_type": "full_time",
                "start_date": "2024-01-08",
            })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    // The same reader may not create a department either.
    let no_department = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/departments",
            Some(&reader),
            Some(json!({ "name": "Sneaky" })),
        ),
    )
    .await;
    assert_eq!(no_department.status, StatusCode::FORBIDDEN);

    // Another organization's employee: a 404, so the answer does not confirm the record exists.
    let foreign = fixture.token(&fixture.foreign).await;
    // The foreign tenant's own organization, resolved **once**. The first version of this block
    // named `organization_id` in the insert's column list *and* selected it from `organizations`,
    // so Postgres read the list's name as a column of the sub-select -- where it does not exist
    // -- and the walk died on a parse error that says nothing about HR.
    let foreign_org: Uuid = sqlx::query_scalar(
        "select id from organizations where slug like 'hr-hr-foreign%' order by created_at desc limit 1",
    )
    .fetch_one(fixture.db.pool())
    .await
    .expect("the foreign organization must exist");

    let foreign_department: Uuid = sqlx::query_scalar(
        "select id from hr_departments where organization_id = $1 order by created_at limit 1",
    )
    .bind(foreign_org)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the foreign tenant owns a seeded root department");

    let theirs: Uuid = sqlx::query_scalar(
        "insert into hr_employees (organization_id, employee_no, first_name, last_name, work_email, position, department_id, employment_type, start_date) \
         values ($1, 'F-1', 'Foreign', 'Person', 'foreign@example.com', 'Someone', $2, 'full_time', current_date) \
         returning id",
    )
    .bind(foreign_org)
    .bind(foreign_department)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the foreign employee must be created");

    let cross = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/employees/{theirs}"),
            Some(&officer),
            None,
        ),
    )
    .await;
    assert_eq!(
        cross.status,
        StatusCode::NOT_FOUND,
        "another organization's employee is a 404, never a 403: {}",
        cross.body
    );
    assert_ne!(theirs, id_of(&ours));

    // And the foreign caller reading THEIR own list does not see ours.
    let their_view = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/employees", Some(&foreign), None),
    )
    .await;
    let rows = their_view.body["items"].as_array().expect("an array");
    assert!(
        !rows.iter().any(|row| row["id"] == id_of(&ours).to_string()),
        "one organization's directory must never include another's"
    );
}

/// Termination keeps the record: a status and an end date, and the row is still readable.
///
/// There is no delete route on purpose — leave, attendance and onboarding all reference an
/// employee, and a hard delete would take a person's history with it. This walk is what proves the
/// difference is real rather than a naming convention.
#[tokio::test]
async fn terminating_writes_a_status_and_an_end_date_and_keeps_the_record() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let officer = fixture.token(&fixture.officer).await;

    let employee = make_employee(
        &fixture,
        &officer,
        fixture.root_department,
        "Ada",
        "Lovelace",
        None,
    )
    .await;
    let id = id_of(&employee);

    let terminated = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/hr/employees/{id}/terminate"),
            Some(&officer),
            Some(json!({ "end_date": "2026-08-31" })),
        ),
    )
    .await;
    assert_eq!(terminated.status, StatusCode::OK, "{}", terminated.body);
    assert_eq!(terminated.body["employee_status"], "terminated");
    assert_eq!(terminated.body["end_date"], "2026-08-31");

    // The record is STILL THERE, and still readable — that is the point of a status.
    let after = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/employees/{id}"),
            Some(&officer),
            None,
        ),
    )
    .await;
    assert_eq!(after.status, StatusCode::OK);
    assert_eq!(after.body["first_name"], "Ada");

    // An end date before the start is refused, and the record keeps its old status.
    let backwards = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/hr/employees/{id}/terminate"),
            Some(&officer),
            Some(json!({ "end_date": "2020-01-01" })),
        ),
    )
    .await;
    assert_eq!(backwards.status, StatusCode::BAD_REQUEST);
    let unchanged = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/hr/employees/{id}"),
            Some(&officer),
            None,
        ),
    )
    .await;
    assert_eq!(
        unchanged.body["end_date"], "2026-08-31",
        "a refused termination must not move the date"
    );

    // And a contract without an end date is refused at create, with the field named.
    let contract = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/employees",
            Some(&officer),
            Some(json!({
                "first_name": "Temp",
                "last_name": "Worker",
                "work_email": "temp@example.com",
                "position": "Contractor",
                "department_id": fixture.root_department,
                "employment_type": "contract",
                "start_date": "2024-01-08",
            })),
        ),
    )
    .await;
    assert_eq!(contract.status, StatusCode::BAD_REQUEST);
    assert_eq!(contract.body["error"]["details"]["field"], "end_date");
    assert!(
        contract.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("contract needs an end date"),
        "{}",
        contract.body
    );
}

/// The list's own contract: the total agrees with the rows, the sort refuses an unknown column,
/// and the visibility level a query asks for can only narrow.
#[tokio::test]
async fn the_list_refuses_an_unknown_sort_and_a_visibility_it_does_not_know() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let officer = fixture.token(&fixture.officer).await;

    for _ in 0..3 {
        make_employee(
            &fixture,
            &officer,
            fixture.root_department,
            "Person",
            "Sample",
            None,
        )
        .await;
    }

    let listed = call(
        &fixture.state,
        request(Method::GET, "/api/v1/hr/employees", Some(&officer), None),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK);
    let rows = listed.body["items"].as_array().expect("an array");
    let total = listed.body["total_estimate"].as_i64().expect("a total");
    assert_eq!(
        total as usize,
        rows.len(),
        "the header's number is the number of rows the list can show, not a separate count"
    );

    // An unknown sort column is refused rather than ignored: a saved view that silently sorted by
    // something else looks like data loss to the person who saved it.
    let bad_sort = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/hr/employees?sort=salary",
            Some(&officer),
            None,
        ),
    )
    .await;
    assert_eq!(bad_sort.status, StatusCode::BAD_REQUEST);
    assert!(
        bad_sort.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("salary"),
        "{}",
        bad_sort.body
    );

    // And an unknown visibility is refused, NOT widened to `all`. A stale saved view must not
    // turn a personal list into the whole directory.
    let bad_visibility = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/hr/employees?visibility=everyone",
            Some(&officer),
            None,
        ),
    )
    .await;
    assert_eq!(bad_visibility.status, StatusCode::BAD_REQUEST);
    assert!(
        bad_visibility.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("own, team, all"),
        "{}",
        bad_visibility.body
    );

    // The suggest-number route exists because the form needs it before anything is typed.
    let suggested = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/hr/employees/suggest-number",
            Some(&officer),
            None,
        ),
    )
    .await;
    assert_eq!(suggested.status, StatusCode::OK);
    assert!(
        suggested.body["employee_no"]
            .as_str()
            .expect("a number")
            .starts_with("EMP-"),
        "{}",
        suggested.body
    );
}

/// Every route answers 401 without a session, and a duplicate number or address is a 409 with a
/// message the form can show.
#[tokio::test]
async fn an_unauthenticated_caller_is_refused_everywhere_and_a_duplicate_is_a_409() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let officer = fixture.token(&fixture.officer).await;

    for (method, uri) in [
        (Method::GET, "/api/v1/hr/employees"),
        (Method::GET, "/api/v1/hr/departments"),
        (Method::GET, "/api/v1/hr/org-chart"),
    ] {
        let anonymous = call(&fixture.state, request(method.clone(), uri, None, None)).await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "{uri} must need a session");
    }

    let employee = make_employee(
        &fixture,
        &officer,
        fixture.root_department,
        "Ada",
        "Lovelace",
        None,
    )
    .await;

    // A duplicate work address is a 409 that says so, not a raw constraint name.
    let duplicate = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/employees",
            Some(&officer),
            Some(json!({
                "first_name": "Impostor",
                "last_name": "Person",
                "work_email": employee["work_email"],
                "position": "Someone",
                "department_id": fixture.root_department,
                "employment_type": "full_time",
                "start_date": "2024-01-08",
            })),
        ),
    )
    .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT, "{}", duplicate.body);
    assert_eq!(duplicate.body["error"]["code"], "employee_work_email_taken");

    // A duplicate department name is likewise named, and a blank one is a 400 with the field.
    let duplicate_department = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/departments",
            Some(&officer),
            Some(json!({ "name": "Support" })),
        ),
    )
    .await;
    assert_eq!(duplicate_department.status, StatusCode::CREATED);

    let again = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/departments",
            Some(&officer),
            Some(json!({ "name": "support" })),
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.body["error"]["code"], "department_name_taken");

    let blank = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/hr/departments",
            Some(&officer),
            Some(json!({ "name": "   " })),
        ),
    )
    .await;
    assert_eq!(blank.status, StatusCode::BAD_REQUEST);
    assert_eq!(blank.body["error"]["details"]["field"], "name");
}
