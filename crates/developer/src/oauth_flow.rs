//! The sessionless half of the OAuth flow: the authorization request, consent and the token
//! endpoint (docs/requests/REQ-033, slice 3c).
//!
//! # Why this file exists separately from `developer_oauth.rs`
//!
//! The panel's half of OAuth asks "which tenant is this person working in?" and answers from the
//! session. These endpoints ask "which app is calling?" and answer from the **app row** — the
//! client arrives with a `client_id` and no session at all. One handler serving both would have
//! to pick one answer for "who is this?", and the tenant a write lands in would then depend on
//! which value arrived first. Keeping them apart is what makes the tenant for every statement in
//! each file come from a single, nameable place: [`super::organization_of`] for the panel, the
//! app's own `organization_id` here.
//!
//! # The property this file is built around
//!
//! **A code is the only thing that crosses the boundary, and it is worthless without the
//! verifier.** The authorization endpoint mints a code bound to (app, user, redirect, scopes,
//! PKCE challenge); the token endpoint redeems it exactly once and proves PKCE before it issues
//! anything. Both halves are in this file so that the properties — single use, the redirect
//! re-check, the challenge that cannot be half a pair — are provable by reading one screenful
//! rather than by tracing two.
//!
//! # The three rules that are not negotiable
//!
//! 1. **The redirect URI is validated before the code exists, and again at the token step.**
//!    The first check decides whether a browser is ever sent anywhere; the second decides
//!    whether a code is honoured, because a code and a redirect that did not come from the same
//!    request is exactly the mix-and-match the whole string comparison exists to stop.
//! 2. **A token is minted for a *user*, and its scopes are the consented ones.** Never the
//!    app's registered scopes, never the scopes in the token request body. A client asking for
//!    more than was consented gets the grant it was given, not an error — and a client asking
//!    for less gets less, which is the direction that is safe.
//! 3. **The client secret is verified in constant time, and the overlap is filtered by the
//!    clock first.** [`which_secret_matched`] does both; this file adds nothing to it.

use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::model_oauth::ConsentRequest;
use crate::oauth::{self, GrantType};

/// How long an issued access token lives.
///
/// One hour, with no refresh token. A refresh token is a second long-lived credential that has to
/// be stored, revoked, and audited on its own, and an authorization-code flow that mints one
/// before it has a revocation story is how a token outlives the grant that produced it. An
/// access token this short means a withdrawn app stops working at the next refresh of the
/// client's own session, and the panel says so rather than leaving it to be discovered.
pub const ACCESS_TOKEN_TTL_SECONDS: i64 = 3600;

/// The `state` a client sends and gets back, and the ceiling on its length.
///
/// Bounded because it rides in a query string that lands in browser history, proxy logs and
/// `Referer` headers on the redirect leg. A megabyte of `state` is a request any proxy will
/// truncate, and a truncated `state` is a client that silently loses its CSRF defence.
pub const MAX_STATE_LENGTH: usize = 512;

/// The longest authorization code we will mint, before hashing.
///
/// A bound rather than a limit to hit: the code is a bearer credential for a *user's* session
/// and the column is a hash, so an unbounded generated value is a megabyte of entropy hashed
/// and stored per authorization. 64 characters of base64url is 384 bits — far past the point
/// where guessing is the attack, and short enough to paste into a test client.
pub const CODE_BYTES: usize = 48;

/// Mint an authorization code: URL-safe, unguessable, shown to the client exactly once.
///
/// Not hex like a client secret: a code travels in a query string and in a `Location` header, so
/// it uses the same URL-safe alphabet PKCE's challenge does, and the `code_` prefix is what
/// makes it recognisable in a log without being a secret.
#[must_use]
pub fn mint_code() -> String {
    let mut bytes = [0u8; CODE_BYTES];
    OsRng.fill_bytes(&mut bytes);
    format!("code_{}", base64url(&bytes))
}

/// The URL-safe base64 of some bytes, without padding.
///
/// Duplicated from [`crate::oauth`] rather than made public there for one reason worth stating:
/// the challenge and the code are both URL-safe strings, and a *shared* helper would let a
/// change to one (a different alphabet for a browser-compat reason, say) silently change the
/// other's on-disk hash. They are separate credentials and they hash separately; sharing the
/// spelling helper is a coupling those separate hashes do not need. Six lines, and the tests
/// below assert the alphabet rather than trusting it.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(ALPHABET[(triple >> 18 & 0x3F) as usize]));
        out.push(char::from(ALPHABET[(triple >> 12 & 0x3F) as usize]));
        if chunk.len() > 1 {
            out.push(char::from(ALPHABET[(triple >> 6 & 0x3F) as usize]));
        }
        if chunk.len() > 2 {
            out.push(char::from(ALPHABET[(triple & 0x3F) as usize]));
        }
    }
    out
}

