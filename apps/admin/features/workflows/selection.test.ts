/**
 * The selection rules' own tests (REQ-004 slice 2).
 *
 * The browser found both of the bugs this module fixes, and neither was reachable from a test
 * because the logic lived in a 2,200-line component. That is the point of the module: the
 * questions are answerable without a DOM.
 *
 * The two regressions are named explicitly at their tests, because "a test that cannot go
 * red is not a gate": `selection-deselects-on-shift-click` fails if the toggle keeps the
 * focus on the node it just removed, and `del-on-an-edge-wins-over-nodes` fails if the delete
 * target is rebuilt from the node state while an edge is selected.
 */
import assert from "node:assert/strict";
import test from "node:test";

import {
  clearSelection,
  deleteTarget,
  EMPTY_SELECTION,
  escapeStepIsHandled,
  extendGroup,
  focusOrder,
  isNodeSelected,
  membersOf,
  pruneSelection,
  selectAll,
  selectEdge,
  selectGroup,
  selectNode,
  selectionSize,
  toggleNode,
  whatEscapeClears,
  type CanvasSelection,
} from "./selection.ts";

const three = ["trigger-1", "wait-2", "transform-3"];

test("a plain click focuses one node and starts no group", () => {
  const state = selectNode("wait-2");
  assert.deepEqual(state, { nodes: [], focus: "wait-2", edge: null });
  // The card is drawn as selected, so the outline has an answer.
  assert.equal(isNodeSelected(state, "wait-2"), true);
  assert.equal(selectionSize(state), 1);
  // …and Del on it removes exactly that one node.
  assert.deepEqual(deleteTarget(state), { kind: "nodes", ids: ["wait-2"] });
});

test("a marquee selects the group it caught", () => {
  const state = selectGroup(three);
  assert.deepEqual(state.nodes, three);
  // The inspector needs *a* node to show; the last one is the honest choice because the
  // marquee finished on it.
  assert.equal(state.focus, "transform-3");
  assert.equal(selectionSize(state), 3);
  assert.deepEqual(deleteTarget(state), { kind: "nodes", ids: three });
});

test("a marquee that caught nothing selects nothing", () => {
  // The bug this prevents: a zero-area band that "selected" the node under the pointer.
  const state = selectGroup([]);
  assert.deepEqual(state, EMPTY_SELECTION);
  assert.equal(selectionSize(state), 0);
  assert.deepEqual(deleteTarget(state), { kind: "nothing" });
});

test("a duplicated id in a marquee is counted once", () => {
  // The band is computed by overlap, so a card it grazes can only enter once — but the
  // count is what the status bar and the minimap report, and a double count reads as two.
  assert.equal(selectionSize(selectGroup(["wait-2", "wait-2", "wait-2"])), 1);
});

test("shift-click adds a node to the group without stealing the focus", () => {
  const state = toggleNode(selectNode("trigger-1"), "wait-2");
  assert.deepEqual(state.nodes, ["wait-2"]);
  // The plain click left no group, so the new node IS the group, plus the focus.
  assert.equal(state.focus, "trigger-1", "the inspector keeps showing the node being read");
  assert.equal(selectionSize(state), 2);
  // Del removes what is *drawn* as selected, so it must include the focused node too —
  // otherwise the user presses Del and one of two outlined cards survives.
  assert.deepEqual(deleteTarget(state), { kind: "nodes", ids: ["wait-2", "trigger-1"] });
});

test("shift-click takes a node *out* of the group — and drops the focus with it", () => {
  // THE REGRESSION. Before this module, the toggle removed the id from the set and then
  // called `setSelected(node.id)` anyway, so the card the user had just de-selected was
  // still drawn with the single-selection outline and a second Shift+click did nothing
  // visible. The gesture and the highlight disagreed about what had happened.
  const two = selectGroup(["trigger-1", "wait-2"]);
  assert.equal(two.focus, "wait-2");

  const after = toggleNode(two, "wait-2");
  assert.deepEqual(after.nodes, ["trigger-1"]);
  assert.equal(after.focus, "trigger-1", "the focus must not stay on the node that left");
  assert.equal(isNodeSelected(after, "wait-2"), false, "the de-selected card must not be outlined");
  assert.equal(selectionSize(after), 1);
});

test("shift-click on a node that was not the focus keeps the focus", () => {
  const three = selectGroup(["trigger-1", "wait-2", "transform-3"]);
  const after = toggleNode(three, "trigger-1");
  assert.deepEqual(after.nodes, ["wait-2", "transform-3"]);
  assert.equal(after.focus, "transform-3", "the inspector should not jump off a still-live node");
});

test("a shift-marquee adds to the group instead of replacing it", () => {
  const after = extendGroup(selectNode("trigger-1"), ["wait-2", "transform-3"]);
  assert.equal(selectionSize(after), 3, "the plain click's node is still selected");
  assert.deepEqual(membersOf(after), ["trigger-1", "wait-2", "transform-3"]);
});

