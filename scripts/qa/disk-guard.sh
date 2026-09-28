#!/usr/bin/env bash
# Omnion — keep the worktrees from filling the disk.
#
# Seven writers each own a Rust target/ and a Next.js .next/, and a full disk stops the
# agent from even starting ("cron external worker exited before ownership
# acknowledgement"). Two cheap rules keep it bounded without touching a live build:
#
#   1. incremental compilation caches are pure build speed, never correctness — drop the
#      ones older than a day.
#   2. QA artifact directories (screenshots per pass) — keep the two newest per worktree.
#   3. If the disk is still tight, drop target/ in the worktrees that are furthest from
#      their next build. Cargo rebuilds in about 90 seconds; the next tick pays it.
#
# Usage: disk-guard.sh [--check]   (--check only reports, never deletes)
set -uo pipefail

ROOT="${OMNION_ROOT:-/mnt/apopic}"
LOOP_W="${LOOP_W:-40}"
MIN_FREE_GB="${MIN_FREE_GB:-8}"
MODE="${1:-run}"

say() { printf '%s\n' "$*"; }
free_gb() { df -BG --output=avail "$ROOT" 2>/dev/null | tail -1 | tr -dc '0-9'; }
dir_mb() { du -sm "$1" 2>/dev/null | cut -f1; }

if [ "$MODE" = "--check" ]; then
  say "free: $(free_gb)G (need ${MIN_FREE_GB}G)"
  for d in "$ROOT"/omnion "$ROOT"/omnion-w*; do
    [ -d "$d/target" ] || continue
    say "$(basename "$d"): target $(dir_mb "$d/target")M"
  done
  exit 0
fi

freed=0

# 1. incremental caches older than a day
while read -r inc; do
  [ -n "$inc" ] || continue
  m=$(dir_mb "$inc"); freed=$((freed + m))
  say "rm incremental $(du -sm --dereference "$inc" 2>/dev/null | cut -f1)M: $inc"
  rm -rf "$inc"
done < <(find "$ROOT"/omnion*/target/debug/incremental -maxdepth 0 -type d -mtime +1 2>/dev/null)

# 2. old QA artifact folders
while read -r art; do
  [ -n "$art" ] || continue
  m=$(dir_mb "$art"); freed=$((freed + m))
  say "rm qa-artifacts $(du -sh "$art" 2>/dev/null | cut -f1): $art"
  rm -rf "$art"
done < <(find "$ROOT"/omnion*/qa-artifacts -maxdepth 1 -mindepth 1 -type d 2>/dev/null | sort | head -n -${LOOP_W})

# 3. last resort: a cold target/ in the worktree with the most to lose
while [ "$(free_gb)" -lt "$MIN_FREE_GB" ]; do
  victim=""
  best=0
  for d in "$ROOT"/omnion "$ROOT"/omnion-w*; do
    [ -d "$d/target" ] || continue
    m=$(dir_mb "$d/target")
    # never pick the main checkout: its binary is what the deploy script runs
    [ "$d" = "$ROOT/omnion" ] && continue
    if [ "$m" -gt "$best" ]; then best=$m; victim="$d"; fi
  done
  [ -n "$victim" ] || break
  say "disk tight ($(free_gb)G) — dropping $(du -sh "$victim/target" | cut -f1) target: $victim"
  freed=$((freed + best))
  rm -rf "$victim/target"
done

say "freed ~${freed}M — free now $(free_gb)G"
