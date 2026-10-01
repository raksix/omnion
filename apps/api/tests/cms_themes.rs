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
use omnion_core::config::{Config, CsrfSecret, DatabaseConfig};
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

/// What a walk needs: its own database, its router, and the handle that drops the database.
struct Harness {
    state: AppState,
    db: Db,
    maintenance: Db,
    database: String,
}

impl Harness {
    /// Open a throwaway database with every migration applied.
    ///
    /// **A database of its own, and this suite previously did not have one.** It ran against
    /// whatever `OMNION_DATABASE_URL` named — on a writer's box, the *shared* QA database
    /// every other stack and every other writer also points at. Two consequences, and both of
    /// them are how a suite reports numbers nobody earned:
    ///
    /// * Two walks inserting the same `themes.key` collide on `themes_key_live_idx`, so the
    ///   failure is a duplicate-key error on a fixed fixture name rather than anything about
    ///   the behaviour under test.
    /// * A run whose credentials were wrong connected nowhere, took the skip branch below, and
    ///   reported `13 passed` — with every walk having done nothing. A green line and an
    ///   empty run are the same output.
    ///
    /// `event_retention` and a dozen other suites already do this; this one had not caught up.
    async fn fresh() -> Option<Self> {
        let mut config = Config::from_env().expect("environment must be valid");
        // A cookie-authenticated write is refused outright when the process has no CSRF
        // secret configured, so a suite that leaves it to the environment measures 403s on
        // whatever machine happens to run it.
        config.csrf = CsrfSecret::new(Some(CSRF_SECRET.to_owned()));

        let maintenance = match Db::connect(&config.database).await {
            Ok(db) => db,
            Err(error) => {
                announce_skip(&format!("PostgreSQL is not reachable ({error})"));
                return None;
            }
        };

        // Drop anything a PREVIOUS run left behind, before taking a new name.
        //
        // **This is the half of the cleanup that actually holds, and finding that out took
        // four attempts.** A `Drop` guard does not: a panic inside `#[tokio::test]` unwinds
        // the runtime TASK, not the future the macro awaits, so a guard in that future never
        // runs — measured, not assumed, and each of the other three shapes failed for its own
        // measurable reason (`catch_unwind` needs the `futures` crate this workspace does not
        // carry; `tokio::spawn`ing the body needs a `Send` result and `Box<dyn Error>` is not
        // one; a detached `handle.spawn` for the drop is not polled before the process exits).
        //
        // The next run is the only process guaranteed to exist on the failing path, so the
        // sweep belongs here. Twelve databases accumulated over six failing runs while the
        // guard was "fixed"; this clears them whatever the previous run did.
        // `r#"..."#` because a `LIKE` pattern needs backslashes to escape its own wildcard
        // characters, and escaping those once for Rust and once for SQL inside a normal
        // string literal is how a query silently matches nothing. The first version of this
        // line was exactly that: correct in psql, empty from the suite.
        let stale: Vec<String> = sqlx::query_scalar(
            r#"
            select datname
              from pg_database
             where datname like 'omnion\_themes\_%'
               and datname <> current_database()
               and not exists (
                   select 1 from pg_stat_activity a where a.datname = pg_database.datname
               )
            "#,
        )
        .fetch_all(maintenance.pool())
        .await
        .unwrap_or_default();
        for database in stale {
            // A failure here is not the walk's failure: another writer may be using it, and
            // a sweep that refused to start would be worse than a sweep that skips one.
            let _ = sqlx::query(&format!("drop database if exists \"{database}\" with (force)"))
                .execute(maintenance.pool())
                .await;
        }

        let database = format!("omnion_themes_{}", &Uuid::new_v4().simple().to_string()[..12]);
        if let Err(error) = sqlx::query(&format!("create database \"{database}\""))
            .execute(maintenance.pool())
            .await
        {
            announce_skip(&format!("a throwaway database could not be created ({error})"));
            return None;
        }

        let db = Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, &database),
            max_connections: 4,
        })
        .await
        .expect("the fresh database must connect");
        db.migrate().await.expect("migrations must apply");

        let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
        let state = AppState::new(
            BuildInfo::new("omnion-api", "0.0.0-test"),
            config,
            db.clone(),
            redis,
            test_storage(),
        );
        give_the_suite_its_own_sign_in_budget(&state);

        Some(Self {
            state,
            db,
            maintenance,
            database,
        })
    }

    /// Drop the throwaway database. Called by the `walk!` macro when the walk RETURNS.
    ///
    /// Not on a panic — see [`Harness::fresh`] for the measurement behind that and for the
    /// sweep that covers it. `dispose` consuming `self` is deliberate: it cannot be called
    /// twice, so there is no path where a walk drops its database and then drops it again.
    async fn dispose(self) {
        self.db.pool().close().await;
        let database = self.database;
        sqlx::query(&format!("drop database if exists \"{database}\" with (force)"))
            .execute(self.maintenance.pool())
            .await
            .expect("the throwaway database must be removed");
    }
}

