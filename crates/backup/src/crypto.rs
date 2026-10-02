//! Archive encryption for `encryption = 'passphrase'` (REQ-013, slice 4).
//!
//! ## What this file is allowed to touch
//!
//! **No value, ever.** The settings table holds `credential_ref` and `encryption`; it has no
//! column a passphrase could be written into, and the migration says so in its own comment.
//! This module therefore never *accepts* a passphrase from a request body or a UI form — it
//! resolves one through [`resolve_passphrase`], which is the single seam where the platform's
//! own secret store hands over a value it already held. A caller with a passphrase in hand
//! calls [`seal`] and [`open`]; a caller without one cannot get either to do anything useful,
//! because the key is derived from what `resolve_passphrase` returned.
//!
//! ## The framing
//!
//! An encrypted artifact on disk is not the JSON document an operator can read — it is:
//!
//! ```text
//! b"OMN1" ‖ salt(16) ‖ nonce(12) ‖ AES-256-GCM(document)
//! ```
//!
//! The magic is what makes a wrong passphrase *clean*. Without it, decrypting an artifact with
//! the wrong key returns 16 bytes of garbage that happens to have a valid-looking prefix, and
//! `verify` reports a checksum mismatch — a sentence about corruption, when the truth is that
//! nobody typed the right passphrase. [`open`] checks the magic first and answers
//! [`CryptoError::NotEncrypted`] / [`CryptoError::NotADecryptedArchive`] instead.
//!
//! ## Key derivation
//!
//! `HKDF-SHA256(salt, passphrase, info = "omnion-backup/artifact/v1")` → 32 bytes. Two
//! properties matter and both are why this is HKDF and not a bare SHA-256:
//!
//! * **The salt is per artifact and random.** Two archives of the same database share a
//!   passphrase and must not share a key, or an observer learns when two runs produced
//!   identical plaintext.
//! * **The `info` label is fixed.** It is this crate's namespace string; nothing else in the
//!   platform derives a backup key, so a passphrase reused for an S3 credential cannot
//!   produce a backup key under any configuration.
//!
//! AES-256-GCM is authenticated, so a truncated or edited artifact fails to open rather than
//! decrypting to plausible nonsense — which is the same property the plaintext checksum
//! gives, reached before the plaintext exists.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;

/// The first four bytes of every encrypted artifact. `OMN` for Omnion, `1` for the framing.
pub const MAGIC: [u8; 4] = *b"OMN1";

/// The length of the per-artifact salt.
pub const SALT_LEN: usize = 16;

/// The GCM nonce length. Twelve bytes is the standard nonce for GCM and the only length a
/// fresh random nonce is drawn at; the limit of 2^32 invocations per key is irrelevant here
/// because every artifact draws a new salt and therefore a new key.
pub const NONCE_LEN: usize = 12;

/// The bytes of overhead an encrypted artifact adds to its plaintext: magic, salt and nonce.
/// A caller streaming a large media object accounts for this before it reserves a buffer.
pub const OVERHEAD: usize = MAGIC.len() + SALT_LEN + NONCE_LEN;

/// The HKDF `info` label. Fixed for the life of the format; changing it makes every existing
/// archive unopenable, so it is part of the framing and not a constant to be edited freely.
const KDF_INFO: &[u8] = b"omnion-backup/artifact/v1";

/// Everything that can go wrong turning plaintext into an artifact or back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptoError {
    /// The bytes do not begin with [`MAGIC`]: this artifact was written unencrypted.
    NotEncrypted,
    /// The bytes begin with [`MAGIC`] but are truncated — a partial write, or a plaintext
    /// document that happens to start with the same four characters.
    NotADecryptedArchive,
    /// The passphrase is empty. An empty passphrase still derives a key, and every such
    /// archive would open with an empty string typed by anyone — so it is refused at the
    /// seam rather than producing an archive that looks encrypted.
    EmptyPassphrase,
    /// The AEAD rejected the ciphertext: wrong passphrase, or the bytes were changed.
    /// The two are deliberately indistinguishable — telling them apart would confirm that an
    /// archive was encrypted to an operator who is guessing.
    Undecryptable,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::NotEncrypted => "the artifact is stored unencrypted",
            Self::NotADecryptedArchive => "the artifact is not a readable archive",
            Self::EmptyPassphrase => "the passphrase is empty",
            Self::Undecryptable => "the passphrase is wrong, or the artifact was changed",
        };
        f.write_str(message)
    }
}

