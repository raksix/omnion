//! The pre-flight checks, and what a failed one costs.
//!
//! The spec is blunt about why this module exists: "pre-flight exists to stop a deploy that would
//! fail halfway, so it must be honest — if backup freshness cannot be determined that is a visible
//! warning, never a silent pass."
//!
//! That last clause is the design. Every check returns one of three states, and the third is the
//! one a boolean gets wrong:
//!
//! * [`CheckState::Pass`] — the thing was checked and it is fine.
//! * [`CheckState::Warn`] — it was checked and something needs a decision. The deploy continues
//!   **only** after somebody acknowledges it, which is why the acknowledgement is a first-class
//!   part of [`PreflightReport`] rather than a UI convention.
//! * [`CheckState::Fail`] — the deploy cannot go ahead.
//! * [`CheckState::Unknown`] — it could not be determined.
//!
//! [`CheckState::Unknown`] is deliberately not a `Pass` and deliberately not a `Fail`. Folding it
//! into either is how a broken probe reads as a healthy install; the honest answer blocks the
//! deploy on the same line as a failure and names the probe, because "we could not reach the
//! object store" and "the object store is full" are different conversations with the operator.

use serde::{Deserialize, Serialize};

/// How a check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckState {
    /// Checked, and fine.
    Pass,
    /// Checked, and fine — but somebody should know before this goes to production.
    Warn,
    /// Checked, and the deploy is refused.
    Fail,
    /// Could not be determined. Blocks the deploy like a failure, and says which probe.
    Unknown,
}

impl CheckState {
    /// May the wizard show step 2 for this state?
    ///
    /// `Warn` is included deliberately: the wizard's rule is that a warning needs an explicit
    /// acknowledgement checkbox, so a warning is *not* a dead end. A `Fail` and an `Unknown` are.
    pub fn allows_continue(self) -> bool {
        matches!(self, CheckState::Pass | CheckState::Warn)
    }

    /// Does the wizard have to show the acknowledgement box for this state?
    pub fn needs_acknowledgement(self) -> bool {
        matches!(self, CheckState::Warn)
    }

    /// The one-word label the row renders.
    pub fn label(self) -> &'static str {
        match self {
            CheckState::Pass => "pass",
            CheckState::Warn => "warn",
            CheckState::Fail => "fail",
            CheckState::Unknown => "unknown",
        }
    }
}

/// The stable id of a check, so a client can key its own display off it and a test can name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckId {
    /// The most recent backup is recent enough to roll back to.
    BackupFreshness,
    /// Migrations are pending, and the count is known.
    PendingMigrations,
    /// Free disk space, per the release's own requirement where it declares one.
    FreeDiskSpace,
    /// Background jobs that would be interrupted mid-write.
    RunningBackgroundJobs,
    /// The services this release needs are answering.
    DependencyHealth,
    /// Whether this environment needs a maintenance window for the deploy.
    MaintenanceWindow,
    /// The installation's core is new enough for the target release.
    CoreCompatibility,
}

impl CheckId {
    /// Every check, in the order the wizard lists them.
    pub const ALL: [CheckId; 7] = [
        CheckId::BackupFreshness,
        CheckId::PendingMigrations,
        CheckId::FreeDiskSpace,
        CheckId::RunningBackgroundJobs,
        CheckId::DependencyHealth,
        CheckId::MaintenanceWindow,
        CheckId::CoreCompatibility,
    ];

    /// The row title, written for somebody reading a list of seven lines under pressure.
    pub fn title(self) -> &'static str {
        match self {
            CheckId::BackupFreshness => "Backup is recent enough to roll back to",
            CheckId::PendingMigrations => "Migrations this release ships",
            CheckId::FreeDiskSpace => "Free disk space",
            CheckId::RunningBackgroundJobs => "Background jobs in flight",
            CheckId::DependencyHealth => "Dependency health",
            CheckId::MaintenanceWindow => "Maintenance window",
            CheckId::CoreCompatibility => "Core version compatibility",
        }
    }
}

/// One check's outcome: the state, the detail a person reads, and what to do about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckOutcome {
    /// Which check this is.
    pub id: CheckId,
    /// [`CheckId::title`], carried in the payload so a client need not hard-code the copy.
    pub title: String,
    /// What it found.
    pub state: CheckState,
    /// The specifics — a timestamp, a file name, a number.
    pub detail: String,
    /// The suggested action. Empty when there is nothing to suggest, which is the `Pass` case:
    /// suggesting something for a passing check teaches operators to ignore the column.
    pub suggestion: String,
}

impl CheckOutcome {
    /// A passing check.
    pub fn pass(id: CheckId, detail: impl Into<String>) -> Self {
        CheckOutcome {
            id,
            title: id.title().to_string(),
            state: CheckState::Pass,
            detail: detail.into(),
            suggestion: String::new(),
        }
    }

