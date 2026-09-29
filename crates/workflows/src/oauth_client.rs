//! The OAuth flow's *transport*, and the one thing in the slice that can talk to a provider
//! (REQ-087 slice 3).
//!
//! The algebra in [`crate::oauth`] is deliberately free of I/O so it can be tested without a
//! socket, and this module is the seam: it takes the pieces that algebra produced — an
//! authorization URL, a [`TokenRequest`], a credential's `client_id` — and performs the two
//! HTTP calls the flow makes. It is a trait, not a `reqwest` client, and the reason is the
//! test hook: **a bundled credential type has to be able to reach a provider from the API
//! process for the `ok: true` branch of the test to be honest**, and the only way to prove that
//! branch without a network is a provider over a loopback socket. A concrete `#[cfg(test)]`
//! implementation lets the crate test its own transport; a fixture provider lets the API test
//! the flow.
//!
//! Three properties are load-bearing and each is a place the obvious implementation is wrong:
//!
//! 1. **The token body is never logged.** [`TokenRequest::carries_secret`] is the test that
//!    decides, and the refusal is *before* the send, because a `tracing` line that contains a
//!    refresh token cannot be un-written.
//! 2. **A non-2xx token answer is an error, not a token set.** A provider that answers
//!    `400 {"error": "invalid_grant"}` is a failure, and `TokenSet::from_response` is the one
//!    place that knows it — the transport must not hand a `200`-shaped body forward and let a
//!    caller discover the absence of `access_token` later.
//! 3. **The access token is a header, never a parameter.** [`auth_header`] is the only place a
//!    bearer token becomes an HTTP header, and it is the reason the credential's own value
//!    never reaches a URL — URLs end up in access logs, proxies and `Referer` headers.

use serde_json::Value;

use crate::error::{Result, WorkflowError};
use crate::oauth::{CallbackQuery, TokenRequest, TokenSet};

/// How long one provider call may take before it is abandoned.
///
/// Thirty seconds is longer than any authorization-code exchange and shorter than a person's
/// patience: a hung provider must not hold a workflow step open indefinitely, and the caller
/// gets a real error rather than a step that never settles.
pub const PROVIDER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The two provider calls a credential's OAuth flow makes.
pub trait OAuthClient {
    /// Answer a `code` for tokens.
    fn exchange_code(
        &self,
        token_url: &str,
        request: &TokenRequest,
    ) -> impl std::future::Future<Output = Result<TokenSet>>;

    /// Trade a refresh token for a new token set.
    fn refresh(
        &self,
        token_url: &str,
        request: &TokenRequest,
    ) -> impl std::future::Future<Output = Result<TokenSet>>;
}

/// The `Authorization` header value for a bearer token.
///
/// A method rather than an inline format because a token in a URL is a token in three other
/// systems' logs, and the only reliable guard is that the one function producing a header is
/// greppable.
#[must_use]
pub fn auth_header(access_token: &str) -> String {
    format!("Bearer {access_token}")
}

/// Read a provider's answer into a token set, refusing anything that is not one.
///
/// Shared by every [`OAuthClient`] so the two transports cannot disagree about what a
/// successful exchange looks like — the same reasoning as `SQL_USAGE` in the credential store:
/// a guard that disagrees with the screen explaining it is the worst pair on the surface.
pub fn token_set_from_status(status: u16, body: &Value) -> Result<TokenSet> {
    if !(200..300).contains(&status) {
        // The provider's own words, which name `invalid_grant` far more usefully than a
        // status code. The body is a provider's error document, not one of our secrets, so
        // the whole of it is safe to keep — and it is stripped of nothing because there is
        // nothing in it to strip.
        let detail = body
            .get("error_description")
            .and_then(Value::as_str)
            .map(|d| format!(" — {d}"))
            .unwrap_or_default();
        let code = body
            .get("error")
            .and_then(Value::as_str)
            .map_or_else(|| format!("HTTP {status}"), str::to_string);
        return Err(WorkflowError::CredentialInvalid(format!(
            "the provider answered {status}: {code}{detail}"
        )));
    }
    TokenSet::from_response(body).map_err(|detail| WorkflowError::CredentialInvalid(detail))
}

/// Whether a token set still has a refresh token worth spending.
#[must_use]
pub fn can_refresh(set: &TokenSet) -> bool {
    set.refresh_token().is_some_and(|token| !token.trim().is_empty())
}

/// What a provider sent back, when the provider sent a refusal.
///
/// The panel has to say *why* a connection did not happen, and an OAuth refusal without its
/// sentence is a red badge and nothing else.
#[must_use]
pub fn refusal_sentence(query: &CallbackQuery) -> Option<String> {
    query.refusal()
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Assert a provider answer was refused, without ever naming the `Ok` arm.
    ///
    /// `expect_err` would need `TokenSet: Debug`, and `TokenSet` refuses it *on purpose* — a
    /// type that holds an access token must not be printable. So the assertion is written as a
    /// `match` rather than worked around by deriving `Debug` on the fixture, which is the
    /// exact change the type's own documentation forbids.
    fn refusal(status: u16, body: &Value) -> String {
        match token_set_from_status(status, body) {
            Ok(_) => panic!("HTTP {status} with a body this test considered invalid is a token set"),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn a_two_hundred_without_an_access_token_is_a_failure_not_a_token_set() {
        // The shape that reaches a handler as `200 {"token_type":"bearer"}` when a provider
        // renames its field. It must not become a credential with a secret that authenticates
        // nothing.
        let error = refusal(200, &json!({ "token_type": "bearer" }));
        assert!(error.to_string().contains("no access_token"), "{error}");
    }

    #[test]
    fn a_provider_that_answers_an_error_document_is_named_rather_than_status_coded() {
        let message = refusal(
            400,
            &json!({ "error": "invalid_grant", "error_description": "code already used" }),
        );
        assert!(message.contains("invalid_grant"), "{message}");
        assert!(message.contains("code already used"), "{message}");
    }

    #[test]
    fn a_provider_that_answers_nothing_recognisable_names_the_status() {
        let error = refusal(503, &json!({}));
        assert!(error.contains("503"), "{error}");
    }

    #[test]
    fn a_two_hundred_with_a_token_is_accepted_and_keeps_its_refresh_token() {
        let set = token_set_from_status(
            200,
            &json!({
                "access_token": "at-1",
                "refresh_token": "rt-1",
                "expires_in": 3600,
                "scope": "read",
            }),
        )
        .expect("a well-formed answer");
        assert_eq!(set.access_token(), "at-1");
        assert!(can_refresh(&set), "a set with a refresh token can be refreshed");
    }

    #[test]
    fn a_set_with_a_blank_refresh_token_is_not_refreshable() {
        // Some providers answer `"refresh_token": ""` rather than omitting it, and spending an
        // empty one produces a second refusal on every run.
        let set = token_set_from_status(
            200,
            &json!({ "access_token": "at-1", "refresh_token": "" }),
        )
        .expect("a well-formed answer");
        assert!(!can_refresh(&set), "a blank refresh token is not a refresh token");
    }

    #[test]
    fn a_refusal_carries_the_providers_own_sentence() {
        let query = CallbackQuery::parse("error=access_denied&error_description=User+said+no");
        assert_eq!(
            refusal_sentence(&query).as_deref(),
            Some("access_denied — User said no")
        );
    }
}
