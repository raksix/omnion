//! OAuth application material: client ids, client secrets, redirect URIs and PKCE (slice 3).
//!
//! # The three rules this module exists to enforce
//!
//! **1. A client secret is write-only, exactly like an API key.** [`mint_client_secret`]
//! returns it once, [`hash_client_secret`] is the only form that is ever stored, and
//! [`verify_client_secret`] is the only thing that reads one. The struct that carries a
//! plaintext ([`MintedClientSecret`]) has a hand-written `Debug` that prints `<redacted>`,
//! for the same reason [`crate::secret::MintedKey`] does: one `{:?}` in an error path is
//! enough to leak a credential that everything else was careful about.
//!
//! **2. A redirect URI is checked against the *exact* string the app registered.** This is
//! the single most consequential rule in the file and it is the one OAuth implementations
//! most often get wrong. The dangerous shortcut is prefix matching (`starts_with`), and it
//! fails in the direction that matters: an app registered at
//! `https://app.example.com/callback` would accept
//! `https://app.example.com/callback-attacker` and, worse,
//! `https://app.example.com/callback/../../evil` would be normalised by the *client* into a
//! different origin after the check had already passed. So [`check_redirect_uri`] compares
//! whole strings and additionally normalises the two shapes a browser can rewrite — a
//! trailing slash and a `?`/`#` with an empty remainder — before comparing. Anything else
//! has to be registered verbatim.
//!
//! The one exception is the `https`-or-localhost rule from the request: an `http` URI is
//! accepted when its host is exactly `localhost`, `127.0.0.1` or `[::1]`, because that is a
//! developer's own machine and the loopback exception exists in every RFC 8252 device flow.
//! It is deliberately narrow — `http://localhost.attacker.example` is **not** localhost, and
//! checking it with `starts_with("localhost")` is how that hole gets written.
//!
//! **3. PKCE is checked by hashing, never by comparing the verifier to the challenge.** The
//! stored challenge is `BASE64URL(SHA256(verifier))` and the comparison is constant-time, so
//! a code that is intercepted is useless without the verifier, and a timing oracle on the
//! comparison cannot be built from a string compare. `plain` is accepted because the request
//! says authorization code plus PKCE and a caller of a first-party client may send it — but
//! it is accepted *loudly*: [`verify_code_verifier`] reports which method matched, so the
//! authorization endpoint can log that a client chose the weaker one, which is the only way
//! anybody finds out before an audit does.

use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Marker on every stored client-secret hash. Distinct from the API key's scheme prefix even
/// though the digest is the same construction: a row this build cannot read must be an
/// authentication failure rather than a comparison against a hash made some other way, and
/// the two key spaces should never be interchangeable.
const SECRET_SCHEME: &str = "omnion-oauth-secret.v1";

/// The scheme a client id carries. Not a secret — it is stored in the clear and copied into
/// a form — but namespaced so a support conversation can tell an Omnion client from another
/// platform's.
const CLIENT_NAMESPACE: &str = "omn_app_";

/// Bytes of entropy in a client secret. 32 bytes, the same answer as an API key: a client
/// secret is a machine-generated credential with no weak variants to slow down.
const SECRET_BYTES: usize = 32;

/// Bytes of entropy in a generated client id. Shorter than an API key's prefix on purpose —
/// this one is meant to be read aloud to somebody debugging a redirect.
const CLIENT_ID_BYTES: usize = 12;

/// How long an authorization code stays redeemable.
///
/// Ten minutes with a single use. The number is short because the code is a bearer
/// credential for a *user's* session, and the flow that mints it has a person at a consent
/// screen in the middle of it; anything longer widens the window a shoulder-surfer, a
/// browser history entry or a proxy log needs and buys nothing.
pub const CODE_TTL_MINUTES: i64 = 10;

/// How long a rotated client secret keeps working.
///
/// Seven days, capped rather than open-ended. The request asks for an overlap window so a
/// deployment that reads its secret from a config file does not break the instant somebody
/// rotates it, and the risk note says to cap it: every second of overlap is a second during
/// which two credentials are valid, so the window is long enough to redeploy and short
/// enough that the old one is genuinely gone. The expiry is stored per row, so shortening or
/// extending it later is a migration and not a code change.
pub const SECRET_OVERLAP_DAYS: i64 = 7;

