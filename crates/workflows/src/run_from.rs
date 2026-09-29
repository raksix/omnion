//! *Run from here* — start a run at a mid-graph node (REQ-004 slice 3).
//!
//! The criterion is three sentences and each one has a trap:
//!
//!   1. the run's **first step is that node**;
//!   2. the **earlier nodes stay `skipped`**;
//!   3. the trace **says why**.
//!
//! The first two are one decision — which steps run, and in what order — and the third is
//! the part that is easy to satisfy by accident and impossible to satisfy honestly later.
//! "Skipped" on its own tells an operator that something was passed over; only a reason
//! tells them *that the run was started on purpose down here*, which is a completely
//! different fact from "the engine got lost".
//!
//! ## Why this is a pure function and not a route
//!
//! The plan is a decision about an ordered list, and every way of getting it wrong is
//! invisible from the outside: an off-by-one starts the run one node too early, a *strict*
//! comparison skips the target node itself, and both produce a run that completes. The
//! plan is therefore a function from a projected step list to a plan, and the tests
//! exercise it without a database — which is also why the three tricky cases
//! (the trigger, the last step, a node that projects to nothing) are cheap to state.
//!
//! ## Why the order is preserved rather than renumbered
//!
//! The plan marks rows and leaves `step_no` alone. Two reasons, and the second is the
//! real one: `claim_due_step` claims strictly in `step_no` order and refuses a step while
//! an earlier one is open, so renumbering would mean rewriting the *whole* prefix to make
//! room; and the trace is read against the definition it started with, where a step's
//! position is a fact about the graph. Marking is a `status` write; renumbering is a
//! rewrite of history that the UI would then have to un-do on the next edit.

use serde::Serialize;

use crate::definition::StepDefinition;
use crate::error::Result;
use crate::graph::Graph;
use crate::model::StepKind;

/// What a run-from-here decision looks like, before it is written.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunFromPlan {
    /// The node the run was started at, as the graph names it.
    pub node_id: String,
    /// Position in the run of the first step that runs, 1-based — the same number
    /// `step_no` carries, so the panel can say "starts at step 2" without a second count.
    pub first_step_index: Option<usize>,
    /// Steps before the start, which are skipped rather than run.
    pub skipped: Vec<SkippedStep>,
    /// Steps from the start on, which run in this order.
    pub runs: Vec<PlannedStep>,
    /// The sentence the trace shows, already in the author's language.
    pub reason: String,
}

/// One step the run passes over.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SkippedStep {
    /// 1-based position in the run, as stored.
    pub step_no: i32,
    /// The step's name, for the trace.
    pub name: String,
    /// The node it came from.
    pub node_id: String,
    /// Why it did not run.
    pub reason: String,
}

/// One step the run will run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlannedStep {
    /// 1-based position in the run, as stored — the position is kept, not renumbered.
    pub step_no: i32,
    /// The node it came from.
    pub node_id: String,
    /// The step itself, as the runner will receive it.
    pub step: StepDefinition,
}

/// Why the prefix was skipped, in a sentence the trace can print unchanged.
///
/// It names the node the run started at rather than saying "this step was skipped",
/// because those are different sentences and only one of them tells an operator whether
/// the run they are looking at is the run they asked for.
fn skip_reason(node_id: &str, from_label: &str) -> String {
    format!("the run was started at {from_label:?} ({node_id}) further down the graph")
}

