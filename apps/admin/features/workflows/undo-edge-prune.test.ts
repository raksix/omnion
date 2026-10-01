/**
 * Undo and redo replace the graph wholesale, and a wholesale replacement prunes the selection
 * — **including the connection**, which one caller of the rule was left not to do.
 *
 * ## The defect
 *
 * `pruneSelection` grew an optional third argument for the edge ids two ticks ago, because a
 * selection naming a connection the adopted graph does not have is not cosmetic: an edge
 * outranks every node selection in `deleteTarget`, so `Del` resolves to a line that is not
 * there (`removeEdge` finds nothing and changes nothing), and the status bar announces
 * "1 connection selected (Del removes it)" over a canvas drawing no such line.
 *
 * The argument was wired into `rebaseAfterReload` — the **Reload** exit of the two-tab
 * conflict — and into nothing else. `applyHistoryStep`, the shared step undo and redo both
 * restore through, still called it with two arguments and pruned nodes only.
 *
 * ## Why the undo path is the reachable one, and the reload path was not the only gap
 *
 * Undo needs no second editor, no conflict and no banner. The ordinary shape is: draw a
 * connection, select it, press ⌘Z. The restored snapshot is the graph from **before** the
 * connection existed, so `restored.edges` is empty — the answer was sitting in the next local
 * over, unasked. The reverse is why this is a prune and not a blanket clear: undoing an edge
 * *delete* restores the graph **with** that edge, and the author is looking straight at it.
 *
 * `removeNodes` was the second two-argument caller, and the interesting part is why nobody
 * noticed: the comment written next to the first fix described `removeNodes` as the caller
 * that "has no edge list in hand". It does — `nextEdges` is computed two lines above its own
 * prune, on the same breath as `nextNodes`, because removing a node removes the connections
 * that end on it. **A comment naming the wrong caller is worse than no comment**, because it
 * reads as the reason the argument was left off, and the next reader greps the comment rather
 * than the call site.
 *
 * ## What is asserted here, and what is not
 *
 * `prune-edge.test.ts` already owns the *rule* (asked → decide, not asked → do not guess), and
 * those rows stay green and unchanged: this is not the fix re-argued, it is the fix reaching
 * the two callers that were never handed the set. So this file asserts:
 *
 * * the rule, restated over the two call sites' own inputs (the restored/next edge lists), so a
 *   revert of the *wiring* cannot hide behind a rule that is still correct in isolation; and
 * * the wiring itself — read off the component, because every rule assertion above passes
 *   against a `pruneSelection` the component never calls with the third argument, which is
 *   precisely the defect.
 *
 * The wiring assertion counts arguments at paren depth. Its first draft used `split(",")`,
 * which read 4 against a correct three-argument call — a trailing comma in the formatter plus
 * the one inside `map((edge) => edge.id)` — and the cheap repair was to widen the expectation
 * to 4. That is how a broken guard becomes a green one, so the count is taken at depth and the
 * comment says why.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import {
  deleteTarget,
  EMPTY_SELECTION,
  escapeStepIsHandled,
  pruneSelection,
  selectEdge,
  selectNode,
  whatEscapeClears,
  type CanvasSelection,
} from "./selection.ts";
import { rebaseAfterReload } from "./reload-rebase.ts";

const BUILDER_SOURCE = readFileSync(new URL("./builder-view.tsx", import.meta.url), "utf8");
const WITHOUT_IMPORTS = BUILDER_SOURCE.replace(/^import[\s\S]*?;\s*$/gm, "");

/** The source of one `useCallback` handler, matched by bracket depth rather than `indexOf(");")`. */
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

