//! Integration tests for the tenancy surface: organizations, sites, domains and the scope rules
//! that keep one tenant out of another (docs/01-VISION.md §10, docs/07-IAM.md §7, phase P04).
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
use omnion_identity::sites;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// Permission keys the organization administrator of this suite holds.
const ADMIN_PERMISSIONS: [&str; 7] = [
    "organizations.read",
    "organizations.manage",
    "sites.read",
    "sites.create",
    "sites.update",
    "sites.delete",
    "domains.manage",
];

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

/// Two organizations, a platform Owner and two accounts of the first organization: one holding
/// the tenancy permissions at organization scope, one holding nothing.
///
/// Every row this suite creates carries a `tenancy-` slug or address prefix, so cleanup can
/// remove exactly its own work without touching parallel suites.
struct Fixture {
    state: AppState,
    db: Db,
    platform_email: String,
    org_a: Uuid,
    org_b: Uuid,
    admin_id: Uuid,
    admin_email: String,
    member_email: String,
    /// Every account this fixture created, so cleanup removes exactly its own work.
    accounts: Vec<Uuid>,
    /// Every organization this fixture created (tests add the ones they open over HTTP).
    organizations: Vec<Uuid>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let org_a = create_organization_row(&db, "a", "Tenancy Test A").await;
        let org_b = create_organization_row(&db, "b", "Tenancy Test B").await;

        // The platform Owner: no primary organization, so it may work across tenants.
        let (platform_id, platform_email) = create_account(&db, None).await;
        seed::bind_owner(db.pool(), platform_id)
            .await
            .expect("the owner binding must be created");

        // The organization administrator: tenancy permissions bound at organization scope.
        let (admin_id, admin_email) = create_account(&db, Some(org_a)).await;
        let role = role_store::create_role(
            db.pool(),
            NewRole {
                organization_id: org_a,
                key: format!("site-admin-{}", Uuid::new_v4().simple()),
                name: "Site Administrator".to_owned(),
                description: "Runs the sites of one organization".to_owned(),
                priority: 800,
                inherits_role_id: None,
            },
        )
        .await
        .expect("the organization role must be created");

        let entries: Vec<RolePermissionInput> = ADMIN_PERMISSIONS
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
            user_id: admin_id,
            scope: Scope::Organization {
                organization_id: org_a,
            },
            granted_by: Some(platform_id),
            expires_at: None,
        };
        bindings::validate(db.pool(), &binding)
            .await
            .expect("the binding must validate");
        bindings::grant(db.pool(), binding)
            .await
            .expect("the binding must be granted");

        // A plain member of the same organization, without a tenancy permission.
        let (member_id, member_email) = create_account(&db, Some(org_a)).await;

        Some(Self {
            state,
            db,
            platform_email,
            org_a,
            org_b,
            admin_id,
            admin_email,
            member_email,
            accounts: vec![platform_id, admin_id, member_id],
            organizations: vec![org_a, org_b],
        })
    }

    /// The platform Owner, signed in.
    async fn platform_token(&self) -> String {
        login(&self.state, &self.platform_email).await
    }

    /// The organization administrator, signed in.
    async fn admin_token(&self) -> String {
        login(&self.state, &self.admin_email).await
    }

    /// The plain member of the first organization, signed in.
    async fn member_token(&self) -> String {
        login(&self.state, &self.member_email).await
    }

    /// Remember an organization the test opened over HTTP, so cleanup removes it too.
    fn remember_organization(&mut self, organization_id: Uuid) {
        self.organizations.push(organization_id);
    }

    /// Remove exactly what this fixture created — by id, never by a pattern: the tests of this
    /// suite run in parallel, so a prefix delete would remove a neighbour's rows.
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

