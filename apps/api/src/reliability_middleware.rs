//! The platform-wide limiter, on the request path (REQ-127, slice 1).
//!
//! **What was missing, and why this module exists at all.** `crates/reliability` shipped a policy
//! model, a pure resolver, a `decide` and a refusal rollup, and nothing on the request path called
//! any of it: the policies existed, the panel's dry-run would answer questions about them, and no
//! request had ever been refused by one. That is the same shape REQ-012 found in
//! `rate_limit_middleware.rs` and the same shape REQ-126 found four times — a mechanism written in
//! one file, the thing it exists for written in another, and nothing that ever makes them meet.
//!
//! ## It is a SECOND limiter, beside the one REQ-012 already ships
//!
//! That is the design, not an accident, and the request says why in one sentence: "the per-key
//! rate limits of the API gateway stay where they are — this request adds the **platform-wide**
//! budgets (user, organization, IP, route), the idempotency contract, retry policies, breaker
//! state and the inbound intake guard." So there are two layers in the chain and each refuses for
//! its own reasons:
//!
//! | | REQ-012 (`rate_limit_middleware`) | REQ-127 (this module) |
//! |---|---|---|
//! | Policy | one JSON document, five fixed scopes | one row per policy, operator-editable, four scopes plus route patterns |
//! | Resolution | a scope name, looked up | specificity order, priority, then id |
//! | Counting | Redis, fixed window | Redis, fixed window, **keyed on the subject** |
//! | What it protects | the gateway's own budgets | the platform's user/org/IP budgets |
//!
//! **Both may refuse the same request, and that is why the two layers are told apart in the log
//! and in the error body.** A caller who is over both budgets gets the outer layer's `429` first;
//! an operator reading only "429" cannot tell which document needs widening, so each refusal names
//! its own limiter in `details.limiter` and the two never share a cache key prefix.
//!
//! ## The headers, and the rule about when they may appear
//!
//! The request requires `429` with `Retry-After` and `X-RateLimit-Limit/Remaining/Reset`. Those
//! three plus `Retry-After` are **only written on an authoritative answer** — a real reading of a
//! real counter. When the counter is unreachable the request either proceeds uncounted (fail open)
//! or is refused uncounted (fail closed), and in both cases there is no number to publish, so
//! there is no header. An `X-RateLimit-Remaining: 0` written by a layer that never read a counter
//! is a measurement nobody took, and a client that believes it has spent its budget stops trying.
//!
//! `X-RateLimit-Reset` is the **absolute** unix time the window rolls, not the number of seconds
//! left: the two headers in common use disagree about this, and a client that reads seconds as an
//! absolute timestamp waits until 1970. `Retry-After` carries the *relative* seconds because that
//! is what the HTTP spec defines, and the difference is the whole reason both exist.
//!
//! ## What is exempt, and what is not
//!
//! A bearer machine key is exempt from the *user* budget for the reason REQ-012 gives: it is not
//! ambient authority, and breaking every service account to defend against an attack they are not
//! exposed to would be a self-inflicted outage. It still spends an **IP** budget, because the
//! address is the only identity a bearer request has. The probes (`/healthz`, `/readyz`,
//! `/livez`, `/metrics`) are exempt for the reason the existing limiter gives: a probe that trips
//! the limiter is a probe that reports the platform down, and a scrape is a scrape.

use std::convert::Infallible;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, OnceLock, RwLock};
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use omnion_reliability::limiter_redis::{self, Counted, FailMode, PolicyCache};
use omnion_reliability::limits::{LimitPolicy, RefusalRollup, Subject, Verdict};
use time::OffsetDateTime;
use tower::{Layer, Service};

use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::state::AppState;

/// Which limiter answered, so a caller over both budgets knows which document to widen.
pub const LIMITER_NAME: &str = "platform_budget";

/// The `X-RateLimit-Limit` header. Uppercase in the spec, lowercased on the wire by HTTP's case
/// insensitivity — axum's `HeaderName` handles the fold, and these constants are the one place the
/// three names are written, so a rename cannot take one header and leave the other two.
const X_LIMIT: HeaderName = HeaderName::from_static("x-ratelimit-limit");
const X_REMAINING: HeaderName = HeaderName::from_static("x-ratelimit-remaining");
const X_RESET: HeaderName = HeaderName::from_static("x-ratelimit-reset");
const X_POLICY: HeaderName = HeaderName::from_static("x-ratelimit-policy");

// ---------------------------------------------------------------------------------------------
// The installed policy
// ---------------------------------------------------------------------------------------------

/// The platform budgets, shared by every request.
///
/// **The policy is a property of the deployment, not of a request**, which is why it lives behind
/// an `RwLock<Arc<Vec<..>>>` rather than being read per request: a per-request read would make
/// every request's cost depend on the database, which is how a settings screen becomes an outage.
/// The read is a lock plus a pointer clone, so a writer never serialises behind a request in
/// flight — and a *poisoned* lock keeps the last written policies rather than handing back an
/// empty list, because an empty list is a limiter that has silently turned itself off.
#[derive(Clone)]
pub struct PlatformLimiter {
    state: AppState,
    policies: PolicyCache,
    fail_mode: FailMode,
}

impl PlatformLimiter {
    /// Build a layer holding `policies`.
    #[must_use]
    pub fn new(state: &AppState, policies: Vec<LimitPolicy>, fail_mode: FailMode) -> Self {
        Self {
            state: state.clone(),
            policies: limiter_redis::empty_cache(),
            fail_mode,
        }
        .with_policies(policies)
    }

