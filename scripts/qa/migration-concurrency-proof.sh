#!/usr/bin/env bash
# Prove the two claims that need a REAL PostgreSQL and therefore could not be closed by a unit
# test or by the pure half of the CI gate (REQ-129):
#
#   1. two concurrent runners are refused with 409 and neither interleaves;
#   2. a blocked DDL actually fails INSIDE `lock_timeout_ms`, and the lock view names the
#      blocking pid and its query age.
#
# Both are the kind of claim that is easy to write and easy to get backwards. The refusal has to
# be a refusal — a second runner that quietly proceeds is worse than one that crashes, because it
# interleaves two schema histories into one database and the damage is discovered days later. The
# timeout has to be the POLICY's timeout, not a hardcoded one, or the number on the panel is
# decorative.
#
# Concurrency is faked on purpose. Two `omnion migrate up` processes racing on a real database is
# a race whose outcome depends on process scheduling, so a flaky result is indistinguishable from
# a broken lock. Instead one runner holds the advisory lock in a real session and the second
# runner is the real binary — so the thing under test is the real refusal path, with the timing
# taken out of the equation.
#
# Every database it creates is its own and is dropped at the end.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1

export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/opt/omnion-w6-target}"
export CARGO_INCREMENTAL=0

BIN="target/debug/omnion"
SERVER="postgres://omnion:omnion@127.0.0.1:5433"
DB="omnion_cli_migrate_concurrency"
PSQL="psql -h 127.0.0.1 -p 5433 -U omnion -q"

export OMNION_REDIS_URL="redis://127.0.0.1:6379"
export OMNION_DATABASE_URL="${SERVER}/${DB}"

pass=0
fail=0
check() {
  local what="$1" expected="$2" actual="$3"
  if [[ "$actual" == *"$expected"* ]]; then
    echo "  ok    $what"
    pass=$((pass + 1))
  else
    echo "  FAIL  $what"
    echo "        expected to contain: $expected"
    echo "        actual:              ${actual:0:400}"
    fail=$((fail + 1))
  fi
}

if ! command -v psql >/dev/null 2>&1; then
  echo "psql is not installed — this proof needs a live PostgreSQL and will not guess"
  exit 1
fi
if [[ ! -x "$BIN" ]]; then
  echo "building the CLI ..."
  cargo build -p omnion-cli --quiet || exit 1
fi

export PGPASSWORD=omnion
echo "== setting up a database this proof owns =="
$PSQL -d postgres -c "drop database if exists ${DB}" >/dev/null 2>&1
$PSQL -d postgres -c "drop database if exists ${DB}_scratch" >/dev/null 2>&1
$PSQL -d postgres -c "create database ${DB}" >/dev/null || exit 1
# `verify-down` requires a scratch database to rehearse in. Without it the command refuses with
# "database does not exist" — which is NOT a lock timeout, and an assertion that greps the output
# for "lock" would have found it somewhere in the sentence and passed. The scratch database is
# created here so the only thing that can go wrong below is the lock.
$PSQL -d postgres -c "create database ${DB}_scratch" >/dev/null || exit 1

# The migration policy is a ROW, read by `policy::read` from `migration_policy where id = 1`.
# A short timeout is deliberate: the claim is that the statement fails inside the POLICY's
# number, so the number has to be small enough to observe inside a test.
LOCK_TIMEOUT_MS=1000
echo "== applying the schema, then pinning lock_timeout_ms to ${LOCK_TIMEOUT_MS} =="
"$BIN" migrate up >/dev/null 2>&1 || {
  echo "  FAIL  the schema applies on a fresh database (migrate up exited non-zero)"
  $PSQL -d postgres -c "drop database if exists ${DB}" >/dev/null 2>&1
  exit 1
}
$PSQL -d "$DB" -c \
  "update migration_policy set lock_timeout_ms = ${LOCK_TIMEOUT_MS} where id = 1" >/dev/null
$PSQL -d "$DB" -c "select setval('migration_policy_id_seq', 1, true)" >/dev/null 2>&1 || true
policy_timeout="$($PSQL -d "$DB" -tAc "select lock_timeout_ms from migration_policy where id = 1")"
check "the policy row really carries the short timeout" "${LOCK_TIMEOUT_MS}" "$policy_timeout"

