//! The encrypted half of Web Push: one body a push service will accept (RFC 8291, RFC 8188).
//!
//! [`crate::vapid`] signs the request. This builds the request's *body*, and the two are a
//! package: a signed request carrying an unencrypted body is refused by every push service,
//! and an encrypted body with no signature is refused too. Nothing about encryption is
//! optional here, which is why it exists rather than being left to the transport.
//!
//! **The payload travels through a service the platform does not control**, so a body that is
//! only "hard to read" is not enough: it is encrypted to the subscriber's public key, and the
//! service holding it in the middle learns nothing about it. That is the reason for the
//! complexity below rather than a base64 blob.
//!
//! ## What this implements, and where each piece comes from
//!
//! | Step | Source |
//! |---|---|
//! | P-256 ECDH against the subscriber's `p256dh` | RFC 8291 §3.1 |
//! | `IKM = HKDF(auth, "WebPush: info" ‖ ua_public ‖ as_public)` | RFC 8291 §3.3 |
//! | `PRK = HKDF(salt, IKM)`, then `CEK` and `NONCE` | RFC 8188 §2.2–2.3 |
//! | `AES-128-GCM` over the body, header as AAD | RFC 8188 §2 |
//! | The `salt ‖ rs ‖ idlen ‖ as_public ‖ ciphertext` framing | RFC 8188 §2 |
//!
//! ## Four ways a hand-rolled version is wrong, and what each one looks like
//!
//! 1. **An ephemeral key per send, not per installation.** A body encrypted to a fixed
//!    `as_public` leaks the fact that two messages went to the same subscriber and lets the
//!    service test whether a body it already holds equals one it has just seen. A fresh
//!    ephemeral pair per body costs one scalar multiplication and removes the question.
//! 2. **`CEK`/`NONCE` derived from the wrong PRK.** RFC 8188 hashes the *salt* into the PRK
//!    before expanding, which is why the salt is random per body and not decoration.
//! 3. **The header as AAD.** RFC 8188 authenticates `salt ‖ rs ‖ idlen ‖ as_public` as
//!    associated data. Encrypting without it produces a body whose framing can be rewritten
//!    in transit without the tag noticing.
//! 4. **A `p256dh` that is a compressed point.** 33 bytes is a valid P-256 point and an
//!    invalid `applicationServerKey`; [`PublicKey::parse`] takes only the 65-byte
//!    uncompressed form so the failure is at registration rather than at every send.
//!
//! `hkdf` is used where the specification says HKDF and `hmac` nowhere: RFC 5869 extract and
//! expand *are* HMAC-SHA-256 in this construction, and writing them out by hand is where the
//! length-prefix bug in #2 above comes from.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, Nonce};
use hkdf::Hkdf;
use p256::ecdh::diffie_hellman;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use sha2::Sha256;

use crate::base64url;

/// The content encoding this module produces. The push service reads it off the request.
pub const CONTENT_ENCODING: &str = "aes128gcm";

/// The record size this module writes.
///
/// RFC 8188 permits any value; the specification recommends one record per body, and a push
/// body is small by construction. A reader on the other end decodes `rs` and would allocate a
/// buffer of this size — so it is a *limit*, not padding.
pub const RECORD_SIZE: u32 = 4096;

/// A subscriber's two public keys, as the browser handed them over.
///
/// Both are base64url without padding, exactly as `PushSubscription.toJSON()` emits them.
/// Decoding is done here rather than at the registration route so that the *only* thing that
/// can accept a malformed key is the code that has to succeed with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriberKeys {
    /// The subscription's signing public key, `p256dh`.
    pub p256dh: String,
    /// The subscription's authentication secret, `auth`.
    pub auth: String,
}

/// What one encrypted body is made of.
///
/// Returned as pieces rather than as a single blob because the body goes into an HTTP request
/// and the `Content-Encoding` header is set from [`Self::content_encoding`]: a caller that
/// concatenated these itself would eventually hard-code the header string next to a
/// construction that had changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedBody {
    /// The framed body: `salt ‖ rs ‖ idlen ‖ as_public ‖ ciphertext‖tag`.
    pub body: Vec<u8>,
}