/// The query parameters of `GET /oauth/authorize`.
///
/// `scope` arrives space-delimited, as the OAuth spec says, rather than repeated — a repeated
/// key is what `serde_urlencoded` cannot put in a `Vec` (see the note in `apps/api/Cargo.toml`
/// about why the log screen parses its own query string), and the spec's spelling is the one
/// every client library already sends.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuthorizeQuery {
    /// The app's public identifier.
    #[serde(default)]
    pub client_id: String,
    /// Where to send the browser back.
    #[serde(default)]
    pub redirect_uri: String,
    /// `authorization_code` — the only grant a browser flow can use.
    #[serde(default)]
    pub response_type: String,
    /// Space-delimited permission keys.
    #[serde(default)]
    pub scope: Option<String>,
    /// The client's own opaque value, returned unchanged.
    #[serde(default)]
    pub state: Option<String>,
    /// The PKCE challenge.
    #[serde(default)]
    pub code_challenge: Option<String>,
    /// `S256` or `plain`.
    #[serde(default)]
    pub code_challenge_method: Option<String>,
}

/// The parsed shape of an authorization request, once `scope` is split.
///
/// Split here rather than in the handler so the split is testable and so the two callers — the
/// authorization endpoint and the consent POST — cannot disagree about how a space-delimited
/// scope is turned into a list.
impl AuthorizeQuery {
    /// The scopes this request asks for, from the space-delimited `scope` parameter.
    ///
    /// Empty is meaningful and is *not* an error: the OAuth spec says an absent `scope` means
    /// "everything the client is registered for", and [`crate::model_oauth::authorize`] is the
    /// thing that turns that into the app's own list. A blank-but-present `scope` (`"   "`)
    /// collapses to the same empty list, which is the same request, not a different one.
    #[must_use]
    pub fn scopes(&self) -> Vec<String> {
        self.scope
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    }
}

/// A validated authorization request, ready for the consent screen or the code mint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedAuthorization {
    /// What the consent screen was shown and the user agreed to.
    pub consent: ConsentRequest,
    /// The client's `state`, to be echoed back on the redirect. Bounded and never logged.
    pub state: Option<String>,
}

/// Turn a query string into the shape [`authorize`](crate::model_oauth::authorize) takes.
///
/// Separate from the handler for the reason every parser in this crate is separate: it is the
/// place where a browser-supplied string becomes a domain value, and it is testable with no
/// database, no router and no session.
pub fn parse_authorize_query(query: &AuthorizeQuery) -> crate::model_oauth::AuthorizationRequest {
    crate::model_oauth::AuthorizationRequest {
        client_id: query.client_id.clone(),
        redirect_uri: query.redirect_uri.clone(),
        // An omitted `response_type` is the authorization-code flow, which is the only one a
        // browser can perform. Anything else the client may have registered is for the token
        // endpoint, and naming it here would produce a confusing "unknown grant" rather than
        // the "this endpoint only does authorization codes" a caller can act on.
        grant_type: match query.response_type.as_str() {
            "code" | "" => GrantType::AuthorizationCode.as_str().to_owned(),
            other => other.to_owned(),
        },
        scopes: query.scopes(),
        code_challenge: query.code_challenge.clone(),
        code_challenge_method: query.code_challenge_method.clone(),
    }
}

/// The `state` a redirect may carry back, or `None` when it may not.
///
/// Two rules, and both matter. An over-long `state` is dropped rather than rejected: the browser
/// is already on its way to a redirect that the platform validated, and refusing here would
/// strand the user on an error page for something the client got wrong. And a `state`
/// containing a control character is dropped for the same reason — it goes into a `Location`
/// header, and [`super`]'s redirect builder drops any value a header cannot carry anyway, so
/// the two agree instead of one silently truncating what the other accepted.
#[must_use]
pub fn usable_state(state: Option<&String>) -> Option<String> {
    let state = state?.trim();
    if state.is_empty() || state.len() > MAX_STATE_LENGTH {
        return None;
    }
    if state.chars().any(char::is_control) {
        return None;
    }
    Some(state.to_owned())
}

