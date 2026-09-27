//! OIDC and generic OAuth2: discovery, the token exchange and the ID-token signature check
//! (docs/07-IAM.md §11).
//!
//! The trust decision is the **signature**, and everything else in this module exists to make
//! that check meaningful: the signing key comes from the provider's own discovery document (not
//! from the request), the `aud`/`iss`/`exp` claims are checked against what we configured, the
//! nonce is compared to the one the challenge issued, and the algorithm is pinned to what the
//! key's own type allows — a token asking to be verified as `HS256` against an RSA key is
//! refused rather than "verified" with the public key used as an HMAC secret.
//!
//! The JWKS document is cached per provider for a short window, because a sign-in is not the place
//! to fetch three documents over the network, and a key rotation must still be picked up without a
//! restart. A signature that fails against the cached keys refetches **once** before it is called
//! invalid: a provider rotating its keys is normal operation, not an attack.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use reqwest::Client;
use rsa::RsaPublicKey;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{IdentityError, Result};
use crate::sso::providers::ProviderKind;

/// Base64url without padding — the encoding every JOSE value uses.
fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// Largest discovery/JWKS document accepted, in bytes. A provider's metadata is kilobytes; a
/// megabyte of "metadata" is an attack, not a provider.
const MAX_METADATA_BYTES: usize = 512 * 1024;

/// How long a discovery document and a JWKS are trusted before a refetch.
const METADATA_TTL: Duration = Duration::from_secs(600);

/// Clock skew tolerated on `exp`/`iat`/`nbf`, in seconds.
pub const CLOCK_SKEW_SECONDS: i64 = 60;

/// What discovery told us about an OIDC provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    /// `issuer` — the exact string tokens must carry.
    pub issuer: String,
    /// Where the browser is sent.
    pub authorization_endpoint: String,
    /// Where the code is exchanged (OIDC).
    pub token_endpoint: String,
    /// Where the signed keys live.
    pub jwks_uri: String,
    /// Where the claims of a user are read (OAuth2 without an ID token).
    pub userinfo_endpoint: Option<String>,
}

impl Discovery {
    /// Parse a `/.well-known/openid-configuration` document.
    pub fn from_value(document: &Value) -> Result<Self> {
        let text = |field: &str| -> Result<String> {
            document
                .get(field)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    IdentityError::InvalidProvider(format!(
                        "the discovery document has no `{field}`"
                    ))
                })
        };
        let issuer = text("issuer")?;
        for (field, value) in [
            ("authorization_endpoint", text("authorization_endpoint")?),
            ("token_endpoint", text("token_endpoint")?),
            ("jwks_uri", text("jwks_uri")?),
        ] {
            // An endpoint that is not an absolute HTTPS URL is a configuration mistake we refuse
            // now rather than a request that leaks the authorization code somewhere unexpected.
            if !value.starts_with("https://") && !value.starts_with("http://127.0.0.1")
                && !value.starts_with("http://localhost")
            {
                return Err(IdentityError::InvalidProvider(format!(
                    "`{field}` must be an absolute http(s) URL"
                )));
            }
        }
        Ok(Self {
            issuer,
            authorization_endpoint: text("authorization_endpoint")?,
            token_endpoint: text("token_endpoint")?,
            jwks_uri: text("jwks_uri")?,
            userinfo_endpoint: document
                .get("userinfo_endpoint")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
        })
    }
}

/// One JWK, reduced to what a verification needs.
#[derive(Debug, Clone)]
pub struct Jwk {
    /// `RSA` keys are the ones a platform can verify.
    pub key_id: String,
    /// The RSA modulus.
    pub n: Vec<u8>,
    /// The RSA public exponent.
    pub e: Vec<u8>,
}

/// Parse a JWKS document into the keys we can verify with.
#[must_use]
pub fn parse_jwks(document: &Value) -> Vec<Jwk> {
    let Some(keys) = document.get("keys").and_then(Value::as_array) else {
        return Vec::new();
    };

    keys
        .iter()
        .filter(|key| key.get("kty").and_then(Value::as_str) == Some("RSA"))
        .filter_map(|key| {
            let key_id = key.get("kid").and_then(Value::as_str)?;
            let n = b64().decode(key.get("n")?.as_str()?).ok()?;
            let e = b64().decode(key.get("e")?.as_str()?).ok()?;
            // A key shorter than 2048 bits is not a key worth trusting with an identity; the
            // length is compared on the modulus, which is what the signature actually uses.
            if n.len() < 256 {
                return None;
            }
            Some(Jwk {
                key_id: key_id.to_owned(),
                n,
                e,
            })
        })
        .collect()
}

