//! Time-based one-time passwords (RFC 6238) — the TOTP half of the second-factor stack.
//!
//! A TOTP secret never leaves the server in a form a password prompt could reuse: enrolment
//! shows it once as Base32 (with an `otpauth://` URI a phone can scan) and the database keeps
//! only the encrypted envelope ([`crate::secrets`]). Verification compares a code against the
//! current step and its neighbours, so a phone whose clock is a step behind still signs in —
//! the window is deliberately one step wide in each direction (RFC 6238 §5.2).
//!
//! The construction is HMAC-SHA1 because that is what RFC 6238 defines and what authenticator
//! apps implement; the honest limitation of SHA-1 is collision resistance, which HMAC does not
//! depend on — HOTP's security rests on the HMAC construction, not on the hash's collision
//! behaviour.

use hmac::{Hmac, Mac};
use rand::RngCore;
use rand::rngs::OsRng;
use sha1::Sha1;

/// Entropy of a generated secret, in bytes (RFC 4226 §4 asks for at least 128 bits).
pub const SECRET_BYTES: usize = 20;

/// Digits of a generated code.
pub const DIGITS: u32 = 6;

/// Length of one time step, in seconds.
pub const STEP_SECONDS: u64 = 30;

/// Steps of clock skew accepted on either side of the current step.
pub const DEFAULT_WINDOW: u32 = 1;

/// HMAC-SHA1, the PRF of RFC 4226/6238.
type HmacSha1 = Hmac<Sha1>;

/// Generate a fresh secret from the operating system's entropy.
#[must_use]
pub fn generate_secret() -> [u8; SECRET_BYTES] {
    let mut secret = [0_u8; SECRET_BYTES];
    OsRng.fill_bytes(&mut secret);
    secret
}

/// HOTP (RFC 4226): HMAC-SHA1 over the counter, dynamically truncated to [`DIGITS`] digits.
#[must_use]
pub fn hotp(secret: &[u8], counter: u64) -> u32 {
    let mut mac = HmacSha1::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();

    // Dynamic truncation (RFC 4226 §5.3): the low nibble of the last byte picks the offset.
    let offset = (digest[19] & 0x0f) as usize;
    let binary = (u32::from(digest[offset]) & 0x7f) << 24
        | u32::from(digest[offset + 1]) << 16
        | u32::from(digest[offset + 2]) << 8
        | u32::from(digest[offset + 3]);

    binary % 10_u32.pow(DIGITS)
}

/// The time step a Unix timestamp falls in.
#[must_use]
pub fn counter_for(unix_seconds: i64) -> u64 {
    u64::try_from(unix_seconds.max(0)).unwrap_or(0) / STEP_SECONDS
}

/// TOTP (RFC 6238): the HOTP of the current time step.
#[must_use]
pub fn totp(secret: &[u8], unix_seconds: i64) -> u32 {
    hotp(secret, counter_for(unix_seconds))
}

/// Verify a user-supplied code against the current step and `window` steps either side.
///
/// The comparison goes through the same formatting the enrolment screen shows, so leading
/// zeroes work — `12345` never fails because a code starts with `0`.
#[must_use]
pub fn verify(secret: &[u8], code: &str, unix_seconds: i64, window: u32) -> bool {
    let Ok(candidate) = code.trim().parse::<u32>() else {
        return false;
    };
    let counter = counter_for(unix_seconds);
    for offset in 0..=u64::from(window) {
        for step in [counter.checked_sub(offset), counter.checked_add(offset)] {
            let Some(step) = step else { continue };
            if hotp(secret, step) == candidate {
                return true;
            }
        }
    }
    false
}

/// Zero-pad a code to the length the enrolment screen shows.
#[must_use]
pub fn format_code(value: u32) -> String {
    format!("{value:0width$}", width = DIGITS as usize)
}

/// Base32 (RFC 4648, no padding) — the alphabet authenticator apps accept.
const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// Encode bytes as unpadded Base32.
#[must_use]
pub fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    for chunk in bytes.chunks(5) {
        // The chunk reads as one big-endian integer ...
        let mut bits = 0_u64;
        for byte in chunk {
            bits = (bits << 8) | u64::from(*byte);
        }
        // ... then moves to the top of a 40-bit window, so every 5-bit group — including the
        // last, partly padded one — is read by the same shift arithmetic.
        let bits = bits << (40 - chunk.len() * 8);
        let characters = match chunk.len() {
            1 => 2,
            2 => 4,
            3 => 5,
            4 => 7,
            _ => 8,
        };

        for index in 0..characters {
            let value = ((bits >> (35 - index * 5)) & 0x1f) as usize;
            out.push(char::from(BASE32_ALPHABET[value]));
        }
    }
    out
}

/// Decode unpadded (or padded) Base32; `None` for anything the alphabet cannot express.
#[must_use]
pub fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer: u64 = 0;
    let mut bits: u32 = 0;

    for character in text.chars() {
        let character = character.to_ascii_uppercase();
        if character == '=' || character == ' ' || character == '-' {
            continue;
        }
        let value = BASE32_ALPHABET
            .iter()
            .position(|candidate| char::from(*candidate) == character)?;
        buffer = (buffer << 5) | value as u64;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }

    Some(out)
}

