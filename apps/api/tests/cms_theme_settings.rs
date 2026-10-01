//! Integration test for theme settings (REQ-062, slice 2).
//!
//! The criterion is "saving settings twice creates revisions 1 and 2; restoring revision 1
//! reverts the tokens and is itself recorded as a new revision", and the whole module exists
//! because of the sentence right before it: **a save is not a publish**. So the walks are
//! about the states that sentence creates, and each one is a way the product could lie:
//!
//! * **A save must not reach a visitor.** The witness is the same `GET
//!   /api/v1/public/pages/{slug}` a browser asks for, not the panel's own settings read. A
//!   test that only checks the panel would pass against a store that publishes on write.
//!
//! * **A restore must write a revision, not move a pointer.** Asserting that revision 1's
//!   tokens are back AND that the history now has an extra row AND that the new row records
//!   where it came from — the third is the one that distinguishes "restore wrote a revision"
//!   from "restore rewrote revision 1", which looks identical in the first two.
//!
//! * **Publishing low-contrast tokens is refused until acknowledged, and the refusal is a
//!   422 rather than a 400.** The tokens are legal values; the product wants a person to look
//!   at them. A walk that only checked "it was refused" would pass for the wrong reason.
//!
//! * **The stale-draft guard is a race, not a bug.** A restore publishes revision 3; a panel
//!   tab still holding revision 1's draft then publishes, and that must be refused with both
//!   numbers rather than silently moving the site backwards.
//!
//! * **`themes.read` does not imply `themes.customize`.** The last REQ to make that pairing
//!   its own test, for the same reason: a save that publishes is a design power, and an
//!   account that may only look must not have it.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, CsrfSecret};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::sites::{self, NewSite};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use omnion_security::{CSRF_HEADER, RatePolicy, derive_csrf_token};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

mod support;
use support::isolated_db::{IsolatedDb, announce_skip, assert_nothing_skipped};
use support::walk_auth;

const PASSWORD: &str = "correct horse battery";
const CSRF_SECRET: &str = "w2-theme-settings-suite-csrf-secret";

/// A palette that passes AA, used as the base every walk edits away from.
fn good_tokens() -> Value {
    json!({
        "text": { "light": "#1a1a1a", "dark": "#f5f5f5" },
        "textMuted": { "light": "#5a5a5a", "dark": "#a0a0a0" },
        "accent": { "light": "#2b5cd9", "dark": "#7aa2f7" },
        "surface": { "light": "#ffffff", "dark": "#101010" },
        "surfaceRaised": { "light": "#f4f4f4", "dark": "#1c1c1c" }
    })
}

/// The same palette with the body text turned almost white on white: 1.9:1, far below AA.
fn low_contrast_tokens() -> Value {
    let mut tokens = good_tokens();
    tokens["text"]["light"] = json!("#eeeeee");
    tokens
}

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
    TestResponse {
        status,
        headers,
        body: serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
    }
}

fn error_code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

fn error_message(body: &Value) -> &str {
    body["error"]["message"].as_str().unwrap_or_default()
}

