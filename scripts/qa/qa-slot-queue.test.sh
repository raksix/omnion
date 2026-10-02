#!/usr/bin/env bash
# The QA slot queue must be a function of who is ALIVE, not of who was alive when the
# waiter started.
#
# `qa-slot.sh` reaped stale places once, before the wait loop. A holder that died WHILE
# waiters were queued was therefore never reclaimed: every later waiter counted the dead
# place, printed "waiting for a QA slot" and sat out its whole deadline. On this box that
# was twenty-two waiters behind one dead holder, each holding a loop's tick open for an
# hour. A queue that cannot recover from a crash is a queue that converts one crashed pass
# into a box-wide stall.
#
# Two properties, and the second is the one a reaper fix can easily break:
#   1. a place whose holder is GONE is reclaimed, and the waiter proceeds;
#   2. a place whose holder is ALIVE is left alone — otherwise a pass that is working
#      loses its place to the queue behind it and two passes run at once.
#
# This runs against a THROWAWAY slot directory (`QA_SLOT_DIR`), never the live one: the
# live queue is shared with every other writer and a test that touched it would be
# stealing somebody's pass.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
SLOT="$ROOT/scripts/qa/qa-slot.sh"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
export QA_SLOT_DIR="$WORK/slot"
HOLDERS="$QA_SLOT_DIR-holders"
mkdir -p "$QA_SLOT_DIR" "$HOLDERS"

fail() { echo "FAIL $*"; exit 1; }

# Run the slot script, keeping stdout and stderr apart: the holder pid is on stdout
# (run.sh reads it with `tail -n 1`) and every narration line is on stderr. Merging them
# puts a log line last and the test then rejects a perfectly good pid.
run_slot() {
  local errfile="$WORK/stderr.$$"
  OUT_ERR="$(QA_SLOTS=1 QA_SLOT_WAIT="$1" QA_SLOT_REAP_GRACE=120 bash "$SLOT" 2>"$errfile")"
  RC=$?
  ERR="$(cat "$errfile" 2>/dev/null)"
  rm -f "$errfile"
}

# --- 1. A LIVE holder keeps its place ----------------------------------------
# `$$` is this script, which is running: the reaper must not take a place whose holder
# can still answer kill -0, or a working pass loses its slot to the queue behind it.
: > "$QA_SLOT_DIR/live-1"
echo $$ > "$HOLDERS/live-1"
touch -d '10 minutes ago' "$QA_SLOT_DIR/live-1" "$HOLDERS/live-1"

run_slot 6
if [ "$RC" -ne 0 ]; then fail "qa-slot.sh exited $RC: $ERR"; fi
if [ ! -f "$QA_SLOT_DIR/live-1" ]; then
  fail "the reaper took a place whose holder is ALIVE"
fi
if [ -n "$OUT_ERR" ]; then
  fail "a live place was granted on top of an existing one (stdout: $OUT_ERR) — two passes would run at once"
fi
case "$ERR" in
  *"no place after"*) echo "ok   a live holder keeps its place and the waiter waits" ;;
  *) fail "the waiter did not report waiting: $ERR" ;;
esac
rm -f "$QA_SLOT_DIR/live-1" "$HOLDERS/live-1"

# --- 2. A DEAD holder's place is reclaimed -----------------------------------
# This is the defect, and the timing IS the defect: a place that is already dead when
# the waiter starts is reaped by the pre-loop call, which the old script also had. What
# the old script could not do was reclaim a place whose holder dies WHILE the waiter is
# already polling. So the sequence has to be: start a waiter, let it fail to get a
# place, THEN kill the holder, and require the waiter to wake up and take it.
: > "$QA_SLOT_DIR/busy-1"
# The holder is a real, killable process standing in for a running pass.
sleep 30 &
busy_pid=$!
echo "$busy_pid" > "$HOLDERS/busy-1"
touch -d '10 minutes ago' "$QA_SLOT_DIR/busy-1" "$HOLDERS/busy-1"
kill -0 "$busy_pid" 2>/dev/null || fail "the stand-in holder is not running"

errfile="$WORK/stderr.concurrent"
( QA_SLOTS=1 QA_SLOT_WAIT=45 QA_SLOT_REAP_GRACE=1 bash "$SLOT" >"$WORK/out.concurrent" 2>"$errfile" ) &
waiter=$!

# Let the waiter start, count the place, and begin polling.
sleep 6
if ! kill -0 "$waiter" 2>/dev/null; then
  kill "$busy_pid" 2>/dev/null
  fail "the waiter exited before the holder died — it took a place it should not have"
fi
if [ -f "$WORK/out.concurrent" ] && [ -s "$WORK/out.concurrent" ]; then
  kill "$busy_pid" 2>/dev/null
  fail "the waiter was granted a place while a live holder owned one"
fi
echo "ok   the waiter is queued behind a live holder"

# Now the pass that owned the place crashes: its holder dies and nothing releases it.
kill -9 "$busy_pid" 2>/dev/null
wait "$waiter" 2>/dev/null
OUT_ERR="$(cat "$WORK/out.concurrent" 2>/dev/null)"
ERR="$(cat "$errfile" 2>/dev/null)"

if [ -f "$QA_SLOT_DIR/busy-1" ]; then
  fail "the crashed holder's place was never reclaimed — every later waiter would sit out its full deadline"
fi
echo "ok   a holder that dies mid-wait is reclaimed and the waiter proceeds"

if [ "$(printf '%s\n' "$ERR" | grep -c 'reclaimed a stale place')" -lt 1 ]; then
  fail "the reclaim was silent (stderr: $ERR)"
fi
echo "ok   the reclaim is announced, not silent"

places="$(find "$QA_SLOT_DIR" -maxdepth 1 -type f | wc -l)"
if [ "$places" -ne 1 ]; then
  fail "expected exactly one place after the queue drained, found $places"
fi
echo "ok   the waiter took the freed place"

# --- 3. The granted holder is alive and releasable ----------------------------
# A holder that dies with the script leaves a place nothing will ever release — the same
# stall by a different route. run.sh kills this pid in its EXIT trap.
holder="$(printf '%s\n' "$OUT_ERR" | tail -n 1)"
case "$holder" in
  ''|*[!0-9]*) fail "the holder pid on stdout is not a number: '$holder' (stdout: $OUT_ERR)" ;;
esac
if kill -0 "$holder" 2>/dev/null; then
  echo "ok   the holder is alive and can hold the place"
  kill "$holder" 2>/dev/null
else
  fail "the holder pid $holder is already dead — the place would never be released"
fi

echo
echo "qa-slot queue recovery: all cases pass"
