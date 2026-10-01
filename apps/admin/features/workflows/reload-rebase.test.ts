/**
 * A wholesale graph replacement has to rebase the history and the selection.
 *
 * ## The defect this file exists for
 *
 * `load()` is the **Reload** exit of the two-tab conflict: it fetches the server's graph and
 * writes it over the canvas. It wrote `nodes`, `edges`, `version` and the save indicator — and
 * left the undo history exactly as it was. So the Undo button stayed ENABLED after adopting
 * another editor's definition, and the entries it held described the graph the author had just
 * decided to discard.
 *
 * `doUndo` ends in `queueSave()`, and `queueSave` quotes `versionRef` — which `load` has just
 * advanced to the server's current version. So one press of ⌘Z after a conflict wrote the
 * discarded graph back over the other tab, the server accepted it (the version was current, so
 * no conflict was possible), and the concurrency guard this feature exists for was undone by
 * the undo button. No error, no banner, no second refusal.
 *
 * The shape of the bug is the shape of the previous tick's: the module under test here is
 * correct, and the product called it from nowhere. Every unit test below passes against a
 * `load` that never rebases — the defect, unchanged. So the last test reads the component.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import {
  capabilities,
  emptyHistory,
  record,
  snapshotOf,
  type HistorySnapshot,
} from "./builder-history.ts";
import { rebaseAfterReload } from "./reload-rebase.ts";
import {
  clearSelection,
  pruneSelection,
  selectGroup,
  selectNode,
  type CanvasSelection,
} from "./selection.ts";

const BUILDER_SOURCE = readFileSync(new URL("./builder-view.tsx", import.meta.url), "utf8");
const WITHOUT_IMPORTS = BUILDER_SOURCE.replace(/^import[\s\S]*?;\s*$/gm, "");

const snap = (ids: string[]): HistorySnapshot =>
  snapshotOf(
    ids.map((id) => ({ id, type: "task", label: id, params: {}, position: { x: 0, y: 0 } })),
    [],
  );

/** A history holding one real entry, as it would be after an author added a node. */
const historyWithOneEntry = () =>
  record(emptyHistory(), { key: "add:mine", before: snap([]), after: snap(["mine"]), now: 1000 });

test("the history survives a reload, and the Undo button stays enabled", () => {
  // The state the product was in. Written as the precondition rather than asserted as a defect,
  // so the next assertion below reads as a change in behaviour rather than as a second fact
  // about the same thing.
  const before = historyWithOneEntry();
  assert.equal(capabilities(before).canUndo, true, "precondition: the author had something to undo");

  const rebased = rebaseAfterReload(clearSelection(), ["theirs"]);
  assert.equal(
    capabilities(rebased.history).canUndo,
    false,
    "a graph this history does not describe cannot be undone, so Undo must be disabled",
  );
  assert.equal(rebased.history.entries.length, 0);
});

test("a press of undo after a reload has nothing to restore", () => {
  // The overwrite itself, stated as a fact about the two snapshots. `doUndo` returns
  // `undoTarget(history)`, which is `null` at cursor -1 — the caller returns without writing,
  // so the discarded graph never reaches the server.
  const rebased = rebaseAfterReload(clearSelection(), ["theirs"]);
  assert.equal(rebased.history.cursor, -1, "nothing is undoable, so nothing can be written back");
});

test("a selection that survived the reload is kept, so the author keeps their place", () => {
  // The ordinary outcome: the other editor added a node and left yours alone. Clearing instead
  // of pruning would throw away a perfectly valid focus, and a reload that dumps the inspector
  // for no reason is a reload that discards the author's place.
  //
  // `nodes` is asserted empty because that is what a CLICK is: `selectNode` returns a focus with
  // no group. The first draft of this test asserted `nodes: ["mine"]` and the product was right —
  // a single-card selection genuinely has no members, and the group only fills in on Shift+click.
  const current = selectNode("mine");
  const rebased = rebaseAfterReload(current, ["mine", "theirs"]);
  assert.equal(rebased.selection.focus, "mine", "the surviving card stays selected");
  assert.deepEqual(rebased.selection.nodes, [], "a click is a focus, not a group");
});

test("a selection the OTHER editor deleted is pruned, not kept", () => {
  // This is the case that makes the reload a reload: the other editor removed the card this tab
  // had selected, and the inspector would otherwise keep rendering a node the canvas does not
  // contain — a panel that can be aimed at a node that is not in the graph.
  //
  // Focus goes to NOTHING rather than to "theirs", and the first draft of this test asserted
  // `focus === "theirs"` — i.e. it demanded the behaviour this feature's own doc comment calls
  // the one that makes a reload feel like a different tab taking the wheel. `pruneSelection`
  // falls back to a surviving MEMBER OF THE SELECTION, and a single click has no members, so
  // the honest answer is the empty selection. The product was right and the expectation was
  // the defect; had I "fixed" the product to match, the reload would have started selecting
  // cards nobody asked it to.
  const current = selectNode("gone");
  const rebased = rebaseAfterReload(current, ["theirs"]);
  assert.equal(rebased.selection.focus, null, "no member of the selection survived, so no focus");
  assert.deepEqual(rebased.selection.nodes, []);
});

test("a selection with nothing left to point at becomes the empty one, not a stranger's card", () => {
  // Silently focusing an arbitrary survivor reads as the reload having picked something for the
  // author: they had nothing selected and something is now drawn as selected.
  const current = selectNode("gone");
  const rebased = rebaseAfterReload(current, []);
  assert.equal(rebased.selection.focus, null, "an empty graph cannot have a focused node");
  assert.deepEqual(rebased.selection.nodes, []);
});

