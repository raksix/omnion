//! A cache rule and the policy it produces (docs/requests/REQ-011).

use serde::{Deserialize, Serialize};

use crate::matcher::{CacheKey, PathPattern, RequestShape};

/// One year, the cap both TTLs are validated against.
pub const MAX_TTL_SECONDS: i32 = 31_536_000;

/// The bounds on a rule name; the minimum is 1, which `char_length` already enforces
/// in the table, so only the maximum needs a constant here.
const NAME_MAX: usize = 64;

/// A cache rule as the panel stores it.
///
/// `pattern` and `cache_key` are validated on the way in ([`CacheRule::checked`]),
/// so a rule read back out of the database is always a rule that can be matched
/// and always has a compilable pattern — a `Result` in the struct would push that
/// problem onto every reader instead of onto the one place that writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheRule {
    /// Rule name, unique per site (case-insensitively).
    pub name: String,
    /// Lower number wins when two rules match the same request.
    pub priority: i32,
    /// Glob over the request path.
    pub pattern: PathPattern,
    /// Methods the rule applies to; a request whose method is absent is not matched.
    #[serde(default = "default_methods")]
    pub methods: Vec<String>,
    /// How long an edge may keep the response, in seconds. `0` means not cacheable.
    pub edge_ttl_seconds: i32,
    /// How long a browser may keep the response, in seconds.
    pub browser_ttl_seconds: i32,
    /// How long a stale response may still be served while it revalidates.
    pub swr_seconds: i32,
    /// Which request parts make up the cache key.
    #[serde(default)]
    pub cache_key: CacheKey,
    /// Conditions under which the rule is bypassed entirely.
    #[serde(default)]
    pub bypass: Bypass,
    /// A disabled rule stays in the table but never matches.
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn default_methods() -> Vec<String> {
    vec!["GET".into(), "HEAD".into()]
}

fn yes() -> bool {
    true
}

/// The bypass conditions of a rule.
///
/// A rule with any of these set is only cacheable for requests that carry none of
/// them: a logged-in visitor, a preview cookie or an internal query flag all mean
/// the response belongs to one person and must not be stored for another.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bypass {
    /// Cookie names that force a bypass when present.
    #[serde(default)]
    pub cookies: Vec<String>,
    /// Query parameter names that force a bypass when present.
    #[serde(default)]
    pub query_params: Vec<String>,
    /// Header names that force a bypass when present.
    #[serde(default)]
    pub headers: Vec<String>,
}

impl Bypass {
    /// Whether this request must skip the rule.
    ///
    /// Each condition is a "the request carries this, so the response belongs to
    /// one person" test. An authorization header is always treated as a bypass
    /// even when the rule does not name it: a rule that stored an authenticated
    /// response would serve it to the next anonymous visitor, so the one condition
    /// that is never optional is applied unconditionally.
    #[must_use]
    pub fn applies(&self, request: &RequestShape) -> bool {
        if request
            .headers
            .iter()
            .any(|name| name.eq_ignore_ascii_case("authorization"))
        {
            return true;
        }
        let query = request.query.as_deref().unwrap_or("");
        self.query_params.iter().any(|name| {
            query
                .split('&')
                .any(|pair| pair.split('=').next() == Some(name.as_str()))
        }) || request
            .cookies
            .iter()
            .any(|cookie| self.cookies.iter().any(|name| cookie == name))
    }
}

/// A rule that failed validation, with the field at fault.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuleError {
    /// The name is blank.
    #[error("a rule name is required")]
    NameEmpty,
    /// The name is longer than 64 characters.
    #[error("a rule name may be at most 64 characters")]
    NameTooLong,
    /// The name is not on a character boundary count.
    #[error("a rule name may not contain a control character")]
    NameNotPrintable,
    /// The pattern did not compile.
    #[error("{0}")]
    Pattern(#[from] crate::matcher::PatternError),
    /// A TTL is negative.
    #[error("{field} may not be negative")]
    NegativeTtl {
        /// Which of the two TTLs is at fault.
        field: &'static str,
    },
    /// A TTL is above the one-year cap.
    #[error("{field} may be at most {MAX_TTL_SECONDS} seconds (one year)")]
    TtlTooLarge {
        /// Which of the two TTLs is at fault.
        field: &'static str,
    },
    /// The method list is empty.
    #[error("a rule needs at least one method; use GET and HEAD for read-only caching")]
    NoMethods,
}