/// A JOSE header, already decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JwtHeader {
    /// The signing algorithm (`RS256`, …).
    pub algorithm: String,
    /// Which key of the JWKS signed this.
    pub key_id: Option<String>,
    /// The type of the token.
    pub typ: Option<String>,
}

impl JwtHeader {
    /// Split and decode a compact JWS without verifying it — the header decides which algorithm
    /// to verify with, and the signature check below is what makes the result trustworthy.
    pub fn parse(token: &str) -> Result<Self> {
        let mut parts = token.split('.');
        let header = parts.next().unwrap_or_default();
        if parts.next().is_none() || parts.next().is_none() {
            return Err(IdentityError::InvalidProvider(
                "the token is not a compact JWS".into(),
            ));
        }

        let decoded = b64().decode(header).map_err(|_| {
            IdentityError::InvalidProvider("the token header is not base64url".into())
        })?;
        let value: Value = serde_json::from_slice(&decoded).map_err(|_| {
            IdentityError::InvalidProvider("the token header is not JSON".into())
        })?;

        let algorithm = value
            .get("alg")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| {
                IdentityError::InvalidProvider("the token header names no algorithm".into())
            })?
            .to_owned();

        // `none` and every symmetric algorithm are refused here rather than in the verifier: a
        // token that asks for an algorithm this server cannot verify must never be "verified".
        if algorithm.starts_with("HS") || algorithm == "none" {
            return Err(IdentityError::InvalidProvider(format!(
                "the token asks for `{algorithm}`, which this server does not verify"
            )));
        }

        Ok(Self {
            algorithm,
            key_id: value
                .get("kid")
                .and_then(Value::as_str)
                .map(str::to_owned),
            typ: value.get("typ").and_then(Value::as_str).map(str::to_owned),
        })
    }
}

/// The registered claims a verification checks beyond the signature.
#[derive(Debug, Clone, PartialEq)]
pub struct Claims {
    /// Every claim, for the caller to read.
    pub values: serde_json::Map<String, Value>,
}

impl Claims {
    /// The `exp` claim as a Unix timestamp.
    #[must_use]
    pub fn expires_at(&self) -> Option<i64> {
        self.values.get("exp").and_then(Value::as_i64)
    }