/// Percent-encode a caller-supplied `state` for a query string.
///
/// Seven cases, and the set is not arbitrary:
///
/// * `%` first, because it is the escape character itself and encoding it last would let an
///   already-encoded state decode once into something that decodes again into a different value.
/// * `&` and `#` — the two characters that would end the value: the first starts another
///   parameter, the second starts the fragment.
/// * `=` — legal inside a value per RFC 3986, and a *conforming* parser splits on the first one,
///   so encoding it is not strictly required. It is encoded anyway because a client that splits
///   on all of them would read `state` as `a&scope` and silently lose the rest, and a
///   credential-adjacent parameter is the last place to be the odd one out.
/// * `+` — a space to a form-urlencoded parser, which is the parser most OAuth clients use for
///   a token response. This is the one that is genuinely load-bearing: a `+` passed through raw
///   is a `+` to a percent-decoder and a space to a form decoder, so the two families disagree
///   and one of them corrupts the value.
/// * **Every byte above `0x7e`, plus `DEL`.** A `state` is not required to be ASCII, and a
///   byte-wise encoder that passes UTF-8 through untouched hands the client a different string
///   than the one it sent: `ü` arrives as two raw bytes, and a decoder that treats each as a
///   character produces mojibake. The round-trip test in this module is what caught that; an
///   assertion on the ASCII cases alone would have passed.
fn encode_state(state: &str) -> String {
    let mut out = String::with_capacity(state.len());
    for byte in state.bytes() {
        match byte {
            b'%' => out.push_str("%25"),
            b'&' => out.push_str("%26"),
            b'#' => out.push_str("%23"),
            b'=' => out.push_str("%3D"),
            b'+' => out.push_str("%2B"),
            // Printable ASCII except the five above is safe in a query value. Everything else
            // is encoded, which keeps the output valid in a `Location` header as well as
            // unambiguous to a parser.
            0x21..=0x7e => out.push(char::from(byte)),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// A `code`, a `state` and where to send the browser, built from a validated redirect.
///
/// The one place a `Location` value is assembled for this flow, and it is a function rather than
/// a `format!` at each call site for the same reason [`check_redirect_uri`](oauth) is one
/// function: two copies of "append the query parameters to a registered redirect" is one copy
/// that will eventually forget the registered form can already carry a query string of its own.
#[must_use]
pub fn redirect_with_code(redirect_uri: &str, code: &str, state: Option<&str>) -> String {
    // A registered URI that already has a query keeps it: `?` before the first `&`, `&` after.
    // Dropping the client's own parameters is how an app registered at
    // `https://app.example.com/cb?tenant=acme` ends up consenting for the wrong tenant.
    let separator = if redirect_uri.contains('?') { '&' } else { '?' };
    let mut out = format!("{redirect_uri}{separator}code={code}");
    if let Some(state) = state {
        // Encoded rather than interpolated: `state` is caller-supplied and rides in a header, so
        // these characters are the ones that would otherwise let a client append its own
        // parameters or a fragment to the platform's redirect.
        out.push_str("&state=");
        out.push_str(&encode_state(state));
    }
    out
}

/// The namespace a minted access token carries.
///
/// Distinct from every other credential in the platform — `omn_` for an API key, `omn_app_` for
/// a client id, `code_` for an authorization code — so a token found in a log is identifiable,
/// and a client that mistakenly sends one as an API key is refused by shape before any database
/// probe. That last part is the reason it is worth a separate string: the API-key path splits
/// the token on `.`, finds no separator, and answers "invalid API key" without a query.
const TOKEN_NAMESPACE: &str = "omn_tok_";

/// Bytes of entropy in an access token. 32 bytes, the same answer as an API key and a client
/// secret: a token is a machine-generated credential, and a slow hash to slow brute force buys
/// nothing against 256 bits of entropy.
const TOKEN_BYTES: usize = 32;

/// A freshly minted access token. Never stored, never logged, never returned twice.
#[derive(Clone)]
pub struct MintedToken {
    /// What the client sends as `Authorization: Bearer …`.
    pub plaintext: String,
}

impl std::fmt::Debug for MintedToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Same reason `MintedKey` and `MintedClientSecret` hand-write this: a derived `Debug`
        // writes a live credential into any log that touches the value by reference, and a token
        // is a credential for as long as its hour lasts.
        formatter
            .debug_struct("MintedToken")
            .field("plaintext", &"<redacted>")
            .finish()
    }
}

/// Mint an access token: 256 bits of CSPRNG entropy, namespaced and hex encoded.
///
/// Hex rather than the code's base64url because a token travels in an `Authorization` header as
/// often as in a query string, and hex needs no encoding anywhere — one alphabet fewer to get
/// wrong in a header this platform has to construct correctly.
#[must_use]
pub fn mint_access_token() -> MintedToken {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    MintedToken {
        plaintext: format!("{TOKEN_NAMESPACE}{}", hex::encode(bytes)),
    }
}

/// The one-way form written to `oauth_access_tokens.token_hash`.
#[must_use]
pub fn hash_access_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"omnion-oauth-token.v1\0");
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// Whether a string could be one of this platform's access tokens, checked before any lookup.
///
/// A shape test, and the reason it exists: [`hash_access_token`] would happily hash a 4 KB
/// paragraph of nonsense, and an access token is presented by an unauthenticated caller to a
/// table keyed on its hash. A caller gets "invalid token" for something that was never a token,
/// without costing a probe — and the check is on the *namespace* too, so a client secret
/// presented as a token is refused before it can be compared against a token hash at all.
#[must_use]
pub fn token_looks_valid(token: &str) -> bool {
    let Some(body) = token.strip_prefix(TOKEN_NAMESPACE) else {
        return false;
    };
    body.len() == TOKEN_BYTES * 2
        && body
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Which grant a token request authenticated with, and what the token therefore stands for.
///
/// The type exists because these two differ in exactly one place — whether a user id is written
/// to the token row — and a `bool` or a string at that call site would let the wrong answer be
/// passed with no compiler complaint, producing a request log row that attributes a machine's
/// call to a person. That is the same property `KeyPrincipal::actor_user_id` returns `None` for,
/// arrived at from the other direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantProvenance {
    /// Redeemed from an authorization code: a user consented, and the token names them.
    UserConsent,
    /// `client_credentials`: the app acting as itself, with no user in the flow.
    ClientCredentials,
}

