//! Integration tests for the ABAC policy surface and the permission safety invariants
//! (docs/requests/REQ-006, slice 4a; docs/07-IAM.md §11, §19).
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) — locally and in CI. When
//! PostgreSQL is not reachable the suite skips itself with a printed reason, so `cargo test`
//! stays usable on a machine without Docker.
//!
//! The walk proves, over the real router:
//!
//! * a **deny** policy takes away a permission RBAC granted (the Owner loses `users.read`);
//! * an **allow** policy grants a permission RBAC never gave (a member with no binding reads the
//!   user list);
//! * priority decides between them, a disabled policy decides nothing, and the simulator's verdict
//!   equals the guard's in every one of those states;
//! * the dry run answers with the leaf-by-leaf evaluation (before anything is saved);
//! * every save stores a version, and the history reads back;
//! * the safety invariants refuse removing the organization's last owner binding and the caller's
//!   own last privileged binding, while an ordinary revocation still goes through;
//! * every change lands in the audit trail and the `iam.policy_changed` event stream.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::{bindings, roles as role_store, seed};
use serde_json::{Value, json};
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

/// A fresh organization with an Owner, and the accounts it created.
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

        let slug = format!("iam-policy-{}", Uuid::new_v4().simple());
        let organization_id: Uuid = sqlx::query_scalar(
            "insert into organizations (name, slug) values ($1, $2) returning id",
        )
        .bind("IAM Policy Test Organization")
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

    /// The id of one live binding of an account, by role key.
    async fn binding_of(&self, user_id: Uuid, role_key: &str) -> Uuid {
        let role_id = self.system_role(role_key).await;
        bindings::list_for_user(self.db.pool(), user_id)
            .await
            .expect("bindings must load")
            .into_iter()
            .find(|binding| binding.role_id == role_id && binding.revoked_at.is_none())
            .unwrap_or_else(|| panic!("the account must carry a live {role_key} binding"))
            .id
    }

    /// Remove what this fixture created: bindings, accounts, policies and the organization.
    async fn cleanup(&self) {
        sqlx::query("delete from role_bindings where subject_id = any($1) or user_id = any($1)")
            .bind(&self.accounts)
            .execute(self.db.pool())
            .await
            .expect("binding cleanup must run");
        sqlx::query("delete from policies where organization_id = $1")
            .bind(self.organization_id)
            .execute(self.db.pool())
            .await
            .expect("policy cleanup must run");
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
    let email = format!("iam-policy-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "IAM Policy Test".to_owned(),
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

/// Create a policy over HTTP and return its id.
async fn create_policy(
    fixture: &Fixture,
    token: &str,
    name: &str,
    effect: &str,
    priority: i64,
    targets: Value,
    conditions: Value,
) -> String {
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/policies",
            Some(token),
            Some(json!({
                "name": name,
                "description": "walk",
                "effect": effect,
                "priority": priority,
                "target_permissions": targets,
                "conditions": conditions,
                "organization_id": fixture.organization_id,
            })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "create {name}: {}",
        response.body
    );
    response.body["id"].as_str().expect("policy id").to_owned()
}

/// Save a policy over HTTP.
async fn update_policy(
    fixture: &Fixture,
    token: &str,
    id: &str,
    name: &str,
    effect: &str,
    priority: i64,
    enabled: bool,
) -> Value {
    let response = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/iam/policies/{id}"),
            Some(token),
            Some(json!({
                "name": name,
                "description": "walk",
                "effect": effect,
                "priority": priority,
                "target_permissions": ["users.read"],
                "conditions": conditions_always(),
                "enabled": enabled,
            })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "update {name}: {}",
        response.body
    );
    response.body
}

/// A condition the engine reads as "always true" for these walks.
fn conditions_always() -> Value {
    json!({ "all": [] })
}

/// A condition that pins one attribute to one value.
fn condition_equals(attribute: &str, value: Value) -> Value {
    json!({ "all": [{ "attribute": attribute, "operator": "==", "value": value }] })
}

/// The simulated verdict for one subject, permission and (the caller's) context.
async fn simulate(
    fixture: &Fixture,
    token: &str,
    owner: bool,
    subject_id: Uuid,
    permission: &str,
) -> Value {
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/simulations",
            Some(token),
            Some(json!({
                "subject_type": "user",
                "subject_id": subject_id,
                "permission": permission,
                "organization_id": fixture.organization_id,
            })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "simulate ({owner}) {permission}: {}",
        response.body
    );
    response.body
}

/// The `action` values in an audit response.
fn audit_actions(body: &Value) -> Vec<String> {
    body["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("entries must be an array in {body}"))
        .iter()
        .filter_map(|entry| entry["action"].as_str().map(str::to_owned))
        .collect()
}

/// REQ-006, slice 4a: the ABAC policy engine over the real router, plus the safety invariants.
#[tokio::test]
async fn abac_policies_and_the_safety_invariants_are_proven_end_to_end() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.owner_token().await;
    let organization_id = fixture.organization_id;
    let (member_id, member_email) = fixture.add_account(Some(organization_id)).await;
    let member = login(&fixture.state, &member_email).await;
    let subject_member = condition_equals("subject.id", json!(member_id.to_string()));

    // --- The surface is permission-gated ------------------------------------------------------
    let anonymous = call(
        &fixture.state,
        request(Method::GET, "/api/v1/iam/policies", None, None),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    let refused = call(
        &fixture.state,
        request(Method::GET, "/api/v1/iam/policies", Some(&member), None),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a member without iam.policies.read is refused: {}",
        refused.body
    );
    assert_eq!(refused.body["error"]["code"], "permission_denied");

    // --- A deny policy takes away what RBAC granted -------------------------------------------
    // The Owner holds `users.read` through the Owner role; with no policy the call succeeds.
    let before = call(
        &fixture.state,
        request(Method::GET, "/api/v1/iam/users", Some(&owner), None),
    )
    .await;
    assert_eq!(before.status, StatusCode::OK, "{}", before.body);

    let deny_id = create_policy(
        &fixture,
        &owner,
        "Freeze user reads",
        "deny",
        300,
        json!(["users.read"]),
        conditions_always(),
    )
    .await;

    let denied = call(
        &fixture.state,
        request(Method::GET, "/api/v1/iam/users", Some(&owner), None),
    )
    .await;
    assert_eq!(
        denied.status,
        StatusCode::FORBIDDEN,
        "the deny policy takes the permission away: {}",
        denied.body
    );
    assert_eq!(denied.body["error"]["code"], "permission_denied");
    assert_eq!(denied.body["error"]["details"]["reason"], "policy_denied");
    assert_eq!(
        denied.body["error"]["details"]["source"]["role_key"],
        "policy"
    );
    assert_eq!(
        denied.body["error"]["details"]["source"]["policy"]["policy_name"],
        "Freeze user reads"
    );

    // The simulator agrees with the guard (one decision path).
    let owner_verdict = simulate(&fixture, &owner, true, fixture.owner_id, "users.read").await;
    assert_eq!(owner_verdict["allowed"], false);
    assert_eq!(owner_verdict["reason"], "policy_denied");
    assert_eq!(owner_verdict["source"]["role_key"], "policy");
    assert_eq!(
        owner_verdict["source"]["policy"]["policy_id"].as_str(),
        Some(deny_id.as_str())
    );
    let policy_steps = owner_verdict["policies"]
        .as_array()
        .expect("the policy list is reported");
    assert!(
        policy_steps
            .iter()
            .any(|policy| policy["policy_id"] == deny_id.as_str()
                && policy["applies"] == true
                && policy["targeted"] == true
                && policy["satisfied"] == true),
        "the simulator names the policy that applies: {owner_verdict}"
    );

    // --- An allow policy grants what RBAC never gave ------------------------------------------
    // The member has no binding at all: RBAC grants nothing (the deny policy is in play for
    // everyone meanwhile, which is exactly why the allow below has to flip the decision).
    let member_before = call(
        &fixture.state,
        request(Method::GET, "/api/v1/iam/users", Some(&member), None),
    )
    .await;
    assert_eq!(member_before.status, StatusCode::FORBIDDEN);

    let member_rbac = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/iam/effective-permissions?user_id={member_id}"),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(member_rbac.status, StatusCode::OK, "{}", member_rbac.body);
    assert!(
        member_rbac.body["granted"]
            .as_array()
            .expect("granted must be an array")
            .iter()
            .all(|entry| entry["key"] != "users.read"),
        "the member's RBAC set must not carry users.read: {}",
        member_rbac.body
    );

    let allow_id = create_policy(
        &fixture,
        &owner,
        "Readers for the new member",
        "allow",
        500,
        json!(["users.read"]),
        subject_member.clone(),
    )
    .await;

    let member_after = call(
        &fixture.state,
        request(Method::GET, "/api/v1/iam/users", Some(&member), None),
    )
    .await;
    assert_eq!(
        member_after.status,
        StatusCode::OK,
        "the allow policy grants the permission: {}",
        member_after.body
    );

    let member_verdict = simulate(&fixture, &owner, false, member_id, "users.read").await;
    assert_eq!(member_verdict["allowed"], true, "{member_verdict}");
    assert_eq!(member_verdict["reason"], "allowed");
    assert_eq!(
        member_verdict["source"]["policy"]["policy_name"],
        "Readers for the new member"
    );

    // The Owner is untouched by the allow policy — its condition names the member only.
    let owner_still_denied = simulate(&fixture, &owner, true, fixture.owner_id, "users.read").await;
    assert_eq!(owner_still_denied["allowed"], false);
    assert_eq!(owner_still_denied["reason"], "policy_denied");

    // --- Priority decides, and a disabled policy decides nothing -------------------------------
    let raised = update_policy(
        &fixture,
        &owner,
        &deny_id,
        "Freeze user reads",
        "deny",
        900,
        true,
    )
    .await;
    assert_eq!(raised["version"], 2, "a save moves the version");

    let member_denied_again = call(
        &fixture.state,
        request(Method::GET, "/api/v1/iam/users", Some(&member), None),
    )
    .await;
    assert_eq!(
        member_denied_again.status,
        StatusCode::FORBIDDEN,
        "the higher-priority deny wins over the allow: {}",
        member_denied_again.body
    );
    assert_eq!(
        member_denied_again.body["error"]["details"]["reason"],
        "policy_denied"
    );

    let disabled = update_policy(
        &fixture,
        &owner,
        &deny_id,
        "Freeze user reads",
        "deny",
        900,
        false,
    )
    .await;
    assert_eq!(disabled["version"], 3);

    let member_allowed_again = call(
        &fixture.state,
        request(Method::GET, "/api/v1/iam/users", Some(&member), None),
    )
    .await;
    assert_eq!(
        member_allowed_again.status,
        StatusCode::OK,
        "a disabled policy decides nothing: {}",
        member_allowed_again.body
    );

    // --- The history reads back ----------------------------------------------------------------
    let history = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/iam/policies/{deny_id}/versions"),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(history.status, StatusCode::OK, "{}", history.body);
    let versions = history.body["versions"]
        .as_array()
        .expect("versions must be an array");
    assert_eq!(
        versions.len(),
        3,
        "every save stored a version: {}",
        history.body
    );
    assert_eq!(history.body["current_version"], 3);
    assert_eq!(versions[0]["version"], 3);
    assert_eq!(versions[0]["enabled"], false);

    // --- The dry run (nothing is written) ------------------------------------------------------
    let dry_run = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/iam/policies/{deny_id}/test"),
            Some(&owner),
            Some(json!({
                "permission": "users.read",
                "attributes": { "resource": { "path": "/legal/terms" } },
                "policy": {
                    "name": "Freeze user reads",
                    "effect": "deny",
                    "priority": 900,
                    "target_permissions": ["users.read"],
                    "conditions": json!({"all": [
                        {"attribute": "action", "operator": "==", "value": "users.read"},
                        {"attribute": "resource.path", "operator": "starts_with", "value": "/legal"}
                    ]}),
                },
            })),
        ),
    )
    .await;
    assert_eq!(dry_run.status, StatusCode::OK, "{}", dry_run.body);
    assert_eq!(dry_run.body["draft"], true, "the unsaved draft was tested");
    assert_eq!(dry_run.body["targeted"], true);
    assert_eq!(dry_run.body["conditions_satisfied"], true);
    assert_eq!(dry_run.body["applies"], true);
    assert_eq!(dry_run.body["effect"], "deny");
    let leaves = dry_run.body["trace"]
        .as_array()
        .expect("the trace is a leaf list");
    assert_eq!(leaves.len(), 2);
    assert!(
        leaves.iter().all(|leaf| leaf["satisfied"] == true),
        "every leaf is highlighted as matched: {}",
        dry_run.body
    );
    assert_eq!(
        leaves[1]["resolved"], "/legal/terms",
        "the resolved value is reported"
    );

    let dry_run_miss = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/iam/policies/{deny_id}/test"),
            Some(&owner),
            Some(json!({
                "permission": "users.read",
                "attributes": { "resource": { "path": "/blog/hello" } },
                "policy": {
                    "name": "Freeze user reads",
                    "effect": "deny",
                    "priority": 900,
                    "target_permissions": ["users.read"],
                    "conditions": json!({"all": [
                        {"attribute": "resource.path", "operator": "starts_with", "value": "/legal"}
                    ]}),
                },
            })),
        ),
    )
    .await;
    assert_eq!(dry_run_miss.status, StatusCode::OK, "{}", dry_run_miss.body);
    assert_eq!(dry_run_miss.body["applies"], false);
    assert_eq!(dry_run_miss.body["trace"][0]["satisfied"], false);
    assert_eq!(
        dry_run_miss.body["trace"][0]["resolved"], "/blog/hello",
        "the dry run shows what the attribute resolved to"
    );

    // --- Refusals the reader can act on --------------------------------------------------------
    for (body, expected) in [
        (
            json!({
                "name": "Unknown target",
                "effect": "deny",
                "priority": 400,
                "target_permissions": ["nope.nothing"],
                "conditions": conditions_always(),
                "organization_id": organization_id,
            }),
            "invalid_policy",
        ),
        (
            json!({
                "name": "Unknown operator",
                "effect": "deny",
                "priority": 400,
                "target_permissions": ["users.read"],
                "conditions": json!({"all": [{"attribute": "a", "operator": "~", "value": 1}]}),
                "organization_id": organization_id,
            }),
            "invalid_policy",
        ),
        (
            json!({
                "name": "Priority out of range",
                "effect": "deny",
                "priority": 1001,
                "target_permissions": ["users.read"],
                "conditions": conditions_always(),
                "organization_id": organization_id,
            }),
            "invalid_policy",
        ),
        (
            json!({
                "name": "   ",
                "effect": "deny",
                "priority": 400,
                "target_permissions": ["users.read"],
                "conditions": conditions_always(),
                "organization_id": organization_id,
            }),
            "invalid_policy",
        ),
    ] {
        let response = call(
            &fixture.state,
            request(
                Method::POST,
                "/api/v1/iam/policies",
                Some(&owner),
                Some(body.clone()),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "refusal for {body}: {}",
            response.body
        );
        assert_eq!(response.body["error"]["code"], expected);
    }

    // --- The safety invariants -----------------------------------------------------------------
    // The fixture Owner holds the platform (global) owner binding: revoking it would leave them
    // without a privileged binding of their own, whatever else the platform carries.
    let owner_binding = fixture.binding_of(fixture.owner_id, "owner").await;
    let self_lockout = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/iam/bindings/{owner_binding}"),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(
        self_lockout.status,
        StatusCode::CONFLICT,
        "the caller keeps a privileged binding of their own: {}",
        self_lockout.body
    );
    assert_eq!(self_lockout.body["error"]["code"], "self_lockout");
    assert!(
        self_lockout.body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("your own last")),
        "the refusal names the invariant: {}",
        self_lockout.body
    );

    // The refused change left the row alone.
    let still_bound = fixture.binding_of(fixture.owner_id, "owner").await;
    assert_eq!(
        still_bound, owner_binding,
        "a refused change must not revoke"
    );

    // An organization-scoped binding is defended by its own class: with no other privileged
    // binding in the organization, removing it is refused even though the caller is not its
    // subject.
    let (second_id, second_email) = fixture.add_account(Some(organization_id)).await;
    let _second = login(&fixture.state, &second_email).await;
    let owner_role = fixture.system_role("owner").await.to_string();
    let second_binding = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/bindings",
            Some(&owner),
            Some(json!({
                "subject_type": "user",
                "subject_id": second_id,
                "role_id": owner_role,
                "scope_type": "organization",
                "organization_id": organization_id,
            })),
        ),
    )
    .await;
    assert_eq!(
        second_binding.status,
        StatusCode::CREATED,
        "{}",
        second_binding.body
    );
    let second_binding_id = second_binding.body["id"]
        .as_str()
        .expect("binding id")
        .to_owned();

    let last_owner = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/iam/bindings/{second_binding_id}"),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(
        last_owner.status,
        StatusCode::CONFLICT,
        "the organization's last privileged binding is defended: {}",
        last_owner.body
    );
    assert_eq!(last_owner.body["error"]["code"], "last_owner_binding");
    assert!(
        last_owner.body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("at least one")),
        "the refusal names the invariant: {}",
        last_owner.body
    );

    // A second privileged binding in the same organization makes an ordinary revocation possible:
    // the subject is somebody else, and the organization keeps a live privileged binding.
    let (third_id, third_email) = fixture.add_account(Some(organization_id)).await;
    let _third = login(&fixture.state, &third_email).await;
    let administrator_role = fixture.system_role("administrator").await.to_string();
    let third_binding = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/bindings",
            Some(&owner),
            Some(json!({
                "subject_type": "user",
                "subject_id": third_id,
                "role_id": administrator_role,
                "scope_type": "organization",
                "organization_id": organization_id,
            })),
        ),
    )
    .await;
    assert_eq!(
        third_binding.status,
        StatusCode::CREATED,
        "{}",
        third_binding.body
    );

    let ordinary = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/iam/bindings/{second_binding_id}"),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(
        ordinary.status,
        StatusCode::OK,
        "an ordinary revocation is not blocked: {}",
        ordinary.body
    );

    // --- Removing a policy ---------------------------------------------------------------------
    let removed = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/iam/policies/{allow_id}"),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.body);
    assert_eq!(removed.body["deleted"], true);

    let gone = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/iam/policies/{allow_id}"),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    assert_eq!(gone.body["error"]["code"], "policy_not_found");

    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/iam/policies?organization_id={organization_id}"),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    let names: Vec<&str> = listed.body["policies"]
        .as_array()
        .expect("policies must be an array")
        .iter()
        .filter_map(|policy| policy["name"].as_str())
        .collect();
    assert!(names.contains(&"Freeze user reads"));
    assert!(!names.contains(&"Readers for the new member"));

    // --- The trail -----------------------------------------------------------------------------
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
    assert_eq!(audit.status, StatusCode::OK, "{}", audit.body);
    let actions = audit_actions(&audit.body);
    for action in [
        "iam.policy.created",
        "iam.policy.updated",
        "iam.policy.deleted",
        "iam.binding.granted",
        "iam.binding.revoked",
    ] {
        assert!(
            actions.iter().any(|entry| entry == action),
            "{action} must be in the audit trail: {actions:?}"
        );
    }

    let events: i64 = sqlx::query_scalar(
        "select count(*) from events where name = 'iam.policy_changed' and organization_id = $1",
    )
    .bind(organization_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the event count must run");
    assert!(
        events >= 5,
        "every create, update and delete emits iam.policy_changed (saw {events})"
    );

    fixture.cleanup().await;
}
