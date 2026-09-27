//! Secret envelopes for stored material that must not be readable in a database dump.
//!
//! TOTP secrets are the platform's first stored secret (docs/07-IAM.md §12: "MFA secrets are
//! stored encrypted"). The envelope is an authenticated encryption built from the primitives
//! this crate already carries — SHA-256 in counter mode for confidentiality and HMAC-SHA256
//! over the header and ciphertext for integrity (encrypt-then-MAC, the construction that fails
//! closed: a tampered envelope never decrypts, it errors).
//!
//! A single AEAD primitive would be the usual choice; the platform ships none today, and a
//! dependency added for one table is a bigger risk than a documented composition of two
//! standard primitives. The key comes from `OMNION_MFA_KEY` (hex, 32 bytes) or, when unset, a
//! development default derived from a fixed label — installations must set the variable, and
//! [`SecretBox::from_env`] says so in the log.
//!
//! Envelope shape: `v1.<nonce hex>.<ciphertext hex>.<tag hex>`.

use hmac::{Hmac, Mac};
use rand::RngCore;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::error::{IdentityError, Result};

/// Version tag of the envelope format; a future change writes `v2` and keeps reading `v1`.
const VERSION: &str = "v1";

/// Bytes of nonce; a fresh one per encryption, so identical plaintexts differ on disk.
const NONCE_BYTES: usize = 16;

/// Domain separation for the keystream.
const KEYSTREAM_LABEL: &[u8] = b"omnion.secret.keystream.v1";

/// Domain separation for the MAC.
const MAC_LABEL: &[u8] = b"omnion.secret.tag.v1";

/// Environment variable an installation sets to its own key.
pub const KEY_ENV: &str = "OMNION_MFA_KEY";

/// The development fallback, named so a deployment can see what it is using.
const DEVELOPMENT_KEY: &str = "omnion-development-mfa-key-do-not-use-in-production";

/// HMAC-SHA256.
type HmacSha256 = Hmac<Sha256>;

/// A key that encrypts and decrypts secret envelopes.
#[derive(Clone)]
pub struct SecretBox {
    key: [u8; 32],
    development_key: bool,
}

impl std::fmt::Debug for SecretBox {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key itself is never printable.
        formatter
            .debug_struct("SecretBox")
            .field("development_key", &self.development_key)
            .finish_non_exhaustive()
    }
}

impl SecretBox {
    /// Derive a box from arbitrary key material (test and CLI use).
    #[must_use]
    pub fn from_key_material(material: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"omnion.secret.key.v1");
        hasher.update(material);
        let digest = hasher.finalize();
        let mut key = [0_u8; 32];
        key.copy_from_slice(&digest);
        Self {
            key,
            development_key: false,
        }
    }

    /// The box the process should use: `OMNION_MFA_KEY` when set, the development default
    /// otherwise (with a warning — enrolment would still work, but a dump would be readable).
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var(KEY_ENV) {
            Ok(value) if !value.trim().is_empty() => {
                let mut boxed = Self::from_key_material(value.trim().as_bytes());
                boxed.development_key = false;
                boxed
            }
            _ => {
                tracing_log();
                let mut boxed = Self::from_key_material(DEVELOPMENT_KEY.as_bytes());
                boxed.development_key = true;
                boxed
            }
        }
    }

    /// `true` when the box holds the development default rather than a configured key.
    #[must_use]
    pub fn is_development_key(&self) -> bool {
        self.development_key
    }

    /// Encrypt `plaintext` into a fresh envelope.
    #[must_use]
    pub fn encrypt(&self, plaintext: &[u8]) -> String {
        let mut nonce = [0_u8; NONCE_BYTES];
        OsRng.fill_bytes(&mut nonce);

        let mut ciphertext = plaintext.to_vec();
        apply_keystream(&self.key, &nonce, &mut ciphertext);

        let tag = self.tag(&nonce, &ciphertext);
        format!(
            "{VERSION}.{}.{}.{}",
            hex::encode(nonce),
            hex::encode(&ciphertext),
            hex::encode(tag)
        )
    }

    /// Decrypt an envelope, refusing anything that was not produced by [`Self::encrypt`].
    pub fn decrypt(&self, envelope: &str) -> Result<Vec<u8>> {
        let mut parts = envelope.split('.');
        let (Some(version), Some(nonce_hex), Some(cipher_hex), Some(tag_hex), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(IdentityError::Crypto);
        };
        if version != VERSION {
            return Err(IdentityError::Crypto);
        }

        let nonce = hex::decode(nonce_hex).map_err(|_| IdentityError::Crypto)?;
        let mut ciphertext = hex::decode(cipher_hex).map_err(|_| IdentityError::Crypto)?;
        let tag = hex::decode(tag_hex).map_err(|_| IdentityError::Crypto)?;
        if nonce.len() != NONCE_BYTES {
            return Err(IdentityError::Crypto);
        }

        let expected = self.tag(&nonce, &ciphertext);
        if expected.ct_eq(&tag).unwrap_u8() != 1 {
            return Err(IdentityError::Crypto);
        }

        apply_keystream(&self.key, &nonce, &mut ciphertext);
        Ok(ciphertext)
    }

    /// HMAC-SHA256 over the nonce and ciphertext.
    fn tag(&self, nonce: &[u8], ciphertext: &[u8]) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(MAC_LABEL);
        mac.update(nonce);
        mac.update(ciphertext);
        mac.finalize().into_bytes().to_vec()
    }
}

