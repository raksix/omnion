//! The key ring: the root key hierarchy, the seal self-check and the rotation ceremony.
//!
//! Everything here is pure — it takes keys and envelopes in and returns keys, envelopes and
//! decisions out. The rows live in `secret_root_keys` / `secret_versions`
//! (`database/migrations/0019_secret_hierarchy.sql`) and the job that walks them lives in the
//! API's re-wrap runner; this module only decides *what* has to happen.
//!
//! # The rule that makes rotation safe
//!
//! An envelope carries the `key_id` that sealed it. Unsealing resolves that key from the ring,
//! never the active one. A rotation therefore has three windows, not one:
//!
//! * **before** the flip — new writes seal under the new active key, old versions still name
//!   the old one;
//! * **during** the re-wrap — a consumer resolving a version the job has not reached still
//!   succeeds on the retired key, because the retired key stays in the ring as `retiring`;
//! * **after** the flip — versions still on the old key keep resolving until the job reaches
//!   them, and a completed job has re-wrapped everything.
//!
//! Retiring a key therefore *never* makes an existing version unreadable. That is the invariant
//! the whole design exists to keep, and [`KeyRing::resolve`]/[`KeyRing::retire`] are where it
//! is enforced.

use hmac::{Hmac, Mac};
use rand::RngCore;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::error::{Result, SecretsError};

/// Version tag of the envelope format; a future change writes `v2` and keeps reading `v1`.
const VERSION: &str = "v1";

/// Bytes of nonce; a fresh one per encryption, so identical plaintexts differ on disk.
const NONCE_BYTES: usize = 16;

/// Bytes of a root key: 32 bytes of CSPRNG output, hex-encoded at the boundary.
pub const ROOT_KEY_BYTES: usize = 32;

/// Domain separation for the keystream.
const KEYSTREAM_LABEL: &[u8] = b"omnion.secrets.keystream.v1";

/// Domain separation for the MAC.
const MAC_LABEL: &[u8] = b"omnion.secrets.tag.v1";

/// Domain separation for the fingerprint, so it is not the same digest as a value hint.
const FINGERPRINT_LABEL: &[u8] = b"omnion.secrets.fingerprint.v1";

/// Domain separation for the seal checksum: proves an unwrapped key is the one that sealed.
const SEAL_LABEL: &[u8] = b"omnion.secrets.seal.v1";

/// Environment variable an installation sets to its key-encryption key.
pub const KEY_ENCRYPTION_ENV: &str = "OMNION_KEY_ENCRYPTION_KEY";

/// Environment variable naming a file that holds the key-encryption key — the option for
/// installations that mount the operator key instead of putting it in the process environment.
pub const KEY_ENCRYPTION_FILE_ENV: &str = "OMNION_KEY_ENCRYPTION_KEY_FILE";

/// HMAC-SHA256.
type HmacSha256 = Hmac<Sha256>;

/// The lifecycle state of a root key in the ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStatus {
    /// The key new writes seal under.
    Active,
    /// A rotation is walking its versions; it still resolves reads.
    Retiring,
    /// Fully re-wrapped. Kept for the audit trail, and still resolves reads for a straggler.
    Retired,
}

impl KeyStatus {
    /// The value stored in `secret_root_keys.status`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Retiring => "retiring",
            Self::Retired => "retired",
        }
    }

    /// Parse a stored status.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "active" => Some(Self::Active),
            "retiring" => Some(Self::Retiring),
            "retired" => Some(Self::Retired),
            _ => None,
        }
    }
}

/// The operator's key-encryption key: the material that wraps every root key.
///
/// It is never stored by the platform, never logged and never rendered. It comes from
/// [`KEY_ENCRYPTION_ENV`] or, when that is unset, from the file [`KEY_ENCRYPTION_FILE_ENV`]
/// names.
#[derive(Clone)]
pub struct OperatorKey {
    key: [u8; 32],
    /// `true` when the value came from a file, so the UI can say where it was read from.
    from_file: Option<String>,
}

impl std::fmt::Debug for OperatorKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OperatorKey")
            .field("from_file", &self.from_file)
            .finish_non_exhaustive()
    }
}

