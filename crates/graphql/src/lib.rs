//! Omnion GraphQL (docs/requests/REQ-130-graphql-and-sdk-generation.md, slice 1).
//!
//! The request's headline risk is named in its own notes: *"Parity drift between REST and
//! GraphQL is the headline risk: one service layer, one guard chain, and a test that asserts equal
//! outcomes for the same permission set."* The crate is arranged so that risk has nowhere to hide:
//! it contains **no resolver and no HTTP handler**, only the decisions both transports make.
//!
//! * [`document`] — just enough GraphQL to read a document before executing it: operations,
//!   fields, aliases, arguments, fragments. A partial parser that fails closed.
//! * [`limits`] — the pre-execution refusals: depth, cost, aliases, fragments, page size. Each
//!   with its own error code, because a client must branch precisely.
//! * [`cost`] — the weights, with the reason each one costs what it does. An unpriced field is
//!   refused rather than free.
//! * [`parity`] — the **closed** permission vocabulary this surface may name. Not an internal
//!   detail: it is what stops a field from being gated on a permission the platform does not ship.
//! * [`schema`] — the per-caller schema composition, and the cache key that keeps one role's
//!   schema from being served to another.
//! * [`settings`] — what an operator may change, and the validation the playground's cost meter
//!   and the endpoint share so the meter can refuse before it sends.
//!
//! ## The rule every module obeys
//!
//! **The decision is pure and its inputs are visible in its answer.** Nothing here reaches for a
//! database, a clock or a socket; the endpoint passes the document, the caller's permissions and
//! the settings, and gets back a decision it can also show in a cost meter. That is what makes
//! *"the playground refuses over-budget queries and explains the top cost contributors"* provable
//! without a browser, and it is why slice 2's persistence lives in its own module rather than
//! threaded through these functions.
//!
//! ## The one check this crate cannot perform, and where it lives instead
//!
//! [`parity`] closes the permission vocabulary, so a typo like `"content.read"` is a compile
//! error. It does **not** stop a *fabrication*: a new variant whose string nobody checked would
//! compile, and — measured, not assumed — one was added during development (`"billing.read"`) and
//! all of this crate's own tests stayed green, because nothing here can read the platform's
//! catalogue to disagree.
//!
//! So the gate that actually catches it lives in `apps/api/tests/graphql_parity.rs`, in a crate
//! that links both sides. That is the same split as above applied to a test: keep the decision
//! pure, keep the cross-boundary check where both halves are visible.

#![forbid(unsafe_code)]

pub mod cost;
pub mod document;
pub mod error;
pub mod limits;
pub mod parity;
pub mod persisted;
pub mod schema;
pub mod settings;

pub use cost::{Catalogue, Priced, Weight, price};
pub use document::{
    Document, Field, MAX_DOCUMENT_BYTES, Operation, OperationKind, PARSE_DEPTH_CEILING, Selection,
    parse,
};
pub use error::{Code, Error, Result};
pub use limits::{CONTRIBUTOR_COUNT, Limits, Measurement, check, measure_operation};
pub use parity::{ALL as KNOWN_PERMISSIONS, Known, PermissionSet, VARIANT_COUNT};
pub use schema::{CacheKey, ComposedSchema, SchemaCatalogue, TypeDefinition};
pub use settings::Settings;

/// The `extensions` block every response carries, whatever the outcome.
///
/// The request's acceptance line: *"Responses carry `extensions` with depth, cost, duration and
/// request id, verified on a real call."* Assembled in one place so a successful response and a
/// refused one cannot drift — a client that parses `extensions` should not need to know which
/// branch produced it.
pub fn extensions(
    measurement: &Measurement,
    duration_ms: u64,
    request_id: &str,
) -> serde_json::Value {
    serde_json::json!({
        "depth": measurement.depth,
        "cost": measurement.cost,
        "durationMs": duration_ms,
        "requestId": request_id,
        "aliases": measurement.aliases,
        "pageSize": measurement.page_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extensions_block_carries_exactly_what_the_request_names() {
        let doc = parse("{ org: organization(first: 5) { id name } }").expect("parses");
        let measurement = check(&doc, &Limits::default()).expect("within the limits");
        let ext = extensions(&measurement, 12, "req-1");
        // Depth, cost, duration and request id are the four the acceptance line names.
        assert!(ext["depth"].is_u64());
        assert!(ext["cost"].is_u64());
        assert_eq!(ext["durationMs"], 12);
        assert_eq!(ext["requestId"], "req-1");
    }
}
