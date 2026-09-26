//! Integration tests for the IAM surface: permission guards, custom roles, role assignments,
//! effective permissions and the audit trail (docs/07-IAM.md, phase P03).
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) — locally and in CI. When
//! PostgreSQL is not reachable the suite skips itself with a printed reason, so `cargo test`
//! stays usable on a machine without Docker.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{NewBinding, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

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
///
/// These suites never touch the object store — that is the media suite's job — so the default
/// development configuration is enough: it opens without contacting anything.
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

/// A throwaway organization with an Owner account.
///
/// The Owner holds a platform-level owner binding, so the fixture can drive the whole IAM
/// surface over HTTP the way an installation would.
struct Fixture {
    state: AppState,
    db: Db,
    organization_id: Uuid,
    owner_id: Uuid,
    owner_email: String,
    accounts: Vec<Uuid>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let slug = format!("iam-{}", Uuid::new_v4().simple());
        let organization_id: Uuid = sqlx::query_scalar(
            "insert into organizations (name, slug) values ($1, $2) returning id",
        )
        .bind("IAM Test Organization")
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created");

        let (owner_id, owner_email) = create_account(&db, Some(organization_id)).await;
        seed::bind_owner(db.pool(), owner_id)
            .await
            .expect("the owner binding must be created");

        Some(Self {
            state,
            db,
            organization_id,
            owner_id,
            owner_email,
            accounts: vec![owner_id],
        })
    }

    /// Create one more account and remember it for cleanup.
    async fn add_account(&mut self, organization_id: Option<Uuid>) -> (Uuid, String) {
        let (id, email) = create_account(&self.db, organization_id).await;
        self.accounts.push(id);
        (id, email)
    }

    /// The Owner of the fixture, signed in.
    async fn owner_token(&self) -> String {
        login(&self.state, &self.owner_email).await
    }

    /// Look a platform role up by key.
    async fn system_role(&self, key: &str) -> Uuid {
        role_store::find_role_by_key(self.db.pool(), None, key)
            .await
            .expect("role lookup must run")
            .unwrap_or_else(|| panic!("the {key} role must exist after the seed"))
            .id
    }

    /// Remove what this fixture created: bindings, accounts and the organization.
    async fn cleanup(&self) {
        sqlx::query("delete from role_bindings where user_id = any($1)")
            .bind(&self.accounts)
            .execute(self.db.pool())
            .await
            .expect("binding cleanup must run");
        sqlx::query("delete from users where id = any($1)")
            .bind(&self.accounts)
            .execute(self.db.pool())
            .await
            .expect("account cleanup must run");
        sqlx::query("delete from organizations where id = $1")
            .bind(self.organization_id)
            .execute(self.db.pool())
            .await
            .expect("organization cleanup must run");
    }
}

/// Create an account with a unique address so parallel runs cannot collide.
async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("iam-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "IAM Test".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("creating a test account must succeed");
    (user.id, email)
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
    let cookie = response
        .set_cookie
        .clone()
        .expect("login must set the session cookie");
    cookie
        .split(';')
        .next()
        .expect("cookie has a value")
        .split_once('=')
        .expect("cookie is name=value")
        .1
        .to_owned()
}

/// Collect the `key` field of every object in a response array.
fn keys_of(body: &Value, pointer: &str, field: &str) -> Vec<String> {
    body[pointer]
        .as_array()
        .unwrap_or_else(|| panic!("{pointer} must be an array in {body}"))
        .iter()
        .filter_map(|entry| entry[field].as_str().map(str::to_owned))
        .collect()
}

/// The `id` of the role whose `key` is `key`.
fn role_id_of(body: &Value, key: &str) -> String {
    body["roles"]
        .as_array()
        .expect("roles must be an array")
        .iter()
        .find(|role| role["key"] == key)
        .unwrap_or_else(|| panic!("role {key} must be visible in {body}"))
        .get("id")
        .and_then(Value::as_str)
        .expect("a role carries an id")
        .to_owned()
}

/// One entry of a `granted`/`denied` list, by permission key.
fn entry_for<'a>(body: &'a Value, pointer: &str, key: &str) -> Option<&'a Value> {
    body[pointer]
        .as_array()
        .unwrap_or_else(|| panic!("{pointer} must be an array in {body}"))
        .iter()
        .find(|entry| entry["key"] == key)
}

