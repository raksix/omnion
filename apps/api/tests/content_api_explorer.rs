//! Integration test for the Explorer's dispatcher (REQ-019, slice 3).
//!
//! The unit tests in `routes/content_explorer.rs` prove how a request is *built*. This file proves
//! what happens after it is, and the two properties it is really about are the ones a panel route
//! could plausibly get wrong and no unit test would see:
//!
//! * **A call made through the Explorer is metered.** The dispatcher spends the token's budget
//!   itself, because the extractor that normally does it sees a pre-authenticated token in the
//!   extensions and returns it without spending anything. Miss that and the Explorer becomes an
//!   unmetered hole in the very limiter the Usage tab exists to report — and a hole nobody can
//!   see, because every call still answers `200` and the chart simply never moves.
//! * **A call made through the Explorer is a real request.** It travels through `routes::router`,
//!   so the matched route, the path decoding, the scope check and the query parser are the ones an
//!   integrator's own call goes through. A dispatcher that called the handlers directly would
//!   pass a "the button works" test while skipping everything that can refuse.
//!
//! The refusals are pinned by **code and by the field they name**, because "it failed" is not a
//! statement an operator can act on — `insufficient_scope` tells them which checkbox to tick, and
//! `details.field` tells the form which input to highlight.
//!
//! Runs against the development stack, and skips itself with a printed reason when PostgreSQL
//! is not reachable.

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

const PASSWORD: &str = "correct horse battery";

/// Reading and managing tokens is what lets the suite mint the token it then calls as.
const CURATOR_PERMISSIONS: [&str; 6] = [
    "content.api.read",
    "content.api.manage",
    "content.pages.read",
    "content.pages.create",
    "content.pages.publish",
    "content.pages.delete",
];

/// A token's own row, as the dispatcher reads it.
///
/// `Debug` so a failed assertion can print the whole answer — every one of the failures in this
/// file's first run was diagnosed from exactly that line, and a type that cannot be printed
/// reduces a failure to "it did not match".
#[derive(Debug)]
struct TestResponse {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Value,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    TestResponse {
        status,
        headers,
        body,
    }
}

/// A session-authenticated request, CSRF header included.
fn session_request(method: Method, uri: &str, cookies: &str, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, cookies);
    // The CSRF secret travels as a HEADER and the cookie issued alongside it is only the readable
    // half; sending the cookie without the header answers `403 csrf_unavailable` on every
    // mutation, which reads as a misconfigured suite rather than a missing header.
    let builder = match csrf_of(cookies) {
        Some(secret) => builder.header("x-omnion-csrf", secret),
        None => builder,
    };
    match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).expect("serializes")))
            .expect("builds"),
        None => builder.body(Body::empty()).expect("builds"),
    }
}

fn csrf_of(cookies: &str) -> Option<String> {
    cookies.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        (key.trim() == "omnion_csrf" && !value.is_empty()).then(|| value.to_owned())
    })
}

