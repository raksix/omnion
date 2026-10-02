#!/usr/bin/env bash
# mutate-keyboard-run-row.mjs — every mutation must turn the gate RED on a NAMED rule.
#
# Two rules this harness follows, both earned in this REQ:
#  * a mutation that does not change the file is a strawman and reports SURVIVED (tick 61's M3);
#  * the gate asserts on STRIPPED source for code, so a comment quoting the defect cannot keep
#    it red forever (tick 55's lesson, this file's own first-draft failure).
set -uo pipefail
cd /mnt/apopic/omnion-w3

WALK=scripts/qa/walkthrough.cjs
GATE=apps/admin/features/workflows/keyboard-pass-row.test.ts
SUITE_CMD="cd /mnt/apopic/omnion-w3/apps/admin && env -i HOME=/root PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin node --test features/workflows/keyboard-pass-row.test.ts"

pass=0; fail=0; survived=0
before_md5="$(md5sum "$WALK" | cut -d' ' -f1)"

run_mutation() { # name  file  python-replacement-script
  local name="$1" file="$2" script="$3"
  local md5_before; md5_before="$(md5sum "$file" | cut -d' ' -f1)"
  cp "$file" "$file.bak-mutate"
  python3 "$script"
  local md5_after; md5_after="$(md5sum "$file" | cut -d' ' -f1)"
  if [ "$md5_before" = "$md5_after" ]; then
    echo "FAIL [$name] the mutation did not change the file (strawman — nothing was proven)"
    survived=$((survived+1)); fail=$((fail+1)); mv "$file.bak-mutate" "$file"; return
  fi
  local out; out="$(eval "$SUITE_CMD" 2>&1)"
  if echo "$out" | grep -q '^# fail 0'; then
    echo "SURVIVED [$name] the suite stayed green — the gate does not hold this"
    survived=$((survived+1)); fail=$((fail+1))
  else
    local named; named="$(printf '%s\n' "$out" \
      | grep -A 4 '^not ok ' \
      | grep -oE "the row (asks|reads)[^\"]*|a run count is reported[^\"]*|an unreadable endpoint[^\"]*|must (fetch|read|not read|carry|name|be refused)[^\"]*|the difference must[^\"]*|the claim must[^\"]*|the run whose origin[^\"]*|which the router does not mount[^\"]*" \
      | head -1)"
    if [ -n "$named" ]; then
      echo "red  [$name] failed on: $named"
      pass=$((pass+1))
    else
      echo "FAIL [$name] red but NOT on a named rule — a red for the wrong reason is not a gate"
      fail=$((fail+1))
    fi
  fi
  mv "$file.bak-mutate" "$file"
}

# M1 — the shipped defect: the unmounted route.
cat > /tmp/m1.py <<'PY'
p='scripts/qa/walkthrough.cjs'; s=open(p).read()
s=s.replace('/api/v1/workflows/${id}/executions?limit=50', '/api/v1/workflows/${id}/runs?limit=5')
open(p,'w').write(s)
PY
run_mutation M1-unmounted-route "$WALK" /tmp/m1.py

# M2 — the second defect: the wrong payload key.
cat > /tmp/m2.py <<'PY'
p='scripts/qa/walkthrough.cjs'; s=open(p).read()
s=s.replace('Array.isArray(runAfterKey?.executions) ? runAfterKey.executions : null',
            'Array.isArray(runAfterKey?.runs) ? runAfterKey.runs : null')
open(p,'w').write(s)
PY
run_mutation M2-wrong-payload-key "$WALK" /tmp/m2.py

# M3 — the third defect: the count with no baseline.
cat > /tmp/m3.py <<'PY'
p='scripts/qa/walkthrough.cjs'; s=open(p).read()
s=s.replace('Math.max(0, runAfterKeyList.length - runCountBeforeKey)',
            'runAfterKeyList.length')
open(p,'w').write(s)
PY
run_mutation M3-no-baseline "$WALK" /tmp/m3.py

# M4 — the baseline collected but never compared: the difference silently becomes null.
cat > /tmp/m4.py <<'PY'
p='scripts/qa/walkthrough.cjs'; s=open(p).read()
s=s.replace('runAfterKeyList !== null && runCountBeforeKey !== null\n        ? Math.max',
            'runAfterKeyList !== null && runCountBeforeKey !== null && false\n        ? Math.max')
open(p,'w').write(s)
PY
run_mutation M4-baseline-ignored "$WALK" /tmp/m4.py

# M5 — the origin reported under the wrong wire key.
cat > /tmp/m5.py <<'PY'
p='scripts/qa/walkthrough.cjs'; s=open(p).read()
s=s.replace('newestRun?.trigger ?? null', 'newestRun?.trigger_kind ?? newestRun?.status ?? null')
open(p,'w').write(s)
PY
run_mutation M5-column-name-on-the-wire "$WALK" /tmp/m5.py

# M6 — index 0 of the list rather than the run the key produced.
cat > /tmp/m6.py <<'PY'
p='scripts/qa/walkthrough.cjs'; s=open(p).read()
s=s.replace('runsAddedByKey > 0 ? runAfterKeyList[0] : null', 'runAfterKeyList[0] ?? null')
open(p,'w').write(s)
PY
run_mutation M6-index-zero-run "$WALK" /tmp/m6.py

# M7 — the note drops the baseline on the way out (collected, never reported).
cat > /tmp/m7.py <<'PY'
p='scripts/qa/walkthrough.cjs'; s=open(p).read()
s=s.replace('      runCountBeforeKey,\n      runsAfterKey:', '      runsAfterKey:')
open(p,'w').write(s)
PY
run_mutation M7-baseline-not-reported "$WALK" /tmp/m7.py

# M8 — the boolean the criterion states is replaced by the bare count it was hiding.
cat > /tmp/m8.py <<'PY'
p='scripts/qa/walkthrough.cjs'; s=open(p).read()
s=s.replace('runStartedFromKey: runsAddedByKey > 0', 'runStartedFromKey: runsAfterKeyList !== null')
open(p,'w').write(s)
PY
run_mutation M8-claim-weakened "$WALK" /tmp/m8.py

after_md5="$(md5sum "$WALK" | cut -d' ' -f1)"
echo
if [ "$before_md5" != "$after_md5" ]; then
  echo "FAIL the walkthrough was NOT restored byte-exact ($before_md5 -> $after_md5)"
  fail=$((fail+1))
else
  echo "walkthrough restored byte-exact"
fi
echo "red=$pass  fail=$fail  survived=$survived"
[ "$fail" -eq 0 ]