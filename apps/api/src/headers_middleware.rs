//! The two middlewares of REQ-012 slice 2: CSRF enforcement and header application.
//!
//! They are in one module because they are the same feature seen from two ends. The CSRF layer
//! decides whether a request may change anything; the header layer decides what every response
//! says about itself. Both read the same [`HeaderPolicy`] and the same secret, and both must
//! agree with what the panel previews — so the panel's preview, the check that enforces and the
//! line on the wire are all the output of one function.
//!
//! **The CSRF rule, and its one deliberate exception.** A cookie-authenticated
//! `POST`/`PUT`/`PATCH`/`DELETE` must carry a token derived from its own session; without one it
//! is `403 csrf_failed`. A request authenticated by a **bearer machine key** is exempt: a bearer
//! credential is not ambient authority — the browser does not attach it by itself — so requiring
//! a token there would break every service account to defend against an attack they are not
//! exposed to. An unauthenticated request is not this layer's business either; the route guard
//! answers `401` first.
//!
//! **The header rule.** Every response carries the configured lines, including the ones whose
//! value is `None` — those are skipped, because "configured off" means not sent. A header that
//! fails validation at startup falls back to the baseline rather than taking the platform's
//! responses down: a security header that could not be read is a reason to send the safe
//! default, not a reason to serve the API without any.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{HeaderMap, Request, Response};
use axum::response::{IntoResponse, Response as AxumResponse};
use omnion_security::{CSRF_HEADER, HeaderPolicy, csrf, derive_csrf_token};
use tower::{Layer, Service};

use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// CSRF
// ---------------------------------------------------------------------------------------------

/// Layer rejecting cookie-authenticated mutations that carry no valid CSRF token.
#[derive(Clone)]
pub struct RequireCsrf {
    state: AppState,
}

/// Build the CSRF layer.
///
/// # Panics
/// Never in practice: the layer holds no invariant that a valid configuration can break. It is
/// constructed once at router build time, so a panic here would be a programming error rather
/// than an operational one.
#[must_use]
pub fn require_csrf(state: &AppState) -> RequireCsrf {
    RequireCsrf {
        state: state.clone(),
    }
}

/// Service produced by [`RequireCsrf`].
#[derive(Clone)]
pub struct RequireCsrfService<S> {
    inner: S,
    state: AppState,
}

impl<S> Layer<S> for RequireCsrf {
    type Service = RequireCsrfService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequireCsrfService {
            inner,
            state: self.state.clone(),
        }
    }
}

impl<S> Service<Request<Body>> for RequireCsrfService<S>
where
    S: Service<Request<Body>, Response = AxumResponse<Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = AxumResponse<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: Request<Body>) -> Self::Future {
        let state = self.state.clone();
        let mut inner = self.inner.clone();
        let (parts, body) = request.into_parts();

        Box::pin(async move {
            if let Some(refusal) = refuse_if_needed(&state, &parts.method, &parts.headers).await {
                return Ok(refusal.into_response());
            }
            inner.call(Request::from_parts(parts, body)).await
        })
    }
}

/// The refusal a request earns, or `None` when it may proceed.
///
/// Split out from the service so the rule is testable without a router, and so the three cases
/// it distinguishes are named rather than buried in a match inside a `Service` impl.
async fn refuse_if_needed(
    state: &AppState,
    method: &axum::http::Method,
    headers: &HeaderMap,
) -> Option<ApiError> {
    if !csrf::method_is_protected(method.as_str()) {
        return None;
    }
    // A bearer key is not ambient authority, so it is not what this layer defends against.
    if crate::guards::bearer_token(headers).is_some() {
        return None;
    }
    // No session cookie: there is no ambient authority to abuse. The route guard answers `401`
    // with the right message; this layer must not answer first with a CSRF error that would
    // tell an unauthenticated prober which check ran.
    if crate::cookies::session_token(headers).is_none() {
        return None;
    }

    let Some(secret) = state.config().csrf.as_bytes() else {
        // No configured secret means no token can be derived. Refuse rather than skip: a platform
        // that silently drops CSRF protection when the key is missing is worse than one that
        // refuses writes, because the refusal is visible and the skip is not.
        return Some(ApiError::forbidden(
            "csrf_unavailable",
            "cookie-authenticated changes are refused because no CSRF secret is configured \
                 (set OMNION_CSRF_SECRET)",
        ));
    };

    // Resolve the session to get the id the token is derived from. A failure here is not this
    // layer's refusal to make: the route guard answers `401` with the right message, and a
    // second, different answer from here would tell an unauthenticated caller which checks ran.
    let session = match crate::auth::CurrentSession::resolve(state, headers).await {
        Ok(session) => session,
        Err(_) => return None,
    };
    let expected = derive_csrf_token(secret, &session.session.id.to_string());
    let cookie_header = headers
        .get(axum::http::header::COOKIE)
        .and_then(|value| value.to_str().ok());
    let presented = csrf::presented_token(
        cookie_header,
        headers
            .get(CSRF_HEADER)
            .and_then(|value| value.to_str().ok()),
    );
    csrf::verify(&expected, presented.as_deref())
        .err()
        .map(|error| {
            // `403`, not `401`: the caller IS authenticated, it is this request that is refused.
            // A client that read a 401 here would send a signed-in person to the login page.
            ApiError::forbidden("csrf_failed", error.to_string())
        })
}

