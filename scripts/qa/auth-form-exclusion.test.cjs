/**
 * The gate for `interact()`'s auth-form exclusion (REQ-130, slice 2's run finding).
 *
 * ## The defect this holds shut
 *
 * On tick 69's pass, `interact()` on the overview walked into the sign-in form (a nav click had
 * navigated there), filled it with valid credentials and pressed "Sign in". The platform's limiter
 * answered `429` — its budget is ten sign-ins per five minutes per PROCESS, and the browser is one
 * process. `ensureSignedIn` then could not spend that budget, so every route after it was measured
 * as a login form and reported as "the session was lost". **One incidental click cost the run
 * 44 screens.** That is tick 68's 22-screen defect recurring with the same cause one layer down,
 * and the "fix" that addressed it (`refusedSince` scoped to `/auth/login`) could not see it,
 * because by the time the limiter mattered the budget was already spent.
 *
 * So the exclusion has to exist AND be provable, and the checks below are written to fail on the
 * pre-fix file — which is the only version of "provable" that has meant anything in this
 * repository.
 */

const fs = require("fs");
const path = require("path");

const HARNESS = path.join(__dirname, "walkthrough.cjs");
const source = fs.readFileSync(HARNESS, "utf8");

let failures = 0;
let checks = 0;

function check(name, ok, detail) {
  checks += 1;
  if (ok) {
    console.log(`  ok   ${name}`);
    return true;
  }
  failures += 1;
  console.log(`  FAIL ${name}${detail ? ` — ${detail}` : ""}`);
  return false;
}

// The exclusion must live INSIDE `interact`, before the click, and before the fill.
//
// Locating it by position is the whole point: a guard pasted somewhere else in a 660 KB file
// satisfies a substring check while changing nothing, which is the `if (false)` defect the exports
// gate already caught once.
const interactStart = source.indexOf("async function interact(page, pageName, report) {");
const interactEnd = source.indexOf("\n/**", interactStart);
check("interact is locatable", interactStart > 0 && interactEnd > interactStart);
const body = interactEnd > interactStart ? source.slice(interactStart, interactEnd) : "";

const signInSkip = body.indexOf('outcome: "deferred-signin"');
check("interact skips the sign-in control", signInSkip > 0, "no `deferred-signin` skip in interact()");

const passwordSkip = body.indexOf('outcome: "deferred-password-field"');
check("interact skips a password field", passwordSkip > 0, "no `deferred-password-field` skip in interact()");

// Both must be BEFORE the click/fill that would spend the budget.
const clickAt = body.search(/index \+= 1;/);
const fillAt = body.search(/action: "fill"/);
check("the sign-in skip precedes the click", signInSkip > 0 && clickAt > 0 && signInSkip < clickAt,
  `the skip is at ${signInSkip} and the click at ${clickAt}; a guard after the click changes nothing`);
check("the password skip precedes the click", passwordSkip > 0 && clickAt > 0 && passwordSkip < clickAt,
  `the skip is at ${passwordSkip} and the click at ${clickAt}`);
check("the skips precede the fill", passwordSkip > 0 && fillAt > 0 && passwordSkip < fillAt,
  `the skip is at ${passwordSkip} and the fill at ${fillAt}`);

// The pre-existing sign-out exclusion must survive — this fix must not have replaced it.
check("the sign-out exclusion is still there", body.includes('outcome: "deferred-signout"'));

// And the limiter-aware sign-in path must survive too: excluding the generic pass's click does not
// make `ensureSignedIn` unnecessary, and removing its 429 handling would re-open tick 68's defect.
check(
  "ensureSignedIn still handles a 429",
  /rateLimited = refusedSince\(mark, "\/auth\/login"\)/.test(source),
  "the 429-aware retry in ensureSignedIn is gone, which is the tick-68 defect",
);
check(
  "ensureSignedIn still fills the form deliberately",
  /await email\.fill\(CREDS\.email\)/.test(source),
  "ensureSignedIn no longer signs in — the pass would have no session at all",
);

// =====================================================================================================
// PROVEN-TO-FAIL: the pre-fix file must fail these checks, not merely the mutated one.
// =====================================================================================================

function runChecks(text) {
  let red = 0;
  const start = text.indexOf("async function interact(page, pageName, report) {");
  const end = text.indexOf("\n/**", start);
  const pass = end > start ? text.slice(start, end) : "";
  if (!pass.includes('outcome: "deferred-signin"')) red += 1;
  if (!pass.includes('outcome: "deferred-password-field"')) red += 1;
  const clickAt = pass.search(/index \+= 1;/);
  const signInAt = pass.indexOf('outcome: "deferred-signin"');
  if (!(signInAt > 0 && clickAt > 0 && signInAt < clickAt)) red += 1;
  const passwordAt = pass.indexOf('outcome: "deferred-password-field"');
  const fillAt = pass.search(/action: "fill"/);
  if (!(passwordAt > 0 && fillAt > 0 && passwordAt < fillAt)) red += 1;
  return red;
}

console.log("\nproven-to-fail:");
const control = runChecks(source);
if (control !== 0) {
  failures += 1;
  console.log(`  FAIL control — the clean file already reports ${control} failure(s)`);
} else {
  console.log("  ok   control — the clean file is green");
}

// The pre-fix shape: interact() with no auth exclusion at all. This IS the file that produced the
// 44-screen run, so it is the real counter-example rather than a synthetic mutation.
//
// Built by DELETING the two added blocks by their unique outcome strings, not by a regex over the
// sign-out branch: the first version of this reconstruction used a regex that did not match, and it
// reported "the mutation did not change the file" only because it checked that. A counter-example
// that cannot be built is not a counter-example, and the honest outcome is to refuse rather than to
// count it green.
const preFix = source
  .replace(
    /    \/\/ \*\*An auth form is never filled or submitted by the generic pass[\s\S]*?if \(meta\.type === "password"\) \{\n      record\(\{ page: pageName, i, \.\.\.meta, action: "skip", outcome: "deferred-password-field" \}\);\n      continue;\n    \}\n/,
    "",
  )
  .replace('outcome: "deferred-signin"', 'outcome: "deferred-signin-removed"');
if (preFix === source || preFix.includes('outcome: "deferred-password-field"')) {
  failures += 1;
  console.log("  FAIL the pre-fix reconstruction did not remove the guards");
} else {
  const red = runChecks(preFix);
  if (red === 0) {
    failures += 1;
    console.log("  FAIL PROVEN NOT TO FAIL — the pre-fix file passes");
  } else {
    console.log(`  ok   the pre-fix file — ${red} check(s) went red`);
  }
}

// A guard placed AFTER the click: reads exactly like the fix and is dead.
const afterClick = source.replace('outcome: "deferred-password-field"', 'outcome: "deferred-password-field-late"').replace(
  /if \(meta\.type === "password"\) \{[\s\S]*?\n    \}\n/,
  "",
);
const afterRed = runChecks(afterClick);
if (afterClick === source || afterRed === 0) {
  failures += 1;
  console.log("  FAIL the guard-moved-below-the-click mutation stayed green");
} else {
  console.log(`  ok   the password guard removed — ${afterRed} check(s) went red`);
}

console.log(`\n${checks} checks, 2 proven-to-fail cases`);
if (failures > 0) {
  console.log(`FAILED: ${failures}`);
  process.exit(1);
}
console.log("PASS");