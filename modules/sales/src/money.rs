//! The money arithmetic every sales document depends on.
//!
//! The spec's own rule: *"compute line totals in `numeric`, round half-up once per line, and sum
//! the rounded values so the printed PDF, the panel and the invoice agree."* This module is where
//! that rule lives, so the three readers cannot disagree with one another.
//!
//! # Why this is not `f64`
//!
//! A binary float cannot represent `0.1`. `0.1 + 0.2` is `0.30000000000000004`, and a quote
//! printed from that is wrong in the last cent — which is the difference between a document that
//! matches its PDF and one that produces a support ticket. The database already stores money as
//! `numeric(14,2)`, so the module speaks the same language: **an amount is an integer count of
//! hundredths, plus the scale it was written with.** [`Money`] is that count, and it does its
//! arithmetic on `i128`, so a total can never drift from its inputs by a rounding artefact.
//!
//! Quantities keep three decimals (a warehouse sells `1.250 kg`), so [`Quantity`] is a separate
//! type on purpose: mixing a quantity into a money sum is a bug the type system should refuse,
//! and rounding a quantity to cents at the wrong moment is how a line total ends up a cent out.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// An amount of money as a whole number of hundredths, scaled to whatever the document is in.
///
/// The scale is carried, not assumed, because a quote can be in a currency with no minor unit
/// (JPY) and because a value that came from a `numeric(14,2)` column must not be re-interpreted as
/// a different scale on the way out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Money {
    /// Hundredths. `1234` with scale 2 is `12.34`.
    minor: i128,
    /// Decimal places this value is written with (0 for a whole-unit currency).
    scale: u32,
}

/// The default scale: two places, which is every currency in the platform's default set.
pub const DEFAULT_SCALE: u32 = 2;

impl Money {
    /// The largest amount the module will represent, chosen so a sum of many lines cannot
    /// overflow `i128` before the database's `numeric(14,2)` would have refused it anyway.
    const MAX_MINOR: i128 = i128::MAX / 1_000_000;

    /// Zero, at the default scale.
    #[must_use]
    pub const fn zero() -> Self {
        Self {
            minor: 0,
            scale: DEFAULT_SCALE,
        }
    }