/// The longest redirect URI accepted. A bound, because the column is a `text` that a
/// hand-written request can fill with a megabyte of nonsense, and because no legitimate
/// callback is longer than a URL.
pub const MAX_REDIRECT_URI_LENGTH: usize = 512;

/// The most redirect URIs one app may register.
///
/// A bound rather than a limit to hit: the platform's own clients are single-tenant and
/// register one or two, and a form that accepts five hundred rows is a form nobody reads.
pub const MAX_REDIRECT_URIS: usize = 10;

/// A freshly minted client secret. Never stored, never logged, never returned twice.
#[derive(Clone)]
pub struct MintedClientSecret {
    /// What the app owner copies into their client configuration. Shown once.
    pub plaintext: String,
}

impl std::fmt::Debug for MintedClientSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Same reasoning as `MintedKey`: deriving `Debug` would write a live credential into
        // any log that touches this by reference, and a client secret is a credential.
        formatter
            .debug_struct("MintedClientSecret")
            .field("plaintext", &"<redacted>")
            .finish()
    }
}

/// Mint a client secret: 256 bits of CSPRNG entropy, hex encoded.
#[must_use]
pub fn mint_client_secret() -> MintedClientSecret {
    let mut bytes = [0u8; SECRET_BYTES];
    OsRng.fill_bytes(&mut bytes);
    MintedClientSecret {
        plaintext: hex::encode(bytes),
    }
}

/// Generate a client id.
///
/// Public, and the reason it is a separate function from the secret is that a client id
/// appears in a browser's address bar, in a support ticket and in a log line, while a secret
/// appears in exactly one place. Drawing them from separate OS calls keeps the id from being
/// a prefix of anything sensitive even though both come from the same generator.
#[must_use]
pub fn mint_client_id() -> String {
    let mut bytes = [0u8; CLIENT_ID_BYTES];
    OsRng.fill_bytes(&mut bytes);
    format!("{CLIENT_NAMESPACE}{}", hex::encode(bytes))
}

/// The one-way form written to `oauth_apps.client_secret_hash`.
#[must_use]
pub fn hash_client_secret(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(SECRET_SCHEME.as_bytes());
    hasher.update(b"\0");
    hasher.update(secret.as_bytes());
    format!("{SECRET_SCHEME}${}", hex::encode(hasher.finalize()))
}

/// Whether a stored client-secret hash was written by this scheme.
#[must_use]
pub fn is_readable_hash(stored: &str) -> bool {
    stored.starts_with(SECRET_SCHEME) && stored.len() == SECRET_SCHEME.len() + 1 + 64
}

/// Check a presented client secret against a stored hash.
///
/// Constant-time, and `false` rather than an error for a row this build cannot read: the
/// client learns nothing beyond "these do not match", which is the whole of what an
/// unauthorized caller should get.
#[must_use]
pub fn verify_client_secret(presented: &str, stored_hash: &str) -> bool {
    if !is_readable_hash(stored_hash) {
        return false;
    }
    hash_client_secret(presented)
        .as_bytes()
        .ct_eq(stored_hash.as_bytes())
        .into()
}

/// A grant type an app may be registered for.
///
/// `Serialize`/`Deserialize` with the stored spelling, because `OAuthApp::grant_types` is a
/// field the API returns and the panel edits. The round trip is part of the contract: what the
/// panel submits is `["authorization_code"]` and what it reads back has to be the same two
/// strings, not the derived `AuthorizationCode` variant name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantType {
    /// Authorization code, with PKCE. The flow a user-facing client uses.
    AuthorizationCode,
    /// Client credentials: the app authenticates as itself, with no user in the flow.
    ClientCredentials,
}

impl GrantType {
    /// The stored form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AuthorizationCode => "authorization_code",
            Self::ClientCredentials => "client_credentials",
        }
    }

    /// Parse a stored or submitted value, refusing anything else rather than defaulting.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "authorization_code" => Some(Self::AuthorizationCode),
            "client_credentials" => Some(Self::ClientCredentials),
            _ => None,
        }
    }

    /// Whether this grant is performed by a person, and therefore needs a user in the flow.
    #[must_use]
    pub fn involves_user(self) -> bool {
        matches!(self, Self::AuthorizationCode)
    }
}

