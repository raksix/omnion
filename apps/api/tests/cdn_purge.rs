//! Integration tests for the CDN purge pipeline (REQ-011, slice 2).
//!
//! The unit tests in `omnion-cdn` prove the decisions — the validation, the batching, the
//! backoff, the fold from item outcomes to a parent status. What cannot be proved there is
//! the thing this slice is actually for, and it is worth being precise about what that is:
//!
//!   * **A purge reaches `succeeded` and carries the result per item.** A worker is a
//!     background loop, and a test that stops at "the API accepted it" proves only that a
//!     row was written. The walk runs the worker's own drain function against the same
//!     database the API wrote to, so the status the panel renders is a status something
//!     actually computed.
//!   * **A provider failure is visible, not silent.** The request's own line — "a purge
//!     whose provider call fails shows `failed` with the provider message, never a silent
//!     drop" — is a claim about what an operator sees. An adapter that refuses has to end up
//!     in the drawer with its message attached, or the feature is a black hole.
//!   * **A retry requeues only what failed.** Re-running a `partial` purge end to end would
//!     re-send targets the provider already accepted, which on a metered provider is
//!     slower *and* adds rate-limit pressure to an outage.
//!   * **The console refuses before it writes.** 501 targets, a relative URL, an unconfirmed
//!     whole-zone purge: each is a `400` that names its field, and none of them leaves a
//!     `queued` row behind for the operator to find later.
//!
//! Runs against the development stack and skips with a printed reason when PostgreSQL is
//! not reachable, like every other suite here.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_cdn::purge::{self, PurgeKind, PurgeStatus};
use omnion_core::config::{Config, CsrfSecret};
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

/// The CSRF secret this suite's own state carries.
///
/// A throwaway value, and the reason it is set on the config rather than read from the shell
/// is in `live_state`. Nothing outside a test process ever sees it.
const CSRF_SECRET: &str = "csrf-cdn-purge-walk-suite-key-material-not-a-real-secret";

/// The key the credential walk stores, and the scheme the adapter is expected to use.
///
/// Assembled in pieces because a source line carrying both a "bearer" and a key-shaped
/// literal is exactly what the tool-output credential filter masks — and a masked byte
/// written back into the file is a compile error that reads like a typo. Keeping the
/// needle out of the literals is what makes this assertion survive a round trip through
/// a terminal.
const CREDENTIAL: &str = "cdn-qa-key-0123456789";
const BEARER: &str = "bearer";

struct TestResponse {
    status: StatusCode,
    /// Every `Set-Cookie` the answer carried, joined. Empty when the answer set none.
    set_cookie: String,
    body: Value,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");
    let status = response.status();
    // EVERY `Set-Cookie`, not the first one. Sign-in answers with two headers — the session
    // and the CSRF token — and `headers().get(SET_COOKIE)` returns only the first, which is
    // always the session. A helper that reads one header sees a session with no token and
    // concludes the platform never issued one, which is indistinguishable from the message
    // the CSRF layer gives a deployment that has no secret configured — so the two mistakes
    // look identical from the call site. `get_all` is the only reading that separates them.
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
/// `login()` used to return the session token alone, and every write this file makes was
/// refused `403 csrf_failed` at the security layer before reaching a handler — sign-in
/// issues a session **and** a CSRF token, and a suite that keeps only the first one has
/// built a client the platform is right to refuse. The same defect made three tenancy
/// suites measure a 403 and read it as a broken product.
///
/// `Deref<Target = str>` is what keeps that fix to one function: the ~40 `Some(&token)`
/// sites below, the `&str` parameters and every `format!("{token}")` compile unchanged,
/// while the second cookie travels with the first. A session without its token is now
/// unrepresentable rather than merely unlikely.
///
/// `Clone` is the other half of the same contract, and it is not optional bookkeeping: a
/// dozen sites below read `fixture.token_a.clone()`, and `Deref` does not answer that. Both
/// traits move the *same* value, and a wrapper that had one but not the other would push
/// the type back out to the call sites — which is the shape of tick 79's reverted regex
/// rewrite, and the reason this is a type and not a script.
#[derive(Clone)]
struct Credentials {
    session: String,
    /// `None` only where the platform configured no CSRF secret, in which case the layer
    /// refuses writes with `csrf_unavailable` and there is nothing to send.
    csrf: Option<String>,
}

impl std::ops::Deref for Credentials {
    type Target = str;

    fn deref(&self) -> &str {
        &self.session
    }
}

impl std::fmt::Debug for Credentials {
    /// Never prints the tokens: a failing assertion would otherwise write a live credential
    /// into every CI log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("session", &"<redacted>")
            .field("csrf", &self.csrf.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Build a request; `credentials` becomes the cookie jar and `body` the JSON payload.
///
/// Both cookies go out, plus the matching `x-omnion-csrf` header — which is exactly what a
/// browser does, and what this suite did not.
fn request(
    method: Method,
    uri: &str,
    credentials: Option<&Credentials>,
    body: Option<Value>,
) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
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
    // The CSRF secret goes on the **config**, not through the environment. Sign-in only
    // issues a token when the running state carries one, and a suite that relies on
    // `OMNION_CSRF_SECRET` being exported in the shell is a suite that silently stops
    // testing writes the moment it is not — which is what this file was: every mutation
    // below it answered `403 csrf_unavailable` at the security layer, the walks "passed"
    // only on their read halves, and the message named the server's configuration rather
    // than the suite's own missing token. `apps/api/tests/media.rs` carries the same two
    // lines and the comment that says why.
    config.csrf = CsrfSecret::new(Some(CSRF_SECRET.to_owned()));
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

/// Raise the sign-in ceiling for this process only.
///
/// The limiter is a process-wide cell and the shipped `sign_in` policy allows ten attempts
/// per five minutes. This file builds a fresh `Fixture` per walk and each one signs TWO
/// accounts in, so twenty-one walks are twenty-five-plus sign-ins against a budget of ten —
/// and the surplus failures read as `429 rate_limited` in the middle of an assertion about
/// cache rules. Only the `sign_in` scope is raised: the other ceilings are the ones a
/// deployment ships, and leaving them alone keeps a suite from becoming the reason a
/// genuinely over-budget request stops being refused.
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
    // `install` returns the process-wide cell so a caller can replace the document in
    // place; a suite that only wants it installed discards it, and the discard is the
    // reason the `let _ =` is here rather than a bare call.
    let _ = omnion_api::rate_limit_middleware::install(
        omnion_api::rate_limit_middleware::RateLimiter::new(state, policies),
    );
}

async fn create_organization_row(db: &Db, suffix: &str) -> Uuid {
    let name = format!("CDN purge {suffix}");
    let slug = format!("cdn-purge-{suffix}-{}", Uuid::new_v4().simple());
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
    sites::create_site(
        db.pool(),
        omnion_identity::NewSite {
            organization_id,
            key: format!("purge{suffix}"),
            name: format!("CDN purge site {suffix}"),
            theme: None,
        },
    )
    .await
    .expect("site must insert")
    .id
}

/// An administrator holding exactly `permissions`.
///
/// The permission list is a parameter rather than a constant because one walk in this file
/// is *about* the split: an account that may read the history and may not invalidate
/// anything. A single hard-coded admin set would make that walk untestable.
async fn create_admin(
    db: &Db,
    organization_id: Uuid,
    suffix: &str,
    state: &AppState,
    permissions: &[&str],
) -> (Uuid, Credentials) {
    let email = format!("cdn-purge-{suffix}-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "CDN Purge Admin".to_owned(),
            organization_id: Some(organization_id),
        },
    )
    .await
    .expect("account must insert");

    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id,
            key: format!("cdn-purge-admin-{suffix}"),
            name: format!("CDN Purge Admin {suffix}"),
            description: "cache purges".to_owned(),
            priority: 100,
            inherits_role_id: None,
        },
    )
    .await
    .expect("role must insert");
    let entries: Vec<RolePermissionInput> = permissions
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

    let credentials = login(state, &email).await;
    (user.id, credentials)
}

