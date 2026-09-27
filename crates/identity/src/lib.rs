//! Omnion identity.
//!
//! The identity store of the platform (docs/07-IAM.md): user accounts, Argon2 password
//! hashing, server-side sessions, the security policy that governs sign-in, known devices, and
//! the second factors (TOTP and recovery codes) an account can hold.
//!
//! The modules compose rather than duplicate: [`signin`] runs the checks the policy describes
//! (address lists, lockout, password, factor) and never re-implements a hash; [`sessions`]
//! reads its lifetimes from [`security`]; [`mfa`] keeps secrets behind [`secrets`] envelopes.
//! Provider-based sign-in (OIDC/SAML) and SCIM provisioning arrive in a later slice.

#![forbid(unsafe_code)]

pub mod authentication;
pub mod devices;
pub mod error;
pub mod mfa;
pub mod organizations;
pub mod password;
pub mod provisioning;
pub mod secrets;
pub mod security;
pub mod sessions;
pub mod signin;
pub mod sites;
pub mod totp;
pub mod users;
pub mod webauthn;

pub use authentication::{AuthOutcome, authenticate};
pub use error::{IdentityError, Result};
pub use organizations::{
    NewOrganization, Organization, OrganizationChanges, create_organization, delete_organization,
    find_organization, find_organization_by_slug, list_organizations, update_organization,
};
pub use password::{MIN_PASSWORD_LENGTH, hash_password, verify_password};
pub use secrets::SecretBox;
pub use security::SessionPolicy;
pub use sessions::{
    AuthenticatedSession, NewSession, SESSION_TTL_DAYS, SESSION_TTL_SECONDS, Session,
    SessionFilter, SessionView, create_session, create_session_with_policy, hash_token,
    resolve_session, revoke_session, touch_session,
};
pub use signin::{SignInOutcome, sign_in};
pub use sites::{
    DEFAULT_THEME, NewSite, Site, SiteChanges, SiteDomain, add_domain, count_for_organization,
    create_site, delete_site, find_site, find_site_by_global_key, find_site_by_host,
    find_site_by_key, list_domains, list_sites, list_sites_for_organization, remove_domain,
    set_primary_domain, update_site, validate_theme,
};
pub use users::{
    BootstrapOutcome, NewUser, User, bootstrap_first_admin, count_users, create_user,
    earliest_active, find_by_id, find_credentials, has_any, normalize_email, set_status,
};
