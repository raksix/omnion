//! WebAuthn (passkeys): the ceremony a browser and an authenticator run, verified here.
//!
//! A passkey is a credential the authenticator holds and the server only ever sees the public
//! half of. The server's whole job is the ceremony: it issues a challenge, the browser and the
//! authenticator sign something around it, and the server checks that **it** issued that
//! challenge, that the signature covers the data it thinks it covers, and that the origin is
//! one this installation serves.
//!
//! # What is verified
//!
//! - **Registration** (`navigator.credentials.create`): the client data says `webauthn.create`
//!   and names the challenge this server issued; the authenticator data hashes to the relying
//!   party id; the user-presence flag is set; the attested credential is extracted together
//!   with its COSE public key ([`super::webauthn::cose`]). When the attestation statement
//!   carries a self signature (`packed` without a certificate chain) that signature is verified
//!   too. Attestation is requested as `none`, so a statement that carries no signature is
//!   accepted as-is — nothing about the authenticator's provenance is claimed, only that the
//!   key it attests to signs.
//! - **Assertion** (`navigator.credentials.get`, the sign-in): the client data says
//!   `webauthn.get` and names the challenge; the signature is verified over
//!   `authenticatorData || SHA-256(clientDataJSON)` — the formula the specification names —
//!   with the stored public key; and the signature counter must move forward, so a replayed
//!   assertion from a cloned authenticator is refused.
//!
//! # The loopback exception (documented on purpose)
//!
//! Browsers treat `http://localhost` and `http://127.0.0.1` as secure contexts, which is what
//! lets the QA stack exercise a real ceremony without TLS. [`OriginPolicy`] therefore accepts a
//! loopback origin from any port while `OMNION_WEBAUTHN_ALLOW_LOOPBACK` is not `false` — the
//! exception is a first-class, documented rule rather than a test that silently skips. A
//! production installation pins `OMNION_WEBAUTHN_ORIGINS` to its own origin and sets the flag
//! to `false`; the RP id (`OMNION_WEBAUTHN_RP_ID`) is the host a credential is bound to, and an
//! assertion that was created for another host never verifies here.
//!
//! Challenges live in `webauthn_challenges`: single-use, purpose-scoped, short-lived, and read
//! back only by the completion call they belong to. They are never logged.

mod cbor;
pub mod cose;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{IdentityError, Result};

/// Environment variable holding the relying party id (the credential's host).
pub const RP_ID_ENV: &str = "OMNION_WEBAUTHN_RP_ID";
/// Environment variable holding the comma-separated origins a ceremony may come from.
pub const ORIGINS_ENV: &str = "OMNION_WEBAUTHN_ORIGINS";
/// Environment variable turning the loopback exception off (`false`).
pub const LOOPBACK_ENV: &str = "OMNION_WEBAUTHN_ALLOW_LOOPBACK";
/// The relying party id a development or QA installation uses.
pub const DEFAULT_RP_ID: &str = "localhost";
/// How long a ceremony challenge stays valid.
pub const CHALLENGE_TTL_MINUTES: i64 = 5;

/// Purpose of a challenge: enrolling a passkey (a signed-in account).
pub const PURPOSE_REGISTRATION: &str = "registration";
/// Purpose of a challenge: proving a passkey at sign-in.
pub const PURPOSE_AUTHENTICATION: &str = "authentication";

/// Authenticator-data flag: user present.
const FLAG_USER_PRESENT: u8 = 0x01;
/// Authenticator-data flag: user verified.
const FLAG_USER_VERIFIED: u8 = 0x04;
/// Authenticator-data flag: attested credential data follows.
const FLAG_ATTESTED: u8 = 0x40;

/// Which origins a ceremony is allowed to come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginPolicy {
    /// Origins accepted verbatim (a scheme, host and port, as the browser reports them).
    pub allowed: Vec<String>,
    /// Whether a loopback origin (`localhost`, `127.0.0.1`, `[::1]`) is accepted from any port.
    /// See the module documentation — this is the documented QA exception.
    pub allow_loopback: bool,
}