impl EncryptedBody {
    /// The value for the request's `Content-Encoding` header.
    #[must_use]
    pub fn content_encoding(&self) -> &'static str {
        CONTENT_ENCODING
    }

    /// The body as a lowercase hex string, for a log line that must never carry the plaintext.
    ///
    /// The *length* is the useful part of that line and the bytes are not, so this is a
    /// `{:x?}`-free helper: the outbox stores the size, not the ciphertext.
    #[must_use]
    pub fn len(&self) -> usize {
        self.body.len()
    }

    /// Whether the body is empty. Always false for a real body; present so a caller can use
    /// the `is_empty`/`len` pairing without an `unwrap_or_default`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.body.is_empty()
    }
}

/// Why a body could not be built. Each variant is a sentence the outbox can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CryptoError {
    /// `p256dh` is not a 65-byte uncompressed P-256 point.
    #[error("the subscriber's p256dh key is not a valid uncompressed P-256 point")]
    BadPublicKey,
    /// `auth` is not the 16-byte authentication secret the specification requires.
    #[error("the subscriber's auth secret is {0} bytes; the specification says 16")]
    BadAuthSecret(usize),
    /// The working system's randomness is unavailable.
    #[error("the operating system did not provide randomness for this body")]
    NoRandomness,
    /// The content-encryption step itself failed, which for `Aes128Gcm` is a length error.
    #[error("the body could not be encrypted")]
    Encrypt,
}

/// Encrypt one Web Push payload to one subscriber.
///
/// `plaintext` is what the service worker receives; on this side it is assembled by the caller
/// from the notification's title, body and link, and **the URL is the only field that reaches
/// the device as structured data** — a title and a body are already inside the ciphertext.
#[must_use]
pub fn encrypt(
    keys: &SubscriberKeys,
    plaintext: &[u8],
    salt: [u8; 16],
    ephemeral: &[u8; 32],
) -> Result<EncryptedBody, CryptoError> {
    let decoded = base64url::decode(&keys.p256dh).ok_or(CryptoError::BadPublicKey)?;
    // **A compressed point is a valid P-256 key and an invalid `p256dh`.** `from_sec1_bytes`
    // accepts both — the 33-byte form decodes to the same point as the 65-byte one, so ECDH
    // works and a body is built — but the browser's `applicationServerKey` is by definition
    // the uncompressed encoding, and a subscriber that sent the compressed form is either a
    // non-conforming client or a mistake worth naming at registration rather than at every
    // send. The length is checked here, before the curve check, so the error says which of the
    // two problems it is.
    if decoded.len() != 65 {
        return Err(CryptoError::BadPublicKey);
    }
    let public = PublicKey::from_sec1_bytes(&decoded).map_err(|_| CryptoError::BadPublicKey)?;

    let auth_bytes = base64url::decode(&keys.auth).ok_or(CryptoError::BadAuthSecret(0))?;
    if auth_bytes.len() != 16 {
        return Err(CryptoError::BadAuthSecret(auth_bytes.len()));
    }

    let sender = SecretKey::from_slice(ephemeral).map_err(|_| CryptoError::NoRandomness)?;
    let shared = diffie_hellman(sender.to_nonzero_scalar(), public.as_affine());

    // RFC 8291 §3.3. `WebPush: info\0 ‖ ua_public ‖ as_public` — the order is the
    // subscriber's key first and this side's second, and reversing it produces a body that
    // decrypts to nothing at the device.
    let ua_public = public.to_encoded_point(false);
    let as_public = sender.public_key().to_encoded_point(false);
    let mut info = Vec::with_capacity(12 + ua_public.as_bytes().len() + as_public.as_bytes().len());
    info.extend_from_slice(b"WebPush: info\0");
    info.extend_from_slice(ua_public.as_bytes());
    info.extend_from_slice(as_public.as_bytes());

    // RFC 5869 extract-and-expand with the subscription's auth secret as the PRK. This is the
    // only step that uses `auth`, which is why it cannot be a constant.
    let prk_key = Hkdf::<Sha256>::new(Some(&auth_bytes), shared.raw_secret_bytes().as_slice());
    let mut ikm = [0u8; 32];
    prk_key
        .expand(&info, &mut ikm)
        .map_err(|_| CryptoError::Encrypt)?;

    // RFC 8188 §2.2: the salt is mixed in *before* the content key is expanded out of it.
    let prk = Hkdf::<Sha256>::new(Some(&salt), &ikm);

    let mut cek = [0u8; 16];
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .map_err(|_| CryptoError::Encrypt)?;
    let mut nonce = [0u8; 12];
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
        .map_err(|_| CryptoError::Encrypt)?;

    // The header is authenticated, not encrypted: it is what the device reads to know how to
    // decrypt, so it cannot be inside the ciphertext.
    let header = header(&salt, as_public.as_bytes());
    let cipher = Aes128Gcm::new_from_slice(&cek).map_err(|_| CryptoError::Encrypt)?;
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &header,
            },
        )
        .map_err(|_| CryptoError::Encrypt)?;

    let mut body = Vec::with_capacity(header.len() + ciphertext.len());
    body.extend_from_slice(&header);
    body.extend_from_slice(&ciphertext);
    Ok(EncryptedBody { body })
}

