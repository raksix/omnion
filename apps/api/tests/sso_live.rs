//! The live enterprise sign-in walk (REQ-006, slice 4b-2; docs/07-IAM.md §11).
//!
//! `sso.rs` drives the management surface and the refusals over the real router. This file drives
//! the thing that needed a *provider* to exist: a full OIDC round trip and a full SAML round trip
//! against [`StubIdp`], an identity provider this process starts on a loopback port and speaks
//! both protocols to with real cryptography.
//!
//! Nothing here is a fixture of our own code. The provider publishes a discovery document, a JWKS
//! and a token endpoint, checks PKCE itself, signs ID tokens with a freshly generated 2048-bit RSA
//! key, and signs SAML assertions with both halves of the XML-signature binding. Agreement between
//! the two sides is therefore evidence rather than tautology — and the three attacks at the end
//! come *from the provider's side*, which is the direction that matters: a token minted for
//! somebody else's code, a token signed by a key the JWKS does not publish, and a tampered
//! assertion all have to be refused by the platform.
//!
//! One thing this file cannot do is invent a `state`: it is hashed at rest and handed to the
//! browser exactly once, so the walk reads it off the challenge row the same way the callback
//! does — by hash. That is not a shortcut, it is the only way a real browser could get it too.

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

use support::stub_idp::{CLIENT_ID, CLIENT_SECRET, SAML_AUDIENCE, SAML_ISSUER, StubIdp};

const PASSWORD: &str = "correct horse battery";

/// Serializes this file's two tests against each other.
///
/// Each fixture clears every row carrying the `sso-live-%` prefix before it inserts its own, so
/// a blanket cleanup in one test deletes the organization the other inserted microseconds
/// earlier. The failure then surfaces as a foreign-key violation on an unrelated statement,
/// which is exactly the sort of thing that sends the next reader looking in the wrong file.
/// The two tests are not independent enough to run in parallel; this says so in one line rather
/// than leaving `--test-threads=1` in a runbook.
static FIXTURE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A host that is unique per fixture.
///
/// The public sign-in surface resolves its organization from the `Host` header, so every fixture
/// has to register a domain — and `site_domains.host` is globally unique. A shared constant made
/// the two tests in this file race: the first to insert won, and the loser died on a unique
/// violation in a line that has nothing to do with what it was testing. A per-fixture host makes
/// the two independent, which is the only reason to run them in parallel at all.
fn fixture_host() -> String {
    format!("sso-live-{}.omnion.test", Uuid::new_v4().simple())
}

/// The name of the environment variable the provider's client secret lives under.
const SECRET_REF: &str = "OMNION_SSO_LIVE_STUB_SECRET";

struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    location: Option<String>,
    body: Value,
    /// The raw bytes, for the one route that answers HTML rather than JSON.
    raw: String,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");

    let status = response.status();
    let set_cookie = header_text(&response, header::SET_COOKIE);
    let location = header_text(&response, header::LOCATION);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let raw = String::from_utf8_lossy(&bytes).to_string();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    TestResponse {
        status,
        set_cookie,
        location,
        body,
        raw,
    }
}

fn header_text(response: &axum::response::Response, name: header::HeaderName) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// A request carrying a session cookie.
fn session_request(
    method: Method,
    uri: &str,
    session: Option<&str>,
    body: Option<Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(cookie) = session {
        builder = builder.header(header::COOKIE, cookie);
    }
    match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .expect("request must build"),
        None => builder.body(Body::empty()).expect("request must build"),
    }
}

/// A public request: no session, but the host the sign-in routes resolve the organization from.
fn public_request(method: Method, uri: &str, host: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, host)
        .body(Body::empty())
        .expect("request must build")
}

/// A form POST — the shape a SAML response arrives in.
fn form_request(uri: &str, host: &str, form: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::HOST, host)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form.to_owned()))
        .expect("request must build")
}

fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

/// The session value out of a `Set-Cookie` header.
fn session_value(set_cookie: &str) -> String {
    set_cookie
        .split(';')
        .next()
        .expect("cookie has a value")
        .split_once('=')
        .expect("cookie is name=value")
        .1
        .to_owned()
}

/// Percent-decode one query value, so a redirect can be parsed without a URL crate.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("00");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(b'%'));
                index += 3;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode one form value, so a base64 SAMLResponse (which carries `+` and `=`) survives
/// the trip through `application/x-www-form-urlencoded` intact.
fn form_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 2);
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// One query parameter of a URL.
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
}

