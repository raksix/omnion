#!/usr/bin/env bash
# Project limit notices fire once per limit per period (REQ-133, slice 4) — against a real
# database.
#
#   bash scripts/qa/run-project-limit-notices.sh
#
# ## What this gate exists for
#
# Slice 4's limits sentence is one clause that could not be true on this branch: "the 80 percent
# warning **fires once**". Everything else about it was shipped and green — `Limits::warns`
# computed the crossing, the screen drew the amber bar, `ensure_run_within_limits` refused the
# over-quota run by name — and the two `automation.project.limit.*` events the REQ names were
# emitted by nothing at all.
#
# So the state was derived per read (reloading the screen re-warned for ever) and the once-ness
# had no home. This gate starts where the product starts: caps are written through `set_limits`,
# usage through `record_usage`, and the sweep is called the way `project_limit_runner` calls it.
# A test that built a `LimitNotice` by hand would prove the `on conflict` clause and nothing else.
#
# ## The pre-flight
#
# If migration 0172 did not apply, every claim is a `42P01` and the suite reports a product defect
# where there is a schema one — the trap this branch has already paid for twice, where the code
# under test created the very schema it was measured against.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_notices}"

# Its own database, never the pass's: the DROP below terminates the browser pass's API connections
# and then fails twenty routes from the cause.
if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database (${DB})." >&2
  exit 1
fi

# Lifted as a whole `postgres://user:***@host:port` PREFIX from a sibling gate, byte-level and
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

echo "[notices] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The pre-flight the whole gate rests on: without 0172 there is no claim table, and the suite would
# be measuring a world where "fires once" is not expressible.
HAS_TABLE=$(docker exec "$CONTAINER" psql -U omnion -d "$DB" -q -A -t \
  -c "select count(*) from information_schema.tables
       where table_name = 'automation_project_limit_notices'")
if [ "${HAS_TABLE}" != "1" ]; then
  echo "  FAIL: automation_project_limit_notices is missing — 0172 did not apply." >&2
  exit 1
fi
echo "[notices] 0172 applied: the claim table exists"

echo
set +e
cargo test -p omnion-workflows --test project_limit_notices -- --test-threads=2 --nocapture
STATUS=$?
set -e

echo
if [ "${STATUS}" != "0" ]; then
  echo "[notices] FAILED (exit ${STATUS}) — read the error above before theorising." >&2
  exit 1
fi
echo "[notices] passed"
