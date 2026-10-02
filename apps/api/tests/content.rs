//! Integration tests for the content surface: pages, their revision history, publish and
//! restore, and the translation rows of a revision (docs/05-VERSIONING.md §4–§7,
//! docs/01-VISION.md §5, §7, phase P05).
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

/// Two organizations with one site each, a platform Owner, an editor of the first organization
/// that holds the content keys and a member without any.
///
/// Every row carries a `content-` prefix or a random address, and cleanup removes exactly the
/// rows this fixture created — by id, never by pattern, so parallel suites cannot collide.
struct Fixture {
    state: AppState,
    db: Db,
    platform_email: String,
    org_a: Uuid,
    site_a: Uuid,
    site_b: Uuid,
    editor_email: String,
    member_email: String,
    accounts: Vec<Uuid>,
    organizations: Vec<Uuid>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let org_a = create_organization_row(&db, "a", "Content Test A").await;
        let org_b = create_organization_row(&db, "b", "Content Test B").await;
        let site_a = create_site_row(&db, org_a, "main", "Content Site A").await;
        let site_b = create_site_row(&db, org_b, "main", "Content Site B").await;

        // The platform Owner: no primary organization, so it may work across tenants.
        let (platform_id, platform_email) = create_account(&db, None).await;
        seed::bind_owner(db.pool(), platform_id)
            .await
            .expect("the owner binding must be created");

        // The editor: the content keys bound at organization scope.
        let (admin_id, editor_email) = create_account(&db, Some(org_a)).await;
        let role = role_store::create_role(
            db.pool(),
            NewRole {
                organization_id: org_a,
                key: format!("content-editor-{}", Uuid::new_v4().simple()),
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

        // A plain member of the same organization, without a content permission.
        let (member_id, member_email) = create_account(&db, Some(org_a)).await;

        Some(Self {
            state,
            db,
            platform_email,
            org_a,
            site_a,
            site_b,
            editor_email,
            member_email,
            accounts: vec![platform_id, admin_id, member_id],
            organizations: vec![org_a, org_b],
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

    /// The plain member of the first organization, signed in.
    async fn member_token(&self) -> String {
        login(&self.state, &self.member_email).await
    }

    /// Remove exactly what this fixture created.
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
    let slug = format!("content-fix-{label}-{}", Uuid::new_v4().simple());
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

/// Create an account with a unique address so parallel runs cannot collide.
async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("content-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Content Test".to_owned(),
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
async fn the_pages_surface_is_permission_gated() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let member = fixture.member_token().await;
    let editor = fixture.editor_token().await;

    // Without a session nothing answers.
    let anonymous = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages?site_id={}", fixture.site_a),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    let anonymous_write = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/pages",
            None,
            Some(json!({ "site_id": fixture.site_a, "slug": "home", "title": "Home" })),
        ),
    )
    .await;
    assert_eq!(anonymous_write.status, StatusCode::UNAUTHORIZED);

    // A member of the organization holds no content permission.
    let denied = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages?site_id={}", fixture.site_a),
            Some(&member),
            None,
        ),
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(denied.body["error"]["code"], "permission_denied");

    let denied_create = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/pages",
            Some(&member),
            Some(json!({ "site_id": fixture.site_a, "slug": "home", "title": "Home" })),
        ),
    )
    .await;
    assert_eq!(denied_create.status, StatusCode::FORBIDDEN);

    // The editor holds them at organization scope.
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/pages",
            Some(&editor),
            Some(json!({
                "site_id": fixture.site_a,
                "slug": "  Home  ",
                "title": "Welcome",
                "body": "Our first page.",
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
    assert_eq!(created.body["slug"], "home", "slugs normalize to lowercase");
    assert_eq!(created.body["page_type"], "page");
    assert_eq!(created.body["status"], "draft");
    assert_eq!(created.body["published_revision_id"], Value::Null);
    assert_eq!(created.body["published"], Value::Null);
    assert_eq!(created.body["draft"]["revision_no"], 1);
    assert_eq!(created.body["draft"]["state"], "draft");
    assert_eq!(created.body["draft"]["title"], "Welcome");
    let page_id = id_of(&created.body);

    // The page reads back; the member is refused the read too.
    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "read: {}", read.body);
    assert_eq!(read.body["id"], page_id);

    let member_read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}"),
            Some(&member),
            None,
        ),
    )
    .await;
    assert_eq!(member_read.status, StatusCode::FORBIDDEN);

    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages?site_id={}", fixture.site_a),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "list: {}", listed.body);
    assert_eq!(field_of_all(&listed.body, "pages", "id"), vec![page_id]);

    fixture.cleanup().await;
}

