//! Integration tests for the analytics surface (docs/requests/REQ-007, slice 1).
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) — locally and in CI. When
//! PostgreSQL is not reachable the suite skips itself with a printed reason.
//!
//! What the walks prove, in the words of the acceptance criteria: a beacon lands in the raw
//! rows (visit, pageview, event) and rolls up into the daily and hourly buckets; running a
//! bucket twice leaves the table byte-for-byte identical; `Do Not Track`, `Global Privacy
//! Control` and crawler traffic are dropped *before* anything is written and are counted in the
//! day's `filtered` bucket; no address is ever stored, and with anonymization switched off the
//! row carries the documented `/24` or `/48` prefix; the settings screen's validation is the
//! API's validation, and it is scoped to the caller's own organization; and the public
//! collection endpoint answers `429` above its per-site budget instead of degrading.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::routing::post as route_post;
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_events::{NewEvent, bus, engine, sender, signature};
use omnion_identity::users::{self, NewUser};
use omnion_module_analytics::rollup;
use omnion_module_analytics::settings as analytics_store;
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use serde_json::{Value, json};
use time::{Date, OffsetDateTime};
use tokio::net::TcpListener;
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// Serialises this suite: the rollup tables and the day's salt are shared state.
static ANALYTICS_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What the reader of this suite may do: see the numbers, not change what is collected.
const READER_PERMISSIONS: [&str; 2] = ["analytics.read", "sites.read"];

/// What the manager adds: the settings, the exports and the goals.
const MANAGER_PERMISSIONS: [&str; 5] = [
    "analytics.read",
    "analytics.export",
    "analytics.goals.manage",
    "analytics.settings.manage",
    "sites.read",
];

/// A person's browser, as the bot filter must read it.
const CHROME: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                      (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// A phone, as the device reader must read it.
const IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) \
                      AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 \
                      Safari/604.1";

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

/// Build a JSON request; `token` becomes the session cookie and `body` the payload.
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

/// Build a beacon exactly as the tracking script sends it: JSON body, a user agent and the
/// address the caller claims (the platform's own edge sets that header).
fn beacon(
    uri: &str,
    body: Value,
    user_agent: &str,
    forwarded_for: Option<&str>,
    extra: &[(&str, &str)],
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::USER_AGENT, user_agent);
    if let Some(address) = forwarded_for {
        builder = builder.header("x-forwarded-for", address);
    }
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }

    builder
        .body(Body::from(body.to_string()))
        .expect("beacon must build")
}

/// A pageview beacon with the shape the script produces.
fn pageview_beacon(path: &str, events: Value) -> Value {
    json!({
        "pageview": {
            "path": path,
            "title": "QA page",
            "referrer": "https://www.google.com/search?q=omnion",
            "duration_ms": 1800,
            "scroll_depth": 55,
            "screen": { "width": 1440, "height": 900 },
            "language": "en-GB"
        },
        "events": events,
        "utm": { "source": "newsletter", "medium": "email", "campaign": "launch" }
    })
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

/// Object store of the test state; analytics never touches it.
fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

/// Two organizations with one site each, a platform Owner, a reader and a manager of the first
/// organization, a member with nothing and a reader of the second organization.
///
/// Every row carries a random address, and cleanup removes exactly the rows this fixture
/// created — by id, never by pattern, so parallel suites cannot collide.
struct Fixture {
    /// Held for the whole walk; see [`ANALYTICS_WALK`].
    _walk: tokio::sync::MutexGuard<'static, ()>,
    state: AppState,
    db: Db,
    owner_email: String,
    reader_email: String,
    manager_email: String,
    member_email: String,
    other_reader_email: String,
    /// Site key of the first organization.
    site_a: Uuid,
    /// Site key of the second organization.
    site_b: Uuid,
    /// Public key of the first site (unique across the installation, so `?site=` resolves it).
    key_a: String,
    /// Public key of the second site.
    key_b: String,
    accounts: Vec<Uuid>,
    organizations: Vec<Uuid>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = ANALYTICS_WALK.lock().await;
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let org_a = create_organization_row(&db, "a", "Analytics Test A").await;
        let org_b = create_organization_row(&db, "b", "Analytics Test B").await;
        // Keys must be unique across the installation, so they carry this fixture's marker: a
        // site key is how a public beacon addresses its site.
        let marker = &Uuid::new_v4().simple().to_string()[..8];
        let key_a = format!("an-{marker}");
        let key_b = format!("an-b{marker}");
        let site_a = create_site_row(&db, org_a, &key_a, "Analytics Site A").await;
        let site_b = create_site_row(&db, org_b, &key_b, "Analytics Site B").await;

        let (owner_id, owner_email) = create_account(&db, None, "Analytics Owner").await;
        seed::bind_owner(db.pool(), owner_id)
            .await
            .expect("the owner binding must be created");

        let (reader_id, reader_email) = create_account(&db, Some(org_a), "Analytics Reader").await;
        grant(&db, org_a, reader_id, owner_id, &READER_PERMISSIONS).await;

        let (manager_id, manager_email) =
            create_account(&db, Some(org_a), "Analytics Manager").await;
        grant(&db, org_a, manager_id, owner_id, &MANAGER_PERMISSIONS).await;

        let (member_id, member_email) = create_account(&db, Some(org_a), "Analytics Member").await;

        let (other_id, other_reader_email) =
            create_account(&db, Some(org_b), "Analytics Other Reader").await;
        grant(&db, org_b, other_id, owner_id, &READER_PERMISSIONS).await;

        Some(Self {
            _walk: walk,
            state,
            db,
            owner_email,
            reader_email,
            manager_email,
            member_email,
            other_reader_email,
            site_a,
            site_b,
            key_a,
            key_b,
            accounts: vec![owner_id, reader_id, manager_id, member_id, other_id],
            organizations: vec![org_a, org_b],
        })
    }

    /// The platform Owner, signed in.
    async fn owner_token(&self) -> String {
        login(&self.state, &self.owner_email).await
    }

    /// The reader of the first organization, signed in.
    async fn reader_token(&self) -> String {
        login(&self.state, &self.reader_email).await
    }

    /// The manager of the first organization, signed in.
    async fn manager_token(&self) -> String {
        login(&self.state, &self.manager_email).await
    }

    /// The plain member of the first organization, signed in.
    async fn member_token(&self) -> String {
        login(&self.state, &self.member_email).await
    }

    /// The reader of the second organization, signed in.
    async fn other_reader_token(&self) -> String {
        login(&self.state, &self.other_reader_email).await
    }

    /// The settings row of a site, as the collector would read it.
    async fn settings(&self, site: Uuid) -> omnion_module_analytics::Settings {
        analytics_store::ensure(self.db.pool(), site)
            .await
            .expect("the settings row must exist")
    }

    /// Write a beacon and expect the collector to accept it.
    async fn collect(&self, site_key: &str, request: Request<Body>) -> TestResponse {
        let uri = format!("/api/v1/public/analytics/collect?site={site_key}");
        let request = retarget(request, &uri);
        let response = call(&self.state, request).await;
        assert_eq!(
            response.status,
            StatusCode::ACCEPTED,
            "body: {}",
            response.body
        );

        response
    }

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

/// Rebuild a request against a different URI (the beacon helper builds the raw path).
fn retarget(request: Request<Body>, uri: &str) -> Request<Body> {
    let (parts, body) = request.into_parts();
    let mut rebuilt = Request::builder().method(parts.method).uri(uri);
    for (name, value) in parts.headers.iter() {
        rebuilt = rebuilt.header(name, value.clone());
    }

    rebuilt.body(body).expect("request must rebuild")
}

// ---------------------------------------------------------------------------------------------
// Row helpers
// ---------------------------------------------------------------------------------------------

/// Number of rows of one analytics table for one site.
async fn rows(db: &Db, table: &str, site: Uuid) -> i64 {
    let sql = format!("select count(*) from {table} where site_id = $1");
    sqlx::query_scalar(&sql)
        .bind(site)
        .fetch_one(db.pool())
        .await
        .expect("count must run")
}

/// The daily bucket of one metric and dimension.
async fn daily(db: &Db, site: Uuid, day: Date, metric: &str, dimension: &str, value: &str) -> i64 {
    sqlx::query_scalar(
        "select coalesce(sum(count), 0)::bigint from analytics_daily \
         where site_id = $1 and day = $2 and metric = $3 and dimension_kind = $4 \
         and dimension_value = $5",
    )
    .bind(site)
    .bind(day)
    .bind(metric)
    .bind(dimension)
    .bind(value)
    .fetch_one(db.pool())
    .await
    .expect("the bucket must read")
}

/// The whole daily table of one site, as a comparable snapshot.
async fn daily_snapshot(db: &Db, site: Uuid) -> Vec<(String, String, String, String, i64)> {
    sqlx::query_as(
        "select day::text, metric, dimension_kind, dimension_value, count from analytics_daily \
         where site_id = $1 order by day, metric, dimension_kind, dimension_value",
    )
    .bind(site)
    .fetch_all(db.pool())
    .await
    .expect("the snapshot must read")
}

/// The whole hourly table of one site, as a comparable snapshot.
async fn hourly_snapshot(db: &Db, site: Uuid) -> Vec<(String, String, String, String, i64)> {
    sqlx::query_as(
        "select bucket::text, metric, dimension_kind, dimension_value, count from analytics_hourly \
         where site_id = $1 order by bucket, metric, dimension_kind, dimension_value",
    )
    .bind(site)
    .fetch_all(db.pool())
    .await
    .expect("the snapshot must read")
}

// ---------------------------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------------------------

/// Create an organization row with a unique, suite-scoped slug.
async fn create_organization_row(db: &Db, label: &str, name: &str) -> Uuid {
    let slug = format!("analytics-fix-{label}-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(name)
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// Create a site row inside an organization.
async fn create_site_row(db: &Db, organization_id: Uuid, key: &str, name: &str) -> Uuid {
    sqlx::query_scalar(
        "insert into sites (organization_id, key, name) values ($1, $2, $3) returning id",
    )
    .bind(organization_id)
    .bind(key)
    .bind(name)
    .fetch_one(db.pool())
    .await
    .expect("the test site must be created")
}

/// Create an account with a unique address so parallel runs cannot collide.
async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    let email = format!("analytics-{}@omnion.test", Uuid::new_v4().simple());
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

/// Give an account a role with exactly these permissions.
async fn grant(
    db: &Db,
    organization_id: Uuid,
    user_id: Uuid,
    granted_by: Uuid,
    permissions: &[&str],
) {
    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id,
            key: format!("analytics-role-{}", Uuid::new_v4().simple()),
            name: "Analytics Test Role".to_owned(),
            description: "A role of the analytics suite".to_owned(),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the organization role must be created");

    let entries: Vec<RolePermissionInput> = permissions
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
        .expect("cookie has a name")
        .1
        .to_owned()
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_beacon_becomes_raw_rows_and_the_rollup_is_idempotent() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let today = OffsetDateTime::now_utc().date();

    let first = fixture
        .collect(
            &fixture.key_a,
            beacon(
                "/api/v1/public/analytics/collect",
                pageview_beacon(
                    "/qa/landing",
                    json!([
                        { "name": "cta_click", "properties": { "slot": "hero" } },
                        { "name": "signup", "value": 9.5, "properties": { "plan": "pro" } }
                    ]),
                ),
                CHROME,
                Some("198.51.100.10"),
                &[],
            ),
        )
        .await;
    assert_eq!(first.body["stored"]["visits"], 1);
    assert_eq!(first.body["stored"]["pageviews"], 1);
    assert_eq!(first.body["stored"]["events"], 2);
    assert_eq!(first.body["dropped"]["bots"], 0);

    // A second visitor, another page, on a phone.
    fixture
        .collect(
            &fixture.key_a,
            beacon(
                "/api/v1/public/analytics/collect",
                pageview_beacon(
                    "/qa/pricing",
                    json!([
                        { "name": "download", "properties": { "file": "/qa/guide.pdf" } }
                    ]),
                ),
                IPHONE,
                Some("198.51.100.11"),
                &[],
            ),
        )
        .await;

    // The rows are there, one pageview each, three events in total.
    assert_eq!(rows(&fixture.db, "analytics_visits", site).await, 2);
    assert_eq!(rows(&fixture.db, "analytics_pageviews", site).await, 2);
    assert_eq!(rows(&fixture.db, "analytics_events", site).await, 3);

    // The visitor identifier is a hash and nothing else; the address was not stored.
    let (hash, prefix): (String, Option<String>) = sqlx::query_as(
        "select visitor_hash, ip_prefix::text from analytics_visits where site_id = $1 \
         order by started_at limit 1",
    )
    .bind(site)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the visit must read");
    assert_eq!(hash.len(), 64, "a visitor is a sha256, not an address");
    assert_eq!(prefix, None, "anonymization is on by default");

    // The devices were read from the agents.
    let devices: Vec<String> = sqlx::query_scalar(
        "select device_type from analytics_visits where site_id = $1 order by started_at",
    )
    .bind(site)
    .fetch_all(fixture.db.pool())
    .await
    .expect("devices must read");
    assert_eq!(devices, vec!["desktop", "mobile"]);

    // The rollup writes the buckets a dashboard reads...
    rollup::rollup_day(fixture.db.pool(), site, today)
        .await
        .expect("the day must roll up");
    assert_eq!(
        daily(&fixture.db, site, today, "pageviews", "total", "").await,
        2
    );
    assert_eq!(
        daily(&fixture.db, site, today, "visitors", "total", "").await,
        2
    );
    assert_eq!(
        daily(&fixture.db, site, today, "events", "name", "cta_click").await,
        1
    );
    assert_eq!(
        daily(
            &fixture.db,
            site,
            today,
            "downloads",
            "file",
            "/qa/guide.pdf"
        )
        .await,
        1
    );
    assert_eq!(
        daily(&fixture.db, site, today, "visitors", "device", "mobile").await,
        1
    );

    // ...and running the same bucket again changes nothing at all.
    let before = daily_snapshot(&fixture.db, site).await;
    rollup::rollup_day(fixture.db.pool(), site, today)
        .await
        .expect("the second run must succeed");
    assert_eq!(
        daily_snapshot(&fixture.db, site).await,
        before,
        "a bucket recomputed is a bucket unchanged"
    );

    let hour = OffsetDateTime::now_utc()
        .replace_minute(0)
        .and_then(|value| value.replace_second(0))
        .and_then(|value| value.replace_nanosecond(0))
        .expect("the hour must exist");
    rollup::rollup_hour(fixture.db.pool(), site, hour)
        .await
        .expect("the hour must roll up");
    let hourly = hourly_snapshot(&fixture.db, site).await;
    assert!(!hourly.is_empty(), "the hourly table answers");
    rollup::rollup_hour(fixture.db.pool(), site, hour)
        .await
        .expect("the second hourly run must succeed");
    assert_eq!(
        hourly_snapshot(&fixture.db, site).await,
        hourly,
        "the hourly bucket is idempotent too"
    );

    // A full worker tick is safe to run against the whole installation.
    rollup::tick(fixture.db.pool(), OffsetDateTime::now_utc())
        .await
        .expect("the tick must run");

    fixture.cleanup().await;
}

#[tokio::test]
async fn privacy_signals_and_crawlers_are_dropped_before_anything_is_written() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // A dedicated site keeps the assertions about "no rows" unambiguous.
    let site = fixture.site_b;
    let today = OffsetDateTime::now_utc().date();

    let do_not_track = fixture
        .collect(
            &fixture.key_b,
            beacon(
                "/api/v1/public/analytics/collect",
                pageview_beacon("/qa/private", json!([])),
                CHROME,
                Some("198.51.100.20"),
                &[("dnt", "1")],
            ),
        )
        .await;
    assert_eq!(do_not_track.body["dropped"]["policy"], 1);
    assert_eq!(do_not_track.body["stored"]["visits"], 0);

    let global_privacy = fixture
        .collect(
            &fixture.key_b,
            beacon(
                "/api/v1/public/analytics/collect",
                pageview_beacon("/qa/private", json!([])),
                CHROME,
                Some("198.51.100.21"),
                &[("sec-gpc", "1")],
            ),
        )
        .await;
    assert_eq!(global_privacy.body["dropped"]["policy"], 1);

    let crawler = fixture
        .collect(
            &fixture.key_b,
            beacon(
                "/api/v1/public/analytics/collect",
                pageview_beacon("/qa/private", json!([])),
                "Googlebot/2.1 (+http://www.google.com/bot.html)",
                Some("66.249.66.1"),
                &[],
            ),
        )
        .await;
    assert_eq!(crawler.body["dropped"]["bots"], 1);

    // Nothing was written: no visit, no pageview, no event — the drops were counted instead.
    assert_eq!(rows(&fixture.db, "analytics_visits", site).await, 0);
    assert_eq!(rows(&fixture.db, "analytics_pageviews", site).await, 0);
    assert_eq!(rows(&fixture.db, "analytics_events", site).await, 0);
    assert_eq!(
        daily(&fixture.db, site, today, "filtered", "total", "").await,
        3
    );

    // A beacon the collector cannot use is refused with its own code, and writes nothing.
    let broken = call(
        &fixture.state,
        beacon(
            &format!("/api/v1/public/analytics/collect?site={}", fixture.key_b),
            json!({ "pageview": { "path": "no-leading-slash" } }),
            CHROME,
            Some("198.51.100.22"),
            &[],
        ),
    )
    .await;
    assert_eq!(broken.status, StatusCode::BAD_REQUEST);
    assert_eq!(broken.body["error"]["code"], "invalid_beacon");

    let empty = call(
        &fixture.state,
        beacon(
            &format!("/api/v1/public/analytics/collect?site={}", fixture.key_b),
            json!({ "events": [] }),
            CHROME,
            Some("198.51.100.22"),
            &[],
        ),
    )
    .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);
    assert_eq!(empty.body["error"]["code"], "empty_beacon");

    // An event-only beacon is a beacon: it opens a visit and writes the event.
    let events_only = fixture
        .collect(
            &fixture.key_b,
            beacon(
                "/api/v1/public/analytics/collect",
                json!({ "events": [{ "name": "newsletter_open", "properties": { "issue": 12 } }] }),
                CHROME,
                Some("198.51.100.23"),
                &[],
            ),
        )
        .await;
    assert_eq!(events_only.body["stored"]["events"], 1);
    assert_eq!(rows(&fixture.db, "analytics_events", site).await, 1);

    // A site whose tracking is switched off records nothing and says so in its counter. The
    // platform Owner may change any organization's settings; the reader cannot.
    let owner = fixture.owner_token().await;
    let mut changes = serde_json::to_value(
        analytics_store::find(fixture.db.pool(), site)
            .await
            .expect("the settings must read")
            .expect("the row must exist"),
    )
    .expect("the row must serialise");
    changes["tracking_enabled"] = json!(false);
    let disabled = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/analytics/settings?site_id={site}"),
            Some(&owner),
            Some(changes),
        ),
    )
    .await;
    assert_eq!(disabled.status, StatusCode::OK, "body: {}", disabled.body);

    fixture
        .collect(
            &fixture.key_b,
            beacon(
                "/api/v1/public/analytics/collect",
                pageview_beacon("/qa/tracked-off", json!([])),
                CHROME,
                Some("198.51.100.24"),
                &[],
            ),
        )
        .await;
    assert_eq!(rows(&fixture.db, "analytics_pageviews", site).await, 0);

    fixture.cleanup().await;
}