/**
 * The arguments of one call, counted as top-level SEGMENTS.
 *
 * Two instruments were wrong here before the right one, and the second is the one worth
 * writing down:
 *
 * * `split(",")` reads 4 against a correct three-argument call — the formatter's trailing comma
 *   plus the one inside `map((edge) => edge.id)`. Widening the expectation to 4 is how a broken
 *   guard becomes a green one.
 * * Starting the scan *at* the open paren puts every separator at depth 1, so nothing is
 *   top-level and the count collapses to 1. Starting it one character *after* the paren fixes
 *   that and immediately reintroduces the trailing comma, because a comma before the close is a
 *   separator with nothing after it.
 *
 * So: split at depth zero and count the pieces that hold something. Both defects produced a
 * green-looking number, which is the only reason they are described rather than just fixed.
 */
const callArgCount = (source: string, callee: string): number => {
  const at = source.indexOf(`${callee}(`);
  assert.notEqual(at, -1, `expected a call to ${callee}`);
  let depth = 0;
  let segments = 0;
  let current = "";
  for (let i = at + callee.length + 1; i < source.length; i += 1) {
    const char = source[i];
    if (char === "(" || char === "[" || char === "{") {
      depth += 1;
      current += char;
    } else if (char === ")" || char === "]" || char === "}") {
      if (depth === 0) break;
      depth -= 1;
      current += char;
    } else if (char === "," && depth === 0) {
      if (current.trim() !== "") segments += 1;
      current = "";
    } else {
      current += char;
    }
  }
  if (current.trim() !== "") segments += 1;
  return segments;
};

// The two shapes, as the product's own history produces them. `edge:add` records the graph
// *without* the connection in its `before` snapshot, so undoing a connection restore is a graph
// whose edge list is empty — the case the two-argument prune answered "keep" to.
const graphBeforeTheConnection: { nodes: string[]; edges: string[] } = {
  nodes: ["a", "b"],
  edges: [],
};
const graphAfterTheConnection: { nodes: string[]; edges: string[] } = {
  nodes: ["a", "b"],
  edges: ["e-1"],
};

/**
 * Every `pruneSelection(` call in the component, taken whole.
 *
 * The first draft of this was a non-greedy regex, `/pruneSelection\([\s\S]*?\)/g`, and it is the
 * same early-window bug `useCallbackBody` above documents: `?` stops at the first `)`, which in
 * a multi-line call is the close of `map((node) => node.id)` — one argument short of the call's
 * own end. The inventory then reported two arguments for a correct three-argument prune and the
 * row went red against the fix it exists to police. A window that stops early is worse than no
 * window: it invents a defect, and the natural reaction is to weaken the assertion until it
 * agrees. Bracket depth it is.
 */
const pruneSelectionCalls = (source: string): string[] => {
  const calls: string[] = [];
  const needle = "pruneSelection(";
  let from = 0;
  for (;;) {
    const at = source.indexOf(needle, from);
    if (at === -1) break;
    let depth = 0;
    let end = -1;
    for (let i = at + needle.length - 1; i < source.length; i += 1) {
      const char = source[i];
      if (char === "(" || char === "[" || char === "{") depth += 1;
      else if (char === ")" || char === "]" || char === "}") {
        depth -= 1;
        if (depth === 0) {
          end = i + 1;
          break;
        }
      }
    }
    if (end === -1) break;
    calls.push(source.slice(at, end));
    from = end;
  }
  return calls;
};

const pruneAsUndoWould = (current: CanvasSelection, restored: typeof graphBeforeTheConnection) =>
  pruneSelection(
    current,
    restored.nodes,
    restored.edges,
  );

test("undoing a connection does not leave that connection selected", () => {
  // The defect, stated as a fact about the product rather than about a file. The author drew a
  // line, selected it, pressed ⌘Z, and the line is gone — correctly, that is what undo means.
  const selection = selectEdge(EMPTY_SELECTION, "e-1");
  const pruned = pruneAsUndoWould(selection, graphBeforeTheConnection);

  assert.equal(
    pruned.edge,
    null,
    "a connection the restored graph does not have must not survive the undo",
  );
});

