//! `Idempotency-Key` on the request path (REQ-127, slice 2).
//!
//! [`crate::reliability_middleware`] answers "may this request spend a budget" and
//! [`omnion_reliability::idempotency`] answers "what should a replayed key do". Neither of them
//! looks at an HTTP request. This module is the piece that joins them, and the piece that did not
//! exist for the whole of slice 1 — a key store with no caller, which is the "documented but
//! unreachable" shape this request's siblings have produced four times already.
//!
//! ## Where in the chain the claim happens, and why that is the whole design
//!
//! ```text
//! request_log  →  rate_limit  →  platform_limit  →  csrf  →  headers
//!      →  guards::require("…")        ← a 403 is answered HERE
//!      →  idempotency::require(state) ← the key is claimed HERE
//!      →  handler
//! ```
//!
//! The claim runs **inside** the permission guard. That placement is the acceptance criterion
//! "a keyed request that is refused by a permission check never consumes an idempotency key",
//! and it is proved by the layer's position rather than by a code path: a request the guard
//! refused never reaches the layer that inserts a row, so there is nothing to roll back and
//! nothing to undo.
//!
//! **A rollback would be a second bug, not a fix.** The tempting repair — release the key when
//! the handler answers `4xx`/`5xx` — would let a refused request delete the *winner's*
//! `in_progress` row: the winner is a different request, running concurrently, and its row is
//! the only thing standing between it and a second execution. So nothing is ever rolled back
//! here. [`omnion_reliability::idem_store::release_stale`] exists for the opposite case — an
//! attempt that **crashed** — and the operator's release action exists for the one case neither
//! of them covers.
//!
//! ## What a replay answers
//!
//! | On the key | The answer |
//! |---|---|
//! | nobody's | run the handler, store the response, answer it |
//! | same fingerprint, `completed` | the stored status, body and headers, with `Idempotent-Replay: true` |
//! | same fingerprint, `in_progress` | `409 idempotency_in_progress` + `Retry-After` |
//! | different fingerprint | `409 idempotency_conflict` + `reliability.idempotency.conflict` |
//!
//! Every keyed answer — first or replay — carries `Idempotency-Original-Request-Id`, the request
//! id of the **first** execution. A replay that reported its own id would send an operator to
//! the log explorer and find nothing there, because the replay wrote no lines: it answered from
//! the store.
//!
//! ## What is stored, and what is deliberately not
//!
//! The stored header subset is `location`, `etag` and the original request id. Three headers are
//! **excluded on purpose**: `set-cookie` (a replay must never re-issue a session — that is a
//! second authentication, written by a request that already finished), `www-authenticate` (it
//! describes the *caller's* credentials, and the caller is the same one) and every
//! `X-RateLimit-*` (those describe the budget of the request being answered, and the layer that
//! writes them sits **outside** this one, so a replay gets fresh, truthful numbers whatever is
//! in the store).
//!
//! ## A `5xx` is not stored, and that is the interesting decision
//!
//! A stored `500` is a promise: for 24 hours, every replay of that key returns the same failure
//! even though the write may well succeed on the second attempt. A truncated body would be worse
//! still, and an oversized body with nowhere to put it is the same case. So a response the layer
//! cannot replay is recorded as [`omnion_reliability::vocabulary::IDEMPOTENCY_FAILED`]
//! ([`omnion_reliability::idem_store::abandon`]), which is the one state that says "run it again" —
//! a visible retry rather than a silent lie. The alternative answers are named in `abandon`'s
//! documentation because each was the obvious first draft.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes, to_bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use omnion_events::{NewEvent, bus};
use omnion_reliability::idempotency::{self, Replay, StoredResponse};
use omnion_reliability::idem_store;
use omnion_reliability::vocabulary::events::IDEMPOTENCY_CONFLICT;
use serde_json::json;
use time::OffsetDateTime;
use tower::{Layer, Service};
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::guards::MachinePrincipal;
use crate::state::AppState;

/// The header a caller presents the key in.
pub const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";

