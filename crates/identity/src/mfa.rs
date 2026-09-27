//! Second factors: TOTP enrolment and verification, plus single-use recovery codes.
//!
//! A factor is a row in `mfa_factors`; its secret lives encrypted ([`crate::secrets`]) and is
//! read back only to verify a code. Enrolment is two steps on purpose — creating the factor and
//! confirming it with a code the phone just produced — because a secret nobody has proven they
//! can read would lock the account out of its own second factor.
//!
//! Recovery codes are generated at confirmation: ten single-use strings, stored as SHA-256
//! hashes, shown exactly once. Verification consumes a code with a conditional update
//! (`where used_at is null`), which is what makes "works exactly once" true even when two
//! requests race for it.

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{IdentityError, Result};
use crate::secrets::SecretBox;
use crate::totp;

/// How many recovery codes a confirmation issues.
pub const RECOVERY_CODE_COUNT: usize = 10;

/// Characters of one recovery code half (two halves make one code).
const RECOVERY_CODE_CHARS: usize = 5;

/// The alphabet recovery codes draw from: no `I`, `O`, `0` or `1`, so a code read over the
/// phone cannot be mistyped into another valid one.
const RECOVERY_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Longest label accepted for a factor.
const MAX_LABEL_LENGTH: usize = 64;

/// An enrolled (or pending) second factor.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MfaFactor {
    /// Primary key.
    pub id: Uuid,
    /// Owning account.
    pub user_id: Uuid,
    /// `totp`, `webauthn` or `recovery`.
    pub kind: String,
    /// Reader-facing label.
    pub label: String,
    /// Encrypted TOTP secret (TOTP only).
    pub secret_ciphertext: Option<String>,
    /// WebAuthn credential id (passkey only).
    pub credential_id: Option<String>,
    /// WebAuthn public key (passkey only).
    pub public_key: Option<String>,
    /// WebAuthn signature counter.
    pub sign_count: i64,
    /// WebAuthn transports.
    pub transports: Vec<String>,
    /// When the factor was confirmed with a code.
    pub confirmed_at: Option<OffsetDateTime>,
    /// When it last verified.
    pub last_used_at: Option<OffsetDateTime>,
    /// When it was created.
    pub created_at: OffsetDateTime,
}

/// What a fresh TOTP enrolment hands back to the caller — exactly once.
#[derive(Debug, Clone)]
pub struct TotpEnrollment {
    /// The pending factor (unconfirmed until a code arrives).
    pub factor: MfaFactor,
    /// The secret in Base32, for manual entry.
    pub secret: String,
    /// The `otpauth://` URI a phone scans.
    pub otpauth_uri: String,
}

/// The outcome of verifying a user-supplied code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    /// A TOTP factor matched.
    Totp {
        /// The factor that matched.
        factor_id: Uuid,
    },
    /// A recovery code matched and is now spent.
    Recovery,
}

/// Column list of a factor row.
const FACTOR_COLUMNS: &str = "id, user_id, kind, label, secret_ciphertext, credential_id, \
     public_key, sign_count, transports, confirmed_at, last_used_at, created_at";

/// Every live factor of an account, newest first.
pub async fn list_factors(pool: &PgPool, user_id: Uuid) -> Result<Vec<MfaFactor>> {
    let factors: Vec<MfaFactor> = sqlx::query_as(&format!(
        "select {FACTOR_COLUMNS} from mfa_factors \
         where user_id = $1 and revoked_at is null \
         order by created_at desc"
    ))
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(factors)
}

/// How many confirmed factors an account holds.
pub async fn confirmed_count(pool: &PgPool, user_id: Uuid) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from mfa_factors \
         where user_id = $1 and revoked_at is null and confirmed_at is not null \
           and kind in ('totp', 'webauthn')",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Whether a sign-in has to ask for a second factor.
pub async fn has_confirmed_factor(pool: &PgPool, user_id: Uuid) -> Result<bool> {
    Ok(confirmed_count(pool, user_id).await? > 0)
}