test("after the fix a stale connection no longer wins Del, Escape, or the status bar", () => {
  // The three claims that made the half-prune visible on the reload path, restated for the undo
  // path, and each one read through the product's own rule rather than through the field — a
  // test asserting `selection.edge === null` alone would pass against a prune that dropped the
  // edge for the wrong reason.
  const pruned = pruneAsUndoWould(selectEdge(EMPTY_SELECTION, "e-1"), graphBeforeTheConnection);

  assert.notDeepEqual(
    deleteTarget(pruned),
    { kind: "edge", id: "e-1" },
    "Del must not resolve to a connection the restored graph does not have",
  );
  assert.equal(
    whatEscapeClears(pruned, false),
    "nothing",
    "and Escape must stop being consumed on behalf of a line that is not there",
  );
  assert.equal(
    escapeStepIsHandled("nothing"),
    false,
    "which is what lets the browser's own dismiss through again",
  );
  assert.equal(
    pruned.edge === null ? "0 selected" : "1 connection selected",
    "0 selected",
    "the status bar must not name a connection the restored graph does not have",
  );
});

test("undoing an edge DELETE keeps the connection selected, because the author brought it back", () => {
  // The other direction, and the reason this is a prune rather than a blanket clear — the same
  // argument the node half made two ticks earlier. `edge:remove` restores the graph *with* the
  // connection, so dropping it would leave the author staring at a line they just recovered with
  // nothing selected, which reads as the undo having only half worked.
  const pruned = pruneAsUndoWould(selectEdge(EMPTY_SELECTION, "e-1"), graphAfterTheConnection);

  assert.equal(pruned.edge, "e-1", "a restored connection keeps the selection");
  assert.deepEqual(
    deleteTarget(pruned),
    { kind: "edge", id: "e-1" },
    "so Del still resolves to it, and the key the author pressed still removes it",
  );
});

test("a node selection and a connection cannot both survive the same undo", () => {
  // The status bar's ternary is `selectedEdge ? "1 connection selected" : "${count} selected"`,
  // so a surviving edge id makes the bar describe a line while the canvas highlights a card.
  // The prune has to move the two together, and a fix that only touched the node half would
  // leave this answer describing something the graph does not contain.
  const mixed: CanvasSelection = { nodes: ["a"], focus: "a", edge: "e-1" };
  const pruned = pruneAsUndoWould(mixed, { nodes: ["a", "b"], edges: [] });

  assert.equal(pruned.edge, null, "the connection the undo removed is gone from the selection");
  assert.deepEqual(pruned.nodes, ["a"], "and the node the undo kept is still selected");
  assert.equal(
    pruned.edge === null ? `${pruned.nodes.length} selected` : "1 connection selected",
    "1 selected",
  );
});

test("deleting a node prunes the connections that ended on it", () => {
  // The second call site, and the one whose comment said it could not answer. `removeNodes`
  // computes `nextEdges` on the same breath as `nextNodes` — a connection whose endpoint is
  // gone is exactly the dangling edge validation refuses the whole graph for — so the edge set
  // it installs is already on hand. The comment claiming otherwise is what let this sit.
  const pruned = pruneSelection(
    selectEdge(EMPTY_SELECTION, "e-1"),
    ["a", "b"],
    ["e-survivor"],
  );

  assert.equal(pruned.edge, null, "a connection removed with its endpoint cannot survive");
});

test("undo, redo and reload all land on the SAME prune for the same inputs", () => {
  // Three graph replacements, one rule. The first half of this file fixed the rule's reach; this
  // is what stops the three paths from drifting apart again: an author who undoes and an author
  // who reloads after a conflict get the same answer to the same question about the same
  // deletion, and a fix applied to one and not the other is visible here rather than in a
  // screenshot neither of them took.
  const cases: Array<{ current: CanvasSelection; nodes: string[]; edges: string[] }> = [
    { current: selectEdge(EMPTY_SELECTION, "e-1"), nodes: ["a"], edges: [] },
    { current: selectEdge(EMPTY_SELECTION, "e-1"), nodes: ["a"], edges: ["e-1"] },
    { current: selectNode("gone"), nodes: ["a"], edges: [] },
    { current: { nodes: ["a"], focus: "a", edge: "e-1" }, nodes: ["a"], edges: ["e-1"] },
  ];
  for (const { current, nodes, edges } of cases) {
    assert.deepEqual(
      pruneAsUndoWould(current, { nodes, edges }),
      rebaseAfterReload(current, nodes, edges).selection,
      "the undo's rule and the reload's rule must be the same prune",
    );
  }
});

