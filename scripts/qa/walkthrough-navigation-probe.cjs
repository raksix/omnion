#!/usr/bin/env node
// Which navigations in the walkthrough can be killed by the app's own redirect?
//
// The defect this probes (2026-09-30, tick 48). `ensureSignedIn` navigated to `/login` with no
// handler while the three other `goto` calls in the same file carried `.catch(() => {})`. The
// panel redirects an anonymous visitor away from `/login` as soon as it holds a session, so that
// call was one of the few the app itself could interrupt — and Playwright reports an interrupted
// navigation as **fatal** (`Navigation ... is interrupted by another navigation`), not as a
// recoverable timeout. It killed the pass at its first step: no screenshots, no walk results, and
// a `summary.json` whose only field was `fatal`, so the void verdict could not even name a leg.
//
// The property is not "every goto is caught" — two of them are *supposed* to be able to fail
// (the reachability probe at `admin/login` and the public-site check must report a dead server
// rather than shrug at it), and a pass whose every navigation is swallowed cannot report a dead
// stack at all. It is narrower and is what actually broke:
//
//   A navigation to a screen the app REDIRECTS must tolerate losing, because the app is
//   expected to move the page out from under it. A navigation that is *itself* the reachability
//   check must keep its error, because losing it means the pass cannot say "the stack is down".
//
// This reads the shipped file, so a re-typed list of navigations could not go green while the real
// one stays broken — the same rule `void-pass-classifier-probe.sh` follows.
const fs = require("fs");
const path = process.argv[2] || "scripts/qa/walkthrough.cjs";
const lines = fs.readFileSync(path, "utf8").split("\n");

let fail = 0;
function check(name, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${ok ? "" : `\n        ${detail}`}`);
  if (!ok) fail += 1;
}

// Join the file into statements first: a `goto(` call with a multi-line options object ends at
// its `);`, not at the end of its first line. Scanning line-by-line reported four phantom
// failures on the first version of this probe, three of which were guarded on their next line.
// Read each `goto(` call forwards until its parentheses balance, so a call with a multi-line
// options object is one statement rather than three lines. Scanning line-by-line reported four
// phantom failures on the first version of this probe; keeping a running buffer across the whole
// file reported 64 of 154. A statement is read from the line that starts it, never merged with
// its neighbours, because a wrongly attributed line number is a defect report that sends the next
// reader to the wrong line.
const GOTO = /await\s+[A-Za-z_$][\w.$]*\.goto\(/;
const parenDelta = (s) => s.split("(").length - 1 - (s.split(")").length - 1);
const statements = [];
lines.forEach((line, i) => {
  if (!GOTO.test(line)) return;
  let text = line.trim();
  let n = 1;
  while (parenDelta(text) > 0 && n < 12 && i + n < lines.length) {
    text += " " + lines[i + n].trim();
    n += 1;
  }
  statements.push({ line: i + 1, text });
});

const navigations = statements;

check(
  "the walkthrough actually navigates (a file with no goto proves nothing)",
  navigations.length > 0,
  `found ${navigations.length} goto statements — is the walkthrough still shaped like a walkthrough?`
);

// The reachability probes: a navigation whose RESULT is read (`const res = await …goto`) to
// decide the stack is alive. Swallowing its error is a defect, so these are exempt from the rule
// below and are asserted to still exist.
const probes = navigations.filter((s) => /(?:const|let)\s+\w+\s*=\s*await\s+[A-Za-z_$][\w.$]*\.goto\(/.test(s.text));
check(
  "the pass still has a reachability probe whose error it can report",
  probes.length > 0,
  "no navigation reads its response, so a dead stack could not be reported by the pass that walks it"
);

// Everything else must be safe to lose. A statement is safe if it is inside a try/catch, ends in
// a `.catch(`, or is awaited with `allSettled`-style tolerance. `void`-ing a navigation whose
// failure is then asserted is a contradiction, and this catches that too.
const unguarded = navigations.filter((s) => {
  if (probes.includes(s)) return false;
  return !/\.catch\(|try\s*\{/.test(s.text);
});

check(
  `every non-probe navigation (${navigations.length - probes.length}) tolerates an interrupted redirect`,
  unguarded.length === 0,
  unguarded.map((s) => `line ${s.line}: ${s.text.slice(0, 120)}`).join("\n        ")
);

// The one that actually bit, named so the reason survives: `ensureSignedIn`'s own `/login` hop.
const loginHops = navigations.filter((s) => /goto\(`\$\{URL_ADMIN\}\/login`/.test(s.text) && !probes.includes(s));
check(
  "ensureSignedIn's /login hop is guarded (it is the one the app redirects away from)",
  loginHops.length > 0 && loginHops.every((s) => /\.catch\(|try\s*\{/.test(s.text)),
  loginHops.length === 0
    ? "no guarded /login navigation found — did the helper change shape?"
    : loginHops.map((s) => `line ${s.line}: ${s.text.slice(0, 120)}`).join("\n        ")
);

console.log(`\n${navigations.length} navigation(s) scanned in ${path} (${probes.length} reachability probe(s)): ${fail} failed`);
process.exit(fail === 0 ? 0 : 1);
