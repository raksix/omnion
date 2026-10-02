/**
 * The `undo-selection-edge` row has to be able to go RED, and this file is what proves it can.
 *
 * ## Why a second row at all
 *
 * The `undo-selection` row shipped two ticks ago and it reads
 * `[data-node-id][data-node-selected='true']`, which is the right marker — the cards write
 * `data-node-selected`, and the tick-48 constant `data-selected` is a substring of it that
 * counts zero forever. But that marker describes a *node* selection, and an edge selection is
 * a different object: `selectEdge` sets `nodes: []` and `focus: null` and puts the id in
 * `edge`. So for a selection left naming a connection the undo just removed, the node count
 * is **zero whether or not the prune ran**. The row cannot go red on the defect its own REQ
 * fixed. That is not a weak assertion — it is an absent reading, wearing the costume of a
 * green one.
 *
 * This is the tick-48 shape exactly one level over. There, a row read the Undo button on a
 * page where no reload had ever happened and returned a plausible number against a defect
 * that was entirely unchanged. Here, a row reads the node marker on a canvas whose surviving
 * selection is an edge, and returns `selectionPruned: true` against a defect that is entirely
 * unchanged. **A row is not proven by the value it returns; it is proven by the defect it can
 * still catch.**
 *
 * ## Why every assertion here is about a CONSTRUCT, never a name
 *
 * The first draft of this file asserted that certain strings appear somewhere in the row's
 * window, and its mutation harness came back **7/12**. Every failure was the same defect, and
 * it is the sixth one in this REQ: a name that appears in two places satisfies an assertion
 * made about one of them.
 *
 * * `getPointAtLength` also appears in the row's **guard** (`typeof hit.getPointAtLength !==
 *   "function"`), so replacing the *measurement* with a bounding box left the assertion green.
 * * `getScreenCTM` also appears in the **null check**, so dropping the transform did the same.
 * * `[data-edge-selected='true']` appears in **both** the pre-undo and the post-undo read, so
 *   breaking only the pre-undo one changed nothing the assertion could see.
 * * `readout` is a **variable name**; naming it `null` still satisfies "the row reads the
 *   status bar".
 * * `edgeWasSelected` is declared as a `const` and also *reported* in the note, so deleting
 *   the report left the name in the window.
 *
 * So every assertion below names the construct that does the work — the assignment, not the
 * mention, and the read inside the block that performs it, not the read in the file. That is
 * the whole difference between a check that can fail and a check that is satisfied by
 * something nearby, and it is why this file is longer than the row it describes.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const WALKTHROUGH = readFileSync(
  new URL("../../../../scripts/qa/walkthrough.cjs", import.meta.url),
  "utf8",
);

/** The row's own source, from the point computation to the note that reports it. */
const ROW = (() => {
  const start = WALKTHROUGH.indexOf("const undoSelEdgePoint = await page");
  assert.notEqual(start, -1, "the undo-selection-edge row must exist");
  const end = WALKTHROUGH.indexOf('note({ step: "undo-selection-edge"', start);
  assert.notEqual(end, -1, "the row must report itself");
  return WALKTHROUGH.slice(start, end);
})();

/** The PRE-undo read: the state the row needs a selection to lose. */
const PRE = (() => {
  const start = ROW.indexOf("const edgePre = await page");
  assert.notEqual(start, -1, "the pre-undo read must exist");
  return ROW.slice(start, ROW.indexOf("const edgeWasSelected"));
})();

/** The POST-undo read: the state the assertion is about. */
const POST = (() => {
  const start = ROW.indexOf("const edgePost = await page");
  assert.notEqual(start, -1, "the post-undo read must exist");
  // `indexOf(needle)` with no second argument searches from the TOP, and this needle's first
  // occurrence is in the `if (!edgeWasSelected)` branch — which sits ABOVE this block. So the
  // slice was taken from a negative index and came back empty, which the suite reported as
  // "the post-undo read does not use the connection's marker" against a correct row. That is
  // the seventh instance in this REQ of one class: a window that ends at the first match
  // rather than the next one. `useCallbackBody` in `undo-selection.test.ts` documents it for
  // bracket matching; the mutation harness in this directory found it for an `indexOf`.
  const end = ROW.indexOf("undoSelEdgeNote = {", start);
  assert.notEqual(end, -1, "the post-undo read must be followed by the note");
  assert.ok(end > start, "the window must not end before it begins");
  return ROW.slice(start, end);
})();

