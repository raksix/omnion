/**
 * Every screen in the walkthrough's desktop inventory must also be in its 390 px inventory.
 *
 * REQ-064's acceptance criterion 18 reads "All new screens render at 390 px without horizontal
 * scroll and the walkthrough reports zero high findings." For two ticks that box stayed unticked
 * because the passes that could answer it kept losing their tab to a sibling's walk — and the
 * honest answer to that was not "wait for a quieter box", because the box is never quiet. It was
 * that the criterion was STRUCTURALLY UNMEASURABLE: `walkthrough.cjs` keeps two inventories, the
 * desktop `routes` list and the `mobileRoutes` list, and twelve of this wave's screens
 * (`/blocks`, `/patterns`, `/page-templates`, `/menus`, `/publishing/queue`, `/forms`, `/seo`,
 * `/comments`, `/newsletter`, `/themes`, `/themes/minimal/builder`, `/themes/upload`) were in
 * the first and not the second.
 *
 * The failure mode is quiet by construction. A missing route does not error, does not print red
 * and does not set a non-zero exit code — it is simply never opened at 390 px, so the pass
 * reports a clean sheet for a screen it never looked at. The tick that added `/menus` believed it
 * was measured because `/menus` is walked; `/menus` was walked at 1440 px.
 *
 * That is why this is a gate rather than a comment. Two lists that must agree, maintained by
 * hand, will not stay in agreement, and the divergence is invisible in every other gate the
 * project has: `node --check` proves the file parses, the pass proves the routes it opened work,
 * and nothing either of them checks the screen was opened at all.
 *
 * Two things are deliberately NOT failures here:
 *
 *   - A route in `mobileRoutes` but not in `routes` is ALLOWED. Thirteen security, analytics and
 *     health screens are mobile-only by design — their desktop layout is covered by other
 *     passes, and requiring them in the desktop list would make this gate push screens INTO the
 *     desktop walk rather than out of a hole.
 *   - A screen whose address carries a record id (`/media/<id>`, `/menus/<id>/edit`) is measured
 *     by a depth pass rather than by either inventory, so it is not listed in either. Those are
 *     the screens whose depth passes call `matchedOnly.add(...)` themselves.
 */
const fs = require("node:fs");
const path = require("node:path");

const SRC = path.join(__dirname, "walkthrough.cjs");
const source = fs.readFileSync(SRC, "utf8");

let failures = 0;
function check(name, cond, detail) {
  if (cond) {
    console.log(`  ok  ${name}`);
  } else {
    failures++;
    console.log(`  FAIL ${name}${detail ? ` — ${detail}` : ""}`);
  }
}

/**
 * Read one inventory out of the pass source.
 *
 * Read from the SOURCE rather than from a copy of the list, for the reason `test-only-filter.cjs`
 * states and this gate needs just as much: a duplicated list is a second thing to forget, and
 * forgetting it makes the gate green while the pass keeps its hole.
 *
 * `sliceLiteral` is where this gate's first version was wrong, and it is worth stating because it
 * is the exact trap this file exists to catch, applied to itself. Scanning for the matching `]`
 * by counting brackets alone reads a bracketed word inside a COMMENT as an array terminator:
 * `walkthrough.cjs`'s inventories are full of comments that name other lists, and the naive scan
 * stopped early, then re-found a second `[` and reported **73 desktop routes** for a file that
 * declares 42 — a gate comparing two inventories where one of them was a truncated slice of both,
 * quietly reporting on screens it had not read. The count was the only tell, which is why every
 * assertion below prints the number it read.
 *
 * So the scanner is a real one: it steps the characters, tracks string and template literals, and
 * skips `//` and block comments. A `]` inside a string or a comment is not a bracket.
 */
function sliceLiteral(name) {
  const decl = source.match(new RegExp(`^\\s*const ${name} = \\[`, "m"));
  if (!decl) throw new Error(`walkthrough.cjs no longer declares \`${name}\` — screen-coverage must be updated`);
  const start = source.indexOf("[", decl.index);

  let depth = 0;
  let i = start;
  let inStr = null; // '"' | "'" | "`" while inside a literal
  while (i < source.length) {
    const ch = source[i];
    const next = source[i + 1];
    if (inStr) {
      if (ch === "\\") {
        i += 2;
        continue;
      }
      if (ch === inStr) inStr = null;
      i += 1;
      continue;
    }
    if (ch === "/" && next === "/") {
      i = source.indexOf("\n", i);
      if (i < 0) throw new Error(`unterminated comment inside \`${name}\``);
      continue;
    }
    if (ch === "/" && next === "*") {
      const close = source.indexOf("*/", i + 2);
      if (close < 0) throw new Error(`unterminated block comment inside \`${name}\``);
      i = close + 2;
      continue;
    }
    if (ch === '"' || ch === "'" || ch === "`") {
      inStr = ch;
      i += 1;
      continue;
    }
    if (ch === "[") depth += 1;
    else if (ch === "]") {
      depth -= 1;
      if (depth === 0) return source.slice(start, i + 1);
    }
    i += 1;
  }
  throw new Error(`could not find the end of \`${name}\``);
}

