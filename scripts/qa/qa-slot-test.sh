#!/usr/bin/env bash
# Proves the qa-slot.sh guards against a scratch slot directory.
#
# Every guard here was written from a real observation on the box (six orphaned places from
# three worktrees over one night), so each case drives the real script and a real process
# tree — a re-implementation would only prove the re-implementation.
#
# The signal under test is QA_SLOT_OWNER, the pid the pass volunteers, because the holder
# cannot answer the question: it is a background job of qa-slot.sh and is reparented the
# moment that script exits, which is the successful case, not a symptom. Each case therefore
# builds a REAL (owner, holder) pair and then does the one thing that matters — kills the
# owner, the way a `timeout` that fired or a SIGKILL does, without running any trap.
set -uo pipefail

SLOT="$(mktemp -d)"
export QA_SLOT_DIR="$SLOT"
export QA_SLOTS=1
export QA_SLOT_WAIT=0            # every invocation is here to test the reaper, not to wait
export QA_SLOT_REAP_GRACE=0
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/qa-slot.sh"
HOLDERS="$SLOT-holders"
mkdir -p "$HOLDERS"

PIDS=""
cleanup() {
  # SIGKILL, not kill: the pass stands in for a walkthrough and its own child chain
  # (`sleep 5`, the holder) does not forward SIGTERM reliably, so a polite kill leaves the
  # suite waiting on a trap that never returns. Nothing here is a production process — every
  # pid is one this file created in a scratch directory.
  for p in $PIDS; do kill -9 "$p" 2>/dev/null || true; done
  rm -rf "$SLOT" "$HOLDERS"
}
trap cleanup EXIT

fail=0
check() { local ok="$1" name="$2"; echo "$ok  $name"; [ "$ok" = "yes" ] || fail=$((fail + 1)); }
total=0

# A probe that runs qa-slot.sh has to be a DEAD pass, or it is a live one.
#
# qa-slot.sh is a script about taking a place, so invoking it to observe the reaper is also
# an invocation that takes one: with a live QA_SLOT_OWNER it finds a free slot, writes its
# place and holder, and exits — leaving a place this test then has to reason about, and a
# holder that outlives the test. The first run of this file did exactly that and reported
# five failures, every one of them the probe's own place rather than the reaper's answer:
# a real defect and an identical artefact of the instrument. The probes below therefore
# volunteer a pid that is certain to be gone, which makes them a dead pass — the only kind
# that is entitled to be here — and never a waiter. `999998` is the same pid case 2 leaves
# behind; the reaper's job in cases 3, 4 and 6 is to clean up after that, and a probe that
# is itself alive would mask the very thing it is measuring.
DEAD="999998"
probe() { QA_SLOT_OWNER="$DEAD" bash "$SRC" "$@"; }

# Start a real pass: it takes a place, and the place records (holder, owner).
#
# The pass must be a REAL long-lived process, and the first version of this fixture got that
# wrong in a way that made five cases report the reaper stealing a live pass's place. The
# fixture built its pass as a one-shot `bash -c … &`, which exits the moment qa-slot.sh hands
# it a place — so the "live" owner was dead within milliseconds and every later reaper
# reclaimed it, correctly. That is the very defect qa-slot.sh's own header documents (a place
# named after a pid that is dead within milliseconds), reproduced by the instrument: a
# fixture that walks into the bug cannot test the guard against it.
#
# So the pass below is what run.sh actually is — a process that stays alive for the length of
# a walkthrough — and it is killed, never exited, when a case needs it gone.
#
# $BASHPID, not $$, because the pass runs in a subshell: bash defines $$ as the *parent*
# shell's pid in a subshell, so `QA_SLOT_OWNER=$$` here would have recorded this test's own
# pid and every case would have been measuring the test rather than the pass. A pid that names
# the wrong process is the same defect as no pid at all, and quieter.
start_pass() { # -> "place holder owner"
  local out i place="" holder="" entry
  out="$(mktemp)"
  # The subshell's OWN stdout is redirected away from the command substitution's pipe. It
  # inherits that pipe, and a background process holding a pipe open means `read … <<< "$(…)"`
  # waits for an EOF that never arrives: the suite hangs, and the `timeout` that kills it
  # leaves the place and holder behind in the LIVE /tmp queue, which is the orphan this file
  # exists to prevent. qa-slot.sh's own output is captured separately into $out, so nothing is
  # lost by closing the inherited pipe.
  (
    QA_SLOT_DIR="$SLOT" QA_SLOTS=1 QA_SLOT_WAIT=0 QA_SLOT_OWNER="$BASHPID" \
      bash "$SRC" >"$out" 2>/dev/null || true
    # Stand in for the walkthrough: the pass is alive until a case kills it.
    while :; do sleep 5; done
  ) </dev/null >/dev/null 2>&1 &
  local owner=$!
  for i in $(seq 1 40); do
    entry="$(find "$HOLDERS" -maxdepth 1 -type f | head -1)"
    [ -n "$entry" ] && break
    sleep 0.1
  done
  if [ -n "$entry" ]; then
    place="$(basename "$entry")"
    holder="$(cut -d' ' -f1 "$entry")"
  fi
  PIDS="$PIDS $owner $holder"
  rm -f "$out"
  echo "$place $holder $owner"
}

