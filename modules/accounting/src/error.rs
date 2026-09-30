//! Errors of the accounting module.
//!
//! The platform's rule from REQ-051 onward, followed rather than reinvented: a refused field is a
//! `400` naming the field, a row that is not there — **or belongs to another organization, which
//! must be indistinguishable from not being there** — is a `404`, a code or a name that is taken
//! is a `409`, and a write the module will not perform is its own conflict.
//!
//! Accounting adds exactly one thing to that list, and it is the reason this file exists:
//! [`AccountingError::UnbalancedEntry`]. Every other module's "will not perform" is a sentence —
//! "insufficient stock", "this quote has been sent" — that leaves the person with a decision. An
//! unbalanced journal entry has no decision: the numbers are simply wrong, and the operator is
//! staring at two totals and a difference. So this variant carries **all three numbers**, and the
//! route renders them into the message rather than surfacing a constraint name. A refusal whose
//! text is `accounting_journal_entries_check` has told the reader nothing and told the database
//! everything.

use thiserror::Error;
use uuid::Uuid;

/// The result every fallible function in this crate returns.
///
/// One alias rather than a spelled-out `Result<T, AccountingError>` at every signature: a module
/// whose error type is named in forty places has forty places to forget it, and the compiler does
/// not check the ones that were forgotten.
pub type Result<T> = std::result::Result<T, AccountingError>;

/// Everything the accounting module can refuse to do.
#[derive(Debug, Error)]
pub enum AccountingError {
    /// A field the platform refuses, named so the form can attach the message to it.
    #[error("invalid {entity}.{field}: {message}")]
    Invalid {
        /// What was being written (`account`, `tax_rate`, `journal_entry`, `journal_line`).
        entity: &'static str,
        /// The field the message belongs under.
        field: &'static str,
        /// The sentence the person reads.
        message: String,
    },
    /// A list query the platform refuses (an unknown sort column, a bad page size).
    #[error("invalid list query: {0}")]
    InvalidQuery(String),
    /// The record does not exist in this organization.
    ///
    /// The message carries the kind and nothing else. A caller must not be able to learn that a
    /// record exists in another organization by comparing a `404` with a `403`, and one
    /// organization's chart of accounts is the thing this module exists to keep apart.
    #[error("no such {0} in this organization")]
    NotFound(&'static str),
    /// Another live record of the organization already carries this code or name.
    #[error("another {entity} of this organization is already called {code}")]
    NameTaken {
        /// What the code belongs to (`account`, `tax_rate`).
        entity: &'static str,
        /// The code or name that is taken.
        code: String,
    },
    /// **The entry does not balance.** Carries the two totals and the difference.
    ///
    /// Not a validation error and not a conflict: the inputs parsed, the lines are individually
    /// legal, and the statement that would have written them is refused because the arithmetic
    /// says no. The numbers travel because the person fixing it is looking at a grid of debits
    /// and credits and needs to be told which way the difference runs.
    #[error(
        "the entry does not balance: debits {debit_total}, credits {credit_total}, \
         difference {difference}"
    )]
    UnbalancedEntry {
        /// The sum of the debit column, to the cent.
        debit_total: String,
        /// The sum of the credit column, to the cent.
        credit_total: String,
        /// `debit_total - credit_total`, signed, so the direction is readable from the message.
        difference: String,
    },
    /// **An allocation asks for more than the invoice still owes.**
    ///
    /// Its own variant, and a `422` rather than the family's usual `400` or `409`, because it is
    /// the one refusal in this module whose answer is a *number* rather than a rule. The person
    /// fixing it is looking at an outstanding balance and needs to be told what it is, what they
    /// asked for, and by how much the two disagree — "payment rejected" sends them to a report.
    /// The route maps this one status to `422` while every other refusal in the family keeps
    /// its own, which is what the REQ's "refused with a 422 unless the override permission is
    /// held" asks for.
    #[error(
        "invoice {invoice_number} has {outstanding} outstanding, which is less than the \
         {attempted} this payment allocates to it"
    )]
    OverAllocation {
        /// The invoice's number.
        invoice_number: String,
        /// What the payment asked to apply.
        attempted: String,
        /// What the invoice actually has left.
        outstanding: String,
    },
    /// A stored amount this build cannot read.
    ///
    /// Not [`AccountingError::InvalidNumber`], because that one is about a value a **caller**
    /// sent and belongs to a field on the form. This is a row the database handed back, so the
    /// person who caused it is the migration or the drift, and the entity/field of the form
    /// would point them at the wrong screen.
    #[error("a stored amount is unreadable: {message}")]
    InvalidAmount {
        /// What could not be parsed, named so the log names a column.
        message: String,
    },
    /// A write the module will not perform for a reason the caller can act on.
    #[error("{0}")]
    NotAllowed(String),
    /// An account that has postings cannot be deleted.
    ///
    /// Its own variant rather than [`AccountingError::NotAllowed`] because the number of lines is
    /// the answer: "cannot delete" sends the operator to a report to find out how exposed the
    /// account is, and the count is a single indexed lookup.
    #[error("account {code} has {lines} journal lines; deactivate it instead of deleting it")]
    AccountInUse {
        /// The account's code.
        code: String,
        /// How many journal lines name it.
        lines: i64,
    },
    /// A record referenced by id is not in this organization.
    #[error("{kind} {id} is not in this organization")]
    ForeignKey {
        /// What was referenced (`account`, `organization`).
        kind: &'static str,
        /// The id that could not be used.
        id: Uuid,
    },
    /// An amount the platform will not accept, carrying the parse failure's reason.
    #[error("invalid {entity}.{field}: {source}")]
    InvalidNumber {
        /// What was being written.
        entity: &'static str,
        /// The field to attach the message to.
        field: &'static str,
        /// Why the number was refused.
        #[source]
        source: crate::money::DecimalError,
    },
    /// PostgreSQL refused or could not answer.
    #[error("accounting storage error: {0}")]
    Database(#[from] sqlx::Error),
}