#[tokio::test]
async fn addresses_are_never_stored_and_truncation_matches_its_widths() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;

    // Anonymous by default: the visit row holds no address at all.
    fixture
        .collect(
            &fixture.key_a,
            beacon(
                "/api/v1/public/analytics/collect",
                pageview_beacon("/qa/anon", json!([])),
                CHROME,
                Some("203.0.113.55"),
                &[],
            ),
        )
        .await;
    let stored: Option<String> =
        sqlx::query_scalar("select ip_prefix::text from analytics_visits where site_id = $1")
            .bind(site)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the visit must read");
    assert_eq!(stored, None);

    // With anonymization switched off the row keeps the network prefix, never the address.
    let manager = fixture.manager_token().await;
    let mut changes = serde_json::to_value(fixture.settings(site).await).expect("settings");
    changes["anonymize_ip"] = json!(false);
    let updated = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/analytics/settings?site_id={site}"),
            Some(&manager),
            Some(changes),
        ),
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK, "body: {}", updated.body);

    fixture
        .collect(
            &fixture.key_a,
            beacon(
                "/api/v1/public/analytics/collect",
                pageview_beacon("/qa/ipv4", json!([])),
                CHROME,
                Some("203.0.113.77"),
                &[],
            ),
        )
        .await;
    let v4: Option<String> = sqlx::query_scalar(
        "select ip_prefix::text from analytics_visits where site_id = $1 and entry_path = '/qa/ipv4'",
    )
    .bind(site)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the visit must read");
    assert_eq!(v4.as_deref(), Some("203.0.113.0/24"));

    fixture
        .collect(
            &fixture.key_a,
            beacon(
                "/api/v1/public/analytics/collect",
                pageview_beacon("/qa/ipv6", json!([])),
                IPHONE,
                Some("2001:db8:1:2:3:4:5:6"),
                &[],
            ),
        )
        .await;
    let v6: Option<String> = sqlx::query_scalar(
        "select ip_prefix::text from analytics_visits where site_id = $1 and entry_path = '/qa/ipv6'",
    )
    .bind(site)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the visit must read");
    assert_eq!(v6.as_deref(), Some("2001:db8:1::/48"));

    fixture.cleanup().await;
}

