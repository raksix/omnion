//! Errors of the security centre.
//!
//! Like every other crate in the workspace this one never decides an HTTP status code — the API
//! layer maps it. The taxonomy is small and each variant answers a different question an
//! operator will ask: *was it my fault, or yours?*

/// Result alias used across the security crate.
pub type Result<T, E = SecurityError> = std::result::Result<T, E>;

/// What can go wrong while posture is evaluated, findings are read or a status is changed.
#[derive(Debug, thiserror::Error)]
pub enum SecurityError {
    /// A database operation failed.
    #[error("security store: {0}")]
    Database(#[from] sqlx::Error),
    /// The caller sent something the platform will not store: a severity that is not one of
    /// ours, an ignore without a reason, a title that is empty. Always the caller's to fix.
    #[error("invalid security input: {0}")]
    Invalid(String),
    /// The row the caller named does not exist, or belongs to another organization.
    ///
    /// One variant, deliberately: "no such finding" and "a finding you may not see" must be
    /// the same answer, or the detail route becomes a probe for what exists.
    #[error("finding not found")]
    NotFound,
}

impl SecurityError {
    /// Stable, machine-readable code for this failure.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Database(_) => "internal_error",
            Self::Invalid(_) => "invalid_security_input",
            Self::NotFound => "not_found",
        }
    }

    /// Smaller constructor for a definition the platform will not store.
    #[must_use]
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}