/// Give this process a sign-in budget large enough for the walks in it.
///
/// **The counter is shared even though the policy need not be.** The limiter's counters live
/// in one Redis, and a sign-in request carries no session, so its budget is keyed on the
/// peer address — `ip:127.0.0.1` for every walk in every suite on this box, in every writer's
/// worktree. The stored `sign_in` policy is ten requests per five minutes, and this file
/// signs in once per walk across thirteen walks. So the eleventh sign-in is refused with a
/// `429` naming a rate limit on a suite that was never testing rate limits, and every walk
/// after it dies on a line that has nothing to do with themes.
///
/// Raising the policy fixes the *decision* but not the counter: a raised ceiling still counts
/// against the same shared key, which is the thing that actually exhausts. What matters here
/// is that this suite is the only writer of its own number, so the refusal is now a fact
/// about the suite instead of a race with nine other writers.
///
/// Only `sign_in` moves. The other ceilings are the ones a deployment ships, and raising
/// them would let a suite be the reason a genuinely over-budget request stops being refused.
fn give_the_suite_its_own_sign_in_budget(state: &AppState) {
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
    omnion_api::rate_limit_middleware::install(omnion_api::rate_limit_middleware::RateLimiter::new(
        state,
        policies,
    ));
}

/// Point a connection string at a different database, keeping host, port and credentials.
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

/// Say a walk did not run, in a way cargo cannot hide.
///
/// The skip branch used to `eprintln!` and return `Ok(())`, which is a test that passes
/// because it declined to do anything — and cargo captures the message, so a reading of the
/// summary alone cannot tell a skip from a pass. The count is now reported as an atom and
/// asserted at the end of the run by `every_walk_that_skipped_said_so`, so a run that skipped
/// anything is *red*, and the reason is printed on stdout where it survives capture.
static SKIPPED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn announce_skip(reason: &str) {
    SKIPPED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    println!("WALK SKIPPED: {reason}");
}

