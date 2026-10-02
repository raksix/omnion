#!/usr/bin/env bash
# Omnion — bound what a single worktree may hold on disk.
#
# Seven writers, six cores, one disk. Each worktree's target/ grows without limit
# until the disk is full, and a full disk is not a slow day: the agent cannot even
# start ("cron external worker exited before ownership acknowledgement"). That happened
# twice, so the ceiling is enforced here rather than left to chance.
#
#   WORKTREE_TARGET_MAX_MB  how large one worktree's target/ may get (default 6000)
#   NEXT_MAX_MB           how large one .next cache may get before it is dropped (default 1200)
#   MIN_FREE_GB            below this, drop the fattest target/ (default 10)
#
# A cold target/ is not a loss: cargo rebuilds the workspace in about ninety seconds.
# What matters is never being the reason the loop cannot run.
set -uo pipefail

ROOT="${OMNION_ROOT:-/mnt/apopic}"
MAX_MB="${WORKTREE_TARGET_MAX_MB:-6000}"
NEXT_MAX_MB="${NEXT_MAX_MB:-1200}"
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

# Is a live process WORKING IN this directory (or anything under it)?
#
# `worktree_busy` above asks a narrower question and, for a Next cache, asks it of the wrong
# thing. It compares `/proc/N/cwd` to a worktree root with `=`, and a live QA admin server's
# cwd is not the worktree root — pm2 starts it in the app it serves, so it is
# `<worktree>/apps/admin`. The equality never held, `worktree_busy` answered "nobody", and the
# guard deleted the `.next` of a server that was serving the walkthrough at the time.
#
# That is not a rare shape: it is every pass. `run.sh` starts `omnion-qa-admin-<stack>` in the
# app directory, and the dev server grows its cache well past `NEXT_MAX_MB` while the pass is
# still walking — the cache is large BECAUSE it is in use.
#
# So the test is a PREFIX test over cwd, not an equality, and it is the honest question anyway:
# "is anybody still working here" is what every reclaim decision in this script needs, and an
# exact match answers a question nobody asked — a process whose cwd is a sibling app in the
# same worktree is still a reason to leave the worktree alone.
#
# `/proc/N/cwd` on a process we cannot read answers empty, and empty is not a prefix of any
# worktree, so an unreadable entry cannot manufacture a false "busy" — it just does not count.
#
# The test accepts the directory ITSELF as well as anything under it, and the first draft did
# not: `"$wt"/*` matches only children, so asking about the one directory a process is actually
# sitting in — `<worktree>/apps/admin`, where the dev server's cwd lives — answered "idle"
# while the caller was describing a directory that was in use. The guard was written, proven
# against the wrong argument, and green. Equality is not a special case here: "working in this
# directory" and "working in something inside it" are the same answer to a question about
# whether to delete something.
worktree_under() {
  local wt="$1" p cwd
  for p in /proc/[0-9]*; do
    cwd="$(readlink "$p/cwd" 2>/dev/null)"
    [ -n "$cwd" ] || continue
    case "$cwd" in
      "$wt"|"$wt"/*) return 0 ;;
    esac
  done
  return 1
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

# 2. Next.js build cache. A QA pass starts a Turbopack dev server, and its cache is the
# single biggest thing on this disk: ten worktrees held 13 GB of it. The dev server
# rebuilds what it needs, so a stale cache is pure waste.
#
# — and a LIVE cache is not a stale one.
#
# This step had no liveness test while every other reclaim step had one, and it is the only
# step that deletes a directory a RUNNING PROCESS IS USING. Observed on 2026-10-01, on the
# wave-3 stack: at 03:54 the guard removed `omnion-w3/apps/admin/.next`, the dev server logged
# `The directory at ".../.next/dev" was deleted. Restarting the server to recover...`, and the
# walkthrough continued into a server that was restarting. It did not crash the pass, so the
# summary read "55 pages, no findings on the builder" and three ticks of notes were written
# about the box being tired. What actually happened is in the admin error log with a timestamp
# on it, and every screen after `/analytics/downloads` came back `chrome-error://chromewebdata/`
# — the harness recorded those as pages with no problems, because a page that never loaded has
# no problems.
#
# The asymmetry is the point: `target/` is disposable SPEED, so losing it costs a rebuild. A
# dev server's cache is its live working set, and deleting it does not slow the server down, it
# stops it. So this step is the one that had to ask first, and steps 3b/4/5 already knew how.
for nx in "$ROOT"/omnion*/apps/*/.next; do
  [ -d "$nx" ] || continue
  m=$(dir_mb "$nx")
  if [ "$m" -gt "$NEXT_MAX_MB" ]; then
    # The APP that owns this cache — `<root>/omnion[-<stack>]/apps/<app>`. This is the directory
    # to ask about: a dev server's cwd is the app, and asking about the worktree ROOT answers the
    # broader question ("is anybody working in this worktree") — true for a sibling app, which is
    # a reason to be careful, but it names the wrong directory in the log line below.
    app="$(dirname "$nx")"
    wt="$(dirname "$(dirname "$app")")"
    label="$(basename "$wt")/$(basename "$app")"
    if worktree_under "$app"; then
      say "keep next cache ${m}M: ${label} — a live process is working in it"
      continue
    fi
    say "drop next cache ${m}M: ${label}"
    freed=$((freed + m)); rm -rf "$nx"
  fi
done

# 3. keep the newest QA artifacts only (each pass writes ~90 MB of screenshots)
for art in "$ROOT"/omnion*/qa-artifacts; do
  [ -d "$art" ] || continue
  while read -r old; do
    [ -n "$old" ] || continue
    m=$(dir_mb "$old")
    say "drop old qa-artifacts ${m}M: $(basename "$old")"
    freed=$((freed + m)); rm -rf "$old"
  done < <(find "$art" -maxdepth 1 -mindepth 1 -type d 2>/dev/null | sort | head -n -2)
done

# 4. per-worktree ceiling: never let one target/ grow past the cap
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

# 5. last resort while the disk is still tight: the fattest one that is not main
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