/// Written on every response to a keyed request, first or replay, naming the FIRST execution.
pub const IDEMPOTENCY_ORIGINAL_HEADER: &str = "idempotency-original-request-id";

/// Written **only** on a replay. A client that sees it knows the handler did not run again, which
/// is the entire claim — and it is a header rather than only a body field because a body may be a
/// file reference with no body to look at.
pub const IDEMPOTENCY_REPLAY_HEADER: &str = "idempotent-replay";

const ORIGINAL_HEADER: HeaderName = HeaderName::from_static(IDEMPOTENCY_ORIGINAL_HEADER);
const REPLAY_HEADER: HeaderName = HeaderName::from_static(IDEMPOTENCY_REPLAY_HEADER);

/// The largest response body this layer will buffer in order to store it.
///
/// Above [`omnion_reliability::idempotency::INLINE_BODY_CAP`] on purpose: a body that fits the
/// cap is stored, a body between the cap and this limit is **returned to its caller intact** and
/// the key is marked `failed` (a retry re-runs, visibly), and only a body past this limit cannot
/// be re-assembled at all — at which point the answer is a `500` that says so, because the
/// alternative is answering a mutating request with an empty body and a `200`.
const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;

/// The methods a key can protect. A `GET` is absent because replaying a read is a cache and the
/// platform does not turn every read into a stored row.
fn is_mutating(method: &Method) -> bool {
    matches!(*method, Method::POST | Method::PUT | Method::PATCH | Method::DELETE)
}

/// Layer that claims, replays and completes an `Idempotency-Key`.
#[derive(Clone)]
pub struct RequireIdempotency {
    state: AppState,
}

/// Build the layer. It is applied per route, beside the route's permission guard.
#[must_use]
pub fn require(state: &AppState) -> RequireIdempotency {
    RequireIdempotency { state: state.clone() }
}

/// Service produced by [`RequireIdempotency`].
#[derive(Clone)]
pub struct RequireIdempotencyService<S> {
    inner: S,
    state: AppState,
}

impl<S> Layer<S> for RequireIdempotency {
    type Service = RequireIdempotencyService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequireIdempotencyService {
            inner,
            state: self.state.clone(),
        }
    }
}

