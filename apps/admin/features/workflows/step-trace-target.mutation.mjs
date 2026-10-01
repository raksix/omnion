#!/usr/bin/env node
/**
 * Mutation harness for `step-trace-row.test.ts`.
 *
 * ## What this is for
 *
 * A green row test proves only that the assertions run. What makes them worth anything is
 * whether the DEFECT they were written for turns them red — and the defect in this row is
 * *vacuity*, not a wrong number. A vacuous gate and a passing gate print the same digits,
 * so no assertion about the printed value can distinguish them. The only way to know the
 * test has teeth is to put the defect BACK and confirm red.
 *
 * ## Why every edit is SCOPED to the row's own window
 *
 * A global `String.replace` hits the FIRST occurrence, and three of this REQ's earlier
 * harnesses silently edited a *different* row while reporting an honest result for a test
 * they never touched. So each edit below is applied inside the `step-trace` block, the
 * window is asserted to be the expected one before anything is written, and the restore is
 * byte-for-byte on every path including a throw — a harness that leaves the repo edited is
 * not run twice.
 *
 * ## Scope, and why the read-guards live in the other harness
 *
 * These mutations target `step-trace-target.test.ts` only — the TARGET, the wait, and the
 * measurability switch. The per-step payload read, the wire read and the two-directional
 * step-number comparison are guarded by `step-trace-row.test.ts` (commit `81a93235`) and
 * proven by the pre-existing suite; M8–M10 in the first draft of this file re-proved them
 * against a test that is not in scope here, and a harness that claims to measure a file it
 * does not run is a harness lying about its coverage.
 *
 * ## Why M1 is the one that matters
 *
 * M1 restores the exact line tick 60 replaced. The others guard against the fix being undone
 * in subtler ways, but M1 is the defect: with the target derived from the pill, a pill
 * regression leaves the panel shut and all three conjunction gates green. If only one mutation
 * in this file goes red, it must be that one.
 */
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { copyFileSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const WALKTHROUGH = join(HERE, "..", "..", "..", "..", "scripts", "qa", "walkthrough.cjs");
const BACKUP = `${WALKTHROUGH}.mutation-backup`;
const TEST = join(HERE, "step-trace-target.test.ts");

/** The row's own window — the target's derivation through the note and the shot. */
const ROW_START = "const runCandidateIds = (after?.steps ?? [])";
const ROW_END = 'await shot(page, "page-workflow-builder-step-trace")';

const rowWindow = (source) => {
  const start = source.indexOf(ROW_START);
  const end = source.indexOf(ROW_END, start);
  assert.notEqual(start, -1, "the step-trace row must derive its target");
  assert.notEqual(end, -1, "the row must take a screenshot");
  return { start, end };
};

/** Every mutation: a name, a find and a replace — all inside the row window. */
const MUTATIONS = [
  {
    name: "M1 the target goes back to the pill — the defect: the gate goes VOID, not red",
    find: [
      "const paintedNodeId = runCandidateIds.find((id) => canvasIds.includes(id)) ?? null;",
    ].join("\n"),
    replace: [
      "const paintedNodeId =",
      "  painted.painted.find((entry) => entry.status !== \"skipped\")?.nodeId ?? null;",
    ].join("\n"),
  },
  {
    name: "M2 the run's candidate list is built from the canvas, so the target is circular again",
    find: "const runCandidateIds = (after?.steps ?? [])",
    replace: "const runCandidateIds = (painted.painted ?? [])",
  },
  {
    name: "M3 the intersection with the cards the canvas drew is dropped",
    find: "const paintedNodeId = runCandidateIds.find((id) => canvasIds.includes(id)) ?? null;",
    replace: "const paintedNodeId = runCandidateIds[0] ?? null;",
  },
  {
    name: "M4 the wait for the panel goes back to a fixed delay",
    find: [
      "        await page",
      "          .waitForSelector(`[data-step-trace=\"${paintedNodeId}\"]`, { timeout: 8000 })",
      "          .catch(() => {});",
    ].join("\n"),
    replace: "        await page.waitForTimeout(500);",
  },
  {
    name: "M5 the measurability switch is deleted — the note cannot tell a shut panel from a clean one",
    find: "        rowIsMeasurable: trace !== null && (trace?.steps.length ?? 0) > 0,\n",
    replace: "",
  },
  {
    name: "M6 the measurability switch accepts a panel that opened with no steps in it",
    find: "rowIsMeasurable: trace !== null && (trace?.steps.length ?? 0) > 0,",
    replace: "rowIsMeasurable: trace !== null,",
  },
  {
    name: "M7 the pill's own choice is dropped, so a disagreement is diagnosed against the wrong row",
    find: "        pillChoseSameAsRun: pillChosenNodeId === paintedNodeId,\n",
    replace: "",
  },
  {
    name: "M8 the note stops saying whether the clicked node carried a pill at all",
    find: "        paintedOnTarget: painted.painted.some((entry) => entry.nodeId === paintedNodeId),\n",
    replace: "",
  },
  {
    name: "M9 `targetFromRun` is deleted, so a graph/canvas divergence reads as a panel that failed to open",
    find: "        targetFromRun: paintedNodeId !== null,\n",
    replace: "",
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

let failures = 0;
try {
  const baseline = runSuite();
  assert.ok(baseline.green, "the suite must be green before any mutation is applied");
  console.log("baseline: green");

  for (const mutation of MUTATIONS) {
    writeFileSync(WALKTHROUGH, mutate(mutation.find, mutation.replace));
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
