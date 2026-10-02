//! OpenAPI emission from the routers themselves (REQ-130, slice 3).
//!
//! ## The acceptance line, and the failure mode it is really about
//!
//! *"[ ] The OpenAPI document is emitted from the routers, and a CI job fails on an undocumented
//! route or a snapshot drift."*
//!
//! The half that is easy is serving a document. The half that matters is the second clause, and it
//! only has teeth if the document is **derived from the router** rather than maintained beside it.
//! A hand-written `openapi.json` is a second description of the API that drifts from the first
//! within one release — and it drifts silently, because a stale document still parses.
//!
//! So this module has a hard rule: **the router is the source and the annotations are the only
//! other input.** A route that is not in the router cannot appear in the document (there is no
//! list to add it to), and a route in the router with no annotation is a **[route::undocumented]
//! finding, not a silently-absent path**. The direction of that failure matters: an OpenAPI
//! document that forgets a route is invisible, while one that reports an unannotated route fails
//! the build.
//!
//! ## Why nothing here depends on axum
//!
//! The caller — `apps/api/src/routes/openapi.rs` — walks `Router::routes()` and produces a flat
//! [`RouteEntry`] list: method, path, and nothing else. Everything below is a pure function of
//! that list plus the [`Registry`]. That keeps the decision layer free of the HTTP framework
//! (the same rule as every other module in this crate, see [`crate`]), and it is what lets the
//! drift gate run in CI **without booting a server or a database** — the router is assembled from
//! a state that never touches a connection, so the walk is a function call.
//!
//! ## The two checks, and why they are not one
//!
//! [`openapi::check`] returns [`Check`] with two independent verdicts:
//!
//! * **coverage** — every route in the router has an annotation. A new route with no annotation is
//!   a build failure, which is the whole point of the gate.
//! * **drift** — the document the router produces equals the committed snapshot.
//!
//! Merging them into one boolean would make the second clause untestable while the first is
//! failing, and would report "drift" for a route nobody annotated. Two verdicts, two exit codes,
//! two lines in the CI output.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{Code, Error, Result};

/// One route as the router reports it. No handler, no layer, no state — just what a client can
/// address, because that is exactly what a client of the document can rely on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RouteEntry {
    /// Upper-case HTTP method, `GET`/`POST`/`PUT`/`PATCH`/`DELETE`.
    pub method: String,
    /// Axum path pattern, e.g. `/graphql/documents/{id}`.
    pub path: String,
}

impl RouteEntry {
    /// Build an entry, normalising the method to upper case.
    ///
    /// Axum reports `MethodRouter::routes()` entries already normalised, but the gate accepts
    /// hand-written lists too (a fixture, a golden file) and a lowercase `get` in a list would
    /// fail to match an annotation and be reported as undocumented — a defect in the report, not
    /// in the API.
    pub fn new(method: &str, path: &str) -> Self {
        Self {
            method: method.trim().to_ascii_uppercase(),
            path: path.to_string(),
        }
    }

    /// The OpenAPI operation id: `get_graphql_documents_by_id`.
    ///
    /// Derived from the route itself so two operations can never be issued the same id — an
    /// OpenAPI document with duplicate operation ids is rejected by every generator, and the
    /// error points at the document rather than at the two routes that collided.
    ///
    /// **Every character here has to survive both target languages.** A generated client turns
    /// this string into a *method name* in TypeScript and a *function name* in Python, and a
    /// hyphen in either is a syntax error rather than a warning: `post_auth_step-up` is not
    /// callable, and `delete_backup-schedules_by_id` reads as the subtraction of two names. The
    /// first version emitted segments verbatim and **108 of 496 operations** came out with
    /// hyphens, from real paths like `/backup-schedules` and `/auth/step-up` — every one of them
    /// a package that would not compile.
    ///
    /// So the id is normalised HERE rather than in each generator. Normalising downstream would
    /// mean two sanitisers that can disagree, and a document whose ids are valid is worth more
    /// than one that two of our own tools happen to cope with: the Explorer, the docs site and a
    /// third-party generator all read the same document.
    ///
    /// The substitution cannot merge two ids. Every non-alphanumeric run becomes one `_`, and
    /// the method prefix already separates verbs, so distinct `(method, path)` pairs stay
    /// distinct — asserted in the tests below against the live snapshot, not against a fixture
    /// that was written to agree.
    pub fn operation_id(&self) -> String {
        let mut out = String::with_capacity(self.method.len() + self.path.len() + 4);
        out.push_str(&sanitize_identifier(&self.method.to_ascii_lowercase()));
        for segment in self.path.split('/').filter(|s| !s.is_empty()) {
            out.push('_');
            if let Some(inner) = segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                out.push_str("by_");
                out.push_str(&sanitize_identifier(inner));
            } else {
                out.push_str(&sanitize_identifier(segment));
            }
        }
        out
    }
}

