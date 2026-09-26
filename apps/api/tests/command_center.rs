//! Integration tests for the command centre: the command registry's projection, the route
//! suggestions and the caller's own recents (docs/requests/REQ-032, slice 1).
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) — locally and in CI. When
//! PostgreSQL is not reachable the suite skips itself with a printed reason.
//!
//! What the walks prove, in the words of the acceptance criteria: the command list is filtered
//! **server-side** (a caller without `content.pages.create` never receives the create command —
//! the title is absent from the body, not merely hidden), the projection follows the same
//! permission set the rest of the API uses, suggestions follow the screen without offering the
//! one already open, recents are per user (never per organization), repeating one moves it up
//! instead of stacking a copy, the history is trimmed to fifty rows, and a recent that names a
//! command the caller may not run is refused rather than stored.

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

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// What the content editor of this suite may do: search, read and write pages, read media — and
/// deliberately **not** manage the index, read sites or reach the AI hub.
const EDITOR_PERMISSIONS: [&str; 4] = [
    "search.read",
    "content.pages.read",
    "content.pages.create",
    "media.read",
];

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    body: Value,
}

/// Drive the real router without a network socket.
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
        serde_json::from_slice(&bytes).expect("body must be JSON")
    };

    TestResponse {
        status,
        set_cookie,
        body,
    }
}

/// Build a request; `token` becomes the session cookie and `body` the JSON payload.
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

/// Object store of the test state; the command centre never touches it.
fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

/// Connect to the compose PostgreSQL; `None` means the stack is not running.
async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&config.database).await {
        Ok(db) => Some(db),
        Err(err) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({err}) — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
            );
            None
        }
    }
}

/// A state whose database has all migrations applied and the IAM seed loaded.
async fn live_state() -> Option<(AppState, Db)> {
    let config = Config::from_env().expect("environment must be valid");
    let db = live_db(&config).await?;
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

/// One organization, a platform Owner, a content editor and a plain member — the three points of
/// view the projection has to answer for.
struct Fixture {
    state: AppState,
    db: Db,
    platform_email: String,
    editor_email: String,
    member_email: String,
    accounts: Vec<Uuid>,
    organizations: Vec<Uuid>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let org = create_organization_row(&db, "Command Centre Test").await;

        let (platform_id, platform_email) = create_account(&db, None, "Command Owner").await;
        seed::bind_owner(db.pool(), platform_id)
            .await
            .expect("the owner binding must be created");

        let (editor_id, editor_email) = create_account(&db, Some(org), "Command Editor").await;
        grant_role(
            &db,
            org,
            editor_id,
            platform_id,
            "Command Editor",
            &EDITOR_PERMISSIONS,
        )
        .await;

        let (member_id, member_email) = create_account(&db, Some(org), "Command Member").await;
        grant_role(
            &db,
            org,
            member_id,
            platform_id,
            "Command Member",
            &["search.read"],
        )
        .await;

        Some(Self {
            state,
            db,
            platform_email,
            editor_email,
            member_email,
            accounts: vec![platform_id, editor_id, member_id],
            organizations: vec![org],
        })
    }

    /// The platform Owner, signed in.
    async fn platform_token(&self) -> String {
        login(&self.state, &self.platform_email).await
    }

    /// The content editor of the organization, signed in.
    async fn editor_token(&self) -> String {
        login(&self.state, &self.editor_email).await
    }

    /// The plain member of the organization, signed in.
    async fn member_token(&self) -> String {
        login(&self.state, &self.member_email).await
    }

    /// Remove exactly what this fixture created; recents cascade with the accounts.
    async fn cleanup(&self) {
        sqlx::query("delete from users where id = any($1)")
            .bind(&self.accounts)
            .execute(self.db.pool())
            .await
            .expect("account cleanup must run");
        sqlx::query("delete from organizations where id = any($1)")
            .bind(&self.organizations)
            .execute(self.db.pool())
            .await
            .expect("organization cleanup must run");
    }
}

/// Create an organization row with a unique slug.
async fn create_organization_row(db: &Db, name: &str) -> Uuid {
    let slug = format!("command-fix-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(name)
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// Create an account with a unique address so parallel runs cannot collide.
async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    let email = format!("command-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: name.to_owned(),
            organization_id,
        },
    )
    .await
    .expect("creating a test account must succeed");
    (user.id, email)
}

