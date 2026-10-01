//! Omnion developer platform (docs/requests/REQ-033).
//!
//! The developer surface is the one place a person extends Omnion without leaving it: keys
//! they authenticate with, the requests those keys make, and the tooling that turns a template
//! into an extension. It is infrastructure, like `omnion-events` and `omnion-audit` — it knows
//! what a key *is*, not what a deploy or an approval means.
//!
//! # The property this crate is built around
//!
//! **Key material is write-only.** It is minted in [`secret::mint`], handed to the caller once
//! in a [`Minted`] response, hashed into `api_keys.secret_hash` and never recoverable. That
//! is enforced in three independent places, deliberately, because the leak it prevents is the
//! one nobody notices:
//!
//! * [`model::ApiKey`] — the shape a list, a detail read and a CSV export share — has no secret
//!   field. A query written wrong cannot return one, because there is nowhere to put it.
//! * [`model::Minted`] is a separate type that only [`store::create`] and
//!   [`store::rotate`] produce, so "the secret came back a second time" needs a code path that
//!   does not exist rather than a test that has to remember to fail.
//! * The stored hash carries a scheme prefix ([`secret`]), so a row this build cannot read is
//!   an authentication failure — indistinguishable from a wrong secret — instead of a
//!   comparison against a different algorithm.
//!
//! Slice 1 is the keys-and-logs half. The API Explorer, OAuth apps, the events catalog, the
//! SDK scaffolds and the CLI device-code flow are slices 2 through 4, and their tables are not
//! created early to be filled in later.

#![forbid(unsafe_code)]

pub mod authn;
pub mod error;
pub mod model;
pub mod openapi;
pub mod secret;
#[cfg(feature = "store")]
pub mod store;

pub use authn::{
    AuthenticatedKey, KeyRefusal, address_allowed, cidr_contains, decide, scope_allows,
};
pub use error::{DeveloperError, Result};
pub use model::{
    ApiKey, Environment, KeyStatus, Minted, NewKey, RateTier, RequestLog, RequestLogPage,
    RequestLogQuery, UsageDay, key_rules,
};
pub use secret::{MintedKey, mint};

/// The label the API uses for this surface in the permission catalogue and the event bus.
///
/// One constant, because the request names `developer.keys.manage` in a QA step and the route
/// guard, the audit entry and the event name all have to agree on it — and three hand-typed
/// copies of a permission key is three chances to spell one of them differently.
pub const PERMISSION_PREFIX: &str = "developer.";
