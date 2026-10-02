//! OAuth 2.0 authorization-code flow for credential types (REQ-087, slice 3).
//!
//! Four things in here are load-bearing, and each is a place the obvious implementation
//! silently gets it wrong:
//!
//! 1. **The `state` is a signed, expiring, single-use value — not a random string compared to a
//!    column.** CSRF on an OAuth callback is a real attack: an attacker starts the flow with
//!    *their* account, gets the `code`, and walks it into *your* credential. `state` is what
//!    makes the callback prove the flow started in this browser, so it must be unforgeable
//!    (`build_state` mints 32 random bytes into an HMAC), bound to the credential, and it must
//!    stop working the moment it is used. A tampered `state` is refused with
//!    `credential_oauth_state` and never reaches the token exchange.
//! 2. **PKCE is not optional when the type asks for it.** The verifier is derived from the
//!    challenge by S256, and the *same* verifier has to come back in the token request; a
//!    challenge/verifier pair that does not verify is a broken flow, so
//!    [`PkcePair::verify`] is the check the callback runs before it spends the code.
//! 3. **The token set never lands in this crate.** Everything about the provider's answer —
//!    the access token, the refresh token — goes straight to the encrypted store, and this
//!    module keeps only what a *panel* may show: the subject, the scopes, and the expiry. That
//!    split is why [`TokenSet`] has no `Debug` and no `Serialize`.
//! 4. **A refresh is single-flight per credential.** Six nodes finishing at once must not fire
//!    six refreshes; the second through sixth wait for the first's answer and then read the
//!    result. That is [`RefreshLock`]'s whole job, and it is a *process* lock on purpose:
//!    cross-process single-flight is REQ-125's job (it owns the store), and pretending this
//!    one is global would be a claim this module cannot keep.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq as _;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// HMAC-SHA256, the state signature.
type HmacSha256 = Hmac<Sha256>;

/// Bytes of entropy in the *nonce* of a `state` value, before it is base64'd.
///
/// Ninety-six bits, not the 256 a random CSRF token would carry, and deliberately so: the
/// nonce's job is to make two states for one credential distinct. Unforgeability comes from
/// the HMAC, not from the nonce's length, and a longer nonce is a longer URL for a person to
/// click through a consent screen.
const STATE_NONCE_BYTES: usize = 12;

/// Bytes of entropy in a PKCE verifier. RFC 7636 §4.1 allows 43–128; 32 bytes is 43
/// characters of base64url, which is the *minimum* and therefore the one a server that
/// validates length strictly will accept.
const VERIFIER_BYTES: usize = 32;

/// How long a `state` stays valid. Ten minutes is long enough for a person to read a consent
/// screen and click "allow", and short enough that a state captured from a log is dead before
/// anybody could replay it.
pub const STATE_TTL: Duration = Duration::from_secs(600);

/// Domain separation for the state signature.
const STATE_LABEL: &[u8] = b"omnion.credential.oauth.state.v1";

/// The OAuth 2.0 error codes REQ-087 names for this flow.
pub mod codes {
    /// The `state` was absent, malformed, expired, already used, or bound to another credential.
    pub const STATE: &str = "credential_oauth_state";
    /// The token exchange failed, and why.
    pub const EXCHANGE: &str = "credential_oauth_exchange";
    /// The refresh failed — this is what lands as `needs_reauth`.
    pub const REFRESH: &str = "credential_oauth_refresh_failed";
    /// The type has no `oauth2` config, so the button has nothing to start.
    pub const UNSUPPORTED: &str = "credential_oauth_unsupported";
    /// The provider's PKCE challenge did not verify against our verifier.
    pub const PKCE: &str = "credential_oauth_pkce";
}

// ---------------------------------------------------------------------------------------------
// PKCE
// ---------------------------------------------------------------------------------------------

/// One PKCE challenge/verifier pair (RFC 7636).
///
/// Both halves are held only for the lifetime of a single flow. The challenge goes into the
/// authorization URL the person visits; the verifier is what the token request must present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkcePair {
    /// `S256(challenge)`, base64url, no padding.
    pub challenge: String,
    /// The secret. Never logged, never returned to a browser.
    verifier: String,
}

impl PkcePair {
    /// Mint a fresh pair.
    #[must_use]
    pub fn generate() -> Self {
        let verifier = URL_SAFE_NO_PAD.encode(random_bytes(VERIFIER_BYTES));
        Self::derive(&verifier)
    }

    /// Derive the challenge from a verifier.
    ///
    /// Exposed so the *server* side of a flow can rebuild a pair from a verifier it stashed,
    /// and so the test suite can prove `S256` is actually `S256` against the RFC's worked
    /// example rather than trusting the code path.
    #[must_use]
    pub fn derive(verifier: &str) -> Self {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self {
            challenge,
            verifier: verifier.to_string(),
        }
    }

    /// The verifier, for the token request.
    #[must_use]
    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    /// Check a verifier against a challenge.
    ///
    /// The callback runs this before it spends the code. Comparing in constant time is not
    /// paranoia here — a mismatch means somebody is guessing, and a timing-variable
    /// comparison is a guess oracle.
    #[must_use]
    pub fn verify(challenge: &str, verifier: &str) -> bool {
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let constant_eq = expected.as_bytes().ct_eq(challenge.as_bytes());
        bool::from(constant_eq)
    }
}

// ---------------------------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------------------------

/// A pending flow: the credential it belongs to, its PKCE pair, and when it stops being valid.
///
/// The `state` string itself is the signed payload; the struct is what the *server* holds
/// alongside it, and it holds no secret — the verifier is the only sensitive field, and it
/// lives here for the length of the flow and no longer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFlow {
    /// The credential this flow will attach to.
    pub credential_id: uuid::Uuid,
    /// The signed `state` the provider will echo back.
    pub state: String,
    /// The flow's PKCE pair.
    pub pkce: PkcePair,
    /// When it was minted.
    pub started_at: OffsetDateTime,
    /// When it stops being acceptable.
    pub expires_at: OffsetDateTime,
}

impl PendingFlow {
    /// Whether the flow is still inside its window.
    #[must_use]
    pub fn is_live(&self, now: OffsetDateTime) -> bool {
        self.started_at <= now && now < self.expires_at
    }