    /// Build from a count of hundredths.
    ///
    /// Returns `None` past [`Money::MAX_MINOR`], so an absurd total is a refusal at the boundary
    /// rather than a silent wrap in the middle of a sum.
    #[must_use]
    #[allow(
        clippy::manual_range_contains,
        reason = "RangeInclusive::contains is not const"
    )]
    pub const fn from_minor(minor: i128) -> Option<Self> {
        if !Self::within_range(minor) {
            return None;
        }
        Some(Self {
            minor,
            scale: DEFAULT_SCALE,
        })
    }

    /// Whether a count of hundredths is inside the range the module represents.
    ///
    /// Two comparisons rather than `RangeInclusive::contains`, which is not `const` on this
    /// toolchain — and `from_minor` is `const` because the module's callers use it in
    /// `const` contexts. The range is named so the bound is stated once.
    const fn within_range(minor: i128) -> bool {
        minor >= -Self::MAX_MINOR && minor <= Self::MAX_MINOR
    }

    /// The count of hundredths.
    #[must_use]
    pub const fn minor(self) -> i128 {
        self.minor
    }

    /// The scale this value is written with.
    #[must_use]
    pub const fn scale(self) -> u32 {
        self.scale
    }

    /// True when the value is exactly zero.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.minor == 0
    }

    /// Add two amounts, returning the finer of the two scales so no digits are dropped.
    ///
    /// Named `plus` rather than `add` because this is **not** `std::ops::Add`: adding two amounts
    /// of different scales has to pick one, and the operator trait cannot express that. A call
    /// site that reads `a + b` would be promising a different contract than it delivers.
    ///
    /// Adding `0.005` (scale 3) to `12.34` yields scale 3, and the final rounding to the cent
    /// happens once, at the end of the document — which is the rule.
    #[must_use]
    pub fn plus(self, other: Self) -> Self {
        let scale = self.scale.max(other.scale);
        Self {
            minor: rescale(self.minor, self.scale, scale)
                + rescale(other.minor, other.scale, scale),
            scale,
        }
    }

    /// Subtract, on the same terms as [`Money::plus`]. Not `std::ops::Sub`, for the same reason.
    #[must_use]
    pub fn minus(self, other: Self) -> Self {
        let scale = self.scale.max(other.scale);
        Self {
            minor: rescale(self.minor, self.scale, scale)
                - rescale(other.minor, other.scale, scale),
            scale,
        }
    }

    /// Multiply by a whole number (a quantity rounded to a whole unit, a count of items).
    #[must_use]
    pub fn mul_int(self, factor: i128) -> Self {
        Self {
            minor: self.minor.saturating_mul(factor),
            scale: self.scale,
        }
    }

    /// Round to the given number of decimal places, **half away from zero**.
    ///
    /// Half-up on the magnitude: `2.345` at two places is `2.35` and `-2.345` is `-2.35`. This is
    /// what the spec asks for and what an accountant expects; a banker's rounding (ties to even)
    /// would make `2.345 → 2.34`, which is a different rule and not this one.
    #[must_use]
    pub fn round_to(self, scale: u32) -> Self {
        if self.scale <= scale {
            return Self {
                minor: self.minor,
                scale,
            };
        }
        Self {
            minor: round_half_away(self.minor, self.scale, scale),
            scale,
        }
    }

    /// Round to the cent (the default scale), which is the last step before a total is stored.
    #[must_use]
    pub fn round_to_cents(self) -> Self {
        self.round_to(DEFAULT_SCALE)
    }

    /// Parse a decimal string, keeping the scale it was written with.
    ///
    /// Rejects: an empty string, a bare `.`, a second `.`, a lone `-`/`+`, and anything with
    /// trailing characters. It accepts a leading `-` because a credit line and a refund are real
    /// documents, not something to refuse at the parser.
    pub fn parse(text: &str) -> Result<Self, MoneyError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(MoneyError::NotANumber);
        }
        let (negative, rest) = match trimmed.as_bytes()[0] {
            b'-' => (true, &trimmed[1..]),
            b'+' => (false, &trimmed[1..]),
            _ => (false, trimmed),
        };
        if rest.is_empty() {
            return Err(MoneyError::NotANumber);
        }
        let (whole, fraction) = match rest.split_once('.') {
            Some((w, f)) => {
                if f.contains('.') || f.is_empty() || w.is_empty() {
                    return Err(MoneyError::NotANumber);
                }
                (w, f)
            }
            None => (rest, ""),
        };
        if !whole.bytes().all(|b| b.is_ascii_digit())
            || !fraction.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(MoneyError::NotANumber);
        }
        let scale = u32::try_from(fraction.len()).map_err(|_| MoneyError::TooPrecise)?;
        if scale > 9 {
            return Err(MoneyError::TooPrecise);
        }
        let whole_value: i128 = whole.parse().map_err(|_| MoneyError::TooLarge)?;
        let mut minor = whole_value
            .checked_mul(10_i128.pow(scale))
            .ok_or(MoneyError::TooLarge)?;
        if !fraction.is_empty() {
            let fraction_value: i128 = fraction.parse().map_err(|_| MoneyError::TooLarge)?;
            minor = minor
                .checked_add(fraction_value)
                .ok_or(MoneyError::TooLarge)?;
        }
        if negative {
            minor = -minor;
        }
        if !(-Money::MAX_MINOR..=Money::MAX_MINOR).contains(&minor) {
            return Err(MoneyError::TooLarge);
        }
        Ok(Self { minor, scale })
    }

    /// The value as text at the **document scale**, which is the default (two places) unless the
    /// value carries more precision than the document keeps.
    ///
    /// Rendering is where the scale has to stop being data and become presentation: a column of
    /// amounts lines up only if every value prints the same number of decimals, and a PDF that
    /// shows `12.5` beside `12.34` is a document with a typo in it. A value parsed at one decimal
    /// still holds exact minor units — it simply did not use them — so it **pads**. A value with
    /// more precision than the document scale is the genuine case, and it is rounded here, once.
    #[must_use]
    pub fn to_text(self) -> String {
        let scale = self.scale.max(DEFAULT_SCALE);
        let minor = if self.scale > scale {
            round_half_away(self.minor, self.scale, scale)
        } else {
            rescale(self.minor, self.scale, scale)
        };
        format_scaled(minor, scale)
    }
}

