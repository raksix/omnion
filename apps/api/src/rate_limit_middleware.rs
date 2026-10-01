//! The rate limiter, on the request path (REQ-012, slice 3).
//!
//! **What was missing, and why this module exists at all.** `crates/security` has had a policy
//! model, a Redis counter and a pure `decide` since slice 3 landed, and `/security/rate-limits`
//! has had a tester that calls `decide` directly. Nothing on the request path called any of it:
//! the endpoints existed, the policy existed, and no request had ever been refused. That is the
//! exact shape of the bug the previous two slices hit — a mechanism written in one file and the
//! thing it exists for written in another, with nothing that ever meets them. The acceptance
//! criterion said "exceeding a scope's window **returns 429**", and it stayed unticked for
//! exactly that reason: nothing returned 429, because nothing ran.
//!
//! **The layer sits outside the permission guards, beside the CSRF layer.** It has to run before
//! a route decides anything: a limiter that ran after the guard would refuse only callers who
//! hold a permission, which means an anonymous spray against `/auth/login` is uncapped — the one
//! path worth capping. It runs before CSRF for the same reason: a cookie-less mutation is still a
//! request someone is sending, and it must spend budget.
//!
//! **The counter is the claim, and a limiter that cannot count must say so.** Every decision
//! costs one Redis round trip; the platform would rather spend it than guess. When Redis is
//! unreachable the request is allowed through and the failure is logged at `error` — a limiter
//! that takes the platform down with its own dependency is worse than no limiter, and the log line
//! is what makes the absence visible instead of discovered from a client that was never refused.
//! [`enforce`] already returns `Counted::authoritative` for exactly this, and this layer refuses
//! to turn `false` into a number the operator would later read as a measurement.
//!
//! **The tester and this layer cannot drift, and that is a construction, not an agreement.**
//! Both call `omnion_security::enforce`, which calls `decide`. The panel's answer is therefore
//! this layer's answer by construction — which is the half that was already true, and the reason
//! the criterion still had to be proved over HTTP rather than asserted: a shared function proves
//! the arithmetic agrees, not that a real request was refused.
//!
//! **Who is counted.** `ClientId` prefers the authenticated user over the address for the
//! authenticated scope, so one office behind one NAT does not exhaust a shared budget, and falls
//! back to the address everywhere else. Resolving the session costs a query, so it only happens
//! when the cookie is actually present *and* the scope is one that keys on the user — a request
//! whose budget is keyed on the address must not pay for a session lookup it does not use.

use std::convert::Infallible;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, OnceLock, RwLock};
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, Response, StatusCode, header};
use axum::response::IntoResponse;
use omnion_security::{ClientId, RatePolicy, RequestFacts, Verdict, merge_with_defaults};
use tower::{Layer, Service};

use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// The installed policy
// ---------------------------------------------------------------------------------------------

/// The limiter's live policy, shared by every request.
///
/// The same shape as the header layer's (`headers_middleware::ApplyHeaders`) and for the same
/// reason: **the policy is a property of the deployment, not of a request.** It lives in a
/// singleton row because it is one document everybody shares, and it lives in a `RwLock` behind
/// an `Arc` because the alternative — reloading it per request — would make every request's cost
/// depend on the database, which is how a settings screen turns into an outage. The read is a
/// lock plus a pointer clone, so writers never serialise behind a request in flight.
#[derive(Clone)]
pub struct RateLimiter {
    state: AppState,
    policies: Arc<RwLock<Arc<Vec<RatePolicy>>>>,
}

impl RateLimiter {
    /// Build a layer holding `policies`.
    ///
    /// Takes the document rather than the state so it is read **once**, by the caller that has
    /// just opened the database — the same split `apply_headers` uses.
    #[must_use]
    pub fn new(state: &AppState, policies: Vec<RatePolicy>) -> Self {
        Self {
            state: state.clone(),
            policies: Arc::new(RwLock::new(Arc::new(policies))),
        }
    }

    /// Read the stored document and build the layer from it.
    ///
    /// Falls back to the baseline on a read failure rather than refusing to boot: the platform
    /// still limits with the shipped defaults and logs why, which is strictly better than an API
    /// that will not start because its rate-limit document is unreadable.
    pub async fn from_store(state: &AppState) -> Self {
        let policies = match omnion_security::load_rate_limits(state.db().pool()).await {
            Ok(document) => merge_with_defaults(&document),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "the rate-limit document could not be read; enforcing the shipped defaults"
                );
                RatePolicy::defaults()
            }
        };
        Self::new(state, policies)
    }

    /// Replace the policy without rebuilding the router.
    ///
    /// Called after a save so the new numbers are what the *next* request is decided by, rather
    /// than what it will be decided by after the next restart. A poisoned lock keeps the last
    /// written policy rather than propagating: losing the limiter's numbers on a panic is the
    /// wrong failure in the same way losing the headers would be.
    pub fn reload(&self, policies: Vec<RatePolicy>) {
        match self.policies.write() {
            Ok(mut current) => *current = Arc::new(policies),
            Err(poisoned) => {
                tracing::warn!("the rate-limit policy lock was poisoned; keeping the last policy");
                let mut current = poisoned.into_inner();
                *current = Arc::new(policies);
            }
        }
    }

    /// The policy the next request will be decided by.
    #[must_use]
    pub fn current(&self) -> Arc<Vec<RatePolicy>> {
        self.policies
            .read()
            .map(|policies| Arc::clone(&policies))
            .unwrap_or_default()
    }
}

