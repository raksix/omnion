//! Integration tests for the structure inside a tenant (docs/requests/REQ-005, slice 2):
//! departments, department membership and the roles a department carries.
//!
//! The walk proves, over the real router and the real resolver:
//!
//! * the tree is created, renamed, moved and archived, and a **cycle is refused** — a
//!   department can never become its own ancestor, neither directly nor through a chain;
//! * a role bound to a department shows up in `/iam/effective-permissions` for the people *in*
//!   it, and disappears when they leave;
//! * a binding on a parent department reaches its children, and stops reaching them once the
//!   parent is archived;
//! * an account that merely names a department it is not in gains nothing from it;
//! * a department of **another** organization is `404`, never `403`;
//! * deleting is refused while the department still holds people, roles or children.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::departments::{self, NewDepartment};
use omnion_identity::memberships;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::{roles as role_store, seed};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// What the administrator of this suite holds. `iam.roles.read` is in there because the
/// department binding is read back through the effective-permissions screen, and a screen the
/// fixture may not open would prove nothing.
const ADMIN_PERMISSIONS: [&str; 5] = [
    "organizations.read",
    "organizations.manage",
    "sites.read",
    "iam.roles.read",
    "iam.bindings.manage",
];

/// The permission a department role carries, used to prove the binding actually resolves.
/// It is a real catalogue key, and the suite's own role is the one bound to the department.
const PROBE_PERMISSION: &str = "content.pages.read";

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    body: Value,
}

/// Drive the real router without a network socket.
async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");

    let status = response.status();
    let set_cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("body must be JSON")
    };

    TestResponse {
        status,
        set_cookie,
        body,
    }
}

/// Build a request; `token` becomes the session cookie and `body` the JSON payload.
fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match token {
        Some(token) => builder.header(header::COOKIE, format!("omnion_session={token}")),
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

/// Object store of the test state.
fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

/// Connect to the compose PostgreSQL; `None` means the stack is not running.
async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&config.database).await {
        Ok(db) => Some(db),
        Err(err) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({err}) — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
            );
            None
        }
    }
}

/// A state whose database has all migrations applied and the IAM seed loaded.
async fn live_state() -> Option<(AppState, Db)> {
    let config = Config::from_env().expect("environment must be valid");
    let db = live_db(&config).await?;
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

/// Two organizations, each with an administrator, a plain member and a role to bind.
struct Fixture {
    state: AppState,
    db: Db,
    org_a: Uuid,
    org_b: Uuid,
    admin_email: String,
    /// A member of organization A with no tenancy permissions — the account the department
    /// binding has to reach.
    member_email: String,
    member_id: Uuid,
    /// A second member of A, the one that must *not* pick the role up.
    outsider_email: String,
    outsider_id: Uuid,
    /// The role bound to a department, carrying `PROBE_PERMISSION`.
    role_id: Uuid,
    /// The second organization's administrator — the cross-tenant reader.
    other_admin_email: String,
    accounts: Vec<Uuid>,
    organizations: Vec<Uuid>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool()).await.expect("the IAM seed must run");

        let org_a = create_organization_row(&db, "a", "Department Test A").await;
        let org_b = create_organization_row(&db, "b", "Department Test B").await;

        let (admin_id, admin_email) = create_account(&db).await;
        add_membership(&db, org_a, admin_id).await;
        grant_organization_admin(&db, org_a, admin_id).await;

        let (member_id, member_email) = create_account(&db).await;
        add_membership(&db, org_a, member_id).await;

        let (outsider_id, outsider_email) = create_account(&db).await;
        add_membership(&db, org_a, outsider_id).await;

        // A second organization's administrator, for the cross-tenant assertions.
        let (b_admin_id, other_admin_email) = create_account(&db).await;
        add_membership(&db, org_b, b_admin_id).await;
        grant_organization_admin(&db, org_b, b_admin_id).await;

        let role_id = create_probe_role(&db, org_a).await;

        Some(Self {
            state,
            db,
            org_a,
            org_b,
            admin_email,
            member_email,
            member_id,
            outsider_email,
            outsider_id,
            role_id,
            other_admin_email,
            accounts: vec![admin_id, member_id, outsider_id, b_admin_id],
            organizations: vec![org_a, org_b],
        })
    }

    async fn admin_token(&self) -> String {
        login(&self.state, &self.admin_email).await
    }

    async fn member_token(&self) -> String {
        login(&self.state, &self.member_email).await
    }

    async fn outsider_token(&self) -> String {
        login(&self.state, &self.outsider_email).await
    }

    /// The second organization's administrator, for the cross-tenant assertions.
    async fn other_admin_token(&self) -> String {
        login(&self.state, &self.other_admin_email).await
    }

    /// Remove exactly what this fixture created — by id, never by a pattern.
    async fn cleanup(&self) {
        sqlx::query("delete from users where id = any($1)")
            .bind(&self.accounts)
            .execute(self.db.pool())
            .await
            .expect("account cleanup must run");
        sqlx::query("delete from organizations where id = any($1)")
            .bind(&self.organizations)
            .execute(self.db.pool())
            .await
            .expect("organization cleanup must run");
    }
}