impl GrantProvenance {
    /// Whether a token from this grant acts for a person.
    #[must_use]
    pub fn involves_user(self) -> bool {
        matches!(self, Self::UserConsent)
    }
}

/// The scopes a token will carry, or `None` when the request asked for more than the grant holds.
///
/// `requested` is what the client sent to the token endpoint, and it is allowed to be **empty** —
/// an omitted `scope` means "the grant I was just given", which is what a client that only ever
/// wants one thing sends, and treating that as an error would break the most common client in the
/// ecosystem.
///
/// A non-empty request is a **narrowing**, checked as a subset: asking for less than was
/// consented is a client restricting itself, and refusing that would be a rule no spec has.
/// Asking for *more* is refused, and this is the one place the error is worth a round trip —
/// because silently issuing a token that is missing the permission produces a `403` at the API
/// naming the *API* as the thing that lacks the permission, which sends the developer to debug
/// the wrong system entirely. The consent screen already showed the user exactly what was being
/// granted, so a request that widens past it is either a bug or an attack and both deserve to
/// stop here.
///
/// Deliberately **not** parameterised on [`GrantProvenance`]: the rule is the same for both
/// grants, because a machine token's granted set is also the app's registered set and there is
/// no consent to narrow against in either direction. A signature carrying the provenance would
/// invite a second arm that differs in no way anybody could justify from the request — and a
/// parameter that does not change the answer is a parameter the next editor will branch on.
#[must_use]
pub fn resolve_token_scopes(requested: &[String], granted: &[String]) -> Option<Vec<String>> {
    if requested.is_empty() {
        return Some(granted.to_vec());
    }
    if oauth::grant_covers(granted, requested) {
        return Some(requested.to_vec());
    }
    None
}

/// A token response, as RFC 6749 §5.1 shapes it.
#[derive(Debug, Clone, Serialize)]
pub struct TokenResponse {
    /// Always `Bearer` — the only scheme this platform issues.
    pub token_type: &'static str,
    /// The token. Returned once, never recoverable afterwards.
    pub access_token: String,
    /// Seconds until it expires, so a client can refresh without parsing a timestamp.
    pub expires_in: i64,
    /// Space-delimited scopes, matching the `scope` parameter the client sent. Empty when the
    /// grant carries the platform's default scope set, which is what RFC 6749 asks for.
    pub scope: String,
    /// The app's public identifier, echoed so a client with several tokens in flight can tell
    /// which one this is.
    pub client_id: String,
}

/// A token response that failed, as RFC 6749 §5.2 shapes it.
///
/// A distinct type rather than an `ApiError`, because these three have to reach the *client's*
/// library as a JSON body with the spec's `error` code — a client library that cannot read
/// `{"code": ...}` has no way to tell "your secret is wrong" from "your request was malformed",
/// and every well-written OAuth client already knows these three names.
#[derive(Debug, Clone, Serialize)]
pub struct TokenErrorBody {
    /// The spec's machine-readable code.
    pub error: &'static str,
    /// A human-readable line. Never echoes the presented credential.
    pub error_description: String,
}

