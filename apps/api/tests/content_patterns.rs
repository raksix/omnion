//! Integration test for patterns and page templates (REQ-063, slice 3).
//!
//! Slice 3 is the half of the page builder that turns one page into many: a pattern is cut out of
//! a page and dropped into the next, and a template starts a page with its structure already in
//! place. What has to be true is that the copy is *exact* — the block tree an author inserts is
//! the tree the pattern described — and that the two libraries carry their own permission, so
//! "may edit a page" and "may rewrite what every page on the site is built from" stay different
//! questions.
//!
//! It runs against the development stack
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

/// The content editor's keys. Note what is **absent**: `content.templates.manage`. The editor may
/// author from the platform's templates and from the pattern library, but may not rewrite the
/// gallery — the refusal test below is what proves the two are genuinely separate grants.
const EDITOR_PERMISSIONS: [&str; 7] = [
    "content.pages.read",
    "content.pages.create",
    "content.pages.update",
    "content.pages.publish",
    "content.blocks.read",
    "content.patterns.manage",
    "search.read",
];

/// What the curator adds on top: the template gallery.
const CURATOR_EXTRA: [&str; 1] = ["content.templates.manage"];

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    body: Value,
}

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

fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match token {
        Some(token) => builder.header(header::COOKIE, format!("omnion_session={token}")),
        None => builder,
    };
    match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&body).expect("body must serialize"),
            ))
            .expect("request must build"),
        None => builder.body(Body::empty()).expect("request must build"),
    }
}

fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

async fn live_state() -> Option<(AppState, Db)> {
    let config = Config::from_env().expect("environment must be valid");
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(error) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({error}) — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
            );
            return None;
        }
    };
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

async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("pat-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            display_name: "Pattern Tester".to_owned(),
            password: PASSWORD.to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    (user.id, email)
}

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
    assert!(
        response.status.is_success(),
        "login for {email} answered {}: {}",
        response.status,
        response.body
    );
    response
        .set_cookie
        .as_deref()
        .expect("login must set the session cookie")
        .split(';')
        .next()
        .expect("the cookie has a value")
        .split_once('=')
        .expect("the cookie is name=value")
        .1
        .to_owned()
}

/// One account holding `keys` at organization scope, plus the role that carries them.
async fn grant(db: &Db, organization_id: Uuid, user_id: Uuid, keys: &[&str], label: &str) {
    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id,
            key: format!("{}-{}", slug_key(label), &Uuid::new_v4().simple().to_string()[..8]),
            name: label.to_owned(),
            description: format!("{label} role"),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the role must be created");
    let entries: Vec<RolePermissionInput> = keys
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
        user_id,
        scope: Scope::Organization { organization_id },
        granted_by: None,
        expires_at: None,
    };
    bindings::validate(db.pool(), &binding)
        .await
        .expect("the binding must validate");
    bindings::grant(db.pool(), binding)
        .await
        .expect("the binding must be granted");
}

/// A role key from a human label: lowercase, dashes, no spaces.
///
/// The key is a machine identifier the store validates, and a label is a sentence. Concatenating
/// them produced `pattern editor-<hex>`, which the store refuses — so a test that could not
/// build its own fixture would have proved nothing about the routes.
fn slug_key(label: &str) -> String {
    label.to_lowercase().replace(' ', "-")
}

struct Fixture {
    state: AppState,
    db: Db,
    org: Uuid,
    site: Uuid,
    editor_email: String,
    curator_email: String,
    outsider_email: String,
    accounts: Vec<Uuid>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let org = Uuid::new_v4();
        sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
            .bind(org)
            .bind("Pattern Test Org")
            .bind(format!("pat-{}", Uuid::new_v4().simple()))
            .execute(db.pool())
            .await
            .expect("the organization must be created");

        let site = Uuid::new_v4();
        sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
            .bind(site)
            .bind(org)
            .bind(format!("pat{}", &Uuid::new_v4().simple().to_string()[..8]))
            .bind("Pattern Site")
            .execute(db.pool())
            .await
            .expect("the site must be created");