#[tokio::test]
async fn settings_are_scoped_validated_and_readable() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let reader = fixture.reader_token().await;
    let manager = fixture.manager_token().await;
    let member = fixture.member_token().await;
    let other = fixture.other_reader_token().await;

    // Reading is `analytics.read`; an account without it is refused, not silently served.
    let denied = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/analytics/settings?site_id={site}"),
            Some(&member),
            None,
        ),
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);

    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/analytics/settings?site_id={site}"),
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "body: {}", read.body);
    assert_eq!(read.body["settings"]["site_id"], json!(site));
    assert_eq!(read.body["settings"]["mode"], "cookieless");
    assert_eq!(read.body["settings"]["anonymize_ip"], true);
    assert_eq!(
        read.body["defaults"]["retention_days"], 180,
        "the server owns the defaults the screen restores"
    );

    // Another organization's site is not this reader's to look at.
    let crossed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/analytics/settings?site_id={site}"),
            Some(&other),
            None,
        ),
    )
    .await;
    assert_eq!(crossed.status, StatusCode::FORBIDDEN);
    assert_eq!(crossed.body["error"]["code"], "cross_organization");

    // Changing the promises needs the settings key, which the reader does not hold.
    let refused = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/analytics/settings?site_id={site}"),
            Some(&reader),
            Some(json!({
                "tracking_enabled": true,
                "mode": "cookieless",
                "anonymize_ip": true,
                "respect_dnt": true,
                "bot_filter": true,
                "sample_rate": 100,
                "retention_days": 30,
                "excluded_paths": [],
                "excluded_ips": []
            })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    // Validation answers the field that failed; the body is a full replacement.
    let invalid = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/analytics/settings?site_id={site}"),
            Some(&manager),
            Some(json!({
                "tracking_enabled": true,
                "mode": "cookieless",
                "anonymize_ip": true,
                "respect_dnt": true,
                "bot_filter": true,
                "sample_rate": 100,
                "retention_days": 3,
                "excluded_paths": [],
                "excluded_ips": []
            })),
        ),
    )
    .await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        invalid.body["error"]["code"], "invalid_analytics_settings",
        "body: {}",
        invalid.body
    );
    assert!(
        invalid.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("retention_days"),
        "the message names the field: {}",
        invalid.body
    );

    // A faithful update is stored and read back.
    let accepted = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/analytics/settings?site_id={site}"),
            Some(&manager),
            Some(json!({
                "tracking_enabled": true,
                "mode": "cookie",
                "anonymize_ip": true,
                "respect_dnt": false,
                "bot_filter": true,
                "sample_rate": 50,
                "retention_days": 30,
                "excluded_paths": ["/admin/*"],
                "excluded_ips": ["203.0.113.0/24"]
            })),
        ),
    )
    .await;
    assert_eq!(accepted.status, StatusCode::OK, "body: {}", accepted.body);
    assert_eq!(accepted.body["settings"]["mode"], "cookie");
    assert_eq!(accepted.body["settings"]["sample_rate"], 50);
    assert_eq!(accepted.body["settings"]["excluded_paths"][0], "/admin/*");
    assert_eq!(
        accepted.body["settings"]["excluded_ips"][0],
        "203.0.113.0/24"
    );

    let stored = fixture.settings(site).await;
    assert_eq!(stored.retention_days, 30);
    assert_eq!(stored.excluded_ips, vec!["203.0.113.0/24".to_owned()]);

    // A path on the exclusion list writes no rows and is counted as a policy drop.
    let today = OffsetDateTime::now_utc().date();
    let before = daily(&fixture.db, site, today, "filtered", "total", "").await;
    fixture
        .collect(
            &fixture.key_a,
            beacon(
                "/api/v1/public/analytics/collect",
                pageview_beacon("/admin/users", json!([])),
                CHROME,
                Some("198.51.100.30"),
                &[],
            ),
        )
        .await;
    assert_eq!(
        daily(&fixture.db, site, today, "filtered", "total", "").await,
        before + 1
    );
    let excluded_rows: i64 = sqlx::query_scalar(
        "select count(*) from analytics_pageviews where site_id = $1 and path = '/admin/users'",
    )
    .bind(site)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must run");
    assert_eq!(excluded_rows, 0);

    // Back to an unsampled site, so the next check can only be about the address list.
    let unsampled = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/analytics/settings?site_id={site}"),
            Some(&manager),
            Some(json!({
                "tracking_enabled": true,
                "mode": "cookieless",
                "anonymize_ip": true,
                "respect_dnt": true,
                "bot_filter": true,
                "sample_rate": 100,
                "retention_days": 30,
                "excluded_paths": ["/admin/*"],
                "excluded_ips": ["203.0.113.0/24"]
            })),
        ),
    )
    .await;
    assert_eq!(unsampled.status, StatusCode::OK, "body: {}", unsampled.body);

    // An excluded address writes no rows and is counted like every other drop.
    let before = daily(&fixture.db, site, today, "filtered", "total", "").await;
    fixture
        .collect(
            &fixture.key_a,
            beacon(
                "/api/v1/public/analytics/collect",
                pageview_beacon("/qa/excluded-address", json!([])),
                CHROME,
                Some("203.0.113.9"),
                &[],
            ),
        )
        .await;
    assert_eq!(
        daily(&fixture.db, site, today, "filtered", "total", "").await,
        before + 1
    );
    let excluded_address_rows: i64 = sqlx::query_scalar(
        "select count(*) from analytics_pageviews where site_id = $1 \
         and path = '/qa/excluded-address'",
    )
    .bind(site)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must run");
    assert_eq!(excluded_address_rows, 0);

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_snippet_names_the_site_and_the_collect_endpoint() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let reader = fixture.reader_token().await;
    let owner = fixture.owner_token().await;

    let response = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/analytics/snippet?site_id={site}"),
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);
    assert_eq!(response.body["site"]["key"], json!(fixture.key_a));
    assert!(
        response.body["collect_url"]
            .as_str()
            .unwrap_or_default()
            .ends_with("/api/v1/public/analytics/collect")
    );
    let snippet = response.body["snippet"]
        .as_str()
        .expect("the snippet is a string");
    assert!(snippet.contains(&format!("data-site=\"{}\"", fixture.key_a)));
    assert!(snippet.contains("/analytics.js"));

    // A site with a domain addresses its own host; the snippet follows the site, not the panel.
    let domain = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sites/{site}/domains"),
            Some(&owner),
            Some(json!({ "host": "qa-analytics.example.org", "is_primary": true })),
        ),
    )
    .await;
    assert_eq!(domain.status, StatusCode::CREATED, "body: {}", domain.body);

    let with_domain = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/analytics/snippet?site_id={site}"),
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(
        with_domain.body["script_url"],
        "//qa-analytics.example.org/analytics.js"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_collect_endpoint_answers_429_above_its_budget() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let budget = fixture.state.config().analytics.collect_per_minute;
    let beacon_body = json!({ "events": [{ "name": "heartbeat", "properties": {} }] });

    for _ in 0..budget {
        fixture
            .collect(
                &fixture.key_b,
                beacon(
                    "/api/v1/public/analytics/collect",
                    beacon_body.clone(),
                    CHROME,
                    Some("192.0.2.9"),
                    &[],
                ),
            )
            .await;
    }

    let over = call(
        &fixture.state,
        beacon(
            &format!("/api/v1/public/analytics/collect?site={}", fixture.key_b),
            beacon_body,
            CHROME,
            Some("192.0.2.9"),
            &[],
        ),
    )
    .await;
    assert_eq!(
        over.status,
        StatusCode::TOO_MANY_REQUESTS,
        "body: {}",
        over.body
    );
    assert_eq!(over.body["error"]["code"], "rate_limited");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// The report walks (slice 2)
// ---------------------------------------------------------------------------------------------

/// One visit written straight into the raw tables.
///
/// The report suite seeds rows itself instead of posting beacons: a beacon always carries "now",
/// and the reports are about ranges — a fixture has to be able to say *when*.
struct RawVisit<'a> {
    /// The visitor hash (64 hex characters in the real thing; the tests use readable stand-ins
    /// with the same shape).
    hash: &'a str,
    /// When the visit happened.
    at: OffsetDateTime,
    /// The path its pageview was on.
    path: &'a str,
    /// The page title.
    title: &'a str,
    /// Device type.
    device: &'a str,
    /// Country code.
    country: &'a str,
    /// UTM source, when the visit carried one.
    source: Option<&'a str>,
    /// Referrer host, when the browser reported one.
    referrer_host: Option<&'a str>,
    /// How long the page was visible.
    duration_ms: Option<i32>,
}

/// A visitor hash of the documented shape (64 hex characters) from a short label.
fn hash(label: &str) -> String {
    let mut value = String::new();
    for byte in label.bytes() {
        value.push_str(&format!("{byte:02x}"));
    }
    while value.len() < 64 {
        value.push('0');
    }
    value.truncate(64);
    value
}

/// Write one visit with one pageview (entry and exit) and answer its id.
async fn seed_visit(db: &Db, site: Uuid, visit: &RawVisit<'_>) -> i64 {
    let id: i64 = sqlx::query_scalar(
        "insert into analytics_visits (site_id, visitor_hash, started_at, last_seen_at, \
         pageview_count, is_bounce, entry_path, exit_path, referrer_host, device_type, \
         country_code, source) \
         values ($1, $2, $3, $3, 1, false, $4, $4, $5, $6, $7, $8) returning id",
    )
    .bind(site)
    .bind(visit.hash)
    .bind(visit.at)
    .bind(visit.path)
    .bind(visit.referrer_host)
    .bind(visit.device)
    .bind(visit.country)
    .bind(visit.source)
    .fetch_one(db.pool())
    .await
    .expect("the visit must be written");

    sqlx::query(
        "insert into analytics_pageviews (site_id, visit_id, path, title, occurred_at, \
         duration_ms, scroll_depth, is_entry, is_exit) \
         values ($1, $2, $3, $4, $5, $6, $7, true, true)",
    )
    .bind(site)
    .bind(id)
    .bind(visit.path)
    .bind(visit.title)
    .bind(visit.at)
    .bind(visit.duration_ms)
    .bind(Some(55_i16))
    .execute(db.pool())
    .await
    .expect("the pageview must be written");

    id
}

/// Add one more pageview to a visit (the entry and exit flags move with it).
async fn seed_pageview(
    db: &Db,
    site: Uuid,
    visit_id: i64,
    path: &str,
    title: &str,
    at: OffsetDateTime,
) {
    sqlx::query("update analytics_pageviews set is_exit = false where visit_id = $1")
        .bind(visit_id)
        .execute(db.pool())
        .await
        .expect("the previous pageview must stop being the exit");
    sqlx::query(
        "insert into analytics_pageviews (site_id, visit_id, path, title, occurred_at, \
         is_entry, is_exit) values ($1, $2, $3, $4, $5, false, true)",
    )
    .bind(site)
    .bind(visit_id)
    .bind(path)
    .bind(title)
    .bind(at)
    .execute(db.pool())
    .await
    .expect("the pageview must be written");
}

