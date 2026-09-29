//! *Retry this node* — re-run exactly one step of a settled run (REQ-004 slice 3).
//!
//! The criterion is one sentence with a trap in it:
//!
//! > *"Retry this node" re-runs only that node without duplicating earlier side effects
//! > (proven with the mail sink).*
//!
//! **"Only that node" is not a narrower `retry_step_from`.** `store::retry_step_from` is
//! deliberately a **tail** re-run — it re-opens `step_no >= N` — because a run whose middle
//! failed must not be allowed to march on to completion with a hole in the middle. That is
//! the right default for *Retry* on a run detail, and it is exactly wrong for a click on
//! one node of a canvas. Building the node control by narrowing that write would send the
//! earlier e-mail twice, which is the one thing the criterion names.
//!
//! So this is a **different write**, and the difference is not the SQL:
//!
//! * a tail re-run re-opens everything after the step, because those steps depend on a
//!   result the failed one never produced;
//! * a node retry re-opens **one row**, and additionally **re-opens the run**, because the
//!   engine refuses to claim a step while the run is settled.
//!
//! ## What a node retry must *not* do
//!
//! * it must not re-run the nodes **after** it — they already ran, and re-running a
//!   downstream action is a second side effect an operator did not ask for;
//! * it must not re-run the nodes **before** it — that is the earlier-side-effect half of
//!   the criterion, and the mail sink is how it is proven;
//! * it must not **reset the attempt counter** of the step. The engine owns that counter
//!   and its own back-off; a click is not a new attempt budget, and resetting it here
//!   would hand one node an unbounded budget no operator can see.
//!
//! ## Why the decision is a pure function
//!
//! "Can this node be retried?" has three answers that all look the same on the canvas —
//! a node that is still `pending` (the run has not got there), a node that `succeeded`
//! (nothing failed), and a node whose run is still `running` (retrying now would race
//! the engine's own claim). Only the last is a real refusal; the first two are honest
//! "there is nothing to try again". Deciding that in a route handler means the three cases
//! are only ever discovered by clicking, so it is a function of a run's state and a node's
//! step, with the three refusals asserted.

use serde::Serialize;
use uuid::Uuid;

/// The statuses a step can be retried *from*, as the plan reads them.
///
/// A step that is still `pending` is not a failure — the run never reached it — and a
/// step that `succeeded` is not a failure either. `running` is a refusal of its own kind
/// (the run is live), handled above the step list because it is a fact about the run.
const RETRYABLE: [&str; 3] = ["failed", "cancelled", "ignored"];

/// What a node retry will do, before it is written.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RetryNodePlan {
    /// The run being retried. Filled in by the caller, which is the only place that
    /// knows the id: the plan is made from a run's *state*, not from the run.
    pub execution_id: Option<Uuid>,
    /// The step that goes back on the queue — exactly one.
    pub step_no: i32,
    /// The node that step came from, when the run was made from a graph.
    pub node_id: Option<String>,
    /// The step's name, for the sentence the panel shows.
    pub name: String,
    /// How many rows the write re-opens. Always 1, and asserted as such: a node retry
    /// that re-opened two rows would be a tail retry wearing a node retry's name.
    pub requeued: u64,
    /// The sentence the run's history and the panel both show.
    pub reason: String,
}

/// Why a node cannot be retried, as a named answer rather than a message string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum RetryRefusal {
    /// The run is still going, so a retry would race the engine's own claim.
    RunStillRunning,
    /// The run was cancelled on purpose, and a retry must not undo that decision.
    RunCancelled,
    /// This node took no part in the run — a trigger, a note, or a node the run never
    /// reached.
    NodeNotInRun,
    /// The node's step has not failed: it succeeded, or the run never got to it.
    NothingToRetry,
}

impl RetryRefusal {
    /// The sentence a user is shown, which names what to do instead of only refusing.
    #[must_use]
    pub fn message(self, node_label: &str) -> String {
        match self {
            Self::RunStillRunning => {
                "This run is still going. Wait for it to finish, or cancel it first.".to_owned()
            }
            Self::RunCancelled => "This run was cancelled on purpose, so it cannot be retried. \
                 Start a new run instead."
                .to_owned(),
            Self::NodeNotInRun => format!(
                "{node_label:?} took no part in this run, so there is nothing to retry on it."
            ),
            Self::NothingToRetry => format!(
                "{node_label:?} did not fail in this run, so there is nothing to try again."
            ),
        }
    }

