//! The installation's Web Push identity: deriving the public key and signing a JWT (RFC 8292).
//!
//! **This is the half of Web Push that happens before encryption.** A push service will not
//! deliver a request whose `Authorization` header does not carry a JWT signed with the key
//! the browser was told to trust, whose `aud` names the endpoint's host, and whose `sub` is a
//! contact address. Everything else — the encrypted body, the one-time key — is
//! [`crate::push_crypto`]'s problem and is deliberately not here.
//!
//! Three deliberate choices, each of which is a way a hand-rolled implementation goes wrong:
//!
//! * **The signature is `r‖s`, never DER.** A P-256 ECDSA signature in JWS is the fixed-width
//!   64-byte concatenation. `p256`'s `Signature` serializes to DER by default, which is 70–72
//!   bytes of variable length, and a push service that receives one refuses the request. The
//!   `to_bytes()` form is the only correct one and this code says so at the call site.
//! * **`aud` is the endpoint's *origin*, without the path.** The specification requires the
//!   origin. Sending the full endpoint URL is accepted by some services and rejected by
//!   others, and the failure is a `401` on every send with nothing in the outbox to explain it.
//! * **`exp` is short (12 hours, the specification's maximum).** A longer-lived token is
//!   refused by services that check it, and a shorter one costs nothing: the token is built
//!   per send.

use hmac::{Hmac, Mac};
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use sha2::Sha256;

use crate::base64url;

type HmacSha256 = Hmac<Sha256>;

/// The token lifetime, in seconds. Twelve hours is the specification's ceiling; a shorter
/// token is refused by nobody and a longer one is refused by some services.
pub const TOKEN_TTL_SECONDS: u64 = 12 * 60 * 60;

/// How long a generated contact sub-address stays valid (RFC 8292 §2.1 allows a day).
///
/// Only used when the platform mints its own sub for a subscription, which the subscription
/// path does — an operator-supplied contact is long-lived by design.
pub const AUTO_CONTACT_TTL_SECONDS: u64 = 24 * 60 * 60;

/// The installation's key pair, ready to sign.
#[derive(Clone)]
pub struct VapidKeys {
    signing: SigningKey,
    public: String,
}

impl std::fmt::Debug for VapidKeys {
    /// Never renders the private key. A `Debug` that printed it would put a value that can
    /// push to every subscriber into a log line, a panic message or a test failure output.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VapidKeys")
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

impl VapidKeys {
    /// Build from 32 raw private-key bytes.
    ///
    /// `None` when the bytes are not a valid P-256 scalar. That is a real possibility and not
    /// a theoretical one: a key pasted from a hex dump of a 31-byte value, or one whose top
    /// bits are all set, decodes to 32 bytes that are not on the curve.
    #[must_use]
    pub fn from_private_bytes(bytes: &[u8]) -> Option<Self> {
        // Exactly 32 bytes, checked here rather than left to `from_slice`. That function
        // accepts a *shorter* slice and left-pads it, so a paste truncated by one character
        // produced a valid key for a different private scalar: the browser would register
        // against the public key of the padded value, every send would verify against nobody,
        // and the failure would surface days later as push that silently stopped working.
        let signing = SigningKey::from_slice(bytes)
            .ok()
            .filter(|_| bytes.len() == 32)?;
        let public = crate::base64url::encode(&uncompressed_public_point(&signing)?);
        Some(Self { signing, public })
    }

    /// The base64url public key a browser subscribes with — 65 bytes, uncompressed point.
    #[must_use]
    pub fn public_key(&self) -> &str {
        &self.public
    }

    /// Sign one request and return the value for the `Authorization` header.
    ///
    /// `endpoint` is the push service URL; only its origin becomes `aud`, and
    /// `contact` must already be a `mailto:`/`https:` address.
    ///
    /// `now` is passed in rather than read from the clock so the header is testable: a token
    /// whose `exp` is derived from wall time cannot be asserted on without a sleep or a
    /// fixture that tolerates drift.
    #[must_use]
    pub fn authorization(&self, endpoint: &str, contact: &str, now: u64) -> Option<String> {
        let audience = origin_of(endpoint)?;
        let header = base64url::encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = format!(
            r#"{{"aud":"{audience}","exp":{},"sub":"{contact}"}}"#,
            now + TOKEN_TTL_SECONDS
        );
        let signing_input = format!("{header}.{}", base64url::encode(claims.as_bytes()));

        let signature: Signature = self.signing.sign(signing_input.as_bytes());
        // r‖s, 64 bytes. See the module header: DER here is refused by the push service.
        let raw = signature.to_bytes();

        Some(format!(
            "vapid t={signing_input},k={}",
            base64url::encode(raw.as_slice())
        ))
    }

