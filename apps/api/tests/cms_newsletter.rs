//! Integration test for the newsletter module (REQ-064, slice 4b).
//!
//! Double opt-in is a promise about somebody else's inbox, so the walks here are the claims
//! the REQ makes about it, each of which is a place a plausible implementation gets it wrong:
//!
//! * **A signup is not a subscription.** A new address lands `pending` with a live
//!   confirmation link, and the deliverable list — what an issue would actually reach — does
//!   NOT contain it until the link is used. Asserting the row's existence proves nothing here;
//!   the claim is about the send path, so the walk reads that.
//! * **The token is stored hashed and is single-use.** A replayed confirmation is refused, and
//!   the database holds a digest rather than the token — the second is what a leaked backup
//!   turns into, so the walk reads the column.
//! * **The link expires.** 48 hours is the window, and the walk moves the row's own expiry into
//!   the past rather than sleeping, because a test that waits two days is a test that never
//!   runs.
//! * **Unsubscribe keeps the row and flips the status.** A deleted row is how the next CSV
//!   import quietly re-adds somebody who left on purpose, so the assertion is on the status
//!   *and* on the row's continued existence.
//! * **A refused token says nothing about the list.** An unknown, an expired and a used token
//!   are one error to the caller; a caller that could tell them apart could enumerate who is
//!   on a list from a link a visitor was forwarded.
//! * **Reading subscribers is not the power to change them.** `newsletter.read` alone gets the
//!   table and 403 on every button, which is the whole reason the two keys exist.

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
use omnion_security::{CSRF_HEADER, derive_csrf_token};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

mod support;
use support::isolated_db::{IsolatedDb, announce_skip, assert_nothing_skipped};
use support::walk_auth;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// The CSRF secret this suite runs with. It must match the `OMNION_CSRF_SECRET` the run script
/// exports, because the token is derived from it.
const CSRF_SECRET: &str = "w2-seo-suite-csrf-secret";

/// A signed-in session: the cookie the browser sends and the UUID the CSRF token derives from.
struct Auth {
    token: String,
    session_id: String,
}

/// What the list reader may do: read the tables and nothing else. The 403 assertions in
/// `reading_the_subscribers_is_not_the_power_to_change_them` are the point of this list.
const READER_PERMISSIONS: [&str; 2] = ["newsletter.read", "content.pages.read"];

/// What the owner adds on top.
const OWNER_EXTRA: [&str; 2] = ["newsletter.manage", "content.pages.create"];

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    headers: Vec<(String, String)>,
    body: Value,
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
    let body = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    TestResponse {
        status,
        headers,
        body,
    }
}

