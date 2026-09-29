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

    /// A credential payload is not something the type can hold.
    ///
    /// Kept apart from [`WorkflowError::Invalid`] because each of these has its own stable
    /// code in the REQ's list (`credential_type_unknown`, `credential_field_required`,
    /// `credential_secret_write_only`, `credential_in_use`) and a caller branching on the code
    /// should not have to match on prose to tell them apart.
    #[error("credential {0}")]
    CredentialInvalid(String),

    /// A required field of the credential type was not sent.
    #[error("credential field {field:?} is required by its type")]
    CredentialFieldRequired {
        /// The field the type declares as required.
        field: String,
    },

    /// A secret field was sent where only the replace-secret path may write one.
    #[error(
        "credential field {field:?} is write-only — secrets are accepted once, on the \\
         replace-secret path, and are never read back; use POST \\
         /credentials/{{id}}/secret"
    )]
    CredentialSecretWriteOnly {
        /// The field the type declares as a secret.
        field: String,
    },

    /// The credential type is not in the registry, so no form or test hook exists for it.
    #[error("no credential type is registered under {0:?}")]
    CredentialTypeUnknown(String),

    /// A delete was refused because workflows still name this credential.
    #[error("credential {key:?} is used by {workflows} workflow(s)")]
    CredentialInUse {
        /// The credential's key.
        key: String,
        /// How many workflows name it.
        workflows: usize,
    },

    /// A scope or sharing value the platform does not have.
    #[error("credential {field} {value:?} is not one of {}", allowed.join(", "))]
    CredentialScopeDenied {
        /// Which field was refused.
        field: &'static str,
        /// The value the caller sent.
        value: String,
        /// The legal values.
        allowed: Vec<&'static str>,
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
            Self::CredentialInvalid(_) => "credential_invalid",
            Self::CredentialFieldRequired { .. } => "credential_field_required",
            Self::CredentialSecretWriteOnly { .. } => "credential_secret_write_only",
            Self::CredentialTypeUnknown(_) => "credential_type_unknown",
            Self::CredentialInUse { .. } => "credential_in_use",
            Self::CredentialScopeDenied { .. } => "credential_scope_denied",
        }
    }

    /// The human-readable explanation of an [`WorkflowError::Invalid`].
    ///
    /// A *method*, not a field, for the same reason `code()` is one: the enum has nine variants
    /// and only one of them carries a message, so a caller that wants the sentence would have to
    /// pattern-match a type that is mostly a database error. One accessor keeps that knowledge in
    /// `error.rs` — the graph's issue list puts the engine's own wording on the canvas, and reads
    /// it here without depending on `Display`'s shape.
    ///
    /// A store or audit failure has no sentence of its own and answers with
    /// [`STORE_FAILURE_MESSAGE`]; a caller that needs to tell the two apart asks
    /// [`WorkflowError::invalid_message`].
    #[must_use]
    pub fn message(&self) -> &str {
        self.invalid_message().unwrap_or(STORE_FAILURE_MESSAGE)
    }

    /// The message of an [`WorkflowError::Invalid`], or `None` for a store or audit failure.
    ///
    /// The version to reach for when the caller *branches* on whether there is a message to
    /// show, rather than always having a string to print.
    #[must_use]
    pub fn invalid_message(&self) -> Option<&str> {
        match self {
            Self::Invalid { message, .. } => Some(message),
            _ => None,
        }
    }
}

/// What [`WorkflowError::message`] returns for a variant that carries no sentence of its own.
const STORE_FAILURE_MESSAGE: &str = "the workflow store refused the operation";

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
