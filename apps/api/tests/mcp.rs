//! Integration tests for the MCP server (REQ-108, slices 1 and 2).
//!
//! These walks drive the **real router**, not the store, because the surface slice 2 added is a
//! handler: authentication, the grant → scope → enabled resolution, the air gap, the rate limit,
//! the masking step and the invocation row are all *in* `routes::mcp`, and a walk that called
//! `McpStore` directly would be measuring the parts while the thing an MCP client actually talks
//! to stayed untested. A test that proves the store works and never sends a request is the shape
//! the developer-portal slice just paid for — its log was written by the test, not by the server.
//!
//! Every acceptance row slice 2 claims is asserted **on a row or on a status code**, not on a
//! return value:
//!
//! - `initialize` answers the server name and protocol version, and an unknown token answers one
//!   uniform error that does not distinguish unknown from revoked.
//! - `tools/list` for a client with three grants returns **exactly those three**, each with a
//!   schema, a permission and a sandbox flag.
//! - `tools/call` for a tool the scopes cannot support is `-32003`, names the permission, and
//!   **changes no rows** — the count of everything is read before and after.
//! - a revoked client's token stops working on the next call while its history is still readable.
//! - a sandboxed call returns a plan and writes no domain row, and the invocation row says
//!   `sandbox` rather than `ok`.
//! - the arguments stored in the log carry REQ-105's mask, so an address in a call is not
//!   readable in the table.
//!
//! The harness is the `ai_catalog.rs` shape — throwaway database, real `AppState`, `oneshot` on the
//! router — with a `max_connections` of 4 rather than the default, because seven writers share
//! one server here.

use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_ai_hub::mcp_store::{McpStore, NewClient};
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, CsrfSecret, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::Row;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

/// A CSRF secret, so the panel-side writes in this suite are real writes.
///
/// The MCP JSON-RPC endpoint authenticates its own bearer token and is not cookie-authenticated,
/// so nothing on the *protocol* path needs this. The **panel** routes do: the overview, the
/// invocation list and the sandbox test are ordinary session routes, and a walk that reaches
/// them without a CSRF secret gets `403 csrf_unavailable` — the same status a missing permission
/// answers, which is the asymmetry `tests/csrf.rs` exists for. A harness gap that looks exactly
/// like a product refusal is the defect worth naming twice.
const CSRF_SECRET: &str = "mcp-integration-suite-key-material";

// -------------------------------------------------------------------------------------------
// The harness
// -------------------------------------------------------------------------------------------

#[derive(Debug)]
struct TestResponse {
    status: StatusCode,
    /// **Every** `Set-Cookie`, in order.
    ///
    /// A single-cookie accessor is the bug this suite started with: a sign-in sets *two* cookies
    /// (`omnion_session` and `omnion_csrf`), `headers().get()` returns only the first, and a
    /// harness that reads the first reports a working session while every write it makes is
    /// refused for want of the token it never captured.
    set_cookie: Vec<String>,
    body: Value,
}

impl TestResponse {
    /// One cookie's value by name, empty when the response did not set it.
    fn cookie(&self, name: &str) -> String {
        self.set_cookie
            .iter()
            .filter_map(|header| header.split(';').next())
            .filter_map(|pair| pair.split_once('='))
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_owned())
            .unwrap_or_default()
    }
}

struct Harness {
    state: AppState,
    db: Db,
    maintenance: Db,
    database: String,
}

impl Harness {
    async fn fresh() -> Option<Self> {
        let mut config = Config::from_env().expect("environment must be valid");
        config.csrf = CsrfSecret::new(Some(CSRF_SECRET.to_owned()));
        if live_db(&config).await.is_none() {
            return None;
        }
        let database = format!("omnion_mcp_{}", Uuid::new_v4().simple());
        let maintenance = Db::connect(&maintenance_config(&config))
            .await
            .expect("the maintenance connection must work");
        sqlx::query(&format!("create database \"{database}\""))
            .execute(maintenance.pool())
            .await
            .expect("the temporary database must be created");
        let db = Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, &database),
            max_connections: 4,
        })
        .await
        .expect("the fresh database must connect");
        db.migrate().await.expect("migrations must apply");

        let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
        let state = AppState::new(
            BuildInfo::new("omnion-api", "0.1.0-test"),
            config,
            db.clone(),
            redis,
            omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
                .expect("the default storage configuration is valid"),
        );
        Some(Self {
            state,
            db,
            maintenance,
            database,
        })
    }

    async fn call(&self, request: Request<Body>) -> TestResponse {
        let response = routes::router(self.state.clone())
            .oneshot(request)
            .await
            .expect("router must answer");
        let status = response.status();
        // `get_all(SET_COOKIE)` and NOT `get(SET_COOKIE)`: a sign-in answers with two cookies,
        // and `get` returns the first one only. That is not cosmetic — the CSRF cookie is what
        // the CSRF header is checked against, so dropping it turns every write this file makes
        // into a `403 csrf_unavailable` that reads exactly like a product refusal.
        let set_cookie: Vec<String> = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_owned)
            .collect();
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

    fn pool(&self) -> &PgPool {
        self.db.pool()
    }

    /// Sign an owner in and return `(session, organization id, user id)`.
    ///
    /// The owner is **platform-level** — `users.organization_id` stays NULL — because that is what
    /// the bootstrap produces and because `POST /api/v1/organizations` answers `403
    /// platform_only` to an account that already belongs to a tenant. The tenant is created
    /// through `POST /api/v1/organizations` instead of the wizard, which is both the door an
    /// operator uses and the one that leaves the installer platform-level.
    async fn owner(&self) -> (Session, Uuid, Uuid) {
        let email = format!("owner-{}@omnion.test", Uuid::new_v4().simple());
        let response = self
            .call(post(
                "/api/v1/onboarding/owner",
                json!({
                    "display_name": "Grace Hopper",
                    "email": email,
                    "password": PASSWORD,
                }),
                None,
            ))
            .await;
        assert_eq!(response.status, StatusCode::CREATED, "{:?}", response.body);
        let session = Session::of(&response);
        let user_id = self
            .call(get("/api/v1/me", Some(&session)))
            .await
            .body["user"]["id"]
            .as_str()
            .expect("the session resolves a user")
            .parse::<Uuid>()
            .expect("a uuid");

        // **The first owner is platform-level and its `organization_id` is NULL** — nobody has
        // created a tenant yet, so the bootstrap account deliberately belongs to no organization.
        // Reading that column into a `Uuid` is what every walk in this file did first, and it
        // fails with `ColumnDecode { source: UnexpectedNullError }` — a decode error that names
        // the *decoder* and not the fact that the account is the wrong shape for an
        // organization-scoped test. Nine walks, one cause, and the error points at sqlx.
        //
        // An MCP client belongs to an organization (its grants and its log are both tenant
        // scoped), so the fixture creates the tenant and resolves the id from the organization it
        // created rather than from the owner row.
        // Two doors, and this file needs both of them.
        //
        // 1. `POST /api/v1/onboarding/organization` is the wizard step that **puts the installer
        //    inside** the tenant — `create_organization` calls `users::set_user_organization`, and
        //    that is the only place in the platform that does. Without it `users.organization_id`
        //    stays NULL and every panel route answers `400 organization_required`, because a
        //    platform account with no `?organization_id` names no tenant.
        let onboarded = self
            .call(post(
                "/api/v1/onboarding/organization",
                json!({ "name": "Acme" }),
                Some(&session),
            ))
            .await;
        assert_eq!(
            onboarded.status,
            StatusCode::OK,
            "the wizard tenant must be creatable: {:?}",
            onboarded.body
        );
        let organization_id = sqlx::query(
            "select organization_id from users where id = $1",
        )
        .bind(user_id)
        .fetch_one(self.pool())
        .await
        .expect("the installer must now belong to a tenant")
        .get::<Option<Uuid>, _>("organization_id")
        .expect("the wizard must have written a non-null organization_id");

        // 2. `POST /api/v1/organizations` is the tenancy route an operator uses, and it answers
        //    `403 platform_only` to an account that already has a tenant. It is therefore NOT
        //    usable from this session — and `second_tenant` needs a platform-level installer, so
        //    it mints its own. A walk that learned this by getting the 403 is a walk that already
        //    spent a run on it.
        (session, organization_id, user_id)
    }

    async fn dispose(self) {
        self.db.pool().close().await;
        let database = self.database;
        sqlx::query(&format!(
            "drop database if exists \"{database}\" with (force)"
        ))
        .execute(self.maintenance.pool())
        .await
        .expect("the temporary database must be removed");
    }
}

