//! The shared vocabulary of the accounting module.
//!
//! Every word here is one the API, the screens and the migration all have to agree on, and the
//! reason they live in one file is that a screen which switches on a string the schema refuses
//! grows a permanently dead branch. Each enum has exactly one definition of "what does this mean
//! for the numbers", and it is the definition the SQL uses.
//!
//! The two that matter most:
//!
//! * [`AccountKind`] — the five top-level kinds, and the reason the chart is a **tree** with a
//!   parent pointer rather than a fixed depth. A hierarchy of a depth the data happens to have
//!   today is a limit, and a chart of accounts always grows deeper.
//! * [`EntrySource`] — what caused an entry. It is the link an auditor walks backwards from, and
//!   it is why the migration carries `source_kind`/`source_id` on the entry rather than a second
//!   lookup table: a person reconciling a bank line should not have to guess which table to open.

use serde::{Deserialize, Serialize};

/// The five top-level kinds of account.
///
/// The order of the variants is the order the tree screen renders, and the order reports sum by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    /// What the organization owns.
    Asset,
    /// What it owes.
    Liability,
    /// What the owners put in and leave in.
    Equity,
    /// What it earns.
    Income,
    /// What it spends.
    Expense,
}

impl AccountKind {
    /// The value stored in `accounting_accounts.kind`, which the check constraint allows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Asset => "asset",
            Self::Liability => "liability",
            Self::Equity => "equity",
            Self::Income => "income",
            Self::Expense => "expense",
        }
    }

    /// Read a stored kind.
    ///
    /// An unknown value is `None` rather than a default, so a row written by a newer version is
    /// reported instead of being shown as an asset.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "asset" => Some(Self::Asset),
            "liability" => Some(Self::Liability),
            "equity" => Some(Self::Equity),
            "income" => Some(Self::Income),
            "expense" => Some(Self::Expense),
            _ => None,
        }
    }

    /// Every kind, in report order.
    #[must_use]
    pub const fn all() -> [Self; 5] {
        [
            Self::Asset,
            Self::Liability,
            Self::Equity,
            Self::Income,
            Self::Expense,
        ]
    }

    /// The name the tree header prints.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Asset => "Assets",
            Self::Liability => "Liabilities",
            Self::Equity => "Equity",
            Self::Income => "Income",
            Self::Expense => "Expenses",
        }
    }
}

/// Which side of the sale a tax rate applies to.
///
/// A rate is **not** shared between the two, which is why `kind` is on the row and not derived:
/// an organization that sells at 20% and buys at a different rate has two rows with the same
/// name, and a single global "default rate" would let an expense be issued at the sales rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaxRateKind {
    /// Applied to a line on an invoice the organization issues.
    Sales,
    /// Applied to an expense the organization pays.
    Purchase,
}

impl TaxRateKind {
    /// The value stored in `accounting_tax_rates.kind`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sales => "sales",
            Self::Purchase => "purchase",
        }
    }

    /// Read a stored kind.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "sales" => Some(Self::Sales),
            "purchase" => Some(Self::Purchase),
            _ => None,
        }
    }

    /// Every kind, in the order the editor lists them.
    #[must_use]
    pub const fn all() -> [Self; 2] {
        [Self::Sales, Self::Purchase]
    }
}

/// What caused a journal entry.
///
/// `manual` is the only one a person chooses on the form; the other three are set by the route
/// that writes the document. The enum is closed so a fourth source cannot be spelled into a row
/// that no report groups by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntrySource {
    /// Typed by a person on the journal screen.
    Manual,
    /// Written when an invoice is issued.
    Invoice,
    /// Written when a payment is recorded.
    Payment,
    /// Written when an expense is approved.
    Expense,
}

impl EntrySource {
    /// The value stored in `accounting_journal_entries.source_kind`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Invoice => "invoice",
            Self::Payment => "payment",
            Self::Expense => "expense",
        }
    }

    /// Read a stored source.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "manual" => Some(Self::Manual),
            "invoice" => Some(Self::Invoice),
            "payment" => Some(Self::Payment),
            "expense" => Some(Self::Expense),
            _ => None,
        }
    }
}
