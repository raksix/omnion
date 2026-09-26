//! Omnion search — the engine behind the platform's one search box (docs/requests/REQ-002).
//!
//! The design is an **index**, not a scatter of per-table queries. Every searchable thing in
//! the platform becomes one `search_documents` row, written by the provider that owns its
//! domain, maintained from the event bus and rebuildable from scratch by a reindex pass. One
//! row answers the palette, the results screen and (later) the public site's search, so the
//! ranking, the scoping and the permission rules live in exactly one place.
//!
//! * [`providers`] — the registry: one entry per searchable domain (pages, media, users, sites
//!   today) naming its key, its document type and the permission a caller needs to see its
//!   rows. A provider is registered once its domain exists; entities without a module yet
//!   (posts, plugins, themes, orders) register from their own REQ when they ship.
//! * [`query`] — the query language and the search itself: the scoped syntax (`type:`, `site:`,
//!   `owner:`, `before:`/`after:`, `is:`), the PostgreSQL query that ranks with
//!   `ts_rank_cd` over the generated vector plus a `pg_trgm` prefix/near-miss match, and the
//!   scope + permission narrowing that happens **inside** the query (never after paging).
//! * [`indexer`] — the write side: per-provider reindex over the source tables (upsert plus
//!   prune, idempotent by construction) and the event-bus drain that keeps the index fresh.
//!
//! The HTTP shape around it is `apps/api/src/routes/search.rs`; the palette and the results
//! screen are the admin's business (`apps/admin`).

#![forbid(unsafe_code)]

pub mod error;
pub mod indexer;
pub mod providers;
pub mod query;

pub use error::{Result, SearchError};
pub use providers::{PROVIDERS, ProviderSpec, provider, provider_keys};
pub use query::{Hit, HitPage, Query, QueryError, SearchRequest, Sort};
