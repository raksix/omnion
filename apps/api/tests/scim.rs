//! Integration tests for SCIM 2.0 provisioning (docs/requests/REQ-006, slice 4b;
//! docs/07-IAM.md §19).
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) — locally and in CI. When
//! PostgreSQL is not reachable the suite skips itself with a printed reason, so `cargo test`
//! stays usable on a machine without Docker.
//!
//! The walk proves, over the real router:
//!
//! * `ServiceProviderConfig` and `Schemas` answer without a token (a client must be able to
//!   introspect the endpoint); everything else refuses a missing, unknown or revoked token;
//! * a token is minted on the provisioning surface, its secret is returned **once** and only its
//!   hash reaches the database;
//! * a SCIM **create → filter → patch → deactivate** round trip provisions a real account, and
//!   deactivation leaves the account in place (the SCIM default);
//! * every one of those calls lands in the sync log with its action and outcome;
//! * groups provision and take members;
//! * a session cookie is not a provisioning token: the surface refuses a signed-in browser.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::seed;
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

/// Build a request; `token` becomes a bearer credential (or the session cookie when `session`).
fn request(
    method: Method,
    uri: &str,
    auth: Option<(&str, bool)>,
    body: Option<Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some((token, session)) = auth {
        builder = if session {
            builder.header(header::COOKIE, format!("omnion_session={token}"))
        } else {
            builder.header(header::AUTHORIZATION, format!("Bearer {token}"))
        };
    }

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
    provisioned: Vec<String>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let slug = format!("scim-{}", Uuid::new_v4().simple());
        let organization_id: Uuid = sqlx::query_scalar(
            "insert into organizations (name, slug) values ($1, $2) returning id",
        )
        .bind("SCIM Test Organization")
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created");

        let email = format!("scim-owner-{}@omnion.test", Uuid::new_v4().simple());
        let owner = users::create_user(
            db.pool(),
            NewUser {
                email: email.clone(),
                password: PASSWORD.to_owned(),
                display_name: "SCIM Test Owner".to_owned(),
                organization_id: Some(organization_id),
            },
        )
        .await
        .expect("the owner must be created");
        seed::bind_owner(db.pool(), owner.id)
            .await
            .expect("the owner binding must be created");

        Some(Self {
            state,
            db,
            organization_id,
            owner_id: owner.id,
            owner_email: email,
            provisioned: Vec::new(),
        })
    }

    /// The Owner of the fixture, signed in.
    async fn owner_token(&self) -> String {
        let response = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/auth/login",
                None,
                Some(json!({ "email": self.owner_email, "password": PASSWORD })),
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "login: {}", response.body);

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

    /// Remember a provisioned address so cleanup can find it.
    fn remember(&mut self, email: &str) {
        self.provisioned.push(email.to_owned());
    }

    /// Remove what this fixture created.
    async fn cleanup(&self) {
        for email in &self.provisioned {
            let user = users::find_by_email(self.db.pool(), email)
                .await
                .expect("lookup must run");
            if let Some(user) = user {
                sqlx::query("delete from group_members where user_id = $1")
                    .bind(user.id)
                    .execute(self.db.pool())
                    .await
                    .expect("membership cleanup must run");
                sqlx::query("delete from users where id = $1")
                    .bind(user.id)
                    .execute(self.db.pool())
                    .await
                    .expect("account cleanup must run");
            }
        }
        sqlx::query("delete from provisioning_log where organization_id = $1")
            .bind(self.organization_id)
            .execute(self.db.pool())
            .await
            .expect("log cleanup must run");
        sqlx::query("delete from provisioning_tokens where organization_id = $1")
            .bind(self.organization_id)
            .execute(self.db.pool())
            .await
            .expect("token cleanup must run");
        sqlx::query("delete from group_members where group_id in (select id from groups where organization_id = $1)")
            .bind(self.organization_id)
            .execute(self.db.pool())
            .await
            .expect("group membership cleanup must run");
        sqlx::query("delete from groups where organization_id = $1")
            .bind(self.organization_id)
            .execute(self.db.pool())
            .await
            .expect("group cleanup must run");
        sqlx::query("delete from role_bindings where subject_id = $1 or user_id = $1")
            .bind(self.owner_id)
            .execute(self.db.pool())
            .await
            .expect("binding cleanup must run");
        sqlx::query("delete from users where id = $1")
            .bind(self.owner_id)
            .execute(self.db.pool())
            .await
            .expect("owner cleanup must run");
        sqlx::query("delete from organizations where id = $1")
            .bind(self.organization_id)
            .execute(self.db.pool())
            .await
            .expect("organization cleanup must run");
    }
}

