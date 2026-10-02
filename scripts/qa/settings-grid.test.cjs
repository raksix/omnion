/**
 * The gate for the settings screen's number inputs (REQ-130, slice 2).
 *
 * ## The defect, and why it is arithmetic rather than visual
 *
 * `<input type="number" min={min} step={step}>` does not accept "a number in range". It accepts a
 * number ON THE GRID `(min + k·step)`. The screen declared `min: 1, step: 10` for the cost budget
 * while the endpoint's shipped default is `1000` — and `1000` is not on that grid, so the browser's
 * own validation refused it with a tooltip naming `991` and `1001` as the nearest valid values.
 * The endpoint accepted it happily. Two validators, two answers, and the screen was the one telling
 * the operator their legal value was invalid.
 *
 * Three of the six fields had it. The first was found by the walk refusing a legal save; the second
 * and third were found by doing the arithmetic over all six rather than fixing one and stopping —
 * because the second and third instances of a defect are the ones a reviewer stops looking for.
 *
 * So this gate computes the grid for every declared field and asserts the SHIPPED DEFAULT is on it.
 * The defaults are read from the API's own Rust constants, so a change there breaks this gate
 * instead of silently making the screen reject the new default.
 */

const fs = require("fs");
const path = require("path");

const VIEW = path.join(__dirname, "../../apps/admin/features/developer/settings-view.tsx");
const SETTINGS_RS = path.join(__dirname, "../../crates/graphql/src/settings.rs");

const view = fs.readFileSync(VIEW, "utf8");
const settings = fs.readFileSync(SETTINGS_RS, "utf8");

let failures = 0;
let checks = 0;

function check(name, ok, detail) {
  checks += 1;
  if (ok) {
    console.log(`  ok   ${name}`);
    return true;
  }
  failures += 1;
  console.log(`  FAIL ${name}${detail ? ` — ${detail}` : ""}`);
  return false;
}

// ---- The shipped defaults, read from Rust, not re-typed here -----------------------------------

function rustConstant(name) {
  // Rust literals may carry underscores (`10_000`), so the capture strips them — reading the digits
  // out of a formatted number is the kind of thing that makes a gate silently measure nothing.
  const match = settings.match(new RegExp(`pub const ${name}: u\\d+ = ([\\d_]+);`));
  if (!match) {
    failures += 1;
    console.log(`  FAIL the default ${name} could not be read from crates/graphql/src/settings.rs`);
    return null;
  }
  return Number(match[1].replace(/_/g, ""));
}

const defaults = {
  max_depth: rustConstant("DEFAULT_MAX_DEPTH"),
  cost_budget: rustConstant("DEFAULT_COST_BUDGET"),
  max_aliases: rustConstant("DEFAULT_MAX_ALIASES"),
  max_fragments: rustConstant("DEFAULT_MAX_FRAGMENTS"),
  max_page_size: rustConstant("DEFAULT_MAX_PAGE_SIZE"),
  timeout_ms: rustConstant("DEFAULT_TIMEOUT_MS"),
};

check("every default was read from the Rust source", defaults.timeout_ms !== undefined);

// ---- Every field's grid, parsed out of the screen -----------------------------------------------