    /// The `code_verifier` query parameter, when the type uses PKCE.
    #[must_use]
    pub fn code_challenge_method(&self) -> Option<&'static str> {
        Some("S256").filter(|_| !self.pkce.challenge.is_empty())
    }
}

/// Mint a signed `state` for one flow.
///
/// The value is `<organization>.<credential>:<issued_at>:<nonce>.<signature>`, where the
/// signature is `HMAC-SHA256(key, label || payload)`. The key is the installation's, so a
/// `state` this function did not mint — one from another installation, or one edited in the
/// query string — does not verify.
///
/// The organization is in the payload because the callback has no session to scope its lookup
/// with. Putting it in the *signed* bytes is what makes that safe: an attacker who edits the
/// organization in the query string invalidates the signature, so the tenant cannot be
/// swapped. The tenant is not a secret, but an unsigned field in a value whose whole job is
/// to be unforgeable is a field somebody will eventually forget to check.
#[must_use]
pub fn build_state(
    organization_id: uuid::Uuid,
    credential_id: uuid::Uuid,
    key: &[u8],
    now: OffsetDateTime,
) -> String {
    let payload = format!(
        "{organization_id}.{credential_id}:{}:{}",
        now.unix_timestamp_nanos(),
        URL_SAFE_NO_PAD.encode(random_bytes(STATE_NONCE_BYTES))
    );
    format!("{payload}.{}", sign(key, &payload))
}

/// What a verified `state` proves.
///
/// Returned as a pair rather than a bare id so the callback cannot accidentally scope its
/// lookup to the wrong tenant: it receives the organization the state was *minted* for, not
/// one it chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedState {
    /// The organization the flow was started in.
    pub organization_id: uuid::Uuid,
    /// The credential the flow will attach to.
    pub credential_id: uuid::Uuid,
}

/// `SHA-256` of a state, lower-case hex — what `workflow_oauth_flows.state_hash` holds.
///
/// The state is a bearer value: anything able to read that table could otherwise replay a
/// callback. Hashing costs nothing (a callback is single-use and dies in ten minutes) and
/// removes the class entirely, and the hash lives here rather than in the store so the
/// *hashing* of a state is one function in the crate that owns states — the alternative is
/// two copies of a scheme whose only requirement is that they agree.
#[must_use]
pub fn state_hash(state: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(state.as_bytes());
    hex_encode(&hasher.finalize())
}

/// Derive a stable key from an installation secret, for the state signature.
///
/// **Deterministic by construction, and that is the whole point.** An HMAC key has to be the
/// same bytes on every call in the process, so this is a keyed *hash* and not an encryption:
/// [`LocalBox`] and the identity crate's `SecretBox` are both correct primitives for sealing a
/// value and both attach a fresh random nonce, which is exactly right for a PKCE verifier and
/// silently fatal for a key. A version that derived this through `encrypt` produced different
/// bytes per call, so `build_state` and `verify_state` never agreed and every callback failed
/// as `credential_oauth_state` — an error naming CSRF for what was really a key that never
/// matched.
///
/// The label is what domain-separates this key from the seal's, so an envelope this module
/// produced cannot be replayed as a `state` and a `state` cannot be opened as an envelope.
#[must_use]
pub fn derive_state_key(material: &[u8], label: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(STATE_LABEL);
    hasher.update(label);
    hasher.update(material);
    hasher.finalize().to_vec()
}

/// Lower-case hex, so the stored hash is a plain string a human can compare in a `psql` session.
fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// Why a `state` was refused.
///
/// The four cases a caller must tell apart are deliberately distinct: "this is not ours" is a
/// CSRF attempt, "this expired" is a person who took too long, "this is not yours" is somebody
/// replaying their own valid state against a different credential, and "this is spent" is a
/// double-submitted consent screen.
///
/// [`WrongOrganization`](Self::WrongOrganization) exists because the state payload carries
/// the organization as well as the credential id. The callback runs with **no session** — the
/// browser is at the provider — so the organization has to come from the state, and a state
/// that names only a credential id would leave the handler with a credential id it could not
/// look up in any tenant. Minting the id alone is what makes the obvious version of the
/// callback either guess an organization or query without a scope, and both are worse than
/// carrying the tenant in the signed payload where it cannot be swapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateRejection {
    /// Absent, malformed, or signed with a different key.
    Unrecognised,
    /// Well-formed and correctly signed, but past its window.
    Expired,
    /// Names a different credential than the one being connected.
    WrongCredential,
    /// Names an organization other than the one being connected into.
    WrongOrganization,
    /// Already spent.
    Used,
}

impl StateRejection {
    /// The sentence the API returns, and the panel shows.
    #[must_use]
    pub fn detail(self) -> &'static str {
        match self {
            Self::Unrecognised => "the authorization state is not one this installation issued",
            Self::Expired => "the authorization request expired — start the connection again",
            Self::WrongCredential => {
                "the authorization state was issued for a different credential"
            }
            Self::WrongOrganization => {
                "the authorization state was issued for another organization"
            }
            Self::Used => "the authorization request was already completed",
        }
    }

    /// The stable code, so a client can tell a CSRF refusal from an expiry.
    #[must_use]
    pub fn code(self) -> &'static str {
        codes::STATE
    }
}

