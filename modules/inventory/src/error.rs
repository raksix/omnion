//! Errors of the inventory module.
//!
//! The platform's rule, from REQ-051 onward and followed here rather than reinvented: input the
//! platform refuses is a `400` naming the field; a record that is not there — **or belongs to
//! another organization, which must be indistinguishable from not being there** — is a `404`; a
//! name or SKU that is taken is a `409`; and a write the module will not perform is its own
//! conflict, because nothing the caller typed is wrong and a form that only learns "cannot do
//! that" has nowhere to send the person next.
//!
//! Two of those are inventory's own:
//!
//! * [`InventoryError::WouldGoNegative`] carries the number that was available. "Insufficient
//!   stock" without the number makes the person count the shelf again; the whole point of the
//!   refusal is that the server knows.
//! * [`InventoryError::ApprovalNotGranted`] says the write is **waiting**, not wrong — the
//!   over-threshold adjustment, which changes nothing until somebody decides it.

use thiserror::Error;
use uuid::Uuid;

/// Everything the inventory module can refuse to do.
#[derive(Debug, Error)]
pub enum InventoryError {
    /// A description the platform refuses, naming the field to attach the message to.
    #[error("invalid {entity}.{field}: {message}")]
    Invalid {
        /// What was being written (`item`, `movement`, `transfer`, …).
        entity: &'static str,
        /// The field to attach the message to in the form.
        field: &'static str,
        /// The sentence the person reads.
        message: String,
    },
    /// A list query the platform refuses (an unknown sort column, a bad page size).
    #[error("invalid list query: {0}")]
    InvalidQuery(String),
    /// The scope mixes currencies, so there is no total to report.
    ///
    /// A module that summed across two currencies would produce a number that is
    /// arithmetically correct and commercially meaningless, and the person who quotes
    /// it to a customer is the one who finds out. Refusing is the honest answer; the
    /// currencies travel with the error so the screen can name them and the operator
    /// can see which item to re-price.
    #[error("this scope prices stock in more than one currency ({currencies:?}), so it has no single value")]
    MixedCurrency {
        /// The currencies the priced rows actually carry, sorted and deduplicated.
        currencies: Vec<String>,
    },
    /// The record does not exist in this organization.
    ///
    /// The message carries the kind and nothing else: a caller must not be able to learn that a
    /// record exists in another organization by comparing a 404 with a 403.
    #[error("no such {0} in this organization")]
    NotFound(&'static str),
    /// Another live record of the organization already carries this code or SKU.
    #[error("another {entity} of this organization is already called {code}")]
    CodeTaken {
        /// What the code belongs to (`item`, `warehouse`, `location`).
        entity: &'static str,
        /// The code that is taken.
        code: String,
    },
    /// A write the module will not perform because the numbers say no.
    ///
    /// `available` travels with it on purpose — the person standing at the shelf is the one who
    /// has to decide what to do about it, and a bare "insufficient stock" sends them back to the
    /// stock list to subtract it themselves.
    #[error("{message} (available {available})")]
    WouldGoNegative {
        /// The sentence, naming the item and the location.
        message: String,
        /// The quantity that is actually available to draw on, to the module's three decimals.
        available: String,
    },
    /// A transfer or a stocktake step the document's status does not allow.
    #[error("invalid status change: {0}")]
    InvalidStatusChange(String),
    /// A write that is waiting for a decision rather than being wrong.
    ///
    /// Its own variant because the recovery is not in the form: somebody else has to decide it.
    #[error("{0}")]
    ApprovalNotGranted(String),
    /// A quantity or amount the platform will not accept, carrying the parse failure's reason.
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
    /// The caller needs a permission this module cannot check for itself.
    ///
    /// The negative-stock rule is a **service** rule, not a schema rule: `inventory.negative.manage`
    /// is a permission of the organization, and only the HTTP layer can ask whether the caller
    /// holds it. The store therefore takes the answer as a boolean and this variant is how a
    /// deeper call (a replay, an import) refuses.
    #[error("{0}")]
    MissingPermission(String),
    /// A record referenced by id does not belong to this organization.
    #[error("{kind} {id} is not in this organization")]
    ForeignKey {
        /// What was referenced (`item`, `location`).
        kind: &'static str,
        /// The id that could not be used.
        id: Uuid,
    },
    /// PostgreSQL refused or could not answer.
    #[error("inventory storage error: {0}")]
    Database(#[from] sqlx::Error),
}

impl InventoryError {
    /// A refused write: the message the form renders under the field.
    #[must_use]
    pub fn invalid(entity: &'static str, field: &'static str, message: impl Into<String>) -> Self {
        Self::Invalid {
            entity,
            field,
            message: message.into(),
        }
    }

    /// A number the platform will not accept, carrying the parser's own reason.
    pub fn number(entity: &'static str, field: &'static str, source: crate::money::DecimalError) -> Self {
        Self::InvalidNumber {
            entity,
            field,
            source,
        }
    }

    /// A code that is already taken by a live record of the same kind.
    #[must_use]
    pub fn code_taken(entity: &'static str, code: impl Into<String>) -> Self {
        Self::CodeTaken {
            entity,
            code: code.into(),
        }
    }

    /// A draw that would leave the location below zero, carrying what is available.
    #[must_use]
    pub fn negative(message: impl Into<String>, available: impl Into<String>) -> Self {
        Self::WouldGoNegative {
            message: message.into(),
            available: available.into(),
        }
    }
}

/// Result alias of the module.
pub type Result<T> = std::result::Result<T, InventoryError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_names_the_entity_the_field_and_the_reason() {
        let sentence =
            InventoryError::invalid("item", "sku", "use 2–32 letters, digits, . _ or -").to_string();
        assert!(sentence.contains("item"), "{sentence}");
        assert!(sentence.contains("sku"), "{sentence}");
        assert!(sentence.contains("2–32"), "{sentence}");
    }

    #[test]
    fn a_missing_record_is_a_404_and_names_only_its_kind() {
        // The id must not be in the message: a 404 that echoes the id a caller supplied teaches
        // nothing new, but a 404 that differs from a 403's would confirm the row exists.
        assert_eq!(
            InventoryError::NotFound("item").to_string(),
            "no such item in this organization"
        );
    }

    #[test]
    fn going_negative_reports_the_number_that_is_available() {
        // The whole point of the variant: the person at the shelf is told what they can draw.
        let sentence = InventoryError::negative("Only 3.000 available at RETURNS", "3.000")
            .to_string();
        assert!(sentence.contains("3.000"), "{sentence}");
        assert!(sentence.contains("available"), "{sentence}");
    }

    #[test]
    fn a_taken_code_says_which_kind_of_record_took_it() {
        let sentence = InventoryError::code_taken("item", "BOLT-M8").to_string();
        assert!(sentence.contains("item"), "{sentence}");
        assert!(sentence.contains("BOLT-M8"), "{sentence}");
    }

    #[test]
    fn a_waiting_approval_does_not_read_like_a_bad_field() {
        let error = InventoryError::ApprovalNotGranted(
            "this adjustment changes 500.000 units, above the 100.000 threshold — it is waiting on a decision".into(),
        );
        let sentence = error.to_string();
        assert!(!sentence.contains("invalid"), "{sentence}");
        assert!(sentence.contains("waiting"), "{sentence}");
    }
}
