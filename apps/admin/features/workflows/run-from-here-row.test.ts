/**
 * The `run-from-here` row has to be able to go RED, and this file is what proves it can.
 *
 * ## What it found
 *
 * The criterion reads *"After a run each node shows its status pill"*, and the row's own
 * comment above the note claimed *"every node the run touched is painted, and nothing else
 * is"* — a claim of set **equality**, stated in the source as though it were checked. It was
 * not. One direction existed:
 *
 * ```js
 * const paintedButNotInRun = paintedIds.filter((id) => !runNodes.has(id));
 * ```
 *
 * so a canvas that painted the two nodes which ran and painted **nothing** for the skipped
 * prefix reported `pillsPainted: 2` and an empty `paintedButNotInRun` — and the gate the
 * criterion is actually closed on ("`pillsPainted > 0` with `paintedButNotInRun` empty")
 * went green. The direction that catches a missing pill is the one that was missing.
 *
 * ## Why this is the tick-48 shape, and why it kept its disguise
 *
 * The REQ has a documented history of rows that read the state they started in and returned a
 * plausible number against a defect that was entirely unchanged. Those were caught because
 * the row read the *wrong marker*. This one read the right markers, in the right place, and
 * simply never took the other side of the comparison — so nothing about it looked wrong. The
 * tell is available only in the criterion's own words: "each node" is a universal claim over
 * the nodes, and a universal needs both the membership and the non-membership to be right.
 *
 * The skipped prefix is where it bites hardest, and not by accident. `node-status.ts` paints
 * a `skipped` pill for exactly one reason: so an operator can see the prefix was skipped
 * rather than run. Those nodes are rows in the run's steps like any other, so they belong in
 * `runNodes` — and they were the most likely nodes to go unpainted in a regression.
 *
 * ## Why the assertions name CONSTRUCTS
 *
 * The tenth instance of the class this directory keeps re-learning: a name that appears in
 * two places satisfies an assertion made about one of them. `paintedButNotInRun` is both the
 * computed variable and the reported field, so demanding the *name* proves nothing — the
 * computation could be deleted and the report would remain. Every check below therefore
 * names the expression that does the work, in the block that performs it, and the windows
 * are sliced so a mention elsewhere in the row cannot stand in.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const WALKTHROUGH = readFileSync(
  new URL("../../../../scripts/qa/walkthrough.cjs", import.meta.url),
  "utf8",
);

/**
 * The row's own source, from the paint read through the note that reports it.
 *
 * The window ENDS at the shot, not at the note. It ended at the note in the first draft,
 * which excluded the very block two of the assertions are about — and a check for text in
 * a window that does not contain the text is a check that cannot pass. That is the same
 * "window ends at the wrong place" family this directory already documents twice for
 * `indexOf` and bracket matching, and it is worth a distinct name here because the failure
 * is silent: a too-narrow window is a test that reports on a construct it never read.
 */
const ROW = (() => {
  const start = WALKTHROUGH.indexOf("const painted = await page.evaluate");
  assert.notEqual(start, -1, "the run-from-here row must exist");
  const end = WALKTHROUGH.indexOf(
    'await shot(page, "page-workflow-builder-run-from-here")',
    start,
  );
  assert.notEqual(end, -1, "the row must report itself");
  return WALKTHROUGH.slice(start, end);
})();

/** The computation block: the two SET COMPARISONS, not the note that mentions them. */
const COMPARE = (() => {
  const start = ROW.indexOf("const runNodes = new Set(");
  assert.notEqual(start, -1, "the run's node set must be built");
  const end = ROW.indexOf("const skippedPill =", start);
  assert.notEqual(end, -1, "the comparison must be followed by the pill read");
  assert.ok(end > start, "the window must not end before it begins");
  return ROW.slice(start, end);
})();

test("the painted set is compared against the run's set in BOTH directions", () => {
  // The defect, stated as the two expressions that close it. A check for the *name*
  // `paintedButNotInRun` passes against a row that computes it and never reports it, and
  // against a row that reports it and never computes it — which is what tick 55 shipped.
  assert.ok(
    /const paintedButNotInRun = paintedIds\.filter\(\(id\) => !runNodes\.has\(id\)\)/.test(COMPARE),
    "a pill on a node the run never reached is a claim about work the engine did not do",
  );
  assert.ok(
    /const inRunButNotPainted = \[\.\.\.runNodes\]\.filter\(\(id\) => !paintedIds\.includes\(id\)\)/.test(
      COMPARE,
    ),
    "the other direction is the criterion's own claim ('each node shows its status pill') and it was the one missing",
  );
});