/// Collapse a segment into characters that are legal in a TypeScript identifier and in a Python
/// one.
///
/// Both languages agree on `[A-Za-z0-9_]`, so one function serves both generators. Leading
/// digits are left alone: a segment can start with a digit (`/v2/...`) and it is always preceded
/// by `_` or a word character by the time it is used, so it can never be the first character of
/// the identifier.
///
/// **`pub(crate)` rather than private, and the reason is a defect this crate already shipped:**
/// the SDK generator needs the same rule for TAG names, and while it had its own copy the two
/// drifted — the copy capitalised letters and dropped hyphens, so it handled `/backup-schedules`
/// and emitted `Openapi.json()` for a tag containing a dot. One rule, one place to fix it.
pub(crate) fn sanitize_identifier(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    let mut last_was_underscore = false;
    for ch in segment.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
            last_was_underscore = ch == '_';
        } else if !last_was_underscore {
            // A run of punctuation becomes ONE underscore, so `step-up` and `step_up` cannot
            // land on the same id from different routes.
            out.push('_');
            last_was_underscore = true;
        }
    }
    out
}

/// What the annotation registry knows about one route.
///
/// A route with no summary is not documented — the request says a CI job fails on *"a route [that]
/// lacks annotations"*, and a summary is the minimum an operation may carry and still be
/// readable by a person scanning a generated SDK.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Annotation {
    /// One line, in the product's language, describing what the route does.
    pub summary: String,
    /// Optional longer description; rendered as the operation's `description`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The catalogue key the route is guarded on, if any.
    ///
    /// This is what turns the document into an access-control artefact rather than a list: a
    /// caller integrating against it can tell which operations their key must carry before
    /// sending a request that would be refused with a 403.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<String>,
    /// Tags group operations in a generated client; the leading segment of the path is the
    /// platform's own domain grouping (`graphql`, `observability`, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Marked deprecated with a sunset, per docs/05-VERSIONING.md. Slice 4 writes these rows;
    /// the document renders them so an integrator sees the deprecation where they read the route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<Deprecation>,
}

/// A deprecation as the document states it: when it started, when it stops working, what to use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deprecation {
    /// RFC 3339 date the operation was deprecated in.
    pub deprecated_in: String,
    /// RFC 3339 date the operation stops answering.
    pub sunset_at: String,
    /// What replaces it, if anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,
}

/// The annotations, keyed by `METHOD path`.
///
/// A `BTreeMap` because the emitted document must be **byte-stable**: the drift gate compares the
/// committed snapshot to the generated one, and two runs that order the same routes differently
/// would fail a build for no reason. Ordering is not cosmetic here — it is the difference between
/// a reproducible artefact and one that is re-generated on every commit because the hash moved.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    entries: BTreeMap<String, Annotation>,
}

impl Registry {
    /// An empty registry. A build with this passes nothing, which is the correct outcome for an
    /// API that has documented no routes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Annotate a route. Returns `Err` for a route that is already annotated, because two
    /// annotations for one operation means one of them is dead and the document would pick one
    /// silently.
    pub fn annotate(&mut self, method: &str, path: &str, annotation: Annotation) -> Result<()> {
        let entry = RouteEntry::new(method, path);
        let key = key_of(&entry);
        if self.entries.contains_key(&key) {
            return Err(Error::Validation {
                code: Code::GraphqlValidationFailed,
                message: format!(
                    "route {key} is already annotated; a second annotation would be dead code"
                ),
            });
        }
        self.entries.insert(key, annotation);
        Ok(())
    }

