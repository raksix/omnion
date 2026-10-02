//! Items, warehouses and locations: the vocabulary every other part of the module borrows.
//!
//! Three rules live here rather than in a screen, because a screen, a CSV import and a scanner
//! box are all going to call them and must agree:
//!
//! * **A SKU is one item per organization, case-insensitively.** The unique index enforces it
//!   among live rows; [`validate_sku`] is what the form calls so the message arrives before the
//!   round trip rather than after it.
//! * **A barcode is a scanner's key, so it is normalized before it is compared.** A scanner
//!   emits `8691234567890` and a keyboard-typed one emits `869 12345 67890`; two rows differing
//!   only by the spaces would be two items on a shelf that has one.
//! * **Nothing is deleted.** An item a past movement still names keeps reading exactly as it was
//!   on the day it moved — the ledger is the reason the module exists, and a ledger that points
//!   at a deleted row is worthless.

use serde::{Deserialize, Serialize};

use crate::error::{InventoryError, Result};
use crate::model::ReasonCode;
use crate::money::{Amount, Quantity};

/// Longest a name may be, the same bound the schema stores.
pub const MAX_NAME_LENGTH: usize = 160;
/// Longest a category may be.
pub const MAX_CATEGORY_LENGTH: usize = 80;
/// Longest a unit may be.
pub const MAX_UNIT_LENGTH: usize = 24;
/// Longest a code may be (`WH-A`, `STOCK`, `BOLT-M8`).
pub const MAX_CODE_LENGTH: usize = 32;
/// Longest the notes may be.
pub const MAX_NOTES_LENGTH: usize = 2_000;
/// Longest a movement's note may be.
pub const MAX_MOVEMENT_NOTE_LENGTH: usize = 500;
/// Longest a document reference may be (`Q-2026-0007`, `PO-9931`).
pub const MAX_REFERENCE_LENGTH: usize = 64;

/// An item as the module holds it, without the row's bookkeeping columns.
///
/// `Serialize` because the item is returned inside the store's views: a screen that had to ask a
/// second question about the thresholds would be a screen that could draw the two halves of an
/// item differently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// The row's id.
    pub id: uuid::Uuid,
    /// The organization's SKU, as written (the unique index compares it case-insensitively).
    pub sku: String,
    /// The name shown on the list and the stock screens.
    pub name: String,
    /// The category, when set — the list's category filter reads it.
    pub category: Option<String>,
    /// The unit it is counted in.
    pub unit: String,
    /// The barcode a scanner resolves this item by, when set.
    pub barcode: Option<String>,
    /// The level at or below which the line is `critical`.
    pub min_threshold: Quantity,
    /// The level at or below which the line is `low` and wants reordering.
    pub reorder_point: Quantity,
    /// How much a reorder brings, shown next to the alert.
    pub reorder_qty: Quantity,
    /// The unit cost, when the organization has set one. Never invented.
    pub cost: Option<Amount>,
    /// The currency of `cost`, from the organization.
    pub currency: String,
    /// The REQ-052 product this item mirrors, when it is sellable.
    ///
    /// **Optional in both directions**: an inventory item is not necessarily sellable (a
    /// consumable in a workshop is not) and a catalog product need not be stocked (a service is
    /// not). The link is one column, and the screens say which side they are looking at.
    pub product_id: Option<uuid::Uuid>,
    /// Free notes, up to 2 000 characters.
    pub notes: String,
    /// Whether a new movement may name it.
    pub active: bool,
}

impl Item {
    /// The quantity a person may actually draw: on hand minus what is held for orders.
    #[must_use]
    pub fn available(&self, on_hand: Quantity, reserved: Quantity) -> Quantity {
        on_hand.checked_sub(reserved).unwrap_or(Quantity::ZERO)
    }
}

/// A warehouse and its locations, as the tree editor draws them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warehouse {
    /// The row's id.
    pub id: uuid::Uuid,
    /// The short code (`MAIN`).
    pub code: String,
    /// The name shown in the tree.
    pub name: String,
    /// Whether new stock may be booked here.
    pub active: bool,
}

/// A location inside a warehouse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    /// The row's id.
    pub id: uuid::Uuid,
    /// The warehouse it belongs to.
    pub warehouse_id: uuid::Uuid,
    /// The short code (`STOCK`, `RETURNS`).
    pub code: String,
    /// The name shown in the tree.
    pub name: String,
    /// What the location is for.
    pub kind: crate::model::LocationKind,
    /// Whether new stock may be booked here.
    pub active: bool,
}

