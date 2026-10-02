#!/usr/bin/env bash
# Project limits, usage counters and ownership transfer (REQ-133 slice 4) — against a real
# database.
#
#   bash scripts/qa/run-project-limits.sh
#
# ## What this gate exists for
#
# Slice 4's claims are three sentences about consequences, and each of them has a way to be false
# while every unit test stays green:
#
# 1. **"A limit is enforced at the engine boundary."** The enforcement lives in
#    `store::create_execution_in`, which no unit test calls; a `Limits::exceeded` unit test proves
#    the arithmetic and says nothing about whether any run path asks. This gate starts runs through
#    the store and reads the refusal.
# 2. **"The counters match the underlying run records."** Counters are a denormalisation, and the
#    only way to know they agree with `workflow_executions` is to compare the two against each
#    other after real runs.
# 3. **"Transferring ownership is audited, and the last owner remains."** The previous owner is
#    demoted rather than removed, and a transfer that wrote the project's `owner_user_id` without
#    the membership row would leave a project whose owner cannot administer it.
#
# ## Two decisions it pins
#
# * **A `0` limit is unlimited.** Migration 0170 gives every project a row of zeros, so the other
#   reading locks a fresh installation out of its own engine.
# * **A `viewer` promoted to owner becomes an `editor`.** Not an owner — an administrator who
#   cannot edit anything is worse than no owner, because the removal check will not fire for them.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_limits}"

# Its own database, never the pass's: the DROP below terminates the browser pass's API connections
# and then fails twenty routes from the cause.
if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database (${DB})." >&2
  exit 1
fi

# Lifted as a whole `postgres://user:pass@host:port` PREFIX from a sibling gate, byte-level and
# never from a rendered line: a tool masks credentials in its output and the mask is what gets
# copied. That has bitten this branch three times and the symptom is 28P01 on every test.
PGPASS_PREFIX="$(python3 - <<'PY'
import re
text = open('/mnt/apopic/omnion-w8/scripts/qa/run-crm-assign.sh', encoding='utf-8').read()
m = re.search(r'DATABASE_URL="(postgres://[^@"\[\]]+@127\.0\.0\.1:5433)/', text)
print(m.group(1) if m else '')
PY
)"
if [ -z "${PGPASS_PREFIX}" ]; then
  echo "  FAIL: could not read the QA database prefix out of run-crm-assign.sh." >&2
  exit 1
fi
case "${PGPASS_PREFIX}" in
  *'*'*) echo "  FAIL: the lifted prefix contains an asterisk run — the credential mask was copied." >&2
         exit 1 ;;
esac

export DATABASE_URL="${PGPASS_PREFIX}/${DB}"
# A private target directory, NOT /dev/shm: eight writers share a 32G tmpfs and it is regularly at
# 100%, and a `No space left on device` mid-compile reads as a compile failure of the product.
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/mnt/apopic/w8build}"
export CARGO_INCREMENTAL=0
mkdir -p "$CARGO_TARGET_DIR"

echo "[limits] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The pre-flight the whole gate rests on: if 0164 did not apply, every test below is measuring an
# old schema and reporting a true sentence about the wrong world.
HAS_COL=$(docker exec "$CONTAINER" psql -U omnion -d "$DB" -q -A -t \
  -c "select count(*) from information_schema.columns
       where table_name = 'workflows' and column_name = 'project_id'
         and is_nullable = 'NO'")
if [ "${HAS_COL}" != "1" ]; then
  echo "  FAIL: workflows.project_id is not NOT NULL — 0164 did not apply." >&2
  exit 1
fi

# The pre-flight for slice 4 specifically: if 0170 did not apply, every limits test is measuring a
# world where the tables are absent, and `read_limits` would create the row itself -- which is
# exactly the "the gate passes because the code under test creates the schema" trap.
HAS_LIMITS=$(docker exec "$CONTAINER" psql -U omnion -d "$DB" -q -A -t \
  -c "select count(*) from information_schema.tables
       where table_name in ('automation_project_limits', 'automation_project_usage')")
if [ "${HAS_LIMITS}" != "2" ]; then
  echo "  FAIL: the limits/usage tables are missing — 0170 did not apply." >&2
  exit 1
fi
echo "[limits] 0164 + 0170 applied: workflows.project_id is NOT NULL"

echo
set +e
cargo test -p omnion-workflows --test project_limits -- --test-threads=4 --nocapture
STATUS=$?
set -e

echo
if [ "${STATUS}" != "0" ]; then
  echo "[limits] FAILED (exit ${STATUS}) — read the error above before theorising." >&2
  exit 1
fi
echo "[limits] passed"
