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

/**
 * The write-gesture class guard — the test whose absence is why this row still held the defect
 * that its three siblings had already been fixed for.
 *
 * ## The shape of the mistake
 *
 * Ticks 61 and 62 found `waitForTimeout(1200)` after a `Control+z` in the undo's edge rows.
 * `AUTOSAVE_MS` in `builder-view.tsx` is **1_200**, so the sleep raced the write it existed to
 * measure. Tick 62 fixed three sites and called this row's gesture "the same gesture" in its own
 * comment — while this row kept its own 1200ms. **Fixing the sites a report names leaves every
 * unnamed site holding the defect.** A row-by-row audit found the fourth one in minutes; the
 * three fixes had taken a tick each.
 *
 * So the check is a PATTERN search over the whole builder pass rather than another row-specific
 * claim: every write gesture in `runWorkflowBuilderDepth` has to be followed by one of the
 * settle helpers before the next note. That way a fifth site cannot be added without this file
 * going red, which is the property the row-specific tests do not have.
 *
 * ## Why the window ends at the next note
 *
 * The window runs from the gesture to the next `note({ step:` — NOT to the next settle helper.
 * A window ending at the next helper is satisfied by the *following* row's helper, which is how
 * tick 62's new helpers silently un-guarded a sibling (`run-from-here` went 13/13 → 12/13 with
 * its own mutation still green). A window that spans past the construct under test is a window
 * satisfied by the wrong occurrence, and it fails as a green suite.
 */
const BUILDER_PASS = (() => {
  const start = WALKTHROUGH.indexOf("async function runWorkflowBuilderDepth");
  assert.notEqual(start, -1, "the builder depth pass must exist");
  const end = WALKTHROUGH.indexOf("\nasync function ", start + 10);
  assert.notEqual(end, -1, "the builder depth pass must end at the next top-level pass");
  return WALKTHROUGH.slice(start, end);
})();

/** Gestures that change the graph, and so need the write to land before they are read. */
const WRITE_GESTURES = [
  { pattern: /page\.keyboard\.press\("Control\+z"\)/g, name: "Control+z" },
  { pattern: /page\.keyboard\.press\("Delete"\)/g, name: "Delete" },
];

/**
 * The helpers that make a gesture's reading trustworthy, positive and negative.
 *
 * `awaitGraphUnchanged` belongs here for the opposite reason to the others: it does not wait for
 * the write to arrive, it waits long enough that an absent write is a real absence. A row that
 * asserts "the lock refused this" needs that, and `settleGraph` would return `settled: false` on
 * a correctly-locked page — the right answer to the wrong question.
 *
 * Each helper carries the kind of claim it can support, because the two kinds are not
 * interchangeable: `settleGraph`/`settleRun` support "the write happened", `awaitEdgeSelection`
 * supports "the thing appeared", and `awaitGraphUnchanged` supports "the write did not happen,
 * and was given the chance to".
 */
const SETTLE_HELPERS = [
  "settleGraph(",
  "settleRun(",
  "awaitEdgeSelection(",
  "awaitNodeSelection(",
  "settleCanvasCards(",
  "awaitGraphUnchanged(",
];

/**
 * The next note, whatever shape it is written in.
 *
 * This was `indexOf("note({ step:")` in the first draft, and it was WRONG: most notes in this
 * file are written across lines as `note({\n  step: "edge-delete",`, so the single-line search
 * skipped them and the window ran on to the next single-line note — 100KB for `edge-delete`,
 * which then condemned a `waitForTimeout(400)` belonging to a site two hundred lines away.
 *
 * A window that reaches past the construct under test is the failure this directory already
 * documents twice (`run-from-here` 13/13 → 12/13, and the `indexOf` window that excluded its own
 * block), and it is wrong in BOTH directions: too wide condemns innocent sites and too narrow
 * cannot pass. Found by dumping the window the guard actually read and comparing it to the
 * verdict it printed, which is the only way the two can be trusted to describe the same thing.
 */
