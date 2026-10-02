#!/usr/bin/env bash
# Proves `stack-liveness.test.ts` can go RED.
#
# A guard that cannot fail is a comment. This restores the three defects the test is written
# against, one at a time, and requires the suite to catch each — reverting a construct the file
# claims to protect and watching it stay green is how a false sense of coverage is manufactured.
#
#   M1  the ladder collapses to the single 8s budget   -> the retry assertions must fail
#   M2  a 5xx is read as death again                   -> the status assertion must fail
#   M3  a refused connection no longer decides death   -> the refusal assertion must fail
#   M4  the exhausted ladder reports alive             -> the "cannot go green" assertion must fail
#
# Every mutation is reverted and the file is checked against its md5, so a run that fails midway
# cannot leave the harness broken for the next writer.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TARGET="$ROOT/scripts/qa/walkthrough.cjs"
TEST="$ROOT/apps/admin/features/workflows/stack-liveness.test.ts"
TMP="$(mktemp)"

BEFORE="$(md5sum "$TARGET" | cut -d' ' -f1)"
pass=0
fail=0

restore() {
  if [ ! -f "$TARGET.bak-w3-mutate" ]; then
    echo "FAIL: no backup to restore from — refusing to touch $TARGET"
    exit 1
  fi
  mv "$TARGET.bak-w3-mutate" "$TARGET"
}
trap restore EXIT
cp "$TARGET" "$TARGET.bak-w3-mutate"

# run <label> <expect-substring-in-output>
run() {
  local label="$1" out
  out="$(cd "$ROOT" && node --test --experimental-strip-types "$TEST" 2>&1)"
  if echo "$out" | grep -qE '^# fail [1-9]'; then
    echo "  OK   $label — suite went red"
    pass=$((pass + 1))
  else
    echo "  MISS $label — the suite stayed GREEN with the defect restored"
    fail=$((fail + 1))
  fi
}

mutate() {
  # mutate <label> <python-replacement-expression>
  local label="$1" expr="$2"
  cp "$TARGET.bak-w3-mutate" "$TARGET"
  TARGET="$TARGET" EXPR="$expr" python3 - <<'PY'
import os
path = os.environ["TARGET"]
old, new = os.environ["EXPR"].split("|||")
src = open(path).read()
if old not in src:
    print("  MISS could not find the construct to mutate")
    raise SystemExit(9)
open(path, "w").write(src.replace(old, new, 1))
PY
  if [ $? -ne 0 ]; then
    echo "  MISS $label — the construct is not in the file any more (test is stale)"
    fail=$((fail + 1))
    return
  fi
  run "$label"
}

echo "mutation harness: stack-liveness"

mutate "M1 the retry ladder collapses to one 8s budget" \
  'for (const timeout of [8000, 15000, 25000]) {|||for (const timeout of [8000]) {'

mutate "M2 a 5xx is read as death again" \
  '        return false;
      } catch (err) {|||        return (await context.request.get(`${URL_ADMIN}/login`, { timeout })).status() >= 500;
      } catch (err) {'

mutate "M3 a refused connection no longer decides death" \
  '/ECONNREFUSED|ECONNRESET|EPIPE|socket hang up|ECONNABORTED/i|||/NEVER_MATCHES_THE_REFUSAL/i'

mutate "M4 the exhausted ladder reports alive" \
  '    log(`stack liveness: ${lastErr ? String(lastErr.message || lastErr) : "unknown"} after three attempts`);
    return true;|||    log(`stack liveness: ${lastErr ? String(lastErr.message || lastErr) : "unknown"} after three attempts`);
    return false;'

restore
trap - EXIT

AFTER="$(md5sum "$TARGET" | cut -d' ' -f1)"
echo "  file restored byte-exact: $([ "$BEFORE" = "$AFTER" ] && echo yes || echo NO)"
[ "$BEFORE" = "$AFTER" ] || { echo "FAIL: $TARGET is not byte-identical after the run"; exit 1; }

echo "mutations: $pass caught, $fail missed"
[ "$pass" -eq 4 ] || exit 1
echo "MUTATIONS_ALL_CAUGHT=$pass"
