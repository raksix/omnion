//! A real, in-process identity provider for the enterprise sign-in walk (REQ-006, slice 4b-2).
//!
//! Every other proof of enterprise sign-in tests one layer at a time: a token verifier against a
//! synthetic token, a SAML reader against a hand-built assertion, the HTTP layer against refusals
//! it triggers itself. None of them proves the thing a person actually does — that a **browser**
//! can be sent to a provider, come back with a code, and end up with a session here — because that
//! needs a provider on the other end.
//!
//! So this file is one. It speaks both protocols the platform claims to support, over a real TCP
//! listener, with real cryptography:
//!
//! * **OIDC**: `/.well-known/openid-configuration`, `/authorize`, `/token`, `/jwks`. It signs ID
//!   tokens with a freshly generated 2048-bit RSA key, publishes the matching JWKS, checks the
//!   PKCE `S256` challenge itself, and issues an `id_token` whose `c_hash` is the real code hash.
//! * **SAML**: an endpoint that answers a POSTed `SAMLResponse` with a signed assertion — the
//!   enveloped digest *and* the RSA signature over `SignedInfo`, both computed the way a
//!   directory computes them.
//!
//! The walk therefore proves the whole chain rather than a link of it: discovery over the wire, the
//! authorization URL the redirect really carries, the code exchange, RS256 verification against
//! published keys, the claim → role mapping, JIT provisioning, and the session cookie at the end.
//!
//! It is deliberately **not** a mock of our own code: the key material, the token assembly and the
//! XML signature are all produced here from first principles, so agreement between the two sides is
//! evidence rather than tautology. What it does share with the walk is the `rsa` crate, because the
//! arithmetic has to be real for either side to mean anything.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Base64url without padding — the JOSE alphabet.
fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Standard base64 — the XML signature alphabet.
fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The client id the walk registers the provider under.
pub const CLIENT_ID: &str = "omnion-stub";

/// The client secret the walk puts in the environment under `secret_ref`.
pub const CLIENT_SECRET: &str = "stub-client-secret-not-a-real-credential";

/// The audience a SAML assertion is issued for.
pub const SAML_AUDIENCE: &str = "https://omnion.test/sp";

/// The entity id the SAML provider signs as.
pub const SAML_ISSUER: &str = "https://idp.omnion.test/saml";

/// The signing key of the stub, in both of the shapes the two protocols need.
pub struct StubKey {
    private: rsa::RsaPrivateKey,
    /// The public half as a JWK (base64url modulus/exponent), for the JWKS document.
    jwk: Value,
    /// The public half as a PEM certificate, for the SAML `certificate_pem`.
    certificate_pem: String,
}

impl StubKey {
    /// Generate a 2048-bit key and derive both public shapes from it.
    pub fn generate() -> Self {
        use rsa::traits::PublicKeyParts;

        let private = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048)
            .expect("the platform has entropy");
        let modulus = private.n().to_bytes_be();
        let exponent = private.e().to_bytes_be();

        let jwk = json!({
            "kty": "RSA",
            "use": "sig",
            "alg": "RS256",
            "kid": "stub-key",
            "n": b64url(&modulus),
            "e": b64url(&exponent),
        });

