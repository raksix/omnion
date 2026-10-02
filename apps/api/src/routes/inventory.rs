//! The route inventory: what the router serves, recorded where the routes are declared
//! (REQ-130, slice 3).
//!
//! ## Why this module exists at all, given the request says "from the routers"
//!
//! The request asks for an OpenAPI document *"generated from the REST routers themselves"*. Axum
//! 0.8 offers no way to ask a `Router` what it serves: `Router::routes()` does not exist in the
//! version this repository builds against, and the `PathRouter` holding the match table is
//! `pub(super)` — private to axum. That was **measured, not assumed**: a probe calling
//! `r.routes()` compiles to `error[E0599]: no method named 'routes' found for struct 'Router<S>'`.
//! Reading the vendored source confirms it: `axum-0.8.9/src/routing/` exposes `route`, `nest`,
//! `merge` and the method helpers, and nothing that enumerates.
//!
//! So "from the router" is achieved the only way it can be without forking the framework: the
//! inventory is recorded **in the same expression that registers the route**. [`documented!`]
//! builds the `MethodRouter`, records the `METHOD path` pair and stores the operation's summary
//! and permission in one step.
//!
//! ## Why that is stronger than a list written beside the router
//!
//! A hand-maintained list has two failure modes and this has neither *by omission*: a route
//! cannot be registered without being recorded, and it cannot be recorded without a summary and a
//! permission, because the macro will not expand without them. Both halves of the acceptance line
//! — "fails on an undocumented route" and "fails on a snapshot drift" — need the inventory to be
//! exact, and the only version of it that cannot lag is the one produced at registration time.
//!
//! What it does **not** catch is a route whose recorded path disagrees with what the router
//! serves, because both then agree on the wrong thing. That is a copy-paste error and it is held
//! by [`crate::openapi::check`]'s orphan leg plus the walk test, not by this module.
//!
//! ## The inventory is global and filled once
//!
//! `routes::router()` runs exactly once per process, at boot, so a process-global list is the
//! honest shape — a `Mutex<Vec>` says "this changes at runtime" and it does not. The recorder
//! refuses a **second** registration of the same `METHOD path`, because axum's `Router::route`
//! silently overwrites: without the check a duplicated line yields a document describing one
//! route while the router serves the second of two handlers, and the dead one is invisible.

use std::sync::OnceLock;

use omnion_graphql::openapi::{Annotation, RouteEntry};

fn inventory() -> &'static std::sync::Mutex<Vec<Entry>> {
    static INVENTORY: OnceLock<std::sync::Mutex<Vec<Entry>>> = OnceLock::new();
    INVENTORY.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// One route plus the documentation the emitter needs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    route: RouteEntry,
    annotation: Annotation,
}

/// A duplicate registration, refused loudly rather than silently overwriting.
#[derive(Debug)]
pub struct DuplicateRoute {
    /// The `METHOD path` registered twice.
    pub key: String,
}

impl std::fmt::Display for DuplicateRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "route {} is registered twice; axum would serve the second handler while the OpenAPI \
             document describes one operation — one of the two registrations is dead code",
            self.key
        )
    }
}

impl std::error::Error for DuplicateRoute {}

fn record(entry: Entry) -> Result<(), DuplicateRoute> {
    let mut guard = inventory()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.iter().any(|e| e.route == entry.route) {
        return Err(DuplicateRoute {
            key: format!("{} {}", entry.route.method, entry.route.path),
        });
    }
    guard.push(entry);
    Ok(())
}

/// Every route the process registered, sorted, for the emitter and the drift gate.
///
/// Sorted because registration order follows the order of `let` bindings in a 2 500-line
/// function, and a document whose bytes move when a router is assembled differently would fail
/// the drift gate for a reason no reader could act on. `RouteEntry` derives `Ord`, so the sort is
/// by method first and then path — a total order, which is what byte-stability needs.
pub fn routes() -> Vec<RouteEntry> {
    let mut out: Vec<RouteEntry> = inventory()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .map(|e| e.route.clone())
        .collect();
    out.sort();
    out
}

/// The annotations the routes declared, as the emitter's registry.
///
/// Built from the same records as [`routes`], so the registry cannot describe a route the
/// router does not serve — that combination is exactly the orphan leg of the gate, and here it is
/// unrepresentable rather than merely checked.
pub fn registry() -> omnion_graphql::openapi::Registry {
    let mut registry = omnion_graphql::openapi::Registry::new();
    for entry in inventory()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
    {
        // Cannot fail: `record` refuses a duplicate, and the registry is built fresh from one
        // pass over records with no duplicate `METHOD path` among them.
        registry
            .annotate_unchecked(&entry.route.method, &entry.route.path, entry.annotation.clone());
    }
    registry
}