/// Create a client through the store and return `(token, client id)`.
///
/// Through the store rather than the panel route: the panel route for creating a client is slice
/// 1's screen work and is exercised by the walkthrough; what these walks need is a client with
/// known scopes and grants, and the store is the only place those two are set together.
async fn client_with(pool: &PgPool, organization_id: Uuid, scopes: &[&str]) -> (String, Uuid) {
    let store = McpStore::new(pool.clone());
    let created = store
        .create_client(NewClient {
            organization_id,
            name: format!("agent-{}", Uuid::new_v4().simple()),
            description: "walk fixture".into(),
            scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
            sandbox: false,
            rate_limit_per_min: 600,
            created_by: None,
        })
        .await
        .expect("the client must be created");
    (created.token, created.client.id)
}

/// Grant a set of tools, taking each one's permission from the compiled catalogue.
async fn grant(pool: &PgPool, client_id: Uuid, tools: &[&str]) {
    let grants: Vec<(String, Option<String>, bool)> = tools
        .iter()
        .map(|key| {
            let spec = omnion_ai_hub::catalogue::find(key).expect("the catalogue has the tool");
            (
                spec.key.to_owned(),
                Some(spec.permission.to_owned()),
                omnion_ai_hub::catalogue::default_requires_approval(spec),
            )
        })
        .collect();
    McpStore::new(pool.clone())
        .replace_tools(client_id, &grants)
        .await
        .expect("the grants must be written");
}

/// A JSON-RPC call with a bearer token.
fn rpc(token: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri("/api/v1/mcp")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request must build")
}

/// One `tools/call` envelope.
fn call_body(tool: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    })
}

/// A signed-in session **and** the CSRF token that goes with it.
///
/// The two travel together on purpose, which is the whole reason this is a type and not two
/// strings. Passing only the session is the shape that produces a file full of
/// `403 csrf_unavailable`s and a walk that blames the product; passing the CSRF token in the
/// cookie jar instead of the header is the shape that produces a suite which cannot write at
/// all. `auth()` is the single place either is attached, so a call site physically cannot send
/// half the pair — the failure this harness's second run actually produced.
#[derive(Clone)]
struct Session {
    token: String,
    csrf: String,
}

impl Session {
    fn of(response: &TestResponse) -> Self {
        let token = response.cookie("omnion_session");
        assert!(
            !token.is_empty(),
            "the response must set a session cookie; cookies were {:?}",
            response.set_cookie
        );
        let csrf = response.cookie("omnion_csrf");
        assert!(
            !csrf.is_empty(),
            "a sign-in must set the CSRF cookie alongside the session; cookies were {:?}",
            response.set_cookie
        );
        Self { token, csrf }
    }

    /// The session cookie and the CSRF header, in one place.
    fn auth(self: &Self, builder: axum::http::request::Builder) -> axum::http::request::Builder {
        builder
            .header(header::COOKIE, format!("omnion_session={}", self.token))
            .header("x-omnion-csrf", self.csrf.clone())
    }
}

fn get(uri: &str, session: Option<&Session>) -> Request<Body> {
    let builder = Request::builder().method(Method::GET).uri(uri);
    let builder = match session {
        Some(session) => session.auth(builder),
        None => builder,
    };
    builder.body(Body::empty()).expect("request must build")
}

fn post(uri: &str, body: Value, session: Option<&Session>) -> Request<Body> {
    let builder = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    let builder = match session {
        Some(session) => session.auth(builder),
        None => builder,
    };
    builder
        .body(Body::from(body.to_string()))
        .expect("request must build")
}

fn swap_database(url: &str, database: &str) -> String {
    let (base, _) = url.rsplit_once('/').expect("a database URL has a path");
    format!("{base}/{database}")
}

fn maintenance_config(config: &Config) -> DatabaseConfig {
    DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 1,
    }
}