        Self {
            certificate_pem: der_certificate(&private),
            jwk,
            private,
        }
    }

    /// The JWKS document this key set is published as.
    fn jwks(&self) -> Value {
        json!({ "keys": [self.jwk.clone()] })
    }

    /// Sign a compact JWS over an arbitrary payload.
    fn sign_jws(&self, payload: &Value) -> String {
        use rsa::pkcs1v15::SigningKey;
        use rsa::signature::{SignatureEncoding, Signer};

        let header = b64url(br#"{"alg":"RS256","kid":"stub-key","typ":"JWT"}"#);
        let body = b64url(payload.to_string().as_bytes());
        let signing_input = format!("{header}.{body}");

        let signature = SigningKey::<Sha256>::new(self.private.clone())
            .try_sign(signing_input.as_bytes())
            .expect("a signature");
        format!("{signing_input}.{}", b64url(&signature.to_bytes()))
    }

    /// Sign a SAML assertion, producing the two halves of the XML-signature binding.
    fn sign_saml(
        &self,
        assertion_id: &str,
        subject: &str,
        attributes: &[(&str, &str)],
        not_before: i64,
        not_after: i64,
    ) -> String {
        use rsa::pkcs1v15::SigningKey;
        use rsa::signature::{SignatureEncoding, Signer};

        let attributes_xml = attributes
            .iter()
            .map(|(name, value)| {
                format!(
                    r#"<saml:Attribute Name="{name}"><saml:AttributeValue>{value}</saml:AttributeValue></saml:Attribute>"#
                )
            })
            .collect::<String>();

        let body = format!(
            r#"<saml:Issuer>{SAML_ISSUER}</saml:Issuer>\
<saml:Subject><saml:NameID>{subject}</saml:NameID></saml:Subject>\
<saml:Conditions NotBefore="{not_before}" NotOnOrAfter="{not_after}">\
<saml:AudienceRestriction><saml:Audience>{SAML_AUDIENCE}</saml:Audience></saml:AudienceRestriction>\
</saml:Conditions>\
<saml:AttributeStatement>{attributes_xml}</saml:AttributeStatement>"#
        );

        // The enveloped-signature transform: the digest covers the assertion with its own
        // signature removed, which is exactly how a directory computes it.
        let envelope = format!(
            r#"<saml:Assertion xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" Version="2.0" ID="{assertion_id}">{body}</saml:Assertion>"#
        );
        let digest = b64(&Sha256::digest(envelope.as_bytes()));

        // `r##"…"##` rather than `r#"…"#`: the reference URI contains a literal `"#` (the
        // attribute delimiter followed by the fragment marker), which would close the raw string
        // one character early — a compile error that reads like a typo in the XML.
        let signed_info = format!(
            r##"<ds:SignedInfo><ds:SignatureMethod Algorithm="http://www.w3.org/2001/04/xmldsig-more#rsa-sha256"/><ds:Reference URI="#{assertion_id}"><ds:DigestMethod Algorithm="http://www.w3.org/2001/04/xmlenc#sha256"/><ds:DigestValue>{digest}</ds:DigestValue></ds:Reference></ds:SignedInfo>"##
        );
        let signature = SigningKey::<Sha256>::new(self.private.clone())
            .try_sign(signed_info.as_bytes())
            .expect("a signature");

        let signature_xml = format!(
            r#"<ds:Signature xmlns:ds="http://www.w3.org/2000/09/xmldsig#">{signed_info}<ds:SignatureValue>{value}</ds:SignatureValue></ds:Signature>"#,
            value = b64(&signature.to_bytes()),
        );

        format!(
            r#"<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" Version="2.0" ID="stub-response"><saml:Assertion xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" Version="2.0" ID="{assertion_id}">{signature_xml}{body}</saml:Assertion></samlp:Response>"#
        )
    }
}

// -------------------------------------------------------------------------------------------------
// DER helpers — a public key as a `SubjectPublicKeyInfo` a SAML verifier can read.
// -------------------------------------------------------------------------------------------------

/// Wrap already-encoded parts in a DER TLV with the given tag.
fn der_wrap(tag: u8, parts: &[&[u8]]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(part);
    }
    let mut out = vec![tag];
    if body.len() < 128 {
        out.push(body.len() as u8);
    } else {
        let mut encoded = Vec::new();
        let mut value = body.len();
        while value > 0 {
            encoded.insert(0, (value & 0xff) as u8);
            value >>= 8;
        }
        out.push(0x80 | encoded.len() as u8);
        out.extend_from_slice(&encoded);
    }
    out.extend_from_slice(&body);
    out
}

