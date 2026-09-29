//! Integration test for the theme gallery and a site's activation (REQ-062, slice 1).
//!
//! The criterion is "activating a theme from the preview changes what a signed-out visitor
//! sees within one refresh, and keeps the previous theme key for rollback", and the three
//! walks that matter are the ones that can catch a product lying through its own API:
//!
//! * **The renderer is the witness, not the gallery.** An activation is proved by reading
//!   `GET /api/v1/public/pages/{slug}` — the payload a visitor's browser receives. A test that
//!   reads the panel's own gallery proves the gallery agrees with itself, which is the pair
//!   that can agree while the site keeps rendering the old theme.
//!
//! * **Rollback is not "compute the previous key when asked".** After two activations there
//!   are two candidates and no arithmetic that picks the right one, so the walk switches
//!   twice, restores, and then restores AGAIN — which is only possible if the row recorded
//!   what it replaced rather than deriving it.
//!
//! * **A refusal must write nothing.** Activating a key nothing carries, and activating
//!   another organization's upload, are both refused *and* checked in SQL afterwards: a
//!   refusal that left an activation row behind is a site pointing at nothing.
//!
//! Two more things that are easy to ship wrong and are therefore walked here: the **re-activation
//! of the theme that is already active** must not spend the rollback target (it is the case
//! where a naive `on conflict do update` silently makes *Restore previous* a no-op), and the
//! **guards are two**: an account that may read the gallery cannot activate.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::sites::{self, NewSite};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use omnion_security::{CSRF_HEADER, derive_csrf_token};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";
const CSRF_SECRET: &str = "w2-themes-suite-csrf-secret";

/// What the gallery reader holds: it may LOOK and may not activate. The last REQ to make that
/// pairing its own test, for the same reason.
const READER_PERMISSIONS: [&str; 2] = ["themes.read", "content.pages.read"];

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

fn error_message(body: &Value) -> &str {
    body["error"]["message"].as_str().unwrap_or_default()
}

fn error_code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
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
        .header(header::USER_AGENT, "themes-suite/1.0")
        .body(Body::empty())
        .expect("request must build")
}

fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

async fn live_state() -> Option<(AppState, Db)> {
    let config = Config::from_env().expect("environment must be valid");
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(error) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({error})");
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

async fn create_organization(db: &Db) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "insert into organizations (name, slug) values ($1, $2) returning id",
    )
    .bind("Themes Tester Co")
    .bind(format!("themes-{}", &Uuid::new_v4().simple().to_string()[..12]))
    .fetch_one(db.pool())
    .await
    .expect("the organization must be created");
    seed::seed_defaults(db.pool(), Some(id))
        .await
        .expect("the catalogue must seed");
    id
}

async fn create_account(db: &Db, organization_id: Uuid) -> (Uuid, String) {
    let email = format!("themes-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            display_name: "Themes Tester".to_owned(),
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
            name: "Themes Site".to_owned(),
            domain: None,
        },
    )
    .await
    .expect("the site must be created")
}

/// Mirror the platform's bundled manifests, the way boot does.
async fn mirror_bundled(db: &Db, keys: &[&str]) {
    let manifests: Vec<(String, Value)> = keys
        .iter()
        .map(|key| {
            (
                (*key).to_owned(),
                json!({
                    "key": key,
                    "name": key,
                    "version": "1.0.0",
                    "modes": ["light", "dark"],
                    "slots": ["header", "page"],
                    "tokens": { "bg": { "light": "#ffffff" } }
                }),
            )
        })
        .collect();
    omnion_content::themes::sync_bundled(db.pool(), &manifests)
        .await
        .expect("the bundled manifests must mirror into the table");
}

/// A published page the public route can answer with, so "the visitor sees the new theme" is
/// a statement about a real request rather than about a row.
async fn publish_page(db: &Db, site_id: Uuid, slug: &str) {
    let page = omnion_content::pages::create_page(
        db.pool(),
        omnion_content::NewPage {
            site_id,
            slug: slug.to_owned(),
            title: "Themed page".to_owned(),
            page_type: "page".to_owned(),
            created_by: None,
        },
    )
    .await
    .expect("the page must be created");
    omnion_content::pages::update_page(
        db.pool(),
        page.id,
        &omnion_content::PageChanges {
            title: Some("Themed page".to_owned()),
            body: Some("Hello from the theme suite.".to_owned()),
            ..Default::default()
        },
    )
    .await
    .expect("the draft must save");
    omnion_content::pages::publish_page(db.pool(), page.id)
        .await
        .expect("the page must publish");
}

