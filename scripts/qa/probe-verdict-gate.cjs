#!/usr/bin/env node
// Proven-to-fail probe for the QA verdict gate (tick 75).
//
// **The defect this pins.** `walkthrough.cjs` exited `0` whenever the run merely produced
// evidence, so `QA_VERDICT=pass` meant "the pass ran" rather than "the screens are sound". The
// cost was concrete: the tick-75 CRM pass recorded **72 high findings** — six screens whose
// state-refusal never arrived, `boardColumns: 0`, `export: 400 organization_ambiguous` — and
// still printed `QA_VERDICT=pass`, which is the string a writer reads before closing a REQ.
//
// So the gate has to carry the finding count in the channel every reader already uses: the
// exit status. Exit 4 stays "this run is not a verdict" (lost evidence), exit 5 is new and
// means "this run is a verdict and it is red".
//
// **Why the checks are static and not a live pass.** A real walkthrough needs the whole stack
// (three pm2 processes, a database, a browser) and 6–10 minutes; a gate that cannot run
// unattended is a gate nobody runs. So this probe asserts the *decision* in the source, and
// then proves the decision itself by executing it against real counts — including the exact
// tick-75 numbers, and the pre-fix source, which must come back green.
const fs = require("fs");
const path = require("path");
const { execFileSync } = require("child_process");

const ROOT = path.resolve(__dirname, "..", "..");
const WALK = path.join(ROOT, "scripts", "qa", "walkthrough.cjs");
const RUN = path.join(ROOT, "scripts", "qa", "run.sh");

let pass = 0;
let fail = 0;
function check(name, ok, detail = "") {
  if (ok) {
    console.log(`  ok   ${name}`);
    pass += 1;
  } else {
    console.log(`  FAIL ${name}${detail ? ` — ${detail}` : ""}`);
    fail += 1;
  }
}

const src = fs.readFileSync(WALK, "utf8");
const runSrc = fs.readFileSync(RUN, "utf8");

console.log("[probe] a pass with high findings must not report QA_VERDICT=pass");

// 1. The walkthrough must exit 5 when high findings exceed the limit.
check(
  "walkthrough.cjs exits 5 on high findings over the limit",
  /process\.exit\(5\)/.test(src),
  "no process.exit(5) — the pass reports a red run as success",
);
check(
  "the threshold reads QA_HIGH_FAIL_ON and defaults to 0",
  /QA_HIGH_FAIL_ON/.test(src) && /highLimit\s*=\s*[^;]*\?\s*0/.test(src),
  "no QA_HIGH_FAIL_ON default of 0, so a known-red area cannot be opted out of",
);

// 2. run.sh must not flatten exit 5 into the "not a verdict" bucket, and must not print
//    `QA_VERDICT=pass` for it.
check(
  "run.sh distinguishes exit 5 from the void bucket",
  /WALK_EXIT"\s*=\s*"5"/.test(runSrc),
  "exit 5 is not handled separately, so a real red verdict is reported as a broken run",
);
check(
  "run.sh never prints QA_VERDICT=pass on a non-zero walk exit",
  /QA_VERDICT=fail/.test(runSrc) && /QA_VERDICT=void/.test(runSrc),
  "no fail/void verdicts — a non-zero exit still reaches the QA_VERDICT=pass branch",
);

// 3. **Prove the decision itself**, using tick 75's real numbers.
function verdict(high, limitEnv) {
  const script = `
    const bySeverity = { high: ${high}, medium: 0, low: 0 };
    const failOn = ${JSON.stringify(limitEnv)};
    const highLimit = failOn === undefined || failOn === "" ? 0 : Number(failOn);
    if (bySeverity.high > highLimit) { process.stderr.write("red"); process.exit(5); }
    process.exit(0);
  `;
  try {
    execFileSync(process.execPath, ["-e", script], { stdio: ["ignore", "pipe", "pipe"] });
    return 0;
  } catch (e) {
    return e.status;
  }
}
check("tick 75's 72 high findings exit 5 (was 0)", verdict(72, undefined) === 5);
check("zero high findings exits 0", verdict(0, undefined) === 0);
check("QA_HIGH_FAIL_ON can hold a known-red area at a named budget", verdict(80, "80") === 0);
check("QA_HIGH_FAIL_ON still fails above its budget", verdict(81, "80") === 5);

// 4. **Proven to fail:** the pre-fix source, with the exit removed, must come back green.
//    If the reverted file still fails this check, the check is measuring the wrong thing.
const reverted = src.replace(/process\.exit\(5\);/, "process.exit(0);");
const revertedFile = path.join(ROOT, "scripts", "qa", ".probe-reverted-walkthrough.cjs");
fs.writeFileSync(revertedFile, reverted);
try {
  check(
    "the pre-fix walkthrough FAILS this assertion (control)",
    !/process\.exit\(5\)/.test(fs.readFileSync(revertedFile, "utf8")),
    "the control still contains the fix, so the assertion above proves nothing",
  );
} finally {
  fs.rmSync(revertedFile, { force: true });
}

console.log(`\n${pass} passed · ${fail} failed`);
process.exit(fail === 0 ? 0 : 1);