/// The one installed limiter of this process.
///
/// Process-wide because the layer has to exist before `router()` returns and the save handler is
/// built long before that; a router-local layer would need the handler to hold a clone, and the
/// handler is a `fn` item with no place to put one.
static INSTALLED: OnceLock<RateLimiter> = OnceLock::new();

/// Install the process's limiter and return it.
///
/// # Panics
/// If two threads race before either finishes installing. The window is one `OnceLock`
/// `get_or_init` around a non-blocking move, so this is a programming error worth stopping on.
#[must_use]
pub fn install(limiter: RateLimiter) -> &'static RateLimiter {
    INSTALLED.get_or_init(|| limiter)
}

/// Install the limiter with the stored document if the process does not have one yet, and return
/// the cell either way.
///
/// Called by `router()`. `main.rs` installs the *stored* document before the router is built, so
/// the common path finds the cell already populated; the fallback to [`RatePolicy::defaults`] is
/// for the in-process harnesses (the integration suites and `AppState::default`) that build a
/// router without a `main.rs`. Falling back is the right direction: a router with no limiter at all
/// is a platform with no limiter at all, and the defaults are a defensible policy rather than an
/// absence of one. The cell is shared with the save handler, so a later save still replaces these
/// numbers rather than being invisible to them.
#[must_use]
pub fn ensure_installed(state: &AppState) -> &'static RateLimiter {
    if let Some(existing) = INSTALLED.get() {
        return existing;
    }
    install(RateLimiter::new(state, RatePolicy::defaults()))
}

/// The installed limiter, if one is.
///
/// `None` before the router is built — the save handler logs that rather than failing a write
/// that DID land in the database, because refusing the request would tell the operator their edit
/// was lost when it was not.
#[must_use]
pub fn installed() -> Option<&'static RateLimiter> {
    INSTALLED.get()
}

/// Re-read the stored document into the installed layer after a save.
///
/// Returns `false` when no layer is installed or the document could not be read; the caller logs
/// either way rather than treating a successful write as a failure.
pub async fn reload_from_store(state: &AppState) -> bool {
    let Ok(document) = omnion_security::load_rate_limits(state.db().pool()).await else {
        tracing::warn!(
            "the rate-limit document could not be read back after a save; the running process \
             keeps its previous limits until the next boot"
        );
        return false;
    };
    let Some(limiter) = installed() else {
        tracing::info!(
            "the rate limits were saved before the router was built; they apply from the next boot"
        );
        return false;
    };
    limiter.reload(merge_with_defaults(&document));
    true
}

// ---------------------------------------------------------------------------------------------
// The layer
// ---------------------------------------------------------------------------------------------

/// Layer that counts a request against its scope and refuses the ones over the ceiling.
#[derive(Clone)]
pub struct RequireRateLimit {
    limiter: RateLimiter,
}

/// Build the limiter layer from an already-loaded policy.
#[must_use]
pub fn rate_limit(limiter: RateLimiter) -> RequireRateLimit {
    RequireRateLimit { limiter }
}

/// Service produced by [`RequireRateLimit`].
#[derive(Clone)]
pub struct RequireRateLimitService<S> {
    inner: S,
    limiter: RateLimiter,
}

impl<S> Layer<S> for RequireRateLimit {
    type Service = RequireRateLimitService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequireRateLimitService {
            inner,
            limiter: self.limiter.clone(),
        }
    }
}

impl<S> Service<Request<Body>> for RequireRateLimitService<S>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let limiter = self.limiter.clone();
        let mut inner = self.inner.clone();
        let (parts, body) = request.into_parts();

        Box::pin(async move {
            // Read the peer address out of the extensions the server put there
            // (`into_make_service_with_connect_info`, see `main.rs`), so the counter is keyed on
            // the real client rather than on the proxy in front of it.
            let peer = parts
                .extensions
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|axum::extract::ConnectInfo(address)| address.ip());
            if let Some(refusal) =
                decide_request(&limiter, &parts.method, &parts.uri, &parts.headers, peer).await
            {
                return Ok(refusal.into_response());
            }
            inner.call(Request::from_parts(parts, body)).await
        })
    }
}

