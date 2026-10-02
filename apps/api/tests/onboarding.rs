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
        // EVERY `Set-Cookie`, joined. `HeaderMap::get` answers the first header only, and a
        // sign-in sends two — the session, then the CSRF token APPENDED after it. Reading one
        // made the token structurally invisible to this suite, which then reported "the cookie is
        // not set" about a cookie the server had sent on every single sign-in.
        let set_cookie = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect::<Vec<_>>()
            .join("\n");
        let set_cookie = (!set_cookie.is_empty()).then_some(set_cookie);
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
    /// Create an account and a session for it WITHOUT going through `/auth/login`, so the two
    /// cookies a browser gets (session + readable CSRF) are minted here instead.
    ///
    /// The CSRF token is not optional on a cookie-authenticated write: the layer answers
    /// `403 csrf_failed` before the handler runs, so a helper that returns only the session
    /// produces requests that can never succeed — and the assertions downstream then measure the
    /// refusal rather than the behaviour.
    async fn direct_account(&self, email: &str) -> (Uuid, Session) {
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
        let (session, token) = sessions::create_session(self.db.pool(), user.id, None, None)
            .await
            .expect("the direct session must be created");
        // The SAME derivation `cookies::csrf_cookie_for` uses, from the same one place in the
        // codebase: `derive_csrf_token(secret, session_id)`. Copying the shape instead of calling
        // it would be a second implementation of the rule the CSRF layer checks, which is exactly
        // how the two drift.
        //
        // The secret comes from the CONFIG this harness built with (`Config::from_env`), not from a
        // literal here: a hard-coded key in the test would agree with the layer on the day it was
        // written and disagree the first time someone ran the suite with a different
        // `OMNION_CSRF_SECRET` — and the disagreement reads as "the CSRF layer is broken".
        let Some(secret) = self.state.config().csrf.as_bytes().map(<[u8]>::to_vec) else {
            panic!(
                "this suite exercises cookie-authenticated writes, so OMNION_CSRF_SECRET must be set \
                 in the environment it runs in"
            );
        };
        let csrf = omnion_security::derive_csrf_token(&secret, &session.id.to_string());
        (user.id, Session { token, csrf })
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
    cookie_value(response, "omnion_session").expect("the response must set a session cookie")
}

/// The value of one named cookie out of a response's `Set-Cookie` list.
fn cookie_value(response: &TestResponse, name: &str) -> Option<String> {
    response.set_cookie.as_deref()?.lines().find_map(|line| {
        let pair = line.split(';').next()?.trim();
        let (key, value) = pair.split_once('=')?;
        (key.trim() == name).then(|| value.to_owned())
    })
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
///
/// ## The CSRF header is not optional on a cookie-authenticated write
///
/// This suite went red before it went wrong: it POSTs with a session cookie and no CSRF token, and
/// the API answers `403 csrf_failed` — *before the handler runs*, so every assertion about what the
/// handler did was measuring a refusal. The four tests failed identically and the file's own
/// comments still described a working first run, which is the worst combination available: an
/// assertion about behaviour, sitting on a request that never reached the behaviour.
///
/// The token is the readable `omnion_csrf` cookie the owner POST set, read by
/// [`TestResponse::set_cookie`] and echoed in `x-omnion-csrf` — the same two-hop the panel's own
/// client does. `create_owner` now returns BOTH values, because the cookie is only on that one
/// response and every later step needs it.
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

/// The readable CSRF token out of a response's `Set-Cookie` list, if it set one.
fn csrf_of(response: &TestResponse) -> Option<String> {
    cookie_value(response, "omnion_csrf")
}

/// A POST carrying the session cookie **and** the CSRF token, as a browser would.
fn post_authed(uri: &str, body: &Value, session: &Session) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, format!("omnion_session={}", session.token))
        .header("x-omnion-csrf", &session.csrf)
        .body(Body::from(body.to_string()))
        .expect("request must build")
}

/// A DELETE carrying the session cookie and the CSRF token.
fn delete_authed(uri: &str, session: &Session) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .header(header::COOKIE, format!("omnion_session={}", session.token))
        .header("x-omnion-csrf", &session.csrf)
        .body(Body::empty())
        .expect("request must build")
}

/// The owner's session: the cookie AND the CSRF token, which arrive on the same response.
#[derive(Clone)]
struct Session {
    token: String,
    csrf: String,
}

