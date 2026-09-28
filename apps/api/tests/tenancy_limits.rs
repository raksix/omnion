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
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// What the administrator of this suite holds.
///
/// `sites.update`, `sites.delete` and `domains.manage` are here because the suspend/archive rule
/// has to be proven on the *site* surface as well as the tenancy one, and a walk that reaches a
/// 403 for "you do not hold this permission" has proved nothing about the freeze — the guard
/// never ran. Every permission the walks touch belongs in this list for that reason: a refusal
/// from the wrong layer is a refusal that passes for the right one. I found that twice in one
/// tick (`sites.update`, then `domains.manage`), each time on the assertion *after* the one
/// that had been waiting silently.
const ADMIN_PERMISSIONS: [&str; 7] = [
    "organizations.read",
    "organizations.manage",
    "sites.read",
    "sites.create",
    "sites.update",
    "sites.delete",
    "domains.manage",
];

/// The powers the module-switch walk needs *on top* of [`ADMIN_PERMISSIONS`].
///
/// A separate list, and a separate grant, on purpose. The module guard refuses with
/// `organization.module.disabled` only *after* the permission guard has said yes — so an
/// administrator without `media.read` would answer `403 permission_denied` on the media route
/// and the walk would "prove" the module switch works while proving the permission instead. The
/// two refusals are indistinguishable from the outside, which is the whole reason this list
/// exists.
const MODULE_PERMISSIONS: [&str; 4] = [
    "media.read",
    "analytics.read",
    "webhooks.read",
    "ai.providers.read",
];

/// The module-switch walk's *core* surfaces, which must keep answering while a module is off.
///
/// `content.pages.read` is here for the same reason [`MODULE_PERMISSIONS`] exists: the core
/// assertion is "the core is unaffected", and a `403 permission_denied` from a fixture that
/// never held the permission looks exactly like a module refusal until the code is read.
const CORE_PERMISSIONS: [&str; 1] = ["content.pages.read"];

/// What the Audit tab needs on top of that, and what a tenancy administrator deliberately does
/// not get for free: a trail names every privileged act in the tenant, so `audit.read` is its own
/// permission rather than a consequence of `organizations.read`.
const AUDIT_PERMISSIONS: [&str; 1] = ["audit.read"];

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

    /// The *second* organization — the cross-tenant reader's tenant.
    ///
    /// A method rather than a second field: the fixture already keeps both ids in
    /// `organizations`, and a duplicate field is a second source of truth that can drift from
    /// the list the cleanup deletes by.
    fn org_b(&self) -> Uuid {
        self.organizations[1]
    }

    /// Remember an account a walk created *after* the fixture was built, so cleanup reaches it.
    ///
    /// The walks add owners mid-test; without this, an owner account survives the suite and the
    /// next run's "the queue must be empty" counts their leftover rows.
    async fn track_account(&mut self, user_id: Uuid) {
        self.accounts.push(user_id);
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
    grant_permissions(db, organization_id, user_id, &ADMIN_PERMISSIONS).await;
}

/// Grant one account an exact set of permissions inside one organization.
///
/// Parameterised rather than a second copy of `grant_organization_admin`: two copies of a
/// fixture that *drifts* produce a walk that passes for the wrong reason — one of them quietly
/// holding a permission the other does not, and the difference never showing up as a failure.
async fn grant_permissions(db: &Db, organization_id: Uuid, user_id: Uuid, keys: &[&str]) {
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

    let entries: Vec<omnion_permissions::model::RolePermissionInput> = keys
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

/// Bind the platform `owner` role to one account, at this organization's scope.
///
/// The queue's release rule is a *per-tenant* fact, so a walk that only ever binds the suite's
/// "organization administrator" can never prove it. This is the shape the first-run owner has:
/// the seeded `owner` role lives at platform scope, and the binding is what makes the account
/// this tenant's owner.
async fn grant_organization_owner(db: &Db, organization_id: Uuid, user_id: Uuid) {
    let role_id: Uuid = sqlx::query_scalar(
        "select id from roles where key = 'owner' and organization_id is null limit 1",
    )
    .fetch_one(db.pool())
    .await
    .expect("the seeded owner role must exist");

    omnion_permissions::bindings::grant(
        db.pool(),
        omnion_permissions::model::NewBinding {
            role_id,
            user_id,
            scope: omnion_permissions::Scope::Organization { organization_id },
            granted_by: None,
            expires_at: None,
        },
    )
    .await
    .expect("the owner binding must be granted");
}

/// Set one organization's invite policy, straight through the store.
///
/// Only the policy is sent: `SettingsChanges` is a *patch* shape where `None` means "leave it
/// alone", so copying the rest of the row across would be a second thing to get wrong — and
/// `Some(None)` on a nullable field is not "unchanged", it is "clear it".
async fn set_invite_policy(db: &Db, organization_id: Uuid, policy: &str) {
    tenancy_limits::update_settings(
        db.pool(),
        organization_id,
        tenancy_limits::SettingsChanges {
            invite_policy: Some(policy.to_owned()),
            ..Default::default()
        },
    )
    .await
    .expect("the invite policy must be saved");
}

/// Invite somebody, returning the response as-is.
///
/// Several walks assert *how* a create ended — `201` with a link, `202` queued, `403` closed —
/// and a helper that panicked on the wrong status would hide which one it was.
async fn invite(
    fixture: &Fixture,
    token: &str,
    email: &str,
) -> TestResponse {
    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/organizations/{}/invitations", fixture.org_a),
            Some(token),
            Some(json!({ "email": email })),
        ),
    )
    .await
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

/// The `id` of a response body, as the string the API answered with.
///
/// A helper rather than `as_str().unwrap()` at every call site, and it returns the *string* on
/// purpose: a uuid has to be parsed back out of the JSON to be compared, and doing that in the
/// helper means every assertion reads `body["id"] == id_of(body)`. The parse failure a test
/// *should* see is "the response has no id", and a panic that says so is worth more than an
/// `unwrap()` forty lines from the assertion that cares.
fn id_of(body: &Value) -> String {
    body["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the body carries an id: {body}"))
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

// ---------------------------------------------------------------------------------------------
// The invite policy: three stored values, three behaviours
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_closed_organization_refuses_an_invitation_and_names_its_policy() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    set_invite_policy(&fixture.db, fixture.org_a, "closed").await;
    let admin = fixture.admin_token().await;
    let address = format!("closed-{}@omnion.test", Uuid::new_v4().simple());

    let refused = invite(&fixture, &admin, &address).await;

    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a closed tenant must refuse the invitation, body: {}",
        refused.body
    );
    assert_eq!(code_of(&refused.body), "invitations_closed");
    assert_eq!(
        refused.body["error"]["details"]["invite_policy"], "closed",
        "the refusal names the policy that caused it: {}",
        refused.body
    );

    // And it changed nothing: a refused invitation leaves no row behind, so opening the tenant to
    // invitations again and inviting the same address succeeds. Without this, a policy could
    // "refuse" by writing a dead row.
    let stored: i64 = sqlx::query_scalar(
        "select count(*) from organization_invitations where organization_id = $1",
    )
    .bind(fixture.org_a)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must run");
    assert_eq!(stored, 0, "a refused invitation must not leave a row");

    set_invite_policy(&fixture.db, fixture.org_a, "self_serve").await;
    let allowed = invite(&fixture, &admin, &address).await;
    assert_eq!(
        allowed.status,
        StatusCode::CREATED,
        "the same address invites fine once the tenant is open: {}",
        allowed.body
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn self_serve_hands_over_a_working_link_to_anyone_who_may_manage() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    set_invite_policy(&fixture.db, fixture.org_a, "self_serve").await;
    let admin = fixture.admin_token().await;
    let address = format!("selfserve-{}@omnion.test", Uuid::new_v4().simple());

    let created = invite(&fixture, &admin, &address).await;

    assert_eq!(created.status, StatusCode::CREATED, "body: {}", created.body);
    let token = created.body["token"]
        .as_str()
        .expect("a self-serve invitation must carry its link")
        .to_owned();
    assert!(
        !token.is_empty(),
        "an empty token would be a dead link that looks alive"
    );
    assert_eq!(created.body["invitation"]["status"], "pending");

    // …and the link really works: the public preview sees the tenant, and the address is masked in
    // the event but named in the invitation the inviter holds.
    let preview = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/invitations/{token}"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(preview.status, StatusCode::OK, "body: {}", preview.body);
    assert_eq!(preview.body["usable"], true, "the link must be usable: {}", preview.body);

    fixture.cleanup().await;
}

