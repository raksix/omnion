//! Error type of the audit store.
//!
//! Like the other crates, it never decides HTTP status codes — the API layer maps it.

/// Errors returned by the audit store.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// A database operation failed.
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
}

/// Result alias used across the audit crate.
pub type Result<T, E = AuditError> = std::result::Result<T, E>;
