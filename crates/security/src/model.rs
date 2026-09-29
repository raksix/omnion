//! The posture check result, the finding and the query shapes the store accepts.

use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{Result, SecurityError};
use crate::vocabulary::{
    FINDING_STATUSES, MAX_IGNORE_REASON, MAX_NOTE, MAX_TITLE, SEVERITIES, is_finding_status,
    is_severity, is_source,
};

/// One posture check's most recent result.
///
/// This is the *conclusion* of a check, not the check itself: a check that has never run has no
/// row, and the panel renders "not evaluated yet" from the absence plus the registry. Once a
/// row exists it always has a state, because the state column is not nullable and `unknown` is
/// one of its four legal values.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct CheckResult {
    /// The row's id.
    pub id: i64,
    /// Organization the posture belongs to (`None` = platform level).
    pub organization_id: Option<Uuid>,
    /// The check this answers for, e.g. `mfa_enforced`.
    pub check_key: String,
    /// One of the four words in [`crate::vocabulary::STATES`].
    pub state: String,
    /// What the check saw. Structured on purpose: the panel renders fields from it and never
    /// shows a raw blob.
    pub detail: serde_json::Value,
    /// The run that produced this row; every check in a run shares it.
    pub run_id: Uuid,
    /// When it was evaluated.
    pub checked_at: OffsetDateTime,
}

impl CheckResult {
    /// `true` when the check is saying something is wrong, which is what the score ring counts
    /// as a deduction. `unknown` is **not** a deduction: an unanswered question is not a
    /// failure, and scoring it as one would push operators to fill in a value they do not have
    /// in order to make the number go up.
    #[must_use]
    pub fn is_deduction(&self) -> bool {
        matches!(self.state.as_str(), "warn" | "fail")
    }
}

/// A posture check's result, before it is stored.
#[derive(Debug, Clone, PartialEq)]
pub struct NewCheckResult {
    /// The check this answers for.
    pub check_key: String,
    /// The state it reported.
    pub state: String,
    /// What it saw.
    pub detail: serde_json::Value,
    /// The run it belongs to.
    pub run_id: Uuid,
}

impl NewCheckResult {
    /// Validate the shape the SQL constraints would otherwise catch as a raw error.
    ///
    /// # Errors
    ///
    /// Returns [`SecurityError::Invalid`] when the key is not a slug, or the state is not one
    /// of the four words, with a message naming the field.
    pub fn build(self) -> Result<Self> {
        if !crate::vocabulary::is_state(&self.state) {
            return Err(SecurityError::invalid(format!(
                "state must be one of {}, got {:?}",
                crate::vocabulary::STATES.join(", "),
                self.state
            )));
        }
        if !is_slug(&self.check_key) {
            return Err(SecurityError::invalid(format!(
                "check_key {:?} must be 3-64 characters of a-z, 0-9 or _",
                self.check_key
            )));
        }
        Ok(self)
    }
}

/// A finding: something the platform believes is wrong, with what is known about it.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Finding {
    /// The row's id.
    pub id: Uuid,
    /// Organization it belongs to.
    pub organization_id: Option<Uuid>,
    /// Where it came from.
    pub source: String,
    /// How bad it is.
    pub severity: String,
    /// One line, shown in the table.
    pub title: String,
    /// The detail drawer's body.
    pub description: String,
    /// The dependency it is about, when there is one.
    pub component: Option<String>,
    /// The version currently present.
    pub component_version: Option<String>,
    /// The version that fixes it.
    pub fixed_in: Option<String>,
    /// What has been done about it.
    pub status: String,
    /// Why it was ignored. Required by the database whenever the status is `ignored`.
    pub ignore_reason: Option<String>,
    /// When the ignore lapses.
    pub ignored_until: Option<OffsetDateTime>,
    /// Who acknowledged it.
    pub acknowledged_by: Option<Uuid>,
    /// When they did.
    pub acknowledged_at: Option<OffsetDateTime>,
    /// The operator's note.
    pub note: Option<String>,
    /// When it was first raised. Never moves — a finding that has been open for a year has
    /// been open for a year, and rewriting this on re-ingest would make age meaningless.
    pub first_seen_at: OffsetDateTime,
    /// When it was last confirmed still true. This is the one that moves on re-ingest.
    pub last_seen_at: OffsetDateTime,
    /// Component + title, hashed; what makes a re-ingest the same finding.
    pub fingerprint: String,
}

