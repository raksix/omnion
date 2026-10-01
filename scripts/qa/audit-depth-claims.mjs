#!/usr/bin/env node
// Audit the walkthrough's depth passes: which claims are GATED and which are merely COLLECTED.
//
//   node scripts/qa/audit-depth-claims.mjs            human-readable report, exit 0
//   node scripts/qa/audit-depth-claims.mjs --check    non-zero when any pass is ungated
//
// WHY THIS FILE EXISTS
//
// A depth pass returns a `steps` object. That object is spread into `summary.json`, so every
// claim it computes is visible to whoever opens the report — and read by no code at all.
// `steps.prioritiesDense = true` and `steps.prioritiesDense = false` produce a summary that
// differs in exactly one word, and nothing in the harness fails on the difference. The claim
// is decoration. A reviewer who reads the report believes the assertion was enforced; it was
// not, and the only way to know is to read the source and notice the absence of a `record()`.
//
// That mistake has now appeared three times in this repo, each under a different name:
//   * a `failure_rate == 0.0` comparison that a zero-counter made true,
//   * a `has_credential == false` field pinned to a literal,
//   * an `errorOffersRetry` that measured a string inside the element it had just counted.
// None of them could fail. Each was reported as a passing claim.
//
// So the test is not "does a step name exist" — it is "is the step READ by a gate". This
// script is that test, applied to the whole file at once instead of by hand, so the next
// writer learns from a one-line command rather than from a third incident.
//
// WHAT COUNTS AS GATED
//
//   * `record({ severity: "high" | "medium" | ... })` — reaches `findings` in `summary.json`
//     and is counted by the roll-up's `bySeverity`. This is the real gate.
//   * a read of the claim in a conditional (`if (!steps.x)`, `steps.x > 0 &&`, `steps.x?.y`),
//     which is where a `record()` on the false branch would sit.
//
// `report.findings.push(...)` does NOT count: `findings` and `pushFindings` are declared
// inside `main()` at the roll-up, so no depth pass can reach them, and the call throws
// `TypeError: Cannot read properties of undefined (reading 'push')` — which `runDepthPass`
// catches and turns into `{ ok: false }`. A pass written that way is dead from its first
// gated line onward, and every claim after it was collected by a run that had already stopped.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const HERE = dirname(fileURLToPath(import.meta.url));
const SOURCE = join(HERE, "walkthrough.cjs");

const source = readFileSync(SOURCE, "utf8");
const lines = source.split("\n");

/** Depth pass name -> [startLine, endLine) in 0-based line indexes. */
function depthPassSpans(text) {
  const markers = [];
  text.split("\n").forEach((line, index) => {
    const match = /^async function (run\w*Depth)\(/.exec(line);
    if (match) markers.push([index, match[1]]);
  });
  const total = text.split("\n").length;
  markers.push([total, null]);
  const spans = new Map();
  for (let i = 0; i < markers.length - 1; i += 1) {
    spans.set(markers[i][1], [markers[i][0], markers[i + 1][0]]);
  }
  return spans;
}

/**
 * A claim is a `steps.x = …` assignment. A read is any other mention of `steps.x`.
 * `steps.x == y` is deliberately NOT an assignment: only a single `=` immediately after the
 * name (no second `=`) writes, so `===` reads. That is the same distinction a JS parser makes.
 */
function auditPass(body) {
  const assigned = new Set();
  for (const match of body.matchAll(/steps\.([A-Za-z0-9_]+)\s*=(?!=)/g)) {
    assigned.add(match[1]);
  }
  const read = new Set();
  for (const match of body.matchAll(/steps\.([A-Za-z0-9_]+)\s*(=(?!=))?/g)) {
    if (!match[2]) read.add(match[1]);
  }
  // The `gate("claim", …)` helper gates a claim BY NAME, so a static scan that only looks for
  // `steps.claim` cannot see it. Without this, a pass that has done the right thing still reads
  // as ungated — and an audit that cries wolf is an audit people stop running, which is the
  // exact outcome the file exists to prevent. A tool that lies about the thing it was built to
  // measure is worse than no tool.
  for (const match of body.matchAll(/\bgate\(\s*"([A-Za-z0-9_]+)"/g)) {
    read.add(match[1]);
  }
  const ungated = [...assigned].filter((name) => !read.has(name)).sort();
  return { assigned: assigned.size, gated: assigned.size - ungated.length, ungated };
}

const spans = depthPassSpans(source);
const report = [];
for (const [name, [start, end]] of spans) {
  const result = auditPass(lines.slice(start, end).join("\n"));
  report.push({ name, line: start + 1, ...result });
}

const totalAssigned = report.reduce((sum, pass) => sum + pass.assigned, 0);
const totalUngated = report.reduce((sum, pass) => sum + pass.ungated.length, 0);
const clean = report.filter((pass) => pass.assigned === 0 || pass.ungated.length === 0);

for (const pass of report) {
  if (!pass.assigned) continue;
  const share = pass.assigned === 0 ? "n/a" : `${((pass.assigned - pass.ungated.length) / pass.assigned * 100).toFixed(0)}%`;
  console.log(
    `${pass.name} (walkthrough.cjs:${pass.line}) — ${pass.assigned} claims, ${pass.ungated.length} ungated, gated ${share}`,
  );
  if (pass.ungated.length) {
    console.log(`    ${pass.ungated.join(", ")}`);
  }
}
console.log(
  `\n${totalAssigned} claims across ${report.length} depth passes · ${totalAssigned - totalUngated} gated · ${totalUngated} collected only · ${clean.length}/${report.length} passes clean`,
);

if (process.argv.includes("--check")) {
  if (totalUngated > 0) {
    console.error(
      "\nFAIL: " + totalUngated + " claim(s) reach summary.json and gate nothing. " +
        "A record({ severity: … }) on the false branch, or a read of the claim in a conditional, " +
        "is what makes an assertion real.",
    );
    process.exit(1);
  }
  console.log("\nOK: every computed claim is read by a gate.");
}
