#!/usr/bin/env bash
# A pass that measured NOTHING must not be able to print a CLEAN-looking report.
#
# The defect this probes (2026-10-01, tick 62). Three steps failed in a row and the last two made
# the failure silent:
#
#   1. `/hr/attendance` (route `hr-attendance-roster`) threw `Execution context was destroyed,
#      most likely because of a navigation` out of its page walk, so it was pushed as
#      `{ ...route, failed }` with NO `diagnostics` key.
#   2. The findings roll-up guarded that (`if (!d)` → an `unmeasured-page` high finding), but the
#      markdown reporter's "Per-page diagnostics" loop did not — `d.horizontalOverflow` read off
#      `undefined` threw a TypeError *after* `summary.json` was written and *before* `report.md`.
#   3. `main().catch` then OVERWROTE the complete `summary.json` with `{ fatal: ... }`.
#
# `run.sh` reads that file with `summary.counts?.clicks ?? 0` and friends, so every counter
# defaulted to 0 and `docs/qa/QA-LATEST-w4.md` was regenerated as "0 clicks · 0 screenshots ·
# 0 findings" for a pass that had taken 232 screenshots and 292 clicks. The artifacts on disk
# were correct; the report a human reads was a fabrication, and it was indistinguishable from a
# clean pass. That is worse than a red pass, because it is what a REQ gets closed on.
#
# The property being asserted is not "the reporter cannot crash" — it is the one that matters:
# when the pass cannot measure, the report must SAY SO in the first lines a reader sees, and it
# must never present a default as a measurement.
#
# Reads the shipped files, so a re-typed list of properties could not go green while the
# real ones stay broken — the same rule `walkthrough-navigation-probe.cjs` follows.
#
# `set -uo pipefail` is deliberate, and the `grep -q`-in-a-pipeline traps below are the price of
# it. With `pipefail` on, `awk ... | grep -q 'x'` is **flaky, not correct**: `grep -q` exits at the
# first match, `awk` is still writing and takes SIGPIPE (141), and `pipefail` turns that into a
# failed pipeline — so an `if` over it answers "not present" at random. Measured on this file:
# 10 identical runs gave `1000100010` with `pipefail` and `1111111111` without it. A gate whose
# verdict changes run to run on unchanged source is worse than no gate, because it trains the
# reader to re-run it until it agrees. So every check here is one of:
#   * `grep -c` compared numerically  (reads the whole stream, no SIGPIPE), or
#   * `grep -q` with the input **redirected from a file**, never through a pipe.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 4

fail=0
check() {
  if [ "$2" = "1" ]; then
    printf 'PASS  %s\n' "$1"
  else
    printf 'FAIL  %s\n        %s\n' "$1" "$3"
    fail=$((fail + 1))
  fi
}

WALK=scripts/qa/walkthrough.cjs
RUN=scripts/qa/run.sh

# ---------------------------------------------------------------- 1. both loops guard `!d`
# The desktop findings roll-up and the mobile one already guard. The markdown reporter did not,
# and it is the one that runs last and decides whether report.md exists.
report_leg_guards() {
  # Every `d.horizontalOverflow` read inside a loop over a diagnostics-bearing array must be
  # preceded by a `!d` guard in the same loop body. Count guarded reads vs total reads.
  local total guarded
  total=$(grep -c 'd\.horizontalOverflow' "$WALK")
  guarded=$(grep -B 25 'd\.horizontalOverflow' "$WALK" | grep -c 'if (!d)')
  [ "$total" -gt 0 ] && [ "$guarded" -ge 3 ]
}
if report_leg_guards; then
  check "every d.horizontalOverflow read is guarded by a preceding 'if (!d)'" 1
else
  check "every d.horizontalOverflow read is guarded by a preceding 'if (!d)'" 0 \
    "found $(grep -c 'd\.horizontalOverflow' "$WALK") read(s) but only $(grep -B 25 'd\.horizontalOverflow' "$WALK" | grep -c 'if (!d)') guard(s); the markdown reporter's per-page loop is the one that throws"
fi

# The markdown leg must print something for an unmeasured page rather than skipping silently.
if grep -q 'NOT MEASURED' "$WALK"; then
  check "the markdown report marks an unmeasured page instead of reading through it" 1
else
  check "the markdown report marks an unmeasured page instead of reading through it" 0 \
    "no 'NOT MEASURED' line in $WALK; a failed page reaches report.md as a TypeError or vanishes"
fi

# ------------------------------------------------- 2. the catch must not destroy a good summary
# `awk | grep -q` would be the SIGPIPE trap described at the top, so the slice is written to a
# scratch file once and grepped from there.
CATCH=$(mktemp)
trap 'rm -f "$CATCH"' EXIT
awk '/main\(\)\.catch/,/^}\);/' "$WALK" > "$CATCH"

if grep -q 'kept' "$CATCH"; then
  check "the failure handler keeps an existing summary.json instead of overwriting it" 1
else
  check "the failure handler keeps an existing summary.json instead of overwriting it" 0 \
    "main().catch still writes a bare { fatal } summary, destroying the findings the pass had computed"
fi

if grep -q 'reporterFailed' "$CATCH"; then
  check "a reporter that dies after measuring records WHY separately from the results" 1
else
  check "a reporter that dies after measuring records WHY separately from the results" 0 \
    "'reporterFailed' is missing; 'ran and counted' and 'died before counting' share one shape"
fi

# ------------------------------------------------------------ 3. no default may read as a result
# A `fatal` summary has no `counts`. If run.sh renders those as 0 without saying so, the report
# is a lie. It must lead with a banner instead.
if grep -q 'summary\.fatal' "$RUN"; then
  check "a fatal pass leads the report with a banner instead of defaulting to zero" 1
else
  check "a fatal pass leads the report with a banner instead of defaulting to zero" 0 \
    "run.sh still fills '?? 0' for a summary that measured nothing; the report reads as clean"
fi

# The banner must say the zeros are defaults, so a skimmer cannot take the counts as results.
if grep -q 'NOT results' "$RUN"; then
  check "the banner says the zeros are defaults, not results" 1
else
  check "the banner says the zeros are defaults, not results" 0 \
    "the banner exists but never says the numbers above it are defaults"
fi

# `reporterFailed` and `fatal` demand different reactions and must not share a branch.
if grep -q 'summary\.reporterFailed' "$RUN"; then
  check "reporterFailed and fatal are reported as different verdicts" 1
else
  check "reporterFailed and fatal are reported as different verdicts" 0 \
    "only one of the two is handled; a real measurement and a dead pass look identical"
fi

# ------------------------------------- 4. the real artifact must have been the crashing input
# This probe is only meaningful if the property it asserts is the one that just bit. If someone
# rewrites the reporter and the routes change, the check below still has to find the crash text.
if grep -q 'hr-attendance-roster' "$WALK"; then
  check "the route that crashed (hr-attendance-roster) is still in the inventory" 1
else
  check "the route that crashed (hr-attendance-roster) is still in the inventory" 0 \
    "the route was removed; re-verify that the fix still covers the general case before trusting this probe"
fi

echo
if [ "$fail" -eq 0 ]; then
  echo "OK — a pass that measures nothing cannot print a clean report."
else
  echo "$fail check(s) failed — a silent green report is reachable again."
fi
exit $((fail > 0 ? 1 : 0))