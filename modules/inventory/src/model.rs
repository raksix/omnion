//! The shared vocabulary of the inventory module: movement kinds, reason codes, stock status and
//! the settings row.
//!
//! These are the words the API, the screens and the migration all have to agree on, and the reason
//! they live in one file is that a screen which switches on a kind string the schema refuses grows
//! a permanently dead branch. Every enum here has exactly one definition of "what does this mean
//! for the numbers", and it is the definition the SQL uses.
//!
//! The two that matter most:
//!
//! * [`MovementKind::signed`] — whether a kind **adds** or **removes**. A ledger row stores a
//!   positive `quantity` plus a kind, and the display sign is derived, because storing a signed
//!   number next to a kind is two sources of truth for one fact. A transfer out and a transfer in
//!   are the same movement seen from two locations, which is why there are two kinds rather than
//!   one with a sign.
//! * [`StockStatus::of`] — one definition of "low", shared by the stock list, the item list, the
//!   overview and the alert sweep. Four screens that each decide this for themselves will
//!   disagree, and the disagreement is invisible until somebody trusts the wrong one.

use serde::{Deserialize, Serialize};

use crate::money::Quantity;

/// What a movement does to the location's `on_hand`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MovementKind {
    /// Goods arriving: a purchase receipt, a customer return.
    Receipt,
    /// Goods leaving for a customer.
    Issue,
    /// Goods leaving for another location of the same organization.
    TransferOut,
    /// Goods arriving from another location of the same organization.
    TransferIn,
    /// A counted correction, **either sign** — the only kind that may be negative.
    Adjustment,
    /// Set aside for a sales order. Moves `reserved`, never `on_hand`.
    Reserve,
    /// Given back: a cancelled order, a released transfer. Moves `reserved`, never `on_hand`.
    Release,
}

impl MovementKind {
    /// The value stored in `inventory_movements.kind`, which the check constraint allows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Receipt => "receipt",
            Self::Issue => "issue",
            Self::TransferOut => "transfer_out",
            Self::TransferIn => "transfer_in",
            Self::Adjustment => "adjustment",
            Self::Reserve => "reserve",
            Self::Release => "release",
        }
    }

    /// Read a stored kind. An unknown value is `None` rather than a default, so a row written by
    /// a newer version is reported instead of being shown as a receipt.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "receipt" => Self::Receipt,
            "issue" => Self::Issue,
            "transfer_out" => Self::TransferOut,
            "transfer_in" => Self::TransferIn,
            "adjustment" => Self::Adjustment,
            "reserve" => Self::Reserve,
            "release" => Self::Release,
            _ => return None,
        })
    }

    /// The direction the kind moves `on_hand`: `+1` adds, `-1` removes, `0` leaves it alone.
    #[must_use]
    pub const fn signed(self) -> i8 {
        match self {
            Self::Receipt | Self::TransferIn => 1,
            Self::Issue | Self::TransferOut => -1,
            // An adjustment carries its own sign — that is the whole point of it — and a
            // reservation moves `reserved`, never `on_hand`.
            Self::Adjustment | Self::Reserve | Self::Release => 0,
        }
    }

    /// Whether the kind moves the `reserved` column rather than `on_hand`.
    #[must_use]
    pub const fn touches_reserved(self) -> bool {
        matches!(self, Self::Reserve | Self::Release)
    }

    /// The kinds that reduce `on_hand` — the ones the available check applies to.
    #[must_use]
    pub const fn is_outbound(self) -> bool {
        self.signed() < 0
    }

    /// Kinds a person may record by hand from the adjust drawer.
    ///
    /// `reserve` and `release` are **not** in this list: they exist to hold stock for a sales
    /// order, and a hand-written reservation is an order that nobody will ever fulfil. The
    /// screen and the API both refuse it, for the same reason.
    #[must_use]
    pub const fn is_recordable_by_hand(self) -> bool {
        matches!(
            self,
            Self::Receipt | Self::Issue | Self::TransferIn | Self::TransferOut | Self::Adjustment
        )
    }

    /// Every kind, in the order the select shows them.
    pub const ALL: &'static [Self] = &[
        Self::Receipt,
        Self::Issue,
        Self::TransferIn,
        Self::TransferOut,
        Self::Adjustment,
        Self::Reserve,
        Self::Release,
    ];
}

/// Why a movement happened — the code an auditor reads six months later.
///
/// Separate from the kind because the kind says *what the numbers did* and the reason says *why
/// somebody did it*, and collapsing the two is how a stocktake variance ends up filed as a
/// delivery. The `correction` reason is the only one that may drive `on_hand` negative, and that
/// is a **service** rule rather than a schema rule, because the schema cannot know whether the
/// caller holds `inventory.negative.manage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    /// A supplier delivered.
    PurchaseReceipt,
    /// Goods left for a customer.
    SaleShipment,
    /// A customer sent something back.
    CustomerReturn,
    /// We sent something back to a supplier.
    SupplierReturn,
    /// Broken, spoiled or otherwise lost to damage.
    Damage,
    /// Lost without anyone recording why.
    Loss,
    /// A counted correction — the only reason that may go below zero.
    Correction,
    /// Used by the organization itself.
    InternalUse,
    /// Posted by closing a stocktake.
    StocktakeVariance,
    /// Part of a transfer between locations.
    Transfer,
}

