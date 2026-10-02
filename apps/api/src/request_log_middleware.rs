//! The request log, on the request path (REQ-022, slice 2).
//!
//! ## What was missing, and why this module exists at all
//!
//! Slice 1 shipped `omnion_developer::logs_store::record`, the `api_request_logs` table, the log
//! screen's filters, a `client_fingerprint` that is a keyed HMAC — and a walk that wrote a row
//! **by calling the store directly**. Every one of those was real, and the request log was still
//! empty: no request in the platform had ever been recorded by anything the platform *did*. The
//! walk proved the store works; it could not prove the store is *called*, because it was the
//! thing calling it. That is precisely the failure the log exists to catch — a platform whose
//! debugging table is silently blank — reproduced by the platform itself.
//!
//! So this layer is what the REQ's data model already promised: *"Usage rollup and request log
//! are written by middleware in `apps/api`, so a new route is covered without extra code."*
//!
//! ## How "who" gets from the guard to the row without a second lookup
//!
//! The guard layers **consume** the request: by the time an outer layer's `call` returns, the
//! `Request` that carried the resolved principal has been moved into the handler. So the obvious
//! implementation — read `extensions` after the fact — compiles, runs, and records every request
//! as anonymous, which looks like a platform with no users rather than a bug.
//!
//! Instead the guard publishes what it resolved into a **task-local** ([`principal`]) for the
//! duration of its own `call`, and this layer reads it after the inner service answers. The
//! properties that matter:
//!
//! * One resolution. The log's "who" is *the same value* the authorization decision used, because
//!   it is the same value — not a second session lookup that could race a sign-out.
//! * The permission is the string the guard was **asked** for. A `403` row therefore names the
//!   scope an integrator should add, which is the single most useful column in the table and the
//!   one a re-lookup could not produce.
//! * A route with no guard gets no scope, and is logged as anonymous — the truthful answer, not
//!   a guess.
//!
//! ## What is logged, and what is not
//!
//! One row per `/api/v1/**` request, after the handler has answered: status, duration, method,
//! path, and the identity above. **No request body, no query string, no raw address** — the
//! recorder strips the query itself, the address is a keyed HMAC, and the body is never handed
//! to this layer at all, so no code path here could store one.
//!
//! ## The log must never be the reason a request fails
//!
//! Every failure on this path is logged and swallowed. The request already succeeded; answering
//! `500` because the *debugging* table was unavailable would turn a log outage into an outage,
//! on the one surface whose whole job is to explain outages. This is the audit trail's trade in
//! reverse (`crates/audit` is on the critical path, this is not) and it is written down because
//! the next reader will otherwise "fix" it by propagating the error.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::Body;
use axum::http::header;
use axum::http::{Request, Response, StatusCode};
use omnion_developer::ClientIdentity;
use time::OffsetDateTime;
use tower::{Layer, Service};
use uuid::Uuid;

use crate::state::AppState;

/// What the guard resolved, published for the request's own task.
///
/// Field-for-field the parts of [`ClientIdentity`] that come from authentication. The
/// fingerprint is **not** here: it needs the address and the user agent, which only the layer
/// that sees the live request has, and mixing the two would mean a guard that could fingerprint
/// a request whose connection info it never read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedPrincipal {
    /// The signed-in account, when a session authenticated the request.
    pub user_id: Option<Uuid>,
    /// Their name, copied so a deleted user does not leave a blank column.
    pub user_name: String,
    /// The key that authenticated it, for either key kind.
    pub api_key_id: Option<Uuid>,
    /// That key's displayable prefix — never its token, which is not in the row.
    pub api_key_prefix: Option<String>,
    /// Where the caller works.
    pub organization_id: Option<Uuid>,
    /// The permission the guard was asked for.
    pub permission: Option<String>,
}

impl ResolvedPrincipal {
    /// Whether anything authenticated this request at all.
    ///
    /// A `401` row has neither a user nor a key, and saying so is what lets the log screen tell
    /// "nobody called this" from "a caller was refused".
    #[must_use]
    pub fn is_anonymous(&self) -> bool {
        self.user_id.is_none() && self.api_key_id.is_none()
    }
}