/**
 * The note the row reports when it HAS a reading — the second of the two it builds.
 *
 * The row reports twice, and the two notes answer different questions, so they get separate
 * windows rather than one union. `lastIndexOf` is deliberate: the needle appears in BOTH
 * branches (the missed-click note and the reading), and a window that opens on the first one
 * describes a block that reports a miss — which is how the three evidence assertions below
 * went red against a correct row.
 */
const READ_NOTE = (() => {
  const start = ROW.lastIndexOf("undoSelEdgeNote = {");
  assert.notEqual(start, -1, "the row must build a reading note");
  const before = ROW.indexOf("undoSelEdgeNote = {");
  assert.ok(before !== start, "the row must report a miss separately from a reading");
  return ROW.slice(start);
})();

/**
 * The note the row reports when the click MISSED — the first of the two.
 *
 * Its fields are the evidence for a conclusion the browser has to be trusted to draw, and
 * they are asserted in their own window because that is the block that carries them. Asserting
 * them over the whole row would be satisfied by a mention in the *other* branch, which is the
 * eighth instance in this REQ of one class: a name that appears in two places satisfies an
 * assertion made about one of them.
 */
const MISS_NOTE = (() => {
  const start = ROW.indexOf("undoSelEdgeNote = {");
  const end = ROW.lastIndexOf("undoSelEdgeNote = {");
  assert.ok(start !== -1 && end > start, "the row must report a miss and a reading separately");
  return ROW.slice(start, end);
})();

test("the row presses the gesture it is measuring", () => {
  // A row that only reads the state it started in is the tick-48 defect, one level over. The
  // keypress is the exit under test; everything else is a precondition or a reading.
  assert.ok(
    /page\.keyboard\.press\("Control\+z"\)/.test(ROW),
    "the row must cause the undo it measures",
  );
});