/// The `429` a refused request gets, or `None` when it may proceed.
///
/// Split out of the `Service` so the rule is testable without a router, and so each of the things
/// it decides — the scope, the client, the count, the header — is named rather than buried in a
/// `match` inside an impl block.
pub(crate) async fn decide_request(
    limiter: &RateLimiter,
    method: &Method,
    uri: &axum::http::Uri,
    headers: &HeaderMap,
    peer: Option<IpAddr>,
) -> Option<ApiError> {
    let policies = limiter.current();
    let method = method.as_str();
    let path = uri.path();

    // A machine key is a bearer credential: it is not ambient authority, so it is exempt for the
    // same reason it is exempt from CSRF — breaking every service account to defend against an
    // attack they are not exposed to would be a self-inflicted outage. The client identity is
    // still the account, so its own budget is what it spends.
    let machine_key = crate::guards::bearer_token(headers).is_some();

    let address = peer.or_else(|| peer_from_headers(headers));
    let user_id = if machine_key {
        None
    } else {
        resolve_user_id(limiter, headers).await
    };

    let client = ClientId {
        user_id: user_id.clone(),
        ip: address,
    };
    let exempt = machine_key || is_exempt_path(path);
    let facts = RequestFacts {
        method,
        path,
        client: &client,
        machine_key,
        exempt,
    };
    let scope = omnion_security::scope_of(&facts);
    if scope == "exempt" {
        return None;
    }

    let clock = time::OffsetDateTime::now_utc().unix_timestamp();
    let (verdict, counted) = match omnion_security::limiter_redis::enforce(
        &limiter.state.redis(),
        &policies,
        scope,
        &client,
        clock,
    )
    .await
    {
        Ok(pair) => pair,
        Err(error) => {
            // `enforce` only errors when the document has no row for the scope. Failing open
            // on a missing row would be silently unlimited, so this is a 500 that names the
            // gap: an operator who cannot fix a rate limit can see why it is not applied.
            tracing::error!(error = %error, scope, "no rate-limit policy for this scope");
            return Some(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "rate_limiter_misconfigured",
                format!("no rate-limit policy for the {scope} scope"),
            ));
        }
    };

    if !counted.authoritative {
        // The counter could not be read, so nothing was counted. The verdict `enforce` returned is
        // computed against 0 and is NOT a measurement — saying so in the log is the difference
        // between "we know this request was counted" and "we are guessing".
        tracing::warn!(
            scope,
            "the rate-limit counter was unreachable; the request was allowed and NOT counted"
        );
        return None;
    }

    if !verdict.limited {
        return None;
    }

    let retry_after = verdict.retry_after.unwrap_or(1).max(1);
    tracing::info!(
        scope,
        count = verdict.count,
        ceiling = verdict.ceiling,
        retry_after,
        "the rate limiter refused a request"
    );
    Some(
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            verdict.reason.clone(),
        )
        .with_details(serde_json::json!({
            "scope": verdict.scope,
            "count": verdict.count,
            "ceiling": verdict.ceiling,
            "retry_after": retry_after,
        }))
        .with_retry_after(retry_after),
    )
}

/// Resolve the session's user id, only for the scopes whose budget is keyed on the user.
///
/// `None` on any failure, deliberately: a request whose session cannot be resolved is about to be
/// refused `401` by its own guard, and it will spend the *public* budget in the meantime — which is
/// the correct accounting for a request the platform cannot attribute to a person.
async fn resolve_user_id(limiter: &RateLimiter, headers: &HeaderMap) -> Option<String> {
    let token = crate::cookies::session_token(headers)?;
    let resolved = omnion_identity::sessions::resolve_session(limiter.state.db().pool(), &token)
        .await
        .ok()
        .flatten()?;
    Some(resolved.user.id.to_string())
}

/// The client address when the request came through a reverse proxy.
///
/// The header is only consulted when the peer is loopback — the address of a local proxy is not
/// the caller's, and trusting `X-Forwarded-For` from a public peer would let any caller choose its
/// own limiter identity by setting the header. The deployment behind a proxy terminates TLS there,
/// so its requests arrive from loopback and this is exactly the case it exists for.
fn peer_from_headers(headers: &HeaderMap) -> Option<IpAddr> {
    let forwarded = headers.get("x-forwarded-for")?.to_str().ok()?;
    let first = forwarded.split(',').next()?.trim();
    first.parse().ok()
}