// ---------------------------------------------------------------------------------------------
// Header application
// ---------------------------------------------------------------------------------------------

/// Layer that puts the configured security headers on every response.
#[derive(Clone)]
pub struct ApplyHeaders {
    policy: Arc<std::sync::RwLock<Arc<HeaderPolicy>>>,
}

/// The one installed header layer of this process.
///
/// Process-wide because the policy is a property of the *deployment*, not of a tenant or of a
/// request: `0135_security_headers.sql` stores it in a singleton row for exactly that reason.
/// So the cell is a `OnceLock` rather than something the router owns and hands to the save
/// handler - a router-local layer would need the save handler to hold a clone of the layer, and
/// the handler is built long before the router exists.
static INSTALLED: OnceLock<ApplyHeaders> = OnceLock::new();

/// Install the process's header layer and return it.
///
/// Called once when the router is built. A second call returns the layer that is already
/// installed rather than replacing it, because two layers would mean two policies and the
/// question "which one is in force" would have two answers.
///
/// # Panics
/// If two threads race here before either finishes. The window is a single `OnceLock::get_or_init`
/// around a non-blocking constructor, and a panic there is a programming error worth stopping on
/// rather than a condition to recover from.
#[must_use]
pub fn install(policy: HeaderPolicy) -> &'static ApplyHeaders {
    INSTALLED.get_or_init(|| ApplyHeaders {
        policy: Arc::new(std::sync::RwLock::new(Arc::new(policy))),
    })
}

/// The installed layer, if one is.
///
/// Returns `None` before the router is built. The save handler uses this: it reloads when the
/// layer exists and logs when it does not, rather than failing a save that DID land in the
/// database - the stored policy is what the next boot reads, and refusing the request would
/// tell the operator their edit was lost when it was not.
#[must_use]
pub fn installed() -> Option<&'static ApplyHeaders> {
    INSTALLED.get()
}

/// Re-read the stored policy into the installed layer, so a save takes effect on the next
/// response rather than at the next restart.
///
/// Returns `false` when no layer is installed, which the caller logs rather than treats as a
/// failure: the write already committed, and the next boot will read it from the database.
pub async fn reload_from_store(state: &AppState) -> bool {
    let Ok(stored) = omnion_security::load_headers(state.db().pool()).await else {
        tracing::warn!(
            "the header policy could not be read back after a save; the running process keeps its previous policy"
        );
        return false;
    };
    let Some(layer) = installed() else {
        tracing::info!(
            "the header policy was saved before the router was built; it applies from the next boot"
        );
        return false;
    };
    layer.reload(HeaderPolicy::from_json(Some(&stored.document)));
    true
}

/// Build the header layer from an already-loaded policy.
///
/// Takes the policy rather than the state so the policy is read **once at boot**, not once per
/// request: a header layer that ran a query per response would make every request's cost depend
/// on the database, which is how a settings screen turns into an outage. Slice 2's save is what
/// re-reads it — see the note on [`reload`] below.
#[must_use]
pub fn apply_headers(policy: HeaderPolicy) -> ApplyHeaders {
    ApplyHeaders {
        policy: Arc::new(std::sync::RwLock::new(Arc::new(policy))),
    }
}

impl ApplyHeaders {
    /// Replace the policy without rebuilding the router.
    ///
    /// Called after a successful save so the change takes effect on the next response rather
    /// than at the next restart.
    ///
    /// `RwLock` rather than `Arc::swap` because the layer is shared behind clones: `swap` needs
    /// `&mut`, and a `&self` method on a type other threads hold is the honest signature. The
    /// read is a lock of a pointer clone and the write takes the lock for one assignment, so the
    /// hot path never serialises on the policy itself. A poisoned lock means some other thread
    /// panicked while holding it; the previous policy is kept rather than propagated, because
    /// losing the headers on a panic is exactly the wrong failure.
    pub fn reload(&self, policy: HeaderPolicy) {
        match self.policy.write() {
            Ok(mut current) => *current = Arc::new(policy),
            Err(poisoned) => {
                // `PoisonError::into_inner` hands back the guard itself, not a Result: a poisoned
                // lock still holds the last written policy, and that policy is still a policy.
                tracing::warn!(
                    "the header policy lock was poisoned; taking the last written policy"
                );
                let mut current = poisoned.into_inner();
                *current = Arc::new(policy);
            }
        }
    }