    /// A warning, with the action it asks for.
    pub fn warn(id: CheckId, detail: impl Into<String>, suggestion: impl Into<String>) -> Self {
        CheckOutcome {
            id,
            title: id.title().to_string(),
            state: CheckState::Warn,
            detail: detail.into(),
            suggestion: suggestion.into(),
        }
    }

    /// A failure, with the action it asks for.
    pub fn fail(id: CheckId, detail: impl Into<String>, suggestion: impl Into<String>) -> Self {
        CheckOutcome {
            id,
            title: id.title().to_string(),
            state: CheckState::Fail,
            detail: detail.into(),
            suggestion: suggestion.into(),
        }
    }

    /// A check that could not run. Never a silent pass.
    pub fn unknown(id: CheckId, detail: impl Into<String>, suggestion: impl Into<String>) -> Self {
        CheckOutcome {
            id,
            title: id.title().to_string(),
            state: CheckState::Unknown,
            detail: detail.into(),
            suggestion: suggestion.into(),
        }
    }
}

/// The whole pre-flight result: the rows, and the two answers the wizard needs from them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreflightReport {
    /// The rows, one per [`CheckId`].
    pub checks: Vec<CheckOutcome>,
    /// Whether the environment is production. Production additionally requires the target
    /// version to be typed, which is a step-2 rule rather than a check.
    pub production: bool,
    /// Whether a maintenance window is required for this deploy.
    pub requires_maintenance_window: bool,
}

impl PreflightReport {
    /// Fold a set of outcomes into a report.
    ///
    /// Missing rows are filled in as `Unknown` rather than omitted, so a caller that forgets one
    /// check produces a report that *blocks* rather than a report that silently passes with six
    /// of seven rows. The alternative — render what you were given — is how a deploy ships past a
    /// check that was never written.
    pub fn from_outcomes(production: bool, outcomes: Vec<CheckOutcome>) -> Self {
        let mut checks: Vec<CheckOutcome> = outcomes;
        for id in CheckId::ALL {
            if !checks.iter().any(|c| c.id == id) {
                checks.push(CheckOutcome::unknown(
                    id,
                    "this check did not run",
                    "re-run the pre-flight; a check that did not answer is not a passing check",
                ));
            }
        }
        checks.sort_by_key(|c| CheckId::ALL.iter().position(|id| *id == c.id));
        let requires_maintenance_window = checks
            .iter()
            .any(|c| c.id == CheckId::MaintenanceWindow && c.state == CheckState::Fail);
        PreflightReport {
            checks,
            production,
            requires_maintenance_window,
        }
    }

    /// The state of one check, or `Unknown` if the row is missing.
    pub fn state_of(&self, id: CheckId) -> CheckState {
        self.checks
            .iter()
            .find(|c| c.id == id)
            .map_or(CheckState::Unknown, |c| c.state)
    }

    /// May the operator press `Continue`?
    ///
    /// Only after every check has passed, or has warned **and** been acknowledged. This is the one
    /// place the rule lives, so the wizard's disabled button and the server's `422` cannot
    /// disagree about the same report.
    pub fn can_continue(&self, acknowledged: bool) -> bool {
        if self.checks.iter().any(|c| !c.state.allows_continue()) {
            return false;
        }
        acknowledged || !self.checks.iter().any(|c| c.state.needs_acknowledgement())
    }

    /// Why `Continue` is disabled, in a sentence. `None` when it is not.
    pub fn blocked_reason(&self, acknowledged: bool) -> Option<String> {
        if self.can_continue(acknowledged) {
            return None;
        }
        let failing: Vec<&str> = self
            .checks
            .iter()
            .filter(|c| !c.state.allows_continue())
            .map(|c| c.title.as_str())
            .collect();
        if !failing.is_empty() {
            return Some(format!("fix first: {}", failing.join(", ")));
        }
        Some("acknowledge the warnings before continuing".to_string())
    }
}

/// The typed-confirmation rule for a production deploy.
///
/// A separate type because the rule is a *product* decision, not a UI detail: production takes
/// the target version in plain sight, and anything less has a long history of shipping the wrong
/// artifact. Staging and sandbox do not ask, because a sandbox deploy is a rehearsal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Confirmation {
    /// The exact target version must be typed.
    TypeVersion,
    /// A plain press is enough.
    None,
}

/// What confirmation this environment needs.
pub fn confirmation_for(production: bool) -> Confirmation {
    if production {
        Confirmation::TypeVersion
    } else {
        Confirmation::None
    }
}

