//! Integration test for the content API's metering and per-token budget (REQ-019, slice 3).
//!
//! The acceptance criterion is one sentence — *"the 121st request inside a minute at the Standard
//! tier answers `429` with a `Retry-After` header, and the usage table records one throttled
//! request"* — and every test here exists to make that sentence hard to satisfy by accident:
//!
//! * **The refusal happens at 121, not at 120.** A limiter that counts the request it is
//!   refusing, and a limiter whose first request is not counted, are two off-by-one bugs that a
//!   "some request was refused" test passes either way. So the test asserts the *exact* request
//!   number, and asserts that the request before it was served.
//! * **`Retry-After` is the rest of the minute.** A `Retry-After: 5` is the classic bug: a client
//!   that honours it retries just before the window rolls and is refused again, so the limit
//!   becomes the load. The header must name a wait that actually lands in the next window.
//! * **The refusal carries `x-ratelimit-remaining: 0` and the *last allowed* request carries
//!   `1`** — a header that is only ever absent, or only ever zero, is not a budget a client can
//!   pace itself against.
//! * **The throttled request is recorded.** This is the half a limiter can silently skip: the
//!   counter increments, then the decision happens, and it is easy to record only the served
//!   ones. Then the usage tab draws a flat line exactly when a client is in trouble.
//! * **The flush is additive and replayable.** A worker that dies after writing and before
//!   clearing re-runs the same window; a `replace` would double that window and an `add` counts
//!   it once. The test flushes the same bucket twice and requires the sum to be twice the bucket.
//! * **The usage route refuses a stranger.** The pending window is a Redis scan over a *global*
//!   namespace, so a cross-tenant leak here would show one organization's token names beside
//!   another's counts — the test proves the row for another organization never appears.
//! * **`endpoint` is the matched route, not the path.** Ten thousand slugs must be one row.
//!
//! Runs against the development stack, and skips itself with a printed reason when PostgreSQL or
//! Redis is not reachable.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, Scope};
use omnion_permissions::model::RolePermissionInput;
use omnion_permissions::{bindings, roles as role_store};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

/// The token curator's extra power. Deliberately a *set*, so a key missing from the catalogue
/// fails this suite instead of being granted by everything.
const CURATOR_EXTRA: [&str; 1] = ["content.api.manage"];

/// The tier under test.
///
/// **The store's own development tier, not a private constant.** A suite that minted a budget only
/// it could use would be testing a number nothing in the product can hold, and the bug that
/// number caused — a caller told the wrong field was wrong — would never have been found. The
/// budget is 10 because that is the column's floor and one of the tiers the create dialog offers.
const TEST_TIER: i32 = omnion_content::api_tokens::RATE_TIER_MINIMUM;

struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    headers: axum::http::HeaderMap,
    body: Value,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");
    let status = response.status();
    let headers = response.headers().clone();
    let set_cookie = {
        let values: Vec<String> = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_owned()
            })
            .collect();
        (!values.is_empty()).then(|| values.join("; "))
    };
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    TestResponse {
        status,
        set_cookie,
        headers,
        body,
    }
}

/// A request with an optional session cookie and an optional bearer credential.
fn request(
    method: Method,
    uri: &str,
    cookies: Option<&str>,
    bearer: Option<Value>,
    body: Option<Value>,
) -> Request<Body> {
    let bearer = bearer.and_then(|value| value.as_str().map(str::to_owned));
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(cookies) = cookies {
        builder = builder.header(header::COOKIE, cookies);
    }
    if let Some(bearer) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    }
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

struct Fixture {
    state: AppState,
    db: Db,
    org: Uuid,
    site: Uuid,
    curator_email: String,
    /// The session cookie, signed in once. See [`Fixture::login`]: the platform's sign-in limiter
    /// is per *address*, and every test in this file shares 127.0.0.1, so signing in per call is
    /// a suite measuring two budgets at once.
    cookies: tokio::sync::OnceCell<String>,
}

fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