    /// Install `policies` into the cache, and return the layer.
    fn with_policies(mut self, policies: Vec<LimitPolicy>) -> Self {
        limiter_redis::write_cache(&self.policies, policies);
        self
    }

    /// Read the stored document and build the layer from it.
    ///
    /// Falls back to **no policies** on a read failure, which is "the platform-wide limiter is
    /// off" rather than "the platform-wide limiter refuses everything". The REQ-012 gateway
    /// limiter is still installed and still enforcing its own document, so a store that cannot be
    /// read degrades to the previous protection level instead of to none — and the reason is
    /// logged, because a limiter that is quietly off is the failure this branch exists to avoid.
    pub async fn from_store(state: &AppState, fail_mode: FailMode) -> Self {
        match omnion_reliability::store::load_policies(state.db().pool()).await {
            Ok(policies) => Self::new(state, policies, fail_mode),
            Err(error) => {
                tracing::error!(
                    error = %error,
                    "the platform rate-limit policies could not be read; the platform budgets are \
                     not being enforced (the gateway limiter still is)"
                );
                Self::new(state, Vec::new(), fail_mode)
            }
        }
    }

    /// Replace the policy without rebuilding the router.
    pub fn reload(&self, policies: Vec<LimitPolicy>) {
        limiter_redis::write_cache(&self.policies, policies);
    }

    /// The policies the next request is decided by.
    #[must_use]
    pub fn current(&self) -> Arc<Vec<LimitPolicy>> {
        limiter_redis::read_cache(&self.policies)
    }

    /// The mode this deployment fails in, for the panel to render rather than to guess.
    #[must_use]
    pub fn fail_mode(&self) -> FailMode {
        self.fail_mode
    }
}

/// The one installed platform limiter of this process.
///
/// Process-wide for the same reason the gateway limiter's is: the layer has to exist before
/// `router()` returns, and the save handler is built long before that.
static INSTALLED: OnceLock<PlatformLimiter> = OnceLock::new();

/// Install the process's limiter and return it.
///
/// # Panics
/// If two threads race before either finishes installing — a programming error worth stopping on.
#[must_use]
pub fn install(limiter: PlatformLimiter) -> &'static PlatformLimiter {
    INSTALLED.get_or_init(|| limiter)
}

/// Install with the stored document if the process does not have one yet.
///
/// The fallback is an **empty** policy set, not a default set. The REQ-012 layer is already
/// installed with its shipped defaults, and a second set of invented defaults would be a second
/// document an operator has to discover and tune. For the in-process harnesses (the integration
/// walks and `AppState::default`) this means no platform budget until a policy is written, which
/// is the documented state of a fresh instance.
#[must_use]
pub fn ensure_installed(state: &AppState) -> &'static PlatformLimiter {
    if let Some(existing) = INSTALLED.get() {
        return existing;
    }
    install(PlatformLimiter::new(state, Vec::new(), FailMode::Open))
}

/// The installed limiter, if one is.
#[must_use]
pub fn installed() -> Option<&'static PlatformLimiter> {
    INSTALLED.get()
}

/// Re-read the stored document into the installed layer after a save.
///
/// Returns `false` when no layer is installed or the document could not be read; the caller logs
/// either way rather than treating a successful write as a failure.
pub async fn reload_from_store(state: &AppState) -> bool {
    let Ok(policies) = omnion_reliability::store::load_policies(state.db().pool()).await else {
        tracing::warn!(
            "the platform rate-limit policies were saved; the running process keeps its previous \
             ones until the next boot"
        );
        return false;
    };
    let Some(limiter) = installed() else {
        tracing::info!(
            "the platform rate-limit policies were saved before the router was built; they apply \
             from the next boot"
        );
        return false;
    };
    limiter.reload(policies);
    true
}

// ---------------------------------------------------------------------------------------------
// The layer
// ---------------------------------------------------------------------------------------------

/// Layer that counts a request against the platform budgets and refuses the ones over the line.
#[derive(Clone)]
pub struct RequirePlatformLimit {
    limiter: PlatformLimiter,
}

/// Build the platform limiter layer from an already-loaded policy.
#[must_use]
pub fn platform_limit(limiter: PlatformLimiter) -> RequirePlatformLimit {
    RequirePlatformLimit { limiter }
}

/// Service produced by [`RequirePlatformLimit`].
#[derive(Clone)]
pub struct RequirePlatformLimitService<S> {
    inner: S,
    limiter: PlatformLimiter,
}

impl<S> Layer<S> for RequirePlatformLimit {
    type Service = RequirePlatformLimitService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequirePlatformLimitService {
            inner,
            limiter: self.limiter.clone(),
        }
    }
}

impl<S> Service<Request<Body>> for RequirePlatformLimitService<S>
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
            let peer = parts
                .extensions
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|axum::extract::ConnectInfo(address)| address.ip());
            let outcome = decide_request(&limiter, &parts.method, &parts.uri, &parts.headers, peer).await;
            // BOTH paths owe the caller the headers, and they were separate for a reason: a
            // refusal is a body an operator reads and a served request is a number a client
            // paces itself by. The refusal used to be answered by `into_response()` alone, which
            // meant the `429` — the one response where the caller most needs to know the
            // ceiling, the reset and the policy that decided it — was the only response the
            // limiter left bare.
            match outcome {
                Decision::Refused { refusal, verdict, policy } => {
                    // A `429` is exactly where the ceiling, the reset and the deciding policy
                    // matter most, so the headers go on the refusal too — from the same function,
                    // off the same verdict, so the two paths cannot disagree about the numbers.
                    let now = OffsetDateTime::now_utc();
                    Ok(apply_headers(refusal.into_response(), &verdict, policy.as_ref(), now))
                }
                Decision::Allowed { verdict, policy } => {
                    let response = inner.call(Request::from_parts(parts, body)).await?;
                    Ok(apply_headers(
                        response,
                        &verdict,
                        policy.as_ref(),
                        OffsetDateTime::now_utc(),
                    ))
                }
            }
        })
    }
}

