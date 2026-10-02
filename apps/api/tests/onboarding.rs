//! Integration tests for the first-run flow (docs/requests/REQ-050, phase P10).
//!
//! Every test opens its own throwaway database, so the suite proves what the acceptance
//! criteria ask for — a fresh install reaches a working admin without a single line of SQL by
//! hand — and never touches the development database. They run against the compose stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`); without it the suite skips
//! itself with a printed reason, like the other integration suites.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::sessions;
use omnion_identity::users::{self, NewUser};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// One throwaway installation: its database, its state and a router to drive.
struct Harness {
    state: AppState,
    db: Db,
    maintenance: Db,
    database: String,
}

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    body: Value,
}

impl Harness {
    /// Open a fresh database with every migration applied.
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().expect("environment must be valid");
        live_db(&config).await?;

        let database = format!("omnion_onboarding_{}", Uuid::new_v4().simple());
        let maintenance = Db::connect(&maintenance_config(&config))
            .await
            .expect("the maintenance connection must work");
        sqlx::query(&format!("create database \"{database}\""))
            .execute(maintenance.pool())
            .await
            .expect("the temporary database must be created");

        let db = Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, &database),
            max_connections: 2,
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

    /// Drive the router without a network socket.
    async fn call(&self, request: Request<Body>) -> TestResponse {
        let response = routes::router(self.state.clone())
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

    /// Create an account directly (bypassing the wizard) and sign it in.
    async fn direct_account(&self, email: &str) -> (Uuid, String) {
        let user = users::create_user(
            self.db.pool(),
            NewUser {
                email: email.to_owned(),
                password: PASSWORD.to_owned(),
                display_name: "Direct".to_owned(),
                organization_id: None,
            },
        )
        .await
        .expect("the direct account must be created");
        let (_, token) = sessions::create_session(self.db.pool(), user.id, None, None)
            .await
            .expect("the direct session must be created");
        (user.id, token)
    }

    /// Drop the throwaway database.
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

/// The session token a `Set-Cookie` header carries.
fn token_of(response: &TestResponse) -> String {
    let cookie = response
        .set_cookie
        .as_deref()
        .expect("the response must set a cookie");
    cookie
        .split(';')
        .next()
        .expect("cookie has a value")
        .split_once('=')
        .expect("cookie is name=value")
        .1
        .to_owned()
}

/// A GET request, optionally with a session cookie.
fn get(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let builder = Request::builder().method("GET").uri(uri);
    let builder = match cookie {
        Some(token) => builder.header(header::COOKIE, format!("omnion_session={token}")),
        None => builder,
    };
    builder.body(Body::empty()).expect("request must build")
}

/// A POST request with a JSON body, optionally with a session cookie.
fn post(uri: &str, body: &Value, cookie: Option<&str>) -> Request<Body> {
    let builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    let builder = match cookie {
        Some(token) => builder.header(header::COOKIE, format!("omnion_session={token}")),
        None => builder,
    };
    builder
        .body(Body::from(body.to_string()))
        .expect("request must build")
}

/// Create the owner account and return its session token.
async fn create_owner(harness: &Harness, email: &str) -> String {
    let response = harness
        .call(post(
            "/api/v1/onboarding/owner",
            &json!({
                "display_name": "Ada Lovelace",
                "email": email,
                "password": PASSWORD,
            }),
            None,
        ))
        .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "the owner step must create the account: {:?}",
        response.body
    );
    token_of(&response)
}

