//! The run guard: how a process tells the engine "this run is repeating itself".
//!
//! The engine owns *when* a step runs and *whether* it succeeded; it does not know what a
//! rule repeating itself means, because the endless-loop guard is the automation layer's
//! vocabulary (REQ-003 slice 4) and the engine crate must not depend on it — the
//! dependency runs the other way. So the engine asks, through this trait, at exactly one
//! point: **after a step succeeded and before the run is settled**.
//!
//! Why the engine asks *there* rather than owning the check:
//!
//! * a guard that ran *before* the step could not tell a repeat from a first run — the
//!   fingerprint it would compare is the one it is about to produce;
//! * a guard that ran in a sweep would be a tick behind, so a rule that loops four times
//!   in a second would already have four runs before anyone noticed;
//! * every other ending of a run is already a write in the engine (a branch that fails
//!   closes the steps after it, a `stop` settles the run), so a guard that has to write
//!   *again* from outside would have to know the same thing.
//!
//! ## The default: a guard that is never installed
//!
//! [`NoRunGuard`] lets every run through. That is the right default for the engine in
//! isolation — a process that installs no host action handler also gets no loop guard, and
//! a synthetic `wait → wait` step pair is a legitimate definition in a process that is not
//! running automations. The automation layer installs the real one
//! (`omnion_automation::loopguard`).

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;
use uuid::Uuid;

/// What a guard decided about a run that has just finished a step.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GuardVerdict {
    /// `true` when the run must stop here.
    ///
    /// `Default` is `false`, so a verdict that was never built (a handler that returned
    /// nothing, a future that resolved to `()`) means "let the run go on" — the same
    /// direction as a guard that was never installed. The failure mode of a bug in the
    /// guard is therefore a loop that is not caught, not a run that stops for no reason.
    pub stop: bool,
    /// The message the run's error carries, when it stops.
    pub reason: Option<String>,
}

impl GuardVerdict {
    /// Let the run continue.
    #[must_use]
    pub fn clear() -> Self {
        Self::default()
    }

    /// Stop the run here, with the reason an operator reads.
    #[must_use]
    pub fn stop(reason: impl Into<String>) -> Self {
        Self {
            stop: true,
            reason: Some(reason.into()),
        }
    }
}

/// The step a guard is asked about.
pub struct GuardStep<'a> {
    /// The run the step belongs to.
    pub execution_id: Uuid,
    /// Position in the run (1-based).
    pub step_no: i32,
    /// Step kind: `task`, `wait`, `branch`, `stop` or `approval`.
    pub kind: &'a str,
    /// Built-in action of a task step.
    pub action: Option<&'a str>,
    /// The step's parameters, as resolved for this run.
    pub params: &'a Value,
}

/// The future a guard returns: its verdict.
pub type GuardFuture<'a> = Pin<Box<dyn Future<Output = GuardVerdict> + Send + 'a>>;

/// A process's answer to "this run has just done the same thing twice".
pub trait RunGuard: Send + Sync {
    /// Decide whether the run may continue past the step it just finished.
    fn check<'a>(&'a self, step: GuardStep<'a>) -> GuardFuture<'a>;
}

/// The guard of a process that installs none: every run continues.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoRunGuard;

impl RunGuard for NoRunGuard {
    fn check<'a>(&'a self, _step: GuardStep<'a>) -> GuardFuture<'a> {
        Box::pin(async move { GuardVerdict::clear() })
    }
}

/// A guard built from a closure, for a process with one rule to check.
pub struct FnRunGuard<F>(pub F)
where
    F: for<'a> Fn(GuardStep<'a>) -> GuardFuture<'a> + Send + Sync;

impl<F> RunGuard for FnRunGuard<F>
where
    F: for<'a> Fn(GuardStep<'a>) -> GuardFuture<'a> + Send + Sync,
{
    fn check<'a>(&'a self, step: GuardStep<'a>) -> GuardFuture<'a> {
        (self.0)(step)
    }
}

/// Ask a guard about a step and log the question.
///
/// Kept next to the trait rather than in the engine so the *shape* of the call — one
/// place, one argument list, one log line — is visible with the thing it exists for.
///
/// The step is logged **before** it is handed over, because the guard's future owns the
/// borrow for its whole life: anything that reads the step afterwards is reading a value
/// that has moved, and the only reason it would want to is to log it.
pub async fn check_run(guard: &dyn RunGuard, step: GuardStep<'_>) -> GuardVerdict {
    tracing::debug!(
        execution_id = %step.execution_id,
        step_no = step.step_no,
        kind = step.kind,
        action = step.action.unwrap_or("-"),
        "a run guard is being asked about this step"
    );

    guard.check(step).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step() -> GuardStep<'static> {
        static PARAMS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        let params = PARAMS.get_or_init(|| json!({ "slug": "home" }));
        GuardStep {
            execution_id: Uuid::nil(),
            step_no: 2,
            kind: "task",
            action: Some("publish_page"),
            params,
        }
    }

    #[tokio::test]
    async fn a_process_without_a_guard_lets_every_run_through() {
        // The engine in isolation must not stop runs: a synthetic `wait` step twice in a
        // row is a legitimate definition in a process that is not running automations.
        let verdict = check_run(&NoRunGuard, step()).await;
        assert!(!verdict.stop);
        assert!(verdict.reason.is_none());
    }

    #[tokio::test]
    async fn a_verdict_that_was_never_built_lets_the_run_go_on() {
        // A bug in a guard must fail open, not closed: a verdict that defaulted to
        // `stop` would turn any mistake in the automation layer into every run halting.
        let verdict = GuardVerdict::default();
        assert!(!verdict.stop);
        assert!(verdict.reason.is_none());
        assert!(!GuardVerdict::clear().stop);
    }

    #[tokio::test]
    async fn a_guard_that_stops_a_run_carries_the_reason_an_operator_reads() {
        let guard = FnRunGuard(|_step: GuardStep<'_>| {
            Box::pin(async move { GuardVerdict::stop("the rule is repeating itself") })
                as GuardFuture<'_>
        });

        let verdict = check_run(&guard, step()).await;
        assert!(verdict.stop);
        assert_eq!(
            verdict.reason.as_deref(),
            Some("the rule is repeating itself")
        );
    }
}