/// Every walk in this file is skipped, loudly, when PostgreSQL is absent — and then asserts.
/// A suite that silently passes because it never ran is a suite that reports a number nobody
/// earned.
macro_rules! walk {
    ($state:expr, $body:expr) => {
        let Some((state, db)) = live_state().await else {
            eprintln!("SKIP: no database, this walk did not run");
            return Ok(());
        };
        $body(state, db).await
    };
}

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// The gallery of a site that never chose a theme: the bundled default is active, the
/// `Active` badge has a card, and *Restore previous* has nothing to restore.
#[tokio::test]
async fn the_gallery_names_the_active_theme_and_offers_no_rollback() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "gallery-fresh").await;
        mirror_bundled(&db, &["minimal", "corporate", "agency"]).await;
        let (_, email) = create_account(&db, organization_id).await;
        let auth = login(&state, &db, &email).await;

        let response = call(
            &state,
            request(
                Method::GET,
                &format!("/api/v1/themes?site={}", site.id),
                Some(&auth),
                None,
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        assert_eq!(
            response.body["activeKey"], json!("minimal"),
            "a site that never chose a theme renders the default"
        );
        assert_eq!(
            response.body["activeKnown"], json!(true),
            "the default theme is one the gallery can show, so the badge has a card"
        );
        assert!(
            response.body["rollbackTarget"].is_null(),
            "nothing to go back to means the button is ABSENT, not armed to change nothing, got \
             {:?}",
            response.body["rollbackTarget"]
        );
        let active_cards = response.body["themes"]
            .as_array()
            .expect("themes is an array")
            .iter()
            .filter(|entry| entry["isActive"] == json!(true))
            .count();
        assert_eq!(active_cards, 1, "exactly one card carries the badge");
        Ok(())
    })
    .await
}

/// The criterion itself: activation reaches the payload a signed-out visitor receives.
#[tokio::test]
async fn activating_a_theme_changes_what_a_signed_out_visitor_receives() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "gallery-activate").await;
        mirror_bundled(&db, &["minimal", "corporate"]).await;
        publish_page(&db, site.id, "themed").await;
        let (user_id, email) = create_account(&db, organization_id).await;
        grant(&db, organization_id, user_id, &["themes.activate"], "Theme Owner");
        let auth = login(&state, &db, &email).await;

        // The site before: the renderer says `minimal`.
        let before = call(
            &state,
            visitor(Method::GET, "/api/v1/public/pages/themed", &site.key),
        )
        .await;
        assert_eq!(before.status, StatusCode::OK, "{}", before.body);
        assert_eq!(before.body["site"]["theme"], json!("minimal"));

        let response = call(
            &state,
            request(
                Method::POST,
                &format!("/api/v1/sites/{}/theme", site.id),
                Some(&auth),
                Some(json!({ "theme_key": "corporate" })),
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        assert_eq!(response.body["themeKey"], json!("corporate"));
        assert_eq!(
            response.body["previousThemeKey"], json!("minimal"),
            "the confirmation strip names what is being replaced"
        );
        assert_eq!(response.body["restored"], json!(false));

        // The visitor's request, again. THIS is the assertion the criterion is about — the
        // gallery agreeing with itself would pass against a broken activation.
        let after = call(
            &state,
            visitor(Method::GET, "/api/v1/public/pages/themed", &site.key),
        )
        .await;
        assert_eq!(after.status, StatusCode::OK, "{}", after.body);
        assert_eq!(
            after.body["site"]["theme"],
            json!("corporate"),
            "the payload a signed-out visitor receives must carry the new theme, or the switch \
             only ever happened in the panel"
        );

        // And the response's own gallery agrees, so the panel needs no second request.
        let gallery = &response.body["gallery"];
        assert_eq!(gallery["activeKey"], json!("corporate"));
        assert_eq!(
            gallery["rollbackTarget"], json!("minimal"),
            "the displaced key is now restorable, which is what makes the button appear"
        );
        Ok(())
    })
    .await
}

