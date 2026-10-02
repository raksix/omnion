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
//! * **A key is never compared as a key.** The stored value is a SHA-256 digest, so every
//!   comparison here is digest-against-digest and the presented secret goes through
//!   [`hash_key`] first. *That* is what makes this surface safe, and it is a stronger
//!   property than a constant-time comparison: an attacker who recovered the stored digest
//!   byte by byte from a timing side channel would still have to invert SHA-256 to spend it.
//!   [`verify_key`] compares the digests without an early return as well, because a module
//!   that issues credentials and offers a `verify` written as `==` teaches the wrong thing
//!   by example — and the caller who reaches for `if row.endpoint_key == presented` has
//!   removed the hash from the path entirely and made the side channel real.
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
///
/// ## The property that actually protects this surface, and why the obvious reading of the
/// old comment was wrong
///
/// This function was documented as *"Comparison is constant-time — hashes first and then
/// compares the digests, so the check does not leak the stored hash byte by byte"*, and its
/// body was `hash_key(presented) == stored_hash`. **`String == String` short-circuits at
/// the first differing byte**, so the code said the opposite of the comment: load-bearing
/// prose, and a one-line `==` underneath it.
///
/// Two things are true at once, and only one of them is the reason the surface is safe:
///
/// * **What protects this surface is the hash, not the comparison.** The stored value is a
///   SHA-256 digest and `presented` is hashed before anything is compared, so a caller
///   cannot learn the stored value one byte at a time and *spend* it — recovering a digest
///   through a timing signal still leaves the attacker needing to invert SHA-256. The
///   constant-time compare is hygiene on top of that, and claiming it was the thing doing
///   the work is how the next reader ends up storing a clear key somewhere and trusting the
///   comment.
/// * **The `==` still had to go**, because `verify_key` is this crate's only
///   credential-checking verb and its signature invites exactly the mistake that undoes the
///   hash: a caller holding a row writes `row.endpoint_key_hash == hash_key(presented)` and
///   gets a short-circuiting comparison on the stored secret, having skipped the one helper
///   that got it right.
///
/// ## What this function is not for
///
/// **The live public path does not call it, and should not.** `store::find_source_by_key`
/// matches the digest in SQL (`where endpoint_key_hash = $1 and active and kind = $2`), and
/// that is the better design on this surface rather than the compromise: the comparison
/// happens inside the index lookup, so the alternative — select every candidate row and
/// compare in Rust — is a full scan of every tenant's sources on the platform's one
/// deliberately unauthenticated endpoint. `tests/crm_key_lifecycle.rs` drives that path.
///
/// Which leaves this function with **no production caller**, and that is worth saying
/// plainly instead of shipping a helper a future caller might reach for: the honest
/// statement is that a public path authenticating by digest belongs in the query, and the
/// Rust-side verb exists for the paths that compare a secret they have already fetched —
/// none of which this module has yet. A constant-time comparison is not worth inventing a
/// caller for. If a future surface needs it, `subtle::ConstantTimeEq` is the import and
/// `crates/security/src/csrf.rs` plus `crates/permissions/src/service_accounts.rs` are the
/// two places in the platform that already do this by hand.
///
/// Note what this does **not** claim: the length check returns early, and that is correct —
/// the length of a SHA-256 digest is a constant of the algorithm rather than a secret, so
/// there is nothing to hide, and comparing different lengths byte-wise would be a scan with
/// no answer at the end of it.
#[must_use]
pub fn verify_key(presented: &str, stored_hash: &str) -> bool {
    if presented.trim().is_empty() || stored_hash.is_empty() {
        return false;
    }
    use subtle::ConstantTimeEq;
    hash_key(presented)
        .as_bytes()
        .ct_eq(stored_hash.as_bytes())
        .into()
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

    /// The gate for the comparison itself, and the reason it is a test about **the source**
    /// rather than about the function's return value.
    ///
    /// `verify_key` was documented as a constant-time comparison and was `hash_key(..) == stored`,
    /// which short-circuits at the first differing byte. Every behavioural test above passes
    /// on both implementations — a wrong key does not verify either way — so **no test in this
    /// file could ever have caught it.** A security property that is only visible in the
    /// operator's disassembly is a property that needs a source-level assertion, and this is
    /// it.
    ///
    /// It is deliberately a **negative** assertion on the body: the function must not contain
    /// a `==` or an early `return true`. That is the shape of the defect, so it is the shape of
    /// the test — a test that re-asserts the correct answers is decoration, and would still be
    /// green on the broken version this gate exists to fail.
    #[test]
    fn the_verifier_does_not_compare_with_a_short_circuiting_equality() {
        let source = include_str!("keys.rs");

        // Skip the doc comments: this file's own module docs *quote* the old comment verbatim
        // (and quote the `==` with it) precisely so the history stays in the file. Locating
        // the function by a line that also appears in prose is the trap this branch keeps
        // meeting — the slice-21 unit test located a migration statement by substring and
        // matched a comment, then asserted `not contains "unique"` against a line saying
        // `create unique index`. A gate that can be satisfied by its own documentation is
        // not a gate.
        let body = function_body(source, "pub fn verify_key");

        assert!(
            !body.contains("=="),
            "`verify_key` must not compare with `==`: that short-circuits at the first differing \
             byte, which is the exact leak its doc comment promised it did not have. Found: \
             {body}"
        );
        assert!(
            !body.contains("return true"),
            "`verify_key` must not return early on a comparison — an early return is the same \
             leak wearing different clothes. Found: {body}"
        );
        assert!(
            body.contains("ct_eq"),
            "`verify_key` must compare through `subtle`'s `ct_eq`; the helper exists so the \
             platform has one constant-time equality rather than a fourth hand-rolled loop. \
             Found: {body}"
        );
    }

    /// The same property, for the helper's dependency: a hand-rolled `diff |= a ^ b` loop is
    /// what the two other places in the platform do, and the `ct_eq` assertion above would pass
    /// on a file that also carried a fourth copy of it.
    ///
    /// **This assertion failed on its first run and the failure is the lesson.** It read the
    /// whole file — and this file's own doc comments *name* the loop they are replacing, so
    /// `source.contains("diff |=")` matched the prose three lines above. A gate that scans the
    /// text of the file it is written in cannot distinguish "the code does this" from "this
    /// file explains that the code stopped doing this", and the result is a test that must be
    /// weakened every time somebody documents the fix properly. It therefore scans **code
    /// lines only**: doc comments and string literals are stripped first, because the thing
    /// being asserted is a statement, not a mention.
    #[test]
    fn the_crate_does_not_hand_roll_a_fourth_constant_time_loop() {
        let code = code_lines(include_str!("keys.rs"));
        assert!(
            !code.contains("diff |="),
            "this module must use `subtle`, not a fourth transcription of the `diff |= a ^ b` \
             loop already present in crates/security/src/csrf.rs and \
             crates/permissions/src/service_accounts.rs"
        );
    }

    /// The lines of a file that are **code** rather than documentation or message text.
    ///
    /// A `//` or `///` prefix and anything inside a `"` string are removed. This is a
    /// deliberately crude filter and that is the right trade: the alternative — parsing Rust —
    /// is a dependency this module does not need to check its own source, and a crude filter
    /// that errs towards *including* a comment is harmless because every assertion made
    /// against its output is a negative one about a pattern nobody writes in prose except to
    /// say they stopped writing it.
    fn code_lines(source: &str) -> String {
        let mut out = String::with_capacity(source.len());
        for line in source.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            // Strip a trailing line comment, and any `"`-quoted run on the line. A pattern
            // that only ever appears inside a message string is prose in disguise.
            let without_comment = line.split("//").next().unwrap_or(line);
            let mut kept = String::with_capacity(without_comment.len());
            let mut in_string = false;
            for character in without_comment.chars() {
                match character {
                    '"' => {
                        in_string = !in_string;
                        continue;
                    }
                    _ if in_string => continue,
                    other => kept.push(other),
                }
            }
            out.push_str(&kept);
            out.push('\n');
        }
        out
    }

    /// The source of a function, from its `fn` line to the line that closes its brace block.
    ///
    /// Written as a brace counter rather than a regex because a regex over a function body
    /// stops at the first `}` it meets — and a doc comment, a string literal or a nested
    /// block all contain one.
    fn function_body<'a>(source: &'a str, signature: &str) -> &'a str {
        let start = source
            .find(signature)
            .unwrap_or_else(|| panic!("`{signature}` is not in the source at all"))
            + signature.len();
        let rest = &source[start..];
        let open = rest
            .find('{')
            .unwrap_or_else(|| panic!("`{signature}` has no body — it is a declaration"));
        let mut depth = 0_usize;
        for (index, character) in rest[open..].char_indices() {
            match character {
                // Depth goes to 1 at the function's own opening brace and the body ends at the
                // brace that would take it back to 0 — so the closing brace itself is excluded
                // and the returned slice starts *at* the opening one.
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &rest[open..open + index];
                    }
                }
                _ => {}
            }
        }
        panic!("`{signature}` has no closing brace")
    }
}