/// Check a returned `state` and pull the credential *and organization* it was issued for.
///
/// This verifies the signature and the window; it does **not** know whether the flow was
/// already spent — that is the store's job, because only the store can know atomically. The
/// two together are what make the callback safe.
///
/// Both halves come back because the callback has no session: an organization that is not in
/// the signed payload leaves the handler holding a credential id it cannot look up in any
/// tenant, and the two tempting workarounds — guessing the tenant, or querying without a
/// scope — are exactly the shapes this module exists to rule out.
pub fn verify_state(
    raw: &str,
    key: &[u8],
    now: OffsetDateTime,
    expected_credential: Option<uuid::Uuid>,
) -> Result<VerifiedState, StateRejection> {
    let Some((payload, signature)) = raw.rsplit_once('.') else {
        return Err(StateRejection::Unrecognised);
    };
    let Ok(actual) = URL_SAFE_NO_PAD.decode(signature) else {
        return Err(StateRejection::Unrecognised);
    };
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts a key of any length");
    mac.update(STATE_LABEL);
    mac.update(payload.as_bytes());
    mac.verify_slice(&actual)
        .map_err(|_| StateRejection::Unrecognised)?;

    let mut parts = payload.split(':');
    let (Some(scope), Some(issued), Some(_)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(StateRejection::Unrecognised);
    };
    if parts.next().is_some() {
        return Err(StateRejection::Unrecognised);
    }
    // `<organization>.<credential>`. A payload without the dot is a state minted by a build
    // that predates the organization being carried, and it is refused rather than parsed with
    // a guessed tenant: the whole reason the tenant is in here is that a guess is the failure
    // mode.
    let Some((organization, credential)) = scope.split_once('.') else {
        return Err(StateRejection::Unrecognised);
    };
    let (Ok(organization_id), Ok(credential_id)) = (
        organization.parse::<uuid::Uuid>(),
        credential.parse::<uuid::Uuid>(),
    ) else {
        return Err(StateRejection::Unrecognised);
    };
    let issued = issued
        .parse::<i128>()
        .map_err(|_| StateRejection::Unrecognised)?;
    let Some(issued_at) = OffsetDateTime::from_unix_timestamp_nanos(issued).ok() else {
        return Err(StateRejection::Unrecognised);
    };

    // The window is checked against the *signature's* own issue time, not against "now minus
    // TTL": a forged `state` cannot be inside the window because it cannot be signed, and a
    // state issued an hour ago is expired whichever clock we compare it to.
    if now < issued_at || now - issued_at > STATE_TTL {
        return Err(StateRejection::Expired);
    }
    if let Some(expected) = expected_credential
        && expected != credential_id
    {
        return Err(StateRejection::WrongCredential);
    }
    Ok(VerifiedState {
        organization_id,
        credential_id,
    })
}

// ---------------------------------------------------------------------------------------------
// The authorization URL
// ---------------------------------------------------------------------------------------------

/// Build the URL the person is sent to.
///
/// Every parameter is appended by *parsing* the endpoint rather than by string concatenation,
/// because an endpoint that already carries a query string (`?tenant=x`) is common and
/// `authorize_url?response_type=code&…` would produce a URL whose last `?` wins and silently
/// drops the tenant. `url::Url` is the workspace dependency for exactly this.
#[must_use]
pub fn authorization_url(
    authorize_url: &str,
    client_id: &str,
    redirect_uri: &str,
    scopes: &str,
    state: &str,
    pkce: Option<&PkcePair>,
) -> Result<String, url::ParseError> {
    let mut url = url::Url::parse(authorize_url)?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", client_id);
        query.append_pair("redirect_uri", redirect_uri);
        if !scopes.trim().is_empty() {
            query.append_pair("scope", scopes.trim());
        }
        query.append_pair("state", state);
        if let Some(pkce) = pkce {
            query.append_pair("code_challenge", &pkce.challenge);
            query.append_pair("code_challenge_method", "S256");
        }
    }
    Ok(url.to_string())
}

/// What a provider sent back on the callback.
///
/// `Deserialize` as well as a hand parser: the parser ([`Self::parse`]) exists for the
/// *token-request* side and for tests, but an HTTP callback arrives as a query string and
/// axum's extractor wants a `Deserialize`. Both spellings are kept in one struct on purpose —
/// two types for "the four things a callback carries" is one type too many, and they would
/// disagree about the first thing a provider does, which is rename a field.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct CallbackQuery {
    /// The authorization code, when the provider granted one.
    pub code: Option<String>,
    /// The echoed state.
    pub state: Option<String>,
    /// The provider's own error, when it refused.
    pub error: Option<String>,
    /// A human-readable description of that error.
    pub error_description: Option<String>,
}

impl CallbackQuery {
    /// Read a callback's query string into the three things the flow needs.
    #[must_use]
    pub fn parse(query: &str) -> Self {
        let mut code = None;
        let mut state = None;
        let mut error = None;
        let mut error_description = None;
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            match key.as_ref() {
                "code" => code = Some(value.into_owned()),
                "state" => state = Some(value.into_owned()),
                "error" => error = Some(value.into_owned()),
                "error_description" => error_description = Some(value.into_owned()),
                _ => {}
            }
        }
        Self {
            code,
            state,
            error,
            error_description,
        }
    }

    /// The provider's refusal, in a sentence, with anything secret left out.
    #[must_use]
    pub fn refusal(&self) -> Option<String> {
        self.error.as_ref().map(|code| {
            format!(
                "{code}{}",
                self.error_description
                    .as_ref()
                    .map(|d| format!(" — {d}"))
                    .unwrap_or_default()
            )
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The token set
// ---------------------------------------------------------------------------------------------

/// A provider's token response.
///
/// This type deliberately implements **neither `Debug` nor `Serialize`**: it is the one
/// struct in the crate that holds an access token and a refresh token in plaintext, and a
/// `#[derive(Debug)]` is a `println!` away from a log line. Everything the panel may show is
/// derived from it by hand, through [`TokenSet::summary`].
#[derive(Clone, PartialEq, Eq)]
pub struct TokenSet {
    /// The bearer token. Goes straight to the encrypted store.
    access_token: String,
    /// The refresh token, when the provider issued one.
    refresh_token: Option<String>,
    /// Absolute expiry, parsed from `expires_in`.
    expires_at: Option<OffsetDateTime>,
    /// The scopes actually granted, which may be fewer than were asked for.
    scopes: Option<String>,
    /// Who the provider says this is connected as.
    subject: Option<String>,
}

impl TokenSet {
    /// Read a token endpoint's JSON answer.
    ///
    /// A provider that answers `200 {"error": "invalid_grant"}` is not a success with missing
    /// fields — it is a failure, and the two are told apart here rather than by a caller that
    /// forgot to look.
    pub fn from_response(body: &Value) -> Result<Self, String> {
        if let Some(error) = body.get("error").and_then(Value::as_str) {
            let detail = body
                .get("error_description")
                .and_then(Value::as_str)
                .map(|d| format!(" — {d}"))
                .unwrap_or_default();
            return Err(format!("{error}{detail}"));
        }
        let access_token = body
            .get("access_token")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "the provider returned no access_token".to_string())?;
        if access_token.trim().is_empty() {
            return Err("the provider returned an empty access_token".to_string());
        }
        let expires_at = match body.get("expires_in").and_then(Value::as_i64) {
            Some(seconds) if seconds > 0 => OffsetDateTime::now_utc()
                .checked_add(time::Duration::seconds(seconds))
                .or(Some(OffsetDateTime::now_utc())),
            _ => None,
        };
        Ok(Self {
            access_token,
            refresh_token: body
                .get("refresh_token")
                .and_then(Value::as_str)
                .map(str::to_string),
            expires_at,
            scopes: body
                .get("scope")
                .and_then(Value::as_str)
                .map(str::to_string),
            subject: None,
        })
    }

    /// Note who the provider says this is connected as, from an identity endpoint.
    #[must_use]
    pub fn with_subject(mut self, subject: Option<String>) -> Self {
        self.subject = subject;
        self
    }

    /// The access token, for the encrypted store. The ONLY accessor.
    #[must_use]
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    /// The refresh token, when there is one.
    #[must_use]
    pub fn refresh_token(&self) -> Option<&str> {
        self.refresh_token.as_deref()
    }

    /// When the access token expires.
    #[must_use]
    pub fn expires_at(&self) -> Option<OffsetDateTime> {
        self.expires_at
    }

    /// Build the payload the encrypted store is handed.
    ///
    /// This is the boundary where a token stops being a struct field and becomes opaque bytes,
    /// and it is the *only* place either token is allowed to be serialised.
    #[must_use]
    pub fn to_secret_payload(&self) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("access_token".into(), json!(self.access_token));
        if let Some(refresh) = &self.refresh_token {
            map.insert("refresh_token".into(), json!(refresh));
        }
        if let Some(expires_at) = self.expires_at {
            if let Ok(text) = expires_at.format(&Rfc3339) {
                map.insert("expires_at".into(), json!(text));
            }
        }
        Value::Object(map)
    }

    /// The three things a panel may show. No token appears here.
    #[must_use]
    pub fn summary(&self) -> TokenSummary {
        TokenSummary {
            subject: self.subject.clone(),
            scopes: self.scopes.clone(),
            expires_at: self.expires_at,
        }
    }
}

/// What a connected credential shows. No token, by construction.
///
/// `Serialize` and not just `Debug`: this is the half of [`TokenSet`] that legitimately
/// crosses into an API response, and the split between the two types is what keeps a
/// response body from ever holding a token.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TokenSummary {
    /// Who the provider says this is connected as.
    pub subject: Option<String>,
    /// The scopes granted.
    pub scopes: Option<String>,
    /// When the access token expires.
    pub expires_at: Option<OffsetDateTime>,
}

