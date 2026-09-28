//! The sellable catalog: products and price lists.
//!
//! Two rules live here rather than in a screen, because the screens are not the only callers — the
//! quote builder, the CSV import and a future storefront all need them to agree:
//!
//! * **A SKU is one product per organization, case-insensitively.** The unique index enforces it
//!   among live rows; [`validate_sku`] is what the form calls so the message arrives before the
//!   round-trip rather than after it.
//! * **A product with no price row falls back to its default price.** That is the spec's rule and
//!   it is the reason [`resolve_price`] exists as a function: a builder that prefilled nothing
//!   would make a person look the price up by hand for every product not on the list.

use crate::error::{Result, SalesError};
use crate::money::Money;

/// A product as the module holds it, without the row's bookkeeping columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Product {
    /// The row's id.
    pub id: uuid::Uuid,
    /// The organization's SKU, as written (the unique index compares it case-insensitively).
    pub sku: String,
    /// The name shown on the quote.
    pub name: String,
    /// The longer description, when the seller wrote one.
    pub description: String,
    /// The category, when set.
    pub category: Option<String>,
    /// The unit it is sold in.
    pub unit: crate::model::Unit,
    /// The tax rate snapshotted onto a line that uses this product.
    pub tax_percent: i32,
    /// The price a quote line gets when no price list says otherwise.
    pub default_price: Money,
    /// The currency of `default_price`.
    pub currency: String,
    /// Whether a new line may use it.
    pub active: bool,
}

/// One row of a price list: a price, and the quantity from which it applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceListItem {
    /// The product the row prices.
    pub product_id: uuid::Uuid,
    /// The quantity from which this price applies (`1` for a flat price).
    pub min_quantity: crate::money::Quantity,
    /// The price for one unit at that quantity.
    pub price: Money,
}

/// Validate a SKU the way the form needs to know about it.
///
/// The same shape as the migration's `sales_products_sku_format` constraint, deliberately: a
/// constraint that disagrees with the validator means the form accepts something the database
/// refuses, and the person only finds out after filling in the rest of the form.
pub fn validate_sku(sku: &str) -> Result<()> {
    let trimmed = sku.trim();
    if trimmed.len() < 2 || trimmed.len() > 32 {
        return Err(SalesError::invalid(
            "product",
            "sku",
            "a SKU is 2 to 32 characters",
        ));
    }
    if !trimmed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(SalesError::invalid(
            "product",
            "sku",
            "use letters, digits, dot, underscore or hyphen only",
        ));
    }
    Ok(())
}

/// Validate a product's name, the way the form and the schema agree on it.
pub fn validate_name(name: &str) -> Result<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(SalesError::invalid(
            "product",
            "name",
            "a product needs a name",
        ));
    }
    if trimmed.chars().count() > 160 {
        return Err(SalesError::invalid(
            "product",
            "name",
            "that name is longer than 160 characters",
        ));
    }
    Ok(())
}

/// Validate a price: never negative, because a negative "price" is a credit note, not a product.
pub fn validate_price(price: Money) -> Result<()> {
    if price.minor() < 0 {
        return Err(SalesError::invalid(
            "product",
            "default_price",
            "a price cannot be negative",
        ));
    }
    Ok(())
}

/// Validate a tax percentage against the range the schema stores.
pub fn validate_tax_percent(percent: i32) -> Result<()> {
    if !(0..=100).contains(&percent) {
        return Err(SalesError::invalid(
            "product",
            "tax_percent",
            "tax is a percentage between 0 and 100",
        ));
    }
    Ok(())
}

/// Validate a price-list row: a positive threshold quantity and a price that is not negative.
pub fn validate_price_item(item: &PriceListItem) -> Result<()> {
    if !item.min_quantity.is_positive() {
        return Err(SalesError::invalid(
            "price_list_item",
            "min_quantity",
            "the quantity a price starts at must be more than zero",
        ));
    }
    validate_price(item.price).map_err(|_| {
        SalesError::invalid("price_list_item", "price", "a price cannot be negative")
    })?;
    Ok(())
}