/// The suite's own honesty gate.
///
/// A file of walks that skips everything and reports `ok` is the worst outcome a test suite
/// can produce, because it is indistinguishable from success in every artifact a human or a
/// CI job reads. This test fails if any walk in the file declined to run — which is the only
/// way the skip becomes visible in the same summary a pass would have appeared in.
#[test]
fn every_walk_that_skipped_said_so() {
    let skipped = SKIPPED.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        skipped, 0,
        "{skipped} walk(s) in this file were SKIPPED, not passed. Their output above says why \
         (a database that cannot be reached, or one that cannot be created). A skipped walk \
         reports `ok`, which is how a suite that measured nothing becomes a green line."
    );
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
    seed::ensure(db.pool())
        .await
        .expect("the IAM seed must run");
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
            theme: None,
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
    let (page, _) = omnion_content::pages::create_page(
        db.pool(),
        omnion_content::NewPage {
            site_id,
            slug: slug.to_owned(),
            page_type: Some("page".to_owned()),
            title: "Themed page".to_owned(),
            body: Some("Hello from the theme suite.".to_owned()),
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
            match Harness::fresh().await {
                Some(harness) => {
                    let state = harness.state.clone();
                    let db = harness.db.clone();
                    let outcome = $body(state, db).await;
                    harness.dispose().await;
                    outcome
                }
                // The skip is counted in `announce_skip`, and
                // `every_walk_that_skipped_said_so` turns a non-zero count into a red run.
                None => {
                    announce_skip("no database, this walk did not run");
                    Ok(())
                }
            }
        }
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
        let (user_id, email) = create_account(&db, organization_id).await;
        // Granted HERE rather than assumed. This walk used to take no grant at all and pass
        // only because `seed::ensure` binds the Owner role to the EARLIEST user in the
        // database — which, on a shared test database, is whichever walk inserted first. On a
        // database of its own this walk was refused `permission_denied` with `"considered": 0`,
        // which is the guard saying it found no binding to count. A test that depends on
        // another test's ordering is a test whose result belongs to the schedule.
        grant(&db, organization_id, user_id, &READER_PERMISSIONS, "Theme Reader").await;
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
        grant(&db, organization_id, user_id, &["themes.activate"], "Theme Owner").await;
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
        grant(&db, organization_id, user_id, &["themes.activate"], "Theme Owner").await;
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
        grant(&db, organization_id, user_id, &["themes.activate"], "Theme Owner").await;
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
///
/// **What this walk asserts changed, and why the old version could not be kept.** It used to
/// assert that a re-activation leaves `rollbackTarget` ABSENT, which was true while the FIRST
/// activation of a site's life recorded no displaced theme at all (a site with no `site_themes`
/// row has no previous key, so there was nothing to roll back to). That null was the defect:
/// the site was rendering `minimal`, the operator's only ever switch was `minimal → corporate`,
/// and the button was hidden while a perfectly reversible change sat behind it. `activate` now
/// displaces the site's RESOLVED theme, so the target exists and *should* be offered.
///
/// The trap the walk was written for is still real, so it is asserted in the form that
/// survives: a naive `on conflict do update set previous_theme_key = $active` would move the
/// target onto the theme already in use, and a rollback would then "restore" the current
/// theme. Here the target must stay `minimal` — a DIFFERENT key — after two re-activations.
#[tokio::test]
async fn re_activating_the_active_theme_keeps_the_rollback_target() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "gallery-noop").await;
        mirror_bundled(&db, &["minimal", "corporate"]).await;
        let (user_id, email) = create_account(&db, organization_id).await;
        grant(&db, organization_id, user_id, &["themes.activate"], "Theme Owner").await;
        let auth = login(&state, &db, &email).await;

        // The state is cloned INSIDE the closure, not outside it: the `async move` block below
        // takes ownership, so a closure that borrowed it would be `FnOnce` and the second
        // `activate("corporate")` in this walk would not compile. The repetition is the point
        // of the walk, so the closure has to be callable twice.
        let activate = |key: &'static str| {
            let auth = Auth {
                token: auth.token.clone(),
                session_id: auth.session_id.clone(),
            };
            let state = state.clone();
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
        assert_eq!(
            first.body["previousThemeKey"], json!("minimal"),
            "the FIRST activation of a site's life displaces the theme the site was already \
             rendering, which is the row that used to be absent"
        );
        assert_eq!(
            first.body["gallery"]["rollbackTarget"], json!("minimal"),
            "so the operator's only ever switch is reversible from the moment they make it"
        );

        // The same key, twice more.
        for _ in 0..2 {
            let again = activate("corporate").await;
            assert_eq!(again.status, StatusCode::OK, "{}", again.body);
            assert_eq!(again.body["restored"], json!(false));
            assert_eq!(
                again.body["previousThemeKey"], Value::Null,
                "a repeated activation of the key already in use displaces nothing"
            );
            assert_eq!(
                again.body["gallery"]["rollbackTarget"], json!("minimal"),
                "and the target from the EARLIER switch must survive it: a rollback that \
                 restored the theme already in use would report work it did not do"
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
        grant(&db, organization_id, user_id, &["themes.activate"], "Theme Owner").await;
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
        grant(&db, organization_id, user_id, &READER_PERMISSIONS, "Theme Reader").await;
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
        grant(&db, organization_id, user_id, &READER_PERMISSIONS, "Theme Reader").await;
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
            // The gallery nests the theme under `theme`: a row is
            // `{ theme: ThemeCard, isActive: bool }`, not a bare theme. Reading `entry["key"]`
            // asks a field that is not there, so `keys` was EMPTY and the assertion below
            // compared an empty list against `minimal` — a test that could only ever fail, on a
            // gallery that was correct all along.
            .filter_map(|entry| entry["theme"]["key"].as_str())
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
        grant(&db, organization_id, user_id, &READER_PERMISSIONS, "Theme Reader").await;
        let auth = login(&state, &db, &email).await;

        // `minimal` is mirrored HERE rather than being assumed present. This walk asserted the
        // gallery contains the bundled default while only ever mirroring two OTHER keys, so
        // it passed only against a database another walk had already populated — which is
        // exactly what a shared test database lets happen, and exactly what the throwaway
        // database this suite now uses makes impossible.
        mirror_bundled(&db, &["minimal", "corporate", "agency"]).await;
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
            // The gallery nests the theme under `theme`: a row is
            // `{ theme: ThemeCard, isActive: bool }`, not a bare theme. Reading `entry["key"]`
            // asks a field that is not there, so `keys` was EMPTY and the assertion below
            // compared an empty list against `minimal` — a test that could only ever fail, on a
            // gallery that was correct all along.
            .filter_map(|entry| entry["theme"]["key"].as_str())
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


// ---------------------------------------------------------------------------------------------
// Rollback and the settings revision (REQ-062, acceptance 5)
// ---------------------------------------------------------------------------------------------

/// Save a settings revision and publish it, returning the tokens the site is live with.
async fn publish_settings_revision(db: &Db, site_id: Uuid, accent: &str) -> i32 {
    let view = omnion_content::theme_settings::settings_view(
        db.pool(),
        site_id,
        omnion_content::themes::DEFAULT_THEME_KEY,
        json!({ "accent": { "light": "#123456" } }),
    )
    .await
    .expect("the settings view must read");
    let input = omnion_content::theme_settings::SettingsInput {
        theme_key: "minimal".to_owned(),
        tokens: json!({ "accent": { "light": accent } }),
        typography: json!({}),
        layout: json!({}),
        branding: json!({}),
        header_footer: json!({}),
        default_mode: "light".to_owned(),
    };
    let _ = view;
    let draft = omnion_content::theme_settings::save_draft(db.pool(), site_id, &input, None)
        .await
        .expect("the draft must save");
    let change = omnion_content::theme_settings::publish(db.pool(), site_id, None)
        .await
        .expect("the draft must publish");
    change
        .revision_no()
        .unwrap_or_else(|| panic!("a publish must name the revision it made live, not {draft:?}"))
}

/// The tokens the site is live with, read from the published pointer rather than from a draft.
async fn live_tokens(db: &Db, site_id: Uuid) -> Option<serde_json::Value> {
    omnion_content::theme_settings::published_tokens(db.pool(), site_id)
        .await
        .expect("the published pointer must read")
}

/// The criterion, in the form that can catch the defect it was written for: "Restore previous
/// brings back the previous theme **and its published settings revision**, confirmed by
/// comparing the rendered page."
///
/// The comparison is against [`live_tokens`] — the published pointer the renderer reads — and
/// not against a draft. A rollback that republished into the draft would pass a test reading
/// the draft and leave the public site on the wrong colours, which is the whole failure.
#[tokio::test]
async fn a_rollback_brings_back_the_settings_the_displaced_theme_was_published_with() -> TestResult
{
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "rollback-settings").await;
        mirror_bundled(&db, &["minimal", "corporate"]).await;
        publish_page(&db, site.id, "themed").await;
        let (user_id, email) = create_account(&db, organization_id).await;
        grant(
            &db,
            organization_id,
            user_id,
            &["themes.activate", "themes.customize"],
            "Theme Owner",
        )
        .await;
        let auth = login(&state, &db, &email).await;

        // 1. The site publishes settings of its own and activates `corporate`.
        let minimal_revision = publish_settings_revision(&db, site.id, "#101010").await;
        let activate = call(
            &state,
            request(
                Method::POST,
                &format!("/api/v1/sites/{}/theme", site.id),
                Some(&auth),
                Some(json!({ "theme_key": "corporate" })),
            ),
        )
        .await;
        assert_eq!(activate.status, StatusCode::OK, "{}", activate.body);

        // 2. The site switches look entirely, and publishes DIFFERENT settings.
        let corporate_revision = publish_settings_revision(&db, site.id, "#f0f0f0").await;
        assert_ne!(
            minimal_revision, corporate_revision,
            "the fixture must actually move between two live revisions"
        );
        assert_eq!(
            live_tokens(&db, site.id).await,
            Some(json!({ "accent": { "light": "#f0f0f0" } })),
            "the site is live with the corporate revision's colours"
        );

        // 3. Activate `minimal` again — this is the write that must CAPTURE the settings
        //    being displaced. A rollback can only bring back what an activation recorded.
        let back_to_minimal = call(
            &state,
            request(
                Method::POST,
                &format!("/api/v1/sites/{}/theme", site.id),
                Some(&auth),
                Some(json!({ "theme_key": "minimal" })),
            ),
        )
        .await;
        assert_eq!(back_to_minimal.status, StatusCode::OK, "{}", back_to_minimal.body);
        assert_eq!(back_to_minimal.body["restoredSettingsRevisionNo"], json!(null));

        // 4. Now change the look again while `minimal` is active, so a rollback has to
        //    distinguish "the theme that comes back" from "the colours that come with it".
        publish_settings_revision(&db, site.id, "#00ff00").await;

        let rollback = call(
            &state,
            request(
                Method::POST,
                &format!("/api/v1/sites/{}/theme/rollback", site.id),
                Some(&auth),
                None,
            ),
        )
        .await;
        assert_eq!(rollback.status, StatusCode::OK, "{}", rollback.body);
        assert_eq!(rollback.body["themeKey"], json!("corporate"));

        // The theme came back on its own before this tick, so asserting only the key would
        // have passed against a rollback that changed nothing a visitor can see.
        let restored_no = rollback.body["restoredSettingsRevisionNo"]
            .as_i64()
            .and_then(|number| i32::try_from(number).ok())
            .unwrap_or_else(|| {
                panic!(
                    "the rollback must report the settings revision it republished, got {}",
                    rollback.body
                )
            });
        assert_ne!(
            restored_no, corporate_revision,
            "the revision it restores is a NEW one: a restore is recorded, not pointed at"
        );

        // 5. THE ASSERTION: the public site's colours are the ones the RESTORED THEME was
        //    published with — `#f0f0f0`, which is what `corporate` was live with — and not
        //    the `#00ff00` that was live a moment ago under `minimal`.
        //
        //    The revision restored is `#f0f0f0`'s and not the earlier `#101010`, because the
        //    activation in step 3 recorded the settings live at the moment IT displaced
        //    something — which is what "the previous theme AND its published settings
        //    revision" means. Restoring the oldest revision would restore a state the site
        //    had already left. Step 4 is what gives this assertion teeth: `#00ff00` is live
        //    right up to the rollback, so a rollback that republishes nothing is visibly
        //    different from one that republishes the right thing.
        assert_eq!(
            live_tokens(&db, site.id).await,
            Some(json!({ "accent": { "light": "#f0f0f0" } })),
            "a rollback that restores the KEY but not the SETTINGS leaves the restored theme \\
             rendering under another theme's colours — a complete, valid, wrong page"
        );

        // 6. And the history recorded it: the newest revision is the restore, and it points
        //    at the revision it came from rather than at the one it displaced.
        let revisions = omnion_content::theme_settings::list_revisions(db.pool(), site.id)
            .await
            .expect("the history must read");
        let newest = revisions
            .first()
            .expect("the restore wrote a revision, so the history has one");
        assert_eq!(
            newest.revision_no, restored_no,
            "the number the response reported is the number the history holds"
        );
        assert!(
            newest.restored_from_no.is_some(),
            "a restore is recorded as coming FROM something, or the history cannot tell a \\
             restore from an ordinary save"
        );
        Ok(())
    })
    .await
}

