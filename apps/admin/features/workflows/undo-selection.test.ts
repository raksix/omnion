/**
 * Undo and redo replace the graph wholesale, and a wholesale replacement has to prune the
 * selection. They do not, and the consequences are reachable with the keyboard alone.
 *
 * ## The defect
 *
 * `doUndo` and `doRedo` end in `setNodes(restored.nodes)` / `setEdges(restored.edges)` and
 * nothing else. `setSelection` is not called. So the selection still names cards from the
 * graph that was just thrown away, and every consumer of the selection is now answering
 * about a node that is not on the canvas:
 *
 * * The **inspector** looks the focus up with `nodes.find(...) ?? null`, so the panel goes
 *   blank — but the toolbar's Duplicate and Copy buttons read `disabled={!selected}`, and
 *   `selected` is `selection.focus`, a string that survived. Both are ENABLED for a node
 *   that does not exist.
 * * The **status bar** counts `selectionSize(selection)`, the union of `nodes` and `focus`,
 *   so it reads "1 selected" over an empty canvas.
 * * `Del` resolves its target through `deleteTarget(selection)` and would remove ids the
 *   restored graph does not have — which is a no-op the user reads as a broken key.
 *
 * This is the same hole the reload path closed in tick 48, reached from the other
 * direction. `load()` adopted a whole new graph and is now pruned; undo adopts a whole
 * *earlier* graph and is not. The rule is one rule — a graph replacement prunes — and it had
 * two callers, so one of them kept the pre-fix behaviour.
 *
 * ## The ordinary shape that reaches it
 *
 * Not exotic, and not two tabs: add two cards from the palette, select the first, and press
 * ⌘Z twice. The second press restores the graph from before the *first* add, so neither card
 * is on the canvas any more, and the inspector holds a node id nothing renders. The author
 * undid their own work correctly and is left looking at a selection that cannot be acted on.
 *
 * ## Why the prune is not a clear
 *
 * Same argument as the reload, and the same function: undoing a `remove` should leave the
 * node SELECTED again, because the author is looking at the card they just brought back.
 * Clearing would throw away a perfectly valid focus on the single most likely press in the
 * builder. `pruneSelection` already encodes the fallback, and reusing it means the two graph
 * replacements cannot drift apart.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import { capabilities, emptyHistory, record, snapshotOf, undo, type HistorySnapshot } from "./builder-history.ts";
import { deleteTarget, isNodeSelected, pruneSelection, selectGroup, selectNode, selectionSize, type CanvasSelection } from "./selection.ts";
import { rebaseAfterReload } from "./reload-rebase.ts";

const BUILDER_SOURCE = readFileSync(new URL("./builder-view.tsx", import.meta.url), "utf8");
const WITHOUT_IMPORTS = BUILDER_SOURCE.replace(/^import[\s\S]*?;\s*$/gm, "");

/**
 * The source of one `useCallback` handler, from `const <name> = useCallback` to the matching
 * close of that call.
 *
 * Bracket matching rather than `indexOf(");")`, and the first draft used the latter. It
 * returned a window that ended at the first `);` INSIDE the body — which, for a handler that
 * calls a helper, is the helper's own call — so the guard read three lines of a thirty-line
 * function and went red against correct code. A window that stops early is worse than no
 * window: it reports a defect that is not there, and the natural reaction is to weaken the
 * assertion until it agrees.
 */
const useCallbackBody = (name: string): string => {
  const at = WITHOUT_IMPORTS.indexOf(`const ${name} = useCallback`);
  assert.notEqual(at, -1, `the ${name} handler must exist`);
  const open = WITHOUT_IMPORTS.indexOf("(", at);
  assert.notEqual(open, -1, `${name} must be a useCallback call`);
  let depth = 0;
  for (let i = open; i < WITHOUT_IMPORTS.length; i += 1) {
    const char = WITHOUT_IMPORTS[i];
    if (char === "(") depth += 1;
    else if (char === ")") {
      depth -= 1;
      if (depth === 0) return WITHOUT_IMPORTS.slice(at, i + 1);
    }
  }
  assert.fail(`the ${name} handler is never closed`);
};