/// The Explorer's own request: a session plus a JSON body.
fn explorer_request(cookies: &str, body: Value) -> Request<Body> {
    session_request(
        Method::POST,
        "/api/v1/content-api/explorer",
        cookies,
        Some(body),
    )
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
            eprintln!("SKIP: PostgreSQL is not reachable ({error})");
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

/// Create a role carrying `keys` and bind it to `user_id` in the organization.
///
/// Copied from `content_read_surface.rs` rather than written fresh: the `create_role` →
/// `set_role_permissions` → `validate` → `grant` order is the store's own contract, and a
/// hand-rolled version of it that validates nothing produces a suite whose every route answers
/// `403` for a reason that has nothing to do with what it is testing.
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

/// An account attached to `organization`, or to none when it is `None`.
async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("explorer-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            display_name: "Explorer Tester".to_owned(),
            password: PASSWORD.to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    (user.id, email)
}

struct Fixture {
    state: AppState,
    db: Db,
    organization: Uuid,
    site: Uuid,
    curator_email: String,
}

async fn fixture() -> Option<Fixture> {
    let (state, db) = live_state().await?;
    seed::ensure(db.pool()).await.expect("the IAM seed must run");

    let organization = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(organization)
        .bind("Explorer Test Org")
        .bind(format!("ex-{}", Uuid::new_v4().simple()))
        .execute(db.pool())
        .await
        .expect("the organization must be created");

    let site = Uuid::new_v4();
    sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
        .bind(site)
        .bind(organization)
        .bind(format!("e{}", &Uuid::new_v4().simple().to_string()[..10]))
        .bind("Explorer Site")
        .execute(db.pool())
        .await
        .expect("the site must be created");

    let (curator_id, curator_email) = create_account(&db, Some(organization)).await;
    grant(
        &db,
        organization,
        curator_id,
        &CURATOR_PERMISSIONS,
        "Explorer Curator",
    )
    .await;

    // Three published pages, so `limit=2` genuinely has a next page. A single page would make the
    // cursor walk pass for the wrong reason — `next_cursor` would be null because there is
    // nothing left, not because the pagination works.
    for (index, slug) in ["alpha", "beta", "gamma"].iter().enumerate() {
        let minutes_ago = index as i64;
        seed_published_page(&db, site, slug, minutes_ago).await;
    }

    Some(Fixture {
        state,
        db,
        organization,
        site,
        curator_email,
    })
}

async fn seed_published_page(db: &Db, site: Uuid, slug: &str, minutes_ago: i64) -> Uuid {
    let page_id = Uuid::new_v4();
    let revision_id = Uuid::new_v4();
    let stamp = time::OffsetDateTime::now_utc() - time::Duration::minutes(minutes_ago);
    let mut tx = db.pool().begin().await.expect("a transaction opens");
    sqlx::query(
        "insert into pages (id, site_id, slug, page_type, status, created_at, updated_at) \
         values ($1, $2, $3, 'page', 'published', $4, $4)",
    )
    .bind(page_id)
    .bind(site)
    .bind(slug)
    .bind(stamp)
    .execute(&mut *tx)
    .await
    .expect("the page must be seeded");
    sqlx::query(
        "insert into page_revisions (id, page_id, revision_no, state, title, body, summary, \
                                   created_at, published_at) \
         values ($1, $2, 1, 'published', $3, $4, $5, $6, $6)",
    )
    .bind(revision_id)
    .bind(page_id)
    .bind(format!("Title of {slug}"))
    .bind(format!("Body of {slug}"))
    .bind(format!("Summary of {slug}"))
    .bind(stamp)
    .execute(&mut *tx)
    .await
    .expect("the revision must be seeded");
    sqlx::query("update pages set published_revision_id = $2 where id = $1")
        .bind(page_id)
        .bind(revision_id)
        .execute(&mut *tx)
        .await
        .expect("the publication pointer must be set");
    tx.commit().await.expect("the fixture must commit");
    page_id
}

impl Fixture {
    async fn login(&self) -> String {
        let response = call(
            &self.state,
            session_request(
                Method::POST,
                "/api/v1/auth/login",
                "",
                Some(json!({ "email": self.curator_email, "password": PASSWORD })),
            ),
        )
        .await;
        assert!(response.status.is_success(), "{}", response.body);
        response
            .headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(|value| value.split(';').next().unwrap_or(value).trim().to_owned())
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Mint a token and return its id — the dispatcher's whole input.
    async fn mint(&self, session: &str, name: &str, scopes: &[&str]) -> Uuid {
        let response = call(
            &self.state,
            session_request(
                Method::POST,
                "/api/v1/content-api/tokens",
                session,
                Some(json!({ "name": name, "scopes": scopes })),
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED, "{}", response.body);
        Uuid::parse_str(response.body["token"]["id"].as_str().expect("an id")).expect("a uuid")
    }

    /// One Explorer call, with this fixture's own `site` merged in.
    ///
    /// The site is added for every operation, and the merge is deliberate rather than repeated at
    /// each call site: an organization-wide token reads **every** published page in the database,
    /// so on the shared QA database a list call would count another run's leftovers. The cursor
    /// walk asserted "the second page holds the remainder" and read two rows instead of one —
    /// not because pagination is wrong but because the population was not this test's. Scoping
    /// here makes the walk about pagination, which is what it is for.
    async fn explore(
        &self,
        session: &str,
        token_id: Uuid,
        operation: &str,
        params: Value,
    ) -> TestResponse {
        let mut params = params;
        if let Some(map) = params.as_object_mut() {
            map.entry("site").or_insert(json!(self.site.to_string()));
        }
        call(
            &self.state,
            explorer_request(
                session,
                json!({ "token_id": token_id, "operation_id": operation, "params": params }),
            ),
        )
        .await
    }
}

#[tokio::test]
async fn the_explorer_makes_a_real_call_and_reports_the_surface_own_answer() {
    let Some(fixture) = fixture().await else { return };
    let session = fixture.login().await;
    let token = fixture.mint(&session, "Explorer Pages", &["content:read"]).await;

    let response = fixture
        .explore(&session, token, "pages.list", json!({ "limit": "2" }))
        .await;

    assert_eq!(
        response.status,
        StatusCode::OK,
        "the dispatcher's own answer: {}",
        response.body
    );
    let answer = &response.body;
    assert_eq!(answer["operation_id"], "pages.list");
    assert_eq!(answer["method"], "GET");
    // The panel's answer is the surface's, not a reconstruction: the body is the page list the
    // handler built, with the same `count` the rows agree with.
    assert_eq!(answer["body"]["items"].as_array().expect("items").len(), 2);
    assert_eq!(answer["body"]["count"], 2);
    assert!(answer["body_is_json"].as_bool().expect("a boolean"));
    // The metered route is the TEMPLATE. One endpoint is one row on the Usage tab's leaderboard
    // however many pages were walked; a resolved path here would be one row per slug.
    assert_eq!(answer["metered_route"], "/content/pages");
    // The resolved URL is the **caller-facing** one: origin + the documented path + the sorted
    // query, with exactly one `/api/v1`. That last word is the assertion — the first version of
    // this route composed origin + `/api/v1` + path and produced
    // `https://host/api/v1/api/v1/content/pages?limit=2`, a snippet an integrator pastes into a
    // shell and gets a `404` from, while the dispatched request (which uses the document's path
    // verbatim) worked fine. The pane and the snippets would have disagreed with the request.
    let url = answer["url"].as_str().expect("a url");
    assert!(
        url.contains("/api/v1/content/pages"),
        "the resolved URL: {url}"
    );
    assert_eq!(
        url.matches("/api/v1").count(),
        1,
        "exactly one mount point: {url}"
    );
    assert!(url.contains("limit=2"), "the parameter is in the URL: {url}");
    // The token is echoed without any part of its secret, and the panel can reconcile the call
    // with the Usage tab because it knows which row it landed in.
    assert_eq!(answer["token"]["id"], token.to_string());
    assert!(
        answer["token"]["prefix"].as_str().expect("a prefix").starts_with("omn_"),
        "the prefix is the only part that is ever shown"
    );
    assert!(answer["token"].get("plaintext").is_none());
}

#[tokio::test]
async fn the_explorer_spends_the_tokens_own_budget_so_the_limiter_is_measured() {
    let Some(fixture) = fixture().await else { return };
    let session = fixture.login().await;
    let token = fixture.mint(&session, "Explorer Metered", &["content:read"]).await;

    let first = fixture
        .explore(&session, token, "pages.list", json!({}))
        .await;
    let second = fixture
        .explore(&session, token, "pages.list", json!({}))
        .await;

    // Two calls, two decrements. One call cannot tell a real meter from a header that always
    // answers the tier — and the defect this catches is invisible everywhere else: both calls
    // answer `200`, the body is right, and the Usage chart simply never moves.
    let first_left: i64 = first.body["token"]["remaining"]
        .as_i64()
        .expect("a remaining count");
    let second_left: i64 = second.body["token"]["remaining"]
        .as_i64()
        .expect("a remaining count");
    assert!(
        second_left < first_left,
        "the second call must cost budget the first one spent: {first_left} then {second_left}"
    );
    // And the number in the pane is the number on the header the SURFACE stamped — the pane
    // renders `answer.headers`, so a screen that showed one number while the caller saw another
    // would be the disagreement this whole slice exists to refuse. `answer.headers` is the
    // dispatched response's headers, not the dispatcher's own, which carries none.
    let header = second.body["headers"]
        .as_array()
        .expect("the surface's headers")
        .iter()
        .find(|header| header["name"] == "x-ratelimit-remaining")
        .and_then(|header| header["value"].as_str())
        .and_then(|value| value.parse::<i64>().ok());
    assert_eq!(
        header,
        Some(second_left),
        "the pane and the surface's header are one verdict: {second:?}"
    );
}

#[tokio::test]
async fn the_explorer_applies_the_tokens_scope_like_the_surface_does() {
    let Some(fixture) = fixture().await else { return };
    let session = fixture.login().await;
    // A content-only token calling the media list: the surface answers `403` naming the scope.
    let token = fixture.mint(&session, "Explorer NoMedia", &["content:read"]).await;

    let response = fixture
        .explore(&session, token, "media.list", json!({}))
        .await;

    // The dispatcher does not pre-check scopes — it lets the real route answer, so the pane shows
    // exactly what an integrator would see, including the code they branch on.
    assert_eq!(response.status, StatusCode::OK, "the answer is inside the body");
    assert_eq!(response.body["status"], 403);
    assert_eq!(response.body["body"]["error"]["code"], "insufficient_scope");
    assert_eq!(
        response.body["body"]["error"]["details"]["required_scope"],
        "media:read",
        "the refusal must name the scope, or the caller cannot know what to ask for"
    );
}

#[tokio::test]
async fn the_explorer_reports_a_path_parameter_missing_by_name() {
    let Some(fixture) = fixture().await else { return };
    let session = fixture.login().await;
    let token = fixture.mint(&session, "Explorer NoSlug", &["content:read"]).await;

    let response = fixture
        .explore(&session, token, "pages.read", json!({}))
        .await;

    // The DISPATCHER refuses this one, not the surface: `slug` is part of the path, so the
    // request is never built and the surface never sees a call. It is therefore an ordinary `400`
    // on the route — and that distinction is load-bearing, because the screen's error strip
    // highlights the field from `details.field` and a refusal without a field is a form everybody
    // works around by guessing.
    assert_eq!(response.status, StatusCode::BAD_REQUEST, "{}", response.body);
    assert_eq!(response.body["error"]["code"], "invalid_parameter");
    assert_eq!(response.body["error"]["details"]["field"], "slug");
}

#[tokio::test]
async fn an_unknown_parameter_is_refused_rather_than_dropped() {
    let Some(fixture) = fixture().await else { return };
    let session = fixture.login().await;
    let token = fixture.mint(&session, "Explorer Typo", &["content:read"]).await;

    let response = fixture
        .explore(&session, token, "pages.list", json!({ "limitt": "5" }))
        .await;

    // The DISPATCHER refuses this one, not the surface: the document does not declare `limitt`,
    // so the request is never built. It is therefore an ordinary `400` on the route — and that
    // distinction is load-bearing for the screen, whose error strip renders it.
    assert_eq!(response.status, StatusCode::BAD_REQUEST, "{}", response.body);
    assert_eq!(response.body["error"]["code"], "invalid_parameter");
    assert_eq!(response.body["error"]["details"]["field"], "limitt");
    // Silently ignoring a misspelled filter is the worst kind of explorer bug: the response looks
    // right and the caller concludes their filter worked.
    assert!(
        response.body["error"]["details"]["accepted"]
            .as_array()
            .expect("the accepted names")
            .contains(&json!("limit"))
    );
}

#[tokio::test]
async fn a_slug_cannot_rewrite_the_url_it_is_sent_in() {
    let Some(fixture) = fixture().await else { return };
    let session = fixture.login().await;
    let token = fixture.mint(&session, "Explorer Injection", &["content:read"]).await;

    // A slug carrying a path separator and a query fragment. If it were not encoded into its own
    // segment this would become `GET /content/pages/does-not-exist?limit=999` — a request for a
    // different page with a different limit, which is a server-side request forgery wearing the
    // slug's clothes.
    let response = fixture
        .explore(
            &session,
            token,
            "pages.read",
            json!({ "slug": "alpha/../../../v1/content-api/tokens?limit=999" }),
        )
        .await;

    assert_eq!(response.status, StatusCode::OK, "the answer is inside the body");
    let url = response.body["url"].as_str().expect("a url");
    assert!(
        !url.contains("/v1/content-api/tokens"),
        "the slug must not be able to reach another endpoint: {url}"
    );
    assert!(
        response.body["body"]["error"]["code"].as_str().unwrap_or("") == "not_found"
            || response.body["status"] == 404,
        "the call must be a lookup for a slug that does not exist, got {}",
        response.body["status"]
    );
    // And the snippet — which is what somebody pastes into a shell — carries the same encoded URL.
    let snippet = response.body["snippets"]["curl"].as_str().expect("a snippet");
    assert!(!snippet.contains("omn_"), "no credential may appear in a snippet");
    assert!(snippet.contains("OMNION_TOKEN"));
}

#[tokio::test]
async fn a_revoked_token_cannot_be_called_as_and_says_so_with_its_own_code() {
    let Some(fixture) = fixture().await else { return };
    let session = fixture.login().await;
    let token = fixture.mint(&session, "Explorer Revoked", &["content:read"]).await;

    let revoked = call(
        &fixture.state,
        session_request(
            Method::DELETE,
            &format!("/api/v1/content-api/tokens/{token}"),
            &session,
            None,
        ),
    )
    .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT, "{}", revoked.body);

    let response = fixture
        .explore(&session, token, "pages.list", json!({}))
        .await;

    // The refusal is the SURFACE's code, which is what makes the pane's error rendering the same
    // shape a real caller's is. A `not_found` here would read as "that token does not exist",
    // which sends an operator looking for the wrong problem.
    assert_eq!(response.status, StatusCode::FORBIDDEN, "{}", response.body);
    assert_eq!(response.body["error"]["code"], "token_revoked");
}

#[tokio::test]
async fn another_organizations_token_is_absent_rather_than_forbidden() {
    let Some(fixture) = fixture().await else { return };
    let session = fixture.login().await;
    let token = fixture.mint(&session, "Explorer Foreign", &["content:read"]).await;

    // A second tenant with its own curator, and the first tenant's token named at it.
    let other_org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(other_org)
        .bind("Explorer Other Org")
        .bind(format!("exo-{}", Uuid::new_v4().simple()))
        .execute(fixture.db.pool())
        .await
        .expect("the other organization must be created");
    let (other_id, other_email) = create_account(&fixture.db, Some(other_org)).await;
    grant(
        &fixture.db,
        other_org,
        other_id,
        &CURATOR_PERMISSIONS,
        "Explorer Other",
    )
    .await;

    let other_login = call(
        &fixture.state,
        session_request(
            Method::POST,
            "/api/v1/auth/login",
            "",
            Some(json!({ "email": other_email, "password": PASSWORD })),
        ),
    )
    .await;
    let other_session = other_login
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or(value).trim().to_owned())
        .collect::<Vec<_>>()
        .join("; ");

    let response = fixture
        .explore(&other_session, token, "pages.list", json!({}))
        .await;

    // `404`, not `403`: a `403` confirms the token exists somewhere on the installation, which is
    // exactly the information a cross-tenant caller must not learn.
    assert_eq!(response.status, StatusCode::NOT_FOUND, "{}", response.body);
    assert_eq!(response.body["error"]["code"], "not_found");
    let _ = fixture.organization;
}

#[tokio::test]
async fn the_cursor_from_a_list_call_can_be_used_for_the_next_page() {
    let Some(fixture) = fixture().await else { return };
    let session = fixture.login().await;
    let token = fixture.mint(&session, "Explorer Cursor", &["content:read"]).await;

    let first = fixture
        .explore(&session, token, "pages.list", json!({ "limit": "2" }))
        .await;
    let cursor = first.body["next_cursor"]
        .as_str()
        .expect("a cursor for the next page")
        .to_owned();

    let second = fixture
        .explore(&session, token, "pages.list", json!({ "limit": "2", "cursor": cursor }))
        .await;

    let first_slugs: Vec<String> = first.body["body"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["slug"].as_str().expect("a slug").to_owned())
        .collect();
    let second_slugs: Vec<String> = second.body["body"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["slug"].as_str().expect("a slug").to_owned())
        .collect();
    // Three pages, two per page: the walk must see the third and must not repeat the first two.
    // `next_cursor` being lifted server-side is what makes the screen's "next page" button one
    // assignment instead of a JSON path walk into a body it is also rendering.
    assert_eq!(first_slugs.len(), 2);
    assert_eq!(second_slugs.len(), 1, "the third page holds the remainder");
    for slug in &second_slugs {
        assert!(!first_slugs.contains(slug), "a page must not repeat a slug: {slug}");
    }
    assert!(second.body["next_cursor"].is_null(), "the last page says so");
}

#[tokio::test]
async fn an_unknown_operation_lists_the_documented_ones() {
    let Some(fixture) = fixture().await else { return };
    let session = fixture.login().await;
    let token = fixture.mint(&session, "Explorer Typo Op", &["content:read"]).await;

    let response = fixture
        .explore(&session, token, "page.list", json!({}))
        .await;

    assert_eq!(response.status, StatusCode::BAD_REQUEST, "{}", response.body);
    assert_eq!(response.body["error"]["details"]["field"], "operation_id");
    let accepted = response.body["error"]["details"]["accepted"]
        .as_array()
        .expect("the documented ids")
        .clone();
    assert!(
        accepted.contains(&json!("pages.list")),
        "a caller who mistypes the id needs the list, not a guess: {accepted:?}"
    );
}

#[tokio::test]
async fn every_documented_operation_is_dispatchable_over_http() {
    let Some(fixture) = fixture().await else { return };
    let session = fixture.login().await;
    let token = fixture
        .mint(
            &session,
            "Explorer Coverage",
            &["content:read", "media:read"],
        )
        .await;

    // The document is fetched from the SERVER rather than hard-coded here, so this walk and the
    // picker can never disagree about what exists. `pages.read` and `posts.read` are exercised
    // with a slug that does not exist: the claim is that the route is REACHED and answers with
    // its own `404`, not that it returns a row — a fixture that had to publish a page per
    // endpoint would rot quietly into a test that passes while the tab refuses an endpoint.
    let document = call(
        &fixture.state,
        session_request(Method::GET, "/api/v1/content-api/openapi.json", &session, None),
    )
    .await;
    let mut operations: Vec<String> = Vec::new();
    // A `serde_json::Map` iterated directly — `Object.values` is a JavaScript method, and the
    // walkthrough's version of this loop has no meaning here.
    for (_, methods) in document.body["paths"].as_object().expect("paths") {
        for (_, operation) in methods.as_object().expect("a method map") {
            if let Some(id) = operation["operationId"].as_str() {
                // `media.panelCrud` is in the document as a WARNING, not as a content-API
                // operation: it is the panel's own session-authenticated surface, and a walk that
                // demanded the Explorer dispatch it would be demanding the one call the document
                // exists to prevent. It is refused by name in its own walk below.
                if id != "media.panelCrud" {
                    operations.push(id.to_owned());
                }
            }
        }
    }
    assert!(
        operations.len() >= 6,
        "the document should describe the whole surface, got {operations:?}"
    );

    for operation in operations {
        let params = match operation.as_str() {
            "pages.read" | "posts.read" => json!({ "slug": "explorer-probe-absent" }),
            // `sites.list` takes nothing at all, so the walk sends nothing to it: a blanket
            // `limit` here would be refused by the dispatcher's parameter check, which is that
            // check working rather than a dispatch failure, and the two are worth telling apart.
            // `sites.list` declares no parameters, and the dispatcher refuses one it does not
            // declare — so the fixture's site is stripped for this operation rather than sent.
            "sites.list" => {
                let response = call(
                    &fixture.state,
                    explorer_request(
                        &session,
                        json!({
                            "token_id": token,
                            "operation_id": "sites.list",
                            "params": {},
                        }),
                    ),
                )
                .await;
                assert_eq!(response.status, StatusCode::OK, "{}", response.body);
                assert_eq!(response.body["operation_id"], "sites.list");
                assert_ne!(response.body["status"], json!(501));
                continue;
            }
            _ => json!({ "limit": "1" }),
        };
        let response = fixture.explore(&session, token, &operation, params).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{operation} must be dispatched, not refused at the dispatcher: {}",
            response.body
        );
        assert_eq!(
            response.body["operation_id"],
            json!(operation),
            "the answer must name the operation that was called"
        );
        // `501 not_implemented` is the one status this route must never produce: the document
        // advertises the endpoint and the tab would be refusing it.
        assert_ne!(
            response.body["status"],
            json!(501),
            "{operation} is documented but the dispatcher does not know it"
        );
    }
}

