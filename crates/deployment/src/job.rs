//! The deployment job: steps, order, cancel, and the one rule that is easy to get wrong.
//!
//! A deploy is a sequence of named steps that run in order, each leaving a line in the log pane
//! the wizard shows. Three properties of that sequence carry the whole request, and each is a
//! place a shortcut produces a deployment that cannot be recovered:
//!
//! * **Order is fixed and complete.** [`plan_steps`] is the only place the step list is written.
//!   A step the UI never renders is a step that never ran, so the list is derived, not assembled
//!   by a caller from whatever it happens to know.
//! * **Cancel stops before the first irreversible step.** The spec says "cancel while
//!   pre-migration". Once migrations have begun, the data has already moved and stopping is not a
//!   cancel — it is a rollback, which is a different action with its own confirmation. Folding
//!   the two together is how a half-migrated database is left behind "cancelled".
//! * **The log is append-only.** A step's output is added to, never rewritten, because the log is
//!   the only record of what the run did when the operator closed the browser.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Which of the three jobs this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    /// Moving an environment forward to a release.
    Deploy,
    /// Moving an environment back to a known-good release.
    Rollback,
    /// Restarting the workload behind a cluster or a single process.
    Restart,
}

impl JobKind {
    /// The value stored in `deployments.kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            JobKind::Deploy => "deploy",
            JobKind::Rollback => "rollback",
            JobKind::Restart => "restart",
        }
    }

    /// Parse a stored kind. `None` for anything unknown, so a row written by a newer build
    /// renders as unknown rather than being coerced into a deploy.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "deploy" => Some(JobKind::Deploy),
            "rollback" => Some(JobKind::Rollback),
            "restart" => Some(JobKind::Restart),
            _ => None,
        }
    }
}

/// Where a job is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    /// Accepted, steps not started.
    Preflight,
    /// At least one step is running.
    Running,
    /// Every step ran; the health verification is in flight.
    Verifying,
    /// Finished and healthy.
    Succeeded,
    /// Finished with an error.
    Failed,
    /// Stopped before the first irreversible step.
    Cancelled,
}

impl JobStatus {
    /// The value stored in `deployments.status`.
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Preflight => "preflight",
            JobStatus::Running => "running",
            JobStatus::Verifying => "verifying",
            JobStatus::Succeeded => "succeeded",
            JobStatus::Failed => "failed",
            JobStatus::Cancelled => "cancelled",
        }
    }

    /// Parse a stored status, with the same unknown-value rule as [`JobKind::parse`].
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "preflight" => Some(JobStatus::Preflight),
            "running" => Some(JobStatus::Running),
            "verifying" => Some(JobStatus::Verifying),
            "succeeded" => Some(JobStatus::Succeeded),
            "failed" => Some(JobStatus::Failed),
            "cancelled" => Some(JobStatus::Cancelled),
            _ => None,
        }
    }

    /// Is the job still holding the environment?
    ///
    /// This is what the `409` on a second deploy reads, so it is deliberately the *storage* set:
    /// the three states a row can be in while it is still the active job for its environment.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            JobStatus::Preflight | JobStatus::Running | JobStatus::Verifying
        )
    }

    /// Has the job finished, one way or another?
    pub fn is_finished(self) -> bool {
        !self.is_active()
    }
}

/// One step of a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// Position in the sequence, 0-based. The order the wizard's timeline renders.
    pub position: u32,
    /// The step name as stored in `deployment_steps.name`.
    pub name: String,
    /// Running, done, failed, skipped, or still pending.
    pub status: StepStatus,
    /// The accumulated log for this step. Append-only.
    pub output: String,
    /// When the step started.
    pub started_at: Option<OffsetDateTime>,
    /// When the step finished.
    pub finished_at: Option<OffsetDateTime>,
}

/// A step's own state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    /// Not started.
    Pending,
    /// Started, not finished.
    Running,
    /// Finished successfully.
    Done,
    /// Finished with an error.
    Failed,
    /// Not run, because an earlier step failed.
    Skipped,
    /// A status this build does not know.
    ///
    /// Its own variant for the same reason `preflight::CheckState::Unknown` exists: a step whose
    /// stored status is unrecognised is **not** pending, because pending is the state the wizard
    /// offers to start. Coercing it would let a re-run of an already-finished step be launched
    /// against a release that wrote a status this build has never heard of. It is never written
    /// — `as_str` exists for values the database accepts, and this one has no column.
    Unknown,
}