/// A DER INTEGER from a big-endian byte string, sign-padded.
fn der_integer(bytes: &[u8]) -> Vec<u8> {
    let mut body = bytes.to_vec();
    if body.first().is_some_and(|byte| byte & 0x80 != 0) {
        body.insert(0, 0x00);
    }
    der_wrap(0x02, &[&body])
}

/// The PEM form of the key's public half, wrapped as an SPKI a SAML reader parses.
fn der_certificate(private: &rsa::RsaPrivateKey) -> String {
    use rsa::traits::PublicKeyParts;

    let rsa_key = der_wrap(
        0x30,
        &[
            &der_integer(&private.n().to_bytes_be()),
            &der_integer(&private.e().to_bytes_be()),
        ],
    );
    // rsaEncryption OID.
    let algorithm = der_wrap(
        0x30,
        &[
            &[
                0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01,
            ],
            &[],
        ],
    );
    let bit_string = der_wrap(0x03, &[&[0x00], &rsa_key]);
    let spki = der_wrap(0x30, &[&algorithm, &bit_string]);
    format!(
        "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
        b64(&spki)
    )
}

// -------------------------------------------------------------------------------------------------
// The provider
// -------------------------------------------------------------------------------------------------

/// A one-shot authorization code the walk can spend exactly once.
#[derive(Debug, Clone)]
struct IssuedCode {
    challenge: String,
    subject: String,
    email: String,
    display_name: String,
    groups: Vec<String>,
}

/// What the walk can ask the stub about after a round trip.
#[derive(Debug, Default, Clone)]
pub struct StubObservations {
    /// The PKCE challenges the provider received, in order.
    pub challenges: Vec<String>,
    /// The `code_verifier` values the token endpoint accepted, in order.
    pub verifiers: Vec<String>,
    /// Whether the token endpoint ever refused an exchange.
    pub refused: bool,
}

/// A running identity provider.
pub struct StubIdp {
    addr: SocketAddr,
    key: Arc<StubKey>,
    state: Arc<Mutex<StubState>>,
    handle: tokio::task::JoinHandle<()>,
}

struct StubState {
    codes: HashMap<String, IssuedCode>,
    observations: StubObservations,
    /// Where the walk wants the browser sent, and the identity it should assert.
    next_subject: Option<(String, String, String, Vec<String>)>,
    next_challenge: Option<String>,
    /// Set when the walk wants the provider to issue a token for *some other* code, to prove the
    /// `c_hash` binding is real rather than cosmetic.
    wrong_code_hash: bool,
    /// Set when the walk wants a token signed by a key the JWKS does not publish.
    unsigned_by_published_key: bool,
}

impl StubIdp {
    /// Start the provider on an ephemeral port and serve until dropped.
    pub async fn start() -> Self {
        let key = Arc::new(StubKey::generate());
        let state = Arc::new(Mutex::new(StubState {
            codes: HashMap::new(),
            observations: StubObservations::default(),
            next_subject: None,
            next_challenge: None,
            wrong_code_hash: false,
            unsigned_by_published_key: false,
        }));

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port is available");
        let addr = listener
            .local_addr()
            .expect("a bound socket has an address");

        let server_state = Arc::clone(&state);
        let server_key = Arc::clone(&key);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let key = Arc::clone(&server_key);
                let state = Arc::clone(&server_state);
                tokio::spawn(async move {
                    let _ = serve(stream, key, state).await;
                });
            }
        });

        Self {
            addr,
            key,
            state,
            handle,
        }
    }

    /// The issuer the platform discovers, i.e. the `issuer` a provider row stores.
    pub fn issuer(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The `authorization_endpoint` a browser would be sent to.
    pub fn authorization_endpoint(&self) -> String {
        format!("{}/authorize", self.issuer())
    }

    /// The PEM certificate a SAML provider row must be given.
    pub fn certificate_pem(&self) -> String {
        self.key.certificate_pem.clone()
    }

    /// Tell the next `/authorize` which identity to assert, and with which PKCE challenge.
    pub fn expect_identity(
        &self,
        subject: &str,
        email: &str,
        display_name: &str,
        groups: &[&str],
        code_challenge: &str,
    ) {
        let mut state = self.state.lock().expect("the stub state is not poisoned");
        state.next_subject = Some((
            subject.to_owned(),
            email.to_owned(),
            display_name.to_owned(),
            groups.iter().map(|group| (*group).to_owned()).collect(),
        ));
        state.next_challenge = Some(code_challenge.to_owned());
    }

    /// Make the next token carry the code hash of a *different* code.
    pub fn corrupt_code_hash(&self) {
        self.state
            .lock()
            .expect("the stub state is not poisoned")
            .wrong_code_hash = true;
    }

    /// Make the next token signed by a key that is **not** the published one.
    pub fn sign_with_unpublished_key(&self) {
        self.state
            .lock()
            .expect("the stub state is not poisoned")
            .unsigned_by_published_key = true;
    }

    /// What the provider saw while the walk ran.
    pub fn observations(&self) -> StubObservations {
        self.state
            .lock()
            .expect("the stub state is not poisoned")
            .observations
            .clone()
    }
}