const fieldBlock = view.slice(view.indexOf("const FIELDS: Field[] = ["));
// Parse by SPLITTING on `key: "`, not by one cross-field regex.
//
// The looser patterns all had a false positive in the same run: a field's doc comment contains
// backticked expressions, and any pattern that can cross a field boundary pairs one field's `min`
// with the next field's `max` — which reported cost_budget's range as 1–200 and accused the screen
// of a defect it did not have, on the same run that found a real one. **A gate that invents a
// defect teaches the reader to distrust the gate**, so the parser is deliberately the dumbest one
// that cannot cross a boundary: split on the field marker and read each chunk on its own.
const chunks = fieldBlock.split(/(?=key: ")/).filter((part) => part.includes('key: "'));
const declared = chunks.map((part) => {
  const key = (part.match(/key: "(\w+)"/) || [])[1];
  const min = (part.match(/\n\s*min: (\d+),/) || [])[1];
  const max = (part.match(/\n\s*max: ([\d_]+),/) || [])[1];
  const step = (part.match(/\n\s*step: (\d+),/) || [])[1];
  return [key, min, max, step];
});

check(
  "all six fields were parsed out of the screen",
  declared.length === 6 && declared.every(([, min, max, step]) => min && max && step),
  `the gate parsed ${declared.length} of six, or one of them is missing min/max/step — a field this gate cannot see is a field it cannot hold`,
);

// Destructure WITHOUT the leading hole. The tuples are `[key, min, max, step]`; `[, key, …]`
  // skipped the key and read the numbers into it — which made every field report "no shipped
  // default", and the gate was red for a reason of its own making while proving nothing.
  for (const [key, min, max, step] of declared) {
    const value = defaults[key];
    if (value === undefined || value === null) {
      failures += 1;
      console.log(`  FAIL ${key} — no shipped default was found, so reachability cannot be asserted`);
      continue;
    }
    const low = Number(min);
    const high = Number(String(max).replace(/_/g, ""));
    const stride = Number(step);
  // HTML's rule: `step` is measured FROM `min`. `(value - min) % step == 0` is validity, and the
  // two halves below are what an operator actually meets — the spinner, and the range itself.
  const onGrid = (value - low) % stride === 0;
  const inRange = value >= low && value <= high;
  check(
    `${key}: the shipped default ${value} is on the grid (min ${low}, step ${stride})`,
    onGrid,
    `(value - min) % step === ${(value - low) % stride}, not 0 — the browser's validation refuses this value`,
  );
  check(
    `${key}: the shipped default ${value} is inside the range ${low}–${high}`,
    inRange,
    "the endpoint accepts it and the screen's own range does not",
  );
}

// ---- And the screen must not re-declare its own default -----------------------------------------

check(
  "the screen carries no hard-coded default of its own",
  !/DEFAULT_(MAX_DEPTH|COST_BUDGET|MAX_ALIASES|MAX_FRAGMENTS|MAX_PAGE_SIZE|TIMEOUT_MS)/.test(view),
  "a constant in the screen is a second source of truth, and the row nobody read was exactly that shape",
);

// =====================================================================================================
// PROVEN-TO-FAIL
// =====================================================================================================

function gridRed(text) {
  let red = 0;
  const block = text.slice(text.indexOf("const FIELDS: Field[] = ["));
  const fields = block
    .split(/(?=key: ")/)
    .filter((part) => part.includes('key: "'))
    .map((part) => {
      const key = (part.match(/key: "(\w+)"/) || [])[1];
      const min = (part.match(/\n\s*min: (\d+),/) || [])[1];
      const max = (part.match(/\n\s*max: ([\d_]+),/) || [])[1];
      const step = (part.match(/\n\s*step: (\d+),/) || [])[1];
      return [key, min, max, step];
    });
  if (fields.length !== 6) red += 1;
  if (fields.some(([, min, , step]) => !min || !step)) red += 1;
  // Same leading-hole bug as the reporting loop above, and it is WORSE here: `[, key, …]` reads
  // the numbers into `key`, so `defaults[key]` is `defaults[1]` — undefined — and every field hit
  // the `continue`. The checker reported the clean file green and every mutation green with it,
  // which is the `if (false)` defect wearing a checker. The five main checks caught it because they
  // share the reporting loop's fixed destructuring; nothing else could have.
  for (const [key, min, max, step] of fields) {
    const value = defaults[key];
    if (value === undefined || value === null) continue;
    if ((value - Number(min)) % Number(step) !== 0) red += 1;
    const high = Number(String(max).replace(/_/g, ""));
    if (value > high || value < Number(min)) red += 1;
  }
  if (/DEFAULT_(MAX_DEPTH|COST_BUDGET)/.test(text)) red += 1;
  return red;
}

console.log("\nproven-to-fail:");
const control = gridRed(view);
if (control !== 0) {
  failures += 1;
  console.log(`  FAIL control — the clean file reports ${control} failure(s)`);
} else {
  console.log("  ok   control — every default is on its grid");
}

// Set one field's `step`, editing ONLY that field's chunk. Returns the source unchanged when the
// chunk does not contain the line, so the caller's identity check catches a no-op mutation.
function setStep(text, key, value) {
  return editField(text, key, /(\n\s*)step: \d+,/, `$1step: ${value},`);
}
function setMin(text, key, value) {
  return editField(text, key, /(\n\s*)min: \d+,/, `$1min: ${value},`);
}
function editField(text, key, pattern, replacement) {
  const start = text.indexOf(`key: "${key}",`);
  if (start < 0) return text;
  const next = text.indexOf('key: "', start + 5);
  const end = next < 0 ? text.length : next;
  const chunk = text.slice(start, end);
  if (!pattern.test(chunk)) return text;
  return text.slice(0, start) + chunk.replace(pattern, replacement) + text.slice(end);
}

// The mutations edit the `step` line WITHIN each field's chunk, matched from the field's own
// marker — the first versions used `(key: "x",[\s\S]*?step: )1,` which is a cross-field pattern,
// and it silently matched nothing for two of the three fields whose chunks contain a comment
// between `min`/`max` and `step`. A mutation that does not change the file is the `if (false)`
// defect in another costume, which is why every one below is followed by an identity check.
const mutations = [
  ["cost_budget step 10 (the shipped defect)", (t) => setStep(t, "cost_budget", 10)],
  ["max_page_size step 10 (the second instance)", (t) => setStep(t, "max_page_size", 10)],
  ["timeout_ms step 500 (the third instance)", (t) => setStep(t, "timeout_ms", 500)],
  ["a field's min raised above its own default", (t) => setMin(t, "max_page_size", 200)],
  ["a screen-local default reintroduced", (t) => `${t}\nconst DEFAULT_MAX_DEPTH = 10;\n`],
];

for (const [label, mutate] of mutations) {
  const mutated = mutate(view);
  if (mutated === view) {
    failures += 1;
    console.log(`  FAIL ${label} — the mutation did not change the file`);
    continue;
  }
  const red = gridRed(mutated);
  if (red === 0) {
    failures += 1;
    console.log(`  FAIL ${label} — PROVEN NOT TO FAIL`);
  } else {
    console.log(`  ok   ${label} — ${red} check(s) went red`);
  }
}

console.log(`\n${checks} checks, ${mutations.length + 1} proven-to-fail cases`);
if (failures > 0) {
  console.log(`FAILED: ${failures}`);
  process.exit(1);
}
console.log("PASS");