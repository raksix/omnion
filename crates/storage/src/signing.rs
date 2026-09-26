//! AWS Signature Version 4 for S3 requests.
//!
//! MinIO (development) and any S3-compatible endpoint (production) authenticate a request with
//! the same signature, so this crate signs its own requests instead of pulling a vendor SDK
//! (docs/04-MONOREPO.md: keep the dependency surface small). The implementation follows the
//! documented algorithm and is checked against the published example signature in the tests
//! below.
//!
//! A signed request is the URL (with its query string) plus the headers to send, including the
//! `Authorization` header. Nothing here talks to the network — [`crate::s3`] sends what this
//! module produces.

use hmac::{Hmac, Mac};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use sha2::{Digest, Sha256};

/// Hex-encoded SHA-256 of a payload — the `x-amz-content-sha256` value.
#[must_use]
pub fn payload_hash(payload: &[u8]) -> String {
    hex::encode(Sha256::digest(payload))
}

/// SHA-256 of the empty payload, used by requests without a body.
pub const EMPTY_PAYLOAD_HASH: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Characters that stay literal in the canonical URI: the RFC 3986 unreserved set plus `/`.
const URI_ALLOWED: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

/// Characters that must be encoded inside a query-string component.
const QUERY_ALLOWED: &AsciiSet = &URI_ALLOWED
    .add(b'/')
    .add(b':')
    .add(b'@')
    .add(b'&')
    .add(b'=')
    .add(b'+')
    .add(b',');

type HmacSha256 = Hmac<Sha256>;

/// The credentials one signature is made with.
#[derive(Debug, Clone)]
pub struct Credentials {
    /// Access key id.
    pub access_key: String,
    /// Secret access key.
    pub secret_key: String,
}

/// Everything the signer needs about one request.
#[derive(Debug, Clone)]
pub struct RequestToSign<'request> {
    /// HTTP method, upper-case (`GET`, `PUT`, `DELETE`, …).
    pub method: &'request str,
    /// Host that answers the request, port included when it is not the scheme default.
    pub host: &'request str,
    /// Path of the request, already percent-encoded, starting with `/`.
    pub path: &'request str,
    /// Query parameters, in the order the URL carries them (the signer sorts them).
    pub query: Vec<(&'request str, &'request str)>,
    /// Headers that belong to the signature (`content-type`, `range`, …), lower-case names.
    pub headers: Vec<(&'request str, &'request str)>,
    /// Hex-encoded SHA-256 of the body; [`EMPTY_PAYLOAD_HASH`] when there is none.
    pub payload_hash: &'request str,
}

/// A signed request, ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedRequest {
    /// Query string the URL carries, canonicalised so the signature and the URL agree.
    pub query: String,
    /// Headers to send, including `Authorization`; sorted by name.
    pub headers: Vec<(String, String)>,
    /// Canonical request that was signed — kept for diagnostics and tests.
    pub canonical_request: String,
    /// String-to-sign of the request.
    pub string_to_sign: String,
    /// Signature, hex-encoded.
    pub signature: String,
}

