//! Integration walk for the bounded telemetry write path (REQ-126, `omnion_telemetry::sink`).
//!
//! ## The defect this file exists to hold shut
//!
//! `apps/api/src/request_log.rs` used to write the request's log line **on the request's own
//! task**, immediately after the handler returned, and then write the trace index row the same
//! way. Both were `await`ed, both took a connection from the same bounded pool the handlers use,
//! and both inherited that pool's **5 s `acquire_timeout`**. So when the pool was exhausted:
//!
//! - every request spent up to five seconds, twice, on telemetry;
//! - and the line it was waiting to write was **discarded** at the end of it.
//!
//! The module comment above `store::write` called this "a request path never blocks on
//! telemetry", justified by there being no queue to drain. That was the wrong reason: the absence
//! of a queue was not a protection, it **was** the stall. The queue in `omnion_telemetry::sink` is
//! what makes the claim true — the request pushes and returns, and a background drain owns the
//! connections.
//!
//! ## Why the first assertion is about POOL SIZE and not about latency
//!
//! The obvious assertion — "the request answers quickly" — cannot fail on the broken code. A
//! healthy database hands the write a free connection in under a millisecond, so the synchronous
//! version is *faster* while it is not under pressure, and the defect only appears when the pool
//! is exactly the thing being contended. A latency assertion on an uncontended pool measures the
//! database, not the design.
//!
//! So the walk takes the LAST connection. The pool is built with a capacity the walk controls and
//! no spare, the request path is deliberately starved, and the old code's write then had to wait
//! out an acquire timeout that the walk measures in milliseconds. The broken version spends
//! seconds here; the fixed version returns immediately because it never asks for a connection.
//!
//! That is also why the assertion is `< 1 s` and not "is fast": a threshold loose enough to absorb
//! CI noise still cannot absorb a 5 s `acquire_timeout`, so the test separates the two designs
//! without being flaky against either.

mod support;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::seed;
use sqlx::PgPool;
use std::time::{Duration, Instant};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "Telemetry-Sink-Passw0rd-2026!";

/// A pool with exactly one connection, so the write path and the handler compete for the same
/// one. The capacity is the assertion's instrument: with a spare connection the broken code
/// succeeds and the test cannot tell the two designs apart.
fn starved_pool(url: &str) -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        // Short, so a walk that regresses fails in seconds rather than hanging the suite for the
        // production 5 s per write. A regression is still unmistakably slow against it.
        // Two seconds, so it sits ABOVE this walk's one-second bar. The first draft set it to
        // 750 ms against a 1 s bar, and the timing assertion then could not fail — a bar above the
        // timeout it is meant to detect is a comment. The queue assertion caught the regression
        // anyway, which is the only reason the first version was not a green walk over a broken
        // design.
        .acquire_timeout(Duration::from_secs(2))
        .connect_lazy(url)
        .expect("the test database URL must parse")
}