const snap = (ids: string[]): HistorySnapshot =>
  snapshotOf(
    ids.map((id) => ({ id, type: "task", label: id, params: {}, position: { x: 0, y: 0 } })),
    [],
  );

/**
 * The state the product was in: two cards added, the first selected, then two presses of ⌘Z.
 * The second press restores the graph from before the *first* add, so the graph is empty and
 * the selection is untouched.
 */
function afterTwoUndos(): { nodes: string[]; selection: CanvasSelection } {
  // add a, select a
  let history = record(emptyHistory(), { key: "add:a", before: snap([]), after: snap(["a"]), now: 1000 });
  // add b
  history = record(history, { key: "add:b", before: snap(["a"]), after: snap(["a", "b"]), now: 2000 });
  // The author selected `a` after adding it, and no press has touched the selection.
  const selection = selectNode("a");
  // undo -> cursor 1, undo -> cursor 0. Both restore graphs; the second one is the empty one.
  history = undo(undo(history));
  const nodes = history.entries[0].before.nodes.map((n) => (n as { id: string }).id);
  return { nodes, selection };
}

test("undoing past the node that was selected leaves the selection pointing at nothing", () => {
  // The precondition, written as a fact about the product rather than as the defect, so the
  // assertions below read as a change in behaviour.
  const { nodes, selection } = afterTwoUndos();
  assert.deepEqual(nodes, [], "precondition: both cards are gone from the canvas");
  assert.equal(selection.focus, "a", "precondition: the selection was never touched by the undo");
});

test("a selection whose nodes are all gone is pruned, so nothing is drawn as selected", () => {
  const { selection } = afterTwoUndos();
  const pruned = pruneSelection(selection, []);
  assert.equal(pruned.focus, null, "no surviving member, so no focus");
  assert.deepEqual(pruned.nodes, []);
  assert.equal(selectionSize(pruned), 0, "the status bar cannot count a card that is not there");
  assert.equal(isNodeSelected(pruned, "a"), false);
});

test("undoing a REMOVE leaves the node selected again, because the author brought it back", () => {
  // The other direction, and the reason this is a prune rather than a clear. Undo of a delete is
  // the single most likely press in the builder: the author pressed Del on the wrong card and
  // wants it back. Clearing the selection on that press would leave the card on the canvas and
  // nothing selected, which reads as the undo having only half worked.
  const current = selectNode("a");
  const afterUndoOfDelete = pruneSelection(current, ["a"]);
  assert.equal(afterUndoOfDelete.focus, "a", "the restored card keeps the inspector");
  assert.equal(selectionSize(afterUndoOfDelete), 1, "and it is drawn as selected");
});

test("a group selection is pruned per member, not dropped whole", () => {
  // Undoing a group delete restores every member, so all of them survive and the group is
  // untouched. The interesting case is a partial one, which no single press produces today —
  // but the rule is stated over members so that a graph replacement cannot keep a card it
  // does not have while keeping another.
  const current = selectGroup(["a", "b"]);
  const pruned = pruneSelection(current, ["a"]);
  assert.deepEqual(pruned.nodes, ["a"]);
  assert.equal(pruned.focus, "a", "the focused member survived, so focus is untouched");
});

test("Del after the prune has nothing to remove, so the key is not a silent no-op", () => {
  // The consequence that made the defect worth fixing rather than tidying. `deleteTarget`
  // resolves ids out of the selection; with a phantom focus it names a card the graph does
  // not have, and `removeNodes` returns early because the filter changed nothing.
  const { selection } = afterTwoUndos();
  const before = deleteTarget(selection);
  assert.deepEqual(before, { kind: "nodes", ids: ["a"] }, "precondition: the phantom names a card");
  const after = deleteTarget(pruneSelection(selection, []));
  assert.deepEqual(after, { kind: "nothing" }, "after the prune, Del is honestly disabled");
});