impl OperatorKey {
    /// Build an operator key from arbitrary material (the environment's value is hashed down).
    #[must_use]
    pub fn from_material(material: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"omnion.secrets.operator.v1");
        hasher.update(material);
        let digest = hasher.finalize();
        let mut key = [0_u8; 32];
        key.copy_from_slice(&digest);
        Self {
            key,
            from_file: None,
        }
    }

    /// Where the key was read from, for the screen's own words ("read from /etc/…").
    #[must_use]
    pub fn source(&self) -> Option<&str> {
        self.from_file.as_deref()
    }

    /// The key the process should use: [`KEY_ENCRYPTION_ENV`] first, then the file
    /// [`KEY_ENCRYPTION_FILE_ENV`] names. An empty value is treated as absent, because an
    /// operator who exported `KEY=` meant "not set", not "set to the empty string".
    ///
    /// # Errors
    ///
    /// Returns [`SecretsError::OperatorKeyMissing`] when neither source yields a non-empty
    /// value, and refuses an unreadable or empty key file rather than falling through to a
    /// default — a wrong key must fail loudly here, not silently at the first unseal.
    pub fn from_env() -> Result<Self> {
        if let Ok(value) = std::env::var(KEY_ENCRYPTION_ENV)
            && !value.trim().is_empty()
        {
            return Ok(Self::from_material(value.trim().as_bytes()));
        }

        let path = match std::env::var(KEY_ENCRYPTION_FILE_ENV) {
            Ok(path) if !path.trim().is_empty() => path.trim().to_owned(),
            _ => return Err(SecretsError::OperatorKeyMissing),
        };
        let raw = std::fs::read_to_string(&path).map_err(|error| {
            SecretsError::Invalid(format!("the key file could not be read: {error}"))
        })?;
        let value = raw.trim();
        if value.is_empty() {
            return Err(SecretsError::Invalid(format!(
                "the key file {path} is empty"
            )));
        }
        Ok(Self {
            key: Self::from_material(value.as_bytes()).key,
            from_file: Some(path),
        })
    }
}

/// One root key as the ring knows it: the identity, the wrapped material and its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootKey {
    /// The opaque id recorded on every version this key sealed.
    pub key_id: String,
    /// The root key, wrapped by the operator key. Never unwrapped in this struct.
    pub wrapped_key: String,
    /// A checksum over the unwrapped key: the seal self-check's proof.
    pub seal_checksum: String,
    /// The non-reversible fingerprint an operator can compare against their own note.
    pub fingerprint: String,
    /// Where the key is in its lifecycle.
    pub status: KeyStatus,
}

impl RootKey {
    /// A freshly generated root key, wrapped by `operator` and ready to become the active key.
    ///
    /// # Errors
    ///
    /// Returns [`SecretsError::Crypto`] only if the CSPRNG fails, which the `OsRng` fill turns
    /// into a hard panic before it can — the signature keeps the caller honest about failure.
    pub fn generate(operator: &OperatorKey) -> Result<Self> {
        let mut material = [0_u8; ROOT_KEY_BYTES];
        OsRng.fill_bytes(&mut material);
        Self::from_material(&material, operator)
    }

    /// Build a ring key from known material (the ceremony, the tests, the CLI).
    ///
    /// # Errors
    ///
    /// Returns [`SecretsError::Crypto`] if the material is not [`ROOT_KEY_BYTES`] long — a
    /// short key is refused rather than padded, because a padded key is a weaker key that looks
    /// like a strong one.
    pub fn from_material(material: &[u8], operator: &OperatorKey) -> Result<Self> {
        if material.len() != ROOT_KEY_BYTES {
            return Err(SecretsError::Crypto);
        }
        let mut key = [0_u8; ROOT_KEY_BYTES];
        key.copy_from_slice(material);
        let seal_checksum = seal_digest(&key);
        Ok(Self {
            key_id: generate_key_id(),
            wrapped_key: wrap_material(&key, operator),
            seal_checksum,
            fingerprint: fingerprint_of(&key),
            status: KeyStatus::Active,
        })
    }

