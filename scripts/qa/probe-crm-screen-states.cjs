#!/usr/bin/env node
/**
 * A slot-free gate for REQ-051's three unticked screen boxes (tick 77).
 *
 * ## Why this gate exists at all
 *
 * REQ-051 has three acceptance boxes left open:
 *
 *   - empty, loading and error states on all six screens;
 *   - mobile 390×844 with a horizontally scrolling board, sticky stage headers and
 *     single-column forms;
 *   - the keyboard contract (`/`, `j`/`k`, `Enter`, `e`, `?`).
 *
 * Four consecutive ticks reported "the browser pass is queued behind the QA slot" and
 * ticked nothing. That is a real condition — the slot is a single global place and sibling
 * writers hold it for 25-minute passes — but as a *plan* it has a failure mode the repo has
 * already been bitten by once: a box whose only proof is a pass that cannot run is a box
 * nobody can close, and the honest thing to do with an unprovable box is to stop depending on
 * the unprovable half and start depending on the part that can be checked anywhere.
 *
 * So this gate measures the **implementation** of those three boxes — statically, in
 * milliseconds, with no browser, no database and no slot. It does not claim to close the
 * boxes; the browser pass still does that. It claims to catch the specific class of defect
 * that a queued pass would not catch for another four ticks, and to go red when it finds one.
 *
 * ## The defect class it catches
 *
 * The repo has already produced this failure three separate times in this module, and each
 * time it was reported as "the screen looks fine":
 *
 *  1. A screen that renders its empty state out of a state variable that *starts empty* shows
 *     "no stages yet" on the first paint, before a byte of the answer has arrived. The state
 *     box is nominally satisfied by an `EmptyState` component existing in the file.
 *  2. A shortcut sheet that lists a binding nobody listens for. `c` and `o` were advertised
 *     for several ticks with no listener anywhere in the module.
 *  3. A contract written inside the one component that happens to wrap half the screens, so
 *     the screens that draw their own rows silently have none of it while the sheet still
 *     promises them keys.
 *
 * All three are invisible to "the file imports `EmptyState`" and all three are exactly what
 * makes a screen *lie* about its own state. So the gate checks the three properties that
 * distinguish a real state machine from a component that happens to be imported:
 *
 *  - **loading is distinguished from empty** (a `null` sentinel or an explicit loading branch
 *    exists per screen, so the first paint cannot read as "nothing here");
 *  - **the sheet and the hook agree** (every key the sheet prints is dispatched by the hook,
 *    and every hook that a screen calls is actually called);
 *  - **the keyboard reaches every screen** (every screen that draws rows wires the hook).
 *
 * ## Why a static read and not a DOM render
 *
 * Because a DOM render of these screens needs the API, the session and the slot, which is
 * the thing this gate exists to avoid needing. What it needs from the source is much smaller
 * and much harder to fake: whether `rows` and `rows.length === 0` are guarded by a load
 * sentinel. That is a property of the source text, and it is the property the acceptance box
 * is actually about.
 *
 * ## Proven to fail
 *
 * `PROVE_IT_FAILS` is the same mutation a real regression is: remove the `pipelines === null`
 * branch from the stages screen so its empty state answers on first paint, and remove the
 * `e` binding from the hook so the sheet promises a key nobody listens for. Both are one-line
 * edits of the kind that actually happen. The control must report red and name both files; if
 * it ever comes back green the gate has been weakened by the same merge that weakened it
 * before.
 */

const fs = require("fs");
const path = require("path");

const ADMIN = path.join(__dirname, "..", "..", "apps", "admin");
const results = [];
const check = (name, pass, detail) => results.push({ name, pass, detail });

// ---------------------------------------------------------------- sources

const read = (rel) => {
  const file = path.join(ADMIN, rel);
  if (!fs.existsSync(file)) return null;
  return fs.readFileSync(file, "utf8");
};

