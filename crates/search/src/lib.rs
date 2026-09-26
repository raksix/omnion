//! Omnion search — the engine behind the platform's one search box.
//!
//! `docs/requests/REQ-002` asks for a single box that finds everything, with a `Ctrl/⌘ + K`
//! command palette on top of it. The design keeps that honest in two pieces:
//!
//! * [`catalogue`] — the **registry** of searchable sources: a stable key, the permission a
//!   caller must hold, and the panel route a hit lives at. Sources are only registered once
//!   their screen exists, so the palette never offers a result that goes nowhere.
//! * [`query`] — the **query and ranking rule**, pure and unit-tested: parse, normalize, score
//!   and order. Nothing in this module touches the database, so the ranking can be reasoned
//!   about in isolation.
//!
//! [`sources`] is the database side — one SQL statement per source, tenant-scoped with the same
//! rule the rest of the panel uses, handing candidates to the shared ranker.
//!
//! The API surface that carries a query is `apps/api/src/routes/search.rs`; the palette that
//! front-ends it is `apps/admin/components/command-palette.tsx`.

#![forbid(unsafe_code)]

pub mod catalogue;
pub mod error;
pub mod query;
pub mod sources;

pub use catalogue::{SEARCH_SOURCES, SourceSpec, source, source_keys};
pub use error::{Result, SearchError};
pub use query::{Hit, Query};