/// Decide a run-from-here plan for one node of a graph.
///
/// The plan is made against [`crate::graph::project_walk`] rather than against the step
/// list, and that is the whole design. The two disagree in three places, and each one
/// decides whether a node is startable at all:
///
/// * a **trigger** contributes no step, and *Run from here* on the trigger is a real,
///   useful thing — an operator re-running a rule from the top without the event that
///   normally starts it;
/// * an **end** node contributes a `stop` step, and starting a run *at* it would leave
///   every earlier step pending forever;
/// * a **note** is inert and contributes nothing, yet it sits between two real steps on
///   the canvas, and clicking it must mean "start after what came before".
///
/// A planner that indexed the step list would need a special case for each of the three,
/// and a special case is where an off-by-one lives. Indexing the walk makes all three the
/// same arithmetic: find the node in the walk, skip every step before it, run every step
/// from it on.
///
/// The graph is walked *here* rather than accepted beside a caller-built step list. Two
/// arguments that are supposed to describe the same rule, and can disagree, are a public
/// invitation to disagree — and the failure mode is a plan that skips everything and runs
/// nothing, which is indistinguishable from a rule whose graph is empty.
pub fn plan_from_node(graph: &Graph, node_id: &str) -> Result<RunFromPlan> {
    let walk = crate::graph::project_walk(graph)?;

    // The node must be one the runner would actually reach. Checking it against the walk
    // rather than against `graph.nodes` is the point: a node on the canvas that the walk
    // never reaches is a node the runner would never have stepped through, and starting a
    // run there would promise steps the definition does not contain.
    let reached = walk
        .nodes
        .iter()
        .position(|walked| walked.node_id == node_id)
        .ok_or_else(|| {
            crate::error::WorkflowError::invalid(
                "unknown_node",
                format!(
                    "{node_id:?} is not on the path from the trigger, so a run cannot start \
                     there"
                ),
            )
        })?;

    let node = graph
        .node(node_id)
        .expect("the walk only yields nodes of this graph");
    let reason = skip_reason(node_id, &node.label);

    let mut skipped = Vec::new();
    let mut runs = Vec::new();

    // Positions come off the walk rather than being counted here. A run's `step_no` is
    // dense while the walk is not (the trigger and every note hold no position), so
    // counting while iterating would hand the engine a list whose numbering disagrees
    // with the definition — and the engine orders strictly by that column.
    for walked in walk.nodes[..reached].iter() {
        let (Some(step), Some(step_no)) = (walked.step.clone(), walked.step_no) else {
            continue;
        };
        skipped.push(SkippedStep {
            step_no,
            name: step.name.clone(),
            node_id: walked.node_id.clone(),
            reason: reason.clone(),
        });
    }

    for walked in walk.nodes[reached..].iter() {
        let (Some(step), Some(step_no)) = (walked.step.clone(), walked.step_no) else {
            // A node with no step is a position, not work. The trigger (nothing to run at
            // the top of the graph) and inert nodes (decoration) both land here, and
            // neither is skipped nor run — which is the only honest reading, because
            // inventing a step for them would put work in the run the graph does not have.
            continue;
        };
        runs.push(PlannedStep {
            step_no,
            node_id: walked.node_id.clone(),
            step,
        });
    }

    // Two ways a run can come out empty, and both are the end node: a graph that ends at
    // a `stop` step, and a graph whose last node is inert. Both are refused here rather
    // than special-cased in the walk, because a `stop` is a *reason to end* and a run that
    // starts at one has nothing after it. Answering either with an empty run would leave
    // an execution that settles `completed` having done nothing — the single most
    // misleading thing this feature could do.
    if runs.is_empty() || runs.iter().all(|entry| entry.step.kind == StepKind::Stop) {
        return Err(crate::error::WorkflowError::invalid(
            "nothing_to_run",
            format!(
                "{:?} ({node_id}) is where the graph ends, so a run started there would have \
                 nothing to do — start it at the node before",
                node.label
            ),
        ));
    }

    Ok(RunFromPlan {
        node_id: node_id.to_owned(),
        first_step_index: Some(runs[0].step_no as usize - 1),
        skipped,
        runs,
        reason,
    })
}

/// A run-from-here plan as the caller receives it, with the steps in run order.
///
/// `plan.runs` is already in order and already carries the stored `step_no`, so the
/// caller can hand the runner exactly the slice it needs without re-deriving anything.
#[must_use]
pub fn runnable_steps(plan: &RunFromPlan) -> Vec<StepDefinition> {
    plan.runs.iter().map(|entry| entry.step.clone()).collect()
}

