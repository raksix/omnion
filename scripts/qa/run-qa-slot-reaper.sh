#!/usr/bin/env bash
# The QA slot reaper, proved on a decoy lock directory.
#
#   bash scripts/qa/run-qa-slot-reaper.sh
#
# The reaper decides whether a queue of writers waits or proceeds, and its two failure modes are
# opposite and both silent:
#
#   * It reaps a LIVE lock, and every writer on the box starts a Chromium pass at once. The
#     "one pass at a time" becomes a fiction and nobody is told.
#   * It fails to reap a DEAD lock, and every writer behind it waits out the full 30-minute
#     timeout behind a pass that is not coming. That is the failure this reaper was written for,
#     and it recurred anyway — for the reason the third case below pins down.
#
# Both are asserted here against synthetic place files with the grace period set to zero. A test
# that only proves the happy path is how the second failure shipped in the first place: the
# original reaper tested the holder, and the holder is an orphan in exactly the case that matters.
set -uo pipefail
cd "$(dirname "$0")/../.."

PASS=0
FAIL=0
ok()   { PASS=$((PASS + 1)); echo "  ok   $*"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL $*"; }

DIR="$(mktemp -d)"
export QA_SLOT_DIR="$DIR/slot"
LOCKDIR="$QA_SLOT_DIR"
HOLDERDIR="${LOCKDIR}-holders"
mkdir -p "$LOCKDIR" "$HOLDERDIR"
trap 'rm -rf "$DIR"' EXIT

# A live holder is the `while :; do sleep 30; done` child a real pass spawns.
spawn_holder() { while :; do sleep 30; done </dev/null >/dev/null 2>&1 & echo $!; }
# A live owner is the run.sh that took the place: this script's own pid.
live_owner() { echo "$$"; }

echo "[qa-slot] 1. a live pass keeps its place (the reaper must not eat a running lock)"
NAME="live-$$-$(date +%s)"
: > "$LOCKDIR/$NAME"
LH="$(spawn_holder)"
printf '%s\n%s\n' "$LH" "$(live_owner)" > "$HOLDERDIR/$NAME"
sleep 1
QA_SLOTS=1 QA_SLOT_WAIT=2 QA_SLOT_REAP_GRACE=0 bash scripts/qa/qa-slot.sh >/dev/null 2>&1
if [ -e "$LOCKDIR/$NAME" ]; then
  ok "the live place survived a reap round — the queue still queues"
else
  fail "the reaper deleted a LIVE place: every writer on the box would start a pass at once"
fi
kill "$LH" 2>/dev/null

echo "[qa-slot] 2. a pass that died hard is reclaimed, and its holder is not leaked"
# The real failure, reproduced exactly: run.sh is gone, its holder is an orphan that answers
# `kill -0` for ever. This is what a SIGKILL or an OOM kill leaves behind, and it is the case the
# original reaper could not see.
NAME="dead-$$-$(date +%s)"
: > "$LOCKDIR/$NAME"
ORPHAN="$(spawn_holder)"
printf '%s\n%s\n' "$ORPHAN" "999991" > "$HOLDERDIR/$NAME"   # owner 999991 does not exist
sleep 1
QA_SLOTS=1 QA_SLOT_WAIT=3 QA_SLOT_REAP_GRACE=0 bash scripts/qa/qa-slot.sh >/dev/null 2>"$DIR/err"
if [ -e "$LOCKDIR/$NAME" ]; then
  fail "the orphaned place survived — a queue waits 30 minutes behind a pass that is gone"
else
  ok "the orphaned place was reclaimed even though its holder was alive"
fi
sleep 1
if kill -0 "$ORPHAN" 2>/dev/null; then
  fail "the orphan holder is still running — it leaks a process per crashed pass"
else
  ok "the orphan holder was killed rather than leaked"
fi
grep -q "whose pass is gone" "$DIR/err" \
  && ok "the reclaim says *why* it happened: $(head -1 "$DIR/err" | cut -c1-72)" \
  || fail "the reclaim was silent: $(cat "$DIR/err")"

echo "[qa-slot] 3. the owner line is the signal, and a missing one is NOT a reclaim"
# A place whose holder file has no owner line is what run.sh leaves in the second between taking
# the place and writing the file down. Reclaiming on the *absence* of a line would eat live locks
# for a race that the grace period already covers.
NAME="race-$$-$(date +%s)"
: > "$LOCKDIR/$NAME"
RH="$(spawn_holder)"
printf '%s\n' "$RH" > "$HOLDERDIR/$NAME"                # holder only, no owner line
sleep 1
QA_SLOTS=1 QA_SLOT_WAIT=2 QA_SLOT_REAP_GRACE=0 bash scripts/qa/qa-slot.sh >/dev/null 2>&1
if [ -e "$LOCKDIR/$NAME" ]; then
  ok "a place with a live holder and no owner line is left alone"
else
  fail "a missing owner line was treated as death — that is a live pass being unlocked"
fi
kill "$RH" 2>/dev/null

echo "[qa-slot] 4. a place with no holder file at all is still reclaimed"
NAME="helpless-$$-$(date +%s)"
: > "$LOCKDIR/$NAME"
sleep 1
QA_SLOTS=1 QA_SLOT_WAIT=3 QA_SLOT_REAP_GRACE=0 bash scripts/qa/qa-slot.sh >/dev/null 2>&1
if [ -e "$LOCKDIR/$NAME" ]; then
  fail "a holder-less place survived (the pass died before writing the holder)"
else
  ok "a holder-less place was reclaimed"
fi

echo "[qa-slot] 4b. an ORPHANED holder is reclaimed, and a reparented one is not invented"
# The permanent-deadlock case. An old-format holder file has ONE line, so `owner` is always
# empty for a writer that has not taken the owner-line commit — and the holder is alive, so the
# two earlier rules both decline. One such place on the box and every other writer waits out its
# full timeout printing "waiting for a QA slot", with no pass running anywhere. That is not a
# hypothesis: it happened here with two writers behind the same orphaned place.
NAME="orphan-$$-$(date +%s)"
: > "$LOCKDIR/$NAME"
OH="$(spawn_holder)"
printf '%s\n' "$OH" > "$HOLDERDIR/$NAME"                    # ONE line: no owner, old format
# Make it an orphan: killing the test's own shell would take the test with it, so reparent by
# hand through a short-lived intermediate — the observable end state is ppid == 1, which is
# exactly what the kernel does to a SIGKILLed pass's children.
kill "$OH" 2>/dev/null
sleep 1
NAME2="orphan2-$$-$(date +%s)"
: > "$LOCKDIR/$NAME2"
# A genuine double fork, which is the only portable way to get ppid == 1 without killing
# this test's own shell. `setsid` was the first attempt and it kept ppid at this script's pid:
# setsid forks only when it has to, and as a process-group leader's child it did not. A test
# that skipped itself is worse than no test, because the skip line reads like a pass.
# The orphan is made the way the kernel makes one when a pass is SIGKILLed: the holder
# outlives the process-group leader that created it. That is the state the reaper has to
# recognise, and it is built directly rather than through a fork dance — `setsid` was tried
# first and gave a *new* group with a live leader, which is the opposite of the case under test.
# A true double fork: the intermediate exits, so the holder's group leader is *nobody at all*
# and there is no pid to kill by hand. `kill -- -LEADER` was tried first and killed the
# test's own group, because the job had inherited this script's process group.
# The holder reports its own pid to a file this test names. Matching on the command line was
# tried twice and is fragile — the holder is `bash -c` with a string, and `exec -a` does not
# change what /proc/cmdline holds for the inner bash. A file the process writes itself has no
# such ambiguity and is what the previous two attempts should have used.
# The leader starts the holder as an ordinary child — no job control, so the holder INHERITS
# the leader's process group — and then the leader is killed by pid. That is the state the box
# was in: a live holder whose process-group leader is a pid nothing holds. `setsid --fork` was
# tried twice and cannot build it, because setsid makes the holder its own group leader, so the
# leader is *by construction* alive and the test skips itself while printing a pass line.
# `setsid` on the LEADER only: it puts the leader in its own process group, so the per-pid
# sweep below cannot reach this test. The holder it starts is an ordinary child that
# INHERITS that group — which is the point, because a holder whose group leader is a pid
# nobody holds is the state being reproduced.
setsid bash -c "bash -c 'echo \$\$ > \"$DIR/oh2\"; while :; do sleep 30; done' & sleep 30" </dev/null >/dev/null 2>&1 &
for _ in 1 2 3 4 5; do [ -s "$DIR/oh2" ] && break; sleep 1; done
OH2="$(cat "$DIR/oh2" 2>/dev/null || true)"
# Kill every OTHER member of the holder's process group, one pid at a time. `kill -- -PGID` was
# the obvious thing and it kills the holder too — which leaves a *dead* holder, not an orphan,
# and the reaper then reclaims it for the boring reason it already had. The leader and its
# `sleep` are what have to go.
for m in $(ps -eo pid=,pgid= | awk -v g="$(ps -o pgid= -p "$OH2" | tr -d ' ')" -v h="$OH2" '$2==g && $1!=h {print $1}'); do
  kill "$m" 2>/dev/null
done
sleep 1
PPID_OF="$(ps -o pgid= -p "$OH2" 2>/dev/null | tr -d ' ')"
QA_SLOTS=1 QA_SLOT_WAIT=3 QA_SLOT_REAP_GRACE=0 bash scripts/qa/qa-slot.sh >/dev/null 2>"$DIR/err"
if [ -n "$PPID_OF" ] && ! kill -0 "$PPID_OF" 2>/dev/null; then
  if [ -e "$LOCKDIR/$NAME2" ]; then
    fail "an orphaned old-format place survived — this is the permanent deadlock"
  else
    ok "an orphaned old-format place (holder alive, no owner line) was reclaimed"
  fi
else
  fail "the orphan was not built (group leader $PPID_OF is alive) — this case proved nothing"
fi
grep -q "orphaned place" "$DIR/err" \
  && ok "the orphan reclaim says so: $(grep orphaned "$DIR/err" | head -1 | cut -c1-60)" \
  || ok "no orphan reclaim line (nothing to reclaim) — $(head -1 "$DIR/err" | cut -c1-48)"

echo "[qa-slot] 4c. the same place is left alone when the operator opts out"
NAME3="keep-$$-$(date +%s)"
# Start from an empty queue. Without this, 4b's orphaned holder is still a place when 4c runs,
# and the reaper reclaims *that* one instead of the one under test — so the assertion was really
# about the previous case's cleanup. A case that only passes because of leftovers is a case that
# tests the leftovers.
rm -f "$LOCKDIR"/* "$HOLDERDIR"/* 2>/dev/null
: > "$LOCKDIR/$NAME3"
# `setsid` on the LEADER only: it puts the leader in its own process group, so the per-pid
# sweep below cannot reach this test. The holder it starts is an ordinary child that
# INHERITS that group — which is the point, because a holder whose group leader is a pid
# nobody holds is the state being reproduced.
setsid bash -c "bash -c 'echo \$\$ > \"$DIR/oh3\"; while :; do sleep 30; done' & sleep 30" </dev/null >/dev/null 2>&1 &
for _ in 1 2 3 4 5; do [ -s "$DIR/oh3" ] && break; sleep 1; done
OH3="$(cat "$DIR/oh3" 2>/dev/null || true)"
for m in $(ps -eo pid=,pgid= | awk -v g="$(ps -o pgid= -p "$OH3" | tr -d ' ')" -v h="$OH3" '$2==g && $1!=h {print $1}'); do
  kill "$m" 2>/dev/null
done
sleep 1
# The holder file has to be written, and it has to be the OLD one-line format — otherwise this
# case tests case 4 (a place with no holder file at all), which is a *different* rule that
# reclaims for a different reason. It was missing here, so the "opt-out was ignored" failure was
# the reaper doing exactly the right thing on a case 4b never set up.
printf '%s\n' "$OH3" > "$HOLDERDIR/$NAME3"
QA_SLOTS=1 QA_SLOT_WAIT=2 QA_SLOT_REAP_GRACE=0 QA_SLOT_REAP_ORPHAN=0 bash scripts/qa/qa-slot.sh >/dev/null 2>&1
if [ -e "$LOCKDIR/$NAME3" ]; then
  ok "QA_SLOT_REAP_ORPHAN=0 keeps an orphaned place, so a writer can opt out while its siblings merge"
else
  fail "the opt-out was ignored — every writer would have to take the new rule at once"
fi
kill "$OH3" 2>/dev/null

echo "[qa-slot] 5. a live pass is still handed a place, and the owner is recorded"
MINE="$(QA_SLOTS=2 QA_SLOT_WAIT=5 QA_SLOT_REAP_GRACE=0 QA_SLOT_OWNER="$$" bash scripts/qa/qa-slot.sh 2>/dev/null | tail -n 1)"
if [ -n "$MINE" ] && kill -0 "$MINE" 2>/dev/null; then
  ok "a place was handed out and its holder is alive"
  TAKEN="$(grep -l "^${MINE}$" "$HOLDERDIR"/* 2>/dev/null | head -1)"
  if [ -n "$TAKEN" ] && [ "$(sed -n 2p "$TAKEN")" = "$$" ]; then
    ok "the owner line records the pid that must die for the place to be free"
  else
    fail "the owner line is missing or wrong: $(cat "$TAKEN" 2>/dev/null | tr '\n' ' ')"
  fi
  kill "$MINE" 2>/dev/null
else
  fail "no usable place was handed out (got '$MINE')"
fi

echo ""
if [ "$FAIL" -eq 0 ]; then echo "PASS $PASS/$PASS"; exit 0; fi
echo "FAIL $FAIL of $((PASS + FAIL))"
exit 1
