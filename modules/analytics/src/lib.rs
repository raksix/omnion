//! Omnion analytics — the platform's own measurement engine (docs/requests/REQ-007).
//!
//! Four jobs, one crate:
//!
//! * [`collect`] — one batched beacon per page becomes raw rows: a visit, its pageviews and its
//!   events. Everything the privacy settings promise (cookieless counting, bot filtering,
//!   `Do Not Track` and `Global Privacy Control`, path and address exclusions, sampling) is
//!   decided *before* anything is written, and a dropped beacon is counted in the day's
//!   `filtered` bucket instead of disappearing without a trace.
//! * [`rollup`] — the worker that rebuilds the hourly and daily buckets from those raw rows.
//!   A bucket run deletes its own rows and writes the aggregate it just computed, so running the
//!   same bucket twice leaves the table byte-for-byte identical.
//! * [`settings`] — the per-site configuration: tracking, privacy, retention, exclusions — with
//!   the validation the settings screen renders as field errors.
//! * [`visitor`], [`agent`] — the pieces the promises are made of: the daily-salted visitor hash,
//!   address truncation, exclusion matching, and the user-agent reading bots and devices.
//!
//! The crate is a **module** (docs/04-MONOREPO.md): a feature the platform can carry behind the
//! `analytics.*` permission family, not infrastructure the core depends on. It talks to
//! PostgreSQL and to nothing else — the HTTP shapes live in `apps/api`.

#![forbid(unsafe_code)]

pub mod agent;
pub mod collect;
pub mod error;
pub mod model;
pub mod rollup;
pub mod settings;
pub mod visitor;

pub use error::{AnalyticsError, Result};
pub use model::{Settings, SettingsChanges};