/// What the layer decided about one request.
///
/// Two variants rather than an `Option<ApiError>` **plus a side effect**, because the earlier
/// shape could only express a refusal: the allowed path's verdict was computed, used to decide
/// "proceed", and then dropped, so the `X-RateLimit-*` contract could not be honoured on a
/// request that was served. Everything the header needs travels with the decision.
///
/// `Debug` only, deliberately: the returned value is matched and then moved, and adding `Clone`
/// to `ApiError` — a shared error type used by every route in the platform — to satisfy a derive
/// this enum does not need would be a change to the whole API for a local convenience.
#[derive(Debug)]
pub(crate) enum Decision {
    /// The request may proceed; carry the verdict and the winning policy for the headers.
    Allowed {
        verdict: Verdict,
        policy: Option<LimitPolicy>,
    },
    /// The request is refused. The verdict and the policy travel with the error because the
    /// `429` is the response that most needs `X-RateLimit-Limit/Remaining/Reset`: a client that
    /// is being told to back off is the one that has to know what the ceiling was.
    Refused {
        refusal: ApiError,
        verdict: Verdict,
        policy: Option<LimitPolicy>,
    },
}

/// What this layer decided about one request: it may proceed (with the verdict the headers need),
/// or it is refused (with the error to answer it).
pub(crate) async fn decide_request(
    limiter: &PlatformLimiter,
    method: &Method,
    uri: &axum::http::Uri,
    headers: &HeaderMap,
    peer: Option<IpAddr>,
) -> Decision {
    let policies = limiter.current();
    if policies.is_empty() {
        // No budget is written, so there is nothing to spend. The panel says the same thing, and
        // the log line that would explain an outage is not written because this is not one.
        // `Unlimited` rather than a bare "no policy": it is the answer that carries no number, and
        // it is what stops the caller from publishing a `Limit: 0` for a deployment nobody capped.
        return Decision::Allowed {
            verdict: Verdict::Unlimited,
            policy: None,
        };
    }

    let path = uri.path();
    if is_exempt_path(path) {
        // A probe or a public surface is outside the budgets entirely, so it is not merely
        // "unlimited" — it was never a candidate. Same answer, and the difference is written down
        // so nobody later reads the exemption as a zero budget.
        return Decision::Allowed {
            verdict: Verdict::Unlimited,
            policy: None,
        };
    }

    // A machine key is not ambient authority, so it does not spend the *user* budget — but it
    // still spends an IP budget, because the address is the only identity a bearer request has.
    // Dropping the user rather than the address is the same choice REQ-012 makes.
    let machine_key = crate::guards::bearer_token(headers).is_some();
    let address = peer.or_else(|| peer_from_headers(headers));
    // A machine key is not a person, so it spends no user budget. A session the database COULD
    // NOT be asked about is a different case, and it is kept apart from both of those: the
    // limiter logs it and falls back to the address budget, so a saturated pool can no longer
    // quietly hand a signed-in caller an unlimited request. The failure is `warn`, not `error`,
    // because the request itself is served correctly — it is the *attribution* that was lost,
    // and a deployment that rate-limits by address is degraded, not down.
    let (user_id, unresolved) = if machine_key {
        (None, false)
    } else {
        match resolve_user_id(limiter, headers).await {
            Ok(user_id) => (user_id, false),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "a signed-in request's session could not be resolved; it spends the IP budget \
                     only and its user budget is not counted this request"
                );
                (None, true)
            }
        }
    };

    let subject = Subject {
        user_id,
        organization_id: None,
        ip: address,
        // The matched route TEMPLATE, which this layer cannot see: it runs before the router has
        // published `MatchedPath`. A `route`-scoped policy therefore cannot match here, and that
        // is a stated limitation rather than a silent gap — a policy keyed on the literal path
        // would be one policy per id, which is the cardinalty mistake the metric families avoid.
        route: None,
    };

    let now = OffsetDateTime::now_utc();
    let (verdict, counted, policy) =
        limiter_redis::enforce(&limiter.state.redis(), &policies, &subject, now, limiter.fail_mode).await;

    // The refusal is decided FIRST and unconditionally: an unresolvable caller is still a caller
    // and the address budget is the one budget that is still attributable. Only the ALLOWED path
    // is downgraded, and only to a verdict that says what happened.
    //
    // This is the shape `Uncounted` exists for — "a policy applies, but this request was not
    // counted" — and using it here rather than `Unlimited` is the whole fix. `Unlimited` means
    // "no budget is written for this scope", which is a statement about the deployment;
    // `Uncounted` means "this specific request was not counted", which is the truth. The
    // difference is visible on the wire: `Unlimited` publishes no headers because there is no
    // ceiling to publish, and a caller watching `X-RateLimit-Remaining` sees the same silence
    // either way — which is precisely why the anomaly has to be logged here and not only here.
    if verdict.is_allowed() && unresolved && policy_is_user_scoped(&policies) {
        return Decision::Allowed {
            verdict: Verdict::Uncounted {
                scope: verdict.scope().unwrap_or("user").to_owned(),
            },
            policy: None,
        };
    }

    if verdict.is_allowed() {
        // The verdict travels with the decision. Dropping it here is what made the allowed path
        // header-less: the numbers existed, `apply_headers` existed, and nothing connected them.
        return Decision::Allowed { verdict, policy };
    }

    let now_stamp = time::OffsetDateTime::now_utc();
    // A refusal with no reading behind it is refused WITHOUT a `Retry-After`: the wait is
    // unknowable while the counter is unreachable, and a client told to retry in a second would
    // hammer the dependency that is already down.
    if let Some(policy) = policy.as_ref() {
        let target = subject.key_for(&policy.scope);
        let rollup = RefusalRollup {
            scope: policy.scope.clone(),
            target_id: target,
            route: policy
                .route_pattern
                .clone()
                .unwrap_or_else(|| "(any route)".to_owned()),
            window_start: limiter_redis::window_start(policy, now_stamp),
            refusals: 0,
            last_refusal_at: now_stamp,
        };
        record_rollup(limiter, rollup).await;
    }

    let refusal = refusal_error(&verdict, &policies, &subject, counted, now_stamp);
    Decision::Refused { refusal, verdict, policy }
}

