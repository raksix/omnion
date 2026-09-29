//! `omnion-environment` — staging environments and promotion (docs/requests/REQ-017).
//!
//! Staging here is a **content-and-configuration environment inside one installation**: a second
//! copy of an organization's addressable content that the panel can enter, edit and later promote
//! back. It is deliberately not infrastructure duplication — one database, one deployment, two
//! environments — and the crate's types say so at every point where a name might imply otherwise.
//!
//! What lives here is the part that is a *decision* rather than an I/O: which environment a row
//! belongs to, whether a key and a host are legal, which areas a clone copies, and the order and
//! the per-area bookkeeping of a clone job. The persistence, the runner and the routes are in
//! `apps/api` on top of these types, so the rules can be tested without a database.
//!
//! Layout:
//!
//! * [`model`] — the environment, its type and status, the clone job and the promotion record.
//! * [`key`] — environment keys and staging hosts: the formats, the reserved words and the
//!   derivation from a name.
//! * [`clone`] — the areas a clone covers, what each area copies, and how a job's per-area counts
//!   fold into a progress figure an operator can trust while it runs.
//! * [`changes`] — what a staging environment holds that production does not: the change set the
//!   Changes tab lists and the promotion in slice 3 freezes.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod changes;
pub mod clone;
pub mod error;
pub mod key;
pub mod model;
pub mod runner;
pub mod store;
