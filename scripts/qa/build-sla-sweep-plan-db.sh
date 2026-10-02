#!/usr/bin/env bash
# Build the SLA-sweep planner fixture database: migrate, load, keep it for the gate.
# Sourced by run-crm-sla-sweep-plan.sh; kept separate so a measurement can be re-run by hand
# without the gate's assertions around it.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
DB="${QA_DB:-omnion_qa_w8_slaplan}"
CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database (${DB})." >&2
  exit 1
fi

PGPASSWORD="$(python3 - <<'PY'
import re
text = open('/mnt/apopic/omnion-w8/scripts/qa/run-crm-assign.sh', encoding='utf-8').read()
m = re.search(r'DATABASE_URL="(postgres://[^@"]+)@', text)
print(m.group(1).split('://', 1)[1].split(':', 1)[1] if m else '')
PY
)"
if [ -z "${PGPASSWORD}" ]; then
  echo "  FAIL: could not read the QA database password out of run-crm-assign.sh." >&2
  exit 1
fi

# `px <sql> [database]` — the database is a parameter, not a constant, because the drop below
# has to run against `postgres`: a session cannot drop the database it is connected to.
px() { docker exec -e "PGPASSWORD=${PGPASSWORD}" "$CONTAINER" psql -U omnion -d "${2:-$DB}" -q -A -t -v ON_ERROR_STOP=1 -c "$1"; }
pxi() { docker exec -i -e "PGPASSWORD=${PGPASSWORD}" "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1; }

# The drop runs against `postgres`, never against the database being dropped: a session cannot
# drop the database it is connected to, and a gate that reports that as its own failure is a gate
# whose red tells nobody anything.
px "DROP DATABASE IF EXISTS ${DB} WITH (FORCE)" postgres >/dev/null
px "CREATE DATABASE ${DB} OWNER omnion" postgres >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  pxi <"$f" >/dev/null
done
px "$(cat scripts/qa/fixture-sla-sweep-plan.sql)" >/dev/null
echo "[crm-sla-sweep-plan] ${DB} ready"