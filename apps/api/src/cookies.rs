//! Session cookie helpers.
//!
//! Sessions live server-side: the client holds an opaque random token in an HttpOnly cookie
//! and the database stores only its hash (see `crates/identity`). The cookie is therefore
//! never readable from JavaScript and never contains anything but the token.

use axum::http::HeaderMap;
use axum::http::header::COOKIE;

use omnion_security::{csrf, csrf::CSRF_TTL_SECONDS};

/// Name of the session cookie.
pub const SESSION_COOKIE: &str = "omnion_session";

/// Extract the session token from the request headers.
///
/// Tolerates several `Cookie` headers and whitespace around names and values.
#[must_use]
pub fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|raw| raw.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            if name.trim() != SESSION_COOKIE {
                return None;
            }
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_owned())
        })
}

/// `Set-Cookie` value that starts a session.
#[must_use]
pub fn session_cookie(token: &str, max_age_seconds: i64, secure: bool) -> String {
    let mut cookie = format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_seconds}"
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// `Set-Cookie` value that clears the session cookie on the client.
#[must_use]
pub fn cleared_session_cookie(secure: bool) -> String {
    let mut cookie = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Derive this session's CSRF token, or `None` when the deployment has no secret.
///
/// **The one place a token is minted.** Every sign-in path calls it, so "the layer refuses
/// cookie-authenticated mutations" and "the browser was ever given a token to send" cannot
/// drift apart — the first draft of the CSRF slice wired the layer into the router and never
/// called this, so every panel mutation answered `403 csrf_failed` with nothing the client
/// could have sent.
///
/// The token's `Max-Age` is the session's own remaining life, not the CSRF module's constant:
/// a token that outlives its session is a token an old, leaked page could replay, and the
/// session cookie it guards disappears first. The constant is only the ceiling.
#[must_use]
pub fn csrf_cookie_for(
    session_id: &uuid::Uuid,
    secret: Option<&[u8]>,
    secure: bool,
) -> Option<String> {
    let secret = secret?;
    let token = omnion_security::derive_csrf_token(secret, &session_id.to_string());
    Some(csrf::token_cookie(
        &token,
        sessions_max_age().min(CSRF_TTL_SECONDS),
        secure,
    ))
}

/// The session's remaining life, as the session cookie is issued it.
const fn sessions_max_age() -> i64 {
    omnion_identity::sessions::SESSION_TTL_SECONDS
}

/// The `Set-Cookie` values a sign-out has to send: the session and its CSRF token.
///
/// Two headers, not one. A cleared session cookie with a surviving CSRF cookie leaves a
/// token in the browser that nothing will ever check again, and a token from the next sign-in
/// replaces it — so it is not a live credential, but it is a stale one sitting in a cookie jar
/// that no longer means anything, which is exactly what a sign-out is supposed to stop.
#[must_use]
pub fn signout_cookies(secure: bool) -> [String; 2] {
    [cleared_session_cookie(secure), csrf::cleared_cookie(secure)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(cookies: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in cookies {
            headers.append(COOKIE, value.parse().expect("cookie header"));
        }
        headers
    }

    #[test]
    fn reads_the_token_from_a_plain_cookie_header() {
        let headers = headers(&["omnion_session=abc123; theme=dark"]);
        assert_eq!(session_token(&headers).as_deref(), Some("abc123"));
    }

    #[test]
    fn reads_the_token_from_any_of_several_cookie_headers() {
        let headers = headers(&["theme=dark", "omnion_session=abc123"]);
        assert_eq!(session_token(&headers).as_deref(), Some("abc123"));
    }

    #[test]
    fn tolerates_whitespace_and_missing_cookies() {
        let spaced = headers(&["  omnion_session = abc123 ; x=1"]);
        assert_eq!(session_token(&spaced).as_deref(), Some("abc123"));
        assert_eq!(session_token(&headers(&["theme=dark"])), None);
        assert_eq!(session_token(&HeaderMap::new()), None);
    }

    #[test]
    fn empty_values_are_not_a_token() {
        let empty = headers(&["omnion_session="]);
        assert_eq!(session_token(&empty), None);
    }

    #[test]
    fn session_cookie_is_http_only_and_lax() {
        let cookie = session_cookie("token-value", 2_592_000, false);
        assert!(
            cookie.starts_with("omnion_session=token-value; "),
            "{cookie}"
        );
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("Max-Age=2592000"));
        assert!(
            !cookie.contains("Secure"),
            "dev cookies stay usable over http"
        );

        let production = session_cookie("token-value", 2_592_000, true);
        assert!(production.ends_with("; Secure"));
    }

    #[test]
    fn cleared_cookie_expires_immediately() {
        let cookie = cleared_session_cookie(false);
        assert!(cookie.contains("Max-Age=0"), "{cookie}");
        assert!(cookie.starts_with("omnion_session=;"));
    }

    /// A fixed session id, so the derived token is a fixed value and the test asserts the
    /// *shape* rather than a coincidence.
    fn session_id() -> uuid::Uuid {
        uuid::Uuid::parse_str("11111111-2222-3333-4444-555555555555").expect("a valid uuid")
    }

    #[test]
    fn the_token_cookie_is_readable_by_script_and_bound_to_the_session() {
        let cookie =
            csrf_cookie_for(&session_id(), Some(b"s3cret"), true).expect("a secret is set");
        // Not HttpOnly: the browser has to read it to echo it in a header, which is the whole
        // mechanism. It is not a credential without the session cookie.
        assert!(cookie.starts_with("omnion_csrf="), "{cookie}");
        assert!(!cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Strict"), "{cookie}");
        assert!(cookie.contains("Path=/"), "{cookie}");
        assert!(cookie.ends_with("; Secure"), "{cookie}");

        let other = csrf_cookie_for(
            &uuid::Uuid::parse_str("66666666-7777-8888-9999-000000000000").expect("a uuid"),
            Some(b"s3cret"),
            true,
        )
        .expect("a secret is set");
        assert_ne!(
            cookie, other,
            "a token from one session must be useless in another"
        );
    }

    #[test]
    fn no_secret_means_no_token_rather_than_an_empty_one() {
        // The refusal path in the middleware answers `403 csrf_unavailable`; handing out an
        // empty token here would make the browser send one and turn that into a `csrf_failed`
        // that names the wrong problem.
        assert_eq!(csrf_cookie_for(&session_id(), None, false), None);
    }

    #[test]
    fn the_token_outlives_nothing_it_guards() {
        let cookie = csrf_cookie_for(&session_id(), Some(b"s3cret"), false).expect("a token");
        let max_age: i64 = cookie
            .split("Max-Age=")
            .nth(1)
            .and_then(|rest| rest.split(';').next())
            .and_then(|value| value.parse().ok())
            .unwrap_or_default();
        assert!(max_age > 0, "{cookie}");
        assert!(
            max_age <= omnion_identity::sessions::SESSION_TTL_SECONDS,
            "a token may not outlive the session it guards ({max_age})"
        );
    }

    #[test]
    fn a_signout_clears_both_cookies() {
        let [session, token] = signout_cookies(false);
        assert!(session.contains("Max-Age=0"));
        assert!(token.contains("Max-Age=0"), "{token}");
        assert!(token.starts_with("omnion_csrf=;"));
    }
}
