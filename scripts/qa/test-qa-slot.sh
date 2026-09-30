#!/usr/bin/env bash
# Omnion QA — the slot, tested.
#
# The slot is a semaphore three worktrees share through /tmp, and its failure modes are all
# silent: a pass that never starts prints "waiting for a QA slot" and reports nothing, and a pass
# that starts without a place puts two Chromiums on a box that cannot hold them. There is no
# assertion anywhere else in the harness that either happened, so the wait loop is exercised here
# against real files, real pids and real clocks instead of being read.
#
#   QA_SLOT_DIR=/tmp/omnion-qa-slot-test bash scripts/qa/test-qa-slot.sh
#
# Each case gets its own LOCKDIR so it never touches the live queue: a test that reaped a real
# sibling's place would be the very outage it is trying to catch.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SLOT="$ROOT/scripts/qa/qa-slot.sh"

WORK="$(mktemp -d /tmp/omnion-slot-test-XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

pass=0
fail=0
ok()   { pass=$((pass + 1)); echo "  ok   — $1"; }
bad()  { fail=$((fail + 1)); echo "  FAIL — $1"; }
check() { if [ "$1" = "0" ]; then ok "$2"; else bad "$2 ${3:-}"; fi; }

# A place is a file in LOCKDIR named after the pid that took it, and a holder pid beside it in
# LOCKDIR-holders. The holder lives as long as the pass that owns the place; the place's own name
# is dead by design the moment the taking script exits.
#
# `plant` writes one of each. `plant_dead` writes a place whose holder is a pid that has already
# exited -- a crashed pass, which is the only way a place is ever orphaned.
plant() { # lockdir place_name holder_pid
  mkdir -p "$1" "$1-holders"
  : > "$1/$2"
  printf '%s\n' "$3" > "$1-holders/$2"
}
plant_dead() { # lockdir
  local dir="$1" name="dead-$$-$RANDOM"
  mkdir -p "$dir" "$dir-holders"
  : > "$dir/$name"
  # A pid that cannot be running: start a subshell, let it exit, then write its pid down.
  local pid
  bash -c 'exit 0' & pid=$!
  wait "$pid" 2>/dev/null
  printf '%s\n' "$pid" > "$dir-holders/$name"
  # Backdate the place past the grace period so the reaper is allowed to judge it.
  touch -d '10 minutes ago' "$dir/$name"
  printf '%s' "$name"
}

echo "qa-slot: case 1 — a free place is granted, and the holder keeps it alive"
d="$WORK/case1"
mkdir -p "$d" "$d-holders"
out="$(QA_SLOT_DIR="$d" QA_SLOTS=1 bash "$SLOT" 2>/dev/null | tail -n 1)"
holder="$(printf '%s' "$out" | awk '{print $NF}')"
if [ -n "$holder" ] && kill -0 "$holder" 2>/dev/null; then ok "granted a place and printed a live holder pid ($holder)"; else bad "no live holder pid printed (got '${out:-}')"; fi
if [ "$(find "$d" -maxdepth 1 -type f | wc -l)" = "1" ]; then ok "exactly one place file exists"; else bad "expected 1 place file"; fi
if [ -s "$d-holders/$(basename "$(find "$d" -maxdepth 1 -type f | head -1)")" ]; then ok "the holder pid is written down beside the place"; else bad "no holder file"; fi
# The holder must not hold the caller's stdout: run.sh reads it through `| tail -n 1`, and a
# holder that inherited that pipe would keep the substitution open forever. A FRESH directory:
# the invocation above still holds this one, so reusing it would block and time out — a test that
# fails for a reason it never measured.
d2="$WORK/case1-stdout"
mkdir -p "$d2" "$d2-holders"
if timeout 10 bash -c 'QA_SLOT_DIR="'"$d2"'" QA_SLOTS=1 bash "'"$SLOT"'" 2>/dev/null | tail -n 1' >/dev/null 2>&1; then
  ok "the script's stdout closes (a waiting caller is not held open by the holder)"
else
  bad "the command substitution never saw EOF — the holder inherited stdout"
