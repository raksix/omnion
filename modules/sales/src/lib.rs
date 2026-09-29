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
//! * [`dates`] — how a day and a timestamp cross the wire, in the two shapes a browser sends.
//! * [`quotes`] — the document a seller writes: the grid, the totals computed from the stored
//!   lines, the versions the customer saw, the per-organization numbering, and the public link
//!   whose token is stored only as a hash.
//! * [`approvals`] — the discount gate: ask a manager, decide the request, and let the quote be
//!   sent afterwards.
//! * [`orders`] — the other half of the chain: quote → order → confirm (which holds stock) →
//!   invoice draft, with the status history that says how the document got where it is.
//! * [`reports`] — the answer a desk is judged on: what was won and lost, per owner and per
//!   period, the same rows as a CSV, and one ranked search over every document it wrote.
//!
//! The crate is a **module** (docs/04-MONOREPO.md): a feature the platform can carry behind the
//! `sales.*` permission family, not infrastructure the core depends on. It talks to PostgreSQL
//! and to nothing else — the HTTP shapes, the audit entries and the events live in `apps/api`.

#![forbid(unsafe_code)]

pub mod approvals;
pub mod catalog;
pub mod dates;
pub mod documents;
pub mod error;
pub mod model;
pub mod money;
pub mod orders;
pub mod pdf;
pub mod quotes;
pub mod reports;
pub mod store;

pub use error::{Result, SalesError};
pub use model::{OrderReservationState, OrderStatus, QuoteStatus, Settings, Unit};
pub use money::{LineTotals, QuoteTotals};
pub use approvals::{
    ApprovalDecision, ApprovalQuery, ApprovalRequest, ApprovalRequired, ApprovalScope,
    ApprovalStatus, ApprovalView, DecisionView,
};
pub use quotes::{
    NewQuote, NewQuoteLine, PublicQuote, QuoteDetail, QuoteLineView, QuotePatch, QuoteQuery,
    QuoteTotalsView, QuoteView,
};
pub use reports::{
    GlobalSearchResults, OwnerRow, ReportRow, ReportTotals, ReportQuery, SalesReport, SearchHit,
};
pub use orders::{
    CancelOrder, HistoryEntry, InvoiceHandoffView, NewOrder, NewOrderLine, OrderDetail, OrderLineView,
    OrderQuery, OrderView, ReservationView,
};
pub use store::{
    CatalogQuery, CatalogVocabulary, NewPriceList, NewPriceRow, NewProduct, Page, PriceListDetail,
    PriceListPatch, PriceListView, PriceRowView, ProductPatch, ProductView, SettingsPatch,
};