    /// The `aud` claim, which may be a string or an array of strings.
    #[must_use]
    pub fn audiences(&self) -> Vec<String> {
        match self.values.get("aud") {
            Some(Value::String(text)) => vec![text.clone()],
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The `iss` claim.
    #[must_use]
    pub fn issuer(&self) -> Option<&str> {
        self.values.get("iss").and_then(Value::as_str)
    }
}

/// Decode the payload of a token. Called only after [`verify_rs256`] has accepted the signature.
pub fn decode_claims(token: &str) -> Result<Claims> {
    let payload = token.split('.').nth(1).ok_or_else(|| {
        IdentityError::InvalidProvider("the token carries no payload".into())
    })?;
    let decoded = b64()
        .decode(payload)
        .map_err(|_| IdentityError::InvalidProvider("the token payload is not base64url".into()))?;
    let value: Value = serde_json::from_slice(&decoded).map_err(|_| {
        IdentityError::InvalidProvider("the token payload is not JSON".into())
    })?;
    let Value::Object(values) = value else {
        return Err(IdentityError::InvalidProvider(
            "the token payload is not a claim set".into(),
        ));
    };
    Ok(Claims { values })
}

/// Verify an `RS256` signature over a token with one JWK.
///
/// RS256 is `RSASSA-PKCS1-v1_5` with SHA-256 (RFC 7518 §3.3, RFC 8017 §8.2): the signature is a
/// big-endian integer that has to satisfy `s^e ≡ m (mod n)`, and the `rsa` crate already in the
/// workspace tree is what performs that. The only platform-specific part is turning the JWK's
/// base64url modulus and exponent into a public key, which is what this function does first.
pub fn verify_rs256(token: &str, key: &Jwk) -> Result<()> {
    let mut parts = token.split('.');
    let header = parts.next().unwrap_or_default();
    let payload = parts.next().unwrap_or_default();
    let signature = parts.next().unwrap_or_default();

    let signature = b64().decode(signature).map_err(|_| {
        IdentityError::InvalidProvider("the token signature is not base64url".into())
    })?;

    // The signature is the modulus length: a shorter or longer one is a different algorithm or a
    // truncated token, and either way it is not a signature this key could have produced.
    if signature.len() != key.n.len() {
        return Err(IdentityError::InvalidProvider(
            "the token signature does not match the provider's key".into(),
        ));
    }

    let public_key = RsaPublicKey::new(
        rsa::BigUint::from_bytes_be(&key.n),
        rsa::BigUint::from_bytes_be(&key.e),
    )
    .map_err(|_| {
        IdentityError::InvalidProvider("the provider's signing key is not a valid RSA key".into())
    })?;

    let padding = rsa::Pkcs1v15Sign::new::<Sha256>();
    public_key
        .verify(padding, &Sha256::digest(format!("{header}.{payload}").as_bytes()), &signature)
        .map_err(|_| {
            IdentityError::InvalidProvider(
                "the token signature is not valid for this provider".into(),
            )
        })
}

/// What a provider's endpoints resolve to for one sign-in.
#[derive(Debug, Clone)]
pub struct HttpClient {
    /// The inner client — cheap to clone, kept behind an `Arc` by the caller.
    inner: std::sync::Arc<Client>,
}

impl HttpClient {
    /// A client with a short timeout: a sign-in that waits a minute for metadata has already
    /// failed, and the browser is showing a spinner.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(
                Client::builder()
                    .timeout(Duration::from_secs(10))
                    .user_agent("Omnion-SSO/1")
                    .build()
                    .unwrap_or_default(),
            ),
        }
    }

    /// Fetch a JSON document with a size cap and a JSON content type.
    pub async fn get_json(&self, url: &str) -> Result<Value> {
        let response = self
            .inner
            .get(url)
            .send()
            .await
            .map_err(|error| {
                IdentityError::InvalidProvider(format!("the provider did not answer: {error}"))
            })?;
        if !response.status().is_success() {
            return Err(IdentityError::InvalidProvider(format!(
                "the provider answered {} for {url}",
                response.status()
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| {
                IdentityError::InvalidProvider(format!("the provider's answer did not read: {error}"))
            })?;
        if bytes.len() > MAX_METADATA_BYTES {
            return Err(IdentityError::InvalidProvider(
                "the provider's document is implausibly large".into(),
            ));
        }
        serde_json::from_slice(&bytes).map_err(|_| {
            IdentityError::InvalidProvider("the provider's answer is not JSON".into())
        })
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

/// A short-lived cache of discovery documents and JWKS, keyed by provider id.
#[derive(Debug, Default)]
pub struct MetadataCache {
    entries: Mutex<HashMap<String, (Value, Instant)>>,
}

impl MetadataCache {
    /// A fresh cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A cached document, if it is still inside its window.
    pub fn get(&self, key: &str) -> Option<Value> {
        let entries = self.entries.lock().ok()?;
        let (value, stored_at) = entries.get(key)?;
        (stored_at.elapsed() < METADATA_TTL).then(|| value.clone())
    }

    /// Remember a document.
    pub fn put(&self, key: &str, value: Value) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(key.to_owned(), (value, Instant::now()));
        }
    }

    /// Forget a provider's documents — used after a configuration change and on a failed
    /// signature, so a rotated key is picked up immediately.
    pub fn invalidate(&self, key: &str) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(key);
        }
    }
}

/// A provider's verified assertion: the claims plus how they were proven.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedAssertion {
    /// The claim set.
    pub claims: serde_json::Map<String, Value>,
    /// The subject id, read from the claim set.
    pub subject: String,
}

