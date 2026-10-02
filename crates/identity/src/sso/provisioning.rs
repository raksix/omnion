//! Just-in-time provisioning: turning a verified provider identity into an account (REQ-006,
//! slice 4b-2; docs/07-IAM.md §11).
//!
//! SCIM provisions an account from the *outside* — a directory pushes one at us. SSO provisions
//! it from the *inside*: the first time somebody proves who they are to a provider the platform
//! trusts, an account has to exist before the session can start, and creating it in the middle of
//! the callback is the only moment that is safe (the assertion is already verified, the challenge
//! is already consumed, and nothing else can reach this function).
//!
//! Three rules keep that moment honest:
//!
//! * **The account is matched on the email, not the subject id.** A person who signs in through
//!   two providers must end up as one account, and the only value both providers agree on is the
//!   address. The subject id is stored on the account as an attribute, so support can still see
//!   which provider id produced the account.
//! * **A JIT account has no local password.** [`JIT_PASSWORD_MARKER`] is written in place of a
//!   hash, so `POST /auth/login` can refuse it in constant time and a stolen database row cannot be
//!   turned into a password. (SCIM cannot do this: it still mints a random secret, because a
//!   provisioned account may be expected to use the reset flow.)
//! * **Provisioning is refused, not silent, when it is off.** A provider without JIT answers
//!   `provisioning_disabled` for an unknown subject rather than creating a row behind the
//!   operator's back — an account nobody approved is worse than a failed sign-in.
//!
//! The claim → role binding ([`super::claims::resolve_roles`]) runs *after* the account exists and
//! is applied through the same `role_bindings` rows the panel writes, so a mapped role is an
//! ordinary binding an administrator can see, revoke and audit.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{IdentityError, Result};
use crate::sso::claims::Identity;
use crate::sso::providers::{AuthProvider, JIT_PASSWORD_MARKER};
use crate::users::{self, NewUser, User};

/// Attribute the provider's own subject id is stored under.
pub const SUBJECT_ATTRIBUTE: &str = "sso_subject";

/// Attribute holding `provider_slug:subject` for an account — the key that finds a person again
/// when a provider changes the e-mail address they send.
pub const SUBJECT_INDEX_ATTRIBUTE: &str = "sso_subjects";

/// What a sign-in did to the account store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionOutcome {
    /// The address already belonged to an account; it was signed into.
    Existing,
    /// A new account was created and signed into.
    Created,
    /// The provider has JIT switched off, or the account is not in its organization.
    Refused,
}

impl ProvisionOutcome {
    /// The `auth_provider_events.outcome` value this result is recorded as.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Existing => "success",
            Self::Created => "provisioned",
            Self::Refused => "refused",
        }
    }
}

/// The account a provider sign-in resolved to, and what it took to get there.
#[derive(Debug, Clone)]
pub struct Provisioned {
    /// The account the session is opened for.
    pub user: User,
    /// What happened.
    pub outcome: ProvisionOutcome,
}

/// Read a provider's secret out of the environment.
///
/// The provider row never holds a credential (docs/07-IAM.md §11), so this is the only place a
/// client secret enters the process. A provider that needs a secret and has no `secret_ref`, or
/// names one the environment does not define, is a **configuration** error — never a silent
/// "no secret", which would look like a signing failure at the provider.
pub fn resolve_client_secret(provider: &AuthProvider) -> Result<Option<String>> {
    let Some(reference) = provider
        .secret_ref
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    std::env::var(reference).map(Some).map_err(|_| {
        IdentityError::InvalidProvider(format!(
            "the provider names the secret `{reference}` but this installation does not define it"
        ))
    })
}

/// Find the account a provider identity belongs to.
///
/// Three lookups, in the order that is both cheapest and most correct: the stored subject index
/// (which survives an e-mail change at the provider), then the address itself. A JIT account from
/// a *different* organization is deliberately not returned — the same person signing in against a
/// second tenant's provider creates an account there rather than joining the other organization.
pub async fn find_account(
    pool: &PgPool,
    organization_id: Uuid,
    provider: &AuthProvider,
    identity: &Identity,
) -> Result<Option<User>> {
    let indexed = sqlx::query_scalar::<_, Uuid>(
        "select id from users \
         where organization_id = $1 \
           and attributes -> 'sso_subjects' ->> $2 = $3",
    )
    .bind(organization_id)
    .bind(&provider.slug)
    .bind(&identity.subject)
    .fetch_optional(pool)
    .await?;

    if let Some(id) = indexed
        && let Some(user) = users::find_by_id(pool, id).await?
    {
        return Ok(Some(user));
    }

    let by_email = users::find_by_email(pool, &identity.email).await?;
    Ok(by_email.filter(|user| user.organization_id == Some(organization_id)))
}

/// Resolve — and if allowed, create — the account an identity signs into.
///
/// This is the single decision point of a provider sign-in, and the order matters: an existing
/// account is always preferred, so JIT is a *fallback* rather than a shortcut, and disabling it
/// never locks an already-provisioned person out of their account.
pub async fn provision(
    pool: &PgPool,
    provider: &AuthProvider,
    identity: &Identity,
) -> Result<Provisioned> {
    if let Some(user) = find_account(pool, provider.organization_id, provider, identity).await? {
        return Ok(Provisioned {
            user,
            outcome: ProvisionOutcome::Existing,
        });
    }

    if !provider.jit_enabled {
        return Err(IdentityError::InvalidProvider(format!(
            "{} has no account yet and this provider does not provision new ones",
            identity.email
        )));
    }

    let user = create_account(pool, provider, identity).await?;
    Ok(Provisioned {
        user,
        outcome: ProvisionOutcome::Created,
    })
}

