//! Ceremony tests: a software authenticator runs registration and assertion against the same
//! verification the API calls, so both families (ES256 and EdDSA) are proven end to end
//! here — and every refusal the specification asks for is proven next to it.

use super::*;
use serde_json::json;

use crate::webauthn::cose::{ALG_EDDSA, ALG_ES256};

/// A tiny CBOR writer, so the tests build the blobs an authenticator would send.
fn head(major: u8, length: usize) -> Vec<u8> {
    let mut out = Vec::new();
    match length {
        0..=23 => out.push((major << 5) | u8::try_from(length).expect("small")),
        24..=255 => {
            out.push((major << 5) | 24);
            out.push(u8::try_from(length).expect("one byte"));
        }
        _ => {
            out.push((major << 5) | 25);
            out.push(u8::try_from(length >> 8).expect("two bytes"));
            out.push(u8::try_from(length & 0xff).expect("two bytes"));
        }
    }
    out
}

fn cbor_text(text: &str) -> Vec<u8> {
    let mut out = head(3, text.len());
    out.extend_from_slice(text.as_bytes());
    out
}

fn cbor_bytes(data: &[u8]) -> Vec<u8> {
    let mut out = head(2, data.len());
    out.extend_from_slice(data);
    out
}

fn cbor_uint(value: u64) -> Vec<u8> {
    head(0, usize::try_from(value).expect("small"))
}

fn cbor_neg(value: i64) -> Vec<u8> {
    head(1, usize::try_from(-1 - value).expect("small"))
}

/// A map from already-encoded keys to already-encoded values.
fn cbor_map(entries: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut out = head(5, entries.len());
    for (key, value) in entries {
        out.extend_from_slice(key);
        out.extend_from_slice(value);
    }
    out
}

/// The COSE key for a generated key pair, plus the signer that stands in for the authenticator.
enum Signer {
    /// P-256 (ES256).
    Es256(p256::ecdsa::SigningKey),
    /// Ed25519 (EdDSA).
    Ed25519(Box<ed25519_dalek::SigningKey>),
}

impl Signer {
    fn new_es256() -> Self {
        Signer::Es256(p256::ecdsa::SigningKey::random(&mut rand::rngs::OsRng))
    }

    fn new_ed25519(seed: u8) -> Self {
        Signer::Ed25519(Box::new(ed25519_dalek::SigningKey::from_bytes(&[seed; 32])))
    }

    /// The COSE public key as the attested credential carries it.
    fn cose_key(&self) -> Vec<u8> {
        match self {
            Signer::Es256(signing) => {
                let point = {
                    use p256::elliptic_curve::sec1::ToEncodedPoint;
                    signing.verifying_key().to_encoded_point(false)
                };
                let x = point.x().expect("x").to_vec();
                let y = point.y().expect("y").to_vec();
                cbor_map(&[
                    (cbor_uint(1), cbor_uint(2)),
                    (cbor_uint(3), cbor_neg(ALG_ES256)),
                    (cbor_neg(-1), cbor_uint(1)),
                    (cbor_neg(-2), cbor_bytes(&x)),
                    (cbor_neg(-3), cbor_bytes(&y)),
                ])
            }
            Signer::Ed25519(signing) => {
                let x = signing.verifying_key().to_bytes();
                cbor_map(&[
                    (cbor_uint(1), cbor_uint(1)),
                    (cbor_uint(3), cbor_neg(ALG_EDDSA)),
                    (cbor_neg(-1), cbor_uint(6)),
                    (cbor_neg(-2), cbor_bytes(&x)),
                ])
            }
        }
    }

    /// Sign a message the way an authenticator does.
    fn sign(&self, message: &[u8]) -> Vec<u8> {
        match self {
            Signer::Es256(signing) => {
                use p256::ecdsa::signature::Signer as _;
                let signature: p256::ecdsa::Signature = signing.sign(message);
                signature.to_der().as_bytes().to_vec()
            }
            Signer::Ed25519(signing) => {
                use ed25519_dalek::Signer as _;
                signing.sign(message).to_bytes().to_vec()
            }
        }
    }
}

/// The relying party the tests pretend to be.
const RP_ID: &str = "localhost";

/// Origins the tests accept.
fn origins() -> OriginPolicy {
    OriginPolicy::new(vec!["https://panel.omnion.test".to_owned()], true)
}

/// Authenticator data, with attested credential data when a credential is named.
fn auth_data(
    rp_id: &str,
    flags: u8,
    sign_count: u32,
    credential: Option<(&[u8], &[u8])>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&Sha256::digest(rp_id.as_bytes()));
    let mut flags = flags;
    if credential.is_some() {
        flags |= FLAG_ATTESTED;
    }
    out.push(flags);
    out.extend_from_slice(&sign_count.to_be_bytes());
    if let Some((id, cose)) = credential {
        out.extend_from_slice(&[0; 16]); // aaguid
        out.extend_from_slice(
            &u16::try_from(id.len())
                .expect("a credential id fits two bytes")
                .to_be_bytes(),
        );
        out.extend_from_slice(id);
        out.extend_from_slice(cose);
    }
    out
}

