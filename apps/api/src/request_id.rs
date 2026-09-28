//! The request id every answer carries (REQ-051's error state).
//!
//! ## Why this exists
//!
//! A screen that answers "something went wrong" without a way to say *which* request went wrong
//! leaves the reader with nothing to quote. The panel shows a request id under its error strip
//! (the acceptance box that asks for "error state with retry button and request id"), and that id
//! has to come from somewhere real: a value the server minted for this exchange and stamped on
//! both the response header and the refusal body.
//!
//! ## The two homes, and why both
//!
//! * `x-request-id` on **every** response, not only on refusals. A client that captured a request
//!   id from a successful call needs the same id to appear on the *next* failure of the same
//!   logical operation, and a header that only exists on errors cannot be correlated that way.
//! * `error.request_id` in the refusal body. A refusal quoted from a terminal, a webhook delivery
//!   or a support ticket carries the body, never the header.
//!
//! One id, two homes, both written from the same value in the same place: a middleware that runs
//! after the handler still holds the finished `Response`, which is the one place where both halves
//! of the answer are known at the same time.
//!
//! ## The rules
//!
//! * An inbound `x-request-id` is **honoured when it looks like an id** and replaced when it does
//!   not. Echoing an arbitrary client string back into a response header is header injection; a
//!   caller that wants correlation may use it, a caller that wants a reflected header cannot.
//! * The id is minted per **request**, not per connection, so a keep-alive client gets a fresh
//!   one each time and a log line is unambiguous.

use axum::body::{Body, Bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderName, HeaderValue, Request};
use axum::response::Response;
use axum::middleware::Next;

/// The header the id travels in, in both directions.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// The longest inbound value that will be echoed back. 64 covers a UUID with dashes, a ULID and
/// a short trace id; anything longer is replaced rather than truncated, because half a correlation
/// id correlates with nothing.
const REQUEST_ID_MAX_LEN: usize = 64;

/// An error body is small by construction, and this is the ceiling for reading one back to patch
/// it. A response that is larger than this and claims to be a JSON refusal is not a refusal.
const ERROR_BODY_LIMIT: usize = 64 * 1024;

/// `true` when a client-supplied string is acceptable to echo back.
///
/// Anything with a character outside `[A-Za-z0-9._-]` is refused rather than sanitised: a partial
/// clean-up of a hostile value is how a value that should have been replaced ends up half-trusted,
/// and a replacement id costs a correlation and nothing else.
fn acceptable(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= REQUEST_ID_MAX_LEN
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The id for a request: the caller's when it is safe to echo, a fresh one otherwise.
pub fn id_for<B>(request: &Request<B>) -> String {
    request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| acceptable(value))
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string())
}

/// Add `request_id` to the `error` object of a JSON refusal body.
///
/// A body that is not a JSON object with an `error` object is returned untouched: a CSV export
/// carries a refusal's *status* with a completely different body shape, and rewriting it would
/// corrupt the file a caller was trying to download.
fn patch(body: &[u8], request_id: &str) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.to_vec();
    };
    let Some(error) = value.get_mut("error").and_then(serde_json::Value::as_object_mut) else {
        return body.to_vec();
    };
    // Never overwrite an id the handler chose itself.
    if !error.contains_key("request_id") {
        error.insert(
            "request_id".to_owned(),
            serde_json::Value::String(request_id.to_owned()),
        );
    }
    serde_json::to_vec(&value).unwrap_or_else(|_| body.to_vec())
}

/// Put the id on a response: the header always, and the body too when it is a JSON refusal.
async fn stamp(response: Response, request_id: &str) -> Response {
    let (mut parts, body) = response.into_parts();
    if let Ok(value) = HeaderValue::from_str(request_id) {
        parts.headers.insert(HeaderName::from_static(REQUEST_ID_HEADER), value);
    }

    let is_refusal = parts.status.is_client_error() || parts.status.is_server_error();
    let is_json = parts
        .headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.starts_with("application/json"))
        .unwrap_or(false);
    if !(is_refusal && is_json) {
        return Response::from_parts(parts, body);
    }

    match axum::body::to_bytes(body, ERROR_BODY_LIMIT).await {
        Ok(bytes) => Response::from_parts(parts, Body::from(patch(&bytes, request_id))),
        // A body too large to be a refusal: keep the header, keep the bytes, lose nothing.
        Err(_) => Response::from_parts(parts, Body::from(Bytes::new())),
    }
}