impl CacheRule {
    /// Validate a rule the way the form does, before it is stored.
    ///
    /// Every variant names the field, because the form puts the message under that
    /// input; a single generic "invalid rule" would leave the author guessing.
    pub fn checked(mut self) -> Result<Self, RuleError> {
        let trimmed = self.name.trim();
        if trimmed.is_empty() {
            return Err(RuleError::NameEmpty);
        }
        if trimmed.chars().count() > NAME_MAX {
            return Err(RuleError::NameTooLong);
        }
        if trimmed.chars().any(char::is_control) {
            return Err(RuleError::NameNotPrintable);
        }
        self.name = trimmed.to_string();

        // Recompiling is the validation: a pattern that does not compile never
        // reaches the table.
        self.pattern = PathPattern::parse(self.pattern.as_str())?;

        for (field, value) in [
            ("edge TTL", self.edge_ttl_seconds),
            ("browser TTL", self.browser_ttl_seconds),
        ] {
            if value < 0 {
                return Err(RuleError::NegativeTtl { field });
            }
            if value > MAX_TTL_SECONDS {
                return Err(RuleError::TtlTooLarge { field });
            }
        }
        if self.swr_seconds < 0 {
            return Err(RuleError::NegativeTtl {
                field: "stale-while-revalidate",
            });
        }
        if self.methods.is_empty() {
            return Err(RuleError::NoMethods);
        }
        self.methods = self
            .methods
            .iter()
            .map(|method| method.trim().to_ascii_uppercase())
            .filter(|method| !method.is_empty())
            .collect();
        if self.methods.is_empty() {
            return Err(RuleError::NoMethods);
        }
        Ok(self)
    }

    /// Whether this rule matches a request at all.
    #[must_use]
    pub fn matches(&self, request: &RequestShape) -> bool {
        self.enabled
            && self
                .methods
                .iter()
                .any(|method| method.eq_ignore_ascii_case(&request.method))
            && self.pattern.matches(request.path.as_str())
    }
}

/// The decision a request gets: either a cacheable policy or an explicit refusal.
///
/// The two are different values rather than a policy with `ttl = 0`, because "not
/// cacheable" and "cacheable for zero seconds" lead to the same header bytes but
/// not to the same operator experience — a refused request should be able to say
/// which rule refused it, so the panel can explain the miss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// A rule matched; the request may be cached for the durations it names.
    Cacheable {
        /// Name of the rule that decided, for the panel and the log.
        rule: String,
        /// How long an edge may keep the response.
        edge_ttl_seconds: i32,
        /// How long a browser may keep the response.
        browser_ttl_seconds: i32,
        /// How long a stale response may still be served.
        swr_seconds: i32,
        /// The cache key for this request under the rule's components.
        cache_key: String,
    },
    /// No rule matched, or the matched rule was bypassed.
    Private {
        /// Why it was refused: `no_rule` or the name of the bypassed rule.
        reason: &'static str,
    },
}

