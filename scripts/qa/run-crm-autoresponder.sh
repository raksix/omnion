#!/usr/bin/env bash
# CRM intake slice 3 — the autoresponder, against a real database.
#
#   QA_DB=omnion_qa_w8 bash scripts/qa/run-crm-autoresponder.sh
#
# This gate exists for the same reason run-crm-assignment.sh does, one domain over: **the
# interesting claim is a claim.** "Send the visitor one acknowledgement" is a sentence about a
# conditional insert, not about a function. Every sequential test of it passes, and the tenth
# concurrent capture of one lead still sends ten emails, because a read-then-write is not a
# race anyone can lose.
#
# The other half is what must NOT send. A rejected submission, a spam submission and a source
# with no autoresponder all produce no mail, and each leaves a different line on the trail —
# because "we did not mail them" and "we never considered it" are the question an operator
# opens a lead to answer, and an empty timeline answers neither.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_autoresponder}"

# A gate may not share the browser pass's database. `DROP DATABASE … WITH (FORCE)` terminates
# every connection, and the pass's API is one of them: the walk then dies partway through the
# route list and the failure is reported on whichever screens came *next* (the five CRM screens,
# as offline chrome-error pages) — the cause and the symptom are twenty routes apart. The gate
# fails in silence, the pass fails loudly and wrongly, and the report names the panel.
if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database ($DB)." >&2
  echo "        Give the gate its own: run-crm-intake.sh uses omnion_qa_crm_intake." >&2
  exit 1
fi
export DATABASE_URL="postgres://omnion:omnion@127.0.0.1:5433/${DB}"
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/dev/shm/w8-target}"
export CARGO_INCREMENTAL=0

echo "[crm-autoresponder] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# Each test builds its own organization and tears it down: cargo runs a binary's tests in
# parallel, so a shared fixture would let one test's cleanup delete another's rows mid-run.
# The concurrency test additionally depends on all ten tasks sharing one pool, which is the
# same pool the file builds.
cargo test -p omnion-module-crm-intake --test crm_autoresponder -- --nocapture --test-threads=1