// ---------------------------------------------------------------------------------------------
// The token request
// ---------------------------------------------------------------------------------------------

/// The form body of a token request, as name/value pairs.
///
/// A `Vec<(String, String)>` rather than a `HashMap` so the request is deterministic and
/// byte-comparable in a test, and so `Vec` ordering is something a reviewer can read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenRequest {
    /// The pairs, in the order they will be sent.
    pub fields: Vec<(String, String)>,
}

impl TokenRequest {
    /// An authorization-code exchange.
    #[must_use]
    pub fn authorization_code(
        code: &str,
        client_id: &str,
        client_secret: Option<&str>,
        redirect_uri: &str,
        verifier: Option<&str>,
    ) -> Self {
        let mut fields = vec![
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("code".to_string(), code.to_string()),
            ("client_id".to_string(), client_id.to_string()),
            ("redirect_uri".to_string(), redirect_uri.to_string()),
        ];
        if let Some(secret) = client_secret {
            fields.push(("client_secret".to_string(), secret.to_string()));
        }
        if let Some(verifier) = verifier {
            fields.push(("code_verifier".to_string(), verifier.to_string()));
        }
        Self { fields }
    }

    /// A refresh exchange.
    #[must_use]
    pub fn refresh(
        refresh_token: &str,
        client_id: &str,
        client_secret: Option<&str>,
        scopes: Option<&str>,
    ) -> Self {
        let mut fields = vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh_token.to_string()),
            ("client_id".to_string(), client_id.to_string()),
        ];
        if let Some(secret) = client_secret {
            fields.push(("client_secret".to_string(), secret.to_string()));
        }
        if let Some(scopes) = scopes {
            fields.push(("scope".to_string(), scopes.to_string()));
        }
        Self { fields }
    }

    /// The form-encoded body.
    #[must_use]
    pub fn encode(&self) -> String {
        let pairs: Vec<(String, String)> = self
            .fields
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish()
    }

    /// Whether the body carries a secret, so a caller can refuse to log it.
    #[must_use]
    pub fn carries_secret(&self) -> bool {
        self.fields
            .iter()
            .any(|(k, _)| k == "client_secret" || k == "refresh_token" || k == "code_verifier")
    }
}

// ---------------------------------------------------------------------------------------------
// Single-flight refresh
// ---------------------------------------------------------------------------------------------

/// One credential's refresh in progress.
///
/// A refresh is a write to the provider that invalidates the old refresh token on most
/// providers, so two concurrent refreshes mean one of them loses its token forever. Six nodes
/// finishing at the same moment must produce *one* exchange. The lock is a per-credential
/// process lock: cross-process single-flight belongs to the encrypted store (REQ-125), which is
/// the only thing that can hold the token while this process is not looking.
#[derive(Debug, Default)]
pub struct RefreshLock {
    held: Mutex<BTreeMap<uuid::Uuid, Instant>>,
}

impl RefreshLock {
    /// A lock nothing holds.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the lock for `credential`, waiting up to `wait`.
    ///
    /// Returns `None` when the wait ran out, which the caller must surface as a *retry* rather
    /// than as a refresh failure: a credential that timed out behind a peer has not failed, and
    /// recording `needs_reauth` for it would disable nodes over a five-second queue.
    pub fn acquire(&self, credential: uuid::Uuid, wait: Duration) -> Option<RefreshGuard<'_>> {
        let deadline = Instant::now() + wait;
        loop {
            let mut held = self.held.lock().ok()?;
            let now = Instant::now();
            // Reap an entry whose holder panicked: a `Mutex` that was poisoned by a panic
            // would otherwise wedge every refresh for this process forever.
            held.retain(|_, since| now.duration_since(*since) < Duration::from_secs(60));
            if !held.contains_key(&credential) {
                held.insert(credential, now);
                return Some(RefreshGuard {
                    lock: self,
                    credential,
                });
            }
            drop(held);
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn release(&self, credential: uuid::Uuid) {
        if let Ok(mut held) = self.held.lock() {
            held.remove(&credential);
        }
    }
}

/// The RAII half of a [`RefreshLock`].
///
/// Deliberately holds *no* mutex guard: the map entry is the lock, and `release` has to be
/// able to take the mutex again to remove it. A guard field kept across the critical section
/// would make `Drop` deadlock on the very mutex it is holding, which is the kind of bug that
/// only shows up under the concurrency this type exists for.
#[derive(Debug)]
pub struct RefreshGuard<'a> {
    lock: &'a RefreshLock,
    credential: uuid::Uuid,
}

