#!/usr/bin/env bash
# The project audit filter and the two-confirmation handover (REQ-133 slice 4) — against a real
# database.
#
#   bash scripts/qa/run-project-audit.sh
#
# ## What this gate exists for
#
# Slice 4 shipped `audit_log.project_id` in migration 0164 and then wrote **every project-scoped
# audit row without it**. The column was nullable, the inserts named eight columns, and the project
# audit screen the REQ asks for had nothing to filter on. Both halves of that are invisible to a
# unit test: the insert succeeds, `AuditEntry` still deserialises, and the screen would render an
# empty list that looks exactly like a project where nothing happened.
#
# So the tests here are about the *distinction* between a project row and a platform row in the
# same organization — which only exists once something has actually been inserted.
#
# ## What it pins
#
# * A project mutation names its project; a platform-level act does not. Both facts at once,
#   because "the platform rows started naming a project" is the other half of the same bug.
# * The project trail never carries another project's rows — and never a decoy platform row from
#   the same tenant, which is exactly what a filter over `organization_id` would return.
# * An empty action filter means "every action", not "no action".
# * Deleting the actor nulls `actor_user_id` and keeps the row: an audit trail that vanishes with
#   the account is not a trail.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_audit}"

# Its own database, never the pass's: the DROP below terminates the browser pass's API connections
# and then fails twenty routes from the cause.
if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database (${DB})." >&2
  exit 1
fi

# Lifted as a whole `postgres://user:...@host:port` PREFIX from a sibling gate, byte-level and
# never from a rendered line: a tool masks credentials in its output and the mask is what gets
# copied. That has bitten this branch several times and the symptom is 28P01 on every test.
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

echo "[audit] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The pre-flight the whole gate rests on: without this column there is no filter to test, and every
# assertion below would be measuring an empty trail.
HAS_COL=$(docker exec "$CONTAINER" psql -U omnion -d "$DB" -q -A -t \
  -c "select count(*) from information_schema.columns
       where table_name = 'audit_log' and column_name = 'project_id'")
if [ "${HAS_COL}" != "1" ]; then
  echo "  FAIL: audit_log.project_id is missing — 0164 did not apply." >&2
  exit 1
fi
echo "[audit] 0164 applied: audit_log.project_id exists"

echo
set +e
cargo test -p omnion-workflows --test project_audit -- --test-threads=4 --nocapture
STATUS=$?
set -e

echo
if [ "${STATUS}" != "0" ]; then
  echo "[audit] FAILED (exit ${STATUS}) — read the error above before theorising." >&2
  exit 1
fi
echo "[audit] passed"