impl StepStatus {
    /// The value stored in `deployment_steps.status`.
    pub fn as_str(self) -> &'static str {
        match self {
            StepStatus::Pending => "pending",
            StepStatus::Running => "running",
            StepStatus::Done => "done",
            StepStatus::Failed => "failed",
            StepStatus::Skipped => "skipped",
            // Never stored: the column's check constraint has no such value. The arm exists so
            // the match is total, and `unknown` is what the API serialises it as.
            StepStatus::Unknown => "unknown",
        }
    }

    /// Parse a stored step status, with the same unknown-value rule as [`JobStatus::parse`].
    ///
    /// `None` for anything this build does not know, so a step written by a newer release
    /// renders as `unknown` in the timeline rather than being coerced into "pending" — a pending
    /// step is one the wizard will let the operator start, and a step that already ran is not.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(StepStatus::Pending),
            "running" => Some(StepStatus::Running),
            "done" => Some(StepStatus::Done),
            "failed" => Some(StepStatus::Failed),
            "skipped" => Some(StepStatus::Skipped),
            _ => None,
        }
    }
}

/// The four steps of a deploy, in the order the spec names them: backup → migrate → deploy → verify.
pub const DEPLOY_STEPS: [&str; 4] = ["backup", "migrate", "deploy", "verify"];

/// The steps of a rollback.
///
/// Not the deploy list with different names: a rollback has no migrations of its own (the older
/// binary reads the append-only schema) and it *does* take a backup, because a rollback is itself
/// a change to the running system and the operator needs a way back from it.
pub const ROLLBACK_STEPS: [&str; 3] = ["backup", "deploy", "verify"];

/// The steps of a workload restart.
pub const RESTART_STEPS: [&str; 2] = ["deploy", "verify"];

/// The step list for a job kind.
///
/// The one rule here that is a safety property rather than a convenience: [`MIGRATE_STEP_NAME`]
/// is the boundary a cancel may not cross.
pub fn plan_steps(kind: JobKind) -> &'static [&'static str] {
    match kind {
        JobKind::Deploy => &DEPLOY_STEPS,
        JobKind::Rollback => &ROLLBACK_STEPS,
        JobKind::Restart => &RESTART_STEPS,
    }
}

/// The first step that has already changed the data or the running process.
///
/// A cancel is refused from this step onwards. Everything before it is preparation, and a
/// preparation step can be abandoned with nothing to undo.
pub const MIGRATE_STEP_NAME: &str = "migrate";

/// May a job of this kind be cancelled right now?
///
/// `current_step` is the name of the step that has started, or `None` before the first one.
///
/// The whole rule in four lines: a plan that has no [`MIGRATE_STEP_NAME`] in it is cancellable
/// throughout (there is nothing irreversible to undo), and a plan that has one is cancellable up
/// to the step *before* it. Everything from the migrate step onwards is past the boundary.
///
/// The first version of this was a pair of `any` scans over the slices either side of the current
/// position, which is three ways to write the same predicate and at least one way to get it
/// backwards — the tests caught it refusing the `backup` step, which is exactly the step the
/// Cancel button exists for.
pub fn may_cancel(kind: JobKind, current_step: Option<&str>) -> bool {
    let plan = plan_steps(kind);
    let Some(boundary) = plan.iter().position(|name| *name == MIGRATE_STEP_NAME) else {
        return true;
    };
    let Some(step) = current_step else {
        return true;
    };
    let Some(position) = plan.iter().position(|name| *name == step) else {
        // A step name this build does not know: refuse, because guessing "it is probably early"
        // is how a cancel crosses a migration that has already run.
        return false;
    };
    position < boundary
}

/// Why a cancel was refused, for the message the wizard shows instead of a silent disabled button.
pub fn cancel_refusal(kind: JobKind, current_step: Option<&str>) -> Option<String> {
    if may_cancel(kind, current_step) {
        return None;
    }
    let step = current_step.unwrap_or(MIGRATE_STEP_NAME);
    Some(format!(
        "the {step} step has already started; the data has moved, so this is a rollback rather than a cancel"
    ))
}

