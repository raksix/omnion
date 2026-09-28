/**
 * The builder history's own tests (REQ-004 slice 2).
 *
 * Run with `pnpm --filter @omnion/admin test` — node 22 strips the types, so there is no
 * transpiler between the source and the assertion. These are written against the *questions a
 * user asks of an undo button* — "does one press return what I had", "is the button honest
 * about whether there is anything to undo" — rather than against the implementation's shape,
 * because an undo stack that satisfies its own types can still do nothing when pressed.
 */
import assert from "node:assert/strict";
import test from "node:test";

import {
  COALESCE_MS,
  HISTORY_LIMIT,
  capabilities,
  emptyHistory,
  redo,
  redoTarget,
  record,
  sealGroup,
  snapshotOf,
  undo,
  undoTarget,
  type History,
  type HistorySnapshot,
} from "./builder-history.ts";

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

const ids = (s: HistorySnapshot): string[] =>
  s.nodes.map((n) => (n as TestNode).id);
const xOf = (s: HistorySnapshot, i = 0): number =>
  (s.nodes[i] as TestNode).position.x;

function step(history: History, key: string, before: HistorySnapshot, after: HistorySnapshot, now: number): History {
  return record(history, { key, before, after, now });
}

test("an empty history offers neither undo nor redo", () => {
  const history = emptyHistory();
  assert.deepEqual(capabilities(history), { canUndo: false, canRedo: false });
  assert.equal(undoTarget(history), null);
  assert.equal(redoTarget(history), null);
});

test("one change is one press of undo, back to the graph that was there", () => {
  let history = step(emptyHistory(), "add", snap([]), snap(["a"]), 1000);
  assert.equal(capabilities(history).canUndo, true);

  const target = undoTarget(history);
  assert.ok(target, "an undoable history must name a target");
  assert.deepEqual(ids(target), [], "undo must restore the pre-change nodes, not the current ones");

  history = undo(history);
  assert.equal(undoTarget(history), null, "nothing left to undo after the only press");
  assert.deepEqual(capabilities(history), { canUndo: false, canRedo: true });
});

test("undo then redo returns the change that was made", () => {
  let history = step(emptyHistory(), "add", snap([]), snap(["a"]), 1000);
  history = undo(history);
  const forward = redoTarget(history);
  assert.ok(forward);
  assert.deepEqual(ids(forward), ["a"]);

  history = redo(history);
  assert.equal(redoTarget(history), null);
  assert.equal(capabilities(history).canRedo, false);
});

test("a drag that fires many moves is one press, not one press per pointer event", () => {
  let history = emptyHistory();
  for (let i = 1; i <= 20; i += 1) {
    history = step(history, "move:a", snap(["a"]), snap(["a"], i), 1000 + i * 50);
  }
  assert.equal(history.entries.length, 1, `a gesture must be one entry, got ${history.entries.length}`);
  const back = undoTarget(history);
  assert.ok(back);
  assert.equal(xOf(back), 0, "undo returns the position from before the drag, not the last sample");
});

test("a move is not coalesced with a different node, nor with an unrelated edit", () => {
  let history = step(emptyHistory(), "move:a", snap(["a"]), snap(["a"], 40), 1000);
  history = step(history, "move:b", snap(["a"], 40), snap(["a", "b"], 40), 1030);
  assert.equal(history.entries.length, 2);

  history = step(history, "rename:a", snap(["a", "b"], 40), snap(["a", "b"], 80), 1060);
  assert.equal(history.entries.length, 3, "a different key never coalesces");
});

test("sealing a group stops the next move from merging into the last one", () => {
  let history = step(emptyHistory(), "move:a", snap(["a"]), snap(["a"], 40), 1000);
  history = step(history, "move:a", snap(["a"], 40), snap(["a"], 80), 1020);
  assert.equal(history.entries.length, 1, "still one gesture");

  history = sealGroup(history, 1020);
  history = step(history, "move:a", snap(["a"], 80), snap(["a"], 120), 1040);
  assert.equal(history.entries.length, 2, "a sealed gesture is followed by its own entry");
});

test("a change that changes nothing is not a change", () => {
  const before = emptyHistory();
  const after = step(before, "move:a", snap(["a"]), snap(["a"]), 1000);
  assert.equal(after, before, "a no-op must return the same history object");
  assert.equal(capabilities(after).canUndo, false);
});

test("a new edit discards the redo future", () => {
  let history = step(emptyHistory(), "add:a", snap([]), snap(["a"]), 1000);
  history = step(history, "add:b", snap(["a"]), snap(["a", "b"]), 2000);
  history = undo(history);
  assert.equal(capabilities(history).canRedo, true);

  history = step(history, "add:c", snap(["a"]), snap(["a", "c"]), 3000);
  assert.equal(capabilities(history).canRedo, false, "redo must not survive a divergent edit");
  assert.equal(redoTarget(history), null);
});

test("undo walks all the way back through several changes", () => {
  let history = step(emptyHistory(), "add:a", snap([]), snap(["a"]), 1000);
  history = step(history, "add:b", snap(["a"]), snap(["a", "b"]), 2000);
  history = step(history, "add:c", snap(["a", "b"]), snap(["a", "b", "c"]), 3000);

  history = undo(history);
  history = undo(history);
  history = undo(history);
  assert.equal(undoTarget(history), null);
  assert.equal(capabilities(history).canUndo, false);
  assert.equal(history.entries.length, 3, "undoing walks the entries, it does not delete them");
});

test("a press of undo returns the state before the last change, not the one before that", () => {
  // The classic off-by-one: with entries [add a, add b], the first undo must land on "a" and
  // the second on the empty graph. Reading `entries[cursor - 1].after` gets this backwards.
  let history = step(emptyHistory(), "add:a", snap([]), snap(["a"]), 1000);
  history = step(history, "add:b", snap(["a"]), snap(["a", "b"]), 2000);

  const first = undoTarget(history);
  assert.ok(first);
  assert.deepEqual(ids(first), ["a"], "first undo lands between the two adds");

  history = undo(history);
  const second = undoTarget(history);
  assert.ok(second);
  assert.deepEqual(ids(second), [], "second undo lands on the empty graph");
});

test("the history is bounded and keeps the newest entries", () => {
  let history = emptyHistory();
  for (let i = 0; i < HISTORY_LIMIT + 25; i += 1) {
    history = step(
      history,
      `add:${i}`,
      snap([`n${i}`]),
      snap([`n${i}`, `x${i}`]),
      1000 + i * (COALESCE_MS + 10),
    );
  }
  assert.equal(history.entries.length, HISTORY_LIMIT);
  assert.deepEqual(capabilities(history), { canUndo: true, canRedo: false });
});

test("a snapshot is a copy, so later mutation of the caller's array is not history", () => {
  const nodes = [node("a")];
  const copy = snapshotOf(nodes, []);
  nodes[0].position.x = 999;
  nodes.push(node("b"));
  assert.equal((copy.nodes[0] as TestNode).position.x, 0);
  assert.equal(copy.nodes.length, 1);
});
