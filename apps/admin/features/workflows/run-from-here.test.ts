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

test("an unconnected node is refused, and the reason points at the wiring", () => {
  // **The defect.** An unconnected node answers `isLastOnPath: true` (nothing leaves it)
  // and is not inert — and the old rule read exactly those two facts, so it answered
  // `canStart: true`. The server's walk has never reached it, so every press came back
  // `unknown_node`. Both questions have to be asked: "nothing leaves it" made it *look*
  // like an end, and "not inert" was the only thing that let the button through.
  const answer = startability(node({ type: "action", reachedByTrigger: false }), true);
  assert.equal(answer.canStart, false, "an unconnected node cannot start a run");
  assert.match(answer.reason ?? "", /trigger/, "the reason names what does not reach it");
  assert.match(
    answer.reason ?? "",
    /connect/i,
    "and it points at the fix — the wiring — rather than restating the symptom",
  );
});

test("a node wired onward but unreachable from the trigger is refused too", () => {
  // The other direction, and the one `isLastOnPath` alone could never catch: a node with a
  // connection leaving it (so not "last") that the engine's walk never reaches — a
  // condition whose only arm is `false`. It looks perfectly connected.
  const answer = startability(node({ type: "action", reachedByTrigger: false }), false);
  assert.equal(answer.canStart, false);
  assert.match(answer.reason ?? "", /trigger/);
});

test("an unknown reachability offers the button, because 'cannot tell' is not 'no'", () => {
  // A graph with no trigger has no walk. Greying out the control on every card of a
  // half-written rule is the same teaching-a-user-to-click-anywhere failure as the
  // opposite, and the press goes to the server either way — it is the authority.
  assert.equal(startability(node({ reachedByTrigger: null }), false).canStart, true);
  assert.equal(
    startability(node({ reachedByTrigger: undefined }), false).canStart,
    true,
    "an absent verdict is the same as not knowing",
  );
});

test("the end node is refused even when the walk reaches it", () => {
  // Ordering matters: the end refusal is the more specific sentence, and it is what the
  // author needs to read on the one card they are most likely to press.
  const answer = startability(node({ type: "end", reachedByTrigger: true }), true);
  assert.equal(answer.canStart, false);
  assert.match(answer.reason ?? "", /nothing after it/i);
  assert.doesNotMatch(
    answer.reason ?? "",
    /trigger/i,
    "an end node is reachable and terminal; 'nothing reaches it' would be a lie",
  );
});

test("a reachable, connected, non-terminal node offers the button", () => {
  assert.equal(
    startability(node({ type: "action", reachedByTrigger: true }), false).canStart,
    true,
  );
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
