//! The seam between the request id on the wire and the id in a CRM audit line (REQ-117).
//!
//! The API already knows how to mint a request id — the header travels on the request, so a
//! handler can read it without the middleware threading it through the state (see the
//! request-id middleware's own docs). This module is the one place that **puts that header into
//! the CRM's task-local cell**, and it is deliberately narrow:
//!
//! * It is a **router-level middleware**, not a per-route handler, so a route added tomorrow
//!   gets a correlated audit line without anybody remembering to wrap it. The alternative —
//!   calling `request_id::scope(..)` at the top of fourteen handlers — is fourteen chances to
//!   forget, and forgetting is silent: the line is written with a `null` id.
//! * It **adopts the id the middleware minted** rather than minting a second one. Two ids for one
//!   exchange is worse than no id: the one in the audit row would not match the one on the
//!   response header, which is the only reason a correlation id exists.
//! * It runs **inside** the router, after the CSRF and header layers, so a request that never
//!   reaches a handler never opens a scope.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::extract::Request;
use axum::response::Response;
use omnion_module_crm_intake::request_id as crm_request_id;
use tower::Layer;
// `tower::Service` is re-exported so this layer implements the same trait the router's own layers
// do; depending on `tower-service` directly would add a dependency for one import. Imported by
// name, not as `_`: an anonymous import puts the trait in scope but not its *name*, which is what
// a `Service` impl and its associated types need.
use tower::Service;

/// The header the id travels in, in both directions.
const REQUEST_ID_HEADER: &str = "x-request-id";

/// The longest inbound value that will be correlated. 64 covers a UUID with dashes, a ULID and a
/// short trace id; anything longer is replaced rather than truncated, because half a correlation
/// id correlates with nothing.
const MAX_LEN: usize = 64;

/// `true` when a client-supplied string is acceptable to record.
///
/// Anything outside `[A-Za-z0-9._-]` is refused rather than sanitised: a partial clean-up of a
/// hostile value is how a value that should have been replaced ends up half-trusted, and the id
/// is written into a JSON column a compliance export reads.
fn acceptable(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_LEN
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The id to correlate for this exchange.
///
/// Reads the header the request-id middleware put on the request. A request that never passed
/// through that middleware — an in-process test, a route mounted outside it — gets `None`, which
/// is the honest answer: there is no exchange to point at, and inventing one would write a value
/// that correlates with nothing.
fn adopt(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| acceptable(value))
        .map(str::to_owned)
}

/// Layer opening a CRM request-id scope around every request.
#[derive(Clone, Copy, Default)]
pub struct CorrelateCrm;

impl<S> Layer<S> for CorrelateCrm {
    type Service = CorrelateCrmService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CorrelateCrmService { inner }
    }
}

/// Service produced by [`CorrelateCrm`].
#[derive(Clone)]
pub struct CorrelateCrmService<S> {
    inner: S,
}