test("the undo is read after the WRITE lands, not after a timer that happens to match it", () => {
  // **TICK 62'S FINDING: the same defect tick 61 removed from `run-from-here`, in the one
  // place that tick could not reach, and it is worse here for a reason nobody would guess.**
  //
  // The row was:
  //
  //     await page.keyboard.press("Control+z");
  //     await page.waitForTimeout(1200);
  //     const edgesAfterUndo = (await readGraph())?.edge_count ?? 0;
  //
  // `AUTOSAVE_MS` in `builder-view.tsx` is **1_200**. The wait is not a loose guess at how long
  // a save takes — it is a guess sitting EXACTLY ON the debounce boundary, so the row races the
  // write it is measuring and the verdict is decided by which side of a timer the autosave
  // lands on. `edgeRemovedByUndo` is then `false` on a rule whose undo removed the connection
  // perfectly, and the row goes on to report a selection that was never pruned by a product
  // that never failed.
  //
  // **The obvious fix is wrong, and that is the part worth keeping.** Polling `edge_count` for
  // stability is satisfied immediately: a graph whose debounce has not fired is *stable*. Two
  // identical readings of a count prove only that nothing has changed — which is precisely the
  // state the wait exists to rule out. `settleRun` does not have this hole because it waits for
  // a run to stop MOVING after having observed it start, so a run that never starts is the
  // failure it reports rather than a success. `settleGraph` therefore takes the *witness* of a
  // write: `graph_version` is advanced by every write, so two identical readings of the
  // VERSION cannot happen until the write has landed.
  assert.ok(
    /const undoWrite = await settleGraph\(page, readGraph, edgesVersionBeforeUndo\)/.test(ROW),
    "the undo must be read after the server says the write landed; a fixed wait races the autosave",
  );
  // …and the fixed wait must be GONE from this block, named exactly. It is not merely
  // unnecessary: 1200 is the boundary itself, so leaving it in place beside the poll would
  // make the row report the earlier of the two readings and the poll a decoration.
  assert.ok(
    !/await page\.waitForTimeout\(\s*\d+\s*\)/.test(ROW),
    "a fixed delay here is wrong in the same direction every time and only on a loaded box",
  );
  // The witness has to be the version the row read BEFORE the gesture. Handing the helper a
  // version read after the keypress is the same defect wearing a new name: nothing to move
  // away from, so `changed` is true on any write at all, including an unrelated one.
  assert.ok(
    /const edgesVersionBeforeUndo = edgesGraphBeforeUndo\?\.graph_version \?\? 0/.test(ROW),
    "the version to move away from must be read before the gesture, or any write will do",
  );
  // **THE SAME DEFECT ON THE OTHER SIDE OF THE SAME GESTURE, and the `!waitForTimeout`
  // assertion above is what caught it.** The row clicked the arc and waited a fixed 500ms
  // before reading the selection. That wait cannot tell a click that MISSED from a click that
  // landed on a canvas that had not repainted — both read `edgeWasSelected: false`, and this
  // row then reports two different verdicts for one state: a miss note on one branch and a
  // prune assertion on the other. So the wait is a poll for the POSITIVE condition, bounded,
  // and it says out loud when the selection never appeared.
  assert.ok(
    /const edgeSelectionWait = await awaitEdgeSelection\(page\)/.test(ROW),
    "the click must be waited for as a GESTURE; a fixed delay reads a missed click and an unrepainted canvas as the same thing",
  );
  // And a helper whose only answer is "settled" is wrong here in the mirror-image way: nothing
  // has been written yet, so there is no movement to witness and the honest answer is "did it
  // appear, and how long did it take".
  const selHelperStart = WALKTHROUGH.indexOf("async function awaitEdgeSelection(");
  assert.ok(selHelperStart !== -1, "the selection wait helper must exist");
  const selHelper = WALKTHROUGH.slice(
    selHelperStart,
    WALKTHROUGH.indexOf("async function interact(", selHelperStart),
  );
  assert.ok(
    /appeared: false/.test(selHelper),
    "a selection that never appeared reported as arrived is the miss defect with the sign flipped",
  );
  // `settleGraph` itself: stability alone is not the stop condition.
  const helperStart = WALKTHROUGH.indexOf("async function settleGraph(");
  // Bounded at the NEXT helper, not at `interact`. `settleGraph` is not the last helper in
  // that gap — `awaitEdgeSelection` follows it — and a window that spans both is satisfied by
  // a sibling's `settled: false`. That is not a hypothetical: the identical bug appeared in
  // `run-from-here-row.test.ts` the moment tick 62 added these helpers, and the mutation that
  // reports a hung run as settled went STILL GREEN against it.
  const helperEnd = WALKTHROUGH.indexOf("async function awaitEdgeSelection(", helperStart);
  assert.ok(helperStart !== -1 && helperEnd > helperStart, "the settle helper must exist");
  const helper = WALKTHROUGH.slice(helperStart, helperEnd);
  assert.ok(
    /current\.state === previous && Number\(previous\) !== Number\(versionBefore\)/.test(helper),
    "an unwritten graph is STABLE: without the version witness this helper returns settled on the first poll, which is the state it exists to rule out",
  );
  assert.ok(
    /settled: false/.test(helper),
    "a write that never arrived reported as settled is `run-from-here`'s defect one level over",
  );
});

test("the note says whether the write arrived, so an unread graph cannot read as green", () => {
  // The gates under it — `edgeRemovedByUndo`, `edgeSelectionPruned` — are measured off a graph
  // the autosave writes. Without this field a reader cannot tell a row that was measured after
  // a real write from one that was measured against a graph nobody has committed, and the two
  // look identical in every other field. Same one-switch rule as `runSettled` and
  // `rowIsMeasurable`: a conjunction nobody can hold under time pressure is how a vacuous gate
  // gets closed.
  assert.ok(
    /\bwriteSettled:\s*undoWrite\.settled/.test(READ_NOTE),
    "a note that cannot say 'the write never arrived' reports a mid-flight read as a verdict",
  );
  // The version pair is the evidence for it: `writeSettled: true` is a claim about a write, and
  // a claim about a write without the version it moved is an assertion.
  assert.ok(/\bversionBefore:/.test(READ_NOTE), "the reading must report the version it left");
  assert.ok(/\bversionAfter:/.test(READ_NOTE), "and the version it found");
});