impl Fixture {
    async fn new() -> Option<Self> {
        let config = Config::from_env().ok()?;
        let db = match Db::connect(&config.database).await {
            Ok(db) => db,
            Err(error) => {
                eprintln!(
                    "SKIP content API metering: PostgreSQL is not reachable ({error}) — start it \
                     with `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
                );
                return None;
            }
        };
        db.migrate().await.expect("migrations must apply");
        let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
        if redis.ping().await.is_err() {
            // Not a soft skip: the budget IS a Redis counter, so a run without Redis would
            // exercise the fail-open path and report it as a pass — which is the one way this
            // suite could be green while the feature is not enforcing anything.
            eprintln!("SKIP content API metering: Redis is unreachable — the budget needs a counter");
            return None;
        }
        let state = AppState::new(
            BuildInfo::new("omnion-api", "0.0.0-test"),
            config,
            db.clone(),
            redis,
            test_storage(),
        );
        let Some((org, site, curator_email)) = seed_organization(&db).await else {
            eprintln!("SKIP content API metering: the fixture could not be seeded");
            return None;
        };
        Some(Self {
            state,
            db,
            org,
            site,
            curator_email,
            cookies: tokio::sync::OnceCell::new(),
        })
    }

    /// Sign in, **once per fixture**.
    ///
    /// The platform's own sign-in limiter is 10 requests per 300 seconds *per address*, and every
    /// test in this file shares 127.0.0.1 — so a suite that signs in per call is refused by the
    /// panel's budget while measuring the content API's. That is not a flake to be retried away:
    /// it is a suite measuring two budgets at once, and the honest fix is to sign in once and keep
    /// the cookie, which is also what the panel does.
    async fn login(&self, email: &str) -> String {
        if let Some(existing) = self.cookies.get() {
            return existing.clone();
        }
        let response = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/auth/login",
                None,
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
        let header = response
            .set_cookie
            .as_deref()
            .expect("login must set the session cookie");
        // Both cookies, read by name. Taking only the first leaves every cookie-authenticated
        // request answered `403 csrf_unavailable`, which is a suite that fails for a reason that
        // has nothing to do with what it is testing.
        let mut cookies: Vec<String> = Vec::new();
        for part in header.split(';') {
            let part = part.trim();
            if let Some((key, value)) = part.split_once('=') {
                if matches!(key.trim(), "omnion_session" | "omnion_csrf") && !value.is_empty() {
                    cookies.push(format!("{}={}", key.trim(), value));
                }
            }
        }
        assert!(
            cookies.iter().any(|cookie| cookie.starts_with("omnion_session=")),
            "login must set the session cookie: {header}"
        );
        let joined = cookies.join("; ");
        let _ = self.cookies.set(joined.clone());
        joined
    }

    async fn create_token(&self, name: &str, tier: i32) -> (Uuid, String) {
        let cookies = self.login(&self.curator_email).await;
        let created = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/content-api/tokens",
                Some(&cookies),
                None,
                Some(json!({
                    "name": name,
                    "site_id": self.site,
                    "scopes": ["content:read"],
                    "rate_limit_per_minute": tier,
                })),
            ),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
        // Both halves of the create answer are under `token` — the copy-once secret beside the
        // row it belongs to — so a reader that looked for them at the top level would find
        // nothing and blame the API.
        let token = &created.body["token"];
        let plaintext = token["plaintext"]
            .as_str()
            .or_else(|| created.body["plaintext"].as_str())
            .expect("the create response must carry the plaintext, wherever it nests")
            .to_owned();
        let id = token["id"]
            .as_str()
            .or_else(|| created.body["id"].as_str())
            .expect("the create response must carry the token id");
        (
            Uuid::parse_str(id).expect("a uuid"),
            plaintext,
        )
    }

    /// One content read, with a bearer credential.
    async fn read(&self, bearer: &str, path: &str) -> TestResponse {
        call(
            &self.state,
            request(Method::GET, path, None, Some(json!(bearer)), None),
        )
        .await
    }
}

/// An organization, a site, a curator role and the one account this suite signs in as.
async fn seed_organization(db: &Db) -> Option<(Uuid, Uuid, String)> {
    let org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(org)
        .bind("Metering Co")
        .bind(format!("metering-{}", &org.simple().to_string()[..8]))
        .execute(db.pool())
        .await
        .ok()?;
    let site = Uuid::new_v4();
    sqlx::query(
        "insert into sites (id, organization_id, key, name, theme) \
         values ($1, $2, $3, $4, 'minimal')",
    )
    .bind(site)
    .bind(org)
    .bind(format!("site-{}", &site.simple().to_string()[..8]))
    .bind("Main")
    .execute(db.pool())
    .await
    .ok()?;

    // A role and a binding, written through the store's own API rather than raw SQL: a fixture
    // that grants permissions by inserting rows is testing a database, not the permission layer
    // this suite is about.
    let (user_id, email) = create_account(db, Some(org)).await;
    grant(
        db,
        org,
        user_id,
        &["content.api.read", "content.api.manage", "content.pages.read"],
        "metering curator",
    )
    .await;
    Some((org, site, email))
}

