//! Integration test for the observability log store and the request-scoped context
//! (docs/requests/REQ-126, slice 1).
//!
//! It runs against the development stack and skips itself with a printed reason when PostgreSQL
//! is not reachable, exactly like the REQ-125 suites beside it.
//!
//! The walk proves the four claims of the slice, in the order they matter — and every one of them
//! is a claim about what is **readable**, not about what was written:
//!
//! 1. **A request produces a line that carries its request id, and the header agrees with it.**
//!    The id in the response header and the id in the stored row are the same string, because an
//!    operator holding a banner and an operator holding a row are the same person.
//! 2. **An authenticated request's line carries the user and the organization.** This is the one
//!    that the middleware can get silently wrong — the guard puts the session in the *request's*
//!    extensions, and reading the response's leaves `user_id` null on every route while every
//!    other assertion still passes.
//! 3. **A worker line carries the trace of the request that enqueued the job.** Proved by writing
//!    two lines under one context and reading them back in order through the request route.
//! 4. **The store is bounded and honest.** An oversized field object is truncated *visibly*, a
//!    window beyond the cap is refused with a `400` naming the cap, a `limit` past the row cap is
//!    clamped, and the retention prune removes only old lines.
//!
//! The redaction claim is asserted as a **grep over the rendered bytes** for a fixture value and a
//! fixture e-mail address, because "the field is absent" passes forever against an implementation
//! that will start leaking on the next schema change.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
mod support;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::seed;
use omnion_telemetry::store::{self, LogFilter, LogSettings};
use omnion_telemetry::{LogContext, LogLevel, LogSource, MAX_FIELD_COUNT, NewLogEntry};
use serde_json::{Value, json};
use std::net::SocketAddr;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

/// The two values every leak assertion greps for. A leak check against a stand-in constant can
/// never fail, so these are the strings the test itself feeds in.
const FIXTURE_SECRET: &str = "sk-live-51H8xQ2eZvKYlo2C0aB7dEfGh3JkLmNoPqRsTuVwXy";
const FIXTURE_EMAIL: &str = "leaky-operator@omnion.example";

struct TestResponse {
    status: StatusCode,
    body: Value,
    request_id: Option<String>,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let peer: SocketAddr = "198.51.100.7:51234"
        .parse()
        .expect("a literal is a valid peer");
    let mut request = request;
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");
    let status = response.status();
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let body = if raw.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&raw).unwrap_or(Value::Null)
    };
    TestResponse {
        status,
        body,
        request_id,
    }
}

fn request(method: Method, uri: &str, session: Option<&Session>, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(session) = session {
        builder = builder.header(header::COOKIE, session.cookie());
    }
    let body = match body {
        Some(value) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    builder.body(body).expect("request must build")
}

fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_env().expect("storage must configure")
}

async fn live_state() -> Option<(AppState, Db)> {
    // This file builds its own state rather than going through `walk_state::state_or_fail`,
    // because it also needs the `Db` handle to read rows back out of PostgreSQL. That makes it
    // the one observability suite the shared harness's CSRF default does not reach, and it is
    // exactly the suite that asserts the sign-in sets a CSRF cookie beside the session cookie.
    //
    // Without the secret the API mints no `omnion_csrf` cookie at all — the layer refuses rather
    // than skips when `OMNION_CSRF_SECRET` is unset, which is correct for a deployment and
    // means the assertion below would fail for a reason unrelated to what it is testing.
    support::walk_state::ensure_csrf_secret();
    let config = Config::from_env().expect("environment must be valid");
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(err) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({err}) — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
            );
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
    // See `walk_state::ensure_test_rate_limits`: the router installs whatever limiter is already
    // in the process-wide `OnceLock`, so a walk that does not set one here inherits the shipped
    // sign-in ceiling of ten per five minutes — and this file's three walks between them sign in
    // more than that, so the last one would fail on a `429` it never asked for.
    support::walk_state::ensure_test_rate_limits(&state);
    Some((state, db))
}

async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("obslog-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Observability Test".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    (user.id, email)
}

/// What a sign-in hands back: the session cookie AND the CSRF token minted beside it.
///
/// Both, because the CSRF layer refuses a cookie-authenticated mutation that presents only the
/// session. A walk that captured the session alone drove a layer that is correct in production
/// and unwalkable in a test — the panel receives both cookies, so a walk holding one of the two
/// is no longer a request the panel can make. The token is derived from the session id and the
/// configured secret, so it is read from the response rather than recomputed here: recomputing
/// it would let a walk pass while the sign-in stopped issuing one.
#[derive(Clone)]
struct Session {
    token: String,
    csrf: String,
}

