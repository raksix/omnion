/**
 * Proven-to-fail mutations for `builder-help-dialog.test.ts`.
 *
 * ## Why this file exists
 *
 * The sibling guard suites ship with a mutation list, and the REQ's own rule is blunt about it:
 * a gate that is only *read* is a note. This suite's first draft asserted four things, all of
 * which passed on the very first run — which proves nothing, because a suite that is green
 * against the code as written and has never been seen red is a suite whose assertions might be
 * about anything.
 *
 * Each mutation below undoes ONE decision the suite claims to police, runs the suite, and
 * requires a **named** assertion to go red. A mutation that leaves the suite green is reported as
 * a failure, not skipped: an assertion that cannot be defeated by undoing the thing it describes
 * is not an assertion about that thing.
 *
 * ## Running it
 *
 * ```bash
 * node scripts/qa/mutate-builder-help-dialog.sh
 * ```
 *
 * It restores `builder-view.tsx` byte-exactly (asserted by md5 on every run, including the
 * passing ones) — the file is shared with nine other guards, and a suite that leaves a mutated
 * product behind is worse than one that never ran.
 */
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync, renameSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, "../../../..");
const TARGET = path.join(ROOT, "apps/admin/features/workflows/builder-view.tsx");
const SUITE = path.join(ROOT, "apps/admin/features/workflows/builder-help-dialog.test.ts");

const original = readFileSync(TARGET, "utf8");
const originalMd5 = createHash("md5").update(original).digest("hex");

type Mutation = {
  /** What the mutation undoes, in one line. */
  name: string;
  /** The `find`/`replace` pair. A `find` that matches nothing is a FAILURE, never a skip. */
  find: string | RegExp;
  replace: string;
  /** The assertion message that must go red — a mutation must name what it defeated. */
  expectRed: string;
};

const mutations: Mutation[] = [
  {
    name: "M1 the dialog's own Escape handler is removed",
    find: `            onKeyDown={(event) => {
              if (event.key !== "Escape") {
                return;
              }
              event.stopPropagation();
              event.preventDefault();
              setHelpOpen(false);
            }}
`,
    replace: "",
    expectRed: "the dialog must carry its own onKeyDown",
  },
  {
    name: "M2 Escape is handled but NOT consumed (stopPropagation removed)",
    find: "              event.stopPropagation();\n              event.preventDefault();\n              setHelpOpen(false);",
    replace: "              event.preventDefault();\n              setHelpOpen(false);",
    expectRed: "the dialog's KEYDOWN must consume Escape",
  },
  {
    name: "M3 the dialog checks a key it does not handle",
    find: `              if (event.key !== "Escape") {`,
    replace: `              if (event.key !== "Enter") {`,
    expectRed: "the dialog must claim Escape itself",
  },
  {
    name: "M4 the dialog takes no focus (autoFocus removed)",
    find: "                autoFocus\n",
    replace: "",
    expectRed: "a control inside the dialog must take focus when it opens",
  },
  {
    name: "M5 autoFocus moves onto the dialog's own non-focusable tag",
    find: "            data-builder-help\n          >",
    replace: "            data-builder-help\n            autoFocus\n          >",
    expectRed: "the focused control must be a `<button>`",
  },
  {
    name: "M6 the focused control is no longer the one that closes the dialog",
    find: `                onClick={() => setHelpOpen(false)}
                autoFocus`,
    replace: `                onClick={() => void 0}
                autoFocus`,
    expectRed: "the focused control must be the same one that closes the dialog on click",
  },
  {
    name: "M7 the palette grows its own onKeyDown, which would silently re-bubble Escape",
    find: `          data-builder-palette
`,
    replace: `          data-builder-palette
          onKeyDown={() => void 0}
`,
    expectRed: "palette must not carry onKeyDown",
  },
];

const runSuite = (): { output: string; status: number } => {
  try {
    const output = execFileSync(
      process.execPath,
      ["--test", "--experimental-strip-types", SUITE],
      { encoding: "utf8", cwd: path.join(ROOT, "apps/admin"), stdio: ["ignore", "pipe", "pipe"] },
    );
    return { output, status: 0 };
  } catch (err) {
    const e = err as { stdout?: string; stderr?: string; status?: number };
    return { output: `${e.stdout ?? ""}${e.stderr ?? ""}`, status: e.status ?? 1 };
  }
};

// The suite must be green BEFORE any mutation, or "the mutation turned it red" is meaningless —
// a suite that is already red proves nothing about what any mutation did.
const baseline = runSuite();
if (baseline.status !== 0) {
  console.error("FATAL: the suite is not green before mutation; fix that first");
  console.error(baseline.output.slice(-2000));
  process.exit(1);
}
console.log(`baseline: green (the suite must be green for a mutation to mean anything)`);

let passed = 0;
const failures: string[] = [];

try {
  for (const mutation of mutations) {
    const mutated =
      typeof mutation.find === "string"
        ? original.replace(mutation.find, mutation.replace)
        : original.replace(mutation.find, mutation.replace);

    // A `find` that matches nothing produces `mutated === original`, and the suite stays green —
    // which is indistinguishable from "this assertion cannot be defeated". Refuse it instead.
    if (mutated === original) {
      failures.push(`${mutation.name}: the \`find\` matched nothing (refusing to report a pass)`);
      continue;
    }

    writeFileSync(TARGET, mutated);
    const { output, status } = runSuite();

    if (status === 0) {
      failures.push(`${mutation.name}: the suite stayed GREEN — the guard does not bite`);
      continue;
    }
    if (!output.includes(mutation.expectRed)) {
      failures.push(
        `${mutation.name}: went red, but not on "${mutation.expectRed}" — a different assertion caught it`,
      );
      continue;
    }
    passed += 1;
    console.log(`  red  ${mutation.name}  ->  "${mutation.expectRed}"`);
  }
} finally {
  // Restore byte-exactly, and PROVE it. The suite is shared with nine other guards; leaving a
  // mutated product behind turns one red-able guard into a source of phantom failures.
  writeFileSync(TARGET, original);
  const restored = readFileSync(TARGET, "utf8");
  const restoredMd5 = createHash("md5").update(restored).digest("hex");
  if (restoredMd5 !== originalMd5) {
    console.error(`FATAL: ${TARGET} was NOT restored byte-exactly (md5 ${restoredMd5} != ${originalMd5})`);
    process.exit(1);
  }
  console.log(`restored builder-view.tsx byte-exactly (md5 ${restoredMd5})`);
}

console.log(`\n${passed}/${mutations.length} mutations red with a named assertion`);
if (failures.length) {
  for (const failure of failures) console.error(`  FAIL ${failure}`);
  process.exit(1);
}