#[tokio::test]
async fn owner_approval_queues_the_link_until_an_owner_releases_it() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    set_invite_policy(&fixture.db, fixture.org_a, "owner_approval").await;
    let admin = fixture.admin_token().await;
    let address = format!("queued-{}@omnion.test", Uuid::new_v4().simple());

    // The manager's invite: queued, and with *no* link to forward.
    let queued = invite(&fixture, &admin, &address).await;
    assert_eq!(
        queued.status,
        StatusCode::ACCEPTED,
        "a queued invitation is accepted, not created: {}",
        queued.body
    );
    assert_eq!(queued.body["invitation"]["status"], "awaiting_approval");
    assert_eq!(
        queued.body["token"].as_str().unwrap_or_default(),
        "",
        "a queued invitation must not hand out a link — the queue would be advisory"
    );
    let invitation_id = queued.body["invitation"]["id"]
        .as_str()
        .expect("the queued row has an id")
        .to_owned();

    // The queue is visible, oldest first.
    let queue = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/organizations/{}/invitations/queue", fixture.org_a),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(queue.status, StatusCode::OK, "body: {}", queue.body);
    let queued_rows = queue.body["invitations"].as_array().expect("an array");
    assert!(
        queued_rows.iter().any(|row| row["id"] == invitation_id.as_str()),
        "the queue must list what is waiting: {}",
        queue.body
    );
    assert!(
        queued_rows
            .iter()
            .all(|row| row["status"] == "awaiting_approval"),
        "the queue lists queued rows and nothing else: {}",
        queue.body
    );

    // The manager cannot release it: the guard is `organizations.manage`, which they hold, so the
    // only thing that can refuse them is the owner check inside the handler. That is the whole
    // difference between `self_serve` and `owner_approval`.
    let self_release = call(
        &fixture.state,
        request(
            Method::POST,
            &format!(
                "/api/v1/organizations/{}/invitations/{invitation_id}/release",
                fixture.org_a
            ),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(
        self_release.status,
        StatusCode::FORBIDDEN,
        "a manager must not release their own queue entry, body: {}",
        self_release.body
    );
    assert_eq!(code_of(&self_release.body), "not_an_organization_owner");

    // The owner's release mints the link — the first time a working one exists.
    let (owner_id, owner_email) = create_account(&fixture.db).await;
    add_membership(&fixture.db, fixture.org_a, owner_id).await;
    grant_organization_owner(&fixture.db, fixture.org_a, owner_id).await;
    let owner = login(&fixture.state, &owner_email).await;
    fixture.track_account(owner_id).await;

    let released = call(
        &fixture.state,
        request(
            Method::POST,
            &format!(
                "/api/v1/organizations/{}/invitations/{invitation_id}/release",
                fixture.org_a
            ),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(released.status, StatusCode::OK, "body: {}", released.body);
    assert_eq!(released.body["invitation"]["status"], "pending");
    let token = released.body["token"]
        .as_str()
        .expect("the release must hand over the link")
        .to_owned();
    assert!(!token.is_empty(), "the released link must not be empty");

    // The queue is empty again, and the link now works.
    let drained = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/organizations/{}/invitations/queue", fixture.org_a),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert!(
        drained.body["invitations"]
            .as_array()
            .expect("an array")
            .iter()
            .all(|row| row["id"] != invitation_id.as_str()),
        "a released invitation must leave the queue: {}",
        drained.body
    );

    let preview = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/invitations/{token}"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(
        preview.body["usable"], true,
        "the released link must work: {}",
        preview.body
    );

    // Releasing twice is refused by name rather than silently succeeding: the second release would
    // mint a *new* link, silently orphaning the one the inviter already sent.
    let twice = call(
        &fixture.state,
        request(
            Method::POST,
            &format!(
                "/api/v1/organizations/{}/invitations/{invitation_id}/release",
                fixture.org_a
            ),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(twice.status, StatusCode::CONFLICT, "body: {}", twice.body);
    assert_eq!(code_of(&twice.body), "invitation_not_queued");

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_queued_link_never_works_and_says_so() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    set_invite_policy(&fixture.db, fixture.org_a, "owner_approval").await;
    let admin = fixture.admin_token().await;
    let address = format!("notyet-{}@omnion.test", Uuid::new_v4().simple());

    // Created straight through the store so the test holds the raw token the API refused to
    // return — a real deployment never shows it, but a *leaked* one must still be inert.
    let created = memberships::create_invitation(
        fixture.db.pool(),
        memberships::NewInvitation {
            organization_id: fixture.org_a,
            email: address.clone(),
            role_id: None,
            invited_by: None,
            message: String::new(),
            expires_at: None,
            queued: true,
        },
    )
    .await
    .expect("the queued invitation must be created");

    let preview = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/invitations/{}", created.token),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(
        preview.body["usable"], false,
        "a queued link must never be usable: {}",
        preview.body
    );

    // And the invitee is told *why*, not "this link is not valid" — they would go back to the
    // manager who just invited them.
    let (invitee_id, invitee_email) = create_account(&fixture.db).await;
    fixture.track_account(invitee_id).await;
    let invitee = login(&fixture.state, &invitee_email).await;
    let accepted = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/invitations/{}/accept", created.token),
            Some(&invitee),
            None,
        ),
    )
    .await;
    assert_eq!(accepted.status, StatusCode::CONFLICT, "body: {}", accepted.body);
    assert_eq!(
        code_of(&accepted.body),
        "invitation_awaiting_approval",
        "the invitee is told the invitation is waiting, not that it is invalid: {}",
        accepted.body
    );

    // It is still queued afterwards: a refused acceptance must not have released it.
    let status: String = sqlx::query_scalar("select status from organization_invitations where id = $1")
        .bind(created.invitation.id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the row must still be there");
    assert_eq!(status, "awaiting_approval");

    // A token that was never issued still answers indistinguishably, so the queue is not a probe
    // for which organizations exist.
    let bogus = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/invitations/not-a-real-token",
            None,
            None,
        ),
    )
    .await;
    assert_eq!(
        bogus.body["usable"], false,
        "an unknown token is unusable: {}",
        bogus.body
    );
    assert_eq!(
        bogus.body["reason"], preview.body["reason"],
        "a queued and an unknown token must be indistinguishable: {} vs {}",
        preview.body, bogus.body
    );

    let _ = admin;
    fixture.cleanup().await;
}

#[tokio::test]
async fn an_owner_inviting_into_their_own_tenant_is_not_stuck_behind_the_queue() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    set_invite_policy(&fixture.db, fixture.org_a, "owner_approval").await;
    let (owner_id, owner_email) = create_account(&fixture.db).await;
    add_membership(&fixture.db, fixture.org_a, owner_id).await;
    grant_organization_owner(&fixture.db, fixture.org_a, owner_id).await;
    fixture.track_account(owner_id).await;
    let owner = login(&fixture.state, &owner_email).await;

    let created = invite(
        &fixture,
        &owner,
        &format!("owner-{}@omnion.test", Uuid::new_v4().simple()),
    )
    .await;

    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "an owner must not have to queue an invitation behind themselves: {}",
        created.body
    );
    assert!(!created.body["token"].as_str().unwrap_or_default().is_empty());

    fixture.cleanup().await;
}