    /// The unwrapped key material, for the seal self-check and for sealing a value.
    ///
    /// # Errors
    ///
    /// Returns [`SecretsError::Crypto`] when the operator key does not open this envelope.
    pub fn unwrap(&self, operator: &OperatorKey) -> Result<[u8; ROOT_KEY_BYTES]> {
        let plaintext = unseal_envelope(&self.wrapped_key, &operator.key)?;
        if plaintext.len() != ROOT_KEY_BYTES {
            return Err(SecretsError::Crypto);
        }
        let mut key = [0_u8; ROOT_KEY_BYTES];
        key.copy_from_slice(&plaintext);
        Ok(key)
    }
}

/// What a wrap of the operator key produced — the row the ring stores, plus the proof that it
/// opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapOutcome {
    /// The key to store.
    pub key: RootKey,
    /// `true` when the wrapped material unsealed back to the same key under the operator key.
    pub verified: bool,
}

/// A fresh, opaque key id: a UUID without dashes, quoted as a short string.
#[must_use]
pub fn generate_key_id() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Wrap raw key material with the operator key, producing the stored `wrapped_key` envelope.
#[must_use]
pub fn wrap_key(material: &[u8], operator: &OperatorKey) -> String {
    wrap_material(material, operator)
}

/// The outcome of a seal self-check: which keys the operator key opens, and which it does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfCheck {
    /// Every key id the operator key opened, with the recomputed seal checksum.
    pub sealed: Vec<(String, String)>,
    /// The key ids the operator key could **not** open, with the failure's stable code.
    pub unsealed: Vec<(String, &'static str)>,
}

impl SelfCheck {
    /// `true` when at least one key opened and none failed.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        !self.sealed.is_empty() && self.unsealed.is_empty()
    }
}

/// The ring: every root key the installation has, in the state the migration stores.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyRing {
    keys: Vec<RootKey>,
}

impl KeyRing {
    /// An empty ring — a fresh installation that has not generated its first key.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A ring holding the given keys.
    #[must_use]
    pub fn with_keys(keys: Vec<RootKey>) -> Self {
        Self { keys }
    }

    /// Every key, newest first.
    #[must_use]
    pub fn keys(&self) -> &[RootKey] {
        &self.keys
    }

    /// The key new writes seal under.
    #[must_use]
    pub fn active(&self) -> Option<&RootKey> {
        self.keys.iter().find(|key| key.status == KeyStatus::Active)
    }

    /// The key with this id, whatever its state — the lookup a version's `key_id` drives.
    #[must_use]
    pub fn find(&self, key_id: &str) -> Option<&RootKey> {
        self.keys.iter().find(|key| key.key_id == key_id)
    }

    /// Add a key to the ring.
    pub fn push(&mut self, key: RootKey) {
        self.keys.push(key);
    }

    /// Seal a value with the key that carries this id, returning the envelope and that id.
    ///
    /// # Errors
    ///
    /// [`SecretsError::NoActiveKey`] when the version names no key at all,
    /// [`SecretsError::NotFound("root key")`] when it names one the ring does not hold, and
    /// [`SecretsError::Crypto`] when the operator key cannot open that key.
    pub fn seal(&self, key_id: &str, plaintext: &[u8], operator: &OperatorKey) -> Result<String> {
        let key = self
            .find(key_id)
            .ok_or(SecretsError::NotFound("root key"))?;
        let material = key.unwrap(operator)?;
        Ok(seal_envelope(plaintext, &material))
    }

    /// Unseal a version's envelope using the `key_id` the version itself recorded.
    ///
    /// This is the function the rotation invariant rests on: it never consults the active key,
    /// so a version sealed under a retired key keeps resolving.
    ///
    /// # Errors
    ///
    /// As [`Self::seal`], plus [`SecretsError::Crypto`] for an envelope that does not verify.
    pub fn unseal(&self, key_id: &str, envelope: &str, operator: &OperatorKey) -> Result<Vec<u8>> {
        let key = self
            .find(key_id)
            .ok_or(SecretsError::NotFound("root key"))?;
        let material = key.unwrap(operator)?;
        unseal_envelope(envelope, &material)
    }

