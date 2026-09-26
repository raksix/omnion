//! Integration tests for the search surface: the one search box and its source registry
//! (docs/requests/REQ-002).
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) — locally and in CI. When
//! PostgreSQL is not reachable the suite skips itself with a printed reason.
//!
//! What the walks prove, in the words of the acceptance criteria: the answer is grouped per
//! source; the result set is exactly what the caller's read permissions and their tenancy allow
//! (a page of another organization is invisible, a source the caller cannot read is reported as
//! skipped); every term must match; the source filter and the per-group limit are honoured; and
//! a page renamed in its latest revision is still found by the title it carried before.

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

/// The read keys the search editor of this suite holds: content and media, but not sites.
const EDITOR_PERMISSIONS: [&str; 2] = ["content.pages.read", "media.read"];

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

/// Object store of the test state; search never touches it.
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
/// that holds the content + media read keys, and a member without any.
///
/// Every row carries a `search-` prefix or a random address, and cleanup removes exactly the
/// rows this fixture created — by id, never by pattern, so parallel suites cannot collide.
struct Fixture {
    state: AppState,
    db: Db,
    platform_email: String,
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

        let org_a = create_organization_row(&db, "a", "Search Test A").await;
        let org_b = create_organization_row(&db, "b", "Search Test B").await;
        let site_a = create_site_row(&db, org_a, "main", "Search Site A").await;
        let site_b = create_site_row(&db, org_b, "main", "Search Site B").await;

        let (platform_id, platform_email) = create_account(&db, None).await;
        seed::bind_owner(db.pool(), platform_id)
            .await
            .expect("the owner binding must be created");

        let (editor_id, editor_email) = create_account(&db, Some(org_a)).await;
        let role = role_store::create_role(
            db.pool(),
            NewRole {
                organization_id: org_a,
                key: format!("search-editor-{}", Uuid::new_v4().simple()),
                name: "Search Editor".to_owned(),
                description: "Reads the content and media of one organization".to_owned(),
                priority: 400,
                inherits_role_id: None,
            },
        )
        .await
        .expect("the organization role must be created");

        let entries: Vec<RolePermissionInput> = EDITOR_PERMISSIONS
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

        let (member_id, member_email) = create_account(&db, Some(org_a)).await;

        Some(Self {
            state,
            db,
            platform_email,
            site_a,
            site_b,
            editor_email,
            member_email,
            accounts: vec![platform_id, editor_id, member_id],
            organizations: vec![org_a, org_b],
        })
    }

    /// The platform Owner, signed in.
    async fn platform_token(&self) -> String {
        login(&self.state, &self.platform_email).await
    }

    /// The editor of the first organization, signed in.
    async fn editor_token(&self) -> String {
        login(&self.state, &self.editor_email).await
    }

    /// The plain member of the first organization, signed in.
    async fn member_token(&self) -> String {
        login(&self.state, &self.member_email).await
    }

    /// Remove exactly what this fixture created: the organizations cascade into sites, pages,
    /// revisions and media rows.
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
    let slug = format!("search-fix-{label}-{}", Uuid::new_v4().simple());
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
    let email = format!("search-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Search Test".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("creating a test account must succeed");
    (user.id, email)
}

/// Insert a page with one draft revision and return the page id.
async fn create_page(
    db: &Db,
    site_id: Uuid,
    slug: &str,
    title: &str,
    summary: Option<&str>,
) -> Uuid {
    let page_id: Uuid = sqlx::query_scalar(
        "insert into pages (site_id, slug, status) values ($1, $2, 'draft') returning id",
    )
    .bind(site_id)
    .bind(slug)
    .fetch_one(db.pool())
    .await
    .expect("the test page must be created");

    sqlx::query(
        "insert into page_revisions (page_id, revision_no, state, title, summary) \
         values ($1, 1, 'draft', $2, $3)",
    )
    .bind(page_id)
    .bind(title)
    .bind(summary)
    .execute(db.pool())
    .await
    .expect("the draft revision must be written");

    page_id
}

/// Append one more revision (the working draft) to a page and return its id.
async fn append_revision(db: &Db, page_id: Uuid, revision_no: i32, title: &str) {
    sqlx::query(
        "update page_revisions set state = 'archived' where page_id = $1 and state = 'draft'",
    )
    .bind(page_id)
    .execute(db.pool())
    .await
    .expect("the previous draft must be retired");
    sqlx::query(
        "insert into page_revisions (page_id, revision_no, state, title) values ($1, $2, 'draft', $3)",
    )
    .bind(page_id)
    .bind(revision_no)
    .bind(title)
    .execute(db.pool())
    .await
    .expect("the next draft revision must be written");
}

