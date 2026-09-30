#!/usr/bin/env bash
# Project-scoped global search (REQ-133, acceptance 5) — against a real database.
#
#   bash scripts/qa/run-search-project-scope.sh
#
# ## What this gate exists for
#
# Acceptance 5 says global search "returns only resources the caller may see, **scoped to their
# projects**, verified with two accounts". Nothing on this branch scoped it: `grep -rn project
# crates/search/src/` returned nothing at all, `search_documents` carried `organization_id` and
# `site_id` from migration 0012 and had no project column, and the query narrowed by provider and
# organization only. Two people in ONE organization, in different projects, both passed every
# check the query made — and organization-only is exactly the filter that was already there, so a
# one-account fixture could not have told the difference.
#
# ## What it pins
#
# * The indexed document carries the project, read back from the row — otherwise the clause has
#   nothing to match and every "cannot see it" assertion would be vacuously true.
# * Two accounts in one organization, in different projects, typing the SAME word: each sees only
#   their own side's workflow, and the foreign one is absent from the hits *and* from the
#   subtitles (a name is not the leak; a link to a screen that 404s is).
# * The mirror image, so the rule is not "alice is special".
# * The shared default project stays visible to both — the scope must not become a wall.
# * `None` (unfiltered) is the instance administrator; `Some(vec![])` is a member of no project,
#   and the two must not be the same answer. This is the shape a scoping rule gets un-applied by
#   accident when one caller inherits the other's branch.
# * The palette's suggestion query is narrowed by the same rule — a second statement over the
#   same index, invisible to every other test in the suite because they all go through `search`.
# * A moved workflow stops answering for the membership it lost.
# * A page (no project) stays reachable under an EMPTY project scope, so the fix cannot become
#   an outage of the four non-project providers.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_searchscope}"

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
export TMPDIR="${CARGO_TARGET_DIR}/tmp"
mkdir -p "$TMPDIR"

echo "[searchscope] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The pre-flight the whole gate rests on: without 0180 there is nowhere for a document's project
# to live, and every assertion below would be measuring a world where "scoped to their projects"
# is not expressible.
HAS_COLUMN=$(docker exec "$CONTAINER" psql -U omnion -d "$DB" -q -A -t \
  -c "select count(*) from information_schema.columns
       where table_name = 'search_documents' and column_name = 'project_id'")
if [ "${HAS_COLUMN}" != "1" ]; then
  echo "  FAIL: search_documents.project_id is missing — 0180 did not apply." >&2
  exit 1
fi
echo "[searchscope] 0180 applied: search_documents.project_id exists"

echo
set +e
cargo test -p omnion-search --test project_scoped_search -- --test-threads=2 --nocapture
STATUS=$?
set -e

echo
if [ "${STATUS}" != "0" ]; then
  echo "[searchscope] FAILED (exit ${STATUS}) — read the error above before theorising." >&2
  exit 1
fi
echo "[searchscope] passed"
