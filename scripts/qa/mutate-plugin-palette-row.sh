#!/bin/bash
# Tick 75 mutations: every guard in plugin-palette-row.test.ts must go RED when its target is
# reverted, and each file must come back byte-exact. Run from the worktree root.
set -u
cd "$(dirname "$0")/../.." || exit 1

TEST="apps/admin/features/workflows/plugin-palette-row.test.ts"
WALK="scripts/qa/walkthrough.cjs"
RUN="scripts/qa/run.sh"

md5_before_walk=$(md5sum "$WALK" | cut -d' ' -f1)
md5_before_run=$(md5sum "$RUN" | cut -d' ' -f1)
fail=0

run_suite() {
  ( cd apps/admin && env -i HOME=/root \
      PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
      node --test --experimental-strip-types features/workflows/plugin-palette-row.test.ts 2>&1 )
}

# A python mutation body operating on WALK, read from STDIN.
#
# **The body arrives on stdin, not as `$2`.** The first draft took it as a positional argument and
# every WALK mutation called this function with nothing to expand: `$2` was unbound under
# `set -u`, the python died before writing anything, and the guard that followed reported each of
# M1–M4 "STILL GREEN" — which reads as four leaky guards when the truth is four mutations that
# never ran. `expect_red` checks the SUITE, never whether the mutation landed, so a broken mutator
# and a broken guard are the same red. Hence the md5 comparison here: a mutation that does not
# change the file is refused BEFORE its verdict is recorded, and the harness fails loudly.
mutate_walk() {
  local body before after
  body=$(cat)
  if [ -z "$body" ]; then
    echo "  ERROR empty mutation body — the guard verdict below would be meaningless"
    fail=1
    restore
    return 1
  fi
  before=$(md5sum "$WALK" | cut -d' ' -f1)
  # The body is python SOURCE and it opens the file ITSELF. The wrapper used to splice the
  # mutation into a pre-built script through `$2`; a heredoc hands its text over verbatim
  # instead, so the three prologue lines every mutation used to inherit are prepended here.
  # Without them a body referencing `s` dies on `NameError` — which `expect_red` would report
  # as a leaky guard, the same false red this function exists to prevent.
  { printf 'import sys\np = sys.argv[1]\ns = open(p).read()\n'; printf '%s\n' "$body"; printf "open(p, 'w').write(s)\n"; } \
    | python3 - "$WALK" || {
    echo "  ERROR mutation python failed"; fail=1; restore; return 1; }
  after=$(md5sum "$WALK" | cut -d' ' -f1)
  if [ "$before" = "$after" ]; then
    echo "  ERROR mutation did not change $WALK — refusing to record a guard verdict"
    fail=1
    restore
    return 1
  fi
}

restore() {
  git checkout -- "$WALK" "$RUN"
}

# **Restore BEFORE recording the verdict, not after.** The first run of this harness reported
# "walkthrough.cjs changed" while the file was byte-identical to HEAD: `expect_red` ran the suite
# and then restored, but a mutation whose python FAILED (M1's missing anchor) had already bailed
# out of `mutate_walk` with its own `restore`, and the outer restore then ran against a file the
# next mutation was in the middle of editing. The order that is safe is: apply, verify the file
# changed, restore, THEN run the suite — because the suite is the only step that can leave a
# mutation behind, and it does not modify the source at all.
expect_red() {
  local label="$1" expect="$2"
  local out
  out=$(run_suite)
  restore
  if printf '%s' "$out" | grep -qE "^# fail [1-9]"; then
    echo "  OK    $label -> red"
  else
    echo "  LOOSE $label -> STILL GREEN (expected $expect failing)"
    fail=1
  fi
}

echo "== baseline (must be green) =="
base=$(run_suite | grep -E '^# (pass|fail)' | tr '\n' ' ')
echo "  $base"
case "$base" in
  *"# fail 0"*) ;;
  *) echo "  BASELINE IS RED"; fail=1 ;;
esac

echo "== M1: put node_type back in the row =="
# The anchor is a single line rather than a triple-quoted block: a `"""…"""` python literal
# inside a shell heredoc is one more quoting layer between the mutation and the file it claims to
# edit, and the first run of this harness failed on exactly that — `AssertionError: M1 anchor
# missing` on a string that is present in the file. **A mutation whose anchor cannot be trusted
# reports itself as a leaky guard**, so the anchor is the shortest string that cannot match twice.
mutate_walk <<'PYEOF'
old = '          const target = nodes.find(\n            (node) => !String(node.type ?? "").startsWith("trigger.") && String(node.type ?? "") !== "end",\n          );'
new = '          const target = nodes.find((node) => !node.node_type.startsWith("trigger.") && node.node_type !== "end");'
assert old in s, "M1 anchor missing"
assert s.count(old) == 1, "M1 anchor is not unique"
s = s.replace(old, new)
PYEOF
expect_red "M1 row reads node_type" "test 1"