    /// Re-seal a value from one root key to another — the one operation a re-wrap job performs.
    ///
    /// # Errors
    ///
    /// Fails closed if the plaintext cannot be read under `from_key_id`: the ceremony refuses to
    /// write a value it could not first open, so a bad operator key can never destroy data.
    pub fn rewrap(
        &self,
        from_key_id: &str,
        to_key_id: &str,
        envelope: &str,
        operator: &OperatorKey,
    ) -> Result<String> {
        let plaintext = self.unseal(from_key_id, envelope, operator)?;
        let to_key = self
            .find(to_key_id)
            .ok_or(SecretsError::NotFound("root key"))?;
        let material = to_key.unwrap(operator)?;
        Ok(seal_envelope(&plaintext, &material))
    }

    /// Prove the operator key still opens the ring: every key is unwrapped and its seal
    /// checksum recomputed.
    ///
    /// The rotation wizard calls this first. Losing the operator key makes every locally stored
    /// secret unrecoverable, and this is the check that says so *before* a rotation starts
    /// rather than after.
    #[must_use]
    pub fn self_check(&self, operator: &OperatorKey) -> SelfCheck {
        let mut sealed = Vec::new();
        let mut unsealed = Vec::new();
        for key in &self.keys {
            match key.unwrap(operator) {
                Ok(material) => {
                    let digest = seal_digest(&material);
                    // A key that opens but does not match its own checksum was written by a
                    // different operator key: report it as unsealed, not as healthy.
                    if digest
                        .as_bytes()
                        .ct_eq(key.seal_checksum.as_bytes())
                        .unwrap_u8()
                        == 1
                    {
                        sealed.push((key.key_id.clone(), digest));
                    } else {
                        unsealed.push((key.key_id.clone(), "seal_checksum_mismatch"));
                    }
                }
                Err(error) => unsealed.push((key.key_id.clone(), error.code())),
            }
        }
        SelfCheck { sealed, unsealed }
    }

    /// Mark a key as retiring, as a rotation requires. Refuses to move the *active* key
    /// without a replacement, because an active key that is retiring is a ring with no key new
    /// writes can seal under.
    ///
    /// # Errors
    ///
    /// [`SecretsError::NotFound("root key")`] for an unknown id and [`SecretsError::Invalid`]
    /// when the key is active and no replacement was given.
    pub fn retire(&mut self, key_id: &str, replacement_key_id: &str) -> Result<()> {
        if self.find(replacement_key_id).is_none() {
            return Err(SecretsError::NotFound("root key"));
        }
        let key = self
            .keys
            .iter_mut()
            .find(|key| key.key_id == key_id)
            .ok_or(SecretsError::NotFound("root key"))?;
        if key.status == KeyStatus::Active {
            return Err(SecretsError::Invalid(
                "the active key cannot retire before its replacement is active".to_owned(),
            ));
        }
        key.status = KeyStatus::Retiring;
        Ok(())
    }

    /// Activate a key and retire its predecessor in one step, keeping the predecessor in the
    /// ring as `retiring` so its still-unrewrapped versions keep resolving.
    ///
    /// # Errors
    ///
    /// [`SecretsError::NotFound("root key")`] when either id is unknown. The key statuses
    /// cannot fail otherwise: every state in the migration's check constraint is produced here.
    pub fn activate(&mut self, key_id: &str, previous_key_id: Option<&str>) -> Result<()> {
        if self.find(key_id).is_none() {
            return Err(SecretsError::NotFound("root key"));
        }
        // The predecessor steps down first, so the partial unique index on `active` is never
        // violated even if this function is interrupted between the two writes.
        if let Some(previous) = previous_key_id {
            if let Some(key) = self.keys.iter_mut().find(|key| key.key_id == previous) {
                if key.status == KeyStatus::Active {
                    key.status = KeyStatus::Retiring;
                }
            }
        }
        let key = self
            .keys
            .iter_mut()
            .find(|key| key.key_id == key_id)
            .ok_or(SecretsError::NotFound("root key"))?;
        key.status = KeyStatus::Active;
        Ok(())
    }