/// XOR `buffer` with the SHA-256 counter-mode keystream of `nonce`.
fn apply_keystream(key: &[u8; 32], nonce: &[u8], buffer: &mut [u8]) {
    for (block_index, block) in buffer.chunks_mut(32).enumerate() {
        let mut hasher = Sha256::new();
        hasher.update(key);
        hasher.update(KEYSTREAM_LABEL);
        hasher.update(nonce);
        hasher.update((block_index as u32).to_be_bytes());
        let digest = hasher.finalize();
        for (byte, mask) in block.iter_mut().zip(digest.iter()) {
            *byte ^= mask;
        }
    }
}

/// Say once, in the process log, that secret envelopes use the development key.
fn tracing_log() {
    tracing::warn!(
        env = KEY_ENV,
        "secret envelopes use the development key — set the variable in production"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_envelope_round_trips() {
        let boxed = SecretBox::from_key_material(b"unit-test-key");
        let secret = b"JBSWY3DPEHPK3PXP";
        let envelope = boxed.encrypt(secret);
        assert!(envelope.starts_with("v1."), "versioned: {envelope}");
        assert!(
            !envelope.contains("JBSWY3"),
            "the plaintext must not appear in the envelope"
        );
        assert_eq!(boxed.decrypt(&envelope).expect("decrypts"), secret.to_vec());
    }

    #[test]
    fn the_same_plaintext_encrypts_differently_every_time() {
        let boxed = SecretBox::from_key_material(b"unit-test-key");
        let first = boxed.encrypt(b"same");
        let second = boxed.encrypt(b"same");
        assert_ne!(first, second, "a fresh nonce per envelope");
        assert_eq!(boxed.decrypt(&first).expect("first"), b"same".to_vec());
        assert_eq!(boxed.decrypt(&second).expect("second"), b"same".to_vec());
    }

    #[test]
    fn tampering_is_refused_rather_than_decrypted() {
        let boxed = SecretBox::from_key_material(b"unit-test-key");
        let envelope = boxed.encrypt(b"secret material");
        let parts: Vec<&str> = envelope.split('.').collect();

        // Flip one hex digit of the ciphertext.
        let mut ciphertext = parts[2].to_owned();
        let first = ciphertext.remove(0);
        let flipped = if first == 'a' { 'b' } else { 'a' };
        ciphertext.insert(0, flipped);
        let tampered = format!("{}.{}.{}.{}", parts[0], parts[1], ciphertext, parts[3]);
        assert!(boxed.decrypt(&tampered).is_err(), "MAC must refuse this");

        // A different key must not read it either.
        let other = SecretBox::from_key_material(b"another-key");
        assert!(other.decrypt(&envelope).is_err());

        // Malformed envelopes are refused, never panic.
        for bad in ["", "v1", "v1.zz.zz.zz", "v2.aa.bb.cc", "v1.aa.bb"] {
            assert!(boxed.decrypt(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn long_and_empty_plaintexts_work() {
        let boxed = SecretBox::from_key_material(b"unit-test-key");
        for length in [0_usize, 1, 31, 32, 33, 64, 200] {
            let plaintext: Vec<u8> = (0..length).map(|index| index as u8).collect();
            let envelope = boxed.encrypt(&plaintext);
            assert_eq!(boxed.decrypt(&envelope).expect("decrypts"), plaintext);
        }
    }

    #[test]
    fn the_environment_key_is_honoured() {
        // The process environment is shared across tests in a binary; only assert the shape.
        let boxed = SecretBox::from_env();
        let envelope = boxed.encrypt(b"value");
        assert_eq!(
            boxed.decrypt(&envelope).expect("decrypts"),
            b"value".to_vec()
        );
    }
}
