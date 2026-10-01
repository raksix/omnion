#!/usr/bin/env node
/**
 * The sign-in loop must treat a `429` as a WAIT, not as a refusal — and the report roll-up must
 * survive a page that produced no diagnostics.
 *
 * ## Why this gate exists
 *
 * On 2026-10-01 the w6 pass signed in, was refused `429` when it tried again, and then reported
 * **22 of 44 screens** as "the session was lost — the browser was on the sign-in form". Every one
 * of those screens was measured as a login form: a sign-in form has no broken image, no overflow
 * and no unlabeled input, so the run produced a clean-looking report about screens it never
 * looked at. That is the same shape as tick 59's defect, with a different cause — the diagnosis
 * named a lost session and the network log said `429` three times in the same breath.
 *
 * Then the pass walked 150 screenshots and died in the LAST loop of the report, on
 * `d.horizontalOverflow` with no `diagnostics`, writing a 94-byte summary that said `fatal` and
 * nothing else. So a single untested line at the end of a 12,000-line harness discarded every
 * finding before it.
 *
 * ## What is asserted, and what each assertion would have missed
 *
 * 1. The sign-in loop reads the `429` from `netFailures` (the array `attach` fills) rather than
 *    from the DOM — a `429` never reaches the panel as an alert, so a DOM-only check is always
 *    false and the retry branch is dead code that reads like the fix.
 * 2. The mark is taken BEFORE the click, so a `429` from an earlier phase cannot make a good login
 *    look rate limited.
 * 3. The retry branch exists and waits — a `continue` with a backoff, not a `break`.
 * 4. The scoped helper refuses to match another screen's `429`.
 * 5. The report's per-page roll-up guards a missing `diagnostics` and says NOT MEASURED.
 * 6. The invented `page.__qaNet` property does not come back.
 *
 * Each is measured against the SOURCE, with comments stripped first: this file explains the bug in
 * prose, and a comment containing `429` would satisfy a regex that is only trying to find the
 * string. Run with `--against <file>` to point any check at a different revision.
 */

const fs = require("fs");
const path = require("path");

const target = process.argv.includes("--against")
  ? process.argv[process.argv.indexOf("--against") + 1]
  : path.join(__dirname, "walkthrough.cjs");

const raw = fs.readFileSync(target, "utf8");
// Comments carry the explanation and the literal strings the checks look for, so they are removed
// before anything is matched. Line comments first (which would otherwise eat a `//` inside a URL
// only in the block-comment pass below), then block comments.
const source = raw
  .replace(/\/\*[\s\S]*?\*\//g, "")
  .split("\n")
  .map((line) => line.replace(/^\s*\/\/.*$/, ""))
  .join("\n");

let failures = 0;
const check = (name, ok, detail) => {
  if (ok) {
    console.log(`ok   ${name}`);
  } else {
    failures += 1;
    console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`);
  }
};

// 1. The 429 is read from the response log, not from the DOM.
check(
  "the sign-in loop reads a 429 out of netFailures, the array attach() fills",
  /refusedSince\(mark,\s*"\/auth\/login"\)/.test(source),
  "no call to the scoped helper with the sign-in endpoint",
);
check(
  "a status 429 is what the helper matches on",
  /failure\.status\s*===\s*429/.test(source),
  "the helper does not compare a status of 429",
);
check(
  "the helper is fed netFailures rather than a property invented on the page object",
  /\.slice\(mark\)\s*\.some\(/.test(source) && /netFailures/.test(source),
  "the mark/slice pattern or netFailures is missing",
);

// 2. The mark precedes the click, so a 429 from an earlier phase cannot poison the verdict.
const markAt = source.indexOf("const mark = netFailures.length");
const clickAt = source.search(/const clicked = await primaryClick\(page\)/);
check(
  "the network mark is taken before the sign-in click",
  markAt !== -1 && clickAt !== -1 && markAt < clickAt,
  `mark at ${markAt}, click at ${clickAt}`,
);

// 3. The rate-limited branch WAITS and RETRIES rather than breaking out.
const loopBody = source.slice(source.indexOf("for (let attempt = 0; attempt < 40"), source.indexOf("report.steps.push({ action: \"login\""));
check(
  "a rate-limited attempt waits and continues the loop",
  /rateLimited[\s\S]{0,600}waitForTimeout\(SIGN_IN_BACKOFF_MS\)[\s\S]{0,300}continue;/.test(loopBody),
  "no backoff-and-continue inside the rate-limited branch",
);
check(
  "a genuine refusal (an alert with no 429) still breaks out immediately",
  /if\s*\(state\.alert\)\s*\{\s*refusal\s*=\s*state\.alert;\s*break;/.test(loopBody),
  "the non-429 refusal path is missing or no longer breaks",
);

// 4. The helper is scoped to one endpoint.
check(
  "the 429 check is scoped to a URL fragment rather than matching any 429",
  /refusedSince\([^)]*urlFragment|\.includes\(urlFragment\)/.test(source),
  "the helper matches on the fragment argument, not a fixed path",
);

// 5. The report roll-up survives a page with no diagnostics.
const reportLoop = source.slice(source.indexOf('md.push("## Per-page diagnostics")'));
check(
  "the per-page report loop guards a missing diagnostics object",
  /if\s*\(!d\)\s*\{[\s\S]{0,300}NOT MEASURED[\s\S]{0,200}continue;/.test(reportLoop),
  "the roll-up still dereferences d.horizontalOverflow without a guard",
);

// 6. The invented property must not come back.
check(
  "the invented page.__qaNet property is gone",
  !/__qaNet/.test(source),
  "page.__qaNet is always empty, so the predicate is always false — dead code that reads like a fix",
);

console.log("");
if (failures > 0) {
  console.log(`${failures} check(s) failed against ${target}`);
  process.exit(1);
}
console.log(`all checks pass against ${path.relative(process.cwd(), target)}`);
