//! The attribute map on a **live** sign-in (REQ-065, slice 2).
//!
//! `sso_live.rs` proves a provider can sign a person in; `iam_attribute_map.rs` proves the map can
//! be stored, replaced and rehearsed. Neither proves the thing that matters, which is that the
//! map is actually *consulted* at the moment of sign-in — a preview an operator trusted, and a
//! callback that disagrees with it, is the failure mode this file exists to rule out.
//!
//! So this walk drives the real router against the real [`StubIdp`], over real cryptography, and
//! asserts three things that cannot be true unless `finish_sign_in` reads the stored map:
//!
//! * a map whose email row points at a claim the provider *does* send produces that address —
//!   so the map decided the identity, not the `email` claim the reduction would have found;
//! * the same provider with the map pointed at a claim it does **not** send is **refused**, and
//!   the refusal names the field. An account is not created, so the panel never has a half person
//!   to clean up;
//! * a provider with **no** map at all still signs in, because "not configured yet" must not mean
//!   "nobody can sign in" — the local-sign-in invariant in its provider-shaped form.
//!
//! The first two need the same provider to behave two ways, so the map is rewritten *between*
//! sign-ins rather than set up twice: the state left behind by each step is asserted directly
//! against the database, because a walk that only checks the HTTP answer cannot tell a real
//! refusal from a redirect that went somewhere else.

mod support;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::seed;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use support::stub_idp::{CLIENT_ID, CLIENT_SECRET, StubIdp};

/// The same account the other live walk uses, in a different organization, so the two files
/// cannot interfere with each other when they run in the same `cargo test` invocation.
const HOST: &str = "sso-map.omnion.test";
const PASSWORD: &str = "correct horse battery";
const SECRET_REF: &str = "OMNION_SSO_MAP_STUB_SECRET";

/// The address the mapped sign-in must produce. It is deliberately **not** the claim the stub
/// provider puts in `email`, so a sign-in that ignored the map could not produce it.
///
/// It carries the `sso-map-` prefix like everything else this walk creates. A JIT account is
/// written by the *sign-in path*, not the fixture, so without the prefix a crashed run would
/// leave an organization-less account behind — and `find_by_email` would then return it to the
/// next run's lookup, where the organization filter rejects it and the walk fails on a
/// `provisioning_refused` that has nothing to do with what it is testing.
const MAPPED_EMAIL: &str = "sso-map-mapped-person@omnion.test";
/// The claim address the unmapped sign-in produces, and which a mapped sign-in must NOT use.
const CLAIM_EMAIL: &str = "sso-map-claim-address@omnion.test";

struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    location: Option<String>,
    body: Value,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("the router must answer");
    let status = response.status();
    let set_cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("the body must read")
        .to_bytes();
    TestResponse {
        status,
        set_cookie,
        location,
        body: if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        },
    }
}

fn session_request(method: Method, uri: &str, session: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(credential) = session {
        builder = if credential.starts_with("omnion_session=") {
            builder.header(header::COOKIE, credential)
        } else {
            builder.header(header::AUTHORIZATION, format!("Bearer {credential}"))
        };
    }
    match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .expect("the request must build"),
        None => builder.body(Body::empty()).expect("the request must build"),
    }
}

/// The public half: a sign-in carries no session, only the organization it is aimed at.
fn public_request(method: Method, uri: &str, host: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, host)
        .body(Body::empty())
        .expect("the request must build")
}

fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).unwrap_or_default()
}

/// Drive the provider's own authorization endpoint and take the path it redirects the browser to.
///
/// `redirect::Policy::none` on purpose: the provider names the panel's *public* URL, which no test
/// process is listening on, so following it here would be a network call to nobody. Reading the
/// redirect and issuing the callback against our own router is the browser step.
async fn authorization_target(url: &str) -> String {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("a client");
    let response = client
        .get(url)
        .send()
        .await
        .expect("the stub must answer its own authorization endpoint");
    assert_eq!(
        response.status().as_u16(),
        302,
        "a provider's authorization endpoint redirects the browser: {url}"
    );
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .expect("a redirect carries a Location")
        .to_owned();
    location
        .split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|index| &rest[index..]))
        .unwrap_or_else(|| panic!("the provider did not redirect to an absolute URL: {location}"))
        .to_owned()
}

