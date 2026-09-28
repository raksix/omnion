//! Integration tests for what a tenant is allowed to use and how much of it
//! (docs/requests/REQ-005, slice 3): settings, module enablement, plan ceilings and the usage
//! the Billing tab renders.
//!
//! The walk proves, over the real router and a live database:
//!
//! * settings save, validate and survive a reload — including the accent colour's live value;
//! * a bad locale, timezone, invite policy, accent or retention is refused *naming the field*,
//!   and one bad field leaves the rest of the form stored;
//! * a module with no decision is ON, switching it off persists, and switching a module the
//!   installation does not ship is refused;
//! * **a ceiling really bounds**: creating a site past `site_limit` is refused with the numbers,
//!   raising the limit makes the same create work, and the seat ceiling refuses an invitation
//!   acceptance — then stops refusing once it is raised;
//! * lowering a ceiling below what is already used is refused, and leaves the stored plan alone;
//! * the usage CSV repeats the same figures the tab renders, not a different reading;
//! * another tenant's settings, modules, limits and usage are `404`, never `403`.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::memberships;
use omnion_identity::tenancy_limits;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::{roles as role_store, seed};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// What the administrator of this suite holds.
const ADMIN_PERMISSIONS: [&str; 4] = [
    "organizations.read",
    "organizations.manage",
    "sites.read",
    "sites.create",
];

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    headers: Vec<(String, String)>,
    body: Value,
    /// The raw bytes, for the CSV download.
    raw: Vec<u8>,
}