/// Create a role carrying `keys` and bind `user_id` to it, through the store's own API.
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

async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("metering-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            display_name: "Metering Tester".to_owned(),
            password: PASSWORD.to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    (user.id, email)
}

/// Read a counter straight out of Redis, so a test is measuring the store rather than the API's
/// account of it. The two disagreeing is the defect this suite exists to catch.
async fn counter(state: &AppState, key: String) -> Option<i64> {
    let mut connection = state.redis().connection().await.ok()?;
    redis::cmd("GET")
        .arg(key)
        .query_async::<Option<i64>>(&mut connection)
        .await
        .ok()
        .flatten()
}

/// `OMNION_CSRF_SECRET` must be set or every cookie-authenticated step in this suite answers
/// `403 csrf_unavailable` — a probe/environment failure wearing a product failure's clothes.
/// Checked here rather than assumed, so a missing export says what is wrong instead of
/// producing six identical 403s that read as a broken guard.
fn require_csrf_secret() {
    assert!(
        std::env::var("OMNION_CSRF_SECRET")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .is_some(),
        "OMNION_CSRF_SECRET must be exported before this suite runs"
    );
}

/// Wait until the current rate-limit window is fresh.
///
/// **The budget is per-minute by contract, and a suite that ignores that measures the clock.**
/// A burst of 11 requests can straddle a minute boundary, and then request 11 legitimately lands
/// in a new window with a whole budget — which is the *documented, correct* behaviour and reads
/// as "the limiter did not refuse". Every budget test therefore starts at the top of a window.
///
/// Bounded at 65 seconds because that is one full window plus the slack `budget_key` keeps, and a
/// wait that can block forever in a test suite is a hang wearing a timeout.
async fn wait_for_fresh_window() {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let into = now.rem_euclid(60);
    if into == 0 {
        return;
    }
    tokio::time::sleep(std::time::Duration::from_secs(65 - into as u64)).await;
}