#[tokio::test]
async fn the_iam_surface_is_permission_gated_end_to_end() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.owner_token().await;

    // The catalogue answers for an account that holds the read permission.
    let catalogue = call(
        &fixture.state,
        request(Method::GET, "/api/v1/iam/permissions", Some(&owner), None),
    )
    .await;
    assert_eq!(
        catalogue.status,
        StatusCode::OK,
        "catalogue body: {}",
        catalogue.body
    );
    let catalogue_keys = keys_of(&catalogue.body, "permissions", "key");
    assert!(catalogue_keys.contains(&"content.pages.read".to_owned()));
    assert!(catalogue_keys.contains(&"iam.roles.manage".to_owned()));
    assert!(catalogue_keys.len() >= 25, "catalogue: {catalogue_keys:?}");

    // The Owner sees the platform roles of docs/07-IAM.md §3.
    let roles_response = call(
        &fixture.state,
        request(Method::GET, "/api/v1/iam/roles", Some(&owner), None),
    )
    .await;
    assert_eq!(
        roles_response.status,
        StatusCode::OK,
        "roles body: {}",
        roles_response.body
    );
    let role_keys = keys_of(&roles_response.body, "roles", "key");
    for expected in [
        "owner",
        "administrator",
        "manager",
        "moderator",
        "editor",
        "member",
    ] {
        assert!(
            role_keys.contains(&expected.to_owned()),
            "missing {expected}"
        );
    }
    let member_role_id = role_id_of(&roles_response.body, "member");
    let editor_role_id = role_id_of(&roles_response.body, "editor");

    // A custom role that inherits the editor role (docs/07-IAM.md §4).
    let reviewer_key = format!("reviewer-{}", Uuid::new_v4().simple());
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/roles",
            Some(&owner),
            Some(json!({
                "key": reviewer_key,
                "name": "Content Reviewer",
                "description": "Reviews content before publication",
                "priority": 350,
                "organization_id": fixture.organization_id,
                "inherits_role_id": editor_role_id,
            })),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "create body: {}",
        created.body
    );
    let reviewer_role_id = created.body["id"]
        .as_str()
        .expect("the created role must carry an id")
        .to_owned();
    assert_eq!(created.body["priority"], 350);
    assert_eq!(created.body["is_system"], false);

    // Explicit deny on top of the inherited allow (docs/07-IAM.md §5).
    let updated = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/iam/roles/{reviewer_role_id}/permissions"),
            Some(&owner),
            Some(json!({
                "permissions": [
                    { "key": "content.pages.delete", "effect": "deny" },
                    { "key": "media.read", "effect": "allow" }
                ]
            })),
        ),
    )
    .await;
    assert_eq!(
        updated.status,
        StatusCode::OK,
        "update body: {}",
        updated.body
    );
    assert_eq!(updated.body["denied_permissions"], 1);
    assert_eq!(updated.body["allowed_permissions"], 1);

    // A fresh account holds nothing: the guard refuses the same routes it just answered.
    let (member_id, member_email) = fixture.add_account(Some(fixture.organization_id)).await;
    let member = login(&fixture.state, &member_email).await;

    let denied = call(
        &fixture.state,
        request(Method::GET, "/api/v1/iam/roles", Some(&member), None),
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(denied.body["error"]["code"], "permission_denied");
    assert!(
        denied.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("iam.roles.read"),
        "the refusal names the missing permission: {}",
        denied.body
    );

    let denied_audit = call(
        &fixture.state,
        request(Method::GET, "/api/v1/iam/audit", Some(&member), None),
    )
    .await;
    assert_eq!(denied_audit.status, StatusCode::FORBIDDEN);

    // Assigning the Member role unlocks exactly that role's permissions.
    let granted = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/bindings",
            Some(&owner),
            Some(json!({
                "user_id": member_id,
                "role_id": member_role_id,
                "scope_type": "organization",
                "organization_id": fixture.organization_id,
            })),
        ),
    )
    .await;
    assert_eq!(
        granted.status,
        StatusCode::CREATED,
        "binding body: {}",
        granted.body
    );
    assert_eq!(granted.body["active"], true);
    assert_eq!(granted.body["scope"]["type"], "organization");

    // The account may resolve its own set without a permission …
    let own = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/iam/effective-permissions",
            Some(&member),
            None,
        ),
    )
    .await;
    assert_eq!(own.status, StatusCode::OK, "own set: {}", own.body);
    let own_keys = keys_of(&own.body, "granted", "key");
    assert!(own_keys.contains(&"content.pages.read".to_owned()));
    assert!(
        !own_keys.contains(&"iam.roles.read".to_owned()),
        "the Member role carries no IAM permissions: {own_keys:?}"
    );

    // … and still cannot reach the IAM surface.
    let still_denied = call(
        &fixture.state,
        request(Method::GET, "/api/v1/iam/roles", Some(&member), None),
    )
    .await;
    assert_eq!(still_denied.status, StatusCode::FORBIDDEN);

    // Assigning the reviewer role adds the inherited allows and the explicit deny.
    let reviewer_binding = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/bindings",
            Some(&owner),
            Some(json!({
                "user_id": member_id,
                "role_id": reviewer_role_id,
                "scope_type": "organization",
                "organization_id": fixture.organization_id,
            })),
        ),
    )
    .await;
    assert_eq!(
        reviewer_binding.status,
        StatusCode::CREATED,
        "reviewer binding: {}",
        reviewer_binding.body
    );

    let effective = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/iam/effective-permissions?user_id={member_id}"),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(
        effective.status,
        StatusCode::OK,
        "effective body: {}",
        effective.body
    );

    let denied_entry = entry_for(&effective.body, "denied", "content.pages.delete")
        .expect("the explicit deny must be reported");
    assert_eq!(denied_entry["source"]["via"], "explicit_deny");
    assert_eq!(denied_entry["source"]["role_key"], reviewer_key);

    let inherited = entry_for(&effective.body, "granted", "content.pages.update")
        .expect("the editor allow must be inherited");
    assert_eq!(inherited["source"]["via"], "inherited_allow");
    assert_eq!(inherited["source"]["role_key"], "editor");

    let direct = entry_for(&effective.body, "granted", "content.pages.read")
        .expect("the member allow is direct");
    assert_eq!(direct["source"]["via"], "explicit_allow");

    // The bindings endpoint lists the assignments of the account.
    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/iam/bindings?user_id={member_id}"),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "bindings: {}", listed.body);
    assert_eq!(
        listed.body["bindings"]
            .as_array()
            .expect("bindings array")
            .len(),
        2
    );

    // Every privileged action above left an audit row (docs/07-IAM.md §13).
    let audit = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/iam/audit?limit=200",
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(audit.status, StatusCode::OK, "audit body: {}", audit.body);
    let actions = keys_of(&audit.body, "entries", "action");
    for expected in [
        "iam.role.created",
        "iam.role.permissions_updated",
        "iam.binding.granted",
    ] {
        assert!(
            actions.contains(&expected.to_owned()),
            "audit must record {expected}, got {actions:?}"
        );
    }

    let created_entry = audit.body["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|entry| {
            entry["action"] == "iam.role.created" && entry["target_id"] == reviewer_role_id
        })
        .expect("the role creation row must be found");
    assert_eq!(
        created_entry["actor_user_id"].as_str(),
        Some(fixture.owner_id.to_string().as_str())
    );
    assert_eq!(created_entry["actor_type"], "user");
    assert_eq!(created_entry["metadata"]["key"], reviewer_key);

    fixture.cleanup().await;
}