impl TestResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.raw).into_owned()
    }
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
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();

    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let raw = bytes.to_vec();
    let body = if raw.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&raw).unwrap_or(Value::Null)
    };

    TestResponse {
        status,
        set_cookie,
        headers,
        body,
        raw,
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
///
/// The database is this writer's own (`omnion_w5_tenant_test`): the shared dev `omnion`
/// database carries another branch's migration 19, and `db.migrate()` refuses to run with
/// `VersionMissing(19)` — which fails every migrating test identically, for a reason that has
/// nothing to do with the code under test.
async fn live_state() -> Option<(AppState, Db)> {
    let mut config = Config::from_env().expect("environment must be valid");
    let _ = live_db(&config).await?;
    ensure_test_database().await?;

    // The state is pointed at the suite's own database by rewriting the config field rather
    // than the process environment: `std::env::set_var` is `unsafe` in this edition, and — more
    // to the point — a test that mutates the environment changes what every *other* test in the
    // same binary connects to.
    config.database.url = test_database_url();

    let db = Db::connect(&config.database)
        .await
        .expect("the suite database must connect");
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

/// The database this suite migrates and writes to.
///
/// The port is the compose one (`OMNION_POSTGRES_PORT` defaults to 5433), not the container's
/// internal 5432 — connecting to 5432 reaches something else on the host or nothing at all.
const TEST_DATABASE_NAME: &str = "omnion_w5_tenant_test";
const TEST_ADMIN_URL: &str = "postgres://omnion:omnion@127.0.0.1:5433/omnion";

/// The suite's own connection string, built from the same defaults the compose file uses.
fn test_database_url() -> String {
    format!(
        "postgres://omnion:omnion@127.0.0.1:{}/{}",
        std::env::var("OMNION_POSTGRES_PORT").unwrap_or_else(|_| "5433".to_owned()),
        TEST_DATABASE_NAME
    )
}

/// Create the suite database if it is not there, ignoring "it already exists".
///
/// Twelve `#[tokio::test]`s in one binary run in parallel threads and all call this at once,
/// so "check, then create" is a race: five of them saw the database missing and all five
/// tried to create it. The fix is to tolerate the conflict rather than to serialise the
/// suite — a duplicate-key error here means somebody else did the work this call was about to
/// do, which is exactly the state the caller wanted.
async fn ensure_test_database() -> Option<()> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(TEST_ADMIN_URL)
        .await
        .ok()?;

    if let Err(err) = sqlx::query(&format!("create database {TEST_DATABASE_NAME}"))
        .execute(&pool)
        .await
    {
        // 42P04 is `duplicate_database`; 23505 with the `pg_database_datname_index` constraint
        // is the same thing arriving as a unique violation. Anything else is a real failure
        // and has to surface.
        let benign = err
            .as_database_error()
            .map(|db| {
                db.code().as_deref() == Some("42P04")
                    || (db.code().as_deref() == Some("23505")
                        && db.constraint() == Some("pg_database_datname_index"))
            })
            .unwrap_or(false);
        if !benign {
            panic!("the suite database must be creatable: {err}");
        }
    }

    pool.close().await;
    Some(())
}

/// Two organizations, each with an administrator, so the cross-tenant walks have a reader.
struct Fixture {
    state: AppState,
    db: Db,
    org_a: Uuid,
    /// Organization A's administrator — the account every walk signs in as.
    admin_email: String,
    /// The second organization's administrator — the cross-tenant reader.
    other_admin_email: String,
    accounts: Vec<Uuid>,
    organizations: Vec<Uuid>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let org_a = create_organization_row(&db, "a", "Tenant Limits A").await;
        let org_b = create_organization_row(&db, "b", "Tenant Limits B").await;

        let (admin_id, admin_email) = create_account(&db).await;
        add_membership(&db, org_a, admin_id).await;
        grant_organization_admin(&db, org_a, admin_id).await;

        // A second account of organization A, so the seat ceiling has two members to count
        // and the "lowering a ceiling below what is used" walk has something to sit under.
        let (other_id, _other_email) = create_account(&db).await;
        add_membership(&db, org_a, other_id).await;

        let (b_admin_id, other_admin_email) = create_account(&db).await;
        add_membership(&db, org_b, b_admin_id).await;
        grant_organization_admin(&db, org_b, b_admin_id).await;

        Some(Self {
            state,
            db,
            org_a,
            admin_email,
            other_admin_email,
            accounts: vec![admin_id, other_id, b_admin_id],
            organizations: vec![org_a, org_b],
        })
    }

    async fn admin_token(&self) -> String {
        login(&self.state, &self.admin_email).await
    }

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
    let slug = format!("tenant-fix-{label}-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(name)
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// Create an account with a unique address and no membership.
async fn create_account(db: &Db) -> (Uuid, String) {
    let email = format!("tenant-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Tenant Test".to_owned(),
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

/// The `code` of an error body.
fn code_of(body: &Value) -> String {
    body["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("body carries an error code: {body}"))
        .to_owned()
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn settings_save_and_survive_a_reload() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;
    let uri = format!("/api/v1/organizations/{}/settings", fixture.org_a);

    // The first read is the backfilled default row, not an empty form.
    let initial = call(
        &fixture.state,
        request(Method::GET, &uri, Some(&admin), None),
    )
    .await;
    assert_eq!(initial.status, StatusCode::OK, "body: {}", initial.body);
    assert_eq!(initial.body["settings"]["locale"], "en");
    assert_eq!(initial.body["settings"]["invite_policy"], "owner_approval");
    assert_eq!(initial.body["settings"]["audit_retention_days"], 365);
    assert!(
        initial.body["available_locales"].is_array(),
        "the form offers its locales instead of accepting any string"
    );
    assert_eq!(
        initial.body["invite_policies"]
            .as_array()
            .expect("policies")
            .len(),
        3,
        "the REQ names exactly three policies"
    );

    // Save, then read again in a separate request: a value that only existed in the response
    // of the write is not saved.
    let saved = call(
        &fixture.state,
        request(
            Method::PUT,
            &uri,
            Some(&admin),
            Some(json!({
                "locale": "tr",
                "timezone": "Europe/Istanbul",
                "invite_policy": "self_serve",
                "default_invite_role_id": null,
                "logo_media_id": null,
                "accent_color": "#2F6F4F",
                "audit_retention_days": 180,
            })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "body: {}", saved.body);
    assert_eq!(saved.body["settings"]["accent_color"], "#2f6f4f");

    let reloaded = call(
        &fixture.state,
        request(Method::GET, &uri, Some(&admin), None),
    )
    .await;
    assert_eq!(reloaded.status, StatusCode::OK);
    assert_eq!(reloaded.body["settings"]["locale"], "tr");
    assert_eq!(reloaded.body["settings"]["timezone"], "Europe/Istanbul");
    assert_eq!(reloaded.body["settings"]["invite_policy"], "self_serve");
    assert_eq!(reloaded.body["settings"]["audit_retention_days"], 180);
    assert_eq!(
        reloaded.body["settings"]["accent_color"], "#2f6f4f",
        "the picker sends uppercase; what is stored compares equal anyway"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_refused_settings_field_names_itself_and_changes_nothing() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;
    let uri = format!("/api/v1/organizations/{}/settings", fixture.org_a);

    // Start from a known state so "nothing changed" is provable.
    call(
        &fixture.state,
        request(
            Method::PUT,
            &uri,
            Some(&admin),
            Some(json!({
                "locale": "en",
                "timezone": "UTC",
                "invite_policy": "owner_approval",
                "default_invite_role_id": null,
                "logo_media_id": null,
                "accent_color": null,
                "audit_retention_days": 365,
            })),
        ),
    )
    .await;

    // A good locale with a bad retention: the whole request has to be refused, because a
    // form that half-saved leaves the reader unable to say which half is stored.
    let refused = call(
        &fixture.state,
        request(
            Method::PUT,
            &uri,
            Some(&admin),
            Some(json!({
                "locale": "de",
                "timezone": "UTC",
                "invite_policy": "owner_approval",
                "default_invite_role_id": null,
                "logo_media_id": null,
                "accent_color": null,
                "audit_retention_days": 3,
            })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        refused.body
    );
    assert_eq!(code_of(&refused.body), "invalid_organization_settings");
    assert!(
        refused.body["error"]["message"]
            .as_str()
            .expect("a message")
            .contains("30"),
        "the refusal names the range: {}",
        refused.body["error"]["message"]
    );

    let after = call(
        &fixture.state,
        request(Method::GET, &uri, Some(&admin), None),
    )
    .await;
    assert_eq!(
        after.body["settings"]["locale"], "en",
        "the good half of a refused request must not be stored either"
    );
    assert_eq!(after.body["settings"]["audit_retention_days"], 365);

    // An unknown invite policy is refused the same way.
    let policy = call(
        &fixture.state,
        request(
            Method::PUT,
            &uri,
            Some(&admin),
            Some(json!({
                "locale": "en",
                "timezone": "UTC",
                "invite_policy": "whenever",
                "default_invite_role_id": null,
                "logo_media_id": null,
                "accent_color": null,
                "audit_retention_days": 365,
            })),
        ),
    )
    .await;
    assert_eq!(policy.status, StatusCode::BAD_REQUEST);

    // And so is a colour that is not a hex colour.
    let accent = call(
        &fixture.state,
        request(
            Method::PUT,
            &uri,
            Some(&admin),
            Some(json!({
                "locale": "en",
                "timezone": "UTC",
                "invite_policy": "closed",
                "default_invite_role_id": null,
                "logo_media_id": null,
                "accent_color": "rebeccapurple",
                "audit_retention_days": 365,
            })),
        ),
    )
    .await;
    assert_eq!(
        accent.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        accent.body
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_module_with_no_decision_is_on_and_the_toggle_persists() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;
    let uri = format!("/api/v1/organizations/{}/modules", fixture.org_a);

    // The backfill writes no module rows, so every module reads as on *without a decision*.
    let initial = call(
        &fixture.state,
        request(Method::GET, &uri, Some(&admin), None),
    )
    .await;
    assert_eq!(initial.status, StatusCode::OK, "body: {}", initial.body);
    let modules = initial.body["modules"]
        .as_array()
        .expect("modules array")
        .clone();
    assert!(!modules.is_empty(), "the installation ships modules");
    assert!(
        modules.iter().all(|module| module["enabled"] == true),
        "a module nobody switched off is on: {modules:?}"
    );
    assert!(
        modules.iter().all(|module| module["explicit"] == false),
        "and it says nobody decided: {modules:?}"
    );
    let target = modules[0]["key"].as_str().expect("a key").to_owned();

    // Switch it off.
    let off = call(
        &fixture.state,
        request(
            Method::PUT,
            &uri,
            Some(&admin),
            Some(json!({ "modules": [{ "module_key": target, "enabled": false }] })),
        ),
    )
    .await;
    assert_eq!(off.status, StatusCode::OK, "body: {}", off.body);
    let switched = off.body["modules"]
        .as_array()
        .expect("modules")
        .iter()
        .find(|module| module["key"] == target.as_str())
        .expect("the switched module is still listed")
        .clone();
    assert_eq!(switched["enabled"], false);
    assert_eq!(
        switched["explicit"], true,
        "an explicit switch is what separates 'chosen' from 'on by default'"
    );

    // A separate read proves it persisted rather than being echoed back.
    let reloaded = call(
        &fixture.state,
        request(Method::GET, &uri, Some(&admin), None),
    )
    .await;
    let stored = reloaded.body["modules"]
        .as_array()
        .expect("modules")
        .iter()
        .find(|module| module["key"] == target.as_str())
        .expect("the module is listed")
        .clone();
    assert_eq!(stored["enabled"], false);

    // A module the installation does not ship has no switch, so writing one is refused.
    let unknown = call(
        &fixture.state,
        request(
            Method::PUT,
            &uri,
            Some(&admin),
            Some(json!({ "modules": [{ "module_key": "quantum-hub", "enabled": false }] })),
        ),
    )
    .await;
    assert_eq!(
        unknown.status,
        StatusCode::NOT_FOUND,
        "body: {}",
        unknown.body
    );
    assert_eq!(code_of(&unknown.body), "module_not_installed");

    // Switching it back on restores it.
    let on = call(
        &fixture.state,
        request(
            Method::PUT,
            &uri,
            Some(&admin),
            Some(json!({ "modules": [{ "module_key": target, "enabled": true }] })),
        ),
    )
    .await;
    assert_eq!(on.status, StatusCode::OK);
    let restored = on.body["modules"]
        .as_array()
        .expect("modules")
        .iter()
        .find(|module| module["key"] == target.as_str())
        .expect("the module is listed")
        .clone();
    assert_eq!(restored["enabled"], true);

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_module_the_installation_does_not_ship_leaves_the_others_alone() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;
    let uri = format!("/api/v1/organizations/{}/modules", fixture.org_a);

    // The whole set is validated before any of it is written, so this request has to leave no
    // trace — not even the legitimate first switch it carried.
    let refused = call(
        &fixture.state,
        request(
            Method::PUT,
            &uri,
            Some(&admin),
            Some(json!({
                "modules": [
                    { "module_key": "media", "enabled": false },
                    { "module_key": "not-a-module", "enabled": false },
                ],
            })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::NOT_FOUND,
        "body: {}",
        refused.body
    );

    let after = call(
        &fixture.state,
        request(Method::GET, &uri, Some(&admin), None),
    )
    .await;
    let media = after.body["modules"]
        .as_array()
        .expect("modules")
        .iter()
        .find(|module| module["key"] == "media")
        .expect("media is installed")
        .clone();
    assert_eq!(
        media["enabled"], true,
        "a refused batch must not apply the switch it did carry"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_ceiling_really_bounds_creating_a_site() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;
    let limits_uri = format!("/api/v1/organizations/{}/limits", fixture.org_a);
    let sites_uri = "/api/v1/sites";

    // One site fits.
    call(
        &fixture.state,
        request(
            Method::PUT,
            &limits_uri,
            Some(&admin),
            Some(json!({
                "plan": "standard",
                "seat_limit": null,
                "site_limit": 1,
                "storage_bytes_limit": null,
                "ai_monthly_limit_micros": null,
            })),
        ),
    )
    .await;

    let first = call(
        &fixture.state,
        request(
            Method::POST,
            sites_uri,
            Some(&admin),
            Some(json!({ "key": "first", "name": "First site" })),
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::CREATED, "body: {}", first.body);

    // The second is refused, and the refusal names the ceiling.
    let second = call(
        &fixture.state,
        request(
            Method::POST,
            sites_uri,
            Some(&admin),
            Some(json!({ "key": "second", "name": "Second site" })),
        ),
    )
    .await;
    assert_eq!(
        second.status,
        StatusCode::FORBIDDEN,
        "body: {}",
        second.body
    );
    assert_eq!(code_of(&second.body), "organization.limit.reached");
    let details = &second.body["error"]["details"];
    assert_eq!(details["resource"], "sites");
    assert_eq!(details["limit"], 1);
    assert!(
        second.body["error"]["message"]
            .as_str()
            .expect("a message")
            .contains("1"),
        "the message names the ceiling: {}",
        second.body["error"]["message"]
    );

    // Raising the limit makes the *same* create work — the proof that the refusal came from
    // the stored plan and not from something else about the second call.
    call(
        &fixture.state,
        request(
            Method::PUT,
            &limits_uri,
            Some(&admin),
            Some(json!({
                "plan": "business",
                "seat_limit": null,
                "site_limit": 3,
                "storage_bytes_limit": null,
                "ai_monthly_limit_micros": null,
            })),
        ),
    )
    .await;
    let retried = call(
        &fixture.state,
        request(
            Method::POST,
            sites_uri,
            Some(&admin),
            Some(json!({ "key": "second", "name": "Second site" })),
        ),
    )
    .await;
    assert_eq!(
        retried.status,
        StatusCode::CREATED,
        "raising the ceiling must let the same create through: {}",
        retried.body
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_ceiling_really_bounds_accepting_an_invitation() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;
    let limits_uri = format!("/api/v1/organizations/{}/limits", fixture.org_a);
    let invitations_uri = format!("/api/v1/organizations/{}/invitations", fixture.org_a);

    // The fixture's organization already holds two active members, so a ceiling of two is
    // exactly full.
    call(
        &fixture.state,
        request(
            Method::PUT,
            &limits_uri,
            Some(&admin),
            Some(json!({
                "plan": "standard",
                "seat_limit": 2,
                "site_limit": null,
                "storage_bytes_limit": null,
                "ai_monthly_limit_micros": null,
            })),
        ),
    )
    .await;

    // A unique address per run: a leftover row from a run that was interrupted before its
    // cleanup would otherwise make this walk's account-count assertion describe the previous
    // run rather than this one.
    let invitee = format!("invitee-{}@omnion.test", Uuid::new_v4().simple());

    // Inviting a third address is NOT refused: the REQ is explicit that the plan is charged
    // for people who have joined, and an invitation is a request.
    let invited = call(
        &fixture.state,
        request(
            Method::POST,
            &invitations_uri,
            Some(&admin),
            Some(json!({ "email": invitee.clone() })),
        ),
    )
    .await;
    assert_eq!(
        invited.status,
        StatusCode::CREATED,
        "inviting is not what the ceiling refuses: {}",
        invited.body
    );
    let token = invited.body["token"]
        .as_str()
        .expect("the raw token is returned once, at creation")
        .to_owned();

    // Accepting it is.
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/invitations/{token}/accept"),
            None,
            Some(json!({
                "display_name": "Invitee",
                "password": "correct horse battery",
            })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "body: {}",
        refused.body
    );
    assert_eq!(code_of(&refused.body), "organization.limit.reached");
    assert_eq!(refused.body["error"]["details"]["resource"], "seats");
    assert_eq!(refused.body["error"]["details"]["limit"], 2);

    // And the refusal left no account behind: a sign-up that was refused must not have created
    // a user that holds no membership and cannot get in.
    let stranded: i64 = sqlx::query_scalar("select count(*) from users where email = $1")
        .bind(&invitee)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the account count must read");
    assert_eq!(
        stranded, 0,
        "a refused acceptance must not strand a brand new account"
    );

    // Raising the seat ceiling makes the same acceptance work.
    call(
        &fixture.state,
        request(
            Method::PUT,
            &limits_uri,
            Some(&admin),
            Some(json!({
                "plan": "business",
                "seat_limit": 10,
                "site_limit": null,
                "storage_bytes_limit": null,
                "ai_monthly_limit_micros": null,
            })),
        ),
    )
    .await;
    let accepted = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/invitations/{token}/accept"),
            None,
            Some(json!({
                "display_name": "Invitee",
                "password": "correct horse battery",
            })),
        ),
    )
    .await;
    assert_eq!(
        accepted.status,
        StatusCode::OK,
        "raising the ceiling must let the same acceptance through: {}",
        accepted.body
    );
    assert_eq!(
        accepted.body["organization_id"],
        fixture.org_a.to_string(),
        "and it lands in the organization the plan belongs to"
    );

    // The sign-up the second attempt created is a real account, and this suite has to
    // remove it.
    let created: Vec<Uuid> = sqlx::query_scalar("select id from users where email = $1")
        .bind(&invitee)
        .fetch_all(fixture.db.pool())
        .await
        .expect("the account must exist");
    sqlx::query("delete from users where id = any($1)")
        .bind(&created)
        .execute(fixture.db.pool())
        .await
        .expect("the invited account must be removable");

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_ceiling_cannot_be_lowered_below_what_is_already_used() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;
    let limits_uri = format!("/api/v1/organizations/{}/limits", fixture.org_a);
    let usage_uri = format!("/api/v1/organizations/{}/usage", fixture.org_a);

    // The organization holds two members; a plan of one seat is not a plan anybody can read.
    let refused = call(
        &fixture.state,
        request(
            Method::PUT,
            &limits_uri,
            Some(&admin),
            Some(json!({
                "plan": "standard",
                "seat_limit": 1,
                "site_limit": null,
                "storage_bytes_limit": null,
                "ai_monthly_limit_micros": null,
            })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "body: {}",
        refused.body
    );
    assert_eq!(code_of(&refused.body), "organization.limit.reached");
    assert_eq!(refused.body["error"]["details"]["resource"], "seats");

    // The row was restored, so the stored plan still matches the members tab.
    let after = call(
        &fixture.state,
        request(Method::GET, &usage_uri, Some(&admin), None),
    )
    .await;
    assert_eq!(after.status, StatusCode::OK);
    assert_eq!(
        after.body["seats_used"], 2,
        "two accounts were created by the fixture"
    );
    assert!(
        after.body["limits"]["seat_limit"].is_null(),
        "a refused ceiling is not stored: {}",
        after.body["limits"]
    );

    // A ceiling above what is used is accepted.
    let accepted = call(
        &fixture.state,
        request(
            Method::PUT,
            &limits_uri,
            Some(&admin),
            Some(json!({
                "plan": "business",
                "seat_limit": 20,
                "site_limit": null,
                "storage_bytes_limit": null,
                "ai_monthly_limit_micros": null,
            })),
        ),
    )
    .await;
    assert_eq!(accepted.status, StatusCode::OK, "body: {}", accepted.body);
    assert_eq!(accepted.body["limits"]["seat_limit"], 20);

    // A limit of zero is not "unlimited" and not a ceiling: it is refused.
    let zero = call(
        &fixture.state,
        request(
            Method::PUT,
            &limits_uri,
            Some(&admin),
            Some(json!({
                "plan": "business",
                "seat_limit": 0,
                "site_limit": null,
                "storage_bytes_limit": null,
                "ai_monthly_limit_micros": null,
            })),
        ),
    )
    .await;
    assert_eq!(zero.status, StatusCode::BAD_REQUEST, "body: {}", zero.body);
    assert_eq!(code_of(&zero.body), "invalid_organization_limits");

    // An unknown plan is refused too.
    let plan = call(
        &fixture.state,
        request(
            Method::PUT,
            &limits_uri,
            Some(&admin),
            Some(json!({
                "plan": "platinum",
                "seat_limit": null,
                "site_limit": null,
                "storage_bytes_limit": null,
                "ai_monthly_limit_micros": null,
            })),
        ),
    )
    .await;
    assert_eq!(plan.status, StatusCode::BAD_REQUEST, "body: {}", plan.body);

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_usage_csv_repeats_the_numbers_the_tab_renders() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;
    let usage_uri = format!("/api/v1/organizations/{}/usage", fixture.org_a);
    let limits_uri = format!("/api/v1/organizations/{}/limits", fixture.org_a);

    call(
        &fixture.state,
        request(
            Method::PUT,
            &limits_uri,
            Some(&admin),
            Some(json!({
                "plan": "business",
                "seat_limit": 25,
                "site_limit": 7,
                "storage_bytes_limit": 5_000_000,
                "ai_monthly_limit_micros": 900_000,
            })),
        ),
    )
    .await;

    let json = call(
        &fixture.state,
        request(Method::GET, &usage_uri, Some(&admin), None),
    )
    .await;
    assert_eq!(json.status, StatusCode::OK, "body: {}", json.body);

    let csv = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("{usage_uri}?format=csv"),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(csv.status, StatusCode::OK);
    assert!(
        csv.header("content-type")
            .expect("a content type")
            .starts_with("text/csv"),
        "the download is a spreadsheet: {:?}",
        csv.header("content-type")
    );
    assert!(
        csv.header("content-disposition")
            .expect("a disposition")
            .contains("attachment"),
        "and it downloads rather than rendering"
    );

    let text = csv.text();
    for metric in ["seats", "sites", "storage_bytes", "ai_micros"] {
        assert!(
            text.contains(metric),
            "the CSV names every metric it reports: {text}"
        );
    }

    // The figures the tab shows and the figures in the file are the same reading, not two
    // queries a moment apart.
    for (metric, field) in [
        ("seats", "seats_used"),
        ("sites", "sites_used"),
        ("storage_bytes", "storage_used_bytes"),
        ("ai_micros", "ai_micros_this_month"),
    ] {
        let on_screen = json.body[field].as_i64().unwrap_or_default();
        let row = text
            .lines()
            .find(|line| line.starts_with(&format!("{metric},")))
            .unwrap_or_else(|| panic!("the CSV carries a {metric} row: {text}"));
        let used: i64 = row
            .split(',')
            .nth(1)
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("{metric} row carries a used figure: {row}"));
        assert_eq!(
            used, on_screen,
            "{metric} differs between the screen and the file"
        );
    }

    // Each row repeats the plan, so a spreadsheet that gets forwarded still says what the
    // number is measured against.
    assert!(
        text.contains("business"),
        "every row names the plan: {text}"
    );

    // And the bars name their limit source.
    assert!(
        json.body["sources"]["seats"]
            .as_str()
            .expect("a source")
            .contains("25"),
        "the seat bar names its ceiling: {}",
        json.body["sources"]["seats"]
    );
    assert!(
        json.body["sources"]["ai_monthly_micros"]
            .as_str()
            .expect("a source")
            .contains("this month"),
        "the AI bar names its window: {}",
        json.body["sources"]["ai_monthly_micros"]
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn an_unlimited_ceiling_reads_as_unlimited_everywhere() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;
    let usage_uri = format!("/api/v1/organizations/{}/usage", fixture.org_a);

    let usage = call(
        &fixture.state,
        request(Method::GET, &usage_uri, Some(&admin), None),
    )
    .await;
    assert_eq!(usage.status, StatusCode::OK, "body: {}", usage.body);
    assert_eq!(usage.body["limits"]["plan"], "standard");
    assert!(
        usage.body["limits"]["site_limit"].is_null(),
        "a fresh tenant has no ceiling on sites"
    );
    assert!(
        usage.body["sources"]["sites"]
            .as_str()
            .expect("a source")
            .contains("unlimited"),
        "a null ceiling reads as a word, not a zero: {}",
        usage.body["sources"]["sites"]
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn another_tenants_settings_and_limits_are_a_404() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let other_admin = fixture.other_admin_token().await;

    // Organization B's administrator asks for organization A's tenant surface. Every route
    // answers `404`, never `403` — a 403 would confirm the id is real.
    for suffix in ["settings", "modules", "limits", "usage"] {
        let response = call(
            &fixture.state,
            request(
                Method::GET,
                &format!("/api/v1/organizations/{}/{suffix}", fixture.org_a),
                Some(&other_admin),
                None,
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{suffix} of another tenant must be a 404, body: {}",
            response.body
        );
        assert_eq!(code_of(&response.body), "organization_not_found");
    }

    // Writing to it is refused the same way.
    let write = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/organizations/{}/limits", fixture.org_a),
            Some(&other_admin),
            Some(json!({
                "plan": "enterprise",
                "seat_limit": null,
                "site_limit": null,
                "storage_bytes_limit": null,
                "ai_monthly_limit_micros": null,
            })),
        ),
    )
    .await;
    assert_eq!(write.status, StatusCode::NOT_FOUND, "body: {}", write.body);

    // And the real thing is untouched.
    let mine = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/organizations/{}/limits", fixture.org_a),
            Some(&fixture.admin_token().await),
            None,
        ),
    )
    .await;
    assert_eq!(mine.body["limits"]["plan"], "standard");

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_backfill_gives_every_organization_a_settings_and_a_limits_row() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let pool = fixture.db.pool();

    // Touch the tenant surface first. The backfill is a one-shot insert that ran when the
    // migration was applied, and this fixture's organizations were created *after* that — so
    // the read path is what has to produce their rows, and the assertion below is only
    // meaningful once that has happened.
    let admin = fixture.admin_token().await;
    for suffix in ["settings", "limits"] {
        let read = call(
            &fixture.state,
            request(
                Method::GET,
                &format!("/api/v1/organizations/{}/{suffix}", fixture.org_a),
                Some(&admin),
                None,
            ),
        )
        .await;
        assert_eq!(read.status, StatusCode::OK, "body: {}", read.body);
    }

    // Every organization that has been *touched* carries both rows. Organizations created
    // after the migration are legitimately row-less until they are read, and a test that
    // demanded a row for those would be testing a trigger the design deliberately does not
    // have.
    let missing_settings: Vec<Uuid> = sqlx::query_scalar(
        "select s.organization_id from organization_settings s \
          where not exists (select 1 from organizations o where o.id = s.organization_id)",
    )
    .fetch_all(pool)
    .await
    .expect("the settings rows must read");
    assert!(
        missing_settings.is_empty(),
        "a settings row always names a live organization; orphans: {missing_settings:?}"
    );

    let orphan_limits: Vec<Uuid> = sqlx::query_scalar(
        "select l.organization_id from organization_limits l \
          where not exists (select 1 from organizations o where o.id = l.organization_id)",
    )
    .fetch_all(pool)
    .await
    .expect("the limits rows must read");
    assert!(
        orphan_limits.is_empty(),
        "a limits row always names a live organization; orphans: {orphan_limits:?}"
    );

    // The fixture's own organization has exactly one settings row and one limits row — not
    // two, which a read path that upserted on every call would produce.
    let settings_rows: i64 =
        sqlx::query_scalar("select count(*) from organization_settings where organization_id = $1")
            .bind(fixture.org_a)
            .fetch_one(pool)
            .await
            .expect("the settings row count must read");
    assert_eq!(
        settings_rows, 1,
        "one row per organization, however often it is read"
    );

    let limits_rows: i64 =
        sqlx::query_scalar("select count(*) from organization_limits where organization_id = $1")
            .bind(fixture.org_a)
            .fetch_one(pool)
            .await
            .expect("the limits row count must read");
    assert_eq!(
        limits_rows, 1,
        "one row per organization, however often it is read"
    );

    // Reading twice must not have changed anything a second time — the read path upserts the
    // defaults, so a hand-edited value has to survive it.
    call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/organizations/{}/limits", fixture.org_a),
            Some(&admin),
            Some(json!({
                "plan": "business",
                "seat_limit": 42,
                "site_limit": null,
                "storage_bytes_limit": null,
                "ai_monthly_limit_micros": null,
            })),
        ),
    )
    .await;
    call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/organizations/{}/limits", fixture.org_a),
            Some(&admin),
            None,
        ),
    )
    .await;
    let survived = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/organizations/{}/limits", fixture.org_a),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(
        survived.body["limits"]["seat_limit"], 42,
        "the read path must not reset a ceiling it did not write"
    );

    // And every user with a home organization carries exactly one primary membership — the
    // slice-1 half of the same acceptance criterion, re-proved here so one walk owns the
    // whole line.
    let wrong_primary: Vec<(Uuid, i64)> = sqlx::query_as(
        "select u.id, count(m.id) from users u \
           left join organization_members m on m.user_id = u.id and m.is_primary \
          where u.organization_id is not null \
          group by u.id having count(m.id) <> 1",
    )
    .fetch_all(pool)
    .await
    .expect("the primary membership count must read");
    assert!(
        wrong_primary.is_empty(),
        "every account with a home organization has exactly one primary membership; \
         wrong: {wrong_primary:?}"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_installed_module_keys_all_survive_the_store() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let pool = fixture.db.pool();

    // Every key the installation offers has to pass the shape the column's check enforces, or
    // the Modules tab would offer a switch that the store refuses to write.
    for module in tenancy_limits::installed_modules() {
        tenancy_limits::set_module_enabled(pool, fixture.org_a, module.key, false)
            .await
            .unwrap_or_else(|err| panic!("{} must be writable: {err}", module.key));
    }

    let stored: Vec<(String, bool)> = sqlx::query_as(
        "select module_key, enabled from organization_modules where organization_id = $1",
    )
    .bind(fixture.org_a)
    .fetch_all(pool)
    .await
    .expect("the module rows must read");
    assert_eq!(
        stored.len(),
        tenancy_limits::installed_modules().len(),
        "every installed module has a row after switching each one"
    );
    assert!(
        stored.iter().all(|(_, enabled)| !enabled),
        "and all of them are off: {stored:?}"
    );

    fixture.cleanup().await;
}