#[tokio::test]
async fn editing_appends_a_revision_and_publishing_freezes_it() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/pages",
            Some(&editor),
            Some(json!({
                "site_id": fixture.site_a,
                "slug": "welcome",
                "title": "Welcome",
                "body": "A",
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let page_id = id_of(&created.body);
    let first_revision = id_of(&created.body["draft"]);

    // A second page with the same slug is refused inside the site.
    let duplicate = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/pages",
            Some(&editor),
            Some(json!({ "site_id": fixture.site_a, "slug": "welcome", "title": "Again" })),
        ),
    )
    .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT);
    assert_eq!(duplicate.body["error"]["code"], "slug_taken");

    // Unusable input is a bad request.
    let invalid_slug = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/pages",
            Some(&editor),
            Some(json!({ "site_id": fixture.site_a, "slug": "Not A Slug", "title": "Nope" })),
        ),
    )
    .await;
    assert_eq!(invalid_slug.status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid_slug.body["error"]["code"], "invalid_request");

    let blank_title = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/pages",
            Some(&editor),
            Some(json!({ "site_id": fixture.site_a, "slug": "blank", "title": "   " })),
        ),
    )
    .await;
    assert_eq!(blank_title.status, StatusCode::BAD_REQUEST);

    // Editing content appends revision 2 and archives the draft it supersedes.
    let edited = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "title": "Welcome to Omnion", "body": "B" })),
        ),
    )
    .await;
    assert_eq!(edited.status, StatusCode::OK, "edit: {}", edited.body);
    assert_eq!(edited.body["draft"]["revision_no"], 2);
    assert_eq!(edited.body["draft"]["title"], "Welcome to Omnion");
    assert_eq!(edited.body["draft"]["body"], "B");
    assert_eq!(
        edited.body["published"],
        Value::Null,
        "an edit does not become visible"
    );

    let history = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}/revisions"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(history.status, StatusCode::OK, "history: {}", history.body);
    let numbers: Vec<i64> = history.body["revisions"]
        .as_array()
        .expect("revisions array")
        .iter()
        .map(|revision| revision["revision_no"].as_i64().expect("number"))
        .collect();
    assert_eq!(numbers, vec![2, 1], "newest first");
    let states = field_of_all(&history.body, "revisions", "state");
    assert_eq!(states, vec!["draft", "archived"]);

    // Publishing freezes the draft: the page now serves revision 2.
    let published = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page_id}/publish"),
            Some(&editor),
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
    assert_eq!(published.body["status"], "published");
    assert_eq!(
        published.body["draft"],
        Value::Null,
        "the draft is consumed"
    );
    assert_eq!(published.body["published"]["revision_no"], 2);
    assert_eq!(published.body["published"]["title"], "Welcome to Omnion");
    let published_revision = id_of(&published.body["published"]);
    assert_eq!(published.body["published_revision_id"], published_revision);

    // Publishing again has nothing to publish.
    let republish = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page_id}/publish"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(republish.status, StatusCode::CONFLICT);
    assert_eq!(republish.body["error"]["code"], "no_draft_revision");

    // A further edit becomes revision 3 and stays invisible until published.
    let edited_again = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "body": "C" })),
        ),
    )
    .await;
    assert_eq!(edited_again.status, StatusCode::OK);
    assert_eq!(edited_again.body["draft"]["revision_no"], 3);
    assert_eq!(edited_again.body["draft"]["body"], "C");
    assert_eq!(
        edited_again.body["published"]["revision_no"], 2,
        "visitors still see revision 2"
    );

    // The lifecycle filter reads the page's status: this page is published — it simply has an
    // unpublished draft of its own (docs/05-VERSIONING.md §6: "Published: v12, Draft: v13").
    let published_pages = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages?site_id={}&status=published", fixture.site_a),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(
        published_pages.body["pages"].as_array().map(Vec::len),
        Some(1),
        "the page is live: {}",
        published_pages.body
    );

    let drafts = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages?site_id={}&status=draft", fixture.site_a),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(
        drafts.body["pages"].as_array().map(Vec::len),
        Some(0),
        "a page that has gone live is not a draft"
    );

    let archived = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages?site_id={}&status=archived", fixture.site_a),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(
        archived.body["pages"].as_array().map(Vec::len),
        Some(0),
        "pages, not revisions, are filtered"
    );

    // The history of the page is in the audit trail of its organization (the trail itself
    // needs `audit.read`, which the platform Owner holds).
    let platform = fixture.platform_token().await;
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
    assert!(actions.contains(&"page.created".to_owned()), "{actions:?}");
    assert!(actions.contains(&"page.updated".to_owned()), "{actions:?}");
    assert!(
        actions.contains(&"page.published".to_owned()),
        "{actions:?}"
    );

    // The first revision is still in the history, archived and untouched.
    let first = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}/revisions/{first_revision}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "first: {}", first.body);
    assert_eq!(first.body["state"], "archived");
    assert_eq!(first.body["title"], "Welcome");
    assert_eq!(first.body["body"], "A");

    fixture.cleanup().await;
}

