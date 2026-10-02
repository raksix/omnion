//! Errors the object-storage layer reports.
//!
//! The variants separate what the operator has to do about a failure: fix the configuration
//! (invalid), start the object store (unavailable), look at a refused request (provider), or
//! accept that the object is simply not there (not found).

use thiserror::Error;

/// Result alias of the storage layer.
pub type Result<T> = std::result::Result<T, StorageError>;

/// What went wrong while talking to the object store.
#[derive(Debug, Error)]
pub enum StorageError {
    /// The object does not exist in the bucket (or in the storage directory).
    #[error("no object is stored under {key:?}")]
    NotFound {
        /// Object key that was addressed.
        key: String,
    },
    /// The configuration cannot be used (blank endpoint, unexpected scheme, bad key shape).
    #[error("{0}")]
    Invalid(String),
    /// The object store could not be reached at all.
    #[error("the object store is unreachable: {0}")]
    Unavailable(String),
    /// The object store answered and refused the request.
    #[error("the object store refused the request (status {status}): {message}")]
    Provider {
        /// HTTP status the store answered with.
        status: u16,
        /// Message the store sent back (trimmed, for logs and operators).
        message: String,
    },
    /// Local filesystem trouble in the development driver.
    #[error("the storage directory could not be used: {0}")]
    Io(String),
}

impl StorageError {
    /// `true` when the failure is worth retrying later — the store is there but not answering,
    /// or the local driver hit a transient problem.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Unavailable(_) | Self::Io(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_transport_failures_are_retryable() {
        assert!(StorageError::Unavailable("connection refused".to_owned()).is_retryable());
        assert!(StorageError::Io("disk full".to_owned()).is_retryable());
        assert!(
            !StorageError::NotFound {
                key: "a".to_owned()
            }
            .is_retryable()
        );
        assert!(
            !StorageError::Provider {
                status: 403,
                message: "denied".to_owned(),
            }
            .is_retryable()
        );
        assert!(!StorageError::Invalid("blank bucket".to_owned()).is_retryable());
    }

    #[test]
    fn the_provider_message_keeps_the_status() {
        let error = StorageError::Provider {
            status: 500,
            message: "internal".to_owned(),
        };
        assert!(error.to_string().contains("500"), "rendered: {error}");
    }
}