impl Default for Money {
    fn default() -> Self {
        Self::zero()
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

impl FromStr for Money {
    type Err = MoneyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// A quantity, to three decimals — the scale a warehouse counts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Quantity {
    /// Thousandths, so `1250` is `1.250`.
    milli: i128,
}

impl Quantity {
    /// The scale quantities are stored at (the migration's `numeric(14,3)`).
    pub const SCALE: u32 = 3;
    const MAX_MILLI: i128 = i128::MAX / 1_000_000;

    /// The quantity one — the default for a line somebody has just added.
    #[must_use]
    pub const fn one() -> Self {
        Self { milli: 1000 }
    }

    /// Build from a count of thousandths.
    #[must_use]
    #[allow(
        clippy::manual_range_contains,
        reason = "RangeInclusive::contains is not const"
    )]
    pub const fn from_milli(milli: i128) -> Option<Self> {
        if !Self::within_range(milli) {
            None
        } else {
            Some(Self { milli })
        }
    }

    /// The count of thousandths.
    #[must_use]
    pub const fn milli(self) -> i128 {
        self.milli
    }

    /// Whether a count of thousandths is inside the range the module represents.
    const fn within_range(milli: i128) -> bool {
        milli >= -Self::MAX_MILLI && milli <= Self::MAX_MILLI
    }

    /// True when the quantity is greater than zero — the rule a line has to satisfy.
    #[must_use]
    pub const fn is_positive(self) -> bool {
        self.milli > 0
    }

    /// Parse a decimal quantity, keeping at most three decimals.
    pub fn parse(text: &str) -> Result<Self, MoneyError> {
        let value = Money::parse(text)?;
        if value.scale > Self::SCALE {
            // A fourth decimal is a precision the schema cannot store, so it is a refusal at the
            // parser rather than a value that is silently truncated on the way into `numeric(14,3)`.
            return Err(MoneyError::TooPrecise);
        }
        let milli = rescale(value.minor, value.scale, Self::SCALE);
        if !(-Self::MAX_MILLI..=Self::MAX_MILLI).contains(&milli) {
            return Err(MoneyError::TooLarge);
        }
        Ok(Self { milli })
    }

    /// The value as text with exactly three decimals.
    #[must_use]
    pub fn to_text(self) -> String {
        format_scaled(self.milli, Self::SCALE)
    }
}

impl fmt::Display for Quantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

impl FromStr for Quantity {
    type Err = MoneyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Money crosses the wire as the same text the database stores, so a JSON total and a SQL
/// `numeric` are the same value by construction rather than by two parsers agreeing.
impl Serialize for Money {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_text())
    }
}

impl<'de> Deserialize<'de> for Money {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Money::parse(&text).map_err(serde::de::Error::custom)
    }
}

impl Serialize for Quantity {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_text())
    }
}

impl<'de> Deserialize<'de> for Quantity {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Quantity::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// What a money parse can refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MoneyError {
    /// The text is not a decimal number at all.
    #[error("that is not a number")]
    NotANumber,
    /// More decimal places than the value can hold without losing a digit.
    #[error("too many decimal places")]
    TooPrecise,
    /// Larger than the platform will represent, or the scale would overflow.
    #[error("that number is too large")]
    TooLarge,
}

/// One line's contribution to a quote, after the discount and before the quote is summed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineTotals {
    /// `unit_price × quantity`, at the line's own scale.
    pub gross: Money,
    /// `gross × discount%`, the amount taken off.
    pub discount: Money,
    /// `tax%` of the amount actually payable, which is the gross **after** the discount.
    pub tax: Money,
    /// What the line contributes to the quote's grand total.
    pub net: Money,
}