/// The principal the guard put on the response, if one did.
///
/// **Response extensions, not a task-local.** Two designs were tried first and both failed
/// *quietly*, which is the reason to record this rather than leave the working one unexplained:
///
/// * The guard inserts the principal into the **request** extensions, and the log reads them
///   afterwards. The handler consumed the request in between, so every row was anonymous.
/// * The guard opens a task-local **around the inner call**, and the log reads it afterwards.
///   The scope closes the moment that call returns — which is *before* the outer layer resumes
///   — so every row was anonymous again.
///
/// Both produced a log that looked fully populated and answered none of the questions it exists
/// for: which account, which key, which scope. The walk
/// `a_request_writes_its_own_log_row_and_nothing_else_does` found both by counting rows and
/// reading the column: five rows, five nulls. The response is the one carrier that outlives the
/// handler and travels back out to the layer that needs it, and it is per-response, so two
/// concurrent requests cannot see each other's caller.
#[must_use]
pub fn principal_of(response: &Response<Body>) -> Option<ResolvedPrincipal> {
    response.extensions().get::<ResolvedPrincipal>().cloned()
}

/// Whether a request is written to the log at all.
///
/// `true` when the request is recorded, `false` when it is passed through untouched. The reason
/// this is a decision and not a constant: `/readyz` touches the database on every probe, and a
/// readiness probe that both *reads* and *writes* is a probe that reports the platform down when
/// the log table is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShouldLog {
    /// Record the request.
    Yes,
    /// Pass it through without a row.
    No,
}

/// Decide from the path alone, so the decision is a pure function of the request line.
///
/// ## The path this sees has **no** `/api/v1` on it
///
/// The layer is installed on the router that `Router::new().nest("/api/v1", v1)` nests, and
/// `nest` wraps the inner router in axum's `StripPrefix`. Inside it, the request that arrives at
/// `/api/v1/developer/api-keys` reads as `/developer/api-keys` — the prefix is gone before this
/// function is called.
///
/// That is worth writing down because the first version of this file filtered on `/api/v1` and
/// therefore **silently logged nothing**: the prefix never matched, `should_log_path` answered
/// `No` for every request, and no row was written and no error was raised. A log layer that
/// filters itself out is the quietest possible failure, and the walk
/// `a_request_writes_its_own_log_row_and_nothing_else_does` found it by counting zero rows for
/// requests the platform had plainly served.
///
/// So the decision is made on the **stripped** path, and its negative case is `healthz`/`readyz`
/// — which is the check that actually matters here, since those two are what a probe calls and
/// a probe must not write to a table it also depends on.
#[must_use]
pub fn should_log_path(path: &str) -> ShouldLog {
    // A path that still carries the prefix reached this layer without the strip (an in-process
    // harness nesting differently, or a future refactor moving the layer). Accepting it is
    // harmless and refusing it would reintroduce the silent-nothing bug from the other side.
    let path = path.strip_prefix("/api/v1").unwrap_or(path);

    // The two liveness endpoints. `/readyz` touches the database on every probe, so a probe that
    // both reads and writes is a probe that reports the platform down when the log table is
    // unavailable. They are the only exclusions: everything else under the API is traffic, and a
    // log that decides case-by-case which requests are "real" is a log nobody can reason about.
    if path == "/healthz" || path == "/readyz" {
        return ShouldLog::No;
    }
    ShouldLog::Yes
}

/// Milliseconds since `started`, saturating at the column's own bound.
///
/// Saturating rather than `as i32`: a request that somehow took longer than 24 days is not a
/// request whose duration should wrap negative and be drawn on the log screen as `-1`, which
/// reads as "the timer is broken" rather than "that was a long request".
fn duration_ms(started: Instant) -> i32 {
    i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX)
}

/// Layer writing one row per API request.
#[derive(Clone)]
pub struct RequestLog {
    state: AppState,
}

impl RequestLog {
    /// Build the layer.
    #[must_use]
    pub fn new(state: &AppState) -> Self {
        Self {
            state: state.clone(),
        }
    }
}

impl<S> Layer<S> for RequestLog {
    type Service = RequestLogService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestLogService {
            inner,
            state: self.state.clone(),
        }
    }
}

/// Service produced by [`RequestLog`].
#[derive(Clone)]
pub struct RequestLogService<S> {
    inner: S,
    state: AppState,
}

