//! Error type of the identity store.
//!
//! Like the core, this crate never decides HTTP status codes: it returns [`IdentityError`]
//! and the API layer maps it onto the HTTP surface (`apps/api/src/error.rs`).

/// Errors returned by the identity store.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// A database operation failed.
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
    /// The supplied email address is not usable as an account address.
    #[error("invalid email address: {0}")]
    InvalidEmail(String),
    /// A session token is not in the expected shape.
    #[error("invalid session token: {0}")]
    InvalidToken(String),
    /// The password does not satisfy the minimum policy.
    #[error("password must be at least {min} characters long")]
    WeakPassword {
        /// Minimum accepted length.
        min: usize,
    },
    /// The email address is already registered (case-insensitive).
    #[error("email address is already registered")]
    EmailTaken,
    /// Password hashing or verification failed (broken hash, unsupported parameters).
    #[error("password hashing failed: {0}")]
    PasswordHash(String),
    /// A blocking hashing task could not be joined.
    #[error("hashing task failed: {0}")]
    Task(String),
}

/// Result alias used across the identity crate.
pub type Result<T, E = IdentityError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weak_password_error_states_the_minimum() {
        let error = IdentityError::WeakPassword { min: 10 };
        assert_eq!(
            error.to_string(),
            "password must be at least 10 characters long"
        );
    }

    #[test]
    fn taken_email_error_is_explicit() {
        assert_eq!(
            IdentityError::EmailTaken.to_string(),
            "email address is already registered"
        );
    }
}
