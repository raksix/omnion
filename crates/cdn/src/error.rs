//! The error type of the CDN crate (REQ-011).

use uuid::Uuid;

/// What can go wrong in the CDN layer.
///
/// Split by *who can act on it*: a [`CdnError::Rule`] carries a message an operator
/// reads in a form field, a [`CdnError::Pattern`] names a stored pattern that no longer
/// compiles, and a [`CdnError::Store`] is the database. Collapsing them into one string
/// is what turns "your rule is invalid" into a generic 500.
#[derive(Debug, thiserror::Error)]
pub enum CdnError {
    /// A rule failed validation. The message is written for the form.
    #[error(transparent)]
    Rule(#[from] crate::rule::RuleError),
    /// A stored pattern no longer compiles.
    #[error(transparent)]
    Pattern(#[from] crate::matcher::PatternError),
    /// A name is already taken for this site.
    #[error("a cache rule called {name:?} already exists for this site")]
    DuplicateName {
        /// The name that collided.
        name: String,
    },
    /// The rule does not exist.
    #[error("no cache rule with that id exists for this site")]
    RuleNotFound {
        /// The id that was asked for.
        id: Uuid,
    },
    /// A reorder did not name exactly the site's rules.
    #[error(
        "a reorder must name all {expected} rules of this site exactly once; {given} were named"
    )]
    IncompleteReorder {
        /// How many rules the site actually has.
        expected: usize,
        /// How many ids the caller sent.
        given: usize,
    },
    /// The database refused or was unreachable.
    #[error("cdn storage error: {0}")]
    Store(#[from] sqlx::Error),
}

impl CdnError {
    /// A stable code for the API layer to branch on.
    ///
    /// The store error keeps its own code rather than being flattened to
    /// `internal_error`: an operator reading a log needs to tell "your name is taken"
    /// apart from "the database is down".
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            CdnError::Rule(error) => match error {
                crate::rule::RuleError::NameEmpty
                | crate::rule::RuleError::NameTooLong
                | crate::rule::RuleError::NameNotPrintable
                | crate::rule::RuleError::NoMethods => "invalid_cache_rule",
                crate::rule::RuleError::Pattern(_) => "invalid_path_pattern",
                crate::rule::RuleError::NegativeTtl { .. } => "invalid_ttl",
                crate::rule::RuleError::TtlTooLarge { .. } => "invalid_ttl",
            },
            CdnError::Pattern(_) => "invalid_path_pattern",
            CdnError::DuplicateName { .. } => "duplicate_rule_name",
            CdnError::RuleNotFound { .. } => "cache_rule_not_found",
            CdnError::IncompleteReorder { .. } => "incomplete_reorder",
            CdnError::Store(error) => match error {
                sqlx::Error::RowNotFound => "cache_rule_not_found",
                _ => "cdn_storage_error",
            },
        }
    }

    /// Whether this error is the caller's fault, and so a `4xx` rather than a `500`.
    #[must_use]
    pub fn is_client_error(&self) -> bool {
        !matches!(
            self,
            CdnError::Store(sqlx::Error::Io(_)) | CdnError::Store(sqlx::Error::PoolTimedOut)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::PatternError;
    use crate::rule::RuleError;

    #[test]
    fn a_field_problem_gets_a_400_shaped_code() {
        let error = CdnError::Rule(RuleError::NameEmpty);
        assert_eq!(error.code(), "invalid_cache_rule");
        assert!(error.is_client_error());
    }

    #[test]
    fn a_pattern_problem_names_the_pattern_field() {
        let error = CdnError::Pattern(PatternError::NotAbsolute);
        assert_eq!(error.code(), "invalid_path_pattern");
    }

    #[test]
    fn a_ttl_problem_has_its_own_code_rather_than_a_generic_one() {
        assert_eq!(
            CdnError::Rule(RuleError::TtlTooLarge { field: "edge TTL" }).code(),
            "invalid_ttl"
        );
    }

    #[test]
    fn a_name_collision_is_distinguishable_from_a_bad_name() {
        let error = CdnError::DuplicateName {
            name: "blog".into(),
        };
        assert_eq!(error.code(), "duplicate_rule_name");
    }

    #[test]
    fn an_unreachable_database_is_not_the_callers_fault() {
        let error = CdnError::Store(sqlx::Error::PoolTimedOut);
        assert!(!error.is_client_error());
    }
}