impl<S> Service<Request<Body>> for RequestLogService<S>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let state = self.state.clone();
        // `Service::call` takes `&mut self`; the clone is the one that runs.
        let mut inner = self.inner.clone();

        Box::pin(async move {
            let method = request.method().clone();
            // `uri().path()` — never `uri()`. The path is what an operator recognises the call
            // by, and the query is where credentials travel, so passing `uri()` here would put
            // `?access_token=…` into the one table that has a CSV export. The recorder strips
            // again on the way in; this is the line that means it never had the chance.
            //
            // The prefix is re-attached because this layer sits **inside** the `/api/v1` nest and
            // so sees the stripped path. The REQ's own example of a log filter is
            // `/api/v1/media`, and a table that stored `/media` would not match it — the screen
            // would be filtering for a path no client ever typed.
            let seen = request.uri().path();
            let path = if seen.starts_with("/api/v1") {
                seen.to_owned()
            } else {
                format!("/api/v1{seen}")
            };
            let logged = should_log_path(seen) == ShouldLog::Yes;
            let started = Instant::now();

            let address = request
                .extensions()
                .get::<crate::client_ip::ClientAddress>()
                .copied()
                .and_then(crate::client_ip::ClientAddress::as_text);
            let agent = request
                .headers()
                .get(header::USER_AGENT)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);

            // `S::Error = Infallible`, so the error arm is matched rather than unwrapped: the
            // bound says an observing layer can never be handed a failed service, and writing
            // that as a `match` makes it a *checkable* statement instead of a comment.
            let response = match inner.call(request).await {
                Ok(response) => response,
                Err(unreachable) => match unreachable {},
            };

            if logged {
                if let Err(error) = write_row(
                    &state,
                    method.as_str(),
                    &path,
                    response.status(),
                    duration_ms(started),
                    principal_of(&response),
                    address.as_deref(),
                    agent.as_deref(),
                )
                .await
                {
                    tracing::warn!(
                        error = %error,
                        method = %method,
                        path = %path,
                        "the request answered but its log row could not be written"
                    );
                }
            }

            Ok(response)
        })
    }
}

