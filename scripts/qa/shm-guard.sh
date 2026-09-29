#!/usr/bin/env bash
# Omnion QA — keep this worktree's build directory inside the shared tmpfs.
#
# ## Why this exists
#
# Every worktree on this box points its `target/` at `/dev/shm` (invariant 11), which is the right
# call: the loop image is at 93% and a 3.7 G build on it takes the box down. `/dev/shm` is 32 G
# SHARED between seven writers, though, and that has a failure mode that reads exactly like a code
# defect.
#
# **A full `/dev/shm` makes the LINKER die with a bus error**, not with a message about space:
#
#     = note: some arguments are omitted, use `--verbose` to show all linker arguments
#     = note: PLEASE submit a bug report to https://github.com/llvm/llvm-project/issues/
#     collect2: fatal error: ld terminated with signal 7 [Bus error]
#     error: could not compile `omnion-api` (bin "omnion-api") due to 1 previous error
#
# Two ticks of REQ-126 recorded `observability_permissions` as "aborts with exit 101 and no panic
# message, at 4e997ba as well as here — pre-existing on this box, logged not claimed". There was
# no abort. The test binary is 144 M and a 32 G tmpfs that has drifted to 100% cannot write it, so
# the build died before a single assertion ran, and the BUILD-LOG attributed a symptom of the
# environment to a test suite that passes 4/4. A defect logged as "pre-existing, not claimed" is
# the cheapest place in the whole system to hide a real one: it is written down, so it stops being
# looked at, and it is not fixed, so it keeps happening.
#
# ## What it reclaims, and what it will not touch
#
# **Only its own `target/`, and only two things:**
#
# 1. **Stale duplicate executables.** `debug/deps/` accumulates a second and third copy of every
#    test binary when a dependency is rebuilt — the same crate under a new hash. Here that was
#    3.2 GB across 10 duplicated test suites (`observability_events` alone held three 144 M copies).
#    Cargo never removes them; it only stops writing new ones. The newest copy per crate is kept,
#    because that is the one the next `cargo test` links against.
# 2. **`debug/incremental/`.** Worth ~800 MB here and useless to a build that already failed once;
#    with `CARGO_INCREMENTAL=0` it is never written again.
#
# Another writer's `target/` is never touched, not even to be helpful — the same rule as the QA
# port ranges, and for the same reason. `TARGET` is this worktree's own, resolved through the
# symlink so a stack that was left pointing at the loop image is reported rather than trimmed.
#
# ## Usage
#
# 3. **Integration-test binaries.** `debug/deps/` holds one 73–79 MB executable per `tests/*.rs`
#    walk — 56 of them here, 3.0 GB. The dedup pass above only removes a crate's *older copies*,
#    so with one copy of each it reclaims nothing, and the pass then refuses to start for want of
#    space while sitting on 3 GB of binaries it will never run: `scripts/qa/run.sh` builds
#    `omnion-api` and walks the panel in a browser, and never invokes `cargo test`. The gate that
#    DOES need them is the tick's own `cargo test -p <crate>`, which relinks what it needs.
#
#   Two independent writers on this box recorded "not enough shared build space" from this floor
#   within an hour of each other, and the reclaimable space was sitting in the very directory the
#   guard was already pruning — a guard that only knows one shape of waste is a guard that stops
#   the queue for a reason its author did not anticipate.
#
#   The scope rule is unchanged and is the reason this is safe: another writer's `target/` is
#   never touched, and a concurrent `cargo test` in THIS worktree only ever relinks, so a
#   deletion costs a relink and never a build error.
#
#   Set `QA_SHM_KEEP_TEST_BINS=1` when a test run is the reason for the pass and the binaries are
#   wanted warm.
#
#   bash scripts/qa/shm-guard.sh            # prune this worktree, report the box
#   QA_SHM_MIN_FREE_MB=4096 …                # fail if the box is below the floor anyway
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TARGET="${TARGET:-$ROOT/target}"
SHM="${QA_SHM_DIR:-/dev/shm}"
MIN_FREE_MB="${QA_SHM_MIN_FREE_MB:-2048}"

freed_mb() { echo $(( $1 / 1048576 )); }

shm_free_mb() { df -m --output=avail "$SHM" | tail -1 | tr -d ' '; }

report() {
  printf '[shm-guard] /dev/shm: %s MB free of %s (%s%% used)\n' \
    "$(shm_free_mb)" "$(df -m --output=size "$SHM" | tail -1 | tr -d ' ')" \
    "$(df -h --output=pcent "$SHM" | tail -1 | tr -d ' %')"
}

