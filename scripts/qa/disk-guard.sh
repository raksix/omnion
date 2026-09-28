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

dir_mb() { du -sm "$1" 2>/dev/null | cut -f1; }
free_gb() { df -BG --output=avail "$ROOT" 2>/dev/null | tail -1 | tr -dc '0-9'; }

say() { printf '%s\n' "$*"; }

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

# 4. last resort while the disk is still tight: the fattest one that is not main
while [ "$(free_gb)" -lt "$MIN_FREE_GB" ]; do
  victim=""; best=0
  for t in "$ROOT"/omnion*/target; do
    [ -d "$t" ] || continue
    w="$(dirname "$t")"
    [ "$w" = "$ROOT/omnion" ] && continue      # the deploy script runs this binary
    m=$(dir_mb "$t")
    [ "$m" -gt "$best" ] && { best=$m; victim="$t"; }
  done
  [ -n "$victim" ] || break
  say "disk at $(free_gb)G — dropping $(basename "$(dirname "$victim")") target (${best}M)"
  freed=$((freed + best)); rm -rf "$victim"
done

say "freed ~${freed}M — free now $(free_gb)G"