/// The job row, as the API returns it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    /// The job's id.
    pub id: Uuid,
    /// Which environment it targets.
    pub environment: String,
    /// What kind of job this is.
    pub kind: JobKind,
    /// Where it is in its life.
    pub status: JobStatus,
    /// The version it came from, `None` for a restart.
    pub from_version: Option<String>,
    /// The version it is going to, `None` for a restart.
    pub to_version: Option<String>,
    /// Who asked for it.
    pub started_by: Option<Uuid>,
    /// The reason, mandatory for a rollback.
    pub reason: Option<String>,
    /// When it started.
    pub started_at: OffsetDateTime,
    /// When it finished.
    pub finished_at: Option<OffsetDateTime>,
    /// How long it took, once it finished.
    pub duration_ms: Option<i64>,
    /// The error, when it failed.
    pub error: Option<String>,
    /// Its steps, in order.
    pub steps: Vec<Step>,
}

impl Job {
    /// The step that has started and not finished, if any.
    pub fn current_step(&self) -> Option<&str> {
        self.steps
            .iter()
            .find(|s| s.status == StepStatus::Running)
            .map(|s| s.name.as_str())
    }

    /// May this job be cancelled right now?
    pub fn may_cancel(&self) -> bool {
        may_cancel(self.kind, self.current_step())
    }

    /// How far along it is, as a whole percentage.
    ///
    /// Counts only **finished** steps: done, failed or skipped. A running step is not progress —
    /// a four-step job sitting in `verify` is 75% and saying 100% because everything that has
    /// been touched so far worked is the kind of number that makes the wizard look finished while
    /// the health check is still the thing that decides whether the deploy landed.
    pub fn progress_percent(&self) -> u8 {
        if self.steps.is_empty() {
            return 0;
        }
        let finished = self
            .steps
            .iter()
            .filter(|s| {
                matches!(
                    s.status,
                    StepStatus::Done | StepStatus::Failed | StepStatus::Skipped
                )
            })
            .count();
        ((finished * 100) / self.steps.len()) as u8
    }