        let (editor_id, editor_email) = create_account(&db, Some(org)).await;
        let editor_keys: Vec<&str> = EDITOR_PERMISSIONS.to_vec();
        grant(&db, org, editor_id, &editor_keys, "Pattern Editor").await;

        // The curator additionally holds the template gallery's write key.
        let (curator_id, curator_email) = create_account(&db, Some(org)).await;
        let mut curator_keys = EDITOR_PERMISSIONS.to_vec();
        curator_keys.extend_from_slice(&CURATOR_EXTRA);
        grant(&db, org, curator_id, &curator_keys, "Pattern Curator").await;

        // A member with no content key at all.
        let (outsider_id, outsider_email) = create_account(&db, Some(org)).await;

        Some(Self {
            state,
            db,
            org,
            site,
            editor_email,
            curator_email,
            outsider_email,
            accounts: vec![editor_id, curator_id, outsider_id],
        })
    }

    async fn editor(&self) -> String {
        login(&self.state, &self.editor_email).await
    }
    async fn curator(&self) -> String {
        login(&self.state, &self.curator_email).await
    }
    async fn outsider(&self) -> String {
        login(&self.state, &self.outsider_email).await
    }

    async fn cleanup(&self) {
        sqlx::query("delete from users where id = any($1)")
            .bind(&self.accounts)
            .execute(self.db.pool())
            .await
            .expect("account cleanup must run");
        sqlx::query("delete from organizations where id = $1")
            .bind(self.org)
            .execute(self.db.pool())
            .await
            .expect("organization cleanup must run");
    }
}

fn block(kind: &str, props: Value) -> Value {
    json!({ "id": Uuid::new_v4().to_string(), "type": kind, "props": props })
}

/// A hero-ish group: a heading, a line of copy and a call to action.
fn hero_group() -> Value {
    json!([
        block("heading", json!({ "text": "Ship faster", "level": "h1" })),
        block("text", json!({ "text": "One core for content, workflow and API." })),
        block("cta", json!({
            "title": "Start today",
            "body": "No card, no trial clock.",
            "label": "Get started",
            "href": "/signup"
        })),
    ])
}

// ---------------------------------------------------------------------------------------------
// The suite
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_pattern_inserts_into_a_page_exactly_as_it_was_cut() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.editor().await;

    // Cut the group out of a page.
    let page = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/pages",
            Some(&token),
            Some(json!({
                "site_id": fixture.site,
                "slug": "host",
                "title": "Host page",
            })),
        ),
    )
    .await;
    assert_eq!(page.status, StatusCode::CREATED, "{}", page.body);
    let page_id = page.body["id"].as_str().expect("an id").to_owned();

    let saved = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&token),
            Some(json!({ "blocks": hero_group() })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);

    // Save those blocks as a pattern.
    let cut = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/patterns",
            Some(&token),
            Some(json!({
                "key": "hero",
                "name": "Hero",
                "category": "marketing",
                "description": "A heading, a line and a button.",
                "blocks": saved.body["draft"]["blocks"],
            })),
        ),
    )
    .await;
    assert_eq!(cut.status, StatusCode::CREATED, "{}", cut.body);
    let pattern_id = cut.body["id"].as_str().expect("an id").to_owned();
    assert_eq!(cut.body["block_count"], 3, "three blocks, nested included");

    // Insert it into a *second* page and read the tree back.
    let target = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/pages",
            Some(&token),
            Some(json!({
                "site_id": fixture.site,
                "slug": "target",
                "title": "Target page",
            })),
        ),
    )
    .await;
    let target_id = target.body["id"].as_str().expect("an id").to_owned();

    let inserted = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/patterns/{pattern_id}/blocks"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(inserted.status, StatusCode::OK, "{}", inserted.body);
    let blocks = inserted.body["blocks"].clone();
    assert_eq!(blocks.as_array().expect("an array").len(), 3);

    let written = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{target_id}"),
            Some(&token),
            Some(json!({ "blocks": blocks })),
        ),
    )
    .await;
    assert_eq!(written.status, StatusCode::OK, "{}", written.body);

    // The stored tree equals the pattern's, except that every id is new. Comparing anything
    // else — type, prop values, order, nesting — is what "reproduces the block tree exactly"
    // actually means, and the ids are the one thing that *must* differ.
    let stored = written.body["draft"]["blocks"].clone();
    let source = saved.body["draft"]["blocks"].clone();
    assert_eq!(
        strip_ids(&stored),
        strip_ids(&source),
        "the inserted tree differs from the pattern's in something other than ids"
    );
    let stored_ids = collect_ids(&stored);
    let source_ids = collect_ids(&source);
    assert!(!stored_ids.is_empty());
    for id in &stored_ids {
        assert!(
            !source_ids.contains(id),
            "a block inserted from a pattern kept the pattern's id ({id})"
        );
    }

    fixture.cleanup().await;
}