impl OriginPolicy {
    /// A policy from its two halves.
    #[must_use]
    pub fn new(allowed: Vec<String>, allow_loopback: bool) -> Self {
        Self {
            allowed: allowed
                .into_iter()
                .map(|origin| origin.trim().trim_end_matches('/').to_owned())
                .filter(|origin| !origin.is_empty())
                .collect(),
            allow_loopback,
        }
    }

    /// The policy the environment asks for.
    ///
    /// `OMNION_WEBAUTHN_ORIGINS` is a comma-separated list; the loopback exception is on unless
    /// `OMNION_WEBAUTHN_ALLOW_LOOPBACK` is `false`.
    #[must_use]
    pub fn from_env() -> Self {
        let allowed = std::env::var(ORIGINS_ENV)
            .ok()
            .map(|value| value.split(',').map(str::to_owned).collect::<Vec<String>>())
            .unwrap_or_default();
        let allow_loopback = std::env::var(LOOPBACK_ENV)
            .map(|value| !value.eq_ignore_ascii_case("false"))
            .unwrap_or(true);
        Self::new(allowed, allow_loopback)
    }

    /// Whether the policy accepts this origin.
    #[must_use]
    pub fn accepts(&self, origin: &str) -> bool {
        let origin = origin.trim().trim_end_matches('/');
        if self.allowed.iter().any(|allowed| allowed == origin) {
            return true;
        }
        self.allow_loopback && is_loopback_origin(origin)
    }
}