place_of() { # age a place past any grace period
  local n="case-$$-$RANDOM"
  : > "$SLOT/$n"
  touch -d '-600 seconds' "$SLOT/$n"
  echo "$n"
}

# ------------------------------------------------- 1. a pass that is still running keeps its place
read -r p h o <<< "$(start_pass)"
if [ -n "$p" ]; then
  PIDS="$PIDS $o"
  probe >/dev/null 2>&1 || true
  kept=no; [ -f "$SLOT/$p" ] && kept=yes
  check "$kept" "a live pass keeps its place (the reaper never steals a running pass's slot)"
  check "$([ -f "$HOLDERS/$p" ] && echo yes || echo no)" "and its holder file survives with it"
  kill "$o" "$h" 2>/dev/null
  rm -f "$SLOT/$p" "$HOLDERS/$p"
else
  check no "a live pass keeps its place (the reaper never steals a running pass's slot)"
  check no "and its holder file survives with it"
fi

# ------------------------------------------------- 2. an owner killed without a trap releases the place
# This is the defect the guard exists for. The holder is deliberately left ALIVE, so a reaper
# that only asks "is the holder alive?" answers no and lets the place stand — which is
# exactly the six-orphans-per-night failure. The place must go, and the holder with it.
read -r p h o <<< "$(start_pass)"
if [ -n "$p" ] && kill -0 "$h" 2>/dev/null; then
  kill -9 "$o" 2>/dev/null
  # Age the place past the grace period instead of sleeping through it.
  #
  # `age` is whole seconds from mtime and the reaper skips anything not older than the grace
  # (120 s in production, 0 here). A place killed 0.2 s ago therefore has `age == 0`, and
  # `[ 0 -gt 0 ]` is false: the reaper correctly declines to touch a place that has only just
  # appeared, because a pass takes one and writes its holder a moment later. That is the right
  # behaviour and it is why this case failed for a long time with no defect anywhere.
  #
  # Ageing the file is the faithful version of the situation, not a way round it: a real
  # orphan is minutes or hours old, because it comes from a pass that was SIGKILLed and the
  # next writer arrived afterwards. A sub-second-old orphan is not the thing being modelled.
  touch -d '-600 seconds' "$SLOT/$p"
  msg="$(probe 2>&1 >/dev/null || true)"
  gone=no; [ ! -f "$SLOT/$p" ] && gone=yes
  check "$gone" "a place whose pass was SIGKILLed is released even though its holder is alive"
  # This assertion was inverted and had been failing since the case was written: it asked
  # whether the holder file EXISTS, so it reported the reaper leaving a file behind as a
  # success. A check whose name and whose condition disagree is worse than no check — it
  # closes the one question the case exists to ask, and it did so with a red line that read
  # as a product bug rather than as the test being wrong.
  #
  # The file does have to go, and the reason is not tidiness: `HOLDERDIR` is where run.sh
  # looks up a place's holder to kill it, so a holder file with no place is a leftover that
  # nothing will ever consult — and the reaper only walks places, never holder files, so it
  # would sit there for ever.
  check "$([ ! -f "$HOLDERS/$p" ] && echo yes || echo no)" \
    "its holder file goes with it (a file without a place is a second phantom slot)"
  holder_gone=no; kill -0 "$h" 2>/dev/null || holder_gone=yes
  check "$holder_gone" "the orphaned holder is killed, not left holding a place nobody owns"
  case "$msg" in *"is gone"*) saw=yes ;; *) saw=no ;; esac
  check "$saw" "the reaper names the real cause (owner gone) rather than a dead-pid excuse"
  kill "$h" 2>/dev/null
