//! Errors of the notification store.
//!
//! Like every other crate in the workspace this one never decides an HTTP status code — the API
//! layer maps it. The taxonomy is deliberately small: the store either refuses a definition
//! (something a caller could fix by changing the payload) or the database said no.

/// Result alias used across the notifications crate.
pub type Result<T, E = NotificationError> = std::result::Result<T, E>;

/// What can go wrong while a notification is recorded, read or delivered.
#[derive(Debug, thiserror::Error)]
pub enum NotificationError {
    /// A database operation failed.
    #[error("notification store: {0}")]
    Database(#[from] sqlx::Error),
    /// The category, priority or channel is not one the platform knows.
    #[error("invalid notification: {0}")]
    Invalid(String),
    /// The emit was refused because the actor is over its per-minute budget.
    #[error("notification emit budget exhausted")]
    BudgetExhausted,
}

impl NotificationError {
    /// Stable, machine-readable code for this failure.
    ///
    /// The API puts it in its error bodies so a client can branch on the reason without
    /// reading prose.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Database(_) => "internal_error",
            Self::Invalid(_) => "invalid_notification",
            Self::BudgetExhausted => "notification_rate_limited",
        }
    }

    /// Smaller constructor for a definition the platform will not store.
    #[must_use]
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_and_machine_readable() {
        assert_eq!(
            NotificationError::invalid("category").code(),
            "invalid_notification"
        );
        assert_eq!(NotificationError::BudgetExhausted.code(), "notification_rate_limited");
    }

    #[test]
    fn a_refusal_names_the_field() {
        assert_eq!(
            NotificationError::invalid("category \"invoice\" is not one of …").to_string(),
            "invalid notification: category \"invoice\" is not one of …"
        );
    }
}