/// Create an organization row with a unique, suite-scoped slug.
async fn create_organization_row(db: &Db, label: &str, name: &str) -> Uuid {
    let slug = format!("departments-fix-{label}-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(name)
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// Create an account with a unique address and no membership.
async fn create_account(db: &Db) -> (Uuid, String) {
    let email = format!("departments-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Department Test".to_owned(),
            organization_id: None,
        },
    )
    .await
    .expect("creating a test account must succeed");
    (user.id, email)
}

async fn add_membership(db: &Db, organization_id: Uuid, user_id: Uuid) {
    memberships::add_member(
        db.pool(),
        memberships::NewMembership {
            organization_id,
            user_id,
            status: "active".to_owned(),
            is_primary: true,
        },
    )
    .await
    .expect("the membership must be created");
}

/// Give one account the tenancy permissions of an organization.
async fn grant_organization_admin(db: &Db, organization_id: Uuid, user_id: Uuid) {
    let role = role_store::create_role(
        db.pool(),
        omnion_permissions::model::NewRole {
            organization_id,
            key: format!("org-admin-{}", Uuid::new_v4().simple()),
            name: "Organization Administrator".to_owned(),
            description: "Runs one organization".to_owned(),
            priority: 800,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the organization role must be created");

    let entries: Vec<omnion_permissions::model::RolePermissionInput> = ADMIN_PERMISSIONS
        .iter()
        .map(|key| omnion_permissions::model::RolePermissionInput {
            key: (*key).to_owned(),
            effect: omnion_permissions::model::Effect::Allow,
        })
        .collect();
    role_store::set_role_permissions(db.pool(), role.id, &entries)
        .await
        .expect("the role permission set must be written");

    omnion_permissions::bindings::grant(
        db.pool(),
        omnion_permissions::model::NewBinding {
            role_id: role.id,
            user_id,
            scope: omnion_permissions::Scope::Organization { organization_id },
            granted_by: None,
            expires_at: None,
        },
    )
    .await
    .expect("the binding must be granted");
}

/// A role carrying exactly `PROBE_PERMISSION`, to be bound to a department.
async fn create_probe_role(db: &Db, organization_id: Uuid) -> Uuid {
    let role = role_store::create_role(
        db.pool(),
        omnion_permissions::model::NewRole {
            organization_id,
            key: format!("dept-probe-{}", Uuid::new_v4().simple()),
            name: "Department Reader".to_owned(),
            description: "Reads pages because of the department it is in".to_owned(),
            priority: 100,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the probe role must be created");

    role_store::set_role_permissions(
        db.pool(),
        role.id,
        &[omnion_permissions::model::RolePermissionInput {
            key: PROBE_PERMISSION.to_owned(),
            effect: omnion_permissions::model::Effect::Allow,
        }],
    )
    .await
    .expect("the probe role permission must be written");

    role.id
}

/// Sign an account in and return the raw session token.
async fn login(state: &AppState, email: &str) -> String {
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
    response
        .set_cookie
        .clone()
        .expect("login must set the session cookie")
        .split(';')
        .next()
        .expect("cookie has a value")
        .split_once('=')
        .expect("cookie is name=value")
        .1
        .to_owned()
}

/// The `id` field of a response body, as text.
fn id_of(body: &Value) -> String {
    body["id"]
        .as_str()
        .unwrap_or_else(|| panic!("body carries an id: {body}"))
        .to_owned()
}

/// The `code` of an error body.
fn code_of(body: &Value) -> String {
    body["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("body carries an error code: {body}"))
        .to_owned()
}

/// The granted permission keys of an effective-permissions body.
fn granted_keys(body: &Value) -> Vec<String> {
    body["granted"]
        .as_array()
        .expect("granted is an array")
        .iter()
        .map(|entry| entry["key"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[tokio::test]
async fn a_department_role_reaches_its_members_and_only_its_members() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;

    // A department with one person in it.
    let department = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/organizations/{}/departments", fixture.org_a),
            Some(&admin),
            Some(json!({ "key": "engineering", "name": "Engineering" })),
        ),
    )
    .await;
    assert_eq!(
        department.status,
        StatusCode::CREATED,
        "creating a department: {}",
        department.body
    );
    let department_id = id_of(&department.body);
    assert_eq!(department.body["key"], "engineering");

    // The member goes in; the outsider stays out.
    let added = call(
        &fixture.state,
        request(
            Method::POST,
            &format!(
                "/api/v1/organizations/{}/departments/{}/members",
                fixture.org_a, department_id
            ),
            Some(&admin),
            Some(json!({ "user_id": fixture.member_id })),
        ),
    )
    .await;
    assert_eq!(added.status, StatusCode::CREATED, "adding a member: {}", added.body);

    // Bind the probe role to the department as a whole.
    let bound = call(
        &fixture.state,
        request(
            Method::POST,
            &format!(
                "/api/v1/organizations/{}/departments/{}/roles",
                fixture.org_a, department_id
            ),
            Some(&admin),
            Some(json!({ "role_id": fixture.role_id })),
        ),
    )
    .await;
    assert_eq!(bound.status, StatusCode::CREATED, "binding a role: {}", bound.body);

    // The member, resolving inside the department, holds the role.
    let member = fixture.member_token().await;
    let member_view = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/iam/effective-permissions?organization_id={}&department=engineering",
                fixture.org_a
            ),
            Some(&member),
            None,
        ),
    )
    .await;
    assert_eq!(
        member_view.status,
        StatusCode::OK,
        "the member's effective set: {}",
        member_view.body
    );
    assert!(
        granted_keys(&member_view.body).contains(&PROBE_PERMISSION.to_owned()),
        "a member of the department must hold the role bound to it, got {:?}",
        member_view.body
    );

    // The outsider, resolving in the same department, does not.
    let outsider = fixture.outsider_token().await;
    let outsider_view = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/iam/effective-permissions?organization_id={}&department=engineering",
                fixture.org_a
            ),
            Some(&outsider),
            None,
        ),
    )
    .await;
    assert!(
        !granted_keys(&outsider_view.body).contains(&PROBE_PERMISSION.to_owned()),
        "an account outside the department must not pick the role up, got {:?}",
        outsider_view.body
    );

    // Once the member leaves, the role stops resolving for them.
    let removed = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!(
                "/api/v1/organizations/{}/departments/{}/members/{}",
                fixture.org_a, department_id, fixture.member_id
            ),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT, "removing: {}", removed.body);

    let after = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/iam/effective-permissions?organization_id={}&department=engineering",
                fixture.org_a
            ),
            Some(&member),
            None,
        ),
    )
    .await;
    assert!(
        !granted_keys(&after.body).contains(&PROBE_PERMISSION.to_owned()),
        "leaving the department must take the role with it, got {:?}",
        after.body
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_department_can_never_become_its_own_ancestor() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;

    // A chain: it → team → squad.
    let it = create_department(&fixture, &admin, "it", "IT", None).await;
    let team = create_department(&fixture, &admin, "team", "Team", Some(&it)).await;
    let squad = create_department(&fixture, &admin, "squad", "Squad", Some(&team)).await;

    // Directly onto itself.
    let self_move = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!(
                "/api/v1/organizations/{}/departments/{}",
                fixture.org_a, it
            ),
            Some(&admin),
            Some(json!({ "parent_id": it })),
        ),
    )
    .await;
    assert_eq!(
        self_move.status,
        StatusCode::CONFLICT,
        "a department cannot become its own parent: {}",
        self_move.body
    );
    assert_eq!(code_of(&self_move.body), "department_cycle");

    // Into its own descendant — the case the schema check alone cannot see.
    let cycle = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!(
                "/api/v1/organizations/{}/departments/{}",
                fixture.org_a, it
            ),
            Some(&admin),
            Some(json!({ "parent_id": squad })),
        ),
    )
    .await;
    assert_eq!(
        cycle.status,
        StatusCode::CONFLICT,
        "a department cannot move under its own descendant: {}",
        cycle.body
    );
    assert_eq!(code_of(&cycle.body), "department_cycle");

    // A legal move still works, so the refusal is the rule and not a broken screen.
    let legal = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!(
                "/api/v1/organizations/{}/departments/{}",
                fixture.org_a, squad
            ),
            Some(&admin),
            Some(json!({ "parent_id": null })),
        ),
    )
    .await;
    assert_eq!(legal.status, StatusCode::OK, "promoting to a root: {}", legal.body);
    assert!(legal.body["parent_id"].is_null());

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_binding_on_a_parent_reaches_its_children_until_it_is_archived() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;

    let parent = create_department(&fixture, &admin, "division", "Division", None).await;
    let child = create_department(&fixture, &admin, "squad", "Squad", Some(&parent)).await;

    // The member is only in the child.
    call(
        &fixture.state,
        request(
            Method::POST,
            &format!(
                "/api/v1/organizations/{}/departments/{}/members",
                fixture.org_a, child
            ),
            Some(&admin),
            Some(json!({ "user_id": fixture.member_id })),
        ),
    )
    .await;

    call(
        &fixture.state,
        request(
            Method::POST,
            &format!(
                "/api/v1/organizations/{}/departments/{}/roles",
                fixture.org_a, parent
            ),
            Some(&admin),
            Some(json!({ "role_id": fixture.role_id })),
        ),
    )
    .await;

    // Resolving *at the child* picks up the parent's binding through the tree.
    let member = fixture.member_token().await;
    let in_child = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/iam/effective-permissions?organization_id={}&department=squad",
                fixture.org_a
            ),
            Some(&member),
            None,
        ),
    )
    .await;
    assert!(
        granted_keys(&in_child.body).contains(&PROBE_PERMISSION.to_owned()),
        "a binding on the parent must reach the child, got {:?}",
        in_child.body
    );

    // Archiving the parent stops it granting, without anybody revoking the binding.
    let archived = call(
        &fixture.state,
        request(
            Method::POST,
            &format!(
                "/api/v1/organizations/{}/departments/{}",
                fixture.org_a, parent
            ),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(archived.status, StatusCode::OK, "archiving: {}", archived.body);
    assert_eq!(archived.body["status"], "archived");

    let after_archive = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/iam/effective-permissions?organization_id={}&department=squad",
                fixture.org_a
            ),
            Some(&member),
            None,
        ),
    )
    .await;
    assert!(
        !granted_keys(&after_archive.body).contains(&PROBE_PERMISSION.to_owned()),
        "an archived department must stop granting, got {:?}",
        after_archive.body
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn another_organizations_department_is_a_404_and_deletes_refuse_while_occupied() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;

    // A department in the *other* organization, owned by its own administrator.
    let other = departments::create_department(
        fixture.db.pool(),
        NewDepartment {
            organization_id: fixture.org_b,
            parent_id: None,
            key: "other-dept".to_owned(),
            name: "Other Department".to_owned(),
            description: String::new(),
        },
    )
    .await
    .expect("the second organization's department must be created");

    // Organization A's administrator asks for it: 404, never 403.
    let cross = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/organizations/{}/departments/{}",
                fixture.org_a, other.id
            ),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(
        cross.status,
        StatusCode::NOT_FOUND,
        "another tenant's department must be 404: {}",
        cross.body
    );

    // Delete is refused while the department still holds a role binding.
    let occupied = create_department(&fixture, &admin, "occupied", "Occupied", None).await;
    call(
        &fixture.state,
        request(
            Method::POST,
            &format!(
                "/api/v1/organizations/{}/departments/{}/roles",
                fixture.org_a, occupied
            ),
            Some(&admin),
            Some(json!({ "role_id": fixture.role_id })),
        ),
    )
    .await;
    let refused = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!(
                "/api/v1/organizations/{}/departments/{}",
                fixture.org_a, occupied
            ),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "deleting an occupied department must be refused: {}",
        refused.body
    );

    // An empty leaf deletes cleanly.
    let empty = create_department(&fixture, &admin, "empty", "Empty", None).await;
    let deleted = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!(
                "/api/v1/organizations/{}/departments/{}",
                fixture.org_a, empty
            ),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "deleting: {}", deleted.body);

    // A parent with a child is refused too.
    let parent = create_department(&fixture, &admin, "parent", "Parent", None).await;
    create_department(&fixture, &admin, "child", "Child", Some(&parent)).await;
    let refused_parent = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!(
                "/api/v1/organizations/{}/departments/{}",
                fixture.org_a, parent
            ),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(
        refused_parent.status,
        StatusCode::BAD_REQUEST,
        "deleting a parent with children must be refused: {}",
        refused_parent.body
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_tree_reads_in_order_and_a_taken_key_is_refused() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;

    // Two roots, so the ordering assertion has something to prove.
    let zeta = create_department(&fixture, &admin, "zeta", "Zeta", None).await;
    let alpha = create_department(&fixture, &admin, "alpha", "Alpha", None).await;
    let child = create_department(&fixture, &admin, "beta", "Beta", Some(&zeta)).await;

    let tree = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/organizations/{}/departments", fixture.org_a),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(tree.status, StatusCode::OK, "reading the tree: {}", tree.body);

    let rows = tree.body["departments"]
        .as_array()
        .expect("departments is an array");
    let order: Vec<String> = rows
        .iter()
        .map(|row| row["key"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        order,
        vec!["alpha", "zeta", "beta"],
        "roots come first, each before its own children"
    );

    // Depth is computed over the tree, so the child is indented rather than listed flat.
    let beta = rows
        .iter()
        .find(|row| row["key"] == "beta")
        .expect("the child is listed");
    assert_eq!(beta["depth"], 1, "a child sits at depth 1");
    assert_eq!(beta["parent_key"], "zeta");

    // A taken key is a conflict naming itself.
    let duplicate = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/organizations/{}/departments", fixture.org_a),
            Some(&admin),
            Some(json!({ "key": "alpha", "name": "Another Alpha" })),
        ),
    )
    .await;
    assert_eq!(
        duplicate.status,
        StatusCode::CONFLICT,
        "a taken key must be refused: {}",
        duplicate.body
    );
    assert_eq!(code_of(&duplicate.body), "department_key_taken");

    // An unaddressable key is refused before it reaches the database.
    let bad_key = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/organizations/{}/departments", fixture.org_a),
            Some(&admin),
            Some(json!({ "key": "Not A Key", "name": "Broken" })),
        ),
    )
    .await;
    assert_eq!(
        bad_key.status,
        StatusCode::BAD_REQUEST,
        "a key that could never be addressed must be refused: {}",
        bad_key.body
    );

    let _ = (alpha, child);
    fixture.cleanup().await;
}

