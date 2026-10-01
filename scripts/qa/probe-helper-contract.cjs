#!/usr/bin/env node
/**
 * The harness's own contract gate (REQ-010, tick 100).
 *
 * ## The defect class
 *
 * `runMediaFileDetail` and `runMediaShares` both guarded their upload with
 * `if (!uploaded || !uploaded.ok)`. `uploadMediaSample` has never written an `ok` key — it writes
 * `uploaded`, `file` and `listed`. So `uploaded.ok` was `undefined`, the guard was always true, and
 * both passes returned before touching the screen, on *every* run. Twenty ticks of REQ-010 rest
 * on this pass, and the artifact proves the upload itself was fine (`uploaded: true, listed: 2`,
 * `listed: 10`): the file was in the library and the pass refused to look at it. The build log
 * read the refusal as a crowded box, which is why a two-character bug survived a week.
 *
 * A guard reading a key the helper never writes is **silently true**. There is no error, no
 * warning, no stack — the pass simply returns early and the summary reports a `reason` that names
 * something else entirely.
 *
 * ## What this gate checks
 *
 * 1. Every call site of a helper defined in this file branches only on keys the helper's own body
 *    can produce. That is the check that would have caught this on the day it was written.
 * 2. The specific contract is asserted directly, so a future rename of `ok` breaks here rather
 *    than at 02:00 in a 25-minute browser run.
 *
 * The gate is a static read of the source, so it costs milliseconds and needs no browser, no
 * database and no slot — which is the whole point: the previous twenty ticks spent their gate on
 * a pass that could not run.
 */

const fs = require("fs");
const path = require("path");

const FILE = path.join(__dirname, "walkthrough.cjs");
const source = fs.readFileSync(FILE, "utf8");
const lines = source.split("\n");

const results = [];
const check = (name, pass, detail) => results.push({ name, pass, detail });

// ---------------------------------------------------------------- helpers and their answers

/**
 * The keys a helper can put in its return value, read from its own body.
 *
 * `return { uploaded: true, ... }` and `return { ok: true, ... }` are the shapes that count; an
 * early `return { uploaded: false, note }` counts too. A key that only ever appears on one branch
 * still counts — a caller is allowed to branch on a key that is sometimes absent, but it must not
 * branch on a key that is *never* there.
 */
function returnedKeys(fnName) {
  const start = lines.findIndex((line) => line.includes(`async function ${fnName}(`));
  if (start === -1) return null;
  // A helper's body ends at the next top-level `async function` / `function` at column 0.
  let end = lines.length;
  for (let i = start + 1; i < lines.length; i += 1) {
    if (/^(async )?function [A-Za-z]/.test(lines[i])) {
      end = i;
      break;
    }
  }
  const body = lines.slice(start, end).join("\n");
  const keys = new Set();
  // `return { a, b, c }` and `return { a: 1, ...spread }`.
  for (const match of body.matchAll(/return\s*\{([^}]*)\}/g)) {
    for (const part of match[1].split(",")) {
      const key = part.split(":")[0].trim().replace(/^\.\.\./, "").trim();
      if (/^[A-Za-z_][A-Za-z0-9_]*$/.test(key)) keys.add(key);
    }
  }
  return keys;
}

/** Media helpers this gate holds to a written contract. */
const MEDIA_HELPERS = ["uploadMediaSample", "uploadDuplicateSample"];
const EXPECTED = {
  uploadMediaSample: ["ok", "uploaded", "listed"],
  uploadDuplicateSample: ["ok", "uploaded"],
};

for (const fn of MEDIA_HELPERS) {
  const keys = returnedKeys(fn);
  check(`${fn} is defined`, keys !== null, keys === null ? "not found" : `${keys.size} keys`);
  if (keys === null) continue;
  for (const key of EXPECTED[fn]) {
    check(
      `${fn} can return \`${key}\``,
      keys.has(key),
      keys.has(key) ? "present" : `missing — callers branching on it always fail`,
    );
  }
}

// ------------------------------------------------------- call sites branch on real keys