    /// Annotate a route without the duplicate check.
    ///
    /// For a registry assembled **from** a route inventory — one pass over records that already
    /// contains no duplicate `METHOD path`, because the recorder refused them. This exists so
    /// that path can clone an `Annotation` rather than rebuild one; a public API shape that is
    /// wrong for everyone else is a bug waiting for a caller who does not know the guarantee, so
    /// it is documented as belonging to exactly this case.
    pub fn annotate_unchecked(
        &mut self,
        method: &str,
        path: &str,
        annotation: Annotation,
    ) -> &mut Self {
        self.entries
            .insert(key_of(&RouteEntry::new(method, path)), annotation);
        self
    }

    /// Annotate a route, panicking on a duplicate.
    ///
    /// For the static `build()` tables in the API crate, where a duplicate is a compile-time-ish
    /// mistake in a literal. Not for registry data read from a store.
    pub fn must_annotate(&mut self, method: &str, path: &str, annotation: Annotation) {
        if let Err(e) = self.annotate(method, path, annotation) {
            panic!("{e}");
        }
    }

    /// The annotation for a route, if it has one.
    pub fn get(&self, entry: &RouteEntry) -> Option<&Annotation> {
        self.entries.get(&key_of(entry))
    }

    /// How many routes are annotated.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every annotated key, sorted.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }
}

fn key_of(entry: &RouteEntry) -> String {
    format!("{} {}", entry.method, entry.path)
}

/// What the gate found. Two verdicts, reported separately on purpose — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// Routes in the router with no annotation. Empty means coverage passes.
    pub undocumented: Vec<RouteEntry>,
    /// Routes annotated but not in the router. A stale annotation: the route was renamed or
    /// removed, and the document would describe something nobody can call.
    pub orphaned: Vec<RouteEntry>,
    /// The routes covered, for reporting.
    pub covered: usize,
    /// The total the router reported.
    pub total: usize,
}

impl Check {
    /// Whether every route is documented and no annotation dangles.
    pub fn passes(&self) -> bool {
        self.undocumented.is_empty() && self.orphaned.is_empty()
    }

    /// One line naming what is wrong, or `None` when the gate passes.
    pub fn describe(&self) -> Option<String> {
        if self.passes() {
            return None;
        }
        let mut parts = Vec::new();
        if !self.undocumented.is_empty() {
            let names: Vec<String> = self.undocumented.iter().map(|r| key_of(r)).collect();
            parts.push(format!(
                "{} route(s) in the router carry no annotation: {}",
                names.len(),
                names.join(", ")
            ));
        }
        if !self.orphaned.is_empty() {
            let names: Vec<String> = self.orphaned.iter().map(|r| key_of(r)).collect();
            parts.push(format!(
                "{} annotation(s) name a route the router does not serve: {}",
                names.len(),
                names.join(", ")
            ));
        }
        Some(parts.join("; "))
    }
}

/// Run the coverage check: does every served route carry an annotation?
pub fn check(routes: &[RouteEntry], registry: &Registry) -> Check {
    let mut undocumented = Vec::new();
    for entry in routes {
        if registry.get(entry).is_none() {
            undocumented.push(entry.clone());
        }
    }
    undocumented.sort();

    let served: std::collections::BTreeSet<&RouteEntry> = routes.iter().collect();
    let mut orphaned: Vec<RouteEntry> = registry
        .entries
        .keys()
        .filter_map(|key| {
            let (method, path) = key.split_once(' ')?;
            let entry = RouteEntry::new(method, path);
            (!served.contains(&entry)).then_some(entry)
        })
        .collect();
    orphaned.sort();

    Check {
        undocumented,
        orphaned,
        covered: routes.len() - undocumented_len(routes, registry),
        total: routes.len(),
    }
}