/// Normalise a redirect URI for comparison, or explain why it cannot be one.
///
/// The normalisations are the two a *browser* performs on a URL before sending it, because a
/// comparison that does not account for them rejects a legitimate redirect:
///
/// * a bare `https://host` and `https://host/` are the same origin and the same path, and
///   every client library normalises one into the other;
/// * a trailing `?` or `#` with nothing after it carries no value, so `…/cb?` is `…/cb`.
///
/// Everything else is left exactly as written. No lowercasing (paths are
/// case-sensitive), no trailing-slash removal on a path (`/cb` and `/cb/` can be different
/// endpoints), and no percent-decoding — a URI is compared as the exact octets the client will
/// send.
fn normalise_redirect(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let (scheme, rest) = trimmed.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    // The authority, which is everything up to the first `/`, `?` or `#`.
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let tail = &rest[authority_end..];

    let normalised_tail = tail
        .trim_end_matches('?')
        .trim_end_matches('#')
        .trim_end_matches('?');

    let mut out = format!("{scheme}://{authority}{normalised_tail}");
    if out.ends_with('/') {
        out.pop();
    }
    Some(out)
}

/// Whether a URI's scheme is one the platform will redirect a browser to.
///
/// `https` always. `http` only for the loopback names, which is the exception the request
/// asks for and the one RFC 8252's device flow depends on.
#[must_use]
pub fn redirect_scheme_allowed(uri: &str) -> bool {
    let scheme = uri
        .split_once("://")
        .map(|(scheme, _)| scheme.to_ascii_lowercase());
    match scheme.as_deref() {
        Some("https") => true,
        Some("http") => loopback_host(uri),
        _ => false,
    }
}

/// Whether a URI names the loopback interface.
///
/// An exact match on the three names RFC 8252 names, and *not* a `starts_with` check. The
/// prefix version accepts `http://localhost.attacker.example/cb` and
/// `http://127.0.0.1.attacker.example/cb`, both of which resolve to an attacker's host — the
/// single most common way an "http is allowed for localhost" rule is written and the single
/// most common way it becomes an open redirect.
#[must_use]
pub fn loopback_host(uri: &str) -> bool {
    let Some((_, rest)) = uri.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // Strip userinfo: `user@localhost` names loopback, while `localhost.evil.com@evil.test`
    // is an attacker's host wearing a localhost-looking prefix.
    let host = authority.rsplit('@').next().unwrap_or(authority);
    // Strip the port — but **not** by splitting on the first colon, because the only
    // loopback name that contains one is the IPv6 literal `[::1]`, and a naive `split(':')`
    // reduces it to `[`, which matches nothing. A bracketed authority is a literal as a
    // whole, so it is taken intact before any colon is treated as a port separator.
    //
    // **The `]` has to be the last character of the authority, or of the part before the
    // port.** `[::1].attacker.example` is not an IPv6 literal followed by a port — it is a
    // hostname that happens to start with a bracket, and treating the bracket as "this is a
    // literal, take it whole" accepts it as loopback. So the closing bracket is only honoured
    // when nothing but an optional `:port` follows it; otherwise the whole thing is a hostname
    // and is matched against the loopback names as one string, which it fails. This is the same
    // class of bug as `starts_with("localhost")`, one level deeper, and it was found by the
    // slice-3b list validator rather than by the slice-3a unit tests — the unit tests covered
    // the accepted forms and the `starts_with` near-misses, and not this one.
    let host = match host.strip_prefix('[') {
        // A bracketed authority. The literal is `host[1..=end]`; the remainder after `]` must
        // be empty (no port) or a bare `:port`.
        Some(after_bracket) => match after_bracket.find(']') {
            Some(end) => {
                let literal = &host[1..=end];
                let trailing = &after_bracket[end + 1..];
                let valid_trailing =
                    trailing.is_empty() || trailing.strip_prefix(':').is_some_and(is_port);
                if valid_trailing {
                    // Reconstructed rather than sliced so the match below can stay a plain
                    // comparison against the bracketed spelling, and so the returned string
                    // has the same shape regardless of which branch produced it.
                    return matches!(literal, "::1")
                        && trailing
                            .strip_prefix(':')
                            .is_none_or(|port| port.parse::<u16>().is_ok());
                }
                // Not a literal at all: fall through and let the plain host comparison refuse
                // it, which it will, because the whole authority is not a loopback name.
                host
            }
            None => host,
        },
        None => host.split(':').next().unwrap_or(host),
    };
    matches!(host, "localhost" | "127.0.0.1")
}

