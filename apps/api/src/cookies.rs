//! Session cookie helpers.
//!
//! Sessions live server-side: the client holds an opaque random token in an HttpOnly cookie
//! and the database stores only its hash (see `crates/identity`). The cookie is therefore
//! never readable from JavaScript and never contains anything but the token.

use axum::http::HeaderMap;
use axum::http::header::COOKIE;

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
}