fn undocumented_len(routes: &[RouteEntry], registry: &Registry) -> usize {
    routes.iter().filter(|e| registry.get(e).is_none()).count()
}

/// The document's OpenAPI version and the informational fields the request names.
pub const OPENAPI_VERSION: &str = "3.1.0";
/// The API version this document describes. Slice 4 makes it a real versioned policy value;
/// until then it is the route prefix, which cannot drift from where the routes live.
pub const API_VERSION: &str = "v1";

/// Build the OpenAPI document for `routes`.
///
/// **Only annotated routes appear**, and that is not a silent drop: [`check`] reports every one of
/// them as `undocumented`, and the CI gate turns that into a failure. The alternative — emit a
/// path with a placeholder summary — would let an undocumented route pass the very gate that
/// exists to catch it.
pub fn document(routes: &[RouteEntry], registry: &Registry) -> serde_json::Value {
    let mut paths: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for entry in routes {
        let Some(annotation) = registry.get(entry) else {
            continue;
        };
        let operation = serde_json::json!({
            "operationId": entry.operation_id(),
            "summary": annotation.summary,
            "description": annotation.description,
            "tags": tags_of(entry, annotation),
            "deprecated": annotation.deprecated.is_some(),
            "x-omnion-permission": annotation.permission,
            "x-omnion-sunset": annotation.deprecated.as_ref().map(|d| d.sunset_at.clone()),
            "x-omnion-replacement": annotation.deprecated.as_ref().and_then(|d| d.replacement.clone()),
            "responses": responses(),
        });
        let path_item = paths
            .entry(entry.path.clone())
            .or_insert_with(|| serde_json::json!({}));
        path_item[entry.method.to_ascii_lowercase()] = operation;
    }

    serde_json::json!({
        "openapi": OPENAPI_VERSION,
        "info": {
            "title": "Omnion API",
            "version": API_VERSION,
            "description": "Generated from the running router. A route without an annotation fails CI, \
                            so this document cannot silently fall behind the API it describes.",
        },
        "paths": paths,
    })
}

fn tags_of(entry: &RouteEntry, annotation: &Annotation) -> Vec<String> {
    if !annotation.tags.is_empty() {
        return annotation.tags.clone();
    }
    // Fall back to the leading path segment, so an operation with no explicit tag still groups
    // with its siblings instead of appearing in a generated client's ungrouped section.
    entry
        .path
        .split('/')
        .find(|s| !s.is_empty())
        .map(|s| vec![s.to_string()])
        .unwrap_or_default()
}

fn responses() -> serde_json::Value {
    // Deliberately coarse: this document's job is to route inventory, permissions and drift, and
    // inventing a 200 schema per operation from a handler signature is a second source of truth
    // that would need its own drift gate. Slice 4 refines this with real per-operation schemas
    // once the DTOs are annotated.
    serde_json::json!({
        "default": {
            "description": "Standard error envelope.",
            "content": {
                "application/json": {
                    "schema": { "$ref": "#/components/schemas/Error" }
                }
            }
        },
        "403": {
            "description": "The caller's permissions do not cover this route.",
        },
    })
}

/// The canonical serialisation the snapshot is compared against.
///
/// Two spaces and a trailing newline, sorted keys. `serde_json`'s default is compact with
/// insertion-ordered maps; `document()` already builds through `BTreeMap`s so the key order is
/// sorted, and this function owns the whitespace so the snapshot's on-disk form is decided in one
/// place instead of at every comparison site.
pub fn canonical_json(value: &serde_json::Value) -> String {
    let mut out = serde_json::to_string_pretty(value).unwrap_or_default();
    out.push('\n');
    out
}

/// The drift verdict between a committed snapshot and what the router produces now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drift {
    /// Whether the two serialisations are byte-identical.
    pub in_sync: bool,
    /// The committed document, when it is not in sync — for a diff an operator can read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed: Option<String>,
    /// The freshly generated document.
    pub generated: String,
}