impl Finding {
    /// `true` when the finding is still counting against the posture score.
    #[must_use]
    pub fn is_open(&self) -> bool {
        matches!(self.status.as_str(), "open" | "acknowledged")
    }

    /// `true` when the ignore has lapsed and the finding is back in the open set.
    ///
    /// The row is not rewritten when the clock passes `ignored_until` — the expiry is a
    /// *reading* of the row, not a job. A job would need to run, and a screen that hides a
    /// finding because a cron did not fire is a screen that lies.
    #[must_use]
    pub fn ignore_has_lapsed(&self, now: OffsetDateTime) -> bool {
        self.status == "ignored" && self.ignored_until.is_some_and(|until| until <= now)
    }
}

/// A finding to write or refresh.
#[derive(Debug, Clone, PartialEq)]
pub struct NewFinding {
    /// Where it came from.
    pub source: String,
    /// How bad it is.
    pub severity: String,
    /// One line.
    pub title: String,
    /// The detail drawer's body.
    pub description: String,
    /// The dependency it is about.
    pub component: Option<String>,
    /// The version present.
    pub component_version: Option<String>,
    /// The version that fixes it.
    pub fixed_in: Option<String>,
    /// Structured evidence, kept for the detail drawer. Never a secret: a dependency report
    /// carries package names and versions, and the ingest path rejects a document with keys
    /// that look like credentials in it.
    pub evidence: serde_json::Value,
}

impl NewFinding {
    /// Validate the shape and compute the fingerprint.
    ///
    /// # Errors
    ///
    /// Returns [`SecurityError::Invalid`] naming the offending field for a source, severity or
    /// title the platform will not store.
    pub fn build(self) -> Result<BuiltFinding> {
        if !is_source(&self.source) {
            return Err(SecurityError::invalid(format!(
                "source must be one of {}, got {:?}",
                crate::vocabulary::SOURCES.join(", "),
                self.source
            )));
        }
        if !is_severity(&self.severity) {
            return Err(SecurityError::invalid(format!(
                "severity must be one of {}, got {:?}",
                SEVERITIES.join(", "),
                self.severity
            )));
        }
        let title = self.title.trim().to_string();
        if title.is_empty() || title.chars().count() > MAX_TITLE {
            return Err(SecurityError::invalid(format!(
                "title must be 1-{MAX_TITLE} characters, got {}",
                title.chars().count()
            )));
        }
        // The fingerprint is computed *before* the fields are moved out, because it is a
        // function of two of them. Computing it afterwards is the kind of borrow error that
        // tempts a `.clone()` fix which then hides a second, wrong fingerprint.
        let fingerprint = fingerprint_of(self.component.as_deref(), &title);
        Ok(BuiltFinding {
            source: self.source,
            severity: self.severity,
            title,
            description: self.description,
            component: self.component,
            component_version: self.component_version,
            fixed_in: self.fixed_in,
            fingerprint,
        })
    }
}

/// A finding that passed [`NewFinding::build`]: validated, and carrying its fingerprint.
#[derive(Debug, Clone, PartialEq)]
pub struct BuiltFinding {
    /// Validated source.
    pub source: String,
    /// Validated severity.
    pub severity: String,
    /// Trimmed, length-checked title.
    pub title: String,
    /// Description, verbatim.
    pub description: String,
    /// Component, if any.
    pub component: Option<String>,
    /// Version present, if any.
    pub component_version: Option<String>,
    /// Fix version, if any.
    pub fixed_in: Option<String>,
    /// The computed fingerprint.
    pub fingerprint: String,
}