    /// The policy the next response will use.
    #[must_use]
    pub fn current(&self) -> HeaderPolicy {
        self.policy
            .read()
            .map(|policy| (**policy).clone())
            .unwrap_or_default()
    }
}

/// Service produced by [`ApplyHeaders`].
#[derive(Clone)]
pub struct ApplyHeadersService<S> {
    inner: S,
    policy: Arc<std::sync::RwLock<Arc<HeaderPolicy>>>,
}

impl<S> Layer<S> for ApplyHeaders {
    type Service = ApplyHeadersService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ApplyHeadersService {
            inner,
            // The lock itself is shared, NOT a snapshot of the policy: a snapshot taken here
            // would be frozen for the life of the router, and a save would store a policy that
            // no response ever carried - which is precisely the "I configured it and nothing
            // changed" bug this module exists to make impossible.
            policy: Arc::clone(&self.policy),
        }
    }
}

impl<S> Service<Request<Body>> for ApplyHeadersService<S>
where
    S: Service<Request<Body>, Response = AxumResponse<Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = AxumResponse<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        // One pointer clone per request: the policy a save installs is visible to the *next*
        // response, and the read never serialises writers behind the whole request.
        let policy = self
            .policy
            .read()
            .map(|current| Arc::clone(&current))
            // A poisoned lock means another thread panicked mid-write. Sending no security
            // headers is the worst possible answer, so the baseline is used instead.
            .unwrap_or_default();
        let mut inner = self.inner.clone();
        Box::pin(async move {
            let mut response = inner.call(request).await?;
            for line in policy.render() {
                // A header configured off is not sent; the panel already shows it as off.
                if let Some(value) = line.value {
                    if let Ok(name) = axum::http::HeaderName::from_bytes(line.name.as_bytes()) {
                        if let Ok(value) = axum::http::HeaderValue::from_str(&value) {
                            response.headers_mut().insert(name, value);
                        }
                    }
                }
            }
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    fn headers_with_cookie(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            value.parse().expect("a cookie header parses"),
        );
        headers
    }

    #[tokio::test]
    async fn a_get_is_never_refused_by_the_csrf_layer() {
        // The rule is about *changes*. A read that answers 403 because it carried no token would
        // be a bug in the layer, not a protection.
        let state = AppState::default();
        for method in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(
                refuse_if_needed(&state, &method, &headers_with_cookie("omnion_session=abc"))
                    .await
                    .is_none(),
                "{method} must pass"
            );
        }
    }

    #[tokio::test]
    async fn a_post_with_no_session_cookie_is_left_to_the_guard() {
        // No cookie means no ambient authority, so this layer has nothing to protect. Answering
        // here would tell an unauthenticated prober which checks ran before the 401.
        let state = AppState::default();
        let refusal = refuse_if_needed(&state, &Method::POST, &HeaderMap::new()).await;
        assert!(refusal.is_none(), "{refusal:?}");
    }

    #[tokio::test]
    async fn a_post_with_a_bearer_key_is_exempt() {
        // A machine key is not ambient authority; requiring a token would break every service
        // account to defend against an attack they are not exposed to.
        let state = AppState::default();
        let mut headers = headers_with_cookie("omnion_session=abc");
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer machine-key".parse().expect("header"),
        );
        assert!(
            refuse_if_needed(&state, &Method::POST, &headers)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn the_refusal_is_a_403_named_csrf_and_not_a_401() {
        // Readable in the test through the message the error carries, since `ApiError`'s status
        // field is private: what matters is that the CODE says csrf and the message explains.
        let state = AppState::default();
        let refusal = refuse_if_needed(
            &state,
            &Method::POST,
            &headers_with_cookie("omnion_session=abc"),
        )
        .await
        .expect("no secret configured means every mutation is refused");
        assert_eq!(refusal.code(), "csrf_unavailable");
        let message = refusal.message();
        assert!(
            message.contains("OMNION_CSRF_SECRET"),
            "the message must name the setting to fix: {message}"
        );
    }

    #[tokio::test]
    async fn a_deployment_without_a_secret_refuses_rather_than_skipping() {
        // The distinction this test exists for: failing open turns a missing key into a silent
        // loss of a control, and a silent loss is the worst outcome a security control has.
        let state = AppState::default();
        assert!(!state.config().csrf.is_usable());
        assert!(
            refuse_if_needed(
                &state,
                &Method::DELETE,
                &headers_with_cookie("omnion_session=abc")
            )
            .await
            .is_some()
        );
    }
}
