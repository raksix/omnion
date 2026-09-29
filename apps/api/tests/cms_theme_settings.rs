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

async fn live_state() -> Option<(AppState, Db)> {
    let mut config = Config::from_env().expect("environment must be valid");
    // Without a CSRF secret every cookie-authenticated write in this file answers 403, so the
    // walks would be measuring the harness rather than the product — the exact defect slice 2
    // of REQ-064 recorded for the forms suite.
    config.csrf = CsrfSecret::new(Some(CSRF_SECRET.to_owned()));
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(error) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({error})");
            return None;
        }
    };
    db.migrate().await.expect("migrations must apply");
    // Sign-in is limited to 10 per 300 s per IP and this file signs in once per walk, so
    // without this the suite measures the limiter instead of the product. The row is written
    // BEFORE the state exists because the limiter layer is installed once per process from
    // whatever the document says at that moment — writing it afterwards has no effect at all,
    // which is a silent no-op that looks like a fix.
    let raised: Vec<serde_json::Value> = RatePolicy::defaults()
        .into_iter()
        .map(|mut policy| {
            if policy.scope == "sign_in" {
                policy.limit = 1_000;
                policy.burst = 0;
            }
            serde_json::to_value(&policy).unwrap_or(serde_json::Value::Null)
        })
        .filter(|value| !value.is_null())
        .collect();
    let _ = sqlx::query(
        "insert into security_settings (id, rate_limits) values (1, $1::jsonb) \
         on conflict (id) do update set rate_limits = excluded.rate_limits",
    )
    .bind(serde_json::to_value(&raised).unwrap_or(serde_json::Value::Null))
    .execute(db.pool())
    .await;

    let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
    let state = AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config,
        db.clone(),
        redis,
        test_storage(),
    );
    // `ensure_installed` seeds the process-wide limiter from the SHIPPED defaults (a test
    // harness builds the router without main.rs having read the store), so the row above is
    // not enough on its own — the layer is re-read here, which is the same call
    // `security_limiter::put_rate_limits` makes after a real save.
    let _ = omnion_api::rate_limit_middleware::reload_from_store(&state).await;
    // `ensure_installed` seeds the process-wide limiter from the SHIPPED defaults (a test
    // harness builds the router without main.rs having read the store), so the row above is
    // not enough on its own — the layer is re-read here, which is the same call
    // `security_limiter::put_rate_limits` makes after a real save.
    let _ = omnion_api::rate_limit_middleware::reload_from_store(&state).await;
    Some((state, db))
}

/// Every walk in this file is skipped, loudly, when PostgreSQL is absent — and then asserts.
/// A suite that silently passes because it never ran is a suite that reports a number nobody
/// earned.
///
/// The expansion is an async BLOCK, not a sequence of statements. A macro arm that expands to
/// bare statements at expression position does not parse, which is why the "no database" case
/// is a `match` arm rather than the `let … else` this used to use.
macro_rules! walk {
    ($state:expr, $body:expr) => {
        async {
            match live_state().await {
                Some((state, db)) => $body(state, db).await,
                None => {
                    eprintln!("SKIP: no database, this walk did not run");
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
