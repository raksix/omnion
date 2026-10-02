/**
 * Tests for what a clicked node opens.
 *
 * The cases are the ways this is wrong *invisibly*: a trace that silently shows one branch
 * of a diverged node, a panel that cannot tell "no run" from "this node did nothing", and a
 * payload viewer that throws on the one value that breaks `JSON.stringify`. Each produces a
 * panel that looks correct while answering the wrong question.
 */

import { strict as assert } from "node:assert";
import { test } from "node:test";

import {
  describePayload,
  runDetailForNode,
  traceHeading,
  traceSubheading,
  type RunStepDetail,
} from "./step-detail.ts";
import type { RunStep } from "./node-status.ts";

function step(over: Partial<RunStep> & { step_no: number; status: string }): RunStep {
  return { node_id: `n${over.step_no}`, ...over } as RunStep;
}

test("no run at all is its own state, not an empty step list", () => {
  // The distinction `find` throws away. `null` is "nothing has been read"; `[]` is "the run
  // had no steps". Collapsing them makes a rule that has never run look like a rule whose
  // nodes all sat out.
  const none = runDetailForNode("n1", null);
  assert.equal(none.kind, "no-run");
  assert.match(traceSubheading(none), /Run this rule/);

  const empty = runDetailForNode("n1", []);
  assert.equal(empty.kind, "node-absent");
});

test("a node the run never reached says so instead of showing an empty viewer", () => {
  const detail = runDetailForNode("trigger-1", [step({ step_no: 1, node_id: "n1", status: "succeeded" })]);
  assert.equal(detail.kind, "node-absent");
  if (detail.kind !== "node-absent") return;
  assert.equal(detail.nodeId, "trigger-1");
  assert.match(traceHeading(detail), /took no part/);
});

test("clicking a node opens that step's inputs and output", () => {
  const detail = runDetailForNode("n2", [
    step({ step_no: 1, node_id: "n1", status: "succeeded" }),
    {
      step_no: 2,
      node_id: "n2",
      status: "succeeded",
      params: { url: "https://example.test/hook", retries: 3 },
      output: { status: 200, body: "ok" },
    } as RunStep,
  ]);

  assert.equal(detail.kind, "node");
  if (detail.kind !== "node") return;
  assert.equal(detail.steps.length, 1);
  const only = detail.steps[0];
  assert.equal(only.inputs.shape, "object");
  assert.deepEqual(
    only.inputs.entries.map((entry) => entry.key),
    ["url", "retries"],
  );
  assert.match(only.inputs.entries[0].value, /example\.test/);
  assert.deepEqual(
    only.output.entries.map((entry) => entry.key),
    ["status", "body"],
  );
  assert.equal(traceHeading(detail), "Step 2");
});

test("a node with two branches opens BOTH, not the one the array returned first", () => {
  // The whole reason the mapping is a list rather than a step. A `find` here shows the
  // branch that ran and hides the one that did not — which is the exact information the
  // diverged pill exists to advertise.
  const detail = runDetailForNode("n3", [
    step({ step_no: 2, node_id: "n3", status: "succeeded", output: { value: 1 } }),
    {
      step_no: 4,
      node_id: "n3",
      status: "skipped",
      skip_reason: "The success branch was taken.",
    } as RunStep,
  ]);

  assert.equal(detail.kind, "node");
  if (detail.kind !== "node") return;
  assert.equal(detail.steps.length, 2);
  assert.equal(detail.diverged, true);
  // In run order, not in the order the server happened to send them.
  assert.deepEqual(detail.steps.map((entry) => entry.step.step_no), [2, 4]);
  assert.match(traceHeading(detail), /branches/);
  // The subheading is the run's own reason for the unrun side, not a summary written here.
  assert.equal(traceSubheading(detail), "The success branch was taken.");
});

test("steps behind a node come back in run order whatever order they arrived in", () => {
  const detail = runDetailForNode("n1", [
    step({ step_no: 7, node_id: "n1", status: "succeeded" }),
    step({ step_no: 3, node_id: "n1", status: "succeeded" }),
  ]);
  if (detail.kind !== "node") return assert.fail("expected a node");
  assert.deepEqual(detail.steps.map((entry) => entry.step.step_no), [3, 7]);
});

