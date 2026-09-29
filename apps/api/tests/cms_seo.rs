//! Integration test for the SEO toolkit (REQ-064, slice 3).
//!
//! Slice 3 is the half of the CMS a crawler reads on every page view, and the property that
//! matters most is the one that cannot be seen from the panel: **a stored tag is the tag a
//! crawler gets**. So what has to be true here is:
//!
//! * a page with a title, description, OG image and a schema type emits the correct tag set, and
//!   the JSON-LD is *generated* from the page rather than typed by the owner;
//! * the sitemap contains that page with the right `lastmod` after a regeneration, and a page
//!   that asks to be unindexed is not in it;
//! * a 301 literal rule answers, counts its hit, and a regex rule matches its own dialect — and
//!   a rule that would close a loop is refused *before* it is written;
//! * a path two rules match is reported as ambiguous rather than resolved silently;
//! * the panel's `Test a path` does not count a hit, which is the whole reason it is a separate
//!   entry point from the resolver;
//! * `seo.read` and `seo.manage` are two powers, and an account with only the first sees a 403
//!   on every write.
//!
//! It runs against the development stack. When PostgreSQL is not reachable the suite skips
//! itself with a printed reason — read the `SKIP` line before believing a green count, and note
//! that a *password* failure prints as a SKIP too.

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
use sqlx::Row;
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// What the reader may do. Note what is absent: no `seo.manage`, which is the point of the last
/// test in this file.
const READER_PERMISSIONS: [&str; 3] = ["seo.read", "content.pages.read", "content.pages.manage"];