test("a prune handed no edge ids still does not guess", () => {
  // The half of the design that is correct, asserted here so the wiring fix cannot quietly turn
  // "cannot answer" into "answer no". Nothing in the product takes this path today, and the test
  // is the reason that stays a statement about the contract rather than about a dead branch:
  // if a future caller holds only node ids, this is the answer it is entitled to get.
  const pruned = pruneSelection(selectEdge(EMPTY_SELECTION, "e-1"), ["a"]);

  assert.equal(pruned.edge, "e-1", "a prune with no edge ids to check against must not drop it");
});

test("the undo path hands the prune the restored graph's edges", () => {
  // The wiring assertion, and the reason this file exists. Every rule assertion above passes
  // against an `applyHistoryStep` that never calls `pruneSelection` at all, which is the defect
  // with the fix deleted. The claim is about the ROUTE and about the argument it routes, not
  // about the rule being written twice inside the handler.
  const step = useCallbackBody("applyHistoryStep");
  assert.ok(/pruneSelection\(/.test(step), "the shared step must prune the selection");
  assert.equal(
    callArgCount(step, "pruneSelection"),
    3,
    "the prune must receive the restored graph's edge ids as well as its node ids; two " +
      "arguments are the half-prune, and the count is taken at paren depth because a trailing " +
      "comma and the comma inside map((edge) => edge.id) are both separators at depth zero",
  );
  assert.ok(
    /restored\.edges\.map\(/.test(step),
    "and the third argument must be built from the RESTORED snapshot, not the graph being replaced",
  );
  assert.ok(
    !/pruneSelection\([^)]*restored\.nodes\.map\([^)]*\)\s*\)/.test(step),
    "a three-argument prune whose third argument is absent is the defect itself",
  );
});

test("the node delete prunes the edge set it is about to install", () => {
  // The second wiring assertion, and it is the one the bad comment would have made redundant.
  // `nextEdges` is the list this call installs, two lines above the prune; a caller that can
  // judge a connection and does not is the same defect arriving by a different route.
  const body = useCallbackBody("removeNodes");
  assert.ok(
    /nextEdges\s*=\s*edges\.filter\(/.test(body),
    "precondition: the edge set the delete installs is computed in this handler",
  );
  assert.equal(
    callArgCount(body, "pruneSelection"),
    3,
    "removeNodes hands the prune the edge list it already has, so it must not be left judging " +
      "a connection against nothing",
  );
  assert.ok(
    /nextEdges\.map\(/.test(body),
    "and that third argument must come from nextEdges",
  );
});

test("every wholesale replacement in the component routes through the three-argument prune", () => {
  // The inventory guard, and the one that would have caught the original omission. Reading the
  // call sites one at a time found two of them; a fourth added next quarter is invisible to any
  // test that names the three that exist now. So this counts every `pruneSelection(` in the
  // component and requires each one to carry the edge set.
  const calls = pruneSelectionCalls(WITHOUT_IMPORTS);
  assert.equal(calls.length, 2, "the component has exactly two wholesale replacements to prune");
  for (const call of calls) {
    // The same instrument as the two assertions above, not a second copy of it: the inline
    // version this replaced started its scan at the open paren and counted every separator at
    // depth 1, so it reported 2 for the correct three-argument call and this row went red
    // against a correct fix. A second implementation of an instrument is a second thing to get
    // wrong, and it was wrong the same way in the same tick.
    const depthCount = callArgCount(call, "pruneSelection");
    assert.equal(
      depthCount,
      3,
      `a pruneSelection call without the edge ids reopens the half-prune: ${call.replace(/\s+/g, " ")}`,
    );
  }
});
