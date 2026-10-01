//! The cache policy a public request is served under (REQ-011, slice 1).
//!
//! This is the seam the request did not have: `omnion-cdn` knows *how to decide*, and
//! until now nothing in `apps/api` called it — the public surface wrote a literal
//! `public, max-age=3600` into every response, so a cache rule an operator created
//! changed nothing a visitor's browser did. Two jobs live here:
//!
//! 1. **Load the ordered rule set for a site** and turn a request into a [`Decision`].
//! 2. **Render that decision onto a response** — the headers from `omnion-cdn`, plus the
//!    `ETag` and the `304` shortcut, which is where a validator is actually worth
//!    something.
//!
//! Two deliberate rules:
//!
//! * **A rule read that fails is not a failed request.** A site whose one rule has a
//!   pattern a hand-edited row broke still gets its pages — private, because nothing
//!   decided otherwise. Turning a cache misconfiguration into a `500` on the public
//!   surface would take a site's whole front end down over a header.
//! * **A rule with a broken pattern is counted, not hidden.** [`Policy::broken_rules`]
//!   carries the ids so the panel can say which rule is not firing, which is the only
//!   way an operator finds a rule that silently never applies.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Response, StatusCode, header};
use omnion_cdn::etag::{etag_for_file, etag_for_page, if_none_match_hits, vary_for};
use omnion_cdn::matcher::RequestShape;
use omnion_cdn::store;
use omnion_cdn::{CacheKey, CacheRule, Decision, headers_for, surrogate_keys};
use sqlx::PgPool;
use uuid::Uuid;

/// The tag that names a whole site, appended to every path-derived tag.
///
/// Not derived in `omnion-cdn` because only the caller knows which site a response
/// belongs to, and a wrong site tag would let one site's purge evict another's cache.
fn site_tag(site_id: Uuid) -> String {
    format!("site-{site_id}")
}

/// The decision for one public request, plus everything needed to render it.
#[derive(Debug, Clone)]
pub struct Policy {
    /// What the rule set decided.
    pub decision: Decision,
    /// The key components of the rule that decided, for the `Vary` header.
    pub key: CacheKey,
    /// Tags the response belongs to, for a tag-based purge.
    pub tags: Vec<String>,
    /// Rules that exist but whose pattern no longer compiles.
    pub broken_rules: Vec<Uuid>,
}

impl Policy {
    /// The private policy: nothing decided this response may be shared.
    fn private(site_id: Uuid) -> Self {
        Self {
            decision: Decision::Private { reason: "no_rule" },
            key: CacheKey::default(),
            tags: vec![site_tag(site_id)],
            broken_rules: Vec::new(),
        }
    }

    /// Whether a response under this policy may be stored by a shared cache.
    #[must_use]
    pub fn is_cacheable(&self) -> bool {
        matches!(self.decision, Decision::Cacheable { .. })
    }
}

/// The cookie the panel and the renderer use to pick a language.
const LANGUAGE_COOKIE: &str = "omnion_lang";

/// The request, decomposed into the parts a cache rule is allowed to see.
///
/// A thin constructor over `RequestShape`: the point of having it here is that the two
/// header *name* lists are built from a real `HeaderMap` in one place, and only names —
/// a value that reached a cache key would be a session token in a cache index.
#[must_use]
pub fn request_shape(path: &str, query: Option<&str>, headers: &HeaderMap) -> RequestShape {
    RequestShape::bare(path)
        .with_query(query.unwrap_or_default())
        .with_cookies(cookie_names(headers))
        .with_headers(headers.keys().map(|name| name.as_str().to_string()))
        .with_language(cookie_value(headers, LANGUAGE_COOKIE).unwrap_or_default())
}

/// Decide a public request's policy for a site.
///
/// `None` for the site means the platform-default rules, which an installation with no
/// per-site rules uses; an installation with no rules at all decides `Private`, which is
/// the correct answer for a surface nobody has decided to share.
pub async fn policy_for(pool: &PgPool, site_id: Uuid, request: &RequestShape) -> Policy {
    match store::list_rules_compiled(pool, site_id).await {
        Ok((rules, broken)) => {
            let decision = omnion_cdn::decide(&rules, request);
            let key = key_of(&rules, &decision);
            let mut tags = surrogate_keys(request.path.as_str());
            tags.push(site_tag(site_id));
            Policy {
                decision,
                key,
                tags,
                broken_rules: broken.into_iter().map(|(id, _)| id).collect(),
            }
        }
        // An unreadable rule set is a cache problem, not a serving problem. The response
        // goes out private, which is what the surface did before the CDN layer existed.
        Err(_) => Policy::private(site_id),
    }
}