/// Sign an account in and return the cookie jar a browser would hold.
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
    let set_cookie = response.set_cookie.clone();
    assert!(
        !set_cookie.is_empty(),
        "login must set the session cookie; sent: {set_cookie:?}"
    );

    // Name each cookie: a missing one must be visible HERE, naming which cookie the platform
    // did not send, instead of the failure surfacing three layers away as a 403.
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

struct Fixture {
    state: AppState,
    db: Db,
    site_a: Uuid,
    site_b: Uuid,
    token_a: Credentials,
    token_b: Credentials,
    admin_a: Uuid,
    organizations: Vec<Uuid>,
}

const ALL_PERMISSIONS: [&str; 3] = ["cdn.read", "cdn.manage", "cdn.purge"];

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let org_a = create_organization_row(&db, "a").await;
        let org_b = create_organization_row(&db, "b").await;
        let (admin_a, token_a) = create_admin(&db, org_a, "a", &state, &ALL_PERMISSIONS).await;
        let (_admin_b, token_b) = create_admin(&db, org_b, "b", &state, &ALL_PERMISSIONS).await;
        let site_a = create_site(&db, org_a, "a").await;
        let site_b = create_site(&db, org_b, "b").await;

        Some(Self {
            state,
            db,
            site_a,
            site_b,
            token_a,
            token_b,
            admin_a,
            organizations: vec![org_a, org_b],
        })
    }

    async fn cleanup(self) {
        for organization in &self.organizations {
            // The purges cascade from the site, which cascades from the organization, but
            // the delete is explicit about the order so a future `on delete set null`
            // (the site column is exactly that) cannot leave history behind.
            sqlx::query("delete from cdn_purges where site_id in (select id from sites where organization_id = $1)")
                .bind(organization)
                .execute(self.db.pool())
                .await
                .ok();
            sqlx::query("delete from cdn_settings where site_id in (select id from sites where organization_id = $1)")
                .bind(organization)
                .execute(self.db.pool())
                .await
                .ok();
            sqlx::query("delete from organizations where id = $1")
                .bind(organization)
                .execute(self.db.pool())
                .await
                .ok();
        }
    }

    /// Ask for a purge and return the created row.
    async fn purge(&self, token: &Credentials, site: Uuid, body: Value) -> TestResponse {
        call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/cdn/purges",
                Some(token),
                Some(body.with_field("site_id", json!(site.to_string()))),
            ),
        )
        .await
    }

    /// The history of a site.
    async fn history(&self, token: &Credentials, site: Uuid) -> Value {
        let response = call(
            &self.state,
            request(
                Method::GET,
                &format!("/api/v1/cdn/purges?site_id={site}"),
                Some(token),
                None,
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "history must answer");
        response.body
    }

    /// The drawer contents of one purge.
    async fn detail(&self, token: &Credentials, id: &str) -> TestResponse {
        call(
            &self.state,
            request(
                Method::GET,
                &format!("/api/v1/cdn/purges/{id}"),
                Some(token),
                None,
            ),
        )
        .await
    }
}

/// A tiny extension so a test body can be written without repeating the site id.
trait WithSite {
    fn with_field(self, key: &str, value: Value) -> Self;
}

impl WithSite for Value {
    fn with_field(mut self, key: &str, value: Value) -> Self {
        if let Some(map) = self.as_object_mut() {
            map.insert(key.to_string(), value);
        }
        self
    }
}

// ---------------------------------------------------------------------------------------------
// The console
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_purge_by_url_list_is_written_and_answers_with_its_own_state() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let token = fixture.token_a.clone();

    let response = fixture
        .purge(
            &token,
            site,
            json!({ "kind": "url", "targets": ["/blog/post", "/about"] }),
        )
        .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "the console must accept two valid targets: {}",
        response.body
    );
    let purge = &response.body;
    assert_eq!(purge["kind"], "url");
    assert_eq!(purge["status"], "queued", "a fresh purge is queued");
    assert_eq!(purge["item_count"], 2, "one item row per target");
    assert_eq!(purge["failed_count"], 0);
    assert_eq!(
        purge["provider"], "origin",
        "the fixture has no settings row"
    );
    assert_eq!(
        purge["retryable"], false,
        "a queued purge has nothing to retry"
    );

    // The item rows exist, one per target, and the drawer shows them.
    let detail = fixture.detail(&token, purge["id"].as_str().unwrap()).await;
    assert_eq!(detail.status, StatusCode::OK);
    assert_eq!(
        detail.body["items"].as_array().map(Vec::len),
        Some(2),
        "the drawer must show one row per target: {}",
        detail.body
    );

    // And the history is the same row, read back through the list endpoint.
    let history = fixture.history(&token, site).await;
    assert_eq!(history["total"], 1);
    assert_eq!(history["purges"][0]["id"], purge["id"]);

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_purge_by_tag_and_a_whole_zone_purge_are_both_accepted() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let token = fixture.token_a.clone();

    let tags = fixture
        .purge(
            &token,
            site,
            json!({ "kind": "tag", "targets": ["/blog", "/docs"] }),
        )
        .await;
    assert_eq!(tags.status, StatusCode::CREATED, "tags: {}", tags.body);
    assert_eq!(tags.body["kind"], "tag");

    let all = fixture
        .purge(
            &token,
            site,
            json!({ "kind": "all", "zone_confirmed": true }),
        )
        .await;
    assert_eq!(all.status, StatusCode::CREATED, "all: {}", all.body);
    assert_eq!(all.body["kind"], "all");
    assert_eq!(all.body["item_count"], 1, "a zone purge is one item");

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_whole_zone_purge_without_the_typed_word_is_refused() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let token = fixture.token_a.clone();

    let response = fixture.purge(&token, site, json!({ "kind": "all" })).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.body["error"]["code"], "purge_all_unconfirmed");
    assert_eq!(
        response.body["error"]["details"]["field"], "targets",
        "the refusal must name its field"
    );

    // Nothing was written: an unconfirmed zone purge must not leave a queued row behind
    // for the operator to discover an hour later.
    assert_eq!(fixture.history(&token, site).await["total"], 0);

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_malformed_target_and_an_empty_list_are_refused_with_their_own_messages() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let token = fixture.token_a.clone();

    let relative = fixture
        .purge(
            &token,
            site,
            json!({ "kind": "url", "targets": ["blog/post"] }),
        )
        .await;
    assert_eq!(relative.status, StatusCode::BAD_REQUEST);
    assert_eq!(relative.body["error"]["code"], "invalid_purge_url");
    assert_eq!(relative.body["error"]["details"]["field"], "targets");

    let bad_tag = fixture
        .purge(
            &token,
            site,
            json!({ "kind": "tag", "targets": ["has space"] }),
        )
        .await;
    assert_eq!(bad_tag.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad_tag.body["error"]["code"], "invalid_purge_tag");

    let empty = fixture
        .purge(
            &token,
            site,
            json!({ "kind": "url", "targets": ["  ", ""] }),
        )
        .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);
    assert_eq!(empty.body["error"]["code"], "empty_purge_targets");

    let unknown_kind = fixture
        .purge(
            &token,
            site,
            json!({ "kind": "everything", "targets": ["/a"] }),
        )
        .await;
    assert_eq!(unknown_kind.status, StatusCode::BAD_REQUEST);
    assert_eq!(unknown_kind.body["error"]["code"], "invalid_purge_kind");
    assert_eq!(
        unknown_kind.body["error"]["details"]["field"], "kind",
        "the kind field is named for an unknown kind"
    );

    assert_eq!(fixture.history(&token, site).await["total"], 0);

    fixture.cleanup().await;
}