/// Create the owner account and answer its session: the cookie **and** the CSRF token.
async fn create_owner(harness: &Harness, email: &str) -> Session {
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
    // Refuse here rather than three tests later: every subsequent step's 403 `csrf_failed` would
    // otherwise be a mystery with this line's name nowhere near it.
    let csrf = csrf_of(&response).expect(
        "the owner POST must set the readable omnion_csrf cookie — without it every later step is \
         refused by the CSRF layer before its handler runs",
    );
    Session { token: token_of(&response), csrf }
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
    let created = harness
        .call(post(
            "/api/v1/onboarding/owner",
            &json!({ "display_name": "Ada Lovelace", "email": &email, "password": PASSWORD }),
            None,
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    assert_eq!(created.body["user"]["email"], json!(email.to_lowercase()));
    assert_eq!(created.body["onboarding"]["steps"]["owner"], json!(true));
    assert_eq!(created.body["onboarding"]["needs_setup"], json!(false));
    assert_eq!(created.body["onboarding"]["in_progress"], json!(true));
    let owner = Session {
        token: token_of(&created),
        csrf: csrf_of(&created).expect("the owner POST sets the readable omnion_csrf cookie"),
    };

    // The session works and the account is platform-level.
    let me = harness.call(get("/api/v1/me", Some(&owner.token))).await;
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
        .call(post_authed("/api/v1/onboarding/organization", &json!({ "name": "Acme Corporation", "slug": "acme" }), &owner))
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
        .call(post_authed("/api/v1/onboarding/site", &json!({ "name": "Acme Site", "key": "main", "domain": "acme.test" }), &owner))
        .await;
    assert_eq!(site.status, StatusCode::OK, "{:?}", site.body);
    assert_eq!(site.body["steps"]["site"], json!(true));
    assert_eq!(site.body["summary"]["site_name"], json!("Acme Site"));

    // The theme has to be one of the bundled ones.
    let unknown = harness
        .call(post_authed("/api/v1/onboarding/theme", &json!({ "theme": "neon-void" }), &owner))
        .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    assert_eq!(unknown.body["error"]["code"], "unknown_theme");

    let theme = harness
        .call(post_authed("/api/v1/onboarding/theme", &json!({ "theme": "minimal" }), &owner))
        .await;
    assert_eq!(theme.status, StatusCode::OK, "{:?}", theme.body);
    assert_eq!(theme.body["steps"]["theme"], json!(true));
    assert_eq!(theme.body["summary"]["site_theme"], json!("minimal"));

    // The AI step records the skip (providers arrive with the AI Hub).
    let provider = harness
        .call(post_authed("/api/v1/onboarding/ai-provider", &json!({ "provider": "openai" }), &owner))
        .await;
    assert_eq!(provider.status, StatusCode::CONFLICT);
    assert_eq!(provider.body["error"]["code"], "ai_hub_pending");

    let ai = harness
        .call(post_authed("/api/v1/onboarding/ai-provider", &json!({}), &owner))
        .await;
    assert_eq!(ai.status, StatusCode::OK, "{:?}", ai.body);
    assert_eq!(ai.body["steps"]["ai"], json!(true));

    // Closing the run.
    let completed = harness
        .call(post_authed("/api/v1/onboarding/complete", &json!({}), &owner))
        .await;
    assert_eq!(completed.status, StatusCode::OK, "{:?}", completed.body);
    assert_eq!(completed.body["completed"], json!(true));
    assert_eq!(completed.body["in_progress"], json!(false));

    let after = harness
        .call(post_authed("/api/v1/onboarding/organization", &json!({ "name": "Later" }), &owner))
        .await;
    assert_eq!(after.status, StatusCode::CONFLICT);
    assert_eq!(after.body["error"]["code"], "onboarding_complete");

    // A working admin: the owner sees the tenant and the site through the regular API.
    let organizations = harness
        .call(get("/api/v1/organizations", Some(&owner.token)))
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

    let sites = harness.call(get("/api/v1/sites", Some(&owner.token))).await;
    assert_eq!(sites.status, StatusCode::OK);
    let sites = sites.body["sites"].as_array().expect("sites are a list");
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0]["key"], json!("main"));
    assert_eq!(sites[0]["theme"], json!("minimal"));

    // The owner's ACCOUNT now carries the organization — and this is the assertion that was
    // missing for the whole life of the bug.
    //
    // `GET /api/v1/sites` and `GET /api/v1/organizations` above both passed on a database where
    // `users.organization_id` was still NULL: those two routes resolve the tenant from the single
    // organization row, not from the account. `scope::resolve_organization` is the function that
    // DOES read the account — and it answers `400 organization_required` for an account with
    // `None`. So every org-scoped route on a *platform* read worked, which is why the wizard
    // "succeeded", and the 400 appeared only on the deployment centre and its siblings.
    //
    // Removing the `users::set_organization` call from `onboarding::steps::create_organization`
    // leaves every assertion in this test green — measured, not assumed — and this one red.
    let owner_account: Option<Uuid> =
        sqlx::query_scalar("select organization_id from users where email = $1")
            .bind(&email)
            .fetch_one(harness.db.pool())
            .await
            .expect("the owner row must be readable");
    let organization_id: Uuid = sqlx::query_scalar("select id from organizations limit 1")
        .fetch_one(harness.db.pool())
        .await
        .expect("the organization row must be readable");
    assert_eq!(
        owner_account,
        Some(organization_id),
        "the owner's account must point at the organization, or every org-scoped read is refused \
         with 400 organization_required"
    );

    // And through the API the account itself is asked, not the database.
    let me_after = harness.call(get("/api/v1/me", Some(&owner.token))).await;
    assert_eq!(me_after.status, StatusCode::OK);
    assert_eq!(
        me_after.body["user"]["organization_id"],
        json!(organization_id.to_string()),
        "/api/v1/me must report the account's organization"
    );

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
    let owner = create_owner(&harness, &owner_email).await;

    // A second account exists (invited later, created directly here) but is not the owner.
    let (_, intruder) = harness
        .direct_account(&format!("other-{}@omnion.test", Uuid::new_v4().simple()))
        .await;

    // The intruder carries a valid session AND a valid CSRF token on purpose. Without the token
    // the request is refused by the CSRF layer for a reason that has nothing to do with
    // ownership — and `403` would still be the answer, so the assertion below would pass against
    // the wrong refusal and the ownership check would never have been exercised.
    let refused = harness
        .call(post_authed(
            "/api/v1/onboarding/organization",
            &json!({ "name": "Not Mine" }),
            &intruder,
        ))
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(
        refused.body["error"]["code"], "not_onboarding_owner",
        "the refusal must come from the ownership check, not from CSRF: {:?}",
        refused.body
    );

    let allowed = harness
        .call(post_authed("/api/v1/onboarding/organization", &json!({ "name": "Mine" }), &owner))
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
    let owner = create_owner(
        &harness,
        &format!("owner-{}@omnion.test", Uuid::new_v4().simple()),
    )
    .await;

    // No site yet: a theme has nothing to belong to and the run cannot close.
    let theme = harness
        .call(post_authed("/api/v1/onboarding/theme", &json!({ "theme": "minimal" }), &owner))
        .await;
    assert_eq!(theme.status, StatusCode::CONFLICT);
    assert_eq!(theme.body["error"]["code"], "site_missing");

    let complete = harness
        .call(post_authed("/api/v1/onboarding/complete", &json!({}), &owner))
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
        .call(post_authed("/api/v1/onboarding/organization", &json!({ "name": "Acme" }), &owner))
        .await;
    assert_eq!(organization.status, StatusCode::OK);
    let site = harness
        .call(post_authed("/api/v1/onboarding/site", &json!({ "name": "Acme Site" }), &owner))
        .await;
    assert_eq!(site.status, StatusCode::OK, "{:?}", site.body);
    assert_eq!(site.body["steps"]["site"], json!(true));

    let twice = harness
        .call(post_authed("/api/v1/onboarding/site", &json!({ "name": "Another" }), &owner))
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
    let (account, bootstrap) = harness
        .direct_account(&format!("boot-{}@omnion.test", Uuid::new_v4().simple()))
        .await;
    let _account = account;

    let status = harness.call(get("/api/v1/onboarding", None)).await;
    assert_eq!(status.status, StatusCode::OK);
    assert_eq!(status.body["needs_setup"], json!(false));
    assert_eq!(status.body["in_progress"], json!(true));
    assert_eq!(status.body["steps"]["owner"], json!(true));

    // Creating a "first" owner is refused: this installation already has accounts.
    let late = harness
        .call(post(
            "/api/v1/onboarding/owner",
            &json!({ "display_name": "Late", "email": "late@omnion.test", "password": PASSWORD }),
            None,
        ))
        .await;
    assert_eq!(late.status, StatusCode::CONFLICT);
    assert_eq!(late.body["error"]["code"], "already_installed");

    // The account that is already there may still finish the first run through the wizard — and it
    // is `bootstrap`, not the refused response above. The first draft of this rewrite passed
    // `&owner` (a `TestResponse` for a 409) and got a type error; had it type-checked by
    // borrowing, the write would have been refused with no session and the assertion below would
    // have read that refusal as "the wizard cannot close".
    let organization = harness
        .call(post_authed(
            "/api/v1/onboarding/organization",
            &json!({ "name": "Bootstrap Ltd" }),
            &bootstrap,
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