/// The key components of the rule that produced the decision.
///
/// Read back off the matching rule rather than derived from the decision's cache key:
/// the decision carries a *derived* key (a string), and re-parsing a string to recover
/// which components were used is exactly how two implementations of "what does this rule
/// key on" start to disagree.
fn key_of(rules: &[CacheRule], decision: &Decision) -> CacheKey {
    let Decision::Cacheable { rule, .. } = decision else {
        return CacheKey::default();
    };
    rules
        .iter()
        .find(|candidate| &candidate.name == rule)
        .map(|candidate| candidate.cache_key.clone())
        .unwrap_or_default()
}

/// The `ETag` a response carries, from the content it is serving.
///
/// Public because the call site names the variant: a handler knows whether it is serving a
/// page or a file, and making it compute a validator itself would put the same derivation in
/// three places.
#[derive(Debug, Clone, Copy)]
pub enum Validator<'a> {
    /// A page: its revision and the body as rendered.
    Page {
        /// Published revision number.
        revision_no: i32,
        /// The rendered body.
        body: &'a str,
    },
    /// A file: the checksum the upload computed, with the id as a fallback.
    File {
        /// Stored checksum.
        checksum: &'a str,
        /// File id, used when there is no checksum.
        id: Uuid,
    },
    /// A body that already carries its own validator; this layer must not invent a second one.
    ///
    /// Added by the media merge. The public file path runs **main's** conditional machinery
    /// (a `weak` ETag derived from the checksum plus a `Last-Modified` date) and **my** cache
    /// layer, and both wrote `ETag` on the same response. Whichever wrote last won, and the
    /// two derivations are not the same string — so a client that revalidated against the
    /// `ETag` one of them reported was compared against the other, and never matched. It
    /// surfaces as a media file that re-downloads on every request, which reads as "caching
    /// does not work" rather than as two layers disagreeing.
    ///
    /// So the file path answers its own conditional request *before* the policy is applied and
    /// hands the rest of the work here: the cache layer then only writes what it owns
    /// (`Cache-Control`, `Vary`, `surrogate-key`) and leaves the validator it finds in place.
    /// Keeping one derivation per response is worth more than letting this layer be the single
    /// place a validator comes from, because the conditional machinery has to stay next to the
    /// `Range` logic that shares its instant.
    Preserved,
}

impl Validator<'_> {
    /// The validator to advertise, or `None` to keep the one the body already carries.
    fn etag(&self) -> Option<String> {
        match self {
            Validator::Page { revision_no, body } => Some(etag_for_page(*revision_no, body)),
            Validator::File { checksum, id } => Some(etag_for_file(checksum, *id)),
            Validator::Preserved => None,
        }
    }
}

/// Write the cache headers of a policy onto a response, and answer `304` when it may.
///
/// Returns the response to send. A conditional request that matches the validator gets a
/// `304` with the same cache headers and no body — the point of the validator. The
/// headers are written **before** that decision rather than after it, so the `304` a
/// client revalidates against is the same one a fresh `200` would have carried.
///
/// `Vary` is written from the rule's key components. Note the asymmetry: a *private*
/// response sends no `Vary`, because nothing stores it and varying would only split an
/// intermediary's cache for no benefit.
pub fn apply(
    response: Response<Body>,
    policy: &Policy,
    validator: Validator<'_>,
    request_headers: &HeaderMap,
) -> Response<Body> {
    let mut response = response;
    // A `Preserved` body already advertised its own validator; read it back rather than
    // deriving a second, because the header on the wire is what a client revalidates against
    // and two derivations of "the same" file are two different strings.
    let carried = response
        .headers()
        .get(header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let etag = validator.etag().or(carried);
    let headers = response.headers_mut();

    for (name, value) in headers_for(&policy.decision) {
        // A header value that cannot be built is dropped rather than turned into a 500:
        // the body is already rendered and correct, and a cache header is not worth a
        // failed request. The values here are all crate-generated, so this is unreachable
        // in practice — which is exactly why it must not be the thing that breaks a page.
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    }

    if let Some(vary) = vary_for(&policy.decision, &policy.key) {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(b"Vary"),
            HeaderValue::from_str(&vary),
        ) {
            headers.insert(name, value);
        }
    }

    if let Ok(value) = HeaderValue::from_str(&policy.tags.join(" ")) {
        headers.insert(HeaderName::from_static("surrogate-key"), value);
    }
    if let Some(etag) = &etag
        && let Ok(value) = HeaderValue::from_str(etag)
    {
        headers.insert(header::ETAG, value);
    }

    // The conditional request is answered against the validator that was just written, so
    // the two can never disagree about what this response is. A body that carried no
    // validator at all revalidates to nothing: this layer did not produce one, and inventing
    // a comparison against an absent `ETag` would answer `304` for a `200` nobody can check.
    let revalidated = etag
        .as_deref()
        .and_then(|etag| {
            request_headers
                .get(header::IF_NONE_MATCH)
                .and_then(|value| value.to_str().ok())
                .map(|value| if_none_match_hits(value, etag))
        })
        .unwrap_or(false);

    if revalidated {
        // Built from parts with an *empty* body and then stripped again: `Body::empty()`
        // carries its own `content-length: 0`, and axum writes that on the way out, so
        // removing the header from a response that already has the body is not enough.
        // A `304` must not advertise a length at all — a client that believes one has to
        // wait for bytes that never come.
        let (mut parts, _body) = response.into_parts();
        parts.headers.remove(header::CONTENT_LENGTH);
        parts.headers.remove(header::TRANSFER_ENCODING);
        parts.status = StatusCode::NOT_MODIFIED;
        let mut response = Response::from_parts(parts, Body::empty());
        response.headers_mut().remove(header::CONTENT_LENGTH);
        return response;
    }
    response
}

