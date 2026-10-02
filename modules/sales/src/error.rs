//! Errors of the sales module.
//!
//! The distinction the API layer needs, mirroring the rest of the platform: input the platform
//! refuses is a `400` naming the field, a record that is not there (or belongs to another
//! organization, which must be indistinguishable from not being there) is a `404`, a name or SKU
//! that is already taken is a `409`, and **editing a document the customer has already seen** is
//! its own conflict rather than a validation failure — the caller is not wrong about the data,
//! they are asking for something the module will not do.

use thiserror::Error;
use uuid::Uuid;

/// Everything the sales module can refuse to do.
#[derive(Debug, Error)]
pub enum SalesError {
    /// A description the platform refuses, naming the field to attach the message to.
    #[error("invalid {entity}.{field}: {message}")]
    Invalid {
        /// What was being written (`product`, `price_list`, `quote`, `quote_line`, `order`, …).
        entity: &'static str,
        /// The field to attach the message to in the form.
        field: &'static str,
        /// The sentence the person reads.
        message: String,
    },
    /// A list query the platform refuses (an unknown sort column, a bad page size).
    #[error("invalid list query: {0}")]
    InvalidQuery(String),
    /// The record does not exist in this organization.
    #[error("no such {0} in this organization")]
    NotFound(&'static str),
    /// Another live product of the organization already carries this SKU.
    #[error("another product of this organization already uses this SKU")]
    SkuTaken,
    /// Another live record of the organization already carries this name.
    #[error("another {entity} of this organization is already called {name}")]
    NameTaken {
        /// What the name belongs to (`price list`, `quote`, …).
        entity: &'static str,
        /// The name that is taken.
        name: String,
    },
    /// A write against a document the customer has already seen.
    ///
    /// Its own variant because the two have different recoveries: a validation failure is fixed
    /// in the form, whereas a sent quote can only be changed by **duplicating** it into a new
    /// draft and sending a new version. Reporting this as a `400` would send the caller looking
    /// for a bad field that does not exist.
    #[error(
        "{entity} {number} has already been sent — duplicate it into a new draft instead of editing it"
    )]
    AlreadySent {
        /// What was being written (`quote`, `order`).
        entity: &'static str,
        /// The document's number, so the message names the document the person is looking at.
        number: String,
    },
    /// A status change the platform refuses (sending a cancelled quote, cancelling a delivered
    /// order, accepting an expired quote).
    #[error("invalid status change: {0}")]
    InvalidStatusChange(String),
    /// A public token that does not resolve: wrong, already consumed, or expired. One message for
    /// all three, so a caller cannot use the error to learn which tokens exist.
    #[error("this link is no longer valid")]
    InvalidPublicToken,
    /// A second approval request for a quote that already has one waiting.
    ///
    /// The **existing** request travels with the error rather than just its id: the seller who
    /// pressed the button twice must be shown the request that is already open, not a message
    /// telling them to go and find it, and a form that loses the state it just created is the
    /// bug this variant exists to prevent.
    #[error("quote {quote_number} is already waiting on an approval decision")]
    AlreadyAwaitingApproval {
        /// The quote's number, so the message names the document.
        quote_number: String,
        /// The request that is already open, rendered by the form as the row to show.
        ///
        /// **Boxed**: `ApprovalView` is a wide struct and this error is returned from every
        /// fallible function in the crate, so an unboxed payload would make the `Err` variant —
        /// and therefore every `Result` in the module — hundreds of bytes larger. `SalesError`
        /// is returned by pointer on the error path, and the cost is paid on the hot path.
        existing: Box<crate::approvals::ApprovalView>,
    },
    /// Somebody tried to approve their own quote.
    ///
    /// Its own variant because the recovery is not in the form: the seller has to ask **somebody
    /// else**. Reporting this as a plain `400` would put the message under a discount field and
    /// suggest the number is wrong, when the number is the whole point.
    #[error("you cannot decide your own approval request — ask someone else to review quote ({quote_status})")]
    SelfApproval {
        /// The quote's status, so the seller is told what to do next with it.
        quote_status: crate::model::QuoteStatus,
    },
    /// Somebody other than the requester tried to withdraw a request.
    #[error("only the person who raised this request can withdraw it (raised by {requester})")]
    NotRequester {
        /// The account that raised it.
        requester: Uuid,
    },
    /// A quote over the discount threshold was sent before anybody approved it.
    ///
    /// It is a `409` and not a `400` because nothing the caller typed is wrong: the document is
    /// fine, it is waiting for a decision. The **requirement travels with the error** so the form
    /// can print the discount, the limit and the open request — a bare "cannot send" would make
    /// the seller hunt for the button that clears it.
    #[error("{}", .0.message())]
    ApprovalNotGranted(Box<crate::approvals::ApprovalRequired>),
    /// An amount or quantity the platform will not accept, carrying the module's own reason.
    #[error("invalid {entity}.{field}: {source}")]
    InvalidNumber {
        /// What was being written.
        entity: &'static str,
        /// The field to attach the message to.
        field: &'static str,
        /// Why the number was refused.
        #[source]
        source: crate::money::MoneyError,
    },
    /// PostgreSQL refused or could not answer.
    #[error("sales storage error: {0}")]
    Database(#[from] sqlx::Error),
}