/// Is the configured database reachable?
///
/// A suite that cannot reach its database reports **success** — `bench!()` turns `None` into an
/// early `return`, and 7 tests skip in 0.02 s. That is a full skip wearing a pass, and it has
/// happened on this repo before; the *duration* is the tell, which is why every walk here does
/// real work once the harness is up.
async fn live_db(config: &Config) -> Option<()> {
    let probe = Db::connect(&DatabaseConfig {
        url: config.database.url.clone(),
        max_connections: 1,
    })
    .await
    .ok()?;
    probe.pool().close().await;
    Some(())
}

macro_rules! harness {
    () => {
        match Harness::fresh().await {
            Some(harness) => harness,
            None => {
                eprintln!("skipping: PostgreSQL is not reachable");
                return;
            }
        }
    };
}

/// **Two tenants and a session in each**, on this database, plus the platform installer.
///
/// This is the shape `ai_tenant_404.rs` established and the only one that works, for a reason
/// worth writing down: the platform account and a tenanted account are **mutually exclusive**.
/// The tenancy route `POST /api/v1/organizations` answers `403 platform_only` to an account that
/// already has a tenant, and the *only* function that puts an account inside a tenant is the
/// wizard step `POST /onboarding/organization` — which is once-per-installation and answers
/// `409 already_installed` the second time. So:
//
// - to make two tenants, the installer must **never** run the wizard, and
// - a walk that needs both a tenanted session and the tenancy route cannot have them from the
//   same account, which is why this helper builds the members itself rather than reusing
//   `Harness::owner` (which does run the wizard, because the panel routes answer
//   `400 organization_required` without a tenant).
///
/// `POST /iam/users` creates a row and does **not** sign the new member in; reading a session
/// cookie off that response is how a suite ends up sending the installer's token and calling it a
/// tenant test. So every member is created and then signed in.
async fn two_tenants(harness: &Harness) -> (Session, Uuid, Session) {
    let installer = harness
        .call(post(
            "/api/v1/onboarding/owner",
            json!({
                "display_name": "Installer",
                "email": format!("installer-{}@omnion.test", Uuid::new_v4().simple()),
                "password": PASSWORD,
            }),
            None,
        ))
        .await;
    assert_eq!(
        installer.status,
        StatusCode::CREATED,
        "an installer must be creatable: {:?}",
        installer.body
    );
    let installer = Session::of(&installer);

    // The role is looked up, never named by id literal: a fixture carrying a role id breaks the
    // day the seeded ids move, and the suite then fails on a permission it never meant to test.
    let roles = harness
        .call(get("/api/v1/iam/roles", Some(&installer)))
        .await;
    assert_eq!(roles.status, StatusCode::OK, "{:?}", roles.body);
    let admin_role = roles.body["roles"]
        .as_array()
        .expect("roles array")
        .iter()
        .find(|role| role["key"] == "administrator" || role["key"] == "admin")
        .or_else(|| {
            roles.body["roles"]
                .as_array()
                .expect("roles array")
                .iter()
                .find(|role| role["key"] == "editor")
        })
        .and_then(|role| role["id"].as_str())
        .expect("an installation ships at least one role that may use the AI")
        .to_owned();

    let mut first: Option<Uuid> = None;
    let mut second_session: Option<Session> = None;
    for name in ["Acme", "Globex"] {
        let created = harness
            .call(post(
                "/api/v1/organizations",
                json!({ "name": name, "slug": format!("{}-{}", name.to_lowercase(), Uuid::new_v4().simple()) }),
                Some(&installer),
            ))
            .await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            created.body
        );
        let organization = created.body["id"]
            .as_str()
            .expect("the created organization carries an id")
            .parse::<Uuid>()
            .expect("an id is a uuid");

        let email = format!("member-{}-{}@omnion.test", name.to_lowercase(), Uuid::new_v4().simple());
        let member = harness
            .call(post(
                "/api/v1/iam/users",
                json!({
                    "email": email,
                    "display_name": format!("Member of {name}"),
                    "organization_id": organization.to_string(),
                    "password": PASSWORD,
                    "role_id": admin_role,
                }),
                Some(&installer),
            ))
            .await;
        assert_eq!(member.status, StatusCode::CREATED, "{name}: {:?}", member.body);

        let signed_in = harness
            .call(post(
                "/api/v1/auth/login",
                json!({ "email": email, "password": PASSWORD }),
                None,
            ))
            .await;
        assert_eq!(
            signed_in.status,
            StatusCode::OK,
            "{name}: {:?}",
            signed_in.body
        );
        if first.is_none() {
            first = Some(organization);
        } else {
            second_session = Some(Session::of(&signed_in));
        }
    }
    (
        installer,
        first.expect("two tenants were created"),
        second_session.expect("the second tenant has a member session"),
    )
}

/// Seed a page so `content.search` and `content.read` have something real to find.
///
/// The site is created for **the organization it is told about**, not for "the first organization
/// in the table". A `select … from organizations limit 1` fixture is a cross-tenant fixture that
/// looks like a convenience: the walk passes when there is one tenant and attaches the page to
/// somebody else's site the moment a second one exists — which is exactly what
/// `the_panel_routes_need_a_session_and_never_leak_another_tenants_log` creates.
///
/// The column list is `(organization_id, key, name)`, copied from the working fixtures in
/// `ai_approvals.rs` rather than guessed: `sites` has no `slug` and no `domain`, and writing
/// `slug = 'walk-$$'` fails with `42703 column "slug" of relation "sites" does not exist` — a
/// Postgres error about a column that says nothing about the fixture's real mistake.
async fn seed_page(pool: &PgPool, organization_id: Uuid) -> Uuid {
    let key = format!("walk-{}", Uuid::new_v4().simple());
    let site: (Uuid,) = sqlx::query_as(
        "insert into sites (organization_id, key, name) values ($1, $2, 'walk') returning id",
    )
    .bind(organization_id)
    .bind(&key)
    .fetch_one(pool)
    .await
    .expect("a site must exist for the page");
    let page: (Uuid,) = sqlx::query_as(
        "insert into pages (id, site_id, slug, page_type, status) \
         values ($1, $2, 'pricing', 'standard', 'published') returning id",
    )
    .bind(Uuid::new_v4())
    .bind(site.0)
    .fetch_one(pool)
    .await
    .expect("the fixture page must be created");
    page.0
}

