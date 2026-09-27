//! COSE credential public keys and the two signature families a passkey carries.
//!
//! A WebAuthn credential's public key travels as a COSE key (RFC 9052): a small CBOR map with
//! the key type, the algorithm and the curve's own coordinates. Omnion accepts the two
//! algorithms a passkey realistically presents — **ES256** (P-256 ECDSA, `alg = -7`) and
//! **EdDSA** (Ed25519, `alg = -8`, the CurrentCOSE value `-8`) — and refuses everything else
//! with a sentence naming the algorithm, because a key this server cannot verify is not a
//! factor it can accept.
//!
//! Verification never trusts the client for anything but a signature: the message is assembled
//! here from the authenticator data and the client data hash (that is the formula the
//! specification names), and the signature is checked against the stored key.

use super::cbor;

/// COSE label: key type.
const LABEL_KTY: i64 = 1;
/// COSE label: algorithm.
const LABEL_ALG: i64 = 3;
/// COSE label: curve.
const LABEL_CRV: i64 = -1;
/// COSE label: x coordinate (or the raw public key for OKP).
const LABEL_X: i64 = -2;
/// COSE label: y coordinate (EC2 only).
const LABEL_Y: i64 = -3;

/// COSE key type: OKP (RFC 8037).
const KTY_OKP: i64 = 1;
/// COSE key type: EC2 (RFC 9053).
const KTY_EC2: i64 = 2;

/// COSE curve: Ed25519.
const CRV_ED25519: i64 = 6;
/// COSE curve: P-256.
const CRV_P256: i64 = 1;

/// COSE algorithm: ECDSA with SHA-256 on P-256.
pub const ALG_ES256: i64 = -7;
/// COSE algorithm: EdDSA on Ed25519.
pub const ALG_EDDSA: i64 = -8;

/// The public half of a credential, in the shape the ceremony verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialKey {
    /// P-256 ECDSA (ES256).
    Es256 {
        /// x coordinate, 32 bytes.
        x: [u8; 32],
        /// y coordinate, 32 bytes.
        y: [u8; 32],
    },
    /// Ed25519 (EdDSA).
    Ed25519 {
        /// Raw public key, 32 bytes.
        x: [u8; 32],
    },
}

impl CredentialKey {
    /// The COSE algorithm identifier this key signs with.
    #[must_use]
    pub fn algorithm(&self) -> i64 {
        match self {
            CredentialKey::Es256 { .. } => ALG_ES256,
            CredentialKey::Ed25519 { .. } => ALG_EDDSA,
        }
    }

    /// The algorithm as the panel and the audit entry name it.
    #[must_use]
    pub fn algorithm_name(&self) -> &'static str {
        match self {
            CredentialKey::Es256 { .. } => "ES256",
            CredentialKey::Ed25519 { .. } => "EdDSA",
        }
    }
}

