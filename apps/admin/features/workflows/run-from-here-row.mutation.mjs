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