/// The floor the emitter refuses to go below.
///
/// Far below the smallest router this platform could plausibly ship and far above an empty one,
/// so it can only fire when the inventory is not being filled -- which is the only condition
/// under which the drift gate would be worth nothing. An empty document passes every check the
/// binary makes, which is exactly why it must not be produced: the gate reported `0 routes` and
/// exited 0 on its first real run.
///
/// Public so a test and the binary quote ONE number. Two copies of this constant is the shape
/// where the gate and its own regression test quietly disagree about what "empty" is.
pub const MINIMUM_ROUTES: usize = 100;

/// How many routes are recorded.
pub fn len() -> usize {
    inventory()
        .lock()
        .map(|g| g.len())
        .unwrap_or_default()
}

/// Whether anything is recorded.
pub fn is_empty() -> bool {
    len() == 0
}

/// Reset the inventory. **Test-only.**
///
/// The inventory is process-global because `routes::router()` runs once, and a test that
/// registers routes would otherwise leak them into the next test's document — a suite that passes
/// or fails depending on the order its cases happen to run in.
///
/// **Clearing it is only safe while nothing real is registered.** Before the routes adopted
/// `documented!` this was true of every test in the binary. Afterwards it is not: any test that
/// builds the real router -- and several do -- leaves 401 entries behind, and a case that clears
/// first would then erase them for whatever ran next. Two suites failed exactly that way
/// (`a_route_is_recorded_…` and `the_inventory_is_read_after_the_router_not_before`), each
/// passing alone and failing in the full run.
///
/// The two halves cannot both be satisfied by clearing, so the count is compared only against
/// what the test itself recorded, and `snapshot`/`restore` bracket the real router instead of
/// destroying it. Clearing remains available for tests that own the process.
pub fn reset_for_tests() {
    inventory()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

/// The inventory as it stands, held opaquely. **Test-only.**
///
/// Opaque rather than a `Vec<Entry>` because [`Entry`] is private to this module and a public
/// signature cannot name it -- the same `E0603` the macro hit. A test that wants to restore a
/// real router's contents never looks inside.
#[cfg(test)]
pub struct InventorySnapshot(Vec<Entry>);

/// Take the inventory aside. **Test-only.**
///
/// Pairs with [`restore_for_tests`] to let a test assert about its OWN records without
/// destroying routes a sibling test registered by building the real router.
#[cfg(test)]
pub fn snapshot_for_tests() -> InventorySnapshot {
    InventorySnapshot(
        inventory()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    )
}

/// Restore what [`snapshot_for_tests`] took. **Test-only.**
#[cfg(test)]
pub fn restore_for_tests(saved: InventorySnapshot) {
    let mut guard = inventory()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = saved.0;
}

/// Register a route **and** document it, in one expression.
///
/// ```ignore
/// let pages = documented!(
///     Method::GET,
///     "/pages",
///     { get(content::list_pages) },
///     "content.pages.read",
///     "List pages"
/// );
/// ```
///
/// The permission and the summary are positional rather than optional because both are things a
/// reader of the document needs and neither can be guessed: `content.pages.read` is the catalogue
/// key the guard actually uses, and the summary is what an integrator sees in a generated client.
/// Making them optional would put the route back in the position where "no annotation" is the
/// easiest thing to write.
///
/// ## The handler is braced, and it has to be
///
/// `$handler:expr` cannot hold every handler this router has. `get(h).layer(guards::require(&state,
/// "media.read"))` contains a comma at the top level -- the one separating `&state` from the key
/// -- and a macro's `expr` fragment ends at that comma, so the permission silently shifts left by
/// one position and the summary lands in `$permission`. The error is late and it is confusing:
/// *missing tokens in macro arguments* at the guard, in a call that looks correct.
///
/// `$handler:block` accepts the same text and does not care about commas inside it, so braces are
/// required around the handler. That is the whole reason for the shape, and the alternative --
/// keeping `expr` and forbidding top-level commas -- would mean writing `guards::require(&state,
/// ("workflows.manage"))` in several hundred places.
#[macro_export]
macro_rules! documented {
    ($method:expr, $path:expr, {$handler:expr}, $permission:expr, $summary:expr $(,)?) => {{
        let __route = $crate::routes::inventory::new_route_entry(
            $crate::routes::inventory::method_of(&$method),
            $path,
        );
        $crate::routes::inventory::record_entry(__route, $permission, $summary)
            .unwrap_or_else(|e| panic!("{e}"));
        // The handler's TYPE is settled here, in the macro, rather than at the caller's use
        // site. A `MethodRouter`'s second parameter is the handler's error type, and axum's
        // builders leave it to inference; inside a macro that inference has nothing to work
        // from, so 176 `let` bindings lost theirs and every one failed with
        // `E0283: type annotations needed for MethodRouter<AppState, _>` -- reported at
        // `let roles = get(..)`, lines this macro does not touch.
        //
        // It is written as a coercion on the value rather than as a `let` with a type: a macro
        // `block` fragment matches a single `{ expression }`, so `{ let __r = ..; __r }` does
        // not parse (*no rules expected keyword `let`*).
        {
            let __typed: $crate::routes::inventory::MethodRouterOf = $handler;
            __typed
        }
    }};
}

/// The `MethodRouter` type a documented handler is expected to have.
///
/// An alias rather than the concrete `MethodRouter<AppState, Infallible>`, so the macro names
/// one item from this module instead of three (`MethodRouter`, `AppState`, `Infallible`) that a
/// caller may or may not have imported -- the macro expands in the CALLER's module, where this
/// file's `use` statements do not apply.
pub type MethodRouterOf = ::axum::routing::MethodRouter<crate::state::AppState, std::convert::Infallible>;

/// The wire method of an `axum::http::Method`, as the inventory spells it.
///
/// `Method::as_str` borrows from the method, so it cannot return `&'static str` from a
/// `&Method` parameter -- the caller owns the `Method` only for the length of the macro's
/// expansion. The inventory copies it into an owned `String` immediately, so the cost is one
/// small allocation per route at boot, on a path that runs once per process.
///
/// Separate from [`new_route_entry`] so the macro's expansion names no type from this module's
/// private imports. The macro expands in the CALLER's module, where `use RouteEntry` does not
/// apply -- naming it there is `E0603: private struct import` at all 401 call sites.
pub fn method_of(method: &::axum::http::Method) -> String {
    method.as_str().to_string()
}

/// Build a [`RouteEntry`] for the macro. **Public for the macro.**
///
/// The macro expands in the CALLER's module, where this module's private `use RouteEntry`
/// does not apply -- naming it there is `E0603: private struct import` at all 401 call sites.
/// A constructor is the narrowest thing that can be exposed for this, and it keeps the type
/// itself out of the macro's expansion.
///
/// Takes an owned `String` for the method because [`method_of`] cannot lend: `as_str` borrows
/// from the `Method`, and the macro's `&method` temporary does not outlive the call.
pub fn new_route_entry(method: String, path: &str) -> RouteEntry {
    RouteEntry::new(&method, path)
}

/// Record an already-built entry. **Public for the macro.**
///
/// Split out from [`record`] so the macro does not have to construct the private [`Entry`], and
/// so the permission and summary are normalised in exactly one place.
///
/// **Both the parameter type and the macro's own construction are spelled as the full path.**
/// A `pub fn` cannot name a private import, and the macro expands in the CALLER's module, where
/// this module's `use` statement does not apply at all -- so `inventory::RouteEntry` is
/// `E0603: private struct import` at every one of the 401 call sites. The private alias stays for
/// this module's own tests and helpers, which read better with it.
pub fn record_entry(
    route: omnion_graphql::openapi::RouteEntry,
    permission: &str,
    summary: &str,
) -> Result<(), DuplicateRoute> {
    record(Entry {
        route,
        annotation: Annotation {
            summary: summary.to_string(),
            description: None,
            permission: (!permission.is_empty()).then(|| permission.to_string()),
            tags: Vec::new(),
            deprecated: None,
        },
    })
}

// The inventory is process-global, so every case here must not be the only one using it.
//
// **Two suites failed the moment the routes adopted `documented!`**, both for the same reason
// and both invisible when run alone: any test that builds the real router now registers 401
// entries, and this module's cases asserted `all.len() == 2` against whatever was there. They
// also race each other, which is what the original comment warned about -- five cases
// clearing and filling ONE `Mutex<Vec>` pass by luck and fail by schedule.
//
// So the guard is ONE object that both isolates and serializes: it takes a process-wide test
// lock for its lifetime and restores the real inventory when it drops. `clear()` alone would
// be enough for the assertion, and is not enough for the run.
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) struct Isolated {
    _lock: std::sync::MutexGuard<'static, ()>,
    saved: InventorySnapshot,
}

