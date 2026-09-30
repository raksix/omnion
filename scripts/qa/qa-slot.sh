#!/usr/bin/env bash
# Omnion QA — concurrency slot.
#
# The browser walkthrough is the heaviest step a loop performs. Several worktrees can
# want a pass at the same moment, and seven Chromium sessions on one box turn into a
# load average of 20 for no gain. This takes one of QA_SLOTS places (default 1), waits
# its turn, prints the pid of a background holder that keeps the place, and exits 0.
#
#   run.sh starts it in the background and kills the holder in its EXIT trap, so the
#   place is freed the moment the pass ends — or the loop is interrupted.
#
#   QA_SLOTS=1     how many passes may run at once (0 disables the wait entirely)
#   QA_SLOT_WAIT   seconds to wait for a place before giving up and proceeding anyway
#
#   QA_SLOT_OWNER  pid of the pass asking for a place. REQUIRED, and the only liveness
#                  signal the reaper can use — see the note on the holder file below.
set -euo pipefail

MAX="${QA_SLOTS:-1}"
LOCKDIR="${QA_SLOT_DIR:-/tmp/omnion-qa-slot}"
# Holder pids live outside LOCKDIR: a place is ONE file, and anything else in the
# directory would be counted as a second place and halve the real capacity.
HOLDERDIR="${LOCKDIR}-holders"
WAIT="${QA_SLOT_WAIT:-1800}"
OWNER="${QA_SLOT_OWNER:-}"

mkdir -p "$LOCKDIR" "$HOLDERDIR"
mine="$LOCKDIR/$$-$(date +%s)"

count_places() { find "$LOCKDIR" -maxdepth 1 -type f | wc -l; }
owner_alive() { [ -n "$OWNER" ] && kill -0 "$OWNER" 2>/dev/null; }

# Reap first, then decide. These used to be the other way round, and the order is load-bearing
# in both directions.
#
# The reaper is a QUEUE hygiene step: it repairs places left behind by passes that are gone.
# A pass asking for a place is exactly the party that has to do it, because it is the only
# thing that is looking at the queue at all — a waiter inside `while :; sleep 15` sees the
# places but never cleans them. So the reaper must not be gated behind a liveness check on the
# asker: a dead-owner probe exits at `owner_alive` and the abandoned places stay for ever,
# which is the exact symptom this file exists to clear.
#
# Conversely the liveness check must not run before the reaper claims a place for the caller,
# or a killed waiter holds a place nothing can release. Hence: reap unconditionally, then test
# the owner, then take. Reading it as "check then reap" looks tidier and is a queue that never
# moves.
reap() {
  local f pid holder age grace owner_pid
  grace="${QA_SLOT_REAP_GRACE:-120}"
  for f in "$LOCKDIR"/*; do
    [ -e "$f" ] || continue
    pid="$(basename "$f")"
    # "<holder> <owner>" — the holder so run.sh can kill it, the owner so the next pass
    # can tell a real pass from a corpse. An entry with no owner predates this and is
    # treated as unknown rather than as live.
    holder="$(cut -d' ' -f1 "${HOLDERDIR}/${pid}" 2>/dev/null || echo '')"
    owner_pid="$(cut -d' ' -f2 "${HOLDERDIR}/${pid}" 2>/dev/null || echo '')"
    age=$(( $(date +%s) - $(stat -c %Y "$f" 2>/dev/null || echo 0) ))
    [ "$age" -gt "$grace" ] || continue
    # No holder file at all, this long after the place appeared, means the pass died between
    # taking the place and writing the holder down.
    if [ -z "$holder" ] || ! kill -0 "$holder" 2>/dev/null; then
      rm -f "$f" "${HOLDERDIR}/${pid}" 2>/dev/null || true
      echo "[qa-slot] reclaimed a stale place from ${pid} (${age}s old, holder ${holder:-none})" >&2
      continue
    fi
    # A live holder proves only that nobody ran the trap. The owner's liveness is the
    # question: while it is alive the trap will fire; once it is gone it cannot.
    if [ -n "$owner_pid" ] && ! kill -0 "$owner_pid" 2>/dev/null; then
      kill "$holder" 2>/dev/null || true
      rm -f "$f" "${HOLDERDIR}/${pid}" 2>/dev/null || true
      echo "[qa-slot] reclaimed an ABANDONED place from ${pid} (${age}s old, holder ${holder} alive but its pass ${owner_pid} is gone)" >&2
    fi
  done
}
reap

# A waiter that outlives its pass must not take a place. It has no run.sh, so the EXIT trap
# that would release what it takes can never run — the place would outlive every future
# pass by exactly as long as the orphan lives, and the orphan would be the queue. This is
# the same reasoning as the reaper's, applied before the fact instead of after it.
if ! owner_alive; then
  echo "[qa-slot] pass ${OWNER:-unknown} is not running; taking no place, because nothing could ever release it" >&2
  exit 0
fi

deadline=$(( $(date +%s) + WAIT ))
while :; do
  count="$(count_places)"
  if [ "$count" -lt "$MAX" ]; then
    if ! owner_alive; then
      echo "[qa-slot] this waiter was orphaned; giving up rather than holding a place nobody can release" >&2
      exit 0
    fi
    : > "$mine"
    # The holder must not inherit this script's stdout: `run.sh` reads the pid with
    # `… | tail -n 1`, and a background child holding the same pipe open means `tail` never
    # sees EOF — the pass prints "place taken" and then hangs there forever, with a slot held
    # and no walkthrough running. Redirecting the holder's stdio to /dev/null is what makes the
    # pipeline finish.
    while :; do sleep 30; done </dev/null >/dev/null 2>&1 &
    holder=$!
    echo "$holder $OWNER" > "${HOLDERDIR}/${mine##*/}"
    echo "$holder"                                # stdout: the holder pid for run.sh
    echo "[qa-slot] place taken ($(( count + 1 ))/$MAX) for pass ${OWNER}" >&2
    exit 0
  fi
  if [ "$(date +%s)" -ge "$deadline" ]; then
    echo "[qa-slot] no place after ${WAIT}s, proceeding without one" >&2
    exit 0
  fi
  sleep 15
  # Reap **again** here, not only before the first check.
  #
  # The one-shot reap at the top of this script is correct for a pass that starts into a clean
  # queue, and useless for a pass that waits. A waiting pass is blocked on a place whose owner
  # was killed — a timeout, a `timeout 1400` that fired, a writer that was interrupted — and
  # killing the pass does not run `run.sh`'s EXIT trap, so the place and its holder file stay
  # behind. Nothing else in the loop can clear it either: the reaper only runs when a *new*
  # pass starts, and the only pass here is the one already waiting. The symptom is a queue that
  # never moves, one "waiting for a QA slot" line, and a pass that dies at its own timeout
  # having measured nothing.
  #
  # The check is cheap — a handful of `kill -0` calls per 15 s — and it is the difference between
  # a queue that drains and one that needs a human to delete a file out of /tmp.
  reap
done