/// Create an organization row with a unique, suite-scoped slug (`tenancy-fix-<label>-<uuid>`).
async fn create_organization_row(db: &Db, label: &str, name: &str) -> Uuid {
    let slug = format!("tenancy-fix-{label}-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(name)
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// Create an account with a unique address so parallel runs cannot collide.
async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("tenancy-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Tenancy Test".to_owned(),
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

/// The `id` field of a response body, as text.
fn id_of(body: &Value) -> String {
    body["id"]
        .as_str()
        .unwrap_or_else(|| panic!("body carries an id: {body}"))
        .to_owned()
}

/// Every `slug`/`key` value of an array inside `pointer`.
fn field_of_all(body: &Value, pointer: &str, field: &str) -> Vec<String> {
    body[pointer]
        .as_array()
        .unwrap_or_else(|| panic!("{pointer} must be an array in {body}"))
        .iter()
        .filter_map(|entry| entry[field].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn the_tenancy_surface_is_permission_gated() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let member = fixture.member_token().await;
    let admin = fixture.admin_token().await;

    // Without a session nothing answers.
    for route in ["/api/v1/organizations", "/api/v1/sites"] {
        let anonymous = call(&fixture.state, request(Method::GET, route, None, None)).await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "{route}");
    }

    // A member of the organization holds no tenancy permission.
    let denied = call(
        &fixture.state,
        request(Method::GET, "/api/v1/sites", Some(&member), None),
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(denied.body["error"]["code"], "permission_denied");

    // The administrator holds them at organization scope.
    let allowed = call(
        &fixture.state,
        request(Method::GET, "/api/v1/sites", Some(&admin), None),
    )
    .await;
    assert_eq!(allowed.status, StatusCode::OK, "sites: {}", allowed.body);

    let organizations = call(
        &fixture.state,
        request(Method::GET, "/api/v1/organizations", Some(&admin), None),
    )
    .await;
    assert_eq!(organizations.status, StatusCode::OK);
    assert_eq!(
        field_of_all(&organizations.body, "organizations", "id").len(),
        1,
        "an organization account sees exactly its own organization: {}",
        organizations.body
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn only_the_platform_opens_tenants_and_reads_across_them() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let platform = fixture.platform_token().await;
    let admin = fixture.admin_token().await;
    let org_b = fixture.org_b;

    // The platform Owner opens a tenant.
    let slug = format!("tenancy-{}", Uuid::new_v4().simple());
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/organizations",
            Some(&platform),
            Some(json!({ "name": "Tenancy Created", "slug": slug })),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "create body: {}",
        created.body
    );
    assert_eq!(created.body["slug"], slug);
    assert_eq!(created.body["status"], "active");
    let created_id = id_of(&created.body);
    fixture.remember_organization(Uuid::parse_str(&created_id).expect("tenant id"));

    // The same slug is refused.
    let duplicate = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/organizations",
            Some(&platform),
            Some(json!({ "name": "Tenancy Created Again", "slug": slug })),
        ),
    )
    .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT);
    assert_eq!(duplicate.body["error"]["code"], "organization_slug_taken");

    // An organization account may not open one, even holding `organizations.manage`-adjacent
    // permissions: opening tenants is a platform action.
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/organizations",
            Some(&admin),
            Some(json!({ "name": "Tenancy Refused", "slug": format!("tenancy-no-{}", Uuid::new_v4().simple()) })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(refused.body["error"]["code"], "platform_only");

    // The platform sees every tenant; the organization account is refused the other one.
    let all = call(
        &fixture.state,
        request(Method::GET, "/api/v1/organizations", Some(&platform), None),
    )
    .await;
    assert_eq!(all.status, StatusCode::OK);
    let ids = field_of_all(&all.body, "organizations", "id");
    assert!(ids.contains(&created_id), "the new tenant is listed");
    assert!(
        ids.contains(&org_b.to_string()),
        "the fixture tenant is listed"
    );

    let foreign = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/organizations/{org_b}"),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(foreign.status, StatusCode::FORBIDDEN);
    assert_eq!(foreign.body["error"]["code"], "cross_organization");

    // … while its own organization answers.
    let own = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/organizations/{}", fixture.org_a),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(own.status, StatusCode::OK, "own organization: {}", own.body);

    // The platform renames the new tenant.
    let updated = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/organizations/{created_id}"),
            Some(&platform),
            Some(json!({ "name": "Tenancy Renamed", "status": "suspended" })),
        ),
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK, "update: {}", updated.body);
    assert_eq!(updated.body["name"], "Tenancy Renamed");
    assert_eq!(updated.body["status"], "suspended");

    // … and the change is in the audit trail of that tenant.
    let audit = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/iam/audit?organization_id={created_id}"),
            Some(&platform),
            None,
        ),
    )
    .await;
    assert_eq!(audit.status, StatusCode::OK, "audit: {}", audit.body);
    let actions = field_of_all(&audit.body, "entries", "action");
    assert!(
        actions.contains(&"organization.created".to_owned()),
        "{actions:?}"
    );
    assert!(
        actions.contains(&"organization.updated".to_owned()),
        "{actions:?}"
    );

    // A tenant that still owns a site is not deleted in one step.
    let site = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sites",
            Some(&platform),
            Some(json!({ "organization_id": created_id, "key": "main", "name": "Main Site" })),
        ),
    )
    .await;
    assert_eq!(site.status, StatusCode::CREATED, "site: {}", site.body);
    let site_id = id_of(&site.body);

    let blocked = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/organizations/{created_id}"),
            Some(&platform),
            None,
        ),
    )
    .await;
    assert_eq!(
        blocked.status,
        StatusCode::CONFLICT,
        "blocked: {}",
        blocked.body
    );
    assert_eq!(blocked.body["error"]["code"], "organization_not_empty");

    // An organization account may not remove a tenant at all.
    let refused_delete = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/organizations/{}", fixture.org_a),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(refused_delete.status, StatusCode::FORBIDDEN);
    assert_eq!(refused_delete.body["error"]["code"], "platform_only");

    // Emptied, the tenant goes — the site first, then the organization itself.
    let removed_site = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/sites/{site_id}"),
            Some(&platform),
            None,
        ),
    )
    .await;
    assert_eq!(removed_site.status, StatusCode::NO_CONTENT);

    let removed = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/organizations/{created_id}"),
            Some(&platform),
            None,
        ),
    )
    .await;
    assert_eq!(
        removed.status,
        StatusCode::NO_CONTENT,
        "delete: {}",
        removed.body
    );

    let gone = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/organizations/{created_id}"),
            Some(&platform),
            None,
        ),
    )
    .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    assert_eq!(gone.body["error"]["code"], "organization_not_found");

    fixture.cleanup().await;
}

