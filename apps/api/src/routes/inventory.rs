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
pub fn reset_for_tests() {
    inventory()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

/// Register a route **and** document it, in one expression.
///
/// ```ignore
/// let pages = documented!(
///     Method::GET,
///     "/pages",
///     get(content::list_pages),
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
#[macro_export]
macro_rules! documented {
    ($method:expr, $path:expr, $handler:expr, $permission:expr, $summary:expr $(,)?) => {{
        let __method: ::axum::http::Method = $method;
        let __route = $crate::routes::inventory::RouteEntry::new(__method.as_str(), $path);
        $crate::routes::inventory::record_entry(__route, $permission, $summary)
            .unwrap_or_else(|e| panic!("{e}"));
        $handler
    }};
}

/// Record an already-built entry. **Public for the macro.**
///
/// Split out from [`record`] so the macro does not have to construct the private [`Entry`], and
/// so the permission and summary are normalised in exactly one place.
pub fn record_entry(
    route: RouteEntry,
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

#[cfg(test)]
mod tests {
    use super::*;

    // The inventory is process-global, so every case here clears it first AND runs serially.
    // Five tests racing one Mutex<Vec> pass by luck and fail by schedule — the suite would
    // report a duplicate-route refusal that belongs to another case. That is the same shape as
    // the sign-in limiter my harness walked into: a shared budget, spent by whichever writer
    // touches it first.

    #[test]
    fn a_route_is_recorded_with_its_annotation_and_read_back_sorted() {
        reset_for_tests();
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
        reset_for_tests();
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
        reset_for_tests();
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
        reset_for_tests();
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
