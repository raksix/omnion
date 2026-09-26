//! Errors of the automation layer.
//!
//! The shapes match the rest of the platform: a database failure is reported as itself, and a
//! rule that breaks a rule of the layer carries the stable `code` the HTTP layer answers with
//! (`invalid_conditions`, `invalid_binding`, …). Failures of an *action* are not errors of this
//! type at all — an action reports a message to the engine, which decides whether that means a
//! retry or a failed run.

use thiserror::Error;

/// Everything that can go wrong while storing or matching an automation rule.
#[derive(Debug, Error)]
pub enum AutomationError {
    /// The database refused the operation.
    #[error(transparent)]
    Database(#[from] sqlx::Error),

    /// The engine refused a definition the layer built.
    #[error(transparent)]
    Workflows(#[from] omnion_workflows::WorkflowError),

    /// The audit trail refused a row.
    #[error(transparent)]
    Audit(#[from] omnion_audit::AuditError),

    /// A rule breaks one of the layer's rules.
    #[error("{message}")]
    Invalid {
        /// Stable machine-readable code, e.g. `invalid_conditions`.
        code: &'static str,
        /// Human-readable explanation.
        message: String,
    },
}

impl AutomationError {
    /// Build an [`AutomationError::Invalid`].
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
            Self::Database(_) => "automation_store_error",
            Self::Workflows(err) => err.code(),
            Self::Audit(_) => "automation_audit_error",
            Self::Invalid { code, .. } => code,
        }
    }
}

/// Result alias of the crate.
pub type Result<T> = std::result::Result<T, AutomationError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_carries_its_code() {
        let error = AutomationError::invalid("invalid_conditions", "no such operator");
        assert_eq!(error.code(), "invalid_conditions");
        assert!(error.to_string().contains("no such operator"));
    }

    #[test]
    fn a_database_failure_uses_the_store_code() {
        assert_eq!(
            AutomationError::Database(sqlx::Error::RowNotFound).code(),
            "automation_store_error"
        );
    }
}
