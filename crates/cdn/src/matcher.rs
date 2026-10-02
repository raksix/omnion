//! Cache-rule matching for the CDN/edge layer (docs/requests/REQ-011).
//!
//! A cache rule says *which requests are cacheable and for how long*. Matching a
//! request is therefore a pure function of the request and the ordered rule set —
//! no database, no I/O — which is what makes it testable and what lets the
//! header middleware and the purge planner agree on what a URL "is".
//!
//! Two decisions live here and are deliberately not the caller's:
//!
//! * **First match wins.** Rules are ordered by priority and the first enabled rule
//!   whose pattern and methods match decides the policy. Evaluating every rule and
//!   merging the results would let a broad rule silently override a specific one
//!   that was written above it; "the topmost rule decides" is the contract an
//!   operator can reason about when a cache behaves unexpectedly.
//! * **No rule means private.** A request that matches nothing is never cacheable.
//!   Defaulting to a positive TTL would make an unconfigured installation publicly
//!   cacheable, and the one thing a cache layer must never do is leak a response
//!   nobody decided to share.

use serde::{Deserialize, Serialize};
use std::fmt;

/// The wildcard that stays inside one path segment.
const STAR: char = '*';

/// A path pattern, compiled.
///
/// The type exists so a stored rule is always a pattern that can be matched:
/// construction goes through [`PathPattern::parse`], which refuses the two shapes
/// that cannot be matched unambiguously. A rule that exists but never fires is
/// worse than a rule the form refuses to save.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PathPattern(String);

impl PathPattern {
    /// Compile a pattern, or say what is wrong with it.
    ///
    /// An empty pattern, a pattern that does not start at the site root, and a
    /// run of three or more asterisks are all refused: the first two because
    /// there is nothing sensible to match, the third because guessing which
    /// wildcard was meant is how a cache rule ends up surprising its author.
    pub fn parse(raw: &str) -> Result<Self, PatternError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(PatternError::Empty);
        }
        if !trimmed.starts_with('/') {
            return Err(PatternError::NotAbsolute);
        }
        let mut run = 0usize;
        for character in trimmed.chars() {
            if character == STAR {
                run += 1;
                if run > 2 {
                    return Err(PatternError::MalformedWildcard);
                }
            } else {
                run = 0;
            }
        }
        Ok(Self(trimmed.to_string()))
    }

    /// The pattern as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Wrap an already-validated pattern, for test fixtures.
    ///
    /// Production code reaches a `PathPattern` either by `parse` (anything an author
    /// types) or by serde (a row read from the database, via the transparent derive).
    /// This constructor exists so a test can build a rule whose pattern is *meant* to be
    /// invalid and still hand it to `CacheRule::checked`.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn from_raw(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// Whether this pattern matches a request path.
    ///
    /// Recursive backtracking over characters, where `**` may consume any number
    /// of characters (including none) and `*` may consume any number that
    /// contains no `/`. The search is bounded by the pattern length, so a long
    /// path cannot turn this into a hang — but the pattern is operator input,
    /// which is why it is length-capped at compile time by the rule validation.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        let pattern: Vec<char> = self.0.chars().collect();
        let target: Vec<char> = path.chars().collect();
        match_at(&pattern, 0, &target, 0)
    }
}

/// Match `pattern` against `target` starting at both offsets.
fn match_at(pattern: &[char], p: usize, target: &[char], s: usize) -> bool {
    if p == pattern.len() {
        return s == target.len();
    }
    if pattern[p] == STAR {
        // `**` crosses segment boundaries, `*` does not. Consume one character at
        // a time and try the rest of the pattern from each position.
        let crosses = pattern.get(p + 1) == Some(&STAR);
        let mut cursor = s;
        loop {
            if match_at(pattern, p + if crosses { 2 } else { 1 }, target, cursor) {
                return true;
            }
            let Some(next) = target.get(cursor) else {
                return false;
            };
            if !crosses && *next == '/' {
                return false;
            }
            cursor += 1;
        }
    }
    match target.get(s) {
        Some(character) if *character == pattern[p] => match_at(pattern, p + 1, target, s + 1),
        _ => false,
    }
}

impl fmt::Display for PathPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for PathPattern {
    type Err = PatternError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Why a pattern was refused. Each variant names the field problem so the form
/// can put a message under the input instead of a generic failure banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PatternError {
    /// The pattern is blank.
    #[error("a path pattern is required")]
    Empty,
    /// The pattern does not start at the site root.
    #[error("a path pattern starts with / — it is matched against the whole path")]
    NotAbsolute,
    /// A run of three or more `*`.
    #[error("use * for one segment and ** for many; three asterisks is not a wildcard")]
    MalformedWildcard,
}

// ---------------------------------------------------------------------------------------------
// Cache key
// ---------------------------------------------------------------------------------------------