test("the run's node set comes from steps, not from the canvas it is compared to", () => {
  // A set built off the canvas would make the comparison trivially empty in the direction
  // that matters: every painted node is in a set made of painted nodes. The nodes have to
  // be named by the RUN — the independent witness — or the equality proves nothing.
  assert.ok(
    /const runNodes = new Set\(\s*\(after\?\.steps \?\? \[\]\)\.map\(\(step\) => step\.node_id\)\.filter\(\(id\) => typeof id === "string"\)/.test(
      COMPARE,
    ),
    "the expectation must come from the run's own steps, or the comparison is circular",
  );
});

test("BOTH directions are REPORTED, and the reported one is a property of the note", () => {
  // `paintedButNotInRun` is a shorthand field in the note, and it was already there — which
  // is exactly why the omission went unnoticed: the note looked complete. The check is made
  // against the NOTE rather than the row, and accepts the shorthand, because the criterion
  // is that the number is read by somebody, not that it is spelled a particular way.
  const noteStart = ROW.indexOf('step: "run-from-here",');
  assert.notEqual(noteStart, -1, "the note must exist");
  const note = ROW.slice(noteStart);
  assert.ok(/\bpaintedButNotInRun\b\s*[:,]/.test(note), "the extra-pill direction is reported");
  assert.ok(/\binRunButNotPainted\b\s*[:,]/.test(note), "the missing-pill direction is reported");
});

test("the note does not claim an equality it no longer computes in one direction", () => {
  // The old comment said "every node the run touched is painted, and nothing else is" above
  // a computation that only checked the second clause. The claim outliving the measurement
  // is what makes a reader believe a gate is stronger than it is, so the sentence now names
  // both halves — and this asserts the *reading*, not that a comment was deleted.
  assert.ok(
    /the painted set and the run's set are the SAME set/.test(ROW),
    "the row must state the set equality it is now actually measuring",
  );
  assert.ok(
    !/every node the run touched is painted, and nothing else is/.test(ROW),
    "a claim of equality above a one-sided comparison is the defect restated",
  );
});

test("the pill count is READ off the canvas, not asserted as a number", () => {
  // This one came back STILL GREEN under mutation, and it is the same defect class as every
  // other check in this file: the criterion is closed on `pillsPainted > 0`, so the field is
  // a gate, and a gate that can be written as a literal is a gate that measures nothing.
  // Hard-coding `pillsPainted: 2` produces a note that reads exactly like a passing pass —
  // which is the most expensive kind of wrong, because the reader has no way to tell.
  //
  // The count has to come from the cards the row itself collected, so the assertion names
  // the read and not the field. `painted.painted` is the array the row built from the DOM,
  // and `.length` on it is the only thing that can report how many cards carried a pill.
  assert.ok(
    /pillsPainted: painted\.painted\.length/.test(ROW),
    "the pill count must be read off the canvas, or a passing note can be written by hand",
  );
  assert.ok(
    /const painted = await page\.evaluate[\s\S]*?data-node-status/.test(ROW),
    "and the canvas read is the one that looks for the pill the product writes",
  );
});

test("a canvas that drops the skipped prefix is distinguishable from a short run", () => {
  // The direction is only useful if the SKIPPED nodes are in the run's set, so this pins
  // that the comparison is not accidentally scoped to settled steps only. `skippedRows` is
  // read separately for the reason field, and a row that filtered `runNodes` to
  // non-skipped steps would leave the missing prefix invisible while still reporting the
  // number.
  assert.ok(
    /const runNodes = new Set\([\s\S]*?\.map\(\(step\) => step\.node_id\)/.test(ROW) &&
      !/runNodes = new Set\([\s\S]{0,200}status !== "skipped"/.test(ROW),
    "the run's set must not be pre-filtered to the steps that ran",
  );
});
