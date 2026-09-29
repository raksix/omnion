#!/usr/bin/env bash
# Omnion — a global semaphore around cargo builds.
#
# WHY THIS EXISTS
# Eight writer loops share six cores. Each QA pass runs `cargo build -p omnion-api`,
# and a single cargo invocation takes all six cores on its own. Eight at once is 48
# runnable threads on six cores: the compiles crawl, the box sits at load 30+, and the
# memory pressure starts swapping. Every writer still compiles with
# CARGO_BUILD_JOBS=3, so this ceiling is deliberately generous — the point is to keep
# three or four builds *running* instead of eight all thrashing at once.
#
# HOW IT WORKS
# A slot is a file under a lock directory, held for the duration of the command. The
# rest queue. A slot is released even when the builder dies mid-build, because each
# holder file records its pid and the waiter reaps holders whose process is gone — a
# killed compile must not wedge the queue for everyone.
#
# USAGE
#   scripts/qa/cargo-slot.sh cargo test -p omnion-api
#   CARGO_SLOTS=3 scripts/qa/cargo-slot.sh cargo build --workspace
#
# Env:
#   CARGO_SLOTS        how many builds may compile at once (default 2)
#   CARGO_SLOT_WAIT    seconds to wait for a slot before giving up (default 1800)
#   CARGO_SLOT_DIR     where holder files live (default /tmp/omnion-cargo-slots)
set -uo pipefail

SLOTS="${CARGO_SLOTS:-2}"
WAIT="${CARGO_SLOT_WAIT:-1800}"
DIR="${CARGO_SLOT_DIR:-/tmp/omnion-cargo-slots}"

if [ "$#" -eq 0 ]; then
  echo "usage: cargo-slot.sh <command> [args...]" >&2
  exit 64
fi

mkdir -p "$DIR"
start=$(date +%s)
self=$$

release() { rm -f "$DIR/$self" 2>/dev/null; }
trap release EXIT INT TERM

# Drop slots whose owner is gone.
reap() {
  local f pid
  for f in "$DIR"/*; do
    [ -e "$f" ] || continue
    pid=$(cat "$f" 2>/dev/null)
    if [ -z "$pid" ]; then
      rm -f "$f"
    elif ! kill -0 "$pid" 2>/dev/null; then
      rm -f "$f"
    fi
  done
}

acquire() {
  local waited=0 busy n
  while :; do
    reap
    busy=$(find "$DIR" -maxdepth 1 -type f 2>/dev/null | wc -l)
    if [ "$busy" -lt "$SLOTS" ]; then
      # Claim under a lock directory so two writers cannot both see the same free slot.
      if mkdir "$DIR.lock" 2>/dev/null; then
        n=$(find "$DIR" -maxdepth 1 -type f 2>/dev/null | wc -l)
        if [ "$n" -lt "$SLOTS" ]; then
          echo "$self" > "$DIR/$self"
          rmdir "$DIR.lock" 2>/dev/null
          BUSY_BEFORE="$busy"
          return 0
        fi
        busy=$n
        rmdir "$DIR.lock" 2>/dev/null
      fi
    fi
    if [ "$waited" -ge "$WAIT" ]; then
      echo "cargo-slot: gave up after ${waited}s waiting for a slot" >&2
      return 1
    fi
    if [ $((waited % 60)) -eq 0 ]; then
      echo "cargo-slot: queued ${waited}s ($busy/$SLOTS busy)" >&2
    fi
    sleep 5
    waited=$((waited + 5))
  done
}

BUSY_BEFORE=0
acquire || exit 75   # EX_TEMPFAIL: the caller's retry policy should treat this as busy
echo "cargo-slot: slot taken after $(( $(date +%s) - start ))s ($BUSY_BEFORE/$SLOTS were busy)" >&2

"$@"
rc=$?
release
trap - EXIT INT TERM
exit $rc