/// Which parts of a request make up its cache key.
///
/// A cache that keys on more than it stores serves one visitor's response to
/// another. The components are therefore an allow-list: anything not named is
/// *not* part of the key, and the parts that are named are what gets hashed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheKey {
    /// Include the request host, so two domains on one edge do not share entries.
    #[serde(default)]
    pub host: bool,
    /// Include the path. Always true in practice; named so a stored rule reads
    /// the same way the form does.
    #[serde(default = "path_defaults_on")]
    pub path: bool,
    /// Query parameters that take part in the key, in the order the rule lists.
    #[serde(default)]
    pub query_allow: Vec<String>,
    /// Include the language cookie, so `/` in two languages are two entries.
    #[serde(default)]
    pub language_cookie: bool,
}

const fn path_defaults_on() -> bool {
    true
}

/// What a request looks like to the matcher: the parts of an HTTP request a cache
/// policy is allowed to depend on.
///
/// Deliberately not `http::Request`: this type is built from a request at the
/// edge and from literals in a test, and the second is the point.
///
/// The scalar fields borrow — they are slices of a URI, which outlives the match —
/// but the two name *lists* are owned. They are the only part a caller has to
/// allocate, and a borrowed list would have to be built by the caller and kept
/// alive by a struct this one cannot name, which is the self-referential shape a
/// handler cannot satisfy: it parses a `HeaderMap` into a local, and the local dies
/// at the `await` the decision needs. Owning them costs two small allocations per
/// public request and removes a lifetime that could not be satisfied.
#[derive(Debug, Clone)]
pub struct RequestShape {
    /// Request path, without the query string.
    pub path: String,
    /// Request host, if host-keyed entries are wanted.
    pub host: Option<String>,
    /// Raw query string without the leading `?`.
    pub query: Option<String>,
    /// Value of the language cookie, if present.
    pub language: Option<String>,
    /// Names of the cookies the request carried. Values are deliberately absent: a
    /// cache key built from a cookie value is a cache key built from a secret.
    pub cookies: Vec<String>,
    /// Names of the request headers that were present.
    pub headers: Vec<String>,
    /// Request method, upper-case.
    pub method: String,
}

impl RequestShape {
    /// A request with nothing but a path and `GET`.
    ///
    /// The starting point every caller builds on: a rule that keys on nothing but the path
    /// is the common case, and a test that wants to prove a decision does not care about
    /// cookies or headers should not have to name empty lists to say so.
    #[must_use]
    pub fn bare(path: &str) -> Self {
        RequestShape {
            path: path.to_string(),
            host: None,
            query: None,
            language: None,
            cookies: Vec::new(),
            headers: Vec::new(),
            method: "GET".to_string(),
        }
    }

    /// Set the host.
    #[must_use]
    pub fn with_host(mut self, host: &str) -> Self {
        self.host = Some(host.to_string());
        self
    }

    /// Set the query string, without the leading `?`.
    ///
    /// An empty value sets *nothing* rather than `Some("")`. The two key identically
    /// everywhere a rule can look, but they do not read identically in a log line, and a
    /// shape that says "this request had a query string" when it did not is the kind of
    /// small lie that gets copied into a decision three layers down.
    #[must_use]
    pub fn with_query(mut self, query: &str) -> Self {
        if !query.is_empty() {
            self.query = Some(query.to_string());
        }
        self
    }

    /// Set the language cookie value. An empty value sets nothing, for the same reason
    /// [`RequestShape::with_query`] does.
    #[must_use]
    pub fn with_language(mut self, language: &str) -> Self {
        if !language.is_empty() {
            self.language = Some(language.to_string());
        }
        self
    }

    /// Set the request method.
    #[must_use]
    pub fn with_method(mut self, method: &str) -> Self {
        self.method = method.to_string();
        self
    }

    /// Set the cookie names the request carried.
    ///
    /// Takes names, not `name=value` pairs, because that is all a rule may match on: the
    /// value is what a cache must never key on.
    #[must_use]
    pub fn with_cookies<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.cookies = names.into_iter().map(Into::into).collect();
        self
    }

    /// Set the header names the request carried.
    #[must_use]
    pub fn with_headers<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.headers = names.into_iter().map(Into::into).collect();
        self
    }
}

impl CacheKey {
    /// Derive the cache key for a request under this policy.
    ///
    /// The result is a string a cache can index on. Components are joined with a
    /// separator that cannot appear in a host, and the query parameters are
    /// emitted as `name=value` pairs rather than concatenated values, so two
    /// different requests can never collide by running their parts together.
    #[must_use]
    pub fn derive(&self, request: &RequestShape) -> String {
        let mut key = String::with_capacity(128);
        if self.host {
            key.push_str(request.host.as_deref().unwrap_or(""));
        }
        key.push('|');
        if self.path {
            key.push_str(request.path.as_str());
        }
        // Only allow-listed parameters take part, and they are emitted in the
        // order the rule lists them: a rule that names `page` before `limit`
        // keys identically regardless of the order the client sent them in.
        if !self.query_allow.is_empty() {
            let query = request.query.as_deref().unwrap_or("");
            for name in &self.query_allow {
                key.push('|');
                key.push_str(name);
                key.push('=');
                if let Some(value) = query_param(query, name) {
                    key.push_str(value);
                }
            }
        }
        if self.language_cookie {
            key.push_str("|lang=");
            key.push_str(request.language.as_deref().unwrap_or(""));
        }
        key
    }
}

