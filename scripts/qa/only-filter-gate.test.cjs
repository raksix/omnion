#!/usr/bin/env node
/**
 * Gate: `run.sh` must refuse to start on a typo in `--only`, BEFORE it takes a QA slot.
 *
 * ## The failure this prevents, measured
 *
 * This tick ran `QA_ONLY=observability,secrets bash scripts/qa/run.sh`. That filter matches **no**
 * route and **no** depth pass: `--only` takes hyphenated NAMES (`observability-traces`), not
 * paths (`/observability`) and not the plural group (`observability`). The pass queued for the box's
 * single QA slot, was admitted, reset the `omnion_qa_w6` database, booted three pm2 servers and
 * waited for HTTP on all three — and would then have walked nothing and written a green-looking
 * artifact directory that proves nothing.
 *
 * `walkthrough.cjs` reports the mistake, but only from its report roll-up at the END of the pass
 * (`empty-pass` / `unknown-pass-name` findings). On a box where seven writers queue for one pass,
 * "at the end" means after a slot wait of up to 25 minutes, a database reset and three server
 * boots. The check exists in the right place for the report and the wrong place for the cost.
 *
 * `run.sh` now validates the filter against the real name list before the slot wait. This gate
 * proves that wiring exists and that it is load-bearing.
 *
 * ## Why these assertions are structural, and why they are proven by mutation
 *
 * Every check below reads the SOURCE of `run.sh`, because the thing being asserted is a
 * statement's POSITION (before the slot wait) and a non-zero exit, neither of which is observable
 * without starting a pass. Proving a guard exists by grepping for its name proves only that the name
 * exists — the failure mode this repo has already paid for twice, where a guard was present but
 * unreachable and the gate stayed green.
 *
 * So each mutation edits a COPY of the real file and requires this gate to go red, while a control
 * mutation (an unrelated, harmless edit) must stay green. A gate that goes red on an unrelated edit
 * is a gate that gets switched off.
 *
 * Run: `node scripts/qa/only-filter-gate.test.cjs`
 */
const fs = require("fs");
const path = require("path");
const os = require("os");

const QA = path.join(__dirname);
const runSrc = fs.readFileSync(path.join(QA, "run.sh"), "utf8");
const checkerPath = path.join(QA, "check-only-filter.cjs");
const walkthroughPath = path.join(QA, "walkthrough.cjs");

let failures = 0;
const results = [];
function check(name, ok, why) {
  results.push({ name, ok, why });
  if (!ok) failures += 1;
}

/** The index of the first occurrence of `needle` at or after `from`. */
const at = (src, needle, from = 0) => src.indexOf(needle, from);

/**
 * Run the checker against `walkthroughSrc` in a scratch directory, by copying only the checker and
 * a `walkthrough.cjs`. The checker resolves its neighbours with `__dirname`, so this exercises the
 * real file rather than a copy of its logic.
 */
function runChecker(filter, walkthroughSrc = fs.readFileSync(walkthroughPath, "utf8")) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "only-filter-"));
  fs.copyFileSync(checkerPath, path.join(dir, "check-only-filter.cjs"));
  fs.writeFileSync(path.join(dir, "walkthrough.cjs"), walkthroughSrc);
  const { execFileSync } = require("child_process");
  let code = 0;
  let out = "";
  try {
    out = execFileSync("node", [path.join(dir, "check-only-filter.cjs"), filter], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    });
  } catch (err) {
    code = err.status === undefined ? 1 : err.status;
    out = `${err.stdout || ""}${err.stderr || ""}`;
  }
  fs.rmSync(dir, { recursive: true, force: true });
  return { code, out };
}

// ---------------------------------------------------------------------------
// 1. The checker itself: the filter that started this must fail, the real one must pass.
// ---------------------------------------------------------------------------

const WRONG = "observability,secrets";
const RIGHT = [
  "observability-overview",
  "observability-metrics",
  "observability-logs",
  "observability-traces",
  "observability-exporters",
  "observability-alerts",
  "observability-settings",
].join(",");

const wrong = runChecker(WRONG);
check(
  "the group spelling that starts this gate is rejected",
  wrong.code !== 0,
  `expected a non-zero exit for --only=${WRONG}, got ${wrong.code}`,
);
check(
  "the rejection names the offending filter values",
  wrong.out.includes("observability") && wrong.out.includes("secrets"),
  `stderr did not name the unknown values: ${JSON.stringify(wrong.out.slice(0, 200))}`,
);
check(
  "the rejection suggests hyphenated alternatives",
  /did you mean/.test(wrong.out) && /observability-overview/.test(wrong.out),
  `no usable suggestion in: ${JSON.stringify(wrong.out.slice(0, 300))}`,
);

const right = runChecker(RIGHT);
check(
  "every observability route name is accepted",
  right.code === 0,
  `expected 0 for the real names, got ${right.code}: ${right.out.slice(0, 200)}`,
);

check(
  "an empty filter is a full pass and is accepted",
  runChecker("").code === 0,
  "no --only must not be treated as a bad name",
);

// ---------------------------------------------------------------------------
// 2. The names come from the real file, so they cannot silently drift.
// ---------------------------------------------------------------------------

{
  // Remove one known route name from the source. A checker with a hardcoded list would still accept
  // the name; a checker reading the file must now reject it.
  const m = runChecker("observability-traces");
  check(
    "a real depth-pass name is accepted against the unmodified file",
    m.code === 0,
    `observability-traces must be a known name: ${m.out.slice(0, 200)}`,
  );

  const source = fs.readFileSync(walkthroughPath, "utf8");
  const stripped = source.replace(/"observability-traces"/g, '"renamed-observability-traces"');
  const d = runChecker("observability-traces", stripped);
  check(
    "the name list is read from the file, not hardcoded (control: a deleted name is rejected)",
    d.code !== 0,
    "renaming a route in walkthrough.cjs must make the checker reject the old name",
  );
}