/// Rollback after two activations, then rollback AGAIN — the case that separates "records what
/// it replaced" from "computes it when asked".
#[tokio::test]
async fn rollback_restores_the_displaced_key_and_stays_reversible() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "gallery-rollback").await;
        mirror_bundled(&db, &["minimal", "corporate", "agency"]).await;
        let (user_id, email) = create_account(&db, organization_id).await;
        grant(&db, organization_id, user_id, &["themes.activate"], "Theme Owner");
        let auth = login(&state, &db, &email).await;

        for key in ["corporate", "agency"] {
            let response = call(
                &state,
                request(
                    Method::POST,
                    &format!("/api/v1/sites/{}/theme", site.id),
                    Some(&auth),
                    Some(json!({ "theme_key": key })),
                ),
            )
            .await;
            assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        }

        let back = call(
            &state,
            request(
                Method::POST,
                &format!("/api/v1/sites/{}/theme/rollback", site.id),
                Some(&auth),
                None,
            ),
        )
        .await;
        assert_eq!(back.status, StatusCode::OK, "{}", back.body);
        assert_eq!(
            back.body["themeKey"], json!("corporate"),
            "the key the LAST activation displaced is the one to return to, not the first one"
        );
        assert_eq!(back.body["restored"], json!(true));

        // Reversible in the same way an activation is: try a theme, dislike it, go back, and
        // try it again. Only a row that records its displacement can answer this.
        let again = call(
            &state,
            request(
                Method::POST,
                &format!("/api/v1/sites/{}/theme/rollback", site.id),
                Some(&auth),
                None,
            ),
        )
        .await;
        assert_eq!(again.status, StatusCode::OK, "{}", again.body);
        assert_eq!(again.body["themeKey"], json!("agency"));

        // The column, because that is what a visitor's request reads.
        let column: String = sqlx::query_scalar("select theme from sites where id = $1")
            .bind(site.id)
            .fetch_one(db.pool())
            .await?;
        assert_eq!(column, "agency");
        Ok(())
    })
    .await
}

/// A site that never switched has nothing to restore, and the refusal says so with its own
/// code rather than reporting success.
#[tokio::test]
async fn rolling_back_a_site_that_never_switched_is_refused() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "gallery-norollback").await;
        let (user_id, email) = create_account(&db, organization_id).await;
        grant(&db, organization_id, user_id, &["themes.activate"], "Theme Owner");
        let auth = login(&state, &db, &email).await;

        let response = call(
            &state,
            request(
                Method::POST,
                &format!("/api/v1/sites/{}/theme/rollback", site.id),
                Some(&auth),
                None,
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::CONFLICT, "{}", response.body);
        assert_eq!(
            error_code(&response.body),
            "theme_rollback_unavailable",
            "a Restore previous button that re-activates the current theme reports work it did \
             not do, so the refusal needs its own code"
        );
        assert!(!error_message(&response.body).is_empty());
        Ok(())
    })
    .await
}

/// Re-activating the ACTIVE theme spends nothing — the trap that makes a naive upsert turn
/// *Restore previous* into a no-op.
#[tokio::test]
async fn re_activating_the_active_theme_keeps_the_rollback_target() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "gallery-noop").await;
        mirror_bundled(&db, &["minimal", "corporate"]).await;
        let (user_id, email) = create_account(&db, organization_id).await;
        grant(&db, organization_id, user_id, &["themes.activate"], "Theme Owner");
        let auth = login(&state, &db, &email).await;

        let activate = |key: &'static str| {
            let auth = Auth {
                token: auth.token.clone(),
                session_id: auth.session_id.clone(),
            };
            let site_id = site.id;
            async move {
                call(
                    &state,
                    request(
                        Method::POST,
                        &format!("/api/v1/sites/{site_id}/theme"),
                        Some(&auth),
                        Some(json!({ "theme_key": key })),
                    ),
                )
                .await
            }
        };

        let first = activate("corporate").await;
        assert_eq!(first.status, StatusCode::OK, "{}", first.body);
        // The same key, twice more.
        for _ in 0..2 {
            let again = activate("corporate").await;
            assert_eq!(again.status, StatusCode::OK, "{}", again.body);
            assert_eq!(again.body["restored"], json!(false));
            assert_eq!(
                again.body["previousThemeKey"], Value::Null,
                "a repeated activation of the key already in use displaces nothing"
            );
            assert!(
                again.body["gallery"]["rollbackTarget"].is_null(),
                "so the rollback button must stay ABSENT, or it would restore the theme that was \
                 already in use"
            );
        }
        Ok(())
    })
    .await
}