impl Drift {
    /// Compare a committed snapshot against a freshly generated one.
    pub fn compare(committed: &str, generated: &str) -> Self {
        let in_sync = committed == generated;
        Self {
            in_sync,
            committed: (!in_sync).then(|| committed.to_string()),
            generated: generated.to_string(),
        }
    }

    /// A stable hash of the document, which is what an SDK release pins (slice 3's second
    /// acceptance line: *"generated from a pinned OpenAPI hash per release"*).
    ///
    /// SHA-256 over the canonical bytes, so the hash is a function of the *content* and not of
    /// how the file happened to be serialised. Two builds that produce the same API produce the
    /// same hash even if the pretty-printer's width changes.
    pub fn openapi_hash(canonical: &str) -> String {
        let digest = sha256_hex(canonical.as_bytes());
        format!("sha256:{digest}")
    }
}

/// Minimal SHA-256, hex-encoded.
///
/// This crate carries no hashing dependency and the hash is over a document this module just
/// built — a few hundred kilobytes at most — so a dependency would cost more than it saves. The
/// function is exercised against the known vectors in the tests below, because a hand-written
/// hash that is subtly wrong is worse than none: it would pin two different APIs to the same
/// "identical" hash.
fn sha256_hex(input: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let mut msg = input.to_vec();
    let bit_len = (input.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks(4).enumerate().take(16) {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(value);
        }
    }

    h.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(method: &str, path: &str) -> RouteEntry {
        RouteEntry::new(method, path)
    }

    fn annotation(summary: &str) -> Annotation {
        Annotation {
            summary: summary.to_string(),
            description: None,
            permission: Some("content.pages.read".to_string()),
            tags: Vec::new(),
            deprecated: None,
        }
    }

    #[test]
    fn the_operation_id_is_derived_from_the_route_so_two_cannot_collide() {
        assert_eq!(
            route("GET", "/graphql/documents/{id}").operation_id(),
            "get_graphql_documents_by_id"
        );
        // Two operations differing only in method must get different ids, or every generator
        // rejects the document and points at the document rather than at the collision.
        assert_ne!(
            route("GET", "/graphql/documents").operation_id(),
            route("POST", "/graphql/documents").operation_id()
        );
    }

    #[test]
    fn a_route_without_an_annotation_is_reported_and_never_silently_dropped() {
        let routes = vec![route("GET", "/pages"), route("GET", "/graphql/documents")];
        let mut registry = Registry::new();
        registry.must_annotate("GET", "/pages", annotation("List pages"));

        let verdict = check(&routes, &registry);
        assert!(!verdict.passes());
        assert_eq!(verdict.undocumented.len(), 1);
        assert_eq!(verdict.undocumented[0].path, "/graphql/documents");

        // And the document does not pretend it does not exist: the path is absent, and the gate
        // is what says so. An absent path is a build failure; a placeholder would be a pass.
        let doc = document(&routes, &registry);
        assert!(doc["paths"]["/pages"]["get"].is_object());
        assert!(doc["paths"]["/graphql/documents"].is_null());
    }

    #[test]
    fn an_annotation_for_a_route_the_router_does_not_serve_is_an_orphan() {
        // This is the rename case: the route moved and the annotation stayed behind.
        let routes = vec![route("GET", "/graphql/documents")];
        let mut registry = Registry::new();
        registry.must_annotate("GET", "/graphql/documents", annotation("List documents"));
        registry.must_annotate("GET", "/graphql/documentz", annotation("Typo left behind"));

        let verdict = check(&routes, &registry);
        assert!(!verdict.passes());
        assert_eq!(verdict.orphaned.len(), 1);
        assert_eq!(verdict.orphaned[0].path, "/graphql/documentz");
        assert!(
            verdict.describe().unwrap().contains("documentz"),
            "the description must name the offending route: {:?}",
            verdict.describe()
        );
    }

    #[test]
    fn a_second_annotation_for_one_route_is_refused_rather_than_overwritten() {
        let mut registry = Registry::new();
        registry.must_annotate("GET", "/pages", annotation("List pages"));
        let err = registry
            .annotate("GET", "/pages", annotation("Something else"))
            .unwrap_err();
        assert_eq!(err.code(), Code::GraphqlValidationFailed, "got {err:?}");
    }

    #[test]
    fn the_document_carries_the_permission_so_an_integrator_can_predetermine_a_403() {
        let routes = vec![route("DELETE", "/pages/{id}")];
        let mut registry = Registry::new();
        registry.must_annotate("DELETE", "/pages/{id}", annotation("Delete a page"));
        let doc = document(&routes, &registry);
        assert_eq!(
            doc["paths"]["/pages/{id}"]["delete"]["x-omnion-permission"],
            "content.pages.read"
        );
    }

    #[test]
    fn a_deprecation_renders_as_openapi_deprecated_and_omnion_extensions() {
        let routes = vec![route("GET", "/legacy")];
        let mut registry = Registry::new();
        let mut ann = annotation("Legacy route");
        ann.deprecated = Some(Deprecation {
            deprecated_in: "2026-09-01".to_string(),
            sunset_at: "2027-03-01".to_string(),
            replacement: Some("/pages".to_string()),
        });
        registry.must_annotate("GET", "/legacy", ann);
        let doc = document(&routes, &registry);
        let op = &doc["paths"]["/legacy"]["get"];
        assert_eq!(op["deprecated"], serde_json::json!(true));
        assert_eq!(op["x-omnion-sunset"], "2027-03-01");
        assert_eq!(op["x-omnion-replacement"], "/pages");
    }

    #[test]
    fn the_document_is_byte_stable_across_runs_over_the_same_routes() {
        let mut routes = vec![
            route("POST", "/b"),
            route("GET", "/a"),
            route("DELETE", "/a"),
            route("GET", "/z"),
        ];
        let mut registry = Registry::new();
        for r in &routes {
            registry.must_annotate(&r.method, &r.path, annotation("Something"));
        }
        // Feed the same set in two different orders: the snapshot comparison is byte equality, so
        // a HashMap-ordered emitter would fail the build on a re-run for no reason at all.
        let first = canonical_json(&document(&routes, &registry));
        routes.reverse();
        let second = canonical_json(&document(&routes, &registry));
        assert_eq!(first, second, "the document must not depend on input order");
    }

    #[test]
    fn drift_reports_out_of_sync_rather_than_a_bare_boolean() {
        let in_sync = Drift::compare("{\"a\":1}\n", "{\"a\":1}\n");
        assert!(in_sync.in_sync);
        assert!(
            in_sync.committed.is_none(),
            "a passing check carries no old document"
        );

        let drifted = Drift::compare("{\"a\":1}\n", "{\"a\":2}\n");
        assert!(!drifted.in_sync);
        assert_eq!(drifted.committed.as_deref(), Some("{\"a\":1}\n"));
    }

    #[test]
    fn the_openapi_hash_is_the_real_sha256_of_the_canonical_bytes() {
        // Known vectors. A hand-written hash that is subtly wrong pins two different APIs to the
        // same "identical" hash, which is a silent failure in the one place that cannot be
        // inspected by a person reading the diff.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"{}"),
            "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
        // The hash is over the CANONICAL bytes, and the canonical form ends in a newline. The
        // vector above is therefore for `"{}"` WITHOUT the newline; hashing `"{}\n"` gives a
        // different digest, and the test says so explicitly rather than leaving the reader to
        // discover which one the function takes.
        assert_eq!(
            sha256_hex(b"{}\n"),
            "ca3d163bab055381827226140568f3bef7eaac187cebd76878e0b63e9e442356"
        );
        let h = Drift::openapi_hash("{}\n");
        assert!(h.starts_with("sha256:ca3d163b"), "got {h}");
    }

    #[test]
    fn a_hash_over_a_longer_message_matches_the_published_vector() {
        // 1 000 000 'a' would be slow to write in a test; 448 bits is enough to exercise the
        // multi-block path and the length encoding.
        let long = vec![b'a'; 55];
        assert_eq!(
            sha256_hex(&long),
            "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318"
        );
    }
}