impl<S> Service<Request<Body>> for RequireIdempotencyService<S>
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
        let state = self.state.clone();
        let mut inner = self.inner.clone();
        let (parts, body) = request.into_parts();

        // The cheap rejections happen BEFORE the body is read, so a request this layer does not
        // serve costs it nothing: no buffering, no database, no `to_bytes`.
        if !is_mutating(&parts.method) || presented_key(&parts.headers).is_none() {
            return Box::pin(async move {
                Ok(inner.call(Request::from_parts(parts, body)).await?)
            });
        }
        // The subject is read from the extensions the GUARD wrote. If the guard is not outside
        // this layer the request has no subject, and a key scoped to nothing is a key any caller
        // can replay — so the request is passed through unclaimed and the mistake is logged,
        // rather than silently claimed under an empty subject.
        let Some(subject) = subject_of(&parts.extensions) else {
            tracing::warn!(
                path = %parts.uri.path(),
                "a keyed request reached the idempotency layer with no session in its extensions; \
                 the key is not claimed"
            );
            return Box::pin(async move {
                Ok(inner.call(Request::from_parts(parts, body)).await?)
            });
        };

        // The key is a `String`, not a borrow of the header map. The returned future must be
        // `'static` (it is boxed into `Pin<Box<dyn Future + Send>>` and the service is cloned per
        // request), and a `&str` taken out of `parts` would keep the whole `Parts` — which is
        // moved into the inner call — borrowed for the life of the request. A header is at most
        // 255 characters, so the copy is smaller than the header it came from.
        let key = presented_key(&parts.headers)
            .expect("checked above")
            .to_owned();
        if let Err(error) = idempotency::KeyRecord::validate_key(&key) {
            let error = ApiError::bad_request("invalid_idempotency_key", error.to_string());
            return Box::pin(async move { Ok(error.into_response()) });
        }

        let method = parts.method.clone();
        // The scope is built from the MATCHED ROUTE TEMPLATE where the router has published one,
        // and from the raw path only when it has not. A template is what makes the key portable:
        // the layer runs inside a nested `/api/v1` router, so `uri.path()` is the fragment
        // `/automations`, and a key scoped to THAT is a different key from the one an operator
        // reading the panel sees for the same endpoint. The first version did exactly this — the
        // walk claimed a row under `POST /api/v1/automations`, the layer claimed under
        // `POST /automations`, the unique index treated them as two keys, and the keyed request
        // ran as if unkeyed while the panel showed a stuck one nobody could clear.
        //
        // When no template is published (the layer is installed on a route the router has not
        // matched, or by a harness that drives the service directly) the raw path is used, and
        // the key is still consistent for as long as the caller keeps sending the same path —
        // which is the property a fallback can honestly promise.
        let path = parts
            .extensions
            .get::<axum::extract::MatchedPath>()
            .map(|matched| matched.as_str().to_owned())
            .unwrap_or_else(|| parts.uri.path().to_owned());
        let scope = format!("{} {path}", method.as_str().to_ascii_uppercase());
        let request_id = omnion_telemetry::LogContext::current()
            .request_id
            .unwrap_or_else(Uuid::new_v4);

        Box::pin(async move {
            let Ok(bytes) = to_bytes(body, MAX_CAPTURE_BYTES).await else {
                // The request body was larger than the platform is willing to fingerprint. The
                // store keeps a hash, not the body, so the *limit* is ours and it is honest: the
                // answer says the key was refused, so a client that retries without a key gets a
                // normal request rather than a `409` against a key nobody claimed.
                return Ok(ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "payload_too_large",
                    format!("a keyed request body must be at most {MAX_CAPTURE_BYTES} bytes"),
                )
                .into_response());
            };
            let body_text = String::from_utf8_lossy(&bytes).into_owned();
            let fingerprint = idempotency::fingerprint(method.as_str(), &path, &body_text);
            let now = OffsetDateTime::now_utc();
            let pool = state.db().pool();

            // `find` then `claim` would be the read-then-write shape the store's documentation
            // names as the one that loses the race. The claim goes first and its OWN answer is
            // the decision: `rows_affected` decided the race inside one statement.
            match idem_store::claim(
                pool,
                &scope,
                &subject,
                &key,
                method.as_str(),
                &path,
                &fingerprint,
                now,
            )
            .await
            {
                Err(error) => {
                    // The store is unreachable. The write has NOT happened, so the honest answer
                    // is a refusal the client can retry — and not a run: running a keyed write
                    // nobody can later replay is exactly the duplicate this feature exists to
                    // prevent.
                    return Ok(ApiError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "idempotency_unavailable",
                        "the idempotency store could not be reached; the request was not executed",
                    )
                    .with_retry_after(1)
                    .with_details(json!({ "error": error.to_string() }))
                    .into_response());
                }
                Ok(idem_store::Claim::Existing(existing)) => {
                    return Ok(replay_answer(&state, &existing, &fingerprint, now).await);
                }
                Ok(idem_store::Claim::Claimed) => {}
            }

            // The claim is ours. Run the handler, then record what it answered.
            let response = inner
                .call(Request::from_parts(parts, Bytes::from(body_text).into()))
                .await?;
            Ok(record_and_respond(&state, response, &scope, &subject, &key, request_id).await)
        })
    }
}

/// The key a caller presented, if any.
///
/// Header names are case-insensitive and `axum::http::HeaderMap::get` already folds, so this is a
/// `get` and not a scan: a client that sends `Idempotency-KEY` gets the same answer as one that
/// sends it in the canonical case.
fn presented_key(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(IDEMPOTENCY_KEY_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|key| !key.is_empty())
}

/// The scope a key belongs to: the user, or the machine key for a service account.
fn subject_of(extensions: &axum::http::Extensions) -> Option<String> {
    if let Some(session) = extensions.get::<CurrentSession>() {
        return Some(session.user.id.to_string());
    }
    if let Some(machine) = extensions.get::<MachinePrincipal>() {
        return Some(format!("key:{}", machine.key_id));
    }
    None
}