/// Create a department through the API and return its id.
async fn create_department(
    fixture: &Fixture,
    token: &str,
    key: &str,
    name: &str,
    parent_id: Option<&str>,
) -> String {
    let mut payload = json!({ "key": key, "name": name });
    if let Some(parent) = parent_id {
        payload["parent_id"] = json!(parent);
    }
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/organizations/{}/departments", fixture.org_a),
            Some(token),
            Some(payload),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "creating department {key}: {}",
        response.body
    );
    id_of(&response.body)
}

// The `department` query parameter of the effective-permissions screen is what the resolver
// needs; assert it is read rather than silently ignored, so a rename of that parameter cannot
// quietly break every department binding.
#[test]
fn the_effective_permissions_screen_takes_a_department() {
    let query = omnion_permissions::model::ResourceContext {
        organization_id: Some(Uuid::nil()),
        site_id: None,
        department: Some("engineering".to_owned()),
        module: None,
        path: None,
    };
    let scope = omnion_permissions::Scope::Department {
        organization_id: Uuid::nil(),
        department: "engineering".to_owned(),
    };
    assert!(scope.applies_to(&query));
    assert!(
        !omnion_permissions::Scope::Department {
            organization_id: Uuid::nil(),
            department: "sales".to_owned(),
        }
        .applies_to(&query),
        "another department's binding must not apply"
    );
    // A department binding never applies to a request that names no department.
    let bare = omnion_permissions::model::ResourceContext {
        organization_id: Some(Uuid::nil()),
        site_id: None,
        department: None,
        module: None,
        path: None,
    };
    assert!(!scope.applies_to(&bare));
}