#[tokio::test]
async fn the_first_run_walks_a_fresh_database_to_a_signed_in_owner() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let email = format!("owner-{}@omnion.test", Uuid::new_v4().simple());

    // A fresh install: no accounts, every step open, the bundled theme offered.
    let status = harness.call(get("/api/v1/onboarding", None)).await;
    assert_eq!(status.status, StatusCode::OK);
    assert_eq!(status.body["needs_setup"], json!(true));
    assert_eq!(status.body["completed"], json!(false));
    assert_eq!(status.body["steps"]["owner"], json!(false));
    assert!(
        status.body["themes"]
            .as_array()
            .expect("themes are a list")
            .iter()
            .any(|theme| theme["key"] == "minimal"),
        "the bundled theme must be offered: {}",
        status.body["themes"]
    );
    assert_eq!(
        status.body["checklist"]
            .as_array()
            .expect("checklist is a list")
            .len(),
        6,
        "the getting-started checklist is complete"
    );

    // The owner step signs the account in right away.
    let owner = harness
        .call(post(
            "/api/v1/onboarding/owner",
            &json!({ "display_name": "Ada Lovelace", "email": &email, "password": PASSWORD }),
            None,
        ))
        .await;
    assert_eq!(owner.status, StatusCode::CREATED, "{:?}", owner.body);
    assert_eq!(owner.body["user"]["email"], json!(email.to_lowercase()));
    assert_eq!(owner.body["onboarding"]["steps"]["owner"], json!(true));
    assert_eq!(owner.body["onboarding"]["needs_setup"], json!(false));
    assert_eq!(owner.body["onboarding"]["in_progress"], json!(true));
    let token = token_of(&owner);

    // The session works and the account is platform-level.
    let me = harness.call(get("/api/v1/me", Some(&token))).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.body["user"]["email"], json!(email.to_lowercase()));
    assert_eq!(me.body["user"]["organization_id"], Value::Null);

    // The owner step runs once.
    let again = harness
        .call(post(
            "/api/v1/onboarding/owner",
            &json!({ "display_name": "Second", "email": "second@omnion.test", "password": PASSWORD }),
            None,
        ))
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.body["error"]["code"], "already_installed");

    // Later steps need the owner's session.
    let anonymous = harness
        .call(post(
            "/api/v1/onboarding/organization",
            &json!({ "name": "Acme" }),
            None,
        ))
        .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    let organization = harness
        .call(post(
            "/api/v1/onboarding/organization",
            &json!({ "name": "Acme Corporation", "slug": "acme" }),
            Some(&token),
        ))
        .await;
    assert_eq!(
        organization.status,
        StatusCode::OK,
        "{:?}",
        organization.body
    );
    assert_eq!(organization.body["steps"]["organization"], json!(true));
    assert_eq!(
        organization.body["summary"]["organization_name"],
        json!("Acme Corporation")
    );

    let site = harness
        .call(post(
            "/api/v1/onboarding/site",
            &json!({ "name": "Acme Site", "key": "main", "domain": "acme.test" }),
            Some(&token),
        ))
        .await;
    assert_eq!(site.status, StatusCode::OK, "{:?}", site.body);
    assert_eq!(site.body["steps"]["site"], json!(true));
    assert_eq!(site.body["summary"]["site_name"], json!("Acme Site"));

    // The theme has to be one of the bundled ones.
    let unknown = harness
        .call(post(
            "/api/v1/onboarding/theme",
            &json!({ "theme": "neon-void" }),
            Some(&token),
        ))
        .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    assert_eq!(unknown.body["error"]["code"], "unknown_theme");

    let theme = harness
        .call(post(
            "/api/v1/onboarding/theme",
            &json!({ "theme": "minimal" }),
            Some(&token),
        ))
        .await;
    assert_eq!(theme.status, StatusCode::OK, "{:?}", theme.body);
    assert_eq!(theme.body["steps"]["theme"], json!(true));
    assert_eq!(theme.body["summary"]["site_theme"], json!("minimal"));

    // The AI step records the skip (providers arrive with the AI Hub).
    let provider = harness
        .call(post(
            "/api/v1/onboarding/ai-provider",
            &json!({ "provider": "openai" }),
            Some(&token),
        ))
        .await;
    assert_eq!(provider.status, StatusCode::CONFLICT);
    assert_eq!(provider.body["error"]["code"], "ai_hub_pending");

    let ai = harness
        .call(post(
            "/api/v1/onboarding/ai-provider",
            &json!({}),
            Some(&token),
        ))
        .await;
    assert_eq!(ai.status, StatusCode::OK, "{:?}", ai.body);
    assert_eq!(ai.body["steps"]["ai"], json!(true));

    // Closing the run.
    let completed = harness
        .call(post(
            "/api/v1/onboarding/complete",
            &json!({}),
            Some(&token),
        ))
        .await;
    assert_eq!(completed.status, StatusCode::OK, "{:?}", completed.body);
    assert_eq!(completed.body["completed"], json!(true));
    assert_eq!(completed.body["in_progress"], json!(false));

    let after = harness
        .call(post(
            "/api/v1/onboarding/organization",
            &json!({ "name": "Later" }),
            Some(&token),
        ))
        .await;
    assert_eq!(after.status, StatusCode::CONFLICT);
    assert_eq!(after.body["error"]["code"], "onboarding_complete");

    // A working admin: the owner sees the tenant and the site through the regular API.
    let organizations = harness
        .call(get("/api/v1/organizations", Some(&token)))
        .await;
    assert_eq!(
        organizations.status,
        StatusCode::OK,
        "{:?}",
        organizations.body
    );
    assert_eq!(
        organizations.body["organizations"]
            .as_array()
            .expect("organizations are a list")
            .len(),
        1
    );

    let sites = harness.call(get("/api/v1/sites", Some(&token))).await;
    assert_eq!(sites.status, StatusCode::OK);
    let sites = sites.body["sites"].as_array().expect("sites are a list");
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0]["key"], json!("main"));
    assert_eq!(sites[0]["theme"], json!("minimal"));

    // The public renderer sees the site's theme too.
    let page = harness
        .call(get("/api/v1/public/pages/home?site=acme.test", None))
        .await;
    assert_eq!(
        page.status,
        StatusCode::NOT_FOUND,
        "no page exists yet — but the site resolution must not fail"
    );
    let site_theme: String = sqlx::query_scalar("select theme from sites where key = 'main'")
        .fetch_one(harness.db.pool())
        .await
        .expect("the site theme must be readable");
    assert_eq!(site_theme, "minimal");

    // Every step left an audit row.
    let actions: Vec<String> = sqlx::query_scalar(
        "select action from audit_log where action like 'onboarding.%' order by id",
    )
    .fetch_all(harness.db.pool())
    .await
    .expect("audit rows must be readable");
    for expected in [
        "onboarding.owner_created",
        "onboarding.organization_created",
        "onboarding.site_created",
        "onboarding.theme_chosen",
        "onboarding.ai_step_skipped",
        "onboarding.completed",
    ] {
        assert!(
            actions.iter().any(|action| action == expected),
            "missing audit row {expected}: {actions:?}"
        );
    }

    harness.dispose().await;
}

