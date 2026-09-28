//! Errors of the CRM intake module.
//!
//! Like every other crate in the workspace this one never decides an HTTP status code — the
//! API layer maps it. The taxonomy is deliberately small: a submission is refused because of
//! what it *is* (invalid, spam, rate-limited, over-sized), or the database said no.
//!
//! The interesting member is [`CrmIntakeError::Spam`]. A spam submission is not an error in
//! the sense of "something broke" — it is a *recorded outcome*. The API answers `202` to the
//! submitter (telling a spammer their submission failed only teaches them to try again) and
//! the store keeps the row with its score, so an operator can see what was discarded instead
//! of a form that silently stopped receiving anything.

/// Result alias used across the CRM intake module.
pub type Result<T, E = CrmIntakeError> = std::result::Result<T, E>;

/// What can go wrong while a submission is captured, mapped or filed.
#[derive(Debug, thiserror::Error)]
pub enum CrmIntakeError {
    /// A database operation failed.
    #[error("crm intake: {0}")]
    Database(#[from] sqlx::Error),
    /// The mapping, the source or the submission is one the platform will not store.
    #[error("invalid intake: {0}")]
    Invalid(String),
    /// The submission tripped a spam heuristic. Carries the score so the row records *why*.
    #[error("submission flagged as spam (score {0})")]
    Spam(i32),
    /// The source's per-hour ceiling is reached.
    #[error("this intake source has reached its hourly limit")]
    RateLimited,
    /// The submission is larger than the platform accepts.
    #[error("submission is too large (max {max} bytes, got {actual})")]
    PayloadTooLarge {
        /// The ceiling.
        max: usize,
        /// What the submission actually weighed.
        actual: usize,
    },
    /// The endpoint key is unknown, wrong or has been rotated away.
    #[error("unknown intake key")]
    UnknownKey,
}

impl CrmIntakeError {
    /// Stable, machine-readable code for this failure.
    ///
    /// The API puts it in its error bodies so a client can branch on the reason without
    /// reading prose. Note that [`Self::Spam`] and [`Self::RateLimited`] are codes a *public
    /// intake caller* never sees — the public endpoint answers `202` for both, because a
    /// submission that discloses why it was dropped is a submission an attacker can tune.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Database(_) => "internal_error",
            Self::Invalid(_) => "invalid_intake",
            Self::Spam(_) => "spam",
            Self::RateLimited => "rate_limited",
            Self::PayloadTooLarge { .. } => "payload_too_large",
            Self::UnknownKey => "unknown_key",
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
        assert_eq!(CrmIntakeError::invalid("x").code(), "invalid_intake");
        assert_eq!(CrmIntakeError::Spam(80).code(), "spam");
        assert_eq!(CrmIntakeError::RateLimited.code(), "rate_limited");
        assert_eq!(CrmIntakeError::UnknownKey.code(), "unknown_key");
    }

    #[test]
    fn a_refusal_names_the_field() {
        assert_eq!(
            CrmIntakeError::invalid("target \"salary\"").to_string(),
            "invalid intake: target \"salary\""
        );
    }

    #[test]
    fn the_spam_error_carries_the_score_that_judged_it() {
        // The row records the score, so the reason survives the HTTP hop; a spam verdict
        // without its score cannot be explained to the operator who asks why a lead vanished.
        assert_eq!(
            CrmIntakeError::Spam(85).to_string(),
            "submission flagged as spam (score 85)"
        );
    }

    #[test]
    fn an_oversized_submission_reports_both_numbers() {
        let error = CrmIntakeError::PayloadTooLarge {
            max: 262_144,
            actual: 300_000,
        };
        let text = error.to_string();
        assert!(text.contains("262144"), "{text}");
        assert!(text.contains("300000"), "{text}");
    }
}
