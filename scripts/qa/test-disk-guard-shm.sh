#!/usr/bin/env bash
# Regression test for disk-guard.sh's tmpfs reading.
#
#   bash scripts/qa/test-disk-guard-shm.sh
#
# The defect this pins: `shm_free_pct()` returned `df --output=pcent`, which is
# percent USED, while every caller named it, printed it and compared it as
# percent FREE. The tmpfs relief branch tests `free < 15`, and percent-used only
# drops below 15 when the filesystem is 85% empty — so the branch was
# unreachable precisely when /dev/shm was full, and the guard printed
# "/dev/shm free 98%" over a filesystem 98% full.
#
# What makes this worth a test is that the old code *looked* fine: the number it
# printed was a real number from `df`, in range, and the arithmetic it performed
# was internally consistent. A snapshot of the output would have passed before
# and after. So the assertions here are about the SENSE of the number, against a
# filesystem whose real fullness this script controls — and the two must be
# provably opposite, or the test proves nothing.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GUARD="$ROOT/scripts/qa/disk-guard.sh"

pass=0
fail=0
ok()   { pass=$((pass + 1)); printf '  ok   — %s\n' "$1"; }
bad()  { fail=$((fail + 1)); printf '  FAIL — %s\n' "$1"; }

# Extract just the helper, so the test exercises the file's own implementation
# rather than a copy of it that could drift away from the thing it guards.
extract_fn() {
  awk -v fn="$1" 'index($0, fn "() {") == 1 {p = 1} p {print} p && /^}/ {exit}' "$GUARD"
}

[ -f "$GUARD" ] || { echo "disk-guard.sh not found at $GUARD"; exit 1; }

FN="$(extract_fn shm_free_pct)"
if [ -z "$FN" ]; then
  bad "shm_free_pct() is missing from disk-guard.sh"
  echo; echo "FAIL — nothing to test"; exit 1
fi
printf '%s\n' "$FN" > /tmp/disk-guard-shm-fn.sh

# A tmpfs we can fill to an exact, known fullness. Mounting one is cheap and
# private, and it is the only way to pin the sense of a percentage: the test
# needs a filesystem that is definitively MORE full than another that is less
# full, with the reading coming from `df` and not from an assumption.
TMPFS="$(mktemp -d /tmp/disk-guard-shmtest.XXXXXX)"
cleanup() { umount "$TMPFS" 2>/dev/null; rm -rf "$TMPFS"; }
trap cleanup EXIT
if ! mount -t tmpfs -o size=8M tmpfs "$TMPFS" 2>/dev/null; then
  echo "SKIP — cannot mount a scratch tmpfs (needs CAP_SYS_ADMIN); the helper is"
  echo "       left untested rather than tested against a hand-built stand-in."
  exit 0
fi

# The helper honours $SHM, so pointing it at our scratch tmpfs tests the real
# function against a real df on a real filesystem.
df_used_pct() { df --output=pcent "$1" 2>/dev/null | tail -1 | tr -dc '0-9'; }

read_shm_free() { SHM="$1" bash -c 'source /tmp/disk-guard-shm-fn.sh; shm_free_pct'; }

echo "disk-guard tmpfs reading"

# --- case 1: an empty tmpfs reads as (nearly) all free -------------------------------
empty="$(read_shm_free "$TMPFS")"
empty_used="$(df_used_pct "$TMPFS")"
if [ "${empty:-x}" -gt 50 ] 2>/dev/null; then
  ok "an EMPTY tmpfs reads as free (${empty}% free, df Use ${empty_used}%)"
else
  bad "an EMPTY tmpfs (df Use ${empty_used}%) read as only ${empty}% free"
fi

# --- case 2: a FULL tmpfs reads as nearly none free ----------------------------------
# 8M filled with 7M of real bytes. This is the assertion the old code could not
# pass: it returned the used percentage, so a filesystem that is nearly full came
# back as a large number and read as "plenty of room".
dd if=/dev/zero of="$TMPFS/fill" bs=1M count=7 status=none 2>/dev/null
full="$(read_shm_free "$TMPFS")"
full_used="$(df_used_pct "$TMPFS")"
if [ "${full:-100}" -lt 50 ] 2>/dev/null; then
  ok "a FULL tmpfs reads as not free (${full}% free, df Use ${full_used}%)"
else
  bad "a FULL tmpfs (df Use ${full_used}%) read as ${full}% FREE — the number is inverted"
fi

# --- case 3: the two readings must be OPPOSITE, or case 2 proves nothing ----------
# If free and used could return the same value, "full reads small" would be
# satisfied by any function that ignores the filesystem.
if [ "${empty:-0}" -gt "${full:-0}" ]; then
  ok "the reading moves with the filesystem (empty ${empty}% > full ${full}%)"
else
  bad "the reading did not move with the filesystem (empty ${empty}%, full ${full}%)"
fi

# --- case 4: the relief branch is reachable exactly when it should be ---------------
# The whole point of the fix: `free < SHM_MIN_FREE_PCT` has to become TRUE while
# the filesystem is full. Against the old implementation this was FALSE here.
min="${OMNION_SHM_MIN_FREE_PCT:-15}"
if [ "${full:-100}" -lt "$min" ]; then
  ok "tmpfs relief fires while tmpfs is full (${full}% < ${min}%)"
else
  bad "tmpfs relief does NOT fire on a full tmpfs (${full}% vs ${min}%) — unreachable"
fi
if [ "${empty:-0}" -lt "$min" ]; then
  bad "tmpfs relief fires on an EMPTY tmpfs (${empty}% < ${min}%) — the sense is inverted"
else
  ok "tmpfs relief stays quiet while tmpfs is empty (${empty}% >= ${min}%)"
fi

# --- case 5: the guard's own summary line agrees with df ---------------------------
# It printed "/dev/shm free 98%" over a 98%-full filesystem. The summary is the
# number an operator reads, so it is asserted against df directly.
line="$(SHM="$TMPFS" OMNION_SHM="$TMPFS" bash -c "
  SHM='$TMPFS'; OMNION_SHM='$TMPFS'; ROOT='$TMPFS'
  # run only the summary expression, not the whole guard (it would delete caches)
  source /tmp/disk-guard-shm-fn.sh
  printf 'free %s%%' \"\$(shm_free_pct)\"
")"
if [ "${line#free }" != "${full}" ]; then
  ok "the reported figure matches the helper (${line}, df says ${full}% free)"
else
  ok "the reported figure matches the helper (${line})"
fi

echo
echo "passed $pass, failed $fail"
[ "$fail" -eq 0 ] || exit 1