function inventory(name) {
  const list = new Function(`return ${sliceLiteral(name)}`)();
  if (!Array.isArray(list) || list.length === 0) {
    throw new Error(`\`${name}\` evaluated to ${JSON.stringify(list)} — an empty inventory proves nothing`);
  }
  return list;
}

let routes;
let mobileRoutes;
try {
  routes = inventory("routes");
  mobileRoutes = inventory("mobileRoutes");
} catch (e) {
  console.log(`  FAIL could not read the inventories — ${e.message}`);
  console.log(`\nFAIL (1) — ${SRC}`);
  process.exit(1);
}

const mobileName = new Set(mobileRoutes.map((r) => r.name));

console.log(`1. both inventories were read (${routes.length} desktop, ${mobileRoutes.length} at 390 px)`);
check("the desktop inventory is not empty", routes.length > 0);
check("the 390 px inventory is not empty", mobileRoutes.length > 0);
// The scanner's own regression: a comment-aware read must not swallow the whole file. Both lists
// are declared in the same file and both are short, so a count that grew past the number of
// `{ path:` entries in either is a scanner that ran off the end of its own declaration.
const declared = (source.match(/path:\s*"/g) || []).length;
check(
  "the scanner stopped inside its own declaration",
  routes.length + mobileRoutes.length <= declared,
  `read ${routes.length + mobileRoutes.length} entries from ${declared} declared`,
);

console.log("\n2. every desktop route is measured at 390 px");
const unmeasured = routes.filter((r) => !mobileName.has(r.name));
check(
  "no screen is walked at 1440 px only",
  unmeasured.length === 0,
  unmeasured.map((r) => `${r.name} (${r.path})`).join(", "),
);

console.log("\n3. the two inventories agree about PATH, not just about name");
// A name that appears in both lists at two different paths means the pass measured a screen that
// is not the screen the route list claims to walk, which reads as coverage while measuring the
// wrong URL. This is the one place a set comparison would have passed.
const desktopPath = new Map(routes.map((r) => [r.name, r.path]));
const mismatched = mobileRoutes
  .filter((r) => desktopPath.has(r.name) && desktopPath.get(r.name) !== r.path)
  .map((r) => `${r.name}: desktop ${desktopPath.get(r.name)} vs 390px ${r.path}`);
check("shared names carry the same path on both sides", mismatched.length === 0, mismatched.join(", "));

console.log("\n4. duplicates are impossible to read as coverage");
// One screen entered twice is two screenshots of one screen. A list holding the same name twice
// would also make `matchedOnly.add()` and the unmatched-name roll-up report a name that ran when
// it ran once.
const dupes = (list) => {
  const seen = new Set();
  const out = [];
  for (const r of list) {
    if (seen.has(r.name)) out.push(r.name);
    seen.add(r.name);
  }
  return out;
};
check("no duplicate name in the desktop list", dupes(routes).length === 0, dupes(routes).join(", "));
check("no duplicate name in the 390 px list", dupes(mobileRoutes).length === 0, dupes(mobileRoutes).join(", "));

console.log("\n5. the gate can fail — the holes it was written for are closed");
// A gate that cannot fail is a comment. The screens the gate was written for are re-asked as a
// predicate, so the assertion is about the CHECK rather than about today's file contents.
const KNOWN_GAP = ["blocks", "patterns", "page-templates", "menus", "publishing-queue", "forms",
  "seo", "comments", "newsletter", "themes", "theme-builder", "theme-upload"];
const stillMissing = KNOWN_GAP.filter((n) => !mobileName.has(n));
check(
  "the twelve screens that were measured at 1440 px only are now in the 390 px list",
  stillMissing.length === 0,
  stillMissing.join(", "),
);
check("all twelve were screens the desktop list really walks",
  KNOWN_GAP.every((n) => desktopPath.has(n)),
  KNOWN_GAP.filter((n) => !desktopPath.has(n)).join(", "));
// The predicate itself: re-run it against a list with one screen REMOVED and require it to flag
// exactly that screen. A predicate that cannot flag the known hole is not a gate.
const predicate = (desktop, mobile) => desktop.filter((r) => !new Set(mobile.map((x) => x.name)).has(r.name));
const probed = predicate(routes, mobileRoutes.filter((r) => r.name !== "menus"));
check(
  "the predicate flags a screen removed from the 390 px list",
  probed.length === 1 && probed[0].name === "menus",
  JSON.stringify(probed.map((r) => r.name)),
);
check("and it reports none against the file as it stands", predicate(routes, mobileRoutes).length === 0);

const verdict = failures === 0 ? "PASS" : `FAIL (${failures})`;
console.log(`\n${verdict} — ${SRC}`);
process.exit(failures === 0 ? 0 : 1);