#[tokio::test]
async fn an_explicit_deny_removes_an_inherited_allow_through_the_api() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.owner_token().await;
    let editor_role_id = fixture.system_role("editor").await.to_string();

    // A reviewer role that keeps every editor permission except deletion.
    let reviewer_key = format!("reviewer-{}", Uuid::new_v4().simple());
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/roles",
            Some(&owner),
            Some(json!({
                "key": reviewer_key,
                "name": "Content Reviewer",
                "organization_id": fixture.organization_id,
                "inherits_role_id": editor_role_id,
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let reviewer_role_id = created.body["id"].as_str().expect("id").to_owned();

    let updated = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/iam/roles/{reviewer_role_id}/permissions"),
            Some(&owner),
            Some(json!({ "permissions": [{ "key": "content.pages.delete", "effect": "deny" }] })),
        ),
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK, "{}", updated.body);

    let (account_id, email) = fixture.add_account(Some(fixture.organization_id)).await;
    let account_token = login(&fixture.state, &email).await;

    let bound = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/bindings",
            Some(&owner),
            Some(json!({
                "user_id": account_id,
                "role_id": reviewer_role_id,
                "scope_type": "organization",
                "organization_id": fixture.organization_id,
            })),
        ),
    )
    .await;
    assert_eq!(bound.status, StatusCode::CREATED, "{}", bound.body);

    // The account itself sees the same verdict as the administrator looking at it.
    for token in [account_token.as_str(), owner.as_str()] {
        let uri = format!("/api/v1/iam/effective-permissions?user_id={account_id}");
        let response = call(
            &fixture.state,
            request(Method::GET, &uri, Some(token), None),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);

        let granted = keys_of(&response.body, "granted", "key");
        assert!(
            granted.contains(&"content.pages.update".to_owned()),
            "the inherited allow stays: {granted:?}"
        );
        assert!(
            !granted.contains(&"content.pages.delete".to_owned()),
            "the denied permission is not granted: {granted:?}"
        );

        let denied = entry_for(&response.body, "denied", "content.pages.delete")
            .expect("the deny is reported");
        assert_eq!(denied["source"]["via"], "explicit_deny");
    }

    fixture.cleanup().await;
}

