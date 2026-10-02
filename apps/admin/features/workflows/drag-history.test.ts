/**
 * A pointer drag is one undo step (REQ-004 criterion 6, "restore add, move, connect, delete").
 *
 * ## The defect this file exists for
 *
 * `commitMove` queued a save and recorded nothing, so a drag moved a card on the canvas and left
 * the Undo button grey. The product's own test file was green throughout, because it tests
 * `builder-history` — a module that has always been able to undo a move. The gap was in the
 * **caller**, and a unit test of the callee cannot see a caller that never calls.
 *
 * ## How this file avoids being that same blind test
 *
 * The load-bearing test is the last one. It reads `builder-view.tsx` and fails unless the drag
 * path goes through `beginDrag` on the way down and `endDrag` on the way up — because the unit
 * tests above can all pass against a `commitMove` that records nothing at all, which is the bug.
 * A module proved correct in isolation and never called is the failure mode this tick walked
 * into, and the assertion has to be about the wiring.
 *
 * The other question worth answering in isolation is the coalesce window, because it is the
 * one place where "record the drag" is not sufficient: two quick drags of the same card merge
 * into one entry and the position between them becomes unreachable.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import {
  COALESCE_MS,
  capabilities,
  emptyHistory,
  redo,
  redoTarget,
  snapshotOf,
  undo,
  undoTarget,
  type History,
  type HistorySnapshot,
} from "./builder-history.ts";
import { beginDrag, dragKey, dragMoved, endDrag } from "./drag-history.ts";

const BUILDER_SOURCE = readFileSync(new URL("./builder-view.tsx", import.meta.url), "utf8");

interface TestNode {
  id: string;
  type: string;
  label: string;
  params: Record<string, unknown>;
  position: { x: number; y: number };
}

const node = (id: string, x = 0): TestNode => ({
  id,
  type: "task",
  label: id,
  params: {},
  position: { x, y: 0 },
});

const snap = (ids: string[], x = 0): HistorySnapshot =>
  snapshotOf(ids.map((id) => node(id, x)), []);

/**
 * A graph with one card per entry, each at its own x — the shape a real graph has.
 *
 * `snap` collapses every card onto one x, which is fine for "did this card move" and wrong for
 * anything about *which cards exist*: the first draft of the depth test used it to build
 * "before drag b", produced a graph with no `b` in it, and the assertion then reported the
 * history as losing a node when the fixture had never had one.
 */
const graphOf = (...xs: number[]): HistorySnapshot =>
  snapshotOf(xs.map((x, i) => node(String.fromCharCode(97 + i), x)), []);

const xOf = (s: HistorySnapshot, i = 0): number => (s.nodes[i] as TestNode).position.x;
const xsOf = (s: HistorySnapshot): number[] => s.nodes.map((n) => (n as TestNode).position.x);
const ids = (s: HistorySnapshot): string[] => s.nodes.map((n) => (n as TestNode).id);

/** Drag one node from `from` to `to` and return the history. */
function drag(history: History, id: string, from: number, to: number, at: number): History {
  return endDrag(history, beginDrag([id], snap([id], from)), snap([id], to), at);
}

test("a drag is one press of undo, back to where the card was", () => {
  const history = drag(emptyHistory(), "a", 0, 120, 1000);
  assert.equal(history.entries.length, 1, "a gesture is one entry, not one per pointer frame");
  assert.equal(xOf(undoTarget(history) as HistorySnapshot), 0, "undo returns the pre-drag position");
  assert.equal(capabilities(history).canUndo, true, "the Undo button must be offered after a drag");
});

