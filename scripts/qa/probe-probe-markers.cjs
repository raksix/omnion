#!/usr/bin/env node
/**
 * Every `data-*` attribute the walkthrough queries must exist in the product.
 *
 * ## The defect class, and why it keeps recurring
 *
 * A row that queries a `data-*` attribute the product never renders returns a **constant**: a
 * count of 0, or a boolean that is always false. Nothing turns red, nothing crashes, and the
 * field reads like a measurement. This REQ has now produced five of them, each one a claim the
 * criterion names being answered by a marker that does not exist:
 *
 *   | row | queried | product ships | consequence |
 *   |---|---|---|---|
 *   | `narrow-lock` (tick 86) | `data-table-edit` | `data-table-label` / `-param` / `-save` | `editControls: 0`, always |
 *   | `undo-selection` (tick 86) | `data-node-inspector` | `data-inspector={node.id}` | `inspectorOpen: false`, always |
 *   | `keyboard-pass` (tick 74) | `node.node_type` | the wire calls it `type` | `paramWrote` structurally null |
 *   | `step-trace` (tick 57) | `step.output` | the panel renders both sides | one step read as the node's |
 *
 * The fifth (`undo-selection`) is the reason this file is a **repo-wide sweep** rather than
 * another per-row test: it is the same defect one row over from the first, in the same pass, and
 * it survived precisely because tick 62's fix was written from the report that named four sites
 * instead of from a search for the pattern — which is what the walkthrough's own comment says,
 * three lines above the defect, in its own words.
 *
 * ## Why this was three wrong scripts before it was right
 *
 * The hard part is not the sweep, it is deciding whether an attribute is present. A JSX
 * attribute may be written bare (`data-builder`), quoted (`data-table-save="x"`), single-quoted
 * in a `querySelector`, or alone on its own line. Three attempts failed in three different ways,
 * and **every one of them reported a clean bill of health while being wrong**:
 *
 * 1. Requiring quotes missed every bare attribute, then the fix "match bare OR quoted" used an
 *    alternation containing a bare `("|…)`, which matches **a quote alone** — so every file
 *    containing any quote was a hit, including `node_modules/.bin/tsc`. 578 of 590 attributes
 *    "unresolved" turned into zero.
 * 2. Requiring `=` after the bare form missed attributes written alone on a line — which is how
 *    this codebase writes them (`data-builder` on line 2178, nothing after it but the newline).
 * 3. A trailing-delimiter class without `$` cannot match a line-final attribute, because
 *    **grep is line-oriented**: the newline is not in the line. This is the mistake that made
 *    version 3 disagree with the tree in both directions at once.
 *
 * So the gate below carries a **control**: it is handed the two attributes the fix removed and
 * must report both. A sweep whose control passes nothing is a no-op with a green light, and that
 * is exactly the shape this file exists to end.
 *
 * Usage: `node scripts/qa/probe-probe-markers.cjs`
 * Exit 0 = every attribute the builder pass queries exists in the product, AND the control bites.
 */

const { readFileSync } = require("node:fs");
const { execFileSync } = require("node:child_process");
const { dirname, join } = require("node:path");


// `__dirname` rather than `import.meta.url`: this is a CommonJS gate, like every sibling
// `probe-*.cjs` in this directory. A gate that cannot load is a gate nobody runs.
const HERE = __dirname;
const REPO = join(HERE, "..", "..");
const WALKTHROUGH = join(REPO, "scripts", "qa", "walkthrough.cjs");

