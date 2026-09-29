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
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
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
) -> (Uuid, String) {
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

    let token = login(state, &email).await;
    (user.id, token)
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
        .expect("login must set the session cookie")
        .split(';')
        .next()
        .expect("the cookie has a value")
        .split_once('=')
        .expect("the cookie is name=value")
        .1
        .to_string()
}

struct Fixture {
    state: AppState,
    db: Db,
    site_a: Uuid,
    site_b: Uuid,
    token_a: String,
    token_b: String,
    admin_a: Uuid,
    organizations: Vec<Uuid>,
}

const ALL_PERMISSIONS: [&str; 3] = ["cdn.read", "cdn.manage", "cdn.purge"];

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool()).await.expect("the IAM seed must run");

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
    async fn purge(&self, token: &str, site: Uuid, body: Value) -> TestResponse {
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
    async fn history(&self, token: &str, site: Uuid) -> Value {
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
    async fn detail(&self, token: &str, id: &str) -> TestResponse {
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
    assert_eq!(purge["provider"], "origin", "the fixture has no settings row");
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
        .purge(&token, site, json!({ "kind": "tag", "targets": ["/blog", "/docs"] }))
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

    let response = fixture
        .purge(&token, site, json!({ "kind": "all" }))
        .await;
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
        .purge(&token, site, json!({ "kind": "url", "targets": ["blog/post"] }))
        .await;
    assert_eq!(relative.status, StatusCode::BAD_REQUEST);
    assert_eq!(relative.body["error"]["code"], "invalid_purge_url");
    assert_eq!(relative.body["error"]["details"]["field"], "targets");

    let bad_tag = fixture
        .purge(&token, site, json!({ "kind": "tag", "targets": ["has space"] }))
        .await;
    assert_eq!(bad_tag.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad_tag.body["error"]["code"], "invalid_purge_tag");

    let empty = fixture
        .purge(&token, site, json!({ "kind": "url", "targets": ["  ", ""] }))
        .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);
    assert_eq!(empty.body["error"]["code"], "empty_purge_targets");

    let unknown_kind = fixture
        .purge(&token, site, json!({ "kind": "everything", "targets": ["/a"] }))
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
        .purge(&fixture.token_a, fixture.site_a, json!({ "kind": "url", "targets": ["/a"] }))
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
    assert_eq!(fixture.history(&fixture.token_b, fixture.site_b).await["total"], 0);

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
    let id: Uuid = created.body["id"].as_str().expect("an id").parse().expect("a uuid");

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
        (third_failed, third_attempted), (0, 0),
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
        !final_state.body["purge"]["error"].as_str().unwrap_or("").is_empty(),
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
        retry.body["items"]
            .as_array()
            .map(|items| items
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
    let id: Uuid = created.body["id"].as_str().expect("an id").parse().expect("a uuid");

    // No settings row, so the adapter is `origin`: the correct answer for an installation
    // with no edge in front of it, and a successful no-op rather than a fake success.
    let (failed, attempted) = drain_once(&fixture.state, Some(site)).await;
    assert_eq!(attempted, 3, "all three targets were sent");
    assert_eq!(failed, 0, "`origin` accepts every target, so nothing failed");

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
    let id: Uuid = created.body["id"].as_str().expect("an id").parse().expect("a uuid");

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
    assert_eq!(body["provider_shipped"], true, "origin is a shipped adapter");
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

    for (batch_size, max_attempts, field) in [(0, 5, "batch_size"), (1001, 5, "batch_size"), (10, 0, "max_attempts"), (10, 11, "max_attempts")] {
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
    let rows: i64 = sqlx::query_scalar(
        "select count(*) from cdn_settings where site_id = $1",
    )
    .bind(site)
    .fetch_one(fixture.db.pool())
    .await
    .expect("count must read");
    assert_eq!(rows, 0, "a refused save must not leave a row behind");

    // And the database refuses it directly. `ok()` is not used to discard the error: the
    // assertion is that this statement *is* an error, and a swallowed failure would make
    // the walk pass on a database with no constraint at all — which is exactly the state it
    // is here to rule out.
    let direct = sqlx::query(
        "insert into cdn_settings (site_id, provider) values ($1, 'fastly')",
    )
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
            let owner: Option<Uuid> = sqlx::query_scalar("select site_id from cdn_purges where id = $1")
                .bind(item.purge_id)
                .fetch_one(pool)
                .await
                .expect("the purge row must exist");
            per_site.entry(owner.unwrap_or_else(Uuid::nil)).or_default().push(item);
        }
    }

    // Mark the parents running, exactly as `cdn_purge_runner::tick` does. Without this the
    // walk drains a queue the binary would have stamped, and `started_at` stays null — which
    // is a difference between the helper and the worker, not a difference between a purge
    // that started and one that did not.
    let claimed_purges: Vec<Uuid> = per_site.values().flatten().map(|item| item.purge_id).collect();
    purge::mark_running(pool, &claimed_purges).await.expect("parents must be marked running");

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

        let (key, settings, max_attempts) = purge::provider_for_site(pool, Some(site)).await
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
    POOL.get().expect("the pool is registered by the first drain_once call")
}

#[allow(dead_code)]
fn unused_status(state: PurgeStatus) -> &'static str {
    state.as_str()
}
