#!/usr/bin/env node
/**
 * A static gate: every `[data-*]` the walkthrough addresses must be a hook the product renders.
 *
 * ## The defect class
 *
 * A browser pass can only assert against a hook the product actually draws. When a pass asks for
 * `[data-member-drawer-action="block"]` and the component renders `[data-member-action="block"]`,
 * the locator matches **nothing**, `click()` throws, the surrounding `.catch(() => {})` swallows
 * it, and the step that was supposed to prove "blocking a member kills their session" is simply
 * never run — while every other step in the same pass keeps reporting, so the run looks like a
 * product defect instead of a dead instrument.
 *
 * It is invisible in review because the selector is a plausible name for a real control, and it is
 * invisible in the artifact because the refusal is silent. This gate is the static counterpart of
 * the box being free: it costs milliseconds, needs no browser, no database and no QA slot, so a
 * tick that cannot run the pass can still hold the harness honest.
 *
 * ## What counts as "rendered"
 *
 * Reading the source for `data-<name>` alone is what produced a first list of seven, and six of
 * the seven were wrong in instructive ways. Each of these is a real hook the product really draws,
 * and each would have been a false report:
 *
 *   - `data-content-api-tab={entry.label.toLowerCase()}` — a JSX **expression** attribute, so the
 *     literal name never appears in the file. The tab *is* rendered; the gate read its absence.
 *   - `data-webhook-header`, `[...], [data-webhook-overview-test]` — an **OR-alternative** where
 *     the second alternative is the real hook.
 *   - `data-push-enable-reason` — the product writes `id="push-enable-reason"`, because
 *     `aria-describedby` points at an **id**, not a data attribute. Demanding a `data-` here would
 *     be asking the product to render the same string twice.
 *   - `dataAttribute="analytics-overview-empty"` — a **string prop** the component helper
 *     interpolates into `data-{value}`.
 *   - `data-qa-idx` — the harness **injects** it into the page before driving it.
 *   - `[data-site-switcher]` — a line inside a **comment**, describing a selector the component
 *     deliberately does not carry. A gate that reads comments reports the note as a missing hook.
 *
 * So the gate models the four real indirection paths (expression attribute, string prop, `id`,
 * self-injection), strips comments and strings, and treats an OR-alternative as satisfied when
 * ANY branch of it resolves. Only what survives all of that is a finding.
 */
const fs = require("fs");
const path = require("path");

const ROOT = path.join(__dirname, "..", "..");
const FILE = path.join(ROOT, "scripts/qa/walkthrough.cjs");
const source = fs.readFileSync(FILE, "utf8");

function walk(dir, acc = []) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      if (["node_modules", ".next", ".git", "dist"].includes(entry.name)) continue;
      walk(p, acc);
    } else if (/\.(tsx|ts|jsx|js)$/.test(entry.name)) acc.push(p);
  }
  return acc;
}

/**
 * Blank a comment, but only one that **starts a line** (after indentation).
 *
 * ## The two wrong answers, and why this one is neither
 *
 * The first version was `source.replace(/\/\*[\s\S]*?\*\//g, …)`. It opened a block comment inside
 * a **string literal**: this repo contains `placeholder="/blog/*"`, and from that slash-star the
 * pattern ran to the next terminator in the file — 18,106 characters and 600 lines of real JSX in
 * `user-detail-view.tsx` swallowed as "comment". Four real `data-factor-*` hooks were then reported
 * as missing by a product that renders all four.
 *
 * The second version was a proper character scanner that also tracked string literals. It fixed that
 * file and broke a different one: `analytics/settings-view.tsx` contains the JSX *prose* `a prefix
 * (/admin/*) or`, and in JSX text an apostrophe or a stray quote is not a string delimiter, so the
 * scanner left its string state and stopped recognising the code around it. Eight real
 * `data-analytics-purge-*` and `data-analytics-erase-*` hooks vanished. Each fix moved the failure
 * rather than removing it.
 *
 * The lesson is the shape of the problem: **both wrong answers lose findings**, and a gate that
 * finds fewer things is indistinguishable from a clean tree. So the rule is deliberately the crudest
 * one that cannot misfire — in this codebase every comment delimiter is at the start of a line, and
 * no JSX prose line begins with one. A mid-line `//` (a trailing comment) is left in, which costs
 * this gate nothing: a comment cannot invent a `data-` name that the selector scan would then miss,
 * because the only thing a missed comment can do is produce a *false finding*, and every finding is
 * hand-checked against the product before anything is changed.
 *
 * Newlines are preserved, so a finding still carries its real line number.
 */
function strip(source) {
  const out = source.split("");
  const anchored = /^[ \t]*(\/\*[\s\S]*?(?:\*\/[ \t]*$|\*\/)|\/\/[^\n]*)/gm;
  let match;
  while ((match = anchored.exec(source)) !== null) {
    for (let k = match.index; k < match.index + match[0].length; k += 1) {
      if (out[k] !== "\n") out[k] = " ";
    }
  }
  return out.join("");
}

const code = strip(source);

