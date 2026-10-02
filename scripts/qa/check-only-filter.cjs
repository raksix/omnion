#!/usr/bin/env node
/**
 * Validate a `--only` filter against the names the walkthrough actually knows.
 *
 * ## The failure this prevents, measured
 *
 * A focused pass is the cheapest way to close one request, and `--only` takes a comma-separated
 * list of route and depth-pass NAMES — `observability-traces`, not `/observability/traces` and not
 * `observability`. The two spellings are not interchangeable and the route list never had the
 * plural, so `--only=observability,secrets` — the natural way to write "the observability screens
 * and the secrets screens" — matches **nothing at all**.
 *
 * `walkthrough.cjs` already fails such a pass, loudly, with `unknown-pass-name` findings and an
 * `empty-pass` finding when it matches zero names. That check is in the report roll-up, which runs
 * at the very END of the pass. So the failure is detected only after the pass has:
 *
 *   1. queued for a QA slot (this box runs ONE concurrent pass, so it may wait up to 25 minutes),
 *   2. reset a QA database,
 *   3. booted three pm2 servers and waited for them to answer HTTP,
 *
 * and then walks no routes and produces no screenshot of the screens it was asked to prove. On a
 * contended box a typo in the filter is therefore a full-tick cost, discovered at the end, with the
 * slot held throughout — and the artifact directory left behind is indistinguishable from a real
 * pass that simply found nothing.
 *
 * This script performs the SAME name check against the SAME source file, and is called from
 * `run.sh` BEFORE the slot wait. A typo costs one second and no slot.
 *
 * ## Why the names are extracted from the file rather than listed here
 *
 * A second hardcoded list would drift from the route list, and the drift would be invisible in
 * exactly the way this check exists to make visible: it would reject a perfectly good name, or
 * accept one that no longer exists. So both the route names and the depth-pass names are read out
 * of `walkthrough.cjs` itself.
 *
 * Run: `node scripts/qa/check-only-filter.cjs <name,name>` (exit 0 = all names known)
 */
const fs = require("fs");
const path = require("path");

const src = fs.readFileSync(path.join(__dirname, "walkthrough.cjs"), "utf8");

/** Every route name in the `routes` array. */
function routeNames(source) {
  const block = source.slice(source.indexOf("const routes = ["));
  if (block < 0) return [];
  const end = block.indexOf("];");
  return [...block.slice(0, end).matchAll(/\bname:\s*"([^"]+)"/g)].map((m) => m[1]);
}

/** Every depth-pass name guarded by a `wants("…")` block. */
function depthPassNames(source) {
  return [...source.matchAll(/\bwants\(\s*"([^"]+)"\s*\)/g)].map((m) => m[1]);
}

/**
 * `MOBILE_NAMES` holds `mobile:<name>` spellings the roll-up also accepts; they are not routes,
 * but they are legitimate filter values and must not be reported as unknown.
 */
function mobileNames(source) {
  const start = source.indexOf("const MOBILE_NAMES = new Set(");
  if (start < 0) return [];
  const block = source.slice(start, source.indexOf(");", start));
  return [...block.matchAll(/"([^"]+)"/g)].map((m) => m[1]);
}

const known = new Set([...routeNames(src), ...depthPassNames(src)]);
const alsoKnown = new Set(mobileNames(src));

const filter = (process.argv[2] || "").trim();
if (!filter) {
  // No filter means a full pass, which is always valid.
  process.exit(0);
}

const names = filter
  .split(",")
  .map((n) => n.trim())
  .filter(Boolean);

const unknown = names.filter((n) => !known.has(n) && !alsoKnown.has(n));
if (unknown.length > 0) {
  console.error(`[qa] --only names that match no route and no depth pass: ${unknown.join(", ")}`);
  console.error(`[qa] walkthrough.cjs knows ${known.size} name(s).`);
  // Rank candidates by how much of the name actually matches, so the most likely intent is first.
  // A substring test alone ranks `observability` against every screen on the product, which is the
  // opposite of a suggestion: it is the whole route list. Prefix and substring both have to hold.
  const suggestions = [];
  for (const name of unknown) {
    const hits = [...known].filter(
      (candidate) => candidate.startsWith(name) || candidate.split("-").includes(name),
    );
    const tail = [...known].filter((candidate) => candidate.split("-").includes(name));
    const ranked = [...hits, ...tail].filter((c, i, a) => a.indexOf(c) === i);
    if (ranked.length > 0) suggestions.push(`${name} -> ${ranked.slice(0, 8).join(" | ")}`);
  }
  if (suggestions.length > 0) {
    console.error(`[qa] did you mean: ${suggestions.join("; ")}`);
  }
  process.exit(1);
}

process.exit(0);