/// What the editor adds on top.
const EDITOR_EXTRA: [&str; 1] = ["seo.manage"];

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    headers: Vec<(String, String)>,
    body: Value,
    raw: Vec<u8>,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
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
    let raw = bytes.to_vec();
    // The sitemap and robots routes answer text, not JSON; a JSON-only reader would panic on
    // them, so the reader is told which it got.
    let body = serde_json::from_slice::<Value>(&raw).unwrap_or(Value::Null);
    TestResponse {
        status,
        headers,
        body,
        raw,
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
    let email = format!("seo-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            display_name: "SEO Tester".to_owned(),
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
    let cookie = response
        .headers
        .iter()
        .find(|(name, _)| name == "set-cookie")
        .map(|(_, value)| value.clone())
        .expect("login must set the session cookie");
    cookie
        .split(';')
        .next()
        .expect("the cookie has a value")
        .split_once('=')
        .expect("the cookie is name=value")
        .1
        .to_owned()
}

async fn grant(db: &Db, organization_id: Uuid, user_id: Uuid, keys: &[&str], label: &str) {
    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id,
            key: format!(
                "{}-{}",
                label.to_lowercase().replace(' ', "-"),
                &Uuid::new_v4().simple().to_string()[..8]
            ),
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

/// A site with a primary host, one published page and one draft.
struct Fixture {
    state: AppState,
    db: Db,
    site: Uuid,
    host: String,
    page: Uuid,
    editor_email: String,
    reader_email: String,
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
            .bind("SEO Test Org")
            .bind(format!("seo-org-{}", Uuid::new_v4().simple()))
            .execute(db.pool())
            .await
            .expect("the organization must be created");

        let site = Uuid::new_v4();
        let site_key = format!("seo{}", &Uuid::new_v4().simple().to_string()[..8]);
        sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
            .bind(site)
            .bind(org)
            .bind(&site_key)
            .bind("SEO Site")
            .execute(db.pool())
            .await
            .expect("the site must be created");

        let host = format!("{}.example.test", &Uuid::new_v4().simple().to_string()[..8]);
        sqlx::query(
            "insert into site_domains (site_id, host, is_primary) values ($1, $2, true)",
        )
        .bind(site)
        .bind(&host)
        .execute(db.pool())
        .await
        .expect("the site domain must be created");

        let page = published_page(&db, site, "about", "About the studio").await;
        let draft = published_page(&db, site, "secret-plan", "Secret plan").await;
        // The second page becomes a draft, so "a draft is not in the sitemap" is proved by a row
        // that exists rather than by an absence.
        sqlx::query("update pages set status = 'draft', published_revision_id = null where id = $1")
            .bind(draft)
            .execute(db.pool())
            .await
            .expect("the page must become a draft");

        let (editor_id, editor_email) = create_account(&db, Some(org)).await;
        let mut editor_keys = READER_PERMISSIONS.to_vec();
        editor_keys.extend_from_slice(&EDITOR_EXTRA);
        grant(&db, org, editor_id, &editor_keys, "SEO Editor").await;

        let (reader_id, reader_email) = create_account(&db, Some(org)).await;
        grant(&db, org, reader_id, &READER_PERMISSIONS, "SEO Reader").await;

        Some(Self {
            state,
            db,
            site,
            host,
            page,
            editor_email,
            reader_email,
            accounts: vec![editor_id, reader_id],
        })
    }

    async fn editor(&self) -> String {
        login(&self.state, &self.editor_email).await
    }

    async fn reader(&self) -> String {
        login(&self.state, &self.reader_email).await
    }

}

/// Insert a page with its first revision and publish it, returning its id.
///
/// The store has no separate "save draft" call: `create_page` writes the page and revision 1 in
/// one transaction, and `publish_page` freezes it. The fixture goes through the store rather
/// than the HTTP surface because what these walks are about is the SEO layer — a page created
/// through the API would be testing page editing with a SEO hat on.
async fn published_page(db: &Db, site: Uuid, slug: &str, title: &str) -> Uuid {
    let (page, _revision) = omnion_content::pages::create_page(
        db.pool(),
        omnion_content::model::NewPage {
            site_id: site,
            slug: slug.to_owned(),
            page_type: Some("page".to_owned()),
            title: title.to_owned(),
            body: Some(format!("<p>{title}</p>")),
            summary: Some(format!("The {slug} page.")),
            created_by: None,
        },
    )
    .await
    .expect("the page must be created");

    omnion_content::pages::publish_page(db.pool(), page.id)
        .await
        .expect("the page must publish");
    page.id
}

#[tokio::test]
async fn a_page_with_metadata_emits_the_right_tags_and_lands_in_the_sitemap() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.editor().await;

    // A media row for the OG card — the card has to point at something real, and a URL built
    // from a column that was never set is the classic fake-looking preview.
    let media = sqlx::query(
        "insert into media (site_id, storage_key, filename, content_type, size_bytes, checksum) \
         values ($1, $2, $3, 'image/png', 2048, $4) returning id",
    )
    .bind(fixture.site)
    .bind(format!("seo/og-{}.png", Uuid::new_v4().simple()))
    .bind("og.png")
    .bind("a".repeat(64))
    .fetch_one(fixture.db.pool())
    .await
    .expect("the media row must be created")
    .get::<Uuid, _>("id");

    let saved = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/pages/{}/seo", fixture.page),
            Some(&token),
            Some(json!({
                "seo_title": "About the studio — what we build",
                "seo_description": "Who we are, what we build, and why it takes this long.",
                "canonical_url": "https://studio.example/about",
                "og_title": "About the studio",
                "og_description": "What we build.",
                "og_image_media_id": media,
                "twitter_card": "summary_large_image",
                "robots": "index,follow",
                "structured_data_type": "Article",
                "structured_data": {}
            })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "save answered {}", saved.body);

    let tags = &saved.body["tags"];
    assert_eq!(tags["title"], "About the studio — what we build");
    assert_eq!(
        tags["description"],
        "Who we are, what we build, and why it takes this long."
    );
    assert_eq!(tags["canonical"], "https://studio.example/about");
    assert_eq!(tags["og_title"], "About the studio");
    assert_eq!(tags["og_type"], "website");
    assert_eq!(tags["robots"], "index,follow");
    assert!(
        tags["og_image"].as_str().is_some_and(|url| url.starts_with("/media/")),
        "the OG card points at the media row: {}",
        tags["og_image"]
    );

    // The JSON-LD is *generated*, so it carries the page's own title and a real schema.org
    // envelope — not whatever the owner typed into a free-text box.
    let json_ld: Value = serde_json::from_str(
        tags["json_ld"]
            .as_str()
            .expect("an Article page emits JSON-LD"),
    )
    .expect("the JSON-LD must be valid JSON");
    assert_eq!(json_ld["@context"], "https://schema.org");
    assert_eq!(json_ld["@type"], "Article");
    assert_eq!(json_ld["headline"], "About the studio — what we build");
    assert_eq!(json_ld["url"], format!("https://{}/about", fixture.host));
    // An Article with a description has nothing missing; a Product would have listed `offers`.
    assert_eq!(tags["missing_fields"].as_array().map(Vec::len), Some(0));

    // The sitemap includes the page, with a `lastmod` taken from the published revision, and
    // excludes the draft.
    let regenerated = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sites/{}/seo/sitemap/regenerate", fixture.site),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(regenerated.status, StatusCode::OK);
    assert_eq!(regenerated.body["sitemap_url_count"], 1, "one published page");

    let xml = regenerated.body["sitemap_xml"]
        .as_str()
        .expect("a regenerated sitemap is stored");
    assert!(xml.starts_with("<?xml"), "it is a document: {xml}");
    assert!(xml.contains(&format!("<loc>https://{}/about</loc>", fixture.host)));
    assert!(xml.contains("<lastmod>"), "every URL carries a lastmod");
    assert!(
        !xml.contains("secret-plan"),
        "a draft is not advertised to a crawler"
    );

    // And the public surface serves exactly that document, with the right content type.
    let served = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/public/{}/sitemap.xml", fixture.host),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(served.status, StatusCode::OK);
    assert!(
        served
            .headers
            .iter()
            .any(|(name, value)| name == "content-type" && value.contains("xml")),
        "a crawler is told it is XML: {:?}",
        served.headers
    );
    assert!(String::from_utf8_lossy(&served.raw).contains("<urlset"));

    for account in &fixture.accounts {
        let _ = sqlx::query("delete from sessions where user_id = $1")
            .bind(account)
            .execute(fixture.db.pool())
            .await;
    }
}