/// A site that published nothing has nothing to bring back, and the rollback must say so
/// rather than invent a revision or fail the switch it was asked for.
#[tokio::test]
async fn a_rollback_of_a_site_that_published_no_settings_still_restores_the_theme() -> TestResult
{
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "rollback-nothing").await;
        mirror_bundled(&db, &["minimal", "corporate"]).await;
        let (user_id, email) = create_account(&db, organization_id).await;
        grant(&db, organization_id, user_id, &["themes.activate"], "Theme Owner").await;
        let auth = login(&state, &db, &email).await;

        let activate = call(
            &state,
            request(
                Method::POST,
                &format!("/api/v1/sites/{}/theme", site.id),
                Some(&auth),
                Some(json!({ "theme_key": "corporate" })),
            ),
        )
        .await;
        assert_eq!(activate.status, StatusCode::OK, "{}", activate.body);

        let rollback = call(
            &state,
            request(
                Method::POST,
                &format!("/api/v1/sites/{}/theme/rollback", site.id),
                Some(&auth),
                None,
            ),
        )
        .await;
        assert_eq!(
            rollback.status,
            StatusCode::OK,
            "nothing to republish is not a refusal — the theme still comes back: {}",
            rollback.body
        );
        assert_eq!(rollback.body["themeKey"], json!("minimal"));
        assert_eq!(
            rollback.body["restoredSettingsRevisionNo"],
            json!(null),
            "the honest answer is null; a revision invented here would put a row in a history \\
             the site never had"
        );
        assert!(
            omnion_content::theme_settings::list_revisions(db.pool(), site.id)
                .await
                .expect("the history must read")
                .is_empty(),
            "and no revision row was written for a rollback that republished nothing"
        );
        Ok(())
    })
    .await
}

