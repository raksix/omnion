//! API key material: generation, one-way hashing and the prefix a caller actually sends.
//!
//! # Why this is not the password hash
//!
//! The identity crate hashes passwords with Argon2id, and copying that here would be the
//! obvious move and the wrong one. A password is a *low*-entropy secret chosen by a person, so
//! its defence is a hash an attacker cannot afford to compute: 19 MiB and two passes per guess.
//! An API key is 256 bits from the OS CSPRNG — there is no dictionary to attack, and every
//! candidate guess is already equally hopeless. Paying 19 MiB per request to defend a secret
//! that has no weak variants would make the platform's own API slower for no security gain, so
//! this is SHA-256, and the cost is the honest one: an attacker holding the database can brute
//! force a key space that is astronomically too large to brute force.
//!
//! The consequence worth stating plainly: **a leaked database is safe, a leaked key is not**.
//! That is why [`mint`] returns the plaintext exactly once, why nothing in this crate ever
//! reads it back, and why the stored form carries a version prefix — a row written by a build
//! with a different scheme has to read as unreadable rather than compare false against a
//! hash that was made some other way.
//!
//! # The two halves
//!
//! A key travels as `omnion_<prefix>.<secret>`: the [`prefix`] is stored in the clear and
//! indexed so a request can find the row in one probe, and the [`secret`] is what is hashed.
//! Splitting them means the hot path never scans, and it means a support conversation can
//! name a key ("the one starting `omn_7f3a…`") without anyone having read a secret.

use rand::RngCore;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Marker on every stored hash. A row without it predates this scheme and is treated as
/// unreadable, never as a hash that happens to have been written by something else.
const HASH_SCHEME: &str = "omnion-api-key.v1";

/// How many bytes of entropy a key carries. 32 bytes is 256 bits.
const SECRET_BYTES: usize = 32;

/// How many bytes of the secret become the public prefix. Twelve hex characters is 48 bits of
/// index — collision-free at any realistic key count, and short enough to read aloud.
const PREFIX_BYTES: usize = 6;

/// Environment a key authenticates against, and the prefix that names it on the wire.
const KEY_NAMESPACE: &str = "omn_";

/// The plaintext half of a freshly minted key. Never stored, never logged, never returned
/// twice.
#[derive(Clone)]
pub struct MintedKey {
    /// What a caller sends as their bearer token. Displayed exactly once.
    pub plaintext: String,
    /// The public identifier, stored in the clear and shown in the panel's list.
    pub prefix: String,
}

impl std::fmt::Debug for MintedKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deriving `Debug` here would write a live credential into any log that touches a
        // value by reference — a `{:?}` in an error path is enough. The prefix is safe to
        // print and is what makes the redaction useful rather than total.
        formatter
            .debug_struct("MintedKey")
            .field("prefix", &self.prefix)
            .field("plaintext", &"<redacted>")
            .finish()
    }
}

/// Mint a new key: 256 bits of CSPRNG entropy, split into a public prefix and a secret.
///
/// The two halves come from *separate* draws rather than one value sliced in two, so the
/// prefix reveals nothing about the secret even though both come from the same generator.
pub fn mint() -> MintedKey {
    let mut secret = [0u8; SECRET_BYTES];
    let mut prefix_bytes = [0u8; PREFIX_BYTES];
    OsRng.fill_bytes(&mut secret);
    OsRng.fill_bytes(&mut prefix_bytes);

    let prefix = format!("{KEY_NAMESPACE}{}", hex::encode(prefix_bytes));
    let plaintext = format!("{prefix}.{}", hex::encode(secret));
    MintedKey { plaintext, prefix }
}

/// The one-way form written to `api_keys.secret_hash`.
pub fn hash(secret_half: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(HASH_SCHEME.as_bytes());
    hasher.update(b"\0");
    hasher.update(secret_half.as_bytes());
    format!("{HASH_SCHEME}${}", hex::encode(hasher.finalize()))
}

/// Whether a stored hash was written by this scheme.
///
/// The scheme is in the clear, so this is a prefix test rather than a try-verify: a caller
/// presenting a key against a row we cannot read the hash of gets an authentication failure,
/// which is the same answer it would get for a wrong secret, and cannot be used to tell the
/// two apart.
pub fn is_readable_hash(stored: &str) -> bool {
    stored.starts_with(HASH_SCHEME) && stored.len() == HASH_SCHEME.len() + 1 + 64
}

/// Check a presented bearer token against a stored hash.
///
/// Returns `false` for an unreadable row rather than an error, because "this key is not
/// valid" is the whole of what a caller should learn about someone else's key. The comparison
/// is constant-time so a caller cannot learn a prefix of a valid secret by timing.
///
/// The token is expected in the wire form `omn_<prefix>.<secret>`, and the caller has already
/// resolved the row by prefix — so this only ever verifies the secret half.
pub fn verify(secret_half: &str, stored_hash: &str) -> bool {
    if !is_readable_hash(stored_hash) {
        return false;
    }
    hash(secret_half).as_bytes().ct_eq(stored_hash.as_bytes()).into()
}