    /// Mint a fresh key pair from the operating system's randomness.
    ///
    /// The one place in the platform that *writes* push key material, and it exists for the
    /// deploy-time step the request describes ("generated at deploy time"). It returns the
    /// private key base64url-encoded so it can be handed to an operator as the value of
    /// `OMNION_PUSH_PRIVATE_KEY` — the platform deliberately does not persist it itself.
    #[must_use]
    pub fn generate() -> Option<(Self, String)> {
        let bytes: [u8; 32] = rand_core_bytes();
        let keys = Self::from_private_bytes(&bytes)?;
        let encoded = base64url::encode(&bytes);
        Some((keys, encoded))
    }
}

/// The uncompressed SEC1 encoding of the public point: `0x04 ‖ X ‖ Y`, 65 bytes.
///
/// The leading `0x04` matters — the browser's `applicationServerKey` is exactly this byte
/// string, and a compressed point (33 bytes) is accepted by no push service.
#[must_use]
pub fn public_key_from_private(private: &[u8]) -> Option<String> {
    let signing = SigningKey::from_slice(private).ok()?;
    Some(base64url::encode(&uncompressed_public_point(&signing)?))
}

/// The uncompressed SEC1 encoding of the public point: `0x04 ‖ X ‖ Y`, 65 bytes.
fn uncompressed_public_point(signing: &SigningKey) -> Option<Vec<u8>> {
    let point = signing.verifying_key().to_encoded_point(false);
    let bytes = point.as_bytes();
    (bytes.len() == 65).then(|| bytes.to_vec())
}

/// The `Origin` of an endpoint URL, or `None` when it has none.
///
/// A hand-rolled split rather than a URL parser, because the only thing this needs to be
/// right about is where the scheme stops and the path begins — and a push endpoint is
/// `https://host/…` by definition. Rejecting anything without a scheme keeps a relative or
/// bare-host string from becoming an `aud` that no service will accept.
fn origin_of(endpoint: &str) -> Option<&str> {
    // The index below is into the string that had the scheme *stripped*, so the prefix length
    // has to come from the split rather than from a literal "://". An earlier version added
    // `"://".len()` to it, which is three characters short of where the host starts — every
    // `aud` came out as `https://fcm.googleapi` and a push service answers 401 to that on
    // every single send, with nothing in the outbox to say why.
    let (scheme, after_scheme) = endpoint.split_once("://")?;
    if scheme.is_empty() || !scheme.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    (end > 0).then(|| &endpoint[..scheme.len() + "://".len() + end])
}

/// 32 bytes from the operating system.
///
/// `getrandom` directly rather than through `rand`: this runs once at deploy time, and a
/// key derived from a seeded generator would be a key that can be reconstructed from the
/// seed.
fn rand_core_bytes() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    // `getrandom` returns an error only when the OS entropy source is broken, in which case
    // a panic at deploy time is the right outcome and returning zeros would be the worst one.
    getrandom::getrandom(&mut bytes)
        .expect("the operating system must provide randomness for a push key");
    bytes
}

/// Derive the deterministic `sub` contact a subscription may be registered under.
///
/// One address per installation, salted by the subscription's endpoint so two installations
/// sharing a mail domain never collide, and with the RFC's own expiry so the contact stops
/// working on its own even if nobody rotates it.
#[must_use]
pub fn auto_contact(endpoint: &str, now: u64) -> String {
    let mut mac = HmacSha256::new_from_slice(b"omnion-vapid-contact").expect("a fixed-length key");
    mac.update(endpoint.as_bytes());
    mac.update(&now.to_be_bytes());
    let hash = mac.finalize().into_bytes();
    let id = hex_of(&hash[..8]);
    format!("mailto:push-{id}@omnion.invalid")
}