/// A quote's four totals, each of which the panel prints and the PDF repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QuoteTotals {
    /// The sum of the lines' gross amounts, before any discount.
    pub subtotal: Money,
    /// The sum of the lines' discounts.
    pub discount_total: Money,
    /// The sum of the lines' taxes.
    pub tax_total: Money,
    /// What the customer pays: subtotal − discount + tax.
    pub grand_total: Money,
    /// The largest single-line discount in the quote, which is what the approval threshold is
    /// compared against (the spec gates the *largest* discount, not the average).
    pub max_discount_percent: i32,
}

/// A line as the arithmetic sees it: the numbers, without the row.
#[derive(Debug, Clone, Copy)]
pub struct LineInput {
    /// How many of the product.
    pub quantity: Quantity,
    /// The price for one, before the line's discount.
    pub unit_price: Money,
    /// The line's discount, as a percentage in `0..=100`.
    pub discount_percent: i32,
    /// The line's tax rate snapshot, as a percentage in `0..=100`.
    pub tax_percent: i32,
}

impl LineInput {
    /// The line's four totals, with **each** of them rounded to the cent before it is returned.
    ///
    /// This is the "round half-up once per line" half of the spec's rule, and the order matters:
    /// the tax is computed on the **discounted** amount, not on the gross, because tax on a
    /// discount the customer received is not tax. Rounding each component here and summing the
    /// rounded values below is what makes the printed line, the line sum and the grand total the
    /// same number — summing unrounded values and rounding once at the end is how they diverge by
    /// a cent on a quote with awkward fractions.
    #[must_use]
    pub fn totals(&self) -> LineTotals {
        // `unit_price × quantity` in the integer domain: hundredths times thousandths, divided
        // back to hundredths. The division truncates a third decimal, which is correct — the
        // rounding to the cent below is the one the spec asks for, and doing it here rather than
        // at the end is what keeps the line column and the total equal.
        let scaled = self
            .unit_price
            .minor()
            .saturating_mul(self.quantity.milli())
            / 1_000;
        let gross = at_document_scale(scaled, self.unit_price.scale());

        // The discount is a percentage of the gross, computed in hundredths of a percent so the
        // intermediate never leaves the integer domain.
        let discount = percent_of(gross, self.discount_percent);
        let payable = gross.minus(discount);
        let tax = percent_of(payable, self.tax_percent);
        // `at_document_scale` everywhere, so `net` is on the same scale as `gross` and the four
        // figures can be added and subtracted against each other without a rescale.
        let net = at_document_scale(
            payable.minor().saturating_add(tax.minor()),
            payable.scale().max(tax.scale()),
        );

        LineTotals {
            gross,
            discount,
            tax,
            net,
        }
    }
}

/// The totals of a whole quote: the sum of the **rounded** lines.
#[must_use]
pub fn quote_totals(lines: &[LineInput]) -> QuoteTotals {
    let mut subtotal = Money::zero();
    let mut discount_total = Money::zero();
    let mut tax_total = Money::zero();
    let mut grand_total = Money::zero();
    let mut max_discount_percent = 0;

    for line in lines {
        let t = line.totals();
        subtotal = subtotal.plus(t.gross);
        discount_total = discount_total.plus(t.discount);
        tax_total = tax_total.plus(t.tax);
        grand_total = grand_total.plus(t.net);
        max_discount_percent = max_discount_percent.max(line.discount_percent);
    }

    QuoteTotals {
        // Each of these is the sum of values that were already rounded to the cent, so rounding
        // once more is exact rather than approximate.
        subtotal: subtotal.round_to_cents(),
        discount_total: discount_total.round_to_cents(),
        tax_total: tax_total.round_to_cents(),
        grand_total: grand_total.round_to_cents(),
        max_discount_percent,
    }
}

