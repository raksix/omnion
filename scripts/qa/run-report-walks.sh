#!/usr/bin/env bash
# Run the report walks ONE PER PROCESS.
#
# The suite serialises on a static mutex, so a static-mutex suite reports everything behind the
# first failure as "blocked" — a label that describes a QUEUE, not a defect. Tick 40 paid for
# this: a walk named as BLOCKING passed alone in ten seconds. One process per walk costs about a
# minute and names all of them.
#
# A pass counts ONLY when libtest's result line says "1 passed or more" and no test failed. A walk
# that RETURNS EARLY (the database did not migrate, so `live_state` answered None) is reported `ok`
# at 0.00s -- the most expensive shape of false green there is -- so the 0.00s case is called out
# rather than counted as a pass.
set -uo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/mnt/apopic/omnion-w4-target}"
export OMNION_DATABASE_URL="postgres://omnion:omnion@127.0.0.1:5433/omnion_t_rpt"
TEST=accounting_reports
LOG=/tmp/w4-report-walks.log
: >"$LOG"

# The listing is built WITHOUT a pipe to the parser, so a compile failure is visible as one.
# "0 found" is otherwise ambiguous: an empty suite and a suite that did not build look identical,
# and the earlier run of this script reported `NO WALKS FOUND` for a binary with a type error.
if ! cargo test -p omnion-api --test "$TEST" -- --list >/tmp/w4-walk-list.txt 2>/tmp/w4-walk-list.err; then
  echo "[walks] the test binary did not build — refusing to run an empty suite"
  tail -20 /tmp/w4-walk-list.err
  exit 1
fi
mapfile -t WALKS < <(grep -E ': test$' /tmp/w4-walk-list.txt)
echo "[walks] ${#WALKS[@]} found"
[ "${#WALKS[@]}" -eq 0 ] && { echo "NO WALKS FOUND — refusing to report 0/0 as green"; exit 1; }

PASS=0
FAIL=0
ZERO=0
FAILED_NAMES=()
for entry in "${WALKS[@]}"; do
  name="${entry%%:*}"
  out=$(cargo test -p omnion-api --test "$TEST" -- --exact "$name" --test-threads=1 2>&1)
  result=$(printf '%s\n' "$out" | grep -E '^test result:' | tail -1)
  # **Anchored at the start of the line, and by the COUNT rather than the word.** libtest's
  # success line contains the word "failed" ("0 passed; 0 failed"), so a keyword search for
  # "failed" classifies every green walk as red -- the tally then reads 0/15 while the log beside
  # it says "1 passed" fourteen times. A checker that reads a word where a state was meant is
  # the same defect the module's own aging note warns about.
  if printf '%s\n' "$result" | grep -qE '^test result: FAILED'; then
    FAIL=$((FAIL + 1)); FAILED_NAMES+=("$name")
    printf '%s\n' "$out" >"/tmp/w4-walk-$name.log"
  elif [ -z "$result" ] && printf '%s\n' "$out" | grep -qE '^error'; then
    FAIL=$((FAIL + 1)); FAILED_NAMES+=("$name (build error)")
    printf '%s\n' "$out" >"/tmp/w4-walk-$name.log"
  elif printf '%s\n' "$result" | grep -qE '^test result: ok\.' \
       && printf '%s\n' "$result" | grep -qE '(^|[^0-9])[1-9][0-9]* passed'; then
    if printf '%s\n' "$result" | grep -q 'finished in 0\.00s'; then
      ZERO=$((ZERO + 1)); FAILED_NAMES+=("$name (0.00s: returned early, did not run)")
    else
      PASS=$((PASS + 1))
    fi
  else
    FAIL=$((FAIL + 1)); FAILED_NAMES+=("$name (no result line: ${result:-none})")
  fi
  printf '%-64s %s\n' "$name" "$result" >>"$LOG"
done

echo "[walks] passed=$PASS failed=$FAIL zero-second=$ZERO"
if [ "$FAIL" -gt 0 ] || [ "$ZERO" -gt 0 ]; then
  printf '[walks] NOT GREEN: %s\n' "${FAILED_NAMES[*]}"
  exit 1
fi
echo "[walks] ALL GREEN"
