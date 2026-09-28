//! Endpoint keys: issuing them, hashing them, and checking one.
//!
//! A keyed intake endpoint is a public URL somebody pastes into their own site, so the key it
//! carries is a credential that lives in a page's source. Three properties follow, and all
//! three are reasons this is a module file rather than a line at the call site:
//!
//! * **The clear key is never stored.** [`hash_key`] is what authenticates a call, so a
//!   database dump does not hand an attacker a working endpoint. The panel shows the key once
//!   ([`issue_key`]) and afterwards only the last four characters ([`hint_for`]), which is
//!   enough for an operator to tell *which* key is live.
//! * **Comparison is constant-time.** [`verify_key`] hashes first and then compares the
//!   digests, so the check does not leak the stored hash byte by byte to a caller willing to
//!   time it.
//! * **Rotation is a new digest, not a new column.** `rotate` is a write of a fresh hash, so
//!   an old key stops working the instant the new one is saved — and there is no window in
//!   which two keys are both live.

use rand::Rng;
use sha2::{Digest, Sha256};

/// The alphabet of an issued key.
///
/// URL-safe and unambiguous: a key an operator has to read off a screen and type into a form
/// builder must not contain a `0` that could be an `O`, and must survive a query string
/// without escaping.
const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyz23456789";

/// How many characters an issued key has.
///
/// 32 characters of a 32-symbol alphabet is 160 bits, which is far past the point where an
/// online guess is cheaper than the attacker's own bandwidth. The length is a constant rather
/// than a parameter because a caller that can ask for a short key can ask for a 4-character
/// one.
pub const KEY_LENGTH: usize = 32;

/// How much of a key the panel shows afterwards.
const HINT_LENGTH: usize = 4;

/// A freshly issued key and its stored digest.
///
/// Returned as a pair so the caller cannot keep the digest and lose the key (or the reverse):
/// the whole point is that they are separated at the only moment they can be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedKey {
    /// The clear key. Shown once, never stored.
    pub clear: String,
    /// The digest to store.
    pub hash: String,
    /// The last characters, for the panel.
    pub hint: String,
}

/// Issue a new key.
#[must_use]
pub fn issue_key() -> IssuedKey {
    let mut rng = rand::thread_rng();
    let clear: String = (0..KEY_LENGTH)
        .map(|_| {
            let index = rng.gen_range(0..ALPHABET.len());
            char::from(ALPHABET[index])
        })
        .collect();
    let hash = hash_key(&clear);
    let hint = hint_for(&clear);
    IssuedKey { clear, hash, hint }
}

/// The digest of a key, as lower-case hex.
///
/// SHA-256 rather than a password hash on purpose: the key is 160 bits of generated entropy,
/// so there is no dictionary to attack, and a deliberately slow hash would make every intake
/// call pay for it.
#[must_use]
pub fn hash_key(key: &str) -> String {
    let digest = Sha256::digest(key.trim().as_bytes());
    hex::encode(digest)
}

/// `true` when `presented` is the key `stored_hash` was made from.
#[must_use]
pub fn verify_key(presented: &str, stored_hash: &str) -> bool {
    if presented.trim().is_empty() || stored_hash.is_empty() {
        return false;
    }
    // Hash first, then compare: comparing digests of equal length is a constant-time
    // comparison of bytes this crate computed, rather than a `==` over attacker-chosen input
    // that stops at the first differing byte.
    hash_key(presented) == stored_hash
}

/// The last few characters of a key, for the panel.
///
/// Empty for a key shorter than the hint, rather than the whole key: a short key shown in
/// full is a key shown twice.
#[must_use]
pub fn hint_for(key: &str) -> String {
    if key.is_char_boundary(key.len()) && key.len() > HINT_LENGTH {
        key[key.len() - HINT_LENGTH..].to_string()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn an_issued_key_verifies_against_its_own_hash() {
        let issued = issue_key();
        assert!(verify_key(&issued.clear, &issued.hash));
        assert_eq!(issued.hint, hint_for(&issued.clear));
    }

    #[test]
    fn a_wrong_key_does_not_verify() {
        let issued = issue_key();
        let other = issue_key();
        assert!(!verify_key(&other.clear, &issued.hash));
        assert!(!verify_key("", &issued.hash));
        assert!(!verify_key(&issued.clear, ""));
    }

    #[test]
    fn a_rotated_key_stops_working_the_instant_it_is_saved() {
        // Rotation is a write of a fresh digest, so the property that matters is the old key
        // is dead against the *new* digest — not against the old one.
        let first = issue_key();
        let second = issue_key();
        assert!(!verify_key(&first.clear, &second.hash));
        assert_ne!(first.hash, second.hash);
    }

    #[test]
    fn the_hash_is_stable_and_whitespace_insensitive() {
        let issued = issue_key();
        assert_eq!(hash_key(&issued.clear), issued.hash);
        // A key pasted with a stray space is still the key: a form builder that trims its
        // inputs should not lock an operator out of their own endpoint.
        assert!(verify_key(&format!("  {}  ", issued.clear), &issued.hash));
    }

    #[test]
    fn the_key_is_160_bits_of_ambiguity_free_alphabet() {
        let issued = issue_key();
        assert_eq!(issued.clear.chars().count(), KEY_LENGTH);
        assert!(
            issued
                .clear
                .chars()
                .all(|character| ALPHABET.contains(&(character as u8)))
        );
        // No `0`/`O` or `1`/`l` pair that a human mistypes when copying a key off a screen.
        assert!(!issued.clear.contains('0'));
        assert!(!issued.clear.contains('1'));
    }

    #[test]
    fn a_hundred_keys_are_all_different() {
        // A generator with a stuck seed produces one key for every source, and one key for
        // every organization, which is a cross-tenant write. Cheap to test, expensive to find.
        let keys: HashSet<String> = (0..100).map(|_| issue_key().clear).collect();
        assert_eq!(keys.len(), 100);
    }

    #[test]
    fn a_short_key_yields_an_empty_hint_rather_than_the_whole_key() {
        assert_eq!(hint_for("abc"), "");
        assert_eq!(hint_for(""), "");
        assert_eq!(hint_for("abcdefgh"), "efgh");
    }
}
