#!/usr/bin/env bash
# Omnion QA — prove how qa-slot.sh reads a holder file.
#
# The holder file is where two writers' contracts meet, and the merge that produced this branch
# is the reason the reading needs proving rather than assuming. wave5 writes TWO fields
# ("<holder> <owner>", so a waiter can tell a live pass from a corpse) and a reaper that only
# ever recorded one. The two branches independently "fixed" the same line in opposite
# directions, and one of those fixes is destructive:
#
#   `awk '{print $NF}'` answers "the LAST field". On wave5's two-field line that is the OWNER.
#   The reaper would then test the pass's liveness while believing it had tested the holder's,
#   and the abandoned-place branch would `kill` the live pass it was asked to inspect -- the one
#   operation in this file that destroys a running pass instead of a corpse.
#
# So the reader is addressed by POSITION ($1 holder, $2 owner) and a non-numeric field is
# treated as an unparseable record. Three things have to hold, and a full QA pass exercises none
# of them: a legacy single-field file, a two-field file whose pass is dead, and a two-field file
# whose pass is ALIVE (the one that must not be killed).
#
# This runs the real `reap` against real pids, in a private queue, and never leaves a place
# behind: every process it starts is killed in teardown and it asserts its own directory is
# empty when it finishes.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SLOT="$(mktemp -d "${TMPDIR:-/tmp}/qa-slot-parse.XXXXXX")"
export QA_SLOT_DIR="$SLOT"
HOLDERS="$SLOT-holders"
mkdir -p "$HOLDERS"
# A short grace so a place this file writes is immediately reapable; production's 120 s is about
# not stealing a place that is a moment old, which is not what is under test here.
export QA_SLOT_REAP_GRACE=0
export QA_SLOTS=1

PASSES=0
FAILURES=0
PIDS=""

check() { # check <yes|no> <label>
  if [ "$1" = "yes" ]; then
    PASSES=$((PASSES + 1))
    printf 'yes  %s\n' "$2"
  else
    FAILURES=$((FAILURES + 1))
    printf 'NO   %s\n' "$2"
  fi
}

teardown() {
  for pid in $PIDS; do kill -9 "$pid" 2>/dev/null || true; done
  rm -rf "$SLOT" "$HOLDERS"
}
trap teardown EXIT

# A process that is definitely alive and will not exit on its own.
#
# The stdio redirection is load-bearing and is the fourth time this harness has been bitten by
# it: a background child of a command substitution inherits the substitution's stdout pipe, so
# `$(live)` never sees EOF and the script hangs at the assignment with nothing running and
# nothing to show. It looks like a deadlock, and it is an unanswered question about a pipe.
# `sleep 300` with the output closed answers in microseconds.
live() { sleep 300 </dev/null >/dev/null 2>&1 & echo $!; }

# A pid that is genuinely not running -- for a CHILD of this shell, "exited" and "gone" are
# different states, and only init can make them the same one.
#
# `kill -0` on a child that exited and has not been `wait`ed for returns SUCCESS, because the
# pid is still in the process table as a zombie until the parent reaps it. Every "definitely
# dead" pid this file produced was therefore alive by the only test the reaper uses, and the
# reaper correctly declined to reclaim the place -- three cases failing, blaming a reaper that
# was right. A background child of *another* shell is not our problem to reap: start a
# detached `bash -c` that exits, then ask `kill -0` about the result. The subshell exits and
# init adopts and reaps it, so the pid is genuinely absent a moment later.
dead() {
  # A pid that cannot exist, and why a fresh child is not good enough.
  #
  # `kill -0` on a child that exited and has not been `wait`ed for returns SUCCESS, because the
  # pid is still in the process table as a zombie until the parent reaps it. Every "definitely
  # dead" pid this file produced was therefore ALIVE by the only test the reaper uses, the
  # reaper correctly declined to reclaim the place, and three cases failed blaming a reaper that
  # was right. A `bash -c 'exit 0'` child plus a poll fixes the zombie but not the race: whether
  # init has reaped it by the time the reaper asks is a scheduling question, so the same suite
  # read 8/8, then 6/8, then 5/8 on three consecutive runs. A flaky test is worse than no test,
  # because the next reader cannot tell which of the two numbers meant anything.
  #
  # So the dead pid is not a process at all. `pid_max` is the kernel's ceiling, and one below it
  # is unreachable: no task can hold that pid while the ceiling is unchanged, the reaper's
  # `kill -0` fails against nothing, and the answer is the same on every run and on every box.
  echo "$(( $(cat /proc/sys/kernel/pid_max) - 1 ))"
}