#[tokio::test]
async fn temporary_bindings_expire_and_can_be_assigned_again() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.owner_token().await;
    let editor_role_id = fixture.system_role("editor").await;
    let (account_id, email) = fixture.add_account(Some(fixture.organization_id)).await;
    let account_token = login(&fixture.state, &email).await;

    let expired_at = (OffsetDateTime::now_utc() - time::Duration::hours(1))
        .format(&time::format_description::well_known::Rfc3339)
        .expect("timestamp must format");

    let expiring = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/bindings",
            Some(&owner),
            Some(json!({
                "user_id": account_id,
                "role_id": editor_role_id.to_string(),
                "scope_type": "organization",
                "organization_id": fixture.organization_id,
                "expires_at": expired_at,
            })),
        ),
    )
    .await;
    assert_eq!(expiring.status, StatusCode::CREATED, "{}", expiring.body);
    assert_eq!(
        expiring.body["active"], false,
        "an expired binding is not active"
    );

    // The expired binding grants nothing.
    let after_expiry = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/iam/effective-permissions",
            Some(&account_token),
            None,
        ),
    )
    .await;
    let keys = keys_of(&after_expiry.body, "granted", "key");
    assert!(
        !keys.contains(&"content.pages.update".to_owned()),
        "an expired binding must not count: {keys:?}"
    );

    // Re-assigning the same role works: the expired row is retired, not immortal.
    let re_granted = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/bindings",
            Some(&owner),
            Some(json!({
                "user_id": account_id,
                "role_id": editor_role_id.to_string(),
                "scope_type": "organization",
                "organization_id": fixture.organization_id,
            })),
        ),
    )
    .await;
    assert_eq!(
        re_granted.status,
        StatusCode::CREATED,
        "re-grant body: {}",
        re_granted.body
    );

    let live = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/iam/effective-permissions",
            Some(&account_token),
            None,
        ),
    )
    .await;
    let keys = keys_of(&live.body, "granted", "key");
    assert!(
        keys.contains(&"content.pages.update".to_owned()),
        "{keys:?}"
    );

    // The same live combination cannot be granted twice.
    let duplicate = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/bindings",
            Some(&owner),
            Some(json!({
                "user_id": account_id,
                "role_id": editor_role_id.to_string(),
                "scope_type": "organization",
                "organization_id": fixture.organization_id,
            })),
        ),
    )
    .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT);
    assert_eq!(duplicate.body["error"]["code"], "already_bound");

    fixture.cleanup().await;
}