#[tokio::test]
async fn one_tenants_queue_is_another_tenants_invisible_row() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    set_invite_policy(&fixture.db, fixture.org_a, "owner_approval").await;
    let admin = fixture.admin_token().await;
    let other_admin = fixture.other_admin_token().await;
    let queued = invite(
        &fixture,
        &admin,
        &format!("private-{}@omnion.test", Uuid::new_v4().simple()),
    )
    .await;
    assert_eq!(queued.status, StatusCode::ACCEPTED, "body: {}", queued.body);
    let invitation_id = queued.body["invitation"]["id"]
        .as_str()
        .expect("an id")
        .to_owned();

    // Reading organization A's queue as B is a 404 — the isolation rule every tenancy route
    // follows, and the queue is no exception.
    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/organizations/{}/invitations/queue", fixture.org_a),
            Some(&other_admin),
            None,
        ),
    )
    .await;
    assert_eq!(read.status, StatusCode::NOT_FOUND, "body: {}", read.body);
    assert_eq!(code_of(&read.body), "organization_not_found");

    // Releasing it is refused the same way, not `403` — a 403 would confirm the row exists.
    let release = call(
        &fixture.state,
        request(
            Method::POST,
            &format!(
                "/api/v1/organizations/{}/invitations/{invitation_id}/release",
                fixture.org_a
            ),
            Some(&other_admin),
            None,
        ),
    )
    .await;
    assert_eq!(release.status, StatusCode::NOT_FOUND, "body: {}", release.body);
    assert_eq!(code_of(&release.body), "organization_not_found");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// Suspend and archive: reads stay, writes go, and the status change itself is escapable
// ---------------------------------------------------------------------------------------------

/// The settings payload the route expects.
///
/// `PUT /settings` is a whole-row replace, not a patch: `timezone`, `invite_policy` and
/// `audit_retention_days` are *required* fields, so a body carrying only the field under test
/// answers `422` and the walk fails for a reason that has nothing to do with the freeze. The
/// values are the backfilled defaults, so a refused write still leaves the row untouched.
fn settings_body(locale: &str) -> Value {
    json!({
        "locale": locale,
        "timezone": "UTC",
        "invite_policy": "self_serve",
        "default_invite_role_id": null,
        "logo_media_id": null,
        "accent_color": null,
        "audit_retention_days": 365,
    })
}

/// Suspend or re-activate one organization through the route the panel's button uses.
async fn set_status(fixture: &Fixture, token: &str, status: &str) -> TestResponse {
    call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/organizations/{}", fixture.org_a),
            Some(token),
            Some(json!({ "status": status })),
        ),
    )
    .await
}