/// The answer to a request whose key somebody else already holds.
async fn replay_answer(
    state: &AppState,
    existing: &omnion_reliability::idempotency::KeyRecord,
    fingerprint: &str,
    now: OffsetDateTime,
) -> Response<Body> {
    match idempotency::decide(Some(existing), fingerprint, now) {
        Replay::ReturnStored { status, body, body_ref } => {
            // A replay of a body that was never stored cannot be served, and serving an empty
            // `200` would be the same lie as truncating it. `body_ref` is the honest answer: the
            // caller is told where the response went.
            if body.is_none() && body_ref.is_none() {
                return ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "idempotency_body_unavailable",
                    "this key stored no replayable response body",
                )
                .into_response();
            }
            if let Some(reference) = body_ref {
                return ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "idempotency_body_reference",
                    format!("the stored response for this key is at {reference}"),
                )
                .into_response();
            }

            // Counting is best-effort BY DESIGN: the replay is already correct, and a counter
            // that fails to move must not turn that into an error.
            if let Err(error) =
                idem_store::count_replay(state.db().pool(), &existing.scope, &existing.subject_id, &existing.key)
                    .await
            {
                tracing::warn!(error = %error, "a replay was answered but its counter did not move");
            }
            serve_stored(state, existing, status, body.unwrap_or_default(), true).await
        }
        Replay::Conflict => {
            emit_conflict(state, existing, fingerprint).await;
            ApiError::new(
                StatusCode::CONFLICT,
                "idempotency_conflict",
                format!(
                    "idempotency key '{}' was first used with a different request body",
                    existing.key
                ),
            )
            .with_details(json!({
                "key": existing.key,
                "scope": existing.scope,
                "method": existing.method,
                "path": existing.path,
                "state": existing.state,
            }))
            .into_response()
        }
        Replay::InProgress { retry_after } => ApiError::new(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            format!(
                "the first attempt for idempotency key '{}' is still running",
                existing.key
            ),
        )
        .with_retry_after(retry_after)
        .with_details(json!({
            "key": existing.key,
            "scope": existing.scope,
            "retry_after_seconds": retry_after,
        }))
        .into_response(),
        // `claim` already took an expired row over, so a record that reaches here as `Expired`
        // was expired a microsecond after the claim read it. Re-deciding would be the same
        // decision, so the answer is the same `Proceed` the claim already bought — and a caller
        // is better served by a `409` it can retry than by a claim nobody will honour.
        Replay::Expired | Replay::Proceed => ApiError::new(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            format!(
                "idempotency key '{}' is being claimed by another request; retry",
                existing.key
            ),
        )
        .with_retry_after(idempotency::IN_PROGRESS_RETRY_AFTER)
        .into_response(),
    }
}

/// Rebuild a stored response, with the replay marker and the original request id.
///
/// The original request id is **read back from the stored headers**, not recomputed and not taken
/// from the record. It is the id of the first execution, the only place it exists, and a replay
/// that reported its own id would send an operator into the log explorer to a request that wrote
/// no lines — it answered from the store. The first version of this function had no database
/// argument and so had nowhere to read the id from, which is why it silently omitted the header;
/// the walk caught it by comparing the replay's header against the first execution's.
async fn serve_stored(
    state: &AppState,
    existing: &omnion_reliability::idempotency::KeyRecord,
    status: i16,
    body: String,
    replay: bool,
) -> Response<Body> {
    let mut response = serve_stored_marked(existing, status, body, replay);
    let original = stored_original_request_id(
        state.db().pool(),
        &existing.scope,
        &existing.subject_id,
        &existing.key,
    )
    .await;
    if let Some(id) = original
        && let Ok(value) = HeaderValue::from_str(&id)
    {
        response.headers_mut().insert(ORIGINAL_HEADER, value);
    }
    response
}