#[tokio::test]
async fn a_noindex_page_is_emitted_with_the_directive_and_kept_out_of_the_sitemap() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.editor().await;

    let saved = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/pages/{}/seo", fixture.page),
            Some(&token),
            Some(json!({ "robots": "noindex,follow", "structured_data": {} })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "a minimal payload is accepted: {}", saved.body);
    // A minimal payload must NOT be refused: this is the defect the unit test caught, where
    // `#[serde(default)]` handed the store a JSON `null` and the object check called it malformed.
    assert_eq!(saved.body["seo"]["robots"], "noindex,follow");
    assert_eq!(saved.body["tags"]["robots"], "noindex,follow");

    let regenerated = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sites/{}/seo/sitemap/regenerate", fixture.site),
            Some(&token),
            None,
        ),
    )
    .await;
    let xml = regenerated.body["sitemap_xml"].as_str().unwrap_or_default();
    assert!(
        !xml.contains("/about"),
        "a page that asks crawlers to skip it is not in the sitemap that asks them to read it: {xml}"
    );
    assert_eq!(regenerated.body["sitemap_url_count"], 0);
}

#[tokio::test]
async fn a_redirect_fires_counts_its_hit_and_a_rule_that_closes_a_loop_is_refused() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.editor().await;

    let literal = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/seo/redirects",
            Some(&token),
            Some(json!({
                "site_id": fixture.site,
                "from_path": "/old-about",
                "to_path": "/about",
                "status_code": 301
            })),
        ),
    )
    .await;
    assert_eq!(literal.status, StatusCode::CREATED, "{}", literal.body);
    let literal_id = literal.body["id"].as_str().expect("the rule has an id");

    let pattern = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/seo/redirects",
            Some(&token),
            Some(json!({
                "site_id": fixture.site,
                "from_path": "/legacy/.*",
                "to_path": "/about",
                "status_code": 302,
                "pattern": "regex"
            })),
        ),
    )
    .await;
    assert_eq!(pattern.status, StatusCode::CREATED, "{}", pattern.body);

    // The public resolver: 301 + Location for the literal, 302 for the pattern, 404 for a path
    // no rule answers. And the hit counter moves, because a rule that does not count is a rule
    // the owner cannot tell is needed.
    let first = resolve(&fixture, "/old-about").await;
    assert_eq!(first.0, 301);
    assert_eq!(first.1, "/about");
    let second = resolve(&fixture, "/old-about?ref=news").await;
    assert_eq!(
        second.0, 301,
        "a query string does not make a redirect a different rule"
    );
    let patterned = resolve(&fixture, "/legacy/2024/spring").await;
    assert_eq!(patterned.0, 302, "a regex rule answers its own pattern");
    assert_eq!(patterned.1, "/about");
    assert_eq!(resolve(&fixture, "/nothing-here").await.0, 404);

    let hits: i64 = sqlx::query_scalar("select hits from cms_seo_redirects where id = $1")
        .bind(Uuid::parse_str(literal_id).expect("the id is a uuid"))
        .fetch_one(fixture.db.pool())
        .await
        .expect("the counter must read");
    assert_eq!(hits, 2, "both resolutions of the literal counted, the 302 did not");

    // The loop refusal, BEFORE the rule exists: /loop-a → /loop-b and /loop-b → /loop-a.
    call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/seo/redirects",
            Some(&token),
            Some(json!({
                "site_id": fixture.site,
                "from_path": "/loop-a",
                "to_path": "/loop-b"
            })),
        ),
    )
    .await;
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/seo/redirects",
            Some(&token),
            Some(json!({
                "site_id": fixture.site,
                "from_path": "/loop-b",
                "to_path": "/loop-a"
            })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "a cycle is a 409");
    assert_eq!(refused.body["error"]["code"], "redirect_loop");
    assert!(
        refused.body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("circle")),
        "the message says what would happen: {}",
        refused.body["error"]["message"]
    );
    let stored: i64 = sqlx::query_scalar(
        "select count(*) from cms_seo_redirects where from_path = '/loop-b'",
    )
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must read");
    assert_eq!(stored, 0, "the refused rule was never written");
}