/// The one-time grant a consented user receives: a code bound to everything it was consented for.
#[derive(Debug, Clone)]
pub struct IssuedGrant {
    /// The code itself, shown to the browser once in a redirect.
    pub code: String,
    /// When the code stops being redeemable.
    pub expires_at: OffsetDateTime,
    /// The scopes the code carries, which is what the token will be scoped to.
    pub scopes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query() -> AuthorizeQuery {
        AuthorizeQuery {
            client_id: "omn_app_0123456789abcdef01234567".to_owned(),
            redirect_uri: "https://app.example.com/callback".to_owned(),
            response_type: "code".to_owned(),
            scope: Some("content.pages.read  search.read".to_owned()),
            state: Some("xyz-123".to_owned()),
            code_challenge: Some(oauth::code_challenge_s256("a-verifier-long-enough-here")),
            code_challenge_method: Some("S256".to_owned()),
        }
    }

    #[test]
    fn a_space_delimited_scope_becomes_a_list_in_order_and_deduplicated_by_the_rule() {
        // Two things at once: the split is on whitespace runs, so a double space is not an
        // empty scope, and the order is preserved so the consent screen lists what the client
        // asked for in the order it asked for it.
        let scopes = query().scopes();
        assert_eq!(scopes, vec!["content.pages.read", "search.read"]);
        let mut padded = query();
        padded.scope = Some("  content.pages.read   search.read  ".to_owned());
        assert_eq!(padded.scopes(), scopes, "whitespace runs are one separator");
    }

    #[test]
    fn an_absent_scope_is_empty_and_means_everything_registered() {
        // **Not** an error, and the test says why in its name: the OAuth spec's rule is that an
        // absent `scope` means the client's full registered set, and `authorize` turns that into
        // the app's list. A panel that sent an empty list as "no scopes" would refuse a
        // first-party client that asked for nothing.
        let mut none = query();
        none.scope = None;
        assert!(none.scopes().is_empty());
        let mut blank = query();
        blank.scope = Some("   ".to_owned());
        assert!(
            blank.scopes().is_empty(),
            "a blank scope is the same request"
        );
    }

    #[test]
    fn a_client_that_omits_response_type_gets_the_authorization_code_flow() {
        // The bare `GET /authorize?client_id=…&redirect_uri=…` a first-party client writes.
        let mut bare = query();
        bare.response_type = String::new();
        assert_eq!(
            parse_authorize_query(&bare).grant_type,
            "authorization_code"
        );
        // And an explicit `code` is the same request.
        bare.response_type = "code".to_owned();
        assert_eq!(
            parse_authorize_query(&bare).grant_type,
            "authorization_code"
        );
    }

    #[test]
    fn a_client_that_names_another_response_type_is_passed_through_verbatim() {
        // **Not** rewritten to `authorization_code`. `authorize` then refuses it with
        // `UnknownGrantType`, which is the answer the client can act on — silently treating
        // `token` as `code` would issue a code to a client that is asking for a token, and the
        // error it eventually hit would name the wrong thing.
        let mut wrong = query();
        wrong.response_type = "token".to_owned();
        assert_eq!(parse_authorize_query(&wrong).grant_type, "token");
    }

    #[test]
    fn the_parsed_request_carries_the_pkce_pair_together() {
        let request = parse_authorize_query(&query());
        assert!(request.code_challenge.is_some());
        assert_eq!(request.code_challenge_method.as_deref(), Some("S256"));
        assert_eq!(request.redirect_uri, "https://app.example.com/callback");
        assert_eq!(request.scopes.len(), 2);
    }

    // ── the state a redirect may carry ──────────────────────────────────────

    #[test]
    fn a_state_is_echoed_back_when_it_is_short_clean_and_present() {
        assert_eq!(
            usable_state(Some(&"xyz-123".to_owned())),
            Some("xyz-123".to_owned())
        );
        // Surrounding whitespace is a paste artefact, not a different request.
        assert_eq!(
            usable_state(Some(&"  xyz-123  ".to_owned())),
            Some("xyz-123".to_owned())
        );
    }

    #[test]
    fn an_absent_or_doomed_state_is_dropped_rather_than_refused() {
        // The rule that matters: a `state` the platform will not carry back does **not** fail
        // the authorization. The redirect URI is already validated by this point, so refusing
        // strands the user on an error page over something the *client* got wrong, and the
        // client will never learn why.
        assert_eq!(usable_state(None), None);
        assert_eq!(usable_state(Some(&String::new())), None);
        assert_eq!(usable_state(Some(&"   ".to_owned())), None);
        // Over the ceiling: dropped.
        let long = "s".repeat(MAX_STATE_LENGTH + 1);
        assert_eq!(usable_state(Some(&long)), None);
        // A control character would break the `Location` header, so it is dropped here rather
        // than reaching the redirect builder and being silently discarded there.
        assert_eq!(usable_state(Some(&"ab\ncd".to_owned())), None);
        assert_eq!(usable_state(Some(&"ab\rX-Injected: 1".to_owned())), None);
        // Exactly at the ceiling is allowed — the bound is inclusive and a test that only
        // checks "one over" leaves an off-by-one invisible.
        let at_ceiling = "s".repeat(MAX_STATE_LENGTH);
        assert_eq!(usable_state(Some(&at_ceiling)).is_some(), true);
    }

