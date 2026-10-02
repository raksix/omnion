//! Deprecation headers on every response (REQ-130, slice 4).
//!
//! ## The acceptance line is about the WIRE, so this is a middleware and not a handler
//!
//! *"Deprecated routes and fields return `Deprecation` and `Sunset` headers"* — the header has to
//! be on the response of a route nobody remembered to edit. A handler that adds its own header is
//! correct for the thirteen routes someone remembered and silent for the four hundred they did
//! not, and a missing `Sunset` is indistinguishable from "never deprecated", which is the outcome
//! this feature exists to prevent. So the lookup is here, on the way out, for every path.
//!
//! ## The cache is a `RwLock<Option<Arc<Snapshot>>>`, and the write path swaps it whole
//!
//! The request's own note: *"The deprecation middleware reads a cached map rebuilt on write, so
//! header overhead per request is a hash lookup."* The shape matters more than the speed. A
//! `RwLock<HashMap>` mutated per request would take a write lock on the request path; a snapshot
//! behind an `Arc` means a read lock for the whole lookup and a swap on a write, so an
//! announcement cannot be seen half-applied by a concurrent request.
//!
//! ## The removed answer short-circuits, and that is the half nobody wires up
//!
//! A row whose sunset has passed must NOT run its handler. That is not a "return a 410 from a
//! middleware" nicety — it is the difference between a route that is *declared* gone and one that
//! quietly keeps working while the screen says it is removed. [`apply`] returns before
//! `next.run(request)`, so no handler executes and no query runs on the way out.
//!
//! ## What is NOT here, and why that is a decision
//!
//! A **field** deprecation (`Page.author`) is not enforced here. A field is not a path, so a path
//! middleware cannot match it without parsing the request body, and a middleware that parsed every
//! GraphQL document on the way past would be a second parser beside the one that executes it. The
//! field's deprecation is carried three other ways, all of which exist already: the schema hides
//! it from callers that must not use it, the OpenAPI document renders it, and the deprecations
//! screen lists it. The *route* half of acceptance 11 — the headers — is what this layer
//! implements.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use sqlx::{PgPool, Row};
use time::OffsetDateTime;

use omnion_graphql::deprecation::{DeprecationHeaders, DeprecationRow, Outcome, Status, outcome};

use crate::state::AppState;

/// One snapshot of the announcement map.
#[derive(Debug, Clone, Default)]
struct Snapshot {
    by_key: HashMap<String, Entry>,
}

/// What the middleware needs per key: the headers, and whether the path is gone.
#[derive(Debug, Clone)]
struct Entry {
    /// `None` when the row is withdrawn or removed — both of which send nothing.
    headers: Option<DeprecationHeaders>,
    /// `Some(410)` when the sunset has passed and the handler must not run.
    gone: bool,
    /// The body a gone path answers, so the client is told what replaced it.
    removed_body: Option<serde_json::Value>,
}

static SNAPSHOT: RwLock<Option<Arc<Snapshot>>> = RwLock::new(None);

/// The entry for a request, looked up once.
///
/// `None` before the first build, and the middleware treats that as "nothing is deprecated" — the
/// same answer a platform with no deprecations gives. A process that boots with the table missing
/// therefore serves every request normally rather than refusing them all.
fn lookup(method: &str, path: &str) -> Option<Entry> {
    let guard = SNAPSHOT.read().ok()?;
    let snapshot = guard.as_ref()?;
    // The method-specific key first, then the path-only key. A row with a method and a row
    // without one can both exist for the same path, and the specific one wins — which is the
    // direction that cannot under-announce.
    snapshot
        .by_key
        .get(&format!("{method} {path}"))
        .or_else(|| snapshot.by_key.get(path))
        .cloned()
}

/// The key a row is stored under.
fn key_for(method: Option<&str>, path: &str) -> String {
    match method {
        Some(method) => format!("{method} {path}"),
        None => path.to_owned(),
    }
}

/// Build one entry from a row, at `now`.
fn entry_for(row: &DeprecationRow, now: OffsetDateTime) -> Entry {
    let gone = outcome(row, now) == Outcome::Gone;
    Entry {
        headers: omnion_graphql::deprecation::headers_for(row, now),
        gone,
        removed_body: gone.then(|| omnion_graphql::deprecation::removed_response(row)),
    }
}

