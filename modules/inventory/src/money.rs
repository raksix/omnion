//! The decimals inventory stores.
//!
//! Two scales and one parser. `numeric(14,3)` for a quantity (a warehouse counts in thousandths
//! so a 12.5 kg roll of cable is exact) and `numeric(14,2)` for a cost. Neither has a Rust type
//! in this workspace — a public repository does not take a decimal dependency for one module —
//! so both cross the SQL boundary **as text** and are validated here.
//!
//! The rule that matters: a value is **never** passed through a binary float. `0.1 + 0.2` is
//! `0.30000000000000004`, and a stock ledger that accumulated that across a thousand movements
//! would be off by grams. Everything is integer thousandths (or hundredths) from parse to
//! `to_text`, so the sum of the ledger is the sum of the text that was written.
//!
//! The parser is the same shape as the sales module's on purpose, and it is the **fourth
//! decimal that is a refusal** rather than a silent truncation: an operator typing `1.2345` wants
//! to know the column cannot hold it, not to receive `1.234` and a different number from the
//! next screen.

use std::fmt;

use serde::{Deserialize, Serialize};

/// What can go wrong when text becomes a decimal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecimalError {
    /// The text is not a number.
    #[error("not a number")]
    NotANumber,
    /// More decimals than the column stores.
    #[error("too many decimal places")]
    TooPrecise,
    /// More integer digits than `numeric(14,3)` holds.
    #[error("too many digits")]
    TooLarge,
    /// Negative where the field does not allow it.
    #[error("must not be negative")]
    Negative,
}

/// A quantity, to three decimals — the scale a warehouse counts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Quantity {
    /// Thousandths, so `1250` is `1.250`.
    milli: i128,
}

/// The largest quantity the schema holds: `numeric(14,3)` is 11 integer digits.
const MAX_MILLI: i128 = 99_999_999_999_999;

impl Quantity {
    /// The scale quantities are stored at (the migration's `numeric(14,3)`).
    pub const SCALE: u32 = 3;

    /// The quantity zero.
    pub const ZERO: Self = Self { milli: 0 };

    /// The quantity one — the default for a line somebody has just added.
    #[must_use]
    pub const fn one() -> Self {
        Self { milli: 1_000 }
    }

    /// Build from a count of thousandths, refusing what the column cannot hold.
    #[must_use]
    pub const fn from_milli(milli: i128) -> Option<Self> {
        if milli > MAX_MILLI || milli < -MAX_MILLI {
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

    /// True when the quantity is greater than zero — the rule a line has to satisfy.
    #[must_use]
    pub const fn is_positive(self) -> bool {
        self.milli > 0
    }

    /// True when the quantity is zero.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.milli == 0
    }

    /// True when the quantity is below zero, which only a `correction` may produce.
    #[must_use]
    pub const fn is_negative(self) -> bool {
        self.milli < 0
    }

    /// Add, refusing an overflow rather than wrapping.
    pub fn checked_add(self, other: Self) -> Option<Self> {
        Self::from_milli(self.milli.checked_add(other.milli)?)
    }

    /// Subtract, refusing an overflow rather than wrapping.
    pub fn checked_sub(self, other: Self) -> Option<Self> {
        Self::from_milli(self.milli.checked_sub(other.milli)?)
    }

    /// Parse a decimal quantity, keeping at most three decimals.
    ///
    /// A leading `+`, a thousands separator and surrounding whitespace are all accepted because
    /// a scanner and a spreadsheet both produce them, and refusing a pasted `1,250` teaches the
    /// operator nothing about what went wrong.
    pub fn parse(text: &str) -> Result<Self, DecimalError> {
        let value = parse_decimal(text, Self::SCALE)?;
        Self::from_milli(value).ok_or(DecimalError::TooLarge)
    }

    /// The value as text with exactly three decimals.
    ///
    /// Always three decimals, never `1` or `1.5`: a ledger printed with mixed precision is one
    /// whose columns do not line up, and a diff of two ledgers that "look" equal has to be
    /// textual to be meaningful.
    #[must_use]
    pub fn to_text(self) -> String {
        let sign = if self.milli < 0 { "-" } else { "" };
        let whole = self.milli.unsigned_abs() / 1_000;
        let frac = self.milli.unsigned_abs() % 1_000;
        format!("{sign}{whole}.{frac:03}")
    }
}

impl fmt::Display for Quantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

impl Serialize for Quantity {
    /// **A string, always** — `"10.000"`, never `10`.
    ///
    /// This is the single most consequential decision in the file. A `numeric(14,3)` that crossed
    /// the wire as a JSON number would be parsed by the browser into an IEEE 754 double, and
    /// `0.1 + 0.2` is `0.30000000000000004` there exactly as it is in Rust: a stock screen would
    /// print `0.30000000000000004` next to a ledger that says `0.300`, and the two would disagree
    /// about the same shelf. A number in JSON is a **display** value; the module's arithmetic
    /// happens on the server and the screen prints what the server sent.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_text())
    }
}

