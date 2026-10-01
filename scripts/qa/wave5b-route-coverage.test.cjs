#!/usr/bin/env node
/**
 * Gate: the wave-5b screens a merge must not be able to delete silently.
 *
 * The failure this prevents is measured, not hypothetical. Merge `8c6ab11d` resolved a conflict in
 * `walkthrough.cjs` by taking `origin/main`'s copy of the route list, and the merge diff shows all
 * thirteen wave-5b entries — six `/secrets` paths and all seven `/observability` paths — as `-`
 * deletions. Nothing failed. The file parsed, the depth passes still existed, and the only symptom
 * was that the screens stopped being measured, which is the one thing a walkthrough exists to do.
 *
 * A depth pass and a route entry are DIFFERENT lists, and that is what hid this. `c0b3a58b` wrote
 * the metric screen's depth pass and its route entry in the same commit but no `wants()` block, so
 * `--only=observability-metrics` answered `unknown-pass-name` while the route list already named
 * the screen. After the merge both directions were broken at once for the seven observability
 * screens: no route entry to measure layout, no reachable depth pass to drive behaviour.
 *
 * ## Why these assertions are written the way they are
 *
 * The first version of this gate stripped comments with `src.replace(/\/\*[\s\S]*?\*\//g, "")` and
 * then tested for `async function <name>(`. It reported the orphan check green on a file where the
 * orphan check had been reproduced exactly — because the stripper matched a `/*` inside a string
 * somewhere in `walkthrough.cjs`, opened a "comment" that ran for four kilobytes, and deleted the
 * metric pass's definition along with a dozen other declarations. Every depth pass therefore read
 * as "not defined in this file", the `if (!defined) return false` short-circuit skipped all of
 * them, and the check passed on nothing. The mutation harness then printed a green verdict for a
 * mutation it had failed to catch, because its own failure list was spliced back into place
 * before the summary read it.
 *
 * Two rules follow, and both are load-bearing:
 *   - **Never strip comments to assert on code.** A regex stripper cannot tell a `/*` in a comment
 *     from one in a string, and getting it wrong does not error — it silently deletes the very
 *     declarations under test. Every anchor below is structural instead: line-anchored regexes over
 *     the `const routes = [` block, and `^\s*name(` at the start of a statement, which a `//`
 *     comment line and a ` * ` docblock line can never both match.
 *   - **A mutation that does not go red fails the run, loudly and separately.** The mutation count
 *     is its own accumulator, so a harness bug cannot cancel itself out of the summary.
 *
 * Run: `node scripts/qa/wave5b-route-coverage.test.cjs`
 */
const fs = require("fs");
const path = require("path");

const WALKTHROUGH = path.join(__dirname, "walkthrough.cjs");

/** The screens merge `8c6ab11d` deleted. Paths are concatenated so this file's own docblock cannot
 *  satisfy a route assertion by naming them — see the note above. */
const SECRETS = ["root-key", "credentials", "slots", "leases", "deploy-keys", "audit"].map(
  (s) => "/sec" + "rets/" + s,
);
const OBSERVABILITY = ["", "/metrics", "/logs", "/traces", "/exporters", "/alerts", "/settings"].map(
  (s) => "/observ" + "ability" + s,
);
const WAVE5B_SCREENS = [...SECRETS, ...OBSERVABILITY];

/** Depth passes this writer's wave owns. A pass that stops being invoked is a failure, not dead code. */
const WAVE5B_DEPTH_PASSES = [
  "runObservabilityOverviewDepth",
  "runObservabilityMetricsDepth",
  "runObservabilityTracesDepth",
  "runObservabilityLogsDepth",
  "runObservabilityExportersDepth",
  "runObservabilityAlertsDepth",
  "runObservabilitySettingsDepth",
  "runSecretsRootKeyDepth",
  "runSecretsCredentialsDepth",
  "runSecretsLeasesDepth",
  "runSecretsAuditDepth",
  "runReliabilityRetriesDepth",
  "runReliabilityBreakersDepth",
  "runReliabilityIntakeDepth",
];

