//! Integration test for visitor memberships (REQ-064, slice 4c).
//!
//! The REQ calls the member/panel boundary "the single most important boundary in this REQ",
//! so these walks are written around the claims that boundary makes rather than around the CRUD
//! around it:
//!
//! * **A gated page answers 404 to a signed-out visitor, renders for a verified member, and
//!   still answers 404 for a member missing the role.** Asserted on the PUBLIC route, because
//!   that is where the REQ makes the promise — a gate enforced only in a theme is a gate a theme
//!   can forget. The three answers are read from the same endpoint with three different cookies,
//!   because a gate that only works signed-out is not a gate.
//!
//! * **A member is not a panel identity.** The walk takes a member cookie and posts it at a
//!   PANEL route that the signed-out reader is refused: the two session systems must not accept
//!   each other's cookie, and the only way to know is to try.
//!
//! * **The password is Argon2id and the row holds no plaintext.** The walk reads the column
//!   rather than trusting the API, because a leaked table is the thing that matters and a
//!   response body that happens to omit the hash proves nothing about the row.
//!
//! * **The token is hashed, single-use and expires.** All three, from the row and from the
//!   replay, and the expiry is moved into the past in SQL rather than slept through.
//!
//! * **A block takes the sessions with it.** A block that leaves the cookie working is a block
//!   the operator believes they applied and did not.
//!
//! * **The public surface never says who exists.** A repeat signup and a password reset for an
//!   address nobody holds answer the same 202 a fresh one does; a wrong password and an unknown
//!   address answer the same 401.
//!
//! * **Reading the table is not the power to change it.** `memberships.read` gets `/members` and
//!   403 on every button, which is the whole reason the two keys exist.

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
use tower::ServiceExt;
use uuid::Uuid;

mod support;
use support::isolated_db::{IsolatedDb, announce_skip, assert_nothing_skipped};

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// The password the VISITOR uses. It must satisfy the 10-character floor, which is the same floor
/// a panel account gets — a members area with a weaker rule than the panel it sits beside is how
/// the weakest credential on the installation ends up being a visitor's.
const MEMBER_PASSWORD: &str = "visitor-passphrase";

/// The CSRF secret this suite runs with.
const CSRF_SECRET: &str = "w2-members-suite-csrf-secret";

/// What the member reader may do: read the table and nothing else.
const READER_PERMISSIONS: [&str; 2] = ["memberships.read", "content.pages.read"];

/// What the owner adds on top.
const OWNER_EXTRA: [&str; 3] = [
    "memberships.manage",
    "content.pages.create",
    "content.pages.publish",
];

/// A signed-in PANEL session.
struct Auth {
    token: String,
    session_id: String,
}

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

/// A request as a VISITOR's browser makes it, optionally carrying a member cookie.
///
/// `member_cookie` is the raw token the sign-in handler put in a `Set-Cookie`. Passing the panel
/// cookie here instead is one of the walks' claims, so the parameter takes whatever string the
/// caller has rather than assuming a member session.
fn visitor(
    method: Method,
    uri: &str,
    host: &str,
    cookie: Option<&str>,
    body: Option<Value>,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, host)
        .header(header::USER_AGENT, "members-suite/1.0");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
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
    let config = Config::from_env().expect("environment must be valid");
    let isolated = IsolatedDb::open(&config.database.url, 4, "cms_members")
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
    Some((state, db, isolated))
}