#[tokio::test]
async fn a_suspended_organization_keeps_reads_and_refuses_writes_by_name() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;

    // A tenant with a site in it, so the site and domain write paths have something to act on.
    let site = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sites",
            Some(&admin),
            Some(json!({ "key": "before", "name": "Before the freeze" })),
        ),
    )
    .await;
    assert_eq!(site.status, StatusCode::CREATED, "body: {}", site.body);
    let site_id = site.body["id"].as_str().expect("a created site carries an id");

    // While it is active, the same writes work. A walk that only ever exercises the frozen state
    // cannot tell "the guard refused" from "this endpoint was always broken".
    let settings_uri = format!("/api/v1/organizations/{}/settings", fixture.org_a);
    let warm = call(
        &fixture.state,
        request(
            Method::PUT,
            &settings_uri,
            Some(&admin),
            Some(settings_body("en-GB")),
        ),
    )
    .await;
    assert_eq!(warm.status, StatusCode::OK, "body: {}", warm.body);

    // Suspend it. This is a status change, so it is the one write that gets through.
    let suspended = set_status(&fixture, &admin, "suspended").await;
    assert_eq!(suspended.status, StatusCode::OK, "body: {}", suspended.body);
    assert_eq!(suspended.body["status"], "suspended");

    // ---- reads stay available ----------------------------------------------------------------------------
    // Every one of these is a *read* of a suspended tenant, and every one must answer. A guard
    // placed on the resolver rather than on the write would turn them into 404s and leave an
    // operator unable to find out why the tenant stopped working.
    for uri in [
        format!("/api/v1/organizations/{}", fixture.org_a),
        format!("/api/v1/organizations/{}/members", fixture.org_a),
        format!("/api/v1/organizations/{}/departments", fixture.org_a),
        format!("/api/v1/organizations/{}/settings", fixture.org_a),
        format!("/api/v1/organizations/{}/modules", fixture.org_a),
        format!("/api/v1/organizations/{}/limits", fixture.org_a),
        format!("/api/v1/organizations/{}/usage", fixture.org_a),
        format!("/api/v1/sites/{}", site_id),
        format!("/api/v1/sites/{}/domains", site_id),
        "/api/v1/me/organizations".to_owned(),
    ] {
        let read = call(
            &fixture.state,
            request(Method::GET, &uri, Some(&admin), None),
        )
        .await;
        assert_eq!(
            read.status,
            StatusCode::OK,
            "a suspended tenant must stay readable at {uri}, body: {}",
            read.body
        );
    }

    // ---- writes are refused, by name, with the reason -----------------------------------------------------
    // One walk over *every* write family. A guard that covers the tenancy surface and forgets
    // the site surface is the defect this list exists to prevent, and each line is a separate
    // endpoint so a hole in one shows up as a hole in one.
    let writes: Vec<(&str, Method, String, Option<Value>)> = vec![
        (
            "settings",
            Method::PUT,
            settings_uri.clone(),
            Some(settings_body("de-DE")),
        ),
        (
            "modules",
            Method::PUT,
            format!("/api/v1/organizations/{}/modules", fixture.org_a),
            Some(json!({ "modules": [] })),
        ),
        (
            "limits",
            Method::PUT,
            format!("/api/v1/organizations/{}/limits", fixture.org_a),
            Some(json!({ "plan": "enterprise" })),
        ),
        (
            "departments",
            Method::POST,
            format!("/api/v1/organizations/{}/departments", fixture.org_a),
            Some(json!({ "key": "new-team", "name": "New team" })),
        ),
        (
            "invitations",
            Method::POST,
            format!("/api/v1/organizations/{}/invitations", fixture.org_a),
            Some(json!({ "email": format!("frozen-{}@omnion.test", Uuid::new_v4().simple()) })),
        ),
        (
            "sites",
            Method::POST,
            "/api/v1/sites".to_owned(),
            Some(json!({ "key": "after", "name": "After the freeze" })),
        ),
        (
            "site rename",
            Method::PATCH,
            format!("/api/v1/sites/{site_id}"),
            Some(json!({ "name": "Renamed while frozen" })),
        ),
        (
            "domains",
            Method::POST,
            format!("/api/v1/sites/{site_id}/domains"),
            Some(json!({ "host": "frozen.example" })),
        ),
    ];

    for (label, method, uri, body) in &writes {
        let refused = call(
            &fixture.state,
            request(method.clone(), uri, Some(&admin), body.clone()),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::CONFLICT,
            "a suspended tenant must refuse the {label} write, body: {}",
            refused.body
        );
        assert_eq!(
            code_of(&refused.body),
            "organization_not_writable",
            "the {label} refusal names the rule: {}",
            refused.body
        );
        assert_eq!(
            refused.body["error"]["details"]["status"], "suspended",
            "the {label} refusal names the status: {}",
            refused.body
        );
        assert_eq!(
            refused.body["error"]["details"]["reads"], true,
            "the refusal must say reads still work, or an operator reads it as a deletion: {}",
            refused.body
        );
    }

    // ---- and it really refused: nothing was written --------------------------------------------------------
    // A refusal that leaves the row behind is a refusal that failed. The site's name and the
    // settings' locale are the two writes above, read back from the database.
    let stored_name: String =
        sqlx::query_scalar("select name from sites where id = $1")
            .bind(Uuid::parse_str(site_id).expect("a site id parses"))
            .fetch_one(fixture.db.pool())
            .await
            .expect("the site must still be there");
    assert_eq!(
        stored_name, "Before the freeze",
        "the refused site rename must not have been written"
    );

    let stored_locale: String = sqlx::query_scalar(
        "select locale from organization_settings where organization_id = $1",
    )
    .bind(fixture.org_a)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the settings row must still be there");
    assert_eq!(
        stored_locale, "en-GB",
        "the refused settings write must not have been written"
    );

    let frozen_departments: i64 =
        sqlx::query_scalar("select count(*) from departments where organization_id = $1")
            .bind(fixture.org_a)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the count must run");
    assert_eq!(
        frozen_departments, 0,
        "the refused department create must not have left a row"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn reactivating_restores_writes_and_archives_freeze_them_too() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;
    let settings_uri = format!("/api/v1/organizations/{}/settings", fixture.org_a);

    // ---- suspend, refuse, reactivate, write ----------------------------------------------------------------
    assert_eq!(set_status(&fixture, &admin, "suspended").await.status, StatusCode::OK);

    let refused = call(
        &fixture.state,
        request(
            Method::PUT,
            &settings_uri,
            Some(&admin),
            Some(settings_body("fr-FR")),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "body: {}", refused.body);

    // The escape hatch. If this is the step that fails, the feature has shipped a tenant that
    // can be suspended and never brought back — a guard in front of the only control that undoes
    // it, which reads in review as "correct" and is the worst possible outcome.
    let reactivated = set_status(&fixture, &admin, "active").await;
    assert_eq!(
        reactivated.status,
        StatusCode::OK,
        "reactivating must work while the tenant is frozen, body: {}",
        reactivated.body
    );
    assert_eq!(reactivated.body["status"], "active");

    let restored = call(
        &fixture.state,
        request(
            Method::PUT,
            &settings_uri,
            Some(&admin),
            Some(settings_body("fr-FR")),
        ),
    )
    .await;
    assert_eq!(
        restored.status,
        StatusCode::OK,
        "writes must work again after a reactivation, body: {}",
        restored.body
    );

    // ---- archive: the same freeze, a quieter one ------------------------------------------------------------
    assert_eq!(set_status(&fixture, &admin, "archived").await.status, StatusCode::OK);

    let archived_write = call(
        &fixture.state,
        request(
            Method::PUT,
            &settings_uri,
            Some(&admin),
            Some(settings_body("it-IT")),
        ),
    )
    .await;
    assert_eq!(
        archived_write.status,
        StatusCode::CONFLICT,
        "an archived tenant refuses writes too, body: {}",
        archived_write.body
    );
    assert_eq!(
        archived_write.body["error"]["details"]["status"], "archived",
        "the refusal names the archived status: {}",
        archived_write.body
    );

    // And an archived tenant is still readable — the audit trail of why it was archived is the
    // one thing an operator needs, and it is behind the same reads.
    let read = call(
        &fixture.state,
        request(Method::GET, &settings_uri, Some(&admin), None),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "body: {}", read.body);

    // A rename of a frozen tenant is a plain write and is refused: the escape hatch is for a
    // *status change*, not for any request that happens to carry one.
    let renamed = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/organizations/{}", fixture.org_a),
            Some(&admin),
            Some(json!({ "name": "Renamed while archived" })),
        ),
    )
    .await;
    assert_eq!(
        renamed.status,
        StatusCode::CONFLICT,
        "a rename is not a status change and must be refused, body: {}",
        renamed.body
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_status_move_is_audited_and_announced_as_its_own_event() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;

    assert_eq!(set_status(&fixture, &admin, "suspended").await.status, StatusCode::OK);

    // The audit row is the *lifecycle* action, not a generic `organization.updated`: a trail that
    // records "someone edited the organization" cannot answer "who suspended this tenant and
    // when", which is the question the trail exists for.
    let trail: Vec<String> = sqlx::query_scalar(
        "select action from audit_log where organization_id = $1 order by created_at desc",
    )
    .bind(fixture.org_a)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the trail must read");
    assert!(
        trail.iter().any(|action| action == "organization.suspended"),
        "the suspension must be audited as itself: {trail:?}"
    );
    assert!(
        !trail.iter().any(|action| action == "organization.updated"),
        "a status move must not be filed as a plain update: {trail:?}"
    );

    // The event bus carries it too, so a subscriber can freeze downstream work on the tenant
    // without polling the organization row.
    let emitted: Vec<String> = sqlx::query_scalar(
        "select name from events where organization_id = $1 and name = $2",
    )
    .bind(fixture.org_a)
    .bind("organization.suspended")
    .fetch_all(fixture.db.pool())
    .await
    .expect("the events must read");
    assert_eq!(
        emitted.len(),
        1,
        "exactly one organization.suspended event must be emitted"
    );

    // Reactivating is its own action rather than a second `suspended`, so a consumer can count
    // the freezes a tenant went through.
    assert_eq!(set_status(&fixture, &admin, "active").await.status, StatusCode::OK);
    let reactivated: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where organization_id = $1 and action = $2",
    )
    .bind(fixture.org_a)
    .bind("organization.reactivated")
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must run");
    assert_eq!(reactivated, 1, "the reactivation must be audited as itself");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// The Audit tab
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_audit_tab_reads_this_tenant_only_and_exports_what_it_shows() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;
    let uri = format!("/api/v1/organizations/{}/audit", fixture.org_a);

    // A tenancy administrator is refused: `audit.read` is not implied by `organizations.read`.
    // Without this assertion the tab would "work" for anyone, and the split would be untested.
    let refused = call(
        &fixture.state,
        request(Method::GET, &uri, Some(&admin), None),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a trail is not a member listing: {}",
        refused.body
    );

    // An auditor sees the tenant's own trail — and the settings change the walk is about to make
    // is already in it, because every privileged write records in the same request.
    let (auditor_id, auditor_email) = create_account(&fixture.db).await;
    add_membership(&fixture.db, fixture.org_a, auditor_id).await;
    grant_permissions(
        &fixture.db,
        fixture.org_a,
        auditor_id,
        &["organizations.read", &AUDIT_PERMISSIONS[0]],
    )
    .await;
    fixture.track_account(auditor_id).await;
    let auditor = login(&fixture.state, &auditor_email).await;

    set_invite_policy(&fixture.db, fixture.org_a, "self_serve").await;
    invite(
        &fixture,
        &admin,
        &format!("audited-{}@omnion.test", Uuid::new_v4().simple()),
    )
    .await;

    let feed = call(
        &fixture.state,
        request(Method::GET, &uri, Some(&auditor), None),
    )
    .await;
    assert_eq!(feed.status, StatusCode::OK, "body: {}", feed.body);
    let entries = feed.body["entries"].as_array().expect("an array");
    assert!(
        entries.iter().any(|row| row["action"] == "organization.settings.updated"),
        "the settings change must be in this tenant's trail: {}",
        feed.body
    );
    assert!(
        entries.iter().any(|row| row["action"] == "organization.member.invited"),
        "and so must the invitation: {}",
        feed.body
    );

    // Every row carries a readable actor — the whole point of a feed. A system row says so rather
    // than rendering a blank cell.
    for row in entries {
        assert!(row["actor_type"].is_string(), "every row names its actor type: {row}");
        if row["actor_type"] == "user" {
            assert!(
                row["actor_name"].as_str().is_some_and(|name| !name.is_empty()),
                "a human row names the human: {row}"
            );
        }
    }

    // The action filter is exact, chosen from the list the same response returns.
    let actions = feed.body["actions"].as_array().expect("an array");
    assert!(
        actions.iter().any(|value| value == "organization.settings.updated"),
        "the filter offers what this tenant has done: {actions:?}"
    );
    let filtered = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("{uri}?action=organization.settings.updated"),
            Some(&auditor),
            None,
        ),
    )
    .await;
    assert_eq!(filtered.status, StatusCode::OK, "body: {}", filtered.body);
    let narrowed = filtered.body["entries"].as_array().expect("an array");
    assert!(
        !narrowed.is_empty(),
        "the filter must match the row that exists"
    );
    assert!(
        narrowed
            .iter()
            .all(|row| row["action"] == "organization.settings.updated"),
        "the filter must not leak other actions: {}",
        filtered.body
    );
    assert!(
        filtered.body["total"].as_i64().unwrap_or_default() < feed.body["total"].as_i64().unwrap_or_default(),
        "a narrowed feed counts fewer rows than the whole one: {} vs {}",
        filtered.body["total"], feed.body["total"]
    );

    // A typo is refused rather than answered as "this tenant has no history" — the one reading an
    // audit screen must never give by accident.
    let nonsense = call(
        &fixture.state,
        request(Method::GET, &format!("{uri}?actor=not-an-id"), Some(&auditor), None),
    )
    .await;
    assert_eq!(nonsense.status, StatusCode::BAD_REQUEST, "body: {}", nonsense.body);
    assert_eq!(code_of(&nonsense.body), "invalid_actor_filter");

    // The CSV repeats the rows on screen, not a different query's idea of them.
    let csv = call(
        &fixture.state,
        request(Method::GET, &format!("{uri}?format=csv"), Some(&auditor), None),
    )
    .await;
    assert_eq!(csv.status, StatusCode::OK);
    let text = String::from_utf8_lossy(&csv.raw);
    let header = text.lines().next().expect("a header row");
    assert!(header.starts_with("id,action,actor,"), "the header names the columns: {header}");
    let rows = text.lines().skip(1).filter(|line| !line.trim().is_empty()).count();
    assert_eq!(
        rows,
        entries.len(),
        "the CSV must carry the page the tab rendered: {rows} vs {}",
        entries.len()
    );
    assert!(
        text.contains("organization.settings.updated"),
        "the export carries the rows, not just the header"
    );

    // Another tenant's trail is a 404, never a 403.
    let other_admin = fixture.other_admin_token().await;
    let foreign = call(
        &fixture.state,
        request(Method::GET, &uri, Some(&other_admin), None),
    )
    .await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND, "body: {}", foreign.body);
    assert_eq!(code_of(&foreign.body), "organization_not_found");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// The module switch, applied (REQ-005, slice 4)