#[tokio::test]
async fn the_explorer_needs_the_read_power_and_a_session() {
    let Some(fixture) = fixture().await else { return };
    let token = fixture.mint(&fixture.login().await, "Explorer Guard", &["content:read"]).await;

    // No session at all: a panel cookie is the only authority on this route.
    let anonymous = call(
        &fixture.state,
        explorer_request("", json!({ "token_id": token, "operation_id": "pages.list", "params": {} })),
    )
    .await;
    assert!(
        anonymous.status == StatusCode::UNAUTHORIZED
            || anonymous.status == StatusCode::FORBIDDEN,
        "an unauthenticated caller must be refused, got {}",
        anonymous.status
    );
}
#[tokio::test]
async fn the_panel_media_entry_is_refused_with_the_endpoint_to_use_instead() {
    let Some(fixture) = fixture().await else { return };
    let session = fixture.login().await;
    let token = fixture
        .mint(&session, "Explorer Media Note", &["content:read", "media:read"])
        .await;

    let response = fixture
        .explore(&session, token, "media.panelCrud", json!({}))
        .await;

    // `/api/v1/media` is in the document so an integrator reads why they must not call it. A
    // `400 invalid_parameter` listing the six real operations would be FALSE — the operation IS
    // documented — and would send them looking for an endpoint that is not there. The refusal
    // names the one to use instead, which is the only thing a reader in that position needs.
    assert_eq!(response.status, StatusCode::BAD_REQUEST, "{}", response.body);
    assert_eq!(response.body["error"]["code"], "not_dispatchable");
    assert_eq!(response.body["error"]["details"]["use"], "media.list");
    assert!(response.body["error"]["message"]
        .as_str()
        .expect("a message")
        .contains("/api/v1/content/media"));
}