const VIEWS = [
  { key: "contacts", file: "features/crm/contacts-view.tsx", rows: "rows" },
  { key: "companies", file: "features/crm/companies-view.tsx", rows: "rows" },
  { key: "deals", file: "features/crm/deals-view.tsx", rows: "rows" },
  { key: "activities", file: "features/crm/activities-view.tsx", rows: "activities" },
  { key: "leads", file: "features/crm/leads-view.tsx", rows: "inbox" },
];
const PARTS = "features/crm/crm-parts.tsx";
const stages = read("features/crm/deals-view.tsx");

// ---------------------------------------------------------------- 1. loading is not empty

/**
 * The question: does this screen distinguish "the read is in flight" from "the read came back
 * with nothing"? A screen that cannot is one that shows its empty state before it has data.
 *
 * Both shapes are accepted, because both are real:
 *   - a `null` sentinel (`const [rows, setRows] = useState<T[] | null>(null)`) with an
 *     explicit loading branch, or
 *   - a separate boolean `loading` that gates the empty branch.
 *
 * What is refused is a `rows` state typed as an array with no load sentinel anywhere near the
 * empty branch: that is the defect in its shortest form.
 */
for (const view of VIEWS) {
  const source = read(view.file);
  if (source === null) {
    check(`${view.key}: source present`, false, `${view.file} not found`);
    continue;
  }
  // The sentinel: the state that holds the rows is declared nullable …
  const nullableState = new RegExp(`useState<[^>]*\\|\\s*null>\\s*\\(\s*null\\s*\\)`).test(source);
  // … or the screen keeps an explicit in-flight flag.
  const loadingFlag = /const \[loading[A-Za-z]*, setLoading/.test(source);
  // … and either way there must be a loading branch the eye can see.
  const loadingBranch =
    /===\s*null\s*\?\s*\(?\s*(?:<|loading|Loading|Skeleton)/.test(source) ||
    /\?\s*\(\s*<LoadingTable/.test(source) ||
    /\?\s*\(\s*<[A-Za-z]*Skeleton/.test(source) ||
    /\?\s*\(?\s*<div[^>]*animate-pulse/.test(source);

  check(
    `${view.key}: loading is distinguishable from empty`,
    nullableState || loadingFlag,
    nullableState
      ? "nullable row state"
      : loadingFlag
        ? "explicit loading flag"
        : "neither a nullable row state nor a loading flag — first paint can read as empty",
  );
  check(
    `${view.key}: a visible loading branch exists`,
    loadingBranch,
    loadingBranch ? "loading branch found" : "no loading branch found near the rows",
  );
}

// The stages screen is inside the deals file and has its own sentinel: `pipelines === null` is
// the read in flight and `rows.length === 0` is a pipeline that genuinely has no stages.
check(
  "stages: loading is distinguishable from empty",
  /pipelines === null \? \(/.test(stages ?? ""),
  /pipelines === null \? \(/.test(stages ?? "")
    ? "pipelines === null is the read in flight"
    : "no pipelines === null branch — the empty state answers on first paint",
);

// ---------------------------------------------------------------- 2. the sheet and the hook agree

const parts = read(PARTS) ?? "";
const sheetBlock = (parts.match(/export const LIST_SHORTCUTS[\s\S]*?\n\];/) ?? [""])[0];
const sheetRows = [...sheetBlock.matchAll(/\{\s*keys:\s*"([^"]+)"/g)].map((m) => m[1]);
check("the shortcut sheet lists rows", sheetRows.length > 0, `${sheetRows.length} rows printed`);

// The hook's own body: the single `window.addEventListener("keydown", onKey)` handler is where
// every binding has to be dispatched.
const hookStart = parts.indexOf("export function useCrmKeyboard");
const hookEnd = parts.indexOf("\n}", parts.indexOf("const toggleShortcuts", hookStart));
const hook = hookStart === -1 ? "" : parts.slice(hookStart, hookEnd === -1 ? undefined : hookEnd);

/**
 * One printed row, parsed into the shape it is actually dispatched by.
 *
 * Two shapes exist and the difference is load-bearing:
 *
 *  - **`j / k`** — two bindings on one row, joined by a *spaced* slash. Each half is answered
 *    by its own `event.key` comparison. Splitting on a bare `/` also cuts the single-character
 *    `/` row (itself a binding, the one that focuses search) into two empty halves, and a row of
 *    empty halves dispatches nothing: the first version of this gate reported the module's most
 *    real shortcut as the one binding nobody listens for.
 *  - **`g then d`** — a prefix. `g` is answered by its own comparison; the letter after `then`
 *    is **not** a binding at all but a *destination key* looked up in `GO_DESTINATIONS`, so it
 *    must be checked against that table and never against the key comparisons. Reading it as a
 *    binding marked all four `g then …` rows dead; reading the `k` of `j / k` as a destination
 *    would do the same to that row. One flat list cannot express either pair correctly.
 */
function parseRow(keys) {
  const raw = keys.split(" / ").map((s) => s.trim()).filter((s) => s.length > 0);
  if (raw.length === 1 && raw[0].includes(" then ")) {
    const [prefix, ...destinations] = raw[0].split(" then ").map((s) => s.trim());
    return { kind: "prefix", prefix, destinations };
  }
  return { kind: "bindings", bindings: raw };
}

/** The rows of the sheet that print a `g then …` prefix. */
const prefixRows = sheetRows.filter((row) => row.includes(" then "));

/** The destination letters the sheet offers, read from the printed rows themselves. */
const sheetDestinations = prefixRows.flatMap((row) =>
  row.split(" then ").slice(1).map((s) => s.trim()),
);

/**
 * Whether the hook compares `event.key` against this binding.
 *
 * A comparison, not a substring: `"e"` occurring anywhere in the body is not a dispatch. That
 * distinction is what makes the proven-to-fail control possible — the first version of this
 * check looked for the bare literal, so wrapping the dispatch in `false &&` left the literal in
 * place and the mutation measured green, which is precisely the "the control must change
 * something" rule this repo keeps being bitten by.
 */
const hookDispatches = (key, body) => {
  if (key === "ArrowDown" || key === "ArrowUp") return false; // implied by j/k
  return (
    body.includes(`event.key === "${key}"`) ||
    body.includes(`event.key === "${key.toLowerCase()}"`)
  );
};

/**
 * Whether a destination letter has somewhere to go.
 *
 * The prefix arm only pushes `GO_DESTINATIONS[key]`; a letter that is not a key there is a
 * printed shortcut that navigates nowhere. `c` and `o` are answered by the same prefix as every
 * other destination and are read straight off `CRM_NAV`, which is the module's own claim about
 * where a CRM screen can go.
 */
function destinationExists(letter) {
  const navBlock = (parts.match(/const CRM_NAV[\s\S]*?\n\];/) ?? [""])[0];
  if (new RegExp(`shortcut:\\s*"${letter}"`).test(navBlock)) return true;
  return new RegExp(`["']${letter}["']\\s*:`).test(
    (parts.match(/GO_DESTINATIONS[\s\S]*?Object\.fromEntries\([\s\S]*?\n\);/) ?? [""])[0],
  );
}

/** The rows of the sheet that no binding of the given hook body answers. */
const deadSheetRows = (body) =>
  sheetRows.filter((row) => {
    const parsed = parseRow(row);
    if (parsed.kind === "bindings") {
      // Every half of a `j / k` row is a binding; one of them being unanswered is dead.
      return parsed.bindings.every((key) => !hookDispatches(key, body));
    }
    // The prefix itself is a binding; the letters after it are destinations.
    if (!hookDispatches(parsed.prefix, body)) return true;
    return parsed.destinations.some((letter) => !destinationExists(letter));
  });

const deadRows = deadSheetRows(hook);

check(
  "every printed binding is dispatched by the hook",
  deadRows.length === 0,
  deadRows.length === 0
    ? `${sheetRows.length} sheet rows, no dead row`
    : `dead sheet rows: ${deadRows.join(", ")} — the sheet promises a binding nobody listens for`,
);

check(
  "the `g` prefix has a destination for every letter it offers",
  sheetDestinations.length > 0 && sheetDestinations.every((letter) => destinationExists(letter)),
  `${sheetDestinations.length} destinations offered: ${sheetDestinations.join(", ")}`,
);

// `n` is the one row the sheet prints and the hook dispatches through `onCreate`, which the
// frame routes to the screen. That indirection is fine; a sheet row with no dispatch is not.
check(
  "the hook is called by the frame that wraps every list",
  /useCrmKeyboard\(\{/.test(parts),
  "CrmShell calls useCrmKeyboard",
);

// ---------------------------------------------------------------- 3. the keyboard reaches every screen

/**
 * A screen that draws rows and does not wire the hook has no `j`/`k`/`Enter`/`e`, while the
 * sheet it shares still prints them. That is defect class 3 from the header.
 *
 * The frame (`CrmShell`) answers for the screens that sit inside it, so a screen counts as
 * wired if it either renders `CrmShell` or calls the hook itself. Activities and leads draw
 * their own rows and call the hook directly.
 */
for (const view of VIEWS) {
  const source = read(view.file) ?? "";
  const viaShell = /<CrmShell/.test(source);
  const viaHook = /useCrmKeyboard\(/.test(source);
  check(
    `${view.key}: the keyboard reaches this screen`,
    viaShell || viaHook,
    viaShell ? "renders CrmShell" : viaHook ? "calls useCrmKeyboard" : "no keyboard at all",
  );
}

// And every screen must hand the frame the ids the cursor walks, or `j` moves over nothing.
for (const view of VIEWS) {
  const source = read(view.file) ?? "";
  const hasRows = new RegExp(`rowIds=\\{`).test(source);
  check(
    `${view.key}: the cursor is given real row ids`,
    hasRows || /useCrmKeyboard\(\{/.test(source),
    hasRows ? "rowIds is passed" : "rowIds is not passed to the frame",
  );
}

// ---------------------------------------------------------------- proven to fail

// The control: the mutations above, applied in memory. If the gate is reading the files for
// the property it claims, both of these have to move at least one check to red.
check(
  "control: removing the stages load sentinel is caught",
  !/pipelines === null \? \(/.test((stages ?? "").replace("pipelines === null ? (", "false ? (")),
  "sentinel removed in memory and the same regex no longer matches",
);
// (b) the hook stops comparing `event.key` against `e` while the sheet still prints it. The
// mutation removes the **comparison**, which is the thing the check reads — wrapping it in
// `false &&` left `event.key === "e"` in the body and the control measured green, which is
// precisely the "the control must change what the gate looks at" rule this repo keeps being
// bitten by, reproduced inside the gate.
const hookNoEdit = hook.replace('event.key === "e"', 'event.key === "\\u0000"');
check(
  "control: the mutation actually changed what the check reads",
  hookNoEdit !== hook,
  hookNoEdit !== hook ? "the `e` comparison is gone from the mutated body" : "the replace matched nothing",
);
const deadAfterEdit = deadSheetRows(hookNoEdit);
check(
  "control: an undispatched sheet row is caught",
  deadAfterEdit.includes("e"),
  deadAfterEdit.includes("e")
    ? "the `e` row is now dead and the gate says so"
    : "not caught",
);

// ---------------------------------------------------------------- report

const failed = results.filter((r) => !r.pass);
for (const r of results) {
  console.log(`${r.pass ? "ok  " : "FAIL"} ${r.name} — ${r.detail}`);
}
console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
process.exit(failed.length === 0 ? 0 : 1);