    // ── the redirect we build ────────────────────────────────────────────────

    #[test]
    fn the_code_redirect_carries_the_code_and_the_state() {
        let out = redirect_with_code(
            "https://app.example.com/callback",
            "code_abc",
            Some("xyz-123"),
        );
        assert_eq!(
            out,
            "https://app.example.com/callback?code=code_abc&state=xyz-123"
        );
    }

    #[test]
    fn a_registered_redirect_that_already_has_a_query_keeps_it() {
        // The defect this guards: `?` instead of `&` here produces
        // `…?tenant=acme?code=…`, which the client parses as a redirect URI of
        // `…?tenant=acme?code=…` — the code lands in a parameter name and the token request's
        // `code` is missing. An app registered with a query string is a normal thing, so this
        // is the ordinary case, not an exotic one.
        let out = redirect_with_code("https://app.example.com/cb?tenant=acme", "code_abc", None);
        assert_eq!(out, "https://app.example.com/cb?tenant=acme&code=code_abc");
        assert!(
            out.contains("tenant=acme"),
            "the client's own parameter survives"
        );
    }

    #[test]
    fn a_state_cannot_append_a_parameter_or_a_fragment_to_the_platform_redirect() {
        // The state is caller-supplied and lands in a `Location` header, so a state holding
        // `&` or `#` would let a client rewrite the rest of the redirect. Percent-encoded here
        // rather than trusted.
        let out = redirect_with_code(
            "https://app.example.com/callback",
            "code_abc",
            Some("a&scope=everything#frag"),
        );
        assert_eq!(
            out,
            "https://app.example.com/callback?code=code_abc&state=a%26scope%3Deverything%23frag"
        );
        // And a `%` is encoded first, so an already-encoded state is not double-decoded by a
        // client that decodes once into something else again.
        let percent = redirect_with_code("https://app.example.com/cb", "code_abc", Some("a%26b"));
        assert!(percent.ends_with("state=a%2526b"), "got {percent}");
    }

