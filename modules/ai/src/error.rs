//! Errors of the AI workflow builder (docs/requests/REQ-046).
//!
//! The distinction callers act on is the one the request itself draws: a failure of the
//! *store* is the platform's problem ([`AiWorkflowError::Database`]), a failure of the
//! *answer* is the draft's ([`AiWorkflowError::Invalid`], carrying the stable `code` the HTTP
//! layer answers with), and a failure of the *provider* is [`AiWorkflowError::Ai`] — kept
//! separate because it is the only one that can be retried by asking the model again, which
//! is what the single repair round-trip does.

use thiserror::Error;

/// Everything that can go wrong storing, generating or validating a draft.
#[derive(Debug, Error)]
pub enum AiWorkflowError {
    /// The database refused the read or the write.
    #[error(transparent)]
    Database(#[from] sqlx::Error),

    /// The audit trail refused a row.
    #[error(transparent)]
    Audit(#[from] omnion_audit::AuditError),

    /// The AI Hub could not produce an answer, or refused to route the request.
    #[error("{0}")]
    Ai(String),

    /// The draft, the prompt or the answer breaks a rule.
    #[error("{message}")]
    Invalid {
        /// Stable machine-readable code, e.g. `invalid_draft_definition`.
        code: &'static str,
        /// Human-readable explanation.
        message: String,
    },
}

impl AiWorkflowError {
    /// Build an [`AiWorkflowError::Invalid`].
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
            Self::Database(_) => "ai_workflow_store_error",
            Self::Audit(_) => "ai_workflow_audit_error",
            Self::Ai(_) => "ai_provider_error",
            Self::Invalid { code, .. } => code,
        }
    }

    /// `true` when the failure is the provider's rather than the draft's.
    ///
    /// This is the question the repair round-trip asks. A *validation* failure is answered by
    /// asking the model again with the reason attached — the answer was close, and the model
    /// can be told exactly what was wrong. A *transport* failure is answered by trying again
    /// identically, and spending a repair on it would hand a second identical answer to a
    /// validator that will refuse it the same way.
    #[must_use]
    pub fn is_provider_failure(&self) -> bool {
        matches!(self, Self::Ai(_))
    }
}

/// Result alias of the module.
pub type Result<T> = std::result::Result<T, AiWorkflowError>;
