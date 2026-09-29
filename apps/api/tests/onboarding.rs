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
    /// Every cookie the response set, in order. Sign-in sets *two* — the session and the CSRF
    /// token — and keeping only the first silently dropped the second, which is why every
    /// cookie-authenticated POST in this suite answered `403 csrf_failed` after the guard
    /// landed: a header set cannot hold both, so the token was discarded rather than refused.
    set_cookies: Vec<String>,
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
        let set_cookies: Vec<String> = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_owned)
            .collect();
        let set_cookie = set_cookies.first().cloned();
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
            set_cookies,
            body,
        }
    }

    /// Create an account directly (bypassing the wizard) and sign it in.
    ///
    /// Create an account directly (bypassing the wizard) and sign it in.
    ///
    /// The session is created through the store rather than through `POST /auth/login`, so no
    /// CSRF cookie is minted. That is the point: the caller gets a session token **only**, and
    /// every mutation it makes is refused at the CSRF guard.
    ///
    /// Which is a trap for a test that wants to prove something *else* about the account — one
    /// that asserts a stranger cannot finish the first run would otherwise be asserting the
    /// CSRF guard, and would keep passing if the ownership check were deleted. A caller that
    /// needs a usable session signs in over HTTP with [`sign_in`].
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
        // An account that arrives from outside the wizard is still an account of a platform
        // whose roles may not be seeded yet — the API bootstraps the first user, and this is
        // the shape that produces. Seeding here mirrors what the API does at boot, so a test
        // about the *first run* is not really a test about an unseeded installation.
        omnion_permissions::seed::ensure(self.db.pool())
            .await
            .expect("the permission catalogue must be seedable");
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

/// The CSRF cookie a sign-in set, if it set one.
fn csrf_of(response: &TestResponse) -> Option<String> {
    response
        .set_cookies
        .iter()
        .filter_map(|cookie| cookie.split(';').next())
        .find_map(|cookie| {
            cookie
                .strip_prefix("omnion_csrf=")
                .map(str::to_owned)
        })
}

/// A cookie header carrying the session *and* the CSRF token sign-in issued.
///
/// The middleware treats a session cookie as ambient authority, so a cookie-authenticated
/// mutation must present the token too. Before this, the suite sent the session alone and every
/// step after the owner answered `403 csrf_failed` — four tests red on a change that landed in
/// `main` and not here, and the failure text pointed at the CSRF guard rather than at the
/// request builder that dropped the cookie.
fn session_cookie(response: &TestResponse) -> String {
    let session = token_of(response);
    match csrf_of(response) {
        Some(csrf) => format!("{session}; omnion_csrf={csrf}"),
        None => session,
    }
}

/// A GET request, optionally with the caller's cookies.
///
/// The value is a *cookie string*, not a session token: [`session_cookie`] builds
/// `token; omnion_csrf=token` and the header carries it verbatim.
fn get(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let builder = Request::builder().method("GET").uri(uri);
    let builder = match cookie {
        Some(cookies) => builder.header(header::COOKIE, format!("omnion_session={cookies}")),
        None => builder,
    };
    builder.body(Body::empty()).expect("request must build")
}

/// A POST request with a JSON body, optionally with the caller's cookies.
fn post(uri: &str, body: &Value, cookie: Option<&str>) -> Request<Body> {
    let builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    let builder = match cookie {
        Some(cookies) => builder.header(header::COOKIE, format!("omnion_session={cookies}")),
        None => builder,
    };
    builder
        .body(Body::from(body.to_string()))
        .expect("request must build")
}

/// Sign an account in over HTTP and return a cookie string a mutation can carry.
///
/// The store-level [`Harness::direct_account`] deliberately mints no CSRF token, so a test that
/// needs to *reach* a later check has to sign in the way a browser does.
async fn sign_in(harness: &Harness, email: &str) -> String {
    let response = harness
        .call(post(
            "/api/v1/auth/login",
            &json!({ "email": email, "password": PASSWORD }),
            None,
        ))
        .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the account must be able to sign in: {:?}",
        response.body
    );
    session_cookie(&response)
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
    session_cookie(&response)
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
    let token = session_cookie(&owner);

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

    // A second account exists (invited later) but is not the owner. It signs in properly, so
    // the refusal below is the *ownership* check and not the CSRF guard standing in front of
    // it — a test that cannot tell those two apart proves nothing about either.
    let stranger = format!("other-{}@omnion.test", Uuid::new_v4().simple());
    harness.direct_account(&stranger).await;
    let intruder = sign_in(&harness, &stranger).await;

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
    // It signs in over HTTP because the step below is a mutation, and a store-level session
    // carries no CSRF token — the guard would refuse it first and the assertion below would be
    // about the guard rather than about the first run.
    let bootstrap_email = format!("boot-{}@omnion.test", Uuid::new_v4().simple());
    let (account, _token) = harness.direct_account(&bootstrap_email).await;
    let _account = account;
    let token = sign_in(&harness, &bootstrap_email).await;

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

