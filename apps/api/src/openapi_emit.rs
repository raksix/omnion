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
        //
        // The route-adoption pass turned this case into an accidental canary. `routes::router()`
        // now RECORDS every registration, and the inventory refuses a second registration of the
        // same `METHOD path` -- so a process that builds the router twice panics with
        // *"route POST /media is registered twice"*. That is the refusal working: axum's own
        // `Router::route` silently overwrites, and the check is what stops the document from
        // describing one operation while the router serves the other.
        //
        // It also means this case can no longer build the router on its own: the sibling case in
        // this module does, and the test harness runs them on separate threads in one process.
        // Clearing the inventory first is therefore not tidiness, it is the only way the case can
        // mean what it says.
        let guard = crate::routes::inventory::Isolated::new();

        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(1))
            .connect_lazy(EMITTER_POOL_URL)
            .expect("lazy");
        let state = AppState::with_pool(pool);
        let _router = routes::router(state);

        assert!(
            !crate::routes::inventory::routes().is_empty(),
            "the router assembled without a database AND registered nothing -- one of those two \
             is a regression, and this test is the only place both are visible together"
        );
        drop(guard);
    }

    #[tokio::test]
    async fn the_inventory_is_read_after_the_router_not_before() {
        // The failure this guards is quiet and total: reading the inventory first returns an
        // empty list, and an empty router has no undocumented routes, so the emitter writes a
        // document with `paths: {}` and every check it makes passes. The floor in the binary
        // exists because of exactly this.
        //
        // **The assertion had to be INVERTED along with the router.** It used to read
        // `routes.is_empty()` after clearing, which was true before the route-adoption pass --
        // `inventory_and_registry()` had registered nothing -- so the case measured the empty
        // state it had just created and passed on it. With the routes adopted it registered 401
        // and the same line failed, which read as a regression rather than as the sentence finally
        // meaning something.
        //
        // So the claim is now the one worth asserting: building the router FILLS the inventory,
        // and reading it afterwards returns those routes. Clearing first is still necessary --
        // otherwise a sibling test's routes make this pass for the wrong reason -- which is why
        // the shared `Isolated` guard, not a bare `reset_for_tests`.
        let guard = crate::routes::inventory::Isolated::new();

        // Before: the inventory was cleared and nothing had registered yet.
        assert!(
            crate::routes::inventory::routes().is_empty(),
            "the guard must start from an empty inventory, or this case proves nothing"
        );

        let _ = super::inventory_and_registry();
        let routes = crate::routes::inventory::routes();

        assert!(
            !routes.is_empty(),
            "building the router registered nothing, so the drift gate has no document to check"
        );
        assert!(
            routes.len() >= crate::routes::inventory::MINIMUM_ROUTES,
            "the real router registered {} routes, below the floor the binary refuses to emit \
             under -- the gate and this test disagree about what an empty router looks like",
            routes.len()
        );
        drop(guard);
    }
}
