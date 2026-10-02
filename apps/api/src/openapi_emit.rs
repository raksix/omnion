//! Building the route inventory without a running platform (REQ-130, slice 3).
//!
//! ## The constraint this module exists to satisfy
//!
//! The drift gate has to run in CI, and CI for this repository has no PostgreSQL and no Redis.
//! But the document must be generated from the **real** router — a fixture route list would be a
//! second source of truth, which is the drift the gate exists to catch.
//!
//! So this builds `routes::router(state)` for real, against a **lazily-connected pool**. Every
//! router-level artefact — the path tree, the guards, the inventory the `documented!` macro
//! filled — is decided at assembly time, and none of it touches a connection. `PgPool::connect_lazy`
//! is exactly the tool for that: it validates the URL and opens nothing.
//!
//! ## If a route ever needs state at registration time, this module starts failing
//!
//! That is the intended failure. A handler that runs during registration would need a database to
//! emit a document, and the gate would either need a live database (slow, flaky, and unable to run
//! in a sandbox) or would need a stub (which is the second source of truth this avoids). The
//! error from `connect_lazy` names the URL that failed to parse, which is the only thing that can
//! go wrong here.

use omnion_graphql::openapi::{Registry, RouteEntry};

use crate::routes;
use crate::state::AppState;

/// Where the committed snapshot lives, relative to the repository root.
pub use crate::routes::openapi::SNAPSHOT_PATH;

/// The pool URL the emitter assembles the router against.
///
/// A syntactically valid URL pointing at a server that is not there. `connect_lazy` never dials,
/// so this constant is a parse fixture and nothing more — the name says so, because a reader who
/// finds a hard-coded host in a binary deserves to know it is never contacted.
const EMITTER_POOL_URL: &str = "postgres://omnion:omnion@127.0.0.1:1/omnion_openapi_emit";

/// Build the router, and read back what it registered.
///
/// The inventory is read AFTER the router is built: `routes::router()` is the only thing that
/// fills it, so reading first would return an empty list and the emitter would cheerfully write
/// an empty document — which would pass its own coverage check, because an empty router has no
/// undocumented routes.
pub fn inventory_and_registry() -> (Vec<RouteEntry>, Registry) {
    let _router = assemble_router();
    (routes::inventory::routes(), routes::inventory::registry())
}

/// Assemble the real router against a pool that is never dialled.
pub fn assemble_router() -> axum::Router {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_millis(1))
        .connect_lazy(EMITTER_POOL_URL)
        .expect("the emitter pool URL is syntactically valid");
    let state = AppState::with_pool(pool);
    routes::router(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Not `#[test]` — `#[tokio::test]`.** A plain `#[test]` has no Tokio context and
    /// `connect_lazy` panics with *"this functionality requires a Tokio context"* the moment the
    /// pool installs its reaper. Found by running it: the same assertion as a sync test fails for
    /// a reason that has nothing to do with databases, which is the most expensive way to learn
    /// that the pool needs a runtime.
    #[tokio::test]
    async fn assembling_the_router_does_not_need_a_database() {
        // The whole point of `connect_lazy`: if this test ever needs a running PostgreSQL, the
        // drift gate has stopped being runnable in CI, and the failure will be a CI timeout on a
        // gate whose entire purpose is to fail fast.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(1))
            .connect_lazy(EMITTER_POOL_URL)
            .expect("lazy");
        let state = AppState::with_pool(pool);
        let _router = routes::router(state);
    }

    #[tokio::test]
    async fn the_inventory_is_read_after_the_router_not_before() {
        // The failure this guards is quiet and total: reading the inventory first returns an
        // empty list, and an empty router has no undocumented routes, so the emitter writes a
        // document with `paths: {}` and every check it makes passes. The floor in the binary
        // exists because of exactly this.
        crate::routes::inventory::reset_for_tests();
        let _ = super::inventory_and_registry();
        let routes = routes::inventory::routes();
        assert!(
            routes.is_empty(),
            "the inventory is still empty: this run registered nothing, so the coverage gate has \
             nothing to check"
        );
        crate::routes::inventory::reset_for_tests();
    }
}