/// Lowercase hex, for the contact address only.
///
/// `hex` is a workspace dependency of the platform but is not one of `crates/core`'s, and a
/// contact local-part is not worth a dependency: eight bytes as lowercase hex is four lines.
fn hex_of(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::VerifyingKey;
    use p256::ecdsa::signature::Verifier;

    /// A fixed private key, so a failing assertion names a deterministic key.
    const PRIVATE: [u8; 32] = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e,
        0x1f, 0x20,
    ];

    fn keys() -> VapidKeys {
        VapidKeys::from_private_bytes(&PRIVATE).expect("the fixture key is on the curve")
    }

    /// Split an `Authorization` header value back into its three parts.
    fn parts(header: &str) -> (String, String, String) {
        assert!(header.starts_with("vapid t="), "missing the scheme");
        let rest = header.trim_start_matches("vapid t=");
        let (token, k) = rest.split_once(",k=").expect("missing the key parameter");
        let (header_b64, payload_b64) = token.split_once('.').expect("missing the signature");
        (header_b64.to_owned(), payload_b64.to_owned(), k.to_owned())
    }

    #[test]
    fn the_public_key_is_the_sixty_five_byte_uncompressed_point() {
        let keys = keys();
        let bytes = base64url::decode(keys.public_key()).expect("base64url");
        assert_eq!(bytes.len(), 65, "an uncompressed P-256 point is 65 bytes");
        assert_eq!(bytes[0], 0x04, "the uncompressed point tag is 0x04");
    }

    #[test]
    fn a_configured_private_key_produces_the_key_the_browser_is_told_to_trust() {
        // The two halves must agree, or the service refuses every send while the browser
        // happily registers. This is the whole reason the public key is derived.
        let private = base64url::encode(&PRIVATE);
        let decoded = base64url::decode(&private).expect("base64url");
        let keys = VapidKeys::from_private_bytes(&decoded).expect("valid");
        assert_eq!(
            keys.public_key(),
            base64url::encode(&uncompressed_public_point_for(&keys))
        );
    }

    fn uncompressed_public_point_for(keys: &VapidKeys) -> Vec<u8> {
        base64url::decode(keys.public_key()).expect("base64url")
    }

    #[test]
    fn the_header_carries_a_token_the_public_key_verifies() {
        let keys = keys();
        let header = keys
            .authorization(
                "https://fcm.googleapis.com/fcm/send/abc123",
                "mailto:ops@example.com",
                1_700_000_000,
            )
            .expect("a full URL and a mailto contact both sign");

        let (h, p, k) = parts(&header);
        assert_eq!(
            base64url::decode(&h).as_deref(),
            Some(&br#"{"typ":"JWT","alg":"ES256"}"#[..])
        );

        // Verify with the *public* key — the same thing a push service does.
        let public = base64url::decode(keys.public_key()).expect("base64url");
        let verifying = VerifyingKey::from_sec1_bytes(&public).expect("a point on the curve");
        let signature = Signature::from_slice(&base64url::decode(&k).expect("base64url"))
            .expect("64 raw bytes");
        verifying
            .verify(format!("{h}.{p}").as_bytes(), &signature)
            .expect("the signature must verify against the published public key");

        let claims = String::from_utf8(base64url::decode(&p).expect("base64url")).expect("utf8");
        assert!(
            claims.contains(r#""aud":"https://fcm.googleapis.com""#),
            "{claims}"
        );
        assert!(
            claims.contains(r#""sub":"mailto:ops@example.com""#),
            "{claims}"
        );
    }

    #[test]
    fn the_signature_is_sixty_four_raw_bytes_and_never_der() {
        let keys = keys();
        let header = keys
            .authorization(
                "https://updates.push.services.mozilla.com/wpush/v2/xyz",
                "mailto:a@b.test",
                1,
            )
            .expect("signs");
        let (_, _, k) = parts(&header);
        let raw = base64url::decode(&k).expect("base64url");
        assert_eq!(
            raw.len(),
            64,
            "r‖s is 64 bytes; DER would be 70-72 and refused"
        );
        // A DER signature starts with the SEQUENCE tag 0x30. If that ever appears here the
        // signing path changed back to `to_der()`.
        assert_ne!(raw[0], 0x30, "the signature is not DER-encoded");
    }

    #[test]
    fn aud_is_the_origin_and_never_the_path() {
        let keys = keys();
        for (endpoint, expected) in [
            (
                "https://fcm.googleapis.com/fcm/send/abc123",
                "https://fcm.googleapis.com",
            ),
            (
                "https://updates.push.services.mozilla.com/wpush/v2/xyz",
                "https://updates.push.services.mozilla.com",
            ),
            (
                "https://push.example.net:8443",
                "https://push.example.net:8443",
            ),
        ] {
            let header = keys
                .authorization(endpoint, "mailto:a@b.test", 1)
                .expect("signs");
            let (_, p, _) = parts(&header);
            let claims = String::from_utf8(base64url::decode(&p).expect("b64")).expect("utf8");
            assert!(
                claims.contains(&format!(r#""aud":"{expected}""#)),
                "{endpoint} produced {claims}"
            );
        }
    }

    #[test]
    fn a_relative_or_schemeless_endpoint_is_refused_rather_than_signed() {
        let keys = keys();
        assert!(
            keys.authorization("fcm.googleapis.com/send", "mailto:a@b.test", 1)
                .is_none()
        );
        assert!(
            keys.authorization("/fcm/send/abc", "mailto:a@b.test", 1)
                .is_none()
        );
        assert!(keys.authorization("", "mailto:a@b.test", 1).is_none());
    }

    #[test]
    fn the_token_expires_twelve_hours_out() {
        let keys = keys();
        let now = 1_700_000_000u64;
        let header = keys
            .authorization("https://push.example/x", "mailto:a@b.test", now)
            .expect("signs");
        let (_, p, _) = parts(&header);
        let claims = String::from_utf8(base64url::decode(&p).expect("b64")).expect("utf8");
        // `exp` is a JSON *number*, not a string, so the terminator is the next structural
        // character rather than a closing quote.
        let exp = claims
            .split("\"exp\":")
            .nth(1)
            .map(str::trim)
            .map(|rest| rest.trim_start_matches('"'))
            .and_then(|raw| {
                raw.split([',', '}'])
                    .next()
                    .and_then(|n| n.parse::<u64>().ok())
            })
            .expect("the claims carry a numeric exp");
        assert_eq!(exp, now + 12 * 60 * 60);
    }

    #[test]
    fn the_signature_is_deterministic_and_still_verifies() {
        // **Deterministic, and that is the correct choice for this signature.** RFC 6979
        // derives the nonce from the key and the message, so the same request signed twice
        // gives the same token. The security argument for a randomised ECDSA nonce does not
        // apply here: a push service verifies the token and never learns a signature over a
        // *different* message from the same key, so there is nothing to leak — and the
        // specification's own reference implementations (including the one in the W3C push
        // drafts) are deterministic. An earlier version of this test asserted two signatures
        // differ, and the test was wrong: it was checking a property VAPID must not have.
        let keys = keys();
        let mut seen = Vec::new();
        for _ in 0..2 {
            let header = keys
                .authorization("https://push.example/x", "mailto:a@b.test", 1)
                .expect("signs");
            let (h, p, k) = parts(&header);
            let public = base64url::decode(keys.public_key()).expect("b64");
            let verifying = VerifyingKey::from_sec1_bytes(&public).expect("point");
            let signature =
                Signature::from_slice(&base64url::decode(&k).expect("b64")).expect("raw");
            verifying
                .verify(format!("{h}.{p}").as_bytes(), &signature)
                .expect("verifies");
            seen.push(k);
        }
        assert_eq!(
            seen[0], seen[1],
            "RFC 6979: the same input signs identically"
        );

        // And a *different* timestamp must change it, or the token would be replayable for
        // its whole life with one captured header.
        let other = keys
            .authorization("https://push.example/x", "mailto:a@b.test", 2)
            .expect("signs");
        let (_, _, other_k) = parts(&other);
        assert_ne!(seen[0], other_k, "a different `exp` is a different message");
    }

    #[test]
    fn an_off_curve_private_key_is_refused() {
        // 32 bytes of 0xff is greater than the P-256 group order, so it is not a scalar.
        assert!(VapidKeys::from_private_bytes(&[0xff; 32]).is_none());
        assert!(VapidKeys::from_private_bytes(&[0u8; 32]).is_none());
        assert!(VapidKeys::from_private_bytes(&[1u8; 31]).is_none());
    }

    #[test]
    fn a_generated_pair_is_usable_and_encodes_to_thirty_two_bytes() {
        let (keys, encoded) = VapidKeys::generate().expect("the OS provides randomness");
        assert_eq!(base64url::decode(&encoded).expect("b64").len(), 32);
        assert_eq!(
            VapidKeys::from_private_bytes(&base64url::decode(&encoded).expect("b64"))
                .expect("a generated key round-trips")
                .public_key(),
            keys.public_key()
        );
    }

    #[test]
    fn two_generated_pairs_are_different() {
        let (first, _) = VapidKeys::generate().expect("generates");
        let (second, _) = VapidKeys::generate().expect("generates");
        assert_ne!(first.public_key(), second.public_key());
    }

    #[test]
    fn the_auto_contact_is_per_endpoint_and_carries_the_expiry() {
        let now = 1_700_000_000u64;
        let a = auto_contact("https://push.example/device-a", now);
        let b = auto_contact("https://push.example/device-b", now);
        assert_ne!(a, b, "two endpoints must not share a contact");
        assert_eq!(a, auto_contact("https://push.example/device-a", now));
        assert_ne!(
            a,
            auto_contact(
                "https://push.example/device-a",
                now + AUTO_CONTACT_TTL_SECONDS
            ),
            "a later registration is a different contact"
        );
        assert!(a.starts_with("mailto:push-"), "{a}");
        assert!(a.ends_with("@omnion.invalid"), "{a}");
    }
}