/// The JSON a browser hands the server as `clientDataJSON`.
fn client_data(ceremony: &str, challenge: &str, origin: &str, cross_origin: bool) -> String {
    json!({
        "type": ceremony,
        "challenge": challenge,
        "origin": origin,
        "crossOrigin": cross_origin,
    })
    .to_string()
}

/// An attestation object over the given authenticator data.
fn attestation_object(fmt: &str, auth_data: &[u8], self_signature: Option<Vec<u8>>) -> Vec<u8> {
    let mut statement = cbor_map(&[]);
    if let Some(signature) = self_signature {
        statement = cbor_map(&[
            (cbor_text("alg"), cbor_neg(ALG_ES256)),
            (cbor_text("sig"), cbor_bytes(&signature)),
        ]);
    }
    cbor_map(&[
        (cbor_text("fmt"), cbor_text(fmt)),
        (cbor_text("attStmt"), statement),
        (cbor_text("authData"), cbor_bytes(auth_data)),
    ])
}

/// The base64url the API receives.
fn b64(bytes: &[u8]) -> String {
    encode_b64(bytes)
}

#[test]
fn an_es256_passkey_registers_and_signs_in() {
    let signer = Signer::new_es256();
    let key = signer.cose_key();
    let credential_id = b"credential-id-es256";
    let challenge = new_challenge();

    let registration_data = auth_data(
        RP_ID,
        FLAG_USER_PRESENT | FLAG_USER_VERIFIED,
        0,
        Some((credential_id, &key)),
    );
    let client = client_data(
        "webauthn.create",
        &challenge,
        "http://localhost:3100",
        false,
    );
    let registration = verify_registration(
        &client,
        &b64(&attestation_object("none", &registration_data, None)),
        &challenge,
        RP_ID,
        &origins(),
    )
    .expect("the registration verifies");
    assert_eq!(registration.credential_id, b64(credential_id));
    assert_eq!(registration.public_key, b64(&key));
    assert_eq!(registration.algorithm, "ES256");

    // The sign-in: the signature covers authenticatorData || sha256(clientDataJSON).
    let assertion_data = auth_data(RP_ID, FLAG_USER_PRESENT, 7, None);
    let assertion_challenge = new_challenge();
    let assertion_client = client_data(
        "webauthn.get",
        &assertion_challenge,
        "http://127.0.0.1:3100",
        false,
    );
    let mut message = assertion_data.clone();
    message.extend_from_slice(&Sha256::digest(assertion_client.as_bytes()));
    let signature = signer.sign(&message);

    let assertion = verify_assertion(
        &assertion_client,
        &b64(&assertion_data),
        &b64(&signature),
        &assertion_challenge,
        RP_ID,
        &origins(),
        &registration.public_key,
        i64::from(registration.sign_count),
    )
    .expect("the assertion verifies");
    assert_eq!(assertion.sign_count, 7);
    assert!(assertion.user_verified == false, "UV was not asserted");

    // A replayed counter is refused: the counter must move forward.
    let replayed = verify_assertion(
        &assertion_client,
        &b64(&assertion_data),
        &b64(&signature),
        &assertion_challenge,
        RP_ID,
        &origins(),
        &registration.public_key,
        7,
    );
    assert!(replayed.is_err(), "a counter that does not move is refused");

    // A tampered signature is refused too.
    let mut tampered = signature.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    assert!(
        verify_assertion(
            &assertion_client,
            &b64(&assertion_data),
            &b64(&tampered),
            &assertion_challenge,
            RP_ID,
            &origins(),
            &registration.public_key,
            0,
        )
        .is_err()
    );
}

