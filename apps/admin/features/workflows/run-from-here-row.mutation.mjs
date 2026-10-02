#!/usr/bin/env node
/**
 * Mutation harness for the `run-from-here` row's instrument test.
 *
 * ## What this is for
 *
 * A test that is green on a correct row and on a broken one is the failure mode this
 * directory keeps re-learning. The defect this file exists for was invisible precisely
 * because the row looked complete: the note had a `paintedButNotInRun` field, the criterion
 * had a gate written in terms of that field, and both were satisfied by a canvas that
 * painted **nothing** for the nodes the run touched. Only taking the other side of the set
 * comparison found it. So each assertion below is proven by taking the thing it measures
 * away and confirming the suite goes red.
 *
 * ## Why every edit is SCOPED to the row window
 *
 * The previous harness in this directory learned the hard way that a global `String.replace`
 * hits the FIRST occurrence, and that three of its twelve mutations had been silently editing
 * a *different* row while reporting an honest result for an untouched test. So each edit
 * here is applied inside the row's own window, the window is asserted to be the expected one
 * before anything is written, and the restore is byte-for-byte on every path including a
 * throw — a harness that leaves the repo edited is not run twice.
 */
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { copyFileSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const WALKTHROUGH = join(HERE, "..", "..", "..", "..", "scripts", "qa", "walkthrough.cjs");
const BACKUP = `${WALKTHROUGH}.mutation-backup`;
const TEST = join(HERE, "run-from-here-row.test.ts");

/** The row's own window — the same slice the instrument test reads, plus the note. */
const ROW_START = "const painted = await page.evaluate";
const ROW_END = 'await shot(page, "page-workflow-builder-run-from-here")';

const rowWindow = (source) => {
  const start = source.indexOf(ROW_START);
  const end = source.indexOf(ROW_END, start);
  assert.notEqual(start, -1, "the run-from-here row must exist");
  assert.notEqual(end, -1, "the row must take a screenshot");
  return { start, end };
};

/** Every mutation: a name, a find, and a replace — all inside the row window. */
const MUTATIONS = [
  {
    name: "M1 the missing direction is deleted — the defect this file was written for",
    find: "const inRunButNotPainted = [...runNodes].filter((id) => !paintedIds.includes(id));",
    replace: "const inRunButNotPainted = [];",
  },
  {
    name: "M2 the missing direction becomes a constant, so it is reported but never measured",
    find: "const inRunButNotPainted = [...runNodes].filter((id) => !paintedIds.includes(id));",
    replace: 'const inRunButNotPainted = "none";',
  },
  {
    name: "M3 the two directions become one — the set is compared with itself",
    find: "const inRunButNotPainted = [...runNodes].filter((id) => !paintedIds.includes(id));",
    replace: "const inRunButNotPainted = paintedIds.filter((id) => !runNodes.has(id));",
  },
  {
    name: "M4 the missing direction is computed and then thrown away",
    find: "      inRunButNotPainted,\n",
    replace: "",
  },
  {
    name: "M5 the run's node set is built off the CANVAS, making the comparison circular",
    find: `(after?.steps ?? []).map((step) => step.node_id).filter((id) => typeof id === "string"),`,
    replace: `painted.painted.map((entry) => entry.nodeId).filter((id) => typeof id === "string"),`,
  },
  {
    name: "M6 the run's set is pre-filtered to the steps that ran, hiding the skipped prefix",
    find: `(after?.steps ?? []).map((step) => step.node_id).filter((id) => typeof id === "string"),`,
    replace: `(after?.steps ?? []).filter((step) => step.status !== "skipped").map((step) => step.node_id).filter((id) => typeof id === "string"),`,
  },
  {
    name: "M7 the original direction is deleted, so only the new one is checked",
    find: "const paintedButNotInRun = paintedIds.filter((id) => !runNodes.has(id));",
    replace: "const paintedButNotInRun = [];",
  },
  {
    name: "M8 the pill count is reported as a constant rather than a read",
    find: "pillsPainted: painted.painted.length,",
    replace: "pillsPainted: 2,",
  },
  {
    name: "M9 the row's comment goes back to claiming an equality it no longer measures",
    find: "the painted set and the run's set are the SAME set",
    replace: "every node the run touched is painted, and nothing else is",
  },
  // ---- TICK 61: the run is read before it settles -----------------------------------------
  // These four sit BEFORE the row window opens, which is why `rowWindow` above could not have
  // found them. The harness learned this the same way the test did: a window that starts one
  // block too late is a check that reports on a construct it never read, and the mutation
  // harness is where that is cheapest to discover. `mutateIn` widens the window rather than
  // loosening the assertion.
  {
    name: "M10 the run is read once after a fixed delay again (THE tick-61 defect)",
    in: "run",
    find: "const settled = await settleRun(page, readRun);",
    replace: "await page.waitForTimeout(2500);\n      const settled = { run: await readRun(), settled: true };",
  },
  {
    name: "M11 the settle poll accepts ONE unchanged reading instead of two",
    // My first attempt at this mutation APPENDED a second early-return beside the real check
    // and left the real check in place, so the row still required two readings and the suite
    // stayed green — a strawman, and this harness now has a second one to report on. The
    // regression has to REMOVE the two-reading requirement, not sit next to it.
    //
    // **The anchor was the one-line form, and tick 88 replaced it with a block.** A `find`
    // string that no longer occurs does not fail a mutation harness — the replace is a no-op,
    // the suite runs green, and the line prints as a mutation that found nothing to say. It is
    // the strawman wearing a green shirt: the shape this harness already documents twice, now
    // as a MUTATION THAT MUTATES NOTHING rather than as a test that asserts the wrong thing.
    // Every `find` below is therefore checked for occurrence by the harness itself.
    global: true,
    find: "if (current.state === previous && started) {",
    replace: "if (previous !== null) {",
  },
  {
    name: "M12 the note drops the field saying whether the run ever settled",
    // This one is in the NOTE, which is neither the run window nor the row window — the harness
    // refused it on the first run, correctly. Two named windows now exist and neither covers
    // the note, which is the third window in this directory for the third block of this row.
    // Rather than widen either (a wider window is a window that can land on the wrong
    // occurrence of a common line), the note gets its own, and the refusal above is left in
    // place: it is a load-bearing check, not an obstacle to route around.
    in: "note",
    find: "      runSettled,\n",
    replace: "",
  },
  {
    name: "M13 a run that never settles is reported as settled",
    // The fallthrough return moved when tick 88 added the witness: the exhausted loop now
    // reads once more into a named local, because the answer has to carry `started` and
    // `finished` as well as `settled`. Same anchor-staleness as M11, and the same lesson.
    global: true,
    find: "settled: false, started: hasStarted(last.run) || started, finished: false",
    replace: "settled: true, started: hasStarted(last.run) || started, finished: true",
  },
  {
    name: "M14 the WITNESS is dropped from the stop condition (tick 88's defect, in this harness)",
    // The mutation that matters most here, because it is the defect tick 88 found: a run the
    // engine never claimed is stable, so stability alone calls it finished.
    global: true,
    find: "if (current.state === previous && started) {",
    replace: "if (current.state === previous) {",
  },
];

const runSuite = () => {
  try {
    execFileSync(process.execPath, ["--test", "--experimental-strip-types", TEST], {
      cwd: dirname(TEST),
      stdio: "pipe",
    });
    return { green: true, output: "" };
  } catch (error) {
    return { green: false, output: `${error.stdout ?? ""}${error.stderr ?? ""}` };
  }
};

const original = readFileSync(WALKTHROUGH, "utf8");
copyFileSync(WALKTHROUGH, BACKUP);

/** Replace inside the row window only, and prove the window is the expected one. */
const mutate = (find, replace) => {
  const { start, end } = rowWindow(original);
  const window = original.slice(start, end);
  assert.ok(
    window.includes(find),
    `the mutation target is not inside the row window, so this harness would measure another block: ${find.slice(0, 60)}`,
  );
  return original.slice(0, start) + window.replace(find, replace) + original.slice(end);
};

/**
 * The RUN window: from the click to the reduction, which is where the tick-61 settle-wait lives.
 *
 * The row window opens at `const painted = …`, one block AFTER the run is read, so a mutation
 * about the run cannot be expressed in it at all. Widening `rowWindow` instead would weaken
 * the nine mutations above — a window that spans twice as much is a window that can land on the
 * wrong occurrence of a common line. A second, named window keeps both guarantees.
 */
const runWindow = (source) => {
  const start = source.indexOf('page.locator("[data-run-from-here-button]")');
  const end = source.indexOf("const runnable = ", start);
  assert.notEqual(start, -1, "the run-from-here click must exist");
  assert.notEqual(end, -1, "the run must be reduced");
  return { start, end };
};

const mutateIn = (which, find, replace) => {
  const { start, end } = rowWindows[which](original);
  const window = original.slice(start, end);
  assert.ok(
    window.includes(find),
    `the mutation target is not inside the ${which} window, so this harness would measure another block: ${find.slice(0, 60)}`,
  );
  return original.slice(0, start) + window.replace(find, replace) + original.slice(end);
};

/** The NOTE: the block the row reports itself in, which neither of the other two reaches. */
const noteWindow = (source) => {
  const start = source.indexOf('step: "run-from-here",');
  const end = source.indexOf('await shot(page, "page-workflow-builder-run-from-here")', start);
  assert.notEqual(start, -1, "the run-from-here note must exist");
  assert.notEqual(end, -1, "the row must take a screenshot");
  return { start, end };
};

const rowWindows = { row: rowWindow, run: runWindow, note: noteWindow };

let failures = 0;
try {
  const baseline = runSuite();
  assert.ok(baseline.green, "the suite must be green before any mutation is applied");
  console.log("baseline: green");

  for (const mutation of MUTATIONS) {
    // `global: true` targets the `settleRun` helper, which lives above every row window — the
    // helper is shared, so scoping it to a row would be wrong rather than merely narrow.
    const mutated = mutation.global
      ? (() => {
          const helperStart = original.indexOf("async function settleRun(");
          const helperEnd = original.indexOf("async function interact(");
          assert.notEqual(helperStart, -1, "the settle helper must exist");
          assert.ok(helperEnd > helperStart, "the settle helper window must not end before it begins");
          const helper = original.slice(helperStart, helperEnd);
          assert.ok(
            helper.includes(mutation.find),
            `the mutation target is not inside the settleRun helper: ${mutation.find.slice(0, 60)}`,
          );
          return (
            original.slice(0, helperStart) +
            helper.replace(mutation.find, mutation.replace) +
            original.slice(helperEnd)
          );
        })()
      : mutateIn(mutation.in ?? "row", mutation.find, mutation.replace);
    writeFileSync(WALKTHROUGH, mutated);
    const result = runSuite();
    writeFileSync(WALKTHROUGH, original);
    if (result.green) {
      failures += 1;
      console.log(`  STILL GREEN  ${mutation.name}`);
    } else {
      const failed = result.output.match(/^not ok \d+ - (.+)$/m)?.[1] ?? "a test";
      console.log(`  red (${failed})  ${mutation.name}`);
    }
  }
} finally {
  // Restored unconditionally, including on a throw.
  try {
    renameSync(BACKUP, WALKTHROUGH);
  } catch {
    copyFileSync(BACKUP, WALKTHROUGH);
  }
  if (readFileSync(WALKTHROUGH, "utf8") !== original) {
    console.error("RESTORE FAILED — the walkthrough does not match its committed content");
    process.exit(1);
  }
  console.log("\nwalkthrough restored and byte-identical");
}

console.log(`\n${MUTATIONS.length - failures}/${MUTATIONS.length} mutations red`);
process.exit(failures === 0 ? 0 : 1);
