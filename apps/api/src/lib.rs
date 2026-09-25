//! Omnion API crate.
//!
//! This crate is the HTTP layer only: routing, extractors, response shapes and wiring. Real
//! work is delegated to the crates under `crates/` (see `docs/04-MONOREPO.md`).

#![forbid(unsafe_code)]

pub mod auth;
pub mod client_ip;
pub mod cookies;
pub mod dto;
pub mod error;
pub mod guards;
pub mod routes;
pub mod state;