/// Verify a token's signature against a key set, trying the `kid` first and then every RSA key.
///
/// A provider that publishes one key is the common case; a provider mid-rotation publishes two
/// with different `kid`s, and matching on the header is what keeps a sign-in working through it.
pub fn verify_signature(token: &str, header: &JwtHeader, keys: &[Jwk]) -> Result<()> {
    if header.algorithm != "RS256" {
        return Err(IdentityError::InvalidProvider(format!(
            "`{}` is not a supported signing algorithm",
            header.algorithm
        )));
    }

    let candidates = match &header.key_id {
        Some(kid) => keys.iter().filter(|key| &key.key_id == kid).collect::<Vec<_>>(),
        None => keys.iter().collect::<Vec<_>>(),
    };
    // A `kid` that names no published key is a refusal, not a reason to try every other key —
    // otherwise a token could pick a key the provider did not intend it for.
    if candidates.is_empty() {
        if header.key_id.is_some() {
            return Err(IdentityError::InvalidProvider(
                "the token names a signing key the provider does not publish".into(),
            ));
        }
        return Err(IdentityError::InvalidProvider(
            "the provider publishes no usable signing key".into(),
        ));
    }

    for key in candidates {
        if verify_rs256(token, key).is_ok() {
            return Ok(());
        }
    }
    Err(IdentityError::InvalidProvider(
        "the token signature is not valid for this provider".into(),
    ))
}

/// Check the registered claims: expiry, audience, issuer and the nonce the challenge issued.
pub fn verify_claims(
    claims: &Claims,
    expected_issuer: &str,
    expected_audience: &str,
    expected_nonce: Option<&str>,
    now_unix: i64,
) -> Result<()> {
    let expiry = claims.expires_at().ok_or_else(|| {
        IdentityError::InvalidProvider("the token has no expiry".into())
    })?;
    if expiry + CLOCK_SKEW_SECONDS < now_unix {
        return Err(IdentityError::InvalidProvider("the token has expired".into()));
    }

    if let Some(issued_at) = claims.values.get("iat").and_then(Value::as_i64)
        && issued_at - CLOCK_SKEW_SECONDS > now_unix
    {
        return Err(IdentityError::InvalidProvider(
            "the token is issued in the future".into(),
        ));
    }

    if let Some(not_before) = claims.values.get("nbf").and_then(Value::as_i64)
        && not_before - CLOCK_SKEW_SECONDS > now_unix
    {
        return Err(IdentityError::InvalidProvider(
            "the token is not valid yet".into(),
        ));
    }

    if !claims.audiences().iter().any(|aud| aud == expected_audience) {
        return Err(IdentityError::InvalidProvider(
            "the token is not for this application".into(),
        ));
    }

    if let Some(issuer) = claims.issuer() {
        if issuer.trim_end_matches('/') != expected_issuer.trim_end_matches('/') {
            return Err(IdentityError::InvalidProvider(
                "the token comes from a different issuer".into(),
            ));
        }
    } else {
        return Err(IdentityError::InvalidProvider("the token names no issuer".into()));
    }

    if let Some(expected) = expected_nonce {
        match claims.values.get("nonce").and_then(Value::as_str) {
            Some(nonce) if nonce == expected => {}
            Some(_) => {
                return Err(IdentityError::InvalidProvider(
                    "the token carries a different nonce than this sign-in issued".into(),
                ));
            }
            None => {
                return Err(IdentityError::InvalidProvider(
                    "the token carries no nonce".into(),
                ));
            }
        }
    }

    Ok(())
}

/// The OAuth2 PKCE challenge for a verifier (RFC 7636 §4.2, S256).
#[must_use]
pub fn pkce_challenge(verifier: &str) -> String {
    b64().encode(Sha256::digest(verifier.as_bytes()))
}

