//! Integration tests for the cache headers a public request is served under (REQ-011, slice 1).
//!
//! The unit tests in `omnion-cdn` and in `routes::cdn_cache` prove that a *decision* turns
//! into headers. None of them proves the decision reaches a visitor. That is what this
//! suite is for, and it is the half that was missing: the rule engine was written, wired
//! to a permission, tested end to end against a database — and never once called, because
//! the public surface wrote a literal `max-age` into every response. A feature can be
//! completely green and still change nothing a browser does.
//!
//! Four things are proved here, each against a real router and a real database:
//!
//!   * **A site with no rules is private.** The default is not "cache for an hour because
//!     the old code did"; it is `private, no-store`.
//!   * **A rule an operator creates changes the response.** The same URL, before and after
//!     the rule, carries different `Cache-Control` — that sentence *is* the slice's done.
//!   * **A conditional request answers `304`.** And it keeps the cache headers, because a
//!     304 that drops the TTLs makes the client refetch on every navigation.
//!   * **`Vary` follows the rule's key components**, and a rule that keys on the language
//!     cookie says so. A shared cache that is not told will serve one language's page to a
//!     visitor who asked for another.
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) and skip themselves
//! with a printed reason when PostgreSQL is not reachable, like every other suite here.

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

const PASSWORD: &str = "correct horse battery";

/// The keys the administrator of this suite holds: the CDN surface under test, plus the
/// content keys the fixture needs in order to publish the page the public surface serves.
/// Without the second group there is nothing published to ask for, and the suite would
/// measure an empty site rather than a cache.
const ADMIN_PERMISSIONS: [&str; 8] = [
    "cdn.read",
    "cdn.manage",
    "cdn.purge",
    "content.pages.read",
    "content.pages.create",
    "content.pages.update",
    "content.pages.publish",
    "content.pages.delete",
];

/// One response, in the pieces the header assertions need.
struct Response {
    status: StatusCode,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    /// One header's value, lower-cased name lookup so a test does not have to know whether
    /// the router wrote `ETag` or `etag`.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Drive the real router without a network socket.
///
/// The body is kept as bytes rather than parsed: a `304` has none, and a harness that
/// insists on JSON would fail on exactly the response this suite exists to produce.
async fn call(state: &AppState, request: Request<Body>) -> Response {
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
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes()
        .to_vec();
    Response {
        status,
        headers,
        body,
    }
}

fn request(
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Request<Body> {
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

/// A public read, with optional extra headers.
fn public_get(uri: &str, extra: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder().method(Method::GET).uri(uri);
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::empty()).expect("request must build")
}

fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

async fn live_state() -> Option<(AppState, Db)> {
    let config = Config::from_env().ok()?;
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(err) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({err})");
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

/// One organization, one site, one administrator, one published page.
struct Fixture {
    state: AppState,
    db: Db,
    site: Uuid,
    organization: Uuid,
    account: Uuid,
    token: String,
    slug: String,
    /// The site's own key, which the public surface addresses as `?site=`.
    key: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool()).await.expect("the IAM seed must run");

        let suffix = Uuid::new_v4().simple().to_string();
        let organization: Uuid = sqlx::query_scalar(
            "insert into organizations (name, slug) values ($1, $2) returning id",
        )
        .bind(format!("CDN Headers {suffix}"))
        .bind(format!("cdn-headers-{suffix}"))
        .fetch_one(db.pool())
        .await
        .expect("organization must insert");

        let site = sites::create_site(
            db.pool(),
            omnion_identity::NewSite {
                organization_id: organization,
                key: format!("main{suffix}"),
                // (`key` is built from the same `suffix` above and read back from the
                // struct below, so the two cannot drift.)
                name: "CDN Headers".to_owned(),
                theme: None,
            },
        )
        .await
        .expect("site must insert")
        .id;

        let email = format!("cdn-headers-{suffix}@omnion.test");
        let user = users::create_user(
            db.pool(),
            NewUser {
                email: email.clone(),
                password: PASSWORD.to_owned(),
                display_name: "CDN Headers".to_owned(),
                organization_id: Some(organization),
            },
        )
        .await
        .expect("account must insert");
        let account = user.id;

        let role = role_store::create_role(
            db.pool(),
            NewRole {
                organization_id: organization,
                key: format!("cdn-headers-{suffix}"),
                name: "CDN Headers".to_owned(),
                description: "cache headers".to_owned(),
                priority: 100,
                inherits_role_id: None,
            },
        )
        .await
        .expect("role must insert");
        let entries: Vec<RolePermissionInput> = ADMIN_PERMISSIONS
            .iter()
            .map(|key| RolePermissionInput {
                key: (*key).to_owned(),
                effect: Effect::Allow,
            })
            .collect();
        role_store::set_role_permissions(db.pool(), role.id, &entries)
            .await
            .expect("role permissions must save");
        // The binding grants the *role*; the keys the role carries are the ones written
        // above, so there is no permission key on the binding itself.
        bindings::grant(
            db.pool(),
            NewBinding {
                role_id: role.id,
                user_id: user.id,
                scope: Scope::Organization { organization_id: organization },
                granted_by: None,
                expires_at: None,
            },
        )
        .await
        .expect("binding must insert");

        let key = format!("main{suffix}");
        let token = login(&state, &email).await;
        let slug = format!("about-{suffix}");
        publish_page(&state, &token, site, &slug, "About", "the body of the page").await;

        Some(Fixture {
            state,
            db,
            site,
            organization,
            account,
            token,
            slug,
            key,
        })
    }

    /// Create a cache rule for this fixture's site.
    async fn rule(&self, body: Value) -> Value {
        let response = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/cdn/rules",
                Some(&self.token),
                Some(body),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::CREATED,
            "a rule must be creatable: {}",
            String::from_utf8_lossy(&response.body)
        );
        serde_json::from_slice(&response.body).expect("the rule body is JSON")
    }

