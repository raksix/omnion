//! Integration tests for the search surface: the index, the query language and the API
//! (docs/requests/REQ-002).
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) — locally and in CI. When
//! PostgreSQL is not reachable the suite skips itself with a printed reason.
//!
//! What the walks prove, in the words of the acceptance criteria: a seeded installation answers
//! a query across pages, media and users; an entity the caller cannot open is never returned
//! (an editor without `users.read` gets no user rows); results are scoped to the caller's
//! organization; the scoped syntax narrows honestly (an unknown `type:` is empty *and*
//! explained); publishing a page makes it findable within one indexer tick; a reindex is
//! idempotent and gated by `search.manage`; and the index's status and suggestions answer.

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
use omnion_search::indexer;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// Serialises this suite. The index has ONE cursor row and ONE event bus, so two of these walks
/// in flight settle each other's events — the lesson the automation suite learned first. The
/// guard is held for the whole walk.
static SEARCH_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What the content editor of this suite may do: read content and media, write and publish
/// pages, and search — but **not** read accounts or sites.
const EDITOR_PERMISSIONS: [&str; 5] = [
    "search.read",
    "content.pages.read",
    "content.pages.create",
    "content.pages.publish",
    "media.read",
];

/// What the librarian adds on top: the account read key, which is what makes user rows appear.
const LIBRARIAN_EXTRA: [&str; 1] = ["users.read"];

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

/// Two organizations with one site each, a platform Owner, a content editor of the first
/// organization, a librarian (the editor plus `users.read`) and a member with nothing.
///
/// Every row carries a `search-` prefix or a random address, and cleanup removes exactly the
/// rows this fixture created — by id, never by pattern, so parallel suites cannot collide.
struct Fixture {
    /// Held for the whole walk; see [`SEARCH_WALK`].
    _walk: tokio::sync::MutexGuard<'static, ()>,
    state: AppState,
    db: Db,
    platform_email: String,
    /// Eight hex characters unique to this run, planted in the fixture's site names so a
    /// search can address exactly this fixture's rows.
    marker: String,
    site_a: Uuid,
    site_b: Uuid,
    editor_email: String,
    librarian_email: String,
    member_email: String,
    accounts: Vec<Uuid>,
    organizations: Vec<Uuid>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = SEARCH_WALK.lock().await;
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let marker = Uuid::new_v4().simple().to_string()[..8].to_owned();
        let org_a = create_organization_row(&db, "a", "Search Test A").await;
        let org_b = create_organization_row(&db, "b", "Search Test B").await;
        let site_a = create_site_row(&db, org_a, "main", &format!("Search Site A {marker}")).await;
        let site_b = create_site_row(&db, org_b, "main", &format!("Search Site B {marker}")).await;

        let (platform_id, platform_email) = create_account(&db, None, "Release Owner").await;
        seed::bind_owner(db.pool(), platform_id)
            .await
            .expect("the owner binding must be created");

