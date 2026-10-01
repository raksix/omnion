#!/usr/bin/env node
// Every depth pass must be handed to `runDepthPass` as a THUNK, and must report through `note`.
//
// Two defects, both found by a pass that ran for fifty minutes and produced no summary:
//
//   1. `runDeploymentMigrationsDepth` ended with `report.push({...})`. `report` is the run's
//      OBJECT (`{ startedAt, admin, steps, pages, ... }`), not an array, so `.push` is undefined
//      and the line threw — on the pass's LAST statement, after every check had run and every
//      screenshot had been taken. The throw unwound the process and cost the whole report.
//
//   2. That pass was wired as a THIRD ARGUMENT to `runDepthPass(name, pass)`. JavaScript
//      evaluates arguments eagerly at the call site, so it ran outside the helper's try/catch —
//      the guard protected nothing. The signature was never wrong (a third argument is ignored),
//      which is exactly why nothing complained until the throw arrived.
//
// The cost of both: `report.push is not a function` after 400+ screenshots, no summary.json, and
// the three depth passes queued behind it never ran. So this gate exists to make the class
// unrepresentable rather than to catch the one instance.
//
// What it checks, against the source rather than a running browser:
//   - no depth-pass body calls `report.push` (report is an object everywhere in this file);
//   - every `runDepthPass(` call passes exactly TWO arguments — a name and a thunk — so nothing
//     is evaluated eagerly outside the guard;
//   - the migrations pass is INSIDE the artifacts thunk, not merely mentioned near it.
//
// Comments are stripped before matching, with the reason inline: this file names both defects in
// prose, so a comment containing `report.push` or a third argument would otherwise satisfy the
// very regex meant to forbid them.
const fs = require("fs");
const path = require("path");

const src = fs.readFileSync(path.join(__dirname, "walkthrough.cjs"), "utf8");
const stripComments = (text) =>
  text.replace(/\/\*[\s\S]*?\*\//g, " ").replace(/^[ \t]*\/\/.*$/gm, " ");

const failures = [];
const check = (name, ok, why) => {
  if (!ok) failures.push(`${name} — ${why}`);
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}`);
};

const code = stripComments(src);

// ---- 1. no depth pass reports by mutating `report` as if it were an array -------------------
// Scoped to depth-pass bodies by name, so an unrelated `steps.push` elsewhere cannot be blamed
// and a legitimate array in some other helper cannot excuse one here.
// The match is (full, capture-group-1 = the name, index). `m[2]` is undefined — a capture group
// that did not match is `undefined`, not `null` — so reading `at` from it sent `indexOf` a
// missing offset and the failure came back naming NO pass, which is the one thing a defect
// report must never do: "report.push — " tells the reader nothing about where to look.
const depthBodies = [...code.matchAll(/async function (run[A-Za-z0-9]+Depth)\([^)]*\)\s*\{/g)];
check(
  "depth passes were found to inspect",
  depthBodies.length >= 10,
  `${depthBodies.length} found — if this renamed the passes, this gate no longer covers them`,
);
const pushOffenders = [];
for (const match of depthBodies) {
  const name = match[1];
  const start = code.indexOf("{", match.index);
  const end = code.indexOf("\nasync function ", start);
  const body = code.slice(start, end > 0 ? end : undefined);
  if (/\breport\s*\.\s*push\s*\(/.test(body)) pushOffenders.push(name);
}
check(
  "no depth pass calls report.push",
  pushOffenders.length === 0,
  `${pushOffenders.join(", ")} — report is the run's object, so this is a TypeError on the last line of the pass`,
);

// ---- 2. every runDepthPass call hands over exactly two arguments ----------------------------
// The third argument of a two-parameter helper is not "an extra pass": it is work evaluated
// eagerly, outside the try/catch that is the entire reason the helper exists.
const calls = [...code.matchAll(/runDepthPass\(/g)];
check("runDepthPass call sites were found", calls.length > 0, "none found — the helper was renamed");
const arityOffenders = [];
for (const call of calls) {
  const open = call.index + call[0].length - 1;
  // Walk the argument list by bracket depth so a `()` inside a string or a nested call cannot
  // end the scan early.
  let depth = 0;
  let commas = 0;
  let i = open;
  let inStr = null;
  for (; i < code.length; i += 1) {
    const ch = code[i];
    if (inStr) {
      if (ch === "\\") i += 1;
      else if (ch === inStr) inStr = null;
      continue;
    }
    if (ch === '"' || ch === "'" || ch === "`") inStr = ch;
    else if ("([{".includes(ch)) depth += 1;
    else if (")]}".includes(ch)) {
      depth -= 1;
      if (depth === 0) break;
    } else if (ch === "," && depth === 1) commas += 1;
  }
  // A TRAILING comma is not an argument. This file is formatted with one
  // (`runDepthPass("name", () =>\n  runPass(page, report),\n);`), and counting it turned sixteen
  // correct two-argument calls into "3 args" — a gate that reports sixteen phantom violations is
  // a gate nobody runs. The comma only separates arguments when something follows it.
  const argText = code.slice(open + 1, i);
  const trimmed = argText.trim();
  const trailingComma = /,\s*$/.test(trimmed);
  let argCount = trimmed === "" ? 0 : commas + 1 - (trailingComma ? 1 : 0);
  const line = code.slice(0, call.index).split("\n").length;
  if (argCount !== 2) arityOffenders.push(`line ${line} (${argCount} args)`);
}
check(
  "every runDepthPass call passes exactly two arguments",
  arityOffenders.length === 0,
  `${arityOffenders.join("; ")} — a third argument is evaluated eagerly, outside runDepthPass's try/catch`,
);

// ---- 3. the migrations pass is inside the artifacts thunk, not called beside it -------------
const block = code.slice(code.indexOf('report.deploymentArtifacts = await runDepthPass'));
const nextBlock = code.indexOf('report.deploymentInstall');
const wiring = block.slice(0, nextBlock > 0 ? nextBlock - block.indexOf('report.deploymentArtifacts = await runDepthPass') : undefined);
check(
  "the migrations pass runs inside the guarded thunk",
  /async\s*\(\)\s*=>\s*\{/.test(wiring) && /await\s+runDeploymentMigrationsDepth\s*\(/.test(wiring),
  "outside the thunk it still runs, and still unguarded — the guard has to wrap the call",
);

if (failures.length) {
  console.error(`\n${failures.length} depth-pass thunk check(s) failed:`);
  for (const f of failures) console.error(`  - ${f}`);
  process.exit(1);
}
console.log(`\nall depth-pass thunk checks passed (${depthBodies.length} depth passes inspected)`);
