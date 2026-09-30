#!/usr/bin/env bash
# A shared link's `?project=` resolves for the person who OPENS it (REQ-133, acceptance 3).
#
#   bash scripts/qa/run-project-link.sh
#
# ## What this gate exists for
#
# Acceptance 3 says the selection "is encoded in URLs so a shared link reproduces the view". The
# switcher WROTE that parameter and read it in exactly one place — the button's own label — so a
# colleague opening your link saw *their* stored selection, not the project you sent them. The
# write half had been shipped, typechecked and driven by the harness for many ticks, which is what
# made the missing read half invisible: a parameter that is written, named in the REQ and read by
# nothing is the shape of a feature that only needs one more line, and nobody goes looking for the
# one line nobody needed.
#
# A store test is the right level here because the decision is a *tenancy* decision: the question
# "may THIS caller see the project a link names" can only be answered with the caller's identity
# in hand, and every browser assertion about it would be measuring the panel's rendering of an
# answer the store gives.
#
# ## What it pins
#
# * No link is `None`, and that is a DIFFERENT sentence from a refusal.
# * A link to a visible project resolves as visible.
# * A link to a project you may not see answers `(id, false)` — the id comes back so the panel can
#   explain the refusal. `None` would silently drop the reader on their own selection, which is the
#   one outcome a shared link must never have.
# * Another organization's project and a deleted one are both refused, never raised.
# * Reading a link never writes the caller's own selection.
# * An instance administrator reaches every project in the organization, links included.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_link}"

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

echo "[link] applying migrations to ${DB}"
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
echo "[link] 0176 applied: recents and selection exist"

echo
set +e
cargo test -p omnion-workflows --test project_link -- --test-threads=2 --nocapture
STATUS=$?
set -e

echo
if [ "${STATUS}" != "0" ]; then
  echo "[link] FAILED (exit ${STATUS}) — read the error above before theorising." >&2
  exit 1
fi
echo "[link] passed"
