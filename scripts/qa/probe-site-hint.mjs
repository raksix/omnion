// Probe: does the renderer's `?site=` hint reach the API the way the contract says?
//
// This is the REQ-063 criterion-17 blocker. The renderer used to resolve the site from
// OMNION_SITE or the visitor host only, so `?site=main` was inert and a multi-site lookup
// went out with no site at all. The fix forwards the hint, bounded to a token.
//
// It exercises the *contract* — which values are accepted as a site hint and what they mean —
// against the same rules `normalizeSiteHint` and the API's `classify_hint` apply, without
// needing a browser or a QA slot. The browser pass remains the only thing that can tick the
// criterion; this says the two ends of the wire agree on what a hint is.
const ACCEPT = /^[a-z0-9.-]+$/;
const MAX = 64;

/** Mirror of apps/web/lib/api.ts normalizeSiteHint. */
function normalizeSiteHint(value) {
  const first = Array.isArray(value) ? value[0] : value;
  if (typeof first !== "string") return null;
  const trimmed = first.trim().toLowerCase();
  if (!trimmed || trimmed.length > MAX || !ACCEPT.test(trimmed)) return null;
  return trimmed;
}

/** Mirror of the API's classify_hint: a dotted value is a host, otherwise a key. */
function classifyHint(value) {
  return value.includes(".") ? "Host" : "Key";
}

const cases = [
  // [raw, expected hint, what the API must classify it as]
  ["main", "main", "Key"],
  ["  Main  ", "main", "Key"],
  ["qa.omnion.test", "qa.omnion.test", "Host"],
  ["", null, null],
  ["<script>", null, null],
  ["main&x=1", null, null],
  ["a".repeat(65), null, null],
  ["a".repeat(64), "a".repeat(64), "Key"],
  ["../etc/passwd", null, null],
  ["main%00", null, null],
  [undefined, null, null],
  [["main", "second"], "main", "Key"],
];

let pass = 0;
const failures = [];
for (const [raw, expected, kindWanted] of cases) {
  const got = normalizeSiteHint(raw);
  if (got === expected) pass += 1;
  else failures.push(`normalize(${JSON.stringify(raw)?.slice(0, 30)}) = ${JSON.stringify(got)}, wanted ${JSON.stringify(expected)}`);
  // Whatever survives must also be classified the way the API classifies it, so the two
  // ends of the wire agree on what a site hint is.
  if (got !== null && classifyHint(got) !== kindWanted) {
    failures.push(`${JSON.stringify(got)} classified as ${classifyHint(got)}, wanted ${kindWanted}`);
  }
}

console.log(`site-hint probe: ${pass}/${cases.length} pass`);
for (const f of failures) console.log(`  FAIL ${f}`);
process.exit(failures.length === 0 ? 0 : 1);