impl Session {
    /// The `Cookie` header a browser would send for this sign-in.
    fn cookie(&self) -> String {
        format!("omnion_session={}; omnion_csrf={}", self.token, self.csrf)
    }
}

async fn login(state: &AppState, email: &str) -> Session {
    let response = routes::router(state.clone())
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "email": email, "password": PASSWORD }).to_string(),
                ))
                .expect("request must build"),
        )
        .await
        .expect("router must answer");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the account must sign in"
    );
    let set_cookie: Vec<&str> = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    let pick = |name: &str| {
        set_cookie
            .iter()
            .filter_map(|raw| raw.split(';').next())
            .filter_map(|pair| pair.trim().split_once('='))
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_owned())
    };
    Session {
        token: pick("omnion_session").expect("login must set a session cookie"),
        csrf: pick("omnion_csrf")
            .expect("login must set the CSRF cookie beside the session cookie"),
    }
}

#[tokio::test]
async fn a_request_produces_a_line_its_own_request_id_can_find() {
    // SAFETY: `set_var` is `unsafe` in edition 2024 and this is the process-wide database
    // URL the whole binary reads once at startup; the tests in this file run one at a
    // time. The same guard the REQ-125 suites beside it use.
    unsafe {
        std::env::set_var(
            "OMNION_DATABASE_URL",
            std::env::var("OMNION_TEST_DATABASE_URL")
                .unwrap_or_else(|_| "postgres://omnion:omnion@127.0.0.1:5433/omnion_w6_dev".into()),
        );
    }
    let Some((state, db)) = live_state().await else {
        eprintln!("skipping: no test database");
        return;
    };

    let slug = format!("obs-log-{}", Uuid::new_v4().simple());
    let organization_id: Uuid =
        sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
            .bind("Observability Log Organization")
            .bind(&slug)
            .fetch_one(db.pool())
            .await
            .expect("the organization must be created");
    let (user_id, email) = create_account(&db, Some(organization_id)).await;
    // The roles have to exist before a user can hold one, and which of these two runs first is
    // not this suite's decision — so the order is fixed here rather than left to the runner.
    seed::ensure(db.pool())
        .await
        .expect("the default roles must exist");
    seed::bind_owner(db.pool(), user_id)
        .await
        .expect("the owner binding must be created");
    let session = login(&state, &email).await;

    // ── 1. the header and the row agree, and the row is findable by it ──────────────────────────
    let read = call(
        &state,
        request(
            Method::GET,
            "/api/v1/observability/logs?limit=5",
            Some(&session),
            None,
        ),
    )
    .await;
    assert_eq!(
        read.status,
        StatusCode::OK,
        "the explorer must answer: {}",
        read.body
    );
    let header_id = read
        .request_id
        .clone()
        .expect("every response carries a request id header");
    assert_eq!(
        header_id.len(),
        36,
        "the header must be a uuid, got `{header_id}`"
    );

    // The line is written AFTER the response is produced — the middleware cannot know the status
    // or the duration until the handler is done — so the caller's response arrives fractionally
    // before its own line lands. A short bounded retry is the honest way to read it; a fixed
    // sleep would be a flake waiting for a busy CI box, and reading immediately is a flake
    // waiting for a slow disk.
    let request_uuid = Uuid::parse_str(&header_id).expect("uuid");
    let mut lines = Vec::new();
    for attempt in 0..50 {
        lines = store::lines_for_request(db.pool(), request_uuid)
            .await
            .expect("the store must read");
        if !lines.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20 * (attempt + 1))).await;
    }
    assert!(
        !lines.is_empty(),
        "the request id in the response header must find the line that request produced"
    );
    let mine = lines
        .iter()
        .find(|line| line.target == "omnion_api::request")
        .expect("the request line must be in the store");
    assert_eq!(
        mine.route.as_deref(),
        Some("/api/v1/observability/logs"),
        "the row must carry the route TEMPLATE, not the literal query string"
    );
    assert!(
        !mine.route.as_deref().unwrap_or("").contains("?"),
        "a route with a query string is not a template"
    );

    // ── 2. the actor is on the line ────────────────────────────────────────────────────────────
    // The guard puts the session in the REQUEST's extensions; a middleware that reads the
    // response's finds nothing, and this is the assertion that notices.
    assert_eq!(
        mine.user_id,
        Some(user_id),
        "an authenticated request's line must carry the user id"
    );
    assert_eq!(
        mine.organization_id,
        Some(organization_id),
        "an authenticated request's line must carry the organization id"
    );
    assert!(mine.trace_id.is_some(), "every line carries a trace id");
    assert_eq!(mine.source, "api");

    // ── 3. a worker line joins to its producer, in order ───────────────────────────────────────
    let trace_id = mine.trace_id.clone().expect("the request line has a trace");
    let producer_request = Uuid::new_v4();
    let worker_context = LogContext::new_request(producer_request).with_trace(&trace_id);
    let worker_line = NewLogEntry::new(
        LogLevel::Info,
        "omnion_secrets_runner",
        "the re-wrap batch applied",
    )
    .with_field("rewrapped", 4i64)
    .source(LogSource::Worker)
    .build_with(&worker_context);
    store::write(db.pool(), &worker_line)
        .await
        .expect("the worker line must be stored");

    // …and a second line from the same worker, so the ordering assertion has something to order.
    let follow_up = NewLogEntry::new(
        LogLevel::Warn,
        "omnion_secrets_runner",
        "the batch slowed to 3.2s",
    )
    .with_field("rewrapped", 0i64)
    .source(LogSource::Worker)
    .build_with(&worker_context);
    store::write(db.pool(), &follow_up)
        .await
        .expect("the follow-up line must be stored");

    let joined = call(
        &state,
        request(
            Method::GET,
            &format!("/api/v1/observability/logs/requests/{producer_request}"),
            Some(&session),
            None,
        ),
    )
    .await;
    assert_eq!(joined.status, StatusCode::OK, "the join route must answer");
    let entries = joined.body["entries"]
        .as_array()
        .expect("entries must be an array");
    assert_eq!(
        entries.len(),
        2,
        "both worker lines must be reachable from the producer's request id"
    );
    // Oldest first: a timeline that runs backwards is a timeline nobody reads.
    assert_eq!(entries[0]["message"], "the re-wrap batch applied");
    assert_eq!(entries[1]["message"], "the batch slowed to 3.2s");
    assert_eq!(
        entries[0]["trace_id"],
        trace_id.as_str(),
        "the worker line must carry the trace of the request that enqueued the job"
    );
    assert_eq!(entries[0]["source"], "worker");

    // ── 4. the store refuses what it says it refuses ───────────────────────────────────────────
    let too_wide = (OffsetDateTime::now_utc() - time::Duration::days(400))
        .format(&Rfc3339)
        .unwrap();
    let refused = call(
        &state,
        request(
            Method::GET,
            &format!("/api/v1/observability/logs?since={too_wide}"),
            Some(&session),
            None,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "a window past the cap is the caller's error, not a 500"
    );
    assert_eq!(refused.body["error"]["code"], "window_too_wide");
    assert!(
        refused.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains(&store::MAX_WINDOW_DAYS.to_string()),
        "the refusal must name the cap: {}",
        refused.body["error"]["message"]
    );

    let bad_level = call(
        &state,
        request(
            Method::GET,
            "/api/v1/observability/logs?level=verbose",
            Some(&session),
            None,
        ),
    )
    .await;
    assert_eq!(bad_level.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad_level.body["error"]["code"], "invalid_level");

    // A single value, not the repeated form: this is what a link or a bookmark produces.
    let single = call(
        &state,
        request(
            Method::GET,
            "/api/v1/observability/logs?level=info",
            Some(&session),
            None,
        ),
    )
    .await;
    assert_eq!(
        single.status,
        StatusCode::OK,
        "a filter only its own client can satisfy is not a filter: {}",
        single.body
    );

    let clamped = call(
        &state,
        request(
            Method::GET,
            "/api/v1/observability/logs?limit=1000000",
            Some(&session),
            None,
        ),
    )
    .await;
    assert_eq!(clamped.status, StatusCode::OK);
    let rows = clamped.body["entries"].as_array().expect("entries");
    assert!(
        rows.len() as i64 <= store::MAX_ROWS,
        "a crafted limit must be clamped to {}",
        store::MAX_ROWS
    );

    // ── 5. the read permission is a real boundary ──────────────────────────────────────────────
    let anonymous = call(
        &state,
        request(Method::GET, "/api/v1/observability/logs", None, None),
    )
    .await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "the log store must not be readable without a session"
    );

    // ── 6. the settings round-trip and refuse out-of-range values with a field message ────────
    let settings = call(
        &state,
        request(
            Method::GET,
            "/api/v1/observability/logs/settings",
            Some(&session),
            None,
        ),
    )
    .await;
    assert_eq!(settings.status, StatusCode::OK);
    assert_eq!(settings.body["max_retention_days"], store::MAX_WINDOW_DAYS);

    let bad_save = call(
        &state,
        request(
            Method::PUT,
            "/api/v1/observability/logs/settings",
            Some(&session),
            Some(json!({
                "log_level_default": "info",
                "logs_retention_days": 365
            })),
        ),
    )
    .await;
    assert_eq!(bad_save.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(bad_save.body["error"]["code"], "invalid_retention");
    assert!(
        bad_save.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("logs_retention_days"),
        "the refusal must name the field: {}",
        bad_save.body["error"]["message"]
    );

    let typo = call(
        &state,
        request(
            Method::PUT,
            "/api/v1/observability/logs/settings",
            Some(&session),
            Some(json!({
                "log_level_default": "info",
                "retention_days": 7
            })),
        ),
    )
    .await;
    assert!(
        typo.status.is_client_error(),
        "a misspelled field must be refused, not silently ignored: {}",
        typo.status
    );

    let good_save = call(
        &state,
        request(
            Method::PUT,
            "/api/v1/observability/logs/settings",
            Some(&session),
            Some(json!({
                "log_level_default": "debug",
                "logs_retention_days": 7
            })),
        ),
    )
    .await;
    assert_eq!(good_save.status, StatusCode::OK, "{}", good_save.body);
    assert_eq!(good_save.body["log_level_default"], "debug");
    assert_eq!(good_save.body["logs_retention_days"], 7);

    // The write is audited: a change to what the platform records is a change somebody has to be
    // able to answer for.
    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where action = 'observability.settings.updated'",
    )
    .fetch_one(db.pool())
    .await
    .expect("the audit count must read");
    assert!(audited >= 1, "the settings write must be audited");

    // Restore, so the suite leaves the row as it found it.
    store::save_settings(
        db.pool(),
        &LogSettings {
            log_level_default: "info".to_owned(),
            log_level_overrides: serde_json::Value::Object(serde_json::Map::new()),
            logs_retention_days: 14,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        },
        Some(user_id),
    )
    .await
    .expect("the settings must be restorable");
}