/// Give `user_id` a fresh role inside `organization_id` holding exactly `keys`.
async fn grant_role(
    db: &Db,
    organization_id: Uuid,
    user_id: Uuid,
    granted_by: Uuid,
    label: &str,
    keys: &[&str],
) {
    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id,
            key: format!(
                "command-{}-{}",
                label.to_lowercase().replace(' ', "-"),
                Uuid::new_v4().simple()
            ),
            name: label.to_owned(),
            description: format!("{label} of the command centre suite"),
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
        .expect("the permission set must be written");

    let binding = NewBinding {
        role_id: role.id,
        user_id,
        scope: Scope::Organization { organization_id },
        granted_by: Some(granted_by),
        expires_at: None,
    };
    bindings::validate(db.pool(), &binding)
        .await
        .expect("the binding must validate");
    bindings::grant(db.pool(), binding)
        .await
        .expect("the binding must be granted");
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

/// The command ids of a `GET /api/v1/commands` (or context) answer, in order.
fn command_ids(body: &Value) -> Vec<String> {
    body["commands"]
        .as_array()
        .unwrap_or_else(|| panic!("commands must be an array in {body}"))
        .iter()
        .map(|row| {
            row["id"]
                .as_str()
                .unwrap_or_else(|| panic!("every command needs an id in {row}"))
                .to_owned()
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The projection
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn commands_are_projected_through_the_callers_permissions() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    let owner = fixture.platform_token().await;
    let response = call(
        &fixture.state,
        request(Method::GET, "/api/v1/commands", Some(&owner), None),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);
    let ids = command_ids(&response.body);
    assert!(
        ids.contains(&"nav.search-settings".to_owned()) && ids.contains(&"nav.sites".to_owned()),
        "the platform Owner holds every key, got {ids:?}"
    );

    // The editor: content and media commands, no index settings, no sites, no AI hub.
    let editor = fixture.editor_token().await;
    let response = call(
        &fixture.state,
        request(Method::GET, "/api/v1/commands", Some(&editor), None),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);
    let ids = command_ids(&response.body);
    assert!(ids.contains(&"nav.pages".to_owned()), "got {ids:?}");
    assert!(ids.contains(&"nav.create-page".to_owned()), "got {ids:?}");
    assert!(ids.contains(&"nav.media".to_owned()), "got {ids:?}");
    assert!(
        !ids.contains(&"nav.search-settings".to_owned()),
        "search.manage is not part of the editor's keys, got {ids:?}"
    );
    assert!(
        !ids.contains(&"nav.sites".to_owned()) && !ids.contains(&"nav.ai".to_owned()),
        "unheld keys must never be projected, got {ids:?}"
    );

    // The member holds `search.read` and nothing else: the box, and the panel's home.
    let member = fixture.member_token().await;
    let response = call(
        &fixture.state,
        request(Method::GET, "/api/v1/commands", Some(&member), None),
    )
    .await;
    let ids = command_ids(&response.body);
    assert_eq!(
        ids,
        vec!["nav.overview".to_owned(), "nav.search".to_owned()],
        "a member without content keys sees exactly the two unguarded commands"
    );

    // The leak test is on the raw body: a title the caller may not have must not be present as
    // text, not merely absent as an id.
    let member = fixture.member_token().await;
    let response = call(
        &fixture.state,
        request(Method::GET, "/api/v1/commands", Some(&member), None),
    )
    .await;
    let raw = response.body.to_string().to_lowercase();
    for forbidden in ["create a page", "open pages", "search settings", "ai hub"] {
        assert!(
            !raw.contains(forbidden),
            "the title {forbidden:?} leaked into a member's command list"
        );
    }

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_signed_out_caller_reaches_nothing() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    for (method, uri, body) in [
        (Method::GET, "/api/v1/commands", None),
        (Method::GET, "/api/v1/command-center/context?route=/", None),
        (Method::GET, "/api/v1/command-center/recent", None),
        (
            Method::POST,
            "/api/v1/command-center/recent",
            Some(json!({ "kind": "query", "query": "release" })),
        ),
        (Method::DELETE, "/api/v1/command-center/recent", None),
    ] {
        let response = call(&fixture.state, request(method.clone(), uri, None, body)).await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} must refuse a signed-out caller, got {}",
            response.body
        );
    }

    fixture.cleanup().await;
}

#[tokio::test]
async fn suggestions_follow_the_screen_the_caller_is_on() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;

    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/command-center/context?route=%2Fpages%3Ffocus%3Dabc",
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);
    let ids = command_ids(&response.body);
    assert!(
        ids.contains(&"nav.create-page".to_owned()),
        "the pages screen suggests its own create form, got {ids:?}"
    );
    assert!(
        !ids.contains(&"nav.pages".to_owned()),
        "the screen the caller is already on is never suggested, got {ids:?}"
    );
    assert!(
        !ids.contains(&"nav.search-settings".to_owned()),
        "suggestions respect permissions too, got {ids:?}"
    );

    // A screen none of the entries names still gets a useful fallback.
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/command-center/context?route=%2Freports%2F2026",
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);
    assert_eq!(
        command_ids(&response.body).first().map(String::as_str),
        Some("nav.search"),
        "an unnamed screen falls back to the search box"
    );

    // A missing or malformed route is refused, not guessed.
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/command-center/context",
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.body["error"]["code"], "route_required");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// Recents
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn recents_are_per_user_and_never_per_organization() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;
    let member = fixture.member_token().await;

    // The editor runs a command and commits a search.
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/command-center/recent",
            Some(&editor),
            Some(json!({ "kind": "command", "command_id": "nav.pages" })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::NO_CONTENT,
        "body: {}",
        response.body
    );

    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/command-center/recent",
            Some(&editor),
            Some(json!({ "kind": "query", "query": "release notes", "result_count": 4 })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);

    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/command-center/recent",
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);
    let items = response.body["items"]
        .as_array()
        .expect("items must be an array");
    assert_eq!(
        items.len(),
        2,
        "two recents, newest first: {}",
        response.body
    );
    assert_eq!(items[0]["kind"], "query", "the newest row comes first");
    assert_eq!(items[0]["query"], "release notes");
    assert_eq!(items[0]["result_count"], 4);
    assert_eq!(items[1]["kind"], "command");
    assert_eq!(items[1]["command_id"], "nav.pages");
    assert_eq!(
        items[1]["title"], "Open pages",
        "a command recent carries its current title and route"
    );
    assert_eq!(items[1]["route"], "/pages");

    // The same organization, a different account: nothing of the editor's.
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/command-center/recent",
            Some(&member),
            None,
        ),
    )
    .await;
    assert_eq!(
        response.body["items"].as_array().map(Vec::len),
        Some(0),
        "recents are personal, got {}",
        response.body
    );

    // Clearing is personal too.
    let response = call(
        &fixture.state,
        request(
            Method::DELETE,
            "/api/v1/command-center/recent",
            Some(&member),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);

    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/command-center/recent",
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(
        response.body["items"].as_array().map(Vec::len),
        Some(2),
        "one account clearing must not empty another's history"
    );

    let response = call(
        &fixture.state,
        request(
            Method::DELETE,
            "/api/v1/command-center/recent",
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/command-center/recent",
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(
        response.body["items"].as_array().map(Vec::len),
        Some(0),
        "after a clear the history is empty"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn repeating_a_recent_moves_it_up_instead_of_stacking_a_copy() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;

    for query in ["alpha", "beta", "alpha"] {
        let response = call(
            &fixture.state,
            request(
                Method::POST,
                "/api/v1/command-center/recent",
                Some(&editor),
                Some(json!({ "kind": "query", "query": query })),
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::NO_CONTENT);
    }

    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/command-center/recent",
            Some(&editor),
            None,
        ),
    )
    .await;
    let items = response.body["items"]
        .as_array()
        .expect("items must be an array");
    assert_eq!(items.len(), 2, "two distinct queries, not three rows");
    assert_eq!(
        items[0]["query"], "alpha",
        "the repeat moved back to the top"
    );
    assert_eq!(items[1]["query"], "beta");

    // Running the same command twice is one row as well.
    for _ in 0..2 {
        let response = call(
            &fixture.state,
            request(
                Method::POST,
                "/api/v1/command-center/recent",
                Some(&editor),
                Some(json!({ "kind": "command", "command_id": "nav.media" })),
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::NO_CONTENT);
    }
    let count: i64 = sqlx::query_scalar(
        "select count(*) from command_recents where user_id = $1 and kind = 'command'",
    )
    .bind(fixture.accounts[1])
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must answer");
    assert_eq!(count, 1, "a repeated command is one row");

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_history_is_trimmed_to_fifty_rows() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;

    for index in 0..55 {
        let response = call(
            &fixture.state,
            request(
                Method::POST,
                "/api/v1/command-center/recent",
                Some(&editor),
                Some(json!({ "kind": "query", "query": format!("query-{index:03}") })),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::NO_CONTENT,
            "recording {index}: {}",
            response.body
        );
    }

    let kept: i64 = sqlx::query_scalar("select count(*) from command_recents where user_id = $1")
        .bind(fixture.accounts[1])
        .fetch_one(fixture.db.pool())
        .await
        .expect("the count must answer");
    assert_eq!(kept, 50, "the history keeps the newest fifty rows");

    // The oldest five are gone, the newest is present.
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/command-center/recent",
            Some(&editor),
            None,
        ),
    )
    .await;
    let items = response.body["items"]
        .as_array()
        .expect("items must be an array");
    assert_eq!(items.len(), 20, "the palette reads its newest twenty");
    assert_eq!(items[0]["query"], "query-054", "newest first");

    fixture.cleanup().await;
}

#[tokio::test]
async fn an_unusable_recent_is_refused_with_a_reason() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;
    let member = fixture.member_token().await;

    let cases = [
        (json!({ "kind": "note", "query": "x" }), "unknown_kind"),
        (json!({ "kind": "query", "query": "   " }), "query_required"),
        (
            json!({ "kind": "query", "query": "x".repeat(201) }),
            "query_too_long",
        ),
        (
            json!({ "kind": "command", "command_id": "nav.invoices" }),
            "unknown_command",
        ),
    ];
    for (body, code) in cases {
        let response = call(
            &fixture.state,
            request(
                Method::POST,
                "/api/v1/command-center/recent",
                Some(&editor),
                Some(body.clone()),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "body {body} must be refused"
        );
        assert_eq!(response.body["error"]["code"], code, "body {body}");
    }

    // A command the caller may not run is refused rather than remembered: the palette never
    // stores what it would not offer.
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/command-center/recent",
            Some(&member),
            Some(json!({ "kind": "command", "command_id": "nav.pages" })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(response.body["error"]["code"], "command_not_allowed");

    // Nothing of the refused attempts was written.
    let count: i64 =
        sqlx::query_scalar("select count(*) from command_recents where user_id = any($1)")
            .bind(&fixture.accounts)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the count must answer");
    assert_eq!(count, 0, "a refused recent leaves no row behind");

    fixture.cleanup().await;
}

#[tokio::test]
async fn every_registered_command_can_be_remembered() {
    // The column's own format check once rejected hyphenated ids (`nav.create-page`) — an
    // ordinary run answered 500. Walk the whole registry through the endpoint so a new id
    // cannot bring that back.
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.platform_token().await;

    for spec in omnion_search::COMMANDS {
        let response = call(
            &fixture.state,
            request(
                Method::POST,
                "/api/v1/command-center/recent",
                Some(&owner),
                Some(json!({ "kind": "command", "command_id": spec.id })),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::NO_CONTENT,
            "recording {}: {}",
            spec.id,
            response.body
        );
    }

    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/command-center/recent",
            Some(&owner),
            None,
        ),
    )
    .await;
    let ids: Vec<&str> = response.body["items"]
        .as_array()
        .expect("items must be an array")
        .iter()
        .filter_map(|row| row["command_id"].as_str())
        .collect();
    assert_eq!(
        ids.len(),
        omnion_search::COMMANDS.len(),
        "every command of the registry is remembered, got {ids:?}"
    );
    assert!(
        ids.contains(&"nav.create-page"),
        "a hyphenated id is stored like any other, got {ids:?}"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_recent_whose_command_is_out_of_reach_is_not_shown() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;

    for id in ["nav.pages", "nav.media"] {
        let response = call(
            &fixture.state,
            request(
                Method::POST,
                "/api/v1/command-center/recent",
                Some(&editor),
                Some(json!({ "kind": "command", "command_id": id })),
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::NO_CONTENT);
    }

    // Take the editor's content keys away: the same rows are read back, but the commands they
    // name are resolved against the caller's *current* permissions.
    let role_id: Uuid = sqlx::query_scalar(
        "select rb.role_id from role_bindings rb where rb.user_id = $1 and rb.role_id in \
         (select id from roles where name = 'Command Editor') limit 1",
    )
    .bind(fixture.accounts[1])
    .fetch_one(fixture.db.pool())
    .await
    .expect("the editor's binding must exist");
    sqlx::query(
        "delete from role_permissions where role_id = $1 and permission_key = 'content.pages.read'",
    )
    .bind(role_id)
    .execute(fixture.db.pool())
    .await
    .expect("the permission must be removable");

    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/command-center/recent",
            Some(&editor),
            None,
        ),
    )
    .await;
    let items = response.body["items"]
        .as_array()
        .expect("items must be an array");
    let ids: Vec<&str> = items
        .iter()
        .filter_map(|row| row["command_id"].as_str())
        .collect();
    assert!(
        !ids.contains(&"nav.pages"),
        "a command the caller may no longer run is dropped, got {ids:?}"
    );
    assert!(
        ids.contains(&"nav.media"),
        "the commands still within reach stay, got {ids:?}"
    );

    fixture.cleanup().await;
}
