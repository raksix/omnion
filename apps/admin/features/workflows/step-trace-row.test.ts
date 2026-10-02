/**
 * The `step-trace` row has to be able to go RED, and this file is what proves it can.
 *
 * ## What it found
 *
 * The criterion reads *"clicking the node opens that step's **inputs and output**"*, and the
 * row read the payload blocks off the PANEL:
 *
 * ```js
 * const block = panel.querySelector(`[data-step-trace-payload="${name}"]`);
 * ```
 *
 * `querySelector` is the FIRST match. The panel renders one Inputs/Output pair inside every
 * `[data-step-trace-step]` container, so that expression answers for the first step whatever
 * the node did. A node with two steps — a switch that branched, the case the `diverged` pill
 * exists to advertise — reported `stepsShown: 2` beside **one** step's payloads, and the
 * second step could have rendered nothing at all with every number in the note unchanged.
 *
 * The product guards this exact loss, on purpose and twice: `step-detail.ts` says *"a map
 * keyed by node is the shape that loses the second branch, and this function's only job is
 * to be the one place that answers 'which steps is this node', so the loss cannot happen
 * twice"* — and `runDetailForNode` returns a list rather than a lookup. The read threw the
 * guard away, which is the shape that makes this worth a test: nothing about the row looked
 * wrong, it just answered a smaller question than the criterion asks.
 *
 * ## The same missing half, a second time in the same note
 *
 * The wire probe read `step.params` and never `step.output`:
 *
 * ```js
 * hasParams: step.params !== undefined,
 * ```
 *
 * so the note carried `stepsWithParams === stepsTotal > 0` — the gate the criterion was
 * written against — while `outputRendered`, the other half of the sentence, was measured
 * against a panel that could only ever have been fed by a half-populated wire. A server that
 * sent `params` and dropped `output` was a healthy reading. That is tick 56's defect in a
 * different place, which is what makes it a habit rather than an accident.
 *
 * ## Why the assertions name CONSTRUCTS
 *
 * The twelfth instance of the class this directory keeps re-learning: a name that appears in
 * two places satisfies an assertion made about one of them. `blocks` is both the helper and
 * the thing under test; demanding the *name* proves nothing, because a helper that takes
 * `(name)` and never uses its first argument passes it. Every check below therefore names
 * the expression that does the work, in the block that performs it, and the windows are
 * sliced so a mention elsewhere in the row cannot stand in.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const WALKTHROUGH = readFileSync(
  new URL("../../../../scripts/qa/walkthrough.cjs", import.meta.url),
  "utf8",
);

/**
 * The row's own source, from the panel read through the note that reports it.
 *
 * The window starts at the panel read and ends at the shot that follows the note. Ending it
 * at the note would exclude the very block two of the assertions are about — and a check for
 * text in a window that cannot contain the text is a check that reports on a construct it
 * never read. That is the "window ends at the wrong place" family this directory already
 * documents three times, and its failure is the quiet one: a test that is confident about a
 * thing it never looked at.
 */
const ROW = (() => {
  const start = WALKTHROUGH.indexOf("const trace = await page.evaluate((nodeId) =>");
  assert.notEqual(start, -1, "the step-trace row must read a panel");
  const end = WALKTHROUGH.indexOf(
    'await shot(page, "page-workflow-builder-step-trace")',
    start,
  );
  assert.notEqual(end, -1, "the row must report itself");
  assert.ok(end > start, "the window must not end before it begins");
  return WALKTHROUGH.slice(start, end);
})();

/** The DOM read: the per-step collection, not the note that mentions it. */
const PANEL_READ = (() => {
  const start = ROW.indexOf("const blocks = (stepBlock, name) =>");
  assert.notEqual(start, -1, "the payload helper must exist");
  const end = ROW.indexOf("}, paintedNodeId);", start);
  assert.notEqual(end, -1, "the panel read must be passed the node it clicked");
  assert.ok(end > start, "the window must not end before it begins");
  return ROW.slice(start, end);
})();

/** The wire probe: the one assertion about the server, and the place `output` was absent. */
const WIRE_READ = (() => {
  const start = ROW.indexOf("const paramsOnWire = await page.evaluate(");
  assert.notEqual(start, -1, "the wire must be read from the API");
  const end = ROW.indexOf(").catch(() => null);", start);
  assert.notEqual(end, -1, "the wire read must be caught");
  assert.ok(end > start, "the window must not end before it begins");
  return ROW.slice(start, end);
})();

