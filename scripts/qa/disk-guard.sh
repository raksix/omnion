#!/usr/bin/env bash
# Omnion — bound what a single worktree may hold on disk.
#
# Seven writers, six cores, one disk. Each worktree's target/ grows without limit
# until the disk is full, and a full disk is not a slow day: the agent cannot even
# start ("cron external worker exited before ownership acknowledgement"). That happened
# twice, so the ceiling is enforced here rather than left to chance.
#
#   WORKTREE_TARGET_MAX_MB  how large one worktree's target/ may get (default 6000)
#   MIN_FREE_GB            below this, drop the fattest target/ (default 10)
#
# A cold target/ is not a loss: cargo rebuilds the workspace in about ninety seconds.
# What matters is never being the reason the loop cannot run.
set -uo pipefail

ROOT="${OMNION_ROOT:-/mnt/apopic}"
MAX_MB="${WORKTREE_TARGET_MAX_MB:-6000}"
MIN_FREE_GB="${MIN_FREE_GB:-10}"
# The second cliff, and the one that actually stopped a QA pass.
#
# The worktrees on this box do not all build into `target/`: several point CARGO_TARGET_DIR at
# `/dev/shm` so a compile does not fill the disk. That moves the same unbounded growth onto a
# 32 GB tmpfs, and a full tmpfs is not a slow day either — it is a *shared* one, because every
# worktree's build cache lives in the same filesystem and a build that cannot write its output
# dies with a "No space left on device" that reads like a source error. It showed up as
# "the QA slot is held by a sibling writer" for three consecutive REQs, which named the wrong
# culprit: the slot was the symptom, the full tmpfs was the cause.
SHM="${OMNION_SHM:-/dev/shm}"
SHM_MIN_FREE_PCT="${OMNION_SHM_MIN_FREE_PCT:-15}"

dir_mb() { du -sm "$1" 2>/dev/null | cut -f1; }
free_gb() { df -BG --output=avail "$ROOT" 2>/dev/null | tail -1 | tr -dc '0-9'; }
shm_free_pct() { df --output=pcent "$SHM" 2>/dev/null | tail -1 | tr -dc '0-9'; }

say() { printf '%s\n' "$*"; }

# Is any live process building into this directory?
#
# A build cache is disposable; a *running build* is not, and deleting one produces exactly the
# failure this script exists to prevent. The test reads `CARGO_TARGET_DIR` out of each
# process's own environment rather than guessing from a name, because a sibling's target is
# named after its stack and a stack that renamed is a stack whose build would be deleted.
# Unreadable `/proc/N/environ` (the process exited between the glob and the read) is not a
# "held" answer — it is a process that no longer exists.
in_use() {
  local dir="$1" p
  for p in /proc/[0-9]*; do
    tr '\0' '\n' < "$p/environ" 2>/dev/null | grep -qqx "CARGO_TARGET_DIR=$dir" && return 0
  done
  return 1
}

# Is a QA pass running against the worktree that owns this tmpfs target?
#
# `in_use` alone was not enough, and this function exists because of the day it was not. A
# stack's build runs in its own wrapper process (`qa-pass.sh` → `cargo build`), which **exits**
# once the binary is staged; the pass that then depends on that target — `run.sh`, the pm2 API
# process, the walkthrough — carries no `CARGO_TARGET_DIR` at all, because it never set one.
# So the environment test answered "not held" and deleted a 7 GB target out from under a pass
# that was a minute from using it. The honest test is not "is somebody building right now" but
# "is anybody at all still working in that worktree", and a live process with its cwd in the
# worktree is the cheapest honest answer.
worktree_busy() {
  local dir="$1" token wt p
  token="$(basename "$dir")"; token="${token%-target}"; token="${token#omnion-}"
  for wt in "$ROOT"/omnion-$token "$ROOT"/omnion; do
    [ -d "$wt" ] || continue
    for p in /proc/[0-9]*; do
      [ "$(readlink "$p/cwd" 2>/dev/null)" = "$wt" ] && return 0
    done
  done
  return 1
}

# The full test: a target is reclaimable only when nothing is building into it AND no pass is
# still living in the worktree that owns it.
reclaimable() {
  in_use "$1" && return 1
  worktree_busy "$1" && return 1
  return 0
}

