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
/// The value is `<payload>.<signature>`, where the payload is `credential_id:issued_at:nonce`
/// and the signature is `HMAC-SHA256(key, label || payload)`. The key is the installation's,
/// so a `state` this function did not mint — one from another installation, or one edited in
/// the query string — does not verify.
#[must_use]
pub fn build_state(credential_id: uuid::Uuid, key: &[u8], now: OffsetDateTime) -> String {
    let payload = format!(
        "{credential_id}:{}:{}",
        now.unix_timestamp_nanos(),
        URL_SAFE_NO_PAD.encode(random_bytes(STATE_NONCE_BYTES))
    );
    format!("{payload}.{}", sign(key, &payload))
}

/// Why a `state` was refused.
///
/// The three cases a caller must tell apart are deliberately distinct: "this is not ours" is a
/// CSRF attempt, "this expired" is a person who took too long, and "this is not yours" is
/// somebody replaying their own valid state against a different credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateRejection {
    /// Absent, malformed, or signed with a different key.
    Unrecognised,
    /// Well-formed and correctly signed, but past its window.
    Expired,
    /// Names a different credential than the one being connected.
    WrongCredential,
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
            Self::WrongCredential => "the authorization state was issued for a different credential",
            Self::Used => "the authorization request was already completed",
        }
    }

    /// The stable code, so a client can tell a CSRF refusal from an expiry.
    #[must_use]
    pub fn code(self) -> &'static str {
        codes::STATE
    }
}

/// Check a returned `state` and pull the credential it was issued for back out.
///
/// This verifies the signature and the window; it does **not** know whether the flow was
/// already spent — that is the store's job, because only the store can know atomically. The
/// two together are what make the callback safe.
pub fn verify_state(
    raw: &str,
    key: &[u8],
    now: OffsetDateTime,
    expected: Option<uuid::Uuid>,
) -> Result<uuid::Uuid, StateRejection> {
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
    let (Some(id), Some(issued), Some(_)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(StateRejection::Unrecognised);
    };
    if parts.next().is_some() {
        return Err(StateRejection::Unrecognised);
    }
    let Ok(credential_id) = id.parse::<uuid::Uuid>() else {
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
    if let Some(expected) = expected
        && expected != credential_id
    {
        return Err(StateRejection::WrongCredential);
    }
    Ok(credential_id)
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    pub fn acquire(
        &self,
        credential: uuid::Uuid,
        wait: Duration,
    ) -> Option<RefreshGuard<'_>> {
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
    fn a_state_round_trips_and_names_its_credential() {
        let id = Uuid::new_v4();
        let now = OffsetDateTime::now_utc();
        let state = build_state(id, &key(), now);
        assert_eq!(verify_state(&state, &key(), now, Some(id)).unwrap(), id);
    }

    #[test]
    fn a_state_signed_with_another_key_is_not_recognised() {
        let id = Uuid::new_v4();
        let now = OffsetDateTime::now_utc();
        let state = build_state(id, &key(), now);
        assert_eq!(
            verify_state(&state, b"a-different-installation-key-entirely!", now, Some(id)),
            Err(StateRejection::Unrecognised)
        );
    }

    #[test]
    fn a_tampered_state_is_refused_and_the_reason_is_stable() {
        let id = Uuid::new_v4();
        let now = OffsetDateTime::now_utc();
        let state = build_state(id, &key(), now);
        let (payload, signature) = state.rsplit_once('.').unwrap();
        // Swap in a different credential, keep the signature: the payload no longer matches.
        let forged = format!("{}:{payload}:x.{signature}", Uuid::nil());
        let rejection = verify_state(&forged, &key(), now, None).unwrap_err();
        assert_eq!(rejection, StateRejection::Unrecognised);
        assert_eq!(rejection.code(), codes::STATE);
        assert!(!rejection.detail().is_empty());
    }

    #[test]
    fn a_state_past_its_window_is_expired_not_unrecognised() {
        let id = Uuid::new_v4();
        let issued = OffsetDateTime::now_utc() - STATE_TTL - Duration::from_secs(5);
        let state = build_state(id, &key(), issued);
        assert_eq!(
            verify_state(&state, &key(), OffsetDateTime::now_utc(), Some(id)),
            Err(StateRejection::Expired)
        );
    }

    #[test]
    fn a_state_for_another_credential_names_that_rather_than_pretending() {
        let mine = Uuid::new_v4();
        let theirs = Uuid::new_v4();
        let now = OffsetDateTime::now_utc();
        let state = build_state(theirs, &key(), now);
        assert_eq!(
            verify_state(&state, &key(), now, Some(mine)),
            Err(StateRejection::WrongCredential)
        );
    }

    #[test]
    fn a_state_with_no_signature_is_refused_without_panicking() {
        for raw in ["", "abc", "a.b.c", "....", "not-a-state"] {
            assert!(verify_state(raw, &key(), OffsetDateTime::now_utc(), None).is_err());
        }
    }

    #[test]
    fn two_states_for_one_credential_differ() {
        let id = Uuid::new_v4();
        let now = OffsetDateTime::now_utc();
        assert_ne!(build_state(id, &key(), now), build_state(id, &key(), now));
    }

    #[test]
    fn the_pkce_challenge_is_the_rfc_example() {
        // RFC 7636 appendix B: verifier dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk
        // has challenge E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM.
        let pair = PkcePair::derive("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(pair.challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
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
        assert_eq!(query.get("code_challenge").map(String::as_str), Some(&pair.challenge[..]));
        assert_eq!(query.get("code_challenge_method").map(String::as_str), Some("S256"));
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
        let parsed = CallbackQuery::parse(
            "code=abc&state=xyz&tracking=drop&error_description=hi%20there",
        );
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
            "code-1", "client-1", None, "https://app.test/callback", Some("verifier-1"),
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
        assert!(!needs_refresh(Some(in_2_minutes), now), "120s of headroom is not due");
        assert!(!is_expired(Some(in_10), now), "but it is not expired yet");

        assert!(is_expired(Some(past), now));
        assert!(!needs_refresh(Some(past), now), "expired is not 'due', it is broken");

        assert!(!needs_refresh(None, now), "no expiry means nothing to schedule");
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
        assert!(lock.acquire(Uuid::new_v4(), Duration::from_millis(0)).is_some());

        drop(held);
        assert!(
            lock.acquire(credential, Duration::from_millis(0)).is_some(),
            "releasing lets the next one in"
        );
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