/// Whether an origin is served from the machine the browser runs on.
///
/// The port is deliberately free: a development server picks one, and a QA stack starts a new
/// one every run — the host is what makes the origin local.
#[must_use]
pub fn is_loopback_origin(origin: &str) -> bool {
    let Some((scheme, rest)) = origin.split_once("://") else {
        return false;
    };
    if scheme != "http" && scheme != "https" {
        return false;
    }
    let host = rest.split('/').next().unwrap_or_default();
    // Strip a port; an IPv6 literal keeps its brackets.
    let host = match host.rfind(':') {
        Some(index) if !host.ends_with(']') => &host[..index],
        _ => host,
    };
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

/// The relying party id this installation binds credentials to.
#[must_use]
pub fn rp_id_from_env() -> String {
    std::env::var(RP_ID_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_RP_ID.to_owned())
}

/// A fresh challenge: 32 random bytes, base64url, no padding.
#[must_use]
pub fn new_challenge() -> String {
    let mut bytes = [0_u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    B64.encode(bytes)
}

/// Encode bytes the way the ceremony's wire strings are encoded.
#[must_use]
pub fn encode_b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

/// Decode a ceremony wire string.
///
/// # Errors
/// [`IdentityError::WebAuthn`] when the text is not base64url.
pub fn decode_b64(text: &str) -> Result<Vec<u8>> {
    B64.decode(text.trim())
        .map_err(|_| IdentityError::WebAuthn("the request carried invalid base64url".to_owned()))
}

/// The client data a ceremony signs, as this server reads it.
#[derive(Debug, Clone)]
pub struct ClientData {
    /// `webauthn.create` or `webauthn.get`.
    pub ceremony: String,
    /// The challenge the browser saw (base64url, exactly as issued).
    pub challenge: String,
    /// The origin the ceremony ran on.
    pub origin: String,
    /// Whether the ceremony ran in a cross-origin frame.
    pub cross_origin: bool,
}

/// Parse the client data JSON.
///
/// # Errors
/// [`IdentityError::WebAuthn`] when the JSON is not an object or a field is missing.
pub fn parse_client_data(json: &str) -> Result<ClientData> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|_| IdentityError::WebAuthn("the client data is not JSON".to_owned()))?;
    let text = |field: &str| -> Result<String> {
        value
            .get(field)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| IdentityError::WebAuthn(format!("the client data has no {field}")))
    };
    Ok(ClientData {
        ceremony: text("type")?,
        challenge: text("challenge")?,
        origin: text("origin")?,
        cross_origin: value
            .get("crossOrigin")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

/// The authenticator data a ceremony carries.
#[derive(Debug, Clone)]
pub struct AuthenticatorData {
    /// SHA-256 of the relying party id.
    pub rp_id_hash: [u8; 32],
    /// The raw flag byte.
    pub flags: u8,
    /// The signature counter.
    pub sign_count: u32,
    /// The attested credential (registration only).
    pub credential: Option<AttestedCredential>,
}

impl AuthenticatorData {
    /// Whether the user-presence flag is set.
    #[must_use]
    pub fn user_present(&self) -> bool {
        self.flags & FLAG_USER_PRESENT != 0
    }

    /// Whether the user-verification flag is set.
    #[must_use]
    pub fn user_verified(&self) -> bool {
        self.flags & FLAG_USER_VERIFIED != 0
    }
}

/// The credential an authenticator attests to.
#[derive(Debug, Clone)]
pub struct AttestedCredential {
    /// The credential id, as stored (base64url).
    pub credential_id: String,
    /// The COSE public key, as stored (base64url of its own CBOR).
    pub public_key: String,
    /// The parsed key.
    pub key: cose::CredentialKey,
}

/// Parse authenticator data.
///
/// # Errors
/// [`IdentityError::WebAuthn`] when the blob is short, or the attested credential is malformed.
pub fn parse_authenticator_data(bytes: &[u8]) -> Result<AuthenticatorData> {
    if bytes.len() < 37 {
        return Err(IdentityError::WebAuthn(
            "the authenticator data is too short".to_owned(),
        ));
    }
    let mut rp_id_hash = [0_u8; 32];
    rp_id_hash.copy_from_slice(&bytes[..32]);
    let flags = bytes[32];
    let sign_count = u32::from_be_bytes([bytes[33], bytes[34], bytes[35], bytes[36]]);

    let mut credential = None;
    if flags & FLAG_ATTESTED != 0 {
        // aaguid (16) then a 2-byte credential id length, then the id and the COSE key.
        let rest = &bytes[37..];
        if rest.len() < 18 {
            return Err(IdentityError::WebAuthn(
                "the attested credential data is too short".to_owned(),
            ));
        }
        let id_length = usize::from(u16::from_be_bytes([rest[16], rest[17]]));
        let id_start = 18;
        let id_end = id_start + id_length;
        if rest.len() < id_end {
            return Err(IdentityError::WebAuthn(
                "the credential id is shorter than its length says".to_owned(),
            ));
        }
        let credential_id = &rest[id_start..id_end];

        // The COSE key is itself CBOR; it is decoded here to know where it ends, and stored
        // verbatim so a later assertion verifies against exactly the bytes the authenticator
        // signed for.
        let key_bytes = &rest[id_end..];
        let (_, key_length) = cbor::decode_prefix(key_bytes).map_err(|error| {
            IdentityError::WebAuthn(format!("the COSE key is not readable: {error}"))
        })?;
        let key_bytes = &key_bytes[..key_length];

        credential = Some(AttestedCredential {
            credential_id: encode_b64(credential_id),
            public_key: encode_b64(key_bytes),
            key: cose::parse_key(key_bytes).map_err(IdentityError::WebAuthn)?,
        });
    }

    Ok(AuthenticatorData {
        rp_id_hash,
        flags,
        sign_count,
        credential,
    })
}

/// What a verified registration hands back.
#[derive(Debug, Clone)]
pub struct Registration {
    /// Credential id (base64url).
    pub credential_id: String,
    /// COSE public key (base64url of its own CBOR).
    pub public_key: String,
    /// The algorithm, as the panel names it.
    pub algorithm: String,
    /// The counter the authenticator reported.
    pub sign_count: u32,
}

/// Verify a registration ceremony.
///
/// # Errors
/// [`IdentityError::WebAuthn`] with a sentence naming the first thing that did not hold.
pub fn verify_registration(
    client_data_json: &str,
    attestation_object_b64: &str,
    expected_challenge: &str,
    rp_id: &str,
    origins: &OriginPolicy,
) -> Result<Registration> {
    let client = parse_client_data(client_data_json)?;
    check_client_data(&client, "webauthn.create", expected_challenge, origins)?;

    let attestation = decode_b64(attestation_object_b64)?;
    let attestation = cbor::decode(&attestation).map_err(|error| {
        IdentityError::WebAuthn(format!("the attestation object is unreadable: {error}"))
    })?;
    let fmt = attestation
        .map_get_text("fmt")
        .and_then(cbor::Item::as_text)
        .unwrap_or("none")
        .to_owned();
    let raw_auth_data = attestation
        .map_get_text("authData")
        .and_then(cbor::Item::as_bytes)
        .ok_or_else(|| {
            IdentityError::WebAuthn(
                "the attestation object carries no authenticator data".to_owned(),
            )
        })?;
    let auth_data = parse_authenticator_data(raw_auth_data)?;
    if !auth_data.user_present() {
        return Err(IdentityError::WebAuthn(
            "the authenticator did not report a user present".to_owned(),
        ));
    }
    check_rp_id_hash(&auth_data.rp_id_hash, rp_id)?;

    let credential = auth_data.credential.ok_or_else(|| {
        IdentityError::WebAuthn("the attestation carries no credential data".to_owned())
    })?;

    // A packed statement without a certificate chain is a self attestation: the credential's
    // own key signs `authenticatorData || clientDataHash`. When it is present it is verified;
    // the `none` format (what this server requests) carries nothing to verify.
    if fmt == "packed" {
        let has_chain = attestation
            .map_get_text("attStmt")
            .and_then(|statement| statement.map_get_text("x5c"))
            .is_some();
        if !has_chain {
            let statement = attestation.map_get_text("attStmt").ok_or_else(|| {
                IdentityError::WebAuthn("the packed statement is missing".to_owned())
            })?;
            let signature = statement
                .map_get_text("sig")
                .and_then(cbor::Item::as_bytes)
                .ok_or_else(|| {
                    IdentityError::WebAuthn(
                        "the packed statement carries no self signature".to_owned(),
                    )
                })?;
            let message = registration_message(raw_auth_data, client_data_json);
            cose::verify(&credential.key, &message, signature).map_err(|error| {
                IdentityError::WebAuthn(format!("the attestation signature was refused: {error}"))
            })?;
        }
    }

    Ok(Registration {
        credential_id: credential.credential_id,
        public_key: credential.public_key,
        algorithm: credential.key.algorithm_name().to_owned(),
        sign_count: auth_data.sign_count,
    })
}

/// What a verified assertion hands back.
#[derive(Debug, Clone)]
pub struct Assertion {
    /// The counter the authenticator reported (stored so the next one must be larger).
    pub sign_count: u32,
    /// Whether the authenticator reported a verified user.
    pub user_verified: bool,
}

/// Verify an assertion (the sign-in ceremony).
///
/// `stored_sign_count` is what the factor row holds; a counter that does not move forward is
/// refused — that is the cloned-authenticator check. Authenticators that do not implement a
/// counter report `0` forever, which is allowed as long as the stored value is `0` too.
///
/// # Errors
/// [`IdentityError::WebAuthn`] with a sentence naming the first thing that did not hold.
pub fn verify_assertion(
    client_data_json: &str,
    authenticator_data_b64: &str,
    signature_b64: &str,
    expected_challenge: &str,
    rp_id: &str,
    origins: &OriginPolicy,
    stored_public_key: &str,
    stored_sign_count: i64,
) -> Result<Assertion> {
    let client = parse_client_data(client_data_json)?;
    check_client_data(&client, "webauthn.get", expected_challenge, origins)?;

    let authenticator_data = decode_b64(authenticator_data_b64)?;
    let parsed = parse_authenticator_data(&authenticator_data)?;
    if !parsed.user_present() {
        return Err(IdentityError::WebAuthn(
            "the authenticator did not report a user present".to_owned(),
        ));
    }
    check_rp_id_hash(&parsed.rp_id_hash, rp_id)?;

    let signature = decode_b64(signature_b64)?;
    let key_bytes = decode_b64(stored_public_key)?;
    let key = cose::parse_key(&key_bytes).map_err(IdentityError::WebAuthn)?;

    let mut message = authenticator_data.clone();
    message.extend_from_slice(&Sha256::digest(client_data_json.as_bytes()));
    cose::verify(&key, &message, &signature).map_err(|error| {
        IdentityError::WebAuthn(format!("the assertion signature was refused: {error}"))
    })?;

    let stored = u32::try_from(stored_sign_count).unwrap_or(0);
    if parsed.sign_count == 0 && stored == 0 {
        // A counter-less authenticator: nothing to compare, and nothing to update.
    } else if parsed.sign_count <= stored {
        return Err(IdentityError::WebAuthn(
            "the signature counter did not move forward — the assertion may be a replay".to_owned(),
        ));
    }

    Ok(Assertion {
        sign_count: parsed.sign_count,
        user_verified: parsed.user_verified(),
    })
}

/// The shared client-data checks: ceremony, challenge, origin.
fn check_client_data(
    client: &ClientData,
    ceremony: &str,
    expected_challenge: &str,
    origins: &OriginPolicy,
) -> Result<()> {
    if client.ceremony != ceremony {
        return Err(IdentityError::WebAuthn(format!(
            "this is a {} ceremony, not {ceremony}",
            client.ceremony
        )));
    }
    if client.challenge != expected_challenge {
        return Err(IdentityError::WebAuthn(
            "the ceremony answered a challenge this server did not issue".to_owned(),
        ));
    }
    if client.cross_origin {
        return Err(IdentityError::WebAuthn(
            "a cross-origin ceremony is not accepted".to_owned(),
        ));
    }
    if !origins.accepts(&client.origin) {
        return Err(IdentityError::WebAuthn(format!(
            "the ceremony ran on {} — that origin is not accepted here",
            client.origin
        )));
    }
    Ok(())
}

/// The relying party id hash must be the one for this installation.
fn check_rp_id_hash(hash: &[u8; 32], rp_id: &str) -> Result<()> {
    if hash.as_slice() != Sha256::digest(rp_id.as_bytes()).as_slice() {
        return Err(IdentityError::WebAuthn(format!(
            "this credential belongs to another relying party than {rp_id}"
        )));
    }
    Ok(())
}

/// `authenticatorData || SHA-256(clientDataJSON)` — the message an attestation signature covers.
fn registration_message(auth_data: &[u8], client_data_json: &str) -> Vec<u8> {
    let mut message = auth_data.to_vec();
    message.extend_from_slice(&Sha256::digest(client_data_json.as_bytes()));
    message
}

// ---------------------------------------------------------------------------------------------
// Challenges
// ---------------------------------------------------------------------------------------------

/// One live ceremony challenge.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Challenge {
    /// Primary key.
    pub id: Uuid,
    /// Owning account.
    pub user_id: Uuid,
    /// `registration` or `authentication`.
    pub purpose: String,
    /// The base64url challenge as issued.
    pub challenge: String,
}

/// Issue a ceremony challenge and store it (the value is answered to the client once).
///
/// One live challenge per purpose: asking again retires the previous one, so an abandoned
/// dialog cannot leave a usable challenge behind.
pub async fn create_challenge(
    pool: &PgPool,
    user_id: Uuid,
    purpose: &str,
    rp_id: &str,
) -> Result<String> {
    let challenge = new_challenge();
    let expires_at = OffsetDateTime::now_utc() + time::Duration::minutes(CHALLENGE_TTL_MINUTES);

    sqlx::query(
        "update webauthn_challenges set consumed_at = now() \
         where user_id = $1 and purpose = $2 and consumed_at is null",
    )
    .bind(user_id)
    .bind(purpose)
    .execute(pool)
    .await?;

    sqlx::query(
        "insert into webauthn_challenges (user_id, purpose, challenge, rp_id, expires_at) \
         values ($1, $2, $3, $4, $5)",
    )
    .bind(user_id)
    .bind(purpose)
    .bind(&challenge)
    .bind(rp_id)
    .bind(expires_at)
    .execute(pool)
    .await?;

    Ok(challenge)
}

/// Consume the challenge a completion call answers, if it is live and belongs to this purpose.
///
/// Returns `Ok(None)` when the challenge is unknown, expired, already used or was issued for
/// another account or purpose — the caller answers one refusal for all of them.
pub async fn take_challenge(
    pool: &PgPool,
    user_id: Uuid,
    purpose: &str,
    challenge: &str,
) -> Result<Option<Challenge>> {
    let row: Option<Challenge> = sqlx::query_as(
        "update webauthn_challenges set consumed_at = now() \
         where user_id = $1 and purpose = $2 and challenge = $3 \
           and consumed_at is null and expires_at > now() \
         returning id, user_id, purpose, challenge",
    )
    .bind(user_id)
    .bind(purpose)
    .bind(challenge)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

#[cfg(test)]
mod tests;