#[tokio::test]
async fn cross_organization_work_and_system_roles_are_refused() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.owner_token().await;

    // A custom role in the fixture's organization, to prove isolation.
    let foreign_key = format!("foreign-{}", Uuid::new_v4().simple());
    let foreign = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/roles",
            Some(&owner),
            Some(json!({
                "key": foreign_key,
                "name": "Foreign Role",
                "organization_id": fixture.organization_id,
            })),
        ),
    )
    .await;
    assert_eq!(foreign.status, StatusCode::CREATED, "{}", foreign.body);
    let foreign_role_id = foreign.body["id"].as_str().expect("id").to_owned();

    // A second organization with an Administrator of its own.
    let other_org: Uuid =
        sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
            .bind("Other Test Organization")
            .bind(format!("iam-other-{}", Uuid::new_v4().simple()))
            .fetch_one(fixture.db.pool())
            .await
            .expect("the second organization must be created");
    let (other_id, other_email) = fixture.add_account(Some(other_org)).await;
    let administrator = fixture.system_role("administrator").await;

    bindings::grant(
        fixture.db.pool(),
        NewBinding {
            role_id: administrator,
            user_id: other_id,
            scope: Scope::Organization {
                organization_id: other_org,
            },
            granted_by: None,
            expires_at: None,
        },
    )
    .await
    .expect("the administrator binding must be created");

    let other_token = login(&fixture.state, &other_email).await;

    // Its role list stays inside its own organization.
    let list = call(
        &fixture.state,
        request(Method::GET, "/api/v1/iam/roles", Some(&other_token), None),
    )
    .await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.body);
    let visible = keys_of(&list.body, "roles", "id");
    assert!(
        !visible.contains(&foreign_role_id),
        "another organization's role must not be visible: {visible:?}"
    );
    assert!(visible.contains(&administrator.to_string()));

    // Creating a role in another organization is refused.
    let cross_create = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/roles",
            Some(&other_token),
            Some(json!({
                "key": format!("cross-{}", Uuid::new_v4().simple()),
                "name": "Cross Tenant Role",
                "organization_id": fixture.organization_id,
            })),
        ),
    )
    .await;
    assert_eq!(
        cross_create.status,
        StatusCode::FORBIDDEN,
        "{}",
        cross_create.body
    );
    assert_eq!(cross_create.body["error"]["code"], "cross_organization");

    // Reading another organization's permissions and audit trail is refused too.
    let cross_permissions = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/iam/effective-permissions?user_id={}&organization_id={}",
                fixture.owner_id, fixture.organization_id
            ),
            Some(&other_token),
            None,
        ),
    )
    .await;
    assert_eq!(cross_permissions.status, StatusCode::FORBIDDEN);
    assert_eq!(
        cross_permissions.body["error"]["code"],
        "cross_organization"
    );

    let cross_audit = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/iam/audit?organization_id={}",
                fixture.organization_id
            ),
            Some(&other_token),
            None,
        ),
    )
    .await;
    assert_eq!(cross_audit.status, StatusCode::FORBIDDEN);

    // Platform roles cannot be edited, even by an Administrator.
    let editor_role_id = fixture.system_role("editor").await;
    let system_edit = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/iam/roles/{editor_role_id}/permissions"),
            Some(&other_token),
            Some(json!({ "permissions": [] })),
        ),
    )
    .await;
    assert_eq!(
        system_edit.status,
        StatusCode::FORBIDDEN,
        "{}",
        system_edit.body
    );
    assert_eq!(system_edit.body["error"]["code"], "system_role");

    // An unknown permission key is a client error, not a silent no-op.
    let bad_key = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/iam/roles/{foreign_role_id}/permissions"),
            Some(&owner),
            Some(json!({ "permissions": [{ "key": "content.pages.explode", "effect": "allow" }] })),
        ),
    )
    .await;
    assert_eq!(bad_key.status, StatusCode::BAD_REQUEST, "{}", bad_key.body);
    assert_eq!(bad_key.body["error"]["code"], "invalid_request");

    // Without a session nothing is answered at all.
    let anonymous = call(
        &fixture.state,
        request(Method::GET, "/api/v1/iam/roles", None, None),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    assert_eq!(anonymous.body["error"]["code"], "unauthenticated");

    let anonymous_self = call(
        &fixture.state,
        request(Method::GET, "/api/v1/iam/effective-permissions", None, None),
    )
    .await;
    assert_eq!(anonymous_self.status, StatusCode::UNAUTHORIZED);

    sqlx::query("delete from users where id = $1")
        .bind(other_id)
        .execute(fixture.db.pool())
        .await
        .expect("account cleanup must run");
    sqlx::query("delete from organizations where id = $1")
        .bind(other_org)
        .execute(fixture.db.pool())
        .await
        .expect("organization cleanup must run");

    fixture.cleanup().await;
}