/// Parse a COSE credential public key.
///
/// # Errors
/// A sentence naming what is wrong — an unknown key type, an unsupported algorithm, a
/// coordinate of the wrong length.
pub fn parse_key(bytes: &[u8]) -> Result<CredentialKey, String> {
    let item = cbor::decode(bytes)?;

    let kty = item
        .map_get(LABEL_KTY)
        .and_then(cbor::Item::as_integer)
        .ok_or("the COSE key does not name a key type")?;
    let crv = item
        .map_get(LABEL_CRV)
        .and_then(cbor::Item::as_integer)
        .ok_or("the COSE key does not name a curve")?;
    // The algorithm is optional in COSE; when present it must agree with the key material, or
    // the server would verify one thing and record another.
    let alg = item.map_get(LABEL_ALG).and_then(cbor::Item::as_integer);

    let coordinate = |label: i64, what: &str| -> Result<[u8; 32], String> {
        let raw = item
            .map_get(label)
            .and_then(cbor::Item::as_bytes)
            .ok_or_else(|| format!("the COSE key has no {what} coordinate"))?;
        <[u8; 32]>::try_from(raw).map_err(|_| format!("the {what} coordinate is not 32 bytes"))
    };

    match (kty, crv) {
        (KTY_EC2, CRV_P256) => {
            if let Some(alg) = alg {
                if alg != ALG_ES256 {
                    return Err(format!("unsupported COSE algorithm {alg} for a P-256 key"));
                }
            }
            Ok(CredentialKey::Es256 {
                x: coordinate(LABEL_X, "x")?,
                y: coordinate(LABEL_Y, "y")?,
            })
        }
        (KTY_OKP, CRV_ED25519) => {
            if let Some(alg) = alg {
                if alg != ALG_EDDSA {
                    return Err(format!(
                        "unsupported COSE algorithm {alg} for an Ed25519 key"
                    ));
                }
            }
            Ok(CredentialKey::Ed25519 {
                x: coordinate(LABEL_X, "public key")?,
            })
        }
        (kty, crv) => Err(format!(
            "unsupported COSE key: type {kty}, curve {crv} (this server verifies ES256 and EdDSA)"
        )),
    }
}

