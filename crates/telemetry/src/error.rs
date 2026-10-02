//! The crate's error type.
//!
//! Two variants and no more, because the crate has exactly two ways to fail in a way a caller
//! can act on: the store refused something the caller asked for ([`TelemetryError::WindowTooWide`],
//! which the route turns into a `400` naming the cap), and something else went wrong
//! ([`TelemetryError::Telemetry`], which is a `500`).
//!
//! The window variant exists as its own case rather than as a formatted string because the
//! settings screen and the explorer both need to *say the cap* — a generic "invalid request"
//! tells an operator nothing about why their 30-day query was refused.

/// Anything the observability store can refuse or fail at.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    /// The requested window is older than the documented retention cap.
    #[error("the log store keeps {days} days; ask for a narrower window or raise the retention")]
    WindowTooWide {
        /// The cap, in days.
        days: i64,
    },
    /// Anything else: a query failed, a row could not be decoded, the schema is missing.
    #[error("{0}")]
    Telemetry(String),
}

impl From<sqlx::Error> for TelemetryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Telemetry(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_refusal_names_the_cap() {
        let error = TelemetryError::WindowTooWide { days: 30 };
        let message = error.to_string();
        assert!(
            message.contains("30 days"),
            "the cap must be in the message: {message}"
        );
    }

    #[test]
    fn a_sqlx_error_becomes_a_telemetry_error_rather_than_panicking() {
        let error: TelemetryError = sqlx::Error::RowNotFound.into();
        assert!(matches!(error, TelemetryError::Telemetry(_)));
    }
}
