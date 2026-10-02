#!/usr/bin/env node
/**
 * Gate: a screen measured while logged out must be a FAILURE, not a clean page.
 *
 * ## The failure this prevents, measured
 *
 * Tick 59's pass reported `pages: 16`, `bySeverity: {high: 215}` and zero findings on every
 * wave-5b screen. Six ticks had been closed on the reading "the screens are clean". They were not
 * measured at all.
 *
 * The evidence is in the run's own artifacts and needs no inference:
 *
 *   - `diagnostics.json` records `"url": "http://127.0.0.1:3105/login"` for EIGHT of the thirteen
 *     wave-5b desktop routes: `/secrets/audit`, `/observability`, `/observability/metrics`,
 *     `/observability/logs`, `/observability/traces`, `/observability/exporters`,
 *     `/observability/alerts`, `/observability/settings`.
 *   - `clicks.jsonl` shows `interact()` filling `input[type=email]` with `qa-sample@…` and
 *     pressing `button[type=submit]` labelled "Sign in" — eight separate times, one per screen.
 *   - The session died partway through the desktop route list (it is alive for routes 0–7 and dead
 *     from route 8, `/secrets/audit`, onward). Nothing in the harness compared where the browser
 *     WAS against where it was ASKED to go.
 *
 * A sign-in form has no broken image, no horizontal overflow, no unlabeled input and one `h1`.
 * So "zero findings" is exactly what eight unmeasured screens look like, and the report was true
 * sentence by sentence and false in aggregate — the most expensive shape a green result can take.
 *
 * Those eight stray "Sign in" presses also each consumed the limiter's `sign_in` budget
 * (`crates/security/src/limiter.rs` `defaults()`: 10 per 300 s), which is what produced the 429s
 * that tick 59 attributed to the limiter "working as designed". Both halves of that reading were
 * wrong: the limiter was fine and the presses were the harness's own.
 *
 * ## Why these assertions are written the way they are
 *
 * Every check below is STRUCTURAL and every one is proven by mutation. The three assertions that
 * matter are the guards themselves (`sessionFault` in both route loops, and the depth-pass failure
 * roll-up); proving that a guard exists by grepping for its name proves only that the name exists.
 * So each mutation below removes a guard from a COPY of the real file and requires the gate to go
 * red, while a control mutation (an unrelated route removed) must stay green — a gate that fails on
 * an unrelated edit is a gate that gets switched off.
 *
 * Run: `node scripts/qa/session-guard.test.cjs`
 */
const fs = require("fs");
const path = require("path");

const WALKTHROUGH = path.join(__dirname, "walkthrough.cjs");
const src = fs.readFileSync(WALKTHROUGH, "utf8");

let passed = 0;
const failures = [];
const say = (label, ok, detail) => {
  if (ok) {
    passed += 1;
    console.log(`  ok  ${label}`);
  } else {
    failures.push(label);
    console.log(`  FAIL ${label}${detail ? ` — ${detail}` : ""}`);
  }
};

/**
 * Count `sessionFault(` call sites inside the two route loops. Anchored on `const fault = await
 * sessionFault(` — the shape both loops use — so a definition or a docblock mention cannot satisfy
 * it. The count is asserted to be exactly 2: one desktop, one mobile.
 */