impl SignedRequest {
    /// Value of one signed header, by lower-case name.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Sign one request with SigV4 (`s3` service).
///
/// `datetime` must be the UTC timestamp of the request in basic ISO 8601
/// (`20130524T000000Z`); `date` is its day part (`20130524`).
pub fn sign(
    request: &RequestToSign<'_>,
    credentials: &Credentials,
    region: &str,
    datetime: &str,
    date: &str,
) -> SignedRequest {
    let mut headers: Vec<(String, String)> = vec![
        ("host".to_owned(), request.host.to_owned()),
        (
            "x-amz-content-sha256".to_owned(),
            request.payload_hash.to_owned(),
        ),
        ("x-amz-date".to_owned(), datetime.to_owned()),
    ];
    for (name, value) in &request.headers {
        headers.push(((*name).to_owned(), (*value).to_owned()));
    }
    headers.sort_by(|left, right| left.0.cmp(&right.0));
    headers.dedup_by(|left, right| left.0 == right.0);

    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}:{}\n", value.trim()))
        .collect();
    let signed_headers: String = headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_query = canonical_query_string(&request.query);
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        request.method,
        request.path,
        canonical_query,
        canonical_headers,
        signed_headers,
        request.payload_hash
    );

    let scope = format!("{date}/{region}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{datetime}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );

    let signing_key = signing_key(&credentials.secret_key, date, region);
    let signature = hex::encode(hmac(&signing_key, string_to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key
    );

    headers.push(("authorization".to_owned(), authorization));
    headers.sort_by(|left, right| left.0.cmp(&right.0));

    SignedRequest {
        query: canonical_query,
        headers,
        canonical_request,
        string_to_sign,
        signature,
    }
}

/// Percent-encode one URI path, keeping the separators literal.
#[must_use]
pub fn encode_path(path: &str) -> String {
    utf8_percent_encode(path, URI_ALLOWED).to_string()
}

/// Percent-encode one query-string or header component.
#[must_use]
pub fn encode_component(value: &str) -> String {
    utf8_percent_encode(value, QUERY_ALLOWED).to_string()
}

/// Canonical query string: components encoded, sorted by name then value.
fn canonical_query_string(query: &[(&str, &str)]) -> String {
    let mut encoded: Vec<(String, String)> = query
        .iter()
        .map(|(name, value)| (encode_component(name), encode_component(value)))
        .collect();
    encoded.sort();
    encoded
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Derive the SigV4 signing key: `AWS4{secret}` → date → region → service → terminator.
fn signing_key(secret_key: &str, date: &str, region: &str) -> Vec<u8> {
    let mut key = hmac(format!("AWS4{secret_key}").as_bytes(), date.as_bytes());
    key = hmac(&key, region.as_bytes());
    key = hmac(&key, b"s3");
    hmac(&key, b"aws4_request")
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published example of the complete SigV4 signing process ("Example: GET Object" of the
    /// AWS S3 API reference), reproduced with the documented example credentials.
    #[test]
    fn the_published_example_signature_matches() {
        let credentials = Credentials {
            access_key: "AKIAIOSFODNN7EXAMPLE".to_owned(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_owned(),
        };
        let request = RequestToSign {
            method: "GET",
            host: "examplebucket.s3.amazonaws.com",
            path: "/test.txt",
            query: vec![],
            headers: vec![("range", "bytes=0-9")],
            payload_hash: EMPTY_PAYLOAD_HASH,
        };

        let signed = sign(
            &request,
            &credentials,
            "us-east-1",
            "20130524T000000Z",
            "20130524",
        );

        assert_eq!(
            signed.canonical_request,
            "GET\n/test.txt\n\nhost:examplebucket.s3.amazonaws.com\n\
             range:bytes=0-9\n\
             x-amz-content-sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n\
             x-amz-date:20130524T000000Z\n\n\
             host;range;x-amz-content-sha256;x-amz-date\n\
             e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            signed.string_to_sign,
            "AWS4-HMAC-SHA256\n20130524T000000Z\n20130524/us-east-1/s3/aws4_request\n\
             7344ae5b7ee6c3e7e6b0fe0640412a37625d1fbfff95c48bbb2dc43964946972"
        );
        assert_eq!(
            signed.signature,
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        assert_eq!(
            signed.header("authorization"),
            Some(
                "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/\
                 aws4_request, SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
                 Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
            )
        );
    }

    /// The published example of a signed PUT with a query string
    /// ("Example: PUT Object" / "Example: GET Bucket Lifecycle" style requests): the signature
    /// covers the canonicalised query, so a `?lifecycle` request must sign that parameter.
    #[test]
    fn a_query_parameter_enters_the_signature() {
        let credentials = Credentials {
            access_key: "AKIAIOSFODNN7EXAMPLE".to_owned(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_owned(),
        };
        let request = RequestToSign {
            method: "GET",
            host: "examplebucket.s3.amazonaws.com",
            path: "/",
            query: vec![("lifecycle", "")],
            headers: vec![],
            payload_hash: EMPTY_PAYLOAD_HASH,
        };

        let signed = sign(
            &request,
            &credentials,
            "us-east-1",
            "20130524T000000Z",
            "20130524",
        );

        assert_eq!(
            signed.signature,
            "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543"
        );
        assert_eq!(signed.query, "lifecycle=");
    }

    #[test]
    fn paths_and_components_are_percent_encoded() {
        assert_eq!(encode_path("/sites/a b/ç.png"), "/sites/a%20b/%C3%A7.png");
        assert_eq!(encode_component("a b&c"), "a%20b%26c");
    }

    #[test]
    fn the_payload_hash_is_the_sha256_of_the_body() {
        assert_eq!(payload_hash(b""), EMPTY_PAYLOAD_HASH);
        assert_eq!(
            payload_hash(b"omnion"),
            "f2503e75006348ceef2daa88f76b568426f1024a08fd0a77932f045c6d871928"
        );
    }

    #[test]
    fn headers_are_signed_sorted_and_deduplicated() {
        let credentials = Credentials {
            access_key: "key".to_owned(),
            secret_key: "secret".to_owned(),
        };
        let request = RequestToSign {
            method: "PUT",
            host: "127.0.0.1:9000",
            path: "/omnion-media/sites/a.png",
            query: vec![],
            headers: vec![("content-type", "image/png"), ("content-type", "image/png")],
            payload_hash: &payload_hash(b"bytes"),
        };

        let signed = sign(
            &request,
            &credentials,
            "us-east-1",
            "20260926T000000Z",
            "20260926",
        );

        let signed_names: Vec<&str> = signed
            .headers
            .iter()
            .filter(|(name, _)| name != "authorization")
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(
            signed_names,
            vec!["content-type", "host", "x-amz-content-sha256", "x-amz-date"],
            "a duplicated header is signed once"
        );
        assert!(
            signed
                .canonical_request
                .contains("content-type;host;x-amz-content-sha256;x-amz-date\n"),
            "the signed-header line lists every signed header: {}",
            signed.canonical_request
        );
    }
}
