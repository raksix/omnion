//! CSRF protection for cookie-authenticated mutations (REQ-012, slice 2).
//!
//! The session cookie is `SameSite=Lax`, which stops a cross-site `POST` from carrying it. That is
//! a real protection and it is not enough, for three reasons this module exists for:
//!
//! * **Lax does not cover everything.** A top-level GET navigation *does* carry a Lax cookie, so
//!   any state change reachable by GET is exposed. The token below closes that regardless.
//! * **Lax is a browser setting, not a server decision.** An operator who changes it, a client
//!   that ignores it, or a future `SameSite=None` for an embedded panel all remove the one
//!   control we did not choose. A server-checked token is a decision we do choose.
//! * **"It is only cookies, so it is fine" is the wrong mental model.** Any site the operator
//!   visits while signed in can issue the request; the browser attaches our cookie and the
//!   server has no way to tell that the intent came from another origin.
//!
//! The design is the double-submit token bound to the session, and the decisions that make it
//! safe rather than decorative:
//!
//! * **A machine key never needs a token.** A bearer credential is not sent ambiently by a
//!   browser, so requiring a token there would break every service account for no gain. The
//!   token guards *ambient* authority — cookies — and nothing else.
//! * **The token is derived, not stored.** `HMAC(secret, session id)` means there is no table to
//!   rotate, nothing to leak from a dump, and a token from one session is useless in another.
//! * **A mismatch is `403 csrf_failed`, never `401`.** The caller *is* authenticated; the request
//!   is what is being refused, and a client that treats this as "sign in again" would send the
//!   person to a login page when their session is perfectly valid.
//! * **A token with no session is refused, not ignored.** "No session → no cookie → no ambient
//!   authority" is a separate, earlier question, answered by `401` from the guard.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::{Result, SecurityError};

type HmacSha256 = Hmac<Sha256>;

/// Name of the cookie the token is handed out in.
///
/// Deliberately *not* HttpOnly: the browser has to read it to send it back in a header, which is
/// the whole mechanism. The token is not a credential on its own — it is worthless without the
/// session cookie, and it is compared with a constant-time equality.
pub const CSRF_COOKIE: &str = "omnion_csrf";

/// Name of the request header the token is echoed back in.
pub const CSRF_HEADER: &str = "x-omnion-csrf";

/// How long a token stays valid, in seconds.
///
/// Bound to the session's own lifetime rather than to a clock of its own: a token that outlived
/// its session would be a token an old, leaked page could replay. The cookie's `Max-Age` is set
/// to the session's remaining life by the caller, so there is nothing to synchronise here.
pub const CSRF_TTL_SECONDS: i64 = 8 * 60 * 60;

/// Request methods that may change state and therefore need a token.
///
/// `GET`, `HEAD` and `OPTIONS` are excluded: `OPTIONS` is how a browser asks permission for a
/// cross-origin request, and answering it with 403 would break the preflight for the very request
/// the token is supposed to protect.
pub const PROTECTED_METHODS: &[&str] = &["POST", "PUT", "PATCH", "DELETE"];

/// `true` when this method needs a token.
#[must_use]
pub fn method_is_protected(method: &str) -> bool {
    PROTECTED_METHODS
        .iter()
        .any(|protected| method.eq_ignore_ascii_case(protected))
}

/// Derive the token for a session.
///
/// The session id, not the token or the user id: a session id is unpredictable, unique, and
/// rotates on every sign-in — so a stolen token cannot be replayed into another session, and
/// signing out and back in invalidates the old token without touching a store.
#[must_use]
pub fn derive_token(secret: &[u8], session_id: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("an HMAC accepts any key length");
    // A domain-separated prefix so this derivation can never collide with another use of the same
    // secret elsewhere in the platform — the reason HMAC keys are shared across subsystems at all.
    mac.update(b"omnion.csrf.v1:");
    mac.update(session_id.as_bytes());
    base64_url_encode(&mac.finalize().into_bytes())
}

/// Compare a presented token against the expected one.
///
/// Constant-time, and it compares *bytes*: two tokens that are equal are equal, and two that are
/// not differ in the first byte that differs. A `==` on two attacker-controlled strings of
/// different lengths leaks the length through timing.
#[must_use]
pub fn tokens_match(expected: &str, presented: &str) -> bool {
    // Hash both sides to the same fixed length first, then compare. One domain for BOTH: two
    // different prefixes would make every pair of digests differ by construction, which is the
    // bug this shape exists to avoid — the domain is here to separate this comparison from
    // `derive_token`, not the two sides from each other.
    let digest = |value: &[u8]| {
        let mut mac =
            HmacSha256::new_from_slice(b"omnion.csrf.compare").expect("an HMAC accepts any key");
        mac.update(b"omnion.csrf.token:");
        mac.update(value);
        mac.finalize().into_bytes()
    };
    constant_time_eq(&digest(expected.as_bytes()), &digest(presented.as_bytes()))
}

