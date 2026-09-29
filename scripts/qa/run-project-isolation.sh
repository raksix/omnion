#!/usr/bin/env bash
# Automation project isolation (REQ-133 slice 2) — the write path and the 404 boundary, against
# a real database.
#
#   QA_DB=omnion_qa_w8_isolation bash scripts/qa/run-project-isolation.sh
#
# ## The defect this gate exists for
#
# Slice 1 shipped migration 0164, which made `workflows.project_id` `not null` after backfilling
# the existing rows. The store's insert statement named twelve columns and none of them was
# `project_id`. Every unit test on the branch stayed green, because no unit test executes SQL.
#
# So the moment 0164 landed, **every workflow creation raised `23502`** — and the crate the
# project feature lives in reported 48/48 passing. This gate drives the store against a database
# that has actually applied 0164, which is the only place that sentence can be true or false.
#
# ## Why it is a crate test and not a psql script
#
# A psql script would have to re-implement the store's insert, and then it would prove the script
# rather than the code. `cargo test -p omnion-workflows --test projects_isolation` calls
# `store::insert_workflow` and `projects::visible_project_ids` directly, so a change to either
# function is what changes this gate's result.
#
# Its own database, never the pass's: the open below is `DROP DATABASE … WITH (FORCE)`, which
# terminates the browser pass's API connections and then fails twenty routes from the cause.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_isolation}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database (${DB})." >&2
  echo "        Give the gate its own, as the other w8 gates do." >&2
  exit 1
fi

# Lifted as a whole `postgres://user:***@host:port` PREFIX from a sibling gate, byte-level and
# never from a rendered line: a tool masks credentials in its output and the mask is what gets
# copied. That has bitten this branch three times, and the symptom is `28P01` on every test.
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
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/dev/shm/w8-target}"
export CARGO_INCREMENTAL=0

echo "[isolation] applying migrations to ${DB} (0164 included — it is the subject)"
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
echo "[isolation] 0164 applied: workflows.project_id is NOT NULL"

echo
set +e
cargo test -p omnion-workflows --test projects_isolation -- --test-threads=4 --nocapture
STATUS=$?
set -e

echo
if [ "${STATUS}" != "0" ]; then
  echo "[isolation] FAILED (exit ${STATUS}) — read the error above before theorising: the first"
  echo "           failure with 23502 on project_id is the insert path, and everything after it"
  echo "           is an echo of the same missing column."
  exit 1
fi
echo "[isolation] passed"
