/**
 * The `step-trace` row's TARGET has to be possible, and this file is what proves it.
 *
 * `step-trace-row.test.ts` already guards what the row READS (per-step payloads, both sides,
 * both wire fields, both directions of the step-number comparison). It does not guard what it
 * clicks, and that is where the fourth instance of this REQ's worst habit lives.
 *
 * ## What it found
 *
 * The criterion reads *"clicking the node opens that step's inputs and output"*, and the row
 * chose which node to click by asking the CANVAS for a painted status pill:
 *
 * ```js
 * const paintedNodeId = painted.painted.find((e) => e.status !== "skipped")?.nodeId ?? null;
 * ```
 *
 * `painted` is the `run-from-here` row's own read — the status pill, which is the OTHER half
 * of this same criterion. So a regression in the pill does not make this row red. It makes it
 * **void**: with no card carrying a pill the id is `null`, no click is issued, the inspector
 * never mounts, and then
 *
 * ```text
 * stepsWithoutBothSides: []   an empty list, read as "no step lost a side"
 * stepsInRunButNotShown: []   runStepNos is filtered by a null id, so it is empty
 * stepsWithParams === stepsWithOutput === stepsTotal > 0   read off the wire, healthy
 * ```
 *
 * — **all three green, with the panel shut and not one step rendered.** `panelFound: false`
 * and `stepsShown: 0` were in the note the whole time and neither was in the conjunction, so
 * the next tick would have read three satisfied gates and closed the criterion on a panel
 * that had never opened.
 *
 * ## Why this is the fourth instance of the same defect, and the worst of them
 *
 * Ticks 57, 58 and 59 each moved a read *off the wire and onto the screen* without first
 * checking the screen could be put into the state the read needed. The consequence in all
 * three was an unsatisfiable gate: red forever, on a correct product. This instance is
 * quieter and therefore more dangerous — the gate is not unsatisfiable, it is **vacuous**,
 * and vacuity is indistinguishable from success in every digit the note prints. A gate a
 * defect can make invisible is worse than a missing one, because a missing one is at least a
 * hole somebody can see.
 *
 * The structural cause is the same in all four and is worth naming plainly: **the row's
 * TARGET was derived from the row's own SUBJECT.** Any read whose input is the thing it
 * measures can go void instead of red, and no amount of re-reading the DOM repairs it — the
 * id has to come from an independent witness, which here is the run (`after.steps`), already
 * in hand two lines above.
 *
 * ## Why the assertions name CONSTRUCTS, and why the windows are sliced
 *
 * The recurring lesson of this directory: a token in a *mention* satisfies a claim made about
 * a *use*, and a name appearing in two places satisfies an assertion about one of them. Every
 * check below names the expression that does the work, inside a window that contains it, and
 * `CODE` has the row's own prose stripped first — a harness test that greps a window
 * containing its own explanation always finds the mistake it is warning about (tick 59: the
 * window quoted `waitForTimeout(1500)` in order to explain why that delay is wrong, and the
 * assertion fired on the comment).
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const WALKTHROUGH = readFileSync(
  new URL("../../../../scripts/qa/walkthrough.cjs", import.meta.url),
  "utf8",
);

/**
 * The target block: from the target's derivation to the screenshot that ends the row.
 *
 * The window OPENS at the target derivation, because that is the construct the central test
 * is about — a window starting at the `trace` read would exclude the very statement that made
 * the read possible, and an assertion about a construct outside its window is a check that
 * cannot pass (tick 59: two windows pinned to an anchor the rewrite renamed, both reporting
 * `actual: -1, expected: -1`, a message that reads like a missing row).
 */
const BLOCK = (() => {
  const start = WALKTHROUGH.indexOf("const runCandidateIds = (after?.steps ?? [])");
  assert.notEqual(start, -1, "the step-trace block must derive its target");
  const end = WALKTHROUGH.indexOf(
    'await shot(page, "page-workflow-builder-step-trace")',
    start,
  );
  assert.notEqual(end, -1, "the step-trace block must take a screenshot");
  assert.ok(end > start, "the window must not end before it begins");
  return WALKTHROUGH.slice(start, end);
})();

/** The same window with the PROSE removed, so a mention cannot stand in for a use. */
const CODE = BLOCK.split("\n")
  .filter((line) => !/^\s*(\/\/|\*|\/\*)/.test(line))
  .join("\n");

/** The note alone: what the row REPORTS, as opposed to what it does. */
const NOTE = (() => {
  const start = BLOCK.indexOf('step: "step-trace",');
  assert.notEqual(start, -1, "the note must exist");
  return BLOCK.slice(start);
})();

