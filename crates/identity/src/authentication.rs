//! Password-based authentication on top of the identity store.
//!
//! The flow is deliberately shaped so a failed attempt reveals nothing about the account:
//! unknown addresses burn the same Argon2 work as known ones, wrong passwords and unknown
//! accounts return one outcome, and account status is only disclosed after the password was
//! verified. Provider-based sign-in (LDAP/AD/SAML/OAuth2/OIDC, MFA — docs/01-VISION.md)
//! extends this module in later phases.

use sqlx::PgPool;

use crate::error::Result;
use crate::password::{dummy_verify, verify_password};
use crate::users::{User, find_credentials, normalize_email};

/// Outcome of a sign-in attempt.
#[derive(Debug, Clone)]
pub enum AuthOutcome {
    /// Email and password matched an active account.
    Authenticated(User),
    /// Unknown address or wrong password — one outcome for both, on purpose.
    InvalidCredentials,
    /// Password was correct but the account is not active (`invited`/`disabled`).
    AccountDisabled {
        /// Stored status of the account.
        status: String,
    },
}

/// Verify an email/password pair.
pub async fn authenticate(pool: &PgPool, email: &str, password: &str) -> Result<AuthOutcome> {
    let Ok(email) = normalize_email(email) else {
        // A malformed address cannot match an account; burn the same work and report the
        // same outcome as a wrong password.
        dummy_verify(password.to_owned()).await?;
        return Ok(AuthOutcome::InvalidCredentials);
    };

    let Some(credentials) = find_credentials(pool, &email).await? else {
        dummy_verify(password.to_owned()).await?;
        return Ok(AuthOutcome::InvalidCredentials);
    };

    if !verify_password(password.to_owned(), credentials.password_hash).await? {
        return Ok(AuthOutcome::InvalidCredentials);
    }

    if !credentials.user.is_active() {
        return Ok(AuthOutcome::AccountDisabled {
            status: credentials.user.status,
        });
    }

    Ok(AuthOutcome::Authenticated(credentials.user))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_are_distinguishable_for_callers() {
        let disabled = AuthOutcome::AccountDisabled {
            status: "disabled".to_owned(),
        };
        assert!(
            matches!(disabled, AuthOutcome::AccountDisabled { status } if status == "disabled")
        );
        assert!(matches!(
            AuthOutcome::InvalidCredentials,
            AuthOutcome::InvalidCredentials
        ));
    }
}
