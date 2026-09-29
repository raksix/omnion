#!/usr/bin/env bash
# The disk guard, proved on a decoy root.
#
#   bash scripts/qa/test-disk-guard.sh
#
# The guard is the one script on the box whose failure mode is *silent and destructive*: it
# `rm -rf`s a directory another writer is using, and the writer that loses it reports a build
# error, a `cp: cannot create regular file`, or a green gate that suddenly stops compiling.
# None of those name the guard, which is why the guard has to be asserted rather than trusted.
#
# Four defects are asserted here, each with the incident that produced it:
#
#   1. **A target over the ceiling is protected when a pass is living in its worktree.** The
#      per-worktree ceiling ran `rm -rf` with no liveness question at all. A QA pass builds
#      into a tmpfs CARGO_TARGET_DIR and then copies the binary into `target/debug/` — so
#      target/ is over the ceiling *precisely because* the pass just filled it, and the guard
#      deleted the directory between the build and the copy.
#   2. **`worktree_busy` must resolve a worktree's OWN target/ to that worktree.** It derived
#      the worktree from the basename, so `omnion-w9/target` produced the token `target`,
#      looked for `omnion-target` (which does not exist), and fell through to its last
#      candidate — the main checkout. The one directory whose own liveness matters most
#      resolved to a tree nobody was in.
#   3. **`in_use` must match a relative `CARGO_TARGET_DIR`.** An interactive shell that
#      exported `.tmp-target` and then `cd`'d into the worktree carries the bare name, and
#      comparing that against an absolute path answers "not held" for a build that is running.
#   4. **An idle worktree is still reclaimable.** A liveness test that is simply always-true
#      would pass 1–3 and leak the disk for ever, which is the failure mode that replaces the
#      one being fixed. It is asserted because it is the only way a guard of this shape is
#      safe to leave switched on.
set -uo pipefail
cd "$(dirname "$0")/../.."

PASS=0
FAIL=0
ok()  { PASS=$((PASS + 1)); echo "  ok   $*"; }
bad() { FAIL=$((FAIL + 1)); echo "  FAIL $*"; }

SANDBOX="$(mktemp -d)"

# The guard reads its root from OMNION_ROOT and its tmpfs from OMNION_SHM, so the whole thing
# runs against a decoy filesystem and the real worktrees are never candidates.
#
# These are exported because the guard is invoked as a child (`bash scripts/qa/disk-guard.sh`),
# and an export is the only way to reach it. The cost of that is that they stay set in the
# *calling* shell for as long as this script runs — and a sourced test that leaves a decoy root
# behind in the environment of the shell that ran it produces the most confusing failure there
# is: the next real `bash scripts/qa/disk-guard.sh` then sweeps a deleted tmpdir, prints
# `integer expression expected` twice, and reports a free count of nothing. They are therefore
# unset again on the way out, whether the test passed or failed.
export OMNION_ROOT="$SANDBOX/root"
export OMNION_SHM="$SANDBOX/shm"
mkdir -p "$OMNION_ROOT" "$OMNION_SHM"
restore_env() {
  unset OMNION_ROOT OMNION_SHM
  rm -rf "$SANDBOX"
}
trap restore_env EXIT

# A worktree with a `target/` of a known size, and a second one with none.
mk_worktree() { # name, megabytes
  local wt="$OMNION_ROOT/omnion-$1" mb="${2:-0}"
  mkdir -p "$wt/target/debug"
  [ "$mb" -gt 0 ] && dd if=/dev/zero of="$wt/target/blob" bs=1M count="$mb" status=none
  echo "$wt"
}

# Run the guard with a ceiling low enough to act, and report what it dropped.
run_guard() { # max_mb, min_free_gb
  WORKTREE_TARGET_MAX_MB="$1" MIN_FREE_GB="$2" OMNION_SHM_MIN_FREE_PCT=0 \
    bash scripts/qa/disk-guard.sh 2>&1
}

# Load the guard's own functions into THIS shell so the direct assertions below can call them.
# Sourcing runs its body once, which is harmless and in fact desirable: it proves the script
# still executes end to end against the decoy root before its functions are trusted. It is not
# done in a subshell, because a subshell's functions do not outlive it.
# shellcheck source=/dev/null
source scripts/qa/disk-guard.sh >/dev/null 2>&1

echo "[disk-guard] 0. the guard runs end to end against a decoy root"
grep -q "free" <<<"$(run_guard 20 99999)" \
  && ok "it executes and prints its own summary" \
  || bad "it printed no summary — it died before its last line"

echo "[disk-guard] 1. a live pass protects its own target/ from the ceiling"
WT="$(mk_worktree live 40)"
# A process living in the worktree is what a pass looks like from outside: the wrapper exits
# when it stages the binary, and the walkthrough's node and chromium children do not carry
# CARGO_TARGET_DIR at all.
( cd "$WT" && exec sleep 120 ) &
BUSY_PID=$!
sleep 0.4
OUT="$(run_guard 20 99999)"
if [ -d "$WT/target" ]; then
  ok "target/ survives while a pass is living in the worktree"
