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

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_module_analytics::rollup;
use omnion_module_analytics::settings as analytics_store;
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use serde_json::{Value, json};
use time::{Date, OffsetDateTime};
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