/// The walk. One test, because each step's state is the next step's precondition.
#[tokio::test]
async fn a_scim_round_trip_provisions_and_logs() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };

    let owner_token = fixture.owner_token().await;

    // ---- 1. The introspection documents need no token; the data does -------------------------
    let config_doc = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/scim/v2/ServiceProviderConfig",
            None,
            None,
        ),
    )
    .await;
    assert_eq!(config_doc.status, StatusCode::OK);
    assert_eq!(config_doc.body["patch"]["supported"], json!(true));

    let schemas = call(
        &fixture.state,
        request(Method::GET, "/api/v1/scim/v2/Schemas", None, None),
    )
    .await;
    assert_eq!(schemas.status, StatusCode::OK);
    assert_eq!(schemas.body["totalResults"], json!(2));

    let anonymous = call(
        &fixture.state,
        request(Method::GET, "/api/v1/scim/v2/Users", None, None),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        anonymous.body["schemas"][0],
        json!("urn:ietf:params:scim:api:messages:2.0:Error")
    );

    // A signed-in browser is not a provisioning token.
    let session_call = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/scim/v2/Users",
            Some((&owner_token, true)),
            None,
        ),
    )
    .await;
    assert_eq!(session_call.status, StatusCode::UNAUTHORIZED);

    // ---- 2. Mint a token; only its hash is stored --------------------------------------------
    let minted = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/provisioning/tokens",
            Some((&owner_token, true)),
            Some(json!({ "name": "CI directory" })),
        ),
    )
    .await;
    assert_eq!(minted.status, StatusCode::CREATED, "mint: {}", minted.body);
    let secret = minted.body["secret"].as_str().expect("a secret").to_owned();
    assert!(
        secret.starts_with("omsc_"),
        "the secret carries the namespace"
    );

    let stored_hash: String =
        sqlx::query_scalar("select token_hash from provisioning_tokens where id = $1")
            .bind(Uuid::parse_str(minted.body["token"]["id"].as_str().unwrap()).unwrap())
            .fetch_one(fixture.db.pool())
            .await
            .expect("the token row must exist");
    assert_ne!(
        stored_hash, secret,
        "the secret itself never reaches the database"
    );
    assert_eq!(
        stored_hash.len(),
        64,
        "the stored value is a SHA-256 hex digest"
    );

    // ---- 3. The round trip -------------------------------------------------------------------
    let email = format!("scim-{}@omnion.test", Uuid::new_v4().simple());
    fixture.remember(&email);

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/scim/v2/Users",
            Some((&secret, false)),
            Some(json!({
                "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"],
                "userName": email,
                "displayName": "Provisioned Person",
                "externalId": "ci-4711",
                "active": true,
            })),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "create: {}",
        created.body
    );
    assert_eq!(created.body["userName"], json!(email));
    assert_eq!(created.body["active"], json!(true));
    assert_eq!(created.body["externalId"], json!("ci-4711"));
    let user_id = created.body["id"].as_str().expect("id").to_owned();

    // The account really exists, with a status a sign-in respects.
    let stored: String = sqlx::query_scalar("select status from users where id = $1")
        .bind(Uuid::parse_str(&user_id).unwrap())
        .fetch_one(fixture.db.pool())
        .await
        .expect("the provisioned account must exist");
    assert_eq!(stored, "active");

    // The documented filter finds it.
    let filtered = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/scim/v2/Users?filter=userName%20eq%20%22{email}%22"),
            Some((&secret, false)),
            None,
        ),
    )
    .await;
    assert_eq!(filtered.status, StatusCode::OK);
    assert_eq!(filtered.body["totalResults"], json!(1));

    // An unsupported filter is refused rather than half-applied.
    let bad_filter = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/scim/v2/Users?filter=userName%20co%20%22a%22",
            Some((&secret, false)),
            None,
        ),
    )
    .await;
    assert_eq!(bad_filter.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad_filter.body["scimType"], json!("invalidFilter"));

    // PATCH deactivates (a PatchOp, the shape directories send).
    let patched = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/scim/v2/Users/{user_id}"),
            Some((&secret, false)),
            Some(json!({
                "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
                "Operations": [{ "op": "replace", "path": "active", "value": false }],
            })),
        ),
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK, "patch: {}", patched.body);
    assert_eq!(patched.body["active"], json!(false));

    let status_after: String = sqlx::query_scalar("select status from users where id = $1")
        .bind(Uuid::parse_str(&user_id).unwrap())
        .fetch_one(fixture.db.pool())
        .await
        .expect("the row must still exist");
    assert_eq!(
        status_after, "disabled",
        "deactivation disables, never deletes"
    );

    // DELETE is the SCIM default deactivation; the account stays.
    let deleted = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/scim/v2/Users/{user_id}"),
            Some((&secret, false)),
            None,
        ),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);

    let still_there: i64 = sqlx::query_scalar("select count(*) from users where id = $1")
        .bind(Uuid::parse_str(&user_id).unwrap())
        .fetch_one(fixture.db.pool())
        .await
        .expect("count must run");
    assert_eq!(still_there, 1, "DELETE deactivates instead of deleting");

    // ---- 4. Groups -----------------------------------------------------------------------------
    let group_name = format!("SCIM QA {}", Uuid::new_v4().simple());
    let group = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/scim/v2/Groups",
            Some((&secret, false)),
            Some(json!({
                "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Group"],
                "displayName": group_name,
                "members": [{ "value": user_id }],
            })),
        ),
    )
    .await;
    assert_eq!(group.status, StatusCode::CREATED, "group: {}", group.body);
    assert_eq!(group.body["members"].as_array().map(Vec::len), Some(1));

    let group_id = group.body["id"].as_str().expect("group id").to_owned();
    let removed = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/scim/v2/Groups/{group_id}"),
            Some((&secret, false)),
            Some(json!({
                "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
                "Operations": [
                    { "op": "remove", "path": "members", "value": [{ "value": user_id }] }
                ],
            })),
        ),
    )
    .await;
    assert_eq!(
        removed.status,
        StatusCode::OK,
        "group patch: {}",
        removed.body
    );
    assert_eq!(removed.body["members"].as_array().map(Vec::len), Some(0));

    // ---- 5. The sync log ------------------------------------------------------------------------
    let log = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/iam/provisioning/log?limit=50",
            Some((&owner_token, true)),
            None,
        ),
    )
    .await;
    assert_eq!(log.status, StatusCode::OK);
    let rows = log.body["log"].as_array().expect("log rows").clone();
    let outcomes: Vec<(String, String)> = rows
        .iter()
        .map(|row| {
            (
                row["resource"].as_str().unwrap_or_default().to_owned(),
                row["outcome"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    assert!(
        outcomes.contains(&("user".to_owned(), "created".to_owned())),
        "the create is logged: {outcomes:?}"
    );
    assert!(
        outcomes.contains(&("user".to_owned(), "deactivated".to_owned())),
        "the deactivation is logged: {outcomes:?}"
    );
    assert!(
        outcomes.iter().any(|(resource, _)| resource == "group"),
        "the group write is logged: {outcomes:?}"
    );

    // ---- 6. Revocation is immediate --------------------------------------------------------------
    let revoked = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!(
                "/api/v1/iam/provisioning/tokens/{}",
                minted.body["token"]["id"].as_str().unwrap()
            ),
            Some((&owner_token, true)),
            None,
        ),
    )
    .await;
    assert_eq!(revoked.status, StatusCode::OK);
    assert_eq!(revoked.body["revoked"], json!(true));

    let after_revoke = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/scim/v2/Users",
            Some((&secret, false)),
            None,
        ),
    )
    .await;
    assert_eq!(
        after_revoke.status,
        StatusCode::UNAUTHORIZED,
        "a revoked token is refused on its next call"
    );

    fixture.cleanup().await;
}