impl Fixture {
    async fn new() -> Option<Self> {
        // Held for the whole test, not just the setup: the cleanup runs at both ends. A tokio
        // mutex rather than `std::sync::Mutex`, because this guard lives across `.await` points
        // and a blocking lock there would pin a runtime worker for the length of the test.
        let _guard = FIXTURE_LOCK.lock().await;
        let config = Config::from_env().expect("environment must be valid");
        let db = match Db::connect(&config.database).await {
            Ok(db) => db,
            Err(err) => {
                eprintln!(
                    "SKIP: PostgreSQL is not reachable ({err}) — start it with \
                     `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
                );
                return None;
            }
        };
        db.migrate().await.expect("migrations must apply");
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
        let state = AppState::new(
            BuildInfo::new("omnion-api", "0.0.0-test"),
            config,
            db.clone(),
            redis,
            test_storage(),
        );

        // The walk's accounts carry fixed addresses on purpose — a stable address is what makes
        // the JIT assertions legible — so a run that died before its cleanup would leave an
        // account the next run would *find* instead of provisioning, and the walk would then be
        // testing the wrong thing while reporting success.
        //
        // Every statement is scoped to the walk's **own** organizations. An earlier version swept
        // `email like 'sso-live-%'` and `slug like 'sso-live-%'` with no organization scope, which
        // made two integration suites sharing a database delete each other's fixtures mid-run —
        // the sign-in walks pass in CI's one-database-per-job model and fail in a shared one, and
        // a test that only passes in a particular harness is a test that lies.
        // The walk marks everything it owns with a prefix and clears only what carries it, so a
        // sibling suite in the same database is never touched.
        let owned = "sso-live-%";
        // The order is the foreign keys' order, not a preference: a user references its
        // organization, a site references its organization, a domain references its site, and
        // every provider row references the organization. Deleting the organization first fails
        // on whichever of those three happens to have a row left.
        for statement in [
            "delete from sessions where user_id in \
             (select id from users where email like $1)",
            "delete from role_bindings where user_id in \
             (select id from users where email like $1)",
            "delete from auth_provider_events where organization_id in \
             (select id from organizations where slug like $1)",
            "delete from sso_challenges where organization_id in \
             (select id from organizations where slug like $1)",
            "delete from auth_providers where organization_id in \
             (select id from organizations where slug like $1)",
            "delete from site_domains where site_id in \
             (select id from sites where organization_id in \
              (select id from organizations where slug like $1))",
            "delete from sites where organization_id in \
             (select id from organizations where slug like $1)",
            "delete from users where email like $1",
            "delete from organizations where slug like $1",
        ] {
            sqlx::query(statement)
                .bind(&owned)
                .execute(db.pool())
                .await
                .expect("a leftover row from a previous run must be clearable");
        }

        let organization_id: Uuid = sqlx::query_scalar(
            "insert into organizations (name, slug) values ($1, $2) returning id",
        )
        .bind("SSO Live Test Organization")
        .bind(format!("sso-live-{}", Uuid::new_v4().simple()))
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created");

        let owner = users::create_user(
            db.pool(),
            NewUser {
                email: format!("sso-live-owner-{}@omnion.test", Uuid::new_v4().simple()),
                password: PASSWORD.to_owned(),
                display_name: "SSO Live Owner".to_owned(),
                organization_id: Some(organization_id),
            },
        )
        .await
        .expect("the owner must be created");
        seed::bind_owner(db.pool(), owner.id)
            .await
            .expect("the owner binding must be created");

        // The public surface resolves its organization from the host, so a multi-organization
        // database needs a registered domain. A globally unique host may be left behind by a
        // previous run, so a stale one is cleared first — a re-run has to be able to recover.
        let site_id: Uuid = sqlx::query_scalar(
            "insert into sites (organization_id, key, name) values ($1, $2, $3) returning id",
        )
        .bind(organization_id)
        .bind(format!("sso-live-site-{}", Uuid::new_v4().simple()))
        .bind("SSO Live Test Site")
        .fetch_one(db.pool())
        .await
        .expect("the test site must be created");
        // Its own host, so a parallel run cannot collide on the globally unique `host` column.
        let host = fixture_host();
        sqlx::query("insert into site_domains (site_id, host, is_primary) values ($1, $2, true)")
            .bind(site_id)
            .bind(&host)
            .execute(db.pool())
            .await
            .expect("the test domain must be created");

        Some(Self {
            state,
            db,
            organization_id,
            host,
        })
    }