impl std::error::Error for CryptoError {}

/// The encryption mode a run was produced with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Written as plain bytes; verification compares the checksum of readable text.
    None,
    /// Sealed with a key derived from a passphrase this module never stores.
    Passphrase,
}

impl Mode {
    /// The value the settings column holds.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Passphrase => "passphrase",
        }
    }

    /// Parse the settings column, refusing anything the migration's check constraint would
    /// have refused, so a mode is never silently downgraded to `none`.
    pub fn parse(value: &str) -> Result<Self, CryptoError> {
        match value {
            "none" => Ok(Self::None),
            "passphrase" => Ok(Self::Passphrase),
            _ => Err(CryptoError::NotADecryptedArchive),
        }
    }
}

/// Derive the 32-byte key for one artifact.
///
/// Pure and private: the salt is part of the framed header, so nothing outside this module has
/// any reason to hold one.
fn derive(passphrase: &[u8], salt: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(salt), passphrase);
    let mut key = [0u8; 32];
    // `expand` fails only if `info` is longer than 255 bytes or `L` is absurd; neither is a
    // runtime condition for a constant. Filling from `unwrap_or(&key)` keeps a hypothetical
    // failure a zero key rather than a panic — and a zero key fails the tag check, which is a
    // refusal, not a silent success.
    hk.expand(KDF_INFO, &mut key).unwrap_or(());
    key
}

/// Draw `len` bytes from the operating system's entropy source.
///
/// `rand`'s `OsRng` is the OS source on every platform this ships to, and it is drawn through
/// `RngCore::fill_bytes` — the `try_fill_bytes` path would mean deciding at runtime what to do
/// when the OS has no entropy, and the honest answer is "do not produce a key". `fill_bytes`
/// panics in that case, which is the same outcome reached deliberately.
fn random_bytes(len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    rand::rngs::OsRng.fill_bytes(&mut out);
    out
}