/// The part of [`serve_stored`] that touches no database.
///
/// Split out so the header map a replay publishes is unit-testable without a platform, and so the
/// one query the replay path makes is visible as a single named call rather than as a line buried
/// in a builder chain.
fn serve_stored_marked(
    existing: &omnion_reliability::idempotency::KeyRecord,
    status: i16,
    body: String,
    replay: bool,
) -> Response<Body> {
    let code = StatusCode::from_u16(u16::try_from(status).unwrap_or(200)).unwrap_or(StatusCode::OK);
    // `Response::builder()` is fallible and its only failure mode is a header this function
    // writes itself — two names and two ASCII literals. The status comes from the STORE, so it
    // is validated above and a stored value that is not a status is answered as `200` rather
    // than panicked on: a corrupt row must not be able to take the process down.
    let mut response = match Response::builder().status(code).body(Body::from(body)) {
        Ok(response) => response,
        Err(_) => Response::builder()
            .status(StatusCode::OK)
            .body(Body::empty())
            .expect("a status and an empty body always build"),
    };
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(existing.scope.as_str()) {
        // The scope is echoed so a client integrating two endpoints can tell WHICH key family
        // answered without keeping its own map. It is a template, never a query string.
        headers.insert(HeaderName::from_static("idempotency-scope"), value);
    }
    if replay {
        headers.insert(REPLAY_HEADER, HeaderValue::from_static("true"));
    }
    response
}

/// The original request id, read from the response headers stored with the first execution.
///
/// `None` — rather than the replay's own id — is the honest answer for a row written before the
/// header was stored, and a replay that omitted the header is one a client can detect, while a
/// replay that carried a WRONG id is one it cannot.
async fn stored_original_request_id(
    pool: &sqlx::PgPool,
    scope: &str,
    subject_id: &str,
    key: &str,
) -> Option<String> {
    let headers: Option<serde_json::Value> = sqlx::query_scalar(
        "select response_headers from idempotency_keys \
          where scope = $1 and subject_id = $2 and key = $3",
    )
    .bind(scope)
    .bind(subject_id)
    .bind(key)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    // Two levels of absence, named rather than chained: the row may not exist, and the stored
    // headers may be empty. `.ok().flatten()?` looks like it says both and says only the first —
    // the `?` on the `Option` that survives it is the one that returns, so what the tail sees is
    // the row's `Value` and not the column's. One compile error, and the comment is the receipt.
    let headers = headers?;

    headers
        .get(IDEMPOTENCY_ORIGINAL_HEADER)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .filter(|id| !id.is_empty())
}

