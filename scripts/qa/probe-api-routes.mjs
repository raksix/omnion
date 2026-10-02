#!/usr/bin/env node
/**
 * probe-api-routes.mjs — every API path the walkthrough fetches must be a path the router mounts.
 *
 * ## Why this gate exists
 *
 * Tick 89 found that `keyboard-pass` fetched `/api/v1/workflows/{id}/runs`, which **no route in
 * this server answers** (the run list is `/executions`). The fetch 404'd, `if (!response.ok)
 * return null` swallowed it, and `runsAfterKey` was structurally `null` on every run since the
 * row was written — a gate that could not go green reading exactly like a gate that could not go
 * red. Two earlier ticks in this REQ found the same CLASS in another surface: a probe marker the
 * product never renders (tick 86, four sites) and a selector read off an attribute written as a
 * *value* rather than a name (tick 87, two sites).
 *
 * **Fixing the site a report names leaves every unnamed site holding the defect.** That is this
 * REQ's standing rule, and the reason this is a sweep rather than one more assertion in
 * `keyboard-pass-row.test.ts` (which now covers its own row — this covers the other ~40 paths).
 *
 * ## What makes it trustworthy
 *
 *  * The route list is read from `apps/api/src/routes/mod.rs`, the source axum is built from. A
 *    hand-kept allowlist of "the paths that are real" is the same defect one layer out and drifts
 *    the first time a sibling mounts a route.
 *  * **Path PARAMETERS collapse to one token** (`/files/{id}` ≡ `/files/${x}`): the router and the
 *    walkthrough spell the same route differently, and a comparison that does not normalise
 *    reports every templated path as broken.
 *  * **Query strings are dropped** — `/x?limit=5` is the route `/x`.
 *  * **A site whose leading segment is interpolated is DEFERRED, not reported.** A fetch built on
 *    a base URL (`${base}/api/v1/auth/login`) has no path this file can resolve, and calling it
 *    broken is a false positive that teaches the reader to ignore the sweep.
 *  * **A control that bites.** `--self-test` proves the matcher resolves a mounted path and does
 *    NOT resolve the one that shipped the defect, and that the parameter normalisation compares
 *    equal. A sweep that matched nothing would otherwise print `0 unresolved` and be
 *    indistinguishable from a clean bill of health.
 *
 * ## This gate was five wrong versions before it was right
 *
 * Recorded because every one of them reported a clean bill of health while wrong, which is this
 * REQ's standing trap:
 *   1. `.map((m) => m.group(1))` over a `matchAll` result — `matchAll` yields arrays, not match
 *      objects, so the sweep threw rather than reporting.
 *   2. `m.start` on a `matchAll` result is `undefined`, so **every finding carried the same line
 *      number** (the last). Two overlapping regexes then collected each site twice.
 *   3. The self-test compared a `/api/v1/…` spelling against a `/…` spelling directly, so it
 *      failed on CORRECT normalisation — the gate's own control was wrong before the gate was.
 *   4. A literal-path regex matched *inside* URLs (`http://127.0.0.1:${port}/api/v1/…`), which
 *      is why an external base URL appeared as a site.
 *   5. A prefix match accepted any mounted route that merely STARTED WITH a site's shape, which
 *      would accept `/workflows/{id}` for a fetch of `/workflows`. (The one legitimate prefix case
 *      — a concatenated path — is deferred instead.)
 *
 * Usage: `node scripts/qa/probe-api-routes.mjs [--json] [--self-test]`
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const WALK = path.join(ROOT, "scripts/qa/walkthrough.cjs");
const ROUTES = path.join(ROOT, "apps/api/src/routes/mod.rs");

const walk = readFileSync(WALK, "utf8");
const routesSrc = readFileSync(ROUTES, "utf8");

const asJson = process.argv.includes("--json");
const selfTest = process.argv.includes("--self-test");

/** Collapse path parameters to `~` and drop the query string and trailing slash. */
const shape = (p) =>
  p
    .split("?", 1)[0]
    .replace(/\/+$/, "")
    .replace(/\$\{[^}]*\}/g, "~")
    .replace(/\{[^}]*\}/g, "~");

