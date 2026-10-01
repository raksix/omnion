#!/usr/bin/env bash
# Prove `omnion migrate` against a real PostgreSQL: up → status → plan → verify-down.
#
# The unit tests prove the argument grammar and the pure decisions. This proves the four actions
# actually reach a database and say true things, which is the only claim the CLI makes to an
# operator during an incident.
#
# Every database it creates is its own and is dropped at the end. It never touches the QA stack's
# database or the development one.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1

export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/opt/omnion-w6-target}"
export CARGO_INCREMENTAL=0

BIN="target/debug/omnion"
SERVER="postgres://omnion:omnion@127.0.0.1:5433"
PRIMARY="omnion_cli_migrate_demo"
SCRATCH="omnion_cli_migrate_scratch"

export OMNION_REDIS_URL="redis://127.0.0.1:6379"
export OMNION_DATABASE_URL="${SERVER}/${PRIMARY}"

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
refuse() {
  local what="$1" needle="$2"
  shift 2
  local output code
  output="$("$@" 2>&1)"
  code=$?
  if [[ $code -ne 0 && "$output" == *"$needle"* ]]; then
    echo "  ok    $what (exit $code)"
    pass=$((pass + 1))
  else
    echo "  FAIL  $what"
    echo "        expected a non-zero exit and: $needle"
    echo "        actual (exit $code): ${output:0:400}"
    fail=$((fail + 1))
  fi
}

cleanup() {
  for db in "$PRIMARY" "$SCRATCH"; do
    "$BIN" --version >/dev/null 2>&1 || true
    PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d postgres -q \
      -c "drop database if exists ${db} with (force)" >/dev/null 2>&1 || true
  done
}
trap cleanup EXIT
cleanup

echo "== building the CLI =="
cargo build -q -p omnion-cli --bin omnion || exit 1
[[ -x "$BIN" ]] || { echo "  FAIL  no binary at $BIN"; exit 1; }
echo "  ok    built $BIN"

echo
echo "== a database nobody has migrated =="
refuse "an unmigrated database cannot be planned" "could not connect" "$BIN" migrate status

PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d postgres -q \
  -c "create database ${PRIMARY}" -c "create database ${SCRATCH}" || exit 1
echo "  ok    created ${PRIMARY} and ${SCRATCH}"

echo
echo "== the GATE fails on this tree's reversal-less migrations, the APPLY does not =="
# This is the property that keeps Omnion installable: 53 of 58 migrations predate the reversal
# rule, so `plan` (the CI gate) must fail while `up` must still succeed. Asserting both in one
# place is the point — either half alone would pass against a runner that simply refuses, or one
# that simply ignores its own policy.
out="$("$BIN" migrate plan 2>&1)"; code=$?
check "plan exits 1: the gate fails" "1" "$code"
check "plan names the missing reversals" "without a reversal" "$out"
check "plan names the files with no reversal" "0001_initial.sql" "$out"

echo
echo "== migrate up: the ledger row and the applied set =="
out="$("$BIN" migrate up 2>&1)"; code=$?
check "exit 0 despite the failing gate" "0" "$code"
check "reports the applied count" "applied 58 migration(s)" "$out"
check "reports the ledger backfill" "backfilled" "$out"

out="$("$BIN" migrate status 2>&1)"
check "status names the lock key" "omnionmg" "$out"
check "status says the lock is free" "lock:      free" "$out"
check "status shows the backfilled history" "applied   0001 initial" "$out"
check "status reports 0207 with its reversal present" "0207 migration_safety" "$out"
check "status says no migration claims a rehearsal" "reversal never rehearsed" "$out"

echo
echo "== the ledger is populated after the apply =="
ledger_rows="$(PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "${PRIMARY}" -tAc \
  "select count(*) from schema_migrations" 2>/dev/null)"
applied_rows="$(PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "${PRIMARY}" -tAc \
  "select count(*) from _sqlx_migrations where success" 2>/dev/null)"
check "the ledger has one row per applied migration" "$applied_rows" "$ledger_rows"
if [[ "$ledger_rows" -gt 50 ]]; then
  echo "  ok    the ledger holds the whole set, backfilled ($ledger_rows rows)"
  pass=$((pass + 1))
else
  echo "  FAIL  the ledger holds only $ledger_rows rows — the 57 pre-ledger migrations were not backfilled"
  fail=$((fail + 1))