#[test]
fn an_ed25519_passkey_registers_and_signs_in() {
    let signer = Signer::new_ed25519(9);
    let key = signer.cose_key();
    let credential_id = b"credential-id-eddsa";
    let challenge = new_challenge();
    let registration_data = auth_data(
        RP_ID,
        FLAG_USER_PRESENT | FLAG_USER_VERIFIED,
        0,
        Some((credential_id, &key)),
    );
    let client = client_data(
        "webauthn.create",
        &challenge,
        "https://panel.omnion.test",
        false,
    );

    let registration = verify_registration(
        &client,
        &b64(&attestation_object("none", &registration_data, None)),
        &challenge,
        RP_ID,
        &origins(),
    )
    .expect("the registration verifies");
    assert_eq!(registration.algorithm, "EdDSA");
    assert_eq!(registration.public_key, b64(&key));

    let assertion_data = auth_data(RP_ID, FLAG_USER_PRESENT | FLAG_USER_VERIFIED, 1, None);
    let assertion_challenge = new_challenge();
    let assertion_client = client_data(
        "webauthn.get",
        &assertion_challenge,
        "https://panel.omnion.test",
        false,
    );
    let mut message = assertion_data.clone();
    message.extend_from_slice(&Sha256::digest(assertion_client.as_bytes()));
    let signature = signer.sign(&message);

    let assertion = verify_assertion(
        &assertion_client,
        &b64(&assertion_data),
        &b64(&signature),
        &assertion_challenge,
        RP_ID,
        &origins(),
        &registration.public_key,
        0,
    )
    .expect("the assertion verifies");
    assert_eq!(assertion.sign_count, 1);
    assert!(assertion.user_verified);
}

#[test]
fn a_ceremony_from_another_challenge_origin_or_relying_party_is_refused() {
    let signer = Signer::new_es256();
    let key = signer.cose_key();
    let credential_id = b"credential-id-refusals";
    let challenge = new_challenge();
    let data = auth_data(RP_ID, FLAG_USER_PRESENT, 0, Some((credential_id, &key)));
    let object = b64(&attestation_object("none", &data, None));

    // Another challenge.
    let wrong_challenge = client_data(
        "webauthn.create",
        &new_challenge(),
        "http://localhost:3100",
        false,
    );
    let error = verify_registration(&wrong_challenge, &object, &challenge, RP_ID, &origins())
        .expect_err("a challenge this server did not issue is refused");
    assert!(error.to_string().contains("did not issue"), "{error}");

    // Another origin.
    let wrong_origin = client_data("webauthn.create", &challenge, "https://evil.example", false);
    let error = verify_registration(&wrong_origin, &object, &challenge, RP_ID, &origins())
        .expect_err("a foreign origin is refused");
    assert!(error.to_string().contains("not accepted"), "{error}");

    // A cross-origin ceremony.
    let cross = client_data("webauthn.create", &challenge, "http://localhost:3100", true);
    let error = verify_registration(&cross, &object, &challenge, RP_ID, &origins())
        .expect_err("a cross-origin ceremony is refused");
    assert!(error.to_string().contains("cross-origin"), "{error}");

    // A credential bound to another relying party.
    let other_rp = auth_data(
        "panel.example",
        FLAG_USER_PRESENT,
        0,
        Some((credential_id, &key)),
    );
    let error = verify_registration(
        &client_data(
            "webauthn.create",
            &challenge,
            "http://localhost:3100",
            false,
        ),
        &b64(&attestation_object("none", &other_rp, None)),
        &challenge,
        RP_ID,
        &origins(),
    )
    .expect_err("another relying party's credential is refused");
    assert!(error.to_string().contains("relying party"), "{error}");

    // A registration that does not claim a present user.
    let absent = auth_data(RP_ID, 0x00, 0, Some((credential_id, &key)));
    let error = verify_registration(
        &client_data(
            "webauthn.create",
            &challenge,
            "http://localhost:3100",
            false,
        ),
        &b64(&attestation_object("none", &absent, None)),
        &challenge,
        RP_ID,
        &origins(),
    )
    .expect_err("a missing user-presence flag is refused");
    assert!(error.to_string().contains("user present"), "{error}");

    // The other ceremony's client data (`get` where `create` is expected).
    let swapped = client_data("webauthn.get", &challenge, "http://localhost:3100", false);
    let error = verify_registration(&swapped, &object, &challenge, RP_ID, &origins())
        .expect_err("a swapped ceremony type is refused");
    assert!(error.to_string().contains("webauthn.create"), "{error}");
}

#[test]
fn a_packed_self_attestation_is_verified_and_a_broken_one_is_refused() {
    let signer = Signer::new_es256();
    let key = signer.cose_key();
    let credential_id = b"credential-id-packed";
    let challenge = new_challenge();
    let data = auth_data(RP_ID, FLAG_USER_PRESENT, 0, Some((credential_id, &key)));
    let client = client_data(
        "webauthn.create",
        &challenge,
        "http://localhost:3100",
        false,
    );

    let mut message = data.clone();
    message.extend_from_slice(&Sha256::digest(client.as_bytes()));
    let signature = signer.sign(&message);

    let honest = b64(&attestation_object(
        "packed",
        &data,
        Some(signature.clone()),
    ));
    verify_registration(&client, &honest, &challenge, RP_ID, &origins())
        .expect("a self-attested packed registration verifies");

    let mut broken = signature;
    broken[0] ^= 0x01;
    let tampered = b64(&attestation_object("packed", &data, Some(broken)));
    let error = verify_registration(&client, &tampered, &challenge, RP_ID, &origins())
        .expect_err("a broken self attestation is refused");
    assert!(
        error.to_string().contains("attestation signature"),
        "{error}"
    );
}

