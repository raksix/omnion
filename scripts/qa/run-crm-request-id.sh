#!/usr/bin/env bash
# CRM intake — the request id on a trail line (REQ-117, acceptance 17).
#
#   QA_DB=omnion_qa_w8_reqid bash scripts/qa/run-crm-request-id.sh
#
# ## Why this gate is its own file
#
# Acceptance 17 asks for an audit entry with "actor, before/after **and request id**". The first
# two shipped long ago; the third did not, and the REQ said why in writing: the API had no
# per-request id on these routes, and inventing a header-shaped column that is never populated
# would be "a column that reads as recorded and is not".
#
# That refusal is exactly why the gate is needed. **A trail line written without an id is not an
# error, is not logged, and is indistinguishable from a healthy row on every screen.** So the box
# could be ticked with the column in place and nothing behind it, and every assertion that only
# checked "the column exists" would have stayed green forever. This gate reads the value back out
# of the stored row instead, in the three states that have to be told apart: an exchange's own id,
# `null` for a worker sweep that belongs to no exchange, and — the case a single-task test cannot
# reach — two concurrent exchanges not writing each other's id.
#
# **Its own database, never the pass's**, for the reason `run-crm-claims.sh` carries: these gates
# open with `DROP DATABASE … WITH (FORCE)`, which terminates the browser pass's API connections
# and lands the failure twenty routes from the cause.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_reqid}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database ($DB)." >&2
  echo "        Give the gate its own: the other crm gates use omnion_qa_w8_*." >&2
  exit 1
fi

# The password is read out of a sibling gate's script rather than retyped: a hand-written URL
# produces "N failed" that is the script's configuration, not a regression. Byte-level, never
# from a rendered line — a tool masks credentials in output and the mask is what gets copied.
PGPASS_PORT="$(grep -oE '127\.0\.0\.1:5433' scripts/qa/run-crm-assign.sh | head -1)"
PGPASS_USER="$(grep -oE 'postgres://[a-z_]+' scripts/qa/run-crm-assign.sh | head -1 | cut -d/ -f3)"
PGPASS_PASS="$(python3 - <<'PY'
import re
text = open('/mnt/apopic/omnion-w8/scripts/qa/run-crm-assign.sh', encoding='utf-8').read()
m = re.search(r'DATABASE_URL="postgres://[^:]+:([^@]+)@', text)
print(m.group(1) if m else '')
PY
)"
if [ -z "${PGPASS_PASS}" ]; then
  echo "  FAIL: could not read the QA database password out of run-crm-assign.sh." >&2
  exit 1
fi
export DATABASE_URL="postgres://${PGPASS_USER}:${PGPASS_PASS}@127.0.0.1:5433/${DB}"
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/dev/shm/w8t}"
export CARGO_INCREMENTAL=0

echo "[crm-request-id] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The trail has to be readable at all before "it carries an id" means anything: a query that finds
# no rows would otherwise satisfy a comparison against nothing.
TABLES=$(docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -A -t -v ON_ERROR_STOP=1 -c \
  "select count(*) from information_schema.tables
    where table_name in ('crm_lead_events','crm_leads','crm_intake_sources')")
if [ "${TABLES}" != "3" ]; then
  echo "  FAIL: the CRM intake tables are not all present (found ${TABLES}/3)." >&2
  exit 1
fi

# --test-threads=1: each test creates and drops rows for its own organization, and the
# cross-talk test needs the two captures to be the only work in flight.
cargo test -p omnion-module-crm-intake --test crm_request_id -- --test-threads=1 --nocapture
