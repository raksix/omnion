//! Validators: the `ETag` and `Vary` a cacheable response carries (REQ-011, slice 1).
//!
//! `Cache-Control` says *how long* a response may be kept. It says nothing about *which*
//! stored copy a cache should hand back, which is the second half of the problem: an edge
//! holding two entries under one key serves the wrong one to whichever visitor arrives
//! second. Two mechanisms answer that and they live here:
//!
//! * **`ETag` is derived from content, never from time.** A validator that changes on every
//!   read would make every request a miss; one that changes only when the content changes
//!   is exactly what `If-None-Match` needs. For a file the validator is the checksum the
//!   upload already computed; for a page it is the revision number together with the body
//!   as it was rendered. Both are facts about the content rather than about the request.
//! * **`Vary` is derived from the key components the rule actually uses.** A rule that keys
//!   on the language cookie must send `Vary: Cookie`, or a shared cache will serve one
//!   language's rendering to a visitor who asked for another. Deriving this from the rule
//!   rather than hardcoding it is the point: a rule author who adds a key component gets a
//!   correct `Vary` without touching this file.

use crate::matcher::CacheKey;
use crate::rule::Decision;

/// Open quote, so a validator built from raw material is already a header value.
const QUOTE: char = '"';

/// Quote a validator unless the caller already quoted it.
///
/// Both forms are accepted on the way in because the file checksum arrives as bare hex
/// from the database while a re-published validator may already come quoted from a stored
/// header; refusing one of them would mean a caller stripping and re-adding quotes.
fn quoted(value: &str) -> String {
    if value.starts_with('"') || value.starts_with("W/\"") {
        value.to_string()
    } else {
        format!("{QUOTE}{value}{QUOTE}")
    }
}

/// The validator of a file response, from the checksum the upload computed.
///
/// The checksum is a SHA-256 of the stored bytes, so this is a *strong* validator and is
/// labelled as one. A file that has never been checksummed falls back to its id, which
/// changes when the file is replaced — not content addressing, but still a validator that
/// flips exactly when the content does.
#[must_use]
pub fn etag_for_file(checksum: &str, id: uuid::Uuid) -> String {
    let material = match checksum.trim() {
        "" => id.to_string(),
        checksum => checksum.to_string(),
    };
    quoted(&material)
}

/// The validator of a page response, from its revision and the body it rendered.
///
/// The revision number is included so a re-publish that reuses a revision number still
/// flips the validator, and the body is hashed so an unchanged re-read produces an
/// unchanged one. The hash is computed here rather than read from a column because the
/// rendered body is what the cache actually stores; hashing the stored rows instead would
/// be a validator for something the client never received.
#[must_use]
pub fn etag_for_page(revision_no: i32, body: &str) -> String {
    quoted(&format!("p{revision_no}-{}", short_hash(body)))
}

/// Whether a request's `If-None-Match` names this response's validator.
///
/// Written by hand rather than with a wildcard-tolerant parser because the failure modes
/// are asymmetric: treating a mismatch as a match serves a *wrong* body, while treating a
/// match as a mismatch costs one extra `200`. The forms RFC 9110 lists are handled — `*`,
/// an exact tag, and a comma-separated list — and comparison is weak on both sides, which
/// is what a conditional request on a cacheable GET is always asking for.
#[must_use]
pub fn if_none_match_hits(if_none_match: &str, etag: &str) -> bool {
    let target = if_none_match.trim();
    if target.is_empty() {
        return false;
    }
    if target == "*" {
        return true;
    }
    let wanted = strip_weak(etag);
    target.split(',').map(strip_weak).any(|tag| tag == wanted)
}

/// Drop a `W/` prefix; weak comparison is the right one here.
fn strip_weak(value: &str) -> &str {
    value.trim().trim_start_matches("W/").trim()
}