// ---------------------------------------------------------------------------------------------

/// Switching a module off takes its screens away and its API with them, and switching it back
/// restores both.
///
/// Slice 3 stored the decision and this is the half that *applies* it. The walk is careful about
/// three ways it can pass for the wrong reason, and each of them is guarded here rather than left
/// to review:
///
/// 1. **A 403 from the permission guard looks exactly like a 403 from the module guard.** The
///    fixture grants [`MODULE_PERMISSIONS`] to the administrator first, so the module refusal is
///    the one that can be reached — and the assertion is on the *code*, not the status.
/// 2. **A core route must not move.** `/organizations` and `/pages` are never modules, so a
///    tenant that switched three of five modules off has still lost nothing it cannot get back.
/// 3. **A module the platform did not ship must not be switchable into place** — which the
///    Modules endpoint already refuses, so the guard never has an unknown key to look up.
#[tokio::test]
async fn switching_a_module_off_hides_its_api_and_switching_it_back_restores_it() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // The module routes carry their own permissions; without them the walk would prove the
    // permission guard instead of the module guard, which reads the same from the outside.
    // Organization B's administrator gets them too, for the isolation assertions below: a 403
    // there would be indistinguishable from B having inherited A's switch.
    let admin_id = fixture.accounts[0];
    let other_admin_id = fixture.accounts[2];
    grant_permissions(&fixture.db, fixture.org_a, admin_id, &MODULE_PERMISSIONS).await;
    grant_permissions(&fixture.db, fixture.org_a, admin_id, &CORE_PERMISSIONS).await;
    grant_permissions(
        &fixture.db,
        fixture.organizations[1],
        other_admin_id,
        &MODULE_PERMISSIONS,
    )
    .await;
    let admin = fixture.admin_token().await;
    let modules_uri = format!("/api/v1/organizations/{}/modules", fixture.org_a);

    // The media library is site-scoped, so the walk creates a site rather than calling the
    // collection with a blank `site_id`. That is not a convenience: an empty id answers `400`
    // from `site_in_scope`, and a `400` in the "before" position would make the whole comparison
    // meaningless — the walk would be comparing a parameter error to a module refusal and
    // reading the difference as "the switch worked".
    let site = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sites",
            Some(&admin),
            Some(json!({ "key": "module-switch", "name": "Module switch" })),
        ),
    )
    .await;
    assert_eq!(site.status, StatusCode::CREATED, "body: {}", site.body);
    let site_id = site.body["id"]
        .as_str()
        .expect("a created site carries an id")
        .to_owned();
    let media_uri = format!("/api/v1/media?site_id={site_id}");

    // Before: media works. This is the control — a walk that never showed the route working
    // cannot show that switching the module off *changed* anything.
    let before = call(
        &fixture.state,
        request(Method::GET, &media_uri, Some(&admin), None),
    )
    .await;
    assert_eq!(
        before.status,
        StatusCode::OK,
        "media answers for an organization that never switched it off: {}",
        before.body
    );

    // Switch media off.
    let switched = call(
        &fixture.state,
        request(
            Method::PUT,
            &modules_uri,
            Some(&admin),
            Some(json!({ "modules": [{ "module_key": "media", "enabled": false }] })),
        ),
    )
    .await;
    assert_eq!(switched.status, StatusCode::OK, "body: {}", switched.body);

    // The API now refuses it, and the refusal names the module rather than a permission.
    let after = call(
        &fixture.state,
        request(Method::GET, &media_uri, Some(&admin), None),
    )
    .await;
    assert_eq!(
        after.status,
        StatusCode::FORBIDDEN,
        "a module switched off refuses its own routes: {}",
        after.body
    );
    assert_eq!(
        code_of(&after.body),
        "organization.module.disabled",
        "and the refusal names the switch, not the role"
    );
    assert_eq!(after.body["error"]["details"]["module"], "media");
    assert_eq!(
        after.body["error"]["details"]["module_name"], "Media library",
        "a module key alone reads as a path segment to somebody who has never seen the tab"
    );

    // A *sub*-route is refused the same way — the guard keys on the module, not on one endpoint.
    // The mount a screen actually uses is `/media/files`, and a guard that only covered the
    // collection route would leave the file manager reachable with the switch off.
    let sub = call(
        &fixture.state,
        request(Method::GET, "/api/v1/media/files", Some(&admin), None),
    )
    .await;
    assert_eq!(sub.status, StatusCode::FORBIDDEN, "body: {}", sub.body);
    assert_eq!(code_of(&sub.body), "organization.module.disabled");

    // Another module is untouched: one tenant's switch is not a global one.
    let other_module = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/analytics/overview?site_id={site_id}"),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(
        other_module.status,
        StatusCode::OK,
        "switching one module off leaves the others alone: {}",
        other_module.body
    );

    // The core is never a module. Tenancy and content stay reachable with media off, because an
    // organization that could lose its own member list to a module switch could not be
    // administered back — the same rule as the frozen tenant's reactivation.
    for core in [
        format!("/api/v1/organizations/{}", fixture.org_a),
        format!("/api/v1/organizations/{}/members", fixture.org_a),
        format!("/api/v1/pages?site_id={site_id}"),
    ] {
        let response = call(
            &fixture.state,
            request(Method::GET, &core, Some(&admin), None),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{core} is the core and stays reachable with a module switched off: {}",
            response.body
        );
    }

    // Another tenant is unaffected — the decision is per organization, not per installation.
    // Their own site, so the read is not a `404` from tenancy: the point of the assertion is
    // that the *module* decision did not travel, and reusing organization A's site would prove
    // only that sites are isolated.
    let other_admin = fixture.other_admin_token().await;
    let their_site = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sites",
            Some(&other_admin),
            Some(json!({ "key": "other-tenant", "name": "Other tenant" })),
        ),
    )
    .await;
    assert_eq!(
        their_site.status,
        StatusCode::CREATED,
        "body: {}",
        their_site.body
    );
    let their_site_id = their_site.body["id"]
        .as_str()
        .expect("a created site carries an id");

    let foreign = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media?site_id={their_site_id}"),
            Some(&other_admin),
            None,
        ),
    )
    .await;
    assert_eq!(
        foreign.status,
        StatusCode::OK,
        "one tenant's switch must not reach another: {}",
        foreign.body
    );

    // And the isolation is real in the other direction too: organization A's site is not this
    // tenant's to read, so the 403 above was a *module* refusal and not a tenancy one. The code
    // is asserted as well as the status: `media.read` and a cross-tenant site produce two 403s
    // that differ only in the body, and reading the status alone would let the tenancy guard
    // pass for the module guard — the same substitution as a missing fixture permission, one
    // layer further out.
    let cross = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media?site_id={site_id}"),
            Some(&other_admin),
            None,
        ),
    )
    .await;
    assert_eq!(
        cross.status,
        StatusCode::FORBIDDEN,
        "another tenant cannot read this site's media: {}",
        cross.body
    );
    assert_eq!(
        code_of(&cross.body),
        "cross_organization",
        "and the reason is the tenancy boundary, not the module switch — otherwise the two \
         assertions above prove the same refusal twice and the module guard was never reached"
    );

    // The sidebar is told, so the panel can hide the entry: the same response the switcher reads
    // carries the keys.
    let mine = call(
        &fixture.state,
        request(Method::GET, "/api/v1/me/organizations", Some(&admin), None),
    )
    .await;
    let disabled: Vec<&str> = mine.body["disabled_modules"]
        .as_array()
        .expect("disabled_modules is an array")
        .iter()
        .map(|key| key.as_str().expect("a module key"))
        .collect();
    assert_eq!(
        disabled,
        vec!["media"],
        "the shell needs exactly the switched-off key to hide the entry: {}",
        mine.body
    );

    let theirs = call(
        &fixture.state,
        request(Method::GET, "/api/v1/me/organizations", Some(&other_admin), None),
    )
    .await;
    assert_eq!(
        theirs.body["disabled_modules"].as_array().map(Vec::len),
        Some(0),
        "a tenant that switched nothing off is told nothing: {}",
        theirs.body
    );

    // Switching it back on restores the route. A guard that only ever refused would satisfy
    // every assertion above, so the round trip is the whole proof.
    let restored = call(
        &fixture.state,
        request(
            Method::PUT,
            &modules_uri,
            Some(&admin),
            Some(json!({ "modules": [{ "module_key": "media", "enabled": true }] })),
        ),
    )
    .await;
    assert_eq!(restored.status, StatusCode::OK, "body: {}", restored.body);

    let again = call(
        &fixture.state,
        request(Method::GET, &media_uri, Some(&admin), None),
    )
    .await;
    assert_eq!(
        again.status,
        StatusCode::OK,
        "switching it back on restores the module: {}",
        again.body
    );

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// The per-organization retention sweep (slice 4)
// ---------------------------------------------------------------------------------------------