#[tokio::test]
async fn a_path_two_rules_match_is_reported_as_ambiguous_and_a_test_does_not_count_a_hit() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.editor().await;

    let literal = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/seo/redirects",
            Some(&token),
            Some(json!({
                "site_id": fixture.site,
                "from_path": "/promo",
                "to_path": "/about"
            })),
        ),
    )
    .await;
    let rule_id = literal.body["id"].as_str().expect("the rule has an id").to_owned();

    let broad = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/seo/redirects",
            Some(&token),
            Some(json!({
                "site_id": fixture.site,
                "from_path": "/promo/.*",
                "to_path": "/about",
                "pattern": "regex"
            })),
        ),
    )
    .await;
    assert_eq!(broad.status, StatusCode::CREATED, "{}", broad.body);

    // Both rules answer `/promo`. The resolver takes the literal, and the panel's test says so
    // rather than letting the owner believe one rule owns the path.
    let tested = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/seo/redirects/{rule_id}/test"),
            Some(&token),
            Some(json!({ "path": "/promo" })),
        ),
    )
    .await;
    assert_eq!(tested.status, StatusCode::OK);
    assert_eq!(tested.body["matched"]["from_path"], "/promo");
    assert_eq!(
        tested.body["also_matched"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        1,
        "the regex rule also answers this path and the panel says so"
    );

    // The test did not count. This is the reason it is a separate entry point, so it is asserted
    // rather than assumed: an owner trying three candidate rules must not leave three hits in
    // the column they are reading.
    let hits: i64 = sqlx::query_scalar(
        "select coalesce(sum(hits), 0) from cms_seo_redirects where site_id = $1",
    )
    .bind(fixture.site)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the counters must read");
    assert_eq!(hits, 0, "a test is a question, not a visit");
}

#[tokio::test]
async fn reading_seo_is_not_the_power_to_change_it() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let reader = fixture.reader().await;

    // The reader can look.
    let overview = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/seo/settings?site_id={}", fixture.site),
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(overview.status, StatusCode::OK, "{}", overview.body);
    assert_eq!(overview.body["site"]["id"], fixture.site.to_string());
    // The vocabularies the panel renders come from the store, never from a list hard-coded in
    // the admin app that will later disagree with what the store refuses.
    assert!(
        overview.body["vocabulary"]["structured_data_types"]
            .as_array()
            .is_some_and(|types| types.iter().any(|t| t == "Article")),
        "the schema vocabulary is served: {}",
        overview.body["vocabulary"]
    );
    assert!(
        overview.body["vocabulary"]["page_types"]
            .as_array()
            .is_some_and(|types| types.contains(&json!("page"))),
        "the page types are this site's own: {}",
        overview.body["vocabulary"]["page_types"]
    );

    // And cannot touch.
    for (method, uri, body) in [
        (
            Method::POST,
            "/api/v1/seo/redirects".to_owned(),
            json!({ "site_id": fixture.site, "from_path": "/a", "to_path": "/b" }),
        ),
        (
            Method::PUT,
            format!("/api/v1/pages/{}/seo", fixture.page),
            json!({ "robots": "noindex" }),
        ),
        (
            Method::PUT,
            format!("/api/v1/sites/{}/seo/settings", fixture.site),
            json!({ "default_priority": 0.9 }),
        ),
    ] {
        let refused = call(
            &fixture.state,
            request(method.clone(), &uri, Some(&reader), Some(body)),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{method} {uri} must be refused for a reader: {}",
            refused.body
        );
    }
}

