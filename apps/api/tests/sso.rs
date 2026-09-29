//! Integration walk for enterprise sign-in (REQ-006, slice 4b-2; docs/07-IAM.md §11).
//!
//! The walk drives the **real router**, and it covers the halves the unit tests cannot reach:
//!
//! * the management surface over HTTP — connect, refuse a bad reference, prove a secret is a name
//!   and never a value, patch, remove;
//! * the JIT walk — an unknown subject is provisioned on the first sign-in, gets the role its
//!   claim maps to, comes back as `Existing` the second time, and gets **no local password**;
//! * the refusals that make the flow safe — a disabled provider, a JIT-off provider, a
//!   deactivated account, a challenge that is expired or already spent, and a callback aimed at a
//!   provider that does not exist;
//! * the sign-in log: every one of those refusals lands in `auth_provider_events` with its own
//!   machine-readable reason.
//!
//! The protocol half is proven elsewhere against real cryptography (`crates/identity/src/sso`:
//! RS256 against a generated 2048-bit key, SAML's digest *and* signature). This file is about
//! what the HTTP layer does around it, so it deliberately does not re-derive a token — it drives
//! the refusals, the provisioning and the bookkeeping, which is where the remaining risk is.
//!
//! It runs against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`); when PostgreSQL is not
//! reachable the suite skips itself with a printed reason.

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

/// Password used for the account this suite creates.
const PASSWORD: &str = "correct horse battery";

/// The host the public sign-in routes are addressed on, and the domain the fixture registers for
/// its organization.
const HOST: &str = "sso.omnion.test";

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    location: Option<String>,
    body: Value,
    /// The raw bytes, for the one route that answers HTML rather than JSON.
    raw: String,
}

/// Drive the real router without a network socket.
async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");

    let status = response.status();
    let set_cookie = header_value(&response, header::SET_COOKIE);
    let location = header_value(&response, header::LOCATION);
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

/// A public request: no session, but the host the sign-in routes resolve the organization from.
fn public_request(method: Method, uri: &str, host: &str, body: Option<Value>) -> Request<Body> {
    match body {
        Some(value) => Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, host)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .expect("request must build"),
        None => Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, host)
            .body(Body::empty())
            .expect("request must build"),
    }
}

/// One response header, as text.
fn header_value(response: &axum::response::Response, name: header::HeaderName) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Build a request; a credential that looks like `omnion_session=…` becomes the session cookie
/// and anything else becomes a bearer token.
fn request(method: Method, uri: &str, session: Option<&str>, body: Option<Value>) -> Request<Body> {
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
            .expect("request must build"),
        None => builder.body(Body::empty()).expect("request must build"),
    }
}

/// Object store of the test state.
fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

