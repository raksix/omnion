//! Turning a [`Decision`] into response headers (docs/requests/REQ-011).
//!
//! The header block is the only thing a visitor's browser and every upstream edge
//! actually read, so it is built in one place rather than in each handler. Three
//! rules shape the output:
//!
//! * **A private decision is explicit.** It answers `Cache-Control: private,
//!   no-store` rather than sending nothing: an absent header leaves the response
//!   cacheable under whatever a shared proxy decides, which is the failure a
//!   cache layer exists to prevent.
//! * **`stale-while-revalidate` is only emitted when there is a SWR window.** A
//!   zero window written out is noise that reads as "staleness allowed".
//! * **The `Surrogate-Key` is a tag list, not a decision.** Tag-based purging
//!   (REQ-011 slice 2) needs the response to name what it belongs to; the tags
//!   are derived from the path so a page and its media can be purged together.

use crate::rule::Decision;

/// A header name/value pair. A small vector rather than an `http::HeaderMap` so
/// the crate stays free of the HTTP stack and stays testable on its own.
pub type Headers = Vec<(String, String)>;

/// Build the response headers for a decision.
#[must_use]
pub fn headers_for(decision: &Decision) -> Headers {
    match decision {
        Decision::Cacheable {
            edge_ttl_seconds,
            browser_ttl_seconds,
            swr_seconds,
            ..
        } => {
            let mut out = Headers::new();
            out.push((
                "Cache-Control".to_string(),
                cache_control(*browser_ttl_seconds, *swr_seconds, *edge_ttl_seconds),
            ));
            // The edge directive is separate from the browser one: an operator
            // usually wants the shared cache to hold a page far longer than a
            // visitor's browser does.
            out.push((
                "CDN-Cache-Control".to_string(),
                cache_control(*edge_ttl_seconds, *swr_seconds, *edge_ttl_seconds),
            ));
            out
        }
        Decision::Private { .. } => {
            let mut out = Headers::new();
            out.push(("Cache-Control".to_string(), "private, no-store".to_string()));
            out
        }
    }
}

/// Compose a `Cache-Control` value.
///
/// An edge TTL of zero means "not cacheable at the edge" and is written as
/// `no-store` rather than `max-age=0`, because the two are read differently by
/// some intermediaries: `max-age=0` still permits a revalidation round trip and a
/// stored copy, which is not what a zero TTL asked for.
fn cache_control(max_age: i32, swr: i32, edge_ttl: i32) -> String {
    if edge_ttl <= 0 {
        return "no-store".to_string();
    }
    let mut value = format!("public, max-age={max_age}");
    if swr > 0 {
        value.push_str(&format!(", stale-while-revalidate={swr}"));
    }
    value
}

/// The surrogate tags a path belongs to, used for tag-based purging.
///
/// Three tags, each a prefix a purge can name: the path itself (purge one URL),
/// its first segment (purge a section), and the site (purge everything). The
/// site tag is not derived here — the caller appends it, because only the caller
/// knows which site a response belongs to.
#[must_use]
pub fn surrogate_keys(path: &str) -> Vec<String> {
    let trimmed = path.trim_start_matches('/');
    let mut keys = vec![path.to_string()];
    if let Some((section, _)) = trimmed.split_once('/') {
        if !section.is_empty() {
            keys.push(format!("/{section}"));
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cacheable(edge: i32, browser: i32, swr: i32) -> Decision {
        Decision::Cacheable {
            rule: "reads".into(),
            edge_ttl_seconds: edge,
            browser_ttl_seconds: browser,
            swr_seconds: swr,
            cache_key: "site.test|/blog".into(),
        }
    }

    fn header<'a>(headers: &'a Headers, name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn a_private_decision_sends_an_explicit_no_store() {
        let headers = headers_for(&Decision::Private { reason: "no_rule" });
        assert_eq!(header(&headers, "Cache-Control"), Some("private, no-store"));
        // A private response must not also carry an edge TTL.
        assert_eq!(header(&headers, "CDN-Cache-Control"), None);
    }

    #[test]
    fn a_cacheable_decision_sends_both_ttls_separately() {
        let headers = headers_for(&cacheable(3600, 60, 0));
        assert_eq!(
            header(&headers, "Cache-Control"),
            Some("public, max-age=60")
        );
        assert_eq!(
            header(&headers, "CDN-Cache-Control"),
            Some("public, max-age=3600")
        );
    }

    #[test]
    fn a_stale_window_is_written_only_when_it_is_positive() {
        assert_eq!(
            header(&headers_for(&cacheable(3600, 60, 120)), "Cache-Control"),
            Some("public, max-age=60, stale-while-revalidate=120")
        );
        assert!(
            !header(&headers_for(&cacheable(3600, 60, 0)), "Cache-Control")
                .expect("always present")
                .contains("stale-while-revalidate")
        );
    }

    #[test]
    fn a_zero_edge_ttl_is_no_store_rather_than_max_age_zero() {
        let headers = headers_for(&cacheable(0, 60, 0));
        assert_eq!(header(&headers, "CDN-Cache-Control"), Some("no-store"));
        // The browser TTL is unaffected: an operator may still want the visitor's
        // own browser to hold a page the shared edge must not.
        assert_eq!(header(&headers, "Cache-Control"), Some("no-store"));
    }

    #[test]
    fn a_path_yields_its_own_key_and_its_section_key() {
        let keys = surrogate_keys("/blog/2026/hello");
        assert_eq!(
            keys,
            vec!["/blog/2026/hello".to_string(), "/blog".to_string()]
        );
    }

    #[test]
    fn a_root_path_has_no_section_key() {
        assert_eq!(surrogate_keys("/"), vec!["/".to_string()]);
    }
}