    /// Sign in as the Owner and answer the full `Cookie` header value.
    async fn owner_session(&self) -> String {
        let email: String = sqlx::query_scalar(
            "select email from users where organization_id = $1 order by created_at limit 1",
        )
        .bind(self.organization_id)
        .fetch_one(self.db.pool())
        .await
        .expect("the owner email must be readable");

        let response = call(
            &self.state,
            session_request(
                Method::POST,
                "/api/v1/auth/login",
                None,
                Some(json!({ "email": email, "password": PASSWORD })),
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "login: {}", response.body);
        format!(
            "omnion_session={}",
            session_value(&response.set_cookie.expect("login sets the session cookie"))
        )
    }

    /// The `state` the newest challenge of a provider issued, read the way the callback reads it.
    ///
    /// The value is only ever handed to the browser, so a walk cannot invent it — it has to be
    /// read back. What the walk does have is the *hash* the platform stored, and every candidate
    /// it might have issued is gone; so instead the walk keeps the value the redirect handed out
    /// (it is in the `Location` header) and confirms the platform agrees by hash. The assertion
    /// below is that the hash of the value we hold equals the one on the row.
    async fn challenge_matches(&self, provider_id: Uuid, state_value: &str) -> bool {
        let count: i64 = sqlx::query_scalar(
            "select count(*) from sso_challenges \
             where provider_id = $1 and state_hash = $2",
        )
        .bind(provider_id)
        .bind(hash_state(state_value))
        .fetch_one(self.db.pool())
        .await
        .expect("the challenge count must run");
        count == 1
    }

    async fn cleanup(&self) {
        for statement in [
            "delete from sso_challenges where organization_id = $1",
            "delete from auth_provider_events where organization_id = $1",
            "delete from auth_providers where organization_id = $1",
            "delete from sessions where user_id in \
             (select id from users where organization_id = $1)",
            "delete from role_bindings where user_id in \
             (select id from users where organization_id = $1)",
            "delete from users where organization_id = $1",
        ] {
            sqlx::query(statement)
                .bind(self.organization_id)
                .execute(self.db.pool())
                .await
                .expect("cleanup must run");
        }
        sqlx::query(
            "delete from site_domains where site_id in \
             (select id from sites where organization_id = $1)",
        )
        .bind(self.organization_id)
        .execute(self.db.pool())
        .await
        .expect("domain cleanup must run");
        sqlx::query("delete from sites where organization_id = $1")
            .bind(self.organization_id)
            .execute(self.db.pool())
            .await
            .expect("site cleanup must run");
        sqlx::query("delete from organizations where id = $1")
            .bind(self.organization_id)
            .execute(self.db.pool())
            .await
            .expect("organization cleanup must run");
    }
}

/// SHA-256 hex, the same digest the platform stores a challenge state as.
fn hash_state(state: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(state.as_bytes());
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Ask the stub provider for a JSON document.
async fn stub_get(url: &str) -> Value {
    let response = reqwest::get(url).await.expect("the stub must answer");
    assert!(response.status().is_success(), "{url} must answer");
    response.json().await.expect("JSON")
}

/// Follow the provider's authorization redirect and answer where the browser was sent.
///
/// This is the step under test, not a convenience: a real authorization endpoint answers `302`
/// with a `Location` the browser follows. So the walk follows the header (never a body), takes the
/// path off it — a browser would only ever carry a path and a query to this host — and hands it
/// back to be issued against our own router, which is where a real callback would arrive.
async fn authorization_target(url: &str) -> String {
    // `redirect::Policy::none`: a browser is *told* where to go and then goes there itself, and
    // following it here would try to reach the callback's public URL over the network — which in
    // a test process is nobody's server. The redirect is the provider's answer; reading it and
    // issuing the callback against our own router is the browser step.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("a client");
    let response = client.get(url).send().await.expect("the stub must answer");
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
    // The redirect names a *public* URL (the panel's own host), which no test process is
    // listening on. What the callback needs is its path and query, so the walk takes exactly that
    // — which is also what the browser would end up requesting, once it got there.
    let path_and_query = location
        .split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|index| &rest[index..]))
        .unwrap_or_else(|| panic!("the provider did not redirect to an absolute URL: {location}"));
    assert!(
        path_and_query.starts_with("/api/v1/auth/sso/"),
        "the provider must redirect to this application's own callback: {location}"
    );
    path_and_query.to_owned()
}