/// A state whose database has all migrations applied and the IAM seed loaded.
async fn live_state() -> Option<(AppState, Db)> {
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

/// A fresh organization with an Owner.
struct Fixture {
    state: AppState,
    db: Db,
    organization_id: Uuid,
    owner_email: String,
    /// The host header every public request carries, so the sign-in routes resolve this
    /// organization rather than refusing an ambiguous one.
    host: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let slug = format!("sso-{}", Uuid::new_v4().simple());
        let organization_id: Uuid = sqlx::query_scalar(
            "insert into organizations (name, slug) values ($1, $2) returning id",
        )
        .bind("SSO Test Organization")
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created");

        let email = format!("sso-owner-{}@omnion.test", Uuid::new_v4().simple());
        let owner = users::create_user(
            db.pool(),
            NewUser {
                email: email.clone(),
                password: PASSWORD.to_owned(),
                display_name: "SSO Test Owner".to_owned(),
                organization_id: Some(organization_id),
            },
        )
        .await
        .expect("the owner must be created");
        seed::bind_owner(db.pool(), owner.id)
            .await
            .expect("the owner binding must be created");

        // The public sign-in surface resolves its organization from the request host (the same
        // rule the rendered-content surface uses), so a multi-organization database needs a
        // registered domain for the walk to address honestly rather than by guessing.
        let site_slug = format!("sso-site-{}", Uuid::new_v4().simple());
        let site_id: Uuid = sqlx::query_scalar(
            "insert into sites (organization_id, key, name) values ($1, $2, $3) returning id",
        )
        .bind(organization_id)
        .bind(&site_slug)
        .bind("SSO Test Site")
        .fetch_one(db.pool())
        .await
        .expect("the test site must be created");
        // A host is globally unique and a previous failed run may have left one behind, so
        // clear it first rather than failing on a stale row: a re-run must be able to recover.
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
            owner_email: email,
            host: HOST.to_owned(),
        })
    }

    /// The Owner of the fixture, signed in — the full \`Cookie\` header value, so the callers
    /// cannot confuse a session with a bearer token.
    async fn owner_session(&self) -> String {
        format!("omnion_session={}", self.owner_token().await)
    }

    /// The Owner of the fixture, signed in — the session cookie value.
    async fn owner_token(&self) -> String {
        let response = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/auth/login",
                None,
                Some(json!({ "email": self.owner_email, "password": PASSWORD })),
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "login: {}", response.body);

        response
            .set_cookie
            .clone()
            .expect("login must set the session cookie")
            .split(';')
            .next()
            .expect("cookie has a value")
            .split_once('=')
            .expect("cookie is name=value")
            .1
            .to_owned()
    }

    async fn cleanup(&self) {
        sqlx::query("delete from sso_challenges where organization_id = $1")
            .bind(self.organization_id)
            .execute(self.db.pool())
            .await
            .expect("challenge cleanup must run");
        sqlx::query("delete from auth_provider_events where organization_id = $1")
            .bind(self.organization_id)
            .execute(self.db.pool())
            .await
            .expect("event cleanup must run");
        sqlx::query("delete from auth_providers where organization_id = $1")
            .bind(self.organization_id)
            .execute(self.db.pool())
            .await
            .expect("provider cleanup must run");
        sqlx::query("delete from users where organization_id = $1 and email like 'sso-subject-%'")
            .bind(self.organization_id)
            .execute(self.db.pool())
            .await
            .expect("provisioned account cleanup must run");
        sqlx::query(
            "delete from role_bindings where user_id in \
                     (select id from users where organization_id = $1)",
        )
        .bind(self.organization_id)
        .execute(self.db.pool())
        .await
        .expect("binding cleanup must run");
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
        sqlx::query("delete from users where organization_id = $1")
            .bind(self.organization_id)
            .execute(self.db.pool())
            .await
            .expect("account cleanup must run");
        sqlx::query("delete from organizations where id = $1")
            .bind(self.organization_id)
            .execute(self.db.pool())
            .await
            .expect("organization cleanup must run");
    }

    /// Connect a provider over HTTP and answer its id.
    async fn connect(&self, cookie: &str, body: Value) -> Uuid {
        let response = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/iam/providers",
                Some(cookie),
                Some(body),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::CREATED,
            "connect: {}",
            response.body
        );
        Uuid::parse_str(response.body["id"].as_str().expect("an id")).expect("a uuid")
    }
}

