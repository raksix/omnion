//! Emit the OpenAPI document and refuse drift (REQ-130, slice 3).
//!
//! ```text
//! cargo run -p omnion-api --bin openapi-emit          # write or rewrite the snapshot
//! cargo run -p omnion-api --bin openapi-emit --check  # gate: exit 1 on drift or a gap
//! ```
//!
//! ## Why a binary and not a test
//!
//! The acceptance line says *"a CI job fails on an undocumented route or a snapshot drift"*, and
//! both halves are true of this binary rather than of a `#[test]`: `cargo test` output is parsed
//! out of a test harness and a failing test among 380 is one line in a log, whereas this writes
//! the offending operations to stderr and exits non-zero, which is what a pipeline reads.
//!
//! **It builds the router, so the inventory is the real one.** There is no fixture route list and
//! no "expected document" in the source — the document is whatever `routes::router()` actually
//! serves. A fixture would be a second source of truth, which is the drift the gate exists to
//! catch.
//!
//! ## Why the router can be built without a database
//!
//! `routes::router(state)` needs an `AppState`, and this binary must not need one. The state is
//! assembled exactly as `main.rs` does it minus every pool, so a caller who adds a state
//! dependency to a router learns about it here rather than in CI.

use std::process::ExitCode;

fn main() -> ExitCode {
    let check_only = std::env::args().any(|a| a == "--check");
    // A Tokio runtime is required and it is NOT decoration: `PgPool::connect_lazy` builds a pool
    // that installs a background reaper, and without a runtime context it panics with
    // "this functionality requires a Tokio context". Discovered by running it — a sync `main`
    // compiles cleanly and fails on the first call.
    let outcome = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt.block_on(run(check_only)),
        Err(e) => Err(format!("could not start a runtime: {e}")),
    };
    match outcome {
        Ok(code) => code,
        Err(message) => {
            eprintln!("openapi-emit: {message}");
            ExitCode::from(2)
        }
    }
}

async fn run(check_only: bool) -> Result<ExitCode, String> {
    let (routes, registry) = omnion_api::openapi_emit::inventory_and_registry();

    let verdict = omnion_graphql::openapi::check(&routes, &registry);
    let generated =
        omnion_graphql::openapi::canonical_json(&omnion_graphql::openapi::document(&routes, &registry));
    let hash = omnion_graphql::openapi::Drift::openapi_hash(&generated);

    eprintln!(
        "openapi-emit: {} routes, {} documented, {} undocumented, {} orphaned",
        verdict.total,
        verdict.covered,
        verdict.undocumented.len(),
        verdict.orphaned.len(),
    );
    eprintln!("openapi-emit: hash {hash}");

    // Coverage first. A route with no annotation cannot be in the document, so reporting drift
    // against a document that is missing the route would name the wrong problem — the order is
    // the order a reader can act in.
    let mut failed = false;
    if let Some(described) = verdict.describe() {
        eprintln!("openapi-emit: COVERAGE FAILED — {described}");
        failed = true;
    }

    // **A gate that passes on an empty input is worse than no gate.** This one did, on its first
    // real run: every route registers with `.route(get(handler))` rather than through
    // `documented!`, so the inventory came back empty — and an EMPTY router has no undocumented
    // routes, so coverage passed, drift passed against the empty snapshot it had just written,
    // and the binary exited 0 while describing nothing at all. Every acceptance line would have
    // been ticked by a document with `paths: {}`.
    //
    // The floor is not a magic number: it is far below the smallest router this platform could
    // plausibly ship and far above an empty one, so it can only ever fire when the inventory is
    // not being filled — which is the only condition under which the check would be worth nothing.
    //
    // Quoted from the inventory rather than repeated here, so the gate and the test that guards
    // this very condition cannot disagree about what an empty router is.
    if verdict.total < omnion_api::routes::inventory::MINIMUM_ROUTES {
        eprintln!(
            "openapi-emit: INVENTORY FAILED — the router reported {} routes, below the floor of \
             {}. Either no route has adopted documented! yet, or the \
             inventory is being read before the router is assembled. An empty document passes \
             every check this binary makes, which is exactly why it must not be produced.",
            verdict.total,
            omnion_api::routes::inventory::MINIMUM_ROUTES
        );
        failed = true;
    }

    let path = omnion_api::openapi_emit::SNAPSHOT_PATH;
    let existing = tokio::fs::read_to_string(path).await.ok();

    match (&existing, check_only) {
        (Some(text), true) => {
            let drift = omnion_graphql::openapi::Drift::compare(text, &generated);
            if drift.in_sync {
                eprintln!("openapi-emit: snapshot is in sync");
            } else {
                eprintln!(
                    "openapi-emit: DRIFT — {} is out of date. \
                     Run `cargo run -p omnion-api --bin openapi-emit` and commit the result.",
                    path
                );
                failed = true;
            }
        }
        (None, true) => {
            eprintln!(
                "openapi-emit: DRIFT — no committed snapshot at {path}. \
                 Run `cargo run -p omnion-api --bin openapi-emit` and commit the result."
            );
            failed = true;
        }
        (_, false) => {
            if let Some(parent) = std::path::Path::new(path).parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
            }
            std::fs::write(path, &generated)
                .map_err(|e| format!("could not write {path}: {e}"))?;
            eprintln!("openapi-emit: wrote {path}");
        }
    }

    if failed {
        Ok(ExitCode::from(1))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}
