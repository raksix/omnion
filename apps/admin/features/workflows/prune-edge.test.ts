/**
 * A prune that drops every node but keeps the edge is a half-prune (REQ-004).
 *
 * ## The defect
 *
 * `pruneSelection(current, alive)` takes the ids of the nodes the adopted graph still has and
 * filters `current.nodes` against them. It returns `edge: current.edge` **verbatim** — so a
 * selection that was pointing at a *connection* survives every wholesale replacement with the
 * connection named still on it.
 *
 * That is the same hole the node half of this rule closed two ticks earlier, and it was closed
 * by giving the prune an `alive` set. The edge was left out of that set, and the only test that
 * mentions it asserts the edge *survives* — naming the limit honestly in a comment ("this
 * function prunes NODES") and calling the result conservative.
 *
 * ## Why "conservative" is wrong on the reload path specifically
 *
 * The justification on that test is real for the direction it considers: a prune must not
 * *invent* an edge, and a connection that is still present is better left selected than dropped.
 * But `rebaseAfterReload` is not a refresh. It is the **Reload** exit of the two-tab conflict,
 * and its entire purpose is to adopt a graph **the other editor changed** — including, in the
 * ordinary case, edges they removed. So the surviving edge id names a connection that is
 * usually *not in the adopted graph at all*, which is the exact sentence the node prune
 * prevents for nodes ("is not a node type the platform knows" is the friendly version).
 *
 * ## What the screen then claims
 *
 * Three places, all of them about a line nobody can see:
 *
 *   * `deleteTarget` gives the edge precedence over any node selection, so `Del` resolves to
 *     `{kind: "edge", id}` and `removeEdge` finds no such edge and returns having changed
 *     nothing. The author pressed the key that removes a selection and the canvas did not move.
 *   * the status bar renders `1 connection selected (Del removes it)` from `selectedEdge` — a
 *     sentence naming a connection, on a canvas with no selected line drawn.
 *   * `whatEscapeClears` returns `"edge"` first, so the first Escape consumes the browser's own
 *     dismiss and the author's selection is still pointing at nothing afterwards.
 *
 * The same three claims the node half already answers, which is why this is the same defect and
 * not a new one.
 *
 * ## The fix
 *
 * `pruneSelection` takes the alive set for edges too. `rebaseAfterReload` is the caller that
 * already has the edge ids of the graph it just adopted, so it passes them. A prune that is
 * handed no edge ids (`removeNodes`, which cannot know them from a node set) keeps the current
 * behaviour — the selection module is shared, and the rule "do not guess" still has to hold for
 * the callers that genuinely cannot answer.
 */
import assert from "node:assert/strict";
import test from "node:test";

import {
  deleteTarget,
  EMPTY_SELECTION,
  escapeStepIsHandled,
  selectEdge,
  whatEscapeClears,
  type CanvasSelection,
} from "./selection.ts";
import { rebaseAfterReload } from "./reload-rebase.ts";

const withEdge: CanvasSelection = { nodes: ["a"], focus: "a", edge: "e-1" };

test("a reload drops an edge selection the OTHER editor removed", () => {
  // The fixture is the ordinary outcome of a conflict: this tab had a connection selected and
  // the other editor's saved graph does not contain it.
  const rebased = rebaseAfterReload(withEdge, ["a"], ["e-survivor"]);

  assert.equal(
    rebased.selection.edge,
    null,
    "a selection naming a connection the adopted graph does not have must not survive the reload",
  );
});

test("a reload KEEPS an edge selection that survived, because the author keeps their place", () => {
  // The other half. Dropping every edge unconditionally would be its own defect: the author
  // who selects a connection and then hits Reload because the *node* count changed would lose
  // their selection for no reason. The prune decides, and this is the case where it says keep.
  const rebased = rebaseAfterReload(withEdge, ["a"], ["e-1"]);

  assert.equal(rebased.selection.edge, "e-1", "a connection the adopted graph still has is kept");
});

test("a prune handed no edge ids does not guess, and keeps the edge", () => {
  // The callers that only know the node set — `removeNodes` prunes against `nextNodes.map(...)`
  // and has no edge list to hand. For them the old behaviour is correct, and this test is what
  // stops the fix from quietly turning "cannot answer" into "answer no".
  const pruned = rebaseAfterReload(withEdge, ["a"], undefined);

  assert.equal(
    pruned.selection.edge,
    "e-1",
    "a prune with no edge ids to check against must not drop a selection it cannot judge",
  );
});

test("after the fix a stale edge no longer wins Del, and no longer swallows Escape", () => {
  // The two claims that made the half-prune visible, both stated through the product's own
  // rules rather than through the field: an edge outranks every node selection, so a surviving
  // stale id is not cosmetic — it is what `Del` resolves to.
  const rebased = rebaseAfterReload(withEdge, ["a"], ["e-survivor"]);

  const target = deleteTarget(rebased.selection);
  assert.notDeepEqual(
    target,
    { kind: "edge", id: "e-1" },
    "Del must not resolve to a connection the adopted graph does not have",
  );

  // With the edge gone, the selection is a single node focus and Escape has something real to
  // clear, so the browser's own dismiss is still refused (handled) — but for the node, not for
  // a line that is not there.
  assert.equal(whatEscapeClears(rebased.selection, false), "nodes");
  assert.equal(escapeStepIsHandled("nodes"), true);
});

test("a node selection and an edge selection cannot both claim the status bar", () => {
  // The status bar renders `selectedEdge ? "1 connection selected" : "${count} selected"`, so a
  // surviving edge id makes the bar describe a connection while the canvas draws a highlighted
  // card. This is the third claim, and it is the one a screenshot shows without any gesture.
  const rebased = rebaseAfterReload(withEdge, ["a"], ["e-survivor"]);

  assert.equal(
    rebased.selection.edge === null ? "N selected" : "1 connection selected",
    "N selected",
    "the bar must not announce a connection the adopted graph does not have",
  );
});

test("selectEdge is the only way an edge enters the selection, and the prune removes it", () => {
  // Guards the shape of the fix: the edge is cleared by the PRUNE, not by every caller
  // remembering to clear it. A future caller that prunes nodes only re-opens the defect
  // silently, because the field is still there and still typed as `string | null`.
  const selected = selectEdge(EMPTY_SELECTION, "e-1");
  assert.equal(selected.edge, "e-1", "selecting a connection is what puts it in the selection");

  const rebased = rebaseAfterReload(selected, ["a"], []);
  assert.equal(rebased.selection.edge, null);
});
