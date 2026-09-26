//! The signature scheme a receiver verifies a delivery with.
//!
//! A delivery carries three headers: `X-Omnion-Event` (the name), `X-Omnion-Delivery` (the
//! delivery id, useful for de-duplication) and `X-Omnion-Timestamp`. The body is signed with
//! HMAC-SHA256 over `<timestamp>.<raw body>` and the result travels in `X-Omnion-Signature` as
//! `v1=<hex>`. The timestamp is inside the signed material, so a receiver can refuse a replay
//! by comparing it with its own clock — the same shape Stripe and GitHub use, because it is
//! the shape receiver libraries already implement.
//!
//! The reference receiver is `infra/mocks/webhook-receiver.mjs`; this module is the platform's
//! own verifier and the one the integration suite checks the wire format with.

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Header carrying the signature (`v1=<hex>`).
pub const SIGNATURE_HEADER: &str = "x-omnion-signature";

/// Header carrying the Unix timestamp the signature was computed at.
pub const TIMESTAMP_HEADER: &str = "x-omnion-timestamp";

/// Header carrying the event name.
pub const EVENT_HEADER: &str = "x-omnion-event";

/// Header carrying the delivery id.
pub const DELIVERY_HEADER: &str = "x-omnion-delivery";

/// Version prefix of the signature scheme.
pub const SIGNATURE_VERSION: &str = "v1";

/// The MAC over `timestamp.body` with one secret.
fn mac(secret: &str, timestamp: i64, body: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts a key of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    mac
}

/// The bare signature of one delivery: hex-encoded HMAC-SHA256.
#[must_use]
pub fn sign(secret: &str, timestamp: i64, body: &[u8]) -> String {
    hex::encode(mac(secret, timestamp, body).finalize().into_bytes())
}

/// The value of the `X-Omnion-Signature` header: `v1=<hex>`.
#[must_use]
pub fn signature_header(secret: &str, timestamp: i64, body: &[u8]) -> String {
    format!("{SIGNATURE_VERSION}={}", sign(secret, timestamp, body))
}

/// Verify a `X-Omnion-Signature` header against a body.
///
/// The comparison is constant-time (`verify_slice`), so a receiver — or this function — leaks
/// nothing about how far a wrong signature matched.
#[must_use]
pub fn verify(secret: &str, timestamp: i64, body: &[u8], header: &str) -> bool {
    let signature = header.trim();
    let Some(encoded) = signature
        .strip_prefix(SIGNATURE_VERSION)
        .and_then(|rest| rest.strip_prefix('='))
    else {
        return false;
    };
    let Ok(expected) = hex::decode(encoded) else {
        return false;
    };

    mac(secret, timestamp, body).verify_slice(&expected).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The secret every test signs with.
    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn a_signature_verifies_against_the_exact_body() {
        let body = br#"{"id":1,"name":"page.published"}"#;
        let timestamp = 1_760_000_000_i64;
        let header = signature_header(SECRET, timestamp, body);

        assert!(header.starts_with("v1="), "{header}");
        assert!(verify(SECRET, timestamp, body, &header));
    }

    #[test]
    fn a_changed_body_or_timestamp_breaks_the_signature() {
        let body = br#"{"id":1}"#;
        let timestamp = 1_760_000_000_i64;
        let header = signature_header(SECRET, timestamp, body);

        assert!(!verify(SECRET, timestamp, br#"{"id":2}"#, &header));
        assert!(!verify(SECRET, timestamp + 1, body, &header));
        assert!(
            !verify("another-secret-0123456789", timestamp, body, &header),
            "a different secret must not verify"
        );
    }

    #[test]
    fn malformed_headers_are_refused_without_panicking() {
        let body = b"{}";
        for header in ["", "v2=abcd", "v1=", "v1=zzzz", "abcd", "v1=00ff"] {
            assert!(
                !verify(SECRET, 1_760_000_000, body, header),
                "{header:?} must not verify"
            );
        }
    }

    #[test]
    fn the_signed_material_pins_the_wire_format() {
        // A receiver written against this crate's rule must agree with a receiver written in
        // another language: the expected value is the HMAC over `timestamp.body` bytes.
        let body = b"omnion";
        let timestamp = 42_i64;
        let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).expect("key");
        mac.update(b"42.omnion");
        let expected = hex::encode(mac.finalize().into_bytes());

        assert_eq!(sign(SECRET, timestamp, body), expected);
    }
}