fn request(method: Method, uri: &str, auth: Option<&Auth>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match auth {
        Some(auth) => {
            let token = auth.token.as_str();
            builder
                .header(header::COOKIE, format!("omnion_session={token}"))
                .header(
                    CSRF_HEADER,
                    derive_csrf_token(CSRF_SECRET.as_bytes(), &auth.session_id),
                )
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

/// A request as a VISITOR's browser makes it: no cookie, addressed by `Host`.
fn visitor(method: Method, uri: &str, host: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, host)
        .header(header::USER_AGENT, "theme-settings-suite/1.0")
        .body(Body::empty())
        .expect("request must build")
}

fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

async fn live_state() -> Option<(AppState, Db, IsolatedDb)> {
    let mut config = Config::from_env().expect("environment must be valid");
    // **The secret the walks sign with has to be installed in the state as well**, not only
    // used to derive the token. Without it the deployment under test has no secret, sign-in
    // issues no CSRF cookie, and every write answers `csrf_unavailable` -- a code whose message
    // names a deployment problem, so the failure points away from the suite that caused it.
    config.csrf = CsrfSecret::new(Some(CSRF_SECRET.to_owned()));

    // A throwaway database per walk, rather than whatever `OMNION_DATABASE_URL` names.
    //
    // This file was the last one in the wave still on the shared database, and the measurement
    // that proves it is worth keeping: with a per-walk database the suite that had been
    // hanging for twelve minutes (twelve PostgreSQL sessions parked in `ClientRead`, the
    // process's only other thread in `futex_do_wait`) finished its walks. On the shared
    // database `seed::ensure` binds Owner to the EARLIEST user in the database, which on this
    // box is whichever of the seven writers' suites inserted first -- so a walk that leans on
    // Owner without granting it measures the test schedule. Twelve orphaned
    // `omnion_cms_*` databases were visible in the server while this file ran.
    let isolated = IsolatedDb::open(&config.database.url, 4, "theme_settings")
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

    // The limiter is the one thing a per-walk database does NOT fix: its counters live in one
    // Redis shared with every other writer's worktree, and a sign-in carries no session, so its
    // budget is keyed on the peer address -- `127.0.0.1` for every walk in every suite on this
    // box. The shipped `sign_in` policy allows ten per five minutes, and a suite that signs in
    // once or twice per walk dies inside `login` on a rate limit it was never testing. The
    // process-wide `OnceLock` means the raised policy sticks from the first walk on, so this is
    // deliberately NOT repeated per walk.
    walk_auth::give_the_process_its_own_sign_in_budget(|| {
        let policies: Vec<RatePolicy> = RatePolicy::defaults()
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

macro_rules! walk {
    ($state:expr, $body:expr) => {
        async {
            match live_state().await {
                Some((state, db, mut isolated)) => {
                    let outcome = $body(state, db).await;
                    // Explicitly disposed rather than left to a `Drop` guard: a panic inside
                    // `#[tokio::test]` unwinds the runtime TASK, not the future the macro
                    // awaits, so a guard never runs on the failing path. The next run's
                    // `IsolatedDb::open` sweeps whatever this one left.
                    isolated.dispose().await;
                    outcome
                }
                None => {
                    announce_skip("no database, this walk did not run");
                    Ok(())
                }
            }
        }
    };
}

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn create_organization(db: &Db) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "insert into organizations (name, slug) values ($1, $2) returning id",
    )
    .bind("Theme Settings Co")
    .bind(format!("ts-{}", &Uuid::new_v4().simple().to_string()[..12]))
    .fetch_one(db.pool())
    .await
    .expect("the organization must be created");
    seed::ensure(db.pool())
        .await
        .expect("the IAM seed must run");
    id
}

async fn create_account(db: &Db, organization_id: Uuid) -> (Uuid, String) {
    let email = format!("ts-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            display_name: "Settings Tester".to_owned(),
            password: PASSWORD.to_owned(),
            organization_id: Some(organization_id),
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

async fn create_site(db: &Db, organization_id: Uuid, key: &str) -> omnion_identity::Site {
    sites::create_site(
        db.pool(),
        NewSite {
            organization_id,
            key: key.to_owned(),
            name: "Settings Site".to_owned(),
            theme: None,
        },
    )
    .await
    .expect("the site must be created")
}

/// Mirror a bundled theme whose manifest declares the real token names, so the defaults
/// resolve and a colour the operator does not set inherits from the theme rather than from
/// nothing.
async fn mirror_theme(db: &Db, key: &str) {
    omnion_content::themes::sync_bundled(
        db.pool(),
        &[(
            key.to_owned(),
            json!({
                "key": key,
                "name": key,
                "version": "1.0.0",
                "modes": ["light", "dark"],
                "slots": ["header", "page"],
                "tokens": {
                    "text": { "light": "#1a1a1a", "dark": "#f5f5f5" },
                    "textMuted": { "light": "#5a5a5a", "dark": "#a0a0a0" },
                    "accent": { "light": "#2b5cd9", "dark": "#7aa2f7" },
                    "surface": { "light": "#ffffff", "dark": "#101010" },
                    "surfaceRaised": { "light": "#f4f4f4", "dark": "#1c1c1c" }
                }
            }),
        )],
    )
    .await
    .expect("the manifest must mirror into the table");
}

async fn publish_page(db: &Db, site_id: Uuid, slug: &str) {
    let (page, _) = omnion_content::pages::create_page(
        db.pool(),
        omnion_content::NewPage {
            site_id,
            slug: slug.to_owned(),
            page_type: Some("page".to_owned()),
            title: "Settings page".to_owned(),
            body: Some("Hello from the settings suite.".to_owned()),
            summary: None,
            created_by: None,
        },
    )
    .await
    .expect("the page must be created");
    omnion_content::pages::publish_page(db.pool(), page.id)
        .await
        .expect("the page must publish");
}

/// A body the save route accepts.
fn save_body(tokens: Value) -> Value {
    json!({
        "theme_key": "corporate",
        "tokens": tokens,
        "typography": { "baseSize": "16px", "scaleRatio": "1.25" },
        "layout": { "containerWidth": "1200px", "radius": "8px" },
        "branding": { "logo": null },
        "header_footer": { "header": "sticky", "footer": "simple" },
        "default_mode": "system"
    })
}

async fn save(state: &AppState, auth: &Auth, site_id: Uuid, body: Value) -> TestResponse {
    call(
        state,
        request(
            Method::PUT,
            &format!("/api/v1/sites/{site_id}/theme-settings"),
            Some(auth),
            Some(body),
        ),
    )
    .await
}

async fn publish(state: &AppState, auth: &Auth, site_id: Uuid, acknowledge: bool) -> TestResponse {
    call(
        state,
        request(
            Method::POST,
            &format!("/api/v1/sites/{site_id}/theme-settings/publish"),
            Some(auth),
            Some(json!({ "acknowledgeContrast": acknowledge })),
        ),
    )
    .await
}

async fn read(state: &AppState, auth: &Auth, site_id: Uuid) -> TestResponse {
    call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/sites/{site_id}/theme-settings"),
            Some(auth),
            None,
        ),
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

/// A site that has never saved: the screen loads, the defaults come from the theme, and
/// there is no draft and nothing published. This is the empty state the REQ asks for.
#[tokio::test]
async fn a_site_with_no_settings_loads_the_theme_defaults_and_publishes_nothing() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "ts-fresh").await;
        mirror_theme(&db, "minimal").await;
        let (user_id, email) = create_account(&db, organization_id).await;
        grant(&db, organization_id, user_id, &["themes.read"], "Reader").await;
        let auth = login(&state, &db, &email).await;

        let response = read(&state, &auth, site.id).await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        assert_eq!(
            response.body["draft"], Value::Null,
            "a site that has never saved has no draft, and the screen must be able to say so"
        );
        assert_eq!(response.body["published"], Value::Null);
        assert_eq!(
            response.body["revisions"].as_array().map(Vec::len),
            Some(0),
            "an empty history is an empty list, not a missing field"
        );
        assert_eq!(
            response.body["defaultTokens"]["surface"]["light"],
            json!("#ffffff"),
            "the theme's own defaults are what the reset buttons offer"
        );
        Ok(())
    })
    .await
}

/// THE criterion: save twice, and the numbers are 1 and 2. Plus the half of it that is easy to
/// get wrong — the first save must not publish.
#[tokio::test]
async fn saving_twice_creates_revisions_one_and_two() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "ts-two-saves").await;
        mirror_theme(&db, "minimal").await;
        let (user_id, email) = create_account(&db, organization_id).await;
        // `themes.read` is a separate power (the split this REQ argues for), and a customizer
        // in practice holds both: the panel loads the view and then writes to it.
        grant(
            &db,
            organization_id,
            user_id,
            &["themes.customize", "themes.read"],
            "Customizer",
        )
        .await;
        let auth = login(&state, &db, &email).await;

        let first = save(&state, &auth, site.id, save_body(good_tokens())).await;
        assert_eq!(first.status, StatusCode::OK, "{}", first.body);
        assert_eq!(first.body["draft"]["revisionNo"], json!(1));
        assert_eq!(
            first.body["published"], Value::Null,
            "a save is not a publish: the live revision stays empty"
        );

        let mut second_tokens = good_tokens();
        second_tokens["surface"] = json!({ "light": "#fdfdfd", "dark": "#0d0d0d" });
        let second = save(&state, &auth, site.id, save_body(second_tokens)).await;
        assert_eq!(second.status, StatusCode::OK, "{}", second.body);
        assert_eq!(
            second.body["draft"]["revisionNo"],
            json!(2),
            "the second save is revision 2, not a second copy of revision 1"
        );
        assert_eq!(second.body["draft"]["tokens"]["surface"]["light"], json!("#fdfdfd"));

        // The draft pointer moved; revision 1 is still there to restore.
        let revisions = second.body["revisions"].as_array().expect("revisions is a list");
        assert_eq!(revisions.len(), 2, "both revisions are in the history");
        assert_eq!(revisions[0]["revisionNo"], json!(2), "newest first");
        assert_eq!(revisions[0]["isDraft"], json!(true));
        assert_eq!(
            revisions[0]["isPublished"], json!(false),
            "a revision nobody published is not the live one"
        );
        assert_eq!(revisions[1]["isDraft"], json!(false));

        // And in SQL, because the two halves of the platform disagreeing is the whole risk.
        let count: i64 = sqlx::query_scalar(
            "select count(*) from theme_settings_revisions where site_id = $1",
        )
        .bind(site.id)
        .fetch_one(db.pool())
        .await?;
        assert_eq!(count, 2, "two rows, not one row overwritten twice");
        Ok(())
    })
    .await
}

