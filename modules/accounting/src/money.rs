//! The money arithmetic of the ledger.
//!
//! `numeric(14,2)` has no Rust type in this workspace — a public repository does not take a
//! decimal dependency for one module — so **an amount crosses the SQL boundary as text** and is
//! parsed here into integer hundredths. The same shape the sales and inventory modules wrote, and
//! for the same reason: a balance computed through a binary float is a balance that is a cent
//! wrong, and in accounting a cent is a support ticket.
//!
//! Two decisions worth stating, because both are ways the data could have lied:
//!
//! * **A journal line is one side or the other.** [`LineAmount::parse`] takes a debit and a
//!   credit and refuses `0/0` and refuses `50/50`. Both-zero is a comment wearing a line's
//!   clothes, and it makes the entry's line count disagree with its arithmetic; both-positive is
//!   nonsense that a sum would then double-count.
//! * **The difference is signed.** [`Amount::signed_difference`] returns which way a mismatch
//!   runs, because the message an operator reads is "debits 100, credits 90, difference 10" and
//!   not the same sentence with the sign flipped, which says the opposite thing.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::AccountingError;

/// What can go wrong when text becomes a decimal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecimalError {
    /// The text is not a number.
    #[error("not a number")]
    NotANumber,
    /// More than two decimal places.
    #[error("more than two decimal places")]
    TooPrecise,
    /// More integer digits than `numeric(14,2)` holds.
    #[error("too many digits")]
    TooLarge,
    /// Negative where the field does not allow it.
    #[error("must not be negative")]
    Negative,
    /// Empty, which is what a form sends for a field nobody filled in.
    #[error("is required")]
    Required,
}

/// A money amount, to two decimals — `numeric(14,2)` as an integer count of hundredths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Amount {
    /// Hundredths, so `1234` is `12.34`.
    cents: i128,
}

/// The scale the schema stores amounts at.
pub const AMOUNT_SCALE: u32 = 2;

impl Amount {
    /// The amount zero.
    pub const ZERO: Self = Self { cents: 0 };

    /// Build from a count of hundredths, refusing a value `numeric(14,2)` could not hold.
    #[must_use]
    pub const fn from_cents(cents: i128) -> Option<Self> {
        // `numeric(14,2)` is 12 integer digits: 99_999_999_999_999 cents.
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

    /// True when the amount is exactly zero.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.cents == 0
    }

    /// Parse a decimal amount, keeping at most two decimals.
    pub fn parse(text: &str) -> Result<Self, DecimalError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(DecimalError::Required);
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
        if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
            return Err(DecimalError::NotANumber);
        }
        if !fraction.bytes().all(|b| b.is_ascii_digit()) {
            return Err(DecimalError::NotANumber);
        }
        // The third decimal is a **refusal**, not a truncation. An accountant typing `12.345` is
        // asking a question about rounding that the column cannot answer, and answering it by
        // silently dropping a digit hands them a different number than they wrote.
        if fraction.len() > AMOUNT_SCALE as usize {
            return Err(DecimalError::TooPrecise);
        }
        let fraction = format!("{fraction:0<width$}", width = AMOUNT_SCALE as usize);
        let value: i128 = format!("{whole}{fraction}")
            .parse()
            .map_err(|_| DecimalError::TooLarge)?;
        Self::from_cents(sign * value).ok_or(DecimalError::TooLarge)
    }

    /// The value as text with exactly two decimals, which is what binds back to `numeric`.
    #[must_use]
    pub fn to_text(self) -> String {
        let sign = if self.cents < 0 { "-" } else { "" };
        let whole = self.cents.unsigned_abs() / 100;
        let frac = self.cents.unsigned_abs() % 100;
        format!("{sign}{whole}.{frac:02}")
    }

    /// Add two amounts.
    ///
    /// Named `plus` rather than `add` because this is **not** `std::ops::Add`: the operator
    /// trait would let a call site read `a + b` without saying what the rule is, and a journal
    /// that summed in a different scale than the column it writes to would disagree with it.
    #[must_use]
    pub fn plus(self, other: Self) -> Self {
        Self {
            cents: self.cents + other.cents,
        }
    }

    /// Subtract two amounts, on the same terms as [`Amount::plus`].
    #[must_use]
    pub fn minus(self, other: Self) -> Self {
        Self {
            cents: self.cents - other.cents,
        }
    }

    /// The amount another amount differs by, **signed**: positive when `self` is the larger.
    ///
    /// Used for the two messages that have to name a direction — the balance refusal and the
    /// summary screen's "you are short by" — where an unsigned difference would be read as the
    /// opposite of what it is.
    #[must_use]
    pub fn signed_difference(self, other: Self) -> Self {
        Self {
            cents: self.cents - other.cents,
        }
    }
}

/// The absolute value of an amount, for a `numeric(14,2)` column that is `not null` but must not
/// be negative.
#[must_use]
pub fn absolute(amount: Amount) -> Amount {
    Amount::from_cents(amount.cents().abs()).unwrap_or(Amount::ZERO)
}

/// One side of a journal line: exactly one of these is non-zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LineAmount {
    /// The debit column.
    pub debit: Amount,
    /// The credit column.
    pub credit: Amount,
}