/// Insert a media row; the bytes never matter to search.
async fn create_media(db: &Db, site_id: Uuid, filename: &str) -> Uuid {
    sqlx::query_scalar(
        "insert into media (site_id, storage_key, filename, content_type, size_bytes, checksum) \
         values ($1, $2, $3, 'image/png', 2048, $4) returning id",
    )
    .bind(site_id)
    .bind(format!("search/{}/{}", Uuid::new_v4().simple(), filename))
    .bind(filename)
    .bind("a".repeat(64))
    .fetch_one(db.pool())
    .await
    .expect("the test media row must be created")
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

/// The titles of one group's hits, in the order the API answered them.
fn group_titles(body: &Value, source: &str) -> Vec<String> {
    let group = body["groups"]
        .as_array()
        .unwrap_or_else(|| panic!("groups must be an array in {body}"))
        .iter()
        .find(|group| group["source"] == source)
        .unwrap_or_else(|| panic!("group {source} must exist in {body}"));
    group["hits"]
        .as_array()
        .unwrap_or_else(|| panic!("hits must be an array in {group}"))
        .iter()
        .filter_map(|hit| hit["title"].as_str().map(str::to_owned))
        .collect()
}

/// The ids of one group's hits.
fn hit_ids(body: &Value, source: &str) -> Vec<String> {
    let group = body["groups"]
        .as_array()
        .unwrap_or_else(|| panic!("groups must be an array in {body}"))
        .iter()
        .find(|group| group["source"] == source)
        .unwrap_or_else(|| panic!("group {source} must exist in {body}"));
    group["hits"]
        .as_array()
        .unwrap_or_else(|| panic!("hits must be an array in {group}"))
        .iter()
        .filter_map(|hit| hit["id"].as_str().map(str::to_owned))
        .collect()
}

/// The `source` values of the groups the API answered, in order.
fn group_keys(body: &Value) -> Vec<String> {
    body["groups"]
        .as_array()
        .unwrap_or_else(|| panic!("groups must be an array in {body}"))
        .iter()
        .filter_map(|group| group["source"].as_str().map(str::to_owned))
        .collect()
}

/// The `source` values of the sources the API skipped.
fn skipped_keys(body: &Value) -> Vec<String> {
    body["skipped"]
        .as_array()
        .unwrap_or_else(|| panic!("skipped must be an array in {body}"))
        .iter()
        .filter_map(|entry| entry["source"].as_str().map(str::to_owned))
        .collect()
}

/// Search as `token` and return the parsed body, asserting `200`.
async fn search(state: &AppState, token: &str, query: &str) -> Value {
    let response = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/search?q={query}"),
            Some(token),
            None,
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "search body: {}",
        response.body
    );
    response.body
}

#[tokio::test]
async fn search_groups_hits_by_source_and_scopes_them_to_the_caller() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    // Site A (the editor's organization): two matching pages, one matching file.
    create_page(
        &fixture.db,
        fixture.site_a,
        "release-notes",
        "Release notes",
        Some("What changed this week"),
    )
    .await;
    create_page(&fixture.db, fixture.site_a, "keynotes", "Keynotes", None).await;
    create_media(&fixture.db, fixture.site_a, "release-poster.png").await;
    // Site B (another organization): the same words, invisible to the editor.
    create_page(
        &fixture.db,
        fixture.site_b,
        "release-notes-b",
        "Release notes of another tenant",
        None,
    )
    .await;
    create_media(&fixture.db, fixture.site_b, "release-secret.png").await;

    let editor = fixture.editor_token().await;
    let body = search(&fixture.state, &editor, "release").await;

    assert_eq!(body["query"], "release");
    assert_eq!(body["terms"], json!(["release"]));
    assert!(body["took_ms"].is_number(), "took_ms: {body}");

    // Only the two readable sources answer; sites is skipped, not silently missing.
    assert_eq!(group_keys(&body), vec!["pages", "media"]);
    assert_eq!(skipped_keys(&body), vec!["sites"]);
    assert_eq!(body["skipped"][0]["reason"], "permission");

    // The other organization's rows are not in the result set.
    let pages = group_titles(&body, "pages");
    assert_eq!(pages, vec!["Release notes"]);
    let media = group_titles(&body, "media");
    assert_eq!(media, vec!["release-poster.png"]);

    fixture.cleanup().await;
}