/// Validate a SKU the way the form needs to know about it.
///
/// The same shape as the migration's `inventory_items_sku_format` check, deliberately: the form
/// and the schema must refuse the same strings, or a person fills in a value the round trip throws
/// away.
pub fn validate_sku(raw: &str) -> Result<String> {
    let sku = raw.trim();
    if !(2..=MAX_CODE_LENGTH).contains(&sku.len())
        || !sku
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(InventoryError::invalid(
            "item",
            "sku",
            format!(
                "use 2–{MAX_CODE_LENGTH} letters, digits, dots, underscores or dashes — \
                 a SKU has to survive a label printer and a barcode scanner"
            ),
        ));
    }
    Ok(sku.to_string())
}

/// Validate a name: not blank, not longer than the column.
pub fn validate_name(entity: &'static str, raw: &str) -> Result<String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(InventoryError::invalid(entity, "name", "give it a name"));
    }
    if name.chars().count() > MAX_NAME_LENGTH {
        return Err(InventoryError::invalid(
            entity,
            "name",
            format!("keep it under {MAX_NAME_LENGTH} characters"),
        ));
    }
    Ok(name.to_string())
}

/// Validate a warehouse or location code: upper-cased, because a location is read on a label.
pub fn validate_code(entity: &'static str, raw: &str) -> Result<String> {
    let code = raw.trim().to_ascii_uppercase();
    if code.is_empty() {
        return Err(InventoryError::invalid(entity, "code", "give it a code"));
    }
    if code.len() > MAX_CODE_LENGTH
        || !code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(InventoryError::invalid(
            entity,
            "code",
            format!("use 1–{MAX_CODE_LENGTH} letters, digits, dashes or underscores"),
        ));
    }
    Ok(code)
}

/// Normalize a barcode for storage and comparison.
///
/// EAN-13, Code-128 and the internal labels the platform prints are all digits, upper-case
/// letters and dashes. **Separators are removed**, because a scanner that reads
/// `869 12345 67890` and a person who types `8691234567890` are looking at the same label, and
/// storing them as two items is a bug the person only finds at the till.
pub fn normalize_barcode(raw: Option<&str>) -> Result<Option<String>> {
    // `Option` rather than `&str` because every caller holds an `Option<String>`: the patch route's
    // "clear it" is `Some("")` and the create route's "not set" is `None`, and both mean "no
    // barcode" through the same branch. Making the function take `&str` forces every call site to
    // write `.unwrap_or_default()`, which is how one of the two paths ends up skipping the
    // normalizer.
    let trimmed = raw.unwrap_or_default().trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let cleaned: String = trimmed
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_'))
        .collect();
    if !(6..=MAX_CODE_LENGTH).contains(&cleaned.len())
        || !cleaned
            .bytes()
            .all(|b| b.is_ascii_alphanumeric())
    {
        return Err(InventoryError::invalid(
            "item",
            "barcode",
            format!(
                "use 6–{MAX_CODE_LENGTH} letters or digits (spaces and dashes are ignored), \
                 as an EAN-13 or Code-128 label reads"
            ),
        ));
    }
    Ok(Some(cleaned.to_ascii_uppercase()))
}

/// Validate a unit: not blank, not absurdly long.
pub fn validate_unit(raw: &str) -> Result<String> {
    let unit = raw.trim();
    if unit.is_empty() {
        return Err(InventoryError::invalid("item", "unit", "say what it is counted in"));
    }
    if unit.chars().count() > MAX_UNIT_LENGTH {
        return Err(InventoryError::invalid(
            "item",
            "unit",
            format!("keep the unit under {MAX_UNIT_LENGTH} characters"),
        ));
    }
    Ok(unit.to_string())
}

/// Validate a category: trimmed, bounded, and empty is `None` rather than `Some("")`.
#[must_use]
pub fn normalize_category(raw: Option<&str>) -> Option<String> {
    let value = raw?.trim();
    if value.is_empty() || value.chars().count() > MAX_CATEGORY_LENGTH {
        return None;
    }
    Some(value.to_string())
}

/// Validate the two thresholds against each other.
///
/// The rule is the spec's: **`reorder_point >= min_threshold`**. It is enforced here and mirrored
/// by a check constraint, because a reorder point below the critical level is not a warning a
/// person can act on — it means the line goes critical and low in the wrong order.
pub fn validate_thresholds(
    min_threshold: Quantity,
    reorder_point: Quantity,
) -> Result<(Quantity, Quantity)> {
    if reorder_point.milli() < min_threshold.milli() {
        return Err(InventoryError::invalid(
            "item",
            "reorder_point",
            "the reorder point cannot be below the minimum threshold — the line would go \
             critical and low in the wrong order",
        ));
    }
    Ok((min_threshold, reorder_point))
}

