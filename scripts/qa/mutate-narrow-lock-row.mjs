#!/usr/bin/env node
/**
 * Mutations for `narrow-lock-row.test.ts`.
 *
 * **The rule this file exists to police: a gate that has never been red proves nothing.** Tick
 * 84's MUTANT 1 matched nothing, returned 15/15 green on a fully reverted defect, and sat in a
 * table certifying a mutation that never happened. So this runner asserts three things before it
 * reports a result:
 *
 *   1. the mutation actually changed the bytes it claims to change (`before !== src`);
 *   2. the suite failed afterwards, and names an assertion (not a crash);
 *   3. the file was restored byte-exact, **including on green runs**.
 *
 * The control is `git show HEAD:…` against the walkthrough — the real pre-fix source, not a
 * hand-written approximation of it (tick 83: five hand-written mutations all passed while the
 * shipped defect sat in the tree).
 *
 * Usage: `node scripts/qa/mutate-narrow-lock-row.mjs`
 * Exits 0 when every mutation is red AND the control is green.
 */

import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = join(HERE, "..", "..");
const WALKTHROUGH = join(REPO, "scripts", "qa", "walkthrough.cjs");
const SUITE = join(REPO, "apps", "admin", "features", "workflows", "narrow-lock-row.test.ts");

const md5 = (path) => createHash("md5").update(readFileSync(path)).digest("hex");

function runSuite() {
  return spawnSync(
    process.execPath,
    ["--test", "--experimental-strip-types", SUITE],
    { encoding: "utf8", cwd: join(REPO, "apps", "admin") },
  );
}