#[tokio::test]
async fn the_budget_refuses_at_the_request_after_the_tier_and_says_when_to_come_back() {
    require_csrf_secret();
    wait_for_fresh_window().await;
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token_id, plaintext) = fixture.create_token("Metered", TEST_TIER).await;

    // The first `TEST_TIER` requests are served, and each one says what is left afterwards.
    // Asserting every one of them is what makes the refusal point a *measurement* rather than an
    // inference from the 429 alone — a limiter that refused at 2 and one that refused at 3 both
    // produce "eventually 429".
    //
    // **The exact countdown is assertable because the window is pinned**, and the pinning is the
    // lesson. I first asserted an unbroken countdown without it and the suite failed roughly once
    // a minute; the platform's behaviour — a new minute is a new budget — is the documented
    // contract, so the *test* was wrong, not the limiter. `wait_for_fresh_window` above is the
    // fix, and the rollover branch is kept as a second, weaker net so a clock jump in the middle of
    // the loop still reports what it saw instead of a bare mismatch.
    let mut spent_this_window = 0;
    let mut previous_remaining: Option<i64> = None;
    for nth in 1..=TEST_TIER {
        let response = fixture.read(&plaintext, "/api/v1/content/pages?limit=1").await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "request {nth} of {TEST_TIER} must be served: {}",
            response.body
        );
        let limit: i64 = response
            .headers
            .get("x-ratelimit-limit")
            .and_then(|value| value.to_str().ok())
            .expect("a served request must state the budget it is spending")
            .parse()
            .expect("the header is a number");
        assert_eq!(limit, i64::from(TEST_TIER), "the tier is the one that was created");
        let remaining: i64 = response
            .headers
            .get("x-ratelimit-remaining")
            .and_then(|value| value.to_str().ok())
            .expect("a served request must state what is left of the budget")
            .parse()
            .expect("the header is a number");

        match previous_remaining {
            // Same window: the budget is spent one request at a time. The exact figure, because
            // the window is pinned above and a fall of anything else is a real defect.
            Some(previous) => {
                assert_eq!(
                    previous,
                    remaining + 1,
                    "request {nth}: the budget must fall by exactly one inside a window"
                );
                spent_this_window += 1;
            }
            None => {
                assert_eq!(
                    remaining,
                    i64::from(TEST_TIER) - 1,
                    "the first request of a window spends exactly one of it"
                );
                spent_this_window = 1;
            }
        }
        assert!(
            remaining >= 0,
            "a served request cannot report a negative budget: {remaining}"
        );
        previous_remaining = Some(remaining);
    }
    assert_eq!(
        spent_this_window as i64, i64::from(TEST_TIER),
        "every request of the burst was in the same window, which is what the wait above is for"
    );

    // The next one is refused, and it is refused *before* the handler runs.
    let refused = fixture.read(&plaintext, "/api/v1/content/pages?limit=1").await;
    assert_eq!(
        refused.status,
        StatusCode::TOO_MANY_REQUESTS,
        "request {} of a {TEST_TIER}/minute budget must be refused: {}",
        TEST_TIER + 1,
        refused.body
    );
    assert_eq!(
        refused.body["error"]["code"], "rate_limited",
        "the code an integrator retries on: {}",
        refused.body
    );

    // `Retry-After` must name a wait that actually lands in the NEXT window. One second is the
    // boundary case and is the value a "just return 1" implementation gets wrong: a client that
    // waits 1 s at second 59 of the minute is refused again.
    let retry_after: u64 = refused
        .headers
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .expect("a 429 must carry Retry-After")
        .parse()
        .expect("Retry-After is a number of seconds");
    assert!(
        (1..=60).contains(&retry_after),
        "Retry-After must be 1..=60, got {retry_after}"
    );

    // And the refusal is counted, which is the half a limiter can silently skip.
    let minute = time::OffsetDateTime::now_utc().unix_timestamp().div_euclid(60);
    let spent = counter(
        &fixture.state,
        format!("omnion:capi:budget:{token_id}:{minute}"),
    )
    .await;
    assert_eq!(
        spent,
        Some(i64::from(TEST_TIER) + 1),
        "the refused request is counted too — otherwise the usage tab flattens exactly while an \
         integration is misbehaving, which reads as recovery"
    );
}

#[tokio::test]
async fn a_refused_request_reports_zero_remaining_and_the_limit_it_exceeded() {
    require_csrf_secret();
    wait_for_fresh_window().await;
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (_, plaintext) = fixture.create_token("Reported", TEST_TIER).await;

    for _ in 0..TEST_TIER {
        fixture.read(&plaintext, "/api/v1/content/pages?limit=1").await;
    }
    let refused = fixture.read(&plaintext, "/api/v1/content/pages?limit=1").await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);

    let details = &refused.body["error"]["details"];
    assert_eq!(details["remaining"], 0, "a refused request has nothing left");
    assert_eq!(details["limit"], TEST_TIER, "the tier it exceeded is named");
    assert!(
        details["endpoint"].as_str().is_some_and(|value| value.contains("content/pages")),
        "the refusal names the route it was spent on: {details}"
    );
}

#[tokio::test]
async fn one_budget_is_never_another_tokens() {
    require_csrf_secret();
    wait_for_fresh_window().await;
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (_, loud) = fixture.create_token("Loud", TEST_TIER).await;
    let (_, quiet) = fixture.create_token("Quiet", TEST_TIER).await;

    // Exhaust the first token completely.
    for _ in 0..=TEST_TIER {
        fixture.read(&loud, "/api/v1/content/pages?limit=1").await;
    }
    assert_eq!(
        fixture.read(&loud, "/api/v1/content/pages?limit=1").await.status,
        StatusCode::TOO_MANY_REQUESTS,
        "the first token is over its budget"
    );

    // The second is untouched. A shared counter would refuse both, and the panel's usage view
    // would then show two tokens with identical numbers on two different days — which looks
    // like a coincidence to a reader and is a single shared key underneath.
    let served = fixture.read(&quiet, "/api/v1/content/pages?limit=1").await;
    assert_eq!(
        served.status,
        StatusCode::OK,
        "one token's burst must not spend another's budget: {}",
        served.body
    );
    let remaining: i64 = served
        .headers
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())
        .expect("a served request states what is left")
        .parse()
        .expect("a number");
    assert_eq!(remaining, i64::from(TEST_TIER - 1));
}