/// Write one audit row for `organization_id`, dated `created_at`.
///
/// Straight into `audit_log` rather than through a route: the sweep's job is to remove rows the
/// platform wrote over time, and a row planted in the past is the only way to prove a *window*
/// without waiting ten years for it to close.
async fn plant_audit_row(db: &Db, organization_id: Uuid, action: &str, created_at: OffsetDateTime) {
    sqlx::query(
        "insert into audit_log (organization_id, actor_type, action, target_type, target_id, \
         metadata, created_at) \
         values ($1, 'system', $2, 'organization', $1, '{}'::jsonb, $3)",
    )
    .bind(organization_id)
    .bind(action)
    .bind(created_at)
    .execute(db.pool())
    .await
    .expect("the audit row must be planted");
}

/// How many *non-housekeeping* rows one organization still holds.
///
/// The sweep's own receipt is excluded on purpose. It is written by this walk's own
/// assertion subjects, so counting it would make "did the sweep keep what it should" answer
/// `2` for a tenant that correctly kept exactly one row — a failure that reads as a leak when
/// it is really a receipt.
async fn audit_count(db: &Db, organization_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "select count(*) from audit_log where organization_id = $1 \
         and action <> 'organization.retention.swept'",
    )
    .bind(organization_id)
    .fetch_one(db.pool())
    .await
    .expect("the trail must read")
}

#[tokio::test]
async fn the_retention_sweep_applies_each_tenants_own_window() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let now = OffsetDateTime::now_utc();
    let day = time::Duration::days(1);

    // Two tenants with two *different* windows, and both windows already closed. A sweep that
    // used one platform-wide default would delete the 100-day-old row of the tenant that asked
    // for a 365-day window — and the only assertion that catches it is the one on the other
    // tenant, which is exactly the assertion people skip.
    set_retention(&fixture.db, fixture.org_a, 30).await;
    set_retention(&fixture.db, fixture.org_b(), 365).await;

    // A: two rows outside its 30-day window, one inside it.
    plant_audit_row(&fixture.db, fixture.org_a, "test.old", now - (day * 60)).await;
    plant_audit_row(&fixture.db, fixture.org_a, "test.ancient", now - (day * 400)).await;
    plant_audit_row(&fixture.db, fixture.org_a, "test.recent", now - (day * 2)).await;

    // B: one row outside A's window but well inside its own, and one outside both.
    plant_audit_row(&fixture.db, fixture.org_b(), "test.b100", now - (day * 100)).await;
    plant_audit_row(&fixture.db, fixture.org_b(), "test.b400", now - (day * 400)).await;

    let removed = omnion_api::retention_runner::sweep_once(&fixture.state)
        .await
        .expect("the sweep must run");
    assert_eq!(removed, 3, "two rows from A and one from B, and not one more");

    assert_eq!(
        audit_count(&fixture.db, fixture.org_a).await,
        1,
        "A keeps only the row inside its 30-day window"
    );
    assert_eq!(
        audit_count(&fixture.db, fixture.org_b()).await,
        1,
        "B keeps its 100-day-old row, which is inside its own 365-day window"
    );

    let b_actions: Vec<String> =
        sqlx::query_scalar("select action from audit_log where organization_id = $1 order by action")
            .bind(fixture.org_b())
            .fetch_all(fixture.db.pool())
            .await
            .expect("B's trail must read");
    assert!(
        b_actions.contains(&"test.b100".to_owned()),
        "the 100-day-old row is inside B's window and must survive: {b_actions:?}"
    );
    assert!(
        !b_actions.contains(&"test.b400".to_owned()),
        "the 400-day-old row is outside B's window and must not: {b_actions:?}"
    );

    // The receipt: a system row naming the tenant, the window it applied and the count. Read
    // from the database the way an operator's next audit would find it — the route is
    // `audit.read`-guarded and this walk is about the sweep, not about the tab.
    let filed: Vec<(String, String, i64, i32)> = sqlx::query_as(
        "select actor_type, action, (metadata->>'rows_removed')::bigint, \
                (metadata->>'retention_days')::int \
         from audit_log where organization_id = $1 and action = 'organization.retention.swept'",
    )
    .bind(fixture.org_a)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the receipts must read");
    assert_eq!(filed.len(), 1, "one receipt per sweep that removed something");
    assert_eq!(filed[0].0, "system", "nobody performed a sweep");
    assert_eq!(filed[0].1, "organization.retention.swept");
    assert_eq!(filed[0].2, 2, "the receipt repeats the count the sweep removed");
    assert_eq!(filed[0].3, 30, "the receipt names the window it applied");

    // The bus carries it, so a subscriber can archive elsewhere in step with the platform.
    let emitted: Vec<(String, i64)> = sqlx::query_as(
        "select name, (payload->>'rows_removed')::bigint from events \
         where organization_id = $1 and name = 'organization.retention.swept'",
    )
    .bind(fixture.org_a)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the events must read");
    assert_eq!(emitted, vec![("organization.retention.swept".to_owned(), 2)]);

    // A second sweep over the same data removes nothing and files nothing: a trail full of its
    // own nightly housekeeping is a trail nobody reads.
    let again = omnion_api::retention_runner::sweep_once(&fixture.state)
        .await
        .expect("the sweep must run again");
    assert_eq!(again, 0, "a second sweep over the same rows is a no-op");
    let receipts: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where organization_id = $1 \
         and action = 'organization.retention.swept'",
    )
    .bind(fixture.org_a)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the receipt count must run");
    assert_eq!(receipts, 1, "a sweep that removed nothing files nothing");

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_tenant_keeps_rows_inside_its_window_and_one_without_settings_is_still_swept() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let now = OffsetDateTime::now_utc();

    // A tenant whose rows are all *inside* its window, and a second one with no settings row at
    // all. The first must be untouched; the second must be held to the default window rather
    // than being invisible to the sweep — a tenant created after the settings backfill has no
    // row, and "skip it" would keep its history forever, the opposite of what the tab promises.
    set_retention(&fixture.db, fixture.org_a, 365).await;
    plant_audit_row(
        &fixture.db,
        fixture.org_a,
        "test.fresh",
        now - time::Duration::days(1),
    )
    .await;
    plant_audit_row(
        &fixture.db,
        fixture.org_b(),
        "test.b.ancient",
        now - time::Duration::days(400),
    )
    .await;
    sqlx::query("delete from organization_settings where organization_id = $1")
        .bind(fixture.org_b())
        .execute(fixture.db.pool())
        .await
        .expect("the settings row must be removable for this walk");

    let removed = omnion_api::retention_runner::sweep_once(&fixture.state)
        .await
        .expect("the sweep must run");
    assert_eq!(removed, 1, "only the settings-less tenant's expired row goes");
    assert_eq!(
        audit_count(&fixture.db, fixture.org_a).await,
        1,
        "a row inside the window is not the sweep's business"
    );
    assert_eq!(
        audit_count(&fixture.db, fixture.org_b()).await,
        0,
        "a tenant with no settings row is held to the 365-day default, not skipped"
    );

    // The backfill restores the row, so this walk does not leave the tenant without settings.
    tenancy_limits::load_settings(fixture.db.pool(), fixture.org_b())
        .await
        .expect("the settings row must be backfillable");

    fixture.cleanup().await;
}

