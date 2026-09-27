//! Redaction: the single place that decides what a secret looks like when it is *not* being
//! returned to a machine identity.
//!
//! docs/requests/REQ-125 names the risk this closes ("two redaction implementations would
//! drift"), so there is exactly one of each here and every list, detail, audit row and error
//! message goes through it. Three rules, all of them non-negotiable:
//!
//! 1. A value is **never** printed. Not in a list, not in a log line, not in an error, not in a
//!    toast, not in a tooltip.
//! 2. `hint_for` returns a short, stable, non-reversible handle — a hash prefix of the value
//!    with a label — so an operator can recognise *which* value a secret holds ("this is the one
//!    I rotated last week") without the string being readable in a database dump.
//! 3. `mask_value` exists only for the two places a human must recognise a value's shape (a
//!    one-time reveal panel and the REQ-037 secret list); it keeps the first and last two
//!    characters and drops the middle, and it refuses short values outright rather than
//!    echoing most of them.

use sha2::{Digest, Sha256};

/// Domain separation for the redaction hint, so a hint cannot collide with another digest in
/// the codebase by accident.
const HINT_LABEL: &[u8] = b"omnion.secrets.hint.v1";

/// A recognisable, non-reversible handle for a value: `omnh_` plus the first 12 hex of a
/// domain-separated SHA-256. Long enough to compare, far too short to reconstruct.
#[must_use]
pub fn hint_for(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(HINT_LABEL);
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    format!("omnh_{}", hex::encode(&digest[..6]))
}

/// The masked form of a value, or `None` when the value is too short to mask safely.
///
/// The one-time reveal panel shows the real value; every other surface shows this. A value of
/// four characters or fewer is refused rather than echoed — masking `abc` as `ab` would leak
/// most of it.
#[must_use]
pub fn mask_value(value: &str) -> Option<String> {
    let characters: Vec<char> = value.chars().collect();
    if characters.len() < 5 {
        return None;
    }
    let head: String = characters.iter().take(2).collect();
    let tail: String = characters
        .iter()
        .skip(characters.len().saturating_sub(2))
        .collect();
    let stars = characters.len().saturating_sub(4);
    Some(format!("{head}{}{tail}", "*".repeat(stars)))
}

/// Replace every occurrence of a value in a free-text blob with its hint.
///
/// Used for the last line of defence: a validator or a provider that echoes a value back inside
/// an error sentence must not have that sentence stored, logged or rendered verbatim.
#[must_use]
pub fn redact(text: &str, value: &str) -> String {
    if value.is_empty() {
        return text.to_owned();
    }
    text.replace(value, &hint_for(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hint_is_stable_and_hides_the_value() {
        let hint = hint_for("sk-live-1234567890");
        assert_eq!(hint, hint_for("sk-live-1234567890"));
        assert!(hint.starts_with("omnh_"));
        assert!(!hint.contains("sk-live"));
        // Two different values do not share a handle.
        assert_ne!(hint, hint_for("sk-live-0987654321"));
    }

    #[test]
    fn masking_keeps_the_ends_and_drops_the_middle() {
        assert_eq!(
            mask_value("sk-live-1234567890").as_deref(),
            Some("sk**************90")
        );
    }

    #[test]
    fn a_short_value_is_refused_rather_than_echoed() {
        assert_eq!(mask_value("abcd"), None);
        assert_eq!(mask_value(""), None);
    }

    #[test]
    fn redact_replaces_a_value_anywhere_in_a_sentence() {
        let sentence = "provider rejected sk-live-1234567890 twice";
        let redacted = redact(sentence, "sk-live-1234567890");
        assert!(!redacted.contains("1234567890"));
        assert!(redacted.contains("rejected "));
        assert!(redacted.contains("twice"));
    }
}