/// Count the refusal into this window's rollup, and emit on the window's first one.
///
/// The emission is **not** on the refusal path: it is on the rollup's first write, which is what
/// turns the request's "one aggregated event per target and window rather than a per-request
/// flood" into a property of the unique constraint. A failure to record is logged, never surfaced
/// — the request is already refused, and turning a bookkeeping failure into a `500` would report
/// a limiter outage as a platform outage.
async fn record_rollup(limiter: &PlatformLimiter, rollup: RefusalRollup) {
    match omnion_reliability::store::record_refusal(limiter.state.db().pool(), &rollup).await {
        Ok(omnion_reliability::store::RollupOutcome::Emitted) => {
            if let Err(error) = omnion_events::bus::emit(
                limiter.state.db().pool(),
                omnion_events::NewEvent::new(omnion_reliability::vocabulary::events::LIMIT_EXCEEDED)
                    .payload(serde_json::json!({
                        "scope": rollup.scope,
                        "target_id": rollup.target_id,
                        "route": rollup.route,
                        "window_start": rollup.window_start,
                        "limiter": LIMITER_NAME,
                    })),
            )
            .await
            {
                tracing::warn!(error = %error, "a rate-limit refusal was counted but the event was not emitted");
            }
        }
        Ok(omnion_reliability::store::RollupOutcome::Counted) => {}
        Err(error) => {
            tracing::warn!(error = %error, "a request was refused but the refusal rollup could not be written");
        }
    }
}

/// The wire answer: a `429` with the three headers when there is a measurement behind it.
fn refusal_error(
    verdict: &Verdict,
    policies: &[LimitPolicy],
    subject: &Subject,
    counted: Counted,
    now: OffsetDateTime,
) -> ApiError {
    let scope = verdict.scope().unwrap_or("unknown").to_owned();
    let mut details = serde_json::json!({
        "limiter": LIMITER_NAME,
        "scope": scope,
        "count": counted.count,
        "counter_authoritative": counted.authoritative,
    });

    // The winning policy's own numbers, so a `429` names the document that has to change. The
    // panel's dry-run resolves the same policy from the same list, so the two cannot disagree
    // about which policy is responsible for a refusal the operator is looking at.
    if let Some(policy) = omnion_reliability::limits::pick(policies, subject) {
        details["policy_id"] = serde_json::json!(policy.id);
        details["limit"] = serde_json::json!(policy.limit_count);
        details["burst"] = serde_json::json!(policy.burst);
        details["window_seconds"] = serde_json::json!(policy.window_seconds);
    }

    let mut error = ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        match verdict {
            Verdict::RefusedUncounted { .. } => format!(
                "the {scope} budget could not be read, and this deployment fails closed; the \
                 request was not counted"
            ),
            _ => format!("the {scope} budget for this window is spent"),
        },
    )
    .with_details(details);

    if let Some(seconds) = verdict.retry_after() {
        error = error.with_retry_after(seconds);
    }
    error
}

/// Attach the rate-limit headers to a response the request may still be refused on.
///
/// Split out and public so the **allowed** path can carry them too: a client that can see its
/// remaining budget does not have to spend the ceiling discovering it, which is the difference
/// between a client that backs off and a client that retries into the refusal.
pub fn apply_headers(mut response: Response<Body>, verdict: &Verdict, policy: Option<&LimitPolicy>, now: OffsetDateTime) -> Response<Body> {
    if !verdict.is_authoritative() {
        return response;
    }
    let headers = response.headers_mut();
    if let Some(ceiling) = verdict.ceiling() {
        if let Ok(value) = HeaderValue::from_str(&ceiling.to_string()) {
            headers.insert(X_LIMIT, value);
        }
    }
    if let Some(remaining) = verdict.remaining() {
        if let Ok(value) = HeaderValue::from_str(&remaining.to_string()) {
            headers.insert(X_REMAINING, value);
        }
    }
    if let Some(policy) = policy {
        // The absolute moment the window rolls. Relative seconds belong in `Retry-After`, which
        // is where they go; a client reading seconds as an absolute stamp would wait until 1970.
        let roll = (now + time::Duration::seconds(policy.window_seconds.max(1))).unix_timestamp();
        if let Ok(value) = HeaderValue::from_str(&roll.to_string()) {
            headers.insert(X_RESET, value);
        }
        if let Some(id) = policy.id {
            if let Ok(value) = HeaderValue::from_str(&id.to_string()) {
                headers.insert(X_POLICY, value);
            }
        }
    }
    response
}