else
  bad "target/ was deleted out from under a running pass — output: $OUT"
fi
if grep -q "over the 20M ceiling" <<<"$OUT"; then
  bad "the guard announced a drop it did not perform"
else
  ok "and said nothing about dropping it"
fi
kill "$BUSY_PID" 2>/dev/null; wait "$BUSY_PID" 2>/dev/null

echo "[disk-guard] 2. the ceiling still reclaims an idle worktree"
WT2="$(mk_worktree idle 40)"
OUT="$(run_guard 20 99999)"
if [ -d "$WT2/target" ]; then
  bad "an idle over-ceiling target was kept — the guard is now leaking the disk"
else
  ok "idle over-ceiling target is dropped"
fi
grep -q "over the 20M ceiling" <<<"$OUT" && ok "and it says which one" || bad "no line naming the drop"

echo "[disk-guard] 3. worktree_of resolves a worktree's own target/ to that worktree"
# The fixtures these assertions name have to exist, because the tmpfs arm of `worktree_of`
# resolves through the *filesystem* (`[ -d "$ROOT/omnion-$w" ]`) and correctly answers
# "nothing" for a worktree that was never created. That is the honest answer, and asserting it
# against a missing directory would have tested nothing at all.
W9="$(mk_worktree w9 0)"
MINE="$(worktree_of "$OMNION_ROOT/omnion-w9/target")"
[ "$MINE" = "$OMNION_ROOT/omnion-w9" ] \
  && ok "omnion-w9/target → omnion-w9" \
  || bad "omnion-w9/target resolved to '${MINE:-nothing}' (it used to fall through to main)"
MAIN="$(worktree_of "$OMNION_ROOT/omnion/target")"
[ "$MAIN" = "$OMNION_ROOT/omnion" ] \
  && ok "omnion/target → omnion" \
  || bad "omnion/target resolved to '${MAIN:-nothing}'"
SHM_DIR="$(worktree_of "$OMNION_SHM/w9-target")"
[ "$SHM_DIR" = "$OMNION_ROOT/omnion-w9" ] \
  && ok "w9-target → omnion-w9 (the tmpfs naming still works)" \
  || bad "w9-target resolved to '${SHM_DIR:-nothing}'"

echo "[disk-guard] 4. a process under the worktree counts as living in it"
# `run.sh` leaves the admin server with its cwd in apps/admin, and Chromium's children in the
# worktree root. Neither is the process that started them, so the test puts a process in a
# SUBDIRECTORY and asserts the prefix arm — the case an equality test would miss.
mkdir -p "$W9/apps/admin"
( cd "$W9/apps/admin" && exec sleep 120 ) &
DEEP_PID=$!
sleep 0.4
if worktree_busy "$OMNION_ROOT/omnion-w9/target"; then
  ok "a live process under apps/admin reads as busy"
else
  bad "a process one level below the worktree did not read as busy"
fi
kill "$DEEP_PID" 2>/dev/null; wait "$DEEP_PID" 2>/dev/null
if worktree_busy "$OMNION_ROOT/omnion-w9/target" 2>/dev/null; then
  bad "the worktree still reads as busy after the process exited"
else
  ok "and stops reading as busy once it exits"
fi

echo "[disk-guard] 5. in_use matches a relative CARGO_TARGET_DIR"
REL_WT="$(mk_worktree rel 1)"
( cd "$REL_WT" && CARGO_TARGET_DIR=.tmp-target exec sleep 120 ) &
REL_PID=$!
sleep 0.4
if in_use "$REL_WT/.tmp-target"; then
  ok "a build into a bare relative target is held"
else
  bad "a running relative-CARGO_TARGET_DIR build was reported as unused"
fi
kill "$REL_PID" 2>/dev/null; wait "$REL_PID" 2>/dev/null
( cd "$REL_WT" && CARGO_TARGET_DIR="$REL_WT/.tmp-target" exec sleep 120 ) &
ABS_PID=$!
sleep 0.4
in_use "$REL_WT/.tmp-target" \
  && ok "an absolute CARGO_TARGET_DIR is still held" \
  || bad "an absolute CARGO_TARGET_DIR stopped being detected"
kill "$ABS_PID" 2>/dev/null; wait "$ABS_PID" 2>/dev/null

echo "[disk-guard] 6. a dead worktree is not busy (the leak guard)"
if worktree_busy "$OMNION_ROOT/omnion-nobody/target" 2>/dev/null; then
  bad "a worktree with no processes in it still reads as busy"
else
  ok "an idle worktree reads as not busy"
fi

echo "[disk-guard] $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