fn query_value(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then(|| percent_decode(value))
    })
}

struct Fixture {
    state: AppState,
    db: Db,
    organization_id: Uuid,
    host: String,
    /// The owner's address carries the cleanup prefix, so it cannot be hard-coded here.
    owner_email: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let config = Config::from_env().expect("the environment must be valid");
        let db = match Db::connect(&config.database).await {
            Ok(db) => db,
            Err(error) => {
                eprintln!(
                    "SKIP: PostgreSQL is not reachable ({error}) — the mapped sign-in walk needs a \
                     database with every migration applied"
                );
                return None;
            }
        };
        db.migrate().await.expect("migrations must apply");
        let redis = RedisClient::new(&config.redis.url).expect("the redis URL must parse");
        let state = AppState::new(
            BuildInfo::new("omnion-api", "0.0.0-test"),
            config,
            db.clone(),
            redis,
            test_storage(),
        );
        seed::ensure(db.pool()).await.expect("the IAM seed must run");

        // Everything this walk owns is marked with a prefix and cleared only by that prefix, so
        // a suite sharing the database is never touched. The addresses are fixed so a run that
        // died before cleanup leaves an account the *next* run finds instead of provisioning —
        // which would make the walk test the wrong thing while reporting success.
        const OWNED: &str = "sso-map-";
        for statement in [
            "delete from sessions where user_id in \
             (select id from users where email like $1)",
            "delete from role_bindings where user_id in \
             (select id from users where email like $1)",
            "delete from auth_provider_events where organization_id in \
             (select id from organizations where slug like $1)",
            "delete from sso_challenges where organization_id in \
             (select id from organizations where slug like $1)",
            "delete from provider_attribute_mappings where provider_id in \
             (select id from auth_providers where organization_id in \
              (select id from organizations where slug like $1))",
            "delete from auth_providers where organization_id in \
             (select id from organizations where slug like $1)",
            "delete from users where email like $1",
        ] {
            sqlx::query(statement)
                .bind(format!("{OWNED}%"))
                .execute(db.pool())
                .await
                .expect("a leftover row from a previous run must be clearable");
        }
        sqlx::query(
            "delete from site_domains where site_id in \
             (select id from sites where organization_id in \
              (select id from organizations where slug like $1))",
        )
        .bind(format!("{OWNED}%"))
        .execute(db.pool())
        .await
        .expect("a leftover domain from a previous run must be clearable");
        sqlx::query(
            "delete from sites where organization_id in \
             (select id from organizations where slug like $1)",
        )
        .bind(format!("{OWNED}%"))
        .execute(db.pool())
        .await
        .expect("a leftover site from a previous run must be clearable");
        sqlx::query("delete from organizations where slug like $1")
            .bind(format!("{OWNED}%"))
            .execute(db.pool())
            .await
            .expect("a leftover organization from a previous run must be clearable");

        let organization_id: Uuid = sqlx::query_scalar(
            "insert into organizations (name, slug) values ($1, $2) returning id",
        )
        .bind("SSO Attribute Map Test Organization")
        .bind(format!("{}{}", OWNED, Uuid::new_v4().simple()))
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created");

        // The owner address carries the prefix too, so cleanup reaches it.
        let owner = users::create_user(
            db.pool(),
            NewUser {
                email: format!("{}owner-{}@omnion.test", OWNED, Uuid::new_v4().simple()),
                password: PASSWORD.to_owned(),
                display_name: "SSO Map Owner".to_owned(),
                organization_id: Some(organization_id),
            },
        )
        .await
        .expect("the owner must be created");
        seed::bind_owner(db.pool(), owner.id)
            .await
            .expect("the owner binding must be created");