/// Whether a string is a decimal port number.
///
/// A bound rather than a parse-and-hope: `[::1]:99999999999` is not a URL any browser accepts,
/// and answering "that is loopback, with a nonsense port" is an answer about a URL that cannot
/// be redirected to. `u16` is the port range every browser honours.
fn is_port(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 5
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<u16>().is_ok()
}

/// Check a submitted redirect URI against an app's registered list.
///
/// Returns `Ok(())` when the submitted URI is one the app registered — after the two
/// normalisations a browser performs — and refuses everything else. The comparison is a
/// whole-string match, never a prefix: see the module docs for why the prefix form is the
/// hole this function exists to not have.
pub fn check_redirect_uri(submitted: &str, registered: &[String]) -> Result<(), RedirectRejection> {
    let normalised = normalise_redirect(submitted).ok_or(RedirectRejection::NotAbsolute)?;
    for candidate in registered {
        if let Some(allowed) = normalise_redirect(candidate)
            && allowed == normalised
        {
            return Ok(());
        }
    }
    Err(RedirectRejection::NotRegistered)
}

/// Why a submitted redirect URI was refused.
///
/// A distinct type rather than a `String`, because the panel has to render this inline under
/// the field and the three answers need three different words. `NotRegistered` deliberately
/// does **not** carry the submitted value: an error that quotes what the caller sent is an
/// echo of a URL that may itself be an attack, and it ends up in a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectRejection {
    /// Not an absolute URI, so it cannot be compared to anything.
    NotAbsolute,
    /// Well formed, but not one this app registered.
    NotRegistered,
}

impl std::fmt::Display for RedirectRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAbsolute => formatter.write_str("a redirect URI must be an absolute URL"),
            Self::NotRegistered => {
                formatter.write_str("this redirect URI is not registered for the app")
            }
        }
    }
}

/// The BASE64URL form of a digest, without padding.
///
/// Standard base64 uses `+` and `/`, both of which have to be percent-encoded in a URL, so
/// PKCE specifies the URL-safe alphabet. Hand-rolled because it is six lines and the
/// alternative is a dependency for one function.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18 & 0x3F) as usize] as char);
        out.push(ALPHABET[(triple >> 12 & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(triple >> 6 & 0x3F) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3F) as usize] as char);
        }
    }
    out
}

/// The challenge for a verifier under `S256`.
#[must_use]
pub fn code_challenge_s256(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    base64url(&hasher.finalize())
}

/// Whether a verifier satisfies a stored challenge, and under which method.
///
/// The comparison is constant-time *and* both sides are derived first, so nothing in this
/// path compares caller text to stored text: the stored value is already a digest, and the
/// candidate is hashed before the two are compared. That is what makes an intercepted code
/// useless — the attacker holds the challenge, which is a hash, and cannot go backwards.
///
/// `plain` is accepted for interoperability, and the returned method is what lets the
/// caller record that a client took the weaker path.
pub fn verify_code_verifier(
    verifier: &str,
    stored_challenge: &str,
    stored_method: &str,
) -> Option<&'static str> {
    let candidate = match stored_method {
        "S256" => code_challenge_s256(verifier),
        "plain" => verifier.to_owned(),
        _ => return None,
    };
    let matches: bool = candidate
        .as_bytes()
        .ct_eq(stored_challenge.as_bytes())
        .into();
    // The method is returned as a `&'static str` rather than the caller's `&str`, and that is
    // not a signature convenience: the value is a fact about *which method matched*, drawn
    // from this function's own vocabulary, so handing back the caller's reference would
    // invite a caller to log a method string it supplied. The two happen to be the same
    // bytes today; they are not the same thing.
    matches.then_some(if stored_method == "S256" {
        "S256"
    } else {
        "plain"
    })
}