/// Is the typed value a valid confirmation of `to_version`?
///
/// Both sides are trimmed before comparison and nothing else: no case folding (versions are
/// digits and dots, and lowercasing a `2.5.0` cannot help) and no whitespace collapsing beyond the
/// trim, so `2.5.0 extra` is refused rather than accepted.
pub fn confirmation_matches(typed: &str, to_version: &str) -> bool {
    typed.trim() == to_version.trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_pass() -> Vec<CheckOutcome> {
        CheckId::ALL
            .iter()
            .map(|id| CheckOutcome::pass(*id, "ok"))
            .collect()
    }

    #[test]
    fn an_all_passing_report_lets_the_wizard_through() {
        let report = PreflightReport::from_outcomes(true, all_pass());
        assert!(report.can_continue(false));
        assert_eq!(report.blocked_reason(false), None);
        assert!(!report.requires_maintenance_window);
    }

    #[test]
    fn a_failure_blocks_and_names_itself() {
        let mut checks = all_pass();
        checks[0] = CheckOutcome::fail(
            CheckId::BackupFreshness,
            "the newest backup is 19 days old",
            "take a backup first",
        );
        let report = PreflightReport::from_outcomes(true, checks);
        assert!(!report.can_continue(false));
        let reason = report.blocked_reason(false).expect("a reason");
        assert!(
            reason.contains("Backup"),
            "the reason must name the row: {reason}"
        );
    }

    #[test]
    fn a_warning_needs_an_acknowledgement_and_the_box_is_the_difference() {
        // This pair is the load-bearing one: a boolean that forgot the acknowledgement would pass
        // here, and the acknowledgement is what the spec means by "a warning requires an
        // acknowledgement checkbox".
        let mut checks = all_pass();
        checks[1] = CheckOutcome::warn(
            CheckId::PendingMigrations,
            "3 migrations will run",
            "read them before continuing",
        );
        let report = PreflightReport::from_outcomes(true, checks);
        assert!(
            !report.can_continue(false),
            "an unacknowledged warning must not pass"
        );
        assert!(
            report.can_continue(true),
            "an acknowledged warning must pass"
        );
        assert_eq!(
            report.blocked_reason(false).as_deref(),
            Some("acknowledge the warnings before continuing")
        );
        assert!(!CheckState::Warn.needs_acknowledgement() == false);
    }

    #[test]
    fn a_check_that_did_not_run_blocks_instead_of_being_ignored() {
        // The failure mode this guards: a caller that forgets a check gets a report that blocks,
        // not a report with six rows that quietly passes.
        let partial = vec![CheckOutcome::pass(CheckId::BackupFreshness, "ok")];
        let report = PreflightReport::from_outcomes(false, partial);
        assert_eq!(report.checks.len(), CheckId::ALL.len());
        assert_eq!(
            report.state_of(CheckId::DependencyHealth),
            CheckState::Unknown
        );
        assert!(
            !report.can_continue(true),
            "acknowledging must not override an unknown"
        );
    }

    #[test]
    fn an_unknown_is_not_a_pass_and_not_a_warn() {
        assert!(!CheckState::Unknown.allows_continue());
        assert!(!CheckState::Unknown.needs_acknowledgement());
        assert_ne!(CheckState::Unknown, CheckState::Pass);
        assert_ne!(CheckState::Unknown, CheckState::Warn);
    }

    #[test]
    fn a_maintenance_failure_is_what_marks_the_window_required() {
        let mut checks = all_pass();
        checks[5] = CheckOutcome::fail(
            CheckId::MaintenanceWindow,
            "writes would be visible to users during the swap",
            "open a maintenance window for the deploy",
        );
        let report = PreflightReport::from_outcomes(true, checks);
        assert!(report.requires_maintenance_window);
        assert!(!report.can_continue(true));
    }

    #[test]
    fn rows_come_back_in_a_stable_order_whatever_order_they_arrive_in() {
        let shuffled = vec![
            CheckOutcome::pass(CheckId::MaintenanceWindow, "ok"),
            CheckOutcome::pass(CheckId::BackupFreshness, "ok"),
            CheckOutcome::pass(CheckId::CoreCompatibility, "ok"),
        ];
        let report = PreflightReport::from_outcomes(false, shuffled);
        let ids: Vec<CheckId> = report.checks.iter().map(|c| c.id).collect();
        let expected: Vec<CheckId> = CheckId::ALL.to_vec();
        assert_eq!(
            ids, expected,
            "the wizard's row order must not depend on map iteration"
        );
    }

    #[test]
    fn production_requires_typing_the_version_and_nothing_else_does() {
        assert_eq!(confirmation_for(true), Confirmation::TypeVersion);
        assert_eq!(confirmation_for(false), Confirmation::None);
        assert!(confirmation_matches(" 2.5.0 ", "2.5.0"));
        // The classic failure: accepting anything that *contains* the version, so a paste of
        // "deploy 2.5.0 now" confirms a deploy of 2.5.0.
        assert!(!confirmation_matches("deploy 2.5.0 now", "2.5.0"));
        assert!(!confirmation_matches("2.5.1", "2.5.0"));
        assert!(!confirmation_matches("2.5.0 extra", "2.5.0"));
        assert!(!confirmation_matches("", "2.5.0"));
    }
}