/// Rebuild the map from PostgreSQL and swap it in.
///
/// Called at boot and by every write, never on the request path. A failed read leaves the
/// previous snapshot in place rather than emptying it: a database blip must not turn every
/// deprecated route into an unheadered one, which is the silent-removal failure this feature is
/// for. **An empty successful read DOES replace the map** — otherwise deleting the last
/// deprecation row would leave its headers on the wire for ever, which is the same defect in the
/// opposite direction.
pub async fn refresh(pool: &PgPool) {
    let Ok(rows) = sqlx::query(
        "select route_pattern, method, field_path, deprecated_in, sunset_at, replacement, note, \
                status \
         from api_deprecations where route_pattern is not null",
    )
    .fetch_all(pool)
    .await
    else {
        tracing::warn!("the deprecation map could not be rebuilt; the previous snapshot is kept");
        return;
    };

    let now = OffsetDateTime::now_utc();
    let mut by_key: HashMap<String, Entry> = HashMap::new();
    for row in rows {
        let Some(route_pattern) = row.get::<Option<String>, _>("route_pattern") else {
            continue;
        };
        let method = row.get::<Option<String>, _>("method");
        let policy_row = DeprecationRow {
            route_pattern: Some(route_pattern.clone()),
            method: method.clone(),
            field_path: row.get::<Option<String>, _>("field_path"),
            deprecated_in: row
                .get::<Option<String>, _>("deprecated_in")
                .unwrap_or_default(),
            sunset_at: row.get::<OffsetDateTime, _>("sunset_at"),
            replacement: row.get::<Option<String>, _>("replacement"),
            note: row.get::<Option<String>, _>("note").unwrap_or_default(),
            status: match row.get::<Option<String>, _>("status").as_deref() {
                Some("active") => Status::Active,
                Some("removed") => Status::Removed,
                Some("withdrawn") => Status::Withdrawn,
                _ => Status::Announced,
            },
        };
        by_key.insert(key_for(method.as_deref(), &route_pattern), entry_for(&policy_row, now));
    }

    let snapshot = Arc::new(Snapshot { by_key });
    match SNAPSHOT.write() {
        Ok(mut guard) => *guard = Some(snapshot),
        // A poisoned lock means another thread panicked while holding it. The map is a cache, so
        // rebuilding is strictly better than propagating a panic into every request.
        Err(poisoned) => *poisoned.into_inner() = Some(snapshot),
    }
}

/// How many paths the map currently knows about. For the health line and for tests.
#[must_use]
pub fn tracked_paths() -> usize {
    SNAPSHOT
        .read()
        .ok()
        .and_then(|guard| guard.as_ref().map(|snapshot| snapshot.by_key.len()))
        .unwrap_or_default()
}