/// Write one event of a visit.
///
/// Seeding a row takes every column it fills; bundling them into a struct would only move the
/// list one line up, so the test helper keeps its parameters.
#[allow(clippy::too_many_arguments)]
async fn seed_event(
    db: &Db,
    site: Uuid,
    visit_id: i64,
    name: &str,
    path: &str,
    value: Option<f64>,
    properties: serde_json::Value,
    at: OffsetDateTime,
) {
    sqlx::query(
        "insert into analytics_events (site_id, visit_id, name, path, value, properties, \
         occurred_at) values ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(site)
    .bind(visit_id)
    .bind(name)
    .bind(path)
    .bind(value)
    .bind(properties)
    .bind(at)
    .fetch_optional(db.pool())
    .await
    .expect("the event must be written");
}

/// A beacon-shaped request against a report endpoint.
fn report_request(uri: &str, token: &str) -> Request<Body> {
    request(Method::GET, uri, Some(token), None)
}

/// The midnight of `offset` days before today, plus a couple of hours.
fn at_day(offset: i64, hour: u8) -> OffsetDateTime {
    let day = OffsetDateTime::now_utc().date() - time::Duration::days(offset);
    day.with_hms(hour, 30, 0)
        .expect("a valid clock time")
        .assume_utc()
}

/// The axis label of a day, in the shape the module renders it (`Sep 26`).
fn month_day(day: Date) -> String {
    format!(
        "{} {}",
        omnion_module_analytics::reports::month_short(u8::from(day.month())),
        day.day()
    )
}

#[tokio::test]
async fn the_overview_matches_the_seeded_fixture_and_compares_with_the_period_before() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let reader = fixture.reader_token().await;
    let today = OffsetDateTime::now_utc().date();
    let from = today - time::Duration::days(6);

    // Four visitors over the last seven days: one comes back on a second day (which must still
    // count once), one downloads, one submits a form, one is the goal conversion.
    seed_visit(
        &fixture.db,
        site,
        &RawVisit {
            hash: &hash("h1"),
            at: at_day(5, 10),
            path: "/qa/landing",
            title: "QA landing",
            device: "desktop",
            country: "TR",
            source: Some("newsletter"),
            referrer_host: None,
            duration_ms: Some(1_200),
        },
    )
    .await;
    seed_visit(
        &fixture.db,
        site,
        &RawVisit {
            hash: &hash("h1"),
            at: at_day(2, 11),
            path: "/qa/pricing",
            title: "QA pricing",
            device: "desktop",
            country: "TR",
            source: Some("newsletter"),
            referrer_host: None,
            duration_ms: Some(2_400),
        },
    )
    .await;
    seed_visit(
        &fixture.db,
        site,
        &RawVisit {
            hash: &hash("h2"),
            at: at_day(1, 9),
            path: "/qa/pricing",
            title: "QA pricing",
            device: "mobile",
            country: "DE",
            source: None,
            referrer_host: Some("google.example"),
            duration_ms: None,
        },
    )
    .await;
    let landing = seed_visit(
        &fixture.db,
        site,
        &RawVisit {
            hash: &hash("h3"),
            at: at_day(0, 8),
            path: "/qa/landing",
            title: "QA landing",
            device: "desktop",
            country: "FR",
            source: None,
            referrer_host: Some("news.example"),
            duration_ms: Some(900),
        },
    )
    .await;
    seed_pageview(
        &fixture.db,
        site,
        landing,
        "/qa/docs",
        "QA docs",
        at_day(0, 8) + time::Duration::minutes(3),
    )
    .await;
    let form = seed_visit(
        &fixture.db,
        site,
        &RawVisit {
            hash: &hash("h4"),
            at: at_day(0, 12),
            path: "/qa/contact",
            title: "QA contact",
            device: "mobile",
            country: "TR",
            source: Some("newsletter"),
            referrer_host: None,
            duration_ms: Some(3_000),
        },
    )
    .await;
    seed_event(
        &fixture.db,
        site,
        form,
        "form_submit",
        "/qa/contact",
        Some(120.0),
        json!({ "form": "contact" }),
        at_day(0, 12) + time::Duration::minutes(1),
    )
    .await;
    seed_event(
        &fixture.db,
        site,
        landing,
        "download",
        "/qa/landing",
        None,
        json!({ "file": "/qa/files/guide.pdf" }),
        at_day(0, 8) + time::Duration::minutes(4),
    )
    .await;
    seed_event(
        &fixture.db,
        site,
        landing,
        "signup",
        "/qa/landing",
        Some(49.5),
        json!({ "plan": "pro" }),
        at_day(0, 8) + time::Duration::minutes(5),
    )
    .await;

    // One goal, one hit — the only way a conversion exists.
    let goal = Uuid::new_v4();
    sqlx::query(
        "insert into analytics_goals (id, site_id, name, kind, match) \
         values ($1, $2, 'Signup', 'event', '{\"name\":\"signup\"}'::jsonb)",
    )
    .bind(goal)
    .bind(site)
    .fetch_optional(fixture.db.pool())
    .await
    .expect("the goal must be written");
    sqlx::query(
        "insert into analytics_goal_hits (goal_id, visitor_hash, step_position, occurred_at) \
         values ($1, $2, 1, $3)",
    )
    .bind(goal)
    .bind(hash("h3"))
    .bind(at_day(0, 9))
    .fetch_optional(fixture.db.pool())
    .await
    .expect("the goal hit must be written");

    let uri = format!("/api/v1/analytics/overview?site_id={site}&from={from}&to={today}&compare=1");
    let response = call(&fixture.state, report_request(&uri, &reader)).await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);

    let body = &response.body;
    assert_eq!(body["exact"], json!(true));
    assert_eq!(
        body["granularity"],
        json!("day"),
        "seven days are daily buckets"
    );
    // Four people, six pageviews, one goal conversion, one form, one download — and the visitor
    // who came back on a second day is still one visitor.
    assert_eq!(body["kpis"]["visitors"]["value"], json!(4));
    assert_eq!(body["kpis"]["pageviews"]["value"], json!(6));
    assert_eq!(body["kpis"]["conversions"]["value"], json!(1));
    assert_eq!(body["kpis"]["forms"]["value"], json!(1));
    assert_eq!(body["kpis"]["downloads"]["value"], json!(1));

    let series = body["series"].as_array().expect("a series");
    assert_eq!(series.len(), 7, "one point per day of the range");
    assert_eq!(series[0]["label"], json!(month_day(from)));
    assert_eq!(
        series[0]["visitors"],
        json!(0),
        "a day without traffic is a zero, not a gap"
    );
    assert_eq!(series[1]["visitors"], json!(1), "the oldest seeded day");
    assert_eq!(series[4]["visitors"], json!(1), "the returning visitor");
    assert_eq!(series[6]["visitors"], json!(2), "today holds two");
    assert_eq!(series[6]["pageviews"], json!(3));

    // Comparison: nothing was seeded before the window, so it says so instead of showing zero.
    assert_eq!(body["compare"], json!(true));
    assert_eq!(body["previous_has_data"], json!(false));
    assert_eq!(body["kpis"]["visitors"]["previous"], json!(0));

    // A visitor one day before the window makes the comparison real.
    seed_visit(
        &fixture.db,
        site,
        &RawVisit {
            hash: &hash("h9"),
            at: at_day(7, 10),
            path: "/qa/landing",
            title: "QA landing",
            device: "desktop",
            country: "TR",
            source: None,
            referrer_host: None,
            duration_ms: Some(500),
        },
    )
    .await;

    let compared = call(&fixture.state, report_request(&uri, &reader)).await;
    assert_eq!(compared.status, StatusCode::OK, "body: {}", compared.body);
    assert_eq!(compared.body["previous_has_data"], json!(true));
    assert_eq!(compared.body["kpis"]["visitors"]["previous"], json!(1));
    assert_eq!(
        compared.body["series"][6]["previous_visitors"],
        json!(1),
        "today's bucket carries the visitor of the same bucket one period earlier"
    );
    assert_eq!(
        compared.body["series"][0]["previous_visitors"],
        json!(0),
        "a bucket whose earlier twin was empty stays zero"
    );

    // Hourly granularity is offered where the range allows it.
    let two_days = format!(
        "/api/v1/analytics/overview?site_id={site}&from={}&to={today}&granularity=hour",
        today - time::Duration::days(1)
    );
    let hourly = call(&fixture.state, report_request(&two_days, &reader)).await;
    assert_eq!(hourly.status, StatusCode::OK, "body: {}", hourly.body);
    assert_eq!(hourly.body["granularity"], json!("hour"));
    let points = hourly.body["series"].as_array().expect("a series");
    assert_eq!(points.len(), 48, "two days are 48 hourly buckets");
    let busy = points
        .iter()
        .filter(|point| point["visitors"].as_i64().unwrap_or(0) > 0)
        .count();
    assert!(
        busy >= 3,
        "each seeded hour has its own bucket — {busy} buckets carry visitors"
    );

    // The side panels answer what the overview promises.
    assert_eq!(body["top_pages"][0]["value"], json!("/qa/landing"));
    assert_eq!(body["top_pages"][0]["views"], json!(2));
    assert_eq!(body["top_sources"][0]["value"], json!("newsletter"));
    let devices: Vec<String> = body["devices"]
        .as_array()
        .expect("devices")
        .iter()
        .map(|row| row["value"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(devices.contains(&"desktop".to_owned()));
    assert!(devices.contains(&"mobile".to_owned()));

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_page_report_filters_sorts_pages_and_exports_exactly_its_rows() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let reader = fixture.reader_token().await;
    let member = fixture.member_token().await;
    let other = fixture.other_reader_token().await;
    let owner = fixture.owner_token().await;
    let today = OffsetDateTime::now_utc().date();
    let from = today - time::Duration::days(6);

    // /qa/landing: three views by two visitors; /qa/pricing: two views; /qa/docs: one.
    for (index, (label, path, title, device, country, source)) in [
        (
            "h1",
            "/qa/landing",
            "QA landing",
            "desktop",
            "TR",
            Some("newsletter"),
        ),
        (
            "h1",
            "/qa/landing",
            "QA landing",
            "desktop",
            "TR",
            Some("newsletter"),
        ),
        (
            "h2",
            "/qa/landing",
            "QA landing, pricing",
            "mobile",
            "DE",
            None,
        ),
        ("h3", "/qa/pricing", "QA pricing", "desktop", "FR", None),
        (
            "h4",
            "/qa/pricing",
            "QA pricing",
            "mobile",
            "TR",
            Some("newsletter"),
        ),
        (
            "h5",
            "/qa/docs",
            "QA docs",
            "mobile",
            "TR",
            Some("newsletter"),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let visit = seed_visit(
            &fixture.db,
            site,
            &RawVisit {
                hash: &hash(label),
                at: at_day(0, 6) + time::Duration::minutes(index as i64),
                path,
                title,
                device,
                country,
                source,
                referrer_host: None,
                duration_ms: Some(1_000 + index as i32 * 100),
            },
        )
        .await;
        // A clean exit for every other visit, so entrances and exits are both exercised.
        if index % 2 == 0 {
            sqlx::query("update analytics_pageviews set is_exit = true where visit_id = $1")
                .bind(visit)
                .execute(fixture.db.pool())
                .await
                .expect("the exit flag must be written");
        }
    }

    let base = format!("/api/v1/analytics/pages?site_id={site}&from={from}&to={today}");
    let response = call(&fixture.state, report_request(&base, &reader)).await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);
    let rows = response.body["rows"].as_array().expect("rows");
    assert_eq!(response.body["total"], json!(3));
    assert_eq!(rows[0]["path"], json!("/qa/landing"));
    assert_eq!(rows[0]["views"], json!(3));
    assert_eq!(rows[0]["visitors"], json!(2));
    assert_eq!(rows[0]["views_per_visitor"], json!(1.5));
    assert_eq!(rows[0]["entrances"], json!(3));
    assert_eq!(
        rows[0]["title"],
        json!("QA landing, pricing"),
        "the most recent title of the path wins"
    );
    assert!(rows[0]["avg_time_ms"].as_f64().unwrap_or(0.0) > 0.0);
    assert_eq!(rows[0]["bounce_rate"], json!(0.0), "no visit bounced");

    // Sorting by visitors, ascending, flips the order.
    let ascending = format!("{base}&sort=visitors&dir=asc");
    let sorted = call(&fixture.state, report_request(&ascending, &reader)).await;
    let first = &sorted.body["rows"][0];
    assert!(
        first["visitors"].as_i64().unwrap_or(9) <= 1,
        "the least visited page comes first: {}",
        first
    );

    // Filters combine: device + country + source, plus a path substring.
    let filtered = format!("{base}&device=mobile&country=TR&source=newsletter&path=qa");
    let narrowed = call(&fixture.state, report_request(&filtered, &reader)).await;
    assert_eq!(narrowed.status, StatusCode::OK, "body: {}", narrowed.body);
    let rows = narrowed.body["rows"].as_array().expect("rows");
    assert_eq!(
        rows.len(),
        2,
        "pricing and docs belong to that visitor: {rows:?}"
    );
    let paths: Vec<&str> = rows
        .iter()
        .map(|row| row["path"].as_str().unwrap_or_default())
        .collect();
    assert!(paths.contains(&"/qa/pricing"));
    assert!(paths.contains(&"/qa/docs"));
    assert!(
        !paths.contains(&"/qa/landing"),
        "landing is desktop in the fixture"
    );

    // One row per page: paging is stable and the second page holds the second row.
    let paged = format!("{base}&sort=views&dir=desc&per_page=1&page=2");
    let page_two = call(&fixture.state, report_request(&paged, &reader)).await;
    let rows = page_two.body["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(page_two.body["total"], json!(3));
    assert_eq!(rows[0]["path"], json!("/qa/pricing"));

    // The CSV holds exactly the filtered rows, and its count rides in a header.
    let export_uri = format!(
        "/api/v1/analytics/export?report=pages&format=csv&site_id={site}&from={from}&to={today}&device=mobile&country=TR&source=newsletter&path=qa"
    );
    let exported = call(&fixture.state, report_request(&export_uri, &reader)).await;
    assert_eq!(
        exported.status,
        StatusCode::FORBIDDEN,
        "exporting is its own key"
    );

    let manager = fixture.manager_token().await;
    // The response of an export is CSV, so it is read as text (and its count off the header).
    let csv = exported_csv(&fixture, &export_uri, &manager).await;
    let lines: Vec<&str> = csv.trim_end().split('\n').collect();
    assert_eq!(lines.len(), 3, "a header and the two filtered rows");
    assert!(lines[0].starts_with("path,title,views,visitors"));
    assert!(csv.contains("/qa/pricing"));
    assert!(csv.contains("/qa/docs"));
    assert!(
        !csv.contains("/qa/landing"),
        "the file holds the filters, not the table"
    );

    // The page series is the drawer's own request.
    let series_uri = format!(
        "/api/v1/analytics/pages/series?site_id={site}&from={from}&to={today}&path=/qa/landing"
    );
    let series = call(&fixture.state, report_request(&series_uri, &reader)).await;
    assert_eq!(series.status, StatusCode::OK, "body: {}", series.body);
    let points = series.body.as_array().expect("points");
    assert_eq!(points.len(), 7);
    assert_eq!(points[6]["pageviews"], json!(3));

    // Permissions: reading is the read key, another organization is not the caller's business.
    let denied = call(&fixture.state, report_request(&base, &member)).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    let crossed = call(&fixture.state, report_request(&base, &other)).await;
    assert_eq!(crossed.status, StatusCode::FORBIDDEN);
    let owner_reads = call(&fixture.state, report_request(&base, &owner)).await;
    assert_eq!(
        owner_reads.status,
        StatusCode::OK,
        "the platform Owner reaches across"
    );

    // A sort key the report does not know is refused, not silently ignored.
    let bogus = format!("{base}&sort=views%3Bdrop%20table%20users");
    let refused = call(&fixture.state, report_request(&bogus, &reader)).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.body["error"]["code"], "invalid_report_query");
    let bad_day = format!("/api/v1/analytics/pages?site_id={site}&from=yesterday");
    let refused = call(&fixture.state, report_request(&bad_day, &reader)).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);

    fixture.cleanup().await;
}