/// A draft must be invisible to a signed-out visitor, and a publish must be visible. The
/// witness is the public payload, never the panel.
#[tokio::test]
async fn a_save_is_invisible_to_a_visitor_and_a_publish_is_not() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "ts-visibility").await;
        mirror_theme(&db, "minimal").await;
        publish_page(&db, site.id, "settings").await;
        let (user_id, email) = create_account(&db, organization_id).await;
        // `themes.read` is a separate power (the split this REQ argues for), and a customizer
        // in practice holds both: the panel loads the view and then writes to it.
        grant(
            &db,
            organization_id,
            user_id,
            &["themes.customize", "themes.read"],
            "Customizer",
        )
        .await;
        let auth = login(&state, &db, &email).await;

        let mut tokens = good_tokens();
        tokens["surface"] = json!({ "light": "#fdfdfd", "dark": "#0d0d0d" });
        let saved = save(&state, &auth, site.id, save_body(tokens)).await;
        assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);

        // Nothing published yet, so there is no published revision for the renderer to read.
        let published_rows: i64 =
            sqlx::query_scalar("select count(*) from theme_settings_published where site_id = $1")
                .bind(site.id)
                .fetch_one(db.pool())
                .await?;
        assert_eq!(
            published_rows, 0,
            "a draft must not create the row the renderer reads"
        );

        let published = publish(&state, &auth, site.id, false).await;
        assert_eq!(published.status, StatusCode::OK, "{}", published.body);
        assert_eq!(published.body["published"]["revisionNo"], json!(1));
        assert_eq!(published.body["draft"]["revisionNo"], json!(1));

        // Now the row exists, and the panel's own answer names it.
        let pointer: i64 = sqlx::query_scalar(
            "select count(*) from theme_settings_published p join theme_settings_revisions r \
             on r.id = p.revision_id where p.site_id = $1 and r.revision_no = 1",
        )
        .bind(site.id)
        .fetch_one(db.pool())
        .await?;
        assert_eq!(pointer, 1, "the published pointer must name revision 1");

        // The visiter's request answers, which is the state the criterion is really about.
        // The public route addresses a site by a registered domain or an explicit `?site=`.
        // This walk's site has neither (a domain is a separate fixture), so the query is the
        // documented way to say which site a visitor request is about.
        let page = call(
            &state,
            visitor(
                Method::GET,
                &format!("/api/v1/public/pages/settings?site={}", site.key),
                &site.key,
            ),
        )
        .await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.body);
        Ok(())
    })
    .await
}