impl LineAmount {
    /// Parse a line's two sides, refusing the three shapes the schema also refuses.
    ///
    /// The database's CHECK is the last line of defence and this is the first: refusing here
    /// means the person sees which of the two fields to fix, while the constraint would surface
    /// as a constraint name. The rule is one side **or** the other — `0/0` and `50/50` are both
    /// nonsense, and both of them make an entry whose line count disagrees with its arithmetic.
    pub fn parse(debit: &str, credit: &str) -> Result<Self, DecimalError> {
        let debit = Amount::parse(debit)?;
        let credit = Amount::parse(credit)?;
        match (debit.is_zero(), credit.is_zero()) {
            (true, true) => Err(DecimalError::Required),
            (false, false) => Err(DecimalError::Required),
            _ => Ok(Self { debit, credit }),
        }
    }

    /// The signed value of the line, for the balance sum: debits positive, credits negative.
    #[must_use]
    pub fn signed(self) -> Amount {
        Amount::from_cents(self.debit.cents() - self.credit.cents()).unwrap_or(Amount::ZERO)
    }
}

/// The refusal a line hands back, naming the field so the grid can highlight a cell.
pub(crate) fn line_refusal(
    debit: &str,
    credit: &str,
) -> Result<LineAmount, AccountingError> {
    let debit_text = if debit.trim().is_empty() { "0" } else { debit };
    let credit_text = if credit.trim().is_empty() { "0" } else { credit };
    let parsed_debit = Amount::parse(debit_text).map_err(|source| {
        AccountingError::number("journal_line", "debit", source)
    })?;
    let parsed_credit = Amount::parse(credit_text).map_err(|source| {
        AccountingError::number("journal_line", "credit", source)
    })?;
    match (parsed_debit.is_zero(), parsed_credit.is_zero()) {
        (true, true) => Err(AccountingError::invalid(
            "journal_line",
            "debit",
            "a line carries a debit or a credit, not both and not neither",
        )),
        (false, false) => Err(AccountingError::invalid(
            "journal_line",
            "credit",
            "a line carries a debit or a credit, not both — clear one of them",
        )),
        _ => Ok(LineAmount {
            debit: parsed_debit,
            credit: parsed_credit,
        }),
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

impl Serialize for Amount {
    /// A **string** with two decimals, for the same reason the other modules do it: a money
    /// amount that crossed the wire as a JSON number is a double in the browser, and a balance
    /// drawn from a double is a balance that is a cent out.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_text())
    }
}

impl<'de> Deserialize<'de> for Amount {
    /// Accepts a string **and** a number: a form that posts `100` has said exactly the right
    /// thing, and refusing it would be pedantry at the boundary.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_amount_round_trips_through_text() {
        // The expectation is the module's own output format, not the input: `0` and `100` are
        // written as `0.00` and `100.00`, and a test that asserted the input back would be
        // asserting that the module does not format — which is the whole of its job.
        for (input, expected) in [
            ("0", "0.00"),
            ("0.00", "0.00"),
            ("12.34", "12.34"),
            ("100", "100.00"),
            ("1234.56", "1234.56"),
            ("-7.05", "-7.05"),
            ("  8.5  ", "8.50"),
        ] {
            let amount = Amount::parse(input).expect("parses");
            assert_eq!(amount.to_text(), expected, "{input} formats as {expected}");
            // And the formatted value parses back to itself, which is the property the SQL
            // binding depends on.
            assert_eq!(Amount::parse(&amount.to_text()).expect("round-trips"), amount);
        }
    }

    #[test]
    fn a_third_decimal_is_refused_rather_than_truncated() {
        // The rule from the sales module, kept because the alternative is an amount the person
        // did not type arriving in a financial document.
        assert_eq!(Amount::parse("12.345"), Err(DecimalError::TooPrecise));
        assert_eq!(Amount::parse("0.001"), Err(DecimalError::TooPrecise));
    }

    #[test]
    fn an_empty_amount_is_required_rather_than_not_a_number() {
        // A form that submits an empty field should say "is required", not "not a number": the
        // first is something the person can fix, the second is not.
        assert_eq!(Amount::parse("   "), Err(DecimalError::Required));
    }

    #[test]
    fn a_line_is_one_side_or_the_other() {
        assert!(LineAmount::parse("100", "0").is_ok());
        assert!(LineAmount::parse("0", "100").is_ok());
        // Both zero is a comment wearing a line's clothes.
        assert!(LineAmount::parse("0", "0").is_err());
        // Both positive is nonsense a sum would double-count.
        assert!(LineAmount::parse("50", "50").is_err());
    }

    #[test]
    fn the_signed_difference_says_which_way_the_mismatch_runs() {
        let debits = Amount::parse("100").expect("parses");
        let credits = Amount::parse("90").expect("parses");
        assert_eq!(debits.signed_difference(credits).to_text(), "10.00");
        // The same two numbers the other way round is the OPPOSITE statement, and an unsigned
        // difference would print the same sentence for both.
        assert_eq!(credits.signed_difference(debits).to_text(), "-10.00");
    }

    #[test]
    fn an_amount_above_the_column_is_refused_at_the_boundary() {
        let too_big = i128::from(100_000_000_000_000_i64);
        assert!(Amount::from_cents(too_big).is_none());
    }
}