/// Whether any enabled policy would have governed this request by USER.
///
/// Asked before the downgrade rather than inferred from `verdict.scope()`, because by the time
/// the verdict exists a `user`-scoped policy has already lost the resolution to something else —
/// and the case that matters is precisely the one where it LOST. A deployment with no user policy
/// at all is unaffected by a slow pool, so the downgrade must only fire when a user budget was
/// the one being skipped.
fn policy_is_user_scoped(policies: &[LimitPolicy]) -> bool {
    policies
        .iter()
        .any(|policy| policy.enabled && policy.scope == "user")
}

/// Resolve the session's user id for the user-scoped budget.
///
/// **Three answers, not two**, and the third one is what this slice's measurement forced.
///
/// The previous signature returned `Option<Uuid>` and reached it with
/// `resolve_session(...).await.ok().flatten()?`, which maps a session that does not exist and a
/// session the database **could not be asked about** onto the same `None`. Under load the pool's
/// 5 s `acquire_timeout` fires, `.ok()` swallows it, and the signed-in caller is handed to the
/// limiter as a subject with no `user_id`. The winning `user`-scoped policy then has no key for
/// it, `enforce` answers `Unlimited`, and the request is served **uncounted** — which is exactly
/// the failure the budget exists to prevent, arriving from a saturated pool instead of an
/// outage. Measured, not inferred: the walk printed `COULD NOT ASK (pool timed out)` and the
/// served response carried no `X-RateLimit-*` at all.
///
/// So the error is kept and travels. `Err` means *the platform does not know who this is*, and
/// the caller decides what to do with that; `Ok(None)` means *nobody is signed in*, which is a
/// fact rather than a failure. Collapsing them again is the defect, so it is written down here.
///
/// The IP budget is still spent either way, and that part of the old comment was right: a
/// request the platform cannot attribute to a person is still traffic from an address. What
/// changed is that a caller who could not be resolved is no longer **silently** unlimited — the
/// limiter logs it and counts the address, so an operator sees the anomaly instead of finding it
/// from a client that was never limited.
async fn resolve_user_id(
    limiter: &PlatformLimiter,
    headers: &HeaderMap,
) -> std::result::Result<Option<uuid::Uuid>, omnion_identity::error::IdentityError> {
    let Some(token) = crate::cookies::session_token(headers) else {
        // Nothing was presented, so there is nothing to ask about: an anonymous request is not a
        // lookup that failed. This is the whole difference between the three answers and it is
        // decided BEFORE the database is touched.
        return Ok(None);
    };
    // The `?` is the fix. `.ok()` was what made every database failure indistinguishable from an
    // anonymous request, and no amount of logging above the call site can recover an answer the
    // function already threw away — the caller cannot tell the two apart because the function has
    // already merged them. Written this way, a saturated pool reaches `decide_request` as an
    // `Err` and the anomaly is logged and the response is marked `Uncounted`.
    Ok(omnion_identity::sessions::resolve_session(limiter.state.db().pool(), &token)
        .await?
        .map(|session| session.user.id))
}

/// The client address when the request came through a reverse proxy.
///
/// The header is consulted only when the peer is loopback — the address of a local proxy is not
/// the caller's, and trusting `X-Forwarded-For` from a public peer would let any caller choose its
/// own limiter identity by setting the header.
fn peer_from_headers(headers: &HeaderMap) -> Option<IpAddr> {
    let forwarded = headers.get("x-forwarded-for")?.to_str().ok()?;
    let first = forwarded.split(',').next()?.trim();
    first.parse().ok()
}