#[cfg(test)]
impl Isolated {
    /// Take the test lock and start from an empty inventory; the real one comes back on drop.
    pub(crate) fn new() -> Self {
        // A poisoned lock is this module's own doing (a panicking case), so it is recovered
        // rather than propagated: the next case needs the lock, not the previous case's panic.
        let lock = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved = snapshot_for_tests();
        reset_for_tests();
        Isolated { _lock: lock, saved }
    }
}

#[cfg(test)]
impl Drop for Isolated {
    fn drop(&mut self) {
        let placeholder = InventorySnapshot(Vec::new());
        let saved = std::mem::replace(&mut self.saved, placeholder);
        restore_for_tests(saved);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_route_is_recorded_with_its_annotation_and_read_back_sorted() {
        let _guard = Isolated::new();
        record_entry(RouteEntry::new("GET", "/z"), "content.pages.read", "Zeta")
            .expect("first");
        record_entry(RouteEntry::new("GET", "/a"), "content.pages.read", "Alpha")
            .expect("second");

        let all = routes();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].path, "/a", "sorted, not registration-ordered");
        assert_eq!(all[1].path, "/z");

        // And the annotation came back with it: a registry that could describe a route the
        // router does not serve is what the orphan leg exists to catch, and here the two are
        // built from ONE record.
        let registry = registry();
        assert_eq!(
            registry.get(&RouteEntry::new("GET", "/a")).unwrap().summary,
            "Alpha"
        );
        assert_eq!(
            registry
                .get(&RouteEntry::new("GET", "/a"))
                .unwrap()
                .permission
                .as_deref(),
            Some("content.pages.read")
        );
    }

    #[test]
    fn registering_the_same_method_and_path_twice_is_refused() {
        // axum's `Router::route` silently overwrites, so without this a copy-pasted line yields
        // a document describing one operation while the router serves the second handler.
        let _guard = Isolated::new();
        record_entry(RouteEntry::new("GET", "/pages"), "content.pages.read", "List").expect("first");
        let err = record_entry(RouteEntry::new("GET", "/pages"), "content.pages.read", "List")
            .unwrap_err();
        assert!(err.to_string().contains("GET /pages"), "got {err}");
        assert_eq!(len(), 1, "the refused registration must not be recorded");
    }

    #[test]
    fn the_same_path_under_two_methods_is_two_routes_not_a_duplicate() {
        // GET and POST on one path is a `MethodRouter` with two handlers; calling that a
        // duplicate would make the gate reject the most ordinary route shape in the API.
        let _guard = Isolated::new();
        record_entry(RouteEntry::new("GET", "/pages"), "content.pages.read", "List").expect("get");
        record_entry(RouteEntry::new("POST", "/pages"), "content.pages.create", "Create")
            .expect("post");
        assert_eq!(len(), 2);
    }

    #[test]
    fn an_empty_permission_is_stored_as_absent_rather_than_as_an_empty_string() {
        // An empty permission key would reach a route guard and refuse every caller, which is
        // the defect REQ-133 and REQ-128 each lost ticks to. `None` here means "documented route
        // with no catalogue key" — a fact the emitter can show instead of a key that lies.
        let _guard = Isolated::new();
        record_entry(RouteEntry::new("GET", "/healthz"), "", "Liveness probe").expect("record");
        let registry = registry();
        assert_eq!(
            registry.get(&RouteEntry::new("GET", "/healthz")).unwrap().permission,
            None
        );
    }

    #[test]
    fn the_verb_the_macro_spells_becomes_the_wire_method() {
        // The macro derives the method from an axum `Method` parse; if that spelling drifted,
        // every recorded route would stop matching its annotation and the gate would report the
        // whole API as undocumented. Assert the spellings the macro relies on.
        for (verb, wire) in [
            ("GET", "GET"),
            ("POST", "POST"),
            ("PUT", "PUT"),
            ("PATCH", "PATCH"),
            ("DELETE", "DELETE"),
        ] {
            let m: axum::http::Method = verb.parse().unwrap();
            assert_eq!(m.as_str(), wire);
        }
    }
}