/// The refusal a failed check produces.
///
/// # Errors
/// Always [`SecurityError::Invalid`] with a message the panel can show: the reason is never "no",
/// it is *what* was wrong.
pub fn refuse(presented: Option<&str>, expected: &str) -> SecurityError {
    SecurityError::invalid(match presented {
        None => format!(
            "this request carries no CSRF token — the session cookie is ambient authority, so the \
             \"{CSRF_HEADER}\" header (or an \"{CSRF_COOKIE}\" cookie) is required"
        ),
        // The same comparison `verify` uses, not `==`: a guard that disagrees with the check
        // it documents turns a valid token into a panic instead of a pass.
        Some(token) if tokens_match(expected, token) => {
            unreachable!("a matching token is not refused")
        }
        Some(_) => format!(
            "the CSRF token does not belong to this session — read it from the \"{CSRF_COOKIE}\" \
             cookie and send it in the \"{CSRF_HEADER}\" header"
        ),
    })
}

/// Verify a mutation.
///
/// `presented` is whatever the request carried, in either place, and `expected` is the token the
/// session derives. The header wins over the cookie when both are present and they disagree —
/// reading the cookie as a fallback would let a stale cookie silently authorise a request whose
/// header was deliberately wrong.
///
/// # Errors
/// [`SecurityError::Invalid`] when the token is absent or does not match.
pub fn verify(expected: &str, presented: Option<&str>) -> Result<()> {
    match presented {
        Some(token) if tokens_match(expected, token) => Ok(()),
        other => Err(refuse(other, expected)),
    }
}

/// The `Set-Cookie` value that hands the token to the browser.
#[must_use]
pub fn token_cookie(token: &str, max_age_seconds: i64, secure: bool) -> String {
    let mut cookie =
        format!("{CSRF_COOKIE}={token}; Path=/; SameSite=Strict; Max-Age={max_age_seconds}");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// The `Set-Cookie` value that takes the token away on sign-out.
#[must_use]
pub fn cleared_cookie(secure: bool) -> String {
    let mut cookie = format!("{CSRF_COOKIE}=; Path=/; SameSite=Strict; Max-Age=0");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Base64url without padding — the two characters `=` and `+` are not worth carrying through a
/// cookie and a header for no gain.
fn base64_url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3f) as usize] as char);
        }
    }
    out
}

