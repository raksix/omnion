//! Errors of the health centre.
//!
//! Like every other crate in the workspace this one never decides an HTTP status
//! code — the API layer maps it. The taxonomy is small, and the distinction that
//! matters is between *a probe could not reach* and *the store refused*:
//!
//! * A probe that cannot read is **not** an error. It is a `down` row with a
//!   message, because the whole point of the screen is to show that dependency as
//!   unhealthy. Turning a refused connection into a `Result::Err` and letting the
//!   caller skip the row would render a stopped Redis as a *missing* Redis, which
//!   on a status screen is the most dangerous way to be wrong.
//! * Everything here is therefore genuinely exceptional: a store failure, or a
//!   request for a service key the registry does not have.

/// Result alias used across the health crate.
pub type Result<T, E = HealthError> = std::result::Result<T, E>;

/// What can go wrong while samples are stored or settings are written.
#[derive(Debug, thiserror::Error)]
pub enum HealthError {
    /// A database operation failed.
    #[error("health store: {0}")]
    Database(#[from] sqlx::Error),
    /// The caller sent something the platform will not store: a service key the
    /// registry does not have, a threshold pair that runs the wrong way round, an
    /// interval outside the range the spec names. Always the caller's to fix.
    #[error("invalid health input: {0}")]
    Invalid(String),
    /// The row the caller named does not exist.
    #[error("not found")]
    NotFound,
}

impl HealthError {
    /// Stable, machine-readable code for this failure.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Database(_) => "internal_error",
            Self::Invalid(_) => "invalid_health_input",
            Self::NotFound => "not_found",
        }
    }

    /// Smaller constructor for a definition the platform will not store.
    #[must_use]
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}