#[tokio::test]
async fn search_ranks_a_prefix_above_a_word_inside_and_requires_every_term() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    create_page(
        &fixture.db,
        fixture.site_a,
        "release-notes",
        "Release notes",
        None,
    )
    .await;
    create_page(&fixture.db, fixture.site_a, "keynotes", "Keynotes", None).await;
    create_page(
        &fixture.db,
        fixture.site_a,
        "weekly",
        "Weekly update",
        Some("Release notes for the week"),
    )
    .await;

    let editor = fixture.editor_token().await;

    // "notes" hits both titles; the word beginning of "Release notes" outranks the inside
    // match of "Keynotes", and a title match outranks a match that only rides the summary.
    let body = search(&fixture.state, &editor, "notes").await;
    assert_eq!(
        group_titles(&body, "pages"),
        vec!["Release notes", "Keynotes", "Weekly update"]
    );

    // Every term must appear: "release invoice" matches nothing.
    let body = search(&fixture.state, &editor, "release%20invoice").await;
    assert!(group_titles(&body, "pages").is_empty());
    assert!(group_titles(&body, "media").is_empty());
    assert_eq!(
        group_keys(&body),
        vec!["pages", "media"],
        "an empty group is still listed, never a placeholder row"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn search_finds_a_page_by_a_revision_that_was_renamed_away() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    let page = create_page(
        &fixture.db,
        fixture.site_a,
        "changelog",
        "Release log",
        None,
    )
    .await;
    append_revision(&fixture.db, page, 2, "Changelog").await;

    let editor = fixture.editor_token().await;
    let body = search(&fixture.state, &editor, "release").await;

    // The displayed title is the latest revision; the match rode the older one.
    assert_eq!(group_titles(&body, "pages"), vec!["Changelog"]);

    fixture.cleanup().await;
}

#[tokio::test]
async fn search_answers_only_what_the_permissions_allow() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    let page = create_page(
        &fixture.db,
        fixture.site_a,
        "release-notes",
        "Release notes",
        None,
    )
    .await;

    // The member of the same organization holds no read key at all.
    let member = fixture.member_token().await;
    let body = search(&fixture.state, &member, "release").await;
    assert!(group_keys(&body).is_empty(), "no group is readable: {body}");
    assert_eq!(skipped_keys(&body), vec!["pages", "media", "sites"]);

    // The platform Owner reads across tenants, holds every key (nothing is skipped) and sees
    // the page this fixture wrote.
    let owner = fixture.platform_token().await;
    let body = search(&fixture.state, &owner, "release").await;
    assert!(group_keys(&body).contains(&"pages".to_owned()));
    assert!(
        group_keys(&body).contains(&"sites".to_owned()),
        "the owner holds sites.read: {body}"
    );
    assert!(
        skipped_keys(&body).is_empty(),
        "the owner holds every read key: {body}"
    );
    assert!(
        hit_ids(&body, "pages").contains(&page.to_string()),
        "the owner sees the fixture's page: {body}"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn search_honours_the_source_filter_and_the_group_limit() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    create_page(
        &fixture.db,
        fixture.site_a,
        "release-notes",
        "Release notes",
        None,
    )
    .await;
    create_page(
        &fixture.db,
        fixture.site_a,
        "release-archive",
        "Release archive",
        None,
    )
    .await;
    create_media(&fixture.db, fixture.site_a, "release-poster.png").await;

    let editor = fixture.editor_token().await;

    // A source filter narrows the answer to exactly those groups.
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/search?q=release&sources=pages",
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);
    assert_eq!(group_keys(&response.body), vec!["pages"]);

    // The limit caps each group.
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/search?q=release&limit=1",
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);
    assert_eq!(group_titles(&response.body, "pages").len(), 1);
    assert_eq!(group_titles(&response.body, "media").len(), 1);

    fixture.cleanup().await;
}

#[tokio::test]
async fn search_refuses_a_missing_session_query_or_unknown_source() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;

    let anonymous = call(
        &fixture.state,
        request(Method::GET, "/api/v1/search?q=release", None, None),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    let empty = call(
        &fixture.state,
        request(Method::GET, "/api/v1/search?q=", Some(&editor), None),
    )
    .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);
    assert_eq!(empty.body["error"]["code"], "query_required");

    let unknown = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/search?q=release&sources=pages,nope",
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    assert_eq!(unknown.body["error"]["code"], "unknown_source");
    let message = unknown.body["error"]["message"]
        .as_str()
        .expect("the error names the sources");
    assert!(message.contains("pages"), "message: {message}");
    assert!(message.contains("nope"), "message: {message}");

    fixture.cleanup().await;
}