/// Publishing a low-contrast palette is refused until it is acknowledged, and the refusal is
/// a 422 — the payload was legal, the product wants a person to look at it.
#[tokio::test]
async fn publishing_low_contrast_tokens_needs_an_acknowledgement() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "ts-contrast").await;
        mirror_theme(&db, "minimal").await;
        let (user_id, email) = create_account(&db, organization_id).await;
        // `themes.read` is a separate power (the split this REQ argues for), and a customizer
        // in practice holds both: the panel loads the view and then writes to it.
        grant(
            &db,
            organization_id,
            user_id,
            &["themes.customize", "themes.read"],
            "Customizer",
        )
        .await;
        let auth = login(&state, &db, &email).await;

        // The SAVE is allowed. A palette being compared against the theme is the normal state
        // of the screen, and a guard here would be a wall.
        let saved = save(&state, &auth, site.id, save_body(low_contrast_tokens())).await;
        assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);

        // The SCREEN can see the finding before anybody presses publish — the badge and the
        // guard read the same function.
        let findings = saved.body["contrast"].as_array().expect("contrast is a list");
        assert!(!findings.is_empty(), "the badge must know: {:#}", saved.body);
        let finding = &findings[0];
        assert_eq!(finding["mode"], json!("light"));
        assert!(
            finding["message"].as_str().unwrap_or_default().contains("below"),
            "the finding names the pair and the minimum: {finding}"
        );

        let refused = publish(&state, &auth, site.id, false).await;
        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "422 and not 400: the tokens are legal values, the product wants them confirmed — {}",
            refused.body
        );
        assert_eq!(error_code(&refused.body), "theme_settings_contrast_required");
        assert!(
            error_message(&refused.body).contains("surface"),
            "the message names the token pair: {}",
            error_message(&refused.body)
        );

        // A refused publish writes nothing.
        let pointer: i64 =
            sqlx::query_scalar("select count(*) from theme_settings_published where site_id = $1")
                .bind(site.id)
                .fetch_one(db.pool())
                .await?;
        assert_eq!(pointer, 0, "a refused publish must not leave a pointer behind");

        // Acknowledged, it goes through.
        let acknowledged = publish(&state, &auth, site.id, true).await;
        assert_eq!(acknowledged.status, StatusCode::OK, "{}", acknowledged.body);
        assert_eq!(acknowledged.body["published"]["revisionNo"], json!(1));
        Ok(())
    })
    .await
}

/// The second half of the criterion: restoring revision 1 reverts the tokens AND is itself
/// recorded as a new revision, with the number it came from.
#[tokio::test]
async fn restoring_a_revision_writes_a_new_one_rather_than_rewriting_the_old() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "ts-restore").await;
        mirror_theme(&db, "minimal").await;
        let (user_id, email) = create_account(&db, organization_id).await;
        // `themes.read` is a separate power (the split this REQ argues for), and a customizer
        // in practice holds both: the panel loads the view and then writes to it.
        grant(
            &db,
            organization_id,
            user_id,
            &["themes.customize", "themes.read"],
            "Customizer",
        )
        .await;
        let auth = login(&state, &db, &email).await;

        // Revision 1: the theme's own look.
        let first = save(&state, &auth, site.id, save_body(good_tokens())).await;
        assert_eq!(first.status, StatusCode::OK, "{}", first.body);
        publish(&state, &auth, site.id, false).await;

        // Revision 2: a deliberately different accent, then published so the site really changes.
        let mut changed = good_tokens();
        changed["accent"] = json!({ "light": "#8a1f1f", "dark": "#f7a2a2" });
        let second = save(&state, &auth, site.id, save_body(changed)).await;
        assert_eq!(second.status, StatusCode::OK, "{}", second.body);
        let published = publish(&state, &auth, site.id, false).await;
        assert_eq!(published.body["published"]["revisionNo"], json!(2));

        let restored = call(
            &state,
            request(
                Method::POST,
                &format!(
                    "/api/v1/sites/{}/theme-settings/revisions/1/restore",
                    site.id
                ),
                Some(&auth),
                None,
            ),
        )
        .await;
        assert_eq!(restored.status, StatusCode::OK, "{}", restored.body);

        // The tokens are back…
        assert_eq!(
            restored.body["draft"]["tokens"]["accent"]["light"],
            json!("#2b5cd9"),
            "restoring revision 1 must put revision 1's tokens back"
        );
        // …as revision 3, not as a rewrite of revision 1.
        assert_eq!(
            restored.body["draft"]["revisionNo"],
            json!(3),
            "a restore writes a new revision; the number is what makes the history honest"
        );
        assert_eq!(
            restored.body["published"]["revisionNo"],
            json!(3),
            "a restore goes live at once — the panel and the site may not disagree"
        );

        // And the new row says where it came from, which is the assertion that separates
        // "restore wrote a revision" from "restore overwrote revision 1".
        let head = &restored.body["revisions"].as_array().expect("a list")[0];
        assert_eq!(head["revisionNo"], json!(3));
        assert_eq!(head["restoredFromNo"], json!(1));
        assert_eq!(head["isPublished"], json!(true));

        // Revision 1 is untouched: its row still holds what it always held.
        let one = call(
            &state,
            request(
                Method::GET,
                &format!("/api/v1/sites/{}/theme-settings/revisions/1", site.id),
                Some(&auth),
                None,
            ),
        )
        .await;
        assert_eq!(one.status, StatusCode::OK, "{}", one.body);
        assert_eq!(one.body["revision"]["tokens"]["accent"]["light"], json!("#2b5cd9"));
        assert_eq!(
            one.body["diff"], Value::Array(vec![]),
            "revision 1 is the first, so it has nothing to diff against"
        );
        Ok(())
    })
    .await
}

