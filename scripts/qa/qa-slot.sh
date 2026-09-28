#!/usr/bin/env bash
# Omnion QA — concurrency slot.
#
# The browser walkthrough is the heaviest step a loop performs. Several worktrees can
# want a pass at the same moment, and seven Chromium sessions on one box turn into a
# load average of 20 for no gain. This takes one of QA_SLOTS places (default 2), waits
# its turn, prints the pid of a background holder that keeps the place, and exits 0.
#
#   run.sh starts it in the background and kills the holder in its EXIT trap, so the
#   place is freed the moment the pass ends — or the loop is interrupted.
#
#   QA_SLOTS=2     how many passes may run at once (0 disables the wait entirely)
#   QA_SLOT_WAIT   seconds to wait for a place before giving up and proceeding anyway
set -euo pipefail

MAX="${QA_SLOTS:-2}"
LOCKDIR="${QA_SLOT_DIR:-/tmp/omnion-qa-slot}"
# Holder pids live outside LOCKDIR: a place is ONE file, and anything else in the
# directory would be counted as a second place and halve the real capacity.
HOLDERDIR="${LOCKDIR}-holders"
WAIT="${QA_SLOT_WAIT:-900}"

mkdir -p "$LOCKDIR" "$HOLDERDIR"
mine="$LOCKDIR/$$-$(date +%s)"

# The caller reads our stdout through a command substitution, which only ends when EVERY
# process holding the write end of that pipe has exited. The holder below is a child of this
# script, so without this it inherits the pipe, `$(… | tail -n 1)` never sees EOF, and the pass
# waits forever on a place it already took. Detaching the holder's standard streams is what
# makes the script's own exit the end of the pipe.
hold() { exec >/dev/null 2>&1 </dev/null; while :; do sleep 30; done; }

count_places() { find "$LOCKDIR" -maxdepth 1 -type f | wc -l; }

# A stale place from a killed pass would block the queue forever: reclaim one that is
# older than the maximum wait and whose owning process is gone.
reap() {
  local f pid age
  for f in "$LOCKDIR"/*; do
    [ -e "$f" ] || continue
    pid="$(basename "$f" | cut -d- -f1)"
    age=$(( $(date +%s) - $(stat -c %Y "$f" 2>/dev/null || echo 0) ))
    [ "$age" -gt $(( WAIT + 900 )) ] || continue
    if ! kill -0 "$pid" 2>/dev/null; then
      rm -f "$f" "${HOLDERDIR}/${f##*/}" 2>/dev/null || true
      echo "[qa-slot] reclaimed a stale place from pid $pid (${age}s old)" >&2
    fi
  done
}
reap

deadline=$(( $(date +%s) + WAIT ))
while :; do
  count="$(count_places)"
  if [ "$count" -lt "$MAX" ]; then
    : > "$mine"
    # The holder must not inherit this script's stdout: `run.sh` reads the pid with
    # `… | tail -n 1`, and a background child holding the same pipe open means `tail` never
    # sees EOF — the pass prints "place taken" and then hangs there forever, with a slot held
    # and no walkthrough running. Redirecting the holder's stdio to /dev/null is what makes the
    # pipeline finish.
    while :; do sleep 30; done </dev/null >/dev/null 2>&1 &
    holder=$!
    echo "$holder" > "${HOLDERDIR}/${mine##*/}"
    echo "$holder"                                # stdout: the holder pid for run.sh
    echo "[qa-slot] place taken ($(( count + 1 ))/$MAX)" >&2
    exit 0
  fi
  if [ "$(date +%s)" -ge "$deadline" ]; then
    echo "[qa-slot] no place after ${WAIT}s, proceeding without one" >&2
    exit 0
  fi
  sleep 15
done