function result() {
  const out = runSuite().stdout ?? "";
  const pass = Number(out.match(/^# pass (\d+)$/m)?.[1] ?? -1);
  const fail = Number(out.match(/^# fail (\d+)$/m)?.[1] ?? -1);
  return { pass, fail, out };
}

/**
 * Each mutation: a name, the expectation it breaks, and the replacement. `expect` names the
 * substring of the failing assertion's message, so a red run has to be red for the REASON the
 * mutation was written for rather than for some unrelated breakage.
 */
const MUTATIONS = [
  {
    id: "M1",
    why: "restore the two markers the product has never rendered — the shipped defect verbatim",
    expect: "data-workflow-table-edit",
    find: 'document.querySelectorAll("[data-table-label], [data-table-param]")',
    replace: 'document.querySelectorAll("[data-workflow-table-edit], [data-table-edit]")',
  },
  {
    id: "M2",
    why: "collapse `editable` back to `rendered` — the one-sided count that made a constant look like a reading",
    expect: "actually writable",
    find: "editable: inputs.filter(",
    replace: "editable: inputs.length, // inputs.filter(",
  },
  {
    id: "M3",
    why: "drop `tableMounted` — a zero that no longer says whether the page rendered",
    expect: "tableMounted",
    find: "tableMounted,",
    replace: "tableMountedDropped: null,",
  },
  {
    id: "M4",
    why: "hardcode the mounted flag — the wait stays in the source but stops deriving anything",
    expect: "wait for the table to mount",
    find: 'const tableMounted =\n        (await page\n          .waitForSelector("[data-table-mode]", { timeout: 15000 })\n          .then(() => true)\n          .catch(() => false)) || false;',
    replace: "const tableMounted = true;\n        void page.waitForSelector;",
  },
  {
    id: "M5",
    // **Stripping only the FIRST of two `dispatchEvent(new Event("input"…))` sites** — the
    // restore's own dispatch kept the substring rule green on a row that no longer types.
    why: "stop typing — count inputs again, which proves nothing about writability",
    expect: "no extra exit",
    find:
      'const typed = await page.evaluate(() => {\n          const input = document.querySelector("[data-table-label], [data-table-param]");',
    replace:
      'const typed = await page.evaluate(() => {\n          const input = document.querySelector("[data-table-label], [data-table-param]"); // typing disabled\n          if (input) return null;',
  },
  {
    id: "M6",
    why: "rename `editControls` — a silent rename hides which earlier reports cited the constant",
    expect: "keep `editControls`",
    find: "editControls: editSurface?.editable ?? null,",
    replace: "editableControls: editSurface?.editable ?? null,",
  },
  {
    id: "M7",
    why: "restore from the DOM instead of the saved value — a no-op that leaves the draft dirty",
    expect: "typed.original",
    find: "}, typed.original);",
    replace: "});",
  },
];

const original = readFileSync(WALKTHROUGH, "utf8");
const originalHash = md5(WALKTHROUGH);
let failures = 0;

function restore() {
  writeFileSync(WALKTHROUGH, original);
}
restore();

// --- control: the real pre-fix bytes must turn the suite red -----------------------------
process.stdout.write("control · real pre-fix source (git show HEAD:scripts/qa/walkthrough.cjs)\n");
try {
  const head = execFileSync("git", ["show", "HEAD:scripts/qa/walkthrough.cjs"], {
    cwd: REPO,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
  });
  // Only the row matters, but a whole-file revert is the honest control: it proves the suite is
  // red against the tree as it was, not merely against a string I typed.
  writeFileSync(WALKTHROUGH, head);
  if (head === original) {
    // The fix is uncommitted — that is expected mid-tick. Skip rather than report a false red.
    process.stdout.write("  SKIP · HEAD already contains the fix (nothing committed yet)\n\n");
  } else {
    const control = result();
    if (control.fail > 0) {
      const named = MUTATIONS.filter((m) => control.out.includes(m.expect)).length;
      process.stdout.write(
        `  RED · ${control.fail} failed / ${control.pass} passed` +
          (named > 0 ? `, naming ${named} expectation(s)\n\n` : " (unnamed — see below)\n\n"),
      );
    } else {
      process.stdout.write(`  GREEN · ${control.pass} passed — THE GATE IS NOT PROVEN\n\n`);
      failures += 1;
    }
  }
} catch (error) {
  process.stdout.write(`  SKIP · no HEAD version available (${String(error).slice(0, 60)})\n\n`);
} finally {
  restore();
}

// --- control: the unmutated file must be green -------------------------------------------
process.stdout.write("control · unmutated\n");
const green = result();
if (green.fail === 0) {
  process.stdout.write(`  GREEN · ${green.pass} passed\n\n`);
} else {
  process.stdout.write(`  RED · ${green.fail} failed — the suite is red on arrival\n\n`);
  failures += 1;
}

// --- mutations ----------------------------------------------------------------------------
for (const mutation of MUTATIONS) {
  const before = readFileSync(WALKTHROUGH, "utf8");
  const src = before.replace(mutation.find, mutation.replace);

  if (src === before) {
    process.stdout.write(`${mutation.id} · NOT APPLIED · ${mutation.why}\n`);
    process.stdout.write(`      (the anchor "${mutation.find.slice(0, 60)}" is not in the file)\n`);
    failures += 1;
    restore();
    continue;
  }
  writeFileSync(WALKTHROUGH, src);
  const outcome = result();
  restore();

  const red = outcome.fail > 0;
  const named = outcome.out.includes(mutation.expect);
  const status = red && named ? "RED   " : red ? "RED?  " : "GREEN ";
  process.stdout.write(
    `${mutation.id} · ${status} ${outcome.fail} failed / ${outcome.pass} passed · ${mutation.why}\n`,
  );
  if (!red) {
    process.stdout.write(`      EXPECTED TO NAME: ${mutation.expect}\n`);
    failures += 1;
  } else if (!named) {
    process.stdout.write(`      red, but not on the named expectation: ${mutation.expect}\n`);
    failures += 1;
  }
}

// The restore is asserted on every run, green included: a mutation script that leaves the tree
// mutated is worse than one with no mutations at all, because the next gate reads the wreckage.
const finalHash = md5(WALKTHROUGH);
if (finalHash !== originalHash) {
  process.stdout.write(`\nRESTORE FAILED · the walkthrough was left modified\n`);
  failures += 1;
} else {
  process.stdout.write(`\nrestore · byte-exact\n`);
}

process.stdout.write(`\n${failures === 0 ? "ALL MUTATIONS RED" : `${failures} PROBLEM(S)`}\n`);
process.exit(failures === 0 ? 0 : 1);