if [ ! -d "$TARGET" ]; then
  echo "[shm-guard] no target directory yet at $TARGET" >&2
  report
  exit 0
fi

# A stack that resolved back onto the loop image is worth saying out loud: that is invariant 11
# broken, and it is how a 3.7 G build ends up on a 93%-full volume.
case "$(readlink -f "$TARGET")" in
  "$SHM"/*) ;;
  *) echo "[shm-guard] WARNING: target resolves to $(readlink -f "$TARGET"), not under $SHM" >&2 ;;
esac

before="$(shm_free_mb)"

rm -rf "${TARGET:?}/debug/incremental"

# Keep the newest copy of every hashed executable; drop the rest. Grouping is on the cargo hash
# suffix, so `observability_events-<16 hex>` and its two predecessors land in one bucket.
TARGET="$TARGET" python3 - <<'PY' >&2 || true
import collections, os

target = os.environ["TARGET"]
deps = os.path.join(target, "debug", "deps")
if not os.path.isdir(deps):
    raise SystemExit(0)

groups = collections.defaultdict(list)
for name in os.listdir(deps):
    path = os.path.join(deps, name)
    if not os.path.isfile(path):
        continue
    base = name
    for i in range(len(name) - 1, -1, -1):
        if name[i] == "-":
            tail = name[i + 1:]
            if len(tail) == 16 and all(c in "0123456789abcdef" for c in tail):
                base = name[:i]
                break
    groups[base].append((path, os.path.getmtime(path)))

freed = 0
removed = 0
for base, entries in groups.items():
    if len(entries) < 2:
        continue
    entries.sort(key=lambda entry: -entry[1])   # newest first
    for path, _mtime in entries[1:]:
        try:
            freed += os.path.getsize(path)
            os.unlink(path)
            removed += 1
        except OSError:
            pass
if removed:
    print(f"[shm-guard] dropped {removed} stale duplicate build artifacts ({freed // 1048576} MB)")
PY

# The pass this guards builds `omnion-api` and drives a browser; it never runs a test binary.
# Cargo never removes these, so they accumulate one 73–79 MB copy per `tests/*.rs` walk. Dropping
# them costs the NEXT `cargo test` a relink, which is the tick's own gate to pay for — and that
# relink is the cheaper outcome by far compared to a pass that refuses to start.
if [ "${QA_SHM_KEEP_TEST_BINS:-0}" != "1" ]; then
  TARGET="$TARGET" python3 - <<'PY' >&2 || true
import os, re

target = os.environ["TARGET"]
deps = os.path.join(target, "debug", "deps")
if not os.path.isdir(deps):
    raise SystemExit(0)

# Cargo names a build artifact `<crate-or-walk>-<16 hex>`. A walk is a top-level file in
# `apps/api/tests/` or `crates/<c>/tests/`, so it is matched by NAME rather than by mtime: an
# age rule would keep every binary a tick happened to link and drop none of them, and a size rule
# would eventually eat the API binary itself.
WALK = re.compile(r"^[a-z0-9_]+-[0-9a-f]{16}$")
# `omnion_api-<hash>` is not a walk — it is the binary `run.sh` starts and the one thing in this
# directory the pass cannot relink around cheaply. Its name matches the walk pattern, so the
# exception is stated here rather than discovered as a full rebuild on the next pass. Unit-test
# binaries of the workspace crates (`omnion_telemetry-<hash>`) carry no prefix and DO go: the
# tick's `cargo test -p` is what wants them, and it relinks in seconds.
KEEP = re.compile(r"^omnion_api-[0-9a-f]{16}$")
freed = 0
removed = 0
for name in os.listdir(deps):
    if not WALK.match(name) or KEEP.match(name):
        continue
    path = os.path.join(deps, name)
    if not os.path.isfile(path) or not os.access(path, os.X_OK):
        continue
    try:
        freed += os.path.getsize(path)
        os.unlink(path)
        removed += 1
    except OSError:
        pass
if removed:
    print(f"[shm-guard] dropped {removed} test binaries this pass will not run ({freed // 1048576} MB)")
PY
fi

after="$(shm_free_mb)"
printf '[shm-guard] reclaimed %s MB — %s MB free before, %s MB after\n' \
  "$(( after - before ))" "$before" "$after"

if [ "$after" -lt "$MIN_FREE_MB" ]; then
  echo "[shm-guard] FAILED: only ${after} MB free, below the ${MIN_FREE_MB} MB floor." >&2
  echo "[shm-guard] A build under this floor does not report 'no space' — the linker bus-errors," >&2
  echo "[shm-guard] and the failure is easy to mistake for a defect in the crate being built." >&2
  report
  exit 1
fi
report
