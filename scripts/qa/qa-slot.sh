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
set -euo pipefail

MAX="${QA_SLOTS:-1}"
LOCKDIR="${QA_SLOT_DIR:-/tmp/omnion-qa-slot}"
# Holder pids live outside LOCKDIR: a place is ONE file, and anything else in the
# directory would be counted as a second place and halve the real capacity.
HOLDERDIR="${LOCKDIR}-holders"
WAIT="${QA_SLOT_WAIT:-1800}"

mkdir -p "$LOCKDIR" "$HOLDERDIR"
mine="$LOCKDIR/$$-$(date +%s)"

count_places() { find "$LOCKDIR" -maxdepth 1 -type f | wc -l; }

# Reclaim a place whose holder is gone.
#
# The liveness test has to read the **holder** pid, and the reason is not a nicety: the place
# file is named after `$$` — the pid of *this* script — and this script exits the moment it takes
# the place. So the pid in the place file is dead within milliseconds of a perfectly healthy
# pass, and a reaper that tested it would either reclaim every live place or, having learned
# nothing, fall back on age alone. That is what it did: `age > WAIT + 900`, which is 75 minutes
# on this box, so one crashed pass held the whole queue hostage for over an hour while every
# later pass printed "waiting for a QA slot" and died at its own timeout with no report.
#
# The holder is the `while :; do sleep 30; done` child, whose pid is written beside the place and
# killed by run.sh's EXIT trap — so it lives exactly as long as the pass that owns the place.
# The short grace period covers the one race that remains: the place is created a moment before
# the holder file, and a reaper running in that window must not decide the place is unowned.
reap() {
  local f pid holder age grace
  grace="${QA_SLOT_REAP_GRACE:-120}"
  for f in "$LOCKDIR"/*; do
    [ -e "$f" ] || continue
    # The place file is named `$$-<timestamp>`. The whole name is the holder file's key.
    #
    # What must NOT be tested is the pid in that name. `$$` here is *this script's* pid, and this
    # script exits the moment it takes the place — so the owner recorded in the file is dead within
    # milliseconds of a perfectly healthy pass. A reaper that tested it would delete the lock of a
    # pass that is running, every writer on the box would start a Chromium pass at once, and the
    # "one pass at a time" would be a fiction nobody was told about. run.sh is the process that
    # actually owns the place, and it records its own pid beside the holder's; that pair is the
    # only liveness signal here that means anything.
    name="$(basename "$f")"
    # The holder file is TWO lines (holder child, then owner), so `cat` would hand `kill -0` a
    # two-line string, which never matches a pid and reports *every* place as dead. Reading each
    # line separately is the difference between a reaper that reclaims exactly the dead places
    # and one that clears the whole queue.
    holder="$(sed -n 1p "${HOLDERDIR}/${name}" 2>/dev/null || echo '')"
    # The owner's pid is the second line, written by run.sh — the process that actually holds the
    # place.
    owner="$(sed -n 2p "${HOLDERDIR}/${name}" 2>/dev/null || echo '')"
    age=$(( $(date +%s) - $(stat -c %Y "$f" 2>/dev/null || echo 0) ))
    [ "$age" -gt "$grace" ] || continue
    # Two tests, and the owner is the one that matters. A holder is an ordinary orphan the moment
    # its pass ends badly: run.sh releases the place on EXIT INT TERM, and none of those arrive on
    # a SIGKILL, an OOM kill, or a box that simply loses the process. The holder then answers
    # `kill -0` for ever, so a reaper that tests only the holder is blind to precisely the case it
    # was written for — which is what happened: one writer's crashed pass held four others for
    # their whole 30-minute timeout, each printing "waiting for a QA slot" and reporting nothing.
    #
    # A missing owner line is NOT grounds for a reclaim on its own: it is what a place looks like
    # in the second between being created and run.sh writing the file down. The grace period
    # covers that race, and nothing else.
    if [ -n "$owner" ] && ! kill -0 "$owner" 2>/dev/null; then
      rm -f "$f" "${HOLDERDIR}/${name}" 2>/dev/null || true
      [ -n "$holder" ] && kill "$holder" 2>/dev/null
      echo "[qa-slot] reclaimed a place whose pass is gone: ${name} (${age}s old, owner ${owner}, holder ${holder:-none})" >&2
      continue
    fi
    # No holder file at all, this long after the place appeared, means the pass died between
    # taking the place and writing the holder down.
    if [ -z "$holder" ] || ! kill -0 "$holder" 2>/dev/null; then
      rm -f "$f" "${HOLDERDIR}/${name}" 2>/dev/null || true
      echo "[qa-slot] reclaimed a stale place from ${name} (${age}s old, holder ${holder:-none})" >&2
    fi
  done
}
# Reap, *then* wait — and reaping again on every turn of the wait loop, not only before it.
#
# Running `reap` once before the loop is right when every pass is healthy, and exactly wrong when
# one dies: the place it left behind is already older than the grace period by the time the next
# waiter looks, but nobody re-checks, so the queue waits out its whole timeout for a pass that is
# not coming. The symptom is a line that says "waiting for a QA slot" forever and then a pass
# that either proceeds unslotted or dies at its own timeout with no report — which is precisely
# what happened here, on a box where one writer's crashed pass held four other writers hostage.
#
# Reaping per turn costs one `stat` and one `kill -0` per waiting pass every 15 seconds, which is
# nothing next to a Chromium session, and the grace period already protects the only race there
# is (a place created moments before its holder file is written).
reap

deadline=$(( $(date +%s) + WAIT ))
while :; do
  reap
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
    # Two lines: the holder child, then the OWNER — the run.sh that is really holding this place.
    # The owner is what a reaper must test, and it cannot be recovered from the place file: `$$`
    # there is *this* script's pid, which exits immediately. Without this line a crashed pass
    # (SIGKILL, OOM kill) leaves a holder that answers `kill -0` for ever, and every writer behind
    # it waits out the full timeout behind a pass that is not coming.
    printf '%s\n%s\n' "$holder" "${QA_SLOT_OWNER:-$$}" > "${HOLDERDIR}/${mine##*/}"
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