/// The price a quote line gets, given a list's rows and the quantity being ordered.
///
/// The rule, in order:
///
/// 1. Of the rows for this product whose `min_quantity` the ordered quantity satisfies, the one
///    with the **largest** threshold wins — that is what "from 100 units the price is lower"
///    means, and taking the first match instead would give a customer the 1-unit price for a
///    500-unit order.
/// 2. If no row qualifies, the product's own default price.
///
/// An empty list and a missing product therefore both land on the default, which is the spec's
/// "never a silent failure" requirement: the builder shows the price it is about to charge, and
/// that price is always *a* price.
#[must_use]
pub fn resolve_price(
    default_price: Money,
    rows: &[PriceListItem],
    product_id: uuid::Uuid,
    quantity: crate::money::Quantity,
) -> Money {
    rows.iter()
        .filter(|row| row.product_id == product_id && row.min_quantity <= quantity)
        .max_by_key(|row| row.min_quantity)
        .map_or(default_price, |row| row.price)
}

/// The next number in a series, given the numbers already taken in this organization.
///
/// Zero-padded to four digits, which is the `Q-2026-0001` shape the spec's columns show. The
/// year is part of the prefix, so the series restarts each January and a quote from last year
/// does not collide with today's first one — while the unique index still refuses a genuine
/// duplicate inside a year.
#[must_use]
pub fn next_number(prefix: &str, year: i32, existing: &[String]) -> String {
    let stem = format!("{prefix}-{year}-");
    let highest = existing
        .iter()
        .filter_map(|number| number.strip_prefix(&stem))
        .filter_map(|tail| tail.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    format!("{stem}{:04}", highest + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::Quantity;

    fn product_id(n: u128) -> uuid::Uuid {
        uuid::Uuid::from_u128(n)
    }

    fn item(id: u128, min: &str, price: &str) -> PriceListItem {
        PriceListItem {
            product_id: product_id(id),
            min_quantity: Quantity::parse(min).expect("a valid quantity"),
            price: Money::parse(price).expect("a valid price"),
        }
    }

    // ---- validation ----------------------------------------------------------------------------

    #[test]
    fn a_sku_of_the_shape_the_index_allows_is_accepted() {
        for good in ["AB", "abc", "A.B_C-9", "12345678", &"x".repeat(32)] {
            assert!(validate_sku(good).is_ok(), "{good}");
        }
    }

    #[test]
    fn a_sku_the_index_would_refuse_is_refused_by_the_validator_too() {
        // The point of these pairs: the form and the database must agree, or a person fills in
        // the whole form and is told at the end that the first field was wrong.
        for bad in [
            "",
            " ",
            "a",
            &"x".repeat(33),
            "with space",
            "slash/",
            "ü",
            "a+b",
        ] {
            assert!(validate_sku(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn a_blank_name_is_refused_and_a_long_one_says_so() {
        assert!(validate_name("   ").is_err());
        assert!(validate_name("Widget").is_ok());
        let long = "x".repeat(161);
        let error = validate_name(&long).unwrap_err().to_string();
        assert!(error.contains("160"), "{error}");
        assert!(validate_name(&"x".repeat(160)).is_ok());
    }

    #[test]
    fn a_negative_price_is_refused_because_a_credit_note_is_not_a_product() {
        let negative = Money::parse("-1.00").expect("parses");
        assert!(validate_price(negative).is_err());
        assert!(validate_price(Money::zero()).is_ok());
    }

    #[test]
    fn a_tax_percentage_outside_zero_to_a_hundred_is_refused() {
        assert!(validate_tax_percent(0).is_ok());
        assert!(validate_tax_percent(100).is_ok());
        assert!(validate_tax_percent(20).is_ok());
        assert!(validate_tax_percent(-1).is_err());
        assert!(validate_tax_percent(101).is_err());
    }

    #[test]
    fn a_price_row_of_zero_units_is_refused() {
        let bad = PriceListItem {
            product_id: product_id(1),
            min_quantity: Quantity::from_milli(0).expect("zero is representable"),
            price: Money::zero(),
        };
        assert!(validate_price_item(&bad).is_err());
        assert!(validate_price_item(&item(1, "1", "10.00")).is_ok());
    }

    // ---- price resolution ----------------------------------------------------------------------

    #[test]
    fn a_product_with_no_row_on_the_list_costs_its_default_price() {
        let rows = [item(1, "1", "5.00")];
        let price = resolve_price(
            Money::parse("9.99").expect("ok"),
            &rows,
            product_id(2),
            Quantity::one(),
        );
        assert_eq!(
            price.to_text(),
            "9.99",
            "an empty list must not look like a free product"
        );
    }

    #[test]
    fn an_empty_list_is_also_the_default_price() {
        let price = resolve_price(
            Money::parse("9.99").expect("ok"),
            &[],
            product_id(1),
            Quantity::one(),
        );
        assert_eq!(price.to_text(), "9.99");
    }

    #[test]
    fn a_bulk_row_wins_over_the_single_unit_row() {
        // The mistake this guards: taking the *first* qualifying row gives the 1-unit price to a
        // 500-unit order, which is the opposite of what a tiered list means.
        let rows = [item(1, "1", "10.00"), item(1, "100", "7.50")];
        let price = resolve_price(
            Money::parse("10.00").expect("ok"),
            &rows,
            product_id(1),
            Quantity::parse("500").expect("ok"),
        );
        assert_eq!(price.to_text(), "7.50");
    }

    #[test]
    fn a_quantity_below_the_bulk_threshold_gets_the_single_unit_price() {
        let rows = [item(1, "1", "10.00"), item(1, "100", "7.50")];
        let price = resolve_price(
            Money::parse("10.00").expect("ok"),
            &rows,
            product_id(1),
            Quantity::parse("99").expect("ok"),
        );
        assert_eq!(price.to_text(), "10.00");
    }

    #[test]
    fn the_threshold_is_inclusive_so_exactly_a_hundred_units_qualifies() {
        let rows = [item(1, "1", "10.00"), item(1, "100", "7.50")];
        let price = resolve_price(
            Money::parse("10.00").expect("ok"),
            &rows,
            product_id(1),
            Quantity::parse("100").expect("ok"),
        );
        assert_eq!(
            price.to_text(),
            "7.50",
            "'from 100 units' means from 100 units"
        );
    }

    #[test]
    fn a_row_of_a_different_product_is_never_used() {
        // The list is read with the whole set of rows, so a row for another product must not
        // leak into this product's price.
        let rows = [item(1, "1", "5.00"), item(2, "1", "1.00")];
        let price = resolve_price(
            Money::parse("9.99").expect("ok"),
            &rows,
            product_id(1),
            Quantity::one(),
        );
        assert_eq!(price.to_text(), "5.00");
    }

    #[test]
    fn rows_given_out_of_order_still_resolve_to_the_highest_threshold() {
        let rows = [
            item(1, "100", "7.50"),
            item(1, "1", "10.00"),
            item(1, "1000", "6.00"),
        ];
        let price = resolve_price(
            Money::parse("10.00").expect("ok"),
            &rows,
            product_id(1),
            Quantity::parse("2000").expect("ok"),
        );
        assert_eq!(
            price.to_text(),
            "6.00",
            "ordering of rows must not change the answer"
        );
    }

    // ---- numbering -----------------------------------------------------------------------------

    #[test]
    fn the_first_number_of_a_year_is_the_first_quote_of_that_year() {
        assert_eq!(next_number("Q", 2026, &[]), "Q-2026-0001");
    }

    #[test]
    fn numbering_continues_past_the_numbers_already_taken() {
        let taken = vec!["Q-2026-0001".to_string(), "Q-2026-0002".to_string()];
        assert_eq!(next_number("Q", 2026, &taken), "Q-2026-0003");
    }

    #[test]
    fn a_gap_does_not_hand_out_a_number_that_is_already_taken() {
        let taken = vec!["Q-2026-0001".to_string(), "Q-2026-0007".to_string()];
        assert_eq!(next_number("Q", 2026, &taken), "Q-2026-0008");
    }

    #[test]
    fn each_year_counts_on_its_own() {
        let taken = vec!["Q-2025-0042".to_string(), "Q-2026-0003".to_string()];
        assert_eq!(next_number("Q", 2026, &taken), "Q-2026-0004");
        assert_eq!(next_number("Q", 2025, &taken), "Q-2025-0043");
    }

    #[test]
    fn a_number_of_another_prefix_or_shape_does_not_derail_the_series() {
        let taken = vec![
            "SO-2026-0009".to_string(),
            "Q-2026-0001".to_string(),
            "Q-2026-EXTRA".to_string(),
        ];
        assert_eq!(next_number("Q", 2026, &taken), "Q-2026-0002");
    }

    #[test]
    fn an_order_and_a_quote_have_different_prefixes_and_both_are_well_formed() {
        assert_eq!(next_number("Q", 2026, &[]), "Q-2026-0001");
        assert_eq!(next_number("SO", 2026, &[]), "SO-2026-0001");
    }
}