/// Decide what a request gets, given the ordered rule set.
///
/// `rules` must be in priority order (lowest number first); the caller loads them
/// that way so the same ordering drives the panel table and this decision.
#[must_use]
pub fn decide(rules: &[CacheRule], request: &RequestShape) -> Decision {
    for rule in rules {
        if !rule.matches(request) {
            continue;
        }
        if rule.bypass.applies(request) {
            return Decision::Private { reason: "bypassed" };
        }
        // A rule with no edge TTL is a deliberate "do not store this here"; the
        // browser TTL may still apply, so it is not the same as no rule at all.
        return Decision::Cacheable {
            rule: rule.name.clone(),
            edge_ttl_seconds: rule.edge_ttl_seconds,
            browser_ttl_seconds: rule.browser_ttl_seconds,
            swr_seconds: rule.swr_seconds,
            cache_key: rule.cache_key.derive(request),
        };
    }
    Decision::Private { reason: "no_rule" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(path: &str, method: &str) -> RequestShape {
        RequestShape::bare(path).with_method(method).with_host("site.test")
    }

    fn rule(name: &str, pattern: &str, edge: i32) -> CacheRule {
        CacheRule {
            name: name.into(),
            priority: 0,
            // An unparseable pattern is carried through as a bare literal: the
            // tests that check refusal must reach `checked()` with the pattern
            // intact, so the helper cannot insist on compiling it here.
            pattern: PathPattern::from_raw(pattern),
            methods: default_methods(),
            edge_ttl_seconds: edge,
            browser_ttl_seconds: 60,
            swr_seconds: 0,
            cache_key: CacheKey::default(),
            bypass: Bypass::default(),
            enabled: true,
        }
    }

    #[test]
    fn a_request_matching_no_rule_is_private() {
        let decision = decide(&[], &shape("/blog", "GET"));
        assert_eq!(decision, Decision::Private { reason: "no_rule" });
    }

    #[test]
    fn the_first_matching_rule_in_priority_order_decides() {
        let mut specific = rule("specific", "/blog/hello", 60);
        specific.priority = 1;
        let mut broad = rule("broad", "/blog/**", 3600);
        broad.priority = 2;
        let decision = decide(&[specific, broad], &shape("/blog/hello", "GET"));
        match decision {
            Decision::Cacheable {
                rule,
                edge_ttl_seconds,
                ..
            } => {
                assert_eq!(rule, "specific");
                assert_eq!(edge_ttl_seconds, 60);
            }
            other => panic!("expected the specific rule, got {other:?}"),
        }
    }

    #[test]
    fn a_disabled_rule_is_skipped_even_though_it_comes_first() {
        let mut disabled = rule("disabled", "/**", 3600);
        disabled.enabled = false;
        let enabled = rule("enabled", "/blog/**", 120);
        let decision = decide(&[disabled, enabled], &shape("/blog/hello", "GET"));
        match decision {
            Decision::Cacheable { rule, .. } => assert_eq!(rule, "enabled"),
            other => panic!("expected the enabled rule, got {other:?}"),
        }
    }

    #[test]
    fn a_method_outside_the_rule_is_not_matched_by_it() {
        let rules = vec![rule("reads", "/blog/**", 300)];
        let decision = decide(&rules, &shape("/blog/hello", "POST"));
        assert_eq!(decision, Decision::Private { reason: "no_rule" });
    }

    #[test]
    fn the_head_method_is_cacheable_by_default() {
        let rules = vec![rule("reads", "/blog/**", 300)];
        assert!(matches!(
            decide(&rules, &shape("/blog/hello", "HEAD")),
            Decision::Cacheable { .. }
        ));
    }

    #[test]
    fn an_empty_name_is_refused() {
        let error = rule("  ", "/blog", 60).checked().unwrap_err();
        assert_eq!(error, RuleError::NameEmpty);
    }

    #[test]
    fn a_name_longer_than_sixty_four_characters_is_refused() {
        let long = "x".repeat(NAME_MAX + 1);
        let error = rule(&long, "/blog", 60).checked().unwrap_err();
        assert_eq!(error, RuleError::NameTooLong);
    }

    #[test]
    fn a_name_of_exactly_sixty_four_characters_is_accepted() {
        let exact = "x".repeat(NAME_MAX);
        assert!(rule(&exact, "/blog", 60).checked().is_ok());
    }

    #[test]
    fn a_malformed_pattern_is_refused_with_the_pattern_message() {
        let error = rule("bad", "blog", 60).checked().unwrap_err();
        assert!(matches!(error, RuleError::Pattern(_)));
        assert!(error.to_string().contains("starts with /"));
    }

    #[test]
    fn a_ttl_above_the_one_year_cap_is_refused() {
        let error = rule("long", "/blog", MAX_TTL_SECONDS + 1)
            .checked()
            .unwrap_err();
        assert!(matches!(
            error,
            RuleError::TtlTooLarge { field: "edge TTL" }
        ));
    }

    #[test]
    fn a_browser_ttl_above_the_cap_names_the_browser_field() {
        let mut rule = rule("long", "/blog", 60);
        rule.browser_ttl_seconds = MAX_TTL_SECONDS + 1;
        let error = rule.checked().unwrap_err();
        assert!(matches!(
            error,
            RuleError::TtlTooLarge {
                field: "browser TTL"
            }
        ));
    }

    #[test]
    fn a_negative_ttl_is_refused() {
        let error = rule("negative", "/blog", -1).checked().unwrap_err();
        assert!(matches!(
            error,
            RuleError::NegativeTtl { field: "edge TTL" }
        ));
    }

    #[test]
    fn a_rule_with_no_methods_is_refused() {
        let mut rule = rule("no methods", "/blog", 60);
        rule.methods = vec![];
        assert_eq!(rule.checked().unwrap_err(), RuleError::NoMethods);
    }

    #[test]
    fn methods_are_normalised_to_upper_case() {
        let mut rule = rule("lower", "/blog", 60);
        rule.methods = vec!["get".into(), " head ".into()];
        let checked = rule.checked().expect("valid");
        assert_eq!(checked.methods, vec!["GET".to_string(), "HEAD".to_string()]);
        assert!(checked.matches(&shape("/blog", "get")));
    }
}