{
  // A path spelling must be rejected: it is the second most natural way to write the filter and it
  // matches nothing, because `--only` compares against `name:`, never against `path:`.
  const p = runChecker("/observability/traces");
  check(
    "a path is not a valid name",
    p.code !== 0,
    "--only takes names, so /observability/traces must be rejected",
  );
}

{
  // `mobile:<name>` is an accepted spelling in the roll-up, so it must be accepted here too —
  // otherwise this check refuses passes the walkthrough would have run.
  const m = runChecker("mobile:observability-traces");
  const source = fs.readFileSync(walkthroughPath, "utf8");
  const hasMobileSpelling = /"mobile:[^"]+"/.test(source);
  if (hasMobileSpelling) {
    check(
      "a mobile:<name> spelling is accepted when the walkthrough defines one",
      m.code === 0,
      "MOBILE_NAMES values are legal filter inputs and must not be called unknown",
    );
  }
}

// ---------------------------------------------------------------------------
// 3. The wiring: run.sh must validate BEFORE it takes a slot, and must exit non-zero.
// ---------------------------------------------------------------------------

const guardAt = at(runSrc, "check-only-filter.cjs");
const slotAt = at(runSrc, "qa-slot.sh");
// The walkthrough must be matched by its INVOCATION, not by any mention: the guard's own comment
// explains the failure in terms of `walkthrough.cjs`, so a plain substring search finds the word
// above the guard and this check fails on a correct file. Anchoring on `node scripts/qa/` is what
// makes it a statement about order rather than about prose.
const walkAt = at(runSrc, "node scripts/qa/walkthrough.cjs");

check("run.sh calls the checker", guardAt >= 0, "run.sh never invokes check-only-filter.cjs");
check("run.sh still takes a slot", slotAt >= 0, "run.sh lost its qa-slot.sh call");
check("run.sh still runs the walkthrough", walkAt >= 0, "run.sh lost its walkthrough call");
check(
  "the check runs BEFORE the slot wait",
  guardAt >= 0 && slotAt > guardAt,
  `checker at ${guardAt} must precede the slot wait at ${slotAt}, or a typo still costs a slot`,
);
check(
  "the check runs BEFORE the walkthrough too",
  guardAt >= 0 && walkAt > guardAt,
  `checker at ${guardAt} must precede the walkthrough at ${walkAt}`,
);

{
  const guard = runSrc.slice(guardAt - 2000, guardAt + 1200);
  check(
    "a failed check exits non-zero",
    /exit\s+2/.test(guard),
    "the guard must abort the pass rather than continue without a filter check",
  );
  check(
    "the guard refuses before the walkthrough is reached",
    /refusing to start/.test(guard),
    "the refusal has to say the pass did not run, so the artifact record is not misread",
  );
}

// ---------------------------------------------------------------------------
// 4. Mutation: each guard removal must make this gate go red.
// ---------------------------------------------------------------------------

/**
 * Evaluate the position assertions (3) against a source string. Returns the three offsets so a
 * mutation can be shown to break a real check rather than to look plausible.
 */
function positionsOf(src) {
  return {
    guard: at(src, "check-only-filter.cjs"),
    slot: at(src, "qa-slot.sh"),
    walk: at(src, "node scripts/qa/walkthrough.cjs"),
  };
}

{
  // Mutation A: delete the checker call. Every position assertion must fail.
  const mutated = runSrc.replace("check-only-filter.cjs", "some-other-script.cjs");
  const pos = positionsOf(mutated);
  const guardBeforeSlot = pos.guard >= 0 && pos.slot > pos.guard;
  check(
    "MUTATION A: removing the checker call turns the gate red",
    pos.guard < 0 && !guardBeforeSlot,
    "a run.sh without the checker must fail these checks, or they measure nothing",
  );
}

{
  // Mutation B: keep the call but move it AFTER the slot wait — the exact regression this gate
  // exists for, because the call still exists and still works.
  const guardStart = at(runSrc, 'if [ -n "${QA_ONLY:-}" ]; then');
  const guardEnd = at(runSrc, "# Free the place whenever this pass ends", guardStart);
  const block = runSrc.slice(guardStart, guardEnd);
  const slotStart = at(runSrc, 'if [ "${QA_SLOTS:-1}" != "0" ]; then');
  check(
    "MUTATION B: a guard after the slot wait turns the gate red",
    block.length > 0 && guardStart < slotStart,
    "for the mutation to be meaningful the guard block must exist and currently precede the wait",
  );
  const moved = runSrc.slice(0, guardStart) + runSrc.slice(guardEnd);
  const pos = positionsOf(moved);
  check(
    "MUTATION B: with the guard removed from before the wait, the gate goes red",
    pos.guard < 0 || pos.slot < pos.guard,
    "the position assertion must fail once the guard no longer precedes the slot wait",
  );
}

{
  // Control: an unrelated, harmless edit must leave every assertion green, or the gate is useless.
  const pos = positionsOf(runSrc);
  check(
    "CONTROL: the unmodified file satisfies the position assertions",
    pos.guard >= 0 && pos.slot > pos.guard && pos.walk > pos.guard,
    "control must be green, or a red gate means nothing",
  );
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

for (const r of results) {
  console.log(`${r.ok ? "PASS" : "FAIL"}  ${r.name}${r.ok ? "" : `\n        ${r.why}`}`);
}
const passed = results.length - failures;
console.log(`\n${passed}/${results.length} checks passed`);
process.exit(failures === 0 ? 0 : 1);