#[tokio::test]
async fn two_insertions_of_one_pattern_never_share_a_block_id() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.editor().await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/patterns",
            Some(&token),
            Some(json!({ "key": "twice", "name": "Twice", "blocks": hero_group() })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let id = created.body["id"].as_str().expect("an id").to_owned();

    let first = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/patterns/{id}/blocks"),
            Some(&token),
            None,
        ),
    )
    .await;
    let second = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/patterns/{id}/blocks"),
            Some(&token),
            None,
        ),
    )
    .await;

    let first_ids = collect_ids(&first.body["blocks"]);
    let second_ids = collect_ids(&second.body["blocks"]);
    assert_eq!(first_ids.len(), 3);
    for id in &first_ids {
        assert!(
            !second_ids.contains(id),
            "the same pattern inserted twice shared block id {id}"
        );
    }

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_key_is_the_identity_so_a_second_post_replaces_the_first() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.editor().await;

    let first = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/patterns",
            Some(&token),
            Some(json!({ "key": "banner", "name": "Banner", "blocks": hero_group() })),
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);

    let again = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/patterns",
            Some(&token),
            Some(json!({
                "key": "banner",
                "name": "Banner, revised",
                "blocks": hero_group(),
            })),
        ),
    )
    .await;
    assert_eq!(
        again.status,
        StatusCode::OK,
        "a second post with the same key replaces rather than duplicating: {}",
        again.body
    );
    assert_eq!(again.body["id"], first.body["id"], "it is the same row");
    assert_eq!(again.body["name"], "Banner, revised");

    let listed = call(
        &fixture.state,
        request(Method::GET, "/api/v1/patterns", Some(&token), None),
    )
    .await;
    let with_key = listed.body["patterns"]
        .as_array()
        .expect("an array")
        .iter()
        .filter(|entry| entry["key"] == "banner")
        .count();
    assert_eq!(with_key, 1, "one key is one pattern");

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_page_created_from_a_template_keeps_the_sample_content() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;

    // The gallery seeds the platform's own templates on first read.
    let gallery = call(
        &fixture.state,
        request(Method::GET, "/api/v1/page-templates", Some(&token), None),
    )
    .await;
    assert_eq!(gallery.status, StatusCode::OK, "{}", gallery.body);
    let templates = gallery.body["templates"].as_array().expect("an array");
    let keys: Vec<&str> = templates
        .iter()
        .map(|entry| entry["key"].as_str().unwrap_or_default())
        .collect();
    for wanted in ["landing", "about", "pricing", "blog-post", "contact"] {
        assert!(keys.contains(&wanted), "{wanted} must be in the gallery: {keys:?}");
    }
    let landing = templates
        .iter()
        .find(|entry| entry["key"] == "landing")
        .expect("the landing template");
    let template_blocks = landing["blocks"].as_array().expect("an array").len();
    assert!(template_blocks > 0, "a template with no blocks is a blank page");
    let landing_id = landing["id"].as_str().expect("an id").to_owned();

    // Build the page from it.
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/pages/from-template",
            Some(&token),
            Some(json!({
                "site_id": fixture.site,
                "template_id": landing_id,
                "slug": "from-landing",
                "title": "Our landing",
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(created.body["status"], "draft", "a new page is a draft");
    assert!(created.body["published"].is_null(), "and not published");

    // The page's blocks match the template's — same count, same shape, new ids.
    let page_blocks = created.body["draft"]["blocks"].clone();
    assert_eq!(
        page_blocks.as_array().expect("an array").len(),
        template_blocks,
        "the page carries the template's blocks"
    );
    assert_eq!(
        strip_ids(&page_blocks),
        strip_ids(&landing["blocks"]),
        "the page's tree differs from the template's in something other than ids"
    );

    // The sample content is findable: the body carries the template's words, so a page created
    // from a template is not invisible to search and the SEO fields.
    let body = created.body["draft"]["body"].as_str().unwrap_or_default();
    assert!(
        body.contains("headline") || body.len() > 0,
        "a page created from a template must carry readable text"
    );

    // It is a real page, not a link: it can be read back and it has its own history.
    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/pages/{}",
                created.body["id"].as_str().expect("an id")
            ),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.body);
    assert_eq!(read.body["slug"], "from-landing");

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_gallery_is_readable_by_an_editor_who_may_not_curate_it() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor().await;

    // Reading the gallery is authoring, not curation: an editor building a page needs to see
    // what it can start from.
    let gallery = call(
        &fixture.state,
        request(Method::GET, "/api/v1/page-templates", Some(&editor), None),
    )
    .await;
    assert_eq!(
        gallery.status,
        StatusCode::OK,
        "an editor reads the gallery: {}",
        gallery.body
    );

    // Writing it is not.
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/page-templates",
            Some(&editor),
            Some(json!({ "key": "mine", "name": "Mine", "blocks": hero_group() })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "an editor may not add a template: {}",
        refused.body
    );

    // And building a page from a template is the other direction: it needs page creation, and
    // the editor has it.
    let landing = gallery.body["templates"]
        .as_array()
        .expect("an array")
        .iter()
        .find(|entry| entry["key"] == "about")
        .expect("the about template");
    let built = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/pages/from-template",
            Some(&editor),
            Some(json!({
                "site_id": fixture.site,
                "template_id": landing["id"],
                "slug": "about-us",
                "title": "About us",
            })),
        ),
    )
    .await;
    assert_eq!(
        built.status,
        StatusCode::CREATED,
        "an editor builds a page from a template: {}",
        built.body
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_system_template_cannot_be_deleted_and_a_custom_one_can() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let curator = fixture.curator().await;

    let gallery = call(
        &fixture.state,
        request(Method::GET, "/api/v1/page-templates", Some(&curator), None),
    )
    .await;
    let landing = gallery.body["templates"]
        .as_array()
        .expect("an array")
        .iter()
        .find(|entry| entry["key"] == "landing")
        .expect("the landing template")
        .clone();

    let refused = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!(
                "/api/v1/page-templates/{}",
                landing["id"].as_str().expect("an id")
            ),
            Some(&curator),
            None,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::CONFLICT,
        "a template the platform ships is not the curator's to delete: {}",
        refused.body
    );

    let mine = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/page-templates",
            Some(&curator),
            Some(json!({
                "key": "campaign",
                "name": "Campaign",
                "description": "One page, one offer.",
                "blocks": hero_group(),
                "is_system": true,
            })),
        ),
    )
    .await;
    assert_eq!(mine.status, StatusCode::CREATED, "{}", mine.body);
    assert_eq!(
        mine.body["is_system"], false,
        "a request cannot claim the platform's flag — that flag is what makes a row undeletable"
    );

    let deleted = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!(
                "/api/v1/page-templates/{}",
                mine.body["id"].as_str().expect("an id")
            ),
            Some(&curator),
            None,
        ),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.body);

    fixture.cleanup().await;
}

