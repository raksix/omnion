#!/usr/bin/env bash
# The project switcher's persistence and ranking (REQ-133, acceptance 3) — against a real
# database.
#
#   bash scripts/qa/run-project-switcher.sh
#
# ## What this gate exists for
#
# Acceptance 3 says the switcher's "selection **persists per user** and is written into URLs". The
# URL half is client state; the persistence half had nothing behind it. `ProjectListQuery.mine` was
# declared on the API's query struct, the REQ's API table named `GET /api/v1/projects?mine=1` as
# "the switcher's list", and no code on this branch read the parameter — so the switcher had no
# server-side list and nowhere to remember a choice. It is the ninth instance of this branch's
# signature defect (a documented surface with no caller) and the first where the missing thing is a
# *table* rather than a caller.
#
# A browser-only selection would have passed "survives navigation" and failed the "per user" half on
# a second device, and an `on conflict do nothing` upsert would have passed every unit test over a
# helper while pinning the first project ever selected at rank 0 for ever. So the tests drive the
# store and read the rows back, in a second function from the one that wrote them.
#
# ## What it pins
#
# * The selection is durable, per user, and answered as a flag on the rows.
# * Recents rank most-recent-first, and re-selecting a project DEMOTES the previous head — the exact
#   failure a naive upsert has, asserted before the fix rather than after.
# * The window is eight and the ranks are dense: an eviction, not a sparse list and not an error.
# * An instance administrator reaches every project; a member reaches only their own.
# * A refused switch writes nothing — not the selection, not the recents row.
# * A selection whose project is deleted leaves the reader in the default state, not on a 404.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_switcher}"

# Its own database, never the pass's: the DROP below terminates the browser pass's API connections
# and then fails twenty routes from the cause.
if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database (${DB})." >&2
  exit 1
fi

# Lifted as a whole `postgres://user:***@host:port` PREFIX from a sibling gate, byte-level and
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

echo "[switcher] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The pre-flight the whole gate rests on: without 0176 there is nowhere for a selection to live,
# and every assertion below would be measuring a world where "persists per user" is not expressible.
HAS_TABLES=$(docker exec "$CONTAINER" psql -U omnion -d "$DB" -q -A -t \
  -c "select count(*) from information_schema.tables
       where table_name in ('automation_project_recent', 'automation_project_selection')")
if [ "${HAS_TABLES}" != "2" ]; then
  echo "  FAIL: the recents/selection tables are missing — 0176 did not apply." >&2
  exit 1
fi
echo "[switcher] 0176 applied: recents and selection exist"

echo
set +e
cargo test -p omnion-workflows --test project_switcher -- --test-threads=2 --nocapture
STATUS=$?
set -e

echo
if [ "${STATUS}" != "0" ]; then
  echo "[switcher] FAILED (exit ${STATUS}) — read the error above before theorising." >&2
  exit 1
fi
echo "[switcher] passed"
