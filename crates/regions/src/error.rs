//! The errors this crate returns (REQ-035).
//!
//! One rule runs through the list: **a refusal names a field and a rule, never a value the
//! caller could have got wrong in a way that leaks.** The only free-form strings are region
//! codes and service names, and both are closed vocabularies this crate owns — a country code
//! from a routing rule is the one exception and it is capped at 8 characters for that reason.

use thiserror::Error;

/// What can go wrong in the region surface.
#[derive(Debug, Error)]
pub enum RegionError {
    /// No region has that code.
    #[error("no region is registered with the code {0:?}")]
    UnknownRegion(String),

    /// A region code that does not match the shape the database enforces.
    #[error("a region code looks like `tr-ankara` — two lowercase letters, a dash, then a place name (max {max} characters)")]
    InvalidCode {
        /// The longest accepted code.
        max: usize,
    },

    /// A submitted display name was outside the accepted length.
    #[error("a region name must be between {min} and {max} characters")]
    InvalidDisplayName {
        /// Shortest accepted.
        min: usize,
        /// Longest accepted.
        max: usize,
    },

    /// A service other than the seven this platform runs.
    #[error("{0:?} is not a service this platform runs")]
    UnknownService(String),

    /// A service status outside the four the matrix renders.
    #[error("{0:?} is not a health status")]
    UnknownServiceStatus(String),

    /// A region status outside the four the registry carries.
    #[error("{0:?} is not a region status")]
    UnknownRegionStatus(String),

    /// A traffic share outside 0–100.
    #[error("a traffic share is a percentage between 0 and 100")]
    TrafficShareOutOfRange,

    /// A second region was made default without the first being cleared.
    ///
    /// A separate variant from the generic database error because the API layer has to render
    /// it as a *conflict with a named region*, and a `23505` reaching the panel as a 500
    /// would look like a broken installation rather than a rule the caller can satisfy.
    #[error("another region is already the default")]
    DefaultAlreadyTaken,

    /// A multi-region action on a deployment that has one region.
    ///
    /// Named rather than generic because the REQ requires these to be **disabled with an
    /// explanation**, not hidden: a single-region operator must see the surfaces and be told
    /// why they do nothing.
    #[error("this deployment runs a single region, so multi-region routing is inactive")]
    MultiRegionInactive,

    /// A country code that is not two letters.
    #[error("{0:?} is not a two-letter country code")]
    InvalidCountryCode(String),

    /// Persistence failed.
    #[cfg(feature = "store")]
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// The crate's result type.
pub type Result<T> = std::result::Result<T, RegionError>;