/// Encrypt a payload with a freshly generated ephemeral key and a fresh salt.
///
/// The convenience wrapper the transport uses. Generating both here rather than taking them
/// as parameters is deliberate: the only reason to pass an ephemeral key in is a test, and a
/// production call site that chooses its own ephemeral key is a production call site that can
/// reuse it.
pub fn seal(keys: &SubscriberKeys, plaintext: &[u8]) -> Result<EncryptedBody, CryptoError> {
    encrypt(keys, plaintext, random_16(), &random_32())
}

/// The 86-byte header: `salt ‖ rs ‖ idlen ‖ as_public`.
///
/// `idlen` is one byte and is checked against the actual length, so a future change to the
/// sender's key size cannot produce a body whose framing lies about itself.
fn header(salt: &[u8; 16], as_public: &[u8]) -> Vec<u8> {
    let mut header = Vec::with_capacity(16 + 4 + 1 + as_public.len());
    header.extend_from_slice(salt);
    header.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    header.push(u8::try_from(as_public.len()).unwrap_or(0));
    header.extend_from_slice(as_public);
    header
}

/// Parse a header back into its fields. The counterpart of [`header`], used by the tests and
/// by any reader of these bodies.
///
/// Returns `(salt, record_size, sender_public)` or `None` when the bytes are not a framing
/// this module would have written.
#[must_use]
pub fn parse_header(bytes: &[u8]) -> Option<([u8; 16], u32, Vec<u8>)> {
    if bytes.len() < 21 {
        return None;
    }
    let salt: [u8; 16] = bytes[..16].try_into().ok()?;
    let record_size = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let idlen = usize::from(bytes[20]);
    let sender = bytes.get(21..21 + idlen)?;
    (sender.len() == idlen).then(|| (salt, record_size, sender.to_vec()))
}

/// 16 bytes from the operating system, for the salt.
#[must_use]
pub fn random_16() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("the operating system must provide randomness");
    bytes
}