const NEXT_NOTE = /\bnote\(\s*\{\s*step:/g;

test("every write gesture waits for the write instead of guessing its duration", () => {
  const offenders: string[] = [];
  let seen = 0;

  for (const { pattern, name } of WRITE_GESTURES) {
    for (const match of BUILDER_PASS.matchAll(pattern)) {
      seen += 1;
      const index = match.index ?? 0;
      // The window ends at the NEXT NOTE, so the helper that satisfies this row cannot be the
      // next row's helper.
      NEXT_NOTE.lastIndex = index;
      const noteAt = NEXT_NOTE.exec(BUILDER_PASS)?.index ?? -1;
      const window = BUILDER_PASS.slice(index, noteAt === -1 ? BUILDER_PASS.length : noteAt);
      // A window this wide is not a row, it is a region of the file. Assert the shape of the
      // instrument as well as its verdict, so a future note format that the search misses fails
      // loudly instead of silently widening every window in the suite.
      assert.ok(
        window.length < 4000,
        `the window after a ${name} is ${window.length} chars: the next-note search missed a ` +
          `note format and this is judging the whole rest of the pass`,
      );
      const settled = SETTLE_HELPERS.some((helper) => window.includes(helper));
      // A fixed sleep ANYWHERE in the window is the defect only when it stands in for the
      // gesture's OWN claim. A row may legitimately contain several gestures — the locked-builder
      // row presses `Delete`, `c`, `Enter`, `Escape` and `Control+z`, and each is measured by its
      // own helper. Flagging every sleep in the row would condemn the unrelated `c`/`Enter` link
      // probe for the `Delete` that precedes it, which is the "widen until it agrees" failure in
      // its purest form: a rule that cannot distinguish the defect from its neighbour.
      //
      // So the sleep is judged by POSITION. The claim under test is the first read that follows
      // the gesture: anything before that settle helper is the defect, anything after belongs to
      // the next gesture. A fixed sleep sitting BETWEEN a gesture and its helper is exactly the
      // race, and a sleep beyond the helper cannot be what the gesture's reading raced.
      const firstHelper = SETTLE_HELPERS.map((h) => window.indexOf(h)).filter((at) => at >= 0);
      const helperAt = firstHelper.length > 0 ? Math.min(...firstHelper) : -1;
      const sleepAt = window.search(/page\.waitForTimeout\(\d+\)/);
      // Only a sleep that appears BEFORE the first settle helper can be standing in for it.
      const sleepBeforeHelper = sleepAt !== -1 && (helperAt === -1 || sleepAt < helperAt);
      if (!settled || sleepBeforeHelper) {
        offenders.push(
          `${name} at offset ${index}: ` +
            `${settled ? "" : "no settle helper; "}` +
            (sleepBeforeHelper
              ? `fixed waitForTimeout(${window.match(/page\.waitForTimeout\((\d+)\)/)?.[1]}) ` +
                `before its settle helper`
              : "no fixed sleep before its helper"),
        );
      }
    }
  }

  assert.ok(seen > 0, "the search must actually find gestures, or this guard guards nothing");
  assert.deepEqual(
    offenders,
    [],
    "a write gesture measured after a fixed sleep reports a graph nobody has committed:\n" +
      offenders.join("\n"),
  );
});

test("the row's own write is settle-driven and says whether it settled", () => {
  // The row-specific half, kept beside the pattern guard so the fix is named as well as
  // enforced. `writeSettled` is the field that makes the reading honest: `cardRemovedByUndo`
  // on an unwritten graph is a card that vanished from the canvas without the undo having been
  // saved, and without the field that reading is indistinguishable from a working prune.
  assert.ok(
    !/keyboard\.press\("Control\+z"\);\s*\n\s*await page\.waitForTimeout/.test(ROW),
    "the row must not measure its undo after a fixed sleep",
  );
  assert.ok(/settleGraph\(/.test(ROW), "the row must wait for the server's graph_version to move");
  assert.ok(
    /versionBeforeUndoAdd\s*=\s*\(await readGraph\(\)[\s\S]{0,40}graph_version/.test(ROW),
    "the witness is the version read BEFORE the gesture — reading it after cannot rule out " +
      "an unwritten graph, because the debounce may not have fired yet",
  );
  for (const field of ["writeSettled", "graphVersionBefore", "graphVersionAfter"]) {
    assert.ok(
      ROW.includes(field),
      `${field} must be reported: the readings under it are about an uncommitted graph when ` +
        `writeSettled is false`,
    );
  }
});

test("the row reads the words the status bar prints, not only a class", () => {
  // The status bar announces the count in prose, so a selection the canvas is not drawing is
  // visible in two places. Reading only the DOM marker would still catch the defect; reading
  // the words as well means a screen that draws a phantom can no longer be reported as clean
  // from a selector alone.
  assert.ok(/data-builder-selection/.test(ROW), "the status bar's selection text must be read");
  assert.ok(/selectionTextAfter:/.test(ROW), "and reported");
});