test("a drag is undoable at any depth, not only the first", () => {
  // Each gesture's snapshots are the WHOLE graph, the way the product holds it — so the fixture
  // has to accumulate. Building a one-node graph per drag would undo just as cleanly and would
  // have proven nothing about a second card still being where it was.
  let history = emptyHistory();
  history = endDrag(history, beginDrag(["a"], graphOf()), graphOf(40), 1000);
  history = endDrag(history, beginDrag(["b"], graphOf(40, 0)), graphOf(40, 40), 2000);
  history = endDrag(history, beginDrag(["c"], graphOf(40, 40, 0)), graphOf(40, 40, 40), 3000);
  assert.equal(history.entries.length, 3);

  history = undo(history);
  assert.deepEqual(
    ids(undoTarget(history) as HistorySnapshot),
    ["a", "b"],
    "the third drag is undone first, and the two cards it did not touch are still there",
  );
  assert.deepEqual(
    xsOf(undoTarget(history) as HistorySnapshot),
    [40, 0],
    "and the first two are exactly where the gestures left them",
  );
  history = undo(history);
  // Entry 0's `before` is the graph as it was before the FIRST drag, which is an empty canvas —
  // not "one card". The first draft of this assertion expected `["a"]` and read the history as
  // losing a node, when the three snapshots it was built from never had a card to lose.
  assert.deepEqual(ids(undoTarget(history) as HistorySnapshot), [], "the second drag is undone");
  history = undo(history);
  assert.equal(undoTarget(history), null, "the first drag is the last one to go");
  assert.equal(capabilities(history).canUndo, false);
});

test("undoing a drag and redoing it offers the dropped position again", () => {
  let history = drag(emptyHistory(), "a", 0, 200, 1000);
  history = undo(history);
  assert.equal(xOf(redoTarget(history) as HistorySnapshot), 200, "redo re-applies the move");

  history = redo(history);
  assert.equal(redoTarget(history), null, "the card is back where it was dropped");
  assert.equal(
    xOf(undoTarget(history) as HistorySnapshot),
    0,
    "and undoing again returns it to where the drag started, so the pair is symmetric",
  );
});

test("a drag of the whole selection is one entry, and undo restores every card", () => {
  const before = snap(["a", "b", "c"], 0);
  const moved = snapshotOf(
    [
      node("a", 40),
      node("b", 40),
      node("c", 40),
    ],
    [],
  );
  const history = endDrag(emptyHistory(), beginDrag(["c", "a", "b"], before), moved, 1000);
  assert.equal(history.entries.length, 1, "a group move is one gesture");
  assert.deepEqual(ids(undoTarget(history) as HistorySnapshot), ["a", "b", "c"]);
  assert.ok(
    moved.nodes.every((n, i) => xOf(moved, i) === 40) && before.nodes.every((n, i) => xOf(before, i) === 0),
    "the group really moved",
  );
});

test("the key names the set, so the order the gesture happened to be selected in does not matter", () => {
  assert.equal(dragKey(["a", "b"]), dragKey(["b", "a"]));
  assert.notEqual(dragKey(["a"]), dragKey(["a", "b"]), "one card is not a group");
  assert.notEqual(dragKey(["a"]), "add:a", "a move never shares a key with an add");
});

test("a click that never moved adds no step, so Undo does not spend a press on nothing", () => {
  const before = emptyHistory();
  const after = drag(before, "a", 0, 0, 1000);
  assert.equal(after, before, "no movement means no entry");
  assert.equal(capabilities(after).canUndo, false);
});

test("dragMoved tells a stationary click from a real move", () => {
  const origin = beginDrag(["a"], snap(["a"], 0));
  assert.equal(dragMoved(origin, snap(["a"], 0)), false);
  assert.equal(dragMoved(origin, snap(["a"], 8)), true);
});

test("two quick drags of the same card are two steps, and the position between them survives", () => {
  // The window is the whole reason `endDrag` seals. Without it these two entries carry the same
  // key inside COALESCE_MS and `record` merges them: `before` is kept from the first and `after`
  // taken from the second, so x=80 is never returned by either key. That is a history with a
  // hole in it, produced by the fix for "a drag is not undoable".
  let history = drag(emptyHistory(), "a", 0, 80, 1000);
  history = drag(history, "a", 80, 160, 1000 + COALESCE_MS - 10);
  assert.equal(history.entries.length, 2, "a release ends its own group");
  assert.equal(xOf(undoTarget(history) as HistorySnapshot), 80, "the intermediate position is reachable");
});

