//! Integration tests for the public surface: the unauthenticated read route the public site
//! renderer (`apps/web`) consumes, the way a request resolves its site, and the guarantee that
//! drafts never leave the panel (phase P07, docs/05-VERSIONING.md §6).
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
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// Permission keys the content editor of this suite holds.
const CONTENT_PERMISSIONS: [&str; 7] = [
    "content.pages.read",
    "content.pages.create",
    "content.pages.update",
    "content.pages.delete",
    "content.pages.publish",
    "content.pages.schedule",
    "content.pages.restore",
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

/// Build a request; `token` becomes the session cookie, `extra_headers` are added on top and
/// `body` is the JSON payload.
fn request(
    method: Method,
    uri: &str,
    token: Option<&str>,
    extra_headers: &[(&str, &str)],
    body: Option<Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);

    if let Some(token) = token {
        builder = builder.header(header::COOKIE, format!("omnion_session={token}"));
    }
    for (name, value) in extra_headers {
        builder = builder.header(*name, *value);
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

/// Two organizations with one site each — keys and hosts unique per run, so parallel suites
/// cannot collide — plus a platform Owner and a content editor of the first organization.
struct Fixture {
    state: AppState,
    db: Db,
    platform_email: String,
    editor_email: String,
    site_a: Uuid,
    site_a_key: String,
    site_a_domain: String,
    site_b: Uuid,
    site_b_key: String,
    site_b_domain: String,
    organization_ids: Vec<Uuid>,
    account_ids: Vec<Uuid>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let suffix = Uuid::new_v4().simple().to_string();
        let org_a = create_organization_row(&db, "a", "Public Test A").await;
        let org_b = create_organization_row(&db, "b", "Public Test B").await;

        let site_a_key = format!("pub-a-{}", &suffix[..8]);
        let site_b_key = format!("pub-b-{}", &suffix[..8]);
        let site_a = create_site_row(&db, org_a, &site_a_key, "Public Site A").await;
        let site_b = create_site_row(&db, org_b, &site_b_key, "Public Site B").await;

        let site_a_domain = format!("pub-a-{}.omnion.test", &suffix[..12]);
        let site_b_domain = format!("pub-b-{}.omnion.test", &suffix[..12]);
        add_domain_row(&db, site_a, &site_a_domain, true).await;
        add_domain_row(&db, site_b, &site_b_domain, true).await;

        // The platform Owner: no primary organization, so it may work across tenants.
        let (platform_id, platform_email) = create_account(&db, None).await;
        seed::bind_owner(db.pool(), platform_id)
            .await
            .expect("the owner binding must be created");

        // The editor: the content keys bound at organization scope.
        let (editor_id, editor_email) = create_account(&db, Some(org_a)).await;
        let role = role_store::create_role(
            db.pool(),
            NewRole {
                organization_id: org_a,
                key: format!("public-editor-{}", Uuid::new_v4().simple()),
                name: "Content Editor".to_owned(),
                description: "Writes and publishes the content of one organization".to_owned(),
                priority: 400,
                inherits_role_id: None,
            },
        )
        .await
        .expect("the organization role must be created");

        let entries: Vec<RolePermissionInput> = CONTENT_PERMISSIONS
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
            user_id: editor_id,
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

        Some(Self {
            state,
            db,
            platform_email,
            editor_email,
            site_a,
            site_a_key,
            site_a_domain,
            site_b,
            site_b_key,
            site_b_domain,
            organization_ids: vec![org_a, org_b],
            account_ids: vec![platform_id, editor_id],
        })
    }

    /// The platform Owner, signed in.
    async fn platform_token(&self) -> String {
        login(&self.state, &self.platform_email).await
    }

    /// The content editor of the first organization, signed in.
    async fn editor_token(&self) -> String {
        login(&self.state, &self.editor_email).await
    }

    /// Remove exactly what this fixture created.
    async fn cleanup(&self) {
        sqlx::query("delete from users where id = any($1)")
            .bind(&self.account_ids)
            .execute(self.db.pool())
            .await
            .expect("account cleanup must run");
        sqlx::query("delete from organizations where id = any($1)")
            .bind(&self.organization_ids)
            .execute(self.db.pool())
            .await
            .expect("organization cleanup must run");
    }
}

/// Create an organization row with a unique, suite-scoped slug.
async fn create_organization_row(db: &Db, label: &str, name: &str) -> Uuid {
    let slug = format!("public-fix-{label}-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(name)
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// Create a site row inside an organization.
async fn create_site_row(db: &Db, organization_id: Uuid, key: &str, name: &str) -> Uuid {
    sqlx::query_scalar(
        "insert into sites (organization_id, key, name) values ($1, $2, $3) returning id",
    )
    .bind(organization_id)
    .bind(key)
    .bind(name)
    .fetch_one(db.pool())
    .await
    .expect("the test site must be created")
}

/// Register a host on a site — the routing primitive the public surface resolves.
async fn add_domain_row(db: &Db, site_id: Uuid, host: &str, is_primary: bool) {
    sqlx::query("insert into site_domains (site_id, host, is_primary) values ($1, $2, $3)")
        .bind(site_id)
        .bind(host)
        .bind(is_primary)
        .execute(db.pool())
        .await
        .expect("the test domain must be created");
}

/// Create an account with a unique address so parallel runs cannot collide.
async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("public-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Public Test".to_owned(),
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
            &[],
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
        .expect("login must set the session cookie")
        .split(';')
        .next()
        .expect("cookie has a value")
        .split_once('=')
        .expect("cookie is name=value")
        .1
        .to_owned()
}

/// Create a page and publish it; returns the page id.
async fn publish_page(
    state: &AppState,
    token: &str,
    site_id: Uuid,
    slug: &str,
    title: &str,
    body: &str,
) -> String {
    let created = call(
        state,
        request(
            Method::POST,
            "/api/v1/pages",
            Some(token),
            &[],
            Some(json!({ "site_id": site_id, "slug": slug, "title": title, "body": body })),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "create: {}",
        created.body
    );
    let page_id = created.body["id"]
        .as_str()
        .expect("the created page carries an id")
        .to_owned();

    let published = call(
        state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page_id}/publish"),
            Some(token),
            &[],
            None,
        ),
    )
    .await;
    assert_eq!(
        published.status,
        StatusCode::OK,
        "publish: {}",
        published.body
    );
    page_id
}

/// The public read of one address, anonymously.
async fn public_read(
    state: &AppState,
    slug: &str,
    query: &str,
    extra_headers: &[(&str, &str)],
) -> TestResponse {
    call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/public/pages/{slug}{query}"),
            None,
            extra_headers,
            None,
        ),
    )
    .await
}

