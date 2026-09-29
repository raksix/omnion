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
# ## Why the holder carries the worktree's identity
#
# The holder used to be an anonymous `while :; do sleep 30; done` whose pid was all
# that tied it to a place, and that turned out to be two bugs at once once several
# writers ran passes side by side.
#
# First, `run.sh` reads the holder pid out of a pipeline with `| tail -n 1`, so a holder
# whose stdout was not fully redirected keeps the pipe open and the pass hangs with a
# place held and no walkthrough running — the redirect below is what makes the pipeline
# finish.
#
# Second, and worse: the place file is named after the *acquiring* script's pid, while the
# holder pid was recorded beside it in a file named after the place. Two writers that
# acquired in either order would then leave the holder file describing one pass and the
# place file naming the other, so a reaper asked "is this place's holder alive?" answered
# about somebody else's `sleep` and kept a place that no pass was using — a queue that
# never drains. The holder now records its own worktree in `$0`/cwd, and the place file
# records the holder, so both answers are about the same process.
set -euo pipefail

MAX="${QA_SLOTS:-1}"
LOCKDIR="${QA_SLOT_DIR:-/tmp/omnion-qa-slot}"
# Holder pids live outside LOCKDIR: a place is ONE file, and anything else in the
# directory would be counted as a second place and halve the real capacity.
HOLDERDIR="${LOCKDIR}-holders"
WAIT="${QA_SLOT_WAIT:-1800}"

mkdir -p "$LOCKDIR" "$HOLDERDIR"
# The token is minted once, from this process and the machine it is running on, and the SAME
# token names the place, the holder file and the holder's own command line. That is what makes
# the three provably about one pass: a reaper that reads the holder pid out of the holder file
# is holding a token that a different writer — or a recycled pid — cannot have produced, and
# `holder_is_ours` can refuse a pid that belongs to somebody else's sleep loop.
TOKEN="$$-$(date +%s)-$(printf '%s' "$PWD" | cksum | cut -d' ' -f1)"
mine="$LOCKDIR/$TOKEN"

count_places() { find "$LOCKDIR" -maxdepth 1 -type f | wc -l; }

# Is this pid genuinely the holder belonging to PLACE TOKEN?
#
# Without this check the queue cannot drain. The place is named after the acquiring pid and the
# holder pid is written beside it, so a place whose holder file was written by another writer —
# a pid collision after a recycle, or a place left behind by a pass that was killed between
# taking the place and writing the holder — reads as "alive" for as long as that unrelated
# process lives. Observed on this box: a place named after wave6's waiter recorded a holder
# whose working directory was wave4's, so every later pass queued behind a place no pass was
# using and each died at its own timeout with no report.
#
# The test is against the PLACE's token, not this script's: the reaper is a different process
# from the holder it is judging, so comparing against its own token would reject every live
# place. The token is minted by the acquiring process and carried in the holder's argv, so a
# holder that answers with this place's token is the one this place created.
holder_is_ours() {
  local pid="${1:-}" token="${2:-}"
  [ -n "$pid" ] && [ -n "$token" ] || return 1
  kill -0 "$pid" 2>/dev/null || return 1
  tr '\0' '\n' < "/proc/$pid/cmdline" 2>/dev/null | grep -qx -- "qa-slot-holder $token"
}

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
# The short grace period covers the one race that remains: the place is created a moment before
# the holder file, and a reaper running in that window must not decide the place is unowned.
reap() {
  local f token holder age grace
  grace="${QA_SLOT_REAP_GRACE:-120}"
  for f in "$LOCKDIR"/*; do
    [ -e "$f" ] || continue
    token="$(basename "$f")"
    holder="$(cat "${HOLDERDIR}/${token}" 2>/dev/null || echo '')"
    age=$(( $(date +%s) - $(stat -c %Y "$f" 2>/dev/null || echo 0) ))
    [ "$age" -gt "$grace" ] || continue
    # No holder file at all, this long after the place appeared, means the pass died between
    # taking the place and writing the holder down.
    if ! holder_is_ours "$holder" "$token"; then
      rm -f "$f" "${HOLDERDIR}/${token}" 2>/dev/null || true
      echo "[qa-slot] reclaimed a stale place ${token} (${age}s old, holder ${holder:-none})" >&2
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
    #
    # The token rides in argv so `holder_is_ours` can recognise this pid as ours and nobody
    # else's, which is what stops one writer's place from being kept alive by another's process.
    # `exec sleep` keeps it a single process, so the pid run.sh kills in its EXIT trap is the
    # pid this script just reported.
    exec -a "qa-slot-holder $TOKEN" sleep "${QA_SLOT_HOLDER_TTL:-86400}" </dev/null >/dev/null 2>&1 &
    holder=$!
    echo "$holder" > "${HOLDERDIR}/${TOKEN}"
    echo "$holder"                                # stdout: the holder pid for run.sh
    echo "[qa-slot] place taken ($(( count + 1 ))/$MAX) as ${TOKEN}" >&2
    exit 0
  fi
  if [ "$(date +%s)" -ge "$deadline" ]; then
    echo "[qa-slot] no place after ${WAIT}s, proceeding without one" >&2
    exit 0
  fi
  sleep 15
done