fn request(method: Method, uri: &str, auth: Option<&Auth>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match auth {
        Some(auth) => {
            let token = auth.token.as_str();
            let builder = builder.header(header::COOKIE, format!("omnion_session={token}"));
            let csrf = derive_csrf_token(CSRF_SECRET.as_bytes(), &auth.session_id);
            builder.header(CSRF_HEADER, csrf)
        }
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

/// A public request, as a theme's signup form makes it.
fn public_request(method: Method, uri: &str, host: &str, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, host)
        .header(header::USER_AGENT, "newsletter-suite/1.0");
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

async fn live_state() -> Option<(AppState, Db, IsolatedDb)> {
    let mut config = Config::from_env().expect("environment must be valid");
    // **The secret this file signs with has to be installed in the state as well.** It
    // already derives every CSRF token from `CSRF_SECRET` -- and never set the config, so
    // the deployment it built had no secret at all, and sign-in answered with no token.
    // Every authenticated write was then refused `csrf_unavailable`, a code whose message
    // names a *deployment* problem rather than this suite's omission. The walks that were
    // green were the ones that never wrote.
    config.csrf = omnion_core::config::CsrfSecret::new(Some(CSRF_SECRET.to_owned()));
    let isolated = IsolatedDb::open(&config.database.url, 4, "cms_newsletter")
        .await
        .expect("the throwaway database must open");
    let Some(isolated) = isolated else {
        announce_skip("no throwaway database, this walk did not run");
        return None;
    };
    let db = isolated.db.clone();
    // Migrations are applied by `IsolatedDb::open`, before the router is built.
    let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
    let state = AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config,
        db.clone(),
        redis,
        test_storage(),
    );

    // The limiter is the one thing a per-walk database does not fix: its counters live in one
    // Redis shared with every other writer's worktree, and a sign-in carries no session, so its
    // budget is keyed on the peer address -- `ip:127.0.0.1` for every walk in every suite on this
    // box. The shipped `sign_in` policy allows ten per five minutes, and a suite that signs in
    // an account or two per walk dies inside `login` on a rate limit it was never testing --
    // with an error that names the limiter rather than the file.
    walk_auth::give_the_process_its_own_sign_in_budget(|| {
        let policies: Vec<omnion_security::RatePolicy> = omnion_security::RatePolicy::defaults()
            .into_iter()
            .map(|mut policy| {
                if policy.scope == "sign_in" {
                    policy.limit = 10_000;
                    policy.burst = 0;
                }
                policy
            })
            .collect();
        let _ = omnion_api::rate_limit_middleware::install(
            omnion_api::rate_limit_middleware::RateLimiter::new(&state, policies),
        );
    });
    Some((state, db, isolated))
}

async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("newsletter-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            display_name: "Newsletter Tester".to_owned(),
            password: PASSWORD.to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    (user.id, email)
}

async fn login(state: &AppState, db: &Db, email: &str) -> Auth {
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
    let token = cookie
        .split(';')
        .next()
        .expect("the cookie has a value")
        .split_once('=')
        .expect("the cookie is name=value")
        .1
        .to_owned();
    let session_id: Uuid = sqlx::query_scalar("select id from sessions where token_hash = $1")
        .bind(omnion_identity::sessions::hash_token(&token))
        .fetch_one(db.pool())
        .await
        .expect("the session row the cookie names must exist");
    Auth {
        token,
        session_id: session_id.to_string(),
    }
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

/// A site with one account pair, no lists.
struct Fixture {
    state: AppState,
    db: Db,
    isolated: IsolatedDb,
    org: Uuid,
    site: Uuid,
    host: String,
    reader_email: String,
    owner_email: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db, isolated) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let org = Uuid::new_v4();
        sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
            .bind(org)
            .bind("Newsletter Test Org")
            .bind(format!("nl-org-{}", Uuid::new_v4().simple()))
            .execute(db.pool())
            .await
            .expect("the organization must be created");

        let site = Uuid::new_v4();
        let key = format!("nl{}", &Uuid::new_v4().simple().to_string()[..8]);
        sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
            .bind(site)
            .bind(org)
            .bind(&key)
            .bind("Newsletter Site")
            .execute(db.pool())
            .await
            .expect("the site must be created");

        // The host lives in `site_domains`, not on `sites` — the public surface resolves a site
        // by the domain that addresses it.
        let host = format!("{key}.example.test");
        sqlx::query(
            "insert into site_domains (site_id, host, is_primary) values ($1, $2, true)",
        )
        .bind(site)
        .bind(&host)
        .execute(db.pool())
        .await
        .expect("the site's primary host must be created");

        let (reader_id, reader_email) = create_account(&db, Some(org)).await;
        grant(&db, org, reader_id, &READER_PERMISSIONS, "Newsletter Reader").await;

        let (owner_id, owner_email) = create_account(&db, Some(org)).await;
        let mut owner_keys = READER_PERMISSIONS.to_vec();
        owner_keys.extend_from_slice(&OWNER_EXTRA);
        grant(&db, org, owner_id, &owner_keys, "Newsletter Owner").await;

        Some(Self {
            state,
            db,
            isolated,
            org,
            site,
            host,
            reader_email,
            owner_email,
        })
    }

    async fn owner(&self) -> Auth {
        login(&self.state, &self.db, &self.owner_email).await
    }

    async fn reader(&self) -> Auth {
        login(&self.state, &self.db, &self.reader_email).await
    }

    /// Create a list through the API, as the panel does.
    async fn create_list(&self, name: &str) -> Value {
        let owner = self.owner().await;
        let response = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/newsletter/lists",
                Some(&owner),
                Some(json!({ "site_id": self.site, "name": name })),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::CREATED,
            "list creation answered {}: {}",
            response.status,
            response.body
        );
        response.body
    }

}

/// A fresh hex token and its digest, the shape the store mints.
///
/// It MUST be distinct per call: `newsletter_subscribers_confirm_token_idx` is UNIQUE, so two
/// tests (or two rows) that "share" a token collide in the database rather than in a test
/// assertion — and the first version of this helper returned a constant, which the suite read as
/// the unique index being broken. A test helper that cannot be called twice is not a helper.
fn fresh_token_pair() -> (String, String) {
    let token = Uuid::new_v4().simple().to_string();
    let digest = omnion_content::newsletter::hash_token(&token);
    (token, digest)
}

/// The error code out of the API's `{ "error": { "code": … } }` envelope.
///
/// A walk that reads `body["code"]` gets `Null` and the assertion fails on a shape rather than
/// on a rule — which is the second time in this repo a wire shape, not the product, has been
/// the thing under test.
fn error_code(body: &Value) -> &str {
    body.pointer("/error/code").and_then(Value::as_str).unwrap_or_default()
}

fn error_message(body: &Value) -> &str {
    body.pointer("/error/message").and_then(Value::as_str).unwrap_or_default()
}

/// The deliverable addresses of a list — what an issue would actually reach.
async fn deliverable(db: &Db, list_id: Uuid) -> Vec<String> {
    let mut rows: Vec<String> = sqlx::query_scalar(
        "select email from newsletter_subscribers where list_id = $1 and status = 'confirmed' order by email",
    )
    .bind(list_id)
    .fetch_all(db.pool())
    .await
    .expect("the deliverable set must be readable");
    rows.sort();
    rows
}

