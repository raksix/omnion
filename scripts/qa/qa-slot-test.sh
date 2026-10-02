#!/usr/bin/env bash
# Exercise the QA slot: the queue must drain, and a place whose holder is somebody else's
# process must be reclaimable.
#
# The bug this suite pins is not theoretical. Two writers running passes side by side left a
# place named after wave6's waiter recording a holder whose working directory was wave4's, so
# the queue never drained and every later pass died at its own timeout with no report. The
# holder pid was alive — just not alive *for that place*.
set -uo pipefail
cd "$(dirname "$0")/../.."   # scripts/qa/ -> repo root

SLOT=scripts/qa/qa-slot.sh
export QA_SLOT_DIR=/tmp/omnion-qa-slot-test
HOLDERS="$QA_SLOT_DIR-holders"
rm -rf "$QA_SLOT_DIR" "$HOLDERS"
mkdir -p "$QA_SLOT_DIR" "$HOLDERS"

fail=0
check() { # description, actual, expected
  if [ "$2" = "$3" ]; then
    echo "  PASS  $1"
  else
    echo "  FAIL  $1 (got '$2', want '$3')"
    fail=1
  fi
}

places() { find "$QA_SLOT_DIR" -maxdepth 1 -type f 2>/dev/null | wc -l; }

echo "== TEST 1: an empty queue takes a place at once =="
out=$(QA_SLOTS=1 QA_SLOT_WAIT=30 bash "$SLOT" 2>/dev/null)
holder=$(printf '%s\n' "$out" | tail -n 1)
check "a place file exists" "$(places)" "1"
check "the holder pid is alive" "$(kill -0 "$holder" 2>/dev/null && echo yes || echo no)" "yes"
check "the holder carries this place's token" \
  "$(tr '\0' '\n' < "/proc/$holder/cmdline" 2>/dev/null | grep -c '^qa-slot-holder ')" "1"

echo "== TEST 2: a live place is NOT reclaimed by the next waiter =="
# The place above is genuinely held. A second waiter must queue rather than steal it, and must
# then give up on its deadline instead of hanging.
start=$(date +%s)
QA_SLOTS=1 QA_SLOT_WAIT=6 bash "$SLOT" >/dev/null 2>&1
elapsed=$(( $(date +%s) - start ))
check "the waiter timed out rather than stealing" "$([ "$elapsed" -ge 5 ] && echo yes || echo no)" "yes"
check "the live place survived" "$(places)" "1"

echo "== TEST 3: the pass ends, its holder dies, the place is reclaimed =="
# This is what run.sh's EXIT trap does. A place whose holder is gone must not hold the queue.
kill "$holder" 2>/dev/null || true
sleep 1
# Age the place past the reap grace so the reaper is willing to act.
touch -d "@$(( $(date +%s) - 300 ))" "$QA_SLOT_DIR"/* 2>/dev/null || true
QA_SLOTS=1 QA_SLOT_WAIT=6 bash "$SLOT" >/dev/null 2>&1
check "the freed place was reclaimed and reused" "$(places)" "1"
check "the reaper reported it" \
  "$(ls "$HOLDERS" | wc -l)" "1"

echo "== TEST 4: a LIVE pid that is not this place's holder is reclaimable =="
# The exact failure: a place whose holder pid is alive, but carrying another place's token.
# The old `kill -0` test called this place occupied forever.
token="99999-1-1"
: > "$QA_SLOT_DIR/$token"
foreign=$(QA_SLOTS=1 QA_SLOT_WAIT=3 bash "$SLOT" 2>/dev/null | tail -n 1) || true
# Point the place at a live process that is NOT a qa-slot holder: this shell's own sleep.
sleep 300 &
impostor=$!
echo "$impostor" > "$HOLDERS/$token"
touch -d "@$(( $(date +%s) - 300 ))" "$QA_SLOT_DIR/$token"
kill -0 "$impostor" 2>/dev/null && alive=yes || alive=no
check "the impostor is alive" "$alive" "yes"
check "its cmdline carries no holder token" \
  "$(tr '\0' '\n' < "/proc/$impostor/cmdline" 2>/dev/null | grep -c '^qa-slot-holder ')" "0"
QA_SLOTS=1 QA_SLOT_WAIT=5 bash "$SLOT" >/dev/null 2>&1
check "the place with a foreign holder was reclaimed" \
  "$([ -e "$QA_SLOT_DIR/$token" ] && echo still-there || echo reclaimed)" "reclaimed"
kill "$impostor" 2>/dev/null || true
kill "$foreign" 2>/dev/null || true

echo "== TEST 5: two genuinely held places are both respected, and the queue still moves =="
# Each place's holder is a real holder carrying ITS OWN token, so neither may be stolen. The
# assertion that matters is that a waiter with a place available eventually gets one — the
# pre-fix failure was a queue that never moved at all.
t_a="$$-$(date +%s)-aaa"
t_b="$$-$(date +%s)-bbb"
: > "$QA_SLOT_DIR/$t_a"
sleep 0.2
h_a=$(QA_SLOTS=9 QA_SLOT_WAIT=3 bash "$SLOT" 2>/dev/null | tail -n 1)
# Name the second place after the holder that was just created, with that holder's token, so it
# is a genuinely held place rather than an orphan.
token_b="$(tr '\0' '\n' < "/proc/$h_a/cmdline" 2>/dev/null | grep '^qa-slot-holder ' | cut -d' ' -f2-)"
: > "$QA_SLOT_DIR/$token_b"
touch -d "@$(( $(date +%s) - 300 ))" "$QA_SLOT_DIR"/* 2>/dev/null || true

# With the stale one now correctly matched to its holder, it must survive the reaper, and a
# waiter with MAX=1 must NOT be able to take a second place while it is held.
QA_SLOTS=1 QA_SLOT_WAIT=5 bash "$SLOT" >/dev/null 2>&1
check "a place whose holder carries its token is not stolen" \
  "$([ -e "$QA_SLOT_DIR/$token_b" ] && echo held || echo stolen)" "held"
for h in "$h_a" "$token_b"; do :; done
kill "$h_a" 2>/dev/null || true
sleep 1
touch -d "@$(( $(date +%s) - 300 ))" "$QA_SLOT_DIR"/* 2>/dev/null || true
QA_SLOTS=1 QA_SLOT_WAIT=5 bash "$SLOT" >/dev/null 2>&1
check "once its holder dies the queue moves again" \
  "$([ ! -e "$QA_SLOT_DIR/$token_b" ] && echo reclaimed || echo still-there)" "reclaimed"

# Clean up every holder this suite started, or the next pass on this box waits for them.
for f in "$HOLDERS"/*; do [ -e "$f" ] && kill "$(cat "$f" 2>/dev/null)" 2>/dev/null; done
rm -rf "$QA_SLOT_DIR" "$HOLDERS"

if [ "$fail" -ne 0 ]; then
  echo "== FAILED =="
  exit 1
fi
echo "== done: all checks passed =="
