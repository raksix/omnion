//! `omnion-cdn` — the CDN / edge layer (docs/requests/REQ-011).
//!
//! The crate holds everything about caching that is a *decision* rather than an
//! I/O: whether a request may be cached, under which rule, for how long, under
//! which key, and what the response headers must therefore say. Persistence and
//! the purge worker live in `apps/api` on top of these types, so the policy can be
//! tested without a database or a network — which is the reason it is a crate.
//!
//! Layout:
//!
//! * [`matcher`] — the path-pattern language, the request shape and cache-key
//!   derivation.
//! * [`rule`] — a cache rule, its validation, and the ordered evaluation that
//!   turns a request into a [`rule::Decision`].
//! * [`headers`] — a decision rendered into `Cache-Control`, `CDN-Cache-Control`
//!   and surrogate keys.
//! * [`etag`] — the `ETag` and `Vary` that tell a cache *which* stored copy to hand back.
//! * [`provider`] — the adapter seam the purge worker dispatches through, and the
//!   catalogue of adapters that actually ship.
//! * [`purge`] — the queue’s own decisions: target validation, batching, backoff, and the
//!   fold from item outcomes to a parent status.
//! * [`invalidation`] — the automatic half (slice 3): a platform event, the trigger
//!   toggles and the site’s published addresses in, one planned purge out.
//!
//! The purge *adapters* are here; the queue, worker and history that drive them (slice 2)
//! are the persistence layer in `apps/api` on top of [`provider`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod credential;
pub mod error;
pub mod etag;
pub mod headers;
pub mod invalidation;
pub mod matcher;
pub mod provider;
pub mod purge;
pub mod rule;
pub mod store;

pub use error::CdnError;
pub use etag::{etag_for_file, etag_for_page, if_none_match_hits, vary_for};
pub use headers::{headers_for, surrogate_keys};
pub use matcher::{CacheKey, PathPattern, PatternError, RequestShape};
pub use provider::{
    AdapterInfo, CloudflareStyleProvider, GenericHttpProvider, Provider, ProviderSettings, Purge,
    PurgeOutcome, catalogue, is_shipped, provider_for,
};
pub use rule::{Bypass, CacheRule, Decision, RuleError, decide};