impl ReasonCode {
    /// The value stored in `inventory_movements.reason`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PurchaseReceipt => "purchase_receipt",
            Self::SaleShipment => "sale_shipment",
            Self::CustomerReturn => "customer_return",
            Self::SupplierReturn => "supplier_return",
            Self::Damage => "damage",
            Self::Loss => "loss",
            Self::Correction => "correction",
            Self::InternalUse => "internal_use",
            Self::StocktakeVariance => "stocktake_variance",
            Self::Transfer => "transfer",
        }
    }

    /// Read a stored reason.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "purchase_receipt" => Self::PurchaseReceipt,
            "sale_shipment" => Self::SaleShipment,
            "customer_return" => Self::CustomerReturn,
            "supplier_return" => Self::SupplierReturn,
            "damage" => Self::Damage,
            "loss" => Self::Loss,
            "correction" => Self::Correction,
            "internal_use" => Self::InternalUse,
            "stocktake_variance" => Self::StocktakeVariance,
            "transfer" => Self::Transfer,
            _ => return None,
        })
    }

    /// Whether this reason is allowed to drive `on_hand` below zero **with the permission**.
    ///
    /// The other half of the rule lives in the service: a `correction` still needs
    /// `inventory.negative.manage`, and every other reason is refused outright. Together: negative
    /// stock is possible, and only deliberately.
    #[must_use]
    pub const fn may_go_negative(self) -> bool {
        matches!(self, Self::Correction)
    }

    /// Every reason, in the order the drawer shows them.
    pub const ALL: &'static [Self] = &[
        Self::PurchaseReceipt,
        Self::SaleShipment,
        Self::CustomerReturn,
        Self::SupplierReturn,
        Self::Damage,
        Self::Loss,
        Self::Correction,
        Self::InternalUse,
        Self::StocktakeVariance,
        Self::Transfer,
    ];
}

/// What a location is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocationKind {
    /// Somewhere stock normally lives.
    Internal,
    /// Where a dispatched transfer sits between two locations.
    InTransit,
    /// Where a customer returned goods.
    Returns,
    /// Damaged or spoiled, kept out of the sellable count.
    Quarantine,
}

impl LocationKind {
    /// The value stored in `inventory_locations.kind`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::InTransit => "in_transit",
            Self::Returns => "returns",
            Self::Quarantine => "quarantine",
        }
    }

    /// Read a stored kind.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "internal" => Self::Internal,
            "in_transit" => Self::InTransit,
            "returns" => Self::Returns,
            "quarantine" => Self::Quarantine,
            _ => return None,
        })
    }

    /// Every kind, for the location editor.
    pub const ALL: &'static [Self] = &[
        Self::Internal,
        Self::InTransit,
        Self::Returns,
        Self::Quarantine,
    ];
}

/// How a stock row reads, in the words the badge prints.
///
/// The order **is** the severity, because [`StockStatus::of`] returns the first that applies and
/// the list sorts by it. A negative beats a below-reorder, which beats a low: a reader scanning
/// the column must see the worst row first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StockStatus {
    /// On hand, nothing held: `available` at or above `reorder_point`.
    Ok,
    /// At or below `reorder_point` but still positive: reorder.
    Low,
    /// At or below `min_threshold`: the line the spec calls "below reorder".
    Critical,
    /// `available` is below zero — more was promised than exists.
    Negative,
}

impl StockStatus {
    /// The value stored in a filter, and the string the API answers with.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Low => "low",
            Self::Critical => "critical",
            Self::Negative => "negative",
        }
    }

    /// Read a stored or requested status.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "ok" => Self::Ok,
            "low" => Self::Low,
            "critical" => Self::Critical,
            "negative" => Self::Negative,
            _ => return None,
        })
    }

    /// The single definition of the status of a row.
    ///
    /// The arguments are the ones the row already has: `available` is `on_hand - reserved`, and
    /// the two thresholds are the item's. **The order is the rule** — a row that is both negative
    /// and below its critical threshold is reported negative, because "you owe three" is a
    /// different conversation from "you are nearly out".
    #[must_use]
    pub fn of(available: Quantity, min_threshold: Quantity, reorder_point: Quantity) -> Self {
        if available.is_negative() {
            Self::Negative
        } else if available.milli() <= min_threshold.milli() {
            Self::Critical
        } else if available.milli() <= reorder_point.milli() {
            Self::Low
        } else {
            Self::Ok
        }
    }

    /// The filter tokens the stock list accepts, in the order the select shows them.
    ///
    /// `below_threshold` is the union of `low` and `critical` rather than a fourth state: an
    /// operator asking "what needs reordering?" does not care which of the two badges it wears.
    pub const ALL_FILTERS: &'static [&'static str] =
        &["ok", "low", "critical", "negative", "below_threshold"];
}