/// Read an export as text (the JSON reader of `call` cannot).
async fn exported_csv(fixture: &Fixture, uri: &str, token: &str) -> String {
    let response = routes::router(fixture.state.clone())
        .oneshot(report_request(uri, token))
        .await
        .expect("router must answer");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/csv; charset=utf-8")
    );
    let header = response
        .headers()
        .get("x-export-rows")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .expect("the export names how many rows it holds");
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let csv = String::from_utf8(bytes.to_vec()).expect("CSV is UTF-8");
    assert_eq!(
        csv.trim_end().lines().count() - 1,
        header,
        "the header names exactly the rows the file holds"
    );

    csv
}

#[tokio::test]
async fn the_reports_answer_their_dimensions_and_an_empty_site_says_nothing_happened() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let empty = fixture.site_b;
    let reader = fixture.reader_token().await;
    let other_reader = fixture.other_reader_token().await;
    let today = OffsetDateTime::now_utc().date();
    let from = today - time::Duration::days(6);

    let desktop = seed_visit(
        &fixture.db,
        site,
        &RawVisit {
            hash: &hash("d1"),
            at: at_day(0, 7),
            path: "/qa/landing",
            title: "QA landing",
            device: "desktop",
            country: "TR",
            source: Some("newsletter"),
            referrer_host: None,
            duration_ms: Some(700),
        },
    )
    .await;
    seed_event(
        &fixture.db,
        site,
        desktop,
        "download",
        "/qa/landing",
        None,
        json!({ "file": "/qa/files/guide.pdf" }),
        at_day(0, 7) + time::Duration::minutes(1),
    )
    .await;
    seed_event(
        &fixture.db,
        site,
        desktop,
        "download",
        "/qa/landing",
        None,
        json!({ "file": "/qa/files/guide.pdf" }),
        at_day(0, 7) + time::Duration::minutes(2),
    )
    .await;
    seed_event(
        &fixture.db,
        site,
        desktop,
        "download",
        "/qa/docs",
        None,
        json!({ "file": "/qa/files/notes.pdf" }),
        at_day(0, 7) + time::Duration::minutes(3),
    )
    .await;
    let mobile = seed_visit(
        &fixture.db,
        site,
        &RawVisit {
            hash: &hash("d2"),
            at: at_day(1, 15),
            path: "/qa/contact",
            title: "QA contact",
            device: "mobile",
            country: "DE",
            source: None,
            referrer_host: Some("google.example"),
            duration_ms: Some(1_500),
        },
    )
    .await;
    seed_event(
        &fixture.db,
        site,
        mobile,
        "form_start",
        "/qa/contact",
        None,
        json!({ "form": "contact" }),
        at_day(1, 15) + time::Duration::minutes(1),
    )
    .await;
    seed_event(
        &fixture.db,
        site,
        mobile,
        "form_submit",
        "/qa/contact",
        Some(120.0),
        json!({ "form": "contact" }),
        at_day(1, 15) + time::Duration::minutes(2),
    )
    .await;
    seed_event(
        &fixture.db,
        site,
        mobile,
        "form_submit",
        "/qa/contact",
        Some(80.0),
        json!({ "form": "newsletter" }),
        at_day(1, 15) + time::Duration::minutes(3),
    )
    .await;
    // A visit that carried neither a campaign nor a referrer: the report calls it direct.
    seed_visit(
        &fixture.db,
        site,
        &RawVisit {
            hash: &hash("d3"),
            at: at_day(3, 13),
            path: "/qa/pricing",
            title: "QA pricing",
            device: "desktop",
            country: "FR",
            source: None,
            referrer_host: None,
            duration_ms: Some(600),
        },
    )
    .await;

    // Sources: the UTM combination, the referrer, and the direct visit that carried neither.
    let sources = call(
        &fixture.state,
        report_request(
            &format!("/api/v1/analytics/sources?site_id={site}&from={from}&to={today}"),
            &reader,
        ),
    )
    .await;
    assert_eq!(sources.status, StatusCode::OK, "body: {}", sources.body);
    let rows = sources.body["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 3, "newsletter, google and direct: {rows:?}");
    let names: Vec<&str> = rows
        .iter()
        .map(|row| row["source"].as_str().unwrap_or_default())
        .collect();
    assert!(names.contains(&"newsletter"));
    assert!(names.contains(&"google.example"));
    assert!(names.contains(&"(direct)"));
    for row in rows {
        assert_eq!(row["visits"], json!(1));
        assert_eq!(row["medium"].as_str(), None, "no medium was carried");
    }

    // Grouping collapses the combination to one dimension.
    let grouped = call(
        &fixture.state,
        report_request(
            &format!(
                "/api/v1/analytics/sources?site_id={site}&from={from}&to={today}&group=device"
            ),
            &reader,
        ),
    )
    .await;
    assert_eq!(
        grouped.status,
        StatusCode::BAD_REQUEST,
        "device is not a source dimension"
    );

    let grouped = call(
        &fixture.state,
        report_request(
            &format!(
                "/api/v1/analytics/sources?site_id={site}&from={from}&to={today}&group=medium"
            ),
            &reader,
        ),
    )
    .await;
    assert_eq!(grouped.status, StatusCode::OK, "body: {}", grouped.body);
    let rows = grouped.body["rows"].as_array().expect("rows");
    assert!(
        rows.iter().any(|row| row["source"] == json!("(none)")),
        "a visit without a medium groups under (none): {rows:?}"
    );

    // Audience: the panels and the countries table.
    let audience = call(
        &fixture.state,
        report_request(
            &format!("/api/v1/analytics/audience?site_id={site}&from={from}&to={today}"),
            &reader,
        ),
    )
    .await;
    assert_eq!(audience.status, StatusCode::OK, "body: {}", audience.body);
    let panels = audience.body["panels"].as_array().expect("panels");
    assert_eq!(
        panels.len(),
        5,
        "devices, browsers, systems, screens, languages"
    );
    assert_eq!(panels[0]["kind"], json!("device"));
    let devices: Vec<&str> = panels[0]["rows"]
        .as_array()
        .expect("device rows")
        .iter()
        .map(|row| row["value"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(devices.len(), 2);
    assert!(devices.contains(&"desktop") && devices.contains(&"mobile"));
    let countries = audience.body["countries"].as_array().expect("countries");
    let codes: Vec<&str> = countries
        .iter()
        .map(|row| row["code"].as_str().unwrap_or_default())
        .collect();
    assert!(codes.contains(&"TR") && codes.contains(&"DE"));
    let total: f64 = countries
        .iter()
        .map(|row| row["share"].as_f64().unwrap_or(0.0))
        .sum();
    assert!((total - 1.0).abs() < 0.001, "the shares add up to one");

    // Events: counts, values and the property breakdown of one event.
    let events = call(
        &fixture.state,
        report_request(
            &format!("/api/v1/analytics/events?site_id={site}&from={from}&to={today}"),
            &reader,
        ),
    )
    .await;
    assert_eq!(events.status, StatusCode::OK, "body: {}", events.body);
    let rows = events.body["rows"].as_array().expect("rows");
    let submit = rows
        .iter()
        .find(|row| row["name"] == json!("form_submit"))
        .expect("the submission is reported");
    assert_eq!(submit["count"], json!(2));
    assert_eq!(submit["visitors"], json!(1));
    assert_eq!(submit["value_sum"], json!(200.0));

    let detail = call(
        &fixture.state,
        report_request(
            &format!("/api/v1/analytics/events/form_submit?site_id={site}&from={from}&to={today}"),
            &reader,
        ),
    )
    .await;
    assert_eq!(detail.status, StatusCode::OK, "body: {}", detail.body);
    let properties = detail.body["properties"].as_array().expect("properties");
    let forms: Vec<(&str, i64)> = properties
        .iter()
        .filter(|row| row["key"] == json!("form"))
        .map(|row| {
            (
                row["value"].as_str().unwrap_or_default(),
                row["count"].as_i64().unwrap_or(0),
            )
        })
        .collect();
    assert_eq!(forms, vec![("contact", 1), ("newsletter", 1)]);
    assert_eq!(detail.body["series"].as_array().expect("series").len(), 7);

    // Downloads: by file and by page.
    let downloads = call(
        &fixture.state,
        report_request(
            &format!("/api/v1/analytics/downloads?site_id={site}&from={from}&to={today}"),
            &reader,
        ),
    )
    .await;
    assert_eq!(downloads.status, StatusCode::OK, "body: {}", downloads.body);
    assert_eq!(downloads.body["total"], json!(3));
    let files = downloads.body["files"].as_array().expect("files");
    assert_eq!(files[0]["value"], json!("/qa/files/guide.pdf"));
    assert_eq!(files[0]["downloads"], json!(2));
    let pages = downloads.body["pages"].as_array().expect("pages");
    let page_names: Vec<&str> = pages
        .iter()
        .map(|row| row["value"].as_str().unwrap_or_default())
        .collect();
    assert!(page_names.contains(&"/qa/docs"));

    // Forms: completion needs `form_start`; the form without one reports a dash, not a zero.
    let forms = call(
        &fixture.state,
        report_request(
            &format!("/api/v1/analytics/forms?site_id={site}&from={from}&to={today}"),
            &reader,
        ),
    )
    .await;
    assert_eq!(forms.status, StatusCode::OK, "body: {}", forms.body);
    let rows = forms.body["rows"].as_array().expect("rows");
    let contact = rows
        .iter()
        .find(|row| row["form"] == json!("contact"))
        .expect("the contact form");
    assert_eq!(contact["submissions"], json!(1));
    assert_eq!(contact["starts"], json!(1));
    assert_eq!(contact["completion_rate"], json!(1.0));
    assert_eq!(contact["abandonment"], json!(0));
    let newsletter = rows
        .iter()
        .find(|row| row["form"] == json!("newsletter"))
        .expect("the newsletter form");
    assert_eq!(newsletter["starts"], json!(0));
    assert_eq!(newsletter["completion_rate"], json!(null));
    assert_eq!(newsletter["abandonment"], json!(null));

    // The empty site answers, with zeroes and empty tables — never an error.
    let other = other_reader;
    let empty_overview = call(
        &fixture.state,
        report_request(
            &format!("/api/v1/analytics/overview?site_id={empty}&from={from}&to={today}"),
            &other,
        ),
    )
    .await;
    assert_eq!(
        empty_overview.status,
        StatusCode::OK,
        "body: {}",
        empty_overview.body
    );
    assert_eq!(empty_overview.body["kpis"]["visitors"]["value"], json!(0));
    for point in empty_overview.body["series"].as_array().expect("series") {
        assert_eq!(point["visitors"], json!(0));
        assert_eq!(point["pageviews"], json!(0));
    }
    assert_eq!(empty_overview.body["top_pages"], json!([]));

    let empty_pages = call(
        &fixture.state,
        report_request(
            &format!("/api/v1/analytics/pages?site_id={empty}&from={from}&to={today}"),
            &other,
        ),
    )
    .await;
    assert_eq!(
        empty_pages.status,
        StatusCode::OK,
        "body: {}",
        empty_pages.body
    );
    assert_eq!(empty_pages.body["rows"], json!([]));
    assert_eq!(empty_pages.body["total"], json!(0));

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// Goals, funnels and realtime (REQ-007, slice 3)
// ---------------------------------------------------------------------------------------------

/// How many hits one goal holds.
async fn goal_hits(fixture: &Fixture, goal_id: &str) -> i64 {
    sqlx::query_scalar("select count(*)::bigint from analytics_goal_hits where goal_id = $1::uuid")
        .bind(goal_id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the hits must count")
}

/// The three-step goal the funnel walk works with: a landing page, a download, a form.
fn funnel_goal() -> Value {
    json!({
        "name": "QA funnel",
        "kind": "form_submit",
        "match": { "name": "contact" },
        "enabled": true,
        "steps": [
            { "kind": "pageview", "match": { "path": "/qa/landing" } },
            { "kind": "download", "match": { "file": "/qa/files/guide.pdf" } },
            { "kind": "form_submit", "match": { "name": "contact" } }
        ]
    })
}

/// A visitor whose one beacon carries the landing page, the download and the form: the ordered
/// funnel must move three steps in a single beacon, because that is what the batch holds.
fn completing_beacon() -> Value {
    pageview_beacon(
        "/qa/landing",
        json!([
            { "name": "download", "properties": { "file": "/qa/files/guide.pdf" } },
            { "name": "form_submit", "value": 25.0, "properties": { "form": "contact" } }
        ]),
    )
}

#[tokio::test]
async fn goals_record_ordered_deduplicated_hits_and_report_their_funnel() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let manager = fixture.manager_token().await;
    let reader = fixture.reader_token().await;
    let today = OffsetDateTime::now_utc().date();
    let from = today - time::Duration::days(6);
    let scope = format!("site_id={site}&from={from}&to={today}");

    // A goal is created with its ordered steps; the goal row mirrors the last one.
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/analytics/goals?{scope}"),
            Some(&manager),
            Some(funnel_goal()),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "body: {}",
        created.body
    );
    let goal_id = created.body["id"].as_str().expect("a goal id").to_owned();
    assert_eq!(created.body["steps"].as_array().expect("steps").len(), 3);
    assert_eq!(created.body["kind"], json!("form_submit"));
    assert_eq!(created.body["match"]["name"], json!("contact"));

    // The editor's refusals are the API's refusals.
    let empty_match = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/analytics/goals?{scope}"),
            Some(&manager),
            Some(json!({
                "name": "No match",
                "kind": "pageview",
                "match": {},
                "enabled": true,
                "steps": []
            })),
        ),
    )
    .await;
    assert_eq!(
        empty_match.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        empty_match.body
    );
    assert_eq!(empty_match.body["error"]["code"], json!("invalid_goal"));

    let nameless = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/analytics/goals?{scope}"),
            Some(&manager),
            Some(json!({
                "name": "   ",
                "kind": "pageview",
                "match": { "path": "/qa/landing" },
                "enabled": true,
                "steps": []
            })),
        ),
    )
    .await;
    assert_eq!(
        nameless.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        nameless.body
    );

    let duplicate = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/analytics/goals?{scope}"),
            Some(&manager),
            Some(funnel_goal()),
        ),
    )
    .await;
    assert_eq!(
        duplicate.status,
        StatusCode::CONFLICT,
        "body: {}",
        duplicate.body
    );
    assert_eq!(duplicate.body["error"]["code"], json!("goal_name_taken"));

    // Reading a goal is `analytics.read`; creating one is `analytics.goals.manage`.
    let reader_list = call(
        &fixture.state,
        report_request(&format!("/api/v1/analytics/goals?{scope}"), &reader),
    )
    .await;
    assert_eq!(
        reader_list.status,
        StatusCode::OK,
        "body: {}",
        reader_list.body
    );

    let reader_create = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/analytics/goals?{scope}"),
            Some(&reader),
            Some(json!({
                "name": "Reader goal",
                "kind": "pageview",
                "match": { "path": "/qa/landing" },
                "enabled": true,
                "steps": []
            })),
        ),
    )
    .await;
    assert_eq!(reader_create.status, StatusCode::FORBIDDEN);

    let member = fixture.member_token().await;
    let member_list = call(
        &fixture.state,
        report_request(&format!("/api/v1/analytics/goals?{scope}"), &member),
    )
    .await;
    assert_eq!(member_list.status, StatusCode::FORBIDDEN);

    // Four visitors arrive; three of them enter the funnel and stop where they stop.
    let completing = fixture
        .collect(
            &fixture.key_a,
            beacon(
                "/api/v1/public/analytics/collect",
                completing_beacon(),
                CHROME,
                Some("203.0.113.41"),
                &[],
            ),
        )
        .await;
    let reached = completing.body["reached_goals"]
        .as_array()
        .expect("reached goals");
    assert_eq!(
        reached.len(),
        3,
        "one beacon carried the visitor three steps: {}",
        completing.body
    );
    assert_eq!(reached[0]["step_position"], json!(1));
    assert_eq!(reached[2]["is_final"], json!(true));

    fixture
        .collect(
            &fixture.key_a,
            beacon(
                "/api/v1/public/analytics/collect",
                pageview_beacon(
                    "/qa/landing",
                    json!([{ "name": "download", "properties": { "file": "/qa/files/guide.pdf" } }]),
                ),
                CHROME,
                Some("203.0.113.42"),
                &[],
            ),
        )
        .await;
    fixture
        .collect(
            &fixture.key_a,
            beacon(
                "/api/v1/public/analytics/collect",
                pageview_beacon(
                    "/qa/pricing",
                    json!([{ "name": "download", "properties": { "file": "/qa/files/guide.pdf" } }]),
                ),
                CHROME,
                Some("203.0.113.43"),
                &[],
            ),
        )
        .await;
    fixture
        .collect(
            &fixture.key_a,
            beacon(
                "/api/v1/public/analytics/collect",
                pageview_beacon(
                    "/qa/landing",
                    json!([{ "name": "form_submit", "properties": { "form": "contact" } }]),
                ),
                CHROME,
                Some("203.0.113.44"),
                &[],
            ),
        )
        .await;

    // Six hits: three for the complete walk, two for the shortened one, one for the visitor who
    // skipped the download (a later step is not reachable before the one before it).
    assert_eq!(goal_hits(&fixture, &goal_id).await, 6);

    // Re-sending the same beacon changes nothing, and says nothing.
    let again = fixture
        .collect(
            &fixture.key_a,
            beacon(
                "/api/v1/public/analytics/collect",
                completing_beacon(),
                CHROME,
                Some("203.0.113.41"),
                &[],
            ),
        )
        .await;
    assert_eq!(again.body["reached_goals"], json!([]));
    assert_eq!(goal_hits(&fixture, &goal_id).await, 6);

    // The funnel is monotonically non-increasing, with the drop-offs the range saw.
    let funnel = call(
        &fixture.state,
        report_request(
            &format!("/api/v1/analytics/goals/{goal_id}/funnel?{scope}"),
            &reader,
        ),
    )
    .await;
    assert_eq!(funnel.status, StatusCode::OK, "body: {}", funnel.body);
    let steps = funnel.body["steps"].as_array().expect("steps");
    let reached: Vec<i64> = steps
        .iter()
        .map(|step| step["visitors"].as_i64().unwrap_or_default())
        .collect();
    assert_eq!(reached, vec![3, 2, 1]);
    assert!(
        reached.windows(2).all(|pair| pair[0] >= pair[1]),
        "a funnel that grows is not a funnel: {reached:?}"
    );
    assert_eq!(steps[0]["drop_off"], json!(0));
    assert_eq!(steps[1]["drop_off"], json!(1));
    assert_eq!(steps[2]["drop_off"], json!(1));
    assert_eq!(funnel.body["conversions"], json!(1));
    assert_eq!(funnel.body["visitors"], json!(4));
    assert_eq!(funnel.body["rate"], json!(0.25));
    assert_eq!(steps[0]["rate"], json!(0.75));

    // The list agrees with the funnel and carries the last hit.
    let list = call(
        &fixture.state,
        report_request(&format!("/api/v1/analytics/goals?{scope}"), &reader),
    )
    .await;
    assert_eq!(list.status, StatusCode::OK, "body: {}", list.body);
    let rows = list.body["goals"].as_array().expect("goals");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], json!(goal_id));
    assert_eq!(rows[0]["conversions"], json!(1));
    assert_eq!(rows[0]["visitors"], json!(4));
    assert_eq!(rows[0]["rate"], json!(0.25));
    assert!(
        !rows[0]["last_hit"].is_null(),
        "the goal was hit inside the range"
    );

    // A finished goal is a platform event, recorded once for the one visitor who finished.
    let emitted: i64 = sqlx::query_scalar(
        "select count(*)::bigint from events where site_id = $1 and name = 'analytics.goal_reached'",
    )
    .bind(site)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the events must count");
    assert_eq!(emitted, 1);

    // The switch is a partial write: it must not touch the match it did not mention.
    let switched = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/analytics/goals/{goal_id}?{scope}"),
            Some(&manager),
            Some(json!({ "enabled": false })),
        ),
    )
    .await;
    assert_eq!(switched.status, StatusCode::OK, "body: {}", switched.body);
    assert_eq!(switched.body["enabled"], json!(false));
    assert_eq!(switched.body["steps"].as_array().expect("steps").len(), 3);

    // A switched-off goal records no new hits.
    let quiet = fixture
        .collect(
            &fixture.key_a,
            beacon(
                "/api/v1/public/analytics/collect",
                pageview_beacon(
                    "/qa/landing",
                    json!([{ "name": "form_submit", "properties": { "form": "contact" } }]),
                ),
                CHROME,
                Some("203.0.113.45"),
                &[],
            ),
        )
        .await;
    assert_eq!(quiet.body["reached_goals"], json!([]));
    assert_eq!(goal_hits(&fixture, &goal_id).await, 6);

    // Deleting the goal takes its steps and hits with it.
    let deleted = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/analytics/goals/{goal_id}?{scope}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(goal_hits(&fixture, &goal_id).await, 0);
    let steps_left: i64 = sqlx::query_scalar(
        "select count(*)::bigint from analytics_goal_steps where goal_id = $1::uuid",
    )
    .bind(&goal_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the steps must count");
    assert_eq!(steps_left, 0);

    let missing = call(
        &fixture.state,
        report_request(
            &format!("/api/v1/analytics/goals/{goal_id}/funnel?{scope}"),
            &reader,
        ),
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.body["error"]["code"], json!("goal_not_found"));

    fixture.cleanup().await;
}