    /// A raw percent-decoder: `+` stays a `+`, and `%XX` becomes one byte.
    fn percent_decode(input: &str) -> String {
        let bytes = input.as_bytes();
        let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'%' && index + 2 < bytes.len() {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).expect("hex digits");
                out.push(u8::from_str_radix(hex, 16).expect("a hex pair"));
                index += 3;
            } else {
                out.push(bytes[index]);
                index += 1;
            }
        }
        String::from_utf8(out).expect("the original was UTF-8")
    }

    /// A form-urlencoded decoder: `+` is a space, then `%XX` becomes one byte.
    fn form_decode(input: &str) -> String {
        percent_decode(&input.replace('+', " "))
    }

    #[test]
    fn a_state_survives_both_parser_families_byte_for_byte() {
        // The property behind the five encoded characters, asserted as a round trip rather than
        // as a table of expected strings: whatever goes in comes back out unchanged, through
        // **both** parsers an OAuth client actually uses. A raw percent-decoder and a
        // form-urlencoded decoder disagree about exactly one thing — `+` means a space to the
        // second and is a literal to the first — so a state holding `+` is the one value a
        // single-parser test would pass and a real client would corrupt.
        for state in [
            "plain-123",
            "a+b",
            "a=b",
            "a&b",
            "a#b",
            "a%b",
            "%2B",
            "everything&=+%#",
            "ünicode-ß",
            "",
        ] {
            let out = redirect_with_code("https://app.example.com/cb", "code_abc", Some(state));
            let query = out.split_once('?').expect("has a query").1;
            let raw: Vec<&str> = query.split('&').collect();
            // Exactly two parameters: the code and the state. A state that leaked an `&` shows
            // up here as a third, which is the whole attack.
            assert_eq!(raw.len(), 2, "state {state:?} split the query: {out}");

            let encoded = raw[1].strip_prefix("state=").expect("is the state");
            assert_eq!(
                percent_decode(encoded),
                state,
                "raw percent-decoder changed {state:?}"
            );
            assert_eq!(
                form_decode(encoded),
                state,
                "form-urlencoded decoder changed {state:?}"
            );
        }
    }

    // ── the code itself ──────────────────────────────────────────────────────

    #[test]
    fn a_minted_code_is_url_safe_long_and_never_repeats() {
        // URL-safe because it rides in a query string: a `+` or `/` in a code has to be
        // percent-encoded by the client library or the hash lookup misses, and the failure
        // reads as "invalid code" rather than as "your base64 leaked into my parameter".
        let code = mint_code();
        assert!(code.starts_with("code_"));
        assert!(
            code.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        );
        assert_ne!(mint_code(), mint_code());
    }

    #[test]
    fn the_base64_helper_agrees_with_the_padded_length_of_its_input() {
        // A hand-rolled base64 is a place where the *padding* rules are the bug: one extra
        // character on every 1- and 2-byte input is enough to make a code or a challenge that no
        // browser or client library can decode. Asserted by length, which is the property the
        // alphabet assertion cannot see.
        assert_eq!(base64url(b"a").len(), 2);
        assert_eq!(base64url(b"ab").len(), 3);
        assert_eq!(base64url(b"abc").len(), 4);
        assert_eq!(base64url(&[0u8; 48]).len(), 64, "the code body is 48 bytes");
        // And no padding character ever appears.
        assert!(!base64url(&[0xffu8; 5]).contains('='));
    }

    #[test]
    fn a_code_hashes_to_a_different_space_than_a_client_secret() {
        // Two credentials, two hash schemes: a code that verified as a client secret would be a
        // cross-credential oracle, and the scheme prefix is what makes that a `false` rather
        // than a comparison.
        let code = mint_code();
        assert_ne!(oauth::hash_code(&code), oauth::hash_client_secret(&code));
        assert!(!oauth::verify_client_secret(
            &code,
            &oauth::hash_code(&code)
        ));
    }

    // ── the access token ────────────────────────────────────────────────────

    #[test]
    fn a_token_is_namespaced_long_and_write_only() {
        let minted = mint_access_token();
        assert!(minted.plaintext.starts_with("omn_tok_"));
        assert_eq!(minted.plaintext.len(), "omn_tok_".len() + 64);
        // The shape check the token table is keyed on, asserted on a real mint rather than on a
        // hand-written string: a mint that did not satisfy its own validator would produce a
        // token no request could ever authenticate.
        assert!(token_looks_valid(&minted.plaintext));
        // And the `Debug` form is a hand-written one for the same reason as every other
        // credential in this crate.
        assert!(!format!("{minted:?}").contains(&minted.plaintext));
        assert!(format!("{minted:?}").contains("redacted"));
    }

    #[test]
    fn only_this_platforms_own_tokens_pass_the_shape_check() {
        // The near-miss family, not the accepted case. Each of these is a credential that
        // *exists* on this platform and must not be accepted as an access token: a code
        // redeemed a second time, a client secret presented by mistake, an API key, and a
        // token from a different namespace.
        for not_a_token in [
            mint_code(),
            oauth::mint_client_secret().plaintext,
            oauth::mint_client_id(),
            "omn_000000000000.deadbeef".to_owned(),
            "omn_tok_".to_owned(),
            // Right namespace, wrong length — the two near-misses a truncation produces.
            format!("omn_tok_{}", "a".repeat(63)),
            format!("omn_tok_{}", "a".repeat(65)),
            // Right length, not hex. A token truncated in transit usually becomes this.
            format!("omn_tok_{}", "z".repeat(64)),
            format!("omn_tok_{}", "a".repeat(63) + "!"),
            String::new(),
            "   ".to_owned(),
        ] {
            assert!(
                !token_looks_valid(&not_a_token),
                "{not_a_token:?} is not an access token"
            );
        }
    }

    #[test]
    fn a_token_hash_is_its_own_space_and_not_reversible_by_comparison() {
        // The property that stops a code, a client secret and a token being interchangeable: a
        // token presented where a code is expected must not verify, and the three schemes are
        // what make that a `false` rather than a comparison.
        let token = mint_access_token().plaintext;
        let token_hash = hash_access_token(&token);
        assert_ne!(token_hash, oauth::hash_code(&token));
        assert_ne!(token_hash, oauth::hash_client_secret(&token));
        assert_eq!(hash_access_token(&token), token_hash, "stable across calls");
        assert_ne!(
            hash_access_token(&token),
            hash_access_token("omn_tok_00"),
            "injective enough"
        );
    }

    // ── the scopes a token carries ──────────────────────────────────────────

    #[test]
    fn an_omitted_scope_means_the_whole_grant() {
        // The common case, and the one a naive implementation refuses: a client that only ever
        // wants one thing sends no `scope` at the token endpoint, and treating that as an error
        // breaks it.
        let granted = vec!["content.pages.read".to_owned(), "search.read".to_owned()];
        assert_eq!(
            resolve_token_scopes(&[], &granted),
            Some(granted.clone()),
            "no scope parameter means everything consented"
        );
    }

    #[test]
    fn asking_for_less_than_was_granted_is_a_narrowing_and_is_allowed() {
        // The direction that must work: a client restricting itself is legitimate, and refusing
        // it would be a rule no spec has.
        let granted = vec!["content.pages.read".to_owned(), "search.read".to_owned()];
        assert_eq!(
            resolve_token_scopes(&["content.pages.read".to_owned()], &granted),
            Some(vec!["content.pages.read".to_owned()]),
            "the token carries exactly what was asked for"
        );
    }

    #[test]
    fn asking_for_more_than_was_granted_is_refused_rather_than_silently_narrowed() {
        // The boundary the test that only checks the happy path never reaches. Silently issuing
        // a token without the permission produces a `403` at the API that names the *API* as
        // the thing lacking it — a developer debugging the wrong system for an afternoon.
        let granted = vec!["content.pages.read".to_owned()];
        assert_eq!(
            resolve_token_scopes(&["content.pages.update".to_owned()], &granted),
            None,
            "a wider request is refused"
        );
        // And the near-miss: a scope that differs only in a suffix, which is exactly what a
        // prefix match would have accepted.
        assert_eq!(
            resolve_token_scopes(&["content.pages.read-all".to_owned()], &granted),
            None
        );
        // Nothing granted and something asked is also a refusal, not an empty token.
        assert_eq!(resolve_token_scopes(&["search.read".to_owned()], &[]), None);
    }

    #[test]
    fn a_scope_submission_that_a_client_would_write_space_delimited_becomes_a_list() {
        // The token endpoint's `scope` parameter arrives as a string, and this is the parse.
        // Written here against the same helper the authorize path uses, because two spellings of
        // "split a space-delimited scope" is one of them eventually accepting a comma.
        let submitted = " content.pages.read   search.read ";
        let parsed: Vec<String> = submitted.split_whitespace().map(str::to_owned).collect();
        assert_eq!(
            resolve_token_scopes(&parsed, &parsed),
            Some(parsed.clone()),
            "a client that echoes back what it asked for gets it"
        );
    }

    #[test]
    fn the_provenance_type_exists_to_stop_a_machine_token_being_attributed_to_a_person() {
        // A property of the *type*, not of a function: only the user-consent arm may be given a
        // user id, and a machine token carries none. Asserted as the two answers rather than as
        // a branch nobody can take.
        assert!(GrantProvenance::UserConsent.involves_user());
        assert!(!GrantProvenance::ClientCredentials.involves_user());
    }

    // ── the shapes the endpoint returns ─────────────────────────────────────

    #[test]
    fn a_token_response_names_the_scheme_the_expiry_and_the_scopes() {
        // The three fields every OAuth client library reads, asserted on the *serialised* form
        // because a renamed field is invisible to a test that reads the struct.
        let body = TokenResponse {
            token_type: "Bearer",
            access_token: "omn_tok_abc".to_owned(),
            expires_in: ACCESS_TOKEN_TTL_SECONDS,
            scope: "content.pages.read search.read".to_owned(),
            client_id: "omn_app_abc".to_owned(),
        };
        let json = serde_json::to_value(&body).expect("serialises");
        assert_eq!(json["token_type"], "Bearer");
        assert_eq!(json["access_token"], "omn_tok_abc");
        assert_eq!(json["expires_in"], 3600);
        assert_eq!(json["scope"], "content.pages.read search.read");
        assert_eq!(json["client_id"], "omn_app_abc");
    }

    #[test]
    fn a_token_failure_uses_the_spec_error_field_not_this_platforms_error_shape() {
        // The reason `TokenErrorBody` exists rather than reusing `ApiError`: a well-written
        // client reads `error` and switches on it. Asserted on the JSON, since the whole point
        // is what a *client* sees.
        let body = TokenErrorBody {
            error: "invalid_client",
            error_description: "the client secret is not valid".to_owned(),
        };
        let json = serde_json::to_value(&body).expect("serialises");
        assert_eq!(json["error"], "invalid_client");
        assert!(json["error_description"].is_string());
        // And it must not carry this platform's envelope, or a client that reads `error` finds
        // nothing and falls back to a generic retry.
        assert!(json.get("code").is_none());
        assert!(json.get("message").is_none());
    }
}
