//! Omnion CRM — the relationship layer of the platform (docs/requests/REQ-051).
//!
//! Three jobs, one crate:
//!
//! * [`contacts`] — companies and people: the validation the forms render as field messages,
//!   the list queries with their filters, the soft archive, and the merge that moves what other
//!   rows point at instead of deleting anything.
//! * [`query`] — the list contract every screen sends (filters, sort, cursor paging) and the
//!   **visibility level**, which is enforced in SQL rather than in the UI.
//! * [`model`] — the shared vocabulary: lifecycle statuses, tag and address shapes, and the
//!   field-hiding rule a list, a detail screen and an export all read.
//! * [`csv`] — the file a person imports and exports: the header mapping, the dry run that names
//!   the line it refuses, and the writers whose output is the importer's own input.
//! * [`views`] — the saved views: a filter, a column set and a sort stored as the query it
//!   stands for, so a view never goes stale.
//!
//! The crate is a **module** (docs/04-MONOREPO.md): a feature the platform can carry behind the
//! `crm.*` permission family, not infrastructure the core depends on. It talks to PostgreSQL and
//! to nothing else — the HTTP shapes, the audit entries and the events live in `apps/api`.

#![forbid(unsafe_code)]

pub mod contacts;
pub mod csv;
pub mod dates;
pub mod deals;
pub mod error;
pub mod model;
pub mod query;
pub mod views;

pub use error::{CrmError, Result};
pub use model::Visibility;
pub use query::{ListQuery, Page, Scope};
