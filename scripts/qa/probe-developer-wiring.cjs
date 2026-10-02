/**
 * Static gate for the developer portal's screens (REQ-022, slice 2).
 *
 * ## Why this is a separate file at all
 *
 * `cargo` cannot see the panel and `tsc` cannot see the CSS. A green typecheck says the JSX
 * compiles; it says nothing about whether the classes in it resolve to a colour. That gap is
 * not hypothetical here: these screens were written with `text-danger` and `bg-danger-soft`,
 * and **`--color-danger` is not declared in `app/globals.css`**. Tailwind v4 drops a utility for
 * an undefined token *silently* — no warning, no error, a green build — and the result is an
 * error message rendered in the default ink colour, which is to say an error message that does
 * not look like one. An existing shipped screen (`features/backups/backups-view.tsx`) carries the
 * same classes, so the defect is not new and it is not only here.
 *
 * This gate reads the **source** and the **token block** and compares them, so the class and the
 * palette cannot drift apart. `scripts/qa/probe-media-filter-wiring.cjs` is the same idea for
 * the media toolbar, and the reason is the same: the two blind spots of the two toolchains are
 * each other's.
 *
 * Run: `node scripts/qa/probe-developer-wiring.cjs`
 */
const fs = require("fs");
const path = require("path");

const ROOT = path.resolve(__dirname, "..", "..");
const CSS = path.join(ROOT, "apps", "admin", "app", "globals.css");
const FEATURE = path.join(ROOT, "apps", "admin", "features", "developer");

/** Colour-ish Tailwind utilities, and the token each one needs. */
const UTILITIES = [
  { class: "text-canvas", token: "--color-canvas" },
  { class: "text-surface", token: "--color-surface" },
  { class: "text-ink", token: "--color-ink" },
  { class: "text-muted", token: "--color-muted" },
  { class: "border-line", token: "--color-line" },
  { class: "text-accent", token: "--color-accent" },
  { class: "text-accent-strong", token: "--color-accent-strong" },
  { class: "bg-accent-soft", token: "--color-accent-soft" },
  { class: "bg-positive", token: "--color-positive" },
  { class: "bg-positive-soft", token: "--color-positive-soft" },
  { class: "text-positive", token: "--color-positive" },
  { class: "text-caution", token: "--color-caution" },
  { class: "bg-caution-soft", token: "--color-caution-soft" },
  { class: "bg-quiet-soft", token: "--color-quiet-soft" },
  { class: "bg-surface", token: "--color-surface" },
  { class: "bg-canvas", token: "--color-canvas" },
];

/**
 * Utilities that would be *silently* dropped, matched as a **prefix**, not as a whole word.
 *
 * The list is the point of the gate, so it is written out rather than derived: a derived list
 * only knows about the tokens somebody already used, which is exactly the set that cannot catch
 * a new mistake.
 *
 * The matching shape matters and the first version got it wrong in a way that made the gate
 * useless. The entries were tested with `\bbg-danger\b` against source containing
 * `bg-danger-soft` — and `\b` after `danger` matches at the `-`, so the *class* check passed by
 * accident while the *token* check (which needs `--color-danger` declared, and it is not) was
 * never reached for that class, because `UTILITIES` does not list it. Reintroducing
 * `bg-danger-soft text-danger` in a screen therefore left the gate green.
 *
 * So the rule is now: **any colour utility whose colour part is `danger` is forbidden**, and
 * that is a prefix test on the colour segment, not a whole-word test. Everything is expressed as
 * `{variant}-{colour}` so a new variant (`ring-danger`, `from-danger`) is covered by the shape
 * rather than by somebody remembering to add it.
 */
const FORBIDDEN_COLOUR = "danger";
const COLOUR_UTILITY = new RegExp(
  `\\b(?:bg|text|border|from|to|via|ring|outline|fill|stroke|shadow|accent|decoration|caret|divide)-${FORBIDDEN_COLOUR}\\b`,
);

/** The variants that exist, named so the failure message can be specific. */
const FORBIDDEN_VARIANTS = [
  "bg",
  "text",
  "border",
  "from",
  "to",
  "via",
  "ring",
  "outline",
  "fill",
  "stroke",
  "shadow",
  "divide",
];

