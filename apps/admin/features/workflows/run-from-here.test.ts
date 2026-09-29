import { strict as assert } from "node:assert";
import test from "node:test";

import { startability, startMessage, type StartabilityNode } from "./run-from-here.ts";

function node(over: Partial<StartabilityNode> = {}): StartabilityNode {
  return { id: "n1", type: "action", inert: false, ...over };
}

test("a runnable node offers the button", () => {
  const answer = startability(node(), false);
  assert.equal(answer.canStart, true);
  assert.equal(answer.reason, null);
});

test("a trigger offers the button, because re-running from the top is real", () => {
  const answer = startability(node({ type: "trigger.manual" }), false);
  assert.equal(
    answer.canStart,
    true,
    "a canvas that greys out its own trigger cannot express 'run the whole rule'",
  );
});

test("an inert node in the middle offers the button", () => {
  const answer = startability(node({ type: "note", inert: true }), false);
  assert.equal(
    answer.canStart,
    true,
    "'start here' on a note means 'start after it', which the server resolves",
  );
});

test("the end node refuses, and says what to do instead", () => {
  const answer = startability(node({ type: "end" }), true);
  assert.equal(answer.canStart, false);
  assert.match(answer.reason ?? "", /nothing after it/i);
  assert.match(answer.reason ?? "", /one node before/i, "a refusal must offer the way out");
});

test("an inert node at the very end refuses, because nothing follows it", () => {
  // The same outcome as the end node, reached by a different route. Testing only the end
  // node would leave this live button unproven — and it is the one that produces a run
  // settling `completed` having done nothing.
  const answer = startability(node({ type: "note", inert: true }), true);
  assert.equal(answer.canStart, false);
  assert.match(answer.reason ?? "", /no step to run/i);
});

test("an inert last node is refused but a trigger in the same position is not", () => {
  // A trigger is the one node with no step that is still startable, so the two cases must
  // be told apart by *what* the node is, not by whether it contributes a step.
  const inert = startability(node({ type: "note", inert: true }), true);
  const trigger = startability(node({ type: "trigger.schedule", inert: true }), true);
  assert.equal(inert.canStart, false);
  assert.equal(trigger.canStart, true);
});

test("the message names the node, because 'run started' says nothing about which run", () => {
  assert.equal(startMessage("Transform", []), "Run started at Transform.");
  assert.equal(
    startMessage("Transform", [{ name: "Fetch" }]),
    "Run started at Transform. Fetch was skipped.",
  );
  assert.equal(
    startMessage("Notify", [{ name: "Fetch" }, { name: "Transform" }]),
    "Run started at Notify. Fetch and Transform were skipped.",
  );
});

test("a three-step skip reads as a list, not a run-on", () => {
  assert.equal(
    startMessage("Notify", [{ name: "a" }, { name: "b" }, { name: "c" }]),
    "Run started at Notify. a, b and c were skipped.",
  );
});