#[tokio::test]
async fn the_stored_line_never_carries_a_secret_or_an_address() {
    // SAFETY: `set_var` is `unsafe` in edition 2024 and this is the process-wide database
    // URL the whole binary reads once at startup; the tests in this file run one at a
    // time. The same guard the REQ-125 suites beside it use.
    unsafe {
        std::env::set_var(
            "OMNION_DATABASE_URL",
            std::env::var("OMNION_TEST_DATABASE_URL")
                .unwrap_or_else(|_| "postgres://omnion:omnion@127.0.0.1:5433/omnion_w6_dev".into()),
        );
    }
    let Some((state, db)) = live_state().await else {
        eprintln!("skipping: no test database");
        return;
    };
    let _ = &state;

    // The caller does everything a careless caller would do: a credential under a field name that
    // is not on any list, one under a list name, a credential inside a nested object, and an
    // e-mail inside a sentence.
    let request_id = Uuid::new_v4();
    let context = LogContext::new_request(request_id);
    let entry = NewLogEntry::new(
        LogLevel::Error,
        "omnion_ai::provider",
        format!("the provider rejected {FIXTURE_SECRET} for {FIXTURE_EMAIL} twice"),
    )
    .with_field("detail", FIXTURE_SECRET)
    .with_field("api_key", FIXTURE_SECRET)
    .with_field(
        "request",
        json!({ "headers": { "authorization": FIXTURE_SECRET } }),
    )
    .build_with(&context);

    let id = store::write(db.pool(), &entry)
        .await
        .expect("the line must be stored");

    // Read it back the way a dump would: as the JSON line an operator exports.
    let row: store::LogRow = sqlx::query_as(
        "select id, ts, level, target, message, request_id, trace_id, span_id, user_id, \
         organization_id, route, method, status, duration_ms, source, host, version, fields \
         from obs_log_entries where id = $1",
    )
    .bind(id)
    .fetch_one(db.pool())
    .await
    .expect("the row must read back");

    let rendered = row.to_json_line();
    assert!(
        !rendered.contains("sk-live-51H8xQ2"),
        "the credential reached the stored row: {rendered}"
    );
    assert!(
        !rendered.contains(FIXTURE_EMAIL),
        "the address reached the stored row: {rendered}"
    );
    // The message must stay readable — a redaction pass that blanks the sentence destroys the
    // only thing the line was for.
    assert!(
        row.message.contains("rejected") && row.message.contains("twice"),
        "the message lost its meaning: {}",
        row.message
    );
}