/// The status change an operator asked for.
///
/// Built with [`StatusChange::of`] and the setters, so the store sees only combinations the
/// crate has agreed to. In particular [`StatusChange::ignore`] carries a reason, because
/// "ignore" without one is a dismissal and the platform refuses to record it.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusChange {
    /// The status being moved to.
    pub status: String,
    /// The reason, required when the status is `ignored`.
    pub ignore_reason: Option<String>,
    /// When an ignore lapses.
    pub ignored_until: Option<OffsetDateTime>,
    /// An operator note.
    pub note: Option<String>,
}

impl StatusChange {
    /// Acknowledge: seen, under assessment, still counted.
    ///
    /// # Errors
    ///
    /// Returns [`SecurityError::Invalid`] for an unknown status.
    pub fn of(status: &str) -> Result<Self> {
        if !is_finding_status(status) {
            return Err(SecurityError::invalid(format!(
                "status must be one of {}, got {status:?}",
                FINDING_STATUSES.join(", ")
            )));
        }
        Ok(Self {
            status: status.to_string(),
            ignore_reason: None,
            ignored_until: None,
            note: None,
        })
    }

    /// Set the reason an ignore is justified.
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.ignore_reason = Some(reason.into());
        self
    }

    /// Set when an ignore lapses.
    #[must_use]
    pub fn with_expiry(mut self, until: OffsetDateTime) -> Self {
        self.ignored_until = Some(until);
        self
    }

    /// Attach an operator note.
    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// Validate the combination the SQL constraint enforces, with a better message.
    ///
    /// # Errors
    ///
    /// Returns [`SecurityError::Invalid`] when an ignore carries no reason, when an ignore
    /// carries an expiry in the past, or when a note is longer than [`MAX_NOTE`].
    pub fn build(self, now: OffsetDateTime) -> Result<Self> {
        if self.status == "ignored" {
            let reason = self.ignore_reason.as_deref().unwrap_or_default().trim();
            if reason.is_empty() {
                return Err(SecurityError::invalid(
                    "ignore_reason is required when a finding is ignored",
                ));
            }
            if reason.chars().count() > MAX_IGNORE_REASON {
                return Err(SecurityError::invalid(format!(
                    "ignore_reason must be at most {MAX_IGNORE_REASON} characters"
                )));
            }
            if self.ignored_until.is_some_and(|until| until <= now) {
                return Err(SecurityError::invalid(
                    "ignored_until must be in the future; an ignore that has already lapsed is not an ignore",
                ));
            }
        }
        if let Some(note) = &self.note {
            if note.chars().count() > MAX_NOTE {
                return Err(SecurityError::invalid(format!(
                    "note must be at most {MAX_NOTE} characters"
                )));
            }
        }
        Ok(self)
    }
}

/// A list read of the findings table.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FindingQuery {
    /// Only this severity.
    pub severity: Option<String>,
    /// Only this status.
    pub status: Option<String>,
    /// Only this source.
    pub source: Option<String>,
    /// Only this component.
    pub component: Option<String>,
    /// Case-insensitive substring over the title and the description.
    pub search: Option<String>,
    /// Only rows seen at or after this instant.
    pub seen_after: Option<OffsetDateTime>,
    /// How many to skip.
    pub offset: i64,
    /// How many to return; clamped to [`crate::vocabulary::MAX_PAGE`].
    pub limit: i64,
}

impl FindingQuery {
    /// An unfiltered first page.
    #[must_use]
    pub fn new() -> Self {
        Self {
            limit: 50,
            ..Self::default()
        }
    }

    /// The page size after the clamp, which is what the store will actually honour.
    #[must_use]
    pub fn effective_limit(&self) -> usize {
        if self.limit <= 0 {
            return 1;
        }
        (self.limit as usize).min(crate::vocabulary::MAX_PAGE)
    }
}

/// One page of findings and the total the current filter matches.
#[derive(Debug, Clone, PartialEq)]
pub struct FindingPage {
    /// The rows on this page.
    pub findings: Vec<Finding>,
    /// How many rows the filter matches in total, not just on this page — the count above the
    /// list and the list itself must be the same question.
    pub total: i64,
    /// The offset this page started at.
    pub offset: i64,
}

