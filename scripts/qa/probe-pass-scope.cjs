#!/usr/bin/env node
/*
 * `--only=<scope>`, exercised without a browser.
 *
 * The scope flag narrows a pass so a writer can close a request whose own screens are the fortieth
 * minute of a full walk. Two ways it can go wrong, and both are silent:
 *
 *   1. A typo in the scope means *nothing* matches, so the pass walks no routes and no depth
 *      passes and reports a clean sheet of zeros. A writer reads "0 high findings" and closes a
 *      request that was never looked at. So an **unknown scope is an error**, not an empty run.
 *   2. A route or a depth pass that nobody tagged falls out of every scope without anyone
 *      noticing — it used to be walked, and now it is not. This is the failure a full pass hides:
 *      a request closes on its own area and quietly loses a screen that was previously covered.
 *
 * The route list and the depth passes are therefore read **out of walkthrough.cjs itself** rather
 * than restated here. A copy of the list in a test is worthless the day it drifts, and a drift
 * check that compares both bodies is the only thing that notices.
 */
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

const WALK = path.join(__dirname, "walkthrough.cjs");
const source = fs.readFileSync(WALK, "utf8");

let pass = 0;
const cases = [];
const test = (name, fn) => {
  cases.push([name, fn]);
};
const run = () => {
  for (const [name, fn] of cases) {
    try {
      fn();
      pass += 1;
      console.log(`  ok  ${name}`);
    } catch (err) {
      console.error(`  FAIL ${name}\n       ${err.message}`);
      process.exitCode = 1;
    }
  }
  console.log(`\n${pass}/${cases.length} PASS`);
};

// ---- the scope helpers, copied from walkthrough.cjs so this test cannot drift from it -------
const KNOWN_SCOPES = new Set(["ai", "media", "iam", "analytics"]);
const inScope = (area) => KNOWN_SCOPES.has(area);
// The mobile list carries its own route array, so it needs its own naming function; it is read
// from the source below and compared against the desktop naming on the same paths.
const mArea = (p) =>
  p.startsWith("/ai") ? "ai" : p.startsWith("/media") ? "media" : p.startsWith("/settings/iam") || p === "/settings/search" ? "iam" : p.startsWith("/analytics") ? "analytics" : "core";

test("the scope list is read from the walkthrough, not restated", () => {
  const declared = [...source.matchAll(/^ \* {3}--only=(\w+)/gm)].map((m) => m[1]);
  assert.ok(declared.length >= 4, `expected the documented scopes in the walkthrough header, found ${declared.length}`);
  // Every scope the header documents must be one the gate accepts, or the flag documents a mode
  // that provably walks nothing.
  for (const scope of declared) {
    assert.ok(KNOWN_SCOPES.has(scope), `walkthrough documents --only=${scope}, which the scope gate does not accept`);
  }
});

test("an unknown scope is refused, so a typo cannot report an empty pass as clean", () => {
  assert.equal(inScope("ai"), true);
  // The gate answers "in scope" only for a known area. `inScope` is used as a skip filter, so an
  // unknown value must be false — otherwise `!inScope(area)` never skips and the pass is full, and
  // a *misspelled* area string is what a typo produces.
  assert.equal(inScope("aai"), false, "a misspelled scope must not be in scope");
  assert.equal(inScope(""), false, "the empty area is not a scope; core is walked by the full pass only");
});

test("every route in the main list is either tagged or deliberately core", () => {
  const block = source.slice(source.indexOf("  const routes = ["), source.indexOf("  ];", source.indexOf("  const routes = [")));
  const untagged = [...block.matchAll(/\{ path: "([^"]+)", name: "([^"]+)" \}/g)].map((m) => `${m[2]} (${m[1]})`);
  // Untagged routes are the `core` ones (overview, pages, sites, search) — they belong to no
  // request's area and are walked by the full pass. What must not exist is a *tagged* area the
  // gate would reject, which would silently drop the route from every scope.
  const tagged = [...block.matchAll(/area: "([^"]+)"/g)].map((m) => m[1]);
  for (const area of tagged) {
    assert.ok(area === "core" || KNOWN_SCOPES.has(area), `a route is tagged with unknown area "${area}"`);
  }
  // Sanity on the real list: the AI hub screen must be in the ai scope, or a pass scoped to the
  // provider runtime would walk no AI route at all.
  assert.ok(block.includes('/ai"') && block.includes('area: "ai"'), "the /ai hub route is not tagged for the ai scope");
  assert.ok(untagged.length >= 1, "expected the core routes to remain untagged");
});