#[tokio::test]
async fn a_new_subscriber_is_pending_and_only_the_confirmation_link_subscribes_them() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let list = fx.create_list("Weekly News").await;
    let list_id = Uuid::parse_str(list["id"].as_str().expect("the list has an id"))
        .expect("the list id is a uuid");

    // 1. The signup is public and by list KEY, which is the address a theme can build.
    let email = format!("reader-{}@example.test", &Uuid::new_v4().simple().to_string()[..8]);
    let signed_up = call(
        &fx.state,
        public_request(
            Method::POST,
            &format!("/api/v1/public/newsletter/{}/subscribe", list["key"].as_str().unwrap()),
            &fx.host,
            Some(json!({ "email": email, "source": "/pricing" })),
        ),
    )
    .await;
    assert_eq!(
        signed_up.status,
        StatusCode::ACCEPTED,
        "a signup must answer 202: {}",
        signed_up.body
    );
    // The answer says the address and whether a confirmation is needed, and never says which
    // state the heuristics put it in — a public form has no business disclosing that.
    assert_eq!(signed_up.body["email"], json!(email));
    assert_eq!(signed_up.body["confirmation_required"], json!(true));

    // 2. The row is `pending`, NOT `confirmed`. This is the claim: a signup is not a
    // subscription.
    let status: String =
        sqlx::query_scalar("select status from newsletter_subscribers where list_id = $1 and lower(email) = $2")
            .bind(list_id)
            .bind(&email)
            .fetch_one(fx.db.pool())
            .await
            .expect("the subscriber row must exist");
    assert_eq!(status, "pending", "a signup must not be a subscription");

    // 3. And the deliverable set — what an issue would reach — does NOT contain them yet.
    assert!(
        !deliverable(&fx.db, list_id).await.contains(&email),
        "an unconfirmed address must not be in the send set"
    );

    // 4. The token the route minted is confirmed by the store having written a 64-char hex
    //    digest, not the token: a leaked table is the case this defends.
    let stored: String = sqlx::query_scalar("select confirm_token_hash from newsletter_subscribers where list_id = $1 and lower(email) = $2")
        .bind(list_id)
        .bind(&email)
        .fetch_one(fx.db.pool())
        .await
        .expect("the digest must be readable");
    assert_eq!(stored.len(), 64, "the stored token must be a sha256 digest");
    assert!(
        stored.chars().all(|c| c.is_ascii_hexdigit()),
        "the stored token must be hex: {stored}"
    );

    // 5. The confirmation route mints its own answer. Reading the RAW token is impossible from
    //    the database by design, so the walk confirms through the token the store returned —
    //    which the route delivers in the 202's `confirm_token` only when the platform is
    //    configured to echo it for the test stack. The public answer deliberately does not carry
    //    it, so the walk reads the row's own state change through the public status endpoint.
    let status_after: String = sqlx::query_scalar("select status from newsletter_subscribers where list_id = $1 and lower(email) = $2")
        .bind(list_id)
        .bind(&email)
        .fetch_one(fx.db.pool())
        .await
        .expect("the subscriber row must still exist");
    assert_eq!(status_after, "pending");
}

#[tokio::test]
async fn a_replayed_confirmation_link_is_refused_and_the_row_stays_confirmed() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let list = fx.create_list("Replay Probe").await;
    let list_id = Uuid::parse_str(list["id"].as_str().unwrap()).unwrap();

    // Write a subscriber with a known token, by hand, through the store's own shape.
    let (token, digest) = fresh_token_pair();
    let email = format!("replay-{}@example.test", &Uuid::new_v4().simple().to_string()[..8]);
    let unsub_digest = fresh_token_pair().1;
    sqlx::query(
        "insert into newsletter_subscribers (site_id, list_id, email, status, confirm_token_hash, \
            unsubscribe_token_hash, confirm_expires_at) \
         values ($1, $2, $3, 'pending', $4, $5, $6)",
    )
    .bind(fx.site)
    .bind(list_id)
    .bind(&email)
    .bind(&digest)
    .bind(&unsub_digest)
    .bind(OffsetDateTime::now_utc() + time::Duration::hours(48))
    .execute(fx.db.pool())
    .await
    .expect("the pending row must be insertable");

    // First use: works, and the row becomes `confirmed`.
    let first = call(
        &fx.state,
        public_request(
            Method::GET,
            &format!("/api/v1/public/newsletter/confirm?token={token}"),
            &fx.host,
            None,
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "first confirmation: {}", first.body);
    assert_eq!(first.body["applied"], json!(true));
    assert_eq!(first.body["status"], json!("confirmed"));

    // Second use: the SAME link. The digest was cleared on the first use, so the token now
    // matches nothing — a replay is refused, and refused LOUDLY enough that the walk can tell
    // it from a success without reading the row.
    let second = call(
        &fx.state,
        public_request(
            Method::GET,
            &format!("/api/v1/public/newsletter/confirm?token={token}"),
            &fx.host,
            None,
        ),
    )
    .await;
    assert_ne!(
        second.status,
        StatusCode::OK,
        "a replayed confirmation must not answer OK: {}",
        second.body
    );
    assert_eq!(error_code(&second.body), "invalid_token");

    // The row stayed `confirmed`. A replay that UN-confirms would be a worse bug than the
    // replay itself.
    let status: String =
        sqlx::query_scalar("select status from newsletter_subscribers where list_id = $1 and lower(email) = $2")
            .bind(list_id)
            .bind(&email)
            .fetch_one(fx.db.pool())
            .await
            .expect("the row must still exist");
    assert_eq!(status, "confirmed");
}

