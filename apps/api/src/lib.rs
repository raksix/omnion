//! Omnion API crate.
//!
//! This crate is the HTTP layer only: routing, extractors and wiring. Real work is
//! delegated to the crates under `crates/` (see `docs/04-MONOREPO.md`).

#![forbid(unsafe_code)]

pub mod routes;
pub mod state;
