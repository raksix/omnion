//! Errors of the deployment tooling.

use uuid::Uuid;

/// Anything that can go wrong while caching a release or building an upgrade plan.
#[derive(Debug, thiserror::Error)]
pub enum DeploymentError {
    /// The database refused or could not run a query.
    #[error("deployment store: {0}")]
    Store(#[from] sqlx::Error),
    /// A version string is not a SemVer, so no plan can be built from it.
    #[error("{field} is not a version: {value:?}")]
    InvalidVersion {
        /// The field that was not a version.
        field: &'static str,
        /// What was supplied.
        value: String,
    },
    /// A range that is not an upgrade: a downgrade, or the same version twice.
    ///
    /// A downgrade runs the same migrations in reverse with a different step order, so it is
    /// refused rather than planned. The message says what to do instead.
    #[error("{0}")]
    NotAnUpgrade(String),
    /// A topology, bundle kind, channel or step kind outside the closed vocabulary.
    #[error("{0}")]
    UnknownVocabulary(String),
    /// A row the caller addressed is not there.
    #[error("no {what} {id}")]
    NotFound {
        /// What was addressed (`upgrade plan`, `release manifest`, …).
        what: &'static str,
        /// Its identifier.
        id: String,
    },
    /// A conflict the caller has to resolve rather than retry.
    #[error("{0}")]
    Conflict(String),
}

impl DeploymentError {
    /// A missing row, built from the two things a reader needs.
    pub fn not_found(what: &'static str, id: impl std::fmt::Display) -> Self {
        Self::NotFound {
            what,
            id: id.to_string(),
        }
    }
}

/// Result alias of the deployment crate.
pub type Result<T> = std::result::Result<T, DeploymentError>;

/// Kept so the id type appears in this module's surface: every `uuid` in a store signature is
/// an account, an organization or a plan, and a reader of the error enum should not have to
/// guess which.
const _: fn(Uuid) -> Uuid = |id| id;