/// 32 bytes from the operating system, for the ephemeral sender key.
#[must_use]
pub fn random_32() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("the operating system must provide randomness");
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The subscriber's key pair, fixed so a failure names a deterministic subscriber.
    const SUBSCRIBER_PRIVATE: [u8; 32] = [
        0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f,
        0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e,
        0x3f, 0x40,
    ];
    /// 16 bytes, the length the specification fixes for `auth`.
    const SUBSCRIBER_AUTH: [u8; 16] = [
        0xb1, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xbb, 0xbc, 0xbd, 0xbe, 0xbf,
        0xc0,
    ];

    fn subscriber() -> SubscriberKeys {
        let secret = SecretKey::from_slice(&SUBSCRIBER_PRIVATE).expect("on the curve");
        let point = secret.public_key().to_encoded_point(false);
        SubscriberKeys {
            p256dh: base64url::encode(point.as_bytes()),
            auth: base64url::encode(&SUBSCRIBER_AUTH),
        }
    }

    /// Decrypt a body the way a device would, panicking when it does not open.
    ///
    /// The happy-path shorthand over [`try_decrypt_as_subscriber`]. The failure assertions need
    /// the other one: a helper that panicked could not be the thing that proves a tampered body
    /// is refused, since a panic is indistinguishable from any other test failure.
    fn decrypt_as_subscriber(body: &[u8]) -> Vec<u8> {
        try_decrypt_as_subscriber(body)
            .expect("the body this module wrote opens for its own subscriber")
    }

    /// The reader the tests measure against, written from the specification rather than from the
    /// code above — a decryption helper that called `encrypt`'s own helpers would pass a body
    /// built with the wrong key order.
    fn try_decrypt_as_subscriber(body: &[u8]) -> Result<Vec<u8>, aes_gcm::Error> {
        let (salt, _record_size, sender_public) =
            parse_header(body).expect("the body carries a header this module wrote");
        let (encrypted, idlen_len) = body.split_at(21 + sender_public.len());
        let _ = idlen_len;
        let ciphertext = &body[21 + sender_public.len()..];

        let secret = SecretKey::from_slice(&SUBSCRIBER_PRIVATE).expect("on the curve");
        let sender = PublicKey::from_sec1_bytes(&sender_public).expect("an uncompressed point");
        let shared = diffie_hellman(secret.to_nonzero_scalar(), sender.as_affine());

        let ua_public = secret.public_key().to_encoded_point(false);
        let mut info = Vec::new();
        info.extend_from_slice(b"WebPush: info\0");
        info.extend_from_slice(ua_public.as_bytes());
        info.extend_from_slice(sender_public.as_slice());

        let prk_key =
            Hkdf::<Sha256>::new(Some(&SUBSCRIBER_AUTH), shared.raw_secret_bytes().as_slice());
        let mut ikm = [0u8; 32];
        prk_key
            .expand(&info, &mut ikm)
            .expect("32 bytes out of a 32-byte input");

        let prk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
        let mut cek = [0u8; 16];
        prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
            .expect("16 bytes");
        let mut nonce = [0u8; 12];
        prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
            .expect("12 bytes");

        let cipher = Aes128Gcm::new_from_slice(&cek).expect("16 bytes is a key");
        cipher.decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: ciphertext,
                aad: encrypted,
            },
        )
    }

    #[test]
    fn a_sealed_body_opens_for_the_subscriber_it_was_sealed_to() {
        let keys = subscriber();
        let plaintext = br#"{"title":"A page is waiting","body":"Somebody asked for a review."}"#;
        let sealed = seal(&keys, plaintext).expect("the fixture keys are valid");
        assert_eq!(decrypt_as_subscriber(&sealed.body), plaintext);
    }

    #[test]
    fn the_plaintext_is_not_in_the_body_in_the_clear() {
        // The assertion that matters for a service that sees every push body in the platform:
        // the title is a reader's business, and it must not be readable in transit.
        let keys = subscriber();
        let sealed = seal(&keys, b"security: a page is waiting for review")
            .expect("the fixture keys are valid");
        let needle = b"page is waiting";
        assert!(
            !sealed.body.windows(needle.len()).any(|w| w == needle),
            "the plaintext appears in the body"
        );
    }

    #[test]
    fn the_header_carries_the_salt_the_record_size_and_the_sender_key() {
        let keys = subscriber();
        let sealed = seal(&keys, b"hello").expect("the fixture keys are valid");
        let (salt, record_size, sender) = parse_header(&sealed.body).expect("a header");
        assert_eq!(record_size, RECORD_SIZE);
        assert_eq!(
            sender.len(),
            65,
            "the sender key is an uncompressed point, like every other one"
        );
        // 21 bytes of framing + 65 of key + the plaintext + a 16-byte tag.
        assert_eq!(sealed.body.len(), 21 + 65 + 5 + 16);
        assert_ne!(
            salt, [0u8; 16],
            "a zero salt is not a salt; every body must carry its own"
        );
    }

    #[test]
    fn two_bodies_for_one_subscriber_are_not_the_same_bytes() {
        // The property a fresh ephemeral key and a fresh salt buy. Without either, two bodies
        // for the same subscriber are byte-identical, which tells the service exactly that.
        let keys = subscriber();
        let first = seal(&keys, b"same message").expect("sealed");
        let second = seal(&keys, b"same message").expect("sealed");
        assert_ne!(first.body, second.body);
        assert_eq!(decrypt_as_subscriber(&first.body), b"same message");
        assert_eq!(decrypt_as_subscriber(&second.body), b"same message");
    }

    #[test]
    fn a_rewritten_header_is_refused() {
        // The AAD is the whole reason the framing is authenticated: change one byte of the
        // salt in flight and the tag stops verifying at the device.
        let keys = subscriber();
        let sealed = seal(&keys, b"hello").expect("sealed");
        let mut tampered = sealed.body.clone();
        tampered[0] ^= 0xff;
        assert!(
            try_decrypt_as_subscriber(&tampered).is_err(),
            "a rewritten salt must not decrypt"
        );
    }

    #[test]
    fn a_compressed_public_key_is_refused_rather_than_encrypted_to() {
        // A 33-byte point is a perfectly valid P-256 key and an invalid `p256dh`. The
        // alternative is a body that is built successfully and refused by the device.
        let secret = SecretKey::from_slice(&SUBSCRIBER_PRIVATE).expect("on the curve");
        let compressed = secret.public_key().to_encoded_point(true);
        let keys = SubscriberKeys {
            p256dh: base64url::encode(compressed.as_bytes()),
            auth: base64url::encode(&SUBSCRIBER_AUTH),
        };
        assert_eq!(
            seal(&keys, b"hello").expect_err("a compressed point is refused"),
            CryptoError::BadPublicKey
        );
    }

    #[test]
    fn an_auth_secret_of_the_wrong_length_is_named_in_its_own_words() {
        let mut keys = subscriber();
        keys.auth = base64url::encode(&[0u8; 8]);
        assert_eq!(
            seal(&keys, b"hello").expect_err("8 bytes is not 16"),
            CryptoError::BadAuthSecret(8)
        );

        // And the same for something that is not base64url at all.
        keys.auth = "!!! not base64 !!!".to_owned();
        assert_eq!(
            seal(&keys, b"hello").expect_err("not base64url"),
            CryptoError::BadAuthSecret(0)
        );
    }

    #[test]
    fn one_subscriber_cannot_open_another_subscribers_body() {
        let mine = subscriber();
        let other_secret = SecretKey::from_slice(&[0x51; 32]).expect("on the curve");
        let other = SubscriberKeys {
            p256dh: base64url::encode(other_secret.public_key().to_encoded_point(false).as_bytes()),
            auth: base64url::encode(&[0x77; 16]),
        };
        let sealed = seal(&other, b"for somebody else").expect("sealed");
        assert!(try_decrypt_as_subscriber(&sealed.body).is_err());
        // And the body is not merely undecryptable — it is a different body than one sealed to
        // the right key, which is the other half of the property.
        assert_ne!(
            sealed.body,
            seal(&mine, b"for somebody else").expect("sealed").body
        );
    }

    #[test]
    fn an_empty_payload_still_produces_a_well_formed_body() {
        // A notification whose title and body are both empty is legal in this platform, and
        // "no plaintext" must not mean "no body": the device still needs the framing to show
        // something and to report the click.
        let keys = subscriber();
        let sealed = seal(&keys, b"").expect("sealed");
        assert_eq!(sealed.body.len(), 21 + 65 + 16);
        assert_eq!(decrypt_as_subscriber(&sealed.body), b"");
    }

    #[test]
    fn the_body_is_summarised_without_its_bytes() {
        // The `Debug` on the body type is what a `tracing::warn!` would print. It must not be
        // the ciphertext under any circumstance — a log aggregator is a third party too.
        let keys = subscriber();
        let sealed = seal(&keys, b"a reader's private text").expect("sealed");
        let rendered = format!("{sealed:?}");
        assert!(!rendered.contains("private"), "{rendered}");
        assert!(!rendered.contains(&base64url::encode(b"a reader's private text")));
    }

    #[test]
    fn a_truncated_header_is_not_a_header() {
        assert!(parse_header(&[0u8; 20]).is_none());
        assert!(
            parse_header(&[0u8; 21]).is_some(),
            "the framing alone is well formed"
        );
    }
}