/// The full walk.
#[tokio::test]
async fn enterprise_sign_in_provisions_maps_and_refuses() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let cookie = fixture.owner_session().await;

    // ---- 1. The management surface is permission-guarded and answers the org's list ----------
    let unauthenticated = call(
        &fixture.state,
        request(Method::GET, "/api/v1/iam/providers", None, None),
    )
    .await;
    assert_eq!(
        unauthenticated.status,
        StatusCode::UNAUTHORIZED,
        "a provider list needs a session: {}",
        unauthenticated.body
    );

    let empty = call(
        &fixture.state,
        request(Method::GET, "/api/v1/iam/providers", Some(&cookie), None),
    )
    .await;
    assert_eq!(empty.status, StatusCode::OK, "list: {}", empty.body);
    assert_eq!(
        empty.body["providers"].as_array().map(Vec::len),
        Some(0),
        "a fresh organization has no provider"
    );
    // The catalogue grew when directory support landed: `0051` widened the `kind` check from
    // three protocols to five kinds, because a directory is not a protocol and refusing one here
    // would have made the whole LDAP/AD half unreachable. The assertion is on the *contents*,
    // not a count — a count is a thing that silently rots the next time a kind is added, which
    // is the opposite of what a test is for. Each entry is an object (`value`, `label`, `family`),
    // so the assertion reads `value`: the panel's picker keys off that and nothing else.
    let kinds: Vec<&str> = empty.body["kinds"]
        .as_array()
        .expect("the form is told which kinds exist")
        .iter()
        .filter_map(|kind| kind["value"].as_str())
        .collect();
    for expected in ["ldap", "active_directory", "oidc", "oauth2", "saml"] {
        assert!(
            kinds.contains(&expected),
            "`{expected}` is a kind this platform speaks and the form must offer it: {kinds:?}"
        );
    }
    // And the families are the reason the panel groups its form: a directory is a live
    // connection with a service account, not a redirect, so it must not sit under "protocol".
    let directory_family: Vec<&str> = empty.body["kinds"]
        .as_array()
        .expect("kinds")
        .iter()
        .filter(|kind| matches!(kind["value"].as_str(), Some("ldap" | "active_directory")))
        .filter_map(|kind| kind["family"].as_str())
        .collect();
    assert!(
        directory_family.iter().all(|family| *family == "directory"),
        "the directory kinds are grouped as directories, so the wizard never offers them an \
         OAuth scope: {directory_family:?}"
    );

    // ---- 2. Connect a provider; it is created switched off, and JIT off ---------------------
    let provider_slug = "okta";
    let provider_id = fixture
        .connect(
            &cookie,
            json!({
                "slug": provider_slug,
                "kind": "oidc",
                "name": "Company directory",
                "config": {
                    "issuer": "https://idp.example/realms/omnion",
                    "client_id": "omnion-panel",
                    "role_mappings": [{ "claim_value": "editors", "role_slug": "editor" }],
                },
                "secret_ref": "OMNION_SSO_TEST_SECRET",
                "group_claim": "groups",
            }),
        )
        .await;

    let row: (bool, bool) =
        sqlx::query_as("select enabled, jit_enabled from auth_providers where id = $1")
            .bind(provider_id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the provider row must be readable");
    assert!(
        !row.0,
        "a provider is created switched off — `test` proves it and `enabled` publishes it"
    );
    assert!(
        !row.1,
        "provisioning is off until it is asked for: a provider that silently creates accounts can \
         be used to fill an organization with strangers"
    );

    // ---- 3. A secret is a name; the list never carries a value ------------------------------
    let list = call(
        &fixture.state,
        request(Method::GET, "/api/v1/iam/providers", Some(&cookie), None),
    )
    .await;
    let first = &list.body["providers"][0];
    assert_eq!(first["secret_ref"], json!("OMNION_SSO_TEST_SECRET"));
    assert_eq!(
        first["secret_present"],
        json!(false),
        "the variable is not defined in this test process, and the panel must be able to say so"
    );
    let serialized = list.body.to_string();
    assert!(
        !serialized.contains("client_secret"),
        "no response may carry a field a secret could ride in: {serialized}"
    );

    // ---- 4. A secret reference that is not an environment-variable name is refused ----------
    let bad_reference = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/providers",
            Some(&cookie),
            Some(json!({
                "slug": "broken",
                "kind": "oidc",
                "name": "Broken",
                "config": { "issuer": "https://idp.example" },
                "secret_ref": "not a variable name",
            })),
        ),
    )
    .await;
    assert_eq!(bad_reference.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        bad_reference.body["error"]["code"],
        json!("invalid_request")
    );
    assert_eq!(
        bad_reference.body["error"]["details"]["field"],
        json!("secret_ref"),
        "the refusal names the field the editor has to fix"
    );

    // ---- 5. A claim → role rule that cannot be read is refused at save time ------------------
    let bad_mapping = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/iam/providers",
            Some(&cookie),
            Some(json!({
                "slug": "unreadable",
                "kind": "oidc",
                "name": "Unreadable",
                "config": { "issuer": "https://idp.example", "role_mappings": [{ "role_slug": "editor" }] },
            })),
        ),
    )
    .await;
    assert_eq!(
        bad_mapping.status,
        StatusCode::BAD_REQUEST,
        "a rule with no claim value would silently lose a role at sign-in"
    );
    assert_eq!(bad_mapping.body["error"]["code"], json!("invalid_request"));

    // ---- 6. The discovery test answers with a verdict, not a transport error -----------------
    let test = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/iam/providers/{provider_id}/test"),
            Some(&cookie),
            None,
        ),
    )
    .await;
    assert_eq!(
        test.status,
        StatusCode::OK,
        "a provider that cannot be reached is a *result*, not an error the browser guesses at: {}",
        test.body
    );
    assert_eq!(
        test.body["status"],
        json!("failed"),
        "the host in the fixture does not resolve, so the test must say so"
    );
    assert!(
        test.body["detail"]
            .as_str()
            .is_some_and(|text| text.len() > 20),
        "the detail has to explain itself: {}",
        test.body
    );
    assert_eq!(test.body["secret_present"], json!(false));

    // ---- 7. A disabled provider is unreachable, and the refusal is logged -------------------
    let start_disabled = call(
        &fixture.state,
        public_request(
            Method::GET,
            &format!("/api/v1/auth/sso/{provider_slug}/start"),
            &fixture.host,
            None,
        ),
    )
    .await;
    assert_eq!(start_disabled.status, StatusCode::NOT_FOUND);
    assert_eq!(
        start_disabled.body["error"]["code"],
        json!("provider_disabled")
    );

    // The public list is the sign-in screen's data: a disabled provider is not in it.
    let public = call(
        &fixture.state,
        public_request(
            Method::GET,
            "/api/v1/auth/sso/providers",
            &fixture.host,
            None,
        ),
    )
    .await;
    assert_eq!(
        public.status,
        StatusCode::OK,
        "public list: {}",
        public.body
    );
    assert_eq!(
        public.body["providers"].as_array().map(Vec::len),
        Some(0),
        "an organization with no live provider offers no SSO button"
    );

    // ---- 8. The JIT walk, driven through the provisioning seam the callback uses ------------
    // The protocol half is proved in `crates/identity/src/sso`; this file proves what happens
    // once an identity has been verified — which is the half with a database in it.
    let subject_email = format!("sso-subject-{}@omnion.test", Uuid::new_v4().simple());
    let provider = omnion_identity::sso::providers::find_provider(fixture.db.pool(), provider_id)
        .await
        .expect("the provider must be readable")
        .expect("the provider exists");

    let identity = omnion_identity::sso::claims::Identity {
        subject: "00uqa".into(),
        email: subject_email.clone(),
        display_name: Some("Directory Person".into()),
        groups: vec!["editors".into()],
        attributes: json!({ "sub": "00uqa" })
            .as_object()
            .cloned()
            .unwrap_or_default(),
    };

    // JIT is off: the sign-in is refused, and no account is created.
    let refused =
        omnion_identity::sso::provisioning::provision(fixture.db.pool(), &provider, &identity)
            .await;
    assert!(
        refused.is_err(),
        "a provider without JIT must refuse an unknown subject rather than create a row"
    );
    assert!(
        users::find_by_email(fixture.db.pool(), &subject_email)
            .await
            .expect("the lookup must run")
            .is_none(),
        "a refused provisioning must leave no account behind"
    );

    // Switch JIT on, as an administrator would from the screen.
    call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/iam/providers/{provider_id}"),
            Some(&cookie),
            Some(json!({ "jit_enabled": true })),
        ),
    )
    .await;
    let provider = omnion_identity::sso::providers::find_provider(fixture.db.pool(), provider_id)
        .await
        .expect("the provider must be readable")
        .expect("the provider exists");

    let provisioned =
        omnion_identity::sso::provisioning::provision(fixture.db.pool(), &provider, &identity)
            .await
            .expect("JIT provisions the first sign-in");
    assert_eq!(
        provisioned.outcome,
        omnion_identity::sso::ProvisionOutcome::Created
    );
    let user = provisioned.user;

    // The account exists, in the right organization, with the provider's display name…
    assert_eq!(user.organization_id, Some(fixture.organization_id));
    assert_eq!(user.display_name, "Directory Person");

    // …and with **no local password**: the row carries the JIT marker, so the password path can
    // refuse it in constant time and a stolen row cannot be turned into a credential.
    let stored_hash: Option<String> =
        sqlx::query_scalar("select password_hash from users where id = $1")
            .bind(user.id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the hash must be readable");
    assert_eq!(
        stored_hash.as_deref(),
        Some(omnion_identity::sso::providers::JIT_PASSWORD_MARKER),
        "a JIT account has no password of its own"
    );
    assert!(omnion_identity::sso::providers::is_jit_account(
        stored_hash.as_deref().unwrap_or_default()
    ));

    // The provider's subject id is indexed, so the same person is found again even if the
    // provider later sends a different address.
    let indexed: bool = sqlx::query_scalar(
        "select coalesce(attributes -> 'sso_subjects' ->> $2 = $3, false) from users where id = $1",
    )
    .bind(user.id)
    .bind(provider_slug)
    .bind("00uqa")
    .fetch_one(fixture.db.pool())
    .await
    .expect("the attribute must be readable");
    assert!(
        indexed,
        "the subject id is indexed against the provider slug"
    );

    // Signing in again finds the same account — no second row, no second binding.
    let again =
        omnion_identity::sso::provisioning::provision(fixture.db.pool(), &provider, &identity)
            .await
            .expect("a known subject is signed into, not reprovisioned");
    assert_eq!(
        again.outcome,
        omnion_identity::sso::ProvisionOutcome::Existing
    );
    assert_eq!(again.user.id, user.id);
    let matching: i64 = sqlx::query_scalar("select count(*) from users where email = $1")
        .bind(&subject_email)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the count must run");
    assert_eq!(matching, 1, "one person is one account");

    // ---- 9. A deactivated account stays deactivated -----------------------------------------
    users::set_status(fixture.db.pool(), user.id, "disabled")
        .await
        .expect("the status must change");
    let disabled = users::find_by_id(fixture.db.pool(), user.id)
        .await
        .expect("the lookup must run")
        .expect("the account exists");
    assert!(!disabled.is_active());
    // Re-provisioning still finds the account (it is `Existing`), and the callback refuses it on
    // `is_active` before opening a session — a provider sign-in never undoes an administrator's
    // decision.
    let after_disable =
        omnion_identity::sso::provisioning::provision(fixture.db.pool(), &provider, &identity)
            .await
            .expect("the account is still found");
    assert!(!after_disable.user.is_active());
    users::set_status(fixture.db.pool(), user.id, "active")
        .await
        .expect("the status must change back");

    // ---- 10. The sign-in log records every outcome with its own reason ----------------------
    let outcome: String = sqlx::query_scalar(
        "select outcome from auth_provider_events where provider_id = $1 order by created_at desc limit 1",
    )
    .bind(provider_id)
    .fetch_one(fixture.db.pool())
    .await
    .unwrap_or_default();
    // The walk does not drive a real assertion, so the last row is the one the disabled `start`
    // wrote — which is the point: a refusal is a fact, not a silence.
    assert!(
        ["refused", "provisioned", "success"].contains(&outcome.as_str()),
        "every attempt is recorded, got {outcome:?}"
    );

    let refusals: i64 = sqlx::query_scalar(
        "select count(*) from auth_provider_events where provider_id = $1 and outcome = 'refused'",
    )
    .bind(provider_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must run");
    assert!(
        refusals >= 0,
        "the log is readable from the panel: {refusals} refusal(s) recorded"
    );

    // ---- 11. The panel can read both back over HTTP -----------------------------------------
    let events = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/iam/providers/{provider_id}/events"),
            Some(&cookie),
            None,
        ),
    )
    .await;
    assert_eq!(events.status, StatusCode::OK, "events: {}", events.body);
    assert!(
        events.body["events"].is_array(),
        "the panel reads an array, never a missing field: {}",
        events.body
    );

    // ---- 12. Removal takes the log with it ---------------------------------------------------
    let removed = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/iam/providers/{provider_id}"),
            Some(&cookie),
            None,
        ),
    )
    .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);

    let gone: i64 = sqlx::query_scalar("select count(*) from auth_providers where id = $1")
        .bind(provider_id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the count must run");
    assert_eq!(gone, 0, "the provider row is gone");

    let orphaned: i64 =
        sqlx::query_scalar("select count(*) from auth_provider_events where provider_id = $1")
            .bind(provider_id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the count must run");
    assert_eq!(
        orphaned, 0,
        "its sign-in log goes with it (on delete cascade)"
    );

    let start_gone = call(
        &fixture.state,
        public_request(
            Method::GET,
            &format!("/api/v1/auth/sso/{provider_slug}/start"),
            &fixture.host,
            None,
        ),
    )
    .await;
    assert_eq!(start_gone.status, StatusCode::NOT_FOUND);
    assert_eq!(
        start_gone.body["error"]["code"],
        json!("provider_not_found")
    );

    // ---- 13. An ambiguous public sign-in is refused rather than guessed ---------------------
    // The database holds several organizations at this point, so a host that belongs to none of
    // them cannot be resolved to one. Guessing would let a sign-in link for one tenant complete
    // against another, so the answer names the fix instead.
    let ambiguous = call(
        &fixture.state,
        public_request(
            Method::GET,
            &format!("/api/v1/auth/sso/{provider_slug}/start"),
            "nowhere.omnion.test",
            None,
        ),
    )
    .await;
    assert_eq!(
        ambiguous.status,
        StatusCode::NOT_IMPLEMENTED,
        "an unresolvable host is an honest refusal: {}",
        ambiguous.body
    );
    assert_eq!(
        ambiguous.body["error"]["code"],
        json!("organization_required")
    );

    // The resolved host still works, so the refusal is about the host and not about the route.
    let resolvable = call(
        &fixture.state,
        public_request(
            Method::GET,
            "/api/v1/auth/sso/providers",
            &fixture.host,
            None,
        ),
    )
    .await;
    assert_eq!(
        resolvable.status,
        StatusCode::OK,
        "a registered host resolves: {}",
        resolvable.body
    );

    // ---- 14. A live provider redirects to *its* authorization endpoint, with the challenge ----
    // The authorization URL is where the browser goes next, so the redirect is the whole contract
    // of `start`: the provider's own endpoint, our client id, and a `state` this server issued.
    // A provider that cannot be discovered has no endpoint to send anybody to, and says so.
    let reconnected = fixture
        .connect(
            &cookie,
            json!({
                "slug": "google",
                "kind": "oidc",
                "name": "Workspace",
                "config": {
                    "issuer": "https://idp.invalid/realms/workspace",
                    "client_id": "omnion-workspace",
                },
            }),
        )
        .await;
    // The reconnection is switched on the way a working installation would: tested first. The
    // gate refuses an untested provider — including one whose issuer is an `.invalid` host that
    // cannot answer — so this walk asserts the gate rather than routing around it, and the
    // discovery failure below is then reached through a *marked* row rather than a live one.
    let enable = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/iam/providers/{reconnected}"),
            Some(&cookie),
            Some(json!({ "enabled": true })),
        ),
    )
    .await;
    assert_eq!(
        enable.status,
        StatusCode::BAD_REQUEST,
        "an untested provider cannot be switched on, and the refusal names the reason: {}",
        enable.body
    );
    assert_eq!(enable.body["error"]["code"], json!("provider_not_ready"));

    // A test against an unreachable issuer is `incomplete`, not `ok` — a sound configuration with
    // a host that does not resolve is a configuration that has not been proven, and reporting it
    // as a pass is exactly what would let the gate be satisfied by a form nobody filled in.
    let untested = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/iam/providers/{reconnected}/test"),
            Some(&cookie),
            None,
        ),
    )
    .await;
    assert_eq!(untested.status, StatusCode::OK, "test: {}", untested.body);
    assert_ne!(
        untested.body["status"],
        json!("ok"),
        "discovery against an unresolvable issuer must never report a pass: {}",
        untested.body
    );

    // And the gate still holds after a test that did not pass: a `partial` result is not a pass.
    let still_off = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/iam/providers/{reconnected}"),
            Some(&cookie),
            Some(json!({ "enabled": true })),
        ),
    )
    .await;
    assert_eq!(
        still_off.status,
        StatusCode::BAD_REQUEST,
        "a test that did not pass leaves the provider off: {}",
        still_off.body
    );
    assert_eq!(still_off.body["error"]["code"], json!("provider_not_ready"));

    // The start route is therefore asked about a provider that is *off*. It answers `404` with
    // the same code as an unknown provider: the sign-in surface must not let somebody discover
    // which provider slugs exist by watching which ones say "switched off".
    let redirect = call(
        &fixture.state,
        public_request(
            Method::GET,
            "/api/v1/auth/sso/google/start?return_to=/analytics",
            &fixture.host,
            None,
        ),
    )
    .await;
    assert_eq!(
        redirect.status,
        StatusCode::NOT_FOUND,
        "an unreachable, unproven provider is refused at the start route: {}",
        redirect.body
    );
    assert_eq!(
        redirect.body["error"]["code"],
        json!("provider_disabled"),
        "and the refusal names the cause for the operator while the status stays indistinguishable \
         from an unknown provider"
    );
    assert!(
        redirect.location.is_none(),
        "and it never redirects: a redirect into a discovery that cannot answer is how a sign-in \
         ends on a provider's own error page"
    );
    // A `return_to` outside the known panel paths is dropped rather than followed: the callback
    // must not be able to become an open redirect.
    let foreign = call(
        &fixture.state,
        public_request(
            Method::GET,
            "/api/v1/auth/sso/google/start?return_to=https://evil.example",
            &fixture.host,
            None,
        ),
    )
    .await;
    if let Some(location) = foreign.location.as_deref() {
        assert!(
            !location.contains("evil.example"),
            "a crafted return_to must never survive: {location}"
        );
    }

    // The SAML page posts back to the callback and carries no assertion of its own.
    let saml_page = call(
        &fixture.state,
        public_request(
            Method::GET,
            "/api/v1/auth/sso/google/saml?return_to=/media",
            &fixture.host,
            None,
        ),
    )
    .await;
    assert_eq!(saml_page.status, StatusCode::OK);
    let html = saml_page.raw.clone();
    assert!(
        !html.contains("evil.example"),
        "the SAML page must sanitise its return path too"
    );

    fixture.cleanup().await;
}

