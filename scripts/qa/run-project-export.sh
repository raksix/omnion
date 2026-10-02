#!/usr/bin/env bash
# Exporting an automation project (REQ-133) — the word the settings row has carried since slice 1,
# between "archive" and "delete", with nothing behind it.
#
#   bash scripts/qa/run-project-export.sh
#
# ## The defect this gate exists for
#
# The settings screen's row reads "Name, key, description, colour, archive, export, delete" and the
# REQ's Risks section names the "export-first hint" as one of the four things that make deleting a
# project with dependencies safe. Slice 19 wrote `delete` — the row's LAST word — and left the export
# it names as the remedy unwritten. So the refusal message pointed at a button that did not exist.
#
# Same shape as the delete gate, one word earlier: **the safe half (archive) and the destructive
# half (delete) were both implemented and the one between them was described.**
#
# ## Why the gate is a crate test and not a query
#
# Half of these assertions are about what the file CONTAINS (nested `steps` survive as stored, a
# membership whose account is gone still appears), and a `psql` fixture cannot see a serialization
# bug at all. The other half — the cross-tenant refusal — belongs to `find_visible`, which is the
# function the handler calls, so calling it here is what proves the route's caller resolution.
#
# ## Its own database, never the pass's
#
# The `DROP DATABASE … WITH (FORCE)` below terminates the browser pass's API connections. Each w8
# gate owns a disposable database and refuses to start if pointed at `omnion_qa` or `omnion_qa_w8`.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_projectexport}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database (${DB})." >&2
  echo "        Give the gate its own, as the other w8 gates do." >&2
  exit 1
fi

# Lifted as a whole `postgres://user:pass@host:port` PREFIX from a sibling gate, byte-level and
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

echo "[project-export] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The pre-flight. Without it, a migration that did not create one of these reports a fixture
# problem as a product verdict — and the membership-survival test queries `pg_constraint` for the
# membership's user foreign key, so a table under a different name would make it read as "the
# export dropped a row" when it never found one.
NEEDED=$(docker exec "$CONTAINER" psql -U omnion -d "$DB" -q -A -t \
  -c "select count(*) from information_schema.tables
       where table_name in ('automation_projects', 'automation_project_members', 'users', 'workflows')")
if [ "${NEEDED}" != "4" ]; then
  echo "  FAIL: expected automation_projects, automation_project_members, users and workflows; found ${NEEDED}/4." >&2
  exit 1
fi
echo "[project-export] migrations applied: the four tables the snapshot reads are present"

echo
set +e
cargo test -p omnion-workflows --test project_export -- --test-threads=4 --nocapture
STATUS=$?
set -e

echo
if [ "${STATUS}" != "0" ]; then
  echo "[project-export] FAILED (exit ${STATUS}) — read the failure above before theorising."
  echo "                  'expected X and got 1' on the members count means the snapshot lost a"
  echo "                  row; 'no function named create_workflow' means the fixture, not the"
  echo "                  product."
  exit 1
fi
echo "[project-export] passed"