#[tokio::test]
async fn restoring_an_earlier_revision_brings_it_forward() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;

    // v1 «Welcome» goes live, then v2 changes it.
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/pages",
            Some(&editor),
            Some(json!({
                "site_id": fixture.site_a,
                "slug": "home",
                "title": "Welcome",
                "body": "A",
            })),
        ),
    )
    .await;
    let page_id = id_of(&created.body);
    let v1 = id_of(&created.body["draft"]);

    let published = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page_id}/publish"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(published.status, StatusCode::OK);

    let edited = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "title": "Welcome to Omnion", "body": "B" })),
        ),
    )
    .await;
    assert_eq!(edited.body["draft"]["revision_no"], 2);

    let published = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page_id}/publish"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(published.status, StatusCode::OK);
    assert_eq!(published.body["published"]["revision_no"], 2);

    // Restoring v1 copies it forward as revision 3 — the history itself does not move.
    let restored = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page_id}/restore"),
            Some(&editor),
            Some(json!({ "revision_id": v1 })),
        ),
    )
    .await;
    assert_eq!(
        restored.status,
        StatusCode::CREATED,
        "restore: {}",
        restored.body
    );
    assert_eq!(restored.body["revision_no"], 3);
    assert_eq!(restored.body["state"], "draft");
    assert_eq!(restored.body["title"], "Welcome");
    assert_eq!(restored.body["body"], "A");
    assert_eq!(restored.body["restored_from_id"], v1);
    let v3 = id_of(&restored.body);

    // The restored draft is not live until it is published.
    let current = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(current.body["draft"]["revision_no"], 3);
    assert_eq!(
        current.body["published"]["revision_no"], 2,
        "the restore waits for a publish"
    );

    let published = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page_id}/publish"),
            Some(&editor),
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
    assert_eq!(published.body["published"]["revision_no"], 3);
    assert_eq!(published.body["published"]["title"], "Welcome");
    assert_eq!(published.body["published_revision_id"], v3);

    // The whole history is intact: 3 current, 2 archived, 1 archived and never rewritten.
    let history = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}/revisions"),
            Some(&editor),
            None,
        ),
    )
    .await;
    let numbers: Vec<i64> = history.body["revisions"]
        .as_array()
        .expect("revisions array")
        .iter()
        .map(|revision| revision["revision_no"].as_i64().expect("number"))
        .collect();
    assert_eq!(numbers, vec![3, 2, 1]);
    let states = field_of_all(&history.body, "revisions", "state");
    assert_eq!(states, vec!["published", "archived", "archived"]);

    let v1_again = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}/revisions/{v1}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(v1_again.body["state"], "archived");
    assert_eq!(v1_again.body["title"], "Welcome");
    assert_eq!(v1_again.body["body"], "A", "the original row never changed");

    // A revision that belongs to another page is not restorable through this one.
    let other = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/pages",
            Some(&editor),
            Some(json!({ "site_id": fixture.site_a, "slug": "about", "title": "About" })),
        ),
    )
    .await;
    let other_page = id_of(&other.body);
    let other_revision = id_of(&other.body["draft"]);

    let crossed = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page_id}/restore"),
            Some(&editor),
            Some(json!({ "revision_id": other_revision })),
        ),
    )
    .await;
    assert_eq!(crossed.status, StatusCode::NOT_FOUND);
    assert_eq!(crossed.body["error"]["code"], "revision_not_found");

    let unknown = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page_id}/restore"),
            Some(&editor),
            Some(json!({ "revision_id": Uuid::new_v4() })),
        ),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);

    // The revision of the other page is still its own draft.
    let other_read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{other_page}/revisions/{other_revision}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(other_read.body["state"], "draft");

    fixture.cleanup().await;
}

