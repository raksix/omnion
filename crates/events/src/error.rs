//! Errors of the event bus and the delivery path.
//!
//! The variants are the taxonomy the API surface maps onto HTTP: an endpoint that was never
//! registered (`EndpointNotFound`), a name already used inside the organization
//! (`EndpointNameTaken`), a definition the platform refuses to store (`Invalid*`), and the
//! store itself (`Store`).

/// Result alias of the events crate.
pub type Result<T> = std::result::Result<T, EventsError>;

/// What can go wrong while the platform records an event or delivers a webhook.
#[derive(Debug, thiserror::Error)]
pub enum EventsError {
    /// The database refused the read or the write.
    #[error("event store error: {0}")]
    Store(#[from] sqlx::Error),
    /// No endpoint with that id.
    #[error("no webhook endpoint carries that id")]
    EndpointNotFound,
    /// Another endpoint of the same organization already uses the name.
    #[error("a webhook endpoint named \"{0}\" already exists in this organization")]
    EndpointNameTaken(String),
    /// The endpoint definition is unusable.
    #[error("invalid webhook endpoint: {0}")]
    InvalidEndpoint(String),
    /// The event to record is unusable.
    #[error("invalid event: {0}")]
    InvalidEvent(String),
    /// The delivery HTTP client could not be built.
    #[error("webhook client error: {0}")]
    Client(String),
}

impl EventsError {
    /// Stable, machine-readable code for this failure.
    ///
    /// The API puts it in its error bodies, so a client can branch on the reason without
    /// reading prose.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Store(_) => "internal_error",
            Self::EndpointNotFound => "webhook_endpoint_not_found",
            Self::EndpointNameTaken(_) => "webhook_name_taken",
            Self::InvalidEndpoint(_) => "invalid_webhook_endpoint",
            Self::InvalidEvent(_) => "invalid_event",
            Self::Client(_) => "internal_error",
        }
    }
}

/// Smaller constructor for validation failures.
impl EventsError {
    /// An endpoint definition the platform will not store.
    #[must_use]
    pub fn invalid_endpoint(message: impl Into<String>) -> Self {
        Self::InvalidEndpoint(message.into())
    }

    /// An event the platform will not record.
    #[must_use]
    pub fn invalid_event(message: impl Into<String>) -> Self {
        Self::InvalidEvent(message.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_and_machine_readable() {
        assert_eq!(
            EventsError::EndpointNotFound.code(),
            "webhook_endpoint_not_found"
        );
        assert_eq!(
            EventsError::EndpointNameTaken("receiver".to_owned()).code(),
            "webhook_name_taken"
        );
        assert_eq!(
            EventsError::invalid_event("bad name").code(),
            "invalid_event"
        );
    }

    #[test]
    fn messages_carry_the_detail() {
        assert_eq!(
            EventsError::invalid_endpoint("url must be http(s)").to_string(),
            "invalid webhook endpoint: url must be http(s)"
        );
    }
}
