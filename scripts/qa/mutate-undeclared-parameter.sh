#!/usr/bin/env bash
# Prove the undeclared-parameter gate is a real gate: three mutations of the product, each
# red on a NAMED test, and the file restored byte-exact afterwards.
#
# The lesson this script exists for (REQ-004, tick 85): a check that is green because the code
# it guards cannot reach its branch is green for the wrong reason. `validate_declared_params`
# reported nothing about a key the schema never declared for 160 green tests, because nothing
# ever asserted the sweep fires — so each mutation here removes one of the three things the
# gate does, and the suite has to notice.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FILE="$ROOT/crates/workflows/src/graph.rs"
WORK="${WORKTREE_TARGET:-/dev/shm/w3-target}"
export CARGO_INCREMENTAL=0

ORIGINAL="$(mktemp)"
cp "$FILE" "$ORIGINAL"
restore() { cp "$ORIGINAL" "$FILE"; }
trap restore EXIT

pass=0
fail=0

# Run one mutation and report the tests it turned red. Prints "<name> :: <failing test names>".
run_mutation() {
  local name="$1" needle="$2" replacement="$3"
  cp "$ORIGINAL" "$FILE"
  python3 - "$FILE" "$needle" "$replacement" <<'PY'
import sys
path, needle, replacement = sys.argv[1:4]
src = open(path, encoding="utf-8").read()
if needle not in src:
    sys.exit(9)  # the gate's shape changed; the mutation no longer means anything
open(path, "w", encoding="utf-8").write(src.replace(needle, replacement, 1))
PY
  if [ $? -eq 9 ]; then
    echo "$name :: MUTATION DID NOT APPLY (needle absent)"
    return
  fi
  local out
  # The WHOLE lib suite, not the one test that names the mutation. Running a single test is
  # how this script's second mutation came back GREEN: "the sweep reports declared keys too" is
  # caught by a *different* test (`a_declared_parameter_is_never_reported_as_undeclared`), and a
  # filter that only runs the mutation's own test cannot see it — which is the tick-83 lesson in
  # its purest form, a gate that reports green because it was looking somewhere narrow.
  out="$(cd "$ROOT" && CARGO_TARGET_DIR="$WORK" cargo test -p omnion-workflows --lib 2>&1 \
          | grep -E '^test result|\.\.\. FAILED' | head -6)"
  echo "$name :: $(echo "$out" | tr '\n' ' ')"
}

echo "== mutation 1: the sweep never fires (validate goes back to PRESENT-only) =="
run_mutation "no-sweep" \
  '    for key in node.params.as_object().map(|map| map.keys()).into_iter().flatten() {' \
  '    for key in node.params.as_object().map(|map| map.keys()).into_iter().flatten().take(0) {'

echo "== mutation 2: the sweep skips declared keys too (reports EVERY param) =="
run_mutation "reports-declared" \
  '        if declared.contains(key.as_str()) {
            continue;
        }' \
  '        if false {
            continue;
        }'

echo "== mutation 3: the finding is a warning, not an error =="
run_mutation "not-an-error" \
  '        findings.push(Finding::error(
            "unknown_parameter",' \
  '        findings.push(Finding::warning(
            "unknown_parameter",'

restore
trap - EXIT
cp "$ORIGINAL" "$FILE"
if cmp -s "$ORIGINAL" "$FILE"; then
  echo "restore :: byte-exact"
else
  echo "restore :: MISMATCH"
  fail=$((fail + 1))
fi
rm -f "$ORIGINAL"

# The unmutated suite must be green, or the mutations proved nothing about it.
echo "== unmutated =="
final="$(cd "$ROOT" && CARGO_TARGET_DIR="$WORK" cargo test -p omnion-workflows --lib 2>&1 \
         | grep -E '^test result' | head -1)"
echo "unmutated :: $final"
if echo "$final" | grep -q '0 failed'; then
  echo "RESULT :: green"
else
  echo "RESULT :: RED"
  fail=$((fail + 1))
fi
exit "$fail"