/// Create the account of a verified identity.
async fn create_account(
    pool: &PgPool,
    provider: &AuthProvider,
    identity: &Identity,
) -> Result<User> {
    // `create_user` hashes its password, so a JIT account cannot be written through it: the
    // marker is written directly instead, and the row is then read back through the normal
    // reader so the caller sees the same shape as every other account.
    let password = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let created = users::create_user(
        pool,
        NewUser {
            email: identity.email.clone(),
            password,
            display_name: identity.display_name_or_email().to_owned(),
            organization_id: Some(provider.organization_id),
        },
    )
    .await?;

    sqlx::query(
        "update users set \
             password_hash = $2, \
             attributes = attributes || jsonb_build_object( \
                 $3, $4, \
                 $5, jsonb_build_object($6, $4), \
                 'sso_last_provider', $7, \
                 'sso_last_seen_at', to_char(now() at time zone 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
             ) \
         where id = $1",
    )
    .bind(created.id)
    .bind(JIT_PASSWORD_MARKER)
    .bind(SUBJECT_ATTRIBUTE)
    .bind(&identity.subject)
    .bind(SUBJECT_INDEX_ATTRIBUTE)
    .bind(&provider.slug)
    .bind(&provider.slug)
    .execute(pool)
    .await?;

    users::find_by_id(pool, created.id)
        .await?
        .ok_or_else(|| IdentityError::InvalidProvider("the account could not be read back".into()))
}

/// Remember a sign-in on the account: the provider's subject id is re-indexed, the display name
/// follows the provider when it starts sending one, and the last-seen stamp moves.
///
/// A disabled account is **not** re-enabled here. Deactivation is a deliberate administrative act
/// and a provider sign-in must never undo it — the refusal happens in the callback instead.
pub async fn touch_account(
    pool: &PgPool,
    user_id: Uuid,
    provider: &AuthProvider,
    identity: &Identity,
) -> Result<()> {
    sqlx::query(
        "update users set \
             display_name = coalesce(nullif($2, ''), display_name), \
             updated_at = now(), \
             attributes = attributes || jsonb_build_object( \
                 $3, $4, \
                 'sso_subjects', coalesce(attributes -> 'sso_subjects', '{}'::jsonb) \
                     || jsonb_build_object($5, $4), \
                 'sso_last_provider', $6, \
                 'sso_last_seen_at', to_char(now() at time zone 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
             ) \
         where id = $1",
    )
    .bind(user_id)
    .bind(identity.display_name.as_deref().unwrap_or_default())
    .bind(SUBJECT_ATTRIBUTE)
    .bind(&identity.subject)
    .bind(&provider.slug)
    .bind(&provider.slug)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a JIT account so the local sign-in path refuses it in constant time.
///
/// The password path already calls [`crate::sso::providers::is_jit_account`]; this is the write
/// side, exposed so the migration and the tests can assert the same rule from both ends.
pub async fn mark_as_jit(pool: &PgPool, user_id: Uuid) -> Result<bool> {
    let updated = sqlx::query("update users set password_hash = $2 where id = $1")
        .bind(user_id)
        .bind(JIT_PASSWORD_MARKER)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(updated > 0)
}

/// When a JIT account was last seen through a provider, for the user detail screen.
pub async fn last_provider_seen(pool: &PgPool, user_id: Uuid) -> Result<Option<OffsetDateTime>> {
    Ok(sqlx::query_scalar::<_, OffsetDateTime>(
        "select (attributes ->> 'sso_last_seen_at')::timestamptz from users where id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_outcome_names_its_own_event_value() {
        assert_eq!(ProvisionOutcome::Existing.as_str(), "success");
        assert_eq!(ProvisionOutcome::Created.as_str(), "provisioned");
        assert_eq!(ProvisionOutcome::Refused.as_str(), "refused");
    }

    #[test]
    fn a_provider_without_a_secret_reference_resolves_to_none() {
        // The unit half that does not need a database: a provider wired for a public client
        // (PKCE, no client secret) resolves to "no secret" rather than failing.
        let provider = AuthProvider {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            slug: "public".into(),
            kind: crate::sso::providers::ProviderKind::Oidc,
            name: "Public".into(),
            config: serde_json::json!({}),
            secret_ref: None,
            scopes: vec![],
            group_claim: None,
            default_role_id: None,
            jit_enabled: true,
            enabled: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        assert!(
            resolve_client_secret(&provider)
                .expect("no reference is not an error")
                .is_none()
        );
    }

    #[test]
    fn a_secret_reference_this_installation_does_not_define_is_named_in_the_error() {
        // The name has to reach the operator: "the provider did not answer" is useless when the
        // real cause is a variable the operator forgot to set in the environment.
        let reference = "OMNION_TEST_SECRET_THAT_DOES_NOT_EXIST";
        let provider = AuthProvider {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            slug: "broken".into(),
            kind: crate::sso::providers::ProviderKind::Oidc,
            name: "Broken".into(),
            config: serde_json::json!({}),
            secret_ref: Some(reference.into()),
            scopes: vec![],
            group_claim: None,
            default_role_id: None,
            jit_enabled: true,
            enabled: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let error = resolve_client_secret(&provider).expect_err("the reference is not defined");
        assert!(error.to_string().contains(reference));
    }

    #[test]
    fn a_blank_secret_reference_is_treated_as_no_reference() {
        let provider = AuthProvider {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            slug: "blank".into(),
            kind: crate::sso::providers::ProviderKind::Oidc,
            name: "Blank".into(),
            config: serde_json::json!({}),
            secret_ref: Some("   ".into()),
            scopes: vec![],
            group_claim: None,
            default_role_id: None,
            jit_enabled: true,
            enabled: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        assert!(
            resolve_client_secret(&provider)
                .expect("blank is not a reference")
                .is_none()
        );
    }
}