#[tokio::test]
async fn sites_stay_inside_their_organization() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let platform = fixture.platform_token().await;
    let admin = fixture.admin_token().await;
    let org_b = fixture.org_b;

    // A site of the second organization, created by the platform.
    let foreign_site = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sites",
            Some(&platform),
            Some(json!({ "organization_id": org_b, "key": "main", "name": "Company B" })),
        ),
    )
    .await;
    assert_eq!(
        foreign_site.status,
        StatusCode::CREATED,
        "foreign site: {}",
        foreign_site.body
    );
    let foreign_site_id = id_of(&foreign_site.body);

    // The organization administrator creates a site of its own.
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sites",
            Some(&admin),
            Some(json!({ "key": "  Main  ", "name": "Main Site" })),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "create: {}",
        created.body
    );
    assert_eq!(created.body["key"], "main", "keys normalize to lowercase");
    assert_eq!(created.body["organization_id"], fixture.org_a.to_string());
    let site_id = id_of(&created.body);

    // A second site with the same key is refused inside the organization.
    let duplicate = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sites",
            Some(&admin),
            Some(json!({ "key": "main", "name": "Main Site Again" })),
        ),
    )
    .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT);
    assert_eq!(duplicate.body["error"]["code"], "site_key_taken");

    // An unusable key is a bad request.
    let invalid = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sites",
            Some(&admin),
            Some(json!({ "key": "not a key", "name": "Bad" })),
        ),
    )
    .await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid.body["error"]["code"], "invalid_request");

    // Naming another tenant is refused; an unknown tenant is a 404.
    let cross = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sites",
            Some(&admin),
            Some(json!({ "organization_id": org_b, "key": "sneak", "name": "Sneak" })),
        ),
    )
    .await;
    assert_eq!(cross.status, StatusCode::FORBIDDEN);
    assert_eq!(cross.body["error"]["code"], "cross_organization");

    let unknown = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sites",
            Some(&platform),
            Some(json!({ "organization_id": Uuid::new_v4(), "key": "ghost", "name": "Ghost" })),
        ),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown.body["error"]["code"], "organization_not_found");

    // The list is scoped: the organization sees its own sites only.
    let listed = call(
        &fixture.state,
        request(Method::GET, "/api/v1/sites", Some(&admin), None),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK);
    let keys = field_of_all(&listed.body, "sites", "id");
    assert!(keys.contains(&site_id), "the own site is listed");
    assert!(
        !keys.contains(&foreign_site_id),
        "the other tenant's site is not: {}",
        listed.body
    );

    // Naming the other organization as a filter is refused too.
    let filtered = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/sites?organization_id={org_b}"),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(filtered.status, StatusCode::FORBIDDEN);
    assert_eq!(filtered.body["error"]["code"], "cross_organization");

    // Reading, editing and deleting the foreign site are all denied…
    for (method, uri) in [
        (Method::GET, format!("/api/v1/sites/{foreign_site_id}")),
        (Method::PATCH, format!("/api/v1/sites/{foreign_site_id}")),
        (Method::DELETE, format!("/api/v1/sites/{foreign_site_id}")),
        (
            Method::GET,
            format!("/api/v1/sites/{foreign_site_id}/domains"),
        ),
    ] {
        let denied = call(
            &fixture.state,
            request(
                method.clone(),
                &uri,
                Some(&admin),
                Some(json!({ "name": "Nope" })),
            ),
        )
        .await;
        assert_eq!(
            denied.status,
            StatusCode::FORBIDDEN,
            "{method} {uri}: {}",
            denied.body
        );
        assert_eq!(denied.body["error"]["code"], "cross_organization");
    }

    // … while the platform edits it, and the organization edits its own.
    let renamed = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/sites/{foreign_site_id}"),
            Some(&platform),
            Some(json!({ "name": "Company B Renamed" })),
        ),
    )
    .await;
    assert_eq!(
        renamed.status,
        StatusCode::OK,
        "platform patch: {}",
        renamed.body
    );
    assert_eq!(renamed.body["name"], "Company B Renamed");

    let archived = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/sites/{site_id}"),
            Some(&admin),
            Some(json!({ "status": "archived" })),
        ),
    )
    .await;
    assert_eq!(
        archived.status,
        StatusCode::OK,
        "own patch: {}",
        archived.body
    );
    assert_eq!(archived.body["status"], "archived");
    assert_eq!(archived.body["key"], "main", "the key does not move");

    // The change is audited inside the organization.
    let audit = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/iam/audit?organization_id={}", fixture.org_a),
            Some(&platform),
            None,
        ),
    )
    .await;
    let actions = field_of_all(&audit.body, "entries", "action");
    assert!(actions.contains(&"site.created".to_owned()), "{actions:?}");
    assert!(actions.contains(&"site.updated".to_owned()), "{actions:?}");

    // Deleting the own site is allowed; the foreign one stays.
    let deleted = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/sites/{site_id}"),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);

    let gone = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/sites/{site_id}"),
            Some(&platform),
            None,
        ),
    )
    .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    assert_eq!(gone.body["error"]["code"], "site_not_found");

    fixture.cleanup().await;
}