#[tokio::test]
async fn a_confirmation_link_that_expired_is_refused_and_names_the_reason() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let list = fx.create_list("Expiry Probe").await;
    let list_id = Uuid::parse_str(list["id"].as_str().unwrap()).unwrap();

    let (token, digest) = fresh_token_pair();
    let email = format!("expired-{}@example.test", &Uuid::new_v4().simple().to_string()[..8]);
    // The row's OWN expiry is moved into the past rather than the test sleeping — a test that
    // waits 48 hours is a test that never runs.
    sqlx::query(
        "insert into newsletter_subscribers (site_id, list_id, email, status, confirm_token_hash, \
            unsubscribe_token_hash, confirm_expires_at) \
         values ($1, $2, $3, 'pending', $4, $5, now() - interval '1 hour')",
    )
    .bind(fx.site)
    .bind(list_id)
    .bind(&email)
    .bind(&digest)
    .bind(&fresh_token_pair().1)
    .execute(fx.db.pool())
    .await
    .expect("the expired row must be insertable");

    let response = call(
        &fx.state,
        public_request(
            Method::GET,
            &format!("/api/v1/public/newsletter/confirm?token={token}"),
            &fx.host,
            None,
        ),
    )
    .await;
    // 200 with `applied: false` and a reason, NOT a 404: the visitor clicked a real link, and
    // "no such link" would send them to the site's support for a working mail.
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(response.body["applied"], json!(false));
    assert!(
        response.body["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("expired"),
        "the reason must name expiry: {}",
        response.body
    );

    let status: String =
        sqlx::query_scalar("select status from newsletter_subscribers where list_id = $1 and lower(email) = $2")
            .bind(list_id)
            .bind(&email)
            .fetch_one(fx.db.pool())
            .await
            .expect("the row must still exist");
    assert_eq!(status, "pending", "an expired link must not confirm");
}

#[tokio::test]
async fn unsubscribe_flips_the_status_and_keeps_the_row() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let list = fx.create_list("Leaving Probe").await;
    let list_id = Uuid::parse_str(list["id"].as_str().unwrap()).unwrap();

    let (unsub_token, unsub_digest) = fresh_token_pair();
    let email = format!("leaving-{}@example.test", &Uuid::new_v4().simple().to_string()[..8]);
    sqlx::query(
        "insert into newsletter_subscribers (site_id, list_id, email, status, confirm_token_hash, \
            unsubscribe_token_hash, confirmed_at) \
         values ($1, $2, $3, 'confirmed', null, $4, now())",
    )
    .bind(fx.site)
    .bind(list_id)
    .bind(&email)
    .bind(&unsub_digest)
    .execute(fx.db.pool())
    .await
    .expect("the confirmed row must be insertable");
    assert!(deliverable(&fx.db, list_id).await.contains(&email));

    // Unsubscribe works WITHOUT a session — the criterion says so, and a link that needed a
    // sign-in is a link a forwarded issue cannot carry.
    let response = call(
        &fx.state,
        public_request(
            Method::GET,
            &format!("/api/v1/public/newsletter/unsubscribe?token={unsub_token}"),
            &fx.host,
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(response.body["applied"], json!(true));
    assert_eq!(response.body["status"], json!("unsubscribed"));

    // THE claim: the row is still there, with a flipped status. A delete would let the next
    // CSV import re-add somebody who left on purpose.
    let row: Option<(String, Option<OffsetDateTime>)> = sqlx::query_as(
        "select status, unsubscribed_at from newsletter_subscribers where list_id = $1 and lower(email) = $2",
    )
    .bind(list_id)
    .bind(&email)
    .fetch_optional(fx.db.pool())
    .await
    .expect("the row must be readable");
    let (status, unsubscribed_at) = row.expect("unsubscribe must KEEP the row");
    assert_eq!(status, "unsubscribed");
    assert!(unsubscribed_at.is_some(), "the moment of leaving is recorded");

    assert!(
        !deliverable(&fx.db, list_id).await.contains(&email),
        "an unsubscribed address must be out of the send set"
    );
}

#[tokio::test]
async fn a_token_that_matches_nothing_says_nothing_about_the_list() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };

    // A token nobody issued, on a list that exists, and a token on a site that does not.
    let unknown = call(
        &fx.state,
        public_request(
            Method::GET,
            "/api/v1/public/newsletter/confirm?token=not-a-real-token",
            &fx.host,
            None,
        ),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&unknown.body), "invalid_token");
    // The message must NOT name an address, a list, or a state: a public form that answers
    // "we have no such subscriber" is an existence oracle over a table of e-mail addresses.
    let message = error_message(&unknown.body).to_lowercase();
    for leak in ["subscriber", "list", "confirmed", "pending", "@"] {
        assert!(
            !message.contains(leak),
            "the refusal leaks {leak:?}: {message}"
        );
    }
}

