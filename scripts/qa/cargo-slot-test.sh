#!/usr/bin/env bash
# Exercise the cargo semaphore: three requests against two slots must run two now and
# queue the third, and no holder may survive the run.
set -uo pipefail
cd "$(dirname "$0")/../.."   # scripts/qa/ -> repo root

SLOT=scripts/qa/cargo-slot.sh
export CARGO_SLOT_DIR=/tmp/omnion-cargo-slots-test
rm -rf "$CARGO_SLOT_DIR" "$CARGO_SLOT_DIR.lock"
mkdir -p "$CARGO_SLOT_DIR"

echo "== TEST 1: empty queue takes a slot at once =="
CARGO_SLOTS=2 bash "$SLOT" sh -c 'echo "  ran, pid=$$"'
echo "  exit=$?"

echo "== TEST 2: three requests, two slots -> third waits =="
( CARGO_SLOTS=2 CARGO_SLOT_WAIT=40 bash "$SLOT" sh -c 'sleep 5; echo "  A(slot) done"' ) 2>/dev/null &
( CARGO_SLOTS=2 CARGO_SLOT_WAIT=40 bash "$SLOT" sh -c 'sleep 5; echo "  B(slot) done"' ) 2>/dev/null &
( CARGO_SLOTS=2 CARGO_SLOT_WAIT=40 bash "$SLOT" sh -c 'echo "  C(waited) ran"' ) 2>/dev/null &
wait
echo "  holders left: $(find "$CARGO_SLOT_DIR" -maxdepth 1 -type f 2>/dev/null | wc -l)"

echo "== TEST 3: a dead holder is reaped, not waited on forever =="
echo 999999 > "$CARGO_SLOT_DIR/999999"      # a pid that does not exist
CARGO_SLOTS=1 CARGO_SLOT_WAIT=10 bash "$SLOT" true 2>/dev/null
echo "  exit=$? (0 means the dead slot was reclaimed)"
echo "  stale holder left: $([ -e "$CARGO_SLOT_DIR/999999" ] && echo yes || echo no)"

echo "== TEST 4: giving up returns EX_TEMPFAIL (75) =="
mkdir -p "$CARGO_SLOT_DIR"
echo $$ > "$CARGO_SLOT_DIR/live"              # occupy the only slot with a live pid
CARGO_SLOTS=1 CARGO_SLOT_WAIT=5 bash "$SLOT" true 2>/dev/null
rc=$?
rm -f "$CARGO_SLOT_DIR/live"
echo "  exit=$rc (75 = busy, caller should retry)"

rm -rf "$CARGO_SLOT_DIR" "$CARGO_SLOT_DIR.lock"
echo "== done =="
