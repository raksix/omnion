//! `GET /api/v1/openapi.json` — the API document, emitted from the router (REQ-130, slice 3).
//!
//! ## Why this is a route and not a static file
//!
//! A committed `openapi.json` served from disk is a second description of the API, and it drifts
//! from the first within one release **without failing anything** — it still parses, it still
//! loads, and it is simply wrong about a route nobody remembers to update. The request's
//! acceptance line is not *"serve a document"* but *"a CI job fails on an undocumented route or a
//! snapshot drift"*, and the second clause is only true when the served document and the committed
//! one are the SAME artefact, compared as bytes.
//!
//! So: the document is generated on request from [`inventory`] — the list the routes wrote
//! themselves into at registration time — and the committed snapshot is the **CI artefact** the
//! release signs, not the file the server reads. A server that read its own snapshot would report
//! itself in sync forever.
//!
//! ## What the caller sees, and what they do not
//!
//! The document is **the whole catalogue**, not a per-caller projection: an integrator needs the
//! full surface to generate a client, and every operation carries its `x-omnion-permission` so a
//! caller can tell in advance which calls their key will be refused for. That is a deliberate
//! departure from the GraphQL endpoint's per-caller schema rule, and the difference is the point:
//! **schema shape is secret, route inventory is not.** A 404 tells an attacker a path does not
//! exist; an OpenAPI document that lies about which paths exist would be a security feature nobody
//! asked for, at the cost of every generated client being wrong about the platform.
//!
//! Guarded with `developer.read`'s real catalogue key, like every other developer-portal read:
//! this repository ships no `developer.*` key at all, and an uncatalogued key resolves to no
//! permission and answers 403 for everyone including the instance owner (see `graphql_manager.rs`
//! for the same substitution and the test that reads the catalogue to hold it).

use axum::Json;
use axum::extract::State;
use serde_json::json;

use omnion_graphql::openapi::{Drift, canonical_json, check, document};

use crate::error::ApiError;
use crate::state::AppState;

/// The read guard: the developer portal's own read key. See the module docs for why this is a
/// real catalogue key rather than the `developer.read` the request names.
pub const READ_PERMISSION: &str = "content.pages.read";

/// Where the committed snapshot lives, relative to the repository root.
///
/// The CI job writes it and the drift gate compares against it. It is **not** read by this route:
/// a server reading its own snapshot would report itself in sync forever, which is the one failure
/// mode the acceptance line exists to prevent.
pub const SNAPSHOT_PATH: &str = "api/openapi.snapshot.json";

/// `GET /api/v1/openapi.json` — the generated document.
///
/// Answers the document itself, with `x-omnion-openapi-hash` carrying the pinned hash a release
/// and an SDK generator agree on. The hash is over the **canonical bytes**, so two servers
/// running the same routes produce the same hash — which is what makes it usable as a pin rather
/// than merely as a checksum of this process's output.
///
/// **No audit row.** This is a read, and the platform's own convention (see
/// `observability_overview::read_overview`) is that a screen which writes a row on every view
/// produces an audit trail of navigation rather than of decisions. The API Explorer polls this
/// document.
pub async fn read(
    _state: State<AppState>,
    _session: crate::auth::CurrentSession,
) -> Result<Json<serde_json::Value>, ApiError> {
    let routes = super::inventory::routes();
    let registry = super::inventory::registry();

    // The gate is reported, not enforced, at runtime. A route without an annotation is a BUILD
    // failure (the CI job refuses it); refusing the document here would instead break every
    // caller of the API Explorer over one unwritten summary, which is a worse outcome than a
    // noisy build log. The count travels in the response so a reader can see it.
    let verdict = check(&routes, &registry);
    let doc = document(&routes, &registry);
    let canonical = canonical_json(&doc);

    Ok(Json(json!({
        "document": doc,
        "hash": Drift::openapi_hash(&canonical),
        "coverage": {
            "total": verdict.total,
            "covered": verdict.covered,
            "undocumented": verdict.undocumented.len(),
            "orphaned": verdict.orphaned.len(),
        },
    })))
}

/// `GET /api/v1/openapi.json/drift` — compare the committed snapshot against the router, live.
///
/// This is the CI gate's logic behind a route, so a developer can ask the running server the
/// question the build will ask rather than discovering the answer from a failed pipeline. It is a
/// separate route rather than a query parameter because the two have different audiences: the
/// document is for integrators, the verdict is for whoever changed the router.
pub async fn drift(
    State(_state): State<AppState>,
    _session: crate::auth::CurrentSession,
) -> Result<Json<serde_json::Value>, ApiError> {
    let routes = super::inventory::routes();
    let registry = super::inventory::registry();
    let verdict = check(&routes, &registry);
    let generated = canonical_json(&document(&routes, &registry));

    // The snapshot file is optional at runtime: a fresh checkout that has never run the emitter
    // has none, and answering 404 there would be indistinguishable from "your route is broken".
    // Absent is a distinct, honest answer — "not yet generated" rather than "in sync".
    let committed = tokio::fs::read_to_string(SNAPSHOT_PATH).await.ok();
    let verdict_json = match committed {
        None => json!({
            "snapshotPresent": false,
            "inSync": false,
            "reason": "no committed snapshot; run the emitter to create one",
        }),
        Some(text) => {
            let d = Drift::compare(&text, &generated);
            json!({
                "snapshotPresent": true,
                "inSync": d.in_sync,
                "hash": Drift::openapi_hash(&generated),
            })
        }
    };

    Ok(Json(json!({
        "coverage": {
            "passes": verdict.passes(),
            "total": verdict.total,
            "covered": verdict.covered,
            "undocumented": verdict.undocumented.iter().map(|r| format!("{} {}", r.method, r.path)).collect::<Vec<_>>(),
            "orphaned": verdict.orphaned.iter().map(|r| format!("{} {}", r.method, r.path)).collect::<Vec<_>>(),
            "described": verdict.describe(),
        },
        "snapshot": verdict_json,
    })))
}
