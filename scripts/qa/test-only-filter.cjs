/**
 * Prove the `--only` filter actually selects — and COMPOSES.
 *
 * Every depth pass in `walkthrough.cjs` is reached through one of fourteen entry points that
 * ended in `process.exit`, and every one of them was gated on a literal argv token. Three
 * separate defects made `--only=a,b` — the exact form `run.sh` emits — silently walk the entire
 * box instead of the two passes asked for, write no `summary.json`, and report nothing. The
 * symptom in every case was a pass that "took too long", which is why three ticks of REQ-063
 * blamed scheduling while the filter was broken.
 *
 * This file exercises the real helpers from `walkthrough.cjs` — not a reimplementation of them,
 * which would pass while the real ones stayed broken. The module is loaded for its parser and
 * its two scoped-pass helpers with the browser stubbed, and three properties are asserted:
 *
 *   1. BOTH argv spellings select. `--only=x` and `--only x` reach the same list; only the first
 *      used to be read, which is the whole defect.
 *   2. A comma list enters EVERY named pass, not the first one.
 *   3. The summary of two passes is MERGED, and an early red survives into the final exit code.
 *      A pass that overwrote the summary, or exited green because the LAST module was green,
 *      would both read as "the pass ran" — and would have hidden REQ-063's acceptance 17 a fourth
 *      time.
 */
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

let failures = 0;
function check(name, cond, detail) {
  if (cond) {
    console.log(`  ok  ${name}`);
  } else {
    failures++;
    console.log(`  FAIL ${name}${detail ? ` — ${detail}` : ""}`);
  }
}

// A throwaway OUT directory, so this test never reads or writes a real pass's report.
const OUT = fs.mkdtempSync(path.join(os.tmpdir(), "omnion-only-filter-"));

/**
 * Load the REAL helpers out of walkthrough.cjs.
 *
 * The file is a top-level script: importing it would launch a browser. So the three declarations
 * are read from its source by name and evaluated on their own. Reading the source rather than
 * copying it is the point — a copy would keep passing if the original regressed, which is
 * exactly the failure this file exists to catch.
 */
const SRC = path.join(__dirname, "walkthrough.cjs");
const source = fs.readFileSync(SRC, "utf8");

function extract(name) {
  const fn = source.match(new RegExp(`^function ${name}\\([\\s\\S]*?^}`, "m"));
  if (fn) return fn[0];
  const arrow = source.match(new RegExp(`^const ${name} = [^\\n]*`, "m"));
  if (arrow) return arrow[0];
  const decl = source.match(new RegExp(`^const ${name} = \\([\\s\\S]*?^};$`, "m"));
  if (decl) return decl[0];
  throw new Error(`walkthrough.cjs no longer declares ${name} — the filter test must be updated`);
}

const argSrc = extract("arg");
const finishSrc = extract("finishScopedPass");
const exitSrc = extract("exitScoped");
// `exitScoped` closes over this counter, so it has to travel with the function — a harness that
// silently dropped it would report a ReferenceError as a harness fault and hide the real test.
const failuresSrc = source.match(/^let SCOPED_FAILURES = \d+;$/m)[0];
const onlyEntrySrc = source.match(/^const onlyEntry = [^\n]*/m)[0];
const isLastSrc = source.match(/^const isLastEntry = [^\n]*/m)[0];

const factory = new Function(
  "process",
  "path",
  "fs",
  "OUT",
  "browser",
  "exits",
  [
    argSrc,
    "const ONLY = (arg('only', 'all') || 'all').split(',').map((s) => s.trim()).filter(Boolean);",
    onlyEntrySrc,
    isLastSrc,
    failuresSrc,
    exitSrc,
    finishSrc,
    "return { ONLY, onlyEntry, isLastEntry, exitScoped, finishScopedPass };",
  ].join("\n"),
);

/** Build the helpers against a given argv, and capture process.exit instead of ending. */
function load(argv) {
  const exits = [];
  const browser = { close: () => Promise.resolve() };
  // The real `process.exit` never returns, and everything below depends on that: after
  // `exitScoped` ends the run there must be no second `exit` recorded. A stub that merely
  // recorded the code and fell through would report "the process exited twice" for a function
  // that is in fact correct — so the stub throws, and the throw is swallowed only where a test
  // deliberately keeps calling past an exit.
  const fake = {
    argv,
    exit: (c) => {
      exits.push(c);
      throw new Error("__EXIT__");
    },
  };
  const call = (fn, ...a) => {
    try {
      return fn(...a);
    } catch (e) {
      if (e.message !== "__EXIT__") throw e;
      return undefined;
    }
  };
  const api = factory(fake, path, fs, OUT, browser, exits);
  return {
    exits,
    ONLY: api.ONLY,
    onlyEntry: api.onlyEntry,
    isLastEntry: api.isLastEntry,
    // Wrappers so a test can step past an intentional exit without catching its own bugs.
    exitScoped: (name, code = 0) => call(api.exitScoped, name, code),
    finishScopedPass: (name, json, code = 0) => call(api.finishScopedPass, name, json, code),
  };
}

