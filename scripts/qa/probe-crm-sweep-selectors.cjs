#!/usr/bin/env node
/**
 * Every literal selector the CRM sweep legs query must exist in the module.
 *
 * **What this gate is for.** `runCrmStateSweep` and `runCrmKeyboardAndMobile` cannot be measured
 * slot-free — they press keys and stub the network against a served stack. So the one thing that
 * can be checked while the QA slot is held by a sibling is whether they are *able* to pass at all.
 *
 * **The defect it found (2026-10-02, tick 78).** The retry leg's final assertion read
 *
 *     [data-qa='crm-row'], [data-qa='crm-contacts-empty']
 *
 * and **neither marker exists anywhere in the module**. The contacts table draws rows through
 * `CrmRow`, which emits `data-qa-crm-cursor` and no `data-qa` at all; `EmptyState` renders a bare
 * `<div>` with no hook whatsoever. The clause was therefore permanently `0 > 0`, so
 * `theRetryRecoversTheScreen` could only ever be `false` — a **working** retry reported as broken,
 * on the single step whose entire purpose is to prove a retry is wired to something rather than
 * decorative. Had the slot been free, this tick would have burned a 25-minute pass to produce a
 * false defect.
 *
 * A permanent `false` is the most expensive shape a harness bug can take: it looks like evidence,
 * and the reader has to spend a full pass to disprove it. This gate makes that shape impossible to
 * ship, which is the only reason a queued leg is not just a queued leg.
 *
 * **Scope, honestly stated.** It checks *literal* `[data-qa='…']` selectors — the ones a screen can
 * pass a fixed string to. A selector built from a variable (`[data-qa='${screen.qa}']`) is resolved
 * against the module by the sweep itself and is not this gate's business; checking those would mean
 * re-implementing the sweep's own table, and a gate that re-implements its subject is a second
 * subject. What it does check is the class that bit us: a name written by hand, matching nothing.
 */
const fs = require("node:fs");
const path = require("node:path");

const ROOT = path.resolve(__dirname, "..", "..");
const ADMIN = path.join(ROOT, "apps", "admin");

function read(dir, ext, out = []) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    if (entry.name === "node_modules" || entry.name === ".next") continue;
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) read(full, ext, out);
    else if (ext.some((e) => entry.name.endsWith(e))) out.push(fs.readFileSync(full, "utf8"));
  }
  return out;
}

/** The bodies of the two legs, extracted by brace balance from their `async function` header. */
function extractFunction(source, name) {
  const start = source.indexOf(`async function ${name}(`);
  if (start < 0) return null;
  const open = source.indexOf("{", start);
  let depth = 0;
  for (let i = open; i < source.length; i += 1) {
    if (source[i] === "{") depth += 1;
    else if (source[i] === "}") {
      depth -= 1;
      if (depth === 0) return source.slice(start, i + 1);
    }
  }
  return null;
}

/**
 * The module's rendered markers.
 *
 * A marker counts as present if the literal value appears in the source **or** the component takes
 * it as a `qa="…"` prop, because `ErrorState`/`ErrorStrip` render `data-qa={qa}` — the string only
 * ever reaches the DOM through that prop.
 */
function renderedMarkers() {
  const sources = read(ADMIN, [".tsx"]).join("\n");
  const values = new Set();
  for (const match of sources.matchAll(/data-qa(?:-[a-z-]+)?=(?:"([^"]+)"|\{`([^`]+)`\})/g)) {
    if (match[1] !== undefined) values.add(match[1]);
  }
  for (const match of sources.matchAll(/\bqa="([^"]+)"/g)) values.add(match[1]);
  // Template-built values expand to `<qa>-retry` and friends; keep the base and its suffix set.
  const rendered = new Set();
  for (const value of values) {
    rendered.add(value);
    if (!value.includes("{")) rendered.add(`${value}-retry`);
  }
  return { sources, rendered };
}

const legs = ["runCrmStateSweep", "runCrmKeyboardAndMobile"];
const walkthrough = fs.readFileSync(path.join(__dirname, "walkthrough.cjs"), "utf8");
const { sources, rendered } = renderedMarkers();