#[tokio::test]
async fn only_published_pages_answer_and_drafts_stay_private() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;
    let hint = format!("?site={}", fixture.site_a_key);

    // A fresh draft is invisible to the public surface — and it answers with the very same 404
    // an unknown address gets, so nothing discloses that a draft exists.
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/pages",
            Some(&editor),
            &[],
            Some(json!({
                "site_id": fixture.site_a,
                "slug": "home",
                "title": "Welcome",
                "body": "Our first page.",
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let page_id = created.body["id"]
        .as_str()
        .expect("the created page carries an id")
        .to_owned();

    let draft_read = public_read(&fixture.state, "home", &hint, &[]).await;
    assert_eq!(draft_read.status, StatusCode::NOT_FOUND);
    assert_eq!(draft_read.body["error"]["code"], "page_not_found");

    // Publishing makes exactly the published revision readable — anonymously, without a cookie.
    let published = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page_id}/publish"),
            Some(&editor),
            &[],
            None,
        ),
    )
    .await;
    assert_eq!(published.status, StatusCode::OK, "{}", published.body);

    let live = public_read(&fixture.state, "home", &hint, &[]).await;
    assert_eq!(live.status, StatusCode::OK, "live: {}", live.body);
    assert_eq!(live.body["site"]["key"], fixture.site_a_key.as_str());
    assert_eq!(live.body["site"]["name"], "Public Site A");
    assert_eq!(live.body["page"]["slug"], "home");
    assert_eq!(live.body["page"]["page_type"], "page");
    assert_eq!(live.body["revision"]["revision_no"], 1);
    assert_eq!(live.body["revision"]["title"], "Welcome");
    assert_eq!(live.body["revision"]["body"], "Our first page.");
    assert_eq!(live.body["revision"]["summary"], Value::Null);
    assert!(
        live.body["revision"]["published_at"].is_string(),
        "a published revision carries its publish time: {}",
        live.body
    );
    // The public shape carries no internal identifiers at all.
    assert!(live.body["site"].get("id").is_none());
    assert!(live.body["page"].get("id").is_none());
    assert!(live.body["revision"].get("id").is_none());

    // A further edit sits in the panel: visitors keep seeing revision 1 until it is published.
    let edited = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            &[],
            Some(json!({ "title": "Welcome to Omnion", "body": "Second take." })),
        ),
    )
    .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.body);

    let still_v1 = public_read(&fixture.state, "home", &hint, &[]).await;
    assert_eq!(still_v1.body["revision"]["revision_no"], 1);
    assert_eq!(still_v1.body["revision"]["title"], "Welcome");
    assert_eq!(still_v1.body["revision"]["body"], "Our first page.");

    let published = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page_id}/publish"),
            Some(&editor),
            &[],
            None,
        ),
    )
    .await;
    assert_eq!(published.status, StatusCode::OK, "{}", published.body);

    let v2 = public_read(&fixture.state, "home", &hint, &[]).await;
    assert_eq!(v2.body["revision"]["revision_no"], 2);
    assert_eq!(v2.body["revision"]["title"], "Welcome to Omnion");
    assert_eq!(v2.body["revision"]["body"], "Second take.");

    // Unknown addresses and shapes that are not slugs answer 404 — never a 400 that would leak
    // the panel's validation rules to a visitor.
    for slug in ["missing", "bad_slug"] {
        let not_found = public_read(&fixture.state, slug, &hint, &[]).await;
        assert_eq!(
            not_found.status,
            StatusCode::NOT_FOUND,
            "{slug}: {}",
            not_found.body
        );
        assert_eq!(not_found.body["error"]["code"], "page_not_found");
    }

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_request_resolves_its_site_by_domain_or_key() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;
    let platform = fixture.platform_token().await;

    // One published page per tenant, so a wrong resolution cannot pass unnoticed.
    publish_page(
        &fixture.state,
        &editor,
        fixture.site_a,
        "home",
        "Tenant A Home",
        "Body of A.",
    )
    .await;
    publish_page(
        &fixture.state,
        &platform,
        fixture.site_b,
        "home",
        "Tenant B Home",
        "Body of B.",
    )
    .await;

    // The site's own domain resolves through the Host header.
    let by_host = public_read(
        &fixture.state,
        "home",
        "",
        &[("host", fixture.site_a_domain.as_str())],
    )
    .await;
    assert_eq!(by_host.status, StatusCode::OK, "by host: {}", by_host.body);
    assert_eq!(by_host.body["site"]["key"], fixture.site_a_key.as_str());
    assert_eq!(by_host.body["revision"]["title"], "Tenant A Home");

    // … and the other tenant's domain serves the other tenant's page.
    let by_other_host = public_read(
        &fixture.state,
        "home",
        "",
        &[("host", fixture.site_b_domain.as_str())],
    )
    .await;
    assert_eq!(
        by_other_host.status,
        StatusCode::OK,
        "{}",
        by_other_host.body
    );
    assert_eq!(
        by_other_host.body["site"]["key"],
        fixture.site_b_key.as_str()
    );
    assert_eq!(by_other_host.body["revision"]["title"], "Tenant B Home");

    // A proxy in front sets X-Forwarded-Host, and it wins over the internal Host.
    let forwarded = public_read(
        &fixture.state,
        "home",
        "",
        &[
            ("host", "internal:8080"),
            ("x-forwarded-host", fixture.site_a_domain.as_str()),
        ],
    )
    .await;
    assert_eq!(
        forwarded.status,
        StatusCode::OK,
        "forwarded: {}",
        forwarded.body
    );
    assert_eq!(forwarded.body["site"]["key"], fixture.site_a_key.as_str());

    // `?site=` takes a key as well as a host — what a renderer that calls the API directly
    // (and has no visitor host of its own) sends.
    for hint in [fixture.site_b_key.clone(), fixture.site_b_domain.clone()] {
        let resolved = public_read(&fixture.state, "home", &format!("?site={hint}"), &[]).await;
        assert_eq!(
            resolved.status,
            StatusCode::OK,
            "hint {hint}: {}",
            resolved.body
        );
        assert_eq!(resolved.body["site"]["key"], fixture.site_b_key.as_str());
        assert_eq!(resolved.body["revision"]["title"], "Tenant B Home");
    }

    // With more than one site the request has to address one: an unknown hint, an unregistered
    // host and no hint at all are all a 404 instead of a guess.
    let unknown_hint = public_read(&fixture.state, "home", "?site=no-such-site", &[]).await;
    assert_eq!(unknown_hint.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown_hint.body["error"]["code"], "site_not_found");

    let unknown_host = public_read(
        &fixture.state,
        "home",
        "",
        &[("host", "unregistered.omnion.test")],
    )
    .await;
    assert_eq!(unknown_host.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown_host.body["error"]["code"], "site_not_found");

    let no_hint = public_read(&fixture.state, "home", "", &[]).await;
    assert_eq!(no_hint.status, StatusCode::NOT_FOUND);
    assert_eq!(no_hint.body["error"]["code"], "site_not_found");

    fixture.cleanup().await;
}
