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
# The pid of the PASS that is asking for a place, i.e. the process whose EXIT trap will kill the
# holder. Optional, because this script is also run by its own test with no pass behind it.
#
# Without it a holder is immortal. The holder is a `while :; do sleep 30; done` child of THIS
# script, and this script exits within milliseconds of taking the place — so the holder's own
# parent is already gone by design, and it has no way to learn that the pass that owns the place
# died with it. `run.sh` is the only process that can kill the holder, and a pass that is SIGKILLed
# (or whose whole process group is taken down) never runs its trap. The holder then keeps
# sleeping, the reaper reads a live pid and dutifully leaves the place alone, and the queue is
# held hostage until someone walks up and kills a stranger's holder by hand. That is not
# hypothetical: 38 such waiters had accumulated box-wide, 14 of them in this worktree, every one
# of them a pass that no longer existed waiting to take a place it could never give back.
OWNER="${QA_SLOT_OWNER_PID:-}"
# Holder pids live outside LOCKDIR: a place is ONE file, and anything else in the
# directory would be counted as a second place and halve the real capacity.
HOLDERDIR="${LOCKDIR}-holders"
WAIT="${QA_SLOT_WAIT:-1800}"

mkdir -p "$LOCKDIR" "$HOLDERDIR"
mine="$LOCKDIR/$$-$(date +%s)"

# The caller reads our stdout through a command substitution, which only ends when EVERY
# process holding the write end of that pipe has exited. The holder below is a child of this
# script, so without this it inherits the pipe, `$(… | tail -n 1)` never sees EOF, and the pass
# waits forever on a place it already took. Detaching the holder's standard streams is what
# makes the script's own exit the end of the pipe.
hold() { exec >/dev/null 2>&1 </dev/null; while :; do sleep 30; done; }
holder_alive() { kill -0 "$1" 2>/dev/null; }

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
  local f pid holder age grace owner
  grace="${QA_SLOT_REAP_GRACE:-120}"
  for f in "$LOCKDIR"/*; do
    [ -e "$f" ] || continue
    pid="$(basename "$f")"
    # The holder file must yield ONE pid: the last field of the LAST line.
    #
    # `awk '{print $NF}'` alone is not that. It prints the final field of *every* line, so a
    # holder file of "owner 123\n456" yields "123\n456" — a value `kill -0` cannot parse. The
    # place is then judged holderless and reclaimed *while its owner is still running*, and its
    # own owner can never reclaim it either, which is how a queue deadlocks. The owner line
    # below is what makes this file two lines, so the reader has to cope with it: last line
    # first, last field of that line. The same is true of `run.sh`'s `tail -n 1` on stdout,
    # which is why the holder pid is always written last.
    holder="$(tail -n 1 "${HOLDERDIR}/${pid}" 2>/dev/null | awk '{print $NF}')"
    age=$(( $(date +%s) - $(stat -c %Y "$f" 2>/dev/null || echo 0) ))
    [ "$age" -gt "$grace" ] || continue
    # The PASS that owns this place, written beside the holder when the place was taken. A
    # place whose owner is gone is the state every interrupted pass leaves behind, and judging
    # it by the holder alone is what made these immortal: the holder outlives the pass that
    # kills it. Recorded only when the caller passed one, so a place taken by this script's own
    # test (no pass behind it) is judged on the holder exactly as before.
    owner="$(awk '$1 == "owner" {print $2; exit}' "${HOLDERDIR}/${pid}" 2>/dev/null || true)"
    if [ -n "${owner:-}" ] && ! holder_alive "$owner"; then
      rm -f "$f" "${HOLDERDIR}/${pid}" 2>/dev/null || true
      kill "$holder" 2>/dev/null || true
      echo "[qa-slot] reclaimed a place whose pass (${owner}) is gone (${age}s old, holder ${holder:-none})" >&2
      continue
    fi
    # No holder file at all, this long after the place appeared, means the pass died between
    # taking the place and writing the holder down.
    if [ -z "$holder" ] || ! holder_alive "$holder"; then
      rm -f "$f" "${HOLDERDIR}/${pid}" 2>/dev/null || true
      echo "[qa-slot] reclaimed a stale place from ${pid} (${age}s old, holder ${holder:-none})" >&2
    fi
  done
}
deadline=$(( $(date +%s) + WAIT ))
while :; do
  # Reap on EVERY turn, not once before the loop. The reaper is the only thing that can free a
  # place whose owner died, and the owner is most likely to die *while this loop is running* --
  # that is exactly when a queued pass is waiting. Reaping once before the loop left every pass
  # that was already queued when a sibling crashed to sit on a dead place until its own
  # QA_SLOT_WAIT expired, which is 1800s here and 3600s in the writer loops: the whole queue
  # stalled behind one corpse, and the passes it stalled then timed out and proceeded anyway.
  # The grace period inside reap() covers the place/holder creation race, so re-reading it every
  # 15s cannot reclaim a place that was taken a moment ago.
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
    # The holder pid LAST, so the `awk '{print $NF}'` reader above (and `run.sh`'s
    # `tail -n 1`) still answers with the holder whatever else is in the file. The owner line
    # is what makes the place reclaimable when the pass is killed without running its trap.
    { [ -n "$OWNER" ] && echo "owner $OWNER"; echo "$holder"; } > "${HOLDERDIR}/${mine##*/}"
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
