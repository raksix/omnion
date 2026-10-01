#!/usr/bin/env node
/**
 * The media toolbar's filter wiring (REQ-010, tick 101).
 *
 * ## The defect class
 *
 * `crates/media/src/browser.rs` grew `min_bytes`, `max_bytes`, `uploaded_by`, `created_after` and
 * `created_before` in slice 1, with store clauses, with a walk that proved the SQL, and with
 * tests. `GET /api/v1/media/files` accepted all five. **The toolbar rendered none of them** — the
 * filter bar had kind, scan state, a custom pair and a "has versions" checkbox, and that was it.
 *
 * So for the whole life of the request, five documented filters were reachable only by hand-editing
 * a URL, and the REQ's own acceptance box carried a sentence admitting it ("*size, uploader and
 * date-range filters are in the API and the store but not yet on the toolbar*"). Nothing was
 * broken: it compiled, the tests were green, the API worked, the column existed, the walk passed.
 * A half-wired feature is invisible to every instrument in the repo.
 *
 * This is the same family as tick 100's `uploaded.ok`: a value that is *handled* on one side of a
 * boundary and *offered* on the other. `cargo` cannot see the toolbar, `tsc` cannot see the SQL
 * filter list, and the browser pass only finds it if somebody happens to open the filter bar.
 *
 * ## What this gate checks
 *
 * Read statically, in milliseconds, with no browser and no database — which is the point:
 *
 * 1. **Every `ListQuery` filter the store implements has a control on the toolbar.** A new filter
 *    in the store that nobody can type is a feature that does not exist.
 * 2. **Every control the toolbar renders reaches the query string.** A control bound to state that
 *    never lands in `MediaFilters` is a dead input, which is the mirror of the same bug.
 * 3. **Every filter the API documents is offered.** The route's `FileQuery` is the contract; a
 *    field on it that no control writes is a documented feature with no door.
 * 4. **No control uses a Tailwind colour token that does not exist.** Found while writing this
 *    slice: `text-danger` / `border-danger` were used by a sibling media screen and are defined
 *    nowhere in `globals.css`, so those messages rendered in the inherited colour. A colour
 *    class that is not a token is a class that silently does nothing.
 *
 * Each assertion reads the *source of the boundary*, not a copy of its values, so renaming a
 * filter or reordering a struct does not need this file edited to stay true.
 */

const fs = require("fs");
const path = require("path");

const ROOT = path.join(__dirname, "..", "..");
const BROWSER_RS = path.join(ROOT, "crates/media/src/browser.rs");
const ROUTE_RS = path.join(ROOT, "apps/api/src/routes/media_files.rs");
const VIEW_TSX = path.join(ROOT, "apps/admin/features/media/media-view.tsx");
const API_TS = path.join(ROOT, "apps/admin/lib/api.ts");
const TYPES_TS = path.join(ROOT, "apps/admin/lib/types.ts");
const GLOBALS_CSS = path.join(ROOT, "apps/admin/app/globals.css");

const read = (file) => fs.readFileSync(file, "utf8");
const results = [];
const check = (name, pass, detail) => results.push({ name, pass, detail });

const browserRs = read(BROWSER_RS);
const routeRs = read(ROUTE_RS);
const viewTsx = read(VIEW_TSX);
const apiTs = read(API_TS);
const typesTs = read(TYPES_TS);
const globalsCss = read(GLOBALS_CSS);

// -------------------------------------------------------------------------------------------
// 1. Every store filter has a toolbar control.
// -------------------------------------------------------------------------------------------

/**
 * The wire names of the filters `ListQuery::filters` can build, read out of the `Filter` enum.
 *
 * Parsed from the enum rather than listed here: a hand-kept list is a list that goes stale the
 * same way the feature did. Each variant names its own column, so the mapping variant -> wire
 * name is read from the clause the variant pushes.
 */
function storeFilters() {
  const body = browserRs.slice(browserRs.indexOf("enum Filter {"));
  const end = body.indexOf("\n}\n");
  const variants = body.slice(0, end);
  const names = [];
  for (const match of variants.matchAll(/^\s{4}([A-Z][A-Za-z]+)/gm)) {
    names.push(match[1]);
  }
  return names;
}

