//! Sign-in challenges: the `state` a provider round trip is bound to (docs/07-IAM.md §11).
//!
//! Every flow — OIDC, OAuth2, SAML — starts by writing one row here and handing the browser an
//! opaque `state`. The callback presents it back; this module decides whether the round trip may
//! continue **before** any token, assertion or signature is looked at. That order is the point: a
//! callback with a forged, replayed, expired or foreign `state` is refused with no possibility of
//! a session, so the protocol exchange itself never becomes the trust anchor.
//!
//! The value is stored as a hash for the same reason session tokens are: a database read must not
//! be enough to replay a sign-in. The PKCE verifier (a `code` flow's secret half) *is* stored in
//! clear because the token endpoint has to send it back to the provider — it never leaves this
//! server, and it is destroyed the moment the challenge is consumed.

use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::{IdentityError, Result};

/// How long a started sign-in may take to come back. Ten minutes covers a directory that asks for
/// a second factor of its own, and expires a challenge nobody finished.
pub const CHALLENGE_TTL_MINUTES: i64 = 10;

/// How many callbacks a challenge may see. The second one is refused even if the first failed —
/// a challenge that is being guessed at is burned, not retried.
pub const MAX_CHALLENGE_ATTEMPTS: i32 = 2;

/// Bytes of randomness in a `state` value.
const STATE_BYTES: usize = 32;

/// A started round trip, as the callback finds it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SsoChallenge {
    /// Primary key.
    pub id: Uuid,
    /// Provider the callback must belong to.
    pub provider_id: Uuid,
    /// Organization the session will be created in.
    pub organization_id: Uuid,
    /// `oidc`, `oauth2` or `saml`.
    pub flow: String,
    /// SHA-256 of the presented state.
    pub state_hash: String,
    /// PKCE verifier, when the flow uses one.
    pub code_verifier: Option<String>,
    /// Panel path the callback returns to.
    pub return_to: String,
    /// How many callbacks have presented it.
    pub attempts: i32,
    /// When it stops being valid.
    pub expires_at: OffsetDateTime,
    /// When a successful callback consumed it.
    pub consumed_at: Option<OffsetDateTime>,
}

/// Column list of every challenge query.
const COLUMNS: &str = "id, provider_id, organization_id, flow, state_hash, code_verifier, \
     return_to, attempts, expires_at, consumed_at";

/// A fresh challenge with the value the browser will carry.
#[derive(Debug, Clone)]
pub struct IssuedChallenge {
    /// The stored row.
    pub challenge: SsoChallenge,
    /// The opaque `state` — handed to the browser exactly once and never stored in clear.
    pub state: String,
}

/// SHA-256 of a state value, hex encoded.
#[must_use]
pub fn hash_state(state: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(state.as_bytes());
    hex::encode(hasher.finalize())
}

/// A URL-safe random token (the `state` and the PKCE verifier both use it).
fn random_token() -> String {
    let mut bytes = [0_u8; 64];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes);
    encoded[..STATE_BYTES * 2].trim_end_matches('=').to_owned()
}

/// Write a challenge for a provider and hand back its `state`.
pub async fn issue(
    pool: &PgPool,
    provider_id: Uuid,
    organization_id: Uuid,
    flow: &str,
    return_to: &str,
) -> Result<IssuedChallenge> {
    if !matches!(flow, "oidc" | "oauth2" | "saml") {
        return Err(IdentityError::InvalidProvider(format!(
            "unknown sign-in flow `{flow}`"
        )));
    }

    let state = random_token();
    let verifier = random_token();
    let expires_at = OffsetDateTime::now_utc() + Duration::minutes(CHALLENGE_TTL_MINUTES);

    let row = sqlx::query_as::<_, SsoChallenge>(&format!(
        "insert into sso_challenges \
             (provider_id, organization_id, flow, state_hash, code_verifier, return_to, expires_at) \
         values ($1, $2, $3, $4, $5, $6, $7) returning {COLUMNS}"
    ))
    .bind(provider_id)
    .bind(organization_id)
    .bind(flow)
    .bind(hash_state(&state))
    .bind(&verifier)
    .bind(return_to)
    .bind(expires_at)
    .fetch_optional(pool)
    .await?;

    row.map(|challenge| IssuedChallenge { challenge, state })
        .ok_or_else(|| IdentityError::InvalidProvider("the sign-in could not be started".into()))
}

/// Find a live challenge for a provider by the state the browser presented.
///
/// Every refusal is a distinct reason, because "sign-in failed" is useless to the person trying
/// to sign in: an expired challenge (they waited), an unknown one (the state was not ours — a
/// forged or already-consumed callback) and a provider mismatch (a callback aimed at another
/// provider) are three different problems.
pub async fn claim(pool: &PgPool, provider_id: Uuid, state: &str) -> Result<SsoChallenge> {
    if state.trim().is_empty() {
        return Err(IdentityError::InvalidProvider(
            "the sign-in state is missing".into(),
        ));
    }

    let row = sqlx::query_as::<_, SsoChallenge>(&format!(
        "select {COLUMNS} from sso_challenges \
         where state_hash = $1 and provider_id = $2 \
         order by created_at desc limit 1"
    ))
    .bind(hash_state(state))
    .bind(provider_id)
    .fetch_optional(pool)
    .await?;

    let challenge = row.ok_or_else(|| {
        IdentityError::InvalidProvider(
            "this sign-in link is not valid any more — start again".into(),
        )
    })?;

    if challenge.consumed_at.is_some() {
        return Err(IdentityError::InvalidProvider(
            "this sign-in link was already used — start again".into(),
        ));
    }
    if challenge.expires_at <= OffsetDateTime::now_utc() {
        return Err(IdentityError::InvalidProvider(
            "this sign-in link expired — start again".into(),
        ));
    }
    if challenge.attempts >= MAX_CHALLENGE_ATTEMPTS {
        return Err(IdentityError::InvalidProvider(
            "this sign-in link was tried too often — start again".into(),
        ));
    }

    // Every presentation counts, successful or not: a challenge under a guessing attack is burned
    // rather than retried.
    sqlx::query("update sso_challenges set attempts = attempts + 1 where id = $1")
        .bind(challenge.id)
        .execute(pool)
        .await?;

    Ok(challenge)
}

/// Mark a challenge used. A second call finds nothing, so a replayed callback cannot consume a
/// fresh challenge.
pub async fn consume(pool: &PgPool, id: Uuid) -> Result<bool> {
    let result = sqlx::query(
        "update sso_challenges set consumed_at = now() \
         where id = $1 and consumed_at is null",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Delete the challenges of a provider whose window has passed. Called opportunistically when a
/// new sign-in starts, so the table does not accumulate expired rows on its own.
pub async fn purge_expired(pool: &PgPool) -> Result<u64> {
    let result = sqlx::query(
        "delete from sso_challenges \
         where expires_at < now() - interval '1 day'",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_is_url_safe_and_unpredictable() {
        let one = random_token();
        let two = random_token();
        assert_ne!(one, two, "two states must never collide");
        assert!(one.len() >= 32, "a state is long enough to resist guessing");
        assert!(
            one.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "a state travels in a query string unescaped"
        );
    }

    #[test]
    fn hashing_a_state_is_stable_and_not_reversible() {
        let state = "the-state-a-browser-carries";
        let hash = hash_state(state);
        assert_eq!(hash, hash_state(state));
        assert_ne!(hash, state);
        assert_eq!(hash.len(), 64, "a SHA-256 hex digest");
    }
}