/// Start TOTP enrolment: create a pending factor and hand back its secret once.
pub async fn enroll_totp(
    pool: &PgPool,
    user_id: Uuid,
    label: Option<&str>,
    issuer: &str,
    account: &str,
    secret_box: &SecretBox,
) -> Result<TotpEnrollment> {
    let secret = totp::generate_secret();
    let secret_base32 = totp::base32_encode(&secret);
    let ciphertext = secret_box.encrypt(&secret_base32.clone().into_bytes());
    let label = label
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("Authenticator app");
    let label: String = label.chars().take(MAX_LABEL_LENGTH).collect();

    // One pending TOTP enrolment at a time: an abandoned attempt must not leave dead secrets
    // behind (the account holder restarts the flow and the old row is retired).
    sqlx::query(
        "update mfa_factors set revoked_at = now() \
         where user_id = $1 and kind = 'totp' and confirmed_at is null and revoked_at is null",
    )
    .bind(user_id)
    .execute(pool)
    .await?;

    let factor: MfaFactor = sqlx::query_as(&format!(
        "insert into mfa_factors (user_id, kind, label, secret_ciphertext) \
         values ($1, 'totp', $2, $3) returning {FACTOR_COLUMNS}"
    ))
    .bind(user_id)
    .bind(&label)
    .bind(&ciphertext)
    .fetch_one(pool)
    .await?;

    Ok(TotpEnrollment {
        factor,
        otpauth_uri: totp::otpauth_uri(issuer, account, &secret_base32),
        secret: secret_base32,
    })
}

/// Confirm a pending TOTP factor with a code, and issue the recovery codes once.
pub async fn confirm_totp(
    pool: &PgPool,
    user_id: Uuid,
    factor_id: Uuid,
    code: &str,
    secret_box: &SecretBox,
    unix_seconds: i64,
) -> Result<Vec<String>> {
    let factor = find_factor(pool, user_id, factor_id)
        .await?
        .ok_or(IdentityError::FactorNotFound)?;
    if factor.kind != "totp" {
        return Err(IdentityError::InvalidFactor(
            "only a TOTP factor is confirmed with a code".to_owned(),
        ));
    }
    let ciphertext = factor
        .secret_ciphertext
        .as_deref()
        .ok_or(IdentityError::Crypto)?;
    let secret = secret_box.decrypt(ciphertext)?;
    let secret = totp::base32_decode(
        std::str::from_utf8(&secret).map_err(|_| IdentityError::Crypto)?,
    )
    .ok_or(IdentityError::Crypto)?;

    if !totp::verify(&secret, code, unix_seconds, totp::DEFAULT_WINDOW) {
        return Err(IdentityError::InvalidFactor(
            "that code does not match — check the clock on the device".to_owned(),
        ));
    }

    sqlx::query(
        "update mfa_factors set confirmed_at = now(), last_used_at = now() where id = $1",
    )
    .bind(factor_id)
    .execute(pool)
    .await?;

    // Recovery codes are re-issued on every confirmation: the old set belongs to the state the
    // account is leaving, and keeping both would mean two live sets.
    refresh_recovery_codes(pool, user_id).await
}