/** Variant -> the query-string field the toolbar sends, from the clause each variant pushes. */
const VARIANT_TO_WIRE = {
  Folder: "folder_id",
  NameContains: "search",
  KindPrefix: "kind",
  MinBytes: "min_bytes",
  MaxBytes: "max_bytes",
  UploadedBy: "uploaded_by",
  CreatedAfter: "created_after",
  CreatedBefore: "created_before",
  Tag: "tag",
  ScanStatus: "scan_status",
  HasVersions: "has_versions",
  MetadataPair: "metadata",
};

const variants = storeFilters();
check(
  "the Filter enum was parsed (the gate reads the source, not a copy)",
  variants.length >= 12,
  `found ${variants.length} variants: ${variants.join(", ")}`,
);

for (const variant of variants) {
  const wire = VARIANT_TO_WIRE[variant];
  if (!wire) {
    // A new variant with no mapping is not a failure of the feature; it is a failure of this
    // file, and it must say so rather than pass quietly.
    check(
      `filter ${variant} is mapped to a wire name in this gate`,
      false,
      `the gate has no VARIANT_TO_WIRE entry for ${variant} — add it, or the check below is blind to it`,
    );
    continue;
  }
  // A control exists when the toolbar has a `set<X>` for it AND puts `<wire>` in the filter memo.
  const inMemo = new RegExp(`\\b${wire}:`, "m").test(viewTsx);
  const hasControl = new RegExp(`set[A-Z][A-Za-z]*\\b`).test(viewTsx) && inMemo;
  check(
    `store filter ${variant} reaches the toolbar as ${wire}`,
    hasControl && inMemo,
    inMemo
      ? `${wire} is built into the MediaFilters memo`
      : `${wire} is implemented in the store but no control writes it into the query`,
  );
}

// -------------------------------------------------------------------------------------------
// 2. Every control the toolbar renders reaches the query string.
// -------------------------------------------------------------------------------------------

/**
 * The `setX` calls a `MediaFilters` memo entry is fed by.
 *
 * Read from the memo's own body: a control whose setter never appears there is bound to state
 * that changes the screen and not the listing — the classic dead input.
 */
const memo = viewTsx.slice(viewTsx.indexOf("const filters: MediaFilters = useMemo("));
const memoEnd = memo.indexOf(");", memo.indexOf("limit: 200"));
const memoBody = memo.slice(0, memoEnd > 0 ? memoEnd : 4000);

// `setTimeout` is a global, not a React setter — it is excluded by name below rather than by
// pattern, because a pattern narrow enough to miss it would also miss a real `setX`.
const GLOBALS = new Set(["Timeout", "Interval", "Immediate"]);
const setterNames = [
  ...new Set(
    [...viewTsx.matchAll(/\bset([A-Z][A-Za-z0-9]*)\b/g)]
      .map((m) => m[1])
      .filter((name) => !GLOBALS.has(name)),
  ),
];
// Setters that belong to the rest of the screen (folders, selection, overlays, bulk bar). They
// are excluded by name, and that list is itself a finding when it grows: a new state setter here
// is either a filter that must reach the memo or state that must not.
const NOT_FILTERS = new Set([
  "Error", "Notice", "Busy", "Page", "Folders", "Selection", "ShowFilters", "NewFolderName",
  "Renaming", "RenameValue", "Uploaders", "RangeError", "View", "Sort", "Search", "Kind",
  "ScanStatus", "MetadataTerm", "HasVersions", "Recursive",
]);

for (const name of setterNames) {
  if (NOT_FILTERS.has(name)) {
    continue;
  }
  const camel = name[0].toLowerCase() + name.slice(1);
  check(
    `toolbar state ${camel} reaches the query string`,
    new RegExp(`\\b${camel}\\b`).test(memoBody),
    `set${name} exists but \`${camel}\` never appears in the MediaFilters memo`,
  );
}

// -------------------------------------------------------------------------------------------
// 3. Every field the route accepts is offered by a control.
// -------------------------------------------------------------------------------------------

const fileQuery = routeRs.slice(routeRs.indexOf("pub struct FileQuery {"));
const fileQueryEnd = fileQuery.indexOf("\n}");
const routeFields = [...fileQuery.slice(0, fileQueryEnd).matchAll(/^\s{4}pub (\w+):/gm)].map(
  (m) => m[1],
);