fi
rm -f "$d2"/* "$d2-holders"/* 2>/dev/null
# Free the place the way run.sh's trap does.
[ -n "$holder" ] && kill "$holder" 2>/dev/null
rm -f "$d"/* "$d-holders"/* 2>/dev/null

echo "qa-slot: case 2 — a live place blocks a second pass, and the blocked pass TIMES OUT"
d="$WORK/case2"
mkdir -p "$d" "$d-holders"
# A live holder, held by a sleep that is still running.
sleep 120 & live=$!
plant "$d" "live-$$-$RANDOM" "$live"
start=$SECONDS
err="$(QA_SLOT_DIR="$d" QA_SLOTS=1 QA_SLOT_WAIT=6 bash "$SLOT" 2>&1 >/dev/null)"
elapsed=$((SECONDS - start))
if printf '%s' "$err" | grep -q "no place after"; then ok "reported giving up ($elapsed s)"; else bad "did not report a timeout: $err"; fi
if [ "$elapsed" -ge 5 ] && [ "$elapsed" -le 40 ]; then ok "waited for its own deadline instead of returning at once ($elapsed s)"; else bad "returned after $elapsed s, outside the 6 s deadline"; fi
if [ "$(find "$d" -maxdepth 1 -type f | wc -l)" = "1" ]; then ok "the live owner's place is still there"; else bad "the live place was stolen or removed"; fi
kill "$live" 2>/dev/null; wait "$live" 2>/dev/null
rm -f "$d"/* "$d-holders"/* 2>/dev/null

echo "qa-slot: case 3 — a CRASHED pass's place is reclaimed by a pass that is already waiting"
d="$WORK/case3"
mkdir -p "$d" "$d-holders"
# The owner dies holding the place: this is the state a crashed/interrupted pass leaves behind,
# and the case that had no coverage at all until the wait loop started reaping on every turn.
name="$(plant_dead "$d")"
if [ -f "$d/$name" ]; then ok "planted an orphaned place ($name, holder already gone)"; else bad "failed to plant an orphan"; fi
# A pass that is ALREADY waiting: it must reclaim the orphan and proceed, not sit on it.
out="$(QA_SLOT_DIR="$d" QA_SLOTS=1 QA_SLOT_WAIT=30 bash "$SLOT" 2>/dev/null | tail -n 1)"
new_holder="$(printf '%s' "$out" | awk '{print $NF}')"
if [ -n "$new_holder" ] && kill -0 "$new_holder" 2>/dev/null; then ok "the waiting pass reclaimed the orphan and took the place ($new_holder)"; else bad "the waiting pass never got a place (out='${out:-}')"; fi
if [ ! -f "$d/$name" ] && [ ! -f "$d-holders/$name" ]; then ok "the orphan's place AND holder file are both gone"; else bad "the orphan was only half reclaimed"; fi
[ -n "$new_holder" ] && kill "$new_holder" 2>/dev/null
rm -f "$d"/* "$d-holders"/* 2>/dev/null

echo "qa-slot: case 4 — reaping is a LOOP, not a one-shot before the wait"
d="$WORK/case4"
mkdir -p "$d" "$d-holders"
# The regression this whole file exists for. A pass is ALREADY waiting when the owner of the place
# it is waiting for dies: the place stays, the holder does not, and nothing in the loop can tell
# the difference between "busy" and "corpse" unless the reaper runs again. Reaping once before the
# loop -- which is what the script used to do -- leaves the corpse in place, the pass burns its
# whole QA_SLOT_WAIT and then proceeds with no place at all, which on this box is two Chromiums
# instead of one.
#
# The first draft of this case planted no place at all, so the pass was granted one in 0 s and the
# "it did not time out" assertion passed without the wait ever happening: a check that cannot
# fail. It then "failed" for a second unmeasured reason -- the real reaper's grace is 120 s, longer
# than the 40 s the case allowed, so a place that had just been written can never be reaped inside
# that window. Both are properties of the case, not of the script, so the timings are now explicit
# and every one of them is asserted rather than assumed: a short grace so the loop's reaping is
# observable, and a pass that is demonstrably waiting before the owner dies.
sleep 6 & live=$!
plant "$d" "live4-$$-$RANDOM" "$live"
start=$SECONDS
# The owner dies at ~6 s. The pass is asleep in the loop from ~0 s.
#
# The marker is written OUTSIDE the lock directory. The first draft put `owner-died` inside it,
# and the reaper — correctly, and to the test's own cost — deleted it: a file in LOCKDIR with no
# holder IS an orphaned place, so the script under test ate the evidence the assertion read. A
# probe that lives in the directory being probed gets probed.
MARKER="$WORK/case4-owner-died"
( sleep 6; echo dead > "$MARKER" ) &
planter=$!
err="$(QA_SLOT_DIR="$d" QA_SLOTS=1 QA_SLOT_WAIT=40 QA_SLOT_REAP_GRACE=1 bash "$SLOT" 2>&1 >/tmp/qa-slot-case4.out)"
elapsed=$((SECONDS - start))
wait "$planter" 2>/dev/null
kill "$live" 2>/dev/null; wait "$live" 2>/dev/null
# The case only means anything if the pass really did wait AND the owner really did die while it
# was waiting. An instant grant, or a pass that finished before the owner died, proves nothing
# about the loop — and reading the two assertions below without these is reading a green light
# that a broken script produces just as happily.
if [ -f "$MARKER" ]; then
  ok "the owner died while the pass was still waiting (the corpse the reaper has to find)"
else
  bad "the owner outlived the pass — the case never created a corpse to reclaim"
fi
if [ "$elapsed" -ge 4 ]; then
  ok "the pass really did wait for a place (${elapsed}s, so the loop ran)"
else
  bad "the pass was granted a place in ${elapsed}s — the case never exercised the wait loop"
fi
if printf '%s' "$err" | grep -q "reclaimed a stale place"; then ok "the reaper noticed the orphan appear mid-wait"; else bad "the reaper never ran during the wait: $err"; fi
if printf '%s' "$err" | grep -q "no place after"; then
  bad "the pass gave up on a place it could have reclaimed (waited ${elapsed}s)"
else
  ok "the pass took the place instead of timing out (${elapsed}s)"
fi
rm -f "$d"/* "$d-holders"/* /tmp/qa-slot-case4.out 2>/dev/null

echo "qa-slot: case 5 — a place younger than the grace period is never reclaimed"
d="$WORK/case5"
mkdir -p "$d" "$d-holders"
name="young-$$-$RANDOM"
plant "$d" "$name" "$$" # this shell is alive, and the place was written a moment ago
out="$(QA_SLOT_DIR="$d" QA_SLOTS=1 QA_SLOT_WAIT=2 bash "$SLOT" 2>&1 >/dev/null)"
if printf '%s' "$out" | grep -q "reclaimed a stale place"; then
  bad "reclaimed a place inside the grace period — a pass can be killed between taking the place and writing the holder"
else
  ok "a young place was left alone"
fi
rm -f "$d"/* "$d-holders"/* 2>/dev/null

echo
echo "qa-slot: ${pass} passed, ${fail} failed"
[ "$fail" = "0" ] || exit 1