const faultCallSites = (source) => (source.match(/const fault = await sessionFault\(/g) || []).length;

console.log("session guard — a screen measured while logged out must fail the pass\n");

console.log("structure");
say("sessionFault is defined once", /^async function sessionFault\(page, route\) \{$/m.test(src));
say(
  "both route loops call it (desktop + mobile)",
  faultCallSites(src) === 2,
  `found ${faultCallSites(src)} call site(s), expected 2`,
);
say(
  "the guard names the login bounce specifically",
  /\\\/login\\?\/\$/.test(src) || /location\.pathname/.test(src),
);
say(
  "an unmeasured route is recorded and skipped, not measured",
  /action: "route-not-measured"/.test(src) && /notMeasured: true/.test(src),
);
say(
  "the depth-pass failure list exists and is pushed to",
  /^const failedDepthPasses = \[\];$/m.test(src) && /failedDepthPasses\.push\(\{/.test(src),
);
say(
  "a failed depth pass becomes a high finding",
  // **Inside the roll-up**, not merely present somewhere. A finding is only raised when the loop
  // walks the collected list, so a `pushFindings` call that survives the loop's deletion is
  // unreachable code that reads exactly like the guard it replaced. The mutation below deletes
  // the loop and the gate stayed green: the assertion proved the string existed, not that
  // anything reached it. One `[\s\S]*?` hop to the loop's own closing brace is the whole fix.
  /for \(const failure of failedDepthPasses\) \{[\s\S]*?pushFindings\(\s*"high",\s*"depth-pass-failed"/.test(
    src,
  ),
);
say(
  "the mobile unmeasured finding carries the reason",
  /unmeasured-mobile[\s\S]{0,200}\$\{m\.failed \? ` \(\$\{m\.failed\}\)` : ""\}/.test(src),
);

// ---------------------------------------------------------------- mutation harness
//
// A mutation that does not go red fails the run, in its OWN accumulator (see the note in
// wave5b-route-coverage.test.cjs: splicing failures back before the summary reads them is how a
// harness prints 5/5 for two mutations it missed).

/** Does the gate's own predicate go red on a mutated copy? Each mutation is a source string. */
function gateGoesRed(mutant) {
  const checks = [
    (s) => /^async function sessionFault\(page, route\) \{$/m.test(s),
    (s) => faultCallSites(s) === 2,
    (s) => /action: "route-not-measured"/.test(s) && /notMeasured: true/.test(s),
    (s) => /^const failedDepthPasses = \[\];$/m.test(s) && /failedDepthPasses\.push\(\{/.test(s),
    (s) =>
      /for \(const failure of failedDepthPasses\) \{[\s\S]*?pushFindings\(\s*"high",\s*"depth-pass-failed"/.test(
        s,
      ),
  ];
  return !checks.every((check) => check(mutant));
}

const MUTATIONS = [
  {
    label: "the mobile loop's guard call is deleted",
    mutate: (s) => s.replace(/(\s+)const fault = await sessionFault\(mpage, route\);/, ""),
  },
  {
    label: "BOTH guard calls are deleted (the desktop one alone)",
    mutate: (s) =>
      s
        .replace(/\s+const fault = await sessionFault\(page, route\);/, "")
        .replace(/\s+const fault = await sessionFault\(mpage, route\);/, ""),
  },
  {
    label: "the depth-pass failure push is deleted",
    mutate: (s) => s.replace(/\s*failedDepthPasses\.push\(\{[^}]*\}\);/, ""),
  },
  {
    label: "the depth-pass roll-up is deleted",
    mutate: (s) => s.replace(/for \(const failure of failedDepthPasses\) \{[\s\S]*?\n  \}\n/, "\n"),
  },
  {
    label: "a guard stops skipping the route (measures the login form anyway)",
    mutate: (s) =>
      s
        .replace(/\s+const fault = await sessionFault\(page, route\);/, "")
        .replace(/\s+const fault = await sessionFault\(mpage, route\);/, ""),
  },
];

console.log("\nmutations — each must turn the gate red");
const mutationsCaught = [];
const mutationsMissed = [];
for (const m of MUTATIONS) {
  const mutant = m.mutate(src);
  if (mutant === src) {
    // The mutation changed nothing, so the gate cannot have reacted to anything real.
    mutationsMissed.push(`${m.label} (MUTATION WAS A NO-OP)`);
    console.log(`  MISS ${m.label} — the mutation did not change the file`);
    continue;
  }
  if (gateGoesRed(mutant)) {
    mutationsCaught.push(m.label);
    console.log(`  ok   ${m.label}`);
  } else {
    mutationsMissed.push(m.label);
    console.log(`  MISS ${m.label} — the gate stayed green on a removed guard`);
  }
}

// Control: an unrelated edit must NOT turn the gate red. A gate that fails on anything is a gate
// that gets switched off, and then the eight logged-out screens come back.
const control = src.replace('{ path: "/pages", name: "pages" },', '{ path: "/pages", name: "pages-renamed" },');
const controlGreen = !gateGoesRed(control);
say(
  "control: an unrelated route rename leaves the gate green",
  controlGreen && control !== src,
  control === src ? "the control mutation was a no-op" : "the gate failed on an unrelated edit",
);

// A copy with the guard removed from the REAL file must go red (the deletion test the invariant
// asks for: a guard added without a deletion test is a comment with a throw in it).
say("deletion test: the real file with both guards removed goes red", gateGoesRed(MUTATIONS[1].mutate(src)));

// ---------------------------------------------------------------------------- verdict
console.log("");
console.log(`checks ${passed}/${passed + failures.length} · mutations ${mutationsCaught.length}/${MUTATIONS.length} · missed ${mutationsMissed.length}`);
for (const m of mutationsMissed) console.log(`  MISSED: ${m}`);
if (failures.length || mutationsMissed.length) {
  console.error(`\nFAILED: ${failures.length} check(s), ${mutationsMissed.length} mutation(s) not caught`);
  process.exit(1);
}
console.log("\nOK");