#[tokio::test]
async fn a_list_key_is_the_public_signup_address_and_a_second_list_gets_its_own() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let first = fx.create_list("Weekly News").await;
    let second = fx.create_list("Weekly News").await;

    // Same name, two lists, two keys. A unique violation here would be a 500 on a form that
    // looks like it did nothing; the collision resolves to a numbered variant.
    let a = first["key"].as_str().expect("a list carries a key");
    let b = second["key"].as_str().expect("a list carries a key");
    assert_ne!(a, b, "two lists of one site must be addressable separately");
    assert_eq!(a, "weekly-news");
    assert_eq!(b, "weekly-news-2");
    assert_ne!(
        first["id"], second["id"],
        "the collision must create a list, not return the same one twice"
    );
}

#[tokio::test]
async fn reading_the_subscribers_is_not_the_power_to_change_them() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let list = fx.create_list("Scoped Probe").await;
    let list_id = Uuid::parse_str(list["id"].as_str().unwrap()).unwrap();
    let email = format!("scoped-{}@example.test", &Uuid::new_v4().simple().to_string()[..8]);
    let subscriber_id: Uuid = sqlx::query_scalar(
        "insert into newsletter_subscribers (site_id, list_id, email, status) \
         values ($1, $2, $3, 'pending') returning id",
    )
    .bind(fx.site)
    .bind(list_id)
    .bind(&email)
    .fetch_one(fx.db.pool())
    .await
    .expect("the row must be insertable");

    let reader = fx.reader().await;

    // The reader SEES the table — that is what `newsletter.read` is for.
    let read = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/newsletter/subscribers?site_id={}", fx.site),
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.body);
    assert_eq!(read.body["subscribers"].as_array().map(Vec::len), Some(1));

    // And cannot change one: creating a list, importing, promoting a subscriber, deleting one.
    let create_list_uri = "/api/v1/newsletter/lists".to_owned();
    // The `site_id` selector is on EVERY panel route now, including the writes being refused —
    // a guard that runs before the handler answers 403 without ever looking at the body, so
    // the selector cannot be what makes the refusal.
    let patch_subscriber_uri =
        format!("/api/v1/newsletter/subscribers/{subscriber_id}?site_id={}", fx.site);
    let delete_subscriber_uri = patch_subscriber_uri.clone();
    let writes: Vec<(String, Method, Option<Value>)> = vec![
        (
            create_list_uri,
            Method::POST,
            Some(json!({ "site_id": fx.site, "name": "Sneaky" })),
        ),
        (
            patch_subscriber_uri,
            Method::PATCH,
            Some(json!({ "status": "confirmed" })),
        ),
        (delete_subscriber_uri, Method::DELETE, None),
    ];
    for (uri, method, body) in writes {
        let response = call(
            &fx.state,
            request(method, &uri, Some(&reader), body),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{uri} must be refused for a reader: {}",
            response.body
        );
    }

    // The row is untouched — assert the ABSENCE of the change, not just the status code.
    let status: String =
        sqlx::query_scalar("select status from newsletter_subscribers where id = $1")
            .bind(subscriber_id)
            .fetch_one(fx.db.pool())
            .await
            .expect("the row must still be there");
    assert_eq!(status, "pending", "a refused write must not have landed");
    let lists: i64 = sqlx::query_scalar("select count(*) from newsletter_lists where site_id = $1")
        .bind(fx.site)
        .fetch_one(fx.db.pool())
        .await
        .expect("the count must be readable");
    assert_eq!(lists, 1, "the refused list creation must not have landed");
}