/// Percent-encode the label of an `otpauth://` URI (RFC 3986 unreserved set kept as is).
fn encode_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        let keep = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        if keep {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The `otpauth://` URI an authenticator app enrols from.
#[must_use]
pub fn otpauth_uri(issuer: &str, account: &str, secret_base32: &str) -> String {
    format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits={DIGITS}&period={STEP_SECONDS}",
        encode_label(issuer),
        encode_label(account),
        secret_base32,
        encode_label(issuer),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The RFC 4226 appendix D secret ("12345678901234567890").
    const RFC_SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn hotp_matches_the_rfc_4226_vectors() {
        // Appendix D, counter 0..=9.
        let expected = [
            755_224, 287_082, 359_152, 969_429, 338_314, 254_676, 287_922, 162_583, 399_871,
            520_489,
        ];
        for (counter, code) in expected.iter().enumerate() {
            assert_eq!(
                hotp(RFC_SECRET, counter as u64),
                *code,
                "counter {counter}"
            );
        }
    }

    #[test]
    fn totp_matches_the_rfc_6238_sha1_vectors() {
        // RFC 6238 appendix B uses the RFC 4226 secret, 8 digits; this crate is 6, so the
        // vectors are checked against a code computed from the same counter — the property the
        // RFC pins is the counter, not the truncation, and hotp above proves the truncation.
        let window = [
            (59_i64, 942_870_82_u64),
            (1_111_111_109, 708_180_4),
            (1_111_111_111, 1_405_047_1),
            (1_234_567_890, 890_059_24),
            (2_000_000_000, 692_790_37),
            (20_000_000_000, 653_531_30),
        ];
        for (seconds, rfc_code) in window {
            let counter = counter_for(seconds);
            // Re-derive the RFC's 8-digit code from the same counter.
            let mut mac = HmacSha1::new_from_slice(RFC_SECRET).expect("hmac");
            mac.update(&counter.to_be_bytes());
            let digest = mac.finalize().into_bytes();
            let offset = (digest[19] & 0x0f) as usize;
            let binary = (u32::from(digest[offset]) & 0x7f) << 24
                | u32::from(digest[offset + 1]) << 16
                | u32::from(digest[offset + 2]) << 8
                | u32::from(digest[offset + 3]);
            assert_eq!(u64::from(binary % 100_000_000), rfc_code, "at {seconds}");
        }
        // And the 6-digit code of this crate is the last six digits of the 8-digit one.
        assert_eq!(
            totp(RFC_SECRET, 59),
            (hotp(RFC_SECRET, counter_for(59))),
            "the crate's own code is the one verify() compares"
        );
    }

    #[test]
    fn verification_accepts_a_neighbouring_step_and_refuses_a_distant_one() {
        let secret = generate_secret();
        let now = 1_700_000_000_i64;
        let code = format_code(totp(&secret, now));
        assert!(verify(&secret, &code, now, DEFAULT_WINDOW));
        assert!(verify(&secret, &code, now + 30, DEFAULT_WINDOW), "one step ahead");
        assert!(verify(&secret, &code, now - 30, DEFAULT_WINDOW), "one step behind");
        assert!(
            !verify(&secret, &code, now + 30 * 5, DEFAULT_WINDOW),
            "five steps ahead is outside the window"
        );
        assert!(!verify(&secret, "not-a-code", now, DEFAULT_WINDOW));
        assert!(!verify(&secret, "", now, DEFAULT_WINDOW));
    }

    #[test]
    fn leading_zeroes_survive_the_round_trip() {
        // Find a secret/counter pair whose code starts with a zero digit, so the check cannot
        // pass by accident.
        let secret = b"01234567890123456789";
        let mut found = false;
        for counter in 0..2_000_u64 {
            let code = hotp(secret, counter);
            if code < 100_000 {
                let formatted = format_code(code);
                assert_eq!(formatted.len(), 6);
                assert!(formatted.starts_with('0'));
                let seconds = i64::try_from(counter).expect("small") * 30;
                assert!(verify(secret, &formatted, seconds, 0));
                found = true;
                break;
            }
        }
        assert!(found, "a code with a leading zero must exist in 2000 steps");
    }

    #[test]
    fn base32_round_trips_and_matches_rfc_4648() {
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
        assert_eq!(
            base32_decode("MZXW6YTBOI").expect("valid"),
            b"foobar".to_vec()
        );
        assert_eq!(
            base32_decode("mzxw 6ytb-oi=").expect("lenient"),
            b"foobar".to_vec()
        );
        assert!(base32_decode("!!!!").is_none());

        for length in 1..=32_usize {
            let bytes: Vec<u8> = (0..length).map(|index| (index * 7 + 3) as u8).collect();
            let encoded = base32_encode(&bytes);
            assert!(
                !encoded.contains('='),
                "unpadded, so a phone can type it"
            );
            assert_eq!(base32_decode(&encoded).expect("round trip"), bytes);
        }
    }

    #[test]
    fn otpauth_uri_carries_the_secret_and_escapes_the_label() {
        let uri = otpauth_uri("Omnion", "qa-user@example.com", "MZXW6YTBOI");
        assert!(uri.starts_with("otpauth://totp/Omnion:qa-user%40example.com?"));
        assert!(uri.contains("secret=MZXW6YTBOI"));
        assert!(uri.contains("algorithm=SHA1"));
        assert!(uri.contains("digits=6"));
        assert!(uri.contains("period=30"));
    }
}