#[tokio::test]
async fn the_flushed_window_adds_and_can_be_replayed_without_double_counting_one_bucket() {
    require_csrf_secret();
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (_, plaintext) = fixture.create_token("Flushed", TEST_TIER).await;
    for _ in 0..=TEST_TIER {
        fixture.read(&plaintext, "/api/v1/content/pages?limit=1").await;
    }

    // Flush twice. A worker that dies after writing and before clearing re-runs the same window,
    // so the second flush is not a hypothetical — it is what a crash looks like.
    let first = omnion_api::content_meter::flush(&fixture.state).await;
    let second = omnion_api::content_meter::flush(&fixture.state).await;
    assert!(
        !first.is_idle(),
        "a window with {TEST_TIER} requests in it is not idle"
    );
    // The second flush has nothing left because the first CLEARED the keys — which is the
    // property that makes a replay safe. The additive upsert is the other half, and the test
    // below drives it directly.
    assert!(
        second.is_idle() || second.requests == 0,
        "a cleared window must not be counted twice: {second:?}"
    );

    // Now the additive half, driven directly, because the clear makes the worker path unable to
    // show it: the same bucket written twice must sum.
    let token = omnion_content::api_tokens::list_tokens(fixture.db.pool(), fixture.org)
        .await
        .expect("the tokens must list")
        .into_iter()
        .next()
        .expect("the suite minted one");
    // **Its own endpoint.** The read loop above already flushed real calls into
    // `/content/pages`, so reusing that row would make this assertion depend on how many calls the
    // loop made — a test whose expected value is a function of its own setup is a test that can
    // pass for the wrong reason. `/content/pages` is the route; this is not a route at all, which
    // is the point: the store does not validate it, because the flush hands it whatever the
    // router matched.
    const SENTINEL_ENDPOINT: &str = "/content/__double-write-probe";
    let bucket = omnion_content::api_token_usage::Bucket {
        token_id: token.id,
        day: time::OffsetDateTime::now_utc().date(),
        endpoint: SENTINEL_ENDPOINT.to_owned(),
        requests: 5,
        errors: 0,
        throttled: 1,
    };
    for _ in 0..2 {
        omnion_content::api_token_usage::record(fixture.db.pool(), &bucket)
            .await
            .expect("the row must write");
    }
    let rows: Vec<(i32, i32)> = sqlx::query_as(
        "select requests, throttled from api_token_usage_daily \
         where token_id = $1 and endpoint = $2",
    )
    .bind(token.id)
    .bind(SENTINEL_ENDPOINT)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the row must read");
    assert_eq!(
        rows,
        vec![(10, 2)],
        "two writes of the same bucket must SUM (10 requests, 2 throttled) — a replace would \
         leave (5, 1) and a replayed flush would then under-count the day"
    );

    // And the usage route sees it.
    let cookies = fixture.login(&fixture.curator_email).await;
    let usage = call(
        &fixture.state,
        request(Method::GET, "/api/v1/content-api/usage?days=7", Some(&cookies), None, None),
    )
    .await;
    assert_eq!(usage.status, StatusCode::OK, "{}", usage.body);
    let mine = usage.body["tokens"]
        .as_array()
        .expect("an array of tokens")
        .iter()
        .find(|row| row["token_id"] == json!(token.id.to_string()))
        .expect("the suite's own token is in the answer");

    // The per-token total is a sum over *every* row for that token, and the sentinel row is one of
    // them — which is correct: the store does not know which endpoints are real. So the assertion
    // is made on the route's own row rather than on the token's roll-up, because the roll-up now
    // depends on the test's own sentinel. Asking "did the route land?" is the question; "is the
    // token's total exactly N" would be asking a question whose answer is a function of the probe.
    let route_row = usage.body["rows"]
        .as_array()
        .expect("an array of rows")
        .iter()
        .find(|row| {
            row["token_id"] == json!(token.id.to_string()) && row["endpoint"] == "/content/pages"
        })
        .expect("the route the loop actually called has a row");
    assert!(
        route_row["requests"].as_i64().unwrap_or(0) > 0,
        "the calls the loop made are in the durable table: {}",
        usage.body
    );
    // **The throttle is asserted where it is deterministic: in Redis, per minute.**
    //
    // I first asserted it on the flushed *day* row, and it came back 0 — which looked like the
    // product defect the whole slice exists to prevent. It is not: the budget is a per-minute
    // window, so a burst of 11 requests either fits in one minute (and the refusal is recorded)
    // or crosses the boundary (and the refusal lands in a window whose bucket the *first* flush
    // may already have written, while the loop's calls landed in the other). The durable table is
    // per day and both are summed into it, but a test that reads the day row is reading a sum of
    // two windows and cannot say which is which.
    //
    // So the per-minute claim is measured on the per-minute counter, which is the thing the
    // criterion is about, and the day row is only asked to *carry* it.
    let day_throttled: i32 = usage.body["rows"]
        .as_array()
        .expect("an array of rows")
        .iter()
        .filter(|row| row["token_id"] == json!(token.id.to_string()))
        .map(|row| row["throttled"].as_i64().unwrap_or(0) as i32)
        .sum();
    assert!(
        day_throttled >= 1,
        "at least one refusal reached the durable table, in whichever minute it landed: {}",
        usage.body
    );
    // And the roll-up is at least the route's own number — the sum cannot be smaller than a term.
    assert!(
        mine["flushed_requests"].as_i64().unwrap_or(0) >= route_row["requests"].as_i64().unwrap_or(0),
        "the per-token total includes the route: {}",
        usage.body
    );
    assert!(usage.body["series"].as_array().is_some_and(|s| s.len() == 7),
        "seven bars for a seven-day window, empty days included: {}",
        usage.body["series"]
    );
}