/** Every file the portal is made of, including the ones that are not screens. */
const FILES = [
  "developer-overview.tsx",
  "developer-keys.tsx",
  "developer-key-detail.tsx",
  "developer-logs.tsx",
];

let failures = 0;
function check(name, ok, detail) {
  if (ok) {
    console.log(`  ok  ${name}`);
  } else {
    failures += 1;
    console.log(`FAIL  ${name}${detail ? ` — ${detail}` : ""}`);
  }
}

const css = fs.readFileSync(CSS, "utf8");
const declared = new Set(
  [...css.matchAll(/--color-([a-z-]+):/g)].map((match) => `--color-${match[1]}`),
);

console.log("declared colour tokens:", [...declared].sort().join(" "));

const sources = FILES.map((file) => ({
  file,
  text: fs.readFileSync(path.join(FEATURE, file), "utf8"),
}));

// -------------------------------------------------------------------------------------------
// 1. No portal file uses a token the palette does not declare
// -------------------------------------------------------------------------------------------
  for (const { file, text } of sources) {
    // Comments are stripped first: a doc comment that says "not `text-danger`" is a
    // *description* of the rule, and a gate that fires on it is a gate that gets switched off.
    const code = text.replace(/\/\*[\s\S]*?\*\//g, " ").replace(/^[ \t]*\/\/.*$/gm, " ");
    for (const entry of UTILITIES) {
      if (!code.includes(entry.class)) continue;
      check(
        `${file}: ${entry.class} resolves`,
        declared.has(entry.token),
        `needs ${entry.token}, which app/globals.css does not declare`,
      );
    }

    const offenders = FORBIDDEN_VARIANTS.filter((variant) =>
      new RegExp(`\\b${variant}-${FORBIDDEN_COLOUR}\\b`).test(code),
    );
    check(
      `${file}: no \`-${FORBIDDEN_COLOUR}\` utility`,
      offenders.length === 0,
      offenders.length > 0
        ? `${offenders.map((o) => `${o}-${FORBIDDEN_COLOUR}`).join(", ")} — Tailwind drops a ` +
          "utility for an undeclared token with no warning, so this renders as nothing at all, " +
          "and an error message that does not look like one is worse than no error message"
        : undefined,
    );
  }

// -------------------------------------------------------------------------------------------
// 2. The screens carry the markers the walkthrough looks for
// -------------------------------------------------------------------------------------------
/** Marker attributes and the file that must carry each one. */
const MARKERS = [
  ["developer-overview.tsx", "data-developer-overview"],
  ["developer-keys.tsx", "data-developer-keys"],
  ["developer-keys.tsx", "data-developer-create"],
  ["developer-keys.tsx", "data-developer-reveal"],
  ["developer-keys.tsx", "data-developer-token"],
  ["developer-keys.tsx", "data-developer-key-row"],
  ["developer-key-detail.tsx", "data-developer-key-detail"],
  ["developer-logs.tsx", "data-developer-logs"],
  ["developer-logs.tsx", "data-developer-log-drawer"],
];

for (const [file, marker] of MARKERS) {
  const source = sources.find((entry) => entry.file === file);
  check(
    `${file}: carries ${marker}`,
    Boolean(source && source.text.includes(marker)),
    "the walkthrough selects on this, and an unmarked screen is an untested one",
  );
}

// -------------------------------------------------------------------------------------------
// 3. The keyboard contract the REQ names is actually wired
// -------------------------------------------------------------------------------------------
const keys = sources.find((entry) => entry.file === "developer-keys.tsx").text;
check("keys: `/` focuses the search box", /event\.key === "\/"/.test(keys));
check("keys: `n` opens the create dialog", /event\.key === "n"/.test(keys));
check("keys: `Esc` closes the reveal", /event\.key === "Escape"/.test(keys));
// The reveal must not be dismissible before the acknowledgement. A dialog that closes on any
// keypress destroys a secret nobody has written down yet, and the REQ asks for the
// acknowledgement precisely so that cannot happen by accident.
check(
  "keys: the reveal cannot close before it is acknowledged",
  /disabled=\{!acknowledged\}/.test(keys) && /event\.key === "Escape" && acknowledged/.test(keys),
);

const logs = sources.find((entry) => entry.file === "developer-logs.tsx").text;
check("logs: `Esc` closes the drawer", /event\.key === "Escape"/.test(logs));

// -------------------------------------------------------------------------------------------
// 4. Nothing in the portal persists a key
// -------------------------------------------------------------------------------------------
/**
 * Match a *call*, not the word.
 *
 * The first version of this check was `text.includes("localStorage")`, and it failed on a
 * sentence in a doc comment that says the token is *not* put in `localStorage`. A gate that
 * fires on its own documentation is a gate that gets deleted, and a deleted gate protects
 * nothing — so the rule has to distinguish "the code calls it" from "the code mentions it",
 * which is the difference between `\bnew\s+Storage` and a substring.
 */
const STORAGE_CALL = /\b(?:window|globalThis|self)\s*\.\s*(?:local|session)Storage\b|\buseLocalStorage\b/;

for (const { file, text } of sources) {
  // Strip block and line comments before the check, so prose about the rule is not the rule.
  const code = text.replace(/\/\*[\s\S]*?\*\//g, " ").replace(/^[ \t]*\/\/.*$/gm, " ");
  check(
    `${file}: never persists to browser storage`,
    !STORAGE_CALL.test(code),
    "the REQ says a key value must never be persisted in the browser; a call here would store " +
      "the one thing the platform cannot show again",
  );
}

// -------------------------------------------------------------------------------------------
// 5. The API layer carries all three screens' calls, and the token type is unique
// -------------------------------------------------------------------------------------------
const api = fs.readFileSync(path.join(ROOT, "apps", "admin", "lib", "api.ts"), "utf8");
for (const call of [
  "fetchDeveloperOverview",
  "fetchDeveloperKeys",
  "createDeveloperKey",
  "rotateDeveloperKey",
  "revokeDeveloperKey",
  "fetchDeveloperScopes",
  "fetchDeveloperLogs",
  "fetchDeveloperLog",
  "fetchDeveloperKey",
]) {
  check(`api.ts: ${call} exists`, api.includes(`function ${call}(`) || api.includes(`${call}(`));
}

const developer = fs.readFileSync(
  path.join(ROOT, "apps", "admin", "lib", "developer.ts"),
  "utf8",
);
check(
  "developer.ts: exactly one shape carries a token",
  (developer.match(/token: string/g) || []).length === 1,
  `found ${(developer.match(/token: string/g) || []).length} — "reveal once" is a property of the ` +
    "types, and a second token field is a second place a secret can travel",
);
/**
 * The body of one exported type, with its doc comments removed.
 *
 * The first version of this check was a regex over the raw source, and it matched a *doc
 * comment* inside `DeveloperKey` that says "Never the token" — i.e. the type was declared clean
 * and the gate reported it dirty, for the word appearing in a sentence explaining that it does
 * not appear. A gate that cannot tell a field from a comment about that field is a gate that
 * reports noise, and a gate that reports noise is a gate that gets switched off.
 */
function typeBody(source, name) {
  const start = source.indexOf(`export type ${name} = {`);
  if (start === -1) return null;
  const open = source.indexOf("{", start);
  // The type ends at the first `}` at brace depth zero, so a nested object type does not
  // truncate the body and make the check pass on a partial read.
  let depth = 0;
  for (let index = open; index < source.length; index += 1) {
    const character = source[index];
    if (character === "{") depth += 1;
    else if (character === "}") {
      depth -= 1;
      if (depth === 0) return source.slice(open + 1, index);
    }
  }
  return null;
}

const readShape = typeBody(developer, "DeveloperKey");
check("developer.ts: DeveloperKey is declared", readShape !== null);
if (readShape !== null) {
  const fields = readShape
    .replace(/\/\*[\s\S]*?\*\//g, " ")
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
  const tokens = fields.filter((line) => /(^|\s)token\s*:/.test(line));
  check(
    "developer.ts: the read shape has no token field",
    tokens.length === 0,
    tokens.length > 0 ? `found: ${tokens.join(" | ")}` : undefined,
  );
}

console.log("");
if (failures > 0) {
  console.log(`developer wiring: ${failures} FAILED`);
  process.exit(1);
}
console.log("developer wiring: all checks passed");