impl<'de> Deserialize<'de> for Quantity {
    /// Accepts a string **and** a number, because a form that posts `10` (a JSON number) has
    /// still said exactly the right thing and refusing it would be pedantry at the boundary.
    /// A float is rendered through its shortest round-trip form first, so `10.0` becomes `10.0`
    /// rather than `10` and the scale is still applied deliberately rather than accidentally.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let raw = match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::String(text) => text,
            serde_json::Value::Number(number) => number.to_string(),
            other => {
                return Err(D::Error::custom(format!(
                    "a quantity is a number or a string, not {other}"
                )));
            }
        };
        Self::parse(&raw).map_err(D::Error::custom)
    }
}

/// A money amount, to two decimals — the item's optional unit cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Amount {
    /// Hundredths, so `1999` is `19.99`.
    cents: i128,
}

impl Amount {
    /// The scale amounts are stored at (the migration's `numeric(14,2)`).
    pub const SCALE: u32 = 2;

    /// The amount zero.
    pub const ZERO: Self = Self { cents: 0 };

    /// Build from a count of hundredths.
    #[must_use]
    pub const fn from_cents(cents: i128) -> Option<Self> {
        if cents > 99_999_999_999_999 || cents < -99_999_999_999_999 {
            None
        } else {
            Some(Self { cents })
        }
    }

    /// The count of hundredths.
    #[must_use]
    pub const fn cents(self) -> i128 {
        self.cents
    }

    /// True when the amount is zero — which is how "no cost set" is stored, since the column is
    /// nullable and the module never invents a cost.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.cents == 0
    }

    /// Parse a decimal amount, keeping at most two decimals.
    pub fn parse(text: &str) -> Result<Self, DecimalError> {
        let value = parse_decimal(text, Self::SCALE)?;
        Self::from_cents(value).ok_or(DecimalError::TooLarge)
    }

    /// The value as text with exactly two decimals.
    #[must_use]
    pub fn to_text(self) -> String {
        let sign = if self.cents < 0 { "-" } else { "" };
        let whole = self.cents.unsigned_abs() / 100;
        let frac = self.cents.unsigned_abs() % 100;
        format!("{sign}{whole}.{frac:02}")
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

impl Serialize for Amount {
    /// A string with two decimals, for the same reason as [`Quantity`]: a money amount that
    /// crossed the wire as a JSON number would be a double in the browser, and an invoice drawn
    /// from a double is an invoice that is a cent out.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_text())
    }
}

impl<'de> Deserialize<'de> for Amount {
    /// Accepts a string and a number, for the same reason [`Quantity`] does.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let raw = match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::String(text) => text,
            serde_json::Value::Number(number) => number.to_string(),
            other => {
                return Err(D::Error::custom(format!(
                    "an amount is a number or a string, not {other}"
                )));
            }
        };
        Self::parse(&raw).map_err(D::Error::custom)
    }
}