        // The public surface resolves its organization from the host, so a multi-organization
        // database needs a registered domain — and a stale one from a crashed run is cleared
        // first, so a re-run can recover.
        let site_id: Uuid = sqlx::query_scalar(
            "insert into sites (organization_id, key, name) values ($1, $2, $3) returning id",
        )
        .bind(organization_id)
        .bind(format!("sso-map-site-{}", Uuid::new_v4().simple()))
        .bind("SSO Map Test Site")
        .fetch_one(db.pool())
        .await
        .expect("the test site must be created");
        sqlx::query("delete from site_domains where host = $1")
            .bind(HOST)
            .execute(db.pool())
            .await
            .expect("the stale domain must be cleared");
        sqlx::query("insert into site_domains (site_id, host, is_primary) values ($1, $2, true)")
            .bind(site_id)
            .bind(HOST)
            .execute(db.pool())
            .await
            .expect("the test domain must be created");

        Some(Self {
            state,
            db,
            organization_id,
            host: HOST.to_owned(),
            owner_email: owner.email.clone(),
        })
    }

    async fn owner_session(&self) -> String {
        let response = call(
            &self.state,
            session_request(
                Method::POST,
                "/api/v1/auth/login",
                None,
                Some(json!({ "email": self.owner_email, "password": PASSWORD })),
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "login: {}", response.body);
        format!(
            "omnion_session={}",
            response
                .set_cookie
                .as_deref()
                .and_then(|cookie| cookie.split(';').next())
                .and_then(|pair| pair.split_once('=').map(|(_, value)| value))
                .expect("login sets the session cookie")
        )
    }

    /// Every account this organization holds whose address came from the provider.
    async fn provisioned_addresses(&self) -> Vec<String> {
        sqlx::query_scalar(
            "select email from users where organization_id = $1 order by created_at",
        )
        .bind(self.organization_id)
        .fetch_all(self.db.pool())
        .await
        .expect("the address list must read")
    }

    /// Remove everything carrying this walk's prefix. Scoped like the setup, so a suite sharing
    /// the database keeps its own fixtures even if this walk fails halfway.
    async fn cleanup(&self) {
        const OWNED: &str = "sso-map-";
        for statement in [
            "delete from sessions where user_id in \
             (select id from users where email like $1)",
            "delete from role_bindings where user_id in \
             (select id from users where email like $1)",
            "delete from auth_provider_events where organization_id in \
             (select id from organizations where slug like $1)",
            "delete from sso_challenges where organization_id in \
             (select id from organizations where slug like $1)",
            "delete from provider_attribute_mappings where provider_id in \
             (select id from auth_providers where organization_id in \
              (select id from organizations where slug like $1))",
            "delete from auth_providers where organization_id in \
             (select id from organizations where slug like $1)",
            "delete from users where email like $1",
            "delete from site_domains where site_id in \
             (select id from sites where organization_id in \
              (select id from organizations where slug like $1))",
            "delete from sites where organization_id in \
             (select id from organizations where slug like $1)",
            "delete from organizations where slug like $1",
        ] {
            sqlx::query(statement)
                .bind(format!("{OWNED}%"))
                .execute(self.db.pool())
                .await
                .expect("cleanup must run");
        }
    }
}

