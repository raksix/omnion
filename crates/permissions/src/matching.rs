//! Resource matching: does a binding's scope cover the target of a request?
//!
//! Resource-scoped bindings (docs/07-IAM.md §6) name their target as a glob — a path such as
//! `/blog/*` on a site, or a module key. The matcher here is the single implementation the
//! resolver, the members tab and the simulator share, so a verdict can never come from two
//! different readings of the same pattern.

/// `true` when `pattern` matches `value`.
///
/// `*` matches any run of characters (including `/`), `?` matches exactly one. Everything else
/// compares literally, and comparison is case-sensitive, because resource keys are.
///
/// An empty pattern matches only an empty value: a binding without a pattern is not a wildcard.
#[must_use]
pub fn glob_matches(pattern: &str, value: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let value: Vec<char> = value.chars().collect();

    // Classic greedy wildcard walk, backtracking on the last `*` only — linear in practice and
    // exact for the two wildcards the model supports.
    let (mut p, mut v) = (0_usize, 0_usize);
    let (mut star, mut checkpoint) = (None, 0_usize);

    while v < value.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some(p);
                checkpoint = v;
                p += 1;
            }
            Some('?') => {
                p += 1;
                v += 1;
            }
            Some(ch) if *ch == value[v] => {
                p += 1;
                v += 1;
            }
            _ => match star {
                Some(star_at) => {
                    // The last `*` eats one more character and we retry from just after it.
                    p = star_at + 1;
                    checkpoint += 1;
                    v = checkpoint;
                }
                None => return false,
            },
        }
    }

    pattern[p..].iter().all(|ch| *ch == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards_match_paths() {
        assert!(glob_matches("/blog/*", "/blog/hello"));
        assert!(glob_matches("/blog/*", "/blog/2026/hello-world"));
        assert!(!glob_matches("/blog/*", "/legal/terms"));
        assert!(
            !glob_matches("/blog/*", "/blog"),
            "the pattern needs a tail"
        );
        assert!(glob_matches("/blog", "/blog"));
        assert!(!glob_matches("/blog", "/blog/hello"));
    }

    #[test]
    fn question_mark_matches_one_character() {
        assert!(glob_matches("/v?/home", "/v1/home"));
        assert!(!glob_matches("/v?/home", "/v12/home"));
        assert!(!glob_matches("/v?/home", "/v/home"));
    }

    #[test]
    fn a_bare_star_matches_everything_but_empty_needs_empty() {
        assert!(glob_matches("*", "anything/at/all"));
        assert!(glob_matches("/*", "/"));
        assert!(!glob_matches("", "/blog"));
        assert!(glob_matches("", ""));
    }

    #[test]
    fn repeated_stars_and_backtracking_stay_correct() {
        assert!(glob_matches("/a/**/z", "/a/b/c/z"));
        assert!(glob_matches("/a**z", "/az"));
        assert!(glob_matches("*/blog/*", "/sites/main/blog/post-1"));
        assert!(!glob_matches("*/blog/*", "/sites/main/legal/post-1"));
    }

    #[test]
    fn matching_is_case_sensitive() {
        assert!(!glob_matches("/Blog/*", "/blog/x"));
        assert!(glob_matches("/Blog/*", "/Blog/x"));
    }
}
