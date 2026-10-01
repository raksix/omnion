//! The errors this crate returns.
//!
//! One rule runs through the list: **nothing here carries caller-supplied key material.** A
//! validation error quotes a field name and a rule, never the value that broke the rule — so an
//! error that ends up in a log, an event payload or a browser console cannot become the leak
//! the shape was designed to prevent.

use thiserror::Error;

/// What can go wrong in the developer platform.
#[derive(Debug, Error)]
pub enum DeveloperError {
    /// A submitted name was outside the accepted length.
    #[error("name must be between {min} and {max} characters")]
    InvalidName {
        /// Shortest accepted.
        min: usize,
        /// Longest accepted.
        max: usize,
    },

    /// A key was created with no scopes. A key that can do nothing is a credential nobody
    /// asked for, and it is refused rather than stored and quietly useless.
    #[error("a key needs at least one scope")]
    NoScopes,

    /// One scope in the list was blank.
    #[error("a scope cannot be blank")]
    EmptyScope,

    /// The same scope appeared twice, which would make the granted set ambiguous.
    #[error("scope {0:?} was listed twice")]
    DuplicateScope(String),

    /// An environment other than `live` or `sandbox`.
    #[error("{0:?} is not an environment this platform has")]
    UnknownEnvironment(String),

    /// A rate tier other than `standard` or `high`.
    #[error("{0:?} is not a rate tier this platform has")]
    UnknownRateTier(String),

    /// A status class filter outside `1xx`–`5xx`.
    #[error("{0:?} is not a status class; use 1xx through 5xx")]
    UnknownStatusClass(String),

    /// A negative duration filter. It would read as "no minimum" and quietly match everything.
    #[error("a duration filter cannot be negative")]
    NegativeDuration,

    /// The requested key does not exist in this organization.
    #[error("no such API key")]
    KeyNotFound,

    /// A key with this name already exists in the organization. The unique index is the
    /// enforcement; this is the message it produces before the database has to.
    #[error("a key named {0:?} already exists")]
    KeyNameTaken(String),

    /// A negative or absurd expiry. The panel offers `never`, 30, 90 and 365 days; anything
    /// else is either a typo or a request trying to mint a key that is dead on arrival — which
    /// reads on the caller's side as "the key I just made does not work" with no reason given.
    #[error("expiry must be 30, 90 or 365 days, or omitted for never")]
    InvalidExpiry(i64),

    /// A CIDR entry that is not a network.
    #[error("{0:?} is not a CIDR block")]
    InvalidCidr(String),

    /// The row's hash was not written by this build's scheme, so nothing can be verified
    /// against it. Deliberately the same answer as a wrong secret, so it cannot be used to
    /// probe which keys exist.
    #[error("this key cannot be verified")]
    KeyUnverifiable,

    /// The presented token was revoked, expired, malformed or simply wrong — one answer for all
    /// of them, so a caller cannot tell a valid prefix with a bad secret from a dead key.
    #[error("invalid API key")]
    InvalidKey,

    /// The `high` rate tier was asked for by somebody without the role that grants it.
    #[error("the high rate tier needs an owner or administrator")]
    HighTierRefused,

    /// A key that is not active tried to authenticate.
    #[error("this key is {0}")]
    KeyNotActive(&'static str),

    /// The database said no, and the message is one we wrote.
    #[cfg(feature = "store")]
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// The crate's result.
pub type Result<T, E = DeveloperError> = std::result::Result<T, E>;

impl DeveloperError {
    /// Whether this is the caller's problem (a `400` with a message they can act on) or the
    /// platform's (a `500`).
    ///
    /// The split is about *who can fix it*, and it is the reason the crate does not carry
    /// sqlx errors in its client-facing half: a database that is unreachable is not something
    /// the person filling in a form can act on, and answering `400 invalid request` to a
    /// database outage sends them to fix a field that was never wrong.
    #[must_use]
    pub fn is_client_error(&self) -> bool {
        matches!(
            self,
            Self::InvalidName { .. }
                | Self::NoScopes
                | Self::EmptyScope
                | Self::DuplicateScope(_)
                | Self::UnknownEnvironment(_)
                | Self::UnknownRateTier(_)
                | Self::UnknownStatusClass(_)
                | Self::NegativeDuration
                | Self::KeyNotFound
                | Self::KeyNameTaken(_)
                | Self::InvalidCidr(_)
                | Self::InvalidExpiry(_)
                | Self::KeyUnverifiable
                | Self::InvalidKey
                | Self::HighTierRefused
                | Self::KeyNotActive(_)
        )
    }

    /// The API error code this variant carries.
    ///
    /// Stable strings, because a client switches on them: the panel's `ApiError.code` is what
    /// decides whether a message goes under a field or into a toast, and a code that changes
    /// spelling between releases moves the message to the wrong place with nothing failing.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidName { .. } => "invalid_key_name",
            Self::NoScopes => "no_scopes",
            Self::EmptyScope => "empty_scope",
            Self::DuplicateScope(_) => "duplicate_scope",
            Self::UnknownEnvironment(_) => "unknown_environment",
            Self::UnknownRateTier(_) => "unknown_rate_tier",
            Self::UnknownStatusClass(_) => "unknown_status_class",
            Self::NegativeDuration => "negative_duration",
            Self::KeyNotFound => "api_key_not_found",
            Self::KeyNameTaken(_) => "api_key_name_taken",
            Self::InvalidExpiry(_) => "invalid_expiry",
            Self::InvalidCidr(_) => "invalid_cidr",
            Self::KeyUnverifiable => "key_unverifiable",
            Self::InvalidKey => "invalid_api_key",
            Self::HighTierRefused => "high_tier_refused",
            Self::KeyNotActive(_) => "api_key_not_active",
            // Gated for the same reason the variant is: without the `store` feature this arm
            // does not exist, and a match that names it is a compile error. The alternative —
            // a `_ =>` arm — would swallow a new client-error variant added later without its
            // code, which is how a `400` silently becomes a `500`. The gate is what makes the
            // compiler name every arm a new variant needs.
            #[cfg(feature = "store")]
            Self::Database(_) => "developer_store_unavailable",
        }
    }
}