// Adding department-scoped bindings to the resolver must not weaken the scope filter that was
// already there. A role bound to one site used to be dropped when the request named another
// site; once the department load was merged in, that filter could silently disappear and the
// role would answer everywhere. This walks the same resolution over HTTP, because the unit
// test above only proves `applies_to` itself and not that the resolver still calls it.
#[tokio::test]
async fn a_role_bound_to_one_site_never_answers_for_another() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };

    // The member gets the probe role bound at *site* scope — not organization, not department.
    // `subject_type`/`subject_id` are filled by the 0016 trigger from `user_id`, so the insert
    // only names the account.
    let site_id = Uuid::new_v4();
    sqlx::query(
        "insert into sites (id, organization_id, key, name, status, created_at, updated_at) \
         values ($1, $2, 'scoped', 'Scoped', 'active', now(), now())",
    )
    .bind(site_id)
    .bind(fixture.org_a)
    .execute(fixture.db.pool())
    .await
    .expect("the site row must insert");

    let role_id = fixture.role_id;
    let org_a = fixture.org_a;
    let member_id = fixture.member_id;
    sqlx::query(
        "insert into role_bindings \
         (id, role_id, user_id, scope_type, organization_id, site_id, created_at) \
         values (gen_random_uuid(), $1, $2, 'site', $3, $4, now())",
    )
    .bind(role_id)
    .bind(member_id)
    .bind(org_a)
    .bind(site_id)
    .execute(fixture.db.pool())
    .await
    .expect("the site-scoped binding must insert");

    let member = fixture.member_token().await;

    // In the site the role was bound to, it resolves.
    let inside = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/iam/effective-permissions?organization_id={org_a}&site_id={site_id}"),
            Some(&member),
            None,
        ),
    )
    .await;
    assert_eq!(inside.status, StatusCode::OK, "inside the site: {}", inside.body);
    assert!(
        granted_keys(&inside.body).contains(&PROBE_PERMISSION.to_owned()),
        "a role bound to this site must apply inside it, got {:?}",
        inside.body
    );

    // In a different site of the same organization, it must not. Same organization, same role,
    // different site — the only thing that can stop it is the scope filter.
    let other_site_id = Uuid::new_v4();
    sqlx::query(
        "insert into sites (id, organization_id, key, name, status, created_at, updated_at) \
         values ($1, $2, 'elsewhere', 'Elsewhere', 'active', now(), now())",
    )
    .bind(other_site_id)
    .bind(org_a)
    .execute(fixture.db.pool())
    .await
    .expect("the second site row must insert");

    let elsewhere = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/iam/effective-permissions?organization_id={org_a}&site_id={other_site_id}"
            ),
            Some(&member),
            None,
        ),
    )
    .await;
    assert_eq!(
        elsewhere.status,
        StatusCode::OK,
        "in the other site: {}",
        elsewhere.body
    );
    assert!(
        !granted_keys(&elsewhere.body).contains(&PROBE_PERMISSION.to_owned()),
        "a role bound to one site must not answer for another, got {:?}",
        elsewhere.body
    );

    // And a request naming no site at all must not pick it up either — a site-scoped grant is
    // not an organization-wide one.
    let bare = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/iam/effective-permissions?organization_id={org_a}"),
            Some(&member),
            None,
        ),
    )
    .await;
    assert!(
        !granted_keys(&bare.body).contains(&PROBE_PERMISSION.to_owned()),
        "a site-scoped role must not apply to a request that names no site, got {:?}",
        bare.body
    );

    fixture.cleanup().await;
}