/// An unknown key is refused and writes nothing; another tenant's upload is refused by name.
#[tokio::test]
async fn a_key_no_live_theme_carries_is_refused_and_writes_nothing() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "gallery-unknown").await;
        let other_organization = create_organization(&db).await;
        let (user_id, email) = create_account(&db, organization_id).await;
        grant(&db, organization_id, user_id, &["themes.activate"], "Theme Owner");
        let auth = login(&state, &db, &email).await;

        // An upload that belongs to the OTHER organization, written directly: the installer
        // arrives in slice 3, and the gallery's tenant isolation has to be provable before it.
        sqlx::query(
            "insert into themes (organization_id, key, name, version, source, manifest, \
             storage_key) values ($1, 'tenant-theme', 'Tenant Theme', '1.0.0', 'uploaded', \
             $2, 'packages/tenant.zip')",
        )
        .bind(other_organization)
        .bind(json!({ "key": "tenant-theme", "name": "Tenant Theme", "version": "1.0.0" }))
        .execute(db.pool())
        .await?;

        for key in ["no-such-theme", "tenant-theme"] {
            let response = call(
                &state,
                request(
                    Method::POST,
                    &format!("/api/v1/sites/{}/theme", site.id),
                    Some(&auth),
                    Some(json!({ "theme_key": key })),
                ),
            )
            .await;
            assert_eq!(response.status, StatusCode::NOT_FOUND, "{key}: {}", response.body);
            assert_eq!(error_code(&response.body), "theme_not_found");
            assert!(
                error_message(&response.body).contains(key),
                "the refusal names the key, got: {}",
                error_message(&response.body)
            );
        }

        // A refusal that left a row behind is a site pointing at nothing.
        let rows: i64 = sqlx::query_scalar("select count(*) from site_themes where site_id = $1")
            .bind(site.id)
            .fetch_one(db.pool())
            .await?;
        assert_eq!(rows, 0, "both refusals wrote no activation row");
        let column: String = sqlx::query_scalar("select theme from sites where id = $1")
            .bind(site.id)
            .fetch_one(db.pool())
            .await?;
        assert_eq!(column, "minimal", "and the site still renders what it did before");
        Ok(())
    })
    .await
}

/// `themes.read` may look; it may not activate. A theme switch changes every page a
/// signed-out visitor sees, so the two powers are separate keys.
#[tokio::test]
async fn the_gallery_is_readable_without_the_power_to_activate() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "gallery-guards").await;
        mirror_bundled(&db, &["minimal", "corporate"]).await;
        let (user_id, email) = create_account(&db, organization_id).await;
        grant(&db, organization_id, user_id, &READER_PERMISSIONS, "Theme Reader");
        let auth = login(&state, &db, &email).await;

        let read = call(
            &state,
            request(
                Method::GET,
                &format!("/api/v1/themes?site={}", site.id),
                Some(&auth),
                None,
            ),
        )
        .await;
        assert_eq!(read.status, StatusCode::OK, "{}", read.body);
        assert!(
            !read.body["themes"].as_array().expect("array").is_empty(),
            "the gallery is what a reader came for"
        );

        let write = call(
            &state,
            request(
                Method::POST,
                &format!("/api/v1/sites/{}/theme", site.id),
                Some(&auth),
                Some(json!({ "theme_key": "corporate" })),
            ),
        )
        .await;
        assert_eq!(
            write.status, StatusCode::FORBIDDEN,
            "an account that may LOOK at the gallery must not be able to switch the site's \
             theme, got {}: {}",
            write.status,
            write.body
        );
        let rows: i64 = sqlx::query_scalar("select count(*) from site_themes where site_id = $1")
            .bind(site.id)
            .fetch_one(db.pool())
            .await?;
        assert_eq!(rows, 0, "the refusal wrote nothing");
        Ok(())
    })
    .await
}

/// The gallery is site-scoped: another tenant's package is not a theme this site may choose.
#[tokio::test]
async fn a_tenant_upload_is_not_in_another_tenants_gallery() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let other_organization = create_organization(&db).await;
        let site = create_site(&db, organization_id, "gallery-tenant").await;
        mirror_bundled(&db, &["minimal"]).await;
        let (user_id, email) = create_account(&db, organization_id).await;
        grant(&db, organization_id, user_id, &READER_PERMISSIONS, "Theme Reader");
        let auth = login(&state, &db, &email).await;

        sqlx::query(
            "insert into themes (organization_id, key, name, version, source, manifest, \
             storage_key) values ($1, 'tenant-theme', 'Tenant Theme', '1.0.0', 'uploaded', \
             $2, 'packages/tenant.zip')",
        )
        .bind(other_organization)
        .bind(json!({ "key": "tenant-theme", "name": "Tenant Theme", "version": "1.0.0" }))
        .execute(db.pool())
        .await?;

        let gallery = call(
            &state,
            request(
                Method::GET,
                &format!("/api/v1/themes?site={}", site.id),
                Some(&auth),
                None,
            ),
        )
        .await;
        assert_eq!(gallery.status, StatusCode::OK, "{}", gallery.body);
        let keys: Vec<&str> = gallery.body["themes"]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|entry| entry["key"].as_str())
            .collect();
        assert!(
            !keys.contains(&"tenant-theme"),
            "an upload is code, and code is not shared between tenants: {keys:?}"
        );
        assert!(keys.contains(&"minimal"), "and the bundled theme is still there");
        Ok(())
    })
    .await
}

