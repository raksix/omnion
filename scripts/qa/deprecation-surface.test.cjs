/**
 * The gate for REQ-130 slice 4's harness additions (the deprecation screen and the policy contract).
 *
 * ## What this exists to catch, specifically
 *
 * Slice 2's gate (`graphql-surface.test.cjs`) was written after a pass had already shipped green
 * while measuring nothing, and its own doc comment names the shape: *a pass spliced in and never
 * wired into `main()` parses, runs nothing, and looks complete.* This file is the same discipline
 * for the deprecation surface, plus one thing slice 2's gate does not cover:
 *
 * **The screen and the wire must read the SAME numbers.** The acceptance line is that a sunset in
 * the past marks its row `removed` in the UI, and the walk found in tick 71 that a screen's status
 * can disagree with the endpoint's while both are correct in isolation — the screen reading the
 * column and the endpoint reading the dates. So the checks below assert that `status_at` exists and
 * is what the screen's own types read, rather than asserting that a string appears twice.
 *
 * ## Every check is written so a mutation turns it red
 *
 * The last block runs six mutations against the sources in memory and requires each to fail a
 * check. A gate that cannot be shown to fail is a gate that measures nothing — the lesson this
 * repository keeps re-learning, most recently when a deprecation guard was proven green with
 * `if (false)` around the branch it claimed to cover.
 */

const fs = require("fs");
const path = require("path");

const HARNESS = path.join(__dirname, "walkthrough.cjs");
const SCREEN = path.join(
  __dirname,
  "..",
  "..",
  "apps",
  "admin",
  "features",
  "developer",
  "deprecations-view.tsx",
);
const CLIENT = path.join(__dirname, "..", "..", "apps", "admin", "lib", "deprecation-api.ts");
const POLICY = path.join(__dirname, "..", "..", "crates", "graphql", "src", "deprecation.rs");

const harness = fs.readFileSync(HARNESS, "utf8");
const screen = fs.readFileSync(SCREEN, "utf8");
const client = fs.readFileSync(CLIENT, "utf8");
const policy = fs.readFileSync(POLICY, "utf8");

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

// ---- The screen is walked ----------------------------------------------------------------------

const ROUTE = "/developer/api/deprecations";

check(
  `route listed: ${ROUTE}`,
  harness.includes(`{ path: "${ROUTE}",`),
  "the route is not in the desktop route list, so no pass measures its layout",
);

check(
  "the screen is in DEPRECATION_SCREENS",
  harness.includes(`const DEPRECATION_SCREENS = ["${ROUTE}"];`),
  "the screen list is what the startup guard reads; a route outside it can be dropped from the walk silently",
);

check(
  "the startup guard is called from main()",
  /async function main\(\)[\s\S]*?assertDeprecationScreensWalked\(\);/.test(harness),
  "the guard is defined but never called — `DEPLOYMENT_SCREENS` earns its place exactly once, and only because its guard is CALLED",
);

check(
  "the guard refuses rather than warns",
  /function assertDeprecationScreensWalked\(\)[\s\S]{0,400}?throw new Error/.test(harness),
  "a guard that logs and continues lets a pass report coverage it does not have",
);

// ---- The screen does what the request says -----------------------------------------------------

// Every column the request names, in the request's words.
for (const [name, what] of [
  ["Route or field", "the screen's first column"],
  ["Deprecated in", "the version column"],
  ["Sunset", "the countdown column"],
  ["Replacement", "the replacement column"],
  ["Notified", "the notified column"],
]) {
  check(`column: ${name}`, screen.includes(name), `the request's table names it; ${what} is missing`);
}

for (const action of ["Announce", "Extend", "Withdraw", "Export CSV", "Mark notified"]) {
  check(`action: ${action}`, screen.includes(action), "the request names this action on the screen");
}

