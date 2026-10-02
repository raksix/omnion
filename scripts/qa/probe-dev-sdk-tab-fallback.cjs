#!/usr/bin/env node
/**
 * `initialTab()` — the fallback that decides which tab a hand-edited URL gets.
 *
 * The sibling `probe-dev-sdk-screen.cjs` checks that this function EXISTS. That check has a
 * limit worth naming rather than hiding: it matches text, so replacing the body with
 * `return (value as Tab) || "plugin"` still passes it — which returns `"wat"` unchanged and
 * renders a screen with no tab selected at all. The mutation run proved it (M7: green).
 *
 * A text check cannot see behaviour. This one imports the function and asks it, so the mutation
 * above fails here. It is a `.mjs` probe rather than a unit test because the function lives in a
 * `.tsx` that imports the API client and eleven icon components; pulling it out to be testable
 * would mean a module that exists only for the test. Reading the one function's source out of the
 * file and evaluating *just that declaration* keeps the assertion honest about what it runs —
 * it is the file's text, and the file is the shipped one.
 *
 * The claim under test is a security-adjacent one, not a cosmetic one: an unvalidated `?tab=`
 * flows straight into `ScaffoldTab kind={tab}`, where it is passed to `previewScaffold()` as the
 * `kind` in a POST body. A value that reached the server unvalidated would be a request the panel
 * builds from a URL string.
 */

const fs = require("node:fs");
const path = require("node:path");

const ROOT = path.resolve(__dirname, "..", "..");
const VIEW = path.join(ROOT, "apps/admin/features/developer/developer-sdks-view.tsx");

let failures = 0;
const pass = (msg) => console.log(`PASS  ${msg}`);
const fail = (msg) => {
  failures += 1;
  console.log(`FAIL  ${msg}`);
};

const source = fs.readFileSync(VIEW, "utf8");

// Lift the two declarations the claim is about, with their real text. `isTab` first: the
// function under test calls it, and evaluating `initialTab` alone would be a ReferenceError.
const isTabMatch = source.match(/function isTab\(value: string \| null\): value is Tab \{[\s\S]*?\n\}/);
const initialMatch = source.match(/export function initialTab\(value: string \| null\): Tab \{[\s\S]*?\n\}/);

if (!isTabMatch || !initialMatch) {
  fail("the view no longer declares isTab()/initialTab() — this probe cannot run");
  console.log("\n1 check failed");
  process.exit(1);
}

// The two type annotations are TypeScript, so they are stripped rather than the body: stripping
// the body would be rewriting the thing under test, which is the mistake the mutation above
// demonstrates. Only the annotations go.
const js =
  (isTabMatch[0] + "\n" + initialMatch[0])
    // `export` is not valid inside a Function body, and dropping the keyword is not a rewrite of
    // the behaviour — the declaration is otherwise byte-identical to the shipped one.
    .replace("export function initialTab", "function initialTab")
    .replace(/value: string \| null/g, "value")
    .replace(/\): value is Tab/, ")")
    .replace(/\): Tab/, ")")
    // Type assertions and non-null marks go for the same reason the annotations do: they are
    // type-level only. This matters beyond tidiness — the first mutation run of this probe hit
    // `(value as Tab) || "plugin"` and reported "could not be evaluated: Unexpected identifier
    // 'as'", which is a *parse* failure. A probe that can only fail by refusing to compile is a
    // probe that reports "the function is broken" for a mutation whose real defect is that it
    // returns `"wat"`. The cast has to come out for the claim to be the claim.
    .replace(/\s+as\s+Tab\b/g, "")
    .replace(/\s+as\s+[A-Za-z_$][\w$]*\b/g, "");

let initialTab;
try {
  initialTab = new Function(`${js}; return initialTab;`)();
} catch (err) {
  fail(`initialTab could not be evaluated: ${err.message}`);
  console.log("\n1 check failed");
  process.exit(1);
}

// Every tab the strip offers, plus the ones it must refuse.
for (const good of ["plugin", "theme", "workflow", "cli"]) {
  const got = initialTab(good);
  if (got === good) pass(`?tab=${good} selects ${good}`);
  else fail(`?tab=${good} selected ${JSON.stringify(got)} instead of itself`);
}

// The load-bearing refusal: an unknown value must fall back, and `value as Tab || "plugin"` —
// the mutation that sailed past the text probe — returns the garbage unchanged.
for (const bad of [null, "", "wat", "CLI", "../secrets", "plugin;drop", "constructor"]) {
  const got = initialTab(bad);
  if (good_set_has(got)) pass(`?tab=${JSON.stringify(bad)} falls back to a real tab (${got})`);
  else fail(`?tab=${JSON.stringify(bad)} returned ${JSON.stringify(got)}, which is not a tab`);
}

// `constructor` is the one that matters beyond cosmetics: a value like that reaching
// `kind={tab}` is a request built from a string the URL chose.
function good_set_has(v) {
  return v === "plugin" || v === "theme" || v === "workflow" || v === "cli";
}

const total = 4 + 7;
console.log(failures === 0 ? `\n${total}/${total} initialTab checks passed` : `\n${failures} check(s) failed`);
process.exit(failures === 0 ? 0 : 1);
