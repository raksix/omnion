//! Omnion Sales — the selling side of the platform (docs/requests/REQ-052).
//!
//! Four jobs, one crate:
//!
//! * [`money`] — the arithmetic every document depends on: decimal parsing, half-up rounding to
//!   the cent, and the line/quote total rule the panel, the PDF and the future invoice all share.
//! * [`catalog`] — products and price lists: SKU validation, the per-list price a product
//!   resolves to for a given quantity, and the archive that never deletes.
//! * [`error`] — what the API layer needs to tell apart: a refused field, a record that is not
//!   there, a name that is taken, and a conflict with the immutability rules.
//! * [`model`] — the shared vocabulary: statuses, units and the settings row.
//!
//! The crate is a **module** (docs/04-MONOREPO.md): a feature the platform can carry behind the
//! `sales.*` permission family, not infrastructure the core depends on. It talks to PostgreSQL
//! and to nothing else — the HTTP shapes, the audit entries and the events live in `apps/api`.

#![forbid(unsafe_code)]

pub mod catalog;
pub mod error;
pub mod model;
pub mod money;
pub mod store;

pub use error::{Result, SalesError};
pub use model::{QuoteStatus, Settings, Unit};
pub use money::{LineTotals, QuoteTotals};
pub use store::{
    CatalogQuery, CatalogVocabulary, NewPriceList, NewPriceRow, NewProduct, Page, PriceListDetail,
    PriceListPatch, PriceListView, PriceRowView, ProductPatch, ProductView, SettingsPatch,
};
