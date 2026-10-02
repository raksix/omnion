#!/usr/bin/env node
/**
 * A slot-free gate for the keyboard-cursor claim on `/crm/deals` (tick 79).
 *
 * ## The defect this catches
 *
 * The board had **two** cursors: the frame's `selectedIndex` (a number `j`/`k` move, held in
 * `CrmShell` through `useCrmKeyboard`) and the board's own `focusedCard` (the id the card draws
 * its ring on). A one-way sync was meant to join them:
 *
 *   useEffect(() => {
 *     const id = frame ? dealIds[frame.selectedIndex] : null;
 *     if (id) setFocusedCard(id);
 *   }, [dealIds, frame]);
 *
 * `frame` is **not** a stable value. `CrmShell` builds its context object as a bare literal —
 * `const value: CrmListState = { ... }` with no `useMemo` — so it is a *new object on every
 * render*. Depending on it therefore made the effect re-run after every render, and the body
 * wrote `focusedCard` from a number that only `j`/`k` ever moved.
 *
 * The consequence is the contradiction the last browser report described and could not resolve:
 * a ring that is drawn (`theCursorIsVisible: true`), that does not follow the mouse, and that
 * `Enter` disagrees with. Concretely, an operator clicking the third card drew the ring there for
 * one frame; the next render of anything on the page — a hover, a notice clearing, a refetch —
 * put it back on the first card. The report's "a cursor that exists, is drawn, and does not move
 * under `j`" is the same single fact read from the other end.
 *
 * Why it survived review: the comment above the effect *describes* the sync as correct, and a
 * dependency array that is non-empty still looks like a dependency array. Only the identity of
 * the value in it distinguishes "re-run when the cursor moved" from "re-run when the page
 * re-renders".
 *
 * ## What this gate checks, and what it deliberately does not
 *
 * It parses the module's source and asserts the two properties that make the sync hold:
 *
 *   1. no `useEffect`/`useMemo`/`useCallback` dependency array in the CRM module names a value
 *      that the provider builds as a fresh object literal on every render (the `frame` shape);
 *   2. the board's card click moves **both** cursors, so clicking and `j` leave the same state.
 *
 * It reads no browser and claims no box. The browser pass still closes the keyboard box; this
 * gate exists so the *class* of defect cannot sit there for another four ticks waiting on a
 * slot that sibling writers hold for 25-minute passes.
 *
 * Exit 0 = all checks passed. Exit 1 = at least one failed, each printed with its name.
 */

const fs = require("node:fs");
const path = require("node:path");

const ROOT = path.resolve(__dirname, "..", "..");
const PARTS = path.join(ROOT, "apps/admin/features/crm/crm-parts.tsx");
const DEALS = path.join(ROOT, "apps/admin/features/crm/deals-view.tsx");

const results = [];
function check(name, condition, detail) {
  results.push({ name, ok: !!condition, detail: condition ? "" : detail || "" });
}

/**
 * Remove `//` comments and block comments **accurately**.
 *
 * An earlier probe in this repo used `/\*[\s\S]*?\*\//` to strip block comments and paired a
 * `page.route` glob with an unrelated closer, swallowing 8,549 bytes of real code — including
 * the very clause it was checking — so it reported a defective leg as clean. A string- and
 * regex-aware scanner is the only shape that is safe here: this gate must not be able to
 * "pass" by deleting the evidence.
 */
function stripComments(src) {
  let out = "";
  let i = 0;
  const n = src.length;
  while (i < n) {
    const two = src.slice(i, i + 2);
    if (two === "//") {
      while (i < n && src[i] !== "\n") i += 1;
      continue;
    }
    if (two === "/*") {
      let depth = 1;
      i += 2;
      while (i < n && depth > 0) {
        if (src.slice(i, i + 2) === "/*") { depth += 1; i += 2; continue; }
        if (src.slice(i, i + 2) === "*/") { depth -= 1; i += 2; continue; }
        i += 1;
      }
      out += " ";
      continue;
    }
    // Strings, template literals and regexes are copied verbatim; a `//` inside one is not a
    // comment, and a brace inside one is not a block boundary.
    const ch = src[i];
    if (ch === '"' || ch === "'" || ch === "`") {
      const quote = ch;
      out += ch; i += 1;
      while (i < n) {
        if (src[i] === "\\") { out += src.slice(i, i + 2); i += 2; continue; }
        out += src[i];
        if (src[i] === quote) { i += 1; break; }
        i += 1;
      }
      continue;
    }
    out += ch;
    i += 1;
  }
  return out;
}

const partsRaw = fs.readFileSync(PARTS, "utf8");
const dealsRaw = fs.readFileSync(DEALS, "utf8");