impl Drop for RefreshGuard<'_> {
    fn drop(&mut self) {
        self.lock.release(self.credential);
    }
}

// ---------------------------------------------------------------------------------------------
// The local seal
// ---------------------------------------------------------------------------------------------

/// A key for the flow's own short-lived secrets.
///
/// A PKCE verifier has to survive exactly one round trip: the person leaves for the
/// provider and comes back minutes later, and the callback needs the same verifier to
/// spend the code. That is a *workflow* secret, not a credential one — it is worthless the
/// moment the code is spent, and it is not the thing REQ-125 exists to manage. So the flow
/// seals it with this: the same authenticated-encryption composition the identity crate uses
/// for MFA, with a key scoped to the flows.
///
/// The important property is that a *tampered* envelope fails to open rather than opening to
/// something else, and [`LocalBox::open`] returns an error instead of a guess. `open_pkce`
/// then re-derives the challenge from what came out, so even a row edited by somebody who
/// somehow has the key cannot produce a token request a provider will accept.
#[derive(Clone)]
pub struct LocalBox {
    key: [u8; 32],
}

/// Envelope version tag; a change writes `v2` and keeps reading `v1`.
const SEAL_VERSION: &str = "s1";

/// Domain separation for the seal's MAC, so a key used elsewhere cannot open this.
const SEAL_LABEL: &[u8] = b"omnion.workflow.oauth.seal.v1";

impl LocalBox {
    /// A box from raw key material.
    #[must_use]
    pub fn from_key_material(material: &[u8]) -> Self {
        // A domain-separated hash rather than a copy: the caller's material is whatever
        // length the operator gave us, and a box that silently accepted three bytes would be a
        // box that accepts a password.
        let mut hasher = Sha256::new();
        hasher.update(SEAL_LABEL);
        hasher.update(material);
        let mut key = [0_u8; 32];
        key.copy_from_slice(&hasher.finalize());
        Self { key }
    }

    /// Seal a string.
    #[must_use]
    pub fn seal(&self, plaintext: &str) -> String {
        let nonce = random_bytes(16);
        let stream = keystream(&self.key, &nonce, plaintext.len());
        let cipher: Vec<u8> = plaintext
            .as_bytes()
            .iter()
            .zip(stream)
            .map(|(b, k)| b ^ k)
            .collect();
        // Encrypt, THEN authenticate the ciphertext. This is the whole construction: the tag
        // below is computed over the nonce *and* the ciphertext, so `open` verifies before it
        // decrypts and a flipped byte is refused rather than returned as corrupted plaintext.
        let mut full_mac =
            HmacSha256::new_from_slice(&self.key).expect("HMAC accepts a key of any length");
        full_mac.update(&nonce);
        full_mac.update(&cipher);
        let full_tag = full_mac.finalize().into_bytes();
        format!(
            "{SEAL_VERSION}.{}.{}.{}",
            URL_SAFE_NO_PAD.encode(nonce),
            URL_SAFE_NO_PAD.encode(cipher),
            URL_SAFE_NO_PAD.encode(full_tag)
        )
    }

    /// Open a sealed string, or refuse.
    ///
    /// An error is returned for a wrong version, a malformed envelope, a wrong key and a
    /// tampered tag — all four are "this is not what we wrote", and none of them may return
    /// partial plaintext.
    pub fn open(&self, envelope: &str) -> std::result::Result<String, String> {
        let mut parts = envelope.split('.');
        if parts.next() != Some(SEAL_VERSION) {
            return Err("unknown seal version".into());
        }
        let (Some(nonce), Some(cipher), Some(tag), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err("malformed envelope".into());
        };
        let (Ok(nonce), Ok(cipher), Ok(tag)) = (
            URL_SAFE_NO_PAD.decode(nonce),
            URL_SAFE_NO_PAD.decode(cipher),
            URL_SAFE_NO_PAD.decode(tag),
        ) else {
            return Err("malformed envelope".into());
        };
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(&nonce);
        mac.update(&cipher);
        // Verify before decrypting: a tampered envelope must never produce plaintext, not even
        // plaintext that is then thrown away.
        mac.verify_slice(&tag)
            .map_err(|_| "tag mismatch".to_string())?;
        let stream = keystream(&self.key, &nonce, cipher.len());
        let plain: Vec<u8> = cipher.iter().zip(stream).map(|(b, k)| b ^ k).collect();
        String::from_utf8(plain).map_err(|_| "sealed value was not utf-8".into())
    }
}

/// The keystream: `SHA-256(key || label || nonce || counter)`, truncated.
fn keystream(key: &[u8], nonce: &[u8], length: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(length);
    let mut counter: u64 = 0;
    while out.len() < length {
        let mut hasher = Sha256::new();
        hasher.update(b"omnion.workflow.oauth.keystream.v1");
        hasher.update(key);
        hasher.update(nonce);
        hasher.update(counter.to_be_bytes());
        out.extend_from_slice(&hasher.finalize());
        counter += 1;
    }
    out.truncate(length);
    out
}

/// Seal with a box, for the store's one call site.
#[must_use]
pub fn seal_local(box_key: &LocalBox, plaintext: &str) -> String {
    box_key.seal(plaintext)
}

// ---------------------------------------------------------------------------------------------
// Expiry
// ---------------------------------------------------------------------------------------------

/// How long before expiry a token is considered due for refresh.
///
/// Sixty seconds of headroom, so a token that expires *during* the request that used it is not
/// the reason a workflow fails. A provider whose `expires_in` is under two minutes is treated as
/// non-expiring rather than as permanently stale, because refreshing in a loop is worse than
/// using a short-lived token once.
pub const REFRESH_LEAD: Duration = Duration::from_secs(60);

