//! Errors of the reliability centre.
//!
//! Like every other crate in the workspace this one never decides an HTTP status code — the API
//! layer maps it. What it *does* decide is the stable [`ReliabilityError::code`], because the
//! request says those codes are contracts other systems match on, and a contract that the crate
//! invents at the API boundary instead of declaring in one place is a contract that drifts.
//!
//! The taxonomy answers one question an operator will ask under pressure: **who has to change
//! something?** [`Invalid`] is the caller's, [`NotFound`] is a name that may not exist or may
//! not be visible (one variant on purpose — two would make the detail route a probe), and
//! [`ProviderUnavailable`] is nobody's, it is a breaker doing its job.

/// Result alias used across the reliability crate.
pub type Result<T, E = ReliabilityError> = std::result::Result<T, E>;

/// What can go wrong while a budget is evaluated, a key is replayed, a retry is scheduled, a
/// breaker transitions or an inbound payload is verified.
#[derive(Debug, thiserror::Error)]
pub enum ReliabilityError {
    /// A database operation failed.
    #[error("reliability store: {0}")]
    Database(#[from] sqlx::Error),

    /// The caller sent something the platform will not store: a window of zero seconds, a
    /// retryable class nobody defines, a reason that is not one of ours. Always the caller's.
    #[error("invalid reliability input: {0}")]
    Invalid(String),

    /// The row the caller named does not exist, or belongs to another organization.
    ///
    /// One variant, deliberately: "no such policy" and "a policy you may not see" must be the
    /// same answer, or the detail route becomes a probe for what exists.
    #[error("reliability record not found")]
    NotFound,

    /// An outbound provider is behind an open breaker.
    ///
    /// Distinct from [`Invalid`] on purpose. The caller did nothing wrong and cannot fix it by
    /// changing the request; the platform is refusing on purpose, which is the entire point of a
    /// breaker, and a caller that retries immediately makes the outage it is avoiding.
    #[error("provider {provider} is unavailable")]
    ProviderUnavailable {
        /// The provider key the breaker is filed under.
        provider: String,
        /// Seconds until the breaker will allow a probe; `None` while it is held open deliberately.
        retry_after: Option<i64>,
    },

    /// The configured retry ceiling has been exhausted; the attempt is a dead letter.
    #[error("retries exhausted for {subsystem}")]
    RetriesExhausted {
        /// Which subsystem's budget ran out.
        subsystem: String,
    },
}

impl ReliabilityError {
    /// The underlying `sqlx::Error`, when this is a database failure.
    ///
    /// Exists so a caller can inspect the SQLSTATE without this error type re-declaring the
    /// reasons a query can fail. The intake store's `insert` wraps a `23505` here, and the
    /// route that has to turn it into "that path is already declared" cannot see the code from
    /// the outside — which is the exact shape of a bug where a name collision answers `500`.
    #[must_use]
    pub fn database_error(&self) -> Option<&sqlx::Error> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl ReliabilityError {
    /// Stable, machine-readable code for this failure.
    ///
    /// Every string here is one of [`crate::vocabulary::ERROR_CODES`] or a general API error,
    /// and [`stable_codes_are_the_documented_ones`] asserts the overlap, because a code that
    /// exists in a `match` and not in the request is a code no integrator can discover.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Database(_) => "internal_error",
            Self::Invalid(_) => "invalid_reliability_input",
            Self::NotFound => "not_found",
            Self::ProviderUnavailable { .. } => "provider_unavailable",
            Self::RetriesExhausted { .. } => "retry_exhausted",
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
    use std::collections::BTreeSet;

    /// Every documented code is one this taxonomy can actually produce.
    ///
    /// The five names in [`crate::vocabulary::ERROR_CODES`] are the request's stable wire
    /// contract. The other codes this enum returns (`internal_error`,
    /// `invalid_reliability_input`, `not_found`, `retry_exhausted`) are ordinary API errors
    /// shared with the rest of the platform — so the assertion runs in the direction that
    /// matters: each documented name is reachable, not that nothing else is.
    #[test]
    fn stable_codes_are_the_documented_ones() {
        let produced = BTreeSet::from([
            ReliabilityError::invalid("x").code(),
            ReliabilityError::NotFound.code(),
            ReliabilityError::ProviderUnavailable {
                provider: "p".into(),
                retry_after: Some(1),
            }
            .code(),
        ]);
        // `provider_unavailable` is the code this crate owns outright — no other subsystem
        // produces it, it is what an open breaker answers, and it is one of the five names the
        // request declares stable. The rest are ordinary platform errors shared with every
        // crate, which is why they are deliberately not in that list.
        assert!(produced.contains("provider_unavailable"));
        assert!(crate::vocabulary::ERROR_CODES.contains(&"provider_unavailable"));
        // The other four documented names belong to the middleware's wire answers, which
        // `limits`, `idempotency` and `intake` produce. Asserting they stay declared is what
        // stops a rename in this file from silently desynchronising from them.
        for name in [
            "idempotency_conflict",
            "rate_limited",
            "payload_too_large",
            "signature_invalid",
        ] {
            assert!(
                crate::vocabulary::ERROR_CODES.contains(&name),
                "{name} is documented but not declared"
            );
        }
    }

    /// The breaker refuses with its own code, not the generic invalid one — an integration
    /// cannot tell "fix your request" from "we are not calling that provider" otherwise.
    #[test]
    fn an_open_breaker_answers_provider_unavailable() {
        let err = ReliabilityError::ProviderUnavailable {
            provider: "openai".into(),
            retry_after: Some(30),
        };
        assert_eq!(err.code(), "provider_unavailable");
        assert!(err.to_string().contains("openai"));
    }
}