/// A re-activation must not spend the rollback target's settings pointer.
///
/// The naive "read the published revision, write it on every activation" version of this
/// feature passes both walks above and still breaks here: re-activating the theme a site is
/// already on writes no activation row, so a version that captured the pointer on the
/// *forward* path instead of inside the shared writer would overwrite the rollback target's
/// settings with the CURRENT ones — and the next rollback would restore a revision that was
/// never the one being displaced.
#[tokio::test]
async fn re_activating_the_active_theme_leaves_the_rollback_settings_target_alone() -> TestResult {
    walk!(state, |state: AppState, db: Db| async move {
        let organization_id = create_organization(&db).await;
        let site = create_site(&db, organization_id, "reactivate").await;
        mirror_bundled(&db, &["minimal", "corporate"]).await;
        let (user_id, email) = create_account(&db, organization_id).await;
        grant(&db, organization_id, user_id, &["themes.activate"], "Theme Owner").await;
        let auth = login(&state, &db, &email).await;

        publish_settings_revision(&db, site.id, "#101010").await;
        let activate = call(
            &state,
            request(
                Method::POST,
                &format!("/api/v1/sites/{}/theme", site.id),
                Some(&auth),
                Some(json!({ "theme_key": "corporate" })),
            ),
        )
        .await;
        assert_eq!(activate.status, StatusCode::OK, "{}", activate.body);

        let captured = omnion_content::themes::pending_rollback_settings_revision(db.pool(), site.id)
            .await
            .expect("the pending pointer must read");
        assert!(
            captured.is_some(),
            "the activation that displaced a published state recorded its revision"
        );

        // Re-activate the theme already in use. This is a no-op by design, so it must write
        // nothing at all — including the pointer.
        let again = call(
            &state,
            request(
                Method::POST,
                &format!("/api/v1/sites/{}/theme", site.id),
                Some(&auth),
                Some(json!({ "theme_key": "corporate" })),
            ),
        )
        .await;
        assert_eq!(again.status, StatusCode::OK, "{}", again.body);
        assert_eq!(again.body["restored"], json!(false));

        assert_eq!(
            omnion_content::themes::pending_rollback_settings_revision(db.pool(), site.id)
                .await
                .expect("the pending pointer must read"),
            captured,
            "a no-op re-activation must not repoint the rollback target at the CURRENT settings"
        );
        Ok(())
    })
    .await
}
