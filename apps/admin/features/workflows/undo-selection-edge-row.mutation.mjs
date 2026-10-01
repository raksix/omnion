#!/usr/bin/env node
/**
 * Mutation harness for the `undo-selection-edge` row's instrument test.
 *
 * ## What this is for
 *
 * A test that is green on a correct row and red on a correct row is the failure mode this
 * directory keeps re-learning: five checks in this REQ have now gone red for a reason that
 * had nothing to do with what they claimed to measure, and the cheap repair — loosening the
 * assertion until it agrees — is how that becomes permanent. So every assertion here is
 * proven by taking the thing it measures AWAY and confirming it goes red.
 *
 * ## Why every edit is SCOPED to the row window
 *
 * The first draft of this harness replaced text across the whole walkthrough and three of the
 * twelve mutations came back STILL GREEN. The reason is the one this directory already
 * documents twice: **`String.replace` hits the FIRST occurrence, and the `edge-delete` row
 * further down the file contains the same four lines** — the same `getPointAtLength`, the
 * same `getScreenCTM`, the same `querySelectorAll`. So M2, M3 and M4 were each mutating the
 * *other* row and then reporting the honest result for a test that had not been touched.
 *
 * A mutation harness that measures the wrong row is worse than no harness: it prints a
 * number, the number is meaningless, and "12/12 red" is the most reassuring sentence a
 * person can be handed. So each edit is applied to the row's own window — the same slice the
 * instrument test uses — and the harness asserts that the window is the one it expected
 * before it mutates anything. The restore is byte-for-byte, and it happens on every path
 * including a throw, because a harness that leaves the repo edited is not run twice.
 */
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { copyFileSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const WALKTHROUGH = join(HERE, "..", "..", "..", "..", "scripts", "qa", "walkthrough.cjs");
const BACKUP = `${WALKTHROUGH}.mutation-backup`;
const TEST = join(HERE, "undo-selection-edge-row.test.ts");

/** The row's own window — the same slice the instrument test reads. */
const ROW_START = "const undoSelEdgePoint = await page";
const ROW_END = 'note({ step: "undo-selection-edge"';

const rowWindow = (source) => {
  const start = source.indexOf(ROW_START);
  const end = source.indexOf(ROW_END, start);
  assert.notEqual(start, -1, "the undo-selection-edge row must exist");
  assert.notEqual(end, -1, "the row must report itself");
  return { start, end };
};

/** Every mutation: a name, a find, and a replace — all inside the row window. */
const MUTATIONS = [
  {
    name: "M1 the keypress is gone — the row reads the state it started in",
    find: 'await page.keyboard.press("Control+z");\n    await page.waitForTimeout(1200);\n    const edgesAfterUndo',
    replace: "const edgesAfterUndo",
  },
  {
    name: "M2 the click aims at the bounding box — the edge-delete probe defect",
    find: "const mid = hit.getPointAtLength(hit.getTotalLength() / 2);\n      const screen = mid.matrixTransform(ctm);",
    replace: "const box = hit.getBoundingClientRect();\n      const screen = { x: box.x + box.width / 2, y: box.y + box.height / 2 };",
  },
  {
    name: "M3 the screen matrix is dropped — the point is in node space, not screen space",
    find: "const screen = mid.matrixTransform(ctm);",
    replace: "const screen = mid;",
  },
  {
    name: "M4 the PRE-undo read uses the NODE marker, which is zero for an edge selection anyway",
    find: `selected: document.querySelectorAll("[data-edge-selected='true']").length,
        readout:`,
    replace: `selected: document.querySelectorAll("[data-node-id][data-node-selected='true']").length,
        readout:`,
  },
  {
    name: "M5 the POST-undo marker is shortened — a bare selector matches every edge",
    find: `selected: document.querySelectorAll("[data-edge-selected='true']").length,
          readout:`,
    replace: `selected: document.querySelectorAll("[data-edge-selected]").length,
          readout:`,
  },
  {
    name: "M6 the status bar is not read on the POST side — only a CSS class",
    find: `selected: document.querySelectorAll("[data-edge-selected='true']").length,
          readout: el ? (el.textContent ?? "").trim() : null,
          drawn:`,
    replace: `selected: document.querySelectorAll("[data-edge-selected='true']").length,
          readout: null,
          drawn:`,
  },
  {
    name: "M7 the conjunction becomes a disjunction — either half alone is accepted",
    find: 'edgePost.selected === 0 && !(edgePost.readout ?? "").includes("connection selected"),',
    replace: "edgePost.selected === 0,",
  },
  {
    name: "M8 the words are no longer named, so any sentence with 'selected' satisfies it",
    find: '.includes("connection selected"),',
    replace: '.includes("selected"),',
  },
  {
    name: "M9 the precondition is assumed rather than proven — the undo's effect is never checked",
    find: "edgeRemovedByUndo: edgesAfterUndo < edgesBeforeUndo,",
    replace: "edgeRemovedByUndo: true,",
  },
  {
    name: "M10 the preconditions are USED but not REPORTED, which is how they stop being read",
    find: `      edgeWasSelected,
      toolbarClaimedIt,`,
    replace: "",
  },
  {
    name: "M11 a missed click reports a conclusion with no evidence behind it",
    find: `      onEdge: undoSelEdgePoint?.onEdge ?? null,
      inViewport: undoSelEdgePoint?.inViewport ?? null,
      edgesOnCanvas: (await page.locator("[data-edge]").count()) ?? 0,`,
    replace: "      missed: true,",
  },
  {
    name: "M12 the two blockers collapse — a fixture problem reads as a gesture problem",
    find: 'reason: edgesBeforeUndo === 0 ? "the rule has no connection to select" : "the click missed the curve",',
    replace: 'reason: "the click missed the curve",',
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
    `the mutation target is not inside the row window, so this harness would measure the other row: ${find.slice(0, 60)}`,
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
  // Restored unconditionally, including on a throw: a harness that leaves the repo edited
  // is not run a second time.
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