#[test]
fn a_counter_less_authenticator_is_allowed_until_a_counter_appears() {
    let signer = Signer::new_ed25519(3);
    let key = signer.cose_key();
    let credential_id = b"credential-id-no-counter";
    let challenge = new_challenge();
    let data = auth_data(RP_ID, FLAG_USER_PRESENT, 0, Some((credential_id, &key)));
    let registration = verify_registration(
        &client_data(
            "webauthn.create",
            &challenge,
            "http://localhost:3100",
            false,
        ),
        &b64(&attestation_object("none", &data, None)),
        &challenge,
        RP_ID,
        &origins(),
    )
    .expect("the registration verifies");
    assert_eq!(registration.sign_count, 0);

    let assertion_data = auth_data(RP_ID, FLAG_USER_PRESENT, 0, None);
    let assertion_challenge = new_challenge();
    let assertion_client = client_data(
        "webauthn.get",
        &assertion_challenge,
        "http://localhost:3100",
        false,
    );
    let mut message = assertion_data.clone();
    message.extend_from_slice(&Sha256::digest(assertion_client.as_bytes()));
    let signature = signer.sign(&message);

    // Stored 0 and reported 0: an authenticator without a counter is accepted.
    verify_assertion(
        &assertion_client,
        &b64(&assertion_data),
        &b64(&signature),
        &assertion_challenge,
        RP_ID,
        &origins(),
        &registration.public_key,
        0,
    )
    .expect("a counter-less passkey signs in");

    // Once a counter has been seen, a zero is a regression.
    let error = verify_assertion(
        &assertion_client,
        &b64(&assertion_data),
        &b64(&signature),
        &assertion_challenge,
        RP_ID,
        &origins(),
        &registration.public_key,
        4,
    )
    .expect_err("a zero after a counter is refused");
    assert!(error.to_string().contains("counter"), "{error}");
}

#[test]
fn the_loopback_exception_is_what_makes_the_qa_stack_possible() {
    // A development installation accepts the QA panel, which is served over plain HTTP on the
    // machine the browser runs on — the documented exception.
    let policy = OriginPolicy::from_env();
    assert!(policy.accepts("http://127.0.0.1:3100"));
    assert!(policy.accepts("http://localhost:3200"));
    assert!(policy.accepts("https://localhost:8443"));
    assert!(!policy.accepts("http://omnion.example"));
    assert!(!policy.accepts("ftp://localhost:3100"));

    // A pinned deployment turns the exception off and must name its own origin.
    let pinned = OriginPolicy::new(vec!["https://panel.omnion.test".to_owned()], false);
    assert!(pinned.accepts("https://panel.omnion.test"));
    assert!(pinned.accepts("https://panel.omnion.test/"));
    assert!(!pinned.accepts("http://127.0.0.1:3100"));
    assert!(!pinned.accepts("https://other.omnion.test"));

    // And with the exception on, a named origin still works.
    let mixed = OriginPolicy::new(vec!["https://panel.omnion.test".to_owned()], true);
    assert!(mixed.accepts("https://panel.omnion.test"));
    assert!(mixed.accepts("http://localhost:3100"));
}

#[test]
fn a_registration_without_credential_data_or_with_bad_client_data_is_refused() {
    let challenge = new_challenge();
    let signer = Signer::new_es256();
    let key = signer.cose_key();

    // No attested credential data (the AT flag is not set).
    let plain = auth_data(RP_ID, FLAG_USER_PRESENT, 0, None);
    let error = verify_registration(
        &client_data(
            "webauthn.create",
            &challenge,
            "http://localhost:3100",
            false,
        ),
        &b64(&attestation_object("none", &plain, None)),
        &challenge,
        RP_ID,
        &origins(),
    )
    .expect_err("a registration with no credential is refused");
    assert!(error.to_string().contains("credential data"), "{error}");

    // Client data that is not JSON at all.
    let error = verify_registration(
        "not json",
        &b64(&attestation_object(
            "none",
            &auth_data(RP_ID, FLAG_USER_PRESENT, 0, Some((b"id", &key))),
            None,
        )),
        &challenge,
        RP_ID,
        &origins(),
    )
    .expect_err("broken client data is refused");
    assert!(error.to_string().contains("not JSON"), "{error}");

    // An attestation object that is not CBOR.
    let error = verify_registration(
        &client_data(
            "webauthn.create",
            &challenge,
            "http://localhost:3100",
            false,
        ),
        &b64(b"this is not cbor"),
        &challenge,
        RP_ID,
        &origins(),
    )
    .expect_err("a broken attestation object is refused");
    assert!(error.to_string().contains("unreadable"), "{error}");
}