#[tokio::test]
async fn translations_are_rows_per_language_and_field() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/pages",
            Some(&editor),
            Some(json!({
                "site_id": fixture.site_a,
                "slug": "home",
                "title": "Welcome",
                "body": "A",
            })),
        ),
    )
    .await;
    let page_id = id_of(&created.body);
    let revision_id = id_of(&created.body["draft"]);

    // Turkish, written upper-case on the way in and normalized to lowercase.
    let turkish = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/pages/{page_id}/revisions/{revision_id}/translations/TR"),
            Some(&editor),
            Some(json!({ "title": "Merhaba", "body": "Gövde" })),
        ),
    )
    .await;
    assert_eq!(turkish.status, StatusCode::OK, "turkish: {}", turkish.body);
    assert_eq!(
        turkish.body["translations"].as_array().map(Vec::len),
        Some(2)
    );
    let languages = field_of_all(&turkish.body, "translations", "language");
    assert_eq!(languages, vec!["tr", "tr"]);

    let english = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/pages/{page_id}/revisions/{revision_id}/translations/en"),
            Some(&editor),
            Some(json!({ "title": "Welcome" })),
        ),
    )
    .await;
    assert_eq!(english.status, StatusCode::OK, "english: {}", english.body);
    assert_eq!(
        english.body["translations"].as_array().map(Vec::len),
        Some(3)
    );

    // Writing the same field again updates the row in place.
    let revised = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/pages/{page_id}/revisions/{revision_id}/translations/tr"),
            Some(&editor),
            Some(json!({ "title": "Merhaba Dünya" })),
        ),
    )
    .await;
    assert_eq!(revised.status, StatusCode::OK);
    assert_eq!(
        revised.body["translations"].as_array().map(Vec::len),
        Some(3),
        "an update does not add a row"
    );
    let titles: Vec<&str> = revised.body["translations"]
        .as_array()
        .expect("translations array")
        .iter()
        .filter(|row| row["field"] == "title" && row["language"] == "tr")
        .map(|row| row["value"].as_str().expect("value"))
        .collect();
    assert_eq!(titles, vec!["Merhaba Dünya"]);

    // The rows come back in a stable order: language, then field.
    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}/revisions/{revision_id}/translations"),
            Some(&editor),
            None,
        ),
    )
    .await;
    let pairs: Vec<(String, String)> = listed.body["translations"]
        .as_array()
        .expect("translations array")
        .iter()
        .map(|row| {
            (
                row["language"].as_str().expect("language").to_owned(),
                row["field"].as_str().expect("field").to_owned(),
            )
        })
        .collect();
    assert_eq!(
        pairs,
        vec![
            ("en".to_owned(), "title".to_owned()),
            ("tr".to_owned(), "body".to_owned()),
            ("tr".to_owned(), "title".to_owned()),
        ]
    );

    // Empty and unusable requests are refused.
    let empty = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/pages/{page_id}/revisions/{revision_id}/translations/de"),
            Some(&editor),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);
    assert_eq!(empty.body["error"]["code"], "empty_translation");

    let bad_language = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/pages/{page_id}/revisions/{revision_id}/translations/Türkçe"),
            Some(&editor),
            Some(json!({ "title": "Merhaba" })),
        ),
    )
    .await;
    assert_eq!(bad_language.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad_language.body["error"]["code"], "invalid_request");

    let unknown_revision = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!(
                "/api/v1/pages/{page_id}/revisions/{}/translations/en",
                Uuid::new_v4()
            ),
            Some(&editor),
            Some(json!({ "title": "Nope" })),
        ),
    )
    .await;
    assert_eq!(unknown_revision.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown_revision.body["error"]["code"], "revision_not_found");

    // Every row belongs to the organization the revision belongs to.
    let organizations: Vec<Uuid> = sqlx::query_scalar(
        "select organization_id from translations where resource_type = 'page_revision' \
         and resource_id = $1",
    )
    .bind(Uuid::parse_str(&revision_id).expect("revision id"))
    .fetch_all(fixture.db.pool())
    .await
    .expect("translation lookup must run");
    assert_eq!(organizations.len(), 3);
    assert!(
        organizations.iter().all(|org| *org == fixture.org_a),
        "translations land on the revision's own tenant"
    );

    // Another tenant's revision is out of reach for the editor.
    let foreign = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/pages",
            Some(&fixture.platform_token().await),
            Some(json!({
                "site_id": fixture.site_b,
                "slug": "home",
                "title": "Company",
            })),
        ),
    )
    .await;
    assert_eq!(foreign.status, StatusCode::CREATED, "{}", foreign.body);
    let foreign_page = id_of(&foreign.body);
    let foreign_revision = id_of(&foreign.body["draft"]);

    let crossed = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/pages/{foreign_page}/revisions/{foreign_revision}/translations/tr"),
            Some(&editor),
            Some(json!({ "title": "Şirket" })),
        ),
    )
    .await;
    assert_eq!(crossed.status, StatusCode::FORBIDDEN);
    assert_eq!(crossed.body["error"]["code"], "cross_organization");

    fixture.cleanup().await;
}

