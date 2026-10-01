import { strict as assert } from "node:assert";
import test from "node:test";

import {
  isLastOnPath,
  portWalksThrough,
  reachedByTrigger,
  reachedNodes,
  type WalkWorld,
} from "./reachability.ts";
import type { GraphEdge, GraphNode, GraphNodeType } from "@/lib/api";

/** The palette, as the server ships it — the `terminal` flag is the whole input. */
const TYPES: Map<string, GraphNodeType> = new Map(
  (
    [
      ["trigger.manual", [{ key: "out", label: "Next", terminal: false }]],
      ["condition", [
        { key: "true", label: "True", terminal: false },
        { key: "false", label: "False", terminal: true },
      ]],
      ["action", [
        { key: "success", label: "Succeeded", terminal: false },
        { key: "error", label: "Failed", terminal: true },
      ]],
      ["wait", [{ key: "out", label: "Next", terminal: false }]],
      ["end", []],
    ] as Array<[string, Array<{ key: string; label: string; terminal: boolean }>]>
  ).map(([key, outputs]) => [
    key,
    { key, label: key, category: "X", summary: "", outputs, params: [], defaults: {}, inert: key === "note" },
  ]),
);

function n(id: string, type: string): GraphNode {
  return { id, type, label: id, params: {}, position: { x: 0, y: 0 } };
}

function e(source: string, port: string, target: string): GraphEdge {
  return { id: `${source}-${port}-${target}`, source, source_port: port, target };
}

function world(nodes: GraphNode[], edges: GraphEdge[]): WalkWorld {
  return { nodes, edges, types: TYPES };
}

test("the trigger reaches itself", () => {
  const w = world([n("t", "trigger.manual")], []);
  assert.equal(reachedByTrigger(w, "t"), true);
});

test("the walk follows a connection on a non-terminal port", () => {
  const w = world(
    [n("t", "trigger.manual"), n("a", "action")],
    [e("t", "out", "a")],
  );
  assert.equal(reachedByTrigger(w, "a"), true);
});

test("an UNCONNECTED node is not reached — this is the defect the module exists for", () => {
  // The editing state of this builder: the card was dropped from the palette and has not
  // been wired yet. `isLastOnPath` says it looks like the end of a path (no connection
  // LEAVES it), which is exactly what made the old rule offer it a live button while the
  // server refused every press with `unknown_node`.
  const w = world(
    [n("t", "trigger.manual"), n("a", "action"), n("island", "wait")],
    [e("t", "out", "a")],
  );
  assert.equal(isLastOnPath(w, "island"), true, "it looks like the end of its own path");
  assert.equal(
    reachedByTrigger(w, "island"),
    false,
    "and the engine has never heard of it — the two answers are different questions",
  );
});

test("a connection on a terminal port does not carry the walk onward", () => {
  // `action` -> `error` is where a failed step goes; the run is over there. The engine
  // does not walk onto it, so neither can the client.
  const w = world(
    [n("t", "trigger.manual"), n("a", "action"), n("b", "wait")],
    [e("t", "out", "a"), e("a", "error", "b")],
  );
  assert.equal(reachedByTrigger(w, "a"), true);
  assert.equal(reachedByTrigger(w, "b"), false);
});

test("a branch wired only on its false arm reaches nothing", () => {
  // The `false` arm is terminal by the registry's own declaration, so a condition whose
  // only connection is that arm is on the path and everything past it is not.
  const w = world(
    [n("t", "trigger.manual"), n("c", "condition"), n("end", "end")],
    [e("t", "out", "c"), e("c", "false", "end")],
  );
  assert.equal(reachedByTrigger(w, "c"), true);
  assert.equal(reachedByTrigger(w, "end"), false, "the walk stops on the false arm");
});

test("the true arm does carry the walk onward", () => {
  const w = world(
    [n("t", "trigger.manual"), n("c", "condition"), n("a", "action")],
    [e("t", "out", "c"), e("c", "true", "a")],
  );
  assert.equal(reachedByTrigger(w, "a"), true);
});

test("a graph with no trigger has no walk, and says so rather than answering no", () => {
  // `null` is not "unreachable". Refusing the control on every card of a half-written
  // rule is its own defect: the author is not yet running anything, and the press still
  // goes to the server either way.
  const w = world([n("a", "action")], []);
  assert.equal(reachedByTrigger(w, "a"), null);
  assert.equal(reachedNodes(w).size, 0);
});

test("an unknown node type does not truncate the walk", () => {
  // A plugin node the palette does not carry, or a card drawn before a plugin was
  // disabled. The validator refuses such an edge at save time, so this is only ever asked
  // about a graph the server may well accept — and a wrongly DISABLED control is a bug
  // the author cannot work around.
  const w = world(
    [n("t", "trigger.manual"), n("p", "plugin.mailer.send")],
    [e("t", "out", "p")],
  );
  assert.equal(portWalksThrough("out", undefined), true);
  assert.equal(reachedByTrigger(w, "p"), true);
});

test("an unknown PORT on a known type is treated as walkable, for the same reason", () => {
  assert.equal(portWalksThrough("nonexistent", TYPES.get("action")), true);
});

test("the terminal flag alone decides the port rule", () => {
  // There is no list of port names anywhere in this module. The flag is the registry's
  // own answer, which is what stops this becoming a fourth copy of `follows()`.
  assert.equal(portWalksThrough("success", TYPES.get("action")), true);
  assert.equal(portWalksThrough("error", TYPES.get("action")), false);
  assert.equal(portWalksThrough("true", TYPES.get("condition")), true);
  assert.equal(portWalksThrough("false", TYPES.get("condition")), false);
});

test("a cycle does not hang the walk", () => {
  // The engine refuses one at save time; an unsaved canvas can still hold one, and a
  // walk that hangs the inspector is worse than a wrong answer.
  const w = world(
    [n("t", "trigger.manual"), n("a", "action"), n("b", "action")],
    [e("t", "out", "a"), e("a", "success", "b"), e("b", "success", "a")],
  );
  assert.deepEqual([...reachedNodes(w)].sort(), ["a", "b", "t"]);
});

test("isLastOnPath is about connections leaving, not about reachability", () => {
  // The two are separate questions and the island answers them differently: it has no
  // connection leaving it (so it *looks* like an end) and the trigger does not reach it
  // (so it is not one). Collapsing them is the defect.
  const w = world([n("t", "trigger.manual"), n("island", "action")], []);
  assert.equal(isLastOnPath(w, "island"), true);
  assert.equal(reachedByTrigger(w, "island"), false);
});