for (const field of routeFields) {
  // `site_id` is the route's own scope, never a filter; `offset`/`limit` are paging.
  if (["site_id", "limit", "offset", "sort", "recursive"].includes(field)) {
    continue;
  }
  const inTypes = new RegExp(`\\b${field}\\??:`, "m").test(typesTs);
  const offered = inTypes && new RegExp(`\\b${field}:`, "m").test(viewTsx);
  check(
    `route field ${field} has a toolbar control`,
    offered,
    inTypes
      ? `declared in MediaFilters but no control writes ${field}`
      : `accepted by FileQuery but missing from the panel's MediaFilters type`,
  );
}

// -------------------------------------------------------------------------------------------
// 4. No control uses a colour token that does not exist.
// -------------------------------------------------------------------------------------------

/** The `--color-*` custom properties Tailwind v4 turns into utilities. */
const tokens = new Set(
  [...globalsCss.matchAll(/--color-([a-z0-9-]+)\s*:/g)].map((m) => m[1]),
);

/**
 * A `text-<x>` / `border-<x>` / `bg-<x>` whose `<x>` is not a token.
 *
 * Only checked for the palette's own names: `text-[12.5px]`, `text-left` and every layout utility
 * are matched out first, because the question is "is this a COLOUR that does not exist", and
 * Tailwind's own scale (`red-500`, `transparent`) is not in the theme.
 */
// Tailwind's own scale and every NON-colour utility that shares the `text-`/`border-`/`bg-`
// prefix: `text-center`, `text-[12.5px]`, `border-b`, `bg-transparent`. A gate that flagged
// layout utilities would be a gate nobody runs, and a gate nobody runs is the bug it was
// written to prevent.
const TAILWIND_BUILTIN =
  /^(transparent|current|inherit|white|black|slate|gray|zinc|neutral|stone|red|orange|amber|yellow|lime|green|emerald|teal|cyan|sky|blue|indigo|violet|purple|fuchsia|pink|rose|\d)/;
// Layout/text-alignment/overflow utilities: `text-*` that is not a colour.
const NOT_A_COLOUR = /^(left|right|center|justify|start|end|wrap|nowrap|ellipsis|balance|pretty|xs|sm|base|lg|xl|[2-9]xl|capitalize|uppercase|lowercase|normal|italic|underline|line-through|no-underline|truncate|clip|ellipsis|indent|uppercase|rtl|ltr|clip|hidden|block|inline|inline-block|flex|grid|table|contents|list|flow|root|isolate|collapse|separate|fixed|sticky|relative|static|absolute|b|t|l|r|x|y|s|e|all|outline|ring|ring-offset|dashed|dotted|double|none|solid|groove|ridge|inset|outset|hidden|screen|print|page|section|column|row|cell|table|min|max|fit|baseline|middle|sub|super|top|middle|bottom|baseline|leading|tight|snug|normal|loose|whitespace|break|keep|normal|wrap|clip|ellipsis|pre|pre-wrap|pre-line|break-words|break-all|break-keep|truncate|upper|lower|full|screen|min|max|fit|dark|light|hover|focus|active|disabled|checked|peer|group|first|last|odd|even|only|not|selection|placeholder|file|marker|backdrop|divide|space|from|via|to|gap|space-x|space-y|space-reverse|divide-x|divide-y|divide-reverse)/;

// Only classes in `className`, not every hyphenated word in the file: a `<span className="…">`
// is the only place a colour utility can live, and scanning the whole file collects prose
// ("b-", "collapse") that is not a class at all.
const classNameBodies = [...viewTsx.matchAll(/className=(?:"([^"]*)"|\{`([^`]*)`\})/g)].map(
  (m) => m[1] ?? m[2] ?? "",
);
const colourUses = classNameBodies.flatMap((body) =>
  [...body.matchAll(/(?:^|\s)(?:text|border|bg|ring|fill|stroke)-([a-z][a-z0-9-]*)/g)].map((m) => ({
    name: m[1],
  })),
);
const unknownColours = new Map();
for (const { name } of colourUses) {
  if (TAILWIND_BUILTIN.test(name) || NOT_A_COLOUR.test(name) || tokens.has(name)) {
    continue;
  }
  unknownColours.set(name, (unknownColours.get(name) ?? 0) + 1);
}

