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

count_places() { find "$LOCKDIR" -maxdepth 1 -type f | wc -l; }

# A stale place from a killed pass would block the queue forever: reclaim one whose
# owning process is gone. The age guard is only a grace period for a place that is
# *still* being created — a dead owner is reaped immediately, because waiting
# WAIT+900 to release a place nobody is using just stalls every later pass.
reap() {
  local f pid age
  for f in "$LOCKDIR"/*; do
    [ -e "$f" ] || continue
    pid="$(basename "$f" | cut -d- -f1)"
    age=$(( $(date +%s) - $(stat -c %Y "$f" 2>/dev/null || echo 0) ))
    if kill -0 "$pid" 2>/dev/null; then
      # Live owner: only reclaim a place older than the maximum wait.
      [ "$age" -gt $(( WAIT + 900 )) ] || continue
      rm -f "$f" "${HOLDERDIR}/${f##*/}" 2>/dev/null || true
      echo "[qa-slot] reclaimed an expired place from pid $pid (${age}s old)" >&2
      continue
    fi
    rm -f "$f" "${HOLDERDIR}/${f##*/}" 2>/dev/null || true
    echo "[qa-slot] reclaimed a stale place from pid $pid (${age}s old)" >&2
  done
}
reap

deadline=$(( $(date +%s) + WAIT ))
while :; do
  count="$(count_places)"
  if [ "$count" -lt "$MAX" ]; then
    : > "$mine"
    # The holder must NOT inherit stdout: run.sh reads this script through a
    # `$(… | tail -n 1)` command substitution, and a background child that keeps
    # the pipe open makes the substitution wait for an EOF that never arrives —
    # the pass then hangs forever instead of running. Close fd 1 for the holder.
    ( while :; do sleep 30; done ) >/dev/null 2>&1 &   # keeps the place while this caller lives
    echo $! > "${HOLDERDIR}/${mine##*/}"
    echo "$!"                                 # stdout: the holder pid for run.sh
    echo "[qa-slot] place taken ($(( count + 1 ))/$MAX)" >&2
    exit 0
  fi
  if [ "$(date +%s)" -ge "$deadline" ]; then
    echo "[qa-slot] no place after ${WAIT}s, proceeding without one" >&2
    exit 0
  fi
  sleep 15
done
