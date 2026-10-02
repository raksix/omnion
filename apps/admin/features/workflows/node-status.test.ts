/**
 * Tests for the node status pill.
 *
 * The cases are chosen for the ways this is wrong *visibly*: a canvas that paints a node
 * the run never touched, a node that branched where only one side is shown, and a step
 * attributed to a node it did not come from. Each of those produces a screenshot that
 * looks plausible.
 */

import { strict as assert } from "node:assert";
import { test } from "node:test";

import {
  indexStepsByNode,
  nodeRunStatus,
  pillLabel,
  pillText,
  type RunStep,
} from "./node-status.ts";

function step(over: Partial<RunStep> & { step_no: number; status: string }): RunStep {
  return { node_id: `n${over.step_no}`, ...over };
}

test("a node the run never reached paints nothing", () => {
  // The rule that matters most. A pill on a node that took no part in the run is a
  // claim about work the engine never did, and nothing on screen distinguishes it from a
  // real one.
  const status = nodeRunStatus([]);
  assert.equal(status.status, null);
  assert.equal(status.shape, "none");
  assert.equal(pillLabel(status), null);
  assert.equal(pillText(status), null);
});

test("a single step paints its own status", () => {
  const status = nodeRunStatus([step({ step_no: 2, status: "succeeded" })]);
  assert.equal(status.status, "succeeded");
  assert.equal(status.shape, "single");
  assert.equal(pillLabel(status), "Done");
  assert.equal(pillText(status), "Succeeded");
});

test("a skipped step carries the run's own reason, unrewritten", () => {
  // The criterion says the trace says *why*. A client-written summary is a second
  // sentence that can disagree with the server's, so this asserts the exact string.
  const reason = 'the run was started at "c" further down the graph';
  const status = nodeRunStatus([
    step({ step_no: 1, status: "skipped", skip_reason: reason }),
  ]);
  assert.equal(status.status, "skipped");
  assert.equal(status.skipReason, reason);
  assert.equal(pillText(status), reason);
});

test("a node whose branches disagreed is diverged, not the branch that ran", () => {
  // `success` and `error` are two steps of the *same* node. After a run one is succeeded
  // and the other skipped. Painting the succeeded one is true and incomplete; a Map keyed
  // by node would silently keep whichever row came last, which makes the answer depend on
  // the order the API returned.
  const status = nodeRunStatus([
    step({ step_no: 1, status: "succeeded", node_id: "b" }),
    step({ step_no: 2, status: "skipped", node_id: "b", skip_reason: "the other branch was not taken" }),
  ]);
  assert.equal(status.status, "diverged");
  assert.equal(status.shape, "diverged");
  assert.equal(status.stepNos.length, 2);
  // The unrun side's reason is the one an operator needs — the settled side explains itself.
  assert.equal(status.skipReason, "the other branch was not taken");
  assert.equal(pillLabel(status), "Diverged");
});

test("two steps that agree collapse to one pill", () => {
  const status = nodeRunStatus([
    step({ step_no: 1, status: "succeeded", node_id: "b" }),
    step({ step_no: 2, status: "succeeded", node_id: "b" }),
  ]);
  assert.equal(status.status, "succeeded");
  assert.equal(status.shape, "single");
  // Both numbers are still carried: the inspector opens one of them and the other is a real
  // step, not a duplicate to be dropped.
  assert.deepEqual(status.stepNos, [1, 2]);
});

test("a node whose steps are mid-run is running, not pending", () => {
  // Ordering trap: sorted alphabetically `pending` < `running`, so a naive collapse picks
  // the one that is furthest from happening.
  const status = nodeRunStatus([
    step({ step_no: 1, status: "pending", node_id: "b" }),
    step({ step_no: 2, status: "running", node_id: "b" }),
  ]);
  assert.equal(status.status, "running");
});

test("steps with no node are not attributed to one", () => {
  // A rule whose definition predates the builder has no node behind its steps. Bucketing
  // them under an empty key would paint the first card on the canvas, which is worse than
  // painting none.
  const byNode = indexStepsByNode([
    step({ step_no: 1, status: "succeeded", node_id: null }),
    step({ step_no: 2, status: "succeeded", node_id: "a" }),
  ]);
  assert.equal(byNode.has(""), false, "no bucket under an empty key");
  assert.equal(byNode.get("a")?.length, 1);
  assert.equal(byNode.size, 1);
});

test("the index groups every step of a node, not just the first", () => {
  const byNode = indexStepsByNode([
    step({ step_no: 1, status: "succeeded", node_id: "a" }),
    step({ step_no: 2, status: "skipped", node_id: "a", skip_reason: "r" }),
    step({ step_no: 3, status: "succeeded", node_id: "b" }),
  ]);
  assert.equal(byNode.get("a")?.length, 2);
  assert.equal(byNode.get("b")?.length, 1);
});

test("every status the engine stores has a label and a sentence", () => {
  // A status that renders as an empty pill is a dead control. This walks the whole
  // vocabulary the migration allows, so adding one without a label fails here.
  const every = [
    "pending",
    "running",
    "waiting",
    "succeeded",
    "failed",
    "cancelled",
    "skipped",
  ];
  for (const value of every) {
    const status = nodeRunStatus([step({ step_no: 1, status: value })]);
    assert.ok(pillLabel(status), `no label for ${value}`);
    assert.ok(pillText(status), `no sentence for ${value}`);
  }
});

test("a skipped step with no reason still says something", () => {
  // The database refuses a skipped step without a reason, so this cannot reach the
  // canvas. The assertion is on the shape that does exist: a missing reason falls back
  // rather than rendering an empty pill.
  const status = nodeRunStatus([step({ step_no: 1, status: "skipped", skip_reason: null })]);
  assert.equal(status.status, "skipped");
  assert.ok(pillText(status), "a pill with no words is a dead pill");
});