/// Whether a challenge is one this platform can verify.
///
/// Structural only: base64url of a SHA-256 is 43 characters with no padding, and anything
/// else is either a typo or a `plain` challenge recorded under the `S256` label. Checking it
/// before the code is stored means a request that can never be redeemed fails at the
/// authorization step, where the client can still fix it, rather than at the token step.
#[must_use]
pub fn challenge_looks_valid(challenge: &str, method: &str) -> bool {
    match method {
        "S256" => {
            challenge.len() == 43
                && challenge
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        }
        "plain" => !challenge.is_empty() && challenge.len() <= 128,
        _ => false,
    }
}

/// The hash a one-time authorization code is stored and looked up by.
///
/// A separate scheme prefix from both key schemes, for the same reason they differ from each
/// other: the three credential spaces must not be interchangeable, and a code hash that
/// verified as a client secret would be a cross-credential oracle.
#[must_use]
pub fn hash_code(code: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"omnion-oauth-code.v1\0");
    hasher.update(code.as_bytes());
    hex::encode(hasher.finalize())
}

/// Whether an app's scopes cover every scope a request asked for.
///
/// A grant is a *narrowing*: an authorization request may ask for less than the app
/// registered, never more. The comparison is over sorted vectors so it cannot depend on the
/// order the client listed them in, which is the kind of thing that passes a test written
/// with the same order and fails in the field.
#[must_use]
pub fn grant_covers(registered: &[String], requested: &[String]) -> bool {
    let registered: std::collections::BTreeSet<&str> =
        registered.iter().map(String::as_str).collect();
    requested
        .iter()
        .all(|scope| registered.contains(scope.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── client ids and secrets ──────────────────────────────────────────────

    #[test]
    fn a_client_id_is_namespaced_and_long_enough_to_be_unique() {
        let id = mint_client_id();
        assert!(id.starts_with("omn_app_"));
        // 12 bytes of entropy, hex encoded.
        assert_eq!(id.len(), "omn_app_".len() + 24);
    }

    #[test]
    fn two_minted_client_ids_never_collide() {
        assert_ne!(mint_client_id(), mint_client_id());
    }

    #[test]
    fn a_client_secret_is_write_only_and_carries_its_scheme() {
        let minted = mint_client_secret();
        let stored = hash_client_secret(&minted.plaintext);
        assert!(is_readable_hash(&stored));
        assert!(stored.starts_with("omnion-oauth-secret.v1$"));
        // The one-way property, stated as the assertion: the plaintext is not recoverable
        // from what was stored, and the stored form is not a substring of the plaintext.
        assert!(!stored.contains(&minted.plaintext));
        assert!(!minted.plaintext.contains(&stored));
    }

    #[test]
    fn verification_accepts_only_its_own_client_secret() {
        let minted = mint_client_secret();
        let stored = hash_client_secret(&minted.plaintext);
        assert!(verify_client_secret(&minted.plaintext, &stored));
        assert!(!verify_client_secret("wrong", &stored));
        assert!(
            !verify_client_secret(&minted.plaintext, &hash_client_secret("other")),
            "another app's secret must not verify"
        );
    }

    #[test]
    fn a_client_secret_hash_is_not_an_api_key_hash() {
        // The three credential spaces must not be interchangeable. A row written by the API
        // key path must read as unverifiable here, or a leaked `secret_hash` column could be
        // replayed as a client secret.
        let api_hash = crate::secret::hash("some-secret-half");
        assert!(!is_readable_hash(&api_hash));
        assert!(!verify_client_secret("some-secret-half", &api_hash));
        assert!(!crate::secret::verify(
            "some-secret-half",
            &hash_client_secret("x")
        ));
    }

    #[test]
    fn an_unreadable_secret_hash_authenticates_nobody() {
        assert!(!verify_client_secret("anything", ""));
        assert!(!verify_client_secret(
            "anything",
            "$argon2id$v=19$m=1$aaaa$bbbb"
        ));
        assert!(!verify_client_secret(
            "anything",
            "omnion-oauth-secret.v2$0000"
        ));
    }

    #[test]
    fn the_debug_form_cannot_leak_a_client_secret() {
        let minted = mint_client_secret();
        let rendered = format!("{minted:?}");
        assert!(!rendered.contains(&minted.plaintext));
        assert!(rendered.contains("redacted"));
    }

    // ── redirect URIs ────────────────────────────────────────────────────────

    fn registered() -> Vec<String> {
        vec![
            "https://app.example.com/callback".to_string(),
            "http://localhost:3000/callback".to_string(),
        ]
    }

    #[test]
    fn a_registered_uri_is_accepted() {
        assert_eq!(
            check_redirect_uri("https://app.example.com/callback", &registered()),
            Ok(())
        );
        assert_eq!(
            check_redirect_uri("http://localhost:3000/callback", &registered()),
            Ok(())
        );
    }

    #[test]
    fn the_browser_rewrites_are_normalized_before_comparing() {
        // A trailing slash and a bare `?` are the two forms a client library rewrites. A
        // comparison that misses them rejects a legitimate redirect and teaches a developer
        // to paste the exact string the panel showed them.
        assert_eq!(
            check_redirect_uri("https://app.example.com/callback/", &registered()),
            Ok(())
        );
        assert_eq!(
            check_redirect_uri("https://app.example.com/callback?", &registered()),
            Ok(())
        );
        assert_eq!(
            check_redirect_uri("  https://app.example.com/callback  ", &registered()),
            Ok(()),
            "surrounding whitespace is a paste artefact, not a different URI"
        );
    }

    #[test]
    fn a_uri_that_merely_starts_like_a_registered_one_is_refused() {
        // This is the whole point of comparing whole strings. Every one of these shares a
        // prefix with a registered URI and must be refused.
        for hostile in [
            "https://app.example.com/callback-attacker",
            "https://app.example.com/callback.evil",
            "https://app.example.com/callback/../../evil",
            "https://app.example.com/callback#x",
            "https://app.example.com/callback?next=https://evil.example",
            // The registered host, one character longer.
            "https://app.example.com.evil.test/callback",
        ] {
            assert_eq!(
                check_redirect_uri(hostile, &registered()),
                Err(RedirectRejection::NotRegistered),
                "{hostile} must not be accepted"
            );
        }
        // A scheme with a single slash is not an absolute URI at all, so it is reported as
        // the more precise fault rather than as "not registered" — a client that sends this
        // has a malformed URL, and telling it to register the URI would send it away to fix
        // the wrong thing.
        assert_eq!(
            check_redirect_uri("https:/app.example.com/callback", &registered()),
            Err(RedirectRejection::NotAbsolute)
        );
    }

    #[test]
    fn a_relative_uri_cannot_be_a_redirect_target() {
        for bad in ["/callback", "app.example.com/callback", "not a url", ""] {
            assert_eq!(
                check_redirect_uri(bad, &registered()),
                Err(RedirectRejection::NotAbsolute),
                "{bad:?} is not absolute"
            );
        }
    }

    #[test]
    fn http_is_allowed_for_loopback_and_nothing_else() {
        assert!(redirect_scheme_allowed("https://app.example.com/cb"));
        assert!(redirect_scheme_allowed("http://localhost:3000/cb"));
        assert!(redirect_scheme_allowed("http://127.0.0.1:8080/cb"));
        assert!(redirect_scheme_allowed("http://[::1]:3000/cb"));
        // The prefix-matching hole, closed: these all *start with* localhost.
        assert!(!redirect_scheme_allowed(
            "http://localhost.attacker.example/cb"
        ));
        assert!(!redirect_scheme_allowed("http://localhost.evil.test/cb"));
        assert!(!redirect_scheme_allowed(
            "http://127.0.0.1.attacker.example/cb"
        ));
        // And the schemes that are not ours at all.
        assert!(!redirect_scheme_allowed("javascript:alert(1)"));
        assert!(!redirect_scheme_allowed(
            "data:text/html,<script>alert(1)</script>"
        ));
        assert!(!redirect_scheme_allowed("ftp://example.com/cb"));
        assert!(!redirect_scheme_allowed("myapp://callback"));
    }

    #[test]
    fn loopback_detection_ignores_a_port_and_a_userinfo() {
        assert!(loopback_host("http://localhost:9999/x"));
        assert!(loopback_host("http://user:pw@localhost:9999/x"));
        assert!(!loopback_host("http://localhost:9999@evil.example/x"));
        assert!(!loopback_host("http://evil.example/x"));
    }

    #[test]
    fn the_ipv6_loopback_literal_is_recognised_in_every_spelling_a_client_writes() {
        // The three forms: bare, with a port, and with userinfo. The bracketed authority is a
        // literal as a whole, which is why a naive `split(':')` cannot be used here — it
        // reduces `[::1]` to `[`.
        for good in [
            "http://[::1]/cb",
            "http://[::1]:3000/cb",
            "http://[::1]:65535/cb",
            "http://user:pw@[::1]:8080/cb",
        ] {
            assert!(loopback_host(good), "{good} is loopback");
        }
    }

    #[test]
    fn a_hostname_that_begins_with_a_bracket_is_not_an_ipv6_literal() {
        // **This is the bug the slice-3b list validator found in the code above.** The fix for
        // `split(':')` was to take a bracketed authority whole, and taking it whole *as a
        // literal* accepted `[::1].attacker.example` — an attacker's hostname that merely starts
        // with the loopback literal. The test lives here as well as in the validator's module
        // because the defect is in this function: a test in the caller would be fixed by
        // changing the caller, leaving the hole in the shared predicate.
        //
        // The family is the same as `starts_with("localhost")` one level down, so the cases are
        // written as a family rather than one by one.
        for hostile in [
            "http://[::1].attacker.example/cb",
            "http://[::1]attacker.example/cb",
            "http://[::1].example/cb",
            "http://[::1]:3000.attacker.example/cb",
            // A literal that is not loopback, wearing the same spelling.
            "http://[2001:db8::1]/cb",
            "http://[::2]/cb",
            // A bracket that never closes, and one that closes with junk after it.
            "http://[::1:99999/cb",
            "http://[::1]x/cb",
        ] {
            assert!(
                !loopback_host(hostile),
                "{hostile} is an attacker's host, not loopback"
            );
        }
    }

    #[test]
    fn a_port_that_no_browser_accepts_is_not_a_loopback_url() {
        // `[::1]:99999` is out of `u16` range and `[::1]:0x10` is not decimal. Neither is a URL
        // a browser will follow, so treating either as loopback is an answer about a redirect
        // that cannot happen — and, worse, a validator that says "yes" for a URL the client will
        // refuse to use teaches a developer the wrong thing about their own configuration.
        for bad in [
            "http://[::1]:99999/cb",
            "http://[::1]:0x10/cb",
            "http://[::1]:/cb",
        ] {
            assert!(!loopback_host(bad), "{bad} is not a usable loopback URL");
        }
        // And the boundary that does work.
        assert!(loopback_host("http://[::1]:65535/cb"));
    }

    #[test]
    fn the_rejection_never_echoes_what_was_submitted() {
        // The submitted value can itself be an attack string, and this error travels to a log.
        let hostile = "https://evil.example/steal?token=abcdef";
        let rendered = check_redirect_uri(hostile, &registered())
            .unwrap_err()
            .to_string();
        assert!(!rendered.contains("steal"));
        assert!(!rendered.contains("abcdef"));
        assert!(rendered.contains("not registered"));
    }

    // ── PKCE ────────────────────────────────────────────────────────────────

    // The example verifier and challenge pair from RFC 7636 appendix B. Written out rather
    // than generated so the implementation is checked against the specification's own
    // numbers, which is the only external check this arithmetic ever gets.
    const RFC_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const RFC_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    #[test]
    fn the_s256_challenge_matches_rfc_7636() {
        assert_eq!(code_challenge_s256(RFC_VERIFIER), RFC_CHALLENGE);
    }

    #[test]
    fn the_base64url_alphabet_is_the_url_safe_one_without_padding() {
        // A digest of these three bytes is 0xfb 0xff 0xbf, whose standard base64 would be
        // "+/+/" and whose url-safe form is "-_-": no `+`, no `/`, no `=`.
        let encoded = base64url(&[0xfb, 0xff, 0xbf]);
        assert_eq!(encoded, "-_-_");
        assert!(!encoded.contains('+'));
        assert!(!encoded.contains('/'));
        assert!(!encoded.contains('='));
    }

    #[test]
    fn a_verifier_is_accepted_only_for_its_own_challenge() {
        assert_eq!(
            verify_code_verifier(RFC_VERIFIER, RFC_CHALLENGE, "S256"),
            Some("S256")
        );
        assert_eq!(
            verify_code_verifier("wrong-verifier", RFC_CHALLENGE, "S256"),
            None
        );
    }

    #[test]
    fn the_plain_method_is_accepted_but_reported_as_itself() {
        // The return value is what lets the authorization endpoint record that a client chose
        // the weaker method. Accepting it silently would leave the weaker path invisible until
        // an audit found it.
        assert_eq!(
            verify_code_verifier("a-verifier", "a-verifier", "plain"),
            Some("plain")
        );
        assert_eq!(verify_code_verifier("a-verifier", "other", "plain"), None);
        // A `plain` challenge must not satisfy an `S256` slot and the other way round.
        assert_eq!(
            verify_code_verifier(RFC_VERIFIER, RFC_VERIFIER, "S256"),
            None
        );
    }

    #[test]
    fn an_unknown_challenge_method_verifies_nothing() {
        assert_eq!(verify_code_verifier("x", "x", "S512"), None);
        assert_eq!(verify_code_verifier("x", "x", ""), None);
    }

    #[test]
    fn a_challenge_is_structurally_checked_before_it_is_stored() {
        assert!(challenge_looks_valid(RFC_CHALLENGE, "S256"));
        // The right length but the wrong alphabet — a `+` that has not been url-encoded.
        assert!(!challenge_looks_valid(&("A".repeat(42) + "+"), "S256"));
        // A `plain` challenge recorded under the `S256` label can never be redeemed, so it is
        // refused at the door rather than at the token endpoint.
        assert!(!challenge_looks_valid("short", "S256"));
        assert!(!challenge_looks_valid(RFC_CHALLENGE, "S512"));
        assert!(challenge_looks_valid("a-plain-verifier", "plain"));
        assert!(!challenge_looks_valid("", "plain"));
    }

    #[test]
    fn a_code_is_stored_as_a_hash_that_is_not_the_code() {
        let code = "a-one-time-authorization-code";
        let stored = hash_code(code);
        assert_eq!(
            stored.len(),
            64,
            "a hex SHA-256, with no scheme prefix to leak"
        );
        assert!(!stored.contains(code));
        // Stable, so the redemption step can look the row up by the same derivation.
        assert_eq!(hash_code(code), stored);
        assert_ne!(hash_code("other"), stored);
    }

    // ── scope grants ────────────────────────────────────────────────────────

    #[test]
    fn a_grant_covers_a_subset_but_never_a_superset() {
        let registered = vec!["content.read".to_string(), "content.write".to_string()];
        assert!(grant_covers(&registered, &["content.read".to_string()]));
        assert!(grant_covers(&registered, &registered));
        // Order must not matter: a client that lists its scopes alphabetically is asking the
        // same thing.
        assert!(grant_covers(
            &registered,
            &["content.write".to_string(), "content.read".to_string()]
        ));
        // An ungranted scope must refuse the whole grant, not be silently dropped: a client
        // that asked for two things and was quietly given one is a client whose next bug
        // report says "the write worked" when it never was granted.
        let overreach = grant_covers(
            &registered,
            &["content.read".to_string(), "content.delete".to_string()],
        );
        assert!(!overreach, "an ungranted scope must refuse the whole grant");
        assert!(!grant_covers(&[], &["content.read".to_string()]));
    }

    #[test]
    fn grant_types_round_trip_and_only_one_involves_a_user() {
        assert_eq!(
            GrantType::parse("authorization_code"),
            Some(GrantType::AuthorizationCode)
        );
        assert_eq!(
            GrantType::parse("client_credentials"),
            Some(GrantType::ClientCredentials)
        );
        assert_eq!(
            GrantType::parse("password"),
            None,
            "the resource-owner grant is out of scope"
        );
        assert!(GrantType::AuthorizationCode.involves_user());
        assert!(!GrantType::ClientCredentials.involves_user());
    }
}