/** Strip comments, so a selector quoted in prose is not counted as one the probe uses. */
function stripComments(src) {
  return src
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/(^|[^:"'`\\])\/\/[^\n]*/g, "$1");
}

/**
 * Does `apps/` render this attribute?
 *
 * A JSX attribute name ends at end-of-line, whitespace, `/`, `>`, `=`, `{`, `}` or `,`. The `$`
 * alternative is load-bearing and was the third version's bug: **grep is line-oriented**, so an
 * attribute alone on a line has no trailing character *inside the line* to match against, and
 * every line-final attribute in this codebase reads as absent.
 *
 * `re.escape` escapes `-` to `\-`, which `String.prototype.format` then rejects — so the pattern
 * is concatenated rather than formatted.
 */
function patternFor(attr) {
  const e = attr.replace(/[.*+?^${}()|[\]\\-]/g, "\\$&");
  return `(^|[^A-Za-z0-9_-])(?:"${e}"|'${e}'|${e}(?=$|[\\s/>=,{}]))`;
}

function renderedBy(attr) {
  let out = "";
  try {
    out = execFileSync("grep", ["-rlP", patternFor(attr), "apps/"], {
      cwd: REPO,
      encoding: "utf8",
      maxBuffer: 64 * 1024 * 1024,
    });
  } catch (error) {
    // grep exits 1 when nothing matched — a real answer, not an error.
    if (error?.status === 1) return [];
    throw error;
  }
  return out
    .split("\n")
    .filter(Boolean)
    // The product is the apps tree. Tests, mutation runners and the probe's own comments are not
    // the product, and a marker "found" only in a test file is the same defect wearing a hat.
    .filter(
      (path) =>
        !path.endsWith(".test.ts") &&
        !path.endsWith(".test.tsx") &&
        !path.endsWith(".mutation.mjs") &&
        !path.endsWith(".mutation.ts") &&
        !path.includes("node_modules") &&
        !path.includes("/.next/") &&
        !path.includes("/scripts/qa/"),
    );
}

/** The rows this wave owns: the builder depth pass and its table-mode sibling. */
function selectorsIn(source, functionName) {
  const start = source.indexOf(`async function ${functionName}`);
  if (start < 0) return [];
  const end = source.indexOf("\nasync function ", start + 10);
  const body = source.slice(start, end < 0 ? source.length : end);
  return Array.from(new Set(Array.from(body.matchAll(/\[(data-[a-z0-9-]+)[\]=]/g), (m) => m[1])));
}

const WALK = stripComments(readFileSync(WALKTHROUGH, "utf8"));
const rows = ["runWorkflowBuilderDepth", "runWorkflowTableDepth"];
const attributes = rows.flatMap((name) => selectorsIn(WALK, name)).sort();

let failures = 0;

process.stdout.write(`probe-marker sweep · ${attributes.length} data-* selectors in ${rows.join(", ")}\n`);

// --- control: the two attributes this class was found through ------------------------------
const CONTROL = ["data-table-edit", "data-node-inspector"];
process.stdout.write("\ncontrol · the attributes the tick-86 fixes removed\n");
for (const attr of CONTROL) {
  const hits = renderedBy(attr);
  const ok = hits.length === 0;
  process.stdout.write(
    `  ${ok ? "RED" : "GREEN"} · ${attr} — ${ok ? "reported unresolved, as it must be" : `WRONGLY FOUND in ${hits.join(", ")}`}\n`,
  );
  if (!ok) failures += 1;
}

// --- the sweep -------------------------------------------------------------------------------
const missing = attributes.filter((attr) => renderedBy(attr).length === 0);
process.stdout.write(`\nsweep · ${missing.length} unresolved\n`);
for (const attr of missing) {
  const where = attributes.filter((a) => a === attr).length;
  process.stdout.write(
    `  ${attr} — a row queries it and no product file renders it. Every count over it is a\n` +
      `    constant, and a constant reads exactly like a measurement. (${where} site(s))\n`,
  );
}
if (missing.length > 0) failures += 1;

process.stdout.write(
  `\n${failures === 0 ? "ALL RESOLVE" : `${failures} PROBLEM(S)`}\n` +
    (missing.length === 0
      ? "  (every queried marker exists in the product, and the control proves this sweep can see one that does not)\n"
      : ""),
);
process.exit(failures === 0 ? 0 : 1);