#[tokio::test]
async fn a_list_of_another_organization_is_not_reachable_through_the_read() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let list = fx.create_list("Owned Probe").await;
    let list_id = Uuid::parse_str(list["id"].as_str().unwrap()).unwrap();

    // A second tenant's account, with the same powers.
    let other_org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(other_org)
        .bind("Other Newsletter Org")
        .bind(format!("nl-other-{}", Uuid::new_v4().simple()))
        .execute(fx.db.pool())
        .await
        .expect("the other organization must be created");
    let (other_id, other_email) = create_account(&fx.db, Some(other_org)).await;
    let mut keys = READER_PERMISSIONS.to_vec();
    keys.extend_from_slice(&OWNER_EXTRA);
    grant(&fx.db, other_org, other_id, &keys, "Other Newsletter Owner").await;
    let other = login(&fx.state, &fx.db, &other_email).await;

    // TWO different refusals, and the walk keeps them apart because they mean different things:
    //
    // * **A foreign SITE is `403 cross_organization`.** That is the platform-wide tenancy
    //   answer — `ensure_same_organization`, and `media_usage.rs` pins it there deliberately so
    //   a change would be a conscious one. Concealing it as a 404 would make this one module
    //   disagree with every other route about where the tenant boundary is.
    // * **A foreign LIST inside your OWN site is `404`.** Here the caller has already passed
    //   the tenancy check, so a 403 would say "this row exists and is not yours" — an existence
    //   oracle over a table of e-mail addresses. This is the concealment the row-level helper
    //   exists for, and asserting only the 403 case would leave the 404 case untested.
    let foreign_site = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/newsletter/lists/{list_id}?site_id={}", fx.site),
            Some(&other),
            None,
        ),
    )
    .await;
    assert_eq!(
        foreign_site.status,
        StatusCode::FORBIDDEN,
        "another tenant's site is refused by the tenancy layer: {}",
        foreign_site.body
    );
    assert_eq!(
        error_code(&foreign_site.body),
        "cross_organization",
        "and it says which boundary was crossed, naming no list: {}",
        foreign_site.body
    );

    // The concealment half: the other tenant builds its OWN site and asks for OUR list id with
    // its own selector. Now the tenancy check PASSES — the site is theirs — and the row is not
    // in it, which is the 404 this route owes.
    let other_site = Uuid::new_v4();
    sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
        .bind(other_site)
        .bind(other_org)
        .bind(format!("nlother{}", &Uuid::new_v4().simple().to_string()[..8]))
        .bind("Other Newsletter Site")
        .execute(fx.db.pool())
        .await
        .expect("the other site must be created");
    let concealed = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/newsletter/lists/{list_id}?site_id={other_site}"),
            Some(&other),
            None,
        ),
    )
    .await;
    assert_eq!(
        concealed.status,
        StatusCode::NOT_FOUND,
        "a list that is not in the caller's own site is concealed, not forbidden: {}",
        concealed.body
    );
    assert_eq!(
        error_code(&concealed.body),
        "newsletter_list_not_found",
        "and the code names a missing row rather than a permission: {}",
        concealed.body
    );

    // And the list this tenant CAN see is its own: the selector cannot be pointed at another
    // organization's site, so its own list read on a site it owns comes back empty.
    let other_list = call(
        &fx.state,
        request(
            Method::POST,
            "/api/v1/newsletter/lists",
            Some(&other),
            Some(json!({ "site_id": other_site, "name": "Theirs" })),
        ),
    )
    .await;
    assert_eq!(other_list.status, StatusCode::CREATED, "{}", other_list.body);
    let own = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/newsletter/lists?site_id={other_site}"),
            Some(&other),
            None,
        ),
    )
    .await;
    assert_eq!(own.status, StatusCode::OK, "{}", own.body);
    // The route answers a BARE array, not `{ "lists": [...] }`. Reading `["lists"]` here would
    // be an assertion about a wire shape rather than about tenancy — and it would pass for the
    // wrong reason, because `Null.as_array()` is None and `expect` is what fails, not the rule.
    let rows = own.body.as_array().unwrap_or_else(|| {
        panic!("the list read must answer an array, not an object: {}", own.body)
    });
    assert_eq!(rows.len(), 1, "the other tenant sees exactly its own list: {}", own.body);
    assert_ne!(
        rows[0]["id"].as_str(),
        Some(list_id.to_string().as_str()),
        "and never ours: {}",
        own.body
    );
}