#[tokio::test]
async fn domains_are_platform_wide_unique_and_keep_one_primary() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let platform = fixture.platform_token().await;
    let admin = fixture.admin_token().await;
    let org_b = fixture.org_b;

    let site = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sites",
            Some(&admin),
            Some(json!({ "key": "shop", "name": "Shop" })),
        ),
    )
    .await;
    assert_eq!(site.status, StatusCode::CREATED, "site: {}", site.body);
    let site_id = id_of(&site.body);

    // The first host becomes the primary one, a second one does not.
    let suffix = Uuid::new_v4().simple().to_string();
    let second_host = format!("shop-{suffix}.tenancy.test");

    let first = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sites/{site_id}/domains"),
            Some(&admin),
            Some(json!({ "host": format!("WWW-{}.TENANCY.TEST", Uuid::new_v4().simple()) })),
        ),
    )
    .await;
    assert_eq!(
        first.status,
        StatusCode::CREATED,
        "first host: {}",
        first.body
    );
    assert_eq!(first.body["is_primary"], true, "the first host is primary");
    assert!(
        first.body["host"]
            .as_str()
            .expect("host")
            .chars()
            .all(|c| !c.is_ascii_uppercase()),
        "hosts are stored lowercase: {}",
        first.body["host"]
    );
    let first_host = first.body["host"].as_str().expect("host").to_owned();

    let second = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sites/{site_id}/domains"),
            Some(&admin),
            Some(json!({ "host": second_host })),
        ),
    )
    .await;
    assert_eq!(
        second.status,
        StatusCode::CREATED,
        "second host: {}",
        second.body
    );
    assert_eq!(second.body["is_primary"], false);
    let second_id = id_of(&second.body);

    // The list is primary first.
    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/sites/{site_id}/domains"),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "domains: {}", listed.body);
    let hosts = field_of_all(&listed.body, "domains", "host");
    assert_eq!(hosts, vec![first_host.clone(), second_host.clone()]);

    // Promotion moves the flag.
    let promoted = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sites/{site_id}/domains/{second_id}/primary"),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(
        promoted.status,
        StatusCode::OK,
        "promote: {}",
        promoted.body
    );
    assert_eq!(promoted.body["is_primary"], true);

    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/sites/{site_id}/domains"),
            Some(&admin),
            None,
        ),
    )
    .await;
    let hosts = field_of_all(&listed.body, "domains", "host");
    assert_eq!(hosts, vec![second_host.clone(), first_host.clone()]);
    let primaries: Vec<bool> = listed.body["domains"]
        .as_array()
        .expect("domains array")
        .iter()
        .map(|domain| domain["is_primary"] == true)
        .collect();
    assert_eq!(primaries, vec![true, false], "exactly one primary");

    // The host is unique platform-wide: the other tenant cannot take it.
    let foreign_site = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sites",
            Some(&platform),
            Some(json!({ "organization_id": org_b, "key": "company", "name": "Company B" })),
        ),
    )
    .await;
    let foreign_site_id = id_of(&foreign_site.body);

    let stolen = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sites/{foreign_site_id}/domains"),
            Some(&platform),
            Some(json!({ "host": second_host })),
        ),
    )
    .await;
    assert_eq!(stolen.status, StatusCode::CONFLICT);
    assert_eq!(stolen.body["error"]["code"], "domain_taken");

    // A host the site does not carry cannot be promoted or removed.
    let not_there = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sites/{site_id}/domains/{}/primary", Uuid::new_v4()),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(not_there.status, StatusCode::NOT_FOUND);
    assert_eq!(not_there.body["error"]["code"], "domain_not_found");

    // Removing the primary host promotes the remaining one.
    let removed = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/sites/{site_id}/domains/{second_id}"),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);

    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/sites/{site_id}/domains"),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(listed.body["domains"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed.body["domains"][0]["host"], first_host);
    assert_eq!(
        listed.body["domains"][0]["is_primary"], true,
        "the remaining host takes over: {}",
        listed.body
    );

    // The host resolves back to its site — the routing primitive the public renderer uses.
    let resolved = sites::find_site_by_host(fixture.db.pool(), &format!(" {} ", first_host))
        .await
        .expect("host lookup must run");
    assert_eq!(
        resolved.map(|site| site.id),
        Some(Uuid::parse_str(&site_id).expect("site id")),
        "the host belongs to the site"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn site_scoped_bindings_must_name_a_real_site_of_the_same_organization() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let platform = fixture.platform_token().await;
    let org_b = fixture.org_b;

    // A site of the second organization.
    let foreign_site = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sites",
            Some(&platform),
            Some(json!({ "organization_id": org_b, "key": "main", "name": "Company B" })),
        ),
    )
    .await;
    let foreign_site_id = id_of(&foreign_site.body);

    // A platform role can be bound at a site scope — but only to a site of that scope.
    let member_role = role_store::find_role_by_key(fixture.db.pool(), None, "member")
        .await
        .expect("role lookup must run")
        .expect("the member role exists after the seed");

    let mismatch = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/bindings",
            Some(&platform),
            Some(json!({
                "user_id": fixture.admin_id,
                "role_id": member_role.id,
                "scope_type": "site",
                "organization_id": fixture.org_a,
                "site_id": foreign_site_id,
            })),
        ),
    )
    .await;
    assert_eq!(
        mismatch.status,
        StatusCode::BAD_REQUEST,
        "mismatch: {}",
        mismatch.body
    );
    assert_eq!(mismatch.body["error"]["code"], "invalid_request");

    let unknown = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/bindings",
            Some(&platform),
            Some(json!({
                "user_id": fixture.admin_id,
                "role_id": member_role.id,
                "scope_type": "site",
                "organization_id": org_b,
                "site_id": Uuid::new_v4(),
            })),
        ),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    assert_eq!(unknown.body["error"]["code"], "invalid_request");

    // The matching case still works and lands on the account.
    let granted = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/bindings",
            Some(&platform),
            Some(json!({
                "user_id": fixture.admin_id,
                "role_id": member_role.id,
                "scope_type": "site",
                "organization_id": org_b,
                "site_id": foreign_site_id,
            })),
        ),
    )
    .await;
    assert_eq!(
        granted.status,
        StatusCode::CREATED,
        "grant: {}",
        granted.body
    );
    assert_eq!(granted.body["scope"]["type"], "site");
    assert_eq!(granted.body["scope"]["site_id"], foreign_site_id);

    fixture.cleanup().await;
}
