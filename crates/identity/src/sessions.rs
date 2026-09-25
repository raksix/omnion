//! Server-side sessions.
//!
//! A session is a row in the `sessions` table; the client only ever holds the raw token in an
//! HttpOnly cookie. The database stores the SHA-256 hash of that token, so a leaked database
//! dump cannot be replayed as a session (docs/07-IAM.md).

use rand::RngCore;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{IdentityError, Result};
use crate::users::User;

/// Session lifetime in days; also the cookie `Max-Age` handed to the client.
pub const SESSION_TTL_DAYS: i32 = 30;

/// Session lifetime in seconds (cookie `Max-Age`).
pub const SESSION_TTL_SECONDS: i64 = SESSION_TTL_DAYS as i64 * 24 * 60 * 60;

/// Entropy of a session token, in bytes (256 bits).
const TOKEN_BYTES: usize = 32;

/// Longest user-agent string kept for diagnostics.
const MAX_USER_AGENT_LENGTH: usize = 512;

/// A stored session.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Session {
    /// Primary key.
    pub id: Uuid,
    /// Owning account.
    pub user_id: Uuid,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// Expiry timestamp.
    pub expires_at: OffsetDateTime,
    /// Last time the session was seen (best effort).
    pub last_seen_at: Option<OffsetDateTime>,
}

/// A resolved session together with the account it belongs to.
#[derive(Debug, Clone)]
pub struct AuthenticatedSession {
    /// The session row.
    pub session: Session,
    /// The account.
    pub user: User,
}

/// Generate a fresh session token (256 bits of entropy, hex encoded).
#[must_use]
pub fn generate_token() -> String {
    let mut bytes = [0_u8; TOKEN_BYTES];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Hash a session token for storage and lookup.
#[must_use]
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// Create a session for `user_id` and return it together with the raw token.
///
/// The raw token is returned exactly once — it belongs in the response cookie and nowhere
/// else; only its hash is persisted.
pub async fn create_session(
    pool: &PgPool,
    user_id: Uuid,
    user_agent: Option<&str>,
    ip_address: Option<&str>,
) -> Result<(Session, String)> {
    let token = generate_token();
    let token_hash = hash_token(&token);
    let user_agent = user_agent.map(|value| truncate(value, MAX_USER_AGENT_LENGTH));

    let session: Session = sqlx::query_as(
        "insert into sessions (user_id, token_hash, user_agent, ip_address, expires_at) \
         values ($1, $2, $3, cast($4 as inet), now() + make_interval(days => $5)) \
         returning id, user_id, created_at, expires_at, last_seen_at",
    )
    .bind(user_id)
    .bind(&token_hash)
    .bind(user_agent.as_deref())
    .bind(ip_address)
    .bind(SESSION_TTL_DAYS)
    .fetch_one(pool)
    .await?;

    Ok((session, token))
}

/// Resolve a raw token into its session and account.
///
/// Returns `None` for unknown, expired or revoked tokens, and for accounts that are no longer
/// active — the caller cannot tell those cases apart, which is intentional.
pub async fn resolve_session(pool: &PgPool, token: &str) -> Result<Option<AuthenticatedSession>> {
    let token_hash = hash_token(token);

    let row: Option<ResolvedRow> = sqlx::query_as(
        "select s.id as session_id, s.user_id as session_user_id, s.created_at as session_created_at, \
                s.expires_at as session_expires_at, s.last_seen_at as session_last_seen_at, \
                u.id as user_id, u.organization_id, u.email, u.display_name, u.status, u.created_at as user_created_at \
         from sessions s \
         join users u on u.id = s.user_id \
         where s.token_hash = $1 \
           and s.revoked_at is null \
           and s.expires_at > now() \
           and u.status = 'active'",
    )
    .bind(&token_hash)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(ResolvedRow::into_authenticated))
}

/// Record that a session was used. Failures are the caller's to log — they are never fatal.
pub async fn touch_session(pool: &PgPool, session_id: Uuid) -> Result<()> {
    sqlx::query("update sessions set last_seen_at = now() where id = $1")
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Revoke a session by raw token. Returns `true` when a live session was revoked.
pub async fn revoke_session(pool: &PgPool, token: &str) -> Result<bool> {
    let token_hash = hash_token(token);
    let result = sqlx::query(
        "update sessions set revoked_at = now() where token_hash = $1 and revoked_at is null",
    )
    .bind(&token_hash)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Revoke every live session of an account (used by password resets and admin actions).
pub async fn revoke_sessions_for_user(pool: &PgPool, user_id: Uuid) -> Result<u64> {
    let result = sqlx::query(
        "update sessions set revoked_at = now() where user_id = $1 and revoked_at is null",
    )
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Row shape of the session/account join.
#[derive(sqlx::FromRow)]
struct ResolvedRow {
    session_id: Uuid,
    session_user_id: Uuid,
    session_created_at: OffsetDateTime,
    session_expires_at: OffsetDateTime,
    session_last_seen_at: Option<OffsetDateTime>,
    user_id: Uuid,
    organization_id: Option<Uuid>,
    email: String,
    display_name: String,
    status: String,
    user_created_at: OffsetDateTime,
}

impl ResolvedRow {
    fn into_authenticated(self) -> AuthenticatedSession {
        AuthenticatedSession {
            session: Session {
                id: self.session_id,
                user_id: self.session_user_id,
                created_at: self.session_created_at,
                expires_at: self.session_expires_at,
                last_seen_at: self.session_last_seen_at,
            },
            user: User {
                id: self.user_id,
                organization_id: self.organization_id,
                email: self.email,
                display_name: self.display_name,
                status: self.status,
                created_at: self.user_created_at,
            },
        }
    }
}

/// Cut `value` to at most `max` characters, on a character boundary.
fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

/// Reject tokens that cannot have been produced by [`generate_token`].
pub fn validate_token_shape(token: &str) -> Result<()> {
    let shaped = token.len() == TOKEN_BYTES * 2 && token.chars().all(|c| c.is_ascii_hexdigit());
    if shaped {
        return Ok(());
    }
    Err(IdentityError::InvalidToken(
        "expected 64 hexadecimal characters".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn tokens_are_256_bits_of_hex_and_never_repeat() {
        let mut seen = HashSet::new();
        for _ in 0..256 {
            let token = generate_token();
            assert_eq!(token.len(), 64, "token: {token}");
            validate_token_shape(&token).expect("generated tokens must pass the shape check");
            assert!(seen.insert(token), "tokens must not repeat");
        }
    }

    #[test]
    fn token_hashes_are_deterministic_and_hide_the_token() {
        let token = generate_token();
        let hash = hash_token(&token);
        assert_eq!(hash, hash_token(&token), "hashing must be deterministic");
        assert_ne!(hash, token, "the stored hash must not be the token");
        assert_eq!(hash.len(), 64);
        assert_ne!(hash, hash_token(&generate_token()));
    }

    #[test]
    fn malformed_tokens_are_rejected_by_the_shape_check() {
        for bad in ["", "abc", &"z".repeat(64), &"a".repeat(63)] {
            assert!(validate_token_shape(bad).is_err(), "{bad:?} must fail");
        }
    }

    #[test]
    fn user_agent_is_truncated_on_a_character_boundary() {
        let long = "ü".repeat(MAX_USER_AGENT_LENGTH + 10);
        let cut = truncate(&long, MAX_USER_AGENT_LENGTH);
        assert_eq!(cut.chars().count(), MAX_USER_AGENT_LENGTH);
    }

    #[test]
    fn ttl_constants_agree() {
        assert_eq!(SESSION_TTL_SECONDS, 30 * 24 * 60 * 60);
    }
}