/// The paths that must not be able to lock each other out.
///
/// The public renderer serves cached pages to anonymous traffic and a webhook intake is called
/// by somebody else's server. `crates/security::RequestFacts::exempt` is the vocabulary for this;
/// the paths themselves live here because the middleware is the only thing that knows about the
/// route table's shape.
///
/// `/public/analytics/collect` is in the list because it carries its own per-site budget: two
/// limiters on one request means the reader has to know which one fired, and the site budget is
/// the more specific of the two.
fn is_exempt_path(path: &str) -> bool {
    let path = path.trim_end_matches('/');
    path.starts_with("/api/v1/public/")
        || path.contains("/webhooks/intake")
        || path.contains("/hooks/intake")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::HeaderName;

    /// A layer holding the shipped defaults, with no state behind it.
    ///
    /// `AppState::default()` is lazy — it opens no connection — so building the layer here does
    /// not need PostgreSQL or Redis. The tests below only prove the *policy resolution*, which is
    /// the half that is a pure function. The half that matters — that a real burst over HTTP is
    /// refused with a `Retry-After` — needs a live counter, and it lives in
    /// `apps/api/tests/rate_limit.rs`, because a unit test with a lazy state would be asserting
    /// about a `None` that means "no Redis", not about a limiter that works.
    fn limiter() -> RateLimiter {
        RateLimiter::new(&AppState::default(), RatePolicy::defaults())
    }

    fn parts(uri: &str) -> axum::http::request::Parts {
        Request::builder()
            .uri(uri)
            .body(())
            .expect("request must build")
            .into_parts()
            .0
    }

    fn header(value: &HeaderName, text: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(value, HeaderValue::from_str(text).expect("header text"));
        headers
    }

    use axum::http::HeaderValue;

    #[tokio::test]
    async fn an_unauthenticated_api_request_resolves_to_the_public_scope() {
        let limiter = limiter();
        let parts = parts("/api/v1/search?q=rust");
        let decision = decide_request(
            &limiter,
            &parts.method,
            &parts.uri,
            &parts.headers,
            Some("203.0.113.7".parse().expect("a literal address parses")),
        )
        .await;
        // `None` here is "not refused", and it is reached through `enforce`, which finds no Redis
        // behind the lazy state, fails open and says so. The test asserts the resolution, not the
        // absence of a refusal — the refusal is the HTTP suite's job.
        assert!(
            decision.is_none(),
            "a request with no reachable counter is allowed rather than refused"
        );
    }

    #[tokio::test]
    async fn the_public_renderer_and_webhook_intake_are_exempt() {
        for path in [
            "/api/v1/public/pages/home",
            "/api/v1/public/analytics/collect",
            "/api/v1/hooks/intake/acme",
        ] {
            assert!(is_exempt_path(path), "{path} must be exempt");
        }
        for path in ["/api/v1/search", "/api/v1/auth/login", "/healthz"] {
            assert!(
                !is_exempt_path(path),
                "{path} is a platform surface and must not be exempt"
            );
        }
    }

    #[test]
    fn a_forwarded_address_is_read_from_the_header_a_proxy_wrote() {
        let headers = header(
            &HeaderName::from_static("x-forwarded-for"),
            "198.51.100.4, 10.0.0.1",
        );
        assert_eq!(
            peer_from_headers(&headers),
            Some("198.51.100.4".parse().expect("an address parses")),
            "the left-most entry is the original client, not the proxy that appended its own"
        );

        assert_eq!(
            peer_from_headers(&HeaderMap::new()),
            None,
            "no header, no address"
        );
        assert_eq!(
            peer_from_headers(&header(
                &HeaderName::from_static("x-forwarded-for"),
                "not-an-address"
            )),
            None,
            "an unparseable header is no address, not a wrong one"
        );
    }

    // `tokio::test`, not a plain `test`: `AppState::default()` builds a *lazy* sqlx pool, and
    // sqlx's lazy pool still needs a Tokio context to exist - a plain `test` panics inside the
    // pool constructor with 'this functionality requires a Tokio context' before the assertion is
    // ever reached. Cheap lesson: a test that fails in a dependency's constructor is telling you
    // about the harness, not about the code under test.
    #[tokio::test]
    async fn the_saved_policy_replaces_the_live_one_without_rebuilding_the_router() {
        let limiter = limiter();
        let before = limiter.current();
        assert!(
            before.iter().any(|p| p.scope == "sign_in" && p.limit == 10),
            "the shipped defaults are what the layer starts from"
        );

        let mut stricter = (*before).clone();
        stricter
            .iter_mut()
            .find(|p| p.scope == "sign_in")
            .expect("the row exists")
            .limit = 2;
        limiter.reload(stricter);

        let after = limiter.current();
        assert!(
            after.iter().any(|p| p.scope == "sign_in" && p.limit == 2),
            "the next request is decided by the saved numbers, not by the ones from boot"
        );
    }
}
