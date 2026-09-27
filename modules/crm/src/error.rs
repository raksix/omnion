//! Errors of the CRM module.
//!
//! The distinction the API layer needs: input the platform refuses is a `400` naming the field,
//! a record that is not there (or is another organization's, which must be indistinguishable
//! from not being there) is a `404`, a record that already carries the name or the address is a
//! `409`, and a database that will not answer is the retryable dependency failure the rest of the
//! platform reports.

use thiserror::Error;

/// Everything the CRM module can refuse to do.
#[derive(Debug, Error)]
pub enum CrmError {
    /// A contact, company or deal description the platform refuses, naming the field that failed.
    #[error("invalid {entity}.{field}: {message}")]
    Invalid {
        /// What was being written (`contact`, `company`, `deal`, `activity`, …).
        entity: &'static str,
        /// The field to attach the message to in the form.
        field: &'static str,
        /// The sentence the person reads.
        message: String,
    },
    /// A list query the platform refuses (an unknown sort column, a cursor that is not a cursor).
    #[error("invalid list query: {0}")]
    InvalidQuery(String),
    /// The record does not exist in this organization.
    #[error("no such {0} in this organization")]
    NotFound(&'static str),
    /// Another live record of the organization already carries this address.
    #[error("another contact of this organization already uses this e-mail address")]
    EmailTaken,
    /// Another live company of the organization already carries this name.
    #[error("another company of this organization already carries this name")]
    CompanyNameTaken,
    /// A merge whose two records are the same, or a merge of different organizations.
    #[error("invalid merge: {0}")]
    InvalidMerge(String),
    /// A stage move the platform refuses (unknown stage, a won deal without a close date, a lost
    /// deal without a reason).
    #[error("invalid stage change: {0}")]
    InvalidStageChange(String),
    /// PostgreSQL refused or could not answer.
    #[error("crm storage error: {0}")]
    Database(#[from] sqlx::Error),
}

impl CrmError {
    /// A refused write: the message the form renders under the field.
    #[must_use]
    pub fn invalid(entity: &'static str, field: &'static str, message: impl Into<String>) -> Self {
        Self::Invalid {
            entity,
            field,
            message: message.into(),
        }
    }

    /// A refused write naming a field that the schema would refuse too.
    #[must_use]
    pub fn constraint(entity: &'static str, field: &'static str, message: impl Into<String>) -> Self {
        Self::invalid(entity, field, message)
    }
}

/// Result alias of the module.
pub type Result<T> = std::result::Result<T, CrmError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_names_the_entity_the_field_and_the_reason() {
        let error = CrmError::invalid("contact", "email", "that is not an e-mail address");
        let sentence = error.to_string();
        assert!(sentence.contains("contact"), "{sentence}");
        assert!(sentence.contains("email"), "{sentence}");
        assert!(sentence.contains("that is not an e-mail address"), "{sentence}");
    }

    #[test]
    fn a_missing_record_is_a_404_for_the_api_and_nothing_else() {
        assert_eq!(CrmError::NotFound("contact").to_string(), "no such contact in this organization");
    }
}