/// The account that creates the organization must be able to *use* it.
///
/// The first run creates the owner platform-level on purpose — an Owner runs the platform, not
/// one tenant — and `set_organization` only ever wrote the onboarding-state singleton. So after
/// a complete, successful wizard the owner still had `users.organization_id = null` and no
/// organization-scoped binding, and every organization-scoped surface answered
/// `no_organization`: the lead inbox, the media library, the analytics. The wizard reported
/// success and the overview loaded, so the failure surfaced on the first *business* screen a
/// new installation ever opens.
///
/// Two assertions, because either one alone is satisfied by a broken fix: the row must carry
/// the organization, and an organization-scoped Owner binding must exist. The second is the
/// one a fix that only sets the column would miss.
#[tokio::test]
async fn the_owner_of_the_first_run_belongs_to_the_organization_it_created() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!(
            "skipping: the compose stack is not running \
             (`docker compose -f infra/compose/docker-compose.dev.yml up -d`)"
        );
        return;
    };

    let token = create_owner(&harness, "owner@omnion.test").await;

    // Before the organization step the account is deliberately platform-level.
    let before: Option<Uuid> =
        sqlx::query_scalar("select organization_id from users where email = 'owner@omnion.test'")
            .fetch_one(harness.db.pool())
            .await
            .expect("the owner row must be readable");
    assert_eq!(
        before, None,
        "the owner is platform-level until the organization exists"
    );

    let organization = harness
        .call(post(
            "/api/v1/onboarding/organization",
            &json!({ "name": "Acme Corporation", "slug": "acme" }),
            Some(&token),
        ))
        .await;
    assert_eq!(organization.status, StatusCode::OK, "{:?}", organization.body);
    // The step answers with the *status*, not the organization, so the id is read where the
    // row is. Asking the response for it would be a test of a field the route does not claim.
    let organization_id: Uuid = sqlx::query_scalar("select id from organizations where slug = 'acme'")
        .fetch_one(harness.db.pool())
        .await
        .expect("the organization must exist");

    // 1. The account is a member of the organization it just created.
    let after: Option<Uuid> =
        sqlx::query_scalar("select organization_id from users where email = 'owner@omnion.test'")
            .fetch_one(harness.db.pool())
            .await
            .expect("the owner row must be readable");
    assert_eq!(
        after,
        Some(organization_id),
        "the owner must belong to the organization it created, or every \
         organization-scoped screen answers no_organization"
    );

    // 2. And it holds Owner *at that organization*, which is what makes a tenant-scoped
    //    permission check succeed rather than merely resolve to a tenant.
    let scoped: i64 = sqlx::query_scalar(
        "select count(*) from role_bindings b join roles r on r.id = b.role_id \
         where b.subject_type = 'user' and b.subject_id = \
             (select id from users where email = 'owner@omnion.test') \
           and r.key = 'owner' and b.scope_type = 'organization' \
           and b.organization_id = $1 and b.revoked_at is null",
    )
    .bind(organization_id)
    .fetch_one(harness.db.pool())
    .await
    .expect("the bindings must be readable");
    assert_eq!(
        scoped, 1,
        "the owner needs an organization-scoped Owner binding, not only a global one"
    );

    // 3. The global binding survives: this attaches the account to a tenant, it does not
    //    demote it from the platform.
    let global: i64 = sqlx::query_scalar(
        "select count(*) from role_bindings b join roles r on r.id = b.role_id \
         where b.subject_type = 'user' and b.subject_id = \
             (select id from users where email = 'owner@omnion.test') \
           and r.key = 'owner' and b.scope_type = 'global' and b.revoked_at is null",
    )
    .fetch_one(harness.db.pool())
    .await
    .expect("the bindings must be readable");
    assert_eq!(global, 1, "the platform Owner binding must not be replaced");

    // 4. Idempotent: a retried first run must not move the account, because the attach only
    //    fires on a `null` organization and a second call would otherwise be a silent transfer
    //    between tenants — a different operation with its own permission.
    let error = omnion_identity::users::attach_to_organization(
        harness.db.pool(),
        uuid_of_owner(&harness).await,
        Uuid::new_v4(),
    )
    .await
    .expect_err("a second attach must be refused, not applied");
    assert!(
        error.to_string().contains("already belongs"),
        "the refusal must say why: {error}"
    );

    harness.dispose().await;
}

/// The id of the owner account the first run created.
async fn uuid_of_owner(harness: &Harness) -> Uuid {
    sqlx::query_scalar("select id from users where email = 'owner@omnion.test'")
        .fetch_one(harness.db.pool())
        .await
        .expect("the owner row must be readable")
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