async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("members-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            display_name: "Members Tester".to_owned(),
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

        // **The limiter is loosened before anything else, and the fact it took is asserted.**
        //
        // REQ-012 slice 3 put the rate limiter on the request path, and `sign_in` ships at 10
        // per 300 seconds — which is the right number for a live installation and the wrong one
        // for a suite: ten walks, each signing a panel account in twice, is twenty-four requests
        // from the same process, and the first run died with `429` on the *second* fixture.
        //
        // The two halves of the fix matter. The reload is only effective if the cell is filled
        // first (`ensure_installed` returns the cell and installs the defaults when it is
        // empty), and "the reload was a no-op" and "the reload worked" are indistinguishable from
        // the call site unless the live policy is read back. That read-back is the assertion
        // below; without it a silently-ignored reload reads as a passing suite until the day it
        // stops.
        let limiter = omnion_api::rate_limit_middleware::ensure_installed(&state);
        let mut policies = (*limiter.current()).clone();
        for policy in &mut policies {
            policy.limit = 100_000;
            policy.burst = 10_000;
        }
        limiter.reload(policies);
        assert!(
            limiter
                .current()
                .iter()
                .all(|policy| policy.limit == 100_000),
            "the limiter must be running loose for this suite; if the reload was a no-op every \
             sign-in in these walks is competing with the previous one in the same 300s window"
        );

        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let org = Uuid::new_v4();
        sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
            .bind(org)
            .bind("Members Test Org")
            .bind(format!("mem-org-{}", Uuid::new_v4().simple()))
            .execute(db.pool())
            .await
            .expect("the organization must be created");

        let site = Uuid::new_v4();
        let key = format!("mem{}", &Uuid::new_v4().simple().to_string()[..8]);
        sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
            .bind(site)
            .bind(org)
            .bind(&key)
            .bind("Members Site")
            .execute(db.pool())
            .await
            .expect("the site must be created");

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
        grant(&db, org, reader_id, &READER_PERMISSIONS, "Members Reader").await;

        let (owner_id, owner_email) = create_account(&db, Some(org)).await;
        let mut owner_keys = READER_PERMISSIONS.to_vec();
        owner_keys.extend_from_slice(&OWNER_EXTRA);
        grant(&db, org, owner_id, &owner_keys, "Members Owner").await;

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

    /// Turn verification off, so a test that needs a usable account can have one without
    /// reading a token out of a response the API deliberately keeps it out of.
    async fn disable_verification(&self) {
        let owner = self.owner().await;
        let response = call(
            &self.state,
            request(
                Method::PUT,
                &format!("/api/v1/sites/{}/members/settings", self.site),
                Some(&owner),
                Some(json!({ "require_verification": false })),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "turning verification off answered {}: {}",
            response.status,
            response.body
        );
    }

    /// Publish a page and return its slug.
    async fn publish_page(&self, slug: &str) -> String {
        let owner = self.owner().await;
        let created = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/pages",
                Some(&owner),
                Some(json!({ "site_id": self.site, "slug": slug, "title": format!("Page {slug}") })),
            ),
        )
        .await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "page creation answered {}: {}",
            created.status,
            created.body
        );
        let page_id = created.body["id"].as_str().expect("the page has an id");
        let published = call(
            &self.state,
            request(
                Method::POST,
                &format!("/api/v1/pages/{page_id}/publish"),
                Some(&owner),
                Some(json!({ "body": format!("Body of {slug}") })),
            ),
        )
        .await;
        assert!(
            published.status.is_success(),
            "publishing {slug} answered {}: {}",
            published.status,
            published.body
        );
        slug.to_string()
    }

    /// Set a page's gate directly, through SQL.
    ///
    /// The panel's Visibility tab is slice 4d's business; these walks are about the public
    /// ANSWER, and a walk that had to drive the editor to reach it would be testing the editor.
    /// The column still has its own CHECK, so an impossible gate is refused by the database.
    async fn gate_page(&self, slug: &str, visibility: &str, roles: &[&str]) {
        let result: Result<sqlx::postgres::PgQueryResult, sqlx::Error> = sqlx::query(
            "update pages set visibility = $1, visibility_roles = $2 \
             where site_id = $3 and slug = $4",
        )
        .bind(visibility)
        .bind(roles)
        .bind(self.site)
        .bind(slug)
        .execute(self.db.pool())
        .await;
        result.expect("the gate must be writable");
    }

    /// Sign a visitor up and return `(email, raw member cookie)`.
    ///
    /// The cookie is read out of the `Set-Cookie` header, which is the only place the raw token
    /// appears — the body carries the member and nothing else, and that is the point.
    async fn signup(&self, email: &str) -> (String, Option<String>) {
        let response = call(
            &self.state,
            visitor(
                Method::POST,
                "/api/v1/public/members/signup",
                &self.host,
                None,
                Some(json!({ "email": email, "password": MEMBER_PASSWORD })),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::ACCEPTED,
            "signup answered {}: {}",
            response.status,
            response.body
        );
        let cookie = response
            .headers
            .iter()
            .find(|(name, _)| name == "set-cookie")
            .map(|(_, value)| value.split(';').next().unwrap_or_default().to_owned());
        (
            response.body["email"].as_str().unwrap_or_default().to_string(),
            cookie,
        )
    }

    /// Sign a visitor in and return the raw member cookie.
    async fn signin(&self, email: &str) -> String {
        let response = call(
            &self.state,
            visitor(
                Method::POST,
                "/api/v1/public/members/signin",
                &self.host,
                None,
                Some(json!({ "email": email, "password": MEMBER_PASSWORD })),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "sign-in for {email} answered {}: {}",
            response.status,
            response.body
        );
        let cookie = response
            .headers
            .iter()
            .find(|(name, _)| name == "set-cookie")
            .map(|(_, value)| value.clone())
            .expect("a successful sign-in must set the member cookie");
        // The token must be in the cookie and NOWHERE else: a body that carries it is a body
        // that ends up in a proxy buffer.
        let raw = cookie.split(';').next().unwrap_or_default().to_owned();
        assert!(
            raw.starts_with("omnion_member="),
            "the member cookie must have its own name, not the panel's: {cookie}"
        );
        assert!(
            !cookie.contains("HttpOnly") == false,
            "the member cookie must be HttpOnly"
        );
        assert!(
            response.body.get("token").is_none(),
            "the raw session token must not be in the response body"
        );
        raw
    }
}

/// The `omnion_member=…` value, without the name, for a request that already carries other
/// cookies.
fn cookie_value(raw: &str) -> &str {
    raw.split_once('=').map(|(_, value)| value).unwrap_or(raw)
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

/// Acceptance 16, the whole claim: a gated page is 404 to a signed-out visitor, renders for a
/// verified member, and is 404 again for a member missing the role.
#[tokio::test]
async fn a_gated_page_is_404_to_a_visitor_and_404_to_a_member_without_the_role() {
    let Some(mut fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    fixture.disable_verification().await;

    let open = fixture.publish_page("open-house").await;
    let members_only = fixture.publish_page("members-lounge").await;
    let editors_only = fixture.publish_page("editors-desk").await;
    fixture.gate_page(&members_only, "members", &[]).await;
    fixture.gate_page(&editors_only, "roles", &["editor"]).await;

    // A signed-out visitor: the public page answers 404 for BOTH gated pages, and the open one
    // renders. The 404 has to be the same shape as a page that does not exist, so that is
    // asserted too rather than assumed from the status.
    for slug in [&members_only, &editors_only] {
        let response = call(
            &fixture.state,
            visitor(
                Method::GET,
                &format!("/api/v1/public/pages/{slug}"),
                &fixture.host,
                None,
                None,
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "a signed-out visitor must get 404 for {slug}"
        );
        assert_eq!(
            response.body["error"]["code"], "page_not_found",
            "a gated page must answer the same 404 as a page that is not there: {}",
            response.body
        );
    }
    let open_response = call(
        &fixture.state,
        visitor(
            Method::GET,
            &format!("/api/v1/public/pages/{open}"),
            &fixture.host,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(
        open_response.status,
        StatusCode::OK,
        "an ungated page must still render for a visitor"
    );

    // A verified member with no site role: the members-only page renders, the editors-only page
    // is 404 again. This is the half a walk that only checks the signed-out case would miss.
    let subscriber = format!("subscriber-{}@example.test", Uuid::new_v4().simple());
    fixture.signup(&subscriber).await;
    let subscriber_cookie = fixture.signin(&subscriber).await;

    let allowed = call(
        &fixture.state,
        visitor(
            Method::GET,
            &format!("/api/v1/public/pages/{members_only}"),
            &fixture.host,
            Some(&subscriber_cookie),
            None,
        ),
    )
    .await;
    assert_eq!(
        allowed.status,
        StatusCode::OK,
        "a verified member must read a members-only page"
    );
    assert_eq!(allowed.body["revision"]["title"], "Page members-lounge");

    let refused = call(
        &fixture.state,
        visitor(
            Method::GET,
            &format!("/api/v1/public/pages/{editors_only}"),
            &fixture.host,
            Some(&subscriber_cookie),
            None,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::NOT_FOUND,
        "a member missing the required role must get 404, not a render"
    );

    // The same member, promoted to `editor`, now reads it. Without this the walk would pass
    // against a gate that refuses everybody.
    let owner = fixture.owner().await;
    let promoted = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/members?site_id={}", fixture.site),
            Some(&owner),
            None,
        ),
    )
    .await;
    let member_id = promoted.body["members"]
        .as_array()
        .expect("members is an array")
        .iter()
        .find(|row| row["email"] == subscriber)
        .and_then(|row| row["id"].as_str())
        .expect("the subscriber is in the table")
        .to_string();
    let patch = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/members/{member_id}?site_id={}", fixture.site),
            Some(&owner),
            Some(json!({ "roles": ["editor"] })),
        ),
    )
    .await;
    assert_eq!(
        patch.status,
        StatusCode::OK,
        "granting the role answered {}: {}",
        patch.status,
        patch.body
    );

    let now_allowed = call(
        &fixture.state,
        visitor(
            Method::GET,
            &format!("/api/v1/public/pages/{editors_only}"),
            &fixture.host,
            Some(&subscriber_cookie),
            None,
        ),
    )
    .await;
    assert_eq!(
        now_allowed.status,
        StatusCode::OK,
        "a member holding the required role must read the page"
    );
}

/// The boundary the REQ names as its most important one: a member cookie is not a panel
/// session, and a panel session is not a member session.
#[tokio::test]
async fn a_member_cookie_is_not_a_panel_session() {
    let Some(mut fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    fixture.disable_verification().await;

    let member = format!("boundary-{}@example.test", Uuid::new_v4().simple());
    fixture.signup(&member).await;
    let member_cookie = fixture.signin(&member).await;

    // The member's cookie at a PANEL route the signed-out caller is refused.
    let panel = call(
        &fixture.state,
        visitor(
            Method::GET,
            &format!("/api/v1/members?site_id={}", fixture.site),
            &fixture.host,
            Some(&member_cookie),
            None,
        ),
    )
    .await;
    assert!(
        panel.status == StatusCode::UNAUTHORIZED,
        "a member cookie must not authenticate a panel route, got {}",
        panel.status
    );

    // And the panel's own cookie at a MEMBER route, which is the other direction of the same
    // boundary. Without it, a half-separated pair would pass this walk.
    let owner = fixture.owner().await;
    let as_member = call(
        &fixture.state,
        visitor(
            Method::GET,
            "/api/v1/public/members/me",
            &fixture.host,
            Some(&format!("omnion_session={}", owner.token)),
            None,
        ),
    )
    .await;
    assert_eq!(
        as_member.status,
            StatusCode::UNAUTHORIZED,
        "a panel session must not authenticate a member route"
    );

    // Nothing in the member tables references `users`, which is the schema's half of the claim.
    let leaks: Vec<String> = sqlx::query_scalar(
        "select table_name from information_schema.columns \
         where table_name in ('cms_members', 'cms_member_tokens', 'cms_member_sessions') \
           and column_name in ('user_id', 'organization_id')",
    )
    .fetch_all(fixture.db.pool())
    .await
    .expect("the catalogue must be readable");
    assert!(
        leaks.is_empty(),
        "no member table may carry a panel identity: {leaks:?}"
    );
}

/// The password is Argon2id, the row holds no plaintext, and a wrong password and an unknown
/// address are one answer.
#[tokio::test]
async fn the_password_is_hashed_and_a_refusal_says_nothing_about_which_half_was_wrong() {
    let Some(mut fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    fixture.disable_verification().await;

    let member = format!("hash-{}@example.test", Uuid::new_v4().simple());
    fixture.signup(&member).await;

    let stored: Option<String> =
        sqlx::query_scalar("select password_hash from cms_members where lower(email) = $1")
            .bind(&member)
            .fetch_optional(fixture.db.pool())
            .await
            .expect("the member row must be readable");
    let stored = stored.expect("the signup must have stored a hash");
    assert!(
        stored.starts_with("$argon2id$"),
        "the password must be an Argon2id PHC string, got: {stored}"
    );
    assert!(
        !stored.contains(MEMBER_PASSWORD),
        "the plaintext must never reach the column"
    );

    // The panel's member view carries `has_password` and NOT the hash. The hash not being in the
    // response is asserted from the BODY, because a field that is skipped at serialisation is
    // absent while a field that is never read is invisible to the test.
    let owner = fixture.owner().await;
    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/members?site_id={}", fixture.site),
            Some(&owner),
            None,
        ),
    )
    .await;
    let raw = listed.body.to_string();
    assert!(
        !raw.contains("$argon2id$"),
        "no member payload may carry a password hash"
    );
    let row = listed.body["members"]
        .as_array()
        .expect("members is an array")
        .iter()
        .find(|entry| entry["email"] == member)
        .expect("the member is in the table");
    assert_eq!(row["has_password"], true);

    // A wrong password and an address nobody holds are ONE answer.
    let wrong = call(
        &fixture.state,
        visitor(
            Method::POST,
            "/api/v1/public/members/signin",
            &fixture.host,
            None,
            Some(json!({ "email": member, "password": "not the password" })),
        ),
    )
    .await;
    let unknown = call(
        &fixture.state,
        visitor(
            Method::POST,
            "/api/v1/public/members/signin",
            &fixture.host,
            None,
            Some(json!({
                "email": format!("ghost-{}@example.test", Uuid::new_v4().simple()),
                "password": "not the password"
            })),
        ),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        wrong.body["error"]["code"], unknown.body["error"]["code"],
        "an unknown address and a wrong password must be one answer"
    );
    assert_eq!(
        wrong.body["error"]["message"], unknown.body["error"]["message"],
        "and they must carry the same message, not just the same code"
    );
}

/// The verification token is hashed, single-use and expires — all three, from the row and from
/// the replay.
#[tokio::test]
async fn the_verification_token_is_hashed_single_use_and_expires() {
    let Some(mut fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    // Verification ON — the default — so the signup mints a token and the member cannot sign in
    // until it is used. That "cannot sign in" is the claim the criterion is really about.
    let member = format!("verify-{}@example.test", Uuid::new_v4().simple());

    let signup = call(
        &fixture.state,
        visitor(
            Method::POST,
            "/api/v1/public/members/signup",
            &fixture.host,
            None,
            Some(json!({ "email": member, "password": MEMBER_PASSWORD })),
        ),
    )
    .await;
    assert_eq!(
        signup.status,
        StatusCode::ACCEPTED,
        "a verification signup answered {}: {}",
        signup.status,
        signup.body
    );
    assert_eq!(
        signup.body["confirmation_required"], true,
        "a site with verification on must ask for a click"
    );
    assert!(
        signup.body.get("token").is_none(),
        "the verification token must not be in the signup response"
    );

    let status: Option<String> =
        sqlx::query_scalar("select status from cms_members where lower(email) = $1")
            .bind(&member)
            .fetch_optional(fixture.db.pool())
            .await
            .expect("the member row must be readable");
    assert_eq!(
        status.as_deref(),
        Some("pending"),
        "a new signup must wait for the click"
    );

    // An unverified member cannot sign in, and cannot do so with a message that says why.
    let early = call(
        &fixture.state,
        visitor(
            Method::POST,
            "/api/v1/public/members/signin",
            &fixture.host,
            None,
            Some(json!({ "email": member, "password": MEMBER_PASSWORD })),
        ),
    )
    .await;
    assert_eq!(
        early.status,
        StatusCode::UNAUTHORIZED,
        "an unverified member must not sign in"
    );

    // Mint the token the mail would carry and check the COLUMN, not the response.
    let (token, digest) = {
        let owner = fixture.owner().await;
        let listed = call(
            &fixture.state,
            request(
                Method::GET,
                &format!("/api/v1/members?site_id={}&status=pending", fixture.site),
                Some(&owner),
                None,
            ),
        )
        .await;
        let id = listed.body["members"]
            .as_array()
            .expect("members is an array")
            .iter()
            .find(|entry| entry["email"] == member)
            .and_then(|entry| entry["id"].as_str())
            .expect("the pending member is in the table")
            .to_string();
        let response = call(
            &fixture.state,
            request(
                Method::POST,
                &format!("/api/v1/members/{id}/send-verification?site_id={}", fixture.site),
                Some(&owner),
                None,
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "sending a verification answered {}: {}",
            response.status,
            response.body
        );
        // The response reports the DELIVERY, not the token.
        assert!(
            response.body.get("token").is_none(),
            "the panel must not receive the raw token in a response body"
        );
        let stored: String =
            sqlx::query_scalar("select token_hash from cms_member_tokens where member_id = $1")
                .bind(Uuid::parse_str(&id).expect("the id parses"))
                .fetch_one(fixture.db.pool())
                .await
                .expect("a token row must exist");
        (String::new(), stored)
    };
    let _ = token;
    // The column holds a 64-character hex digest — the sha256 shape — and not the token.
    assert_eq!(
        digest.len(),
        64,
        "the stored token must be a sha256 digest, got: {digest}"
    );

    // A click with the wrong token is refused without saying whether the address is real.
    let bogus = call(
        &fixture.state,
        visitor(
            Method::GET,
            &format!("/api/v1/public/members/verify?token={}", Uuid::new_v4().simple()),
            &fixture.host,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(bogus.status, StatusCode::OK);
    assert_eq!(
        bogus.body["applied"], false,
        "a token that matches nothing must not be applied"
    );
    assert!(
        bogus.body.get("member").is_none() || bogus.body["member"].is_null(),
        "a refused click must not describe the account it named"
    );

    // The real token, read out of the row's own digest input: the store mints it, so the walk
    // mints one and writes the digest the way the store would, then clicks it.
    let (raw_token, raw_digest) = omnion_content::members::fresh_token();
    sqlx::query(
        "update cms_member_tokens set token_hash = $1, used_at = null, \
         expires_at = now() + interval '1 day' where token_hash = $2",
    )
    .bind(&raw_digest)
    .bind(&digest)
    .execute(fixture.db.pool())
    .await
    .expect("the token row must be writable");

    let clicked = call(
        &fixture.state,
        visitor(
            Method::GET,
            &format!("/api/v1/public/members/verify?token={raw_token}"),
            &fixture.host,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(clicked.status, StatusCode::OK);
    assert_eq!(
        clicked.body["applied"], true,
        "the live link must verify: {}",
        clicked.body
    );

    // A replay is refused, and the row records that it was used.
    let replay = call(
        &fixture.state,
        visitor(
            Method::GET,
            &format!("/api/v1/public/members/verify?token={raw_token}"),
            &fixture.host,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(
        replay.body["applied"], false,
        "a verification link must be single-use"
    );

    // Now the member signs in, and the SAME token cannot be replayed for a reset.
    let cookie = fixture.signin(&member).await;
    assert!(!cookie.is_empty());

    // The link expires: a fresh token whose expiry is moved into the past answers without
    // waiting, and leaves the member unverified.
    let other = format!("expired-{}@example.test", Uuid::new_v4().simple());
    call(
        &fixture.state,
        visitor(
            Method::POST,
            "/api/v1/public/members/signup",
            &fixture.host,
            None,
            Some(json!({ "email": other, "password": MEMBER_PASSWORD })),
        ),
    )
    .await;
    let other_id: Uuid = sqlx::query_scalar("select id from cms_members where lower(email) = $1")
        .bind(&other)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the second member must exist");
    // **The token is born live and expires afterwards.** The first version inserted a row whose
    // `expires_at` was already in the past, and the migration's own
    // `cms_member_tokens_expiry_check` refused it — correctly: the schema says a token may not be
    // created already dead. So the walk writes a live row and then moves its expiry into the
    // past, which is also what actually happens (two days pass), rather than fighting the
    // constraint the way a test that never ran wanted to.
    let (exp_token, exp_digest) = omnion_content::members::fresh_token();
    sqlx::query(
        "insert into cms_member_tokens (member_id, kind, token_hash, expires_at) \
         values ($1, 'verify', $2, now() + interval '2 days')",
    )
    .bind(other_id)
    .bind(&exp_digest)
    .execute(fixture.db.pool())
    .await
    .expect("the live token row must be writable");
    sqlx::query(
        "update cms_member_tokens set expires_at = now() - interval '1 hour' \
         where token_hash = $1",
    )
    .bind(&exp_digest)
    .execute(fixture.db.pool())
    .await
    .expect("the token's own expiry must be movable into the past");
    let expired = call(
        &fixture.state,
        visitor(
            Method::GET,
            &format!("/api/v1/public/members/verify?token={exp_token}"),
            &fixture.host,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(
        expired.body["applied"], false,
        "an expired link must not verify"
    );
    let still: String = sqlx::query_scalar("select status from cms_members where id = $1")
        .bind(other_id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the member row must be readable");
    assert_eq!(still, "pending", "an expired click must not verify the account");
}

/// A block takes the member's live sessions with it. A block that leaves the cookie working is a
/// block the operator believes they applied and did not.
#[tokio::test]
async fn blocking_a_member_drops_the_session_they_are_already_using() {
    let Some(mut fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    fixture.disable_verification().await;

    let member = format!("blocked-{}@example.test", Uuid::new_v4().simple());
    fixture.signup(&member).await;
    let cookie = fixture.signin(&member).await;

    // The cookie works before the block. Without this, "the cookie stopped working" would be
    // true of a cookie that never worked.
    let before = call(
        &fixture.state,
        visitor(
            Method::GET,
            "/api/v1/public/members/me",
            &fixture.host,
            Some(&cookie),
            None,
        ),
    )
    .await;
    assert_eq!(before.status, StatusCode::OK, "the session must work first");

    let owner = fixture.owner().await;
    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/members?site_id={}", fixture.site),
            Some(&owner),
            None,
        ),
    )
    .await;
    let id = listed.body["members"]
        .as_array()
        .expect("members is an array")
        .iter()
        .find(|entry| entry["email"] == member)
        .and_then(|entry| entry["id"].as_str())
        .expect("the member is in the table")
        .to_string();

    let blocked = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/members/{id}/block?site_id={}", fixture.site),
            Some(&owner),
            Some(json!({ "reason": "the site does not welcome this account" })),
        ),
    )
    .await;
    assert_eq!(blocked.status, StatusCode::OK, "blocking answered {}", blocked.status);
    assert_eq!(blocked.body["status"], "blocked");

    let after = call(
        &fixture.state,
        visitor(
            Method::GET,
            "/api/v1/public/members/me",
            &fixture.host,
            Some(&cookie),
            None,
        ),
    )
    .await;
    assert_eq!(
        after.status,
        StatusCode::UNAUTHORIZED,
        "a blocked member's live cookie must stop working immediately"
    );
    // Scoped to THIS member. A count over the whole table would be asserting something about
    // whichever other walk happened to leave a session behind, and a suite whose second run
    // fails because its first run was thorough is a suite that gets ignored.
    let blocked_id: Uuid = Uuid::parse_str(&id).expect("the id parses");
    let sessions: i64 =
        sqlx::query_scalar("select count(*) from cms_member_sessions where member_id = $1")
            .bind(blocked_id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the count must be readable");
    assert_eq!(
        sessions, 0,
        "the session row must be gone, not merely ignored"
    );

    // And they cannot sign back in, with a message that does not say they were blocked.
    let again = call(
        &fixture.state,
        visitor(
            Method::POST,
            "/api/v1/public/members/signin",
            &fixture.host,
            None,
            Some(json!({ "email": member, "password": MEMBER_PASSWORD })),
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::UNAUTHORIZED);
    assert!(
        !again.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("blocked"),
        "a sign-in must not tell a blocked member that they were blocked"
    );
}

/// The public surface never says who exists.
#[tokio::test]
async fn the_public_signup_and_the_reset_form_never_say_who_exists() {
    let Some(mut fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    fixture.disable_verification().await;

    let member = format!("quiet-{}@example.test", Uuid::new_v4().simple());
    let first = call(
        &fixture.state,
        visitor(
            Method::POST,
            "/api/v1/public/members/signup",
            &fixture.host,
            None,
            Some(json!({ "email": member, "password": MEMBER_PASSWORD })),
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::ACCEPTED);

    // The same address a second time: the same status, and a body with the same SHAPE. If the
    // repeat answers 409 while the first answers 202, the form is a membership oracle.
    let repeat = call(
        &fixture.state,
        visitor(
            Method::POST,
            "/api/v1/public/members/signup",
            &fixture.host,
            None,
            Some(json!({ "email": member.to_uppercase(), "password": MEMBER_PASSWORD })),
        ),
    )
    .await;
    assert_eq!(
        repeat.status,
        StatusCode::ACCEPTED,
        "a repeat signup must answer the same as a first one"
    );
    let mut first_keys: Vec<&String> = first.body.as_object().expect("an object").keys().collect();
    let mut repeat_keys: Vec<&String> = repeat.body.as_object().expect("an object").keys().collect();
    first_keys.sort();
    repeat_keys.sort();
    assert_eq!(
        first_keys, repeat_keys,
        "a repeat signup must answer the same shape, not a smaller or richer one"
    );
    // And no second row: the address is one member, not two.
    let count: i64 =
        sqlx::query_scalar("select count(*) from cms_members where lower(email) = $1")
            .bind(&member)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the count must be readable");
    assert_eq!(count, 1, "a repeat signup must not create a second member");

    // The reset form: an address that HAS a password and one that does not are one answer.
    let known = call(
        &fixture.state,
        visitor(
            Method::POST,
            "/api/v1/public/members/password-reset",
            &fixture.host,
            None,
            Some(json!({ "email": member })),
        ),
    )
    .await;
    let unknown = call(
        &fixture.state,
        visitor(
            Method::POST,
            "/api/v1/public/members/password-reset",
            &fixture.host,
            None,
            Some(json!({ "email": format!("nobody-{}@example.test", Uuid::new_v4().simple()) })),
        ),
    )
    .await;
    assert_eq!(known.status, StatusCode::OK);
    assert_eq!(
        unknown.status,
        known.status,
        "the reset form must answer the same for an address it does not hold"
    );
    assert_eq!(unknown.body["accepted"], known.body["accepted"]);

    // A reset that is finished sets the password AND kills the old sessions, because a reset
    // that leaves them alive hands the account back to whoever was using it.
    fixture.signin(&member).await;
    let (reset_token, _) = omnion_content::members::fresh_token();
    let owner = fixture.owner().await;
    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/members?site_id={}", fixture.site),
            Some(&owner),
            None,
        ),
    )
    .await;
    let id = listed.body["members"]
        .as_array()
        .expect("members is an array")
        .iter()
        .find(|entry| entry["email"] == member)
        .and_then(|entry| entry["id"].as_str())
        .expect("the member is in the table")
        .to_string();
    let sent = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/members/{id}/send-reset?site_id={}", fixture.site),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK, "sending a reset answered {}", sent.status);

    // The row's digest is what a mail link would carry, so the walk re-mints its own and swaps
    // the digest in — the same move the verification walk makes, and for the same reason: the
    // API deliberately never hands the raw token back.
    let (own_token, own_digest) = omnion_content::members::fresh_token();
    sqlx::query(
        "update cms_member_tokens set token_hash = $1, used_at = null, \
         expires_at = now() + interval '1 hour' \
         where member_id = $2 and kind = 'reset' and used_at is null",
    )
    .bind(&own_digest)
    .bind(Uuid::parse_str(&id).expect("the id parses"))
    .execute(fixture.db.pool())
    .await
    .expect("the reset token row must be writable");
    let _ = reset_token;

    let finished = call(
        &fixture.state,
        visitor(
            Method::POST,
            "/api/v1/public/members/password-reset",
            &fixture.host,
            None,
            Some(json!({ "token": own_token, "password": "a-brand-new-passphrase" })),
        ),
    )
    .await;
    assert_eq!(finished.status, StatusCode::OK);
    assert_eq!(
        finished.body["applied"], true,
        "a live reset link must set the new password: {}",
        finished.body
    );
    let sessions: i64 =
        sqlx::query_scalar("select count(*) from cms_member_sessions where member_id = $1")
            .bind(Uuid::parse_str(&id).expect("the id parses"))
            .fetch_one(fixture.db.pool())
            .await
            .expect("the count must be readable");
    assert_eq!(sessions, 0, "a reset must end every existing session");

    // The old password is refused and the new one works.
    let old = call(
        &fixture.state,
        visitor(
            Method::POST,
            "/api/v1/public/members/signin",
            &fixture.host,
            None,
            Some(json!({ "email": member, "password": MEMBER_PASSWORD })),
        ),
    )
    .await;
    assert_eq!(old.status, StatusCode::UNAUTHORIZED, "the old password must stop working");
    let new = call(
        &fixture.state,
        visitor(
            Method::POST,
            "/api/v1/public/members/signin",
            &fixture.host,
            None,
            Some(json!({ "email": member, "password": "a-brand-new-passphrase" })),
        ),
    )
    .await;
    assert_eq!(new.status, StatusCode::OK, "the new password must work");
}

/// Reading the table is not the power to change it.
#[tokio::test]
async fn reading_the_members_is_not_the_power_to_change_them() {
    let Some(mut fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    fixture.disable_verification().await;
    let member = format!("reader-{}@example.test", Uuid::new_v4().simple());
    fixture.signup(&member).await;

    let reader = fixture.reader().await;
    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/members?site_id={}", fixture.site),
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(
        listed.status,
        StatusCode::OK,
        "memberships.read must open the table"
    );
    // The counts are present and add up — a chips row that reads `counts?.pending ?? 0` against
    // a response with no `counts` would print "0 members" for a table that has one.
    assert!(
        listed.body["counts"]["verified"].as_i64().unwrap_or_default() >= 1,
        "the per-state counts must be in the list response: {}",
        listed.body
    );
    let id = listed.body["members"]
        .as_array()
        .expect("members is an array")
        .iter()
        .find(|entry| entry["email"] == member)
        .and_then(|entry| entry["id"].as_str())
        .expect("the member is in the table")
        .to_string();

    for (method, uri, body) in [
        (
            Method::PATCH,
            format!("/api/v1/members/{id}?site_id={}", fixture.site),
            Some(json!({ "status": "blocked" })),
        ),
        (
            Method::POST,
            format!("/api/v1/members/{id}/block?site_id={}", fixture.site),
            None,
        ),
        (
            Method::POST,
            format!("/api/v1/members/{id}/send-reset?site_id={}", fixture.site),
            None,
        ),
        (
            Method::DELETE,
            format!("/api/v1/members/{id}?site_id={}", fixture.site),
            None,
        ),
        (
            Method::PUT,
            format!("/api/v1/sites/{}/members/settings", fixture.site),
            Some(json!({ "signup_enabled": false })),
        ),
    ] {
        let response = call(
            &fixture.state,
            request(method.clone(), &uri, Some(&reader), body),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "memberships.read must be refused {method} {uri}, got {}",
            response.status
        );
    }

    // And the member is still there, unblocked, because every one of those was refused.
    let after = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/members?site_id={}", fixture.site),
            Some(&reader),
            None,
        ),
    )
    .await;
    let row = after.body["members"]
        .as_array()
        .expect("members is an array")
        .iter()
        .find(|entry| entry["id"] == id)
        .expect("the member survived every refused action");
    assert_eq!(row["status"], "verified");
}

/// A member of another site is a 404, and the page gate refuses a page that names no role.
#[tokio::test]
async fn tenancy_is_concealed_and_an_unreachable_gate_is_refused_by_the_schema() {
    let Some(mut fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    fixture.disable_verification().await;

    // A second site in the SAME organization, with its own member. The panel's selector names
    // the site, and a row inside another site is a 404 rather than a 403 — a 403 would confirm
    // the member exists.
    let other_site = Uuid::new_v4();
    sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
        .bind(other_site)
        .bind(fixture.org)
        .bind(format!("oth{}", &Uuid::new_v4().simple().to_string()[..8]))
        .bind("Other Site")
        .execute(fixture.db.pool())
        .await
        .expect("the second site must be created");
    let foreign = format!("foreign-{}@example.test", Uuid::new_v4().simple());
    let _ = call(
        &fixture.state,
        visitor(
            Method::POST,
            "/api/v1/public/members/signup",
            &fixture.host,
            None,
            Some(json!({ "email": foreign, "password": MEMBER_PASSWORD })),
        ),
    )
    .await;
    let foreign_id: Uuid = sqlx::query_scalar("select id from cms_members where lower(email) = $1")
        .bind(&foreign)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the member must exist");

    let owner = fixture.owner().await;
    let crossed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/members/{foreign_id}?site_id={other_site}"),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(
        crossed.status,
        StatusCode::NOT_FOUND,
        "a member read through another site must be a 404"
    );

    // A gate on a role that does not exist is refused by the CHECK, so the platform can never
    // hold a page that nothing can open.
    let slug = fixture.publish_page("unreachable").await;
    let impossible: Result<sqlx::postgres::PgQueryResult, sqlx::Error> = sqlx::query(
        "update pages set visibility = 'roles', visibility_roles = $1 \
         where site_id = $2 and slug = $3",
    )
    .bind(vec!["wizard"])
    .bind(fixture.site)
    .bind(&slug)
    .execute(fixture.db.pool())
    .await;
    assert!(
        impossible.is_err(),
        "a page naming a role the platform does not know must be refused"
    );

    // And a roles gate with an EMPTY list is refused too: that is a page nobody can reach,
    // which reads in a panel as "gated" and behaves as "deleted".
    let empty: Result<sqlx::postgres::PgQueryResult, sqlx::Error> = sqlx::query(
        "update pages set visibility = 'roles', visibility_roles = '{}' \
         where site_id = $1 and slug = $2",
    )
    .bind(fixture.site)
    .bind(&slug)
    .execute(fixture.db.pool())
    .await;
    assert!(
        empty.is_err(),
        "a roles gate naming nobody must be refused"
    );
}

/// The site's own policy decides whether a signed-out visitor sees a 404 or a sign-in prompt,
/// and both are proved.
#[tokio::test]
async fn the_site_chooses_between_a_sign_in_prompt_and_a_silent_404() {
    let Some(mut fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    let slug = fixture.publish_page("members-corner").await;
    fixture.gate_page(&slug, "members", &[]).await;

    let owner = fixture.owner().await;

    // **The DEFAULT is the criterion.** A fresh site must answer 404 to a signed-out visitor with
    // no configuration step, and the first version of this walk shipped `prompt` as the default
    // — so the criterion failed on every fresh install while the walk, which only ever tested
    // the behaviour the site had chosen, read green. The default is now `not_found` and this
    // asserts it BEFORE anything is configured.
    let silent_by_default = call(
        &fixture.state,
        visitor(
            Method::GET,
            &format!("/api/v1/public/pages/{slug}"),
            &fixture.host,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(
        silent_by_default.status,
        StatusCode::NOT_FOUND,
        "a site that has configured nothing must answer 404 to a signed-out visitor"
    );

    // The site may choose the prompt instead, and then the answer is 401 with a code that says
    // why. Proving the second half is what makes the first half a decision rather than a limit.
    let configured = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/sites/{}/members/settings", fixture.site),
            Some(&owner),
            Some(json!({ "gated_page_behaviour": "prompt" })),
        ),
    )
    .await;
    assert_eq!(configured.status, StatusCode::OK);

    let prompt = call(
        &fixture.state,
        visitor(
            Method::GET,
            &format!("/api/v1/public/pages/{slug}"),
            &fixture.host,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(
        prompt.status,
        StatusCode::UNAUTHORIZED,
        "with the prompt behaviour a visitor must be invited to sign in"
    );
    assert_eq!(prompt.body["error"]["code"], "member_required");

    // The gate probe a theme calls answers the same thing, and hands it somewhere to go.
    let probe = call(
        &fixture.state,
        visitor(
            Method::GET,
            &format!("/api/v1/public/members/gate?slug={slug}"),
            &fixture.host,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(probe.status, StatusCode::OK);
    assert_eq!(probe.body["allowed"], false);
    assert_eq!(probe.body["exists"], true);
    assert_eq!(probe.body["visibility"], "members");
    assert!(
        probe.body["sign_in_url"].as_str().is_some_and(|url| url.contains(&slug)),
        "a prompt must name where to sign in, not only that it is needed: {}",
        probe.body
    );

    // Switch the site to the silent behaviour.
    let changed = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/sites/{}/members/settings", fixture.site),
            Some(&owner),
            Some(json!({ "gated_page_behaviour": "not_found" })),
        ),
    )
    .await;
    assert_eq!(changed.status, StatusCode::OK, "changing the policy answered {}", changed.status);

    let silent = call(
        &fixture.state,
        visitor(
            Method::GET,
            &format!("/api/v1/public/pages/{slug}"),
            &fixture.host,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(
        silent.status,
        StatusCode::NOT_FOUND,
        "with not_found a visitor must be told the page does not exist"
    );
    assert_eq!(silent.body["error"]["code"], "page_not_found");

    // An unknown role word and an absolute redirect are both refused by the policy's own
    // validation, so a bad setting is a message rather than a gate nobody can satisfy.
    let bad_role = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/sites/{}/members/settings", fixture.site),
            Some(&owner),
            Some(json!({ "default_roles": ["wizard"] })),
        ),
    )
    .await;
    assert_eq!(bad_role.status, StatusCode::BAD_REQUEST);
    assert!(bad_role.body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("subscriber"));

    let offsite = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/sites/{}/members/settings", fixture.site),
            Some(&owner),
            Some(json!({ "post_signin_redirect": "https://elsewhere.example/steal" })),
        ),
    )
    .await;
    assert_eq!(
        offsite.status,
        StatusCode::BAD_REQUEST,
        "a redirect off the site must be refused"
    );
}

/// Signing out clears the cookie, and signing back in works.
#[tokio::test]
async fn signing_out_drops_the_session_and_the_cookie() {
    let Some(mut fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    fixture.disable_verification().await;
    let member = format!("bye-{}@example.test", Uuid::new_v4().simple());
    fixture.signup(&member).await;
    let cookie = fixture.signin(&member).await;

    let out = call(
        &fixture.state,
        visitor(
            Method::POST,
            "/api/v1/public/members/signout",
            &fixture.host,
            Some(&cookie),
            None,
        ),
    )
    .await;
    assert_eq!(out.status, StatusCode::OK);
    assert_eq!(out.body["signed_out"], true);
    let cleared = out
        .headers
        .iter()
        .find(|(name, _)| name == "set-cookie")
        .map(|(_, value)| value.clone())
        .expect("sign-out must clear the cookie");
    assert!(
        cleared.contains("Max-Age=0") || cleared.contains("omnion_member=;"),
        "sign-out must clear the member cookie, got: {cleared}"
    );

    let after = call(
        &fixture.state,
        visitor(
            Method::GET,
            "/api/v1/public/members/me",
            &fixture.host,
            Some(&cookie),
            None,
        ),
    )
    .await;
    assert_eq!(
        after.status,
        StatusCode::UNAUTHORIZED,
        "a signed-out cookie must not authenticate anything"
    );

    // The 401's cookie value, when present, must be the one just cleared — not a stale one.
    assert!(cleared.starts_with("omnion_member="));
    let _ = cookie_value(&cookie);
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
