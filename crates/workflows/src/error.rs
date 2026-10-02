//! Errors of the workflow engine.
//!
//! Two kinds of failure exist and callers act on them differently: the database refused a
//! read or write ([`WorkflowError::Database`]), or a definition/trigger breaks a rule of the
//! engine ([`WorkflowError::Invalid`] — carries the stable `code` the HTTP layer answers with,
//! e.g. `invalid_step_action`).

use thiserror::Error;

/// Everything that can go wrong while storing or running a workflow.
#[derive(Debug, Error)]
pub enum WorkflowError {
    /// The database refused the operation.
    #[error(transparent)]
    Database(#[from] sqlx::Error),

    /// The audit trail refused a row — a state change is not reported as done without one.
    #[error(transparent)]
    Audit(#[from] omnion_audit::AuditError),

    /// The definition (or the trigger that addressed it) breaks an engine rule.
    #[error("{message}")]
    Invalid {
        /// Stable machine-readable code, e.g. `invalid_step_action`.
        code: &'static str,
        /// Human-readable explanation.
        message: String,
    },
}

impl WorkflowError {
    /// Build an [`WorkflowError::Invalid`].
    #[must_use]
    pub fn invalid(code: &'static str, message: impl Into<String>) -> Self {
        Self::Invalid {
            code,
            message: message.into(),
        }
    }

    /// Stable code of the failure, for the HTTP layer.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Database(_) => "workflow_store_error",
            Self::Audit(_) => "workflow_audit_error",
            Self::Invalid { code, .. } => code,
        }
    }
}

/// Result alias of the crate.
pub type Result<T> = std::result::Result<T, WorkflowError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_carries_its_code() {
        let error = WorkflowError::invalid("invalid_step_action", "no such action");
        assert_eq!(error.code(), "invalid_step_action");
        assert!(error.to_string().contains("no such action"));
    }

    #[test]
    fn database_errors_use_the_store_code() {
        let error = WorkflowError::Database(sqlx::Error::RowNotFound);
        assert_eq!(error.code(), "workflow_store_error");
    }
}