impl<S, B> Service<Request<B>> for CorrelateCrmService<S>
where
    S: Service<Request<B>, Response = Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send + 'static,
    B: Send + 'static,
{
    type Response = Response;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<B>) -> Self::Future {
        let id = adopt(request.headers());
        // `Service::call` takes `&mut self`, so the clone is the one that runs and this one stays
        // available for the next request.
        let mut inner = self.inner.clone();
        // The inner service's own `Result` is returned, not wrapped: this layer only opens and
        // closes a scope, so it has nothing to turn a refusal into. Wrapping it would make the
        // router's error type ambiguous — `Result<Result<Response, Infallible>, _>`.
        Box::pin(async move {
            crm_request_id::scope(id, inner.call(request)).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;

    fn headers_with(value: &str) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::HeaderName::from_static(REQUEST_ID_HEADER),
            value.parse().expect("header value parses"),
        );
        headers
    }

    #[test]
    fn a_well_formed_id_is_adopted() {
        assert_eq!(adopt(&headers_with("req-01H8Z")).as_deref(), Some("req-01H8Z"));
        assert_eq!(adopt(&headers_with("a.b_c-1")).as_deref(), Some("a.b_c-1"));
    }

    #[test]
    fn a_request_with_no_id_gets_none_rather_than_an_invented_one() {
        assert_eq!(adopt(&axum::http::HeaderMap::new()), None);
    }

    #[test]
    fn a_hostile_or_oversized_id_is_not_recorded() {
        // The value is written into a JSON column a compliance export reads, so the same rule the
        // response header has to survive applies here — and the replacement is `None`, not a
        // different id, because this layer does not mint.
        for hostile in ["", "has space", "quote\"injection", "semi;colon", "back\\slash"] {
            assert_eq!(adopt(&headers_with(hostile)), None, "hostile: {hostile}");
        }
        let long = "x".repeat(MAX_LEN + 1);
        assert_eq!(adopt(&headers_with(&long)), None);
    }

    #[tokio::test]
    async fn the_layer_puts_the_header_id_where_the_store_will_read_it() {
        use tower::ServiceExt;

        // The handler reads the cell, which is the only place the value is consumed — so the
        // test reads it the same way. An earlier version of this test asserted only that the
        // scope *closed* after the request, which a layer that never opened one would also pass.
        async fn what_the_handler_sees() -> Option<String> {
            crm_request_id::current()
        }
        let app = axum::Router::new()
            .route(
                "/",
                axum::routing::get(|| async { axum::Json(serde_json::json!({ "seen": what_the_handler_sees().await })) }),
            )
            .layer(CorrelateCrm)
            .with_state(());

        let request = HttpRequest::builder()
            .uri("/")
            .header(REQUEST_ID_HEADER, "req-scoped-1")
            .body(axum::body::Body::empty())
            .expect("request");
        let response = app.oneshot(request).await.expect("the router answers");
        assert_eq!(response.status(), 200);

        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(
            body["seen"],
            serde_json::Value::String("req-scoped-1".into()),
            "the handler must see the header's id, not None"
        );

        // And the scope closes with the request: the next exchange must not inherit this one.
        assert_eq!(crm_request_id::current(), None);
    }

    #[tokio::test]
    async fn a_request_with_no_id_gives_the_handler_none() {
        use tower::ServiceExt;

        let app = axum::Router::new()
            .route(
                "/",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({ "seen": crm_request_id::current() }))
                }),
            )
            .layer(CorrelateCrm)
            .with_state(());

        let response = app
            .oneshot(HttpRequest::builder().uri("/").body(axum::body::Body::empty()).expect("request"))
            .await
            .expect("the router answers");
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(body["seen"], serde_json::Value::Null, "no id in, no id out");
    }

    #[tokio::test]
    async fn two_requests_do_not_see_each_others_id() {
        use tower::ServiceExt;

        // The cross-talk case. A shared cell would write one exchange's id onto the other's audit
        // line, and neither row would show anything wrong.
        async fn what_the_handler_sees() -> Option<String> {
            crm_request_id::current()
        }
        let build = || {
            axum::Router::new()
                .route(
                    "/",
                    axum::routing::get(|| async {
                        axum::Json(serde_json::json!({ "seen": what_the_handler_sees().await }))
                    }),
                )
                .layer(CorrelateCrm)
                .with_state(())
        };

        let call = |id: &'static str| async move {
            let request = HttpRequest::builder()
                .uri("/")
                .header(REQUEST_ID_HEADER, id)
                .body(axum::body::Body::empty())
                .expect("request");
            let response = build().oneshot(request).await.expect("the router answers");
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .expect("body");
            serde_json::from_slice::<serde_json::Value>(&bytes).expect("json")["seen"].clone()
        };

        let (first, second) = tokio::join!(call("req-one"), call("req-two"));
        assert_eq!(first, serde_json::Value::String("req-one".into()));
        assert_eq!(second, serde_json::Value::String("req-two".into()));
    }
}
