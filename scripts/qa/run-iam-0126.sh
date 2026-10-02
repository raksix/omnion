#!/usr/bin/env bash
# Omnion — `0126` on a POPULATED `provisioning_log` (REQ-065, slice 4).
#
# A `add column … not null default 0` looks atomic in a text editor and is not what most
# migration accidents are about: the failure mode is the *populated* table, where a rewrite of
# every existing row happens under the constraint being added, and where a check constraint
# written against rows that are all zero never runs its own predicate.
#
# `drop constraint` is worse: a constraint added over a table with no rows is verified against no
# rows at all, so `check (revoked_sessions >= 0)` "passes" and proves nothing. The refusals below
# are therefore asserted against a table that had rows *before* `0126` ran — and the pre-existing
# rows are asserted to still be readable, because a default that rewrites history is the other
# half of the same mistake.
#
# Run with `bash scripts/qa/run-iam-directory.sh`-style isolation: this creates and drops its own
# database and never touches the shared development one.
set -euo pipefail

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="omnion_walk_0126_populated"
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/dev/shm/w9-target}"
export CARGO_INCREMENTAL=0

docker exec "$CONTAINER" psql -U omnion -d postgres -v ON_ERROR_STOP=1 \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null

cleanup() {
  docker exec "$CONTAINER" psql -U omnion -d postgres \
    -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" >/dev/null 2>&1 || true
}
trap cleanup EXIT

psql_() {
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -v ON_ERROR_STOP=1 "$@"
}

echo "== applying every migration up to (but not including) 0126 =="
for file in database/migrations/*.sql; do
  base="$(basename "$file")"
  case "$base" in
    0126_*) break ;;
  esac
  psql_ -q -f - < "$file" >/dev/null
done

echo "== seeding rows that predate 0126 =="
psql_ -q <<'SQL'
insert into organizations (name, slug) values ('0126 Populated', '0126-populated');
insert into provisioning_log (organization_id, direction, resource, action, outcome, detail)
select id, 'inbound', 'user', 'create', 'created', 'a user from before 0126'
from organizations where slug = '0126-populated';
insert into provisioning_log (organization_id, direction, resource, action, outcome, detail)
select id, 'inbound', 'group', 'update', 'updated', 'a group from before 0126'
from organizations where slug = '0126-populated';
SQL

before="$(psql_ -t -A -c "select count(*) from provisioning_log")"
test "$before" = "2" || { echo "FAIL: expected 2 pre-existing rows, found $before"; exit 1; }

echo "== applying 0126 over the populated table =="
psql_ -q -f - < database/migrations/0126_provisioning_log_revoked_sessions.sql

echo "== the pre-existing rows survived and read 0 =="
after="$(psql_ -t -A -c "select count(*) from provisioning_log")"
test "$after" = "2" || { echo "FAIL: 0126 changed the row count to $after"; exit 1; }

zeros="$(psql_ -t -A -c "select count(*) from provisioning_log where revoked_sessions = 0")"
test "$zeros" = "2" || { echo "FAIL: pre-existing rows must read 0, got $zeros"; exit 1; }

details="$(psql_ -t -A -c "select string_agg(detail, ' | ' order by id) from provisioning_log")"
case "$details" in
  *"a user from before 0126"*) : ;;
  *) echo "FAIL: 0126 rewrote a pre-existing row's detail: $details"; exit 1 ;;
esac

echo "== a negative count is refused, and the constraint is the one that refuses it =="
if psql_ -q -c "update provisioning_log set revoked_sessions = -1" >/dev/null 2>&1; then
  echo "FAIL: the check constraint did not refuse -1"
  exit 1
fi

# The narrow-constraint lesson: if two constraints survive, the wider one silently wins. Reading
# the surviving definition is the only way to know the one that is actually in force — a `drop
# constraint` that names a constraint which is not there reports nothing on this database.
def="$(psql_ -t -A -c "
  select pg_get_constraintdef(oid) from pg_constraint
  where conname = 'provisioning_log_revoked_sessions_check'")"
echo "   in force: $def"
case "$def" in
  *revoked_sessions*">= 0"*) : ;;
  *) echo "FAIL: the surviving constraint is not the non-negative one: $def"; exit 1 ;;
esac

echo "== a real count is accepted =="
psql_ -q -c "update provisioning_log set revoked_sessions = 3 where action = 'create'"
read_back="$(psql_ -t -A -c "select revoked_sessions from provisioning_log where action = 'create'")"
test "$read_back" = "3" || { echo "FAIL: a legal count did not survive, got $read_back"; exit 1; }

echo "== the partial index is there and is partial =="
indexdef="$(psql_ -t -A -c "select indexdef from pg_indexes where indexname = 'provisioning_log_revoked_idx'")"
echo "   $indexdef"
case "$indexdef" in
  *'revoked_sessions > 0'*) : ;;
  *) echo "FAIL: the index is not the partial one the migration describes"; exit 1 ;;
esac

# A non-partial index of the same name would answer the same question more slowly and would carry
# every row forever; the predicate is the point, so it is asserted rather than assumed.
sized="$(psql_ -t -A -c "
  select indexdef like '%WHERE (revoked_sessions > 0)%' from pg_indexes
  where indexname = 'provisioning_log_revoked_idx'")"
test "$sized" = "t" || { echo "FAIL: the index lost its predicate"; exit 1; }

echo "== re-applying 0126 is refused (a migration is not idempotent by accident) =="
if psql_ -q -f - < database/migrations/0126_provisioning_log_revoked_sessions.sql >/dev/null 2>&1; then
  echo "FAIL: 0126 applied twice without a complaint"
  exit 1
fi

echo
echo "PASS 0126_populated: 2 pre-existing rows survived and read 0, -1 is refused by the"
echo "                    non-negative constraint that is actually in force, a real count survives,"
echo "                    the index keeps its predicate, and a second apply is refused."