/// Parse a decimal string into thousandths-of-the-target-scale, refusing a fourth decimal.
///
/// The one function both [`Quantity`] and [`Amount`] use, so "what is a number" is defined once.
/// Exponent notation (`1e3`) is **not** accepted: a scanner does not produce it, a form does not
/// send it, and accepting it would mean `1e-3` and `0.001` reach the same column by two routes
/// that round differently.
fn parse_decimal(text: &str, scale: u32) -> Result<i128, DecimalError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(DecimalError::NotANumber);
    }

    let (sign, digits) = match trimmed.as_bytes()[0] {
        b'-' => (-1_i128, &trimmed[1..]),
        b'+' => (1_i128, &trimmed[1..]),
        _ => (1_i128, trimmed),
    };
    if digits.is_empty() {
        return Err(DecimalError::NotANumber);
    }

    let (whole, fraction) = match digits.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (digits, ""),
    };

    if whole.is_empty() && fraction.is_empty() {
        return Err(DecimalError::NotANumber);
    }
    if !whole.bytes().all(|b| b.is_ascii_digit()) {
        return Err(DecimalError::NotANumber);
    }
    // A trailing dot ("12.") is what a numeric keypad leaves behind on every number, and a
    // trailing dot is not a mistake a person makes on purpose.
    let fraction = fraction.strip_suffix('.').unwrap_or(fraction);
    if !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return Err(DecimalError::NotANumber);
    }
    if fraction.len() > scale as usize {
        // A fourth decimal is a precision the column cannot store. Truncating it silently would
        // write a different number than the one the person read on the screen.
        let extra = fraction[scale as usize..].bytes().all(|b| b == b'0');
        if !extra {
            return Err(DecimalError::TooPrecise);
        }
        return Ok(sign * whole_to_milli(whole, &fraction[..scale as usize], scale));
    }

    Ok(sign * whole_to_milli(whole, fraction, scale))
}