/// Verify `signature` over `message` with `key`.
///
/// # Errors
/// A sentence the ceremony turns into a refusal; a tampered message, a tampered signature and a
/// key that does not match all land here.
pub fn verify(key: &CredentialKey, message: &[u8], signature: &[u8]) -> Result<(), String> {
    match key {
        CredentialKey::Es256 { x, y } => {
            use p256::ecdsa::signature::Verifier;

            let point = p256::EncodedPoint::from_affine_coordinates(x.into(), y.into(), false);
            let verifying = p256::ecdsa::VerifyingKey::from_encoded_point(&point)
                .map_err(|_| "the stored P-256 key is not a point on the curve".to_owned())?;
            // WebAuthn and COSE carry ECDSA signatures in ASN.1 DER, which is what the
            // `Signature::from_der` path reads; `verify` hashes with SHA-256.
            let signature = p256::ecdsa::Signature::from_der(signature)
                .map_err(|_| "the signature is not DER-encoded ECDSA".to_owned())?;
            verifying
                .verify(message, &signature)
                .map_err(|_| "the signature does not match".to_owned())
        }
        CredentialKey::Ed25519 { x } => {
            let verifying = ed25519_dalek::VerifyingKey::from_bytes(x)
                .map_err(|_| "the stored Ed25519 key is not usable".to_owned())?;
            let signature = ed25519_dalek::Signature::from_slice(signature)
                .map_err(|_| "the signature is not 64 bytes".to_owned())?;
            // Strict verification rejects the small-order and non-canonical keys that allow a
            // signature to verify under more than one key.
            verifying
                .verify_strict(message, &signature)
                .map_err(|_| "the signature does not match".to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the CBOR a P-256 credential key travels in.
    fn es256_key(x: &[u8; 32], y: &[u8; 32]) -> Vec<u8> {
        let mut blob = vec![0xa5];
        blob.extend([0x01, 0x02]); // 1 (kty): 2 (EC2)
        blob.extend([0x03, 0x26]); // 3 (alg): -7
        blob.extend([0x20, 0x01]); // -1 (crv): 1 (P-256)
        blob.extend([0x21, 0x58, 0x20]); // -2 (x): 32 bytes
        blob.extend_from_slice(x);
        blob.extend([0x22, 0x58, 0x20]); // -3 (y): 32 bytes
        blob.extend_from_slice(y);
        blob
    }

    /// Build the CBOR an Ed25519 credential key travels in.
    fn ed25519_key(x: &[u8; 32]) -> Vec<u8> {
        let mut blob = vec![0xa4];
        blob.extend([0x01, 0x01]); // 1 (kty): 1 (OKP)
        blob.extend([0x03, 0x27]); // 3 (alg): -8
        blob.extend([0x20, 0x06]); // -1 (crv): 6 (Ed25519)
        blob.extend([0x21, 0x58, 0x20]); // -2 (x): 32 bytes
        blob.extend_from_slice(x);
        blob
    }

    #[test]
    fn parses_both_families() {
        let key = parse_key(&es256_key(&[7; 32], &[9; 32])).expect("ES256 parses");
        assert_eq!(key.algorithm_name(), "ES256");
        assert_eq!(key.algorithm(), ALG_ES256);

        let key = parse_key(&ed25519_key(&[5; 32])).expect("EdDSA parses");
        assert_eq!(key.algorithm_name(), "EdDSA");
        assert_eq!(key.algorithm(), ALG_EDDSA);
    }

    #[test]
    fn refuses_keys_it_cannot_verify() {
        // kty 3 (a symmetric key) is not a passkey.
        let mut blob = vec![0xa4];
        blob.extend([0x01, 0x03]);
        blob.extend([0x03, 0x26]);
        blob.extend([0x20, 0x01]);
        blob.extend([0x21, 0x58, 0x20]);
        blob.extend_from_slice(&[0; 32]);
        let error = parse_key(&blob).expect_err("a symmetric key is refused");
        assert!(error.contains("unsupported COSE key"), "{error}");

        // A P-256 key whose algorithm is the EdDSA one would be verified with the wrong hash.
        let mut mismatched = vec![0xa5];
        mismatched.extend([0x01, 0x02]);
        mismatched.extend([0x03, 0x27]); // alg: -8 on an EC2 key
        mismatched.extend([0x20, 0x01]);
        mismatched.extend([0x21, 0x58, 0x20]);
        mismatched.extend_from_slice(&[1; 32]);
        mismatched.extend([0x22, 0x58, 0x20]);
        mismatched.extend_from_slice(&[2; 32]);
        let error = parse_key(&mismatched).expect_err("a mismatched algorithm is refused");
        assert!(error.contains("unsupported COSE algorithm"), "{error}");

        // A coordinate of the wrong length is refused rather than padded.
        let mut short = es256_key(&[0; 32], &[0; 32]);
        short.truncate(short.len() - 1);
        assert!(parse_key(&short).is_err());
    }

    #[test]
    fn verifies_an_ed25519_signature_and_refuses_a_tampered_one() {
        use ed25519_dalek::Signer;

        let signing = ed25519_dalek::SigningKey::from_bytes(&[42; 32]);
        let public = signing.verifying_key().to_bytes();
        let key = parse_key(&ed25519_key(&public)).expect("the generated key parses");
        let message = b"authenticator data and a client data hash";
        let signature = signing.sign(message).to_bytes();

        verify(&key, message, &signature).expect("the honest signature verifies");
        assert!(verify(&key, b"another message", &signature).is_err());
        let mut tampered = signature;
        tampered[0] ^= 0x01;
        assert!(verify(&key, message, &tampered).is_err());
    }

    #[test]
    fn verifies_a_p256_signature_and_refuses_a_tampered_one() {
        use p256::ecdsa::signature::Signer;
        use p256::elliptic_curve::sec1::ToEncodedPoint;

        let signing = p256::ecdsa::SigningKey::random(&mut rand::rngs::OsRng);
        let point = signing.verifying_key().to_encoded_point(false);
        let x: [u8; 32] = point.x().expect("x").to_vec().try_into().expect("32 bytes");
        let y: [u8; 32] = point.y().expect("y").to_vec().try_into().expect("32 bytes");
        let key = parse_key(&es256_key(&x, &y)).expect("the generated key parses");

        let message = b"authenticator data and a client data hash";
        let signature: p256::ecdsa::Signature = signing.sign(message);
        verify(&key, message, &signature.to_der().as_bytes())
            .expect("the honest signature verifies");
        assert!(verify(&key, b"another message", &signature.to_der().as_bytes()).is_err());

        let mut tampered = signature.to_der().as_bytes().to_vec();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(verify(&key, message, &tampered).is_err());
    }
}