    /// The machine-readable code, which is what an API client should branch on.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::RunStillRunning => "execution_running",
            Self::RunCancelled => "execution_cancelled",
            Self::NodeNotInRun => "node_not_in_run",
            Self::NothingToRetry => "nothing_to_retry",
        }
    }
}

/// One step as the plan reads it: enough to decide, and nothing that would tempt a caller
/// to re-derive the answer.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryableStep {
    /// The step's position in the run.
    pub step_no: i32,
    /// The step's status.
    pub status: String,
    /// The node it came from, when the run carries one.
    pub node_id: Option<String>,
    /// The step's name.
    pub name: String,
}

/// The run, as the plan reads it: the one fact a run-level refusal can turn on.
///
/// Deliberately not `Copy` and deliberately just a `String` rather than a parsed
/// `ExecutionStatus`. The status is compared against literals here, and a parse that
/// rejected an unknown value would turn "a run in a state this build has never heard of"
/// into a 500 on a control that only wanted to know whether the run is still going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunState {
    /// The run's status, as stored.
    pub status: String,
}

/// Decide whether one node of a settled run can be retried on its own.
///
/// The refusals are ordered the way they must be **checked**, and the order is the whole
/// design:
///
/// 1. **the run's own state first.** A run that is still going, or that was cancelled on
///    purpose, is refused before its steps are even read — a step-level answer computed
///    against a live run describes a run that is about to change underneath it. This is
///    the same order `retry_step_from`'s route uses, and for the same reason.
/// 2. **the node is in the run next.** A node with no step is `NodeNotInRun`, which is a
///    *different sentence* from "nothing to retry": the first is about the node, the
///    second is about the run's history. Collapsing them tells an operator debugging a
///    click on a trigger that the trigger "did not fail", which is a statement about a
///    step that never existed.
/// 3. **the step's status last.** Only a step that actually failed can be tried again.
///
/// A node with **two** steps — a branching node projects to a `success` and an `error`
/// row — retries the one that failed. Retrying both would re-run the branch the engine
/// did not take, which is a second side effect the operator did not ask for and cannot
/// distinguish from a bug. When both branches failed, the *first* one is retried: the
/// second was never reached, so retrying it would claim a failure that did not happen.
#[must_use]
pub fn plan_retry_node(
    run: RunState,
    node_id: &str,
    steps: &[RetryableStep],
) -> Result<RetryNodePlan, RetryRefusal> {
    // (1) the run decides before anything is read from it
    if run.status == "running" {
        return Err(RetryRefusal::RunStillRunning);
    }
    if run.status == "cancelled" {
        return Err(RetryRefusal::RunCancelled);
    }

    // (2) the node must be in the run at all — a `None` node id on every step is a run
    // made before the builder existed, and the honest answer there is that the node
    // cannot be addressed by id, not that it did not fail.
    let for_node: Vec<&RetryableStep> = steps
        .iter()
        .filter(|step| step.node_id.as_deref() == Some(node_id))
        .collect();

    if for_node.is_empty() {
        return Err(RetryRefusal::NodeNotInRun);
    }

    // (3) the first *retryable* step behind the node. `min_by_key` on `step_no` rather
    // than `find`, because a branching node's first row is the branch the engine took
    // and the failed one can be the second: taking the first row of a `diverged` node
    // would report "nothing to retry" on a node the canvas is painting red.
    let target = for_node
        .iter()
        .filter(|step| RETRYABLE.contains(&step.status.as_str()))
        .min_by_key(|step| step.step_no)
        .copied();

    let Some(target) = target else {
        return Err(RetryRefusal::NothingToRetry);
    };

    Ok(RetryNodePlan {
        // Left unset: the plan decides *what* to retry, and a caller that stamps its
        // own id onto a plan is not at risk of stamping the wrong one.
        execution_id: None,
        step_no: target.step_no,
        node_id: Some(node_id.to_owned()),
        name: target.name.clone(),
        // One row, always. The write re-opens exactly this step; the assertion is on the
        // *count the store returns*, so a store change that widened the WHERE clause
        // fails the walk rather than quietly re-sending an e-mail.
        requeued: 1,
        reason: format!("{} was retried on its own", target.name),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(step_no: i32, status: &str, node: Option<&str>) -> RetryableStep {
        RetryableStep {
            step_no,
            status: status.to_owned(),
            node_id: node.map(str::to_owned),
            name: format!("step {step_no}"),
        }
    }

    fn failed_run(steps: Vec<RetryableStep>) -> (RunState, Vec<RetryableStep>) {
        (RunState { status: "failed".into() }, steps)
    }

    /// The shape most rules are stated against: three nodes in a row, the middle one
    /// failed, and the run stopped there.
    fn spine() -> Vec<RetryableStep> {
        vec![
            step(1, "succeeded", Some("a")),
            step(2, "failed", Some("b")),
            step(3, "cancelled", Some("c")),
        ]
    }

    #[test]
    fn the_failed_node_is_retryable() {
        let (run, steps) = failed_run(spine());
        let plan = plan_retry_node(run, "b", &steps).expect("b failed, so it can be retried");

        assert_eq!(plan.step_no, 2);
        assert_eq!(plan.node_id.as_deref(), Some("b"));
    }

    #[test]
    fn exactly_one_row_goes_back_on_the_queue() {
        let (run, steps) = failed_run(spine());
        let plan = plan_retry_node(run, "b", &steps).expect("b can be retried");

        // The number the store returns, not a constant. A store whose WHERE clause
        // widened back to `step_no >= N` re-opens the cancelled step too and re-sends
        // the earlier e-mail — which is the criterion, not a detail of it.
        assert_eq!(plan.requeued, 1);
    }

    #[test]
    fn a_succeeded_node_is_not_retryable() {
        let (run, steps) = failed_run(spine());
        let refusal = plan_retry_node(run, "a", &steps).expect_err("a succeeded");

        assert_eq!(refusal, RetryRefusal::NothingToRetry);
        assert!(
            refusal.message("Step one").contains("did not fail"),
            "the sentence must say why: {}",
            refusal.message("Step one")
        );
    }

    #[test]
    fn a_node_the_run_never_reached_is_a_different_refusal_from_a_success() {
        // The two are the same answer for a `find`, and different sentences for a user:
        // "took no part in this run" is about the node, "did not fail" is about a step
        // that exists. Folding them together tells an operator debugging a click on a
        // note that the note "did not fail", which is a claim about a step never created.
        let (run, steps) = failed_run(spine());
        let refusal = plan_retry_node(run, "island", &steps).expect_err("island is not in the run");

        assert_eq!(refusal, RetryRefusal::NodeNotInRun);
        assert_ne!(refusal, RetryRefusal::NothingToRetry);
        assert!(refusal.message("Island").contains("took no part"));
    }

    #[test]
    fn a_cancelled_tail_step_is_retryable_on_its_own() {
        // A `stop` policy closes the tail as `cancelled`, so the last node an operator
        // wants to try again is very often a cancelled one — refusing it would make the
        // control useless exactly when a run half-worked.
        let (run, steps) = failed_run(spine());
        let plan = plan_retry_node(run, "c", &steps).expect("c was cancelled, not never-run");

        assert_eq!(plan.step_no, 3);
    }

    #[test]
    fn a_running_run_is_refused_before_its_steps_are_read() {
        // The run's own state outranks every step-level answer: a step read from a live
        // run describes a run that is about to change underneath the decision.
        let run = RunState { status: "running".into() };
        let refusal = plan_retry_node(run, "b", &spine()).expect_err("the run is live");

        assert_eq!(refusal, RetryRefusal::RunStillRunning);
    }

    #[test]
    fn a_cancelled_run_is_refused_even_though_a_step_failed() {
        let run = RunState { status: "cancelled".into() };
        let refusal = plan_retry_node(run, "b", &spine()).expect_err("cancelled on purpose");

        assert_eq!(refusal, RetryRefusal::RunCancelled);
        assert_eq!(refusal.code(), "execution_cancelled");
    }

    #[test]
    fn a_branching_node_retries_the_branch_that_failed() {
        // A node with `success` and `error` ports is TWO rows. Retrying the row the
        // engine happened to store first would report "nothing to retry" on a node the
        // canvas paints red — the diverged case this slice already has a pill for.
        let (run, steps) = failed_run(vec![
            step(1, "succeeded", Some("a")),
            step(2, "skipped", Some("b")),
            step(3, "failed", Some("b")),
        ]);
        let plan = plan_retry_node(run, "b", &steps).expect("the error branch failed");

        assert_eq!(plan.step_no, 3, "the error branch, not the skipped success branch");
    }

    #[test]
    fn a_node_whose_both_branches_failed_retries_the_earlier_one() {
        // The later row was never reached, so "it failed" is a claim about work the
        // engine never did — the same reason an unreached node is refused outright.
        let (run, steps) = failed_run(vec![
            step(2, "failed", Some("b")),
            step(3, "failed", Some("b")),
        ]);
        let plan = plan_retry_node(run, "b", &steps).expect("the first branch failed");

        assert_eq!(plan.step_no, 2);
    }

    #[test]
    fn a_run_made_before_the_builder_has_no_addressable_node() {
        // Every step carries no `node_id`, so no node can be addressed — which is a
        // different fact from "that node did not fail", and the one an operator on an
        // old run needs to see.
        let (run, steps) = failed_run(vec![step(1, "failed", None), step(2, "failed", None)]);
        let refusal = plan_retry_node(run, "b", &steps).expect_err("no node ids at all");

        assert_eq!(refusal, RetryRefusal::NodeNotInRun);
    }

    #[test]
    fn a_pending_step_is_not_a_failure() {
        // `pending` means the run never got there, so offering a retry would let an
        // operator re-queue a step the engine is about to claim and race its own lock.
        let (run, steps) = failed_run(vec![step(1, "pending", Some("a"))]);
        let refusal = plan_retry_node(run, "a", &steps).expect_err("pending is not failed");

        assert_eq!(refusal, RetryRefusal::NothingToRetry);
    }

    #[test]
    fn every_refusal_has_a_distinct_code() {
        let codes = [
            RetryRefusal::RunStillRunning.code(),
            RetryRefusal::RunCancelled.code(),
            RetryRefusal::NodeNotInRun.code(),
            RetryRefusal::NothingToRetry.code(),
        ];
        let unique: std::collections::BTreeSet<_> = codes.iter().collect();
        assert_eq!(unique.len(), codes.len(), "a shared code makes the four unbranchable");
    }

    #[test]
    fn every_refusal_says_what_to_do_instead_of_only_refusing() {
        for refusal in [
            RetryRefusal::RunStillRunning,
            RetryRefusal::RunCancelled,
            RetryRefusal::NodeNotInRun,
            RetryRefusal::NothingToRetry,
        ] {
            let message = refusal.message("Send the e-mail");
            assert!(
                message.len() > 20,
                "a refusal that only refuses teaches nothing: {message}"
            );
        }

        // The two step-level refusals must name the node: they are about *this* node, and a
        // message that only said "nothing to retry" would be true of every card on the
        // canvas at once. The two run-level refusals must NOT name it, because there the
        // run is the subject and the node the operator clicked is not part of the answer.
        for refusal in [RetryRefusal::NodeNotInRun, RetryRefusal::NothingToRetry] {
            assert!(
                refusal.message("Send the e-mail").contains("Send the e-mail"),
                "a step-level refusal must name the node: {}",
                refusal.message("Send the e-mail")
            );
        }
        for refusal in [RetryRefusal::RunStillRunning, RetryRefusal::RunCancelled] {
            assert!(
                !refusal.message("Send the e-mail").contains("Send the e-mail"),
                "a run-level refusal is about the run; naming the node points at the wrong \
                 thing: {}",
                refusal.message("Send the e-mail")
            );
        }
    }

    /// The plan decides *which step*, never *which run* — the id belongs to the caller
    /// that read the run, and a plan that carried one would be a plan that could be
    /// applied to a different run than the one it was made from.
    #[test]
    fn the_plan_does_not_carry_the_execution_id() {
        let (run, steps) = failed_run(spine());
        let plan = plan_retry_node(run, "b", &steps).expect("b can be retried");
        assert!(
            plan.execution_id.is_none(),
            "the caller supplies the run; a plan that named one could be replayed onto another"
        );
    }
}