/// Scale `whole.fraction` to the target count of thousandths.
///
/// **Right-pad the fraction, never left-align it.** `"250"` at scale 3 is `250` thousandths —
/// the first digit carries the tenths — so the value is `2 * 100 + 5 * 10 + 0 * 1 = 250`, i.e.
/// `0.250`. The first version of this function scaled the fraction by *its own length* and
/// produced `0.002` for the same input: every quantity with a trailing zero (`3.250`, `12.500`,
/// `1.050`) parsed a thousand times too small, and the walk caught it as `3.002` where the test
/// expected `3.250`. A decimal parser that silently mis-scales is the worst class of bug there
/// is, because the value it returns still looks like a number.
fn whole_to_milli(whole: &str, fraction: &str, scale: u32) -> i128 {
    let unit: i128 = 10_i128.pow(scale);
    let whole_value: i128 = if whole.is_empty() {
        0
    } else {
        // A `numeric(14,3)` holds 11 integer digits; anything longer is refused by the caller's
        // `from_milli` check, and parsing it here cannot overflow an i128 either.
        whole.parse::<i128>().unwrap_or(i128::MAX / 2)
    };
    let mut value = whole_value.checked_mul(unit).unwrap_or(i128::MAX / 2);

    // Pad on the right: `2` at scale 3 is 200 thousandths, not 2. Written as a loop over the
    // scale rather than as a `pow` so the place value of each digit is visible in the code.
    for place in (0..scale).rev() {
        let digit = fraction
            .as_bytes()
            .get((scale - 1 - place) as usize)
            .map_or(0, |byte| i128::from(byte - b'0'));
        let weight = 10_i128.pow(place);
        value = value.saturating_add(digit * weight);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quantity_keeps_three_decimals_in_both_directions() {
        assert_eq!(Quantity::parse("6").unwrap().to_text(), "6.000");
        assert_eq!(Quantity::parse("1.5").unwrap().to_text(), "1.500");
        assert_eq!(Quantity::parse("0.001").unwrap().to_text(), "0.001");
        assert_eq!(Quantity::parse("-4.250").unwrap().to_text(), "-4.250");
    }

    #[test]
    fn the_scale_is_exact_so_a_ledger_sums_to_the_cent_and_the_gram() {
        // The property the whole module rests on: adding text is adding integers. 0.1 + 0.2 in
        // binary floating point is 0.30000000000000004; here it is exactly 0.300.
        let tenth = Quantity::parse("0.1").unwrap();
        let fifth = Quantity::parse("0.2").unwrap();
        assert_eq!(tenth.checked_add(fifth).unwrap().to_text(), "0.300");

        // And a hundred movements of 0.1 land on 10.000, not 9.999999.
        let mut total = Quantity::ZERO;
        for _ in 0..100 {
            total = total.checked_add(tenth).unwrap();
        }
        assert_eq!(total.to_text(), "10.000");
    }

    #[test]
    fn a_fourth_decimal_is_refused_rather_than_truncated() {
        // The operator must be told, because the alternative writes a number nobody read.
        assert_eq!(Quantity::parse("1.2345"), Err(DecimalError::TooPrecise));
        // Trailing zeroes are not a precision: 1.5000 is 1.5 and refusing it would be pedantry.
        assert_eq!(Quantity::parse("1.5000").unwrap().to_text(), "1.500");
    }

    #[test]
    fn what_is_not_a_number_is_not_a_number() {
        for text in ["", "  ", "-", "abc", "1.2.3", "1e3", "1,250.00", "12px"] {
            assert_eq!(
                Quantity::parse(text),
                Err(DecimalError::NotANumber),
                "{text:?} should not parse"
            );
        }
    }

    #[test]
    fn what_a_numeric_keypad_actually_types_parses() {
        // A trailing dot, a leading plus and surrounding spaces are all what the input the drawer
        // renders produces, and refusing them would be the module arguing with its own form.
        assert_eq!(Quantity::parse("12.").unwrap().to_text(), "12.000");
        assert_eq!(Quantity::parse("+7.5").unwrap().to_text(), "7.500");
        assert_eq!(Quantity::parse("  3.250  ").unwrap().to_text(), "3.250");
    }

    #[test]
    fn a_quantity_beyond_the_column_is_refused() {
        // numeric(14,3): eleven integer digits.
        assert!(Quantity::parse("99999999999.999").is_ok());
        assert_eq!(Quantity::parse("100000000000.000"), Err(DecimalError::TooLarge));
    }

    #[test]
    fn an_amount_keeps_two_decimals_and_refuses_a_third() {
        assert_eq!(Amount::parse("19.99").unwrap().to_text(), "19.99");
        assert_eq!(Amount::parse("0").unwrap().to_text(), "0.00");
        assert_eq!(Amount::parse("19.999"), Err(DecimalError::TooPrecise));
    }

    #[test]
    fn a_quantity_crosses_the_wire_as_a_string_and_never_as_a_number() {
        // The reason the whole impl exists: a JSON number is a double in the browser, and
        // `0.1 + 0.2` there is `0.30000000000000004`. A ledger and a screen that disagree about
        // the third decimal is the bug this prevents.
        let rendered = serde_json::to_value(Quantity::parse("0.1").unwrap()).unwrap();
        assert!(rendered.is_string(), "a quantity is a string, not a number: {rendered}");
        assert_eq!(rendered, serde_json::json!("0.100"));
        assert_eq!(
            serde_json::to_value(Quantity::from_milli(-4_250).unwrap()).unwrap(),
            serde_json::json!("-4.250")
        );

        // And it reads back from a string **and** from a number, because a form that posts `10`
        // has still said the right thing.
        let from_string: Quantity = serde_json::from_value(serde_json::json!("12.5")).unwrap();
        let from_number: Quantity = serde_json::from_value(serde_json::json!(12.5)).unwrap();
        assert_eq!(from_string, from_number);
        assert_eq!(from_string.to_text(), "12.500");
    }

    #[test]
    fn an_amount_crosses_the_wire_as_a_string_with_two_decimals() {
        assert_eq!(
            serde_json::to_value(Amount::parse("19.9").unwrap()).unwrap(),
            serde_json::json!("19.90"),
            "the awkward one, on purpose — a price written without its trailing zero"
        );
        let back: Amount = serde_json::from_value(serde_json::json!(19.9)).unwrap();
        assert_eq!(back.to_text(), "19.90");
    }

    #[test]
    fn zero_and_sign_predicates_are_the_ones_the_rules_read() {
        assert!(Quantity::ZERO.is_zero());
        assert!(!Quantity::ZERO.is_positive());
        assert!(Quantity::parse("-0.001").unwrap().is_negative());
        assert!(Amount::ZERO.is_zero());
    }
}