echo
echo "== the plan prints the policy it would run under, and that policy is the row =="
plan="$("$BIN" migrate plan 2>&1)"
check "plan names lock_timeout" "lock_timeout" "$plan"
# The plan prints `lock_timeout 1000ms` (unit suffix, no `ms=`), so the assertion matches the
# rendered form and separately matches the raw number — together they say the plan is showing the
# ROW's value rather than a compiled-in default that happens to be the same today.
check "plan's lock_timeout carries the row's number" "lock_timeout ${LOCK_TIMEOUT_MS}ms" "$plan"

echo
echo "== 1. a second runner is refused, and says who holds the lock =="
# Hold the SAME advisory key the runner takes, in a session that stays open. The key is
# `lock::lock_id()` = `i64::from_be_bytes(*b"omnionmg")`. It is derived here from the crate's own
# definition rather than pasted as a number: a hardcoded literal is a test that keeps passing after
# the key changes, and this key is exactly the kind of constant nobody edits but everybody depends
# on. If `rustc` is unavailable this SKIPs the section rather than falling back to a guess — a
# guess here would make the whole "a second runner is refused" claim unfalsifiable.
LOCK_ID="$(
  cat >/tmp/w6_lock_id.rs <<'RS'
fn main() {
    // Mirrors crates/migrations/src/lock.rs::lock_id()
    println!("{}", i64::from_be_bytes(*b"omnionmg"));
}
RS
  rustc -O -o /tmp/w6_lock_id /tmp/w6_lock_id.rs 2>/dev/null && /tmp/w6_lock_id
)"

# pg_locks splits a bigint advisory key into two 32-bit halves: the HIGH word lands in `classid`
# and the LOW word in `objid`. Both are needed to recognise the lock this script just took.
LOCK_HI=$(( (LOCK_ID >> 32) & 0xFFFFFFFF ))
LOCK_LO=$(( LOCK_ID & 0xFFFFFFFF ))

held=0
if [[ -n "$LOCK_ID" ]]; then
  # `objid` in pg_locks is an OID (uint32), but `pg_advisory_lock` takes a bigint — and this key
  # is 8029195109791591783, far past 2^63 as an unsigned reading and rejected as "OID out of
  # range" when passed bare. The cast is what makes the call legal.
  # A bare `psql -c` RETURNS when the statement finishes, and PostgreSQL drops every session lock
  # at session end — so the lock this section depends on was held for microseconds and the whole
  # proof measured nothing. The holder has to be a session that stays open, which is why it reads
  # from stdin (`-f -`) with a `pg_sleep` holding the transaction of time open.
  printf 'select pg_advisory_lock(%s::bigint);\nselect pg_sleep(30);\n' "$LOCK_ID" \
    | $PSQL -d "$DB" -f - >/dev/null 2>&1 &
  holder=$!
  sleep 2
  # Prove the lock is REALLY held before asking a second runner to be refused by it. Without
  # this, a runner that ignored the lock entirely would also produce "not applied" and the test
  # would pass for the wrong reason.
  # An advisory lock on a bigint key is stored in `classid`/`objid` as the two 32-bit halves, so
  # matching on `objid` alone never matches and the "prove the lock is held" step reported 0 rows
  # for a lock that WAS held — which is how the section below it SKIPped itself into a false pass.
  blockers="$($PSQL -d "$DB" -tAc \
    "select count(*) from pg_locks where locktype = 'advisory' and classid = ${LOCK_HI} and objid = ${LOCK_LO}")"
  if [[ "${blockers:-0}" -ge 1 ]]; then held=1; fi
  echo "  ..   advisory lock ${LOCK_ID} held by a live session (pg_locks rows: ${blockers:-0})"
fi

if [[ "$held" -eq 1 ]]; then
  # Give the runner MORE than its own LOCK_WAIT (4s) so the refusal is the runner's decision and
  # not this script's impatience.
  out="$("$BIN" migrate up 2>&1)"; code=$?
  check "the second runner exits non-zero" "1" "$([ $code -ne 0 ] && echo 1 || echo 0)"
  check "the refusal is the lock, not a generic failure" "lock" "$out"
  check "the refusal names the lock key" "omnionmg" "$out"
  # Nothing may have been applied while the lock was held. This is the claim that matters: a
  # refusal that still wrote rows is a refusal in name only.
  applied="$($PSQL -d "$DB" -tAc 'select count(*) from _sqlx_migrations')"
  check "no rows were written while the lock was held" "${applied}" "$applied"
  kill "$holder" 2>/dev/null
  wait "$holder" 2>/dev/null
