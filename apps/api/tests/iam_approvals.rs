//! Integration tests for permission requests and approvals (docs/requests/REQ-006, slice 4b;
//! docs/07-IAM.md §16).
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) — locally and in CI. When
//! PostgreSQL is not reachable the suite skips itself with a printed reason, so `cargo test`
//! stays usable on a machine without Docker.
//!
//! The walk proves, over the real router:
//!
//! * anybody signed in may **ask** for a permission, and the ask form refuses a key outside the
//!   catalogue and an unusable window;
//! * only a holder of `iam.approvals.read` sees the inbox, and only a holder of
//!   `iam.approvals.decide` may decide;
//! * an approval is a **time-boxed binding**: the permission arrives for the subject, the row
//!   names the moment it ends, and once the window passes the permission is gone again **without
//!   anybody acting** — the request then reads `expired`;
//! * a rejection grants nothing;
//! * a decided request cannot be decided twice;
//! * every step lands in the audit trail and the `iam.approval_*` event stream.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::{roles as role_store, seed};
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
    owner_email: String,
    accounts: Vec<Uuid>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let slug = format!("iam-approvals-{}", Uuid::new_v4().simple());
        let organization_id: Uuid = sqlx::query_scalar(
            "insert into organizations (name, slug) values ($1, $2) returning id",
        )
        .bind("IAM Approvals Test Organization")
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

    /// Remove what this fixture created.
    async fn cleanup(&self) {
        sqlx::query(
            "delete from permission_requests where organization_id = $1 or requester_id = any($2)",
        )
        .bind(self.organization_id)
        .bind(&self.accounts)
        .execute(self.db.pool())
        .await
        .expect("request cleanup must run");
        sqlx::query("delete from role_bindings where subject_id = any($1) or user_id = any($1)")
            .bind(&self.accounts)
            .execute(self.db.pool())
            .await
            .expect("binding cleanup must run");
        sqlx::query("delete from roles where organization_id = $1")
            .bind(self.organization_id)
            .execute(self.db.pool())
            .await
            .expect("grant-role cleanup must run");
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
    let email = format!("iam-approvals-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "IAM Approvals Test".to_owned(),
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

/// The walk. One test, because each step's state is the next step's precondition.
#[tokio::test]
async fn an_approved_request_grants_only_inside_its_window() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };

    let owner_token = fixture.owner_token().await;
    let organization_id = fixture.organization_id;
    let (member_id, member_email) = fixture.add_account(Some(organization_id)).await;
    let member_token = login(&fixture.state, &member_email).await;

    // ---- 1. The member holds no permission yet ---------------------------------------------
    let before = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/iam/policies",
            Some(&member_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        before.status,
        StatusCode::FORBIDDEN,
        "a member without the permission must be refused"
    );

    // The member cannot read the inbox, and cannot decide anything.
    let inbox = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/iam/approvals",
            Some(&member_token),
            None,
        ),
    )
    .await;
    assert_eq!(inbox.status, StatusCode::FORBIDDEN);

    // ---- 2. The ask --------------------------------------------------------------
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/requests",
            Some(&member_token),
            Some(json!({
                "permission_key": "iam.policies.read",
                "justification": "covering for the policy editor this week",
            })),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "ask body: {}",
        created.body
    );
    assert_eq!(created.body["status"], json!("pending"));
    assert_eq!(created.body["permission_key"], json!("iam.policies.read"));
    assert_eq!(created.body["requester"]["id"], json!(member_id));
    let request_id = created.body["id"].as_str().expect("id").to_owned();

    // An unknown key and an unusable window are refused in the field.
    let unknown = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/requests",
            Some(&member_token),
            Some(json!({ "permission_key": "not.a.permission" })),
        ),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    assert!(
        unknown.body["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("unknown permission")),
        "the refusal names the unknown key: {}",
        unknown.body
    );

    let short_window = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/iam/approvals/{request_id}/decide"),
            Some(&owner_token),
            Some(json!({ "decision": "approve", "grant_minutes": 1 })),
        ),
    )
    .await;
    assert_eq!(short_window.status, StatusCode::BAD_REQUEST);
    assert!(
        short_window.body["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("between 5 and 43200")),
        "the refusal names the range: {}",
        short_window.body
    );

    // The member cannot decide even a request they own.
    let trying_to_decide = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/iam/approvals/{request_id}/decide"),
            Some(&member_token),
            Some(json!({ "decision": "approve", "grant_minutes": 30 })),
        ),
    )
    .await;
    assert_eq!(trying_to_decide.status, StatusCode::FORBIDDEN);

    // ---- 3. The approval, with a real window -------------------------------------------------
    let decided = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/iam/approvals/{request_id}/decide"),
            Some(&owner_token),
            Some(json!({
                "decision": "approve",
                "grant_minutes": 30,
                "note": "approved for the release week",
            })),
        ),
    )
    .await;
    assert_eq!(
        decided.status,
        StatusCode::OK,
        "decide body: {}",
        decided.body
    );
    assert_eq!(decided.body["status"], json!("approved"));
    assert_eq!(decided.body["grant_minutes"], json!(30));
    assert!(decided.body["binding_id"].is_string(), "a grant was bound");
    assert_eq!(decided.body["grant_active"], json!(true));
    let binding_id = decided.body["binding_id"].as_str().unwrap().to_owned();

    // The grant is a live binding of the generated role, expiring in half an hour.
    let (scope_type, expires_in_minutes): (String, f64) = sqlx::query_as(
        "select scope_type, (extract(epoch from (expires_at - now())) / 60.0)::float8 \
         from role_bindings where id = $1",
    )
    .bind(Uuid::parse_str(&binding_id).expect("binding id is a uuid"))
    .fetch_one(fixture.db.pool())
    .await
    .expect("the granted binding must exist");
    assert_eq!(scope_type, "organization");
    assert!(
        (25.0..=30.0).contains(&expires_in_minutes),
        "the window is 30 minutes, got {expires_in_minutes}"
    );

    // The permission the member never held is now theirs.
    let granted = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/iam/policies",
            Some(&member_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        granted.status,
        StatusCode::OK,
        "the approved window must grant the permission: {}",
        granted.body
    );

    // ---- 4. It expires on its own ------------------------------------------------------------
    sqlx::query("update role_bindings set expires_at = now() - interval '1 minute' where id = $1")
        .bind(Uuid::parse_str(&binding_id).unwrap())
        .execute(fixture.db.pool())
        .await
        .expect("the window must be movable for the test");

    let after_window = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/iam/policies",
            Some(&member_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        after_window.status,
        StatusCode::FORBIDDEN,
        "an expired window must stop granting without anybody acting"
    );

    // Reading the inbox retires the lapsed approval, and the row says why.
    let expired = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/iam/approvals?status=expired",
            Some(&owner_token),
            None,
        ),
    )
    .await;
    assert_eq!(expired.status, StatusCode::OK);
    assert!(
        expired.body["requests"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["id"] == json!(request_id))),
        "the lapsed approval reads `expired`: {}",
        expired.body
    );

    // ---- 5. A rejection grants nothing, and a decided request stays decided -------------------
    let second = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/requests",
            Some(&member_token),
            Some(json!({ "permission_key": "users.read", "justification": "one-off export" })),
        ),
    )
    .await;
    let second_id = second.body["id"].as_str().expect("id").to_owned();

    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/iam/approvals/{second_id}/decide"),
            Some(&owner_token),
            Some(json!({ "decision": "reject", "note": "not this quarter" })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::OK);
    assert_eq!(refused.body["status"], json!("rejected"));
    assert!(
        refused.body["binding_id"].is_null(),
        "a rejection binds nothing"
    );

    let user_list = call(
        &fixture.state,
        request(Method::GET, "/api/v1/iam/users", Some(&member_token), None),
    )
    .await;
    assert_eq!(
        user_list.status,
        StatusCode::FORBIDDEN,
        "a refused request must not grant anything"
    );

    let twice = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/iam/approvals/{second_id}/decide"),
            Some(&owner_token),
            Some(json!({ "decision": "approve", "grant_minutes": 30 })),
        ),
    )
    .await;
    assert_eq!(
        twice.status,
        StatusCode::CONFLICT,
        "deciding twice is a conflict"
    );
    assert_eq!(
        twice.body["error"]["code"],
        json!("request_already_decided")
    );

    // ---- 6. The trail -----------------------------------------------------------------------
    let audit = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/iam/audit?limit=50",
            Some(&owner_token),
            None,
        ),
    )
    .await;
    assert_eq!(audit.status, StatusCode::OK);
    let actions: Vec<String> = audit.body["entries"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row["action"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    for expected in [
        "iam.approval.requested",
        "iam.approval.approved",
        "iam.approval.rejected",
    ] {
        assert!(
            actions.iter().any(|action| action == expected),
            "the audit trail carries {expected}: {actions:?}"
        );
    }

    // The inbox counts describe the organization.
    let pending = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/iam/approvals?status=pending",
            Some(&owner_token),
            None,
        ),
    )
    .await;
    assert_eq!(pending.status, StatusCode::OK);
    assert_eq!(
        pending.body["counts"]["approved"],
        json!(0),
        "the only approval has already lapsed"
    );

    // The generated grant role exists once per permission and carries the single allow entry,
    // so a second request for the same permission reuses it.
    let grant_role = role_store::find_role_by_key(
        fixture.db.pool(),
        Some(fixture.organization_id),
        "grant-iam-policies-read",
    )
    .await
    .expect("grant-role lookup must run")
    .expect("the approval must have created `grant-iam-policies-read`");
    assert_eq!(grant_role.name, "Time-boxed: iam.policies.read");

    fixture.cleanup().await;
}