echo "== M2: assign the plugin key to node_type =="
mutate_walk <<'PYEOF'
old = "          target.type = key;"
new = "          target.node_type = key;"
assert old in s, "M2 anchor missing"
s = s.replace(old, new)
PYEOF
expect_red "M2 rename lands on the wrong field" "test 2"

echo "== M3: drop the null-legibility field =="
mutate_walk <<'PYEOF'
old = "      pluginProbeReachedServer: disabledRead !== null,"
assert old in s, "M3 anchor missing"
s = s.replace(old, "")
PYEOF
expect_red "M3 null is folded into the verdict" "test 3"

echo "== M4: collapse the criterion's claims into one flag =="
mutate_walk <<'PYEOF'
old = '      saysTypo: Boolean(typoSentence),'
assert old in s, "M4 anchor missing"
assert s.count(old) == 1, "M4 anchor is not unique"
s = s.replace(old, '      saysTypo: Boolean(pluginSentence),')
PYEOF
expect_red "M4 both sentences look the same" "test 5"

echo "== M5: run.sh stops reading the command line =="
python3 - "$RUN" <<'PY'
import sys, re
p = sys.argv[1]
s = open(p).read()
start = s.index("QA_ONLY_ARGS=()")
end = s.index('[ -n "$QA_ONLY_FILTER" ]')
s = s[:start] + 'QA_ONLY_ARGS=()\n' + s[end:]
s = s.replace('[ -n "$QA_ONLY_FILTER" ] && QA_ONLY_ARGS=(--only="$QA_ONLY_FILTER")',
              '[ -n "${QA_ONLY:-}" ] && QA_ONLY_ARGS=(--only="$QA_ONLY")')
open(p, 'w').write(s)
PY
expect_red "M5 --only back to env-only" "test 6"

echo "== M6: the cursor is written but never read =="
python3 - "$RUN" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
old = 'if [ "$_prev" = "--only" ]; then QA_ONLY_FILTER="$arg"; fi'
assert old in s, "M6 anchor missing"
s = s.replace(old, ":")
open(p, 'w').write(s)
PY
expect_red "M6 cursor write-only" "test 6"

echo "== M7: the banner goes back to reading the env var =="
python3 - "$RUN" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
old = 'step "browser walkthrough${QA_ONLY_FILTER:+ (focused: $QA_ONLY_FILTER)}"'
assert old in s, "M7 anchor missing"
s = s.replace(old, 'step "browser walkthrough${QA_ONLY:+ (focused: $QA_ONLY)}"')
open(p, 'w').write(s)
PY
expect_red "M7 banner misstates the scope" "test 7"

echo "== M8: the typo lookup is aliased to the plugin lookup =="
mutate_walk <<'PYEOF'
old = 'const typoSentence = unknownFindings.find((finding) => /not a node type the platform knows/i.test(finding.message));'
new = 'const typoSentence = unknownFindings.find((finding) => /re-enable/i.test(finding.message));'
assert old in s, "M8 anchor missing"
assert s.count(old) == 1, "M8 anchor is not unique"
s = s.replace(old, new)
PYEOF
expect_red "M8 both sentences searched with one regex" "test 5"

echo "== M9: the artifact root is hardcoded again =="
python3 - "$RUN" <<'PYEOF'
import sys
p = sys.argv[1]
s = open(p).read()
old = 'OUT="${QA_OUT_ROOT:-$ROOT/qa-artifacts}/$TS"'
assert old in s, "M9 anchor missing"
assert s.count(old) == 1, "M9 anchor is not unique"
s = s.replace(old, 'OUT="$ROOT/qa-artifacts/$TS"')
open(p, 'w').write(s)
PYEOF
expect_red "M9 a full disk decides the measurement" "test 8"

echo "== restore check =="
restore
w=$(md5sum "$WALK" | cut -d' ' -f1); r=$(md5sum "$RUN" | cut -d' ' -f1)
[ "$w" = "$md5_before_walk" ] && echo "  OK    walkthrough.cjs byte-exact" || { echo "  BAD   walkthrough.cjs changed"; fail=1; }
[ "$r" = "$md5_before_run" ] && echo "  OK    run.sh byte-exact" || { echo "  BAD   run.sh changed"; fail=1; }

echo "== final green =="
base=$(run_suite | grep -E '^# (pass|fail)' | tr '\n' ' ')
echo "  $base"
case "$base" in
  *"# fail 0"*) ;;
  *) echo "  POST-MUTATION SUITE IS RED"; fail=1 ;;
esac

echo "MUTATION_RESULT=$([ $fail -eq 0 ] && echo ALL-RED-AND-RESTORED || echo LEAKS-OR-DIRTY)"
exit $fail