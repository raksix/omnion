//! Integration tests for the CDN cache-rule surface (REQ-011, slice 1).
//!
//! Three things are proved here that a unit test cannot:
//!
//!   * **The migration applies** and the two settings indexes behave — a site row and a
//!     platform row can both exist, but two platform rows cannot.
//!   * **The API is scoped.** An account in one organization cannot read, change or even
//!     learn the existence of another organization's rule.
//!   * **A rule survives a round trip** and a reorder persists the new precedence, which
//!     is the slice's own "Done" sentence.
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
use omnion_security::RatePolicy;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

/// The keys this suite's administrator holds.
const ADMIN_PERMISSIONS: [&str; 3] = ["cdn.read", "cdn.manage", "cdn.purge"];

struct TestResponse {
    status: StatusCode,
    /// The `Set-Cookie` value, when the response set one.
    set_cookie: String,
    body: Value,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");

    let status = response.status();
    // The session token arrives in a cookie and *nowhere else* — the login body carries no
    // `token` field. A harness that reads `body["token"]` therefore reports "the fixture
    // account cannot sign in" for a login that succeeded, which is how this suite looked
    // like sixteen product failures instead of one broken helper.
    let set_cookie = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>()
        .join("; ");
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
        set_cookie,
        body,
    }
}

/// A signed-in account's cookie jar, in the shape a browser actually holds.
///
/// `login()` used to return the session token alone. Sign-in issues a session **and** a CSRF
/// token, so a suite that keeps the first has built a client the platform is right to refuse
/// — and since this file's state carried no secret at all, the layer answered
/// `csrf_unavailable` and every mutation here was refused before its handler.
///
/// `Deref<Target = str>` keeps the fix to one function: the twenty-nine `&token_a` sites,
/// the `&str` parameters and every `format!("{token}")` compile unchanged while the second
/// cookie travels with the first. A session without its token is now unrepresentable rather
/// than merely unlikely. `Debug` is hand-written to print `<redacted>`, because deriving it
/// would write a live session token into every CI log.
#[derive(Clone)]
struct Credentials {
    session: String,
    csrf: Option<String>,
}

impl std::ops::Deref for Credentials {
    type Target = str;

