//! What `support::walk_auth` guarantees, proved without a database.
//!
//! The defect this module exists to close was **invisible to compilation and invisible to review**:
//! twenty helpers were shaped the same way, they compiled, and every write they made was refused
//! at runtime with a message naming the *server*. So the parser is tested here directly — and
//! the first case is the input that caused it: a sign-in that issues **two** cookies.

mod support;

use support::walk_auth::{CSRF_COOKIE, CSRF_HEADER, PASSWORD, SESSION_COOKIE, Session};

/// The exact shape of a real sign-in response: the session first, the token beside it.
fn two_cookies() -> Vec<String> {
    vec![
        format!("{SESSION_COOKIE}=sess-abc; Path=/; HttpOnly; SameSite=Lax"),
        format!("{CSRF_COOKIE}=tok-xyz; Path=/; SameSite=Lax"),
    ]
}

#[test]
fn both_cookies_are_kept_which_is_the_whole_point() {
    let session = Session::from_set_cookies(two_cookies());
    assert_eq!(session.session, "sess-abc");
    assert_eq!(session.csrf.as_deref(), Some("tok-xyz"));
}

#[test]
fn a_helper_that_reads_only_the_first_cookie_loses_the_token() {
    // The expression twenty helpers used, kept here as an executable warning: it is correct for
    // one cookie and silently discards everything after it. A helper that keeps it signs in
    // holding a session with no token, and every write it makes is refused with a code that
    // names the deployment's configuration rather than the helper.
    let first = two_cookies()[0].clone();
    let kept = first.split(';').next().unwrap_or_default().to_owned();
    assert_eq!(kept, format!("{SESSION_COOKIE}=sess-abc"));
    assert!(
        !kept.contains(CSRF_COOKIE),
        "the token is not in the first cookie, which is the defect"
    );
}

#[test]
fn the_cookie_header_carries_both_values() {
    let session = Session::from_set_cookies(two_cookies());
    let header = session.cookie_header();
    assert!(header.contains(&format!("{SESSION_COOKIE}=sess-abc")));
    assert!(header.contains(&format!("{CSRF_COOKIE}=tok-xyz")));
}

#[test]
fn a_write_carries_the_token_in_the_header_as_well_as_the_cookie() {
    use axum::http::Request;

    let session = Session::from_set_cookies(two_cookies());
    let request = session
        .apply(Request::builder().method("POST").uri("/api/v1/media"))
        .body(())
        .expect("the request builds");

    let cookies = request
        .headers()
        .get("cookie")
        .and_then(|value| value.to_str().ok())
        .expect("the cookie header is text");
    assert!(
        cookies.contains(CSRF_COOKIE),
        "the cookie is the value to compare against"
    );

    let echoed = request
        .headers()
        .get(CSRF_HEADER)
        .and_then(|value| value.to_str().ok())
        .expect("a write must echo the token in the header");
    assert_eq!(echoed, "tok-xyz");
}

#[test]
#[should_panic(expected = "must issue a omnion_csrf cookie")]
fn a_deployment_that_issues_no_token_fails_at_sign_in_where_the_message_can_name_it() {
    // Panicking here is the design: twenty assertions further down would each rediscover the
    // same fact, and the last one would report it as an authorization decision.
    let _ = Session::from_set_cookies(vec![format!("{SESSION_COOKIE}=sess-abc")]);
}

#[test]
fn the_login_body_is_the_one_the_route_expects() {
    let body = Session::login_body("walker@example.test");
    assert_eq!(body["email"], "walker@example.test");
    assert_eq!(body["password"], PASSWORD);
    assert!(
        !PASSWORD.contains("sk-"),
        "a test password, not a credential"
    );
}
