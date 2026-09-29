/**
 * The connection rules' own tests (REQ-004 slice 2).
 *
 * Every one of these is a refusal or a success the acceptance criteria name: "a connection can
 * be drawn between compatible ports; an incompatible target refuses with a visible reason".
 * The regression this file exists for is a single word — the gesture carries a *node id*, the
 * ports live on a *type* — and a typecheck cannot see it, because both are strings. So the
 * first test is the one that failed in the browser: a node id that is not also a type key.
 */
import assert from "node:assert/strict";
import test from "node:test";

import {
  decideConnection,
  typeKeyOf,
  type ConnectableEdge,
  type ConnectableNode,
  type ConnectableType,
} from "./connect-edge.ts";

const types = new Map<string, ConnectableType>([
  ["wait", { label: "Wait", outputs: [{ key: "out", label: "After" }] }],
  ["transform", { label: "Transform", outputs: [{ key: "in", label: "In" }] }],
  ["end", { label: "End", outputs: [] }],
]);

const nodes: ConnectableNode[] = [
  { id: "wait-1", type: "wait" },
  { id: "transform-2", type: "transform" },
  { id: "end-3", type: "end" },
];

const decide = (source: string, port: string, target: string, edges: ConnectableEdge[] = []) =>
  decideConnection(source, port, target, nodes, types, edges, "edge-new");

test("a node id resolves to the type whose ports it exposes", () => {
  // The bug: "wait-1" is not a key in `types`, only "wait" is. A lookup of the id in the type
  // map returns undefined, and every port reads as absent.
  assert.equal(typeKeyOf("wait-1", nodes), "wait");
  assert.equal(typeKeyOf("wait", nodes), undefined);
});

test("a connection between two real nodes is allowed", () => {
  const result = decide("wait-1", "out", "transform-2");
  assert.equal(result.ok, true);
  if (!result.ok) {
    return;
  }
  assert.deepEqual(result.edge, {
    id: "edge-new",
    source: "wait-1",
    source_port: "out",
    target: "transform-2",
  });
  // The notice names both nodes by label, not by id — an id in user-facing copy reads as a bug.
  assert.equal(result.text, "Wait · After → Transform");
});

test("a node with no outputs refuses, and says why", () => {
  const result = decide("end-3", "out", "wait-1");
  assert.equal(result.ok, false);
  if (result.ok) {
    return;
  }
  assert.equal(result.reason, "no_outputs");
  assert.match(result.text, /End has no output ports/);
});

test("an unknown port refuses, and lists the ports that exist", () => {
  const result = decide("wait-1", "sideways", "transform-2");
  assert.equal(result.ok, false);
  if (result.ok) {
    return;
  }
  assert.equal(result.reason, "unknown_port");
  assert.match(result.text, /It exports out\./);
});

test("a node that is not on the canvas refuses", () => {
  const result = decide("ghost-9", "out", "wait-1");
  assert.equal(result.ok, false);
  if (result.ok) {
    return;
  }
  assert.equal(result.reason, "unknown_source");
});

test("a node cannot connect to itself", () => {
  const result = decide("wait-1", "out", "wait-1");
  assert.equal(result.ok, false);
  if (result.ok) {
    return;
  }
  assert.equal(result.reason, "self");
  assert.match(result.text, /cannot connect to itself/);
});

test("the same port cannot lead to the same node twice", () => {
  const existing: ConnectableEdge[] = [
    { id: "e1", source: "wait-1", source_port: "out", target: "transform-2" },
  ];
  const result = decide("wait-1", "out", "transform-2", existing);
  assert.equal(result.ok, false);
  if (result.ok) {
    return;
  }
  assert.equal(result.reason, "taken");
  assert.match(result.text, /already leads to that node/);
});

test("the same port may still lead to a different node", () => {
  const existing: ConnectableEdge[] = [
    { id: "e1", source: "wait-1", source_port: "out", target: "transform-2" },
  ];
  assert.equal(decide("wait-1", "out", "end-3", existing).ok, true);
});

test("a refusal produces no edge, so a refused gesture leaves the graph untouched", () => {
  for (const [source, port, target] of [
    ["end-3", "out", "wait-1"],
    ["wait-1", "sideways", "transform-2"],
    ["wait-1", "out", "wait-1"],
  ] as const) {
    const result = decide(source, port, target);
    assert.equal(result.ok, false, `${source} → ${target} should be refused`);
    assert.equal((result as { edge?: unknown }).edge, undefined);
  }
});