/// `iam.group_membership_synced` — the event a group-based role rule depends on.
///
/// A `when_group` rule grants its role through `group_members`, so somebody joining a directory
/// group changes their effective permissions **without a sign-in**. Nothing observable records
/// that unless the platform emits something, and the criterion the request names is exactly
/// this event.
///
/// Three claims, pinned separately. Collapsing any two produces a subscriber that looks wired
/// and is not:
///
/// 1. **A group created with members emits it.** The "before" is empty, and a listener that
///    only watched PATCHes would never see the case a rule is most often waiting for.
/// 2. **It fires on a change, not on a write.** A PATCH re-sending the member list it already
///    has changes nothing, and a connector re-sends whole groups on a timer — an event for that
///    trains a subscriber to ignore the name.
/// 3. **The counts are the diff.** A PATCH that adds one and removes one is `added: 1,
///    removed: 1`, not `members: 2`. A subscriber that only wants revocations cannot get them
///    from a size.
#[tokio::test]
async fn a_group_write_emits_its_membership_change() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let owner_token = fixture.owner_token().await;

    let minted = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/provisioning/tokens",
            Some((&owner_token, true)),
            Some(json!({ "name": "Group events" })),
        ),
    )
    .await;
    let secret = minted.body["secret"]
        .as_str()
        .expect("a secret")
        .to_owned();

    // Two accounts, so the diff can be a real diff rather than an empty-to-one transition.
    let mut members = Vec::new();
    for label in ["a", "b"] {
        let email = format!("scim-group-{label}-{}@omnion.test", Uuid::new_v4().simple());
        fixture.remember(&email);
        let created = call(
            &fixture.state,
            request(
                Method::POST,
                "/api/v1/scim/v2/Users",
                Some((&secret, false)),
                Some(json!({
                    "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"],
                    "userName": email,
                })),
            ),
        )
        .await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "create: {}",
            created.body
        );
        members.push(created.body["id"].as_str().expect("id").to_owned());
    }
    let (first, second) = (members[0].clone(), members[1].clone());

    // How many of these events this organization has recorded, so "one more" is a measurement.
    async fn event_count(fixture: &Fixture) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "select count(*) from events where organization_id = $1 \
             and name = 'iam.group_membership_synced'",
        )
        .bind(fixture.organization_id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the event count must read")
    }

    // The newest event for one group, read back through the real table.
    async fn latest_for(fixture: &Fixture, group_id: &str) -> Value {
        sqlx::query_scalar::<_, Value>(
            "select payload from events where organization_id = $1 \
             and name = 'iam.group_membership_synced' and payload ->> 'group_id' = $2 \
             order by created_at desc limit 1",
        )
        .bind(fixture.organization_id)
        .bind(group_id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the event must exist")
    }

    let before = event_count(&fixture).await;

    // ---- 1. A group created WITH a member announces the change -----------------------------
    let name = format!("SCIM Group {}", Uuid::new_v4().simple());
    let group = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/scim/v2/Groups",
            Some((&secret, false)),
            Some(json!({
                "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Group"],
                "displayName": name,
                "members": [{ "value": first }],
            })),
        ),
    )
    .await;
    assert_eq!(group.status, StatusCode::CREATED, "group: {}", group.body);
    let group_id = group.body["id"].as_str().expect("group id").to_owned();

    let created_event = latest_for(&fixture, &group_id).await;
    assert_eq!(
        created_event["added"], json!(1),
        "one person arrived with the group: {created_event}"
    );
    assert_eq!(created_event["removed"], json!(0));
    assert_eq!(created_event["members"], json!(1));

    // ---- 2. A write that changes nothing fires nothing --------------------------------------
    let noop = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/scim/v2/Groups/{group_id}"),
            Some((&secret, false)),
            Some(json!({
                "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
                "Operations": [
                    { "op": "add", "path": "members", "value": [{ "value": first }] }
                ],
            })),
        ),
    )
    .await;
    assert_eq!(noop.status, StatusCode::OK, "noop: {}", noop.body);
    assert_eq!(
        event_count(&fixture).await,
        before + 1,
        "re-sending the member list a group already has is not a change, and a connector does \
         exactly that on a timer — an event for it trains a subscriber to ignore the name"
    );

    // ---- 3. A swap is a diff: one in, one out -----------------------------------------------
    let swap = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/scim/v2/Groups/{group_id}"),
            Some((&secret, false)),
            Some(json!({
                "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
                "Operations": [
                    { "op": "add", "path": "members", "value": [{ "value": second }] },
                    { "op": "remove", "path": "members", "value": [{ "value": first }] }
                ],
            })),
        ),
    )
    .await;
    assert_eq!(swap.status, StatusCode::OK, "swap: {}", swap.body);
    assert_eq!(swap.body["members"].as_array().map(Vec::len), Some(1));

    let swap_event = latest_for(&fixture, &group_id).await;
    assert_eq!(
        swap_event["added"], json!(1),
        "one person arrived: {swap_event}"
    );
    assert_eq!(
        swap_event["removed"], json!(1),
        "and one left — a size would read 1, and a subscriber that only cares about revocations \
         would see nothing"
    );
    assert_eq!(
        swap_event["members"], json!(1),
        "and the group still holds one person, which is the size a snapshot would have carried"
    );
    assert_eq!(event_count(&fixture).await, before + 2, "exactly two real changes");

    // The payload names the group and counts — never the people in it. A member list is the
    // directory's most personal export, and an event is delivered outside the tenant.
    let rendered = swap_event.to_string();
    assert!(
        !rendered.contains(&first) && !rendered.contains(&second),
        "the event must not carry member ids: {rendered}"
    );

    fixture.cleanup().await;
}