test("both graph replacements land on the SAME selection for the same inputs", () => {
  // `load()` and undo both replace the whole graph, so both prune — and the prune they share
  // has to be one implementation, or the two paths drift: an author who reloads after a
  // conflict and an author who presses ⌘Z would get different answers to the same question
  // about the same deletion. `rebaseAfterReload` is the loader's own rule, so comparing the
  // two over a set of cases is a real guard rather than a restatement: clearing instead of
  // pruning in either one turns this red.
  const cases: Array<{ current: CanvasSelection; alive: string[] }> = [
    { current: selectNode("gone"), alive: [] },
    { current: selectNode("gone"), alive: ["theirs"] },
    { current: selectNode("a"), alive: ["a", "b"] },
    { current: selectGroup(["a", "b", "c"]), alive: ["a", "c"] },
  ];
  for (const { current, alive } of cases) {
    assert.deepEqual(
      rebaseAfterReload(current, alive).selection,
      pruneSelection(current, alive),
      "the loader's rule and the undo's rule must be the same prune",
    );
  }
});

test("both undo and redo route through the one step that prunes", () => {
  // The wiring assertion. Every test above exercises `pruneSelection` directly, and all of
  // them pass against handlers that never call it — which is the defect, unchanged. This
  // reads the component.
  //
  // The claim is deliberately about the ROUTE rather than about text in each handler. The
  // first draft asserted a `setSelection((current) => pruneSelection(` inside `doUndo` and
  // `doRedo` and went red against a fix that was correct: the prune lives once in
  // `applyHistoryStep` and both handlers call it. A guard that demands the rule be written
  // twice is a guard that would make the next reader copy it — which is how two paths
  // drifted in the first place.
  for (const handler of ["doUndo", "doRedo"]) {
    assert.ok(
      /applyHistoryStep\(/.test(useCallbackBody(handler)),
      `${handler} must restore the graph through the shared step, or it reinstates the defect`,
    );
  }
  const step = useCallbackBody("applyHistoryStep");
  // `\s*` across the arrow, not a literal space. The claim is the ROUTE — "the shared step
  // replaces the whole graph, so the selection is pruned there" — and a guard that also pins
  // the formatter's line breaking reports a reformat as a defect. That is the same lesson as
  // the arg counter in `undo-edge-prune.test.ts`, in its other form: a check written about the
  // TEXT goes red for a reason that has nothing to do with the thing it claims to measure, and
  // the cheap repair — loosening it until it agrees — is how that becomes permanent.
  //
  // What is deliberately NOT relaxed: `setSelection`, the functional update, and the call to
  // `pruneSelection` all still have to be there, so this stays red against a step that drops
  // the prune entirely (mutation M1) rather than red against a line break.
  assert.ok(
    /setSelection\(\(current\) =>\s*pruneSelection\(/.test(step),
    "the shared step replaces the whole graph, so the selection must be pruned there",
  );
  // Both halves of the write, not just the prune: a step that prunes but never stores the
  // moved cursor would leave the buttons describing a history the canvas is not showing.
  assert.ok(/historyRef\.current = next/.test(step), "the cursor move must be stored");
  assert.ok(/setNodes\(restored\.nodes\)/.test(step), "and the graph must actually be restored");
});

test("the prune uses the RESTORED graph, not the one being replaced", () => {
  // The load-bearing detail, and the one a lazy fix gets wrong. `nodes` in the closure is the
  // graph *now* — the one that still holds both cards — so pruning against it would keep a
  // node the restore is about to remove, and the prune would be a no-op on exactly the case it
  // exists for. Asserted on the shared step, which is where the alive set is computed.
  const step = useCallbackBody("applyHistoryStep");
  assert.ok(
    /restored\.nodes\.map\(/.test(step),
    "the alive set must come from the restored snapshot, or the prune keeps a card the undo removes",
  );
  assert.ok(
    !/pruneSelection\([^,]+, nodes\b/.test(step),
    "pruning against the graph being replaced is the shape of the defect itself",
  );
});