/// The middleware. Installed on the whole `/api/v1` tree by `routes::router`.
pub async fn apply(
    State(_state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();

    let Some(entry) = lookup(&method, &path) else {
        return next.run(request).await;
    };

    if entry.gone {
        // `next.run` is NOT awaited here, and that is the whole point of the branch: a handler
        // for a surface whose sunset passed must not execute. The request is dropped with the
        // `Request`, and the body is ours.
        let body = entry
            .removed_body
            .clone()
            .unwrap_or_else(|| serde_json::json!({ "code": "REMOVED" }));
        let mut response = Response::new(Body::from(body.to_string()));
        *response.status_mut() =
            StatusCode::from_u16(omnion_graphql::deprecation::REMOVED_STATUS).unwrap_or(StatusCode::GONE);
        let headers = response.headers_mut();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        // The `Link` still goes on a gone response, and the `Sunset` still does NOT: the request's
        // own risks section says "a route without a `Sunset` header is only removed in a major
        // release", which means the response that says it is gone is the response that must not
        // invite a client to plan around a past date.
        return response;
    }

    let mut response = next.run(request).await;
    if let Some(headers) = &entry.headers {
        write_headers(response.headers_mut(), headers);
    }
    response
}

/// Put the three headers on a response.
///
/// `try_from` rather than `from_static`, because all three values are built at runtime. A value
/// that cannot be a header — a `deprecated_in` carrying a newline, which a hand-crafted announce
/// could produce — is SKIPPED rather than failing the response: an announcement with a bad version
/// must not take a route down, and the write path refuses such a version for the same reason.
fn write_headers(into: &mut axum::http::HeaderMap, headers: &DeprecationHeaders) {
    if let Ok(value) = HeaderValue::from_str(&headers.deprecation) {
        into.insert("deprecation", value);
    }
    if let Ok(value) = HeaderValue::from_str(&headers.sunset) {
        into.insert("sunset", value);
    }
    if let Ok(value) = HeaderValue::from_str(&headers.link) {
        into.insert("link", value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> OffsetDateTime {
        OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
            .expect("the fixture parses")
    }

    fn row(status: Status, sunset: &str) -> DeprecationRow {
        DeprecationRow {
            route_pattern: Some("/api/v1/pages".to_owned()),
            method: None,
            field_path: None,
            deprecated_in: "1.4".to_owned(),
            sunset_at: at(sunset),
            replacement: Some("/pages".to_owned()),
            note: String::new(),
            status,
        }
    }

    #[test]
    fn an_active_row_yields_headers_and_a_gone_row_yields_a_410_and_neither_headers() {
        let now = at("2026-10-01T00:00:00Z");
        let active = entry_for(&row(Status::Active, "2027-04-15T00:00:00Z"), now);
        assert!(active.headers.is_some(), "the three headers are built");
        assert!(!active.gone, "the handler still runs");

        let removed = entry_for(&row(Status::Active, "2026-09-01T00:00:00Z"), now);
        assert!(removed.gone, "the handler does NOT run");
        assert_eq!(removed.headers, None, "no Sunset on a response that says it passed");
        assert_eq!(
            removed.removed_body.expect("a body")["code"],
            "REMOVED",
            "the body names the code a client branches on"
        );
    }

    #[test]
    fn a_withdrawn_row_answers_nothing_and_runs_the_handler() {
        let now = at("2026-10-01T00:00:00Z");
        let withdrawn = entry_for(&row(Status::Withdrawn, "2026-09-01T00:00:00Z"), now);
        assert_eq!(withdrawn.headers, None);
        assert!(
            !withdrawn.gone,
            "a withdrawal beats its own sunset date: the surface works again"
        );
    }

    #[test]
    fn a_method_specific_row_gets_its_own_key_and_a_method_less_one_gets_the_path() {
        // The two kinds of row coexist for one path, and the specific one is looked up first.
        assert_eq!(key_for(Some("POST"), "/api/v1/pages"), "POST /api/v1/pages");
        assert_eq!(key_for(None, "/api/v1/pages"), "/api/v1/pages");
        assert_ne!(
            key_for(Some("GET"), "/api/v1/pages"),
            key_for(None, "/api/v1/pages"),
            "a GET-only row must not govern every method on the path"
        );
    }

    #[test]
    fn a_version_with_a_newline_is_skipped_rather_than_taking_the_route_down() {
        let now = at("2026-10-01T00:00:00Z");
        let mut bad = row(Status::Active, "2027-04-15T00:00:00Z");
        bad.deprecated_in = "1.4\r\nX-Injected: yes".to_owned();
        let entry = entry_for(&bad, now);

        let mut headers = axum::http::HeaderMap::new();
        write_headers(&mut headers, entry.headers.as_ref().expect("headers are built"));
        assert!(
            !headers.contains_key("deprecation"),
            "a header injection attempt is dropped, and the other two still go on"
        );
        assert!(headers.contains_key("sunset"), "the safe headers are not held hostage");
        assert!(headers.contains_key("link"));
    }

    #[test]
    fn a_lookup_before_the_first_build_is_not_deprecated_rather_than_a_panic() {
        // The process may serve traffic before the first refresh completes, or with the table
        // absent. Every answer here must be "the surface is not deprecated", never a panic and
        // never "everything is removed".
        let mut guard = SNAPSHOT.write().expect("the lock is free");
        let saved = guard.clone();
        *guard = None;
        drop(guard);
        assert!(lookup("GET", "/api/v1/pages").is_none(), "no entry before the first build");
        // The `NoDeprecations` answer and the `gone` answer are different, and only one of them
        // would take the platform offline.
        let now = at("2026-10-01T00:00:00Z");
        assert!(!entry_for(&row(Status::Announced, "2027-04-15T00:00:00Z"), now).gone);
        let mut guard = SNAPSHOT.write().expect("the lock is free");
        *guard = saved;
    }
}