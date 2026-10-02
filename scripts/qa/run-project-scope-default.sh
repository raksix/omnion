#!/usr/bin/env bash
# The stored project selection must be the DEFAULT for a list, not just the switcher's label.
#
#   bash scripts/qa/run-project-scope-default.sh
#
# ## What this gate exists for
#
# REQ-133's API table promises `GET /workflows` gains "a `project_id` filter **and scoped
# defaults**", and acceptance 3 says "the switcher filters workflows, credentials, folders,
# schedules and executions; the selection survives navigation".
#
# Only the filter half shipped. `automation_project_selection` was written by `select_project`
# and read back by `selected_project` — the switcher's own button label. **No list ever read it**:
# `list_scoped_workflows` and `list_automations` both computed
# `match wanted { Some(id) => …, None => visible }`, so a request with no `project_id` returned
# every project the caller can see. A reader who picked OPS and then opened a list got the
# unscoped list back, under a header still reading "OPS".
#
# ## Why the existing gates could not see it
#
# `run-project-switcher.sh` (8/8) proves the selection is *stored* and *recency-ordered*.
# `run-project-link.sh` (7/7) proves `?project=` is *read* and refused when invisible. Neither
# asks whether anything CONSUMES the stored value — and both would have stayed green with the
# whole feature switched off. `stored_selection` had **zero callers on the branch**, which is this
# module's thirteenth signature defect and the first whose dead thing was a *default* rather than
# a value.
#
# ## What it pins
#
# * No filter ⇒ the caller's stored selection decides the list.
# * The selection survives navigation because it is a DATABASE row, asserted as a row.
# * An explicit `project_id` still outranks the selection (negative control).
# * A selection naming a project the caller can no longer see is IGNORED, not honoured — otherwise
#   removing a member would widen the list for the person who just lost access.
# * A selection from another tenant is never a default.
# * Never having selected is not the same as having selected the default project.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_scopedefault}"

# Its own database, never the pass's.
if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database (${DB})." >&2
  exit 1
fi

# Lifted as a whole `postgres://user:...@host:port` PREFIX from a sibling gate, byte-level and
# never from a rendered line: a tool masks credentials in its output and the mask is what gets
# copied.
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

echo "[scopedefault] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

echo
set +e
cargo test -p omnion-workflows --test project_scope_default -- --test-threads=1 --nocapture
STATUS=$?
set -e

echo
if [ "${STATUS}" != "0" ]; then
  echo "[scopedefault] FAILED (exit ${STATUS}) — read the error above before theorising." >&2
  exit 1
fi
echo "[scopedefault] passed"