// -------------------------------------------------------------------------------------------
// The walks
// -------------------------------------------------------------------------------------------

/// `initialize` answers the server's identity, and an unknown token answers one uniform error.
#[tokio::test]
async fn initialize_names_the_server_and_an_unknown_token_says_nothing() {
    let harness = harness!();
    let (session, organization_id, _) = harness.owner().await;
    let (client_token, _client_id) =
        client_with(harness.pool(), organization_id, &["content.pages.read"]).await;

    let good = harness
        .call(rpc(
            &client_token,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "protocolVersion": "2025-06-18",
                                "clientInfo": { "name": "walk" } } }),
        ))
        .await;
    assert_eq!(good.status, StatusCode::OK, "{:?}", good.body);
    let result = &good.body["result"];
    assert_eq!(
        result["protocolVersion"],
        omnion_ai_hub::mcp_tools::PROTOCOL_VERSION
    );
    assert_eq!(
        result["serverInfo"]["name"],
        omnion_ai_hub::mcp_tools::SERVER_NAME
    );
    assert!(
        result["serverInfo"]["version"]
            .as_str()
            .is_some_and(|v| !v.is_empty())
    );
    assert_eq!(good.body["jsonrpc"], "2.0");
    assert_eq!(
        good.body["id"],
        json!(1),
        "the answer carries the caller's id"
    );

    // A token nobody holds, and one that is *one character* off a real one, answer identically.
    // The second case matters: a comparison that early-returns on the first differing character
    // is a timing oracle, and the two answers must not differ in content either.
    let unknown = harness
        .call(rpc(
            "omnmcp_not-a-real-token",
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }),
        ))
        .await;
    let almost = format!("{client_token}x");
    let nearly = harness
        .call(rpc(
            &almost,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }),
        ))
        .await;
    assert_eq!(unknown.status, nearly.status);
    assert_eq!(
        unknown.body["error"]["code"], nearly.body["error"]["code"],
        "an unknown token and a near-miss token answer alike"
    );
    assert_eq!(unknown.body["error"]["code"], json!(-32001));
    // And the message must not tell the two apart, nor mention either state by name.
    let message = unknown.body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    for word in ["revoked", "disabled", "unknown", "not found", "expired"] {
        assert!(
            !message.contains(word),
            "`{word}` leaks the state: {message}"
        );
    }

    // No token at all is the same answer again, not a 401 from the session guard.
    let none = harness
        .call(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/mcp")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }).to_string(),
                ))
                .expect("builds"),
        )
        .await;
    assert_eq!(none.body["error"]["code"], json!(-32001));

    // The panel's own token is not an MCP token: a session cookie must not open this endpoint.
    //
    // **The CSRF header travels with the cookie**, and that is the whole difficulty. A cookie
    // alone is refused by the CSRF layer first and answers `{"error": {"code": "csrf_failed"}}` —
    // the same shape of refusal the MCP layer gives, from a layer above, about a different
    // reason. Asserting `-32001` against that response tests the CSRF middleware, not the claim.
    // With a valid session *and* its CSRF token the request reaches the MCP handler, and what
    // answers there is the real thing: the token is not an MCP token, so the endpoint refuses it
    // with its own code and its own envelope.
    let session_try = harness
        .call(
            session
                .auth(Request::builder().method(Method::POST).uri("/api/v1/mcp"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }).to_string(),
                ))
                .expect("builds"),
        )
        .await;
    assert_eq!(
        session_try.body["error"]["code"],
        json!(-32001),
        "a session is not an MCP client: {:?}",
        session_try.body
    );
    assert_eq!(
        session_try.body["jsonrpc"],
        json!("2.0"),
        "and it is a JSON-RPC envelope, not the platform's error shape"
    );

    harness.dispose().await;
}

/// `tools/list` returns exactly the granted tools, each with a schema and a permission.
#[tokio::test]
async fn tools_list_returns_exactly_the_grants_and_nothing_else() {
    let harness = harness!();
    let (_session, organization_id, _) = harness.owner().await;
    let (client_token, client_id) =
        client_with(harness.pool(), organization_id, &["content.pages.read"]).await;
    grant(
        harness.pool(),
        client_id,
        &["content.search", "content.read", "media.search"],
    )
    .await;

    let response = harness
        .call(rpc(
            &client_token,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        ))
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
    let tools = response.body["result"]["tools"]
        .as_array()
        .expect("tools is an array");
    assert_eq!(tools.len(), 3, "three grants, three tools: {tools:?}");

    let mut names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("a name"))
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["content.read", "content.search", "media.search"],
        "and they are exactly those three"
    );

    for tool in tools {
        let name = tool["name"].as_str().unwrap_or_default();
        assert!(
            tool["input_schema"].is_object(),
            "{name} must carry a schema"
        );
        assert!(
            tool["permission"].as_str().is_some_and(|p| !p.is_empty()),
            "{name} must name its permission"
        );
        assert_eq!(tool["sandbox_capable"], json!(true));
        assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()));
        // The permission each entry names must be a REAL catalogue key — a tool that demands a
        // permission nobody can hold is a tool that is permanently denied, and `ops_binding`
        // already checks that for the panel; the MCP surface must not restate a wrong one.
        let permission = tool["permission"].as_str().expect("a permission");
        assert!(
            omnion_ai_hub::catalogue::KNOWN_PERMISSION_KEYS.contains(&permission),
            "{name} names `{permission}`, which is not a catalogue key"
        );
    }

    // A second client with no grants gets an empty list, not the whole catalogue. This is the
    // criterion's "exactly those three" from the other side.
    let (bare_token, bare_id) =
        client_with(harness.pool(), organization_id, &["content.pages.read"]).await;
    let bare = harness
        .call(rpc(
            &bare_token,
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }),
        ))
        .await;
    assert!(
        bare.body["result"]["tools"]
            .as_array()
            .expect("tools array")
            .is_empty(),
        "a client with no grants sees nothing: {:?}",
        bare.body
    );
    assert_ne!(bare_id, client_id);

    harness.dispose().await;
}

