#!/usr/bin/env bash
# Guard for this worktree's cargo target directory.
#
# THE PROBLEM. The Omnion writers share one box. A sibling that runs out of disk deletes
# `target/` to reclaim it — and `rm -rf target` on someone else's worktree is invisible
# from here until cargo fails with:
#
#   error: could not write output to .../target/debug/deps/....rcgu.o: No such file or directory
#   error: couldn't create a temp dir: No such file or directory (os error 2)
#
# which reads exactly like a code fault and costs a full rebuild every time. It has cost
# this worktree two ticks (34 and 35). `os error 2` on an `.o`/`.rmeta` path is THIS,
# not a broken crate: `os error 28` is the real "disk full".
#
# WHAT THIS DOES. A build that started with a target directory that no longer exists gets a
# clear diagnosis instead of a bare error, and — more usefully — the rebuild is kicked off
# immediately rather than being discovered by the next unrelated command.
set -uo pipefail

WORKTREE="${WORKTREE:-/mnt/apopic/omnion-w4}"
TARGET="$WORKTREE/target"
LOAD_1M="$(cut -d' ' -f1 /proc/loadavg)"
DISK_PCT="$(df -P /mnt/apopic | awk 'NR==2 {gsub(/%/,"",$5); print $5}')"
DISK_FREE="$(df -Ph /mnt/apopic | awk 'NR==2 {print $4}')"

echo "worktree=$WORKTREE load_1m=$LOAD_1M disk=$DISK_PCT% free=$DISK_FREE"

# A browser pass on a saturated box measures the machine, not the product. Screenshots
# time out at 15s and every locator after them fails, which the pass then reports as
# dozens of unrelated UI defects. A tick that starts a pass under this load produces
# findings that are all false, and the honest result is an unticked box.
if [ "${LOAD_1M%%.*}" -gt 60 ]; then
  echo "BLOCKED: load_1m=$LOAD_1M is above 60 — a browser pass here measures the box, not the product."
  echo "Run the Rust walks and typecheck instead; they are unaffected by load."
fi

if [ "${DISK_PCT%%.*}" -ge 95 ]; then
  echo "BLOCKED: /mnt/apopic is at $DISK_PCT% — the next build that starts may lose its target again."
  echo "Free your own worktree's artifacts (apps/*/.next/cache, .tmp-target) before building."
fi

if [ ! -d "$TARGET" ]; then
  echo "MISSING: $TARGET does not exist. A sibling almost certainly deleted it to reclaim disk."
  echo "This is os error 2, not a code fault. Rebuilding now so the cost is not paid twice."
  ( cd "$WORKTREE" && PATH="$HOME/.cargo/bin:$PATH" cargo build -p omnion-api --quiet ) \
    >/tmp/w4-target-rebuild.log 2>&1 &
  echo "rebuild started (pid $!), log /tmp/w4-target-rebuild.log"
  exit 0
fi

echo "target: present ($(du -sh "$TARGET" 2>/dev/null | cut -f1))"
exit 0
