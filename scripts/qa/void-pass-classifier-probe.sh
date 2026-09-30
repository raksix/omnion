#!/usr/bin/env bash
# Is the walkthrough's "this pass is not a verdict" rule correct, checked against every shape it
# can meet?
#
# The rule is three predicates — evidence was lost, the stack was not serving, the pass interacted
# with nothing — and each one has a FALSE side that is easy to get wrong in the direction that
# costs a feature its evidence:
#
#   * "any screenshot failed" is not "every screenshot failed". One capture lost on a full disk
#     while the other forty-six succeeded is a degraded pass, not a void one, and calling it void
#     would make writers re-run passes that are already good evidence.
#   * "the stack is down" must be a POSITIVE liveness answer, not the absence of a recorded probe.
#     A summary from before this field existed has `apiLiveness === undefined`; treating "unknown"
#     as "down" would void every old artifact, and treating it as "up" would void nothing.
#   * "no clicks and no shots" is a void pass, but "no clicks" alone is not — a focused pass
#     (`--only=wizard`) legitimately does little, and the tick-46 pass DID click 35 times while
#     proving nothing, so the conjunction is the assertion.
#
# The classifier is extracted from the walkthrough by `sed` rather than re-typed, because a copy
# of a rule is a second rule: this probe would go green against a correct `passIsVoid` while the
# walkthrough kept a broken one, which is the same false green this file exists to prevent.
set -uo pipefail

WALK="${1:-scripts/qa/walkthrough.cjs}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Pull the real function out of the walkthrough. `passIsVoid` is pure (no browser, no fs), so the
# extracted source is callable on its own.
sed -n '/^function passIsVoid/,/^}/p' "$WALK" > "$TMP/passIsVoid.js"
if [ ! -s "$TMP/passIsVoid.js" ]; then
  echo "FAIL  could not extract passIsVoid() from $WALK — the probe must test the shipped rule"
  exit 1
fi

cat > "$TMP/check.cjs" <<'EOF'
const fs = require("fs");
const { passIsVoid } = require(process.argv[2]);
let fail = 0;
function check(name, want, args) {
  const got = passIsVoid(args);
  const ok = (got.length === 0) === (want === "pass");
  console.log(`${ok ? "PASS" : "FAIL"}  ${name} — ${got.length ? got.join("; ") : "(a verdict)"}`);
  if (!ok) fail += 1;
}
const live = { up: true, url: "http://127.0.0.1:3103/healthz", reason: "" };
const dead = { up: false, url: "http://127.0.0.1:3103/readyz", reason: "answered 503" };
// A complete pass: evidence written, stack serving, the walker moved.
const good = { shotFailures: [], apiLiveness: live, clicks: 35, shots: 47 };
// The tick-46 pass, exactly: every capture lost, the database gone, clicks still counted.
const tick46 = { shotFailures: new Array(47).fill({ name: "x", error: "ENOSPC" }), apiLiveness: dead, clicks: 35, shots: 0 };

check("a complete pass is a verdict", "pass", good);
check("the tick-46 pass is void", "void", tick46);
check("one lost capture voids the pass", "void", { ...good, shotFailures: [{ name: "a", error: "ENOSPC" }] });
check("a dead stack voids the pass", "void", { ...good, apiLiveness: dead });
check("no evidence at all voids the pass", "void", { ...good, shots: 0 });
check("no clicks at all voids the pass", "void", { ...good, clicks: 0 });
// The one that must NOT be void: an artifact written before this field existed. `undefined` is
// "we did not ask", and "we did not ask" is not "the answer was no".
check("an unknown liveness is not a dead stack", "pass", { ...good, apiLiveness: undefined });
// A focused pass that did little is still evidence.
check("a quiet focused pass is still a verdict", "pass", { ...good, clicks: 1, shots: 1 });
// The same facts as the roll-up's `counts`, where they are NUMBERS rather than arrays. This case
// is the one that caught the real defect: `.length` on a number is `undefined`, so `!undefined` is
// true and every well-formed pass was voided by its own evidence check.
check("counts as numbers are understood", "pass", { shotFailures: 0, apiLiveness: live, clicks: 35, shots: 47 });
check("zero counts as numbers is void", "void", { shotFailures: 0, apiLiveness: live, clicks: 0, shots: 0 });
check("a lost capture counted as a number is void", "void", { shotFailures: 3, apiLiveness: live, clicks: 35, shots: 47 });
// The reasons must name the cause, because "void" with no reason is what the last pass produced.
const reasons = passIsVoid(tick46);
if (reasons.length !== 3) { console.log(`FAIL  expected 3 reasons, got ${reasons.length}`); fail += 1; }
else console.log(`PASS  every cause is named — ${reasons.join(" / ")}`);
process.exit(fail ? 1 : 0);
EOF

# The extracted function must be requirable: no browser handles, no fs, no top-level side effects.
# Only `module.exports` is assigned — a probe that also re-declares `module` shadows the CommonJS
# one and `require` hands back the probe's own empty object, which reads as "the function does not
# exist" and looks like a product failure.
echo "module.exports.passIsVoid = passIsVoid;" >> "$TMP/passIsVoid.js"
node "$TMP/check.cjs" "$TMP/passIsVoid.js"
RC=$?
if [ "$RC" = "0" ]; then
  echo "OK: the shipped passIsVoid() classifies every shape above correctly"
else
  echo "FAILED: $RC assertion(s) — a pass that cannot be believed must not be able to look like one that can"
fi
exit "$RC"