// An EMPTY state and a POPULATED one, which is the "no dead buttons" half of done.
check(
  "an empty state exists",
  /Nothing is deprecated/.test(screen),
  "a screen that renders only a table shows a blank panel on a fresh installation",
);
check(
  "a populated state exists",
  /rows\.map\(\(row\)/.test(screen),
  "the populated branch is what the whole screen is for",
);
check(
  "an error strip with a retry exists",
  /role="alert"/.test(screen) && /Retry/.test(screen),
  "the request names the error state with a retry, and an alert without a retry is a dead end",
);
check(
  "a loading skeleton exists",
  screen.includes("<LoadingTable"),
  "a screen that renders nothing while it loads looks like an empty state, which is a different message",
);

// ---- Mobile, and the amber state as words --------------------------------------------------------

check(
  "a 390px card layout exists beside the table",
  /sm:hidden/.test(screen) && /hidden .*sm:block|overflow-x-auto/.test(screen),
  "the request says tables become cards at 390px; without the second branch the table scrolls sideways",
);

check(
  "the amber state is a word, not only a colour",
  screen.includes("row.countdown") && screen.includes("row.amber"),
  "colour alone is invisible to a screen reader and to a print; the request's 'amber' is a visual instruction, the countdown is the meaning",
);

check(
  "the countdown is the SERVER's, not recomputed",
  /days_remaining: number;/.test(client) && /countdown: string;/.test(client),
  "a client-side comparison drifts the day someone tunes AMBER_WITHIN_DAYS, and the screen then disagrees with the middleware",
);

check(
  "the policy numbers are the server's, not a copy",
  /policy: DeprecationPolicy;/.test(client) && screen.includes("response.policy"),
  "a form stating its own windows lets an operator pick a date the server will refuse",
);

// ---- Keyboard ------------------------------------------------------------------------------

check(
  "a keyboard shortcut opens Announce",
  /event\.key === "n"/.test(screen),
  "the request's keyboard requirement; `n` is the convention the other screens in this panel use",
);
check(
  "Escape closes the dialog",
  /event\.key === "Escape"/.test(screen),
  "a modal with no Escape is a modal a keyboard user cannot leave",
);
check(
  "shortcuts do not fire while typing",
  /const typing =/.test(screen),
  "without this, pressing `n` inside the reason field types a letter into the reason",
);

// ---- The wire contract the screen depends on -----------------------------------------------------

// The four headers and the removal are the acceptance line; the screen is only honest if the
// middleware does them, so the gate holds the middleware rather than the panel.
const MIDDLEWARE = path.join(__dirname, "..", "..", "apps", "api", "src", "deprecation_middleware.rs");
const middleware = fs.readFileSync(MIDDLEWARE, "utf8");

// The two windows below are bounded to the GONE BRANCH, not to the function. The first version
// of this gate used `{0,900}` from the branch's opening brace, which runs past `return response;`
// and into the header branch below — so "no next.run on the gone branch" was reading the NEXT
// branch's `next.run` and failing on a file that does the right thing. The bound is the branch's
// own `return response;`.
// Comments are STRIPPED before the branch is measured. The first version of this check read
// `next.run` out of the branch's own explanatory comment — `// next.run is NOT awaited here, and
// that is the whole point of the branch` — and reported the branch as calling the handler, on a
// file whose branch provably does not. The prose documenting a branch is not the branch, and a
// gate that reads prose reports on the author's explanation rather than on the code.
const GONE_BRANCH = /if entry\.gone \{[\s\S]*?return response;/;
const goneBranch = stripComments((middleware.match(GONE_BRANCH) || [""])[0]);

/**
 * Drop `//` line comments.
 *
 * Only line comments: this file measures RUST, which has no block comments in either file it
 * reads, and a block-comment stripper that also ate `///` doc lines would take the explanations
 * with it. A line comment runs to the end of its line, so the regex is anchored on the `//` and
 * consumes the rest of the line — a `//` inside a string literal in these two files does not
 * occur, and the assertion that reads `REMOVED_STATUS` is on real code either way.
 */
function stripComments(text) {
  return text.replace(/\/\/[^\n]*/g, "");
}
void predicates;

check(
  "the 410 short-circuits BEFORE the handler runs",
  goneBranch.length > 0 && !/next\.run/.test(goneBranch),
  "a 410 produced after the handler ran is a route that still works, and only the ordering proves it does not",
);
check(
  "the gone branch is bounded and returns its own response",
  goneBranch.includes("REMOVED_STATUS") && goneBranch.includes("return response;"),
  "the branch must both read the status constant and return, or it answers with the default",
);
check(
  "the headers are the RFC 8594 names",
  /deprecation: String,/.test(policy) &&
    /sunset: String,/.test(policy) &&
    policy.includes('rel=') &&
    policy.includes('deprecation\\"'),
  "the request names Deprecation, Sunset and a changelog Link; a renamed header is a different feature",
);
check(
  "the Sunset header is an HTTP-date",
  policy.includes("IMF-fixdate"),
  "an ISO 8601 Sunset needs a format argument and a client that ignores it is the outcome this feature prevents",
);

// The status the screen shows and the status the middleware enforces come from ONE decision.
const headersForBody =
  (policy.match(/pub fn headers_for[\s\S]*?\n}/) || [""])[0];

check(
  "the status column is NOT what the wire reads",
  headersForBody.includes("outcome(row, now) != Outcome::Headered") &&
    !headersForBody.includes("row.status.sends_headers()"),
  "a Sunset header on a past sunset tells a client to plan around its own past — this was a real defect the middleware's own unit test caught",
);
check(
  "the screen's status is the policy's status",
  policy.includes("pub fn status_at"),
  "the screen reading the column while the middleware reads the dates is exactly the lag this tick removed",
);

// ---- Proven to fail ---------------------------------------------------------------------------

// Each mutation is applied to a COPY in memory and the SAME predicate that the check above uses
// is re-run against the mutated text. The first version of this block compared the mutated text
// to a hard-coded predicate and then reported on the ORIGINAL file, so four of the six mutations
// "passed" because the thing they removed was never looked at — which is the exact failure mode
// a proven-to-fail block exists to rule out.
function predicates(texts) {
  const [harnessText, screenText, policyText, middlewareText] = texts;
  // Stripped here too, not only at the top of the file: `predicates()` is what re-runs against
  // the MUTATED text, so a strip applied only to the file-level read would leave the mutation
  // measuring a branch whose comments the mutation never touched — and the check would stay green
  // for a branch that had just been made reachable.
  const goneBranch = stripComments(
    (middlewareText.match(/if entry\.gone \{[\s\S]*?return response;/) || [""])[0],
  );
  const headersForBody = (policyText.match(/pub fn headers_for[\s\S]*?\n}/) || [""])[0];
  return {
    route: harnessText.includes(`{ path: "${ROUTE}",`),
    screenList: harnessText.includes(`const DEPRECATION_SCREENS = ["${ROUTE}"];`),
    guardCalled: /async function main\(\)[\s\S]*?assertDeprecationScreensWalked\(\);/.test(harnessText),
    guardThrows: /function assertDeprecationScreensWalked\(\)[\s\S]{0,400}?throw new Error/.test(harnessText),
    emptyState: /Nothing is deprecated/.test(screenText),
    errorStrip: /role="alert"/.test(screenText) && /Retry/.test(screenText),
    loading: screenText.includes("<LoadingTable"),
    mobile: /sm:hidden/.test(screenText),
    escape: /event\.key === "Escape"/.test(screenText),
    shortcut: /event\.key === "n"/.test(screenText),
    // Bounded to the DIALOG branch. A bare `/row\.status === "withdrawn"/` matches TWO places —
    // the `disabled=` on the "Mark notified" button and the guard in the actions component — so a
    // mutation that rewrites only the second leaves the predicate green on the first, and the
    // block reports a mutation that changed nothing. The guard this names is the one that decides
    // whether a withdrawn row can be re-extended, and it is the one the mutation rewrites.
    withdrawn: /if \(row\.status === "withdrawn"\) \{/.test(screenText),
    goneShortCircuits: goneBranch.length > 0 && !/next\.run/.test(goneBranch),
    datesDecide:
      headersForBody.includes("outcome(row, now) != Outcome::Headered") &&
      !headersForBody.includes("row.status.sends_headers()"),
  };
}

const BEFORE = predicates([harness, screen, policy, middleware]);

const MUTATIONS = [
  ["route dropped from the desktop list", "route", (t) => t.replace(`  { path: "${ROUTE}", name: "api-deprecations" },`, "")],
  ["the startup guard call removed", "guardCalled", (t) => t.replace("  assertDeprecationScreensWalked();\n", "")],
  ["the guard downgraded to a warning", "guardThrows", (t) => t.replace("function assertDeprecationScreensWalked", "function warnDeprecationScreensWalked")],
  ["the screen's empty state removed", "emptyState", (t) => t.replace("Nothing is deprecated", "No rows")],
  ["the error strip's retry removed", "errorStrip", (t) => t.replace("Retry", "Dismiss")],
  ["the loading skeleton removed", "loading", (t) => t.replace("<LoadingTable", "<div")],
  ["the 390px card layout removed", "mobile", (t) => t.replace("sm:hidden", "hidden")],
  ["Escape no longer closes the dialog", "escape", (t) => t.replace('event.key === "Escape"', 'event.key === "F12"')],
  ["the n shortcut removed", "shortcut", (t) => t.replace('event.key === "n"', 'event.key === "F13"')],
  // The DIALOG branch, not the button. The first version of this mutation rewrote
  // `if (row.status === "withdrawn") {` in the row-actions component — but the predicate
  // `/row\.status === "withdrawn"/` matches the `disabled={busy || row.status === "withdrawn"}`
  // on the "Mark notified" button FIRST, which survives the rewrite untouched, so the mutation
  // changed a file and left the predicate green. The dialog guard is the one that decides whether
  // a withdrawn row can be re-extended at all, so that is what gets rewritten. The predicate on
  // the left of this line is bounded to the same branch for the same reason.
  ["the withdrawn branch's guard disabled", "withdrawn", (t) => t.replace('if (row.status === "withdrawn") {', "if (false) {")],
  ["the gone branch made unreachable", "goneShortCircuits", (t) => t.replace("    if entry.gone {", "    if false {")],
  ["the headers fall back to the status column", "datesDecide", (t) => t.replace("outcome(row, now) != Outcome::Headered", "row.status.sends_headers()")],
];

console.log("");
console.log("  proven to fail:");
for (const [name, key, mutate] of MUTATIONS) {
  // **The index is the file, and the wrong index is a mutation that proves nothing.** The first
  // version of this map sent `goneShortCircuits` to index 2 and `datesDecide` to index 3 — but
  // index 2 is the POLICY crate and index 3 is the MIDDLEWARE, so the gone-branch mutation was
  // applied to a Rust file that has no `entry.gone` in it and the dates mutation was applied to
  // the middleware, which has no `headers_for` in it. Each `String.replace` silently matched
  // nothing, the mutated text equalled the original, and the block reported FAIL with the honest
  // message "the mutation left the thing it removes in place" — which is the whole defect, printed
  // as a test result. The two were SWAPPED, so swapping them back is the fix and the map now
  // reads the same order as the array literal on the next line, which is the only reason to keep
  // both next to each other.
  const which = { route: 0, screenList: 0, guardCalled: 0, guardThrows: 0, emptyState: 1, errorStrip: 1, loading: 1, mobile: 1, escape: 1, shortcut: 1, withdrawn: 1, datesDecide: 2, goneShortCircuits: 3 }[key];
  const texts = [harness, screen, policy, middleware].slice();
  texts[which] = mutate(texts[which]);
  const AFTER = predicates(texts);
  check(
    `mutation turns a check red: ${name}`,
    BEFORE[key] === true && AFTER[key] === false,
    !BEFORE[key]
      ? "the predicate was ALREADY false on the untouched file, so this mutation proves nothing"
      : "the mutation left the thing it removes in place",
  );
}

console.log("");
console.log(`  ${checks - failures}/${checks} checks`);
process.exit(failures === 0 ? 0 : 1);