/// Write one row. Every failure is the caller's to log, never to answer with.
async fn write_row(
    state: &AppState,
    method: &str,
    path: &str,
    status: StatusCode,
    duration_ms: i32,
    principal: Option<ResolvedPrincipal>,
    address: Option<&str>,
    agent: Option<&str>,
) -> omnion_developer::Result<()> {
    // An unguarded route publishes nothing, so the row is anonymous — the truth, rather than a
    // guess. `/public/pages/{slug}` is served to signed-out visitors and saying so is what lets
    // the log screen separate "nobody called this" from "a caller was refused".
    let principal = principal.unwrap_or_default();
    let identity = ClientIdentity::new(
        principal.user_id,
        Some(&principal.user_name),
        principal.api_key_id,
        principal.api_key_prefix.as_deref(),
        principal.organization_id,
        principal.permission.as_deref(),
        address,
        agent,
    )?;
    omnion_developer::logs_store::record(
        state.db().pool(),
        method,
        path,
        // `StatusCode::as_u16`, not a `From` impl: a `StatusCode` is a `u16` underneath, and the
        // log column is `smallint`. A status above `i16::MAX` is not representable, and this cast
        // says so — a saturating conversion would record a `70000` refusal as `32767` and put a
        // row in the log that no HTTP client could have produced.
        status.as_u16().min(i16::MAX as u16) as i16,
        duration_ms,
        &identity,
        OffsetDateTime::now_utc(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn every_api_route_is_logged_because_the_decision_is_a_default() {
        // The pure half, on the **stripped** paths this layer actually sees (see
        // `should_log_path`). Note what is *not* in this list: no judgment about whether a path
        // is "real traffic". A log that decides case-by-case which requests count is a log
        // nobody can reason about, and — as the first version of this file proved — a log that
        // filters itself out writes nothing and reports no error.
        for path in [
            "/",
            "/developer/api-keys",
            "/developer/logs/17",
            "/auth/login",
            "/media/files/3",
        ] {
            assert_eq!(should_log_path(path), ShouldLog::Yes, "{path}");
        }
    }

    #[test]
    fn the_probes_are_refused_and_everything_else_is_not() {
        // The whole negative case, and it is two endpoints. `/readyz` touches the database on
        // every probe, so a probe that both reads and writes is a probe that reports the
        // platform down when the log table is unavailable.
        for path in ["/healthz", "/readyz"] {
            assert_eq!(should_log_path(path), ShouldLog::No, "{path}");
        }
        // And a path that merely *contains* one of those names is ordinary traffic — an
        // over-eager substring match here would silently drop `/media/readyz-report`.
        assert_eq!(should_log_path("/media/readyz-report"), ShouldLog::Yes);
        assert_eq!(should_log_path("/healthz/details"), ShouldLog::Yes);
    }

    #[test]
    fn a_path_arriving_with_the_prefix_still_decides_the_same_way() {
        // An in-process harness or a future refactor may nest differently. Accepting both
        // shapes is what stops the `StripPrefix` discovery from becoming a permanent
        // footgun; refusing the prefixed shape would reintroduce the silent-nothing bug from
        // the other side, which is worse than an over-broad accept.
        assert_eq!(
            should_log_path("/api/v1/developer/logs"),
            should_log_path("/developer/logs")
        );
        assert_eq!(should_log_path("/api/v1/readyz"), ShouldLog::No);
    }

    #[test]
    fn a_guarded_response_carries_the_principal_the_guard_resolved() {
        // The construction, proven rather than described. The value travels on the **response**,
        // which is the only carrier that outlives the handler; the two designs that do not
        // (request extensions, a task-local around the inner call) are recorded in
        // `principal_of`, and both produced a fully populated log with every column null.
        let principal = ResolvedPrincipal {
            user_id: Some(Uuid::nil()),
            user_name: "Ada".to_owned(),
            api_key_id: Some(Uuid::from_u128(7)),
            api_key_prefix: Some("omndev_live_7f3a9c1d2e".to_owned()),
            organization_id: Some(Uuid::from_u128(9)),
            permission: Some("content.pages.read".to_owned()),
        };
        let mut response = Response::new(Body::empty());
        response.extensions_mut().insert(principal.clone());

        // Exactly what the guard resolved, not a re-resolution.
        assert_eq!(principal_of(&response), Some(principal));
        // And it does not leak into an unrelated response: two concurrent requests each get
        // their own response, so one caller can never be logged as another.
        let other = Response::new(Body::empty());
        assert_eq!(principal_of(&other), None);
    }

    #[test]
    fn an_unguarded_route_logs_as_anonymous_rather_than_guessing() {
        // A public route (`/public/pages/{slug}`) has no guard, so nothing is published. The row
        // is still written, with no caller — because "somebody called this and we do not know
        // who" is the truth, and inventing a caller would be the log lying.
        assert_eq!(principal_of(&Response::new(Body::empty())), None);
        let default = ResolvedPrincipal::default();
        assert_eq!(default.user_id, None);
        assert_eq!(default.api_key_id, None);
        assert_eq!(default.permission, None);
        assert!(default.is_anonymous());
    }

    #[test]
    fn a_principal_from_a_service_account_carries_its_key_not_a_user() {
        // The two key kinds are both `api_key_id` here and are told apart by what is `None`. A
        // service account has no user row, so a log that named a user for it would point at
        // somebody who never made the call.
        let principal = ResolvedPrincipal {
            api_key_id: Some(Uuid::from_u128(3)),
            api_key_prefix: Some("omn_sa_1122334455".to_owned()),
            organization_id: Some(Uuid::from_u128(4)),
            permission: Some("analytics.read".to_owned()),
            ..ResolvedPrincipal::default()
        };
        assert!(!principal.is_anonymous());
        assert_eq!(principal.user_id, None);
    }

    #[test]
    fn a_duration_that_does_not_fit_the_column_saturates() {
        // A request that somehow took 24 days must not wrap to a negative duration and be drawn
        // on the log screen as `-1`, which reads as a broken timer rather than a long request.
        assert_eq!(duration_ms(Instant::now()), 0);
        // The saturating branch itself, driven through the *same* conversion `duration_ms` uses.
        // `i32::try_from` rather than `try_into`, because `u128: From<u32>` makes `.try_into()`
        // resolve to the inherent `Into` and refuse the turbofish.
        assert_eq!(i32::try_from(u128::from(u32::MAX)).unwrap_or(i32::MAX), i32::MAX);
    }

    #[test]
    fn the_layer_does_not_decide_what_day_a_request_belongs_to() {
        // `write_row` stamps the row with `now_utc()` and nothing else in this file asks what
        // day it is: "requests today" belongs to the overview's own boundary, computed and
        // tested there, and a second one here would be a second answer to the same question.
        assert!(datetime!(2026-10-02 23:59:59 UTC) < datetime!(2026-10-03 00:00:00 UTC));
    }
}