test("the clicked node is derived from the RUN, not from the pill the row also measures", () => {
  // THE FINDING OF THIS FILE. The target used to be the first non-skipped painted pill, so a
  // pill regression voided the panel read instead of reddening it. The run is the independent
  // witness and is already in hand, so the target comes from `after.steps` and is intersected
  // with the cards the canvas actually drew.
  assert.ok(
    /const paintedNodeId = runCandidateIds\.find\(\(id\) => canvasIds\.includes\(id\)\) \?\? null;/.test(
      CODE,
    ),
    "the clicked node must come from the run's own steps, or a pill regression makes this row vacuous",
  );
  assert.ok(
    /const runCandidateIds = \(after\?\.steps \?\? \[\]\)[\s\S]{0,240}?step\.node_id/.test(CODE),
    "the candidates must be named by the run — the witness that cannot be the subject",
  );
  // The old form has to be gone, named in full rather than by keyword: `painted.painted.find`
  // is what made the read self-referential.
  assert.ok(
    !/paintedNodeId = painted\.painted\.find\(/.test(CODE),
    "deriving the target from the status pill is the defect restated: a defect in the pill leaves the panel unclicked and every gate below it green",
  );
  // …and the TARGET'S OWN STATEMENT may not read the pill at all, however it is spelled.
  //
  // This guard was `paintedNodeId[\s\S]{0,160}?painted\.painted…` — a window across
  // statements — and it went red against the fix, because the line below the declaration is
  // the *evidence* field (`pillChosenNodeId = painted.painted.find(…)`), which is the right
  // thing for a note to carry. So the check is scoped to the single `const paintedNodeId = …`
  // statement, because that is the only place a default can be introduced. The lesson is the
  // one in this file's header and it is worth saying out loud: I wrote it, then wrote a guard
  // that violated it, and the guard failed for exactly the reason the lesson describes.
  const targetStatement = CODE.match(/const paintedNodeId =[^;]*;/);
  assert.ok(targetStatement, "the target must be declared in a single statement");
  assert.ok(
    !/painted\.painted/.test(targetStatement[0]),
    "the target's own declaration may not read the pill; the pill is the subject, not the witness",
  );
});

test("the click is WAITED for, not slept through", () => {
  // `waitForTimeout(500)` was the quiet form of the race the `table-save-survives` row was
  // fixed for: every assertion below a fixed delay is green against a page that has not
  // drawn, and only on a slow machine. The panel writes `data-step-trace` at its root for all
  // three of its states — no-run, node-absent and steps — so waiting on the marker can neither
  // be satisfied by nothing nor time out on a panel that opened without a step in it.
  assert.ok(
    /waitForSelector\(`\[data-step-trace="\$\{paintedNodeId\}"\]`/.test(CODE),
    "the read must wait for the panel's own marker, or a page that has not drawn is a green row",
  );
  assert.ok(
    !/await page\.waitForTimeout\(\s*\d+\s*\);/.test(CODE),
    "a fixed delay is wrong in the same direction every time and only on a slow machine",
  );
});

test("the note says whether the row was measurable at all", () => {
  // The three gates the next tick's conjunction is written over are ALL satisfied by an
  // unopened panel: an empty steps list makes `stepsWithoutBothSides` empty, and a null node
  // id makes `runStepNos` empty so `stepsInRunButNotShown` is empty too. So the note needs ONE
  // switch saying the panel opened and rendered something, and the conjunction means nothing
  // without it. One field, not three conditions — nobody should have to re-derive this under
  // time pressure, which is exactly how a vacuous gate gets closed.
  assert.ok(
    /rowIsMeasurable: trace !== null && \(trace\?\.steps\.length \?\? 0\) > 0/.test(NOTE),
    "a note that cannot distinguish 'no step lost a side' from 'no step was rendered' reports a vacuous gate as a verdict",
  );
  // The vacuity has to be DIAGNOSABLE, not merely flagged: a null target beside a run that
  // named a node is a graph/canvas divergence, which is a different defect from a panel that
  // failed to mount, and the two need different fixes.
  assert.ok(
    /targetFromRun: paintedNodeId !== null/.test(NOTE),
    "a null target is 'the run named a node the canvas does not have', which is not the same defect as a panel that failed to open",
  );
  // When the two derivations disagree, the PILL moved — and the pill is `run-from-here`'s
  // subject. Recording which one the pill would have chosen is what turns "everything green,
  // no idea why" into "the pill is the defect, and that row is the one that measures it".
  assert.ok(
    /pillChosenNode: pillChosenNodeId/.test(NOTE) && /pillChoseSameAsRun: /.test(NOTE),
    "a disagreement between the two derivations must be reported, or a pill regression is diagnosed against the wrong row",
  );
  assert.ok(
    /paintedOnTarget: painted\.painted\.some\(\(entry\) => entry\.nodeId === paintedNodeId\)/.test(
      NOTE,
    ),
    "a panel opened for a node with no pill is the pill's row failing; the note must be able to say so rather than blaming the panel",
  );
});