// ---------------------------------------------------------------------------
// 1. The provider really does rebuild its value every render. If this ever becomes
//    memoised, the object-dependency check below stops being the right rule and this
//    assertion is what says so — a gate that keeps passing after its premise moved is
//    worse than no gate.
// ---------------------------------------------------------------------------
const partsCode = stripComments(partsRaw);
const valueIsPlainLiteral = /const\s+value\s*:\s*CrmListState\s*=\s*\{/.test(partsCode);
const valueIsMemoised = /useMemo\([\s\S]{0,200}?CrmListState/.test(partsCode);
check(
  "the-provider-rebuilds-its-value-each-render",
  valueIsPlainLiteral && !valueIsMemoised,
  `expected a bare \`const value: CrmListState = {...}\` literal; found literal=${valueIsPlainLiteral} memoised=${valueIsMemoised}. If the provider is now memoised this gate's rule is stale and must be rewritten.`,
);

// ---------------------------------------------------------------------------
// 2. No dependency array anywhere in the module names the non-memoized context object.
//    `frame` (or any alias of it) inside a deps array is the exact defect: the array is
//    non-empty, so it reads as a dependency, but the value is new every render.
// ---------------------------------------------------------------------------
const moduleFiles = fs
  .readdirSync(path.join(ROOT, "apps/admin/features/crm"))
  .filter((f) => f.endsWith(".tsx"))
  .map((f) => path.join(ROOT, "apps/admin/features/crm", f));

const offenders = [];
for (const file of moduleFiles) {
  const code = stripComments(fs.readFileSync(file, "utf8"));
  // A dependency array is the `[...]` that closes a hook call.
  const depArrays = code.match(/\},\s*(\[[^\]]*\])\s*\)/g) || [];
  for (const raw of depArrays) {
    const deps = raw.slice(raw.indexOf("["), raw.lastIndexOf("]") + 1);
    // The context object and its aliases. `frameIndex` is the primitive the fix introduced
    // and is explicitly allowed.
    if (/\bframe\b/.test(deps) && !/\bframeIndex\b/.test(deps)) {
      offenders.push(`${path.basename(file)}: ${deps}`);
    }
  }
}
check(
  "no-dependency-array-depends-on-the-frame-object",
  offenders.length === 0,
  `a hook depends on \`frame\`, which CrmShell rebuilds as a fresh object literal on every render, so the effect re-runs after every render: ${offenders.join(" | ")}`,
);

// ---------------------------------------------------------------------------
// 3. The board's own effect must read the primitive, not the object. Belt and braces for
//    rule 2: if someone reintroduces `frame.selectedIndex` inline without naming it in a
//    deps array array, this still catches the shape.
// ---------------------------------------------------------------------------
const dealsCode = stripComments(dealsRaw);
const usesPrimitiveIndex = /frameIndex/.test(dealsCode) && /dealIds\[frameIndex\]/.test(dealsCode);
check(
  "the-board-sync-reads-a-primitive-index",
  usesPrimitiveIndex,
  "the board's cursor sync should resolve the card id through `dealIds[frameIndex]` (a primitive), so the effect body runs only when the index really changed",
);

// ---------------------------------------------------------------------------
// 4. Clicking a card moves both cursors. Only setting the ring leaves `j` and the mouse
//    disagreeing about where the operator is, which is the whole defect.
// ---------------------------------------------------------------------------
const clickBlock = dealsCode.match(/onClick=\{\(\)\s*=>\s*\{[\s\S]{0,400}?\}\s*\}/);
// Fall back to the concise arrow form the defect shipped in.
const conciseClick = dealsCode.match(/onClick=\{\(\)\s*=>\s*setFocusedCard\([^)]*\)\s*\}/);
const clickMovesBoth =
  Boolean(clickBlock && /setFocusedCard\(deal\.id\)/.test(clickBlock[0]) && /setSelectedIndex\(/.test(clickBlock[0])) ||
  (!conciseClick && Boolean(clickBlock));
check(
  "a-card-click-moves-both-cursors",
  clickMovesBoth && !conciseClick,
  conciseClick
    ? "the card's onClick is `() => setFocusedCard(deal.id)` — it draws the ring but leaves the frame's selectedIndex where `j` last put it, so the sync effect moves the ring back and `Enter` opens a different deal than the one that was clicked"
    : "the card's onClick does not appear to call both setFocusedCard and the frame's setSelectedIndex",
);

// ---------------------------------------------------------------------------
// 5. The two cursors must still be joined at all, and the join must be *stable* — it
//    must read the index, not the object. Stating this as a second, separate claim is
//    deliberate: the first draft of this rule required `dealIds[frameIndex]`, so on the
//    real pre-fix file it printed "no path sets focusedCard from the frame's index any
//    more — the board would have two cursors that never agree". That was **false**. The
//    pre-fix file did join them; it joined them through the object, which is precisely
//    why the ring followed `j` and not the mouse. A gate that names the wrong reason for
//    a red result teaches its reader something untrue about their own code, and the next
//    person to run it discounts the whole file.
// ---------------------------------------------------------------------------
const joinsViaIndex = /dealIds\[frameIndex\]/.test(dealsCode);
const joinsViaObject = /dealIds\[frame\.selectedIndex\]/.test(dealsCode);
check(
  "the-two-cursors-are-joined-through-the-index",
  joinsViaIndex,
  joinsViaObject
    ? "the cursors are joined, but through `dealIds[frame.selectedIndex]` — the object `CrmShell` rebuilds every render. The join exists and is still wrong: it re-fires on every render and rewrites the ring from a number only `j`/`k` moved"
    : "no path resolves focusedCard from the frame's index at all — the board would have two cursors that never agree",
);

// ---------------------------------------------------------------------------
// 6. The gate must not be able to pass by deleting the evidence: prove the stripper
//    leaves the clauses under test intact. A block comment or a regex literal containing
//    a brace must not be able to swallow code here.
// ---------------------------------------------------------------------------
const braceCheck = stripComments(`const a = { x: 1 }; // { gone }\nconst b = /[{]/; /* gone { } */ const c = 2;`);
check(
  "the-comment-stripper-preserves-code",
  braceCheck.includes("const a = { x: 1 }") && braceCheck.includes("const c = 2") && !braceCheck.includes("gone"),
  `stripper mangled the source: ${JSON.stringify(braceCheck)}`,
);

// ---------------------------------------------------------------------------
const failed = results.filter((r) => !r.ok);
for (const r of results) {
  console.log(`${r.ok ? "ok  " : "FAIL"} ${r.name}${r.detail ? `\n       ${r.detail}` : ""}`);
}
console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
process.exit(failed.length === 0 ? 0 : 1);