    /// Fold the step outcomes into the job's own status.
    ///
    /// A failed step makes the job `failed` and every later step `skipped` — a deploy whose
    /// `verify` never ran must not read as succeeded, and the alternative (carrying the status in
    /// the row and letting the two disagree) is what produces a green history entry for a deploy
    /// that never came up.
    pub fn fold_status(&self) -> JobStatus {
        if self.steps.iter().any(|s| s.status == StepStatus::Failed) {
            return JobStatus::Failed;
        }
        if self.steps.iter().any(|s| s.status == StepStatus::Running) {
            return JobStatus::Running;
        }
        if self.steps.iter().any(|s| s.status == StepStatus::Pending) {
            return JobStatus::Preflight;
        }
        // Every step is done or skipped. A skipped step means an earlier failure already
        // returned, so this only reaches here on a clean run.
        if self.steps.iter().any(|s| s.status == StepStatus::Skipped) {
            return JobStatus::Failed;
        }
        JobStatus::Verifying
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steps_for(kind: JobKind, statuses: &[StepStatus]) -> Vec<Step> {
        plan_steps(kind)
            .iter()
            .enumerate()
            .map(|(i, name)| Step {
                position: i as u32,
                name: (*name).to_string(),
                status: statuses[i],
                output: String::new(),
                started_at: None,
                finished_at: None,
            })
            .collect()
    }

    #[test]
    fn each_kind_has_its_own_step_plan() {
        assert_eq!(plan_steps(JobKind::Deploy), &DEPLOY_STEPS);
        assert_eq!(plan_steps(JobKind::Rollback), &ROLLBACK_STEPS);
        assert_eq!(plan_steps(JobKind::Restart), &RESTART_STEPS);
        // A rollback has no migrations: the older binary reads the append-only schema. Giving it
        // a migrate step would make the cancel rule refuse a rollback that is entirely safe.
        assert!(!ROLLBACK_STEPS.contains(&MIGRATE_STEP_NAME));
    }

    #[test]
    fn a_deploy_is_cancellable_until_the_migrations_start() {
        assert!(
            may_cancel(JobKind::Deploy, None),
            "a queued job is cancellable"
        );
        assert!(may_cancel(JobKind::Deploy, Some("backup")));
        assert!(
            !may_cancel(JobKind::Deploy, Some("migrate")),
            "the boundary itself is refused"
        );
        assert!(!may_cancel(JobKind::Deploy, Some("deploy")));
        assert!(!may_cancel(JobKind::Deploy, Some("verify")));
    }

    #[test]
    fn the_refusal_explains_that_this_is_a_rollback_not_a_cancel() {
        let reason = cancel_refusal(JobKind::Deploy, Some("deploy")).expect("a message");
        assert!(
            reason.contains("rollback"),
            "the message must name the real action: {reason}"
        );
        assert_eq!(cancel_refusal(JobKind::Deploy, Some("backup")), None);
    }

    #[test]
    fn a_rollback_and_a_restart_are_cancellable_throughout() {
        for step in plan_steps(JobKind::Rollback) {
            assert!(
                may_cancel(JobKind::Rollback, Some(step)),
                "{step} should be cancellable"
            );
        }
        for step in plan_steps(JobKind::Restart) {
            assert!(
                may_cancel(JobKind::Restart, Some(step)),
                "{step} should be cancellable"
            );
        }
    }

    #[test]
    fn an_unknown_step_name_refuses_the_cancel_instead_of_guessing() {
        // The step name comes from the row, so a row written by a newer build can carry a name
        // this build does not know. Guessing "it is probably early" is how a cancel crosses a
        // migration that already ran.
        assert!(!may_cancel(JobKind::Deploy, Some("prewarm-cdn")));
        assert!(!may_cancel(JobKind::Deploy, Some("")));
    }

    #[test]
    fn a_folded_status_never_reports_green_for_a_deploy_that_never_came_up() {
        let ok = steps_for(
            JobKind::Deploy,
            &[
                StepStatus::Done,
                StepStatus::Done,
                StepStatus::Done,
                StepStatus::Done,
            ],
        );
        assert_eq!(fold(&ok), JobStatus::Verifying);

        let failed_migrate = steps_for(
            JobKind::Deploy,
            &[
                StepStatus::Done,
                StepStatus::Failed,
                StepStatus::Skipped,
                StepStatus::Skipped,
            ],
        );
        assert_eq!(fold(&failed_migrate), JobStatus::Failed);
    }

    #[test]
    fn progress_counts_pending_steps_so_verifying_does_not_read_as_a_hundred() {
        let at_verify = steps_for(
            JobKind::Deploy,
            &[
                StepStatus::Done,
                StepStatus::Done,
                StepStatus::Done,
                StepStatus::Running,
            ],
        );
        assert_eq!(percent(&at_verify), 75);
        let all_pending = steps_for(
            JobKind::Deploy,
            &[
                StepStatus::Pending,
                StepStatus::Pending,
                StepStatus::Pending,
                StepStatus::Pending,
            ],
        );
        assert_eq!(percent(&all_pending), 0);
        assert_eq!(percent(&[]), 0);
    }

    #[test]
    fn status_and_kind_names_round_trip_and_reject_unknowns() {
        for kind in [JobKind::Deploy, JobKind::Rollback, JobKind::Restart] {
            assert_eq!(JobKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(JobKind::parse("upgrade"), None);
        for status in [
            JobStatus::Preflight,
            JobStatus::Running,
            JobStatus::Verifying,
            JobStatus::Succeeded,
            JobStatus::Failed,
            JobStatus::Cancelled,
        ] {
            assert_eq!(JobStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(JobStatus::parse("halfway"), None);
    }

    #[test]
    fn only_the_three_live_states_hold_the_environment() {
        assert!(JobStatus::Preflight.is_active());
        assert!(JobStatus::Running.is_active());
        assert!(JobStatus::Verifying.is_active());
        // Verifying counts: a second deploy while the first is still proving it is healthy would
        // swap the version out from under the verification.
        assert!(!JobStatus::Succeeded.is_active());
        assert!(!JobStatus::Failed.is_active());
        assert!(!JobStatus::Cancelled.is_active());
        assert!(JobStatus::Succeeded.is_finished());
    }

    // Helpers so the tests above read as claims about the job rather than about its construction.
    fn fold(steps: &[Step]) -> JobStatus {
        let job = job_with(steps.to_vec());
        job.fold_status()
    }

    fn percent(steps: &[Step]) -> u8 {
        job_with(steps.to_vec()).progress_percent()
    }

    fn job_with(steps: Vec<Step>) -> Job {
        Job {
            id: Uuid::nil(),
            environment: "production".to_string(),
            kind: JobKind::Deploy,
            status: JobStatus::Running,
            from_version: Some("2.4.1".to_string()),
            to_version: Some("2.5.0".to_string()),
            started_by: None,
            reason: None,
            started_at: OffsetDateTime::UNIX_EPOCH,
            finished_at: None,
            duration_ms: None,
            error: None,
            steps,
        }
    }
}