/**
 * Line comments are stripped before a leg is scanned; block comments are **not**.
 *
 * The fix for the dead-marker defect is *documented in place*, and a prose sentence naming
 * `[data-qa='crm-row']` is not a query — a gate that reads its own documentation as code reports
 * the fix as the defect it removed, and the next reader "restores" a line that was never broken.
 * That is the same class as measuring the original file instead of the mutated one.
 *
 * `//` only, deliberately. A block-comment stripper is a parser pretending to be a regex: this file
 * contains `page.route` registrations whose glob pattern ends in two asterisks and a slash, and the
 * run of punctuation right after it looks to a non-greedy matcher like a comment opener, which it
 * then pairs with an unrelated closing marker thousands of characters away — it swallowed **8,549
 * bytes of real code**, including the whole clause this gate exists to check, and reported the leg
 * as clean. Stripping only line comments cannot do that: it never needs to find a closing marker,
 * and the source's block-comment openers all sit at the start of a line.
 *
 * (Three instances of the same trap, all recorded here rather than only fixed: a closing marker
 * written inside a comment closes it, an opening marker written inside one opens a nested one, and
 * a glob written inside one looks like both. Prose about a delimiter cannot contain the delimiter —
 * this paragraph had to be rewritten three times before the file would parse, which is the cheapest
 * possible proof that the regex was the wrong shape.)
 */
function stripComments(source) {
  return source.replace(/(^|[^:])\/\/[^\n]*/g, "$1");
}

const results = [];
const fail = (name, detail) => results.push({ ok: false, name, detail });
const pass = (name, detail = "") => results.push({ ok: true, name, detail });

// --- 1. both legs are present, so this gate cannot pass by finding nothing to check ------------
const bodies = {};
for (const leg of legs) {
  const raw = extractFunction(walkthrough, leg);
  if (raw === null) fail(`${leg} is in the walkthrough`, "no such function — the gate is measuring nothing");
  else {
    bodies[leg] = stripComments(raw);
    pass(`${leg} is in the walkthrough`, `${raw.length} bytes`);
  }
}

// --- 2. every literal [data-qa='…'] selector resolves -------------------------------------------
// The template-literal forms are excluded deliberately: they carry `${`, so there is no literal
// value to check, and the sweep resolves them from its own `screens` table.
const LITERAL = /\[data-qa='([a-z0-9-]+)'\]/g;
let checked = 0;
const seen = new Map();
for (const [leg, body] of Object.entries(bodies)) {
  for (const match of body.matchAll(LITERAL)) {
    const value = match[1];
    if (value.includes("$")) continue;
    // Deduplicate: the sweep asks about `crm-contacts-error` from four different steps, and a gate
    // that reports the same fact four times inflates its own denominator into looking thorough.
    if (seen.has(`${leg}::${value}`)) continue;
    seen.set(`${leg}::${value}`, true);
    checked += 1;
    if (rendered.has(value)) pass(`${leg}: [data-qa='${value}'] exists`, "");
    else fail(`${leg}: [data-qa='${value}']`, "the module never renders this marker");
  }
}

// --- 3. the retry-recovery clause specifically ------------------------------------------------
// The clause is the one that can silently be a permanent `false`, so it is also asserted as its own
// check: a *composite* assertion hides a dead second clause behind a healthy first one, which is
// exactly the shape the defect had.
const retryMatch = bodies.runCrmStateSweep?.match(/theRetryRecoversTheScreen\s*=\s*[\s\S]{0,400}?;\n/);
const retry = retryMatch?.[0] ?? "";
if (!retry) {
  fail("the retry-recovery clause is present", "not found in the sweep");
} else if (!/recoveredRows|recoveredTable|data-qa-crm-cursor|count\(\)/.test(retry)) {
  fail("the retry-recovery clause is present", "it asserts no marker or count at all");
} else {
  // Every marker still named inside the clause must be one the module renders. A clause can be
  // rewritten to look thorough and still keep a dead marker, so the clause is re-scanned.
  const named = [...retry.matchAll(/\[data-qa(?:-[a-z-]+)?='([a-z0-9-]+)'\]/g)].map((m) => m[1]);
  const dead = named.filter((v) => !rendered.has(v));
  if (dead.length > 0) {
    fail("the retry-recovery clause is present", `names a marker the module never renders: ${dead.join(", ")}`);
  } else {
    pass("the retry-recovery clause is present", "asserts markers the module actually renders");
  }
}

// --- 4. the control: the gate must notice a marker that does not exist ---------------------------
// Without this the gate is indistinguishable from one that checks nothing — the defect class this
// very file was written to catch, reproduced inside the gate.
const control = "crm-contacts-error-definitely-not-rendered";
if (rendered.has(control)) {
  fail("control: a fake marker is reported as missing", `${control} resolved, so the check cannot fail`);
} else {
  pass("control: a fake marker is reported as missing", control);
}

// --- report -------------------------------------------------------------------------------------
for (const r of results) console.log(`${r.ok ? "ok  " : "FAIL"} ${r.name}${r.detail ? ` — ${r.detail}` : ""}`);
const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} checks passed (${checked} literal selectors resolved)`);
process.exit(failed === 0 ? 0 : 1);