    /// Mark a re-wrap job's from-key as finished. The key stays in the ring so a straggler
    /// version still resolves, and the audit trail keeps the row.
    pub fn finish(&mut self, key_id: &str) -> Result<()> {
        let key = self
            .keys
            .iter_mut()
            .find(|key| key.key_id == key_id)
            .ok_or(SecretsError::NotFound("root key"))?;
        key.status = KeyStatus::Retired;
        Ok(())
    }

    /// How many versions are sealed under a key — the number a re-wrap job has to walk.
    #[must_use]
    pub fn coverage(&self, key_id: &str, version_counts: &[(String, i32)]) -> i32 {
        version_counts
            .iter()
            .filter(|(id, _)| id == key_id)
            .map(|(_, count)| *count)
            .sum()
    }
}

/// A domain-separated, hex checksum over key material. The self-check's proof that a key is the
/// one that sealed its own row.
#[must_use]
pub fn seal_digest(material: &[u8; ROOT_KEY_BYTES]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(SEAL_LABEL);
    hasher.update(material);
    hex::encode(hasher.finalize())
}

/// The non-reversible fingerprint an operator compares against their own note.
#[must_use]
pub fn fingerprint_of(material: &[u8; ROOT_KEY_BYTES]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(FINGERPRINT_LABEL);
    hasher.update(material);
    let digest = hasher.finalize();
    format!("omnion-root-{}", hex::encode(&digest[..8]))
}

/// Encrypt `plaintext` into a fresh envelope.
#[must_use]
pub fn seal_envelope(plaintext: &[u8], material: &[u8; ROOT_KEY_BYTES]) -> String {
    let mut nonce = [0_u8; NONCE_BYTES];
    OsRng.fill_bytes(&mut nonce);

    let mut ciphertext = plaintext.to_vec();
    apply_keystream(material, &nonce, &mut ciphertext);

    let tag = envelope_tag(material, &nonce, &ciphertext);
    format!(
        "{VERSION}.{}.{}.{}",
        hex::encode(nonce),
        hex::encode(&ciphertext),
        hex::encode(tag)
    )
}

/// Decrypt an envelope, refusing anything this module did not produce.
pub fn unseal_envelope(envelope: &str, material: &[u8; ROOT_KEY_BYTES]) -> Result<Vec<u8>> {
    let mut parts = envelope.split('.');
    let (Some(version), Some(nonce_hex), Some(cipher_hex), Some(tag_hex), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(SecretsError::Crypto);
    };
    if version != VERSION {
        return Err(SecretsError::Crypto);
    }

    let nonce = hex::decode(nonce_hex).map_err(|_| SecretsError::Crypto)?;
    let mut ciphertext = hex::decode(cipher_hex).map_err(|_| SecretsError::Crypto)?;
    let tag = hex::decode(tag_hex).map_err(|_| SecretsError::Crypto)?;
    if nonce.len() != NONCE_BYTES {
        return Err(SecretsError::Crypto);
    }

    let expected = envelope_tag(material, &nonce, &ciphertext);
    if expected.ct_eq(&tag).unwrap_u8() != 1 {
        return Err(SecretsError::Crypto);
    }

    apply_keystream(material, &nonce, &mut ciphertext);
    Ok(ciphertext)
}

/// Wrap raw key material into a stored envelope (the same construction, with its own key).
#[must_use]
fn wrap_material(material: &[u8], operator: &OperatorKey) -> String {
    seal_envelope(material, &operator.key)
}

/// HMAC-SHA256 over the nonce and ciphertext.
fn envelope_tag(material: &[u8; ROOT_KEY_BYTES], nonce: &[u8], ciphertext: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(material).expect("HMAC accepts any key length");
    mac.update(MAC_LABEL);
    mac.update(nonce);
    mac.update(ciphertext);
    mac.finalize().into_bytes().to_vec()
}