/// The names of the cookies a request carried, in the order they appeared.
///
/// Parsed rather than taken from a `Cookie` header wholesale: a cache rule matches on a
/// cookie *name*, and passing a header value into the matcher is the shape of bug where a
/// session token ends up in a cache key.
#[must_use]
pub fn cookie_names(headers: &HeaderMap) -> Vec<String> {
    cookie_pairs(headers)
        .map(|(name, _)| name.to_string())
        .collect()
}

/// One `(name, value)` pair per cookie the request carried.
///
/// A pair with no `=` is still a cookie — a flag set as `Set-Cookie: preview` arrives
/// back as `Cookie: preview` — and it is exactly the shape a bypass rule names. Dropping
/// it would mean a rule saying "do not cache anyone carrying `preview`" quietly fails to
/// fire for the people who set the flag, which is the direction a cache bug has to fail.
fn cookie_pairs(headers: &HeaderMap) -> impl Iterator<Item = (&str, &str)> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) => (name.trim(), value.trim()),
            None => (pair, ""),
        })
}

/// The value of one cookie, when the request carried it.
fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    cookie_pairs(headers)
        .find(|(cookie, _)| *cookie == name)
        .map(|(_, value)| value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    fn headers_of(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut builder = Request::builder().uri("/");
        for (name, value) in pairs {
            builder = builder.header(*name, *value);
        }
        builder.body(()).expect("request builds").headers().clone()
    }

    fn cacheable() -> Policy {
        Policy {
            decision: Decision::Cacheable {
                rule: "blog".into(),
                edge_ttl_seconds: 3600,
                browser_ttl_seconds: 60,
                swr_seconds: 0,
                cache_key: "site.test|/blog/hello".into(),
            },
            key: CacheKey {
                host: false,
                path: true,
                query_allow: vec![],
                language_cookie: true,
            },
            tags: vec!["/blog/hello".into(), "site-1".into()],
            broken_rules: Vec::new(),
        }
    }

    fn private() -> Policy {
        Policy {
            decision: Decision::Private { reason: "no_rule" },
            key: CacheKey::default(),
            tags: vec!["site-1".into()],
            broken_rules: Vec::new(),
        }
    }

    fn body() -> Response<Body> {
        Response::new(Body::from("hello"))
    }

    fn header(response: &Response<Body>, name: &str) -> Option<String> {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    #[test]
    fn a_cacheable_response_carries_the_ttls_the_rule_named() {
        let response = apply(
            body(),
            &cacheable(),
            Validator::Page {
                revision_no: 1,
                body: "hello",
            },
            &HeaderMap::new(),
        );
        assert_eq!(
            header(&response, "cache-control").as_deref(),
            Some("public, max-age=60")
        );
        assert_eq!(
            header(&response, "cdn-cache-control").as_deref(),
            Some("public, max-age=3600")
        );
    }

    #[test]
    fn a_private_response_sends_no_store_and_no_vary() {
        let response = apply(
            body(),
            &private(),
            Validator::Page {
                revision_no: 1,
                body: "hello",
            },
            &HeaderMap::new(),
        );
        assert_eq!(
            header(&response, "cache-control").as_deref(),
            Some("private, no-store")
        );
        assert_eq!(
            header(&response, "vary"),
            None,
            "a response nobody stores has nothing to vary on"
        );
    }

    #[test]
    fn a_response_carries_its_surrogate_tags_including_the_site() {
        let response = apply(
            body(),
            &cacheable(),
            Validator::File {
                checksum: &"a".repeat(64),
                id: Uuid::nil(),
            },
            &HeaderMap::new(),
        );
        let tags = header(&response, "surrogate-key").expect("tags are written");
        assert!(tags.contains("/blog/hello"));
        assert!(tags.contains("site-1"));
    }

    #[test]
    fn a_conditional_request_gets_a_304_with_the_same_cache_headers_and_no_body() {
        let etag = etag_for_page(1, "hello");
        let request = headers_of(&[("if-none-match", &etag)]);
        let response = apply(
            body(),
            &cacheable(),
            Validator::Page {
                revision_no: 1,
                body: "hello",
            },
            &request,
        );
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(header(&response, "etag").as_deref(), Some(etag.as_str()));
        assert_eq!(
            header(&response, "cache-control").as_deref(),
            Some("public, max-age=60"),
            "a 304 that drops the TTLs makes the client refetch next time"
        );
        assert_eq!(
            response.headers().get(header::CONTENT_LENGTH),
            None,
            "a 304 with the 200's length is a lie the client may believe"
        );
    }

    #[test]
    fn a_conditional_request_for_another_validator_gets_the_full_response() {
        let request = headers_of(&[("if-none-match", &etag_for_page(2, "different"))]);
        let response = apply(
            body(),
            &cacheable(),
            Validator::Page {
                revision_no: 1,
                body: "hello",
            },
            &request,
        );
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn a_private_response_still_answers_a_conditional_request() {
        // A validator is a fact about the bytes, not a permission. A response that is not
        // stored is still the same bytes, so revalidating it must not fail the request.
        let etag = etag_for_page(1, "hello");
        let request = headers_of(&[("if-none-match", &etag)]);
        let response = apply(
            body(),
            &private(),
            Validator::Page {
                revision_no: 1,
                body: "hello",
            },
            &request,
        );
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            header(&response, "cache-control").as_deref(),
            Some("private, no-store")
        );
    }

    #[test]
    fn a_rule_keying_on_the_language_cookie_varies_on_cookie() {
        let response = apply(
            body(),
            &cacheable(),
            Validator::Page {
                revision_no: 1,
                body: "hello",
            },
            &HeaderMap::new(),
        );
        assert_eq!(header(&response, "vary").as_deref(), Some("Cookie"));
    }

    #[test]
    fn a_file_validator_is_the_upload_checksum() {
        let checksum = "b".repeat(64);
        let response = apply(
            body(),
            &cacheable(),
            Validator::File {
                checksum: &checksum,
                id: Uuid::nil(),
            },
            &HeaderMap::new(),
        );
        assert_eq!(
            header(&response, "etag").as_deref(),
            Some(format!("\"{checksum}\"").as_str())
        );
    }

    #[test]
    fn a_preserved_validator_is_the_one_the_body_carried() {
        // The merge case, restated as a unit test because the failure it prevents is silent:
        // a file response reaches this layer already carrying the weak ETag the conditional
        // machinery derived, and re-deriving one here would overwrite it with a *strong* tag
        // of the same checksum. The client would revalidate against the tag it was given and
        // be told no, forever, with nothing in the logs to say why.
        let carried = "W/\"b".to_string() + &"b".repeat(63) + "\"";
        let mut response = body();
        response
            .headers_mut()
            .insert(header::ETAG, HeaderValue::from_str(&carried).expect("a valid tag"));

        let response = apply(
            response,
            &cacheable(),
            Validator::Preserved,
            &HeaderMap::new(),
        );
        assert_eq!(
            header(&response, "etag").as_deref(),
            Some(carried.as_str()),
            "the cache layer must not replace a validator the body already published"
        );
    }

    #[test]
    fn a_preserved_validator_still_answers_its_own_conditional_request() {
        let carried = "W/\"preserved\"";
        let request = headers_of(&[("if-none-match", carried)]);
        let mut response = body();
        response
            .headers_mut()
            .insert(header::ETAG, HeaderValue::from_static("W/\"preserved\""));

        let response = apply(response, &cacheable(), Validator::Preserved, &request);
        assert_eq!(
            response.status(),
            StatusCode::NOT_MODIFIED,
            "revalidating against the carried tag must reach the same body as before"
        );
        assert_eq!(header(&response, "etag").as_deref(), Some(carried));
    }

    #[test]
    fn a_body_with_no_validator_is_not_revalidated_against_nothing() {
        // The counterpart of the case above: with nothing to compare, this layer must leave the
        // response alone. Answering `304` because the header was *absent* would drop a body a
        // client never claimed to have.
        let response = apply(body(), &cacheable(), Validator::Preserved, &headers_of(&[(
            "if-none-match",
            "\"anything\"",
        )]));
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header(&response, "etag"), None);
    }

    #[test]
    fn the_parts_a_request_presents_name_cookies_without_their_values() {
        let headers = headers_of(&[("cookie", "omnion_lang=tr; preview=1")]);
        let shape = request_shape("/blog/hello", Some("page=2"), &headers);
        assert_eq!(shape.language.as_deref(), Some("tr"));
        assert_eq!(shape.cookies, ["omnion_lang", "preview"]);
        assert!(
            !shape.cookies.iter().any(|c| c.contains("preview=1")),
            "a cookie value never becomes part of the shape"
        );
        assert_eq!(shape.query.as_deref(), Some("page=2"));
        assert_eq!(shape.path, "/blog/hello");
        assert_eq!(shape.method, "GET");
    }

    #[test]
    fn the_parts_report_the_header_names_and_no_host() {
        let headers = headers_of(&[("host", "site.test"), ("accept", "*/*")]);
        let shape = request_shape("/blog", None, &headers);
        assert!(shape.headers.iter().any(|name| name == "accept"));
        assert_eq!(
            shape.host, None,
            "the host is resolved to a site before this point, so the key cannot claim one"
        );
    }

    #[test]
    fn a_cookie_with_no_equals_is_still_a_name() {
        let headers = headers_of(&[("cookie", "flag; omnion_lang=de")]);
        let names = cookie_names(&headers);
        assert!(names.contains(&"flag".to_string()));
        assert_eq!(cookie_value(&headers, "omnion_lang"), Some("de"));
        assert_eq!(cookie_value(&headers, "absent"), None);
    }

    #[test]
    fn a_valueless_language_cookie_reads_as_no_language() {
        // `Cookie: omnion_lang` with no value is a flag, not a language. Reporting `Some("")`
        // would key every such request identically to a request in no language, which is the
        // same bucket — but reporting it as a *present* language would make the shape claim
        // the request said something about its language that it did not.
        let headers = headers_of(&[("cookie", "omnion_lang")]);
        let shape = request_shape("/", None, &headers);
        assert_eq!(shape.language, None);
        assert_eq!(shape.cookies, ["omnion_lang"]);
    }

    #[test]
    fn an_empty_cookie_header_yields_no_names() {
        let headers = headers_of(&[("cookie", "")]);
        let shape = request_shape("/", None, &headers);
        assert!(shape.cookies.is_empty());
    }

    #[test]
    fn a_request_with_no_cookies_presents_none() {
        let headers = headers_of(&[]);
        let shape = request_shape("/blog", None, &headers);
        assert!(shape.cookies.is_empty());
        assert_eq!(shape.language, None);
        assert_eq!(shape.query, None);
    }

    #[test]
    fn the_shape_can_be_matched_against_twice() {
        // The shape is built from a `HeaderMap` the router owns; matching must read it
        // without consuming or mutating it.
        let headers = headers_of(&[("cookie", "omnion_lang=tr")]);
        let shape = request_shape("/blog", None, &headers);
        assert_eq!(shape.language.as_deref(), Some("tr"));
        let rules = vec![
            omnion_cdn::CacheRule {
                name: "reads".into(),
                priority: 0,
                pattern: omnion_cdn::PathPattern::parse("/blog").expect("compiles"),
                methods: vec!["GET".into()],
                edge_ttl_seconds: 300,
                browser_ttl_seconds: 60,
                swr_seconds: 0,
                cache_key: CacheKey::default(),
                bypass: omnion_cdn::Bypass::default(),
                enabled: true,
            }
            .checked()
            .expect("the fixture is a valid rule"),
        ];
        assert!(matches!(
            omnion_cdn::decide(&rules, &shape),
            Decision::Cacheable { .. }
        ));
        assert_eq!(
            shape.language.as_deref(),
            Some("tr"),
            "matching did not consume it"
        );
    }
}