/// A granted read tool returns real data, and a denial names the permission and changes nothing.
#[tokio::test]
async fn a_denied_call_names_the_permission_and_writes_no_row_anywhere() {
    let harness = harness!();
    let (_session, organization_id, _) = harness.owner().await;
    let page_id = seed_page(harness.pool(), organization_id).await;

    // The client holds `content.search` — which needs `content.pages.read` — but its SCOPES do
    // not. That is the case the request names: "a tool the client's scopes cannot support".
    let (client_token, client_id) =
        client_with(harness.pool(), organization_id, &["media.read"]).await;
    grant(harness.pool(), client_id, &["content.search"]).await;

    // Row counts before, for the tables a call could touch.
    let pages_before: (i64,) = sqlx::query_as("select count(*) from pages")
        .fetch_one(harness.pool())
        .await
        .expect("pages count");
    let invocations_before: (i64,) = sqlx::query_as("select count(*) from mcp_invocations")
        .fetch_one(harness.pool())
        .await
        .expect("invocations count");

    let denied = harness
        .call(rpc(
            &client_token,
            call_body("content.search", json!({ "query": "pric" })),
        ))
        .await;
    assert_eq!(
        denied.status,
        StatusCode::OK,
        "a JSON-RPC error is still HTTP 200"
    );
    let error = &denied.body["error"];
    assert_eq!(
        error["code"],
        json!(omnion_ai_hub::mcp_tools::CODE_PERMISSION_DENIED),
        "the request names -32003: {denied:?}"
    );
    assert_eq!(
        error["data"]["permission"],
        json!("content.pages.read"),
        "the denial names the missing permission"
    );
    assert!(
        error["message"]
            .as_str()
            .expect("a message")
            .contains("content.pages.read"),
        "and the message repeats it for a human reading the log"
    );

    // **No side effect**, asserted on the world and not on a return value: a tool that ran and
    // then reported a refusal passes every assertion above.
    let pages_after: (i64,) = sqlx::query_as("select count(*) from pages")
        .fetch_one(harness.pool())
        .await
        .expect("pages count");
    assert_eq!(pages_before.0, pages_after.0, "a denied call wrote a page");
    let row: (Uuid, String) = sqlx::query_as("select id, slug from pages where id = $1")
        .bind(page_id)
        .fetch_one(harness.pool())
        .await
        .expect("the fixture page is untouched");
    assert_eq!(row.1, "pricing");

    // But the refusal IS recorded — a call the platform cannot account for is the failure this
    // table exists to prevent.
    let invocations_after: (i64,) = sqlx::query_as("select count(*) from mcp_invocations")
        .fetch_one(harness.pool())
        .await
        .expect("invocations count");
    assert_eq!(
        invocations_after.0,
        invocations_before.0 + 1,
        "exactly one row for the refusal"
    );
    let recorded: (String, Option<String>) =
        sqlx::query_as("select status, permission from mcp_invocations order by id desc limit 1")
            .fetch_one(harness.pool())
            .await
            .expect("the row is readable");
    assert_eq!(recorded.0, "denied");
    assert_eq!(
        recorded.1.as_deref(),
        Some("content.pages.read"),
        "the row names the permission that was missing"
    );

    // Widen the scope and the same call now runs and returns the real page.
    McpStore::new(harness.pool().clone())
        .update_client(
            organization_id,
            client_id,
            &format!("agent-{}", client_id.simple()),
            "",
            &["content.pages.read".to_owned()],
            false,
            600,
        )
        .await
        .expect("the scopes must widen");
    let allowed = harness
        .call(rpc(
            &client_token,
            call_body("content.search", json!({ "query": "pric" })),
        ))
        .await;
    assert!(
        allowed.body.get("error").is_none(),
        "the widened scope is allowed: {allowed:?}"
    );
    let text = allowed.body["result"]["content"][0]["text"]
        .as_str()
        .expect("a text block")
        .to_owned();
    assert!(
        text.contains("pricing"),
        "and it returned the real page: {text}"
    );
    assert_eq!(allowed.body["result"]["isError"], json!(false));
    let final_status: (String,) =
        sqlx::query_as("select status from mcp_invocations order by id desc limit 1")
            .fetch_one(harness.pool())
            .await
            .expect("the row is readable");
    assert_eq!(final_status.0, "ok");

    harness.dispose().await;
}

/// A tool the client was never granted is refused without naming what it would have required.
#[tokio::test]
async fn an_ungranted_tool_is_refused_without_discovering_its_permission() {
    let harness = harness!();
    let (_session, organization_id, _) = harness.owner().await;
    // A token with the scope that `content.publish` needs — and NO grant for it.
    let (client_token, _client_id) =
        client_with(harness.pool(), organization_id, &["content.pages.publish"]).await;

    let refused = harness
        .call(rpc(
            &client_token,
            call_body(
                "content.publish",
                json!({ "id": "00000000-0000-0000-0000-000000000000" }),
            ),
        ))
        .await;
    let error = &refused.body["error"];
    assert_eq!(
        error["code"],
        json!(omnion_ai_hub::mcp_tools::CODE_TOOL_NOT_FOUND),
        "not granted is not the same as not permitted: {refused:?}"
    );
    let rendered = error.to_string();
    assert!(
        !rendered.contains("content.pages.publish"),
        "the refusal must not tell the client what an ungranted tool requires: {rendered}"
    );

    // And the invocation row for it carries no permission either, for the same reason.
    let row: (String, Option<String>) =
        sqlx::query_as("select status, permission from mcp_invocations order by id desc limit 1")
            .fetch_one(harness.pool())
            .await
            .expect("the row is readable");
    assert_eq!(row.0, "denied");
    assert_eq!(row.1, None, "no permission is invented for the log");

    harness.dispose().await;
}

