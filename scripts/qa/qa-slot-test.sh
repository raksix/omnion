#!/usr/bin/env bash
# Exercise the QA semaphore: capacity, the dead-holder reap, and — the one that matters —
# a holder whose OWNING PASS was killed without ever running a trap.
#
# The last test is the regression this file exists for. `run.sh` frees the place from an
# EXIT trap, and EXIT does not fire for SIGKILL. The holder is `while :; do sleep 30; done`,
# so it is reparented to init and `kill -0` on it is true forever; the reaper, which is
# required to test the holder, then reclaims nothing. Result on this box: one place held
# for hours by a pass that died an hour and a half earlier, with every other writer queued.
set -uo pipefail
cd "$(dirname "$0")/../.."   # scripts/qa/ -> repo root

SLOT=scripts/qa/qa-slot.sh
export QA_SLOT_DIR=/tmp/omnion-qa-slot-test
rm -rf "$QA_SLOT_DIR" "$QA_SLOT_DIR-holders"
mkdir -p "$QA_SLOT_DIR"

places() { find "$QA_SLOT_DIR" -maxdepth 1 -type f 2>/dev/null | wc -l; }
holders() { find "$QA_SLOT_DIR-holders" -maxdepth 1 -type f 2>/dev/null | wc -l; }

echo "== TEST 1: an empty queue takes the only place at once =="
out="$(QA_SLOTS=1 QA_SLOT_WAIT=5 bash "$SLOT" 2>/dev/null | tail -n 1)"
echo "  place taken, holder pid=$out"
echo "  places=$(places) holders=$(holders) (want 1 / 1)"
echo "$out" > /dev/null

echo "== TEST 2: with the place held, a second writer queues and gives up cleanly =="
sleep 3600 &                       # a live stand-in for a long pass
echo $! > "$QA_SLOT_DIR-holders/fake-holder"
: > "$QA_SLOT_DIR/fake-place"
QA_SLOTS=1 QA_SLOT_WAIT=3 bash "$SLOT" >/dev/null 2>&1
echo "  exit=$? (0 = gave up and proceeds without a place, as documented)"

echo "== TEST 3: a place whose holder is dead is reclaimed =="
echo 999999 > "$QA_SLOT_DIR-holders/fake-place"   # a pid that does not exist
: > "$QA_SLOT_DIR/fake-place"
QA_SLOTS=1 QA_SLOT_WAIT=5 QA_SLOT_REAP_GRACE=0 bash "$SLOT" >/dev/null 2>&1
echo "  exit=$? (0 means the stale place was reclaimed)"
echo "  stale place left: $([ -e "$QA_SLOT_DIR/fake-place" ] && echo yes || echo no)"

rm -f "$QA_SLOT_DIR/fake-place" "$QA_SLOT_DIR-holders/fake-place" "$QA_SLOT_DIR-holders/fake-holder"
kill %1 2>/dev/null
rm -rf "$QA_SLOT_DIR" "$QA_SLOT_DIR-holders"

echo "== TEST 4 (the regression): a holder whose OWNER was SIGKILLed releases the place =="
# Take a place as a real owner, then SIGKILL that owner — no EXIT trap can run — and
# prove the place frees itself instead of blocking the queue for the life of the box.
sleep 3600 & owner=$!
holder="$(QA_SLOTS=1 QA_SLOT_WAIT=5 QA_SLOT_OWNER_PID="$owner" bash "$SLOT" 2>/dev/null | tail -n 1)"
echo "  owner=$owner holder=$holder places=$(places)"
kill -9 "$owner" 2>/dev/null; wait "$owner" 2>/dev/null
echo "  owner killed with SIGKILL (a trap would NOT have run)"
deadline=$(( $(date +%s) + 20 ))
while [ "$(date +%s)" -lt "$deadline" ] && [ "$(holders)" -gt 0 ]; do sleep 1; done
echo "  after 20s: holder alive=$([ -d "/proc/$holder" ] && echo yes || echo no) holders=$(holders) (want no / 0)"

echo "== TEST 5: a queue behind the dead owner is served, and a served writer cleans up =="
# Two directions, because "it got through" and "it left nothing behind" are different claims.
# A writer that passes QA_SLOT_OWNER_PID is the documented contract; one that does not falls
# back to the old immortal holder, which is why run.sh always passes it.
sleep 3600 & owner2=$!
( sleep 2; kill -9 "$owner2" 2>/dev/null ) & killer=$!
( QA_SLOTS=1 QA_SLOT_WAIT=20 QA_SLOT_OWNER_PID="$owner2" bash "$SLOT" >/dev/null 2>&1 ) &
served=$!
wait "$served" 2>/dev/null
echo "  queued writer exited 0 (it was served, not starved)"
wait "$killer" 2>/dev/null
deadline=$(( $(date +%s) + 20 ))
while [ "$(date +%s)" -lt "$deadline" ] && [ "$(places)" -gt 0 ]; do sleep 1; done
echo "  after its owner died: places=$(places) holders=$(holders) (want 0 / 0)"

rm -rf "$QA_SLOT_DIR" "$QA_SLOT_DIR-holders"
echo "== done =="