impl Drop for StubIdp {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// A parsed request line and body.
struct Request {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    body: String,
}

impl Request {
    /// One query parameter of the request.
    fn param(&self, key: &str) -> Option<String> {
        for pair in self.query.split('&') {
            let (name, value) = pair.split_once('=')?;
            if percent_decode(name) == key {
                return Some(percent_decode(value));
            }
        }
        None
    }

    /// One form field of the body.
    fn field(&self, key: &str) -> Option<String> {
        if !self
            .headers
            .get("content-type")
            .is_some_and(|value| value.contains("form-urlencoded"))
        {
            return None;
        }
        for pair in self.body.split('&') {
            if let Some((name, value)) = pair.split_once('=')
                && percent_decode(name) == key
            {
                return Some(percent_decode(value));
            }
        }
        None
    }
}

/// The `code` for a state we issued, computed the way a provider computes it.
fn make_code(issuer: &str, state: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(issuer.as_bytes());
    hasher.update(b"|");
    hasher.update(state.as_bytes());
    b64url(&hasher.finalize()[..18])
}

/// Percent-decode a form value (`+` is a space in `application/x-www-form-urlencoded`).
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("00");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(b'%'));
                index += 3;
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One connection: read the request, answer it.
async fn serve(
    mut stream: TcpStream,
    key: Arc<StubKey>,
    state: Arc<Mutex<StubState>>,
) -> std::io::Result<()> {
    let request = match read_request(&mut stream).await {
        Some(request) => request,
        None => return Ok(()),
    };

    let answer = route(&request, &key, &state).await;
    write_response(&mut stream, &answer).await
}

/// What a route decided: a status, a content type, a body, and an optional `Location`.
struct Answer {
    status: u16,
    content_type: &'static str,
    body: String,
    /// Set on a redirect, exactly as a real authorization endpoint sets it. The walk plays the
    /// browser, and a browser follows a header — a redirect delivered in a body is not a redirect.
    location: Option<String>,
}

impl Answer {
    fn json(status: u16, body: String) -> Self {
        Self {
            status,
            content_type: "application/json",
            body,
            location: None,
        }
    }

    fn redirect(location: String) -> Self {
        Self {
            status: 302,
            content_type: "text/plain",
            body: String::new(),
            location: Some(location),
        }
    }
}

/// Read one HTTP request; `None` when the peer closed before sending a line.
async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];

    // Headers first, until the blank line.
    let header_end = loop {
        if let Some(position) = find_subsequence(&buffer, b"\r\n\r\n") {
            break position;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }

    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let body_start = header_end + 4;
    while buffer.len() < body_start + content_length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }

    Some(Request {
        method,
        path: path.to_owned(),
        query: query.to_owned(),
        headers,
        body: String::from_utf8_lossy(
            &buffer[body_start..(body_start + content_length).min(buffer.len())],
        )
        .into_owned(),
    })
}

