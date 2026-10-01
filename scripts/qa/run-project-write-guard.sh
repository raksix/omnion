#!/usr/bin/env bash
# The project write guard (REQ-133) — archived means read-only, and `max_workflows` is a cap.
#
#   bash scripts/qa/run-project-write-guard.sh
#
# ## The defect this gate exists for
#
# Two REQ promises were enforced by nothing a write path reaches, and **three gates on this branch
# were green while both were false**:
#
#   * *"Archived projects are read-only — no new runs, no edits."* Only the **runs** half had an
#     enforcement point. `run-project-isolation.sh` (14/14) proves a run in an archived project is
#     refused — which is exactly why the other half stayed invisible: the gate was named after the
#     sentence and measured the clause that happened to be implemented.
#   * *"Per-project maxima for workflows, credentials, runs per day and concurrent runs."*
#     `run-project-limits.sh` (14/14) proves `max_runs_per_day` and `max_concurrent_runs` are
#     refused. It does not ask about `max_workflows`, because `ensure_run_within_limits` does not
#     ask about it either — and neither does anything else. A project capped at five workflows
#     could hold five hundred while the limits bar read "at the limit".
#
# The general rule, which is the fourth time this branch has written it down: **a gate named after
# a sentence measures the clauses that exist, and the missing clause is invisible by construction.**
#
# ## Why it is a crate test and not psql
#
# The guard's whole content is *where it is called from* — three store functions. A SQL script
# would have to re-implement them and would prove the script. `cargo test -p omnion-workflows
# --test project_write_guard` calls `store::insert_workflow`, `store::update_workflow` and
# `store::delete_workflow` directly, so changing any of them changes this gate's result.
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
DB="${QA_DB:-omnion_qa_w8_writeguard}"

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

echo "[write-guard] applying migrations to ${DB} (0170 limits included — they are the subject)"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The pre-flight the whole gate rests on. If the limits table is missing, `set_limits` fails and
# every test below reports a fixture problem as a product verdict.
HAS_LIMITS=$(docker exec "$CONTAINER" psql -U omnion -d "$DB" -q -A -t \
  -c "select count(*) from information_schema.columns
       where table_name = 'automation_project_limits' and column_name = 'max_workflows'")
if [ "${HAS_LIMITS}" != "1" ]; then
  echo "  FAIL: automation_project_limits.max_workflows is absent — 0170 did not apply." >&2
  exit 1
fi
echo "[write-guard] 0170 applied: automation_project_limits.max_workflows exists"

echo
set +e
cargo test -p omnion-workflows --test project_write_guard -- --test-threads=4 --nocapture
STATUS=$?
set -e

echo
if [ "${STATUS}" != "0" ]; then
  echo "[write-guard] FAILED (exit ${STATUS}) — read the failure above before theorising."
  echo "              A test that says the count is wrong is the guard writing before refusing;"
  echo "              one that says the code is wrong is the guard not being reached at all."
  exit 1
fi
echo "[write-guard] passed"
