#!/usr/bin/env bash
# Is "reap on every wait turn" the correct shape for the QA slot's stale-place reaper, checked
# against every state a place can be found in?
#
# The defect this probes (2026-09-30). `qa-slot.sh` called `reap` exactly once, before the wait
# loop. A place is reclaimed only when its **holder** pid is gone, so a pass that took a place and
# was then killed leaves a place nobody owns — and a waiter that already ran `reap` before that
# death can never learn about it: it re-reads the same place file on every 15 s turn, counts it,
# and sits out the whole `QA_SLOT_WAIT` on a semaphore nobody is using. With `QA_SLOT_WAIT=3600`
# that is an hour of a writer's tick spent waiting for a lock that had been free since the crash.
#
# Reaping inside the loop makes the wait self-healing, and the one risk that introduces is
# **reaping a place whose holder is alive** — a bug that would let two passes run at once on a box
# whose RAM is the constraint this semaphore exists to protect. So the negative cases are the
# important ones, and there are two: a live holder, and a holder that is alive but whose place is
# brand new (below the grace period).
#
# Every case runs the real script as a subprocess against a private `QA_SLOT_DIR`. A re-typed
# reaper would go green here while the real one stayed broken, which is the false green this
# file exists to prevent.
set -uo pipefail

SLOT="${1:-scripts/qa/qa-slot.sh}"
# Two seconds is below the 15 s poll, so a case that must wait for a place proves the loop really
# turns rather than getting lucky. Cases that expect a place immediately are unaffected.
GRACE=0
WAIT=2
export QA_SLOTS=1 QA_SLOT_REAP_GRACE="$GRACE" QA_SLOT_WAIT="$WAIT"

pass=0
fail=0