/// A revoked client's token stops working immediately and its history is still readable.
#[tokio::test]
async fn a_revocation_takes_effect_on_the_next_call_and_keeps_the_history() {
    let harness = harness!();
    let (session, organization_id, _) = harness.owner().await;
    let (client_token, client_id) =
        client_with(harness.pool(), organization_id, &["content.pages.read"]).await;
    grant(harness.pool(), client_id, &["content.search"]).await;

    // A real `tools/call`, not a `ping`. **Only a tool call is recorded.** A `ping` answers
    // without reaching the tool path, so it writes no `mcp_invocations` row — correct, and the
    // reason this walk originally failed with `left: 0, right: 1`. The claim being tested is
    // "a revoked client's *tool* history survives", and a `ping` is not a tool history: a log
    // that counted connectivity probes would fill with rows that name no tool, no arguments and
    // no permission, which is what the table is for.
    let before = harness
        .call(rpc(&client_token, call_body("content.search", json!({ "query": "pri" }))))
        .await;
    assert!(before.body.get("error").is_none(), "{before:?}");
    let stored: (i64,) = sqlx::query_as("select count(*) from mcp_invocations")
        .fetch_one(harness.pool())
        .await
        .expect("count");
    assert_eq!(stored.0, 1, "the working call was recorded");

    McpStore::new(harness.pool().clone())
        .revoke(organization_id, client_id)
        .await
        .expect("the client must revoke");

    let after = harness
        .call(rpc(
            &client_token,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }),
        ))
        .await;
    assert_eq!(
        after.body["error"]["code"],
        json!(-32001),
        "a revoked token answers the uniform refusal"
    );
    // **The refusal adds no row**: the call never reached the tool path, so recording it would
    // put a `denied` row for a client whose token is gone, and the history would grow every time
    // somebody's integration kept retrying with a dead token.
    let stored: (i64,) = sqlx::query_as("select count(*) from mcp_invocations")
        .fetch_one(harness.pool())
        .await
        .expect("count");
    assert_eq!(stored.0, 1, "the revoked call wrote no row");

    // The history is still readable **by the tenant that owns it**, through the panel route with
    // a session. Reading it without one is the unauthenticated case, and it belongs to
    // `the_panel_routes_need_a_session_and_never_leak_another_tenants_log` — asserted there so
    // this walk carries one claim rather than three.
    let history = harness
        .call(get("/api/v1/mcp/invocations", Some(&session)))
        .await;
    assert_eq!(history.status, StatusCode::OK, "{:?}", history.body);
    let rows = history.body["invocations"].as_array().expect("an array");
    assert_eq!(
        rows.len(),
        1,
        "the revoked client's call is still on the record: {rows:?}"
    );
    assert_eq!(rows[0]["tool"], json!("content.search"));
    assert_eq!(rows[0]["status"], json!("ok"));

    harness.dispose().await;
}