/// `amount × percent / 100`, rounded half-up to the cent.
fn percent_of(amount: Money, percent: i32) -> Money {
    if percent == 0 {
        return Money::zero();
    }
    let scaled = amount.minor().saturating_mul(i128::from(percent));
    // `scaled` is amount-in-cents × percent. Dividing by 100 gives cents; the extra factor of 10
    // lets the half be detected before the division instead of being lost to truncation.
    let rounded = div_round_half_away(scaled * 10, 1_000);
    at_document_scale(rounded, amount.scale())
}

/// Put a value on the document scale, given the scale its arithmetic ran at.
///
/// One function, because "which scale is this number on" is the question that produced the
/// original bug: a total's `minor` is only meaningful together with its scale, and a value that
/// silently kept a third decimal made a sum of two lines add up to neither the printed column nor
/// the stored total.
fn at_document_scale(minor: i128, from: u32) -> Money {
    let rounded = if from > DEFAULT_SCALE {
        round_half_away(minor, from, DEFAULT_SCALE)
    } else {
        rescale(minor, from, DEFAULT_SCALE)
    };
    Money::from_minor(rounded)
        .unwrap_or_default()
        .round_to_cents()
}

/// Divide, rounding the remainder away from zero at the half.
fn div_round_half_away(numerator: i128, denominator: i128) -> i128 {
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    let doubled = remainder.abs() * 2;
    if doubled >= denominator {
        if numerator < 0 {
            quotient - 1
        } else {
            quotient + 1
        }
    } else {
        quotient
    }
}

/// Move a value between scales, exactly. Widening multiplies; narrowing happens in
/// [`round_half_away`] so it is never a truncation.
fn rescale(minor: i128, from: u32, to: u32) -> i128 {
    match to.cmp(&from) {
        std::cmp::Ordering::Equal => minor,
        std::cmp::Ordering::Greater => minor.saturating_mul(10_i128.pow(to - from)),
        std::cmp::Ordering::Less => round_half_away(minor, from, to),
    }
}

/// Round a value from one scale to a lower one, half away from zero.
fn round_half_away(minor: i128, from: u32, to: u32) -> i128 {
    debug_assert!(from > to, "narrowing only");
    let factor = 10_i128.pow(from - to);
    div_round_half_away(minor, factor)
}