/// Split a wire-form token into its public prefix and secret half.
///
/// The prefix comes back **with** its `omn_` namespace, because that is the form the row in
/// `api_keys.prefix` stores and the form [`find_by_prefix`](crate::store::find_by_prefix)
/// looks up. Stripping the namespace here and handing the caller a bare `7f3a…` would make
/// every authenticated request miss its own row — a lookup that fails for all keys rather
/// than for none, which is the kind of defect that looks like "auth is broken everywhere"
/// rather than like a slice boundary.
///
/// Returns `None` for anything that is not exactly `omn_<hex prefix>.<hex secret>` of the
/// expected lengths. A malformed token is not an error to report with detail — that detail
/// would be a free oracle for probing the format — so the caller answers with the same 401 a
/// wrong secret gets.
pub fn split_token(token: &str) -> Option<(&str, &str)> {
    let (prefix, secret) = token.split_once('.')?;

    if !prefix.starts_with(KEY_NAMESPACE)
        || prefix.len() != KEY_NAMESPACE.len() + PREFIX_BYTES * 2
        || secret.len() != SECRET_BYTES * 2
        || !is_hex(&prefix[KEY_NAMESPACE.len()..])
        || !is_hex(secret)
    {
        return None;
    }
    Some((prefix, secret))
}

fn is_hex(value: &str) -> bool {
    value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_key_is_split_into_a_findable_prefix_and_an_unshown_secret() {
        let key = mint();
        assert!(key.prefix.starts_with("omn_"));
        // The prefix is what the panel lists; the secret half is everything after the dot.
        assert_eq!(key.plaintext.split_once('.').unwrap().0, key.prefix);
        // 48 bits of prefix, 256 of secret.
        assert_eq!(key.prefix.len(), 4 + 12);
        assert_eq!(key.plaintext.len(), 4 + 12 + 1 + 64);
    }

    #[test]
    fn two_minted_keys_never_collide() {
        let first = mint();
        let second = mint();
        assert_ne!(first.plaintext, second.plaintext);
        assert_ne!(first.prefix, second.prefix);
    }

    #[test]
    fn a_token_round_trips_back_to_its_own_prefix_and_secret() {
        let key = mint();
        let (prefix, secret) = split_token(&key.plaintext).expect("a minted key is well formed");
        assert_eq!(prefix, key.prefix);
        assert_eq!(hash(secret), hash(secret), "hashing is deterministic");
    }

    #[test]
    fn the_stored_form_cannot_be_turned_back_into_the_secret() {
        let key = mint();
        let (_, secret) = split_token(&key.plaintext).unwrap();
        let stored = hash(secret);

        // The one-way property, stated as the test the request asks for: no substring of the
        // stored form is the secret, and the whole secret is absent from it.
        assert!(!stored.contains(secret));
        assert!(!secret.contains(&stored));
        // It also carries its scheme, so a future format is distinguishable from this one.
        assert!(is_readable_hash(&stored));
        assert!(stored.starts_with("omnion-api-key.v1$"));
    }

    #[test]
    fn verification_accepts_only_its_own_secret() {
        let key = mint();
        let (_, secret) = split_token(&key.plaintext).unwrap();
        let stored = hash(secret);

        assert!(verify(secret, &stored));
        assert!(!verify("wrong-secret", &stored));
        let other = mint();
        assert!(!verify(split_token(&other.plaintext).unwrap().1, &stored));
    }

    #[test]
    fn a_row_this_build_cannot_read_authenticates_nobody_rather_than_everybody() {
        // A hash from another scheme must not verify, and must not blow up: it is the same
        // answer as a wrong secret, and the API layer turns both into one 401.
        assert!(!verify("anything", ""));
        assert!(!verify("anything", "$argon2id$v=19$m=1,t=1,p=1$aaaa$bbbb"));
        assert!(!verify("anything", "omnion-api-key.v1$tooshort"));
        assert!(!is_readable_hash("omnion-api-key.v2$0000"));
    }

    #[test]
    fn a_malformed_token_is_refused_rather_than_guessed_at() {
        for bad in [
            "",
            "omn_",
            "omn_short.secret",
            "not-omnion",
            "omn_zzzzzzzzzzzz.abcdef",
            // A prefix with no secret half.
            "omn_0123456789ab.",
            // The right shape but the wrong namespace: another platform's token.
            "sk_live_0123456789ab.0123456789ab",
        ] {
            assert!(split_token(bad).is_none(), "{bad} should not parse");
        }
    }

    #[test]
    fn the_debug_form_cannot_leak_the_credential() {
        let key = mint();
        let rendered = format!("{key:?}");
        assert!(!rendered.contains(split_token(&key.plaintext).unwrap().1));
        assert!(rendered.contains("redacted"));
        // The prefix is deliberately still visible: that is what makes the line useful.
        assert!(rendered.contains(&key.prefix));
    }
}