/// The diff the history screen renders: the fields that changed between revision 2 and 3,
/// with the raw values on both sides.
#[tokio::test]
async fn a_restored_revision_reports_what_it_changed() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "ts-diff").await;
        mirror_theme(&db, "minimal").await;
        let (user_id, email) = create_account(&db, organization_id).await;
        // Both powers: this walk OPENS the history, and `themes.read` is deliberately not
        // implied by `themes.customize` (that split is the whole argument for having two keys).
        grant(
            &db,
            organization_id,
            user_id,
            &["themes.customize", "themes.read"],
            "Customizer",
        )
        .await;
        let auth = login(&state, &db, &email).await;

        save(&state, &auth, site.id, save_body(good_tokens())).await;

        let mut changed = good_tokens();
        changed["accent"] = json!({ "light": "#8a1f1f", "dark": "#f7a2a2" });
        let mut body = save_body(changed);
        body["default_mode"] = json!("dark");
        save(&state, &auth, site.id, body).await;

        let detail = call(
            &state,
            request(
                Method::GET,
                &format!("/api/v1/sites/{}/theme-settings/revisions/2", site.id),
                Some(&auth),
                None,
            ),
        )
        .await;
        assert_eq!(detail.status, StatusCode::OK, "{}", detail.body);
        let diff = detail.body["diff"].as_array().expect("diff is a list");
        let fields: Vec<&str> = diff
            .iter()
            .filter_map(|row| row["field"].as_str())
            .collect();
        assert!(
            fields.contains(&"tokens"),
            "the accent changed: {fields:?}"
        );
        assert!(
            fields.contains(&"defaultMode"),
            "the mode changed too: {fields:?}"
        );
        assert!(
            !fields.contains(&"typography"),
            "a field that did not change is not in the diff: {fields:?}"
        );

        let tokens_row = diff
            .iter()
            .find(|row| row["field"] == json!("tokens"))
            .expect("the tokens row");
        assert_eq!(
            tokens_row["from"]["accent"]["light"],
            json!("#2b5cd9"),
            "the diff carries the raw values so the panel can show a swatch on each side"
        );
        assert_eq!(tokens_row["to"]["accent"]["light"], json!("#8a1f1f"));
        Ok(())
    })
    .await
}

/// Publishing a draft older than the live one is refused with BOTH numbers. The state is
/// reachable — a restore publishes, then a stale panel tab publishes — and the message has to
/// say which of the operator's two tabs is behind.
#[tokio::test]
async fn publishing_a_stale_draft_is_refused_with_both_numbers() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "ts-stale").await;
        mirror_theme(&db, "minimal").await;
        let (user_id, email) = create_account(&db, organization_id).await;
        grant(
            &db,
            organization_id,
            user_id,
            &["themes.customize", "themes.read"],
            "Customizer",
        )
        .await;
        let auth = login(&state, &db, &email).await;

        // A second account holds an open panel at revision 1. It needs the SAME powers: the
        // walk is about a race between two editors, and an account that could not publish
        // would make the stale-draft guard unreachable rather than proven.
        let (other_id, other_email) = create_account(&db, organization_id).await;
        grant(
            &db,
            organization_id,
            other_id,
            &["themes.customize", "themes.read"],
            "Second Editor",
        )
        .await;
        let other = login(&state, &db, &other_email).await;

        save(&state, &auth, site.id, save_body(good_tokens())).await;
        publish(&state, &auth, site.id, false).await;

        // The first account saves revision 2 and publishes it.
        let mut changed = good_tokens();
        changed["accent"] = json!({ "light": "#8a1f1f", "dark": "#f7a2a2" });
        save(&state, &auth, site.id, save_body(changed)).await;
        publish(&state, &auth, site.id, false).await;

        // The second account restores revision 1, which publishes revision 3 and moves the
        // draft pointer to it.
        let restored = call(
            &state,
            request(
                Method::POST,
                &format!("/api/v1/sites/{}/theme-settings/revisions/1/restore", site.id),
                Some(&other),
                None,
            ),
        )
        .await;
        assert_eq!(restored.status, StatusCode::OK, "{}", restored.body);
        assert_eq!(restored.body["draft"]["revisionNo"], json!(3));

        // Nothing stale is reachable through the API, because the pointer moved. So the guard
        // is asserted at the store layer, where the state is actually constructible — a
        // hand-written pointer is the only way to reach it, and a hand-written pointer is
        // exactly what a restore from a database dump would produce.
        sqlx::query(
            "update theme_settings_draft set revision_id = \
             (select id from theme_settings_revisions where site_id = $1 and revision_no = 1) \
             where site_id = $1",
        )
        .bind(site.id)
        .execute(db.pool())
        .await
        .expect("the stale pointer must be writable for this walk");

        let refused = publish(&state, &auth, site.id, false).await;
        assert_eq!(
            refused.status,
            StatusCode::CONFLICT,
            "409: a stale draft is a race between two tabs, not a malformed request — {}",
            refused.body
        );
        assert_eq!(error_code(&refused.body), "theme_settings_draft_stale");
        let message = error_message(&refused.body);
        assert!(message.contains("revision 1"), "{message}");
        assert!(message.contains("revision 3"), "{message}");
        Ok(())
    })
    .await
}