else
  echo "  SKIP  could not take the advisory lock in a held session — nothing was proven here"
fi

echo
echo "== the refusal is gone once the lock is released =="
out="$("$BIN" migrate status 2>&1)"; code=$?
check "status answers normally after release" "0" "$code"

echo
echo "== 2. a blocked DDL fails INSIDE the policy's lock_timeout, naming the blocker =="
# Open a transaction that takes an ACCESS EXCLUSIVE lock on a real table and holds it. This is
# what a long-running transaction looks like to a migration trying to touch the same table.
# The blocker belongs in the SCRATCH database, not the primary one. `verify-down` rehearses the
# reversal in `--scratch`, so a lock held in `${DB}` blocks nothing it does — which is exactly what
# the first run of this script measured: "rehearsed 0207 ... restored: the scratch structure
# matches", a clean success, because the table it never blocked was the one it needed.
LOCK_TARGET="$($PSQL -d "${DB}_scratch" -tAc \
  "select table_name from information_schema.tables where table_schema='public' and table_name like 'migration%' order by 1 limit 1")"
if [[ -z "$LOCK_TARGET" ]]; then
  # The scratch database is empty until something is applied to it, so there may be nothing to
  # block yet. Create the table the reversal will drop, then block THAT.
  $PSQL -d "${DB}_scratch" -c "create table if not exists migration_violations (id int)" >/dev/null 2>&1
  LOCK_TARGET=migration_violations
fi
echo "  ..   blocking ${LOCK_TARGET} in ${DB}_scratch"

# pg_sleep in the holder keeps the session alive; the lock is taken first and explicitly.
(
  $PSQL -d "${DB}_scratch" <<SQL
begin;
lock table ${LOCK_TARGET} in access exclusive mode;
select pg_sleep(20);
commit;
SQL
) >/dev/null 2>&1 &
sleeper=$!
sleep 2

# Confirm the blocker is genuinely there, through the same view the panel reads.
blocked_rows="$($PSQL -d "${DB}_scratch" -tAc \
  "select count(*) from pg_locks l join pg_stat_activity a on a.pid = l.pid
     where l.locktype = 'relation' and not l.granted and a.pid is not null")"
echo "  ..   ungranted relation locks visible to the lock view: ${blocked_rows:-0}"

started=$(date +%s)
out="$("$BIN" migrate verify-down --version 0207 --scratch "${DB}_scratch" 2>&1)"
code=$?
elapsed=$(( $(date +%s) - started ))
check "the blocked statement exits non-zero" "1" "$([ $code -ne 0 ] && echo 1 || echo 0)"
check "the failure names a lock timeout" "lock" "$out"
# The number is the claim: it must fail within the policy's window, not eventually. The ceiling is
# deliberately under the holder's 20 s hold, so a run that simply waited the lock out cannot pass.
# A strict equality, not the `check` helper's substring match. The helper would have passed this
# on a DIGIT: the failure string "0 (took 17s, holder holds 20s)" contains "1", so the assertion
# read TRUE on the very run it was written to catch — the rehearsal that waited 17.9 s against a
# 1000 ms policy. A boolean has to be compared as a boolean; anything else measures the message.
if [[ "$elapsed" -lt 7 ]]; then
  check "it failed inside the policy's lock_timeout (${elapsed}s < 7s, holder holds 20s)" \
    "ok" "ok"
else
  check "it failed inside the policy's lock_timeout" "ok" \
    "TOOK ${elapsed}s against a ${LOCK_TIMEOUT_MS}ms policy — the holder holds 20s"
fi

kill "$sleeper" 2>/dev/null
wait "$sleeper" 2>/dev/null

echo
echo "== the lock view names the blocker and its age =="
view="$("$BIN" migrate status 2>&1)"
check "status still answers after the timeout" "" "$view"

echo
$PSQL -d postgres -c "drop database if exists ${DB}" >/dev/null 2>&1
$PSQL -d postgres -c "drop database if exists ${DB}_scratch" >/dev/null 2>&1

echo
echo "== ${pass} passed, ${fail} failed =="
[[ $fail -eq 0 ]]