#[tokio::test]
async fn only_the_account_that_owns_the_first_run_may_finish_it() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let owner_email = format!("owner-{}@omnion.test", Uuid::new_v4().simple());
    let token = create_owner(&harness, &owner_email).await;

    // A second account exists (invited later, created directly here) but is not the owner.
    let (_, intruder) = harness
        .direct_account(&format!("other-{}@omnion.test", Uuid::new_v4().simple()))
        .await;

    let refused = harness
        .call(post(
            "/api/v1/onboarding/organization",
            &json!({ "name": "Not Mine" }),
            Some(&intruder),
        ))
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(refused.body["error"]["code"], "not_onboarding_owner");

    let allowed = harness
        .call(post(
            "/api/v1/onboarding/organization",
            &json!({ "name": "Mine" }),
            Some(&token),
        ))
        .await;
    assert_eq!(allowed.status, StatusCode::OK, "{:?}", allowed.body);
    assert_eq!(allowed.body["steps"]["organization"], json!(true));

    harness.dispose().await;
}

#[tokio::test]
async fn the_flow_refuses_what_it_still_needs() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let token = create_owner(
        &harness,
        &format!("owner-{}@omnion.test", Uuid::new_v4().simple()),
    )
    .await;

    // No site yet: a theme has nothing to belong to and the run cannot close.
    let theme = harness
        .call(post(
            "/api/v1/onboarding/theme",
            &json!({ "theme": "minimal" }),
            Some(&token),
        ))
        .await;
    assert_eq!(theme.status, StatusCode::CONFLICT);
    assert_eq!(theme.body["error"]["code"], "site_missing");

    let complete = harness
        .call(post(
            "/api/v1/onboarding/complete",
            &json!({}),
            Some(&token),
        ))
        .await;
    assert_eq!(complete.status, StatusCode::CONFLICT);
    assert_eq!(complete.body["error"]["code"], "setup_incomplete");
    assert!(
        complete.body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("organization") && message.contains("site")),
        "the answer must name the missing steps: {}",
        complete.body["error"]["message"]
    );

    // A site is created once; a second attempt is refused instead of duplicating.
    let organization = harness
        .call(post(
            "/api/v1/onboarding/organization",
            &json!({ "name": "Acme" }),
            Some(&token),
        ))
        .await;
    assert_eq!(organization.status, StatusCode::OK);
    let site = harness
        .call(post(
            "/api/v1/onboarding/site",
            &json!({ "name": "Acme Site" }),
            Some(&token),
        ))
        .await;
    assert_eq!(site.status, StatusCode::OK, "{:?}", site.body);
    assert_eq!(site.body["steps"]["site"], json!(true));

    let twice = harness
        .call(post(
            "/api/v1/onboarding/site",
            &json!({ "name": "Another" }),
            Some(&token),
        ))
        .await;
    assert_eq!(twice.status, StatusCode::CONFLICT);
    assert_eq!(twice.body["error"]["code"], "step_already_done");

    harness.dispose().await;
}

