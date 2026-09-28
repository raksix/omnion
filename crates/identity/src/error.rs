//! Error type of the identity store.
//!
//! Like the core, this crate never decides HTTP status codes: it returns [`IdentityError`]
//! and the API layer maps it onto the HTTP surface (`apps/api/src/error.rs`).

use crate::memberships::Invitation;

/// Errors returned by the identity store.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// A database operation failed.
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
    /// The supplied email address is not usable as an account address.
    #[error("invalid email address: {0}")]
    InvalidEmail(String),
    /// A session token is not in the expected shape.
    #[error("invalid session token: {0}")]
    InvalidToken(String),
    /// The password does not satisfy the minimum policy.
    #[error("password must be at least {min} characters long")]
    WeakPassword {
        /// Minimum accepted length.
        min: usize,
    },
    /// The email address is already registered (case-insensitive).
    #[error("email address is already registered")]
    EmailTaken,
    /// Password hashing or verification failed (broken hash, unsupported parameters).
    #[error("password hashing failed: {0}")]
    PasswordHash(String),
    /// A blocking hashing task could not be joined.
    #[error("hashing task failed: {0}")]
    Task(String),
    /// The organization slug is already taken.
    #[error("organization slug is already taken")]
    OrganizationSlugTaken,
    /// No organization carries this identifier.
    #[error("no such organization")]
    OrganizationNotFound,
    /// An organization field is not usable (slug shape, blank name, unknown status).
    #[error("invalid organization: {0}")]
    InvalidOrganization(String),
    /// A membership field is not usable (unknown status).
    #[error("invalid membership: {0}")]
    InvalidMembership(String),
    /// The account already belongs to this organization.
    #[error("this account is already a member of the organization")]
    MemberAlreadyPresent,
    /// No membership carries this (organization, account) pair.
    #[error("this account is not a member of the organization")]
    MemberNotFound,
    /// An invitation field is not usable (message length, shape).
    #[error("invalid invitation: {0}")]
    InvalidInvitation(String),
    /// No invitation carries this token.
    #[error("this invitation link is not valid")]
    InvitationNotFound,
    /// The token was already accepted; an invitation is single-use.
    #[error("this invitation has already been accepted")]
    InvitationAlreadyUsed,
    /// The token was revoked before it was used.
    #[error("this invitation was revoked")]
    InvitationRevoked,
    /// The token is past its expiry.
    #[error("this invitation has expired")]
    InvitationExpired,
    /// The token is real but its organization runs the `owner_approval` policy, so nobody has
    /// released it yet. This is deliberately its own answer rather than a generic "not valid":
    /// the holder is somebody a manager invited, and telling them "not valid" would send them
    /// back to the person who just invited them for no reason. It reveals nothing about any
    /// *other* organization, because a token nobody issued answers `InvitationNotFound`.
    #[error("this invitation is waiting for an owner to release it")]
    InvitationAwaitingApproval,
    /// The address already holds a live invitation in this organization; the row is carried so
    /// the API can name the existing invitation instead of mailing the address twice.
    #[error("this address already has a pending invitation")]
    InvitationAlreadyPending(Invitation),
    /// The site key is already taken inside the organization.
    #[error("site key is already taken in this organization")]
    SiteKeyTaken,
    /// No site carries this identifier.
    #[error("no such site")]
    SiteNotFound,
    /// A site field is not usable (key shape, blank name, unknown status).
    #[error("invalid site: {0}")]
    InvalidSite(String),
    /// The domain host is already bound to a site.
    #[error("this host is already bound to a site")]
    DomainTaken,
    /// The site does not carry this domain.
    #[error("no such domain on this site")]
    DomainNotFound,
    /// A domain host is not usable (shape or case).
    #[error("invalid host: {0}")]
    InvalidHost(String),
    /// A department field is not usable (key shape, blank name, unknown status).
    #[error("invalid department: {0}")]
    InvalidDepartment(String),
    /// No department carries this identifier.
    #[error("no such department")]
    DepartmentNotFound,
    /// The department key is already taken inside the organization.
    #[error("department key is already taken in this organization")]
    DepartmentKeyTaken,
    /// A move or re-parent would make a department its own ancestor.
    #[error("a department cannot be moved inside itself")]
    DepartmentCycle,
    /// A security-policy field is out of range or not usable.
    #[error("{field}: {message}")]
    InvalidPolicy {
        /// The control the reader has to fix.
        field: String,
        /// What is wrong with it.
        message: String,
    },
    /// An IP list entry is not a usable address or network.
    #[error("invalid network: {0}")]
    InvalidNetwork(String),
    /// A secret envelope could not be read (wrong key, or the value was tampered with).
    #[error("the stored secret could not be read")]
    Crypto,
    /// No such second factor for this account.
    #[error("no such second factor")]
    FactorNotFound,
    /// A factor field is not usable (kind, label, or a code that does not match).
    #[error("{0}")]
    InvalidFactor(String),
    /// A WebAuthn ceremony was refused; the message names what did not hold.
    #[error("{0}")]
    WebAuthn(String),
    /// A provisioning token or sync-log entry is not usable (shape or unknown value).
    #[error("{0}")]
    InvalidProvisioning(String),
    /// A sign-in provider, its configuration, its challenge or its assertion is not usable.
    #[error("{0}")]
    InvalidProvider(String),
    /// An account status is not one the schema allows.
    #[error("{0}")]
    InvalidUser(String),
    /// A settings field is not usable (locale, timezone, invite policy, accent, retention).
    #[error("invalid organization settings: {0}")]
    InvalidSettings(String),
    /// A limit or plan field is not usable (unknown plan, non-positive ceiling).
    #[error("invalid organization limits: {0}")]
    InvalidLimits(String),
    /// A module key is not usable as an address.
    #[error("invalid module key: {0}")]
    InvalidModule(String),
    /// The installation does not ship the module the request named.
    #[error("this installation does not ship the module {0:?}")]
    ModuleNotInstalled(String),
}

/// Result alias used across the identity crate.
pub type Result<T, E = IdentityError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weak_password_error_states_the_minimum() {
        let error = IdentityError::WeakPassword { min: 10 };
        assert_eq!(
            error.to_string(),
            "password must be at least 10 characters long"
        );
    }

    #[test]
    fn taken_email_error_is_explicit() {
        assert_eq!(
            IdentityError::EmailTaken.to_string(),
            "email address is already registered"
        );
    }
}