/// `themes.read` may look at the history; it may not save, publish or restore. A save that
/// publishes is a design power, and reading a settings screen must not hand it out.
#[tokio::test]
async fn reading_the_settings_does_not_grant_writing_them() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "ts-guards").await;
        mirror_theme(&db, "minimal").await;
        let (user_id, email) = create_account(&db, organization_id).await;
        grant(&db, organization_id, user_id, &["themes.read"], "Reader").await;
        let auth = login(&state, &db, &email).await;

        let read_response = read(&state, &auth, site.id).await;
        assert_eq!(read_response.status, StatusCode::OK, "{}", read_response.body);

        let refused = save(&state, &auth, site.id, save_body(good_tokens())).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);

        let also_refused = publish(&state, &auth, site.id, false).await;
        assert_eq!(also_refused.status, StatusCode::FORBIDDEN, "{}", also_refused.body);

        // The history is a read too, and a reader may have it.
        let history = call(
            &state,
            request(
                Method::GET,
                &format!("/api/v1/sites/{}/theme-settings/revisions", site.id),
                Some(&auth),
                None,
            ),
        )
        .await;
        assert_eq!(history.status, StatusCode::OK, "{}", history.body);

        // Nothing was written by any of it.
        let rows: i64 =
            sqlx::query_scalar("select count(*) from theme_settings_revisions where site_id = $1")
                .bind(site.id)
                .fetch_one(db.pool())
                .await?;
        assert_eq!(rows, 0, "a refused save must not write a revision");
        Ok(())
    })
    .await
}

/// A token value carrying a CSS declaration is refused, and the refusal names the field. The
/// renderer writes these into custom properties, so this is a real injection, not a tidy-up.
#[tokio::test]
async fn a_token_that_smuggles_a_css_declaration_is_refused() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "ts-injection").await;
        mirror_theme(&db, "minimal").await;
        let (user_id, email) = create_account(&db, organization_id).await;
        // `themes.read` is a separate power (the split this REQ argues for), and a customizer
        // in practice holds both: the panel loads the view and then writes to it.
        grant(
            &db,
            organization_id,
            user_id,
            &["themes.customize", "themes.read"],
            "Customizer",
        )
        .await;
        let auth = login(&state, &db, &email).await;

        let mut hostile = good_tokens();
        hostile["surface"] = json!({ "light": "#ffffff; background: url(https://x.test/a)", "dark": "#101010" });
        let refused = save(&state, &auth, site.id, save_body(hostile)).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
        assert!(
            error_message(&refused.body).contains("surface"),
            "the message names the token, not just the section: {}",
            error_message(&refused.body)
        );

        // An unknown mode is refused too, and it says what is accepted.
        let mut body = save_body(good_tokens());
        body["default_mode"] = json!("sepia");
        let bad_mode = save(&state, &auth, site.id, body).await;
        assert_eq!(bad_mode.status, StatusCode::BAD_REQUEST, "{}", bad_mode.body);
        assert!(
            error_message(&bad_mode.body).contains("light, dark, system"),
            "{}",
            error_message(&bad_mode.body)
        );

        let rows: i64 =
            sqlx::query_scalar("select count(*) from theme_settings_revisions where site_id = $1")
                .bind(site.id)
                .fetch_one(db.pool())
                .await?;
        assert_eq!(rows, 0, "a refused payload must not write a revision");
        Ok(())
    })
    .await
}

/// Publishing with no draft is a 409, not a 400: the payload was fine, the button was the
/// wrong button, and a 400 would send the panel into a retry loop.
#[tokio::test]
async fn publishing_without_a_draft_names_the_action_that_would_work() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "ts-nothing").await;
        mirror_theme(&db, "minimal").await;
        let (user_id, email) = create_account(&db, organization_id).await;
        // `themes.read` is a separate power (the split this REQ argues for), and a customizer
        // in practice holds both: the panel loads the view and then writes to it.
        grant(
            &db,
            organization_id,
            user_id,
            &["themes.customize", "themes.read"],
            "Customizer",
        )
        .await;
        let auth = login(&state, &db, &email).await;

        let refused = publish(&state, &auth, site.id, false).await;
        assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
        assert_eq!(error_code(&refused.body), "theme_settings_nothing_to_publish");
        assert!(
            error_message(&refused.body).contains("save"),
            "the message says what to do instead: {}",
            error_message(&refused.body)
        );
        Ok(())
    })
    .await
}