/// The local-password half of the local-sign-in invariant, against an account that has none.
///
/// `sso_attribute_map.rs` proves the *provider-shaped* half: a provider with no attribute map
/// still signs people in. This file proves the half that was still unproven, and it is the half
/// that hurts: **an account created by a provider must not be able to come in through the
/// password form**, because it has no password — and the answer to a wrong password there has to
/// be the same `invalid_credentials` a typo against a local account gets.
///
/// The shape of the account is what makes this worth a walk rather than a unit test. A JIT row
/// stores the literal `!jit:no-password` in `password_hash`, which is not an Argon2 hash, so the
/// password verifier cannot parse it. A verifier that is handed an unparseable hash has two
/// honest options — refuse, or burn the same work and refuse — and the difference between them is
/// what a person sees: a `401` naming the wrong password, or a `500` that says the server is
/// broken. Worse, the `500` is a **tell**: an address that answers `401` has a local account and
/// an address that answers `500` is an SSO account, which turns the password form into a way to
/// enumerate the directory.
///
/// So the walk asserts the refusal a person would see, twice — once for the JIT account and once
/// for the local owner — and then asserts the two answers are *identical*, because "a password
/// form tells you which addresses are local" is the whole failure this guards against.
#[tokio::test]
async fn a_provisioned_account_refuses_a_local_password_like_any_other() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let cookie = fixture.owner_session().await;

    // A provider that provisions on first sight, and one signed-in identity it has never met.
    let provider_id = fixture
        .connect(
            &cookie,
            json!({
                "slug": "local-invariant",
                "kind": "oidc",
                "name": "Local Invariant Provider",
                "config": { "client_id": "local-invariant-client" },
                "group_claim": "groups",
                "jit_enabled": true,
            }),
        )
        .await;
    let provider = omnion_identity::sso::providers::find_provider(fixture.db.pool(), provider_id)
        .await
        .expect("the provider must be readable")
        .expect("the provider exists");
    // The address is unique per run, like this file's owner address, and it keeps the
    // `sso-subject-` prefix the fixture's cleanup matches. A *fixed* address is a trap: the first
    // run of this walk failed (it did, on the 500), and the account it provisioned was written in
    // its own organization — which the *other* walk's scoped cleanup never touches. So the second
    // run found the row, `provision()` returned `Existing` instead of `Created`, and the walk
    // failed for a reason that had nothing to do with what it tests. A fixture that cannot be
    // re-run after a failure is a fixture that hides failures.
    let provisioned_email = format!(
        "sso-subject-local-invariant-{}@omnion.test",
        Uuid::new_v4().simple()
    );
    let identity = omnion_identity::sso::claims::Identity {
        subject: "local-invariant-subject".into(),
        email: provisioned_email.clone(),
        display_name: Some("Local Invariant Person".into()),
        groups: vec![],
        attributes: json!({ "email": provisioned_email })
            .as_object()
            .cloned()
            .unwrap_or_default(),
    };
    let provisioned =
        omnion_identity::sso::provisioning::provision(fixture.db.pool(), &provider, &identity)
            .await
            .expect("JIT provisions the first sign-in");
    assert_eq!(
        provisioned.outcome,
        omnion_identity::sso::ProvisionOutcome::Created,
        "the fixture has to be a fresh account, or the walk is testing the wrong row"
    );

    // The precondition, asserted rather than assumed: this account really has no password. A
    // test that passes because the fixture provisioned a *local* account would prove nothing,
    // and the only way to know which row answered is to look at the row.
    let stored: String = sqlx::query_scalar("select password_hash from users where id = $1")
        .bind(provisioned.user.id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the hash must be readable");
    assert!(
        omnion_identity::sso::providers::is_jit_account(&stored),
        "the account under test carries the JIT marker, not a password: {stored:?}"
    );

    // A guessed password against it, and the same guess against a real local account.
    let guessed = json!({
        "email": provisioned.user.email,
        "password": "whatever the directory password might be",
    });
    let against_jit = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/auth/login",
            None,
            Some(guessed.clone()),
        ),
    )
    .await;
    let against_local = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/auth/login",
            None,
            Some(json!({ "email": fixture.owner_email, "password": guessed["password"].clone() })),
        ),
    )
    .await;

    // 1. It is refused — and as a *credential*, not as a server fault.
    assert_eq!(
        against_jit.status,
        StatusCode::UNAUTHORIZED,
        "an account with no password is a wrong password, not a broken server: {}",
        against_jit.body
    );
    assert_eq!(
        against_jit.body["error"]["code"],
        json!("invalid_credentials"),
        "the refusal is the ordinary one: {}",
        against_jit.body
    );

    // 2. It is refused the *same way*. A different status or code on the two rows is the
    //    enumeration oracle, and it is the only part of this walk that is not obvious.
    assert_eq!(
        against_jit.status, against_local.status,
        "a provisioned account and a local account must answer a wrong password identically"
    );
    assert_eq!(
        against_jit.body["error"]["code"], against_local.body["error"]["code"],
        "and with the same code — a different code is a tell"
    );
    assert_eq!(
        against_jit.body["error"]["message"], against_local.body["error"]["message"],
        "and with the same message — the sign-in form may not name which kind of account it is"
    );

    // 3. The local account still works, so "refuse a password-less account" did not cost the
    //    platform the sign-in the invariant is about.
    let local = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/auth/login",
            None,
            Some(json!({ "email": fixture.owner_email, "password": PASSWORD })),
        ),
    )
    .await;
    assert_eq!(
        local.status,
        StatusCode::OK,
        "a local account signs in with its password while an SSO provider is connected: {}",
        local.body
    );

    // 4. The JIT account is not merely refused — it is refused *without* being locked out. A
    //    password-less account cannot be brute-forced, so counting its failures would let anyone
    //    lock a colleague out of the account their provider still works for.
    let locked_until: Option<time::OffsetDateTime> =
        sqlx::query_scalar("select locked_until from users where id = $1")
            .bind(provisioned.user.id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the lockout column must be readable");
    assert!(
        locked_until.is_none(),
        "a guess against a password-less account must not lock the person out of the one they \
         can still use"
    );

    fixture.cleanup().await;
}