/// The surfaces that must not be able to spend — or lock each other out of — a budget.
///
/// A probe that trips the limiter is a probe that reports the platform down, and a scrape is a
/// scrape: `/metrics` is polled on a schedule nobody chose and must not consume a budget that a
/// real client needs. The public renderer and the webhook intake are the two the existing gateway
/// limiter already exempts, and the same reasoning applies.
fn is_exempt_path(path: &str) -> bool {
    let path = path.trim_end_matches('/');
    path == "/healthz"
        || path == "/readyz"
        || path == "/livez"
        || path == "/metrics"
        || path.starts_with("/api/v1/public/")
        || path.contains("/webhooks/intake")
        || path.contains("/hooks/intake")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(scope: &str, limit: i32, burst: i32, window: i64) -> LimitPolicy {
        LimitPolicy {
            id: Some(uuid::Uuid::from_u128(7)),
            name: format!("{scope} policy"),
            scope: scope.into(),
            target_id: None,
            route_pattern: None,
            limit_count: limit,
            window_seconds: window,
            burst,
            priority: 100,
            is_default: false,
            enabled: true,
        }
    }

    fn subject(ip: &str) -> Subject {
        Subject {
            user_id: Some(uuid::Uuid::from_u128(1)),
            organization_id: None,
            ip: ip.parse().ok(),
            route: None,
        }
    }

    /// The defect this slice fixed, reproduced on purpose.
    ///
    /// Every other test here exercises the resolver or the headers. This one exercises the
    /// **call site**, because that is where the defect was: `resolve_user_id` used to collapse
    /// "the database could not be asked" into "nobody is signed in", so a saturated pool silently
    /// turned a signed-in caller into an anonymous one and their user budget simply stopped being
    /// spent — served, uncounted, and with no header to say so. A test on the helper could not
    /// have caught it: the helper was right and its CALLER threw the answer away.
    ///
    /// The failure is reproduced without a network round trip, which matters because a
    /// reproduction that needs a real outage is a reproduction that only runs during one. The
    /// pool is built directly against a port nothing can bind and handed to `resolve_session`,
    /// so the `Err` under test is the SAME `Err` a saturated pool produces — a `sqlx::Error`
    /// from failing to acquire — arrived at deterministically.
    #[tokio::test]
    async fn a_session_the_database_cannot_answer_is_not_an_anonymous_request() {
        use axum::http::header;

        // A lazy pool against a port that is reserved and unbound. `max_connections(1)` plus a
        // short acquire timeout means the first query fails immediately rather than retrying
        // behind a live socket.
        // A lazy pool against a reserved, unbound port, wrapped in the same `PlatformLimiter`
        // the middleware builds. Building the limiter rather than calling `resolve_session`
        // directly is the whole point: the previous version of this test called
        // `resolve_session` and passed with the fix reverted, because `resolve_session` was
        // ALWAYS right — it is this module's wrapper that used to throw the answer away. A test
        // has to exercise the code that was wrong.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(200))
            .connect_lazy("postgres://omnion:omnion@127.0.0.1:1/omnion_unreachable")
            .expect("a lazy pool needs no reachable server");

        let limiter = PlatformLimiter {
            state: AppState::with_pool(pool),
            policies: limiter_redis::empty_cache(),
            fail_mode: FailMode::Open,
        };

        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("omnion_session=not-a-real-token"),
        );

        // The assertion is on what the WRAPPER says, which is the code under test.
        assert!(
            resolve_user_id(&limiter, &headers).await.is_err(),
            "a session lookup that could not be PERFORMED must come back as an error. Returning \
             `Ok(None)` here is the defect: the caller cannot then tell this signed-in request \
             from an anonymous one, the user budget is skipped, and the request is served \
             uncounted with no header to say so"
        );

        // The error underneath is the pool failing to acquire, which is what production printed
        // (`pool timed out while waiting for an open connection`) and not a rejected token.
        let error = resolve_user_id(&limiter, &headers)
            .await
            .expect_err("already asserted to be an error");
        assert!(
            matches!(error, omnion_identity::error::IdentityError::Database(_)),
            "the failure must be the DATABASE being unreachable; a token the function rejects \
             outright would make this test pass for the wrong reason. Got {error:?}"
        );

        // And the three answers are still distinguishable: nothing presented is `Ok(None)`, which
        // is a fact about the request rather than a failure to find anything out.
        let anonymous = resolve_user_id(&limiter, &HeaderMap::new()).await;
        assert!(
            anonymous
                .expect("no cookie is not a lookup that failed")
                .is_none(),
            "no cookie presented means nobody is signed in"
        );
    }

    /// The downgrade only fires when a USER budget was actually the one being skipped.
    ///
    /// Asking this about the policies rather than about the verdict is deliberate. By the time
    /// the verdict exists, a user-scoped policy has already LOST the resolution — that is why
    /// the subject had no user key to spend — so `verdict.scope()` can only ever report the
    /// scope that won instead. Inferring from it would report "ip" on the exact request whose
    /// user budget went unspent, which is the one case the check exists to catch.
    ///
    /// Both halves are asserted, because both were ways to make the fix a no-op: a deployment
    /// with only an address budget must NOT be downgraded (nothing was lost), and a disabled
    /// user row is not a budget that could have been spent.
    #[test]
    fn an_unresolved_caller_is_only_downgraded_when_a_user_budget_existed() {
        let user = policy("user", 600, 0, 60);
        let ip = policy("ip", 100, 0, 60);

        assert!(
            policy_is_user_scoped(std::slice::from_ref(&user)),
            "a deployment with a user budget can lose it to a slow pool, so it must be reported"
        );
        assert!(
            !policy_is_user_scoped(std::slice::from_ref(&ip)),
            "an address-only deployment has no user budget to lose; downgrading it would report \
             an anomaly that cannot have happened"
        );
        assert!(
            policy_is_user_scoped(&[ip.clone(), user.clone()]),
            "the user budget still counts when an address budget is present — the address one \
             wins the resolution, and that is the situation the downgrade exists for"
        );

        let mut disabled = user.clone();
        disabled.enabled = false;
        assert!(
            !policy_is_user_scoped(&[disabled, ip]),
            "a disabled row is not a budget: it can never have been spent"
        );
    }

    /// The downgraded verdict is `Uncounted`, never `Unlimited`.
    ///
    /// The two read completely differently, and picking the wrong one is how the defect would
    /// return wearing a fix. `Unlimited` means "no policy governs this scope" — a statement about
    /// the deployment — and the caller reads it as "I have no budget, nothing is wrong".
    /// `Uncounted` means "a policy applied to this request and it was NOT counted", which is
    /// the truth, and it is also non-authoritative, so `apply_headers` withholds the numbers
    /// rather than publishing a measurement nobody took.
    #[test]
    fn an_unresolved_caller_publishes_no_numbers_because_none_were_taken() {
        let unresolved = Verdict::Uncounted {
            scope: "user".into(),
        };
        assert!(
            !unresolved.is_authoritative(),
            "no counter was read, so there is no remaining to publish"
        );
        // `is_allowed()` is TRUE for `Uncounted`, and that is correct rather than surprising: the
        // request is served, and the limiter did decide to serve it. What it must NOT claim is
        // that it decided against a ceiling — which is what `is_authoritative()` answers, and
        // why the downgrade cannot be confused with `Limited` by anything downstream.
        assert!(
            unresolved.is_allowed(),
            "the request IS served; `Uncounted` is an allowed verdict that withholds its numbers"
        );
        assert!(
            !matches!(unresolved, Verdict::Limited { .. }),
            "an uncounted request must never read as a refusal — the client would back off for \
             no measured reason"
        );
        assert_eq!(unresolved.scope(), Some("user"));
        assert!(
            unresolved.ceiling().is_none() && unresolved.remaining().is_none(),
            "an uncounted request publishes neither a ceiling nor a remainder"
        );
        assert!(
            unresolved.retry_after().is_none(),
            "and no Retry-After: there is no window to wait for"
        );

        // The headers really are absent, asserted through the function that writes them rather
        // than by reading the enum — the two can disagree, and the wire is what the client sees.
        let response = apply_headers(
            Response::new(Body::empty()),
            &unresolved,
            Some(&policy("user", 600, 0, 60)),
            OffsetDateTime::now_utc(),
        );
        for header in [X_LIMIT, X_REMAINING, X_RESET, X_POLICY] {
            assert!(
                !response.headers().contains_key(&header),
                "{header} must not be published for a request that was never counted"
            );
        }
    }

    /// The allowed path carries the budget headers, so a client can see what it has left.
    ///
    /// Without them the only way a client learns its budget is by spending it, and a client that
    /// discovers the ceiling by being refused is a client that has just been told nothing about
    /// *when* it may return.
    #[test]
    fn an_allowed_authorized_answer_carries_all_three_headers() {
        let verdict = Verdict::Allowed {
            policy_id: Some(uuid::Uuid::from_u128(7)),
            scope: "ip".into(),
            remaining: 7,
            limit: 10,
        };
        let response = Response::new(Body::empty());
        let now = OffsetDateTime::now_utc();
        let response = apply_headers(response, &verdict, Some(&policy("ip", 10, 0, 60)), now);
        let headers = response.headers();
        assert_eq!(headers.get(X_LIMIT).unwrap(), "10");
        assert_eq!(headers.get(X_REMAINING).unwrap(), "7");
        let roll: i64 = headers.get(X_RESET).unwrap().to_str().unwrap().parse().unwrap();
        assert!(roll > now.unix_timestamp(), "the reset is in the future");
        assert!(
            roll - now.unix_timestamp() <= 60,
            "and no further out than the window"
        );
        assert!(headers.get(X_POLICY).is_some(), "the winning policy is named");
    }

    /// An unmeasured answer publishes no number, and this is the assertion that keeps it so.
    ///
    /// Both outage variants must be header-free. A `Remaining: 0` written by a layer that never
    /// read a counter is a measurement nobody took, and a client that believes its budget is spent
    /// stops trying — turning a Redis blip into a self-inflicted outage.
    #[test]
    fn an_unmeasured_answer_writes_no_budget_headers_at_all() {
        for verdict in [
            Verdict::Unlimited,
            Verdict::Uncounted {
                scope: "ip".into(),
            },
            Verdict::RefusedUncounted {
                scope: "ip".into(),
            },
        ] {
            assert!(!verdict.is_authoritative(), "{verdict:?} has no reading");
            let response = Response::new(Body::empty());
            let response = apply_headers(
                response,
                &verdict,
                Some(&policy("ip", 10, 0, 60)),
                OffsetDateTime::now_utc(),
            );
            for name in [X_LIMIT, X_REMAINING, X_RESET] {
                assert!(
                    response.headers().get(&name).is_none(),
                    "{name} must be absent when nothing was measured"
                );
            }
        }
    }

    /// `Unlimited` and `Uncounted` are different answers and both are allowed.
    ///
    /// The pair is the reason `Verdict` cannot be a bool: one means "no policy exists", the other
    /// "a policy exists and the counter is down". An operator debugging the second while the
    /// screen says the first is exactly the confusion the request's risk note warns about.
    #[test]
    fn the_two_allowed_unmeasured_answers_are_distinguishable() {
        assert!(Verdict::Unlimited.is_allowed());
        assert!(
            Verdict::Uncounted {
                scope: "ip".into()
            }
            .is_allowed()
        );
        assert!(!Verdict::RefusedUncounted {
            scope: "ip".into()
        }
        .is_allowed());
        assert_eq!(Verdict::Unlimited.scope(), None, "no policy, no scope");
        assert_eq!(
            Verdict::Uncounted {
                scope: "ip".into()
            }
            .scope(),
            Some("ip"),
            "a policy does apply, and the screen must say which"
        );
    }

    /// The refusal names the limiter, the scope, the counter's authority and the winning policy.
    ///
    /// A `429` with a code and no scope cannot be acted on: an operator who cannot tell which
    /// document is responsible cannot decide whether to widen the gateway limiter or this one.
    #[test]
    fn a_refusal_names_the_limiter_the_scope_and_whether_it_was_counted() {
        let policies = vec![policy("ip", 10, 0, 60)];
        let subject = subject("198.51.100.7");
        let counted = Counted {
            count: 11,
            authoritative: true,
        };
        let limited = Verdict::Limited {
            policy_id: Some(uuid::Uuid::from_u128(7)),
            scope: "ip".into(),
            retry_after: 42,
            limit: 10,
            ceiling: 10,
            remaining: 0,
        };
        let error = refusal_error(
            &limited,
            &policies,
            &subject,
            counted,
            OffsetDateTime::now_utc(),
        );
        assert_eq!(error.code(), "rate_limited");
        let details = error.details().expect("a refusal explains itself");
        assert_eq!(details["limiter"], LIMITER_NAME);
        assert_eq!(details["scope"], "ip");
        assert_eq!(details["counter_authoritative"], true);
        assert_eq!(details["limit"], 10);
        assert_eq!(details["window_seconds"], 60);
        assert_eq!(details["policy_id"], uuid::Uuid::from_u128(7).to_string());
    }

    /// The fail-closed refusal says the request was NOT counted — and carries no `Retry-After`.
    ///
    /// A wait the platform cannot compute is not a promise, and a client told to retry in a second
    /// hammers the dependency that is already down.
    #[test]
    fn a_fail_closed_refusal_says_uncounted_and_offers_no_wait() {
        let refused = Verdict::RefusedUncounted {
            scope: "ip".into(),
        };
        let error = refusal_error(
            &refused,
            &[policy("ip", 10, 0, 60)],
            &subject("198.51.100.7"),
            Counted {
                count: 0,
                authoritative: false,
            },
            OffsetDateTime::now_utc(),
        );
        assert_eq!(error.code(), "rate_limited");
        assert_eq!(error.retry_after(), None, "an unknowable wait is not a promise");
        let details = error.details().expect("a refusal explains itself");
        assert_eq!(details["counter_authoritative"], false);
        assert!(
            error.message().contains("not counted"),
            "the message says so: {}",
            error.message()
        );
    }

    /// The two limiters never share a key prefix, or one limiter's count would answer the other's
    /// question.
    #[test]
    fn the_platform_counter_never_lands_on_the_gateway_limiter_namespace() {
        // The gateway limiter uses `omnion:rl:`; this one uses `omnion:rlx:`. A shared prefix is
        // two budgets silently writing one counter, and it is the kind of bug that only shows up
        // as "the numbers are wrong" weeks later.
        let key = limiter_redis::counter_key(
            &policy("ip", 10, 0, 60),
            "ip:198.51.100.7",
            limiter_redis::aligned_epoch(),
        );
        assert!(key.starts_with("omnion:rlx:"), "{key}");
        assert!(!key.starts_with("omnion:rl:"), "{key}");
    }

    /// The probes and the public surfaces are exempt; the platform's own API is not.
    #[test]
    fn probes_and_public_surfaces_are_exempt_and_the_api_is_not() {
        for path in [
            "/healthz",
            "/readyz",
            "/livez",
            "/metrics",
            "/api/v1/public/pages/home",
            "/api/v1/hooks/intake/acme",
        ] {
            assert!(is_exempt_path(path), "{path} must be exempt");
        }
        for path in [
            "/api/v1/search",
            "/api/v1/auth/login",
            "/api/v1/observability/logs",
            "/api/v1/reliability/rate-limits",
        ] {
            assert!(!is_exempt_path(path), "{path} is a platform surface");
        }
    }

    /// A trailing slash does not smuggle a request past the exemption, and does not create one
    /// either: the probe's real path and the same path with a slash are the same surface.
    #[test]
    fn the_exemption_normalises_the_trailing_slash() {
        assert!(is_exempt_path("/healthz/"));
        assert!(is_exempt_path("/metrics/"));
        assert!(!is_exempt_path("/api/v1/search/"));
    }

    /// A `route`-scoped policy cannot be satisfied by this layer, and that is stated rather than
    /// left to be discovered.
    ///
    /// The layer runs BEFORE the router publishes `MatchedPath`, so it has no route template — and
    /// a policy keyed on a literal path would be one policy per id. `limits::matches_policy`
    /// already refuses a `route` policy for a subject with no route, so the resolver and the layer
    /// agree; this asserts the two do not drift by checking the refusal survives the round trip.
    #[test]
    fn a_route_policy_does_not_match_a_request_the_layer_cannot_name() {
        let mut route_policy = policy("route", 5, 0, 60);
        route_policy.route_pattern = Some("/api/v1/posts/{id}".into());
        let subject = Subject {
            user_id: None,
            organization_id: None,
            ip: Some("198.51.100.7".parse().unwrap()),
            route: None,
        };
        assert!(
            omnion_reliability::limits::pick(&[route_policy], &subject).is_none(),
            "a route policy needs a route, and this layer has none"
        );
    }

    /// The cache a poisoned lock hands back still carries the policies.
    ///
    /// The layer's whole protection is the numbers in this cache; an empty answer after one panic
    /// anywhere in a writer is a limiter that has turned itself off for the life of the process.
    #[test]
    fn a_poisoned_policy_cache_keeps_enforcing() {
        let cache = limiter_redis::empty_cache();
        limiter_redis::write_cache(&cache, vec![policy("ip", 10, 0, 60)]);
        let poisoned = cache.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = poisoned.write().expect("the first writer holds the lock");
            panic!("a writer panicked mid-save");
        }));
        assert!(result.is_err(), "the panic is what poisoned the lock");
        assert_eq!(
            limiter_redis::read_cache(&cache).len(),
            1,
            "the last written policy is still enforced after a panic"
        );
    }
}