test("the many pointer frames of one drag never become many entries", () => {
  // What the product actually does: every frame calls `moveNode`, and only the release records.
  let history = emptyHistory();
  const origin = beginDrag(["a"], snap(["a"], 0));
  for (let frame = 1; frame <= 12; frame += 1) {
    // The graph moves on a ref without touching the history — this is `moveNode`. Nothing here
    // may add an entry, which is the whole point: a per-frame entry would turn one gesture into
    // twelve presses of undo.
    assert.equal(history.entries.length, 0, `frame ${frame} wrote positions, not history`);
  }
  history = endDrag(history, origin, snap(["a"], 12 * 8), 1000);
  assert.equal(history.entries.length, 1, "the release is the single entry");
  assert.equal(xOf(undoTarget(history) as HistorySnapshot), 0, "one press returns the start position");
});

test("a drag after an undo cancels the redo future, like every other edit", () => {
  let history = drag(emptyHistory(), "a", 0, 100, 1000);
  history = drag(history, "b", 0, 100, 5000);
  history = undo(history);
  assert.equal(capabilities(history).canRedo, true);

  history = drag(history, "a", 0, 300, 9000);
  assert.equal(capabilities(history).canRedo, false, "a new gesture is a new future");
  assert.equal(redoTarget(history), null);
});

test("the builder's drag path actually records: the history is only correct if it is called", () => {
  // The wiring assertion. Every test above exercises `drag-history` directly, and all of them
  // pass against a `commitMove` that records nothing — which is the defect, unchanged. This
  // reads the component and fails unless both ends of the gesture are present.
  const begins = BUILDER_SOURCE.match(/beginDrag\(/g) ?? [];
  const ends = BUILDER_SOURCE.match(/endDrag\(/g) ?? [];
  assert.ok(begins.length >= 1, "the gesture must open where the pre-drag graph still exists");
  assert.ok(ends.length >= 1, "the gesture must close where the history is written");

  // Order matters and a set membership test cannot see it: the `before` has to be captured on
  // the way DOWN, because by pointer-up every frame has already written the new position and
  // there is no snapshot of the old one left to capture.
  // Find the CALL, not the name. The import line is the first occurrence of both, it sits above
  // every call site, and `indexOf` on the raw source therefore answers the import — so the first
  // draft of this guard passed on a component that opened no gesture at all.
  //
  // What this used to assert instead — "beginDrag appears before endDrag in the file" — is a
  // statement about how React source is ORDERED, and React source is not ordered by events:
  // `commitMove` (the release) is written hundreds of lines above the `pointerdown` that opens
  // the gesture, so the real file reads end-then-begin. It went red on correct code, which is
  // worse than useless for a guard. The claim worth keeping is the *claim itself*: the origin is
  // taken at the gesture's start, and it is the whole pre-drag graph rather than a position.
  const WITHOUT_IMPORTS = BUILDER_SOURCE.replace(/^import[\s\S]*?;\s*$/gm, "");
  const callOf = (name: string): number => WITHOUT_IMPORTS.indexOf(`${name}(`);
  assert.ok(callOf("beginDrag") !== -1, "the gesture must open where the pre-drag graph still exists");
  assert.ok(callOf("endDrag") !== -1, "the gesture must close where the history is written");

  // The `before` is the WHOLE graph, captured through `currentSnapshot` — not a position, which
  // is what a reconstruction from the dragged node would have produced.
  const open = WITHOUT_IMPORTS.slice(callOf("beginDrag"), callOf("beginDrag") + 80);
  assert.ok(
    /beginDrag\(\[node\.id\], currentSnapshot\(\)\)/.test(open),
    "the pre-drag graph is taken at pointerdown, while it is still the pre-drag graph",
  );

  // And the release must hand that origin to the commit rather than the commit re-deriving one.
  assert.ok(
    /commitMove\(dragging\.origin\)/.test(WITHOUT_IMPORTS),
    "pointerup must spend the origin the gesture opened with",
  );

  // And the recording must be the drag's commit, not a decoration beside it: `commitMove` is
  // what `pointerup` calls, and while it only queued a save the button stayed grey.
  const commitMove = BUILDER_SOURCE.slice(
    BUILDER_SOURCE.indexOf("const commitMove"),
    BUILDER_SOURCE.indexOf("const commitMove") + 700,
  );
  assert.ok(
    /endDrag\(/.test(commitMove),
    "commitMove -- the gesture's commit -- must record the move, or the drag is not undoable",
  );
});