    /// `GET /api/v1/public/pages/{slug}` with the site's own key as the hint.
    async fn public_page(&self, extra: &[(&str, &str)]) -> Response {
        call(
            &self.state,
            public_get(
                &format!("/api/v1/public/pages/{}?site={}", self.slug, self.key),
                extra,
            ),
        )
        .await
    }

    async fn cleanup(self) {
        sqlx::query("delete from organizations where id = $1")
            .bind(self.organization)
            .execute(self.db.pool())
            .await
            .ok();
        sqlx::query("delete from users where id = $1")
            .bind(self.account)
            .execute(self.db.pool())
            .await
            .ok();
    }
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
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the fixture account must be able to sign in: {}",
        String::from_utf8_lossy(&response.body)
    );
    // The token is in the cookie and nowhere else — reading `body["token"]` reports a
    // successful sign-in as a failed fixture.
    response
        .header(header::SET_COOKIE.as_str())
        .expect("login must set the session cookie")
        .split(';')
        .next()
        .expect("the cookie has a value")
        .split_once('=')
        .expect("the cookie is name=value")
        .1
        .to_owned()
}

/// Create a page and publish it, so the public surface has something to answer with.
async fn publish_page(
    state: &AppState,
    token: &str,
    site_id: Uuid,
    slug: &str,
    title: &str,
    body: &str,
) {
    let created = call(
        state,
        request(
            Method::POST,
            "/api/v1/pages",
            Some(token),
            Some(json!({ "site_id": site_id, "slug": slug, "title": title, "body": body })),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "create: {}",
        String::from_utf8_lossy(&created.body)
    );
    let page: Value = serde_json::from_slice(&created.body).expect("the page body is JSON");
    let page_id = page["id"].as_str().expect("the page carries an id");

    let published = call(
        state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page_id}/publish"),
            Some(token),
            None,
        ),
    )
    .await;
    assert_eq!(
        published.status,
        StatusCode::OK,
        "publish: {}",
        String::from_utf8_lossy(&published.body)
    );
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_site_with_no_cache_rule_serves_its_public_page_as_private() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let response = fixture.public_page(&[]).await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.header("cache-control"),
        Some("private, no-store"),
        "an installation nobody configured must not be publicly cacheable"
    );
    assert_eq!(
        response.header("cdn-cache-control"),
        None,
        "a private response sends no edge TTL"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_rule_an_operator_creates_changes_the_cache_control_of_a_matching_page() {
    // This is the slice's own done-sentence, and the thing that was missing: the rule
    // engine existed, was permissioned, and was never consulted by a response.
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    let before = fixture.public_page(&[]).await;
    assert_eq!(before.header("cache-control"), Some("private, no-store"));

    let rule = fixture
        .rule(json!({
            "site_id": fixture.site,
            "name": "public pages",
            "path_pattern": format!("/{}", fixture.slug),
            "edge_ttl_seconds": 3600,
            "browser_ttl_seconds": 60,
        }))
        .await;
    assert_eq!(rule["edge_ttl_seconds"], 3600);

    let after = fixture.public_page(&[]).await;
    assert_eq!(
        after.header("cache-control"),
        Some("public, max-age=60"),
        "the rule created a moment ago must be what this response says"
    );
    assert_eq!(
        after.header("cdn-cache-control"),
        Some("public, max-age=3600"),
        "the edge TTL is the rule's, and it is a separate header from the browser's"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_rule_that_matches_nothing_does_not_make_the_page_cacheable() {
    // The other half of the same rule: a pattern that does not match must leave the
    // response alone, or "any rule exists" and "this rule applies" become the same thing.
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture
        .rule(json!({
            "site_id": fixture.site,
            "name": "somewhere else",
            "path_pattern": "/a/different/page",
        }))
        .await;

    let response = fixture.public_page(&[]).await;
    assert_eq!(
        response.header("cache-control"),
        Some("private, no-store"),
        "a rule for another path must not cache this one"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_disabled_rule_leaves_the_page_private() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture
        .rule(json!({
            "site_id": fixture.site,
            "name": "switched off",
            "path_pattern": format!("/{}", fixture.slug),
            "enabled": false,
        }))
        .await;

    let response = fixture.public_page(&[]).await;
    assert_eq!(response.header("cache-control"), Some("private, no-store"));
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_public_page_carries_an_etag_and_its_surrogate_tags() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let response = fixture.public_page(&[]).await;

    let etag = response
        .header("etag")
        .expect("a public response carries a validator");
    assert!(
        etag.starts_with('"') && etag.ends_with('"'),
        "an ETag is a quoted opaque string, got {etag:?}"
    );
    let keys = response
        .header("surrogate-key")
        .expect("a public response names what it can be purged with");
    assert!(
        keys.contains(&format!("/{}", fixture.slug)),
        "the page's own address is a tag a purge can name: {keys:?}"
    );
    assert!(
        keys.contains(&format!("site-{}", fixture.site)),
        "and so is its site: {keys:?}"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_conditional_read_answers_304_without_the_body_and_keeps_the_ttls() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture
        .rule(json!({
            "site_id": fixture.site,
            "name": "public pages",
            "path_pattern": format!("/{}", fixture.slug),
        }))
        .await;

    let first = fixture.public_page(&[]).await;
    let etag = first
        .header("etag")
        .expect("the first read carries a validator")
        .to_owned();

    let second = fixture.public_page(&[("if-none-match", &etag)]).await;
    assert_eq!(second.status, StatusCode::NOT_MODIFIED);
    assert!(
        second.body.is_empty(),
        "a 304 carries no body, got {} bytes",
        second.body.len()
    );
    assert_eq!(
        second.header("etag"),
        Some(etag.as_str()),
        "the 304 must name the same validator the 200 did"
    );
    assert_eq!(
        second.header("cache-control"),
        first.header("cache-control"),
        "a 304 that drops the TTLs makes the client refetch on every navigation"
    );
    // The length a 304 advertises must describe *its own* (empty) body, never the 200's.
    // A framework is entitled to write `0` here; what must never appear is the length of
    // the body that was not sent.
    let declared: Option<usize> = second.header("content-length").and_then(|v| v.parse().ok());
    assert_eq!(
        declared.unwrap_or(0),
        second.body.len(),
        "the advertised length must describe the body that was actually sent"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_conditional_read_for_another_revision_gets_the_whole_response() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture
        .rule(json!({
            "site_id": fixture.site,
            "name": "public pages",
            "path_pattern": format!("/{}", fixture.slug),
        }))
        .await;

    let response = fixture.public_page(&[("if-none-match", "\"p99-deadbeef\"")]).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "a validator the page does not carry is a miss, not an error"
    );
    assert!(!response.body.is_empty());
    fixture.cleanup().await;
}

#[tokio::test]
async fn two_reads_of_one_revision_share_a_validator_and_a_republish_changes_it() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let first = fixture.public_page(&[]).await;
    let second = fixture.public_page(&[]).await;
    assert_eq!(
        first.header("etag"),
        second.header("etag"),
        "reading the same revision twice must not invalidate every cache in front of it"
    );

    // Republish with different text. The page id is needed, and the panel's list is the
    // only place a test can get it without a second fixture.
    let pages = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages?site_id={}", fixture.site),
            Some(&fixture.token),
            None,
        ),
    )
    .await;
    let body: Value = serde_json::from_slice(&pages.body).expect("the list is JSON");
    let page_id = body["pages"][0]["id"]
        .as_str()
        .expect("the list carries a page id");

    let updated = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&fixture.token),
            Some(json!({ "title": "About", "body": "different text entirely" })),
        ),
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK, "{:?}", updated.body);
    let published = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page_id}/publish"),
            Some(&fixture.token),
            None,
        ),
    )
    .await;
    assert_eq!(published.status, StatusCode::OK);

    let after = fixture.public_page(&[]).await;
    assert_ne!(
        first.header("etag"),
        after.header("etag"),
        "a republished page must not answer 304 to a client holding the old validator"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_rule_that_keys_on_the_language_cookie_varies_on_cookie() {
    // The failure this prevents is not a slow one: a shared cache that is not told it
    // varies will serve the Turkish rendering of a page to an English visitor, forever,
    // with nothing in any log.
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture
        .rule(json!({
            "site_id": fixture.site,
            "name": "public pages, per language",
            "path_pattern": format!("/{}", fixture.slug),
            "cache_key": { "host": false, "path": true, "language_cookie": true },
        }))
        .await;

    let response = fixture
        .public_page(&[("cookie", "omnion_lang=tr")])
        .await;
    assert_eq!(
        response.header("vary"),
        Some("Cookie"),
        "a rule keyed on the language cookie must say so"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_rule_that_keys_on_nothing_extra_carries_no_vary() {
    // The other direction: sending `Vary: Cookie` on every response splits every shared
    // cache into one entry per cookie for no protection at all.
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture
        .rule(json!({
            "site_id": fixture.site,
            "name": "public pages",
            "path_pattern": format!("/{}", fixture.slug),
        }))
        .await;

    let response = fixture
        .public_page(&[("cookie", "omnion_lang=tr")])
        .await;
    assert_eq!(
        response.header("vary"),
        None,
        "a rule that does not key on the cookie does not vary on it"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn an_authorized_visitor_is_never_served_from_a_cache() {
    // A cache rule must not be able to make a response shareable for someone who is
    // signed in. This is the reason the bypass conditions exist, and it is the one case
    // where getting it wrong leaks one visitor's data to another.
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture
        .rule(json!({
            "site_id": fixture.site,
            "name": "public pages",
            "path_pattern": format!("/{}", fixture.slug),
        }))
        .await;

    let anonymous = fixture.public_page(&[]).await;
    assert_eq!(anonymous.header("cache-control"), Some("public, max-age=60"));

    let signed_in = fixture
        .public_page(&[("authorization", "Bearer a-token")])
        .await;
    assert_eq!(
        signed_in.header("cache-control"),
        Some("private, no-store"),
        "an authorization header bypasses every rule, whatever the rule says"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_page_that_is_not_published_answers_404_and_never_a_cache_header() {
    // The refusal must not become a hint. A 404 that is publicly cacheable for an hour is
    // a page that stays missing after the author fixes it.
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture
        .rule(json!({
            "site_id": fixture.site,
            "name": "everything",
            "path_pattern": "/**",
        }))
        .await;

    let response = call(
        &fixture.state,
        public_get(
            &format!("/api/v1/public/pages/never-written?site={}", fixture.key),
            &[],
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert_eq!(
        response.header("cache-control"),
        None,
        "a 404 from the public surface carries no cache policy at all"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_rule_whose_pattern_a_hand_edit_broke_does_not_take_the_page_down() {
    // A cache rule is configuration, and configuration gets edited by hand. One row whose
    // pattern no longer compiles must not become a 500 on a site's whole front end — the
    // response goes out private, which is what it did before the CDN layer existed.
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    sqlx::query(
        "insert into cdn_cache_rules (site_id, name, priority, path_pattern, enabled) \
         values ($1, 'hand edited', 0, '', true)",
    )
    .bind(fixture.site)
    .execute(fixture.db.pool())
    .await
    .expect("the broken row must insert — it is written by a migration, not the API");

    let response = fixture.public_page(&[]).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "one unreadable rule must not fail the page"
    );
    assert_eq!(response.header("cache-control"), Some("private, no-store"));
    fixture.cleanup().await;
}
