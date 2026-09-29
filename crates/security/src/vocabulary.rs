//! The words of the security centre, as compile-time lists.
//!
//! Every list here is the **same list** the SQL check constraints, the panel's filter dropdowns
//! and the API's validators are built from. A state the database refuses but the panel offers
//! is a filter that silently returns nothing; a state the database accepts but the panel cannot
//! name is a finding an operator can never find. `0054_security_posture.sql` states the same
//! lists in its own comments (it cannot import Rust), and the test at the bottom of this file
//! names that file as the list it must agree with — so the two cannot drift without a red test.

/// A posture check's state.
///
/// The four states are not a severity scale, they are *what this platform can truthfully say*.
/// `Unknown` exists so that a check which cannot verify something has an honest answer: the
/// single rule this crate holds above every other is that **a check never reports `pass` on the
/// strength of its own absence of evidence.** A missing table, an unreadable setting or an
/// evaluation that failed is `Unknown`, never `Pass`.
pub const STATES: &[&str] = &["pass", "warn", "fail", "unknown"];

/// Where a finding came from.
pub const SOURCES: &[&str] = &["config", "dependency", "platform", "report"];

/// How bad a finding is, worst first.
pub const SEVERITIES: &[&str] = &["critical", "high", "medium", "low", "info"];

/// What has been done about a finding.
pub const FINDING_STATUSES: &[&str] = &["open", "acknowledged", "fixed", "ignored"];

/// The `state` a check reports before it has ever been evaluated.
///
/// Not a fifth state: a check that has never run *is* `unknown`, and the panel renders that
/// with the "Run checks" call to action next to it. The distinction between "never evaluated"
/// and "evaluated and could not tell" is carried by the null timestamp, not by a new colour.
pub const STATE_WHEN_UNEVALUATED: &str = "unknown";

/// Largest page any list read will return, whatever the caller asks for.
pub const MAX_PAGE: usize = 200;

/// Largest number of ids one bulk status action accepts.
pub const MAX_BULK_IDS: usize = 500;

/// Longest title a finding may carry (the SQL check constraint is the same number).
pub const MAX_TITLE: usize = 200;

/// Longest ignore reason the store accepts, and the shortest one that counts as given.
pub const MAX_IGNORE_REASON: usize = 1000;

/// Longest note an acknowledgement or a comment may carry.
pub const MAX_NOTE: usize = 4000;

/// `true` when `value` is a state the platform knows.
#[must_use]
pub fn is_state(value: &str) -> bool {
    STATES.contains(&value)
}

/// `true` when `value` is a finding source the platform knows.
#[must_use]
pub fn is_source(value: &str) -> bool {
    SOURCES.contains(&value)
}

/// `true` when `value` is a severity the platform knows.
#[must_use]
pub fn is_severity(value: &str) -> bool {
    SEVERITIES.contains(&value)
}

/// `true` when `value` is a finding status the platform knows.
#[must_use]
pub fn is_finding_status(value: &str) -> bool {
    FINDING_STATUSES.contains(&value)
}

/// Rank a severity for sorting, worst first. Unknown severities sort last, never first.
#[must_use]
pub fn severity_rank(value: &str) -> i32 {
    match value {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        "info" => 4,
        _ => 99,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lists_match_the_migration() {
        // The migration names these lists in its own comments; if one grows, that file must
        // grow with it. This is the test that makes the duplication bearable.
        let migration = include_str!("../../../database/migrations/0054_security_posture.sql");
        for state in STATES {
            assert!(
                migration.contains(&format!("'{state}'")),
                "state {state} is not in the migration's list"
            );
        }
        for source in SOURCES {
            assert!(
                migration.contains(&format!("'{source}'")),
                "source {source} is not in the migration's list"
            );
        }
        for severity in SEVERITIES {
            assert!(
                migration.contains(&format!("'{severity}'")),
                "severity {severity} is not in the migration's list"
            );
        }
        for status in FINDING_STATUSES {
            assert!(
                migration.contains(&format!("'{status}'")),
                "finding status {status} is not in the migration's list"
            );
        }
    }

    #[test]
    fn a_state_the_platform_does_not_know_is_not_a_state() {
        assert!(!is_state("ok"), "'ok' is not one of our words for it");
        assert!(!is_state(""), "an empty state is not a state");
        assert!(is_state("unknown"), "unknown is a state — it is the honest one");
    }

    #[test]
    fn severity_ranks_worst_first_and_unknown_last() {
        let ranked: Vec<&str> = ["info", "critical", "low", "high", "medium"]
            .into_iter()
            .collect();
        let mut sorted = ranked.clone();
        sorted.sort_by_key(|s| severity_rank(s));
        assert_eq!(sorted, vec!["critical", "high", "medium", "low", "info"]);
        assert_eq!(severity_rank("critical"), 0);
        assert_eq!(severity_rank("nonsense"), 99, "an unknown severity is never the worst");
    }

    #[test]
    fn an_unevaluated_check_is_unknown_not_pass() {
        assert_eq!(STATE_WHEN_UNEVALUATED, "unknown");
        assert!(!STATES.contains(&"pass-pending"));
    }
}
