//! Omnion Inventory — what is on the shelf and how it got there (docs/requests/REQ-053).
//!
//! The module is one rule wearing five hats: **`inventory_stock` is a rollup of
//! `inventory_movements`, and the two may never disagree.** Every write goes through
//! [`ledger::record_movement`], which appends the ledger row and updates the rollup in the same
//! transaction with the stock row locked `for update`. A screen that adjusts stock by writing the
//! rollup directly is a screen that can make the two disagree, and the only way to notice is to
//! replay the ledger afterwards — which is why [`ledger::replay`] exists and why the acceptance
//! criteria demand a reconciliation test rather than a spot check.
//!
//! What lives here:
//!
//! * [`error`] — the four refusals the API has to tell apart: a bad field, a row that is not
//!   there, a name that is taken, and a write the module will not perform.
//! * [`model`] — the shared vocabulary: movement kinds, reason codes, location kinds and the
//!   settings row. One definition of "kind" so a screen cannot grow a branch for a string the
//!   schema refuses.
//! * [`items`] — SKU/barcode validation and the item a screen edits.
//! * [`money`] — the decimal helpers. Inventory is not a money module, but a cost column is
//!   `numeric(14,2)` and a quantity is `numeric(14,3)`, and neither has a Rust type in this
//!   workspace; the parse/format pair lives here rather than being copied per module.
//! * [`store`] — items, warehouses, locations, the stock list and the rollup itself.
//! * [`ledger`] — the append-only movement ledger, the reason codes, the negative-stock rule and
//!   the replay that proves the rollup.
//!
//! The crate is a **module** (docs/04-MONOREPO.md): a feature behind the `inventory.*`
//! permission family, not infrastructure the core depends on. It talks to PostgreSQL and to
//! nothing else — the HTTP shapes, the audit entries and the events live in `apps/api`.

#![forbid(unsafe_code)]

pub mod approvals;
pub mod csv;
pub mod dates;
pub mod error;
pub mod items;
pub mod ledger;
pub mod model;
pub mod money;
pub mod store;

pub use error::{InventoryError, Result};
pub use items::Item;
pub use ledger::{Movement, MovementQuery, NewMovement, Recorded, apply_movement, replay, record_movement};
pub use model::{
    LocationKind, MovementKind, ReasonCode, Settings, StockStatus, TransferStatus,
};
pub use store::{
    ItemQuery, ItemView, LocationView, NewItem, NewLocation, NewWarehouse, Overview, Page,
    SettingsPatch, StockLevel, StockPosition, StockQuery, StocktakeSnapshot, WarehouseView,
};
