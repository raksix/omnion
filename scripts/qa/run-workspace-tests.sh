#!/usr/bin/env bash
# Omnion — the whole-workspace test gate, on a disposable database.
#
# `cargo test --workspace` is the gate every request closes on (docs/BUILD-PLAN-v2.md §4), and
# running it bare is a lie: the integration walks take their database from `OMNION_DATABASE_URL`,
# and with that variable unset they fall through to `DEFAULT_DATABASE_URL` — the SHARED
# `omnion` database. This box runs seven writers, so that database carries a migration 19
# from whichever branch got there first (`0019_cms_blocks` in wave 2, `0019_secret_hierarchy`
# here) and sqlx's per-version checksum can never match. The run then dies with
# `migration 19 was previously applied but has been modified` before a single assertion, in
# every DB-backed suite at once.
#
# That failure has been read as a repository defect for at least one tick, twice, on this wave
# — including a suite that is in fact 4/4 green. The message is a lie about its own cause: it
# names an edited migration when what happened is that nobody said WHICH database. Two tickets
# have now been spent on it (`0019_secret_hierarchy` was rebuilt from scratch once, and the
# blame went to a suite that had never been pointed anywhere).
#
# So this script makes the database explicit, disposable and derived from the branch, which is
# what the media walks already do (scripts/qa/run-media-walk.sh) for the subset nobody made
# universal. It is the general form of that script:
#
#   - the database is named after the BRANCH, not the worktree path, so two worktrees of the
#     same branch share a warm database and two branches never share one;
#   - it is CREATED IF MISSING rather than dropped, because a workspace gate runs 40+ suites
#     and a fresh drop per invocation would pay the migrate cost 40 times over;
#
#   **The three failures this run reported were NOT leftover rows, and the drop above was the
#   wrong fix.** The first guess was: `apps/api/tests/automation.rs` seeds the matcher cursor and
#   asserts `report.evaluated == 1`, so a reused bus makes the second run read the first run's
#   events back. Measured, and it is false — a freshly created, empty database fails identically
#   (3/5, `left: 2, right: 1`). The bus carries BOTH `page.created` (id 1) and `page.published`
#   (id 2), and `matcher::seed_cursor` only moves a cursor that is still at 0, so a single publish
#   is two events. The real break is main's `056b04d` adding `page.created` to a walk that
#   hard-codes one, in wave 3's file. Logged, not papered over: a gate whose fix is "delete the
#   rows" would have gone green against a repository that genuinely emits two events.
#   - `--no-fail-fast` is not optional. Cargo stops at the first failing target, so one red suite
#     hides the other ~40: the run that found those three failures reported six suites and
#     `error: test failed`, and every suite after `automation` was never executed. A gate that
#     reports a subset of the repository while reading as a verdict on it answers a different
#     question than the one it was asked.
#   - `--test-threads=1` because several suites legitimately share tables (the alert evaluator
#     is database-wide, the media rollups have a per-day salt) and parallel tests would report
#     product defects that do not exist;
#   - the gate is never pointed at the development database or at `omnion_qa`, both of which
#     are reset by other processes and turn into a migration error that has nothing to do with
#     the code under test.
#
# Usage: scripts/qa/run-workspace-tests.sh [extra cargo test args ...]
set -euo pipefail

BRANCH="${QA_BRANCH:-$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo detached)}"
# Only [A-Za-z0-9_] survives into a SQL identifier, so a branch like `feat/x` cannot inject or
# simply fail. `main` deliberately keeps the historical name so CI and local runs agree.
SAFE="$(printf '%s' "$BRANCH" | tr -c 'A-Za-z0-9_' '_' | cut -c1-32)"
DB="omnion_gate_${SAFE}"

PGHOST="${QA_PGHOST:-127.0.0.1}"
PGPORT="${QA_PGPORT:-5433}"
PGUSER="${QA_PGUSER:-omnion}"
export PGPASSWORD="${QA_PGPASSWORD:-omnion}"

case "$DB" in
  omnion|omnion_qa|"")
    echo "[gate] refusing to run against '$DB' — that database belongs to another process" >&2
    exit 2
    ;;
esac

psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -v ON_ERROR_STOP=1 -tAc \
  "select 1 from pg_database where datname = '$DB'" | grep -q 1 \
  || psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -v ON_ERROR_STOP=1 \
       -c "create database \"$DB\" owner \"$PGUSER\"" >/dev/null

export OMNION_DATABASE_URL="postgres://${PGUSER}@${PGHOST}:${PGPORT}/${DB}"
export OMNION_REDIS_URL="${QA_REDIS_URL:-redis://127.0.0.1:6380}"
export OMNION_ENV="${OMNION_ENV:-development}"
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"

# **Debug info off, and this is not optional on a box that builds in tmpfs.**
# Each of the ~40 integration test binaries is a 145 MB executable when it carries debuginfo,
# so the workspace gate alone wants ~5.8 GB of a directory that seven writers share. When that
# does not fit, `ld` does not say "out of space": it dies with
#
#     collect2: fatal error: ld terminated with signal 7 [Bus error]
#
# and reports `could not compile omnion-api (test "webauthn")` — a *source* error for a
# *filesystem* condition, in whichever suite happened to link when the tmpfs filled. That
# misdiagnosis costs a tick every time, and the fix (`df -h /dev/shm`) is nowhere in the
# message. Nothing in this repository debugs a test binary: they are built to be run, and the
# panic output is identical without debuginfo. Set `QA_DEBUG_INFO=1` when someone is
# actually stepping through one.
if [ "${QA_DEBUG_INFO:-0}" != "1" ]; then
  export CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG:-0}"
  export CARGO_PROFILE_TEST_DEBUG="${CARGO_PROFILE_TEST_DEBUG:-0}"
fi

echo "[gate] database $DB (branch $BRANCH)"
# `--no-fail-fast` so one red target cannot hide the suites behind it. See the note above: the
# run that found the three failures reported six of forty-three, and read as a verdict.
cargo test --workspace --no-fail-fast --quiet -- --test-threads=1 "$@"