const productFiles = [...walk(path.join(ROOT, "apps/admin")), ...walk(path.join(ROOT, "apps/web"))];
let blob = "";
for (const file of productFiles) blob += strip(fs.readFileSync(file, "utf8")) + "\n";

const rendered = new Set();
const add = (name) => rendered.add(String(name).replace(/^data-/, ""));

// (a) literal `data-foo` in any attribute position, including a JSX expression attribute's own
//     attribute name, and (b) `{... "data-foo" ...}` / `{...'data-foo'...}` object spreads.
for (const m of blob.matchAll(/data-([a-z0-9-]+)/g)) add(m[1]);
// (c) a string prop the product interpolates into a data attribute: dataAttribute="foo" -> data-foo.
for (const m of blob.matchAll(/\bdata[A-Za-z]*="([a-z0-9-]+)"/g)) add(m[1]);
for (const m of blob.matchAll(/\bdata[A-Za-z]*='([a-z0-9-]+)'/g)) add(m[1]);
for (const m of blob.matchAll(/\bdata[A-Za-z]*:\s*"([a-z0-9-]+)"/g)) add(m[1]);
// (d) `id="foo"`, which is what `aria-describedby` / `for` actually points at.
for (const m of blob.matchAll(/\bid="([a-z0-9-]+)"/g)) add(m[1]);