/// Store one organization's own retention window.
async fn set_retention(db: &Db, organization_id: Uuid, days: i32) {
    sqlx::query(
        "insert into organization_settings (organization_id, audit_retention_days) \
         values ($1, $2) \
         on conflict (organization_id) do update \
         set audit_retention_days = excluded.audit_retention_days",
    )
    .bind(organization_id)
    .bind(days)
    .execute(db.pool())
    .await
    .expect("the retention window must be storable");
}


// ---------------------------------------------------------------------------------------------
// The member drawer (REQ-005, slice 4)
// ---------------------------------------------------------------------------------------------

/// The powers the drawer walks need on top of [`ADMIN_PERMISSIONS`], and the reason they are a
/// separate grant.
///
/// The drawer's three writes are guarded by `iam.bindings.manage` *as well as*
/// `organizations.manage`. An administrator holding only the latter answers `403
/// permission_denied` from the permission layer, and that is indistinguishable — from the
/// outside — from the guard the walk means to exercise. A walk built on that fixture would
/// "prove" the drawer refuses a cross-tenant grant while proving only that the caller was
/// short of a permission.
const DRAWER_PERMISSIONS: [&str; 3] = [
    "iam.bindings.read",
    "iam.bindings.manage",
    "audit.read",
];

/// Grant the drawer powers to an account and return the role it created.
///
/// The role id is returned because the walk has to grant *a* role through the drawer and needs
/// a role that is definitely this tenant's; building one inline and then reading it back out of
/// the response would make the test depend on the API it is testing.
async fn grant_drawer_permissions(db: &Db, organization_id: Uuid, user_id: Uuid) -> Uuid {
    let role = role_store::create_role(
        db.pool(),
        omnion_permissions::model::NewRole {
            organization_id,
            key: format!("drawer-admin-{}", Uuid::new_v4().simple()),
            name: "Drawer Administrator".to_owned(),
            description: "Runs the member drawer".to_owned(),
            priority: 800,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the drawer role must be created");

    let entries: Vec<omnion_permissions::model::RolePermissionInput> = DRAWER_PERMISSIONS
        .iter()
        .map(|key| omnion_permissions::model::RolePermissionInput {
            key: (*key).to_owned(),
            effect: omnion_permissions::model::Effect::Allow,
        })
        .collect();
    role_store::set_role_permissions(db.pool(), role.id, &entries)
        .await
        .expect("the drawer permission set must be written");

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
    .expect("the drawer binding must be granted");

    role.id
}

/// The member drawer answers for a member of this tenant, and the whole of it: the identity, the
/// bindings with their scope and expiry, the departments and the member's own trail.
///
/// The point of the first assertions is negative as much as positive: a drawer that returned
/// `200` with an empty `bindings` array for a member who *does* hold roles would render
/// perfectly and be wrong, so the walk grants first and reads after.
#[tokio::test]
async fn the_member_drawer_answers_with_everything_it_renders() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;

    // The second account of organization A is the subject of the drawer.
    let subject = fixture.accounts[1];
    let role_id = grant_drawer_permissions(&fixture.db, fixture.org_a, fixture.accounts[0]).await;

    let uri = format!(
        "/api/v1/organizations/{}/members/{subject}",
        fixture.org_a
    );

    let opened = call(
        &fixture.state,
        request(Method::GET, &uri, Some(&admin), None),
    )
    .await;
    assert_eq!(opened.status, StatusCode::OK, "drawer: {}", opened.body);
    assert_eq!(opened.body["user_id"], subject.to_string());
    assert!(opened.body["email"].is_string());
    assert!(opened.body["membership_id"].is_string());
    assert!(opened.body["status"].is_string());
    // The empty state is a real answer, not a missing key: a panel branching on
    // `bindings.length` must not have to guard against `undefined`.
    assert!(opened.body["bindings"].is_array());
    assert!(opened.body["departments"].is_array());
    assert!(opened.body["recent_audit"].is_array());

    // Grant a role through the drawer and read it back.
    let granted = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("{uri}/role-bindings"),
            Some(&admin),
            Some(json!({ "role_id": role_id })),
        ),
    )
    .await;
    assert_eq!(granted.status, StatusCode::CREATED, "grant: {}", granted.body);
    assert_eq!(granted.body["role_id"], role_id.to_string());
    // The default scope is the organization's, and it is named rather than assumed — a grant
    // that silently resolved to `global` would be a tenant administrator handing out a platform
    // grant.
    assert_eq!(granted.body["scope_type"], "organization");
    assert_eq!(granted.body["active"], true);
    assert!(granted.body["expires_at"].is_null());
    let binding_id = id_of(&granted.body);

    let after = call(
        &fixture.state,
        request(Method::GET, &uri, Some(&admin), None),
    )
    .await;
    assert_eq!(after.status, StatusCode::OK);
    let bindings = after.body["bindings"].as_array().expect("an array of bindings");
    assert_eq!(bindings.len(), 1, "{:?}", after.body["bindings"]);
    assert_eq!(bindings[0]["id"], binding_id);
    assert_eq!(bindings[0]["role_id"], role_id.to_string());

    // Whose trail is this? The subject's own acts. The grant was performed by the
    // *administrator*, so it belongs on the administrator's drawer and in the organization's
    // Audit tab — and showing an administrator's private trail inside a panel about a colleague
    // would be the wrong half of a privacy decision made by accident. Asserting the subject's
    // trail is empty therefore pins the boundary down rather than leaving it to whichever side
    // of the line the implementation happened to land on.
    let subject_trail = after.body["recent_audit"].as_array().expect("an audit array");
    assert!(
        !subject_trail
            .iter()
            .any(|row| row["action"] == "organization.member.role_changed"),
        "the subject's drawer must not carry the administrator's own acts: {subject_trail:?}"
    );

    // The grant is on the *grantor's* trail, in this tenant, naming the member and the role.
    let grantor_uri = format!(
        "/api/v1/organizations/{}/members/{}",
        fixture.org_a, fixture.accounts[0]
    );
    let grantor = call(
        &fixture.state,
        request(Method::GET, &grantor_uri, Some(&admin), None),
    )
    .await;
    assert_eq!(grantor.status, StatusCode::OK, "grantor: {}", grantor.body);
    let grantor_trail = grantor.body["recent_audit"]
        .as_array()
        .expect("an audit array");
    let role_changed = grantor_trail
        .iter()
        .find(|row| row["action"] == "organization.member.role_changed");
    assert!(
        role_changed.is_some(),
        "the grant must be on the grantor's own trail: {grantor_trail:?}"
    );
    // And a human name, not a blank — the Audit tab resolves it and so must this.
    assert_eq!(role_changed.expect("the grant row")["actor_name"], "Tenant Test");

    // The other tenant's administrator may not open this member at all — a `404`, because a
    // `403` would confirm that the user id exists somewhere.
    let other = fixture.other_admin_token().await;
    let foreign = call(
        &fixture.state,
        request(Method::GET, &uri, Some(&other), None),
    )
    .await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&foreign.body), "organization_not_found");

    fixture.cleanup().await;
}

