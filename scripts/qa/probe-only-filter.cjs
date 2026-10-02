#!/usr/bin/env node
/**
 * scripts/qa/probe-only-filter.cjs — a filter that silently runs wide is worse than no filter.
 *
 * `--only` exists because a full pass over the route list plus thirty-odd depth passes does not
 * finish inside the budget a browser pass is given, and a pass that is cut off proves nothing about
 * the screens after the cut while still looking like a pass. So the loop spends one focused pass on
 * the screen it just built, and the wave-3 tick that started this file said in its own build log
 * that a `--only=workflow-table` run is "a bounded five-minute run rather than a twenty-five-minute
 * gamble".
 *
 * **It was not, and the reason is in the source.** `runDepthPass` is called 49 times. Thirty-four of
 * those call sites are wrapped in `if (wants("…"))`, and those are individually focusable through the
 * route filter. The other fifteen are **not** — including `automations`, `automations-operations`,
 * `workflow-builder` and `workflow-table` itself. For those, focusability comes from a second,
 * separate lookup: the `DEPTH_PASSES` table and the early exit at line ~9213.
 *
 * **The defect.** `DEPTH_PASSES`'s keys are **de-hyphenated** (`workflowtable`, `automationsoperations`,
 * `iamauthentication`) while every other name in the file — the `runDepthPass("workflow-table", …)`
 * call sites, the `wants("…")` guards, the roll-up's matched-name set — is **hyphenated**. The early
 * exit does `DEPTH_PASSES[only]` with no normalisation, so `--only=workflow-table`, the spelling
 * `run.sh`'s own comment recommends, matched nothing: the exit never fired, the pass walked the full
 * inventory, and `clicks.jsonl` filled with `automations-operations-depth` while the tick believed it
 * had bought a five-minute measurement.
 *
 * **Why four ticks missed it.** It fails *wide*, not red. The run produces a summary, walks routes,
 * exits 0, and prints a banner that correctly says `focused: workflow-table` — because `run.sh` prints
 * `QA_ONLY_FILTER`, the name the caller asked for, never the names it got. The only symptom is the gap
 * between the banner and `clicks.jsonl`. This is tick 74's `--only=` that `run.sh` dropped, one layer
 * down and quieter: a filter that only works when spelled a particular way, and ignores the other
 * spelling silently.
 *
 * Three properties, each proven red against a mutated copy of the source:
 *
 *   1. the early exit **normalises**, so both spellings of every focused name reach it;
 *   2. every `DEPTH_PASSES` key is the de-hyphenation of a depth pass that actually runs, so the
 *      table and the call sites cannot drift apart (an entry that names a pass nobody calls is a
 *      filter that reports coverage for a screen that was never visited);
 *   3. `run.sh` advertises no spelling the walkthrough rejects.
 *
 * Run: `node scripts/qa/probe-only-filter.cjs` (exit 0 = pass, 1 = fail).
 */
const fs = require("fs");
const path = require("path");

const SRC = process.env.QA_WALKTHROUGH_SRC || path.join(__dirname, "walkthrough.cjs");
const IS_MUTANT = process.env.QA_ONLY_MUTANT === "1";

let pass = 0;
let fail = 0;
const failures = [];

function check(name, ok, detail = "") {
  if (ok) {
    pass++;
    console.log(`  ok ${pass} - ${name}`);
  } else {
    fail++;
    failures.push(`${name}${detail ? ` (${detail})` : ""}`);
    console.log(`  not ok ${fail} - ${name}${detail ? ` (${detail})` : ""}`);
  }
}

const src = fs.readFileSync(SRC, "utf8");
const deHyphenate = (name) => name.replace(/-/g, "");

// ---- 0. Read the two tables the filter is built from --------------------------------------------