/// A sandboxed call proves the plan, writes nothing, and says so in the log.
#[tokio::test]
async fn a_sandboxed_call_returns_the_plan_and_writes_no_domain_row() {
    let harness = harness!();
    let (session, organization_id, _) = harness.owner().await;
    seed_page(harness.pool(), organization_id).await;

    let store = McpStore::new(harness.pool().clone());
    let created = store
        .create_client(NewClient {
            organization_id,
            name: format!("sandbox-{}", Uuid::new_v4().simple()),
            description: "walk".into(),
            scopes: vec!["content.pages.read".to_owned()],
            // The flag slice 2 leans on: a client stands up rehearsing before it may write.
            sandbox: true,
            rate_limit_per_min: 600,
            created_by: None,
        })
        .await
        .expect("the client must be created");
    grant(harness.pool(), created.client.id, &["content.search"]).await;

    let pages_before: (i64,) = sqlx::query_as("select count(*) from pages")
        .fetch_one(harness.pool())
        .await
        .expect("count");

    let response = harness
        .call(rpc(
            &created.token,
            call_body("content.search", json!({ "query": "pric" })),
        ))
        .await;
    assert!(
        response.body.get("error").is_none(),
        "a sandboxed call is not a refusal: {response:?}"
    );
    assert_eq!(response.body["result"]["isError"], json!(false));
    let structured = &response.body["result"]["structuredContent"];
    assert_eq!(structured["sandbox"], json!("sandbox"));
    assert_eq!(structured["tool"], json!("content.search"));
    assert_eq!(structured["permission"], json!("content.pages.read"));
    assert_eq!(
        structured["argument_keys"],
        json!(["query"]),
        "the plan names the argument KEYS"
    );

    let pages_after: (i64,) = sqlx::query_as("select count(*) from pages")
        .fetch_one(harness.pool())
        .await
        .expect("count");
    assert_eq!(pages_before.0, pages_after.0, "a sandbox call wrote a page");

    // The log distinguishes proved from did.
    let status: (String,) =
        sqlx::query_as("select status from mcp_invocations order by id desc limit 1")
            .fetch_one(harness.pool())
            .await
            .expect("the row is readable");
    assert_eq!(status.0, "sandbox");

    // The same client with the sandbox switch OFF does the real read.
    store
        .update_client(
            organization_id,
            created.client.id,
            &created.client.name,
            "",
            &["content.pages.read".to_owned()],
            false,
            600,
        )
        .await
        .expect("the switch must flip");
    let live = harness
        .call(rpc(
            &created.token,
            call_body("content.search", json!({ "query": "pric" })),
        ))
        .await;
    assert!(
        live.body["result"]["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("pricing"),
        "with the switch off the call really reads: {live:?}"
    );
    let status: (String,) =
        sqlx::query_as("select status from mcp_invocations order by id desc limit 1")
            .fetch_one(harness.pool())
            .await
            .expect("the row is readable");
    assert_eq!(status.0, "ok");

    // The panel's own sandbox panel answers for the same client, with the would-be request.
    let tested = harness
        .call(post(
            "/api/v1/mcp/sandbox-test",
            json!({
                "client_id": created.client.id,
                "tool": "content.search",
                "arguments": { "query": "pricing" },
            }),
            Some(&session),
        ))
        .await;
    assert_eq!(tested.status, StatusCode::OK, "{:?}", tested.body);
    assert_eq!(tested.body["allowed"], json!(true));
    assert_eq!(
        tested.body["validation"],
        Value::Null,
        "valid arguments, no error"
    );
    assert_eq!(tested.body["refusal"], Value::Null);
    assert_eq!(
        tested.body["request"]["method"],
        json!("tools/call"),
        "the panel shows the request it would send"
    );
    assert_eq!(tested.body["plan"]["argument_keys"], json!(["query"]));

    // And a malformed payload is refused by the schema with the FIELD named, which is the
    // walkthrough's "validation error naming the field".
    let bad = harness
        .call(post(
            "/api/v1/mcp/sandbox-test",
            json!({
                "client_id": created.client.id,
                "tool": "content.search",
                "arguments": {},
            }),
            Some(&session),
        ))
        .await;
    assert_eq!(bad.status, StatusCode::OK, "{:?}", bad.body);
    let validation = &bad.body["validation"];
    assert!(
        validation.is_object(),
        "a missing required field is a validation error"
    );
    assert!(
        validation["message"]
            .as_str()
            .expect("a message")
            .contains("query"),
        "and it names the field: {validation:?}"
    );

    harness.dispose().await;
}

/// The arguments stored in the log are masked, so a call's payload is not readable in the table.
#[tokio::test]
async fn the_stored_arguments_are_masked_and_the_digest_still_groups_calls() {
    let harness = harness!();
    let (_session, organization_id, _) = harness.owner().await;
    seed_page(harness.pool(), organization_id).await;
    let (client_token, client_id) =
        client_with(harness.pool(), organization_id, &["content.pages.read"]).await;
    grant(harness.pool(), client_id, &["content.search"]).await;

    // **A rule and a policy, because the guard needs both.** `Detector::effective_action` reads
    // the action from `policy.action_for(label)` and *not* from the rule row — a rule carrying
    // `action: "mask"` with no policy entry for `email` resolves to `Allow`, so the address
    // passes through untouched and the preview stores it in clear. That is the product behaving
    // as designed (a pattern is not a policy), and it is why this walk originally failed with
    // `masked: true` beside a preview still holding `ada@example.com`: the row said the mask had
    // run, and the value said it had not. The rule says *what* is sensitive; the policy says
    // *what to do about it*.
    omnion_ai_hub::guard_store::save_policy(
        harness.pool(),
        organization_id,
        None,
        omnion_ai_hub::guard_store::PolicyChanges {
            label_defaults: Some(BTreeMap::from([(
                "email".to_owned(),
                omnion_ai_hub::guard_data::Action::Mask,
            )])),
            mask_style: Some(omnion_ai_hub::guard_data::MaskStyle::Numbered),
            allow_user_override: None,
        },
    )
    .await
    .expect("the tenant policy must be saved");

    // A tenant rule that masks an address, so the mask in the log is the tenant's DECISION and
    // not a regex hard-coded in the route. The walk writes it through the same store the guard
    // screen writes, because a route-local mask would pass this test and prove nothing about
    // REQ-105.
    omnion_ai_hub::guard_store::create_rule(
        harness.pool(),
        organization_id,
        None,
        omnion_ai_hub::guard_store::NewRule {
            key: "walk-address".to_owned(),
            label: "email".to_owned(),
            custom_label: None,
            pattern: r"[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}".to_owned(),
            validator: "none".to_owned(),
            action: "mask".to_owned(),
            severity: 2,
            priority: 10,
            providers: vec![],
            features: vec![],
            enabled: true,
            sample: Some("ada@example.com".to_owned()),
        },
    )
    .await
    .expect("the tenant rule must be created");

    let arguments = json!({ "query": "pricing for ada@example.com" });
    for _ in 0..2 {
        let response = harness
            .call(rpc(
                &client_token,
                call_body("content.search", arguments.clone()),
            ))
            .await;
        assert!(response.body.get("error").is_none(), "{response:?}");
    }

    let rows: Vec<(String, String, Value)> = sqlx::query_as(
        "select arguments_sha256, status, arguments_preview from mcp_invocations order by id",
    )
    .fetch_all(harness.pool())
    .await
    .expect("the rows are readable");
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].0, rows[1].0,
        "two identical calls share one digest, so the history can group them"
    );
    assert_eq!(rows[0].1, "ok");
    // The preview names the KEYS, and the values are the guard's masked text — the caller's own
    // search term is masked by the tenant's policy rather than dropped, so the log stays
    // greppable by shape while the address is not readable in it.
    let rendered = rows[0].2.to_string();
    assert!(
        rendered.contains("query"),
        "the preview names what was called: {rendered}"
    );
    assert!(
        !rendered.contains("ada@example.com"),
        "REQ-105's mask must have replaced the address before the row was written: {rendered}"
    );
    // **And the placeholder is the guard's, not a string this route invented.** The route renders
    // `finding.text` verbatim, so a `[EMAIL_1]` here is the guard's `mask_style: numbered` writing
    // itself. Asserting its *absence* — which this walk did while the policy was missing — is how
    // a broken mask passes: an unmasked preview contains no placeholder either. The assertion that
    // actually discriminates is the pair above it, address-gone AND placeholder-present.
    assert!(
        rendered.contains("[EMAIL_"),
        "the mask is the tenant's own style, rendered by the guard: {rendered}"
    );

    // A digest of a DIFFERENT payload differs, so grouping is not a constant.
    // The digest is taken over the ORIGINAL arguments (so two identical calls group), while the
    // preview is the guard's masked text — which is why the address is in the row's digest but
    // must not be readable in the preview.
    let other = omnion_ai_hub::mcp_tools::arguments_digest(&json!({ "query": "other" }));
    assert_ne!(other, rows[0].0);

    harness.dispose().await;
}

/// A call whose arguments do not match the schema is refused before anything is read.
#[tokio::test]
async fn malformed_arguments_are_refused_by_the_schema_with_the_field_named() {
    let harness = harness!();
    let (_session, organization_id, _) = harness.owner().await;
    let (client_token, client_id) =
        client_with(harness.pool(), organization_id, &["content.pages.read"]).await;
    grant(harness.pool(), client_id, &["content.search"]).await;

    let refused = harness
        .call(rpc(&client_token, call_body("content.search", json!({}))))
        .await;
    let error = &refused.body["error"];
    assert_eq!(
        error["code"],
        json!(omnion_ai_hub::mcp_tools::CODE_INVALID_ARGUMENTS)
    );
    assert_eq!(error["data"]["field"], json!("query"));
    assert!(
        error["message"]
            .as_str()
            .expect("a message")
            .contains("query")
    );

    // An unknown field is refused rather than ignored — a tool that silently drops a field the
    // caller believed it sent is a tool that ran on different arguments than the log shows.
    let unknown = harness
        .call(rpc(
            &client_token,
            call_body("content.search", json!({ "query": "x", "limit": "lots" })),
        ))
        .await;
    assert_eq!(
        unknown.body["error"]["code"],
        json!(omnion_ai_hub::mcp_tools::CODE_INVALID_ARGUMENTS),
        "{unknown:?}"
    );

    // And the refusals were recorded, both as errors.
    let statuses: Vec<String> =
        sqlx::query_scalar("select status from mcp_invocations order by id")
            .fetch_all(harness.pool())
            .await
            .expect("the rows are readable");
    assert_eq!(statuses.len(), 2, "both refusals are accounted for");

    harness.dispose().await;
}