impl SalesError {
    /// A refused write: the message the form renders under the field.
    #[must_use]
    pub fn invalid(entity: &'static str, field: &'static str, message: impl Into<String>) -> Self {
        Self::Invalid {
            entity,
            field,
            message: message.into(),
        }
    }

    /// A refused number, carrying the money module's own reason to the field.
    pub fn number(
        entity: &'static str,
        field: &'static str,
        source: crate::money::MoneyError,
    ) -> Self {
        Self::InvalidNumber {
            entity,
            field,
            source,
        }
    }

    /// A name that is already taken.
    #[must_use]
    pub fn name_taken(entity: &'static str, name: impl Into<String>) -> Self {
        Self::NameTaken {
            entity,
            name: name.into(),
        }
    }

    /// A write against a document the customer has already seen.
    #[must_use]
    pub fn already_sent(entity: &'static str, number: impl Into<String>) -> Self {
        Self::AlreadySent {
            entity,
            number: number.into(),
        }
    }
}

impl From<crate::money::MoneyError> for SalesError {
    fn from(source: crate::money::MoneyError) -> Self {
        Self::InvalidNumber {
            entity: "quote",
            field: "amount",
            source,
        }
    }
}

/// Result alias of the module.
pub type Result<T> = std::result::Result<T, SalesError>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::MoneyError;

    #[test]
    fn a_refusal_names_the_entity_the_field_and_the_reason() {
        let sentence =
            SalesError::invalid("product", "sku", "use 2–32 letters, digits, . _ or -").to_string();
        assert!(sentence.contains("product"), "{sentence}");
        assert!(sentence.contains("sku"), "{sentence}");
        assert!(sentence.contains("2–32"), "{sentence}");
    }

    #[test]
    fn a_missing_record_is_a_404_and_names_only_its_kind() {
        // The message must not carry the id: a caller must not be able to learn that a record
        // exists in another organization by comparing a 404 with a 403.
        assert_eq!(
            SalesError::NotFound("quote").to_string(),
            "no such quote in this organization"
        );
    }

    #[test]
    fn an_already_sent_quote_says_what_to_do_instead_about_a_field() {
        let sentence = SalesError::already_sent("quote", "Q-2026-0007").to_string();
        assert!(sentence.contains("Q-2026-0007"), "{sentence}");
        assert!(sentence.contains("duplicate"), "{sentence}");
        assert!(
            !sentence.contains("invalid"),
            "this is not a validation failure and must not read like one: {sentence}"
        );
    }

    #[test]
    fn a_bad_number_carries_the_money_modules_own_reason() {
        let error = SalesError::number("quote_line", "quantity", MoneyError::TooPrecise);
        assert!(error.to_string().contains("quantity"), "{error}");
        assert!(
            error.to_string().contains("too many decimal places"),
            "{error}"
        );
    }

    #[test]
    fn a_taken_name_says_which_kind_of_record_took_it() {
        let sentence = SalesError::name_taken("price list", "Wholesale").to_string();
        assert!(sentence.contains("price list"), "{sentence}");
        assert!(sentence.contains("Wholesale"), "{sentence}");
    }
}
