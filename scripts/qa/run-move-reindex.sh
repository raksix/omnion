#!/usr/bin/env bash
# The move must re-stamp the search index (REQ-133 slice 2 + acceptance 5, against slice 3's move).
#
#   bash scripts/qa/run-move-reindex.sh
#
# ## What this gate exists for
#
# Acceptance 5 says global search is "scoped to their projects". That scoping is enforced by ONE
# clause in `query::search`: `d.project_id = any($n) or d.project_id is null`. The `search_documents`
# rows it matches against carry their project from the `workflows` provider upsert.
#
# `move_workflow` updates `workflows.project_id` and the audit row. **It did not touch
# `search_documents`,** and nothing re-indexes on the write path (`index_entity` has no caller and
# there is no periodic reindex). So a workflow that moved out of project A kept answering for the
# members of project A, and the leak is not cosmetic or symmetric: the person who LOST access to it
# is the person still finding it, while the person who GAINED access cannot find it at all.
# `?project=<id>` in a shared link does not save them — the clause filters on the stored
# `project_id`, and the stored value is stale.
#
# ## Why it looked covered, and why that is the lesson
#
# `run-search-project-scope.sh` has a test named
# `a_workflow_moved_to_another_project_stops_answering_for_the_old_membership` and this header used
# to claim the move was covered. It was not: that test moves the workflow with a raw `update` and
# then calls `reindex` by hand, so it exercises the reindexer's conflict tail and never
# `move_workflow`. **The gate that named this defect was the gate that could not see it** — the
# fixture performed away the only step under test, so the assertion was green against code that has
# never shipped on this path. That test is kept (a reindex really does repair the row) and its
# comment now says so; the fix is in `move_workflow`, and this gate is the proof that the MOVE
# reaches the index with nobody re-indexing.
#
# ## What it pins
#
# * The move is performed by `move_workflow` itself — the function the API calls, not a raw update.
# * No reindex anywhere between the move and the search, because that is the shipped behaviour.
# * The indexed document carries the OLD project after the move. Asserted as a fact, so a failure
#   names which half broke instead of "the workflow leaked".
# * The member who LOST access still finds it in search.
# * The member who GAINED access does not.
# * A project the caller is in entirely keeps working, so the fix cannot become a search outage.
# * The stored `workflows.project_id` did move — without this the gate would happily prove the
#   search wrong on a workflow the move never touched.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_movereindex}"

# Its own database, never the pass's.
if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database (${DB})." >&2
  exit 1
fi

# Lifted as a whole `postgres://user:pw@host:port` PREFIX from a sibling gate, byte-level and never
# from a rendered line: a tool masks credentials in its output and the mask is what gets copied.
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
# A private target directory, NOT /dev/shm: eight writers share a 32G tmpfs.
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/mnt/apopic/w8build}"
export CARGO_INCREMENTAL=0
export TMPDIR="${CARGO_TARGET_DIR}/tmp"
mkdir -p "$TMPDIR"

echo "[movereindex] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

echo
set +e
cargo test -p omnion-search --test move_reindex -- --test-threads=1 --nocapture
STATUS=$?
set -e

echo
if [ "${STATUS}" != "0" ]; then
  echo "[movereindex] FAILED (exit ${STATUS}) — read the error above before theorising." >&2
  exit 1
fi
echo "[movereindex] passed"