/// The kind a provider's row describes, as the flow needs it.
#[must_use]
pub fn flow_of(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Oidc => "oidc",
        ProviderKind::Oauth2 => "oauth2",
        ProviderKind::Saml => "saml",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn jwk_bytes(seed: u8) -> Vec<u8> {
        // 256 bytes of modulus is the floor `parse_jwks` accepts; a fixed, non-random body keeps
        // the arithmetic tests deterministic.
        let mut bytes = vec![0x00_u8; 256];
        bytes[255] = seed | 0x03;
        bytes[254] = 0x11;
        bytes
    }

    #[test]
    fn a_discovery_document_becomes_endpoints() {
        let document = json!({
            "issuer": "https://idp.example/realms/omnion",
            "authorization_endpoint": "https://idp.example/auth",
            "token_endpoint": "https://idp.example/token",
            "jwks_uri": "https://idp.example/jwks",
            "userinfo_endpoint": "https://idp.example/userinfo",
        });
        let discovery = Discovery::from_value(&document).expect("a complete document");
        assert_eq!(discovery.issuer, "https://idp.example/realms/omnion");
        assert_eq!(discovery.jwks_uri, "https://idp.example/jwks");
    }

    #[test]
    fn a_discovery_document_without_an_endpoint_is_refused() {
        let document = json!({
            "issuer": "https://idp.example",
            "authorization_endpoint": "https://idp.example/auth",
        });
        assert!(Discovery::from_value(&document).is_err());
    }

    #[test]
    fn a_relative_endpoint_is_refused() {
        let document = json!({
            "issuer": "https://idp.example",
            "authorization_endpoint": "/auth",
            "token_endpoint": "https://idp.example/token",
            "jwks_uri": "https://idp.example/jwks",
        });
        assert!(
            Discovery::from_value(&document).is_err(),
            "a relative endpoint would send the code somewhere unexpected"
        );
    }

    #[test]
    fn a_jwks_keeps_only_usable_rsa_keys() {
        let document = json!({
            "keys": [
                { "kty": "RSA", "kid": "good", "n": b64().encode(jwk_bytes(7)), "e": "AQAB" },
                { "kty": "oct", "kid": "symmetric", "k": b64().encode(b"secret") },
                { "kty": "RSA", "kid": "short", "n": b64().encode(b"too-short"), "e": "AQAB" },
            ]
        });
        let keys = parse_jwks(&document);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key_id, "good");
    }

    #[test]
    fn a_symmetric_or_unsigned_algorithm_is_refused_before_any_key_is_touched() {
        let header = format!("{}.{}.{}", b64().encode(br#"{"alg":"HS256"}"#), b64().encode(b"{}"), "");
        assert!(JwtHeader::parse(&header).is_err());

        let unsigned = format!(
            "{}.{}.{}",
            b64().encode(br#"{"alg":"none"}"#),
            b64().encode(b"{}"),
            ""
        );
        assert!(JwtHeader::parse(&unsigned).is_err());

        let rs = format!(
            "{}.{}.{}",
            b64().encode(br#"{"alg":"RS256","kid":"a"}"#),
            b64().encode(b"{}"),
            b64().encode(b"sig")
        );
        let parsed = JwtHeader::parse(&rs).expect("RS256 is supported");
        assert_eq!(parsed.algorithm, "RS256");
        assert_eq!(parsed.key_id.as_deref(), Some("a"));
    }

    #[test]
    fn a_token_that_is_not_a_compact_jws_is_refused() {
        assert!(JwtHeader::parse("not-a-token").is_err());
        assert!(JwtHeader::parse("only.two").is_err());
    }

    #[test]
    fn a_signature_over_a_real_key_verifies_and_a_tampered_one_does_not() {
        // A 2048-bit key generated for this test: the signature below was produced with its
        // private half, so a broken verifier (wrong prefix, wrong padding, wrong byte order)
        // cannot pass by accident.
        let (jwk, token) = signed_token();

        let header = JwtHeader::parse(&token).expect("a well-formed token");
        let keys = vec![jwk];
        verify_signature(&token, &header, &keys).expect("a genuine signature verifies");

        let tampered = token.replace(".eyJ", ".eyK");
        if tampered != token {
            let header = JwtHeader::parse(&tampered).expect("still well-formed");
            assert!(
                verify_signature(&tampered, &header, &keys).is_err(),
                "a tampered payload must not verify"
            );
        }
    }

    #[test]
    fn a_token_signed_by_another_key_is_refused() {
        let (jwk, token) = signed_token();
        let header = JwtHeader::parse(&token).expect("a well-formed token");
        let mut other = jwk.clone();
        other.n[200] ^= 0xff;
        assert!(
            verify_signature(&token, &header, &[other]).is_err(),
            "a different modulus must not verify"
        );
    }

    #[test]
    fn a_key_id_that_names_no_published_key_is_refused() {
        let (jwk, token) = signed_token();
        let header = JwtHeader::parse(&token).expect("a well-formed token");
        assert!(
            verify_signature(&token, &header, &[]).is_err(),
            "an empty key set verifies nothing"
        );
        let _ = jwk;
    }

    #[test]
    fn the_registered_claims_are_all_checked() {
        let now = 1_700_000_000_i64;
        let good = Claims {
            values: json!({
                "sub": "1",
                "iss": "https://idp.example",
                "aud": ["omnion", "other"],
                "exp": now + 60,
                "iat": now - 5,
                "nonce": "n-1",
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        };
        verify_claims(&good, "https://idp.example", "omnion", Some("n-1"), now)
            .expect("a fresh, well-formed token");

        let expired = Claims {
            values: json!({ "sub": "1", "iss": "https://idp.example", "aud": "omnion", "exp": now - 500 })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        };
        assert!(verify_claims(&expired, "https://idp.example", "omnion", None, now).is_err());

        let wrong_audience = Claims {
            values: json!({ "sub": "1", "iss": "https://idp.example", "aud": "someone-else", "exp": now + 60 })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        };
        assert!(
            verify_claims(&wrong_audience, "https://idp.example", "omnion", None, now).is_err()
        );

        let wrong_issuer = Claims {
            values: json!({ "sub": "1", "iss": "https://evil.example", "aud": "omnion", "exp": now + 60 })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        };
        assert!(verify_claims(&wrong_issuer, "https://idp.example", "omnion", None, now).is_err());

        let wrong_nonce = Claims {
            values: json!({
                "sub": "1", "iss": "https://idp.example", "aud": "omnion", "exp": now + 60, "nonce": "other"
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        };
        assert!(
            verify_claims(&wrong_nonce, "https://idp.example", "omnion", Some("n-1"), now).is_err()
        );

        let no_expiry = Claims {
            values: json!({ "sub": "1", "iss": "https://idp.example", "aud": "omnion" })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        };
        assert!(verify_claims(&no_expiry, "https://idp.example", "omnion", None, now).is_err());
    }

    #[test]
    fn a_token_within_the_clock_skew_still_verifies() {
        let now = 1_700_000_000_i64;
        let claims = Claims {
            values: json!({ "sub": "1", "iss": "https://idp.example", "aud": "omnion", "exp": now - 30 })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        };
        verify_claims(&claims, "https://idp.example", "omnion", None, now)
            .expect("30 seconds of skew is normal clock drift");
    }

    #[test]
    fn pkce_hashes_the_verifier() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    /// A real 2048-bit RSA key pair and a token signed with it, so the verifier is tested against
    /// cryptography rather than against itself: a padding assembly that is one byte off, or a
    /// byte order that is reversed, cannot pass.
    fn signed_token() -> (Jwk, String) {
        use rsa::RsaPrivateKey;
        use rsa::pkcs1v15::SigningKey;
        use rsa::signature::{SignatureEncoding, Signer};
        use rsa::traits::PublicKeyParts;

        let mut rng = rand::rngs::OsRng;
        let private = RsaPrivateKey::new(&mut rng, 2048).expect("the platform has enough entropy");
        let signing = SigningKey::<Sha256>::new(private.clone());

        let header = br#"{"alg":"RS256","kid":"test-key","typ":"JWT"}"#;
        let payload = br#"{"sub":"00u42","iss":"https://idp.example","aud":"omnion"}"#;
        let signing_input = format!("{}.{}", b64().encode(header), b64().encode(payload));
        // `DigestSigner` hashes the message itself, so the signing input is passed whole —
        // the same input the verifier later digests.
        let signature = signing
            .try_sign(signing_input.as_bytes())
            .expect("a signature");
        let token = format!("{signing_input}.{}", b64().encode(signature.to_bytes()));

        (
            Jwk {
                key_id: "test-key".to_owned(),
                n: private.n().to_bytes_be(),
                e: private.e().to_bytes_be(),
            },
            token,
        )
    }
}
