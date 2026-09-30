//! Errors of the HR module.
//!
//! The distinction the API layer needs: input the platform refuses is a `400` naming the field,
//! a record that is not there (or is another organization's, which must be indistinguishable
//! from not being there) is a `404`, a record that already carries the number or the work e-mail
//! is a `409`, and a database that will not answer is the retryable dependency failure the rest of
//! the platform reports.
//!
//! The two cycle refusals have their own variants rather than a formatted [`HrError::Invalid`],
//! because they are the request's named acceptance criteria and a caller (and a test) has to be
//! able to match on the *kind* — "is this the self-manager case?" is a question about a variant,
//! and a substring test on a message is a question that breaks when somebody improves the wording.

use thiserror::Error;

/// Everything the HR module can refuse to do.
#[derive(Debug, Error)]
pub enum HrError {
    /// An employee or department description the platform refuses, naming the field that failed.
    #[error("invalid {entity}.{field}: {message}")]
    Invalid {
        /// What was being written (`employee`, `department`, …).
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
    /// Another employee of the organization already carries this employee number.
    #[error("employee number {0} is already used in this organization")]
    EmployeeNoTaken(String),
    /// Another employee of the organization already carries this work e-mail address.
    #[error("another employee of this organization already uses this work e-mail address")]
    WorkEmailTaken,
    /// Another department of the organization already carries this name.
    #[error("another department of this organization already carries this name")]
    DepartmentNameTaken,
    /// An employee may not be their own manager.
    #[error("an employee cannot be their own manager")]
    SelfManager,
    /// The proposed manager already reports, directly or through a chain, to this employee.
    ///
    /// The variant carries the chain because a person asked to fix a cycle needs to know *which*
    /// line closes it — "set someone as manager who is below you" sends them back to the tree to
    /// guess. The list is the management chain from the proposed manager up to the employee.
    ///
    /// The chain is rendered through a `Display` newtype rather than a `{}` placeholder calling a
    /// function: `thiserror`'s format strings are `format!`, and a bare `{join(0)}` is a field
    /// access it cannot resolve, not a function call.
    #[error("that employee already reports to this one through {0}")]
    ManagerCycle(ManagementChain),
    /// A department may not be moved under one of its own descendants.
    #[error("a department cannot be moved under itself or one of its own children")]
    DepartmentCycle,
    /// A department that still has members or child departments cannot be deleted.
    #[error("this department still has {members} members and {children} child departments; move them first")]
    DepartmentNotEmpty {
        /// Members that would be orphaned.
        members: i64,
        /// Child departments that would be orphaned.
        children: i64,
    },
    /// A merge whose two records are the same, or a merge of different organizations.
    #[error("invalid merge: {0}")]
    InvalidMerge(String),
    /// PostgreSQL refused or could not answer.
    #[error("hr storage error: {0}")]
    Database(#[from] sqlx::Error),
}

impl HrError {
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

/// The management chain a cycle refusal names, rendered as `a → b → c`.
///
/// A newtype rather than a bare `Vec<String>` in the error, for two reasons: the message needs the
/// arrow (so the person can see which way the chain runs) and an empty chain still has to read as
/// a sentence rather than as nothing at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementChain(Vec<String>);

impl ManagementChain {
    /// Wrap the names the store walked, nearest first.
    #[must_use]
    pub fn new(names: Vec<String>) -> Self {
        Self(names)
    }

    /// The names, for a caller that wants to render them itself.
    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.0
    }
}

impl std::fmt::Display for ManagementChain {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            return formatter.write_str("the reporting chain already in place");
        }
        formatter.write_str(&self.0.join(" → "))
    }
}

/// Result alias of the module.
pub type Result<T> = std::result::Result<T, HrError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_names_the_entity_the_field_and_the_reason() {
        let error = HrError::invalid("employee", "work_email", "that is not an e-mail address");
        let sentence = error.to_string();
        assert!(sentence.contains("employee"), "{sentence}");
        assert!(sentence.contains("work_email"), "{sentence}");
        assert!(sentence.contains("that is not an e-mail address"), "{sentence}");
    }

    #[test]
    fn a_missing_record_is_a_404_for_the_api_and_nothing_else() {
        assert_eq!(
            HrError::NotFound("employee").to_string(),
            "no such employee in this organization"
        );
    }

    #[test]
    fn the_cycle_refusals_are_their_own_variants_not_a_formatted_message() {
        // The acceptance criterion asks for a *cycle* to be refused, and the two cases are
        // different bugs to fix: one person is a fixed point, the other is a loop in the chain.
        // Matching on the variant is what makes that distinction testable at all.
        assert!(matches!(HrError::SelfManager, HrError::SelfManager));
        let cycle = HrError::ManagerCycle(ManagementChain::new(vec![
            "Ada".to_owned(),
            "Grace".to_owned(),
        ]));
        assert!(matches!(cycle, HrError::ManagerCycle(_)));
        assert!(cycle.to_string().contains("Ada → Grace"), "{cycle}");
    }

    #[test]
    fn an_empty_chain_still_reads_as_a_sentence() {
        // Unreachable through the store, and the message must not be a bare empty list if it ever
        // becomes reachable.
        let cycle = HrError::ManagerCycle(ManagementChain::new(Vec::new()));
        assert!(cycle.to_string().contains("the reporting chain already in place"));
    }

    #[test]
    fn a_non_empty_department_reports_both_counts() {
        let error = HrError::DepartmentNotEmpty {
            members: 4,
            children: 2,
        };
        let sentence = error.to_string();
        assert!(sentence.contains('4'), "{sentence}");
        assert!(sentence.contains('2'), "{sentence}");
    }
}
