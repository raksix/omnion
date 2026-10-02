#!/usr/bin/env bash
# CRM intake — hand assignment and the batch that hands twenty over at once, against a real
# database.
#
#   QA_DB=omnion_qa_w8_assign bash scripts/qa/run-crm-assign.sh
#
# `POST /crm/leads/{id}/assign` is in REQ-117's API table from the first tick and did not exist
# until this one, so this gate is the whole of its evidence rather than one case among many.
# It is small on purpose: the store function is one statement plus a trail line, and the only
# claims that need a database are the ones a unit test structurally cannot make — that the owner
# change and its trail line are ONE commit, that the `for update` read of the previous owner is
# what stops two hands from claiming the same predecessor, and that the refusal path writes
# nothing at all.
#
# Its own database, never the pass's. See the long note in run-crm-convert.sh: these gates open
# with `DROP DATABASE … WITH (FORCE)`, which terminates the browser pass's API connections, and
# the symptom then lands twenty routes away from the cause as offline error pages.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_assign}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database ($DB)." >&2
  echo "        Give the gate its own, as the other crm gates do." >&2
  exit 1
fi
export DATABASE_URL="postgres://omnion:omnion@127.0.0.1:5433/${DB}"
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/dev/shm/w8-target}"
export CARGO_INCREMENTAL=0

echo "[crm-assign] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# One organization per test, for the reason the other gates say it: cargo runs a binary's tests
# concurrently, so a shared fixture means each test deletes the others' rows mid-run.
cargo test -p omnion-module-crm-intake --test crm_assign --test crm_bulk -- --nocapture