    fn deref(&self) -> &str {
        &self.session
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("session", &"<redacted>")
            .field("csrf", &self.csrf.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

fn request(
    method: Method,
    uri: &str,
    credentials: Option<&Credentials>,
    body: Option<Value>,
) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    // Both cookies go out, plus the matching `x-omnion-csrf` header — which is what a browser
    // does, and what this file did not. The state below used to carry no CSRF secret at all,
    // so `refuse_if_needed` answered every mutation with `403 csrf_unavailable` **before the
    // handler ran**. A suite in that state passes every read half and silently measures
    // nothing about writes: it is the same green a build that never implemented the route
    // would produce. This is the sixth file to need the fix.
    let builder = match credentials {
        Some(credentials) => {
            let mut cookies = format!("omnion_session={}", credentials.session);
            let builder = match &credentials.csrf {
                Some(csrf) => {
                    cookies.push_str(&format!("; omnion_csrf={csrf}"));
                    builder.header("x-omnion-csrf", csrf.as_str())
                }
                None => builder,
            };
            builder.header(header::COOKIE, cookies)
        }
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

fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

async fn live_state() -> Option<(AppState, Db)> {
    let mut config = Config::from_env().ok()?;
    // The secret goes on the **config**, not through the environment. Sign-in only issues a
    // token when the running state carries one, and a suite that relies on
    // `OMNION_CSRF_SECRET` being exported in the shell is a suite that silently stops
    // testing writes the moment it is not.
    config.csrf = omnion_core::config::CsrfSecret::new(Some("cdn-suite-csrf-key".to_owned()));
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
    give_the_suite_its_own_rate_limit(&state);
    Some((state, db))
}

/// Two organizations, each with a site and an administrator holding the CDN keys.
struct Fixture {
    state: AppState,
    db: Db,
    site_a: Uuid,
    site_b: Uuid,
    token_a: Credentials,
    token_b: Credentials,
    admin_a: Uuid,
    accounts: Vec<Uuid>,
    organizations: Vec<Uuid>,
}

/// Raise the `sign_in` ceiling for this suite.
///
/// This file builds a fresh `Fixture` per walk and each one signs two accounts in, so fifteen
/// walks are thirty-plus sign-ins against a budget of ten per five minutes. The surplus
/// failures arrive as `429 rate_limited` in the middle of an assertion about cache rules —
/// and the walk that adds a cross-tenant toggle to the cross-tenant test was the one that
/// finally tripped it, which is why a suite that had been green for a dozen walks went red
/// on an unrelated line. This is the FIFTH file to need it, after `media.rs`, `tenancy.rs`,
/// `tenancy_members`, `tenancy_departments`, `tenancy_limits` and `cdn_purge.rs`.
///
/// Only `sign_in` is raised. The other ceilings are the ones a deployment ships, and a suite
/// that lifts those becomes the reason a genuinely over-budget request stops being refused.
fn give_the_suite_its_own_rate_limit(state: &AppState) {
    let policies: Vec<RatePolicy> = RatePolicy::defaults()
        .into_iter()
        .map(|mut policy| {
            if policy.scope == "sign_in" {
                policy.limit = 10_000;
            }
            policy
        })
        .collect();
    let _ = omnion_api::rate_limit_middleware::install(
        omnion_api::rate_limit_middleware::RateLimiter::new(state, policies),
    );
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let org_a = create_organization_row(&db, "a").await;
        let org_b = create_organization_row(&db, "b").await;
        let (admin_a, token_a) = create_admin(&db, org_a, "a", &state).await;
        let (admin_b, token_b) = create_admin(&db, org_b, "b", &state).await;
        let site_a = create_site(&db, org_a, "a").await;
        let site_b = create_site(&db, org_b, "b").await;

        let fixture = Self {
            state,
            db,
            site_a,
            site_b,
            token_a,
            token_b,
            admin_a,
            accounts: vec![admin_a, admin_b],
            organizations: vec![org_a, org_b],
        };
        fixture.create_rules("cdn-a", 2).await;
        fixture.create_rules("cdn-b", 1).await;
        Some(fixture)
    }

    /// Seed a prefix's worth of rules so the cleanup can find them all.
    async fn create_rules(&self, prefix: &str, count: usize) {
        for index in 0..count {
            let site = if prefix.ends_with('a') {
                self.site_a
            } else {
                self.site_b
            };
            let token = if prefix.ends_with('a') {
                &self.token_a
            } else {
                &self.token_b
            };
            let response = call(
                &self.state,
                request(
                    Method::POST,
                    "/api/v1/cdn/rules",
                    Some(token),
                    Some(json!({
                        "site_id": site,
                        "name": format!("{prefix}-rule-{index}"),
                        "path_pattern": format!("/cdn-{prefix}/**"),
                    })),
                ),
            )
            .await;
            assert_eq!(
                response.status,
                StatusCode::CREATED,
                "seeding a rule must succeed: {}",
                response.body
            );
        }
    }

    async fn cleanup(self) {
        for organization in &self.organizations {
            sqlx::query("delete from organizations where id = $1")
                .bind(organization)
                .execute(self.db.pool())
                .await
                .ok();
        }
        for account in &self.accounts {
            sqlx::query("delete from users where id = $1")
                .bind(*account)
                .execute(self.db.pool())
                .await
                .ok();
        }
    }
}

async fn create_organization_row(db: &Db, suffix: &str) -> Uuid {
    let name = format!("CDN Test {suffix}");
    let slug = format!("cdn-test-{suffix}-{}", Uuid::new_v4().simple());
    let id: Uuid =
        sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
            .bind(&name)
            .bind(&slug)
            .fetch_one(db.pool())
            .await
            .expect("organization must insert");
    id
}

async fn create_site(db: &Db, organization_id: Uuid, suffix: &str) -> Uuid {
    let key = format!("main{suffix}");
    sites::create_site(
        db.pool(),
        omnion_identity::NewSite {
            organization_id,
            key,
            name: format!("CDN Site {suffix}"),
            theme: None,
        },
    )
    .await
    .expect("site must insert")
    .id
}

async fn create_admin(
    db: &Db,
    organization_id: Uuid,
    suffix: &str,
    state: &AppState,
) -> (Uuid, Credentials) {
    let email = format!("cdn-admin-{suffix}-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "CDN Admin".to_owned(),
            organization_id: Some(organization_id),
        },
    )
    .await
    .expect("account must insert");

    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id,
            key: format!("cdn-admin-{suffix}"),
            name: format!("CDN Admin {suffix}"),
            description: "cache rules".to_owned(),
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
        .expect("the role permission set must be written");
    bindings::grant(
        db.pool(),
        NewBinding {
            role_id: role.id,
            user_id: user.id,
            scope: Scope::Organization { organization_id },
            granted_by: None,
            expires_at: None,
        },
    )
    .await
    .expect("binding must insert");

    let token = login(state, &email).await;
    (user.id, token)
}

/// Sign an account in and return the raw session token.
async fn login(state: &AppState, email: &str) -> Credentials {
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
    // `get_all`, not `get`: sign-in sends TWO Set-Cookie headers, and reading one is
    // indistinguishable from a deployment that issues no CSRF token at all.
    let set_cookie = response.set_cookie.clone();
    assert!(
        !set_cookie.is_empty(),
        "login must set the session cookie; sent: {set_cookie:?}"
    );
    // Name each cookie: a missing one must be visible HERE, naming which cookie the platform
    // did not send, instead of surfacing three layers away as a 403.
    let cookie_value = |name: &str| -> Option<String> {
        set_cookie
            .split(';')
            .filter_map(|pair| pair.trim().split_once('='))
            .find(|(cookie, _)| *cookie == name)
            .map(|(_, value)| value.to_owned())
    };
    let session = cookie_value("omnion_session")
        .unwrap_or_else(|| panic!("login must set the omnion_session cookie; sent: {set_cookie}"));
    let csrf = cookie_value("omnion_csrf");
    assert!(
        csrf.is_some(),
        "login must set the omnion_csrf cookie beside the session one; sent: {set_cookie}"
    );
    Credentials { session, csrf }
}

/// The rules of a site, in the order the API returns them.
async fn rules_of(state: &AppState, token: &Credentials, site_id: Uuid) -> Vec<Value> {
    let response = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/cdn/rules?site_id={site_id}"),
            Some(token),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "list must answer");
    response.body["rules"]
        .as_array()
        .cloned()
        .expect("rules must be an array")
}

#[tokio::test]
async fn a_site_with_no_rules_answers_an_empty_list_not_an_error() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // A third site, in the first organization, with nothing configured.
    let site = create_site(&fixture.db, fixture.organizations[0], "c").await;
    let rules = rules_of(&fixture.state, &fixture.token_a, site).await;
    assert!(rules.is_empty(), "a fresh site has no rules: {rules:?}");
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_created_rule_comes_back_with_the_values_it_was_given() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/cdn/rules",
            Some(&fixture.token_a),
            Some(json!({
                "site_id": fixture.site_a,
                "name": "blog section",
                "path_pattern": "/blog/**",
                "edge_ttl_seconds": 3600,
                "browser_ttl_seconds": 120,
                "swr_seconds": 30,
            })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "{}", response.body);
    assert_eq!(response.body["name"], "blog section");
    assert_eq!(response.body["path_pattern"], "/blog/**");
    assert_eq!(response.body["edge_ttl_seconds"], 3600);
    assert_eq!(response.body["enabled"], true);
    // Defaults the form did not send.
    assert_eq!(response.body["browser_ttl_seconds"], 120);
    assert_eq!(
        response.body["methods"],
        json!(["GET", "HEAD"]),
        "a read rule defaults to GET and HEAD"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_new_rule_is_appended_after_the_existing_ones() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let before = rules_of(&fixture.state, &fixture.token_a, fixture.site_a).await;
    call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/cdn/rules",
            Some(&fixture.token_a),
            Some(json!({
                "site_id": fixture.site_a,
                "name": "appended",
                "path_pattern": "/appended/**",
            })),
        ),
    )
    .await;
    let after = rules_of(&fixture.state, &fixture.token_a, fixture.site_a).await;
    assert_eq!(after.len(), before.len() + 1);
    assert_eq!(
        after.last().expect("a rule was appended")["name"],
        "appended",
        "the newest rule lands last, not first"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn an_empty_name_is_refused_with_a_message_under_the_name_field() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/cdn/rules",
            Some(&fixture.token_a),
            Some(json!({
                "site_id": fixture.site_a,
                "name": "   ",
                "path_pattern": "/x/**",
            })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.body["error"]["code"], "invalid_cache_rule");
    assert_eq!(response.body["error"]["details"]["field"], "name");
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_malformed_pattern_is_refused_and_names_the_pattern_field() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/cdn/rules",
            Some(&fixture.token_a),
            Some(json!({
                "site_id": fixture.site_a,
                "name": "bad pattern",
                "path_pattern": "blog/**",
            })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.body["error"]["code"], "invalid_path_pattern");
    assert_eq!(response.body["error"]["details"]["field"], "path_pattern");
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_ttl_above_the_one_year_cap_is_refused_and_names_the_ttl_field() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/cdn/rules",
            Some(&fixture.token_a),
            Some(json!({
                "site_id": fixture.site_a,
                "name": "too long",
                "path_pattern": "/x/**",
                "edge_ttl_seconds": 31_536_001i64,
            })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.body["error"]["code"], "invalid_ttl");
    assert_eq!(
        response.body["error"]["details"]["field"],
        "edge_ttl_seconds"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_duplicate_name_on_the_same_site_is_refused() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/cdn/rules",
            Some(&fixture.token_a),
            Some(json!({
                "site_id": fixture.site_a,
                "name": "CDN-A-rule-0",
                "path_pattern": "/dupe/**",
            })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.body["error"]["code"], "duplicate_rule_name");
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_toggle_flips_the_flag_without_touching_the_rest_of_the_rule() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let rules = rules_of(&fixture.state, &fixture.token_a, fixture.site_a).await;
    let id = rules[0]["id"].as_str().expect("an id").to_string();

    let response = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/cdn/rules/{id}/toggle"),
            Some(&fixture.token_a),
            Some(json!({ "site_id": fixture.site_a, "enabled": false })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(response.body["enabled"], false);
    assert_eq!(
        response.body["path_pattern"], rules[0]["path_pattern"],
        "a toggle must not rewrite the rule"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_reorder_persists_the_new_precedence() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let before = rules_of(&fixture.state, &fixture.token_a, fixture.site_a).await;
    let ids: Vec<&str> = before
        .iter()
        .rev()
        .map(|rule| rule["id"].as_str().expect("an id"))
        .collect();

    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/cdn/rules/reorder",
            Some(&fixture.token_a),
            Some(json!({ "site_id": fixture.site_a, "order": ids })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);

    let after = rules_of(&fixture.state, &fixture.token_a, fixture.site_a).await;
    let after_ids: Vec<&str> = after
        .iter()
        .map(|rule| rule["id"].as_str().expect("an id"))
        .collect();
    // `ids` is the order the client SENT (the reverse of the order it read). That is the
    // order the store must now report. Comparing against the order it started in would
    // pass for a store that ignored the request entirely and fail for one that obeyed it.
    assert_eq!(
        after_ids, ids,
        "the order the client sent is the order stored"
    );
    let priorities: Vec<i64> = after
        .iter()
        .map(|rule| rule["priority"].as_i64().expect("a priority"))
        .collect();
    assert_eq!(priorities, vec![0, 1], "priorities are rewritten as 0,1,2…");
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_reorder_that_omits_a_rule_is_refused_and_changes_nothing() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let before = rules_of(&fixture.state, &fixture.token_a, fixture.site_a).await;
    let first: String = before[0]["id"].as_str().expect("an id").to_string();

    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/cdn/rules/reorder",
            Some(&fixture.token_a),
            Some(json!({ "site_id": fixture.site_a, "order": [first] })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.body["error"]["code"], "incomplete_reorder");

    let after = rules_of(&fixture.state, &fixture.token_a, fixture.site_a).await;
    assert_eq!(
        before.iter().map(|r| &r["id"]).collect::<Vec<_>>(),
        after.iter().map(|r| &r["id"]).collect::<Vec<_>>(),
        "a refused reorder must leave the order untouched"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_deleted_rule_is_gone_from_the_list() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let before = rules_of(&fixture.state, &fixture.token_a, fixture.site_a).await;
    let id = before[0]["id"].as_str().expect("an id").to_string();

    let response = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/cdn/rules/{id}"),
            Some(&fixture.token_a),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);

    let after = rules_of(&fixture.state, &fixture.token_a, fixture.site_a).await;
    assert_eq!(after.len(), before.len() - 1);
    assert!(!after.iter().any(|rule| rule["id"] == id.as_str()));
    fixture.cleanup().await;
}

#[tokio::test]
async fn another_organizations_rule_is_neither_readable_nor_writable() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // A rule that belongs to organization B.
    let theirs = rules_of(&fixture.state, &fixture.token_b, fixture.site_b).await;
    let id = theirs[0]["id"].as_str().expect("an id").to_string();

    // Organization A may not read it...
    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/cdn/rules/{id}"),
            Some(&fixture.token_a),
            None,
        ),
    )
    .await;
    assert_eq!(read.status, StatusCode::FORBIDDEN, "{}", read.body);
    assert_eq!(read.body["error"]["code"], "permission_denied");

    // ...nor change it, even while naming its own site.
    let write = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/cdn/rules/{id}"),
            Some(&fixture.token_a),
            None,
        ),
    )
    .await;
    assert_eq!(write.status, StatusCode::FORBIDDEN);

    // ...nor flip its live flag, which is the one this test did not cover until the store
    // was fixed. `POST /rules/{id}/toggle` took a `site_id` from the *body* and used the path
    // id alone in its `WHERE` clause, then compared the row it had already written against
    // the caller's site and answered `403`. So a caller in A who named a rule id belonging to
    // B got a refusal **and** changed B's cache rule -- the audit trail recorded the attempt
    // and the other tenant's rules were off anyway. The `403` is what made the test suite
    // call this endpoint guarded.
    //
    // The assertion that catches it is the *state*, not the status: a walk that stopped at
    // "the caller was refused" passes against a build that refuses after writing. So the
    // flag is read back through B's own list, which is the only reading that can see it.
    let before = theirs[0]["enabled"].clone();
    let toggle = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/cdn/rules/{id}/toggle"),
            Some(&fixture.token_a),
            Some(json!({ "site_id": fixture.site_a, "enabled": !before.as_bool().unwrap_or(true) })),
        ),
    )
    .await;
    assert!(
        toggle.status == StatusCode::NOT_FOUND || toggle.status == StatusCode::FORBIDDEN,
        "a cross-tenant toggle must be refused, not applied: {}",
        toggle.body
    );
    let afterwards = rules_of(&fixture.state, &fixture.token_b, fixture.site_b).await;
    let theirs_now = afterwards
        .iter()
        .find(|rule| rule["id"].as_str() == Some(id.as_str()))
        .expect("B's rule is still there")
        .clone();
    assert_eq!(
        theirs_now["enabled"], before,
        "the refusal must not have changed the other organization's rule"
    );

    // The same id under B's own site_id is the ordinary success, so the test above is
    // measuring the scope and not a route that never works.
    let own = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/cdn/rules/{id}/toggle"),
            Some(&fixture.token_b),
            Some(json!({ "site_id": fixture.site_b, "enabled": false })),
        ),
    )
    .await;
    assert_eq!(own.status, StatusCode::OK, "{}", own.body);
    assert_eq!(own.body["enabled"], false);

    // ...nor name its site at all.
    let list = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/cdn/rules?site_id={}", fixture.site_b),
            Some(&fixture.token_a),
            None,
        ),
    )
    .await;
    assert_eq!(list.status, StatusCode::FORBIDDEN);
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_caller_without_the_cdn_keys_is_refused() {
    let Some((state, db)) = live_state().await else {
        return;
    };
    let organization = create_organization_row(&db, "noperm").await;
    let site = create_site(&db, organization, "noperm").await;
    let email = format!("cdn-noperm-{}@omnion.test", Uuid::new_v4().simple());
    // An account with no role at all: the guard, not the scope check, must refuse it.
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "No Permissions".to_owned(),
            organization_id: Some(organization),
        },
    )
    .await
    .expect("account must insert");
    let token = login(&state, &email).await;

    let response = call(
        &state,
        request(
            Method::GET,
            &format!("/api/v1/cdn/rules?site_id={site}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN, "{}", response.body);
    assert_eq!(response.body["error"]["code"], "permission_denied");

    sqlx::query("delete from users where id = $1")
        .bind(user.id)
        .execute(db.pool())
        .await
        .ok();
    sqlx::query("delete from organizations where id = $1")
        .bind(organization)
        .execute(db.pool())
        .await
        .ok();
}

#[tokio::test]
async fn the_two_settings_rows_are_both_allowed_and_the_platform_row_is_unique() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let pool = fixture.db.pool();
    // A per-site row and the installation row coexist.
    sqlx::query("insert into cdn_settings (site_id, provider) values ($1, 'origin')")
        .bind(fixture.site_a)
        .execute(pool)
        .await
        .expect("a site row must insert");
    // The platform row is installation-wide and survives `cleanup`, so a leftover from a
    // previous run would be refused here and read as the partial index being broken.
    sqlx::query("delete from cdn_settings where site_id is null")
        .execute(pool)
        .await
        .expect("a stale platform row must be removable");
    sqlx::query("insert into cdn_settings (site_id, provider) values (null, 'origin')")
        .execute(pool)
        .await
        .expect("the platform row must insert");

    // A second platform row is refused — this is the partial index's whole job.
    let second =
        sqlx::query("insert into cdn_settings (site_id, provider) values (null, 'origin')")
            .execute(pool)
            .await;
    assert!(
        second.is_err(),
        "two platform rows must not both be accepted"
    );

    // A second row for the same site is refused too.
    let duplicate =
        sqlx::query("insert into cdn_settings (site_id, provider) values ($1, 'origin')")
            .bind(fixture.site_a)
            .execute(pool)
            .await;
    assert!(duplicate.is_err(), "one settings row per site");

    // The settings read path falls back to the platform row for a site without one.
    let resolved = omnion_cdn::store::resolve_settings(pool, Some(fixture.site_b))
        .await
        .expect("the platform row must answer for a site with none");
    assert!(
        resolved.site_id.is_none(),
        "the fallback is the platform row"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_mutation_writes_an_audit_entry_naming_the_actor_and_the_action() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let before: i64 =
        sqlx::query_scalar("select count(*) from audit_log where action = 'cdn.rule.changed'")
            .fetch_one(fixture.db.pool())
            .await
            .expect("audit table must be readable");

    call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/cdn/rules",
            Some(&fixture.token_a),
            Some(json!({
                "site_id": fixture.site_a,
                "name": "audited",
                "path_pattern": "/audited/**",
            })),
        ),
    )
    .await;

    let entry: (String, Option<Uuid>, Option<String>) = sqlx::query_as(
        "select action, actor_user_id, ip_address from audit_log \
         where action = 'cdn.rule.changed' order by created_at desc limit 1",
    )
    .fetch_one(fixture.db.pool())
    .await
    .expect("the mutation must leave an audit row");
    assert_eq!(entry.0, "cdn.rule.changed");
    assert_eq!(entry.1, Some(fixture.admin_a), "the actor is recorded");
    let after: i64 =
        sqlx::query_scalar("select count(*) from audit_log where action = 'cdn.rule.changed'")
            .fetch_one(fixture.db.pool())
            .await
            .expect("audit table must be readable");
    assert_eq!(after, before + 1);
    fixture.cleanup().await;
}