/// XOR `buffer` with the SHA-256 counter-mode keystream of `nonce`.
fn apply_keystream(material: &[u8; ROOT_KEY_BYTES], nonce: &[u8], buffer: &mut [u8]) {
    for (block_index, block) in buffer.chunks_mut(32).enumerate() {
        let mut hasher = Sha256::new();
        hasher.update(material);
        hasher.update(KEYSTREAM_LABEL);
        hasher.update(nonce);
        hasher.update((block_index as u32).to_be_bytes());
        let digest = hasher.finalize();
        for (byte, mask) in block.iter_mut().zip(digest.iter()) {
            *byte ^= mask;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operator() -> OperatorKey {
        OperatorKey::from_material(b"operator-key-for-tests")
    }

    fn other_operator() -> OperatorKey {
        OperatorKey::from_material(b"a-completely-different-operator-key")
    }

    fn material(seed: u8) -> [u8; ROOT_KEY_BYTES] {
        let mut key = [0_u8; ROOT_KEY_BYTES];
        for (index, byte) in key.iter_mut().enumerate() {
            *byte = seed ^ (index as u8);
        }
        key
    }

    #[test]
    fn a_generated_key_is_wrapped_and_unwraps_under_its_operator() {
        let key = RootKey::generate(&operator()).expect("a key is generated");
        let unwrapped = key.unwrap(&operator()).expect("the operator key opens it");
        assert_eq!(unwrapped.len(), ROOT_KEY_BYTES);
        // The wrapped form is an envelope, never the raw bytes.
        assert!(key.wrapped_key.starts_with("v1."));
        assert!(!key.wrapped_key.contains(&hex::encode(unwrapped)));
    }

    #[test]
    fn another_operator_key_cannot_unwrap() {
        let key = RootKey::generate(&operator()).expect("a key is generated");
        assert!(matches!(
            key.unwrap(&other_operator()),
            Err(SecretsError::Crypto)
        ));
    }

    #[test]
    fn short_material_is_refused_rather_than_padded() {
        assert!(matches!(
            RootKey::from_material(&[0_u8; 16], &operator()),
            Err(SecretsError::Crypto)
        ));
    }

    #[test]
    fn sealing_the_same_value_twice_produces_different_envelopes() {
        let key = RootKey::from_material(&material(1), &operator()).expect("a key");
        let ring = KeyRing::with_keys(vec![key]);
        let first = ring
            .seal(
                &ring.active().expect("active").key_id,
                b"value",
                &operator(),
            )
            .expect("sealed");
        let second = ring
            .seal(
                &ring.active().expect("active").key_id,
                b"value",
                &operator(),
            )
            .expect("sealed");
        assert_ne!(first, second, "a fresh nonce per encryption");
    }

    #[test]
    fn a_tampered_envelope_never_decrypts() {
        let key = RootKey::from_material(&material(2), &operator()).expect("a key");
        let key_id = key.key_id.clone();
        let ring = KeyRing::with_keys(vec![key]);
        let envelope = ring.seal(&key_id, b"value", &operator()).expect("sealed");

        let mut tampered = envelope.clone();
        tampered.push('0');
        assert!(ring.unseal(&key_id, &tampered, &operator()).is_err());

        assert!(ring.unseal(&key_id, "v1.zz.zz.zz", &operator()).is_err());
        assert!(
            ring.unseal(&key_id, "not-an-envelope", &operator())
                .is_err()
        );
    }

    #[test]
    fn a_version_sealed_under_a_retired_key_still_resolves() {
        // The invariant the whole rotation design exists for.
        let old = RootKey::from_material(&material(3), &operator()).expect("old key");
        let old_id = old.key_id.clone();
        let new = RootKey::from_material(&material(4), &operator()).expect("new key");
        let new_id = new.key_id.clone();

        let mut ring = KeyRing::with_keys(vec![old, new]);
        let envelope = ring
            .seal(&old_id, b"still readable", &operator())
            .expect("sealed");

        ring.activate(&new_id, Some(&old_id)).expect("flip");
        assert_eq!(
            ring.find(&old_id).expect("still in the ring").status,
            KeyStatus::Retiring
        );

        // Both during and after the ceremony the old version unseals.
        assert_eq!(
            ring.unseal(&old_id, &envelope, &operator())
                .expect("resolves"),
            b"still readable"
        );
        ring.finish(&old_id).expect("retired");
        assert_eq!(
            ring.unseal(&old_id, &envelope, &operator())
                .expect("still resolves"),
            b"still readable"
        );
    }

    #[test]
    fn rewrap_moves_a_value_onto_the_new_key_and_back_off_the_old() {
        let old = RootKey::from_material(&material(5), &operator()).expect("old key");
        let old_id = old.key_id.clone();
        let new = RootKey::from_material(&material(6), &operator()).expect("new key");
        let new_id = new.key_id.clone();
        let ring = KeyRing::with_keys(vec![old, new]);

        let envelope = ring
            .seal(&old_id, b"rotate me", &operator())
            .expect("sealed");
        let moved = ring
            .rewrap(&old_id, &new_id, &envelope, &operator())
            .expect("re-wrapped");

        // The value is unchanged and the new envelope only opens under the new key.
        assert_eq!(
            ring.unseal(&new_id, &moved, &operator()).expect("opens"),
            b"rotate me"
        );
        assert!(ring.unseal(&old_id, &moved, &operator()).is_err());
    }

    #[test]
    fn rewrap_fails_closed_when_the_old_key_cannot_be_opened() {
        let old = RootKey::from_material(&material(7), &operator()).expect("old key");
        let old_id = old.key_id.clone();
        let new = RootKey::from_material(&material(8), &operator()).expect("new key");
        let new_id = new.key_id.clone();
        let ring = KeyRing::with_keys(vec![old, new]);
        let envelope = ring.seal(&old_id, b"value", &operator()).expect("sealed");

        // A wrong operator key must never be able to overwrite the stored envelope.
        assert!(matches!(
            ring.rewrap(&old_id, &new_id, &envelope, &other_operator()),
            Err(SecretsError::Crypto)
        ));
    }

    #[test]
    fn the_self_check_reports_a_healthy_ring_and_a_broken_one() {
        let key = RootKey::generate(&operator()).expect("a key");
        let ring = KeyRing::with_keys(vec![key.clone()]);
        let healthy = ring.self_check(&operator());
        assert!(healthy.is_healthy());
        assert_eq!(healthy.sealed.len(), 1);

        let broken = ring.self_check(&other_operator());
        assert!(!broken.is_healthy());
        assert!(broken.sealed.is_empty());
        assert_eq!(broken.unsealed.len(), 1);
        assert_eq!(broken.unsealed[0].0, key.key_id);
        assert_eq!(broken.unsealed[0].1, "secret_unsealable");
    }

    #[test]
    fn an_empty_ring_refuses_to_seal() {
        let ring = KeyRing::new();
        assert!(ring.active().is_none());
        assert!(ring.seal("", b"value", &operator()).is_err());
    }

    #[test]
    fn the_active_key_cannot_retire_before_its_replacement() {
        let key = RootKey::generate(&operator()).expect("a key");
        let key_id = key.key_id.clone();
        let mut ring = KeyRing::with_keys(vec![key]);
        assert!(matches!(
            ring.retire(&key_id, &key_id),
            Err(SecretsError::Invalid(_))
        ));
        assert!(matches!(
            ring.retire(&key_id, "unknown"),
            Err(SecretsError::NotFound(_))
        ));
    }

    #[test]
    fn an_unknown_key_id_is_refused_on_both_sides() {
        let mut ring = KeyRing::new();
        assert!(ring.find("nope").is_none());
        assert!(matches!(
            ring.seal("nope", b"value", &operator()),
            Err(SecretsError::NotFound("root key"))
        ));
        assert!(matches!(
            ring.activate("nope", None),
            Err(SecretsError::NotFound("root key"))
        ));
    }

    #[test]
    fn coverage_counts_the_versions_still_on_a_key() {
        let ring = KeyRing::new();
        let counts = vec![("key-a".to_owned(), 3), ("key-b".to_owned(), 1)];
        assert_eq!(ring.coverage("key-a", &counts), 3);
        assert_eq!(ring.coverage("key-missing", &counts), 0);
    }

    #[test]
    fn statuses_round_trip_through_the_database_spelling() {
        for status in [KeyStatus::Active, KeyStatus::Retiring, KeyStatus::Retired] {
            assert_eq!(KeyStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(KeyStatus::parse("nonsense"), None);
    }
}