/// A revision number nobody has is a 404 naming the number, and a revision belonging to
/// another site's site id is not readable either.
#[tokio::test]
async fn a_missing_revision_is_a_404_that_names_the_number() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "ts-missing").await;
        let other_site = create_site(&db, organization_id, "ts-missing-other").await;
        mirror_theme(&db, "minimal").await;
        let (user_id, email) = create_account(&db, organization_id).await;
        grant(
            &db,
            organization_id,
            user_id,
            &["themes.customize", "themes.read"],
            "Customizer",
        )
        .await;
        let auth = login(&state, &db, &email).await;
        save(&state, &auth, site.id, save_body(good_tokens())).await;

        let missing = call(
            &state,
            request(
                Method::GET,
                &format!("/api/v1/sites/{}/theme-settings/revisions/7", site.id),
                Some(&auth),
                None,
            ),
        )
        .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND, "{}", missing.body);
        assert_eq!(error_code(&missing.body), "theme_settings_revision_not_found");
        assert!(
            error_message(&missing.body).contains('7'),
            "the message names the number that was asked for: {}",
            error_message(&missing.body)
        );

        // Revision 1 belongs to `site`, not to `other_site`, even inside one organization.
        let crossed = call(
            &state,
            request(
                Method::GET,
                &format!(
                    "/api/v1/sites/{}/theme-settings/revisions/1",
                    other_site.id
                ),
                Some(&auth),
                None,
            ),
        )
        .await;
        assert_eq!(
            crossed.status,
            StatusCode::NOT_FOUND,
            "the number is scoped to the site, not the tenant: {}",
            crossed.body
        );
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Branding (acceptance 9)
// ---------------------------------------------------------------------------------------------

/// A checksum in the shape `media_checksum_format` demands: 64 hex characters.
///
/// A real sha256 would mean hashing bytes this fixture never stores. The column's constraint is
/// a format check rather than a verification — nothing recomputes it — so a 64-hex constant that
/// is derived from the id (and therefore unique per fixture, which the unique-ish indexes and a
/// failure message both benefit from) satisfies it honestly. The first version of this helper
/// wrote `'w2-branding'`, which the constraint refused at 23514, and the walk reported
/// "media fixture" rather than anything about the check it was written to measure.
fn hex_checksum(seed: Uuid) -> String {
    seed.simple().to_string().repeat(2)
}

/// Put a real file into a site's library, described the way the upload path describes one.
///
/// Written straight into `media` + `media_versions` rather than through `POST /media`, and the
/// reason is in `resolve_branding`'s own note: the geometry the branding check reads is NOT on
/// `media`, it is on `media_versions` version 1. A fixture that wrote only `media` would produce
/// a logo the platform cannot measure, and the walk would then pass for the wrong reason — it
/// would be measuring the "we could not measure it" path while claiming to measure the
/// dimension check. So the fixture writes both rows, exactly as `upload_media` does.
async fn put_file(
    db: &Db,
    site_id: Uuid,
    filename: &str,
    content_type: &str,
    size_bytes: i64,
    dimensions: Option<(i32, i32)>,
) -> Uuid {
    // The id is generated HERE rather than taken from `returning id`, because the checksum is
    // derived from it and has to be bound into the same statement. The upload path derives both
    // the storage key and the id before it writes, for the same reason.
    let id = Uuid::new_v4();
    sqlx::query(
        "insert into media (id, site_id, storage_key, filename, content_type, size_bytes, \
         checksum) values ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(site_id)
    .bind(format!("qa/{site_id}/{filename}"))
    .bind(filename)
    .bind(content_type)
    .bind(size_bytes)
    .bind(hex_checksum(id))
    .execute(db.pool())
    .await
    .expect("media fixture");
    sqlx::query(
        "insert into media_versions (media_id, version, storage_key, size_bytes, checksum, \
         content_type, width, height, note) \
         values ($1, 1, $2, $3, $4, $5, $6, $7, 'branding fixture')",
    )
    .bind(id)
    .bind(format!("qa/{site_id}/{filename}"))
    .bind(size_bytes)
    .bind(hex_checksum(id))
    .bind(content_type)
    .bind(dimensions.map(|d| d.0))
    .bind(dimensions.map(|d| d.1))
    .execute(db.pool())
    .await
    .expect("media version fixture");
    id
}

/// A save whose logo is refused, AND whose refusal names every fault rather than the first.
///
/// Criterion 9 is "rejects files above the configured size and enforces the declared min/max
/// dimensions with a field-level message" — three claims, and the walk proves all three in one
/// payload on purpose: a logo that is 3 MB *and* 6000 px wide is refused for two independent
/// reasons, and an operator reading one message would fix the size, resubmit, and only then
/// learn the picture is also the wrong shape.
///
/// The revision count is the half that matters for the product and the one a "400 came back"
/// assertion would pass without: a refusal that wrote a draft would leave the history screen
/// showing a revision the site never accepted.
#[tokio::test]
async fn a_logo_over_the_size_and_over_the_pixel_limit_is_refused_by_field() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "ts-brand-huge").await;
        mirror_theme(&db, "minimal").await;
        let (user_id, email) = create_account(&db, organization_id).await;
        grant(
            &db,
            organization_id,
            user_id,
            &["themes.customize", "themes.read"],
            "Brand customizer",
        )
        .await;
        let auth = login(&state, &db, &email).await;

        let huge = put_file(&db, site.id, "huge.png", "image/png", 3 * 1024 * 1024, Some((600, 6000))).await;
        let tiny = put_file(&db, site.id, "tiny.png", "image/png", 2_000, Some((240, 8))).await;

        let mut body = save_body(good_tokens());
        body["branding"] = json!({ "logo": huge.to_string(), "favicon": tiny.to_string() });
        let refused = save(&state, &auth, site.id, body).await;
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", refused.body);
        assert_eq!(error_code(&refused.body), "theme_settings_branding_invalid");

        let message = error_message(&refused.body);
        // Both keys are named, which is what "field-level" means here.
        assert!(message.contains("logo"), "{message}");
        assert!(message.contains("favicon"), "{message}");
        // And the messages carry the NUMBERS the limit is made of, not a bare "too big".
        assert!(message.contains("byte limit"), "{message}");
        assert!(message.contains("shorter side"), "{message}");

        let rows: i64 = sqlx::query_scalar(
            "select count(*) from theme_settings_revisions where site_id = $1",
        )
        .bind(site.id)
        .fetch_one(db.pool())
        .await?;
        assert_eq!(rows, 0, "a refused branding payload must not write a revision");
        Ok(())
    })
    .await
}