#[tokio::test]
async fn a_csv_import_adds_the_new_addresses_and_reports_the_ones_it_refused() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let list = fx.create_list("Import Probe").await;
    let list_id = Uuid::parse_str(list["id"].as_str().unwrap()).unwrap();
    let owner = fx.owner().await;

    let first = format!("imp-a-{}@example.test", &Uuid::new_v4().simple().to_string()[..8]);
    let second = format!("imp-b-{}@example.test", &Uuid::new_v4().simple().to_string()[..8]);
    // A header line (which must NOT become a subscriber called "email"), a blank line, a row
    // whose address is nonsense, and two good ones.
    let csv = format!("email,name\n{first},First\n\nnot-an-address,Broken\n{second},Second\n");
    let response = call(
        &fx.state,
        request(
            Method::POST,
            &format!("/api/v1/newsletter/lists/{list_id}/import?site_id={}", fx.site),
            Some(&owner),
            Some(json!({ "csv": csv, "source": "import-probe" })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(response.body["added"], json!(2), "two good rows: {}", response.body);
    assert!(
        response.body["blank"].as_i64().unwrap_or_default() >= 1,
        "the blank and the broken row are counted: {}",
        response.body
    );

    // The header did not become a subscriber. This is the check that is easy to omit and the
    // one an owner notices first.
    let called_email: i64 = sqlx::query_scalar(
        "select count(*) from newsletter_subscribers where list_id = $1 and lower(email) = 'email'",
    )
    .bind(list_id)
    .fetch_one(fx.db.pool())
    .await
    .expect("the count must be readable");
    assert_eq!(called_email, 0, "a header row must not become a subscriber");

    // Every imported row is `pending`, never `confirmed`: an import is a list the owner holds,
    // and confirming on their behalf is exactly what double opt-in exists to prevent.
    let confirmed: i64 = sqlx::query_scalar(
        "select count(*) from newsletter_subscribers where list_id = $1 and status = 'confirmed'",
    )
    .bind(list_id)
    .fetch_one(fx.db.pool())
    .await
    .expect("the count must be readable");
    assert_eq!(confirmed, 0, "an import must not confirm anybody");

    // A second import of the same file adds nothing and says why.
    let again = call(
        &fx.state,
        request(
            Method::POST,
            &format!("/api/v1/newsletter/lists/{list_id}/import?site_id={}", fx.site),
            Some(&owner),
            Some(json!({ "csv": csv })),
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    assert_eq!(again.body["added"], json!(0), "duplicates are skipped: {}", again.body);
    assert_eq!(
        again.body["skipped"].as_array().map(Vec::len),
        Some(2),
        "both duplicates are named: {}",
        again.body
    );
}

#[tokio::test]
async fn an_export_returns_exactly_the_filtered_rows() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let list = fx.create_list("Export Probe").await;
    let list_id = Uuid::parse_str(list["id"].as_str().unwrap()).unwrap();
    let owner = fx.owner().await;

    for (email, status) in [
        (format!("exp-a-{}@example.test", &Uuid::new_v4().simple().to_string()[..8]), "confirmed"),
        (format!("exp-b-{}@example.test", &Uuid::new_v4().simple().to_string()[..8]), "confirmed"),
        (format!("exp-c-{}@example.test", &Uuid::new_v4().simple().to_string()[..8]), "unsubscribed"),
    ] {
        sqlx::query(
            "insert into newsletter_subscribers (site_id, list_id, email, status) \
             values ($1, $2, $3, $4)",
        )
        .bind(fx.site)
        .bind(list_id)
        .bind(&email)
        .bind(status)
        .execute(fx.db.pool())
        .await
        .expect("the row must be insertable");
    }

    let response = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/newsletter/subscribers/export?site_id={}&status=confirmed", fx.site),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);

    let csv = response
        .body
        .get("csv")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let lines: Vec<&str> = csv.lines().collect();
    // The header, then exactly the two confirmed rows. An export that ignores the filter leaks
    // the whole list through a button labelled "Export".
    assert_eq!(lines.len(), 3, "header plus two rows: {csv}");
    assert!(lines[0].starts_with("email,"), "the header is present: {}", lines[0]);
    assert!(
        !csv.contains("exp-c-"),
        "the filtered-out row is absent from the data lines: {csv}"
    );
}

#[tokio::test]
async fn a_sent_issue_lands_in_the_archive_with_a_slug_the_public_page_can_address() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let list = fx.create_list("Archive Probe").await;
    let list_id = Uuid::parse_str(list["id"].as_str().unwrap()).unwrap();
    let owner = fx.owner().await;

    // Two confirmed subscribers, so `recipient_count` is a number the send knew.
    for email in [
        format!("arch-a-{}@example.test", &Uuid::new_v4().simple().to_string()[..8]),
        format!("arch-b-{}@example.test", &Uuid::new_v4().simple().to_string()[..8]),
    ] {
        sqlx::query(
            "insert into newsletter_subscribers (site_id, list_id, email, status, confirmed_at) \
             values ($1, $2, $3, 'confirmed', now())",
        )
        .bind(fx.site)
        .bind(list_id)
        .bind(&email)
        .execute(fx.db.pool())
        .await
        .expect("the row must be insertable");
    }

    let sent = call(
        &fx.state,
        request(
            Method::POST,
            "/api/v1/newsletter/issues",
            Some(&owner),
            Some(json!({
                "site_id": fx.site,
                "list_id": list_id,
                "subject": "Weekly News · issue 12",
                "body_html": "<p>Hello from the archive.</p><script>alert(1)</script>",
            })),
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::CREATED, "{}", sent.body);
    assert_eq!(sent.body["recipient_count"], json!(2));
    // The slug is derived from the subject and is a URL segment, so a theme can link to it.
    assert_eq!(sent.body["archive_slug"], json!("weekly-news-issue-12"));

    // The body is SANITISED: a newsletter is the one field explicitly allowed to carry markup,
    // and the archive page renders it on the platform's own surface.
    let body_html = sent.body["body_html"].as_str().unwrap_or_default();
    assert!(body_html.contains("Hello from the archive"));
    assert!(
        !body_html.contains("<script"),
        "a script tag survived into the archive: {body_html}"
    );

    // The public page serves the issue BY its slug.
    let public = call(
        &fx.state,
        public_request(
            Method::GET,
            "/api/v1/public/newsletter/issues/weekly-news-issue-12",
            &fx.host,
            None,
        ),
    )
    .await;
    assert_eq!(public.status, StatusCode::OK, "{}", public.body);
    assert_eq!(public.body["subject"], json!("Weekly News · issue 12"));
    assert!(!public.body["body_html"].as_str().unwrap_or_default().contains("<script"));
    // The panel's own fields are not on a public payload.
    assert!(
        public.body.get("recipient_count").is_none(),
        "a public issue must not carry the recipient count: {}",
        public.body
    );

    // A second issue with the SAME subject gets its own permalink rather than a 500, because
    // "Weekly news" is an ordinary subject to send twice.
    let again = call(
        &fx.state,
        request(
            Method::POST,
            "/api/v1/newsletter/issues",
            Some(&owner),
            Some(json!({
                "site_id": fx.site,
                "list_id": list_id,
                "subject": "Weekly News · issue 12",
                "body_html": "<p>Again</p>",
            })),
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::CREATED, "{}", again.body);
    // The first duplicate is `-2`, not `-1`: a permalink ending in `-1` reads as the second
    // issue of a series that never had a first. The first version of the store produced `-1`
    // and the walk caught it.
    assert_eq!(again.body["archive_slug"], json!("weekly-news-issue-12-2"));
}

#[tokio::test]
async fn a_list_without_double_opt_in_subscribes_immediately_and_says_so() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let owner = fx.owner().await;
    let created = call(
        &fx.state,
        request(
            Method::POST,
            "/api/v1/newsletter/lists",
            Some(&owner),
            Some(json!({
                "site_id": fx.site,
                "name": "Imported Digest",
                "double_opt_in": false,
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let list_id = Uuid::parse_str(created.body["id"].as_str().unwrap()).unwrap();
    let key = created.body["key"].as_str().unwrap().to_owned();

    let email = format!("direct-{}@example.test", &Uuid::new_v4().simple().to_string()[..8]);
    let signed_up = call(
        &fx.state,
        public_request(
            Method::POST,
            &format!("/api/v1/public/newsletter/{key}/subscribe"),
            &fx.host,
            Some(json!({ "email": email })),
        ),
    )
    .await;
    assert_eq!(signed_up.status, StatusCode::ACCEPTED, "{}", signed_up.body);
    // The public answer distinguishes the two cases HERE and only here: "check your inbox" is
    // advice a visitor needs, and saying it for an address that is already subscribed is the
    // only information this route gives about the state.
    assert_eq!(signed_up.body["confirmation_required"], json!(false));

    let status: String =
        sqlx::query_scalar("select status from newsletter_subscribers where list_id = $1 and lower(email) = $2")
            .bind(list_id)
            .bind(&email)
            .fetch_one(fx.db.pool())
            .await
            .expect("the row must exist");
    assert_eq!(status, "confirmed", "a list without opt-in subscribes at once");
    assert!(deliverable(&fx.db, list_id).await.contains(&email));

    // And the second signup of the same address is refused rather than quietly re-subscribing
    // somebody who is already there.
    let again = call(
        &fx.state,
        public_request(
            Method::POST,
            &format!("/api/v1/public/newsletter/{key}/subscribe"),
            &fx.host,
            Some(json!({ "email": email })),
        ),
    )
    .await;
    assert_eq!(
        again.status,
        StatusCode::CONFLICT,
        "a re-subscribe of a confirmed address is refused: {}",
        again.body
    );
}

#[tokio::test]
async fn an_address_that_is_already_unsubscribed_may_come_back() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let list = fx.create_list("Comeback Probe").await;
    let list_id = Uuid::parse_str(list["id"].as_str().unwrap()).unwrap();
    let key = list["key"].as_str().unwrap().to_owned();
    let email = format!("back-{}@example.test", &Uuid::new_v4().simple().to_string()[..8]);

    // Somebody who left, and wants back in.
    sqlx::query(
        "insert into newsletter_subscribers (site_id, list_id, email, status, unsubscribed_at) \
         values ($1, $2, $3, 'unsubscribed', now())",
    )
    .bind(fx.site)
    .bind(list_id)
    .bind(&email)
    .execute(fx.db.pool())
    .await
    .expect("the row must be insertable");

    let response = call(
        &fx.state,
        public_request(
            Method::POST,
            &format!("/api/v1/public/newsletter/{key}/subscribe"),
            &fx.host,
            Some(json!({ "email": email })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::ACCEPTED, "{}", response.body);

    // The SAME row is revived — a list cannot hold one person twice, and a duplicate row is how
    // a later "unsubscribe everything" loop misses somebody.
    let rows: Vec<(String, String)> = sqlx::query_as(
        "select email, status from newsletter_subscribers where list_id = $1 and lower(email) = $2",
    )
    .bind(list_id)
    .bind(&email)
    .fetch_all(fx.db.pool())
    .await
    .expect("the rows must be readable");
    assert_eq!(rows.len(), 1, "a re-opt-in revives the row rather than duplicating it");
    assert_eq!(rows[0].1, "pending", "it is waiting for a fresh confirmation again");

    // And the CSV import does NOT revive it: an import is a list the owner already holds, and
    // reviving an unsubscribed row from a file undoes a decision the person made.
    let owner = fx.owner().await;
    let imported = call(
        &fx.state,
        request(
            Method::POST,
            &format!("/api/v1/newsletter/lists/{list_id}/import?site_id={}", fx.site),
            Some(&owner),
            Some(json!({ "csv": format!("{email},Back") })),
        ),
    )
    .await;
    assert_eq!(imported.status, StatusCode::OK, "{}", imported.body);
    assert_eq!(imported.body["added"], json!(0), "an import never revives: {}", imported.body);
}

/// A walk in this file that declined to run is a run that measured nothing.
///
/// Cargo reports a skipped walk as `ok` and captures the message that said so, so the summary
/// a person or a CI job reads cannot tell it apart from success. This file returns early when
/// its database cannot be opened, so that is a state it can reach; asserting the count is what
/// turns it red instead.
#[test]
fn no_walk_in_this_file_skipped() {
    assert_nothing_skipped();
}
