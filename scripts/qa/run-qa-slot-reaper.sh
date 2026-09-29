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
