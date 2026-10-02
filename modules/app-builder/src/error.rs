//! Errors of the AI app builder (docs/requests/REQ-045).
//!
//! Three callers act on three different failures, and collapsing them is how a plan gets
//! regenerated because the database was briefly busy:
//!
//! * the **store** failing is the platform's problem ([`AppBuilderError::Database`]);
//! * the **answer** breaking a rule is the plan's ([`AppBuilderError::Invalid`], carrying the
//!   stable `code` the HTTP layer answers with);
//! * the **provider** failing is [`AppBuilderError::Ai`] — the only one worth spending a
//!   second attempt on, and the only one the single repair round-trip may answer.
//!
//! [`AppBuilderError::Blocked`] is the fourth and is not a failure of any of them: the plan is
//! well-formed and the answer is fine, and apply is refused because a human has not resolved
//! an artifact. It is a different variant rather than an `Invalid` because the review screen
//! answers it with a list of what is missing rather than with an error banner.

use thiserror::Error;

/// Everything that can go wrong storing, generating or validating a plan.
#[derive(Debug, Error)]
pub enum AppBuilderError {
    /// The database refused the read or the write.
    #[error(transparent)]
    Database(#[from] sqlx::Error),

    /// The audit trail refused a row.
    #[error(transparent)]
    Audit(#[from] omnion_audit::AuditError),

    /// The AI Hub could not produce an answer, or refused to route the request.
    #[error("{0}")]
    Ai(String),

    /// The plan could not be rendered as the document it is exported as.
    ///
    /// Its own variant rather than folded into [`Self::Ai`] or [`Self::Database`] because
    /// neither is the blame: the rows were read fine and no provider was called, so an
    /// export that reported a store fault or an AI fault would send an operator to look in
    /// the two places that cannot be at fault. `serde_json` cannot actually fail on these
    /// types today — every field is a plain scalar, a `String`, an `Option`, a `Vec` or a
    /// `Value` — and the variant is here so that a future field which *can* fail is
    /// reported honestly instead of being unwrapped.
    #[error("the plan could not be written as an export document: {0}")]
    Render(String),

    /// The plan, the prompt or an artifact breaks a rule.
    #[error("{message}")]
    Invalid {
        /// Stable machine-readable code, e.g. `invalid_plan_prompt`.
        code: &'static str,
        /// Human-readable explanation.
        message: String,
    },

    /// The operation is refused because artifacts are unresolved.
    ///
    /// Carries the artifacts by name rather than only a count: "3 artifacts are unresolved" is
    /// a number the operator has to go and look up, and "the report artifact is unresolved" is
    /// the sentence that lets them press the right row.
    #[error("apply is blocked: {}", describe_blockers(.0))]
    Blocked(Vec<BlockedArtifact>),
}

/// An artifact standing between a plan and its apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedArtifact {
    /// Which kind it is.
    pub kind: String,
    /// Its key within the plan.
    pub key: String,
    /// Why it blocks: `rejected`, `invalid` or `pending`.
    pub status: String,
    /// The first validation finding, when the status is `invalid`.
    pub reason: Option<String>,
}

/// One blocking artifact, named.
#[must_use]
pub fn describe_blocker(artifact: &BlockedArtifact) -> String {
    match (&artifact.reason, artifact.status.as_str()) {
        (Some(reason), "invalid") => {
            format!(
                "the {} artifact `{}` is invalid — {reason}",
                artifact.kind, artifact.key
            )
        }
        (_, "rejected") => format!(
            "the {} artifact `{}` was rejected",
            artifact.kind, artifact.key
        ),
        (Some(reason), _) => format!(
            "the {} artifact `{}` — {reason}",
            artifact.kind, artifact.key
        ),
        (_, status) => format!(
            "the {} artifact `{}` is {status}",
            artifact.kind, artifact.key
        ),
    }
}

/// One blocker named; several joined.
///
/// The plural exists because "apply is blocked: the report artifact is pending" is a sentence
/// about one thing, and a plan with three unresolved artifacts needs all three named — the
/// first one alone would send the reviewer round the tree again after fixing it.
fn describe_blockers(blocked: &[BlockedArtifact]) -> String {
    blocked
        .iter()
        .map(describe_blocker)
        .collect::<Vec<String>>()
        .join("; ")
}

impl AppBuilderError {
    /// Build an [`AppBuilderError::Invalid`].
    #[must_use]
    pub fn invalid(code: &'static str, message: impl Into<String>) -> Self {
        Self::Invalid {
            code,
            message: message.into(),
        }
    }