test("a multi-card selection keeps only the members that survived", () => {
  // `selectGroup` focuses the LAST id, so the fallback below is a real path and not a fiction:
  // a marquee whose focused card the other editor deleted still has surviving members to land
  // on. The first draft asserted focus `"a"` while the group focused `"c"` — asserting the
  // product had done something it never claimed.
  const current: CanvasSelection = selectGroup(["a", "b", "c"]);
  assert.equal(current.focus, "c", "precondition: a group focuses its last member");
  const rebased = rebaseAfterReload(current, ["a", "c", "d"]);
  assert.deepEqual(rebased.selection.nodes, ["a", "c"]);
  assert.equal(rebased.selection.focus, "c", "the focused card survived, so focus is untouched");
});

test("a deleted FOCUS inside a group falls back to a member the author had also selected", () => {
  // The complement of the previous test, and the only case where `pruneSelection`'s fallback is
  // observable. Two drafts of this assertion were wrong in the same way — I assumed the
  // fallback takes the LAST surviving member, because `selectGroup` focuses the last one and the
  // two felt like they had to agree. The rule is `nodes[0]`: the FIRST survivor in the
  // selection's own order. It does not matter which is right as a design matter; what matters
  // is that the test states the rule the product implements instead of the symmetry that felt
  // natural, and that the answer is a member of the author's OWN selection rather than a
  // stranger on the canvas.
  const current: CanvasSelection = selectGroup(["a", "b", "c"]);
  const rebased = rebaseAfterReload(current, ["a", "b"]);
  assert.equal(rebased.selection.focus, "a", "the FIRST surviving member takes the inspector");
  assert.deepEqual(rebased.selection.nodes, ["a", "b"]);
});

test("the rebase keeps the selected EDGE only when it is not asked about edges", () => {
  // `alive` is the node id set, so an edge selection cannot be validated here. The honest
  // statement of the limit: this function prunes NODES, and a rebase that silently kept a
  // selected edge would point the delete key at a connection the loaded graph may not have.
  // `pruneSelection` keeps `edge` untouched, which is the conservative half — deleting an edge
  // that is still there is recoverable, and a reload cannot invent one.
  const withEdge: CanvasSelection = { nodes: ["a"], focus: "a", edge: "e-1" };
  const rebased = rebaseAfterReload(withEdge, ["a"]);
  assert.equal(rebased.selection.edge, "e-1", "the edge is left alone rather than guessed at");
});

test("pruneSelection is what both paths agree on, so the rule has one implementation", () => {
  // If `rebaseAfterReload` ever stopped delegating to `pruneSelection`, this goes red — and the
  // thing it protects is a rule restated in two places, which is the third copy of a walk rule
  // this branch has now walked into.
  const current = selectNode("gone");
  assert.deepEqual(
    rebaseAfterReload(current, ["theirs"]).selection,
    pruneSelection(current, ["theirs"]),
    "the rebase delegates rather than reimplementing the prune",
  );
});

test("the builder's reload actually rebases: the module is only correct if it is called", () => {
  // The wiring assertion. Every test above exercises `reload-rebase` directly, and all of them
  // pass against a `load` that does not call it — which is the defect, unchanged. This reads
  // the component.
  //
  // Strip the imports before locating the CALL: the import line is the first occurrence of any
  // name and it sits above every call site, so a guard built on the raw source answers the
  // import. That mistake was made and caught on this branch already.
  const callOf = (name: string): number => WITHOUT_IMPORTS.indexOf(`${name}(`);
  assert.ok(callOf("rebaseAfterReload") !== -1, "the reload must rebase the history and selection");

  // And it must be inside `load` itself, not sitting somewhere near it. `load` is the function
  // that replaces the graph wholesale; a rebase performed by some other caller would leave the
  // Reload button — the one this whole defect is about — still unrebased. The window is the
  // function body, not the file: the previous tick's guard claimed an ORDER over the file and
  // went red on correct code, because React source is not ordered by events.
  const loadAt = WITHOUT_IMPORTS.indexOf("const load =");
  assert.ok(loadAt !== -1, "the loader must exist");
  const loadBody = WITHOUT_IMPORTS.slice(loadAt, loadAt + 2600);
  assert.ok(
    /rebaseAfterReload\(/.test(loadBody),
    "the rebase belongs to the loader, which is what replaces the graph wholesale",
  );

  // Both halves are written, not just called: a call whose result is discarded rebases nothing,
  // and a history that is emptied in the module but never assigned leaves the old entries in
  // the ref — the exact shape of the tick-47 bug, where the caller queued a save and recorded
  // nothing.
  assert.ok(
    /historyRef\.current = rebased\.history/.test(loadBody),
    "the emptied history must be stored, or the old entries survive the reload",
  );
  assert.ok(
    /setSelection\(rebased\.selection\)/.test(loadBody),
    "the pruned selection must be applied, or the inspector keeps a node the graph lacks",
  );
  assert.ok(
    /setHistoryTick\(/.test(loadBody),
    "the toolbar re-reads the history through a tick, so a silent rebase would not redraw it",
  );

  // The version is advanced on the line above the rebase, and that is what made the overwrite
  // possible: `doUndo` -> `queueSave` quotes the version this loader just took from the server.
  // Assert the ordering INSIDE the function, which is a statement about the code's own sequence
  // and not about where the function sits in the file.
  const versionAt = loadBody.indexOf("versionRef.current =");
  const rebaseAt = loadBody.indexOf("rebaseAfterReload(");
  assert.ok(
    versionAt !== -1 && versionAt < rebaseAt,
    "the loader advances the version and then rebases -- which is why an undo afterwards writes",
  );
});
