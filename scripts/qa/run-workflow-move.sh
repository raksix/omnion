#!/usr/bin/env bash
# The workflow move (REQ-133 slice 3) — the dependency report, the refusals and the audit row,
# against a real database.
#
#   bash scripts/qa/run-workflow-move.sh
#
# ## What this gate exists for
#
# Slice 3's promise is mostly about what does NOT happen: a dry run writes nothing, a move into an
# archived project writes nothing, a move of a workflow that moved underneath the caller writes
# nothing and says so. None of those are reachable by a unit test, because each of them is a
# question about the transaction rather than about a returned value.
#
# It also pins the one claim that is easy to state falsely: the report names the dependency kinds
# it **did not** check. `credential_reference` and the rest cannot be detected on this branch
# (there is no `credentials` table, no sub-workflow action), so a report that implied full
# coverage would be lying to the move dialog. The test asserts the names are present.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_move}"

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

echo "[move] applying migrations to ${DB}"
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
echo "[move] 0164 applied: workflows.project_id is NOT NULL"

echo
set +e
cargo test -p omnion-workflows --test workflow_move -- --test-threads=4 --nocapture
STATUS=$?
set -e

echo
if [ "${STATUS}" != "0" ]; then
  echo "[move] FAILED (exit ${STATUS}) — read the error above before theorising." >&2
  exit 1
fi
echo "[move] passed"