const tableStart = src.indexOf("const DEPTH_PASSES = {");
const tableEnd = tableStart < 0 ? -1 : src.indexOf("\n};", tableStart);
check("the source declares a DEPTH_PASSES table", tableStart >= 0 && tableEnd > tableStart);
const tableBlock = tableEnd > tableStart ? src.slice(tableStart, tableEnd) : "";
const tableKeys = [...tableBlock.matchAll(/^\s{2}([A-Za-z][A-Za-z0-9]*):\s*\(/gm)].map((m) => m[1]);
check("the DEPTH_PASSES table has keys", tableKeys.length > 0, `found ${tableKeys.length}`);

/** Every depth pass registered at a call site, and whether that call site honours `wants()`. */
const callSites = (() => {
  const lines = src.split("\n");
  const out = [];
  for (const [i, line] of lines.entries()) {
    const m = line.match(/runDepthPass\(\s*"([^"]+)"\s*,\s*\(\)\s*=>\s*(\w+)\(/);
    if (!m) continue;
    // The name this call site registers in the roll-up's vocabulary. It is the file's own answer to
    // "what does `--only` call this pass", and it is a THIRD string in seven places
    // (`runDepthPass("analyticsDepth")` with `matchedOnly.add("analytics-depth")`).
    //
    // The window is the whole guard block, not a fixed line count: in some call sites
    // `matchedOnly.add` sits between the guard and the call, in others it sits above the guard. A
    // four-line window found the first kind and missed the second and reported seven working call
    // sites; a six-line window still missed them. The block is the unit, and the search for it is
    // "the nearest `matchedOnly.add` above this call", which is what the code actually means.
    const above = lines.slice(0, i).join("\n");
    const canonicalMatches = [...above.matchAll(/matchedOnly\.add\("([^"]+)"\)/g)];
    const canonical = canonicalMatches[canonicalMatches.length - 1];
    const guarded = lines.slice(Math.max(0, i - 6), i).join("\n").includes("wants(");
    out.push({
      name: m[1],
      fn: m[2],
      guarded,
      canonical: canonical ? canonical[1] : null,
      line: i + 1,
    });
  }
  return out;
})();
check("the file registers depth passes at call sites", callSites.length > 0, `found ${callSites.length}`);

// ---- 1. The early exit normalises -------------------------------------------------------------
//
// This is the shipped defect, stated as the smallest rule that catches it: a lookup that compares
// a caller-supplied hyphenated name against de-hyphenated keys without normalising cannot match.

const exitStart = src.indexOf("const only = (process.argv.find");
// The window has to hold the lookup AND the comment above it. 1400 was not enough once the fix
// landed: the comment is 20 lines of *why*, and a window that stops short of the code it exists
// to explain reports a defect that is not there. The end anchor is the exit the branch ends on —
// which is AFTER the summary write and the row echo, because the rules below check the echo too,
// and a window that stopped at the stack check would have reported the echo rule red on a
// source that contains it (which is what happened the first time this ran after the list fix).
const exitEnd = exitStart < 0 ? -1 : src.indexOf("process.exit(netFailures.length === 0 ? 0 : 1);", exitStart);
const exitBlock = exitStart < 0 ? "" : src.slice(exitStart, exitEnd > exitStart ? exitEnd : exitStart + 2600);
check("the focused depth-pass exit exists", exitStart >= 0);
check("the exit consults DEPTH_PASSES", /DEPTH_PASSES\[/.test(exitBlock));
check(
  "the exit normalises the requested name before looking it up",
  /replace\(\s*\/\-\/g\s*,\s*""\s*\)/.test(exitBlock) || /replaceAll\(\s*"-"\s*,\s*""\s*\)/.test(exitBlock),
  "the lookup compares a hyphenated name against de-hyphenated keys, so it can never match",
);
// **A comma filter is the same defect one level up, and normalising a scalar does not cover it.**
// `ONLY` is parsed as a LIST above and `run.sh` documents `--only=a,b`; resolving over the raw
// `--only=` value de-hyphenates `"workflow-table,workflow-builder"` into a string that is not a
// key, the branch is skipped and the pass walks the whole inventory while the banner names the
// filter. So the resolution has to iterate `ONLY`'s entries rather than de-hyphenate one string,
// and the rule has to read the LIST to keep saying so — a gate written against the scalar it
// happened to be shipped with is a gate that goes stale the moment the argument is fixed.
check(
  "the exit resolves the filter over ONLY's entries, not over the raw --only= string",
  /ONLY\.filter\(/.test(exitBlock) || /ONLY\.(map|forEach)\(/.test(exitBlock),
  "a `--only=a,b` filter de-hyphenates to a string that is not a key, so it matches nothing and the pass runs wide",
);
// And the step echo has to follow it: matching one name against `"workflow-table,workflow-builder"`
// finds nothing, so the rows the pass just produced are printed for no input the filter accepts.
check(
  "the focused pass echoes rows for every name it resolved, not one joined string",
  /onlyNames\.some\(/.test(exitBlock) || /some\(\(name\)\s*=>/.test(exitBlock),
  "a two-name filter echoes no rows at all, because the joined string is a substring of no page name",
);

// ---- 2. Every key names a pass that actually runs ------------------------------------------------
//
// Compared by FUNCTION, not by string, and that is the correction an earlier draft of this gate
// needed. Matching names produced eleven false "orphan key" findings, because the call sites use a
// third naming convention: `runDepthPass("analyticsDepth", …)`, `runDepthPass("iamRoles", …)`,
// `runDepthPass("search-depth", …)`. `analytics`, `iamroles` and `search` are therefore reachable
// three ways — by the route filter, by the table, or both — and a string comparison cannot tell a
// drifted key from a differently-spelled one.
//
// The function is the only identity that survives all three conventions, so it is what the rule
// compares. Two table entries naming the same function would mean one screen reachable under two
// names, which is a coverage claim nobody can verify, so that is asserted too.

const tableEntries = [...tableBlock.matchAll(/^\s{2}([A-Za-z][A-Za-z0-9]*):\s*\(page, report\)\s*=>\s*(\w+)\(/gm)].map(
  (m) => ({ key: m[1], fn: m[2] }),
);
check("every DEPTH_PASSES entry delegates to a named function", tableEntries.length === tableKeys.length);

{
  const calledFns = new Set(
    [...src.matchAll(/runDepthPass\(\s*"[^"]+"\s*,\s*\(\)\s*=>\s*(\w+)\(/g)].map((m) => m[1]),
  );
  const orphans = tableEntries.filter((e) => !calledFns.has(e.fn));
  check(
    "every DEPTH_PASSES key drives a pass the file actually runs",
    orphans.length === 0,
    `keys naming a pass no call site runs: ${orphans.map((o) => `${o.key}->${o.fn}`).join(", ")}`,
  );
  const dupeFn = tableEntries.filter((e, i) => tableEntries.findIndex((o) => o.fn === e.fn) !== i);
  check(
    "no two DEPTH_PASSES entries drive the same pass",
    dupeFn.length === 0,
    `duplicated: ${dupeFn.map((d) => d.fn).join(", ")}`,
  );
}

// ---- 3. Every UNGUARDED screen pass is reachable through the table --------------------------------
//
// A call site that ignores `wants()` runs in every pass, so those are exactly the ones that have to
// come through `DEPTH_PASSES` — that table is not a convenience index, it is the only way to focus
// them. An unguarded screen pass missing from the table is a screen a REQ close cannot measure without
// buying a full pass, which is the defect this tick's own build log assumed away.

const unguarded = callSites.filter((c) => !c.guarded).map((c) => c);
check("the file has unguarded depth passes to account for", unguarded.length > 0);
{
  // Two kinds of unguarded call site are NOT screens and must not be focusable:
  //   - `schedules` runs *inside* the backups pass, on a screen that pass already measures;
  //   - `before-crash` / `on-dead-tab` / `after-recovery` / `dead-again-*` are the wrapper's own
  //     self-tests, and `onboarding-wizard` runs before the filter is even consulted.
  const nonScreens = new Set([
    "schedules",
    "before-crash",
    "on-dead-tab",
    "after-recovery",
    "on-dead-browser",
    "dead-again-a",
    "dead-again-b",
    "dead-again-c",
    "onboarding-wizard",
  ]);
  const tableFns = new Set(tableEntries.map((e) => e.fn));
  const missing = unguarded
    .filter((c) => !nonScreens.has(c.name))
    .map((c) => c.fn)
    .filter((fn) => !tableFns.has(fn));
  check(
    "every unguarded screen pass is focusable through DEPTH_PASSES",
    missing.length === 0,
    `unfocusable: ${missing.join(", ")}`,
  );
}

// ---- 3b. A guard that covers several passes must OR in each pass's own name ----------------------
//
// The health defect was not "a guard is missing" — it was "one guard covers four passes", and every
// `runDepthPass` call site still sat inside an `if (wants(...))`. Nothing above could see it: the
// table and the call sites were both intact.
//
// **This rule is about guards that cover MORE THAN ONE pass, and it deliberately is not about the
// name matching.** An earlier draft required `wants("<its own name>")` and reported eleven
// legitimate cases as defects: `runDepthPass("analyticsDepth")` guarded by `wants("analytics-depth")`,
// `runDepthPass("iamRoles")` by `wants("iam-roles-depth")`. Those are ROUTE names and PASS names, two
// conventions that differ on purpose — a route is a URL segment, a pass is a walk. Requiring them to
// match would have demanded a rename of eleven working call sites to satisfy a rule I had invented.
//
// The invariant that is actually true, and the one worth holding: **a single-name guard may name
// something other than the pass** (that is the route naming), **but a guard that covers several
// passes cannot — because then each of those passes is only reachable through a filter that also
// pulls in the others.** `health-overview` covering four passes is the defect; `analytics-depth`
// covering one differently-named pass is not.

{
  const lines = src.split("\n");
  // Group call sites by the guard they sit under: same nearest-enclosing `if (wants(` line.
  //
  // **`callSites` itself, not a copy of it.** An earlier version rebuilt the array field by field and
  // silently dropped `canonical`, which is the name the rule reads — so every call site reported
  // `canonical: undefined` and the rule fell back to the pass name, failing seven working sites. A
  // copy of a parsed record that forgets one field is worse than no copy.
  const byGuard = new Map();
  for (const c of callSites) {
    if (!c.guarded) continue;
    let guardLine = -1;
    for (let i = c.line - 2; i >= 0 && i > c.line - 12; i--) {
      if (/^\s*if \(wants\(/.test(lines[i])) { guardLine = i; break; }
    }
    if (guardLine < 0) continue;
    const header = lines[guardLine];
    const g = header.match(/wants\("([^"]+)"\)/);
    if (!g) continue;
    const key = `${guardLine}:${g[1]}`;
    if (!byGuard.has(key)) byGuard.set(key, { guard: g[1], header, line: guardLine, passes: [] });
    byGuard.get(key).passes.push(c);
  }

  // **The rule, stated over the guard's whole condition rather than over how many passes sit
  // under it.** An earlier version grouped by "a guard covering more than one pass", and that was
  // wrong in a way that hid the shipped defect: collapsing `health-incidents` onto the
  // `health-overview` guard leaves it as the *only* pass under that guard, so the group shrinks to
  // one and the rule reports nothing. The shape that matters is not "how many passes share this
  // guard" but "can this pass be reached by its own name".
  //
  // So: for EVERY guarded call site, `--only=<its own name>` must make the guard true. That is the
  // whole invariant, it needs no grouping, and it is exactly the property the health defect broke.
  //
  // **The de-hyphenated spelling is also accepted**, because a route name and a pass name differ
  // only by that in every one of these cases: `wants("analytics-depth")` guarding
  // `runDepthPass("analyticsDepth")` is eight working call sites, not eight defects. A rename is
  // not a filter bug -- `--only=analytics-depth` reaches the pass exactly as intended. What would
  // be a bug is a spelling that reaches NEITHER form, and that is what the de-hyphenated fallback
  // guarantees.
  const unreachable = [];
  for (const [key, g] of byGuard) {
    for (const c of g.passes) {
      if (c.name === g.guard) continue; // its own name IS the guard; nothing to prove
      // **Four spellings are the same name**, and the set is not arbitrary -- it is exactly what the
      // file and `run.sh` actually use between them:
      //
      //   camelCase  `analyticsDepth`      the walkthrough's own call site
      //   kebab      `analytics-depth`     the route name
      //   flattened  `analyticsdepth`      `DEPTH_PASSES`'s key and the early-exit lookup
      //   `-depth`    `notifications-depth` the ROUTE name, which carries a suffix the pass drops
      //
      // The `-depth` suffix is the fourth convention and it is why `notifications` behind
      // `notifications-depth` is NOT a defect: the guard names the screen's route and the pass is
      // the walk over it, and the two have never been the same string. An earlier version of this
      // rule demanded an exact match and reported eight working call sites; the version before that
      // accepted only de-hyphenation and reported eight different ones. Matching on the flattened
      // form with an optional `-depth` suffix is what makes the rule match the file rather than my
      // idea of it.
      // **Two different questions, and the earlier rule conflated them.**
      //
      // *Can the pass be REACHED?* — yes, if any name it declares appears in the guard. Eight call
      // sites differ between the guard and the pass by case and hyphens (`analytics-depth` vs
      // `analyticsDepth`) and every one of them is reachable. This is not a bug and must not be
      // reported.
      //
      // *Can it be reached BY ITS OWN NAME?* — that is the property the health defect broke.
      // Collapsing `health-incidents` onto the `health-overview` guard leaves it reachable (the
      // roll-up still calls it `health-incidents`) but reachable only through a filter that also
      // runs the overview. So the rule is narrower than "is it reachable": **the guard's own
      // condition must mention the name the call site registers**, and `matchedOnly` alone does not
      // count — it is what the roll-up prints, not what the filter reads.
      // `\"` inside a template literal is a literal BACKSLASH followed by a quote, not an escaped
      // quote — so this regex was looking for `wants\("` in the source and never matched a single
      // one of the 31 guards. The bug is invisible from reading the line: `\\"` looks like a
      // correctly-escaped quote, and it is only wrong once the template is evaluated.
      //
      // The quotes are therefore escaped the way a regex needs them — `["]` — rather than by
      // doubling a backslash, so the template contains exactly the characters the pattern wants.
      const spelled = (n) => g.header.includes(`wants("${n}")`);
      // Every spelling the file and run.sh use for one name: as written, de-hyphenated, kebab,
      // and with the route-only `-depth` suffix.
      const spellings = (n) =>
        [n, deHyphenate(n), `${n}-depth`, `${deHyphenate(n)}-depth`].filter(Boolean);
      const guardReads = (n) => spellings(n).some(spelled);
      // Reachable through ANY of the three declared names — for the report, not the rule.
      const declared = [c.name, g.guard, c.canonical].filter(Boolean);
      const reachableBySomeName = declared.some(guardReads);
      // **The rule:** the name the call site ANSWERS to (`matchedOnly`, falling back to the pass
      // name) must be read by the guard itself. If it is not, `--only=<that name>` cannot select
      // this pass without selecting whatever else shares the guard.
      //
      // **The guard is read against the CANONICAL name, not the pass name.** This is the health
      // defect exactly: collapsing `health-incidents` onto `health-overview` leaves the pass
      // reachable — the roll-up still calls it `health-incidents` — but reachable only by asking for
      // the overview. `matchedOnly` is what the roll-up PRINTS; `wants()` is what the filter READS,
      // and the gap between them is the whole bug.
      const answeredAs = c.canonical ?? c.name;
      const selectableByOwnName = guardReads(answeredAs);
      if (!selectableByOwnName) {
        unreachable.push(
          `${c.name} answers to "${answeredAs}" but its guard reads only "${g.guard}"` +
            (reachableBySomeName ? " (reachable only as a side effect)" : ""),
        );
      }
    }
  }
  check(
    "a guarded pass is reachable by --only under its own or its de-hyphenated name",
    unreachable.length === 0,
    unreachable.join(", "),
  );

  // The multi-pass guards are reported by COUNT so a reader sees the shape rather than inferring it:
  // this is the only place in the file where one filter name covers more than the pass it names.
  const shared = [...byGuard.values()].filter((g) => g.passes.length > 1);
  check(
    "every guard covering several passes names each of them",
    shared.every((g) => g.passes.every((c) => c.name === g.guard || new RegExp(`wants\\("${c.name}"\\)`).test(g.header))),
    shared.map((g) => `${g.guard}(${g.passes.length})`).join(", "),
  );
}

// ---- 4. run.sh advertises only spellings the walkthrough accepts ---------------------------------

{
  const runSh = path.join(__dirname, "run.sh");
  const runSrc = fs.existsSync(runSh) ? fs.readFileSync(runSh, "utf8") : "";
  const advertised = [...new Set([...runSrc.matchAll(/--only=([a-z0-9-]+)/g)].map((m) => m[1]))];
  check("run.sh names at least one spelling", advertised.length > 0);
  const known = new Set([...callSites.map((c) => c.name), ...tableKeys]);
  const bad = advertised.filter((n) => n !== "all" && !known.has(n) && !known.has(deHyphenate(n)));
  check("run.sh advertises no spelling the walkthrough rejects", bad.length === 0, bad.join(", "));
}

// ---- 5. The banner reports what ran, not what was asked ------------------------------------------
//
// The reason four ticks read this as a capacity problem: `run.sh` prints the name the caller asked
// for. A banner that printed the names actually walked would have said `workflow-builder,
// automations-operations` while the caller believed it had a single focused pass, and the tick would
// have cost one glance instead of four.

{
  const runSh = path.join(__dirname, "run.sh");
  const runSrc = fs.existsSync(runSh) ? fs.readFileSync(runSh, "utf8") : "";
  const bannerLine = runSrc.split("\n").find((l) => l.includes("focused: $QA_ONLY_FILTER"));
  check("run.sh prints a focus banner", Boolean(bannerLine));
  check(
    "…and it is stated as the requested filter, not a coverage claim",
    Boolean(bannerLine) && /QA_ONLY_FILTER/.test(bannerLine),
  );
  // The walkthrough already logs the names it walked; the banner line above must not also claim
  // them. This rule pins that the walk's own `focused pass:` line exists, because that is the one
  // line that can contradict the banner.
  check(
    "the walkthrough logs the names it actually walked",
    /log\(`focused pass: \$\{walkedRoutes\.length\}\/\$\{routes\.length\} routes/.test(src),
  );
}

// ---- 6. Proven against MUTATED copies of the real source -----------------------------------------

if (!IS_MUTANT) {
  const realSrc = fs.readFileSync(SRC, "utf8");
  const mutations = [
    {
      name: "the shipped defect: the exit loses its normalisation",
      why: "`--only=workflow-table` matches no key and the pass walks the whole inventory",
      // Two replacements, because the shipped defect is the PAIR: a key that is not normalised and
      // a lookup that uses the un-normalised name. Patching only the declaration would leave the
      // rule green on a source whose lookup can still never match.
      //
      // Anchored on the RESOLUTION rather than on the old scalar, so this mutation keeps working
      // when the fix changes shape — a mutation that anchors on the code it was written beside is
      // a mutation that reports "this gate is stale" the moment that code is legitimately
      // rewritten, which is what happened the first time this rule ran after the list fix.
      apply: (s) =>
        s
          .replace(
            /ONLY\.filter\(\(name\) => Object\.prototype\.hasOwnProperty\.call\(DEPTH_PASSES, name\.replace\(\/-\/g, ""\)\)\)/,
            'ONLY.filter((name) => Object.prototype.hasOwnProperty.call(DEPTH_PASSES, name))',
          )
          // BOTH sites, because the resolution normalises twice — once to test membership and once
          // to build the key. Stripping only the lookup leaves the `.map` still calling
          // `.replace(/-/g, "")`, so the normalisation rule stays GREEN on a source whose lookup
          // can never match — a mutation that passes for the wrong reason, which is worse than a
          // mutation that does not apply at all.
          .replace(/key: name\.replace\(\/-\/g, ""\)/, "key: name"),
      expectRule: /exit normalises the requested name/,
    },
    {
      name: "a comma filter resolved as one string, so two names match nothing",
      why: "`--only=workflow-table,workflow-builder` walks the whole inventory while the banner names the filter",
      // The list half of the same defect, and it is the one a single-name mutation cannot reach:
      // normalisation is still present, it is simply applied to a JOINED string instead of to each
      // entry, so the key it builds is `"workflowtable,workflowbuilder"` and no lookup matches.
      apply: (s) =>
        s.replace(
          /const onlyResolved = ONLY\.filter[\s\S]*?\.map\(\(name\) => \(\{ asked: name, key: name\.replace\(\/-\/g, ""\) \}\)\);/,
          'const onlyResolved = [{ asked: only, key: only.replace(/-/g, "") }].filter((entry) => DEPTH_PASSES[entry.key]);',
        ),
      expectRule: /resolves the filter over ONLY's entries/,
    },
    {
      name: "rows echoed for the joined filter instead of per name",
      why: "a two-name filter prints no rows, because the joined string is a substring of no page name",
      apply: (s) =>
        s.replace(
          /report\.steps\.filter\(\(step\) => onlyNames\.some\(\(name\) => String\(step\.page \|\| ""\)\.includes\(name\)\)\)/,
          'report.steps.filter((step) => String(step.page || "").includes(only.replace(/Depth$/, "")))',
        ),
      expectRule: /echoes rows for every name it resolved/,
    },
    {
      name: "a key that names a pass nobody runs",
      why: "`--only=<it>` exits early, measures nothing and reports a pass",
      apply: (s) => s.replace(
        "  analytics: (page, report) => runAnalyticsDepth(page, report),",
        "  pluginpalette: (page, report) => runPluginPaletteDepth(page, report),",
      ),
      expectRule: /every DEPTH_PASSES key drives a pass the file actually runs/,
    },
    {
      name: "an unguarded screen pass dropped from the table",
      why: "the builder cannot be focused, so a REQ close has to buy a full pass",
      apply: (s) => s.replace(
        "  workflowbuilder: (page, report) => runWorkflowBuilderDepth(page, report),",
        "",
      ),
      expectRule: /every unguarded screen pass is focusable through DEPTH_PASSES/,
    },
    {
      name: "the health passes collapsed back onto one guard",
      why: "`--only=health-incidents` stops running the incidents pass and `--only=health-overview` runs three the caller did not ask for",
      // **Only the guard changes.** The shipped shape was four calls under one
      // `if (wants("health-overview")) {`, and the `matchedOnly.add` line inside each of them was the
      // guard's own name — so mutating both together (an earlier version of this did) reproduced a
      // source where the pass is *called* `health-overview`, which is a different defect and one this
      // rule is right not to flag.
      //
      // Dropping the OR alone is the honest mutation: the pass still answers to
      // `health-incidents` in the roll-up, so it still LOOKS focusable, while the filter can no
      // longer select it without also running the overview. That gap — printed name versus read
      // name — is precisely what made the shipped defect invisible.
      apply: (s) =>
        s.replace(
          'if (wants("health-overview") || wants("health-incidents")) {',
          'if (wants("health-overview")) {',
        ),
      expectRule: /a guarded pass is reachable by --only/,
    },
  ];

  const { execFileSync } = require("child_process");
  for (const mutation of mutations) {
    const mutated = mutation.apply(realSrc);
    if (mutated === realSrc) {
      check(`mutation "${mutation.name}" still applies to the source`, false, "apply() matched nothing — this gate is stale");
      continue;
    }
    const tmp = path.join(__dirname, `.mutant-${process.pid}.cjs`);
    fs.writeFileSync(tmp, mutated);
    let out = "";
    let code = 0;
    try {
      out = execFileSync("node", [__filename], {
        env: { ...process.env, QA_ONLY_MUTANT: "1", QA_WALKTHROUGH_SRC: tmp },
        encoding: "utf8",
        stdio: ["ignore", "pipe", "pipe"],
      });
    } catch (err) {
      code = err.status ?? 1;
      out = `${err.stdout ?? ""}${err.stderr ?? ""}`;
    } finally {
      fs.unlinkSync(tmp);
    }
    check(
      `the gate is RED on: ${mutation.name}`,
      code !== 0,
      `exit=${code} — the gate passed a source with the defect re-introduced`,
    );
    check(
      `…naming the rule that caught it (${mutation.why})`,
      new RegExp(`not ok \\d+ - .*${mutation.expectRule.source}`, "i").test(out),
      `no "not ok" line for ${mutation.expectRule.source}`,
    );
  }

  check("the gate is GREEN on the unmutated source", fail === 0, `${fail} rule(s) failed on the real file`);
}

// -------------------------------------------------------------------------------------------------

console.log(`\n${pass} passed, ${fail} failed${IS_MUTANT ? " (mutant run)" : ""}`);
if (fail) {
  console.log("\nA filter that silently runs wide is not a focused pass, it is a full pass with a lie in its banner:");
  for (const f of failures) console.log(`  - ${f}`);
  process.exit(1);
}
process.exit(0);
