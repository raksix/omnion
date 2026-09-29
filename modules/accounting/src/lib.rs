//! Omnion Accounting — the money has to balance (docs/requests/REQ-054, slice 1).
//!
//! One rule, three files: **an entry that does not balance cannot be read.** Every other decision
//! in this crate is downstream of it, and the slice's own wording for "done" is a manual entry
//! that posts and an unbalanced one that is refused with a message naming the two totals.
//!
//! What lives here:
//!
//! * [`error`] — the four refusals the API has to tell apart: a field, a row that is not there, a
//!   code that is taken, and a write the module will not perform. The last one is
//!   [`AccountingError::UnbalancedEntry`], and it is the only variant in this workspace that
//!   carries **numbers** rather than a sentence — because "cannot post" sends the operator back
//!   to the line grid to subtract by hand, and the server already knows the difference.
//! * [`model`] — the shared vocabulary: account kinds, the tax-rate kinds, the source of an
//!   entry, and the numbering per organization.
//! * [`money`] — `numeric(14,2)` has no Rust type here, so amounts cross SQL as text and are
//!   parsed to integer hundredths. The same shape as the sales and inventory modules wrote, for
//!   the same reason: a cent that disagrees between the panel and the PDF is a support ticket.
//! * [`dates`] — a day is `YYYY-MM-DD` and a timestamp is RFC 3339 in UTC. Both are the forms a
//!   browser sends, because a `<input type="date">` is the only date picker the module has.
//! * [`accounts`] — the chart of accounts and the tax rates: a tree by kind, seeded per
//!   organization, where an account with postings deactivates and never deletes; and rates with
//!   **exactly one default per (organization, kind)**, enforced by a partial unique index rather
//!   than by a flag the next route can forget.
//! * [`journal`] — the entries, and the posting path that writes the lines and both totals in
//!   one statement.

#![forbid(unsafe_code)]

pub mod accounts;
pub mod dates;
pub mod error;
pub mod journal;
pub mod model;
pub mod money;
pub mod store;

pub use accounts::{
    AccountKind, AccountPatch, AccountView, NewAccount, NewTaxRate, TaxRatePatch, TaxRateKind,
    TaxRateView,
};
pub use error::{AccountingError, Result, status_of};
pub use journal::{
    EntrySource, JournalEntrySummary, JournalEntryView, JournalLineInput, JournalLineView,
    NewJournalEntry,
};
pub use store::Page;