#[tokio::test]
async fn pages_stay_inside_their_organization() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let platform = fixture.platform_token().await;
    let editor = fixture.editor_token().await;

    // A page of the second tenant, created by the platform.
    let foreign = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/pages",
            Some(&platform),
            Some(json!({
                "site_id": fixture.site_b,
                "slug": "company",
                "title": "Company B",
            })),
        ),
    )
    .await;
    assert_eq!(
        foreign.status,
        StatusCode::CREATED,
        "foreign: {}",
        foreign.body
    );
    let foreign_page = id_of(&foreign.body);

    // Reading, listing, editing, publishing, restoring and deleting are all denied.
    for (method, uri) in [
        (Method::GET, format!("/api/v1/pages/{foreign_page}")),
        (Method::PATCH, format!("/api/v1/pages/{foreign_page}")),
        (Method::DELETE, format!("/api/v1/pages/{foreign_page}")),
        (
            Method::POST,
            format!("/api/v1/pages/{foreign_page}/publish"),
        ),
        (
            Method::GET,
            format!("/api/v1/pages/{foreign_page}/revisions"),
        ),
        (
            Method::GET,
            format!("/api/v1/pages?site_id={}", fixture.site_b),
        ),
    ] {
        let denied = call(
            &fixture.state,
            request(
                method.clone(),
                &uri,
                Some(&editor),
                Some(json!({ "title": "Nope" })),
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

    // The editor's own page works end to end, and deleting it cleans up after itself.
    let own = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/pages",
            Some(&editor),
            Some(json!({
                "site_id": fixture.site_a,
                "slug": "temporary",
                "title": "Temporary",
                "summary": "Gone soon.",
            })),
        ),
    )
    .await;
    assert_eq!(own.status, StatusCode::CREATED, "own: {}", own.body);
    let own_page = id_of(&own.body);
    let own_revision = id_of(&own.body["draft"]);

    let translated = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/pages/{own_page}/revisions/{own_revision}/translations/tr"),
            Some(&editor),
            Some(json!({ "title": "Geçici" })),
        ),
    )
    .await;
    assert_eq!(translated.status, StatusCode::OK, "{}", translated.body);

    let removed = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/pages/{own_page}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);

    let gone = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{own_page}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    assert_eq!(gone.body["error"]["code"], "page_not_found");

    // Deleting the page removed its revisions and their translation rows with it.
    let orphaned: i64 = sqlx::query_scalar(
        "select count(*) from translations where resource_type = 'page_revision' \
         and resource_id = $1",
    )
    .bind(Uuid::parse_str(&own_revision).expect("revision id"))
    .fetch_one(fixture.db.pool())
    .await
    .expect("translation lookup must run");
    assert_eq!(orphaned, 0, "no translation row outlives its revision");

    // The page list of a foreign site stays closed even for the platform's own page.
    let platform_list = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages?site_id={}", fixture.site_b),
            Some(&platform),
            None,
        ),
    )
    .await;
    assert_eq!(platform_list.status, StatusCode::OK);
    assert_eq!(
        field_of_all(&platform_list.body, "pages", "id"),
        vec![foreign_page]
    );

    fixture.cleanup().await;
}
