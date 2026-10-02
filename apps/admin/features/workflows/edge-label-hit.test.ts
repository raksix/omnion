/**
 * An edge's port label must not be able to swallow the click that selects the edge.
 *
 * ## The defect this exists for
 *
 * The browser pass read `edge-delete` with the evidence it had been given to build:
 *
 * ```
 * point: { x: 690, y: 268.25, onEdge: true, blockedBy: "text", inViewport: true }
 * selected: false
 * ```
 *
 * `onEdge: true` means `document.elementFromPoint` resolved **inside** the `[data-edge]`
 * group — the click was on the edge, on screen, in the viewport. `blockedBy: "text"` names
 * what won the hit test inside that group, and it is the group's own `<text>`: the port name
 * (`next`, `result`, …) drawn at `(from + to) / 2` with `textAnchor="middle"`.
 *
 * **That is the curve's own midpoint.** It is the one point on a bezier where a person aims
 * and the one point `getPointAtLength(len / 2)` returns, so the label is not an occasional
 * obstruction — it is the guaranteed-obstructed point on the whole edge. And with SVG's
 * default `pointer-events: auto`, a `<text>` node *is* a hit target and wins against the
 * `strokeWidth={14}` transparent stroke drawn beneath it, whose `onPointerDown` is the only
 * thing that selects an edge.
 *
 * So the edge drew, was walkable by keyboard (`tabIndex={-1}` is there), and could not be
 * selected by pointer **at all**. The criterion "Del on a selected edge removes it" had no
 * reachable way in, for anyone — and the two earlier readings of this row (a bounding-box
 * click, then a hit-test report) both described a *probe* problem while the pointer half of
 * the product was dead.
 *
 * ## What is asserted, and what is deliberately NOT
 *
 * * The label is inert to the pointer. This is the whole rule.
 * * The edge still carries a *selectable* stroke: a guard that forbade `pointerEvents`
 *   outright would be satisfied by deleting the hit area along with the label, and an edge
 *   with no hit area is the original defect wearing a different hat. So the fat transparent
 *   stroke and its `onPointerDown` have to still be there — the criterion's keyboard walk
 *   survives either way, and its pointer half does not.
 * * Comments are stripped first, so this file's own prose about `pointerEvents` cannot
 *   satisfy an assertion about it.
 * * No claim is made that the browser now selects the edge. `tsc` cannot hit-test. The
 *   honest claim is the inverse one: reverting the render reaches a red suite instead of
 *   passing silently, and the browser pass reads `edge-delete.removed`.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const VIEW = readFileSync(new URL("./builder-view.tsx", import.meta.url), "utf8");

const stripComments = (source: string): string =>
  source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^[ \t]*\/\/.*$/gm, "");

const view = stripComments(VIEW);

/**
 * The `<g data-edge>` render, so a match cannot be satisfied by some other `<text>`.
 *
 * The slice ends at the group's own `</g>`, NOT at some inner landmark. Cutting at the first
 * `markerEnd` was the first version of this helper, and it silently cut off the port label —
 * the exact element the first test is about — so all three substantive assertions reported
 * red against a **correct** fix. An extraction window that stops before its own subject is
 * the harness defect this file exists to prevent, caught in the harness.
 */
function edgeBlock(): string {
  const at = view.indexOf("data-edge={edge.id}");
  assert.notEqual(at, -1, "the canvas draws no [data-edge] group — the file's shape changed");
  const end = view.indexOf("</g>", at);
  assert.notEqual(end, -1, "the [data-edge] group is never closed — the file's shape changed");
  return view.slice(at, end);
}

test("the port label drawn at the curve's midpoint is inert to the pointer", () => {
  const block = edgeBlock();
  const label = block.match(/<text\b[\s\S]*?<\/text>/);
  assert.ok(label, "the edge group draws no port label — nothing left to obstruct the stroke");

  // The whole rule. `none` is what gives the midpoint back to the stroke underneath.
  assert.match(
    label[0],
    /pointerEvents:\s*"none"/,
    "the edge's port label is hit-testable: it sits on the curve's own midpoint and wins the " +
      "hit test against the transparent stroke, so the edge can never be selected by pointer",
  );
});

test("the label is drawn where a click aims, so the guard is about that node and not another", () => {
  const block = edgeBlock();
  const label = block.match(/<text\b[\s\S]*?<\/text>/);
  assert.ok(label, "the edge group draws no port label");

  // Midpoint of BOTH endpoints, offset by the half-card. The third version of this regex was
  // written against the midpoint of the *sum* and asked for `from.position.x)` — a shape
  // this file never had. It went red against correct code, which is the fourth harness
  // bug in this file, and the reason the assertion now names the whole expression instead
  // of a fragment of it: a regex that has to be re-read next to its subject is one more
  // thing to get wrong than the defect it guards.
  assert.match(label[0], /\(from\.position\.x\s*\+\s*to\.position\.x\)\s*\/\s*2\s*\+\s*CARD_W\s*\/\s*2/);
  assert.match(label[0], /textAnchor="middle"/);
});

test("the fat transparent hit stroke is still there, or the fix removed the way in", () => {
  const block = edgeBlock();
  assert.match(
    block,
    /stroke="transparent"/,
    "the edge lost its wide transparent hit area: a 2px bezier is unclickable on its own",
  );
  assert.match(
    block,
    /strokeWidth=\{14\}/,
    "the edge's hit stroke narrowed: selection depends on a 14px band around the curve",
  );
  assert.match(
    block,
    /pointerEvents:\s*"stroke"/,
    "the hit stroke no longer listens to the pointer, so selecting an edge by clicking it " +
      "cannot work no matter what the label does",
  );
  // And the handler that records the selection — the only writer of `selectedEdge`.
  assert.match(block, /onPointerDown[\s\S]*?selectEdge/);
});

test("the visible hairline is still pointer-inert, so the label was never its problem", () => {
  const block = edgeBlock();
  // `[^>]*` and not `[\s\S]*?`: the second version stops at the word `markerEnd`, which sits
  // in the ATTRIBUTE LIST, and the `style` that answers this assertion comes two lines
  // later — so the slice ended before its own subject and the guard reported red against a
  // hairline that is provably pointer-inert. Same class as the `</g>` bug above, and the
  // reason this file carries three of them as a matter of record.
  const visible = block.match(/<path\b[^>]*markerEnd[^>]*\/>/);
  assert.ok(visible, "the edge group draws no visible stroke");
  assert.match(
    visible[0],
    /pointerEvents:\s*"none"/,
    "the visible hairline became hit-testable and would sit on top of the label's half of " +
      "the same contest — the fix has to be on the label, and the hairline is the control",
  );
});
