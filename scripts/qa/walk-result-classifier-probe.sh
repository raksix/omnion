#!/usr/bin/env bash
# The walk runner's classifier, checked against every libtest result line it can meet.
#
# `run-report-walks.sh` MISCOUNTED ITS OWN PASSES TWICE, and both times it printed a confident
# wrong total rather than refusing: first `grep -E 'test result: FAILED'` matched the substring
# inside the success line (`0 passed; 0 failed` contains "failed"), then the fix asked for the
# literal phrase `1 passed or more` -- which is what a FULL-SUITE summary prints and a filtered
# single-walk run never does. Fourteen green walks read as `passed=0 failed=15` while the log
# beside them said `1 passed` fourteen times.
#
# A checker that cannot itself be checked is how that happened twice. The five shapes below are
# the ones the runner can meet, including the ZERO case -- a walk that returned early because the
# database would not migrate is reported `ok` and is the most expensive false green there is.
set -uo pipefail

classify() {
  local result="$1"
  if printf '%s\n' "$result" | grep -qE '^test result: FAILED'; then
    echo FAIL; return
  fi
  if [ -z "$result" ]; then echo NONE; return; fi
  if printf '%s\n' "$result" | grep -qE '^test result: ok\.' \
     && printf '%s\n' "$result" | grep -qE '(^|[^0-9])[1-9][0-9]* passed'; then
    if printf '%s\n' "$result" | grep -q 'finished in 0\.00s'; then echo ZERO; else echo PASS; fi
    return
  fi
  echo UNKNOWN
}

FAIL=0
check() {
  local want="$1" line="$2" got
  got=$(classify "$line")
  if [ "$got" = "$want" ]; then
    printf 'PASS  %-8s %s\n' "$got" "$line"
  else
    printf 'FAIL  want=%-6s got=%-6s %s\n' "$want" "$got" "$line"
    FAIL=$((FAIL + 1))
  fi
}

# What a single-walk run actually prints -- this is the shape the phrase-gate missed.
check PASS "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 14 filtered out; finished in 120.67s"
check PASS "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; finished in 3.10s"
check PASS "test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; finished in 45.00s"
# A green run that DID NOT ACTUALLY RUN: `live_state` answered None and the walk returned early.
check ZERO "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; finished in 0.00s"
# The real failure shapes.
check FAIL "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 14 filtered out; finished in 77.04s"
# The one that caused this: the summary phrase is NOT what a filtered run prints.
check UNKNOWN "test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; finished in 0.00s"
check NONE ""

echo
if [ "$FAIL" -eq 0 ]; then
  echo "ALL 6 PASS"
else
  echo "$FAIL FAILED"
  exit 1
fi