/**
 * `true` when a line inside the desktop `routes` array declares this path.
 *
 * Scoped to that array and line-anchored. A whole-file `includes()` would be satisfied by the mobile
 * list or by prose — the earlier `srcHasRoute` defect recorded in `walkthrough.cjs` itself — and a
 * substring test would be satisfied by a comment line. A real entry is `{ path: …` as the first
 * non-space token of a line, which is what a `//` comment and a `*` docblock line both fail.
 */
function desktopRoutes(src) {
  const block = src.match(/const routes = \[[\s\S]*?\n {2}\];/);
  if (!block) throw new Error("the desktop route list could not be read from walkthrough.cjs");
  const declared = new Set();
  for (const line of block[0].split("\n")) {
    const m = line.match(/^\s*\{\s*path:\s*"([^"]+)"/);
    if (m) declared.add(m[1]);
  }
  return declared;
}

function mobileRoutes(src) {
  const block = src.match(/const mobileRoutes = \[[\s\S]*?\];/);
  if (!block) throw new Error("the mobile route list could not be read from walkthrough.cjs");
  const declared = new Set();
  for (const m of block[0].matchAll(/\{\s*path:\s*"([^"]+)"/g)) declared.add(m[1]);
  return declared;
}

/** `true` when `name` is DEFINED as a top-level function — line-anchored, so a docblock mention
 *  reading `` `runObservabilityMetricsDepth(` `` inside backticks never matches. */
function isDefined(src, name) {
  return new RegExp(`^async function ${name}\\s*\\(`, "m").test(src) ||
    new RegExp(`^function ${name}\\s*\\(`, "m").test(src);
}

/** The `wants()` block bodies, each as an array of its statement lines. */
function wantsBlocks(src) {
  const bodies = [];
  const re = /^[ \t]*if \(wants\("([a-z0-9-]+)"\)\) \{$([\s\S]*?)^ {2}\}$/gm;
  for (const m of src.matchAll(re)) {
    bodies.push({
      name: m[1],
      lines: m[2].split("\n"),
    });
  }
  return bodies;
}

/** `true` when `name` is CALLED as a statement inside some `wants()` block.
 *
 *  An optional leading `await` is allowed, and it has to be: `observability-traces` wraps TWO passes
 *  in one `runDepthPass(…, async () => { await a(); await b(); })` sequence, so those calls sit on
 *  an `await` and not at the start of the line. A first version anchored on `^\s*name(` and reported
 *  the two as orphaned on the UNMODIFIED file — a gate that is red on the tree it was written for is
 *  a gate nobody runs, and the honest reading of that failure is "my anchor is wrong", not "the code
 *  is wrong". An `await` prefix cannot be produced by a `//` comment line, which still fails the
 *  anchor, so comment-proofness survives the loosening. */
function isInvoked(src, name) {
  const call = new RegExp(`^[ \\t]*(?:await\\s+)?${name}\\s*\\(`);
  return wantsBlocks(src).some((b) => b.lines.some((line) => call.test(line)));
}

/**
 * Run the checks against a source string.
 *
 * Returns its OWN failure list. The caller decides what to do with it, and the mutation harness
 * uses this return value rather than a shared mutable — the first version spliced a shared array
 * back into place between mutations and its own summary then reported the mutations it had failed
 * to catch as caught.
 */
function runChecks(src, label) {
  const out = [];
  const say = (name, ok, detail) => {
    console.log(`  ${ok ? "ok  " : "FAIL"} ${name}${!ok && detail ? ` — ${detail}` : ""}`);
    if (!ok) out.push(name);
  };
  console.log(`\n${label}`);

  const desktop = desktopRoutes(src);
  const missingDesktop = WAVE5B_SCREENS.filter((p) => !desktop.has(p));
  say(
    "all thirteen wave-5b screens are in the desktop route list",
    missingDesktop.length === 0,
    missingDesktop.length ? `missing: ${missingDesktop.join(", ")}` : undefined,
  );

  const mobile = mobileRoutes(src);
  const missingMobile = WAVE5B_SCREENS.filter((p) => !mobile.has(p));
  say(
    "all thirteen are in the 390px mobile route list",
    missingMobile.length === 0,
    missingMobile.length ? `missing: ${missingMobile.join(", ")}` : undefined,
  );

  say("assertWave5bScreensWalked is defined", /^function assertWave5bScreensWalked\s*\(/m.test(src));

  const mainBody = (src.match(/^async function main\(\) \{[\s\S]*?^\}$/m) || [""])[0];
  say(
    "main() calls assertWave5bScreensWalked",
    /^\s*assertWave5bScreensWalked\(\);/m.test(mainBody),
  );

  // The guard's own body has to actually TEST the list. Without this, a guard that is defined,
  // called, and empty passes every other check in this file — which is the "documented but
  // unreachable" shape this gate exists to catch, one level up: `assertDeploymentScreensWalked`
  // learned it in REQ-128 when a notes check shipped that nothing read. Two mutations reproduce it
  // here and both had to go green before this check was added: emptying the body, and deleting one
  // of the thirteen paths from its list so a genuinely missing route is no longer named.
  //
  // The paths are read from the guard's OWN body only — its array literal and the `WAVE5B_SCREENS`
  // const it filters. A first version also swept every `"…"` line in the file, and the
  // "shortened list" mutation then stayed GREEN because the deleted path was still named in the
  // `WAVE5B_SCREENS` array a few lines above. Two lists, one subject: the check has to read the one
  // the guard filters, or it reads the other and agrees with itself.
  const guardRegion =
    (src.match(/^const WAVE5B_SCREENS = \[[\s\S]*?^\];/m) || [""])[0] +
    (src.match(/^function assertWave5bScreensWalked\(\) \{([\s\S]*?)^\}$/m) || [, ""])[0];
  const guardBody =
    (src.match(/^function assertWave5bScreensWalked\(\) \{([\s\S]*?)^\}$/m) || [, ""])[1];
  const guardNames = new Set(
    [...guardRegion.matchAll(/^\s*"([^"]+)",$/gm)].map((m) => m[1]),
  );
  const unguarded = WAVE5B_SCREENS.filter((p) => !guardNames.has(p));
  say(
    "the guard's own list names all thirteen screens it protects",
    guardBody.length > 0 && unguarded.length === 0,
    unguarded.length
      ? `not named in the guard's list: ${unguarded.join(", ")}`
      : guardBody.length === 0
        ? "the guard body is empty — it is defined and called and tests nothing"
        : undefined,
  );
  say(
    "the guard refuses rather than warns",
    /throw new Error\(\s*`?wave-5b screens missing/.test(guardBody) ||
      /wave-5b screens missing from the route list/.test(guardBody),
  );

  // The orphan check. Every pass listed must be BOTH defined and invoked — a name that is not
  // defined here belongs to another writer and is not this gate's business, and saying so
  // explicitly is what stops a rename from reading as a pass.
  const present = WAVE5B_DEPTH_PASSES.filter((fn) => isDefined(src, fn));
  const orphans = present.filter((fn) => !isInvoked(src, fn));
  say(
    "no wave-5b depth pass is orphaned",
    orphans.length === 0,
    orphans.length ? `defined but never invoked: ${orphans.join(", ")}` : undefined,
  );
  // The gate is itself only meaningful if it is looking at something: `present.length` must equal
  // the list, or a mass rename would make every orphan check pass by definition.
  say(
    `all ${WAVE5B_DEPTH_PASSES.length} wave-5b depth passes are found in the file`,
    present.length === WAVE5B_DEPTH_PASSES.length,
    present.length === WAVE5B_DEPTH_PASSES.length
      ? undefined
      : `only ${present.length} found; absent: ${WAVE5B_DEPTH_PASSES.filter((fn) => !present.includes(fn)).join(", ")}`,
  );

  return out;
}

// ---------------------------------------------------------------------------------- the real run
const original = fs.readFileSync(WALKTHROUGH, "utf8");
const real = runChecks(original, "unmutated walkthrough.cjs");

// ---------------------------------------------------------------------------------- mutations
// Each must go red. A gate that survives its own mutation is not a gate, so a surviving mutation is
// reported as a failure of the GATE and not of the source.
const mutationFailures = [];

/**
 * Apply a mutation and report whether the gate went red.
 *
 * `expectRed` is explicit because a mutation that is NOT a defect must stay green: a gate that
 * fails on an unrelated route being added is a gate that gets switched off. The control below is
 * that case, and it is what stops "every mutation went red" from being a vacuous claim.
 */
function mutate(name, fn, expectRed = true) {
  let mutated;
  try {
    mutated = fn(original);
  } catch (err) {
    console.log(`\nmutation ${name}\n  FAIL the mutation threw before the gate could read it: ${err}`);
    mutationFailures.push(name);
    return;
  }
  if (mutated === original) {
    console.log(`\nmutation ${name}\n  FAIL did not apply — it matched nothing in the file`);
    mutationFailures.push(name);
    return;
  }
  let wentRed = false;
  try {
    wentRed = runChecks(mutated, `mutation ${name}`).length > 0;
  } catch (err) {
    wentRed = true;
    console.log(`\nmutation ${name}\n  (the gate refused to read the mutated file: ${err.message})`);
  }
  const asExpected = wentRed === expectRed;
  console.log(
    `\nmutation ${name}\n  ${asExpected ? "ok  " : "FAIL"} went ${wentRed ? "red" : "green"}, expected ${expectRed ? "red" : "green"}`,
  );
  if (!asExpected) mutationFailures.push(name);
}

// Exactly what merge 8c6ab11d did to this file: one route entry, deleted.
mutate("one wave-5b route entry deleted", (src) =>
  src.replace(/^\s*\{ path: "\/observability\/logs", name: "observability-logs" \},\n/m, ""),
);

// A route entry moved OUT of the routes array — e.g. relocated into the mobile list by a merge.
mutate("a wave-5b route entry demoted to mobile-only", (src) =>
  src.replace(/^\s*\{ path: "\/secrets\/slots", name: "secrets-slots" \},\n/m, ""),
);

// The guard's call removed, function left defined — what an unused-looking cleanup does.
mutate("the guard call removed from main()", (src) =>
  src.replace(/^\s*assertWave5bScreensWalked\(\);\n/m, ""),
);

// The guard body emptied, which is what a lint that flags an unused-looking helper pushes toward.
mutate("the guard neutered to an empty body", (src) =>
  src.replace(/function assertWave5bScreensWalked\(\) \{[\s\S]*?\n\}/, "function assertWave5bScreensWalked() {\n}"),
);

// The guard's list shortened — the "just tidy the list" edit that deletes a real screen.
mutate("the guard list shortened to hide a missing screen", (src) =>
  src.replace(/^\s*"\/observability\/settings",\n/m, ""),
);

// The tick-58 orphan restored: defined and exported, invoked by nothing.
mutate("the metric depth pass orphaned again", (src) =>
  src.replace(/^ {2}if \(wants\("observability-metrics"\)\) \{[\s\S]*?^ {2}\}$/m, ""),
);

// A screen dropped from the phone pass only.
mutate("one wave-5b screen dropped from the mobile list", (src) =>
  src.replace(/\{ path: "\/secrets\/audit", name: "secrets-audit" \}, /, ""),
);

// A control: a change that is genuinely NOT a defect must stay green, or the gate is noise.
mutate(
  "control — an unrelated route added",
  (src) =>
    src.replace(/^\s*\{ path: "\/health\/settings", name: "health-settings" \},$/m,
      '$&\n    { path: "/control/only", name: "control-only" },'),
  false,
);

const total = real.length + mutationFailures.length;
if (total === 0) {
  console.log(
    `\nPASS wave-5b route coverage: 8/8 checks, 6/6 defect mutations caught, 1/1 control stayed green`,
  );
} else {
  console.log(`\nFAIL ${total}: checks [${real.join(", ") || "none"}] mutations [${mutationFailures.join(", ") || "none"}]`);
}
process.exit(total === 0 ? 0 : 1);