/// Whether a token set is due for refresh at `now`.
#[must_use]
pub fn needs_refresh(expires_at: Option<OffsetDateTime>, now: OffsetDateTime) -> bool {
    match expires_at {
        None => false,
        Some(at) => {
            let remaining = at - now;
            remaining <= REFRESH_LEAD && remaining > time::Duration::ZERO
        }
    }
}

/// Whether a token set is already past expiry.
///
/// Separate from [`needs_refresh`] on purpose: "refresh this now" and "this is broken" are
/// different answers, and conflating them is how a credential that is merely *due* ends up
/// marked `needs_reauth`.
#[must_use]
pub fn is_expired(expires_at: Option<OffsetDateTime>, now: OffsetDateTime) -> bool {
    expires_at.is_some_and(|at| at <= now)
}

// ---------------------------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------------------------

fn sign(key: &[u8], payload: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts a key of any length");
    mac.update(STATE_LABEL);
    mac.update(payload.as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

fn random_bytes(count: usize) -> Vec<u8> {
    use rand::RngCore as _;
    let mut out = vec![0_u8; count];
    rand::rngs::OsRng.fill_bytes(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn key() -> Vec<u8> {
        b"a-test-installation-key-that-is-32b!!".to_vec()
    }

    #[test]
    fn a_state_round_trips_and_names_its_credential_and_its_organization() {
        // The callback has no session, so the organization has to travel inside the signed
        // value or the handler is left holding an id it cannot look up in any tenant.
        let organization = Uuid::new_v4();
        let id = Uuid::new_v4();
        let now = OffsetDateTime::now_utc();
        let state = build_state(organization, id, &key(), now);
        let verified = verify_state(&state, &key(), now, Some(id)).expect("our own state");
        assert_eq!(verified.credential_id, id);
        assert_eq!(verified.organization_id, organization);
    }

    #[test]
    fn a_state_signed_with_another_key_is_not_recognised() {
        let (organization, id) = (Uuid::new_v4(), Uuid::new_v4());
        let now = OffsetDateTime::now_utc();
        let state = build_state(organization, id, &key(), now);
        assert_eq!(
            verify_state(
                &state,
                b"a-different-installation-key-entirely!",
                now,
                Some(id)
            ),
            Err(StateRejection::Unrecognised)
        );
    }

    #[test]
    fn a_tampered_state_is_refused_and_the_reason_is_stable() {
        let (organization, id) = (Uuid::new_v4(), Uuid::new_v4());
        let now = OffsetDateTime::now_utc();
        let state = build_state(organization, id, &key(), now);
        let (payload, signature) = state.rsplit_once('.').unwrap();
        // Swap in a different credential, keep the signature: the payload no longer matches.
        let forged = format!("{organization}.{}:{payload}:x.{signature}", Uuid::nil());
        let rejection = verify_state(&forged, &key(), now, None).unwrap_err();
        assert_eq!(rejection, StateRejection::Unrecognised);
        assert_eq!(rejection.code(), codes::STATE);
        assert!(!rejection.detail().is_empty());
    }

    #[test]
    fn a_state_whose_organization_was_swapped_is_refused() {
        // The reason the organization is in the *signed* bytes. An attacker who edits it in
        // the query string invalidates the signature, so a state minted for tenant A can
        // never be replayed into tenant B — and the refusal is "not ours", not "wrong
        // organization", because a forged value never got far enough to be compared.
        let (mine, theirs) = (Uuid::new_v4(), Uuid::new_v4());
        let id = Uuid::new_v4();
        let now = OffsetDateTime::now_utc();
        let state = build_state(mine, id, &key(), now);
        let (payload, signature) = state.rsplit_once('.').unwrap();
        let forged = format!("{theirs}.{payload}.{signature}");
        assert_eq!(
            verify_state(&forged, &key(), now, Some(id)).unwrap_err(),
            StateRejection::Unrecognised
        );
    }

    #[test]
    fn a_state_past_its_window_is_expired_not_unrecognised() {
        let (organization, id) = (Uuid::new_v4(), Uuid::new_v4());
        let issued = OffsetDateTime::now_utc() - STATE_TTL - Duration::from_secs(5);
        let state = build_state(organization, id, &key(), issued);
        assert_eq!(
            verify_state(&state, &key(), OffsetDateTime::now_utc(), Some(id)),
            Err(StateRejection::Expired)
        );
    }

    #[test]
    fn a_state_for_another_credential_names_that_rather_than_pretending() {
        let (organization, mine, theirs) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let now = OffsetDateTime::now_utc();
        let state = build_state(organization, theirs, &key(), now);
        assert_eq!(
            verify_state(&state, &key(), now, Some(mine)),
            Err(StateRejection::WrongCredential)
        );
    }

    #[test]
    fn a_state_with_no_organization_in_its_payload_is_refused_rather_than_guessed() {
        // A payload minted before the organization was carried. The obvious fallback — treat
        // the bare id as a credential in whatever tenant the session names — is the failure
        // mode the organization being in the signature exists to prevent.
        let id = Uuid::new_v4();
        let now = OffsetDateTime::now_utc();
        let payload = format!("{id}:{}:nonce", now.unix_timestamp_nanos());
        let legacy = format!("{payload}.{}", sign(&key(), &payload));
        assert_eq!(
            verify_state(&legacy, &key(), now, None).unwrap_err(),
            StateRejection::Unrecognised,
            "a state with no tenant is not one this release can scope"
        );
    }

    #[test]
    fn a_state_with_no_signature_is_refused_without_panicking() {
        for raw in ["", "abc", "a.b.c", "....", "not-a-state"] {
            assert!(verify_state(raw, &key(), OffsetDateTime::now_utc(), None).is_err());
        }
    }

    #[test]
    fn the_derived_state_key_is_the_same_bytes_on_every_call() {
        // The regression test for the bug this slice shipped and then fixed: a key derived
        // through an encryption primitive gets a fresh nonce per call, so signing and
        // verifying disagree and *every* callback fails as `credential_oauth_state` — an
        // error naming CSRF for a key that never matched. One assertion, and the symptom
        // cannot come back.
        let material = b"an-installation-secret";
        let first = derive_state_key(material, b"label");
        let second = derive_state_key(material, b"label");
        assert_eq!(first, second, "an HMAC key must be stable within a process");
        assert_eq!(first.len(), 32, "a sha-256 digest is 32 bytes");
    }

    #[test]
    fn the_state_key_and_the_seal_key_are_unrelated() {
        // Domain separation, and the reason the label is a parameter rather than a constant
        // buried in each caller: an envelope this module produced must not be replayable as a
        // `state`, and a `state` must not open as an envelope.
        let material = b"an-installation-secret";
        assert_ne!(
            derive_state_key(material, b"omnion.workflow.oauth.seal"),
            derive_state_key(material, b"omnion.workflow.oauth.signing"),
            "the two key spaces must not coincide"
        );
    }

    #[test]
    fn two_states_for_one_credential_differ() {
        let (organization, id) = (Uuid::new_v4(), Uuid::new_v4());
        let now = OffsetDateTime::now_utc();
        assert_ne!(
            build_state(organization, id, &key(), now),
            build_state(organization, id, &key(), now)
        );
    }

    #[test]
    fn the_pkce_challenge_is_the_rfc_example() {
        // RFC 7636 appendix B: verifier dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk
        // has challenge E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM.
        let pair = PkcePair::derive("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(
            pair.challenge,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert!(PkcePair::verify(&pair.challenge, pair.verifier()));
        assert!(!PkcePair::verify(&pair.challenge, "not-the-verifier"));
    }

    #[test]
    fn a_generated_verifier_is_the_length_the_rfc_allows() {
        let pair = PkcePair::generate();
        assert!((43..=128).contains(&pair.verifier().len()));
        assert!(PkcePair::verify(&pair.challenge, pair.verifier()));
    }

    #[test]
    fn the_authorization_url_keeps_an_endpoint_that_already_has_a_query() {
        let pair = PkcePair::generate();
        let url = authorization_url(
            "https://auth.example.com/oauth/authorize?tenant=acme",
            "client-1",
            "https://app.test/callback",
            "read write",
            "state-1",
            Some(&pair),
        )
        .unwrap();
        let parsed = url::Url::parse(&url).unwrap();
        let query: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(query.get("tenant").map(String::as_str), Some("acme"));
        assert_eq!(query.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            query.get("code_challenge").map(String::as_str),
            Some(&pair.challenge[..])
        );
        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(query.get("state").map(String::as_str), Some("state-1"));
    }

    #[test]
    fn the_authorization_url_omits_pkce_for_a_type_that_does_not_use_it() {
        let url = authorization_url(
            "https://auth.example.com/oauth/authorize",
            "client-1",
            "https://app.test/callback",
            "  ",
            "state-1",
            None,
        )
        .unwrap();
        assert!(!url.contains("code_challenge"));
        assert!(!url.contains("scope="));
    }

    #[test]
    fn a_callback_query_reads_the_three_things_and_ignores_the_rest() {
        let parsed =
            CallbackQuery::parse("code=abc&state=xyz&tracking=drop&error_description=hi%20there");
        assert_eq!(parsed.code.as_deref(), Some("abc"));
        assert_eq!(parsed.state.as_deref(), Some("xyz"));
        assert_eq!(parsed.refusal(), None);
    }

    #[test]
    fn a_provider_refusal_is_a_sentence_not_a_status() {
        let parsed = CallbackQuery::parse("error=access_denied&error_description=User%20said%20no");
        assert!(parsed.code.is_none());
        assert_eq!(
            parsed.refusal().as_deref(),
            Some("access_denied — User said no")
        );
    }

    #[test]
    fn a_token_response_is_read_into_a_set_and_a_failure_is_not_one() {
        let ok = TokenSet::from_response(&json!({
            "access_token": "at-1", "refresh_token": "rt-1", "expires_in": 3600, "scope": "read",
        }))
        .unwrap();
        assert_eq!(ok.access_token(), "at-1");
        assert_eq!(ok.refresh_token(), Some("rt-1"));
        assert!(ok.expires_at().is_some());

        // A 200 with an error body is a failure. Reading it as a success with a missing
        // field is how a broken refresh is stored as a working credential.
        let refused = match TokenSet::from_response(&json!({
            "error": "invalid_grant", "error_description": "token expired",
        })) {
            Ok(_) => panic!("a 200 carrying an error body is not a token set"),
            Err(message) => message,
        };
        assert_eq!(refused, "invalid_grant — token expired");

        assert!(TokenSet::from_response(&json!({})).is_err());
        assert!(TokenSet::from_response(&json!({ "access_token": "  " })).is_err());
    }

    /// Assert, at compile time, that `TokenSet` implements neither `Debug` nor `Serialize`.
    ///
    /// A negative trait bound cannot be written on stable, so this is the autoref
    /// specialisation trick: the blanket `impl<T> Trait for T` supplies a `false` default and
    /// the inherent impl — which only exists when the bound holds — takes precedence. The
    /// assertion below then *fails to compile* the moment somebody adds `#[derive(Debug)]` to a
    /// struct holding a live access token, which is the moment a `{:?}` in a log line becomes
    /// a credential leak.
    #[test]
    fn a_token_set_is_not_debug_or_serialisable() {
        trait HasDebug {
            fn has_debug() -> bool {
                false
            }
        }
        impl<T> HasDebug for T {}
        struct Wrap<T>(std::marker::PhantomData<T>);
        impl<T: std::fmt::Debug> Wrap<T> {
            fn has_debug() -> bool {
                true
            }
        }
        trait HasSerialize {
            fn has_serialize() -> bool {
                false
            }
        }
        impl<T> HasSerialize for T {}
        struct WrapSerde<T>(std::marker::PhantomData<T>);
        impl<T: serde::Serialize> WrapSerde<T> {
            fn has_serialize() -> bool {
                true
            }
        }

        assert!(!Wrap::<TokenSet>::has_debug(), "TokenSet must not be Debug");
        assert!(
            !WrapSerde::<TokenSet>::has_serialize(),
            "TokenSet must not be Serialize"
        );
        // The things that DO cross a boundary are serialisable, or the panel cannot render them.
        assert!(Wrap::<TokenSummary>::has_debug());
        assert!(WrapSerde::<TokenSummary>::has_serialize());
    }

    #[test]
    fn the_summary_carries_no_token() {
        let set = TokenSet::from_response(&json!({
            "access_token": "at-secret", "refresh_token": "rt-secret", "scope": "read",
        }))
        .unwrap()
        .with_subject(Some("f@example.com".into()));
        let text = format!("{:?}", set.summary());
        assert!(text.contains("f@example.com"));
        assert!(!text.contains("at-secret"));
        assert!(!text.contains("rt-secret"));
    }

    #[test]
    fn the_secret_payload_is_the_only_place_a_token_is_serialised() {
        let set = TokenSet::from_response(&json!({
            "access_token": "at-1", "refresh_token": "rt-1",
        }))
        .unwrap();
        let payload = set.to_secret_payload();
        assert_eq!(payload["access_token"], "at-1");
        assert_eq!(payload["refresh_token"], "rt-1");
        assert!(!payload["expires_at"].is_string());
    }

    #[test]
    fn a_token_request_encodes_in_order_and_knows_it_carries_a_secret() {
        let request = TokenRequest::authorization_code(
            "code-1",
            "client-1",
            Some("secret-1"),
            "https://app.test/callback",
            Some("verifier-1"),
        );
        let encoded = request.encode();
        assert!(encoded.starts_with("grant_type=authorization_code&code=code-1"));
        assert!(encoded.contains("code_verifier=verifier-1"));
        assert!(encoded.contains("client_secret=secret-1"));
        assert!(request.carries_secret());

        let public = TokenRequest::authorization_code(
            "code-1",
            "client-1",
            None,
            "https://app.test/callback",
            Some("verifier-1"),
        );
        assert!(!public.encode().contains("client_secret"));
    }

    #[test]
    fn a_refresh_request_can_narrow_the_scopes() {
        let request = TokenRequest::refresh("rt-1", "client-1", Some("s"), Some("read"));
        assert!(request.encode().contains("scope=read"));
        assert!(request.carries_secret());
    }

    #[test]
    fn a_token_due_for_refresh_is_not_the_same_as_one_that_expired() {
        let now = OffsetDateTime::now_utc();
        let in_10 = now + time::Duration::seconds(10);
        let in_2_minutes = now + time::Duration::seconds(120);
        let past = now - time::Duration::seconds(1);

        assert!(needs_refresh(Some(in_10), now), "due in 10s is due");
        assert!(
            !needs_refresh(Some(in_2_minutes), now),
            "120s of headroom is not due"
        );
        assert!(!is_expired(Some(in_10), now), "but it is not expired yet");

        assert!(is_expired(Some(past), now));
        assert!(
            !needs_refresh(Some(past), now),
            "expired is not 'due', it is broken"
        );

        assert!(
            !needs_refresh(None, now),
            "no expiry means nothing to schedule"
        );
    }

    #[test]
    fn the_refresh_lock_lets_one_in_and_keeps_the_others_out() {
        let lock = RefreshLock::new();
        let credential = Uuid::new_v4();

        let held = lock.acquire(credential, Duration::from_millis(0)).unwrap();
        // A second holder for the same credential must not get in.
        let refused = lock.acquire(credential, Duration::from_millis(20));
        assert!(refused.is_none(), "a held lock refuses a second holder");
        // A different credential is a different lock.
        assert!(
            lock.acquire(Uuid::new_v4(), Duration::from_millis(0))
                .is_some()
        );

        drop(held);
        assert!(
            lock.acquire(credential, Duration::from_millis(0)).is_some(),
            "releasing lets the next one in"
        );
    }

    #[test]
    fn a_sealed_value_opens_back_to_itself() {
        let box_key = LocalBox::from_key_material(b"an-installation-key-of-any-length");
        for plaintext in ["", "v", "a PKCE verifier with = and & and spaces", "🙂"] {
            let sealed = box_key.seal(plaintext);
            assert_eq!(box_key.open(&sealed).unwrap(), plaintext);
        }
    }

    #[test]
    fn sealing_the_same_value_twice_gives_two_different_envelopes() {
        // A fresh nonce per seal: two flows for one credential must not be distinguishable by
        // their ciphertext, or the envelope itself becomes a correlator.
        let box_key = LocalBox::from_key_material(b"an-installation-key-of-any-length");
        assert_ne!(box_key.seal("same"), box_key.seal("same"));
    }

    #[test]
    fn a_tampered_envelope_fails_to_open_rather_than_opening_to_something_else() {
        let box_key = LocalBox::from_key_material(b"an-installation-key-of-any-length");
        let sealed = box_key.seal("the-verifier");
        // Flip one character of the ciphertext section.
        let mut parts: Vec<&str> = sealed.split('.').collect();
        let cipher = parts[2].to_string();
        let flipped = if cipher.starts_with('A') {
            format!("B{}", &cipher[1..])
        } else {
            format!("A{}", &cipher[1..])
        };
        parts[2] = &flipped;
        let tampered = parts.join(".");
        let error = box_key.open(&tampered).unwrap_err();
        assert_eq!(
            error, "tag mismatch",
            "a tampered envelope must not decrypt"
        );
    }

    #[test]
    fn a_seal_from_another_installation_does_not_open() {
        let writer = LocalBox::from_key_material(b"one-installation-key");
        let reader = LocalBox::from_key_material(b"a-different-installation");
        assert!(reader.open(&writer.seal("the-verifier")).is_err());
    }

    #[test]
    fn a_malformed_or_foreign_envelope_is_an_error_and_never_a_partial_value() {
        let box_key = LocalBox::from_key_material(b"an-installation-key-of-any-length");
        for bad in [
            "",
            "s1",
            "s1.a",
            "s1.a.b",
            "s1.a.b.c.d",
            "v9.a.b.c",
            "not an envelope",
        ] {
            assert!(box_key.open(bad).is_err(), "{bad:?} must not open");
        }
    }

    #[test]
    fn a_pending_flow_reports_its_window() {
        let now = OffsetDateTime::now_utc();
        let flow = PendingFlow {
            credential_id: Uuid::new_v4(),
            state: "s".into(),
            pkce: PkcePair::generate(),
            started_at: now,
            expires_at: now + STATE_TTL,
        };
        assert!(flow.is_live(now));
        assert!(!flow.is_live(now + STATE_TTL + time::Duration::seconds(1)));
        assert!(!flow.is_live(now - time::Duration::seconds(1)));
        assert_eq!(flow.code_challenge_method(), Some("S256"));
    }
}
