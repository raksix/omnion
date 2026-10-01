/**
 * The mutation harness for `step-trace-row.test.ts`.
 *
 * ## Why this file exists
 *
 * This REQ has produced FIVE readings that were green against a defect entirely unchanged, and
 * the fifth one is the reason a harness is needed here rather than a longer list of shape
 * assertions. The pass recorded, **against a panel that was correct**:
 *
 *     shownStepNos:            ["1"]
 *     runStepNos:              [1]
 *     stepsShownButNotInRun:   ["1"]
 *     stepsInRunButNotShown:   [1]
 *
 * The panel had opened (`rowIsMeasurable: true`) and rendered its one step. The two sets are
 * equal, and `["1"].includes(1)` is `false`, so the gate was unsatisfiable on correct code.
 *
 * The guard already in that file could not have caught it, and saying why is the point: it
 * asserted the comparison's **shape** — both directions present, `filter`/`includes` spelled
 * the way the note spells them. All of that was true. It never asked whether the two sides of
 * that comparison could ever be equal, because nothing in the expression doing the comparing
 * can answer that question. Every assertion in the suite was a true statement about a piece
 * of the row, and together they still did not add up to a verdict.
 *
 * ## What a mutation run buys here
 *
 * "The tests pass" is not evidence for a row. These mutations each break the product or the
 * harness in the specific way a matching assertion exists to catch, and the run is only
 * correct if all of them go red. M1 is the tick-61 defect itself, reverted exactly: it is the
 * one that says the new type-coherence assertion is load-bearing rather than decorative.
 */
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("../../../../", import.meta.url));
const WALKTHROUGH = `${ROOT}scripts/qa/walkthrough.cjs`;
const TEST = `${ROOT}apps/admin/features/workflows/step-trace-row.test.ts`;

const original = readFileSync(WALKTHROUGH, "utf8");

const MUTATIONS = [
  {
    name: "M1 the panel's step number goes back to a raw string (THE tick-61 defect)",
    // The finding this harness was written for. `getAttribute` returns a string, the run's
    // `step_no` is a number, and the set comparison is unsatisfiable on a correct product.
    from: 'stepNo: Number(block.getAttribute("data-step-trace-step")),',
    to: 'stepNo: block.getAttribute("data-step-trace-step"),',
  },
  {
    name: "M2 the number is coerced in the comparison instead of at the read",
    // The plausible wrong repair. The two sets become comparable, so the shape assertions still
    // pass, and `stepsWithoutBothSides` and the note's own `shownStepNos` keep the string. A
    // fix applied at one use while a second use keeps the old value is the shape this REQ's
    // mistakes always take, so it is worth a mutation of its own.
    from: 'stepNo: Number(block.getAttribute("data-step-trace-step")),',
    to: "stepNo: block.getAttribute(\"data-step-trace-step\"), // normalised below",
  },
  {
    name: "M3 the payload helper scopes back to the panel, not to the step",
    // The tick-57 defect. My first draft of this mutation rewrote the CALL SITE
    // (`blocks(block, "inputs")`) and survived — because the call site was never the defect.
    // The guard reads the HELPER, and the helper is where the scope was fixed: it takes
    // `stepBlock` and queries inside it. Rewriting the call site while the helper still scopes
    // to the step is a strawman, and a strawman in a mutation harness is worse than no
    // mutation: it reports "the suite is still green" for a defect nobody committed.
    // So the mutation has to touch the construct the assertion is about.
    from: "const block = stepBlock.querySelector(",
    to: "const block = panel.querySelector(",
  },
  {
    name: "M4 'has both sides' accepts either side",
    from: "hasBothSides: inputs !== null && output !== null,",
    to: "hasBothSides: inputs !== null || output !== null,",
  },
  {
    name: "M5 the wire probe stops reading output",
    // The second missing half in the same note: a server that sent `params` and dropped
    // `output` reported a healthy `stepsWithParams`.
    from: 'hasOutput: "output" in step,',
    to: "hasOutput: true,",
  },
  {
    name: "M6 the wire probe counts 'output' as truthy rather than present",
    // A `false` or `0` output is an output. `step.output != null` throws the explicit null away,
    // and the explicit null is the sentence the panel says out loud.
    from: 'hasOutput: "output" in step,',
    to: "hasOutput: step.output != null,",
  },
  {
    name: "M7 one direction of the set comparison is dropped",
    from:
      "stepsInRunButNotShown: runStepNos.filter((no) => !shownStepNos.includes(no)),",
    to: "stepsInRunButNotShown: [],",
  },
  {
    name: "M8 the step numbers are compared as a count instead of a set",
    // Both directions present, and a count that is equal for a panel showing one step against a
    // run with one step for it — which is the case the criterion is about.
    from:
      "stepsShownButNotInRun: shownStepNos.filter((no) => !runStepNos.includes(no)),",
    to:
      "stepsShownButNotInRun: shownStepNos.length === runStepNos.length ? [] : shownStepNos,",
  },
  {
    name: "M9 the row stops saying whether it was measurable at all",
    // The tick-60 vacuity, and this one is NOT expected to survive — the note field it
    // guards is declared in this file's own suite. A first run reported it green here, which
    // is worth one paragraph rather than a `expectSurvives` flag: the assertion lives in
    // `step-trace-target.test.ts` (it guards the target, and the switch was added in the same
    // tick), so the mutation has to be measured against a suite that carries it. Running the
    // wrong suite is the same error as the strawman above — a green answer about a file that
    // was never asked the question.
    test: `${ROOT}apps/admin/features/workflows/step-trace-target.test.ts`,
    from: "rowIsMeasurable: trace !== null && (trace?.steps.length ?? 0) > 0,",
    to: "rowIsMeasurable: true,",
  },
  {
    name: "M10 the target goes back to being derived from the pill the row also measures",
    // The tick-60 finding, kept here because this file guards the panel read and the tick-60
    // target bug made the panel read VOID rather than red. Reverting it must not turn the
    // suite green — the guard in `step-trace-target.test.ts` is the one that owns it, so this
    // mutation is expected to SURVIVE here by design, and the runner reports that honestly.
    from: "const paintedNodeId = runCandidateIds.find((id) => canvasIds.includes(id)) ?? null;",
    to: 'const paintedNodeId = painted.painted.find((entry) => entry.status !== "skipped")?.nodeId ?? null;',
    expectSurvives: true,
  },
];