        let (editor_id, editor_email) = create_account(&db, Some(org_a), "Release Editor").await;
        let role = role_store::create_role(
            db.pool(),
            NewRole {
                organization_id: org_a,
                key: format!("search-editor-{}", Uuid::new_v4().simple()),
                name: "Search Editor".to_owned(),
                description: "Reads content and media of one organization and searches".to_owned(),
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

        // The librarian: everything the editor holds plus `users.read`.
        let (librarian_id, librarian_email) =
            create_account(&db, Some(org_a), "Release Librarian").await;
        let librarian_role = role_store::create_role(
            db.pool(),
            NewRole {
                organization_id: org_a,
                key: format!("search-librarian-{}", Uuid::new_v4().simple()),
                name: "Search Librarian".to_owned(),
                description: "Searches, and may see the accounts of the organization".to_owned(),
                priority: 420,
                inherits_role_id: None,
            },
        )
        .await
        .expect("the librarian role must be created");
        let entries: Vec<RolePermissionInput> = EDITOR_PERMISSIONS
            .iter()
            .chain(LIBRARIAN_EXTRA.iter())
            .map(|key| RolePermissionInput {
                key: (*key).to_owned(),
                effect: Effect::Allow,
            })
            .collect();
        role_store::set_role_permissions(db.pool(), librarian_role.id, &entries)
            .await
            .expect("the librarian permission set must be written");
        let binding = NewBinding {
            role_id: librarian_role.id,
            user_id: librarian_id,
            scope: Scope::Organization {
                organization_id: org_a,
            },
            granted_by: Some(platform_id),
            expires_at: None,
        };
        bindings::grant(db.pool(), binding)
            .await
            .expect("the librarian binding must be granted");

        let (member_id, member_email) = create_account(&db, Some(org_a), "Release Member").await;

        Some(Self {
            _walk: walk,
            state,
            db,
            platform_email,
            marker,
            site_a,
            site_b,
            editor_email,
            librarian_email,
            member_email,
            accounts: vec![platform_id, editor_id, librarian_id, member_id],
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

    /// The librarian (editor plus `users.read`), signed in.
    async fn librarian_token(&self) -> String {
        login(&self.state, &self.librarian_email).await
    }

    /// The plain member of the first organization, signed in.
    async fn member_token(&self) -> String {
        login(&self.state, &self.member_email).await
    }

    /// Rebuild the whole index (as the platform Owner, who holds `search.manage`).
    async fn reindex(&self) {
        let owner = self.platform_token().await;
        let response = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/search/reindex",
                Some(&owner),
                Some(json!({})),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "reindex: {}",
            response.body
        );
    }

    /// Remove exactly what this fixture created: the organizations cascade into sites, pages,
    /// revisions and media rows, and the search documents of those sites follow them.
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
async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    let email = format!("search-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: name.to_owned(),
            organization_id,
        },
    )
    .await
    .expect("creating a test account must succeed");
    (user.id, email)
}

/// Insert a page with one draft revision and return the page id.
async fn create_page(db: &Db, site_id: Uuid, slug: &str, title: &str) -> Uuid {
    let page_id: Uuid = sqlx::query_scalar(
        "insert into pages (site_id, slug, status) values ($1, $2, 'draft') returning id",
    )
    .bind(site_id)
    .bind(slug)
    .fetch_one(db.pool())
    .await
    .expect("the test page must be created");

    sqlx::query(
        "insert into page_revisions (page_id, revision_no, state, title) values ($1, 1, 'draft', $2)",
    )
    .bind(page_id)
    .bind(title)
    .execute(db.pool())
    .await
    .expect("the draft revision must be written");

    page_id
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

/// Search as `token` with a raw query string and assert `200`.
async fn search(state: &AppState, token: &str, q: &str) -> Value {
    let response = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/search?q={q}"),
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

/// The provider keys the hits of an answer come from.
fn hit_providers(body: &Value) -> Vec<String> {
    body["hits"]
        .as_array()
        .unwrap_or_else(|| panic!("hits must be an array in {body}"))
        .iter()
        .filter_map(|hit| hit["provider"].as_str().map(str::to_owned))
        .collect()
}

/// How many hits of one provider an answer carries.
fn provider_hits(body: &Value, source: &str) -> usize {
    body["hits"]
        .as_array()
        .unwrap_or_else(|| panic!("hits must be an array in {body}"))
        .iter()
        .filter(|hit| hit["provider"] == source)
        .count()
}

/// The titles of an answer's hits.
fn hit_titles(body: &Value) -> Vec<String> {
    body["hits"]
        .as_array()
        .unwrap_or_else(|| panic!("hits must be an array in {body}"))
        .iter()
        .filter_map(|hit| hit["title"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn search_answers_across_providers_and_hides_what_the_caller_may_not_read() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    create_page(
        &fixture.db,
        fixture.site_a,
        "release-notes",
        "Release notes",
    )
    .await;
    create_media(&fixture.db, fixture.site_a, "release-poster.png").await;

    // The librarian holds `users.read`, so the accounts are in their result set.
    fixture.reindex().await;

    let librarian = fixture.librarian_token().await;
    let body = search(&fixture.state, &librarian, "release").await;
    assert_eq!(body["query"], "release");
    let providers = hit_providers(&body);
    for expected in ["pages", "media", "users"] {
        assert!(
            providers.contains(&expected.to_owned()),
            "expected a {expected} hit in {body}"
        );
    }
    assert!(
        hit_titles(&body)
            .iter()
            .any(|title| title == "Release Editor"),
        "the editor account is findable by name: {body}"
    );

    // The editor may not read accounts: the same query answers no user row at all.
    let editor = fixture.editor_token().await;
    let body = search(&fixture.state, &editor, "release").await;
    let providers = hit_providers(&body);
    assert!(providers.contains(&"pages".to_owned()), "body: {body}");
    assert!(providers.contains(&"media".to_owned()), "body: {body}");
    assert!(
        !providers.contains(&"users".to_owned()),
        "an editor without users.read must not see user rows: {body}"
    );
    assert!(
        !providers.contains(&"sites".to_owned()),
        "an editor without sites.read must not see site rows: {body}"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn search_scopes_results_to_the_callers_organization() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    // A token unique to this run keeps the assertion independent of what the other suites
    // (running in parallel against the same database) put in the index.
    let token = Uuid::new_v4().simple().to_string();
    let mine = format!("Release notes {token}");
    let theirs = format!("Release notes of another tenant {token}");
    create_page(
        &fixture.db,
        fixture.site_a,
        &format!("release-{token}"),
        &mine,
    )
    .await;
    create_page(
        &fixture.db,
        fixture.site_b,
        &format!("release-b-{token}"),
        &theirs,
    )
    .await;
    fixture.reindex().await;

    // The editor of organization A sees exactly their own page.
    let editor = fixture.editor_token().await;
    let body = search(&fixture.state, &editor, &token).await;
    let titles = hit_titles(&body);
    assert_eq!(titles, vec![mine.clone()], "body: {body}");

    // The platform Owner holds every key and reads across tenants; the site rows come too.
    let owner = fixture.platform_token().await;
    let body = search(&fixture.state, &owner, &token).await;
    let titles = hit_titles(&body);
    assert!(
        titles.contains(&theirs),
        "the owner reads across tenants: {body}"
    );
    assert!(titles.contains(&mine), "body: {body}");

    // The sites provider answers the owner too: its site rows carry the fixture's marker.
    let body = search(&fixture.state, &owner, &fixture.marker).await;
    assert!(
        hit_providers(&body).contains(&"sites".to_owned()),
        "the owner reads sites as well: {body}"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn search_speaks_the_scoped_syntax() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    create_page(
        &fixture.db,
        fixture.site_a,
        "release-notes",
        "Release notes",
    )
    .await;
    create_page(
        &fixture.db,
        fixture.site_a,
        "release-archive",
        "Release archive",
    )
    .await;
    create_media(&fixture.db, fixture.site_a, "release-poster.png").await;
    fixture.reindex().await;

    let editor = fixture.editor_token().await;

    // type: narrows to one provider; the entity type spelling works too.
    let body = search(&fixture.state, &editor, "release%20type:page").await;
    assert!(
        hit_providers(&body)
            .iter()
            .all(|provider| provider == "pages"),
        "body: {body}"
    );
    assert!(body["total"].as_i64().unwrap_or(0) >= 2, "body: {body}");
    let body = search(&fixture.state, &editor, "release%20type:media").await;
    assert!(
        hit_providers(&body)
            .iter()
            .all(|provider| provider == "media"),
        "body: {body}"
    );

    // is:draft matches the draft pages, owner:me the caller's own rows.
    let body = search(&fixture.state, &editor, "type:page%20is:draft").await;
    assert!(body["total"].as_i64().unwrap_or(0) >= 2, "body: {body}");

    // site: narrows to the site's key.
    let body = search(&fixture.state, &editor, "release%20site:main").await;
    assert!(body["total"].as_i64().unwrap_or(0) >= 1, "body: {body}");

    // An unknown type is an honest empty answer with a hint, never a silent match-all.
    let body = search(&fixture.state, &editor, "release%20type:unicorn").await;
    assert_eq!(body["total"], 0, "body: {body}");
    assert!(
        !body["hints"].as_array().expect("hints").is_empty(),
        "an unknown type is explained: {body}"
    );

    // A date window narrows by the entity's own timestamp: the fixture's rows are brand new,
    // so a window that ends yesterday is empty and one that starts yesterday is not.
    let body = search(&fixture.state, &editor, "release%20after:2020-01-01").await;
    assert!(body["total"].as_i64().unwrap_or(0) >= 1, "body: {body}");
    let body = search(&fixture.state, &editor, "release%20before:2020-01-01").await;
    assert_eq!(body["total"], 0, "body: {body}");

    // A malformed date is explained too.
    let body = search(&fixture.state, &editor, "release%20before:soon").await;
    assert!(
        !body["hints"].as_array().expect("hints").is_empty(),
        "body: {body}"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn publishing_a_page_makes_it_findable_within_one_indexer_tick() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.reindex().await;

    let editor = fixture.editor_token().await;
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/pages",
            Some(&editor),
            Some(json!({
                "site_id": fixture.site_a,
                "slug": "launch-notes",
                "title": "Launch notes",
                "body": "The launch is on Friday."
            })),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "body: {}",
        created.body
    );
    let page_id = created.body["id"].as_str().expect("page id").to_owned();

    // Before publishing, the draft is not in the index under this title.
    let body = search(&fixture.state, &editor, "launch").await;
    assert_eq!(body["total"], 0, "a draft is not indexed yet: {body}");

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
    assert_eq!(published.status, StatusCode::OK, "body: {}", published.body);

    // One indexer tick: the published fact meets the index.
    let report = indexer::drain(fixture.db.pool(), 100)
        .await
        .expect("the drain must run");
    assert!(report.applied >= 1, "report: {report:?}");

    let body = search(&fixture.state, &editor, "launch").await;
    assert_eq!(body["total"], 1, "the published page is findable: {body}");
    assert_eq!(hit_titles(&body), vec!["Launch notes".to_owned()]);
    // The hit is a deep link: it opens the page's own editor, not just the pages screen.
    let hit_url = body["hits"][0]["url"].as_str().unwrap_or_default();
    assert!(
        hit_url.contains(&format!("focus={page_id}")),
        "the hit opens the page: {hit_url}"
    );

    // owner:me answers the caller's own rows only: the editor created this page, the librarian
    // did not.
    let body = search(&fixture.state, &editor, "launch%20owner:me").await;
    assert_eq!(body["total"], 1, "the editor owns the page: {body}");
    let librarian = fixture.librarian_token().await;
    let body = search(&fixture.state, &librarian, "launch%20owner:me").await;
    assert_eq!(body["total"], 0, "the librarian owns no page: {body}");

    // The cursor moved past the event: another tick neither duplicates the document nor loses
    // it (other suites put their own events on the same bus, so idleness is not the measure).
    let second = indexer::drain(fixture.db.pool(), 100)
        .await
        .expect("the second drain must run");
    assert!(second.cursor >= report.cursor, "report: {second:?}");
    let body = search(&fixture.state, &editor, "launch").await;
    assert_eq!(body["total"], 1, "still exactly one document: {body}");

    fixture.cleanup().await;
}

/// The producers the palette needed: an upload and a new site reach the index through the bus.
///
/// Slice 1 could index pages from the bus because content was the only module that emitted
/// events; media, sites and accounts had plans waiting for producers. This walk drives the real
/// routes — `POST /api/v1/sites`, `POST /api/v1/media`, `DELETE /api/v1/media/{id}` — and holds
/// the index to what the bus carried: a new site and a new file appear within one tick, and a
/// removed file leaves with it.
#[tokio::test]
async fn an_upload_and_a_new_site_reach_the_index_through_the_bus() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.reindex().await;
    let owner = fixture.platform_token().await;

    let organization_id: Uuid =
        sqlx::query_scalar("select organization_id from sites where id = $1")
            .bind(fixture.site_a)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the fixture's site has an organization");

    // A new site, announced by the route that creates it.
    let site_name = format!("Kaizen {} ", fixture.marker).trim().to_owned();
    let created_site = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sites",
            Some(&owner),
            Some(json!({
                "organization_id": organization_id,
                "key": format!("kaizen-{}", fixture.marker),
                "name": site_name,
            })),
        ),
    )
    .await;
    assert_eq!(
        created_site.status,
        StatusCode::CREATED,
        "body: {}",
        created_site.body
    );

    let tick = indexer::drain(fixture.db.pool(), 200)
        .await
        .expect("the drain must run");
    assert!(tick.applied >= 1, "report: {tick:?}");

    let query = format!("kaizen%20{}", fixture.marker);
    let body = search(&fixture.state, &owner, &query).await;
    // The trigram near-miss branch also answers rows that merely carry the marker, so the claim
    // is the honest one: the new site is found, and it ranks first.
    assert!(
        body["total"].as_i64().unwrap_or(0) >= 1,
        "the new site is findable: {body}"
    );
    assert_eq!(
        hit_titles(&body).first().map(String::as_str),
        Some(site_name.as_str()),
        "the new site ranks first: {body}"
    );
    assert_eq!(
        hit_providers(&body).first().map(String::as_str),
        Some("sites"),
        "and it is the sites provider that answers: {body}"
    );

    // A file, announced by the upload.
    let boundary = "omnionsearchwalk";
    let filename = format!("zephyr-{}.png", fixture.marker);
    let upload = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/v1/media?site_id={}", fixture.site_a))
        .header(header::COOKIE, format!("omnion_session={owner}"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(multipart_file(boundary, &filename, "image/png")))
        .expect("request must build");
    let uploaded = call(&fixture.state, upload).await;
    if uploaded.status.is_server_error() {
        // The object store is part of the development stack; without it there is nothing to
        // index, and the walk has to say so instead of failing on the store's behalf.
        eprintln!(
            "SKIP: the object store refused the upload ({}): {}",
            uploaded.status, uploaded.body
        );
        fixture.cleanup().await;
        return;
    }
    assert_eq!(
        uploaded.status,
        StatusCode::CREATED,
        "body: {}",
        uploaded.body
    );
    let media_id = uploaded.body["id"]
        .as_str()
        .expect("the upload answers with the row")
        .to_owned();

    let tick = indexer::drain(fixture.db.pool(), 200)
        .await
        .expect("the second drain must run");
    assert!(tick.applied >= 1, "report: {tick:?}");

    let query = format!("zephyr%20{}", fixture.marker);
    let body = search(&fixture.state, &owner, &query).await;
    assert!(
        provider_hits(&body, "media") >= 1,
        "the upload is findable: {body}"
    );
    assert_eq!(
        hit_titles(&body).first().map(String::as_str),
        Some(filename.as_str()),
        "the uploaded file ranks first: {body}"
    );
    // The hit names the file and its site, which is what the library's `?focus=` row needs.
    let hit_url = body["hits"][0]["url"].as_str().unwrap_or_default();
    assert!(
        hit_url.contains(&format!("site={}", fixture.site_a))
            && hit_url.contains(&format!("focus={media_id}")),
        "the hit opens the file: {hit_url}"
    );

    // Removing it takes the document with it — the branch that had no producer until now.
    let removed = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/media/{media_id}"),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(
        removed.status,
        StatusCode::NO_CONTENT,
        "body: {}",
        removed.body
    );

    let tick = indexer::drain(fixture.db.pool(), 200)
        .await
        .expect("the third drain must run");
    assert!(tick.applied >= 1, "report: {tick:?}");

    let body = search(&fixture.state, &owner, &query).await;
    assert_eq!(
        provider_hits(&body, "media"),
        0,
        "the removed file is gone: {body}"
    );

    fixture.cleanup().await;
}

/// One file as a `multipart/form-data` body, the shape `POST /api/v1/media` reads.
fn multipart_file(boundary: &str, filename: &str, content_type: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
    // The bytes only have to be bytes: the library stores what it is handed, and search indexes
    // the metadata, never the content.
    body.extend_from_slice(b"\x89PNG\r\n\x1a\nsearch-walk");
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

#[tokio::test]
async fn reindex_is_idempotent_and_gated_by_search_manage() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    create_page(
        &fixture.db,
        fixture.site_a,
        "release-notes",
        "Release notes",
    )
    .await;

    // The editor holds search.read but not search.manage.
    let editor = fixture.editor_token().await;
    let denied = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/search/reindex",
            Some(&editor),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(
        denied.status,
        StatusCode::FORBIDDEN,
        "body: {}",
        denied.body
    );
    assert_eq!(denied.body["error"]["code"], "permission_denied");

    // An unknown provider is the caller's mistake, named as such.
    let owner = fixture.platform_token().await;
    let unknown = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/search/reindex",
            Some(&owner),
            Some(json!({ "provider": "unicorns" })),
        ),
    )
    .await;
    assert_eq!(
        unknown.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        unknown.body
    );
    assert_eq!(unknown.body["error"]["code"], "unknown_provider");

    // Two runs, the same index: a second pass must neither duplicate nor drop the fixture's
    // document (the installation-wide count is not the measure here — other suites run in
    // parallel against the same database).
    fixture.reindex().await;
    let status = call(
        &fixture.state,
        request(Method::GET, "/api/v1/search/status", Some(&editor), None),
    )
    .await;
    assert_eq!(status.status, StatusCode::OK, "body: {}", status.body);
    assert!(
        status.body["documents"].as_i64().unwrap_or(0) >= 1,
        "the index has documents: {}",
        status.body
    );

    let first = search(&fixture.state, &editor, "release").await;
    assert_eq!(first["total"], 1, "one document for the page: {first}");

    fixture.reindex().await;
    let second = search(&fixture.state, &editor, "release").await;
    assert_eq!(
        second["total"], 1,
        "a second reindex must not duplicate the document: {second}"
    );

    // The removal branch: `media.deleted` has no producer yet, so the path is exercised
    // directly — dropping a document and reindexing must not bring it back.
    let file = create_media(&fixture.db, fixture.site_a, "release-poster.png").await;
    fixture.reindex().await;
    let body = search(&fixture.state, &editor, "release-poster").await;
    assert_eq!(
        provider_hits(&body, "media"),
        1,
        "the file is indexed: {body}"
    );
    indexer::remove_entity(fixture.db.pool(), "media", file)
        .await
        .expect("the document must be removed");
    let body = search(&fixture.state, &editor, "release-poster").await;
    assert_eq!(
        provider_hits(&body, "media"),
        0,
        "the document is gone: {body}"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn suggest_answers_with_the_palettes_first_paint() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    create_page(
        &fixture.db,
        fixture.site_a,
        "release-notes",
        "Release notes",
    )
    .await;
    fixture.reindex().await;

    let editor = fixture.editor_token().await;
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/search/suggest?q=rel",
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);
    let suggestions = response.body["suggestions"]
        .as_array()
        .expect("suggestions");
    assert!(!suggestions.is_empty(), "body: {}", response.body);
    assert!(suggestions.len() <= 8);
    for suggestion in suggestions {
        let title = suggestion["title"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase();
        assert!(title.starts_with("rel"), "suggestion: {suggestion}");
    }

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_callers_search_history_is_kept_and_clearable() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    create_page(
        &fixture.db,
        fixture.site_a,
        "release-notes",
        "Release notes",
    )
    .await;
    fixture.reindex().await;

    let editor = fixture.editor_token().await;
    search(&fixture.state, &editor, "release").await;
    search(&fixture.state, &editor, "release%20notes").await;

    let response = call(
        &fixture.state,
        request(Method::GET, "/api/v1/search/recent", Some(&editor), None),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);
    let queries: Vec<String> = response.body["queries"]
        .as_array()
        .expect("queries")
        .iter()
        .filter_map(|value| value.as_str().map(str::to_owned))
        .collect();
    assert_eq!(
        queries,
        vec!["release notes".to_owned(), "release".to_owned()],
        "newest first: {}",
        response.body
    );

    let cleared = call(
        &fixture.state,
        request(Method::DELETE, "/api/v1/search/recent", Some(&editor), None),
    )
    .await;
    assert_eq!(cleared.status, StatusCode::NO_CONTENT);

    let response = call(
        &fixture.state,
        request(Method::GET, "/api/v1/search/recent", Some(&editor), None),
    )
    .await;
    assert_eq!(response.body["queries"], json!([]));

    fixture.cleanup().await;
}

#[tokio::test]
async fn search_refuses_what_it_cannot_answer() {
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

    // The member holds no keys at all — not even search.read.
    let member = fixture.member_token().await;
    let denied = call(
        &fixture.state,
        request(Method::GET, "/api/v1/search?q=release", Some(&member), None),
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(denied.body["error"]["code"], "permission_denied");

    let empty = call(
        &fixture.state,
        request(Method::GET, "/api/v1/search?q=", Some(&editor), None),
    )
    .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);
    assert_eq!(empty.body["error"]["code"], "query_required");

    let bad_sort = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/search?q=release&sort=sideways",
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(bad_sort.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad_sort.body["error"]["code"], "unknown_sort");

    let bad_filter = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/search?q=release&updated=century",
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(bad_filter.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad_filter.body["error"]["code"], "unknown_updated_range");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// Slice 3 — the results depth: facets, the export, the weights
// ---------------------------------------------------------------------------------------------

/// Result of one call whose body is not JSON (the CSV export).
struct RawResponse {
    status: StatusCode,
    headers: std::collections::HashMap<String, String>,
    body: String,
}

/// Drive the router and keep the body as text, with the headers the export promises.
async fn call_raw(state: &AppState, request: Request<Body>) -> RawResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");
    let status = response.status();
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
    RawResponse {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// One facet group of an answer, by key.
fn facet<'a>(body: &'a Value, key: &str) -> &'a Value {
    body["facets"]
        .as_array()
        .unwrap_or_else(|| panic!("facets must be an array in {body}"))
        .iter()
        .find(|group| group["key"] == key)
        .unwrap_or_else(|| panic!("facet {key} must be answered in {body}"))
}

/// A facet value's count, by value.
fn facet_count(body: &Value, key: &str, value: &str) -> Option<i64> {
    facet(body, key)["values"]
        .as_array()
        .expect("facet values must be an array")
        .iter()
        .find(|row| row["value"] == value)
        .and_then(|row| row["count"].as_i64())
}

#[tokio::test]
async fn facets_count_what_the_filters_leave() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.platform_token().await;

    create_page(
        &fixture.db,
        fixture.site_a,
        "facet-one",
        &format!("Facet {} one", fixture.marker),
    )
    .await;
    create_page(
        &fixture.db,
        fixture.site_a,
        "facet-two",
        &format!("Facet {} two", fixture.marker),
    )
    .await;
    // An owner: both pages belong to the fixture's editor, so the rail has a name to offer.
    let editor_id: Uuid = sqlx::query_scalar("select id from users where email = $1")
        .bind(&fixture.editor_email)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the editor must exist");
    sqlx::query("update pages set created_by = $1 where site_id = $2")
        .bind(editor_id)
        .bind(fixture.site_a)
        .execute(fixture.db.pool())
        .await
        .expect("the pages must gain an owner");
    fixture.reindex().await;

    let answer = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/search?q={}&facets=true", fixture.marker),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "body: {}", answer.body);
    let body = answer.body;

    // Six groups, each one the rail renders.
    for key in ["type", "site", "owner", "language", "status", "updated"] {
        assert!(
            !facet(&body, key)["values"]
                .as_array()
                .expect("values")
                .is_empty(),
            "facet {key} must carry values: {body}"
        );
    }

    // The type facet reads as the registry writes it, not as the database stores it.
    let pages_value = facet(&body, "type")["values"]
        .as_array()
        .expect("values")
        .iter()
        .find(|row| row["value"] == "pages")
        .expect("the pages value must be there")
        .clone();
    assert_eq!(pages_value["label"], "Pages");
    assert!(
        pages_value["count"].as_i64().unwrap_or(0) >= 2,
        "two pages carry the marker: {pages_value}"
    );
    assert!(facet_count(&body, "site", &fixture.site_a.to_string()).unwrap_or(0) >= 2);
    assert!(facet_count(&body, "language", "en").unwrap_or(0) >= 2);
    assert!(facet_count(&body, "status", "draft").unwrap_or(0) >= 2);
    assert!(facet_count(&body, "updated", "today").unwrap_or(0) >= 2);
    // The owner facet carries the account that owns the pages, by name.
    assert_eq!(
        facet(&body, "owner")["values"]
            .as_array()
            .expect("values")
            .iter()
            .find(|row| row["value"] == editor_id.to_string())
            .map(|row| row["label"].clone()),
        Some(Value::String("Release Editor".to_owned())),
        "owner facet: {}",
        facet(&body, "owner")
    );

    // A facet's own filter is left out of its own counts: under `types=media` the type facet still
    // says what the pages would answer, because that is the number the click would produce.
    let filtered = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/search?q={}&types=media&facets=true",
                fixture.marker
            ),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(filtered.status, StatusCode::OK, "body: {}", filtered.body);
    assert_eq!(
        filtered.body["total"].as_i64(),
        Some(0),
        "the marker names no media: {}",
        filtered.body
    );
    assert!(
        facet_count(&filtered.body, "type", "pages").unwrap_or(0) >= 2,
        "the type facet must still answer for pages: {}",
        filtered.body
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_export_answers_one_row_per_hit_and_honours_a_selection() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.platform_token().await;

    create_page(
        &fixture.db,
        fixture.site_a,
        "export-one",
        &format!("Export {} one", fixture.marker),
    )
    .await;
    create_page(
        &fixture.db,
        fixture.site_a,
        "export-two",
        &format!("Export {} two", fixture.marker),
    )
    .await;
    fixture.reindex().await;

    let answer = search(&fixture.state, &owner, &fixture.marker).await;
    let total = answer["total"].as_i64().expect("total");

    let exported = call_raw(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/search/export?q={}", fixture.marker),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(exported.status, StatusCode::OK, "body: {}", exported.body);
    assert!(
        exported
            .headers
            .get("content-type")
            .is_some_and(|value| value.contains("text/csv"))
    );
    assert_eq!(
        exported.headers.get("x-export-rows").map(String::as_str),
        Some(total.to_string().as_str()),
        "the count header must match the result set"
    );
    let rows: Vec<&str> = exported
        .body
        .lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(
        rows.len() as i64,
        total,
        "one row per hit; file:\n{}",
        exported.body
    );
    assert!(
        exported
            .body
            .starts_with("title,type,provider,owner,updated,tags,url,subtitle"),
        "header: {}",
        exported.body.lines().next().unwrap_or_default()
    );

    // A selection exports exactly the rows the caller picked.
    let page_hit = answer["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .find(|hit| hit["provider"] == "pages")
        .expect("the fixture's pages must answer");
    let key = format!(
        "{}:{}:{}",
        page_hit["provider"].as_str().unwrap_or_default(),
        page_hit["entity_type"].as_str().unwrap_or_default(),
        page_hit["entity_id"].as_str().unwrap_or_default()
    );
    let selected = call_raw(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/search/export?q={}&selected={}",
                fixture.marker,
                urlencoding(&key)
            ),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(selected.status, StatusCode::OK, "body: {}", selected.body);
    let selected_rows: Vec<&str> = selected
        .body
        .lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(
        selected_rows.len(),
        1,
        "a one-row selection exports one row: {}",
        selected.body
    );
    assert!(
        selected.body.contains(&fixture.marker),
        "the exported row names the fixture: {}",
        selected.body
    );

    // A malformed selection is named, not exported as if it were empty.
    let broken = call_raw(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/search/export?q={}&selected=pages:page",
                fixture.marker
            ),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(broken.status, StatusCode::BAD_REQUEST);

    fixture.cleanup().await;
}

/// Percent-encode the characters a CSV row key may carry in a URL.
fn urlencoding(value: &str) -> String {
    value.replace(':', "%3A")
}

#[tokio::test]
async fn ranking_weights_change_the_order_of_a_fixture_result_set() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.platform_token().await;
    let term = format!("wt{}", &Uuid::new_v4().simple().to_string()[..8]);
    let titled = format!("Alpha {term}");

    // Two documents carry the term in two different sections of the index: a page whose title
    // names it, and the site whose *name* names it (the site name reaches a page's subtitle, so
    // the fixture's second page answers through the subtitle section). One query, two weights:
    // the order is the settings' answer.
    create_page(&fixture.db, fixture.site_a, "weights-title", &titled).await;
    create_page(
        &fixture.db,
        fixture.site_b,
        "weights-subtitle",
        "Weights subtitle page",
    )
    .await;
    sqlx::query("update sites set name = $1 where id = $2")
        .bind(format!("Beta {term} Site"))
        .bind(fixture.site_b)
        .execute(fixture.db.pool())
        .await
        .expect("the second site must be renamed");
    fixture.reindex().await;

    let default_order = hit_titles(&search(&fixture.state, &owner, &term).await);
    assert_eq!(
        default_order.first().map(String::as_str),
        Some(titled.as_str()),
        "with the defaults the title match leads: {default_order:?}"
    );

    // Title 1 · tags 10 · subtitle 10 · body 1 — allowed by the form's own rules (title ≥ body)
    // and it puts a subtitle match above the lighter title match.
    let saved = call(
        &fixture.state,
        request(
            Method::PUT,
            "/api/v1/search/settings",
            Some(&owner),
            Some(json!({
                "weights": { "title": 1, "tags": 10, "subtitle": 10, "body": 1 },
                "enabled_providers": [
                    "pages", "media", "users", "sites", "logs", "translations", "settings"
                ],
            })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "save: {}", saved.body);

    let flipped_order = hit_titles(&search(&fixture.state, &owner, &term).await);

    // Put the defaults back before asserting: a failed expectation must not leave the
    // installation tuned.
    let restored = call(
        &fixture.state,
        request(
            Method::PUT,
            "/api/v1/search/settings",
            Some(&owner),
            Some(json!({
                "weights": { "title": 6, "tags": 4, "subtitle": 3, "body": 1 },
                "enabled_providers": [
                    "pages", "media", "users", "sites", "logs", "translations", "settings"
                ],
            })),
        ),
    )
    .await;
    assert_eq!(
        restored.status,
        StatusCode::OK,
        "restore: {}",
        restored.body
    );

    assert_eq!(
        flipped_order.first().map(String::as_str),
        Some("Weights subtitle page"),
        "the heavier subtitle leads: {flipped_order:?}"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_settings_screen_reads_writes_and_refuses_what_it_promises() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.platform_token().await;
    let editor = fixture.editor_token().await;

    // Reading is `search.read`; the answer carries the defaults, so "restore defaults" is the
    // server's own number rather than a constant copied into the panel.
    let read = call(
        &fixture.state,
        request(Method::GET, "/api/v1/search/settings", Some(&editor), None),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "body: {}", read.body);
    assert_eq!(read.body["defaults"]["title"], 6);
    assert_eq!(read.body["weights"]["title"], 6);
    assert_eq!(
        read.body["available_providers"].as_array().map(Vec::len),
        Some(omnion_search::PROVIDERS.len())
    );

    // Writing is `search.manage`, which the editor does not hold.
    let denied = call(
        &fixture.state,
        request(
            Method::PUT,
            "/api/v1/search/settings",
            Some(&editor),
            Some(json!({
                "weights": { "title": 6, "tags": 4, "subtitle": 3, "body": 1 },
                "enabled_providers": ["pages"],
            })),
        ),
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);

    // A body heavier than the title is refused with its own code.
    let refused = call(
        &fixture.state,
        request(
            Method::PUT,
            "/api/v1/search/settings",
            Some(&owner),
            Some(json!({
                "weights": { "title": 2, "tags": 4, "subtitle": 3, "body": 9 },
                "enabled_providers": ["pages"],
            })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.body["error"]["code"], "title_below_body");

    // Switching every provider off would leave a search box that answers nothing.
    let empty = call(
        &fixture.state,
        request(
            Method::PUT,
            "/api/v1/search/settings",
            Some(&owner),
            Some(json!({
                "weights": { "title": 6, "tags": 4, "subtitle": 3, "body": 1 },
                "enabled_providers": [],
            })),
        ),
    )
    .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);
    assert_eq!(empty.body["error"]["code"], "no_providers");

    // A disabled provider stops answering, and switching it back on brings it back.
    let off = call(
        &fixture.state,
        request(
            Method::PUT,
            "/api/v1/search/settings",
            Some(&owner),
            Some(json!({
                "weights": { "title": 6, "tags": 4, "subtitle": 3, "body": 1 },
                "enabled_providers": ["media"],
            })),
        ),
    )
    .await;
    assert_eq!(off.status, StatusCode::OK, "off: {}", off.body);

    create_page(
        &fixture.db,
        fixture.site_a,
        "settings-off",
        &format!("Settings {} off", fixture.marker),
    )
    .await;
    fixture.reindex().await;
    let hidden = search(&fixture.state, &owner, &fixture.marker).await;
    assert_eq!(
        provider_hits(&hidden, "pages"),
        0,
        "pages are switched off: {hidden}"
    );

    let back_on = call(
        &fixture.state,
        request(
            Method::PUT,
            "/api/v1/search/settings",
            Some(&owner),
            Some(json!({
                "weights": { "title": 6, "tags": 4, "subtitle": 3, "body": 1 },
                "enabled_providers": [
                    "pages", "media", "users", "sites", "logs", "translations", "settings"
                ],
            })),
        ),
    )
    .await;
    assert_eq!(back_on.status, StatusCode::OK, "on: {}", back_on.body);
    let visible = search(&fixture.state, &owner, &fixture.marker).await;
    assert!(
        provider_hits(&visible, "pages") >= 1,
        "pages answer again: {visible}"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn activity_and_settings_join_the_index_with_their_own_screens() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.platform_token().await;

    // A page, its audit entry (the publish route writes one) and the settings row every
    // organization carries.
    let page_id = create_page(
        &fixture.db,
        fixture.site_a,
        "activity-one",
        &format!("Activity {} page", fixture.marker),
    )
    .await;
    sqlx::query(
        "insert into audit_log (organization_id, actor_user_id, action, target_type, target_id) \
         values ($1, null, 'page.updated', 'page', $2)",
    )
    .bind(fixture.organizations[0])
    .bind(page_id.to_string())
    .execute(fixture.db.pool())
    .await
    .expect("the audit entry must be written");

    fixture.reindex().await;

    let activity = search(&fixture.state, &owner, "page.updated").await;
    assert!(
        provider_hits(&activity, "logs") >= 1,
        "the audit entry must be findable: {activity}"
    );
    let log_hit = activity["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .find(|hit| hit["provider"] == "logs")
        .expect("a log hit");
    assert_eq!(log_hit["provider"], "logs");
    assert!(
        log_hit["url"]
            .as_str()
            .unwrap_or_default()
            .starts_with("/pages?site="),
        "the entry opens the page it touched: {log_hit}"
    );

    // The settings provider answers one row per organization, opening the screen that owns it.
    let settings = search(&fixture.state, &owner, "ranking+weights").await;
    assert!(
        provider_hits(&settings, "settings") >= 1,
        "the settings row must be findable: {settings}"
    );
    let settings_hit = settings["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .find(|hit| hit["provider"] == "settings")
        .expect("a settings hit");
    assert_eq!(settings_hit["url"], "/settings/search");

    // The status screen knows the pass that just ran.
    let status = call(
        &fixture.state,
        request(Method::GET, "/api/v1/search/status", Some(&owner), None),
    )
    .await;
    assert_eq!(status.status, StatusCode::OK, "body: {}", status.body);
    let lines = status.body["providers"].as_array().expect("providers");
    let pages_line = lines
        .iter()
        .find(|line| line["provider"] == "pages")
        .expect("the pages line");
    assert!(
        matches!(
            pages_line["state"].as_str(),
            Some("ready") | Some("stale") | Some("indexing")
        ),
        "state: {pages_line}"
    );
    assert!(
        pages_line["last_run"]["finished_at"].as_str().is_some(),
        "the pass is recorded: {pages_line}"
    );

    fixture.cleanup().await;
}