/// Store the handler's response and answer the caller with it.
///
/// The body is buffered because a stored response is the whole point: a body read in a stream is
/// gone by the time the row could be written. The read is bounded (see [`MAX_CAPTURE_BYTES`]) and
/// a response that cannot be stored is recorded as `failed` rather than dropped.
async fn record_and_respond(
    state: &AppState,
    response: Response<Body>,
    scope: &str,
    subject: &str,
    key: &str,
    request_id: Uuid,
) -> Response<Body> {
    let (parts, body) = response.into_parts();
    let status = parts.status;
    let stored_headers = subset_of(&parts.headers, request_id);

    let (mut response, body) = match to_bytes(body, MAX_CAPTURE_BYTES).await {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            // `StatusCode`'s range is `100..=999`, so this is lossless — and a status that could
            // not fit would be a status the platform has no HTTP meaning for, which is exactly
            // what a stored `i16` is for.
            // `StatusCode`'s valid range is `100..=999`, so the narrowing is lossless and there
            // is no value a real response can carry that this cast would mangle. `i16::from` does
            // not accept a `u16` (the `From<u16>` impl is not a thing in the std), so the cast is
            // explicit — and it is on the HTTP value rather than on a stored one, so a corrupt row
            // cannot reach it.
            let sealed =
                StoredResponse::seal(status.as_u16() as i16, text, request_id, None);
            if sealed.oversized_without_reference() {
                // Past the inline cap with nowhere to put it. The write happened, the caller
                // still gets the whole body, and the key is left in a state that says "run it
                // again" instead of a state that replays half an answer for a day.
                abandon(state, scope, subject, key, "response body is larger than the inline cap")
                    .await;
            } else {
                let mut sealed = sealed;
                sealed.headers = stored_headers;
                match idem_store::complete(state.db().pool(), scope, subject, key, &sealed, OffsetDateTime::now_utc())
                    .await
                {
                    Ok(()) => {}
                    Err(error) => {
                        // The row may still be `in_progress` if the UPDATE matched nothing, which
                        // is the case the store's own `NotFound` names. Marking it `failed` here
                        // is what keeps one write from stranding a key for 24 hours.
                        tracing::warn!(error = %error, scope, key, "a keyed response could not be stored");
                        abandon(state, scope, subject, key, "the stored response could not be written").await;
                    }
                }
            }
            // The caller's response is rebuilt from the SAME `parts` rather than patched, so
            // the status, version, extensions and every header the handler set survive
            // untouched. Only the body is swapped for the buffered bytes.
            (Response::from_parts(parts, Body::from(bytes)), Body::empty())
        }
        Err(_) => {
            // Past the capture limit: the body cannot be read back, and a mutating request
            // answered with an empty body is a client that believes a write it never received.
            abandon(state, scope, subject, key, "the response body could not be captured").await;
            let error_body = json!({
                    "error": {
                        "code": "idempotency_response_too_large",
                        "message": format!(
                            "this endpoint's response exceeds {MAX_CAPTURE_BYTES} bytes and cannot \
                             be stored for replay; the write ran, retry with a new idempotency key \
                             to repeat it"
                        ),
                    }
                })
                .to_string();
            let mut out = Response::from_parts(parts, Body::from(error_body));
            *out.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            (out, Body::empty())
        }
    };

    let mut response = response;
    if let Ok(value) = HeaderValue::from_str(&request_id.to_string()) {
        response.headers_mut().insert(ORIGINAL_HEADER, value);
    }
    response
}

/// The three headers a replay is allowed to inherit, plus the original request id.
///
/// `set-cookie`, `www-authenticate` and every `X-RateLimit-*` are excluded on purpose; see the
/// module header. The set is a `match`, not a filter, so adding a header to this function is a
/// decision somebody wrote down rather than a default somebody inherited.
fn subset_of(headers: &HeaderMap, request_id: Uuid) -> std::collections::BTreeMap<String, String> {
    let mut kept = std::collections::BTreeMap::new();
    for name in ["location", "etag"] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) {
            kept.insert(name.to_owned(), value.to_owned());
        }
    }
    kept.insert(IDEMPOTENCY_ORIGINAL_HEADER.to_owned(), request_id.to_string());
    kept
}

/// Record that a keyed write ran but left nothing replayable behind.
async fn abandon(state: &AppState, scope: &str, subject: &str, key: &str, why: &'static str) {
    match idem_store::abandon(
        state.db().pool(),
        scope,
        subject,
        key,
        OffsetDateTime::now_utc(),
    )
    .await
    {
        Ok(true) => tracing::warn!(scope, key, reason = why, "a keyed write left no replayable response"),
        Ok(false) => {}
        Err(error) => tracing::warn!(error = %error, scope, key, "a key could not be marked failed"),
    }
}