test("the payload blocks are read per STEP, not off the panel", () => {
  // The defect, stated as the expression that closes it. `panel.querySelector` is the first
  // match, so it answers for one step whatever the node did; the step's own container is
  // what makes the read cover the node's second branch.
  assert.ok(
    /const blocks = \(stepBlock, name\) => \{\s*const block = stepBlock\.querySelector\(/.test(
      PANEL_READ,
    ),
    "reading the panel's first payload block answers for the first step only, and a node with two steps then reports one step's data as the node's",
  );
  // The collection the note counts has to be the per-step one, which is what makes the
  // reported `inputsRendered`/`outputRendered` totals cover every step rather than one.
  assert.ok(
    /const steps = Array\.from\(panel\.querySelectorAll\("\[data-step-trace-step\]"\)\)\.map\(/g.test(
      PANEL_READ,
    ),
    "the steps must be collected from their own containers before any payload is read",
  );
  assert.ok(
    /hasBothSides: inputs !== null && output !== null/.test(PANEL_READ),
    "'inputs and output' is a conjunction; a step that rendered one side has not opened the other",
  );
});

test("a step missing either side is named, not just counted", () => {
  // The count alone is the tick-48 trap in a new coat: `stepsShown: 2` is satisfied by two
  // blocks, two of which rendered nothing. The gate has to be the list of the ones that did
  // not, and the field that carries it is the criterion's gate — so it is asserted twice,
  // once as the computation and once as the report.
  assert.ok(
    /stepsWithoutBothSides: \(trace\?\.steps \?\? \[\]\)\s*\.filter\(\(entry\) => !entry\.hasBothSides\)/.test(
      ROW,
    ),
    "the note must carry the steps that failed to open both sides, or 'each step's inputs and output' is only ever checked on one",
  );
  assert.ok(
    /stepsWithoutBothSides: \(trace\?\.steps \?\? \[\]\)\s*\n?\s*\.filter\(\(entry\) => !entry\.hasBothSides\)\s*\n?\s*\.map\(\(entry\) => entry\.stepNo \?\? null\)/.test(
      ROW,
    ),
    "the list has to name WHICH steps failed (their numbers), not merely how many: a count of one is the same number whether it is step 2 or a step the reader cannot find",
  );
  assert.ok(
    !/stepsWithoutBothSides: \(\(trace\?\.steps \?\? \[\]\)\.length/.test(ROW),
    "a count is not a list — this is the M8 shape from tick 55, where a field satisfied by a literal reads exactly like a measured one",
  );
});

test("the panel's step numbers are compared against the run's in BOTH directions", () => {
  // Same shape as the pill half, one row up: a set comparison in one direction is half a
  // comparison. The panel is the surface under test; the run is the independent witness, and
  // it is the copy the runner will execute.
  assert.ok(
    /stepsShownButNotInRun: shownStepNos\.filter\(\(no\) => !runStepNos\.includes\(no\)\)/.test(ROW),
    "a step the panel showed that the run does not have is a panel answering a different question",
  );
  assert.ok(
    /stepsInRunButNotShown: runStepNos\.filter\(\(no\) => !shownStepNos\.includes\(no\)\)/.test(
      ROW,
    ),
    "a step the run attributes to this node that the panel did not open is the half that catches a dropped branch",
  );
});

test("the two step-number sets are TYPE-COHERENT, and this is the one the shape check missed", () => {
  // TICK 61, and the pass caught it on a panel that was CORRECT.
  //
  // `shownStepNos` came off `getAttribute("data-step-trace-step")` — a STRING — and
  // `runStepNos` came out of the run's JSON `step_no` — a NUMBER. So the panel said `["1"]`
  // and the run said `[1]`, and `["1"].includes(1)` is `false` in BOTH directions. The pass
  // recorded, against a panel that had opened and rendered its one step:
  //
  //     stepsShownButNotInRun: ["1"]
  //     stepsInRunButNotShown: [1]
  //
  // Both non-empty, both fiction. The sets are equal; the comparison simply could not see it.
  // So the gate `stepsInRunButNotShown: []` was **unsatisfiable on a correct product** — the
  // fifth instance of this REQ's habit, and the first one the SHAPE check above could not
  // have found: it asserted that both directions are present and spelled correctly, which
  // they were. It never asked whether the two sides could ever be equal.
  //
  // The lesson is narrow and worth stating as its own rule, because it is not the
  // "read the subject" rule the previous four instances were: **a set comparison between two
  // sources is only a comparison if both sides are the same type, and nothing in the
  // expression doing the comparing can tell you that.** `["1"]` and `[1]` are different sets
  // by every rule the language has. Coercing at the boundary is the repair; the guard has to
  // be about the coercion, because the comparison will keep its shape forever and be wrong
  // forever without it.
  assert.ok(
    /stepNo: Number\(block\.getAttribute\("data-step-trace-step"\)\)/.test(PANEL_READ),
    "the panel's step number must be normalised to a number where it leaves the DOM; compared as a string against the run's number, the set equality is unsatisfiable on a correct product",
  );
  // The old form has to be gone, named exactly — a bare `getAttribute` is the defect.
  assert.ok(
    !/stepNo: block\.getAttribute\("data-step-trace-step"\)/.test(PANEL_READ),
    "a raw getAttribute here is the unsatisfiable gate restated: ['1'] never equals [1]",
  );
  // The coercion has to happen at the READ, not inside the comparison. `stepNo` is consumed
  // by `stepsWithoutBothSides` and by the note's own `shownStepNos` as well, and a normalise-
  // at-each-use fix is three chances to forget one — the shape this REQ's mistakes always
  // take is a fix applied at one use while a second use keeps the old value.
  assert.ok(
    !/stepsWithoutBothSides:[\s\S]{0,200}?Number\(/.test(ROW),
    "the number is normalised once at the DOM boundary; coercing again per use is what leaves two of the three uses on the old value",
  );
});

test("the wire probe reads BOTH payloads, and distinguishes an absent key from an empty one", () => {
  // `output` was never read here, so the note reported the `params` gate beside an
  // `outputRendered` number that no server assertion supported.
  assert.ok(
    /hasParams: step\.params !== undefined/.test(WIRE_READ),
    "the inputs half of the wire read must still be there",
  );
  assert.ok(
    /hasOutput: "output" in step/.test(WIRE_READ),
    "the output half was missing: a server that sent params and dropped output reported a healthy stepsWithParams",
  );
  // `step.output ?? null` would fold "the step produced nothing" into "the server never
  // sent the key", and the panel renders two different sentences for those.
  assert.ok(
    !/hasOutput: step\.output !== undefined && step\.output !== null/.test(WIRE_READ),
    "counting an explicit null as absent folds two different facts into one count",
  );
  assert.ok(
    /stepsWithOutput: \(paramsOnWire \?\? \[\]\)\.filter\(\(step\) => step\.hasOutput\)\.length/.test(
      ROW,
    ),
    "the output count has to be reported, or it is computed and never read (tick 55's M10)",
  );
});