impl AccountingError {
    /// A refused field: the message the form renders under the input.
    #[must_use]
    pub fn invalid(entity: &'static str, field: &'static str, message: impl Into<String>) -> Self {
        Self::Invalid {
            entity,
            field,
            message: message.into(),
        }
    }

    /// A number the platform will not accept.
    #[must_use]
    pub fn number(
        entity: &'static str,
        field: &'static str,
        source: crate::money::DecimalError,
    ) -> Self {
        Self::InvalidNumber {
            entity,
            field,
            source,
        }
    }

    /// A write the module will not perform, with a sentence the person can act on.
    #[must_use]
    pub fn not_allowed(message: impl Into<String>) -> Self {
        Self::NotAllowed(message.into())
    }
}

/// The HTTP status the API layer should answer with.
///
/// Written here rather than in the route so the two cannot drift: a refusal that is a `404` in the
/// module and a `409` in the route is a route that tells a caller a row of another organization
/// exists, and that is the one mistake this family of modules is built to never make.
#[must_use]
pub fn status_of(error: &AccountingError) -> u16 {
    match error {
        AccountingError::Invalid { .. }
        | AccountingError::InvalidQuery(_)
        | AccountingError::InvalidNumber { .. } => 400,
        AccountingError::NotFound(_) | AccountingError::ForeignKey { .. } => 404,
        AccountingError::NameTaken { .. }
        | AccountingError::UnbalancedEntry { .. }
        | AccountingError::AccountInUse { .. }
        | AccountingError::NotAllowed(_) => 409,
        // The one `422` in the family, and deliberately so: an over-allocation is a request that
        // was well formed, referred to a real invoice, and asked for something the arithmetic
        // refuses. `400` would say the request was malformed and `409` would say the row is in
        // conflict — neither is true, and both send a client to the wrong branch of its error
        // handling.
        AccountingError::OverAllocation { .. } => 422,
        AccountingError::InvalidAmount { .. } => 500,
        AccountingError::Database(_) => 500,
    }
}
