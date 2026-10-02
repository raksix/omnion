//! The router builds. One test, and it is the test this crate should have had from the start.
//!
//! # Why this file exists
//!
//! `axum` validates a router at **construction**, not at request time. Two routes that collide
//! — the same path with the same method, a path segment starting with `*` under 0.8 — make
//! [`omnion_api::routes::router`] **panic**, and the whole API refuses to start. Everything
//! else in this crate is invisible to that failure:
//!
//! - `cargo test -p omnion-api --test ai_skills` drives the **store** through the test binary,
//!   which links the library but never *calls* `router`, so a routing table that cannot be
//!   constructed passes 28 walks.
//! - `cargo test -p omnion-ai-hub` does not even link the API.
//! - `pnpm typecheck` reads the types; two `MethodRouter`s merged onto one path are perfectly
//!   well-typed.
//! - `cargo build` succeeds. The panic is runtime.
//!
//! So the defect is invisible to every gate this repository had, and the only thing that could
//! find it was a QA pass — which is exactly how it was found, on the third attempt, after
//! "the API did not answer" had sent two rounds of debugging at the harness and the database
//! before somebody read the panic's text. Three separate passes failed on this between 16:00
//! and 18:00.
//!
//! A test that constructs the router turns a boot-time panic into a red line, and it costs one
//! function call. That is the cheapest defect-to-signal conversion in the codebase.

use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};

/// Build the router against a real pool, and let axum say no.
///
/// No database: `router()` reads the state's config to layer guards and never touches the pool,
/// so a pool that is *lazily* connected is enough. `connect` would try to reach a server and
/// this would then be a database test that occasionally fails because PostgreSQL is busy — a
/// different failure, and the wrong lesson.
///
/// It is a `#[tokio::test]` rather than a plain `#[test]` because `sqlx` builds its pool
/// internals inside a Tokio context even when it never opens a connection. That is a real
/// constraint, not a style preference: the first version of this test was synchronous and
/// failed with "this functionality requires a Tokio context", which says nothing about the
/// router. Worth stating because the failure would otherwise read as a routing problem.
#[tokio::test]
async fn the_router_constructs() {
    let config = Config::from_env().unwrap_or_else(|_| {
        // `Config::from_env` needs a populated environment; a test that requires one is a test
        // that gets skipped in CI, and a skipped test is a router that is never built.
        Config::default()
    });
    let db = Db::connect_lazy(&DatabaseConfig {
        url: "postgres://omnion:omnion@127.0.0.1:5433/omnion".to_owned(),
        max_connections: 1,
    })
    .expect("a lazy pool needs only a parseable URL");
    let redis = RedisClient::new(&config.redis.url).expect("the redis URL must parse");

    let state = AppState::new(
        BuildInfo::new("omnion-api", "0.1.0-test"),
        config,
        db,
        redis,
        omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
            .expect("the default storage configuration is valid"),
    );

    // The whole test. A panic here is the defect, named by axum, with the file and line.
    let _router = routes::router(state);
}
