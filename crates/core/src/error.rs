//! Shared error primitives.
//!
//! The core never decides HTTP status codes — it returns [`CoreError`] and the API layer
//! maps it onto the HTTP surface (`apps/api/src/error.rs`). That keeps `crates/core`
//! infrastructure-only and reusable by workers and the CLI.

/// An environment value that is missing, malformed or unsupported.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{key}: {message}")]
pub struct ConfigError {
    /// Environment key (or logical name) that failed validation.
    pub key: String,
    /// Human-readable explanation of what was expected.
    pub message: String,
}

impl ConfigError {
    /// Build an error for `key` carrying `message`.
    #[must_use]
    pub fn invalid(key: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            message: message.into(),
        }
    }
}

/// Error type shared by every infrastructure primitive in the core.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// Configuration could not be loaded or validated.
    #[error("configuration: {0}")]
    Config(#[from] ConfigError),
    /// A database operation failed.
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
    /// A migration could not be applied.
    #[error("migration: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    /// A Redis operation failed.
    #[error("redis: {0}")]
    Redis(#[from] redis::RedisError),
    /// Telemetry could not be initialised.
    #[error("telemetry: {0}")]
    Telemetry(String),
    /// A dependency did not answer in time (readiness probes treat this as not ready).
    #[error("{dependency} is unavailable: {message}")]
    Unavailable {
        /// Dependency name, e.g. `database` or `redis`.
        dependency: &'static str,
        /// What exactly went wrong (timeout, unexpected reply).
        message: String,
    },
    /// The process could not bind its socket or another I/O call failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Result alias used across the core.
pub type Result<T, E = CoreError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_error_names_the_offending_key() {
        let error = ConfigError::invalid("OMNION_PORT", "expected a TCP port number");
        assert_eq!(error.to_string(), "OMNION_PORT: expected a TCP port number");
    }

    #[test]
    fn unavailable_error_names_the_dependency() {
        let error = CoreError::Unavailable {
            dependency: "redis",
            message: "PING timed out".to_owned(),
        };
        assert_eq!(error.to_string(), "redis is unavailable: PING timed out");
    }

    #[test]
    fn config_errors_convert_into_core_errors() {
        let error: CoreError = ConfigError::invalid("OMNION_ENV", "unknown").into();
        assert!(matches!(error, CoreError::Config(_)));
        assert!(error.to_string().starts_with("configuration: "));
    }
}