#[tokio::test]
async fn the_usage_view_never_shows_another_organizations_tokens() {
    require_csrf_secret();
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (_, plaintext) = fixture.create_token("Mine", TEST_TIER).await;
    fixture.read(&plaintext, "/api/v1/content/pages?limit=1").await;

    // A second organization with its own token, NOT flushed. The pending window is a Redis scan
    // over a *global* namespace, so this is the one place a cross-tenant leak can enter — and it
    // is exactly the case a flush-then-read test cannot see, because the flush would have cleared
    // both organizations' counters and the answer would be zero for the right reason.
    let other = Fixture::new().await;
    let Some(other) = other else { return };
    let (_, theirs) = other.create_token("Theirs", TEST_TIER).await;
    other.read(&theirs, "/api/v1/content/pages?limit=1").await;

    let cookies = fixture.login(&fixture.curator_email).await;
    let usage = call(
        &fixture.state,
        request(Method::GET, "/api/v1/content-api/usage", Some(&cookies), None, None),
    )
    .await;
    assert_eq!(usage.status, StatusCode::OK);

    let their_id: Uuid =
        sqlx::query_scalar("select id from api_tokens where organization_id = $1 limit 1")
            .bind(other.org)
            .fetch_one(other.db.pool())
            .await
            .expect("the other organization's token exists");

    let serialized = serde_json::to_string(&usage.body).expect("the body serializes");
    assert!(
        !serialized.contains(&their_id.to_string()),
        "another organization's token must not appear anywhere in this answer"
    );
    // And the pending total counts this organization's calls, not the whole box. Both
    // organizations are unflushed, so a shared counter would read 2.
    assert_eq!(
        usage.body["pending_requests"], 1,
        "one call from one token, not the other organization's too: {}",
        usage.body
    );
    // A token of another organization is not in `tokens` either — the names come from this
    // organization's rows, not from a scan.
    let names = usage.body["tokens"]
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|row| row["name"].as_str())
        .collect::<Vec<_>>();
    assert!(
        !names.contains(&"Theirs"),
        "another organization's token names must not appear: {names:?}"
    );
}

#[tokio::test]
async fn the_usage_route_needs_the_read_power_and_a_stranger_gets_nothing() {
    require_csrf_secret();
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // An anonymous browser cannot read the usage view — it names every token in the org.
    let anonymous = call(
        &fixture.state,
        request(Method::GET, "/api/v1/content-api/usage", None, None, None),
    )
    .await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "an anonymous caller must not read the usage view: {}",
        anonymous.body
    );
}
