/**
 * The `undo-selection` walkthrough row has to be able to go RED, and this file is what proves
 * it can.
 *
 * ## Why a unit test reads a QA script
 *
 * This tests the *instrument*, not the product, and the precedent is `reload-rebase-row.test.ts`:
 * a row shipped in tick 48 that read the Undo button and the selection on a page on which no
 * reload had ever happened, so it returned a plausible number against a defect that was entirely
 * unchanged. Three ticks cited it. A row that cannot go red is an absent reading, and a constant
 * in a report (`[data-selected]` where the cards write `[data-node-selected]`) is the shape of
 * wrong answer nobody suspects.
 *
 * The row under test here has the same exposure from the other direction, and the specific way
 * it could measure nothing is worth naming: **"nothing is selected after the undo" is also the
 * answer on a page where nothing was ever selected, and on a page where ⌘Z did nothing.** So a
 * row that adds two cards, reads the count and reports zero has measured a page, not a defect.
 * The preconditions are therefore asserted as first-class: a card WAS selected, and the undo
 * DID remove it. The second is the one a naive row skips, and it is the one that separates "the
 * prune worked" from "the node was never there".
 *
 * ## What is asserted, and what is deliberately NOT
 *
 * A regex is a weak instrument and this file is honest about where it stops:
 *
 * * Every assertion is a **structural** claim — that the row presses ⌘Z, that it reads the
 *   selection marker the product actually writes, that it reads the two toolbar buttons the
 *   defect left enabled, and that it reports the preconditions beside the reading.
 * * None of it claims the row *passes* in a browser. The honest claim is the inverse: the row is
 *   wired to the code path under test, so a defect in `doUndo` reaches a reading instead of
 *   passing silently.
 * * A regex cannot prove a keypress happened, only that the source asks for one. The comments
 *   say so where a liveness claim would otherwise be implied.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const WALKTHROUGH = readFileSync(
  new URL("../../../../scripts/qa/walkthrough.cjs", import.meta.url),
  "utf8",
);

/** The row's own source, from its comment marker to the note that reports it. */
const ROW = (() => {
  const start = WALKTHROUGH.indexOf("const undoSelAdd = async (kind) =>");
  assert.notEqual(start, -1, "the undo-selection row must exist");
  const end = WALKTHROUGH.indexOf('note({ step: "undo-selection"', start);
  assert.notEqual(end, -1, "the row must report itself");
  return WALKTHROUGH.slice(start, end);
})();

test("the row presses the gesture it is measuring", () => {
  // A row that only reads the state it started in is the tick-48 defect. The keypress is the
  // exit under test; everything else in the row is a precondition or a reading.
  assert.ok(
    /page\.keyboard\.press\("Control\+z"\)/.test(ROW),
    "the row must cause the undo it measures",
  );
});

test("the row builds a selection to lose, through the product's own gesture", () => {
  // Two palette adds, because ONE add is undone by the keypress that is already in the history
  // for it in a way that is hard to read, and because the second add is what leaves the
  // selection on a card the following undo removes. The add is a click on the palette rather
  // than an API call because the click-add ends in `setSelection(selectNode(node.id))` — the
  // product's own selection, not one a probe arranged.
  assert.ok(/undoSelAdd\("wait"\)/.test(ROW), "the first add must be a wait node");
  assert.ok(/undoSelAdd\("transform"\)/.test(ROW), "the second add must be a transform node");
  assert.ok(
    /data-palette-node/.test(ROW),
    "the adds must go through the palette, which is what selects the card",
  );
});

test("the row reads the marker the CARDS write, not one nothing emits", () => {
  // The tick-48 constant. `data-selected` is a substring of `data-node-selected`, so a search
  // for the short form looks right and counts zero forever. Asserted with the attribute's own
  // brackets so a shortened selector cannot satisfy it.
  assert.ok(
    /\[data-node-id\]\[data-node-selected='true'\]/.test(ROW),
    "the selection must be read off the marker the canvas actually writes",
  );
  assert.ok(
    !/\[data-selected/.test(ROW),
    "data-selected is emitted by nothing, so counting it is counting a constant",
  );
});

test("the row reads the two controls the defect left enabled", () => {
  // The defect was not visible in the inspector — it looks its node up and renders `null`,
  // which looks correct. It was visible in the toolbar: Duplicate and Copy read
  // `disabled={!selected}`, and `selected` is a string that survived the undo. A row that
  // only counted cards would have seen a correct-looking screen.
  assert.ok(/builder-duplicate/.test(ROW), "Duplicate's disabled state is part of the reading");
  assert.ok(/builder-copy/.test(ROW), "and so is Copy's");
  assert.ok(
    /duplicateDisabled:/.test(ROW) && /copyDisabled:/.test(ROW),
    "both must be REPORTED, not read and dropped",
  );
});

test("the row reports the preconditions beside the reading", () => {
  // The lesson of the whole file. `selectionPruned: true` is also the answer on a page that
  // was never selected and on a page where the keypress did nothing, so each of these has to
  // travel with the reading rather than being assumed.
  for (const field of [
    "cardWasSelected",
    "cardRemovedByUndo",
    "toolbarOfferedTheSelection",
    "cardsBefore",
  ]) {
    assert.ok(
      ROW.includes(field),
      `${field} is a precondition, and a reading without it is the tick-48 defect`,
    );
  }
  // `cardRemovedByUndo` is the half a naive row skips, and it is the one that separates "the
  // prune worked" from "the node was never on the canvas".
  assert.ok(
    /victimStillDrawn === false/.test(ROW),
    "the row must prove the undo actually removed the selected card",
  );
});

test("the row reads the words the status bar prints, not only a class", () => {
  // The status bar announces the count in prose, so a selection the canvas is not drawing is
  // visible in two places. Reading only the DOM marker would still catch the defect; reading
  // the words as well means a screen that draws a phantom can no longer be reported as clean
  // from a selector alone.
  assert.ok(/data-builder-selection/.test(ROW), "the status bar's selection text must be read");
  assert.ok(/selectionTextAfter:/.test(ROW), "and reported");
});