impl fmt::Display for StockStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

use std::fmt;

/// What a transfer is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferStatus {
    /// Written but not moved: nothing has left the source.
    Draft,
    /// Left the source, booked into the organization's transit account.
    Dispatched,
    /// Booked in at the target. Possibly partially, per line.
    Received,
    /// Withdrawn before dispatch, or refused at the target.
    Cancelled,
}

impl TransferStatus {
    /// The value stored in `inventory_transfers.status`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Dispatched => "dispatched",
            Self::Received => "received",
            Self::Cancelled => "cancelled",
        }
    }

    /// Read a stored status.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "draft" => Self::Draft,
            "dispatched" => Self::Dispatched,
            "received" => Self::Received,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    /// True while the transfer may still be dispatched (nothing has left the source).
    #[must_use]
    pub const fn is_open(self) -> bool {
        matches!(self, Self::Draft | Self::Dispatched)
    }
}

/// The organization's inventory settings — the thresholds that make an adjustment need a decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// Above this many units, one adjustment needs a second pair of eyes.
    pub adjustment_approval_threshold: Quantity,
    /// The default reason a new adjustment proposes.
    pub default_adjustment_reason: ReasonCode,
    /// Whether the low-stock sweep runs when a screen is read.
    ///
    /// On by default and turnable off for an installation that runs the sweep on a schedule
    /// instead; the screen says which, so nobody debugs a badge that is not appearing.
    pub alerts_on_read: bool,
    /// The organization's default unit when an item does not set one.
    pub default_unit: String,
}

impl Settings {
    /// The values a fresh organization gets.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            adjustment_approval_threshold: Quantity::from_milli(100_000)
                .unwrap_or_else(Quantity::one),
            default_adjustment_reason: ReasonCode::Correction,
            alerts_on_read: true,
            default_unit: "piece".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kinds_direction_is_defined_once_and_the_ledger_uses_it() {
        assert_eq!(MovementKind::Receipt.signed(), 1);
        assert_eq!(MovementKind::TransferIn.signed(), 1);
        assert_eq!(MovementKind::Issue.signed(), -1);
        assert_eq!(MovementKind::TransferOut.signed(), -1);
        // An adjustment is the one kind that carries its own sign, and a reservation moves
        // `reserved` rather than `on_hand` — which is why its direction is zero.
        assert_eq!(MovementKind::Adjustment.signed(), 0);
        assert_eq!(MovementKind::Reserve.signed(), 0);
        assert_eq!(MovementKind::Release.signed(), 0);
        assert!(MovementKind::Reserve.touches_reserved());
        assert!(!MovementKind::Receipt.touches_reserved());
    }

    #[test]
    fn only_a_correction_may_go_negative() {
        for reason in ReasonCode::ALL {
            assert_eq!(
                reason.may_go_negative(),
                *reason == ReasonCode::Correction,
                "{reason:?}"
            );
        }
    }

    #[test]
    fn a_status_reports_the_worst_thing_that_is_true() {
        let zero = Quantity::ZERO;
        let ten = Quantity::from_milli(10_000).unwrap();
        let fifty = Quantity::from_milli(50_000).unwrap();

        // Out of stock with both thresholds at zero is critical, not ok: a line that is empty is
        // never "fine" just because nobody set a reorder point.
        assert_eq!(StockStatus::of(zero, zero, zero), StockStatus::Critical);

        // Below the reorder point but above the minimum: reorder, not critical.
        assert_eq!(StockStatus::of(fifty, ten, ten), StockStatus::Ok);
        assert_eq!(StockStatus::of(ten, ten, ten), StockStatus::Critical);
        assert_eq!(StockStatus::of(fifty, ten, fifty), StockStatus::Low);

        // Negative beats everything.
        let minus = Quantity::from_milli(-500).unwrap();
        assert_eq!(StockStatus::of(minus, ten, fifty), StockStatus::Negative);
    }

    #[test]
    fn every_kind_and_reason_round_trips_through_its_stored_string() {
        for kind in MovementKind::ALL {
            assert_eq!(MovementKind::parse(kind.as_str()), Some(*kind), "{kind:?}");
        }
        for reason in ReasonCode::ALL {
            assert_eq!(ReasonCode::parse(reason.as_str()), Some(*reason), "{reason:?}");
        }
        // And an unknown value is None, never a silent default: a row written by a newer version
        // has to be reported rather than shown as a receipt.
        assert_eq!(MovementKind::parse("shrinkage"), None);
        assert_eq!(ReasonCode::parse("because"), None);
    }

    #[test]
    fn a_reservation_is_not_something_a_person_records_by_hand() {
        // A hand-written reservation is an order nobody will ever fulfil.
        assert!(!MovementKind::Reserve.is_recordable_by_hand());
        assert!(!MovementKind::Release.is_recordable_by_hand());
        assert!(MovementKind::Adjustment.is_recordable_by_hand());
        assert!(MovementKind::Receipt.is_recordable_by_hand());
    }
}