#[tokio::test]
async fn the_request_path_never_waits_on_a_connection_to_write_telemetry() {
    // SAFETY: `set_var` is `unsafe` in edition 2024 and this is the process-wide database URL the
    // binary reads once at startup; the observability suites are serialised by the database.
    unsafe {
        std::env::set_var(
            "OMNION_DATABASE_URL",
            std::env::var("OMNION_TEST_DATABASE_URL")
                .unwrap_or_else(|_| "postgres://omnion:omnion@127.0.0.1:5433/omnion_w6_dev".into()),
        );
    }
    support::walk_state::ensure_csrf_secret();

    let config = Config::from_env().expect("environment must be valid");
    let warmup = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(error) => {
            eprintln!("skipping: PostgreSQL is not reachable ({error})");
            return;
        }
    };
    warmup.migrate().await.expect("migrations must apply");

    let url = config.database.url.clone();
    let state = AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config.clone(),
        // The state under test is built on the STARVED pool. The warm-up `Db` above is dropped at
        // the end of this function, but it is a separate pool and does not share connections.
        Db::from_pool(starved_pool(&url)),
        RedisClient::new(&config.redis.url).expect("redis URL must parse"),
        omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
            .expect("the default storage configuration is valid"),
    );
    support::walk_state::ensure_test_rate_limits(&state);

    // The fixture: one organization, one account, one role seed. Written on the WARM pool, so the
    // setup is not itself the thing competing for the single connection.
    let slug = format!("sink-{}", Uuid::new_v4().simple());
    let organization_id: Uuid =
        sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
            .bind("Telemetry Sink Organization")
            .bind(&slug)
            .fetch_one(warmup.pool())
            .await
            .expect("the organization must be created");
    let email = format!("sink-{}@example.test", Uuid::new_v4().simple());
    users::create_user(
        warmup.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Telemetry Sink Test".to_owned(),
            organization_id: Some(organization_id),
        },
    )
    .await
    .expect("the account must be created");
    seed::ensure(warmup.pool())
        .await
        .expect("the default roles must exist");

    // Hold the ONE connection the starved pool may use, for the whole measurement. Nothing the
    // request path does can obtain a connection while this guard is alive — which is the point:
    // under the old code the log write and the trace write each blocked on it.
    let held = state
        .db()
        .pool()
        .acquire()
        .await
        .expect("the single connection must be obtainable");

    let app = omnion_api::routes::router(state.clone());
    let started = Instant::now();
    let response = tokio::time::timeout(
        Duration::from_secs(20),
        app.oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/v1/observability/logs?limit=1")
                .body(Body::empty())
                .expect("a valid request"),
        ),
    )
    .await
    .expect("the request must not hang: telemetry blocked it")
    .expect("the router must answer");
    let elapsed = started.elapsed();

    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("the body must read")
        .to_bytes();

    // ── the claim ────────────────────────────────────────────────────────────────────────────
    // The response arrived while the pool had NO free connection for the whole request. The
    // broken version cannot produce this: its telemetry write waits out the pool's acquire
    // timeout with the only connection held, so the response is ≥ the acquire timeout late — 750
    // two seconds here, and 5 s in the shipped configuration.
    //
    // The bar is 1 s against a 2 s acquire timeout configured below, so the two are separated by
    // a factor of two: loose enough that a loaded box does not produce a false positive, and
    // impossible for a regression to squeak under. A wall-clock assertion is normally the weak choice, and it is the strong choice HERE
    // because the defect it detects is a wait — a slow test that fails on the old code and passes
    // on the new one is measuring exactly the thing that changed.
    assert!(
        elapsed < Duration::from_secs(1),
        "the request took {elapsed:?} with the only pooled connection held: the request path is \
         waiting on a connection to write telemetry, which is the defect this walk exists for \
         (the pool's acquire timeout in this walk is 2 s, so any regression lands above this bar)"
    );
    assert!(
        !body.is_empty() || status == StatusCode::UNAUTHORIZED,
        "the router must answer with a body, got {status}"
    );

    // The three halves that make this a REAL assertion rather than a timing note.
    //
    // 1. The queue is what the request path used. A line queued while the pool is starved is a
    //    line that was NOT written synchronously — and it is still recoverable, which is the
    //    difference between a bounded lossy buffer and a log that loses lines quietly.
    let queued = omnion_telemetry::sink::global().depth();
    assert!(
        queued > 0,
        "a request that ran with no free connection must have QUEUED its line, not written it \
         synchronously (and certainly not awaited a connection to do it)"
    );

    // 2. Nothing was lost to the starvation itself. The drop counter is the visible half of the
    //    contract, and it must still be zero here: a queue that is thousands deep does not evict
    //    because a single request was unable to get a connection.
    assert_eq!(
        omnion_telemetry::sink::global().dropped(omnion_telemetry::sink::WriteKind::Log),
        0,
        "one request into an empty queue must not evict anything"
    );

    // 3. The telemetry that could not be written synchronously IS written once a connection is
    //    available — the queue is a delay, not a discard. Drained here by hand because this walk
    //    does not spawn the task `main.rs` spawns, and an assertion that the line exists is the
    //    one that distinguishes a buffer from a hole.
    drop(held);
    let flushed = tokio::time::timeout(
        Duration::from_secs(10),
        omnion_telemetry::sink::drain_until_empty(state.db().pool()),
    )
    .await
    .expect("the drain must not hang");
    assert!(
        flushed.written > 0,
        "the queued line must be written once a connection is free — a queue that drops on drain \
         is a hole, not a buffer (report: {flushed:?})"
    );
    assert_eq!(flushed.failed, 0, "the drain must not fail against a live database");
}
