#!/usr/bin/env bash
# Prove the settle-run gate can go RED.
#
# The convention this REQ arrived at over 87 ticks: a gate that cannot fail is not a gate, and
# the control is the only reason the number above it can be believed. So every mutation here
# re-introduces a real defect this tick fixed, and every mutation of the GATE ITSELF must also
# go red — a gate whose own mutation survives is a gate that is measuring the wrong thing.
#
# M1  the witness check reverted      -> a never-started run reads settled (the tick-61 reading)
# M2  the witness only looks at steps -> a run whose status left `pending` never counts as started
# M3  the `finished` check deleted    -> a hung run is indistinguishable from a finished one
# M4  the caller drops `runFinished`  -> assigned, never reported (the pageIsAlive shape, tick 45)
# M5  the caller drops `runStarted`   -> same, for the witness
# M6  the note drops `runStarted`     -> assigned AND emitted nowhere
# M7  the extractor stops at the paren-> the extraction is a signature with no body
# M8  the length assertion removed   -> a truncated extraction passes silently
#
# Usage: bash apps/admin/features/workflows/mutate-settle-run-row.sh
set -uo pipefail

# Four levels up, not five: this file sits in `apps/admin/features/workflows/`, so
# `../../../../..` is the PARENT of the repo and every mutation then reports "the mutation did
# not apply" against a path that does not exist. The harness said so out loud — the SKIP line
# names a missing file rather than a green run — which is the only reason it took a minute
# instead of shipping as "0 red". The sibling scripts that derive ROOT from BASH_SOURCE all sit
# one level higher; derived by counting, and the count is asserted by the paths themselves.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
if [ ! -f "$ROOT/scripts/qa/walkthrough.cjs" ]; then
  echo "FATAL: ROOT does not contain scripts/qa/walkthrough.cjs — got $ROOT" >&2
  exit 2
fi
WALKTHROUGH="$ROOT/scripts/qa/walkthrough.cjs"
TEST="$ROOT/apps/admin/features/workflows/settle-run-row.test.ts"
BACKUP="$(mktemp -d)"

cp "$WALKTHROUGH" "$BACKUP/walkthrough.cjs"
cp "$TEST" "$BACKUP/test.ts"
trap 'cp "$BACKUP/walkthrough.cjs" "$WALKTHROUGH"; cp "$BACKUP/test.ts" "$TEST"; rm -rf "$BACKUP"' EXIT

pass=0
fail=0

# restore, then apply a python edit, then run only this gate
mutate() {
  local label="$1" file="$2" script="$3"
  cp "$BACKUP/walkthrough.cjs" "$WALKTHROUGH"
  cp "$BACKUP/test.ts" "$TEST"
  if ! python3 - "$file" "$script" <<'PY'
import sys
path, script = sys.argv[1], sys.argv[2]
text = open(path).read()
exec(script)
open(path, "w").write(text)
PY
  then
    echo "  SKIP  $label — the mutation did not apply (anchor moved); a mutation that changes nothing reports a clean bill of health"
    fail=$((fail + 1))
    return
  fi
  local out
  out="$(cd "$ROOT" && node --test --experimental-strip-types "$TEST" 2>&1)"
  local red
  red="$(printf '%s\n' "$out" | grep -c '^not ok')"
  if [ "$red" -ge 1 ]; then
    local name
    name="$(printf '%s\n' "$out" | grep '^not ok' | head -1 | sed 's/^not ok [0-9]* - //')"
    echo "  RED   $label"
    echo "          ^ $name"
    pass=$((pass + 1))
  else
    echo "  GREEN $label  <-- SURVIVED: the gate does not police this"
    fail=$((fail + 1))
  fi
}

echo "settle-run-row mutations:"

mutate "M1 the witness check reverted (a never-started run reads settled)" \
  "$WALKTHROUGH" '
text = text.replace(
    "if (current.state === previous && started) {",
    "if (current.state === previous) {",
)
assert text.count("if (current.state === previous) {") == 2, "M1 anchor did not apply"
'

