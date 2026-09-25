//! Password hashing and the password policy.
//!
//! Passwords are hashed with Argon2id (the default parameters of the `argon2` crate: 19 MiB
//! memory, two passes, one lane) and a fresh random salt per password. Hashing is CPU-bound,
//! so every operation runs on the blocking pool and never stalls the async runtime.

use std::sync::OnceLock;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use rand::rngs::OsRng;

use crate::error::{IdentityError, Result};

/// Minimum accepted password length. Applied when an account or the first administrator is
/// created; sign-in only verifies against the stored hash.
pub const MIN_PASSWORD_LENGTH: usize = 10;

/// Reject obviously unusable passwords before any hashing work happens.
pub fn validate_password_strength(password: &str) -> Result<()> {
    if password.chars().count() < MIN_PASSWORD_LENGTH {
        return Err(IdentityError::WeakPassword {
            min: MIN_PASSWORD_LENGTH,
        });
    }
    Ok(())
}

/// Hash a plaintext password into a PHC string (`$argon2id$…`).
pub async fn hash_password(password: impl Into<String>) -> Result<String> {
    let password = password.into();
    validate_password_strength(&password)?;
    blocking(move || hash_password_blocking(&password)).await
}

/// Verify a plaintext password against a stored PHC hash.
///
/// A mismatch is `Ok(false)`, never an error — malformed stored hashes are the error case.
pub async fn verify_password(password: impl Into<String>, hash: impl Into<String>) -> Result<bool> {
    let password = password.into();
    let hash = hash.into();
    blocking(move || verify_password_blocking(&password, &hash)).await
}

/// Burn the same amount of work as a real verification.
///
/// Used when the email address does not exist (or has no password), so a failed sign-in takes
/// as long as a successful one and the response does not reveal which accounts exist.
pub async fn dummy_verify(password: impl Into<String>) -> Result<()> {
    let password = password.into();
    let hash = dummy_hash()?;
    blocking(move || verify_password_blocking(&password, &hash).map(|_| ())).await
}

/// Argon2id hash of a fixed throwaway value, computed once per process.
fn dummy_hash() -> Result<String> {
    static DUMMY_HASH: OnceLock<String> = OnceLock::new();

    if let Some(hash) = DUMMY_HASH.get() {
        return Ok(hash.clone());
    }
    let hash = hash_password_blocking("omnion-timing-equalizer")?;
    let _ = DUMMY_HASH.set(hash.clone());
    Ok(hash)
}

fn hash_password_blocking(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|err| IdentityError::PasswordHash(err.to_string()))
}

fn verify_password_blocking(password: &str, hash: &str) -> Result<bool> {
    let parsed =
        PasswordHash::new(hash).map_err(|err| IdentityError::PasswordHash(err.to_string()))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

/// Run CPU-bound hashing on the blocking pool.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| IdentityError::Task(err.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hash_and_verify_round_trip() {
        let hash = hash_password("correct horse battery")
            .await
            .expect("hashing must succeed");
        assert!(hash.starts_with("$argon2id$"), "hash: {hash}");
        assert!(
            verify_password("correct horse battery", &hash)
                .await
                .expect("verification must run"),
            "the correct password must verify"
        );
        assert!(
            !verify_password("wrong horse battery", &hash)
                .await
                .expect("verification must run"),
            "a different password must not verify"
        );
    }

    #[tokio::test]
    async fn each_hash_uses_a_fresh_salt() {
        let first = hash_password("correct horse battery")
            .await
            .expect("hashing must succeed");
        let second = hash_password("correct horse battery")
            .await
            .expect("hashing must succeed");
        assert_ne!(first, second, "salts must be random per hash");
    }

    #[tokio::test]
    async fn short_passwords_are_rejected_before_hashing() {
        let error = hash_password("short")
            .await
            .expect_err("short passwords must be rejected");
        assert!(matches!(error, IdentityError::WeakPassword { min } if min == MIN_PASSWORD_LENGTH));
    }

    #[test]
    fn strength_rules_use_character_count() {
        validate_password_strength("abcdefghij").expect("exactly the minimum is fine");
        assert!(validate_password_strength("abcdefghi").is_err());
        validate_password_strength("şifre-çok-uzun").expect("unicode passwords count characters");
    }

    #[tokio::test]
    async fn malformed_stored_hash_is_an_error_not_a_mismatch() {
        let error = verify_password("correct horse battery", "not-a-phc-string")
            .await
            .expect_err("a malformed hash must surface as an error");
        assert!(matches!(error, IdentityError::PasswordHash(_)));
    }

    #[tokio::test]
    async fn dummy_verify_completes_without_touching_any_account() {
        dummy_verify("anything at all")
            .await
            .expect("dummy verification must succeed");
    }
}