# 1. incremental compilation cache is pure speed — always safe, do it first
freed=0
for inc in "$ROOT"/omnion*/target/debug/incremental; do
  [ -d "$inc" ] || continue
  m=$(dir_mb "$inc")
  if [ "$m" -gt 300 ]; then
    say "drop incremental ${m}M: $(dirname "$(dirname "$inc")")"
    freed=$((freed + m)); rm -rf "$inc"
  fi
done

# 2. keep the newest QA artifacts only (each pass writes ~90 MB of screenshots)
for art in "$ROOT"/omnion*/qa-artifacts; do
  [ -d "$art" ] || continue
  while read -r old; do
    [ -n "$old" ] || continue
    m=$(dir_mb "$old")
    say "drop old qa-artifacts ${m}M: $(basename "$old")"
    freed=$((freed + m)); rm -rf "$old"
  done < <(find "$art" -maxdepth 1 -mindepth 1 -type d 2>/dev/null | sort | head -n -2)
done

# 3. per-worktree ceiling: never let one target/ grow past the cap
for t in "$ROOT"/omnion*/target; do
  [ -d "$t" ] || continue
  m=$(dir_mb "$t")
  if [ "$m" -gt "$MAX_MB" ]; then
    w="$(dirname "$t")"
    say "target ${m}M over the ${MAX_MB}M ceiling — dropping: $(basename "$w")"
    freed=$((freed + m)); rm -rf "$t"
  fi
done

# 3b. the tmpfs cliff: an orphaned CARGO_TARGET_DIR in /dev/shm, reclaimable at any time.
#
# This is the step that was missing, and it is the one that mattered. An orphaned cache is not
# a cache anybody is still filling: the test is whether a *live* process names this directory,
# and a directory nobody names is pure dead weight on a filesystem every sibling shares. It
# runs unconditionally rather than only under pressure, because the pressure signal comes too
# late — by the time tmpfs is 99% full, the build that needed the space has already died.
for t in "$SHM"/*-target "$SHM"/qa-*; do
  [ -d "$t" ] || continue
  reclaimable "$t" || continue
  m=$(dir_mb "$t")
  [ "${m:-0}" -gt 200 ] || continue
  say "reclaim orphaned $(basename "$t") (${m}M, no live build or pass) from $SHM"
  freed=$((freed + m)); rm -rf "$t"
done

# 3c. only if tmpfs is still tight: the fattest orphan that is not the main worktree's
if [ "$(shm_free_pct)" -lt "$SHM_MIN_FREE_PCT" ]; then
  while [ "$(shm_free_pct)" -lt "$SHM_MIN_FREE_PCT" ]; do
    victim=""; best=0
    for t in "$SHM"/*-target; do
      [ -d "$t" ] || continue
      [ "$(basename "$t")" = "omnion-main-target" ] && continue
      reclaimable "$t" || continue
      m=$(dir_mb "$t")
      [ "${m:-0}" -gt "$best" ] && { best=$m; victim="$t"; }
    done
    [ -n "$victim" ] || break
    say "$SHM at $(shm_free_pct)% free — dropping $(basename "$victim") (${best}M)"
    freed=$((freed + best)); rm -rf "$victim"
  done
fi

# 4. last resort while the disk is still tight: the fattest one that is not main
while [ "$(free_gb)" -lt "$MIN_FREE_GB" ]; do
  victim=""; best=0
  for t in "$ROOT"/omnion*/target; do
    [ -d "$t" ] || continue
    w="$(dirname "$t")"
    [ "$w" = "$ROOT/omnion" ] && continue      # the deploy script runs this binary
    # Same rule as the tmpfs sweep: a target a live build is writing into is not a victim, no
    # matter how full the disk is. Step 4 is the step that used to skip this, and a disk at 100%
    # is exactly when a wrong `rm -rf` looks like a reasonable idea.
    in_use "$t" && continue
    m=$(dir_mb "$t")
    [ "${m:-0}" -gt "$best" ] && { best=$m; victim="$t"; }
  done
  [ -n "$victim" ] || break
  say "disk at $(free_gb)G — dropping $(basename "$(dirname "$victim")") target (${best}M)"
  freed=$((freed + best)); rm -rf "$victim"
done

# Both cliffs, named. A guard that prints one number leaves the reader guessing which
# filesystem it just rescued, and "disk is fine" is what a tmpfs-full box keeps saying.
say "freed ~${freed}M — $ROOT free $(free_gb)G · $SHM free $(shm_free_pct)%"
