//! Omnion HR — people operations (docs/requests/REQ-055).
//!
//! Slice 1 is the **people core**, and it is deliberately the part that everything else in the
//! request hangs off: an employee, a department and the org chart they make. Leave, attendance
//! and onboarding are all rows that point at an employee, so the one thing that had to be right
//! first is the employee record and the rules that decide who may read it.
//!
//! * [`model`] — the shared vocabulary: employment types and statuses, the visibility level, and
//!   the **field-hiding rule** the list, the detail screen and the export all read.
//! * [`departments`] — the tree, the cycle refusal and the org chart the tree and the chart are
//!   both read from.
//! * [`employees`] — create/update/terminate, the manager-chain cycle refusal, and the list query
//!   with its filters, built as SQL so the visibility level filters the *totals* too.
//!
//! The crate is a **module** (docs/04-MONOREPO.md): a feature the platform can carry behind the
//! `hr.*` permission family, not infrastructure the core depends on. It talks to PostgreSQL and
//! to nothing else — the HTTP shapes, the audit entries and the events live in `apps/api`.
//!
//! **Payroll is not in the schema, on purpose.** The request's risk note asks for salary and bank
//! details to stay out entirely, so a future payroll request brings its own controls instead of
//! inheriting this module's.

#![forbid(unsafe_code)]

pub mod attendance;
pub mod dates;
pub mod departments;
pub mod employees;
pub mod error;
pub mod leave;
pub mod me;
pub mod model;
pub mod requests;
pub mod store;

pub use error::{HrError, Result};
pub use model::Visibility;
pub use store::{MAX_PER_PAGE, Page};