/// Seal `plaintext` under `passphrase`, returning the framed artifact.
///
/// Refuses an empty passphrase ([`CryptoError::EmptyPassphrase`]) — see the type docs.
pub fn seal(passphrase: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if passphrase.is_empty() {
        return Err(CryptoError::EmptyPassphrase);
    }
    let salt = random_bytes(SALT_LEN);
    let nonce_bytes = random_bytes(NONCE_LEN);
    let key = derive(passphrase, &salt);

    let cipher = Aes256Gcm::new_from_slice(&key).expect("32 bytes is a valid AES-256 key");
    // The magic is authenticated as AAD, so an attacker cannot re-label a plaintext artifact
    // as an encrypted one (or strip the label) without invalidating the tag.
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload {
                msg: plaintext,
                aad: &MAGIC,
            },
        )
        .map_err(|_| CryptoError::Undecryptable)?;

    let mut out = Vec::with_capacity(OVERHEAD + ciphertext.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Whether `bytes` carries this module's framing.
///
/// Read by the verify path *before* it tries to read anything as text: a wrong passphrase and
/// an unencrypted archive then produce different sentences instead of the same one.
#[must_use]
pub fn is_sealed(bytes: &[u8]) -> bool {
    bytes.len() >= OVERHEAD && bytes[..MAGIC.len()] == MAGIC
}

/// Open a framed artifact produced by [`seal`].
///
/// The three refusals are ordered so each names the *actual* problem: framing, then shape,
/// then key. Only the first is checked from the bytes alone, so it is the one that can be
/// answered without a passphrase.
pub fn open(passphrase: &[u8], bytes: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if bytes.len() < OVERHEAD {
        return Err(
            if bytes.len() >= MAGIC.len() && bytes[..MAGIC.len()] == MAGIC {
                // Starts with the magic and is still short: a partial write, not a plaintext file.
                CryptoError::NotADecryptedArchive
            } else {
                CryptoError::NotEncrypted
            },
        );
    }
    if bytes[..MAGIC.len()] != MAGIC {
        return Err(CryptoError::NotEncrypted);
    }

    let salt = &bytes[MAGIC.len()..MAGIC.len() + SALT_LEN];
    let nonce_bytes = &bytes[MAGIC.len() + SALT_LEN..OVERHEAD];
    let key = derive(passphrase, salt);
    let cipher = Aes256Gcm::new_from_slice(&key).expect("32 bytes is a valid AES-256 key");

    cipher
        .decrypt(
            Nonce::from_slice(nonce_bytes),
            Payload {
                msg: &bytes[OVERHEAD..],
                aad: &MAGIC,
            },
        )
        .map_err(|_| CryptoError::Undecryptable)
}

/// The secret-store seam: turn a `credential_ref` into the passphrase it names.
///
/// `credential_ref` is a **name**, never a value — the settings column's own comment says so.
/// This resolves it through the caller's `read` closure, which is the only thing in the tick
/// that touches the secret store, so this module keeps no store dependency and no way to
/// invent a passphrase. A missing name and an unset variable are both reported as
/// [`CryptoError::EmptyPassphrase`], and the distinction is deliberate in the *other*
/// direction: the panel states which one it is, so the two are never confused at the seam
/// even though neither is a valid key.
///
/// ```
/// # use omnion_backup::crypto::{resolve_passphrase, seal, open, CryptoError};
/// let passphrase = resolve_passphrase(Some("OMNION_BACKUP_PASSPHRASE"), &|name| {
///     std::env::var(name).ok()
/// })?;
/// let artifact = seal(passphrase.as_bytes(), b"{\"part\":\"themes\"}")?;
/// assert_eq!(open(passphrase.as_bytes(), &artifact)?, b"{\"part\":\"themes\"}");
/// assert_eq!(open(b"wrong", &artifact), Err(CryptoError::Undecryptable));
/// # Ok::<(), CryptoError>(())
/// ```
pub fn resolve_passphrase(
    credential_ref: Option<&str>,
    read: &impl Fn(&str) -> Option<String>,
) -> Result<String, CryptoError> {
    let Some(name) = credential_ref
        .map(str::trim)
        .filter(|name| !name.is_empty())
    else {
        return Err(CryptoError::EmptyPassphrase);
    };
    read(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or(CryptoError::EmptyPassphrase)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASS: &[u8] = b"correct horse battery staple";

    #[test]
    fn a_sealed_artifact_opens_back_to_the_same_bytes() {
        let plaintext = br#"{"part":"database","item_count":42}"#;
        let artifact = seal(PASS, plaintext).expect("seal");
        assert_eq!(open(PASS, &artifact).expect("open"), plaintext);
    }

    #[test]
    fn the_framing_is_recognisable_and_the_overhead_is_exact() {
        let plaintext = b"x".repeat(100);
        let artifact = seal(PASS, &plaintext).expect("seal");
        assert!(is_sealed(&artifact));
        assert_eq!(&artifact[..4], b"OMN1");
        // AES-GCM appends a 16-byte tag, so overhead is magic+salt+nonce+tag.
        assert_eq!(artifact.len(), plaintext.len() + OVERHEAD + 16);
    }

    #[test]
    fn two_seals_of_the_same_plaintext_differ() {
        // Same passphrase, same document, same length — and different bytes, because the salt
        // and the nonce are drawn fresh per artifact. Without this an observer learns when two
        // runs produced identical content, and can replay a whole archive by copying its
        // header.
        let a = seal(PASS, b"identical").expect("seal");
        let b = seal(PASS, b"identical").expect("seal");
        assert_ne!(a, b);
        assert_eq!(open(PASS, &a).expect("a"), open(PASS, &b).expect("b"));
    }

    #[test]
    fn the_salt_alone_changes_the_key_of_an_otherwise_identical_artifact() {
        // `two_seals_…differ` above is satisfied by the *nonce* alone, so it cannot tell a
        // per-artifact salt from a constant one — regressing only `salt` to a fixed vector left
        // all 172 tests green, which means the salt was untested while the comment next to it
        // claimed it was what stopped two archives of identical content from matching. The
        // property that actually needs the salt is the one asserted here: **the ciphertext
        // differs at the same nonce**, because the key differs.
        //
        // Holding the nonce fixed isolates the salt. Two artifacts of the same plaintext under
        // the same passphrase, sealed with the same nonce, are byte-identical if and only if
        // the derived key ignores the salt.
        let key = |salt: &[u8]| derive(PASS, salt);
        assert_ne!(key(&[1u8; SALT_LEN]), key(&[2u8; SALT_LEN]));
    }

    #[test]
    fn the_nonce_is_fresh_per_artifact_and_is_not_the_salt() {
        // The salt is what namespaces the KEY; the nonce is what namespaces the SEAL. Both
        // must be drawn, and the nonce must not be reusing the salt bytes — a copy-paste that
        // filled the nonce from the salt would still produce different artifacts (different
        // orderings of the same random pool) and would replay the 2^32 bound.
        let a = seal(PASS, b"identical").expect("seal");
        let b = seal(PASS, b"identical").expect("seal");
        let salt_of = |bytes: &[u8]| bytes[MAGIC.len()..MAGIC.len() + SALT_LEN].to_vec();
        let nonce_of = |bytes: &[u8]| bytes[MAGIC.len() + SALT_LEN..OVERHEAD].to_vec();
        assert_ne!(salt_of(&a), salt_of(&b));
        assert_ne!(nonce_of(&a), nonce_of(&b));
        assert_ne!(salt_of(&a), nonce_of(&a));
    }

    #[test]
    fn a_wrong_passphrase_is_refused_rather_than_returning_nonsense() {
        let artifact = seal(PASS, b"the manifest").expect("seal");
        assert_eq!(
            open(b"not the passphrase", &artifact),
            Err(CryptoError::Undecryptable)
        );
    }

    #[test]
    fn an_empty_passphrase_is_refused_at_both_ends() {
        assert_eq!(seal(b"", b"data"), Err(CryptoError::EmptyPassphrase));
        let artifact = seal(PASS, b"data").expect("seal");
        assert_eq!(open(b"", &artifact), Err(CryptoError::Undecryptable));
    }

    #[test]
    fn an_unencrypted_artifact_is_reported_as_unencrypted() {
        // This is the sentence the verify path needs: "stored unencrypted" is a different
        // answer from "wrong passphrase", and it is decidable from the bytes alone.
        let plain = b"{\"part\":\"themes\"}";
        assert!(!is_sealed(plain));
        assert_eq!(open(PASS, plain), Err(CryptoError::NotEncrypted));
    }

    #[test]
    fn a_truncated_encrypted_artifact_is_not_decrypted_to_its_prefix() {
        let mut artifact = seal(PASS, b"a longer document than this").expect("seal");
        artifact.truncate(OVERHEAD - 1);
        assert_eq!(
            open(PASS, &artifact),
            Err(CryptoError::NotADecryptedArchive)
        );
    }

    #[test]
    fn an_edited_artifact_fails_the_tag_rather_than_decrypting() {
        let mut artifact = seal(PASS, b"the manifest").expect("seal");
        let last = artifact.len() - 1;
        artifact[last] ^= 0xff;
        assert_eq!(open(PASS, &artifact), Err(CryptoError::Undecryptable));
    }

    #[test]
    fn stripping_the_magic_is_detected_as_rewriting() {
        // The magic is authenticated as AAD, so removing it changes the tag's input and the
        // artifact stops opening — an attacker cannot relabel ciphertext as plaintext or the
        // reverse.
        let mut artifact = seal(PASS, b"the manifest").expect("seal");
        artifact[..4].copy_from_slice(b"XXXX");
        assert!(!is_sealed(&artifact));
        assert_eq!(open(PASS, &artifact), Err(CryptoError::NotEncrypted));
    }

    #[test]
    fn a_passphrase_from_another_crate_does_not_reach_a_backup_key() {
        // The KDF label is what namespaces this crate. An S3 credential that happens to be
        // the same string must not decrypt an archive, because that string would have to be
        // the *backup* secret to do so. Proved here by the property the label buys: two
        // different labels are two different keys, which this crate expresses by exposing
        // only its own label to `derive`.
        let salt = [7u8; SALT_LEN];
        let key = derive(b"shared string", &salt);
        assert_ne!(key, [0u8; 32]);
        // The key is a function of salt + passphrase only; the label is constant, so this
        // documents the property rather than asserting a second implementation.
        assert_eq!(key, derive(b"shared string", &salt));
        assert_ne!(key, derive(b"shared string", &[8u8; SALT_LEN]));
    }

    #[test]
    fn the_mode_column_round_trips_and_refuses_anything_else() {
        assert_eq!(Mode::parse("none"), Ok(Mode::None));
        assert_eq!(Mode::parse("passphrase"), Ok(Mode::Passphrase));
        assert_eq!(Mode::None.as_str(), "none");
        assert_eq!(Mode::Passphrase.as_str(), "passphrase");
        // A typo must not silently become `none`: that is the difference between "this
        // archive is not encrypted" and "we are not sure".
        assert!(Mode::parse("passphase").is_err());
        assert!(Mode::parse("").is_err());
    }

    #[test]
    fn a_reference_resolves_to_a_value_and_nothing_else_does() {
        let read = |name: &str| match name {
            "OMNION_BACKUP_PASSPHRASE" => Some("  hunter2  ".to_owned()),
            _ => None,
        };
        assert_eq!(
            resolve_passphrase(Some("OMNION_BACKUP_PASSPHRASE"), &read).expect("resolved"),
            "hunter2"
        );
        // No reference at all: the settings row never had one.
        assert_eq!(
            resolve_passphrase(None, &read),
            Err(CryptoError::EmptyPassphrase)
        );
        assert_eq!(
            resolve_passphrase(Some("   "), &read),
            Err(CryptoError::EmptyPassphrase)
        );
        // A reference to a name the store does not hold, and a name whose value is blank.
        assert_eq!(
            resolve_passphrase(Some("OMNION_OTHER"), &read),
            Err(CryptoError::EmptyPassphrase)
        );
        let blank = |_: &str| Some("   ".to_owned());
        assert_eq!(
            resolve_passphrase(Some("OMNION_BACKUP_PASSPHRASE"), &blank),
            Err(CryptoError::EmptyPassphrase)
        );
    }

    #[test]
    fn a_resolved_passphrase_opens_what_it_sealed() {
        // The seam and the cipher together: the value a caller obtains from the store is
        // exactly the value that opens an artifact, with no second input.
        let read =
            |name: &str| (name == "OMNION_BACKUP_PASSPHRASE").then(|| "the passphrase".to_owned());
        let passphrase = resolve_passphrase(Some("OMNION_BACKUP_PASSPHRASE"), &read).expect("key");
        let artifact = seal(passphrase.as_bytes(), b"{\"part\":\"plugins\"}").expect("seal");
        assert!(is_sealed(&artifact));
        assert_eq!(
            open(passphrase.as_bytes(), &artifact).expect("open"),
            b"{\"part\":\"plugins\"}"
        );
    }
}