/// Read one query parameter out of a raw query string.
///
/// A parameter matches on its whole name only: `?page=2` answers for `page` and
/// not for `page_size`, which is the difference between a correct key and a
/// collision between two unrelated filters.
fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (key == name).then_some(value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(path: &str) -> RequestShape {
        RequestShape::bare(path)
    }

    fn keyed(query_allow: Vec<&str>, language_cookie: bool) -> CacheKey {
        CacheKey {
            host: false,
            path: true,
            query_allow: query_allow.into_iter().map(str::to_string).collect(),
            language_cookie,
        }
    }

    #[test]
    fn a_literal_pattern_matches_only_its_own_path() {
        let pattern = PathPattern::parse("/blog").expect("compiles");
        assert!(pattern.matches("/blog"));
        assert!(!pattern.matches("/blogging"));
        assert!(!pattern.matches("/blog/2026"));
    }

    #[test]
    fn a_single_star_stays_inside_one_segment() {
        let pattern = PathPattern::parse("/blog/*").expect("compiles");
        assert!(pattern.matches("/blog/hello"));
        assert!(!pattern.matches("/blog/2026/hello"));
    }

    #[test]
    fn a_double_star_crosses_segments() {
        let pattern = PathPattern::parse("/blog/**").expect("compiles");
        assert!(pattern.matches("/blog/hello"));
        assert!(pattern.matches("/blog/2026/hello"));
        assert!(pattern.matches("/blog/2026/hello/world"));
    }

    #[test]
    fn a_catch_all_pattern_matches_every_path() {
        let pattern = PathPattern::parse("/**").expect("compiles");
        assert!(pattern.matches("/"));
        assert!(pattern.matches("/blog"));
        assert!(pattern.matches("/blog/2026/hello"));
    }

    #[test]
    fn a_partial_segment_pattern_matches_a_suffix() {
        let pattern = PathPattern::parse("/blog/hell*").expect("compiles");
        assert!(pattern.matches("/blog/hello"));
        assert!(!pattern.matches("/blog/goodbye"));
    }

    #[test]
    fn a_pattern_that_is_not_absolute_is_refused() {
        assert_eq!(PathPattern::parse("blog"), Err(PatternError::NotAbsolute));
        assert_eq!(PathPattern::parse("  "), Err(PatternError::Empty));
    }

    #[test]
    fn a_three_asterisk_run_is_refused_rather_than_guessed() {
        assert_eq!(
            PathPattern::parse("/blog/***"),
            Err(PatternError::MalformedWildcard)
        );
    }

    #[test]
    fn the_cache_key_ignores_query_parameters_that_are_not_allow_listed() {
        let key = keyed(vec!["page"], false);
        let first = shape("/search").with_query("page=2&limit=10");
        let second = shape("/search").with_query("page=2&limit=99");
        assert_eq!(key.derive(&first), key.derive(&second));
    }

    #[test]
    fn the_cache_key_separates_two_different_allow_listed_values() {
        let key = keyed(vec!["page"], false);
        let first = shape("/search").with_query("page=2");
        let second = shape("/search").with_query("page=3");
        assert_ne!(key.derive(&first), key.derive(&second));
    }

    #[test]
    fn the_order_the_rule_lists_parameters_does_not_depend_on_the_client() {
        let key = keyed(vec!["page", "limit"], false);
        let first = shape("/search").with_query("page=2&limit=10");
        let second = shape("/search").with_query("limit=10&page=2");
        assert_eq!(key.derive(&first), key.derive(&second));
    }

    #[test]
    fn a_query_parameter_matches_on_its_whole_name() {
        assert_eq!(query_param("page_size=10&page=2", "page"), Some("2"));
        assert_eq!(query_param("page_size=10", "page"), None);
    }

    #[test]
    fn the_language_cookie_takes_part_in_the_key_only_when_asked_for() {
        let request = shape("/").with_language("de");
        let without = CacheKey::default();
        assert_ne!(
            without.derive(&request),
            keyed(vec![], true).derive(&request)
        );
    }

    #[test]
    fn the_host_separates_two_domains_sharing_one_edge() {
        let request = shape("/blog").with_host("a.test");
        let keyed = CacheKey {
            host: true,
            ..CacheKey::default()
        };
        let other = shape("/blog").with_host("b.test");
        assert_ne!(keyed.derive(&request), keyed.derive(&other));
    }
}