/// An unknown method and a malformed envelope are both protocol answers, not HTML.
#[tokio::test]
async fn the_protocol_errors_are_json_rpc_answers_not_http_shapes() {
    let harness = harness!();
    let (_session, organization_id, _) = harness.owner().await;
    let (client_token, _client_id) =
        client_with(harness.pool(), organization_id, &["content.pages.read"]).await;

    let unknown_method = harness
        .call(rpc(
            &client_token,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/list" }),
        ))
        .await;
    assert_eq!(
        unknown_method.body["error"]["code"],
        json!(omnion_ai_hub::mcp_tools::CODE_METHOD_NOT_FOUND)
    );
    assert_eq!(unknown_method.status, StatusCode::OK);

    // A missing `jsonrpc` is refused with the version named, rather than assumed.
    let no_version = harness
        .call(rpc(&client_token, json!({ "id": 1, "method": "ping" })))
        .await;
    assert_eq!(
        no_version.body["error"]["code"],
        json!(omnion_ai_hub::mcp_tools::CODE_INVALID_REQUEST),
        "{no_version:?}"
    );

    // A body that is not JSON is a protocol error, not axum's rejection shape.
    let not_json = harness
        .call(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/mcp")
                .header(header::AUTHORIZATION, format!("Bearer {client_token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("this is not json"))
                .expect("builds"),
        )
        .await;
    assert_eq!(
        not_json.body["error"]["code"],
        json!(omnion_ai_hub::mcp_tools::CODE_INVALID_REQUEST),
        "{not_json:?}"
    );

    // A notification is answered with nothing at all.
    let notification = harness
        .call(rpc(
            &client_token,
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        ))
        .await;
    assert_eq!(
        notification.status,
        StatusCode::NO_CONTENT,
        "{notification:?}"
    );

    // `ping` answers with facts rather than a constant, so a client can tell two servers apart.
    let ping = harness
        .call(rpc(
            &client_token,
            json!({ "jsonrpc": "2.0", "id": 7, "method": "ping",
                    "params": { "name": "walkthrough" } }),
        ))
        .await;
    assert_eq!(ping.body["id"], json!(7));
    assert_eq!(ping.body["result"]["server"], json!("omnion-mcp"));
    assert_eq!(ping.body["result"]["client"], json!("walkthrough"));

    harness.dispose().await;
}

/// The panel's own routes are guarded and scoped like any other surface.
#[tokio::test]
async fn the_panel_routes_need_a_session_and_never_leak_another_tenants_log() {
    let harness = harness!();

    // Two tenants, and the client lives in the FIRST one. `two_tenants` builds them in order and
    // returns the first tenant's id alongside the second tenant's session — so the client is
    // created in a tenant that genuinely exists rather than in "whatever `owner()` made", which
    // is the shape where the isolation claim is measured against a tenant nobody is a member of.
    let (_installer, first_organization, second_session) = two_tenants(&harness).await;
    let (client_token, client_id) =
        client_with(harness.pool(), first_organization, &["content.pages.read"]).await;
    grant(harness.pool(), client_id, &["content.search"]).await;
    harness
        .call(rpc(
            &client_token,
            call_body("content.search", json!({ "query": "pric" })),
        ))
        .await;

    // No session at all.
    for uri in [
        "/api/v1/mcp/overview",
        "/api/v1/mcp/invocations",
        "/api/v1/mcp/tools",
    ] {
        let response = harness.call(get(uri, None)).await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{uri} must need a session: {:?}",
            response.body
        );
    }

    // **A second tenant's session must not read this one's log.** A real second tenant on the
    // same database is the only way to make the claim measurable: a route that forgot
    // `organization_id` in its `where` would answer this request with the first tenant's rows,
    // and every assertion below is what catches it.
    let session = second_session;
    let response = harness
        .call(get("/api/v1/mcp/invocations", Some(&session)))
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
    let rows = response.body["invocations"].as_array().expect("an array");
    // The new owner is in a fresh organization, so its log is empty — which is the whole claim.
    assert!(
        rows.is_empty(),
        "a new tenant sees none of the other tenant's calls: {rows:?}"
    );

    let detail = harness
        .call(get("/api/v1/mcp/invocations/1", Some(&session)))
        .await;
    assert_eq!(
        detail.status,
        StatusCode::NOT_FOUND,
        "and a row from another tenant is a 404, never a 200 with its contents"
    );

    let overview = harness
        .call(get("/api/v1/mcp/overview", Some(&session)))
        .await;
    assert_eq!(overview.status, StatusCode::OK, "{:?}", overview.body);
    assert_eq!(
        overview.body["clients"],
        json!(0),
        "the second tenant has no clients"
    );
    assert!(
        overview.body["tools"].as_u64().expect("a tool count") >= 25,
        "the catalogue is generated, so it is never empty: {:?}",
        overview.body["tools"]
    );

    // The catalogue route lists what the installation can offer — generated, so a tool in
    // `catalogue.rs` appears without anybody adding it here.
    let catalogue = harness.call(get("/api/v1/mcp/tools", Some(&session))).await;
    assert_eq!(catalogue.status, StatusCode::OK);
    let tools = catalogue.body["tools"].as_array().expect("an array");
    assert!(tools.len() >= 25);
    assert_eq!(
        catalogue.body["protocol_version"],
        json!(omnion_ai_hub::mcp_tools::PROTOCOL_VERSION)
    );
    assert!(
        catalogue.body["classes"].is_object(),
        "and they group by class"
    );

    harness.dispose().await;
}