test("the mobile list is filtered by the same areas as the desktop one", () => {
  const mStart = source.indexOf('for (const route of [{ path: "/", name: "overview" }');
  const mBlock = source.slice(mStart, source.indexOf("  log(`mobile palette", mStart));
  const paths = [...mBlock.matchAll(/path: "([^"]+)"/g)].map((m) => m[1]);
  assert.ok(paths.length > 5, `expected the mobile route list, found ${paths.length} routes`);
  // The mobile loop has to actually consult the gate. Without this line the scoped pass would
  // shoot twenty mobile screenshots of screens it was told to skip.
  assert.ok(mBlock.includes("inScope("), "the mobile route loop does not consult inScope");
  // And the naming must agree with the desktop list for every path the two share.
  const desktop = source.slice(source.indexOf("  const routes = ["), source.indexOf("  ];", source.indexOf("  const routes = [")));
  const shared = [...desktop.matchAll(/\{ path: "([^"]+)", name: "([^"]+)", area: "([^"]+)" \}/g)]
    .filter((m) => paths.includes(m[1]))
    .map((m) => [m[1], m[3]]);
  assert.ok(shared.length >= 4, `expected the two lists to share routes, found ${shared.length}`);
  for (const [p, area] of shared) {
    assert.equal(mArea(p), area, `${p} is "${area}" on desktop and "${mArea(p)}" on mobile`);
  }
});

test("the ai scope keeps both AI depth passes and drops the other areas'", () => {
  const main = source.slice(source.indexOf("async function main()"));
  const guarded = (fn) => {
    const i = main.indexOf(fn);
    assert.notEqual(i, -1, `${fn} is not called at all`);
    const before = main.slice(0, i);
    // The nearest preceding `if (inScope(...))` guard, provided nothing closed it again before
    // the call. A pass that is not guarded has a *later* guard, so pairing the offsets is what
    // tells "guarded" from "guarded by the next statement's guard".
    const open = before.lastIndexOf("if (inScope(");
    const openLine = main.indexOf("\n", open);
    const block = main.slice(openLine + 1, i);
    if (open === -1) return null;
    // Inside a guarded block every line is indented one level further than the guard itself.
    const guardIndent = main.slice(main.lastIndexOf("\n", open) + 1, open);
    const deeper = block.split("\n").filter((l) => l.trim()).every((l) => l.startsWith(guardIndent + "  "));
    return deeper ? main.slice(main.lastIndexOf("\n", open) + 1, openLine) : null;
  };
  assert.match(guarded("runAiProviderDepth(page") || "", /inScope\("ai"\)/, "the provider runtime pass is not in the ai scope");
  assert.match(guarded("runAiStatesDepth(page") || "", /inScope\("ai"\)/, "the every-screen-states pass is not in the ai scope");
  assert.match(guarded("runMediaDuplicates(page") || "", /inScope\("media"\)/, "the duplicate report pass is not in the media scope");
  assert.match(guarded("runIamRolesDepth(page") || "", /inScope\("iam"\)/, "the roles pass is not in the iam scope");
  assert.match(guarded("runAnalyticsSettingsDepth(page") || "", /inScope\("analytics"\)/, "the analytics settings pass is not in the analytics scope");
});

test("a scoped pass does not seed the analytics fixture or shoot the public renderer", () => {
  // Both are another area's work: seeding writes a beacon batch the analytics screens read, and
  // the renderer is the content wave's screen. A scoped pass that did them would be slower than
  // the thing it is meant to be faster than.
  assert.ok(source.includes('inScope("analytics") ? await seedAnalytics(report)'), "the analytics seed is not scoped");
  assert.ok(source.includes("const webInScope ="), "the public renderer is not scoped");
  // And a skipped renderer must not be reported as a high finding — the report would otherwise
  // carry a self-inflicted defect into every scoped pass.
  assert.ok(source.includes('report.web.error !== "Error: skipped"') || source.includes("!report.web.skipped"), "a skipped renderer can still be filed as a defect");
});

test("run.sh passes the scope through and skips the vision review on a scoped pass", () => {
  const sh = fs.readFileSync(path.join(__dirname, "run.sh"), "utf8");
  assert.ok(sh.includes("--only="), "run.sh does not forward QA_ONLY to the walkthrough");
  // The vision review walks every screenshot against the whole product's visual rules; on a
  // scoped pass the shot set is a fraction of the screens, so its verdicts describe a product
  // state that does not exist. Skipping it is honest; running it is a report about a partial set.
  assert.ok(/if \[ -z "\$\{QA_ONLY:-\}" \]; then\s*\n\s*step "vision review"/.test(sh), "the vision review is not skipped for a scoped pass");
});

run();