/** Every `.route("…")` path axum is given, normalised the same way. */
const mounted = new Set(
  [...routesSrc.matchAll(/\.route\(\s*"([^"]+)"/g)].map((m) => shape(m[1])),
);

const lineOf = (index) => walk.slice(0, index).split("\n").length;

/**
 * The first argument of every `fetch(…)` / `new URL(…)`, captured up to its closing quote.
 * ONE regex, because two overlapping ones collected each site twice (defect 2 above).
 */
const CALL = /(?:fetch|new URL)\(\s*[`"']([^`"']*)[`"']/g;
const sites = new Map(); // shape -> { deferred, lines: Set, raw }
const add = (raw, index) => {
  if (!raw.includes("/")) return;
  // A leading `${…}` is a base URL: the path is decided at runtime, so it is not this gate's
  // to resolve (defect 5). An INTERPOLATION in the middle is fine and stays resolved.
  // **A trailing `${…}` is the same case in the other direction** — ``/api/v1/scim/v2${path}``
  // is a prefix the CALLER extends (`call("POST", "/Users", …)`), so its final segment is only
  // known at runtime. Resolving the prefix alone reports a real, working row as broken, which is
  // the false positive that trains a reader to skip the sweep. Both are listed, not hidden.
  const deferred = raw.startsWith("${") || /\$\{[^}]*\}$/.test(raw);
  if (/^[a-z]+:\/\//i.test(raw)) return; // an absolute URL is not this server's API (defect 4)
  const s = shape(raw);
  if (!s.startsWith("/api/")) return;
  if (!sites.has(s)) sites.set(s, { deferred, lines: new Set(), raw });
  sites.get(s).lines.add(lineOf(index));
};
for (const m of walk.matchAll(CALL)) add(m[1], m.index);

const resolve = (s) => {
  const cands = [s];
  // The walkthrough spells the API prefix; the router mounts under the nest it is layered into.
  if (s.startsWith("/api/v1/")) cands.push(s.slice("/api/v1".length));
  return cands.some((c) => mounted.has(c));
};

const all = [...sites.entries()]
  .map(([s, v]) => ({ shape: s, deferred: v.deferred, raw: v.raw, lines: [...v.lines].sort((a, b) => a - b) }))
  .sort((a, b) => a.shape.localeCompare(b.shape));
const deferred = all.filter((s) => s.deferred).sort((a, b) => a.shape.localeCompare(b.shape));
const unresolved = all
  .filter((s) => !s.deferred && !resolve(s.shape))
  .map((s) => ({ shape: s.shape, lines: s.lines }))
  .sort((a, b) => a.shape.localeCompare(b.shape));

if (selfTest) {
  let ok = true;
  // 1. The path that shipped the defect must NOT resolve.
  const bad = resolve(shape("/api/v1/workflows/${id}/runs"));
  console.log(
    `${bad ? "FAIL" : "PASS"} — /workflows/{id}/runs resolves? ${bad} (want false: it is not mounted)`,
  );
  if (bad) ok = false;
  // 2. The real route must resolve.
  const good = resolve(shape("/api/v1/workflows/${id}/executions"));
  console.log(`${good ? "PASS" : "FAIL"} — /workflows/{id}/executions resolves? ${good} (want true)`);
  if (!good) ok = false;
  // 3. Parameter normalisation is the comparison the whole gate rests on.
  const normalised =
    shape("/api/v1/workflows/${id}/graph").replace("/api/v1", "") === shape("/workflows/{id}/graph");
  console.log(`${normalised ? "PASS" : "FAIL"} — a templated path matches the router's spelling`);
  if (!normalised) ok = false;
  // 4. A PREFIX must not satisfy a longer site (defect 5). `/workflows/{id}/runs` is not
  //    satisfied by the mounted `/workflows/{id}`, which is the whole reason tick 89's defect
  //    survived: the natural "is there a route that starts with this" question answers YES.
  const prefixOk = !resolve(shape("/api/v1/workflows/~/runs")) && resolve(shape("/api/v1/workflows/~/graph"));
  console.log(
    `${prefixOk ? "PASS" : "FAIL"} — a sibling route does not satisfy an unmounted one ` +
      `(/workflows/{id} does not answer for /workflows/{id}/runs)`,
  );
  if (!prefixOk) ok = false;
  // 5. Line numbers must be real (defect 2 — every finding carried the last line).
  const lines = [...walk.matchAll(CALL)].map((m) => lineOf(m.index));
  const distinct = new Set(lines).size;
  const real = distinct > 5 && lines.every((l) => l >= 1 && l <= walk.split("\n").length);
  console.log(`${real ? "PASS" : "FAIL"} — ${distinct} distinct line numbers across ${lines.length} sites`);
  if (!real) ok = false;
  process.exit(ok ? 0 : 1);
}

if (asJson) {
  console.log(JSON.stringify({ mounted: mounted.size, sites: sites.size, unresolved, deferred }, null, 2));
} else {
  console.log(`mounted routes: ${mounted.size}`);
  console.log(`walkthrough API sites: ${sites.size} (${deferred.length} deferred, ${sites.size - deferred.length} resolved)`);
  for (const d of deferred) console.log(`  deferred ${d.shape}  lines=${d.lines.join(",")}`);
  console.log(`UNRESOLVED: ${unresolved.length}`);
  for (const u of unresolved) console.log(`  ${u.shape}  lines=${u.lines.join(",")}`);
}

if (unresolved.length > 0) {
  console.error(
    "\nEach unresolved path is a fetch that answers 404 on a server that does not mount it. Where " +
      "the row swallows the status (`if (!response.ok) return null`), that surfaces as a field " +
      "that is permanently null — which reads exactly like a product that does nothing.",
  );
  process.exit(1);
}