/// Connect a provider to the stub and switch it on, as an administrator would.
///
/// The enablement gate lives inside the generic PATCH rather than a separate verb, so this call
/// is exactly the one the panel makes — and a provider whose test has never passed is refused
/// here, which is the whole reason the gate is there.
async fn connect(fixture: &Fixture, cookie: &str, idp: &StubIdp) -> Uuid {
    let response = call(
        &fixture.state,
        session_request(
            Method::POST,
            "/api/v1/iam/providers",
            Some(cookie),
            Some(json!({
                "slug": "stub-mapped",
                "kind": "oidc",
                "name": "Mapped Stub Directory",
                "config": { "issuer": idp.issuer(), "client_id": CLIENT_ID },
                "secret_ref": SECRET_REF,
                "jit_enabled": true,
            })),
        ),
    )
    .await;
    assert_eq!(
        response.status, StatusCode::CREATED,
        "connect: {}", response.body
    );
    let id = response.body["id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
        .expect("the created provider has an id");

    // The gate, asserted in its own right: an untested provider cannot be switched on, and the
    // refusal says why in a sentence an operator can act on. A screen that only said "no" after
    // the click is a screen people click twice.
    let early = call(
        &fixture.state,
        session_request(
            Method::PATCH,
            &format!("/api/v1/iam/providers/{id}"),
            Some(cookie),
            Some(json!({ "enabled": true })),
        ),
    )
    .await;
    assert_eq!(
        early.status, StatusCode::BAD_REQUEST,
        "a provider that has never passed a test stays off: {}", early.body
    );
    assert_eq!(early.body["error"]["code"], json!("provider_not_ready"));

    // Now the real test, against the stub's discovery document over a real socket.
    let tested = call(
        &fixture.state,
        session_request(
            Method::POST,
            &format!("/api/v1/iam/providers/{id}/test"),
            Some(cookie),
            None,
        ),
    )
    .await;
    assert_eq!(tested.status, StatusCode::OK, "test: {}", tested.body);
    assert_eq!(
        tested.body["status"],
        json!("ok"),
        "discovery answered and the JWKS is readable: {}", tested.body
    );

    let published = call(
        &fixture.state,
        session_request(
            Method::PATCH,
            &format!("/api/v1/iam/providers/{id}"),
            Some(cookie),
            Some(json!({ "enabled": true })),
        ),
    )
    .await;
    assert_eq!(
        published.status, StatusCode::OK,
        "a tested provider may be switched on: {}", published.body
    );
    id
}

/// Run the whole browser half of the flow against the stub provider.
///
/// `subject` and `claim_email` are what the provider will *assert*, passed in so each step can
/// have the provider say something different while the platform's configuration is what changes.
async fn sign_in(
    fixture: &Fixture,
    idp: &StubIdp,
    subject: &str,
    claim_email: &str,
    claim_name: &str,
) -> TestResponse {
    let start = call(
        &fixture.state,
        public_request(
            Method::GET,
            "/api/v1/auth/sso/stub-mapped/start?return_to=/analytics",
            &fixture.host,
        ),
    )
    .await;
    let url = percent_decode(&start.location.expect("start answers with a redirect"));
    assert!(
        url.starts_with(&idp.authorization_endpoint()),
        "the browser is sent to the provider's own endpoint: {url}"
    );
    let challenge = query_value(&url, "code_challenge").expect("PKCE must ride the URL");
    idp.expect_identity(subject, claim_email, claim_name, &["editors"], &challenge);
    let callback_path = authorization_target(&url).await;
    call(
        &fixture.state,
        public_request(Method::GET, &callback_path, &fixture.host),
    )
    .await
}

#[tokio::test]
async fn the_attribute_map_decides_who_a_live_sign_in_becomes() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let cookie = fixture.owner_session().await;
    unsafe { std::env::set_var(SECRET_REF, CLIENT_SECRET) };

    let idp = StubIdp::start().await;
    let provider_id = connect(&fixture, &cookie, &idp).await;

    // ---- 1. No map yet: the provider signs in exactly as it did before the map existed --------
    // "Not configured yet" must not mean "nobody can sign in". This is the local-sign-in
    // invariant in its provider-shaped form, and it is the case a strict implementation breaks.
    let before = sign_in(
        &fixture,
        &idp,
        "stub-mapped-1",
        CLAIM_EMAIL,
        "Unmapped Person",
    )
    .await;
    assert!(
        before.status.is_success(),
        "a provider with no map still signs a person in: {}",
        before.body
    );
    let unmapped_email = before.body["user"]["email"]
        .as_str()
        .expect("a session carries its user")
        .to_owned();
    assert_eq!(
        unmapped_email, CLAIM_EMAIL,
        "with no map the claim reduction decides, exactly as it always did"
    );

    // ---- 2. A map pointing at a claim the provider sends decides the identity ----------------
    let mapped = call(
        &fixture.state,
        session_request(
            Method::PUT,
            &format!("/api/v1/iam/providers/{provider_id}/attribute-mappings"),
            Some(&cookie),
            Some(json!({ "mappings": [
                // The stub sends `sub`, `email` and `name`, and `name` is the human name — so
                // pointing the email row at it is the interesting case: the claim reduction would
                // have used `email` and found something else entirely. `upn` below is the
                // Azure-shaped claim the stub deliberately does *not* send, which is what makes
                // the refusal in step 3 correct rather than accidental.
                { "source_attr": "name", "target_field": "email", "transform": "lowercase", "required": true },
                { "source_attr": "email", "target_field": "display_name", "transform": "none" }
            ]})),
        ),
    )
    .await;
    assert_eq!(mapped.status, StatusCode::OK, "the map must be saved: {}", mapped.body);

    // The same provider, a *different* person: the mapped address can only appear if the
    // callback read the map, because the stub asserts the claim address and the map says
    // `name` is the email.
    let second = sign_in(&fixture, &idp, "stub-mapped-2", "sso-map-second-claim@omnion.test", MAPPED_EMAIL).await;
    assert!(
        second.status.is_success(),
        "a mapped provider signs the person in: {}",
        second.body
    );
    assert_eq!(
        second.body["user"]["email"],
        json!(MAPPED_EMAIL),
        "the email is the one the MAP produced, not the one the claim carried"
    );
    // And the address really is on the account now, not just in the response body.
    let addresses = fixture.provisioned_addresses().await;
    assert!(
        addresses.iter().any(|address| address == MAPPED_EMAIL),
        "the mapped address was written: {addresses:?}"
    );
    assert!(
        !addresses
            .iter()
            .any(|address| address == "sso-map-second-claim@omnion.test"),
        "and the claim address was NOT used as the account key: {addresses:?}"
    );

    // ---- 3. A map pointed at a claim the provider does not send refuses, by name -------------
    let broken = call(
        &fixture.state,
        session_request(
            Method::PUT,
            &format!("/api/v1/iam/providers/{provider_id}/attribute-mappings"),
            Some(&cookie),
            Some(json!({ "mappings": [
                { "source_attr": "upn", "target_field": "email", "transform": "lowercase", "required": true }
            ]})),
        ),
    )
    .await;
    assert_eq!(broken.status, StatusCode::OK, "{}", broken.body);

    let accounts_before = fixture.provisioned_addresses().await;
    let refused = sign_in(
        &fixture,
        &idp,
        "stub-mapped-3",
        "sso-map-third-claim@omnion.test",
        "Third Person",
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "a payload with no mapped email is refused, not half-provisioned: {}",
        refused.body
    );
    assert_eq!(
        refused.body["error"]["code"],
        json!("attributes_incomplete"),
        "the refusal has its own code, so a support conversation can tell it from a wrong password"
    );
    let message = refused.body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        message.contains("email"),
        "and it names the field the operator has to fix: {message}"
    );
    assert_eq!(
        fixture.provisioned_addresses().await,
        accounts_before,
        "a refused sign-in creates nobody — the difference between this and a half account is \
         the whole point"
    );

    // ---- 4. And the refusal was recorded, because a refusal is a fact -------------------------
    let refusals: i64 = sqlx::query_scalar(
        "select count(*) from auth_provider_events \
         where provider_id = $1 and outcome = 'refused'",
    )
    .bind(provider_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the refusal count must read");
    assert!(refusals >= 1, "the refused sign-in left a row: {refusals}");

    // ---- 5. Clearing the map puts the provider back exactly where it started ----------------
    let cleared = call(
        &fixture.state,
        session_request(
            Method::PUT,
            &format!("/api/v1/iam/providers/{provider_id}/attribute-mappings"),
            Some(&cookie),
            Some(json!({ "mappings": [] })),
        ),
    )
    .await;
    assert_eq!(
        cleared.status, StatusCode::OK,
        "clearing a map is a real action, not a refusal: {}",
        cleared.body
    );

    unsafe { std::env::remove_var(SECRET_REF) };
    fixture.cleanup().await;
}