/// Validate a movement's note length.
pub fn validate_note(entity: &'static str, raw: &str) -> Result<String> {
    let note = raw.trim();
    if note.chars().count() > MAX_MOVEMENT_NOTE_LENGTH {
        return Err(InventoryError::invalid(
            entity,
            "note",
            format!("keep the note under {MAX_MOVEMENT_NOTE_LENGTH} characters"),
        ));
    }
    Ok(note.to_string())
}

/// The reason a new adjustment proposes when the caller names none.
///
/// `correction` rather than `loss`: an adjustment that has not been classified is a correction,
/// and the drawer still makes the person pick a reason before the write lands.
#[must_use]
pub fn default_reason(raw: Option<&str>) -> Result<ReasonCode> {
    match raw.map(str::trim) {
        None | Some("") => Ok(ReasonCode::Correction),
        Some(value) => ReasonCode::parse(value).ok_or_else(|| {
            InventoryError::invalid("movement", "reason", format!("{value} is not a reason code"))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_survives_a_label_printer_and_a_scanner() {
        assert_eq!(validate_sku("BOLT-M8").unwrap(), "BOLT-M8");
        assert_eq!(validate_sku("  part_01.v2  ").unwrap(), "part_01.v2");
        // One letter is a code the label printer cannot lay out and a human cannot read aloud.
        assert!(validate_sku("A").is_err());
        assert!(validate_sku("BOLT M8").is_err());
        assert!(validate_sku(&"x".repeat(33)).is_err());
    }

    #[test]
    fn a_scanner_and_a_person_find_the_same_label() {
        // The whole point: one label, one item, whichever way the code arrives.
        assert_eq!(
            normalize_barcode(Some("869 12345 67890")).unwrap(),
            Some("8691234567890".to_string())
        );
        assert_eq!(
            normalize_barcode(Some("869-12345-67890")).unwrap(),
            Some("8691234567890".to_string())
        );
        assert_eq!(
            normalize_barcode(Some("  abc123  ")).unwrap(),
            Some("ABC123".to_string())
        );
        assert_eq!(normalize_barcode(Some("   ")).unwrap(), None, "blank means no barcode");
        assert!(normalize_barcode(Some("12")).is_err());
    }

    #[test]
    fn a_reorder_point_below_the_minimum_is_refused_with_the_reason() {
        let ten = Quantity::from_milli(10_000).unwrap();
        let five = Quantity::from_milli(5_000).unwrap();
        let error = validate_thresholds(ten, five).unwrap_err().to_string();
        assert!(error.contains("reorder_point"), "{error}");
        assert!(error.contains("critical"), "the message must explain the consequence: {error}");

        // Equal is fine — "reorder as soon as you touch the minimum" is a real policy.
        assert!(validate_thresholds(ten, ten).is_ok());
        assert!(validate_thresholds(five, ten).is_ok());
    }

    #[test]
    fn a_blank_category_is_absent_rather_than_an_empty_string() {
        // A `Some("")` would create a third category in the filter that no item visibly has.
        assert_eq!(normalize_category(None), None);
        assert_eq!(normalize_category(Some("   ")), None);
        assert_eq!(normalize_category(Some(" Fasteners ")), Some("Fasteners".to_string()));
    }

    #[test]
    fn an_unclassified_reason_is_a_correction_and_a_nonsense_one_is_refused() {
        assert_eq!(default_reason(None).unwrap(), ReasonCode::Correction);
        assert_eq!(default_reason(Some("")).unwrap(), ReasonCode::Correction);
        assert_eq!(default_reason(Some("damage")).unwrap(), ReasonCode::Damage);
        assert!(default_reason(Some("because")).is_err());
    }

    #[test]
    fn available_is_what_is_left_after_the_orders_are_held() {
        let item = Item {
            id: uuid::Uuid::nil(),
            sku: "X".into(),
            name: "x".into(),
            category: None,
            unit: "piece".into(),
            barcode: None,
            min_threshold: Quantity::ZERO,
            reorder_point: Quantity::ZERO,
            reorder_qty: Quantity::ZERO,
            cost: None,
            currency: "TRY".into(),
            product_id: None,
            notes: String::new(),
            active: true,
        };
        let on_hand = Quantity::from_milli(10_000).unwrap();
        let reserved = Quantity::from_milli(4_000).unwrap();
        assert_eq!(item.available(on_hand, reserved).to_text(), "6.000");
    }
}