test("the point is COMPUTED from the stroke, not merely guarded on it", () => {
  // A bezier's bounding box is the rectangle AROUND the arc, so a bounding-box click lands
  // on the desk — where the canvas handler correctly clears the selection, so the row would
  // report "no connection could be selected" and read that as a clean canvas. That is
  // precisely the probe defect the `edge-delete` row already documents, and a row that
  // repeats it measures the probe rather than the product.
  //
  // Asserted on the ASSIGNMENT and not on the name. The name is also in the row's guard —
  // `typeof hit.getPointAtLength !== "function"` — so a check for the bare name passes
  // against a row that never once took a point off the curve. That is the first of the six
  // survivors in the first draft of this file.
  assert.ok(
    /const mid = hit\.getPointAtLength\(hit\.getTotalLength\(\) \/ 2\)/.test(ROW),
    "the point must be the stroke's own midpoint, not a box centre",
  );
});

test("the point is MAPPED through the screen matrix, not merely null-checked on it", () => {
  // `getScreenCTM` is the only transform that accounts for the viewport's pan and zoom.
  // Without it the coordinates are in node space and the click lands wherever the pan left
  // the layer — a miss that looks exactly like a product defect, and the second of the six.
  assert.ok(
    /const screen = mid\.matrixTransform\(ctm\)/.test(ROW),
    "the point must be mapped to screen space, or the click misses after a pan",
  );
});

test("BOTH reads use the connection's own marker, and neither uses the node one", () => {
  // The whole point of the second row, and it is TWO claims rather than one: an edge
  // selection has an empty `nodes` and a null `focus`, so the node marker reads zero for it
  // no matter what the prune did. A pre-undo read on the wrong marker is the more expensive
  // of the two, because it reports "nothing was selected" — the answer that costs a whole
  // tick to diagnose. Splitting this into per-block assertions is the third through fifth of
  // the six: a single check over the whole window passed while either read was broken.
  assert.ok(
    /selected: document\.querySelectorAll\("\[data-edge-selected='true'\]"\)\.length/.test(PRE),
    "the pre-undo read must come off the connection's marker",
  );
  assert.ok(
    /selected: document\.querySelectorAll\("\[data-edge-selected='true'\]"\)\.length/.test(POST),
    "and so must the post-undo read",
  );
  assert.ok(
    !/data-node-selected/.test(PRE) && !/data-node-selected/.test(POST),
    "the node marker is zero for an edge selection, so reading it measures nothing",
  );
});

test("a bare [data-edge-selected] matches every edge, so the value must be pinned", () => {
  // The shortened form is the tick-48 constant a second time: a selector that looks right and
  // counts all of them. The attribute is written with an explicit `='true'`.
  assert.ok(
    !/querySelectorAll\("\[data-edge-selected\]"\)/.test(ROW),
    "a bare [data-edge-selected] matches selected and unselected alike",
  );
});

test("BOTH sides read the status bar, and the post side reads the DOM", () => {
  // The product's own claim, which is what the author sees. A selection left naming a
  // connection the canvas does not draw is announced as "1 connection selected (Del removes
  // it)", and that sentence is the one thing in the whole screen that cannot be explained by
  // a CSS class that failed to render.
  //
  // Asserted on the READ, not the variable name: `readout` is a name, and naming it `null`
  // satisfies "the row reads the status bar" while measuring nothing — the sixth survivor,
  // and the one that most directly makes this file worth its length.
  assert.ok(
    /readout: el \? \(el\.textContent \?\? ""\)\.trim\(\) : null/.test(PRE),
    "the pre-undo read must take the status bar's sentence from the DOM",
  );
  assert.ok(
    /readout: el \? \(el\.textContent \?\? ""\)\.trim\(\) : null/.test(POST),
    "and so must the post-undo read, or the claim is never checked after the undo",
  );
  assert.ok(
    /document\.querySelector\("\[data-builder-selection\]"\)/.test(ROW),
    "the element read is the status bar's own marker, not a selector that matches nothing",
  );
});