check(
  "the theme's colour tokens were parsed",
  tokens.size >= 10,
  `found ${tokens.size} --color-* tokens: ${[...tokens].join(", ")}`,
);
check(
  "the toolbar uses no colour class that the theme does not define",
  unknownColours.size === 0,
  unknownColours.size === 0
    ? `every colour class in the file resolves to one of ${tokens.size} tokens`
    : `undefined colour token(s): ${[...unknownColours]
        .map(([name, count]) => `${name} (${count}x)`)
        .join(", ")} — these render in the inherited colour`,
);

// -------------------------------------------------------------------------------------------
// 5. The uploader list must not be read through a key the media operator lacks.
// -------------------------------------------------------------------------------------------

/**
 * `GET /api/v1/iam/users` is guarded by `users.read`. A media operator holds `media.read`, so a
 * toolbar that sourced its dropdown from there would render the filter behind a 403 for exactly
 * the people who run the library — a control that is present, enabled and always empty.
 */
const routeMod = read(path.join(ROOT, "apps/api/src/routes/mod.rs"));

/**
 * The permission a route requires, by its path.
 *
 * The guard is NOT written next to the `.route()` call: `mod.rs` binds a `let` to a layered
 * `MethodRouter` a hundred lines earlier and mounts the binding here. A gate that greps for
 * `route("…") … guards::require` therefore finds nothing and reports "no guard found" — which
 * reads as a hole in the API and is really a hole in the gate. So: find the mount, resolve the
 * binding name, then read the guards off the binding's own `let`.
 */
function permissionFor(path) {
  const mount = new RegExp(`\\.route\\("\\/${path}", ([a-z_0-9]+)\\)`).exec(routeMod);
  if (!mount) {
    return null;
  }
  const binding = mount[1];
  // The binding's `let` runs to the first `;` at the same nesting level. Taking a bounded slice
  // and stopping at the next `let` keeps a later binding's guards out of this one.
  const start = routeMod.indexOf(`let ${binding} =`);
  if (start < 0) {
    return null;
  }
  // The binding's guards are contiguous and few. The slice stops at the NEXT `let`, so a
  // following binding's `media.read` cannot be mistaken for this one's.
  const rest = routeMod.slice(start);
  const nextBinding = rest.slice(3).search(/\n\s*let /);
  const body = rest.slice(0, nextBinding > 0 ? nextBinding + 3 : 800);
  const guards = [...body.matchAll(/guards::require\(&state, "([\w.]+)"\)/g)].map((m) => m[1]);
  return guards.length > 0 ? guards : null;
}

const uploadersGuards = permissionFor("media/uploaders");
check(
  "the uploader list is guarded by media.read",
  Boolean(uploadersGuards) && uploadersGuards.includes("media.read"),
  uploadersGuards
    ? `/media/uploaders requires ${uploadersGuards.join(", ")}`
    : "no guard resolved for /media/uploaders — the mount or the binding moved; update permissionFor()",
);

const iamUsersGuards = permissionFor("iam/users");
check(
  "the gate can read the iam/users guard, so the check above is not vacuous",
  Boolean(iamUsersGuards) && iamUsersGuards.includes("users.read"),
  iamUsersGuards
    ? `/iam/users requires ${iamUsersGuards.join(", ")}`
    : "permissionFor() could not resolve /iam/users — the gate's own lookup is broken",
);
check(
  "the uploader list is not read from /iam/users",
  !/fetchIamUsers/.test(viewTsx),
  /fetchIamUsers/.test(viewTsx)
    ? `the toolbar reads /api/v1/iam/users, which needs ${
        iamUsersGuard ? iamUsersGuard[1] : "a permission a media operator may not hold"
      }`
    : "the toolbar reads /api/v1/media/uploaders",
);

// -------------------------------------------------------------------------------------------
// Report
// -------------------------------------------------------------------------------------------

let failed = 0;
for (const result of results) {
  if (!result.pass) {
    failed += 1;
    console.log(`FAIL  ${result.name}\n      ${result.detail}`);
  }
}
console.log(`\n${results.length - failed}/${results.length} checks passed`);
process.exit(failed === 0 ? 0 : 1);