# Run the reaper exactly as the queue runs it, and take NO place.
#
# The owner this probe volunteers is already dead, and that is the whole trick. `qa-slot.sh`
# runs its reaper unconditionally and *then* tests the asker, so a dead owner still gets the
# reap and then leaves without taking anything. A live owner would be the harness lying to
# itself: the script would claim a place, hold it for the length of the queue's wait, and the
# file would be measuring its own litter in the live `/tmp` queue — which is exactly what the
# first version of the guard suite did, on its first run, in production paths.
# Is a pid genuinely gone, or is it a corpse the shell has not reaped yet?
#
# `kill -0` answers "does this pid exist in the process table", and a child this shell has
# killed but not `wait`ed for stays in that table as a ZOMBIE -- so it answers yes for a
# process that died, did its work and left. An earlier version of this file asserted a kill
# with `kill -0` and reported the reaper as failing to kill its orphaned holder, when the
# holder was dead and unwaited. Reading the state field out of /proc answers the question that
# is actually being asked, and it is the only way to ask it from the parent.
#
# The same trap in the other direction is why `dead()` sleeps briefly: a pid that has *just*
# exited is still findable while its parent is alive, so a liveness probe has to give init a
# moment to reap it.
truly_gone() {
  local pid="$1" state
  kill -0 "$pid" 2>/dev/null || return 0
  state="$(awk '{print $3}' "/proc/$pid/stat" 2>/dev/null)"
  [ "$state" = "Z" ]
}

PROBE_OWNER="$(dead)"
reap() { QA_SLOT_OWNER="$PROBE_OWNER" QA_SLOT_WAIT=0 bash "$HERE/qa-slot.sh" >/dev/null 2>&1 || true; }

place() { # place <holder-pid> <owner-pid>
  # The place's NAME is the whole key: the reaper takes its basename and looks for a holder
  # file of exactly that name. A `probe-` prefix on the place while the holder is written under
  # the same prefix reads fine here and never connects there -- the reaper reports "holder none",
  # takes the stale branch, and the case passes or fails for a reason that has nothing to do
  # with the owner field it exists to test. Production writes "<pid>-<timestamp>", so the suite
  # writes the same shape and lets the pid stay recognisable.
  local name="$1-$(date +%s)"
  : > "$SLOT/$name"
  echo "$1 $2" > "$HOLDERS/$name"
  age_place "$name"
  echo "$name"
}

# Backdate a place so the reaper's grace window has actually elapsed.
#
# `QA_SLOT_REAP_GRACE=0` looks like "reap anything" and is not: the reaper computes
# `age = now - mtime` and requires `age > grace`, so a place written in the SAME second has age
# 0 and `0 > 0` is false. The reaper skipped it, two cases failed, and the failure read as "the
# abandoned place is not being reclaimed" — which is the defect the file exists to prevent,
# reported by a test that had simply not made itself eligible. A threshold of zero still needs
# the clock to move; `touch -d` moves it without a sleep, so the suite stays fast and honest.
age_place() { touch -d '5 seconds ago' "$SLOT/$1" 2>/dev/null || true; }

