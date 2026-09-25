//! Omnion identity.
//!
//! The identity store of the platform (docs/07-IAM.md): user accounts, Argon2 password
//! hashing and server-side sessions. Business modules and the API layer build on these
//! primitives instead of talking to the tables directly.
//!
//! Later phases extend this crate with the rest of the IAM model (roles, permissions,
//! scopes); provider-based sign-in (LDAP/AD/SAML/OAuth2/OIDC, MFA — docs/01-VISION.md)
//! arrives as its own crate that composes the identity store.

#![forbid(unsafe_code)]

pub mod authentication;
pub mod error;
pub mod organizations;
pub mod password;
pub mod sessions;
pub mod sites;
pub mod users;

pub use authentication::{AuthOutcome, authenticate};
pub use error::{IdentityError, Result};
pub use organizations::{
    NewOrganization, Organization, OrganizationChanges, create_organization, delete_organization,
    find_organization, find_organization_by_slug, list_organizations, update_organization,
};
pub use password::{MIN_PASSWORD_LENGTH, hash_password, verify_password};
pub use sessions::{
    AuthenticatedSession, SESSION_TTL_DAYS, SESSION_TTL_SECONDS, Session, create_session,
    hash_token, resolve_session, revoke_session, touch_session,
};
pub use sites::{
    NewSite, Site, SiteChanges, SiteDomain, add_domain, count_for_organization, create_site,
    delete_site, find_site, find_site_by_host, find_site_by_key, list_domains, list_sites,
    list_sites_for_organization, remove_domain, set_primary_domain, update_site,
};
pub use users::{
    BootstrapOutcome, NewUser, User, bootstrap_first_admin, count_users, create_user, find_by_id,
    find_credentials, has_any, normalize_email,
};