test("the assertion is a CONJUNCTION, and it names the exact words", () => {
  // Reading the words but not the marker, or the marker but not the words, each leaves a way
  // for the defect to hide: the highlight can be dropped for an unrelated reason, and the
  // status bar can be conditioned on something other than `selectedEdge`. Both together have
  // to be clear for the reading to mean "nothing survived".
  //
  // Both halves of the conjunction are asserted, and the phrase is exact: a check for
  // `"selected"` alone is satisfied by the node-selection sentence the same element would
  // print, so the conjunction would pass over a canvas that still announces a selection.
  assert.ok(
    /edgeSelectionPruned:\s*\n?\s*edgePost\.selected === 0 && !\(edgePost\.readout \?\? ""\)\.includes\("connection selected"\)/.test(
      ROW,
    ),
    "the prune assertion must be the marker AND the words, or it is either half of the answer",
  );
});

test("every precondition is REPORTED, not merely used", () => {
  // `selected === 0` after an undo is ALSO the answer on a rule with no connections, and on
  // a page where the click never landed — so each precondition travels with the reading.
  //
  // Asserted on the note, not the window. `edgeWasSelected` and `toolbarClaimedIt` are
  // `const` declarations that a row can use and never report, and a check made over the
  // whole window cannot tell those two states apart — which is how a precondition quietly
  // stops being read by anybody. `READ_NOTE` opens at the note, so the declarations above it
  // are outside the window and a mention of the name cannot stand in for the report.
  //
  // The separator is `[:,]`, not `:`, because the row uses the object SHORTHAND
  // (`edgeWasSelected,`). The first draft of this assertion demanded the explicit
  // `edgeWasSelected: edgeWasSelected` and went red against a correct row — the ninth
  // instance in this REQ of one class. What has to be true is that the field is a property
  // of the reported note, and both spellings report it.
  assert.ok(
    /\bedgeWasSelected\b\s*[:,]/.test(READ_NOTE),
    "edgeWasSelected is a precondition, and a reading without it is the tick-48 defect",
  );
  assert.ok(
    /\btoolbarClaimedIt\b\s*[:,]/.test(READ_NOTE),
    "toolbarClaimedIt says the status bar agreed before the undo, which is the half that makes the after-reading mean something",
  );
  assert.ok(
    /edgeRemovedByUndo: edgesAfterUndo < edgesBeforeUndo/.test(READ_NOTE),
    "the row must prove the undo actually took the connection away",
  );
  assert.ok(
    /edgesBefore: edgesBeforeUndo/.test(READ_NOTE),
    "and report the count it compared against, so the claim is checkable from the note",
  );
});

test("a missed click reports its EVIDENCE, not a conclusion", () => {
  // "the click missed the curve" is a conclusion with two ordinary causes — the point is
  // off-viewport after a pan, and a card is sitting on the arc — and they need different
  // fixes. A note that cannot tell them apart costs a tick either way, which is exactly what
  // the `edge-delete` row learned by getting it wrong.
  for (const field of ["onEdge", "inViewport", "edgesOnCanvas"]) {
    assert.ok(
      MISS_NOTE.includes(field),
      `${field} is the evidence for a missed click, and a conclusion without it is a guess`,
    );
  }
});

test("the row distinguishes 'no connection exists' from 'the click missed'", () => {
  // Two different blockers with two different fixes — one is a fixture problem, the other is
  // a gesture problem — and collapsing them into a single "attempted: false" is how a probe
  // defect spent three ticks being read as a product defect.
  assert.ok(
    /edgesBeforeUndo === 0 \? "the rule has no connection to select" : "the click missed the curve"/.test(
      MISS_NOTE,
    ),
    "both blockers must be named separately, with the condition that tells them apart",
  );
});