    /// Build an [`AppBuilderError::Blocked`].
    #[must_use]
    pub fn blocked(artifacts: Vec<BlockedArtifact>) -> Self {
        Self::Blocked(artifacts)
    }

    /// Stable code of the failure, for the HTTP layer.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Database(_) => "app_builder_store_error",
            Self::Audit(_) => "app_builder_audit_error",
            Self::Ai(_) => "ai_provider_error",
            Self::Render(_) => "app_builder_export_failed",
            Self::Blocked(_) => "app_builder_blocked",
            Self::Invalid { code, .. } => code,
        }
    }

    /// `true` when the failure is the provider's rather than the plan's.
    ///
    /// This is the question the repair round-trip asks, and it is the same question
    /// REQ-046's generator asks with the same consequence: a *validation* failure is answered
    /// by telling the model which rule it broke, while a *transport* failure is answered by
    /// trying again identically — spending a repair on it hands a second identical answer to
    /// a validator that will refuse it the same way.
    #[must_use]
    pub fn is_provider_failure(&self) -> bool {
        matches!(self, Self::Ai(_))
    }

    /// The artifacts that blocked an apply, or `None` when the failure was something else.
    ///
    /// The review screen needs the list to render "what is missing"; a `match` on the variant
    /// at the call site would work too, but the pattern the sibling module uses is a method,
    /// and a method keeps the `Blocked` payload out of every caller that does not care.
    #[must_use]
    pub fn blocked_by(&self) -> Option<&[BlockedArtifact]> {
        match self {
            Self::Blocked(artifacts) => Some(artifacts),
            _ => None,
        }
    }
}

/// Result alias of the module.
pub type Result<T> = std::result::Result<T, AppBuilderError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blocker_names_the_artifact_rather_than_counting_it() {
        let error = AppBuilderError::blocked(vec![
            BlockedArtifact {
                kind: "entity".into(),
                key: "leave_request".into(),
                status: "accepted".into(),
                reason: None,
            },
            BlockedArtifact {
                kind: "report".into(),
                key: "leave_summary".into(),
                status: "pending".into(),
                reason: None,
            },
        ]);
        // Every blocker is named, not just the first: a reviewer who fixes one blocker and
        // is sent back to the screen to discover the second has been told "apply is blocked"
        // without being told what else to do.
        assert_eq!(
            error.to_string(),
            "apply is blocked: the entity artifact `leave_request` is accepted; \
             the report artifact `leave_summary` is pending"
        );
        assert_eq!(error.code(), "app_builder_blocked");
        assert_eq!(error.blocked_by().map(<[_]>::len), Some(2));
    }

    #[test]
    fn an_invalid_blocker_leads_with_the_validators_own_words() {
        let artifact = BlockedArtifact {
            kind: "field".into(),
            key: "leaver".into(),
            status: "invalid".into(),
            reason: Some("`leaver` is a reserved platform key".into()),
        };
        assert_eq!(
            describe_blocker(&artifact),
            "the field artifact `leaver` is invalid — `leaver` is a reserved platform key"
        );
    }

    #[test]
    fn a_rejected_blocker_says_rejected_rather_than_naming_a_reason_that_does_not_exist() {
        let artifact = BlockedArtifact {
            kind: "role".into(),
            key: "leave_manager".into(),
            status: "rejected".into(),
            reason: None,
        };
        assert_eq!(
            describe_blocker(&artifact),
            "the role artifact `leave_manager` was rejected"
        );
    }

    #[test]
    fn only_the_provider_failure_is_worth_a_repair_round_trip() {
        assert!(AppBuilderError::Ai("no route".into()).is_provider_failure());
        assert!(
            !AppBuilderError::invalid("invalid_plan_prompt", "too short").is_provider_failure()
        );
        assert!(!AppBuilderError::blocked(Vec::new()).is_provider_failure());
    }

    #[test]
    fn a_non_blocked_failure_offers_no_artifacts() {
        assert!(
            AppBuilderError::Ai("no route".into())
                .blocked_by()
                .is_none()
        );
    }
}