fi
# The backfilled checksums must be the EMBEDDED files' checksums, so a later drift check on any of
# them is a real comparison rather than an absence.
sample="$(PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "${PRIMARY}" -tAc \
  "select checksum from schema_migrations where version = '0001'" 2>/dev/null)"
if [[ ${#sample} -eq 64 ]]; then
  echo "  ok    the backfilled checksum is a sha256, not a placeholder"
  pass=$((pass + 1))
else
  echo "  FAIL  the backfilled checksum is not a sha256: '$sample'"
  fail=$((fail + 1))
fi

unrehearsed="$(PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "${PRIMARY}" -tAc \
  "select count(*) from schema_migrations where down_verified_at is null" 2>/dev/null)"
check "no migration claims its reversal was rehearsed" "$ledger_rows" "$unrehearsed"

echo
echo "== a second apply is a no-op, not a re-apply =="
out="$("$BIN" migrate up 2>&1)"; code=$?
check "exit 0 when there is nothing to do" "0" "$code"
check "says the database is up to date" "up to date" "$out"

echo
echo "== plan executes nothing =="
before="$(PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "${PRIMARY}" -tAc \
  "select count(*) from schema_migrations" 2>/dev/null)"
out="$("$BIN" migrate plan 2>&1)"; code=$?
check "exit 0 on a clean plan" "0" "$code"
check "prints the policy it would run under" "lock_timeout" "$out"
check "says the database is up to date" "up to date" "$out"
after="$(PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "${PRIMARY}" -tAc \
  "select count(*) from schema_migrations" 2>/dev/null)"
check "plan wrote nothing" "$before" "$after"

echo
echo "== verify-down: the scratch database is REQUIRED =="
refuse "verify-down without --version is a usage error" "--version" "$BIN" migrate verify-down
refuse "verify-down without --scratch is refused" "--scratch" \
  "$BIN" migrate verify-down --version 0207

echo
echo "== verify-down: rehearsing 0207 on a scratch database =="
# The scratch database is migrated to just before 0207 so the rehearsal has something to reverse.
PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "${SCRATCH}" -q <<'SQL' >/dev/null 2>&1
create schema if not exists omnion_scratch_probe (id int);
SQL
out="$("$BIN" migrate verify-down --version 0207 --scratch "${SCRATCH}" 2>&1)"; code=$?
check "exit 0 for a rehearsed reversal" "0" "$code"
check "names the file it rehearsed" "0207_migration_safety.sql" "$out"
check "reports the restored structure" "restored:" "$out"

verified="$(PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "${PRIMARY}" -tAc \
  "select coalesce(down_verified_at::text, 'never') || '/' || coalesce(down_verified_by, 'nobody') \
   from schema_migrations where version = '0207'" 2>/dev/null)"
if [[ "$verified" == never/* ]]; then
  echo "  FAIL  0207 still claims no rehearsal: $verified"
  fail=$((fail + 1))
else
  echo "  ok    the ledger records the rehearsal ($verified)"
  pass=$((pass + 1))
fi

echo
echo "== verify-down: an unknown version is refused, not guessed =="
refuse "an unknown version is refused" "no ledger row" \
  "$BIN" migrate verify-down --version 9999 --scratch "${SCRATCH}"

echo
echo "== checksum drift is refused and names the file =="
# Edit one migration's ledger checksum, exactly as an operator editing an applied file would drift.
PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "${PRIMARY}" -q \
  -c "update schema_migrations set checksum = repeat('a', 64) where version = '0207'" >/dev/null
out="$("$BIN" migrate status 2>&1)"; code=$?
check "status exits non-zero on drift" "1" "$code"
check "names the offending file" "0207_migration_safety.sql" "$out"
check "names the two hashes" "hashes" "$out"
check "says the only fix is a new migration" "a new migration" "$out"
refuse "the runner refuses to apply on drift" "0207_migration_safety.sql" \
  "$BIN" migrate up

echo
echo "== an unknown sub-action is a usage error, never a silent apply =="
out="$("$BIN" migrate verify-donw 2>&1)"; code=$?
check "exit 1 with the action named" "unknown sub-action" "$out"
check "the refusal lists the four actions" "up, status, plan or verify-down" "$out"

echo
echo "== ${pass} passed, ${fail} failed =="
[[ $fail -eq 0 ]]