/// Extend is a third verb, not a revoke-and-re-grant: the temporary grant moves its own expiry
/// and keeps its identity, its creation record and its place in the trail.
///
/// The last assertion is the one that matters. Revoke-then-grant would pass every other line of
/// this walk, and it would leave two rows — one revoked, one live — where the effective
/// permissions screen shows the same role twice with two different windows. Asserting that the
/// *same* binding id answers with the new date is what proves the row was updated rather than
/// replaced.
#[tokio::test]
async fn a_temporary_grant_is_extended_in_place_and_never_into_a_second_row() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;
    let subject = fixture.accounts[1];
    let role_id = grant_drawer_permissions(&fixture.db, fixture.org_a, fixture.accounts[0]).await;

    let uri = format!(
        "/api/v1/organizations/{}/members/{subject}",
        fixture.org_a
    );

    // A grant with a window an hour out.
    let first_hour = OffsetDateTime::now_utc() + time::Duration::hours(1);
    let granted = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("{uri}/role-bindings"),
            Some(&admin),
            Some(json!({
                "role_id": role_id,
                "expires_at": first_hour.format(&time::format_description::well_known::Rfc3339).unwrap(),
            })),
        ),
    )
    .await;
    assert_eq!(granted.status, StatusCode::CREATED, "grant: {}", granted.body);
    let binding_id = id_of(&granted.body);
    let original_expiry = granted.body["expires_at"].as_str().expect("an expiry").to_owned();

    // Extending backwards is refused: "extend" that lands in the past is a grant that reads as
    // renewed and does nothing.
    let past = OffsetDateTime::now_utc() - time::Duration::hours(1);
    let backwards = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("{uri}/role-bindings/{binding_id}"),
            Some(&admin),
            Some(json!({
                "expires_at": past.format(&time::format_description::well_known::Rfc3339).unwrap(),
            })),
        ),
    )
    .await;
    assert_eq!(backwards.status, StatusCode::BAD_REQUEST);
    assert_eq!(code_of(&backwards.body), "expiry_in_the_past");

    // A timestamp the platform cannot read is refused by name too, not answered as "extended to
    // nothing".
    let garbage = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("{uri}/role-bindings/{binding_id}"),
            Some(&admin),
            Some(json!({ "expires_at": "next tuesday" })),
        ),
    )
    .await;
    assert_eq!(garbage.status, StatusCode::BAD_REQUEST);
    assert_eq!(code_of(&garbage.body), "invalid_expiry");

    // The real extension: a week out, on the same row.
    let week = OffsetDateTime::now_utc() + time::Duration::days(7);
    let extended = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("{uri}/role-bindings/{binding_id}"),
            Some(&admin),
            Some(json!({
                "expires_at": week.format(&time::format_description::well_known::Rfc3339).unwrap(),
            })),
        ),
    )
    .await;
    assert_eq!(
        extended.status, StatusCode::OK,
        "extend: {}",
        extended.body
    );
    assert_eq!(extended.body["id"], binding_id, "the same row answered");
    assert_ne!(extended.body["expires_at"], original_expiry);
    assert_eq!(extended.body["active"], true);

    // Exactly one binding for this member and this role, and it is the extended one.
    let after = call(
        &fixture.state,
        request(Method::GET, &uri, Some(&admin), None),
    )
    .await;
    let bindings = after.body["bindings"].as_array().expect("an array of bindings");
    assert_eq!(bindings.len(), 1, "an extension must not add a row: {bindings:?}");
    assert_eq!(bindings[0]["id"], binding_id);

    // Revoking it is the last step, and a *revoked* binding cannot be extended afterwards: the
    // trail would otherwise record a revocation the row no longer honours.
    let revoked = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("{uri}/role-bindings/{binding_id}"),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(revoked.status, StatusCode::OK, "revoke: {}", revoked.body);
    assert_eq!(revoked.body["active"], false);
    assert!(revoked.body["revoked_at"].is_string());

    let reopen = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("{uri}/role-bindings/{binding_id}"),
            Some(&admin),
            Some(json!({
                "expires_at": week.format(&time::format_description::well_known::Rfc3339).unwrap(),
            })),
        ),
    )
    .await;
    // A revoked grant is refused *by name* rather than answered as a 404. The row exists, the
    // caller can see it in the drawer, and "there is no such grant" would send them looking for
    // a typo instead of reading the sentence that explains the rule.
    assert_eq!(reopen.status, StatusCode::BAD_REQUEST, "reopen: {}", reopen.body);
    assert_eq!(code_of(&reopen.body), "binding_revoked");

    fixture.cleanup().await;
}

/// A tenant cannot grant a role to somebody it does not employ, or a role that belongs to
/// another tenant — and it cannot reach a binding id from elsewhere either.
///
/// Three refusals, three different reasons, and the cross-tenant one is a `404` for the same
/// reason the drawer itself is: a `403` would confirm that the id exists.
#[tokio::test]
async fn a_grant_stays_inside_the_tenant_that_makes_it() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;
    let subject = fixture.accounts[1];
    let role_id = grant_drawer_permissions(&fixture.db, fixture.org_a, fixture.accounts[0]).await;

    // A fresh account that belongs to nobody.
    let (outsider_id, _outsider_email) = create_account(&fixture.db).await;
    fixture.track_account(outsider_id).await;

    let uri = format!(
        "/api/v1/organizations/{}/members/{outsider_id}",
        fixture.org_a
    );

    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("{uri}/role-bindings"),
            Some(&admin),
            Some(json!({ "role_id": role_id })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&refused.body), "member_not_found");

    // A role from the *other* tenant, granted to a real member of this one.
    let other_role = grant_drawer_permissions(&fixture.db, fixture.org_b(), fixture.accounts[2]).await;
    let member_uri = format!(
        "/api/v1/organizations/{}/members/{subject}",
        fixture.org_a
    );
    let cross_role = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("{member_uri}/role-bindings"),
            Some(&admin),
            Some(json!({ "role_id": other_role })),
        ),
    )
    .await;
    assert_eq!(cross_role.status, StatusCode::FORBIDDEN);
    assert_eq!(code_of(&cross_role.body), "cross_organization");

    // A `global` scope is refused by name rather than quietly downgraded to the organization's:
    // a tenant administrator asking for it is asking for something the platform owns.
    let global_scope = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("{member_uri}/role-bindings"),
            Some(&admin),
            Some(json!({ "role_id": role_id, "scope_type": "global" })),
        ),
    )
    .await;
    assert_eq!(global_scope.status, StatusCode::BAD_REQUEST);
    assert_eq!(code_of(&global_scope.body), "unsupported_scope");

    // A department scope needs its key, and says so rather than creating a grant with an empty
    // scope — which would resolve for nobody.
    let headless = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("{member_uri}/role-bindings"),
            Some(&admin),
            Some(json!({ "role_id": role_id, "scope_type": "department" })),
        ),
    )
    .await;
    assert_eq!(headless.status, StatusCode::BAD_REQUEST);
    assert_eq!(code_of(&headless.body), "department_required");

    // A binding id that exists — in this tenant — is still a `404` for another member, so one
    // member's drawer cannot revoke another's grant by guessing an id.
    let granted = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("{member_uri}/role-bindings"),
            Some(&admin),
            Some(json!({ "role_id": role_id })),
        ),
    )
    .await;
    assert_eq!(granted.status, StatusCode::CREATED, "grant: {}", granted.body);
    let binding_id = id_of(&granted.body);

    let other = fixture.other_admin_token().await;
    let foreign = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!(
                "/api/v1/organizations/{}/members/{subject}/role-bindings/{binding_id}",
                fixture.org_b()
            ),
            Some(&other),
            None,
        ),
    )
    .await;
    assert_eq!(
        foreign.status, StatusCode::NOT_FOUND,
        "another tenant may not revoke: {}",
        foreign.body
    );

    fixture.cleanup().await;
}