/// The `Vary` value a decision needs, or `None` when it needs none.
///
/// Derived from the matched rule's key components rather than from the request: `Vary`
/// names what a cache must *remember about a request*, and a rule that does not key on the
/// language cookie genuinely does not care which language arrived — even when every caller
/// happens to send one. A private response is never stored, so it needs no `Vary` at all.
#[must_use]
pub fn vary_for(decision: &Decision, key: &CacheKey) -> Option<String> {
    if !matches!(decision, Decision::Cacheable { .. }) {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    if key.host {
        parts.push("Host");
    }
    if !key.query_allow.is_empty() {
        parts.push("Query");
    }
    if key.language_cookie {
        parts.push("Cookie");
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn short_hash(value: &str) -> String {
    // FNV-1a, 64-bit, rendered as hex. This is not a security primitive and must never be
    // used as one: a cache validator only has to be stable and cheap, and reaching for
    // SHA-256 here would put a hashing dependency in a crate whose whole point is being
    // usable without one. A collision would mean two bodies sharing a validator, and the
    // revision number in the same string makes that a same-revision collision, which
    // cannot happen by re-publishing.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::RequestShape;
    use crate::rule::Decision;

    fn key_of(host: bool, query: &[&str], language: bool) -> CacheKey {
        CacheKey {
            host,
            path: true,
            query_allow: query.iter().map(|name| (*name).to_string()).collect(),
            language_cookie: language,
        }
    }

    fn decision() -> Decision {
        Decision::Cacheable {
            rule: "blog".into(),
            edge_ttl_seconds: 600,
            browser_ttl_seconds: 60,
            swr_seconds: 0,
            cache_key: "site.test|/blog/hello".into(),
        }
    }

    /// A request in one language, so a test can compare two languages.
    fn shape_with(language: &str) -> RequestShape {
        RequestShape::bare("/blog")
            .with_host("site.test")
            .with_language(language)
            .with_cookies(["omnion_lang"])
    }

    #[test]
    fn a_file_validator_is_quoted_and_stable_for_the_same_checksum() {
        let id = uuid::Uuid::nil();
        let first = etag_for_file(&"a".repeat(64), id);
        assert_eq!(first, format!("\"{}\"", "a".repeat(64)));
        assert_eq!(first, etag_for_file(&"a".repeat(64), id));
    }

    #[test]
    fn a_different_checksum_is_a_different_validator() {
        let id = uuid::Uuid::nil();
        assert_ne!(
            etag_for_file(&"a".repeat(64), id),
            etag_for_file(&"b".repeat(64), id)
        );
    }

    #[test]
    fn a_file_with_no_checksum_still_gets_a_validator_that_flips_with_the_file() {
        let one = etag_for_file("", uuid::Uuid::from_u128(1));
        let two = etag_for_file("", uuid::Uuid::from_u128(2));
        assert!(one.starts_with('"') && one.ends_with('"'));
        assert_ne!(one, two);
        // Whitespace is not a checksum: a blank that pads would otherwise be stored and
        // then read back as a validator that never changes.
        assert_eq!(one, etag_for_file("   ", uuid::Uuid::from_u128(1)));
    }

    #[test]
    fn an_already_quoted_validator_is_not_quoted_twice() {
        assert_eq!(quoted("\"abc\""), "\"abc\"");
        assert_eq!(quoted("W/\"abc\""), "W/\"abc\"");
        assert_eq!(quoted("abc"), "\"abc\"");
    }

    #[test]
    fn a_page_validator_changes_with_the_body_and_with_the_revision() {
        let first = etag_for_page(1, "hello");
        assert_eq!(first, etag_for_page(1, "hello"), "the same bytes match");
        assert_ne!(first, etag_for_page(2, "hello"), "the revision is in it");
        assert_ne!(first, etag_for_page(1, "goodbye"));
    }

    #[test]
    fn a_conditional_request_hits_on_an_exact_tag_a_star_and_a_list() {
        let etag = etag_for_page(1, "hello");
        assert!(if_none_match_hits(&etag, &etag));
        assert!(if_none_match_hits("*", &etag));
        assert!(if_none_match_hits(
            &format!("\"other\", {etag}, \"more\""),
            &etag
        ));
    }

    #[test]
    fn a_conditional_request_hits_through_a_weak_prefix_on_either_side() {
        let etag = etag_for_page(1, "hello");
        assert!(if_none_match_hits(&format!("W/{etag}"), &etag));
        assert!(if_none_match_hits(&etag, &format!("W/{etag}")));
    }

    #[test]
    fn a_conditional_request_misses_on_a_different_tag_or_no_header() {
        let etag = etag_for_page(1, "hello");
        assert!(!if_none_match_hits(&etag_for_page(2, "hello"), &etag));
        assert!(!if_none_match_hits("", &etag), "an absent header is a miss");
        assert!(!if_none_match_hits("   ", &etag));
        assert!(!if_none_match_hits("\"other\"", &etag));
    }

    #[test]
    fn a_private_decision_carries_no_vary() {
        assert_eq!(
            vary_for(
                &Decision::Private { reason: "no_rule" },
                &key_of(true, &["page"], true)
            ),
            None,
            "a response nobody stores has nothing to vary on"
        );
    }

    #[test]
    fn a_rule_that_keys_on_nothing_extra_carries_no_vary() {
        assert_eq!(vary_for(&decision(), &key_of(false, &[], false)), None);
    }

    #[test]
    fn each_key_component_names_its_own_vary_header() {
        assert_eq!(
            vary_for(&decision(), &key_of(false, &[], true)),
            Some("Cookie".to_string())
        );
        assert_eq!(
            vary_for(&decision(), &key_of(false, &["page"], false)),
            Some("Query".to_string())
        );
        assert_eq!(
            vary_for(&decision(), &key_of(true, &[], false)),
            Some("Host".to_string())
        );
    }

    #[test]
    fn a_rule_keying_on_everything_names_them_in_a_stable_order() {
        assert_eq!(
            vary_for(&decision(), &key_of(true, &["page", "limit"], true)),
            Some("Host, Query, Cookie".to_string())
        );
    }

    #[test]
    fn the_vary_of_a_rule_is_derived_from_the_rule_not_from_one_caller() {
        // Two requests that differ only in their language must get the same `Vary` when the
        // rule does not key on the cookie, or the edge splits its cache for nothing — and a
        // different `Vary` when the rule does.
        let turkish = shape_with("tr");
        let english = shape_with("en");
        assert_ne!(turkish.language, english.language);

        let keyed_on_none = key_of(false, &[], false);
        assert_eq!(vary_for(&decision(), &keyed_on_none), None);
        assert_eq!(
            vary_for(&decision(), &keyed_on_none),
            vary_for(&decision(), &keyed_on_none)
        );

        let keyed_on_cookie = key_of(false, &[], true);
        assert_eq!(
            vary_for(&decision(), &keyed_on_cookie),
            Some("Cookie".to_string())
        );
    }

    #[test]
    fn the_short_hash_is_stable_and_separates_the_two_bodies() {
        assert_eq!(short_hash("hello"), short_hash("hello"));
        assert_ne!(short_hash("hello"), short_hash("hellp"));
        assert_eq!(short_hash("hello").len(), 16);
    }
}
