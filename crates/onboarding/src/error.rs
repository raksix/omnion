//! Errors of the first-run flow.
//!
//! The variants answer the questions a caller (the admin wizard, the `omnion` CLI) has to put
//! on screen: is this installation already set up, may *this* account finish the first run,
//! which step is missing, and what did the store refuse. Anything that is not a flow question
//! is transparent: the underlying `sqlx`, identity, permission, content or audit error keeps
//! its own message and source.

use omnion_audit::AuditError;
use omnion_content::ContentError;
use omnion_identity::IdentityError;
use omnion_permissions::PermissionsError;
use thiserror::Error;

/// Result alias of the onboarding crate.
pub type Result<T> = std::result::Result<T, OnboardingError>;

/// Why a first-run action was refused.
#[derive(Debug, Error)]
pub enum OnboardingError {
    /// The installation already has accounts; the first run happened (or an environment
    /// bootstrap created the first administrator).
    #[error("this installation already has accounts — sign in to the panel instead")]
    AlreadyInstalled,

    /// The caller is not the account that owns the first run.
    #[error("only the account that owns the first run may finish it")]
    NotOnboardingOwner,

    /// The first run was already closed.
    #[error("the first-run setup is already complete")]
    AlreadyComplete,

    /// A step that only runs once was asked for a second time.
    #[error("the {0} step is already done")]
    StepAlreadyDone(&'static str),

    /// The flow was asked to finish while a step it needs is still open.
    #[error("the setup still needs: {missing}")]
    Incomplete {
        /// Human-readable list of the missing steps.
        missing: String,
    },

    /// A theme was chosen before the site it belongs to existed.
    #[error("no site exists yet — create the first site before choosing a theme")]
    SiteMissing,

    /// The theme key is not one of the themes this installation bundles.
    #[error("unknown theme {0:?}")]
    UnknownTheme(String),

    /// A provider connection was requested; the AI Hub is a later phase.
    #[error("AI provider connections arrive with the AI Hub — skip this step for now")]
    AiHubPending,

    /// The singleton row could not be read back after it was written.
    #[error("the first-run record could not be read back")]
    StateMissing,

    /// The caller sent something the flow cannot use.
    #[error("invalid input: {0}")]
    Invalid(String),

    /// The database refused the query.
    #[error(transparent)]
    Database(#[from] sqlx::Error),

    /// The identity store refused the account.
    #[error(transparent)]
    Identity(#[from] IdentityError),

    /// The permission store refused the role or catalogue work.
    #[error(transparent)]
    Permissions(#[from] PermissionsError),

    /// The content store refused the query.
    #[error(transparent)]
    Content(#[from] ContentError),

    /// The audit trail refused the entry.
    #[error(transparent)]
    Audit(#[from] AuditError),
}