// (e) the hooks the harness injects into the page itself, so its own keyboard index is not a finding.
const selfInjected = [];
for (const m of code.matchAll(/(?:set|remove)Attribute\(\s*"(data-[a-z0-9-]+)"/g)) {
  selfInjected.push(m[1]);
  add(m[1]);
}

const results = [];
const check = (name, pass, detail) => results.push({ name, pass, detail });

// ------------------------------------------------------------------ the gate's own correctness

check(
  "the product blob is real (a gate that reads nothing passes everything)",
  productFiles.length > 100 && rendered.size > 500,
  `${productFiles.length} product files, ${rendered.size} rendered hooks`,
);
check(
  "the self-injected index is recognised as the harness's own",
  selfInjected.includes("data-qa-idx"),
  selfInjected.join(", ") || "none found — the injection scan is broken",
);

/**
 * The scanner's own regressions, on the two inputs that actually broke the two earlier versions.
 *
 * A stripper that eats code makes this gate report *fewer* findings, so a broken stripper is
 * indistinguishable from a clean tree unless it is asserted directly. Both probes are real lines
 * from real product files, not invented ones — inventing a probe is how a gate ends up asserting
 * something the product never had.
 */
const scannerProbes = [
  {
    why: "a string holding a comment opener is not a comment",
    src: 'const a = <input placeholder="/blog/*" />;\n  /* a real comment */\n  const b = <p data-factor-enrolment />;',
    mustKeep: "data-factor-enrolment",
    mustDrop: "a real comment",
  },
  {
    why: "a comment opener inside JSX prose is not a comment either",
    src: '  <span>\n    a prefix (<span className="font-mono">/admin/*</span>) or\n  </span>;\n  <b data-analytics-purge-cutoff />;',
    mustKeep: "data-analytics-purge-cutoff",
    mustDrop: null,
  },
];
for (const probe of scannerProbes) {
  const stripped = strip(probe.src);
  const kept = stripped.includes(probe.mustKeep);
  const dropped = probe.mustDrop === null || !stripped.includes(probe.mustDrop);
  check(
    `the comment stripper: ${probe.why}`,
    kept && dropped,
    `${probe.mustKeep} ${kept ? "kept" : "LOST"}`,
  );
  check(
    `  …and it preserves offsets there too (${probe.mustKeep} at the same line)`,
    probe.src.indexOf(probe.mustKeep) === stripped.indexOf(probe.mustKeep) &&
      stripped.split("\n").length === probe.src.split("\n").length,
    "line count and character offsets unchanged",
  );
}

// ------------------------------------------------------------------ the findings

/**
 * The whole predicate, as a function, so the "can fail" proof below re-runs the SAME code against
 * a deliberately broken input. A proven-to-fail check that re-implements the logic measures the
 * re-implementation, not the gate.
 *
 * ## The OR-alternative escape, and how it nearly hid the real defect
 *
 * `[data-webhook-header], [data-webhook-overview-test]` resolves on the second branch, so demanding
 * both would be demanding a rename. The first version of this escape looked for alternatives in a
 * ±240 character **window** around the match — and in a 16,000-line file that window is full of
 * unrelated selectors. It reported **zero findings**, because some other part of the file mentions
 * a real hook, which "rescued" `data-member-drawer-action` and swallowed the one real defect this
 * gate exists to find.
 *
 * So the group is read properly: a CSS attribute selector runs until its `]`, and a group is the
 * run of comma-separated selectors that follows it. Only selectors in that group count, which is
 * also what the browser does.
 */
function selectorGroup(codeText, start) {
  let end = start;
  while (end < codeText.length && codeText[end] !== "]") end += 1;
  end += 1;
  // Continue across `, [ ... ]` chains, and stop at a closing quote or a newline that ends the
  // expression — a selector list is always inside one string or one call argument.
  for (;;) {
    let cursor = end;
    while (cursor < codeText.length && /[\s,]/.test(codeText[cursor])) cursor += 1;
    if (codeText[cursor] !== "[") break;
    let inner = cursor + 1;
    while (inner < codeText.length && codeText[inner] !== "]") inner += 1;
    if (inner >= codeText.length) break;
    end = inner + 1;
  }
  return codeText.slice(start, end);
}

function unresolved(against) {
  const selRe = /\[(data-[a-z0-9-]+)(?:[~^$*|]?=[^\]]*)?\]/g;
  const found = new Map();
  let match;
  while ((match = selRe.exec(code)) !== null) {
    const name = match[1].replace(/^data-/, "");
    if (against.has(name)) continue;
    const group = selectorGroup(code, match.index);
    const alternatives = [...group.matchAll(/\[(data-[a-z0-9-]+)/g)].map((x) =>
      x[1].replace(/^data-/, ""),
    );
    if (alternatives.some((a) => against.has(a))) continue;
    if (!found.has(name)) {
      found.set(name, { name, line: code.slice(0, match.index).split("\n").length });
    }
  }
  return found;
}

const findings = unresolved(rendered);

check(
  "every [data-*] the walkthrough addresses is a hook the product renders",
  findings.size === 0,
  findings.size === 0
    ? "0 unresolved"
    : [...findings.values()].map((f) => `data-${f.name} (line ${f.line})`).join("; "),
);

// ------------------------------------------------------------------ the gate can fail

/**
 * Four mutations, each removing ONE of the ways the gate resolves a name, and each asserted to turn
 * a specific real hook into a finding. They are the checks that make the green above mean something:
 * a gate that resolved nothing would report the same zero.
 *
 * The first attempt at this block was a tautology — it computed `withoutProduct` and then asserted
 * a name was absent from a set, which is true whether or not the predicate runs. It is written here
 * as calls to `unresolved()`, so what is measured is the same code path the finding above used.
 */
const mutations = [
  {
    what: "the string-prop indirection (dataAttribute=) is load-bearing",
    drop: (set) => {
      for (const m of blob.matchAll(/\bdata[A-Za-z]*="([a-z0-9-]+)"/g)) set.delete(m[1]);
      return "analytics-overview-empty";
    },
  },
  {
    what: "the id= indirection (what aria-describedby points at) is load-bearing",
    drop: (set) => {
      for (const m of blob.matchAll(/\bid="([a-z0-9-]+)"/g)) set.delete(m[1]);
      return "push-enable-reason";
    },
  },
  {
    what: "the OR-alternative escape is load-bearing",
    drop: (set) => {
      set.delete("webhook-overview-test");
      return "webhook-header";
    },
  },
  {
    what: "the self-injected index is not counted as a product hook",
    drop: (set) => {
      set.delete("qa-idx");
      return "qa-idx";
    },
  },
];

for (const mutation of mutations) {
  const stripped = new Set(rendered);
  const expected = mutation.drop(stripped);
  const found = unresolved(stripped);
  check(
    `and it can fail: ${mutation.what}`,
    found.has(expected) && found.get(expected).line > 0,
    `removing it makes data-${expected} a finding at line ${found.get(expected)?.line ?? "?"}`,
  );
}

/**
 * The real defect, as a permanent regression rather than a one-time observation.
 *
 * `data-member-drawer-action` was the name the members pass asked for; the product renders
 * `data-member-action`, on the rows, the phone cards and the drawer alike. The click threw into a
 * `.catch()` and every step from the block dialog to `blockedInSql` reported on a member that was
 * never blocked — silently, on every run since the pass was written.
 *
 * Pinning it as a check that the name is *gone* would break the day someone legitimately needed it.
 * What is worth holding is the fix: the drawer's actions are addressed **through the drawer**, so a
 * shared hook can never be driven by accident and a refactor of the row actions cannot make this
 * pass click the wrong member.
 *
 * The pattern tolerates whitespace because the call is written across four lines by the formatter,
 * and a gate that only matches one formatting is a gate that will be "fixed" by reformatting.
 */
const membersDrawerScoped = new RegExp(
  '\\[data-member-drawer="\\$\\{[a-zA-Z]+Id\\}"\\][\\s\\S]{0,40}?\\[data-member-action=',
).test(code);
check(
  "the members drawer's actions are addressed through the drawer, not by a shared hook",
  membersDrawerScoped,
  'a row also renders data-member-action="block", and .first() would drive the row',
);

/** And the name that caused it must not be back. */
check(
  "and the hook that does not exist is not addressed anywhere",
  !code.includes("data-member-drawer-action"),
  "data-member-drawer-action is rendered by no file in apps/",
);

// ------------------------------------------------------------------ report

let failed = 0;
for (const r of results) {
  if (!r.pass) failed += 1;
  console.log(`${r.pass ? "  ok" : "FAIL"}  ${r.name}`);
  if (r.detail) console.log(`        ${r.detail}`);
}
console.log("");
if (failed) {
  console.log(`FAIL — ${failed} check(s) red`);
  process.exit(1);
}
console.log(`PASS — ${ROOT}/scripts/qa/walkthrough.cjs`);