/// Byte-wise equality over two equal-length slices, without an early return.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Pick the presented token out of a cookie header and an `x-omnion-csrf` header.
///
/// The header is read first and the cookie is only a fallback, because a client that sends both
/// means the header — a page that read a stale cookie and also set the header from a fresh token
/// is describing two different intentions, and the explicit one is the one to honour.
#[must_use]
pub fn presented_token(
    cookie_header: Option<&str>,
    request_header: Option<&str>,
) -> Option<String> {
    if let Some(value) = request_header
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(value.to_owned());
    }
    cookie_header?
        .split(';')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| name.trim() == CSRF_COOKIE)
        .map(|(_, value)| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"a-test-secret-long-enough-to-be-realistic";

    #[test]
    fn a_token_is_derived_the_same_way_twice() {
        let first = derive_token(SECRET, "session-a");
        let second = derive_token(SECRET, "session-a");
        assert_eq!(first, second);
    }

    #[test]
    fn a_token_from_one_session_is_not_another_sessions_token() {
        assert_ne!(
            derive_token(SECRET, "session-a"),
            derive_token(SECRET, "session-b")
        );
    }

    #[test]
    fn a_token_derived_with_another_secret_does_not_verify() {
        let expected = derive_token(SECRET, "session-a");
        let forged = derive_token(b"a-different-secret", "session-a");
        assert!(!tokens_match(&expected, &forged));
    }

    #[test]
    fn a_token_is_url_safe_and_unpadded() {
        let token = derive_token(SECRET, "session-a");
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "a cookie value with + or = would need escaping: {token}"
        );
        assert!(!token.contains('='));
    }

    #[test]
    fn the_verified_token_passes() {
        let token = derive_token(SECRET, "session-a");
        assert!(verify(&token, Some(&token)).is_ok());
    }

    #[test]
    fn a_missing_token_is_refused_with_a_message_that_names_the_header() {
        let token = derive_token(SECRET, "session-a");
        let error = verify(&token, None).expect_err("a mutation with no token must be refused");
        let message = error.to_string();
        assert!(message.contains(CSRF_HEADER), "{message}");
        assert!(message.contains("carries no CSRF token"), "{message}");
    }

    #[test]
    fn a_wrong_token_is_refused_and_the_reason_says_why() {
        let token = derive_token(SECRET, "session-a");
        let error = verify(&token, Some("not-the-token")).expect_err("a wrong token is refused");
        assert!(
            error
                .to_string()
                .contains("does not belong to this session"),
            "{error}"
        );
    }

    #[test]
    fn a_token_from_another_session_is_refused() {
        let mine = derive_token(SECRET, "session-a");
        let theirs = derive_token(SECRET, "session-b");
        assert!(
            verify(&mine, Some(&theirs)).is_err(),
            "a valid token from a different session must not pass — that is the whole point"
        );
    }

    #[test]
    fn a_token_of_a_different_length_is_refused_without_a_length_comparison_shortcut() {
        let mine = derive_token(SECRET, "session-a");
        assert!(!tokens_match(&mine, ""));
        assert!(!tokens_match(&mine, &format!("{mine}x")));
        assert!(verify(&mine, Some("")).is_err());
    }

    #[test]
    fn the_three_mutating_methods_need_a_token_and_the_safe_ones_do_not() {
        for method in ["POST", "put", "PATCH", "delete"] {
            assert!(method_is_protected(method), "{method} must be protected");
        }
        for method in ["GET", "HEAD", "OPTIONS", "TRACE"] {
            assert!(
                !method_is_protected(method),
                "{method} must not be protected"
            );
        }
    }

    #[test]
    fn options_is_not_protected_so_a_preflight_still_answers() {
        // A browser asks permission with OPTIONS *before* it can send the protected request; a
        // 403 here would make the token un-sendable from another origin, which is the case the
        // token exists for.
        assert!(!method_is_protected("OPTIONS"));
    }

    #[test]
    fn the_header_wins_when_the_cookie_and_the_header_disagree() {
        let cookie = format!("{CSRF_COOKIE}=from-cookie; other=1");
        let picked = presented_token(Some(&cookie), Some("from-header")).expect("a token");
        assert_eq!(picked, "from-header");
    }

    #[test]
    fn the_cookie_is_a_fallback_when_there_is_no_header() {
        let cookie = format!("theme=dark; {CSRF_COOKIE}=from-cookie; x=1");
        assert_eq!(
            presented_token(Some(&cookie), None).as_deref(),
            Some("from-cookie")
        );
    }

    #[test]
    fn a_blank_header_falls_back_to_the_cookie_rather_than_verifying_nothing() {
        let cookie = format!("{CSRF_COOKIE}=from-cookie");
        assert_eq!(
            presented_token(Some(&cookie), Some("   ")).as_deref(),
            Some("from-cookie")
        );
    }

    #[test]
    fn no_token_anywhere_is_none() {
        assert_eq!(presented_token(None, None), None);
        assert_eq!(presented_token(Some("theme=dark"), None), None);
        assert_eq!(
            presented_token(Some(&format!("{CSRF_COOKIE}=")), None),
            None
        );
    }

    #[test]
    fn the_token_cookie_is_same_site_strict_and_not_http_only() {
        let cookie = token_cookie("abc", 3_600, false);
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Max-Age=3600"));
        assert!(
            !cookie.contains("HttpOnly"),
            "the browser has to read it — that is the mechanism"
        );
        assert!(
            !cookie.contains("Secure"),
            "a development panel on http cannot use a Secure cookie"
        );
        assert!(token_cookie("abc", 3_600, true).ends_with("; Secure"));
    }

    #[test]
    fn the_cleared_cookie_expires_immediately() {
        let cookie = cleared_cookie(false);
        assert!(cookie.contains("Max-Age=0"), "{cookie}");
        assert!(cookie.starts_with(&format!("{CSRF_COOKIE}=;")));
    }

    #[test]
    fn the_token_survives_the_cookie_round_trip_unchanged() {
        // The whole mechanism: a cookie that carries a value needing quoting is a broken
        // double-submit, because the browser sends back what it can parse.
        let token = derive_token(SECRET, "session-a");
        let cookie = token_cookie(&token, 3_600, false);
        let jar = cookie.split(';').next().expect("the name=value pair");
        let picked = presented_token(Some(jar), None).expect("a token");
        assert_eq!(picked, token);
        assert!(verify(&token, Some(&picked)).is_ok());
    }

    #[test]
    fn base64url_encodes_the_three_tail_lengths_without_padding() {
        assert_eq!(base64_url_encode(b"f"), "Zg");
        assert_eq!(base64_url_encode(b"fo"), "Zm8");
        assert_eq!(base64_url_encode(b"foo"), "Zm9v");
        assert_eq!(base64_url_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64url_never_emits_a_plus_or_a_slash() {
        // Bytes that encode to + and / in standard base64: the case that breaks a cookie.
        let bytes: [u8; 3] = [0xfb, 0xff, 0xfe];
        let encoded = base64_url_encode(&bytes);
        assert!(!encoded.contains('+'), "{encoded}");
        assert!(!encoded.contains('/'), "{encoded}");
        assert_eq!(encoded, "-__-");
    }
}