# check <name> <expected-substring> <expected-place-count> <expected-holder-count> <seed-fn>
# The seed function receives the lock dir and holder dir and builds the initial state; the real
# script then runs as a subprocess. Place and holder counts are measured AFTER it exits, and the
# script's own holder child is killed either way so the probe leaks no `sleep` loops.
check() {
  local name="$1" want="$2" want_places="$3" want_holders="$4" seed="$5"
  local root dir holders out places holders_n ok=1
  root="$(mktemp -d)"
  dir="${root}/slot"
  holders="${dir}-holders"
  mkdir -p "$dir" "$holders"
  QA_SLOT_DIR="$dir" "$seed" "$dir" "$holders"
  out="$(QA_SLOT_DIR="$dir" bash "$SLOT" 2>&1)"
  places="$(find "$dir" -maxdepth 1 -type f | wc -l | tr -d ' ')"
  holders_n="$(find "$holders" -maxdepth 1 -type f | wc -l | tr -d ' ')"
  case "$out" in
    *"$want"*) ;;
    *) ok=0 ;;
  esac
  [ "$places" = "$want_places" ] || ok=0
  [ "$holders_n" = "$want_holders" ] || ok=0
  if [ "$ok" = 1 ]; then
    pass=$(( pass + 1 ))
    printf 'ok    %s\n' "$name"
  else
    fail=$(( fail + 1 ))
    printf 'FAIL  %s\n' "$name"
    printf '        wanted output containing %q, %s place(s), %s holder(s)\n' \
      "$want" "$want_places" "$want_holders"
    printf '        got    %q, %s place(s), %s holder(s)\n' "$out" "$places" "$holders_n"
  fi
  local h
  for h in "$holders"/*; do
    [ -e "$h" ] || continue
    kill "$(cat "$h" 2>/dev/null | awk '{print $NF}')" 2>/dev/null
  done
  rm -rf "$root"
}

# pid 4194303 is above /proc/sys/kernel/pid_max on this box, so it names a process that is
# definitively gone without racing whatever pid the kernel hands out next.
DEAD_PID=4194303

seed_empty() { :; }

seed_dead_holder() {
  : > "$1/$DEAD_PID-1"
  echo "$DEAD_PID" > "$2/$DEAD_PID-1"
}

# A live holder, seeded by a sleep that outlives the script. If this place is reaped, two passes
# run at once — the exact outcome the semaphore exists to prevent.
seed_live_holder() {
  sleep 120 </dev/null >/dev/null 2>&1 &
  echo $! > "$1/$$-holder"
  cp "$1/$$-holder" "$2/$$-holder"
}

# A live holder on a place that is younger than the grace period: the "the place is created a
# moment before the holder file" race the reaper's own comment describes. With GRACE=0 this case
# is indistinguishable from seed_live_holder, so it is run with a generous grace instead.
check "no place is taken immediately when the semaphore is empty" \
  "place taken" 1 1 seed_empty

check "a place whose holder is dead is reclaimed and the wait ends at once" \
  "place taken" 1 1 seed_dead_holder

check "a live holder is NOT reclaimed: the pass proceeds without a place and says so" \
  "no place after" 1 1 seed_live_holder

# Grace-period race: a live holder on a young place. The reaper must skip it (age <= grace) and
# the waiter must report it did not get a place rather than barging in.
GRACE=3600
check "a live holder on a young place is left alone by the age grace period" \
  "no place after" 1 1 seed_live_holder

# The regression that matters most, stated as its own case: the dead-holder plant is created
# AFTER the waiter has already begun waiting. Only a reaper inside the loop can notice this — a
# reaper that ran once before the loop has already made up its mind. seed_* all run *before* the
# script, so this one plants from a wrapper body around the script instead.
# The regression that matters most: a place whose holder is alive when the waiter arrives, and
# dead a second later. That is the ordinary way a place goes stale — a sibling pass is killed
# mid-tick by the box running out of RAM or disk — and only a reaper *inside* the loop can notice
# it. A reaper that ran once before the loop has already decided the place is busy and waits out
# the full window on a semaphore whose owner has been gone for an hour.
#
# So: seed a live holder, let the script enter its wait, then kill the holder. The script must
# reclaim the dead holder's place and take one for itself.
late_body() {
  local d="$1" h="$2" holder="$3" f
  f="$(mktemp)"
  cat > "$f" <<EOF
# The holder is already dead by the time the script's second poll runs, and its place file is
# still on disk naming it — which is exactly what a reaper has to recognise.
( sleep 1; kill "$holder" 2>/dev/null ) &
QA_SLOT_DIR="$d" bash "$SLOT" 2>&1
EOF
  echo "$f"
}
GRACE=0
check_late() {
  local name="$1" want="$2" want_places="$3" want_holders="$4"
  local root dir holders out places holders_n ok=1
  root="$(mktemp -d)"
  dir="${root}/slot"
  holders="${dir}-holders"
  mkdir -p "$dir" "$holders"
  # A live holder, so the script really does enter its wait loop before the death.
  sleep 120 </dev/null >/dev/null 2>&1 &
  local holder_pid=$!
  : > "$dir/$holder_pid-holder"
  echo "$holder_pid" > "$holders/$holder_pid-holder"
  out="$(bash "$(late_body "$dir" "$holders" "$holder_pid")" 2>&1)"
  kill "$holder_pid" 2>/dev/null
  places="$(find "$dir" -maxdepth 1 -type f | wc -l | tr -d ' ')"
  holders_n="$(find "$holders" -maxdepth 1 -type f | wc -l | tr -d ' ')"
  case "$out" in
    *"$want"*) ;;
    *) ok=0 ;;
  esac
  [ "$places" = "$want_places" ] || ok=0
  [ "$holders_n" = "$want_holders" ] || ok=0
  if [ "$ok" = 1 ]; then
    pass=$(( pass + 1 ))
    printf 'ok    %s\n' "$name"
  else
    fail=$(( fail + 1 ))
    printf 'FAIL  %s\n' "$name"
    printf '        wanted output containing %q, %s place(s), %s holder(s)\n' \
      "$want" "$want_places" "$want_holders"
    printf '        got    %q, %s place(s), %s holder(s)\n' "$out" "$places" "$holders_n"
  fi
  local h
  for h in "$holders"/*; do
    [ -e "$h" ] || continue
    kill "$(cat "$h" 2>/dev/null | awk '{print $NF}')" 2>/dev/null
  done
  rm -rf "$root"
}
# The late plant needs the loop to turn at least once after the death, so the wait window has to
# outlast both the 1 s plant and the 15 s poll: 20 s, not the 2 s the other cases use.
QA_SLOT_WAIT=20 check_late \
  "a live holder that dies WHILE the pass is waiting is reclaimed inside the wait loop" \
  "place taken" 1 1

echo
printf '%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