mutate "M2 the witness only looks at steps, not the run status" \
  "$WALKTHROUGH" '
text = text.replace(
    "    if (status && status !== \"pending\" && status !== \"queued\") return true;\n",
    "",
)
assert "status !== \"queued\"" not in text, "M2 anchor did not apply"
'

mutate "M3 the finished check deleted (a hung run reads as finished)" \
  "$WALKTHROUGH" '
text = text.replace(
    "return { ...current, settled: true, started: true, finished: hasFinished(current.run) };",
    "return { ...current, settled: true, started: true, finished: true };",
)
assert "hasFinished(current.run)" not in text, "M3 anchor did not apply"
'

mutate "M4 the caller drops runFinished" \
  "$WALKTHROUGH" '
text = text.replace("      runFinished = settled.finished;\n", "")
assert "runFinished = settled.finished" not in text, "M4 anchor did not apply"
'

mutate "M5 the caller drops runStarted" \
  "$WALKTHROUGH" '
text = text.replace("      runStarted = settled.started;\n", "")
assert "runStarted = settled.started" not in text, "M5 anchor did not apply"
'

mutate "M6 the note drops runStarted and runFinished" \
  "$WALKTHROUGH" '
text = text.replace("      runStarted,\n      runFinished,\n", "")
assert "runStarted,\n      runFinished," not in text, "M6 anchor did not apply"
'

mutate "M7 the extractor stops at the parameter list's own brace" \
  "$TEST" '
text = text.replace(
    "  assert.equal(source[braceAt], \"{\", ",
    "  if (false) assert.equal(source[braceAt], \"{\", ",
)
text = text.replace(
    "  let depth = 0;\n  for (let at = braceAt; at < source.length; at += 1) {\n    const character = source[at];\n    if (character === \"{\") depth += 1;",
    "  let depth = 0;\n  let firstBrace = true;\n  for (let at = start; at < source.length; at += 1) {\n    const character = source[at];\n    if (character === \"{\" && firstBrace) { firstBrace = false; continue; }\n    if (character === \"{\") depth += 1;",
)
assert "firstBrace" in text, "M7 anchor did not apply"
'

# M8 is DECLARED to survive, and the reason is worth more than the mutation.
#
# Relaxing the extraction length assertion leaves the suite green, because nothing else
# depends on it: a truncated body is caught by M7 (the extractor returning a signature with no
# body is a SyntaxError at `new Function`) and by the behaviour tests, which fail loudly when
# the helper they execute does not exist. So the assertion is DOCUMENTATION, not a gate — and
# this REQ has a standing rule for exactly this case (tick 57): an expected survivor says so
# out loud rather than reading as a pass, because a doc claiming a check nobody runs is the
# shape this file exists to prevent.
#
# It is kept anyway: it is the one assertion that turns a *plausible* truncation (a brace pair
# inside a string literal, which yields valid-but-wrong source) into a visible failure, and
# "documented as not load-bearing" is a different statement from "removed".
echo "  GREEN M8 the extraction length assertion relaxed <-- DECLARED SURVIVOR (documentation, not a gate; M7 and the behaviour tests cover truncation)"

cp "$BACKUP/walkthrough.cjs" "$WALKTHROUGH"
cp "$BACKUP/test.ts" "$TEST"

# One mutation is a DECLARED survivor: it never went through `mutate`, so it was never counted
# as a failure and must not be subtracted either — the first draft of this line did exactly that
# and printed "7 red · -1 survived", a summary no reader can interpret. The count is what the
# mutate calls produced.
if [ "$fail" -ne 0 ]; then
  echo ""
  echo "  ${pass} red · ${fail} survived or skipped — a surviving mutation is a gap in the gate, not a pass"
  exit 1
fi
echo ""
echo "  ${pass}/${pass} mutations red, each naming its assertion (+1 declared survivor, see above)"
