//! Error type of the secrets crate.
//!
//! Like every other store in the workspace, this crate never decides HTTP status codes: it
//! returns [`SecretsError`] and the API layer maps it onto the HTTP surface
//! (`apps/api/src/error.rs`).

use crate::keyring::{KEY_ENCRYPTION_ENV, KEY_ENCRYPTION_FILE_ENV};

/// Errors returned by the secrets store.
#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    /// A database operation failed.
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
    /// An envelope could not be read: the wrong key, a tampered ciphertext, or a version whose
    /// `key_id` names a root key the operator has since deleted.
    #[error("the stored secret could not be unsealed")]
    Crypto,
    /// No operator key-encryption key is configured, so nothing can be sealed or unsealed.
    #[error(
        "no operator key is available: set {KEY_ENCRYPTION_ENV} or \
         {KEY_ENCRYPTION_FILE_ENV} to a key-encryption key"
    )]
    OperatorKeyMissing,
    /// The ring has no active key yet (a fresh installation, or a rotation that failed before
    /// it activated one).
    #[error("the installation has no active root key")]
    NoActiveKey,
    /// A rotation is already walking the ring. Only one re-wrap job may be live at a time,
    /// because two would re-wrap the same versions under different keys.
    #[error("a key rotation is already in progress")]
    RotationInProgress,
    /// A key name, kind, slot or scope is not one the schema allows.
    #[error("{0}")]
    Invalid(String),
    /// No secret, version, root key, lease, deployment key or slot assignment carries the id.
    #[error("no such {0}")]
    NotFound(&'static str),
    /// A read-only (`file` / `env`) secret cannot be written to. Structurally true: those rows
    /// carry no envelope, so there is nothing to write.
    #[error("this secret is read-only and managed outside the platform")]
    ReadOnly,
    /// A lease is spent, expired or revoked.
    #[error("the lease is no longer usable: {0}")]
    LeaseUnavailable(&'static str),
    /// A deployment key is past its expiry or revoked.
    #[error("the deployment key is not usable: {0}")]
    DeploymentKeyUnavailable(&'static str),
}

impl SecretsError {
    /// The stable machine-readable code the API surfaces; also the audit row's `code`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Database(_) => "secrets_database_error",
            Self::Crypto => "secret_unsealable",
            Self::OperatorKeyMissing => "operator_key_missing",
            Self::NoActiveKey => "no_active_root_key",
            Self::RotationInProgress => "rotation_in_progress",
            Self::Invalid(_) => "invalid_secrets_request",
            Self::NotFound(what) => match *what {
                "secret" => "secret_not_found",
                "version" => "secret_version_not_found",
                "root key" => "root_key_not_found",
                "lease" => "lease_not_found",
                "deployment key" => "deployment_key_not_found",
                "slot" => "credential_slot_not_found",
                _ => "secrets_not_found",
            },
            Self::ReadOnly => "secret_read_only",
            Self::LeaseUnavailable(_) => "lease_unavailable",
            Self::DeploymentKeyUnavailable(_) => "deployment_key_unavailable",
        }
    }
}

/// Result alias used across the secrets crate.
pub type Result<T, E = SecretsError> = std::result::Result<T, E>;
