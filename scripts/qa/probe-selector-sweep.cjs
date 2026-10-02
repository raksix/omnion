#!/usr/bin/env node
// Every selector walkthrough.cjs queries, checked against the product it measures.
//
// WHY. A `data-*` attribute the product never renders is a CONSTANT, and a constant reads
// exactly like a measurement: `count() > 0` is false forever, so the row reports "the empty
// state never appeared" on every run -- including the runs where it did. Tick 86 found the
// fifth site of that class in the builder passes; this gate makes the CLASS non-recurring
// across the whole walkthrough rather than in the two passes one report named.
//
// WHAT MADE THE FIRST THREE SWEEPS WRONG (do not re-introduce any of these):
//   1. An alternation like ("|…) matches a QUOTE ALONE, so every file with any quote "hit".
//   2. Requiring `=` misses JSX shorthand: `data-foo` alone on its own line is a real,
//      rendered attribute. This is the mistake this gate's own first version made -- it
//      reported 522 unresolved, and a spot check of three showed all three present.
//   3. A delimiter class without `$` can never match a line-final attribute: grep is
//      line-oriented and the newline is not in the line.
//
// ATTRIBUTE VALUE vs NAME. A marker can be an attribute NAME (`data-analytics-row`) or an
// attribute VALUE carried by a shared attribute (`data-analytics-state="analytics-overview-empty"`).
// The first version only knew about names and would have called the second class of site
// unresolved too. Both spellings are accepted here, and a name that only ever appears as
// another attribute's VALUE is not counted as resolved by a bare substring match.
const fs = require("fs");
const path = require("path");

const ROOT = path.resolve(__dirname, "..", "..");
const WT = path.join(ROOT, "scripts", "qa", "walkthrough.cjs");
const src = fs.readFileSync(WT, "utf8");

function productFiles(dir, out = []) {
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    if (e.name === "node_modules" || e.name === ".next" || e.name === "target" || e.name === ".git") continue;
    const p = path.join(dir, e.name);
    if (e.isDirectory()) productFiles(p, out);
    // A marker inside a TEST FILE is the gate asserting the marker exists, not the product
    // rendering it — and a gate's own assertion is the one place a sweep is guaranteed to
    // find the string. Including them makes every sweep look green, which is exactly how the
    // first version of this gate reported 0 unresolved for two markers that do not exist.
    else if (/\.(tsx|ts)$/.test(e.name) && !/\.(test|spec)\.(tsx|ts)$/.test(e.name)) out.push(p);
  }
  return out;
}

const files = [
  ...productFiles(path.join(ROOT, "apps", "admin")),
  ...productFiles(path.join(ROOT, "apps", "web")),
];
const hay = files.map((f) => fs.readFileSync(f, "utf8")).join("\n");

// Collect literals from every DOM query form the pass uses.
//
// The capture must respect the OPENING quote: a single-quoted JS string may contain double
// quotes (`'[data-apply-plan], button:has-text("Apply")'`), and a capture that forbids all
// three quote characters truncates it at the inner `"`. Five compound selectors reported
// unresolved for exactly this reason — the marker was fine and the reader was wrong.
const forms = [
  /locator(?:All)?\(\s*'([^'\n]{2,160})'/g,
  /locator(?:All)?\(\s*"([^"\n]{2,160})"/g,
  /locator(?:All)?\(\s*`([^`\n]{2,160})`/g,
  /(?:querySelector|querySelectorAll|waitForSelector|closest)\(\s*'([^'\n]{2,160})'/g,
  /(?:querySelector|querySelectorAll|waitForSelector|closest)\(\s*"([^"\n]{2,160})"/g,
  /\$\$\(\s*'([^'\n]{2,160})'/g,
  /\$\$\(\s*"([^"\n]{2,160})"/g,
  /getAttributes?\(\s*'([^'\n]{2,160})'/g,
  /getAttributes?\(\s*"([^"\n]{2,160})"/g,
];
const selectors = new Set();
for (const re of forms) {
  for (const m of src.matchAll(re)) selectors.add(m[1]);
}

function simplePart(p) {
  p = p.trim();
  if (!p || /^\[?(text|has-text|has|not|is|visible|and|or)\b/.test(p)) return true; // content engine
  if (/^\/?(regex|text|css|xpath)=/.test(p)) return true;                      // engine prefix
  let m = p.match(/\[([a-zA-Z][\w:.-]*)(?:[*^$|]?=.*)?\]/);
  if (m) return resolves(`[${m[1]}]`);
  m = p.match(/^#([A-Za-z][\w-]*)/);
  if (m) return resolves(`#${m[1]}`);
  m = p.match(/^\.([A-Za-z][\w-]*)/);
  if (m) return tailwindClass(m[1]);                  // `.animate-pulse` — via the stylesheet
  // `button:has-text("Apply")` — the TAG is the marker; the text is content, and the
  // pass's `has-text` is deliberately a second alternative to a data marker.
  m = p.match(/^([a-z][a-zA-Z0-9]*):has-text\(/);
  if (m) return new RegExp(`<${m[1]}[\\s>/]`).test(hay);
  m = p.match(/^([a-z][a-zA-Z0-9]*)/);
  if (m) return new RegExp(`<${m[1]}[\\s>/]`).test(hay);   // a bare tag the product renders
  return true;
}

