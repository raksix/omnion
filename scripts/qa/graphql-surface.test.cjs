/**
 * The gate for REQ-130 slice 2's harness additions (the GraphQL screens and their depth pass).
 *
 * ## Why this exists rather than a `node --check`
 *
 * `node --check` on this repository's harness proves the file PARSES, which is a much weaker claim
 * than the one that has bitten four times now:
 *
 * * A pass spliced in and never wired into `main()` parses, runs nothing, and looks complete.
 * * A route added to `routes` and missing from the mobile list parses and measures one width.
 * * The startup guard `assertGraphqlScreensWalked` refuses a pass whose screens are not walked — and
 *   that guard is only as good as the list it reads, which is the thing this gate measures.
 *
 * So the checks below are structural claims about the file, and each is written so that a mutation
 * turns it red. The last block does that: it runs the four mutations against the source in memory
 * and requires each one to fail a check.
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

// ---- The screens are walked ------------------------------------------------------------------

const screens = [
  "/developer/graphql",
  "/developer/graphql/documents",
  "/developer/graphql/schema",
  "/developer/graphql/settings",
];

for (const screen of screens) {
  check(
    `route listed: ${screen}`,
    source.includes(`{ path: "${screen}",`),
    "the route is not in the desktop route list, so no pass measures its layout",
  );
}

check(
  "the startup guard is called from main()",
  /async function main\(\)[\s\S]*?assertGraphqlScreensWalked\(\);/.test(source),
  "the guard is defined but never called, which is the `DEPLOYMENT_SCREENS` lesson arriving again",
);

// ---- The pass exists, and is WIRED ----------------------------------------------------------------

check(
  "the depth pass is defined exactly once",
  source.split("async function runGraphqlDepth").length - 1 === 1,
  "the pass is defined zero or twice times — either unwalked or shadowed",
);

check(
  "the depth pass is called through runDepthPass",
  /runDepthPass\("graphql", \(\) => runGraphqlDepth\(page, report\)\)/.test(source),
  "the pass is defined but never called; a defined-but-uncalled pass reports nothing and looks green",
);

check(
  "the pass is behind a wants() filter",
  /if \(wants\("graphql-depth"\)\) \{[\s\S]{0,200}?runGraphqlDepth/.test(source),
  "the pass is not behind a wants() filter, so --only cannot select it",
);

check(
  "the pass name is registered in matchedOnly",
  source.includes('matchedOnly.add("graphql-depth")'),
  "a focused --only pass would report this screen as unmatched even though it ran",
);

// ---- The pass ASSERTS, rather than filling a summary object ----------------------------------------

const passStart = source.indexOf("async function runGraphqlDepth");
const passEnd = source.indexOf("\nasync function runSecurityDepth", passStart);
check("the pass body is locatable", passStart > 0 && passEnd > passStart);
const body = passEnd > passStart ? source.slice(passStart, passEnd) : "";

const claimNames = [
  "registered-row-appears",
  "registered-row-is-active",
  "detail-shows-the-document-text",
  "revoke-warns-with-a-count",
  "revoke-is-visible-on-the-row",
  "meter-reads-the-endpoint-budget",
  "over-depth-is-refused-before-sending",
  "run-is-blocked-on-an-over-budget-document",
  "result-carries-the-endpoint-cost",
  "explorer-lists-visible-types",
  "role-diff-answers",
  "out-of-range-is-explained-per-field",
  "save-is-blocked-while-a-field-is-out-of-range",
];

for (const claim of claimNames) {
  check(
    `the pass asserts ${claim}`,
    body.includes(`"${claim}"`),
    "a missing claim is a screen behaviour nothing measures",
  );
}

check(
  "the pass records a HIGH finding on a failed claim",
  body.includes('severity: "high"'),
  "a failed claim that records a note is invisible in the roll-up — the observability pass's defect",
);

check(
  "the pass throws on a failed claim",
  /const failed = claims\.filter\(\(claim\) => !claim\.ok\);[\s\S]{0,400}?throw new Error/.test(body),
  "a pass that returns {ok: true} with failed claims is worse than no pass",
);

// ---- The detail screen is opened from a row, not walked with a fabricated id ---------------------------

check(
  "the detail screen is opened by clicking a registry row",
  /row\.locator\("a"\)\.first\(\)\.click/.test(body),
  "the detail screen's path carries an id; walking it directly with a placeholder proves only the 404",
);

// ---- The pass restores the policy it rewrites -------------------------------------------------------

// A fixed `waitForTimeout` after a click is a HOPE, and two consecutive runs of this pass failed
// two DIFFERENT claims inside the same walk — the signature of races rather than defects, in a
// pass whose screenshots showed both screens rendering correctly. So the two waits that follow a
// write are asserted to be SELECTOR waits: the dialog appearing, and its detaching.
check(
  "the revoke dialog is awaited by selector, not by a fixed span",
      /waitForSelector\("\[data-graphql-revoke-dialog\]", \{ timeout: 15000 \}\)/.test(body),
      "the claim right after the revoke click is a race — the dialog renders only after its own fetch of the caller count",
    );
    check(
      "the revoke dialog's DISAPPEARANCE is awaited, not slept through",
      /waitForSelector\("\[data-graphql-revoke-dialog\]", \{ state: "detached", timeout: 20000 \}\)/.test(body),
      "reading the row's new state after a fixed span measures the race, not the revoke",
    );
    check(
      "the run button is located by its key hint, not by the word Run",
      body.includes('hasText: /⌘↵/'),
      "filtering on /^Run/ matches the SECONDARY control, which is correctly enabled on an over-budget document",
    );

  check("the pass restores max_depth after saving it",
  /fill\("10"\)[\s\S]{0,600}?button\[type='submit'\]/.test(body),
  "the pass rewrites an installation-wide settings row; leaving it changed makes every later pass measure a different platform",
);

// =====================================================================================================
// PROVEN-TO-FAIL: each mutation must turn a check red. A gate that cannot fail is decoration.
// =====================================================================================================

function runChecks(text) {
  let failedCount = 0;
  const run = (ok) => {
    if (!ok) failedCount += 1;
  };
  const screensToCheck = [
    "/developer/graphql",
    "/developer/graphql/documents",
    "/developer/graphql/schema",
    "/developer/graphql/settings",
  ];
  for (const screen of screensToCheck) run(text.includes(`{ path: "${screen}",`));
  run(/async function main\(\)[\s\S]*?assertGraphqlScreensWalked\(\);/.test(text));
  run(text.split("async function runGraphqlDepth").length - 1 === 1);
  run(/runDepthPass\("graphql", \(\) => runGraphqlDepth\(page, report\)\)/.test(text));
  run(/if \(wants\("graphql-depth"\)\) \{[\s\S]{0,200}?runGraphqlDepth/.test(text));
  run(text.includes('matchedOnly.add("graphql-depth")'));

  const start = text.indexOf("async function runGraphqlDepth");
  const end = text.indexOf("\nasync function runSecurityDepth", start);
  const passBody = end > start ? text.slice(start, end) : "";
  for (const claim of [
    "registered-row-appears",
    "revoke-warns-with-a-count",
    "meter-reads-the-endpoint-budget",
    "over-depth-is-refused-before-sending",
    "result-carries-the-endpoint-cost",
    "role-diff-answers",
    "out-of-range-is-explained-per-field",
  ]) {
    run(passBody.includes(`"${claim}"`));
  }
  run(passBody.includes('severity: "high"'));
  run(
    /const failed = claims\.filter\(\(claim\) => !claim\.ok\);[\s\S]{0,400}?throw new Error/.test(
      passBody,
    ),
  );
  run(/row\.locator\("a"\)\.first\(\)\.click/.test(passBody));
  run(/fill\("10"\)[\s\S]{0,600}?button\[type='submit'\]/.test(passBody));
  return failedCount;
}

const mutations = [
  ["a screen dropped from the route list", (t) => t.replace('{ path: "/developer/graphql/schema", name: "graphql-schema" },', "")],
  ["the startup guard unhooked from main()", (t) => t.replace("  assertGraphqlScreensWalked();\n", "")],
  ["the pass definition deleted", (t) => t.replace(/async function runGraphqlDepth[\s\S]*?\n}\n/, "")],
  ["the pass call removed from main()", (t) => t.replace(/runDepthPass\("graphql", \(\) => runGraphqlDepth\(page, report\)\)/, '({ ok: true, steps: 0 })')],
  ["a failed claim downgraded from high", (t) => t.replace('severity: "high"', 'severity: "low"')],
  ["the throw on failure replaced by a return", (t) => t.replace(/throw new Error\(\s*`graphql depth: \$\{failed\.length\}/, 'return ({ ok: true, claims }) // ')],
  ["the detail screen opened by a fabricated route instead of a row", (t) => t.replace(/row\.locator\("a"\)\.first\(\)\.click/, 'page.goto(`${URL_ADMIN}/developer/graphql/documents/00000000-0000-0000-0000-000000000000`)')],
  ["the policy restore removed", (t) => t.replace(/await depthField\.fill\("10"\)[\s\S]{0,600}?\.catch\(\(\) => \{\}\);\n/, "")],
];

console.log(`\nproven-to-fail — ${mutations.length} mutations, each must turn a check red:`);
const control = runChecks(source);
if (control !== 0) {
  failures += 1;
  console.log(`  FAIL the CONTROL run already reports ${control} failure(s); a gate that is red on clean is not a gate`);
} else {
  console.log("  ok   control — the clean file is green");
}

for (const [label, mutate] of mutations) {
  const mutated = mutate(source);
  if (mutated === source) {
    failures += 1;
    console.log(`  FAIL ${label} — the mutation did not change the file, so nothing was proven`);
    continue;
  }
  const red = runChecks(mutated);
  if (red === 0) {
    failures += 1;
    console.log(`  FAIL ${label} — PROVEN NOT TO FAIL: the gate stayed green`);
  } else {
    console.log(`  ok   ${label} — ${red} check(s) went red`);
  }
}

console.log(`\n${checks} checks, ${mutations.length + 1} proven-to-fail cases`);
if (failures > 0) {
  console.log(`FAILED: ${failures}`);
  process.exit(1);
}
console.log("PASS");