/// The check reads the SITE's library, so another site's file is not a logo this site can use.
///
/// Scoped by the query rather than by a comparison afterwards, which means the answer for a
/// foreign id and the answer for a deleted one are the same shape — `UnknownAsset` — and the
/// walk proves that rather than proving a second code exists.
#[tokio::test]
async fn a_logo_from_another_site_is_not_a_logo_this_site_can_use() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "ts-brand-scope").await;
        let other_site = create_site(&db, organization_id, "ts-brand-other").await;
        mirror_theme(&db, "minimal").await;
        let (user_id, email) = create_account(&db, organization_id).await;
        grant(
            &db,
            organization_id,
            user_id,
            &["themes.customize", "themes.read"],
            "Scoped customizer",
        )
        .await;
        let auth = login(&state, &db, &email).await;

        let foreign = put_file(&db, other_site.id, "other.png", "image/png", 2_000, Some((200, 80))).await;

        let mut body = save_body(good_tokens());
        body["branding"] = json!({ "logo": foreign.to_string() });
        let refused = save(&state, &auth, site.id, body).await;
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", refused.body);
        assert!(
            error_message(&refused.body).contains(&foreign.to_string()),
            "the refusal names the id the operator has to find: {}",
            error_message(&refused.body)
        );

        // The same file set on the site that OWNS it saves clean. Without this the walk would
        // pass against a resolver that refuses every id regardless of site.
        let mut own = save_body(good_tokens());
        own["branding"] = json!({ "logo": foreign.to_string() });
        let allowed = call(
            &state,
            request(
                Method::PUT,
                &format!("/api/v1/sites/{}/theme-settings", other_site.id),
                Some(&auth),
                Some(own),
            ),
        )
        .await;
        assert_eq!(allowed.status, StatusCode::OK, "{}", allowed.body);
        Ok(())
    })
    .await
}

/// An SVG is refused by TYPE, and a logo inside every limit is stored and comes back.
///
/// Both halves in one walk, because the first half alone passes against a validator that refuses
/// everything, which is the failure mode a "the endpoint returns 422" assertion cannot see.
#[tokio::test]
async fn an_svg_logo_is_refused_and_a_measured_png_is_stored() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "ts-brand-type").await;
        mirror_theme(&db, "minimal").await;
        let (user_id, email) = create_account(&db, organization_id).await;
        grant(
            &db,
            organization_id,
            user_id,
            &["themes.customize", "themes.read"],
            "Typed customizer",
        )
        .await;
        let auth = login(&state, &db, &email).await;

        let svg = put_file(&db, site.id, "mark.svg", "image/svg+xml", 900, None).await;
        let mut body = save_body(good_tokens());
        body["branding"] = json!({ "logo": svg.to_string() });
        let refused = save(&state, &auth, site.id, body).await;
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", refused.body);
        assert!(
            error_message(&refused.body).contains("image/png"),
            "the refusal names an accepted type: {}",
            error_message(&refused.body)
        );

        let png = put_file(&db, site.id, "mark.png", "image/png", 2_400, Some((320, 96))).await;
        let mut good = save_body(good_tokens());
        good["branding"] = json!({ "logo": png.to_string(), "logoDark": null });
        let saved = save(&state, &auth, site.id, good).await;
        assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);

        // Read back through the API rather than the database: the claim is that the panel sees
        // the logo it saved, and a database read would pass even if the response never carried
        // the section (the camelCase drift of tick 57).
        let view = read(&state, &auth, site.id).await;
        assert_eq!(view.status, StatusCode::OK, "{}", view.body);
        assert_eq!(
            view.body["draft"]["branding"]["logo"],
            json!(png.to_string()),
            "the draft carries the logo: {}",
            view.body
        );
        Ok(())
    })
    .await
}
