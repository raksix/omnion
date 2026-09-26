//! Errors of the analytics module.
//!
//! The distinction the API layer needs: a beacon the collector cannot use is a `400` (the
//! caller sent something wrong), settings that fail validation are a `400` naming the field, a
//! missing settings row is a `404`, and a database that will not answer is the retryable
//! dependency failure the rest of the platform reports.

use thiserror::Error;

/// Everything the analytics module can refuse to do.
#[derive(Debug, Error)]
pub enum AnalyticsError {
    /// The beacon is not usable at all (unparseable body, a path that is not a path).
    #[error("the beacon is not usable: {0}")]
    InvalidPayload(String),
    /// The beacon carries nothing to record.
    #[error("the beacon carries neither a pageview nor an event")]
    EmptyBeacon,
    /// The site has no settings row (it should have been seeded with the site).
    #[error("this site has no analytics settings")]
    SettingsNotFound,
    /// A settings update the platform refuses, with the field that caused it.
    #[error("invalid analytics settings: {0}")]
    InvalidSettings(String),
    /// PostgreSQL refused or could not answer.
    #[error("analytics storage error: {0}")]
    Database(#[from] sqlx::Error),
}

/// Result alias of the module.
pub type Result<T> = std::result::Result<T, AnalyticsError>;