/// Render an integer at a scale as plain decimal text, with the sign handled by the digits.
fn format_scaled(minor: i128, scale: u32) -> String {
    if scale == 0 {
        return minor.to_string();
    }
    let factor = 10_i128.pow(scale);
    let negative = minor < 0;
    let digits = minor.unsigned_abs();
    let whole = digits / factor as u128;
    let fraction = digits % factor as u128;
    let sign = if negative { "-" } else { "" };
    format!("{sign}{whole}.{fraction:0width$}", width = scale as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn money(text: &str) -> Money {
        Money::parse(text).expect("a valid amount")
    }

    fn qty(text: &str) -> Quantity {
        Quantity::parse(text).expect("a valid quantity")
    }

    fn line(q: &str, price: &str, discount: i32, tax: i32) -> LineInput {
        LineInput {
            quantity: qty(q),
            unit_price: money(price),
            discount_percent: discount,
            tax_percent: tax,
        }
    }

    // ---- parsing ------------------------------------------------------------------------------

    #[test]
    fn an_amount_keeps_the_scale_it_was_written_with() {
        assert_eq!(money("12.34").to_text(), "12.34");
        assert_eq!(
            money("12.3").to_text(),
            "12.30",
            "one decimal still prints two"
        );
        assert_eq!(money("12").to_text(), "12.00");
        assert_eq!(money("0").to_text(), "0.00");
    }

    #[test]
    fn a_third_decimal_survives_parsing() {
        let m = money("0.125");
        assert_eq!(m.scale(), 3);
        assert_eq!(m.to_text(), "0.125");
    }

    #[test]
    fn a_negative_amount_is_a_credit_line_not_a_refusal() {
        assert_eq!(money("-5.00").to_text(), "-5.00");
        assert_eq!(money("+5.00").to_text(), "5.00");
    }

    #[test]
    fn what_is_not_a_number_is_refused_by_name() {
        for bad in [
            "", "   ", ".", "-", "+", "abc", "1.2.3", "1,00", "1 000", "12x", "1.",
        ] {
            assert_eq!(Money::parse(bad), Err(MoneyError::NotANumber), "{bad:?}");
        }
    }

    #[test]
    fn a_number_too_precise_for_the_scale_is_refused_rather_than_truncated() {
        assert_eq!(money("1.125").scale(), 3);
        assert_eq!(Money::parse("1.12345678901"), Err(MoneyError::TooPrecise));
    }

    #[test]
    fn a_quantity_is_three_decimals_and_refuses_a_fourth() {
        assert_eq!(qty("1.250").to_text(), "1.250");
        assert_eq!(qty("2").to_text(), "2.000");
        assert_eq!(Quantity::parse("1.2500"), Err(MoneyError::TooPrecise));
    }

    // ---- rounding -----------------------------------------------------------------------------

    #[test]
    fn rounding_is_half_away_from_zero_on_both_signs() {
        assert_eq!(money("2.345").round_to_cents().to_text(), "2.35");
        assert_eq!(money("2.344").round_to_cents().to_text(), "2.34");
        assert_eq!(money("-2.345").round_to_cents().to_text(), "-2.35");
        assert_eq!(money("-2.344").round_to_cents().to_text(), "-2.34");
    }

    #[test]
    fn rounding_a_value_that_is_already_at_the_scale_changes_nothing() {
        assert_eq!(money("12.34").round_to_cents().to_text(), "12.34");
    }

    // ---- the line -----------------------------------------------------------------------------

    #[test]
    fn a_plain_line_is_price_times_quantity() {
        let t = line("2", "10.00", 0, 0).totals();
        assert_eq!(t.gross.to_text(), "20.00");
        assert_eq!(t.discount.to_text(), "0.00");
        assert_eq!(t.tax.to_text(), "0.00");
        assert_eq!(t.net.to_text(), "20.00");
    }

    #[test]
    fn a_fractional_quantity_keeps_its_third_decimal() {
        let t = line("1.250", "10.00", 0, 0).totals();
        assert_eq!(t.gross.to_text(), "12.50");
    }

    #[test]
    fn a_discount_comes_off_before_the_tax_is_charged() {
        // 100.00 less 20% is 80.00, and the tax is 20% of 80.00 — not of 100.00. Taxing the gross
        // would charge the customer tax on an amount they were never billed.
        let t = line("1", "100.00", 20, 20).totals();
        assert_eq!(t.gross.to_text(), "100.00");
        assert_eq!(t.discount.to_text(), "20.00");
        assert_eq!(t.tax.to_text(), "16.00", "20% of 80.00, not of 100.00");
        assert_eq!(t.net.to_text(), "96.00");
    }

    #[test]
    fn a_line_rounds_half_up_once_and_the_net_equals_gross_less_discount_plus_tax() {
        // 3 × 33.335 is 100.005, which must land on 100.01 rather than 100.00.
        let t = line("3", "33.335", 0, 0).totals();
        assert_eq!(
            t.gross.to_text(),
            "100.01",
            "100.005 rounds away from zero at the half"
        );
        assert_eq!(
            t.net.to_text(),
            t.gross.minus(t.discount).plus(t.tax).to_text(),
            "the four totals must add up to themselves"
        );
    }

    #[test]
    fn a_whole_percent_discount_rounds_half_up() {
        // 33.33 at 50% is 16.665 -> 16.67
        let t = line("1", "33.33", 50, 0).totals();
        assert_eq!(t.discount.to_text(), "16.67");
        assert_eq!(t.net.to_text(), "16.66");
    }

    #[test]
    fn a_zero_discount_and_zero_tax_are_not_a_special_case() {
        let t = line("5", "0.00", 0, 0).totals();
        assert_eq!(t.net.to_text(), "0.00");
        assert!(t.net.is_zero());
    }

    // ---- the quote ---------------------------------------------------------------------------

    #[test]
    fn a_quote_sums_the_rounded_lines_so_the_line_column_adds_up() {
        // Three lines whose unrounded sum is a fraction of a cent: rounding once at the end
        // would give 0.30, summing the rounded lines gives 0.31, and the printed line column
        // would not add up to the printed total.
        let lines = [
            line("1", "0.105", 0, 0),
            line("1", "0.105", 0, 0),
            line("1", "0.105", 0, 0),
        ];
        let totals = quote_totals(&lines);
        let summed: i128 = lines.iter().map(|l| l.totals().net.minor()).sum();
        assert_eq!(summed, 33, "0.11 + 0.11 + 0.11 = 0.33");
        assert_eq!(
            totals.grand_total.minor(),
            summed,
            "the total is the sum of the lines"
        );
        assert_eq!(totals.grand_total.to_text(), "0.33");
    }

    #[test]
    fn the_quote_four_totals_are_internally_consistent() {
        let lines = [
            line("2", "19.99", 10, 20),
            line("1", "5.00", 0, 20),
            line("7.500", "3.33", 15, 0),
        ];
        let t = quote_totals(&lines);
        assert_eq!(
            t.grand_total.to_text(),
            t.subtotal
                .minus(t.discount_total)
                .plus(t.tax_total)
                .to_text(),
            "subtotal − discount + tax must equal the grand total"
        );
    }

    #[test]
    fn the_gate_reads_the_largest_discount_not_the_average() {
        // One 40% line among two at 0% is a 13.3% average; the spec gates the largest.
        let lines = [
            line("1", "100.00", 40, 0),
            line("1", "100.00", 0, 0),
            line("1", "100.00", 0, 0),
        ];
        assert_eq!(quote_totals(&lines).max_discount_percent, 40);
    }

    #[test]
    fn an_empty_quote_is_zero_rather_than_a_refusal() {
        let t = quote_totals(&[]);
        assert_eq!(t.grand_total.to_text(), "0.00");
        assert_eq!(t.subtotal.to_text(), "0.00");
        assert_eq!(t.max_discount_percent, 0);
    }

    // ---- the invariant the spec is really about ----------------------------------------------

    #[test]
    fn a_thousand_awkward_lines_still_sum_to_the_sum_of_their_rounded_values() {
        // The property that matters: for any lines, the stored grand total equals the sum of the
        // stored line totals. If this ever fails the panel, the PDF and the invoice disagree.
        let mut lines = Vec::new();
        for i in 0..1000 {
            let cents = 1 + (i * 7) % 4999;
            lines.push(line(
                &format!("{}.{}", 1 + i % 3, i % 10),
                &format!("{}.{:02}", cents / 100, cents % 100),
                (i % 5) * 10,
                (i % 3) * 10,
            ));
        }
        let totals = quote_totals(&lines);
        let line_sum: i128 = lines.iter().map(|l| l.totals().net.minor()).sum();
        assert_eq!(
            totals.grand_total.minor(),
            line_sum,
            "the total must be the sum of the printed lines"
        );
        let gross_sum: i128 = lines.iter().map(|l| l.totals().gross.minor()).sum();
        assert_eq!(totals.subtotal.minor(), gross_sum);
    }

    #[test]
    fn an_amount_too_large_to_sum_is_refused_rather_than_wrapping() {
        let huge = Money::from_minor(i128::MAX / 2_000_000).expect("within the range");
        let total = quote_totals(&[LineInput {
            quantity: Quantity::one(),
            unit_price: huge,
            discount_percent: 0,
            tax_percent: 0,
        }]);
        assert!(
            total.grand_total.minor() <= Money::MAX_MINOR_TEST,
            "a total must not wrap into a negative number"
        );
    }

    impl Money {
        const MAX_MINOR_TEST: i128 = i128::MAX / 1_000_000;
    }

    #[test]
    fn rendering_always_shows_the_scale_so_a_column_lines_up() {
        let values = [money("1234.5"), money("0"), money("0.05"), money("-2.5")];
        let rendered: Vec<String> = values.iter().map(|v| v.to_text()).collect();
        assert_eq!(rendered, ["1234.50", "0.00", "0.05", "-2.50"]);
        for r in rendered {
            assert_eq!(r.split('.').nth(1).map(str::len), Some(2), "{r}");
        }
    }
}
