//! The errors this layer names, and why each one is its own variant.
//!
//! Every error here corresponds to an `error_code` the API returns verbatim, because the panel
//! switches on the name: `environment_not_found` and `environment_key_taken` send the operator to
//! two different places, and a single "invalid environment" string would send both to the same
//! one.

use std::fmt;

/// A refusal by the environment layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentError {
    /// No environment with that id in this organization.
    NotFound,
    /// The key is already taken by another environment of the same organization.
    KeyTaken {
        /// The key that was refused.
        key: String,
    },
    /// The key does not match the environment key format.
    InvalidKey {
        /// The key that was refused.
        key: String,
        /// Why it was refused, in the operator's language.
        reason: String,
    },
    /// The staging host is already used by another environment.
    HostTaken {
        /// The host that was refused.
        host: String,
    },
    /// A staging environment cannot be the source of another clone.
    StagingNestingRefused {
        /// The key of the environment that was asked to clone.
        source_key: String,
    },
    /// A clone is already running for this environment.
    CloneAlreadyRunning {
        /// The id of the job that is already running.
        job_id: String,
    },
    /// The environment is not a staging environment.
    NotStaging {
        /// The key of the environment that was addressed.
        key: String,
    },
    /// The environment is archived and takes no writes.
    Archived {
        /// The key of the environment that was addressed.
        key: String,
    },
    /// A promotion would overwrite production rows that moved on since the clone.
    PromotionConflict {
        /// The item ids that conflict.
        items: Vec<String>,
    },
    /// The requester cannot approve their own promotion.
    SelfApprovalRefused,
    /// The promotion is not in a state that accepts this operation.
    PromotionNotPending {
        /// The status the promotion is actually in.
        status: String,
    },
    /// The store itself failed.
    ///
    /// Its own variant rather than a `Box<dyn Error>` because a store failure and a refusal
    /// have to be told apart at the HTTP boundary: a refusal is a `4xx` naming what the caller
    /// should change, and a store failure is a `5xx` naming what the operator should look at.
    /// A single "any error" variant makes the API answer `500` to a duplicate key, which is a
    /// refusal the caller *can* act on.
    Store {
        /// The driver's message, verbatim.
        message: String,
    },
}

impl fmt::Display for EnvironmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "That environment does not exist."),
            Self::KeyTaken { key } => {
                write!(f, "The key “{key}” is already used by another environment.")
            }
            Self::InvalidKey { key, reason } => write!(f, "“{key}” is not a valid key: {reason}"),
            Self::HostTaken { host } => {
                write!(
                    f,
                    "The host “{host}” is already serving another environment."
                )
            }
            Self::StagingNestingRefused { source_key } => write!(
                f,
                "“{source_key}” is a staging environment. A staging environment cannot be cloned \
                 into another one — clone from production instead."
            ),
            Self::CloneAlreadyRunning { job_id } => {
                write!(
                    f,
                    "A clone is already running for this environment (job {job_id})."
                )
            }
            Self::NotStaging { key } => {
                write!(f, "“{key}” is a production environment, not a staging one.")
            }
            Self::Archived { key } => {
                write!(f, "“{key}” is archived and takes no further changes.")
            }
            Self::PromotionConflict { items } => write!(
                f,
                "{} production item(s) changed since the clone and would be overwritten.",
                items.len()
            ),
            Self::SelfApprovalRefused => {
                write!(
                    f,
                    "A promotion cannot be approved by the person who requested it."
                )
            }
            Self::PromotionNotPending { status } => write!(
                f,
                "This promotion is “{status}” and no longer waits for approval."
            ),
            Self::Store { message } => write!(f, "The environment store failed: {message}"),
        }
    }
}

impl std::error::Error for EnvironmentError {}

impl EnvironmentError {
    /// The stable machine-readable name the API returns and the panel switches on.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound => "environment_not_found",
            Self::KeyTaken { .. } => "environment_key_taken",
            Self::InvalidKey { .. } => "environment_key_invalid",
            Self::HostTaken { .. } => "environment_host_taken",
            Self::StagingNestingRefused { .. } => "staging_nesting_refused",
            Self::CloneAlreadyRunning { .. } => "clone_already_running",
            Self::NotStaging { .. } => "environment_not_staging",
            Self::Archived { .. } => "environment_archived",
            Self::PromotionConflict { .. } => "promotion_conflict",
            Self::SelfApprovalRefused => "self_approval_refused",
            Self::PromotionNotPending { .. } => "promotion_not_pending",
            Self::Store { .. } => "environment_store_failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_carries_its_own_code() {
        // A shared code would collapse two different panel routes into one message.
        let codes = [
            EnvironmentError::NotFound.code(),
            EnvironmentError::KeyTaken { key: "a".into() }.code(),
            EnvironmentError::StagingNestingRefused {
                source_key: "a".into(),
            }
            .code(),
            EnvironmentError::SelfApprovalRefused.code(),
        ];
        let unique: std::collections::HashSet<_> = codes.iter().collect();
        assert_eq!(
            unique.len(),
            codes.len(),
            "codes must be distinct: {codes:?}"
        );
    }

    #[test]
    fn a_conflict_names_how_many_items_are_involved() {
        let err = EnvironmentError::PromotionConflict {
            items: vec!["a".into(), "b".into()],
        };
        assert!(err.to_string().contains("2 production item"));
    }
}