// ---------------------------------------------------------------- 1. both spellings
console.log("1. both argv spellings select the same passes");
{
  const a = load(["node", "w", "--only=block-editor,members"]);
  const b = load(["node", "w", "--only", "block-editor,members"]);
  check("`--only=a,b` parses to [a,b]", JSON.stringify(a.ONLY) === '["block-editor","members"]', JSON.stringify(a.ONLY));
  check("`--only a,b` parses to [a,b]", JSON.stringify(b.ONLY) === '["block-editor","members"]', JSON.stringify(b.ONLY));
  check("equality spelling enters block-editor", a.onlyEntry("block-editor") === true);
  check("equality spelling enters members", a.onlyEntry("members") === true);
  check("space spelling enters block-editor", b.onlyEntry("block-editor") === true);
  check("an unrequested pass is not entered", a.onlyEntry("menus") === false);
}

// ---------------------------------------------------------------- 2. both entry, not just the first
console.log("\n2. a comma list reaches EVERY named pass");
{
  const h = load(["node", "w", "--only=block-editor,members"]);
  check("block-editor entered", h.onlyEntry("block-editor"));
  check("members entered", h.onlyEntry("members"));
  check("block-editor is not last", h.isLastEntry("block-editor") === false);
  check("members IS last", h.isLastEntry("members") === true);
}

// ---------------------------------------------------------------- 3. the summary merges and a red survives
console.log("\n3. two passes merge their summaries and the red survives");
{
  const h = load(["node", "w", "--only=block-editor,members"]);
  const { exits } = h;
  const sumFile = path.join(OUT, "summary.json");

  h.finishScopedPass("block-editor", JSON.stringify({ total: 40, passed: 40, missing: [], steps: { a: true } }));
  check("the early pass did NOT end the process", exits.length === 0, `exits=${JSON.stringify(exits)}`);
  h.exitScoped("block-editor", 0);
  check("a clean early exit still waits its turn", exits.length === 0, `exits=${JSON.stringify(exits)}`);

  const afterFirst = JSON.parse(fs.readFileSync(sumFile, "utf8"));
  check("first pass wrote its summary", Array.isArray(afterFirst.scoped) && afterFirst.scoped[0] === "block-editor");
  check("a single pass is not silently green on total", afterFirst.total === 40);

  h.finishScopedPass("members", JSON.stringify({ total: 57, passed: 56, missing: ["noHorizontalScrollAt390"], steps: { b: true } }));
  h.exitScoped("members", 0);

  const merged = JSON.parse(fs.readFileSync(sumFile, "utf8"));
  check("both passes are listed as scoped", JSON.stringify(merged.scoped) === '["block-editor","members"]', JSON.stringify(merged.scoped));
  check("the SECOND pass did not delete the first", merged.stepsByPass["block-editor"].a === true && merged.stepsByPass.members.b === true, JSON.stringify(Object.keys(merged.stepsByPass)));
  check("counts accumulate across passes", merged.total === 97 && merged.passed === 96, `total=${merged.total} passed=${merged.passed}`);
  check("missing is the CONCATENATION, not an overwrite", JSON.stringify(merged.missing) === '["noHorizontalScrollAt390"]', JSON.stringify(merged.missing));
  check("the process ended exactly once, on the last pass", exits.length === 1 && exits[0] === 0, `exits=${JSON.stringify(exits)}`);
}

console.log("\n4. an early FAILURE still ends the process, and its code survives");
{
  const h = load(["node", "w", "--only=block-editor,members"]);
  const { exits } = h;
  h.exitScoped("block-editor", 4);
  check("a red early pass ends the run even mid-list", exits.length === 1 && exits[0] === 4, `exits=${JSON.stringify(exits)}`);
}
{
  // Order swapped, so the LAST name is `block-editor` and it is the one that ends the run.
  const h = load(["node", "w", "--only=members,block-editor"]);
  const { exits } = h;
  h.exitScoped("members", 0);
  check("a clean non-last pass does not end the run", exits.length === 0, `exits=${JSON.stringify(exits)}`);
  h.exitScoped("block-editor", 4);
  check("the last pass ends the run", exits.length === 1, `exits=${JSON.stringify(exits)}`);
  check("and it carries the failure code", exits[0] === 4, `exits=${JSON.stringify(exits)}`);
}

console.log(`\n${failures === 0 ? "PASS" : `FAIL (${failures})`} — ${SRC}`);
fs.rmSync(OUT, { recursive: true, force: true });
process.exit(failures === 0 ? 0 : 1);