for (const fn of MEDIA_HELPERS) {
  const keys = returnedKeys(fn);
  if (keys === null) continue;
  const callSites = [];
  lines.forEach((line, index) => {
    if (line.includes(`await ${fn}(`) && !line.includes(`function ${fn}`)) callSites.push(index);
  });
  check(`${fn} has call sites to check`, callSites.length > 0, `${callSites.length} found`);

  for (const index of callSites) {
    // The guard is the next few lines after the assignment.
    const window = lines.slice(index, index + 8).join("\n");
    const reads = [...window.matchAll(/(\w+)\.(\w+)/g)]
      .filter(([, object]) => !["page", "console", "Math", "JSON", "document", "window", "path", "fs", "d"].includes(object))
      .map(([, object, prop]) => `${object}.${prop}`);

    // Only `uploaded.<key>` reads matter, and only ones that are *branched* on.
    const branched = [...window.matchAll(/uploaded\.(\w+)/g)].map(([, prop]) => prop);
    for (const prop of new Set(branched)) {
      // `uploaded.ok` is the whole defect: read the key the helper writes.
      const ok = keys.has(prop) || keys.has(prop.replace(/^ok$/, "uploaded")) || prop === "uploaded";
      check(
        `line ${index + 1}: uploaded.${prop} is a real answer`,
        keys.has(prop),
        keys.has(prop)
          ? "present"
          : `uploadMediaSample never writes \`${prop}\` — this guard is silently always true`,
      );
      void ok;
    }
  }
}

// ---------------------------------------------------------------- the specific regression

/**
 * The exact bug, asserted by value.
 *
 * There is deliberately **no** "the guards must stop reading `uploaded.ok`" check. The defect was
 * a mismatch between what the helper writes and what the caller reads, and the fix could go either
 * way: drop the read, or write the key. This went the second way — `uploadMediaSample` now
 * returns `ok` — so a guard reading `uploaded.ok` is the *correct* code and a check forbidding it
 * would fail on the fix. (It did: the first version of this gate reported exactly that, and was
 * wrong.)
 *
 * What is worth pinning is the invariant, and check 1 above already is it: every key a guard
 * branches on is a key the helper can return. So this section asserts the two halves of that
 * invariant on the exact lines that were broken, in both directions — the helper writes the key,
 * and the guards read it. Rename one side and this fails.
 */
const detailGuard = source.slice(
  source.indexOf("async function runMediaFileDetail"),
  source.indexOf("async function runMediaShares"),
);
const shareGuard = source.slice(
  source.indexOf("async function runMediaShares"),
  source.indexOf("async function uploadDuplicateSample"),
);

const helperKeys = returnedKeys("uploadMediaSample") || new Set();

check(
  "uploadMediaSample writes the `ok` key the guards read",
  helperKeys.has("ok"),
  helperKeys.has("ok") ? "present" : "missing — both upload guards are silently always-true again",
);

for (const [name, chunk] of [
  ["runMediaFileDetail", detailGuard],
  ["runMediaShares", shareGuard],
]) {
  const reads = [...chunk.matchAll(/!\s*uploaded\.(\w+)/g)].map(([, prop]) => prop);
  check(
    `${name} guards on a key the helper writes`,
    reads.length > 0 && reads.every((prop) => helperKeys.has(prop)),
    reads.length === 0
      ? "no upload guard found — did the pass stop uploading at all?"
      : `reads ${[...new Set(reads)].join(", ")}; helper writes ${[...helperKeys].sort().join(", ")}`,
  );
}

// ---------------------------------------------------------------- report

const failed = results.filter((r) => !r.pass);
for (const r of results) {
  const mark = r.pass ? "PASS" : "FAIL";
  const detail = typeof r.detail === "string" ? r.detail : JSON.stringify(r.detail);
  console.log(`${mark}  ${r.name}${detail ? ` — ${detail}` : ""}`);
}
console.log(`\n${results.length - failed.length}/${results.length} helper-contract checks passed`);

process.exit(failed.length === 0 ? 0 : 1);