else
  check no "a place whose pass was SIGKILLed is released even though its holder is alive"
  check no "its holder file goes with it (a file without a place is a second phantom slot)"
  check no "the orphaned holder is killed, not left holding a place nobody owns"
  check no "the reaper names the real cause (owner gone) rather than a dead-pid excuse"
fi

# ------------------------------------------------- 3. a dead holder is still reclaimed (no regression)
n="$(place_of)"; echo "999999 $DEAD" > "$HOLDERS/$n"
probe >/dev/null 2>&1 || true
dead_gone=no; [ ! -f "$SLOT/$n" ] && dead_gone=yes
check "$dead_gone" "a place with a dead holder pid is still reclaimed"

# ------------------------------------------------- 4. a place with no holder file is reclaimed
n="$(place_of)"
probe >/dev/null 2>&1 || true
noholder=no; [ ! -f "$SLOT/$n" ] && noholder=yes
check "$noholder" "a place whose pass died before writing its holder is reclaimed"

# ------------------------------------------------- 4b. the probe itself leaves no place behind
# The defect this file was written for, measured on the instrument rather than on the script:
# the first version ran its probes as live passes, so each one took a place on the way out and
# the suite failed on its own litter. Counting the directory is the only assertion that can
# see it — every individual case above can pass while the suite as a whole leaves a place
# behind, which is precisely how six orphans accumulated from three worktrees over a night.
leftovers="$(find "$SLOT" -maxdepth 1 -type f | wc -l)"
check "$([ "$leftovers" -eq 0 ] && echo yes || echo no)" \
  "a reaper probe leaves no place of its own behind (found $leftovers, want 0)"

# ------------------------------------------------- 5. a waiter whose pass is gone takes NO place
# The half that is easy to miss. A waiter that outlives its run.sh has no trap that could
# ever release what it takes, so a place handed to it would outlive every future pass by
# exactly as long as the orphan lives — the orphan would become the queue. A FREE slot is
# therefore not sufficient reason to take one.
out="$(QA_SLOT_DIR="$SLOT" QA_SLOTS=1 QA_SLOT_WAIT=0 QA_SLOT_OWNER="$DEAD" bash "$SRC" 2>&1 >/dev/null || true)"
rc_walk=$?
# `check` compares against the literal string "yes", so a count handed to it raw fails for
# the wrong reason and reads as a verdict. Translate it, or the suite teaches the reader that
# `check 0` is a pass.
left="$(find "$SLOT" -maxdepth 1 -type f | wc -l | tr -d ' ')"
check "$([ "$left" -eq 0 ] && echo yes || echo no)" \
  "a waiter whose pass is gone takes no place even when a place is free"
case "$out" in *"is not running"*) gave=yes ;; *) gave=no ;; esac
check "$gave" "and it says why, so a pass that wanted a slot knows it did not get one"
rc="$(QA_SLOT_DIR="$SLOT" QA_SLOTS=1 QA_SLOT_WAIT=0 QA_SLOT_OWNER="$DEAD" bash "$SRC" >/dev/null 2>&1; echo $?)"
check "$([ "$rc" -eq 0 ] && echo yes || echo no)" \
  "giving up exits 0 — run.sh must not read 'no slot' as a failed pass"

# ------------------------------------------------- 6. the reaper is idempotent and the dir survives
n="$(place_of)"; echo "999999 $DEAD" > "$HOLDERS/$n"
probe >/dev/null 2>&1 || true
probe >/dev/null 2>&1 || true
again=no; [ ! -f "$SLOT/$n" ] && again=yes
check "$again" "reaping the same stale place twice is safe (a second pass sees an empty queue)"

echo
if [ "$fail" -eq 0 ]; then echo "OK: all cases passed"; else echo "FAILED: $fail"; fi
exit "$fail"