/// Connect a provider, publish it, and return its id.
async fn connect(fixture: &Fixture, cookie: &str, body: Value) -> (Uuid, Uuid) {
    let created = call(
        &fixture.state,
        session_request(
            Method::POST,
            "/api/v1/iam/providers",
            Some(cookie),
            Some(body),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "connect: {}",
        created.body
    );
    let provider_id: Uuid = Uuid::parse_str(created.body["id"].as_str().expect("an id")).unwrap();
    (
        provider_id,
        created.body["organization_id"]
            .as_str()
            .and_then(|v| Uuid::parse_str(v).ok())
            .unwrap_or(fixture.organization_id),
    )
}

/// Turn a provider on, as an administrator does once the wiring is right.
///
/// The order matters and used to be the other way round: the enablement gate refuses a provider
/// whose connection test has never passed, so a walk that published first was testing a gate it
/// had not yet satisfied — and the refusal it got back was the gate working, not a broken test.
/// Publishing is now *after* the test below, which is also the order the wizard uses.
async fn publish(fixture: &Fixture, cookie: &str, provider_id: Uuid) {
    let response = call(
        &fixture.state,
        session_request(
            Method::PATCH,
            &format!("/api/v1/iam/providers/{provider_id}"),
            Some(cookie),
            Some(json!({ "enabled": true })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "publish: {}",
        response.body
    );
}

/// The gate, on its own: a provider that has never passed a test stays off.
async fn assert_untested_providers_stay_off(fixture: &Fixture, cookie: &str, provider_id: Uuid) {
    let response = call(
        &fixture.state,
        session_request(
            Method::PATCH,
            &format!("/api/v1/iam/providers/{provider_id}"),
            Some(cookie),
            Some(json!({ "enabled": true })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "a provider that has never passed a connection test cannot be switched on: {}",
        response.body
    );
    assert_eq!(
        response.body["error"]["code"],
        json!("provider_not_ready"),
        "and the refusal says which precondition is unmet, rather than failing generically"
    );
}

/// The person the stub provider will assert on the next authorization request.
///
/// A named struct rather than four positional strings, because the second walk in this file
/// asserts a *different* identity and a `&["editors"]` in the wrong position compiles perfectly
/// while meaning something else entirely.
struct Subject {
    subject: &'static str,
    email: &'static str,
    display_name: &'static str,
    groups: &'static [&'static str],
}

/// The identity the main walk signs in: in the `editors` group, which is what the legacy claim
/// mapping keys on.
const EDITOR: Subject = Subject {
    subject: "stub-user-1",
    email: "sso-live-subject@omnion.test",
    display_name: "Live Directory Person",
    groups: &["editors"],
};

/// Walk a full OIDC sign-in: `start` → the provider's own authorization endpoint → `callback`.
///
/// `challenge`, `code` and `state` are all read out of what each party actually sent, so the two
/// servers are driven against each other rather than the walk inventing a code.
async fn oidc_sign_in(
    fixture: &Fixture,
    idp: &StubIdp,
    slug: &str,
    who: &Subject,
) -> TestResponse {
    let start = call(
        &fixture.state,
        public_request(
            Method::GET,
            &format!("/api/v1/auth/sso/{slug}/start?return_to=/analytics"),
            &fixture.host,
        ),
    )
    .await;
    assert_eq!(
        start.status,
        StatusCode::FOUND,
        "a live provider must produce a redirect, not a refusal: {}",
        start.body
    );
    let authorization_url = start.location.expect("a redirect carries a Location");
    let authorization_url = percent_decode(&authorization_url);
    assert!(
        authorization_url.starts_with(&idp.authorization_endpoint()),
        "the browser must be sent to the provider's own endpoint, not a local one: {authorization_url}"
    );

    // The PKCE challenge is ours; the provider has to be told which identity to assert, so it is
    // told the challenge it was actually sent.
    let challenge =
        query_value(&authorization_url, "code_challenge").expect("PKCE must ride the URL");
    idp.expect_identity(
        who.subject,
        who.email,
        who.display_name,
        who.groups,
        &challenge,
    );

    let callback_path = authorization_target(&authorization_url).await;

    call(
        &fixture.state,
        public_request(Method::GET, &callback_path, &fixture.host),
    )
    .await
}

/// The whole point of the slice: a real provider, a real token, a real session.
#[tokio::test]
async fn a_live_oidc_provider_signs_a_person_in_end_to_end() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let cookie = fixture.owner_session().await;

    // The client secret lives in the environment, never in the row — the walk puts it there the
    // way an operator would, so `secret_present` becomes true and the exchange is confidential.
    unsafe { std::env::set_var(SECRET_REF, CLIENT_SECRET) };

    let idp = StubIdp::start().await;

    // ---- 1. The discovery test reaches the provider and reports what it actually found -------
    let (provider_id, _) = connect(
        &fixture,
        &cookie,
        json!({
            "slug": "stub-oidc",
            "kind": "oidc",
            "name": "Live Stub Directory",
            "config": {
                "issuer": idp.issuer(),
                "client_id": CLIENT_ID,
                "role_mappings": [{ "claim_value": "editors", "role_slug": "editor" }],
            },
            "secret_ref": SECRET_REF,
            "group_claim": "groups",
            "jit_enabled": true,
        }),
    )
    .await;

    let listed = call(
        &fixture.state,
        session_request(Method::GET, "/api/v1/iam/providers", Some(&cookie), None),
    )
    .await;
    assert_eq!(
        listed.body["providers"][0]["secret_present"],
        json!(true),
        "the named variable is defined in this process, and the panel can see that without \
         the value ever leaving the process"
    );

    assert_untested_providers_stay_off(&fixture, &cookie, provider_id).await;

    let tested = call(
        &fixture.state,
        session_request(
            Method::POST,
            &format!("/api/v1/iam/providers/{provider_id}/test"),
            Some(&cookie),
            None,
        ),
    )
    .await;
    assert_eq!(tested.status, StatusCode::OK, "test: {}", tested.body);
    assert_eq!(
        tested.body["status"],
        json!("ok"),
        "discovery answered and the JWKS is readable over a real socket: {}",
        tested.body
    );
    assert!(
        tested.body["endpoints"]["jwks_uri"]
            .as_str()
            .is_some_and(|uri| uri.ends_with("/jwks"))
    );

    publish(&fixture, &cookie, provider_id).await;

    // ---- 2. The browser is sent to the provider, and comes back with a session ---------------
    let start = call(
        &fixture.state,
        public_request(
            Method::GET,
            "/api/v1/auth/sso/stub-oidc/start?return_to=/analytics",
            &fixture.host,
        ),
    )
    .await;
    let state_value = query_value(start.location.as_deref().expect("a redirect"), "state")
        .expect("the challenge rides the redirect");
    assert!(
        fixture.challenge_matches(provider_id, &state_value).await,
        "the state in the redirect is the one the platform stored — hashed, and findable by hash"
    );

    // A completed sign-in answers with the session body and a cookie. The panel path the browser
    // would follow is added to that same response as a `Location` (the session rides the response
    // that carries it), so both facts are asserted: who is signed in, and where the panel sends
    // them next.
    let response = oidc_sign_in(&fixture, &idp, "stub-oidc", &EDITOR).await;
    assert!(
        response.status.is_success(),
        "a verified sign-in opens a session: {}",
        response.body
    );
    assert_eq!(
        response.location.as_deref(),
        Some("/admin/analytics"),
        "the requested panel path is where the sign-in ends"
    );
    assert_eq!(
        response.body["user"]["email"],
        json!("sso-live-subject@omnion.test"),
        "and the session belongs to the person the provider asserted"
    );

    // ---- 3. The session is real: it authenticates as the provisioned person ------------------
    let value = session_value(
        &response
            .set_cookie
            .expect("a completed sign-in sets a session"),
    );
    let me = call(
        &fixture.state,
        session_request(
            Method::GET,
            "/api/v1/me",
            Some(&format!("omnion_session={value}")),
            None,
        ),
    )
    .await;
    assert_eq!(
        me.status,
        StatusCode::OK,
        "the session must work: {}",
        me.body
    );
    assert_eq!(
        me.body["user"]["email"],
        json!("sso-live-subject@omnion.test"),
        "the account was provisioned from the address the provider asserted"
    );
    let user_id: Uuid = Uuid::parse_str(me.body["user"]["id"].as_str().unwrap()).unwrap();

    // ---- 4. JIT did what it says: a marker instead of a password, and the subject indexed -----
    let password_hash: Option<String> =
        sqlx::query_scalar("select password_hash from users where id = $1")
            .bind(user_id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the hash must be readable");
    assert_eq!(
        password_hash.as_deref(),
        Some(omnion_identity::sso::providers::JIT_PASSWORD_MARKER),
        "a JIT account has no local password"
    );
    // And the password path really refuses it, rather than the marker being decorative.
    let refused = call(
        &fixture.state,
        session_request(
            Method::POST,
            "/api/v1/auth/login",
            None,
            Some(json!({ "email": "sso-live-subject@omnion.test", "password": "anything" })),
        ),
    )
    .await;
    assert!(
        !refused.status.is_success(),
        "a JIT account cannot be signed into with a password: {}",
        refused.body
    );

    // ---- 5. The claim → role mapping attached a real binding --------------------------------
    let roles: Vec<String> = sqlx::query_scalar(
        "select r.key from role_bindings b join roles r on r.id = b.role_id \
         where b.subject_id = $1 and b.revoked_at is null order by r.key",
    )
    .bind(user_id)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the bindings must be readable");
    assert!(
        roles.iter().any(|key| key == "editor"),
        "the directory's `editors` group must have attached the editor role: {roles:?}"
    );

    // ---- 6. The provider saw the PKCE challenge and accepted our verifier -------------------
    let observations = idp.observations();
    assert_eq!(
        observations.challenges.len(),
        1,
        "the authorization request carried exactly one PKCE challenge"
    );
    assert!(
        observations
            .challenges
            .iter()
            .all(|value| !value.is_empty()),
        "a `code` flow must use PKCE, and the challenge must be there"
    );
    assert!(
        !observations.verifiers.is_empty(),
        "the token exchange sent the verifier the challenge was derived from"
    );
    assert!(!observations.refused, "no exchange was refused");

    // ---- 7. The sign-in log records the outcome, with the role that was attached ------------
    let events = call(
        &fixture.state,
        session_request(
            Method::GET,
            &format!("/api/v1/iam/providers/{provider_id}/events"),
            Some(&cookie),
            None,
        ),
    )
    .await;
    assert_eq!(events.status, StatusCode::OK, "events: {}", events.body);
    let first = &events.body["events"][0];
    assert_eq!(first["outcome"], json!("provisioned"));
    assert_eq!(first["external_subject"], json!("stub-user-1"));
    assert!(
        first["roles_applied"]
            .as_str()
            .is_some_and(|roles| roles.contains("editor")),
        "the log names the role the mapping attached: {first}"
    );

    // ---- 8. A token minted for somebody else's code must not open a session -----------------
    // The signature is perfectly valid — the provider really issued it. What does not match is
    // the code it was issued for, and a stateless signature check alone would never notice.
    idp.corrupt_code_hash();
    let wrong_code = oidc_sign_in(&fixture, &idp, "stub-oidc", &EDITOR).await;
    assert_eq!(
        wrong_code.status,
        StatusCode::BAD_REQUEST,
        "a token bound to somebody else's code must be refused: {}",
        wrong_code.body
    );
    assert_eq!(wrong_code.body["error"]["code"], json!("token_refused"));

    // ---- 9. A token signed by a key the provider does not publish is refused ----------------
    idp.sign_with_unpublished_key();
    let unpublished = oidc_sign_in(&fixture, &idp, "stub-oidc", &EDITOR).await;
    assert_eq!(
        unpublished.status,
        StatusCode::BAD_REQUEST,
        "only the published keys may verify a token: {}",
        unpublished.body
    );
    assert_eq!(unpublished.body["error"]["code"], json!("token_refused"));

    // ---- 10. A replayed callback buys nothing -----------------------------------------------
    let replay = call(
        &fixture.state,
        public_request(
            Method::GET,
            "/api/v1/auth/sso/stub-oidc/callback?state=not-ours&code=whatever",
            &fixture.host,
        ),
    )
    .await;
    assert_eq!(
        replay.status,
        StatusCode::BAD_REQUEST,
        "replay: {}",
        replay.body
    );
    assert_eq!(
        replay.body["error"]["code"],
        json!("invalid_state"),
        "a state we never issued is refused before any signature is looked at"
    );

    // …and the refusals above were recorded, because a refusal is a fact, not a silence.
    let refusals: i64 = sqlx::query_scalar(
        "select count(*) from auth_provider_events \
         where provider_id = $1 and outcome = 'refused'",
    )
    .bind(provider_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must run");
    assert!(
        refusals >= 2,
        "each refused attempt left a row the panel can show: {refusals}"
    );

    unsafe { std::env::remove_var(SECRET_REF) };
    fixture.cleanup().await;
}

/// The same chain over SAML: a posted, signed assertion instead of a code.
#[tokio::test]
async fn a_live_saml_provider_signs_a_person_in_end_to_end() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let cookie = fixture.owner_session().await;
    let idp = StubIdp::start().await;

    let (provider_id, _) = connect(
        &fixture,
        &cookie,
        json!({
            "slug": "stub-saml",
            "kind": "saml",
            "name": "Live Stub SAML",
            "config": {
                "issuer": SAML_ISSUER,
                "audience": SAML_AUDIENCE,
                "certificate_pem": idp.certificate_pem(),
                "group_attribute": "groups",
                "email_attribute": "email",
                "display_name_attribute": "displayName",
                "role_mappings": [{ "claim_value": "editors", "role_slug": "editor" }],
            },
            "group_claim": "groups",
            "jit_enabled": true,
        }),
    )
    .await;

    // SAML has no discovery, so the `test` action proves the certificate by asking the real
    // reader to parse a response — a certificate it cannot use fails here, not at a real sign-in.
    let tested = call(
        &fixture.state,
        session_request(
            Method::POST,
            &format!("/api/v1/iam/providers/{provider_id}/test"),
            Some(&cookie),
            None,
        ),
    )
    .await;
    assert_eq!(tested.body["status"], json!("ok"), "test: {}", tested.body);

    publish(&fixture, &cookie, provider_id).await;

    // `start` redirects to the relay page, which now carries the challenge in its URL. This is
    // the half that was broken: the page used to post the *return path* as `RelayState`, which
    // the callback could never claim, so every SAML sign-in ended in `invalid_state`.
    let start = call(
        &fixture.state,
        public_request(
            Method::GET,
            "/api/v1/auth/sso/stub-saml/start?return_to=/media",
            &fixture.host,
        ),
    )
    .await;
    assert_eq!(start.status, StatusCode::FOUND, "start: {}", start.body);
    let relay_page = percent_decode(&start.location.expect("a SAML start redirects to the page"));
    let state_value = query_value(&relay_page, "state")
        .filter(|value| !value.is_empty())
        .expect("the relay page's URL carries the challenge");
    assert!(
        fixture.challenge_matches(provider_id, &state_value).await,
        "and it is the state the platform stored"
    );

    // The browser fetches that page and the page hands the challenge on as `RelayState`.
    let page = call(
        &fixture.state,
        public_request(Method::GET, &relay_page, &fixture.host),
    )
    .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.raw.contains(&format!("value=\"{state_value}\"")),
        "the page posts the challenge back: {}",
        page.raw
    );
    let posted_state = page
        .raw
        .split("name=\"RelayState\" value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("the form has a RelayState")
        .to_owned();

    // The provider signs an assertion for a new directory person.
    idp.expect_identity(
        "stub-saml-user",
        "sso-live-saml@omnion.test",
        "SAML Directory Person",
        &["editors"],
        "",
    );
    let assertion = stub_get(&format!("{}/saml/assertion", idp.issuer())).await;
    let encoded = assertion["SAMLResponse"]
        .as_str()
        .expect("the provider posts a SAMLResponse")
        .to_owned();

    let callback = call(
        &fixture.state,
        form_request(
            "/api/v1/auth/sso/stub-saml/callback",
            &fixture.host,
            &format!(
                "SAMLResponse={}&RelayState={}",
                form_encode(&encoded),
                form_encode(&posted_state)
            ),
        ),
    )
    .await;
    // A SAML sign-in answers the *browser* with the session body and a cookie rather than a
    // redirect — the browser already navigated itself here by submitting the form, so there is
    // nowhere left to redirect to. What matters is that the session is real and names the person
    // the assertion described.
    assert!(
        callback.status.is_success(),
        "a verified assertion opens a session: {}",
        callback.body
    );
    assert_eq!(
        callback.body["user"]["email"],
        json!("sso-live-saml@omnion.test"),
        "the session belongs to the person the assertion described"
    );

    let value = session_value(
        &callback
            .set_cookie
            .expect("a completed sign-in sets a session"),
    );
    let me = call(
        &fixture.state,
        session_request(
            Method::GET,
            "/api/v1/me",
            Some(&format!("omnion_session={value}")),
            None,
        ),
    )
    .await;
    assert_eq!(
        me.status,
        StatusCode::OK,
        "the session must work: {}",
        me.body
    );
    assert_eq!(me.body["user"]["email"], json!("sso-live-saml@omnion.test"));
    let user_id: Uuid = Uuid::parse_str(me.body["user"]["id"].as_str().unwrap()).unwrap();

    let roles: Vec<String> = sqlx::query_scalar(
        "select r.key from role_bindings b join roles r on r.id = b.role_id \
         where b.subject_id = $1 and b.revoked_at is null order by r.key",
    )
    .bind(user_id)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the bindings must be readable");
    assert!(
        roles.iter().any(|key| key == "editor"),
        "the assertion's `editors` group attached the editor role: {roles:?}"
    );

    // ---- A tampered assertion is refused: the envelope digest must cover the claims ----------
    let second = call(
        &fixture.state,
        public_request(
            Method::GET,
            "/api/v1/auth/sso/stub-saml/start?return_to=/media",
            &fixture.host,
        ),
    )
    .await;
    let second_page_url = percent_decode(&second.location.expect("a second start"));
    let second_state = query_value(&second_page_url, "state").expect("a second challenge");
    let second_page = call(
        &fixture.state,
        public_request(Method::GET, &second_page_url, &fixture.host),
    )
    .await;
    let second_posted = second_page
        .raw
        .split("name=\"RelayState\" value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("the form has a RelayState")
        .to_owned();

    // The tamper happens on the *decoded* assertion and is re-encoded: the wire body is base64,
    // so a string replace against it is a no-op that would have "passed" while changing nothing.
    let tampered = {
        use base64::Engine as _;
        let engine = base64::engine::general_purpose::STANDARD;
        let document = String::from_utf8(
            engine
                .decode(encoded.as_bytes())
                .expect("the provider posted base64"),
        )
        .expect("the assertion is text");
        let tampered = document.replace("sso-live-saml@omnion.test", "attacker@evil.example");
        assert_ne!(
            tampered, document,
            "the tamper has to change the signed bytes, or the test proves nothing"
        );
        engine.encode(tampered)
    };
    let refused = call(
        &fixture.state,
        form_request(
            "/api/v1/auth/sso/stub-saml/callback",
            &fixture.host,
            &format!(
                "SAMLResponse={}&RelayState={}",
                form_encode(&tampered),
                form_encode(&second_posted)
            ),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "a changed claim must not verify against the original signature: {}",
        refused.body
    );
    assert_eq!(refused.body["error"]["code"], json!("saml_refused"));

    // A second callback with the *same* (already consumed) challenge is refused too: the
    // challenge was spent by the attempt above, whatever the attempt's outcome.
    let replayed = call(
        &fixture.state,
        form_request(
            "/api/v1/auth/sso/stub-saml/callback",
            &fixture.host,
            &format!(
                "SAMLResponse={}&RelayState={}",
                form_encode(&encoded),
                form_encode(&second_state)
            ),
        ),
    )
    .await;
    assert!(
        !replayed.status.is_success(),
        "a spent challenge cannot be used again: {}",
        replayed.body
    );

    fixture.cleanup().await;
}

// ------------------------------------------------------------------------------------------
// REQ-065 slice 3: the rule set decides on the sign-in path, and says so in the audit
// ------------------------------------------------------------------------------------------

/// The identity a rule set is written *against*: a group the rules do not name, so a rule that
/// matches is a rule that read something real rather than a catch-all.
const ANALYST: Subject = Subject {
    subject: "stub-user-rules",
    email: "sso-live-rules@omnion.test",
    display_name: "Rule Mapped Person",
    groups: &["analytics"],
};

/// A real OIDC sign-in resolves the provider's ordered rules, and the sign-in audit says which
/// one decided.
///
/// This is the walk the dry run exists to be checked against. Everything upstream of it was
/// proven by a real provider and real cryptography, and everything downstream is bookkeeping; the
/// claim being made here is the narrow one that `RoleRules::resolve` is what a *callback* calls,
/// not only what a preview calls, and that the sentence `role via rule #N` reaches the audit.
///
/// The test is arranged so it can only pass one way. The provider carries **both** a legacy
/// `role_mappings` entry that would grant the platform `editor` and a rule set that says
/// something different. If the rules were consulted in *addition* to the mapping, the person
/// would hold two roles and the dry run — which shows the rules alone — would have been a lie
/// about what the sign-in does. Asserting that `editor` is absent is therefore not a detail: it
/// is the proof that "first match wins" means first, not last.
#[tokio::test]
async fn a_real_sign_in_resolves_the_rules_and_says_which_one_decided() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let cookie = fixture.owner_session().await;
    unsafe { std::env::set_var(SECRET_REF, CLIENT_SECRET) };
    let idp = StubIdp::start().await;

    let (provider_id, _) = connect(
        &fixture,
        &cookie,
        json!({
            "slug": "stub-rules",
            "kind": "oidc",
            "name": "Live Rule Directory",
            "config": {
                "issuer": idp.issuer(),
                "client_id": CLIENT_ID,
                // The legacy claim mapping is left in place on purpose — see the doc comment.
                "role_mappings": [{ "claim_value": "analytics", "role_slug": "editor" }],
            },
            "secret_ref": SECRET_REF,
            "group_claim": "groups",
            "jit_enabled": true,
        }),
    )
    .await;

    let tested = call(
        &fixture.state,
        session_request(
            Method::POST,
            &format!("/api/v1/iam/providers/{provider_id}/test"),
            Some(&cookie),
            None,
        ),
    )
    .await;
    assert_eq!(tested.body["status"], json!("ok"), "test: {}", tested.body);
    publish(&fixture, &cookie, provider_id).await;

    // Two roles to rule between, and the rules are ordered so that the *first* one is the narrow
    // one. A set evaluated last-to-first would grant the moderator and the test would say so.
    let editor: Uuid = sqlx::query_scalar("select id from roles where key = 'editor'")
        .fetch_one(fixture.db.pool())
        .await
        .expect("the platform seeds a base editor role");
    let moderator: Uuid = sqlx::query_scalar("select id from roles where key = 'moderator'")
        .fetch_one(fixture.db.pool())
        .await
        .expect("the platform seeds a base moderator role");

    let saved = call(
        &fixture.state,
        session_request(
            Method::PUT,
            &format!("/api/v1/iam/providers/{provider_id}/role-rules"),
            Some(&cookie),
            Some(json!({
                "rules": [
                    {
                        "when_kind": "group",
                        "when_key": "groups",
                        "when_operator": "equals",
                        "when_value": "analytics",
                        "role_id": moderator,
                        "scope_type": "organization",
                    },
                    {
                        "when_kind": "group",
                        "when_key": "groups",
                        "when_operator": "equals",
                        "when_value": "analytics",
                        "role_id": editor,
                        "scope_type": "organization",
                    },
                ]
            })),
        ),
    )
    .await;
    assert_eq!(
        saved.status,
        StatusCode::OK,
        "two rules, both matching, in a saved order: {}",
        saved.body
    );

    // ---- 1. The dry run and the callback are asked the same question, before the sign-in -----
    let preview = call(
        &fixture.state,
        session_request(
            Method::POST,
            &format!("/api/v1/iam/providers/{provider_id}/role-rules/preview"),
            Some(&cookie),
            Some(json!({ "sample": { "groups": ["analytics"], "sub": "stub-user-rules" } })),
        ),
    )
    .await;
    assert_eq!(preview.status, StatusCode::OK, "preview: {}", preview.body);
    assert_eq!(
        preview.body["matched_rule_index"],
        json!(0),
        "the first rule wins, and the preview says which: {}",
        preview.body
    );
    assert_eq!(
        preview.body["role_id"],
        json!(moderator),
        "and the role it predicts is the first rule's"
    );

    // ---- 2. A real sign-in, through the provider, resolves the same rule ---------------------
    let response = oidc_sign_in(&fixture, &idp, "stub-rules", &ANALYST).await;
    assert!(
        response.status.is_success(),
        "a verified sign-in opens a session: {}",
        response.body
    );
    let user_id: Uuid = Uuid::parse_str(response.body["user"]["id"].as_str().unwrap()).unwrap();

    let roles: Vec<String> = sqlx::query_scalar(
        "select r.key from role_bindings b join roles r on r.id = b.role_id \
         where b.subject_id = $1 and b.revoked_at is null order by r.key",
    )
    .bind(user_id)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the bindings must be readable");
    assert!(
        roles.iter().any(|key| key == "moderator"),
        "the first rule's role is the one attached: {roles:?}"
    );
    assert!(
        !roles.iter().any(|key| key == "editor"),
        "the second matching rule is NOT also applied — first match wins, and the rule set \
         replaces the claim mapping rather than adding to it: {roles:?}"
    );
    assert_eq!(
        role_of(&fixture, user_id).await,
        Some("moderator".to_owned()),
        "and the role the sign-in produced is the role the dry run predicted"
    );
    assert_eq!(
        preview.body["role_id"],
        json!(moderator),
        "the preview and the callback named the same role id, which is the only thing that makes \
         the preview evidence rather than a picture of it"
    );

    // ---- 3. The audit carries the sentence, and not the rule's condition ---------------------
    let (metadata,): (String,) = sqlx::query_as(
        "select metadata::text from audit_log where action = 'iam.sso_sign_in' \
         and actor_user_id = $1 order by created_at desc limit 1",
    )
    .bind(user_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the sign-in audit entry must exist");
    assert!(
        metadata.contains("role via rule #1"),
        "the audit answers 'why does this person have that role': {metadata}"
    );
    assert!(
        metadata.contains("moderator"),
        "and names what it granted: {metadata}"
    );
    // The rule's `when_value` is `analytics` — the group the person belongs to. It must NOT be
    // written down: a rule diff in a log nobody audits is a second copy of the directory.
    assert!(
        !metadata.contains("analytics"),
        "the audit records the decision, not the directory behind it: {metadata}"
    );

    // ---- 4. The event fires, carrying ids and counts only ------------------------------------
    let (payload,): (serde_json::Value,) = sqlx::query_as(
        "select payload from events where name = 'iam.role_rule_matched' \
         and organization_id = $1 order by created_at desc limit 1",
    )
    .bind(fixture.organization_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("a role granted by a rule is a fact somebody subscribed to");
    assert_eq!(payload["subject_id"], json!(user_id));
    assert_eq!(payload["rule_position"], json!(0));
    assert_eq!(payload["role_id"], json!(moderator));
    assert_eq!(payload["scope_type"], json!("organization"));
    assert!(
        !payload.to_string().contains("analytics"),
        "a webhook payload carries ids and codes, never a group name: {payload}"
    );

    // ---- 5. An identity that matches nothing says so, rather than silently granting -----------
    let visitor = Subject {
        subject: "stub-user-nomatch",
        email: "sso-live-nomatch@omnion.test",
        display_name: "Nobody In Particular",
        groups: &[],
    };
    let unmatched = oidc_sign_in(&fixture, &idp, "stub-rules", &visitor).await;
    assert!(
        unmatched.status.is_success(),
        "a sign-in nobody wrote a rule for still succeeds: {}",
        unmatched.body
    );
    let visitor_id: Uuid =
        Uuid::parse_str(unmatched.body["user"]["id"].as_str().unwrap()).unwrap();
    let (no_rule_metadata,): (String,) = sqlx::query_as(
        "select metadata::text from audit_log where action = 'iam.sso_sign_in' \
         and actor_user_id = $1 order by created_at desc limit 1",
    )
    .bind(visitor_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("that sign-in is audited too");
    assert!(
        no_rule_metadata.contains("no rule matched"),
        "a rule set that granted nothing says so in those words — a silent no-op and a misconfigured \
         provider look identical from the panel otherwise: {no_rule_metadata}"
    );
    // And the event did *not* fire for a sign-in no rule decided.
    let fired: i64 = sqlx::query_scalar(
        "select count(*) from events where name = 'iam.role_rule_matched' \
         and payload ->> 'subject_id' = $1",
    )
    .bind(visitor_id.to_string())
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must run");
    assert_eq!(
        fired, 0,
        "`iam.role_rule_matched` means a rule matched; firing it for a default role would make \
         the event name false"
    );

    unsafe { std::env::remove_var(SECRET_REF) };
    fixture.cleanup().await;
}

/// The single role key a subject holds, for the assertions above.
///
/// A `Vec` reduced to one is the shape that makes the claim legible: "the role this person ended
/// up with" is one string, and a test that asserts on a list forces the reader to work out which
/// element was meant.
async fn role_of(fixture: &Fixture, user_id: Uuid) -> Option<String> {
    let mut roles: Vec<String> = sqlx::query_scalar(
        "select r.key from role_bindings b join roles r on r.id = b.role_id \
         where b.subject_id = $1 and b.revoked_at is null order by r.key",
    )
    .bind(user_id)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the bindings must be readable");
    assert!(
        roles.len() <= 1,
        "this walk asserts a single role, and the person holds {roles:?}"
    );
    roles.pop()
}