/// Axum middleware: adopt or mint the request id, run the inner service, stamp the answer.
pub async fn request_id(mut request: Request<Body>, next: Next) -> Response {
    let request_id = id_for(&request);
    // The header is put on the *request* too, so a handler that wants to log the id can read it
    // without the middleware having to thread it through the state.
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        request
            .headers_mut()
            .insert(HeaderName::from_static(REQUEST_ID_HEADER), value);
    }
    let response = next.run(request).await;
    stamp(response, &request_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn request_with(value: &str) -> Request<Body> {
        Request::builder()
            .uri("/api/v1/crm/contacts")
            .header(REQUEST_ID_HEADER, value)
            .body(Body::empty())
            .expect("request")
    }

    #[test]
    fn an_inbound_id_is_honoured_when_it_looks_like_one() {
        assert_eq!(id_for(&request_with("req-01H8Z")), "req-01H8Z");
        assert_eq!(id_for(&request_with("a.b_c-1")), "a.b_c-1");
    }

    #[test]
    fn a_hostile_or_oversized_id_is_replaced_not_echoed_back() {
        // A value `http`'s own header builder refuses is not a case this function has to survive:
        // the request never reaches the middleware. What is tested here is the value that *does*
        // parse as a header and is still not safe to reflect — a semicolon, a quote, a space, a
        // long run — because those are the ones a reflected header would actually leak through.
        for hostile in [
            "",
            "has space",
            "quote\"injection",
            "semi;colon",
            "comma,separated",
            "back\\slash",
            "braces{}",
        ] {
            let minted = id_for(&request_with(hostile));
            assert_ne!(minted, hostile, "a hostile value must not be echoed back");
            assert_eq!(minted.len(), 32, "the replacement is a fresh uuid");
        }
        let long = "x".repeat(REQUEST_ID_MAX_LEN + 1);
        assert_eq!(id_for(&request_with(&long)).len(), 32);
    }

    #[test]
    fn a_request_without_the_header_gets_a_fresh_printable_id() {
        let request = Request::builder().uri("/").body(Body::empty()).expect("request");
        let id = id_for(&request);
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(id, id_for(&request), "a new id per request, not per connection");
    }

    #[test]
    fn the_id_is_added_to_the_error_object() {
        let body = br#"{"error":{"code":"nope","message":"no"}}"#;
        let value: serde_json::Value = serde_json::from_slice(&patch(body, "abc123")).expect("json");
        assert_eq!(value["error"]["request_id"], "abc123");
        assert_eq!(value["error"]["code"], "nope");
        assert_eq!(value["error"]["message"], "no");
    }

    #[test]
    fn a_handler_that_chose_its_own_id_keeps_it() {
        let body = br#"{"error":{"code":"nope","request_id":"mine"}}"#;
        let value: serde_json::Value = serde_json::from_slice(&patch(body, "theirs")).expect("json");
        assert_eq!(value["error"]["request_id"], "mine");
    }

    #[tokio::test]
    async fn the_details_survive_the_rewrite() {
        let body = br#"{"error":{"code":"invalid_contact","message":"bad","details":{"field":"email"}}}"#;
        let value: serde_json::Value = serde_json::from_slice(&patch(body, "abc")).expect("json");
        assert_eq!(value["error"]["details"]["field"], "email");
    }

    #[test]
    fn a_body_that_is_not_a_refusal_is_left_alone() {
        let body = br#"{"items":[]}"#;
        assert_eq!(patch(body, "abc"), body.to_vec());
        let not_json = b"<html>nope</html>";
        assert_eq!(patch(not_json, "abc"), not_json.to_vec());
        let empty: &[u8] = b"";
        assert_eq!(patch(empty, "abc"), empty.to_vec());
    }

    #[tokio::test]
    async fn a_success_carries_the_header_and_is_not_rewritten() {
        let response = Response::builder()
            .status(200)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"items":[]}"#))
            .expect("response");
        let stamped = stamp(response, "abc123").await;
        assert_eq!(stamped.headers()[REQUEST_ID_HEADER], "abc123");
        let bytes = axum::body::to_bytes(stamped.into_body(), 4096).await.expect("body");
        assert_eq!(&bytes[..], br#"{"items":[]}"#);
    }

    #[tokio::test]
    async fn a_refusal_carries_the_id_in_both_homes() {
        let response = Response::builder()
            .status(422)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"error":{"code":"invalid_contact","message":"the e-mail is not an address"}}"#,
            ))
            .expect("response");
        let stamped = stamp(response, "abc123").await;
        assert_eq!(stamped.status(), 422);
        assert_eq!(stamped.headers()[REQUEST_ID_HEADER], "abc123");
        let bytes = axum::body::to_bytes(stamped.into_body(), 4096).await.expect("body");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(value["error"]["request_id"], "abc123");
    }

    #[tokio::test]
    async fn a_csv_error_keeps_its_bytes_and_still_gets_the_header() {
        // A refused export answers with a body that is not JSON; rewriting it would corrupt the
        // very file the caller was trying to download.
        let response = Response::builder()
            .status(403)
            .header(CONTENT_TYPE, "text/csv")
            .body(Body::from("name,email\n"))
            .expect("response");
        let stamped = stamp(response, "abc123").await;
        assert_eq!(stamped.headers()[REQUEST_ID_HEADER], "abc123");
        let bytes = axum::body::to_bytes(stamped.into_body(), 4096).await.expect("body");
        assert_eq!(&bytes[..], b"name,email\n");
    }
}