/// The node ids a plan touches, in order, for the run's own record.
///
/// The skipped prefix is included deliberately: the trace resolves a node by id even for
/// a step that did not run, and a list that held only the running nodes would make the
/// prefix unresolvable.
#[must_use]
pub fn plan_node_ids(plan: &RunFromPlan) -> Vec<&str> {
    plan.skipped
        .iter()
        .map(|entry| entry.node_id.as_str())
        .chain(plan.runs.iter().map(|entry| entry.node_id.as_str()))
        .collect()
}

/// `true` when a step kind can be the first step of a run started at a node.
///
/// A `stop` step has nothing to do, and starting a run *at* one would settle the run
/// immediately with the other tail steps left pending forever. The refusal belongs here,
/// where the plan is made, rather than in the engine: an engine that discovers it must
/// clean up a run it already created.
#[must_use]
pub fn can_start_at(kind: StepKind) -> bool {
    !matches!(kind, StepKind::Stop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Edge, Graph, Node, Position};
    use serde_json::Value;

    fn node(id: &str, node_type: &str) -> Node {
        Node {
            id: id.to_owned(),
            node_type: node_type.to_owned(),
            label: id.to_owned(),
            params: Value::Object(serde_json::Map::new()),
            position: Position::default(),
        }
    }

    fn action(id: &str) -> Node {
        let mut node = node(id, "action");
        node.params = serde_json::json!({ "action": "echo", "parameters": { "value": 1 } });
        node
    }

    fn edge(id: &str, source: &str, port: &str, target: &str) -> Edge {
        Edge {
            id: id.to_owned(),
            source: source.to_owned(),
            source_port: port.to_owned(),
            target: target.to_owned(),
        }
    }

    /// trigger → a → b → c → end
    ///
    /// The starter's own `trigger → end` edge is *replaced*, not appended to. The walk in
    /// `graph::project` follows the first output edge it finds on a node, so a spine built
    /// by appending to the starter still jumps straight from the trigger to the end and
    /// projects one step — which reads as a planner bug and is really a test that built
    /// the wrong graph. The starter is also why the "start at the trigger" case is worth
    /// a test of its own: the trigger is the one node no spine edge leaves by accident.
    fn spine() -> Graph {
        let mut graph = Graph::starter("manual", None);
        graph.nodes.push(action("a"));
        graph.nodes.push(action("b"));
        graph.nodes.push(action("c"));
        graph.edges = vec![
            edge("e1", "trigger", "out", "a"),
            edge("e2", "a", "success", "b"),
            edge("e3", "b", "success", "c"),
            edge("e4", "c", "success", "end"),
        ];
        graph
    }

    #[test]
    fn the_first_step_of_a_run_from_here_is_the_node_that_was_clicked() {
        let graph = spine();
        let plan = plan_from_node(&graph, "b").expect("b can be started from");

        assert_eq!(plan.runs[0].node_id, "b");
        // `b` is the second step of the definition, and the run starts *at* it rather
        // than one past it. The off-by-one that put the start one node too late is the
        // exact failure this assertion exists for.
        assert_eq!(plan.runs[0].step_no, 2);
        assert_eq!(plan.first_step_index, Some(1));
    }

    #[test]
    fn the_earlier_steps_are_skipped_and_not_merely_absent() {
        let graph = spine();
        let plan = plan_from_node(&graph, "b").expect("b can be started from");

        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].node_id, "a");
        // The prefix keeps its stored position: the trace is read against the definition
        // the run started with, where a step's place is a fact about the graph, and the
        // engine orders by exactly this column.
        assert_eq!(plan.skipped[0].step_no, 1);
    }

    #[test]
    fn every_skipped_step_carries_a_reason_naming_the_node() {
        let graph = spine();
        let plan = plan_from_node(&graph, "c").expect("c can be started from");

        assert_eq!(plan.skipped.len(), 2);
        for entry in &plan.skipped {
            assert!(
                entry.reason.contains("\"c\""),
                "the reason must name the node the run started at: {}",
                entry.reason
            );
        }
    }

    #[test]
    fn a_run_started_at_the_trigger_skips_nothing() {
        let graph = spine();
        let plan = plan_from_node(&graph, "trigger").expect("the trigger is on the path");

        assert!(
            plan.skipped.is_empty(),
            "the trigger holds no step position"
        );
        assert_eq!(plan.runs.len(), 4, "a, b, c and the end's stop step");
        assert_eq!(plan.runs[0].node_id, "a");
        assert_eq!(plan.runs[0].step_no, 1);
    }

    #[test]
    fn the_end_node_cannot_be_started_because_nothing_would_run() {
        let graph = spine();
        let error = plan_from_node(&graph, "end")
            .expect_err("the end is a stop step, so a run there does nothing")
            .to_string();

        assert!(
            error.contains("nothing to do"),
            "the refusal must say what is wrong, not merely refuse: {error}"
        );
    }

    #[test]
    fn a_node_the_walk_never_reaches_is_refused_by_name() {
        // An orphan is refused by the validator, so a hand-built graph is the only way to
        // reach this case: the point is that a canvas node the runner would never step
        // through cannot be a start.
        let mut graph = spine();
        graph.nodes.push(action("island"));
        let error = plan_from_node(&graph, "island")
            .expect_err("a node off the path from the trigger cannot be started")
            .to_string();

        assert!(
            error.contains("island"),
            "the refusal must name it: {error}"
        );
    }

    #[test]
    fn a_node_id_the_graph_does_not_have_is_refused() {
        let graph = spine();
        let error = plan_from_node(&graph, "nope")
            .expect_err("a node id from another revision must not apply")
            .to_string();

        assert!(error.contains("nope"), "the refusal must name it: {error}");
    }

    #[test]
    fn the_runnable_slice_is_exactly_the_tail_in_order() {
        let graph = spine();
        let plan = plan_from_node(&graph, "b").expect("b can be started from");
        let runnable = runnable_steps(&plan);

        assert_eq!(runnable.len(), 3, "b, c and the end");
        assert_eq!(runnable[0].name, "b");
        assert_eq!(runnable[1].name, "c");
    }

    #[test]
    fn the_node_list_covers_the_skipped_prefix_too() {
        let graph = spine();
        let plan = plan_from_node(&graph, "c").expect("c can be started from");
        let ids = plan_node_ids(&plan);

        assert_eq!(ids, vec!["a", "b", "c", "end"]);
    }

    #[test]
    fn a_stop_step_cannot_be_the_start_of_a_run() {
        assert!(!can_start_at(StepKind::Stop));
        assert!(can_start_at(StepKind::Task));
        assert!(can_start_at(StepKind::Wait));
    }

    /// The `note` node type is the platform's only inert node, and the validator refuses a
    /// note with anything leaving it. That means a note is *always* a leaf, and can never
    /// be an inert position the walk reaches *after* a step — so "the clicked node has no
    /// step" is a case that cannot arrive mid-graph. The planner's skip-over is kept simple
    /// because the walk only ever yields a step-bearing node after the first one. What
    /// matters is proved where it can happen: a leaf note holds no step.
    #[test]
    fn a_note_is_inert_and_takes_no_step_position() {
        let registry = crate::graph::NODE_TYPES;
        let note_type = registry
            .iter()
            .find(|node_type| node_type.key == "note")
            .expect("the registry carries a note");
        assert!(note_type.inert, "a note is inert by definition");
        assert!(
            note_type.outputs.is_empty(),
            "which is why it can only ever be a leaf — nothing can leave it"
        );
    }
}