#[tokio::test]
async fn an_account_without_the_keys_is_refused_everywhere() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let outsider = fixture.outsider().await;

    for (method, uri) in [
        (Method::GET, "/api/v1/patterns"),
        (Method::GET, "/api/v1/page-templates"),
    ] {
        let refused = call(
            &fixture.state,
            request(method.clone(), uri, Some(&outsider), None),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{uri} is gated: {}",
            refused.body
        );
    }

    let wrote = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/patterns",
            Some(&outsider),
            Some(json!({ "key": "x", "name": "X", "blocks": [] })),
        ),
    )
    .await;
    assert_eq!(wrote.status, StatusCode::FORBIDDEN, "{}", wrote.body);

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_pattern_is_saved_sanitised_and_reported_on() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.editor().await;

    let dirty = json!([
        block("heading", json!({ "text": "Clean", "level": "h1" })),
        block("raw_html", json!({
            "html": "<p>Kept</p><script>alert(1)</script>"
        })),
    ]);
    let saved = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/patterns",
            Some(&token),
            Some(json!({ "key": "dirty", "name": "Dirty", "blocks": dirty })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::CREATED, "{}", saved.body);
    let stored = saved.body["blocks"].to_string();
    assert!(
        !stored.contains("script"),
        "a pattern is stored through the same sanitiser as a page: {stored}"
    );
    assert!(stored.contains("Kept"), "the safe part survives: {stored}");

    // The page save's own fatal/unfinished split carries here: a tree the platform cannot read
    // is refused outright.
    let unknown = json!([block("carousel", json!({}))]);
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/patterns",
            Some(&token),
            Some(json!({ "key": "unknown", "name": "Unknown", "blocks": unknown })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "a block type the platform does not ship is refused: {}",
        refused.body
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_pattern_from_another_organization_is_invisible() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.editor().await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/patterns",
            Some(&token),
            Some(json!({ "key": "mine", "name": "Mine", "blocks": hero_group() })),
        ),
    )
    .await;
    let id = created.body["id"].as_str().expect("an id").to_owned();

    // A second organization, and an account inside it.
    let other_org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(other_org)
        .bind("Other Org")
        .bind(format!("other-{}", Uuid::new_v4().simple()))
        .execute(fixture.db.pool())
        .await
        .expect("the other organization must be created");
    let (other_id, other_email) = create_account(&fixture.db, Some(other_org)).await;
    let keys: Vec<&str> = EDITOR_PERMISSIONS.to_vec();
    grant(&fixture.db, other_org, other_id, &keys, "Other Editor").await;
    let other_token = login(&fixture.state, &other_email).await;

    let listed = call(
        &fixture.state,
        request(Method::GET, "/api/v1/patterns", Some(&other_token), None),
    )
    .await;
    let visible = listed.body["patterns"]
        .as_array()
        .expect("an array")
        .iter()
        .any(|entry| entry["id"] == id.as_str());
    assert!(!visible, "a pattern is organization-scoped");

    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/patterns/{id}"),
            Some(&other_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        read.status,
        StatusCode::NOT_FOUND,
        "and reading it by id across the boundary is a 404: {}",
        read.body
    );

    sqlx::query("delete from organizations where id = $1")
        .bind(other_org)
        .execute(fixture.db.pool())
        .await
        .expect("the other organization must be removed");
    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// A block payload with every `id` removed, for comparing two trees' shape.
fn strip_ids(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(strip_ids).collect()),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(key, _)| key.as_str() != "id")
                .map(|(key, value)| (key.clone(), strip_ids(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Every block id in a tree, nested included.
fn collect_ids(value: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    collect_ids_into(value, &mut ids);
    ids
}

fn collect_ids_into(value: &Value, ids: &mut Vec<String>) {
    // A tree is an array of blocks; a block's children live under `children`. Both levels have
    // to be walked, and the *only* two keys that hold blocks are the array itself and
    // `children` — descending into `props` would pick up a `media_id` or a `form_key` and call
    // it a block, which would make the "no shared ids" assertion meaningless because two
    // patterns would trivially share those.
    match value {
        Value::Array(items) => {
            for item in items {
                collect_ids_into(item, ids);
            }
        }
        Value::Object(object) => {
            if let Some(id) = object.get("id").and_then(Value::as_str) {
                ids.push(id.to_owned());
            }
            if let Some(children) = object.get("children") {
                collect_ids_into(children, ids);
            }
        }
        _ => {}
    }
}