/// Generate a fresh set of single-use recovery codes, replacing any previous set.
pub async fn refresh_recovery_codes(pool: &PgPool, user_id: Uuid) -> Result<Vec<String>> {
    sqlx::query("delete from mfa_recovery_codes where user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await?;

    let mut codes = Vec::with_capacity(RECOVERY_CODE_COUNT);
    for _ in 0..RECOVERY_CODE_COUNT {
        let code = generate_recovery_code();
        let hash = hash_recovery_code(&code);
        sqlx::query("insert into mfa_recovery_codes (user_id, code_hash) values ($1, $2)")
            .bind(user_id)
            .bind(&hash)
            .execute(pool)
            .await?;
        codes.push(code);
    }
    Ok(codes)
}

/// How many recovery codes an account has left.
pub async fn remaining_recovery_codes(pool: &PgPool, user_id: Uuid) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from mfa_recovery_codes where user_id = $1 and used_at is null",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Verify a code against the account's factors: a TOTP factor first, then a recovery code.
///
/// `Ok(None)` means "no factor matched" — the caller decides whether that is a `400` or a
/// refusal; an `Err` is reserved for the store and the envelope.
pub async fn verify_code(
    pool: &PgPool,
    user_id: Uuid,
    code: &str,
    secret_box: &SecretBox,
    unix_seconds: i64,
) -> Result<Option<Verification>> {
    let code = code.trim();
    if code.is_empty() {
        return Ok(None);
    }

    // A six-digit code can only be a TOTP code; a recovery code is longer, but both are tried
    // in a fixed order so a code that matches neither is refused the same way.
    for factor in list_factors(pool, user_id).await? {
        if factor.kind != "totp" || factor.confirmed_at.is_none() {
            continue;
        }
        let Some(ciphertext) = factor.secret_ciphertext.as_deref() else {
            continue;
        };
        let Ok(secret_base32) = secret_box.decrypt(ciphertext) else {
            continue;
        };
        let Some(secret) = totp::base32_decode(&String::from_utf8_lossy(&secret_base32)) else {
            continue;
        };
        if totp::verify(&secret, code, unix_seconds, totp::DEFAULT_WINDOW) {
            sqlx::query("update mfa_factors set last_used_at = now() where id = $1")
                .bind(factor.id)
                .execute(pool)
                .await?;
            return Ok(Some(Verification::Totp {
                factor_id: factor.id,
            }));
        }
    }

    // A recovery code is spent by the update that reads it: two requests racing for the same
    // code cannot both win, because only one of them sees `used_at is null`.
    let consumed: Option<Uuid> = sqlx::query_scalar(
        "update mfa_recovery_codes set used_at = now() \
         where user_id = $1 and code_hash = $2 and used_at is null \
         returning id",
    )
    .bind(user_id)
    .bind(hash_recovery_code(code))
    .fetch_optional(pool)
    .await?;

    Ok(consumed.map(|_| Verification::Recovery))
}

/// Clear every factor and recovery code of an account (admin reset, after step-up).
pub async fn reset(pool: &PgPool, user_id: Uuid) -> Result<u64> {
    let revoked = sqlx::query(
        "update mfa_factors set revoked_at = now() where user_id = $1 and revoked_at is null",
    )
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected();

    sqlx::query("delete from mfa_recovery_codes where user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await?;

    Ok(revoked)
}

/// Retire one factor (the account holder removing a device they no longer have).
pub async fn revoke_factor(pool: &PgPool, user_id: Uuid, factor_id: Uuid) -> Result<bool> {
    let revoked = sqlx::query(
        "update mfa_factors set revoked_at = now() \
         where id = $1 and user_id = $2 and revoked_at is null",
    )
    .bind(factor_id)
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(revoked > 0)
}

/// Read one live factor of an account.
pub async fn find_factor(
    pool: &PgPool,
    user_id: Uuid,
    factor_id: Uuid,
) -> Result<Option<MfaFactor>> {
    let factor: Option<MfaFactor> = sqlx::query_as(&format!(
        "select {FACTOR_COLUMNS} from mfa_factors \
         where id = $1 and user_id = $2 and revoked_at is null"
    ))
    .bind(factor_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(factor)
}

/// A fresh recovery code, formatted `ABCDE-FGHJK`.
#[must_use]
pub fn generate_recovery_code() -> String {
    let mut bytes = [0_u8; RECOVERY_CODE_CHARS * 2];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    let characters: Vec<char> = bytes
        .iter()
        .map(|byte| char::from(RECOVERY_ALPHABET[usize::from(*byte) % RECOVERY_ALPHABET.len()]))
        .collect();
    let first: String = characters[..RECOVERY_CODE_CHARS].iter().collect();
    let second: String = characters[RECOVERY_CODE_CHARS..].iter().collect();
    format!("{first}-{second}")
}

/// Hash a recovery code for storage; typing dashes, spaces or lower case does not matter.
#[must_use]
pub fn hash_recovery_code(code: &str) -> String {
    let normalized: String = code
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|character| character.to_ascii_uppercase())
        .collect();
    let mut hasher = Sha256::new();
    hasher.update(b"omnion.recovery.v1");
    hasher.update(normalized.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_codes_are_readable_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..500 {
            let code = generate_recovery_code();
            assert_eq!(code.len(), RECOVERY_CODE_CHARS * 2 + 1, "{code}");
            assert!(code.contains('-'));
            assert!(
                !code.contains('I') && !code.contains('O') && !code.contains('0'),
                "ambiguous characters are not in the alphabet: {code}"
            );
            assert!(seen.insert(code), "codes must not repeat");
        }
    }

    #[test]
    fn hashing_forgives_the_way_people_type_codes() {
        let hash = hash_recovery_code("ABCDE-FGHJK");
        assert_eq!(hash, hash_recovery_code("abcde fghjk"));
        assert_eq!(hash, hash_recovery_code("ABCDEFGHJK"));
        assert_ne!(hash, hash_recovery_code("ABCDE-FGHJL"));
        assert_eq!(hash.len(), 64);
    }
}