test("steps with no node never land in another node's trace", () => {
  // A rule predating the builder has steps with no `node_id`. Filtered by equality here, so
  // they cannot be attributed to whichever node happens to sit at the same index.
  const detail = runDetailForNode("n1", [
    { step_no: 1, node_id: null, status: "succeeded" } as unknown as RunStep,
    step({ step_no: 2, node_id: "n1", status: "succeeded" }),
  ]);
  if (detail.kind !== "node") return assert.fail("expected a node");
  assert.equal(detail.steps.length, 1);
  assert.equal(detail.steps[0].step.step_no, 2);
});

test("an absent payload and an empty one are different sentences", () => {
  // A viewer that only checks for emptiness cannot tell "the step ran and returned nothing"
  // from "the step never produced anything", and those are different debugging facts.
  const absent = describePayload(undefined);
  const empty = describePayload({});
  assert.equal(absent.hasContent, false);
  assert.equal(empty.hasContent, true);
  assert.equal(empty.shape, "object");
  assert.match(empty.headline, /ran and returned nothing/);
});

test("a null payload is absent, and an empty list is a list", () => {
  assert.equal(describePayload(null).hasContent, false);
  const list = describePayload([]);
  assert.equal(list.shape, "array");
  assert.equal(list.hasContent, true);
  assert.match(list.headline, /empty list/);
});

test("a payload that is not an object is described in words", () => {
  const scalar = describePayload(42);
  assert.equal(scalar.shape, "scalar");
  assert.match(scalar.headline, /42/);
  assert.match(scalar.headline, /single number/);

  const array = describePayload([1, 2, 3]);
  assert.equal(array.shape, "array");
  assert.deepEqual(array.items, ["1", "2", "3"]);
  assert.match(array.headline, /3 items/);
});

test("a long string is truncated with a visible marker, not silently cut", () => {
  // A reader who sees `…` knows there is more. A value that looks complete and is not is
  // the kind of lie that sends an operator looking in the wrong place.
  const long = "x".repeat(5000);
  const described = describePayload({ body: long });
  const body = described.entries[0].value;
  assert.match(body, /… \(5000 characters\)/);
});

test("a deep payload is capped instead of recursing without end", () => {
  let deep: Record<string, unknown> = { leaf: true };
  for (let i = 0; i < 12; i += 1) deep = { nested: deep };
  const described = describePayload({ payload: deep });
  // The nested value renders as ONE summary line, however deep it goes. The alternative —
  // expanding a payload until it ends — is a page that never finishes rendering on a step
  // that returned a tree.
  assert.equal(described.entries.length, 1);
  assert.equal(described.entries[0].key, "payload");
  assert.match(described.entries[0].value, /^\{\d+ keys?\}$/);
});

test("a cyclic payload describes without throwing", () => {
  // `JSON.stringify` on this throws, and it throws inside a render — which takes the whole
  // panel with it. The classifier never calls it.
  const cyclic: Record<string, unknown> = { name: "loop" };
  cyclic.self = cyclic;
  const described = describePayload(cyclic);
  assert.equal(described.shape, "object");
  assert.equal(described.entries.length, 2);
  assert.match(described.entries[1].value, /…|nested|\{/);
});

test("the trace panel names the step it opened", () => {
  const detail = runDetailForNode("n2", [step({ step_no: 5, node_id: "n2", status: "failed" })]);
  if (detail.kind !== "node") return assert.fail("expected a node");
  assert.equal(traceHeading(detail), "Step 5");
  assert.match(traceSubheading(detail), /1 step behind/);
});

test("a single step reads in the singular", () => {
  const detail: RunStepDetail[] = [];
  const one = runDetailForNode("n1", [step({ step_no: 1, node_id: "n1", status: "succeeded" })]);
  if (one.kind !== "node") return assert.fail("expected a node");
  assert.equal(traceSubheading(one), "1 step behind this node.");
  assert.equal(detail.length, 0);
});
