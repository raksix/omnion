#!/usr/bin/env bash
# Deleting an automation project (REQ-133) — the destructive half of the surface, which the REQ's
# own API table has documented since the module shipped and which nothing implemented.
#
#   bash scripts/qa/run-project-delete.sh
#
# ## The defect this gate exists for
#
# `DELETE /api/v1/projects/{id}` is in REQ-133's API table with "typed confirmation, dependency
# check" as its purpose, the settings screen's row reads "Name, key, description, colour, archive,
# export, delete", and migration `0164` chose `on delete restrict` for the resource → project
# foreign keys *because* "deletion is a deliberate act with its own dependency check (slice 4)".
# There was no `delete_project` in the store, no handler, no route, no button, and
# `automation.project.deleted` was an event nothing emitted.
#
# This is the branch's signature defect in its most complete form: **every safe half of a feature
# exists and the destructive half is described.** The gates that were green here — limits 14/14,
# write-guard 5/5, move 13/13 — each measured the clauses that had been implemented, which is the
# rule this gate's header already states: *a gate named after a sentence measures the clauses that
# exist, and the missing clause is invisible by construction.*
#
# ## Why it is a crate test and not psql
#
# The refusals are about **order** — the default project before the confirmation, the confirmation
# before the dependencies — and an order is invisible to a query that only checks the final state.
# `cargo test -p omnion-workflows --test project_delete` calls `projects::delete_project` itself,
# so reordering its arms changes this gate's result.
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
DB="${QA_DB:-omnion_qa_w8_projectdelete}"

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

echo "[project-delete] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The pre-flight the whole gate rests on. If the table is missing, every test below reports a
# fixture problem as a product verdict — and the audit assertions need the trail's real name,
# which is `audit_log` and not the `audit_logs` a reader of the REQ would guess.
HAS_PROJECTS=$(docker exec "$CONTAINER" psql -U omnion -d "$DB" -q -A -t \
  -c "select count(*) from information_schema.tables
       where table_name in ('automation_projects', 'automation_project_members', 'audit_log')")
if [ "${HAS_PROJECTS}" != "3" ]; then
  echo "  FAIL: expected automation_projects, automation_project_members and audit_log; found ${HAS_PROJECTS}/3." >&2
  exit 1
fi
echo "[project-delete] migrations applied: projects, members and the audit trail are present"

echo
set +e
cargo test -p omnion-workflows --test project_delete -- --test-threads=4 --nocapture
STATUS=$?
set -e

echo
if [ "${STATUS}" != "0" ]; then
  echo "[project-delete] FAILED (exit ${STATUS}) — read the failure above before theorising."
  echo "                  23503 means the dependency check was skipped and the foreign key"
  echo "                  answered; 42P01 means a table this gate names does not exist."
  exit 1
fi
echo "[project-delete] passed"