/// Tell the bus a key was replayed with a different body.
///
/// Best-effort and payload-free: the key, the scope and the state travel; the request body, the
/// stored body and the fingerprint do not. A conflict is a client mistake, and an event payload
/// that carries the body is an event payload that lands in every webhook and every log the
/// platform writes.
async fn emit_conflict(
    state: &AppState,
    existing: &omnion_reliability::idempotency::KeyRecord,
    _fingerprint: &str,
) {
    if let Err(error) = bus::emit(
        state.db().pool(),
        NewEvent::new(IDEMPOTENCY_CONFLICT).payload(json!({
            "key": existing.key,
            "scope": existing.scope,
            "method": existing.method,
            "path": existing.path,
            "state": existing.state,
            "replay_count": existing.replay_count,
        })),
    )
    .await
    {
        tracing::warn!(error = %error, "a key conflicted but the event was not emitted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn a_read_is_never_keyed_because_a_replayed_read_is_a_cache() {
        assert!(is_mutating(&Method::POST));
        assert!(is_mutating(&Method::PATCH));
        assert!(!is_mutating(&Method::GET));
        assert!(!is_mutating(&Method::HEAD));
    }

    #[test]
    fn the_key_header_is_found_whatever_case_the_client_used() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("idempotency-key"),
            HeaderValue::from_static("  abc-123  "),
        );
        // The lookup is case-insensitive by construction (HeaderMap folds) and the value is
        // trimmed, so a header written by a proxy with a trailing space is the same key.
        assert_eq!(presented_key(&headers), Some("abc-123"));
    }

    #[test]
    fn an_absent_or_blank_key_is_not_a_key() {
        let mut headers = HeaderMap::new();
        assert_eq!(presented_key(&headers), None);
        headers.insert(
            HeaderName::from_static("idempotency-key"),
            HeaderValue::from_static("   "),
        );
        assert_eq!(presented_key(&headers), None);
    }

    #[test]
    fn the_replay_marker_and_the_scope_are_the_only_headers_a_replay_adds() {
        // The header MAP of a replay is a property of this module and needs no database, so it is
        // unit-tested here; the request id — which is read from a stored row — is proved by the
        // walk, against a real one. Splitting them like this is the honest division: a test that
        // needs a platform to check a header name is a test that will be skipped one day and
        // nobody will read the skip.
        let record = omnion_reliability::idempotency::KeyRecord::new(
            "POST /api/v1/x",
            "user",
            "k",
            "POST",
            "/api/v1/x",
            "hash",
            time::OffsetDateTime::now_utc(),
        );
        let response = serve_stored_marked(&record, 201, "{}".into(), true);
        assert_eq!(response.headers().get(REPLAY_HEADER), Some(&HeaderValue::from_static("true")));
        assert_eq!(
            response.headers().get("idempotency-scope").and_then(|v| v.to_str().ok()),
            Some("POST /api/v1/x"),
            "the scope is echoed so a client can tell WHICH key family answered"
        );
        assert!(response.headers().get("set-cookie").is_none());
        // A request that has never been keyed stores no headers, and `subset_of` is the only
        // thing that decides which headers a replay could ever inherit.
        let empty = subset_of(&HeaderMap::new(), Uuid::new_v4());
        assert_eq!(
            empty.keys().collect::<Vec<_>>(),
            vec![IDEMPOTENCY_ORIGINAL_HEADER],
            "with no stored headers the ONLY header a replay carries is the original request id, \
             and a replay that could inherit anything else is inheriting it from the request"
        );
    }

    #[test]
    fn the_stored_header_subset_is_three_named_headers_and_nothing_else() {
        let mut headers = HeaderMap::new();
        headers.insert("location", HeaderValue::from_static("/api/v1/x/7"));
        headers.insert("etag", HeaderValue::from_static("\"v1\""));
        headers.insert("set-cookie", HeaderValue::from_static("omnion_session=leaked"));
        headers.insert("x-ratelimit-remaining", HeaderValue::from_static("3"));
        headers.insert("www-authenticate", HeaderValue::from_static("Bearer"));
        let id = Uuid::new_v4();
        let kept = subset_of(&headers, id);

        assert_eq!(kept.get("location").map(String::as_str), Some("/api/v1/x/7"));
        assert_eq!(kept.get("etag").map(String::as_str), Some("\"v1\""));
        assert_eq!(kept.get(IDEMPOTENCY_ORIGINAL_HEADER), Some(&id.to_string()));
        // The three a replay must never inherit. A session cookie replayed from a store is a
        // second authentication written by a request that already finished; a stale remaining is
        // a measurement of somebody else's request.
        assert!(!kept.contains_key("set-cookie"));
        assert!(!kept.contains_key("x-ratelimit-remaining"));
        assert!(!kept.contains_key("www-authenticate"));
    }
}