/// The first occurrence of a byte subsequence.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Answer one request.
async fn route(request: &Request, key: &Arc<StubKey>, state: &Arc<Mutex<StubState>>) -> Answer {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/.well-known/openid-configuration") => {
            // A real provider derives its issuer from its own host; so does this one, so the
            // document the platform discovers is self-consistent on whatever port we bound.
            let issuer = request_issuer(request);
            Answer::json(
                200,
                json!({
                    "issuer": issuer,
                    "authorization_endpoint": format!("{issuer}/authorize"),
                    "token_endpoint": format!("{issuer}/token"),
                    "jwks_uri": format!("{issuer}/jwks"),
                    "userinfo_endpoint": format!("{issuer}/userinfo"),
                    "response_types_supported": ["code"],
                    "subject_types_supported": ["public"],
                    "id_token_signing_alg_values_supported": ["RS256"],
                    "code_challenge_methods_supported": ["S256"],
                })
                .to_string(),
            )
        }
        ("GET", "/jwks") => Answer::json(200, key.jwks().to_string()),

        ("GET", "/authorize") => {
            let redirect_uri = request.param("redirect_uri").unwrap_or_default();
            let state_value = request.param("state").unwrap_or_default();
            let challenge = request.param("code_challenge").unwrap_or_default();
            let client_id = request.param("client_id").unwrap_or_default();

            let Some((subject, email, display_name, groups)) = request_identity(state) else {
                return Answer::json(
                    400,
                    json!({ "error": "no_identity_configured" }).to_string(),
                );
            };

            state
                .lock()
                .expect("the stub state is not poisoned")
                .observations
                .challenges
                .push(challenge.clone());

            if client_id != CLIENT_ID {
                return Answer::json(400, json!({ "error": "unauthorized_client" }).to_string());
            }

            let code = make_code(&request_issuer(request), &state_value);
            state
                .lock()
                .expect("the stub state is not poisoned")
                .codes
                .insert(
                    code.clone(),
                    IssuedCode {
                        challenge,
                        subject,
                        email,
                        display_name,
                        groups,
                    },
                );

            // A real authorization endpoint redirects the *browser*; the walk reads the
            // `Location` and calls the callback itself, which is exactly what a browser does.
            Answer::redirect(format!("{redirect_uri}?code={code}&state={state_value}"))
        }

        ("POST", "/token") => token(request, key, state),
        ("GET", "/userinfo") => Answer::json(
            200,
            json!({ "sub": "stub", "email": "nobody@omnion.test" }).to_string(),
        ),
        ("GET", "/saml/assertion") => saml_assertion(key, state),
        _ => Answer::json(404, json!({ "error": "not_found" }).to_string()),
    }
}

/// The issuer a provider derives from the request's own `Host` header.
fn request_issuer(request: &Request) -> String {
    format!(
        "http://{}",
        request
            .headers
            .get("host")
            .map_or("localhost", String::as_str)
    )
}

/// The identity the next authorization should assert, consumed on read.
fn request_identity(
    state: &Arc<Mutex<StubState>>,
) -> Option<(String, String, String, Vec<String>)> {
    let mut guard = state.lock().expect("the stub state is not poisoned");
    let identity = guard.next_subject.take();
    guard.next_challenge = None;
    identity
}