// React renders `inputMode` as the `inputmode` attribute, so a selector written in HTML
// spelling still matches — the two spellings are the same attribute. Same for the HTML
// attribute names a React prop maps onto. Matching a selector against source that spells
// the prop in camelCase reports a live marker as unresolved, and the fix for "unresolved"
// must never be to stop asking.
function attrAlias(name) {
  // Both directions. `data-foo` is written `dataFoo` in React, and `inputmode` is written
  // `inputMode` — the same attribute in the two spellings. Converting only hyphens (the
  // first version) leaves `inputmode` alone and reports a live marker as unresolved.
  const out = new Set();
  if (name.includes("-")) out.add(name.replace(/-([a-z])/g, (_, c) => c.toUpperCase()));
  for (let i = 1; i < name.length; i += 1) {
    if (/[a-z0-9]/.test(name[i - 1]) && /[a-z]/.test(name[i])) {
      out.add(name.slice(0, i) + name[i].toUpperCase() + name.slice(i + 1));
    }
  }
  out.delete(name);
  return [...out];
}

function resolves(sel) {
  const attr = (name) => new RegExp(`(^|[^\\w-])${name}([^\\w-]|$)`, "m").test(hay);
  if (!sel.trim()) return true;
  // A COMMA GROUP is a UNION, and a union is live when AT LEAST ONE alternative matches.
  // Requiring every alternative made a working locator ("[data-webhook-header], [data-webhook-
  // overview-test]" — the first is dead, the second is the marker the pass really uses) report
  // as unresolved. The defect this gate hunts is an ALL-DEAD group, and only that is a finding.
  if (sel.includes(",")) {
    return sel.split(",").some((part) => resolves(part));
  }
  // A bare attribute NAME (getAttribute("href")) is a property the product sets, not a marker.
  if (/^[a-zA-Z][\w:.-]*$/.test(sel)) return true;
  let m = sel.match(/\[([a-zA-Z][a-zA-Z0-9:_-]*)(?:[*^$]?=|\])/);
  if (m && sel[0] !== "#") {
    const name = m[1];
    const aliases = attrAlias(name);
    if (attr(name) || aliases.some((al) => attr(al))) {
      const asValueOnly =
        !new RegExp(`${name}\\s*[:=]`).test(hay) && new RegExp(`=\\s*['"\`]?${name}`).test(hay);
      if (!asValueOnly) return true;
    }
    // An attribute NAME no product file writes, but which appears as a VALUE.
    if (new RegExp(`=\\s*['"\`]${name}['"\`]`).test(hay)) return true;
    // A marker the PASS writes itself, one line above where it reads it, is an instrument
    // and not a product surface.
    if (probeWritten(name)) return true;
    return false;
  }
  m = sel.match(/^#([A-Za-z][\w-]*)/);
  if (m) return new RegExp(`id\\s*=\\s*['"\`]${m[1]}['"\`]`).test(hay) || attr(m[1]);
  m = sel.match(/^([a-z][a-zA-Z0-9]*)((?:\[[^\]]*\])+)$/);
  if (m) {
    // `input[inputmode="numeric"]` — a TAG with attribute predicates. The tag must render and
    // every attribute must be written SOMEWHERE in the product (React spells `inputMode`,
    // the DOM attribute is `inputmode`, so the alias rule applies here too).
    if (!new RegExp(`<${m[1]}[\\s>/]`).test(hay)) return false;
    return [...m[2].matchAll(/\[([a-zA-Z][\w:.-]*)/g)].every((a) => resolves(`[${a[1]}]`));
  }
  m = sel.match(/^([a-z][a-zA-Z0-9]*)/);
  if (m) return new RegExp(`<${m[1]}[\\s>/]`).test(hay);
  const parts = sel.split(/\s+/).filter(Boolean);
  if (parts.length > 1) return parts.every(simplePart);
  return true; // text=/…/, :has-text(…) and friends are content, not markers
}

// Two spellings that resolve to a marker WITHOUT the product ever writing it, and must
// therefore not be reported as a constant:
//
//  · A CLASS (`[data-loading], .animate-pulse`) — Tailwind utility classes reach the DOM
//    through a stylesheet, so no .tsx line ever contains `.animate-pulse` as an attribute.
//    The walkthrough queries `.animate-pulse` because it is a real, shipped class.
//  · A PROBE-WRITTEN MARKER (`[data-qa-idx]`) — the pass sets it itself with setAttribute,
//    one line above where it reads it. It is an instrument, not a product surface, and
//    reporting it unresolved would point the next reader at the product to fix nothing.
//
// Both are verified against the pass's own text rather than assumed, so a class or an
// attribute the product never defines and never writes still reports as unresolved.
function probeWritten(name) {
  return new RegExp(`setAttribute\\(\\s*['"\`]${name}['"\`]`).test(src);
}
function tailwindClass(name) {
  return new RegExp(`(^|[^\\w-])${name}([^\\w-]|$)`, "m").test(
    files.map((f) => fs.readFileSync(f, "utf8")).join("\n") + src,
  );
}

const literals = [...selectors].filter((s) => !/\$\{|\$\(/.test(s));
const unresolved = literals.filter((s) => !resolves(s));

console.log(`selector sweep · ${literals.length} literal selectors from walkthrough.cjs`);
console.log(`product files · ${files.length} · ${hay.length} bytes`);
console.log(`UNRESOLVED: ${unresolved.length}`);
for (const s of unresolved) console.log(`  ${s}`);

// CONTROL. A sweep whose zero means nothing is worse than no sweep. The two markers removed
// in the commit that fixed them are handed back and must be reported as unresolved; if this
// gate cannot see them, it cannot see anything.
const control = ["data-table-edit", "data-node-inspector"];
const blind = control.filter((c) => resolves(`[${c}]`));
if (blind.length) {
  console.log(`\nCONTROL FAILED · the sweep cannot see a known-absent marker: ${blind.join(", ")}`);
  process.exit(1);
}
console.log(`control · ${control.length} known-absent markers reported unresolved, as they must be`);

if (unresolved.length) {
  console.log("\nFAIL · every marker above is queried but never rendered");
  process.exit(1);
}
console.log("\nALL RESOLVE");