#[tokio::test]
async fn an_installation_with_accounts_keeps_its_own_first_run() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    // An installation whose first account came from the environment bootstrap (or the API).
    let (account, token) = harness
        .direct_account(&format!("boot-{}@omnion.test", Uuid::new_v4().simple()))
        .await;
    let _account = account;

    let status = harness.call(get("/api/v1/onboarding", None)).await;
    assert_eq!(status.status, StatusCode::OK);
    assert_eq!(status.body["needs_setup"], json!(false));
    assert_eq!(status.body["in_progress"], json!(true));
    assert_eq!(status.body["steps"]["owner"], json!(true));

    // Creating a "first" owner is refused: this installation already has accounts.
    let owner = harness
        .call(post(
            "/api/v1/onboarding/owner",
            &json!({ "display_name": "Late", "email": "late@omnion.test", "password": PASSWORD }),
            None,
        ))
        .await;
    assert_eq!(owner.status, StatusCode::CONFLICT);
    assert_eq!(owner.body["error"]["code"], "already_installed");

    // The oldest account may still finish the first run through the wizard.
    let organization = harness
        .call(post(
            "/api/v1/onboarding/organization",
            &json!({ "name": "Bootstrap Ltd" }),
            Some(&token),
        ))
        .await;
    assert_eq!(
        organization.status,
        StatusCode::OK,
        "{:?}",
        organization.body
    );
    assert_eq!(organization.body["steps"]["organization"], json!(true));

    let bound: Option<Uuid> = sqlx::query_scalar("select owner_user_id from onboarding_state")
        .fetch_one(harness.db.pool())
        .await
        .expect("the state row must be readable");
    assert_eq!(
        bound, None,
        "the wizard recorded no owner (it did not create one)"
    );

    harness.dispose().await;
}

/// Connect to the compose PostgreSQL; `None` means the stack is not running.
async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 1,
    })
    .await
    {
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

/// Maintenance connection (`postgres` database) used to create and drop throwaway databases.
fn maintenance_config(config: &Config) -> DatabaseConfig {
    DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 1,
    }
}

/// Replace the database name in a PostgreSQL connection string.
fn swap_database(url: &str, database: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let prefix = base
        .rsplit_once('/')
        .expect("the URL must contain a database path")
        .0;
    match query {
        Some(query) => format!("{prefix}/{database}?{query}"),
        None => format!("{prefix}/{database}"),
    }
}