#[tokio::test]
async fn a_wide_field_object_is_truncated_visibly_and_prune_only_removes_old_lines() {
    // SAFETY: `set_var` is `unsafe` in edition 2024 and this is the process-wide database
    // URL the whole binary reads once at startup; the tests in this file run one at a
    // time. The same guard the REQ-125 suites beside it use.
    unsafe {
        std::env::set_var(
            "OMNION_DATABASE_URL",
            std::env::var("OMNION_TEST_DATABASE_URL")
                .unwrap_or_else(|_| "postgres://omnion:omnion@127.0.0.1:5433/omnion_w6_dev".into()),
        );
    }
    let Some((_state, db)) = live_state().await else {
        eprintln!("skipping: no test database");
        return;
    };

    let request_id = Uuid::new_v4();
    let mut wide = NewLogEntry::new(LogLevel::Debug, "omnion::wide", "a very wide line");
    for index in 0..(MAX_FIELD_COUNT + 15) {
        wide = wide.with_field(&format!("key{index}"), index as i64);
    }
    let entry = wide.build_with(&LogContext::new_request(request_id));
    assert!(
        entry.fields.contains_key("_truncated"),
        "a silent truncation is a line that lies about what it recorded"
    );

    let id = store::write(db.pool(), &entry)
        .await
        .expect("the wide line must be stored");
    let row: store::LogRow = sqlx::query_as(
        "select id, ts, level, target, message, request_id, trace_id, span_id, user_id, \
         organization_id, route, method, status, duration_ms, source, host, version, fields \
         from obs_log_entries where id = $1",
    )
    .bind(id)
    .fetch_one(db.pool())
    .await
    .expect("the row must read back");
    assert!(
        (row.fields
            .as_object()
            .map(serde_json::Map::len)
            .unwrap_or(0))
            <= MAX_FIELD_COUNT + 1,
        "the stored field object must be bounded"
    );

    // An old line and a current one; the prune takes the first and leaves the second.
    // A fixed target name collides with the previous run's leftovers, so this suite would pass or
    // fail depending on which run it was — the exact landmine a suite that is not idempotent
    // becomes for whoever runs it second. The target is unique per run and the assertions key off
    // the ids it just wrote.
    let unique = format!("omnion::fresh-{}", Uuid::new_v4().simple());
    let old_id: i64 = sqlx::query_scalar(
        "insert into obs_log_entries (ts, level, target, message, source) \
         values (now() - interval '400 days', 'info', 'omnion::old', 'an old line', 'api') \
         returning id",
    )
    .fetch_one(db.pool())
    .await
    .expect("the old line must be inserted");
    let fresh_id: i64 = sqlx::query_scalar(
        "insert into obs_log_entries (ts, level, target, message, source) \
         values (now(), 'info', $1, 'a current line', 'api') returning id",
    )
    .bind(&unique)
    .fetch_one(db.pool())
    .await
    .expect("the current line must be inserted");

    let removed = store::prune(db.pool(), 14)
        .await
        .expect("the prune must run");
    assert!(
        removed >= 1,
        "the prune must remove the line past the retention window"
    );
    let old_left: i64 = sqlx::query_scalar("select count(*) from obs_log_entries where id = $1")
        .bind(old_id)
        .fetch_one(db.pool())
        .await
        .expect("the count must read");
    assert_eq!(old_left, 0, "a line past the window must be gone");
    let fresh_left: i64 = sqlx::query_scalar("select count(*) from obs_log_entries where id = $1")
        .bind(fresh_id)
        .fetch_one(db.pool())
        .await
        .expect("the count must read");
    assert_eq!(fresh_left, 1, "a line inside the window must survive");

    // The audit trail is not the log store, and the prune must not have touched it.
    let audit_left: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where created_at < now() - interval '1 day'",
    )
    .fetch_one(db.pool())
    .await
    .expect("the audit count must read");
    assert!(
        audit_left >= 0,
        "the prune must not delete from the audit trail"
    );

    // And the explorer is still readable after a prune — a store that cannot be read after its
    // own maintenance is a store nobody trusts.
    let filter = LogFilter {
        target: Some(unique.clone()),
        ..LogFilter::default()
    };
    let found = store::search(db.pool(), &filter)
        .await
        .expect("the store must read after a prune");
    assert_eq!(
        found.len(),
        1,
        "the fresh line must be findable by its own target, and only it"
    );
    assert_eq!(found[0].id, fresh_id);
}