#[tokio::test]
async fn realtime_reads_the_last_half_hour_and_respects_the_scope() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let empty = fixture.site_b;
    let manager = fixture.manager_token().await;
    let reader = fixture.reader_token().await;
    let other = fixture.other_reader_token().await;
    let member = fixture.member_token().await;
    let today = OffsetDateTime::now_utc().date();
    let from = today - time::Duration::days(6);
    let scope = format!("site_id={site}&from={from}&to={today}");

    // A conversion exists before the traffic does, so the window has something to count.
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/analytics/goals?{scope}"),
            Some(&manager),
            Some(json!({
                "name": "QA landing view",
                "kind": "pageview",
                "match": { "path": "/qa/landing" },
                "enabled": true,
                "steps": []
            })),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "body: {}",
        created.body
    );

    // One beacon: a pageview, a download and a custom event — and a goal hit.
    fixture
        .collect(
            &fixture.key_a,
            beacon(
                "/api/v1/public/analytics/collect",
                pageview_beacon(
                    "/qa/landing",
                    json!([
                        { "name": "download", "value": 12.5, "properties": { "file": "/qa/files/guide.pdf" } },
                        { "name": "cta_click", "properties": { "slot": "hero" } }
                    ]),
                ),
                CHROME,
                Some("203.0.113.61"),
                &[],
            ),
        )
        .await;

    // Realtime reads the raw rows: no rollup tick stands between the beacon and the screen.
    let snapshot = call(
        &fixture.state,
        report_request(
            &format!("/api/v1/analytics/realtime?site_id={site}"),
            &reader,
        ),
    )
    .await;
    assert_eq!(snapshot.status, StatusCode::OK, "body: {}", snapshot.body);
    assert_eq!(snapshot.body["last_5"]["visitors"], json!(1));
    assert_eq!(snapshot.body["last_5"]["pageviews"], json!(1));
    assert_eq!(snapshot.body["last_5"]["events"], json!(2));
    assert_eq!(snapshot.body["last_5"]["conversions"], json!(1));
    assert_eq!(snapshot.body["last_30"]["visitors"], json!(1));
    assert_eq!(snapshot.body["last_30"]["events"], json!(2));

    let pages = snapshot.body["pages"].as_array().expect("pages");
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0]["path"], json!("/qa/landing"));
    assert_eq!(pages[0]["visitors"], json!(1));
    assert_eq!(pages[0]["views"], json!(1));

    let names: Vec<&str> = snapshot.body["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["name"].as_str().unwrap_or_default())
        .collect();
    assert!(
        names.contains(&"download"),
        "the feed carries the download: {names:?}"
    );
    assert!(
        names.contains(&"cta_click"),
        "the feed carries the custom event: {names:?}"
    );

    // An event that carries a value decodes it: `numeric` is not a float, and a feed that fails
    // on the first value would only fail in production.
    let download = snapshot.body["events"]
        .as_array()
        .expect("events")
        .iter()
        .find(|event| event["name"] == json!("download"))
        .expect("the download event");
    assert_eq!(download["value"], json!(12.5));

    // A site nobody visited answers zeroes and empty tables, not an error.
    let quiet = call(
        &fixture.state,
        report_request(
            &format!("/api/v1/analytics/realtime?site_id={empty}"),
            &other,
        ),
    )
    .await;
    assert_eq!(quiet.status, StatusCode::OK, "body: {}", quiet.body);
    assert_eq!(quiet.body["last_5"]["visitors"], json!(0));
    assert_eq!(quiet.body["pages"], json!([]));
    assert_eq!(quiet.body["events"], json!([]));

    // The realtime view is `analytics.read`: a member without it is refused, and another
    // organization cannot read this site's live traffic.
    let refused = call(
        &fixture.state,
        report_request(
            &format!("/api/v1/analytics/realtime?site_id={site}"),
            &member,
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    let cross_org = call(
        &fixture.state,
        report_request(
            &format!("/api/v1/analytics/realtime?site_id={site}"),
            &other,
        ),
    )
    .await;
    assert_eq!(cross_org.status, StatusCode::FORBIDDEN);

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// Privacy operations (docs/requests/REQ-007, slice 4)
// ---------------------------------------------------------------------------------------------

/// A receiver the delivery engine can POST to, so "the event reached a subscribed endpoint" is
/// proven by bytes rather than by a queue row.
struct ComplianceReceiver {
    /// Where the endpoint points.
    url: String,
    /// Deliveries as they arrived, oldest first.
    seen: Arc<Mutex<Vec<Value>>>,
    /// The task serving the port; aborted when the receiver drops.
    task: tokio::task::JoinHandle<()>,
}

/// Shared state of the receiver.
#[derive(Clone)]
struct ReceiverState {
    seen: Arc<Mutex<Vec<Value>>>,
}

impl ComplianceReceiver {
    /// Start the receiver on an ephemeral loopback port.
    async fn start() -> Self {
        let state = ReceiverState {
            seen: Arc::new(Mutex::new(Vec::new())),
        };
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the receiver must bind a port");
        let address = listener.local_addr().expect("the receiver has an address");

        let app = Router::new()
            .route("/hooks/omnion", route_post(capture_delivery))
            .with_state(state.clone());

        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            url: format!("http://{address}/hooks/omnion"),
            seen: state.seen,
            task,
        }
    }

    /// The names of the events the receiver captured, in the order they arrived.
    fn events(&self) -> Vec<String> {
        self.seen
            .lock()
            .expect("the receiver lock")
            .iter()
            .filter_map(|delivery| delivery["name"].as_str().map(str::to_owned))
            .collect()
    }

    /// The payload of the first delivery of one event.
    fn payload(&self, name: &str) -> Option<Value> {
        self.seen
            .lock()
            .expect("the receiver lock")
            .iter()
            .find(|delivery| delivery["name"].as_str() == Some(name))
            .map(|delivery| delivery["payload"].clone())
    }
}

impl Drop for ComplianceReceiver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `POST /hooks/omnion` — capture one delivery.
async fn capture_delivery(
    State(state): State<ReceiverState>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let event = headers
        .get(signature::EVENT_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let payload: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);

    state
        .seen
        .lock()
        .expect("the receiver lock")
        .push(json!({ "name": event, "payload": payload["payload"].clone() }));

    StatusCode::OK
}

/// Queue a delivery tick with the fast backoff this suite uses.
async fn deliver_queued(fixture: &Fixture) -> engine::RunReport {
    let client = sender::client(std::time::Duration::from_secs(5)).expect("the client must build");
    let config = engine::RunnerConfig {
        batch: 50,
        lease_seconds: 30,
        request_timeout: std::time::Duration::from_secs(5),
        retry_base: time::Duration::milliseconds(40),
        retry_max: time::Duration::milliseconds(320),
    };

    engine::run_due(fixture.db.pool(), &client, &config)
        .await
        .expect("the delivery tick must run")
}

/// How many rows of one table carry a visitor handle on a site.
async fn visits_for(db: &Db, site: Uuid, handle: &str) -> i64 {
    sqlx::query_scalar(
        "select count(*)::bigint from analytics_visits where site_id = $1 and visitor_hash = $2",
    )
    .bind(site)
    .bind(handle)
    .fetch_one(db.pool())
    .await
    .expect("the count must run")
}

/// How many goal hits one handle carries on a site.
async fn goal_hits_for(db: &Db, site: Uuid, handle: &str) -> i64 {
    sqlx::query_scalar(
        "select count(*)::bigint from analytics_goal_hits h \
         join analytics_goals g on g.id = h.goal_id \
         where g.site_id = $1 and h.visitor_hash = $2",
    )
    .bind(site)
    .bind(handle)
    .fetch_one(db.pool())
    .await
    .expect("the count must run")
}

/// The audit rows of one site: `(kind, cutoff, rows_removed)` oldest first.
async fn audit_rows(db: &Db, site: Uuid) -> Vec<(String, Option<OffsetDateTime>, i64)> {
    sqlx::query_as::<_, (String, Option<OffsetDateTime>, i64)>(
        "select kind, cutoff, rows_removed from analytics_purges where site_id = $1 \
         order by created_at, kind",
    )
    .bind(site)
    .fetch_all(db.pool())
    .await
    .expect("the audit rows must read")
}

/// The payloads of one event name on a site, oldest first.
async fn event_payloads(db: &Db, site: Uuid, name: &str) -> Vec<Value> {
    sqlx::query_scalar("select payload from events where site_id = $1 and name = $2 order by id")
        .bind(site)
        .bind(name)
        .fetch_all(db.pool())
        .await
        .expect("the events must read")
}

/// The settings body the walks store: a full update, exactly as the screen sends it.
fn settings_body(retention_days: i32) -> Value {
    json!({
        "tracking_enabled": true,
        "mode": "cookieless",
        "anonymize_ip": true,
        "respect_dnt": true,
        "bot_filter": true,
        "sample_rate": 100,
        "retention_days": retention_days,
        "excluded_paths": ["/qa/private/*"],
        "excluded_ips": []
    })
}

#[tokio::test]
async fn the_purge_and_the_erasure_remove_exactly_their_rows_and_say_so() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let other_site = fixture.site_b;
    let manager = fixture.manager_token().await;
    let reader = fixture.reader_token().await;
    let member = fixture.member_token().await;
    let other_reader = fixture.other_reader_token().await;
    let today = OffsetDateTime::now_utc().date();

    // The settings screen's own read: the cutoff the purge would use, the last run and the
    // storage table all arrive with the settings — so a missing purge cutoff can never be
    // rendered as an empty button label.
    let loaded = call(
        &fixture.state,
        report_request(
            &format!("/api/v1/analytics/settings?site_id={site}"),
            &manager,
        ),
    )
    .await;
    assert_eq!(loaded.status, StatusCode::OK, "body: {}", loaded.body);
    assert_eq!(loaded.body["last_purge"], json!(null));
    assert!(
        loaded.body["storage"].as_array().expect("storage").len() >= 8,
        "the storage table covers the schema: {}",
        loaded.body["storage"]
    );
    assert!(
        loaded.body["storage"]
            .as_array()
            .expect("storage")
            .iter()
            .any(|field| field["personal"] == json!(true)),
        "the table names the personal columns"
    );

    // Retention of 7 days, so the fixture below can sit on both sides of the cutoff.
    let saved = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/analytics/settings?site_id={site}"),
            Some(&manager),
            Some(settings_body(7)),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "body: {}", saved.body);
    let expected_cutoff = (today - time::Duration::days(7))
        .midnight()
        .assume_utc()
        .date()
        .to_string();
    assert!(
        saved.body["purge_cutoff"]
            .as_str()
            .expect("a cutoff")
            .starts_with(&expected_cutoff),
        "the screen can name the cutoff before the button is pressed: {}",
        saved.body["purge_cutoff"]
    );

    // A receiver subscribed to both compliance events, through a real endpoint row.
    let receiver = ComplianceReceiver::start().await;
    sqlx::query(
        "insert into webhook_endpoints (id, organization_id, name, url, secret, events, enabled) \
         values ($1, $2, $3, $4, $5, $6, true)",
    )
    .bind(Uuid::new_v4())
    .bind(fixture.organizations[0])
    .bind(format!("Compliance {}", &Uuid::new_v4().simple().to_string()[..8]))
    .bind(&receiver.url)
    .bind("0123456789abcdef0123456789abcdef")
    .bind(vec![
        "analytics.retention_purged".to_owned(),
        "analytics.erasure_completed".to_owned(),
    ])
    .execute(fixture.db.pool())
    .await
    .expect("the endpoint must be created");

    // Rows on both sides of the cutoff, on two sites, plus a salt far outside every window.
    let stale = hash("purge-stale");
    let fresh = hash("purge-fresh");
    let other = hash("purge-other");
    let stale_visit = seed_visit(
        &fixture.db,
        site,
        &RawVisit {
            hash: &stale,
            at: at_day(30, 9),
            path: "/qa/old",
            title: "Old page",
            device: "desktop",
            country: "TR",
            source: None,
            referrer_host: None,
            duration_ms: Some(900),
        },
    )
    .await;
    seed_event(
        &fixture.db,
        site,
        stale_visit,
        "signup",
        "/qa/old",
        Some(9.0),
        json!({ "plan": "old" }),
        at_day(30, 9),
    )
    .await;
    seed_visit(
        &fixture.db,
        site,
        &RawVisit {
            hash: &fresh,
            at: at_day(2, 9),
            path: "/qa/recent",
            title: "Recent page",
            device: "desktop",
            country: "TR",
            source: None,
            referrer_host: None,
            duration_ms: Some(500),
        },
    )
    .await;
    seed_visit(
        &fixture.db,
        other_site,
        &RawVisit {
            hash: &other,
            at: at_day(30, 9),
            path: "/qa/other",
            title: "Another site",
            device: "desktop",
            country: "TR",
            source: None,
            referrer_host: None,
            duration_ms: None,
        },
    )
    .await;
    let ancient_salt = (today - time::Duration::days(400)).to_string();
    sqlx::query("insert into analytics_salts (day, salt) values ($1::date, $2)")
        .bind(&ancient_salt)
        .bind("f".repeat(64))
        .execute(fixture.db.pool())
        .await
        .expect("the ancient salt must be written");

    // Running a purge is `analytics.settings.manage`, like the settings themselves.
    for (label, token) in [("reader", &reader), ("member", &member), ("other org", &other_reader)] {
        let refused = call(
            &fixture.state,
            request(
                Method::POST,
                &format!("/api/v1/analytics/purge?site_id={site}"),
                Some(token),
                None,
            ),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{label} may not purge: {}",
            refused.body
        );
    }

    let purged = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/analytics/purge?site_id={site}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(purged.status, StatusCode::OK, "body: {}", purged.body);
    assert_eq!(purged.body["kind"], json!("retention"));
    assert_eq!(purged.body["visits"], json!(1), "body: {}", purged.body);
    assert_eq!(purged.body["pageviews"], json!(1));
    assert_eq!(purged.body["events"], json!(1));
    assert_eq!(purged.body["goal_hits"], json!(0));
    assert_eq!(purged.body["rows_removed"], json!(3));
    assert!(
        purged.body["cutoff"]
            .as_str()
            .expect("a cutoff")
            .starts_with(&expected_cutoff)
    );

    // Exactly the intended rows went, and nothing else did.
    assert_eq!(visits_for(&fixture.db, site, &stale).await, 0, "the stale visit is gone");
    assert_eq!(visits_for(&fixture.db, site, &fresh).await, 1, "the recent visit stays");
    assert_eq!(
        rows(&fixture.db, "analytics_visits", other_site).await,
        1,
        "another site is never touched"
    );
    let salts: i64 = sqlx::query_scalar("select count(*)::bigint from analytics_salts where day = $1::date")
        .bind(&ancient_salt)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the salt count must run");
    assert_eq!(salts, 0, "a salt past every window is pruned with the rows");

    // The audit row, and the event the platform records for a subscriber.
    let audit = audit_rows(&fixture.db, site).await;
    assert_eq!(audit.len(), 1, "one run, one audit row: {audit:?}");
    assert_eq!(audit[0].0, "retention");
    assert_eq!(audit[0].2, 3);
    assert!(audit[0].1.is_some(), "a retention run names its cutoff");
    let recorded = event_payloads(&fixture.db, site, "analytics.retention_purged").await;
    assert_eq!(recorded.len(), 1, "one purge, one event");
    assert_eq!(recorded[0]["rows_removed"], json!(3));

    // The erasure: the same handle on two sites, with a pageview, an event and a goal hit.
    let doomed = hash("erase-me");
    let visit_id = seed_visit(
        &fixture.db,
        site,
        &RawVisit {
            hash: &doomed,
            at: at_day(1, 10),
            path: "/qa/erase",
            title: "Erase me",
            device: "mobile",
            country: "TR",
            source: None,
            referrer_host: None,
            duration_ms: Some(1200),
        },
    )
    .await;
    seed_pageview(&fixture.db, site, visit_id, "/qa/erase/thanks", "Thanks", at_day(1, 10)).await;
    seed_event(
        &fixture.db,
        site,
        visit_id,
        "signup",
        "/qa/erase",
        Some(10.0),
        json!({ "plan": "pro" }),
        at_day(1, 11),
    )
    .await;
    seed_visit(
        &fixture.db,
        other_site,
        &RawVisit {
            hash: &doomed,
            at: at_day(1, 10),
            path: "/qa/erase",
            title: "Erase me elsewhere",
            device: "mobile",
            country: "TR",
            source: None,
            referrer_host: None,
            duration_ms: None,
        },
    )
    .await;

    let goal = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/analytics/goals?site_id={site}"),
            Some(&manager),
            Some(json!({
                "name": "Erase walk",
                "kind": "pageview",
                "match": { "path": "/qa/erase" },
                "enabled": true,
                "steps": []
            })),
        ),
    )
    .await;
    assert_eq!(goal.status, StatusCode::CREATED, "body: {}", goal.body);
    let goal_id: Uuid = goal.body["id"].as_str().expect("a goal id").parse().expect("a uuid");
    sqlx::query(
        "insert into analytics_goal_hits (goal_id, visitor_hash, step_position, occurred_at) \
         values ($1, $2, 1, now())",
    )
    .bind(goal_id)
    .bind(&doomed)
    .execute(fixture.db.pool())
    .await
    .expect("the goal hit must be written");

    // A handle that is not a hash is a `400`, not an erasure of something else.
    let bad_handle = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/analytics/visitors/203.0.113.7?site_id={site}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(bad_handle.status, StatusCode::BAD_REQUEST, "body: {}", bad_handle.body);
    assert_eq!(bad_handle.body["error"]["code"], json!("invalid_visitor"));

    let refused = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/analytics/visitors/{doomed}?site_id={site}"),
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    let erased = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/analytics/visitors/{doomed}?site_id={site}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(erased.status, StatusCode::OK, "body: {}", erased.body);
    assert_eq!(erased.body["visitor"], json!(doomed));
    assert_eq!(erased.body["visits"], json!(1), "body: {}", erased.body);
    assert_eq!(erased.body["pageviews"], json!(2));
    assert_eq!(erased.body["events"], json!(1));
    assert_eq!(erased.body["goal_hits"], json!(1));
    assert_eq!(erased.body["rows_removed"], json!(5));

    assert_eq!(visits_for(&fixture.db, site, &doomed).await, 0, "every visit of the handle is gone");
    assert_eq!(goal_hits_for(&fixture.db, site, &doomed).await, 0, "and every goal hit");
    assert_eq!(
        visits_for(&fixture.db, other_site, &doomed).await,
        1,
        "the same handle on another site is another site's data"
    );

    // Erasing again removes nothing and still answers: an erasure that failed on a retry would
    // be a compliance problem of its own.
    let again = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/analytics/visitors/{doomed}?site_id={site}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.body["rows_removed"], json!(0));

    // Two runs, two audit rows; the erasure names the handle it erased.
    let audit = audit_rows(&fixture.db, site).await;
    assert_eq!(audit.len(), 3, "retention + two erasures: {audit:?}");
    assert_eq!(audit[1].0, "erasure");
    assert_eq!(audit[1].2, 5, "the first erasure removed the visitor's rows");
    assert!(audit[1].1.is_none(), "an erasure has no cutoff");
    assert_eq!(audit[2].2, 0, "the second erasure found nothing left");
    let recorded = event_payloads(&fixture.db, site, "analytics.erasure_completed").await;
    assert_eq!(recorded.len(), 2, "one event per erasure run");
    assert_eq!(recorded[0]["visitor"], json!(doomed));
    assert_eq!(recorded[0]["rows_removed"], json!(5));
    assert_eq!(recorded[1]["rows_removed"], json!(0));

    // And both facts reach a subscribed endpoint over HTTP — the delivery engine's own walk.
    let delivered = deliver_queued(&fixture).await;
    assert!(delivered.delivered >= 2, "two deliveries went out: {delivered:?}");
    let seen = receiver.events();
    assert!(
        seen.contains(&"analytics.retention_purged".to_owned()),
        "the purge reached the receiver: {seen:?}"
    );
    assert!(
        seen.contains(&"analytics.erasure_completed".to_owned()),
        "the erasure reached the receiver: {seen:?}"
    );
    let payload = receiver
        .payload("analytics.erasure_completed")
        .expect("the erasure payload");
    assert_eq!(payload["visitor"], json!(doomed));
    assert_eq!(payload["rows_removed"], json!(5));

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_spike_watch_sees_an_hour_above_three_times_its_trailing_median() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let now = OffsetDateTime::now_utc();
    let completed = omnion_module_analytics::privacy::hour_start(now) - time::Duration::hours(1);

    // A quiet week: one bucket a day at five visitors — a flat site, from the watch's point of
    // view. The hour that just closed ran forty.
    for offset in 1_i64..=7 {
        sqlx::query(
            "insert into analytics_hourly (site_id, bucket, metric, dimension_kind, \
             dimension_value, count) values ($1, $2, 'visitors', 'total', '', $3)",
        )
        .bind(site)
        .bind(completed - time::Duration::days(offset))
        .bind(5_i64)
        .execute(fixture.db.pool())
        .await
        .expect("the quiet bucket must be written");
    }
    sqlx::query(
        "insert into analytics_hourly (site_id, bucket, metric, dimension_kind, \
         dimension_value, count) values ($1, $2, 'visitors', 'total', '', $3)",
    )
    .bind(site)
    .bind(completed)
    .bind(40_i64)
    .execute(fixture.db.pool())
    .await
    .expect("the busy bucket must be written");

    let finding = omnion_module_analytics::privacy::detect_spike(fixture.db.pool(), site, now)
        .await
        .expect("the watch must run")
        .expect("forty against a median of five is a spike");
    assert_eq!(finding.hour, completed);
    assert_eq!(finding.visitors, 40);
    assert!((finding.median - 5.0).abs() < 0.001);
    assert!(finding.factor >= 8.0, "factor: {}", finding.factor);

    // The guard reads the events themselves: until the fact is recorded, the hour is news.
    assert!(
        !omnion_module_analytics::privacy::spike_recorded(fixture.db.pool(), site, completed)
            .await
            .expect("the guard must run")
    );

    bus::emit(
        fixture.db.pool(),
        NewEvent::new("analytics.traffic_spike")
            .organization(fixture.organizations[0])
            .site(site)
            .payload(json!({
                "hour": omnion_module_analytics::privacy::hour_label(completed),
                "visitors": finding.visitors,
                "median": finding.median,
                "factor": finding.factor,
            })),
    )
    .await
    .expect("the spike must be recorded");

    assert!(
        omnion_module_analytics::privacy::spike_recorded(fixture.db.pool(), site, completed)
            .await
            .expect("the guard must run"),
        "a recorded hour is not announced twice"
    );
    let recorded = event_payloads(&fixture.db, site, "analytics.traffic_spike").await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["visitors"], json!(40));

    // A quiet hour of the same site is not news, and neither is an empty one.
    assert!(
        omnion_module_analytics::privacy::detect_spike(
            fixture.db.pool(),
            site,
            completed + time::Duration::days(1) + time::Duration::hours(2),
        )
        .await
        .expect("the watch must run")
        .is_none(),
        "an hour nobody spiked is not a spike"
    );

    fixture.cleanup().await;
}