/// Open findings grouped by severity, for the overview's score ring.
///
/// `Serialize` is part of the type rather than applied at the route: the bucket is part of the
/// answer, and a route that has to borrow a *local* struct to serialise it is a route that
/// will eventually forget and answer `null` for the whole legend.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SeverityCount {
    /// The severity.
    pub severity: String,
    /// How many open findings carry it.
    pub count: i64,
}

/// `true` when `value` matches the check-key slug rule the migration enforces.
#[must_use]
pub fn is_slug(value: &str) -> bool {
    (3..=64).contains(&value.len())
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The fingerprint of a finding: component (or an empty string) and title, joined by a
/// separator that cannot appear in either, so `("a-b", "c")` and `("a", "b-c")` do not collide.
#[must_use]
pub fn fingerprint_of(component: Option<&str>, title: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(component.unwrap_or("").as_bytes());
    hasher.update([0x1f]);
    hasher.update(title.trim().to_lowercase().as_bytes());
    hex::encode(hasher.finalize())
}

// Local import kept out of the module head so the sha2 dependency reads as what it is: a
// fingerprint, not a signature. `omnion-events` owns signing.
use sha2::{Digest, Sha256};

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    #[test]
    fn a_finding_with_a_bogus_severity_is_refused_with_the_field_named() {
        let err = NewFinding {
            source: "dependency".into(),
            severity: "spicy".into(),
            title: "Something".into(),
            description: String::new(),
            component: None,
            component_version: None,
            fixed_in: None,
            evidence: serde_json::json!({}),
        }
        .build()
        .expect_err("spicy is not a severity");
        let message = err.to_string();
        assert!(message.contains("severity"), "got: {message}");
    }

    #[test]
    fn a_title_is_trimmed_and_length_checked() {
        let built = NewFinding {
            source: "config".into(),
            severity: "high".into(),
            title: "   CSP header is not enforced   ".into(),
            description: String::new(),
            component: None,
            component_version: None,
            fixed_in: None,
            evidence: serde_json::json!({}),
        }
        .build()
        .expect("trimmed title is valid");
        assert_eq!(built.title, "CSP header is not enforced");

        let too_long = "x".repeat(MAX_TITLE + 1);
        let err = NewFinding {
            source: "config".into(),
            severity: "high".into(),
            title: too_long,
            description: String::new(),
            component: None,
            component_version: None,
            fixed_in: None,
            evidence: serde_json::json!({}),
        }
        .build()
        .expect_err("a 201-character title is over the limit");
        assert!(err.to_string().contains("title"), "got: {err}");
    }

    #[test]
    fn the_same_finding_from_two_ingests_has_one_fingerprint() {
        // The whole point of the fingerprint: CI produces the same report twice, and the second
        // ingest must land on the first row rather than doubling the count.
        let a = fingerprint_of(Some("tokio"), "Unpinned dependency");
        let b = fingerprint_of(Some("tokio"), "unpinned dependency");
        assert_eq!(a, b, "case and padding must not make a new finding");
        assert_ne!(a, fingerprint_of(Some("axum"), "Unpinned dependency"));
    }

    #[test]
    fn a_component_and_title_cannot_collide_across_the_separator() {
        assert_ne!(
            fingerprint_of(Some("a-b"), "c"),
            fingerprint_of(Some("a"), "b-c")
        );
    }

    #[test]
    fn ignoring_without_a_reason_is_refused() {
        let err = StatusChange::of("ignored")
            .expect("ignored is a status")
            .build(now())
            .expect_err("an ignore with no reason is a dismissal");
        assert!(err.to_string().contains("ignore_reason"), "got: {err}");

        // Whitespace is not a reason.
        let err = StatusChange::of("ignored")
            .expect("ignored is a status")
            .with_reason("   ")
            .build(now())
            .expect_err("three spaces are not a reason");
        assert!(err.to_string().contains("ignore_reason"), "got: {err}");
    }

    #[test]
    fn an_ignore_that_already_lapsed_is_refused() {
        let err = StatusChange::of("ignored")
            .expect("ignored is a status")
            .with_reason("after the release")
            .with_expiry(now() - time::Duration::minutes(1))
            .build(now())
            .expect_err("an expired ignore is not an ignore");
        assert!(err.to_string().contains("ignored_until"), "got: {err}");
    }

    #[test]
    fn an_ignore_with_a_reason_and_a_future_date_is_accepted() {
        let change = StatusChange::of("ignored")
            .expect("ignored is a status")
            .with_reason("mitigated by the WAF rule")
            .with_expiry(now() + time::Duration::days(7))
            .with_note("ticket OPS-441")
            .build(now())
            .expect("a justified, dated ignore is valid");
        assert_eq!(change.status, "ignored");
    }

    #[test]
    fn acknowledging_needs_no_reason() {
        let change = StatusChange::of("acknowledged")
            .expect("acknowledged is a status")
            .build(now())
            .expect("acknowledging is not a dismissal");
        assert!(change.ignore_reason.is_none());
    }

    #[test]
    fn a_lapsed_ignore_reads_as_open_again_without_rewriting_the_row() {
        let mut finding = Finding {
            id: Uuid::nil(),
            organization_id: None,
            source: "config".into(),
            severity: "medium".into(),
            title: "t".into(),
            description: String::new(),
            component: None,
            component_version: None,
            fixed_in: None,
            status: "ignored".into(),
            ignore_reason: Some("later".into()),
            ignored_until: Some(now() - time::Duration::hours(1)),
            acknowledged_by: None,
            acknowledged_at: None,
            note: None,
            first_seen_at: now(),
            last_seen_at: now(),
            fingerprint: String::new(),
        };
        assert!(
            finding.ignore_has_lapsed(now()),
            "an ignore past its date is not an ignore"
        );
        assert!(
            !finding.is_open(),
            "…but the row still says ignored, which the store keeps"
        );

        finding.status = "open".into();
        assert!(finding.is_open());
        assert!(!finding.ignore_has_lapsed(now()));
    }

    #[test]
    fn a_check_key_is_a_slug_or_it_is_not_a_key() {
        assert!(is_slug("mfa_enforced"));
        assert!(is_slug("https_terminated"));
        assert!(!is_slug("MFA"), "upper case is not a slug");
        assert!(!is_slug("mfa enforced"), "a space is not a slug");
        assert!(
            !is_slug("ab"),
            "two characters is too short for a readable key"
        );
    }

    #[test]
    fn the_page_size_is_clamped_not_trusted() {
        let mut query = FindingQuery::new();
        assert_eq!(query.effective_limit(), 50);
        query.limit = 10_000;
        assert_eq!(query.effective_limit(), crate::vocabulary::MAX_PAGE);
        query.limit = 0;
        assert_eq!(
            query.effective_limit(),
            1,
            "a zero page is one row, not the whole table"
        );
    }

    #[test]
    fn an_unknown_check_state_is_refused_by_the_builder() {
        let err = NewCheckResult {
            check_key: "mfa_enforced".into(),
            state: "probably".into(),
            detail: serde_json::json!({}),
            run_id: Uuid::nil(),
        }
        .build()
        .expect_err("probably is not a state");
        assert!(err.to_string().contains("state"), "got: {err}");
    }

    #[test]
    fn unknown_is_not_a_deduction_but_warn_and_fail_are() {
        let mut result = CheckResult {
            id: 1,
            organization_id: None,
            check_key: "backup_healthy".into(),
            state: "unknown".into(),
            detail: serde_json::json!({}),
            run_id: Uuid::nil(),
            checked_at: now(),
        };
        assert!(
            !result.is_deduction(),
            "an unanswered question is not a failure"
        );
        result.state = "warn".into();
        assert!(result.is_deduction());
        result.state = "fail".into();
        assert!(result.is_deduction());
        result.state = "pass".into();
        assert!(!result.is_deduction());
    }
}
