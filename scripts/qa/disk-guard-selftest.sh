#!/usr/bin/env bash
# Disk guard — a target a build is writing into must survive, an idle one must not.
#
# Why this exists: `scripts/qa/disk-guard.sh` reclaims worktree `target/` directories by size, and
# it once deleted a `cargo test` that was four minutes from finishing. rustc reported three object
# files it could not write (`No such file or directory (os error 2)`) on a crate three levels
# from anything being edited, which reads like a broken dependency and is not one. The guard
# already had a liveness test; the ceiling step did not call it, and the liveness test itself
# derived the worktree from the target directory's *name*, which only works for the
# `/dev/shm/omnion-w2-target` convention — for a worktree's own `target/` it derived the name
# `omnion-target`, a directory that does not exist, and so reported "idle" during any ordinary
# build.
#
# The fixture is small and the ceiling is scaled, so this costs megabytes and about a second.
# The holder is a process whose cwd is the worktree and which names no `CARGO_TARGET_DIR` —
# the exact shape `in_use` cannot see, and the one that actually lost the build.
set -uo pipefail

GUARD="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/scripts/qa/disk-guard.sh"
ROOT="${TMPDIR:-/tmp}/omnion-disk-guard-test"
SHM="$ROOT-shm"
CEILING_MB="${CEILING_MB:-40}"

rm -rf "$ROOT" "$SHM"; mkdir -p "$ROOT" "$SHM"
fail=0

fixture() {
  rm -rf "$ROOT/omnion-fake"
  mkdir -p "$ROOT/omnion-fake/target/debug/incremental" "$ROOT/omnion-fake/target/debug/deps"
  # The worktree marker `worktree_busy` uses to tell a worktree from any directory that merely
  # contains a `target/`.
  echo "gitdir: $ROOT/fake-worktree" > "$ROOT/omnion-fake/.git"
  dd if=/dev/zero of="$ROOT/omnion-fake/target/debug/incremental/blob" bs=1M count=20 2>/dev/null
  dd if=/dev/zero of="$ROOT/omnion-fake/target/debug/deps/blob" bs=1M count=30 2>/dev/null
}

guard() {
  OMNION_ROOT="$ROOT" OMNION_SHM="$SHM" WORKTREE_TARGET_MAX_MB="$CEILING_MB" MIN_FREE_GB=0 \
    bash "$GUARD" 2>/dev/null
}

expect() { # expect <label> <actual> <wanted>
  if [ "$2" = "$3" ]; then printf 'PASS  %s (%s)\n' "$1" "$2"
  else printf 'FAIL  %s: got %s want %s\n' "$1" "$2" "$3"; fail=1; fi
}

alive() { [ -d "$ROOT/omnion-fake/target" ] && echo alive || echo gone; }

# 1. An idle target over the ceiling is reclaimed. The guard that cannot delete is not a guard.
fixture
out=$(guard)
expect "an idle target over the ceiling is dropped" "$(alive)" gone
expect "the drop is announced" "$(grep -c 'dropping: omnion-fake' <<<"$out")" 1

# 2. A build in the worktree keeps its target, its incremental cache and its output directory.
fixture
( cd "$ROOT/omnion-fake" && exec sleep 120 ) &
holder=$!
sleep 0.3
out=$(guard)
expect "a target a build is writing into is kept" "$(alive)" alive
expect "its incremental cache is kept too" \
  "$([ -d "$ROOT/omnion-fake/target/debug/incremental" ] && echo alive || echo gone)" alive
expect "the skip is announced, not silent" "$(grep -c 'is building in it' <<<"$out")" 1
if : > "$ROOT/omnion-fake/target/debug/deps/write-probe.o" 2>/dev/null; then
  printf 'PASS  the build output directory is still writable\n'
else
  printf 'FAIL  the build output directory was deleted underneath the build\n'; fail=1
fi
kill "$holder" 2>/dev/null; wait "$holder" 2>/dev/null

# 3. Once the build ends the ceiling applies again — otherwise "kept" would become a loophole.
sleep 0.3
guard > /dev/null
expect "an idle target is reclaimed again once the build ends" "$(alive)" gone

rm -rf "$ROOT" "$SHM"
if [ "$fail" -eq 0 ]; then echo "ALL PASS"; else echo "SOME FAILED"; fi
exit "$fail"