#[tokio::test]
async fn a_robots_txt_that_blocks_the_whole_site_is_saved_with_the_warning_naming_it() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.editor().await;

    let saved = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/sites/{}/seo/settings", fixture.site),
            Some(&token),
            Some(json!({
                "sitemap_types": ["page"],
                "default_priority": 0.7,
                "default_change_frequency": "daily",
                "robots_txt": "User-agent: *\nDisallow: /\n"
            })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(saved.body["default_priority"], 0.7);
    assert_eq!(saved.body["default_change_frequency"], "daily");

    // It saved. The panel warns instead of refusing, and the warning is in the overview.
    let overview = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/seo/settings?site_id={}", fixture.site),
            Some(&token),
            None,
        ),
    )
    .await;
    let warnings = overview.body["robots_warnings"].as_array().expect("warnings are an array");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].as_str().is_some_and(|w| w.contains("not to read it")),
        "the warning says what it does: {warnings:?}"
    );

    // The public route serves the file with the right content type — this is what a crawler
    // reads, and a JSON error body here would silently deindex the site.
    let served = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/public/{}/robots.txt", fixture.host),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(served.status, StatusCode::OK);
    assert!(
        served
            .headers
            .iter()
            .any(|(name, value)| name == "content-type" && value.contains("text/plain")),
        "robots.txt is plain text: {:?}",
        served.headers
    );
    assert_eq!(String::from_utf8_lossy(&served.raw), "User-agent: *\nDisallow: /\n");

    // A frequency the sitemap does not understand is refused with a message naming it.
    let refused = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/sites/{}/seo/settings", fixture.site),
            Some(&token),
            Some(json!({ "default_change_frequency": "often" })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert_eq!(refused.body["error"]["code"], "invalid_seo");
}

#[tokio::test]
async fn the_broken_link_crawl_finds_a_link_to_a_page_that_does_not_exist() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.editor().await;

    // Rewrite the published body with a link to a page this site does not have, and one it does.
    let body = "<p>Read <a href=\"/about\">about us</a> and \
                <a href=\"/gone-forever\">the old page</a>, or \
                <a href=\"https://elsewhere.example/x\">somewhere else</a>.</p>";
    sqlx::query(
        "update page_revisions set body = $2 where id = \
            (select published_revision_id from pages where id = $1)",
    )
    .bind(fixture.page)
    .bind(body)
    .execute(fixture.db.pool())
    .await
    .expect("the body must be rewritten");

    let scanned = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/seo/broken-links",
            Some(&token),
            Some(json!({ "site_id": fixture.site })),
        ),
    )
    .await;
    assert_eq!(scanned.status, StatusCode::OK, "{}", scanned.body);
    let rows = scanned.body.as_array().expect("the scan returns a list");
    assert_eq!(rows.len(), 1, "exactly the one internal link is broken: {rows:?}");
    assert_eq!(rows[0]["target_url"], "/gone-forever");
    assert_eq!(rows[0]["anchor_text"], "the old page");
    assert_eq!(rows[0]["source_slug"], "about");
    // The off-site link and the link to a real page are both left alone.
    assert!(
        !rows.iter().any(|row| row["target_url"] == "https://elsewhere.example/x"),
        "an off-site link is not this tool's business"
    );

    let link_id = rows[0]["id"].as_str().expect("the row has an id");

    // Dismissing it takes it out of the list, and the scan does NOT bring it back — a dismissal
    // is the owner saying "I know".
    let dismissed = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/seo/broken-links/{link_id}"),
            Some(&token),
            Some(json!({ "ignored": true })),
        ),
    )
    .await;
    assert_eq!(dismissed.status, StatusCode::NO_CONTENT, "{}", dismissed.body);

    call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/seo/broken-links",
            Some(&token),
            Some(json!({ "site_id": fixture.site })),
        ),
    )
    .await;
    let still = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/seo/broken-links?site_id={}", fixture.site),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(
        still.body.as_array().map(Vec::len),
        Some(0),
        "a dismissed link stays dismissed across a rescan"
    );

    // Fixing the link REMOVES it: a broken-link view that can only ever grow is a to-do list
    // nobody trusts.
    sqlx::query(
        "update page_revisions set body = '<p>Read <a href=\"/about\">about us</a>.</p>' \
         where id = (select published_revision_id from pages where id = $1)",
    )
    .bind(fixture.page)
    .execute(fixture.db.pool())
    .await
    .expect("the body must be rewritten");
    let rescan = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/seo/broken-links",
            Some(&token),
            Some(json!({ "site_id": fixture.site })),
        ),
    )
    .await;
    assert_eq!(rescan.body.as_array().map(Vec::len), Some(0), "the fixed link left the list");
}

/// Resolve a path through the public redirect surface, returning (status, location).
async fn resolve(fixture: &Fixture, path: &str) -> (u16, String) {
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/public/{}/redirect?path={}",
                fixture.host,
                urlencode(path)
            ),
            None,
            None,
        ),
    )
    .await;
    let location = response
        .headers
        .iter()
        .find(|(name, _)| name == "location")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    (response.status.as_u16(), location)
}

/// Percent-encode a path for a query string.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}