#[tokio::test]
async fn more_than_the_cap_is_refused_and_the_message_carries_the_count() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let token = fixture.token_a.clone();

    let targets: Vec<String> = (0..=500).map(|n| format!("/page-{n}")).collect();
    let response = fixture
        .purge(&token, site, json!({ "kind": "url", "targets": targets }))
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.body["error"]["code"], "too_many_purge_targets");
    assert!(
        response.body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("501")),
        "the message must carry the count: {}",
        response.body["error"]["message"]
    );

    // 500 exactly is accepted — the cap is inclusive, and a test that only tried the
    // refusal would never notice an off-by-one that locked out a legitimate maximum.
    let at_cap: Vec<String> = (0..500).map(|n| format!("/page-{n}")).collect();
    let ok = fixture
        .purge(&token, site, json!({ "kind": "url", "targets": at_cap }))
        .await;
    assert_eq!(ok.status, StatusCode::CREATED, "500 targets: {}", ok.body);
    assert_eq!(ok.body["item_count"], 500);

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_duplicate_target_is_purged_once() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let token = fixture.token_a.clone();

    let response = fixture
        .purge(
            &token,
            site,
            json!({ "kind": "url", "targets": ["/a", "/a", "/b", "  /a  "] }),
        )
        .await;
    assert_eq!(response.status, StatusCode::CREATED);
    assert_eq!(
        response.body["item_count"], 2,
        "a repeated target is one item, not four: {}",
        response.body
    );

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// Tenancy and permissions
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn another_organizations_purge_is_neither_readable_nor_writable() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let response = fixture
        .purge(
            &fixture.token_a,
            fixture.site_a,
            json!({ "kind": "url", "targets": ["/a"] }),
        )
        .await;
    assert_eq!(response.status, StatusCode::CREATED);
    let id = response.body["id"].as_str().expect("an id").to_string();
    let site = fixture.site_a;

    // B cannot read it: `403`, not `404`. The row exists, and answering "not found" would
    // be a lie that also leaks less — the honest answer is that the purge belongs to a
    // site in another organization, which is what the message says.
    let read = fixture.detail(&fixture.token_b, &id).await;
    assert_eq!(read.status, StatusCode::FORBIDDEN, "{}", read.body);
    assert_eq!(read.body["error"]["code"], "permission_denied");

    // ...cannot retry it either, by the same rule and for the same reason.
    let retry = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/cdn/purges/{id}/retry"),
            Some(&fixture.token_b),
            None,
        ),
    )
    .await;
    assert_eq!(retry.status, StatusCode::FORBIDDEN, "{}", retry.body);

    // B cannot *create* against A's site either — the console submit is scoped the same way.
    let forge = fixture
        .purge(
            &fixture.token_b,
            site,
            json!({ "kind": "url", "targets": ["/b"] }),
        )
        .await;
    assert_eq!(forge.status, StatusCode::FORBIDDEN, "{}", forge.body);

    // ...and B's own history is empty, not A's.
    assert_eq!(
        fixture.history(&fixture.token_b, fixture.site_b).await["total"],
        0
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_caller_without_cdn_purge_may_read_history_but_not_invalidate() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (state, db) = (fixture.state.clone(), fixture.db.clone());
    let org = create_organization_row(&db, "readonly").await;
    let site = create_site(&db, org, "readonly").await;
    let (_id, token) = create_admin(&db, org, "readonly", &state, &["cdn.read"]).await;

    let response = call(
        &state,
        request(
            Method::POST,
            "/api/v1/cdn/purges",
            Some(&token),
            Some(json!({
                "site_id": site.to_string(),
                "kind": "url",
                "targets": ["/a"],
            })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "reading the history is not permission to flush the cache: {}",
        response.body
    );
    assert_eq!(response.body["error"]["code"], "permission_denied");

    // The read half still works — that is the split the request asks for.
    let list = call(
        &state,
        request(
            Method::GET,
            &format!("/api/v1/cdn/purges?site_id={site}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(list.status, StatusCode::OK);

    sqlx::query("delete from organizations where id = $1")
        .bind(org)
        .execute(db.pool())
        .await
        .ok();
    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// The drain
// ---------------------------------------------------------------------------------------------

/// Run the worker's own drain pass against a provider that is scripted to fail.
///
/// This is the assertion the slice is for: the provider refused, and the operator can see
/// that it refused and why. A purge that vanished quietly is the failure mode the request
/// calls out by name.
#[tokio::test]
async fn a_provider_refusal_lands_in_the_drawer_with_its_message_and_is_retryable() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let token = fixture.token_a.clone();

    let created = fixture
        .purge(
            &token,
            site,
            json!({ "kind": "url", "targets": ["/blog/a", "/blog/b"] }),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let id: Uuid = created.body["id"]
        .as_str()
        .expect("an id")
        .parse()
        .expect("a uuid");

    // The fixture's adapter is `origin`, which succeeds — so point the purge at an adapter
    // that fails by giving the site a settings row naming an unreachable endpoint.
    sqlx::query(
        "insert into cdn_settings (site_id, provider, endpoint_url, max_attempts) \
         values ($1, 'generic_http', 'http://127.0.0.1:1/never', 2) \
         on conflict do nothing",
    )
    .bind(site)
    .execute(fixture.db.pool())
    .await
    .expect("the settings row must insert");

    // The row really landed: this walk is only meaningful if the drain used `generic_http`,
    // and a silently-skipped insert would have it run against `origin` and succeed — which
    // would then read as "the purge pipeline ignores the configured adapter".
    let configured: String =
        sqlx::query_scalar("select provider from cdn_settings where site_id = $1")
            .bind(site)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the settings row must be readable");
    assert_eq!(configured, "generic_http");

    // Drain with the real worker logic: claim the due items, ask the adapter, record.
    //
    // `(failed, attempted)`, and the first one must be **zero** — the settings row gives an
    // attempt budget of two, so the first refusal is a retry and not a failure. A walk that
    // asserted `failed == 2` here would be describing a budget of one, and would have
    // shipped a worker that gives up on the first refusal.
    let (failed, attempted) = drain_once(&fixture.state, Some(site)).await;
    assert_eq!(attempted, 2, "both targets were claimed and sent");
    assert_eq!(
        failed, 0,
        "one refusal of a two-attempt budget is a retry, not a failure"
    );

    let detail = fixture.detail(&token, &id.to_string()).await;
    assert_eq!(detail.status, StatusCode::OK);
    let parent = &detail.body["purge"];
    // Two attempts configured, one drain pass: the items are pending again, not failed.
    assert_eq!(parent["status"], "running", "still retrying: {}", parent);
    assert!(
        detail.body["items"]
            .as_array()
            .is_some_and(|items| items.iter().all(|item| item["attempts"] == 1)),
        "each item records exactly one attempt: {}",
        detail.body
    );
    assert!(
        detail.body["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| !item["error"].is_null())),
        "the provider's refusal must be recorded on the item, not swallowed"
    );

    // Two more drains, with the backoff between them. The order matters and getting it wrong
    // is silent: an item that is waiting out a backoff is invisible to `claim_due`, so a
    // drain issued too early claims nothing and the walk then blames the item budget for a
    // scheduling accident. Sleeping *before* the second drain is the only ordering that
    // lets it see the rows.
    wait_for_backoff().await;
    let (second_failed, _) = drain_once(&fixture.state, Some(site)).await;
    assert_eq!(
        second_failed, 2,
        "the second refusal exhausts a two-attempt budget and both items fail"
    );
    let (third_failed, third_attempted) = drain_once(&fixture.state, Some(site)).await;
    assert_eq!(
        (third_failed, third_attempted),
        (0, 0),
        "a failed item is never claimed again without a retry — a worker that keeps picking \
up a `failed` row would retry for ever, which is what the attempt budget exists to prevent"
    );

    let final_state = fixture.detail(&token, &id.to_string()).await;
    assert_eq!(
        final_state.body["purge"]["status"], "failed",
        "a purge whose every attempt was refused is failed, and says why: {}",
        final_state.body
    );
    assert_eq!(final_state.body["purge"]["failed_count"], 2);
    assert!(
        !final_state.body["purge"]["error"]
            .as_str()
            .unwrap_or("")
            .is_empty(),
        "the provider's message must be on the parent row too"
    );
    assert_eq!(
        final_state.body["purge"]["retryable"], true,
        "a failed purge offers a retry"
    );

    // The retry requeues the failed items and nothing else — there is nothing else here,
    // which is exactly why the count is what makes the "only the failed" claim testable.
    let retry = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/cdn/purges/{id}/retry"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(retry.status, StatusCode::OK, "retry: {}", retry.body);
    assert_eq!(retry.body["purge"]["status"], "queued");
    assert_eq!(
        retry.body["items"].as_array().map(|items| items
            .iter()
            .filter(|item| item["status"] == "pending")
            .count()),
        Some(2),
        "every failed item is pending again: {}",
        retry.body
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_retry_on_a_purge_with_nothing_to_retry_is_refused_with_an_explanation() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let token = fixture.token_a.clone();

    let created = fixture
        .purge(&token, site, json!({ "kind": "url", "targets": ["/a"] }))
        .await;
    let id = created.body["id"].as_str().expect("an id").to_string();

    // A `queued` purge has nothing to retry, and a button that does nothing is a dead
    // button — the request forbids those.
    let retry = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/cdn/purges/{id}/retry"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(retry.status, StatusCode::CONFLICT, "{}", retry.body);
    assert_eq!(retry.body["error"]["code"], "purge_not_retryable");
    assert!(
        retry.body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("queued")),
        "the message names the state it is in: {}",
        retry.body
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_successful_drain_leaves_the_purge_succeeded_with_every_item_done() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let token = fixture.token_a.clone();

    let created = fixture
        .purge(
            &token,
            site,
            json!({ "kind": "url", "targets": ["/blog/a", "/blog/b", "/blog/c"] }),
        )
        .await;
    let id: Uuid = created.body["id"]
        .as_str()
        .expect("an id")
        .parse()
        .expect("a uuid");

    // No settings row, so the adapter is `origin`: the correct answer for an installation
    // with no edge in front of it, and a successful no-op rather than a fake success.
    let (failed, attempted) = drain_once(&fixture.state, Some(site)).await;
    assert_eq!(attempted, 3, "all three targets were sent");
    assert_eq!(
        failed, 0,
        "`origin` accepts every target, so nothing failed"
    );

    let detail = fixture.detail(&token, &id.to_string()).await;
    assert_eq!(
        detail.body["purge"]["status"], "succeeded",
        "every target went through: {}",
        detail.body
    );
    assert_eq!(detail.body["purge"]["failed_count"], 0);
    assert_eq!(detail.body["purge"]["retryable"], false);
    assert!(
        detail.body["items"]
            .as_array()
            .is_some_and(|items| items.iter().all(|item| item["status"] == "done")),
        "every item is done: {}",
        detail.body
    );
    // Not `.is_string()`: the API serialises an `OffsetDateTime` as a time tuple, so an
    // assertion about the string form would be testing the wire format rather than whether
    // a finished purge is stamped at all.
    assert!(
        !detail.body["purge"]["finished_at"].is_null(),
        "a finished purge is stamped: {}",
        detail.body
    );
    assert!(
        !detail.body["purge"]["started_at"].is_null(),
        "a drained purge records when it started"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_partial_drain_says_partial_and_counts_only_what_failed() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let token = fixture.token_a.clone();

    let created = fixture
        .purge(
            &token,
            site,
            json!({ "kind": "url", "targets": ["/a", "/b"] }),
        )
        .await;
    let id: Uuid = created.body["id"]
        .as_str()
        .expect("an id")
        .parse()
        .expect("a uuid");

    // Script the outcome directly rather than through a live provider: what is under test
    // is the fold from "one of two failed" to `partial` with `failed_count = 1`, and a real
    // network call would not reliably produce that shape.
    let items = omnion_cdn::purge::items_of(fixture.db.pool(), id)
        .await
        .expect("items must read");
    let mut items = items;
    let (failed, _) = omnion_cdn::purge::apply_outcome(
        &mut items,
        &omnion_cdn::PurgeOutcome::Partial {
            failed: vec!["/b".to_string()],
            message: "one target is not in the zone".to_string(),
        },
        time::OffsetDateTime::now_utc(),
        1,
        0,
    );
    assert_eq!(failed, 1);
    omnion_cdn::purge::save_items(fixture.db.pool(), &items)
        .await
        .expect("items must save");
    omnion_cdn::purge::settle(fixture.db.pool(), &[id])
        .await
        .expect("the parent must settle");

    let detail = fixture.detail(&token, &id.to_string()).await;
    assert_eq!(detail.body["purge"]["status"], "partial", "{}", detail.body);
    assert_eq!(detail.body["purge"]["failed_count"], 1);
    assert_eq!(detail.body["purge"]["retryable"], true);
    assert_eq!(
        detail.body["purge"]["error"], "one target is not in the zone",
        "the provider's own words reach the parent row"
    );
    assert_eq!(detail.body["items"][0]["status"], "done");
    assert_eq!(detail.body["items"][1]["status"], "failed");

    // A retry moves only the failed item; the done one is untouched. This is the assertion
    // that makes "retry failed" different from "run it again".
    let retry = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/cdn/purges/{id}/retry"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(retry.status, StatusCode::OK);
    let statuses: Vec<&str> = retry.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["status"].as_str().expect("a status"))
        .collect();
    assert_eq!(
        statuses,
        vec!["done", "pending"],
        "the target that already succeeded is not re-sent: {statuses:?}"
    );

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// The overview and the settings the slice 1 screens already called
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_overview_reports_the_queue_the_counters_and_the_last_twenty() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let token = fixture.token_a.clone();

    for target in ["/a", "/b"] {
        let response = fixture
            .purge(&token, site, json!({ "kind": "url", "targets": [target] }))
            .await;
        assert_eq!(response.status, StatusCode::CREATED);
    }

    let status = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/cdn/status?site_id={site}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(status.status, StatusCode::OK, "{}", status.body);
    let body = &status.body;
    assert_eq!(body["provider"], "origin");
    assert_eq!(
        body["provider_shipped"], true,
        "origin is a shipped adapter"
    );
    assert_eq!(
        body["queue_depth"], 2,
        "two queued purges are two waiting items: {body}"
    );
    assert_eq!(body["open_purges"], 2);
    assert_eq!(body["purges_24h"], 2);
    assert_eq!(
        body["recent"].as_array().map(Vec::len),
        Some(2),
        "the recent list is the history, not a second source: {body}"
    );
    // Nothing has been drained, so nothing has failed. A card showing 0.0 is right here and
    // a card showing `NaN` (a zero-total division) would not be.
    assert_eq!(body["failure_rate"], 0.0);

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_settings_screen_gets_a_real_row_and_saves_one_it_was_given() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let token = fixture.token_a.clone();

    // A site that has never been configured still answers with a usable form rather than
    // a 404 — otherwise the provider screen's first visit is an error banner.
    let fresh = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/cdn/settings?site_id={site}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(fresh.status, StatusCode::OK, "{}", fresh.body);
    assert_eq!(fresh.body["provider"], "origin");
    assert_eq!(fresh.body["has_credential"], false);
    assert_eq!(fresh.body["batch_size"], 100);

    let saved = call(
        &fixture.state,
        request(
            Method::PUT,
            "/api/v1/cdn/settings",
            Some(&token),
            Some(json!({
                "site_id": site.to_string(),
                "provider": "generic_http",
                "endpoint_url": "https://edge.example/purge",
                "zone_ref": "example",
                "batch_size": 250,
                "max_attempts": 3,
                "auto_purge": { "page.published": true, "media.replaced": false },
            })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "save: {}", saved.body);
    assert_eq!(saved.body["provider"], "generic_http");
    assert_eq!(saved.body["batch_size"], 250);
    assert_eq!(saved.body["max_attempts"], 3);
    assert_eq!(saved.body["auto_purge"]["media.replaced"], false);

    // Reading it back must show the saved values, and must never show a credential value.
    let read_back = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/cdn/settings?site_id={site}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(read_back.body["endpoint_url"], "https://edge.example/purge");
    assert!(
        read_back.body.get("credential").is_none(),
        "the settings response must not carry a credential field at all: {}",
        read_back.body
    );
    assert_eq!(read_back.body["has_credential"], false);

    // A second save updates rather than duplicating: the per-site index is a unique one.
    let again = call(
        &fixture.state,
        request(
            Method::PUT,
            "/api/v1/cdn/settings",
            Some(&token),
            Some(json!({
                "site_id": site.to_string(),
                "provider": "cloudflare_style",
                "zone_ref": "example-zone",
                "batch_size": 50,
                "max_attempts": 2,
            })),
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK);
    let count: i64 = sqlx::query_scalar("select count(*) from cdn_settings where site_id = $1")
        .bind(site)
        .fetch_one(fixture.db.pool())
        .await
        .expect("count must read");
    assert_eq!(count, 1, "a second save must not create a second row");

    fixture.cleanup().await;
}

#[tokio::test]
async fn settings_outside_the_1_to_1000_batch_cap_are_refused_with_their_field() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let token = fixture.token_a.clone();

    for (batch_size, max_attempts, field) in [
        (0, 5, "batch_size"),
        (1001, 5, "batch_size"),
        (10, 0, "max_attempts"),
        (10, 11, "max_attempts"),
    ] {
        let response = call(
            &fixture.state,
            request(
                Method::PUT,
                "/api/v1/cdn/settings",
                Some(&token),
                Some(json!({
                    "site_id": site.to_string(),
                    "provider": "origin",
                    "batch_size": batch_size,
                    "max_attempts": max_attempts,
                })),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "batch {batch_size} / attempts {max_attempts}: {}",
            response.body
        );
        assert_eq!(response.body["error"]["details"]["field"], field);
    }

    // An adapter that does not ship is refused by name, so a hand-edited row cannot point
    // the worker at an adapter that cannot run.
    let unknown = call(
        &fixture.state,
        request(
            Method::PUT,
            "/api/v1/cdn/settings",
            Some(&token),
            Some(json!({
                "site_id": site.to_string(),
                "provider": "fastly",
                "batch_size": 100,
                "max_attempts": 5,
            })),
        ),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    assert_eq!(unknown.body["error"]["code"], "unknown_purge_provider");

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_adapter_catalogue_lists_only_adapters_this_build_ships() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/cdn/adapters",
            Some(&fixture.token_a),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let adapters = response.body["adapters"].as_array().expect("an array");
    assert_eq!(adapters.len(), 3, "origin, generic_http, cloudflare_style");

    let keys: Vec<&str> = adapters
        .iter()
        .map(|adapter| adapter["key"].as_str().expect("a key"))
        .collect();
    assert_eq!(keys, vec!["origin", "generic_http", "cloudflare_style"]);

    for adapter in adapters {
        assert_eq!(
            adapter["shipped"], true,
            "the catalogue lists no unimplemented adapter: {adapter}"
        );
        // The form's "needs a credential" flag is what makes it render the write-only
        // field, so an adapter that needs one and says it does not is a missing input.
        if adapter["key"] == "origin" {
            assert_eq!(adapter["needs_credential"], false);
            assert_eq!(adapter["needs_endpoint"], false);
        } else {
            assert_eq!(adapter["needs_credential"], true);
            assert_eq!(adapter["needs_endpoint"], true);
        }
    }

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_settings_row_cannot_name_an_adapter_this_build_does_not_ship() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let token = fixture.token_a.clone();

    // Two layers, and this walk proves both because either alone is a hole.
    //
    // The API refuses a settings save that names an adapter it does not ship, and the table
    // has a `provider in (...)` check underneath it. The second is not redundant defence for
    // a hand edit: it is the only thing standing between a bad key and a worker that builds
    // an adapter for it. A test that only exercised the API would pass with the constraint
    // dropped, and a test that only exercised the constraint would pass with the API guard
    // dropped.
    let saved = call(
        &fixture.state,
        request(
            Method::PUT,
            "/api/v1/cdn/settings",
            Some(&token),
            Some(json!({
                "site_id": site.to_string(),
                "provider": "fastly",
                "batch_size": 100,
                "max_attempts": 5,
            })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::BAD_REQUEST, "{}", saved.body);
    assert_eq!(saved.body["error"]["code"], "unknown_purge_provider");

    // The row does not exist afterwards, which is the half that is easy to skip: a refusal
    // that still wrote the row would leave the panel showing a provider that cannot run.
    let rows: i64 = sqlx::query_scalar("select count(*) from cdn_settings where site_id = $1")
        .bind(site)
        .fetch_one(fixture.db.pool())
        .await
        .expect("count must read");
    assert_eq!(rows, 0, "a refused save must not leave a row behind");

    // And the database refuses it directly. `ok()` is not used to discard the error: the
    // assertion is that this statement *is* an error, and a swallowed failure would make
    // the walk pass on a database with no constraint at all — which is exactly the state it
    // is here to rule out.
    let direct = sqlx::query("insert into cdn_settings (site_id, provider) values ($1, 'fastly')")
        .bind(site)
        .execute(fixture.db.pool())
        .await;
    let error = direct.expect_err("the provider check must refuse an adapter that is not shipped");
    assert!(
        error.to_string().contains("cdn_settings_provider_known"),
        "the refusal must name the constraint, so a different failure is not mistaken for it: {error}"
    );

    // A purge therefore cannot be queued against an adapter that cannot run: the only way
    // to name one is a row that will not insert.
    let purge = fixture
        .purge(&token, site, json!({ "kind": "url", "targets": ["/a"] }))
        .await;
    assert_eq!(
        purge.status,
        StatusCode::CREATED,
        "with no settings row the adapter is origin, which can run: {}",
        purge.body
    );
    assert_eq!(purge.body["provider"], "origin");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// The write-only provider credential (REQ-011, slice 4)
//
// These walks exist because the screen shipped before the thing behind it. `/cdn/settings`
// has rendered a `Replace credential` field since slice 1: it labels the field write-only,
// starts empty on every visit, and the handler **discarded the value it was sent**. An
// operator pasted a key, was told "Settings saved, and the credential was replaced", and
// the column stayed `null` — so `has_credential` was pinned to `false` for ever and every
// adapter that needs a key could never authenticate. A dead control, with a success message.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_credential_pasted_in_the_panel_is_stored_and_the_worker_sends_it() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let token = fixture.token_a.clone();

    // A loopback endpoint that reports the `Authorization` header it received, so the
    // assertion is about the *request the adapter made* and not about an outcome derived
    // from it. A test that only checked "the purge succeeded" would pass with the
    // credential never sent at all.
    let (endpoint, mut received) = auth_recorder();

    let saved = call(
        &fixture.state,
        request(
            Method::PUT,
            "/api/v1/cdn/settings",
            Some(&token),
            Some(json!({
                "site_id": site.to_string(),
                "provider": "generic_http",
                "endpoint_url": endpoint,
                "zone_ref": "qa",
                "batch_size": 100,
                "max_attempts": 3,
                "credential": CREDENTIAL,
            })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "save: {}", saved.body);

    // The response says a credential exists and does not carry one. Both halves: the flag is
    // what the form's own hint reads, and a body carrying the value would be a leak on a
    // response the panel fetches on every visit to the screen.
    assert_eq!(
        saved.body["has_credential"], true,
        "the save must report the stored credential it just wrote: {}",
        saved.body
    );
    assert!(
        saved.body.get("credential").is_none(),
        "the settings response must not carry a credential field at all: {}",
        saved.body
    );

    // And the column holds an envelope, not the key. Read straight from the database, which
    // is the only place the assertion is worth anything — the API already promised not to
    // return it, and a promise is not a storage format.
    let stored: Vec<u8> =
        sqlx::query_scalar("select credential_ciphertext from cdn_settings where site_id = $1")
            .bind(site)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the credential column must be readable");
    let stored = String::from_utf8(stored).expect("the envelope is text");
    assert!(
        !stored.contains(CREDENTIAL),
        "the column must hold a sealed envelope, not the key: {stored}"
    );
    assert!(
        stored.contains("omnion-cdn-credential.v1:"),
        "the column must name its format, so a value written by another build reads as a \
         corrupt row rather than as a credential: {stored}"
    );

    // The read the worker makes, resolved by the same function the drain calls. This is the
    // assertion that the slice exists for: the screen's save and the worker's load are one
    // contract, and a test of either half alone would pass with the other half missing.
    let (_key, settings, _attempts) = purge::provider_for_site(fixture.db.pool(), Some(site))
        .await
        .expect("the provider settings must resolve");
    assert_eq!(
        settings.credential.as_deref(),
        Some(CREDENTIAL),
        "the worker must receive the credential the panel stored — this is the half that was \
         wired to None and made the whole control dead"
    );

    // And it arrives at the provider, in the header, on a real purge.
    let purge = fixture
        .purge(
            &token,
            site,
            json!({ "kind": "url", "targets": ["/authed"] }),
        )
        .await;
    assert_eq!(purge.status, StatusCode::CREATED, "{}", purge.body);
    let (failed, attempted) = drain_once(&fixture.state, Some(site)).await;
    assert_eq!(
        failed, 0,
        "an authenticated purge must not fail: the worker got {attempted} item(s) and the \
         endpoint answered 200 to all of them"
    );

    let head = tokio::time::timeout(std::time::Duration::from_secs(10), received.recv())
        .await
        .expect("the adapter must have called the endpoint: the drain reported no failure, so \
                 the request either never left the process or never reached the loopback port")
        .expect("the recorder's channel must stay open");
    // Matched case-insensitively, and on a prefix plus suffix rather than the whole value.
    // Two separate reasons, and the first is the one that matters: **HTTP header names are
    // case-insensitive and HTTP/2 lowercases them**, so a walk that asserts on the
    // canonical spelling passes over a plain socket and fails over a real HTTP/2 client.
    // The second is the HTTP client's own redaction: a secret in a header is masked to
    // `<first four>...<last four>` before it reaches the wire, so demanding the full value
    // tests that client's memory hygiene rather than this slice. The credential is still
    // proven end to end by the two assertions above, which read the decrypted value back
    // out of `provider_for_site`.
    let lower = head.to_lowercase();
    assert!(
        lower.contains(&format!("authorization: bearer {}", CREDENTIAL))
            && lower.contains(&CREDENTIAL[16..]),
        "the provider must receive the stored credential, and the assertion is on the wire \
         rather than on the adapter's own field: {head}"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_save_that_omits_the_credential_keeps_the_stored_one() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let token = fixture.token_a.clone();

    let seeded = call(
        &fixture.state,
        request(
            Method::PUT,
            "/api/v1/cdn/settings",
            Some(&token),
            Some(json!({
                "site_id": site.to_string(),
                "provider": "generic_http",
                "endpoint_url": "https://edge.example/purge",
                "batch_size": 100,
                "max_attempts": 5,
                "credential": "the-original-key",
            })),
        ),
    )
    .await;
    assert_eq!(seeded.status, StatusCode::OK, "{}", seeded.body);

    // The batch size is what an operator actually comes back to change. Sending no
    // credential field — which is what the panel does whenever the input is empty, and the
    // common case — must not destroy the key they pasted ten minutes earlier.
    let updated = call(
        &fixture.state,
        request(
            Method::PUT,
            "/api/v1/cdn/settings",
            Some(&token),
            Some(json!({
                "site_id": site.to_string(),
                "provider": "generic_http",
                "endpoint_url": "https://edge.example/purge",
                "batch_size": 250,
                "max_attempts": 5,
            })),
        ),
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK, "{}", updated.body);
    assert_eq!(updated.body["batch_size"], 250);
    assert_eq!(
        updated.body["has_credential"], true,
        "the flag must survive the edit"
    );

    let (_key, settings, _attempts) = purge::provider_for_site(fixture.db.pool(), Some(site))
        .await
        .expect("the provider settings must resolve");
    assert_eq!(
        settings.credential.as_deref(),
        Some("the-original-key"),
        "an omitted field means 'keep', not 'clear' — otherwise saving the batch size \
         silently destroys the operator's credential"
    );

    // Replacing it is a deliberate act and takes effect: the panel says "the credential was
    // replaced", so the second save must be a *different* key rather than a no-op.
    let replaced = call(
        &fixture.state,
        request(
            Method::PUT,
            "/api/v1/cdn/settings",
            Some(&token),
            Some(json!({
                "site_id": site.to_string(),
                "provider": "generic_http",
                "endpoint_url": "https://edge.example/purge",
                "batch_size": 250,
                "max_attempts": 5,
                "credential": "the-rotated-key",
            })),
        ),
    )
    .await;
    assert_eq!(replaced.status, StatusCode::OK, "{}", replaced.body);
    let (_key, settings, _attempts) = purge::provider_for_site(fixture.db.pool(), Some(site))
        .await
        .expect("the provider settings must resolve");
    assert_eq!(
        settings.credential.as_deref(),
        Some("the-rotated-key"),
        "a replacement has to be a replacement, or the operator cannot rotate a key"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_blank_credential_is_refused_naming_the_field_and_stores_nothing() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let token = fixture.token_a.clone();

    // An empty string is a *refusal*, not a clear. "The field is empty" and "remove my
    // stored key" are different requests, and a form that sends the first and means the
    // second is how a working edge provider gets an unauthenticated purge queue.
    for (credential, why) in [("", "an empty string"), ("   ", "whitespace only")] {
        let response = call(
            &fixture.state,
            request(
                Method::PUT,
                "/api/v1/cdn/settings",
                Some(&token),
                Some(json!({
                    "site_id": site.to_string(),
                    "provider": "generic_http",
                    "endpoint_url": "https://edge.example/purge",
                    "batch_size": 100,
                    "max_attempts": 5,
                    "credential": credential,
                })),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "{why} must be refused: {}",
            response.body
        );
        assert_eq!(
            response.body["error"]["code"], "invalid_credential",
            "{}",
            response.body
        );
        assert_eq!(
            response.body["error"]["details"]["field"], "credential",
            "the refusal has to name the field the form can underline: {}",
            response.body
        );
    }

    // Nothing was written by any of them: the row the first refusal would have created is
    // the row the panel would then show as "a credential is stored".
    let rows: i64 = sqlx::query_scalar(
        "select count(*) from cdn_settings where site_id = $1 and credential_ciphertext is not null",
    )
    .bind(site)
    .fetch_one(fixture.db.pool())
    .await
    .expect("count must read");
    assert_eq!(
        rows, 0,
        "a refused credential must not leave a sealed value behind"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_credential_is_never_written_to_the_audit_trail() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let token = fixture.token_a.clone();

    let saved = call(
        &fixture.state,
        request(
            Method::PUT,
            "/api/v1/cdn/settings",
            Some(&token),
            Some(json!({
                "site_id": site.to_string(),
                "provider": "generic_http",
                "endpoint_url": "https://edge.example/purge",
                "batch_size": 100,
                "max_attempts": 5,
                "credential": "cdn-qa-key-do-not-log-me",
            })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);

    // An incident review needs to know *that* a credential was replaced and by whom. It must
    // never be able to learn *what*. The audit table is the one store every support
    // conversation can read, so it is the worst possible place for a provider key.
    let metadata: String = sqlx::query_scalar(
        "select metadata::text from audit_log \
         where action = 'cdn.settings.updated' \
         order by created_at desc limit 1",
    )
    .fetch_one(fixture.db.pool())
    .await
    .expect("the settings write must be audited");
    assert!(
        !metadata.contains("cdn-qa-key"),
        "the audit entry must record that a credential was replaced, never the value: {metadata}"
    );
    // The flag is asserted through the VALUE, not through the rendered text: `metadata::text`
    // is jsonb's own serialisation and it puts a space after the colon, so a substring test
    // on `"credential_replaced":true` fails on a correct row for a formatting reason nobody
    // controls. What matters is that the key is present and true.
    let parsed: serde_json::Value = serde_json::from_str(&metadata).expect("metadata is json");
    assert_eq!(
        parsed["credential_replaced"], true,
        "and it must still say that one was: {metadata}"
    );

    fixture.cleanup().await;
}

/// A loopback endpoint that answers `200 {}` and reports the request head it received.
///
/// Hand-rolled rather than a test framework for the same reason `crates/cdn`'s own adapter
/// tests are: the assertion is one header on one request, and a framework would be a
/// dependency to prove a line about a socket.
fn auth_recorder() -> (String, tokio::sync::mpsc::Receiver<String>) {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").expect("the test server must bind");
    let port = listener.local_addr().expect("the bound address").port();
    // Bounded, not unbounded: the recorder is a fixture, and a fixture that can hold an
    // unlimited number of queued request heads turns a drained port into a memory leak in a
    // suite that runs 21 walks. Four is the cap `crates/cdn`'s own adapter tests use for the
    // same reason.
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    // Every blocking call in here lives on its own `std::thread`, and that is not tidiness
    // — it is the difference between this walk finishing and the whole suite hanging.
    // `#[tokio::test]` is a CURRENT-thread runtime, so one blocking read anywhere on the
    // test's own task parks every other task with it: the walk sat in a channel
    // `recv_timeout` for 40 minutes with the runtime idle, and the failure presented as
    // "this box is slow" rather than as "a test helper blocked an executor". The *walk*
    // awaits with `tokio::time::timeout`; the server stays synchronous and off-thread.
    std::thread::spawn(move || {
        // Several connections: the settings save's own `Test connection` is not called here,
        // but the drain may retry, and a listener that answers once and exits turns a retry
        // into a refused purge that looks like a product bug.
        for _ in 0..4 {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(match stream.try_clone() {
                    Ok(stream) => stream,
                    Err(_) => return,
                });
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    if line.trim().is_empty() {
                        break;
                    }
                    head.push_str(&line);
                }
                let _ = tx.blocking_send(head);
                let mut stream = stream;
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\
                      Connection: close\r\n\r\n{}",
                );
                let _ = stream.flush();
            });
        }
    });
    (format!("http://127.0.0.1:{port}/purge"), rx)
}

// ---------------------------------------------------------------------------------------------
// The drain, shared by the walks above
// ---------------------------------------------------------------------------------------------

/// One pass of the worker's drain, using the crate's own claim, adapter and settle calls.
///
/// The worker itself is `apps/api/src/cdn_purge_runner.rs`; this calls the same functions
/// rather than a copy of them, so a walk and the binary cannot drift apart — a test that
/// reimplemented the fold would keep passing after the fold changed.
async fn drain_once(state: &AppState, site_id: Option<Uuid>) -> (i32, i32) {
    let pool = state.db().pool();
    let _ = POOL.set(pool.clone());

    // Group the claimed items by the provider that owns their site, because one call
    // answers a batch: claiming across two sites and asking one adapter would send site
    // B's URLs to site A's provider.
    let mut per_site: std::collections::BTreeMap<Uuid, Vec<omnion_cdn::purge::PurgeItemRow>> =
        Default::default();
    loop {
        let claimed = purge::claim_due(pool, 100).await.expect("claim must run");
        if claimed.is_empty() {
            break;
        }
        for item in claimed {
            let owner: Option<Uuid> =
                sqlx::query_scalar("select site_id from cdn_purges where id = $1")
                    .bind(item.purge_id)
                    .fetch_one(pool)
                    .await
                    .expect("the purge row must exist");
            per_site
                .entry(owner.unwrap_or_else(Uuid::nil))
                .or_default()
                .push(item);
        }
    }

    // Mark the parents running, exactly as `cdn_purge_runner::tick` does. Without this the
    // walk drains a queue the binary would have stamped, and `started_at` stays null — which
    // is a difference between the helper and the worker, not a difference between a purge
    // that started and one that did not.
    let claimed_purges: Vec<Uuid> = per_site
        .values()
        .flatten()
        .map(|item| item.purge_id)
        .collect();
    purge::mark_running(pool, &claimed_purges)
        .await
        .expect("parents must be marked running");

    let mut total_failed = 0;
    let mut total_items = 0;
    for (site, mut items) in per_site {
        if site_id.is_some_and(|wanted| wanted != site) {
            // Another site's work: leave it claimed-by-nobody is not an option, so put it
            // straight back to pending and move on.
            for item in &mut items {
                item.status = "pending".to_string();
                item.next_attempt_at = time::OffsetDateTime::now_utc();
            }
            purge::save_items(pool, &items).await.ok();
            continue;
        }
        total_items += items.len() as i32;

        let (key, settings, max_attempts) = purge::provider_for_site(pool, Some(site))
            .await
            .expect("the provider must resolve");
        let kind = PurgeKind::Url;
        let targets: Vec<String> = items.iter().map(|item| item.target.clone()).collect();
        let request = kind.to_purge(&targets);

        // The adapter call is blocking by design; in the binary it runs inside
        // `spawn_blocking`, and here `tokio::task::block_in_place` is the same boundary
        // without paying for a thread per drain.
        let outcome = tokio::task::spawn_blocking({
            let key = key.clone();
            move || omnion_cdn::provider_for(&key, &settings).purge(&request)
        })
        .await
        .expect("the adapter must not panic");

        let (failed, _) = purge::apply_outcome(
            &mut items,
            &outcome,
            time::OffsetDateTime::now_utc(),
            max_attempts,
            0,
        );
        purge::save_items(pool, &items).await.ok();
        let purge_ids: Vec<Uuid> = items.iter().map(|item| item.purge_id).collect();
        purge::settle(pool, &purge_ids).await.ok();
        total_failed += failed;
    }

    (total_failed, total_items)
}

/// Wait until every pending item in the queue is due again.
///
/// The backoff is a *schedule* stored in `next_attempt_at`, so the honest way to wait for
/// it is to read the schedule rather than to sleep a guessed number of seconds. A fixed
/// sleep is either too short (a flaky walk that blames the attempt budget for a scheduling
/// accident) or too slow (a suite that takes minutes to prove a two-attempt budget), and
/// both are the same mistake: assuming a duration instead of asking the queue.
///
/// The cap is a safety net against a row whose backoff is far in the future, which would
/// otherwise turn a walk into a hang. The jitter puts the first step under two seconds, so
/// the cap is generous rather than tight.
async fn wait_for_backoff() {
    for _ in 0..40 {
        let due: i64 = sqlx::query_scalar(
            "select count(*) from cdn_purge_items \
             where status = 'pending' and next_attempt_at > now()",
        )
        .fetch_one(current_pool())
        .await
        .expect("the queue must be readable");
        if due == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// The pool the shared helpers use, set by the first `drain_once`.
///
/// A `OnceLock` rather than a parameter threaded through five helpers: the wait loop needs
/// the pool, and every walk that waits has already drained once. The suites run with
/// `--test-threads=1`, which is what makes one shared cell honest — under parallel test
/// threads it would be a cross-test dependency, and the alternative (a parameter on every
/// helper) buys nothing here.
static POOL: std::sync::OnceLock<sqlx::PgPool> = std::sync::OnceLock::new();

/// The registered pool, or a clear panic naming the mistake.
fn current_pool() -> &'static sqlx::PgPool {
    POOL.get()
        .expect("the pool is registered by the first drain_once call")
}

#[allow(dead_code)]
fn unused_status(state: PurgeStatus) -> &'static str {
    state.as_str()
}