/// The token endpoint: PKCE is checked here, and the code is spent exactly once.
fn token(request: &Request, key: &Arc<StubKey>, state: &Arc<Mutex<StubState>>) -> Answer {
    let code = request.field("code").unwrap_or_default();
    let verifier = request.field("code_verifier").unwrap_or_default();
    let client_id = request.field("client_id").unwrap_or_default();
    let redirect_uri = request.field("redirect_uri").unwrap_or_default();

    let mut guard = state.lock().expect("the stub state is not poisoned");

    if client_id != CLIENT_ID {
        return Answer::json(401, json!({ "error": "invalid_client" }).to_string());
    }

    let Some(issued) = guard.codes.get(&code).cloned() else {
        guard.observations.refused = true;
        return Answer::json(400, json!({ "error": "invalid_grant" }).to_string());
    };

    // PKCE, checked the way a provider checks it: the challenge is S256(verifier).
    if !issued.challenge.is_empty() {
        let expected = b64url(&Sha256::digest(verifier.as_bytes()));
        if expected != issued.challenge {
            guard.observations.refused = true;
            return Answer::json(
                400,
                json!({ "error": "invalid_grant", "error_description": "PKCE" }).to_string(),
            );
        }
    }
    guard.observations.verifiers.push(verifier);

    // A code is single-use; spending it here is what makes a replay observable.
    guard.codes.remove(&code);

    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let issuer = request_issuer(request);
    let wrong_code_hash = guard.wrong_code_hash;
    guard.wrong_code_hash = false;
    let sign_with_unpublished = guard.unsigned_by_published_key;
    guard.unsigned_by_published_key = false;
    drop(guard);

    let hashed_code = if wrong_code_hash {
        // A token minted for a *different* code: the hash is right, the code is not.
        make_code(&issuer, "somebody-elses-code")
    } else {
        code_hash_of(&code)
    };

    let mut payload = json!({
        "iss": issuer,
        "aud": CLIENT_ID,
        "sub": issued.subject,
        "exp": now + 300,
        "iat": now,
        "email": issued.email,
        "email_verified": true,
        "name": issued.display_name,
        "c_hash": hashed_code,
        "at_hash": b64url(&Sha256::digest(b"access-token")),
    });
    if !issued.groups.is_empty() {
        payload["groups"] = json!(issued.groups);
    }

    let signing_key = if sign_with_unpublished {
        Arc::new(StubKey::generate())
    } else {
        Arc::clone(key)
    };
    let id_token = signing_key.sign_jws(&payload);
    let _ = redirect_uri;

    Answer::json(
        200,
        json!({
            "access_token": b64url(b"stub-access-token"),
            "token_type": "Bearer",
            "expires_in": 300,
            "id_token": id_token,
        })
        .to_string(),
    )
}

/// The `c_hash` of a code: the left half of its SHA-256, base64url (OIDC Core §3.1.3.6).
fn code_hash_of(code: &str) -> String {
    let digest = Sha256::digest(code.as_bytes());
    b64url(&digest[..digest.len() / 2])
}

/// A SAML provider answering with a signed assertion for whoever the walk set up.
///
/// The assertion is returned base64'd, which is what a browser POSTs to an assertion consumer
/// service — so the walk exercises the platform's own decode path rather than handing it XML.
fn saml_assertion(key: &Arc<StubKey>, state: &Arc<Mutex<StubState>>) -> Answer {
    let Some((subject, email, display_name, groups)) = request_identity(state) else {
        return Answer::json(400, json!({ "error": "no_identity" }).to_string());
    };

    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let attributes = groups
        .iter()
        .map(|group| ("groups", group.as_str()))
        .chain(std::iter::once(("email", email.as_str())))
        .chain(std::iter::once(("displayName", display_name.as_str())))
        .collect::<Vec<_>>();

    let document = key.sign_saml(
        "stub-assertion-1",
        &subject,
        &attributes,
        now - 60,
        now + 300,
    );
    Answer::json(
        200,
        json!({ "SAMLResponse": b64(document.as_bytes()) }).to_string(),
    )
}

/// Write a response with an optional `Location` header.
async fn write_response(stream: &mut TcpStream, answer: &Answer) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n",
        answer.status,
        reason(answer.status),
        answer.content_type,
        answer.body.len(),
    );
    if let Some(location) = answer.location.as_deref() {
        head.push_str(&format!("location: {location}\r\n"));
    }
    head.push_str("\r\n");

    stream.write_all(head.as_bytes()).await?;
    stream.write_all(answer.body.as_bytes()).await?;
    stream.flush().await
}

/// The reason phrase of a status the stub emits.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Unknown",
    }
}