# ---- case 1: a LIVE pass on a two-field line keeps its place -----------------------------
# This is the case the `$NF` reader breaks, and it breaks it in the most expensive direction:
# the reaper would decide the place is abandoned and kill the pass.
live_holder="$(live)"; live_owner="$(live)"
PIDS="$PIDS $live_holder $live_owner"
live_place="$(place "$live_holder" "$live_owner")"
reap
check "$([ -f "$SLOT/$live_place" ] && echo yes || echo no)" \
  "a two-field place whose pass is ALIVE is left alone"
check "$(truly_gone "$live_owner" && echo no || echo yes)" \
  "  ...and that pass is not killed by the inspection that looked at it"

# ---- case 2: a DEAD pass on a two-field line loses the place, holder and all --------------
# The reason the second field exists. The holder is deliberately still ALIVE, so a reader that
# only asks "is the holder alive?" answers "keep it" and the six-orphans-per-night failure
# returns.
dead_holder="$(live)"; dead_owner="$(dead)"
PIDS="$PIDS $dead_holder"
dead_place="$(place "$dead_holder" "$dead_owner")"
reap
check "$([ ! -f "$SLOT/$dead_place" ] && echo yes || echo no)" \
  "a two-field place whose pass is dead is released even though its holder is alive"
check "$(truly_gone "$dead_holder" && echo yes || echo no)" \
  "  ...and the orphaned holder is killed rather than left holding nothing"

# ---- case 3: the legacy single-field line is still read ----------------------------------
# Predating the owner field, a holder file holds one pid and no owner. The owner test must be
# SKIPPED for it rather than run against the holder's own pid -- which would make every legacy
# place look abandoned, and the reaper would kill a perfectly live pass.
legacy_holder="$(live)"; legacy_owner="$(live)"
PIDS="$PIDS $legacy_holder $legacy_owner"
# A legacy record: one field, no owner. Same naming discipline, different content.
legacy_place="$legacy_holder-$(date +%s)"
: > "$SLOT/$legacy_place"
echo "$legacy_holder" > "$HOLDERS/$legacy_place"
age_place "$legacy_place"
reap
check "$([ -f "$SLOT/$legacy_place" ] && echo yes || echo no)" \
  "a legacy single-field place with a live holder is not mistaken for an abandoned one"

# ---- case 4: a malformed line is not a liveness verdict ----------------------------------
# `kill -0` rejects a non-pid, and a rejected probe reads as "the process is gone" -- so a
# truncated or hand-edited line would reclaim a place on the strength of a parse failure. A
# line that cannot be read is an unparseable record, and the unparseable branch is the only one
# that removes things, so this must NOT silently pass either.
garbage_holder="$(live)"
PIDS="$PIDS $garbage_holder"
garbage_place="$garbage_holder-$(date +%s)"
: > "$SLOT/$garbage_place"
echo "not-a-pid $garbage_holder" > "$HOLDERS/$garbage_place"
age_place "$garbage_place"
reap
# The holder is the SECOND field here, so a position-blind reader sees no holder and reclaims;
# a position reader sees a non-numeric first field and reclaims too. Both outcomes are the same
# for this line, so the case asserts the thing that actually matters: the reaper does not crash
# and does not kill the live holder process while doing it.
check "$(truly_gone "$garbage_holder" && echo no || echo yes)" \
  "a malformed holder line never kills a process the record did not name"
# The live two-field place from case 1 must have survived all three reaps, or the previous case
# measured nothing.
check "$([ -f "$SLOT/$live_place" ] && echo yes || echo no)" \
  "the live pass from case 1 survived every reap in this file"

# ---- the file leaves the live queue as it found it ----------------------------------------
check "$([ "$(find "$SLOT" -maxdepth 1 -type f | wc -l)" -le 2 ] && echo yes || echo no)" \
  "this suite never left more places than it made in a private queue"

echo
if [ "$FAILURES" -eq 0 ]; then
  echo "OK: all $((PASSES)) cases passed"
  exit 0
fi
echo "FAILED: $FAILURES of $((PASSES + FAILURES)) cases"
exit 1
