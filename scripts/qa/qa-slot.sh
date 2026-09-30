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
    pid="$(basename "$f")"
    # The holder file must yield ONE pid. `run.sh` reads it with `tail -n 1` on the captured
    # stdout, which is correct; a file holding two pids on one line then fails `kill -0`,
    # so the place is judged ownerless and reclaimed *while its owner is still running* — and
    # it is also never reclaimable by its own owner, which is how a queue deadlocks. Taking
    # the LAST whitespace-separated field makes the test answer the only question it asks
    # (is that process alive?) whatever the file happens to contain.
    holder="$(awk '{print $NF}' "${HOLDERDIR}/${pid}" 2>/dev/null || true)"
    [ -n "${holder:-}" ] || holder="$(cat "${HOLDERDIR}/${pid}" 2>/dev/null || echo '')"
    holder="$(printf '%s' "${holder}" | awk '{print $NF}')"
    age=$(( $(date +%s) - $(stat -c %Y "$f" 2>/dev/null || echo 0) ))
    [ "$age" -gt "$grace" ] || continue
    # No holder file at all, this long after the place appeared, means the pass died between
    # taking the place and writing the holder down.
    if [ -z "$holder" ] || ! kill -0 "$holder" 2>/dev/null; then
      rm -f "$f" "${HOLDERDIR}/${pid}" 2>/dev/null || true
      echo "[qa-slot] reclaimed a stale place from ${pid} (${age}s old, holder ${holder:-none})" >&2
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
  # **The reaper has to run in the loop, not only on entry.** It is called once above, which
  # reclaims a place whose holder was already gone when *this* script started — and misses the
  # case that actually costs a pass its whole tick: the holder dies while this script is already
  # waiting. A pass that crashed two minutes ago keeps its place until the next pass starts, and
  # on a nine-writer box that is every other waiting pass.
  reap
  if [ "$(date +%s)" -ge "$deadline" ]; then
    echo "[qa-slot] no place after ${WAIT}s, proceeding without one" >&2
    exit 0
  fi
  sleep 15
done
