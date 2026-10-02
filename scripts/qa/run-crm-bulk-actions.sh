#!/usr/bin/env bash
# CRM intake — the bulk bar's other verbs and its export, against a real database.
#
#   bash scripts/qa/run-crm-bulk-actions.sh
#
# REQ-117's inbox row has read "Bulk: assign, reassign, mark responded, mark spam, reject with
# reason, export CSV" since the request was written. Only the hand-over existed, so this gate is
# the whole of the evidence for the other four rather than one case among many.
#
# ## Why these claims need a database
#
# * Each lead is its own transaction, so **partial success is the normal case** — nineteen of
#   twenty moving and one refusal is the designed answer, and only a real row read can tell that
#   from twenty successes.
# * `record_response` is idempotent on the *instant* but not on the *trail*: two presses write
#   two lines, and the property worth proving is that both name the same instant so the newest
#   line and the column agree.
# * The export's tenancy claim is a claim about the FILTER, not about the renderer — a renderer
#   faithfully prints whatever it is handed, so "another tenant's lead is absent" is proved by
#   exporting a filtered read and asserting the row is missing from it.
# * The formula guard is proved on a cell a **visitor chose**, because `first_name` is free text
#   on a public form. A guard tested only on synthetic strings has not been tested on the input
#   that reaches it.
#
# Its own database, never the pass's: this gate opens with `DROP DATABASE … WITH (FORCE)`, which
# terminates the browser pass's API connections, and the symptom lands twenty routes from the
# cause as offline error pages.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_bulkact}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database ($DB)." >&2
  echo "        Give the gate its own, as the other crm gates do." >&2
  exit 1
fi
export DATABASE_URL="postgres://omnion:omnion@127.0.0.1:5433/${DB}"
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/dev/shm/w8-target}"
export CARGO_INCREMENTAL=0

echo "[crm-bulk-actions] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# One organization per test: cargo runs a binary's tests concurrently, so a shared fixture means
# each test deletes the others' rows mid-run.
cargo test -p omnion-module-crm-intake --test crm_bulk_actions -- --nocapture