test("select-all covers every node and focuses the last", () => {
  const after = selectAll(three);
  assert.equal(selectionSize(after), 3);
  assert.equal(after.focus, "transform-3");
  assert.deepEqual(membersOf(after), three);
});

test("clicking an edge selects the edge and steps past the node selection", () => {
  // The inspector is driven by `focus`, and an edge has nothing for it to say: without this
  // the author loses the node they were editing to a panel about a line.
  const after = selectEdge(selectNode("wait-2"), "edge-7");
  assert.deepEqual(after, { nodes: [], focus: null, edge: "edge-7" });
  assert.equal(isNodeSelected(after, "wait-2"), false);
});

test("del on an edge wins over the node selection", () => {
  // An edge is selected to be *removed*, and a user pointing at a line means the line. This
  // rule used to be a branch inside the key handler, which is why nothing could assert it.
  const target = deleteTarget(selectEdge(selectAll(three), "edge-7"));
  assert.deepEqual(target, { kind: "edge", id: "edge-7" });
});

test("del with no edge removes the group in one press", () => {
  // One press, one undo: three nodes removed together is one step, not three.
  assert.deepEqual(deleteTarget(selectAll(three)), { kind: "nodes", ids: three });
});

test("del with nothing selected does nothing rather than deleting the canvas", () => {
  assert.deepEqual(deleteTarget(EMPTY_SELECTION), { kind: "nothing" });
  assert.deepEqual(membersOf(EMPTY_SELECTION), []);
});

test("escape reaches the connection first, then the edge, then the nodes", () => {
  // Escape is "I did not mean that", and it has to reach the thing the user is holding — not
  // whatever happened to be selected three gestures ago.
  const withEdge = selectEdge(EMPTY_SELECTION, "edge-7");
  const withNodes = selectAll(three);

  assert.equal(whatEscapeClears(withNodes, true), "connection");
  assert.equal(whatEscapeClears(withEdge, false), "edge");
  assert.equal(whatEscapeClears(withNodes, false), "nodes");
  assert.equal(whatEscapeClears(EMPTY_SELECTION, false), "nothing");
});

test("escape on a selected edge leaves the nodes alone", () => {
  // An edge click already cleared the nodes, so this is the shape the canvas is in when a
  // user presses Escape: the line goes, the graph stays.
  const after = selectEdge(selectAll(three), "edge-7");
  assert.equal(whatEscapeClears(after, false), "edge");
  assert.equal(after.nodes.length, 0);
});

test("escape is a no-op when there is nothing to clear, and says so", () => {
  // A handler that preventDefault()s on an Escape with no selection swallows the browser's
  // own dismiss — the palette search box, a dialog the user opened with the same key.
  assert.equal(whatEscapeClears(clearSelection(), false), "nothing");
  assert.equal(escapeStepIsHandled(whatEscapeClears(clearSelection(), false)), false);
  assert.equal(escapeStepIsHandled("edge"), true);
});

test("a delete leaves no phantom focus behind", () => {
  // The bug this prevents: `setSelected` guarded on the doomed set while the group set was
  // cleared unconditionally, so after deleting a focused node the inspector kept a node id
  // that no longer existed and the toolbar's Duplicate button stayed enabled.
  const state = pruneSelection(selectAll(three), ["trigger-1", "wait-2"]);
  assert.deepEqual(state.nodes, ["trigger-1", "wait-2"]);
  // The deleted node was the focus, so the focus falls to the first node still on the
  // canvas — a live node, not a dead id, and not null while two nodes remain.
  assert.equal(state.focus, "trigger-1");
  assert.equal(isNodeSelected(state, "transform-3"), false);
});

test("deleting everything leaves an empty selection, not a dangling one", () => {
  const state = pruneSelection(selectAll(three), []);
  assert.deepEqual(state, EMPTY_SELECTION);
  assert.deepEqual(deleteTarget(state), { kind: "nothing" });
});

test("a keyboard pass can reach an edge as well as a node", () => {
  // Otherwise "Del on a selected edge removes it" is a criterion only a pointer can satisfy,
  // and the keyboard-only acceptance pass cannot prove it.
  assert.deepEqual(focusOrder({ nodes: three, edges: ["edge-7"] }), [...three, "edge-7"]);
});

test("the outline, the minimap and the status bar cannot disagree about a selection", () => {
  // One definition of "selected", read three ways. A card that is in the group but not the
  // focus is still drawn as selected — the alternative is a marquee that highlights nothing.
  const state = selectGroup(["trigger-1", "wait-2"]);
  const drawn = three.filter((id) => isNodeSelected(state, id));
  assert.deepEqual(drawn, ["trigger-1", "wait-2"]);
  assert.equal(selectionSize(state), 2);
  assert.equal(state.nodes.length, 2);
});