/// The boot-time mirror, and the two properties that make it safe to run on every start.
#[tokio::test]
async fn bundled_themes_are_mirrored_in_place_and_carry_no_tenant() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "gallery-bundled").await;
        let (user_id, email) = create_account(&db, organization_id).await;
        grant(&db, organization_id, user_id, &READER_PERMISSIONS, "Theme Reader");
        let auth = login(&state, &db, &email).await;

        mirror_bundled(&db, &["corporate", "agency"]).await;
        // A second boot with a newer version of one of them.
        omnion_content::themes::sync_bundled(
            db.pool(),
            &[(
                "corporate".to_owned(),
                json!({ "key": "corporate", "name": "Corporate", "version": "1.1.0" }),
            )],
        )
        .await
        .expect("the second mirror must apply");

        let rows: i64 = sqlx::query_scalar("select count(*) from themes where key = 'corporate'")
            .fetch_one(db.pool())
            .await?;
        assert_eq!(rows, 1, "one live row per key, however many boots mirrored it");
        let version: String = sqlx::query_scalar("select version from themes where key = 'corporate'")
            .fetch_one(db.pool())
            .await?;
        assert_eq!(version, "1.1.0", "the mirror is rebuilt, not merged");

        let organization: Option<Uuid> =
            sqlx::query_scalar("select organization_id from themes where key = 'corporate'")
                .fetch_one(db.pool())
                .await?;
        assert!(organization.is_none(), "a bundled theme belongs to no tenant");

        // And the gallery an ORGANIZATION account sees includes them — the union is what lets
        // a platform owner with no organization of their own still see the ten.
        let gallery = call(
            &state,
            request(
                Method::GET,
                &format!("/api/v1/themes?site={}", site.id),
                Some(&auth),
                None,
            ),
        )
        .await;
        assert_eq!(gallery.status, StatusCode::OK, "{}", gallery.body);
        let keys: Vec<&str> = gallery.body["themes"]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|entry| entry["key"].as_str())
            .collect();
        assert!(keys.contains(&"corporate") && keys.contains(&"agency"), "{keys:?}");
        Ok(())
    })
    .await
}

/// The manifest contract, as a unit: v1 keys are required and the v2 additions are counted,
/// not demanded. A theme that declares no slots still renders, and refusing it would fail the
/// first theme author's first upload on a field the renderer never reads.
#[test]
fn a_manifest_needs_a_key_a_name_and_a_version() {
    let v1 = json!({ "key": "minimal", "name": "Minimal", "version": "0.1.0" });
    let shape = omnion_content::themes::manifest_shape(&v1).expect("a v1 manifest is a manifest");
    assert_eq!(shape.slots, 0);
    assert_eq!(shape.tokens, 0);
    assert!(shape.modes.is_empty());

    let blank = json!({ "key": "minimal", "name": "  ", "version": "0.1.0" });
    assert_eq!(
        omnion_content::themes::manifest_shape(&blank).expect_err("a blank name is not a name"),
        "'name' is blank"
    );

    let missing = json!({ "key": "minimal", "name": "Minimal" });
    assert_eq!(
        omnion_content::themes::manifest_shape(&missing).expect_err("version is required"),
        "'version' is missing"
    );

    // The key is the only field that reaches a URL and a filesystem.
    let bad_key = json!({ "key": "Not A Key", "name": "n", "version": "1" });
    assert!(
        omnion_content::themes::manifest_shape(&bad_key).is_err(),
        "a key with spaces is a path segment nobody can route"
    );

    let v2 = json!({
        "key": "corporate", "name": "Corporate", "version": "1.2.0",
        "modes": ["light", "dark"],
        "slots": ["header", "footer", "home", "page"],
        "tokens": { "bg": { "light": "#fff" }, "ink": { "light": "#000" } },
        "settingsSchema": { "radius": "number" },
        "compatibility": { "engine": ">=0.1" }
    });
    let shape = omnion_content::themes::manifest_shape(&v2).expect("a complete manifest validates");
    assert_eq!(shape.slots, 4);
    assert_eq!(shape.tokens, 2);
    assert_eq!(shape.modes, vec!["light", "dark"]);
    assert!(shape.extras.contains(&"settingsSchema"));
    assert!(shape.extras.contains(&"compatibility"));
}
