/**
 * Static gate for the QA harness's own developer pass (REQ-022, slice 2).
 *
 * ## The defect this file exists to prevent
 *
 * `runDepthPass` wraps every depth pass so a crash cannot end the run. It records the failure and
 * returns `{ ok: false }` — and until tick 110 **nothing read that value**. The roll-up counts
 * clicks, console lines and failed requests; a depth pass that died on its first locator produces
 * none of the three. So the pass that threw and the pass that proved everything wrote the same
 * report, differing only in a few missing screenshots.
 *
 * That is the most expensive failure mode this harness has, because it is *silent in the
 * direction that matters*: the report's own numbers go **up** when a screen breaks, because a
 * broken screen logs console errors. Nothing in the summary said "the pass that was supposed to
 * prove the developer portal did not run".
 *
 * ## What is checked here, and why it cannot be checked by reading
 *
 * Four claims about the harness itself:
 *
 * 1. The three `/developer` routes are in the route list **and** the phone list. A route entry that
 *    exists only in one of them is a screen measured on a desktop and assumed on a phone — the
 *    exact assumption the seven-column tables used to rest on.
 * 2. The pass is *invoked* under a `wants()` guard and reached through `runDepthPass`. A depth
 *    function that exists but is never called is a function that proves nothing, and it parses
 *    perfectly.
 * 3. `runDepthPass` counts its own failures — the `depth-pass-failed` roll-up and the partial-step
 *    recovery are present. This is the claim above, restated as something a test can fail.
 * 4. The pass asserts rather than merely records: it collects failures and throws, because a pass
 *    that fills an object and returns **cannot fail** and is reported green while proving nothing.
 *
 * Run: `node scripts/qa/probe-developer-harness.cjs`
 *
 * Proven to fail: deleting the `developer-api-keys` route, deleting the invocation, removing the
 * roll-up, or removing the final `throw` each turn this gate red with the claim named.
 */
const fs = require("fs");
const path = require("path");

const ROOT = path.resolve(__dirname, "..", "..");
const WALK = path.join(ROOT, "scripts", "qa", "walkthrough.cjs");
const source = fs.readFileSync(WALK, "utf8");

let failures = 0;
const ok = (claim, condition, detail) => {
  if (condition) {
    console.log(`  ok  ${claim}`);
  } else {
    failures += 1;
    console.log(`  FAIL ${claim}${detail === undefined ? "" : ` — ${detail}`}`);
  }
};

// ---- 1. The routes -------------------------------------------------------------------------------
console.log("route list");
for (const [name, route] of [
  ["developer-overview", '"/developer", name: "developer-overview"'],
  ["developer-api-keys", '"/developer/api-keys", name: "developer-api-keys"'],
  ["developer-logs", '"/developer/logs", name: "developer-logs"'],
]) {
  // The desktop list and the phone list are separate arrays in this file, so a route has to be
  // present in BOTH to be measured in both. The count is deliberately > 1: the string also appears
  // in the pass's own `goto` calls, and a test that accepted the first occurrence would pass on a
  // pass that never registered the route.
  const hits = source.split(route).length - 1;
  ok(`${name} is registered`, hits >= 2, `${hits} occurrence(s)`);
}
// The detail route is deliberately absent as a route entry — its id comes from the key the pass
// creates — so what is checked is that the pass CLICKS through to it rather than typing a URL.
ok(
  "the detail screen is reached by clicking the list row",
  source.includes('[data-developer-key-row] a:has-text(') && source.includes("key-detail-reached-by-clicking"),
  "a hand-typed URL would prove the page and not the link",
);

// ---- 2. The pass runs ----------------------------------------------------------------------------
console.log("invocation");
ok(
  "the pass is defined",
  /async function runDeveloperDepth\(/.test(source),
  "runDeveloperDepth is missing",
);
ok(
  "the pass is called behind a --only guard",
  /wants\("developer-api-keys"\)/.test(source) && /runDeveloperDepth\(page, report\)/.test(source),
  "a depth function nothing calls parses perfectly and proves nothing",
);
ok("the pass goes through runDepthPass", /runDepthPass\("developer", \(\) => runDeveloperDepth/.test(source));
ok(
  "the pass name is registered in matchedOnly",
  /matchedOnly\.add\("developer-api-keys"\)/.test(source),
  "an unregistered name makes a focused pass report itself as empty",
);

// ---- 3. A crashed pass is counted -----------------------------------------------------------------
console.log("failure accounting");
ok(
  "runDepthPass records the failure",
  /action: "depth-pass-failed"/.test(source),
);
ok(
  "the roll-up turns a crashed pass into a high finding",
  /clickLines\.filter\(\(e\) => e\.action === "depth-pass-failed"\)/.test(source) &&
    /pushFindings\("high", "depth-pass-failed"/.test(source),
  "without this a crashed pass writes the same report as a pass that proved everything",
);
ok(
  "a crashed pass keeps the steps it had already recorded",
  /\.filter\(\(e\) => e\.action === "depth-pass" && e\.pass === name\)/.test(source),
  "returning a fresh { ok: false } throws away every step taken before the crash",
);
ok(
  "the pass records under the recovery key",
  /action: "depth-pass", pass: "developer"/.test(source),
  "the wrapper looks the steps up by this exact pair",
);

// ---- 4. The pass asserts, it does not merely record -----------------------------------------------
console.log("assertions");
ok(
  "the pass collects failures",
  /const failures = \[\];/.test(source) && /failures\.push\(key\)/.test(source),
);
ok(
  "the pass throws when a claim is unproved",
  /throw new Error\(`developer: \$\{failures\.length\} unproved claim/.test(source),
  "a pass that fills an object and returns cannot fail, and is reported green while proving nothing",
);
ok(
  "an unproved claim is a high finding in the roll-up",
  /pushFindings\(\s*"high",\s*"unproved-claim"/.test(source),
);

// ---- 5. The claims the pass exists to prove ---------------------------------------------------------
console.log("claims");
// These are the acceptance criteria's own words. Each one is a locator or an assertion in the pass,
// and a criterion with no line in the pass is a criterion the browser never checked.
for (const [claim, needle] of [
  ["the secret is printed once", "one-time-reveal-appeared"],
  ["the secret does not survive a reload", "secret-absent-after-reload"],
  ["the printed secret authenticates a guarded call", "key-authenticates-a-guarded-call"],
  ["revocation kills the identical bytes", "revoked-secret-is-dead"],
  ["the keyboard shortcut opens the dialog", "keyboard-n-opens-create"],
  ["the reveal refuses an unacknowledged close", "reveal-survives-escape"],
  ["the log filter narrows and resets", "reset-clears-the-filters"],
  ["the drawer names the resolved scope", "drawer-names-the-scope"],
]) {
  ok(claim, source.includes(`"${needle}"`), `no step named ${needle}`);
}

console.log("");
if (failures > 0) {
  console.error(`developer harness: ${failures} check(s) failed`);
  process.exit(1);
}
console.log("developer harness: all checks passed");