let failures = 0;
for (const mutation of MUTATIONS) {
  const pattern =
    mutation.from instanceof RegExp ? mutation.from : new RegExp(escape(mutation.from));
  const mutated = original.replace(pattern, mutation.to);
  if (mutated === original) {
    console.log(`✗ ${mutation.name} — MUTATION DID NOT APPLY (the anchor moved)`);
    failures += 1;
    continue;
  }
  writeFileSync(WALKTHROUGH, mutated);
  // A mutation may name the suite that actually carries the assertion it breaks. Defaulting
  // to this file's own suite is right most of the time and silently wrong when the guard
  // moved to a sibling — the run then reports a green answer about a question nobody asked.
  const run = spawnSync(
    process.execPath,
    ["--test", "--experimental-strip-types", mutation.test ?? TEST],
    { cwd: ROOT, encoding: "utf8" },
  );
  const passed = run.status === 0;
  const named = [...run.stdout.matchAll(/^not ok \d+ - (.+)$/gm)].map((m) => m[1].trim());
  if (mutation.expectSurvives) {
    // A mutation that is EXPECTED to survive is still a measurement. Reporting it as a pass
    // would be the same silence the tick-60 note complains about; reporting it as a failure
    // would send the next reader hunting a bug in a guard that is doing its job elsewhere.
    console.log(
      passed
        ? `~ ${mutation.name} — survived here, as expected (step-trace-target.test.ts owns it)`
        : `✗ ${mutation.name} — WRONGLY RED here; the sibling guard owns this, and it went red for a reason this file should not have to explain`,
    );
    if (!passed) failures += 1;
    continue;
  }
  if (passed) {
    console.log(`✗ ${mutation.name} — SURVIVED (the suite is still green)`);
    failures += 1;
  } else {
    console.log(`✓ ${mutation.name} — red: ${named.slice(0, 3).join("; ") || "(suite failed)"}`);
  }
}
writeFileSync(WALKTHROUGH, original);
assert.equal(readFileSync(WALKTHROUGH, "utf8"), original, "the walkthrough must be restored");
console.log(`\n${MUTATIONS.length - failures}/${MUTATIONS.length} mutations behaved as declared`);

function escape(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
